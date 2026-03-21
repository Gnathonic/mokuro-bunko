"""SQLite database for user, invite, audit, and upload ownership management."""

from __future__ import annotations

import secrets
import sqlite3
import time
from collections.abc import Iterator
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Literal, TypedDict

import bcrypt

from mokuro_bunko.validation import validate_password, validate_username

UserStatus = Literal["active", "pending", "disabled", "deleted"]
UserRole = Literal["anonymous", "registered", "uploader", "inviter", "editor", "admin"]

LEGACY_ROLE_ALIASES: dict[str, str] = {
    "writer": "uploader",
}
VALID_ROLES = frozenset({"anonymous", "registered", "uploader", "inviter", "editor", "admin"})


class UserDict(TypedDict):
    """Type definition for user dictionary."""

    id: int
    username: str
    role: UserRole
    status: UserStatus
    notes: str
    created_at: str


class InviteDict(TypedDict):
    """Type definition for invite dictionary."""

    id: int
    code: str
    role: UserRole
    created_at: str
    expires_at: str
    used_by: str | None
    invited_by: str | None


class AuditEventDict(TypedDict):
    """Type definition for audit event dictionary."""

    id: int
    actor_username: str | None
    action: str
    target_type: str | None
    target_path: str | None
    target_username: str | None
    details: str | None
    created_at: str


def normalize_role(role: str) -> UserRole:
    """Normalize legacy role names and validate role values."""
    normalized = LEGACY_ROLE_ALIASES.get(role, role)
    if normalized not in VALID_ROLES:
        raise ValueError(
            f"Invalid role: {role}. Must be one of: {sorted(VALID_ROLES)}"
        )
    return normalized  # type: ignore[return-value]


def parse_duration(duration: str) -> timedelta:
    """Parse a duration string like '1h', '7d', '30d' into timedelta.

    Args:
        duration: Duration string with suffix (h=hours, d=days, w=weeks).

    Returns:
        Parsed timedelta.

    Raises:
        ValueError: If duration format is invalid.
    """
    if not duration:
        raise ValueError("Duration cannot be empty")

    unit = duration[-1].lower()
    try:
        value = int(duration[:-1])
    except ValueError as exc:
        raise ValueError(f"Invalid duration format: {duration}") from exc

    if value <= 0:
        raise ValueError(f"Duration must be positive: {duration}")

    match unit:
        case "h":
            return timedelta(hours=value)
        case "d":
            return timedelta(days=value)
        case "w":
            return timedelta(weeks=value)
        case _:
            raise ValueError(f"Unknown duration unit: {unit}")


def normalize_volume_key_from_library_relative(path: str) -> str | None:
    """Map a library-relative file path to its canonical volume key (*.cbz)."""
    cleaned = path.strip("/")
    if not cleaned:
        return None

    lower = cleaned.lower()
    if lower.endswith(".cbz"):
        return cleaned
    if lower.endswith(".mokuro.gz"):
        return cleaned[:-len(".mokuro.gz")] + ".cbz"
    if lower.endswith(".mokuro"):
        return cleaned[:-len(".mokuro")] + ".cbz"
    if lower.endswith(".webp"):
        return cleaned[:-len(".webp")] + ".cbz"
    if lower.endswith(".nocover"):
        return cleaned[:-len(".nocover")] + ".cbz"
    return None


