"""Web-level tests for registration page behavior without browser dependency."""

from __future__ import annotations

import io
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, RegistrationConfig, ServerConfig, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app


def _call_app(
    app: Callable[..., Any],
    method: str,
    path: str,
    *,
    headers: dict[str, str] | None = None,
    body: bytes = b"",
) -> tuple[str, dict[str, str], bytes]:
    req_headers = headers or {}
    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "SCRIPT_NAME": "",
        "PATH_INFO": path,
        "QUERY_STRING": "",
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
def self_app(tmp_path: Path) -> Callable[..., Any]:
    storage = tmp_path / "self-storage"
    storage.mkdir(parents=True)
    config = Config(
        server=ServerConfig(host="127.0.0.1", port=0),
        storage=StorageConfig(base_path=storage),
        registration=RegistrationConfig(mode="self"),
    )
    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")
    return create_app(config)


@pytest.fixture
def invite_app(tmp_path: Path) -> tuple[Callable[..., Any], str]:
    storage = tmp_path / "invite-storage"
    storage.mkdir(parents=True)
    config = Config(
        server=ServerConfig(host="127.0.0.1", port=0),
        storage=StorageConfig(base_path=storage),
        registration=RegistrationConfig(mode="invite"),
    )
    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")
    invite_code = db.create_invite(role="registered", expires="7d")
    return create_app(config), invite_code


@pytest.fixture
def disabled_app(tmp_path: Path) -> Callable[..., Any]:
    storage = tmp_path / "disabled-storage"
    storage.mkdir(parents=True)
    config = Config(
        server=ServerConfig(host="127.0.0.1", port=0),
        storage=StorageConfig(base_path=storage),
        registration=RegistrationConfig(mode="disabled"),
    )
    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")
    return create_app(config)


class TestRegistrationWeb:
    def test_register_page_contains_fields(self, self_app: Callable[..., Any]) -> None:
        status, headers, body = _call_app(self_app, "GET", "/register")

        assert status.startswith("200")
        html = body.decode("utf-8")
        assert "id=\"username\"" in html
        assert "id=\"password\"" in html
        assert "id=\"confirm-password\"" in html
        assert "id=\"invite-group\"" in html
        assert headers.get("X-Content-Type-Options") == "nosniff"

    def test_registration_config_self_mode(self, self_app: Callable[..., Any]) -> None:
        status, _headers, body = _call_app(self_app, "GET", "/api/register/config")

        assert status.startswith("200")
        payload = json.loads(body)
        assert payload["mode"] == "self"

    def test_registration_config_invite_mode(self, invite_app: tuple[Callable[..., Any], str]) -> None:
        app, _invite_code = invite_app
        status, _headers, body = _call_app(app, "GET", "/api/register/config")

        assert status.startswith("200")
        payload = json.loads(body)
        assert payload["mode"] == "invite"

    def test_self_registration_succeeds(self, self_app: Callable[..., Any]) -> None:
        payload = {"username": "webuser", "password": "password123"}
        status, _headers, body = _call_app(
            self_app,
            "POST",
            "/api/register",
            body=json.dumps(payload).encode("utf-8"),
        )

        assert status.startswith("201")
        response = json.loads(body)
        assert response["success"] is True

    def test_invite_registration_requires_code(self, invite_app: tuple[Callable[..., Any], str]) -> None:
        app, _invite_code = invite_app
        payload = {"username": "inviteuser", "password": "password123"}
        status, _headers, body = _call_app(
            app,
            "POST",
            "/api/register",
            body=json.dumps(payload).encode("utf-8"),
        )

        assert status.startswith("400")
        response = json.loads(body)
        assert "invite" in response["error"].lower()

    def test_invite_registration_succeeds_with_code(self, invite_app: tuple[Callable[..., Any], str]) -> None:
        app, invite_code = invite_app
        payload = {"username": "inviteok", "password": "password123", "invite_code": invite_code}
        status, _headers, body = _call_app(
            app,
            "POST",
            "/api/register",
            body=json.dumps(payload).encode("utf-8"),
        )

        assert status.startswith("201")
        response = json.loads(body)
        assert response["success"] is True

    def test_disabled_registration_rejects_post(self, disabled_app: Callable[..., Any]) -> None:
        payload = {"username": "blocked", "password": "password123"}
        status, _headers, body = _call_app(
            disabled_app,
            "POST",
            "/api/register",
            body=json.dumps(payload).encode("utf-8"),
        )

        assert status.startswith("403")
        response = json.loads(body)
        assert "disabled" in response["error"].lower()
