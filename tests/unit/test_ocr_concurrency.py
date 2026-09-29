"""`ocr.concurrency`: several OCR jobs at once.

The rules this file pins down:

* a job is claimed exactly once, however many slots reach for it;
* a volume's generations may run at the same time, on different slots (each
  sidecar is stamped with the volume's own `volume_uuid`, whichever lands
  first -- see test_parallel_generations.py);
* `concurrency: 1` is the serial worker this server has always been;
* N slots still drain the queue in the scheduler's order;
* one slot's failure is recorded against its own volume, with its own
  reason, and does not touch another slot;
* a settings change kills only the slots whose generation was removed;
* the queue API reports every running job.

Jobs are `(cbz, generation id)` pairs throughout. A volume offers every row
it lacks at once (`OCRProcessor.missing_generations`); tests that want only
the secondary rows write the primary sidecar first (`primary_done`).
"""

from __future__ import annotations

import io
import json
import threading
import time
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Any
from unittest.mock import patch

import pytest

from mokuro_bunko.config import MAX_OCR_CONCURRENCY, OcrConfig
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.processor import OcrFailure
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.queue.api import QueueAPI

# The rows these tests build their generation lists from. `_gens` mints the
# ids in list order (g-1, g-2, ...), so a list rebuilt from the same rows
# keeps the ids a running job was claimed with -- which is what tells a
# settings change apart from a removal.
PRIMARY: dict[str, Any] = {"name": "mokuro", "engine": "mokuro", "primary": True}
SECOND: dict[str, Any] = {"name": "hayai-nova", "engine": "hayai-nova"}
THIRD: dict[str, Any] = {"name": "paddle-manga", "engine": "paddle-manga"}


def _gens(*rows: dict[str, Any]) -> list[GenerationSpec]:
    """Build a generations list from row literals, through the real parser."""
    return parse_generation_list([dict(row) for row in rows])


def _make_cbz(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as zf:
        zf.writestr("page_000.jpg", b"fake image data")
    return path


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    return tmp_path


def _worker(
    storage: Path,
    generations: list[GenerationSpec],
    concurrency: int = 1,
) -> OCRWorker:
    return OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=generations,
        engines_python_path=Path("/nonexistent"),
        concurrency=concurrency,
        # The per-volume path: these tests stand in for a whole OCR run by
        # patching `process_library_ocr`, which a session never calls.
        sessions=False,
    )


def _library(storage: Path, *, primary_done: bool = False, **series: list[str]) -> None:
    """Create volumes; with `primary_done`, their `<Volume>.mokuro` too.

    A volume that already has the primary sidecar offers every secondary row
    at once, which is the only way two jobs can reach for one volume.
    """
    for name, volumes in series.items():
        for volume in volumes:
            cbz = _make_cbz(storage / "library" / name / f"{volume}.cbz")
            if primary_done:
                cbz.with_suffix(".mokuro").write_text("{}", encoding="utf-8")


def _sidecar(path: Path, generation: GenerationSpec) -> Path:
    return generation.sidecar_paths(path)[0]


def _triple(worker: OCRWorker, job: tuple[Path, str]) -> tuple[str, str, str]:
    """(series, volume, generation name) of a claimed job."""
    path, gen_id = job
    row = worker._generation(gen_id)
    assert row is not None
    return path.parent.name, path.stem, row.name


def _projected(worker: OCRWorker) -> list[tuple[str, str, str]]:
    """The queue page's list, in the triples the recorder records."""
    return [
        (job["series"], job["volume"], job["generation"]) for job in worker.pending_jobs(max_age=0)
    ]


class _Recorder:
    """A fake `process_library_ocr` shared by every slot.

    Records what ran, in order, and what was running at the same time. A
    job blocks until `release` is set, so a test can hold slots open and
    look at the overlap.
    """

    def __init__(
        self,
        worker: OCRWorker,
        *,
        hold: bool = False,
        fail: set[tuple[str, str]] | None = None,
        duration: float = 0.0,
    ) -> None:
        self.worker = worker
        self.hold = hold
        self.fail = fail or set()
        self.duration = duration
        self.ran: list[tuple[str, str, str]] = []
        self.overlaps: list[frozenset[tuple[str, str, str]]] = []
        self.started = threading.Semaphore(0)
        self.release = threading.Event()
        self._lock = threading.Lock()
        self._running: set[tuple[str, str, str]] = set()

    def install(self) -> list[Any]:
        """Patch every slot's processor; returns the patchers to stop."""
        patchers = [
            patch.object(slot.processor, "process_library_ocr", side_effect=self)
            for slot in self.worker._slots
        ]
        for patcher in patchers:
            patcher.start()
        return patchers

    def __call__(self, path: Path, generation: GenerationSpec) -> bool:
        key = (path.parent.name, path.stem, generation.name)
        with self._lock:
            self._running.add(key)
            self.ran.append(key)
            self.overlaps.append(frozenset(self._running))
        self.started.release()
        try:
            if self.hold:
                assert self.release.wait(10)
            elif self.duration:
                time.sleep(self.duration)
            if (path.stem, generation.name) in self.fail:
                return False
            _sidecar(path, generation).write_text("{}", encoding="utf-8")
            return True
        finally:
            with self._lock:
                self._running.discard(key)


