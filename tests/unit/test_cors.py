"""Tests for CORS middleware."""

from __future__ import annotations

import pytest

from mokuro_bunko.config import CorsConfig
from mokuro_bunko.middleware.cors import (
    ALLOWED_HEADERS,
    EXPOSED_HEADERS,
    WEBDAV_METHODS,
    compile_origin_pattern,
    get_cors_headers,
    is_origin_allowed,
)


class TestCompileOriginPattern:
    """Tests for compile_origin_pattern."""

    def test_exact_match(self) -> None:
        """Test exact origin pattern."""
        pattern = compile_origin_pattern("https://example.com")
        assert pattern.match("https://example.com")
        assert not pattern.match("https://example.com:8080")
        assert not pattern.match("https://other.com")

    def test_wildcard_port(self) -> None:
        """Test wildcard port pattern."""
        pattern = compile_origin_pattern("http://localhost:*")
        assert pattern.match("http://localhost:3000")
        assert pattern.match("http://localhost:8080")
        assert pattern.match("http://localhost:80")
        assert not pattern.match("http://localhost")
        assert not pattern.match("http://other:3000")

    def test_https_with_wildcard_port(self) -> None:
        """Test HTTPS with wildcard port."""
        pattern = compile_origin_pattern("https://localhost:*")
        assert pattern.match("https://localhost:443")
        assert pattern.match("https://localhost:8443")
        assert not pattern.match("http://localhost:8080")

    def test_ip_with_wildcard_port(self) -> None:
        """Test IP address with wildcard port."""
        pattern = compile_origin_pattern("http://127.0.0.1:*")
        assert pattern.match("http://127.0.0.1:3000")
        assert pattern.match("http://127.0.0.1:8080")
        assert not pattern.match("http://192.168.1.1:3000")


class TestIsOriginAllowed:
    """Tests for is_origin_allowed."""

    def test_exact_match_allowed(self) -> None:
        """Test exact origin match."""
        origins = ["https://example.com", "https://other.com"]
        assert is_origin_allowed("https://example.com", origins)
        assert is_origin_allowed("https://other.com", origins)
        assert not is_origin_allowed("https://notallowed.com", origins)

    def test_wildcard_port_allowed(self) -> None:
        """Test wildcard port matching."""
        origins = ["http://localhost:*"]
        assert is_origin_allowed("http://localhost:3000", origins)
        assert is_origin_allowed("http://localhost:8080", origins)
        assert not is_origin_allowed("http://localhost", origins)
        assert not is_origin_allowed("http://other:3000", origins)

    def test_mixed_patterns(self) -> None:
        """Test mix of exact and wildcard patterns."""
        origins = [
            "https://example.com",
            "http://localhost:*",
            "http://127.0.0.1:*",
        ]
        assert is_origin_allowed("https://example.com", origins)
        assert is_origin_allowed("http://localhost:3000", origins)
        assert is_origin_allowed("http://127.0.0.1:8080", origins)
        assert not is_origin_allowed("https://other.com", origins)

    def test_empty_allowed_origins(self) -> None:
        """Test empty allowed origins list."""
        assert not is_origin_allowed("https://example.com", [])

    def test_default_mokuro_origins(self) -> None:
        """Test default Mokuro reader origins."""
        origins = [
            "https://reader.mokuro.app",
            "http://localhost:5173",
            "http://localhost:*",
            "http://127.0.0.1:*",
        ]
        assert is_origin_allowed("https://reader.mokuro.app", origins)
        assert is_origin_allowed("http://localhost:5173", origins)
        assert is_origin_allowed("http://localhost:5173", origins)
        assert is_origin_allowed("http://127.0.0.1:3000", origins)


