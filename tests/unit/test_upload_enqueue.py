"""A `.cbz` written over WebDAV is queued at once, and says when to look again.

Without this an upload reached the OCR queue at the library's next poll or
scan. Now the PUT (or a MOVE/COPY into place) hands the archive to the worker
before the response goes out: its jobs join the cached queue in the order the
scheduler would give them -- no library walk -- the OCR loop is woken, and
the PUT answers with the volume's manifest URL and a recheck time priced from
the queue's own plan.
"""

from __future__ import annotations

import base64
import io
import json
import time
import urllib.parse
import zipfile
from collections.abc import Generator
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.server import create_app

ROWS = parse_generation_list(
    [
        {"name": "mokuro", "engine": "mokuro", "primary": True},
        {"name": "nova", "engine": "hayai-nova", "detector": "ppocr-manga"},
    ]
)
UPLOADER = "Basic " + base64.b64encode(b"uploader:pass1234").decode()


def cbz_bytes(pages: int = 3) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as archive:
        for index in range(pages):
            archive.writestr(f"{index:03}.jpg", b"fake image bytes %d" % index)
    return buffer.getvalue()


def call(
    app: Any,
    method: str,
    path: str,
    body: bytes = b"",
    headers: dict[str, str] | None = None,
    query: str = "",
    *,
    content_length: int | None = None,
    stream: Any = None,
) -> tuple[int, dict[str, str], bytes]:
    """One request through the app. ``content_length`` claims a length other
    than the body's (-1: none, as a chunked upload); ``stream`` replaces the
    body's reader."""
    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "SCRIPT_NAME": "",
        "PATH_INFO": path,
        "QUERY_STRING": query,
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
        "SERVER_PROTOCOL": "HTTP/1.1",
        "HTTP_HOST": "localhost:8080",
        "wsgi.version": (1, 0),
        "wsgi.url_scheme": "http",
        "wsgi.input": stream if stream is not None else io.BytesIO(body),
        "wsgi.errors": io.StringIO(),
        "wsgi.multithread": False,
        "wsgi.multiprocess": False,
        "wsgi.run_once": False,
        "CONTENT_LENGTH": str(len(body) if content_length is None else content_length),
        "CONTENT_TYPE": "application/octet-stream",
    }
    if content_length == -1:
        del environ["CONTENT_LENGTH"]
    for key, value in (headers or {}).items():
        environ["HTTP_" + key.upper().replace("-", "_")] = value
    state: dict[str, Any] = {}

    def start_response(status: str, response_headers: list[tuple[str, str]], exc_info: Any = None) -> Any:
        state["status"] = int(status.split(" ", 1)[0])
        state["headers"] = dict(response_headers)
        return lambda _chunk: None

    result = app(environ, start_response)
    try:
        payload = b"".join(result)
    finally:
        if hasattr(result, "close"):
            result.close()
    return state["status"], state["headers"], payload


def put(app: Any, path: str, body: bytes, **headers: str) -> tuple[int, dict[str, str], bytes]:
    return call(app, "PUT", path, body, {"Authorization": UPLOADER, **headers})


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    (base / "library" / "S").mkdir(parents=True)
    (base / "inbox").mkdir()
    (base / "users").mkdir()
    (base / "library" / "S" / "V1.cbz").write_bytes(cbz_bytes())
    db = Database(base / "mokuro.db")
    db.create_user("uploader", "pass1234", "uploader")
    return base


def make_worker(storage: Path, **kwargs: Any) -> OCRWorker:
    kwargs.setdefault("page_count_lookup", lambda _path: 20)
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=3600.0,
        generations=ROWS,
        engines_python_path=Path("/nonexistent"),
        sessions=False,
        **kwargs,
    )
    # Measured rates on this machine: the primary row 2 pages/s, the layer 4.
    worker.rates.record_volume(ROWS[0].id, 20, 10.0)
    worker.rates.record_volume(ROWS[0].id, 20, 10.0)
    worker.rates.record_startup(ROWS[0].id, 5.0)
    worker.rates.record_volume(ROWS[1].id, 20, 5.0)
    worker.rates.record_volume(ROWS[1].id, 20, 5.0)
    worker.rates.record_startup(ROWS[1].id, 5.0)
    return worker


