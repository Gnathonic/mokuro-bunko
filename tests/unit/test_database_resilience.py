"""Tests for Database resilience: lock retries, prune throttle, invite cleanup.

Ports the DB-resilience ideas from PR #2 (by MokuroEnjoyer), adapted to the
persistent single-connection architecture main uses now: retries wrap each
statement and the commit (where an external writer's lock bites), not the
connect.
"""

from __future__ import annotations

import sqlite3
import threading
import time
from datetime import datetime, timedelta
from pathlib import Path

import pytest

from mokuro_bunko.database import Database


@pytest.fixture
def db(tmp_path: Path) -> Database:
    return Database(tmp_path / "test.db")


def _hold_external_write_lock(
    db_path: Path, hold_seconds: float, acquired: threading.Event
) -> None:
    """Acquire a write lock on the DB from a separate connection, then release."""
    conn = sqlite3.connect(db_path, timeout=30)
    try:
        conn.execute("PRAGMA busy_timeout=10000")
        conn.execute("BEGIN IMMEDIATE")
        acquired.set()
        time.sleep(hold_seconds)
        conn.rollback()
    finally:
        conn.close()


class TestCommitLockRetry:
    """Commit retries with backoff when an external process holds the lock."""

    def test_succeeds_after_external_lock_releases(self, db: Database, tmp_path: Path) -> None:
        db.configure_connection(
            busy_timeout_ms=100,
            lock_retries=6,
            retry_initial_delay_seconds=0.05,
        )
        acquired = threading.Event()
        locker = threading.Thread(
            target=_hold_external_write_lock,
            args=(tmp_path / "test.db", 0.6, acquired),
        )
        locker.start()
        try:
            assert acquired.wait(timeout=5)
            # Write must survive the ~0.6s external lock via retries.
            db.create_user("lockeduser", "password123", "registered")
        finally:
            locker.join()
        assert db.get_user("lockeduser") is not None

    def test_raises_after_retries_exhausted(self, db: Database, tmp_path: Path) -> None:
        db.configure_connection(
            busy_timeout_ms=100,
            lock_retries=2,
            retry_initial_delay_seconds=0.01,
        )
        acquired = threading.Event()
        locker = threading.Thread(
            target=_hold_external_write_lock,
            args=(tmp_path / "test.db", 2.0, acquired),
        )
        locker.start()
        try:
            assert acquired.wait(timeout=5)
            with pytest.raises(sqlite3.OperationalError):
                db.create_user("lockeduser", "password123", "registered")
        finally:
            locker.join()


class TestConfigureConnection:
    """Runtime tuning knobs are applied and clamped."""

    def test_clamps_minimums(self, db: Database) -> None:
        db.configure_connection(
            busy_timeout_ms=1,
            lock_retries=0,
            retry_initial_delay_seconds=0.0,
        )
        assert db.busy_timeout_ms == 100
        assert db.lock_retries == 1
        assert db.retry_initial_delay_seconds > 0

    def test_applies_busy_timeout_to_live_connection(self, db: Database) -> None:
        db.configure_connection(busy_timeout_ms=250)
        with db._connection() as conn:
            timeout = conn.execute("PRAGMA busy_timeout").fetchone()[0]
        assert timeout == 250

    def test_none_leaves_settings_unchanged(self, db: Database) -> None:
        before = (db.busy_timeout_ms, db.lock_retries, db.retry_initial_delay_seconds)
        db.configure_connection()
        assert (db.busy_timeout_ms, db.lock_retries, db.retry_initial_delay_seconds) == before


