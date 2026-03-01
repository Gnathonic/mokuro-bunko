"""Unit tests for OCR queue API access and config."""

from __future__ import annotations

import base64
import io
import json
from pathlib import Path
from typing import Any, Callable

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
