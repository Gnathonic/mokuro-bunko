"""A row whose runner will not start on a machine is retried on a backoff.

Perf diagnosis F9: `paddle-manga-animetext` could not start on the workstation
(an onnxruntime with no GPU provider was asked for `cuda:0`), and every scan
tried it again -- 787 failed sessions in 2 h 21 m, one every 10.8 s. The
per-scan strike rule stops a row on a machine for the rest of a scan; this
spaces the attempts ACROSS scans the way a failed volume's retries are
spaced -- `min(poll_interval * 4^(n-1), 1 h)` -- per (row, machine), reset by
a session that becomes ready or by the row changing as that machine runs it.

The row below reads with `ctd`: `animetext` is disabled for now and a row
naming it no longer parses, and the backoff never depended on the detector.
"""

from __future__ import annotations

import json
import sys
import threading
import time
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.devices import DeviceCatalog
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.remote.session import RemoteSession
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.queue.shape import shape_status

PRIMARY: dict[str, Any] = {"name": "mokuro", "engine": "mokuro", "primary": True}
PADDLE: dict[str, Any] = {"name": "paddle-ctd", "engine": "paddle-manga",
                          "detector": "ctd"}
FULL: dict[str, Any] = {
    "engines": ["mokuro", "paddle-manga"], "detectors": ["ctd"],
    "devices": [], "serves_mokuro": True,
}
ERROR = "--device cuda:0 was asked for, but this onnxruntime has no GPU execution provider"


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    for volume in ("Volume 1", "Volume 2"):
        cbz = tmp_path / "library" / "Alpha" / f"{volume}.cbz"
        cbz.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(cbz, "w") as zf:
            zf.writestr("page_000.jpg", b"fake image data")
        cbz.with_suffix(".mokuro").write_text(
            json.dumps({"version": "0.0", "volume_uuid": f"u-{volume}", "pages": [],
                        "chars": 0}),
            encoding="utf-8",
        )
    return tmp_path


def _worker(storage: Path, registry: ProcessorRegistry, rows: list[dict[str, Any]] | None = None
            ) -> OCRWorker:
    worker = OCRWorker(
        storage_path=storage, poll_interval=10.0,
        generations=parse_generation_list(
            [dict(r) for r in (rows or [PRIMARY, PADDLE])], devices=DeviceCatalog()
        ),
        engines_python_path=Path(sys.executable), concurrency=1, sessions=True,
        remote=registry, local_processing=False, autobench=False,
    )
    registry.on_drop = worker.processor_disconnected
    return worker


