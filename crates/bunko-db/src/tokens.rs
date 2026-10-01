//! Bearer tokens (spec §6), the only session mechanism.
//!
//! Reproduced exactly: `secrets.token_urlsafe(32)` tokens (43 chars), only the SHA-256 hex
//! of the token stored, REAL epoch-second times, per-kind lifetimes, `last_used_at`
//! written at most once a minute, role/status read from the user row on every resolve,
//! expired rows left for `prune_expired_auth_tokens`, which 0.5.2 calls only on a
//! successful token login.
//!
//! The touch is a separate write after the read (0.5.2 did both under one process lock);
//! two concurrent resolves may both write the same `last_used_at`, which is harmless.

use crate::database::Database;
use crate::error::{DbError, Result};
use crate::pytime::epoch_now;
use crate::users::{RawUser, User};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

/// A token's last use is written at most this often, seconds.
pub const TOKEN_TOUCH_SECONDS: f64 = 60.0;

/// What a token is for; sets only its lifetime (any kind is valid for any request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    Web,
    Reader,
    Processor,
}

impl TokenKind {
    pub const ALL: [TokenKind; 3] = [TokenKind::Web, TokenKind::Reader, TokenKind::Processor];

    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Web => "web",
            TokenKind::Reader => "reader",
            TokenKind::Processor => "processor",
        }
    }

    /// 0.5.2 `TOKEN_KINDS`: web 7 days, reader 90 days, processor 30 days.
    pub fn lifetime_seconds(self) -> f64 {
        match self {
            TokenKind::Web => 7.0 * 86400.0,
            TokenKind::Reader => 90.0 * 86400.0,
            TokenKind::Processor => 30.0 * 86400.0,
        }
    }
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TokenKind {
    type Err = DbError;
    /// Unknown kinds fail with 0.5.2's `unknown token kind 'x'`.
    fn from_str(s: &str) -> Result<Self> {
        TokenKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| {
                DbError::Invalid(format!("unknown token kind {}", crate::pyfmt::repr_str(s)))
            })
    }
}

