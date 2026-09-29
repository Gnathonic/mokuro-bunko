"""An OCR result is never written beside an archive that is gone or replaced.

Live: a `.cbz` was deleted over WebDAV while its OCR was queued or
running, and the paddle-manga job still wrote
`<Volume>.paddle-manga-ppocr-manga.mokuro` beside the missing archive -- an
orphan the delete's own sidecar cascade had already swept past.

Two defences, both tested here:
* just before a sidecar is written -- by a local run or delivered by a
  session (local or a processor's) -- the archive must still be the file
  the job was claimed from (size and mtime); otherwise the result is
  discarded, nothing is recorded as failed, and the job goes back to the
  queue (where a deleted archive simply has no jobs);
* a WebDAV DELETE or MOVE away of a `.cbz` drops its queued jobs and cancels
  its running ones with the existing no-failure cancellation.
"""

from __future__ import annotations

import base64
import json
import os
import zipfile
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.processor import MokuroRunResult
from mokuro_bunko.ocr.session import SessionVolume
from mokuro_bunko.ocr.watcher import OCRWorker, _SessionJob
from mokuro_bunko.server import create_app
from tests.unit.test_upload_enqueue import call

ROWS = parse_generation_list(
    [
        {"name": "mokuro", "engine": "mokuro", "primary": True},
        {"name": "paddle-manga-ppocr-manga", "engine": "paddle-manga", "detector": "ppocr-manga"},
    ]
)
EDITOR = {"Authorization": "Basic " + base64.b64encode(b"editor:pass1234").decode()}


def make_cbz(path: Path, pages: int = 2) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as archive:
        for index in range(pages):
            archive.writestr(f"{index:03}.jpg", b"not really a jpeg %d" % index)
    return path


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library" / "S").mkdir(parents=True)
    (tmp_path / "inbox").mkdir()
    (tmp_path / "users").mkdir()
    make_cbz(tmp_path / "library" / "S" / "V1.cbz")
    # The primary is done: the layer is what is owed.
    (tmp_path / "library" / "S" / "V1.mokuro").write_text(
        json.dumps({"volume_uuid": "u-1", "pages": []}), encoding="utf-8"
    )
    return tmp_path


def make_worker(storage: Path) -> tuple[OCRWorker, list[str]]:
    said: list[str] = []
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=3600.0,
        generations=ROWS,
        engines_python_path=Path("/nonexistent"),
        sessions=False,
        status_callback=said.append,
    )
    return worker, said


def failures(storage: Path) -> dict[str, Any]:
    path = storage / ".ocr-failures.json"
    return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}


def fake_engine(worker: OCRWorker, monkeypatch: pytest.MonkeyPatch, during: Any) -> None:
    """The engine writes its sidecar -- after ``during()`` has changed the library."""
    processor = worker.processor

    def run(
        generation: GenerationSpec,
        input_path: Path,
        output_dir: Path,
        total_images: int = 0,
        source_cbz: Path | None = None,
    ) -> MokuroRunResult:
        during()
        stem = input_path.stem if input_path.is_file() else input_path.name
        (output_dir / f"{stem}{generation.sidecar_suffix}").write_text(
            json.dumps({"pages": []}), encoding="utf-8"
        )
        return MokuroRunResult(True, None, None)

    monkeypatch.setattr(processor, "_run_engine", run)


def layer_path(storage: Path) -> Path:
    return storage / "library" / "S" / "V1.paddle-manga-ppocr-manga.mokuro"


