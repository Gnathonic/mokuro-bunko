"""This server's OWN hardware is auto-benchmarked like a processor, unless
the row was configured by hand.

The owner's rule: "if the user didn't manually configure it, autobench". A
row is MANUALLY CONFIGURED when its own pools table in the config holds any
explicit value -- a ``stage_workers``, ``queue_capacity`` or ``stage_device``
entry, or a precision other than ``auto`` (`GenerationPools.is_empty`). Such
a row runs its table exactly as it always did. Any other row is benchmarked
on this server the first time a local slot would run it, through the same
gate, the same bench service and the same "store only what differs" rule as
a processor's autobench; the result is this server's own profile
(`profiles.LOCAL_PROFILE`), never the user's config file.
"""

from __future__ import annotations

import json
import sys
import time
import zipfile
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import AdminConfig, Config
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.bench import BenchService, _BenchRun, _spec_payload
from mokuro_bunko.ocr.devices import DeviceCatalog, set_cached_catalog
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.remote.profiles import (
    LOCAL_PROFILE,
    ProcessorProfiles,
    profile_filename,
)
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry, clean_processor_name
from mokuro_bunko.ocr.watcher import LOCAL_SLOT, OCRWorker
from tests.unit.test_ocr_bench import _script
from tests.unit.test_ocr_bench import _worker as bench_worker
from tests.unit.test_processor_speed import _Control, _nothing, _request

MOKURO: dict[str, Any] = {"name": "mokuro", "engine": "mokuro", "primary": True}
HAYAI: dict[str, Any] = {"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"}

# One explicit value of each kind a row's own table can hold.
CONFIGURED: list[dict[str, Any]] = [
    {"stage_workers": {"detect": 2}},
    {"queue_capacity": {"detect": 4}},
    {"stage_device": {"detect": "cpu"}},
    {"stage_device": {"detect": "auto"}},
]


@pytest.fixture(autouse=True)
def _no_published_catalog() -> Iterator[None]:
    set_cached_catalog(None)
    yield
    set_cached_catalog(None)


def _library(storage: Path, *volumes: str) -> None:
    """Volumes whose PRIMARY layer is there: only the hayai row is pending."""
    for volume in volumes:
        cbz = storage / "library" / "Alpha" / f"{volume}.cbz"
        cbz.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(cbz, "w") as zf:
            for n in range(4):
                zf.writestr(f"page_{n:03d}.jpg", b"fake image data" * 20)
        cbz.with_suffix(".mokuro").write_text(
            json.dumps({"version": "0.0", "volume_uuid": f"u-{volume}", "pages": [],
                        "chars": 0}),
            encoding="utf-8",
        )


def _rows(hayai: dict[str, Any] | None = None) -> list[GenerationSpec]:
    return parse_generation_list([dict(MOKURO), dict(hayai or HAYAI)], devices=DeviceCatalog())


class _Bench:
    """A bench service that records what it is asked, and settles on demand."""

    def __init__(self) -> None:
        self.asked: list[tuple[str, Any, str, dict[str, Any]]] = []

    def enqueue(self, key: str, spec: Any, pages: Any = None, processor: str = "local",
                **kwargs: Any) -> dict[str, Any]:
        self.asked.append((key, spec, processor, kwargs))
        return {}

    def settle(self, state: str, n: int = -1) -> None:
        self.asked[n][3]["on_done"](state, None)


def _worker(
    storage: Path,
    rows: list[GenerationSpec] | None = None,
    *,
    remote: ProcessorRegistry | None = None,
    local_processing: bool = True,
    sessions: bool = True,
    autobench: bool = True,
    logs: list[str] | None = None,
) -> OCRWorker:
    (storage / "inbox").mkdir(parents=True, exist_ok=True)
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=rows or _rows(),
        engines_python_path=Path(sys.executable),
        sessions=sessions,
        remote=remote,
        local_processing=local_processing,
        autobench=autobench,
        status_callback=(logs.append if logs is not None else None),
    )
    worker.bench_service = _Bench()
    return worker


