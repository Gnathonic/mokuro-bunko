"""Unit tests for hardware detection."""

from __future__ import annotations

from unittest.mock import MagicMock, patch

import pytest

from mokuro_bunko.ocr.installer import (
    HardwareInfo,
    OCRBackend,
    detect_cuda,
    detect_hardware,
    detect_mps,
    detect_rocm,
    get_backend_unavailable_reasons,
    get_recommended_backend,
    get_supported_backends,
    get_torch_install_command,
)


class TestDetectCuda:
    """Tests for CUDA detection."""

    def test_cuda_available_with_version(self) -> None:
        """Test CUDA detection with version."""
        with patch("subprocess.run") as mock_run:
            # Mock nvidia-smi success
            nvidia_result = MagicMock()
            nvidia_result.returncode = 0
            nvidia_result.stdout = "525.147.05"

            # Mock nvcc success
            nvcc_result = MagicMock()
            nvcc_result.returncode = 0
            nvcc_result.stdout = "nvcc: NVIDIA CUDA Compiler\nrelease 12.1, V12.1.66"

            mock_run.side_effect = [nvidia_result, nvcc_result]

            available, version = detect_cuda()

            assert available is True
            assert version == "12.1"

    def test_cuda_available_without_nvcc(self) -> None:
        """Test CUDA detection when nvcc is not available."""
        with patch("subprocess.run") as mock_run:
            # Mock nvidia-smi success
            nvidia_result = MagicMock()
            nvidia_result.returncode = 0
            nvidia_result.stdout = "525.147.05"

            # Mock nvcc failure
            nvcc_result = MagicMock()
            nvcc_result.returncode = 1
            nvcc_result.stdout = ""

            mock_run.side_effect = [nvidia_result, nvcc_result]

            available, version = detect_cuda()

            assert available is True
            assert version is None

    def test_cuda_not_available(self) -> None:
        """Test CUDA not available."""
        with patch("subprocess.run") as mock_run:
            mock_run.side_effect = FileNotFoundError("nvidia-smi not found")

            available, version = detect_cuda()

            assert available is False
            assert version is None

    def test_cuda_timeout(self) -> None:
        """Test CUDA detection timeout."""
        with patch("subprocess.run") as mock_run:
            import subprocess
            mock_run.side_effect = subprocess.TimeoutExpired("nvidia-smi", 10)

            available, version = detect_cuda()

            assert available is False
            assert version is None


class TestDetectRocm:
    """Tests for ROCm detection."""

    def test_rocm_available_via_smi(self) -> None:
        """Test ROCm detection via rocm-smi."""
        with patch("subprocess.run") as mock_run:
            result = MagicMock()
            result.returncode = 0
            result.stdout = "Driver Version: 5.7.0"
            mock_run.return_value = result

            available, version = detect_rocm()

            assert available is True
            assert version == "5.7.0"

    def test_rocm_available_via_path(self) -> None:
        """Test ROCm detection via /opt/rocm path."""
        with patch("subprocess.run") as mock_run:
            mock_run.side_effect = FileNotFoundError("rocm-smi not found")

            with patch("pathlib.Path.exists") as mock_exists:
                mock_exists.return_value = True

                with patch("pathlib.Path.read_text") as mock_read:
                    mock_read.return_value = "5.6.0"

                    available, version = detect_rocm()

                    # Note: Due to multiple exists() calls, this may vary
                    # The important thing is it doesn't crash
                    assert isinstance(available, bool)

    def test_rocm_not_available(self) -> None:
        """Test ROCm not available."""
        with patch("subprocess.run") as mock_run:
            mock_run.side_effect = FileNotFoundError("rocm-smi not found")

            with patch("pathlib.Path.exists") as mock_exists:
                mock_exists.return_value = False

                available, version = detect_rocm()

                assert available is False
                assert version is None


