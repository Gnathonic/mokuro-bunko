"""Pools, devices and stages, asked for one processor at a time (spec section 4)."""

from __future__ import annotations

import io
import json
import sys
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import AdminConfig, Config
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.devices import (
    DeviceCatalog,
    GpuDevice,
    catalog_from_entries,
    merge_catalogs,
    set_cached_catalog,
)
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.watcher import OCRWorker

ROWS: list[dict[str, Any]] = [
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"},
]
TOWER_DEVICES = [
    {"id": "auto", "label": "Auto — GPU 0 when available"},
    {"id": "cpu", "label": "Threadripper (48 cores)"},
    {"id": "gpu:0", "label": "GPU 0 — RTX 4090 (24 GB)"},
    {"id": "gpu:1", "label": "GPU 1 — RTX 4090 (24 GB)"},
]
CATALOG: dict[str, Any] = {
    "engines": ["mokuro", "hayai-nova"], "detectors": ["ctd"],
    "devices": TOWER_DEVICES, "serves_mokuro": True,
}
ONE_CARD = DeviceCatalog(gpus=(GpuDevice(index=0, name="RX 9070 XT"),), probed=True)


@pytest.fixture(autouse=True)
def _no_published_catalog() -> Iterator[None]:
    """The process-wide probe cache is shared: never leak one between tests."""
    set_cached_catalog(None)
    yield
    set_cached_catalog(None)


class TestTheCatalogFromEntries:
    def test_entries_become_a_catalog_with_the_same_ids(self) -> None:
        catalog = catalog_from_entries(TOWER_DEVICES)
        assert catalog.ids() == ("auto", "cpu", "gpu:0", "gpu:1")
        assert catalog.has_gpu is True
        assert catalog.probed is True
        assert catalog.knows("gpu:1") is True
        assert catalog.knows("gpu:5") is False

    def test_a_remote_label_is_not_wrapped_twice(self) -> None:
        catalog = catalog_from_entries(TOWER_DEVICES)
        assert [row["label"] for row in catalog.entries() if row["id"] == "gpu:0"] == [
            "GPU 0 — RTX 4090 (24 GB)"
        ]

    def test_a_catalog_with_no_gpus_knows_it(self) -> None:
        catalog = catalog_from_entries([{"id": "auto", "label": "Auto — CPU"},
                                        {"id": "cpu", "label": "CPU (4 cores)"}])
        assert catalog.has_gpu is False
        assert catalog.knows("gpu:0") is False

    def test_an_empty_list_is_an_unprobed_catalog(self) -> None:
        assert catalog_from_entries([]).probed is False

    def test_rows_that_are_not_mappings_are_skipped(self) -> None:
        catalog = catalog_from_entries(["gpu:0", 7, {"id": "gpu:2", "label": "GPU 2 — X"}])
        assert catalog.ids() == ("auto", "cpu", "gpu:2")


class TestMergedCatalogs:
    def test_every_machines_cards_are_settable(self) -> None:
        merged = merge_catalogs([ONE_CARD, catalog_from_entries(TOWER_DEVICES)])
        assert merged.knows("gpu:1") is True
        assert merged.knows("gpu:2") is False

    def test_an_unprobed_machine_keeps_the_merge_unprobed(self) -> None:
        merged = merge_catalogs([DeviceCatalog(), catalog_from_entries(TOWER_DEVICES)])
        assert merged.probed is False
        assert merged.knows("gpu:7") is True


