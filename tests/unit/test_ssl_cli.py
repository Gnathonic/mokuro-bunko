"""Tests for SSL CLI commands."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import yaml
from click.testing import CliRunner

from mokuro_bunko.__main__ import cli
from mokuro_bunko.config import Config, save_config


class TestSslEnable:
    """Tests for ssl enable command."""

    def test_enable_auto_cert(self, temp_dir: Path) -> None:
        """Test enabling SSL with auto-cert."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        cert_dir = temp_dir / "certs"
        cert_path = cert_dir / "cert.pem"
        key_path = cert_dir / "key.pem"

        with patch("mokuro_bunko.ssl_cli.get_default_cert_paths", return_value=(cert_path, key_path)):
            runner = CliRunner()
            result = runner.invoke(cli, ["-c", str(config_path), "ssl", "enable", "--auto-cert"])
            assert result.exit_code == 0
            assert "SSL enabled" in result.output

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["ssl"]["enabled"] is True
        assert data["ssl"]["auto_cert"] is True

    def test_enable_with_cert_files(self, temp_dir: Path) -> None:
        """Test enabling SSL with cert files."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        cert_file = temp_dir / "cert.pem"
        key_file = temp_dir / "key.pem"
        cert_file.write_text("cert")
        key_file.write_text("key")

        runner = CliRunner()
        result = runner.invoke(cli, [
            "-c", str(config_path),
            "ssl", "enable",
            "--cert", str(cert_file),
            "--key", str(key_file),
        ])
        assert result.exit_code == 0
        assert "SSL enabled" in result.output

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["ssl"]["enabled"] is True
        assert data["ssl"]["auto_cert"] is False
        assert data["ssl"]["cert_file"] == str(cert_file)

    def test_enable_requires_option(self, temp_dir: Path) -> None:
        """Test that enable requires --auto-cert or --cert/--key."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "ssl", "enable"])
        assert result.exit_code != 0

    def test_enable_cert_without_key(self, temp_dir: Path) -> None:
        """Test that --cert without --key fails."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        cert_file = temp_dir / "cert.pem"
        cert_file.write_text("cert")

        runner = CliRunner()
        result = runner.invoke(cli, [
            "-c", str(config_path),
            "ssl", "enable",
            "--cert", str(cert_file),
        ])
        assert result.exit_code != 0


class TestSslDisable:
    """Tests for ssl disable command."""

    def test_disable(self, temp_dir: Path) -> None:
        """Test disabling SSL."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "ssl", "disable"])
        assert result.exit_code == 0
        assert "SSL disabled" in result.output

        with open(config_path) as f:
            data = yaml.safe_load(f)
        assert data["ssl"]["enabled"] is False


class TestSslStatus:
    """Tests for ssl status command."""

    def test_status_disabled(self, temp_dir: Path) -> None:
        """Test status when SSL is disabled."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        runner = CliRunner()
        result = runner.invoke(cli, ["-c", str(config_path), "ssl", "status"])
        assert result.exit_code == 0
        assert "disabled" in result.output

    def test_status_enabled_no_cert_file(self, temp_dir: Path) -> None:
        """Test status when SSL is enabled but cert doesn't exist yet."""
        config_path = temp_dir / "config.yaml"
        config_path.write_text(
            "ssl:\n  enabled: true\n  auto_cert: true\n  cert_file: ''\n  key_file: ''\n"
            "server:\n  host: '0.0.0.0'\n  port: 8080\n"
        )

        cert_path = temp_dir / "nonexistent" / "cert.pem"
        with patch("mokuro_bunko.ssl_cli.get_default_cert_paths", return_value=(cert_path, temp_dir / "key.pem")):
            runner = CliRunner()
            result = runner.invoke(cli, ["-c", str(config_path), "ssl", "status"])
            assert result.exit_code == 0
            assert "enabled" in result.output
            assert "not found" in result.output


class TestSslGenerate:
    """Tests for ssl generate command."""

    def test_generate(self, temp_dir: Path) -> None:
        """Test generating a self-signed certificate."""
        cert_path = temp_dir / "cert.pem"
        key_path = temp_dir / "key.pem"

        with patch("mokuro_bunko.ssl_cli.get_default_cert_paths", return_value=(cert_path, key_path)):
            runner = CliRunner()
            result = runner.invoke(cli, ["ssl", "generate"])
            assert result.exit_code == 0
            assert cert_path.exists()
            assert key_path.exists()
            assert "Certificate:" in result.output

    def test_generate_custom_hostname(self, temp_dir: Path) -> None:
        """Test generating with custom hostname."""
        cert_path = temp_dir / "cert.pem"
        key_path = temp_dir / "key.pem"

        with patch("mokuro_bunko.ssl_cli.get_default_cert_paths", return_value=(cert_path, key_path)):
            runner = CliRunner()
            result = runner.invoke(cli, ["ssl", "generate", "--hostname", "myhost.local", "--days", "30"])
            assert result.exit_code == 0
            assert cert_path.exists()

    def test_generate_overwrite_confirm(self, temp_dir: Path) -> None:
        """Test overwrite confirmation when cert exists."""
        cert_path = temp_dir / "cert.pem"
        key_path = temp_dir / "key.pem"
        cert_path.write_text("existing")

        with patch("mokuro_bunko.ssl_cli.get_default_cert_paths", return_value=(cert_path, key_path)):
            runner = CliRunner()
            # Say no to overwrite
            result = runner.invoke(cli, ["ssl", "generate"], input="n\n")
            assert result.exit_code == 0
            assert cert_path.read_text() == "existing"
