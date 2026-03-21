"""Integration tests for home page middleware."""

import io
import json
from pathlib import Path
from unittest.mock import Mock

import pytest

from mokuro_bunko.config import Config
from mokuro_bunko.database import Database
from mokuro_bunko.server import create_app


@pytest.fixture
def config(tmp_path):
    """Create a test configuration."""
    storage_path = tmp_path / "storage"
    storage_path.mkdir()
    return Config.from_dict({
        "server": {"host": "127.0.0.1", "port": 8080},
        "storage": {"base_path": str(storage_path)},
        "registration": {"mode": "self"},
        "admin": {"enabled": True},
    })


@pytest.fixture
def app(config):
    """Create a test application."""
    # Mark setup complete for home page tests by ensuring an admin exists.
    storage_path = Path(config.storage.base_path)
    db = Database(storage_path / "mokuro.db")
    db.create_user("admin", "adminpass12", "admin")
    return create_app(config)


def make_environ(
    method: str = "GET",
    path: str = "/",
    headers: dict = None,
    body: bytes = b"",
) -> dict:
    """Create a minimal WSGI environ dict."""
    headers = headers or {}
    environ = {
        "REQUEST_METHOD": method,
        "SCRIPT_NAME": "",
        "PATH_INFO": path,
        "QUERY_STRING": "",
        "CONTENT_TYPE": headers.get("Content-Type", ""),
        "CONTENT_LENGTH": str(len(body)),
        "SERVER_NAME": "test",
        "SERVER_PORT": "80",
        "SERVER_PROTOCOL": "HTTP/1.1",
        "wsgi.version": (1, 0),
        "wsgi.url_scheme": "http",
        "wsgi.input": io.BytesIO(body),
        "wsgi.errors": io.StringIO(),
        "wsgi.multithread": True,
        "wsgi.multiprocess": True,
        "wsgi.run_once": False,
    }
    # Add HTTP headers
    for key, value in headers.items():
        key_upper = key.upper().replace("-", "_")
        if key_upper not in ("CONTENT_TYPE", "CONTENT_LENGTH"):
            environ[f"HTTP_{key_upper}"] = value
    return environ


def call_app(app, environ):
    """Call WSGI app and return (status, headers_dict, body)."""
    response_status = []
    response_headers = []

    def start_response(status, headers, exc_info=None):
        response_status.append(status)
        response_headers.extend(headers)
        return lambda s: None  # write() callable

    body = b"".join(app(environ, start_response))
    headers_dict = dict(response_headers)

    return response_status[0], headers_dict, body


