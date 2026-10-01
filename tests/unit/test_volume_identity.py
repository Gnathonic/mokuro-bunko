"""A primary `.mokuro` made again keeps the ``volume_uuid`` of the one it replaces.

Read progress, stats and every reader's synced `volume-data.json` are keyed by
``volume_uuid``, and a reader that upgrades a volume's OCR keeps its LOCAL id
(it matches by title). So if the server's regenerated primary names the volume
by a new id, every device installed before the re-OCR keeps the old one and
every device installing after gets the new one: one person's progress splits
across their devices for good, and shelf offsets published against the old id
stop applying.

The server only ever makes a primary that is MISSING (`needs_sidecar`), so by
the time it runs the old file is gone -- deleted over WebDAV (the "re-OCR this
volume" gesture), or removed on disk. Its id therefore has to be remembered
before that: `Database.remember_volume_uuid`, written whenever the metadata pass
compiles a volume from its `.mokuro` (the id the index publishes) and when a
primary is deleted over WebDAV, and read by `OCRProcessor.volume_uuid_for`.

The memory belongs to the ARCHIVE PATH, and goes with what goes with the
sidecars: deleting the archive takes its sidecars and the memory with it (a
new upload under that name starts fresh); a PUT over the archive leaves its
sidecars, so the memory stays (readers match by title and already hold that
id); a folder move takes both along.
"""

from __future__ import annotations

import base64
import json
import sqlite3
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.metadata.compiler import SeriesFolder, compile_series_volumes
from mokuro_bunko.metadata.reader_compat import deterministic_uuid
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.server import create_app
from tests.unit.test_parallel_generations import (
    _cbz,
    _fake_run,
    _gens,
    _mokuro,
    _pages,
    _read,
)
from tests.unit.test_upload_enqueue import call, cbz_bytes

UPLOADER = "Basic " + base64.b64encode(b"uploader:pass1234").decode()
ADMIN = "Basic " + base64.b64encode(b"admin:pass1234").decode()
UPLOADED = "the-uploaders-uuid"


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    for name in ("library", "inbox", "users"):
        (base / name).mkdir(parents=True)
    db = Database(base / "mokuro.db")
    db.create_user("uploader", "pass1234", "uploader")
    db.create_user("admin", "pass1234", "admin")
    return base


def _worker(storage: Path) -> OCRWorker:
    return OCRWorker(
        storage_path=storage,
        poll_interval=3600.0,
        generations=_gens(),
        engines_python_path=Path("/nonexistent"),
        sessions=False,
        page_count_lookup=lambda _path: 3,
        database=Database(storage / "mokuro.db"),
    )


def _app(storage: Path, worker: OCRWorker) -> Any:
    control = OcrControl()
    control.worker = worker
    return create_app(Config(storage=StorageConfig(base_path=storage)), ocr_control=control)


def _runs(worker: OCRWorker, monkeypatch: pytest.MonkeyPatch) -> None:
    """Every row's run mints a random id of its own, as mokuro and the runner do."""
    _fake_run(worker.processor, monkeypatch, {
        "mokuro": {"volume_uuid": "random-from-mokuro", "pages": _pages(3)},
        "nova": {"volume_uuid": "random-from-nova", "pages": _pages(3)},
        "paddle": {"volume_uuid": "random-from-paddle", "pages": _pages(3)},
    })


def _uploaded(storage: Path, app: Any, series: str = "S", volume: str = "V1") -> Path:
    """A volume uploaded WITH its `.mokuro` (mokuro's own random id), no layers."""
    (storage / "library" / series).mkdir(exist_ok=True)
    status, _, _ = call(app, "PUT", f"/mokuro-reader/{series}/{volume}.cbz", cbz_bytes(),
                        {"Authorization": UPLOADER})
    assert status == 201
    cbz = storage / "library" / series / f"{volume}.cbz"
    sidecar = _mokuro(cbz, pages=3, uuid=UPLOADED)
    body = sidecar.read_bytes()
    sidecar.unlink()
    status, _, _ = call(app, "PUT", f"/mokuro-reader/{series}/{volume}.mokuro", body,
                        {"Authorization": UPLOADER})
    assert status == 201
    assert _read(sidecar)["volume_uuid"] == UPLOADED
    return cbz


def _delete(app: Any, path: str, who: str = UPLOADER) -> None:
    status, _, _ = call(app, "DELETE", f"/mokuro-reader/{path}", headers={"Authorization": who})
    assert status in (200, 204), status


