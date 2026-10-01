"""Every generation a volume is owed is a real, claimable job from the start.

Owner: many machines sat idle while an upload's layers waited for its
primary -- a layer was not claimable until the volume's `<Volume>.mokuro`
existed, and a volume belonged to one slot at a time, so all of a volume's
generations ran one after another through the primary.

What that gate protected was the `volume_uuid`: a layer inherited it by
reading the primary sidecar, and one written first got a fresh random id,
detached from the volume. The uuid is now the volume's own from the start --
the primary's when a `.mokuro` exists, else an existing layer's, else the
deterministic id the index and the reader already give a volume with no
`.mokuro` -- so whichever sidecar lands first, they all agree.

Row order stays a PRIORITY: every volume's first generation comes before any
volume's second, and with machines equally fit the queue is taken in order.

The missing-pages rule applies only to a `.mokuro` that was already there
(uploaded or imported with the volume): our own primary is produced from the
archive itself, so there is nothing for it to be short against. For a
supplied one the check is made at once, before any OCR, rather than waiting
for the metadata pass.
"""

from __future__ import annotations

import base64
import json
import sys
import zipfile
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.metadata.compiler import (
    SeriesFolder,
    cached_missing_pages,
    compile_series_volumes,
    missing_pages_now,
)
from mokuro_bunko.metadata.reader_compat import deterministic_uuid
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.devices import DeviceCatalog
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.processor import MokuroRunResult, OCRProcessor
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.queue.api import QueueAPI
from mokuro_bunko.server import create_app
from tests.unit.test_upload_enqueue import call, cbz_bytes, put

ROWS: list[dict[str, Any]] = [
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {"name": "nova", "engine": "hayai-nova", "detector": "ctd"},
    {"name": "paddle", "engine": "paddle-manga"},
]
CATALOG: dict[str, Any] = {
    "engines": ["mokuro", "hayai-nova", "paddle-manga"],
    "detectors": ["ctd", "ppocr-manga"],
    "devices": [],
    "serves_mokuro": True,
}
UPLOADER = "Basic " + base64.b64encode(b"uploader:pass1234").decode()


def _gens(rows: list[dict[str, Any]] = ROWS) -> list[GenerationSpec]:
    return parse_generation_list([dict(row) for row in rows], devices=DeviceCatalog())


def _cbz(path: Path, pages: int = 3) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as archive:
        for index in range(pages):
            archive.writestr(f"p{index:03d}.jpg", b"not really a jpeg %d" % index)
    return path


def _mokuro(cbz: Path, *, pages: int, uuid: str = "supplied-uuid") -> Path:
    """A `.mokuro` beside ``cbz`` naming ``pages`` of its images."""
    sidecar = cbz.with_suffix(".mokuro")
    sidecar.write_text(
        json.dumps(
            {
                "version": "0.2.1",
                "title": cbz.parent.name,
                "title_uuid": "t",
                "volume": cbz.stem,
                "volume_uuid": uuid,
                "pages": [
                    {"img_path": f"p{n:03d}.jpg", "img_width": 10, "img_height": 10,
                     "blocks": []}
                    for n in range(pages)
                ],
                "chars": 0,
            }
        ),
        encoding="utf-8",
    )
    return sidecar


def _names(worker: OCRWorker, jobs: Any) -> list[tuple[str, str]]:
    return [(job[0].stem, worker._generation_name(job[1])) for job in jobs]


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    for name in ("library", "inbox", "users"):
        (base / name).mkdir(parents=True)
    return base


def _local_worker(storage: Path, concurrency: int = 1, **kwargs: Any) -> OCRWorker:
    kwargs.setdefault("page_count_lookup", lambda _path: 20)
    return OCRWorker(
        storage_path=storage,
        poll_interval=3600.0,
        generations=_gens(),
        engines_python_path=Path("/nonexistent"),
        concurrency=concurrency,
        sessions=False,
        **kwargs,
    )


class TestEveryGenerationIsOwedAtOnce:
    def test_a_volume_with_no_mokuro_offers_every_row(self, storage: Path) -> None:
        cbz = _cbz(storage / "library" / "S" / "V1.cbz")
        processor = OCRProcessor(storage_path=storage, generations=_gens())
        assert [row.name for row in processor.missing_generations(cbz)] == [
            "mokuro", "nova", "paddle",
        ]

    def test_three_slots_run_three_generations_of_one_volume_together(
        self, storage: Path
    ) -> None:
        _cbz(storage / "library" / "S" / "V1.cbz")
        worker = _local_worker(storage, concurrency=3)
        claimed = [worker.claim_next(slot) for slot in worker._slots]
        assert _names(worker, claimed) == [("V1", "mokuro"), ("V1", "nova"), ("V1", "paddle")]
        assert len(worker._inflight_ocr) == 3

    def test_one_slot_still_takes_the_queue_in_order(self, storage: Path) -> None:
        """Every volume's first generation before any volume's second."""
        for volume in ("V1", "V2"):
            _cbz(storage / "library" / "S" / f"{volume}.cbz")
        worker = _local_worker(storage)
        claimed = [worker.claim_next() for _ in range(6)]
        assert _names(worker, claimed) == [
            ("V1", "mokuro"), ("V2", "mokuro"),
            ("V1", "nova"), ("V2", "nova"),
            ("V1", "paddle"), ("V2", "paddle"),
        ]


