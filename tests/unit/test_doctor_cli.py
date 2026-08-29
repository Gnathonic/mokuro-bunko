"""Unit tests for the doctor diagnostics command."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import pytest
import yaml
from click.testing import CliRunner

from mokuro_bunko.__main__ import cli
from mokuro_bunko.ocr.installer import OCRInstaller


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


@pytest.fixture
def config_file(temp_dir: Path) -> Path:
    """Minimal config pointing storage into the temp dir; port 0 always binds."""
    storage = temp_dir / "storage"
    config_path = temp_dir / "config.yaml"
    config_path.write_text(
        yaml.safe_dump({
            "server": {"host": "127.0.0.1", "port": 0},
            "storage": {"base_path": str(storage)},
        }),
        encoding="utf-8",
    )
    return config_path


class TestDoctorCommand:
    """End-to-end CLI behavior with the environment mocked."""

    def test_doctor_passes_without_ocr_env(
        self, runner: CliRunner, config_file: Path
    ) -> None:
        """Missing OCR env and missing nvidia-smi are warnings, not failures."""
        with (
            patch(
                "mokuro_bunko.doctor_cli.subprocess.run",
                side_effect=FileNotFoundError,
            ),
            patch.object(OCRInstaller, "get_python_executable", return_value=None),
        ):
            result = runner.invoke(cli, ["-c", str(config_file), "doctor"])

        assert result.exit_code == 0
        assert "PASS" in result.output
        assert "nvidia-smi not found" in result.output
        assert "not installed" in result.output
        assert "warning" in result.output.lower()

    def test_doctor_fails_on_broken_ocr_stack(
        self, runner: CliRunner, config_file: Path
    ) -> None:
        """A broken OCR env (e.g. transformers 5.x) is a FAIL with exit 1."""
        with (
            patch(
                "mokuro_bunko.doctor_cli.subprocess.run",
                side_effect=FileNotFoundError,
            ),
            patch.object(
                OCRInstaller,
                "get_python_executable",
                return_value=Path("fake/python.exe"),
            ),
            patch.object(
                OCRInstaller,
                "verify_installation",
                return_value=(False, ["transformers 5.13.0 is incompatible with manga-ocr"]),
            ),
        ):
            result = runner.invoke(cli, ["-c", str(config_file), "doctor"])

        assert result.exit_code == 1
        assert "FAIL" in result.output
        assert "transformers 5.13.0" in result.output
        assert "install-ocr --force" in result.output

    def test_doctor_reports_healthy_ocr_stack(
        self, runner: CliRunner, config_file: Path
    ) -> None:
        """A verified OCR env shows its stack summary."""
        with (
            patch(
                "mokuro_bunko.doctor_cli.subprocess.run",
                side_effect=FileNotFoundError,
            ),
            patch.object(
                OCRInstaller,
                "get_python_executable",
                return_value=Path("fake/python.exe"),
            ),
            patch.object(
                OCRInstaller,
                "verify_installation",
                return_value=(True, ["torch 2.13.0+cu130, cuda available: True"]),
            ),
        ):
            result = runner.invoke(cli, ["-c", str(config_file), "doctor"])

        assert result.exit_code == 0
        assert "torch 2.13.0+cu130" in result.output

    def test_doctor_warns_on_failed_volumes(
        self, runner: CliRunner, config_file: Path, temp_dir: Path
    ) -> None:
        """Persisted OCR failures surface with a pointer to the logs."""
        storage = temp_dir / "storage"
        storage.mkdir(parents=True, exist_ok=True)
        (storage / ".ocr-failures.json").write_text(
            '{"S/V.cbz": {"error": "boom", "attempts": 2}}', encoding="utf-8"
        )
        with (
            patch(
                "mokuro_bunko.doctor_cli.subprocess.run",
                side_effect=FileNotFoundError,
            ),
            patch.object(OCRInstaller, "get_python_executable", return_value=None),
        ):
            result = runner.invoke(cli, ["-c", str(config_file), "doctor"])

        assert result.exit_code == 0
        assert "1 volume(s) failing OCR" in result.output

    def test_doctor_fails_on_bad_config(self, runner: CliRunner, temp_dir: Path) -> None:
        """An invalid config file is a FAIL with exit 1."""
        bad_config = temp_dir / "config.yaml"
        bad_config.write_text("server:\n  port: -5\n", encoding="utf-8")
        with patch(
            "mokuro_bunko.doctor_cli.subprocess.run",
            side_effect=FileNotFoundError,
        ), patch.object(OCRInstaller, "get_python_executable", return_value=None):
            result = runner.invoke(cli, ["-c", str(bad_config), "doctor"])

        assert result.exit_code == 1
        assert "FAIL" in result.output
