"""Integration test: security headers are applied to live app responses."""

from __future__ import annotations

import io
from collections.abc import Callable
from pathlib import Path
from typing import Any

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.server import create_app


def _get(app: Callable[..., Any], path: str) -> tuple[str, list[tuple[str, str]]]:
    captured: dict[str, Any] = {}

    def start_response(
        status: str, headers: list[tuple[str, str]], exc_info: Any = None
    ) -> Callable[[bytes], None]:
        captured["status"] = status
        captured["headers"] = headers
        return lambda data: None

    environ = {
        "REQUEST_METHOD": "GET",
        "SCRIPT_NAME": "",
        "PATH_INFO": path,
        "QUERY_STRING": "",
        "SERVER_NAME": "localhost",
        "SERVER_PORT": "8080",
        "SERVER_PROTOCOL": "HTTP/1.1",
        "wsgi.version": (1, 0),
        "wsgi.url_scheme": "http",
        "wsgi.input": io.BytesIO(b""),
        "wsgi.errors": io.StringIO(),
        "wsgi.multithread": False,
        "wsgi.multiprocess": False,
        "wsgi.run_once": False,
        "CONTENT_LENGTH": "0",
    }
    body = app(environ, start_response)
    for _ in body:
        pass
    if hasattr(body, "close"):
        body.close()
    return captured["status"], captured["headers"]


def _values(headers: list[tuple[str, str]], name: str) -> list[str]:
    return [v for k, v in headers if k.lower() == name.lower()]


def test_security_headers_present_on_json_api_response(temp_dir: Path) -> None:
    storage = temp_dir / "storage"
    (storage / "library").mkdir(parents=True)
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    app = create_app(Config(storage=StorageConfig(base_path=storage)))

    # /login/api/me returns JSON 200 (authenticated: false) for anonymous requests.
    status, headers = _get(app, "/login/api/me")

    assert status.startswith("200")
    assert _values(headers, "X-Content-Type-Options") == ["nosniff"]
    assert _values(headers, "X-Frame-Options") == ["DENY"]
    assert _values(headers, "Referrer-Policy") == ["no-referrer"]
    assert any("no-store" in v for v in _values(headers, "Cache-Control"))


def test_image_get_carries_cache_control_but_series_json_get_does_not(
    temp_dir: Path,
) -> None:
    """Pinning test for the ledger's binding carry-forward.

    A `.webp` cover GET (behind per-user WebDAV auth, revalidated by wsgidav's
    own ETag/Last-Modified) is allowed to be cached by the browser between
    revalidations. A compiled `series.json` GET must stay revalidated-fresh
    (`no-store`), never picking up the image caching header.
    """
    storage = temp_dir / "storage"
    series_dir = storage / "library" / "Dr Stone"
    series_dir.mkdir(parents=True)
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    (series_dir / "Volume 01.webp").write_bytes(b"fake webp bytes")
    (series_dir / "series.json").write_bytes(b'{"name": "Dr Stone", "volumes": []}')
    app = create_app(Config(storage=StorageConfig(base_path=storage)))

    image_status, image_headers = _get(app, "/mokuro-reader/Dr Stone/Volume 01.webp")
    json_status, json_headers = _get(app, "/mokuro-reader/Dr Stone/series.json")

    assert image_status.startswith("200")
    assert _values(image_headers, "Cache-Control") == ["private, max-age=86400"]

    assert json_status.startswith("200")
    json_cache_control = _values(json_headers, "Cache-Control")
    assert "private, max-age=86400" not in json_cache_control
    assert any("no-store" in v for v in json_cache_control)
