"""Each machine is its own: its numbers, its strikes, its absence, its shutdown.

The library's OCR worker was written when every slot shared one machine's
environment. With processors attached, what one machine does must never be
charged to another: a 4090's pages a second are not this box's, a processor
with a broken CUDA install must not stop a row everywhere, a processor that
silently vanished is not a runner that crashed, and a volume still
streaming when the library shuts down did nothing wrong.
"""

from __future__ import annotations

import json
import queue
import sys
import threading
import time
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from mokuro_bunko.ocr.devices import DeviceCatalog
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.watcher import OCRWorker, _SessionClock, _SessionJob

PRIMARY: dict[str, Any] = {"name": "mokuro", "engine": "mokuro", "primary": True}
HAYAI: dict[str, Any] = {"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"}
FULL: dict[str, Any] = {
    "engines": ["mokuro", "hayai-nova"], "detectors": ["ctd"],
    "devices": [], "serves_mokuro": True,
}


def _gens() -> list[GenerationSpec]:
    return parse_generation_list([dict(PRIMARY), dict(HAYAI)], devices=DeviceCatalog())


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    return tmp_path


def _library(storage: Path, *volumes: str) -> None:
    """Volumes of one series with their primary layer: only hayai is pending."""
    for volume in volumes:
        cbz = storage / "library" / "Alpha" / f"{volume}.cbz"
        cbz.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(cbz, "w") as zf:
            for n in range(2):
                zf.writestr(f"page_{n:03d}.jpg", b"fake image data")
        cbz.with_suffix(".mokuro").write_text(
            json.dumps({"version": "0.0", "volume_uuid": f"u-{volume}",
                        "pages": [], "chars": 0}),
            encoding="utf-8",
        )


def _worker(storage: Path, registry: ProcessorRegistry, *, local: bool = False,
            log: list[str] | None = None) -> OCRWorker:
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        status_callback=(log.append if log is not None else None),
        generations=_gens(),
        engines_python_path=Path(sys.executable),
        concurrency=1,
        sessions=True,
        remote=registry,
        local_processing=local,
        autobench=False,
    )
    registry.on_drop = worker.processor_disconnected
    return worker


def _connect(registry: ProcessorRegistry, name: str = "tower") -> Any:
    entry = registry.register(username=name, name=name, host={"gpu": "RTX 4090"},
                              catalog=FULL, max_sessions=1)
    entry.stream_open = True
    return entry


def _wait(predicate: Callable[[], bool], timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.02)
    return predicate()


class _FakeProcessor(threading.Thread):
    """Reads one processor's op queue and answers each `open_session` its way."""

    def __init__(self, entry: Any, on_open: Callable[[Any, dict[str, Any]], None]) -> None:
        super().__init__(daemon=True)
        self.entry = entry
        self.on_open = on_open
        self.ops: list[dict[str, Any]] = []
        self.halt = threading.Event()

    def run(self) -> None:
        while not self.halt.is_set():
            try:
                op = self.entry.ops.get(timeout=0.05)
            except queue.Empty:
                continue
            if op is None:
                return
            self.ops.append(op)
            if op.get("op") == "open_session":
                with self.entry.lock:
                    session = self.entry.sessions.get(op["sid"])
                if session is not None:
                    self.on_open(session, op)


STATS = {
    "elapsed_seconds": 2.0, "items": 10, "bottleneck": "detect",
    "stages": [
        {"key": "detect", "name": "detect", "device": "cpu", "workers": 1, "items": 10,
         "busy_seconds": 1.9, "blocked_seconds": 0.0, "starved_seconds": 0.0},
        {"key": "engine", "name": "engine", "device": "gpu", "workers": 1, "items": 10,
         "busy_seconds": 0.8, "blocked_seconds": 0.0, "starved_seconds": 1.0},
    ],
    "queues": [{"name": "detect->engine", "capacity": 1, "mean_depth": 0.1, "max_depth": 1}],
}


def _in_flight(worker: OCRWorker, slot: Any) -> tuple[_SessionJob, dict[str, _SessionJob],
                                                          list[str]]:
    job = worker.claim_next(slot)
    assert job is not None
    row = worker._generation(job[1])
    assert row is not None
    volume = slot.processor.prepare_session_volume(job[0], row, "v1")
    volume.output.write_text(
        json.dumps({"version": "0.0", "pages": [], "chars": 0}), encoding="utf-8"
    )
    entry = _SessionJob(job=job, generation=row, volume=volume, slot=slot.index,
                        owner=slot, hardware=worker._slot_hardware(slot))
    return entry, {"v1": entry}, ["v1"]


