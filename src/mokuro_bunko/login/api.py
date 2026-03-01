"""Login page API for mokuro-bunko."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Callable, Iterable, Optional, TYPE_CHECKING

from mokuro_bunko.middleware.auth import authenticate_basic_header
from mokuro_bunko.security import AuthAttemptLimiter, get_client_ip, is_within_path

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
AUTH_RATE_LIMITER = AuthAttemptLimiter()


class LoginAPI:
    """WSGI middleware for login page."""

    def __init__(
        self,
        app: Callable[..., Any],
        database: Optional["Database"] = None,
        nav_config: Optional[Any] = None,
    ) -> None:
        """Initialize login API middleware."""
        self.app = app
        self.db = database
        self._nav_config = nav_config

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        # Handle auth check endpoint
        if path == "/login/api/check" and method == "POST":
            return self._check_auth(environ, start_response)

        # Handle user info endpoint (reads Basic auth header)
        if path == "/login/api/me" and method == "GET":
            return self._get_me(environ, start_response)

        # Shared nav configuration endpoint for page headers
        if path == "/api/nav/config" and method == "GET":
            return self._get_nav_config(start_response)

        if method != "GET":
            return self.app(environ, start_response)

        # Handle login routes
        if path == "/login" or path == "/login/":
            return self._serve_static(start_response, "index.html")
        elif path.startswith("/login/"):
            filename = path[len("/login/"):]
            return self._serve_static(start_response, filename)

        return self.app(environ, start_response)

    def _check_auth(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Check authentication credentials."""
        if not self.db:
            return self._json_response(start_response, 500, {"error": "Database not configured"})

        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
            if content_length == 0:
                return self._json_response(start_response, 400, {"error": "Missing credentials"})
            if content_length > MAX_JSON_BODY_BYTES:
                return self._json_response(start_response, 413, {"error": "Request body too large"})

            body = environ["wsgi.input"].read(content_length)
            data = json.loads(body.decode("utf-8"))

            username = data.get("username", "")
            password = data.get("password", "")

            if not username or not password:
                return self._json_response(start_response, 400, {"error": "Missing credentials"})

            key = f"{get_client_ip(environ)}:{username}"
            allowed, retry_after = AUTH_RATE_LIMITER.allow_attempt(key)
            if not allowed:
                return self._json_response(
                    start_response, 429, {"error": f"Too many failed attempts. Retry in {retry_after}s"}
                )

            user = self.db.authenticate_user(username, password)
            if user:
                AUTH_RATE_LIMITER.record_success(key)
                return self._json_response(start_response, 200, {
                    "success": True,
                    "user": {"username": user["username"], "role": user["role"]}
                })
            else:
                AUTH_RATE_LIMITER.record_failure(key)
                return self._json_response(start_response, 401, {"error": "Invalid credentials"})

        except (json.JSONDecodeError, ValueError):
            return self._json_response(start_response, 400, {"error": "Invalid request"})

    def _get_me(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Return user info from Basic auth header."""
        if not self.db:
            return self._json_response(start_response, 500, {"error": "Database not configured"})

        auth_header = environ.get("HTTP_AUTHORIZATION", "")
        if not auth_header.startswith("Basic "):
            return self._json_response(start_response, 401, {"error": "Authentication required"})

        auth_result = authenticate_basic_header(self.db, auth_header)
        if not auth_result.authenticated or not auth_result.user:
            return self._json_response(start_response, 401, {"error": "Invalid credentials"})
        user = auth_result.user

        return self._json_response(start_response, 200, {
            "username": user["username"],
            "role": user["role"],
            "created_at": user["created_at"],
        })

    def _get_nav_config(self, start_response: Callable[..., Any]) -> list[bytes]:
        """Return header/nav feature flags for frontend pages."""
        home_enabled = True
        catalog_enabled = True
        queue_show_in_nav = False
        queue_public_access = True
        registration_enabled = True

        if self._nav_config is not None:
            catalog_enabled = bool(getattr(self._nav_config.catalog, "enabled", False))
            use_as_homepage = bool(getattr(self._nav_config.catalog, "use_as_homepage", False))
            home_enabled = not (catalog_enabled and use_as_homepage)
            queue_show_in_nav = bool(getattr(self._nav_config.queue, "show_in_nav", False))
            queue_public_access = bool(getattr(self._nav_config.queue, "public_access", True))
            registration_enabled = getattr(self._nav_config.registration, "mode", "self") != "disabled"

        return self._json_response(start_response, 200, {
            "home_enabled": home_enabled,
            "catalog_enabled": catalog_enabled,
            "queue_show_in_nav": queue_show_in_nav,
            "queue_public_access": queue_public_access,
            "registration_enabled": registration_enabled,
        })

    def _json_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        data: dict[str, Any],
    ) -> list[bytes]:
        """Return a JSON response."""
        status_map = {
            200: "OK",
            400: "Bad Request",
            401: "Unauthorized",
            429: "Too Many Requests",
            413: "Payload Too Large",
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
        """Serve static files."""
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
        """Return an error response."""
        status_map = {403: "Forbidden", 404: "Not Found", 500: "Internal Server Error"}
        status = f"{status_code} {status_map.get(status_code, 'Error')}"
        body = message.encode("utf-8")
        headers = [
            ("Content-Type", "text/plain"),
            ("Content-Length", str(len(body))),
        ]
        start_response(status, headers)
        return [body]
