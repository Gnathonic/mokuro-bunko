"""CLI entry point for mokuro-bunko."""

from __future__ import annotations

import sys
from pathlib import Path
from typing import cast

import click

from mokuro_bunko import __version__
from mokuro_bunko.admin.cli import admin_group
from mokuro_bunko.config import OcrBackend
from mokuro_bunko.config_cli import config_group
from mokuro_bunko.doctor_cli import doctor_command
from mokuro_bunko.dyndns_cli import dyndns_group
from mokuro_bunko.ocr.engines import DEFAULT_DETECTOR, OFFERED_DETECTOR_IDS
from mokuro_bunko.processor.cli import processor_group
from mokuro_bunko.setup_cli import setup_command
from mokuro_bunko.ssl_cli import ssl_group
from mokuro_bunko.tunnel_cli import tunnel_group


@click.group(invoke_without_command=True)
@click.option(
    "-c",
    "--config",
    type=click.Path(path_type=Path),
    envvar="MOKURO_CONFIG",
    help="Path to configuration file",
)
@click.option("-v", "--verbose", is_flag=True, help="Enable verbose output")
@click.version_option(version=__version__, prog_name="mokuro-bunko")
@click.pass_context
def cli(ctx: click.Context, config: Path | None, verbose: bool) -> None:
    """Mokuro Bunko Server - Manga library with OCR support."""
    ctx.ensure_object(dict)
    ctx.obj["config_path"] = config
    ctx.obj["verbose"] = verbose

    if ctx.invoked_subcommand is None:
        click.echo(ctx.get_help())


@cli.command()
@click.option(
    "--host",
    default="0.0.0.0",
    help="Host to bind to",
    show_default=True,
)
@click.option(
    "--port",
    default=8080,
    type=int,
    help="Port to listen on",
    show_default=True,
)
@click.option(
    "--ocr",
    type=click.Choice(["auto", "cuda", "rocm", "cpu", "skip"]),
    default="auto",
    help="OCR backend to use",
    show_default=True,
)
@click.option(
    "--generations",
    default=None,
    help=(
        "OCR generations as JSON, in run order, overriding ocr.generations: "
        '\'[{"name": "mokuro", "engine": "mokuro", "primary": true}, '
        '{"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"}]\'. '
        "Each row owns its engine, detector, patch_budget and pools "
        "and writes a file named after it"
    ),
)
@click.pass_context
def serve(
    ctx: click.Context,
    host: str,
    port: int,
    ocr: str,
    generations: str | None,
) -> None:
    """Start the WebDAV server."""
    from mokuro_bunko.config import load_config
    from mokuro_bunko.ocr.generations import parse_generation_list
    from mokuro_bunko.server import run_server

    config_path = ctx.obj.get("config_path")
    verbose = ctx.obj.get("verbose", False)

    config = load_config(config_path)

    # Override config with CLI options
    if host != "0.0.0.0":
        config.server.host = host
    if port != 8080:
        config.server.port = port
    if ocr != "auto":
        config.ocr.backend = cast("OcrBackend", ocr)
    if generations:
        try:
            config.ocr.generations = parse_generation_list(generations)
        except ValueError as e:
            raise click.ClickException(str(e)) from e

    if verbose:
        click.echo("Verbose mode enabled")
        click.echo(f"Storage path: {config.storage.base_path}")

    # Start the server
    run_server(config, config_path, verbose=verbose)