class TestReOcrOverWebDav:
    """The `.mokuro` is DELETEd over WebDAV and the server makes it again."""

    def test_the_regenerated_primary_keeps_the_uploaded_uuid(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker = _worker(storage)
        app = _app(storage, worker)
        cbz = _uploaded(storage, app)
        _delete(app, "S/V1.mokuro")
        assert not cbz.with_suffix(".mokuro").exists()

        _runs(worker, monkeypatch)
        mokuro, nova, paddle = worker.processor.generations
        assert worker.processor.process_library_ocr(cbz, mokuro)
        assert _read(cbz.with_suffix(".mokuro"))["volume_uuid"] == UPLOADED
        # ... and every layer made afterwards names the same volume.
        assert worker.processor.process_library_ocr(cbz, nova)
        assert _read(cbz.parent / "V1.nova.mokuro")["volume_uuid"] == UPLOADED

    def test_a_layer_landing_before_the_regenerated_primary_agrees(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker = _worker(storage)
        app = _app(storage, worker)
        cbz = _uploaded(storage, app)
        _delete(app, "S/V1.mokuro")

        _runs(worker, monkeypatch)
        mokuro, nova, _ = worker.processor.generations
        assert worker.processor.process_library_ocr(cbz, nova)
        assert worker.processor.process_library_ocr(cbz, mokuro)
        assert _read(cbz.parent / "V1.nova.mokuro")["volume_uuid"] == UPLOADED
        assert _read(cbz.with_suffix(".mokuro"))["volume_uuid"] == UPLOADED

    def test_the_session_install_path_keeps_it_too(self, storage: Path) -> None:
        """Sessions -- this server's slots and every connected processor's --
        are told the id when the volume is sent and stamp it on install."""
        worker = _worker(storage)
        app = _app(storage, worker)
        cbz = _uploaded(storage, app)
        _delete(app, "S/V1.mokuro")

        # A slot's own processor (a clone of the worker's), as sessions use.
        processor = worker._clone_processor()
        primary = processor.generations[0]
        volume = processor.prepare_session_volume(cbz, primary, "job-1")
        assert volume.volume_uuid == UPLOADED
        volume.output.write_text(
            json.dumps({"volume_uuid": "random-from-runner", "pages": _pages(3)}),
            encoding="utf-8",
        )
        assert processor.install_session_sidecar(cbz, primary, volume.output) is None
        assert _read(cbz.with_suffix(".mokuro"))["volume_uuid"] == UPLOADED

    def test_the_published_index_names_the_same_volume_after(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker = _worker(storage)
        app = _app(storage, worker)
        cbz = _uploaded(storage, app)
        db = Database(storage / "mokuro.db")
        folder = SeriesFolder(title="S", path=cbz.parent)
        (before,) = compile_series_volumes(folder, database=db)
        assert before.volume_uuid == UPLOADED

        _delete(app, "S/V1.mokuro")
        # The pass between the delete and the re-OCR publishes an image-only
        # volume, as it always did ...
        (between,) = compile_series_volumes(folder, database=db)
        assert between.volume_uuid == deterministic_uuid("S/V1")
        # ... which must not overwrite what the volume was known by.
        _runs(worker, monkeypatch)
        assert worker.processor.process_library_ocr(cbz, worker.processor.generations[0])
        (after,) = compile_series_volumes(folder, database=db)
        assert after.volume_uuid == UPLOADED


    def test_a_primary_moved_away_is_remembered_the_same_way(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker = _worker(storage)
        app = _app(storage, worker)
        cbz = _uploaded(storage, app)
        status, _, _ = call(app, "MOVE", "/mokuro-reader/S/V1.mokuro", headers={
            "Authorization": ADMIN, "Destination": "/mokuro-reader/S/V1.mokuro.bak",
        })
        assert status in (201, 204), status

        _runs(worker, monkeypatch)
        assert worker.processor.process_library_ocr(cbz, worker.processor.generations[0])
        assert _read(cbz.with_suffix(".mokuro"))["volume_uuid"] == UPLOADED


class TestRemovedOnDisk:
    """The `.mokuro` vanished without WebDAV (removed by hand, or as corrupt):
    the id the metadata pass last published from it is what it is known by."""

    def test_a_volume_the_pass_has_compiled_keeps_its_uuid(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        cbz = _cbz(storage / "library" / "S" / "V1.cbz")
        sidecar = _mokuro(cbz, pages=3, uuid=UPLOADED)
        compile_series_volumes(
            SeriesFolder(title="S", path=cbz.parent), database=Database(storage / "mokuro.db")
        )
        sidecar.unlink()

        worker = _worker(storage)
        _runs(worker, monkeypatch)
        assert worker.processor.process_library_ocr(cbz, worker.processor.generations[0])
        assert _read(sidecar)["volume_uuid"] == UPLOADED

    def test_a_database_from_before_remembers_what_its_cache_published(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """An existing library: the compiled-entry cache already holds every
        volume's published id, and the upgrade carries it over once."""
        cbz = _cbz(storage / "library" / "S" / "V1.cbz")
        sidecar = _mokuro(cbz, pages=3, uuid=UPLOADED)
        image_only = _cbz(storage / "library" / "S" / "V2.cbz")
        db_path = storage / "mokuro.db"
        compile_series_volumes(
            SeriesFolder(title="S", path=cbz.parent), database=Database(db_path)
        )
        with sqlite3.connect(db_path) as conn:
            conn.execute("DROP TABLE volume_identities")
            conn.execute("UPDATE schema_version SET version = 5")
        sidecar.unlink()

        db = Database(db_path)
        assert db.remembered_volume_uuid("S/V1.cbz") == UPLOADED
        # An image-only volume's id is derived, never remembered.
        assert db.remembered_volume_uuid("S/V2.cbz") is None
        worker = _worker(storage)
        _runs(worker, monkeypatch)
        assert worker.processor.process_library_ocr(cbz, worker.processor.generations[0])
        assert _read(sidecar)["volume_uuid"] == UPLOADED
        assert worker.processor.process_library_ocr(image_only, worker.processor.generations[0])
        assert _read(image_only.with_suffix(".mokuro"))["volume_uuid"] == deterministic_uuid(
            "S/V2"
        )


class TestWhatTheMemoryFollows:
    def test_an_archive_replaced_in_place_stays_the_same_volume(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A PUT over the `.cbz` leaves its sidecars: the old `.mokuro` is still
        the volume's OCR, readers still match it by title and hold its id, so
        a re-OCR of the new archive keeps that id."""
        worker = _worker(storage)
        app = _app(storage, worker)
        cbz = _uploaded(storage, app)
        status, _, _ = call(app, "PUT", "/mokuro-reader/S/V1.cbz", cbz_bytes(4),
                            {"Authorization": UPLOADER})
        assert status == 204
        assert _read(cbz.with_suffix(".mokuro"))["volume_uuid"] == UPLOADED
        _delete(app, "S/V1.mokuro")

        _runs(worker, monkeypatch)
        assert worker.processor.process_library_ocr(cbz, worker.processor.generations[0])
        assert _read(cbz.with_suffix(".mokuro"))["volume_uuid"] == UPLOADED

    def test_a_deleted_archive_takes_its_uuid_with_it(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Deleting the `.cbz` deletes every sidecar with it: a later upload
        under the same name is a new volume and starts fresh."""
        worker = _worker(storage)
        app = _app(storage, worker)
        cbz = _uploaded(storage, app)
        _delete(app, "S/V1.mokuro")
        _delete(app, "S/V1.cbz")
        status, _, _ = call(app, "PUT", "/mokuro-reader/S/V1.cbz", cbz_bytes(),
                            {"Authorization": UPLOADER})
        assert status == 201

        _runs(worker, monkeypatch)
        assert worker.processor.process_library_ocr(cbz, worker.processor.generations[0])
        assert _read(cbz.with_suffix(".mokuro"))["volume_uuid"] == deterministic_uuid("S/V1")

    def test_a_deleted_series_folder_takes_them_all(self, storage: Path) -> None:
        worker = _worker(storage)
        app = _app(storage, worker)
        _uploaded(storage, app)
        _delete(app, "S/V1.mokuro")
        db = Database(storage / "mokuro.db")
        assert db.remembered_volume_uuid("S/V1.cbz") == UPLOADED
        _delete(app, "S", who=ADMIN)
        assert db.remembered_volume_uuid("S/V1.cbz") is None

    def test_a_moved_series_folder_takes_them_along(self, storage: Path) -> None:
        worker = _worker(storage)
        app = _app(storage, worker)
        _uploaded(storage, app)
        _delete(app, "S/V1.mokuro")
        status, _, _ = call(app, "MOVE", "/mokuro-reader/S", headers={
            "Authorization": ADMIN, "Destination": "/mokuro-reader/T",
        })
        assert status in (201, 204), status
        db = Database(storage / "mokuro.db")
        assert db.remembered_volume_uuid("S/V1.cbz") is None
        assert db.remembered_volume_uuid("T/V1.cbz") == UPLOADED
