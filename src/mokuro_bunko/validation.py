"""Shared input validation helpers."""

from __future__ import annotations

import re

MIN_PASSWORD_LENGTH = 8
MAX_PASSWORD_LENGTH = 128
USERNAME_PATTERN = re.compile(r"^[a-zA-Z0-9_-]{3,32}$")


def validate_username(username: str) -> str | None:
    """Validate username format.

    Returns an error message when invalid, otherwise None.
    """
    if not username:
        return "Username is required"
    if not USERNAME_PATTERN.match(username):
        return (
            "Username must be 3-32 characters and contain only "
            "letters, numbers, underscores, and hyphens"
        )
    return None


def validate_password(password: str) -> str | None:
    """Validate password requirements.

    Returns an error message when invalid, otherwise None.
    """
    if not password:
        return "Password is required"
    if len(password) < MIN_PASSWORD_LENGTH:
        return f"Password must be at least {MIN_PASSWORD_LENGTH} characters"
    if len(password) > MAX_PASSWORD_LENGTH:
        return f"Password must be at most {MAX_PASSWORD_LENGTH} characters"
    return None

