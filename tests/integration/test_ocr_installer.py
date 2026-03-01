"""Integration tests for OCR installer."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mokuro_bunko.ocr.installer import (
    OCRBackend,
    OCRInstaller,
    get_supported_backends,
)


@pytest.fixture
def temp_env_path(temp_dir: Path) -> Path:
    """Return a temporary path for the OCR environment."""
    return temp_dir / "ocr-env"


@pytest.fixture
def installer(temp_env_path: Path) -> OCRInstaller:
    """Create an installer with temporary path."""
    return OCRInstaller(env_path=temp_env_path)


class TestOCRInstallerEnvironment:
    """Tests for OCR installer environment management."""

    def test_default_env_path(self) -> None:
        """Test default environment path resolution."""
        installer = OCRInstaller()
        expected = OCRInstaller.get_default_env_path()
        assert installer.env_path == expected

    def test_default_env_path_respects_override(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """Test env var override for default environment path."""
        override = Path("/tmp/custom-ocr-env")
        monkeypatch.setenv("MOKURO_BUNKO_OCR_ENV", str(override))

        installer = OCRInstaller()
        assert installer.env_path == override

    def test_default_env_path_fallback_home(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """Test fallback to home path when not running from project root."""
        monkeypatch.delenv("MOKURO_BUNKO_OCR_ENV", raising=False)
        monkeypatch.setattr(OCRInstaller, "_discover_project_root", lambda: None)

        installer = OCRInstaller()
        expected = Path.home() / ".mokuro-bunko" / "ocr-env"
        assert installer.env_path == expected

    def test_custom_env_path(self, temp_env_path: Path) -> None:
        """Test custom environment path."""
        installer = OCRInstaller(env_path=temp_env_path)
        assert installer.env_path == temp_env_path

    def test_create_environment(self, installer: OCRInstaller) -> None:
        """Test creating virtual environment."""
        result = installer.create_environment()

        assert result is True
        assert installer.env_path.exists()

        # Check Python exists
        python_path = installer._get_python_path()
        assert python_path.exists()

    def test_create_environment_idempotent(self, installer: OCRInstaller) -> None:
        """Test creating environment multiple times is safe."""
        installer.create_environment()
        result = installer.create_environment()

        assert result is True

    def test_create_environment_force(self, installer: OCRInstaller) -> None:
        """Test force recreating environment."""
        installer.create_environment()

        # Create a marker file
        marker = installer.env_path / "marker.txt"
        marker.write_text("test")

        # Force recreate
        result = installer.create_environment(force=True)

        assert result is True
        assert not marker.exists()

    def test_is_installed_false_when_no_env(self, installer: OCRInstaller) -> None:
        """Test is_installed returns False when no environment."""
        assert installer.is_installed() is False

    def test_is_installed_false_when_no_mokuro(self, installer: OCRInstaller) -> None:
        """Test is_installed returns False when mokuro not installed."""
        installer.create_environment()
        assert installer.is_installed() is False


class TestOCRInstallerInstallation:
    """Tests for OCR installation (mocked)."""

    def test_install_skip_does_nothing(self, installer: OCRInstaller) -> None:
        """Test skip backend does nothing."""
        result = installer.install(OCRBackend.SKIP)

        assert result is True
        assert not installer.env_path.exists()

    def test_install_creates_environment(self, installer: OCRInstaller) -> None:
        """Test install creates environment."""
        # Mock pip to avoid actual installation
        with patch.object(installer, "_run_pip") as mock_pip:
            mock_pip.return_value = True

            installer.install(OCRBackend.CPU)

            assert installer.env_path.exists()
            # pip should be called for: upgrade, torch, mokuro
            assert mock_pip.call_count >= 2

    def test_install_force_recreates(self, installer: OCRInstaller) -> None:
        """Test force install recreates environment."""
        with patch.object(installer, "_run_pip") as mock_pip:
            mock_pip.return_value = True

            # First install
            installer.install(OCRBackend.CPU)

            # Create marker
            marker = installer.env_path / "marker.txt"
            marker.write_text("test")

            # Force reinstall
            installer.install(OCRBackend.CPU, force=True)

            assert not marker.exists()

    def test_install_torch_cpu(self, installer: OCRInstaller) -> None:
        """Test PyTorch CPU installation command."""
        installer.create_environment()

        with patch.object(installer, "_run_pip") as mock_pip:
            mock_pip.return_value = True

            installer.install_torch(OCRBackend.CPU)

            # Check pip was called with CPU index
            call_args = mock_pip.call_args[0][0]
            assert any("cpu" in arg for arg in call_args)

    def test_install_torch_cuda(self, installer: OCRInstaller) -> None:
        """Test PyTorch CUDA installation command."""
        installer.create_environment()

        with patch.object(installer, "_run_pip") as mock_pip:
            mock_pip.return_value = True

            installer.install_torch(OCRBackend.CUDA)

            # Check pip was called with CUDA index
            call_args = mock_pip.call_args[0][0]
            assert any("cu" in arg for arg in call_args)

    def test_install_mokuro(self, installer: OCRInstaller) -> None:
        """Test Mokuro installation command."""
        installer.create_environment()

        with patch.object(installer, "_run_pip") as mock_pip:
            mock_pip.return_value = True

            installer.install_mokuro()

            # Check pip was called with mokuro
            call_args = mock_pip.call_args[0][0]
            assert "mokuro" in call_args

    def test_install_fails_if_torch_fails(self, installer: OCRInstaller) -> None:
        """Test install fails if PyTorch installation fails."""
        installer.create_environment()

        with patch.object(installer, "_run_pip") as mock_pip:
            # First call (upgrade pip) succeeds, second (torch) fails
            mock_pip.side_effect = [True, False]

            result = installer.install(OCRBackend.CPU)

            assert result is False

    def test_install_with_fallback_uses_cpu(self, installer: OCRInstaller) -> None:
        """Test fallback path retries CPU when accelerated install fails."""
        with patch.object(installer, "install") as mock_install:
            mock_install.side_effect = [False, True]

            result = installer.install_with_fallback(OCRBackend.CUDA)

            assert result is True
            assert mock_install.call_count == 2
            assert mock_install.call_args_list[0].args[0] == OCRBackend.CUDA
            assert mock_install.call_args_list[1].args[0] == OCRBackend.CPU

    def test_install_with_fallback_cpu_no_retry(self, installer: OCRInstaller) -> None:
        """Test CPU install failure does not retry further."""
        with patch.object(installer, "install") as mock_install:
            mock_install.return_value = False

            result = installer.install_with_fallback(OCRBackend.CPU)

            assert result is False
            assert mock_install.call_count == 1


class TestOCRInstallerUninstall:
    """Tests for OCR uninstallation."""

    def test_uninstall_removes_environment(self, installer: OCRInstaller) -> None:
        """Test uninstall removes the environment."""
        installer.create_environment()
        assert installer.env_path.exists()

        result = installer.uninstall()

        assert result is True
        assert not installer.env_path.exists()

    def test_uninstall_when_not_installed(self, installer: OCRInstaller) -> None:
        """Test uninstall when not installed."""
        result = installer.uninstall()

        assert result is True


class TestOCRInstallerOutput:
    """Tests for installer output."""

    def test_custom_output_callback(self, temp_env_path: Path) -> None:
        """Test custom output callback is used."""
        messages: list[str] = []

        def capture_output(msg: str) -> None:
            messages.append(msg)

        installer = OCRInstaller(
            env_path=temp_env_path,
            output_callback=capture_output,
        )

        installer.create_environment()

        # Should have logged something
        assert len(messages) > 0
        assert any("environment" in msg.lower() for msg in messages)


class TestOCRInstallerPaths:
    """Tests for path handling."""

    def test_get_python_executable(self, installer: OCRInstaller) -> None:
        """Test getting Python executable path."""
        # Not installed yet
        assert installer.get_python_executable() is None

        # After creating environment
        installer.create_environment()
        python = installer.get_python_executable()

        assert python is not None
        assert python.exists()

    def test_python_path_windows(self, temp_env_path: Path) -> None:
        """Test Python path on Windows."""
        with patch("sys.platform", "win32"):
            installer = OCRInstaller(env_path=temp_env_path)
            path = installer._get_python_path()

            assert "Scripts" in str(path)
            assert path.name == "python.exe"

    def test_python_path_unix(self, temp_env_path: Path) -> None:
        """Test Python path on Unix."""
        with patch("sys.platform", "linux"):
            installer = OCRInstaller(env_path=temp_env_path)
            path = installer._get_python_path()

            assert "bin" in str(path)
            assert path.name == "python"


class TestOCRInstallerBackendDetection:
    """Tests for installed backend detection."""

    def test_get_installed_backend_not_installed(
        self, installer: OCRInstaller
    ) -> None:
        """Test getting backend when not installed."""
        assert installer.get_installed_backend() is None

    def test_get_installed_backend_cuda(self, installer: OCRInstaller) -> None:
        """Test detecting CUDA backend (mocked)."""
        installer.create_environment()

        with patch("subprocess.run") as mock_run:
            result = MagicMock()
            result.returncode = 0
            result.stdout = "cuda"
            mock_run.return_value = result

            backend = installer.get_installed_backend()

            assert backend == OCRBackend.CUDA

    def test_get_installed_backend_cpu(self, installer: OCRInstaller) -> None:
        """Test detecting CPU backend (mocked)."""
        installer.create_environment()

        with patch("subprocess.run") as mock_run:
            result = MagicMock()
            result.returncode = 0
            result.stdout = "cpu"
            mock_run.return_value = result

            backend = installer.get_installed_backend()

            assert backend == OCRBackend.CPU

    def test_supported_backends_includes_cpu(self) -> None:
        """Test runtime supported backends always include CPU."""
        backends = get_supported_backends(python_version=(3, 14))
        assert OCRBackend.CPU in backends
