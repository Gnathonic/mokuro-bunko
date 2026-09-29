"""Real throughput, and the admin Processors card that shows it per machine.

* `ocr.throughput`: pages over the wall seconds of finished volumes, from a
  profile's recent runs, its congestion records, or its cumulative totals;
* `ProcessorProfiles.record_run`: keeps the recent volumes and when the last
  one finished;
* `GET /_admin/api/processors`: per machine, per layer -- real pages/min, the
  volumes behind it, the machine's own benchmark, when it last ran the layer
  -- and none of it ever reaches the public queue endpoint.
"""

from __future__ import annotations

import io
import json
import threading
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import AdminConfig, Config, QueueConfig
from mokuro_bunko.database import Database
from mokuro_bunko.ocr import bench as bench_module
from mokuro_bunko.ocr.congestion import CongestionHistory
from mokuro_bunko.ocr.devices import set_cached_catalog
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.remote import profiles as profiles_module
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.throughput import (
    RECENT_VOLUMES,
    profile_throughput,
    records_throughput,
    throughput_of,
)
from mokuro_bunko.queue.api import QueueAPI

ROWS: list[dict[str, Any]] = [
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"},
]


@pytest.fixture(autouse=True)
def _no_published_catalog() -> Iterator[None]:
    set_cached_catalog(None)
    yield
    set_cached_catalog(None)


@pytest.fixture(autouse=True)
def _no_real_host_probe(monkeypatch: pytest.MonkeyPatch) -> None:
    """The panel probes this server's hardware in the background; a test
    never shells out to nvidia-smi for it."""
    monkeypatch.setattr(bench_module, "describe_host",
                        lambda backend, engines_python: {"cpu": "test CPU", "gpu": None})


# --- the arithmetic --------------------------------------------------------------