def _run_scan(recorder: _Recorder) -> None:
    recorder.worker._running = True
    patchers = recorder.install()
    try:
        recorder.worker._scan_ocr_once()
    finally:
        for patcher in patchers:
            patcher.stop()


class TestConfigKnob:
    def test_default_is_one_slot(self) -> None:
        assert OcrConfig().concurrency == 1

    def test_accepts_the_whole_supported_range(self) -> None:
        for slots in range(1, MAX_OCR_CONCURRENCY + 1):
            assert OcrConfig(concurrency=slots).concurrency == slots

    def test_a_string_from_yaml_or_the_cli_is_read_as_a_number(self) -> None:
        assert OcrConfig(concurrency="4").concurrency == 4  # type: ignore[arg-type]

    @pytest.mark.parametrize("bad", [0, -1, MAX_OCR_CONCURRENCY + 1, 64, "many"])
    def test_refuses_what_it_cannot_run(self, bad: object) -> None:
        with pytest.raises(ValueError, match="concurrency"):
            OcrConfig(concurrency=bad)  # type: ignore[arg-type]

    def test_is_settable_by_dotted_key(self) -> None:
        from mokuro_bunko.config import _CONFIG_TYPES, Config, set_by_dotted_key

        assert _CONFIG_TYPES["ocr.concurrency"] is int
        config = Config()
        set_by_dotted_key(config, "ocr.concurrency", "3")
        assert config.ocr.concurrency == 3


class TestSlots:
    def test_one_slot_by_default_and_it_is_the_primary_processor(self, storage: Path) -> None:
        worker = _worker(storage, _gens(PRIMARY))
        assert len(worker._slots) == 1
        assert worker._slots[0].processor is worker.processor

    def test_each_slot_gets_its_own_processor_with_the_same_settings(self, storage: Path) -> None:
        worker = _worker(storage, _gens(PRIMARY, SECOND), concurrency=3)
        processors = [slot.processor for slot in worker._slots]
        assert len(processors) == 3
        assert len({id(p) for p in processors}) == 3
        assert processors[0] is worker.processor
        for processor in processors[1:]:
            # The whole recipe list, so no slot can run a different engine,
            # detector, character map or patch budget from another.
            assert processor.generations == worker.processor.generations
            # Resolved once, on the primary: no slot re-probes the envs.
            assert processor.python_path == worker.processor.python_path
            assert processor.engines_python_path == worker.processor.engines_python_path

    def test_cancelling_one_slot_kills_only_its_own_subprocess(self, storage: Path) -> None:
        # The three fields that made one processor per slot necessary:
        # `_active_process`, `_cancel_requested` and `last_failure`.
        import subprocess
        import sys

        worker = _worker(storage, _gens(PRIMARY), concurrency=2)
        for slot in worker._slots:
            slot.processor.python_path = Path(sys.executable)
        first, second = worker._slots
        results: dict[int, Any] = {}
        cmd = [sys.executable, "-c", "import time; time.sleep(30)"]

        def run(slot: Any) -> None:
            out = storage / f"out{slot.index}"
            out.mkdir()
            results[slot.index] = slot.processor._run_ocr_subprocess(
                cmd, storage / "in", out, storage / f"run{slot.index}.log", label="Mokuro"
            )

        threads = [threading.Thread(target=run, args=(slot,)) for slot in worker._slots]
        for thread in threads:
            thread.start()
        try:
            for _ in range(200):
                if all(slot.processor._active_process is not None for slot in worker._slots):
                    break
                time.sleep(0.05)
            assert first.processor._active_process is not second.processor._active_process
            assert isinstance(second.processor._active_process, subprocess.Popen)
            second_pid = second.processor._active_process.pid

            assert first.processor.cancel_active() is True
            threads[0].join(15)
            assert not threads[0].is_alive()
            # The other slot is untouched: still running, still not cancelled.
            assert second.processor._cancel_requested is False
            assert second.processor._active_process is not None
            assert second.processor._active_process.pid == second_pid
            assert second.processor._active_process.poll() is None
        finally:
            second.processor.cancel_active()
            for thread in threads:
                thread.join(15)
        assert results[0].ok is False and "cancelled" in (results[0].error or "")
        assert results[1].ok is False

    def test_failure_reasons_are_per_slot(self, storage: Path) -> None:
        generations = _gens(PRIMARY)
        worker = _worker(storage, generations, concurrency=2)
        first, second = worker._slots
        first.processor.last_failure = OcrFailure(error="slot 0 broke")
        second.processor.last_failure = OcrFailure(error="slot 1 broke")
        assert first.processor.last_failure.error == "slot 0 broke"
        assert second.processor.last_failure.error == "slot 1 broke"
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        worker._record_ocr_failure(cbz, generations[0], second.processor.last_failure)
        failures = json.loads((storage / ".ocr-failures.json").read_text(encoding="utf-8"))
        assert failures["S/V.cbz"]["error"] == "slot 1 broke"


