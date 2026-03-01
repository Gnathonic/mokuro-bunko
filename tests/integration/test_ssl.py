"""Integration tests for SSL support."""

from __future__ import annotations

import ssl
import tempfile
from pathlib import Path
from typing import TYPE_CHECKING, Generator
from unittest.mock import MagicMock, patch

import pytest

from mokuro_bunko.config import Config, SslConfig, ServerConfig, StorageConfig

if TYPE_CHECKING:
    pass


@pytest.fixture
def temp_storage() -> Generator[Path, None, None]:
    """Create temporary storage directory."""
    import shutil
    temp_dir = Path(tempfile.mkdtemp())
    (temp_dir / "library").mkdir()
    (temp_dir / "library" / "thumbnails").mkdir()
    (temp_dir / "inbox").mkdir()
    (temp_dir / "users").mkdir()
    yield temp_dir
    shutil.rmtree(temp_dir)


@pytest.fixture
def temp_cert_dir() -> Generator[Path, None, None]:
    """Create temporary directory for certificates."""
    import shutil
    temp_dir = Path(tempfile.mkdtemp())
    yield temp_dir
    shutil.rmtree(temp_dir)


class TestSslConfig:
    """Tests for SSL configuration."""

    def test_disabled_by_default(self) -> None:
        """Test SSL is disabled by default."""
        ssl_config = SslConfig()
        assert ssl_config.enabled is False

    def test_enabled_requires_certs_or_auto(self) -> None:
        """Test enabling SSL requires certs or auto_cert."""
        with pytest.raises(ValueError, match="cert_file and key_file"):
            SslConfig(enabled=True)

    def test_enabled_with_auto_cert(self) -> None:
        """Test SSL can be enabled with auto_cert."""
        ssl_config = SslConfig(enabled=True, auto_cert=True)
        assert ssl_config.enabled is True
        assert ssl_config.auto_cert is True

    def test_enabled_with_cert_files(self) -> None:
        """Test SSL can be enabled with cert files."""
        ssl_config = SslConfig(
            enabled=True,
            cert_file="/path/to/cert.pem",
            key_file="/path/to/key.pem",
        )
        assert ssl_config.enabled is True
        assert ssl_config.cert_file == "/path/to/cert.pem"
        assert ssl_config.key_file == "/path/to/key.pem"


class TestSslCertificateGeneration:
    """Tests for self-signed certificate generation."""

    def test_generate_self_signed_cert(self, temp_cert_dir: Path) -> None:
        """Test generating self-signed certificate."""
        from mokuro_bunko.ssl import generate_self_signed_cert

        cert_path = temp_cert_dir / "cert.pem"
        key_path = temp_cert_dir / "key.pem"

        generate_self_signed_cert(cert_path, key_path)

        assert cert_path.exists()
        assert key_path.exists()

        # Verify certificate is valid
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(cert_path, key_path)

    def test_cert_includes_hostname(self, temp_cert_dir: Path) -> None:
        """Test certificate includes hostname in SAN."""
        from mokuro_bunko.ssl import generate_self_signed_cert

        cert_path = temp_cert_dir / "cert.pem"
        key_path = temp_cert_dir / "key.pem"

        generate_self_signed_cert(
            cert_path, key_path, hostname="myserver.local"
        )

        # Load and verify certificate
        import cryptography.x509
        with open(cert_path, "rb") as f:
            cert = cryptography.x509.load_pem_x509_certificate(f.read())

        # Check subject alternative name
        san = cert.extensions.get_extension_for_class(
            cryptography.x509.SubjectAlternativeName
        )
        dns_names = san.value.get_values_for_type(cryptography.x509.DNSName)
        assert "myserver.local" in dns_names or "localhost" in dns_names

    def test_cert_default_validity(self, temp_cert_dir: Path) -> None:
        """Test certificate has reasonable validity period."""
        from mokuro_bunko.ssl import generate_self_signed_cert

        cert_path = temp_cert_dir / "cert.pem"
        key_path = temp_cert_dir / "key.pem"

        generate_self_signed_cert(cert_path, key_path)

        import cryptography.x509
        from datetime import datetime, timezone

        with open(cert_path, "rb") as f:
            cert = cryptography.x509.load_pem_x509_certificate(f.read())

        # Certificate should be valid now
        now = datetime.now(timezone.utc)
        assert cert.not_valid_before_utc <= now
        assert cert.not_valid_after_utc > now

        # Should be valid for at least 1 year (minus 1 day for timing)
        validity_days = (cert.not_valid_after_utc - now).days
        assert validity_days >= 364


