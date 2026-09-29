"""`mokuro-bunko processor service`: a systemd USER unit for this install.

A processor on a GPU box somebody uses under their own account runs as that
user, from wherever they installed it -- so the unit is written from the
running install's own paths, never guessed (docs: deployment.md step 4).
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest
from click.testing import CliRunner

from mokuro_bunko.processor import service
from mokuro_bunko.processor.cli import processor_group


@pytest.fixture
def config_file(tmp_path: Path) -> Path:
    path = tmp_path / "processor.yaml"
    path.write_text(
        "library:\n  url: http://library.example:8080\n  username: gpu-box\n"
        "  password: secret-password\nprocessor:\n  name: gpu-box\n"
        f"  storage: {tmp_path / 'storage'}\n",
        encoding="utf-8",
    )
    path.chmod(0o600)
    return path


def test_the_unit_runs_this_install_with_the_absolute_config(config_file: Path) -> None:
    unit = service.render_user_unit(
        config_file, entry_point=Path("/home/me/mokuro-bunko/.venv/bin/mokuro-bunko"), env={}
    )
    assert (
        "ExecStart=/home/me/mokuro-bunko/.venv/bin/mokuro-bunko processor serve "
        f"--config {config_file.resolve()}" in unit
    )
    assert "WantedBy=default.target" in unit
    assert "Restart=on-failure" in unit
    # No User=: a user unit runs as its owner.
    assert "User=" not in unit


def test_a_custom_environment_location_is_carried_into_the_unit(config_file: Path) -> None:
    unit = service.render_user_unit(
        config_file,
        entry_point=Path("/opt/x/bin/mokuro-bunko"),
        env={"MOKURO_BUNKO_OCR_ENV": "/data/ocr-env", "HOME": "/home/me", "PATH": "/usr/bin"},
    )
    assert 'Environment="MOKURO_BUNKO_OCR_ENV=/data/ocr-env"' in unit
    # Only the variables that decide where OCR lives travel, not the shell's.
    assert "HOME=" not in unit and "PATH=" not in unit


def test_a_path_with_spaces_is_quoted(tmp_path: Path) -> None:
    folder = tmp_path / "my stuff"
    folder.mkdir()
    config = folder / "processor.yaml"
    config.write_text("library: {}\n", encoding="utf-8")
    unit = service.render_user_unit(config, entry_point=Path("/a b/mokuro-bunko"), env={})
    assert f'ExecStart="/a b/mokuro-bunko" processor serve --config "{config.resolve()}"' in unit


def test_the_entry_point_is_the_running_install_s(tmp_path: Path) -> None:
    bin_dir = tmp_path / "venv" / "bin"
    bin_dir.mkdir(parents=True)
    (bin_dir / "mokuro-bunko").write_text("#!/bin/sh\n", encoding="utf-8")
    assert service.entry_point(bin_dir / "python") == bin_dir / "mokuro-bunko"
    with pytest.raises(service.ServiceError):
        service.entry_point(tmp_path / "elsewhere" / "python")


def test_the_command_prints_the_unit_by_default(config_file: Path) -> None:
    result = CliRunner().invoke(processor_group, ["service", "--config", str(config_file)])
    assert result.exit_code == 0, result.output
    assert "[Service]" in result.output
    assert f"--config {config_file.resolve()}" in result.output


def test_install_writes_the_unit_enables_it_and_says_how_to_keep_it_running(
    config_file: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    calls: list[list[str]] = []

    def fake_run(cmd: list[str], **_kwargs: Any) -> subprocess.CompletedProcess[str]:
        calls.append(cmd)
        out = "Linger=no\n" if cmd[:2] == ["loginctl", "show-user"] else ""
        return subprocess.CompletedProcess(cmd, 0, out, "")

    monkeypatch.setattr(service.subprocess, "run", fake_run)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "xdg"))
    result = CliRunner().invoke(
        processor_group, ["service", "--config", str(config_file), "--install"]
    )
    assert result.exit_code == 0, result.output
    unit_path = tmp_path / "xdg" / "systemd" / "user" / service.UNIT_NAME
    assert unit_path.is_file()
    assert f"--config {config_file.resolve()}" in unit_path.read_text(encoding="utf-8")
    assert ["systemctl", "--user", "daemon-reload"] in calls
    assert ["systemctl", "--user", "enable", "--now", service.UNIT_NAME] in calls
    # Without lingering, a user service stops at logout and does not start at boot.
    assert "loginctl enable-linger" in result.output


def test_install_says_nothing_about_lingering_when_it_is_on(
    config_file: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def fake_run(cmd: list[str], **_kwargs: Any) -> subprocess.CompletedProcess[str]:
        out = "Linger=yes\n" if cmd[:2] == ["loginctl", "show-user"] else ""
        return subprocess.CompletedProcess(cmd, 0, out, "")

    monkeypatch.setattr(service.subprocess, "run", fake_run)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "xdg"))
    result = CliRunner().invoke(
        processor_group, ["service", "--config", str(config_file), "--install"]
    )
    assert result.exit_code == 0, result.output
    assert "enable-linger" not in result.output


def test_install_reports_a_systemctl_failure(
    config_file: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def fake_run(cmd: list[str], **_kwargs: Any) -> subprocess.CompletedProcess[str]:
        if cmd[:3] == ["systemctl", "--user", "enable"]:
            return subprocess.CompletedProcess(cmd, 1, "", "Failed to connect to bus")
        return subprocess.CompletedProcess(cmd, 0, "Linger=yes\n", "")

    monkeypatch.setattr(service.subprocess, "run", fake_run)
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "xdg"))
    result = CliRunner().invoke(
        processor_group, ["service", "--config", str(config_file), "--install"]
    )
    assert result.exit_code != 0
    assert "Failed to connect to bus" in result.output


def test_a_missing_config_is_refused(tmp_path: Path) -> None:
    result = CliRunner().invoke(
        processor_group, ["service", "--config", str(tmp_path / "nope.yaml")]
    )
    assert result.exit_code != 0


class TestSpecialCharacters:
    """systemd expands `%` specifiers and `$VARS` in ExecStart= and
    Environment=, and a .cmd file expands `%VAR%`: a path holding one would
    run something other than what was written. A newline would start a new
    directive. Both are escaped or refused, never passed through."""

    def test_percent_and_dollar_reach_systemd_literally(self, tmp_path: Path) -> None:
        folder = tmp_path / "100%$HOME"
        folder.mkdir()
        config = folder / "processor.yaml"
        config.write_text("library: {}\n", encoding="utf-8")
        text = service.render_user_unit(
            config,
            entry_point=Path("/opt/a%h/mokuro-bunko"),
            env={"HF_HOME": "/data/50%$x"},
        )
        assert "/opt/a%%h/mokuro-bunko" in text
        assert f"{folder.resolve()}".replace("%", "%%").replace("$", "$$") in text
        assert 'Environment="HF_HOME=/data/50%%$$x"' in text

    def test_a_newline_in_a_path_is_refused(self, tmp_path: Path) -> None:
        with pytest.raises(service.ServiceError):
            service.render_user_unit(
                tmp_path / "processor.yaml",
                entry_point=Path("/opt/x\nExecStartPre=/bin/evil"),
                env={},
            )

    def test_a_newline_in_a_carried_variable_is_refused(self, tmp_path: Path) -> None:
        with pytest.raises(service.ServiceError):
            service.render_user_unit(
                tmp_path / "processor.yaml",
                entry_point=Path("/opt/mokuro-bunko"),
                env={"HF_HOME": "/data\nExecStartPre=/bin/evil"},
            )

    def test_percent_reaches_cmd_literally(self, tmp_path: Path) -> None:
        text = service.render_windows_startup(
            tmp_path / "processor.yaml", entry_point=Path(r"C:\100%PATH%\mokuro-bunko.exe")
        )
        assert r'"C:\100%%PATH%%\mokuro-bunko.exe"' in text

    def test_a_newline_is_refused_for_windows_too(self, tmp_path: Path) -> None:
        with pytest.raises(service.ServiceError):
            service.render_windows_startup(
                tmp_path / "processor.yaml", entry_point=Path("C:\\x\r\ncalc.exe")
            )
