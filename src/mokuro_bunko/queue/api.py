"""OCR queue status page for mokuro-bunko."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

from mokuro_bunko.library_index import LibraryIndexCache
from mokuro_bunko.middleware.auth import authenticate_basic_header
from mokuro_bunko.security import is_within_path

STATIC_DIR = Path(__file__).parent / "web"

MIME_TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "application/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
}


class QueueAPI:
    """WSGI middleware for OCR queue status page."""

    def __init__(
        self,
        app: Callable[..., Any],
        storage_base_path: str,
        ocr_backend: str = "unknown",
        database: Optional[Any] = None,
        queue_config: Optional[Any] = None,
        library_index: Optional[LibraryIndexCache] = None,
    ) -> None:
        self.app = app
        self.storage_base_path = Path(storage_base_path)
        self.ocr_backend = ocr_backend
        self.database = database
        self._queue_config = queue_config
        if library_index is not None:
            self._library_index = library_index
        else:
            self._library_index = LibraryIndexCache(self.storage_base_path / "library", ttl=30.0)

    @property
    def show_in_nav(self) -> bool:
        if self._queue_config is not None:
            return bool(getattr(self._queue_config, "show_in_nav", False))
        return False

    @property
    def public_access(self) -> bool:
        if self._queue_config is not None:
            return bool(getattr(self._queue_config, "public_access", True))
        return True

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        if path in ("/queue", "/queue/"):
            return self._serve_static(start_response, "index.html")
        elif path == "/queue/api/config" and method == "GET":
            return self._json_response(start_response, 200, {
                "show_in_nav": self.show_in_nav,
                "public_access": self.public_access,
            })
        elif path == "/queue/api/status" and method == "GET":
            if not self.public_access and not self._is_authenticated(environ):
                return self._json_response(start_response, 401, {"error": "Authentication required"})
            return self._get_status(start_response)
        elif path.startswith("/queue/"):
            filename = path[len("/queue/"):]
            return self._serve_static(start_response, filename)

        return self.app(environ, start_response)

    def _is_authenticated(self, environ: dict[str, Any]) -> bool:
        """Return True when request includes valid Basic auth credentials."""
        if self.database is None:
            return False
        auth_header = environ.get("HTTP_AUTHORIZATION")
        auth_result = authenticate_basic_header(self.database, auth_header)
        return bool(auth_result.authenticated)

    def _get_status(
        self, start_response: Callable[..., Any]
    ) -> list[bytes]:
        current = self._read_ocr_progress()
        pending_ocr, pending_thumbs = self._scan_library()

        data = {
            "current": current,
            "pending_ocr": pending_ocr,
            "pending_thumbnails": pending_thumbs,
            "backend": self.ocr_backend,
        }
        return self._json_response(start_response, 200, data)

    def _read_ocr_progress(self) -> Optional[dict[str, Any]]:
        progress_path = self.storage_base_path / ".ocr-progress.json"
        if not progress_path.exists():
            return None
        try:
            data = json.loads(progress_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return None
        if not isinstance(data, dict) or not data.get("active"):
            return None
        return {
            "series": data.get("series"),
            "volume": data.get("volume"),
            "percent": data.get("percent", 0),
            "eta_seconds": data.get("eta_seconds"),
            "done_pages": data.get("done_pages", 0),
            "total_pages": data.get("total_pages"),
            "status": data.get("status", "running"),
        }

    def _scan_library(self) -> tuple[list[dict[str, str]], int]:
        """Read pending OCR/thumbnail counts from shared library index."""
        snapshot = self._library_index.get_snapshot()
        pending_ocr = [
            {"series": series_name, "volume": volume_name}
            for series_name, volume_name in snapshot.pending_ocr
        ]
        return pending_ocr, snapshot.pending_thumbnails

    def _serve_static(
        self, start_response: Callable[..., Any], filename: str
    ) -> list[bytes]:
        if not filename or ".." in filename:
            return self._error_response(start_response, 404, "Not found")
        file_path = STATIC_DIR / filename
        if not file_path.is_file() or not is_within_path(file_path, STATIC_DIR):
            return self._error_response(start_response, 404, "Not found")
        suffix = file_path.suffix.lower()
        content_type = MIME_TYPES.get(suffix, "application/octet-stream")
        try:
            content = file_path.read_bytes()
            start_response("200 OK", [
                ("Content-Type", content_type),
                ("Content-Length", str(len(content))),
                ("Cache-Control", "no-cache"),
            ])
            return [content]
        except IOError:
            return self._error_response(start_response, 500, "Error")

    @staticmethod
    def _json_response(
        start_response: Callable[..., Any],
        status_code: int,
        data: Any,
    ) -> list[bytes]:
        body = json.dumps(data).encode("utf-8")
        status = f"{status_code} OK" if status_code == 200 else f"{status_code} Error"
        start_response(status, [
            ("Content-Type", "application/json; charset=utf-8"),
            ("Content-Length", str(len(body))),
            ("Cache-Control", "no-cache"),
        ])
        return [body]

    @staticmethod
    def _error_response(
        start_response: Callable[..., Any],
        status_code: int,
        message: str,
    ) -> list[bytes]:
        body = message.encode("utf-8")
        start_response(f"{status_code} {message}", [
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Content-Length", str(len(body))),
        ])
        return [body]
