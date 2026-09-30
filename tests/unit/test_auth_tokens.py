"""Bearer tokens: a password is checked once, then a token stands in for it.

Basic auth sent the password with every request and cost a bcrypt check each
time; the web pages kept the password itself in sessionStorage. A token is a
random secret the server stores only as a hash, with an owner, a kind and an
expiry; each request costs one indexed lookup.
"""

from __future__ import annotations

import hashlib
import time
from pathlib import Path

import pytest

from mokuro_bunko.database import TOKEN_KINDS, Database


@pytest.fixture
def db(tmp_path: Path) -> Database:
    database = Database(tmp_path / "mokuro.db")
    database.create_user("alice", "alice-password-1", "uploader")
    database.create_user("bob", "bob-password-12", "registered")
    return database


def test_a_token_names_its_user(db: Database) -> None:
    token, expires_at = db.create_auth_token("alice", "web")
    assert len(token) >= 40
    assert expires_at > time.time()
    user = db.resolve_auth_token(token)
    assert user is not None and user["username"] == "alice" and user["role"] == "uploader"


def test_only_a_hash_of_the_token_is_stored(db: Database) -> None:
    token, _ = db.create_auth_token("alice", "web")
    with db._connection() as conn:
        stored = [row[0] for row in conn.execute("SELECT token_hash FROM auth_tokens")]
    assert stored == [hashlib.sha256(token.encode()).hexdigest()]
    assert token not in stored


def test_an_unknown_token_is_nobody(db: Database) -> None:
    db.create_auth_token("alice", "web")
    assert db.resolve_auth_token("not-a-token") is None
    assert db.resolve_auth_token("") is None


def test_an_expired_token_is_nobody(db: Database) -> None:
    token, _ = db.create_auth_token("alice", "web", lifetime_seconds=-1)
    assert db.resolve_auth_token(token) is None


def test_every_kind_has_a_lifetime(db: Database) -> None:
    for kind in TOKEN_KINDS:
        token, expires_at = db.create_auth_token("alice", kind)
        assert expires_at > time.time() and db.resolve_auth_token(token) is not None
    with pytest.raises(ValueError):
        db.create_auth_token("alice", "forever")


def test_a_deleted_or_inactive_account_is_refused_at_once(db: Database) -> None:
    token, _ = db.create_auth_token("alice", "web")
    db.delete_user("alice")
    assert db.resolve_auth_token(token) is None


def test_a_role_change_is_seen_on_the_next_request(db: Database) -> None:
    token, _ = db.create_auth_token("alice", "web")
    db.update_user_role("alice", "editor")
    user = db.resolve_auth_token(token)
    assert user is not None and user["role"] == "editor"


def test_logout_revokes_that_token_only(db: Database) -> None:
    first, _ = db.create_auth_token("alice", "web")
    second, _ = db.create_auth_token("alice", "reader")
    assert db.revoke_auth_token(first)
    assert db.resolve_auth_token(first) is None
    assert db.resolve_auth_token(second) is not None


def test_a_password_change_revokes_every_token_of_that_user(db: Database) -> None:
    alices = [db.create_auth_token("alice", kind)[0] for kind in ("web", "reader")]
    bobs, _ = db.create_auth_token("bob", "web")
    db.update_user_password("alice", "a-new-password-2")
    assert all(db.resolve_auth_token(t) is None for t in alices)
    assert db.resolve_auth_token(bobs) is not None


def test_deleting_a_user_revokes_their_tokens(db: Database) -> None:
    token, _ = db.create_auth_token("alice", "web")
    db.delete_user("alice")
    db.restore_user("alice", "restored-password-3")
    assert db.resolve_auth_token(token) is None


def test_expired_tokens_are_pruned(db: Database) -> None:
    db.create_auth_token("alice", "web", lifetime_seconds=-1)
    live, _ = db.create_auth_token("alice", "web")
    assert db.prune_expired_auth_tokens() == 1
    assert db.resolve_auth_token(live) is not None


def test_last_use_is_written_at_most_once_a_minute(db: Database) -> None:
    token, _ = db.create_auth_token("alice", "web")
    digest = hashlib.sha256(token.encode()).hexdigest()

    def last_used() -> float:
        with db._connection() as conn:
            return float(conn.execute(
                "SELECT last_used_at FROM auth_tokens WHERE token_hash = ?", (digest,)
            ).fetchone()[0])

    first = last_used()
    db.resolve_auth_token(token)
    assert last_used() == first
    with db._connection() as conn:
        conn.execute("UPDATE auth_tokens SET last_used_at = last_used_at - 120")
    db.resolve_auth_token(token)
    assert last_used() > first - 120
