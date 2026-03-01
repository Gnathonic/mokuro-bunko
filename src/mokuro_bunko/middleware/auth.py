"""Authentication and authorization middleware for mokuro-bunko."""

from __future__ import annotations

import base64
from dataclasses import dataclass
from enum import Enum, auto
from typing import TYPE_CHECKING, Any, Callable, Optional

from mokuro_bunko.security import AuthAttemptLimiter, get_client_ip
from mokuro_bunko.webdav.resources import PathMapper

if TYPE_CHECKING:
    from mokuro_bunko.database import Database, UserDict


AUTH_RATE_LIMITER = AuthAttemptLimiter()


class Permission(Enum):
    """Permission types for WebDAV operations."""

    READ = auto()  # Read files (GET, PROPFIND, OPTIONS, HEAD)
    WRITE_PROGRESS = auto()  # Write per-user progress files (own data only)
    ADD_FILES = auto()  # Add new files to library/inbox (PUT, MKCOL)
    MODIFY_DELETE = auto()  # Modify or delete existing files (DELETE, MOVE, COPY)
    MANAGE_INVITES = auto()  # Create/list/delete invite codes
    ADMIN = auto()  # Access admin panel


# Role permission matrix
ROLE_PERMISSIONS: dict[str, set[Permission]] = {
    "anonymous": {
        Permission.READ,
    },
    "registered": {
        Permission.READ,
        Permission.WRITE_PROGRESS,
    },
    "uploader": {
        Permission.READ,
        Permission.WRITE_PROGRESS,
        Permission.ADD_FILES,
    },
    "inviter": {
        Permission.READ,
        Permission.WRITE_PROGRESS,
        Permission.ADD_FILES,
        Permission.MODIFY_DELETE,
        Permission.MANAGE_INVITES,
    },
    "editor": {
        Permission.READ,
        Permission.WRITE_PROGRESS,
        Permission.ADD_FILES,
        Permission.MODIFY_DELETE,
    },
    "admin": {
        Permission.READ,
        Permission.WRITE_PROGRESS,
        Permission.ADD_FILES,
        Permission.MODIFY_DELETE,
        Permission.MANAGE_INVITES,
        Permission.ADMIN,
    },
}

# HTTP methods mapped to required permissions
METHOD_PERMISSIONS: dict[str, Permission] = {
    "GET": Permission.READ,
    "HEAD": Permission.READ,
    "OPTIONS": Permission.READ,
    "PROPFIND": Permission.READ,
    "PROPPATCH": Permission.MODIFY_DELETE,
    "MKCOL": Permission.ADD_FILES,
    "DELETE": Permission.MODIFY_DELETE,
    "MOVE": Permission.MODIFY_DELETE,
    "COPY": Permission.MODIFY_DELETE,
    "LOCK": Permission.WRITE_PROGRESS,
    "UNLOCK": Permission.WRITE_PROGRESS,
    # PUT is context-dependent: progress files vs library files
}


def get_role_permissions(role: str) -> set[Permission]:
    """Get the set of permissions for a role."""
    return ROLE_PERMISSIONS.get(role, set())


def check_permission(role: str, permission: Permission) -> bool:
    """Check if a role has a specific permission."""
    return permission in get_role_permissions(role)


def is_progress_file(path: str) -> bool:
    """Check if a path is a per-user progress/profile file.

    Per-user files are volume-data.json and profiles.json stored
    directly under /mokuro-reader/.
    """
    path = "/" + path.strip("/")
    prefix = f"/{PathMapper.READER_ROOT}/"
    if path.startswith(prefix):
        relative = path[len(prefix):]
        return relative in PathMapper.PER_USER_FILES
    return False


def is_user_progress_path(path: str, username: str) -> bool:
    """Check if a path is the user's own progress data.

    Since per-user files under /mokuro-reader/ are always mapped to the
    current user's private directory, any authenticated user accessing
    these paths is accessing their own data.
    """
    return is_progress_file(path)


def is_library_path(path: str) -> bool:
    """Check if a path is in the shared library (under /mokuro-reader/).

    Library paths are everything under /mokuro-reader/ that is NOT
    a per-user file.
    """
    path = "/" + path.strip("/")
    prefix = f"/{PathMapper.READER_ROOT}/"
    if path.startswith(prefix):
        relative = path[len(prefix):]
        return relative != "" and relative not in PathMapper.PER_USER_FILES
    return False