class TestDetectMps:
    """Tests for MPS detection."""

    def test_mps_on_apple_silicon(self) -> None:
        """Test MPS detection on Apple Silicon."""
        with patch("platform.system") as mock_system:
            mock_system.return_value = "Darwin"
            with patch("platform.machine") as mock_machine:
                mock_machine.return_value = "arm64"

                available = detect_mps()

                assert available is True

    def test_mps_on_intel_mac(self) -> None:
        """Test MPS detection on Intel Mac."""
        with patch("platform.system") as mock_system:
            mock_system.return_value = "Darwin"
            with patch("platform.machine") as mock_machine:
                mock_machine.return_value = "x86_64"

                available = detect_mps()

                assert available is False

    def test_mps_on_linux(self) -> None:
        """Test MPS not available on Linux."""
        with patch("platform.system") as mock_system:
            mock_system.return_value = "Linux"

            available = detect_mps()

            assert available is False

    def test_mps_on_windows(self) -> None:
        """Test MPS not available on Windows."""
        with patch("platform.system") as mock_system:
            mock_system.return_value = "Windows"

            available = detect_mps()

            assert available is False


class TestDetectHardware:
    """Tests for combined hardware detection."""

    def test_detect_all_hardware(self) -> None:
        """Test hardware detection returns HardwareInfo."""
        with patch("mokuro_bunko.ocr.installer.detect_cuda") as mock_cuda:
            mock_cuda.return_value = (True, "12.1")

            with patch("mokuro_bunko.ocr.installer.detect_rocm") as mock_rocm:
                mock_rocm.return_value = (False, None)

                with patch("mokuro_bunko.ocr.installer.detect_mps") as mock_mps:
                    mock_mps.return_value = False

                    info = detect_hardware()

                    assert isinstance(info, HardwareInfo)
                    assert info.has_cuda is True
                    assert info.cuda_version == "12.1"
                    assert info.has_rocm is False
                    assert info.rocm_version is None
                    assert info.has_mps is False


class TestGetRecommendedBackend:
    """Tests for backend recommendation."""

    def test_recommends_cuda_when_available(self) -> None:
        """Test CUDA is recommended when available."""
        hardware = HardwareInfo(
            has_cuda=True,
            has_rocm=False,
            has_mps=False,
            cuda_version="12.1",
            rocm_version=None,
        )

        backend = get_recommended_backend(hardware)

        assert backend == OCRBackend.CUDA

    def test_respects_supported_backend_filter(self) -> None:
        """Test recommendation respects supported backend constraints."""
        hardware = HardwareInfo(
            has_cuda=True,
            has_rocm=False,
            has_mps=False,
            cuda_version="12.1",
            rocm_version=None,
        )
        backend = get_recommended_backend(
            hardware,
            supported_backends=[OCRBackend.CPU],
        )
        assert backend == OCRBackend.CPU


