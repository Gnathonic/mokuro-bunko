"""Which machine wrote each OCR sidecar, and the audit trail of every result.

The owner's ask: when a processor keeps sending bad `.mokuro` files, the
library must be able to say which machine wrote which file (the provenance
table, one row per sidecar ON DISK) and what it delivered and was refused
(`ocr_sidecar_written` / `ocr_sidecar_rejected` audit events, actor = the
processor's account). A sidecar with no row is "unknown producer" -- never
guessed.
"""

from __future__ import annotations

import base64
import json
import sqlite3
import zipfile
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.processor import MokuroRunResult
from mokuro_bunko.ocr.provenance import (
    REJECTED,
    WRITTEN,
    attribute_volumes,
    failed_pages_from_log,
    read_sidecar_facts,
)
from mokuro_bunko.ocr.remote.session import RemoteSession
from mokuro_bunko.ocr.session import SessionVolume
from mokuro_bunko.ocr.watcher import OCRWorker, _OcrSlot, _SessionJob
from mokuro_bunko.server import create_app
from tests.unit.test_upload_enqueue import call

ROWS = parse_generation_list(
    [
        {"name": "mokuro", "engine": "mokuro", "primary": True},
        {"name": "paddle-manga-ppocr-manga", "engine": "paddle-manga", "detector": "ppocr-manga"},
    ]
)
LAYER = ROWS[1]
EDITOR = {"Authorization": "Basic " + base64.b64encode(b"editor:pass1234").decode()}
LAYER_REL = "S/V1.paddle-manga-ppocr-manga.mokuro"


def make_cbz(path: Path, pages: int = 2) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as archive:
        for index in range(pages):
            archive.writestr(f"{index:03}.jpg", b"not really a jpeg %d" % index)
    return path


def sidecar_json(pages: int = 2, **engine: Any) -> str:
    block = {"id": "paddle-manga", "detector": "ppocr-manga", "generator": "mokuro-bunko",
             "precision": "bf16", **engine}
    return json.dumps(
        {"version": "0.2.0", "ocr_engine": block, "pages": [{"blocks": []}] * pages}
    )


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    (tmp_path / "library" / "S").mkdir(parents=True)
    (tmp_path / "inbox").mkdir()
    (tmp_path / "users").mkdir()
    make_cbz(tmp_path / "library" / "S" / "V1.cbz")
    make_cbz(tmp_path / "library" / "S" / "V2.cbz")
    for stem in ("V1", "V2"):
        (tmp_path / "library" / "S" / f"{stem}.mokuro").write_text(
            json.dumps({"volume_uuid": f"u-{stem}", "pages": []}), encoding="utf-8"
        )
    return tmp_path


@pytest.fixture
def db(storage: Path) -> Database:
    return Database(storage / "mokuro.db")


def make_worker(storage: Path, db: Database | None) -> OCRWorker:
    return OCRWorker(
        storage_path=storage,
        poll_interval=3600.0,
        generations=ROWS,
        engines_python_path=Path("/nonexistent"),
        sessions=False,
        status_callback=lambda _line: None,
        database=db,
    )


def events(db: Database, action: str) -> list[dict[str, Any]]:
    return [e for e in db.list_audit_events(limit=1000) if e["action"] == action]


def details(event: dict[str, Any]) -> dict[str, Any]:
    return json.loads(event["details"] or "{}")


# --- the table ----------------------------------------------------------------------


def row(**overrides: Any) -> dict[str, Any]:
    base: dict[str, Any] = {
        "sidecar_path": LAYER_REL,
        "volume_key": "S/V1.cbz",
        "generation_id": LAYER.id,
        "generation_name": LAYER.name,
        "machine": "tower",
        "account": "proc",
        "engine": "paddle-manga",
        "detector": "ppocr-manga",
        "precision": "bf16",
        "runner_build": "mokuro-bunko 0.5.0, runner abc",
        "pages": 2,
        "failed_pages": 0,
        "archive_size": 10,
        "archive_mtime_ns": 5,
    }
    base.update(overrides)
    return base


