"""Tests for configuration loading and validation."""

from __future__ import annotations

from pathlib import Path

import pytest

from mokuro_bunko.config import (
    AdminConfig,
    Config,
    CorsConfig,
    OcrConfig,
    RegistrationConfig,
    ServerConfig,
    SslConfig,
    StorageConfig,
    get_default_config_path,
    get_default_storage_path,
    load_config,
    save_config,
)


class TestServerConfig:
    """Tests for ServerConfig."""

    def test_defaults(self) -> None:
        """Test default values."""
        config = ServerConfig()
        assert config.host == "0.0.0.0"
        assert config.port == 8080

    def test_custom_values(self) -> None:
        """Test custom values."""
        config = ServerConfig(host="127.0.0.1", port=9000)
        assert config.host == "127.0.0.1"
        assert config.port == 9000

    def test_port_zero_valid(self) -> None:
        """Test that port 0 is valid (used for auto-assigned ports in tests)."""
        config = ServerConfig(port=0)
        assert config.port == 0

    def test_invalid_port_negative(self) -> None:
        """Test that negative port is invalid."""
        with pytest.raises(ValueError, match="Invalid port"):
            ServerConfig(port=-1)

    def test_invalid_port_too_high(self) -> None:
        """Test that port > 65535 is invalid."""
        with pytest.raises(ValueError, match="Invalid port"):
            ServerConfig(port=65536)


class TestStorageConfig:
    """Tests for StorageConfig."""

    def test_defaults(self) -> None:
        """Test default path is set."""
        config = StorageConfig()
        assert config.base_path is not None
        assert isinstance(config.base_path, Path)

    def test_custom_path(self, temp_dir: Path) -> None:
        """Test custom path."""
        config = StorageConfig(base_path=temp_dir)
        assert config.base_path == temp_dir

    def test_string_path_converted(self) -> None:
        """Test that string paths are converted to Path."""
        config = StorageConfig(base_path="/tmp/test")  # type: ignore[arg-type]
        assert isinstance(config.base_path, Path)
        assert config.base_path == Path("/tmp/test")

    def test_path_expansion(self) -> None:
        """Test that ~ is expanded."""
        config = StorageConfig(base_path=Path("~/test"))
        assert "~" not in str(config.base_path)

    def test_derived_paths(self, temp_dir: Path) -> None:
        """Test derived path properties."""
        config = StorageConfig(base_path=temp_dir)
        assert config.library_path == temp_dir / "library"
        assert config.inbox_path == temp_dir / "inbox"
        assert config.users_path == temp_dir / "users"

    def test_ensure_directories(self, temp_dir: Path) -> None:
        """Test directory creation."""
        config = StorageConfig(base_path=temp_dir / "new_storage")
        config.ensure_directories()
        assert config.library_path.exists()
        assert config.inbox_path.exists()
        assert config.users_path.exists()
        assert (config.library_path / "thumbnails").exists()


class TestRegistrationConfig:
    """Tests for RegistrationConfig."""

    def test_defaults(self) -> None:
        """Test default values."""
        config = RegistrationConfig()
        assert config.mode == "self"
        assert config.default_role == "registered"
        assert config.allow_anonymous_browse is True
        assert config.allow_anonymous_download is True

    def test_all_valid_modes(self) -> None:
        """Test all valid registration modes."""
        for mode in ("disabled", "self", "invite", "approval"):
            config = RegistrationConfig(mode=mode)  # type: ignore[arg-type]
            assert config.mode == mode

    def test_invalid_mode(self) -> None:
        """Test invalid registration mode."""
        with pytest.raises(ValueError, match="Invalid registration mode"):
            RegistrationConfig(mode="invalid")  # type: ignore[arg-type]

    def test_valid_default_roles(self) -> None:
        """Test valid default roles."""
        for role in ("registered", "uploader", "inviter", "editor"):
            config = RegistrationConfig(default_role=role)  # type: ignore[arg-type]
            assert config.default_role == role

    def test_legacy_writer_default_role_migrates(self) -> None:
        """Legacy writer role is normalized to uploader."""
        config = RegistrationConfig(default_role="writer")  # type: ignore[arg-type]
        assert config.default_role == "uploader"

    def test_invalid_default_role_admin(self) -> None:
        """Test that admin is not a valid default role."""
        with pytest.raises(ValueError, match="Invalid default role"):
            RegistrationConfig(default_role="admin")  # type: ignore[arg-type]

    def test_invalid_default_role_anonymous(self) -> None:
        """Test that anonymous is not a valid default role."""
        with pytest.raises(ValueError, match="Invalid default role"):
            RegistrationConfig(default_role="anonymous")  # type: ignore[arg-type]


