"""``mokuro-bunko processor serve|install|service|setup|status``.

Module level imports only click, the config loader and the status file:
`bridge`, `client` and the installers pull in the runner and the OCR stack,
and every other `mokuro-bunko` command would otherwise pay for them.
"""

from __future__ import annotations

import logging
import os
import signal
import sys
import time
from pathlib import Path
from typing import Any

import click

from mokuro_bunko.ocr.engines import DEFAULT_DETECTOR, OFFERED_DETECTOR_IDS
from mokuro_bunko.processor.config import (
    VALID_BACKENDS,
    ProcessorConfigError,
    load_processor_config,
)
from mokuro_bunko.processor.status import describe, read_status, write_status

logger = logging.getLogger(__name__)

# Spec section 6: reconnect with backoff 5 s -> 5 min.
BACKOFF_START = 5.0
BACKOFF_MAX = 300.0
# A 409 says "register again NOW", which skips the backoff -- but not the
# floor: a library answering 409 to every stream would otherwise be a hot
# loop of registrations against a server that is already unhappy.
REREGISTER_FLOOR = 1.0


class StorageLock:
    """An exclusive hold on one processor storage, for the life of `serve`.

    One storage, one processor (design section 4.1). `stage_runner` prunes
    every staged runner build its OWN process is not using, so a second
    processor pointed at the same storage -- an acceptance rig aimed at the
    live one by mistake -- would delete the live runner's build from under
    it, and that runner imports its detector adapters lazily, by path,
    mid-volume. The spool's disk fallback is swept at start for the same
    reason: only the storage's one owner may do that.

    An ``fcntl.flock`` on ``<storage>/.processing/serve.lock``: the kernel
    drops it when the process dies, however it dies, so a crash never leaves
    a storage locked. Where there is no ``fcntl`` (Windows) there is no lock.
    """

    def __init__(self, handle: Any | None) -> None:
        self._handle = handle

    def close(self) -> None:
        handle, self._handle = self._handle, None
        if handle is not None:
            handle.close()


def lock_storage(storage: Path) -> StorageLock | None:
    """Take the storage's lock. None when another process already holds it."""
    try:
        import fcntl
    except ImportError:  # pragma: no cover - Windows
        return StorageLock(None)
    path = Path(storage) / ".processing" / "serve.lock"
    path.parent.mkdir(parents=True, exist_ok=True)
    handle = path.open("a+b")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        handle.close()
        return None
    return StorageLock(handle)


@click.group(name="processor")
def processor_group() -> None:
    """Run this machine as an OCR processor for a mokuro-bunko library."""


def _config(path: Path) -> Any:
    try:
        return load_processor_config(path)
    except ProcessorConfigError as e:
        raise click.ClickException(str(e)) from e


def _engines_python(installer: Any) -> Path | None:
    """The engines environment's interpreter, or an override for a test run."""
    override = os.environ.get("MOKURO_PROCESSOR_ENGINES_PYTHON")
    if override:
        return Path(override)
    return installer.get_python_executable() if installer.is_installed() else None