class TestGetCorsHeaders:
    """Tests for get_cors_headers."""

    @pytest.fixture
    def default_config(self) -> CorsConfig:
        """Create default CORS config."""
        return CorsConfig()

    @pytest.fixture
    def custom_config(self) -> CorsConfig:
        """Create custom CORS config."""
        return CorsConfig(
            enabled=True,
            allowed_origins=["https://example.com", "http://localhost:*"],
            allow_credentials=True,
        )

    def test_disabled_cors_returns_empty(self) -> None:
        """Test disabled CORS returns no headers."""
        config = CorsConfig(enabled=False)
        headers = get_cors_headers("https://example.com", config)
        assert headers == []

    def test_no_origin_returns_empty(self, default_config: CorsConfig) -> None:
        """Test missing origin returns no headers."""
        headers = get_cors_headers("", default_config)
        assert headers == []

    def test_disallowed_origin_returns_empty(self, custom_config: CorsConfig) -> None:
        """Test disallowed origin returns no headers."""
        headers = get_cors_headers("https://evil.com", custom_config)
        assert headers == []

    def test_allowed_origin_returns_headers(self, custom_config: CorsConfig) -> None:
        """Test allowed origin returns CORS headers."""
        headers = get_cors_headers("https://example.com", custom_config)
        header_dict = dict(headers)

        assert header_dict["Access-Control-Allow-Origin"] == "https://example.com"
        assert header_dict["Access-Control-Allow-Credentials"] == "true"
        assert header_dict["Vary"] == "Origin"

    def test_wildcard_origin_returns_headers(self, custom_config: CorsConfig) -> None:
        """Test wildcard port origin returns CORS headers."""
        headers = get_cors_headers("http://localhost:3000", custom_config)
        header_dict = dict(headers)

        assert header_dict["Access-Control-Allow-Origin"] == "http://localhost:3000"

    def test_preflight_includes_methods(self, custom_config: CorsConfig) -> None:
        """Test preflight response includes allowed methods."""
        headers = get_cors_headers(
            "https://example.com",
            custom_config,
            is_preflight=True,
        )
        header_dict = dict(headers)

        methods = header_dict["Access-Control-Allow-Methods"]
        for method in WEBDAV_METHODS:
            assert method in methods

    def test_preflight_includes_headers(self, custom_config: CorsConfig) -> None:
        """Test preflight response includes allowed headers."""
        headers = get_cors_headers(
            "https://example.com",
            custom_config,
            is_preflight=True,
        )
        header_dict = dict(headers)

        allowed = header_dict["Access-Control-Allow-Headers"]
        for header in ALLOWED_HEADERS:
            assert header in allowed

    def test_preflight_includes_max_age(self, custom_config: CorsConfig) -> None:
        """Test preflight response includes max age."""
        headers = get_cors_headers(
            "https://example.com",
            custom_config,
            is_preflight=True,
        )
        header_dict = dict(headers)

        assert "Access-Control-Max-Age" in header_dict
        assert int(header_dict["Access-Control-Max-Age"]) > 0

    def test_non_preflight_exposes_headers(self, custom_config: CorsConfig) -> None:
        """Test non-preflight response exposes headers."""
        headers = get_cors_headers(
            "https://example.com",
            custom_config,
            is_preflight=False,
        )
        header_dict = dict(headers)

        exposed = header_dict["Access-Control-Expose-Headers"]
        for header in EXPOSED_HEADERS:
            assert header in exposed

    def test_non_preflight_no_methods(self, custom_config: CorsConfig) -> None:
        """Test non-preflight response doesn't include methods."""
        headers = get_cors_headers(
            "https://example.com",
            custom_config,
            is_preflight=False,
        )
        header_dict = dict(headers)

        assert "Access-Control-Allow-Methods" not in header_dict

    def test_credentials_disabled(self) -> None:
        """Test credentials header not sent when disabled."""
        config = CorsConfig(
            enabled=True,
            allowed_origins=["https://example.com"],
            allow_credentials=False,
        )
        headers = get_cors_headers("https://example.com", config)
        header_dict = dict(headers)

        assert "Access-Control-Allow-Credentials" not in header_dict


class TestWebDAVMethods:
    """Tests for WebDAV methods list."""

    def test_includes_standard_methods(self) -> None:
        """Test standard HTTP methods are included."""
        assert "GET" in WEBDAV_METHODS
        assert "POST" in WEBDAV_METHODS
        assert "PUT" in WEBDAV_METHODS
        assert "DELETE" in WEBDAV_METHODS
        assert "HEAD" in WEBDAV_METHODS
        assert "OPTIONS" in WEBDAV_METHODS

    def test_includes_webdav_methods(self) -> None:
        """Test WebDAV-specific methods are included."""
        assert "PROPFIND" in WEBDAV_METHODS
        assert "PROPPATCH" in WEBDAV_METHODS
        assert "MKCOL" in WEBDAV_METHODS
        assert "COPY" in WEBDAV_METHODS
        assert "MOVE" in WEBDAV_METHODS
        assert "LOCK" in WEBDAV_METHODS
        assert "UNLOCK" in WEBDAV_METHODS


class TestAllowedHeaders:
    """Tests for allowed headers list."""

    def test_includes_auth_header(self) -> None:
        """Test Authorization header is allowed."""
        assert "Authorization" in ALLOWED_HEADERS

    def test_includes_content_headers(self) -> None:
        """Test content headers are allowed."""
        assert "Content-Type" in ALLOWED_HEADERS
        assert "Content-Length" in ALLOWED_HEADERS

    def test_includes_webdav_headers(self) -> None:
        """Test WebDAV-specific headers are allowed."""
        assert "Depth" in ALLOWED_HEADERS
        assert "Destination" in ALLOWED_HEADERS
        assert "Lock-Token" in ALLOWED_HEADERS
        assert "Overwrite" in ALLOWED_HEADERS
        assert "Timeout" in ALLOWED_HEADERS


class TestExposedHeaders:
    """Tests for exposed headers list."""

    def test_includes_common_headers(self) -> None:
        """Test common headers are exposed."""
        assert "Content-Length" in EXPOSED_HEADERS
        assert "Content-Type" in EXPOSED_HEADERS
        assert "ETag" in EXPOSED_HEADERS
        assert "Last-Modified" in EXPOSED_HEADERS

    def test_includes_webdav_headers(self) -> None:
        """Test WebDAV headers are exposed."""
        assert "DAV" in EXPOSED_HEADERS
        assert "Lock-Token" in EXPOSED_HEADERS

    def test_includes_auth_header(self) -> None:
        """Test WWW-Authenticate is exposed."""
        assert "WWW-Authenticate" in EXPOSED_HEADERS