def _profile_pools(storage: Path, row: GenerationSpec, pools: dict[str, Any]) -> None:
    store = ProcessorProfiles(storage)
    store.set_bench(LOCAL_PROFILE, row.id, {"precision": "fp32", "pages_per_second": 7.5, "startup_seconds": 4.0},
                    recipe=row.output_affecting())
    store.set_pools(LOCAL_PROFILE, row.id, pools, recipe=row.output_affecting())


def _flag(command: list[str], flag: str) -> str | None:
    return command[command.index(flag) + 1] if flag in command else None


# --- the gate ----------------------------------------------------------------------


class TestAnUnconfiguredRow:
    def test_is_benchmarked_on_this_server_first_and_the_claim_waits(
        self, tmp_path: Path
    ) -> None:
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path)
        bench: _Bench = worker.bench_service
        row = worker.generations[1]
        slot = worker._slots[0]
        assert worker.autobench_needed(None, row) is True
        assert worker.claim_next(slot) is None, "not before it is measured here"
        assert bench.asked == [], "recorded under the lock, never enqueued there"
        worker._drain_autobench_requests()
        assert [(key, spec, processor) for key, spec, processor, _ in bench.asked] == [
            (row.id, None, "local")
        ]
        assert bench.asked[0][3]["autobench"] is True
        with worker._lock:
            assert worker._autobench_pending(), "the slot waits for it, not the next scan"
        assert worker.claim_next(slot) is None, "not while the benchmark runs"
        worker._drain_autobench_requests()
        assert len(bench.asked) == 1, "asked once per pair"
        ProcessorProfiles(tmp_path).set_bench(
            LOCAL_PROFILE, row.id, {"precision": "fp32", "pages_per_second": 5.0}, recipe=row.output_affecting()
        )
        bench.settle("done")
        assert worker.autobench_needed(None, row) is False
        job = worker.claim_next(slot)
        assert job is not None and job[1] == row.id

    def test_a_failed_benchmark_runs_it_untuned_and_says_so(self, tmp_path: Path) -> None:
        _library(tmp_path, "Volume 1")
        logs: list[str] = []
        worker = _worker(tmp_path, logs=logs)
        bench: _Bench = worker.bench_service
        row = worker.generations[1]
        slot = worker._slots[0]
        assert worker.claim_next(slot) is None
        worker._drain_autobench_requests()
        bench.settle("failed")
        assert worker.autobench_needed(None, row) is False, "never stuck"
        assert any("untuned on this server" in line for line in logs), logs
        assert not any("disconnected" in line for line in logs), (
            "this server never 'left': it is not asked again until a restart"
        )
        job = worker.claim_next(slot)
        assert job is not None and job[1] == row.id
        assert worker._local_run_row(row) is row, "untuned: the row's own (empty) table"

    def test_a_refused_benchmark_runs_it_untuned(self, tmp_path: Path) -> None:
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path)

        class _Refusing:
            @staticmethod
            def enqueue(*args: Any, **kwargs: Any) -> dict[str, Any]:
                raise RuntimeError("no volumes")

        worker.bench_service = _Refusing()
        slot = worker._slots[0]
        assert worker.claim_next(slot) is None
        worker._drain_autobench_requests()
        assert worker.claim_next(slot) is not None

    def test_a_row_whose_recipe_changed_is_measured_again(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        ProcessorProfiles(tmp_path).set_bench(
            LOCAL_PROFILE, row.id, {"precision": "fp32", "pages_per_second": 2.0}, recipe=("hayai-nova", "ppocr-manga", 512)
        )
        assert worker.autobench_needed(None, row) is True

    def test_a_mode_change_mid_scan_is_benchmarked_again_not_skipped(
        self, tmp_path: Path
    ) -> None:
        """Seen live: generation 2 benchmarked (done) earlier in a scan, then
        switched to auto-speed. Its benchmark went stale, but "already asked
        this scan" stopped the re-ask, so every machine skipped generation 2
        and ran generation 3 until the scan ended."""
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path)
        bench: _Bench = worker.bench_service
        row = worker.generations[1]
        slot = worker._slots[0]
        assert worker.claim_next(slot) is None
        worker._drain_autobench_requests()
        ProcessorProfiles(tmp_path).set_bench(
            LOCAL_PROFILE, row.id,
            {"precision": "bf16", "precision_mode": "auto-accuracy", "pages_per_second": 5.0},
            recipe=row.output_affecting(),
        )
        bench.settle("done")
        assert worker.autobench_needed(None, row) is False
        # The owner switches the row's mode, in the middle of the same scan.
        worker.apply_settings(_rows({**HAYAI, "id": row.id, "precision": "auto-speed"}))
        speed = worker.generations[1]
        assert speed.id == row.id and speed.precision == "auto-speed"
        assert worker.autobench_needed(None, speed) is True, "the old benchmark is stale"
        assert worker.claim_next(slot) is None, "not run before it is measured again"
        worker._drain_autobench_requests()
        assert len(bench.asked) == 2, "asked again in this scan, not skipped until the next"

    def test_a_failed_benchmark_is_tried_again_after_a_settings_change(
        self, tmp_path: Path
    ) -> None:
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path)
        bench: _Bench = worker.bench_service
        row = worker.generations[1]
        slot = worker._slots[0]
        assert worker.claim_next(slot) is None
        worker._drain_autobench_requests()
        bench.settle("failed")
        assert worker.autobench_needed(None, row) is False, "never stuck"
        worker.apply_settings(_rows({**HAYAI, "id": row.id, "precision": "auto-speed"}))
        assert worker.autobench_needed(None, worker.generations[1]) is True, (
            "a new configuration is a new chance, as a restart would be"
        )

    def test_with_autobench_off_nothing_is_ever_asked(self, tmp_path: Path) -> None:
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path, autobench=False)
        assert worker.autobench_needed(None, worker.generations[1]) is False
        assert worker.claim_next(worker._slots[0]) is not None