class TestThroughput:
    def test_pages_over_seconds_not_a_mean_of_rates(self) -> None:
        # 10 pages in 1 s and 90 pages in 9 s: 10 pages/s, whatever the mix.
        found = throughput_of([(10, 1.0), (90, 9.0)])
        assert found is not None
        assert found.pages_per_second == pytest.approx(10.0)
        assert found.pages_per_minute == pytest.approx(600.0)
        assert found.volumes == 2

    def test_nothing_usable_is_none(self) -> None:
        assert throughput_of([]) is None
        assert throughput_of([(0, 5.0), (5, 0.0), ("x", 1.0)]) is None

    def test_a_profiles_recent_runs_win(self) -> None:
        runs = {
            "volumes": 100, "pages": 1000, "seconds": 1000.0,
            "recent": [{"pages": 100, "seconds": 4.0, "at": 10.0},
                       {"pages": 100, "seconds": 4.0, "at": 20.0}],
            "last_at": 20.0,
        }
        found = profile_throughput(runs)
        assert found is not None
        assert found.pages_per_second == pytest.approx(25.0)
        assert (found.volumes, found.last_at) == (2, 20.0)

    def test_an_older_profile_reads_its_congestion_records(self) -> None:
        """A profile written before `recent` existed still has its last few
        runs' own volume pages and seconds -- the pipeline's item counters
        (`pages`/`elapsed`) are not pages and are never read."""
        runs = {
            "volumes": 50, "pages": 5000, "seconds": 1000.0,
            "congestion": [
                {"at": 5.0, "pages": 999, "elapsed": 1.0,
                 "volume_pages": 60, "volume_seconds": 3.0},
                {"at": 9.0, "pages": 999, "elapsed": 1.0,
                 "volume_pages": 40, "volume_seconds": 2.0},
                {"at": 11.0, "pages": 999, "elapsed": 1.0},
            ],
        }
        found = profile_throughput(runs)
        assert found is not None
        assert found.pages_per_second == pytest.approx(20.0)
        assert (found.volumes, found.last_at) == (2, 9.0)

    def test_else_the_cumulative_totals(self) -> None:
        found = profile_throughput({"volumes": 7, "pages": 700, "seconds": 350.0})
        assert found is not None
        assert found.pages_per_second == pytest.approx(2.0)
        assert found.volumes == 7
        assert profile_throughput({}) is None
        assert profile_throughput(None) is None

    def test_overlapping_volumes_share_their_seconds(self) -> None:
        """A pipelined session has the next volume in flight before the last
        one is out, so each volume's own seconds overlap its neighbours'.
        Summed, the overlap counted twice: the Processors card read ~30%
        under the benchmark on an idle machine (tower 7.3-7.8 against 11.0).
        With each volume's end, the figure is pages over the wall clock the
        volumes' windows cover."""
        # [0, 10] and [5, 15]: 200 pages in 15 wall seconds, not 20.
        found = throughput_of([(100, 10.0, 10.0), (100, 10.0, 15.0)])
        assert found is not None
        assert found.seconds == pytest.approx(15.0)
        assert found.pages_per_second == pytest.approx(200 / 15)
        assert found.volumes == 2

    def test_time_between_volumes_is_not_counted(self) -> None:
        # [0, 10] then nothing, then [100, 110]: busy 20 s, not 110.
        found = throughput_of([(100, 10.0, 10.0), (100, 10.0, 110.0)])
        assert found is not None and found.seconds == pytest.approx(20.0)

    def test_a_window_inside_another_adds_nothing(self) -> None:
        found = throughput_of([(300, 30.0, 30.0), (50, 5.0, 12.0), (50, 5.0, 40.0)])
        # [0, 30] holds [7, 12]; [35, 40] is apart: 35 wall seconds.
        assert found is not None and found.seconds == pytest.approx(35.0)

    def test_a_volume_without_an_end_stands_alone(self) -> None:
        # No end to place it by: its seconds are its own, as before.
        found = throughput_of([(100, 10.0, 10.0), (100, 10.0, 15.0), (50, 5.0)])
        assert found is not None and found.seconds == pytest.approx(20.0)

    def test_a_profiles_recent_runs_are_placed_by_when_they_finished(self) -> None:
        runs = {
            "recent": [{"pages": 100, "seconds": 10.0, "at": 110.0},
                       {"pages": 100, "seconds": 10.0, "at": 114.0},
                       {"pages": 100, "seconds": 10.0, "at": 118.0}],
            "last_at": 118.0,
        }
        found = profile_throughput(runs)
        # [100, 118]: 300 pages in 18 s, where the summed seconds said 30.
        assert found is not None
        assert found.pages_per_second == pytest.approx(300 / 18)
        assert (found.volumes, found.last_at) == (3, 118.0)

    def test_congestion_records_are_placed_by_their_stamp(self) -> None:
        records = [
            {"at": 50.0, "volume_pages": 40, "volume_seconds": 20.0},
            {"at": 60.0, "volume_pages": 40, "volume_seconds": 20.0},
        ]
        found = records_throughput(records)
        assert found is not None and found.seconds == pytest.approx(30.0)

    def test_records_keep_the_newest(self) -> None:
        records = [{"volume_pages": 10, "volume_seconds": 1.0, "at": float(i)}
                   for i in range(RECENT_VOLUMES + 5)]
        found = records_throughput(records)
        assert found is not None
        assert found.volumes == RECENT_VOLUMES
        assert found.last_at == float(RECENT_VOLUMES + 4)


class TestRecordRun:
    def test_it_keeps_the_recent_volumes_and_the_last_run(self, tmp_path: Path) -> None:
        store = ProcessorProfiles(tmp_path)
        for n in range(RECENT_VOLUMES + 3):
            store.record_run("tower", "g-1", pages=100 + n, seconds=4.0, congestion=None)
        runs = store.row("tower", "g-1").runs  # type: ignore[union-attr]
        assert runs["volumes"] == RECENT_VOLUMES + 3
        assert len(runs["recent"]) == RECENT_VOLUMES
        assert runs["recent"][-1]["pages"] == 100 + RECENT_VOLUMES + 2
        assert runs["last_at"] == runs["recent"][-1]["at"] > 0


# --- the admin endpoint ----------------------------------------------------------


def _request(app: Callable[..., Any], path: str, role: str = "admin") -> tuple[int, Any]:
    environ = {
        "REQUEST_METHOD": "GET", "SCRIPT_NAME": "", "PATH_INFO": path,
        "QUERY_STRING": "", "SERVER_NAME": "localhost", "SERVER_PORT": "8080",
        "SERVER_PROTOCOL": "HTTP/1.1", "wsgi.version": (1, 0), "wsgi.url_scheme": "http",
        "wsgi.input": io.BytesIO(b""), "wsgi.errors": io.StringIO(),
        "CONTENT_LENGTH": "0", "mokuro.role": role, "mokuro.username": role,
    }
    status: list[str] = []

    def start_response(line: str, headers: list[tuple[str, str]], exc: Any = None) -> Any:
        status.append(line)
        return lambda data: None

    raw = b"".join(app(environ, start_response))
    try:
        body = json.loads(raw.decode() or "{}")
    except ValueError:
        body = raw.decode(errors="replace")
    return int(status[0].split()[0]), body


