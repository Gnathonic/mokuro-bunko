//! Users (spec §5): accounts, passwords, statuses and the `users_version` counter.
//!
//! Reproduced: soft delete (the row and name survive), no re-enable path for `disabled`
//! (approve needs `pending`, restore needs `deleted`), case-sensitive usernames, the exact
//! `ValueError` messages, `users_version` bumped by the seven account mutators even when
//! they fail (Python's `finally`), never by notes/invites/tokens.
//!
//! Choices:
//! - `list_users` orders `created_at DESC, id DESC` (0.5.2 left same-second ties
//!   unordered; the spec allows the tiebreak).
//! - A row whose `role`/`status` this build does not know (0.5.2 raised an unhandled
//!   `ValueError`, a 500 that bricked that user): `get_user` returns
//!   [`DbError::CorruptRow`], `list_users` skips the row with a warning (the admin page
//!   keeps working and `update_user_role` can repair it by name), and
//!   `authenticate_user`/`resolve_auth_token` refuse the login.
//! - bcrypt never runs while a database lock is held.

use crate::database::Database;
use crate::error::{DbError, Result};
use crate::pyfmt;
use crate::validation::{hash_password, validate_password, validate_username, verify_password};
use bunko_core::Role;
use rusqlite::{Connection, ErrorCode, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::Ordering;

/// `users.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserStatus {
    Active,
    Pending,
    Disabled,
    Deleted,
}

impl UserStatus {
    pub const ALL: [UserStatus; 4] = [
        UserStatus::Active,
        UserStatus::Pending,
        UserStatus::Disabled,
        UserStatus::Deleted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            UserStatus::Active => "active",
            UserStatus::Pending => "pending",
            UserStatus::Disabled => "disabled",
            UserStatus::Deleted => "deleted",
        }
    }
}

impl fmt::Display for UserStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for UserStatus {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        UserStatus::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| format!("unknown status '{s}'"))
    }
}

/// 0.5.2 `UserDict`: exactly the fields (and JSON key order) every API returns for a user.
/// `password_hash` and `updated_at` are never exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub status: UserStatus,
    pub notes: String,
    /// `YYYY-MM-DD HH:MM:SS` UTC, verbatim from the row.
    pub created_at: String,
}

/// The sorted role list 0.5.2 prints in "Invalid role" messages (`sorted(VALID_ROLES)`).
const VALID_ROLES_SORTED: &str =
    "['admin', 'anonymous', 'editor', 'inviter', 'processor', 'registered', 'uploader']";

/// 0.5.2 `normalize_role`: legacy `writer` -> `uploader`; anything else unknown is
/// `Invalid role: <role>. Must be one of: [...]`. Accepts all seven roles.
pub fn normalize_role(role: &str) -> Result<Role> {
    Role::from_str(role).map_err(|_| {
        DbError::Invalid(format!(
            "Invalid role: {role}. Must be one of: {VALID_ROLES_SORTED}"
        ))
    })
}

/// Raw user columns as stored; converted to [`User`] only once the caller knows it wants
/// one (so a corrupt role can be handled per call site).
pub(crate) struct RawUser {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub status: String,
    pub notes: String,
    pub created_at: String,
}

impl RawUser {
    /// Columns `id, username, role, status, notes, created_at` starting at `offset`.
    pub(crate) fn from_row(row: &Row<'_>, offset: usize) -> rusqlite::Result<RawUser> {
        Ok(RawUser {
            id: row.get(offset)?,
            username: row.get(offset + 1)?,
            role: row.get(offset + 2)?,
            status: row.get(offset + 3)?,
            notes: row.get(offset + 4)?,
            created_at: row.get(offset + 5)?,
        })
    }

    pub(crate) fn into_user(self) -> Result<User> {
        let role = Role::from_str(&self.role).map_err(|_| DbError::CorruptRow {
            table: "users",
            detail: format!("user '{}' has unknown role '{}'", self.username, self.role),
        })?;
        let status = UserStatus::from_str(&self.status).map_err(|_| DbError::CorruptRow {
            table: "users",
            detail: format!(
                "user '{}' has unknown status '{}'",
                self.username, self.status
            ),
        })?;
        Ok(User {
            id: self.id,
            username: self.username,
            role,
            status,
            notes: self.notes,
            created_at: self.created_at,
        })
    }

