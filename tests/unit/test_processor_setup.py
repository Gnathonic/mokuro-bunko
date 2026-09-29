"""`mokuro-bunko processor setup`: check the account, write the config, go.

The library here is the real front of one -- `LoginAPI` over `AuthMiddleware`
over `ProcessorAPI`, with a real account database -- served on port 0, so
every check runs against the endpoints a real library answers with.
"""

from __future__ import annotations

import socket
import ssl
import stat
import subprocess
import threading
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any
from wsgiref.simple_server import WSGIRequestHandler, WSGIServer, make_server

import pytest
import yaml
from click.testing import CliRunner, Result

from mokuro_bunko import __version__
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.remote import library_api
from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.processor import cli as processor_cli
from mokuro_bunko.processor import config as processor_config
from mokuro_bunko.processor import service
from mokuro_bunko.processor import setup as wizard
from mokuro_bunko.processor.cli import processor_group
from mokuro_bunko.processor.config import load_processor_config
from mokuro_bunko.security import AuthAttemptLimiter

PASSWORD = "s3cret: #not a comment"
HOSTNAME = "this-host"
# The real steps, before the autouse fixture stubs them.
REAL_INSTALL_SERVICE = processor_cli.install_service
REAL_DETECT_HARDWARE = wizard.detect_hardware


class _Quiet(WSGIRequestHandler):
    def log_message(self, *args: Any) -> None:
        pass


@dataclass
class Library:
    url: str
    registry: ProcessorRegistry
    requests: list[tuple[str, str]] = field(default_factory=list)


def _serve(
    app: Callable[..., Any], context: ssl.SSLContext | None = None
) -> tuple[WSGIServer, int]:
    server = make_server("127.0.0.1", 0, app, handler_class=_Quiet)
    if context is not None:
        server.socket = context.wrap_socket(server.socket, server_side=True)
    threading.Thread(target=server.serve_forever, args=(0.05,), daemon=True).start()
    return server, server.server_address[1]


