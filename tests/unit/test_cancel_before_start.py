"""A cancel or pre-empt that lands before its process exists is never lost.

Traced (TestPreemptionForBenchmark, 2/10 under load): the claim set
`slot.job` at 0.1740 s, the benchmark's pre-empt ran `cancel_active()` at
0.1741 s and found no process (``-> False``), and the job's subprocess
started 24 ms later and ran its whole 30 s: the pre-empt waited it out and
timed out. The one-volume path also CLEARED the cancel flag the moment it
started its subprocess, so even a recorded request could not survive that
window; and an `OcrSession` killed before `start()` started its runner
anyway.

Each test here injects the cancel in exactly that window -- with a hook on
the start itself, no sleeps -- and asserts nothing runs past it.
"""

from __future__ import annotations

import subprocess
import sys
import threading
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import processor as processor_module
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.session import OcrSession
from tests.unit.test_ocr_sessions import PRIMARY, _gens, _library, _worker

SLEEP_30 = [sys.executable, "-c", "import time; time.sleep(30)"]


class _Spawned:
    """Records every subprocess started, so a test can prove none outlived a cancel."""

    def __init__(self, monkeypatch: pytest.MonkeyPatch, module: Any) -> None:
        self.processes: list[subprocess.Popen[Any]] = []
        real = subprocess.Popen

        def popen(*args: Any, **kwargs: Any) -> subprocess.Popen[Any]:
            process = real(*args, **kwargs)
            self.processes.append(process)
            return process

        monkeypatch.setattr(module.subprocess, "Popen", popen)

    def none_running(self) -> bool:
        for process in self.processes:
            if process.poll() is None:
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    return False
        return True


def _cancel_asked(worker: Any, monkeypatch: pytest.MonkeyPatch) -> threading.Event:
    """Set once anything asks the local slot's processor to cancel."""
    asked = threading.Event()
    processor = worker._slots[0].processor
    real = processor.cancel_active

    def cancel_active() -> bool:
        try:
            return real()
        finally:
            asked.set()

    monkeypatch.setattr(processor, "cancel_active", cancel_active)
    return asked


def _processor(tmp_path: Path) -> OCRProcessor:
    return OCRProcessor(
        storage_path=tmp_path,
        python_path=Path(sys.executable),
        generations=parse_generation_list([PRIMARY]),
        engines_python_path=Path(sys.executable),
    )


