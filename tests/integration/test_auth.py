"""Integration tests for authentication middleware."""

from __future__ import annotations

import base64
from pathlib import Path
from typing import Any, Callable

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.config import RegistrationConfig
from mokuro_bunko.middleware.auth import AuthMiddleware, AuthResult
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


class TestAuthorization:
    """Tests for authorization flow."""

    def test_anonymous_can_read(self, auth_middleware: AuthMiddleware) -> None:
        """Test anonymous can read library."""
        environ = make_environ(method="GET", path="/mokuro-reader/manga.cbz")
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_anonymous_cannot_write_progress(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test anonymous cannot write progress files."""
        environ = make_environ(method="PUT", path="/mokuro-reader/volume-data.json")
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

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
        environ = make_environ(method="GET", path="/mokuro-reader/manga.cbz")
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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 403

    def test_writer_can_add_files(self, auth_middleware: AuthMiddleware) -> None:
        """Test uploader can add files to library."""
        environ = make_environ(
            method="PUT",
            path="/mokuro-reader/new_manga.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_writer_cannot_add_to_inbox_path(self, auth_middleware: AuthMiddleware) -> None:
        """Test uploader cannot add files to /inbox (not exposed)."""
        environ = make_environ(
            method="PUT",
            path="/inbox/upload.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 403

    def test_writer_cannot_delete(self, auth_middleware: AuthMiddleware) -> None:
        """Test uploader cannot delete files."""
        environ = make_environ(
            method="DELETE",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 403

    def test_admin_can_access_admin(self, auth_middleware: AuthMiddleware) -> None:
        """Test admin can access admin panel."""
        environ = make_environ(
            method="GET",
            path="/_admin",
            auth_header=make_basic_auth_header("admin_user", "pass1234"),
        )
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 200


class TestHTTPResponses:
    """Tests for HTTP response codes and headers."""

    def test_401_includes_www_authenticate(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test 401 response includes WWW-Authenticate header."""
        environ = make_environ(method="PUT", path="/mokuro-reader/volume-data.json")
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 200

    def test_options_allowed_without_auth(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test OPTIONS allowed without authentication."""
        environ = make_environ(method="OPTIONS", path="/_admin")
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 200


class TestWebDAVMethods:
    """Tests for WebDAV-specific HTTP methods."""

    def test_propfind_allowed_anonymous(
        self, auth_middleware: AuthMiddleware
    ) -> None:
        """Test PROPFIND allowed for anonymous."""
        environ = make_environ(method="PROPFIND", path="/mokuro-reader")
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)
        assert start_response.status_code == 403

        # Uploader can MKCOL
        environ = make_environ(
            method="MKCOL",
            path="/mokuro-reader/new_folder",
            auth_header=make_basic_auth_header("writer_user", "pass1234"),
        )
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)
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

        result = auth_middleware(environ, start_response)
        assert start_response.status_code == 403

        # Editor can MOVE
        environ = make_environ(
            method="MOVE",
            path="/mokuro-reader/manga.cbz",
            auth_header=make_basic_auth_header("editor_user", "pass1234"),
        )
        start_response = MockStartResponse()

        result = auth_middleware(environ, start_response)
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

        result = auth_middleware(environ, start_response)
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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

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

        result = auth_middleware(environ, start_response)

        assert start_response.status_code == 200


class TestUtf8BasicAuth:
    """UTF-8-only Basic auth: non-UTF-8 headers are malformed -> 401.

    Legacy clients (e.g. mokuro-reader <=1.6.1 via btoa) sent 'user:pass'
    encoded as Latin-1 bytes. Those are NOT accepted: they fail loudly with
    401 + charset="UTF-8" challenge instead of silently degrading to
    anonymous (the original bug). The fixed reader (>=1.6.2) sends UTF-8.
    """

    @pytest.fixture(autouse=True)
    def fresh_rate_limiter(self, monkeypatch: pytest.MonkeyPatch) -> AuthAttemptLimiter:
        """Reset the module-level rate limiter for each test."""
        import mokuro_bunko.middleware.auth as auth_module

        limiter = AuthAttemptLimiter()
        monkeypatch.setattr(auth_module, "AUTH_RATE_LIMITER", limiter)
        return limiter

    @pytest.fixture
    def unicode_db(self, temp_dir: Path) -> Database:
        """Database with users whose passwords contain non-ASCII chars."""
        db = Database(temp_dir / "unicode.db")
        db.create_user("umlaut", "pässwörd", "registered")
        db.create_user("ascii_user", "plainpass", "registered")
        return db

    @pytest.fixture
    def middleware(self, unicode_db: Database) -> AuthMiddleware:
        return AuthMiddleware(dummy_app, unicode_db)

    @staticmethod
    def encoded_header(credentials: str, encoding: str) -> str:
        return "Basic " + base64.b64encode(credentials.encode(encoding)).decode("ascii")

    def test_utf8_user_authenticates_with_utf8_header(
        self, middleware: AuthMiddleware
    ) -> None:
        """M1a: UTF-8-created user authenticates with a UTF-8 encoded header."""
        environ = make_environ(
            auth_header=self.encoded_header("umlaut:pässwörd", "utf-8")
        )
        result = middleware.authenticate(environ)

        assert result.authenticated is True
        assert result.role == "registered"
        assert result.username == "umlaut"

    def test_latin1_header_is_rejected_as_malformed(
        self, middleware: AuthMiddleware
    ) -> None:
        """A Latin-1 encoded header is malformed -> 401, never anonymous."""
        environ = make_environ(
            method="PROPFIND",
            path="/mokuro-reader/",
            auth_header=self.encoded_header("umlaut:pässwörd", "latin-1"),
        )
        result = middleware.authenticate(environ)
        assert result.authenticated is False
        assert result.error == "Invalid authorization header"

        start_response = MockStartResponse()
        middleware(environ, start_response)
        assert start_response.status_code == 401
        www_auth = start_response.get_header("WWW-Authenticate")
        assert www_auth is not None and 'charset="UTF-8"' in www_auth

    def test_exactly_one_password_check_per_request(
        self, middleware: AuthMiddleware, unicode_db: Database
    ) -> None:
        """Only the UTF-8 interpretation is ever attempted (no fallback)."""
        attempts: list[tuple[str, str]] = []
        original = unicode_db.authenticate_user

        def spy(username: str, password: str):
            attempts.append((username, password))
            return original(username, password)

        unicode_db.authenticate_user = spy  # type: ignore[method-assign]
        try:
            environ = make_environ(
                auth_header=self.encoded_header("umlaut:pässwörd", "utf-8")
            )
            result = middleware.authenticate(environ)
        finally:
            unicode_db.authenticate_user = original  # type: ignore[method-assign]

        assert result.authenticated is True
        assert result.user is not None
        assert result.user["username"] == "umlaut"
        assert attempts == [("umlaut", "pässwörd")]

    def test_ascii_credentials_regression(self, middleware: AuthMiddleware) -> None:
        """M2: plain-ASCII credentials behave exactly as before."""
        environ = make_environ(
            auth_header=make_basic_auth_header("ascii_user", "plainpass")
        )
        result = middleware.authenticate(environ)
        assert result.authenticated is True
        assert result.role == "registered"

        environ = make_environ(
            auth_header=make_basic_auth_header("ascii_user", "wrongpass")
        )
        result = middleware.authenticate(environ)
        assert result.authenticated is False
        assert result.error == "Invalid credentials"

    def test_garbage_header_yields_401_not_anonymous(
        self, middleware: AuthMiddleware
    ) -> None:
        """M3a: present-but-garbage Authorization header -> 401, never anonymous."""
        environ = make_environ(
            method="PROPFIND",
            path="/mokuro-reader/",
            auth_header="Basic !!!notb64!!!",
        )
        result = middleware.authenticate(environ)
        assert result.authenticated is False
        assert result.error == "Invalid authorization header"

        start_response = MockStartResponse()
        body = b"".join(middleware(environ, start_response))
        assert start_response.status is not None
        assert start_response.status.startswith("401")
        assert b"Invalid authorization header" in body

    def test_no_header_still_anonymous_browse(
        self, middleware: AuthMiddleware
    ) -> None:
        """M3b: no Authorization header stays anonymous (browse allowed)."""
        environ = make_environ(method="PROPFIND", path="/mokuro-reader/")
        result = middleware.authenticate(environ)
        assert result.authenticated is False
        assert result.role == "anonymous"
        assert result.error is None

        start_response = MockStartResponse()
        middleware(environ, start_response)
        assert start_response.status_code == 200  # passthrough to dummy app

    def test_wrong_password_utf8_fails_with_invalid_credentials(
        self, middleware: AuthMiddleware
    ) -> None:
        """UTF-8 encoded wrong password -> Invalid credentials (not malformed)."""
        environ = make_environ(
            auth_header=self.encoded_header("umlaut:wröng", "utf-8")
        )
        result = middleware.authenticate(environ)
        assert result.authenticated is False
        assert result.error == "Invalid credentials"

    def test_limiter_counts_one_failure_per_request(
        self,
        middleware: AuthMiddleware,
        fresh_rate_limiter: AuthAttemptLimiter,
    ) -> None:
        """Each failing request records exactly ONE limiter failure."""
        environ = make_environ(
            auth_header=self.encoded_header("umlaut:wröng", "utf-8")
        )
        environ["REMOTE_ADDR"] = "192.0.2.50"

        # 9 failing requests: all plain 401s
        for _ in range(9):
            result = middleware.authenticate(environ)
            assert result.error == "Invalid credentials"

        # 10th request still allowed (only 9 failures recorded so far)
        result = middleware.authenticate(environ)
        assert result.error == "Invalid credentials"

        # 11th request: 10 failures recorded -> blocked
        result = middleware.authenticate(environ)
        assert result.error is not None
        assert "Too many failed attempts" in result.error

        # Limiter keys on the UTF-8 (primary) username
        assert "192.0.2.50:umlaut" in fresh_rate_limiter._failures

    def test_garbage_header_records_no_limiter_failure(
        self,
        middleware: AuthMiddleware,
        fresh_rate_limiter: AuthAttemptLimiter,
    ) -> None:
        """Malformed headers (garbage or Latin-1) never count toward the rate limit."""
        malformed_headers = [
            "Basic !!!notb64!!!",
            self.encoded_header("umlaut:pässwörd", "latin-1"),
        ]
        for auth_header in malformed_headers:
            environ = make_environ(
                method="PROPFIND",
                path="/mokuro-reader/",
                auth_header=auth_header,
            )
            environ["REMOTE_ADDR"] = "192.0.2.51"

            for _ in range(20):
                start_response = MockStartResponse()
                middleware(environ, start_response)
                assert start_response.status_code == 401

        assert fresh_rate_limiter._failures == {}
        assert fresh_rate_limiter._blocked_until == {}

    def test_www_authenticate_includes_charset(
        self, middleware: AuthMiddleware
    ) -> None:
        """S3: 401 responses advertise charset=UTF-8 (RFC 7617)."""
        environ = make_environ(
            auth_header=make_basic_auth_header("ascii_user", "wrongpass")
        )
        start_response = MockStartResponse()
        middleware(environ, start_response)

        assert start_response.status_code == 401
        www_auth = start_response.get_header("WWW-Authenticate")
        assert www_auth == f'Basic realm="{middleware.realm}", charset="UTF-8"'

    def test_queue_style_consumer_unchanged(self, unicode_db: Database) -> None:
        """authenticate_basic_header keeps its .authenticated bool semantics."""
        from mokuro_bunko.middleware.auth import authenticate_basic_header

        result = authenticate_basic_header(unicode_db, "Basic !!!notb64!!!")
        assert result.authenticated is False

        header = self.encoded_header("umlaut:pässwörd", "latin-1")
        result = authenticate_basic_header(unicode_db, header)
        assert result.authenticated is False

        header = self.encoded_header("umlaut:pässwörd", "utf-8")
        result = authenticate_basic_header(unicode_db, header)
        assert result.authenticated is True
        assert result.role == "registered"


class TestUtf8FullStack:
    """Full create_app() stack tests for UTF-8 auth and Latin-1 rejection."""

    @pytest.fixture(autouse=True)
    def fresh_rate_limiter(self, monkeypatch: pytest.MonkeyPatch) -> AuthAttemptLimiter:
        import mokuro_bunko.middleware.auth as auth_module

        limiter = AuthAttemptLimiter()
        monkeypatch.setattr(auth_module, "AUTH_RATE_LIMITER", limiter)
        return limiter

    @pytest.fixture
    def full_storage(self, temp_dir: Path) -> Path:
        storage = temp_dir / "storage"
        (storage / "library").mkdir(parents=True)
        (storage / "library" / "manga1.cbz").write_bytes(b"fake cbz content")
        (storage / "inbox").mkdir()
        (storage / "users" / "umlaut").mkdir(parents=True)
        (storage / "users" / "umlaut" / "volume-data.json").write_bytes(
            b"umlaut progress"
        )
        return storage

    @pytest.fixture
    def full_client(self, full_storage: Path):
        from mokuro_bunko.config import Config, StorageConfig
        from mokuro_bunko.server import create_app
        from tests.integration.test_webdav_ops import WSGITestClient

        db = Database(full_storage / "mokuro.db")
        db.create_user("umlaut", "pässwörd", "registered")
        app = create_app(Config(storage=StorageConfig(base_path=full_storage)))
        return WSGITestClient(app)

    @staticmethod
    def encoded_header(credentials: str, encoding: str) -> str:
        return "Basic " + base64.b64encode(credentials.encode(encoding)).decode("ascii")

    def test_full_stack_utf8_put_progress_file(
        self, full_client, full_storage: Path
    ) -> None:
        """UTF-8 header with non-ASCII password authorizes the PUT end-to-end."""
        response = full_client.put(
            "/mokuro-reader/volume-data.json",
            content=b"new progress data",
            headers={"Authorization": self.encoded_header("umlaut:pässwörd", "utf-8")},
        )
        assert 200 <= response.status_code < 300

        created = full_storage / "users" / "umlaut" / "volume-data.json"
        assert created.read_bytes() == b"new progress data"

    def test_full_stack_latin1_put_rejected(self, full_client, full_storage: Path) -> None:
        """Latin-1 header (correct password, wrong encoding) -> 401, no write."""
        response = full_client.put(
            "/mokuro-reader/volume-data.json",
            content=b"should not be written",
            headers={"Authorization": self.encoded_header("umlaut:pässwörd", "latin-1")},
        )
        assert response.status_code == 401

        existing = full_storage / "users" / "umlaut" / "volume-data.json"
        assert existing.read_bytes() == b"umlaut progress"

    def test_utf8_user_gets_propfind_cache_injection(self, full_client) -> None:
        """UTF-8 authenticated PROPFIND gets per-user progress injection."""
        anonymous = full_client.request(
            "PROPFIND", "/mokuro-reader", headers={"Depth": "infinity"}
        )
        assert anonymous.status_code == 207
        assert "volume-data.json" not in anonymous.text

        authenticated = full_client.request(
            "PROPFIND",
            "/mokuro-reader",
            headers={
                "Depth": "infinity",
                "Authorization": self.encoded_header("umlaut:pässwörd", "utf-8"),
            },
        )
        assert authenticated.status_code == 207
        assert "volume-data.json" in authenticated.text
