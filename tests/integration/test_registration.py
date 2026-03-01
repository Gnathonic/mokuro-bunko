"""Integration tests for user registration."""

from __future__ import annotations

import io
import json
from pathlib import Path
from typing import Any, Callable

import pytest

from mokuro_bunko.config import Config, RegistrationConfig, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.registration.api import RegistrationAPI
from mokuro_bunko.registration.invites import InviteManager


class WSGITestClient:
    """Simple WSGI test client."""

    def __init__(self, app: Callable[..., Any]) -> None:
        self.app = app

    def request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        json_body: dict[str, Any] | None = None,
    ) -> "WSGIResponse":
        """Make a request to the WSGI app."""
        headers = headers or {}
        content = b""

        if json_body is not None:
            content = json.dumps(json_body).encode("utf-8")
            headers["Content-Type"] = "application/json"

        environ = {
            "REQUEST_METHOD": method,
            "SCRIPT_NAME": "",
            "PATH_INFO": path,
            "QUERY_STRING": "",
            "SERVER_NAME": "localhost",
            "SERVER_PORT": "8080",
            "SERVER_PROTOCOL": "HTTP/1.1",
            "wsgi.version": (1, 0),
            "wsgi.url_scheme": "http",
            "wsgi.input": io.BytesIO(content),
            "wsgi.errors": io.StringIO(),
            "wsgi.multithread": False,
            "wsgi.multiprocess": False,
            "wsgi.run_once": False,
            "CONTENT_LENGTH": str(len(content)),
            "CONTENT_TYPE": headers.get("Content-Type", "application/octet-stream"),
        }

        # Add headers
        for key, value in headers.items():
            key_upper = key.upper().replace("-", "_")
            if key_upper not in ("CONTENT_TYPE", "CONTENT_LENGTH"):
                environ[f"HTTP_{key_upper}"] = value

        response = WSGIResponse()
        result = self.app(environ, response.start_response)

        body_parts = []
        try:
            for chunk in result:
                body_parts.append(chunk)
        finally:
            if hasattr(result, "close"):
                result.close()

        response.content = b"".join(body_parts)
        return response

    def post(
        self,
        path: str,
        json_body: dict[str, Any] | None = None,
        headers: dict[str, str] | None = None,
    ) -> "WSGIResponse":
        """Make a POST request."""
        return self.request("POST", path, headers, json_body)

    def get(
        self,
        path: str,
        headers: dict[str, str] | None = None,
    ) -> "WSGIResponse":
        """Make a GET request."""
        return self.request("GET", path, headers)


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
        """Extract status code from status string."""
        return int(self.status.split()[0])

    def json(self) -> dict[str, Any]:
        """Parse response body as JSON."""
        return json.loads(self.content.decode("utf-8"))

    def get_header(self, name: str) -> str | None:
        """Get header value by name (case-insensitive)."""
        name_lower = name.lower()
        for header_name, value in self.headers:
            if header_name.lower() == name_lower:
                return value
        return None


def dummy_app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    """Dummy WSGI app that returns 404."""
    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b"Not found"]


@pytest.fixture
def test_storage(temp_dir: Path) -> Path:
    """Create test storage directory."""
    storage = temp_dir / "storage"
    (storage / "library").mkdir(parents=True)
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    return storage


@pytest.fixture
def test_db(test_storage: Path) -> Database:
    """Create test database."""
    return Database(test_storage / "mokuro.db")


@pytest.fixture
def invites(test_db: Database) -> InviteManager:
    """Create invite manager."""
    return InviteManager(test_db)


