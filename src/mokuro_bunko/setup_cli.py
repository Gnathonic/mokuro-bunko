"""Interactive setup wizard for mokuro-bunko."""

from __future__ import annotations

import sys
from pathlib import Path

import click
import yaml

from mokuro_bunko.config import (
    Config,
    CorsConfig,
    DynDNSConfig,
    RegistrationConfig,
    ServerConfig,
    SslConfig,
    StorageConfig,
    get_default_config_path,
    get_default_storage_path,
    save_config,
)


@click.command("setup")
@click.option(
    "--skip-if-exists",
    is_flag=True,
    help="Skip setup if config file already exists",
)
@click.pass_context
def setup_command(ctx: click.Context, skip_if_exists: bool) -> None:
    """Interactive first-time setup wizard."""
    config_path = ctx.obj.get("config_path") or get_default_config_path()
    path = Path(config_path)

    if path.exists() and skip_if_exists:
        click.echo(f"Config file already exists at {path}, skipping setup.")
        return

    if path.exists():
        if not click.confirm(f"Config file exists at {path}. Overwrite?"):
            return

    click.echo("=== mokuro-bunko setup ===\n")

    # 1. Storage path
    default_storage = str(get_default_storage_path())
    storage_path = click.prompt("Storage path", default=default_storage)

    # 2. Server port
    port = click.prompt("Server port", default=8080, type=int)

    # 3. SSL
    ssl_config = SslConfig()
    if click.confirm("Enable SSL?", default=False):
        if click.confirm("  Generate a self-signed certificate?", default=True):
            ssl_config = SslConfig(enabled=True, auto_cert=True)
        else:
            cert_file = click.prompt("  Path to certificate file")
            key_file = click.prompt("  Path to private key file")
            ssl_config = SslConfig(
                enabled=True,
                cert_file=cert_file,
                key_file=key_file,
            )

    # 4. Admin user
    create_admin = click.confirm("Create an admin user?", default=True)
    admin_username = ""
    admin_password = ""
    if create_admin:
        admin_username = click.prompt("  Admin username", default="admin")
        admin_password = click.prompt(
            "  Admin password",
            hide_input=True,
            confirmation_prompt=True,
        )

    # 5. Registration mode
    reg_mode = click.prompt(
        "Registration mode",
        type=click.Choice(["disabled", "self", "invite", "approval"]),
        default="self",
    )

    # 6. Connectivity
    dyndns_config = DynDNSConfig()
    click.echo("\nConnectivity options:")
    connectivity = click.prompt(
        "Access method",
        type=click.Choice(["lan", "cloudflare", "dyndns", "reverse-proxy"]),
        default="lan",
    )

    if connectivity == "dyndns":
        dyndns_provider = click.prompt(
            "  DynDNS provider",
            type=click.Choice(["duckdns", "generic"]),
            default="duckdns",
        )
        dyndns_domain = click.prompt("  Domain")
        dyndns_token = click.prompt("  API token", hide_input=True)
        dyndns_url = ""
        if dyndns_provider == "generic":
            dyndns_url = click.prompt("  Update URL")
        dyndns_config = DynDNSConfig(
            enabled=True,
            provider=dyndns_provider,  # type: ignore[arg-type]
            token=dyndns_token,
            domain=dyndns_domain,
            update_url=dyndns_url,
        )
    elif connectivity == "cloudflare":
        click.echo("  Cloudflare tunnel will be available via the admin panel or 'mokuro-bunko tunnel cloudflare'")
    elif connectivity == "reverse-proxy":
        click.echo("  Configure your reverse proxy to forward to the server port")

    # 7. CORS origins
    cors_origins = CorsConfig().allowed_origins.copy()
    if click.confirm("Add custom CORS origins?", default=False):
        while True:
            origin = click.prompt("  Origin (empty to finish)", default="", show_default=False)
            if not origin:
                break
            cors_origins.append(origin)

    # Build config
    config = Config(
        server=ServerConfig(host="0.0.0.0", port=port),
        storage=StorageConfig(base_path=Path(storage_path)),
        registration=RegistrationConfig(mode=reg_mode),  # type: ignore[arg-type]
        cors=CorsConfig(allowed_origins=cors_origins),
        ssl=ssl_config,
        dyndns=dyndns_config,
    )

    # 7. Summary
    click.echo("\n=== Configuration Summary ===")
    yaml.safe_dump(config.to_dict(), sys.stdout, default_flow_style=False)

    if not click.confirm("\nSave this configuration?", default=True):
        click.echo("Setup cancelled.")
        return

    save_config(config, path)
    click.echo(f"\nConfig saved to {path}")

    # Create admin user if requested
    if create_admin:
        try:
            from mokuro_bunko.database import Database

            config.storage.ensure_directories()
            db = Database(config.storage.base_path / "mokuro.db")
            db.create_user(admin_username, admin_password, "admin")
            click.echo(f"Admin user '{admin_username}' created")
        except Exception as e:
            click.echo(f"Warning: Could not create admin user: {e}", err=True)

    # Generate SSL cert if auto-cert
    if ssl_config.enabled and ssl_config.auto_cert:
        from mokuro_bunko.ssl import generate_self_signed_cert, get_default_cert_paths

        cert_path, key_path = get_default_cert_paths()
        if not cert_path.exists():
            generate_self_signed_cert(cert_path, key_path)
            click.echo(f"SSL certificate generated at {cert_path}")

    click.echo("\nSetup complete! Run 'mokuro-bunko serve' to start the server.")
