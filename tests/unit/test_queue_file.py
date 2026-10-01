"""`/mokuro-reader/.mokuro-queue.json`: the whole pending OCR queue, per volume.

One virtual file the reader polls instead of a timer per volume. Built from
the same priced plan as the manifest (layers chained after their primary,
new uploads priced at once); read with a library file's rules; hidden from
every listing; never written.
"""

from __future__ import annotations

import base64
import json
import time
import urllib.parse
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, CorsConfig, RegistrationConfig, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.middleware import auth as auth_module
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.security import AuthAttemptLimiter
from mokuro_bunko.server import create_app
from tests.unit.test_upload_enqueue import call, cbz_bytes

QUEUE = "/mokuro-reader/.mokuro-queue.json"
ROWS = parse_generation_list(
    [
        {"name": "mokuro-fp16", "engine": "mokuro", "primary": True},
        {"name": "hayai-nova-ppocr", "engine": "hayai-nova", "detector": "ppocr-manga"},
    ]
)
READER = {"Authorization": "Basic " + base64.b64encode(b"reader:pass1234").decode()}
EDITOR = {"Authorization": "Basic " + base64.b64encode(b"editor:pass1234").decode()}
WRONG = {"Authorization": "Basic " + base64.b64encode(b"reader:nope").decode()}


