"""Setup wizard WSGI middleware for first-run onboarding."""

from __future__ import annotations

import json
import mimetypes
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

from mokuro_bunko.config import Config, save_config
from mokuro_bunko.database import Database
from mokuro_bunko.security import get_client_ip, is_loopback_ip, is_within_path
from mokuro_bunko.validation import validate_password, validate_username


# Path to web static files
WEB_DIR = Path(__file__).parent / "web"

# MIME types for static files
MIME_TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "application/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".json": "application/json",
    ".png": "image/png",
    ".ico": "image/x-icon",
    ".svg": "image/svg+xml",
}

MAX_JSON_BODY_BYTES = 64 * 1024


class SetupWizardAPI:
    """WSGI middleware for the first-run setup wizard."""

    def __init__(
        self,
        app: Callable[..., Any],
        database: Database,
        config: Config,
        config_path: Optional[Path] = None,
    ) -> None:
        self.app = app
        self.db = database
        self.config = config
        self.config_path = config_path
        self._setup_complete: Optional[bool] = None

    def _needs_setup(self) -> bool:
        """Check if initial setup is needed (no admin users exist)."""
        # Cache after first False to avoid DB query on every request
        if self._setup_complete is True:
            return False
        users = self.db.list_users()
        has_admin = any(u.get("role") == "admin" for u in users)
        if has_admin:
            self._setup_complete = True
            return False
        return True

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        # Setup API endpoints (restricted to local requests)
        if path == "/setup/api/status" and method == "GET":
            if self._needs_setup() and not self._is_local_request(environ):
                return self._json_response(start_response, 403, {
                    "error": "Setup is only allowed from localhost",
                })
            return self._json_response(start_response, 200, {
                "needs_setup": self._needs_setup(),
            })

        if path == "/setup/api/complete" and method == "POST":
            if not self._is_local_request(environ):
                return self._json_response(start_response, 403, {
                    "error": "Setup is only allowed from localhost",
                })
            if not self._needs_setup():
                return self._json_response(start_response, 400, {
                    "error": "Setup already completed",
                })
            return self._handle_complete(environ, start_response)

        # Serve setup static files (restricted during first-run setup)
        if path in ("/setup", "/setup/"):
            if method == "GET":
                if self._needs_setup() and not self._is_local_request(environ):
                    return self._json_response(start_response, 403, {
                        "error": "Setup is only allowed from localhost",
                    })
                return self._serve_static_file(start_response, "index.html")

        if path.startswith("/setup/") and method == "GET":
            filename = path[len("/setup/"):]
            if filename and not filename.startswith("api/"):
                if self._needs_setup() and not self._is_local_request(environ):
                    return self._json_response(start_response, 403, {
                        "error": "Setup is only allowed from localhost",
                    })
                return self._serve_static_file(start_response, filename)

        # Redirect browser requests to / -> /setup when setup is needed
        if path == "/" and method == "GET" and self._needs_setup():
            accept = environ.get("HTTP_ACCEPT", "")
            if "text/html" in accept:
                start_response("302 Found", [("Location", "/setup")])
                return [b""]

        # Pass through to wrapped app
        return self.app(environ, start_response)

    def _handle_complete(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Handle POST /setup/api/complete."""
        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
            if content_length == 0:
                return self._json_response(start_response, 400, {"error": "Empty body"})
            if content_length > MAX_JSON_BODY_BYTES:
                return self._json_response(start_response, 413, {"error": "Request body too large"})
            body = environ["wsgi.input"].read(content_length)
            data = json.loads(body.decode("utf-8"))
        except (json.JSONDecodeError, ValueError):
            return self._json_response(start_response, 400, {"error": "Invalid JSON"})

        # Create admin user
        admin_data = data.get("admin", {})
        username = admin_data.get("username", "").strip()
        password = admin_data.get("password", "")

        username_error = validate_username(username)
        if username_error:
            return self._json_response(start_response, 400, {"error": username_error})
        password_error = validate_password(password)
        if password_error:
            return self._json_response(start_response, 400, {"error": password_error})

        try:
            self.db.create_user(username, password, "admin")
        except ValueError as e:
            return self._json_response(start_response, 409, {"error": str(e)})

        # Update registration config
        reg_data = data.get("registration", {})
        if "mode" in reg_data:
            valid_modes = ["disabled", "self", "invite", "approval"]
            if reg_data["mode"] in valid_modes:
                self.config.registration.mode = reg_data["mode"]

        # Save config
        if self.config_path:
            save_config(self.config, self.config_path)

        # Mark setup as complete
        self._setup_complete = True

        return self._json_response(start_response, 201, {
            "success": True,
            "message": "Setup completed successfully",
        })

    @staticmethod
    def _is_local_request(environ: dict[str, Any]) -> bool:
        """Allow only loopback setup requests."""
        remote_addr = str(environ.get("REMOTE_ADDR", "") or "").strip()
        if not remote_addr:
            return False
        if not is_loopback_ip(remote_addr):
            return False
        forwarded_ip = get_client_ip(environ)
        if forwarded_ip and forwarded_ip != remote_addr:
            return is_loopback_ip(forwarded_ip)
        return True

    def _serve_static_file(
        self,
        start_response: Callable[..., Any],
        filename: str,
    ) -> list[bytes]:
        """Serve a static file from the web directory."""
        # Security: prevent directory traversal
        if ".." in filename or filename.startswith("/"):
            return self._json_response(start_response, 404, {"error": "Not found"})

        file_path = WEB_DIR / filename

        # Resolve to check for directory traversal
        try:
            resolved = file_path.resolve()
            if not is_within_path(resolved, WEB_DIR):
                return self._json_response(start_response, 403, {"error": "Forbidden"})
        except (OSError, ValueError):
            return self._json_response(start_response, 404, {"error": "Not found"})

        if not file_path.exists() or not file_path.is_file():
            return self._json_response(start_response, 404, {"error": "Not found"})

        # Determine content type
        ext = file_path.suffix.lower()
        content_type = MIME_TYPES.get(ext)
        if content_type is None:
            content_type, _ = mimetypes.guess_type(filename)
            if content_type is None:
                content_type = "application/octet-stream"

        try:
            content = file_path.read_bytes()
        except OSError:
            return self._json_response(start_response, 500, {"error": "Read error"})

        headers = [
            ("Content-Type", content_type),
            ("Content-Length", str(len(content))),
            ("Cache-Control", "no-cache"),
        ]

        start_response("200 OK", headers)
        return [content]

    def _json_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        data: dict[str, Any],
    ) -> list[bytes]:
        """Return a JSON response."""
        status_messages = {
            200: "OK",
            201: "Created",
            302: "Found",
            400: "Bad Request",
            403: "Forbidden",
            404: "Not Found",
            409: "Conflict",
            413: "Payload Too Large",
            500: "Internal Server Error",
        }
        status = f"{status_code} {status_messages.get(status_code, 'Unknown')}"

        body = json.dumps(data).encode("utf-8")
        headers = [
            ("Content-Type", "application/json"),
            ("Content-Length", str(len(body))),
        ]

        start_response(status, headers)
        return [body]