class TestCorsConfig:
    """Tests for CorsConfig."""

    def test_defaults(self) -> None:
        """Test default values."""
        config = CorsConfig()
        assert config.enabled is True
        assert config.allow_credentials is True
        assert len(config.allowed_origins) > 0

    def test_default_origins(self) -> None:
        """Test default allowed origins."""
        config = CorsConfig()
        assert "https://reader.mokuro.app" in config.allowed_origins
        assert "http://localhost:5173" in config.allowed_origins
        assert "http://localhost:*" in config.allowed_origins
        assert "http://127.0.0.1:*" in config.allowed_origins

    def test_exact_origin_match(self) -> None:
        """Test exact origin matching."""
        config = CorsConfig(allowed_origins=["https://example.com"])
        assert config.is_origin_allowed("https://example.com") is True
        assert config.is_origin_allowed("https://other.com") is False

    def test_wildcard_port_match(self) -> None:
        """Test wildcard port matching."""
        config = CorsConfig(allowed_origins=["http://localhost:*"])
        assert config.is_origin_allowed("http://localhost:3000") is True
        assert config.is_origin_allowed("http://localhost:8080") is True
        assert config.is_origin_allowed("http://localhost:") is False
        assert config.is_origin_allowed("http://other:3000") is False

    def test_disabled_cors(self) -> None:
        """Test disabled CORS."""
        config = CorsConfig(enabled=False)
        assert config.is_origin_allowed("https://example.com") is False


class TestSslConfig:
    """Tests for SslConfig."""

    def test_defaults(self) -> None:
        """Test default values."""
        config = SslConfig()
        assert config.enabled is False
        assert config.auto_cert is False

    def test_enabled_without_certs(self) -> None:
        """Test that enabling SSL without certs raises error."""
        with pytest.raises(ValueError, match="cert_file and key_file"):
            SslConfig(enabled=True)

    def test_enabled_with_auto_cert(self) -> None:
        """Test enabling SSL with auto_cert."""
        config = SslConfig(enabled=True, auto_cert=True)
        assert config.enabled is True
        assert config.auto_cert is True

    def test_enabled_with_cert_files(self) -> None:
        """Test enabling SSL with cert files."""
        config = SslConfig(
            enabled=True,
            cert_file="/path/to/cert.pem",
            key_file="/path/to/key.pem"
        )
        assert config.enabled is True
        assert config.cert_file == "/path/to/cert.pem"


class TestOcrConfig:
    """Tests for OcrConfig."""

    def test_defaults(self) -> None:
        """Test default values."""
        config = OcrConfig()
        assert config.backend == "auto"
        assert config.poll_interval == 30

    def test_all_valid_backends(self) -> None:
        """Test all valid OCR backends."""
        for backend in ("auto", "cuda", "rocm", "cpu", "skip"):
            config = OcrConfig(backend=backend)  # type: ignore[arg-type]
            assert config.backend == backend

    def test_invalid_backend(self) -> None:
        """Test invalid OCR backend."""
        with pytest.raises(ValueError, match="Invalid OCR backend"):
            OcrConfig(backend="invalid")  # type: ignore[arg-type]

    def test_invalid_poll_interval(self) -> None:
        """Test invalid poll interval."""
        with pytest.raises(ValueError, match="Invalid poll interval"):
            OcrConfig(poll_interval=0)


class TestConfig:
    """Tests for main Config class."""

    def test_defaults(self) -> None:
        """Test default configuration."""
        config = Config()
        assert isinstance(config.server, ServerConfig)
        assert isinstance(config.storage, StorageConfig)
        assert isinstance(config.registration, RegistrationConfig)
        assert isinstance(config.cors, CorsConfig)
        assert isinstance(config.ssl, SslConfig)
        assert isinstance(config.admin, AdminConfig)
        assert isinstance(config.ocr, OcrConfig)
        assert config.catalog.use_as_homepage is False

    def test_from_dict(self) -> None:
        """Test creating config from dictionary."""
        data = {
            "server": {"host": "127.0.0.1", "port": 9000},
            "registration": {"mode": "invite"},
        }
        config = Config.from_dict(data)
        assert config.server.host == "127.0.0.1"
        assert config.server.port == 9000
        assert config.registration.mode == "invite"

    def test_from_empty_dict(self) -> None:
        """Test creating config from empty dictionary uses defaults."""
        config = Config.from_dict({})
        assert config.server.host == "0.0.0.0"
        assert config.server.port == 8080

    def test_registration_require_login_migrates_to_explicit_anonymous_access(self) -> None:
        """Legacy require_login maps to explicit browse/download controls."""
        config = Config.from_dict({
            "registration": {
                "mode": "self",
                "default_role": "registered",
                "require_login": True,
            }
        })
        assert config.registration.allow_anonymous_browse is False
        assert config.registration.allow_anonymous_download is False

    def test_registration_explicit_anonymous_access_overrides_legacy_require_login(self) -> None:
        """New keys should take precedence when both old+new keys are present."""
        config = Config.from_dict({
            "registration": {
                "mode": "self",
                "default_role": "registered",
                "require_login": True,
                "allow_anonymous_browse": True,
                "allow_anonymous_download": False,
            }
        })
        assert config.registration.allow_anonymous_browse is True
        assert config.registration.allow_anonymous_download is False

    def test_catalog_use_as_homepage_round_trip(self) -> None:
        """Catalog homepage flag is persisted through dict conversion."""
        config = Config.from_dict({
            "catalog": {
                "enabled": True,
                "reader_url": "https://example.com",
                "use_as_homepage": True,
            }
        })
        assert config.catalog.use_as_homepage is True
        data = config.to_dict()
        assert data["catalog"]["use_as_homepage"] is True