@pytest.fixture(autouse=True)
def _private_rate_limiter(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(auth_module, "AUTH_RATE_LIMITER", AuthAttemptLimiter())


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    (base / "library" / "Dr Stone").mkdir(parents=True)
    (base / "inbox").mkdir()
    (base / "users").mkdir()
    for name in ("Dr Stone 01", "Dr Stone 02"):
        (base / "library" / "Dr Stone" / f"{name}.cbz").write_bytes(cbz_bytes(4))
    db = Database(base / "mokuro.db")
    db.create_user("reader", "pass1234", "registered")
    db.create_user("editor", "pass1234", "editor")
    return base


def make_worker(storage: Path, **kwargs: Any) -> OCRWorker:
    worker = OCRWorker(
        storage_path=storage,
        poll_interval=3600.0,
        generations=ROWS,
        engines_python_path=Path("/nonexistent"),
        sessions=False,
        page_count_lookup=lambda _path: 20,
        **kwargs,
    )
    for row in ROWS:
        worker.rates.record_volume(row.id, 20, 10.0)
        worker.rates.record_volume(row.id, 20, 10.0)
        worker.rates.record_startup(row.id, 5.0)
    return worker


def build(storage: Path, worker: OCRWorker | None, **config: Any) -> Any:
    control = OcrControl()
    if worker is not None:
        control.worker = worker
    app = create_app(Config(storage=StorageConfig(base_path=storage), **config), ocr_control=control)
    app.test_control = control
    return app


@pytest.fixture
def worker(storage: Path) -> OCRWorker:
    return make_worker(storage)


@pytest.fixture
def app(storage: Path, worker: OCRWorker) -> Any:
    return build(storage, worker)


def get(app: Any, headers: dict[str, str] | None = None, method: str = "GET") -> tuple[int, dict[str, str], bytes]:
    return call(app, method, QUEUE, headers=headers)


def body_of(app: Any, headers: dict[str, str] | None = None) -> dict[str, Any]:
    status, response_headers, raw = get(app, headers)
    assert status == 200, raw
    assert response_headers["Content-Type"] == "application/json"
    assert response_headers["Cache-Control"] == "no-cache"
    return json.loads(raw)


def write_running(storage: Path, **job: Any) -> None:
    card = {"status": "running", "slot": 0, "machine": "local", "processor": "tower (RTX 4090)",
            "session_ready": True, "started_at": time.time(), **job}
    (storage / ".ocr-progress.json").write_text(
        json.dumps({"active": True, "jobs": [card]}), encoding="utf-8"
    )


class TestTheBody:
    def test_every_pending_volume_with_its_jobs(self, app: Any, worker: OCRWorker) -> None:
        body = body_of(app)

        assert body["version"] == 1
        assert body["held"] is None
        assert body["generated_at"].endswith("Z")
        assert [v["volume"] for v in body["volumes"]] == ["Dr Stone 01", "Dr Stone 02"]
        first = body["volumes"][0]
        assert first["series"] == "Dr Stone"
        assert first["path"] == "/mokuro-reader/Dr%20Stone/Dr%20Stone%2001.cbz"
        assert first["manifest"] == "/catalog/api/manifest?series=Dr%20Stone&volume=Dr%20Stone%2001"
        # The primary is queued; the layer waits for it -- listed (the queue
        # page does not) and priced after it.
        (ocr, layer) = first["jobs"]
        assert (ocr["kind"], ocr["id"], ocr["state"], ocr["progress"]) == (
            "ocr", "mokuro-fp16", "queued", None,
        )
        assert (layer["kind"], layer["id"], layer["state"]) == ("layer", "hayai-nova-ppocr", "queued")
        assert ocr["eta"] and layer["eta"] and layer["eta"] > ocr["eta"]
        assert 30 <= body["next_check_after"] <= 3600

    def test_a_running_job_carries_its_progress(self, storage: Path, app: Any, worker: OCRWorker) -> None:
        write_running(storage, series="Dr Stone", volume="Dr Stone 01", generation="mokuro-fp16",
                      generation_id=ROWS[0].id, done_pages=8, total_pages=20,
                      first_page_at=time.time() - 4)
        worker._inflight_ocr.add((storage / "library" / "Dr Stone" / "Dr Stone 01.cbz", ROWS[0].id))

        body = body_of(app)

        first = next(v for v in body["volumes"] if v["volume"] == "Dr Stone 01")
        running = first["jobs"][0]
        assert (running["state"], running["progress"]) == ("running", 0.4)
        assert running["eta"] is not None
        assert first["jobs"][1]["state"] == "queued"

    def test_it_agrees_with_the_manifest(self, app: Any, worker: OCRWorker) -> None:
        worker.pending_jobs(max_age=0)
        queue = body_of(app)
        for volume in queue["volumes"]:
            status, _, raw = call(app, "GET", "/catalog/api/manifest", query=urllib.parse.urlencode(
                {"series": volume["series"], "volume": volume["volume"]}))
            assert status == 200
            pending = json.loads(raw)["pending"]
            assert [(j["kind"], j["id"], j["eta"]) for j in volume["jobs"]] == [
                (p["kind"], p["id"], p["eta"]) for p in pending
            ]

    def test_next_check_after_follows_the_recheck_rule(self, app: Any) -> None:
        body = body_of(app)
        etas = [j["eta"] for v in body["volumes"] for j in v["jobs"] if j["eta"]]
        from datetime import datetime

        earliest = min(datetime.fromisoformat(e.replace("Z", "+00:00")).timestamp() for e in etas)
        expected = max(30, min(3600, int(earliest - time.time()) + 10))
        assert abs(body["next_check_after"] - expected) <= 2

    def test_an_empty_queue(self, storage: Path) -> None:
        for name in ("Dr Stone 01", "Dr Stone 02"):
            (storage / "library" / "Dr Stone" / f"{name}.mokuro").write_text("{}", encoding="utf-8")
            (storage / "library" / "Dr Stone" / f"{name}.hayai-nova-ppocr.mokuro").write_text(
                "{}", encoding="utf-8")
        body = body_of(build(storage, make_worker(storage)))
        assert body["volumes"] == []
        assert body["next_check_after"] is None
        assert body["held"] is None

    def test_without_an_ocr_worker_nothing_is_pending(self, storage: Path) -> None:
        body = body_of(build(storage, None))
        assert body["volumes"] == [] and body["next_check_after"] is None

    def test_no_machine_names_or_errors(self, storage: Path, app: Any, worker: OCRWorker) -> None:
        write_running(storage, series="Dr Stone", volume="Dr Stone 01", generation="mokuro-fp16",
                      generation_id=ROWS[0].id, done_pages=1, total_pages=4, error="boom",
                      machine="tower")
        (storage / ".ocr-failures.json").write_text(json.dumps({
            "Dr Stone/Dr Stone 02.cbz@x": {"error": "CUDA out of memory", "attempts": 1,
                                           "last_attempt_at": time.time()},
        }), encoding="utf-8")
        _, _, raw = get(app)
        text = raw.decode()
        for secret in ("tower", "RTX", "boom", "CUDA", "error", "machine", "processor"):
            assert secret not in text, secret
        job_keys = {k for v in json.loads(raw)["volumes"] for j in v["jobs"] for k in j}
        assert job_keys <= {"kind", "id", "state", "eta", "progress"}


class TestHeld:
    def test_no_processor(self, storage: Path) -> None:
        worker = make_worker(storage, local_processing=False)
        body = body_of(build(storage, worker))
        assert body["held"] == {"reason": "no-processor"}
        states = {j["state"] for v in body["volumes"] for j in v["jobs"]}
        assert states == {"held"}
        assert all(j["eta"] is None for v in body["volumes"] for j in v["jobs"])
        assert body["next_check_after"] == 300

    def test_benchmarking(self, storage: Path, worker: OCRWorker) -> None:
        app = build(storage, worker)
        control = app.test_control

        class Bench:
            def paused_for_benchmark(self) -> dict[str, Any]:
                return {"key": "g-2", "generation": "x", "queued": 0, "processor": "local"}

        control.bench = Bench()
        worker._holds["local"] = 1
        body = body_of(app)
        assert body["held"] == {"reason": "benchmarking"}
        assert {j["state"] for v in body["volumes"] for j in v["jobs"]} == {"held"}

    def test_paused(self, storage: Path, worker: OCRWorker) -> None:
        app = build(storage, worker)
        worker._holds["local"] = 1
        assert body_of(app)["held"] == {"reason": "paused"}


class TestETag:
    def test_stable_until_the_queue_changes_and_304(self, storage: Path, app: Any, worker: OCRWorker) -> None:
        status, first, _ = get(app)
        time.sleep(1.1)
        _, again, _ = get(app)
        assert first["ETag"] == again["ETag"]
        assert first["ETag"].startswith('"') and not first["ETag"].startswith("W/")

        status, headers, raw = get(app, {"If-None-Match": first["ETag"]})
        assert status == 304 and raw == b""
        assert headers["ETag"] == first["ETag"]

        (storage / "library" / "Dr Stone" / "Dr Stone 02.mokuro").write_text("{}", encoding="utf-8")
        worker._bump_queue_generation()
        time.sleep(2.1)
        _, changed, _ = get(app)
        assert changed["ETag"] != first["ETag"]
        status, _, _ = get(app, {"If-None-Match": first["ETag"]})
        assert status == 200

    def test_gzip_when_accepted_with_its_own_etag(self, app: Any) -> None:
        import gzip

        _, plain, raw = get(app)
        status, packed, zipped = get(app, {"Accept-Encoding": "gzip, deflate"})
        assert status == 200
        assert packed["Content-Encoding"] == "gzip"
        assert json.loads(gzip.decompress(zipped)) == json.loads(raw)
        assert packed["ETag"] != plain["ETag"]
        status, _, _ = get(app, {"Accept-Encoding": "gzip", "If-None-Match": packed["ETag"]})
        assert status == 304

    def test_head(self, app: Any) -> None:
        status, headers, raw = get(app, method="HEAD")
        assert status == 200 and raw == b""
        assert headers["ETag"]


class TestAccessAndCors:
    @pytest.mark.parametrize("anonymous_download", [True, False])
    @pytest.mark.parametrize("who", ["anonymous", "reader", "wrong"])
    def test_same_as_a_library_get(
        self, storage: Path, worker: OCRWorker, anonymous_download: bool, who: str
    ) -> None:
        app = build(storage, worker,
                    registration=RegistrationConfig(allow_anonymous_download=anonymous_download))
        headers = {"anonymous": {}, "reader": READER, "wrong": WRONG}[who]
        library = call(app, "GET", "/mokuro-reader/Dr Stone/Dr Stone 01.cbz", headers=headers)
        queue = get(app, headers)
        assert queue[0] == library[0]
        assert ("WWW-Authenticate" in queue[1]) == ("WWW-Authenticate" in library[1])

    @pytest.mark.parametrize("origin", ["https://reader.mokuro.app", "https://evil.example"])
    def test_cors_like_a_library_file(self, storage: Path, worker: OCRWorker, origin: str) -> None:
        app = build(storage, worker, cors=CorsConfig())
        library = call(app, "GET", "/mokuro-reader/Dr Stone/Dr Stone 01.cbz", headers={"Origin": origin})
        queue = get(app, {"Origin": origin})

        def cors(h: dict[str, str]) -> dict[str, str]:
            return {k: v for k, v in h.items() if k.startswith("Access-Control-")}

        assert cors(queue[1]) == cors(library[1])
        if origin.startswith("https://reader"):
            assert "ETag" in queue[1]["Access-Control-Expose-Headers"]


class TestNeverAFile:
    @pytest.mark.parametrize("depth", ["1", "infinity"])
    def test_hidden_from_propfind(self, app: Any, depth: str) -> None:
        status, _, raw = call(app, "PROPFIND", "/mokuro-reader/", headers={**READER, "Depth": depth})
        assert status == 207
        assert b".mokuro-queue.json" not in raw
        assert b"Dr%20Stone" in raw or b"Dr Stone" in raw
        if depth == "infinity":
            assert b"Dr%20Stone%2001.cbz" in raw or b"Dr Stone 01.cbz" in raw

    @pytest.mark.parametrize("method", ["PUT", "DELETE", "MOVE", "COPY", "PROPPATCH", "LOCK", "MKCOL"])
    def test_writes_are_405(self, storage: Path, app: Any, method: str) -> None:
        headers = {**EDITOR, "Destination": "/mokuro-reader/elsewhere.json"}
        status, response_headers, _ = call(app, method, QUEUE, b"{}", headers)
        assert status == 405
        assert "GET" in response_headers.get("Allow", "")
        assert not (storage / "library" / ".mokuro-queue.json").exists()

    @pytest.mark.parametrize("method", ["MOVE", "COPY"])
    def test_nothing_is_moved_onto_it(self, storage: Path, app: Any, method: str) -> None:
        status, _, _ = call(app, method, "/mokuro-reader/Dr Stone/Dr Stone 01.cbz", headers={
            **EDITOR, "Destination": QUEUE,
        })
        assert status == 405
        assert not (storage / "library" / ".mokuro-queue.json").exists()


class TestScale:
    def test_two_thousand_jobs(self, storage: Path) -> None:
        import shutil

        library = storage / "library"
        shutil.rmtree(library / "Dr Stone")
        for index in range(1000):
            folder = library / f"Series {index % 50:02d}"
            folder.mkdir(exist_ok=True)
            (folder / f"Vol {index:04d}.cbz").write_bytes(b"x")
            # Primary done: both of the next rows' jobs are queued, 2 a volume.
        rows = parse_generation_list([
            {"name": "mokuro-fp16", "engine": "mokuro", "primary": True},
            {"name": "hayai-nova-ppocr", "engine": "hayai-nova", "detector": "ppocr-manga"},
            {"name": "paddle", "engine": "paddle-manga"},
        ])
        for folder in library.iterdir():
            for cbz in folder.glob("*.cbz"):
                cbz.with_suffix(".mokuro").write_text("{}", encoding="utf-8")
        worker = OCRWorker(storage_path=storage, poll_interval=3600.0, generations=rows,
                           engines_python_path=Path("/nonexistent"), sessions=False,
                           page_count_lookup=lambda _path: 200)
        for row in rows:
            worker.rates.record_volume(row.id, 200, 100.0)
        app = build(storage, worker)
        worker.pending_jobs(max_age=0)
        from mokuro_bunko.middleware import queue_file

        started = time.perf_counter()
        raw = queue_file.build_body(app.test_control, storage)
        seconds = time.perf_counter() - started
        import gzip

        packed = len(gzip.compress(raw, compresslevel=6))
        body = json.loads(raw)
        jobs = sum(len(v["jobs"]) for v in body["volumes"])
        # A horizon of the queue, and the whole queue's count: the next
        # hundred waiting volumes (two jobs each) of the thousand.
        assert len(body["volumes"]) == 100
        assert jobs == 200
        assert body["pending_volumes"] == 1000
        print(f"\n[queue file] {jobs} jobs: built in {seconds * 1000:.0f} ms, "
              f"{len(raw)} bytes, {packed} gzipped")
        assert seconds < 2.0
