"""Tests for tunnel CLI commands."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

from click.testing import CliRunner

from mokuro_bunko.__main__ import cli
from mokuro_bunko.config import Config, save_config


class TestTunnelStatus:
    """Tests for tunnel status command."""

    def test_status_not_installed(self) -> None:
        """Test status when cloudflared is not installed."""
        with patch("mokuro_bunko.tunnel_cli.shutil.which", return_value=None):
            runner = CliRunner()
            result = runner.invoke(cli, ["tunnel", "status"])
            assert result.exit_code == 0
            assert "not installed" in result.output

    def test_status_installed(self) -> None:
        """Test status when cloudflared is installed."""
        with patch("mokuro_bunko.tunnel_cli.shutil.which", return_value="/usr/bin/cloudflared"):
            with patch("mokuro_bunko.tunnel_cli.subprocess.run") as mock_run:
                mock_run.return_value.stdout = "cloudflared version 2024.1.0"
                mock_run.return_value.stderr = ""
                runner = CliRunner()
                result = runner.invoke(cli, ["tunnel", "status"])
                assert result.exit_code == 0
                assert "/usr/bin/cloudflared" in result.output


class TestTunnelCloudflare:
    """Tests for tunnel cloudflare command."""

    def test_cloudflare_not_installed(self, temp_dir: Path) -> None:
        """Test error when cloudflared is not installed."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        with patch("mokuro_bunko.tunnel_cli.shutil.which", return_value=None):
            runner = CliRunner()
            result = runner.invoke(cli, ["-c", str(config_path), "tunnel", "cloudflare"])
            assert result.exit_code != 0
            assert "not installed" in result.output or "not installed" in (result.stderr or "")

    def test_cloudflare_uses_config_port(self, temp_dir: Path) -> None:
        """Test that tunnel uses port from config."""
        from mokuro_bunko.config import ServerConfig

        config_path = temp_dir / "config.yaml"
        config = Config(server=ServerConfig(port=9090))
        save_config(config, config_path)

        with patch("mokuro_bunko.tunnel_cli.shutil.which", return_value="/usr/bin/cloudflared"):
            with patch("mokuro_bunko.tunnel_cli.subprocess.Popen") as mock_popen:
                # Simulate the process ending immediately
                mock_process = mock_popen.return_value
                mock_process.stderr.readline.return_value = ""
                mock_process.wait.return_value = 0

                runner = CliRunner()
                result = runner.invoke(cli, ["-c", str(config_path), "tunnel", "cloudflare"])
                assert result.exit_code == 0
                # Verify popen was called with the right URL
                call_args = mock_popen.call_args
                cmd = call_args[0][0]
                assert "http://localhost:9090" in cmd

    def test_cloudflare_custom_port(self, temp_dir: Path) -> None:
        """Test tunnel with custom --port option."""
        config_path = temp_dir / "config.yaml"
        save_config(Config(), config_path)

        with patch("mokuro_bunko.tunnel_cli.shutil.which", return_value="/usr/bin/cloudflared"):
            with patch("mokuro_bunko.tunnel_cli.subprocess.Popen") as mock_popen:
                mock_process = mock_popen.return_value
                mock_process.stderr.readline.return_value = ""
                mock_process.wait.return_value = 0

                runner = CliRunner()
                result = runner.invoke(cli, ["-c", str(config_path), "tunnel", "cloudflare", "--port", "3000"])
                assert result.exit_code == 0
                call_args = mock_popen.call_args
                cmd = call_args[0][0]
                assert "http://localhost:3000" in cmd
