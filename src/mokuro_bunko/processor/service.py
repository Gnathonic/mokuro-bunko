"""A systemd USER unit that runs this install's processor.

The common processor is a GPU box somebody also uses under their own account,
installed from a source checkout in their home. The system unit in
``deploy/`` wants root, a ``mokuro`` user and ``/usr/local/bin`` -- none of
which that install has -- and a hand-written user unit means guessing
absolute paths. So the unit is written from the running install itself: its
own ``mokuro-bunko`` entry point, the absolute path of the processor.yaml it
was given, and the environment variables that say where its OCR lives.
"""

from __future__ import annotations

import os
import subprocess
import sys
from collections.abc import Mapping
from pathlib import Path

UNIT_NAME = "mokuro-bunko-processor.service"
#: Windows: a command file in the user's Startup folder (no admin needed).
STARTUP_NAME = "mokuro-bunko-processor.cmd"

#: The variables that decide where the OCR environments and model caches
#: are: a unit that dropped them would run another install's OCR.
CARRIED_ENV = ("MOKURO_BUNKO_OCR_ENV", "MOKURO_BUNKO_OCR_ENGINES_ENV", "HF_HOME")


class ServiceError(Exception):
    """The unit cannot be written or enabled; the message says why."""


def entry_point(python: Path | None = None) -> Path:
    """The ``mokuro-bunko`` script beside the interpreter running this code."""
    folder = Path(python or sys.executable).parent
    # The console script is `mokuro-bunko` on POSIX, `mokuro-bunko.exe` on Windows.
    for name in ("mokuro-bunko", "mokuro-bunko.exe"):
        candidate = folder / name
        if candidate.is_file():
            return candidate
    raise ServiceError(
        f"no mokuro-bunko entry point beside {python or sys.executable}; "
        "run this command from the install the service should use "
        "(in a source checkout: uv run mokuro-bunko processor service ...)"
    )


def _one_line(value: str) -> str:
    """``value``, refused if it would break out of its line."""
    if "\n" in value or "\r" in value:
        raise ServiceError(f"{value!r} holds a line break; move it to a path without one")
    return value


def _systemd_literal(value: str) -> str:
    """``value`` with systemd's own expansions escaped: ``%`` specifiers and
    ``$`` variables would otherwise run something other than what was written."""
    return _one_line(value).replace("%", "%%").replace("$", "$$")


def _quote(value: str) -> str:
    """One ExecStart word, quoted the way systemd reads it."""
    value = _systemd_literal(value)
    if any(ch.isspace() for ch in value) or '"' in value:
        return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'
    return value


def _env_value(value: str) -> str:
    """One Environment= value: systemd-escaped, and quoted as one assignment."""
    return _systemd_literal(value).replace("\\", "\\\\").replace('"', '\\"')


def render_user_unit(
    config: Path, *, entry_point: Path, env: Mapping[str, str] | None = None
) -> str:
    """The unit text. ``env`` defaults to this process's environment."""
    source = os.environ if env is None else env
    environment = "".join(
        f'Environment="{name}={_env_value(source[name])}"\n'
        for name in CARRIED_ENV
        if source.get(name)
    )
    exec_start = " ".join(
        [_quote(str(entry_point)), "processor", "serve", "--config", _quote(str(config.resolve()))]
    )
    return (
        "[Unit]\n"
        "Description=Mokuro Bunko OCR processor\n"
        "Documentation=https://github.com/Gnathonic/mokuro-bunko/blob/main/docs/deployment.md"
        "#remote-ocr-processors\n"
        "# The processor dials out to the library; it needs the network, not a port.\n"
        "Wants=network-online.target\n"
        "After=network-online.target\n"
        "\n"
        "[Service]\n"
        "Type=simple\n"
        f"{environment}"
        f"ExecStart={exec_start}\n"
        "# SIGTERM stops it cleanly: the volumes it held go back to the library's\n"
        "# queue unrecorded. It reconnects by itself when the library restarts.\n"
        "Restart=on-failure\n"
        "RestartSec=10s\n"
        "TimeoutStopSec=60s\n"
        "\n"
        "[Install]\n"
        "WantedBy=default.target\n"
    )


def user_unit_dir() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME") or str(Path.home() / ".config")
    return Path(base) / "systemd" / "user"


def _run(cmd: list[str]) -> subprocess.CompletedProcess[str]:
    return subprocess.run(cmd, capture_output=True, text=True, check=False)


def install_user_unit(unit_text: str) -> tuple[Path, bool]:
    """Write the unit, reload the user manager and enable + start it.

    Returns the unit's path and whether the account lingers (a user service
    of an account that does not linger stops at logout and does not start
    at boot).
    """
    unit_dir = user_unit_dir()
    unit_dir.mkdir(parents=True, exist_ok=True)
    path = unit_dir / UNIT_NAME
    path.write_text(unit_text, encoding="utf-8")
    for cmd in (
        ["systemctl", "--user", "daemon-reload"],
        ["systemctl", "--user", "enable", "--now", UNIT_NAME],
    ):
        done = _run(cmd)
        if done.returncode != 0:
            raise ServiceError(
                f"`{' '.join(cmd)}` failed: {(done.stderr or done.stdout).strip()}"
            )
    user = os.environ.get("USER") or Path.home().name
    linger = _run(["loginctl", "show-user", user, "-p", "Linger"])
    return path, "Linger=yes" in (linger.stdout or "")


# -- Windows ------------------------------------------------------------------
#
# A logon task (schtasks /SC ONLOGON) usually needs an administrator; the
# user's Startup folder does not, runs for exactly this user at every logon,
# and is removed by deleting one file. The processor starts in its own
# minimized console window, so its output stays readable.


def _cmd_quote(value: str) -> str:
    # `%` doubled: a .cmd file expands `%VAR%` even inside quotes.
    return '"' + _one_line(value).replace("%", "%%").replace('"', '""') + '"'


def render_windows_startup(config: Path, *, entry_point: Path) -> str:
    """The Startup entry's text (CRLF, as cmd expects)."""
    return (
        "@echo off\r\n"
        "rem mokuro-bunko OCR processor: started at logon. Delete this file to stop that.\r\n"
        'start "mokuro-bunko processor" /min '
        f"{_cmd_quote(str(entry_point))} processor serve --config "
        f"{_cmd_quote(str(Path(config).resolve()))}\r\n"
    )


def startup_dir() -> Path:
    base = os.environ.get("APPDATA") or str(Path.home() / "AppData" / "Roaming")
    return Path(base) / "Microsoft" / "Windows" / "Start Menu" / "Programs" / "Startup"


def install_windows_startup(config: Path, *, entry_point: Path) -> Path:
    """Write the Startup entry and start it now; returns its path."""
    folder = startup_dir()
    folder.mkdir(parents=True, exist_ok=True)
    path = folder / STARTUP_NAME
    path.write_bytes(render_windows_startup(config, entry_point=entry_point).encode("utf-8"))
    flags = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) | getattr(
        subprocess, "DETACHED_PROCESS", 0
    )
    try:
        subprocess.Popen(["cmd", "/c", str(path)], creationflags=flags, close_fds=True)
    except OSError as e:
        raise ServiceError(f"wrote {path} but could not start it now: {e}") from e
    return path
