"""``mokuro-bunko processor setup``: from a fresh install to a running processor.

Only three settings have no default -- the library's URL, and the processor
account's username and password -- and hand-editing a YAML file for them is
where setting a processor up went wrong (a Windows editor, a stray tab, a
password with a ``#`` in it). So the wizard asks for those three, CHECKS them
against the library before it writes a byte, writes only what differs from
the defaults, and then runs the steps that used to be typed one by one:
``processor install`` and, where systemd runs user units,
``processor service --install``.

The check never registers. A registration is what makes a machine appear on
the library's admin panel, and a machine that is only being set up would be
a phantom there. Who the account is comes from ``/login/api/me`` (the
reader's identity endpoint, which every role may ask), and the processor
protocol from a registration the library is certain to refuse -- protocol 0,
which no library has ever spoken -- whose refusal names the protocols it
does speak and, since this release, its version.
"""

from __future__ import annotations

import json
import os
import shlex
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
from collections.abc import Callable
from dataclasses import dataclass, field
from http.client import HTTPException
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit, urlunsplit

import click
import yaml

from mokuro_bunko import __version__
from mokuro_bunko.processor.config import (
    VALID_BACKENDS,
    LibrarySettings,
    ProcessorConfig,
    ProcessorConfigError,
    load_processor_config,
)

DEFAULT_CONFIG_NAME = "processor.yaml"
IDENTITY_PATH = "/login/api/me"
REGISTER_PATH = "/_processor/register"
# A registration naming this protocol is refused before anything is
# registered (`ProcessorAPI._register` compares it first).
PROBE_PROTOCOL = 0
VERIFY_TIMEOUT = 15.0
PROCESSOR_ROLE = "processor"

HEADER = """\
# mokuro-bunko processor, written by `mokuro-bunko processor setup`.
#
# Only the settings that differ from the defaults are here. Every other
# setting, and what each one does, is in docs/processor.example.yaml.
#
# This file holds the library password: keep it private.

"""


class SetupError(Exception):
    """Setup cannot go on; the message says why and what to do about it."""


# -- the URL ------------------------------------------------------------------


def normalize_url(raw: str) -> tuple[str, str | None]:
    """``(url, note)``: the library URL as the config stores it.

    A bare host gets ``http://`` -- a note says so, because a library that
    serves TLS then needs ``https://`` typed out -- and a trailing slash is
    dropped (the client appends its own paths).
    """
    text = raw.strip()
    if not text:
        raise SetupError("the library URL is empty")
    defaulted = "://" not in text
    if defaulted:
        text = "http://" + text
    parts = urlsplit(text)
    scheme = parts.scheme.lower()
    if scheme not in ("http", "https"):
        raise SetupError(f"{raw.strip()}: the library URL must start with http:// or https://")
    try:
        parts.port  # noqa: B018 - raises on a port that is not a number
    except ValueError as e:
        raise SetupError(f"{raw.strip()}: {e}") from None
    if not parts.hostname:
        raise SetupError(f"{raw.strip()}: no host name in the library URL")
    if parts.query or parts.fragment:
        raise SetupError(
            f"{raw.strip()}: the library URL is its address only, without ?... or #..."
        )
    url = urlunsplit((scheme, parts.netloc, parts.path.rstrip("/"), "", ""))
    note = (
        f"No scheme given, so using {url} (type https://... if the library uses TLS)."
        if defaulted
        else None
    )
    return url, note


def parse_tls_verify(raw: str) -> bool | str:
    """``--tls-verify``: true, false, or the path of the certificate to trust."""
    value = raw.strip()
    if value.lower() in ("true", "yes", "1", ""):
        return True
    if value.lower() in ("false", "no", "0"):
        return False
    path = Path(value).expanduser()
    if not path.is_file():
        raise SetupError(f"--tls-verify {value}: no such certificate file")
    return str(path.resolve())


# -- the check --------------------------------------------------------------


@dataclass(frozen=True)
class Verified:
    """What the library said about the account."""

    username: str
    role: str
    protocol: int
    #: The library's release, when it says (a library before 0.5.0 does not).
    library_version: str | None = None
    notes: tuple[str, ...] = field(default_factory=tuple)


