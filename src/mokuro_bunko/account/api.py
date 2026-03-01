"""Account page API for mokuro-bunko."""

from __future__ import annotations

import json
import shutil
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Iterable, Optional, TYPE_CHECKING

from mokuro_bunko.middleware.auth import authenticate_basic_header
from mokuro_bunko.security import is_within_path
from mokuro_bunko.validation import validate_password

if TYPE_CHECKING:
    from mokuro_bunko.database import Database

# Static files directory
STATIC_DIR = Path(__file__).parent / "web"

MIME_TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "application/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
}
MAX_JSON_BODY_BYTES = 64 * 1024


@dataclass
class PersonalStats:
    """Personal reading statistics for a single user."""

    volumes: int = 0
    pages_read: int = 0
    characters_read: int = 0
    reading_time_seconds: int = 0

    def to_dict(self) -> dict[str, Any]:
        return {
            "volumes": self.volumes,
            "pages_read": self.pages_read,
            "characters_read": self.characters_read,
            "reading_time_seconds": self.reading_time_seconds,
            "reading_time_formatted": self._format_time(self.reading_time_seconds),
        }

    @staticmethod
    def _format_time(seconds: int) -> str:
        if seconds < 60:
            return f"{seconds}s"
        minutes = seconds // 60
        if minutes < 60:
            return f"{minutes}m"
        hours = minutes // 60
        remaining_minutes = minutes % 60
        if hours < 24:
            return f"{hours}h {remaining_minutes}m"
        days = hours // 24
        remaining_hours = hours % 24
        return f"{days}d {remaining_hours}h"