def _connect(registry: ProcessorRegistry, name: str = "desktop") -> Any:
    entry = registry.register(username=name, name=name, host={"gpu": "RX 9070 XT"},
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


def _opens(sent: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [op for op in sent if op.get("op") == "open_session"]


def _record_ops(entry: Any, monkeypatch: pytest.MonkeyPatch) -> list[dict[str, Any]]:
    sent: list[dict[str, Any]] = []
    real = entry.send

    def send(op: dict[str, Any]) -> bool:
        sent.append(dict(op))
        return bool(real(op))

    monkeypatch.setattr(entry, "send", send)
    return sent


def _die_before_ready(entry: Any, stop: threading.Event) -> None:
    """Every session this processor opens dies before it is ready."""

    def run() -> None:
        seen: set[str] = set()
        while not stop.is_set():
            with entry.lock:
                sessions = [s for s in entry.sessions.values() if isinstance(s, RemoteSession)]
            for session in sessions:
                if session.sid in seen:
                    continue
                seen.add(session.sid)
                session.feed({"event": "fatal", "error": ERROR}, b"")
                session.feed({"event": "exit", "returncode": 1}, b"")
            time.sleep(0.01)

    threading.Thread(target=run, daemon=True).start()


def _paddle(worker: OCRWorker) -> GenerationSpec:
    return next(row for row in worker.generations if row.engine == "paddle-manga")


class TestTheStartBackoff:
    def test_a_runner_that_never_starts_is_not_tried_again_until_its_backoff(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry)
        sent = _record_ops(entry, monkeypatch)
        worker = _worker(storage, registry)
        stop = threading.Event()
        _die_before_ready(entry, stop)
        try:
            started = time.time()
            worker._scan_ocr_once()
            key = (_paddle(worker).id, "desktop")
            backoff = worker._start_backoff[key]
            assert backoff.failures == 1
            assert started + 9 < backoff.until <= time.time() + 10.5, "poll_interval x 4^0"
            assert ERROR in backoff.error
            assert len(_opens(sent)) == 1, "one attempt, then the backoff"
            assert not (storage / ".ocr-failures.json").exists(), "no volume is blamed"
            # The next scan, inside the backoff: nothing is opened at all.
            worker._scan_ocr_once()
            assert len(_opens(sent)) == 1, "a new scan does not re-arm it"
            pending = {j["volume"] for j in worker.pending_jobs(max_age=0)}
            assert pending == {"Volume 1", "Volume 2"}, "the volumes wait, unblamed"
        finally:
            stop.set()

    def test_each_failure_waits_four_times_longer_up_to_an_hour(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        slot = worker._make_remote_slot(0, entry)
        row = _paddle(worker)
        signature = worker._slot_start_signature(slot, row)
        waits = []
        for _ in range(7):
            worker._note_start_failure(row, "desktop", signature, ERROR)
            waits.append(round(worker._start_backoff[(row.id, "desktop")].until - time.time()))
        assert waits == [10, 40, 160, 640, 2560, 3600, 3600]

    def test_a_ready_session_clears_it(self, storage: Path) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        slot = worker._make_remote_slot(0, entry)
        row = _paddle(worker)
        worker._note_start_failure(row, "desktop", worker._slot_start_signature(slot, row), ERROR)
        assert worker._backed_off_rows(slot, "desktop", {row.id}) == {row.id}
        worker._handle_session_event({"event": "ready", "startup_seconds": 1.0}, row, {}, [],
                                     hardware="desktop")
        assert worker._start_backoff == {}
        assert worker._backed_off_rows(slot, "desktop", {row.id}) == set()

    def test_changing_the_row_as_that_machine_runs_it_allows_the_next_try_at_once(
        self, storage: Path
    ) -> None:
        """The config fix for F9's own case: `stage_device detect: cpu`."""
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        slot = worker._make_remote_slot(0, entry)
        row = _paddle(worker)
        worker._note_start_failure(row, "desktop", worker._slot_start_signature(slot, row), ERROR)
        assert worker._backed_off_rows(slot, "desktop", {row.id}) == {row.id}
        rows = [r.to_dict() for r in worker.generations]
        rows[1]["pools"] = {"stage_device": {"detect": "cpu"}}
        worker.apply_settings(parse_generation_list(rows, devices=DeviceCatalog()))
        assert worker._backed_off_rows(slot, "desktop", {row.id}) == set()

    def test_a_processor_that_comes_back_gets_a_fresh_try(self, storage: Path) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        row = _paddle(worker)
        slot = worker._make_remote_slot(0, entry)
        worker._note_start_failure(row, "desktop", worker._slot_start_signature(slot, row), ERROR)
        again = _connect(registry)  # re-registered after a reinstall
        fresh = worker._make_remote_slot(0, again)
        assert worker._backed_off_rows(fresh, "desktop", {row.id}) == set()

    def test_it_is_per_machine(self, storage: Path) -> None:
        registry = ProcessorRegistry()
        desktop = _connect(registry, "desktop")
        tower = _connect(registry, "tower")
        worker = _worker(storage, registry)
        row = _paddle(worker)
        slot = worker._make_remote_slot(0, desktop)
        worker._note_start_failure(row, "desktop", worker._slot_start_signature(slot, row), ERROR)
        assert worker.claim_next(worker._make_remote_slot(1, tower)) is not None

    def test_the_queue_and_admin_cards_say_so(self, storage: Path) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _worker(storage, registry)
        row = _paddle(worker)
        slot = worker._make_remote_slot(0, entry)
        worker._note_start_failure(row, "desktop", worker._slot_start_signature(slot, row), ERROR)
        (machine,) = worker.connected_machines()
        (cannot,) = machine["cannot_start"]
        assert cannot["generation"] == "paddle-ctd" and ERROR in cannot["error"]
        raw = {"connected_machines": [machine], "current_jobs": [], "pending_ocr": []}
        visitor = shape_status(raw, "normal", admin=False)["machines"][0]["cannot_start"]
        assert visitor == [{"generation": "paddle-ctd", "until": cannot["until"]}]
        admin = shape_status(raw, "normal", admin=True)["machines"][0]["cannot_start"]
        assert ERROR in admin[0]["error"]