class TestSelfRegistration:
    """Tests for self-registration mode."""

    @pytest.fixture
    def client(self, test_db: Database) -> WSGITestClient:
        """Create test client with self-registration enabled."""
        config = RegistrationConfig(mode="self", default_role="registered")
        app = RegistrationAPI(dummy_app, test_db, config)
        return WSGITestClient(app)

    def test_register_success(self, client: WSGITestClient) -> None:
        """Test successful registration."""
        response = client.post(
            "/api/register",
            json_body={"username": "newuser", "password": "securepass"},
        )
        assert response.status_code == 201
        data = response.json()
        assert data["success"] is True
        assert data["username"] == "newuser"
        assert data["status"] == "active"

    def test_register_creates_user_in_db(
        self, client: WSGITestClient, test_db: Database
    ) -> None:
        """Test registration creates user in database."""
        client.post(
            "/api/register",
            json_body={"username": "dbuser", "password": "securepass"},
        )

        user = test_db.get_user("dbuser")
        assert user is not None
        assert user["username"] == "dbuser"
        assert user["role"] == "registered"
        assert user["status"] == "active"

    def test_register_password_hashed(
        self, client: WSGITestClient, test_db: Database
    ) -> None:
        """Test password is hashed (can authenticate)."""
        client.post(
            "/api/register",
            json_body={"username": "hashuser", "password": "mypassword"},
        )

        user = test_db.authenticate_user("hashuser", "mypassword")
        assert user is not None

    def test_register_duplicate_username(self, client: WSGITestClient) -> None:
        """Test duplicate username returns 409."""
        client.post(
            "/api/register",
            json_body={"username": "dupuser", "password": "pass1234"},
        )

        response = client.post(
            "/api/register",
            json_body={"username": "dupuser", "password": "pass5678"},
        )

        assert response.status_code == 409
        assert "already exists" in response.json()["error"]

    def test_register_missing_username(self, client: WSGITestClient) -> None:
        """Test missing username returns 400."""
        response = client.post(
            "/api/register",
            json_body={"password": "securepass"},
        )
        assert response.status_code == 400
        assert "Username" in response.json()["error"]

    def test_register_missing_password(self, client: WSGITestClient) -> None:
        """Test missing password returns 400."""
        response = client.post(
            "/api/register",
            json_body={"username": "newuser"},
        )
        assert response.status_code == 400
        assert "Password" in response.json()["error"]

    def test_register_short_password(self, client: WSGITestClient) -> None:
        """Test short password returns 400."""
        response = client.post(
            "/api/register",
            json_body={"username": "newuser", "password": "abc"},
        )
        assert response.status_code == 400
        assert "8 characters" in response.json()["error"]

    def test_register_invalid_username(self, client: WSGITestClient) -> None:
        """Test invalid username format returns 400."""
        response = client.post(
            "/api/register",
            json_body={"username": "ab", "password": "securepass"},
        )
        assert response.status_code == 400
        assert "3-32 characters" in response.json()["error"]

    def test_register_username_special_chars(self, client: WSGITestClient) -> None:
        """Test username with special chars returns 400."""
        response = client.post(
            "/api/register",
            json_body={"username": "user@name", "password": "securepass"},
        )
        assert response.status_code == 400

    def test_register_invalid_json(self, client: WSGITestClient) -> None:
        """Test invalid JSON returns 400."""
        response = client.request(
            "POST",
            "/api/register",
            headers={"Content-Type": "application/json"},
        )
        assert response.status_code == 400
        assert "Invalid JSON" in response.json()["error"]


class TestDisabledRegistration:
    """Tests for disabled registration mode."""

    @pytest.fixture
    def client(self, test_db: Database) -> WSGITestClient:
        """Create test client with registration disabled."""
        config = RegistrationConfig(mode="disabled")
        app = RegistrationAPI(dummy_app, test_db, config)
        return WSGITestClient(app)

    def test_register_disabled_returns_403(self, client: WSGITestClient) -> None:
        """Test registration returns 403 when disabled."""
        response = client.post(
            "/api/register",
            json_body={"username": "newuser", "password": "securepass"},
        )
        assert response.status_code == 403
        assert "disabled" in response.json()["error"]


