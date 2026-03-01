"""Tests for config CLI commands."""

from __future__ import annotations

from pathlib import Path

import yaml
from click.testing import CliRunner

from mokuro_bunko.__main__ import cli
from mokuro_bunko.config import Config, ServerConfig, save_config


class TestConfigShow:
    """Tests for config show command."""

    def test_show_default_config(self, temp_dir: Path) -> None:
        """Test showing default config."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "show"])
        assert result.exit_code == 0
        data = yaml.safe_load(result.output)
        assert data["server"]["port"] == 8080
        assert data["server"]["host"] == "0.0.0.0"

    def test_show_custom_config(self, temp_dir: Path) -> None:
        """Test showing custom config."""
        config_path = temp_dir / "config.yaml"
        config = Config(server=ServerConfig(port=9090))
        save_config(config, config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "show"])
        assert result.exit_code == 0
        data = yaml.safe_load(result.output)
        assert data["server"]["port"] == 9090


class TestConfigSet:
    """Tests for config set command."""

    def test_set_port(self, temp_dir: Path) -> None:
        """Test setting server port."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "set", "server.port", "9090"])
        assert result.exit_code == 0
        assert "Set server.port = 9090" in result.output

        # Verify it was saved
        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["server"]["port"] == 9090

    def test_set_string_value(self, temp_dir: Path) -> None:
        """Test setting a string value."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "set", "registration.mode", "invite"])
        assert result.exit_code == 0

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["registration"]["mode"] == "invite"

    def test_set_bool_value(self, temp_dir: Path) -> None:
        """Test setting a boolean value."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "set", "cors.enabled", "false"])
        assert result.exit_code == 0

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["cors"]["enabled"] is False

    def test_set_invalid_key(self, temp_dir: Path) -> None:
        """Test setting an invalid key."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "set", "invalid.key", "value"])
        assert result.exit_code != 0
        assert "Error" in result.output or "error" in (result.output + (result.stderr or "")).lower()

    def test_set_invalid_int(self, temp_dir: Path) -> None:
        """Test setting an invalid integer value."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "set", "server.port", "notanumber"])
        assert result.exit_code != 0


class TestConfigPath:
    """Tests for config path command."""

    def test_shows_paths(self) -> None:
        """Test that path command shows file locations."""
        runner = CliRunner()
        result = runner.invoke(cli, ["config", "path"])
        assert result.exit_code == 0
        assert "Config file:" in result.output
        assert "Storage dir:" in result.output


class TestConfigInit:
    """Tests for config init command."""

    def test_creates_config(self, temp_dir: Path) -> None:
        """Test creating a new config file."""
        config_path = temp_dir / "config.yaml"

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "init"])
        assert result.exit_code == 0
        assert config_path.exists()
        assert "Created config file" in result.output

    def test_errors_if_exists(self, temp_dir: Path) -> None:
        """Test error when config already exists."""
        config_path = temp_dir / "config.yaml"
        config_path.write_text("existing: true")

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "init"])
        assert result.exit_code != 0

    def test_force_overwrite(self, temp_dir: Path) -> None:
        """Test force overwriting existing config."""
        config_path = temp_dir / "config.yaml"
        config_path.write_text("existing: true")

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "init", "--force"])
        assert result.exit_code == 0
        assert "Created config file" in result.output

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert "server" in data


class TestCorsAdd:
    """Tests for cors-add command."""

    def test_add_origin(self, temp_dir: Path) -> None:
        """Test adding a CORS origin."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "cors-add", "https://example.com"])
        assert result.exit_code == 0
        assert "Added CORS origin" in result.output

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert "https://example.com" in data["cors"]["allowed_origins"]

    def test_add_duplicate_origin(self, temp_dir: Path) -> None:
        """Test adding a duplicate origin is a no-op."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "cors-add", "https://reader.mokuro.app"])
        assert result.exit_code == 0
        assert "already allowed" in result.output


class TestCorsRemove:
    """Tests for cors-remove command."""

    def test_remove_origin(self, temp_dir: Path) -> None:
        """Test removing a CORS origin."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "cors-remove", "http://localhost:5173"])
        assert result.exit_code == 0
        assert "Removed CORS origin" in result.output

    def test_remove_nonexistent_origin(self, temp_dir: Path) -> None:
        """Test removing a nonexistent origin."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "config", "cors-remove", "https://nonexistent.com"])
        assert result.exit_code != 0
