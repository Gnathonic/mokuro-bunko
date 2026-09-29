"""What the library does with a claim its processor could not deliver.

Protocol 2 (design section 6): the processor never records anything and
never fails a volume. It delivers the verified archive to its runner -- and
says `fetch {state: ready}` -- or gives the claim back with
`volume_returned {class}`. The library judges a returned claim from the one
thing only it can see, its own file, and counts returns per processor (the
download breaker) and per job (a "download failed" record after three).

These run the worker's REAL loop against a processor that never answers on
its own: every event is fed into the real `RemoteSession` the worker opened,
exactly as the events sink would.
"""

from __future__ import annotations

import json
import os
import sys
import threading
import time
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import watcher as watcher_module
from mokuro_bunko.ocr.devices import DeviceCatalog
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.remote.session import RemoteSession
from mokuro_bunko.ocr.watcher import (
    DOWNLOAD_BREAKER_HOLD,
    OCRWorker,
    _SessionJob,
)
from mokuro_bunko.queue.shape import REASON_DOWNLOAD, failure_reason

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


def _library(storage: Path, *volumes: str, series: str = "Alpha") -> list[Path]:
    """Volumes whose primary layer is done: only the second row is owed."""
    out = []
    for volume in volumes:
        cbz = storage / "library" / series / f"{volume}.cbz"
        cbz.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(cbz, "w") as zf:
            for n in range(2):
                zf.writestr(f"page_{n:03d}.jpg", b"fake image data")
        cbz.with_suffix(".mokuro").write_text(
            json.dumps({"version": "0.0", "volume_uuid": f"u-{volume}",
                        "pages": [], "chars": 0}),
            encoding="utf-8",
        )
        out.append(cbz)
    return out


def _worker(storage: Path, registry: ProcessorRegistry) -> OCRWorker:
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=_gens(),
        engines_python_path=Path(sys.executable),
        concurrency=1,
        sessions=True,
        remote=registry,
        local_processing=False,
    )
    registry.on_drop = worker.processor_disconnected
    return worker


def _connect(registry: ProcessorRegistry, name: str = "tower") -> Any:
    entry = registry.register(
        username=name, name=name, host={"gpu": "RTX 4090"}, catalog=FULL, max_sessions=1,
    )
    entry.stream_open = True
    return entry


def _wait(predicate: Callable[[], bool], timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.02)
    return predicate()


def _ops(entry: Any) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    while not entry.ops.empty():
        op = entry.ops.get_nowait()
        if op is not None:
            out.append(op)
    return out


def _failures(storage: Path) -> dict[str, Any]:
    path = storage / ".ocr-failures.json"
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}


def _session_of(entry: Any) -> RemoteSession:
    assert _wait(lambda: any(isinstance(s, RemoteSession) for s in list(entry.sessions.values())))
    return next(s for s in list(entry.sessions.values()) if isinstance(s, RemoteSession))


def _claim_of(session: RemoteSession, volume: str) -> str:
    """The claim id the session holds for a volume, by its op's archive."""
    for claim in session.claims():
        held = session._volumes.get(claim)
        if held is not None and held.archive is not None and held.archive.stem == volume:
            return claim
    raise AssertionError(f"{volume} is not held; claims {session.claims()}")


class _Scan:
    def __init__(self, worker: OCRWorker) -> None:
        self.worker = worker
        self.thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        self.thread.start()

    def stop(self, registry: ProcessorRegistry) -> None:
        self.worker._stop_requested = True
        for entry in registry.entries():
            if not entry.local:
                registry.drop(entry.processor_id, "the test is over")
        self.thread.join(timeout=15)
        assert not self.thread.is_alive()


def _returned(session: RemoteSession, claim: str, klass: str = "stalled",
              error: str = "no new byte for 120 s") -> None:
    session.feed({"event": "volume_returned", "id": claim, "class": klass, "error": error,
                  "bytes": 0, "total": None, "requests": 7}, b"")


def _pending(worker: OCRWorker, volume: str) -> dict[str, Any] | None:
    for item in worker.pending_jobs(max_age=0):
        if item["volume"] == volume:
            return item
    return None


