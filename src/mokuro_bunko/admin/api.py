"""Admin REST API for mokuro-bunko."""

from __future__ import annotations

import json
import shutil
import threading
import time
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from mokuro_bunko.config import AdminConfig, Config, save_config
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.installer import (
    OCRInstaller,
    detect_hardware,
    get_backend_unavailable_reasons,
    get_supported_backends,
)
from mokuro_bunko.registration.invites import InviteManager
from mokuro_bunko.security import AuthAttemptLimiter, get_client_ip, is_within_path
from mokuro_bunko.validation import validate_password, validate_username

# Static files directory
STATIC_DIR = Path(__file__).parent / "web"

# MIME types for static files
MIME_TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "application/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".json": "application/json",
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".webp": "image/webp",
    ".ico": "image/x-icon",
    ".svg": "image/svg+xml",
}

MAX_JSON_BODY_BYTES = 64 * 1024
ADMIN_WRITE_RATE_LIMITER = AuthAttemptLimiter(max_failures=120, window_seconds=60, block_seconds=60)


def build_ocr_runtime_status(full_config: Any | None) -> dict[str, Any]:
    """Build OCR runtime status dict (spawns subprocesses — call sparingly)."""
    if not full_config:
        return {"available": False}

    installer = OCRInstaller(output_callback=lambda msg: None)
    hardware = detect_hardware()
    supported = get_supported_backends(hardware=hardware)
    unavailable = get_backend_unavailable_reasons(hardware=hardware)
    installed = installer.is_installed()
    installed_backend = installer.get_installed_backend()

    return {
        "available": True,
        "launch_only": True,
        "configured_backend": full_config.ocr.backend,
        "installed": installed,
        "installed_backend": installed_backend.value if installed_backend else None,
        "env_path": str(installer.env_path),
        "supported_backends": [b.value for b in supported],
        "unavailable_backends": {k.value: v for k, v in unavailable.items()},
        "cli_hint": "Use `mokuro-bunko serve --ocr <auto|cuda|rocm|cpu|skip>` and "
        "`mokuro-bunko install-ocr --list-backends`.",
        "driver_hint": "CUDA/ROCm drivers/toolkits must be installed on the host for GPU backends.",
    }