def _catalog(config: Any) -> tuple[dict[str, Any], dict[str, Any]]:
    """What this machine can run, and what it is.

    An empty engines list is not an error: it is how a processor whose
    environment is not installed yet registers and shows as "installing"
    on the library's admin panel.
    """
    from mokuro_bunko.ocr.bench import describe_host, probe_devices
    from mokuro_bunko.ocr.devices import GPU_FACTS_KEY, ORT_CATALOG_KEY
    from mokuro_bunko.ocr.engines import ENGINES
    from mokuro_bunko.ocr.installer import EnginesInstaller, OCRInstaller

    if os.environ.get("MOKURO_PROCESSOR_RUNNER"):
        # The runner is overridden, so what the installed environments hold
        # says nothing about what this process can run.
        return (
            {
                "engines": sorted(ENGINES),
                "detectors": sorted(OFFERED_DETECTOR_IDS),
                "devices": [{"id": "auto", "label": "Auto"}, {"id": "cpu", "label": "CPU"}],
                "serves_mokuro": True,
            },
            {"cpu": "override", "gpu": None, "backend": config.ocr.backend},
        )

    engines_installer = EnginesInstaller()
    mokuro_installer = OCRInstaller()
    engines_python = _engines_python(engines_installer)
    installed: list[str] = []
    if engines_installer.is_installed():
        installed += [spec.id for spec in ENGINES.values() if not spec.uses_mokuro_env]
    if mokuro_installer.is_installed():
        installed += [spec.id for spec in ENGINES.values() if spec.uses_mokuro_env]
    # EVERY detector bunko offers, probed one by one -- not the installer's
    # own `detectors`, which for a default `EnginesInstaller()` is just the
    # default detector. A row is offered here only if its detector is in
    # this list (`catalog_can_run`), so a `ctd` detector installed by an
    # earlier `processor install --detector ctd` must be reported as such.
    # A disabled one is never reported, even where its packages are there
    # (`animetext` needs nothing `ppocr-manga` does not already install).
    detectors = (
        [d for d in OFFERED_DETECTOR_IDS if engines_installer.has_detector(d)]
        if engines_installer.is_installed()
        else []
    )
    devices = probe_devices(engines_python)
    catalog: dict[str, Any] = {
        "engines": sorted(set(installed)),
        "detectors": sorted(set(detectors)),
        "devices": devices.entries(),
        "serves_mokuro": mokuro_installer.is_installed(),
    }
    if devices.ort_gpu_providers is not None:
        # Which GPU execution providers this machine's onnxruntime offers
        # ([] for a CPU-only wheel), so the library never sends an
        # onnxruntime detector to a card it cannot reach. Left out when the
        # probe could not ask: unknown, which refuses nothing.
        catalog[ORT_CATALOG_KEY] = list(devices.ort_gpu_providers)
    gpu_facts = getattr(devices, "gpu_facts", None)
    facts = gpu_facts() if callable(gpu_facts) else []
    if facts:
        # What each card can compute in, by the probe (fp32 always; fp16 on a
        # card; bf16 where torch says so): the library judges a row's forced
        # precision by it. Optional -- a library that predates it ignores it.
        catalog[GPU_FACTS_KEY] = facts
    return catalog, describe_host(config.ocr.backend, engines_python)


def host_with_build(host: dict[str, Any], runner: Path) -> dict[str, Any]:
    """``host`` plus what software this processor reads pages with.

    Its mokuro-bunko version and the content hash of the runner it pinned
    (`ocr.staging`, the ``runner-<hash>`` directory): the library records
    both beside every sidecar this machine writes (`ocr.provenance`), so a
    bad file can be traced to the exact build that produced it. A library
    that predates them ignores the keys.
    """
    from mokuro_bunko import __version__
    from mokuro_bunko.ocr.staging import RUNNER_STAGE_PREFIX

    stage = runner.parent.name
    build = stage[len(RUNNER_STAGE_PREFIX):] if stage.startswith(RUNNER_STAGE_PREFIX) else None
    return {**host, "version": __version__, **({"runner_build": build} if build else {})}


def _serve_loop(config: Any, engines_python: Path | None, verbose: bool) -> None:
    """Register, take ops until the stream ends, and register again.

    Two endings are different from the rest. A refused LOGIN is final --
    retrying a wrong password forever helps nobody -- and a 409 on the
    stream is the library ASKING for a fresh registration, so it skips the
    backoff rather than sitting out five seconds it was not asked to.
    """
    import importlib

    from mokuro_bunko.ocr.staging import pin_runner
    from mokuro_bunko.processor.archives import ArchiveSpool

    # The runner build this process runs until it exits, staged in the same
    # breath as the bridge that drives it is imported (design section 5.1):
    # code updated on disk under a running processor must never reach its
    # next session as a newer runner than its bridge.
    importlib.import_module("mokuro_bunko.processor.bridge")
    runner = pin_runner(config.processor.storage)
    # Where every archive this process fetches is held: ONE spool, whose RAM
    # budget is the processor's whatever its sessions. Its disk fallback is
    # swept first -- leftovers of a run that crashed -- which is safe because
    # `serve` holds the storage lock.
    spool = ArchiveSpool(
        config.processor.storage, memory_mb=config.processor.archive_memory_mb
    )
    spool.sweep_named()
    try:
        _serve_with(config, engines_python, verbose, runner, spool)
    finally:
        spool.close()