def _client(url: str, username: str, password: str, tls_verify: bool | str) -> Any:
    """The same client, TLS settings and all, that ``processor serve`` uses."""
    from mokuro_bunko.processor.client import LibraryClient

    return LibraryClient(
        ProcessorConfig(
            library=LibrarySettings(
                url=url, username=username, password=password, tls_verify=tls_verify
            )
        )
    )


def _exchange(
    client: Any, url: str, method: str, path: str, body: dict[str, Any] | None = None
) -> tuple[int, dict[str, Any] | None, str | None]:
    """``(status, JSON body or None, Location)``, or a SetupError naming why not."""
    raw = json.dumps(body).encode() if body is not None else None
    headers = client.request_headers()
    if raw is not None:
        headers["Content-Type"] = "application/json"
    connection = client.connect(timeout=VERIFY_TIMEOUT)
    try:
        connection.request(method, client.root + path, body=raw, headers=headers)
        response = connection.getresponse()
        data = response.read()
        status = response.status
        location = response.getheader("Location")
    except ssl.SSLCertVerificationError as e:
        reason = getattr(e, "verify_message", None) or str(e)
        raise SetupError(
            f"The library's TLS certificate at {url} was not accepted: {reason}.\n"
            "If the library uses a self-signed certificate, run setup again with\n"
            "--tls-verify /path/to/its-certificate.pem (or --tls-verify false on a network\n"
            "you trust). That is the `tls_verify` setting in processor.yaml."
        ) from None
    except ssl.SSLError as e:
        raise SetupError(
            f"Could not open a TLS connection to {url}: {e}.\n"
            "If the library serves plain HTTP, use an http:// URL. For certificate trouble, "
            "see --tls-verify (the `tls_verify` setting in processor.yaml)."
        ) from None
    except (OSError, HTTPException) as e:
        raise SetupError(
            f"Could not reach the library at {url}: {e or type(e).__name__}.\n"
            "Check the URL, and that the library is running and reachable from this machine."
        ) from None
    finally:
        connection.close()
    try:
        parsed = json.loads(data.decode("utf-8")) if data else None
    except (UnicodeDecodeError, ValueError):
        parsed = None
    return status, parsed if isinstance(parsed, dict) else None, location


def _not_a_library(url: str, status: int) -> SetupError:
    return SetupError(
        f"{url} does not look like a mokuro-bunko library: it answered {status} to "
        f"{IDENTITY_PATH}. Check the URL (the address the library's web page is at)."
    )


def verify_account(
    url: str, username: str, password: str, tls_verify: bool | str = True
) -> Verified:
    """Log in, and check the account is a processor on a library that speaks
    this machine's protocol. Registers nothing."""
    from mokuro_bunko.ocr.remote.protocol import PROTOCOL_VERSION

    client = _client(url, username, password, tls_verify)

    status, body, location = _exchange(client, url, "GET", IDENTITY_PATH)
    if 300 <= status < 400:
        target = (location or "").split(IDENTITY_PATH, 1)[0].rstrip("/")
        raise SetupError(
            f"{url} redirects to {target or 'somewhere else'}: run setup again with "
            "that URL."
        )
    if status == 401:
        raise SetupError(
            f"The library refused the username {username!r} or its password. Check both; "
            f"an admin can set a new password with: mokuro-bunko admin set-password {username}"
        )
    if status == 429:
        detail = (body or {}).get("error") or "too many failed attempts"
        raise SetupError(f"The library is refusing logins for {username!r} for now: {detail}")
    if status != 200 or body is None or "authenticated" not in body:
        raise _not_a_library(url, status)
    if not body.get("authenticated"):
        raise SetupError(
            f"{url} did not see the login (it answered as if anonymous). A proxy in front "
            "of the library may be dropping the Authorization header."
        )
    role = str(body.get("role") or "")
    account = str(body.get("username") or username)
    if role != PROCESSOR_ROLE:
        raise SetupError(
            f"{account!r} is {_article(role)} {role} account, not a processor account. "
            f"Ask an admin to run: mokuro-bunko admin change-role {account} processor"
        )

    notes: list[str] = []
    status, body, _location = _exchange(
        client, url, "POST", REGISTER_PATH, {"protocol": PROBE_PROTOCOL}
    )
    if status == 404:
        raise SetupError(
            f"The library at {url} has no remote processors (it answered 404 to "
            f"{REGISTER_PATH}). Update the library to the same release as this machine "
            f"(mokuro-bunko {__version__})."
        )
    protocols = (body or {}).get("protocols")
    library_version = (body or {}).get("version")
    library_version = str(library_version) if library_version else None
    if status != 400 or not isinstance(protocols, list):
        notes.append(
            f"Could not check the library's processor protocol (it answered {status}); "
            "`processor serve` will say if they differ."
        )
        return Verified(account, role, PROTOCOL_VERSION, library_version, tuple(notes))
    if PROTOCOL_VERSION not in protocols:
        theirs = ", ".join(str(p) for p in protocols) or "none"
        newer_library = all(isinstance(p, int) and p > PROTOCOL_VERSION for p in protocols)
        side = (
            "Update this machine to the library's release"
            if newer_library
            else "Update the library to this machine's release"
        )
        release = f" (mokuro-bunko {library_version})" if library_version else ""
        raise SetupError(
            f"This machine speaks processor protocol {PROTOCOL_VERSION} (mokuro-bunko "
            f"{__version__}); the library{release} speaks {theirs}. {side} -- both must "
            "run the same release."
        )
    if library_version and library_version != __version__:
        notes.append(
            f"This machine runs mokuro-bunko {__version__} and the library "
            f"{library_version}. Both speak processor protocol {PROTOCOL_VERSION}, so "
            "this works; update them to the same release when you can."
        )
    return Verified(account, role, PROTOCOL_VERSION, library_version, tuple(notes))