class TestLocalRun:
    def test_deleted_while_running_writes_nothing(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker, said = make_worker(storage)
        cbz = storage / "library" / "S" / "V1.cbz"
        fake_engine(worker, monkeypatch, cbz.unlink)

        worker._scan_ocr_once()

        assert not layer_path(storage).exists()
        assert list((storage / "library" / "S").glob("*.paddle*")) == []
        assert failures(storage) == {}
        discarded = [line for line in said if "discarded" in line]
        assert len(discarded) == 1, said
        assert worker._inflight_ocr == set()

    def test_replaced_while_running_is_rerun_on_the_new_file(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker, said = make_worker(storage)
        cbz = storage / "library" / "S" / "V1.cbz"

        def replace() -> None:
            make_cbz(cbz, pages=5)
            os.utime(cbz, ns=(1, 1))

        fake_engine(worker, monkeypatch, replace)
        # One job only: the run whose archive was replaced is discarded.
        slot = worker._slots[0]
        job, _ = worker._claim(slot, None)
        assert job is not None
        worker._run_ocr_job(job, slot)

        assert not layer_path(storage).exists()
        assert failures(storage) == {}
        assert [j["generation"] for j in worker.pending_jobs(max_age=0)] == [
            "paddle-manga-ppocr-manga"
        ]

    def test_an_unchanged_archive_is_written(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker, _ = make_worker(storage)
        fake_engine(worker, monkeypatch, lambda: None)
        worker._scan_ocr_once()
        assert layer_path(storage).exists()


class TestSessionDelivery:
    """A session's (or a remote processor's) finished volume, as it is collected."""

    def entry(self, storage: Path, worker: OCRWorker) -> _SessionJob:
        cbz = storage / "library" / "S" / "V1.cbz"
        job = (cbz, ROWS[1].id)
        with worker._lock:
            worker._inflight_ocr.add(job)
            worker._note_claim(job)
        out = storage / "ws" / "V1.paddle-manga-ppocr-manga.mokuro"
        out.parent.mkdir()
        out.write_text(json.dumps({"pages": []}), encoding="utf-8")
        return _SessionJob(
            job=job,
            generation=ROWS[1],
            volume=SessionVolume(
                id="v1", workspace=storage / "ws", output=out,
                cache_dir=storage / "ws" / "c", detect_dir=storage / "ws" / "d",
                log=storage / "ws" / "v.log", title="S", volume="V1",
            ),
        )

    def done(self) -> dict[str, Any]:
        return {"event": "volume_done", "id": "v1", "pages": 2, "seconds": 1.0}

    def test_a_deleted_archive_gets_no_sidecar(self, storage: Path) -> None:
        worker, said = make_worker(storage)
        entry = self.entry(storage, worker)
        (storage / "library" / "S" / "V1.cbz").unlink()

        assert worker._collect_session_volume(entry, self.done()) is False

        assert not layer_path(storage).exists()
        assert failures(storage) == {}
        assert worker._inflight_ocr == set()
        assert len([line for line in said if "discarded" in line]) == 1

    def test_a_replaced_archive_gets_no_sidecar(self, storage: Path) -> None:
        worker, _ = make_worker(storage)
        entry = self.entry(storage, worker)
        cbz = storage / "library" / "S" / "V1.cbz"
        make_cbz(cbz, pages=7)
        os.utime(cbz, ns=(1, 1))

        assert worker._collect_session_volume(entry, self.done()) is False
        assert not layer_path(storage).exists()
        assert failures(storage) == {}

    def test_replaced_before_a_processor_fetched_it_is_written(self, storage: Path) -> None:
        """The fetch took the NEW file: its result belongs beside it."""
        worker, _ = make_worker(storage)
        entry = self.entry(storage, worker)
        cbz = storage / "library" / "S" / "V1.cbz"
        make_cbz(cbz, pages=7)
        os.utime(cbz, ns=(1, 1))
        worker._download_delivered(entry, {"event": "fetch", "state": "ready"})

        assert worker._collect_session_volume(entry, self.done()) is True
        assert layer_path(storage).exists()

    def test_the_same_archive_gets_its_sidecar(self, storage: Path) -> None:
        worker, _ = make_worker(storage)
        entry = self.entry(storage, worker)
        assert worker._collect_session_volume(entry, self.done()) is True
        assert layer_path(storage).exists()


class TestDeleteCancels:
    @pytest.fixture
    def setup(self, storage: Path) -> tuple[Any, OCRWorker, Path]:
        make_cbz(storage / "library" / "S" / "V2.cbz")
        (storage / "library" / "S" / "V2.mokuro").write_text("{}", encoding="utf-8")
        db = Database(storage / "mokuro.db")
        db.create_user("editor", "pass1234", "editor")
        worker, _ = make_worker(storage)
        control = OcrControl()
        control.worker = worker
        app = create_app(Config(storage=StorageConfig(base_path=storage)), ocr_control=control)
        return app, worker, storage / "library" / "S"

    def run_on_slot(self, worker: OCRWorker, cbz: Path) -> list[str]:
        """Make V1's layer job the slot's running job; record cancels."""
        slot = worker._slots[0]
        job = (cbz, ROWS[1].id)
        with worker._lock:
            worker._inflight_ocr.add(job)
            worker._note_claim(job)
            worker._claim_owner[job] = slot
            slot.job, slot.generation = job, ROWS[1]
        cancels: list[str] = []
        slot.processor.cancel_active = lambda: cancels.append("killed") or True  # type: ignore[method-assign]
        return cancels

    def test_delete_cancels_the_running_job_without_a_failure(
        self, setup: tuple[Any, OCRWorker, Path]
    ) -> None:
        app, worker, series = setup
        cbz = series / "V1.cbz"
        cancels = self.run_on_slot(worker, cbz)

        status, _, _ = call(app, "DELETE", "/mokuro-reader/S/V1.cbz", headers=EDITOR)

        assert status == 204
        assert cancels == ["killed"]
        assert (cbz, ROWS[1].id) in worker._cancelled_ocr

    def test_delete_drops_the_queued_jobs(self, setup: tuple[Any, OCRWorker, Path]) -> None:
        app, worker, _ = setup
        before = {(j["volume"], j["generation"]) for j in worker.pending_jobs(max_age=0)}
        assert ("V2", "paddle-manga-ppocr-manga") in before
        worker._eligible_ocr_jobs = lambda *_a, **_k: pytest.fail("walked")  # type: ignore[method-assign]

        status, _, _ = call(app, "DELETE", "/mokuro-reader/S/V2.cbz", headers=EDITOR)

        assert status == 204
        after = {(j["volume"], j["generation"]) for j in worker.last_pending()}
        assert ("V2", "paddle-manga-ppocr-manga") not in after
        assert ("V1", "paddle-manga-ppocr-manga") in after

    def test_move_away_cancels_too(self, setup: tuple[Any, OCRWorker, Path]) -> None:
        app, worker, series = setup
        cancels = self.run_on_slot(worker, series / "V1.cbz")

        status, _, _ = call(app, "MOVE", "/mokuro-reader/S/V1.cbz", headers={
            **EDITOR, "Destination": "/mokuro-reader/S/V1 renamed.cbz",
        })

        assert status in (201, 204)
        assert cancels == ["killed"]

    def test_a_session_holding_other_volumes_is_not_killed(
        self, setup: tuple[Any, OCRWorker, Path]
    ) -> None:
        """Killing it would throw away the others' work; the result is discarded at collection."""
        app, worker, series = setup
        slot = worker._slots[0]
        kills: list[str] = []

        class Session:
            def kill(self) -> bool:
                kills.append("session")
                return True

        mine = (series / "V1.cbz", ROWS[1].id)
        other = (series / "V2.cbz", ROWS[1].id)
        with worker._lock:
            for job in (mine, other):
                worker._inflight_ocr.add(job)
                worker._note_claim(job)
                worker._claim_owner[job] = slot
            slot.session, slot.generation = Session(), ROWS[1]  # type: ignore[assignment]

        status, _, _ = call(app, "DELETE", "/mokuro-reader/S/V1.cbz", headers=EDITOR)

        assert status == 204
        assert kills == []
        assert mine in worker._cancelled_ocr
        assert other not in worker._cancelled_ocr

    def test_a_session_holding_only_this_volume_is_killed(
        self, setup: tuple[Any, OCRWorker, Path]
    ) -> None:
        app, worker, series = setup
        slot = worker._slots[0]
        kills: list[str] = []

        class Session:
            def kill(self) -> bool:
                kills.append("session")
                return True

        mine = (series / "V1.cbz", ROWS[1].id)
        with worker._lock:
            worker._inflight_ocr.add(mine)
            worker._note_claim(mine)
            worker._claim_owner[mine] = slot
            slot.session, slot.generation = Session(), ROWS[1]  # type: ignore[assignment]

        call(app, "DELETE", "/mokuro-reader/S/V1.cbz", headers=EDITOR)

        assert kills == ["session"]
        assert mine in worker._cancelled_ocr

    def test_deleting_the_series_folder_cancels_its_volumes(
        self, setup: tuple[Any, OCRWorker, Path]
    ) -> None:
        app, worker, series = setup
        cancels = self.run_on_slot(worker, series / "V1.cbz")
        worker.pending_jobs(max_age=0)

        status, _, _ = call(app, "DELETE", "/mokuro-reader/S", headers=EDITOR)

        assert status == 204
        assert cancels == ["killed"]
        assert [j for j in worker.last_pending() if j["series"] == "S"] == []
