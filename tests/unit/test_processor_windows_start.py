"""Windows: the processor starts at logon from a Startup entry (no admin),
and the wizard restricts processor.yaml to the real account.

Seen on Windows 11: USERDOMAIN said WORKGROUP, so icacls was asked for
"WORKGROUP\\alice" (no such account), failed, left the file open, and the warning
named the wizard's temporary file instead of processor.yaml."""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.processor import service, setup


def test_the_startup_entry_runs_this_install_minimized(tmp_path: Path) -> None:
    config = tmp_path / "my stuff" / "processor.yaml"
    config.parent.mkdir()
    config.write_text("library: {}\n", encoding="utf-8")
    text = service.render_windows_startup(
        config, entry_point=Path(r"C:\Users\alice\mokuro-bunko\.venv\Scripts\mokuro-bunko.exe")
    )
    assert text.startswith("@echo off\r\n")
    assert 'start "mokuro-bunko processor" /min' in text
    assert r'"C:\Users\alice\mokuro-bunko\.venv\Scripts\mokuro-bunko.exe" processor serve' in text
    assert f'--config "{config.resolve()}"' in text


def test_install_writes_it_to_the_user_s_startup_folder(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv("APPDATA", str(tmp_path / "Roaming"))
    config = tmp_path / "processor.yaml"
    config.write_text("library: {}\n", encoding="utf-8")
    started: list[Any] = []
    monkeypatch.setattr(service.subprocess, "Popen", lambda *a, **k: started.append(a))
    path = service.install_windows_startup(config, entry_point=Path(r"C:\x\mokuro-bunko.exe"))
    assert path == (tmp_path / "Roaming" / "Microsoft" / "Windows" / "Start Menu" / "Programs"
                    / "Startup" / service.STARTUP_NAME)
    assert "processor serve" in path.read_text(encoding="utf-8")
    assert started, "and started now, not only at the next logon"


def test_icacls_is_asked_for_the_real_account(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[list[str]] = []

    def fake_run(cmd: list[str], **_kwargs: Any) -> subprocess.CompletedProcess[str]:
        calls.append(cmd)
        if cmd[0] == "whoami":
            return subprocess.CompletedProcess(cmd, 0, "box\\alice\r\n", "")
        return subprocess.CompletedProcess(cmd, 0, "", "")

    monkeypatch.setattr(setup.sys, "platform", "win32")
    monkeypatch.setenv("USERDOMAIN", "WORKGROUP")
    monkeypatch.setenv("USERNAME", "red")
    monkeypatch.setattr(setup.subprocess, "run", fake_run)
    staged = tmp_path / ".processor.yaml.abc.tmp"
    staged.write_text("x", encoding="utf-8")
    assert setup._restrict_to_owner(staged, shown=tmp_path / "processor.yaml") is None
    icacls = next(cmd for cmd in calls if cmd[0] == "icacls")
    assert "box\\alice:F" in icacls


def test_a_failed_restriction_names_the_real_file(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(setup.sys, "platform", "win32")
    monkeypatch.setattr(
        setup.subprocess, "run",
        lambda cmd, **k: subprocess.CompletedProcess(cmd, 1, "", "denied"),
    )
    staged = tmp_path / ".processor.yaml.abc.tmp"
    staged.write_text("x", encoding="utf-8")
    warning = setup._restrict_to_owner(staged, shown=tmp_path / "processor.yaml")
    assert warning is not None
    assert str(tmp_path / "processor.yaml") in warning and ".tmp" not in warning


def test_the_wizard_offers_a_start_at_logon_on_windows(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    monkeypatch.setattr(setup.sys, "platform", "win32")
    monkeypatch.setattr(service, "entry_point", lambda python=None: tmp_path / "mokuro-bunko.exe")
    assert setup.user_service_supported() is True


def test_the_entry_point_is_found_as_an_exe_on_windows(tmp_path: Path) -> None:
    """Seen on Windows 11: `mokuro-bunko.exe` sat beside python.exe, and the
    lookup for `mokuro-bunko` alone said there was no entry point -- so the
    wizard never offered the start-at-logon step."""
    scripts = tmp_path / "Scripts"
    scripts.mkdir()
    (scripts / "mokuro-bunko.exe").write_bytes(b"MZ")
    assert service.entry_point(scripts / "python.exe") == scripts / "mokuro-bunko.exe"


def test_service_without_install_prints_the_startup_entry_on_windows(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    from click.testing import CliRunner

    from mokuro_bunko.processor import cli

    config = tmp_path / "processor.yaml"
    config.write_text(
        "library:\n  url: http://library.example:8080\n  username: gpu-box\n"
        "  password: secret-password\n", encoding="utf-8",
    )
    monkeypatch.setattr(cli.sys, "platform", "win32")
    monkeypatch.setenv("APPDATA", str(tmp_path / "Roaming"))
    monkeypatch.setattr(service, "entry_point", lambda python=None: Path(r"C:\x\mokuro-bunko.exe"))
    result = CliRunner().invoke(cli.processor_group, ["service", "--config", str(config)])
    assert result.exit_code == 0, result.output
    assert service.STARTUP_NAME in result.output
    assert "processor serve" in result.output and "[Service]" not in result.output
