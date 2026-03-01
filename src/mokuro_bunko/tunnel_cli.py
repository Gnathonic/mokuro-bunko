"""CLI commands for tunnel management."""

from __future__ import annotations

import re
import shutil
import subprocess
import sys

import click

from mokuro_bunko.config import get_default_config_path, load_config, save_config


@click.group(name="tunnel")
def tunnel_group() -> None:
    """Manage tunnels for remote access."""
    pass


@tunnel_group.command("status")
def tunnel_status() -> None:
    """Check if cloudflared is installed."""
    path = shutil.which("cloudflared")
    if not path:
        click.echo("cloudflared: not installed")
        click.echo("Install from: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/")
        return

    click.echo(f"cloudflared: {path}")
    try:
        result = subprocess.run(
            ["cloudflared", "version"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        version_output = result.stdout.strip() or result.stderr.strip()
        if version_output:
            click.echo(f"Version: {version_output}")
    except Exception as e:
        click.echo(f"Could not get version: {e}", err=True)


@tunnel_group.command("cloudflare")
@click.option(
    "--port",
    type=int,
    default=None,
    help="Local port to tunnel (auto-detects from config)",
)
@click.pass_context
def tunnel_cloudflare(ctx: click.Context, port: int | None) -> None:
    """Start a Cloudflare quick tunnel."""
    if not shutil.which("cloudflared"):
        click.echo("Error: cloudflared is not installed", err=True)
        click.echo(
            "Install from: https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/",
            err=True,
        )
        sys.exit(1)

    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = load_config(config_path)

    if port is None:
        port = config.server.port

    protocol = "https" if config.ssl.enabled else "http"
    local_url = f"{protocol}://localhost:{port}"

    click.echo(f"Starting Cloudflare tunnel for {local_url}...")
    click.echo("Press Ctrl+C to stop\n")

    cmd = ["cloudflared", "tunnel", "--url", local_url]

    try:
        process = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        tunnel_url = None
        # cloudflared prints the URL to stderr
        for line in iter(process.stderr.readline, ""):  # type: ignore[union-attr]
            click.echo(line, nl=False, err=True)
            if not tunnel_url:
                match = re.search(r"(https://[a-z0-9-]+\.trycloudflare\.com)", line)
                if match:
                    tunnel_url = match.group(1)
                    click.echo(f"\nTunnel URL: {tunnel_url}\n")

                    if click.confirm("Add tunnel URL to CORS allowed origins?", default=True):
                        if tunnel_url not in config.cors.allowed_origins:
                            config.cors.allowed_origins.append(tunnel_url)
                            save_config(config, config_path)
                            click.echo(f"Added {tunnel_url} to CORS origins")

        process.wait()
    except KeyboardInterrupt:
        click.echo("\nStopping tunnel...")
        process.terminate()
        process.wait(timeout=5)
    except Exception as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)
