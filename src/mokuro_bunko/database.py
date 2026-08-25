"""SQLite database for user, invite, audit, upload ownership and series metadata."""

from __future__ import annotations

import json
import math
import re
import secrets
import sqlite3
import threading
import time
import unicodedata
from collections.abc import Iterable, Iterator
from contextlib import contextmanager
from datetime import datetime, timedelta
from pathlib import Path
from typing import Any, Literal, TypedDict, cast

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


class SeriesFactsRow(TypedDict):
    """One series' shareable facts plus its shelf alignment (index data).

    `series_key` is the reader's series-identity fold —
    `metadata.reader_compat.normalize_volume_title_key`: NFC-normalize,
    THEN trim, collapse whitespace, lowercase (`normalize_series_key`'s
    three steps applied to the NFC-normalized title, not to the raw one).
    Task 11 review round 3 corrected this paragraph, which used to say "no
    NFC pass" — true only before that round's fix; `metadata/service.py`,
    the sole reader and writer of these rows, keys every `series_facts` row
    with `normalize_volume_title_key` today, precisely so an NFD-spelled
    and an NFC-spelled folder name (the common case for a name that
    round-tripped through a filesystem) collapse onto ONE row instead of
    silently diverging. It is NOT the case-sensitive matching this module
    uses for volume keys (`normalize_volume_key_from_library_relative`), so
    two folders differing only in case (or Unicode composition) share one
    facts row by design. `database.py` stays free of metadata imports (as
    it does of API imports), so callers fold the title before calling in;
    nothing here re-folds what it is handed.

    `facts_updated_at` is the FACTS clock — the value that decides merges. It
    is not `updated_at`, which is this row's own bookkeeping stamp and moves
    whenever anything (including an offset) is written. Both are stored as
    given: the validator has already clamped the facts stamp, and the factless
    epoch literal must survive unchanged.
    """

    series_key: str
    series_title: str
    external_ids: dict[str, int]
    titles: dict[str, str]
    synonyms: list[str]
    tag: str | None
    unit: str | None
    facts_updated_at: str
    spine_offset: float | None
    volume_offsets: dict[str, float]
    updated_by: str | None
    updated_at: str


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
    except ValueError as e:
        raise ValueError(f"Invalid duration format: {duration}") from e

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


_WHITESPACE_RUN_RE = re.compile(r"\s+")


def _fold_series_title_key(title: str) -> str:
    """Fold a series folder name for ownership comparison.

    NFC-normalize, then collapse internal whitespace runs and lowercase —
    exactly the two operations `metadata.reader_compat.normalize_volume_title_key`
    performs (`normalize_series_key(NFC(title))`, itself mirroring the
    reader's `normalizeVolumeTitleKey`). `database.py` deliberately stays
    free of `metadata` package imports (see the `series_facts` layering
    note elsewhere in this module), so this is an independent
    reimplementation of the same two operations, not a shared import — if
    either fold ever changes, the other needs a matching update.

    NFC-first matters on its own: a folder name that round-tripped through a
    filesystem can come back NFD-decomposed while a PUT's `series.json`
    path segment stays composed — two byte-different, semantically
    identical strings that must compare equal here, exactly as the reader
    already treats them (Task 11 review round 1, F2/F5/F6).

    Review round 2 (N2): an earlier version used `casefold()` here, which
    is STRICTER than both `normalize_series_key`'s plain `lower()` and the
    reader's JS `toLowerCase()` (`casefold()` folds `ß`→`ss`; neither of
    the other two does). That let two folders the catalog compiler treated
    as different series (`'Straße'` vs `'STRASSE'`) share one ownership
    fold. Plain `.lower()` fixed the case half.

    Review round 3 (F9 / the N2 residual): round 2's fix here still left a
    FALSE invariant in this docstring — "must never be coarser than
    `normalize_series_key`" — while this function's very first line NFC-
    normalizes and bare `normalize_series_key` does not, so this fold was
    unconditionally coarser than that one by construction (an NFD/NFC pair
    folds equal here, unequal there) and the claim was self-contradicting.
    The actual defect that exposed: `metadata/service.py` used to key
    `series_facts` rows with bare `normalize_series_key` too, so an
    NFD-spelled `series.json` PUT for an NFC-spelled real folder folded
    ownership-equal here while the service treated it as a DIFFERENT,
    unmatched series — an authorized write that landed on an identity no
    real folder shared. The fix was on the SERVICE side, not here:
    `metadata/service.py` now keys `series_facts` rows with
    `normalize_volume_title_key` (NFC + `normalize_series_key`) everywhere,
    making THAT the actual system-wide series-identity fold, not bare
    `normalize_series_key`. Against that corrected baseline, this
    function's true invariant holds and is stronger than "no coarser than":
    it is IDENTICAL, step for step, to `normalize_volume_title_key` — this
    docstring's opening paragraph states the actual operations directly
    rather than relying on an invariant claim like the one that was wrong
    here before.
    """
    normalized = unicodedata.normalize("NFC", title)
    collapsed = _WHITESPACE_RUN_RE.sub(" ", normalized.strip())
    return collapsed.lower()