def _article(word: str) -> str:
    return "an" if word[:1].lower() in "aeiou" else "a"


# -- the hardware -------------------------------------------------------------


@dataclass(frozen=True)
class Hardware:
    """What was found, and the backend ``auto`` resolves to on it."""

    found: str
    recommended: str
    #: backend -> why it cannot be used here.
    unavailable: dict[str, str] = field(default_factory=dict)

    def line(self, backend: str) -> str:
        """``AMD Radeon RX 9070 XT (ROCm 7.2.4) → rocm``, plus what was asked."""
        if backend == "auto":
            return f"{self.found} -> {self.recommended}"
        text = f"{self.found} -> {backend} (as asked; auto would pick {self.recommended})"
        reason = self.unavailable.get(backend)
        if reason:
            text += f"\nWarning: {reason}; `processor install` falls back to what works here."
        return text


def detect_hardware() -> Hardware:
    """The installer's own detection, and the GPU's name as the library shows it."""
    from mokuro_bunko.ocr.bench import describe_host
    from mokuro_bunko.ocr.installer import (
        detect_hardware as detect,
    )
    from mokuro_bunko.ocr.installer import (
        get_backend_unavailable_reasons,
        get_recommended_backend,
        get_supported_backends,
    )

    hardware = detect()
    supported = get_supported_backends(hardware=hardware)
    recommended = get_recommended_backend(hardware=hardware, supported_backends=supported)
    host = describe_host(None, None)
    gpu = host.get("gpu")
    if hardware.has_cuda:
        stack = f"CUDA {hardware.cuda_version}" if hardware.cuda_version else "CUDA"
    elif hardware.has_rocm:
        stack = f"ROCm {hardware.rocm_version}" if hardware.rocm_version else "ROCm"
    else:
        stack = ""
    if gpu:
        found = f"{gpu} ({stack})" if stack else str(gpu)
    elif stack:
        found = f"a GPU with {stack}"
    else:
        found = f"no GPU found; {host.get('cpu') or 'CPU'}"
    reasons = get_backend_unavailable_reasons(hardware=hardware)
    return Hardware(
        found=found,
        recommended=recommended.value,
        unavailable={b.value: r for b, r in reasons.items()},
    )


# -- the file ---------------------------------------------------------------


def render_config(
    *,
    url: str,
    username: str,
    password: str,
    name: str | None = None,
    backend: str = "auto",
    tls_verify: bool | str = True,
    hostname: str | None = None,
) -> str:
    """processor.yaml with only what differs from the defaults."""
    library: dict[str, Any] = {"url": url, "username": username, "password": password}
    if tls_verify is not True:
        library["tls_verify"] = tls_verify
    data: dict[str, Any] = {"library": library}
    if name and name != (hostname or socket.gethostname()):
        data["processor"] = {"name": name}
    if backend != "auto":
        if backend not in VALID_BACKENDS:
            raise SetupError(f"backend {backend!r} is not one of {', '.join(VALID_BACKENDS)}")
        data["ocr"] = {"backend": backend}
    body = yaml.safe_dump(data, sort_keys=False, default_flow_style=False, allow_unicode=True)
    return HEADER + body


