"""CLI entry point for mokuro-bunko."""

from __future__ import annotations

from pathlib import Path

import click

from mokuro_bunko import __version__
from mokuro_bunko.admin.cli import admin_group
from mokuro_bunko.config_cli import config_group
from mokuro_bunko.dyndns_cli import dyndns_group
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
@click.pass_context
def serve(ctx: click.Context, host: str, port: int, ocr: str) -> None:
    """Start the WebDAV server."""
    from mokuro_bunko.config import load_config
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
        config.ocr.backend = ocr

    if verbose:
        click.echo("Verbose mode enabled")
        click.echo(f"Storage path: {config.storage.base_path}")

    # Start the server
    run_server(config, config_path)


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
def install_ocr(force: bool, backend: str, list_backends: bool) -> None:
    """Install or reinstall OCR dependencies."""
    from mokuro_bunko.ocr.installer import (
        OCRBackend,
        OCRInstaller,
        detect_hardware,
        get_backend_unavailable_reasons,
        get_recommended_backend,
        get_supported_backends,
    )

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

    success = installer.install_with_fallback(backend_enum, force=force, hardware=hardware)
    if not success:
        raise click.ClickException("OCR installation failed")
    click.echo("OCR installation completed")


# Register command groups
cli.add_command(admin_group, name="admin")
cli.add_command(config_group, name="config")
cli.add_command(ssl_group, name="ssl")
cli.add_command(setup_command, name="setup")
cli.add_command(tunnel_group, name="tunnel")
cli.add_command(dyndns_group, name="dyndns")


def main() -> None:
    """Main entry point."""
    cli(obj={})


if __name__ == "__main__":
    main()