class _RetryingConnection:
    """Connection proxy that retries execute() while the DB is locked.

    In WAL mode an external writer's lock surfaces at the first write
    statement, so per-statement retry (not just commit retry) is needed.
    Retrying a failed execute is safe: a statement that raised 'database is
    locked' acquired no lock and had no effect.
    """

    def __init__(
        self, conn: sqlite3.Connection, retries: int, initial_delay: float
    ) -> None:
        self._conn = conn
        self._retries = retries
        self._initial_delay = initial_delay

    def execute(self, *args: Any, **kwargs: Any) -> sqlite3.Cursor:
        delay = self._initial_delay
        for attempt in range(self._retries):
            try:
                return self._conn.execute(*args, **kwargs)
            except sqlite3.OperationalError as exc:
                if (
                    "database is locked" not in str(exc).lower()
                    or attempt >= self._retries - 1
                ):
                    raise
                time.sleep(delay)
                delay *= 2
        raise AssertionError("unreachable")

    def __getattr__(self, name: str) -> Any:
        return getattr(self._conn, name)


class Database:
    """SQLite database for user and invite management."""

    SCHEMA_VERSION = 3
    AUDIT_PRUNE_INTERVAL_SECONDS = 3600

    def __init__(self, db_path: Path | str) -> None:
        """Initialize database connection.

        Args:
            db_path: Path to SQLite database file.
        """
        self.db_path = Path(db_path)
        self.busy_timeout_ms = 5000
        self.lock_retries = 5
        self.retry_initial_delay_seconds = 0.05
        self._last_audit_prune_monotonic = 0.0
        self.db_path.parent.mkdir(parents=True, exist_ok=True)
        self._lock = threading.Lock()
        self._conn = sqlite3.connect(
            self.db_path, timeout=30, check_same_thread=False
        )
        self._conn.row_factory = sqlite3.Row
        self._conn.execute("PRAGMA journal_mode=WAL")
        self._conn.execute(f"PRAGMA busy_timeout={self.busy_timeout_ms}")
        self._init_schema()

    def configure_connection(
        self,
        *,
        busy_timeout_ms: int | None = None,
        lock_retries: int | None = None,
        retry_initial_delay_seconds: float | None = None,
    ) -> None:
        """Apply runtime DB tuning settings (values clamped to sane minimums)."""
        if busy_timeout_ms is not None:
            self.busy_timeout_ms = max(100, int(busy_timeout_ms))
            with self._lock:
                self._conn.execute(f"PRAGMA busy_timeout={self.busy_timeout_ms}")
        if lock_retries is not None:
            self.lock_retries = max(1, int(lock_retries))
        if retry_initial_delay_seconds is not None:
            self.retry_initial_delay_seconds = max(0.001, float(retry_initial_delay_seconds))

    @contextmanager
    def _connection(self) -> Iterator[sqlite3.Connection]:
        """Context manager providing serialised access to the persistent connection."""
        with self._lock:
            proxy = _RetryingConnection(
                self._conn, self.lock_retries, self.retry_initial_delay_seconds
            )
            try:
                yield cast("sqlite3.Connection", proxy)
                self._commit_with_retry()
            except Exception:
                self._conn.rollback()
                raise

    def _commit_with_retry(self) -> None:
        """Commit, retrying with exponential backoff while an external process
        (e.g. the admin CLI run against a live server's DB) holds the write lock.

        SQLite's busy_timeout already blocks each attempt; the Python-level
        retries extend resilience across several such windows.
        """
        delay = self.retry_initial_delay_seconds
        for attempt in range(self.lock_retries):
            try:
                self._conn.commit()
                return
            except sqlite3.OperationalError as exc:
                if (
                    "database is locked" not in str(exc).lower()
                    or attempt >= self.lock_retries - 1
                ):
                    raise
                time.sleep(delay)
                delay *= 2

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

            # Schema v3: series facts + the compiled-entry cache. Both are
            # CREATE TABLE IF NOT EXISTS like everything above, so a v2
            # database gains them on the next open and a v3 one is untouched.
            #
            # `spine_offset` is NUMERIC, not REAL, deliberately: alignment
            # numbers are preserved verbatim from a client PUT, and REAL
            # affinity would widen the integer nudge `-40` to `-40.0`, so the
            # republished bytes would stop matching what the client wrote.
            # NUMERIC keeps an integer an integer (and folds `8.0` back to `8`,
            # which is what JSON.stringify writes for that value anyway).
            conn.execute("""
                CREATE TABLE IF NOT EXISTS series_facts (
                    series_key TEXT PRIMARY KEY,
                    series_title TEXT NOT NULL,
                    external_ids TEXT NOT NULL DEFAULT '{}',
                    titles TEXT NOT NULL DEFAULT '{}',
                    synonyms TEXT NOT NULL DEFAULT '[]',
                    tag TEXT,
                    unit TEXT,
                    facts_updated_at TEXT NOT NULL,
                    spine_offset NUMERIC,
                    volume_offsets TEXT NOT NULL DEFAULT '{}',
                    updated_by TEXT,
                    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            """)

            conn.execute("""
                CREATE TABLE IF NOT EXISTS series_entry_cache (
                    volume_key TEXT PRIMARY KEY,
                    series_key TEXT NOT NULL,
                    entry_json TEXT NOT NULL,
                    cbz_size INTEGER NOT NULL,
                    cbz_mtime REAL NOT NULL,
                    sidecar_key TEXT NOT NULL DEFAULT '',
                    computed_at TEXT NOT NULL DEFAULT (datetime('now'))
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
            conn.execute("""
                CREATE INDEX IF NOT EXISTS idx_series_entry_cache_series
                ON series_entry_cache(series_key)
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
            except sqlite3.IntegrityError as e:
                raise ValueError(f"Username '{username}' already exists") from e

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
        # token_urlsafe's alphabet includes '-' and '_'; a leading '-' is
        # misread as an option by positional CLI argument parsing (Click),
        # so regenerate rather than ship a code that breaks admin tooling.
        while code[0] in ("-", "_"):
            code = secrets.token_urlsafe(16)
        duration = parse_duration(expires)
        expires_at = datetime.now() + duration

        with self._connection() as conn:
            conn.execute(
                """
                INSERT INTO invites (code, role, invited_by, expires_at)
                VALUES (?, ?, ?, ?)
                """,
                (code, normalized_role, invited_by, expires_at.isoformat()),
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

        Compares expiry in Python: invites store datetime.now().isoformat()
        (local-naive, 'T' separator), which a TEXT comparison against SQLite's
        UTC datetime('now') gets wrong both by timezone and lexicographically.
        Uses the same local-naive clock as create_invite/validate_invite.

        Returns:
            Number of deleted invites.
        """
        now = datetime.now()
        expired_ids: list[int] = []
        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT id, expires_at FROM invites WHERE used_by IS NULL"
            )
            for row in cursor.fetchall():
                try:
                    expires_at = datetime.fromisoformat(row["expires_at"])
                except (ValueError, TypeError):
                    # Unparseable expiry: leave the row for manual inspection.
                    continue
                if expires_at < now:
                    expired_ids.append(row["id"])

            if not expired_ids:
                return 0

            placeholders = ",".join("?" for _ in expired_ids)
            cursor = conn.execute(
                f"DELETE FROM invites WHERE id IN ({placeholders})",  # noqa: S608
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

        # Prune at most once per interval instead of on every audit write.
        now = time.monotonic()
        should_prune = (
            now - self._last_audit_prune_monotonic
        ) >= self.AUDIT_PRUNE_INTERVAL_SECONDS or self._last_audit_prune_monotonic == 0.0

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
                    INSERT INTO volume_uploads (
                        volume_key, uploader_username, last_modified_by, last_modified_at
                    ) VALUES (?, ?, ?, datetime('now'))
                    ON CONFLICT(volume_key) DO UPDATE SET
                        last_modified_by = excluded.last_modified_by,
                        last_modified_at = datetime('now')
                    """,
                    (volume_key, uploader_username, uploader_username),
                )

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
        """Return True when user owns the library volume represented by virtual path.

        Deliberately BYTE-EXACT (via `get_volume_owner` ->
        `normalize_volume_key_from_library_relative`), unlike the
        Unicode-folded series-edit ownership below (`can_user_edit_series`).
        That is the safe direction, not an oversight (Task 11 review round
        2, N5): a volume-level DELETE never gets MORE permissive by
        loosening how its path is matched, so `'dr stone/Volume 01.cbz'` or
        `'Dr  Stone/Volume 01.cbz'` do NOT resolve to the same row as
        `'Dr Stone/Volume 01.cbz'` here even though they WOULD fold equal
        for `can_user_edit_series`. The two are intentionally different
        identities for two different rights.
        """
        prefix = "/mokuro-reader/"
        if not virtual_path.startswith(prefix):
            return False
        relative = virtual_path[len(prefix):].strip("/")
        if not relative or "/" not in relative and "." not in relative:
            # Disallow deleting /mokuro-reader root or top-level dirs via uploader ownership.
            return False

        owner = self.get_volume_owner(relative)
        return owner == username

    def _volume_upload_folder_owners(self) -> list[tuple[str, str]]:
        """`(folder, uploader_username)` for every tracked `volume_uploads` row.

        One query, no folding — the single scan `series_owners` and
        `list_series_owned_by` both build on, so an ownership-matching fix
        only ever needs to change in one place (Task 11 review round 2,
        N3: `list_series_owned_by` used to call `can_user_edit_series`,
        which itself re-queried and re-folded the WHOLE table, once per
        CANDIDATE folder — O(series_count * rows), measured at 4.6s for
        500 series / 10k rows on the identity endpoint every reader
        connect hits). Rows whose `volume_key` has no `/` (a top-level
        loose file, not a series folder) are dropped here so neither
        caller has to re-check it.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT DISTINCT uploader_username, volume_key FROM volume_uploads"
            )
            pairs: list[tuple[str, str]] = []
            for row in cursor.fetchall():
                folder, separator, _rest = str(row["volume_key"]).partition("/")
                if separator:
                    pairs.append((folder, str(row["uploader_username"])))
            return pairs

    def series_owners(self, series_title: str) -> set[str]:
        """Distinct uploader usernames among a series folder's tracked volumes.

        `series_title` is the literal top-level library folder name — the
        same string `metadata.paths.series_title_from_series_file_path`
        returns. Ownership is decided by FOLDED equality
        (`_fold_series_title_key`: NFC-normalize, collapse whitespace,
        lowercase) between `series_title` and the first path segment of
        each `volume_uploads.volume_key`, not a SQL `LIKE`.

        This replaces a prior unescaped-`LIKE` implementation whose safety
        argument (Task 11 review round 1, F2/F5/F6) was wrong for the exact
        case the brief singled out as load-bearing: an UNTRACKED folder
        whose name happens to contain `%`/`_` could match a DIFFERENT,
        tracked folder's rows and so be granted despite having zero
        `volume_uploads` rows of its own — e.g. `'Dr_Stone'` (no rows)
        matching `'Dr Stone'` (tracked), or `'%'`/`'A%'` (no rows) matching
        anything — precisely the free-for-all the safe default exists to
        prevent. The same unescaped pattern could also FALSELY DENY a
        genuine sole owner of a `LIKE`-collidable name (`'A_ia'` vs
        `'Aria'`). Folded-equality comparison has neither failure mode: no
        wildcard character is special, and NFC/whitespace/case differences
        the reader itself treats as the same title also compare equal here.
        A folder with no tracked volumes returns an empty set.
        """
        prefix = series_title.strip("/")
        if not prefix:
            return set()
        target_key = _fold_series_title_key(prefix)
        return {
            username
            for folder, username in self._volume_upload_folder_owners()
            if _fold_series_title_key(folder) == target_key
        }

    def can_user_edit_series(self, username: str, series_title: str) -> bool:
        """True when `username` owns EVERY tracked volume in a series folder.

        The safe default for a folder with no ownership records — legacy
        content, or a series uploaded before ownership tracking existed — is
        False: an uploader may not claim an untracked series just by being
        the first to PUT its `series.json`. Only a role holding
        `Permission.MODIFY_DELETE` may edit an unowned/untracked series
        (enforced by the caller, `AuthMiddleware._authorize_put`).
        """
        owners = self.series_owners(series_title)
        return bool(owners) and owners == {username}

    def list_series_owned_by(self, username: str) -> list[str]:
        """Series folder names `username` may edit, per `can_user_edit_series`.

        Feeds the identity endpoint's `metadata.ownedSeries` — called on
        every reader connect and login-page load, so this does ONE pass
        over `_volume_upload_folder_owners()` (Task 11 review round 2, N3),
        folding every row's folder ONCE into a `folded_key -> owners` map
        and a parallel `folded_key -> raw folder spellings` map, rather
        than the round-1 approach of calling `can_user_edit_series` (a
        full re-scan) once per candidate folder. A folder where this user
        owns some but not all tracked volumes is excluded — it is not
        editable by them either, so it must not appear in their list.
        """
        owners_by_key: dict[str, set[str]] = {}
        folders_by_key: dict[str, set[str]] = {}
        for folder, owner in self._volume_upload_folder_owners():
            key = _fold_series_title_key(folder)
            owners_by_key.setdefault(key, set()).add(owner)
            folders_by_key.setdefault(key, set()).add(folder)

        owned: set[str] = set()
        for key, owners in owners_by_key.items():
            if owners == {username}:
                owned.update(folders_by_key[key])
        return sorted(owned)

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

    # Series metadata operations

    @staticmethod
    def _load_json_object(raw: Any, fallback: Any) -> Any:
        """Decode a JSON column, degrading to *fallback* on corruption.

        These columns are written by this class alone, but a half-written row
        or a hand-edited database must not take the whole metadata compiler
        down: a series whose facts cannot be read is a factless series.
        """
        if not isinstance(raw, str):
            return fallback
        try:
            decoded = json.loads(raw)
        except ValueError:
            return fallback
        return decoded if isinstance(decoded, type(fallback)) else fallback

    @staticmethod
    def _bindable_offset(value: Any) -> float | int | None:
        """Reduce an alignment number to something SQLite can actually store.

        `spine_offset` is index data preserved verbatim from a client PUT, so a
        hostile payload can carry an integer wider than SQLite's 64 bits (which
        sqlite3 refuses to bind) or a non-finite float. Neither is a shelf
        nudge; both degrade to "no offset" rather than aborting a library-wide
        publish at the write. Per-volume offsets need no such guard — they ride
        in a JSON column, where any magnitude round-trips as written.
        """
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return None
        if isinstance(value, int):
            return value if -(2**63) <= value < 2**63 else None
        return value if math.isfinite(value) else None

    def _series_facts_from_row(self, row: sqlite3.Row) -> SeriesFactsRow:
        return SeriesFactsRow(
            series_key=str(row["series_key"]),
            series_title=str(row["series_title"]),
            external_ids=self._load_json_object(row["external_ids"], {}),
            titles=self._load_json_object(row["titles"], {}),
            synonyms=self._load_json_object(row["synonyms"], []),
            tag=row["tag"],
            unit=row["unit"],
            facts_updated_at=str(row["facts_updated_at"]),
            spine_offset=row["spine_offset"],
            volume_offsets=self._load_json_object(row["volume_offsets"], {}),
            updated_by=row["updated_by"],
            updated_at=str(row["updated_at"]),
        )

    def get_series_facts(self, series_key: str) -> SeriesFactsRow | None:
        """Stored facts for one series, keyed by normalized series title."""
        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT * FROM series_facts WHERE series_key = ?", (series_key,)
            )
            row = cursor.fetchone()
            return self._series_facts_from_row(row) if row else None

    def list_series_facts(self) -> list[SeriesFactsRow]:
        """Every stored series, including ones whose folder is gone."""
        with self._connection() as conn:
            cursor = conn.execute("SELECT * FROM series_facts")
            return [self._series_facts_from_row(row) for row in cursor.fetchall()]

    def put_series_facts(self, row: SeriesFactsRow) -> None:
        """Insert or replace one series' facts and shelf alignment."""
        with self._connection() as conn:
            conn.execute(
                """
                INSERT INTO series_facts (
                    series_key, series_title, external_ids, titles, synonyms,
                    tag, unit, facts_updated_at, spine_offset, volume_offsets,
                    updated_by, updated_at
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))
                ON CONFLICT(series_key) DO UPDATE SET
                    series_title = excluded.series_title,
                    external_ids = excluded.external_ids,
                    titles = excluded.titles,
                    synonyms = excluded.synonyms,
                    tag = excluded.tag,
                    unit = excluded.unit,
                    facts_updated_at = excluded.facts_updated_at,
                    spine_offset = excluded.spine_offset,
                    volume_offsets = excluded.volume_offsets,
                    updated_by = excluded.updated_by,
                    updated_at = datetime('now')
                """,
                (
                    row["series_key"],
                    row["series_title"],
                    json.dumps(row["external_ids"], ensure_ascii=False),
                    json.dumps(row["titles"], ensure_ascii=False),
                    json.dumps(row["synonyms"], ensure_ascii=False),
                    row["tag"],
                    row["unit"],
                    row["facts_updated_at"],
                    self._bindable_offset(row["spine_offset"]),
                    json.dumps(row["volume_offsets"], ensure_ascii=False),
                    row["updated_by"],
                ),
            )

    def get_cached_volume_entry(
        self,
        volume_key: str,
        cbz_size: int,
        cbz_mtime: float,
        sidecar_key: str,
    ) -> dict[str, Any] | None:
        """A previously compiled volume entry, if the sources are unchanged.

        Every stat is compared in Python rather than in SQL: floats compare
        exactly here (they round-trip through REAL unchanged) and a mismatch
        must be a miss, never an approximate hit.
        """
        with self._connection() as conn:
            cursor = conn.execute(
                "SELECT * FROM series_entry_cache WHERE volume_key = ?", (volume_key,)
            )
            row = cursor.fetchone()
        if row is None:
            return None
        if int(row["cbz_size"]) != cbz_size or float(row["cbz_mtime"]) != cbz_mtime:
            return None
        if str(row["sidecar_key"]) != sidecar_key:
            return None
        entry = self._load_json_object(row["entry_json"], {})
        return cast("dict[str, Any]", entry) if entry else None

    def put_cached_volume_entry(
        self,
        volume_key: str,
        series_key: str,
        entry: dict[str, Any],
        cbz_size: int,
        cbz_mtime: float,
        sidecar_key: str,
    ) -> None:
        """Remember a compiled volume entry against its sources' stat."""
        with self._connection() as conn:
            conn.execute(
                """
                INSERT INTO series_entry_cache (
                    volume_key, series_key, entry_json, cbz_size, cbz_mtime,
                    sidecar_key, computed_at
                ) VALUES (?, ?, ?, ?, ?, ?, datetime('now'))
                ON CONFLICT(volume_key) DO UPDATE SET
                    series_key = excluded.series_key,
                    entry_json = excluded.entry_json,
                    cbz_size = excluded.cbz_size,
                    cbz_mtime = excluded.cbz_mtime,
                    sidecar_key = excluded.sidecar_key,
                    computed_at = datetime('now')
                """,
                (
                    volume_key,
                    series_key,
                    json.dumps(entry, ensure_ascii=False),
                    cbz_size,
                    cbz_mtime,
                    sidecar_key,
                ),
            )

    def prune_series_entry_cache(self, keep_volume_keys: Iterable[str]) -> int:
        """Drop cache rows for volumes that no longer exist. Returns the count.

        The scan and the deletes share one `_connection()` block, so the whole
        prune takes the write lock once instead of once per stale row. The
        deletes are issued one `execute()` at a time (not `executemany`)
        because only `execute` goes through the lock-retry proxy, and a delete
        by primary key is idempotent, so a retried one cannot double-apply.
        """
        keep = set(keep_volume_keys)
        with self._connection() as conn:
            cursor = conn.execute("SELECT volume_key FROM series_entry_cache")
            stale = [
                str(row["volume_key"])
                for row in cursor.fetchall()
                if str(row["volume_key"]) not in keep
            ]
            for volume_key in stale:
                conn.execute("DELETE FROM series_entry_cache WHERE volume_key = ?", (volume_key,))
            return len(stale)