@pytest.fixture
def worker(storage: Path) -> OCRWorker:
    return make_worker(storage)


@pytest.fixture
def app(storage: Path, worker: OCRWorker) -> Generator[Any, None, None]:
    control = OcrControl()
    control.worker = worker
    yield create_app(Config(storage=StorageConfig(base_path=storage)), ocr_control=control)


def triples(entries: list[dict[str, Any]]) -> list[tuple[str, str, str]]:
    return [(e["series"], e["volume"], e["generation"]) for e in entries]


def forbid_library_walk(worker: OCRWorker, monkeypatch: pytest.MonkeyPatch) -> None:
    def walked(*_args: Any, **_kwargs: Any) -> Any:
        raise AssertionError("the library was walked")

    monkeypatch.setattr(worker, "_eligible_ocr_jobs", walked)


class TestEnqueue:
    def test_a_put_volume_is_pending_without_a_scan(
        self, app: Any, worker: OCRWorker, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        assert triples(worker.pending_jobs(max_age=0)) == [
            ("S", "V1", "mokuro"), ("S", "V1", "nova"),
        ]
        forbid_library_walk(worker, monkeypatch)

        status, _, _ = put(app, "/mokuro-reader/S/V2.cbz", cbz_bytes())

        assert status == 201
        # Every generation it is owed, at once; each volume's first row first.
        assert triples(worker.last_pending()) == [
            ("S", "V1", "mokuro"), ("S", "V2", "mokuro"),
            ("S", "V1", "nova"), ("S", "V2", "nova"),
        ]
        assert worker.wake_requested()

    def test_the_new_volume_takes_the_schedulers_place_not_the_end(
        self, storage: Path, app: Any, worker: OCRWorker, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Round-robin across series: a new series' volume 1 goes before S's volume 2."""
        (storage / "library" / "S" / "V2.cbz").write_bytes(cbz_bytes())
        (storage / "library" / "T").mkdir()
        worker.pending_jobs(max_age=0)
        forbid_library_walk(worker, monkeypatch)

        status, _, _ = put(app, "/mokuro-reader/T/V1.cbz", cbz_bytes())

        assert status == 201
        assert triples(worker.last_pending()) == [
            ("S", "V1", "mokuro"), ("T", "V1", "mokuro"), ("S", "V2", "mokuro"),
            ("S", "V1", "nova"), ("T", "V1", "nova"), ("S", "V2", "nova"),
        ]

    def test_a_replaced_archive_is_not_listed_twice(
        self, app: Any, worker: OCRWorker
    ) -> None:
        worker.pending_jobs(max_age=0)
        status, _, _ = put(app, "/mokuro-reader/S/V1.cbz", cbz_bytes(4))
        assert status == 204
        assert triples(worker.last_pending()) == [("S", "V1", "mokuro"), ("S", "V1", "nova")]

    def test_with_no_cached_queue_the_next_read_finds_it(self, app: Any, worker: OCRWorker) -> None:
        status, _, _ = put(app, "/mokuro-reader/S/V2.cbz", cbz_bytes())
        assert status == 201
        assert ("S", "V2", "mokuro") in triples(worker.last_pending())
        assert worker.wake_requested()

    def test_a_move_into_place_is_queued_too(
        self, storage: Path, app: Any, worker: OCRWorker, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        (storage / "library" / "S" / "V2.part").write_bytes(cbz_bytes())
        worker.pending_jobs(max_age=0)
        forbid_library_walk(worker, monkeypatch)

        editor_db = Database(storage / "mokuro.db")
        editor_db.create_user("editor", "pass1234", "editor")
        editor = "Basic " + base64.b64encode(b"editor:pass1234").decode()
        status, _, _ = call(
            app, "MOVE", "/mokuro-reader/S/V2.part", headers={
                "Authorization": editor, "Destination": "/mokuro-reader/S/V2.cbz",
            },
        )

        assert status in (201, 204)
        assert ("S", "V2", "mokuro") in triples(worker.last_pending())

    def test_a_sidecar_put_does_not_touch_the_queue(
        self, app: Any, worker: OCRWorker
    ) -> None:
        worker.pending_jobs(max_age=0)
        status, headers, _ = put(app, "/mokuro-reader/S/V1.webp", b"RIFF....WEBP")
        assert status == 201
        assert "X-Mokuro-Manifest" not in headers
        assert not worker.wake_requested()


class TestPutHeaders:
    def test_manifest_and_recheck(self, app: Any) -> None:
        status, headers, _ = put(app, "/mokuro-reader/S/V 2 #x.cbz", cbz_bytes())

        assert status == 201
        assert headers["X-Mokuro-Manifest"] == (
            "/catalog/api/manifest?series=S&volume=" + urllib.parse.quote("V 2 #x", safe="!*'()")
        )
        recheck = int(headers["X-Mokuro-Recheck-After"])
        # Priced from the plan: V1 (20 pages at 2 p/s) then V 2 #x, plus 10 s.
        assert 30 <= recheck < 300

    def test_exposed_through_cors(self, app: Any) -> None:
        _, headers, _ = put(
            app, "/mokuro-reader/S/V2.cbz", cbz_bytes(), Origin="https://reader.mokuro.app"
        )
        exposed = {h.strip() for h in headers["Access-Control-Expose-Headers"].split(",")}
        assert {"X-Mokuro-Manifest", "X-Mokuro-Recheck-After"} <= exposed

    def test_absent_when_every_layer_is_already_there(self, storage: Path, app: Any) -> None:
        series = storage / "library" / "S"
        (series / "V3.mokuro").write_text("{}", encoding="utf-8")
        (series / "V3.nova.mokuro").write_text("{}", encoding="utf-8")
        status, headers, _ = put(app, "/mokuro-reader/S/V3.cbz", cbz_bytes())
        assert status == 201
        assert "X-Mokuro-Manifest" not in headers
        assert "X-Mokuro-Recheck-After" not in headers

    def test_absent_without_an_ocr_worker(self, storage: Path) -> None:
        app = create_app(Config(storage=StorageConfig(base_path=storage)), ocr_control=OcrControl())
        status, headers, _ = put(app, "/mokuro-reader/S/V2.cbz", cbz_bytes())
        assert status == 201
        assert "X-Mokuro-Manifest" not in headers

    def test_held_for_want_of_a_machine_is_unpriced(self, storage: Path) -> None:
        worker = make_worker(storage, local_processing=False)
        control = OcrControl()
        control.worker = worker
        app = create_app(Config(storage=StorageConfig(base_path=storage)), ocr_control=control)
        assert worker.processing_hold() is not None

        status, headers, _ = put(app, "/mokuro-reader/S/V2.cbz", cbz_bytes())

        assert status == 201
        assert headers["X-Mokuro-Recheck-After"] == "300"
        # Nothing can run: the loop is not woken for it.
        assert not worker.wake_requested()

    def test_queueing_and_pricing_stay_cheap(self, app: Any, worker: OCRWorker, storage: Path) -> None:
        """What the upload adds to a PUT, on a 200-volume queue: milliseconds, no walk."""
        from mokuro_bunko.middleware import upload

        for index in range(200):
            folder = storage / "library" / f"Series {index % 20}"
            folder.mkdir(exist_ok=True)
            (folder / f"Vol {index}.cbz").write_bytes(b"x")
        worker.pending_jobs(max_age=0)
        timings: list[float] = []
        real = upload.UploadMiddleware._archive_arrived

        def timed(self: Any, cbz: Path, *, announce: bool) -> Any:
            started = time.perf_counter()
            try:
                return real(self, cbz, announce=announce)
            finally:
                timings.append(time.perf_counter() - started)

        with pytest.MonkeyPatch.context() as patch:
            patch.setattr(upload.UploadMiddleware, "_archive_arrived", timed)
            patch.setattr(worker, "_eligible_ocr_jobs", lambda *_a, **_k: pytest.fail("walked"))
            for index in range(5):
                status, headers, _ = put(app, f"/mokuro-reader/S/New {index}.cbz", cbz_bytes())
                assert status == 201
                assert "X-Mokuro-Recheck-After" in headers
        assert len(timings) == 5
        assert min(timings) < 0.05, timings

    def test_a_volume_far_down_the_queue_is_not_walked_for(
        self, app: Any, worker: OCRWorker, storage: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.middleware import upload

        monkeypatch.setattr(upload, "PUT_PRICE_MAX_ITEMS", 1)
        (storage / "library" / "S" / "V0.cbz").write_bytes(cbz_bytes())
        worker.pending_jobs(max_age=0)
        status, headers, _ = put(app, "/mokuro-reader/S/V9.cbz", cbz_bytes())
        assert status == 201
        assert headers["X-Mokuro-Recheck-After"] == "300"


class TestManifestPending:
    def manifest(self, app: Any, volume: str) -> dict[str, Any]:
        status, headers, body = call(
            app, "GET", "/catalog/api/manifest",
            query=urllib.parse.urlencode({"series": "S", "volume": volume}),
        )
        assert status == 200
        assert headers["Cache-Control"] == "no-store"
        return json.loads(body)

    def test_a_queued_volume(self, app: Any, worker: OCRWorker) -> None:
        worker.pending_jobs(max_age=0)
        manifest = self.manifest(app, "V1")

        (ocr, layer) = manifest["pending"]
        assert (ocr["kind"], ocr["id"]) == ("ocr", "mokuro")
        assert ocr["eta"] is not None and ocr["eta"].endswith("Z")
        # One lane: the layer is read after the primary (row order), though
        # it no longer waits for it.
        assert (layer["kind"], layer["id"]) == ("layer", "nova")
        assert layer["eta"] is not None and layer["eta"] > ocr["eta"]
        assert 30 <= manifest["recheck_after"] < 300

    def test_a_finished_volume(self, storage: Path, app: Any) -> None:
        series = storage / "library" / "S"
        (series / "V1.mokuro").write_text("{}", encoding="utf-8")
        (series / "V1.nova.mokuro.gz").write_bytes(b"gz")
        manifest = self.manifest(app, "V1")
        assert manifest["pending"] == []
        assert manifest["recheck_after"] is None

    def test_without_an_ocr_worker(self, storage: Path) -> None:
        app = create_app(Config(storage=StorageConfig(base_path=storage)), ocr_control=OcrControl())
        manifest = self.manifest(app, "V1")
        assert manifest["pending"] == []
        assert manifest["recheck_after"] is None


def uncompiled_worker(storage: Path) -> OCRWorker:
    """The metadata pass has compiled nothing: every page count comes from the zip."""
    return make_worker(storage, page_count_lookup=lambda _path: None)


def app_for(storage: Path, worker: OCRWorker) -> Any:
    control = OcrControl()
    control.worker = worker
    return create_app(Config(storage=StorageConfig(base_path=storage)), ocr_control=control)


def slow_walk(*_args: Any, **_kwargs: Any) -> tuple[list[Any], list[str]]:
    """A library walk that takes longer than a PUT will wait for (it runs on, in the background)."""
    time.sleep(0.5)
    return [], []


def write_running(storage: Path, **job: Any) -> None:
    card = {
        "status": "running", "done_pages": 0, "slot": 0, "machine": "local",
        "session_ready": True, "started_at": time.time(), **job,
    }
    (storage / ".ocr-progress.json").write_text(
        json.dumps({"active": True, "jobs": [card]}), encoding="utf-8"
    )


class TestPricedFromTheStart:
    """Live: a fresh 4-page upload on an idle queue read `eta: null` and
    `X-Mokuro-Recheck-After: 300` -- the metadata pass had no page count for
    it yet, and the pending snapshot had gone stale. Both are priced now."""

    def test_the_put_answers_a_priced_recheck(self, storage: Path) -> None:
        (storage / "library" / "S" / "V1.mokuro").write_text("{}", encoding="utf-8")
        (storage / "library" / "S" / "V1.nova.mokuro").write_text("{}", encoding="utf-8")
        worker = uncompiled_worker(storage)
        app = app_for(storage, worker)
        worker.pending_jobs(max_age=0)
        # A scan started since: the snapshot is stale, and a fresh list takes
        # longer than a PUT waits for one.
        worker._bump_queue_generation()
        worker._eligible_ocr_jobs = slow_walk  # type: ignore[method-assign]

        status, headers, _ = put(app, "/mokuro-reader/S/V2.cbz", cbz_bytes(4))

        assert status == 201
        # 5 s load + 4 pages at 2/s (+ the row's fill) + 10 s margin: the floor.
        assert headers["X-Mokuro-Recheck-After"] != "300"
        assert 30 <= int(headers["X-Mokuro-Recheck-After"]) < 300

    def test_with_no_snapshot_at_all(self, storage: Path) -> None:
        worker = uncompiled_worker(storage)
        app = app_for(storage, worker)
        worker._eligible_ocr_jobs = slow_walk  # type: ignore[method-assign]
        (storage / "library" / "S" / "V1.mokuro").write_text("{}", encoding="utf-8")
        (storage / "library" / "S" / "V1.nova.mokuro").write_text("{}", encoding="utf-8")

        status, headers, _ = put(app, "/mokuro-reader/S/V2.cbz", cbz_bytes(4))

        assert status == 201
        assert 30 <= int(headers["X-Mokuro-Recheck-After"]) < 300

    def test_the_manifest_prices_primary_and_layer_of_an_uncompiled_volume(
        self, storage: Path
    ) -> None:
        worker = uncompiled_worker(storage)
        app = app_for(storage, worker)
        status, _, body = call(
            app, "GET", "/catalog/api/manifest", query="series=S&volume=V1"
        )
        assert status == 200
        pending = json.loads(body)["pending"]
        assert [(e["kind"], e["id"]) for e in pending] == [("ocr", "mokuro"), ("layer", "nova")]
        assert all(e["eta"] is not None for e in pending), pending

    def test_a_running_card_without_a_page_count_is_priced(self, storage: Path) -> None:
        """Before the runner announces its length, the zip's own count stands in."""
        (storage / "library" / "S" / "V1.mokuro").write_text("{}", encoding="utf-8")
        worker = uncompiled_worker(storage)
        app = app_for(storage, worker)
        write_running(storage, series="S", volume="V1", generation="nova",
                      generation_id=ROWS[1].id)
        _, _, body = call(app, "GET", "/catalog/api/manifest", query="series=S&volume=V1")
        (layer,) = json.loads(body)["pending"]
        assert layer["eta"] is not None


class TestQueueStatusPriced:
    def test_no_null_eta_when_every_input_is_known(self, storage: Path) -> None:
        """The live case: primary done, one layer running, the next layer queued,
        and nothing compiled yet -- the queued layer read `eta_at: null, rough`."""
        from mokuro_bunko.queue.api import QueueAPI

        rows = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "nova", "engine": "hayai-nova", "detector": "ppocr-manga"},
                {"name": "paddle", "engine": "paddle-manga"},
            ]
        )
        (storage / "library" / "S" / "V1.mokuro").write_text("{}", encoding="utf-8")
        worker = OCRWorker(
            storage_path=storage, poll_interval=3600.0, generations=rows,
            engines_python_path=Path("/nonexistent"), sessions=False,
            page_count_lookup=lambda _path: None,
        )
        for row in rows:
            worker.rates.record_volume(row.id, 20, 10.0)
            worker.rates.record_volume(row.id, 20, 10.0)
            worker.rates.record_startup(row.id, 5.0)
        worker._inflight_ocr.add((storage / "library" / "S" / "V1.cbz", rows[1].id))
        write_running(storage, series="S", volume="V1", generation="nova",
                      generation_id=rows[1].id)
        control = OcrControl()
        control.worker = worker
        api = QueueAPI(lambda e, s: [], storage_base_path=str(storage), ocr_control=control)

        status = api.raw_status()

        (queued,) = status["pending_ocr"]
        assert queued["generation"] == "paddle"
        assert queued["eta_at"] is not None
        assert queued["rough"] is False
        (running,) = status["current_jobs"]
        assert running["eta_at"] is not None
        assert status["queue_done_at"] is not None
