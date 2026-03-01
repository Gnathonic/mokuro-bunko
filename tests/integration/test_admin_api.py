"""Integration tests for admin REST API."""

from __future__ import annotations

import io
import json
from pathlib import Path
from typing import Any, Callable

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import AdminConfig, Config
from mokuro_bunko.database import Database


class WSGITestClient:
    """Simple WSGI test client."""

    def __init__(self, app: Callable[..., Any], role: str = "admin") -> None:
        self.app = app
        self.role = role

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
            # Simulated auth info
            "mokuro.role": self.role,
            "mokuro.username": "admin" if self.role == "admin" else "user",
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

    def get(self, path: str) -> "WSGIResponse":
        return self.request("GET", path)

    def post(self, path: str, json_body: dict[str, Any] | None = None) -> "WSGIResponse":
        return self.request("POST", path, json_body=json_body)

    def put(self, path: str, json_body: dict[str, Any] | None = None) -> "WSGIResponse":
        return self.request("PUT", path, json_body=json_body)

    def delete(self, path: str) -> "WSGIResponse":
        return self.request("DELETE", path)


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
def admin_config() -> AdminConfig:
    """Create admin config."""
    return AdminConfig(enabled=True, path="/_admin")


@pytest.fixture
def admin_app(test_db: Database, admin_config: AdminConfig) -> AdminAPI:
    """Create admin API app."""
    return AdminAPI(dummy_app, test_db, admin_config)


@pytest.fixture
def client(admin_app: AdminAPI) -> WSGITestClient:
    """Create test client with admin role."""
    return WSGITestClient(admin_app, role="admin")


@pytest.fixture
def non_admin_client(admin_app: AdminAPI) -> WSGITestClient:
    """Create test client with non-admin role."""
    return WSGITestClient(admin_app, role="registered")


@pytest.fixture
def inviter_client(admin_app: AdminAPI) -> WSGITestClient:
    """Create test client with inviter role."""
    return WSGITestClient(admin_app, role="inviter")


class TestAdminAuthorization:
    """Tests for admin authorization."""

    def test_admin_can_access(self, client: WSGITestClient) -> None:
        """Test admin can access admin API."""
        response = client.get("/_admin/api/users")
        assert response.status_code == 200

    def test_non_admin_denied(self, non_admin_client: WSGITestClient) -> None:
        """Test non-admin is denied access."""
        response = non_admin_client.get("/_admin/api/users")
        assert response.status_code == 403
        assert "Admin access required" in response.json()["error"]

    def test_anonymous_denied(self, admin_app: AdminAPI) -> None:
        """Test anonymous is denied access."""
        client = WSGITestClient(admin_app, role="anonymous")
        response = client.get("/_admin/api/users")
        assert response.status_code == 403


