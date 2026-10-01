"""Tokens through the real app: issue once, use everywhere, revoke at once."""

from __future__ import annotations

import base64
import json
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.middleware import auth as auth_module
from mokuro_bunko.security import AuthAttemptLimiter
from mokuro_bunko.server import create_app
from tests.unit.test_upload_enqueue import call

PASSWORD = "alice-password-1"


@pytest.fixture(autouse=True)
def _private_limiter(monkeypatch: pytest.MonkeyPatch) -> None:
    limiter = AuthAttemptLimiter()
    monkeypatch.setattr(auth_module, "AUTH_RATE_LIMITER", limiter)
    from mokuro_bunko.login import api as login_api

    monkeypatch.setattr(login_api, "AUTH_RATE_LIMITER", limiter)


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    base = tmp_path / "storage"
    (base / "library" / "S").mkdir(parents=True)
    (base / "inbox").mkdir()
    (base / "users").mkdir()
    db = Database(base / "mokuro.db")
    db.create_user("alice", PASSWORD, "admin")
    db.create_user("bob", "bob-password-12", "registered")
    return base


@pytest.fixture
def app(storage: Path) -> Any:
    return create_app(Config(storage=StorageConfig(base_path=storage)))


def _issue(app: Any, username: str = "alice", password: str = PASSWORD, **extra: Any) -> tuple[int, dict[str, Any]]:
    body = json.dumps({"username": username, "password": password, **extra}).encode()
    status, _, raw = call(app, "POST", "/login/api/token", body,
                          {"Content-Type": "application/json"})
    return status, json.loads(raw or b"{}")


def _bearer(token: str) -> dict[str, str]:
    return {"Authorization": f"Bearer {token}"}


def test_a_password_buys_a_token(app: Any) -> None:
    status, body = _issue(app)
    assert status == 200
    assert body["token_type"] == "Bearer" and body["token"]
    assert body["kind"] == "web" and body["expires_at"] > 0
    assert body["user"] == {"username": "alice", "role": "admin"}


def test_basic_credentials_buy_one_too(app: Any) -> None:
    basic = "Basic " + base64.b64encode(f"alice:{PASSWORD}".encode()).decode()
    status, _, raw = call(app, "POST", "/login/api/token", b"", {"Authorization": basic})
    assert status == 200 and json.loads(raw)["token"]


def test_a_wrong_password_buys_nothing(app: Any) -> None:
    status, body = _issue(app, password="wrong-password")
    assert status == 401 and "token" not in body


def test_guessing_is_rate_limited(app: Any) -> None:
    statuses = [_issue(app, password=f"wrong-{n}")[0] for n in range(15)]
    assert statuses[-1] == 429


def test_an_unknown_kind_is_refused(app: Any) -> None:
    status, _ = _issue(app, kind="forever")
    assert status == 400


def test_a_token_opens_the_admin_api(app: Any) -> None:
    _, body = _issue(app)
    status, _, _ = call(app, "GET", "/_admin/api/users", headers=_bearer(body["token"]))
    assert status == 200


def test_a_token_says_who_it_is(app: Any) -> None:
    _, body = _issue(app)
    status, _, raw = call(app, "GET", "/login/api/me", headers=_bearer(body["token"]))
    me = json.loads(raw)
    assert status == 200 and me["authenticated"] and me["username"] == "alice"


def test_a_bad_token_is_a_401_in_the_tokens_own_scheme(app: Any) -> None:
    status, headers, _ = call(app, "GET", "/_admin/api/users", headers=_bearer("nope"))
    assert status == 401
    assert headers["WWW-Authenticate"].startswith("Bearer ")
    status, _, raw = call(app, "GET", "/login/api/me", headers=_bearer("nope"))
    assert status == 401 and json.loads(raw)["authenticated"] is False


def test_logout_revokes_the_token_at_once(app: Any) -> None:
    _, body = _issue(app)
    token = body["token"]
    status, _, _ = call(app, "DELETE", "/login/api/token", headers=_bearer(token))
    assert status == 200
    status, _, _ = call(app, "GET", "/_admin/api/users", headers=_bearer(token))
    assert status == 401
    status, _, _ = call(app, "GET", "/queue/api/status", headers=_bearer(token))
    assert status == 200  # the public page still answers -- as a visitor


def test_basic_auth_still_works(app: Any) -> None:
    basic = "Basic " + base64.b64encode(f"alice:{PASSWORD}".encode()).decode()
    status, _, _ = call(app, "GET", "/_admin/api/users", headers={"Authorization": basic})
    assert status == 200


def test_a_non_admin_token_is_not_an_admin(app: Any) -> None:
    _, body = _issue(app, username="bob", password="bob-password-12")
    status, _, _ = call(app, "GET", "/_admin/api/users", headers=_bearer(body["token"]))
    assert status == 403


def test_a_bad_token_on_the_manifest_route_is_challenged_as_a_token(app: Any, storage: Path) -> None:
    """The volume manifest answers as its `.cbz` would (`gate_read`), and a dead
    token there must not get a Basic challenge: that would pop a browser's
    password dialog over a reader signed in by token."""
    import zipfile

    with zipfile.ZipFile(storage / "library" / "S" / "V.cbz", "w") as archive:
        archive.writestr("001.jpg", b"jpg")
    status, headers, _ = call(
        app, "GET", "/catalog/api/manifest", headers=_bearer("nope"), query="series=S&volume=V"
    )
    assert status == 401
    assert headers["WWW-Authenticate"].startswith("Bearer ")
    assert 'error="invalid_token"' in headers["WWW-Authenticate"]