class TestRunPriority:
    """Which generation keeps normal OS priority, and which is niced.

    ROW ORDER decides, and nothing else: the head of the enabled list is the
    layer readers are waiting for, every row below it is a backlog. There is
    no speed ranking to disagree with the queue any more -- the nice decision
    and the queue order come out of the same `enabled_generations`, so a job
    can no longer run at ni=10 while it is the only thing on the machine.
    """

    def _processor(self, storage: Path, generations: list[GenerationSpec]) -> Any:
        from mokuro_bunko.ocr.processor import OCRProcessor

        return OCRProcessor(storage_path=storage, generations=generations)

    def test_row_order_decides_which_row_keeps_priority(self, storage: Path) -> None:
        rows = _gens(PRIMARY, SECOND, THIRD)
        processor = self._processor(storage, rows)
        assert processor.is_backlog_generation(rows[0]) is False
        assert processor.is_backlog_generation(rows[1]) is True
        assert processor.is_backlog_generation(rows[2]) is True
        # Move the same rows around and the decision moves with them.
        reordered = _gens(THIRD | {"primary": True}, PRIMARY | {"primary": False})
        processor = self._processor(storage, reordered)
        assert processor.is_backlog_generation(reordered[0]) is False
        assert processor.is_backlog_generation(reordered[1]) is True

    def test_two_rows_of_one_engine_are_not_both_protected(self, storage: Path) -> None:
        # The decision is per ROW: keyed on the engine, a second row sharing
        # the head's engine would quietly run at normal priority too.
        rows = _gens(PRIMARY, {"name": "mokuro-again", "engine": "mokuro"})
        processor = self._processor(storage, rows)
        assert rows[0].engine == rows[1].engine
        assert processor.is_backlog_generation(rows[0]) is False
        assert processor.is_backlog_generation(rows[1]) is True

    def test_the_priority_decision_follows_the_workers_generation_order(
        self, storage: Path
    ) -> None:
        # The worker's queue order and the processor's nice decision must
        # not be able to disagree, on any slot.
        worker = _worker(storage, _gens(PRIMARY, SECOND, THIRD), concurrency=3)
        order = worker.generation_order()
        first = worker._generation(order[0]["id"])
        assert first is not None
        for slot in worker._slots:
            assert slot.processor.is_backlog_generation(first) is False
            for entry in order[1:]:
                row = worker._generation(entry["id"])
                assert row is not None
                assert slot.processor.is_backlog_generation(row) is True

    def test_jobs_of_one_generation_are_all_niced_alike(self, storage: Path) -> None:
        # Slots running the same row must not compete at different
        # priorities; the decision may depend on the row, never on the slot.
        rows = _gens(PRIMARY, SECOND, THIRD)
        worker = _worker(storage, rows, concurrency=4)
        for row in rows:
            decisions = {slot.processor.is_backlog_generation(row) for slot in worker._slots}
            assert len(decisions) == 1

    def test_nicing_reaches_popen_for_a_backlog_generation_only(self) -> None:
        from mokuro_bunko.ocr.processor import OCRProcessor

        assert OCRProcessor._priority_popen_kwargs(False) == {}
        assert OCRProcessor._priority_popen_kwargs(True) != {}


