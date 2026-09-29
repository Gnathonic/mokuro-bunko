"""OCR installer for mokuro-bunko.

Handles hardware detection and installation of PyTorch + Mokuro
into an isolated virtual environment.
"""

from __future__ import annotations

import inspect
import os
import platform
import shutil
import subprocess
import sys
import venv
from collections.abc import Callable, Sequence
from enum import Enum
from pathlib import Path
from typing import NamedTuple

from mokuro_bunko.ocr import rocm_gfx as _rocm_gfx_module
from mokuro_bunko.ocr.engines import DEFAULT_DETECTOR, get_detector


class OCRBackend(Enum):
    """Available OCR backends."""

    CUDA = "cuda"
    ROCM = "rocm"
    MPS = "mps"
    CPU = "cpu"
    SKIP = "skip"


# Packages installed alongside mokuro into the OCR environment.
#
# transformers is pinned below 5.x: manga-ocr's `kha-white/manga-ocr-base`
# tokenizer cannot be instantiated by the transformers 5.x tokenizer backend
# (fails with "Couldn't instantiate the backend tokenizer"), and mokuro
# swallows the per-volume error, so an unpinned install silently produces
# thumbnails but no .mokuro files. sentencepiece is required by the
# slow-to-fast tokenizer conversion path.
# The OCR engine is the performance-optimized mokuro fork (fp32 by default,
# output identical to upstream mokuro 0.2.5; 4-20x faster on GPUs and 2-4x on
# CPUs thanks to a worker-process page pipeline). Override the spec with
# MOKURO_BUNKO_MOKURO_SPEC (e.g. "mokuro" for the PyPI release) if needed.
MOKURO_PACKAGE_SPEC: str = os.environ.get(
    "MOKURO_BUNKO_MOKURO_SPEC",
    "mokuro @ git+https://github.com/Gnathonic/mokuro.git@feat/serve-mode",
)
MOKURO_INSTALL_PACKAGES: list[str] = [
    MOKURO_PACKAGE_SPEC,
    "transformers>=4.25,<5",
    "sentencepiece",
]

# What the admin page's Environment block says under the backend line. Plain
# sentences for the person running the server; a command is written as plain
# text, and the page decides how to set it.
OCR_CLI_HINT: str = (
    "To change the backend, restart the server with --ocr set to auto, cuda, rocm, "
    "cpu or skip. To see which backends this machine can use, run "
    "mokuro-bunko install-ocr --list-backends"
)
OCR_DRIVER_HINT: str = (
    "The CUDA and ROCm backends need the graphics card's drivers installed on this machine."
)
OCR_NO_LOCAL_HINT: str = (
    "This server does no OCR itself: local processing is off, or the backend is set "
    "to skip. Volumes are read by processors on other machines, started there with "
    "mokuro-bunko processor serve"
)

# Package set of the separate engines environment (see EnginesInstaller).
# Detector extras (e.g. "mokuro" for the GPL comic-text-detector) are added
# per configured detector, never by default.
# The ``hayai-ocr`` package is deliberately NOT here. It existed only to drive
# hayai-ocr v2, which was withdrawn; v2.5 Nova goes through transformers
# directly, which is what lets it honour a generation's ``patch_budget`` at
# all. Nor is ``manga-ocr``: it was here only to lend attention positions to
# the per-character placement system, and that whole system is gone.
# Environments installed before either removal still carry the package; it is
# inert there, and `install-ocr --force` sheds it.
#
# PINNED, exactly: the three packages that turn a page into the crop every
# read starts from -- Pillow decodes the page, OpenCV warps and cuts the line
# out of it, transformers' image processors turn the crop into patches (and
# run the models). A silent upgrade of any of them could change a sidecar
# with nothing else changing, the way an unpinned model repo could, so they
# get the same treatment: pinned to what both processors were verified on
# (the workstation and tower reproduce each other's line crops bit for bit,
# 403/403), and bumped only after re-running a bench volume and comparing
# the sidecar. transformers 5 is also the floor PaddleOCR-VL-1.6 needs.
# The pins reach NEW installs and `--force` reinstalls; an environment that
# already exists keeps what it has.
ENGINES_ENV_PACKAGES: tuple[str, ...] = (
    "transformers==5.17.0",
    "sentencepiece",
    "peft",
    "accelerate",
    "natsort",
    "opencv-python-headless==5.0.0.93",
    "Pillow==12.3.0",
)


# Map system ROCm major.minor to the best PyTorch wheel channel.
# Falls back to the latest known channel if no exact match is found.
_ROCM_WHEEL_CHANNELS: dict[str, str] = {
    "7.1": "rocm7.1",
    "7.2": "rocm7.1",
    "6.3": "rocm6.3",
    "6.4": "rocm6.4",
}
_ROCM_DEFAULT_CHANNEL = "rocm7.1"