@cli.command()
@click.option(
    "--force",
    is_flag=True,
    help="Reinstall OCR even if already installed",
)
@click.option(
    "--backend",
    type=click.Choice(["auto", "cuda", "rocm", "cpu"]),
    default="auto",
    help="OCR backend to install",
    show_default=True,
)
@click.option(
    "--list-backends",
    is_flag=True,
    help="Show backends available on this host and exit",
)
@click.option(
    "--engines",
    default=None,
    help=(
        "Comma-separated OCR engines to install (mokuro, "
        "hayai-nova, paddle-manga, ppocr-manga). mokuro uses the classic OCR environment; "
        "the others share a second environment. Default: mokuro only."
    ),
)
@click.option(
    "--detector",
    default=DEFAULT_DETECTOR,
    type=click.Choice(OFFERED_DETECTOR_IDS),
    show_default=True,
    help=(
        "Text detector for the hayai-nova / paddle-manga engines. ppocr-manga "
        "(rotated line quads) is Apache-2.0; ctd (comic-text-detector via the "
        "mokuro package) is a GPL-3.0 opt-in."
    ),
)
def install_ocr(
    force: bool, backend: str, list_backends: bool, engines: str | None, detector: str
) -> None:
    """Install or reinstall OCR dependencies."""
    from mokuro_bunko.ocr.engines import MOKURO_ENGINE, get_engine, uses_mokuro_env
    from mokuro_bunko.ocr.installer import (
        EnginesInstaller,
        OCRBackend,
        OCRInstaller,
        detect_hardware,
        get_backend_unavailable_reasons,
        get_recommended_backend,
        get_supported_backends,
    )

    try:
        # An ENVIRONMENT list, not a queue: this flag says which engines to
        # install packages for, so the same engine twice is simply once.
        engine_ids = list(
            dict.fromkeys(
                get_engine(part.strip()).id
                for part in (engines or MOKURO_ENGINE).split(",")
                if part.strip()
            )
        ) or [MOKURO_ENGINE]
    except ValueError as e:
        raise click.ClickException(str(e)) from e

    installer = OCRInstaller(output_callback=click.echo)
    hardware = detect_hardware()
    supported_backends = get_supported_backends(hardware=hardware)
    unavailable_reasons = get_backend_unavailable_reasons(hardware=hardware)

    if list_backends:
        click.echo("Supported OCR backends:")
        for option in supported_backends:
            click.echo(f"  - {option.value}")
        hidden = [b for b in (OCRBackend.CUDA, OCRBackend.ROCM, OCRBackend.MPS) if b not in supported_backends]
        if hidden:
            click.echo("Unavailable backends:")
            for option in hidden:
                reason = unavailable_reasons.get(option, "Unavailable")
                click.echo(f"  - {option.value}: {reason}")
        return

    click.echo(f"Installing OCR with backend: {backend}")
    if force:
        click.echo("Force reinstall enabled")

    available_labels = ", ".join(b.value for b in supported_backends)
    click.echo(f"Available backends on this host: {available_labels}")

    if backend == "auto":
        backend_enum = get_recommended_backend(
            hardware=hardware,
            supported_backends=supported_backends,
        )
        click.echo(f"Auto-selected backend: {backend_enum.value}")
    else:
        backend_enum = OCRBackend(backend)
        if backend_enum not in supported_backends:
            reason = unavailable_reasons.get(
                backend_enum,
                f"Backend {backend_enum.value} is not supported on this host",
            )
            raise click.ClickException(
                f"Requested backend '{backend_enum.value}' is unavailable: {reason}"
            )

    if any(uses_mokuro_env(e) for e in engine_ids):
        success = installer.install_with_fallback(backend_enum, force=force, hardware=hardware)
        if not success:
            raise click.ClickException("OCR installation failed")
        click.echo("OCR installation completed")

    extra_engines = [e for e in engine_ids if not uses_mokuro_env(e)]
    if extra_engines:
        click.echo(
            f"Installing OCR engines environment for: {', '.join(extra_engines)} (detector: {detector})"
        )
        # The configured detector plus the one built into an engine that
        # brings its own (ppocr-manga): whichever this environment will use.
        wanted = list(
            dict.fromkeys(get_engine(e).detector or detector for e in extra_engines)
        )
        engines_installer = EnginesInstaller(
            output_callback=click.echo, detector=detector, detectors=wanted
        )
        success = engines_installer.install_with_fallback(backend_enum, force=force, hardware=hardware)
        if not success:
            raise click.ClickException("OCR engines installation failed")
        click.echo(f"OCR engines installation completed at {engines_installer.env_path}")


# Register command groups
cli.add_command(admin_group, name="admin")
cli.add_command(config_group, name="config")
cli.add_command(doctor_command, name="doctor")
cli.add_command(ssl_group, name="ssl")
cli.add_command(setup_command, name="setup")
cli.add_command(tunnel_group, name="tunnel")
cli.add_command(dyndns_group, name="dyndns")
cli.add_command(processor_group, name="processor")


def tolerant_console_streams(*streams: object) -> None:
    """Never die printing a character the console cannot show.

    A Windows console, or output redirected through one, can run on a legacy
    code page (cp1252) that has no arrow, no kana and no box drawing, and
    Python's default for such a stream is to raise. A stream that is not
    UTF-8 gets ``errors="replace"``: the odd character prints as "?", and the
    command goes on.
    """
    for stream in streams:
        encoding = str(getattr(stream, "encoding", "") or "").lower().replace("_", "-")
        if encoding in ("utf-8", "utf8"):
            continue
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure is None:
            continue
        try:
            reconfigure(errors="replace")
        except (ValueError, OSError):
            pass


def main() -> None:
    """Main entry point."""
    tolerant_console_streams(sys.stdout, sys.stderr)
    cli(obj={})


if __name__ == "__main__":
    main()