class TestClaim:
    def test_a_job_is_handed_to_exactly_one_slot(self, storage: Path) -> None:
        _library(storage, Alpha=["1"])
        generations = _gens(PRIMARY)
        worker = _worker(storage, generations, concurrency=4)
        claimed = [worker.claim_next(slot) for slot in worker._slots]
        assert [job for job in claimed if job is not None] == [
            (storage / "library" / "Alpha" / "1.cbz", generations[0].id)
        ]

    def test_slots_take_the_head_of_the_queue_in_order(self, storage: Path) -> None:
        # Three slots claiming before any of them finishes get the first
        # three of the list the queue page is showing, in that order.
        _library(storage, Alpha=["1", "2", "3"], Beta=["1", "2"], Gamma=["1"])
        worker = _worker(storage, _gens(PRIMARY), concurrency=3)
        projected = _projected(worker)
        taken = [worker.claim_next(slot) for slot in worker._slots]
        assert [_triple(worker, job) for job in taken if job is not None] == projected[:3]
        # And the page now lists exactly the rest.
        assert _projected(worker) == projected[3:]

    def test_racing_slots_never_claim_the_same_job(self, storage: Path) -> None:
        _library(storage, **{f"S{i}": ["1"] for i in range(12)})
        worker = _worker(storage, _gens(PRIMARY), concurrency=8)
        claimed: list[tuple[Path, str]] = []
        lock = threading.Lock()
        start = threading.Barrier(8)

        def claim(slot: Any) -> None:
            start.wait(10)
            for _ in range(3):
                job = worker.claim_next(slot)
                if job is None:
                    return
                with lock:
                    claimed.append(job)
                # Free the volume again so the next round can be claimed.
                with worker._lock:
                    worker._inflight_ocr.discard(job)
                    slot.job = None

        threads = [threading.Thread(target=claim, args=(slot,)) for slot in worker._slots]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(10)
        assert len(claimed) == len(set(claimed)) == 12

    def test_two_generations_of_one_volume_go_to_two_slots(self, storage: Path) -> None:
        # Two secondary rows are missing for the one volume: the second slot
        # takes the other row while the first runs, and neither is taken twice.
        _library(storage, primary_done=True, Alpha=["1"])
        worker = _worker(storage, _gens(PRIMARY, SECOND, THIRD), concurrency=2)
        first = worker.claim_next(worker._slots[0])
        second = worker.claim_next(worker._slots[1])
        assert first is not None and second is not None
        assert second[0] == first[0] and second[1] != first[1]
        assert worker.claim_next(worker._slots[0]) is None

    def test_a_busy_volume_does_not_hold_its_series_turn(self, storage: Path) -> None:
        # Alpha 1 is running; the round moves on rather than leaving Alpha's
        # turn parked on the volume that cannot start.
        _library(storage, Alpha=["1", "2"], Beta=["1"])
        worker = _worker(storage, _gens(PRIMARY, SECOND), concurrency=2)
        first = worker.claim_next(worker._slots[0])
        assert first is not None and _triple(worker, first) == ("Alpha", "1", "mokuro")
        second = worker.claim_next(worker._slots[1])
        assert second is not None and _triple(worker, second) == ("Beta", "1", "mokuro")

    def test_claiming_marks_the_job_and_its_row_on_its_slot(self, storage: Path) -> None:
        _library(storage, Alpha=["1"])
        generations = _gens(PRIMARY)
        worker = _worker(storage, generations, concurrency=2)
        slot = worker._slots[1]
        job = worker.claim_next(slot)
        assert slot.job == job
        # The row is frozen onto the slot at claim time: a settings change
        # landing mid-run cannot move the file this job writes.
        assert slot.generation is not None and slot.generation.id == generations[0].id
        assert worker._slots[0].job is None

    def test_an_empty_queue_claims_nothing(self, storage: Path) -> None:
        worker = _worker(storage, _gens(PRIMARY), concurrency=2)
        assert worker.claim_next(worker._slots[0]) is None

    def test_a_secondary_row_is_claimable_before_the_primary_sidecar(
        self, storage: Path
    ) -> None:
        # A volume with nothing on disk offers every row at once, in order.
        _library(storage, Alpha=["1"])
        worker = _worker(storage, _gens(PRIMARY, SECOND, THIRD), concurrency=3)
        claimed = [worker.claim_next(slot) for slot in worker._slots]
        cbz = storage / "library" / "Alpha" / "1.cbz"
        assert claimed == [(cbz, row.id) for row in worker.generations]