# Python versions that require nightly index (no stable ROCm wheels).
_ROCM_NIGHTLY_MIN_PYTHON = (3, 13)


class HardwareInfo(NamedTuple):
    """Hardware detection results."""

    has_cuda: bool
    has_rocm: bool
    has_mps: bool
    cuda_version: str | None
    rocm_version: str | None


def get_backend_unavailable_reasons(
    hardware: HardwareInfo | None = None,
    python_version: tuple[int, int] | None = None,
) -> dict[OCRBackend, str]:
    """Return reasons for backend unavailability on this host/runtime."""
    hw = hardware or detect_hardware()
    pyver = python_version or (sys.version_info.major, sys.version_info.minor)

    reasons: dict[OCRBackend, str] = {}
    if not hw.has_cuda:
        reasons[OCRBackend.CUDA] = "CUDA backend requires a detected NVIDIA CUDA setup"
    if not hw.has_rocm:
        reasons[OCRBackend.ROCM] = "ROCm backend requires a detected AMD ROCm setup"
    if not hw.has_mps:
        reasons[OCRBackend.MPS] = "MPS backend requires Apple Silicon (macOS arm64)"

    # Current PyTorch accelerator wheel availability is narrower than CPU.
    # Guard against common unsupported interpreter versions.
    # ROCm nightly wheels support Python 3.13+, so only block CUDA.
    if pyver >= (3, 13):
        if OCRBackend.CUDA not in reasons:
            reasons[OCRBackend.CUDA] = (
                f"CUDA wheels are typically unavailable for Python {pyver[0]}.{pyver[1]} "
                "in stable channels"
            )

    return reasons


def get_supported_backends(
    hardware: HardwareInfo | None = None,
    python_version: tuple[int, int] | None = None,
) -> list[OCRBackend]:
    """Return supported OCR backends for current host/runtime."""
    reasons = get_backend_unavailable_reasons(
        hardware=hardware,
        python_version=python_version,
    )
    ordered = [OCRBackend.CUDA, OCRBackend.ROCM, OCRBackend.MPS, OCRBackend.CPU]
    return [backend for backend in ordered if backend not in reasons or backend == OCRBackend.CPU]