def is_inbox_path(path: str) -> bool:
    """Check if a path is in the OCR inbox."""
    return path.startswith("/inbox/") or path == "/inbox"


def is_admin_path(path: str) -> bool:
    """Check if a path is an admin endpoint."""
    return path.startswith("/_admin")


def is_invites_admin_api_path(path: str) -> bool:
    """Check if path is an invite management admin API endpoint."""
    return path == "/_admin/api/invites" or path.startswith("/_admin/api/invites/")


@dataclass
class AuthResult:
    """Result of authentication attempt."""

    authenticated: bool
    user: Optional["UserDict"] = None
    role: str = "anonymous"
    error: Optional[str] = None

    @property
    def username(self) -> Optional[str]:
        """Get username if authenticated."""
        return self.user["username"] if self.user else None


@dataclass
class AuthorizationResult:
    """Result of authorization check."""

    authorized: bool
    status_code: int = 200
    error: Optional[str] = None


def parse_basic_auth(authorization_header: Optional[str]) -> tuple[Optional[str], Optional[str]]:
    """Parse Basic auth header.

    Returns:
        Tuple of (username, password) or (None, None) if invalid.
    """
    if not authorization_header:
        return None, None

    if not authorization_header.startswith("Basic "):
        return None, None

    try:
        encoded = authorization_header[6:]
        decoded = base64.b64decode(encoded).decode("utf-8")
        if ":" not in decoded:
            return None, None
        username, password = decoded.split(":", 1)
        return username, password
    except (ValueError, UnicodeDecodeError):
        return None, None


def authenticate_basic_header(
    database: "Database",
    authorization_header: Optional[str],
) -> AuthResult:
    """Authenticate a user from a Basic auth header."""
    username, password = parse_basic_auth(authorization_header)

    if username is None:
        return AuthResult(
            authenticated=False,
            role="anonymous",
        )

    user = database.authenticate_user(username, password)
    if user is None:
        return AuthResult(
            authenticated=False,
            error="Invalid credentials",
        )

    return AuthResult(
        authenticated=True,
        user=user,
        role=user["role"],
    )