class TestThisServerLeavesAProcessorsCardToIt:
    def test_a_row_pinned_to_a_card_this_server_has_not_got_is_not_run_here(
        self, tmp_path: Path
    ) -> None:
        """Once a row may name a processor's card, this box must decline it
        rather than fail a session start on a card it does not have."""
        rows = parse_generation_list(
            [ROWS[0], {**ROWS[1], "pools": {"stage_device": {"engine": "gpu:1"}}}],
            devices=merge_catalogs([ONE_CARD, catalog_from_entries(TOWER_DEVICES)]),
        )
        local = OCRProcessor(
            storage_path=tmp_path, generations=rows, engines_python_path=Path(sys.executable)
        )
        set_cached_catalog(ONE_CARD)
        reason = local.can_run(rows[1])
        assert reason is not None and "gpu:1" in reason
        assert local.can_run(rows[0]) is None

    def test_an_unprobed_server_refuses_nothing(self, tmp_path: Path) -> None:
        rows = parse_generation_list(
            [ROWS[0], {**ROWS[1], "pools": {"stage_device": {"engine": "gpu:1"}}}],
            devices=DeviceCatalog(),
        )
        local = OCRProcessor(
            storage_path=tmp_path, generations=rows, engines_python_path=Path(sys.executable)
        )
        assert local.can_run(rows[1]) is None


def _worker(tmp_path: Path, registry: ProcessorRegistry) -> OCRWorker:
    for name in ("library", "inbox"):
        (tmp_path / name).mkdir(exist_ok=True)
    return OCRWorker(
        storage_path=tmp_path,
        poll_interval=30.0,
        generations=parse_generation_list([dict(r) for r in ROWS]),
        engines_python_path=Path(sys.executable),
        remote=registry,
        local_processing=False,
    )


def _entry(registry: ProcessorRegistry, name: str) -> Any:
    entry = registry.register(username=name, name=name,
                              host={"cpu": "Threadripper (48 cores)", "gpu": "RTX 4090"},
                              catalog=CATALOG, max_sessions=1)
    entry.stream_open = True
    return entry