class TestTable:
    def test_upsert_replaces_the_row_of_the_same_sidecar(self, db: Database) -> None:
        db.record_ocr_sidecar(row())
        db.record_ocr_sidecar(row(machine="local", account=None, pages=3))
        rows = db.list_ocr_sidecars()
        assert len(rows) == 1
        assert (rows[0]["machine"], rows[0]["account"], rows[0]["pages"]) == ("local", None, 3)
        assert rows[0]["written_at"]

    def test_forget_one_a_volume_a_prefix(self, db: Database) -> None:
        db.record_ocr_sidecar(row())
        db.record_ocr_sidecar(row(sidecar_path="S/V1.mokuro", generation_id=ROWS[0].id))
        db.record_ocr_sidecar(row(sidecar_path="S/V2.mokuro", volume_key="S/V2.cbz"))
        db.record_ocr_sidecar(row(sidecar_path="S_x/V9.mokuro", volume_key="S_x/V9.cbz"))
        assert db.forget_ocr_sidecar("S/V1.mokuro") == 1
        assert db.forget_ocr_sidecars_of_volume("S/V1.cbz") == 1
        # A `_` in a folder name is a character, not a wildcard.
        assert db.forget_ocr_sidecars_under_prefix("S") == 1
        assert [r["sidecar_path"] for r in db.list_ocr_sidecars()] == ["S_x/V9.mokuro"]

    def test_a_folder_rename_moves_its_rows(self, db: Database) -> None:
        db.record_ocr_sidecar(row())
        db.record_ocr_sidecar(row(sidecar_path="Sx/V1.mokuro", volume_key="Sx/V1.cbz"))
        assert db.rename_ocr_sidecars_under_prefix("S", "T/S") == 1
        paths = sorted((r["sidecar_path"], r["volume_key"]) for r in db.list_ocr_sidecars())
        assert paths == [("Sx/V1.mokuro", "Sx/V1.cbz"),
                         ("T/S/V1.paddle-manga-ppocr-manga.mokuro", "T/S/V1.cbz")]

    def test_the_schema_migrates_an_existing_database(self, tmp_path: Path) -> None:
        """A v3 database (no provenance table) gains it in place, rows kept."""
        path = tmp_path / "v3.db"
        old = Database(path)
        old.create_user("alice", "password123", "registered")
        with old._connection() as conn:
            conn.execute("DROP TABLE ocr_sidecars")
            conn.execute("UPDATE schema_version SET version = 3")
        old._conn.close()

        upgraded = Database(path)
        assert upgraded.get_user("alice") is not None
        assert upgraded.list_ocr_sidecars() == []
        upgraded.record_ocr_sidecar(row())
        assert len(upgraded.list_ocr_sidecars()) == 1
        raw = sqlite3.connect(path)
        assert raw.execute("SELECT version FROM schema_version").fetchone()[0] == 5
        raw.close()


class TestAttribution:
    """Per-machine counts: sidecars of the row, on disk, per producer."""

    def test_counts_sum_to_at_most_the_total_and_unknown_is_nobody(self) -> None:
        present = {LAYER.id: {"S/V1.cbz", "S/V2.cbz", "S/V3.cbz"}}
        records = [
            (LAYER.id, "S/V1.cbz", "tower"),
            (LAYER.id, "S/V2.cbz", "local"),
            # Re-run on another machine: the newest record wins, once.
            (LAYER.id, "S/V2.cbz", "tower"),
            # A record whose sidecar is not on disk any more counts nowhere.
            (LAYER.id, "S/Gone.cbz", "tower"),
        ]
        counts = attribute_volumes(records, present)
        assert counts == {LAYER.id: {"tower": 2}}
        # V3 has a sidecar with no record: unknown producer, not attributed.
        assert sum(counts[LAYER.id].values()) <= len(present[LAYER.id])

    def test_a_row_with_no_records_has_no_counts(self) -> None:
        assert attribute_volumes([], {LAYER.id: {"S/V1.cbz"}}) == {}