class TestSerialBehaviourIsUnchanged:
    def test_one_slot_runs_the_projected_list_in_order(self, storage: Path) -> None:
        _library(storage, primary_done=True, Alpha=["1", "2", "3"], Beta=["1"], Gamma=["1", "2"])
        worker = _worker(storage, _gens(PRIMARY, SECOND, THIRD))
        projected = _projected(worker)
        assert len(projected) == 12
        recorder = _Recorder(worker)
        _run_scan(recorder)
        assert recorder.ran == projected
        assert all(len(overlap) == 1 for overlap in recorder.overlaps)

    def test_one_slot_creates_no_threads(self, storage: Path) -> None:
        _library(storage, Alpha=["1", "2"])
        worker = _worker(storage, _gens(PRIMARY))
        seen: list[str] = []
        real_thread = threading.Thread

        def spy(*args: Any, **kwargs: Any) -> threading.Thread:
            seen.append(str(kwargs.get("name")))
            return real_thread(*args, **kwargs)

        recorder = _Recorder(worker)
        worker._running = True
        patchers = recorder.install()
        try:
            with patch("mokuro_bunko.ocr.watcher.threading.Thread", side_effect=spy):
                worker._scan_ocr_once()
        finally:
            for patcher in patchers:
                patcher.stop()
        assert seen == []
        assert len(recorder.ran) == 2


class TestDraining:
    def test_slots_drain_the_queue_in_the_schedulers_order(self, storage: Path) -> None:
        _library(storage, Alpha=["1", "2", "3"], Beta=["1", "2"], Gamma=["1"])
        worker = _worker(storage, _gens(PRIMARY), concurrency=3)
        projected = _projected(worker)
        recorder = _Recorder(worker, duration=0.02)
        _run_scan(recorder)
        assert sorted(recorder.ran) == sorted(projected)
        assert len(recorder.ran) == 6
        # Every job ran once and the queue is empty afterwards.
        assert len(set(recorder.ran)) == 6
        assert worker.pending_jobs(max_age=0) == []

    def test_slots_really_run_at_the_same_time(self, storage: Path) -> None:
        _library(storage, Alpha=["1"], Beta=["1"], Gamma=["1"])
        worker = _worker(storage, _gens(PRIMARY), concurrency=3)
        recorder = _Recorder(worker, hold=True)
        worker._running = True
        patchers = recorder.install()
        scan = threading.Thread(target=worker._scan_ocr_once)
        scan.start()
        try:
            for _ in range(3):
                assert recorder.started.acquire(timeout=10)
            assert max(len(overlap) for overlap in recorder.overlaps) == 3
            # Three different volumes, as the rule demands.
            widest = max(recorder.overlaps, key=len)
            assert len({series for series, _, _ in widest}) == 3
        finally:
            recorder.release.set()
            scan.join(20)
            for patcher in patchers:
                patcher.stop()
        assert not scan.is_alive()
        assert len(recorder.ran) == 3

    def test_two_generations_of_one_volume_run_together(self, storage: Path) -> None:
        # One volume, two claimable rows, four slots: both at once.
        _library(storage, primary_done=True, Alpha=["1"])
        worker = _worker(storage, _gens(PRIMARY, SECOND, THIRD), concurrency=4)
        recorder = _Recorder(worker, hold=True)
        worker._running = True
        patchers = recorder.install()
        scan = threading.Thread(target=worker._scan_ocr_once)
        scan.start()
        try:
            for _ in range(2):
                assert recorder.started.acquire(timeout=10)
            assert max(recorder.overlaps, key=len) == {
                ("Alpha", "1", "hayai-nova"),
                ("Alpha", "1", "paddle-manga"),
            }
        finally:
            recorder.release.set()
            scan.join(20)
            for patcher in patchers:
                patcher.stop()
        assert not scan.is_alive()
        assert sorted(recorder.ran) == [
            ("Alpha", "1", "hayai-nova"),
            ("Alpha", "1", "paddle-manga"),
        ]

    def test_a_slot_waits_for_a_blocked_volume_instead_of_giving_up(self, storage: Path) -> None:
        # Only one volume is left and it has two rows to run: the second
        # slot must not leave the scan, or the second sidecar would wait for
        # the next poll interval.
        _library(storage, primary_done=True, Alpha=["1"])
        worker = _worker(storage, _gens(PRIMARY, SECOND, THIRD), concurrency=2)
        recorder = _Recorder(worker, duration=0.05)
        _run_scan(recorder)
        assert len(recorder.ran) == 2
        assert worker.pending_jobs(max_age=0) == []

    def test_a_failing_job_does_not_poison_another_slot(self, storage: Path) -> None:
        _library(storage, Alpha=["1"], Beta=["1"], Gamma=["1"])
        worker = _worker(storage, _gens(PRIMARY), concurrency=3)
        recorder = _Recorder(worker, fail={("1", "mokuro")}, duration=0.02)

        # Each slot fails with its own reason; the record must name the
        # reason of the job it belongs to, not whichever landed last.
        def failing(path: Path, generation: GenerationSpec) -> bool:
            recorder(path, generation)
            return False

        worker._running = True
        patchers = []
        for index, slot in enumerate(worker._slots):
            slot.processor.last_failure = OcrFailure(error=f"slot {index} broke")
            patchers.append(
                patch.object(slot.processor, "process_library_ocr", side_effect=failing)
            )
        for patcher in patchers:
            patcher.start()
        try:
            worker._scan_ocr_once()
        finally:
            for patcher in patchers:
                patcher.stop()

        failures = json.loads((storage / ".ocr-failures.json").read_text(encoding="utf-8"))
        assert sorted(failures) == ["Alpha/1.cbz", "Beta/1.cbz", "Gamma/1.cbz"]
        # Every record is one attempt on its own volume with a real reason.
        for entry in failures.values():
            assert entry["attempts"] == 1
            assert entry["error"].startswith("slot ")
        assert len(recorder.ran) == 3

    def test_a_slot_that_blows_up_does_not_take_the_scan_with_it(self, storage: Path) -> None:
        # Slot 1 (a helper thread) raises on whatever it picks up; slot 0
        # must still drain everything else, and the server log must say a
        # slot stopped rather than the thread dying into stderr.
        _library(storage, Alpha=["1"], Beta=["1"], Gamma=["1"], Delta=["1"])
        messages: list[str] = []
        worker = OCRWorker(
            storage_path=storage,
            poll_interval=30.0,
            generations=_gens(PRIMARY),
            engines_python_path=Path("/nonexistent"),
            concurrency=2,
            status_callback=messages.append,
        )
        recorder = _Recorder(worker, duration=0.02)

        def explode(path: Path, generation: GenerationSpec) -> bool:
            raise RuntimeError("engine environment exploded")

        worker._running = True
        patchers = [
            patch.object(worker._slots[0].processor, "process_library_ocr", side_effect=recorder),
            patch.object(worker._slots[1].processor, "process_library_ocr", side_effect=explode),
        ]
        for patcher in patchers:
            patcher.start()
        try:
            worker._scan_ocr_once()
        finally:
            for patcher in patchers:
                patcher.stop()

        # The healthy slot ran everything the dead one did not claim.
        assert len(recorder.ran) == 3
        assert any("OCR slot 1 stopped" in message for message in messages)
        # Nothing is left marked in flight by either slot.
        assert worker._inflight_ocr == set()
        assert all(slot.job is None for slot in worker._slots)

    def test_with_one_slot_a_raising_job_still_ends_the_scan(self, storage: Path) -> None:
        # Unchanged from before concurrency existed: the scan thread's
        # exception reaches `_run_ocr_loop`, which logs it and waits for
        # the next poll.
        _library(storage, Alpha=["1"], Beta=["1"])
        worker = _worker(storage, _gens(PRIMARY))

        def explode(path: Path, generation: GenerationSpec) -> bool:
            raise RuntimeError("engine environment exploded")

        worker._running = True
        with patch.object(worker.processor, "process_library_ocr", side_effect=explode):
            with pytest.raises(RuntimeError):
                worker._scan_ocr_once()
        assert worker._inflight_ocr == set()


