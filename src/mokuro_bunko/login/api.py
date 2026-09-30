"""Login page API for mokuro-bunko."""

from __future__ import annotations

import json
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.database import TOKEN_KINDS
from mokuro_bunko.middleware.auth import (
    Permission,
    authenticate_bearer,
    bearer_token,
    check_permission,
    parse_basic_auth_checked,
)
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
        app: Callable[..., Iterable[bytes]],
        database: Database | None = None,
        nav_config: Any | None = None,
        on_processor_login_refused: Callable[[str, str], None] | None = None,
    ) -> None:
        """Initialize login API middleware.

        ``on_processor_login_refused(username, ip)``: a processor's token
        request was refused -- the same report the auth middleware makes for a
        refused `/_processor/` request, so the admin panel can name it.
        """
        self.app = app
        self.db = database
        self._on_processor_login_refused = on_processor_login_refused
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

        # A password buys a bearer token; the token signs itself out.
        if path == "/login/api/token" and method == "POST":
            return self._issue_token(environ, start_response)
        if path == "/login/api/token" and method == "DELETE":
            return self._revoke_token(environ, start_response)

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

    def _issue_token(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """``POST /login/api/token``: check the password once, return a bearer token.

        Credentials as JSON ``{"username", "password"}`` or a Basic header;
        ``kind`` (``web``, ``reader``, ``processor``; default ``web``) sets
        the token's lifetime, ``label`` says what holds it. The password is
        rate-limited exactly like a login; the token is then sent as
        ``Authorization: Bearer <token>`` on every request in its place.
        """
        if not self.db:
            return self._json_response(start_response, 500, {"error": "Database not configured"})
        data: dict[str, Any] = {}
        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
        except ValueError:
            content_length = 0
        if content_length > MAX_JSON_BODY_BYTES:
            return self._json_response(start_response, 413, {"error": "Request body too large"})
        if content_length > 0:
            try:
                parsed = json.loads(environ["wsgi.input"].read(content_length).decode("utf-8"))
            except (json.JSONDecodeError, UnicodeDecodeError):
                return self._json_response(start_response, 400, {"error": "Invalid request"})
            if not isinstance(parsed, dict):
                return self._json_response(start_response, 400, {"error": "Invalid request"})
            data = parsed
        username = data.get("username")
        password = data.get("password")
        if not username and not password:
            creds, parse_error = parse_basic_auth_checked(environ.get("HTTP_AUTHORIZATION", ""))
            if parse_error:
                return self._json_response(start_response, 400, {"error": "Invalid credentials"})
            if creds is not None:
                username, password = creds
        if not isinstance(username, str) or not isinstance(password, str) or not username or not password:
            return self._json_response(start_response, 400, {"error": "Missing credentials"})
        kind = data.get("kind") or "web"
        if kind not in TOKEN_KINDS:
            return self._json_response(
                start_response, 400, {"error": f"kind must be one of {', '.join(TOKEN_KINDS)}"}
            )
        label = data.get("label") if isinstance(data.get("label"), str) else ""

        key = f"{get_client_ip(environ)}:{username}"
        allowed, retry_after = AUTH_RATE_LIMITER.allow_attempt(key)
        if not allowed:
            if kind == "processor":
                self._report_processor_refusal(username, environ)
            return self._json_response(
                start_response, 429, {"error": f"Too many failed attempts. Retry in {retry_after}s"}
            )
        user = self.db.authenticate_user(username, password)
        if user is None:
            AUTH_RATE_LIMITER.record_failure(key)
            if kind == "processor":
                self._report_processor_refusal(username, environ)
            return self._json_response(start_response, 401, {"error": "Invalid credentials"})
        AUTH_RATE_LIMITER.record_success(key)
        self.db.prune_expired_auth_tokens()
        token, expires_at = self.db.create_auth_token(user["username"], kind, label=str(label))
        return self._json_response(start_response, 200, {
            "token": token,
            "token_type": "Bearer",
            "kind": kind,
            "expires_at": expires_at,
            "user": {"username": user["username"], "role": user["role"]},
        })

    def _report_processor_refusal(self, username: str, environ: dict[str, Any]) -> None:
        if self._on_processor_login_refused is None:
            return
        try:
            self._on_processor_login_refused(username, get_client_ip(environ))
        except Exception:  # noqa: BLE001 - a listener never breaks a refusal
            pass

    def _revoke_token(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """``DELETE /login/api/token``: sign out the token the request carries."""
        if not self.db:
            return self._json_response(start_response, 500, {"error": "Database not configured"})
        token = bearer_token(environ.get("HTTP_AUTHORIZATION", ""))
        if not token:
            return self._json_response(start_response, 400, {"error": "No bearer token"})
        revoked = self.db.revoke_auth_token(token)
        return self._json_response(start_response, 200, {"revoked": revoked})

    @staticmethod
    def _role_permissions(role: str) -> dict[str, bool]:
        """Derive the client-facing permissions object from a role."""
        return {
            "canWriteProgress": check_permission(role, Permission.WRITE_PROGRESS),
            "canAddFiles": check_permission(role, Permission.ADD_FILES),
            "canModifyDelete": check_permission(role, Permission.MODIFY_DELETE),
        }

    def _metadata_scope(self, role: str, username: str | None) -> dict[str, Any]:
        """Contract-facing scope for the series.json/catalog.json write gate.

        Mirrors `AuthMiddleware._authorize_put`'s Task 11 policy exactly, so
        this endpoint can never advertise more (or less) than a real PUT would
        actually be allowed to do: a MODIFY_DELETE holder may edit any series,
        an uploader only the series it fully owns (`Database.can_user_edit_series`),
        everyone else (`registered`, anonymous) none.
        """
        if check_permission(role, Permission.MODIFY_DELETE):
            return {"scope": "all"}
        if role == "uploader" and username and self.db is not None:
            return {
                "scope": "owned",
                "ownedSeries": self.db.list_series_owned_by(username),
            }
        return {"scope": "none"}

    def _permissions_payload(self, role: str, username: str | None) -> dict[str, Any]:
        """The `permissions` object exactly as the reader's shipped parser
        reads it (Task 11 review F1): `metadata` nests INSIDE `permissions`,
        not as a body-level sibling. The reader's `identity.ts` calls
        `normalizeMetadataPermissions(record.permissions.metadata)` — a
        top-level `body.metadata` is never read, so publishing it there
        instead leaves every account's per-series edit UI unrestricted.
        """
        payload: dict[str, Any] = dict(self._role_permissions(role))
        payload["metadata"] = self._metadata_scope(role, username)
        return payload

    def _get_me(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Identity endpoint: report auth state, role, and permissions.

        Contract (consumed by mokuro-reader; the "authenticated" boolean is
        load-bearing in EVERY response, including 401/429):
        - valid Basic creds (UTF-8 encoded) or a live Bearer token -> 200
          authenticated:true with username/role/created_at (legacy account.js
          keys) + permissions
        - Basic header present but invalid/malformed -> 401 authenticated:false
        - Bearer token unknown, expired or revoked -> 401 authenticated:false
          (sign in again)
        - no Authorization header or another scheme -> 200 authenticated:false
          (anonymous), never 401
        - rate-limited -> 429 authenticated:false

        No WWW-Authenticate header is emitted: this is a fetch()-consumed
        JSON endpoint and a browser Basic-auth popup must be avoided.
        """
        if not self.db:
            return self._json_response(start_response, 500, {"error": "Database not configured"})

        auth_header = environ.get("HTTP_AUTHORIZATION", "")
        token = bearer_token(auth_header)
        if token is not None:
            # A token: valid -> who it is; revoked or expired -> 401, so a
            # client knows to sign in again rather than carry on anonymous.
            result = authenticate_bearer(self.db, token)
            if result.user is None:
                return self._json_response(start_response, 401, {
                    "authenticated": False,
                    "error": result.error or "Invalid or expired token",
                })
            holder = result.user
            return self._json_response(start_response, 200, {
                "authenticated": True,
                "username": holder["username"],
                "role": holder["role"],
                "created_at": holder["created_at"],
                "permissions": self._permissions_payload(holder["role"], holder["username"]),
            })
        creds, parse_error = parse_basic_auth_checked(auth_header)

        if parse_error:
            # Garbled header: 401, but no rate-limiter interaction
            return self._json_response(start_response, 401, {
                "authenticated": False,
                "error": "Invalid credentials",
            })

        if creds is None:
            # No header / non-Basic scheme: anonymous identity
            return self._json_response(start_response, 200, {
                "authenticated": False,
                "role": "anonymous",
                "permissions": self._permissions_payload("anonymous", None),
            })

        username, password = creds
        key = f"{get_client_ip(environ)}:{username}"
        allowed, retry_after = AUTH_RATE_LIMITER.allow_attempt(key)
        if not allowed:
            return self._json_response(start_response, 429, {
                "authenticated": False,
                "error": f"Too many failed attempts. Retry in {retry_after}s",
            })

        user = self.db.authenticate_user(username, password)
        if user is not None:
            AUTH_RATE_LIMITER.record_success(key)
            return self._json_response(start_response, 200, {
                "authenticated": True,
                "username": user["username"],
                "role": user["role"],
                "created_at": user["created_at"],
                "permissions": self._permissions_payload(user["role"], user["username"]),
            })

        AUTH_RATE_LIMITER.record_failure(key)
        return self._json_response(start_response, 401, {
            "authenticated": False,
            "error": "Invalid credentials",
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
        except OSError:
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