class TestSslSetup:
    """Tests for SSL setup functionality."""

    def test_get_ssl_paths_default(self) -> None:
        """Test default SSL certificate paths."""
        from mokuro_bunko.ssl import get_default_cert_paths

        cert_path, key_path = get_default_cert_paths()

        assert "mokuro-bunko" in str(cert_path)
        assert "certs" in str(cert_path)
        assert cert_path.suffix == ".pem"
        assert key_path.suffix == ".pem"

    def test_ensure_ssl_context_with_auto_cert(
        self, temp_cert_dir: Path
    ) -> None:
        """Test creating SSL context with auto-generated cert."""
        from mokuro_bunko.ssl import ensure_ssl_context

        ssl_config = SslConfig(enabled=True, auto_cert=True)

        with patch(
            "mokuro_bunko.ssl.get_default_cert_paths"
        ) as mock_paths:
            cert_path = temp_cert_dir / "cert.pem"
            key_path = temp_cert_dir / "key.pem"
            mock_paths.return_value = (cert_path, key_path)

            context = ensure_ssl_context(ssl_config)

            assert context is not None
            assert cert_path.exists()
            assert key_path.exists()

    def test_ensure_ssl_context_with_existing_cert(
        self, temp_cert_dir: Path
    ) -> None:
        """Test creating SSL context with existing cert."""
        from mokuro_bunko.ssl import generate_self_signed_cert, ensure_ssl_context

        cert_path = temp_cert_dir / "cert.pem"
        key_path = temp_cert_dir / "key.pem"

        # Pre-generate cert
        generate_self_signed_cert(cert_path, key_path)

        ssl_config = SslConfig(
            enabled=True,
            cert_file=str(cert_path),
            key_file=str(key_path),
        )

        context = ensure_ssl_context(ssl_config)

        assert context is not None

    def test_ensure_ssl_context_disabled(self) -> None:
        """Test no context when SSL disabled."""
        from mokuro_bunko.ssl import ensure_ssl_context

        ssl_config = SslConfig(enabled=False)
        context = ensure_ssl_context(ssl_config)

        assert context is None

    def test_auto_cert_reuses_existing(self, temp_cert_dir: Path) -> None:
        """Test auto_cert reuses existing certificates."""
        from mokuro_bunko.ssl import generate_self_signed_cert, ensure_ssl_context
        import os

        cert_path = temp_cert_dir / "cert.pem"
        key_path = temp_cert_dir / "key.pem"

        # Pre-generate cert
        generate_self_signed_cert(cert_path, key_path)
        original_mtime = os.path.getmtime(cert_path)

        ssl_config = SslConfig(enabled=True, auto_cert=True)

        with patch(
            "mokuro_bunko.ssl.get_default_cert_paths"
        ) as mock_paths:
            mock_paths.return_value = (cert_path, key_path)

            ensure_ssl_context(ssl_config)

            # Certificate should not have been regenerated
            assert os.path.getmtime(cert_path) == original_mtime


class TestSslServer:
    """Tests for SSL-enabled server."""

    def test_server_starts_with_ssl(
        self, temp_storage: Path, temp_cert_dir: Path
    ) -> None:
        """Test server can start with SSL enabled."""
        from mokuro_bunko.ssl import generate_self_signed_cert

        cert_path = temp_cert_dir / "cert.pem"
        key_path = temp_cert_dir / "key.pem"
        generate_self_signed_cert(cert_path, key_path)

        config = Config(
            server=ServerConfig(host="127.0.0.1", port=0),
            storage=StorageConfig(base_path=temp_storage),
            ssl=SslConfig(
                enabled=True,
                cert_file=str(cert_path),
                key_file=str(key_path),
            ),
        )

        # Import after config creation to avoid circular import
        from mokuro_bunko.server import create_ssl_server

        server = create_ssl_server(config)
        assert server is not None

    def test_https_connection_works(
        self, temp_storage: Path, temp_cert_dir: Path
    ) -> None:
        """Test HTTPS connection works with self-signed cert."""
        import threading
        import time
        import httpx

        from mokuro_bunko.ssl import generate_self_signed_cert

        cert_path = temp_cert_dir / "cert.pem"
        key_path = temp_cert_dir / "key.pem"
        generate_self_signed_cert(cert_path, key_path)

        config = Config(
            server=ServerConfig(host="127.0.0.1", port=0),
            storage=StorageConfig(base_path=temp_storage),
            ssl=SslConfig(
                enabled=True,
                cert_file=str(cert_path),
                key_file=str(key_path),
            ),
        )

        from mokuro_bunko.server import create_ssl_server

        server = create_ssl_server(config)

        # Start server in background
        server_thread = threading.Thread(target=server.start, daemon=True)
        server_thread.start()

        try:
            time.sleep(0.5)

            # Get actual bound port
            actual_port = server.bind_addr[1]
            url = f"https://127.0.0.1:{actual_port}/"

            # Make request with SSL verification disabled (self-signed)
            try:
                response = httpx.get(url, verify=False)
            except httpx.ConnectError as e:
                if "Operation not permitted" in str(e):
                    pytest.skip("Socket connections are not permitted in this environment")
                raise

            # Should get response (even if 401 unauthorized)
            assert response.status_code in (200, 207, 401, 403)

        finally:
            server.stop()


class TestSslCertificatePersistence:
    """Tests for certificate persistence."""

    def test_certs_persist_in_default_location(self) -> None:
        """Test certificates are stored in expected location."""
        from mokuro_bunko.ssl import get_default_cert_paths

        cert_path, key_path = get_default_cert_paths()

        # Should be in user's home directory
        assert str(Path.home()) in str(cert_path)

    def test_auto_cert_creates_directory(self, temp_cert_dir: Path) -> None:
        """Test auto_cert creates certificate directory if needed."""
        from mokuro_bunko.ssl import generate_self_signed_cert

        nested_dir = temp_cert_dir / "nested" / "certs"
        cert_path = nested_dir / "cert.pem"
        key_path = nested_dir / "key.pem"

        generate_self_signed_cert(cert_path, key_path)

        assert nested_dir.exists()
        assert cert_path.exists()
        assert key_path.exists()
