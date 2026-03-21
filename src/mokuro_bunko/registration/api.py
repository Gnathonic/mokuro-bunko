"""Registration API for mokuro-bunko."""

from __future__ import annotations

import json
import mimetypes
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any, TypedDict

from mokuro_bunko.config import RegistrationConfig
from mokuro_bunko.database import Database
from mokuro_bunko.middleware.auth import AuthAttemptLimiter
from mokuro_bunko.registration.invites import InviteManager
from mokuro_bunko.security import get_client_ip
from mokuro_bunko.validation import validate_password, validate_username

# Rate limiter for registration abuse
REGISTRATION_RATE_LIMITER = AuthAttemptLimiter(max_failures=20, window_seconds=300, block_seconds=900)

# Path to web static files
WEB_DIR = Path(__file__).parent / "web"
MAX_JSON_BODY_BYTES = 64 * 1024


class RegistrationRequest(TypedDict, total=False):
    """Registration request body."""

    username: str
    password: str
    invite_code: str | None


class RegistrationResponse(TypedDict, total=False):
    """Registration response body."""

    success: bool
    message: str
    username: str | None
    status: str | None


class RegistrationAPI:
    """WSGI middleware for registration endpoints."""

    def __init__(
        self,
        app: Callable[..., Any],
        database: Database,
        config: RegistrationConfig,
        rate_limiter: AuthAttemptLimiter | None = None,
    ) -> None:
        """Initialize registration API middleware.

        Args:
            app: WSGI application to wrap.
            database: Database instance.
            config: Registration configuration.
            rate_limiter: Optional rate limiter (for easier testing or isolation).
        """
        self.app = app
        self.db = database
        self.config = config
        self.invites = InviteManager(database)
        self.rate_limiter = rate_limiter or AuthAttemptLimiter(max_failures=20, window_seconds=300, block_seconds=900)

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        # Handle registration API endpoint
        if path == "/api/register":
            if method == "POST":
                return self._handle_register(environ, start_response)
            elif method == "GET":
                return self._handle_registration_info(environ, start_response)
            elif method == "OPTIONS":
                return self._handle_options(environ, start_response)
            else:
                return self._json_response(
                    start_response,
                    405,
                    {"error": "Method not allowed"},
                )

        # Handle registration config endpoint (for JavaScript)
        if path == "/api/register/config":
            if method == "GET":
                return self._handle_registration_info(environ, start_response)
            elif method == "OPTIONS":
                return self._handle_options(environ, start_response)
            else:
                return self._json_response(
                    start_response,
                    405,
                    {"error": "Method not allowed"},
                )

        # Handle registration web page
        if path == "/register" or path == "/register/":
            if method == "GET":
                return self._serve_static_file(start_response, "register.html")
            elif method == "OPTIONS":
                return self._handle_options(environ, start_response)

        # Handle static files for registration page
        if path.startswith("/register/"):
            filename = path[len("/register/"):]
            if filename and method == "GET":
                return self._serve_static_file(start_response, filename)

        # Pass through to wrapped app
        return self.app(environ, start_response)

    def _handle_register(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Handle POST /api/register."""
        # Rate limit registration attempts per IP
        client_ip = get_client_ip(environ)
        key = f"register:{client_ip}"
        allowed, retry_after = self.rate_limiter.allow_attempt(key)
        if not allowed:
            return self._json_response(
                start_response,
                429,
                {"error": f"Too many registration attempts. Retry in {retry_after}s"},
            )
        # Count all registration attempts against the limit (success and failure).
        self.rate_limiter.record_attempt(key)

        # Check if registration is enabled
        if self.config.mode == "disabled":
            return self._json_response(
                start_response,
                403,
                {"error": "Registration is disabled"},
            )

        # Parse request body
        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
            if content_length > MAX_JSON_BODY_BYTES:
                return self._json_response(
                    start_response,
                    413,
                    {"error": "Request body too large"},
                )
            body = environ["wsgi.input"].read(content_length)
            data: RegistrationRequest = json.loads(body.decode("utf-8"))
        except (json.JSONDecodeError, ValueError):
            return self._json_response(
                start_response,
                400,
                {"error": "Invalid JSON body"},
            )

        # Validate username
        username = data.get("username", "").strip()
        username_error = validate_username(username)
        if username_error:
            return self._json_response(
                start_response,
                400,
                {"error": username_error},
            )

        # Validate password
        password = data.get("password", "")
        password_error = validate_password(password)
        if password_error:
            return self._json_response(
                start_response,
                400,
                {"error": password_error},
            )

        # Handle different registration modes
        if self.config.mode == "self":
            return self._register_self(
                start_response, username, password, key
            )
        elif self.config.mode == "invite":
            invite_code = data.get("invite_code", "")
            return self._register_with_invite(
                start_response, username, password, invite_code, key
            )
        elif self.config.mode == "approval":
            return self._register_approval(
                start_response, username, password, key
            )

        # Should not reach here
        self.rate_limiter.record_failure(key)
        return self._json_response(
            start_response,
            500,
            {"error": "Invalid registration mode"},
        )

    def _register_self(
        self,
        start_response: Callable[..., Any],
        username: str,
        password: str,
        rate_limit_key: str,
    ) -> list[bytes]:
        """Handle self-registration mode."""
        # Check if username exists
        if self.db.get_user(username):
            return self._json_response(
                start_response,
                409,
                {"error": "Username already exists"},
            )

        # Create user with default role
        try:
            self.db.create_user(
                username=username,
                password=password,
                role=self.config.default_role,
                status="active",
            )
        except ValueError as e:
            return self._json_response(
                start_response,
                400,
                {"error": str(e)},
            )

        return self._json_response(
            start_response,
            201,
            {
                "success": True,
                "message": "Registration successful",
                "username": username,
                "status": "active",
            },
        )

    def _register_with_invite(
        self,
        start_response: Callable[..., Any],
        username: str,
        password: str,
        invite_code: str,
        rate_limit_key: str,
    ) -> list[bytes]:
        """Handle invite-based registration."""
        if not invite_code:
            return self._json_response(
                start_response,
                400,
                {"error": "Invite code is required"},
            )

        # Validate invite
        invite = self.invites.validate(invite_code)
        if not invite:
            return self._json_response(
                start_response,
                400,
                {"error": "Invalid or expired invite code"},
            )

        # Check if username exists
        if self.db.get_user(username):
            return self._json_response(
                start_response,
                409,
                {"error": "Username already exists"},
            )

        # Create user with invite's role
        try:
            self.db.create_user(
                username=username,
                password=password,
                role=invite["role"],
                status="active",
            )
        except ValueError as e:
            return self._json_response(
                start_response,
                400,
                {"error": str(e)},
            )

        # Mark invite as used
        self.invites.use(invite_code, username)

        return self._json_response(
            start_response,
            201,
            {
                "success": True,
                "message": "Registration successful",
                "username": username,
                "status": "active",
            },
        )

    def _register_approval(
        self,
        start_response: Callable[..., Any],
        username: str,
        password: str,
        rate_limit_key: str,
    ) -> list[bytes]:
        """Handle approval-based registration."""
        # Check if username exists
        if self.db.get_user(username):
            return self._json_response(
                start_response,
                409,
                {"error": "Username already exists"},
            )

        # Create user with pending status
        try:
            self.db.create_user(
                username=username,
                password=password,
                role=self.config.default_role,
                status="pending",
            )
        except ValueError as e:
            return self._json_response(
                start_response,
                400,
                {"error": str(e)},
            )

        return self._json_response(
            start_response,
            201,
            {
                "success": True,
                "message": "Registration submitted for approval",
                "username": username,
                "status": "pending",
            },
        )

    def _handle_registration_info(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Handle GET /api/register - return registration info."""
        info: dict[str, Any] = {
            "mode": self.config.mode,
            "enabled": self.config.mode != "disabled",
        }

        if self.config.mode == "invite":
            info["requires_invite"] = True

        return self._json_response(start_response, 200, info)

    def _handle_options(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Handle OPTIONS request for CORS."""
        start_response("204 No Content", [
            ("Allow", "GET, POST, OPTIONS"),
        ])
        return [b""]

    def _serve_static_file(
        self,
        start_response: Callable[..., Any],
        filename: str,
    ) -> list[bytes]:
        """Serve a static file from the web directory.

        Args:
            start_response: WSGI start_response callable.
            filename: Name of file to serve.

        Returns:
            File contents as list of bytes.
        """
        # Security: prevent directory traversal
        if ".." in filename or filename.startswith("/"):
            return self._json_response(
                start_response, 404, {"error": "File not found"}
            )

        file_path = WEB_DIR / filename

        if not file_path.exists() or not file_path.is_file():
            return self._json_response(
                start_response, 404, {"error": "File not found"}
            )

        # Determine content type
        content_type, _ = mimetypes.guess_type(filename)
        if content_type is None:
            content_type = "application/octet-stream"

        try:
            content = file_path.read_bytes()
        except OSError:
            return self._json_response(
                start_response, 500, {"error": "Failed to read file"}
            )

        headers = [
            ("Content-Type", content_type),
            ("Content-Length", str(len(content))),
            ("Cache-Control", "no-store"),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            ("X-Frame-Options", "DENY"),
            ("X-XSS-Protection", "1; mode=block"),
        ]

        start_response("200 OK", headers)
        return [content]

    def _json_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        data: dict[str, Any],
    ) -> list[bytes]:
        """Return a JSON response.

        Args:
            start_response: WSGI start_response callable.
            status_code: HTTP status code.
            data: Response data dictionary.

        Returns:
            Response body as list of bytes.
        """
        status_messages = {
            200: "OK",
            201: "Created",
            204: "No Content",
            400: "Bad Request",
            403: "Forbidden",
            404: "Not Found",
            405: "Method Not Allowed",
            409: "Conflict",
            413: "Payload Too Large",
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

        start_response(status, headers)
        return [body]
