"""Unit tests for OCR queue API access and config."""

from __future__ import annotations

import base64
import io
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

from mokuro_bunko.database import Database
from mokuro_bunko.queue.api import QueueAPI


def make_auth_header(username: str, password: str) -> str:
    credentials = base64.b64encode(f"{username}:{password}".encode()).decode()
    return f"Basic {credentials}"


class WSGIResponse:
    def __init__(self) -> None:
        self.status = ""
        self.headers: list[tuple[str, str]] = []
        self.content = b""

    def start_response(
        self,
        status: str,
        headers: list[tuple[str, str]],
        exc_info: Any = None,
    ) -> Callable[[bytes], None]:
        self.status = status
        self.headers = headers
        return lambda data: None

    @property
    def status_code(self) -> int:
        return int(self.status.split()[0])

    def json(self) -> dict[str, Any]:
        return json.loads(self.content.decode("utf-8"))


class WSGITestClient:
    def __init__(self, app: Callable[..., Any]) -> None:
        self.app = app

    def get(self, path: str, headers: dict[str, str] | None = None) -> WSGIResponse:
        headers = headers or {}
        environ = {
            "REQUEST_METHOD": "GET",
            "PATH_INFO": path,
            "QUERY_STRING": "",
            "SERVER_NAME": "localhost",
            "SERVER_PORT": "8080",
            "SERVER_PROTOCOL": "HTTP/1.1",
            "wsgi.version": (1, 0),
            "wsgi.url_scheme": "http",
            "wsgi.input": io.BytesIO(b""),
            "wsgi.errors": io.StringIO(),
            "wsgi.multithread": False,
            "wsgi.multiprocess": False,
            "wsgi.run_once": False,
            "CONTENT_LENGTH": "0",
            "CONTENT_TYPE": "application/json",
        }
        for key, value in headers.items():
            key_upper = key.upper().replace("-", "_")
            if key_upper not in ("CONTENT_TYPE", "CONTENT_LENGTH"):
                environ[f"HTTP_{key_upper}"] = value

        response = WSGIResponse()
        result = self.app(environ, response.start_response)
        response.content = b"".join(result)
        return response


def dummy_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b"Not found"]


def test_queue_config_endpoint(temp_dir: Path) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)

    queue_cfg = type("Cfg", (), {"show_in_nav": True, "public_access": False})()
    app = QueueAPI(dummy_app, storage_base_path=str(storage), queue_config=queue_cfg)
    client = WSGITestClient(app)

    response = client.get("/queue/api/config")
    assert response.status_code == 200
    data = response.json()
    assert data["show_in_nav"] is True
    assert data["public_access"] is False


def test_private_queue_status_requires_auth(temp_dir: Path) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    db = Database(temp_dir / "test.db")
    db.create_user("alice", "password123", "registered")

    queue_cfg = type("Cfg", (), {"show_in_nav": True, "public_access": False})()
    app = QueueAPI(
        dummy_app,
        storage_base_path=str(storage),
        queue_config=queue_cfg,
        database=db,
    )
    client = WSGITestClient(app)

    unauthorized = client.get("/queue/api/status")
    assert unauthorized.status_code == 401

    authorized = client.get(
        "/queue/api/status",
        headers={"Authorization": make_auth_header("alice", "password123")},
    )
    assert authorized.status_code == 200


def test_public_queue_status_allows_anonymous(temp_dir: Path) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)

    queue_cfg = type("Cfg", (), {"show_in_nav": False, "public_access": True})()
    app = QueueAPI(dummy_app, storage_base_path=str(storage), queue_config=queue_cfg)
    client = WSGITestClient(app)

    response = client.get("/queue/api/status")
    assert response.status_code == 200


# -- the running job's stage readout ---------------------------------------


def _progress(storage: Path, job: dict[str, Any]) -> None:
    (storage / ".ocr-progress.json").write_text(json.dumps(job), encoding="utf-8")


def _running_job(**extra: Any) -> dict[str, Any]:
    job = {
        "active": True,
        "series": "S",
        "volume": "V 01",
        "engine": "ppocr-manga",
        "percent": 40,
        "eta_seconds": 90,
        "done_pages": 16,
        "total_pages": 40,
        "status": "running",
    }
    job.update(extra)
    return job