def _serve_with(
    config: Any, engines_python: Path | None, verbose: bool, runner: Path, spool: Any
) -> None:
    from mokuro_bunko.processor.bridge import RunnerBridge
    from mokuro_bunko.processor.client import (
        LibraryClient,
        LibraryError,
        LibraryLoginRefused,
        ReregisterNeeded,
    )

    backoff = BACKOFF_START
    while True:
        client = LibraryClient(config)
        try:
            catalog, host = _catalog(config)
            client.register(catalog, host_with_build(host, runner))
        except LibraryLoginRefused as e:
            write_status(config.processor.storage, state="refused", error=str(e),
                         library=config.library.url)
            click.echo(f"Login refused: {e}", err=True)
            raise SystemExit(1) from None
        except LibraryError as e:
            write_status(config.processor.storage, state="unreachable", error=str(e),
                         library=config.library.url)
            click.echo(f"{e}; retrying in {backoff:.0f}s", err=True)
            time.sleep(backoff)
            backoff = min(backoff * 2, BACKOFF_MAX)
            continue
        # Only once there is something to bridge TO: a registration that
        # never landed has no sessions to shut down.
        bridge = RunnerBridge(
            client,
            storage=config.processor.storage,
            engines_python=engines_python,
            concurrency=config.processor.max_sessions,
            runner=runner,
            spool=spool,
        )
        backoff = BACKOFF_START
        write_status(config.processor.storage, state="connected",
                     library=config.library.url, name=config.processor.name)
        click.echo(
            f"Connected to {config.library.url} as {config.processor.name} "
            f"({len(catalog['engines'])} engine(s), "
            f"{config.processor.max_sessions} session slot(s))"
        )
        at_once = False
        try:
            for op in client.ops():
                if verbose:
                    logger.debug("op %s", op.get("op"))
                bridge.handle(op)
        except ReregisterNeeded as e:
            click.echo(f"{e}", err=True)
            at_once = True
        except LibraryError as e:
            click.echo(f"{e}", err=True)
        finally:
            bridge.shutdown()
            client.close()
        write_status(config.processor.storage, state="disconnected",
                     library=config.library.url)
        if at_once:
            time.sleep(REREGISTER_FLOOR)
            continue
        click.echo(f"Disconnected; reconnecting in {backoff:.0f}s", err=True)
        time.sleep(backoff)
        backoff = min(backoff * 2, BACKOFF_MAX)


@processor_group.command("serve")
@click.option(
    "--config", "config_path", required=True, type=click.Path(path_type=Path),
    envvar="MOKURO_PROCESSOR_CONFIG", help="Path to processor.yaml",
)
@click.option("-v", "--verbose", is_flag=True, help="Log every op and event")
def serve(config_path: Path, verbose: bool) -> None:
    """Log in to the library and process whatever it sends."""
    from mokuro_bunko.ocr.installer import EnginesInstaller

    logging.basicConfig(
        level=logging.DEBUG if verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )
    config = _config(config_path)
    storage = config.processor.storage
    # FIRST, before anything touches the storage: a second processor on it
    # must be refused before it can stage (and prune) a runner, sweep the
    # spool or overwrite the status file of the one that is running.
    lock = lock_storage(storage)
    if lock is None:
        click.echo(
            f"another processor is running on {storage}; "
            "give each processor its own `storage`",
            err=True,
        )
        raise SystemExit(1)
    try:
        engines_python = _engines_python(EnginesInstaller())

        def stop(_signum: int, _frame: Any) -> None:
            raise KeyboardInterrupt

        signal.signal(signal.SIGTERM, stop)

        try:
            _serve_loop(config, engines_python, verbose)
        except KeyboardInterrupt:
            pass
        write_status(storage, state="stopped", library=config.library.url)
        click.echo("Processor stopped")
    finally:
        lock.close()