class TestAuditPruneThrottle:
    """Audit pruning runs at most once per interval, not on every write."""

    def _insert_old_audit_row(self, db: Database) -> int:
        with db._connection() as conn:
            cursor = conn.execute(
                """
                INSERT INTO audit_logs (actor_username, action, created_at)
                VALUES ('old', 'stale_event', datetime('now', '-40 days'))
                """
            )
            return cursor.lastrowid or 0

    def _audit_row_exists(self, db: Database, row_id: int) -> bool:
        with db._connection() as conn:
            return conn.execute(
                "SELECT 1 FROM audit_logs WHERE id = ?", (row_id,)
            ).fetchone() is not None

    def test_first_event_prunes(self, db: Database) -> None:
        old_id = self._insert_old_audit_row(db)
        db.log_audit_event("something_happened")
        assert not self._audit_row_exists(db, old_id)

    def test_prune_throttled_within_interval(self, db: Database) -> None:
        db.log_audit_event("warm_up")  # performs the initial prune
        old_id = self._insert_old_audit_row(db)
        db.log_audit_event("second_event")  # within interval -> no prune
        assert self._audit_row_exists(db, old_id)

    def test_prune_runs_again_after_interval(self, db: Database) -> None:
        db.log_audit_event("warm_up")
        old_id = self._insert_old_audit_row(db)
        db._last_audit_prune_monotonic -= Database.AUDIT_PRUNE_INTERVAL_SECONDS + 1
        db.log_audit_event("third_event")
        assert not self._audit_row_exists(db, old_id)


class TestCleanupExpiredInvites:
    """Expired-invite cleanup compares timestamps in Python, not SQL text.

    Invites are stored with datetime.now().isoformat() (local-naive, 'T'
    separator); comparing that TEXT against SQLite's UTC datetime('now')
    is broken both by timezone and lexicographically ('T' > ' ').
    """

    def _insert_invite(self, db: Database, code: str, expires_at: str) -> None:
        with db._connection() as conn:
            conn.execute(
                "INSERT INTO invites (code, role, expires_at) VALUES (?, 'registered', ?)",
                (code, expires_at),
            )

    def _invite_exists(self, db: Database, code: str) -> bool:
        return db.get_invite(code) is not None

    def test_removes_invite_expired_earlier_today(self, db: Database) -> None:
        # The regression case: same-day expiry, isoformat storage.
        expired = (datetime.now() - timedelta(hours=1)).isoformat()
        self._insert_invite(db, "expired-code", expired)

        assert db.cleanup_expired_invites() == 1
        assert not self._invite_exists(db, "expired-code")

    def test_keeps_invite_expiring_soon(self, db: Database) -> None:
        # Complementary regression: when the local date lags the UTC date
        # (evening in UTC-negative zones), the old TEXT comparison deleted
        # invites that were still valid for minutes more.
        soon = (datetime.now() + timedelta(minutes=10)).isoformat()
        self._insert_invite(db, "soon-code", soon)

        assert db.cleanup_expired_invites() == 0
        assert self._invite_exists(db, "soon-code")

    def test_keeps_valid_and_used_invites(self, db: Database) -> None:
        valid_code = db.create_invite(role="registered", expires="7d")
        used = (datetime.now() - timedelta(hours=2)).isoformat()
        self._insert_invite(db, "used-code", used)
        with db._connection() as conn:
            conn.execute(
                "UPDATE invites SET used_by = 'someone' WHERE code = 'used-code'"
            )

        assert db.cleanup_expired_invites() == 0
        assert self._invite_exists(db, valid_code)
        assert self._invite_exists(db, "used-code")

    def test_skips_malformed_expiry_without_crashing(self, db: Database) -> None:
        self._insert_invite(db, "weird-code", "not-a-timestamp")
        expired = (datetime.now() - timedelta(days=1)).isoformat()
        self._insert_invite(db, "gone-code", expired)

        assert db.cleanup_expired_invites() == 1
        assert self._invite_exists(db, "weird-code")
        assert not self._invite_exists(db, "gone-code")


class TestServerWiring:
    """create_app applies config.database tuning to the Database."""

    def test_create_app_configures_database(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.config import Config
        from mokuro_bunko.server import create_app

        calls: list[dict[str, object]] = []
        original = Database.configure_connection

        def recording(self: Database, **kwargs: object) -> None:
            calls.append(kwargs)
            original(self, **kwargs)  # type: ignore[arg-type]

        monkeypatch.setattr(Database, "configure_connection", recording)

        storage = tmp_path / "storage"
        storage.mkdir()
        config = Config.from_dict({
            "storage": {"base_path": str(storage)},
            "database": {
                "busy_timeout_ms": 2500,
                "lock_retries": 3,
                "retry_initial_delay_seconds": 0.1,
            },
        })
        create_app(config)

        assert calls == [{
            "busy_timeout_ms": 2500,
            "lock_retries": 3,
            "retry_initial_delay_seconds": 0.1,
        }]