def _status(storage: Path, display: str = "detailed") -> dict[str, Any]:
    queue_cfg = type(
        "Cfg", (), {"show_in_nav": False, "public_access": True, "display": display}
    )()
    client = WSGITestClient(
        QueueAPI(dummy_app, storage_base_path=str(storage), queue_config=queue_cfg)
    )
    return client.get("/queue/api/status").json()


def test_status_carries_the_stage_readout(temp_dir: Path) -> None:
    """What the worker wrote about the pipeline reaches the page intact."""
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    readout = {
        "elapsed_seconds": 120.0,
        "items": 16,
        "stages": [
            {
                "key": "detect",
                "name": "detect + CTC read",
                "device": "cpu",
                "workers": 2,
                "fused": False,
                "items": 16,
                "busy_pct": 94.0,
                "blocked_pct": 0.0,
                "starved_pct": 1.0,
                "queue": {
                    "name": "detect->engine",
                    "capacity": 4,
                    "mean_depth": 0.1,
                    "max_depth": 1,
                    "fill_pct": 2.5,
                },
            }
        ],
        "bottleneck": "detect",
        "verdict": "engine starved 38% waiting on detect — widen detect",
    }
    _progress(storage, _running_job(pipeline=readout))

    data = _status(storage)

    sent = data["machines"][0]["jobs"][0]["pipeline"]
    # Rebuilt field by field: the readout's own fields, nothing else.
    assert sent["verdict"] == readout["verdict"]
    assert sent["bottleneck"] == "detect"
    (stage,) = sent["stages"]
    assert stage["busy_pct"] == 94.0 and stage["device"] == "cpu"
    assert stage["queue"] == {"name": "detect->engine", "capacity": 4,
                              "mean_depth": 0.1, "max_depth": 1}
    assert "elapsed_seconds" not in sent and "items" not in stage


def test_the_readout_is_a_detailed_level_field(temp_dir: Path) -> None:
    """At `normal` the pipeline is not rendered, so it is not sent either."""
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    _progress(storage, _running_job(pipeline={"stages": [{"key": "detect"}]}))
    for level in ("minimal", "normal"):
        job = _status(storage, level)["machines"][0]["jobs"][0]
        assert "pipeline" not in job


def test_status_omits_the_readout_when_the_worker_sends_none(temp_dir: Path) -> None:
    """An older worker, the mokuro engine, or a run too young to have numbers."""
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    _progress(storage, _running_job())

    job = _status(storage)["machines"][0]["jobs"][0]

    assert job["pipeline"] is None
    assert job["percent"] == 40


def test_status_drops_a_readout_that_is_not_one(temp_dir: Path) -> None:
    """Junk in the progress file must not reach the page as a stage list."""
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    for junk in ("detect", [], {}, {"stages": []}, None):
        _progress(storage, _running_job(pipeline=junk))
        assert _status(storage)["machines"][0]["jobs"][0]["pipeline"] is None


# -- paused for a benchmark -------------------------------------------------


def test_paused_for_benchmark_is_null_without_an_ocr_control(temp_dir: Path) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    assert _status(storage)["paused_for_benchmark"] is None


def test_paused_for_benchmark_reads_through_the_shared_control_handle(
    temp_dir: Path,
) -> None:
    """`QueueAPI` has no bench state of its own -- it reads the admin API's.

    Uses the REAL `OcrControl` (with no worker, so `pending_jobs` etc. all
    degrade the way they already do) and only stands in for the
    `BenchService` it would otherwise lazily build, the same handle the
    admin API sets on `ocr_control.bench` the first time it is asked for one.
    """
    from mokuro_bunko.ocr.control import OcrControl

    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    queue_cfg = type("Cfg", (), {"show_in_nav": False, "public_access": True})()
    control = OcrControl()
    control.bench = type(  # type: ignore[assignment]
        "FakeBench",
        (),
        {
            "paused_for_benchmark": lambda self: {
                "key": "g-2",
                "generation": "hayai-nova",
                "queued": 2,
            }
        },
    )()
    app = QueueAPI(
        dummy_app, storage_base_path=str(storage), queue_config=queue_cfg, ocr_control=control
    )
    client = WSGITestClient(app)
    data = client.get("/queue/api/status").json()
    # A visitor's copy: picked field by field, and the bench key left out.
    assert data["paused_for_benchmark"] == {
        "generation": "hayai-nova",
        "queued": 2,
        "processor": None,
    }