class TestLoadConfig:
    """Tests for load_config function."""

    def test_load_nonexistent_returns_defaults(self, temp_dir: Path) -> None:
        """Test loading nonexistent config returns defaults."""
        config = load_config(temp_dir / "nonexistent.yaml")
        assert config.server.host == "0.0.0.0"
        assert config.server.port == 8080

    def test_load_valid_config(
        self, temp_config_file: Path, sample_config_yaml: str
    ) -> None:
        """Test loading valid config file."""
        temp_config_file.write_text(sample_config_yaml)
        config = load_config(temp_config_file)
        assert config.server.host == "127.0.0.1"
        assert config.server.port == 9090
        assert config.registration.mode == "invite"
        assert config.registration.default_role == "uploader"
        assert config.ocr.backend == "cpu"

    def test_load_partial_config(self, temp_config_file: Path) -> None:
        """Test loading partial config file uses defaults for missing."""
        temp_config_file.write_text("server:\n  port: 9000\n")
        config = load_config(temp_config_file)
        assert config.server.port == 9000
        assert config.server.host == "0.0.0.0"  # Default

    def test_load_empty_file(self, temp_config_file: Path) -> None:
        """Test loading empty config file returns defaults."""
        temp_config_file.write_text("")
        config = load_config(temp_config_file)
        assert config.server.port == 8080

    def test_load_config_applies_env_overrides(
        self, temp_config_file: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Environment variables override file values."""
        temp_config_file.write_text("server:\n  host: 127.0.0.1\n  port: 8080\n")
        monkeypatch.setenv("MOKURO_SERVER_HOST", "0.0.0.0")
        monkeypatch.setenv("MOKURO_SERVER_PORT", "9001")

        config = load_config(temp_config_file)
        assert config.server.host == "0.0.0.0"
        assert config.server.port == 9001

    def test_load_config_applies_env_aliases(
        self, temp_config_file: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Legacy alias env vars are still supported."""
        temp_config_file.write_text("server:\n  host: 127.0.0.1\n  port: 8080\n")
        monkeypatch.setenv("MOKURO_HOST", "127.0.0.2")
        monkeypatch.setenv("MOKURO_PORT", "9100")

        config = load_config(temp_config_file)
        assert config.server.host == "127.0.0.2"
        assert config.server.port == 9100


class TestSaveConfig:
    """Tests for save_config function."""

    def test_save_and_reload(self, temp_config_file: Path) -> None:
        """Test saving and reloading config."""
        config = Config(
            server=ServerConfig(host="127.0.0.1", port=9000),
            registration=RegistrationConfig(mode="invite", default_role="uploader"),
        )
        save_config(config, temp_config_file)

        loaded = load_config(temp_config_file)
        assert loaded.server.host == "127.0.0.1"
        assert loaded.server.port == 9000
        assert loaded.registration.mode == "invite"
        assert loaded.registration.default_role == "uploader"

    def test_save_creates_parent_dirs(self, temp_dir: Path) -> None:
        """Test save creates parent directories."""
        config = Config()
        path = temp_dir / "subdir" / "config.yaml"
        save_config(config, path)
        assert path.exists()


class TestDefaultPaths:
    """Tests for default path functions."""

    def test_default_storage_path_returns_path(self) -> None:
        """Test default storage path returns a Path."""
        path = get_default_storage_path()
        assert isinstance(path, Path)

    def test_default_config_path_returns_path(self) -> None:
        """Test default config path returns a Path."""
        path = get_default_config_path()
        assert isinstance(path, Path)
        assert path.name == "config.yaml"
