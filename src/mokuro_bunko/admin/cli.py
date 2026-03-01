"""Admin CLI commands for mokuro-bunko."""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Optional

import click

from mokuro_bunko.config import load_config
from mokuro_bunko.database import Database


def get_database(config_path: Optional[Path]) -> Database:
    """Get database instance from config.

    Args:
        config_path: Optional path to config file.

    Returns:
        Database instance.
    """
    config = load_config(config_path)
    return Database(config.storage.base_path / "mokuro.db")


@click.group(name="admin")
def admin_group() -> None:
    """Admin commands for user management."""
    pass


@admin_group.command("add-user")
@click.argument("username")
@click.option(
    "--role",
    type=click.Choice(["registered", "uploader", "inviter", "editor", "admin"]),
    default="registered",
    help="User role",
    show_default=True,
)
@click.option(
    "--password",
    prompt=True,
    hide_input=True,
    confirmation_prompt=True,
    help="User password",
)
@click.pass_context
def add_user(ctx: click.Context, username: str, role: str, password: str) -> None:
    """Add a new user."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    try:
        db.create_user(username, password, role)
        click.echo(f"User '{username}' created with role '{role}'")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


@admin_group.command("delete-user")
@click.argument("username")
@click.option("--yes", "-y", is_flag=True, help="Skip confirmation")
@click.pass_context
def delete_user(ctx: click.Context, username: str, yes: bool) -> None:
    """Delete a user."""
    if not yes:
        click.confirm(f"Delete user '{username}'?", abort=True)

    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    if db.delete_user(username):
        click.echo(f"User '{username}' deleted")
    else:
        click.echo(f"User '{username}' not found", err=True)
        sys.exit(1)


@admin_group.command("list-users")
@click.option(
    "--status",
    type=click.Choice(["active", "pending", "disabled"]),
    help="Filter by status",
)
@click.pass_context
def list_users(ctx: click.Context, status: Optional[str]) -> None:
    """List all users."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    users = db.list_users(status=status)

    if not users:
        if status:
            click.echo(f"No {status} users found")
        else:
            click.echo("No users found")
        return

    click.echo(f"{'Username':<20} {'Role':<12} {'Status':<10} {'Created':<20}")
    click.echo("-" * 64)
    for user in users:
        created = user["created_at"][:19]  # Truncate microseconds
        click.echo(
            f"{user['username']:<20} {user['role']:<12} "
            f"{user['status']:<10} {created:<20}"
        )


@admin_group.command("change-role")
@click.argument("username")
@click.argument(
    "role",
    type=click.Choice(["registered", "uploader", "inviter", "editor", "admin"]),
)
@click.pass_context
def change_role(ctx: click.Context, username: str, role: str) -> None:
    """Change a user's role."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    if db.update_user_role(username, role):
        click.echo(f"User '{username}' role changed to '{role}'")
    else:
        click.echo(f"User '{username}' not found", err=True)
        sys.exit(1)


@admin_group.command("generate-invite")
@click.option(
    "--role",
    type=click.Choice(["registered", "uploader", "inviter", "editor"]),
    default="registered",
    help="Role for invited user",
    show_default=True,
)
@click.option(
    "--expires",
    default="7d",
    help="Expiration time (e.g., 1h, 7d, 30d)",
    show_default=True,
)
@click.pass_context
def generate_invite(ctx: click.Context, role: str, expires: str) -> None:
    """Generate an invite code."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    try:
        code = db.create_invite(role, expires)
        click.echo(f"Invite code: {code}")
        click.echo(f"Role: {role}")
        click.echo(f"Expires in: {expires}")
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)


@admin_group.command("list-invites")
@click.option("--all", "include_all", is_flag=True, help="Include used/expired invites")
@click.pass_context
def list_invites(ctx: click.Context, include_all: bool) -> None:
    """List invite codes."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    invites = db.list_invites(include_used=include_all)

    if not invites:
        click.echo("No invites found")
        return

    if include_all:
        click.echo(f"{'Code':<24} {'Role':<12} {'Expires':<20} {'Used By':<15}")
        click.echo("-" * 73)
        for invite in invites:
            expires = invite["expires_at"][:19]
            used_by = invite["used_by"] or "-"
            click.echo(
                f"{invite['code']:<24} {invite['role']:<12} "
                f"{expires:<20} {used_by:<15}"
            )
    else:
        click.echo(f"{'Code':<24} {'Role':<12} {'Expires':<20}")
        click.echo("-" * 58)
        for invite in invites:
            expires = invite["expires_at"][:19]
            click.echo(f"{invite['code']:<24} {invite['role']:<12} {expires:<20}")


@admin_group.command("delete-invite")
@click.argument("code")
@click.pass_context
def delete_invite(ctx: click.Context, code: str) -> None:
    """Delete an invite code."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    if db.delete_invite(code):
        click.echo(f"Invite '{code}' deleted")
    else:
        click.echo(f"Invite '{code}' not found", err=True)
        sys.exit(1)


@admin_group.command("approve-user")
@click.argument("username")
@click.pass_context
def approve_user(ctx: click.Context, username: str) -> None:
    """Approve a pending user."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    if db.approve_user(username):
        click.echo(f"User '{username}' approved")
    else:
        click.echo(f"User '{username}' not found or not pending", err=True)
        sys.exit(1)


@admin_group.command("disable-user")
@click.argument("username")
@click.pass_context
def disable_user(ctx: click.Context, username: str) -> None:
    """Disable a user account."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    if db.disable_user(username):
        click.echo(f"User '{username}' disabled")
    else:
        click.echo(f"User '{username}' not found", err=True)
        sys.exit(1)


@admin_group.command("set-password")
@click.argument("username")
@click.option(
    "--password",
    prompt=True,
    hide_input=True,
    confirmation_prompt=True,
    help="New password",
)
@click.pass_context
def set_password(ctx: click.Context, username: str, password: str) -> None:
    """Set a user's password."""
    config_path = ctx.obj.get("config_path")
    db = get_database(config_path)

    try:
        if db.update_user_password(username, password):
            click.echo(f"Password updated for '{username}'")
        else:
            click.echo(f"User '{username}' not found", err=True)
            sys.exit(1)
    except ValueError as e:
        click.echo(f"Error: {e}", err=True)
        sys.exit(1)
