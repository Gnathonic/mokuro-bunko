"""Unit tests for OCR queue API access and config."""

from __future__ import annotations

import base64
import io
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

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
        return self.request("GET", path, headers=headers)

    def post(
        self,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes = b"",
    ) -> WSGIResponse:
        return self.request("POST", path, headers=headers, body=body)

    def request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        body: bytes = b"",
    ) -> WSGIResponse:
        headers = headers or {}
        path_part, _, query = path.partition("?")
        environ = {
            "REQUEST_METHOD": method,
            "PATH_INFO": path_part,
            "QUERY_STRING": query,
            "SERVER_NAME": "localhost",
            "SERVER_PORT": "8080",
            "SERVER_PROTOCOL": "HTTP/1.1",
            "wsgi.version": (1, 0),
            "wsgi.url_scheme": "http",
            "wsgi.input": io.BytesIO(body),
            "wsgi.errors": io.StringIO(),
            "wsgi.multithread": False,
            "wsgi.multiprocess": False,
            "wsgi.run_once": False,
            "CONTENT_LENGTH": str(len(body)),
            "CONTENT_TYPE": headers.get("Content-Type", "application/json"),
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


def test_queue_ocr_status_includes_ocr_worker_state(temp_dir: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)

    class MockWorker:
        def __init__(self):
            self.is_running = True
            self._paused = False

        def is_paused(self):
            return self._paused

    worker = MockWorker()
    monkeypatch.setattr(
        "mokuro_bunko.ocr.watcher.CURRENT_OCR_WORKER",
        worker,
    )

    queue_cfg = type("Cfg", (), {"show_in_nav": False, "public_access": True})()
    app = QueueAPI(dummy_app, storage_base_path=str(storage), queue_config=queue_cfg)
    client = WSGITestClient(app)

    response = client.get("/queue/api/ocr")
    assert response.status_code == 200
    data = response.json()
    assert data["ocr_worker"]["available"] is True
    assert data["ocr_worker"]["active"] is True
    assert data["ocr_worker"]["paused"] is False


def test_queue_history_rejects_non_finite_since(temp_dir: Path) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)

    queue_cfg = type("Cfg", (), {"show_in_nav": False, "public_access": True})()
    app = QueueAPI(dummy_app, storage_base_path=str(storage), queue_config=queue_cfg)
    client = WSGITestClient(app)

    response = client.get("/queue/api/ocr?since=nan")
    assert response.status_code == 400

def test_queue_ocr_control_rejects_non_object_json(temp_dir: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    db = Database(temp_dir / "test.db")
    db.create_user("admin", "password123", "admin")

    class MockWorker:
        def __init__(self):
            self.is_running = True
            self._paused = False

        def pause(self):
            self._paused = True

        def resume(self):
            self._paused = False

        def is_paused(self):
            return self._paused

    worker = MockWorker()
    monkeypatch.setattr(
        "mokuro_bunko.ocr.watcher.CURRENT_OCR_WORKER",
        worker,
    )

    queue_cfg = type("Cfg", (), {"show_in_nav": False, "public_access": True})()
    app = QueueAPI(
        dummy_app,
        storage_base_path=str(storage),
        queue_config=queue_cfg,
        database=db,
    )
    client = WSGITestClient(app)

    response = client.post(
        "/queue/api/ocr/control",
        headers={
            "Content-Type": "application/json",
            "Authorization": make_auth_header("admin", "password123"),
        },
        body=b'["pause"]',
    )
    assert response.status_code == 400

def test_queue_ocr_control_rejects_non_string_action(temp_dir: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    db = Database(temp_dir / "test.db")
    db.create_user("admin", "password123", "admin")

    class MockWorker:
        def __init__(self):
            self.is_running = True
            self._paused = False

        def pause(self):
            self._paused = True

        def resume(self):
            self._paused = False

        def is_paused(self):
            return self._paused

    worker = MockWorker()
    monkeypatch.setattr(
        "mokuro_bunko.ocr.watcher.CURRENT_OCR_WORKER",
        worker,
    )

    queue_cfg = type("Cfg", (), {"show_in_nav": False, "public_access": True})()
    app = QueueAPI(
        dummy_app,
        storage_base_path=str(storage),
        queue_config=queue_cfg,
        database=db,
    )
    client = WSGITestClient(app)

    response = client.post(
        "/queue/api/ocr/control",
        headers={
            "Content-Type": "application/json",
            "Authorization": make_auth_header("admin", "password123"),
        },
        body=json.dumps({"action": 123}).encode("utf-8"),
    )
    assert response.status_code == 400

def test_queue_ocr_control_pause_resume(temp_dir: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    storage = temp_dir / "storage"
    storage.mkdir(parents=True)
    db = Database(temp_dir / "test.db")
    db.create_user("admin", "password123", "admin")

    class MockWorker:
        def __init__(self):
            self.is_running = True
            self._paused = False

        def pause(self):
            self._paused = True

        def resume(self):
            self._paused = False

        def is_paused(self):
            return self._paused

    worker = MockWorker()
    monkeypatch.setattr(
        "mokuro_bunko.ocr.watcher.CURRENT_OCR_WORKER",
        worker,
    )

    queue_cfg = type("Cfg", (), {"show_in_nav": False, "public_access": True})()
    app = QueueAPI(
        dummy_app,
        storage_base_path=str(storage),
        queue_config=queue_cfg,
        database=db,
    )
    client = WSGITestClient(app)

    unauthorized = client.post(
        "/queue/api/ocr/control",
        headers={"Content-Type": "application/json"},
        body=json.dumps({"action": "pause"}).encode("utf-8"),
    )
    assert unauthorized.status_code == 403

    response1 = client.post(
        "/queue/api/ocr/control",
        headers={
            "Content-Type": "application/json",
            "Authorization": make_auth_header("admin", "password123"),
        },
        body=json.dumps({"action": "pause"}).encode("utf-8"),
    )
    assert response1.status_code == 200
    assert worker.is_paused() is True

    response2 = client.post(
        "/queue/api/ocr/control",
        headers={
            "Content-Type": "application/json",
            "Authorization": make_auth_header("admin", "password123"),
        },
        body=json.dumps({"action": "resume"}).encode("utf-8"),
    )
    assert response2.status_code == 200
    assert worker.is_paused() is False