    /// For authentication paths: a corrupt row cannot sign in.
    pub(crate) fn into_login(self) -> Option<User> {
        match self.into_user() {
            Ok(user) => Some(user),
            Err(e) => {
                tracing::warn!(error = %e, "refusing a login for an unreadable account row");
                None
            }
        }
    }
}

const USER_COLUMNS: &str = "id, username, role, status, notes, created_at";

/// Bumps `users_version` when dropped: after the method body, whether it succeeded or not.
pub(crate) struct BumpOnDrop<'a>(&'a Database);

impl Drop for BumpOnDrop<'_> {
    fn drop(&mut self) {
        self.0.users_version.fetch_add(1, Ordering::SeqCst);
    }
}

fn validation(err: Option<&'static str>) -> Result<()> {
    match err {
        Some(msg) => Err(DbError::invalid(msg)),
        None => Ok(()),
    }
}

/// Insert an account row inside the caller's write transaction; returns its id. Errors
/// carry 0.5.2's messages (`already exists` as [`DbError::Conflict`], a deleted
/// account's name pointing at `restore-user`).
pub(crate) fn insert_user(
    conn: &Connection,
    username: &str,
    password_hash: &str,
    role: Role,
    status: UserStatus,
    notes: &str,
) -> Result<i64> {
    let inserted = conn.execute(
        "INSERT INTO users (username, password_hash, role, status, notes) \
         VALUES (?, ?, ?, ?, ?)",
        params![
            username,
            password_hash,
            role.as_str(),
            status.as_str(),
            notes
        ],
    );
    match inserted {
        Ok(_) => Ok(conn.last_insert_rowid()),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::ConstraintViolation => {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT status FROM users WHERE username = ?",
                    [username],
                    |r| r.get(0),
                )
                .optional()?;
            if existing.as_deref() == Some("deleted") {
                Err(DbError::Invalid(format!(
                    "Username '{username}' belongs to a deleted account; bring it back \
                     with: mokuro-bunko admin restore-user {username}"
                )))
            } else {
                Err(DbError::Conflict(format!(
                    "Username '{username}' already exists"
                )))
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// The refusal of a change that would leave no active admin (HTTP 409 in the admin API).
pub const LAST_ADMIN_MESSAGE: &str = "This is the last active admin account: make another \
account an admin first, so the server keeps someone who can administer it";

/// Would turning `username` into a non-admin (or making it inactive) leave no active
/// admin? Run inside the write transaction that makes the change.
fn is_last_active_admin(conn: &Connection, username: &str) -> Result<bool> {
    let target_is_admin: bool = conn
        .query_row(
            "SELECT role = 'admin' AND status = 'active' FROM users WHERE username = ?",
            [username],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false);
    if !target_is_admin {
        return Ok(false);
    }
    let others: i64 = conn.query_row(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND status = 'active' \
         AND username != ?",
        [username],
        |r| r.get(0),
    )?;
    Ok(others == 0)
}

/// Whether an account change may remove the last active admin: the web admin keeps one
/// ([`KeepAdmin::Keep`]); the CLI, run by whoever owns the server's files, may not need
/// to ([`KeepAdmin::Allow`], 0.5.2's behaviour).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeepAdmin {
    Keep,
    Allow,
}

fn guard_last_admin(conn: &Connection, username: &str, keep: KeepAdmin) -> Result<()> {
    if keep == KeepAdmin::Keep && is_last_active_admin(conn, username)? {
        return Err(DbError::Conflict(LAST_ADMIN_MESSAGE.into()));
    }
    Ok(())
}

impl Database {
    /// Moves on every change to who may log in as what (`create_user`, `update_user_role`,
    /// `update_user_password`, `approve_user`, `disable_user`, `delete_user`,
    /// `restore_user`). Per handle, per process — a CLI change elsewhere does not move it.
    pub fn users_version(&self) -> u64 {
        self.users_version.load(Ordering::SeqCst)
    }

    pub(crate) fn bump_users_version(&self) -> BumpOnDrop<'_> {
        BumpOnDrop(self)
    }

    /// Create an account; returns its id. Errors carry 0.5.2's messages:
    /// validation messages, `Username '<u>' already exists` ([`DbError::Conflict`]) or the
    /// deleted-account message pointing at `restore-user` ([`DbError::Invalid`]).
    pub fn create_user(
        &self,
        username: &str,
        password: &str,
        role: Role,
        status: UserStatus,
        notes: &str,
    ) -> Result<i64> {
        let _bump = self.bump_users_version();
        let password_hash = self.new_account_hash(username, password)?;
        self.write(|conn| insert_user(conn, username, &password_hash, role, status, notes))
    }

    /// Validate a new account's name and password and hash the password (outside every
    /// lock): the first half of [`create_user`](Self::create_user).
    pub(crate) fn new_account_hash(&self, username: &str, password: &str) -> Result<String> {
        if pyfmt::strip(username).is_empty() {
            return Err(DbError::invalid("Username is required"));
        }
        validation(validate_username(username))?;
        validation(validate_password(password))?;
        Ok(hash_password(password, self.bcrypt_cost)?)
    }

    /// Any account by name, whatever its status.
    pub fn get_user(&self, username: &str) -> Result<Option<User>> {
        let raw = self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {USER_COLUMNS} FROM users WHERE username = ?"
                ))?
                .query_row([username], |r| RawUser::from_row(r, 0))
                .optional()?)
        })?;
        raw.map(RawUser::into_user).transpose()
    }

    /// The account if it is `active` and the password matches. Exactly one bcrypt check
    /// whatever the outcome: an unknown or inactive account is checked against a dummy
    /// hash of the same cost, so the answer's timing does not tell which accounts exist.
    pub fn authenticate_user(&self, username: &str, password: &str) -> Result<Option<User>> {
        let row = self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {USER_COLUMNS}, password_hash FROM users WHERE username = ?"
                ))?
                .query_row([username], |r| {
                    Ok((RawUser::from_row(r, 0)?, r.get::<_, String>(6)?))
                })
                .optional()?)
        })?;
        let Some((raw, password_hash)) = row.filter(|(raw, _)| raw.status == "active") else {
            let _ = verify_password(password, self.dummy_hash()?);
            return Ok(None);
        };
        if !verify_password(password, &password_hash) {
            return Ok(None);
        }
        Ok(raw.into_login())
    }

    /// A bcrypt hash of a random password at this handle's cost, made once: what an
    /// unknown account's password is checked against.
    fn dummy_hash(&self) -> Result<&str> {
        if let Some(h) = self.dummy_hash.get() {
            return Ok(h);
        }
        let secret = crate::tokens::token_urlsafe(16);
        let hash = hash_password(&secret, self.bcrypt_cost)?;
        Ok(self.dummy_hash.get_or_init(|| hash))
    }

    /// A fingerprint of a PROCESSOR account as it stands (role, status and password
    /// hash), or `None` unless the account exists, is active and has the processor role:
    /// `sha256("{role}\0{status}\0{hash}").hexdigest()[:32]`.
    pub fn processor_account_stamp(&self, username: &str) -> Result<Option<String>> {
        let row: Option<(String, String, String)> = self.read(|conn| {
            Ok(conn
                .prepare_cached("SELECT role, status, password_hash FROM users WHERE username = ?")?
                .query_row([username], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?)
        })?;
        let Some((role, status, hash)) = row else {
            return Ok(None);
        };
        if role != "processor" || status != "active" {
            return Ok(None);
        }
        let digest = Sha256::digest(format!("{role}\0{status}\0{hash}").as_bytes());
        Ok(Some(hex::encode(digest)[..32].to_string()))
    }

    /// Every account (or those with `status`), newest first.
    pub fn list_users(&self, status: Option<UserStatus>) -> Result<Vec<User>> {
        let raws = self.read(|conn| {
            let rows = match status {
                Some(st) => conn
                    .prepare_cached(&format!(
                        "SELECT {USER_COLUMNS} FROM users WHERE status = ? \
                         ORDER BY created_at DESC, id DESC"
                    ))?
                    .query_map([st.as_str()], |r| RawUser::from_row(r, 0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => conn
                    .prepare_cached(&format!(
                        "SELECT {USER_COLUMNS} FROM users ORDER BY created_at DESC, id DESC"
                    ))?
                    .query_map([], |r| RawUser::from_row(r, 0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok(rows)
        })?;
        Ok(raws
            .into_iter()
            .filter_map(|raw| match raw.into_user() {
                Ok(user) => Some(user),
                Err(e) => {
                    tracing::warn!(error = %e, "list_users: skipping an unreadable row");
                    None
                }
            })
            .collect())
    }

    /// Set an account's role (any status, deleted included). Tokens are kept: they carry
    /// the account's current role. `false` if there is no such account.
    pub fn update_user_role(&self, username: &str, role: Role) -> Result<bool> {
        self.update_user_role_with(username, role, KeepAdmin::Allow)
    }

    /// [`update_user_role`](Self::update_user_role); with [`KeepAdmin::Keep`], demoting
    /// the last active admin is refused ([`DbError::Conflict`], [`LAST_ADMIN_MESSAGE`])
    /// in the same transaction as the change.
    pub fn update_user_role_with(
        &self,
        username: &str,
        role: Role,
        keep: KeepAdmin,
    ) -> Result<bool> {
        let _bump = self.bump_users_version();
        self.write(|conn| {
            if role != Role::Admin {
                guard_last_admin(conn, username, keep)?;
            }
            Ok(conn.execute(
                "UPDATE users SET role = ?, updated_at = datetime('now') WHERE username = ?",
                params![role.as_str(), username],
            )? > 0)
        })
    }

    /// Set the admin notes (does not move `users_version`).
    pub fn update_user_notes(&self, username: &str, notes: &str) -> Result<bool> {
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE users SET notes = ?, updated_at = datetime('now') WHERE username = ?",
                params![notes, username],
            )? > 0)
        })
    }

    /// Set a new password (validated) and sign out every token of the account.
    pub fn update_user_password(&self, username: &str, password: &str) -> Result<bool> {
        let _bump = self.bump_users_version();
        validation(validate_password(password))?;
        let password_hash = hash_password(password, self.bcrypt_cost)?;
        self.write(|conn| {
            let changed = conn.execute(
                "UPDATE users SET password_hash = ?, updated_at = datetime('now') \
                 WHERE username = ?",
                params![password_hash, username],
            )? > 0;
            if changed {
                conn.execute("DELETE FROM auth_tokens WHERE username = ?", [username])?;
            }
            Ok(changed)
        })
    }

    /// `pending` -> `active`. `false` if not found or not pending.
    pub fn approve_user(&self, username: &str) -> Result<bool> {
        let _bump = self.bump_users_version();
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE users SET status = 'active', updated_at = datetime('now') \
                 WHERE username = ? AND status = 'pending'",
                [username],
            )? > 0)
        })
    }

    /// Any status -> `disabled` (a deleted account too). Tokens stay but stop resolving.
    pub fn disable_user(&self, username: &str) -> Result<bool> {
        self.disable_user_with(username, KeepAdmin::Allow)
    }

    /// [`disable_user`](Self::disable_user), refusing the last active admin with
    /// [`KeepAdmin::Keep`].
    pub fn disable_user_with(&self, username: &str, keep: KeepAdmin) -> Result<bool> {
        let _bump = self.bump_users_version();
        self.write(|conn| {
            guard_last_admin(conn, username, keep)?;
            Ok(conn.execute(
                "UPDATE users SET status = 'disabled', updated_at = datetime('now') \
                 WHERE username = ?",
                [username],
            )? > 0)
        })
    }

    /// Soft delete; the account's tokens are always deleted. `true` only if the account
    /// existed and was not already deleted.
    pub fn delete_user(&self, username: &str) -> Result<bool> {
        self.delete_user_with(username, KeepAdmin::Allow)
    }

    /// [`delete_user`](Self::delete_user), refusing the last active admin with
    /// [`KeepAdmin::Keep`].
    pub fn delete_user_with(&self, username: &str, keep: KeepAdmin) -> Result<bool> {
        let _bump = self.bump_users_version();
        self.write(|conn| {
            guard_last_admin(conn, username, keep)?;
            let changed = conn.execute(
                "UPDATE users SET status = 'deleted', updated_at = datetime('now') \
                 WHERE username = ? AND status != 'deleted'",
                [username],
            )? > 0;
            conn.execute("DELETE FROM auth_tokens WHERE username = ?", [username])?;
            Ok(changed)
        })
    }

    /// Bring a soft-deleted account back with a new password (and `role`, if given).
    /// `false` if `username` is not a deleted account.
    pub fn restore_user(&self, username: &str, password: &str, role: Option<Role>) -> Result<bool> {
        let _bump = self.bump_users_version();
        validation(validate_password(password))?;
        let password_hash = hash_password(password, self.bcrypt_cost)?;
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE users SET status = 'active', password_hash = ?, \
                 role = COALESCE(?, role), updated_at = datetime('now') \
                 WHERE username = ? AND status = 'deleted'",
                params![password_hash, role.map(Role::as_str), username],
            )? > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_db;

    /// Regression (review finding): the admin API could delete, disable or demote the only
    /// active admin, leaving nobody able to administer the server.
    #[test]
    fn the_last_active_admin_is_kept_when_asked() {
        let (_dir, db) = temp_db();
        let mk = |u: &str, role| {
            db.create_user(u, "password123", role, UserStatus::Active, "")
                .unwrap()
        };
        mk("root", Role::Admin);
        mk("bob", Role::Editor);
        let last =
            |r: Result<bool>| matches!(r, Err(DbError::Conflict(m)) if m == LAST_ADMIN_MESSAGE);
        assert!(last(db.delete_user_with("root", KeepAdmin::Keep)));
        assert!(last(db.disable_user_with("root", KeepAdmin::Keep)));
        assert!(last(db.update_user_role_with(
            "root",
            Role::Editor,
            KeepAdmin::Keep
        )));
        // Nothing changed.
        let root = db.get_user("root").unwrap().unwrap();
        assert_eq!((root.role, root.status), (Role::Admin, UserStatus::Active));
        // Re-granting admin, and changes to other accounts, are fine.
        assert!(
            db.update_user_role_with("root", Role::Admin, KeepAdmin::Keep)
                .unwrap()
        );
        assert!(db.disable_user_with("bob", KeepAdmin::Keep).unwrap());
        assert!(
            db.delete_user_with("ghost", KeepAdmin::Keep)
                .is_ok_and(|d| !d)
        );
        // An inactive admin does not count as the other one.
        mk("old", Role::Admin);
        db.disable_user("old").unwrap();
        assert!(last(db.delete_user_with("root", KeepAdmin::Keep)));
        // A second active admin: the first may go.
        mk("root2", Role::Admin);
        assert!(
            db.update_user_role_with("root", Role::Editor, KeepAdmin::Keep)
                .unwrap()
        );
        assert!(last(db.delete_user_with("root2", KeepAdmin::Keep)));
        // The CLI's Allow keeps 0.5.2's behaviour.
        assert!(db.delete_user("root2").unwrap());
    }

    /// Regression (review finding): an unknown or inactive account answered without any
    /// bcrypt work, so response timing told which usernames exist. One check always runs.
    #[test]
    fn unknown_accounts_cost_one_bcrypt_check() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with(
            dir.path().join("mokuro.db"),
            &crate::DbOptions {
                bcrypt_cost: 8,
                ..crate::DbOptions::default()
            },
        )
        .unwrap();
        db.create_user(
            "alice",
            "password123",
            Role::Uploader,
            UserStatus::Active,
            "",
        )
        .unwrap();
        db.create_user(
            "gone",
            "password123",
            Role::Uploader,
            UserStatus::Active,
            "",
        )
        .unwrap();
        db.disable_user("gone").unwrap();
        let time = |u: &str| {
            (0..3)
                .map(|_| {
                    let t = std::time::Instant::now();
                    assert!(db.authenticate_user(u, "wrong-password").unwrap().is_none());
                    t.elapsed()
                })
                .min()
                .unwrap()
        };
        let _ = time("nobody"); // the dummy hash is made on first use
        let known = time("alice");
        for who in ["nobody", "gone"] {
            let t = time(who);
            assert!(t * 3 >= known, "{who}: {t:?} vs a real check {known:?}");
        }
        assert!(
            db.authenticate_user("alice", "password123")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn create_and_get() {
        let (_dir, db) = temp_db();
        let id = db
            .create_user(
                "alice",
                "password123",
                Role::Uploader,
                UserStatus::Active,
                "hi",
            )
            .unwrap();
        let user = db.get_user("alice").unwrap().unwrap();
        assert_eq!(user.id, id);
        assert_eq!(user.role, Role::Uploader);
        assert_eq!(user.status, UserStatus::Active);
        assert_eq!(user.notes, "hi");
        assert_eq!(user.created_at.len(), 19);
        let json = serde_json::to_string(&user).unwrap();
        assert!(json.starts_with(r#"{"id":1,"username":"alice","role":"uploader","status":"active","notes":"hi","created_at":""#));
        assert!(db.get_user("nobody").unwrap().is_none());
        assert!(db.get_user("Alice").unwrap().is_none(), "case-sensitive");
    }

    #[test]
    fn create_errors_carry_python_messages() {
        let (_dir, db) = temp_db();
        let err = |r: Result<i64>| r.unwrap_err().to_string();
        let mk = |u: &str, p: &str| db.create_user(u, p, Role::Registered, UserStatus::Active, "");
        assert_eq!(err(mk("", "password123")), "Username is required");
        assert_eq!(err(mk("   ", "password123")), "Username is required");
        assert_eq!(
            err(mk("a b", "password123")),
            "Username must be 3-32 characters and contain only letters, numbers, underscores, and hyphens"
        );
        assert_eq!(err(mk("alice", "")), "Password is required");
        assert_eq!(
            err(mk("alice", "short")),
            "Password must be at least 8 characters"
        );
        mk("alice", "password123").unwrap();
        let dup = mk("alice", "password123").unwrap_err();
        assert!(matches!(dup, DbError::Conflict(_)));
        assert_eq!(dup.to_string(), "Username 'alice' already exists");
        db.delete_user("alice").unwrap();
        let gone = mk("alice", "password123").unwrap_err();
        assert!(matches!(gone, DbError::Invalid(_)));
        assert_eq!(
            gone.to_string(),
            "Username 'alice' belongs to a deleted account; bring it back with: mokuro-bunko admin restore-user alice"
        );
        assert_eq!(
            normalize_role("boss").unwrap_err().to_string(),
            "Invalid role: boss. Must be one of: ['admin', 'anonymous', 'editor', 'inviter', 'processor', 'registered', 'uploader']"
        );
        assert_eq!(normalize_role("writer").unwrap(), Role::Uploader);
    }

    #[test]
    fn authentication_rules() {
        let (_dir, db) = temp_db();
        db.create_user("alice", "password123", Role::Admin, UserStatus::Active, "")
            .unwrap();
        db.create_user(
            "pend",
            "password123",
            Role::Registered,
            UserStatus::Pending,
            "",
        )
        .unwrap();
        assert!(
            db.authenticate_user("alice", "password123")
                .unwrap()
                .is_some()
        );
        assert!(
            db.authenticate_user("alice", "password124")
                .unwrap()
                .is_none()
        );
        assert!(
            db.authenticate_user("nobody", "password123")
                .unwrap()
                .is_none()
        );
        assert!(
            db.authenticate_user("pend", "password123")
                .unwrap()
                .is_none()
        );
        assert!(db.approve_user("pend").unwrap());
        assert!(!db.approve_user("pend").unwrap(), "only pending accounts");
        assert!(
            db.authenticate_user("pend", "password123")
                .unwrap()
                .is_some()
        );
        assert!(db.disable_user("pend").unwrap());
        assert!(
            db.authenticate_user("pend", "password123")
                .unwrap()
                .is_none()
        );
        assert!(!db.disable_user("nobody").unwrap());
    }

    #[test]
    fn delete_restore_and_password() {
        let (_dir, db) = temp_db();
        db.create_user("bob", "password123", Role::Editor, UserStatus::Active, "")
            .unwrap();
        assert!(db.delete_user("bob").unwrap());
        assert!(!db.delete_user("bob").unwrap());
        assert!(!db.delete_user("nobody").unwrap());
        assert_eq!(
            db.get_user("bob").unwrap().unwrap().status,
            UserStatus::Deleted
        );
        assert!(
            db.authenticate_user("bob", "password123")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.restore_user("bob", "short", None)
                .unwrap_err()
                .to_string(),
            "Password must be at least 8 characters"
        );
        assert!(db.restore_user("bob", "newpassword1", None).unwrap());
        assert!(
            !db.restore_user("bob", "newpassword1", None).unwrap(),
            "not deleted any more"
        );
        let bob = db
            .authenticate_user("bob", "newpassword1")
            .unwrap()
            .unwrap();
        assert_eq!(bob.role, Role::Editor, "keeps its role");
        db.delete_user("bob").unwrap();
        db.restore_user("bob", "newpassword2", Some(Role::Registered))
            .unwrap();
        assert_eq!(db.get_user("bob").unwrap().unwrap().role, Role::Registered);
        assert!(db.update_user_password("bob", "another-pass").unwrap());
        assert!(!db.update_user_password("nobody", "another-pass").unwrap());
        assert!(
            db.authenticate_user("bob", "another-pass")
                .unwrap()
                .is_some()
        );
        assert!(db.update_user_password("bob", "x").is_err());
    }

    #[test]
    fn role_notes_and_listing() {
        let (_dir, db) = temp_db();
        for (u, st) in [
            ("aaa", UserStatus::Active),
            ("bbb", UserStatus::Pending),
            ("ccc", UserStatus::Active),
        ] {
            db.create_user(u, "password123", Role::Registered, st, "")
                .unwrap();
        }
        assert!(db.update_user_role("aaa", Role::Inviter).unwrap());
        assert!(!db.update_user_role("zzz", Role::Inviter).unwrap());
        assert!(db.update_user_notes("aaa", "note").unwrap());
        assert!(!db.update_user_notes("zzz", "note").unwrap());
        let all: Vec<_> = db
            .list_users(None)
            .unwrap()
            .into_iter()
            .map(|u| u.username)
            .collect();
        assert_eq!(all, ["ccc", "bbb", "aaa"], "same second: newest id first");
        let pending = db.list_users(Some(UserStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].username, "bbb");
    }

    #[test]
    fn users_version_moves_on_account_changes_even_failed_ones() {
        let (_dir, db) = temp_db();
        let v0 = db.users_version();
        db.create_user("alice", "password123", Role::Admin, UserStatus::Active, "")
            .unwrap();
        assert_eq!(db.users_version(), v0 + 1);
        let _ = db.create_user("alice", "password123", Role::Admin, UserStatus::Active, "");
        assert_eq!(db.users_version(), v0 + 2, "a failed create still bumps");
        db.update_user_notes("alice", "x").unwrap();
        assert_eq!(db.users_version(), v0 + 2, "notes do not bump");
        db.update_user_role("alice", Role::Editor).unwrap();
        let _ = db.update_user_password("alice", "short");
        db.approve_user("alice").unwrap();
        db.disable_user("alice").unwrap();
        db.delete_user("alice").unwrap();
        db.restore_user("alice", "password123", None).unwrap();
        assert_eq!(db.users_version(), v0 + 8);
    }

    #[test]
    fn processor_stamp() {
        let (_dir, db) = temp_db();
        db.create_user(
            "proc",
            "password123",
            Role::Processor,
            UserStatus::Active,
            "",
        )
        .unwrap();
        db.create_user("human", "password123", Role::Admin, UserStatus::Active, "")
            .unwrap();
        let s1 = db.processor_account_stamp("proc").unwrap().unwrap();
        assert_eq!(s1.len(), 32);
        assert!(db.processor_account_stamp("human").unwrap().is_none());
        assert!(db.processor_account_stamp("nobody").unwrap().is_none());
        db.update_user_password("proc", "password456").unwrap();
        let s2 = db.processor_account_stamp("proc").unwrap().unwrap();
        assert_ne!(s1, s2);
        db.disable_user("proc").unwrap();
        assert!(db.processor_account_stamp("proc").unwrap().is_none());
    }

    #[test]
    fn corrupt_role_degrades_gracefully() {
        let (_dir, db) = temp_db();
        db.create_user("alice", "password123", Role::Admin, UserStatus::Active, "")
            .unwrap();
        db.create_user("bob", "password123", Role::Admin, UserStatus::Active, "")
            .unwrap();
        db.with_writer_connection(|c| {
            c.execute("UPDATE users SET role = 'boss' WHERE username = 'bob'", [])
        })
        .unwrap();
        assert!(matches!(
            db.get_user("bob"),
            Err(DbError::CorruptRow { .. })
        ));
        assert!(
            db.authenticate_user("bob", "password123")
                .unwrap()
                .is_none()
        );
        let names: Vec<_> = db
            .list_users(None)
            .unwrap()
            .into_iter()
            .map(|u| u.username)
            .collect();
        assert_eq!(names, ["alice"]);
        assert!(db.update_user_role("bob", Role::Registered).unwrap());
        assert!(
            db.authenticate_user("bob", "password123")
                .unwrap()
                .is_some()
        );
    }
}
