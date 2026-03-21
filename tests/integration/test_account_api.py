"""Integration tests for account API endpoints."""

from __future__ import annotations

import io
import json
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app


def make_environ(
    method: str = "GET",
    path: str = "/",
    headers: dict[str, str] | None = None,
    body: bytes = b"",
) -> dict[str, Any]:
    headers = headers or {}
    environ = {
        "REQUEST_METHOD": method,
        "SCRIPT_NAME": "",
        "PATH_INFO": path,
        "QUERY_STRING": "",
        "CONTENT_TYPE": headers.get("Content-Type", ""),
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

    for key, value in headers.items():
        key_upper = key.upper().replace("-", "_")
        if key_upper not in ("CONTENT_TYPE", "CONTENT_LENGTH"):
            environ[f"HTTP_{key_upper}"] = value

    return environ


def to_basic_auth(username: str, password: str) -> str:
    import base64

    token = base64.b64encode(f"{username}:{password}".encode()).decode()
    return f"Basic {token}"


def call_app(app, environ):
    status = []
    headers = []

    def start_response(s, h, exc_info=None):
        status.append(s)
        headers.extend(h)
        return lambda data: None

    body = b"".join(app(environ, start_response))
    return status[0], dict(headers), body


@pytest.fixture
def config(tmp_path):
    storage = tmp_path / "storage"
    storage.mkdir()
    return Config(storage=StorageConfig(base_path=storage))


@pytest.fixture
def app(config: Config):
    db = Database(Path(config.storage.base_path) / "mokuro.db")
    db.create_user("user", "password123", "registered")
    return create_app(config)


class TestAccountEndpoints:
    def test_get_me_requires_auth(self, app):
        environ = make_environ(method="GET", path="/api/account/me")
        status, _headers, body = call_app(app, environ)

        assert "401" in status

    def test_get_me_succeeds(self, app):
        environ = make_environ(
            method="GET",
            path="/api/account/me",
            headers={"Authorization": to_basic_auth("user", "password123")},
        )
        status, _headers, body = call_app(app, environ)

        assert "200" in status
        data = json.loads(body)
        assert data["username"] == "user"
        assert data["role"] == "registered"