class TestInviteRegistration:
    """Tests for invite-based registration."""

    @pytest.fixture
    def client(
        self, test_db: Database, invites: InviteManager
    ) -> WSGITestClient:
        """Create test client with invite registration."""
        config = RegistrationConfig(mode="invite", default_role="registered")
        app = RegistrationAPI(dummy_app, test_db, config)
        return WSGITestClient(app)

    def test_register_with_valid_invite(
        self, client: WSGITestClient, invites: InviteManager
    ) -> None:
        """Test registration with valid invite code."""
        code = invites.create_invite(role="uploader")

        response = client.post(
            "/api/register",
            json_body={
                "username": "inviteuser",
                "password": "securepass",
                "invite_code": code,
            },
        )

        assert response.status_code == 201
        data = response.json()
        assert data["success"] is True
        assert data["status"] == "active"

    def test_register_uses_invite_role(
        self, client: WSGITestClient, invites: InviteManager, test_db: Database
    ) -> None:
        """Test registration uses invite's role."""
        code = invites.create_invite(role="editor")

        client.post(
            "/api/register",
            json_body={
                "username": "roleuser",
                "password": "securepass",
                "invite_code": code,
            },
        )

        user = test_db.get_user("roleuser")
        assert user is not None
        assert user["role"] == "editor"

    def test_register_marks_invite_used(
        self, client: WSGITestClient, invites: InviteManager
    ) -> None:
        """Test registration marks invite as used."""
        code = invites.create_invite()

        client.post(
            "/api/register",
            json_body={
                "username": "useduser",
                "password": "securepass",
                "invite_code": code,
            },
        )

        info = invites.get_info(code)
        assert info is not None
        assert info["used_by"] == "useduser"
        assert info["status"] == "used"

    def test_register_without_invite_returns_400(
        self, client: WSGITestClient
    ) -> None:
        """Test registration without invite code returns 400."""
        response = client.post(
            "/api/register",
            json_body={"username": "nocode", "password": "securepass"},
        )
        assert response.status_code == 400
        assert "Invite code is required" in response.json()["error"]

    def test_register_invalid_invite_returns_400(
        self, client: WSGITestClient
    ) -> None:
        """Test registration with invalid invite returns 400."""
        response = client.post(
            "/api/register",
            json_body={
                "username": "badcode",
                "password": "securepass",
                "invite_code": "invalid-code",
            },
        )
        assert response.status_code == 400
        assert "Invalid or expired" in response.json()["error"]

    def test_register_used_invite_returns_400(
        self, client: WSGITestClient, invites: InviteManager, test_db: Database
    ) -> None:
        """Test registration with used invite returns 400."""
        code = invites.create_invite()

        # Register first user
        client.post(
            "/api/register",
            json_body={
                "username": "firstuser",
                "password": "securepass",
                "invite_code": code,
            },
        )

        # Try to use same invite
        response = client.post(
            "/api/register",
            json_body={
                "username": "seconduser",
                "password": "securepass",
                "invite_code": code,
            },
        )

        assert response.status_code == 400
        assert "Invalid or expired" in response.json()["error"]


class TestApprovalRegistration:
    """Tests for approval-based registration."""

    @pytest.fixture
    def client(self, test_db: Database) -> WSGITestClient:
        """Create test client with approval registration."""
        config = RegistrationConfig(mode="approval", default_role="registered")
        app = RegistrationAPI(dummy_app, test_db, config)
        return WSGITestClient(app)

    def test_register_creates_pending_user(
        self, client: WSGITestClient, test_db: Database
    ) -> None:
        """Test registration creates user with pending status."""
        response = client.post(
            "/api/register",
            json_body={"username": "pendinguser", "password": "securepass"},
        )

        assert response.status_code == 201
        data = response.json()
        assert data["status"] == "pending"
        assert "approval" in data["message"]

        user = test_db.get_user("pendinguser")
        assert user is not None
        assert user["status"] == "pending"

    def test_pending_user_cannot_authenticate(
        self, client: WSGITestClient, test_db: Database
    ) -> None:
        """Test pending user cannot authenticate."""
        client.post(
            "/api/register",
            json_body={"username": "pendauth", "password": "securepass"},
        )

        user = test_db.authenticate_user("pendauth", "securepass")
        assert user is None  # Pending users can't authenticate

    def test_approved_user_can_authenticate(
        self, client: WSGITestClient, test_db: Database
    ) -> None:
        """Test approved user can authenticate."""
        client.post(
            "/api/register",
            json_body={"username": "approveduser", "password": "securepass"},
        )

        # Admin approves the user
        test_db.approve_user("approveduser")

        user = test_db.authenticate_user("approveduser", "securepass")
        assert user is not None
        assert user["status"] == "active"