def detect_cuda() -> tuple[bool, str | None]:
    """Detect CUDA availability and version.

    Returns:
        Tuple of (available, version).
    """
    try:
        # Try nvidia-smi first
        result = subprocess.run(
            ["nvidia-smi", "--query-gpu=driver_version", "--format=csv,noheader"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0 and result.stdout.strip():
            # Get CUDA version from nvcc if available
            try:
                nvcc_result = subprocess.run(
                    ["nvcc", "--version"],
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                if nvcc_result.returncode == 0:
                    # Parse version from "release X.Y" pattern
                    for line in nvcc_result.stdout.split("\n"):
                        if "release" in line.lower():
                            parts = line.split("release")
                            if len(parts) > 1:
                                version = parts[1].strip().split(",")[0].strip()
                                return True, version
            except (FileNotFoundError, subprocess.TimeoutExpired, subprocess.SubprocessError):
                pass
            return True, None
    except (FileNotFoundError, subprocess.TimeoutExpired, subprocess.SubprocessError):
        pass

    return False, None


# Run first in the OCR environments' own interpreters by the verify
# snippets: `rocm_gfx` is standard-library only for exactly this.
_ROCM_PRELUDE = inspect.getsource(_rocm_gfx_module)


def detect_rocm() -> tuple[bool, str | None]:
    """Detect ROCm availability and version.

    Returns:
        Tuple of (available, version).
    """
    version: str | None = None

    # Prefer /opt/rocm version file — it contains the real ROCm version
    # (rocm-smi --showdriverversion reports the kernel driver, not ROCm).
    rocm_path = Path("/opt/rocm")
    if rocm_path.exists():
        version_file = rocm_path / ".info" / "version"
        if version_file.exists():
            try:
                version = version_file.read_text().strip()
            except OSError:
                pass
        return True, version

    # Fallback: check for rocm-smi (ROCm may be installed elsewhere).
    try:
        result = subprocess.run(
            ["rocm-smi", "--showdriverversion"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0:
            return True, None
    except (FileNotFoundError, subprocess.TimeoutExpired, subprocess.SubprocessError):
        pass

    # No system ROCm at all: PyTorch's ROCm wheels bring their own runtime and
    # need only the kernel driver, so an AMD GPU the driver sees is enough.
    from mokuro_bunko.ocr import rocm_gfx

    if rocm_gfx.amd_gpu_present():
        return True, None

    return False, None


def detect_mps() -> bool:
    """Detect Apple Metal Performance Shaders availability.

    Returns:
        True if MPS is available.
    """
    if platform.system() != "Darwin":
        return False

    # Check for Apple Silicon
    if platform.machine() == "arm64":
        return True

    # For Intel Macs, MPS might still work but CUDA/CPU is usually better
    return False


def detect_hardware() -> HardwareInfo:
    """Detect available hardware accelerators.

    Returns:
        HardwareInfo with detection results.
    """
    has_cuda, cuda_version = detect_cuda()
    has_rocm, rocm_version = detect_rocm()
    has_mps = detect_mps()

    return HardwareInfo(
        has_cuda=has_cuda,
        has_rocm=has_rocm,
        has_mps=has_mps,
        cuda_version=cuda_version,
        rocm_version=rocm_version,
    )


def get_recommended_backend(
    hardware: HardwareInfo,
    supported_backends: list[OCRBackend] | None = None,
) -> OCRBackend:
    """Get the recommended backend based on hardware.

    Args:
        hardware: Hardware detection results.

    Returns:
        Recommended OCR backend.
    """
    if supported_backends is None:
        if hardware.has_cuda:
            return OCRBackend.CUDA
        if hardware.has_rocm:
            return OCRBackend.ROCM
        if hardware.has_mps:
            return OCRBackend.MPS
        return OCRBackend.CPU

    supported = supported_backends
    # Priority: CUDA > ROCm > MPS > CPU
    for backend in (OCRBackend.CUDA, OCRBackend.ROCM, OCRBackend.MPS, OCRBackend.CPU):
        if backend in supported:
            return backend
    return OCRBackend.CPU


def _resolve_rocm_index_url(
    rocm_version: str | None = None,
    python_version: tuple[int, int] | None = None,
) -> str:
    """Resolve the PyTorch index URL for a ROCm installation.

    Picks the best wheel channel for the detected ROCm version and uses
    the nightly index when stable wheels are unavailable for the current
    Python interpreter.
    """
    pyver = python_version or (sys.version_info.major, sys.version_info.minor)

    # Pick wheel channel from detected ROCm version.
    channel = _ROCM_DEFAULT_CHANNEL
    if rocm_version:
        major_minor = ".".join(rocm_version.split(".")[:2])
        channel = _ROCM_WHEEL_CHANNELS.get(major_minor, _ROCM_DEFAULT_CHANNEL)

    # Nightly is required for Python >= 3.13 (no stable ROCm wheels).
    if pyver >= _ROCM_NIGHTLY_MIN_PYTHON:
        return f"https://download.pytorch.org/whl/nightly/{channel}"
    return f"https://download.pytorch.org/whl/{channel}"


def get_torch_install_command(
    backend: OCRBackend,
    hardware: HardwareInfo | None = None,
) -> list[str]:
    """Get the pip install command for PyTorch based on backend.

    Args:
        backend: Selected OCR backend.
        hardware: Optional hardware info for version-aware URL selection.

    Returns:
        List of pip install arguments.
    """
    if backend == OCRBackend.CUDA:
        return [
            "torch",
            "torchvision",
            "--index-url",
            "https://download.pytorch.org/whl/cu130",
        ]
    elif backend == OCRBackend.ROCM:
        rocm_version = hardware.rocm_version if hardware else None
        index_url = _resolve_rocm_index_url(rocm_version=rocm_version)
        return [
            "torch",
            "torchvision",
            "--index-url",
            index_url,
        ]
    elif backend == OCRBackend.MPS:
        # MPS uses standard PyTorch on macOS
        return ["torch", "torchvision"]
    else:
        # CPU fallback
        return [
            "torch",
            "torchvision",
            "--index-url",
            "https://download.pytorch.org/whl/cpu",
        ]


class OCRInstaller:
    """Installer for OCR dependencies."""

    @staticmethod
    def _discover_project_root() -> Path | None:
        """Find the repository root when running from source."""
        current = Path(__file__).resolve()
        for parent in current.parents:
            if (parent / "pyproject.toml").exists() and (parent / "src" / "mokuro_bunko").exists():
                return parent
        return None

    @classmethod
    def get_default_env_path(cls) -> Path:
        """Resolve default OCR env path with portable project preference."""
        env_override = os.environ.get("MOKURO_BUNKO_OCR_ENV")
        if env_override:
            return Path(env_override).expanduser()

        project_root = cls._discover_project_root()
        if project_root is not None:
            return project_root / ".ocr-env"

        return Path.home() / ".mokuro-bunko" / "ocr-env"

    def __init__(
        self,
        env_path: Path | None = None,
        output_callback: Callable[[str], None] | None = None,
    ) -> None:
        """Initialize OCR installer.

        Args:
            env_path: Path for the isolated virtual environment.
            output_callback: Optional callback for installation output.
        """
        self.env_path = env_path or self.get_default_env_path()
        self.output_callback = output_callback or print
        # The GPU backend a CPU install replaced (install_with_fallback), so a
        # caller can say so; None when nothing fell back.
        self.fell_back_from: OCRBackend | None = None

    def _log(self, message: str) -> None:
        """Log a message using the output callback."""
        self.output_callback(message)

    def is_installed(self) -> bool:
        """Check if OCR environment is installed.

        Returns:
            True if the environment exists and has mokuro installed.
        """
        python_path = self._get_python_path()
        if not python_path.exists():
            return False

        # Check if mokuro is installed
        try:
            result = subprocess.run(
                [str(python_path), "-c", "import mokuro; print(mokuro.__version__)"],
                capture_output=True,
                timeout=30,
            )
            return result.returncode == 0
        except (subprocess.TimeoutExpired, subprocess.SubprocessError):
            return False

    def get_installed_backend(self) -> OCRBackend | None:
        """Get the currently installed backend.

        Returns:
            Installed backend or None if not installed.
        """
        python_path = self._get_python_path()
        if not python_path.exists():
            return None

        try:
            result = subprocess.run(
                [str(python_path), "-c", """
import torch
# ROCm torch answers torch.cuda.is_available() True (HIP wears the cuda API),
# so the HIP check has to come FIRST or every ROCm install calls itself cuda.
if hasattr(torch.version, 'hip') and torch.version.hip is not None:
    print('rocm')
elif torch.cuda.is_available():
    print('cuda')
elif hasattr(torch.backends, 'mps') and torch.backends.mps.is_available():
    print('mps')
else:
    print('cpu')
"""],
                capture_output=True,
                text=True,
                timeout=30,
            )
            if result.returncode == 0:
                backend_str = result.stdout.strip()
                try:
                    return OCRBackend(backend_str)
                except ValueError:
                    return OCRBackend.CPU
        except (subprocess.TimeoutExpired, subprocess.SubprocessError):
            pass

        return None

    def get_installed_torch_build(self) -> OCRBackend | None:
        """Which wheel flavour of torch the environment holds, or None.

        Unlike get_installed_backend this reports the build (a CUDA or ROCm
        wheel counts even when the device is not usable right now), which
        is the right question for "does this environment need rebuilding
        for the GPU we detected".
        """
        python_path = self._get_python_path()
        if not python_path.exists():
            return None
        snippet = (
            "import torch\n"
            "v = torch.version\n"
            "if getattr(v, 'hip', None):\n"
            "    print('rocm')\n"
            "elif getattr(v, 'cuda', None):\n"
            "    print('cuda')\n"
            "elif hasattr(torch.backends, 'mps') and torch.backends.mps.is_built():\n"
            "    print('mps')\n"
            "else:\n"
            "    print('cpu')\n"
        )
        try:
            result = subprocess.run(
                [str(python_path), "-c", snippet], capture_output=True, text=True, timeout=30
            )
        except (subprocess.TimeoutExpired, subprocess.SubprocessError):
            return None
        if result.returncode != 0:
            return None
        try:
            return OCRBackend(result.stdout.strip())
        except ValueError:
            return None

    # Written after a GPU rebuild that still ended on a CPU torch (the GPU
    # wheels failed to install), so the multi-GB attempt is not repeated on
    # every start. Removed by any successful (re)install.
    _REBUILD_FAILED_MARKER = ".gpu-rebuild-failed"

    def needs_rebuild_for(self, backend: OCRBackend) -> bool:
        """True when a GPU backend was selected but the env holds a CPU torch."""
        if backend not in (OCRBackend.CUDA, OCRBackend.ROCM, OCRBackend.MPS):
            return False
        if (self.env_path / self._REBUILD_FAILED_MARKER).exists():
            return False
        return self.get_installed_torch_build() == OCRBackend.CPU

    def rebuild_for(self, backend: OCRBackend) -> bool:
        """Reinstall the environment for a GPU backend.

        Returns True when the environment now holds a build for that backend;
        False (and remembers not to retry) when it fell back to CPU.
        """
        ok = self.install_with_fallback(backend, force=True)
        if ok and self.get_installed_torch_build() == backend:
            return True
        try:
            (self.env_path / self._REBUILD_FAILED_MARKER).write_text(
                f"rebuild for {backend.value} failed; delete this file to retry\n",
                encoding="utf-8",
            )
        except OSError:
            pass
        return False

    def create_environment(self, force: bool = False) -> bool:
        """Create the isolated virtual environment.

        Args:
            force: If True, remove existing environment first.

        Returns:
            True if environment was created successfully.
        """
        if self.env_path.exists():
            if force:
                self._log(f"Removing existing environment at {self.env_path}")
                self._clear_directory(self.env_path)
            elif self._get_python_path().exists():
                self._log(f"Environment already exists at {self.env_path}")
                return True
            else:
                # Directory exists but isn't a valid venv (e.g. mkdir'd by
                # entrypoint).  Remove it so venv.create can start fresh.
                self._log(f"Incomplete environment at {self.env_path}, recreating")
                self._clear_directory(self.env_path)

        self._log(f"Creating virtual environment at {self.env_path}")
        self.env_path.parent.mkdir(parents=True, exist_ok=True)

        try:
            venv.create(self.env_path, with_pip=True)
            return True
        except Exception as e:
            self._log(f"Failed to create environment: {e}")
            return False

    def install_torch(
        self,
        backend: OCRBackend,
        hardware: HardwareInfo | None = None,
    ) -> bool:
        """Install PyTorch for the specified backend.

        Args:
            backend: OCR backend to install for.
            hardware: Optional hardware info for version-aware URL selection.

        Returns:
            True if installation succeeded.
        """
        if backend == OCRBackend.SKIP:
            self._log("Skipping PyTorch installation")
            return True

        pip_path = self._get_pip_path()
        if not pip_path.exists():
            self._log("Pip not found in environment")
            return False

        install_args = get_torch_install_command(backend, hardware=hardware)
        self._log(f"Installing PyTorch for {backend.value}...")

        cmd = [str(pip_path), "install"] + install_args
        return self._run_pip(cmd)

    def install_mokuro(self) -> bool:
        """Install Mokuro and its dependencies.

        Installs mokuro together with a version-pinned tokenizer stack
        (see MOKURO_INSTALL_PACKAGES) so the OCR environment works out of
        the box.

        Returns:
            True if installation succeeded.
        """
        pip_path = self._get_pip_path()
        if not pip_path.exists():
            self._log("Pip not found in environment")
            return False

        self._log("Installing Mokuro...")
        cmd = [str(pip_path), "install", *MOKURO_INSTALL_PACKAGES]
        return self._run_pip(cmd)

    def install(
        self,
        backend: OCRBackend,
        force: bool = False,
        hardware: HardwareInfo | None = None,
    ) -> bool:
        """Perform full OCR installation.

        Args:
            backend: OCR backend to install.
            force: If True, reinstall even if already installed.
            hardware: Optional hardware info for version-aware URL selection.

        Returns:
            True if installation succeeded.
        """
        if backend == OCRBackend.SKIP:
            self._log("OCR installation skipped")
            return True

        # Create environment
        if not self.create_environment(force=force):
            return False

        # Upgrade pip first -- through the interpreter: Windows refuses to let
        # pip.exe replace itself ("ERROR: To modify pip, please run ...").
        self._log("Upgrading pip...")
        self._run_pip([str(self._get_python_path()), "-m", "pip", "install", "--upgrade", "pip"])

        # Install PyTorch
        if not self.install_torch(backend, hardware=hardware):
            return False

        # Install Mokuro
        if not self.install_mokuro():
            return False

        # Smoke-test the environment so a broken install is caught here,
        # loudly, instead of failing silently at OCR time.
        ok, problems = self.verify_installation()
        if not ok:
            self._log("OCR environment verification FAILED:")
            for problem in problems:
                self._log(f"  - {problem}")
            return False
        for line in problems:
            self._log(f"  {line}")

        self._log("OCR installation complete!")
        return True

    # Snippet run inside the OCR env to validate the installed stack.
    # Import-only (no model downloads). Prints one line per check;
    # lines starting with "PROBLEM:" indicate failures.
    _VERIFY_SNIPPET = _ROCM_PRELUDE + """
import importlib.metadata as md

problems = []

try:
    import torch
    _override = apply_for_torch(torch)
    if _override:
        print(f"ROCm: this card is not in the torch build; HSA_OVERRIDE_GFX_VERSION={_override}")
    line = f"torch {torch.__version__}, cuda available: {torch.cuda.is_available()}"
    if torch.cuda.is_available():
        line += f" ({torch.cuda.get_device_name(0)})"
        # "Available" is not "computes": an unbuilt AMD target passes the
        # line above and dumps core at its first kernel.
        _x = torch.randn(64, 64, device="cuda")
        if not bool(torch.isfinite((_x @ _x).sum()).item()):
            problems.append("a matrix multiply on the GPU returned a non-finite result")
    print(line)
except Exception as e:
    problems.append(f"torch failed on this machine: {e}")

try:
    version = md.version("transformers")
    major = int(version.split(".")[0])
    print(f"transformers {version}")
    if major >= 5:
        problems.append(
            f"transformers {version} is incompatible with manga-ocr "
            "(needs >=4.25,<5); reinstall with: mokuro-bunko install-ocr --force"
        )
except Exception as e:
    problems.append(f"transformers check failed: {e}")

for module in ("sentencepiece", "manga_ocr", "mokuro"):
    try:
        __import__(module)
        print(f"{module} ok")
    except Exception as e:
        problems.append(f"{module} import failed: {e}")

for problem in problems:
    print(f"PROBLEM: {problem}")
"""

    def verify_installation(self) -> tuple[bool, list[str]]:
        """Smoke-test the OCR environment without downloading models.

        Imports the critical packages inside the OCR env and checks the
        transformers version pin that manga-ocr requires.

        Returns:
            Tuple of (ok, lines). When ok is True, lines are informational
            (installed versions, CUDA availability). When ok is False,
            lines describe the problems found.
        """
        python_path = self._get_python_path()
        if not python_path.exists():
            return False, ["OCR environment python not found"]

        try:
            result = subprocess.run(
                [str(python_path), "-c", self._VERIFY_SNIPPET],
                capture_output=True,
                text=True,
                encoding="utf-8",
                errors="replace",
                timeout=120,
            )
        except (subprocess.TimeoutExpired, subprocess.SubprocessError) as e:
            return False, [f"verification subprocess failed: {e}"]

        if result.returncode != 0:
            detail = (result.stderr or result.stdout or "").strip()
            return False, [f"verification crashed: {detail[:500]}"]

        lines = [line.strip() for line in result.stdout.splitlines() if line.strip()]
        problems = [line[len("PROBLEM: "):] for line in lines if line.startswith("PROBLEM: ")]
        info = [line for line in lines if not line.startswith("PROBLEM: ")]

        if problems:
            return False, problems
        return True, info

    def install_with_fallback(
        self,
        backend: OCRBackend,
        force: bool = False,
        hardware: HardwareInfo | None = None,
    ) -> bool:
        """Install OCR backend with automatic fallback to CPU when needed."""
        if backend == OCRBackend.SKIP:
            return self.install(backend, force=force, hardware=hardware)

        self.fell_back_from = None
        if self.install(backend, force=force, hardware=hardware):
            return True

        if backend == OCRBackend.CPU:
            return False

        # A GPU build is gigabytes of wheels, and one damaged download (a
        # "Bad CRC-32" in a CUDA DLL) fails the whole install -- possibly from
        # pip's cache, which would serve the same damaged file again. So once
        # more, with the cache off, before settling for the CPU.
        self._log(
            f"Backend {backend.value} installation failed; retrying once without pip's cache..."
        )
        previous = os.environ.get("PIP_NO_CACHE_DIR")
        os.environ["PIP_NO_CACHE_DIR"] = "1"
        try:
            if self.install(backend, force=True, hardware=hardware):
                return True
        finally:
            if previous is None:
                os.environ.pop("PIP_NO_CACHE_DIR", None)
            else:
                os.environ["PIP_NO_CACHE_DIR"] = previous

        self._log(
            f"Backend {backend.value} installation failed again, falling back to CPU backend..."
        )
        if not self.install(OCRBackend.CPU, force=True):
            return False
        self.fell_back_from = backend
        self._log("")
        self._log("=" * 72)
        self._log(
            f"WARNING: {self.env_path.name} could not be installed for {backend.value} "
            "and runs OCR on the CPU instead."
        )
        self._log("  The reason is in the lines above this one.")
        self._log(
            "  Fix the cause (driver, network, disk space), then reinstall with --force "
            "(processor install --force, or install-ocr --force)."
        )
        self._log("=" * 72)
        return True

    def uninstall(self) -> bool:
        """Remove the OCR environment.

        Returns:
            True if removal succeeded.
        """
        if not self.env_path.exists():
            self._log("OCR environment not found")
            return True

        self._log(f"Removing OCR environment at {self.env_path}")
        try:
            shutil.rmtree(self.env_path)
            return True
        except Exception as e:
            self._log(f"Failed to remove environment: {e}")
            return False

    def get_python_executable(self) -> Path | None:
        """Get the Python executable in the OCR environment.

        Returns:
            Path to Python executable or None if not installed.
        """
        python_path = self._get_python_path()
        if python_path.exists():
            return python_path
        return None

    @staticmethod
    def _clear_directory(path: Path) -> None:
        """Remove contents of a directory without removing the directory itself.

        Works correctly when the path is a mount point (e.g. Docker volume).
        """
        for child in path.iterdir():
            if child.is_dir() and not child.is_symlink():
                shutil.rmtree(child)
            else:
                child.unlink()

    def _get_python_path(self) -> Path:
        """Get the path to Python in the virtual environment."""
        if sys.platform == "win32":
            return self.env_path / "Scripts" / "python.exe"
        return self.env_path / "bin" / "python"

    def _get_pip_path(self) -> Path:
        """Get the path to pip in the virtual environment."""
        if sys.platform == "win32":
            return self.env_path / "Scripts" / "pip.exe"
        return self.env_path / "bin" / "pip"

    def _run_pip(self, cmd: list[str]) -> bool:
        """Run a pip command with output logging.

        Args:
            cmd: Full pip command to run.

        Returns:
            True if command succeeded.
        """
        try:
            process = subprocess.Popen(
                cmd,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
            )

            if process.stdout:
                for line in process.stdout:
                    self._log(line.rstrip())

            process.wait()
            return process.returncode == 0
        except subprocess.SubprocessError as e:
            self._log(f"Command failed: {e}")
            return False


class EnginesInstaller(OCRInstaller):
    """Installer for the second, transformers-5 environment used by the
    non-mokuro OCR engines (hayai-ocr v2.5 Nova, PaddleOCR-VL manga LoRA,
    PP-OCRv6 manga).

    Torch is installed exactly like the mokuro environment; on top of it go
    ``ENGINES_ENV_PACKAGES`` plus the extras of the configured detector
    (``detector="ctd"`` pulls the GPL-3.0 comic-text-detector via the mokuro
    package; the default ``ppocr-manga`` needs onnxruntime).

    ``detectors`` is the UNION over the configured generations: a row on an
    engine with a detector of its own (``ppocr-manga``: onnxruntime) needs
    that one, and every other row needs the detector it names. It is
    recomputed by :meth:`set_detectors` on every live settings change,
    because with a detector per generation the set really does move.

    ``hayai-nova`` needs no package of its own: the runner drives it through
    ``transformers`` (``trust_remote_code``, at a pinned commit) directly, and
    transformers is already in ``ENGINES_ENV_PACKAGES``. Its vision half is
    stock SigLIP2 NaFlex, whose processor comes from
    ``google/siglip2-base-patch16-naflex``.
    """

    def __init__(
        self,
        env_path: Path | None = None,
        output_callback: Callable[[str], None] | None = None,
        detector: str = DEFAULT_DETECTOR,
        detectors: Sequence[str] = (),
    ) -> None:
        super().__init__(env_path=env_path, output_callback=output_callback)
        self.detector = get_detector(detector).id
        self.detectors: tuple[str, ...] = (self.detector,)
        if detectors:
            self.set_detectors(detectors)

    def set_detectors(self, detectors: Sequence[str]) -> None:
        """Replace the detectors this environment is for.

        Both fields move together. ``detectors`` is what ``has_detector()``
        and ``install_detector()`` iterate, and setting only ``detector``
        left the readiness check testing the one before the change.
        """
        wanted = tuple(get_detector(d).id for d in detectors)
        self.detectors = wanted or (self.detector,)
        self.detector = self.detectors[0]

    @classmethod
    def get_default_env_path(cls) -> Path:
        """Resolve the engines env path: env override, project dir, then home."""
        env_override = os.environ.get("MOKURO_BUNKO_OCR_ENGINES_ENV")
        if env_override:
            return Path(env_override).expanduser()

        project_root = cls._discover_project_root()
        if project_root is not None:
            return project_root / ".ocr-engines-env"

        return Path.home() / ".mokuro-bunko" / "ocr-engines-env"

    def _probe(self, snippet: str) -> bool:
        python_path = self._get_python_path()
        if not python_path.exists():
            return False
        try:
            result = subprocess.run(
                [str(python_path), "-c", snippet],
                capture_output=True,
                timeout=60,
            )
            return result.returncode == 0
        except (subprocess.TimeoutExpired, subprocess.SubprocessError):
            return False

    def is_installed(self) -> bool:
        """True when the env exists and the engine packages import.

        Not ``hayai_ocr``: it is no longer installed (the v2 engine is gone),
        so probing for it would report every freshly built environment as
        missing.
        """
        return self._probe("import transformers, peft, cv2, natsort")

    def has_detector(self, detector: str | None = None) -> bool:
        """True when the given detector's extra packages are importable.

        Without an argument: every detector this environment is for (the
        configured one and those built into its engines).
        """
        if detector is None:
            return all(self.has_detector(d) for d in self.detectors)
        spec = get_detector(detector)
        if spec.probe_import is None:
            return True
        return self._probe(f"import {spec.probe_import}")

    def install_detector(self, detector: str | None = None) -> bool:
        """Install one detector's extra packages into an existing env.

        Without an argument: the extras of every detector this environment
        is for.
        """
        if detector is None:
            return all(self.install_detector(d) for d in self.detectors)
        spec = get_detector(detector)
        if not spec.extra_packages:
            return True
        pip_path = self._get_pip_path()
        if not pip_path.exists():
            self._log("Pip not found in environment")
            return False
        self._log(
            f"Installing detector '{spec.id}' extras ({spec.license}): "
            f"{' '.join(spec.extra_packages)}"
        )
        return self._run_pip([str(pip_path), "install", *spec.extra_packages])

    def install_mokuro(self) -> bool:
        """Install the engine package set (name kept so ``install`` is reused)."""
        pip_path = self._get_pip_path()
        if not pip_path.exists():
            self._log("Pip not found in environment")
            return False

        extras: list[str] = []
        for detector_id in self.detectors:
            for package in get_detector(detector_id).extra_packages:
                if package not in extras:
                    extras.append(package)
        self._log(
            "Installing OCR engines (hayai-ocr Nova, PaddleOCR-VL support; "
            f"detector={', '.join(self.detectors)})..."
        )
        cmd = [str(pip_path), "install", *ENGINES_ENV_PACKAGES, *extras]
        return self._run_pip(cmd)

    _VERIFY_SNIPPET = _ROCM_PRELUDE + """
import importlib.metadata as md

problems = []

try:
    import torch
    _override = apply_for_torch(torch)
    if _override:
        print(f"ROCm: this card is not in the torch build; HSA_OVERRIDE_GFX_VERSION={_override}")
    line = f"torch {torch.__version__}, cuda available: {torch.cuda.is_available()}"
    if torch.cuda.is_available():
        line += f" ({torch.cuda.get_device_name(0)})"
        # "Available" is not "computes": an unbuilt AMD target passes the
        # line above and dumps core at its first kernel.
        _x = torch.randn(64, 64, device="cuda")
        if not bool(torch.isfinite((_x @ _x).sum()).item()):
            problems.append("a matrix multiply on the GPU returned a non-finite result")
    print(line)
except Exception as e:
    problems.append(f"torch failed on this machine: {e}")

try:
    version = md.version("transformers")
    major = int(version.split(".")[0])
    print(f"transformers {version}")
    if major < 5:
        problems.append(
            f"transformers {version} is too old for PaddleOCR-VL-1.6 (needs >=5); "
            "reinstall with: mokuro-bunko install-ocr --engines hayai-nova,paddle-manga --force"
        )
except Exception as e:
    problems.append(f"transformers check failed: {e}")

for module in ("peft", "cv2", "natsort", "PIL"):
    try:
        __import__(module)
        print(f"{module} ok")
    except Exception as e:
        problems.append(f"{module} import failed: {e}")

for problem in problems:
    print(f"PROBLEM: {problem}")
"""


def prompt_for_backend(hardware: HardwareInfo) -> OCRBackend:
    """Interactively prompt user for backend selection.

    Args:
        hardware: Hardware detection results.

    Returns:
        Selected OCR backend.
    """
    supported = get_supported_backends(hardware=hardware)
    unavailable_reasons = get_backend_unavailable_reasons(hardware=hardware)
    recommended = get_recommended_backend(hardware, supported_backends=supported)

    print("\nOCR Backend Selection")
    print("=" * 40)

    options: list[tuple[OCRBackend, str]] = []
    if OCRBackend.CUDA in supported:
        cuda_info = f"(CUDA {hardware.cuda_version})" if hardware.cuda_version else ""
        options.append((OCRBackend.CUDA, f"CUDA {cuda_info}"))
    if OCRBackend.ROCM in supported:
        rocm_info = f"(ROCm {hardware.rocm_version})" if hardware.rocm_version else ""
        options.append((OCRBackend.ROCM, f"ROCm {rocm_info}"))
    if OCRBackend.MPS in supported:
        options.append((OCRBackend.MPS, "MPS (Apple Silicon)"))
    options.append((OCRBackend.CPU, "CPU (slower, but always works)"))
    options.append((OCRBackend.SKIP, "Skip OCR installation"))

    for i, (backend, description) in enumerate(options, 1):
        rec = " (Recommended)" if backend == recommended else ""
        print(f"  [{i}] {description}{rec}")

    unavailable = [b for b in (OCRBackend.CUDA, OCRBackend.ROCM, OCRBackend.MPS) if b not in supported]
    if unavailable:
        print("\nUnavailable on this host/runtime:")
        for option in unavailable:
            reason = unavailable_reasons.get(option, "Unavailable")
            print(f"  - {option.value}: {reason}")

    print()

    while True:
        try:
            choice = input("Choice [1]: ").strip()
            if not choice:
                return recommended

            idx = int(choice) - 1
            if 0 <= idx < len(options):
                return options[idx][0]
            print(f"Please enter a number between 1 and {len(options)}")
        except ValueError:
            print("Please enter a valid number")
        except (EOFError, KeyboardInterrupt):
            print("\nInstallation cancelled")
            return OCRBackend.SKIP
