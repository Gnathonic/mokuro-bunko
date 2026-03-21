"""Web-level tests for admin panel pages and APIs (no browser dependency)."""

from __future__ import annotations

import base64
import io
import json
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import AdminConfig, Config, RegistrationConfig, ServerConfig, StorageConfig
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
def app(tmp_path: Path) -> Callable[..., Any]:
    storage = tmp_path / "storage"
    storage.mkdir(parents=True)

    config = Config(
        server=ServerConfig(host="127.0.0.1", port=0),
        storage=StorageConfig(base_path=storage),
        admin=AdminConfig(enabled=True, path="/_admin"),
        registration=RegistrationConfig(mode="self"),
    )
    db = Database(storage / "mokuro.db")
    db.create_user("admin", "adminpass", role="admin")
    return create_app(config)


class TestAdminPanelWeb:
    def test_admin_index_contains_primary_sections(self, app: Callable[..., Any]) -> None:
        status, headers, body = _call_app(app, "GET", "/_admin/")

        assert status.startswith("200")
        html = body.decode("utf-8")
        assert "Users" in html
        assert "Invites" in html
        assert "Settings" in html
        assert "id=\"add-user-btn\"" in html
        assert headers.get("X-Content-Type-Options") == "nosniff"

    def test_admin_users_api_requires_admin_auth(self, app: Callable[..., Any]) -> None:
        status, _headers, body = _call_app(app, "GET", "/_admin/api/users")

        assert status.startswith(("401", "403"))
        text = body.decode("utf-8", errors="replace").strip()
        assert text != ""

    def test_admin_users_api_returns_admin_user(self, app: Callable[..., Any]) -> None:
        status, _headers, body = _call_app(
            app,
            "GET",
            "/_admin/api/users",
            headers={"Authorization": _auth_header("admin", "adminpass")},
        )

        assert status.startswith("200")
        payload = json.loads(body)
        assert any(u["username"] == "admin" for u in payload["users"])

    def test_admin_assets_include_security_headers(self, app: Callable[..., Any]) -> None:
        status, headers, _body = _call_app(app, "GET", "/_admin/styles.css")

        assert status.startswith("200")
        assert headers.get("X-Content-Type-Options") == "nosniff"
        assert headers.get("X-Frame-Options") == "DENY"

    def test_admin_prefix_lookalike_does_not_match(self, app: Callable[..., Any]) -> None:
        status, _headers, _body = _call_app(app, "GET", "/_adminx/api/users")

        assert status.startswith("404")