class AdminAPI:
    """WSGI middleware for admin panel and API."""

    def __init__(
        self,
        app: Callable[..., Any],
        database: Database,
        config: AdminConfig,
        full_config: Config | None = None,
        config_path: Path | None = None,
        tunnel_service: Any | None = None,
        dyndns_service: Any | None = None,
        ocr_runtime: dict[str, Any] | None = None,
    ) -> None:
        self.app = app
        self.db = database
        self.config = config
        self.full_config = full_config
        self.config_path = config_path
        self.tunnel_service = tunnel_service
        self.dyndns_service = dyndns_service
        self._ocr_runtime_cache = ocr_runtime or {"available": False}
        self.invites = InviteManager(database)
        self.admin_path = config.path.rstrip("/") or "/"
        self._config_lock = threading.Lock()
        self._start_time = time.time()
        self._write_rate_limiter = ADMIN_WRITE_RATE_LIMITER

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        if not self.config.enabled:
            return self.app(environ, start_response)

        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        normalized_path = "/" + path.strip("/")
        if normalized_path == "":
            normalized_path = "/"

        # Check if this is an admin path
        if not (
            normalized_path == self.admin_path
            or normalized_path.startswith(f"{self.admin_path}/")
        ):
            return self.app(environ, start_response)

        # Strip admin prefix from path
        sub_path = normalized_path[len(self.admin_path):]
        if not sub_path:
            sub_path = "/"

        # Serve static files without auth check (JS handles redirect)
        if not sub_path.startswith("/api/"):
            return self._handle_static(environ, start_response, sub_path)

        # Check admin authorization for API endpoints only
        role = environ.get("mokuro.role", "anonymous")
        if not self._can_access_api(role, sub_path):
            error = "Admin access required"
            if sub_path == "/api/invites" or sub_path.startswith("/api/invites/"):
                error = "Admin or inviter access required"
            return self._json_response(
                start_response,
                403,
                {"error": error},
            )

        if method in ("POST", "PUT", "DELETE"):
            client_ip = get_client_ip(environ)
            allowed, retry_after = self._write_rate_limiter.allow_attempt(f"admin:{client_ip}")
            if not allowed:
                return self._json_response(
                    start_response,
                    429,
                    {
                        "error": f"Too many admin write requests. Retry in {retry_after}s",
                        "rate_limit": {
                            "limit": self._write_rate_limiter.max_failures,
                            "window_seconds": self._write_rate_limiter.window_seconds,
                            "block_seconds": self._write_rate_limiter.block_seconds,
                            "retry_after_seconds": retry_after,
                        },
                    },
                    extra_headers=[
                        ("Retry-After", str(retry_after)),
                        ("X-RateLimit-Limit", str(self._write_rate_limiter.max_failures)),
                        ("X-RateLimit-Window", str(self._write_rate_limiter.window_seconds)),
                        ("X-RateLimit-Block", str(self._write_rate_limiter.block_seconds)),
                    ],
                )
            self._write_rate_limiter.record_attempt(f"admin:{client_ip}")

        # Route to appropriate API handler
        return self._handle_api(environ, start_response, sub_path, method)

    @staticmethod
    def _can_access_api(role: str, path: str) -> bool:
        """Check role access for a given admin API path."""
        if path == "/api/invites" or path.startswith("/api/invites/"):
            return role in ("admin", "inviter")
        return role == "admin"

    @staticmethod
    def _actor_username(environ: dict[str, Any]) -> str | None:
        user = environ.get("mokuro.user")
        if isinstance(user, dict):
            username = user.get("username")
            if isinstance(username, str):
                return username
        username = environ.get("mokuro.username")
        if isinstance(username, str):
            return username
        return None

    def _handle_api(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        path: str,
        method: str,
    ) -> Iterable[bytes]:
        """Handle API requests."""
        if method in ("POST", "PUT", "DELETE"):
            if not self._is_same_origin_request(environ):
                return self._json_response(
                    start_response,
                    403,
                    {"error": "Cross-origin state-changing request blocked"},
                )

        # Users endpoints
        if path == "/api/users" and method == "GET":
            return self._list_users(environ, start_response)
        elif path == "/api/users" and method == "POST":
            return self._create_user(environ, start_response)
        elif path.startswith("/api/users/") and method == "DELETE":
            username = path[len("/api/users/"):]
            return self._delete_user(environ, start_response, username)
        elif path.endswith("/notes") and method == "PUT":
            username = path[len("/api/users/"):-len("/notes")]
            return self._update_user_notes(environ, start_response, username)
        elif path.endswith("/role") and method == "PUT":
            username = path[len("/api/users/"):-len("/role")]
            return self._change_role(environ, start_response, username)
        elif path.endswith("/approve") and method == "POST":
            username = path[len("/api/users/"):-len("/approve")]
            return self._approve_user(environ, start_response, username)
        elif path.endswith("/disable") and method == "POST":
            username = path[len("/api/users/"):-len("/disable")]
            return self._disable_user(environ, start_response, username)

        # Invites endpoints
        elif path == "/api/invites" and method == "GET":
            return self._list_invites(environ, start_response)
        elif path == "/api/invites" and method == "POST":
            return self._create_invite(environ, start_response)
        elif path.startswith("/api/invites/") and method == "DELETE":
            code = path[len("/api/invites/"):]
            return self._delete_invite(environ, start_response, code)
        elif path == "/api/audit" and method == "GET":
            return self._list_audit(environ, start_response)

        # Settings endpoints
        elif path == "/api/settings" and method == "GET":
            return self._get_settings(environ, start_response)
        elif path == "/api/settings/registration" and method == "PUT":
            return self._update_registration(environ, start_response)
        elif path == "/api/settings/cors" and method == "PUT":
            return self._update_cors(environ, start_response)
        elif path == "/api/settings/catalog" and method == "PUT":
            return self._update_catalog(environ, start_response)
        elif path == "/api/settings/queue" and method == "PUT":
            return self._update_queue(environ, start_response)
        elif path == "/api/settings/ocr" and method == "PUT":
            return self._update_ocr(environ, start_response)
        elif path == "/api/settings/dyndns" and method == "PUT":
            return self._update_dyndns_settings(environ, start_response)

        # Status endpoint
        elif path == "/api/status" and method == "GET":
            return self._get_status(environ, start_response)

        # Tunnel endpoints
        elif path == "/api/tunnel/status" and method == "GET":
            return self._get_tunnel_status(environ, start_response)
        elif path == "/api/tunnel/start" and method == "POST":
            return self._start_tunnel(environ, start_response)
        elif path == "/api/tunnel/stop" and method == "POST":
            return self._stop_tunnel(environ, start_response)

        # DynDNS endpoints
        elif path == "/api/dyndns/status" and method == "GET":
            return self._get_dyndns_status(environ, start_response)
        elif path == "/api/dyndns/start" and method == "POST":
            return self._start_dyndns(environ, start_response)
        elif path == "/api/dyndns/stop" and method == "POST":
            return self._stop_dyndns(environ, start_response)
        elif path == "/api/dyndns/test" and method == "POST":
            return self._test_dyndns(environ, start_response)

        # Not found
        return self._json_response(
            start_response,
            404,
            {"error": "API endpoint not found"},
        )

    def _handle_static(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        path: str,
    ) -> Iterable[bytes]:
        """Serve static files."""
        # Default to index.html
        if path == "/" or path == "":
            path = "/index.html"

        # Security: prevent directory traversal
        file_path = (STATIC_DIR / path.lstrip("/")).resolve()
        if not is_within_path(file_path, STATIC_DIR):
            return self._error_response(start_response, 403, "Forbidden")

        if not file_path.exists() or not file_path.is_file():
            # Return index.html for SPA routing
            file_path = STATIC_DIR / "index.html"
            if not file_path.exists():
                return self._error_response(start_response, 404, "Not found")

        # Determine content type
        ext = file_path.suffix.lower()
        content_type = MIME_TYPES.get(ext, "application/octet-stream")

        # Read and serve file
        try:
            content = file_path.read_bytes()
            headers = [
                ("Content-Type", content_type),
                ("Content-Length", str(len(content))),
                ("Cache-Control", "no-cache"),
                ("X-Content-Type-Options", "nosniff"),
                ("Referrer-Policy", "no-referrer"),
                ("X-Frame-Options", "DENY"),
                ("X-XSS-Protection", "1; mode=block"),
            ]
            start_response("200 OK", headers)
            return [content]
        except OSError:
            return self._error_response(start_response, 500, "Error reading file")

    # User API handlers

    def _list_users(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """List all users."""
        users = self.db.list_users()
        return self._json_response(start_response, 200, {"users": users})

    def _create_user(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Create a new user."""
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        username = data.get("username", "").strip()
        password = data.get("password", "")
        role = data.get("role", "registered")
        notes = data.get("notes", "")

        if not username:
            return self._json_response(
                start_response, 400, {"error": "Username is required"}
            )
        username_error = validate_username(username)
        if username_error:
            return self._json_response(start_response, 400, {"error": username_error})
        if not password:
            return self._json_response(
                start_response, 400, {"error": "Password is required"}
            )
        password_error = validate_password(password)
        if password_error:
            return self._json_response(start_response, 400, {"error": password_error})

        try:
            self.db.create_user(username, password, role, notes=notes)
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_create_user",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
                details={"role": role},
            )
            return self._json_response(
                start_response, 201, {"success": True, "user": user}
            )
        except ValueError as e:
            status = 409 if "already exists" in str(e).lower() else 400
            return self._json_response(start_response, status, {"error": str(e)})

    def _delete_user(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Delete a user."""
        if self.db.delete_user(username):
            self.db.log_audit_event(
                action="admin_delete_user",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
            )
            return self._json_response(
                start_response, 200, {"success": True, "message": f"User '{username}' deleted"}
            )
        return self._json_response(
            start_response, 404, {"error": f"User '{username}' not found"}
        )

    def _change_role(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Change a user's role."""
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        role = data.get("role")
        if not role:
            return self._json_response(
                start_response, 400, {"error": "Role is required"}
            )

        valid_roles = ["registered", "uploader", "inviter", "editor", "admin"]
        if role not in valid_roles:
            return self._json_response(
                start_response, 400, {"error": f"Invalid role. Must be one of: {valid_roles}"}
            )

        if self.db.update_user_role(username, role):
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_change_role",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
                details={"role": role},
            )
            return self._json_response(
                start_response, 200, {"success": True, "user": user}
            )
        return self._json_response(
            start_response, 404, {"error": f"User '{username}' not found"}
        )

    def _approve_user(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Approve a pending user."""
        if self.db.approve_user(username):
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_approve_user",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
            )
            return self._json_response(
                start_response, 200, {"success": True, "user": user}
            )
        return self._json_response(
            start_response, 404, {"error": f"User '{username}' not found or not pending"}
        )

    def _disable_user(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Disable a user."""
        if self.db.disable_user(username):
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_disable_user",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
            )
            return self._json_response(
                start_response, 200, {"success": True, "user": user}
            )
        return self._json_response(
            start_response, 404, {"error": f"User '{username}' not found"}
        )

    def _update_user_notes(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> list[bytes]:
        """Update a user's admin notes."""
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        notes = data.get("notes", "")
        if not isinstance(notes, str):
            return self._json_response(start_response, 400, {"error": "notes must be a string"})

        if self.db.update_user_notes(username, notes):
            user = self.db.get_user(username)
            self.db.log_audit_event(
                action="admin_update_notes",
                actor_username=self._actor_username(environ),
                target_type="user",
                target_username=username,
            )
            return self._json_response(start_response, 200, {"success": True, "user": user})
        return self._json_response(start_response, 404, {"error": f"User '{username}' not found"})

    # Invite API handlers

    def _list_invites(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """List all invites."""
        invites = self.invites.list_all()
        return self._json_response(start_response, 200, {"invites": invites})

    def _create_invite(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Create a new invite."""
        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        role = data.get("role", "registered")
        expires = data.get("expires", "7d")

        valid_roles = ["registered", "uploader", "inviter", "editor"]
        if role not in valid_roles:
            return self._json_response(
                start_response, 400, {"error": f"Invalid role. Must be one of: {valid_roles}"}
            )

        try:
            code = self.invites.create_invite(
                role=role,
                expires=expires,
                invited_by=self._actor_username(environ),
            )
            info = self.invites.get_info(code)
            return self._json_response(
                start_response, 201, {"success": True, "invite": info}
            )
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

    def _delete_invite(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        code: str,
    ) -> list[bytes]:
        """Delete an invite."""
        if self.invites.delete(code):
            self.db.log_audit_event(
                action="invite_deleted",
                actor_username=self._actor_username(environ),
                target_type="invite",
                target_path=code,
            )
            return self._json_response(
                start_response, 200, {"success": True, "message": "Invite deleted"}
            )
        return self._json_response(
            start_response, 404, {"error": "Invite not found"}
        )

    def _list_audit(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """List audit events."""
        events = self.db.list_audit_events(limit=200)
        return self._json_response(start_response, 200, {"events": events})

    # Settings API handlers

    def _get_settings(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return full config with masked token."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        data = self.full_config.to_dict()
        # Mask the DynDNS token
        if data.get("dyndns", {}).get("token"):
            data["dyndns"]["token"] = "****"
        data["ocr_runtime"] = self._ocr_runtime_cache
        return self._json_response(start_response, 200, data)

    def _update_registration(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update registration settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "mode" in data:
                valid_modes = ["disabled", "self", "invite", "approval"]
                if data["mode"] not in valid_modes:
                    return self._json_response(
                        start_response, 400, {"error": f"Invalid mode. Must be one of: {valid_modes}"}
                    )
                self.full_config.registration.mode = data["mode"]
            if "default_role" in data:
                valid_roles = ["registered", "uploader", "inviter", "editor"]
                if data["default_role"] not in valid_roles:
                    return self._json_response(
                        start_response, 400, {"error": f"Invalid default_role. Must be one of: {valid_roles}"}
                    )
                self.full_config.registration.default_role = data["default_role"]
            if "allow_anonymous_browse" in data:
                self.full_config.registration.allow_anonymous_browse = bool(
                    data["allow_anonymous_browse"]
                )
            if "allow_anonymous_download" in data:
                self.full_config.registration.allow_anonymous_download = bool(
                    data["allow_anonymous_download"]
                )
            # Backward compatibility for older admin clients
            if "require_login" in data:
                require_login = bool(data["require_login"])
                self.full_config.registration.allow_anonymous_browse = not require_login
                self.full_config.registration.allow_anonymous_download = not require_login
            self._save_config()

        return self._json_response(start_response, 200, {
            "success": True,
            "registration": {
                "mode": self.full_config.registration.mode,
                "default_role": self.full_config.registration.default_role,
                "allow_anonymous_browse": self.full_config.registration.allow_anonymous_browse,
                "allow_anonymous_download": self.full_config.registration.allow_anonymous_download,
                # Legacy compatibility key
                "require_login": (
                    (not self.full_config.registration.allow_anonymous_browse)
                    and (not self.full_config.registration.allow_anonymous_download)
                ),
            },
        })

    def _update_cors(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update CORS settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "enabled" in data:
                self.full_config.cors.enabled = bool(data["enabled"])
            if "allowed_origins" in data:
                if not isinstance(data["allowed_origins"], list):
                    return self._json_response(
                        start_response, 400, {"error": "allowed_origins must be a list"}
                    )
                self.full_config.cors.allowed_origins = data["allowed_origins"]
            self._save_config()

        return self._json_response(start_response, 200, {
            "success": True,
            "cors": {
                "enabled": self.full_config.cors.enabled,
                "allowed_origins": self.full_config.cors.allowed_origins,
            },
        })

    def _update_catalog(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update catalog settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "enabled" in data:
                self.full_config.catalog.enabled = bool(data["enabled"])
            if "reader_url" in data:
                url = data["reader_url"].strip().rstrip("/")
                if url:
                    self.full_config.catalog.reader_url = url
            if "use_as_homepage" in data:
                self.full_config.catalog.use_as_homepage = bool(data["use_as_homepage"])
            self._save_config()

        return self._json_response(start_response, 200, {
            "success": True,
            "catalog": {
                "enabled": self.full_config.catalog.enabled,
                "reader_url": self.full_config.catalog.reader_url,
                "use_as_homepage": self.full_config.catalog.use_as_homepage,
            },
        })

    def _update_queue(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update queue settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "show_in_nav" in data:
                self.full_config.queue.show_in_nav = bool(data["show_in_nav"])
            if "public_access" in data:
                self.full_config.queue.public_access = bool(data["public_access"])
            self._save_config()

        return self._json_response(start_response, 200, {
            "success": True,
            "queue": {
                "show_in_nav": self.full_config.queue.show_in_nav,
                "public_access": self.full_config.queue.public_access,
            },
        })

    def _update_ocr(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update OCR settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            if "backend" in data:
                return self._json_response(
                    start_response,
                    400,
                    {"error": "OCR backend is launch-only. Use CLI flags/config file to change it."},
                )
            if "poll_interval" in data:
                try:
                    interval = int(data["poll_interval"])
                    if interval < 1:
                        raise ValueError
                    self.full_config.ocr.poll_interval = interval
                except (ValueError, TypeError):
                    return self._json_response(
                        start_response, 400, {"error": "poll_interval must be a positive integer"}
                    )
            self._save_config()

        return self._json_response(start_response, 200, {
            "success": True,
            "ocr": {
                "backend": self.full_config.ocr.backend,
                "poll_interval": self.full_config.ocr.poll_interval,
            },
            "ocr_runtime": self._refresh_ocr_runtime_cache(),
        })

    def _refresh_ocr_runtime_cache(self) -> dict[str, Any]:
        """Recompute and cache OCR runtime status."""
        self._ocr_runtime_cache = build_ocr_runtime_status(self.full_config)
        return self._ocr_runtime_cache

    def _update_dyndns_settings(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Update DynDNS settings."""
        if not self.full_config:
            return self._json_response(start_response, 500, {"error": "Config not available"})

        try:
            data = self._parse_json_body(environ)
        except ValueError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

        with self._config_lock:
            dyndns = self.full_config.dyndns
            if "enabled" in data:
                dyndns.enabled = bool(data["enabled"])
            if "provider" in data:
                if data["provider"] not in ("duckdns", "generic"):
                    return self._json_response(
                        start_response, 400, {"error": "Invalid provider"}
                    )
                dyndns.provider = data["provider"]
            # Only update token if explicitly sent and not the masked value
            if "token" in data and data["token"] != "****":
                dyndns.token = data["token"]
            if "domain" in data:
                dyndns.domain = data["domain"]
            if "update_url" in data:
                dyndns.update_url = data["update_url"]
            if "interval" in data:
                try:
                    interval = int(data["interval"])
                    if interval < 30:
                        raise ValueError
                    dyndns.interval = interval
                except (ValueError, TypeError):
                    return self._json_response(
                        start_response, 400, {"error": "interval must be at least 30"}
                    )
            self._save_config()

            # Reconfigure the running service if available
            if self.dyndns_service:
                self.dyndns_service.configure(dyndns)

        result = {
            "enabled": dyndns.enabled,
            "provider": dyndns.provider,
            "domain": dyndns.domain,
            "update_url": dyndns.update_url,
            "interval": dyndns.interval,
            "token": "****" if dyndns.token else "",
        }
        return self._json_response(start_response, 200, {"success": True, "dyndns": result})

    # Status API handler

    def _get_status(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return server status info."""
        uptime = time.time() - self._start_time
        users = self.db.list_users()
        user_count = sum(1 for u in users if u["status"] != "deleted")

        # Disk usage
        storage_path = ""
        disk_total = 0
        disk_used = 0
        disk_free = 0
        volume_count = 0
        if self.full_config:
            storage_path = str(self.full_config.storage.base_path)
            try:
                usage = shutil.disk_usage(storage_path)
                disk_total = usage.total
                disk_used = usage.used
                disk_free = usage.free
            except OSError:
                pass
            # Count volumes in library
            lib_path = self.full_config.storage.library_path
            if lib_path.exists():
                volume_count = sum(1 for p in lib_path.iterdir() if p.is_dir())

        host = ""
        port = 0
        if self.full_config:
            host = self.full_config.server.host
            port = self.full_config.server.port

        return self._json_response(start_response, 200, {
            "uptime": uptime,
            "host": host,
            "port": port,
            "storage_path": storage_path,
            "disk_total": disk_total,
            "disk_used": disk_used,
            "disk_free": disk_free,
            "user_count": user_count,
            "volume_count": volume_count,
            "stats": {},
        })

    # Tunnel API handlers

    def _get_tunnel_status(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return tunnel status."""
        if not self.tunnel_service:
            return self._json_response(start_response, 200, {
                "running": False, "url": None, "available": False,
            })
        return self._json_response(start_response, 200, self.tunnel_service.status)

    def _start_tunnel(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Start the cloudflare tunnel."""
        if not self.tunnel_service:
            return self._json_response(start_response, 500, {"error": "Tunnel service not available"})
        try:
            self.tunnel_service.start()
            return self._json_response(start_response, 200, {"success": True, **self.tunnel_service.status})
        except RuntimeError as e:
            return self._json_response(start_response, 400, {"error": str(e)})

    def _stop_tunnel(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Stop the cloudflare tunnel."""
        if not self.tunnel_service:
            return self._json_response(start_response, 500, {"error": "Tunnel service not available"})
        self.tunnel_service.stop()
        return self._json_response(start_response, 200, {"success": True, **self.tunnel_service.status})

    # DynDNS API handlers

    def _get_dyndns_status(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return DynDNS service status."""
        if not self.dyndns_service:
            return self._json_response(start_response, 200, {
                "enabled": False, "running": False,
            })
        return self._json_response(start_response, 200, self.dyndns_service.status())

    def _start_dyndns(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Start the DynDNS service."""
        if not self.dyndns_service:
            return self._json_response(start_response, 500, {"error": "DynDNS service not available"})
        self.dyndns_service.start()
        return self._json_response(start_response, 200, {"success": True, **self.dyndns_service.status()})

    def _stop_dyndns(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Stop the DynDNS service."""
        if not self.dyndns_service:
            return self._json_response(start_response, 500, {"error": "DynDNS service not available"})
        self.dyndns_service.stop()
        return self._json_response(start_response, 200, {"success": True, **self.dyndns_service.status()})

    def _test_dyndns(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Force an immediate DynDNS update."""
        if not self.dyndns_service:
            return self._json_response(start_response, 500, {"error": "DynDNS service not available"})
        result = self.dyndns_service.update_now()
        return self._json_response(start_response, 200, result)

    # Helper methods

    def _save_config(self) -> None:
        """Save current config to disk."""
        if self.full_config and self.config_path:
            save_config(self.full_config, self.config_path)

    def _parse_json_body(self, environ: dict[str, Any]) -> dict[str, Any]:
        """Parse JSON request body."""
        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
            if content_length < 0:
                raise ValueError("Invalid Content-Length")
            if content_length == 0:
                return {}
            if content_length > MAX_JSON_BODY_BYTES:
                raise ValueError("Request body too large")
            body = environ["wsgi.input"].read(content_length)
            return json.loads(body.decode("utf-8"))
        except json.JSONDecodeError as exc:
            raise ValueError("Invalid JSON body") from exc
        except ValueError as exc:
            raise ValueError(str(exc)) from exc

    def _is_same_origin_request(self, environ: dict[str, Any]) -> bool:
        """Allow mutating requests only from same origin when origin hints are present."""
        expected_host = str(environ.get("HTTP_X_FORWARDED_HOST", "") or "").split(",")[0].strip()
        if not expected_host:
            expected_host = str(environ.get("HTTP_HOST", "") or "").strip()
        if not expected_host:
            server_name = str(environ.get("SERVER_NAME", "") or "").strip()
            server_port = str(environ.get("SERVER_PORT", "") or "").strip()
            expected_host = f"{server_name}:{server_port}" if server_port else server_name

        if not expected_host:
            return True

        origin = str(environ.get("HTTP_ORIGIN", "") or "").strip()
        referer = str(environ.get("HTTP_REFERER", "") or "").strip()

        if origin:
            parsed = urlparse(origin)
            if parsed.netloc and not self._hosts_equivalent(parsed.netloc, expected_host):
                return False

        if referer:
            parsed = urlparse(referer)
            if parsed.netloc and not self._hosts_equivalent(parsed.netloc, expected_host):
                return False

        return True

    @staticmethod
    def _hosts_equivalent(left: str, right: str) -> bool:
        """Return True when two host headers represent the same origin host."""
        def split_host_port(value: str) -> tuple[str, int | None]:
            v = value.strip().lower()
            if not v:
                return "", None
            if ":" not in v:
                return v, None
            host, port_text = v.rsplit(":", 1)
            try:
                return host, int(port_text)
            except ValueError:
                return v, None

        l_host, l_port = split_host_port(left)
        r_host, r_port = split_host_port(right)
        if l_host != r_host:
            return False
        if l_port == r_port:
            return True
        # Treat default ports as equivalent when one side omits the explicit port.
        if l_port is None and r_port in (80, 443):
            return True
        if r_port is None and l_port in (80, 443):
            return True
        return False

    def _json_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        data: dict[str, Any],
        extra_headers: list[tuple[str, str]] | None = None,
    ) -> list[bytes]:
        """Return a JSON response."""
        status_messages = {
            200: "OK",
            201: "Created",
            400: "Bad Request",
            403: "Forbidden",
            404: "Not Found",
            409: "Conflict",
            413: "Payload Too Large",
            429: "Too Many Requests",
            500: "Internal Server Error",
        }
        status = f"{status_code} {status_messages.get(status_code, 'Unknown')}"

        body = json.dumps(data).encode("utf-8")
        headers = [
            ("Content-Type", "application/json; charset=utf-8"),
            ("Content-Length", str(len(body))),
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            ("X-Frame-Options", "DENY"),
            ("X-XSS-Protection", "1; mode=block"),
        ]
        if extra_headers:
            headers.extend(extra_headers)

        start_response(status, headers)
        return [body]

    def _error_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        message: str,
    ) -> list[bytes]:
        """Return an error response."""
        status_messages = {
            403: "Forbidden",
            404: "Not Found",
            500: "Internal Server Error",
        }
        status = f"{status_code} {status_messages.get(status_code, 'Error')}"

        body = message.encode("utf-8")
        headers = [
            ("Content-Type", "text/plain"),
            ("Content-Length", str(len(body))),
        ]

        start_response(status, headers)
        return [body]