def _nothing(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b""]


class _Control:
    def __init__(self, registry: ProcessorRegistry) -> None:
        self.remote = registry
        self.worker = None
        self.selected_backend = None
        self.bench: Any = None
        self.bench_factory: Any = None
        self.runtime: Any = None

    def processing_hold(self) -> None:
        return None


@pytest.fixture
def admin(tmp_path: Path) -> Iterator[tuple[AdminAPI, Config, ProcessorRegistry]]:
    storage = tmp_path / "storage"
    for name in ("library", "inbox", "users"):
        (storage / name).mkdir(parents=True)
    config = Config()
    config.storage.base_path = storage
    config.ocr.generations = parse_generation_list([dict(r) for r in ROWS])
    registry = ProcessorRegistry(local_name="this server")
    app = AdminAPI(
        _nothing, Database(storage / "mokuro.db"), AdminConfig(enabled=True, path="/_admin"),
        full_config=config, config_path=tmp_path / "config.yaml",
        ocr_control=_Control(registry),  # type: ignore[arg-type]
    )
    yield app, config, registry


def _connect(registry: ProcessorRegistry, name: str) -> Any:
    entry = registry.register(username=name, name=name,
                              host={"cpu": "Threadripper", "gpu": "RTX 4090"},
                              catalog={"engines": ["mokuro", "hayai-nova"]}, max_sessions=1)
    entry.stream_open = True
    return entry


def _layers(body: dict[str, Any], name: str) -> dict[str, dict[str, Any]]:
    (machine,) = [m for m in body["speed"] if m["name"] == name]
    return {layer["generation"]: layer for layer in machine["layers"]}


