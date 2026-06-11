"""Integration tests for the /login/api/me identity endpoint."""

from __future__ import annotations

import base64
import json
from pathlib import Path
from typing import Any, Callable

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.login.api import LoginAPI
from mokuro_bunko.security import AuthAttemptLimiter


def make_environ(auth_header: str | None = None) -> dict[str, Any]:
    """Create a minimal WSGI environ dict for GET /login/api/me."""
    environ: dict[str, Any] = {
        "REQUEST_METHOD": "GET",
        "PATH_INFO": "/login/api/me",
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
        "REMOTE_ADDR": "192.0.2.77",
        "wsgi.url_scheme": "http",
    }
    if auth_header:
        environ["HTTP_AUTHORIZATION"] = auth_header
    return environ


class MockStartResponse:
    """Mock WSGI start_response for testing."""

    def __init__(self) -> None:
        self.status: str | None = None
        self.headers: list[tuple[str, str]] = []

    def __call__(self, status: str, headers: list[tuple[str, str]]) -> None:
        self.status = status
        self.headers = headers

    @property
    def status_code(self) -> int:
        if self.status:
            return int(self.status.split()[0])
        return 0


def dummy_app(
    environ: dict[str, Any], start_response: Callable[..., Any]
) -> list[bytes]:
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"OK"]


def encoded_header(credentials: str, encoding: str = "utf-8") -> str:
    return "Basic " + base64.b64encode(credentials.encode(encoding)).decode("ascii")


@pytest.fixture(autouse=True)
def fresh_rate_limiter(monkeypatch: pytest.MonkeyPatch) -> AuthAttemptLimiter:
    """Reset the LoginAPI module-level rate limiter for each test."""
    import mokuro_bunko.login.api as login_api_module

    limiter = AuthAttemptLimiter()
    monkeypatch.setattr(login_api_module, "AUTH_RATE_LIMITER", limiter)
    return limiter


@pytest.fixture
def db(temp_dir: Path) -> Database:
    """Database with users of each role."""
    db = Database(temp_dir / "me.db")
    db.create_user("reg", "pass1234", "registered")
    db.create_user("upl", "pass1234", "uploader")
    db.create_user("edi", "pass1234", "editor")
    db.create_user("adm", "pass1234", "admin")
    db.create_user("umlaut", "pässwörd", "registered")
    return db


@pytest.fixture
def api(db: Database) -> LoginAPI:
    return LoginAPI(dummy_app, database=db)


def call_me(api: LoginAPI, auth_header: str | None = None) -> tuple[int, dict[str, Any]]:
    """Call GET /login/api/me; return (status_code, parsed_json)."""
    start_response = MockStartResponse()
    body = b"".join(api(make_environ(auth_header), start_response))
    return start_response.status_code, json.loads(body.decode("utf-8"))


class TestMeEndpoint:
    """Tests for the extended /login/api/me identity endpoint."""

    def test_me_valid_creds_returns_identity_and_permissions(
        self, api: LoginAPI
    ) -> None:
        status, body = call_me(api, encoded_header("reg:pass1234"))
        assert status == 200
        assert body["authenticated"] is True
        assert body["username"] == "reg"
        assert body["role"] == "registered"
        assert body["created_at"]
        assert body["permissions"] == {
            "canWriteProgress": True,
            "canAddFiles": False,
            "canModifyDelete": False,
        }

    @pytest.mark.parametrize(
        "username,role,write_progress,add_files,modify_delete",
        [
            ("reg", "registered", True, False, False),
            ("upl", "uploader", True, True, False),
            ("edi", "editor", True, True, True),
            ("adm", "admin", True, True, True),
        ],
    )
    def test_me_permissions_per_role(
        self,
        api: LoginAPI,
        username: str,
        role: str,
        write_progress: bool,
        add_files: bool,
        modify_delete: bool,
    ) -> None:
        status, body = call_me(api, encoded_header(f"{username}:pass1234"))
        assert status == 200
        assert body["authenticated"] is True
        assert body["role"] == role
        assert body["permissions"] == {
            "canWriteProgress": write_progress,
            "canAddFiles": add_files,
            "canModifyDelete": modify_delete,
        }

    def test_me_utf8_nonascii_password_authenticates(self, api: LoginAPI) -> None:
        status, body = call_me(api, encoded_header("umlaut:pässwörd", "utf-8"))
        assert status == 200
        assert body["authenticated"] is True
        assert body["username"] == "umlaut"

    def test_me_latin1_header_rejected(self, api: LoginAPI) -> None:
        """Latin-1 encoded creds are malformed -> 401, not authenticated."""
        status, body = call_me(api, encoded_header("umlaut:pässwörd", "latin-1"))
        assert status == 401
        assert body["authenticated"] is False
        assert body["error"] == "Invalid credentials"

    def test_me_invalid_creds_401(self, api: LoginAPI) -> None:
        status, body = call_me(api, encoded_header("reg:wrongpass"))
        assert status == 401
        assert body["authenticated"] is False
        assert body["error"] == "Invalid credentials"

    def test_me_garbled_header_401(self, api: LoginAPI) -> None:
        status, body = call_me(api, "Basic !!!notb64!!!")
        assert status == 401
        assert body["authenticated"] is False
        assert body["error"] == "Invalid credentials"

    def test_me_no_header_200_anonymous(self, api: LoginAPI) -> None:
        status, body = call_me(api)
        assert status == 200
        assert body["authenticated"] is False
        assert body["role"] == "anonymous"
        assert body["permissions"] == {
            "canWriteProgress": False,
            "canAddFiles": False,
            "canModifyDelete": False,
        }

    def test_me_non_basic_scheme_200_anonymous(self, api: LoginAPI) -> None:
        status, body = call_me(api, "Bearer xyz")
        assert status == 200
        assert body["authenticated"] is False
        assert body["role"] == "anonymous"

    def test_me_rate_limited_429(
        self, api: LoginAPI, fresh_rate_limiter: AuthAttemptLimiter
    ) -> None:
        # Garbled headers must NOT count toward the limit
        for _ in range(5):
            status, _body = call_me(api, "Basic !!!notb64!!!")
            assert status == 401

        # 10 invalid-cred attempts fill the window
        for _ in range(10):
            status, _body = call_me(api, encoded_header("reg:wrongpass"))
            assert status == 401

        status, body = call_me(api, encoded_header("reg:wrongpass"))
        assert status == 429
        assert body["authenticated"] is False
        assert "Too many failed attempts" in body["error"]

    def test_me_success_keeps_legacy_keys(self, api: LoginAPI) -> None:
        """account.js compat: username/role/created_at preserved on success."""
        status, body = call_me(api, encoded_header("reg:pass1234"))
        assert status == 200
        assert set(body) >= {"username", "role", "created_at"}