@processor_group.command("install")
@click.option(
    "--config", "config_path", required=True, type=click.Path(path_type=Path),
    envvar="MOKURO_PROCESSOR_CONFIG", help="Path to processor.yaml",
)
@click.option("--force", is_flag=True, help="Reinstall even if already installed")
@click.option(
    "--engines", default=None,
    help="Comma-separated engines to install. Default: every engine bunko ships.",
)
@click.option(
    "--detector", default=DEFAULT_DETECTOR,
    type=click.Choice(OFFERED_DETECTOR_IDS),
    show_default=True, help="Text detector to install alongside the engines.",
)
def install(config_path: Path, force: bool, engines: str | None, detector: str) -> None:
    """Install the OCR environments, with the same installers the server uses."""
    install_environments(config_path, force=force, engines=engines, detector=detector)


def install_environments(
    config_path: Path,
    *,
    force: bool = False,
    engines: str | None = None,
    detector: str = DEFAULT_DETECTOR,
) -> None:
    """`processor install`, for it and for `processor setup`."""
    from mokuro_bunko.ocr.engines import ENGINES, MOKURO_ENGINE, get_engine, uses_mokuro_env
    from mokuro_bunko.ocr.installer import (
        EnginesInstaller,
        OCRBackend,
        OCRInstaller,
        detect_hardware,
        get_recommended_backend,
        get_supported_backends,
    )

    config = _config(config_path)
    ids = (
        [get_engine(part.strip()).id for part in engines.split(",") if part.strip()]
        if engines
        else sorted(ENGINES)
    )
    hardware = detect_hardware()
    supported = get_supported_backends(hardware=hardware)
    if config.ocr.backend == "auto":
        backend = get_recommended_backend(hardware=hardware, supported_backends=supported)
    else:
        backend = OCRBackend(config.ocr.backend)
    click.echo(f"Installing for backend: {backend.value}")
    if any(uses_mokuro_env(engine) for engine in ids) or MOKURO_ENGINE in ids:
        if not OCRInstaller(output_callback=click.echo).install_with_fallback(
            backend, force=force, hardware=hardware
        ):
            raise click.ClickException("the mokuro environment failed to install")
    extra = [engine for engine in ids if not uses_mokuro_env(engine)]
    if extra:
        wanted = list(dict.fromkeys(get_engine(e).detector or detector for e in extra))
        if not EnginesInstaller(
            output_callback=click.echo, detector=detector, detectors=wanted
        ).install_with_fallback(backend, force=force, hardware=hardware):
            raise click.ClickException("the engines environment failed to install")
    click.echo("Processor environments ready")


@processor_group.command("service")
@click.option(
    "--config", "config_path", required=True, type=click.Path(path_type=Path),
    envvar="MOKURO_PROCESSOR_CONFIG", help="Path to processor.yaml",
)
@click.option(
    "--install", "do_install", is_flag=True,
    help="Write it to ~/.config/systemd/user and enable + start it",
)
def service(config_path: Path, do_install: bool) -> None:
    """Run this processor as you, from this install: a systemd user unit, or on
    Windows a Startup entry."""
    from mokuro_bunko.processor import service as unit

    if do_install:
        install_service(config_path)
        return
    _config(config_path)  # refuse a config `serve` would refuse
    try:
        if sys.platform == "win32":
            text = unit.render_windows_startup(config_path, entry_point=unit.entry_point())
            click.echo(f"# {unit.startup_dir() / unit.STARTUP_NAME}")
        else:
            text = unit.render_user_unit(config_path, entry_point=unit.entry_point())
    except unit.ServiceError as e:
        raise click.ClickException(str(e)) from e
    click.echo(text.replace("\r\n", "\n"), nl=False)