class TestTheRowSpecOnTheWire:
    def test_without_a_profile_the_rows_own_pools_go_out(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        entry = _entry(registry, "tower")
        worker = _worker(tmp_path, registry)
        row = worker.generations[1]
        assert worker._remote_row_spec(entry, row)["pools"] == row.pools.to_dict()

    def test_a_profile_replaces_the_pools_for_that_processor_only(
        self, tmp_path: Path
    ) -> None:
        registry = ProcessorRegistry()
        tower = _entry(registry, "tower")
        box = _entry(registry, "box")
        worker = _worker(tmp_path, registry)
        row = worker.generations[1]
        ProcessorProfiles(tmp_path).set_pools(
            "tower", row.id,
            {"stage_workers": {"detect": 6}, "queue_capacity": {},
             "stage_device": {"engine": "gpu:1"}},
            recipe=row.output_affecting(),
        )
        spec = worker._remote_row_spec(tower, row)
        assert spec["pools"]["stage_workers"] == {"detect": 6}
        assert spec["pools"]["stage_device"] == {"engine": "gpu:1"}
        assert worker._remote_row_spec(box, row)["pools"] == row.pools.to_dict()

    def test_the_name_and_id_still_come_from_the_row(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        entry = _entry(registry, "tower")
        worker = _worker(tmp_path, registry)
        row = worker.generations[1]
        ProcessorProfiles(tmp_path).set_pools("tower", row.id,
                                              {"stage_workers": {"detect": 6}})
        spec = worker._remote_row_spec(entry, row)
        assert spec["id"] == row.id
        assert spec["engine"] == "hayai-nova"
        assert spec["detector"] == "ctd"

    def test_pools_tuned_for_another_recipe_are_not_sent(self, tmp_path: Path) -> None:
        """A row whose detector changed runs another pipeline: its old
        processor pools may name stages the new road has not got."""
        registry = ProcessorRegistry()
        entry = _entry(registry, "tower")
        worker = _worker(tmp_path, registry)
        row = worker.generations[1]
        ProcessorProfiles(tmp_path).set_pools(
            "tower", row.id, {"stage_workers": {"detect": 6}},
            recipe=("hayai-nova", "ppocr-manga", 512),
        )
        assert worker._remote_row_spec(entry, row)["pools"] == row.pools.to_dict()


class TestATableTheMachineLeavesEmpty:
    """A machine's pools are read TABLE BY TABLE: one it leaves empty says
    nothing about the machine, so the row's own table runs for it (as all
    three empty already did, `profiles.holds_pools`). What the pre-fix
    auto-benchmark stored for hayai-nova + ppocr-manga on both machines:
    ``{stage_workers: {detect: 2}, queue_capacity: {}, stage_device: {}}``
    against a row whose table is ``{detect: 2}`` + ``{engine: 4, post: 4}``.
    Read as a whole table it dropped the row's capacities on those machines
    for good; the benchmark never chose a capacity."""

    PINNED: dict[str, Any] = {
        **ROWS[1],
        "pools": {"stage_workers": {"detect": 2},
                  "queue_capacity": {"engine": 4, "post": 4}},
    }

    def _setup(self, tmp_path: Path) -> tuple[OCRWorker, Any, Any]:
        registry = ProcessorRegistry()
        entry = _entry(registry, "tower")
        for name in ("library", "inbox"):
            (tmp_path / name).mkdir(exist_ok=True)
        worker = OCRWorker(
            storage_path=tmp_path, poll_interval=30.0,
            generations=parse_generation_list([dict(ROWS[0]), dict(self.PINNED)]),
            engines_python_path=Path(sys.executable), remote=registry,
            local_processing=False,
        )
        return worker, entry, worker.generations[1]

    def test_the_pre_fix_best_runs_the_row_s_own_capacities(self, tmp_path: Path) -> None:
        worker, entry, row = self._setup(tmp_path)
        ProcessorProfiles(tmp_path).set_pools(
            "tower", row.id,
            {"stage_workers": {"detect": 2}, "queue_capacity": {}, "stage_device": {}},
            recipe=row.output_affecting(),
        )
        assert worker._remote_row_spec(entry, row)["pools"] == row.pools.to_dict()

    def test_a_table_the_machine_names_is_still_its_own(self, tmp_path: Path) -> None:
        worker, entry, row = self._setup(tmp_path)
        ProcessorProfiles(tmp_path).set_pools(
            "tower", row.id,
            {"stage_workers": {"detect": 3}, "queue_capacity": {"engine": 8},
             "stage_device": {}},
            recipe=row.output_affecting(),
        )
        pools = worker._remote_row_spec(entry, row)["pools"]
        assert pools["stage_workers"] == {"detect": 3}
        assert pools["queue_capacity"] == {"engine": 8}, "the whole table, not merged"

    def test_auto_against_a_row_s_width_or_capacity_is_derived_there(
        self, tmp_path: Path
    ) -> None:
        """How a machine says "derived" where the row pins one: ``auto``,
        which the runner never sees (an absent key is how IT is told)."""
        worker, entry, row = self._setup(tmp_path)
        ProcessorProfiles(tmp_path).set_pools(
            "tower", row.id,
            {"stage_workers": {"detect": "auto"},
             "queue_capacity": {"engine": "auto", "post": "auto"}, "stage_device": {}},
            recipe=row.output_affecting(),
        )
        pools = worker._remote_row_spec(entry, row)["pools"]
        assert pools["stage_workers"] == {}
        assert pools["queue_capacity"] == {}


# --- the admin API ------------------------------------------------------------


def _request(
    app: Callable[..., Any], method: str, path: str, body: dict[str, Any] | None = None
) -> tuple[int, dict[str, Any]]:
    content = json.dumps(body).encode() if body is not None else b""
    environ = {
        "REQUEST_METHOD": method, "SCRIPT_NAME": "", "PATH_INFO": path,
        "QUERY_STRING": "", "SERVER_NAME": "localhost", "SERVER_PORT": "8080",
        "SERVER_PROTOCOL": "HTTP/1.1", "wsgi.version": (1, 0), "wsgi.url_scheme": "http",
        "wsgi.input": io.BytesIO(content), "wsgi.errors": io.StringIO(),
        "CONTENT_LENGTH": str(len(content)), "CONTENT_TYPE": "application/json",
        "mokuro.role": "admin", "mokuro.username": "admin",
    }
    status: list[str] = []

    def start_response(line: str, headers: list[tuple[str, str]], exc: Any = None) -> Any:
        status.append(line)
        return lambda data: None

    raw = b"".join(app(environ, start_response))
    return int(status[0].split()[0]), json.loads(raw.decode() or "{}")


def _nothing(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b""]


class _Control:
    """What the admin API reads off the OCR control handle, and no more."""

    def __init__(self, registry: ProcessorRegistry) -> None:
        self.remote = registry
        self.worker = None
        self.selected_backend = None
        self.bench: Any = None
        self.bench_factory: Any = None
        self.runtime: Any = None

    def apply(self, generations: Any, poll_interval: Any = None) -> dict[str, Any]:
        del generations, poll_interval
        return {"applied": True, "installing": False, "restart_required": False,
                "reason": ""}


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
    set_cached_catalog(ONE_CARD)
    yield app, config, registry


class TestTheAdminApi:
    def test_the_payload_lists_processors_and_their_numbers_per_row(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _entry(registry, "tower")
        row = config.ocr.generations[1]
        store = ProcessorProfiles(config.storage.base_path)
        store.set_pools("tower", row.id, {"stage_workers": {"detect": 6}},
                        recipe=row.output_affecting())
        store.record_run(
            "tower", row.id, pages=200, seconds=10.0,
            congestion={"bottleneck": "detect", "verdict": None, "items": 200,
                        "elapsed_seconds": 10.0,
                        "stages": [{"key": "detect", "workers": 3, "items": 200, "busy_pct": 90,
                                    "starved_pct": 0, "blocked_pct": 5}]},
            recipe=row.output_affecting(),
        )
        status, body = _request(app, "GET", "/_admin/api/ocr/generations")
        assert status == 200
        assert [p["name"] for p in body["processors"]] == ["tower"]
        hayai = body["generations"][1]
        assert hayai["processor_pools"] == {"tower": {"stage_workers": {"detect": 6}}}
        assert hayai["processor_runs"]["tower"]["pages_per_second"] == pytest.approx(20.0)
        assert hayai["processor_congestion"]["tower"]["bottleneck"] == "detect", (
            "the last few runs' congestion, averaged per machine"
        )
        assert hayai["congestion"] is None, "and never blended into this server's"
        assert body["generations"][0]["processor_pools"] == {}, "no entry: the row's own"

    def test_a_processors_finished_volume_shows_its_queue_depths(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        """N2: the run a processor's volume leaves in its profile is the same
        stored record this server's history keeps (`build_record`), so the
        averaged payload carries that machine's queues -- not only its stages."""
        import zipfile

        from mokuro_bunko.ocr.watcher import _SessionClock, _SessionJob

        app, config, registry = admin
        storage = config.storage.base_path
        _entry(registry, "tower")
        cbz = storage / "library" / "Alpha" / "Volume 1.cbz"
        cbz.parent.mkdir(parents=True)
        with zipfile.ZipFile(cbz, "w") as zf:
            zf.writestr("page_000.jpg", b"fake image data")
        cbz.with_suffix(".mokuro").write_text(
            json.dumps({"version": "0.0", "volume_uuid": "u-1", "pages": [], "chars": 0}),
            encoding="utf-8",
        )
        worker = OCRWorker(
            storage_path=storage, poll_interval=30.0,
            generations=list(config.ocr.generations),
            engines_python_path=Path(sys.executable), sessions=True,
            remote=registry, local_processing=False, autobench=False,
        )
        slot = next(s for s in worker._all_slots() if s.processor_id != "local")
        job = worker.claim_next(slot)
        assert job is not None
        row = worker._generation(job[1])
        assert row is not None
        volume = slot.processor.prepare_session_volume(job[0], row, "v1")
        volume.output.write_text(json.dumps({"version": "0.0", "pages": [], "chars": 0}),
                                 encoding="utf-8")
        entry = _SessionJob(job=job, generation=row, volume=volume, slot=slot.index,
                            owner=slot, hardware="tower")
        stats = {
            "elapsed_seconds": 2.0, "items": 10, "bottleneck": "detect",
            "stages": [
                {"key": "detect", "name": "detect", "device": "cpu", "workers": 1,
                 "items": 10, "busy_seconds": 1.9, "blocked_seconds": 0.0,
                 "starved_seconds": 0.0},
                {"key": "engine", "name": "engine", "device": "gpu", "workers": 1,
                 "items": 10, "busy_seconds": 0.8, "blocked_seconds": 0.0,
                 "starved_seconds": 1.0},
            ],
            "queues": [{"name": "detect->engine", "capacity": 4, "mean_depth": 2.5,
                        "max_depth": 4}],
        }
        worker._handle_session_event(
            {"event": "volume_done", "id": "v1", "pages": 10, "seconds": 2.0,
             "stats": stats},
            row, {"v1": entry}, ["v1"], _SessionClock(), hardware="tower",
        )
        status, body = _request(app, "GET", "/_admin/api/ocr/generations")
        assert status == 200
        averaged = body["generations"][1]["processor_congestion"]["tower"]
        assert averaged["queues"] == [
            {"name": "detect->engine", "capacity": 4, "mean_depth": 2.5, "max_depth": 4}
        ]
        assert averaged["bottleneck"] == "detect"
        stored = ProcessorProfiles(storage).row("tower", row.id).runs["congestion"][0]
        assert stored["volume"] == "Alpha/Volume 1.cbz"
        assert (stored["volume_pages"], stored["volume_seconds"]) == (10, 2.0)

    def test_derive_answers_for_the_machine_asked_about(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _entry(registry, "tower")
        spec = config.ocr.generations[1].to_dict()
        status, local = _request(app, "POST", "/_admin/api/ocr/generations/derive",
                                 {"spec": spec})
        status_b, tower = _request(app, "POST", "/_admin/api/ocr/generations/derive",
                                   {"spec": spec, "processor": "tower"})
        assert status == status_b == 200
        engine_local = next(s for s in local["stages"] if s["key"] == "engine")
        engine_tower = next(s for s in tower["stages"] if s["key"] == "engine")
        assert "gpu:1" in engine_tower["devices_allowed"], "tower's second card is offered"
        assert "gpu:1" in engine_local["devices_allowed"], (
            "the row's own table is every machine's default"
        )

    def test_derive_for_a_processor_that_is_not_connected_is_refused(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, _registry = admin
        status, body = _request(
            app, "POST", "/_admin/api/ocr/generations/derive",
            {"spec": config.ocr.generations[1].to_dict(), "processor": "nobody"},
        )
        assert status == 400 and "nobody" in body["error"]

    def test_pools_are_saved_for_one_processor_and_never_in_the_config(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _entry(registry, "tower")
        row = config.ocr.generations[1]
        before = [r.to_dict() for r in config.ocr.generations]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower",
             "pools": {"stage_workers": {"detect": 4}, "stage_device": {"engine": "gpu:1"}}},
        )
        assert status == 200, body
        assert body["pools"]["stage_device"] == {"engine": "gpu:1"}
        saved = ProcessorProfiles(config.storage.base_path).row(
            "tower", row.id, recipe=row.output_affecting()
        )
        assert saved is not None and saved.pools["stage_workers"] == {"detect": 4}
        assert [r.to_dict() for r in config.ocr.generations] == before

    def test_auto_against_a_row_s_pin_is_kept_as_what_that_machine_runs(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        """Pools that name nothing are no opinion, so a machine that should
        run a stage ``auto`` where the row pins it is saved saying ``auto``."""
        app, config, registry = admin
        entry = _entry(registry, "tower")
        rows = [dict(r) for r in ROWS]
        rows[1]["pools"] = {"stage_device": {"detect": "cpu"}}
        status, body = _request(app, "PUT", "/_admin/api/ocr/generations",
                                {"generations": rows})
        assert status == 200, body
        row = config.ocr.generations[1]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower", "pools": {"stage_device": {"detect": "auto"}}},
        )
        assert status == 200, body
        assert body["pools"]["stage_device"] == {"detect": "auto"}
        worker = OCRWorker(
            storage_path=config.storage.base_path, poll_interval=30.0,
            generations=list(config.ocr.generations),
            engines_python_path=Path(sys.executable), remote=registry,
            local_processing=False,
        )
        sent = worker._remote_row_spec(entry, row)["pools"]["stage_device"]
        assert sent.get("detect", "auto") == "auto", "not the row's cpu pin"

    def test_auto_against_a_row_s_width_and_capacity_is_saved_as_auto(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        """"Save for <machine>" with the row's pinned widths cleared: the
        machine runs them derived, and says so (an empty table would hand
        the stage back to the row's pin)."""
        app, config, registry = admin
        entry = _entry(registry, "tower")
        rows = [dict(r) for r in ROWS]
        rows[1]["pools"] = {"stage_workers": {"detect": 2}, "queue_capacity": {"post": 4}}
        status, body = _request(app, "PUT", "/_admin/api/ocr/generations",
                                {"generations": rows})
        assert status == 200, body
        row = config.ocr.generations[1]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower",
             "pools": {"stage_workers": {"detect": "auto"}, "queue_capacity": {"post": "auto"},
                       "stage_device": {}}},
        )
        assert status == 200, body
        assert body["pools"]["stage_workers"] == {"detect": "auto"}
        assert body["pools"]["queue_capacity"] == {"post": "auto"}
        worker = OCRWorker(
            storage_path=config.storage.base_path, poll_interval=30.0,
            generations=list(config.ocr.generations),
            engines_python_path=Path(sys.executable), remote=registry,
            local_processing=False,
        )
        sent = worker._remote_row_spec(entry, row)["pools"]
        assert sent["stage_workers"] == {} and sent["queue_capacity"] == {}

    def test_auto_for_a_stage_the_row_has_not_got_is_refused(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _entry(registry, "tower")
        row = config.ocr.generations[1]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower", "pools": {"stage_workers": {"nosuch": "auto"}}},
        )
        assert status == 400, body
        assert "nosuch" in body["error"]

    def test_an_entry_of_empty_tables_is_listed_as_no_entry(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _entry(registry, "tower")
        row = config.ocr.generations[1]
        ProcessorProfiles(config.storage.base_path).set_pools(
            "tower", row.id,
            {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
            recipe=row.output_affecting(),
        )
        status, body = _request(app, "GET", "/_admin/api/ocr/generations")
        assert status == 200
        assert "tower" not in body["generations"][1]["processor_pools"]

    def test_pools_naming_a_card_the_processor_has_not_got_are_refused(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, registry = admin
        _entry(registry, "tower")
        row = config.ocr.generations[1]
        status, body = _request(
            app, "PUT", f"/_admin/api/ocr/generations/{row.id}/pools",
            {"processor": "tower", "pools": {"stage_device": {"engine": "gpu:3"}}},
        )
        assert status == 400 and "gpu:3" in body["error"]

    def test_a_row_may_be_pinned_to_a_card_only_a_processor_has(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        """Spec section 3 rule 2's own example: a row pinned to gpu:1 needs
        a processor with two cards. The library box has ONE; tower has two."""
        app, config, registry = admin
        rows = [dict(r) for r in ROWS]
        rows[1]["pools"] = {"stage_device": {"engine": "gpu:1"}}
        status, body = _request(app, "PUT", "/_admin/api/ocr/generations",
                                {"generations": rows})
        assert status == 400, "no machine has gpu:1 yet"
        _entry(registry, "tower")
        status, body = _request(app, "PUT", "/_admin/api/ocr/generations",
                                {"generations": rows})
        assert status == 200, body
        assert config.ocr.generations[1].pools.stage_device == {"engine": "gpu:1"}

    def test_a_processor_that_is_offline_still_counts_by_its_profile(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, _registry = admin
        ProcessorProfiles(config.storage.base_path).set_identity(
            "tower", host={}, catalog=CATALOG
        )
        rows = [dict(r) for r in ROWS]
        rows[1]["pools"] = {"stage_device": {"engine": "gpu:1"}}
        status, body = _request(app, "PUT", "/_admin/api/ocr/generations",
                                {"generations": rows})
        assert status == 200, body

    def test_a_deleted_row_is_pruned_from_every_profile(
        self, admin: tuple[AdminAPI, Config, ProcessorRegistry]
    ) -> None:
        app, config, _registry = admin
        store = ProcessorProfiles(config.storage.base_path)
        row = config.ocr.generations[1]
        store.set_pools("tower", row.id, {"stage_workers": {"detect": 6}})
        status, _body = _request(app, "PUT", "/_admin/api/ocr/generations",
                                 {"generations": [config.ocr.generations[0].to_dict()]})
        assert status == 200
        assert store.row("tower", row.id) is None


# --- the offer gate checks the placement the session carries (B1/B9) -------------


BOX_DEVICES = [
    {"id": "auto", "label": "Auto — GPU 0 when available"},
    {"id": "cpu", "label": "Ryzen (8 cores)"},
    {"id": "gpu:0", "label": "GPU 0 — RTX 3060 (12 GB)"},
]


def _library_with_primary_done(storage: Path) -> None:
    import zipfile

    series = storage / "library" / "Alpha"
    series.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(series / "Volume 1.cbz", "w") as zf:
        zf.writestr("page_000.jpg", b"fake image data")
    (series / "Volume 1.mokuro").write_text(
        json.dumps({"version": "0.0", "volume_uuid": "u-1", "pages": [], "chars": 0}),
        encoding="utf-8",
    )


def _worker_for(
    tmp_path: Path, registry: ProcessorRegistry, rows: list[Any], logs: list[str]
) -> OCRWorker:
    for name in ("library", "inbox"):
        (tmp_path / name).mkdir(exist_ok=True)
    return OCRWorker(
        storage_path=tmp_path, poll_interval=30.0, status_callback=logs.append,
        generations=rows, engines_python_path=Path(sys.executable),
        remote=registry, local_processing=False, autobench=False,
    )


def _register(registry: ProcessorRegistry, name: str, devices: list[dict[str, Any]],
              sessions: int = 1) -> Any:
    entry = registry.register(
        username=name, name=name, host={"cpu": "x (8 cores)"},
        catalog={**CATALOG, "devices": devices}, max_sessions=sessions,
    )
    entry.stream_open = True
    return entry


def _slot_of(worker: OCRWorker, entry: Any) -> Any:
    return next(s for s in worker._all_slots() if s.processor_id == entry.processor_id)


class TestTheGateChecksWhatTheSessionRuns:
    def test_a_per_processor_override_makes_the_row_runnable_there(
        self, tmp_path: Path
    ) -> None:
        """B1 (a): the row's own table pins gpu:1 -- valid, tower has two
        cards -- and box (one card) is given its own pools on gpu:0. The
        gate must check box's pools, which the session will carry."""
        registry = ProcessorRegistry()
        box = _register(registry, "box", BOX_DEVICES)
        rows = parse_generation_list(
            [ROWS[0], {**ROWS[1], "pools": {"stage_device": {"engine": "gpu:1"}}}],
            devices=merge_catalogs([catalog_from_entries(BOX_DEVICES),
                                    catalog_from_entries(TOWER_DEVICES)]),
        )
        worker = _worker_for(tmp_path, registry, rows, [])
        _library_with_primary_done(tmp_path)
        slot = _slot_of(worker, box)
        assert slot.processor.can_run(rows[1]) is not None, "no override: not box's row"
        ProcessorProfiles(tmp_path).set_pools(
            "box", rows[1].id,
            {"stage_workers": {}, "queue_capacity": {}, "stage_device": {"engine": "gpu:0"}},
            recipe=rows[1].output_affecting(),
        )
        assert slot.processor.can_run(rows[1]) is None
        job = worker.claim_next(slot)
        assert job is not None and job[1] == rows[1].id
        assert worker._remote_row_spec(box, rows[1])["pools"]["stage_device"] == {
            "engine": "gpu:0"
        }

    def test_a_profile_pinning_a_card_the_processor_no_longer_reports_is_set_aside(
        self, tmp_path: Path
    ) -> None:
        """B1 (b): tower's pools were saved with engine on gpu:1 while it had
        two cards; it now registers with one. Sending gpu:1 would fail its
        runner before ready and strike the row off tower every scan."""
        registry = ProcessorRegistry()
        tower = _register(registry, "tower", BOX_DEVICES)
        logs: list[str] = []
        worker = _worker_for(tmp_path, registry,
                             parse_generation_list([dict(r) for r in ROWS]), logs)
        row = worker.generations[1]
        ProcessorProfiles(tmp_path).set_pools(
            "tower", row.id,
            {"stage_workers": {"detect": 6}, "queue_capacity": {},
             "stage_device": {"engine": "gpu:1"}},
            recipe=row.output_affecting(),
        )
        spec = worker._remote_row_spec(tower, row)
        assert "gpu:1" not in json.dumps(spec["pools"])
        assert spec["pools"] == row.pools.to_dict(), "the row's own table instead"
        assert _slot_of(worker, tower).processor.can_run(row) is None
        worker._remote_row_spec(tower, row)
        said = [line for line in logs if "no longer reports" in line]
        assert len(said) == 1, "said once, naming the profile"
        assert "gpu:1" in said[0] and "tower" in said[0]

    def test_a_processors_sessions_hold_its_own_cards(self, tmp_path: Path) -> None:
        """B9 (Task 9 M2): the device a remote session holds is the one it
        was opened with, on THAT machine's cards -- not the row's own table
        resolved against this server's."""
        set_cached_catalog(DeviceCatalog(probed=True))  # this server: no card
        registry = ProcessorRegistry()
        tower = _register(registry, "tower", TOWER_DEVICES, sessions=2)
        worker = _worker_for(tmp_path, registry,
                             parse_generation_list([dict(r) for r in ROWS]), [])
        row = worker.generations[1]

        class _Open:
            entry = tower
            generation = row
            row_spec = {"pools": {"stage_device": {"engine": "gpu:1"}}}

        worker._open_sessions.add(_Open())  # type: ignore[arg-type]
        assert worker._devices_in_use(_slot_of(worker, tower)) == {"gpu:1"}

    def test_a_new_session_on_a_processor_prefers_its_own_idle_card(
        self, tmp_path: Path
    ) -> None:
        """Addendum 7's spread, on a processor: g-2 sits on tower's gpu:1
        (busy), g-3 on its gpu:0 (idle), by tower's own profile."""
        set_cached_catalog(DeviceCatalog(probed=True))
        registry = ProcessorRegistry()
        tower = _register(registry, "tower", TOWER_DEVICES, sessions=2)
        rows = parse_generation_list([
            ROWS[0], ROWS[1],
            {"name": "hayai-ctd-b", "engine": "hayai-nova", "detector": "ctd"},
        ])
        worker = _worker_for(tmp_path, registry, rows, [])
        _library_with_primary_done(tmp_path)
        store = ProcessorProfiles(tmp_path)
        for row, card in ((rows[1], "gpu:1"), (rows[2], "gpu:0")):
            store.set_pools("tower", row.id,
                            {"stage_workers": {}, "queue_capacity": {},
                             "stage_device": {"engine": card}},
                            recipe=row.output_affecting())

        class _Open:
            entry = tower
            generation = rows[1]
            row_spec = {"pools": {"stage_device": {"engine": "gpu:1"}}}

        worker._open_sessions.add(_Open())  # type: ignore[arg-type]
        slots = [s for s in worker._all_slots() if s.processor_id == tower.processor_id]
        job = worker.claim_next(slots[1])
        assert job is not None and job[1] == rows[2].id, "the row on tower's idle card"
