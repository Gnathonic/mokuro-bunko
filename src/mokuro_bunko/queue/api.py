"""OCR queue status page for mokuro-bunko."""

from __future__ import annotations

import json
import math
import urllib.parse
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any

from mokuro_bunko.library_index import LibraryIndexCache
from mokuro_bunko.middleware.auth import authenticate_basic_header
from mokuro_bunko.security import is_within_path

STATIC_DIR = Path(__file__).parent / "web"

MIME_TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "application/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
}

MAX_JSON_BODY_BYTES = 64 * 1024


class QueueAPI:
    """WSGI middleware for OCR queue status page."""

    def __init__(
        self,
        app: Callable[..., Any],
        storage_base_path: str,
        ocr_backend: str = "unknown",
        database: Any | None = None,
        queue_config: Any | None = None,
        library_index: LibraryIndexCache | None = None,
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
        elif path == "/queue/api/ocr" and method == "GET":
            if not self.public_access and not self._is_authenticated(environ):
                return self._json_response(start_response, 401, {"error": "Authentication required"})
            return self._get_ocr_status_api(start_response, environ)
        elif path == "/queue/api/ocr/history" and method == "GET":
            if not self.public_access and not self._is_authenticated(environ):
                return self._json_response(start_response, 401, {"error": "Authentication required"})
            parsed = self._parse_history_query(environ, default_limit=50)
            if parsed is None:
                return self._json_response(start_response, 400, {"error": "Invalid history query parameters"})
            return self._json_response(start_response, 200, {
                "events": self._read_ocr_history(
                    limit=parsed["limit"],
                    status=parsed["status"],
                    series=parsed["series"],
                    since=parsed["since"],
                )
            })
        elif path == "/queue/api/ocr/control" and method == "POST":
            return self._control_ocr(start_response, environ)
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

    def _is_admin_authenticated(self, environ: dict[str, Any]) -> bool:
        """Return True only for authenticated admin users."""
        if self.database is None:
            return False
        auth_header = environ.get("HTTP_AUTHORIZATION")
        auth_result = authenticate_basic_header(self.database, auth_header)
        return bool(auth_result.authenticated and auth_result.role == "admin")

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
            "ocr_worker": self._extract_ocr_worker_state(),
        }
        return self._json_response(start_response, 200, data)

    def _get_ocr_worker(self) -> object | None:
        """Return the current global OCR worker, if available."""
        try:
            from mokuro_bunko.ocr.watcher import CURRENT_OCR_WORKER
            return CURRENT_OCR_WORKER
        except ImportError:
            return None

    def _extract_ocr_worker_state(self) -> dict[str, Any]:
        worker = self._get_ocr_worker()
        if not worker:
            return {"available": False, "active": False, "paused": False}

        return {
            "available": True,
            "active": worker.is_running,
            "paused": worker.is_paused(),
        }

    def _get_ocr_status_api(
        self,
        start_response: Callable[..., Any],
        environ: dict[str, Any],
    ) -> list[bytes]:
        state = self._extract_ocr_worker_state()
        progress = self._read_ocr_progress()
        parsed = self._parse_history_query(environ, default_limit=10)
        if parsed is None:
            return self._json_response(start_response, 400, {"error": "Invalid history query parameters"})
        return self._json_response(start_response, 200, {
            "ocr_worker": state,
            "progress": progress,
            "history": self._read_ocr_history(
                limit=parsed["limit"],
                status=parsed["status"],
                series=parsed["series"],
                since=parsed["since"],
            ),
        })

    @staticmethod
    def _parse_history_query(
        environ: dict[str, Any],
        default_limit: int,
    ) -> dict[str, Any] | None:
        """Parse and validate OCR history query arguments."""
        query = urllib.parse.parse_qs(environ.get("QUERY_STRING", ""))
        raw_limit = query.get("limit", [str(default_limit)])[0]
        try:
            limit = int(raw_limit)
        except ValueError:
            return None
        limit = max(1, min(limit, 500))
        status = query.get("status", [None])[0]
        series = query.get("series", [None])[0]
        raw_since = query.get("since", [None])[0]
        since: float | None = None
        if raw_since is not None:
            try:
                since = float(raw_since)
            except ValueError:
                return None
            if not math.isfinite(since):
                return None
        return {
            "limit": limit,
            "status": status,
            "series": series,
            "since": since,
        }

    def _read_ocr_history(
        self,
        limit: int = 50,
        status: str | None = None,
        series: str | None = None,
        since: float | None = None,
    ) -> list[dict[str, Any]]:
        """Read OCR history events from jsonl file."""
        history_path = Path(self.storage_base_path) / ".ocr-history.jsonl"
        if not history_path.exists():
            return []
        try:
            lines = history_path.read_text(encoding="utf-8").splitlines()
        except OSError:
            return []

        status_filter = status.strip().lower() if isinstance(status, str) and status.strip() else None
        series_filter = series.strip().lower() if isinstance(series, str) and series.strip() else None
        events: list[dict[str, Any]] = []
        max_events = max(1, limit)
        for line in reversed(lines):
            if not line.strip():
                continue
            try:
                parsed = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(parsed, dict):
                continue
            if status_filter is not None:
                parsed_status = parsed.get("status")
                if not isinstance(parsed_status, str) or parsed_status.lower() != status_filter:
                    continue
            if series_filter is not None:
                parsed_series = parsed.get("series")
                if not isinstance(parsed_series, str) or series_filter not in parsed_series.lower():
                    continue
            if since is not None:
                parsed_ts = parsed.get("timestamp")
                if not isinstance(parsed_ts, (int, float)) or float(parsed_ts) < since:
                    continue
            events.append(parsed)
            if len(events) >= max_events:
                break
        events.reverse()
        return events

    def _control_ocr(
        self,
        start_response: Callable[..., Any],
        environ: dict[str, Any],
    ) -> list[bytes]:
        if not self._is_admin_authenticated(environ):
            return self._json_response(start_response, 403, {"error": "Admin access required"})

        try:
            content_length = int(environ.get("CONTENT_LENGTH", 0) or 0)
            if content_length < 0:
                return self._json_response(start_response, 400, {"error": "Invalid Content-Length"})
            if content_length > MAX_JSON_BODY_BYTES:
                return self._json_response(start_response, 413, {"error": "Request body too large"})
            body = environ["wsgi.input"].read(content_length)
            data = json.loads(body.decode("utf-8") if body else "{}")
        except (ValueError, json.JSONDecodeError):
            return self._json_response(start_response, 400, {"error": "Invalid JSON body"})

        if not isinstance(data, dict):
            return self._json_response(start_response, 400, {"error": "Invalid JSON body"})

        raw_action = data.get("action", "")
        if not isinstance(raw_action, str):
            return self._json_response(start_response, 400, {"error": "Invalid action"})
        action = raw_action.lower()
        worker = self._get_ocr_worker()
        if not worker:
            return self._json_response(start_response, 503, {"error": "OCR worker unavailable"})

        if action == "pause":
            worker.pause()
            return self._json_response(start_response, 200, {"success": True, "status": "paused"})
        if action == "resume":
            worker.resume()
            return self._json_response(start_response, 200, {"success": True, "status": "running"})

        return self._json_response(start_response, 400, {"error": "Invalid action"})

    def _read_ocr_progress(self) -> dict[str, Any] | None:
        base = Path(self.storage_base_path)
        candidate_paths = [
            base / ".ocr-progress.json",
            base.parent / ".ocr-progress.json",
        ]

        progress_path = None
        for candidate in candidate_paths:
            if candidate.exists():
                progress_path = candidate
                break

        if progress_path is None:
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
            "error": data.get("error"),
            "started_at": data.get("started_at"),
            "updated_at": data.get("updated_at"),
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
                ("X-Content-Type-Options", "nosniff"),
                ("Referrer-Policy", "no-referrer"),
                ("X-Frame-Options", "DENY"),
                ("X-XSS-Protection", "1; mode=block"),
            ])
            return [content]
        except OSError:
            return self._error_response(start_response, 500, "Error")

    @staticmethod
    def _json_response(
        start_response: Callable[..., Any],
        status_code: int,
        data: Any,
    ) -> list[bytes]:
        body = json.dumps(data).encode("utf-8")
        status_map = {
            200: "OK",
            201: "Created",
            400: "Bad Request",
            401: "Unauthorized",
            403: "Forbidden",
            404: "Not Found",
            413: "Payload Too Large",
            429: "Too Many Requests",
            500: "Internal Server Error",
            503: "Service Unavailable",
        }
        status = f"{status_code} {status_map.get(status_code, 'Error')}"
        start_response(status, [
            ("Content-Type", "application/json; charset=utf-8"),
            ("Content-Length", str(len(body))),
            ("Cache-Control", "no-cache"),
            ("X-Content-Type-Options", "nosniff"),
            ("Referrer-Policy", "no-referrer"),
            ("X-Frame-Options", "DENY"),
            ("X-XSS-Protection", "1; mode=block"),
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