def _stack(tmp_path: Path, inner: Callable[..., Any] | None = None) -> tuple[Any, Library]:
    from mokuro_bunko.login.api import LoginAPI
    from mokuro_bunko.middleware.auth import AuthMiddleware

    db = Database(tmp_path / "library.db")
    db.create_user("gpu-box", PASSWORD, "processor")
    db.create_user("reader", PASSWORD, "registered")
    registry = ProcessorRegistry()
    library = Library(url="", registry=registry)

    def nothing(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
        start_response("404 Not Found", [("Content-Type", "text/plain")])
        return [b"not found"]

    app: Any = inner if inner is not None else ProcessorAPI(nothing, registry)
    app = LoginAPI(AuthMiddleware(app, db), db)

    def recording(environ: dict[str, Any], start_response: Callable[..., Any]) -> Any:
        library.requests.append((environ["REQUEST_METHOD"], environ["PATH_INFO"]))
        return app(environ, start_response)

    return recording, library


@pytest.fixture(autouse=True)
def _isolated(monkeypatch: pytest.MonkeyPatch) -> dict[str, list[Path]]:
    """Fresh login limiters, a fixed hostname, known hardware, and no real
    install or service: those are the commands' own tests."""
    import mokuro_bunko.login.api as login_api
    import mokuro_bunko.middleware.auth as auth

    monkeypatch.setattr(login_api, "AUTH_RATE_LIMITER", AuthAttemptLimiter())
    monkeypatch.setattr(auth, "AUTH_RATE_LIMITER", AuthAttemptLimiter())
    monkeypatch.setattr(socket, "gethostname", lambda: HOSTNAME)
    monkeypatch.setattr(
        wizard,
        "detect_hardware",
        lambda: wizard.Hardware(found="NVIDIA GeForce RTX 4060 (CUDA 12.4)", recommended="cuda"),
    )
    monkeypatch.setattr(wizard, "user_service_supported", lambda: True)
    calls: dict[str, list[Path]] = {"install": [], "service": []}
    monkeypatch.setattr(
        processor_cli, "install_environments", lambda path: calls["install"].append(path)
    )
    monkeypatch.setattr(
        processor_cli, "install_service", lambda path: calls["service"].append(path)
    )
    return calls


@pytest.fixture
def library(tmp_path: Path) -> Iterator[Library]:
    app, lib = _stack(tmp_path)
    server, port = _serve(app)
    lib.url = f"http://127.0.0.1:{port}"
    yield lib
    server.shutdown()
    server.server_close()


def _setup(args: list[str], stdin: str | None = None) -> Result:
    return CliRunner().invoke(processor_group, ["setup", *args], input=stdin)


def _quick(library: Library, config: Path, *extra: str) -> Result:
    return _setup(
        ["--config", str(config), "--url", library.url, "--username", "gpu-box",
         "--password-stdin", "--yes", "--no-install", "--no-service", *extra],
        PASSWORD + "\n",
    )


def _body(config: Path) -> dict[str, Any]:
    data: dict[str, Any] = yaml.safe_load(config.read_text(encoding="utf-8"))
    return data


def _no_leftovers(folder: Path) -> None:
    assert [p.name for p in folder.iterdir() if p.name.endswith(".tmp")] == []


# -- a successful run ---------------------------------------------------------


def test_a_non_interactive_run_writes_the_minimal_config_mode_600(
    library: Library, tmp_path: Path
) -> None:
    folder = tmp_path / "my processor"  # a path with a space in it
    config = folder / "processor.yaml"
    result = _setup(
        ["--config", str(config), "--url", library.url + "/", "--username", "gpu-box",
         "--password-stdin", "--yes", "--no-install", "--no-service"],
        PASSWORD + "\n",
    )
    assert result.exit_code == 0, result.output
    assert _body(config) == {
        "library": {"url": library.url, "username": "gpu-box", "password": PASSWORD}
    }
    text = config.read_text(encoding="utf-8")
    assert text.startswith("# ") and "docs/processor.example.yaml" in text
    assert stat.S_IMODE(config.stat().st_mode) == 0o600
    loaded = load_processor_config(config)
    assert loaded.library.password == PASSWORD
    assert loaded.processor.name == HOSTNAME and loaded.ocr.backend == "auto"
    assert "Logged in: gpu-box is a processor account" in result.output
    assert "NVIDIA GeForce RTX 4060 (CUDA 12.4) -> cuda" in result.output
    assert PASSWORD not in result.output
    _no_leftovers(folder)


def test_setup_never_registers_a_processor(library: Library, tmp_path: Path) -> None:
    result = _quick(library, tmp_path / "processor.yaml")
    assert result.exit_code == 0, result.output
    assert library.registry.entries() == []
    assert ("GET", "/login/api/me") in library.requests
    assert f"(protocol 2, mokuro-bunko {__version__})" in result.output


def test_only_what_differs_from_the_defaults_is_written(
    library: Library, tmp_path: Path
) -> None:
    config = tmp_path / "processor.yaml"
    result = _quick(library, config, "--name", "tower", "--backend", "rocm")
    assert result.exit_code == 0, result.output
    body = _body(config)
    assert body["processor"] == {"name": "tower"}
    assert body["ocr"] == {"backend": "rocm"}
    assert "(as asked; auto would pick cuda)" in result.output


def test_the_hostname_and_auto_are_left_to_the_defaults(
    library: Library, tmp_path: Path
) -> None:
    config = tmp_path / "processor.yaml"
    result = _quick(library, config, "--name", HOSTNAME, "--backend", "auto")
    assert result.exit_code == 0, result.output
    assert set(_body(config)) == {"library"}


# -- the URL ------------------------------------------------------------------


@pytest.mark.parametrize(
    ("raw", "url", "noted"),
    [
        ("https://lib.example:8443/", "https://lib.example:8443", False),
        ("lib.example:8080", "http://lib.example:8080", True),
        ("  HTTP://lib.example/sub/  ", "http://lib.example/sub", False),
    ],
)
def test_urls_are_normalised(raw: str, url: str, noted: bool) -> None:
    got, note = wizard.normalize_url(raw)
    assert got == url
    assert (note is not None) == noted


@pytest.mark.parametrize("raw", ["", "ftp://lib.example", "http://", "http://lib:port"])
def test_a_url_that_is_no_address_is_refused(raw: str) -> None:
    with pytest.raises(wizard.SetupError):
        wizard.normalize_url(raw)


def test_a_bare_host_gets_http_with_a_note(library: Library, tmp_path: Path) -> None:
    config = tmp_path / "processor.yaml"
    bare = library.url.removeprefix("http://")
    result = _setup(
        ["--config", str(config), "--url", bare, "--username", "gpu-box",
         "--password-stdin", "--yes", "--no-install", "--no-service"],
        PASSWORD + "\n",
    )
    assert result.exit_code == 0, result.output
    assert "No scheme given" in result.output
    assert _body(config)["library"]["url"] == library.url


# -- every way the check can fail ---------------------------------------------


def _fails(result: Result, config: Path, *needles: str) -> None:
    assert result.exit_code != 0, result.output
    for needle in needles:
        assert needle in result.output, result.output
    assert not config.exists()
    _no_leftovers(config.parent)


def test_a_wrong_password_is_named(library: Library, tmp_path: Path) -> None:
    config = tmp_path / "processor.yaml"
    result = _setup(
        ["--config", str(config), "--url", library.url, "--username", "gpu-box",
         "--password-stdin", "--yes"],
        "wrong\n",
    )
    _fails(result, config, "refused the username 'gpu-box' or its password")


def test_an_account_that_is_not_a_processor_names_the_admin_command(
    library: Library, tmp_path: Path
) -> None:
    config = tmp_path / "processor.yaml"
    result = _setup(
        ["--config", str(config), "--url", library.url, "--username", "reader",
         "--password-stdin", "--yes"],
        PASSWORD + "\n",
    )
    _fails(
        result, config, "'reader' is a registered account",
        "ask an admin to run: mokuro-bunko admin change-role reader processor".capitalize(),
    )
    assert library.registry.entries() == []


def test_an_unreachable_library_names_the_url_and_the_error(tmp_path: Path) -> None:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    config = tmp_path / "processor.yaml"
    url = f"http://127.0.0.1:{port}"
    result = _setup(
        ["--config", str(config), "--url", url, "--username", "gpu-box",
         "--password-stdin", "--yes"],
        PASSWORD + "\n",
    )
    _fails(result, config, f"Could not reach the library at {url}", "refused")


@pytest.fixture
def tls_library(tmp_path: Path) -> Iterator[tuple[Library, Path]]:
    from mokuro_bunko.ssl import generate_self_signed_cert

    cert, key = tmp_path / "cert.pem", tmp_path / "key.pem"
    generate_self_signed_cert(cert, key, hostname="localhost")
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    app, lib = _stack(tmp_path)
    server, port = _serve(app, context)
    lib.url = f"https://127.0.0.1:{port}"
    yield lib, cert
    server.shutdown()
    server.server_close()


def test_a_self_signed_certificate_points_at_tls_verify(
    tls_library: tuple[Library, Path], tmp_path: Path
) -> None:
    library, _cert = tls_library
    config = tmp_path / "processor.yaml"
    result = _quick(library, config)
    _fails(result, config, "TLS certificate", "--tls-verify", "tls_verify")


def test_trusting_the_library_s_certificate_is_written_down(
    tls_library: tuple[Library, Path], tmp_path: Path
) -> None:
    library, cert = tls_library
    config = tmp_path / "processor.yaml"
    result = _quick(library, config, "--tls-verify", str(cert))
    assert result.exit_code == 0, result.output
    assert _body(config)["library"]["tls_verify"] == str(cert.resolve())


def test_a_library_on_a_newer_protocol_says_to_update_this_machine(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(library_api, "PROTOCOL_VERSION", 3)
    monkeypatch.setattr(library_api, "__version__", "9.9.9")
    config = tmp_path / "processor.yaml"
    result = _quick(library, config)
    _fails(
        result, config, "the library (mokuro-bunko 9.9.9) speaks 3",
        "Update this machine to the library's release",
    )
    assert library.registry.entries() == []


def test_a_library_on_an_older_protocol_says_to_update_the_library(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(library_api, "PROTOCOL_VERSION", 1)
    config = tmp_path / "processor.yaml"
    result = _quick(library, config)
    _fails(result, config, "Update the library to this machine's release")


def test_another_release_on_the_same_protocol_is_only_a_note(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setattr(library_api, "__version__", "0.0.1")
    config = tmp_path / "processor.yaml"
    result = _quick(library, config)
    assert result.exit_code == 0, result.output
    assert "the library 0.0.1" in result.output


def test_a_library_without_remote_processors_says_to_update_it(tmp_path: Path) -> None:
    def no_processors(environ: dict[str, Any], start_response: Callable[..., Any]) -> Any:
        start_response("404 Not Found", [("Content-Type", "text/plain")])
        return [b"nope"]

    app, lib = _stack(tmp_path, inner=no_processors)
    server, port = _serve(app)
    try:
        lib.url = f"http://127.0.0.1:{port}"
        config = tmp_path / "processor.yaml"
        result = _quick(lib, config)
    finally:
        server.shutdown()
        server.server_close()
    _fails(result, config, "has no remote processors", "Update the library")


def test_a_server_that_is_no_library_is_named(tmp_path: Path) -> None:
    def elsewhere(environ: dict[str, Any], start_response: Callable[..., Any]) -> Any:
        start_response("404 Not Found", [("Content-Type", "text/html")])
        return [b"<html>not here</html>"]

    server, port = _serve(elsewhere)
    try:
        config = tmp_path / "processor.yaml"
        result = _quick(Library(url=f"http://127.0.0.1:{port}", registry=None), config)  # type: ignore[arg-type]
    finally:
        server.shutdown()
        server.server_close()
    _fails(result, config, "does not look like a mokuro-bunko library")


def test_yes_without_a_required_value_fails_naming_it(tmp_path: Path) -> None:
    config = tmp_path / "processor.yaml"
    result = _setup(["--config", str(config), "--yes", "--url", "http://x.example"])
    _fails(result, config, "--username is required with --yes")


# -- an existing config -----------------------------------------------------


def test_an_existing_config_is_not_overwritten_without_force(
    library: Library, tmp_path: Path
) -> None:
    config = tmp_path / "processor.yaml"
    config.write_text("# mine\n", encoding="utf-8")
    result = _quick(library, config)
    assert result.exit_code != 0
    assert "--force" in result.output
    assert config.read_text(encoding="utf-8") == "# mine\n"
    assert library.requests == [], "refused before asking anything"

    result = _quick(library, config, "--force")
    assert result.exit_code == 0, result.output
    assert _body(config)["library"]["username"] == "gpu-box"


def test_interactively_an_existing_config_is_offered_for_overwrite(
    library: Library, tmp_path: Path
) -> None:
    config = tmp_path / "processor.yaml"
    config.write_text("# mine\n", encoding="utf-8")
    result = _setup(["--config", str(config)], "n\n")
    assert result.exit_code == 0, result.output
    assert "Nothing written" in result.output
    assert config.read_text(encoding="utf-8") == "# mine\n"

    answers = f"y\n{library.url}\ngpu-box\n{PASSWORD}\nn\nn\n"
    result = _setup(["--config", str(config)], answers)
    assert result.exit_code == 0, result.output
    assert _body(config)["library"]["password"] == PASSWORD


# -- the interactive wizard -------------------------------------------------


def test_the_prompts_ask_for_what_is_missing_and_chain_the_steps(
    library: Library, tmp_path: Path, _isolated: dict[str, list[Path]]
) -> None:
    config = tmp_path / "processor.yaml"
    answers = f"{library.url}\ngpu-box\n{PASSWORD}\n\n\n"  # Enter takes each default: yes
    result = _setup(["--config", str(config)], answers)
    assert result.exit_code == 0, result.output
    out = result.output
    for prompt in ("Library URL:", "Processor username:", "Password:",
                   "Install the OCR environments now?", "systemd user service"):
        assert prompt in out
    assert PASSWORD not in out, "the password prompt does not echo"
    assert out.count("Password:") == 1, "asked once: the login check is the confirmation"
    assert _isolated["install"] == [config]
    assert _isolated["service"] == [config]
    assert "Installed: yes, the OCR environments (cuda)" in out
    assert "Running:   yes, as the systemd user service" in out
    assert f"journalctl --user -u {service.UNIT_NAME} -f" in out


def test_no_install_and_no_service_skip_both(
    library: Library, tmp_path: Path, _isolated: dict[str, list[Path]]
) -> None:
    result = _quick(library, tmp_path / "processor.yaml")
    assert result.exit_code == 0, result.output
    assert _isolated == {"install": [], "service": []}
    assert "Installed: skipped" in result.output
    assert "Running:   no" in result.output


def test_yes_runs_install_and_service_by_default(
    library: Library, tmp_path: Path, _isolated: dict[str, list[Path]]
) -> None:
    config = tmp_path / "processor.yaml"
    result = _setup(
        ["--config", str(config), "--url", library.url, "--username", "gpu-box",
         "--password-stdin", "--yes"],
        PASSWORD + "\n",
    )
    assert result.exit_code == 0, result.output
    assert _isolated == {"install": [config], "service": [config]}


def test_without_systemd_the_exact_serve_command_is_printed(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    _isolated: dict[str, list[Path]],
) -> None:
    monkeypatch.setattr(wizard, "user_service_supported", lambda: False)
    config = tmp_path / "with space" / "processor.yaml"
    result = _setup(
        ["--config", str(config), "--url", library.url, "--username", "gpu-box",
         "--password-stdin", "--yes", "--no-install"],
        PASSWORD + "\n",
    )
    assert result.exit_code == 0, result.output
    assert _isolated["service"] == []
    assert wizard.command("processor", "serve", "--config", str(config)) in result.output
    assert f"'{config}'" in result.output, "the path with a space is quoted"


def test_a_failed_install_stops_before_the_service_and_says_how_to_retry(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    _isolated: dict[str, list[Path]],
) -> None:
    import click

    def broken(path: Path) -> None:
        raise click.ClickException("the engines environment failed to install")

    monkeypatch.setattr(processor_cli, "install_environments", broken)
    config = tmp_path / "processor.yaml"
    result = _setup(
        ["--config", str(config), "--url", library.url, "--username", "gpu-box",
         "--password-stdin", "--yes"],
        PASSWORD + "\n",
    )
    assert result.exit_code != 0
    assert "the engines environment failed to install" in result.output
    assert "processor install --config" in result.output
    assert _isolated["service"] == []
    assert config.exists(), "the config is kept: it checked out"


def test_a_failed_service_still_prints_how_to_run_it(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import click

    def broken(path: Path) -> None:
        raise click.ClickException("`systemctl --user daemon-reload` failed: no bus")

    monkeypatch.setattr(processor_cli, "install_service", broken)
    config = tmp_path / "processor.yaml"
    result = _setup(
        ["--config", str(config), "--url", library.url, "--username", "gpu-box",
         "--password-stdin", "--yes", "--no-install"],
        PASSWORD + "\n",
    )
    assert result.exit_code != 0
    assert "Running:   no, the service failed: `systemctl --user daemon-reload`" in result.output
    assert wizard.command("processor", "serve", "--config", str(config)) in result.output
    assert "processor service --install --config" in result.output


def test_ctrl_c_at_a_prompt_leaves_no_file(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    import click

    asked: list[str] = []

    def interrupted(text: str, **_kwargs: Any) -> str:
        asked.append(text)
        if text == "Password":
            raise click.Abort()
        return library.url if text == "Library URL" else "gpu-box"

    monkeypatch.setattr(wizard.click, "prompt", interrupted)
    config = tmp_path / "processor.yaml"
    result = _setup(["--config", str(config)])
    assert result.exit_code != 0
    assert asked == ["Library URL", "Processor username", "Password"]
    assert not config.exists()
    _no_leftovers(tmp_path)


def test_an_interrupted_write_leaves_neither_a_file_nor_a_temporary(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    def interrupted(*_args: Any) -> None:
        raise KeyboardInterrupt

    monkeypatch.setattr(wizard.os, "replace", interrupted)
    config = tmp_path / "out" / "processor.yaml"
    result = _quick(library, config)
    assert result.exit_code != 0
    assert list(config.parent.iterdir()) == []


# -- the password never rides a command line --------------------------------


def test_there_is_no_password_option() -> None:
    command = processor_group.commands["setup"]
    flags = {flag for param in command.params for flag in getattr(param, "opts", [])}
    assert "--password-stdin" in flags
    assert not [f for f in flags if f.startswith("--password") and f != "--password-stdin"]


def test_no_password_in_argv_anywhere(
    library: Library, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A full run with the REAL service step (systemctl stubbed): no process
    it starts, no unit it writes, nothing it prints carries the password."""
    argvs: list[list[str]] = []
    real_popen = subprocess.Popen

    class Recording(real_popen):  # type: ignore[misc,valid-type]
        def __init__(self, args: Any, *rest: Any, **kwargs: Any) -> None:
            argvs.append([str(a) for a in (args if isinstance(args, list) else [args])])
            super().__init__(args, *rest, **kwargs)

    def systemctl(cmd: list[str]) -> subprocess.CompletedProcess[str]:
        argvs.append(list(cmd))
        return subprocess.CompletedProcess(cmd, 0, "Linger=yes\n", "")

    monkeypatch.setattr(subprocess, "Popen", Recording)
    monkeypatch.setattr(service, "_run", systemctl)
    monkeypatch.setattr(service, "entry_point", lambda *a: Path("/opt/bunko/bin/mokuro-bunko"))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(tmp_path / "xdg"))
    monkeypatch.setattr(processor_cli, "install_service", REAL_INSTALL_SERVICE)
    monkeypatch.setattr(wizard, "detect_hardware", REAL_DETECT_HARDWARE)

    config = tmp_path / "processor.yaml"
    result = _setup(
        ["--config", str(config), "--url", library.url, "--username", "gpu-box",
         "--password-stdin", "--yes", "--no-install"],
        PASSWORD + "\n",
    )
    assert result.exit_code == 0, result.output
    assert ["systemctl", "--user", "enable", "--now", service.UNIT_NAME] in argvs
    assert all(PASSWORD not in " ".join(argv) for argv in argvs)
    unit = (tmp_path / "xdg" / "systemd" / "user" / service.UNIT_NAME).read_text()
    assert PASSWORD not in unit and str(config) in unit
    assert PASSWORD not in result.output


# -- the Windows storage default ----------------------------------------------


def test_windows_storage_defaults_to_local_app_data(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(processor_config.sys, "platform", "win32")
    monkeypatch.setenv("LOCALAPPDATA", "/c/Users/me/AppData/Local")
    assert processor_config.default_storage_path() == Path(
        "/c/Users/me/AppData/Local/mokuro-bunko-processor"
    )
    monkeypatch.delenv("LOCALAPPDATA")
    assert processor_config.default_storage_path() == (
        Path.home() / "AppData" / "Local" / "mokuro-bunko-processor"
    )


def test_posix_storage_stays_on_xdg(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
    monkeypatch.setattr(processor_config.sys, "platform", "linux")
    monkeypatch.setenv("LOCALAPPDATA", "/should/not/be/used")
    monkeypatch.setenv("XDG_DATA_HOME", str(tmp_path))
    assert processor_config.default_storage_path() == tmp_path / "mokuro-bunko-processor"
    monkeypatch.delenv("XDG_DATA_HOME")
    assert processor_config.default_storage_path() == (
        Path.home() / ".local" / "share" / "mokuro-bunko-processor"
    )


def test_windows_restricts_the_file_with_icacls(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls: list[list[str]] = []

    def icacls(cmd: list[str], **_kwargs: Any) -> subprocess.CompletedProcess[str]:
        calls.append(cmd)
        # whoami names the real account; USERDOMAIN may say WORKGROUP.
        out = "BOX\\me\r\n" if cmd[0] == "whoami" else ""
        return subprocess.CompletedProcess(cmd, 0, out, "")

    target = tmp_path / "processor.yaml"
    target.write_text("x", encoding="utf-8")
    monkeypatch.setattr(wizard.sys, "platform", "win32")
    monkeypatch.setenv("USERNAME", "me")
    monkeypatch.setenv("USERDOMAIN", "BOX")
    monkeypatch.setattr(wizard.subprocess, "run", icacls)
    assert wizard._restrict_to_owner(target) is None
    assert calls == [
        ["whoami"],
        ["icacls", str(target), "/inheritance:r", "/grant:r", "BOX\\me:F"],
    ]

    monkeypatch.setattr(
        wizard.subprocess, "run",
        lambda cmd, **_k: subprocess.CompletedProcess(cmd, 5, "", "Access is denied."),
    )
    warning = wizard._restrict_to_owner(target)
    assert warning is not None and "holds the library password" in warning


def test_setup_is_listed_in_the_processor_help() -> None:
    result = CliRunner().invoke(processor_group, ["setup", "--help"])
    assert result.exit_code == 0
    assert "--password-stdin" in result.output
