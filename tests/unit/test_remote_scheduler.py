"""Who gets offered what, and what happens when they vanish."""

from __future__ import annotations

import io
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
from mokuro_bunko.ocr.remote.scheduler import catalog_can_run
from mokuro_bunko.ocr.watcher import OCRWorker, _SessionJob

PRIMARY: dict[str, Any] = {"name": "mokuro", "engine": "mokuro", "primary": True}
HAYAI: dict[str, Any] = {"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"}
PADDLE: dict[str, Any] = {"name": "paddle", "engine": "paddle-manga", "detector": "ppocr-manga"}
FULL: dict[str, Any] = {
    "engines": ["mokuro", "hayai-nova"], "detectors": ["ctd"],
    "devices": [], "serves_mokuro": True,
}


def _gens(*rows: dict[str, Any]) -> list[GenerationSpec]:
    # An explicit UNPROBED catalog, rather than whatever `cached_catalog()`
    # holds: a row pinned to `gpu:1` must parse here on a host with no GPU,
    # and must not start refusing because another test published a probe.
    return parse_generation_list([dict(row) for row in rows], devices=DeviceCatalog())


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    return tmp_path


def _library(storage: Path, **series: list[str]) -> None:
    for name, volumes in series.items():
        for volume in volumes:
            cbz = storage / "library" / name / f"{volume}.cbz"
            cbz.parent.mkdir(parents=True, exist_ok=True)
            with zipfile.ZipFile(cbz, "w") as zf:
                for n in range(2):
                    zf.writestr(f"page_{n:03d}.jpg", b"fake image data")
            cbz.with_suffix(".mokuro").write_text(
                json.dumps({"version": "0.0", "volume_uuid": f"u-{volume}",
                            "pages": [], "chars": 0}),
                encoding="utf-8",
            )