def _windows_account() -> str | None:
    """``machine\\user`` as Windows knows the current account.

    Not ``USERDOMAIN\\USERNAME``: on a machine in no domain, and in an SSH
    session, USERDOMAIN can say WORKGROUP -- an account icacls cannot find.
    """
    try:
        done = subprocess.run(["whoami"], capture_output=True, text=True, check=False)
    except OSError:
        done = None
    if done is not None and done.returncode == 0 and done.stdout.strip():
        return done.stdout.strip()
    return os.environ.get("USERNAME") or None


def _restrict_to_owner(path: Path, shown: Path | None = None) -> str | None:
    """Mode 600, or the Windows equivalent. A warning when it cannot be done.

    ``shown`` is the file the user knows (``path`` may be the staged copy).
    """
    shown = shown or path
    if sys.platform != "win32":
        os.chmod(path, 0o600)
        return None
    principal = _windows_account()
    if not principal:
        return f"{shown} holds the library password; keep it where only you can read it."
    try:
        done = subprocess.run(
            ["icacls", str(path), "/inheritance:r", "/grant:r", f"{principal}:F"],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        done = None
    if done is None or done.returncode != 0:
        return f"{shown} holds the library password; keep it where only you can read it."
    return None


def write_config(path: Path, text: str, *, overwrite: bool) -> str | None:
    """Write ``text`` to ``path`` all at once, readable by its owner only.

    A temporary file beside it is written, restricted, read back the way
    ``processor serve`` reads it and only then moved over ``path``: an
    interrupt at any point leaves either no file or the old one, never half
    of a new one. Returns a warning when the file could not be restricted.
    """
    path = Path(path)
    if path.exists() and not overwrite:
        raise SetupError(f"{path} already exists; pass --force to overwrite it")
    path.parent.mkdir(parents=True, exist_ok=True)
    handle, temporary = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    staged = Path(temporary)
    try:
        with os.fdopen(handle, "w", encoding="utf-8", newline="\n") as out:
            out.write(text)
            out.flush()
            os.fsync(out.fileno())
        warning = _restrict_to_owner(staged, shown=path)
        try:
            load_processor_config(staged)
        except ProcessorConfigError as e:  # pragma: no cover - render_config's own output
            raise SetupError(f"the configuration setup wrote does not load: {e}") from e
        os.replace(staged, path)
    except BaseException:
        staged.unlink(missing_ok=True)
        raise
    return warning


# -- what runs it -------------------------------------------------------------


def _join(words: list[str]) -> str:
    return subprocess.list2cmdline(words) if sys.platform == "win32" else shlex.join(words)


def command(*args: str) -> str:
    """A ``mokuro-bunko ...`` command line that works from any directory."""
    folder = Path(sys.executable).parent
    for name in ("mokuro-bunko", "mokuro-bunko.exe"):
        candidate = folder / name
        if candidate.is_file():
            return _join([str(candidate), *args])
    return _join([sys.executable, "-m", "mokuro_bunko", *args])


def user_service_supported() -> bool:
    """True where ``processor service --install`` can work: Linux booted with
    systemd, and this install's own entry point to put in the unit."""
    from mokuro_bunko.processor import service

    if sys.platform == "win32":
        # A Startup entry: no systemd, no administrator.
        try:
            service.entry_point()
        except service.ServiceError:
            return False
        return True
    if not sys.platform.startswith("linux") or shutil.which("systemctl") is None:
        return False
    if not Path("/run/systemd/system").is_dir():  # sd_booted()
        return False
    try:
        service.entry_point()
    except service.ServiceError:
        return False
    return True


# -- the wizard ---------------------------------------------------------------


@dataclass
class Options:
    config: Path
    url: str | None = None
    username: str | None = None
    password: str | None = None
    name: str | None = None
    backend: str = "auto"
    tls_verify: bool | str = True
    yes: bool = False
    install: bool = True
    service: bool = True
    force: bool = False


def _required(value: str | None, flag: str, prompt: str, options: Options, **kw: Any) -> str:
    if value is not None and value.strip():
        return value.strip() if not kw.get("hide_input") else value
    if options.yes:
        raise SetupError(f"{flag} is required with --yes")
    answer: str = click.prompt(prompt, **kw)
    if not answer.strip():
        raise SetupError(f"{prompt.lower()} is empty")
    return answer if kw.get("hide_input") else answer.strip()


def _ask(options: Options, question: str, default: bool) -> bool:
    return default if options.yes else click.confirm(question, default=default)


def run(
    options: Options,
    *,
    install_step: Callable[[Path], None],
    service_step: Callable[[Path], None],
) -> None:
    """The whole wizard. Nothing is written before the account checks out."""
    path = Path(options.config).expanduser()
    path = path if path.is_absolute() else Path.cwd() / path
    overwrite = options.force
    if path.exists() and not overwrite:
        if options.yes:
            raise SetupError(f"{path} already exists; pass --force to overwrite it")
        if not click.confirm(f"{path} already exists. Overwrite it?", default=False):
            click.echo("Nothing written.")
            return
        overwrite = True

    raw_url = _required(options.url, "--url", "Library URL", options)
    url, note = normalize_url(raw_url)
    if note:
        click.echo(note)
    username = _required(options.username, "--username", "Processor username", options)
    password = _required(
        options.password, "--password-stdin", "Password", options, hide_input=True
    )

    click.echo(f"Checking {username} on {url} ...")
    verified = verify_account(url, username, password, options.tls_verify)
    release = f", mokuro-bunko {verified.library_version}" if verified.library_version else ""
    click.echo(
        f"Logged in: {verified.username} is a processor account "
        f"(protocol {verified.protocol}{release})."
    )
    for line in verified.notes:
        click.echo(line)

    hardware = detect_hardware()
    click.echo(f"Hardware: {hardware.line(options.backend)}")

    hostname = socket.gethostname()
    name = (options.name or "").strip() or hostname
    text = render_config(
        url=url,
        username=verified.username,
        password=password,
        name=name,
        backend=options.backend,
        tls_verify=options.tls_verify,
        hostname=hostname,
    )
    warning = write_config(path, text, overwrite=overwrite)
    click.echo(f"Wrote {path}" + ("" if warning else " (readable by you only)"))
    if warning:
        click.echo(f"Note: {warning}")

    installed = "skipped"
    failed: list[str] = []
    if options.install and _ask(
        options, "Install the OCR environments now? (downloads several GB)", True
    ):
        try:
            install_step(path)
            built = hardware.recommended if options.backend == "auto" else options.backend
            installed = f"yes, the OCR environments ({built})"
        except click.ClickException as e:
            installed = f"FAILED: {e.format_message()}"
            failed.append(
                "the OCR environments did not install; fix the cause above and run: "
                + command("processor", "install", "--config", str(path))
            )

    running = "no"
    logs = ""
    if not failed and options.service and user_service_supported():
        windows = sys.platform == "win32"
        question = (
            "Start the processor now, and at every logon (a Startup entry)?"
            if windows
            else "Run the processor as a systemd user service now?"
        )
        if _ask(options, question, True):
            from mokuro_bunko.processor.service import STARTUP_NAME, UNIT_NAME

            try:
                service_step(path)
                if windows:
                    running = f"yes, in its own minimized window; at every logon from {STARTUP_NAME}"
                    logs = "its window; its last state: " + command(
                        "processor", "status", "--config", str(path)
                    )
                else:
                    running = f"yes, as the systemd user service {UNIT_NAME}"
                    logs = f"journalctl --user -u {UNIT_NAME} -f"
            except click.ClickException as e:
                running = f"no, the service failed: {e.format_message()}"
                failed.append(
                    "the service did not start; fix the cause above and run: "
                    + command("processor", "service", "--install", "--config", str(path))
                )
    if not logs:
        click.echo("")
        click.echo("To run the processor:")
        click.echo("  " + command("processor", "serve", "--config", str(path)))
        if sys.platform == "win32":
            click.echo(
                "To start it at every logon: "
                + command("processor", "service", "--install", "--config", str(path))
            )
        status = command("processor", "status", "--config", str(path))
        logs = f"printed by the command above; its last state: {status}"

    click.echo("")
    click.echo("Summary")
    click.echo(f"  Config:    {path}")
    click.echo(f"  Installed: {installed}")
    if installed == "skipped":
        click.echo(f"             later: {command('processor', 'install', '--config', str(path))}")
    click.echo(f"  Running:   {running}")
    click.echo(f"  Logs:      {logs}")
    if failed:
        raise SetupError("; ".join(failed))