class TestCancellation:
    def test_a_removed_generation_cancels_only_its_own_slots(self, storage: Path) -> None:
        # Both volumes already have their primary sidecar, so both slots end
        # up on the one secondary row -- the row that is then dropped.
        _library(storage, primary_done=True, Alpha=["1"], Beta=["1"])
        worker = _worker(storage, _gens(PRIMARY, SECOND), concurrency=2)

        cancelled: list[int] = []
        started = threading.Semaphore(0)
        release = threading.Event()

        def hold(path: Path, generation: GenerationSpec) -> bool:
            started.release()
            assert release.wait(10)
            return False

        patchers = []
        for index, slot in enumerate(worker._slots):
            patchers.append(patch.object(slot.processor, "process_library_ocr", side_effect=hold))
            patchers.append(
                patch.object(
                    slot.processor,
                    "cancel_active",
                    side_effect=lambda index=index: (cancelled.append(index), True)[1],
                )
            )
        for patcher in patchers:
            patcher.start()
        worker._running = True
        scan = threading.Thread(target=worker._scan_ocr_once)
        scan.start()
        try:
            for _ in range(2):
                assert started.acquire(timeout=10)
            # hayai-nova is dropped: both running jobs are hayai-nova jobs.
            worker.apply_settings(_gens(PRIMARY))
            assert sorted(cancelled) == [0, 1]
        finally:
            release.set()
            scan.join(20)
            for patcher in patchers:
                patcher.stop()
        # Cancelled, not failed: no failure record for a job nobody wants.
        assert not (storage / ".ocr-failures.json").exists()

    def test_a_slot_running_a_kept_generation_is_left_alone(self, storage: Path) -> None:
        _library(storage, Alpha=["1"], Beta=["1"])
        generations = _gens(PRIMARY, SECOND)
        worker = _worker(storage, generations, concurrency=2)
        worker._slots[0].job = (storage / "library" / "Alpha" / "1.cbz", generations[0].id)
        worker._slots[0].generation = generations[0]
        worker._slots[1].job = (storage / "library" / "Beta" / "1.cbz", generations[1].id)
        worker._slots[1].generation = generations[1]
        cancelled: list[int] = []
        patchers = [
            patch.object(
                slot.processor,
                "cancel_active",
                side_effect=lambda index=index: (cancelled.append(index), True)[1],
            )
            for index, slot in enumerate(worker._slots)
        ]
        for patcher in patchers:
            patcher.start()
        try:
            worker.apply_settings(_gens(PRIMARY))
        finally:
            for patcher in patchers:
                patcher.stop()
        assert cancelled == [1]

    def test_settings_reach_every_slots_processor(self, storage: Path) -> None:
        worker = _worker(storage, _gens(PRIMARY, SECOND), concurrency=3)
        updated = _gens(
            PRIMARY,
            {
                "name": "paddle-ctd",
                "engine": "paddle-manga",
                "detector": "ctd",
                "patch_budget": 256,
            },
        )
        worker.apply_settings(updated)
        for slot in worker._slots:
            # The rows carry the engine, detector, character map and patch
            # budget now, so one comparison covers all of them.
            assert slot.processor.generations == updated
            assert slot.processor.generations[1].detector == "ctd"
            assert slot.processor.generations[1].patch_budget == 256


