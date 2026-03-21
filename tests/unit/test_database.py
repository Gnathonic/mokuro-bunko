"""Tests for database operations."""

from __future__ import annotations

import time
from datetime import timedelta
from pathlib import Path
from typing import TYPE_CHECKING

import pytest

from mokuro_bunko.database import Database, parse_duration

if TYPE_CHECKING:
    pass


class TestParseDuration:
    """Tests for parse_duration function."""

    def test_parse_hours(self) -> None:
        """Test parsing hour durations."""
        assert parse_duration("1h") == timedelta(hours=1)
        assert parse_duration("24h") == timedelta(hours=24)

    def test_parse_days(self) -> None:
        """Test parsing day durations."""
        assert parse_duration("1d") == timedelta(days=1)
        assert parse_duration("7d") == timedelta(days=7)
        assert parse_duration("30d") == timedelta(days=30)

    def test_parse_weeks(self) -> None:
        """Test parsing week durations."""
        assert parse_duration("1w") == timedelta(weeks=1)
        assert parse_duration("4w") == timedelta(weeks=4)

    def test_case_insensitive(self) -> None:
        """Test duration parsing is case insensitive."""
        assert parse_duration("1H") == timedelta(hours=1)
        assert parse_duration("7D") == timedelta(days=7)
        assert parse_duration("2W") == timedelta(weeks=2)

    def test_empty_duration(self) -> None:
        """Test empty duration raises error."""
        with pytest.raises(ValueError, match="cannot be empty"):
            parse_duration("")

    def test_invalid_format(self) -> None:
        """Test invalid format raises error."""
        with pytest.raises(ValueError, match="Invalid duration format"):
            parse_duration("abc")

    def test_invalid_unit(self) -> None:
        """Test invalid unit raises error."""
        with pytest.raises(ValueError, match="Unknown duration unit"):
            parse_duration("5x")

    def test_negative_value(self) -> None:
        """Test negative value raises error."""
        with pytest.raises(ValueError, match="must be positive"):
            parse_duration("-1d")

    def test_zero_value(self) -> None:
        """Test zero value raises error."""
        with pytest.raises(ValueError, match="must be positive"):
            parse_duration("0d")


class TestDatabaseInit:
    """Tests for Database initialization."""

    def test_creates_database_file(self, temp_dir: Path) -> None:
        """Test database file is created."""
        db_path = temp_dir / "test.db"
        Database(db_path)
        assert db_path.exists()

    def test_creates_parent_directories(self, temp_dir: Path) -> None:
        """Test parent directories are created."""
        db_path = temp_dir / "subdir" / "test.db"
        Database(db_path)
        assert db_path.exists()

    def test_accepts_string_path(self, temp_dir: Path) -> None:
        """Test accepts string path."""
        db_path = str(temp_dir / "test.db")
        db = Database(db_path)
        assert db.db_path.exists()


