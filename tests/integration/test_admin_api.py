"""Integration tests for admin REST API."""

from __future__ import annotations

import io
import json
import threading
import zipfile
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import AdminConfig, Config
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.generations import parse_generation_list


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
    ) -> WSGIResponse:
        """Make a request to the WSGI app."""
        headers = headers or {}
        content = b""

        if json_body is not None:
            content = json.dumps(json_body).encode("utf-8")
            headers["Content-Type"] = "application/json"

        path, _, query = path.partition("?")
        environ = {
            "REQUEST_METHOD": method,
            "SCRIPT_NAME": "",
            "PATH_INFO": path,
            "QUERY_STRING": query,
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

    def get(self, path: str) -> WSGIResponse:
        return self.request("GET", path)

    def post(self, path: str, json_body: dict[str, Any] | None = None) -> WSGIResponse:
        return self.request("POST", path, json_body=json_body)

    def put(self, path: str, json_body: dict[str, Any] | None = None) -> WSGIResponse:
        return self.request("PUT", path, json_body=json_body)

    def delete(self, path: str) -> WSGIResponse:
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
        """Test soft-deleting a user."""
        test_db.create_user("todelete", "pass1234")

        response = client.delete("/_admin/api/users/todelete")
        assert response.status_code == 200
        assert response.json()["success"] is True

        user = test_db.get_user("todelete")
        assert user is not None
        assert user["status"] == "deleted"

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

    def test_settings_ocr_keeps_only_the_poll_interval(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """The recipes moved; this endpoint is down to one setting."""
        from unittest.mock import patch

        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        with patch(
            "mokuro_bunko.admin.api.build_ocr_runtime_status", return_value={"available": True}
        ):
            response = client.put("/_admin/api/settings/ocr", {"poll_interval": 45})
        assert response.status_code == 200
        assert response.json()["ocr"]["poll_interval"] == 45
        assert cfg.ocr.poll_interval == 45

        for key, value in (
            ("engines", ["mokuro"]),
            ("detector", "ctd"),
            ("patch_budget", 256),
        ):
            bad = client.put("/_admin/api/settings/ocr", {key: value})
            assert bad.status_code == 400
            # The error must say where the setting went, not just refuse it.
            assert "/api/ocr/generations" in bad.json()["error"]

    def test_settings_ocr_says_char_map_was_removed_rather_than_moved(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """A setting with nowhere to go must not be sent chasing one.

        The other old scalars moved into the generations list, so their 400
        points there. The character map has no new home -- the whole system
        went -- and pointing at the generations list would send the operator
        to an endpoint that refuses it too. The only fix is to delete the key,
        so that is what the error has to say.
        """
        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        bad = client.put("/_admin/api/settings/ocr", {"char_map": "attn"})
        assert bad.status_code == 400
        error = bad.json()["error"]
        assert "removed" in error
        assert "delete the key" in error
        # Emphatically NOT the "it moved" wording: there is nowhere to PUT it.
        assert "/api/ocr/generations" not in error

    def test_get_generations_returns_the_rows_and_the_catalog(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """Everything the table renders: stored fields, derived fields, catalog."""
        cfg = Config()
        cfg.storage.base_path = test_storage
        cfg.ocr.generations = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {
                    "name": "hayai-nova-ctd",
                    "engine": "hayai-nova",
                    "detector": "ctd",
                    "pools": {"stage_workers": {"engine": 1}},
                },
            ]
        )
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        body = client.get("/_admin/api/ocr/generations").json()
        assert [row["name"] for row in body["generations"]] == ["mokuro", "hayai-nova-ctd"]

        primary, composed = body["generations"]
        assert primary["sidecar"] == "<Volume>.mokuro"
        assert primary["effective_detector"] is None
        assert primary["detector_locked"] is True
        # mokuro is a serve process now, so it has a road and stages of its
        # own -- with its engine stage's Workers cell meaning the ENGINE's
        # pipeline width rather than a pool of ours (Addendum 7's one-stage
        # table, wrapped by Addendum 8's feed and post).
        assert primary["road"] == "served"
        assert [stage["key"] for stage in primary["stages"]] == ["feed", "mokuro", "post"]
        assert [stage["workers_means"] for stage in primary["stages"]] == [
            "pool",
            "engine",
            "pool",
        ]
        # And the Device select is on that one stage: feed and post are a file
        # copy and an assembly, with no model to place.
        by_key = {stage["key"]: stage for stage in primary["stages"]}
        assert "cpu" in by_key["mokuro"]["devices_allowed"]
        assert by_key["mokuro"]["device_locked_reason"] is None
        assert by_key["feed"]["devices_allowed"] == []
        assert by_key["post"]["devices_allowed"] == []
        assert primary["congestion"] is None

        assert composed["sidecar"] == "<Volume>.hayai-nova-ctd.mokuro"
        assert composed["effective_detector"] == "ctd"
        assert composed["detector_locked"] is False
        assert composed["patch_budget_applies"] is True
        assert composed["road"] == "adapter"
        assert composed["stages"]
        for stage in composed["stages"]:
            assert set(stage) == {
                "key",
                "name",
                "device",
                "max_workers",
                "derived_workers",
                "derived_capacity",
                "devices_allowed",
                "device_options",
                "device_locked_reason",
                "workers_means",
            }
            assert [o["id"] for o in stage["device_options"]] == stage["devices_allowed"]
        # The explicit width wins over the derivation.
        engine_stage = next(s for s in composed["stages"] if s["key"] == "engine")
        assert engine_stage["derived_workers"] == 1

        catalog = body["catalog"]
        assert {engine["id"] for engine in catalog["engines"]} >= {"mokuro", "hayai-nova"}
        assert {detector["id"] for detector in catalog["detectors"]} >= {"ctd", "ppocr-manga"}
        # The character-map system is gone, so the catalog must not offer a
        # choice the UI would then render as a select nothing can honour.
        assert "char_maps" not in catalog
        assert catalog["patch_budgets"] == [256, 384, 512]
        assert catalog["name_pattern"] == "^[a-z0-9][a-z0-9-]{0,31}$"
        assert catalog["reserved_names"] == ["original", "gcv"]
        assert catalog["reserved_prefixes"] == ["tr-"]
        # Every device a model may be placed on, and which models may not move.
        assert [row["id"] for row in catalog["devices"]][:2] == ["auto", "cpu"]
        by_id = {engine["id"]: engine for engine in catalog["engines"]}
        assert by_id["hayai-nova"]["devices"] == "any"
        assert by_id["ppocr-manga"]["devices"] == ["cpu"]
        detectors = {row["id"]: row for row in catalog["detectors"]}
        assert detectors["ctd"]["devices"] == "any"
        assert detectors["ppocr-manga"]["devices"] == ["cpu"]

    def test_the_mokuro_stage_keeps_its_device_and_workers_cells(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """Addendum 7 through 8: the row's own pipeline is a stage of the road.

        Addendum 7 gave the mokuro row a one-stage table with a Device select
        and a Workers cell that is the fork's ``--num_workers``; Addendum 8
        wrapped that stage with ``feed`` and ``post``. The cells stay on it,
        and the saved choice comes back in ``pools.stage_device``.
        """
        cfg = Config()
        cfg.storage.base_path = test_storage
        cfg.ocr.generations = parse_generation_list(
            [
                {
                    "name": "mokuro",
                    "engine": "mokuro",
                    "primary": True,
                    "pools": {
                        "stage_device": {"mokuro": "cpu"},
                        "stage_workers": {"mokuro": 6},
                    },
                }
            ]
        )
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        (row,) = client.get("/_admin/api/ocr/generations").json()["generations"]
        assert row["road"] == "served"
        stages = {stage["key"]: stage for stage in row["stages"]}
        stage = stages["mokuro"]
        # Where the serve process is started: what the row asked for, not a
        # probe of this server's own environment.
        assert stage["device"] == "cpu"
        assert stage["device_locked_reason"] is None
        assert "cpu" in stage["devices_allowed"]
        # Its width is the fork's own --num_workers, which is why the cell
        # stays editable although one process holds one model.
        assert stage["workers_means"] == "engine"
        assert stage["max_workers"] == 1
        assert row["pools"]["stage_workers"] == {"mokuro": 6}
        assert row["pools"]["stage_device"] == {"mokuro": "cpu"}

    def test_derive_answers_for_a_row_that_was_never_saved(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """The table follows an engine/detector/device change without a save."""
        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        body = client.post(
            "/_admin/api/ocr/generations/derive",
            {
                "spec": {
                    "engine": "hayai-nova",
                    "detector": "ctd",
                    "pools": {"stage_device": {"detect": "cpu"}},
                }
            },
        ).json()
        assert body["road"] == "adapter"
        stages = {stage["key"]: stage for stage in body["stages"]}
        assert stages["detect"]["device"] == "cpu"
        # A CPU detect stage is a pool again, and its Workers box is live.
        assert stages["detect"]["derived_workers"] >= 1
        assert stages["post"]["devices_allowed"] == []

        # The config was not touched by asking.
        assert [row.name for row in cfg.ocr.generations] == ["mokuro"]

    def test_derive_refuses_a_bad_spec_the_way_a_put_would(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        response = client.post(
            "/_admin/api/ocr/generations/derive",
            {"spec": {"engine": "hayai-nova", "pools": {"stage_device": {"post": "cpu"}}}},
        )
        assert response.status_code == 400
        assert response.json()["field"] == "pools"

    def test_devices_can_be_probed_again_on_demand(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        body = client.post("/_admin/api/ocr/devices/refresh", {}).json()
        assert body["success"] is True
        assert [row["id"] for row in body["devices"]][:2] == ["auto", "cpu"]

    def test_generations_report_how_much_of_the_library_each_row_has_done(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """volumes_done / volumes_total come from the shared library scan."""
        from mokuro_bunko.library_index import LibraryIndexCache

        series = test_storage / "library" / "Series"
        series.mkdir(parents=True)
        for index in (1, 2, 3):
            (series / f"Vol {index}.cbz").write_bytes(b"")
        (series / "Vol 1.mokuro").write_text("{}", encoding="utf-8")
        (series / "Vol 2.mokuro").write_text("{}", encoding="utf-8")
        (series / "Vol 1.nova.mokuro").write_text("{}", encoding="utf-8")

        cfg = Config()
        cfg.storage.base_path = test_storage
        cfg.ocr.generations = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "nova", "engine": "hayai-nova"},
            ]
        )
        app = AdminAPI(
            dummy_app,
            test_db,
            admin_config,
            full_config=cfg,
            library_index=LibraryIndexCache(test_storage / "library", ttl=0.0),
        )
        client = WSGITestClient(app, role="admin")

        rows = client.get("/_admin/api/ocr/generations").json()["generations"]
        assert (rows[0]["volumes_done"], rows[0]["volumes_total"]) == (2, 3)
        assert (rows[1]["volumes_done"], rows[1]["volumes_total"]) == (1, 3)

    def test_generations_count_each_machine_s_sidecars_exactly(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """Per machine: the row's sidecars ON DISK that it wrote, from the
        provenance table -- never a lifetime count, never an unknown file."""
        from mokuro_bunko.library_index import LibraryIndexCache

        series = test_storage / "library" / "Series"
        series.mkdir(parents=True)
        for index in (1, 2, 3, 4):
            (series / f"Vol {index}.cbz").write_bytes(b"")
            (series / f"Vol {index}.nova.mokuro").write_text("{}", encoding="utf-8")
        cfg = Config()
        cfg.storage.base_path = test_storage
        cfg.ocr.generations = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "nova", "engine": "hayai-nova"},
            ]
        )
        nova = cfg.ocr.generations[1].id

        def record(volume: int, machine: str) -> None:
            test_db.record_ocr_sidecar({
                "sidecar_path": f"Series/Vol {volume}.nova.mokuro",
                "volume_key": f"Series/Vol {volume}.cbz",
                "generation_id": nova, "generation_name": "nova",
                "machine": machine, "account": None if machine == "local" else "acct",
            })

        record(1, "tower")
        record(2, "tower")
        record(3, "local")
        # Vol 4's sidecar has no record: unknown, attributed to nobody. A
        # record of a volume no longer in the library counts nowhere.
        test_db.record_ocr_sidecar({
            "sidecar_path": "Series/Gone.nova.mokuro", "volume_key": "Series/Gone.cbz",
            "generation_id": nova, "generation_name": "nova", "machine": "tower",
        })
        app = AdminAPI(
            dummy_app, test_db, admin_config, full_config=cfg,
            library_index=LibraryIndexCache(test_storage / "library", ttl=0.0),
        )
        rows = WSGITestClient(app, role="admin").get("/_admin/api/ocr/generations").json()[
            "generations"
        ]
        assert rows[1]["volumes_by_machine"] == {"tower": 2, "local": 1}
        assert sum(rows[1]["volumes_by_machine"].values()) <= rows[1]["volumes_total"]
        assert rows[0]["volumes_by_machine"] == {}

    def test_generations_carry_their_averaged_congestion(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """The Congestion column reads the recorded runs of that row's id."""
        from mokuro_bunko.ocr.congestion import CongestionHistory

        cfg = Config()
        cfg.storage.base_path = test_storage
        cfg.ocr.generations = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "nova", "engine": "hayai-nova"},
            ]
        )
        CongestionHistory(test_storage).record(
            cfg.ocr.generations[1].id,
            {
                "at": 1_790_000_000.0,
                "volume": "Series/Vol 1",
                "pages": 100,
                "elapsed": 120.0,
                "verdict": None,
                "bottleneck": "engine",
                "stages": [
                    {
                        "key": "engine",
                        "name": "engine read",
                        "device": "gpu",
                        "workers": 1,
                        "fused": False,
                        "items": 100,
                        "busy_pct": 90.0,
                        "starved_pct": 1.0,
                        "blocked_pct": 0.0,
                    }
                ],
                "queues": [],
            },
        )
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        rows = client.get("/_admin/api/ocr/generations").json()["generations"]
        assert rows[0]["congestion"] is None
        assert rows[1]["congestion"]["runs"] == 1
        assert rows[1]["congestion"]["bottleneck"] == "engine"
        assert rows[1]["congestion"]["stages"][0]["busy_pct"] == 90

    def test_put_generations_replaces_the_whole_list_in_order(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """A full replacement, because the ORDER is the setting."""
        from unittest.mock import patch

        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")

        with patch(
            "mokuro_bunko.admin.api.build_ocr_runtime_status", return_value={"available": True}
        ):
            response = client.put(
                "/_admin/api/ocr/generations",
                {
                    "generations": [
                        {"name": "nova", "engine": "hayai-nova", "detector": "ctd"},
                        {"name": "mokuro", "engine": "mokuro", "primary": True},
                    ]
                },
            )
        assert response.status_code == 200
        body = response.json()
        assert body["success"] is True
        assert body["restart_required"] is True and body["applied"] is False
        assert [row["name"] for row in body["generations"]] == ["nova", "mokuro"]
        # The response is the GET body plus the outcome fields.
        assert "catalog" in body
        assert [row.name for row in cfg.ocr.generations] == ["nova", "mokuro"]

        # Re-sending exactly what is stored changes nothing.
        with patch(
            "mokuro_bunko.admin.api.build_ocr_runtime_status", return_value={"available": True}
        ):
            again = client.put(
                "/_admin/api/ocr/generations",
                {"generations": [row.to_dict() for row in cfg.ocr.generations]},
            )
        assert again.json()["restart_required"] is False

    def test_put_generations_mints_ids_and_keeps_the_ones_it_is_sent(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """A new row has no id; an existing row keeps the one it always had."""
        from unittest.mock import patch

        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")
        existing = cfg.ocr.generations[0].to_dict()

        with patch(
            "mokuro_bunko.admin.api.build_ocr_runtime_status", return_value={"available": True}
        ):
            body = client.put(
                "/_admin/api/ocr/generations",
                {"generations": [{"name": "nova", "engine": "hayai-nova"}, existing]},
            ).json()
        assert [row["id"] for row in body["generations"]] == ["g-2", "g-1"]

        unknown = client.put(
            "/_admin/api/ocr/generations",
            {"generations": [{"id": "g-99", "name": "x", "engine": "mokuro", "primary": True}]},
        )
        assert unknown.status_code == 400
        assert unknown.json()["row"] == 0 and unknown.json()["field"] == "id"

    def test_put_generations_400_names_the_row_and_the_field(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """The UI highlights a field, so the error has to name one."""
        cfg = Config()
        cfg.storage.base_path = test_storage
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        client = WSGITestClient(app, role="admin")
        before = [row.to_dict() for row in cfg.ocr.generations]

        bad = client.put(
            "/_admin/api/ocr/generations",
            {
                "generations": [
                    {"name": "mokuro", "engine": "mokuro", "primary": True},
                    {"name": "Bad Name", "engine": "hayai-nova"},
                ]
            },
        )
        assert bad.status_code == 400
        assert bad.json()["row"] == 1 and bad.json()["field"] == "name"

        missing = client.put("/_admin/api/ocr/generations", {})
        assert missing.status_code == 400
        assert missing.json()["row"] is None and missing.json()["field"] is None

        no_primary = client.put(
            "/_admin/api/ocr/generations",
            {"generations": [{"name": "nova", "engine": "hayai-nova"}]},
        )
        assert no_primary.status_code == 400
        assert no_primary.json()["field"] == "primary"

        # Nothing was saved by any of them.
        assert [row.to_dict() for row in cfg.ocr.generations] == before

    def test_put_generations_applies_live_through_the_control_handle(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """With a live handle the saved rows are pushed into the worker."""
        from unittest.mock import patch

        class FakeControl:
            def __init__(self) -> None:
                self.calls: list[tuple[list[str], float | None]] = []
                self.runtime: dict[str, Any] = {}

            def apply(
                self, generations: Any, poll_interval: float | None = None
            ) -> dict[str, Any]:
                self.calls.append(([row.name for row in generations], poll_interval))
                return {
                    "applied": True,
                    "installing": False,
                    "restart_required": False,
                    "reason": "",
                }

        cfg = Config()
        cfg.storage.base_path = test_storage
        control = FakeControl()
        app = AdminAPI(
            dummy_app,
            test_db,
            admin_config,
            full_config=cfg,
            ocr_control=control,  # type: ignore[arg-type]
        )
        client = WSGITestClient(app, role="admin")
        with patch(
            "mokuro_bunko.admin.api.build_ocr_runtime_status", return_value={"available": True}
        ):
            response = client.put(
                "/_admin/api/ocr/generations",
                {
                    "generations": [
                        {"name": "half", "engine": "mokuro", "primary": True, "precision": "auto-speed"},
                        {"name": "nova", "engine": "hayai-nova", "detector": "ctd"},
                    ]
                },
            )
        body = response.json()
        assert body["applied"] is True and body["restart_required"] is False
        assert control.calls == [(["half", "nova"], 30.0)]

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

    def test_queue_display_level_round_trips_and_applies_live(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path, temp_dir: Path
    ) -> None:
        """`queue.display`: saved to the file, read back, and on the very next
        queue poll, with no restart -- the queue page reads the same object."""
        from mokuro_bunko.config import load_config
        from mokuro_bunko.ocr.control import OcrControl
        from mokuro_bunko.queue.api import QueueAPI

        cfg = Config()
        cfg.storage.base_path = test_storage
        config_path = temp_dir / "config.yaml"
        control = OcrControl()
        app = AdminAPI(
            dummy_app, test_db, admin_config, full_config=cfg,
            config_path=config_path, ocr_control=control,
        )
        client = WSGITestClient(app, role="admin")
        queue = QueueAPI(
            dummy_app, storage_base_path=str(test_storage),
            queue_config=cfg.queue, ocr_control=control,
        )

        def level() -> str:
            captured: dict[str, Any] = {}
            environ = {
                "REQUEST_METHOD": "GET", "PATH_INFO": "/queue/api/status",
                "QUERY_STRING": "", "wsgi.input": io.BytesIO(b""),
            }
            body = b"".join(queue(environ, lambda s, h: captured.setdefault("s", s)))
            return str(json.loads(body)["level"])

        assert level() == "normal"
        before = control.queue_state.value
        response = client.put("/_admin/api/settings/queue", {"display": "minimal"})
        assert response.status_code == 200
        assert response.json()["queue"]["display"] == "minimal"
        assert control.queue_state.value > before, "a settings change bumps the version"
        assert level() == "minimal"
        assert load_config(config_path).queue.display == "minimal"
        assert client.get("/_admin/api/settings").json()["queue"]["display"] == "minimal"

        refused = client.put("/_admin/api/settings/queue", {"display": "verbose"})
        assert refused.status_code == 400
        assert refused.json()["field"] == "display"
        assert cfg.queue.display == "minimal"


class TestGenerationBenchAPI:
    """`/api/ocr/generations/<id>/bench`: the exact HTTP face of ADDENDUM 1.

    The state machine itself is pinned in `tests/unit/test_ocr_bench.py`;
    what is checked here is the routing, the status codes and the shape the
    admin panel is already built against.
    """

    @staticmethod
    def _app(
        test_db: Database, admin_config: AdminConfig, storage: Path
    ) -> tuple[Any, Any]:
        cfg = Config()
        cfg.storage.base_path = storage
        cfg.ocr.generations = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "hayai-nova", "engine": "hayai-nova"},
            ]
        )
        app = AdminAPI(dummy_app, test_db, admin_config, full_config=cfg)
        return app, cfg

    def test_a_row_that_never_ran_is_idle_and_the_table_says_null(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        app, cfg = self._app(test_db, admin_config, test_storage)
        client = WSGITestClient(app, role="admin")
        row_id = cfg.ocr.generations[1].id

        body = client.get(f"/_admin/api/ocr/generations/{row_id}/bench").json()
        assert body == {
            "state": "idle",
            "generation": row_id,
            "key": row_id,
            "queue": {"running": None, "queued": []},
        }

        rows = client.get("/_admin/api/ocr/generations").json()["generations"]
        assert [row["bench"] for row in rows] == [None, None]

    def test_a_saved_result_rides_the_table_without_its_trials(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        app, cfg = self._app(test_db, admin_config, test_storage)
        client = WSGITestClient(app, role="admin")
        row_id = cfg.ocr.generations[1].id
        (test_storage / ".ocr-bench.json").write_text(
            json.dumps(
                {
                    row_id: {
                        "state": "done",
                        "generation": row_id,
                        "tunable": True,
                        "best": {"pages_per_second": 2.1, "speedup": 1.4,
                                 "same_as_saved": False},
                        "trials": [{"n": 1}],
                        "progress": {"trial": 1},
                    }
                }
            ),
            encoding="utf-8",
        )
        rows = client.get("/_admin/api/ocr/generations").json()["generations"]
        assert rows[0]["bench"] is None
        bench = rows[1]["bench"]
        assert "trials" not in bench
        assert bench["progress"] is None
        assert bench["best"]["speedup"] == 1.4
        # The row's own endpoint still carries the whole thing.
        full = client.get(f"/_admin/api/ocr/generations/{row_id}/bench").json()
        assert full["trials"] == [{"n": 1}]

    def test_starting_one_without_an_ocr_worker_is_a_400(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        app, cfg = self._app(test_db, admin_config, test_storage)
        client = WSGITestClient(app, role="admin")
        row_id = cfg.ocr.generations[1].id
        response = client.post(f"/_admin/api/ocr/generations/{row_id}/bench", {})
        assert response.status_code == 400
        assert "OCR is disabled" in response.json()["error"]

    def test_an_unknown_row_is_a_400_and_a_cancel_of_nothing_too(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        app, _ = self._app(test_db, admin_config, test_storage)
        client = WSGITestClient(app, role="admin")
        unknown = client.post("/_admin/api/ocr/generations/g-99/bench", {})
        assert unknown.status_code == 400
        assert "no generation" in unknown.json()["error"]
        cancel = client.delete("/_admin/api/ocr/generations/g-99/bench")
        assert cancel.status_code == 400

    def test_a_started_benchmark_answers_202_with_the_bench_object(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """POST → 202 and `state: queued`, GET → the same object, DELETE → cancelled.

        ADDENDUM 6: a SECOND row queues behind the first instead of 409 --
        only re-posting the SAME key while it is already queued/running is.
        """
        from unittest.mock import patch

        app, cfg = self._app(test_db, admin_config, test_storage)
        row_id = cfg.ocr.generations[1].id
        other_id = cfg.ocr.generations[0].id
        # A benchmark samples real pages, so there has to be something to
        # sample -- an empty library is refused before anything is held.
        volume = test_storage / "library" / "Series" / "Volume 1.cbz"
        volume.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(volume, "w") as zf:
            for n in range(8):
                zf.writestr(f"page_{n:03d}.jpg", b"x" * 16)
        started = threading.Event()
        release = threading.Event()

        class FakeWorker:
            thumbnails_only = False
            processor = SimpleNamespace(engines_python_path=Path("/engines/python"))

            def preempt_for_bench(
                self, timeout: float = 0.0, processor: str = "local"
            ) -> tuple[bool, list[Any]]:
                started.set()
                release.wait(timeout=10)
                return True, []

            def release_queue(self, processor: str = "local") -> None:
                return None

        app.ocr_control = SimpleNamespace(
            worker=FakeWorker(),
            selected_backend="rocm",
            mokuro_installer=None,
            engines_installer=None,
        )
        client = WSGITestClient(app, role="admin")
        with patch("mokuro_bunko.ocr.bench.describe_host", return_value={
            "cpu": "test cpu (8 cores)", "gpu": None, "backend": "rocm"
        }):
            response = client.post(f"/_admin/api/ocr/generations/{row_id}/bench", {})
            assert response.status_code == 202
            body = response.json()
            assert body["generation"] == row_id
            assert body["key"] == row_id
            assert body["position"] == 0
            assert body["state"] == "queued"
            assert body["waiting_for_queue"] is True
            assert body["tunable"] is True
            assert body["progress"] is None
            assert started.wait(timeout=10)

            # Re-posting the SAME key is the only thing refused.
            clash = client.post(f"/_admin/api/ocr/generations/{row_id}/bench", {})
            assert clash.status_code == 409
            assert "already queued or running" in clash.json()["error"]

            # A DIFFERENT row queues behind it instead.
            queued = client.post(f"/_admin/api/ocr/generations/{other_id}/bench", {})
            assert queued.status_code == 202
            assert queued.json()["position"] == 1
            assert queued.json()["state"] == "queued"

            live = client.get(f"/_admin/api/ocr/generations/{row_id}/bench").json()
            assert live["state"] in ("queued", "running")

            # The queued one is removed at once, without touching the fake's
            # broken `_run_mokuro`/`open_bench` (it never got a chance to run).
            removed = client.delete(f"/_admin/api/ocr/generations/{other_id}/bench")
            assert removed.status_code == 200
            assert removed.json()["state"] == "cancelled"

            release.set()
            cancelled = client.delete(f"/_admin/api/ocr/generations/{row_id}/bench")
        assert cancelled.status_code == 200
        assert cancelled.json()["state"] in ("cancelled", "failed")

    def test_a_silly_page_count_is_a_400(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        app, cfg = self._app(test_db, admin_config, test_storage)

        class FakeWorker:
            thumbnails_only = False
            processor = SimpleNamespace(engines_python_path=Path("/engines/python"))

        app.ocr_control = SimpleNamespace(
            worker=FakeWorker(),
            selected_backend="rocm",
            mokuro_installer=None,
            engines_installer=None,
        )
        client = WSGITestClient(app, role="admin")
        row_id = cfg.ocr.generations[1].id
        response = client.post(
            f"/_admin/api/ocr/generations/{row_id}/bench", {"pages": 100000}
        )
        assert response.status_code == 400
        assert "whole number between" in response.json()["error"]

    def test_a_spec_measures_a_hypothetical_row_over_http(
        self, test_db: Database, admin_config: AdminConfig, test_storage: Path
    ) -> None:
        """A `spec` in the POST body measures the row as edited, unsaved."""
        app, _ = self._app(test_db, admin_config, test_storage)
        volume = test_storage / "library" / "Series" / "Volume 1.cbz"
        volume.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(volume, "w") as zf:
            for n in range(8):
                zf.writestr(f"page_{n:03d}.jpg", b"x" * 16)

        class FakeWorker:
            thumbnails_only = False
            processor = SimpleNamespace(engines_python_path=Path("/engines/python"))

            def preempt_for_bench(
                self, timeout: float = 0.0, processor: str = "local"
            ) -> tuple[bool, list[Any]]:
                return True, []

            def release_queue(self, processor: str = "local") -> None:
                return None

        app.ocr_control = SimpleNamespace(
            worker=FakeWorker(),
            selected_backend="rocm",
            mokuro_installer=None,
            engines_installer=None,
        )
        client = WSGITestClient(app, role="admin")
        response = client.post(
            "/_admin/api/ocr/generations/draft-1/bench",
            {"spec": {"engine": "not-a-real-engine"}},
        )
        assert response.status_code == 400
        body = response.json()
        assert body["row"] is None
        assert body["field"] == "engine"


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

    def _seed(self, test_db: Database) -> None:
        test_db.log_audit_event(action="upload", actor_username="alice", target_type="progress",
                                target_path="/mokuro-reader/volume-data.json")
        test_db.log_audit_event(action="upload", actor_username="alice", target_type="library",
                                target_path="/mokuro-reader/S/V1.cbz")
        test_db.log_audit_event(action="ocr_sidecar_rejected", actor_username="tower-acct",
                                target_type="sidecar", target_path="/mokuro-reader/S/V1.mokuro",
                                details={"reason": "not readable JSON"})
        test_db.log_audit_event(action="ocr_sidecar_written", actor_username="tower-acct",
                                target_type="sidecar", target_path="/mokuro-reader/S/V2.mokuro")

    def test_progress_is_left_out_unless_asked(
        self, client: WSGITestClient, test_db: Database
    ) -> None:
        self._seed(test_db)
        body = client.get("/_admin/api/audit").json()
        assert [e["target_type"] for e in body["events"]].count("progress") == 0
        assert body["total"] == 3
        body = client.get("/_admin/api/audit?include_progress=1").json()
        assert body["total"] == 4

    def test_filters_combine_and_page(self, client: WSGITestClient, test_db: Database) -> None:
        self._seed(test_db)
        body = client.get(
            "/_admin/api/audit?action=ocr_sidecar_written,ocr_sidecar_rejected"
            "&target_type=sidecar&q=READABLE&limit=1"
        ).json()
        assert [e["action"] for e in body["events"]] == ["ocr_sidecar_rejected"]
        assert body["next_cursor"] is None
        body = client.get("/_admin/api/audit?action=ocr_sidecar_written"
                          "&action=ocr_sidecar_rejected&limit=1").json()
        assert body["next_cursor"]
        second = client.get(f"/_admin/api/audit?action=ocr_sidecar_written"
                            f"&action=ocr_sidecar_rejected&limit=1"
                            f"&cursor={body['next_cursor']}").json()
        assert second["events"][0]["id"] < body["events"][0]["id"]
        assert "facets" not in second

    def test_facets_ride_the_first_page(self, client: WSGITestClient, test_db: Database) -> None:
        self._seed(test_db)
        facets = client.get("/_admin/api/audit").json()["facets"]
        assert facets["actors"] == ["alice", "tower-acct"]
        assert "ocr_sidecar_rejected" in facets["actions"]
        assert facets["target_types"] == ["library", "progress", "sidecar"]

    def test_a_bad_date_is_a_400(self, client: WSGITestClient, test_db: Database) -> None:
        response = client.get("/_admin/api/audit?since=someday")
        assert response.status_code == 400
        assert "since" in response.json()["error"]

    def test_filters_by_actor(self, client: WSGITestClient, test_db: Database) -> None:
        """How a processor that keeps sending bad files is found: its account."""
        test_db.log_audit_event(action="ocr_sidecar_written", actor_username="tower-acct",
                                target_type="sidecar", target_path="/mokuro-reader/S/V1.mokuro")
        test_db.log_audit_event(action="ocr_sidecar_rejected", actor_username="tower-acct",
                                target_type="sidecar", target_path="/mokuro-reader/S/V2.mokuro")
        test_db.log_audit_event(action="upload", actor_username="alice",
                                target_type="library", target_path="/mokuro-reader/S/V3.cbz")
        body = client.get("/_admin/api/audit?actor=tower-acct").json()
        assert sorted(e["action"] for e in body["events"]) == [
            "ocr_sidecar_rejected", "ocr_sidecar_written"
        ]
