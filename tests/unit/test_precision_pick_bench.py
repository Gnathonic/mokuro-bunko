"""Hand-set pools switch width tuning off -- never the precision pick.

A balanced/speed row on a machine that supports more than one of its
candidates needs that machine's PICK. Where a person set the pools (this
server's own table, or an admin's pools saved for a processor), the machine
is never width-tuned, but it still gets a PRECISION-ONLY benchmark: the
candidate trials at its pools exactly as configured, storing the pick, the
trials and the why, and never writing a pool. Automatic benchmarks off, or a
benchmark that failed: the first supported candidate, and the card says so.
"""

from __future__ import annotations

import sys
from dataclasses import replace
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner
from mokuro_bunko.ocr.devices import DeviceCatalog, GpuDevice, set_cached_catalog
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.remote.profiles import LOCAL_PROFILE, ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.watcher import LOCAL_SLOT, OCRWorker
from tests.unit.test_precision_modes import FAST, PRIMARY, _catalog, _connect

PADDLE: dict[str, Any] = {"name": "paddle", "engine": "paddle-manga", "detector": "ctd"}
TRIALS = [{"precision": "bf16", "pages_per_second": 3.1, "chosen": True},
          {"precision": "fp32", "pages_per_second": 1.5, "chosen": False}]


@pytest.fixture(autouse=True)
def _bf16_card_here() -> Any:
    set_cached_catalog(DeviceCatalog(
        gpus=(GpuDevice(0, "card", formats=frozenset({"bf16", "fp16"})),), probed=True,
    ))
    yield
    set_cached_catalog(None)


class _Bench:
    """Records what the worker enqueues; never runs anything."""

    def __init__(self) -> None:
        self.asked: list[dict[str, Any]] = []

    def enqueue(self, key: str, spec: Any, pages: Any = None, **kw: Any) -> dict[str, Any]:
        self.asked.append({"key": key, "spec": spec, **kw})
        return {}


def _worker(tmp_path: Path, row: dict[str, Any], *, registry: ProcessorRegistry | None = None,
            local: bool = True, autobench: bool = True) -> OCRWorker:
    (tmp_path / "library").mkdir(exist_ok=True)
    worker = OCRWorker(
        storage_path=tmp_path, poll_interval=30.0,
        generations=parse_generation_list([dict(PRIMARY), dict(row)], devices=DeviceCatalog()),
        engines_python_path=Path(sys.executable), concurrency=1, sessions=True,
        remote=registry, local_processing=local, autobench=autobench,
    )
    worker.bench_service = _Bench()
    worker.processor.runs_mokuro_cli = lambda _row: False  # type: ignore[method-assign]
    return worker


def _drain(worker: OCRWorker, entry: Any, row: Any) -> list[dict[str, Any]]:
    worker._want_autobench(entry, row)
    worker._drain_autobench_requests()
    return worker.bench_service.asked  # type: ignore[no-any-return]


def _pick(storage: Path, name: str, row: Any, mode: str = "auto-balanced") -> None:
    trials = TRIALS if mode == "auto-balanced" else [
        *TRIALS, {"precision": "fp16", "pages_per_second": 2.9, "chosen": False},
    ]
    ProcessorProfiles(storage).set_bench(
        name, row.id,
        {"precision": "bf16", "precision_mode": mode, "pages_per_second": 3.1,
         "precision_trials": trials, "precision_why": "benchmark: bf16 3.10 p/s beat fp32 1.50 p/s"},
        recipe=row.output_affecting(),
    )


HAND = {"stage_workers": {"detect": 2}, "stage_device": {"engine": "gpu:0"}}


