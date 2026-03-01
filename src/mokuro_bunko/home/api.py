"""Home page API for mokuro-bunko."""

from __future__ import annotations

import json
import mimetypes
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

from mokuro_bunko.security import is_within_path

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
    ".woff2": "font/woff2",
    ".woff": "font/woff",
}


def is_browser_request(environ: dict[str, Any]) -> bool:
    """Check if request appears to be from a browser.

    WebDAV clients typically don't send Accept headers with text/html,
    while browsers do.

    Args:
        environ: WSGI environ dict.

    Returns:
        True if request appears to be from a browser.
    """
    accept = environ.get("HTTP_ACCEPT", "")
    user_agent = environ.get("HTTP_USER_AGENT", "").lower()

    # If Accept header includes text/html, it's likely a browser
    if "text/html" in accept:
        return True

    # Check for common WebDAV client indicators
    webdav_clients = [
        "davfs",
        "cadaver",
        "cyberduck",
        "webdav",
        "gvfs",
        "nautilus",
        "finder",
        "microsoft-webdav",
        "litmus",
    ]
    for client in webdav_clients:
        if client in user_agent:
            return False

    # Default to browser for GET requests without specific WebDAV headers
    if "Depth" in environ.get("HTTP_DEPTH", ""):
        return False

    return True


class HomePageAPI:
    """WSGI middleware for serving the home/welcome page."""

    def __init__(
        self,
        app: Callable[..., Any],
        catalog_config: Optional[Any] = None,
    ) -> None:
        self.app = app
        self._catalog_config = catalog_config

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        # Handle stats API endpoint
        if path == "/api/stats":
            if method == "GET":
                return self._handle_stats(environ, start_response)
            elif method == "OPTIONS":
                return self._handle_options(environ, start_response)
            else:
                return self._json_response(
                    start_response, 405, {"error": "Method not allowed"}
                )

        # Handle home page static files at /_home/
        if path.startswith("/_home/"):
            filename = path[len("/_home/"):]
            if filename and method == "GET":
                return self._serve_static_file(start_response, filename)

        # Optionally replace home page with direct catalog access.
        if (
            path == "/"
            and method == "GET"
            and is_browser_request(environ)
            and self._catalog_config is not None
            and getattr(self._catalog_config, "enabled", False)
            and getattr(self._catalog_config, "use_as_homepage", False)
        ):
            start_response("302 Found", [("Location", "/catalog/")])
            return [b""]

        # Serve welcome page at root for browser requests
        if path == "/" and method == "GET" and is_browser_request(environ):
            return self._serve_static_file(start_response, "index.html")

        # Pass through to wrapped app (WebDAV)
        return self.app(environ, start_response)

    def _handle_stats(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Handle GET /api/stats — returns zeroed stats."""
        return self._json_response(start_response, 200, {
            "total_users": 0,
            "total_volumes": 0,
            "total_pages_read": 0,
            "total_characters_read": 0,
            "total_reading_time_seconds": 0,
            "total_reading_time_formatted": "0s",
            "last_updated": 0,
        })

    def _handle_options(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Handle OPTIONS request for CORS."""
        start_response("204 No Content", [
            ("Allow", "GET, OPTIONS"),
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

        # Resolve to check for directory traversal
        try:
            resolved = file_path.resolve()
            if not is_within_path(resolved, WEB_DIR):
                return self._json_response(
                    start_response, 403, {"error": "Forbidden"}
                )
        except (OSError, ValueError):
            return self._json_response(
                start_response, 404, {"error": "File not found"}
            )

        if not file_path.exists() or not file_path.is_file():
            return self._json_response(
                start_response, 404, {"error": "File not found"}
            )

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
            return self._json_response(
                start_response, 500, {"error": "Failed to read file"}
            )

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
            204: "No Content",
            400: "Bad Request",
            403: "Forbidden",
            404: "Not Found",
            405: "Method Not Allowed",
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