class TestFacts:
    def test_reads_pages_and_the_engine_block(self, tmp_path: Path) -> None:
        path = tmp_path / "x.mokuro"
        path.write_text(sidecar_json(pages=3), encoding="utf-8")
        facts = read_sidecar_facts(path)
        assert facts is not None
        assert facts.pages == 3
        assert facts.engine_block["precision"] == "bf16"
        assert facts.format_version == "0.2.0"

    def test_unreadable_is_none(self, tmp_path: Path) -> None:
        path = tmp_path / "x.mokuro"
        path.write_text("{nope", encoding="utf-8")
        assert read_sidecar_facts(path) is None
        assert read_sidecar_facts(tmp_path / "missing.mokuro") is None

    def test_failed_pages_come_from_the_runner_log(self, tmp_path: Path) -> None:
        log = tmp_path / "v.log"
        log.write_text(
            "[runner] wrote x pages=10 failed_pages=1 elapsed=1s\n"
            "[runner] wrote x pages=10 failed_pages=3 elapsed=1s\n",
            encoding="utf-8",
        )
        assert failed_pages_from_log(log) == 3
        log.write_text("mokuro says nothing of the kind\n", encoding="utf-8")
        assert failed_pages_from_log(log) is None
        assert failed_pages_from_log(None) is None


# --- the local write path -----------------------------------------------------------


def fake_engine(worker: OCRWorker, monkeypatch: pytest.MonkeyPatch, during: Any = None) -> None:
    processor = worker.processor

    def run(
        generation: GenerationSpec,
        input_path: Path,
        output_dir: Path,
        total_images: int = 0,
        source_cbz: Path | None = None,
    ) -> MokuroRunResult:
        if during is not None:
            during()
        stem = input_path.stem if input_path.is_file() else input_path.name
        (output_dir / f"{stem}{generation.sidecar_suffix}").write_text(
            sidecar_json(pages=2), encoding="utf-8"
        )
        log = output_dir / "run.log"
        log.write_text("[runner] wrote x pages=2 failed_pages=1 elapsed=0.1s\n", encoding="utf-8")
        return MokuroRunResult(True, None, log)

    monkeypatch.setattr(processor, "_run_engine", run)