class AccountAPI:
    """WSGI middleware for account page and API."""

    def __init__(
        self,
        app: Callable[..., Any],
        database: Optional["Database"] = None,
        storage_path: Optional[Path] = None,
    ) -> None:
        self.app = app
        self.db = database
        self.storage_path = Path(storage_path) if storage_path else None

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        # API endpoints
        if path == "/api/account/stats" and method == "GET":
            return self._handle_stats(environ, start_response)
        if path == "/api/account/password" and method == "POST":
            return self._handle_change_password(environ, start_response)
        if path == "/api/account/delete" and method == "POST":
            return self._handle_delete_account(environ, start_response)

        # OPTIONS for CORS preflight
        if path.startswith("/api/account/") and method == "OPTIONS":
            return self._handle_options(start_response)

        if method != "GET":
            return self.app(environ, start_response)

        # Serve account page
        if path == "/account" or path == "/account/":
            return self._serve_static(start_response, "index.html")
        elif path.startswith("/account/"):
            filename = path[len("/account/"):]
            return self._serve_static(start_response, filename)

        return self.app(environ, start_response)

    def _authenticate_request(self, environ: dict[str, Any]) -> Optional[dict[str, Any]]:
        """Authenticate from Basic auth header and return user dict."""
        if not self.db:
            return None
        auth_result = authenticate_basic_header(self.db, environ.get("HTTP_AUTHORIZATION"))
        if not auth_result.authenticated or not auth_result.user:
            return None
        return auth_result.user

    def _handle_stats(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        user = self._authenticate_request(environ)
        if not user:
            return self._json_response(start_response, 401, {"error": "Authentication required"})
        return self._json_response(start_response, 200, PersonalStats().to_dict())

    def _handle_change_password(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        user = self._authenticate_request(environ)
        if not user or not self.db:
            return self._json_response(start_response, 401, {"error": "Authentication required"})
        username = user["username"]

        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
            if content_length == 0:
                return self._json_response(start_response, 400, {"error": "Missing request body"})
            if content_length > MAX_JSON_BODY_BYTES:
                return self._json_response(start_response, 413, {"error": "Request body too large"})
            body = environ["wsgi.input"].read(content_length)
            data = json.loads(body.decode("utf-8"))
        except (json.JSONDecodeError, ValueError):
            return self._json_response(start_response, 400, {"error": "Invalid request"})

        current_password = data.get("current_password", "")
        new_password = data.get("new_password", "")

        if not current_password or not new_password:
            return self._json_response(start_response, 400, {"error": "Missing required fields"})

        # Verify current password matches
        if not self.db.authenticate_user(username, current_password):
            return self._json_response(start_response, 401, {"error": "Current password is incorrect"})

        password_error = validate_password(new_password)
        if password_error:
            return self._json_response(start_response, 400, {"error": password_error})

        self.db.update_user_password(username, new_password)
        return self._json_response(start_response, 200, {"success": True})

    def _handle_delete_account(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        user = self._authenticate_request(environ)
        if not user or not self.db:
            return self._json_response(start_response, 401, {"error": "Authentication required"})
        username = user["username"]

        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
            if content_length == 0:
                return self._json_response(start_response, 400, {"error": "Missing request body"})
            if content_length > MAX_JSON_BODY_BYTES:
                return self._json_response(start_response, 413, {"error": "Request body too large"})
            body = environ["wsgi.input"].read(content_length)
            data = json.loads(body.decode("utf-8"))
        except (json.JSONDecodeError, ValueError):
            return self._json_response(start_response, 400, {"error": "Invalid request"})

        confirm_password = data.get("password", "")
        if not confirm_password:
            return self._json_response(start_response, 400, {"error": "Password confirmation required"})

        if not self.db.authenticate_user(username, confirm_password):
            return self._json_response(start_response, 401, {"error": "Password is incorrect"})

        # Delete user from database
        self.db.delete_user(username)

        # Remove user data directory
        if self.storage_path:
            users_root = (self.storage_path / "users").resolve()
            user_dir = (users_root / username).resolve()
            if not user_dir.is_relative_to(users_root):
                return self._json_response(start_response, 400, {"error": "Invalid username path"})
            if user_dir.exists():
                shutil.rmtree(user_dir, ignore_errors=True)

        return self._json_response(start_response, 200, {"success": True})

    def _handle_options(self, start_response: Callable[..., Any]) -> list[bytes]:
        start_response("204 No Content", [
            ("Allow", "GET, POST, OPTIONS"),
        ])
        return [b""]

    def _json_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        data: dict[str, Any],
    ) -> list[bytes]:
        status_map = {
            200: "OK",
            400: "Bad Request",
            401: "Unauthorized",
            413: "Payload Too Large",
            403: "Forbidden",
            404: "Not Found",
            500: "Internal Server Error",
        }
        status = f"{status_code} {status_map.get(status_code, 'Error')}"
        body = json.dumps(data).encode("utf-8")
        headers = [
            ("Content-Type", "application/json"),
            ("Content-Length", str(len(body))),
        ]
        start_response(status, headers)
        return [body]

    def _serve_static(self, start_response: Callable[..., Any], filename: str) -> list[bytes]:
        if not filename or filename == "/":
            filename = "index.html"

        file_path = (STATIC_DIR / filename).resolve()
        if not is_within_path(file_path, STATIC_DIR):
            return self._error_response(start_response, 403, "Forbidden")

        if not file_path.exists() or not file_path.is_file():
            return self._error_response(start_response, 404, "Not found")

        ext = file_path.suffix.lower()
        content_type = MIME_TYPES.get(ext, "application/octet-stream")

        try:
            content = file_path.read_bytes()
            headers = [
                ("Content-Type", content_type),
                ("Content-Length", str(len(content))),
                ("Cache-Control", "no-cache"),
            ]
            start_response("200 OK", headers)
            return [content]
        except IOError:
            return self._error_response(start_response, 500, "Error")

    def _error_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        message: str,
    ) -> list[bytes]:
        status_map = {403: "Forbidden", 404: "Not Found", 500: "Internal Server Error"}
        status = f"{status_code} {status_map.get(status_code, 'Error')}"
        body = message.encode("utf-8")
        headers = [
            ("Content-Type", "text/plain"),
            ("Content-Length", str(len(body))),
        ]
        start_response(status, headers)
        return [body]