class TestUserCrud:
    """Tests for user CRUD operations."""

    def test_create_user(self, temp_db: Database) -> None:
        """Test creating a user."""
        user_id = temp_db.create_user("testuser", "password123", "registered")
        assert user_id > 0

        user = temp_db.get_user("testuser")
        assert user is not None
        assert user["username"] == "testuser"
        assert user["role"] == "registered"
        assert user["status"] == "active"
        assert user["notes"] == ""

    def test_update_user_notes(self, temp_db: Database) -> None:
        """Test updating user notes."""
        temp_db.create_user("notesuser", "password123")
        result = temp_db.update_user_notes("notesuser", "Trusted uploader")
        assert result is True
        user = temp_db.get_user("notesuser")
        assert user is not None
        assert user["notes"] == "Trusted uploader"

    def test_create_user_with_different_roles(self, temp_db: Database) -> None:
        """Test creating users with different roles."""
        for role in ("registered", "uploader", "editor", "admin"):
            username = f"user_{role}"
            temp_db.create_user(username, "password", role)  # type: ignore[arg-type]
            user = temp_db.get_user(username)
            assert user is not None
            assert user["role"] == role

    def test_create_user_with_pending_status(self, temp_db: Database) -> None:
        """Test creating a pending user."""
        temp_db.create_user("pending", "password", "registered", status="pending")
        user = temp_db.get_user("pending")
        assert user is not None
        assert user["status"] == "pending"

    def test_create_duplicate_user_raises(self, temp_db: Database) -> None:
        """Test creating duplicate user raises error."""
        temp_db.create_user("testuser", "password123")
        with pytest.raises(ValueError, match="already exists"):
            temp_db.create_user("testuser", "password456")

    def test_create_user_empty_username(self, temp_db: Database) -> None:
        """Test empty username raises error."""
        with pytest.raises(ValueError, match="Username is required"):
            temp_db.create_user("", "password123")

    def test_create_user_empty_password(self, temp_db: Database) -> None:
        """Test empty password raises error."""
        with pytest.raises(ValueError, match="Password is required"):
            temp_db.create_user("testuser", "")

    def test_create_user_short_password(self, temp_db: Database) -> None:
        """Test short password raises error."""
        with pytest.raises(ValueError, match="at least 8 characters"):
            temp_db.create_user("testuser", "abc")

    def test_create_user_invalid_username_format(self, temp_db: Database) -> None:
        """Reject path-like and invalid usernames."""
        with pytest.raises(ValueError, match="3-32 characters"):
            temp_db.create_user("../admin", "password123")

    def test_get_nonexistent_user(self, temp_db: Database) -> None:
        """Test getting nonexistent user returns None."""
        user = temp_db.get_user("nonexistent")
        assert user is None

    def test_authenticate_user_success(self, temp_db: Database) -> None:
        """Test successful authentication."""
        temp_db.create_user("testuser", "password123")
        user = temp_db.authenticate_user("testuser", "password123")
        assert user is not None
        assert user["username"] == "testuser"

    def test_authenticate_user_wrong_password(self, temp_db: Database) -> None:
        """Test authentication with wrong password."""
        temp_db.create_user("testuser", "password123")
        user = temp_db.authenticate_user("testuser", "wrongpassword")
        assert user is None

    def test_authenticate_nonexistent_user(self, temp_db: Database) -> None:
        """Test authentication with nonexistent user."""
        user = temp_db.authenticate_user("nonexistent", "password")
        assert user is None

    def test_authenticate_pending_user(self, temp_db: Database) -> None:
        """Test authentication fails for pending user."""
        temp_db.create_user("pending", "password123", status="pending")
        user = temp_db.authenticate_user("pending", "password123")
        assert user is None

    def test_authenticate_disabled_user(self, temp_db: Database) -> None:
        """Test authentication fails for disabled user."""
        temp_db.create_user("disabled", "password123", status="disabled")
        user = temp_db.authenticate_user("disabled", "password123")
        assert user is None

    def test_list_users(self, db_with_users: Database) -> None:
        """Test listing all users."""
        users = db_with_users.list_users()
        assert len(users) == 5
        usernames = [u["username"] for u in users]
        assert "alice" in usernames
        assert "bob" in usernames

    def test_list_users_by_status(self, db_with_users: Database) -> None:
        """Test listing users by status."""
        active_users = db_with_users.list_users(status="active")
        assert len(active_users) == 4

        pending_users = db_with_users.list_users(status="pending")
        assert len(pending_users) == 1
        assert pending_users[0]["username"] == "pending_user"

    def test_update_user_role(self, temp_db: Database) -> None:
        """Test updating user role."""
        temp_db.create_user("testuser", "password123", "registered")
        result = temp_db.update_user_role("testuser", "editor")
        assert result is True

        user = temp_db.get_user("testuser")
        assert user is not None
        assert user["role"] == "editor"

    def test_update_nonexistent_user_role(self, temp_db: Database) -> None:
        """Test updating nonexistent user role returns False."""
        result = temp_db.update_user_role("nonexistent", "admin")
        assert result is False

    def test_update_user_password(self, temp_db: Database) -> None:
        """Test updating user password."""
        temp_db.create_user("testuser", "oldpassword")
        result = temp_db.update_user_password("testuser", "newpassword")
        assert result is True

        # Old password should fail
        assert temp_db.authenticate_user("testuser", "oldpassword") is None
        # New password should work
        assert temp_db.authenticate_user("testuser", "newpassword") is not None

    def test_update_password_too_short(self, temp_db: Database) -> None:
        """Test updating with short password raises error."""
        temp_db.create_user("testuser", "password123")
        with pytest.raises(ValueError, match="at least 8 characters"):
            temp_db.update_user_password("testuser", "abc")

    def test_approve_user(self, temp_db: Database) -> None:
        """Test approving a pending user."""
        temp_db.create_user("pending", "password123", status="pending")
        result = temp_db.approve_user("pending")
        assert result is True

        user = temp_db.get_user("pending")
        assert user is not None
        assert user["status"] == "active"

    def test_approve_active_user(self, temp_db: Database) -> None:
        """Test approving already active user returns False."""
        temp_db.create_user("active", "password123", status="active")
        result = temp_db.approve_user("active")
        assert result is False

    def test_disable_user(self, temp_db: Database) -> None:
        """Test disabling a user."""
        temp_db.create_user("testuser", "password123")
        result = temp_db.disable_user("testuser")
        assert result is True


