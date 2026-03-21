"""CLI commands for configuration management."""

from __future__ import annotations

import sys
from pathlib import Path
from urllib.parse import urlparse

import click
import yaml

from mokuro_bunko.config import (
    Config,
    get_default_config_path,
    get_default_storage_path,
    load_config,
    save_config,
    set_by_dotted_key,
)
from mokuro_bunko.ssl import validate_certificate_pair


@click.group(name="config")
def config_group() -> None:
    """Manage configuration."""
    pass


@config_group.command("show")
@click.pass_context
def config_show(ctx: click.Context) -> None:
    """Show current configuration as YAML."""
    config_path = ctx.obj.get("config_path")
    config = load_config(config_path)
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
    config = load_config(config_path)

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
def config_path() -> None:
    """Show config file and storage locations."""
    click.echo(f"Config file: {get_default_config_path()}")
    click.echo(f"Storage dir: {get_default_storage_path()}")


@config_group.command("check")
@click.pass_context
def config_check(ctx: click.Context) -> None:
    """Check config validity and print problems."""
    config_path = ctx.obj.get("config_path")
    try:
        config = load_config(config_path)
    except Exception as e:
        click.echo(f"Config loading failed: {e}", err=True)
        sys.exit(1)

    errors: list[str] = []
    warnings: list[str] = []

    if not config.storage.base_path.exists():
        errors.append(f"Storage base path does not exist: {config.storage.base_path}")
    else:
        probe = config.storage.base_path / ".config-check-write"
        try:
            with probe.open("wb") as handle:
                handle.write(b"ok")
            probe.unlink(missing_ok=True)
        except OSError:
            errors.append(f"Storage base path is not writable: {config.storage.base_path}")

    if config.ssl.enabled and not config.ssl.auto_cert:
        if not config.ssl.cert_file:
            errors.append("SSL enabled but cert_file is empty")
        if not config.ssl.key_file:
            errors.append("SSL enabled but key_file is empty")
        cert_path = Path(config.ssl.cert_file).expanduser() if config.ssl.cert_file else None
        key_path = Path(config.ssl.key_file).expanduser() if config.ssl.key_file else None
        if cert_path is not None and (not cert_path.exists() or not cert_path.is_file()):
            errors.append(f"SSL cert_file not found: {cert_path}")
        elif cert_path is not None:
            try:
                with cert_path.open("rb"):
                    pass
            except OSError:
                errors.append(f"SSL cert_file is not readable: {cert_path}")
        if key_path is not None and (not key_path.exists() or not key_path.is_file()):
            errors.append(f"SSL key_file not found: {key_path}")
        elif key_path is not None:
            try:
                with key_path.open("rb"):
                    pass
            except OSError:
                errors.append(f"SSL key_file is not readable: {key_path}")

        if cert_path is not None and key_path is not None and not errors:
            ssl_errors, ssl_warnings = validate_certificate_pair(cert_path, key_path)
            errors.extend(ssl_errors)
            warnings.extend(ssl_warnings)

    for origin in config.cors.allowed_origins:
        if not _is_valid_origin_pattern(origin):
            errors.append(f"Invalid CORS origin pattern: {origin}")

    if config.registration.mode not in ("disabled", "self", "invite", "approval"):
        errors.append(f"Invalid registration mode: {config.registration.mode}")

    if not config.admin.path.startswith("/"):
        errors.append("admin.path must start with '/'")

    if config.ocr.no_progress_timeout_seconds >= config.ocr.hard_timeout_seconds:
        errors.append("ocr.no_progress_timeout_seconds should be less than ocr.hard_timeout_seconds")

    if config.ocr.finalizing_timeout_seconds >= config.ocr.hard_timeout_seconds:
        errors.append("ocr.finalizing_timeout_seconds should be less than ocr.hard_timeout_seconds")

    if config.database.retry_initial_delay_seconds > config.database.connect_timeout_seconds:
        errors.append(
            "database.retry_initial_delay_seconds should not exceed database.connect_timeout_seconds"
        )

    if errors:
        click.echo("Config check failed with the following issues:")
        for err in errors:
            click.echo(f" - {err}")
        sys.exit(1)

    if warnings:
        click.echo("Config check warnings:")
        for warning in warnings:
            click.echo(f" - {warning}")

    click.echo("Config check passed.")


def _is_valid_origin_pattern(origin: str) -> bool:
    """Validate exact origins and wildcard-port origin patterns."""
    if not isinstance(origin, str):
        return False
    candidate = origin.strip()
    if not candidate:
        return False

    if candidate.endswith(":*"):
        parsed = urlparse(candidate[:-2])
        return parsed.scheme in ("http", "https") and bool(parsed.hostname) and parsed.path in ("", "/")

    parsed = urlparse(candidate)
    if parsed.scheme not in ("http", "https"):
        return False
    if not parsed.hostname:
        return False
    if parsed.path not in ("", "/"):
        return False
    if parsed.query or parsed.fragment or parsed.params:
        return False
    return True


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
    config = load_config(config_path)

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
    config = load_config(config_path)

    if origin not in config.cors.allowed_origins:
        click.echo(f"Error: Origin not found: {origin}", err=True)
        sys.exit(1)

    config.cors.allowed_origins.remove(origin)
    save_config(config, config_path)
    click.echo(f"Removed CORS origin: {origin}")
