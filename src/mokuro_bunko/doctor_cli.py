"""Diagnostics CLI for mokuro-bunko.

``mokuro-bunko doctor`` runs a series of environment checks and prints a
PASS/WARN/FAIL table with fix hints, so stuck users (and setup scripts) can
find out what is wrong without spelunking through logs.
"""

from __future__ import annotations

import json
import shutil
import socket
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Literal

import click

if TYPE_CHECKING:
    from mokuro_bunko.config import Config

Status = Literal["PASS", "WARN", "FAIL"]

_STATUS_COLORS: dict[Status, str] = {"PASS": "green", "WARN": "yellow", "FAIL": "red"}

# Rough size of a CUDA OCR environment plus model cache.
_LOW_DISK_BYTES = 10 * 1024**3


@dataclass
class CheckResult:
    """Outcome of a single doctor check."""

    status: Status
    label: str
    detail: str
    hint: str | None = None


def _check_python() -> CheckResult:
    """Interpreter version; CUDA wheels need < 3.13."""
    version = f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}"
    if sys.version_info >= (3, 13):
        return CheckResult(
            "WARN",
            "Python",
            f"{version} - CUDA OCR wheels are unavailable on Python >= 3.13",
            "Pin Python 3.12 (the repo's .python-version does this for uv installs).",
        )
    return CheckResult("PASS", "Python", version)


def _check_config(config_path: Path | None) -> tuple[CheckResult, Config | None]:
    """Load config and probe storage writability."""
    from mokuro_bunko.config import get_default_config_path, load_config

    shown_path = config_path or get_default_config_path()
    try:
        config = load_config(config_path)
    except Exception as e:
        return (
            CheckResult(
                "FAIL",
                "Config",
                f"{shown_path}: {e}",
                "Fix or delete the config file, then re-run 'mokuro-bunko setup'.",
            ),
            None,
        )

    base = config.storage.base_path
    try:
        config.storage.ensure_directories()
        probe = base / ".mokuro-doctor-probe"
        probe.write_text("ok", encoding="utf-8")
        probe.unlink()
    except OSError as e:
        return (
            CheckResult(
                "FAIL",
                "Storage",
                f"{base} is not writable: {e}",
                "Point storage.base_path at a writable directory.",
            ),
            config,
        )

    exists = "" if shown_path.exists() else " (not found; using defaults)"
    return (
        CheckResult("PASS", "Config", f"{shown_path}{exists} - storage: {base}"),
        config,
    )


def _check_nvidia() -> CheckResult:
    """NVIDIA driver presence via nvidia-smi."""
    try:
        result = subprocess.run(
            ["nvidia-smi", "--query-gpu=name,driver_version", "--format=csv,noheader"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0 and result.stdout.strip():
            return CheckResult("PASS", "NVIDIA driver", result.stdout.strip().splitlines()[0])
    except (FileNotFoundError, subprocess.TimeoutExpired, subprocess.SubprocessError):
        pass
    return CheckResult(
        "WARN",
        "NVIDIA driver",
        "nvidia-smi not found - GPU OCR unavailable, CPU backend will be used",
        "Install the NVIDIA driver if this machine has an NVIDIA GPU.",
    )


def _check_ocr_env() -> list[CheckResult]:
    """OCR environment existence + full stack verification."""
    from mokuro_bunko.ocr.installer import OCRInstaller

    installer = OCRInstaller()
    if installer.get_python_executable() is None:
        return [
            CheckResult(
                "WARN",
                "OCR environment",
                f"not installed (expected at {installer.env_path})",
                "Run: mokuro-bunko install-ocr   (or start the server once; it installs on launch)",
            )
        ]

    results = [CheckResult("PASS", "OCR environment", str(installer.env_path))]
    ok, lines = installer.verify_installation()
    if ok:
        results.append(CheckResult("PASS", "OCR stack", "; ".join(lines)))
    else:
        results.append(
            CheckResult(
                "FAIL",
                "OCR stack",
                "; ".join(lines),
                "Run: mokuro-bunko install-ocr --force",
            )
        )
    return results


def _check_disk(storage_path: Path) -> CheckResult:
    """Free disk space at the storage location."""
    try:
        usage = shutil.disk_usage(storage_path)
    except OSError as e:
        return CheckResult("WARN", "Disk space", f"could not check: {e}")
    free_gb = usage.free / 1024**3
    if usage.free < _LOW_DISK_BYTES:
        return CheckResult(
            "WARN",
            "Disk space",
            f"{free_gb:.1f} GB free at {storage_path}",
            "A CUDA OCR environment plus models needs ~8-10 GB.",
        )
    return CheckResult("PASS", "Disk space", f"{free_gb:.1f} GB free at {storage_path}")


def _check_port(host: str, port: int) -> CheckResult:
    """Whether the configured port can be bound."""
    probe_host = "127.0.0.1" if host == "0.0.0.0" else host
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
            sock.bind((probe_host, port))
        return CheckResult("PASS", "Port", f"{port} available on {probe_host}")
    except OSError:
        return CheckResult(
            "WARN",
            "Port",
            f"{port} is in use on {probe_host} - is the server already running?",
            "Stop the other process or change server.port in the config.",
        )


def _check_failures(storage_path: Path) -> CheckResult:
    """Persisted OCR failure records."""
    failures_path = storage_path / ".ocr-failures.json"
    try:
        data = json.loads(failures_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return CheckResult("PASS", "Failed volumes", "none recorded")
    count = len(data) if isinstance(data, dict) else 0
    if count == 0:
        return CheckResult("PASS", "Failed volumes", "none recorded")
    logs_dir = storage_path / "logs" / "ocr"
    return CheckResult(
        "WARN",
        "Failed volumes",
        f"{count} volume(s) failing OCR (see the Queue page)",
        f"Full per-volume logs: {logs_dir}",
    )


def _print_result(result: CheckResult) -> None:
    """Print one aligned, colored result row."""
    status = click.style(f"{result.status:<4}", fg=_STATUS_COLORS[result.status], bold=True)
    click.echo(f" {status}  {result.label}: {result.detail}")
    if result.hint and result.status != "PASS":
        click.echo(f"        -> {result.hint}")


@click.command("doctor")
@click.pass_context
def doctor_command(ctx: click.Context) -> None:
    """Diagnose common installation and OCR problems."""
    from mokuro_bunko import __version__

    config_path = ctx.obj.get("config_path") if ctx.obj else None

    click.echo(f"mokuro-bunko {__version__} - environment diagnostics\n")

    results: list[CheckResult] = []
    results.append(_check_python())

    config_result, config = _check_config(config_path)
    results.append(config_result)

    results.append(_check_nvidia())
    results.extend(_check_ocr_env())

    if config is not None:
        storage = config.storage.base_path
        results.append(_check_disk(storage))
        results.append(_check_port(config.server.host, config.server.port))
        results.append(_check_failures(storage))

    for result in results:
        _print_result(result)

    failures = [r for r in results if r.status == "FAIL"]
    warnings = [r for r in results if r.status == "WARN"]
    click.echo()
    if failures:
        click.echo(
            click.style(
                f"{len(failures)} problem(s) found - see FAIL lines above.", fg="red", bold=True
            )
        )
        sys.exit(1)
    if warnings:
        click.echo(
            click.style(
                f"OK with {len(warnings)} warning(s) - see WARN lines above.", fg="yellow"
            )
        )
        return
    click.echo(click.style("All checks passed.", fg="green", bold=True))