def test_auth_attempt_limiter_cleans_up_stale_entries() -> None:
    """Expired blocked entries should be cleared to prevent memory growth."""
    from mokuro_bunko.security import AuthAttemptLimiter

    limiter = AuthAttemptLimiter(max_failures=1, window_seconds=1, block_seconds=1)
    key = "ip:tester"

    allowed, retry_after = limiter.allow_attempt(key)
    assert allowed is True

    limiter.record_failure(key)
    allowed, retry_after = limiter.allow_attempt(key)
    assert allowed is False
    assert retry_after >= 1

    time.sleep(1.1)

    allowed, retry_after = limiter.allow_attempt(key)
    assert allowed is True
    assert retry_after == 0


def test_db_connection_retries_on_locked(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    """Fallback in _connection handles sqlite database is locked temporarily."""
    import sqlite3

    db_path = tmp_path / "test.db"
    db = Database(db_path)

    real_connect = sqlite3.connect
    call_count = {"count": 0}

    def fake_connect(path, timeout=30):
        call_count["count"] += 1
        if call_count["count"] == 1:
            raise sqlite3.OperationalError("database is locked")
        return real_connect(path, timeout=timeout)

    monkeypatch.setattr(sqlite3, "connect", fake_connect)

    user_id = db.create_user("retryuser", "password123")
    assert user_id > 0
    assert call_count["count"] == 2


def test_delete_user(temp_db: Database) -> None:
    """Test soft-deleting a user sets status to 'deleted'."""
    temp_db.create_user("testuser", "password123")
    result = temp_db.delete_user("testuser")
    assert result is True

    user = temp_db.get_user("testuser")
    assert user is not None
    assert user["status"] == "deleted"


def test_deleted_user_cannot_authenticate(temp_db: Database) -> None:
    """Test that soft-deleted users cannot log in."""
    temp_db.create_user("testuser", "password123")
    temp_db.delete_user("testuser")
    assert temp_db.authenticate_user("testuser", "password123") is None


def test_delete_already_deleted_user(temp_db: Database) -> None:
    """Test deleting an already-deleted user returns False."""
    temp_db.create_user("testuser", "password123")
    temp_db.delete_user("testuser")
    result = temp_db.delete_user("testuser")
    assert result is False


def test_delete_nonexistent_user(temp_db: Database) -> None:
    """Test deleting nonexistent user returns False."""
    result = temp_db.delete_user("nonexistent")
    assert result is False


class TestInviteCrud:
    """Tests for invite CRUD operations."""

    def test_create_invite(self, temp_db: Database) -> None:
        """Test creating an invite."""
        code = temp_db.create_invite("registered", "7d", invited_by="admin")
        assert code is not None
        assert len(code) > 0

        invite = temp_db.get_invite(code)
        assert invite is not None
        assert invite["code"] == code
        assert invite["role"] == "registered"
        assert invite["used_by"] is None
        assert invite["invited_by"] == "admin"

    def test_create_invite_different_roles(self, temp_db: Database) -> None:
        """Test creating invites with different roles."""
        for role in ("registered", "uploader", "editor"):
            code = temp_db.create_invite(role)  # type: ignore[arg-type]
            invite = temp_db.get_invite(code)
            assert invite is not None
            assert invite["role"] == role

    def test_create_invite_different_durations(self, temp_db: Database) -> None:
        """Test creating invites with different durations."""
        for duration in ("1h", "7d", "30d"):
            code = temp_db.create_invite("registered", duration)
            invite = temp_db.get_invite(code)
            assert invite is not None

    def test_get_nonexistent_invite(self, temp_db: Database) -> None:
        """Test getting nonexistent invite returns None."""
        invite = temp_db.get_invite("nonexistent")
        assert invite is None

    def test_validate_invite(self, temp_db: Database) -> None:
        """Test validating a valid invite."""
        code = temp_db.create_invite("registered", "7d")
        invite = temp_db.validate_invite(code)
        assert invite is not None
        assert invite["code"] == code

    def test_validate_invalid_invite(self, temp_db: Database) -> None:
        """Test validating invalid invite returns None."""
        invite = temp_db.validate_invite("invalid")
        assert invite is None

    def test_validate_used_invite(self, temp_db: Database) -> None:
        """Test validating used invite returns None."""
        code = temp_db.create_invite("registered", "7d")
        temp_db.use_invite(code, "testuser")
        invite = temp_db.validate_invite(code)
        assert invite is None

    def test_use_invite(self, temp_db: Database) -> None:
        """Test using an invite."""
        code = temp_db.create_invite("registered", "7d")
        result = temp_db.use_invite(code, "testuser")
        assert result is True

        invite = temp_db.get_invite(code)
        assert invite is not None
        assert invite["used_by"] == "testuser"

    def test_use_invalid_invite(self, temp_db: Database) -> None:
        """Test using invalid invite returns False."""
        result = temp_db.use_invite("invalid", "testuser")
        assert result is False

    def test_use_invite_twice(self, temp_db: Database) -> None:
        """Test using invite twice fails."""
        code = temp_db.create_invite("registered", "7d")
        temp_db.use_invite(code, "user1")
        result = temp_db.use_invite(code, "user2")
        assert result is False

    def test_list_invites(self, db_with_invites: Database) -> None:
        """Test listing active invites."""
        invites = db_with_invites.list_invites()
        assert len(invites) == 3

    def test_list_invites_excludes_used(self, db_with_invites: Database) -> None:
        """Test listing invites excludes used ones."""
        invites = db_with_invites.list_invites()
        code = invites[0]["code"]
        db_with_invites.use_invite(code, "testuser")

        invites = db_with_invites.list_invites()
        assert len(invites) == 2

    def test_list_invites_include_used(self, db_with_invites: Database) -> None:
        """Test listing invites with include_used."""
        invites = db_with_invites.list_invites()
        code = invites[0]["code"]
        db_with_invites.use_invite(code, "testuser")

        invites = db_with_invites.list_invites(include_used=True)
        assert len(invites) == 3

    def test_delete_invite(self, temp_db: Database) -> None:
        """Test deleting an invite."""
        code = temp_db.create_invite("registered", "7d")
        result = temp_db.delete_invite(code)
        assert result is True
        assert temp_db.get_invite(code) is None

    def test_delete_nonexistent_invite(self, temp_db: Database) -> None:
        """Test deleting nonexistent invite returns False."""
        result = temp_db.delete_invite("nonexistent")
        assert result is False


class TestAuditAndOwnership:
    """Tests for audit logging and upload ownership tracking."""

    def test_log_and_list_audit_events(self, temp_db: Database) -> None:
        """Audit events can be recorded and listed."""
        temp_db.log_audit_event(
            action="upload",
            actor_username="alice",
            target_type="library",
            target_path="/mokuro-reader/series/vol1.cbz",
        )
        events = temp_db.list_audit_events()
        assert len(events) >= 1
        assert events[0]["action"] == "upload"

    def test_volume_ownership_permissions(self, temp_db: Database) -> None:
        """Uploader ownership checks map sidecars back to volume keys."""
        temp_db.record_volume_upload("Series/Vol 01.cbz", "alice")
        assert temp_db.get_volume_owner("Series/Vol 01.cbz") == "alice"
        assert temp_db.get_volume_owner("Series/Vol 01.mokuro") == "alice"
        assert temp_db.get_volume_owner("Series/Vol 01.mokuro.gz") == "alice"
        assert temp_db.get_volume_owner("Series/Vol 01.webp") == "alice"
        assert temp_db.can_user_delete_library_path("alice", "/mokuro-reader/Series/Vol 01.cbz")
        assert not temp_db.can_user_delete_library_path("bob", "/mokuro-reader/Series/Vol 01.cbz")


class TestPasswordHashing:
    """Tests for password hashing."""

    def test_passwords_are_hashed(self, temp_db: Database) -> None:
        """Test that passwords are stored hashed."""
        temp_db.create_user("testuser", "password123")

        # Access raw database to check password is hashed
        import sqlite3
        conn = sqlite3.connect(temp_db.db_path)
        cursor = conn.execute(
            "SELECT password_hash FROM users WHERE username = ?",
            ("testuser",)
        )
        row = cursor.fetchone()
        conn.close()

        assert row is not None
        password_hash = row[0]
        assert password_hash != "password123"
        # bcrypt hashes start with $2b$
        assert password_hash.startswith("$2")

    def test_different_users_different_hashes(self, temp_db: Database) -> None:
        """Test that same password produces different hashes."""
        temp_db.create_user("user1", "samepassword")
        temp_db.create_user("user2", "samepassword")

        import sqlite3
        conn = sqlite3.connect(temp_db.db_path)
        cursor = conn.execute("SELECT password_hash FROM users")
        hashes = [row[0] for row in cursor.fetchall()]
        conn.close()

        # Bcrypt uses random salt, so hashes should be different
        assert hashes[0] != hashes[1]
