"""CLI commands for SSL management."""

from __future__ import annotations

import sys
from pathlib import Path

import click

from mokuro_bunko.config import get_default_config_path, load_config, save_config
from mokuro_bunko.ssl import generate_self_signed_cert, get_default_cert_paths


@click.group(name="ssl")
def ssl_group() -> None:
    """Manage SSL certificates."""
    pass


@ssl_group.command("enable")
@click.option("--auto-cert", is_flag=True, help="Generate a self-signed certificate")
@click.option("--cert", type=click.Path(exists=True), help="Path to certificate file")
@click.option("--key", type=click.Path(exists=True), help="Path to private key file")
@click.pass_context
def ssl_enable(
    ctx: click.Context,
    auto_cert: bool,
    cert: str | None,
    key: str | None,
) -> None:
    """Enable SSL."""
    if not auto_cert and not (cert and key):
        click.echo(
            "Error: Provide --auto-cert or both --cert and --key",
            err=True,
        )
        sys.exit(1)

    if (cert and not key) or (key and not cert):
        click.echo("Error: Both --cert and --key are required", err=True)
        sys.exit(1)

    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = load_config(config_path)

    config.ssl.enabled = True

    if auto_cert:
        config.ssl.auto_cert = True
        config.ssl.cert_file = ""
        config.ssl.key_file = ""

        cert_path, key_path = get_default_cert_paths()
        if not cert_path.exists():
            click.echo(f"Generating self-signed certificate...")
            generate_self_signed_cert(cert_path, key_path)
            click.echo(f"Certificate: {cert_path}")
            click.echo(f"Key: {key_path}")
    else:
        config.ssl.auto_cert = False
        config.ssl.cert_file = cert  # type: ignore[assignment]
        config.ssl.key_file = key  # type: ignore[assignment]

    save_config(config, config_path)
    click.echo("SSL enabled")


@ssl_group.command("disable")
@click.pass_context
def ssl_disable(ctx: click.Context) -> None:
    """Disable SSL."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = load_config(config_path)

    config.ssl.enabled = False
    config.ssl.auto_cert = False
    save_config(config, config_path)
    click.echo("SSL disabled")


@ssl_group.command("status")
@click.pass_context
def ssl_status(ctx: click.Context) -> None:
    """Show SSL status and certificate details."""
    config_path = ctx.obj.get("config_path")
    config = load_config(config_path)

    if not config.ssl.enabled:
        click.echo("SSL: disabled")
        return

    click.echo("SSL: enabled")

    if config.ssl.auto_cert:
        cert_path, _ = get_default_cert_paths()
        click.echo(f"Mode: auto-cert")
    else:
        cert_path = Path(config.ssl.cert_file)
        click.echo(f"Mode: custom certificate")

    click.echo(f"Certificate: {cert_path}")

    if not cert_path.exists():
        click.echo("Certificate file not found (will be generated on server start)")
        return

    try:
        from cryptography import x509
        from cryptography.hazmat.primitives.serialization import Encoding

        with open(cert_path, "rb") as f:
            cert = x509.load_pem_x509_certificate(f.read())

        click.echo(f"Subject: {cert.subject.rfc4514_string()}")
        click.echo(f"Not before: {cert.not_valid_before_utc}")
        click.echo(f"Not after: {cert.not_valid_after_utc}")

        try:
            san = cert.extensions.get_extension_for_class(
                x509.SubjectAlternativeName
            )
            names = san.value.get_values_for_type(x509.DNSName)
            if names:
                click.echo(f"SANs: {', '.join(names)}")
        except x509.ExtensionNotFound:
            pass

    except ImportError:
        click.echo("Install 'cryptography' package for certificate details")
    except Exception as e:
        click.echo(f"Could not read certificate: {e}", err=True)


@ssl_group.command("generate")
@click.option("--hostname", default="localhost", show_default=True, help="Hostname for the certificate")
@click.option("--days", default=365, show_default=True, type=int, help="Validity in days")
def ssl_generate(hostname: str, days: int) -> None:
    """Generate a self-signed certificate."""
    cert_path, key_path = get_default_cert_paths()

    if cert_path.exists():
        if not click.confirm(f"Certificate already exists at {cert_path}. Overwrite?"):
            return

    click.echo(f"Generating self-signed certificate for '{hostname}'...")
    generate_self_signed_cert(cert_path, key_path, hostname=hostname, validity_days=days)
    click.echo(f"Certificate: {cert_path}")
    click.echo(f"Key: {key_path}")
