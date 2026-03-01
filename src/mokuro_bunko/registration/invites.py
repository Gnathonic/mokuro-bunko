"""Invite code management for mokuro-bunko."""

from __future__ import annotations

from datetime import datetime
from typing import TYPE_CHECKING, Literal, Optional, TypedDict

if TYPE_CHECKING:
    from mokuro_bunko.database import Database, InviteDict, UserRole


InviteStatus = Literal["valid", "expired", "used"]


class InviteInfo(TypedDict):
    """Extended invite information with status."""

    code: str
    role: str
    status: InviteStatus
    created_at: str
    expires_at: str
    used_by: Optional[str]
    invited_by: Optional[str]


class InviteManager:
    """Manager for invite code operations."""

    def __init__(self, database: "Database") -> None:
        """Initialize invite manager.

        Args:
            database: Database instance.
        """
        self.db = database

    def create_invite(
        self,
        role: "UserRole" = "registered",
        expires: str = "7d",
        invited_by: Optional[str] = None,
    ) -> str:
        """Create a new invite code.

        Args:
            role: Role to assign when invite is used.
            expires: Expiration duration (e.g., '1h', '7d', '30d').

        Returns:
            Generated invite code.
        """
        return self.db.create_invite(role=role, expires=expires, invited_by=invited_by)

    def validate(self, code: str) -> Optional["InviteDict"]:
        """Validate an invite code.

        Args:
            code: Invite code to validate.

        Returns:
            Invite info if valid, None if invalid/expired/used.
        """
        return self.db.validate_invite(code)

    def use(self, code: str, username: str) -> bool:
        """Mark an invite as used.

        Args:
            code: Invite code.
            username: Username who used the invite.

        Returns:
            True if successfully marked as used.
        """
        return self.db.use_invite(code, username)

    def get_status(self, invite: "InviteDict") -> InviteStatus:
        """Get the status of an invite.

        Args:
            invite: Invite dictionary.

        Returns:
            Status string: 'valid', 'expired', or 'used'.
        """
        if invite["used_by"]:
            return "used"

        expires_at = datetime.fromisoformat(invite["expires_at"])
        if datetime.now() > expires_at:
            return "expired"

        return "valid"

    def get_info(self, code: str) -> Optional[InviteInfo]:
        """Get extended invite information.

        Args:
            code: Invite code.

        Returns:
            Extended invite info or None if not found.
        """
        invite = self.db.get_invite(code)
        if not invite:
            return None

        return InviteInfo(
            code=invite["code"],
            role=invite["role"],
            status=self.get_status(invite),
            created_at=invite["created_at"],
            expires_at=invite["expires_at"],
            used_by=invite["used_by"],
            invited_by=invite.get("invited_by"),
        )

    def list_valid(self) -> list[InviteInfo]:
        """List all valid (unused, unexpired) invites.

        Returns:
            List of invite info dictionaries.
        """
        invites = self.db.list_invites(include_used=False)
        return [
            InviteInfo(
                code=inv["code"],
                role=inv["role"],
                status="valid",
                created_at=inv["created_at"],
                expires_at=inv["expires_at"],
                used_by=inv["used_by"],
                invited_by=inv.get("invited_by"),
            )
            for inv in invites
        ]

    def list_all(self) -> list[InviteInfo]:
        """List all invites with their status.

        Returns:
            List of invite info dictionaries.
        """
        invites = self.db.list_invites(include_used=True)
        return [
            InviteInfo(
                code=inv["code"],
                role=inv["role"],
                status=self.get_status(inv),
                created_at=inv["created_at"],
                expires_at=inv["expires_at"],
                used_by=inv["used_by"],
                invited_by=inv.get("invited_by"),
            )
            for inv in invites
        ]

    def delete(self, code: str) -> bool:
        """Delete an invite code.

        Args:
            code: Invite code.

        Returns:
            True if deleted, False if not found.
        """
        return self.db.delete_invite(code)

    def cleanup_expired(self) -> int:
        """Remove expired invites.

        Returns:
            Number of deleted invites.
        """
        return self.db.cleanup_expired_invites()