class Database:
    """SQLite database for user and invite management."""

    SCHEMA_VERSION = 2
    AUDIT_PRUNE_INTERVAL_SECONDS = 3600

    def __init__(self, db_path: Path | str) -> None:
        """Initialize database connection.

        Args:
            db_path: Path to SQLite database file.
        """
        self.db_path = Path(db_path)
        self.connect_timeout_seconds = 30
        self.busy_timeout_ms = 5000
        self.connect_retries = 5
        self.retry_initial_delay_seconds = 0.05
        self._last_audit_prune_monotonic = 0.0
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._init_schema()

    def configure_connection(
        self,
        *,
        connect_timeout_seconds: int | None = None,
        busy_timeout_ms: int | None = None,
        connect_retries: int | None = None,
        retry_initial_delay_seconds: float | None = None,
    ) -> None:
        """Apply runtime DB connection settings."""
        if connect_timeout_seconds is not None:
            self.connect_timeout_seconds = max(1, int(connect_timeout_seconds))
        if busy_timeout_ms is not None:
            self.busy_timeout_ms = max(100, int(busy_timeout_ms))
        if connect_retries is not None:
            self.connect_retries = max(1, int(connect_retries))
        if retry_initial_delay_seconds is not None:
            self.retry_initial_delay_seconds = max(0.001, float(retry_initial_delay_seconds))

    @contextmanager
    def _connection(self) -> Iterator[sqlite3.Connection]:
        """Context manager for database connections.

        Retries on `sqlite3.OperationalError: database is locked` with
        exponential backoff.
        """
        retries = self.connect_retries
        delay = self.retry_initial_delay_seconds
        for attempt in range(retries):
            try:
                conn = sqlite3.connect(self.db_path, timeout=self.connect_timeout_seconds)
                conn.row_factory = sqlite3.Row
                conn.execute("PRAGMA journal_mode=WAL")
                conn.execute(f"PRAGMA busy_timeout={self.busy_timeout_ms}")
                try:
                    yield conn
                    conn.commit()
                except Exception:
                    conn.rollback()
                    raise
                finally:
                    conn.close()
                return
            except sqlite3.OperationalError as exc:
                if "database is locked" in str(exc).lower() and attempt < retries - 1:
                    time.sleep(delay)
                    delay *= 2
                    continue
                raise

    @staticmethod
    def _column_exists(conn: sqlite3.Connection, table: str, column: str) -> bool:
        cursor = conn.execute(f"PRAGMA table_info({table})")
        return any(row[1] == column for row in cursor.fetchall())

    def _init_schema(self) -> None:
        """Initialize database schema."""
        with self._connection() as conn:
            conn.execute("""
                CREATE TABLE IF NOT EXISTS schema_version (
                    version INTEGER PRIMARY KEY
                )
            """)

            conn.execute("""
                CREATE TABLE IF NOT EXISTS users (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    username TEXT UNIQUE NOT NULL,
                    password_hash TEXT NOT NULL,
                    role TEXT NOT NULL DEFAULT 'registered',
                    status TEXT NOT NULL DEFAULT 'active',
                    notes TEXT NOT NULL DEFAULT '',
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            """)

            conn.execute("""
                CREATE TABLE IF NOT EXISTS invites (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    code TEXT UNIQUE NOT NULL,
                    role TEXT NOT NULL DEFAULT 'registered',
                    invited_by TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    expires_at TEXT NOT NULL,
                    used_by TEXT,
                    used_at TEXT
                )
            """)

            conn.execute("""
                CREATE TABLE IF NOT EXISTS audit_logs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    actor_username TEXT,
                    action TEXT NOT NULL,
                    target_type TEXT,
                    target_path TEXT,
                    target_username TEXT,
                    details TEXT,
                    created_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            """)

            conn.execute("""
                CREATE TABLE IF NOT EXISTS volume_uploads (
                    volume_key TEXT PRIMARY KEY,
                    uploader_username TEXT NOT NULL,
                    uploaded_at TEXT NOT NULL DEFAULT (datetime('now')),
                    last_modified_by TEXT,
                    last_modified_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            """)

            if not self._column_exists(conn, "users", "notes"):
                conn.execute("ALTER TABLE users ADD COLUMN notes TEXT NOT NULL DEFAULT ''")

            if not self._column_exists(conn, "invites", "invited_by"):
                conn.execute("ALTER TABLE invites ADD COLUMN invited_by TEXT")

            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_users_username
                ON users(username)
            """)
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_invites_code
                ON invites(code)
            """)
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_audit_created_at
                ON audit_logs(created_at DESC)
            """)
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_audit_actor
                ON audit_logs(actor_username)
            """)
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_volume_uploads_uploader
                ON volume_uploads(uploader_username)
            """)

            # Role rename migration
            conn.execute("UPDATE users SET role = 'uploader' WHERE role = 'writer'")
            conn.execute("UPDATE invites SET role = 'uploader' WHERE role = 'writer'")

            cursor = conn.execute("SELECT version FROM schema_version")
            if cursor.fetchone() is None:
                conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?)",
                    (self.SCHEMA_VERSION,),
                )
            else:
                conn.execute("UPDATE schema_version SET version = ?", (self.SCHEMA_VERSION,))

    def _hash_password(self, password: str) -> str:
        """Hash a password using bcrypt."""
        return bcrypt.hashpw(password.encode(), bcrypt.gensalt()).decode()

    def _verify_password(self, password: str, password_hash: str) -> bool:
        """Verify a password against its hash."""
        return bcrypt.checkpw(password.encode(), password_hash.encode())

    # User CRUD operations

    def create_user(
        self,
        username: str,
        password: str,
        role: UserRole = "registered",
        status: UserStatus = "active",
        notes: str = "",
    ) -> int:
        """Create a new user.

        Args:
            username: Unique username.
            password: Plain text password (will be hashed).
            role: User role.
            status: User status.
            notes: Admin notes for this user.

        Returns:
            ID of created user.

        Raises:
            ValueError: If username already exists.
        """
        if not username or not username.strip():
            raise ValueError("Username is required")
        username_error = validate_username(username)
        if username_error:
            raise ValueError(username_error)

        password_error = validate_password(password)
        if password_error:
            raise ValueError(password_error)

        normalized_role = normalize_role(role)
        password_hash = self._hash_password(password)

        with self._connection() as conn:
            try:
                cursor = conn.execute(
                    """
                    INSERT INTO users (username, password_hash, role, status, notes)
                    VALUES (?, ?, ?, ?, ?)
                    """,
                    (username, password_hash, normalized_role, status, notes),
                )
                return cursor.lastrowid or 0
            except sqlite3.IntegrityError as exc:
                raise ValueError(f"Username '{username}' already exists") from exc

    def get_user(self, username: str) -> UserDict | None:
        """Get user by username.

        Args:
            username: Username to look up.

        Returns:
            User dictionary or None if not found.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                """
                SELECT id, username, role, status, notes, created_at
                FROM users WHERE username = ?
                """,
                (username,),
            )
            row = cursor.fetchone()
            if row:
                return UserDict(
                    id=row["id"],
                    username=row["username"],
                    role=normalize_role(row["role"]),
                    status=row["status"],
                    notes=row["notes"],
                    created_at=row["created_at"],
                )
            return None

    def authenticate_user(self, username: str, password: str) -> UserDict | None:
        """Authenticate user with username and password.

        Args:
            username: Username.
            password: Plain text password.

        Returns:
            User dictionary if authentication succeeds, None otherwise.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                """
                SELECT id, username, password_hash, role, status, notes, created_at
                FROM users WHERE username = ?
                """,
                (username,),
            )
            row = cursor.fetchone()

            if row and row["status"] == "active":
                if self._verify_password(password, row["password_hash"]):
                    return UserDict(
                        id=row["id"],
                        username=row["username"],
                        role=normalize_role(row["role"]),
                        status=row["status"],
                        notes=row["notes"],
                        created_at=row["created_at"],
                    )
            return None

    def list_users(self, status: UserStatus | None = None) -> list[UserDict]:
        """List all users.

        Args:
            status: Optional filter by status.

        Returns:
            List of user dictionaries.
        """
        with self._connection() as conn:
            if status:
                cursor = conn.execute(
                    """
                    SELECT id, username, role, status, notes, created_at
                    FROM users WHERE status = ?
                    ORDER BY created_at DESC
                    """,
                    (status,),
                )
            else:
                cursor = conn.execute(
                    """
                    SELECT id, username, role, status, notes, created_at
                    FROM users ORDER BY created_at DESC
                    """
                )
            return [
                UserDict(
                    id=row["id"],
                    username=row["username"],
                    role=normalize_role(row["role"]),
                    status=row["status"],
                    notes=row["notes"],
                    created_at=row["created_at"],
                )
                for row in cursor.fetchall()
            ]

    def update_user_role(self, username: str, role: UserRole) -> bool:
        """Update a user's role.

        Args:
            username: Username.
            role: New role.

        Returns:
            True if user was updated, False if not found.
        """
        normalized_role = normalize_role(role)
        with self._connection() as conn:
            cursor = conn.execute(
                """
                UPDATE users SET role = ?, updated_at = datetime('now')
                WHERE username = ?
                """,
                (normalized_role, username),
            )
            return cursor.rowcount > 0

    def update_user_notes(self, username: str, notes: str) -> bool:
        """Update a user's admin notes."""
        with self._connection() as conn:
            cursor = conn.execute(
                """
                UPDATE users SET notes = ?, updated_at = datetime('now')
                WHERE username = ?
                """,
                (notes, username),
            )
            return cursor.rowcount > 0

    def update_user_password(self, username: str, password: str) -> bool:
        """Update a user's password.

        Args:
            username: Username.
            password: New plain text password.

        Returns:
            True if user was updated, False if not found.
        """
        password_error = validate_password(password)
        if password_error:
            raise ValueError(password_error)

        password_hash = self._hash_password(password)
        with self._connection() as conn:
            cursor = conn.execute(
                """
                UPDATE users SET password_hash = ?, updated_at = datetime('now')
                WHERE username = ?
                """,
                (password_hash, username),
            )
            return cursor.rowcount > 0

    def approve_user(self, username: str) -> bool:
        """Approve a pending user.

        Args:
            username: Username.

        Returns:
            True if user was approved, False if not found or not pending.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                """
                UPDATE users SET status = 'active', updated_at = datetime('now')
                WHERE username = ? AND status = 'pending'
                """,
                (username,),
            )
            return cursor.rowcount > 0

    def disable_user(self, username: str) -> bool:
        """Disable a user.

        Args:
            username: Username.

        Returns:
            True if user was disabled, False if not found.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                """
                UPDATE users SET status = 'disabled', updated_at = datetime('now')
                WHERE username = ?
                """,
                (username,),
            )
            return cursor.rowcount > 0

    def delete_user(self, username: str) -> bool:
        """Soft-delete a user by setting status to 'deleted'.

        The user row and volume upload records are preserved for audit trail.

        Args:
            username: Username.

        Returns:
            True if user was deleted, False if not found.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                "UPDATE users SET status = 'deleted', updated_at = datetime('now') "
                "WHERE username = ? AND status != 'deleted'",
                (username,),
            )
            return cursor.rowcount > 0

    # Invite CRUD operations

    def create_invite(
        self,
        role: UserRole = "registered",
        expires: str = "7d",
        invited_by: str | None = None,
    ) -> str:
        """Create an invite code.

        Args:
            role: Role to assign to user who uses invite.
            expires: Expiration duration (e.g., '1h', '7d', '30d').
            invited_by: Username of inviter.

        Returns:
            Generated invite code.
        """
        normalized_role = normalize_role(role)
        code = secrets.token_urlsafe(16)
        duration = parse_duration(expires)
        expires_at = datetime.now() + duration

        with self._connection() as conn:
            conn.execute(
                """
                INSERT INTO invites (code, role, invited_by, expires_at)
                VALUES (?, ?, ?, ?)
                """,
                (code, normalized_role, invited_by, expires_at.strftime("%Y-%m-%d %H:%M:%S")),
            )

        self.log_audit_event(
            actor_username=invited_by,
            action="invite_created",
            target_type="invite",
            target_path=code,
            details={"role": normalized_role, "expires": expires},
        )
        return code

    def get_invite(self, code: str) -> InviteDict | None:
        """Get invite by code.

        Args:
            code: Invite code.

        Returns:
            Invite dictionary or None if not found.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                """
                SELECT id, code, role, created_at, expires_at, used_by, invited_by
                FROM invites WHERE code = ?
                """,
                (code,),
            )
            row = cursor.fetchone()
            if row:
                return InviteDict(
                    id=row["id"],
                    code=row["code"],
                    role=normalize_role(row["role"]),
                    created_at=row["created_at"],
                    expires_at=row["expires_at"],
                    used_by=row["used_by"],
                    invited_by=row["invited_by"],
                )
            return None

    def validate_invite(self, code: str) -> InviteDict | None:
        """Validate an invite code.

        Args:
            code: Invite code.

        Returns:
            Invite dictionary if valid, None if invalid/expired/used.
        """
        invite = self.get_invite(code)
        if not invite:
            return None

        if invite["used_by"]:
            return None

        expires_at = datetime.fromisoformat(invite["expires_at"])
        if datetime.now() > expires_at:
            return None

        return invite

    def use_invite(self, code: str, username: str) -> bool:
        """Mark an invite as used.

        Args:
            code: Invite code.
            username: Username who used the invite.

        Returns:
            True if invite was marked as used, False if invalid.
        """
        invite = self.validate_invite(code)
        if not invite:
            return False

        with self._connection() as conn:
            cursor = conn.execute(
                """
                UPDATE invites
                SET used_by = ?, used_at = datetime('now')
                WHERE code = ? AND used_by IS NULL
                """,
                (username, code),
            )
            changed = cursor.rowcount > 0

        if changed:
            self.log_audit_event(
                actor_username=username,
                action="invite_used",
                target_type="invite",
                target_path=code,
                target_username=username,
                details={"invited_by": invite.get("invited_by"), "role": invite["role"]},
            )
        return changed

    def list_invites(self, include_used: bool = False) -> list[InviteDict]:
        """List invite codes.

        Args:
            include_used: Include used invites.

        Returns:
            List of invite dictionaries.
        """
        with self._connection() as conn:
            if include_used:
                cursor = conn.execute(
                    """
                    SELECT id, code, role, created_at, expires_at, used_by, invited_by
                    FROM invites ORDER BY created_at DESC
                    """
                )
            else:
                cursor = conn.execute(
                    """
                    SELECT id, code, role, created_at, expires_at, used_by, invited_by
                    FROM invites
                    WHERE used_by IS NULL AND expires_at > datetime('now')
                    ORDER BY created_at DESC
                    """
                )
            return [
                InviteDict(
                    id=row["id"],
                    code=row["code"],
                    role=normalize_role(row["role"]),
                    created_at=row["created_at"],
                    expires_at=row["expires_at"],
                    used_by=row["used_by"],
                    invited_by=row["invited_by"],
                )
                for row in cursor.fetchall()
            ]

    def delete_invite(self, code: str) -> bool:
        """Delete an invite code.

        Args:
            code: Invite code.

        Returns:
            True if invite was deleted, False if not found.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                "DELETE FROM invites WHERE code = ?",
                (code,),
            )
            return cursor.rowcount > 0

    def cleanup_expired_invites(self) -> int:
        """Delete expired invites.

        Returns:
            Number of deleted invites.
        """
        # Use Python datetime comparison to avoid SQLite timezone/text format edge cases.
        expired_ids: list[int] = []
        now_utc = datetime.now(timezone.utc).replace(tzinfo=None)

        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT id, expires_at FROM invites WHERE used_by IS NULL"
            )
            for row in cursor.fetchall():
                try:
                    expires_at = datetime.fromisoformat(row["expires_at"])
                except (ValueError, TypeError):
                    continue
                if expires_at < now_utc:
                    expired_ids.append(row["id"])

            if not expired_ids:
                return 0

            cursor = conn.execute(
                f"DELETE FROM invites WHERE id IN ({','.join('?' for _ in expired_ids)})",
                tuple(expired_ids),
            )
            return cursor.rowcount

    # Audit operations

    AUDIT_RETENTION_DAYS = 30

    def log_audit_event(
        self,
        action: str,
        actor_username: str | None = None,
        target_type: str | None = None,
        target_path: str | None = None,
        target_username: str | None = None,
        details: dict[str, Any] | None = None,
    ) -> int:
        """Append an audit event and prune entries older than retention period."""
        details_text = None
        if details is not None:
            import json
            details_text = json.dumps(details, separators=(",", ":"), ensure_ascii=True)

        now = time.monotonic()
        should_prune = (now - self._last_audit_prune_monotonic) >= self.AUDIT_PRUNE_INTERVAL_SECONDS

        with self._connection() as conn:
            cursor = conn.execute(
                """
                INSERT INTO audit_logs (
                    actor_username, action, target_type, target_path,
                    target_username, details
                ) VALUES (?, ?, ?, ?, ?, ?)
                """,
                (actor_username, action, target_type, target_path, target_username, details_text),
            )
            if should_prune:
                conn.execute(
                    "DELETE FROM audit_logs WHERE created_at < datetime('now', ?)",
                    (f"-{self.AUDIT_RETENTION_DAYS} days",),
                )
                self._last_audit_prune_monotonic = now
            return cursor.lastrowid or 0

    def list_audit_events(self, limit: int = 200) -> list[AuditEventDict]:
        """Return newest audit events first."""
        safe_limit = max(1, min(int(limit), 1000))
        with self._connection() as conn:
            cursor = conn.execute(
                """
                SELECT id, actor_username, action, target_type, target_path,
                       target_username, details, created_at
                FROM audit_logs
                ORDER BY created_at DESC, id DESC
                LIMIT ?
                """,
                (safe_limit,),
            )
            return [
                AuditEventDict(
                    id=row["id"],
                    actor_username=row["actor_username"],
                    action=row["action"],
                    target_type=row["target_type"],
                    target_path=row["target_path"],
                    target_username=row["target_username"],
                    details=row["details"],
                    created_at=row["created_at"],
                )
                for row in cursor.fetchall()
            ]

    # Upload ownership operations

    def record_volume_upload(
        self,
        library_relative_path: str,
        uploader_username: str,
        existed_before: bool = False,
    ) -> None:
        """Record upload/edit for a volume and preserve original uploader."""
        volume_key = normalize_volume_key_from_library_relative(library_relative_path)
        if volume_key is None:
            return

        with self._connection() as conn:
            if not existed_before:
                conn.execute(
                    """
                    INSERT INTO volume_uploads (
                        volume_key, uploader_username, last_modified_by, last_modified_at
                    ) VALUES (?, ?, ?, datetime('now'))
                    ON CONFLICT(volume_key) DO UPDATE SET
                        last_modified_by = excluded.last_modified_by,
                        last_modified_at = datetime('now')
                    """,
                    (volume_key, uploader_username, uploader_username),
                )
            else:
                conn.execute(
                    """
                    UPDATE volume_uploads
                    SET last_modified_by = ?, last_modified_at = datetime('now')
                    WHERE volume_key = ?
                    """,
                    (uploader_username, volume_key),
                )

    def count_user_uploads_last_24h(self, username: str) -> int:
        """Return number of uploads a user created in the last 24 hours."""
        with self._connection() as conn:
            cursor = conn.execute(
                """
                SELECT COUNT(*) AS cnt
                FROM volume_uploads
                WHERE uploader_username = ?
                  AND uploaded_at >= datetime('now', '-1 day')
                """,
                (username,),
            )
            row = cursor.fetchone()
        return int(row["cnt"]) if row else 0

    def get_volume_owner(self, library_relative_path: str) -> str | None:
        """Get uploader username for a volume or sidecar path."""
        volume_key = normalize_volume_key_from_library_relative(library_relative_path)
        if volume_key is None:
            return None

        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT uploader_username FROM volume_uploads WHERE volume_key = ?",
                (volume_key,),
            )
            row = cursor.fetchone()
            if not row:
                return None
            return str(row["uploader_username"])

    def can_user_delete_library_path(self, username: str, virtual_path: str) -> bool:
        """Return True when user owns the library volume represented by virtual path."""
        prefix = "/mokuro-reader/"
        if not virtual_path.startswith(prefix):
            return False
        relative = virtual_path[len(prefix):].strip("/")
        if not relative or "/" not in relative and "." not in relative:
            # Disallow deleting /mokuro-reader root or top-level dirs via uploader ownership.
            return False

        owner = self.get_volume_owner(relative)
        return owner == username

    def forget_volume_upload(self, library_relative_path: str) -> None:
        """Delete ownership metadata for a volume key."""
        volume_key = normalize_volume_key_from_library_relative(library_relative_path)
        if volume_key is None:
            return
        with self._connection() as conn:
            conn.execute("DELETE FROM volume_uploads WHERE volume_key = ?", (volume_key,))

    def forget_volume_uploads_under_prefix(self, library_prefix: str) -> int:
        """Delete ownership metadata for all volume keys under a folder prefix."""
        prefix = library_prefix.strip("/")
        if not prefix:
            return 0
        with self._connection() as conn:
            cursor = conn.execute(
                "DELETE FROM volume_uploads WHERE volume_key LIKE ?",
                (f"{prefix}/%",),
            )
            return cursor.rowcount

    def rename_volume_upload(self, old_library_relative: str, new_library_relative: str) -> None:
        """Move ownership metadata when a volume path changes."""
        old_key = normalize_volume_key_from_library_relative(old_library_relative)
        new_key = normalize_volume_key_from_library_relative(new_library_relative)
        if old_key is None or new_key is None or old_key == new_key:
            return

        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT uploader_username, uploaded_at, last_modified_by FROM volume_uploads WHERE volume_key = ?",
                (old_key,),
            )
            row = cursor.fetchone()
            if not row:
                return

            conn.execute(
                """
                INSERT INTO volume_uploads (
                    volume_key, uploader_username, uploaded_at, last_modified_by, last_modified_at
                ) VALUES (?, ?, ?, ?, datetime('now'))
                ON CONFLICT(volume_key) DO UPDATE SET
                    uploader_username = excluded.uploader_username,
                    uploaded_at = excluded.uploaded_at,
                    last_modified_by = excluded.last_modified_by,
                    last_modified_at = datetime('now')
                """,
                (new_key, row["uploader_username"], row["uploaded_at"], row["last_modified_by"]),
            )
            conn.execute("DELETE FROM volume_uploads WHERE volume_key = ?", (old_key,))
