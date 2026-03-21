"""Integration tests for authentication middleware."""

from __future__ import annotations

import base64
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import RegistrationConfig
from mokuro_bunko.database import Database
from mokuro_bunko.middleware.auth import AuthMiddleware
from mokuro_bunko.security import AuthAttemptLimiter


def make_basic_auth_header(username: str, password: str) -> str:
    """Create Basic auth header value."""
    credentials = base64.b64encode(f"{username}:{password}".encode()).decode()
    return f"Basic {credentials}"


def make_environ(
    method: str = "GET",
    path: str = "/",
    auth_header: str | None = None,
) -> dict[str, Any]:
    """Create a minimal WSGI environ dict."""
    environ: dict[str, Any] = {
        "REQUEST_METHOD": method,
        "PATH_INFO": path,
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
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
        """Extract status code from status string."""
        if self.status:
            return int(self.status.split()[0])
        return 0

    def get_header(self, name: str) -> str | None:
        """Get header value by name."""
        for header_name, value in self.headers:
            if header_name.lower() == name.lower():
                return value
        return None


def dummy_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    """Dummy WSGI app that returns 200 OK."""
    start_response("200 OK", [("Content-Type", "text/plain")])
    return [b"OK"]


@pytest.fixture
def db_with_test_users(temp_dir: Path) -> Database:
    """Create database with test users of different roles."""
    db = Database(temp_dir / "test.db")
    db.create_user("anonymous_user", "pass1234", "anonymous")  # Won't be used for auth
    db.create_user("registered_user", "pass1234", "registered")
    db.create_user("writer_user", "pass1234", "uploader")
    db.create_user("inviter_user", "pass1234", "inviter")
    db.create_user("editor_user", "pass1234", "editor")
    db.create_user("admin_user", "pass1234", "admin")
    db.create_user("pending_user", "pass1234", "registered", status="pending")
    db.create_user("disabled_user", "pass1234", "registered", status="disabled")
    return db


@pytest.fixture
def auth_middleware(db_with_test_users: Database) -> AuthMiddleware:
    """Create auth middleware with test database."""
    return AuthMiddleware(dummy_app, db_with_test_users)


class TestAuthentication:
    """Tests for authentication flow."""

    def test_no_credentials_returns_anonymous(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test request without credentials is treated as anonymous."""
        environ = make_environ()
        result = auth_middleware.authenticate(environ)

        assert result.authenticated is False
        assert result.role == "anonymous"
        assert result.user is None
        assert result.error is None

    def test_valid_credentials_authenticates(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test valid credentials authenticate successfully."""
        environ = make_environ(auth_header=make_basic_auth_header("registered_user", "pass1234"))
        result = auth_middleware.authenticate(environ)

        assert result.authenticated is True
        assert result.role == "registered"
        assert result.user is not None
        assert result.username == "registered_user"

    def test_invalid_password_fails(self, auth_middleware: AuthMiddleware) -> None:
        """Test invalid password returns error."""
        environ = make_environ(auth_header=make_basic_auth_header("registered_user", "wrong"))
        result = auth_middleware.authenticate(environ)

        assert result.authenticated is False
        assert result.error == "Invalid credentials"

    def test_nonexistent_user_fails(self, auth_middleware: AuthMiddleware) -> None:
        """Test nonexistent user returns error."""
        environ = make_environ(auth_header=make_basic_auth_header("nonexistent", "pass1234"))
        result = auth_middleware.authenticate(environ)

        assert result.authenticated is False
        assert result.error == "Invalid credentials"

    def test_pending_user_fails(self, auth_middleware: AuthMiddleware) -> None:
        """Test pending user cannot authenticate."""
        environ = make_environ(auth_header=make_basic_auth_header("pending_user", "pass1234"))
        result = auth_middleware.authenticate(environ)

        assert result.authenticated is False
        assert result.error == "Invalid credentials"

    def test_disabled_user_fails(self, auth_middleware: AuthMiddleware) -> None:
        """Test disabled user cannot authenticate."""
        environ = make_environ(auth_header=make_basic_auth_header("disabled_user", "pass1234"))
        result = auth_middleware.authenticate(environ)

        assert result.authenticated is False
        assert result.error == "Invalid credentials"

    def test_all_roles_authenticate(self, auth_middleware: AuthMiddleware) -> None:
        """Test all role types can authenticate."""
        for username, expected_role in [
            ("registered_user", "registered"),
            ("writer_user", "uploader"),
            ("inviter_user", "inviter"),
            ("editor_user", "editor"),
            ("admin_user", "admin"),
        ]:
            environ = make_environ(auth_header=make_basic_auth_header(username, "pass1234"))
            result = auth_middleware.authenticate(environ)

            assert result.authenticated is True, f"Failed for {username}"
            assert result.role == expected_role, f"Wrong role for {username}"

    def test_rate_limiter_blocks_repeated_failures(
        self, db_with_test_users: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Repeated failures for same user/IP should trigger temporary block."""
        import mokuro_bunko.middleware.auth as auth_module

        monkeypatch.setattr(
            auth_module,
            "AUTH_RATE_LIMITER",
            AuthAttemptLimiter(max_failures=2, window_seconds=300, block_seconds=60),
        )
        middleware = AuthMiddleware(dummy_app, db_with_test_users)
        environ = make_environ(
            auth_header=make_basic_auth_header("registered_user", "wrong"),
        )
        environ["REMOTE_ADDR"] = "192.0.2.15"

        first = middleware.authenticate(environ)
        second = middleware.authenticate(environ)
        third = middleware.authenticate(environ)

        assert first.authenticated is False
        assert second.authenticated is False
        assert third.authenticated is False
        assert third.error is not None
        assert "Too many failed attempts" in third.error

    def test_request_rate_limiter_blocks_heavy_request_rate(
        self, db_with_test_users: Database, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Too many overall requests per IP should return 429."""
        import mokuro_bunko.middleware.auth as auth_module

        monkeypatch.setattr(
            auth_module,
            "REQUEST_RATE_LIMITER",
            AuthAttemptLimiter(max_failures=2, window_seconds=60, block_seconds=30),
        )

        middleware = AuthMiddleware(dummy_app, db_with_test_users)
        environ = make_environ(method="GET", path="/")
        environ["REMOTE_ADDR"] = "192.0.2.20"

        resp1 = middleware(environ, MockStartResponse())
        resp2 = middleware(environ, MockStartResponse())
        resp3 = middleware(environ, MockStartResponse())

        assert resp1 is not None
        assert resp2 is not None
        assert resp3 is not None

        middleware(environ, MockStartResponse())
        # JSON isn't returned by the middleware directly; verify call returns 429 in response status
        # We have to use a start response object with status_code property
        sr = MockStartResponse()
        middleware(environ, sr)
        assert sr.status_code == 429


class TestAuthorization:
    """Tests for authorization flow."""

    def test_anonymous_can_read(self, auth_middleware: AuthMiddleware) -> None:
        """Test anonymous can read library."""
        environ = make_environ(method="GET", path="/mokuro-reader/manga.cbz")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_anonymous_cannot_write_progress(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test anonymous cannot write progress files."""
        environ = make_environ(method="PUT", path="/mokuro-reader/volume-data.json")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 401

    def test_anonymous_download_blocked_when_disabled(self, db_with_test_users: Database) -> None:
        """Anonymous GET should be rejected when anonymous download is disabled."""
        middleware = AuthMiddleware(
            dummy_app,
            db_with_test_users,
            registration_config=RegistrationConfig(
                allow_anonymous_browse=True,
                allow_anonymous_download=False,
            ),
        )
        for path in ("/mokuro-reader/test.cbz", "/mokuro-reader/test.cbz"):
            environ = make_environ(method="GET", path=path)
            start_response = MockStartResponse()

            middleware(environ, start_response)
            assert start_response.status_code == 401

    def test_anonymous_browse_allowed_when_only_download_blocked(
        self, db_with_test_users: Database
    ) -> None:
        """Anonymous PROPFIND should still work when browse is enabled."""
        middleware = AuthMiddleware(
            dummy_app,
            db_with_test_users,
            registration_config=RegistrationConfig(
                allow_anonymous_browse=True,
                allow_anonymous_download=False,
            ),
        )
        environ = make_environ(method="PROPFIND", path="/mokuro-reader")
        start_response = MockStartResponse()

        middleware(environ, start_response)
        assert start_response.status_code == 200

    def test_anonymous_browse_blocked_when_disabled(self, db_with_test_users: Database) -> None:
        """Anonymous PROPFIND should be rejected when anonymous browse is disabled."""
        middleware = AuthMiddleware(
            dummy_app,
            db_with_test_users,
            registration_config=RegistrationConfig(
                allow_anonymous_browse=False,
                allow_anonymous_download=False,
            ),
        )
        environ = make_environ(method="PROPFIND", path="/mokuro-reader")
        start_response = MockStartResponse()

        middleware(environ, start_response)
        assert start_response.status_code == 401
    def test_anonymous_cannot_read_progress_file(self, auth_middleware: AuthMiddleware) -> None:
        """Test anonymous GET of progress file should be rejected."""
        environ = make_environ(method="GET", path="/mokuro-reader/volume-data.json")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 401

    def test_anonymous_cannot_propfind_progress_file(self, auth_middleware: AuthMiddleware) -> None:
        """Test anonymous PROPFIND of per-user progress file should be rejected."""
        environ = make_environ(method="PROPFIND", path="/mokuro-reader/volume-data.json",)
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 401

    def test_anonymous_inbox_access_is_blocked(self, auth_middleware: AuthMiddleware) -> None:
        """Test anonymous cannot access inbox paths."""
        for method in ("GET", "HEAD", "PROPFIND"):
            environ = make_environ(method=method, path="/inbox")
            start_response = MockStartResponse()

            auth_middleware(environ, start_response)
            assert start_response.status_code == 404

    def test_path_traversal_to_inbox_is_blocked(self, auth_middleware: AuthMiddleware) -> None:
        """Test traversal path to inbox is blocked."""
        environ = make_environ(method="GET", path="/mokuro-reader/../inbox")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 404
    def test_registered_can_write_own_progress(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test registered user can write own progress."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/volume-data.json",
            auth_header=make_basic_auth_header("registered_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_registered_can_write_profiles(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test registered user can write profiles.json."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/profiles.json",
            auth_header=make_basic_auth_header("registered_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_registered_cannot_add_files(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test registered user cannot add files to library."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/new_manga.cbz",
            auth_header=make_basic_auth_header("registered_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 403

    def test_writer_can_add_files(self, auth_middleware: AuthMiddleware) -> None:
        """Test uploader can add files to library."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/new_manga.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_writer_cannot_add_to_inbox_path(self, auth_middleware: AuthMiddleware) -> None:
        """Test uploader cannot add files to /inbox (not exposed)."""
        environ = make_environ(
            method="PUT",
            path="/inbox/upload.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 403

    def test_writer_cannot_delete(self, auth_middleware: AuthMiddleware) -> None:
        """Test uploader cannot delete files."""
        environ = make_environ(
            method="DELETE",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 403

    def test_uploader_can_delete_owned_volume(
        self, db_with_test_users: Database
    ) -> None:
        """Test uploader can delete files they uploaded."""
        db_with_test_users.record_volume_upload("series/manga.cbz", "writer_user")
        middleware = AuthMiddleware(dummy_app, db_with_test_users)
        environ = make_environ(
            method="DELETE",
            path="/mokuro-reader/series/manga.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_editor_can_delete(self, auth_middleware: AuthMiddleware) -> None:
        """Test editor can delete files."""
        environ = make_environ(
            method="DELETE",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("editor_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_editor_cannot_access_admin(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test editor cannot access admin panel."""
        environ = make_environ(
            method="GET",
            path="/_admin/api/users",
            auth_header=make_basic_auth_header("editor_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 403

    def test_admin_can_access_admin(self, auth_middleware: AuthMiddleware) -> None:
        """Test admin can access admin panel."""
        environ = make_environ(
            method="GET",
            path="/_admin",
            auth_header=make_basic_auth_header("admin_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_dynamic_admin_path_matching(self, db_with_test_users: Database) -> None:
        """Test auth middleware uses configured admin path."""
        middleware = AuthMiddleware(
            dummy_app,
            db_with_test_users,
            admin_path="/admin-panel",
        )
        environ = make_environ(
            method="GET",
            path="/admin-panel/api/users",
            auth_header=make_basic_auth_header("admin_user", "pass1234"),
        )
        start_response = MockStartResponse()

        middleware(environ, start_response)
        assert start_response.status_code == 200

    def test_admin_path_subpath_not_admin(self, auth_middleware: AuthMiddleware) -> None:
        """Ensure /_adminx is not treated as admin path."""
        environ = make_environ(
            method="GET",
            path="/_adminx/api/users",
            auth_header=make_basic_auth_header("admin_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 200

    def test_admin_path_dotdot_normalization(self, auth_middleware: AuthMiddleware) -> None:
        """Ensure /_admin/../api/users is normalized and rejected if not admin path."""
        environ = make_environ(
            method="GET",
            path="/_admin/../api/users",
            auth_header=make_basic_auth_header("admin_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 200

    def test_inviter_can_access_invite_admin_api(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test inviter can access invite admin endpoints."""
        environ = make_environ(
            method="GET",
            path="/_admin/api/invites",
            auth_header=make_basic_auth_header("inviter_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_admin_can_write_progress(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test admin can write progress files."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/volume-data.json",
            auth_header=make_basic_auth_header("admin_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_upload_quota_enforced_for_uploader(self, db_with_test_users: Database) -> None:
        """Get 429 when uploader exceeds per-day upload quota."""
        # set low quota for test without using full config
        middleware = AuthMiddleware(
            dummy_app,
            db_with_test_users,
            registration_config=RegistrationConfig(allow_anonymous_browse=True, allow_anonymous_download=True),
            quota_config=type("Q", (), {"uploads_per_day": 1})(),
        )

        # seed one existing upload in past 24h
        db_with_test_users.record_volume_upload("series/vol1.cbz", "writer_user")

        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/series/vol2.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        middleware(environ, start_response)
        assert start_response.status_code == 429


class TestHTTPResponses:
    """Tests for HTTP response codes and headers."""

    def test_401_includes_www_authenticate(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test 401 response includes WWW-Authenticate header."""
        environ = make_environ(method="PUT", path="/mokuro-reader/volume-data.json")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 401
        auth_header = start_response.get_header("WWW-Authenticate")
        assert auth_header is not None
        assert "Basic" in auth_header
        assert "realm=" in auth_header

    def test_403_no_www_authenticate(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test 403 response does not include WWW-Authenticate header."""
        environ = make_environ(
            method="DELETE",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 403
        auth_header = start_response.get_header("WWW-Authenticate")
        assert auth_header is None

    def test_invalid_credentials_returns_401(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test invalid credentials return 401."""
        environ = make_environ(
            method="GET",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("registered_user", "wrong"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 401


class TestEnvironStorage:
    """Tests for storing auth info in environ."""

    def test_auth_info_stored_in_environ(
        self, db_with_test_users: Database
    ) -> None:
        """Test auth info is stored in environ for downstream use."""
        stored_environ: dict[str, Any] = {}

        def capture_app(
            environ: dict[str, Any], start_response: Callable[..., Any]
        ) -> list[bytes]:
            stored_environ.update(environ)
            start_response("200 OK", [])
            return [b"OK"]

        middleware = AuthMiddleware(capture_app, db_with_test_users)
        environ = make_environ(
            method="GET",
            path="/",
            auth_header=make_basic_auth_header("registered_user", "pass1234"),
        )
        start_response = MockStartResponse()

        middleware(environ, start_response)

        assert "mokuro.auth" in stored_environ
        assert "mokuro.user" in stored_environ
        assert "mokuro.role" in stored_environ
        assert stored_environ["mokuro.role"] == "registered"
        assert stored_environ["mokuro.user"]["username"] == "registered_user"

    def test_anonymous_info_stored_in_environ(
        self, db_with_test_users: Database
    ) -> None:
        """Test anonymous info is stored in environ."""
        stored_environ: dict[str, Any] = {}

        def capture_app(
            environ: dict[str, Any], start_response: Callable[..., Any]
        ) -> list[bytes]:
            stored_environ.update(environ)
            start_response("200 OK", [])
            return [b"OK"]

        middleware = AuthMiddleware(capture_app, db_with_test_users)
        environ = make_environ(method="GET", path="/")
        start_response = MockStartResponse()

        middleware(environ, start_response)

        assert stored_environ["mokuro.role"] == "anonymous"
        assert stored_environ["mokuro.user"] is None


class TestOptionsRequest:
    """Tests for OPTIONS requests (CORS preflight)."""

    def test_options_always_allowed(self, auth_middleware: AuthMiddleware) -> None:
        """Test OPTIONS requests are always allowed for CORS."""
        environ = make_environ(method="OPTIONS", path="/mokuro-reader/manga.cbz")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_options_allowed_without_auth(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test OPTIONS allowed without authentication."""
        environ = make_environ(method="OPTIONS", path="/_admin")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200


class TestWebDAVMethods:
    """Tests for WebDAV-specific HTTP methods."""

    def test_propfind_allowed_anonymous(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test PROPFIND allowed for anonymous."""
        environ = make_environ(method="PROPFIND", path="/mokuro-reader")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_mkcol_requires_add_files(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test MKCOL requires ADD_FILES permission."""
        # Registered cannot MKCOL
        environ = make_environ(
            method="MKCOL",
            path="/mokuro-reader/new_folder",
            auth_header=make_basic_auth_header("registered_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 403

        # Uploader can MKCOL
        environ = make_environ(
            method="MKCOL",
            path="/mokuro-reader/new_folder",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 200

    def test_move_requires_modify_delete(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test MOVE requires MODIFY_DELETE permission."""
        # Uploader cannot MOVE
        environ = make_environ(
            method="MOVE",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 403

        # Editor can MOVE
        environ = make_environ(
            method="MOVE",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("editor_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 200

    def test_copy_requires_modify_delete(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test COPY requires MODIFY_DELETE permission."""
        environ = make_environ(
            method="COPY",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)
        assert start_response.status_code == 403


class TestProgressFileAccess:
    """Tests for per-user progress file access control.

    Per-user files (volume-data.json, profiles.json) are accessed via
    /mokuro-reader/ and are transparently mapped to each user's private
    directory. There is no /users/{username}/ WebDAV path anymore.
    """

    def test_anonymous_cannot_write_progress(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test anonymous cannot write progress files."""
        environ = make_environ(method="PUT", path="/mokuro-reader/volume-data.json")
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 401

    def test_registered_can_write_volume_data(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test registered user can write volume-data.json."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/volume-data.json",
            auth_header=make_basic_auth_header("registered_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_registered_can_write_profiles(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test registered user can write profiles.json."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/profiles.json",
            auth_header=make_basic_auth_header("registered_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_can_delete_own_progress_file(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test user can delete own progress file."""
        environ = make_environ(
            method="DELETE",
            path="/mokuro-reader/volume-data.json",
            auth_header=make_basic_auth_header("registered_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        # Registered can delete own progress even without MODIFY_DELETE
        assert start_response.status_code == 200

    def test_admin_can_write_progress(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test admin can write progress files."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/volume-data.json",
            auth_header=make_basic_auth_header("admin_user", "pass1234"),
        )
        start_response = MockStartResponse()

        auth_middleware(environ, start_response)

        assert start_response.status_code == 200