class TestUsersAPI:
    """Tests for users API endpoints."""

    def test_list_users_empty(self, client: WSGITestClient) -> None:
        """Test listing with no users."""
        response = client.get("/_admin/api/users")
        assert response.status_code == 200
        assert response.json()["users"] == []

    def test_list_users(self, client: WSGITestClient, test_db: Database) -> None:
        """Test listing users."""
        test_db.create_user("user1", "pass1234", role="registered")
        test_db.create_user("user2", "pass1234", role="uploader")

        response = client.get("/_admin/api/users")
        assert response.status_code == 200

        users = response.json()["users"]
        assert len(users) == 2
        usernames = [u["username"] for u in users]
        assert "user1" in usernames
        assert "user2" in usernames

    def test_create_user(self, client: WSGITestClient, test_db: Database) -> None:
        """Test creating a user."""
        response = client.post("/_admin/api/users", {
            "username": "newuser",
            "password": "pass1234",
            "role": "uploader",
        })
        assert response.status_code == 201
        assert response.json()["success"] is True

        user = test_db.get_user("newuser")
        assert user is not None
        assert user["role"] == "uploader"

    def test_create_user_missing_username(self, client: WSGITestClient) -> None:
        """Test creating user without username."""
        response = client.post("/_admin/api/users", {
            "password": "pass1234",
        })
        assert response.status_code == 400
        assert "Username" in response.json()["error"]

    def test_create_user_missing_password(self, client: WSGITestClient) -> None:
        """Test creating user without password."""
        response = client.post("/_admin/api/users", {
            "username": "newuser",
        })
        assert response.status_code == 400
        assert "Password" in response.json()["error"]

    def test_create_user_duplicate(
        self, client: WSGITestClient, test_db: Database
    ) -> None:
        """Test creating duplicate user."""
        test_db.create_user("existing", "pass1234")

        response = client.post("/_admin/api/users", {
            "username": "existing",
            "password": "pass1234",
        })
        assert response.status_code == 409

    def test_create_user_invalid_username(self, client: WSGITestClient) -> None:
        """Test admin cannot create path-like usernames."""
        response = client.post("/_admin/api/users", {
            "username": "../escape",
            "password": "pass1234",
        })
        assert response.status_code == 400

    def test_delete_user(self, client: WSGITestClient, test_db: Database) -> None:
        """Test deleting a user."""
        test_db.create_user("todelete", "pass1234")

        response = client.delete("/_admin/api/users/todelete")
        assert response.status_code == 200
        assert response.json()["success"] is True

        user = test_db.get_user("todelete")
        assert user is None

    def test_delete_user_not_found(self, client: WSGITestClient) -> None:
        """Test deleting nonexistent user."""
        response = client.delete("/_admin/api/users/nonexistent")
        assert response.status_code == 404

    def test_change_role(self, client: WSGITestClient, test_db: Database) -> None:
        """Test changing user role."""
        test_db.create_user("roleuser", "pass1234", role="registered")

        response = client.put("/_admin/api/users/roleuser/role", {
            "role": "editor",
        })
        assert response.status_code == 200
        assert response.json()["user"]["role"] == "editor"

        user = test_db.get_user("roleuser")
        assert user["role"] == "editor"

    def test_change_role_invalid(self, client: WSGITestClient, test_db: Database) -> None:
        """Test changing to invalid role."""
        test_db.create_user("roleuser2", "pass1234")

        response = client.put("/_admin/api/users/roleuser2/role", {
            "role": "invalid",
        })
        assert response.status_code == 400

    def test_change_role_not_found(self, client: WSGITestClient) -> None:
        """Test changing role for nonexistent user."""
        response = client.put("/_admin/api/users/nonexistent/role", {
            "role": "editor",
        })
        assert response.status_code == 404

    def test_update_user_notes(self, client: WSGITestClient, test_db: Database) -> None:
        """Test updating user notes."""
        test_db.create_user("notesuser", "pass1234", role="registered")
        response = client.put("/_admin/api/users/notesuser/notes", {
            "notes": "Can help with onboarding",
        })
        assert response.status_code == 200
        user = test_db.get_user("notesuser")
        assert user is not None
        assert user["notes"] == "Can help with onboarding"

    def test_approve_user(self, client: WSGITestClient, test_db: Database) -> None:
        """Test approving a pending user."""
        test_db.create_user("pending", "pass1234", status="pending")

        response = client.post("/_admin/api/users/pending/approve", {})
        assert response.status_code == 200
        assert response.json()["user"]["status"] == "active"

    def test_approve_user_not_pending(
        self, client: WSGITestClient, test_db: Database
    ) -> None:
        """Test approving non-pending user."""
        test_db.create_user("active", "pass1234", status="active")

        response = client.post("/_admin/api/users/active/approve", {})
        assert response.status_code == 404

    def test_disable_user(self, client: WSGITestClient, test_db: Database) -> None:
        """Test disabling a user."""
        test_db.create_user("todisable", "pass1234")

        response = client.post("/_admin/api/users/todisable/disable", {})
        assert response.status_code == 200
        assert response.json()["user"]["status"] == "disabled"


class TestInvitesAPI:
    """Tests for invites API endpoints."""

    def test_list_invites_empty(self, client: WSGITestClient) -> None:
        """Test listing with no invites."""
        response = client.get("/_admin/api/invites")
        assert response.status_code == 200
        assert response.json()["invites"] == []

    def test_inviter_can_manage_invites(self, inviter_client: WSGITestClient) -> None:
        """Inviter can create and list invites."""
        create = inviter_client.post("/_admin/api/invites", {
            "role": "registered",
            "expires": "1d",
        })
        assert create.status_code == 201
        code = create.json()["invite"]["code"]

        listed = inviter_client.get("/_admin/api/invites")
        assert listed.status_code == 200
        assert any(inv["code"] == code for inv in listed.json()["invites"])

    def test_inviter_cannot_access_users(self, inviter_client: WSGITestClient) -> None:
        """Inviter cannot access admin-only user endpoints."""
        response = inviter_client.get("/_admin/api/users")
        assert response.status_code == 403

    def test_list_invites(self, client: WSGITestClient, test_db: Database) -> None:
        """Test listing invites."""
        test_db.create_invite(role="registered")
        test_db.create_invite(role="uploader")

        response = client.get("/_admin/api/invites")
        assert response.status_code == 200
        assert len(response.json()["invites"]) == 2

    def test_create_invite(self, client: WSGITestClient) -> None:
        """Test creating an invite."""
        response = client.post("/_admin/api/invites", {
            "role": "uploader",
            "expires": "1d",
        })
        assert response.status_code == 201
        assert response.json()["success"] is True
        assert response.json()["invite"]["role"] == "uploader"
        assert response.json()["invite"]["invited_by"] == "admin"
        assert "code" in response.json()["invite"]

    def test_create_invite_default_values(self, client: WSGITestClient) -> None:
        """Test creating invite with default values."""
        response = client.post("/_admin/api/invites", {})
        assert response.status_code == 201
        assert response.json()["invite"]["role"] == "registered"

    def test_create_invite_invalid_role(self, client: WSGITestClient) -> None:
        """Test creating invite with invalid role."""
        response = client.post("/_admin/api/invites", {
            "role": "admin",  # Admin role not allowed for invites
        })
        assert response.status_code == 400

    def test_delete_invite(self, client: WSGITestClient, test_db: Database) -> None:
        """Test deleting an invite."""
        code = test_db.create_invite()

        response = client.delete(f"/_admin/api/invites/{code}")
        assert response.status_code == 200
        assert response.json()["success"] is True

        invite = test_db.get_invite(code)
        assert invite is None

    def test_delete_invite_not_found(self, client: WSGITestClient) -> None:
        """Test deleting nonexistent invite."""
        response = client.delete("/_admin/api/invites/nonexistent")
        assert response.status_code == 404