def _wait(predicate: Callable[[], bool], timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()


def _progress_jobs(storage: Path) -> list[dict[str, Any]]:
    """The running jobs as the worker last wrote them ([] when none)."""
    try:
        data = json.loads((storage / ".ocr-progress.json").read_text("utf-8"))
    except (OSError, ValueError):
        return []
    jobs = data.get("jobs") if isinstance(data, dict) else None
    return jobs if isinstance(jobs, list) else []


def _ops(entry: Any) -> list[dict[str, Any]]:
    """Everything queued for this processor so far, sentinel excluded."""
    out: list[dict[str, Any]] = []
    while not entry.ops.empty():
        op = entry.ops.get_nowait()
        if op is not None:
            out.append(op)
    return out


class TestCatalogMatch:
    def test_a_processor_without_the_detector_is_never_offered_the_row(self) -> None:
        reason = catalog_can_run(
            {"engines": ["hayai-nova"], "detectors": ["ppocr-manga"], "devices": []},
            _gens(PRIMARY, HAYAI)[1],
        )
        assert reason is not None and "ctd" in reason

    def test_a_processor_without_the_engine_is_never_offered_the_row(self) -> None:
        reason = catalog_can_run(
            {"engines": ["hayai-nova"], "detectors": ["ppocr-manga"], "devices": []},
            _gens(PRIMARY, PADDLE)[1],
        )
        assert reason is not None and "paddle-manga" in reason

    def test_an_engines_own_detector_is_checked_too(self) -> None:
        """`ppocr-manga` brings its own detector, so the row never NAMES it --
        and a processor without it installed would fail minutes later."""
        row = _gens(PRIMARY, {"name": "pp", "engine": "ppocr-manga"})[1]
        assert row.detector_locked is True
        reason = catalog_can_run(
            {"engines": ["ppocr-manga"], "detectors": ["ctd"], "devices": []}, row
        )
        assert reason is not None and "ppocr-manga" in reason
        assert catalog_can_run(
            {"engines": ["ppocr-manga"], "detectors": ["ppocr-manga"], "devices": []},
            row,
        ) is None

    def test_a_row_pinned_to_a_card_this_processor_has_not_got_is_refused(self) -> None:
        row = _gens(PRIMARY, {**HAYAI, "pools": {"stage_device": {"engine": "gpu:1"}}})[1]
        reason = catalog_can_run(
            {"engines": ["hayai-nova"], "detectors": ["ctd"],
             "devices": [{"id": "gpu:0", "label": "GPU 0"}]},
            row,
        )
        assert reason is not None and "gpu:1" in reason

    def test_a_processor_that_has_everything_can_run_it(self) -> None:
        row = _gens(PRIMARY, {**HAYAI, "pools": {"stage_device": {"engine": "gpu:0"}}})[1]
        assert catalog_can_run(
            {"engines": ["hayai-nova"], "detectors": ["ctd"],
             "devices": [{"id": "gpu:0", "label": "GPU 0"}]},
            row,
        ) is None

    def test_an_empty_catalog_can_run_nothing(self) -> None:
        assert catalog_can_run(
            {"engines": [], "detectors": [], "devices": []}, _gens(PRIMARY, HAYAI)[1]
        )

    def test_a_mokuro_row_needs_the_processor_to_serve_mokuro(self) -> None:
        row = _gens(PRIMARY)[0]
        assert catalog_can_run(
            {"engines": ["mokuro"], "detectors": [], "devices": [],
             "serves_mokuro": False}, row
        ) is not None
        assert catalog_can_run(
            {"engines": ["mokuro"], "detectors": [], "devices": [],
             "serves_mokuro": True}, row
        ) is None


class TestAnOpenSessionDoesNotJumpTheQueue:
    """Spec section 3 rule 1 is a session's OWN top-up (`claim_for_session`).

    Each session runs its own runner, so a row another slot has open is not
    loaded for THIS slot: letting it jump row order bought nothing and, with
    pre-emption, could keep the earlier row from ever starting -- two slots
    taking turns opening a session for the later row, each pre-empted after
    two volumes, each re-opened while the other was still open.
    """

    def test_a_free_slot_takes_the_earlier_row_over_a_row_open_on_its_sibling(
        self, storage: Path
    ) -> None:
        # Alpha has its primary layer, so only the second row is pending there;
        # Beta is a new upload, so the FIRST row is pending for it.
        _library(storage, Alpha=["Volume 1", "Volume 2", "Volume 3"])
        beta = storage / "library" / "Beta" / "Volume 1.cbz"
        beta.parent.mkdir(parents=True)
        with zipfile.ZipFile(beta, "w") as zf:
            zf.writestr("page_000.jpg", b"fake image data")
        registry = ProcessorRegistry()
        entry = _connect(registry, sessions=2)
        worker = _make_worker(storage, registry)
        primary, secondary = worker.generations
        slot_a, slot_b = worker._all_slots()
        worker._active_slots = [slot_a, slot_b]
        # Slot B is still in a session for the second row (draining or not:
        # either way it is B's runner, not A's).
        open_on_b = type("Open", (), {"entry": entry, "generation": secondary})()
        worker._open_sessions.add(open_on_b)
        slot_b.session = open_on_b

        assert worker.claim_next(slot_a) == (beta, primary.id), (
            "row order decides what a new session starts"
        )

    def test_with_nothing_earlier_a_free_slot_joins_the_open_row(self, storage: Path) -> None:
        """Rule 3 still holds: the open row is simply next by order."""
        _library(storage, Alpha=["Volume 1", "Volume 2"])
        registry = ProcessorRegistry()
        entry = _connect(registry, sessions=2)
        worker = _make_worker(storage, registry)
        secondary = worker.generations[1]
        slot_a, slot_b = worker._all_slots()
        worker._active_slots = [slot_a, slot_b]
        open_on_b = type("Open", (), {"entry": entry, "generation": secondary})()
        worker._open_sessions.add(open_on_b)
        slot_b.session = open_on_b

        job = worker.claim_next(slot_a)
        assert job is not None and job[1] == secondary.id


def _make_worker(
    storage: Path,
    registry: ProcessorRegistry,
    *,
    local: bool = False,
    sessions: bool = True,
) -> OCRWorker:
    return OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=_gens(PRIMARY, HAYAI),
        engines_python_path=Path(sys.executable),
        concurrency=1,
        sessions=sessions,
        remote=registry,
        local_processing=local,
    )


def _connect(registry: ProcessorRegistry, *, name: str = "tower",
             catalog: dict[str, Any] | None = None, sessions: int = 1) -> Any:
    entry = registry.register(
        username=name, name=name, host={"gpu": "RTX 4090"},
        catalog=catalog if catalog is not None else FULL, max_sessions=sessions,
    )
    entry.stream_open = True
    return entry