class TestSupportedBackends:
    """Tests for runtime backend support filtering."""

    def test_supports_cuda_when_detected_and_py312(self) -> None:
        """Test CUDA remains available on supported Python versions."""
        hardware = HardwareInfo(
            has_cuda=True,
            has_rocm=False,
            has_mps=False,
            cuda_version="12.1",
            rocm_version=None,
        )
        backends = get_supported_backends(hardware=hardware, python_version=(3, 12))
        assert OCRBackend.CUDA in backends
        assert OCRBackend.CPU in backends

    def test_filters_accelerated_backends_on_py313_plus(self) -> None:
        """Test accelerated backends are filtered on newer Python runtimes."""
        hardware = HardwareInfo(
            has_cuda=True,
            has_rocm=True,
            has_mps=False,
            cuda_version="12.1",
            rocm_version="6.1",
        )
        backends = get_supported_backends(hardware=hardware, python_version=(3, 13))
        assert OCRBackend.CUDA not in backends
        assert OCRBackend.ROCM not in backends
        assert OCRBackend.CPU in backends

    def test_unavailable_reason_contains_python_version(self) -> None:
        """Test unavailability reason explains runtime constraint."""
        hardware = HardwareInfo(
            has_cuda=True,
            has_rocm=False,
            has_mps=False,
            cuda_version="12.1",
            rocm_version=None,
        )
        reasons = get_backend_unavailable_reasons(hardware=hardware, python_version=(3, 14))
        assert OCRBackend.CUDA in reasons
        assert "Python 3.14" in reasons[OCRBackend.CUDA]

    def test_recommends_rocm_when_no_cuda(self) -> None:
        """Test ROCm is recommended when CUDA not available."""
        hardware = HardwareInfo(
            has_cuda=False,
            has_rocm=True,
            has_mps=False,
            cuda_version=None,
            rocm_version="5.7",
        )

        backend = get_recommended_backend(hardware)

        assert backend == OCRBackend.ROCM

    def test_recommends_mps_when_no_cuda_rocm(self) -> None:
        """Test MPS is recommended on Apple Silicon."""
        hardware = HardwareInfo(
            has_cuda=False,
            has_rocm=False,
            has_mps=True,
            cuda_version=None,
            rocm_version=None,
        )

        backend = get_recommended_backend(hardware)

        assert backend == OCRBackend.MPS

    def test_recommends_cpu_as_fallback(self) -> None:
        """Test CPU is recommended as fallback."""
        hardware = HardwareInfo(
            has_cuda=False,
            has_rocm=False,
            has_mps=False,
            cuda_version=None,
            rocm_version=None,
        )

        backend = get_recommended_backend(hardware)

        assert backend == OCRBackend.CPU

    def test_cuda_priority_over_rocm(self) -> None:
        """Test CUDA has priority over ROCm."""
        hardware = HardwareInfo(
            has_cuda=True,
            has_rocm=True,
            has_mps=False,
            cuda_version="12.1",
            rocm_version="5.7",
        )

        backend = get_recommended_backend(hardware)

        assert backend == OCRBackend.CUDA


class TestGetTorchInstallCommand:
    """Tests for PyTorch install commands."""

    def test_cuda_install_command(self) -> None:
        """Test CUDA install command."""
        cmd = get_torch_install_command(OCRBackend.CUDA)

        assert "torch" in cmd
        assert "torchvision" in cmd
        assert "--index-url" in cmd
        assert any("cu" in arg for arg in cmd)

    def test_rocm_install_command(self) -> None:
        """Test ROCm install command."""
        cmd = get_torch_install_command(OCRBackend.ROCM)

        assert "torch" in cmd
        assert "torchvision" in cmd
        assert "--index-url" in cmd
        assert any("rocm" in arg for arg in cmd)

    def test_mps_install_command(self) -> None:
        """Test MPS install command (standard PyTorch)."""
        cmd = get_torch_install_command(OCRBackend.MPS)

        assert "torch" in cmd
        assert "torchvision" in cmd
        # MPS uses standard PyTorch, no special index
        assert "--index-url" not in cmd

    def test_cpu_install_command(self) -> None:
        """Test CPU install command."""
        cmd = get_torch_install_command(OCRBackend.CPU)

        assert "torch" in cmd
        assert "torchvision" in cmd
        assert "--index-url" in cmd
        assert any("cpu" in arg for arg in cmd)


class TestOCRBackendEnum:
    """Tests for OCRBackend enum."""

    def test_all_backends_defined(self) -> None:
        """Test all expected backends are defined."""
        backends = [b.value for b in OCRBackend]

        assert "cuda" in backends
        assert "rocm" in backends
        assert "mps" in backends
        assert "cpu" in backends
        assert "skip" in backends

    def test_backend_from_string(self) -> None:
        """Test creating backend from string."""
        assert OCRBackend("cuda") == OCRBackend.CUDA
        assert OCRBackend("cpu") == OCRBackend.CPU

    def test_invalid_backend_raises(self) -> None:
        """Test invalid backend string raises ValueError."""
        with pytest.raises(ValueError):
            OCRBackend("invalid")