/// SHA-256 hex of the token's UTF-8 bytes: what `auth_tokens.token_hash` holds.
pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Python `secrets.token_urlsafe(n)`: `n` random bytes, base64url without padding.
pub(crate) fn token_urlsafe(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Database {
    /// A new token for `username`: `(token, expires_at)` (epoch seconds). The caller has
    /// checked the password; the user's existence is not re-checked (as 0.5.2).
    /// `lifetime_seconds` overrides the kind's lifetime (negative is allowed: tests).
    pub fn create_auth_token(
        &self,
        username: &str,
        kind: TokenKind,
        label: &str,
        lifetime_seconds: Option<f64>,
    ) -> Result<(String, f64)> {
        let token = token_urlsafe(32);
        let now = epoch_now();
        let expires_at = now + lifetime_seconds.unwrap_or_else(|| kind.lifetime_seconds());
        // `label[:200]`: the first 200 code points.
        let label: String = label.chars().take(200).collect();
        self.write(|conn| {
            conn.execute(
                "INSERT INTO auth_tokens \
                 (token_hash, username, kind, label, created_at, expires_at, last_used_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    token_hash(&token),
                    username,
                    kind.as_str(),
                    label,
                    now,
                    expires_at,
                    now
                ],
            )?;
            Ok(())
        })?;
        Ok((token, expires_at))
    }

    /// The ACTIVE user a live token belongs to, or `None` (unknown, expired, or the account
    /// is not active). Writes `last_used_at` when the last write is a minute old or more.
    pub fn resolve_auth_token(&self, token: &str) -> Result<Option<User>> {
        if token.is_empty() {
            return Ok(None);
        }
        let digest = token_hash(token);
        let now = epoch_now();
        let row = self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT t.expires_at, t.last_used_at, u.id, u.username, u.role, u.status, \
                     u.notes, u.created_at FROM auth_tokens t JOIN users u \
                     ON u.username = t.username WHERE t.token_hash = ?",
                )?
                .query_row([&digest], |r| {
                    Ok((
                        r.get::<_, f64>(0)?,
                        r.get::<_, f64>(1)?,
                        RawUser::from_row(r, 2)?,
                    ))
                })
                .optional()?)
        })?;
        let Some((expires_at, last_used_at, raw)) = row else {
            return Ok(None);
        };
        if expires_at <= now || raw.status != "active" {
            return Ok(None);
        }
        let Some(user) = raw.into_login() else {
            return Ok(None);
        };
        if now - last_used_at >= TOKEN_TOUCH_SECONDS {
            self.write(|conn| {
                conn.execute(
                    "UPDATE auth_tokens SET last_used_at = ? WHERE token_hash = ?",
                    params![now, digest],
                )?;
                Ok(())
            })?;
        }
        Ok(Some(user))
    }

    /// Sign this one token out. `true` if it existed.
    pub fn revoke_auth_token(&self, token: &str) -> Result<bool> {
        let digest = token_hash(token);
        self.write(|conn| {
            Ok(conn.execute("DELETE FROM auth_tokens WHERE token_hash = ?", [digest])? > 0)
        })
    }

    /// Sign every token of `username` out; how many there were.
    pub fn revoke_user_auth_tokens(&self, username: &str) -> Result<usize> {
        self.write(|conn| {
            Ok(conn.execute("DELETE FROM auth_tokens WHERE username = ?", [username])?)
        })
    }

    /// Drop tokens past their expiry; how many.
    pub fn prune_expired_auth_tokens(&self) -> Result<usize> {
        let now = epoch_now();
        self.write(|conn| Ok(conn.execute("DELETE FROM auth_tokens WHERE expires_at <= ?", [now])?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserStatus;
    use crate::testutil::temp_db;
    use bunko_core::Role;

    fn last_used(db: &Database, token: &str) -> f64 {
        db.with_writer_connection(|c| {
            c.query_row(
                "SELECT last_used_at FROM auth_tokens WHERE token_hash = ?",
                [token_hash(token)],
                |r| r.get(0),
            )
        })
        .unwrap()
    }

    #[test]
    fn tokens_name_their_user_and_store_only_a_hash() {
        let (_dir, db) = temp_db();
        db.create_user(
            "alice",
            "password123",
            Role::Uploader,
            UserStatus::Active,
            "",
        )
        .unwrap();
        let (token, expires) = db
            .create_auth_token("alice", TokenKind::Web, "tab", None)
            .unwrap();
        assert_eq!(token.len(), 43);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        let lifetime = expires - epoch_now();
        assert!((lifetime - 7.0 * 86400.0).abs() < 5.0);
        let user = db.resolve_auth_token(&token).unwrap().unwrap();
        assert_eq!(user.username, "alice");
        let (stored, label): (String, String) = db
            .with_writer_connection(|c| {
                c.query_row("SELECT token_hash, label FROM auth_tokens", [], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
            })
            .unwrap();
        assert_eq!(stored, token_hash(&token));
        assert_eq!(stored.len(), 64);
        assert_eq!(label, "tab");
        assert!(db.resolve_auth_token("nope").unwrap().is_none());
        assert!(db.resolve_auth_token("").unwrap().is_none());
    }

    #[test]
    fn kinds_lifetimes_and_labels() {
        let (_dir, db) = temp_db();
        for kind in TokenKind::ALL {
            let (_, exp) = db.create_auth_token("u", kind, "", None).unwrap();
            assert!((exp - epoch_now() - kind.lifetime_seconds()).abs() < 5.0);
        }
        assert_eq!(
            "bogus".parse::<TokenKind>().unwrap_err().to_string(),
            "unknown token kind 'bogus'"
        );
        let long = "\u{e9}".repeat(300);
        db.create_auth_token("u", TokenKind::Web, &long, None)
            .unwrap();
        let max: i64 = db
            .with_writer_connection(|c| {
                c.query_row("SELECT max(length(label)) FROM auth_tokens", [], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(max, 200);
    }

    #[test]
    fn expiry_status_role_and_revocation() {
        let (_dir, db) = temp_db();
        db.create_user(
            "alice",
            "password123",
            Role::Uploader,
            UserStatus::Active,
            "",
        )
        .unwrap();
        let (expired, _) = db
            .create_auth_token("alice", TokenKind::Web, "", Some(-1.0))
            .unwrap();
        assert!(db.resolve_auth_token(&expired).unwrap().is_none());
        assert_eq!(db.prune_expired_auth_tokens().unwrap(), 1);

        let (t1, _) = db
            .create_auth_token("alice", TokenKind::Web, "", None)
            .unwrap();
        let (t2, _) = db
            .create_auth_token("alice", TokenKind::Reader, "", None)
            .unwrap();
        db.update_user_role("alice", Role::Editor).unwrap();
        assert_eq!(
            db.resolve_auth_token(&t1).unwrap().unwrap().role,
            Role::Editor
        );
        db.disable_user("alice").unwrap();
        assert!(db.resolve_auth_token(&t1).unwrap().is_none());
        db.with_writer_connection(|c| c.execute("UPDATE users SET status = 'active'", []))
            .unwrap();
        assert!(db.revoke_auth_token(&t1).unwrap());
        assert!(!db.revoke_auth_token(&t1).unwrap());
        assert!(
            db.resolve_auth_token(&t2).unwrap().is_some(),
            "logout revokes that token only"
        );
        db.update_user_password("alice", "password456").unwrap();
        assert!(
            db.resolve_auth_token(&t2).unwrap().is_none(),
            "a new password signs out all"
        );
        let (t3, _) = db
            .create_auth_token("alice", TokenKind::Web, "", None)
            .unwrap();
        db.delete_user("alice").unwrap();
        assert!(db.resolve_auth_token(&t3).unwrap().is_none());
        assert_eq!(
            db.revoke_user_auth_tokens("alice").unwrap(),
            0,
            "delete wiped them"
        );
    }

    #[test]
    fn last_use_is_written_at_most_once_a_minute() {
        let (_dir, db) = temp_db();
        db.create_user(
            "alice",
            "password123",
            Role::Uploader,
            UserStatus::Active,
            "",
        )
        .unwrap();
        let (token, _) = db
            .create_auth_token("alice", TokenKind::Web, "", None)
            .unwrap();
        let created = last_used(&db, &token);
        db.resolve_auth_token(&token).unwrap();
        assert_eq!(last_used(&db, &token), created);
        db.with_writer_connection(|c| {
            c.execute(
                "UPDATE auth_tokens SET last_used_at = last_used_at - 61",
                [],
            )
        })
        .unwrap();
        db.resolve_auth_token(&token).unwrap();
        assert!(last_used(&db, &token) >= created);
    }
}
