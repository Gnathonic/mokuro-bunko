"""Tests for setup wizard CLI."""

from __future__ import annotations

from pathlib import Path

import yaml
from click.testing import CliRunner

from mokuro_bunko.__main__ import cli
from mokuro_bunko.config import Config, save_config


class TestSetupWizard:
    """Tests for setup command."""

    def test_setup_defaults(self, temp_dir: Path) -> None:
        """Test setup with all defaults accepted."""
        config_path = temp_dir / "config.yaml"

        runner = CliRunner()
        # Accept defaults for all prompts:
        # storage path (enter=default), port (enter=default), SSL? (n),
        # create admin? (n), registration mode (enter=default), custom CORS? (n),
        # save? (y)
        result = runner.invoke(
            cli,
            ["-c", str(config_path), "setup"],
            input="\n\nn\nn\n\n\nn\ny\n",
        )
        assert result.exit_code == 0
        assert config_path.exists()
        assert "Setup complete" in result.output

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["server"]["port"] == 8080

    def test_setup_custom_port(self, temp_dir: Path) -> None:
        """Test setup with custom port."""
        config_path = temp_dir / "config.yaml"

        runner = CliRunner()
        # storage (default), port (9090), SSL (n), admin (n),
        # registration (default), CORS (n), save (y)
        result = runner.invoke(
            cli,
            ["-c", str(config_path), "setup"],
            input="\n9090\nn\nn\n\n\nn\ny\n",
        )
        assert result.exit_code == 0

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["server"]["port"] == 9090

    def test_setup_skip_if_exists(self, temp_dir: Path) -> None:
        """Test --skip-if-exists with existing config."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "setup", "--skip-if-exists"])
        assert result.exit_code == 0
        assert "skipping setup" in result.output

    def test_setup_cancel_overwrite(self, temp_dir: Path) -> None:
        """Test cancelling when config exists."""
        config_path = temp_dir / "config.yaml"
        config_path.write_text("existing: true")

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "setup"], input="n\n")
        assert result.exit_code == 0
        # Config should be unchanged
        assert config_path.read_text() == "existing: true"

    def test_setup_cancel_save(self, temp_dir: Path) -> None:
        """Test cancelling at save confirmation."""
        config_path = temp_dir / "config.yaml"

        runner = CliRunner()
        # storage (default), port (default), SSL (n), admin (n),
        # registration (default), CORS (n), save (n)
        result = runner.invoke(
            cli,
            ["-c", str(config_path), "setup"],
            input="\n\nn\nn\n\n\nn\nn\n",
        )
        assert result.exit_code == 0
        assert "cancelled" in result.output
        assert not config_path.exists()

    def test_setup_with_invite_mode(self, temp_dir: Path) -> None:
        """Test setup with invite registration mode."""
        config_path = temp_dir / "config.yaml"

        runner = CliRunner()
        # storage (default), port (default), SSL (n), admin (n),
        # registration (invite), CORS (n), save (y)
        result = runner.invoke(
            cli,
            ["-c", str(config_path), "setup"],
            input="\n\nn\nn\ninvite\n\nn\ny\n",
        )
        assert result.exit_code == 0

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["registration"]["mode"] == "invite"

    def test_setup_with_cors_origins(self, temp_dir: Path) -> None:
        """Test setup with custom CORS origins."""
        config_path = temp_dir / "config.yaml"

        runner = CliRunner()
        # storage (default), port (default), SSL (n), admin (n),
        # registration (default), CORS (y), origin, empty to finish, save (y)
        result = runner.invoke(
            cli,
            ["-c", str(config_path), "setup"],
            input="\n\nn\nn\n\n\ny\nhttps://custom.example.com\n\ny\n",
        )
        assert result.exit_code == 0

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert "https://custom.example.com" in data["cors"]["allowed_origins"]