class TestProgressReporting:
    def _progress(self, storage: Path) -> dict[str, Any]:
        return json.loads((storage / ".ocr-progress.json").read_text(encoding="utf-8"))

    def test_one_job_writes_the_shape_readers_already_know(self, storage: Path) -> None:
        _library(storage, Alpha=["1"])
        worker = _worker(storage, _gens(PRIMARY), concurrency=2)
        recorder = _Recorder(worker, hold=True)
        worker._running = True
        patchers = recorder.install()
        scan = threading.Thread(target=worker._scan_ocr_once)
        scan.start()
        try:
            assert recorder.started.acquire(timeout=10)
            data = self._progress(storage)
            assert data["active"] is True
            assert (data["series"], data["volume"]) == ("Alpha", "1")
            assert data["relative_cbz"] == "Alpha/1.cbz"
            assert data["generation"] == "mokuro"
            assert [job["volume"] for job in data["jobs"]] == ["1"]
        finally:
            recorder.release.set()
            scan.join(20)
            for patcher in patchers:
                patcher.stop()
        assert not (storage / ".ocr-progress.json").exists()

    def test_every_running_job_is_reported(self, storage: Path) -> None:
        _library(storage, Alpha=["1"], Beta=["1"], Gamma=["1"])
        worker = _worker(storage, _gens(PRIMARY), concurrency=3)
        recorder = _Recorder(worker, hold=True)
        worker._running = True
        patchers = recorder.install()
        scan = threading.Thread(target=worker._scan_ocr_once)
        scan.start()
        try:
            for _ in range(3):
                assert recorder.started.acquire(timeout=10)
            data = self._progress(storage)
            assert sorted(job["series"] for job in data["jobs"]) == ["Alpha", "Beta", "Gamma"]
            # The top level is the first of them, for readers that know one.
            assert data["series"] == data["jobs"][0]["series"]
            assert data["active"] is True
        finally:
            recorder.release.set()
            scan.join(20)
            for patcher in patchers:
                patcher.stop()
        assert not (storage / ".ocr-progress.json").exists()

    def test_one_job_finishing_leaves_the_others_reported(self, storage: Path) -> None:
        alpha = _make_cbz(storage / "library" / "Alpha" / "1.cbz")
        beta = _make_cbz(storage / "library" / "Beta" / "1.cbz")
        generations = _gens(PRIMARY)
        gen_id = generations[0].id
        worker = _worker(storage, generations, concurrency=2)
        worker._set_active_progress((alpha, gen_id), {"series": "Alpha", "volume": "1"})
        worker._set_active_progress((beta, gen_id), {"series": "Beta", "volume": "1"})
        assert len(self._progress(storage)["jobs"]) == 2
        worker._clear_active_progress((alpha, gen_id))
        data = self._progress(storage)
        assert [job["series"] for job in data["jobs"]] == ["Beta"]
        assert data["series"] == "Beta"
        worker._clear_active_progress((beta, gen_id))
        assert not (storage / ".ocr-progress.json").exists()


class _Resp:
    def __init__(self) -> None:
        self.status = ""

    def start_response(
        self, status: str, headers: list[tuple[str, str]], exc_info: Any = None
    ) -> Callable[[bytes], None]:
        self.status = status
        return lambda data: None