class TestRegistrationInfo:
    """Tests for GET /api/register info endpoint."""

    def test_info_self_mode(self, test_db: Database) -> None:
        """Test info endpoint for self registration."""
        config = RegistrationConfig(mode="self")
        app = RegistrationAPI(dummy_app, test_db, config)
        client = WSGITestClient(app)

        response = client.get("/api/register")
        assert response.status_code == 200
        data = response.json()
        assert data["mode"] == "self"
        assert data["enabled"] is True

    def test_info_disabled_mode(self, test_db: Database) -> None:
        """Test info endpoint for disabled registration."""
        config = RegistrationConfig(mode="disabled")
        app = RegistrationAPI(dummy_app, test_db, config)
        client = WSGITestClient(app)

        response = client.get("/api/register")
        assert response.status_code == 200
        data = response.json()
        assert data["mode"] == "disabled"
        assert data["enabled"] is False

    def test_info_invite_mode(self, test_db: Database) -> None:
        """Test info endpoint for invite registration."""
        config = RegistrationConfig(mode="invite")
        app = RegistrationAPI(dummy_app, test_db, config)
        client = WSGITestClient(app)

        response = client.get("/api/register")
        assert response.status_code == 200
        data = response.json()
        assert data["mode"] == "invite"
        assert data["requires_invite"] is True


class TestRegistrationPassthrough:
    """Tests for passthrough to wrapped app."""

    def test_other_paths_passthrough(self, test_db: Database) -> None:
        """Test other paths are passed to wrapped app."""
        config = RegistrationConfig(mode="self")
        app = RegistrationAPI(dummy_app, test_db, config)
        client = WSGITestClient(app)

        response = client.get("/other/path")
        assert response.status_code == 404  # From dummy_app


class TestUsernameValidation:
    """Tests for username validation rules."""

    @pytest.fixture
    def client(self, test_db: Database) -> WSGITestClient:
        """Create test client."""
        config = RegistrationConfig(mode="self")
        app = RegistrationAPI(dummy_app, test_db, config)
        return WSGITestClient(app)

    def test_valid_username_alphanumeric(self, client: WSGITestClient) -> None:
        """Test alphanumeric username is valid."""
        response = client.post(
            "/api/register",
            json_body={"username": "user123", "password": "securepass"},
        )
        assert response.status_code == 201

    def test_valid_username_with_underscore(self, client: WSGITestClient) -> None:
        """Test username with underscore is valid."""
        response = client.post(
            "/api/register",
            json_body={"username": "user_name", "password": "securepass"},
        )
        assert response.status_code == 201

    def test_valid_username_with_hyphen(self, client: WSGITestClient) -> None:
        """Test username with hyphen is valid."""
        response = client.post(
            "/api/register",
            json_body={"username": "user-name", "password": "securepass"},
        )
        assert response.status_code == 201

    def test_invalid_username_too_short(self, client: WSGITestClient) -> None:
        """Test username too short is invalid."""
        response = client.post(
            "/api/register",
            json_body={"username": "ab", "password": "securepass"},
        )
        assert response.status_code == 400

    def test_invalid_username_too_long(self, client: WSGITestClient) -> None:
        """Test username too long is invalid."""
        response = client.post(
            "/api/register",
            json_body={"username": "a" * 33, "password": "securepass"},
        )
        assert response.status_code == 400

    def test_invalid_username_spaces(self, client: WSGITestClient) -> None:
        """Test username with spaces is invalid."""
        response = client.post(
            "/api/register",
            json_body={"username": "user name", "password": "securepass"},
        )
        assert response.status_code == 400

    def test_invalid_username_special_chars(self, client: WSGITestClient) -> None:
        """Test username with special chars is invalid."""
        for char in ["@", "!", "#", "$", "%", ".", "/"]:
            response = client.post(
                "/api/register",
                json_body={"username": f"user{char}name", "password": "securepass"},
            )
            assert response.status_code == 400, f"Username with '{char}' should be invalid"


class TestDefaultRole:
    """Tests for default role assignment."""

    def test_self_registration_uses_default_role(
        self, test_db: Database
    ) -> None:
        """Test self registration uses configured default role."""
        config = RegistrationConfig(mode="self", default_role="uploader")
        app = RegistrationAPI(dummy_app, test_db, config)
        client = WSGITestClient(app)

        client.post(
            "/api/register",
            json_body={"username": "writeruser", "password": "securepass"},
        )

        user = test_db.get_user("writeruser")
        assert user is not None
        assert user["role"] == "uploader"

    def test_approval_registration_uses_default_role(
        self, test_db: Database
    ) -> None:
        """Test approval registration uses configured default role."""
        config = RegistrationConfig(mode="approval", default_role="editor")
        app = RegistrationAPI(dummy_app, test_db, config)
        client = WSGITestClient(app)

        client.post(
            "/api/register",
            json_body={"username": "editoruser", "password": "securepass"},
        )

        user = test_db.get_user("editoruser")
        assert user is not None
        assert user["role"] == "editor"
