"""A GPU install that fails is retried once, then falls back LOUDLY.

Measured on Windows with an RTX 3070: the CUDA torch wheel arrived damaged
("Bad CRC-32 for file 'torch/lib/cufft64_12.dll'"), the installer fell back
to CPU in one log line among thousands and reported success -- a GPU machine
running OCR on its CPU, and nothing said so. And every install printed pip's
"ERROR: To modify pip..." because pip was asked to upgrade itself through
pip.exe, which Windows forbids.
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.installer import OCRBackend, OCRInstaller


class _Recorder(OCRInstaller):
    def __init__(self, env_path: Path, outcomes: list[bool]) -> None:
        self.lines: list[str] = []
        super().__init__(env_path=env_path, output_callback=self.lines.append)
        self.outcomes = outcomes
        self.calls: list[tuple[OCRBackend, bool]] = []

    def install(self, backend: OCRBackend, force: bool = False, hardware: Any = None) -> bool:
        self.calls.append((backend, os.environ.get("PIP_NO_CACHE_DIR") == "1"))
        return self.outcomes.pop(0)


def test_a_failed_gpu_install_is_retried_once_without_the_download_cache(tmp_path: Path) -> None:
    installer = _Recorder(tmp_path / "env", [False, True])
    assert installer.install_with_fallback(OCRBackend.CUDA)
    assert installer.calls == [(OCRBackend.CUDA, False), (OCRBackend.CUDA, True)]
    assert installer.fell_back_from is None
    assert os.environ.get("PIP_NO_CACHE_DIR") != "1"  # restored


def test_a_fallback_to_cpu_is_said_loudly_with_the_fix(tmp_path: Path) -> None:
    installer = _Recorder(tmp_path / "env", [False, False, True])
    assert installer.install_with_fallback(OCRBackend.CUDA)
    assert [backend for backend, _ in installer.calls] == [
        OCRBackend.CUDA, OCRBackend.CUDA, OCRBackend.CPU,
    ]
    assert installer.fell_back_from == OCRBackend.CUDA
    loud = "\n".join(installer.lines)
    assert "WARNING" in loud and "CPU" in loud and "cuda" in loud
    assert "--force" in loud


def test_a_cpu_install_is_not_retried(tmp_path: Path) -> None:
    installer = _Recorder(tmp_path / "env", [False])
    assert not installer.install_with_fallback(OCRBackend.CPU)
    assert installer.calls == [(OCRBackend.CPU, False)]


def test_pip_upgrades_itself_through_the_interpreter(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    installer = OCRInstaller(env_path=tmp_path / "env", output_callback=lambda _l: None)
    commands: list[list[str]] = []
    monkeypatch.setattr(installer, "create_environment", lambda force=False: True)
    monkeypatch.setattr(installer, "_run_pip", lambda cmd: commands.append(cmd) or False)
    installer.install(OCRBackend.CPU)
    upgrade = next(cmd for cmd in commands if "--upgrade" in cmd and "pip" in cmd[-1:])
    assert upgrade[1:3] == ["-m", "pip"], upgrade
    assert Path(upgrade[0]).name.startswith("python")