class TestLocalRun:
    def test_a_local_write_records_local_and_audits_with_no_actor(
        self, storage: Path, db: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker = make_worker(storage, db)
        fake_engine(worker, monkeypatch)
        worker._scan_ocr_once()

        record = db.get_ocr_sidecar(LAYER_REL)
        assert record is not None
        assert (record["machine"], record["account"]) == ("local", None)
        assert record["volume_key"] == "S/V1.cbz"
        assert (record["generation_id"], record["generation_name"]) == (LAYER.id, LAYER.name)
        assert (record["engine"], record["detector"], record["precision"]) == (
            "paddle-manga", "ppocr-manga", "bf16"
        )
        assert (record["pages"], record["failed_pages"]) == (2, 1)
        cbz = (storage / "library" / "S" / "V1.cbz").stat()
        assert (record["archive_size"], record["archive_mtime_ns"]) == (cbz.st_size, cbz.st_mtime_ns)
        assert record["runner_build"] and record["runner_build"].startswith("mokuro-bunko ")
        assert "runner " in record["runner_build"]

        written = [e for e in events(db, WRITTEN) if e["target_path"].endswith(LAYER_REL)]
        assert len(written) == 1
        event = written[0]
        assert event["actor_username"] is None
        assert event["target_type"] == "sidecar"
        assert event["target_path"] == f"/mokuro-reader/{LAYER_REL}"
        said = details(event)
        assert said["machine"] == "local"
        assert said["generation"] == LAYER.name
        assert (said["engine"], said["detector"], said["precision"]) == (
            "paddle-manga", "ppocr-manga", "bf16"
        )
        assert (said["pages"], said["failed_pages"]) == (2, 1)
        assert said["runner_build"] == record["runner_build"]

    def test_a_discarded_local_result_is_a_rejection_and_no_record(
        self, storage: Path, db: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker = make_worker(storage, db)
        cbz = storage / "library" / "S" / "V1.cbz"
        fake_engine(worker, monkeypatch, cbz.unlink)
        worker._scan_ocr_once()

        assert db.get_ocr_sidecar(LAYER_REL) is None
        rejected = [e for e in events(db, REJECTED) if e["target_path"].endswith(LAYER_REL)]
        assert len(rejected) == 1
        assert rejected[0]["actor_username"] is None
        assert "deleted or replaced" in details(rejected[0])["reason"]

    def test_no_database_writes_the_sidecar_all_the_same(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker = make_worker(storage, None)
        fake_engine(worker, monkeypatch)
        worker._scan_ocr_once()
        assert (storage / "library" / LAYER_REL).exists()


# --- the session / processor write path ---------------------------------------------


def processor_slot(name: str = "tower", account: str = "proc-acct") -> _OcrSlot:
    entry = SimpleNamespace(
        name=name, username=account, host={"version": "0.4.9", "runner_build": "feedface"},
        dropped=False, label=lambda: f"{name} (GPU)",
    )
    return _OcrSlot(index=9, processor=SimpleNamespace(entry=entry), processor_id="pid-1")  # type: ignore[arg-type]


def session_entry(
    storage: Path, worker: OCRWorker, slot: _OcrSlot | None, content: str | None
) -> _SessionJob:
    cbz = storage / "library" / "S" / "V1.cbz"
    job = (cbz, LAYER.id)
    with worker._lock:
        worker._inflight_ocr.add(job)
        worker._note_claim(job)
        if slot is not None:
            worker._claim_owner[job] = slot
    workspace = storage / "ws"
    workspace.mkdir(exist_ok=True)
    out = workspace / "V1.paddle-manga-ppocr-manga.mokuro"
    if content is not None:
        out.write_text(content, encoding="utf-8")
    return _SessionJob(
        job=job,
        generation=LAYER,
        volume=SessionVolume(
            id="v1", workspace=workspace, output=out,
            cache_dir=workspace / "c", detect_dir=workspace / "d",
            log=workspace / "v.log", title="S", volume="V1", archive=cbz,
        ),
        owner=slot,
        hardware=slot.processor.entry.name if slot is not None else "local",  # type: ignore[attr-defined]
    )


def done(failed: int = 0) -> dict[str, Any]:
    return {"event": "volume_done", "id": "v1", "pages": 2, "failed_pages": failed,
            "seconds": 1.0}


class TestSessionDelivery:
    def test_a_processor_result_is_recorded_under_its_machine_and_account(
        self, storage: Path, db: Database
    ) -> None:
        worker = make_worker(storage, db)
        entry = session_entry(storage, worker, processor_slot(), sidecar_json(pages=2))

        assert worker._collect_session_volume(entry, done(failed=1)) is True

        record = db.get_ocr_sidecar(LAYER_REL)
        assert record is not None
        assert (record["machine"], record["account"]) == ("tower", "proc-acct")
        assert (record["pages"], record["failed_pages"]) == (2, 1)
        assert record["runner_build"] == "mokuro-bunko 0.4.9, runner feedface"
        (event,) = events(db, WRITTEN)
        assert event["actor_username"] == "proc-acct"
        assert event["target_type"] == "sidecar"
        said = details(event)
        assert said["machine"] == "tower"
        assert said["failed_pages"] == 1
        assert said["runner_build"] == "mokuro-bunko 0.4.9, runner feedface"

    def test_a_local_session_is_local(self, storage: Path, db: Database) -> None:
        worker = make_worker(storage, db)
        entry = session_entry(storage, worker, None, sidecar_json(pages=2))
        assert worker._collect_session_volume(entry, done()) is True
        record = db.get_ocr_sidecar(LAYER_REL)
        assert record is not None and record["machine"] == "local"
        assert events(db, WRITTEN)[0]["actor_username"] is None

    def test_a_rerun_replaces_the_record(self, storage: Path, db: Database) -> None:
        worker = make_worker(storage, db)
        entry = session_entry(storage, worker, processor_slot(), sidecar_json(pages=2))
        assert worker._collect_session_volume(entry, done()) is True
        (storage / "library" / LAYER_REL).unlink()
        db.forget_ocr_sidecar(LAYER_REL)  # what the sidecar's DELETE does
        again = session_entry(
            storage, worker, processor_slot("desktop", "acct-2"), sidecar_json(pages=2)
        )
        assert worker._collect_session_volume(again, done()) is True

        rows = db.list_ocr_sidecars()
        assert [(r["sidecar_path"], r["machine"]) for r in rows] == [(LAYER_REL, "desktop")]
        assert [e["actor_username"] for e in events(db, WRITTEN)] == ["acct-2", "proc-acct"]

    def test_an_overwrite_in_place_replaces_the_record(self, storage: Path, db: Database) -> None:
        db.record_ocr_sidecar(row(machine="old", account="old-acct"))
        worker = make_worker(storage, db)
        entry = session_entry(storage, worker, processor_slot(), sidecar_json(pages=2))
        assert worker._collect_session_volume(entry, done()) is True
        # The file landed at its own name (none was there); the one row for
        # it names the machine that wrote what is on disk now.
        assert (storage / "library" / LAYER_REL).exists()
        assert [r["machine"] for r in db.list_ocr_sidecars()] == ["tower"]

    def test_an_invalid_result_is_rejected_with_its_reason(
        self, storage: Path, db: Database
    ) -> None:
        worker = make_worker(storage, db)
        entry = session_entry(storage, worker, processor_slot(), "{not json")

        assert worker._collect_session_volume(entry, done()) is False

        assert db.list_ocr_sidecars() == []
        (event,) = events(db, REJECTED)
        assert event["actor_username"] == "proc-acct"
        assert event["target_type"] == "sidecar"
        assert event["target_path"] == f"/mokuro-reader/{LAYER_REL}"
        said = details(event)
        assert said["machine"] == "tower"
        assert "not readable JSON" in said["reason"]
        assert events(db, WRITTEN) == []

    def test_a_missing_result_is_rejected(self, storage: Path, db: Database) -> None:
        worker = make_worker(storage, db)
        entry = session_entry(storage, worker, processor_slot(), None)
        assert worker._collect_session_volume(entry, done()) is False
        (event,) = events(db, REJECTED)
        assert "no valid" in details(event)["reason"]

    def test_a_result_for_a_deleted_archive_is_rejected(
        self, storage: Path, db: Database
    ) -> None:
        worker = make_worker(storage, db)
        entry = session_entry(storage, worker, processor_slot(), sidecar_json())
        (storage / "library" / "S" / "V1.cbz").unlink()

        assert worker._collect_session_volume(entry, done()) is False

        assert db.list_ocr_sidecars() == []
        (event,) = events(db, REJECTED)
        assert event["actor_username"] == "proc-acct"
        assert "deleted or replaced" in details(event)["reason"]

    def test_a_wrongly_named_remote_sidecar_is_rejected(
        self, storage: Path, db: Database
    ) -> None:
        """An integrity failure on the wire: the processor named the file wrongly."""
        worker = make_worker(storage, db)
        slot = processor_slot()
        entry = session_entry(storage, worker, slot, None)

        class Entry:
            name = "tower"
            username = "proc-acct"
            lock = __import__("threading").Lock()
            sessions: dict[str, Any] = {}

            def send(self, op: Any) -> bool:
                return True

            def note_ended(self, sid: str) -> None:
                pass

            def label(self) -> str:
                return "tower"

        session = RemoteSession(
            Entry(), LAYER, sid="s1", row_spec={}, library_path=storage / "library",  # type: ignore[arg-type]
            archives_root="/_processor/archives/",
        )
        worker._watch_remote_rejections(session, slot, LAYER)
        assert session.submit(entry.volume)
        session.feed({"event": "sidecar", "id": "v1", "name": "evil.mokuro"}, b"{}")

        (event,) = events(db, REJECTED)
        assert event["actor_username"] == "proc-acct"
        assert "evil.mokuro" in details(event)["reason"]


# --- WebDAV: sidecars leaving the library take their rows ---------------------------


class TestWebdavCleanup:
    @pytest.fixture
    def app(self, storage: Path, db: Database) -> Any:
        db.create_user("editor", "pass1234", "editor")
        for stem in ("V1", "V2"):
            (storage / "library" / "S" / f"{stem}.paddle-manga-ppocr-manga.mokuro").write_text(
                sidecar_json(), encoding="utf-8"
            )
            db.record_ocr_sidecar(row(
                sidecar_path=f"S/{stem}.paddle-manga-ppocr-manga.mokuro",
                volume_key=f"S/{stem}.cbz",
            ))
            db.record_ocr_sidecar(row(
                sidecar_path=f"S/{stem}.mokuro", volume_key=f"S/{stem}.cbz",
                generation_id=ROWS[0].id,
            ))
        return create_app(Config(storage=StorageConfig(base_path=storage)))

    def paths(self, db: Database) -> list[str]:
        return sorted(r["sidecar_path"] for r in db.list_ocr_sidecars())

    def test_deleting_a_volume_removes_its_records(self, app: Any, db: Database) -> None:
        status, _, _ = call(app, "DELETE", "/mokuro-reader/S/V1.cbz", headers=EDITOR)
        assert status == 204
        assert self.paths(db) == ["S/V2.mokuro", "S/V2.paddle-manga-ppocr-manga.mokuro"]

    def test_moving_a_volume_removes_its_records(self, app: Any, db: Database) -> None:
        status, _, _ = call(app, "MOVE", "/mokuro-reader/S/V1.cbz", headers={
            **EDITOR, "Destination": "/mokuro-reader/S/V1 renamed.cbz",
        })
        assert status in (201, 204)
        assert self.paths(db) == ["S/V2.mokuro", "S/V2.paddle-manga-ppocr-manga.mokuro"]

    def test_deleting_a_sidecar_removes_its_record(self, app: Any, db: Database) -> None:
        status, _, _ = call(
            app, "DELETE", "/mokuro-reader/S/V1.paddle-manga-ppocr-manga.mokuro", headers=EDITOR
        )
        assert status == 204
        assert "S/V1.paddle-manga-ppocr-manga.mokuro" not in self.paths(db)
        assert "S/V1.mokuro" in self.paths(db)

    def test_overwriting_a_sidecar_over_webdav_removes_its_record(
        self, app: Any, db: Database
    ) -> None:
        """Whoever wrote the new bytes, it was not the machine on record."""
        status, _, _ = call(
            app, "PUT", "/mokuro-reader/S/V1.paddle-manga-ppocr-manga.mokuro",
            body=sidecar_json().encode(), headers=EDITOR,
        )
        assert status in (201, 204)
        assert "S/V1.paddle-manga-ppocr-manga.mokuro" not in self.paths(db)

    def test_deleting_the_series_removes_every_record(self, app: Any, db: Database) -> None:
        status, _, _ = call(app, "DELETE", "/mokuro-reader/S", headers=EDITOR)
        assert status == 204
        assert self.paths(db) == []

    def test_renaming_the_series_keeps_the_records_at_the_new_paths(
        self, app: Any, db: Database
    ) -> None:
        """The sidecars travel with the folder, and so does who wrote them."""
        status, _, _ = call(app, "MOVE", "/mokuro-reader/S", headers={
            **EDITOR, "Destination": "/mokuro-reader/T",
        })
        assert status in (201, 204)
        assert self.paths(db) == [
            "T/V1.mokuro", "T/V1.paddle-manga-ppocr-manga.mokuro",
            "T/V2.mokuro", "T/V2.paddle-manga-ppocr-manga.mokuro",
        ]
        assert {r["volume_key"] for r in db.list_ocr_sidecars()} == {"T/V1.cbz", "T/V2.cbz"}


class TestCorruptSweep:
    def test_a_removed_corrupt_sidecar_takes_its_record(
        self, storage: Path, db: Database
    ) -> None:
        bad = storage / "library" / LAYER_REL
        bad.write_text("{broken", encoding="utf-8")
        db.record_ocr_sidecar(row())
        worker = make_worker(storage, db)
        assert worker._remove_corrupt_sidecars() == 1
        assert db.get_ocr_sidecar(LAYER_REL) is None


class TestProcessorReportsItsBuild:
    def test_the_host_it_registers_with_names_its_version_and_runner(
        self, tmp_path: Path
    ) -> None:
        from mokuro_bunko import __version__
        from mokuro_bunko.processor.cli import host_with_build

        runner = tmp_path / ".processing" / "runner-0123456789abcdef" / "engine_runner.py"
        host = host_with_build({"cpu": "x", "gpu": None}, runner)
        assert host == {"cpu": "x", "gpu": None, "version": __version__,
                        "runner_build": "0123456789abcdef"}
