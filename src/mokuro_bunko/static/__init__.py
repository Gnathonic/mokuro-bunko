"""Static file serving for mokuro-bunko."""

from __future__ import annotations

import mimetypes
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

from mokuro_bunko.security import is_within_path

# Path to static files
STATIC_DIR = Path(__file__).parent

# MIME types
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
    ".woff2": "font/woff2",
    ".woff": "font/woff",
}


class StaticMiddleware:
    """WSGI middleware for serving shared static files."""

    def __init__(self, app: Callable[..., Any]) -> None:
        """Initialize static middleware.

        Args:
            app: WSGI application to wrap.
        """
        self.app = app

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        # Serve shared static files at /_static/
        if path.startswith("/_static/") and method == "GET":
            filename = path[len("/_static/"):]
            if filename:
                return self._serve_static_file(start_response, filename)

        return self.app(environ, start_response)

    def _serve_static_file(
        self,
        start_response: Callable[..., Any],
        filename: str,
    ) -> list[bytes]:
        """Serve a static file.

        Args:
            start_response: WSGI start_response callable.
            filename: Name of file to serve.

        Returns:
            File contents as list of bytes.
        """
        # Security: prevent directory traversal
        if ".." in filename or filename.startswith("/"):
            start_response("404 Not Found", [("Content-Type", "text/plain")])
            return [b"Not Found"]

        file_path = STATIC_DIR / filename

        # Resolve to check for directory traversal
        try:
            resolved = file_path.resolve()
            if not is_within_path(resolved, STATIC_DIR):
                start_response("403 Forbidden", [("Content-Type", "text/plain")])
                return [b"Forbidden"]
        except (OSError, ValueError):
            start_response("404 Not Found", [("Content-Type", "text/plain")])
            return [b"Not Found"]

        if not file_path.exists() or not file_path.is_file():
            start_response("404 Not Found", [("Content-Type", "text/plain")])
            return [b"Not Found"]

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
            start_response("500 Internal Server Error", [("Content-Type", "text/plain")])
            return [b"Error reading file"]

        headers = [
            ("Content-Type", content_type),
            ("Content-Length", str(len(content))),
            ("Cache-Control", "public, max-age=3600"),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            ("X-Frame-Options", "DENY"),
            ("X-XSS-Protection", "1; mode=block"),
        ]

        start_response("200 OK", headers)
        return [content]
