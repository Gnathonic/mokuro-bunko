"""Integration tests for CORS middleware."""

from __future__ import annotations

import io
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config, CorsConfig, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.middleware.cors import WEBDAV_METHODS
from mokuro_bunko.server import create_app


class WSGITestClient:
    """Simple WSGI test client."""

    def __init__(self, app: Callable[..., Any]) -> None:
        self.app = app

    def request(
        self,
        method: str,
        path: str,
        headers: dict[str, str] | None = None,
        content: bytes | None = None,
    ) -> WSGIResponse:
        """Make a request to the WSGI app."""
        headers = headers or {}

        environ = {
            "REQUEST_METHOD": method,
            "SCRIPT_NAME": "",
            "PATH_INFO": path,
            "QUERY_STRING": "",
            "SERVER_NAME": "localhost",
            "SERVER_PORT": "8080",
            "SERVER_PROTOCOL": "HTTP/1.1",
            "wsgi.version": (1, 0),
            "wsgi.url_scheme": "http",
            "wsgi.input": io.BytesIO(content or b""),
            "wsgi.errors": io.StringIO(),
            "wsgi.multithread": False,
            "wsgi.multiprocess": False,
            "wsgi.run_once": False,
            "CONTENT_LENGTH": str(len(content or b"")),
            "CONTENT_TYPE": headers.get("Content-Type", "application/octet-stream"),
        }

        # Add headers
        for key, value in headers.items():
            key_upper = key.upper().replace("-", "_")
            if key_upper not in ("CONTENT_TYPE", "CONTENT_LENGTH"):
                environ[f"HTTP_{key_upper}"] = value

        response = WSGIResponse()
        result = self.app(environ, response.start_response)

        body_parts = []
        try:
            for chunk in result:
                body_parts.append(chunk)
        finally:
            if hasattr(result, "close"):
                result.close()

        response.content = b"".join(body_parts)
        return response

    def options(
        self, path: str, headers: dict[str, str] | None = None
    ) -> WSGIResponse:
        """Make an OPTIONS request."""
        return self.request("OPTIONS", path, headers)

    def get(
        self, path: str, headers: dict[str, str] | None = None
    ) -> WSGIResponse:
        """Make a GET request."""
        return self.request("GET", path, headers)


class WSGIResponse:
    """WSGI response wrapper."""

    def __init__(self) -> None:
        self.status: str = ""
        self.headers: list[tuple[str, str]] = []
        self.content: bytes = b""

    def start_response(
        self,
        status: str,
        headers: list[tuple[str, str]],
        exc_info: Any = None,
    ) -> Callable[[bytes], None]:
        self.status = status
        self.headers = headers
        return lambda data: None

    @property
    def status_code(self) -> int:
        """Extract status code from status string."""
        return int(self.status.split()[0])

    def get_header(self, name: str) -> str | None:
        """Get header value by name (case-insensitive)."""
        name_lower = name.lower()
        for header_name, value in self.headers:
            if header_name.lower() == name_lower:
                return value
        return None

    def get_all_headers(self, name: str) -> list[str]:
        """Get all header values by name (case-insensitive)."""
        name_lower = name.lower()
        return [value for header_name, value in self.headers if header_name.lower() == name_lower]


@pytest.fixture
def test_storage(temp_dir: Path) -> Path:
    """Create test storage directory."""
    storage = temp_dir / "storage"
    (storage / "library").mkdir(parents=True)
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    return storage


@pytest.fixture
def test_db(test_storage: Path) -> Database:
    """Create test database."""
    return Database(test_storage / "mokuro.db")


@pytest.fixture
def cors_config() -> CorsConfig:
    """Create CORS config for testing."""
    return CorsConfig(
        enabled=True,
        allowed_origins=[
            "https://reader.mokuro.app",
            "http://localhost:*",
            "http://127.0.0.1:*",
        ],
        allow_credentials=True,
    )


@pytest.fixture
def test_config(test_storage: Path, cors_config: CorsConfig) -> Config:
    """Create test configuration with CORS."""
    return Config(
        storage=StorageConfig(base_path=test_storage),
        cors=cors_config,
    )


@pytest.fixture
def app(test_config: Config, test_db: Database) -> Any:
    """Create test application with CORS."""
    from mokuro_bunko.server import create_app
    return create_app(test_config)


@pytest.fixture
def client(app: Any) -> WSGITestClient:
    """Create test client."""
    return WSGITestClient(app)