class AuthMiddleware:
    """WSGI middleware for authentication and authorization."""

    def __init__(
        self,
        app: Callable[..., Any],
        database: "Database",
        realm: str = "mokuro-bunko",
        allow_anonymous: bool = True,
        registration_config: Any = None,
    ) -> None:
        self.app = app
        self.database = database
        self.realm = realm
        self._allow_anonymous = allow_anonymous
        self._registration_config = registration_config

    @property
    def allow_anonymous(self) -> bool:
        """Check if anonymous access is allowed (reads live config)."""
        if self._registration_config is not None:
            return bool(
                getattr(self._registration_config, "allow_anonymous_browse", True)
                or getattr(self._registration_config, "allow_anonymous_download", True)
            )
        return self._allow_anonymous

    @property
    def allow_anonymous_browse(self) -> bool:
        """Check if anonymous browse/listing access is allowed."""
        if self._registration_config is not None:
            return bool(
                getattr(self._registration_config, "allow_anonymous_browse", not self._registration_config.require_login)
            )
        return self._allow_anonymous

    @property
    def allow_anonymous_download(self) -> bool:
        """Check if anonymous file download access is allowed."""
        if self._registration_config is not None:
            return bool(
                getattr(self._registration_config, "allow_anonymous_download", not self._registration_config.require_login)
            )
        return self._allow_anonymous

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Any:
        """Handle WSGI request."""
        auth_result = self.authenticate(environ)

        environ["mokuro.auth"] = auth_result
        environ["mokuro.user"] = auth_result.user
        environ["mokuro.role"] = auth_result.role
        environ["mokuro.username"] = auth_result.username
        environ["mokuro.db"] = self.database

        authz_result = self.authorize(environ, auth_result)

        if not authz_result.authorized:
            return self._error_response(
                start_response,
                authz_result.status_code,
                authz_result.error or "Access denied",
                include_auth_header=(authz_result.status_code == 401),
            )

        return self.app(environ, start_response)

    def authenticate(self, environ: dict[str, Any]) -> AuthResult:
        """Authenticate request from environ."""
        auth_header = environ.get("HTTP_AUTHORIZATION")
        username, password = parse_basic_auth(auth_header)
        if username is None:
            return AuthResult(authenticated=False, role="anonymous")

        key = f"{get_client_ip(environ)}:{username}"
        allowed, retry_after = AUTH_RATE_LIMITER.allow_attempt(key)
        if not allowed:
            return AuthResult(
                authenticated=False,
                error=f"Too many failed attempts. Retry in {retry_after}s",
            )

        user = self.database.authenticate_user(username, password)
        if user is None:
            AUTH_RATE_LIMITER.record_failure(key)
            return AuthResult(
                authenticated=False,
                error="Invalid credentials",
            )

        AUTH_RATE_LIMITER.record_success(key)
        return AuthResult(
            authenticated=True,
            user=user,
            role=user["role"],
        )

    def authorize(
        self,
        environ: dict[str, Any],
        auth_result: AuthResult,
    ) -> AuthorizationResult:
        """Check if request is authorized."""
        method = environ.get("REQUEST_METHOD", "GET")
        path = environ.get("PATH_INFO", "/")
        role = auth_result.role

        # OPTIONS always allowed for CORS preflight
        if method == "OPTIONS":
            return AuthorizationResult(authorized=True)

        # Handle failed authentication with credentials
        if not auth_result.authenticated and auth_result.error:
            status_code = 429 if "Too many failed attempts" in auth_result.error else 401
            return AuthorizationResult(
                authorized=False,
                status_code=status_code,
                error=auth_result.error,
            )

        # Admin paths require admin permission
        if is_admin_path(path):
            # Allow GET requests to admin static files.
            # AdminAPI enforces role checks for /api/* endpoints.
            if method in ("GET", "HEAD") and "/api/" not in path:
                return AuthorizationResult(authorized=True)

            required_permission = Permission.ADMIN
            if is_invites_admin_api_path(path):
                required_permission = Permission.MANAGE_INVITES

            if not check_permission(role, required_permission):
                if not auth_result.authenticated:
                    return AuthorizationResult(
                        authorized=False,
                        status_code=401,
                        error="Authentication required",
                    )
                return AuthorizationResult(
                    authorized=False,
                    status_code=403,
                    error=(
                        "Invite management access required"
                        if required_permission == Permission.MANAGE_INVITES
                        else "Admin access required"
                    ),
                )
            return AuthorizationResult(authorized=True)

        # Read operations
        if method in ("GET", "HEAD", "PROPFIND"):
            if not auth_result.authenticated:
                if method == "PROPFIND":
                    if not self.allow_anonymous_browse:
                        return AuthorizationResult(
                            authorized=False,
                            status_code=401,
                            error="Authentication required",
                        )
                elif method in ("GET", "HEAD"):
                    # Library file reads are "downloads"
                    if is_library_path(path) and not self.allow_anonymous_download:
                        return AuthorizationResult(
                            authorized=False,
                            status_code=401,
                            error="Authentication required",
                        )

                    # Directory/root reads are "browse" operations
                    if not is_library_path(path) and not self.allow_anonymous_browse:
                        if path in ("/", f"/{PathMapper.READER_ROOT}"):
                            return AuthorizationResult(
                                authorized=False,
                                status_code=401,
                                error="Authentication required",
                            )
            return AuthorizationResult(authorized=True)

        # PUT operation - context dependent
        if method == "PUT":
            return self._authorize_put(path, auth_result)

        # MKCOL (create directory)
        if method == "MKCOL":
            if is_library_path(path):
                if not check_permission(role, Permission.ADD_FILES):
                    if not auth_result.authenticated:
                        return AuthorizationResult(
                            authorized=False,
                            status_code=401,
                            error="Authentication required",
                        )
                    return AuthorizationResult(
                        authorized=False,
                        status_code=403,
                        error="Permission denied: cannot create directories",
                    )
                return AuthorizationResult(authorized=True)
            return AuthorizationResult(
                authorized=False,
                status_code=403,
                error="Permission denied: unsupported target path",
            )

        # DELETE
        if method == "DELETE":
            if is_progress_file(path):
                return self._authorize_progress_write(path, auth_result)

            if (
                role == "uploader"
                and auth_result.username
                and is_library_path(path)
                and self.database.can_user_delete_library_path(auth_result.username, path)
            ):
                return AuthorizationResult(authorized=True)

            if not check_permission(role, Permission.MODIFY_DELETE):
                if not auth_result.authenticated:
                    return AuthorizationResult(
                        authorized=False,
                        status_code=401,
                        error="Authentication required",
                    )
                return AuthorizationResult(
                    authorized=False,
                    status_code=403,
                    error="Permission denied: cannot modify or delete files",
                )
            return AuthorizationResult(authorized=True)

        # MOVE, COPY
        if method in ("MOVE", "COPY"):
            if is_progress_file(path):
                return self._authorize_progress_write(path, auth_result)

            if not check_permission(role, Permission.MODIFY_DELETE):
                if not auth_result.authenticated:
                    return AuthorizationResult(
                        authorized=False,
                        status_code=401,
                        error="Authentication required",
                    )
                return AuthorizationResult(
                    authorized=False,
                    status_code=403,
                    error="Permission denied: cannot modify or delete files",
                )
            return AuthorizationResult(authorized=True)

        # LOCK/UNLOCK
        if method in ("LOCK", "UNLOCK"):
            if is_progress_file(path):
                return self._authorize_progress_write(path, auth_result)
            if not check_permission(role, Permission.MODIFY_DELETE):
                if not auth_result.authenticated:
                    return AuthorizationResult(
                        authorized=False,
                        status_code=401,
                        error="Authentication required",
                    )
                return AuthorizationResult(
                    authorized=False,
                    status_code=403,
                    error="Permission denied",
                )
            return AuthorizationResult(authorized=True)

        # PROPPATCH
        if method == "PROPPATCH":
            if not check_permission(role, Permission.MODIFY_DELETE):
                if not auth_result.authenticated:
                    return AuthorizationResult(
                        authorized=False,
                        status_code=401,
                        error="Authentication required",
                    )
                return AuthorizationResult(
                    authorized=False,
                    status_code=403,
                    error="Permission denied",
                )
            return AuthorizationResult(authorized=True)

        # Default: allow
        return AuthorizationResult(authorized=True)

    def _authorize_put(
        self,
        path: str,
        auth_result: AuthResult,
    ) -> AuthorizationResult:
        """Authorize PUT request based on path context."""
        role = auth_result.role

        # Per-user progress files
        if is_progress_file(path):
            return self._authorize_progress_write(path, auth_result)

        # Library files require ADD_FILES permission
        if is_library_path(path):
            if not check_permission(role, Permission.ADD_FILES):
                if not auth_result.authenticated:
                    return AuthorizationResult(
                        authorized=False,
                        status_code=401,
                        error="Authentication required",
                    )
                return AuthorizationResult(
                    authorized=False,
                    status_code=403,
                    error="Permission denied: cannot add files",
                )
            return AuthorizationResult(authorized=True)

        # Other PUT paths are not supported.
        return AuthorizationResult(
            authorized=False,
            status_code=403,
            error="Permission denied: unsupported target path",
        )

    def _authorize_progress_write(
        self,
        path: str,
        auth_result: AuthResult,
    ) -> AuthorizationResult:
        """Authorize writing to per-user progress files."""
        role = auth_result.role
        username = auth_result.username

        if not auth_result.authenticated:
            return AuthorizationResult(
                authorized=False,
                status_code=401,
                error="Authentication required to save progress",
            )

        if not check_permission(role, Permission.WRITE_PROGRESS):
            return AuthorizationResult(
                authorized=False,
                status_code=403,
                error="Permission denied: cannot save progress",
            )

        # Per-user files under /mokuro-reader/ are always mapped to the
        # current user's directory, so this is always their own data
        if username and is_user_progress_path(path, username):
            return AuthorizationResult(authorized=True)

        return AuthorizationResult(
            authorized=False,
            status_code=403,
            error="Cannot write to other users' progress",
        )

    def _error_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        message: str,
        include_auth_header: bool = False,
    ) -> list[bytes]:
        """Generate error response."""
        status_messages = {
            401: "Unauthorized",
            403: "Forbidden",
            404: "Not Found",
            405: "Method Not Allowed",
            429: "Too Many Requests",
        }
        status = f"{status_code} {status_messages.get(status_code, 'Error')}"

        headers = [("Content-Type", "text/plain; charset=utf-8")]
        if include_auth_header:
            headers.append(("WWW-Authenticate", f'Basic realm="{self.realm}"'))

        start_response(status, headers)
        return [message.encode("utf-8")]