class TestStaticFiles:
    """Tests for static file serving."""

    def test_index_html(self, client: WSGITestClient) -> None:
        """Test serving index.html."""
        response = client.get("/_admin/")
        assert response.status_code == 200
        assert b"<!DOCTYPE html>" in response.content

    def test_styles_css(self, client: WSGITestClient) -> None:
        """Test serving styles.css."""
        response = client.get("/_admin/styles.css")
        assert response.status_code == 200
        assert b".admin-container" in response.content

    def test_admin_js(self, client: WSGITestClient) -> None:
        """Test serving admin.js."""
        response = client.get("/_admin/admin.js")
        assert response.status_code == 200
        assert b"function" in response.content

    def test_nonexistent_file_returns_index(self, client: WSGITestClient) -> None:
        """Test nonexistent file returns index.html (SPA routing)."""
        response = client.get("/_admin/nonexistent")
        assert response.status_code == 200
        assert b"<!DOCTYPE html>" in response.content


class TestAPINotFound:
    """Tests for API 404 responses."""

    def test_unknown_api_endpoint(self, client: WSGITestClient) -> None:
        """Test unknown API endpoint returns 404."""
        response = client.get("/_admin/api/unknown")
        assert response.status_code == 404
        assert "not found" in response.json()["error"]


class TestSettingsAPI:
    """Tests for settings API endpoints."""

    def test_get_settings_includes_ocr_runtime(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """Settings payload includes OCR runtime status block."""
        cfg = Config()
        cfg.storage.base_path = test_storage
        ocr_runtime = {"available": True, "launch_only": True, "configured_backend": "cpu"}
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg, ocr_runtime=ocr_runtime)
        client = WSGITestClient(app, role="admin")

        response = client.get("/_admin/api/settings")
        assert response.status_code == 200
        body = response.json()
        assert "ocr_runtime" in body
        assert body["ocr_runtime"]["launch_only"] is True

    def test_update_ocr_backend_rejected(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """Backend changes via admin settings are blocked."""
        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        response = client.put("/_admin/api/settings/ocr", {"backend": "cpu"})
        assert response.status_code == 400
        assert "launch-only" in response.json()["error"]

    def test_update_catalog_settings(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """Catalog settings update persists homepage replacement toggle."""
        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        response = client.put("/_admin/api/settings/catalog", {
            "enabled": True,
            "reader_url": "https://mokuro-reader-tan.vercel.app/",
            "use_as_homepage": True,
        })
        assert response.status_code == 200
        body = response.json()
        assert body["catalog"]["enabled"] is True
        assert body["catalog"]["reader_url"] == "https://mokuro-reader-tan.vercel.app"
        assert body["catalog"]["use_as_homepage"] is True

    def test_get_settings_includes_catalog_homepage_flag(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """Settings payload includes catalog homepage replacement toggle."""
        cfg = Config()
        cfg.storage.base_path = test_storage
        cfg.catalog.enabled = True
        cfg.catalog.use_as_homepage = True
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        response = client.get("/_admin/api/settings")
        assert response.status_code == 200
        body = response.json()
        assert body["catalog"]["enabled"] is True
        assert body["catalog"]["use_as_homepage"] is True

    def test_update_queue_settings(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """Queue settings update persists nav exposure and public access flags."""
        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        response = client.put("/_admin/api/settings/queue", {
            "show_in_nav": True,
            "public_access": False,
        })
        assert response.status_code == 200
        body = response.json()
        assert body["queue"]["show_in_nav"] is True
        assert body["queue"]["public_access"] is False


class TestPassthrough:
    """Tests for passthrough to wrapped app."""

    def test_non_admin_path_passthrough(self, client: WSGITestClient) -> None:
        """Test non-admin paths are passed through."""
        response = client.get("/other/path")
        assert response.status_code == 404  # From dummy_app


class TestDisabledAdmin:
    """Tests for disabled admin panel."""

    def test_disabled_passthrough(self, test_db: Database) -> None:
        """Test disabled admin passes through."""
        config = AdminConfig(enabled=False)
        app = AdminAPI(dummy_app, test_db, config)
        client = WSGITestClient(app)

        response = client.get("/_admin/api/users")
        assert response.status_code == 404  # From dummy_app, not admin


class TestAuditAPI:
    """Tests for audit API endpoints."""

    def test_list_audit_events(self, client: WSGITestClient, test_db: Database) -> None:
        """Audit endpoint returns logged events."""
        test_db.log_audit_event(
            action="upload",
            actor_username="admin",
            target_type="library",
            target_path="/mokuro-reader/demo.cbz",
        )
        response = client.get("/_admin/api/audit")
        assert response.status_code == 200
        body = response.json()
        assert "events" in body
        assert any(event["action"] == "upload" for event in body["events"])