class TestTheWorkerWithProcessors:
    @staticmethod
    def _worker(storage: Path, registry: ProcessorRegistry, *,
                local: bool = False) -> OCRWorker:
        return OCRWorker(
            storage_path=storage,
            poll_interval=30.0,
            generations=_gens(PRIMARY, HAYAI),
            engines_python_path=Path(sys.executable),
            concurrency=1,
            sessions=True,
            remote=registry,
            local_processing=local,
        )

    @staticmethod
    def _connected(registry: ProcessorRegistry, *, name: str = "tower",
                   catalog: dict[str, Any] | None = None, sessions: int = 1) -> Any:
        entry = registry.register(
            username=name, name=name, host={"gpu": "RTX 4090"},
            catalog=catalog if catalog is not None else FULL, max_sessions=sessions,
        )
        entry.stream_open = True
        return entry

    def test_with_local_processing_off_and_nobody_connected_there_are_no_slots(
        self, storage: Path
    ) -> None:
        worker = self._worker(storage, ProcessorRegistry())
        assert worker._all_slots() == []
        hold = worker.processing_hold()
        assert hold is not None and hold["reason"] == "no-processor"

    def test_a_connected_processor_contributes_its_max_sessions_as_slots(
        self, storage: Path
    ) -> None:
        registry = ProcessorRegistry()
        entry = self._connected(registry, sessions=2)
        worker = self._worker(storage, registry)
        slots = worker._all_slots()
        assert len(slots) == 2
        assert all(slot.processor.entry is entry for slot in slots)
        assert {slot.processor_id for slot in slots} == {entry.processor_id}
        assert worker.processing_hold() is None

    def test_local_hardware_adds_its_own_slots_when_it_is_on(
        self, storage: Path
    ) -> None:
        worker = self._worker(storage, ProcessorRegistry(), local=True)
        slots = worker._all_slots()
        assert len(slots) == 1
        assert slots[0].processor_id == "local"
        assert worker.processing_hold() is None

    def test_a_row_a_processor_cannot_run_is_never_claimed_for_its_slot(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()
        self._connected(
            registry, name="box",
            catalog={"engines": ["hayai-nova"], "detectors": ["ppocr-manga"],
                     "devices": [], "serves_mokuro": False},
        )
        worker = self._worker(storage, registry)
        slot = worker._all_slots()[0]
        assert worker.claim_next(slot) is None, "neither row matches this catalog"

    def test_a_disconnect_returns_a_claim_taken_before_any_session_opened(
        self, storage: Path
    ) -> None:
        """The claim is the SLOT's, not a session's: a processor that drops
        between claiming and opening must still give it back."""
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()
        entry = self._connected(registry)
        worker = self._worker(storage, registry)
        registry.on_drop = worker.processor_disconnected
        slot = worker._all_slots()[0]
        worker._active_slots = [slot]
        job = worker.claim_next(slot)
        assert job is not None and job in worker._inflight_ocr

        registry.drop(entry.processor_id, "stream closed")
        assert worker._inflight_ocr == set(), "the claim came back"
        assert not (storage / ".ocr-failures.json").exists(), (
            "a disconnect is nobody's failure"
        )

    def test_the_hold_names_who_left_and_when(self, storage: Path) -> None:
        registry = ProcessorRegistry()
        entry = self._connected(registry)
        worker = self._worker(storage, registry)
        registry.drop(entry.processor_id, "stream closed")
        hold = worker.processing_hold()
        assert hold is not None
        assert hold["reason"] == "no-processor"
        assert hold["last"]["name"] == "tower"
        assert hold["last"]["disconnected_at"] > 0

    def test_a_remote_slot_never_takes_the_per_volume_path(
        self, storage: Path
    ) -> None:
        registry = ProcessorRegistry()
        self._connected(registry)
        worker = self._worker(storage, registry)
        slot = worker._all_slots()[0]
        for row in worker.generations:
            assert slot.processor.runs_mokuro_cli(row) is False
            assert worker._session_row(row, slot) is True


class TestWhatALeavingProcessorLeavesBehind:
    """Spec section 3 rule 4, and the ledger's late-duplicate carry: a claim
    that came back is somebody else's the moment it is re-offered, so any
    result the old slot still settles for it must change nothing."""

    def test_a_dropped_processors_slot_claims_nothing(self, storage: Path) -> None:
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _make_worker(storage, registry)
        slot = worker._all_slots()[0]
        registry.drop(entry.processor_id, "stream closed")
        assert worker.claim_next(slot) is None
        assert worker._inflight_ocr == set()

    def test_a_late_failure_for_a_returned_claim_is_ignored(self, storage: Path) -> None:
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()
        gone = _connect(registry, name="tower")
        worker = _make_worker(storage, registry)
        registry.on_drop = worker.processor_disconnected
        old = worker._all_slots()[0]
        job = worker.claim_next(old)
        assert job is not None
        row = worker._generation(job[1])
        assert row is not None
        registry.drop(gone.processor_id, "stream closed")

        _connect(registry, name="box")
        new = worker._all_slots()[0]
        assert worker.claim_next(new) == job, "the returned claim is re-offered"

        # The dropped slot's thread settles late. It must neither release the
        # new owner's claim nor blame the volume.
        worker.finish_ocr_job(job, row, ok=False, slot=old)
        worker.release_ocr_job(job, row, reason="late", slot=old)
        assert job in worker._inflight_ocr, "the new owner still holds it"
        assert not (storage / ".ocr-failures.json").exists()

        worker.finish_ocr_job(job, row, ok=False, slot=new)
        assert (storage / ".ocr-failures.json").exists(), (
            "the new owner's own failure is still recorded"
        )
        assert job not in worker._inflight_ocr

    def test_a_failure_in_the_window_before_the_listener_is_not_recorded(
        self, storage: Path
    ) -> None:
        """`drop` marks the entry gone BEFORE its listener runs; a session
        start refused in between must not become the volume's failure."""
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()  # no on_drop: the listener never runs
        entry = _connect(registry)
        worker = _make_worker(storage, registry)
        slot = worker._all_slots()[0]
        job = worker.claim_next(slot)
        assert job is not None
        row = worker._generation(job[1])
        assert row is not None
        registry.drop(entry.processor_id, "stream closed")

        worker.finish_ocr_job(job, row, ok=False, slot=slot)
        assert not (storage / ".ocr-failures.json").exists()
        assert worker._inflight_ocr == set()
        assert job not in worker._attempted_ocr, "it may be retried this scan"

    def test_a_late_sidecar_is_never_installed(self, storage: Path) -> None:
        """A `volume_done` queued before the drop and read after it: the
        claim is no longer this slot's, so its file must not land beside the
        one the next owner will write (it would be filed as a duplicate)."""
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _make_worker(storage, registry)
        registry.on_drop = worker.processor_disconnected
        slot = worker._all_slots()[0]
        job = worker.claim_next(slot)
        assert job is not None
        row = worker._generation(job[1])
        assert row is not None
        volume = slot.processor.prepare_session_volume(job[0], row, "v1")
        volume.output.write_text(
            json.dumps({"version": "0.0", "pages": [], "chars": 0}), encoding="utf-8"
        )
        pending = _SessionJob(job=job, generation=row, volume=volume, slot=slot.index,
                              owner=slot)

        registry.drop(entry.processor_id, "stream closed")
        assert worker._collect_session_volume(pending, {"event": "volume_done"}) is False
        assert not list((storage / "library").rglob("*.hayai-ctd.mokuro"))
        assert not volume.workspace.exists()
        assert not (storage / ".ocr-failures.json").exists()


class TestTheScanWithProcessors:
    """The worker's own loop against a processor that never answers: what the
    wire carries, and what a disconnect, a benchmark and a newcomer do."""

    @staticmethod
    def _scan(worker: OCRWorker) -> threading.Thread:
        thread = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        thread.start()
        return thread

    def test_a_session_on_a_processor_that_leaves_ends_now_and_blames_nothing(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha=["Volume 1", "Volume 2"])
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _make_worker(storage, registry)
        registry.on_drop = worker.processor_disconnected
        thread = self._scan(worker)
        try:
            assert _wait(lambda: len(worker._inflight_ocr) == 2), "both volumes went"
            assert _wait(lambda: entry.ops.qsize() >= 3)
            ops = _ops(entry)
            assert [op["op"] for op in ops] == ["open_session", "volume", "volume"]
            assert ops[1]["archive"] == "/mokuro-reader/Alpha/Volume 1.cbz"
            assert _wait(lambda: len(_progress_jobs(storage)) == 2)
            assert {job["processor"] for job in _progress_jobs(storage)} == {
                "tower (RTX 4090)"
            }

            registry.drop(entry.processor_id, "stream closed")
            assert worker._inflight_ocr == set(), "returned at once, not at the next poll"
            thread.join(timeout=10)
            assert not thread.is_alive(), "the session ended and the scan with it"
            assert not (storage / ".ocr-failures.json").exists()
            assert worker._session_strikes == {}, "a processor leaving is not a crash"
            assert worker._open_sessions == set()
        finally:
            worker._stop_requested = True
            thread.join(timeout=10)

    def test_preempting_for_a_benchmark_on_a_processor_reaches_its_session(
        self, storage: Path
    ) -> None:
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _make_worker(storage, registry)
        registry.on_drop = worker.processor_disconnected
        thread = self._scan(worker)
        try:
            assert _wait(lambda: bool(worker._inflight_ocr))
            quiet, preempted = worker.preempt_for_bench(timeout=10.0, processor="tower")
            assert quiet is True
            assert preempted == [{"generation": "hayai-ctd", "volume": "Volume 1"}]
            kinds = [op["op"] for op in _ops(entry)]
            assert "cancel" in kinds and "close_session" in kinds
            assert not (storage / ".ocr-failures.json").exists()
        finally:
            worker.release_queue(processor="tower")
            worker._stop_requested = True
            thread.join(timeout=10)

    def test_a_benchmark_on_this_server_leaves_a_processor_working(
        self, storage: Path
    ) -> None:
        """Spec section 3 rule 5: the processor the bench is FOR is pre-empted.
        A benchmark of this box's own hardware must not cancel, hold or strike
        a 4090 in another room -- it measures nothing about it."""
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()
        entry = _connect(registry)
        worker = _make_worker(storage, registry, local=True)
        registry.on_drop = worker.processor_disconnected
        # Only the processor can take the volume: this box has no slot free.
        worker._slots = []
        thread = self._scan(worker)
        try:
            assert _wait(lambda: bool(worker._inflight_ocr))
            claimed = set(worker._inflight_ocr)
            quiet, preempted = worker.preempt_for_bench(timeout=10.0)
            assert quiet is True, "nothing of this server's was running"
            assert preempted == []
            assert worker._inflight_ocr == claimed, "the processor kept its claim"
            assert worker.hardware_held("local") is True
            assert worker.hardware_held("tower") is False
            kinds = [op["op"] for op in _ops(entry)]
            assert "cancel" not in kinds and "close_session" not in kinds, kinds
        finally:
            worker.release_queue()
            worker._stop_requested = True
            thread.join(timeout=10)

    def test_a_processor_that_connects_mid_scan_is_given_slots(
        self, storage: Path
    ) -> None:
        """Load balancing is however many are logged in -- not however many
        were logged in when the scan began."""
        _library(storage, Alpha=["Volume 1", "Volume 2", "Volume 3"])
        registry = ProcessorRegistry()
        first = _connect(registry, name="tower")
        worker = _make_worker(storage, registry)
        registry.on_drop = worker.processor_disconnected
        thread = self._scan(worker)
        try:
            assert _wait(lambda: len(worker._inflight_ocr) == 2), "tower took its two"
            second = _connect(registry, name="box")
            assert _wait(lambda: len(worker._inflight_ocr) == 3), "box took the third"
            assert _wait(lambda: second.ops.qsize() >= 2), "and was sent it"
            assert [op["op"] for op in _ops(second)][:2] == ["open_session", "volume"]
            registry.drop(first.processor_id, "stream closed")
            registry.drop(second.processor_id, "stream closed")
            thread.join(timeout=10)
            assert not thread.is_alive()
            assert not (storage / ".ocr-failures.json").exists()
        finally:
            worker._stop_requested = True
            thread.join(timeout=10)


class TestTheQueueWithoutLocalHardware:
    def test_with_sessions_off_a_remote_slot_still_runs_a_session(
        self, storage: Path
    ) -> None:
        """`ocr.sessions: false` is THIS machine's fallback. A processor has
        no per-volume road here -- the old path would run the OCR locally."""
        registry = ProcessorRegistry()
        _connect(registry)
        worker = _make_worker(storage, registry, local=True, sessions=False)
        local, remote = worker._all_slots()
        row = worker.generations[1]
        assert worker._session_row(row, local) is False
        assert worker._session_row(row, remote) is True

    def test_the_queue_page_can_be_priced_with_no_local_slot(self, storage: Path) -> None:
        _library(storage, Alpha=["Volume 1"])
        registry = ProcessorRegistry()
        _connect(registry, sessions=3)
        worker = _make_worker(storage, registry)
        pending = worker.pending_jobs()
        assert [job["volume"] for job in pending] == ["Volume 1"]
        worker.queue_plan([], pending)  # must not index a slot that is not there
        assert worker._lane_count() == 3


class TestArchivesRoot:
    def test_the_mount_and_the_ops_share_one_configured_root(self, storage: Path) -> None:
        from mokuro_bunko.ocr.remote.library_api import ProcessorAPI

        registry = ProcessorRegistry()
        api = ProcessorAPI(lambda environ, start: [], registry, archives_root="manga")
        assert api.archives_root == registry.archives_root == "/manga/"
        entry = _connect(registry)
        worker = _make_worker(storage, registry)
        slot = worker._all_slots()[0]
        session = slot.processor.open_session(worker.generations[1], storage / "s.log")
        assert session.archives_root == "/manga/"
        assert session.entry is entry


class TestTheSurfaces:
    def test_config_carries_local_processing(self) -> None:
        from mokuro_bunko.config import _CONFIG_TYPES, Config, set_by_dotted_key

        config = Config()
        assert config.to_dict()["ocr"]["local_processing"] is True
        assert _CONFIG_TYPES["ocr.local_processing"] is bool
        set_by_dotted_key(config, "ocr.local_processing", "false")
        assert config.ocr.local_processing is False

    def test_control_passes_the_processors_and_the_hold_through(
        self, storage: Path
    ) -> None:
        from mokuro_bunko.ocr.control import OcrControl

        control = OcrControl()
        assert control.processors() == []
        assert control.processing_hold() is None
        registry = ProcessorRegistry()
        _connect(registry)
        control.remote = registry
        control.worker = _make_worker(storage, registry)
        assert [row["name"] for row in control.processors()] == ["tower"]
        assert control.processing_hold() is None
        registry.drop(registry.connected()[0].processor_id, "gone")
        hold = control.processing_hold()
        assert hold is not None and hold["last"]["name"] == "tower"

    def test_the_status_carries_the_hold_and_each_jobs_processor(
        self, storage: Path
    ) -> None:
        from mokuro_bunko.ocr.control import OcrControl
        from mokuro_bunko.queue.api import QueueAPI

        (storage / ".ocr-progress.json").write_text(
            json.dumps({"active": True, "series": "S", "volume": "V",
                        "processor": "tower (RTX 4090)"}),
            encoding="utf-8",
        )
        queue_cfg = type("Cfg", (), {"show_in_nav": False, "public_access": True})()
        control = OcrControl()
        control.worker = _make_worker(storage, ProcessorRegistry())
        data = _status(QueueAPI(_not_found, storage_base_path=str(storage),
                                queue_config=queue_cfg, ocr_control=control))
        assert data["processing_hold"]["reason"] == "no-processor"
        assert data["current_jobs"][0]["processor"] == "tower (RTX 4090)"

        bare = _status(QueueAPI(_not_found, storage_base_path=str(storage),
                                queue_config=queue_cfg))
        assert bare["processing_hold"] is None


def _not_found(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b""]


def _status(app: Callable[..., Any]) -> dict[str, Any]:
    """``GET /queue/api/status`` against the app, decoded."""
    environ = {
        "REQUEST_METHOD": "GET", "PATH_INFO": "/queue/api/status", "QUERY_STRING": "",
        "SERVER_NAME": "localhost", "SERVER_PORT": "8080",
        "SERVER_PROTOCOL": "HTTP/1.1", "wsgi.input": io.BytesIO(b""),
        "wsgi.errors": io.StringIO(), "wsgi.url_scheme": "http",
    }
    status: list[str] = []
    b"".join(app(environ, lambda code, headers, *_: status.append(code)))
    assert status and status[0].startswith("200"), status
    # The endpoint sends a shaped, per-level payload (`queue.shape`); these
    # tests are about the model underneath it, which `raw_status` returns.
    parsed: dict[str, Any] = app.raw_status()  # type: ignore[attr-defined]
    return parsed
