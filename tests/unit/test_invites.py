"""Tests for invite management."""

from __future__ import annotations

from datetime import datetime, timedelta
from pathlib import Path

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.registration.invites import InviteManager


@pytest.fixture
def db(temp_dir: Path) -> Database:
    """Create test database."""
    return Database(temp_dir / "test.db")


@pytest.fixture
def invites(db: Database) -> InviteManager:
    """Create invite manager."""
    return InviteManager(db)


class TestInviteCreation:
    """Tests for invite creation."""

    def test_create_invite_returns_code(self, invites: InviteManager) -> None:
        """Test creating invite returns a code."""
        code = invites.create_invite()
        assert code is not None
        assert len(code) > 0

    def test_create_invite_unique_codes(self, invites: InviteManager) -> None:
        """Test each invite has a unique code."""
        codes = [invites.create_invite() for _ in range(10)]
        assert len(set(codes)) == 10

    def test_create_invite_with_role(self, invites: InviteManager) -> None:
        """Test creating invite with specific role."""
        code = invites.create_invite(role="uploader", invited_by="admin")
        info = invites.get_info(code)
        assert info is not None
        assert info["role"] == "uploader"
        assert info["invited_by"] == "admin"

    def test_create_invite_with_expiry(self, invites: InviteManager) -> None:
        """Test creating invite with expiry duration."""
        code = invites.create_invite(expires="1h")
        info = invites.get_info(code)
        assert info is not None
        # Should expire within about an hour
        expires = datetime.fromisoformat(info["expires_at"])
        now = datetime.now()
        assert expires > now
        assert expires < now + timedelta(hours=2)


class TestInviteValidation:
    """Tests for invite validation."""

    def test_validate_valid_invite(self, invites: InviteManager) -> None:
        """Test validating a valid invite."""
        code = invites.create_invite()
        result = invites.validate(code)
        assert result is not None
        assert result["code"] == code

    def test_validate_nonexistent_invite(self, invites: InviteManager) -> None:
        """Test validating nonexistent invite returns None."""
        result = invites.validate("nonexistent-code")
        assert result is None

    def test_validate_used_invite(self, invites: InviteManager, db: Database) -> None:
        """Test validating used invite returns None."""
        code = invites.create_invite()
        # Create a user and use the invite
        db.create_user("testuser", "password")
        invites.use(code, "testuser")

        result = invites.validate(code)
        assert result is None

    def test_validate_expired_invite(self, db: Database) -> None:
        """Test validating expired invite returns None."""
        # Create invite that expired in the past
        invites = InviteManager(db)
        code = invites.create_invite(expires="1h")

        # Manually update expiry to past
        with db._connection() as conn:
            past = datetime.now() - timedelta(hours=1)
            conn.execute(
                "UPDATE invites SET expires_at = ? WHERE code = ?",
                (past.strftime("%Y-%m-%d %H:%M:%S"), code)
            )

        result = invites.validate(code)
        assert result is None


class TestInviteUsage:
    """Tests for using invites."""

    def test_use_valid_invite(self, invites: InviteManager, db: Database) -> None:
        """Test using a valid invite."""
        code = invites.create_invite()
        db.create_user("testuser", "password")

        result = invites.use(code, "testuser")
        assert result is True

    def test_use_invalid_invite(self, invites: InviteManager) -> None:
        """Test using invalid invite returns False."""
        result = invites.use("nonexistent", "testuser")
        assert result is False

    def test_use_invite_marks_as_used(
        self, invites: InviteManager, db: Database
    ) -> None:
        """Test using invite marks it as used."""
        code = invites.create_invite()
        db.create_user("testuser", "password")
        invites.use(code, "testuser")

        info = invites.get_info(code)
        assert info is not None
        assert info["used_by"] == "testuser"
        assert info["status"] == "used"

    def test_cannot_use_invite_twice(
        self, invites: InviteManager, db: Database
    ) -> None:
        """Test invite cannot be used twice."""
        code = invites.create_invite()
        db.create_user("user1", "password")
        db.create_user("user2", "password")

        result1 = invites.use(code, "user1")
        result2 = invites.use(code, "user2")

        assert result1 is True
        assert result2 is False


