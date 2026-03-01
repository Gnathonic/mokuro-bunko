"""Unit tests for setup wizard API security behavior."""

from __future__ import annotations

import io
import json
from pathlib import Path
from typing import Any, Callable

import pytest

from mokuro_bunko.config import Config, RegistrationConfig, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.setup.api import SetupWizardAPI


class WSGIResponse:
    """WSGI response wrapper."""

    def __init__(self) -> None:
        self.status: str = ""
        self.headers: list[tuple[str, str]] = []
        self.content: bytes = b""

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


def dummy_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    """Dummy wrapped app."""
    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b"Not found"]


def make_request(
    app: SetupWizardAPI,
    method: str,
    path: str,
    *,
    remote_addr: str,
    headers: dict[str, str] | None = None,
    json_body: dict[str, Any] | None = None,
) -> WSGIResponse:
    """Send a simple WSGI request."""
    content = b""
    req_headers = dict(headers or {})
    if json_body is not None:
        content = json.dumps(json_body).encode("utf-8")
        req_headers["Content-Type"] = "application/json"

    environ = {
        "REQUEST_METHOD": method,
        "SCRIPT_NAME": "",
        "PATH_INFO": path,
        "QUERY_STRING": "",
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
        "SERVER_PROTOCOL": "HTTP/1.1",
        "REMOTE_ADDR": remote_addr,
        "wsgi.version": (1, 0),
        "wsgi.url_scheme": "http",
        "wsgi.input": io.BytesIO(content),
        "wsgi.errors": io.StringIO(),
        "wsgi.multithread": False,
        "wsgi.multiprocess": False,
        "wsgi.run_once": False,
        "CONTENT_LENGTH": str(len(content)),
        "CONTENT_TYPE": req_headers.get("Content-Type", "application/octet-stream"),
    }
    for key, value in req_headers.items():
        key_upper = key.upper().replace("-", "_")
        if key_upper not in ("CONTENT_TYPE", "CONTENT_LENGTH"):
            environ[f"HTTP_{key_upper}"] = value

    response = WSGIResponse()
    result = app(environ, response.start_response)
    parts = []
    for chunk in result:
        parts.append(chunk)
    response.content = b"".join(parts)
    return response


@pytest.fixture
def setup_app(temp_dir: Path) -> SetupWizardAPI:
    """Create setup middleware in first-run state."""
    storage = temp_dir / "storage"
    storage.mkdir(parents=True, exist_ok=True)
    db = Database(storage / "mokuro.db")
    config = Config(
        storage=StorageConfig(base_path=storage),
        registration=RegistrationConfig(mode="self"),
    )
    return SetupWizardAPI(dummy_app, db, config, temp_dir / "config.yaml")


def test_setup_complete_requires_localhost(setup_app: SetupWizardAPI) -> None:
    response = make_request(
        setup_app,
        "POST",
        "/setup/api/complete",
        remote_addr="203.0.113.10",
        json_body={
            "admin": {"username": "admin", "password": "password123"},
            "registration": {"mode": "self"},
        },
    )
    assert response.status_code == 403


def test_setup_complete_accepts_localhost(setup_app: SetupWizardAPI) -> None:
    response = make_request(
        setup_app,
        "POST",
        "/setup/api/complete",
        remote_addr="127.0.0.1",
        json_body={
            "admin": {"username": "admin", "password": "password123"},
            "registration": {"mode": "self"},
        },
    )
    assert response.status_code == 201
    assert response.json()["success"] is True


def test_setup_rejects_invalid_admin_username(setup_app: SetupWizardAPI) -> None:
    response = make_request(
        setup_app,
        "POST",
        "/setup/api/complete",
        remote_addr="127.0.0.1",
        json_body={
            "admin": {"username": "../admin", "password": "password123"},
            "registration": {"mode": "self"},
        },
    )
    assert response.status_code == 400


def test_setup_rejects_forwarded_non_loopback(setup_app: SetupWizardAPI) -> None:
    response = make_request(
        setup_app,
        "POST",
        "/setup/api/complete",
        remote_addr="127.0.0.1",
        headers={"X-Forwarded-For": "203.0.113.10"},
        json_body={
            "admin": {"username": "admin", "password": "password123"},
            "registration": {"mode": "self"},
        },
    )
    assert response.status_code == 403


def test_setup_accepts_forwarded_loopback(setup_app: SetupWizardAPI) -> None:
    response = make_request(
        setup_app,
        "POST",
        "/setup/api/complete",
        remote_addr="127.0.0.1",
        headers={"X-Forwarded-For": "127.0.0.1"},
        json_body={
            "admin": {"username": "admin", "password": "password123"},
            "registration": {"mode": "self"},
        },
    )
    assert response.status_code == 201