class TestThisServersHandSetRow:
    def test_balanced_gets_a_precision_only_bench_at_its_own_pools(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path, {**PADDLE, "precision": "auto-balanced", "pools": HAND})
        row = worker.generations[1]
        assert worker.autobench_kind(None, row) == "precision"
        (asked,) = _drain(worker, None, row)
        assert asked["precision_only"] is True and asked["autobench"] is True
        assert asked["spec"]["pools"]["stage_workers"] == {"detect": 2}
        assert asked["spec"]["precision"] == "auto-balanced"
        assert "precision_pick" not in asked["spec"]
        # Once the pick is in, nothing more is asked -- and the pools are
        # still the config's, with nothing stored for this machine.
        _pick(tmp_path, LOCAL_PROFILE, row)
        assert worker.autobench_kind(None, row) is None
        assert worker._local_pools(row) is None
        stored = ProcessorProfiles(tmp_path).load(LOCAL_PROFILE)["rows"][row.id]
        assert "pools" not in stored
        assert worker._local_run_row(row).precision_pick == "bf16"
        assert worker._local_run_row(row).pools == row.pools

    def test_accuracy_needs_no_benchmark_for_precision(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path, {**PADDLE, "pools": HAND})
        assert worker.autobench_kind(None, worker.generations[1]) is None

    def test_a_mode_change_makes_the_pick_stale_and_trials_run_again(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path, {**PADDLE, "precision": "auto-balanced", "pools": HAND})
        row = worker.generations[1]
        _pick(tmp_path, LOCAL_PROFILE, row)
        assert worker.autobench_kind(None, row) is None
        speed = replace(row, precision="auto-speed")
        worker.generations[1] = speed
        assert worker.autobench_kind(None, speed) == "precision"
        assert worker.precision_bench_state(LOCAL_SLOT, speed) == "pending"

    def test_autobench_off_runs_no_trials_and_the_first_candidate(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path, {**PADDLE, "precision": "auto-balanced", "pools": HAND},
                         autobench=False)
        row = worker.generations[1]
        assert worker.autobench_kind(None, row) is None
        assert worker.precision_bench_state(LOCAL_SLOT, row) == "off"
        run = worker._local_run_row(row)
        assert run.precision_pick is None
        command = worker._slots[0].processor.session_command(run, tmp_path / "s.log")
        assert "--precision-pick" not in command  # the runner takes bf16, the first

    def test_a_failed_benchmark_is_said(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path, {**PADDLE, "precision": "auto-balanced", "pools": HAND})
        row = worker.generations[1]
        worker._autobench_failed.add((LOCAL_PROFILE, row.id))
        assert worker.autobench_kind(None, row) is None
        assert worker.precision_bench_state(LOCAL_SLOT, row) == "failed"

    def test_width_tuning_stays_off(self, tmp_path: Path) -> None:
        """No pick needed (one candidate): a hand-set row is never benchmarked."""
        set_cached_catalog(DeviceCatalog(gpus=(GpuDevice(0, "card", formats=frozenset()),),
                                         probed=True))
        worker = _worker(tmp_path, {**PADDLE, "precision": "auto-balanced", "pools": HAND})
        assert worker.autobench_kind(None, worker.generations[1]) is None


class TestAProcessorsSavedPools:
    def test_admin_saved_pools_get_a_precision_only_bench_at_those_pools(
        self, tmp_path: Path
    ) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry, "tower", FAST)
        worker = _worker(tmp_path, {**PADDLE, "precision": "auto-speed"},
                         registry=registry, local=False)
        row = worker.generations[1]
        store = ProcessorProfiles(tmp_path)
        store.set_pools("tower", row.id, {"stage_workers": {"detect": 5}},
                        recipe=row.output_affecting())
        assert worker.autobench_kind(entry, row) == "precision"
        (asked,) = _drain(worker, entry, row)
        assert asked["precision_only"] is True and asked["processor"] == "tower"
        assert asked["spec"]["pools"]["stage_workers"] == {"detect": 5}
        _pick(tmp_path, "tower", row, mode="auto-speed")
        assert worker.autobench_kind(entry, row) is None
        assert store.load("tower")["rows"][row.id]["pools"] == {"stage_workers": {"detect": 5}}

    def test_accuracy_asks_nothing_of_it(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry, "tower", FAST)
        worker = _worker(tmp_path, PADDLE, registry=registry, local=False)
        row = worker.generations[1]
        ProcessorProfiles(tmp_path).set_pools("tower", row.id, {"stage_workers": {"detect": 5}},
                                              recipe=row.output_affecting())
        assert worker.autobench_kind(entry, row) is None