class TestAReturnedClaim:
    def test_it_is_unrecorded_unstruck_and_pending_again_with_its_reason(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(entry)
            assert _wait(lambda: len(session.claims()) == 2)
            session.feed({"event": "ready", "startup_seconds": 1.0}, b"")
            _returned(session, _claim_of(session, "Volume 1"))
            assert _wait(lambda: _pending(worker, "Volume 1") is not None)
            item = _pending(worker, "Volume 1")
            assert item is not None
            assert item["returned"]["class"] == "stalled"
            assert item["returned"]["machine"] == "tower"
            assert "attempts" not in item, "no attempt was spent"
            assert _failures(storage) == {}
            assert worker._session_strikes == {}
        finally:
            scan.stop(registry)

    def test_it_goes_to_another_processor_this_scan_and_to_its_own_next_scan(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        tower = _connect(registry, "tower")
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(tower)
            assert _wait(lambda: len(session.claims()) == 2)
            _ops(tower)
            _returned(session, _claim_of(session, "Volume 1"))
            assert _wait(lambda: _pending(worker, "Volume 1") is not None)
            time.sleep(0.5)  # a top-up would have gone by now
            again = [op for op in _ops(tower) if op["op"] == "volume"]
            assert again == [], "not offered back to the processor that returned it"
            box = _connect(registry, "box")
            assert _wait(lambda: any(
                op.get("op") == "volume" and op["archive"].endswith("Volume 1.cbz")
                for op in list(box.ops.queue) if op
            )), "another processor takes it in the same scan"
        finally:
            scan.stop(registry)
        assert worker._returned_by == {}, "a scan's returns are forgotten when it ends"

    def test_a_file_that_is_gone_is_released_and_not_counted(self, storage: Path) -> None:
        (volume_1, _v2) = _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(entry)
            assert _wait(lambda: len(session.claims()) == 2)
            claim = _claim_of(session, "Volume 1")
            volume_1.unlink()
            _returned(session, claim, "missing", "the library has no ... (404)")
            assert _wait(lambda: all(job[0] != volume_1 for job in worker._inflight_ocr))
            assert _pending(worker, "Volume 1") is None
            assert worker._download_returns == {}
            assert worker._breakers.get(entry.processor_id) is None or (
                worker._breakers[entry.processor_id].consecutive == 0
            )
            assert _failures(storage) == {}
        finally:
            scan.stop(registry)

    def test_missing_from_an_unproven_processor_counts_against_it_not_the_job(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(entry)
            assert _wait(lambda: len(session.claims()) == 2)
            _returned(session, _claim_of(session, "Volume 1"), "missing", "404")
            assert _wait(lambda: _pending(worker, "Volume 1") is not None)
            assert worker._breakers[entry.processor_id].consecutive == 1
            (returns,) = worker._download_returns.values()
            assert returns.count == 0, "an unproven path says nothing about the job"
        finally:
            scan.stop(registry)

    def test_a_file_that_changed_since_the_op_goes_straight_back_out(
        self, storage: Path
    ) -> None:
        (volume_1, _v2) = _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(entry)
            assert _wait(lambda: len(session.claims()) == 2)
            _ops(entry)
            claim = _claim_of(session, "Volume 1")
            with zipfile.ZipFile(volume_1, "a") as zf:
                zf.writestr("page_009.jpg", b"fake image data, a page added later")
            _returned(session, claim, "mismatch", "the library sent 999 bytes")
            assert _wait(lambda: any(
                op.get("op") == "volume" and op["archive"].endswith("Volume 1.cbz")
                and op["size"] == volume_1.stat().st_size
                for op in list(entry.ops.queue) if op
            )), "re-offered at once, to the same processor, with the fresh size"
            assert worker._download_returns == {}
            assert worker._breakers.get(entry.processor_id) is None
        finally:
            scan.stop(registry)

    @pytest.mark.skipif(
        sys.platform == "win32" or (hasattr(os, "geteuid") and os.geteuid() == 0),
        reason="root reads a mode-000 file",
    )
    def test_a_file_the_library_cannot_read_itself_is_recorded(self, storage: Path) -> None:
        (volume_1, _v2) = _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(entry)
            assert _wait(lambda: len(session.claims()) == 2)
            claim = _claim_of(session, "Volume 1")
            volume_1.chmod(0)
            _returned(session, claim, "stalled", "the library answered 500 twice in a row")
            assert _wait(lambda: bool(_failures(storage)))
            (record,) = _failures(storage).values()
            assert record["error"].startswith(
                "the library cannot read its own copy of this archive"
            )
            assert "Permission denied" in record["error"]
            assert worker._breakers.get(entry.processor_id) is None, "the breaker is untouched"
        finally:
            volume_1.chmod(0o644)
            scan.stop(registry)


class TestTheDownloadBreaker:
    def test_three_returns_in_a_row_hold_the_processor_and_a_ready_frees_it(
        self, storage: Path
    ) -> None:
        _library(storage, "Volume 1", "Volume 2", "Volume 3", "Volume 4", "Volume 5")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(entry)
            assert _wait(lambda: len(session.claims()) == 2)
            session.feed({"event": "ready", "startup_seconds": 1.0}, b"")
            returned: list[str] = []
            for _ in range(3):
                assert _wait(lambda: len(session.claims()) >= 1)
                claim = session.claims()[0]
                held = session._volumes[claim]
                returned.append(held.archive.stem if held.archive else "")
                _returned(session, claim, "rejected", "the library answered 400")
                time.sleep(0.2)
            assert _wait(lambda: worker._breaker_open(entry.processor_id))
            assert worker._every_machine_held() is True
            rows = worker.connected_machines()
            assert rows[0]["held"] == "downloads"
            assert "rejected" in rows[0]["held_error"]
            assert entry.to_dict()["transfer"]["held_until"] is not None
            slot = worker._make_remote_slot(9, entry)
            assert worker.claim_next(slot) is None, "held: no claims"
            assert _failures(storage) == {}, "a breaker records nothing"
            # A claim still in flight arrives after all: the path works.
            remaining = session.claims()
            if remaining:
                session.feed({"event": "fetch", "id": remaining[0], "state": "ready",
                              "bytes": 1000, "seconds": 0.1, "requests": 1}, b"")
                assert _wait(lambda: not worker._breaker_open(entry.processor_id))
                assert entry.to_dict()["transfer"]["held_until"] is None
        finally:
            scan.stop(registry)

    def test_after_the_hold_one_more_return_reopens_it_for_twice_as_long(
        self, storage: Path
    ) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        slot = worker._make_remote_slot(0, entry)
        for _ in range(3):
            worker._note_download_return(slot, "missing", "404")
        breaker = worker._breakers[entry.processor_id]
        first = breaker.open_until - time.time()
        assert DOWNLOAD_BREAKER_HOLD - 5 < first <= DOWNLOAD_BREAKER_HOLD
        breaker.open_until = time.time() - 1  # the hold ran out; the count did not
        assert not worker._breaker_open(entry.processor_id)
        worker._note_download_return(slot, "missing", "404")
        second = breaker.open_until - time.time()
        assert 2 * DOWNLOAD_BREAKER_HOLD - 5 < second <= 2 * DOWNLOAD_BREAKER_HOLD

    def test_changed_is_the_file_s_and_never_counts_against_the_processor(
        self, storage: Path
    ) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        slot = worker._make_remote_slot(0, entry)
        for _ in range(5):
            worker._note_download_return(slot, "changed", "kept changing")
        assert not worker._breaker_open(entry.processor_id)

    def test_a_processor_that_comes_back_starts_clean(self, storage: Path) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        slot = worker._make_remote_slot(0, entry)
        for _ in range(3):
            worker._note_download_return(slot, "missing", "404")
        assert worker._breaker_open(entry.processor_id)
        registry.drop(entry.processor_id, "restarted after a fix")
        again = _connect(registry)
        assert not worker._breaker_open(again.processor_id)
        assert entry.processor_id not in worker._breakers


class TestTheJobsOwnCount:
    def test_a_proven_processor_returning_one_job_three_times_records_it(
        self, storage: Path
    ) -> None:
        (volume_1, volume_2) = _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        slot = worker._make_remote_slot(0, entry)
        hayai = worker.generations[1]

        def claim(volume: Path, job_id: str) -> _SessionJob:
            job = worker.claim_next(slot)
            assert job == (volume, hayai.id), job
            prepared = slot.processor.prepare_session_volume(volume, hayai, job_id)
            return _SessionJob(job=job, generation=hayai, volume=prepared, owner=slot,
                               hardware="tower")

        for n in range(1, 4):
            with worker._lock:  # a new scan
                worker._attempted_ocr = set()
                worker._returned_by = {}
            first = claim(volume_1, f"v{n}")
            # Another volume delivers meanwhile: the path is proven, and the
            # processor's run of returns is broken -- so Volume 1's own
            # return is evidence against Volume 1.
            other = claim(volume_2, f"w{n}")
            worker._handle_session_event(
                {"event": "fetch", "id": f"w{n}", "state": "ready", "requests": 1},
                hayai, {f"w{n}": other}, [f"w{n}"],
            )
            worker.release_ocr_job(other.job, hayai, reason="the test", slot=slot)
            worker._handle_session_event(
                {"event": "volume_returned", "id": f"v{n}", "class": "stalled",
                 "error": "no new byte for 120 s"},
                hayai, {f"v{n}": first}, [f"v{n}"],
            )
            if n < 3:
                assert _failures(storage) == {}, n
                assert worker._download_returns[first.job].count == n
        failures = _failures(storage)
        (record,) = failures.values()
        assert record["error"].startswith("download failed on 3 tries (tower, tower, tower)")
        assert "stalled" in record["error"]
        assert failure_reason(record["error"]) == REASON_DOWNLOAD
        assert worker._download_returns == {}


class TestBlame:
    def _run_until_exit(self, storage: Path, *, deliver_first: bool) -> dict[str, Any]:
        _library(storage, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(entry)
            assert _wait(lambda: len(session.claims()) == 2)
            first = session.claims()[0]
            session.feed({"event": "ready", "startup_seconds": 1.0}, b"")
            if deliver_first:
                session.feed({"event": "fetch", "id": first, "state": "ready",
                              "requests": 1, "bytes": 10, "seconds": 0.1}, b"")
            session.feed({"event": "fatal", "error": "HIP error: device lost"}, b"")
            session.feed({"event": "exit", "returncode": 1}, b"")
            assert _wait(lambda: not session.is_alive())
            assert _wait(lambda: session not in worker._open_sessions)
            worker._stop_requested = True
            return _failures(storage)
        finally:
            scan.stop(registry)

    def test_a_runner_that_dies_before_anything_was_delivered_blames_nothing(
        self, storage: Path
    ) -> None:
        assert self._run_until_exit(storage, deliver_first=False) == {}

    def test_a_runner_that_dies_with_a_delivered_volume_blames_that_one(
        self, storage: Path
    ) -> None:
        failures = self._run_until_exit(storage, deliver_first=True)
        assert list(failures) == ["Alpha/Volume 1.cbz@hayai-ctd"], failures
        assert "device lost" in failures["Alpha/Volume 1.cbz@hayai-ctd"]["error"]


class TestTheWedgeClock:
    def test_download_progress_alone_keeps_a_session_alive(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(watcher_module, "SESSION_WEDGE_SECONDS", 1.0)
        _library(storage, "Volume 1")
        registry = ProcessorRegistry()
        entry = _connect(registry)
        # The processor is there: its events body pings.
        worker = _worker(storage, registry)
        scan = _Scan(worker)
        try:
            session = _session_of(entry)
            assert _wait(lambda: len(session.claims()) == 1)
            claim = session.claims()[0]
            session.feed({"event": "ready", "startup_seconds": 1.0}, b"")
            deadline = time.monotonic() + 3.0
            while time.monotonic() < deadline:
                entry.last_seen = time.time()
                session.feed({"event": "fetch", "id": claim, "state": "retrying",
                              "bytes": 0, "requests": 3, "retry_in": 4.0}, b"")
                time.sleep(0.3)
            assert session.is_alive(), "killed as wedged while it was downloading"
            assert not session.killed
        finally:
            scan.stop(registry)