class TestAManuallyConfiguredRow:
    @pytest.mark.parametrize("pools", CONFIGURED, ids=lambda p: next(iter(p)))
    def test_is_never_benchmarked_and_runs_its_own_table(
        self, tmp_path: Path, pools: dict[str, Any]
    ) -> None:
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path, _rows({**HAYAI, "pools": pools}))
        row = worker.generations[1]
        assert not row.pools.is_empty()
        assert worker.autobench_needed(None, row) is False
        job = worker.claim_next(worker._slots[0])
        assert job is not None and job[1] == row.id
        worker._drain_autobench_requests()
        assert worker.bench_service.asked == []
        # Even a stored profile (measured before somebody configured it) is
        # set aside for the row's own table.
        _profile_pools(tmp_path, row, {"stage_workers": {"detect": 9}})
        assert worker._local_run_row(row) is row


# --- what the local runners are started with ----------------------------------------


class TestTheLocalRunners:
    def test_run_with_this_server_s_profile(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        processor = worker._slots[0].processor
        before = processor.session_command(row, tmp_path / "s.log")
        assert "--stage-workers" not in before and "--precision" not in before
        # A precision in this profile is an earlier autobench's (nothing else
        # writes it) and is dropped on read (test_autobench_precision_pins.py):
        # this server runs auto, the runner's policy.
        _profile_pools(tmp_path, row, {"stage_workers": {"detect": 3},
                                       "stage_device": {"detect": "cpu"},
                                       "precision": "fp32"})
        session = processor.session_command(row, tmp_path / "s.log")
        assert _flag(session, "--stage-workers") == "detect=3"
        assert _flag(session, "--stage-device") == "detect=cpu"
        assert "--precision" not in session
        one = processor._engine_runner_command(row, tmp_path / "Vol", tmp_path / "out")
        assert _flag(one, "--stage-workers") == "detect=3"
        assert "--precision" not in one

    def test_every_local_slot_runs_it(self, tmp_path: Path) -> None:
        (tmp_path / "inbox").mkdir()
        worker = OCRWorker(storage_path=tmp_path, poll_interval=30.0, generations=_rows(),
                           engines_python_path=Path(sys.executable), concurrency=2)
        row = worker.generations[1]
        _profile_pools(tmp_path, row, {"stage_workers": {"detect": 3}})
        for slot in worker._slots:
            command = slot.processor.session_command(row, tmp_path / "s.log")
            assert _flag(command, "--stage-workers") == "detect=3"

    def test_a_width_stored_as_auto_goes_out_derived(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        _profile_pools(tmp_path, row, {"stage_workers": {"detect": "auto", "engine": 2}})
        command = worker._slots[0].processor.session_command(row, tmp_path / "s.log")
        assert _flag(command, "--stage-workers") == "engine=2"

    def test_a_benchmark_measures_what_it_is_given(self, tmp_path: Path) -> None:
        """A benchmark starts from the spec it measures; the profile is what
        a benchmark FOUND, never what the next one is handed."""
        worker = _worker(tmp_path)
        row = worker.generations[1]
        _profile_pools(tmp_path, row, {"stage_device": {"detect": "cpu"}, "precision": "bf16"})
        command = worker.processor.open_bench(row, tmp_path / "sample", tmp_path / "b.log").command
        assert "--stage-device" not in command and "--precision" not in command

    def test_the_profile_is_ignored_once_its_recipe_is_stale(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        store = ProcessorProfiles(tmp_path)
        store.set_pools(LOCAL_PROFILE, row.id, {"stage_workers": {"detect": 3}},
                        recipe=("hayai-nova", "ppocr-manga", 512))
        assert worker._local_run_row(row) is row

    def test_a_pin_to_a_card_this_server_no_longer_has_is_set_aside(
        self, tmp_path: Path
    ) -> None:
        logs: list[str] = []
        worker = _worker(tmp_path, logs=logs)
        row = worker.generations[1]
        set_cached_catalog(DeviceCatalog(gpus=(), probed=True))
        _profile_pools(tmp_path, row, {"stage_device": {"engine": "gpu:1"}})
        assert worker._local_run_row(row) is row
        assert worker._local_run_row(row) is row
        assert sum("no longer" in line for line in logs) == 1, logs

    def test_a_row_configured_later_wins_at_once(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        _profile_pools(tmp_path, row, {"stage_workers": {"detect": 3}})
        command = worker._slots[0].processor.session_command(row, tmp_path / "s.log")
        assert _flag(command, "--stage-workers") == "detect=3"
        # The admin saves a table for the row (the config, not the profile).
        worker.apply_settings(_rows({**HAYAI, "pools": {"stage_workers": {"detect": 5}}}))
        configured = worker.generations[1]
        assert configured.id == row.id
        command = worker._slots[0].processor.session_command(configured, tmp_path / "s.log")
        assert _flag(command, "--stage-workers") == "detect=5"
        assert worker.autobench_needed(None, configured) is False

    def test_a_start_backoff_is_void_once_the_profile_changes(self, tmp_path: Path) -> None:
        """What this machine would RUN is what a start failure was about."""
        worker = _worker(tmp_path)
        row = worker.generations[1]
        slot = worker._slots[0]
        before = worker._slot_start_signature(slot, row)
        _profile_pools(tmp_path, row, {"stage_workers": {"detect": 3}})
        assert worker._slot_start_signature(slot, row) != before


# --- which servers do it --------------------------------------------------------------


class TestWhichServers:
    def test_a_local_only_server_does_it(self, tmp_path: Path) -> None:
        """No registry at all: the single-slot server is the case that matters."""
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path, remote=None)
        assert worker.remote is None and len(worker._slots) == 1
        assert worker.claim_next(worker._slots[0]) is None
        worker._drain_autobench_requests()
        assert [asked[2] for asked in worker.bench_service.asked] == ["local"]

    def test_with_sessions_off_too(self, tmp_path: Path) -> None:
        """A per-volume run reads the pools as a session does."""
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path, sessions=False)
        assert worker.claim_next(worker._slots[0]) is None
        worker._drain_autobench_requests()
        assert len(worker.bench_service.asked) == 1

    def test_beside_processors_too(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry(local_name="this server")
        worker = _worker(tmp_path, remote=registry)
        assert worker.autobench_needed(None, worker.generations[1]) is True

    def test_never_with_local_processing_off(self, tmp_path: Path) -> None:
        _library(tmp_path, "Volume 1")
        worker = _worker(tmp_path, remote=ProcessorRegistry(), local_processing=False)
        assert worker._slots == []
        assert worker.autobench_needed(None, worker.generations[1]) is False

    def test_never_for_a_row_read_behind_its_own_command_line(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A monolithic row -- or a served one whose package cannot serve
        here -- has no pipeline for a benchmark to tune."""
        from mokuro_bunko.ocr import engines

        monkeypatch.setitem(
            engines.ENGINES, "mokuro-cli",
            engines.EngineSpec(id="mokuro-cli", label="mokuro CLI",
                               recognizer="kha-white/manga-ocr-base", uses_mokuro_env=True),
        )
        cli = GenerationSpec(id="g-3", name="cli", engine="mokuro-cli")
        assert cli.monolithic
        worker = _worker(tmp_path)
        assert worker.autobench_needed(None, cli) is False
        row = worker.generations[1]
        monkeypatch.setattr(worker.processor, "runs_mokuro_cli", lambda generation: True)
        assert worker.autobench_needed(None, row) is False


# --- the reserved name --------------------------------------------------------------


class TestTheReservedName:
    @pytest.mark.parametrize("raw", ["local", " local", "local ", "Local", "@local", ""])
    def test_no_processor_name_is_stored_as_it(self, raw: str) -> None:
        assert clean_processor_name(raw, "someone") != LOCAL_PROFILE
        assert clean_processor_name(raw, " local") != LOCAL_PROFILE

    def test_its_file_is_no_processor_s(self) -> None:
        mine = profile_filename(LOCAL_PROFILE)
        for name in ("local", "@local", "Local", "local~x", "_local", "LOCAL"):
            assert profile_filename(name) != mine

    def test_a_processor_called_local_keeps_its_own_profile(self, tmp_path: Path) -> None:
        store = ProcessorProfiles(tmp_path)
        store.set_bench("local", "g-2", {"pages_per_second": 1.0})
        store.set_bench(LOCAL_PROFILE, "g-2", {"pages_per_second": 9.0})
        assert store.row("local", "g-2").bench == {"pages_per_second": 1.0}  # type: ignore[union-attr]
        assert store.row(LOCAL_PROFILE, "g-2").bench == {"pages_per_second": 9.0}  # type: ignore[union-attr]

    def test_it_is_not_listed_as_a_processor_but_is_pruned(self, tmp_path: Path) -> None:
        store = ProcessorProfiles(tmp_path)
        store.set_bench("tower", "g-2", {"pages_per_second": 1.0})
        store.set_bench(LOCAL_PROFILE, "g-2", {"pages_per_second": 9.0})
        store.set_bench(LOCAL_PROFILE, "gone", {"pages_per_second": 9.0})
        assert store.names() == ["tower"]
        store.prune(["g-2"])
        assert store.row(LOCAL_PROFILE, "gone") is None
        assert store.row(LOCAL_PROFILE, "g-2") is not None


# --- the result lands in this server's profile -----------------------------------------


def _local_run(storage: Path, rows: list[GenerationSpec], *, autobench: bool,
               best: dict[str, Any]) -> tuple[Any, Any]:
    service = BenchService(storage, worker=lambda: None, generations=lambda: rows,
                           profiles=ProcessorProfiles(storage))
    row = rows[1]
    run = _BenchRun(row.id, row, 8, draft=False, spec=_spec_payload(row),
                    processor="local", autobench=autobench)
    # What the recognizer ran at, as a runner since the precision policy says.
    run.update(best=best, host={"cpu": "this box"}, startup_seconds=6.0, precision="fp32")
    return service, run


class TestWhereTheResultLands:
    def test_an_autobench_stores_only_what_differs(self, tmp_path: Path) -> None:
        rows = _rows()
        row = rows[1]
        service, run = _local_run(tmp_path, rows, autobench=True, best={
            "pages_per_second": 12.0, "stage_workers": {"detect": 3}, "queue_capacity": {},
            "stage_device": {"detect": "cpu"}, "precision": "fp16",
        })
        service._finish(run, "done")
        found = ProcessorProfiles(tmp_path).row(LOCAL_PROFILE, row.id,
                                                recipe=row.output_affecting())
        assert found is not None and found.bench is not None
        assert found.bench["pages_per_second"] == 12.0
        assert found.bench["startup_seconds"] == 6.0
        # No precision: a benchmark never chooses one (auto is the runner's
        # policy), so none is stored, whatever ``best`` carried.
        assert found.pools == {"stage_workers": {"detect": 3}, "queue_capacity": {},
                               "stage_device": {"detect": "cpu"}}
        saved = service.saved(row.id)
        assert saved is not None and saved["best"]["pages_per_second"] == 12.0, (
            "still this server's benchmark of the row, where it always was"
        )

    def test_an_autobench_that_changed_nothing_stores_no_pools(self, tmp_path: Path) -> None:
        rows = _rows()
        row = rows[1]
        service, run = _local_run(tmp_path, rows, autobench=True, best={
            "pages_per_second": 12.0, "stage_workers": {}, "queue_capacity": {},
            "stage_device": {}, "precision": "auto", "same_as_spec": True,
        })
        service._finish(run, "done")
        found = ProcessorProfiles(tmp_path).row(LOCAL_PROFILE, row.id,
                                                recipe=row.output_affecting())
        assert found is not None and found.bench is not None, "measured: not asked again"
        assert found.pools == {}, "derived defaults, still out of the profile's hands"

    def test_a_benchmark_somebody_started_applies_nothing(self, tmp_path: Path) -> None:
        rows = _rows()
        service, run = _local_run(tmp_path, rows, autobench=False, best={
            "pages_per_second": 12.0, "stage_workers": {"detect": 3},
        })
        service._finish(run, "done")
        assert ProcessorProfiles(tmp_path).row(LOCAL_PROFILE, rows[1].id) is None
        assert service.saved(rows[1].id) is not None

    def test_the_config_is_never_written(self, tmp_path: Path) -> None:
        rows = _rows()
        config = tmp_path / "config.yaml"
        config.write_text("ocr: {}\n", encoding="utf-8")
        service, run = _local_run(tmp_path, rows, autobench=True, best={
            "pages_per_second": 12.0, "stage_workers": {"detect": 3},
        })
        service._finish(run, "done")
        assert config.read_text(encoding="utf-8") == "ocr: {}\n"
        assert rows[1].pools.is_empty()


# --- what the queue and the admin panel read ------------------------------------------


class TestTheBenchmarkFigure:
    def test_the_queue_prices_this_server_off_its_profile(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        assert worker._machine_bench(row.id, LOCAL_SLOT, {}) is None
        _profile_pools(tmp_path, row, {"stage_workers": {"detect": 3}})
        assert worker._machine_bench(row.id, LOCAL_SLOT, {}) == {
            "precision": "fp32", "pages_per_second": 7.5, "startup_seconds": 4.0
        }
        rate_for, startup_for = worker._lane_pricing()
        rate = rate_for(row.id, LOCAL_SLOT)
        assert rate is not None and rate.pages_per_second == pytest.approx(7.5)
        assert rate.source == "bench"
        assert startup_for(row.id, LOCAL_SLOT).seconds == pytest.approx(4.0)

    def test_a_saved_benchmark_of_the_row_still_wins(self, tmp_path: Path) -> None:
        """`.ocr-bench.json` is this server's too, and a person's later
        benchmark lands there: it is not overruled by an older autobench."""
        worker = _worker(tmp_path)
        row = worker.generations[1]
        _profile_pools(tmp_path, row, {"stage_workers": {"detect": 3}})
        (tmp_path / ".ocr-bench.json").write_text(
            json.dumps({row.id: {"best": {"pages_per_second": 3.0}, "startup_seconds": 9.0}}),
            encoding="utf-8",
        )
        rate_for, startup_for = worker._lane_pricing()
        rate = rate_for(row.id, LOCAL_SLOT)
        assert rate is not None and rate.pages_per_second == pytest.approx(3.0)
        assert startup_for(row.id, LOCAL_SLOT).seconds == pytest.approx(9.0)

    def test_the_queue_card_says_this_server_is_being_configured(
        self, tmp_path: Path
    ) -> None:
        worker = _worker(tmp_path)
        worker.bench_service = type("Bench", (), {
            "configuring": lambda self: {
                "local": {"key": "g-2", "generation": "hayai-ctd", "auto": True}
            },
        })()
        rows = {row["machine"]: row for row in worker.connected_machines()}
        assert rows[LOCAL_SLOT]["configuring"] == {
            "key": "g-2", "generation": "hayai-ctd", "auto": True
        }


def _admin(tmp_path: Path) -> tuple[Any, Any, Callable[..., tuple[int, Any]]]:
    storage = tmp_path / "storage"
    for name in ("library", "inbox", "users"):
        (storage / name).mkdir(parents=True)
    config = Config()
    config.storage.base_path = storage
    config.ocr.generations = _rows()
    app = AdminAPI(
        _nothing, Database(storage / "mokuro.db"), AdminConfig(enabled=True, path="/_admin"),
        full_config=config, config_path=tmp_path / "config.yaml",
        ocr_control=_Control(ProcessorRegistry(local_name="this server")),  # type: ignore[arg-type]
    )
    return app, config, _request


class TestTheAdminPanel:
    def test_the_processors_card_shows_this_server_s_benchmark(self, tmp_path: Path) -> None:
        app, config, request = _admin(tmp_path)
        row = config.ocr.generations[1]
        _profile_pools(config.storage.base_path, row, {"stage_workers": {"detect": 3}})
        status, body = request(app, "/_admin/api/processors")
        assert status == 200, body
        names = [machine["name"] for machine in body["speed"]]
        assert names == ["local"], "this server's profile is not a processor's"
        (local,) = body["speed"]
        (layer,) = [lay for lay in local["layers"] if lay["generation_id"] == row.id]
        assert layer["bench_pages_per_minute"] == pytest.approx(450.0)

    def test_the_generations_payload_carries_this_server_s_profile(
        self, tmp_path: Path
    ) -> None:
        app, config, request = _admin(tmp_path)
        row = config.ocr.generations[1]
        _profile_pools(config.storage.base_path, row, {"stage_workers": {"detect": 3}})
        status, body = request(app, "/_admin/api/ocr/generations")
        assert status == 200, body
        entry = next(g for g in body["generations"] if g["id"] == row.id)
        assert entry["local_pools"] == {"stage_workers": {"detect": 3}}
        assert entry["local_bench"]["pages_per_second"] == 7.5
        assert entry["configured"] is False
        primary = next(g for g in body["generations"] if g["id"] != row.id)
        assert primary.get("local_pools") is None


# --- end to end: the real bench service, the fake runner ------------------------------


class TestEndToEnd:
    def test_a_local_only_server_measures_applies_and_then_runs_measured(
        self, tmp_path: Path
    ) -> None:
        """Claim waits -> the bench line holds this server -> the fake runner's
        ``best`` (a width) lands in this server's profile -> the slot is woken,
        claims, and its runner is started with it. The precision the runner
        ran at is recorded, never applied: ``auto`` is the runner's policy."""
        _library(tmp_path, "Volume 1", "Volume 2")
        rows = _rows()
        row = rows[1]
        (tmp_path / "inbox").mkdir(exist_ok=True)
        best = {"best": {"trial": 1, "stage_workers": {"detect": 3}, "queue_capacity": {},
                         "stage_device": {}, "pages_per_second": 6.0, "seconds_per_page": 0.17,
                         "speedup": 1.4}, "precision": "bf16"}
        worker = bench_worker(tmp_path, rows, _script(tmp_path, bench=best))
        assert worker.remote is None and len(worker._slots) == 1
        service = BenchService(tmp_path, worker=lambda: worker, generations=lambda: rows,
                               profiles=ProcessorProfiles(tmp_path))
        worker.bench_service = service
        slot = worker._slots[0]
        assert worker.claim_next(slot) is None
        worker._drain_autobench_requests()
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            with worker._lock:
                if not worker._autobench_pending():
                    break
            time.sleep(0.05)
        else:
            raise AssertionError(f"the autobench never settled: {service.get(row.id)}")
        assert service.get(row.id)["state"] == "done", service.get(row.id)
        found = ProcessorProfiles(tmp_path).row(LOCAL_PROFILE, row.id,
                                                recipe=row.output_affecting())
        assert found is not None and found.bench is not None
        assert found.pools["stage_workers"] == {"detect": 3}
        assert "precision" not in found.pools
        assert found.bench["precision"] == "bf16"
        assert rows[1].pools.is_empty(), "the config's row is untouched"
        job = worker.claim_next(slot)
        assert job is not None and job[1] == row.id
        command = slot.processor.session_command(row, tmp_path / "s.log")
        assert _flag(command, "--stage-workers") == "detect=3"
        assert "--precision" not in command
        assert worker.autobench_needed(None, row) is False
