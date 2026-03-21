"""Web-level tests for Queue page UI markup and OCR endpoints."""

from __future__ import annotations

import base64
import io
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, QueueConfig, ServerConfig, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app


def _auth_header(username: str, password: str) -> str:
    token = base64.b64encode(f"{username}:{password}".encode()).decode("utf-8")
    return f"Basic {token}"


def _call_app(
    app: Callable[..., Any],
    method: str,
    path: str,
    *,
    headers: dict[str, str] | None = None,
    body: bytes = b"",
) -> tuple[str, dict[str, str], bytes]:
    req_headers = headers or {}
    path_part, _, query = path.partition("?")
    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "SCRIPT_NAME": "",
        "PATH_INFO": path_part,
        "QUERY_STRING": query,
        "CONTENT_TYPE": req_headers.get("Content-Type", "application/json"),
        "CONTENT_LENGTH": str(len(body)),
        "SERVER_NAME": "test",
        "SERVER_PORT": "80",
        "SERVER_PROTOCOL": "HTTP/1.1",
        "wsgi.version": (1, 0),
        "wsgi.url_scheme": "http",
        "wsgi.input": io.BytesIO(body),
        "wsgi.errors": io.StringIO(),
        "wsgi.multithread": True,
        "wsgi.multiprocess": True,
        "wsgi.run_once": False,
    }

    for key, value in req_headers.items():
        key_upper = key.upper().replace("-", "_")
        if key_upper not in ("CONTENT_TYPE", "CONTENT_LENGTH"):
            environ[f"HTTP_{key_upper}"] = value

    status: list[str] = []
    response_headers: list[tuple[str, str]] = []

    def start_response(s: str, h: list[tuple[str, str]], exc_info: Any = None) -> Callable[[bytes], None]:
        status.append(s)
        response_headers.extend(h)
        return lambda data: None

    body_bytes = b"".join(app(environ, start_response))
    return status[0], dict(response_headers), body_bytes


@pytest.fixture
def app(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Callable[..., Any]:
    storage = tmp_path / "storage"
    storage.mkdir(parents=True)

    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")

    class MockWorker:
        is_running = True

        def __init__(self) -> None:
            self._paused = False

        def pause(self) -> None:
            self._paused = True

        def resume(self) -> None:
            self._paused = False

        def is_paused(self) -> bool:
            return self._paused

    monkeypatch.setattr("mokuro_bunko.ocr.watcher.CURRENT_OCR_WORKER", MockWorker())

    config = Config(
        server=ServerConfig(host="127.0.0.1", port=0),
        storage=StorageConfig(base_path=storage),
        queue=QueueConfig(show_in_nav=True, public_access=True),
    )
    return create_app(config)


class TestQueuePageWeb:
    def test_queue_page_contains_controls_and_history_filters(self, app: Callable[..., Any]) -> None:
        status, _headers, body = _call_app(app, "GET", "/queue")

        assert status.startswith("200")
        html = body.decode("utf-8")
        assert "id=\"ocr-pause-btn\"" in html
        assert "id=\"ocr-resume-btn\"" in html
        assert "id=\"history-status\"" in html
        assert "id=\"history-series\"" in html
        assert "id=\"history-limit\"" in html

    def test_queue_ocr_filtered_history_endpoint_works(self, app: Callable[..., Any]) -> None:
        status, _headers, body = _call_app(app, "GET", "/queue/api/ocr?status=done&limit=5")

        assert status.startswith("200")
        payload = json.loads(body)
        assert "ocr_worker" in payload
        assert "history" in payload

    def test_queue_ocr_control_requires_admin(self, app: Callable[..., Any]) -> None:
        status, _headers, _body = _call_app(
            app,
            "POST",
            "/queue/api/ocr/control",
            headers={"Content-Type": "application/json"},
            body=json.dumps({"action": "pause"}).encode("utf-8"),
        )
        assert status.startswith("403")

        status2, _headers2, _body2 = _call_app(
            app,
            "POST",
            "/queue/api/ocr/control",
            headers={
                "Content-Type": "application/json",
                "Authorization": _auth_header("admin", "adminpass"),
            },
            body=json.dumps({"action": "pause"}).encode("utf-8"),
        )
        assert status2.startswith("200")