class TestOneVolumeRuns:
    def test_a_cancel_before_the_process_exists_means_it_never_starts(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor = _processor(tmp_path)
        spawned = _Spawned(monkeypatch, processor_module)
        processor.cancel_check = lambda: True  # the worker's record: cancelled
        result = processor._run_ocr_subprocess(
            SLEEP_30, tmp_path, tmp_path, tmp_path / "run.log", label="Fake"
        )
        assert not result.ok and "cancel" in (result.error or "")
        assert spawned.processes == [], "nothing was started"

    def test_a_cancel_between_the_check_and_the_popen_kills_it_at_once(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The narrowest window: requested while Popen itself is running."""
        processor = _processor(tmp_path)
        real = subprocess.Popen
        started: list[subprocess.Popen[Any]] = []

        def popen(*args: Any, **kwargs: Any) -> subprocess.Popen[Any]:
            process = real(*args, **kwargs)
            started.append(process)
            # The pre-empt lands now: the process exists, nobody holds it yet.
            processor.cancel_active()
            return process

        monkeypatch.setattr(processor_module.subprocess, "Popen", popen)
        result = processor._run_ocr_subprocess(
            SLEEP_30, tmp_path, tmp_path, tmp_path / "run.log", label="Fake"
        )
        assert not result.ok and "cancel" in (result.error or "")
        (process,) = started
        assert process.wait(timeout=5) is not None, "it was killed, not left to run"

    def test_a_recorded_cancel_is_never_cleared_by_the_start(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor = _processor(tmp_path)
        spawned = _Spawned(monkeypatch, processor_module)
        processor._cancel_requested = True  # asked for before the process existed
        result = processor._run_ocr_subprocess(
            SLEEP_30, tmp_path, tmp_path, tmp_path / "run.log", label="Fake"
        )
        assert not result.ok
        assert spawned.none_running()

    def test_a_new_job_starts_clean(self, tmp_path: Path) -> None:
        """What the flag used to be cleared at Popen FOR: the last job's cancel
        must not cancel the next one. Cleared at the job's start instead."""
        processor = _processor(tmp_path)
        processor._cancel_requested = True
        processor.begin_job()
        assert processor._cancel_requested is False

    def test_the_worker_pre_empt_lands_between_claim_and_popen(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """End to end through the worker: a hook fires the pre-empt from the
        job's own thread, after the claim and before its process exists."""
        storage = tmp_path
        (storage / "inbox").mkdir(exist_ok=True)
        _library(storage, Alpha=["Volume 1"], primary_done=False)
        worker = _worker(storage, _gens(PRIMARY))
        spawned = _Spawned(monkeypatch, processor_module)
        outcome: dict[str, Any] = {}
        asked = _cancel_asked(worker, monkeypatch)
        preempted = threading.Event()

        def preempt() -> None:
            outcome["preempt"] = worker.preempt_for_bench(timeout=10.0)
            preempted.set()

        def process_library_ocr(cbz: Path, generation: Any) -> bool:
            # The claim is made and `slot.job` is set; no process exists.
            threading.Thread(target=preempt, daemon=True).start()
            # The pre-empt has marked the job and asked its slot to cancel --
            # with nothing yet to kill -- before this job starts its process.
            assert asked.wait(10), "the pre-empt never asked the slot to cancel"
            result = worker.processor._run_ocr_subprocess(
                SLEEP_30, storage, storage, storage / "run.log", label="Fake"
            )
            return result.ok

        monkeypatch.setattr(worker.processor, "process_library_ocr", process_library_ocr)
        _scan_through_the_hold(worker, preempted)
        quiet, which = outcome["preempt"]
        assert quiet is True, "the pre-empt waited out a process it had cancelled"
        assert which == [{"generation": "mokuro", "volume": "Volume 1"}]
        # Only the job's own command counts: the scan probes interpreters too.
        assert [p for p in spawned.processes if p.args == SLEEP_30] == [], (
            "the cancelled job started nothing"
        )
        assert not (storage / ".ocr-failures.json").exists()


def _scan_through_the_hold(worker: Any, preempted: threading.Event) -> None:
    """Run a scan; once the pre-empt is done, release the hold it took (the
    scan waits a held machine out) and let the scan end."""
    scan = threading.Thread(target=worker._scan_ocr_once, daemon=True)
    scan.start()
    try:
        assert preempted.wait(20), "the pre-empt never returned"
    finally:
        worker.release_queue()
        worker._stop_requested = True
        scan.join(timeout=30)
    assert not scan.is_alive()


class TestSessions:
    def _session(self, tmp_path: Path) -> OcrSession:
        row = parse_generation_list([PRIMARY])[0]
        return OcrSession(row, SLEEP_30, session_log=tmp_path / "session.log")

    def test_a_session_killed_before_start_never_starts_a_runner(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.ocr import session as session_module

        spawned = _Spawned(monkeypatch, session_module)
        session = self._session(tmp_path)
        session.kill()
        assert session.start() is False
        assert spawned.processes == []
        assert session.poll_event(timeout=1.0)["event"] == "exit"

    def test_a_kill_while_popen_runs_ends_the_runner_at_once(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.ocr import session as session_module

        session = self._session(tmp_path)
        real = subprocess.Popen
        started: list[subprocess.Popen[Any]] = []

        def popen(*args: Any, **kwargs: Any) -> subprocess.Popen[Any]:
            process = real(*args, **kwargs)
            started.append(process)
            session.kill()  # lands before `start` has stored the process
            return process

        monkeypatch.setattr(session_module.subprocess, "Popen", popen)
        monkeypatch.setattr(session_module, "hold_staged_runner", lambda path: None)
        session.start()
        (process,) = started
        assert process.wait(timeout=5) is not None, "the runner was killed"
        session.join_reader()

    def test_a_session_slot_pre_empted_before_its_runner_opens_runs_nothing(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A session row: claimed, pre-empted before `slot.session` exists."""
        storage = tmp_path
        (storage / "inbox").mkdir(exist_ok=True)
        rows = _gens(PRIMARY, {"name": "hayai-nova", "engine": "hayai-nova"})
        _library(storage, Alpha=["Volume 1"])
        worker = _worker(storage, rows)
        outcome: dict[str, Any] = {}
        asked = _cancel_asked(worker, monkeypatch)
        real_open = worker._slots[0].processor.open_session
        preempted = threading.Event()

        def preempt() -> None:
            outcome["preempt"] = worker.preempt_for_bench(timeout=10.0)
            preempted.set()

        def open_session(*args: Any, **kwargs: Any) -> Any:
            # Claimed; the runner does not exist yet. The pre-empt lands now.
            threading.Thread(target=preempt, daemon=True).start()
            assert asked.wait(10), "the pre-empt never asked the slot to cancel"
            return real_open(*args, **kwargs)

        monkeypatch.setattr(worker._slots[0].processor, "open_session", open_session)
        from mokuro_bunko.ocr import session as session_module

        spawned = _Spawned(monkeypatch, session_module)
        _scan_through_the_hold(worker, preempted)
        quiet, which = outcome["preempt"]
        assert quiet is True
        assert which == [{"generation": "hayai-nova", "volume": "Volume 1"}]
        assert spawned.processes == [], "no runner was started for it"
        assert not (storage / ".ocr-failures.json").exists()
        assert not list((storage / "library").rglob("*.hayai-nova.mokuro"))