class TestAdminProcessorsSpeed:
    def test_per_processor_per_layer_real_throughput(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry], monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, config, registry = admin
        _connect(registry, "tower")
        mokuro, hayai = config.ocr.generations
        store = ProcessorProfiles(config.storage.base_path)
        # Four volumes one after another, each finishing 4 s after the last:
        # the clock the profile stamps them with has to move for that.
        clock = iter([1000.0 + 4.0 * n for n in range(1, 5)])
        monkeypatch.setattr(profiles_module.time, "time", lambda: next(clock))
        for _ in range(4):
            store.record_run("tower", mokuro.id, pages=100, seconds=4.0, congestion=None,
                             recipe=mokuro.output_affecting())
        monkeypatch.undo()
        store.set_bench("tower", mokuro.id, {"precision": "fp32", "pages_per_second": 48.9, "at": "x"},
                        recipe=mokuro.output_affecting())
        store.set_bench("tower", hayai.id, {"precision": "fp32", "pages_per_second": 11.0},
                        recipe=hayai.output_affecting())
        status, body = _request(app, "/_admin/api/processors")
        assert status == 200
        layers = _layers(body, "tower")
        # 400 pages over 16 s: 25 pages/s, 1500 a minute -- over 4 volumes.
        assert layers["mokuro"]["pages_per_minute"] == 1500.0
        assert layers["mokuro"]["volumes"] == 4
        assert layers["mokuro"]["bench_pages_per_minute"] == pytest.approx(2934.0)
        assert layers["mokuro"]["last_at"] > 0
        # Benchmarked but never run: the benchmark, and no throughput.
        assert layers["hayai-ctd"]["pages_per_minute"] is None
        assert layers["hayai-ctd"]["volumes"] == 0
        assert layers["hayai-ctd"]["bench_pages_per_minute"] == pytest.approx(660.0)
        (tower,) = [m for m in body["speed"] if m["name"] == "tower"]
        assert tower["connected"] is True and tower["local"] is False

    def test_a_pipelined_session_is_not_charged_its_overlap_twice(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry], monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The card beside the benchmark: 10 s volumes finishing every 5 s
        (two in flight at once) are 20 pages/s, not the 10 their summed
        seconds make."""
        app, config, registry = admin
        _connect(registry, "tower")
        mokuro = config.ocr.generations[0]
        store = ProcessorProfiles(config.storage.base_path)
        clock = iter([1000.0 + 5.0 * n for n in range(1, 7)])
        monkeypatch.setattr(profiles_module.time, "time", lambda: next(clock))
        for _ in range(6):
            store.record_run("tower", mokuro.id, pages=100, seconds=10.0, congestion=None,
                             recipe=mokuro.output_affecting())
        monkeypatch.undo()
        _, body = _request(app, "/_admin/api/processors")
        # Windows [995, 1005], [1000, 1010] ... [1020, 1030]: 600 pages in 35 s.
        assert _layers(body, "tower")["mokuro"]["pages_per_minute"] == round(600 / 35 * 60, 1)

    def test_this_server_reads_its_own_history(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, _registry = admin
        mokuro = config.ocr.generations[0]
        history = CongestionHistory(config.storage.base_path)
        history.save({mokuro.id: [
            {"at": 100.0, "pages": 7, "elapsed": 1.0, "volume_pages": 30, "volume_seconds": 60.0},
            {"at": 200.0, "pages": 7, "elapsed": 1.0, "volume_pages": 30, "volume_seconds": 60.0},
        ]})
        _, body = _request(app, "/_admin/api/processors")
        assert body["speed"][0]["name"] == "local"
        layers = _layers(body, "local")
        assert layers["mokuro"]["pages_per_minute"] == 30.0
        assert layers["mokuro"]["volumes"] == 2
        assert layers["mokuro"]["last_at"] == 200.0

    def test_a_disconnected_processor_keeps_its_numbers(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, _registry = admin
        mokuro = config.ocr.generations[0]
        ProcessorProfiles(config.storage.base_path).record_run(
            "old-laptop", mokuro.id, pages=60, seconds=60.0, congestion=None,
            recipe=mokuro.output_affecting(),
        )
        _, body = _request(app, "/_admin/api/processors")
        (laptop,) = [m for m in body["speed"] if m["name"] == "old-laptop"]
        assert laptop["connected"] is False
        assert _layers(body, "old-laptop")["mokuro"]["pages_per_minute"] == 60.0

    def test_a_row_measured_with_another_recipe_is_not_this_ones(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _connect(registry, "tower")
        mokuro = config.ocr.generations[0]
        ProcessorProfiles(config.storage.base_path).record_run(
            "tower", mokuro.id, pages=60, seconds=1.0, congestion=None,
            recipe=["some", "other", "recipe"],
        )
        _, body = _request(app, "/_admin/api/processors")
        assert _layers(body, "tower") == {}

    def test_not_for_a_non_admin(self, admin: tuple[AdminAPI, Config, ProcessorRegistry]) -> None:
        app, _config, _registry = admin
        status, _body = _request(app, "/_admin/api/processors", role="registered")
        assert status in (401, 403)

    def test_never_on_the_public_queue(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        _app, config, _registry = admin
        mokuro = config.ocr.generations[0]
        ProcessorProfiles(config.storage.base_path).record_run(
            "tower", mokuro.id, pages=100, seconds=4.0, congestion=None,
            recipe=mokuro.output_affecting(),
        )
        queue_config = QueueConfig()
        queue = QueueAPI(_nothing, storage_base_path=str(config.storage.base_path),
                         generations=list(config.ocr.generations), queue_config=queue_config)
        for level in ("minimal", "normal", "detailed"):
            queue_config.display = level
            status, body = _request(queue, "/queue/api/status")
            assert status == 200, level
            text = json.dumps(body)
            assert "1500" not in text and "bench_pages_per_minute" not in text, level
            assert "tower" not in text, level


# --- the hardware beside each machine's numbers ------------------------------------


class _Probe:
    """`describe_host`, counted: the local probe must run once per server."""

    def __init__(self) -> None:
        self.calls = 0

    def __call__(self, backend: Any, engines_python: Any) -> dict[str, Any]:
        self.calls += 1
        return {"cpu": "Ryzen 9 7950X (16 cores)", "gpu": "Radeon RX 7900 XTX",
                "backend": backend}


def _probed(app: AdminAPI) -> None:
    """Wait for the background probe of this server's hardware to answer."""
    thread = app._local_host_thread
    assert thread is not None, "the first request starts the probe"
    thread.join(timeout=10)
    assert not thread.is_alive()