class TestPreflightRequests:
    """Tests for CORS preflight (OPTIONS) requests."""

    def test_preflight_returns_204(self, client: WSGITestClient) -> None:
        """Test preflight request returns 204."""
        response = client.options(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        assert response.status_code == 204

    def test_preflight_includes_allow_origin(self, client: WSGITestClient) -> None:
        """Test preflight includes Access-Control-Allow-Origin."""
        response = client.options(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        assert response.get_header("Access-Control-Allow-Origin") == "https://reader.mokuro.app"

    def test_preflight_includes_allow_methods(self, client: WSGITestClient) -> None:
        """Test preflight includes Access-Control-Allow-Methods."""
        response = client.options(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        methods = response.get_header("Access-Control-Allow-Methods")
        assert methods is not None
        for method in WEBDAV_METHODS:
            assert method in methods

    def test_preflight_includes_allow_headers(self, client: WSGITestClient) -> None:
        """Test preflight includes Access-Control-Allow-Headers."""
        response = client.options(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        headers = response.get_header("Access-Control-Allow-Headers")
        assert headers is not None
        assert "Authorization" in headers
        assert "Content-Type" in headers
        assert "Depth" in headers

    def test_preflight_includes_credentials(self, client: WSGITestClient) -> None:
        """Test preflight includes Access-Control-Allow-Credentials."""
        response = client.options(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        assert response.get_header("Access-Control-Allow-Credentials") == "true"

    def test_preflight_includes_max_age(self, client: WSGITestClient) -> None:
        """Test preflight includes Access-Control-Max-Age."""
        response = client.options(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        max_age = response.get_header("Access-Control-Max-Age")
        assert max_age is not None
        assert int(max_age) > 0

    def test_preflight_wildcard_port(self, client: WSGITestClient) -> None:
        """Test preflight with wildcard port origin."""
        response = client.options(
            "/mokuro-reader",
            headers={"Origin": "http://localhost:5173"},
        )
        assert response.status_code == 204
        assert response.get_header("Access-Control-Allow-Origin") == "http://localhost:5173"

    def test_preflight_disallowed_origin(self, client: WSGITestClient) -> None:
        """Test preflight with disallowed origin."""
        response = client.options(
            "/mokuro-reader",
            headers={"Origin": "https://evil.com"},
        )
        # Should still return 204, but without CORS headers
        assert response.status_code == 204
        assert response.get_header("Access-Control-Allow-Origin") is None


class TestActualRequests:
    """Tests for actual (non-preflight) requests with CORS."""

    def test_get_includes_cors_headers(self, client: WSGITestClient) -> None:
        """Test GET request includes CORS headers."""
        response = client.get(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        assert response.get_header("Access-Control-Allow-Origin") == "https://reader.mokuro.app"

    def test_get_includes_credentials(self, client: WSGITestClient) -> None:
        """Test GET request includes credentials header."""
        response = client.get(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        assert response.get_header("Access-Control-Allow-Credentials") == "true"

    def test_get_includes_expose_headers(self, client: WSGITestClient) -> None:
        """Test GET request includes exposed headers."""
        response = client.get(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        exposed = response.get_header("Access-Control-Expose-Headers")
        assert exposed is not None
        assert "ETag" in exposed
        assert "Content-Length" in exposed

    def test_get_includes_vary(self, client: WSGITestClient) -> None:
        """Test response includes Vary: Origin header."""
        response = client.get(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        vary = response.get_header("Vary")
        assert vary is not None
        assert "Origin" in vary

    def test_get_no_origin_no_cors(self, client: WSGITestClient) -> None:
        """Test request without Origin has no CORS headers."""
        response = client.get("/mokuro-reader")
        assert response.get_header("Access-Control-Allow-Origin") is None

    def test_get_disallowed_origin_no_cors(self, client: WSGITestClient) -> None:
        """Test request from disallowed origin has no CORS headers."""
        response = client.get(
            "/mokuro-reader",
            headers={"Origin": "https://evil.com"},
        )
        assert response.get_header("Access-Control-Allow-Origin") is None


class TestCorsDisabled:
    """Tests for disabled CORS."""

    @pytest.fixture
    def disabled_config(self, test_storage: Path) -> Config:
        """Create config with CORS disabled."""
        return Config(
            storage=StorageConfig(base_path=test_storage),
            cors=CorsConfig(enabled=False),
        )

    @pytest.fixture
    def disabled_app(self, disabled_config: Config, test_db: Database) -> Any:
        """Create app with CORS disabled."""
        return create_app(disabled_config)

    @pytest.fixture
    def disabled_client(self, disabled_app: Any) -> WSGITestClient:
        """Create client for disabled CORS app."""
        return WSGITestClient(disabled_app)

    def test_disabled_no_cors_headers(self, disabled_client: WSGITestClient) -> None:
        """Test disabled CORS returns no CORS headers."""
        response = disabled_client.get(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        assert response.get_header("Access-Control-Allow-Origin") is None

    def test_disabled_preflight_no_cors(self, disabled_client: WSGITestClient) -> None:
        """Test disabled CORS preflight has no CORS headers."""
        response = disabled_client.options(
            "/mokuro-reader",
            headers={"Origin": "https://reader.mokuro.app"},
        )
        assert response.get_header("Access-Control-Allow-Origin") is None


class TestCredentialsDisabled:
    """Tests for disabled credentials."""

    @pytest.fixture
    def no_creds_config(self, test_storage: Path) -> Config:
        """Create config with credentials disabled."""
        return Config(
            storage=StorageConfig(base_path=test_storage),
            cors=CorsConfig(
                enabled=True,
                allowed_origins=["https://example.com"],
                allow_credentials=False,
            ),
        )

    @pytest.fixture
    def no_creds_app(self, no_creds_config: Config, test_db: Database) -> Any:
        """Create app with credentials disabled."""
        return create_app(no_creds_config)

    @pytest.fixture
    def no_creds_client(self, no_creds_app: Any) -> WSGITestClient:
        """Create client for no-credentials app."""
        return WSGITestClient(no_creds_app)

    def test_no_credentials_header(self, no_creds_client: WSGITestClient) -> None:
        """Test credentials header not sent when disabled."""
        response = no_creds_client.get(
            "/mokuro-reader",
            headers={"Origin": "https://example.com"},
        )
        assert response.get_header("Access-Control-Allow-Origin") == "https://example.com"
        assert response.get_header("Access-Control-Allow-Credentials") is None