class TestInviteStatus:
    """Tests for invite status."""

    def test_status_valid(self, invites: InviteManager) -> None:
        """Test status of valid invite."""
        code = invites.create_invite()
        info = invites.get_info(code)
        assert info is not None
        assert info["status"] == "valid"

    def test_status_used(self, invites: InviteManager, db: Database) -> None:
        """Test status of used invite."""
        code = invites.create_invite()
        db.create_user("testuser", "password")
        invites.use(code, "testuser")

        info = invites.get_info(code)
        assert info is not None
        assert info["status"] == "used"

    def test_status_expired(self, invites: InviteManager, db: Database) -> None:
        """Test status of expired invite."""
        code = invites.create_invite()

        # Manually expire it
        with db._connection() as conn:
            past = datetime.now() - timedelta(hours=1)
            conn.execute(
                "UPDATE invites SET expires_at = ? WHERE code = ?",
                (past.strftime("%Y-%m-%d %H:%M:%S"), code)
            )

        info = invites.get_info(code)
        assert info is not None
        assert info["status"] == "expired"


class TestInviteListing:
    """Tests for listing invites."""

    def test_list_valid_empty(self, invites: InviteManager) -> None:
        """Test listing with no invites."""
        result = invites.list_valid()
        assert result == []

    def test_list_valid_returns_valid_only(
        self, invites: InviteManager, db: Database
    ) -> None:
        """Test list_valid only returns valid invites."""
        # Create valid invite
        valid_code = invites.create_invite()

        # Create and use an invite
        used_code = invites.create_invite()
        db.create_user("testuser", "password")
        invites.use(used_code, "testuser")

        result = invites.list_valid()
        assert len(result) == 1
        assert result[0]["code"] == valid_code

    def test_list_all_includes_all(
        self, invites: InviteManager, db: Database
    ) -> None:
        """Test list_all includes all invites."""
        # Create valid invite
        invites.create_invite()

        # Create and use an invite
        used_code = invites.create_invite()
        db.create_user("testuser", "password")
        invites.use(used_code, "testuser")

        result = invites.list_all()
        assert len(result) == 2

    def test_list_returns_invite_info(self, invites: InviteManager) -> None:
        """Test listed invites have correct fields."""
        invites.create_invite(role="uploader")

        result = invites.list_valid()
        assert len(result) == 1

        invite = result[0]
        assert "code" in invite
        assert "role" in invite
        assert invite["role"] == "uploader"
        assert "status" in invite
        assert "created_at" in invite
        assert "expires_at" in invite


class TestInviteDeletion:
    """Tests for deleting invites."""

    def test_delete_existing(self, invites: InviteManager) -> None:
        """Test deleting existing invite."""
        code = invites.create_invite()
        result = invites.delete(code)
        assert result is True

        info = invites.get_info(code)
        assert info is None

    def test_delete_nonexistent(self, invites: InviteManager) -> None:
        """Test deleting nonexistent invite."""
        result = invites.delete("nonexistent")
        assert result is False


class TestInviteCleanup:
    """Tests for cleaning up expired invites."""

    def test_cleanup_removes_expired(
        self, invites: InviteManager, db: Database
    ) -> None:
        """Test cleanup removes expired invites."""
        code = invites.create_invite()

        # Manually expire it (use SQLite datetime format: YYYY-MM-DD HH:MM:SS)
        with db._connection() as conn:
            past = datetime.now() - timedelta(hours=1)
            conn.execute(
                "UPDATE invites SET expires_at = ? WHERE code = ?",
                (past.strftime("%Y-%m-%d %H:%M:%S"), code)
            )

        count = invites.cleanup_expired()
        assert count == 1

        info = invites.get_info(code)
        assert info is None

    def test_cleanup_keeps_valid(self, invites: InviteManager) -> None:
        """Test cleanup keeps valid invites."""
        code = invites.create_invite()

        count = invites.cleanup_expired()
        assert count == 0

        info = invites.get_info(code)
        assert info is not None

    def test_cleanup_keeps_used(
        self, invites: InviteManager, db: Database
    ) -> None:
        """Test cleanup keeps used invites even if expired."""
        code = invites.create_invite()
        db.create_user("testuser", "password")
        invites.use(code, "testuser")

        # Manually expire it
        with db._connection() as conn:
            past = datetime.now() - timedelta(hours=1)
            conn.execute(
                "UPDATE invites SET expires_at = ? WHERE code = ?",
                (past.strftime("%Y-%m-%d %H:%M:%S"), code)
            )

        count = invites.cleanup_expired()
        # Used invites aren't cleaned up (they're historical records)
        assert count == 0


class TestInviteInfo:
    """Tests for getting invite info."""

    def test_get_info_existing(self, invites: InviteManager) -> None:
        """Test getting info for existing invite."""
        code = invites.create_invite(role="editor")
        info = invites.get_info(code)

        assert info is not None
        assert info["code"] == code
        assert info["role"] == "editor"
        assert info["status"] == "valid"
        assert info["used_by"] is None

    def test_get_info_nonexistent(self, invites: InviteManager) -> None:
        """Test getting info for nonexistent invite."""
        info = invites.get_info("nonexistent")
        assert info is None
