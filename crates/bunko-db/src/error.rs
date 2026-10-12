//! The crate's error type.
//!
//! 0.5.2 signals every caller mistake with a `ValueError` whose message the HTTP layer
//! copies into `{"error": msg}` (and the admin API turns into a 409 when the lowercased
//! message contains "already exists"). [`DbError::Invalid`] and [`DbError::Conflict`]
//! carry those messages verbatim — `Display` is exactly the Python text — and the split
//! between them is the 400/409 split the admin API derives from the message.

use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T, E = DbError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// A caller-supplied value was refused (Python `ValueError`): validation, an unknown
    /// role, a bad duration, a deleted account's name, an unknown token kind...
    #[error("{0}")]
    Invalid(String),
    /// A uniqueness conflict: `Username '<u>' already exists`.
    #[error("{0}")]
    Conflict(String),
    /// An audit query argument that cannot be used (a date, a cursor). HTTP 400.
    #[error("{0}")]
    AuditQuery(String),
    /// A stored row holds a value this build cannot represent (e.g. an unknown role).
    /// 0.5.2 raised an unhandled exception (HTTP 500) for these.
    #[error("corrupt {table} row: {detail}")]
    CorruptRow { table: &'static str, detail: String },
    #[error("cannot create the database directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("password hashing failed: {0}")]
    Hash(#[from] bcrypt::BcryptError),
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

impl DbError {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        DbError::Invalid(msg.into())
    }

    /// True for SQLite's "database is locked" (`SQLITE_BUSY` and its extended codes).
    pub fn is_busy(&self) -> bool {
        matches!(self, DbError::Sqlite(e) if crate::database::is_busy(e))
    }
}
