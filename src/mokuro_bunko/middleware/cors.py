"""CORS middleware for mokuro-bunko."""

from __future__ import annotations

import re
from collections.abc import Callable, Iterable
from typing import Any

from mokuro_bunko.config import CorsConfig

# WebDAV methods that need to be allowed
WEBDAV_METHODS = [
    "GET",
    "HEAD",
    "POST",
    "PUT",
    "DELETE",
    "OPTIONS",
    "PROPFIND",
    "PROPPATCH",
    "MKCOL",
    "COPY",
    "MOVE",
    "LOCK",
    "UNLOCK",
]

# Headers that clients may send
ALLOWED_HEADERS = [
    "Authorization",
    "Content-Type",
    "Content-Length",
    "Depth",
    "Destination",
    "If",
    "If-Match",
    "If-None-Match",
    "If-Modified-Since",
    "If-Unmodified-Since",
    "Lock-Token",
    "Overwrite",
    "Range",
    "Timeout",
    "X-Requested-With",
]

# Headers that are exposed to the client
EXPOSED_HEADERS = [
    "Content-Length",
    "Content-Type",
    "DAV",
    "ETag",
    "Last-Modified",
    "Location",
    "Lock-Token",
    "WWW-Authenticate",
]


def compile_origin_pattern(pattern: str) -> re.Pattern[str]:
    """Compile an origin pattern to a regex.

    Supports:
    - Exact match: "https://example.com"
    - Wildcard port: "http://localhost:*"

    Args:
        pattern: Origin pattern string.

    Returns:
        Compiled regex pattern.
    """
    # Escape special regex characters except *
    escaped = re.escape(pattern)
    # Replace escaped \* with regex for port number
    if r"\*" in escaped:
        # Replace :* with :\d+ for port matching
        escaped = escaped.replace(r":\*", r":\d+")
    return re.compile(f"^{escaped}$")


def is_origin_allowed(origin: str, allowed_origins: list[str]) -> bool:
    """Check if an origin is in the allowed list.

    Args:
        origin: Origin header value.
        allowed_origins: List of allowed origin patterns.

    Returns:
        True if origin is allowed.
    """
    return CorsConfig(
        enabled=True,
        allowed_origins=allowed_origins,
        allow_credentials=True,
    ).is_origin_allowed(origin)


def get_cors_headers(
    origin: str,
    config: CorsConfig,
    is_preflight: bool = False,
    private_network_requested: bool = False,
) -> list[tuple[str, str]]:
    """Generate CORS headers for a response.

    Args:
        origin: Request Origin header value.
        config: CORS configuration.
        is_preflight: True if this is a preflight (OPTIONS) request.
        private_network_requested: True if Access-Control-Request-Private-Network is set.

    Returns:
        List of (header_name, header_value) tuples.
    """
    headers: list[tuple[str, str]] = []

    if not config.enabled:
        return headers

    if not origin:
        return headers

    if not is_origin_allowed(origin, config.allowed_origins):
        return headers

    # Access-Control-Allow-Origin
    headers.append(("Access-Control-Allow-Origin", origin))

    # Access-Control-Allow-Credentials
    if config.allow_credentials:
        headers.append(("Access-Control-Allow-Credentials", "true"))

    # Vary header to indicate response varies by Origin
    headers.append(("Vary", "Origin"))

    # Preflight-specific headers
    if is_preflight:
        # Access-Control-Allow-Methods
        headers.append((
            "Access-Control-Allow-Methods",
            ", ".join(WEBDAV_METHODS),
        ))

        # Access-Control-Allow-Headers
        headers.append((
            "Access-Control-Allow-Headers",
            ", ".join(ALLOWED_HEADERS),
        ))

        # Access-Control-Max-Age (cache preflight for 1 hour)
        headers.append(("Access-Control-Max-Age", "3600"))

        # Allow private network access (e.g. public site → localhost)
        if private_network_requested:
            headers.append(("Access-Control-Allow-Private-Network", "true"))

    else:
        # Non-preflight: expose headers
        headers.append((
            "Access-Control-Expose-Headers",
            ", ".join(EXPOSED_HEADERS),
        ))

    return headers


class CorsMiddleware:
    """WSGI middleware for CORS handling."""

    def __init__(
        self,
        app: Callable[..., Any],
        config: CorsConfig,
    ) -> None:
        """Initialize CORS middleware.

        Args:
            app: WSGI application to wrap.
            config: CORS configuration.
        """
        self.app = app
        self.config = config

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        method = environ.get("REQUEST_METHOD", "GET")
        origin = environ.get("HTTP_ORIGIN", "")

        # Handle preflight OPTIONS request
        if method == "OPTIONS" and origin:
            return self._handle_preflight(environ, start_response, origin)

        # For non-preflight requests, wrap start_response to add CORS headers
        def cors_start_response(
            status: str,
            headers: list[tuple[str, str]],
            exc_info: Any = None,
        ) -> Callable[[bytes], None]:
            # Add CORS headers if origin is allowed
            cors_headers = get_cors_headers(origin, self.config, is_preflight=False)
            all_headers = list(headers) + cors_headers
            return start_response(status, all_headers, exc_info)

        return self.app(environ, cors_start_response)

    def _handle_preflight(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        origin: str,
    ) -> list[bytes]:
        """Handle CORS preflight request.

        Args:
            environ: WSGI environ dict.
            start_response: WSGI start_response callable.
            origin: Request Origin header value.

        Returns:
            Empty response body.
        """
        private_network = environ.get(
            "HTTP_ACCESS_CONTROL_REQUEST_PRIVATE_NETWORK", ""
        ).lower() == "true"
        cors_headers = get_cors_headers(
            origin, self.config, is_preflight=True,
            private_network_requested=private_network,
        )

        if cors_headers:
            # Origin is allowed - return 204 with CORS headers
            start_response("204 No Content", cors_headers)
        else:
            # Origin not allowed - return 204 without CORS headers
            # (browser will block the actual request)
            start_response("204 No Content", [])

        return [b""]
