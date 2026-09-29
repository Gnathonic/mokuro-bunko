"""Tests for configuration loading and validation."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from mokuro_bunko.config import (
    AdminConfig,
    Config,
    CorsConfig,
    DatabaseConfig,
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

# The row every `ocr.generations` list needs: exactly one enabled generation
# must be the primary one, or the list is refused.
_MOKURO_ROW = {"name": "mokuro", "engine": "mokuro", "primary": True}


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


class TestDatabaseConfig:
    """Tests for DatabaseConfig."""

    def test_defaults(self) -> None:
        """Test default values."""
        config = DatabaseConfig()
        assert config.busy_timeout_ms == 5000
        assert config.lock_retries == 5
        assert config.retry_initial_delay_seconds == 0.05

    def test_invalid_busy_timeout(self) -> None:
        with pytest.raises(ValueError, match="busy timeout"):
            DatabaseConfig(busy_timeout_ms=50)

    def test_invalid_lock_retries(self) -> None:
        with pytest.raises(ValueError, match="lock retries"):
            DatabaseConfig(lock_retries=0)

    def test_invalid_retry_delay(self) -> None:
        with pytest.raises(ValueError, match="retry initial delay"):
            DatabaseConfig(retry_initial_delay_seconds=0)

    def test_from_dict_and_to_dict_round_trip(self) -> None:
        data = {
            "storage": {"base_path": "/tmp/x"},
            "database": {
                "busy_timeout_ms": 2500,
                "lock_retries": 3,
                "retry_initial_delay_seconds": 0.1,
            },
        }
        config = Config.from_dict(data)
        assert config.database.busy_timeout_ms == 2500
        assert config.database.lock_retries == 3
        assert config.database.retry_initial_delay_seconds == 0.1

        out = config.to_dict()
        assert out["database"] == data["database"]


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


class TestOcrGenerationsSurface:
    """``ocr.generations`` is the config surface the OCR recipe now has.

    The patch budget used to be a scalar ``ocr.*`` key of its own; it is a
    field of a generation row now. What the config layer owes it did not
    change, so the four things the retired ``ocr.patch_budget`` tests pinned
    are pinned here instead: a default, validation, a round trip through the
    file AND the environment, and ``config set``. (The values themselves --
    why the patch budget defaults to 512 and not the model card's 384 --
    belong to the registry and are pinned in ``test_ocr_engines.py``.)
    """

    def test_defaults_are_one_primary_mokuro_row(self) -> None:
        row = OcrConfig().generations[0]
        assert (row.name, row.engine, row.primary, row.enabled) == (
            "mokuro",
            "mokuro",
            True,
            True,
        )
        assert row.patch_budget == 512

        stored = Config().to_dict()["ocr"]
        assert stored["generations"] == [row.to_dict()]
        # `concurrency` was silently dropped on save, so a server tuned to
        # several slots fell back to one on its next restart.
        assert stored["concurrency"] == 1

    def test_values_are_validated(self) -> None:
        row = {"name": "nova", "engine": "hayai-nova"}
        parsed = OcrConfig(
            generations=[
                _MOKURO_ROW,
                {**row, "patch_budget": "384 "},
            ]
        ).generations
        # YAML and the environment hand it over as text.
        assert parsed[1].patch_budget == 384
        for bad in ({"patch_budget": 500}, {"engine": "nope"}, {"detector": "magic"}):
            with pytest.raises(ValueError):
                OcrConfig(generations=[_MOKURO_ROW, {**row, **bad}])

    def test_round_trips_through_file_and_env(
        self, temp_config_file: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        temp_config_file.write_text(
            "ocr:\n"
            "  generations:\n"
            "    - {name: mokuro, engine: mokuro, primary: true, patch_budget: 256}\n"
        )
        assert load_config(temp_config_file).ocr.generations[0].patch_budget == 256
        # A list of objects, so the environment carries it as JSON text.
        monkeypatch.setenv(
            "MOKURO_OCR_GENERATIONS",
            json.dumps([{**_MOKURO_ROW, "patch_budget": 384}, {"engine": "hayai-nova"}]),
        )
        rows = load_config(temp_config_file).ocr.generations
        # An unnamed row is seeded from its engine and detector.
        assert [row.name for row in rows] == ["mokuro", "hayai-nova-ppocr-manga"]
        assert rows[0].patch_budget == 384

    def test_set_by_dotted_key(self) -> None:
        from mokuro_bunko.config import set_by_dotted_key

        config = Config()
        set_by_dotted_key(
            config,
            "ocr.generations",
            json.dumps([{**_MOKURO_ROW, "patch_budget": 256}]),
        )
        assert config.ocr.generations[0].patch_budget == 256
        with pytest.raises(ValueError):
            set_by_dotted_key(config, "ocr.generations", json.dumps([{"engine": "nope"}]))
        # Not YAML, not a comma-separated list: unreadable JSON says so.
        with pytest.raises(ValueError):
            set_by_dotted_key(config, "ocr.generations", "mokuro,hayai-nova")


class TestRetiredOcrKeys:
    """A config still on the pre-generations OCR keys is refused at LOAD.

    Not migrated and not ignored: the keys named an engine list and one
    global recipe, and guessing which generation rows that was meant to be
    would silently re-OCR a library under file names nobody chose. Both
    places a server reads them from are checked -- the file and the
    environment -- because a container sets only the second.
    """

    def test_a_config_file_naming_an_old_key_does_not_load(
        self, temp_config_file: Path
    ) -> None:
        temp_config_file.write_text("ocr:\n  engines: [mokuro, hayai-nova]\n  detector: ctd\n")
        with pytest.raises(ValueError) as excinfo:
            load_config(temp_config_file)
        message = str(excinfo.value)
        assert "ocr.engines" in message and "ocr.detector" in message
        assert "ocr.generations" in message

    @pytest.mark.parametrize(
        ("env_key", "value"),
        [
            ("MOKURO_OCR_ENGINES", "mokuro,hayai-nova"),
            ("MOKURO_OCR_DETECTOR", "ctd"),
            ("MOKURO_OCR_PATCH_BUDGET", "256"),
        ],
    )
    def test_an_old_environment_variable_does_not_load(
        self, temp_dir: Path, monkeypatch: pytest.MonkeyPatch, env_key: str, value: str
    ) -> None:
        monkeypatch.setenv(env_key, value)
        with pytest.raises(ValueError) as excinfo:
            load_config(temp_dir / "nonexistent.yaml")
        message = str(excinfo.value)
        assert env_key in message
        assert "MOKURO_OCR_GENERATIONS" in message

    def test_the_replacement_variable_is_what_works(
        self, temp_dir: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("MOKURO_OCR_GENERATIONS", json.dumps([_MOKURO_ROW]))
        assert load_config(temp_dir / "nonexistent.yaml").ocr.generations[0].name == "mokuro"


class TestRemovedOcrKeys:
    """``ocr.char_map`` named a system that is GONE, not one that moved.

    The retired keys above have somewhere to go -- a generation row -- and
    their message points at it. The character map has nowhere: no
    per-character placement mode produced output worth using, so the whole
    system was deleted and the only fix is to delete the key. Pointing the
    reader at ``ocr.generations`` would send them looking for a row field
    that does not exist either, so the two refusals must not share wording.
    """

    def test_a_config_naming_it_is_refused_as_removed_not_moved(self) -> None:
        with pytest.raises(ValueError) as excinfo:
            Config.from_dict({"ocr": {"char_map": "attn"}})
        message = str(excinfo.value)
        assert "ocr.char_map" in message
        assert "removed" in message and "delete the key" in message
        # Not the retired-key sentence: there is no replacement to name.
        assert "ocr.generations" not in message

    def test_a_config_file_naming_it_does_not_load(self, temp_config_file: Path) -> None:
        temp_config_file.write_text("ocr:\n  char_map: attn\n")
        with pytest.raises(ValueError) as excinfo:
            load_config(temp_config_file)
        assert "ocr.char_map" in str(excinfo.value)

    def test_the_environment_variable_does_not_load(
        self, temp_dir: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # A container is the one place that sets it without a file to edit.
        monkeypatch.setenv("MOKURO_OCR_CHAR_MAP", "attn")
        with pytest.raises(ValueError) as excinfo:
            load_config(temp_dir / "nonexistent.yaml")
        message = str(excinfo.value)
        assert "MOKURO_OCR_CHAR_MAP" in message
        assert "removed" in message and "delete the key" in message
        assert "MOKURO_OCR_GENERATIONS" not in message

    def test_it_is_still_named_beside_a_retired_key(self) -> None:
        # Both checks run at load. If the retired list won, the user would be
        # told to move a key that has nowhere to move to, and the config would
        # keep failing after they did what the message said.
        with pytest.raises(ValueError) as excinfo:
            Config.from_dict({"ocr": {"char_map": "attn", "engines": ["mokuro"]}})
        assert "ocr.char_map" in str(excinfo.value)


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