def _dummy_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [])
    return [b""]


def _status(app: Callable[..., Any]) -> dict[str, Any]:
    resp = _Resp()
    environ = {
        "REQUEST_METHOD": "GET",
        "PATH_INFO": "/queue/api/status",
        "QUERY_STRING": "",
        "wsgi.input": io.BytesIO(b""),
        "wsgi.errors": io.StringIO(),
        "wsgi.url_scheme": "http",
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
    }
    b"".join(app(environ, resp.start_response))
    assert resp.status.startswith("200"), resp.status
    # The endpoint sends a shaped, per-level payload (`queue.shape`); these
    # tests are about the model underneath it, which `raw_status` returns.
    return app.raw_status()  # type: ignore[attr-defined, no-any-return]


class TestQueueApi:
    def _app(
        self,
        storage: Path,
        worker: OCRWorker | None,
        generations: list[GenerationSpec],
    ) -> QueueAPI:
        control = OcrControl()
        control.worker = worker
        return QueueAPI(
            _dummy_app,
            storage_base_path=str(storage),
            generations=generations,
            ocr_control=control,
        )

    def test_reports_every_running_job(self, storage: Path) -> None:
        _library(storage, Alpha=["1"], Beta=["1"], Gamma=["1"])
        generations = _gens(PRIMARY)
        worker = _worker(storage, generations, concurrency=3)
        app = self._app(storage, worker, generations)
        recorder = _Recorder(worker, hold=True)
        worker._running = True
        patchers = recorder.install()
        scan = threading.Thread(target=worker._scan_ocr_once)
        scan.start()
        try:
            for _ in range(3):
                assert recorder.started.acquire(timeout=10)
            data = _status(app)
            running = {
                (job["series"], job["volume"], job["generation"], job["engine"])
                for job in data["current_jobs"]
            }
            assert running == {
                ("Alpha", "1", "mokuro", "mokuro"),
                ("Beta", "1", "mokuro", "mokuro"),
                ("Gamma", "1", "mokuro", "mokuro"),
            }
            # `current` stays the single-job field it always was.
            assert data["current"] == data["current_jobs"][0]
            # None of them is listed as pending as well.
            assert data["pending_ocr"] == []
        finally:
            recorder.release.set()
            scan.join(20)
            for patcher in patchers:
                patcher.stop()

    def test_a_progress_file_without_jobs_is_read_as_one_job(self, storage: Path) -> None:
        # What every version before `ocr.concurrency` wrote.
        (storage / ".ocr-progress.json").write_text(
            json.dumps(
                {
                    "active": True,
                    "series": "S",
                    "volume": "A",
                    "generation": "paddle-manga",
                    "engine": "paddle-manga",
                    "percent": 40,
                    "status": "running",
                }
            ),
            encoding="utf-8",
        )
        data = _status(self._app(storage, None, _gens(PRIMARY)))
        assert data["current"]["volume"] == "A"
        assert data["current_jobs"] == [data["current"]]

    def test_an_idle_worker_reports_no_running_job(self, storage: Path) -> None:
        _library(storage, Alpha=["1"])
        generations = _gens(PRIMARY)
        data = _status(
            self._app(storage, _worker(storage, generations, concurrency=2), generations)
        )
        assert data["current"] is None
        assert data["current_jobs"] == []

    def test_without_a_worker_every_running_job_is_hidden_from_pending(self, storage: Path) -> None:
        # OCR disabled: the page builds the list itself and must not repeat
        # a job the progress file already reports as running.
        _library(storage, Alpha=["1", "2"], Beta=["1"])
        (storage / ".ocr-progress.json").write_text(
            json.dumps(
                {
                    "active": True,
                    "series": "Alpha",
                    "volume": "1",
                    "generation": "mokuro",
                    "engine": "mokuro",
                    "relative_cbz": "Alpha/1.cbz",
                    "jobs": [
                        {
                            "series": "Alpha",
                            "volume": "1",
                            "generation": "mokuro",
                            "engine": "mokuro",
                            "relative_cbz": "Alpha/1.cbz",
                        },
                        {
                            "series": "Beta",
                            "volume": "1",
                            "generation": "mokuro",
                            "engine": "mokuro",
                            "relative_cbz": "Beta/1.cbz",
                        },
                    ],
                }
            ),
            encoding="utf-8",
        )
        data = _status(self._app(storage, None, _gens(PRIMARY)))
        assert len(data["current_jobs"]) == 2
        assert [(j["series"], j["volume"]) for j in data["pending_ocr"]] == [("Alpha", "2")]