def install_service(config_path: Path) -> None:
    """`processor service --install`, for it and for `processor setup`."""
    from mokuro_bunko.processor import service as unit

    _config(config_path)  # refuse a config `serve` would refuse
    if sys.platform == "win32":
        try:
            entry = unit.install_windows_startup(config_path, entry_point=unit.entry_point())
        except unit.ServiceError as e:
            raise click.ClickException(str(e)) from e
        click.echo(f"Added {entry}: the processor starts, minimized, at every logon.")
        click.echo("Started it now in its own window. To stop starting it, delete that file.")
        return
    try:
        text = unit.render_user_unit(config_path, entry_point=unit.entry_point())
        path, lingers = unit.install_user_unit(text)
    except unit.ServiceError as e:
        raise click.ClickException(str(e)) from e
    click.echo(f"Installed {path} and started it.")
    click.echo(f"Follow it with: journalctl --user -u {unit.UNIT_NAME} -f")
    if not lingers:
        user = os.environ.get("USER") or Path.home().name
        click.echo(
            "This account does not linger, so the processor stops when you log out and "
            f"does not start at boot. To keep it running: loginctl enable-linger {user}"
        )


@processor_group.command("setup")
@click.option(
    "--config", "config_path", default="processor.yaml", show_default=True,
    type=click.Path(dir_okay=False, path_type=Path), help="Where to write processor.yaml",
)
@click.option("--url", help="The library's address, as a browser reaches it.")
@click.option("--username", help="A processor account on that library.")
@click.option(
    "--password-stdin", is_flag=True,
    help="Read the password from the first line of stdin (there is no --password: "
    "it would end up in the shell's history).",
)
@click.option("--name", help="How the library shows this machine.  [default: the hostname]")
@click.option(
    "--backend", type=click.Choice(VALID_BACKENDS), default="auto", show_default=True,
    help="Which torch build to install; auto picks by the hardware found.",
)
@click.option(
    "--tls-verify", default="true", show_default=True,
    help="true, false, or the path of the library's certificate (a self-signed one).",
)
@click.option("-y", "--yes", is_flag=True, help="Ask nothing: accept every default.")
@click.option("--no-install", is_flag=True, help="Do not install the OCR environments.")
@click.option(
    "--no-service", is_flag=True,
    help="Do not set up starting it (a systemd user service; on Windows a Startup entry).",
)
@click.option("--force", is_flag=True, help="Overwrite an existing config file.")
def setup(
    config_path: Path,
    url: str | None,
    username: str | None,
    password_stdin: bool,
    name: str | None,
    backend: str,
    tls_verify: str,
    yes: bool,
    no_install: bool,
    no_service: bool,
    force: bool,
) -> None:
    """Set this machine up as a processor: check the account, write the config,
    install, and start it.

    Asks for the library URL, username and password when they are not given,
    checks them against the library before writing anything, and writes only
    the settings that differ from the defaults.
    """
    from mokuro_bunko.processor import setup as wizard

    try:
        password = None
        if password_stdin:
            password = sys.stdin.readline().rstrip("\r\n")
            if not password:
                raise wizard.SetupError("--password-stdin: no password on stdin")
        options = wizard.Options(
            config=config_path,
            url=url,
            username=username,
            password=password,
            name=name,
            backend=backend,
            tls_verify=wizard.parse_tls_verify(tls_verify),
            yes=yes,
            install=not no_install,
            service=not no_service,
            force=force,
        )
        # Looked up when called, so each step is the command's own logic.
        wizard.run(
            options,
            install_step=lambda path: install_environments(path),
            service_step=lambda path: install_service(path),
        )
    except wizard.SetupError as e:
        raise click.ClickException(str(e)) from None


@processor_group.command("status")
@click.option(
    "--config", "config_path", required=True, type=click.Path(path_type=Path),
    envvar="MOKURO_PROCESSOR_CONFIG", help="Path to processor.yaml",
)
def status(config_path: Path) -> None:
    """What this processor last did."""
    config = _config(config_path)
    click.echo(describe(read_status(config.processor.storage)))
