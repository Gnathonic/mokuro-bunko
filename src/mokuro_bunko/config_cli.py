"""CLI commands for configuration management."""

from __future__ import annotations

import sys
from pathlib import Path

import click
import yaml

from mokuro_bunko.config import (
    Config,
    get_default_config_path,
    load_config,
    save_config,
    set_by_dotted_key,
)


@click.group(name="config")
def config_group() -> None:
    """Manage configuration."""
    pass


def _load(config_path: Path | None) -> Config:
    """Load the config, reporting a bad one as a message rather than a trace.

    A config that cannot be read is an ordinary thing to hit from the command
    line -- a typo, or a key that was replaced (``ocr.engines`` and friends
    became ``ocr.generations``) -- and the validator's sentence is exactly
    what the operator needs to act on.
    """
    try:
        return load_config(config_path)
    except (ValueError, yaml.YAMLError) as e:
        raise click.ClickException(str(e)) from e


@config_group.command("show")
@click.pass_context
def config_show(ctx: click.Context) -> None:
    """Show current configuration as YAML."""
    config_path = ctx.obj.get("config_path")
    config = _load(config_path)
    yaml.safe_dump(config.to_dict(), sys.stdout, default_flow_style=False)


@config_group.command("set")
@click.argument("key")
@click.argument("value")
@click.pass_context
def config_set(ctx: click.Context, key: str, value: str) -> None:
    """Set a configuration value by dotted key path.

    Examples: config set server.port 8443, config set registration.mode invite
    """
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = _load(config_path)

    try:
        set_by_dotted_key(config, key, value)
    except KeyError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)

    save_config(config, config_path)
    click.echo(f"Set {key} = {value}")


@config_group.command("path")
@click.pass_context
def config_path(ctx: click.Context) -> None:
    """Show config file and storage locations.

    The file is the one every other config command reads and writes (``-c``
    or ``MOKURO_CONFIG``, else the default), and the storage directory is the
    one that file gives the server.
    """
    path = Path(ctx.obj.get("config_path") or get_default_config_path())
    click.echo(f"Config file: {path}")
    try:
        storage = load_config(path).storage.base_path
    except (OSError, ValueError, yaml.YAMLError) as e:
        click.echo(f"Storage dir: unknown -- the config file cannot be read ({e})")
        return
    click.echo(f"Storage dir: {storage}")


@config_group.command("init")
@click.option("--force", is_flag=True, help="Overwrite existing config file")
@click.pass_context
def config_init(ctx: click.Context, force: bool) -> None:
    """Create a default configuration file."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    path = Path(config_path)

    if path.exists() and not force:
        click.echo(f"Error: Config file already exists at {path}", err=True)
        click.echo("Use --force to overwrite", err=True)
        sys.exit(1)

    config = Config()
    save_config(config, path)
    click.echo(f"Created config file at {path}")


@config_group.command("cors-add")
@click.argument("origin")
@click.pass_context
def cors_add(ctx: click.Context, origin: str) -> None:
    """Add a CORS allowed origin."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = _load(config_path)

    if origin in config.cors.allowed_origins:
        click.echo(f"Origin already allowed: {origin}")
        return

    config.cors.allowed_origins.append(origin)
    save_config(config, config_path)
    click.echo(f"Added CORS origin: {origin}")


@config_group.command("cors-remove")
@click.argument("origin")
@click.pass_context
def cors_remove(ctx: click.Context, origin: str) -> None:
    """Remove a CORS allowed origin."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    config = _load(config_path)

    if origin not in config.cors.allowed_origins:
        click.echo(f"Error: Origin not found: {origin}", err=True)
        sys.exit(1)

    config.cors.allowed_origins.remove(origin)
    save_config(config, config_path)
    click.echo(f"Removed CORS origin: {origin}")