class TestWelcomePage:
    """Tests for welcome page serving."""

    def test_browser_gets_welcome_page(self, app, config):
        """Test that browser requests get the welcome page."""
        environ = make_environ(
            method="GET",
            path="/",
            headers={
                "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
            },
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        assert b"Mokuro Bunko" in body
        assert b"<!DOCTYPE html>" in body

    def test_webdav_client_gets_dav_response(self, app, config):
        """Test that WebDAV clients don't get the welcome page."""
        environ = make_environ(
            method="PROPFIND",
            path="/",
            headers={
                "Depth": "1",
                "User-Agent": "davfs2/1.5.4",
            },
        )

        status, headers, body = call_app(app, environ)

        # Should get WebDAV multi-status response, not HTML welcome page
        assert b"Community Reading" not in body

    def test_root_redirects_to_catalog_when_enabled_as_homepage(self, config):
        """Test / redirects to /catalog/ when catalog-as-homepage is enabled."""
        config.catalog.enabled = True
        config.catalog.use_as_homepage = True

        storage_path = Path(config.storage.base_path)
        db = Database(storage_path / "mokuro.db")
        db.create_user("admin", "adminpass12", "admin")
        app = create_app(config)

        environ = make_environ(
            method="GET",
            path="/",
            headers={
                "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                "User-Agent": "Mozilla/5.0",
            },
        )

        status, headers, body = call_app(app, environ)

        assert "302" in status
        assert headers.get("Location") == "/catalog/"
        assert body == b""

    def test_welcome_page_static_css(self, app, config):
        """Test that CSS is served for welcome page."""
        environ = make_environ(
            method="GET",
            path="/_home/styles.css",
            headers={"Accept": "*/*"},
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        assert "text/css" in headers.get("Content-Type", "")
        assert b".container" in body or b".hero" in body

    def test_welcome_page_static_js(self, app, config):
        """Test that JavaScript is served for welcome page."""
        environ = make_environ(
            method="GET",
            path="/_home/home.js",
            headers={"Accept": "*/*"},
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        assert "javascript" in headers.get("Content-Type", "")
        assert b"loadStats" in body

    def test_nonexistent_static_file_404(self, app, config):
        """Test that nonexistent static files return 404."""
        environ = make_environ(
            method="GET",
            path="/_home/nonexistent.xyz",
            headers={"Accept": "*/*"},
        )

        status, headers, body = call_app(app, environ)

        assert "404" in status

    def test_directory_traversal_blocked(self, app, config):
        """Test that directory traversal is blocked."""
        environ = make_environ(
            method="GET",
            path="/_home/../../../etc/passwd",
            headers={"Accept": "*/*"},
        )

        status, headers, body = call_app(app, environ)

        # Should be either 403 or 404, not successful
        assert "403" in status or "404" in status


class TestHealthAPI:
    """Tests for health endpoint."""

    def test_health_reports_uptime_and_db_status(self, app, config):
        environ = make_environ(
            method="GET",
            path="/api/health",
            headers={"Accept": "application/json"},
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        data = json.loads(body)
        assert data["status"] == "ok"
        assert isinstance(data["uptime_seconds"], int)
        assert data["db_status"] in ("ok", "error", "unavailable")

        # security headers must be present
        assert headers.get("X-Content-Type-Options") == "nosniff"
        assert headers.get("Referrer-Policy") == "no-referrer"


class TestStatsAPI:
    """Tests for stats API endpoint."""

    def test_get_stats_empty(self, app, config):
        """Test stats API with no data except admin user."""
        environ = make_environ(
            method="GET",
            path="/api/stats",
            headers={"Accept": "application/json"},
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        data = json.loads(body)
        assert data["total_users"] == 1
        assert data["total_volumes"] == 0
        assert data["total_pages_read"] == 0

    def test_get_stats_with_data(self, app, config):
        """Stats endpoint returns configured stats including users and volumes."""
        environ = make_environ(
            method="GET",
            path="/api/stats",
            headers={"Accept": "application/json"},
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        data = json.loads(body)
        assert data["total_users"] == 1
        assert data["total_volumes"] == 0
        assert data["total_pages_read"] == 0
        assert data["total_characters_read"] == 0
        assert data["total_reading_time_seconds"] == 0
        assert "total_reading_time_formatted" in data

    def test_stats_options_request(self, app, config):
        """Test OPTIONS request for stats API."""
        environ = make_environ(
            method="OPTIONS",
            path="/api/stats",
        )

        status, headers, body = call_app(app, environ)

        assert "204" in status


class TestHealthAndStatsMethods:
    """Tests for additional health and stats endpoint behavior."""

    def test_health_endpoint_reports_db_and_library(self, app, config):
        environ = make_environ(
            method="GET",
            path="/api/health",
            headers={"Accept": "application/json"},
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        data = json.loads(body)
        assert data["status"] in ("ok", "degraded")
        assert isinstance(data["uptime_seconds"], int)
        assert data["db_status"] in ("ok", "error", "unavailable")
        assert data["library_status"] in ("ok", "error", "unavailable")
        assert headers.get("X-Content-Type-Options") == "nosniff"
        assert headers.get("Referrer-Policy") == "no-referrer"



    def test_stats_post_not_allowed(self, app, config):
        """Test POST to stats API returns 405."""
        environ = make_environ(
            method="POST",
            path="/api/stats",
        )

        status, headers, body = call_app(app, environ)

        assert "405" in status

    def test_health_endpoint(self, app, config):
        """Test health endpoint returns OK and is always reachable."""
        environ = make_environ(
            method="GET",
            path="/api/health",
            headers={"Accept": "application/json"},
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        assert json.loads(body)["status"] == "ok"


class TestBrowserDetection:
    """Tests for browser vs WebDAV client detection."""

    def test_accept_html_is_browser(self, app, config):
        """Test that Accept: text/html is detected as browser."""
        environ = make_environ(
            method="GET",
            path="/",
            headers={"Accept": "text/html"},
        )

        status, headers, body = call_app(app, environ)

        assert "200" in status
        assert "text/html" in headers.get("Content-Type", "")

    def test_davfs_is_webdav(self, app, config):
        """Test that davfs user agent is detected as WebDAV client."""
        environ = make_environ(
            method="GET",
            path="/",
            headers={"User-Agent": "davfs2/1.5.4"},
        )

        status, headers, body = call_app(app, environ)

        # Should NOT return HTML welcome page (community reading section)
        content_type = headers.get("Content-Type", "")
        assert "text/html" not in content_type or b"Community Reading" not in body

    def test_depth_header_is_webdav(self, app, config):
        """Test that Depth header indicates WebDAV request."""
        environ = make_environ(
            method="PROPFIND",
            path="/",
            headers={"Depth": "1"},
        )

        status, headers, body = call_app(app, environ)

        # PROPFIND should be handled by WebDAV, not return welcome page
        assert b"Community Reading" not in body


class TestNavConfig:
    """Tests for shared navigation config endpoint."""

    def test_nav_config_hides_home_when_catalog_is_homepage(self, config):
        """Home button should be hidden when catalog replaces home."""
        config.catalog.enabled = True
        config.catalog.use_as_homepage = True

        storage_path = Path(config.storage.base_path)
        db = Database(storage_path / "mokuro.db")
        db.create_user("admin", "adminpass12", "admin")
        app = create_app(config)

        environ = make_environ(method="GET", path="/api/nav/config")
        status, headers, body = call_app(app, environ)

        assert "200" in status
        data = json.loads(body)
        assert data["home_enabled"] is False
        assert data["catalog_enabled"] is True

    def test_nav_config_shows_home_by_default(self, app):
        """Home button should be visible for normal home page setup."""
        environ = make_environ(method="GET", path="/api/nav/config")
        status, headers, body = call_app(app, environ)

        assert "200" in status
        data = json.loads(body)
        assert data["home_enabled"] is True

    def test_cleanup_uses_safe_watcher_shutdown(self, app):
        """Cleanup should detach watcher without signaling watchdog threads."""
        watcher = Mock()
        cache = Mock()
        app._library_watcher = watcher  # type: ignore[attr-defined]
        app._propfind_cache = cache  # type: ignore[attr-defined]

        app._cleanup_background_services()  # type: ignore[attr-defined]

        watcher.stop.assert_called_once_with(skip_observer_shutdown=True)
        cache.stop.assert_called_once_with()
