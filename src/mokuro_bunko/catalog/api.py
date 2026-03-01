"""Public catalog API for mokuro-bunko."""

from __future__ import annotations

import json
import urllib.parse
from pathlib import Path
from typing import Any, Callable, Iterable, Optional

from mokuro_bunko.library_index import LibraryIndexCache
from mokuro_bunko.security import is_within_path

# Static files directory
STATIC_DIR = Path(__file__).parent / "web"

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
}


class CatalogAPI:
    """WSGI middleware for public catalog."""

    def __init__(
        self,
        app: Callable[..., Any],
        storage_base_path: Optional[str] = None,
        enabled: bool = False,
        catalog_config: Any = None,
        library_index: Optional[LibraryIndexCache] = None,
    ) -> None:
        """Initialize catalog API middleware.

        Args:
            app: WSGI application to wrap.
            storage_base_path: Base path for library storage.
            enabled: Whether the catalog is enabled.
            catalog_config: Live CatalogConfig reference for runtime toggling.
        """
        self.app = app
        self.storage_base_path = Path(storage_base_path) if storage_base_path else None
        self._enabled = enabled
        self._catalog_config = catalog_config
        if library_index is not None:
            self._library_index = library_index
        elif self.storage_base_path is not None:
            self._library_index = LibraryIndexCache(self.storage_base_path, ttl=30.0)
        else:
            self._library_index = None

    @property
    def enabled(self) -> bool:
        """Check if catalog is enabled (reads live config)."""
        if self._catalog_config is not None:
            return self._catalog_config.enabled
        return self._enabled

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Handle WSGI request."""
        if not self.enabled:
            return self.app(environ, start_response)

        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        # Handle catalog routes
        if path == "/catalog" or path == "/catalog/":
            return self._serve_static(start_response, "index.html")
        elif path.startswith("/catalog/api/"):
            return self._handle_api(environ, start_response, path, method)
        elif path.startswith("/catalog/"):
            # Serve static files
            filename = path[len("/catalog/"):]
            return self._serve_static(start_response, filename)

        return self.app(environ, start_response)

    def _handle_api(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        path: str,
        method: str,
    ) -> Iterable[bytes]:
        """Handle API requests."""
        if path == "/catalog/api/library" and method == "GET":
            return self._list_library(start_response)
        elif path == "/catalog/api/config" and method == "GET":
            return self._get_config(start_response)
        elif path == "/catalog/api/ocr-status" and method == "GET":
            return self._get_ocr_status(start_response)
        elif path == "/catalog/api/series" and method == "GET":
            query = urllib.parse.parse_qs(environ.get("QUERY_STRING", ""))
            series_name = query.get("name", [""])[0]
            if not series_name:
                return self._json_response(start_response, 400, {"error": "Missing series name"})
            return self._get_series(start_response, series_name)
        elif path.startswith("/catalog/api/series/") and method == "GET":
            series_name = urllib.parse.unquote(path[len("/catalog/api/series/"):])
            return self._get_series(start_response, series_name)
        elif path == "/catalog/api/cover" and method == "GET":
            query = urllib.parse.parse_qs(environ.get("QUERY_STRING", ""))
            cover_path = query.get("path", [""])[0]
            if not cover_path:
                return self._error_response(start_response, 400, "Missing cover path")
            return self._serve_cover(start_response, cover_path)
        elif path.startswith("/catalog/api/cover/") and method == "GET":
            cover_path = urllib.parse.unquote(path[len("/catalog/api/cover/"):])
            return self._serve_cover(start_response, cover_path)

        return self._json_response(start_response, 404, {"error": "Not found"})

    def _get_ocr_status(self, start_response: Callable[..., Any]) -> list[bytes]:
        """Return live OCR progress status for the currently active volume."""
        progress = self._read_ocr_progress()
        if not progress:
            return self._json_response(start_response, 200, {"active": False})
        return self._json_response(start_response, 200, progress)

    def _get_config(self, start_response: Callable[..., Any]) -> list[bytes]:
        """Return catalog configuration for the frontend."""
        reader_url = "https://reader.mokuro.app"
        if self._catalog_config is not None:
            reader_url = self._catalog_config.reader_url
        return self._json_response(start_response, 200, {"reader_url": reader_url})

    def _list_library(self, start_response: Callable[..., Any]) -> list[bytes]:
        """List all series in the library."""
        if self._library_index is None:
            return self._json_response(start_response, 200, {"series": []})

        snapshot = self._library_index.get_snapshot()
        series_list: list[dict[str, Any]] = []
        for series in snapshot.series:
            series_info: dict[str, Any] = {
                "name": series.name,
                "path": series.name,
                "cover": series.cover,
                "volumes": [],
            }
            for volume in series.volumes:
                vol_info: dict[str, Any] = {
                    "name": volume.name,
                    "ocr_pending": volume.has_cbz and not volume.has_mokuro and not volume.has_mokuro_gz,
                    "ocr_active": False,
                }
                if volume.cover is not None:
                    vol_info["cover"] = volume.cover
                series_info["volumes"].append(vol_info)
            series_list.append(series_info)

        data: dict[str, Any] = {"series": series_list}
        data = self._patch_ocr_progress(data)
        return self._json_response(start_response, 200, data)

    def _get_series(self, start_response: Callable[..., Any], series_name: str) -> list[bytes]:
        """Get volumes for a specific series."""
        if self._library_index is None:
            return self._json_response(start_response, 404, {"error": "Not found"})
        if not self.storage_base_path:
            return self._json_response(start_response, 404, {"error": "Not found"})
        series_dir = (self.storage_base_path / series_name).resolve()
        if not is_within_path(series_dir, self.storage_base_path):
            return self._json_response(start_response, 403, {"error": "Forbidden"})

        snapshot = self._library_index.get_snapshot()
        series = snapshot.series_by_name(series_name)
        if series is None:
            return self._json_response(start_response, 404, {"error": "Series not found"})

        progress = self._read_ocr_progress()

        volumes = []
        for volume in series.volumes:
            vol_info: dict[str, Any] = {"name": volume.name, "cover": volume.cover}
            is_active = self._is_active_ocr_volume(progress, series_name, volume.name)
            vol_info["ocr_pending"] = volume.has_cbz and not volume.has_mokuro and not volume.has_mokuro_gz
            vol_info["ocr_active"] = is_active
            if is_active:
                vol_info["ocr_pending"] = False
                vol_info["ocr_progress"] = self._volume_progress(progress)
            volumes.append(vol_info)

        # Series cover is the first volume's cover
        series_cover = None
        for v in volumes:
            if v["cover"]:
                series_cover = v["cover"]
                break

        return self._json_response(start_response, 200, {
            "name": series_name,
            "cover": series_cover,
            "volumes": volumes,
        })

    def _serve_cover(self, start_response: Callable[..., Any], cover_path: str) -> list[bytes]:
        """Serve a cover image."""
        if not self.storage_base_path:
            return self._error_response(start_response, 404, "Not found")

        try:
            file_path = (self.storage_base_path / cover_path).resolve()
            if not is_within_path(file_path, self.storage_base_path):
                return self._error_response(start_response, 403, "Forbidden")
        except (ValueError, OSError):
            return self._error_response(start_response, 400, "Invalid path")

        if not file_path.exists() or not file_path.is_file():
            return self._error_response(start_response, 404, "Not found")

        ext = file_path.suffix.lower()
        if ext not in (".webp", ".jpg", ".jpeg", ".png"):
            return self._error_response(start_response, 403, "Forbidden")

        content_type = MIME_TYPES.get(ext, "application/octet-stream")

        try:
            content = file_path.read_bytes()
            headers = [
                ("Content-Type", content_type),
                ("Content-Length", str(len(content))),
                ("Cache-Control", "public, max-age=3600"),
            ]
            start_response("200 OK", headers)
            return [content]
        except IOError:
            return self._error_response(start_response, 500, "Error")

    def _read_ocr_progress(self) -> Optional[dict[str, Any]]:
        """Load live OCR progress from sidecar state file."""
        if not self.storage_base_path:
            return None
        progress_path = self.storage_base_path.parent / ".ocr-progress.json"
        if not progress_path.exists():
            return None
        try:
            data = json.loads(progress_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return None
        if not isinstance(data, dict):
            return None
        if not data.get("active"):
            return None
        return data

    def _is_active_ocr_volume(
        self,
        progress: Optional[dict[str, Any]],
        series_name: str,
        volume_name: str,
    ) -> bool:
        """Return True when OCR progress points at this series/volume."""
        if not progress:
            return False
        relative_cbz = progress.get("relative_cbz")
        if not isinstance(relative_cbz, str):
            return False
        expected = f"{series_name}/{volume_name}.cbz"
        return relative_cbz.casefold() == expected.casefold()

    @staticmethod
    def _volume_progress(progress: Optional[dict[str, Any]]) -> Optional[dict[str, Any]]:
        """Return compact progress payload for per-volume API fields."""
        if not progress:
            return None
        return {
            "percent": progress.get("percent"),
            "eta_seconds": progress.get("eta_seconds"),
            "status": progress.get("status"),
            "processed_pages": progress.get("processed_pages"),
            "total_pages": progress.get("total_pages"),
        }

    def _patch_ocr_progress(self, data: dict[str, Any]) -> dict[str, Any]:
        """Overlay live OCR progress without mutating cached base data."""
        progress = self._read_ocr_progress()
        if not progress:
            return data
        relative_cbz = progress.get("relative_cbz")
        if not isinstance(relative_cbz, str):
            return data
        series_list = data.get("series", [])
        if not isinstance(series_list, list):
            return data
        for series_idx, series in enumerate(series_list):
            volumes = series.get("volumes", [])
            if not isinstance(volumes, list):
                continue
            for vol_idx, vol in enumerate(volumes):
                expected = f"{series['name']}/{vol['name']}.cbz"
                if relative_cbz.casefold() == expected.casefold():
                    # Copy only along the path that changes to keep cache hits cheap.
                    out = dict(data)
                    out_series = list(series_list)
                    out["series"] = out_series
                    out_series_item = dict(series)
                    out_series[series_idx] = out_series_item
                    out_volumes = list(volumes)
                    out_series_item["volumes"] = out_volumes
                    out_volume = dict(vol)
                    out_volumes[vol_idx] = out_volume
                    out_volume["ocr_active"] = True
                    out_volume["ocr_pending"] = False
                    out_volume["ocr_progress"] = self._volume_progress(progress)
                    return out
        return data

    def _serve_static(self, start_response: Callable[..., Any], filename: str) -> list[bytes]:
        """Serve static files."""
        if not filename or filename == "/":
            filename = "index.html"

        file_path = (STATIC_DIR / filename).resolve()
        if not is_within_path(file_path, STATIC_DIR):
            return self._error_response(start_response, 403, "Forbidden")

        if not file_path.exists() or not file_path.is_file():
            file_path = STATIC_DIR / "index.html"
            if not file_path.exists():
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

    def _json_response(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        data: dict[str, Any],
    ) -> list[bytes]:
        """Return a JSON response."""
        status_map = {200: "OK", 404: "Not Found", 500: "Internal Server Error"}
        status = f"{status_code} {status_map.get(status_code, 'Error')}"
        body = json.dumps(data).encode("utf-8")
        headers = [
            ("Content-Type", "application/json"),
            ("Content-Length", str(len(body))),
        ]
        start_response(status, headers)
        return [body]

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
