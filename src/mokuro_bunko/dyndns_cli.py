"""CLI commands for Dynamic DNS management."""

from __future__ import annotations

import click

from mokuro_bunko.config import (
    DynDNSConfig,
    get_default_config_path,
    load_config,
    save_config,
)


@click.group(name="dyndns")
def dyndns_group() -> None:
    """Manage Dynamic DNS settings."""
    pass


@dyndns_group.command("setup")
@click.pass_context
def dyndns_setup(ctx: click.Context) -> None:
    """Interactive DynDNS setup."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = load_config(config_path)

    click.echo("=== DynDNS Setup ===\n")

    provider = click.prompt(
        "Provider",
        type=click.Choice(["duckdns", "generic"]),
        default="duckdns",
    )

    domain = click.prompt("Domain", default=config.dyndns.domain or "")

    token = click.prompt("API Token", hide_input=True)

    update_url = ""
    if provider == "generic":
        update_url = click.prompt(
            "Update URL (use {ip}, {domain}, {token} as placeholders)",
            default=config.dyndns.update_url or "",
        )

    interval = click.prompt("Update interval (seconds)", default=300, type=int)

    enabled = click.confirm("Enable DynDNS?", default=True)

    config.dyndns = DynDNSConfig(
        enabled=enabled,
        provider=provider,  # type: ignore[arg-type]
        token=token,
        domain=domain,
        update_url=update_url,
        interval=interval,
    )

    save_config(config, config_path)
    click.echo(f"\nDynDNS configuration saved to {config_path}")

    if enabled:
        click.echo("DynDNS will start automatically when the server runs.")


@dyndns_group.command("status")
@click.pass_context
def dyndns_status(ctx: click.Context) -> None:
    """Show current DynDNS configuration."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = load_config(config_path)

    d = config.dyndns
    click.echo(f"Enabled:   {d.enabled}")
    click.echo(f"Provider:  {d.provider}")
    click.echo(f"Domain:    {d.domain or '(not set)'}")
    click.echo(f"Token:     {'****' if d.token else '(not set)'}")
    click.echo(f"Interval:  {d.interval}s")
    if d.provider == "generic":
        click.echo(f"URL:       {d.update_url or '(not set)'}")


@dyndns_group.command("update")
@click.pass_context
def dyndns_update(ctx: click.Context) -> None:
    """Force an immediate DNS update."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = load_config(config_path)

    if not config.dyndns.token or not config.dyndns.domain:
        click.echo("Error: DynDNS not configured. Run 'mokuro-bunko dyndns setup' first.", err=True)
        return

    from mokuro_bunko.dyndns import DynDNSService

    service = DynDNSService(config.dyndns)
    click.echo(f"Updating DNS for {config.dyndns.domain}...")
    result = service.update_now()

    if result.get("success"):
        click.echo(f"Success! IP: {result.get('ip', 'unknown')}")
    else:
        click.echo(f"Failed: {result.get('error', 'unknown error')}", err=True)


@dyndns_group.command("enable")
@click.pass_context
def dyndns_enable(ctx: click.Context) -> None:
    """Enable DynDNS updates."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = load_config(config_path)
    config.dyndns.enabled = True
    save_config(config, config_path)
    click.echo("DynDNS enabled. Restart the server for changes to take effect.")


@dyndns_group.command("disable")
@click.pass_context
def dyndns_disable(ctx: click.Context) -> None:
    """Disable DynDNS updates."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = load_config(config_path)
    config.dyndns.enabled = False
    save_config(config, config_path)
    click.echo("DynDNS disabled. Restart the server for changes to take effect.")