class TestAnUpload:
    """The PUT enqueue path (`archive_arrived`): all owed generations at once."""

    @pytest.fixture
    def app(self, storage: Path) -> Any:
        db = Database(storage / "mokuro.db")
        db.create_user("uploader", "pass1234", "uploader")
        _cbz(storage / "library" / "S" / "V1.cbz")
        worker = _local_worker(storage)
        for row in worker.generations:
            worker.rates.record_volume(row.id, 20, 10.0)
            worker.rates.record_volume(row.id, 20, 10.0)
            worker.rates.record_startup(row.id, 5.0)
        control = OcrControl()
        control.worker = worker
        app = create_app(Config(storage=StorageConfig(base_path=storage)), ocr_control=control)
        return app, worker, control

    def test_every_generation_is_claimable_the_moment_it_lands(
        self, storage: Path, app: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        app, worker, control = app
        worker.pending_jobs(max_age=0)

        def walked(*_args: Any, **_kwargs: Any) -> Any:
            raise AssertionError("the library was walked")

        monkeypatch.setattr(worker, "_eligible_ocr_jobs", walked)
        status, _, _ = put(app, "/mokuro-reader/S/V2.cbz", cbz_bytes())
        assert status == 201

        listed = [(e["volume"], e["generation"]) for e in worker.last_pending()]
        assert listed == [
            ("V1", "mokuro"), ("V2", "mokuro"),
            ("V1", "nova"), ("V2", "nova"),
            ("V1", "paddle"), ("V2", "paddle"),
        ]

        # The queue page, the queue file and the manifest list the same jobs,
        # each an ordinary queued job with a finishing time.
        api = QueueAPI(lambda e, s: [], storage_base_path=str(storage), ocr_control=control)
        page = [(e["volume"], e["generation"]) for e in api.raw_status()["pending_ocr"]]
        assert page == listed
        assert all(e["eta_at"] is not None for e in api.raw_status()["pending_ocr"])
        held, volumes, _ = control.queue_document([], wait=0.0)
        assert held is None
        in_file = sorted(
            (volume["volume"], job["id"]) for volume in volumes for job in volume["jobs"]
        )
        assert in_file == sorted(listed)
        assert {job["state"] for volume in volumes for job in volume["jobs"]} == {"queued"}
        _, _, body = call(app, "GET", "/catalog/api/manifest", query="series=S&volume=V2")
        pending = json.loads(body)["pending"]
        assert [(e["kind"], e["id"]) for e in pending] == [
            ("ocr", "mokuro"), ("layer", "nova"), ("layer", "paddle"),
        ]
        assert all(e["eta"] is not None for e in pending), pending

    def test_a_supplied_short_mokuro_keeps_the_layers_out_from_the_start(
        self, storage: Path
    ) -> None:
        db = Database(storage / "mokuro.db")
        library = storage / "library"
        cbz = _cbz(library / "S" / "V1.cbz", pages=3)
        _mokuro(cbz, pages=5)  # names two pages the archive does not have
        worker = _local_worker(
            storage, missing_pages_lookup=lambda path: missing_pages_now(db, library, path)
        )
        worker.pending_jobs(max_age=0)
        worker.archive_arrived(cbz)
        assert worker.last_pending() == []
        assert worker.claim_next() is None
        assert worker.owed_generations(cbz) == []


class TestEarliestFinishSpreadsAVolume:
    """Idle machines take a volume's later generations while its first runs."""

    def _rig(self, storage: Path, volumes: list[str], machines: int) -> tuple[OCRWorker, list[Any]]:
        for volume in volumes:
            _cbz(storage / "library" / "S" / f"{volume}.cbz")
        registry = ProcessorRegistry()
        worker = OCRWorker(
            storage_path=storage, poll_interval=30.0, generations=_gens(),
            engines_python_path=Path(sys.executable), concurrency=1, sessions=True,
            remote=registry, local_processing=False,
            page_count_lookup=lambda path: 100,
        )
        slots = []
        for index in range(machines):
            name = f"m{index + 1}"
            entry = registry.register(
                username=name, name=name, host={}, catalog=CATALOG, max_sessions=1
            )
            entry.stream_open = True
            for row in worker.generations:
                key = worker._rate_key(row.id, name)
                for _ in range(3):
                    worker.rates.record_volume(key, 100, 20.0)
                worker.rates.record_startup(key, 5.0)
            slots.append(worker._make_remote_slot(index, entry))
        for slot in slots:
            slot.running = True
        worker._active_slots = list(slots)
        return worker, slots

    def test_three_idle_machines_take_the_three_generations_of_one_volume(
        self, storage: Path
    ) -> None:
        worker, slots = self._rig(storage, ["V1"], machines=3)
        claimed = [worker.claim_next(slot) for slot in slots]
        assert _names(worker, claimed) == [("V1", "mokuro"), ("V1", "nova"), ("V1", "paddle")]
        # All three at once, one per machine.
        assert {worker._claim_owner[job].processor_id for job in claimed} == {
            slot.processor_id for slot in slots
        }
        assert len(worker._inflight_ocr) == 3

    def test_with_equally_fit_machines_the_queue_order_wins(self, storage: Path) -> None:
        """The second machine takes the NEXT volume's first generation, not
        this volume's second: row order is still the priority."""
        worker, slots = self._rig(storage, ["V1", "V2"], machines=2)
        first = worker.claim_next(slots[0])
        second = worker.claim_next(slots[1])
        assert _names(worker, [first, second]) == [("V1", "mokuro"), ("V2", "mokuro")]


class TestTheMissingPagesRule:
    @pytest.fixture
    def database(self, storage: Path) -> Database:
        return Database(storage / "mokuro.db")

    def _processor(self, storage: Path, lookup: Any) -> OCRProcessor:
        processor = OCRProcessor(storage_path=storage, generations=_gens())
        processor.missing_pages_lookup = lookup
        return processor

    def test_a_supplied_short_mokuro_is_judged_before_any_metadata_pass(
        self, storage: Path, database: Database
    ) -> None:
        library = storage / "library"
        cbz = _cbz(library / "S" / "V1.cbz", pages=3)
        _mokuro(cbz, pages=5)
        assert cached_missing_pages(database, library, cbz) == 0, "nothing compiled yet"
        processor = self._processor(
            storage, lambda path: missing_pages_now(database, library, path)
        )
        assert processor.missing_generations(cbz) == []
        assert [row.name for row in processor.skipped_generations(cbz)] == ["nova", "paddle"]
        # The verdict is the metadata pass's own entry, kept for it.
        assert cached_missing_pages(database, library, cbz) == 2
        compile_series_volumes(SeriesFolder(title="S", path=library / "S"), database=database)
        assert cached_missing_pages(database, library, cbz) == 2

    def test_a_supplied_whole_mokuro_gets_its_layers(
        self, storage: Path, database: Database
    ) -> None:
        library = storage / "library"
        cbz = _cbz(library / "S" / "V1.cbz", pages=3)
        _mokuro(cbz, pages=3)
        processor = self._processor(
            storage, lambda path: missing_pages_now(database, library, path)
        )
        assert [row.name for row in processor.missing_generations(cbz)] == ["nova", "paddle"]

    def test_without_a_mokuro_there_is_nothing_to_be_short_against(
        self, storage: Path, database: Database
    ) -> None:
        library = storage / "library"
        cbz = _cbz(library / "S" / "V1.cbz", pages=3)
        assert missing_pages_now(database, library, cbz) == 0
        # Whatever a stale answer says, a volume with no `.mokuro` has no gate.
        processor = self._processor(storage, lambda path: 7)
        assert [row.name for row in processor.missing_generations(cbz)] == [
            "mokuro", "nova", "paddle",
        ]


def _fake_run(
    processor: OCRProcessor, monkeypatch: pytest.MonkeyPatch, payloads: dict[str, dict]
) -> None:
    """`_run_engine` writes the row's sidecar with that row's payload."""

    def run(
        generation: GenerationSpec,
        input_path: Path,
        output_dir: Path,
        total_images: int = 0,
        source_cbz: Path | None = None,
    ) -> MokuroRunResult:
        stem = input_path.stem if input_path.is_file() else input_path.name
        suffix = (
            ".mokuro" if processor.runs_mokuro_cli(generation) else generation.sidecar_suffix
        )
        (output_dir / f"{stem}{suffix}").write_text(
            json.dumps(payloads[generation.name]), encoding="utf-8"
        )
        return MokuroRunResult(True, None, None)

    monkeypatch.setattr(processor, "_run_engine", run)


def _pages(n: int) -> list[dict[str, Any]]:
    return [{"img_path": f"p{i:03d}.jpg", "img_width": 10, "img_height": 10, "blocks": []}
            for i in range(n)]


def _read(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


class TestOneUuidWhicheverLandsFirst:
    """A layer written before its primary is the same volume to every reader."""

    def _setup(self, storage: Path) -> tuple[OCRProcessor, Path]:
        cbz = _cbz(storage / "library" / "S" / "V1.cbz")
        return OCRProcessor(storage_path=storage, generations=_gens()), cbz

    def test_a_layer_that_finishes_first_and_the_primary_after_it_agree(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor, cbz = self._setup(storage)
        mokuro, nova, _ = processor.generations
        _fake_run(processor, monkeypatch, {
            # Each run mints its own random uuid, as the runner and mokuro do.
            "nova": {"volume_uuid": "random-from-nova", "pages": _pages(3)},
            "mokuro": {"volume_uuid": "random-from-mokuro", "pages": _pages(3)},
        })
        assert processor.process_library_ocr(cbz, nova)
        assert processor.process_library_ocr(cbz, mokuro)

        layer = _read(cbz.parent / "V1.nova.mokuro")
        primary = _read(cbz.parent / "V1.mokuro")
        expected = deterministic_uuid("S/V1")
        assert layer["volume_uuid"] == primary["volume_uuid"] == expected
        for key in ("title", "title_uuid", "volume"):
            assert layer[key] == primary[key]
        assert [p["img_path"] for p in layer["pages"]] == [
            p["img_path"] for p in primary["pages"]
        ]
        # The id the catalog index publishes is that same one.
        (entry,) = compile_series_volumes(SeriesFolder(title="S", path=cbz.parent))
        assert entry.volume_uuid == expected

    def test_the_primary_first_gives_the_same_answer(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor, cbz = self._setup(storage)
        mokuro, nova, _ = processor.generations
        _fake_run(processor, monkeypatch, {
            "nova": {"volume_uuid": "random-from-nova", "pages": _pages(3)},
            "mokuro": {"volume_uuid": "random-from-mokuro", "pages": _pages(3)},
        })
        assert processor.process_library_ocr(cbz, mokuro)
        assert processor.process_library_ocr(cbz, nova)
        assert (
            _read(cbz.parent / "V1.mokuro")["volume_uuid"]
            == _read(cbz.parent / "V1.nova.mokuro")["volume_uuid"]
            == deterministic_uuid("S/V1")
        )

    def test_the_session_install_path_agrees_too(self, storage: Path) -> None:
        """Sessions -- this server's and every processor's -- land here."""
        processor, cbz = self._setup(storage)
        mokuro, nova, paddle = processor.generations
        for row, own in ((paddle, "p"), (nova, "n"), (mokuro, "m")):
            volume = processor.prepare_session_volume(cbz, row, f"job-{row.name}")
            # What the runner is told to stamp: the volume's id, no primary needed.
            assert volume.volume_uuid == deterministic_uuid("S/V1")
            volume.output.write_text(
                json.dumps({"volume_uuid": f"random-{own}", "pages": _pages(3)}),
                encoding="utf-8",
            )
            assert processor.install_session_sidecar(cbz, row, volume.output) is None
        uuids = {
            _read(path)["volume_uuid"]
            for path in (cbz.parent / "V1.mokuro", cbz.parent / "V1.nova.mokuro",
                         cbz.parent / "V1.paddle.mokuro")
        }
        assert uuids == {deterministic_uuid("S/V1")}

    def test_a_supplied_primary_still_gives_every_layer_its_uuid(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        processor, cbz = self._setup(storage)
        _mokuro(cbz, pages=3, uuid="the-uploaders-uuid")
        _fake_run(processor, monkeypatch, {
            "nova": {"volume_uuid": "random-from-nova", "pages": _pages(3)},
        })
        assert processor.process_library_ocr(cbz, processor.generations[1])
        assert _read(cbz.parent / "V1.nova.mokuro")["volume_uuid"] == "the-uploaders-uuid"

    def test_a_regenerated_primary_keeps_the_uuid_its_layers_carry(
        self, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The `.mokuro` was deleted and is made again: the layers already on
        disk (and every reader's progress) know the volume by the old id."""
        processor, cbz = self._setup(storage)
        (cbz.parent / "V1.nova.mokuro").write_text(
            json.dumps({"volume_uuid": "the-old-uuid", "pages": _pages(3)}), encoding="utf-8"
        )
        _fake_run(processor, monkeypatch, {
            "mokuro": {"volume_uuid": "random-from-mokuro", "pages": _pages(3)},
            "paddle": {"volume_uuid": "random-from-paddle", "pages": _pages(3)},
        })
        assert processor.process_library_ocr(cbz, processor.generations[2])
        assert processor.process_library_ocr(cbz, processor.generations[0])
        assert _read(cbz.parent / "V1.mokuro")["volume_uuid"] == "the-old-uuid"
        assert _read(cbz.parent / "V1.paddle.mokuro")["volume_uuid"] == "the-old-uuid"