class TestAdminProcessorsHardware:
    def test_this_server_sends_its_own_hardware_probed_once(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry], monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The same probe a processor registers with (`describe_host`), asked
        once in the background and kept -- never once a request."""
        app, _config, _registry = admin
        probe = _Probe()
        monkeypatch.setattr(bench_module, "describe_host", probe)
        _request(app, "/_admin/api/processors")
        _probed(app)
        for _ in range(3):
            _, body = _request(app, "/_admin/api/processors")
        assert probe.calls == 1
        hardware = {"cpu": "Ryzen 9 7950X (16 cores)", "gpu": "Radeon RX 7900 XTX"}
        (local_row,) = [p for p in body["processors"] if p["local"]]
        assert local_row["host"] == hardware
        (local_speed,) = [m for m in body["speed"] if m["local"]]
        assert local_speed["host"] == hardware

    def test_until_the_probe_answers_this_server_has_no_hardware(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry], monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, _config, _registry = admin
        release = threading.Event()

        def slow(backend: Any, engines_python: Any) -> dict[str, Any]:
            release.wait(10)
            return {"cpu": "late", "gpu": None}

        monkeypatch.setattr(bench_module, "describe_host", slow)
        status, body = _request(app, "/_admin/api/processors")
        release.set()
        assert status == 200, "the panel never waits on the probe"
        (local_speed,) = [m for m in body["speed"] if m["local"]]
        assert local_speed["host"] is None

    def test_a_probe_that_fails_still_names_the_cpu(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry], monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, _config, _registry = admin

        def broken(backend: Any, engines_python: Any) -> dict[str, Any]:
            raise RuntimeError("no torch")

        monkeypatch.setattr(bench_module, "describe_host", broken)
        monkeypatch.setattr(bench_module, "cpu_label", lambda: "N100 (4 cores)")
        _request(app, "/_admin/api/processors")
        _probed(app)
        _, body = _request(app, "/_admin/api/processors")
        (local_speed,) = [m for m in body["speed"] if m["local"]]
        assert local_speed["host"] == {"cpu": "N100 (4 cores)", "gpu": None}

    def test_an_offline_processor_carries_the_hardware_it_last_registered_with(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        """`processors/<name>.json` keeps the host a machine registered with
        (`set_identity`): the table names an offline machine's hardware from
        there."""
        app, config, _registry = admin
        mokuro = config.ocr.generations[0]
        store = ProcessorProfiles(config.storage.base_path)
        store.set_identity("old-laptop",
                           host={"cpu": "i5-8250U (4 cores)", "gpu": "MX150", "backend": "cuda"},
                           catalog={"engines": ["mokuro"]})
        store.record_run("old-laptop", mokuro.id, pages=60, seconds=60.0, congestion=None,
                         recipe=mokuro.output_affecting())
        _, body = _request(app, "/_admin/api/processors")
        (laptop,) = [m for m in body["speed"] if m["name"] == "old-laptop"]
        assert laptop["connected"] is False
        assert laptop["host"] == {"cpu": "i5-8250U (4 cores)", "gpu": "MX150"}

    def test_a_profile_from_before_hardware_was_kept_has_none(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, _registry = admin
        mokuro = config.ocr.generations[0]
        ProcessorProfiles(config.storage.base_path).record_run(
            "old-laptop", mokuro.id, pages=60, seconds=60.0, congestion=None,
            recipe=mokuro.output_affecting(),
        )
        _, body = _request(app, "/_admin/api/processors")
        (laptop,) = [m for m in body["speed"] if m["name"] == "old-laptop"]
        assert laptop["host"] is None
        assert _layers(body, "old-laptop")["mokuro"]["pages_per_minute"] == 60.0

    def test_a_connected_processor_carries_the_hardware_it_registered_with(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _connect(registry, "tower")
        mokuro = config.ocr.generations[0]
        ProcessorProfiles(config.storage.base_path).record_run(
            "tower", mokuro.id, pages=60, seconds=60.0, congestion=None,
            recipe=mokuro.output_affecting(),
        )
        _, body = _request(app, "/_admin/api/processors")
        (tower,) = [m for m in body["speed"] if m["name"] == "tower"]
        assert tower["host"] == {"cpu": "Threadripper", "gpu": "RTX 4090"}