class TestTheBenchService:
    def test_a_precision_only_result_stores_the_pick_and_never_a_pool(
        self, tmp_path: Path
    ) -> None:
        from tests.unit.test_remote_bench import TestChoosingAProcessor, _registry, _remote_run

        registry, entry = _registry()
        service = TestChoosingAProcessor._service(tmp_path, registry)
        run = _remote_run(service, entry, autobench=True)
        run.precision_only = True
        run.update(precision="bf16", precision_mode="auto-balanced",
                   precision_trials=TRIALS, precision_why="benchmark: bf16 3.10 p/s")
        service._finish(run, "done")
        raw = ProcessorProfiles(tmp_path).load("tower")["rows"]["g-2"]
        assert "pools" not in raw, "the widths it ran at are the machine's, not its finding"
        assert raw["bench"]["precision_trials"] == TRIALS
        assert raw["bench"]["precision_mode"] == "auto-balanced"

    def test_the_op_says_precision_only_and_the_spec_carries_the_mode(
        self, tmp_path: Path
    ) -> None:
        from mokuro_bunko.ocr.bench import _spec_payload
        from mokuro_bunko.ocr.remote.session import RemoteBench

        row = parse_generation_list([dict(PRIMARY), {**PADDLE, "precision": "auto-speed"}])[1]
        assert _spec_payload(row)["precision"] == "auto-speed"
        registry = ProcessorRegistry()
        entry = registry.register(username="b", name="b", host={}, catalog=_catalog(FAST),
                                  max_sessions=1)
        sent: list[dict[str, Any]] = []
        entry.send = lambda op: sent.append(op) or True  # type: ignore[method-assign]
        RemoteBench(entry, bid="bench-g-2", spec=_spec_payload(row), sample_url="/s", pages=4,
                    precision_only=True).start()
        assert sent[0]["precision_only"] is True
        assert sent[0]["spec"]["precision"] == "auto-speed"

    def test_the_runner_is_asked_for_the_trials_at_the_pools_as_set(
        self, tmp_path: Path
    ) -> None:
        from mokuro_bunko.ocr.processor import OCRProcessor

        row = parse_generation_list([dict(PRIMARY), {**PADDLE, "precision": "auto-balanced",
                                                      "pools": HAND}])[1]
        proc = OCRProcessor(storage_path=tmp_path, generations=[row],
                            engines_python_path=Path(sys.executable))
        whole = proc.open_bench(row, tmp_path / "s", tmp_path / "b.log").command
        assert "--bench-precision-only" not in whole and "--stage-workers" not in whole
        only = proc.open_bench(row, tmp_path / "s", tmp_path / "b.log",
                               precision_only=True).command
        assert "--bench-precision-only" in only
        assert only[only.index("--stage-workers") + 1] == "detect=2"


class TestTheRunner:
    def test_precision_only_runs_the_trials_and_no_width_search(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from tests.unit.test_recognizer_precision import _bench_run, _Pipe, _Target

        pipe = _Pipe("fp32", per_page=0.02)
        pipe.host_budget = 8  # widths it COULD search
        pipe.engine = "paddle-manga"  # type: ignore[attr-defined]
        target = _Target(pipe, frozenset({"fp32", "fp16", "bf16"}), {"bf16": 0.01, "fp32": 0.02})
        pipe.precision_target = lambda: target  # type: ignore[attr-defined]
        monkeypatch.setattr(runner.BenchRun, "_ceiling", lambda self, index: 8)
        original = runner.BenchRun.__init__

        def init(self: Any, args: Any, protocol: Any) -> None:
            args.bench_precision_only = True
            original(self, args, protocol)

        monkeypatch.setattr(runner.BenchRun, "__init__", init)
        events, _configs = _bench_run(monkeypatch, pipe, precision="auto-balanced",
                                      engine="paddle-manga")
        trials = [e for e in events if e["event"] == "bench_trial"]
        assert [t["precision"] for t in trials] == ["bf16", "fp32"], "the candidates only"
        done = next(e for e in events if e["event"] == "bench_done")
        assert done["precision"] == "bf16" and done["best"]["stage_workers"] == {}
        assert next(e for e in events if e["event"] == "bench_ready")["tunable"] is False


def test_the_admin_card_is_told_where_each_pick_stands(tmp_path: Path) -> None:
    from mokuro_bunko.ocr.precision import precision_on

    row = parse_generation_list([dict(PRIMARY), {**PADDLE, "precision": "auto-balanced"}])[1]
    card = DeviceCatalog(gpus=(GpuDevice(0, "card", formats=frozenset({"bf16", "fp16"})),),
                         probed=True)
    bench = {"precision": "bf16", "precision_mode": "auto-balanced", "precision_trials": TRIALS}
    on = precision_on(
        row,
        {"local": (card, None, bench), "tower": (card, None, None), "box": (card, None, None)},
        unpicked=lambda machine: {"tower": "failed", "box": "off"}.get(machine, "pending"),
    )
    assert on["local"]["auto-balanced"]["bench"] == "done"
    assert on["local"]["auto-balanced"]["precision"] == "bf16"
    assert on["tower"]["auto-balanced"]["bench"] == "failed"
    assert on["box"]["auto-balanced"]["bench"] == "off"
    assert on["box"]["auto-balanced"]["precision"] == "bf16", "the first supported candidate"
    assert "bench" not in on["local"]["auto-accuracy"]
    assert on["local"]["auto-speed"]["bench"] == "pending", "another mode: no pick for it yet"