class TestEachMachinesNumbers:
    """C1: a processor's ready/volume_done/stats numbers are ITS own."""

    def test_a_processors_numbers_never_move_this_servers_rate(self, storage: Path) -> None:
        _library(storage, "Volume 1")
        registry = ProcessorRegistry(local_name="this server")
        _connect(registry)
        worker = _worker(storage, registry, local=True)
        slot = next(s for s in worker._all_slots() if s.processor_id != "local")
        entry, inflight, order = _in_flight(worker, slot)
        row = entry.generation
        clock = _SessionClock()
        worker._handle_session_event(
            {"event": "ready", "startup_seconds": 5.0}, row, inflight, order, clock,
            hardware="tower",
        )
        done = worker._handle_session_event(
            {"event": "volume_done", "id": "v1", "pages": 10, "seconds": 2.0,
             "stats": STATS},
            row, inflight, order, clock, hardware="tower",
        )
        assert done == 1
        assert row.id not in worker.rates._session, "this box's rate is untouched"
        assert row.id not in worker.rates._startup
        assert f"{row.id}@tower" in worker.rates._session
        assert worker.rates._startup[f"{row.id}@tower"] == 5.0
        assert worker._congestion.load() == {}, "this box's congestion history too"
        profile = ProcessorProfiles(storage).row("tower", row.id,
                                                recipe=row.output_affecting())
        assert profile is not None
        assert profile.runs["volumes"] == 1
        assert profile.runs["pages_per_second"] == pytest.approx(5.0)
        assert len(profile.runs["congestion"]) == 1, "its pipeline numbers went here"

    def test_this_servers_own_numbers_still_go_where_they_always_went(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1")
        registry = ProcessorRegistry(local_name="this server")
        worker = _worker(storage, registry, local=True)
        slot = worker._all_slots()[0]
        entry, inflight, order = _in_flight(worker, slot)
        row = entry.generation
        worker._handle_session_event(
            {"event": "volume_done", "id": "v1", "pages": 10, "seconds": 2.0,
             "stats": STATS},
            row, inflight, order, _SessionClock(),
        )
        assert row.id in worker.rates._session
        assert row.id in worker._congestion.load()
        assert ProcessorProfiles(storage).names() == []


class TestEachLaneIsPricedByItsOwnMachine:
    """N1: a processor's evidence is filed under ``<row>@<name>``, so the
    queue has to price that processor's lanes with it -- or a server that
    runs no OCR of its own never shows a single ETA."""

    NOW = 1_800_000_000.0

    def test_with_no_local_ocr_one_remote_volume_prices_the_whole_queue(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2", "Volume 3", "Volume 4")
        registry = ProcessorRegistry(local_name="this server")
        _connect(registry)
        worker = _worker(storage, registry, local=False)
        worker.page_count_lookup = lambda path: 20
        slot = next(s for s in worker._all_slots() if s.processor_id != "local")
        entry, inflight, order = _in_flight(worker, slot)
        row = entry.generation
        clock = _SessionClock()
        worker._handle_session_event(
            {"event": "ready", "startup_seconds": 5.0}, row, inflight, order, clock,
            hardware="tower",
        )
        worker._handle_session_event(
            {"event": "volume_done", "id": "v1", "pages": 10, "seconds": 2.0},
            row, inflight, order, clock, hardware="tower",
        )
        pending = worker.pending_jobs(max_age=0)
        assert len(pending) == 3
        plan = worker.queue_plan([], pending, now=self.NOW)
        assert [item["reason"] for item in plan.pending] == [None, None, None]
        assert all(item["eta_at"] is not None for item in plan.pending)
        # tower's own 5 s startup, then 20 pages at tower's 5 pages/s each.
        assert [item["eta_seconds"] for item in plan.pending] == [9, 13, 17]
        assert plan.done_at is not None

    def test_before_any_remote_volume_its_own_benchmark_prices_it(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1")
        registry = ProcessorRegistry(local_name="this server")
        _connect(registry)
        worker = _worker(storage, registry, local=False)
        worker.page_count_lookup = lambda path: 40
        row = worker.generations[1]
        ProcessorProfiles(storage).set_bench(
            "tower", row.id, {"precision": "fp32", "pages_per_second": 8.0, "startup_seconds": 3.0},
            recipe=row.output_affecting(),
        )
        plan = worker.queue_plan([], worker.pending_jobs(max_age=0), now=self.NOW)
        assert [item["eta_seconds"] for item in plan.pending] == [8]

    def test_with_local_and_remote_lanes_each_is_priced_by_its_own_machine(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2", "Volume 3")
        registry = ProcessorRegistry(local_name="this server")
        _connect(registry)
        worker = _worker(storage, registry, local=True)
        worker.page_count_lookup = lambda path: 20
        row = worker.generations[1]
        worker.rates.record_volume(row.id, 20, 20.0)  # this server: 1 page/s
        worker.rates.record_startup(row.id, 0.0)
        worker.rates.record_volume(f"{row.id}@tower", 20, 4.0)  # tower: 5 pages/s
        worker.rates.record_startup(f"{row.id}@tower", 0.0)
        running = [{
            "generation_id": row.id, "slot": 1, "machine": "tower", "status": "running",
            "done_pages": 5, "total_pages": 20, "first_page_at": self.NOW - 1.0,
        }]
        pending = worker.pending_jobs(max_age=0)
        assert len(pending) == 3
        plan = worker.queue_plan(running, pending[:2], now=self.NOW)
        assert plan.running[0]["eta_seconds"] == 3, "15 pages at tower's 5 pages/s"
        # Each to the lane that finishes it first: tower, after the volume it
        # is already reading, reads both (3 + 4, + 4 s) before this server's
        # free lane would read one at 1 page/s (20 s).
        assert [item["eta_seconds"] for item in plan.pending] == [7, 11]

    def test_the_queue_page_keeps_the_machine_the_plan_prices_by(
        self, storage: Path
    ) -> None:
        """B8: the worker stamps `machine` on each card and it reaches
        `.ocr-progress.json`, but the queue page rebuilt every running job
        from a whitelist without it -- so the REAL page gave the plan cards
        with no machine, every one took this server's lane, and tower's
        volume was priced at this server's rate."""
        from mokuro_bunko.ocr.control import OcrControl
        from mokuro_bunko.queue.api import QueueAPI

        _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry(local_name="this server")
        _connect(registry)
        worker = _worker(storage, registry, local=True)
        worker.page_count_lookup = lambda path: 20
        row = worker.generations[1]
        worker.rates.record_volume(row.id, 20, 20.0)  # this server: 1 page/s
        worker.rates.record_startup(row.id, 0.0)
        worker.rates.record_volume(f"{row.id}@tower", 20, 2.0)  # tower: 10 pages/s
        worker.rates.record_startup(f"{row.id}@tower", 0.0)
        card = {
            "series": "Alpha", "volume": "Volume 9", "generation": row.name,
            "generation_id": row.id, "slot": 1, "machine": "tower",
            "processor": "tower (RTX 4090)", "status": "running",
            "done_pages": 5, "total_pages": 20, "first_page_at": time.time() - 1.0,
        }
        (storage / ".ocr-progress.json").write_text(
            json.dumps({"active": True, "jobs": [card]}), encoding="utf-8"
        )
        control = OcrControl()
        control.worker = worker
        api = QueueAPI(
            lambda environ, start: [], storage_base_path=str(storage),
            queue_config=type("Cfg", (), {"show_in_nav": True, "public_access": True})(),
            ocr_control=control,
        )
        environ = {"REQUEST_METHOD": "GET", "PATH_INFO": "/queue/api/status",
                   "QUERY_STRING": "", "wsgi.input": None}
        b"".join(api(environ, lambda *a: None))
        body = api.raw_status()  # the model under the shaped payload
        (running,) = body["current_jobs"]
        assert running["eta_seconds"] is not None and running["eta_seconds"] <= 2, (
            "15 pages at tower's 10 pages/s, not at this server's 1"
        )
        assert running["machine"] == "tower"

    def test_a_volumes_card_says_which_machine_reads_it(self, storage: Path) -> None:
        _library(storage, "Volume 1")
        registry = ProcessorRegistry(local_name="this server")
        _connect(registry)
        worker = _worker(storage, registry, local=True)
        slot = next(s for s in worker._all_slots() if s.processor_id != "local")
        job = worker.claim_next(slot)
        assert job is not None
        row = worker._generation(job[1])
        assert row is not None
        worker.begin_ocr_job(job, row, slot=slot.index, owner=slot,
                             machine=worker._slot_hardware(slot))
        assert worker._active_progress[job]["machine"] == "tower"


class TestABrokenProcessorStopsTheRowOnItselfOnly:
    """I5: strikes are per (row, machine), and a runner that never started
    is the machine's environment, never the volume's fault."""

    @staticmethod
    def _spawn_fails(session: Any, op: dict[str, Any]) -> None:
        del op
        session.claim_events()
        session.feed({"event": "spawn_failed", "error": "libcudnn.so.9: cannot open"}, b"")
        session.feed({"event": "exit", "returncode": None}, b"")
        session.release_events()

    def test_a_spawn_failure_holds_the_row_on_that_processor_and_blames_nothing(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        tower = _connect(registry, "tower")
        log: list[str] = []
        worker = _worker(storage, registry, log=log)
        fake = _FakeProcessor(tower, self._spawn_fails)
        fake.start()
        try:
            worker._scan_ocr_once()
        finally:
            fake.halt.set()
        row = worker.generations[1]
        assert not (storage / ".ocr-failures.json").exists(), (
            "an environment that will not start is no volume's failure"
        )
        # Held on tower: after its first failed start the row waits out a
        # backoff there (F9) -- the per-scan strike never even needs a second.
        tower_slot = worker._remote_slots(tower, first_index=5)[0]
        assert worker._backed_off_rows(tower_slot, "tower", {row.id}) == {row.id}
        assert (row.id, "tower") in worker._start_backoff
        assert (row.id, "local") not in worker._start_backoff
        assert (row.id, "local") not in worker._stopped_generations
        assert any("libcudnn" in line for line in log), "the reason is said"
        assert worker._inflight_ocr == set()

        # Another machine is still offered the row in the same scan.
        box = _connect(registry, "box")
        slot = worker._remote_slots(box, first_index=9)[0]
        job = worker.claim_next(slot)
        assert job is not None and job[1] == row.id

    def test_a_spawn_failure_is_the_sessions_error_not_an_unexplained_exit(
        self, storage: Path
    ) -> None:
        """The watcher used to ignore `spawn_failed`; the record then read
        'the runner exited before its volumes were finished'."""
        _library(storage, "Volume 1")
        registry = ProcessorRegistry()
        tower = _connect(registry, "tower")
        log: list[str] = []
        worker = _worker(storage, registry, log=log)
        fake = _FakeProcessor(tower, self._spawn_fails)
        fake.start()
        try:
            worker._scan_ocr_once()
        finally:
            fake.halt.set()
        assert any("could not start: libcudnn" in line for line in log), log
        assert not any("exited before its volumes were finished" in line for line in log)


class TestAProcessorThatIsSilentlyGone:
    """I3: a vanished processor is a disconnect, never a crash."""

    def test_a_session_whose_events_body_never_opens_returns_its_claims(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        tower = _connect(registry, "tower")
        worker = _worker(storage, registry)
        fake = _FakeProcessor(tower, lambda session, op: None)  # suspended: says nothing
        fake.start()
        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        with patch("mokuro_bunko.ocr.watcher.EVENTS_OPEN_SECONDS", 0.3):
            thread.start()
            try:
                assert _wait(lambda: tower.dropped, timeout=15), "it was never let go"
                thread.join(timeout=15)
                assert not thread.is_alive()
            finally:
                worker._stop_requested = True
                fake.halt.set()
                thread.join(timeout=10)
        assert worker._inflight_ocr == set(), "its claims came back"
        assert not (storage / ".ocr-failures.json").exists(), "nothing was blamed"
        assert worker._session_strikes == {}, "and no row was struck"

    def test_a_silent_processor_past_the_wedge_is_dropped_not_blamed(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1")
        registry = ProcessorRegistry()
        tower = _connect(registry, "tower")
        worker = _worker(storage, registry)

        def ready_then_silence(session: Any, op: dict[str, Any]) -> None:
            del op
            session.claim_events()
            session.feed({"event": "ready", "startup_seconds": 1.0}, b"")
            tower.last_seen = time.time() - 3600  # and then nothing, ever

        fake = _FakeProcessor(tower, ready_then_silence)
        fake.start()
        with (
            patch("mokuro_bunko.ocr.watcher.SESSION_WEDGE_SECONDS", 0.5),
            patch("mokuro_bunko.ocr.watcher.EVENTS_SILENCE_SECONDS", 0.2),
        ):
            try:
                worker._scan_ocr_once()
            finally:
                fake.halt.set()
        assert tower.dropped
        assert not (storage / ".ocr-failures.json").exists()

    def test_a_processor_still_pinging_has_a_wedged_runner_which_is_blamed(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1")
        registry = ProcessorRegistry()
        tower = _connect(registry, "tower")
        worker = _worker(storage, registry)
        pinging = threading.Event()

        def ready_then_wedge(session: Any, op: dict[str, Any]) -> None:
            del op
            session.claim_events()
            session.feed({"event": "ready", "startup_seconds": 1.0}, b"")
            pinging.set()

            def deliver() -> None:
                # The runner TOOK the volume, then went silent: only a
                # delivered volume can be blamed for a runner's death.
                deadline = time.monotonic() + 10
                while not session.claims() and time.monotonic() < deadline:
                    time.sleep(0.01)
                for claim in session.claims():
                    session.feed({"event": "fetch", "id": claim, "state": "ready"}, b"")

            threading.Thread(target=deliver, daemon=True).start()

        def keep_pinging() -> None:
            while not fake.halt.is_set():
                if pinging.is_set():
                    tower.last_seen = time.time()
                time.sleep(0.05)

        fake = _FakeProcessor(tower, ready_then_wedge)
        fake.start()
        threading.Thread(target=keep_pinging, daemon=True).start()
        with (
            patch("mokuro_bunko.ocr.watcher.SESSION_WEDGE_SECONDS", 0.5),
            patch("mokuro_bunko.ocr.watcher.EVENTS_SILENCE_SECONDS", 0.2),
        ):
            try:
                worker._scan_ocr_once()
            finally:
                fake.halt.set()
        assert not tower.dropped
        failures = json.loads((storage / ".ocr-failures.json").read_text("utf-8"))
        assert any("stopped responding" in entry["error"] for entry in failures.values())


class TestAShutdownIsNobodysFailure:
    """I6: a volume_failed that arrives while the worker is stopping is a
    volume the close aborted, and goes back unrecorded."""

    def test_a_failure_reported_during_stop_is_released(self, storage: Path) -> None:
        _library(storage, "Volume 1")
        registry = ProcessorRegistry()
        _connect(registry, "tower")
        worker = _worker(storage, registry)
        slot = worker._all_slots()[0]
        entry, inflight, order = _in_flight(worker, slot)
        worker._stop_requested = True
        worker._handle_session_event(
            {"event": "volume_failed", "id": "v1",
             "error": "the stream ended before the volume's pages did"},
            entry.generation, inflight, order, _SessionClock(), hardware="tower",
        )
        assert worker._inflight_ocr == set()
        assert not (storage / ".ocr-failures.json").exists()

    def test_outside_a_shutdown_a_failure_is_still_recorded(self, storage: Path) -> None:
        _library(storage, "Volume 1")
        registry = ProcessorRegistry()
        _connect(registry, "tower")
        worker = _worker(storage, registry)
        slot = worker._all_slots()[0]
        entry, inflight, order = _in_flight(worker, slot)
        worker._handle_session_event(
            {"event": "volume_failed", "id": "v1", "error": "every page failed"},
            entry.generation, inflight, order, _SessionClock(), hardware="tower",
        )
        assert (storage / ".ocr-failures.json").exists()


class TestAHeldMachineComesBackInTheSameScan:
    """A machine held for a benchmark waits in the scan; it does not leave it.

    A scan adds the slots of processors it has not seen yet, never again the
    slots of one that returned. So a processor whose slot LEFT the scan when
    it was held would sit idle after its benchmark for as long as the other
    machines kept that scan busy -- hours, behind a long backlog.
    """

    def test_a_processor_resumes_as_soon_as_its_hold_is_released(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2", "Volume 3")
        registry = ProcessorRegistry()
        tower = _connect(registry, "tower")
        worker = _worker(storage, registry)
        opened: list[str] = []

        def record(session: Any, op: dict[str, Any]) -> None:
            opened.append(op["sid"])

        fake = _FakeProcessor(tower, record)
        fake.start()
        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        thread.start()
        try:
            assert _wait(lambda: len(opened) == 1), "the first session opened"
            quiet, _preempted = worker.preempt_for_bench(timeout=10.0, processor="tower")
            assert quiet is True
            time.sleep(1.5)
            assert thread.is_alive(), "the scan is still running, waiting for tower"
            assert len(opened) == 1, "nothing is opened on a held machine"
            worker.release_queue(processor="tower")
            assert _wait(lambda: len(opened) == 2, timeout=10), (
                "tower was never offered work again in this scan"
            )
        finally:
            worker._stop_requested = True
            fake.halt.set()
            registry.drop(tower.processor_id, "the test is over")
            thread.join(timeout=15)
