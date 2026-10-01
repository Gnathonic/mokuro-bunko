//! `mokuro.db`: the SQLite database of mokuro-bunko, a drop-in over 0.5.2 files.
//!
//! The same file keeps working in both directions: this crate opens any 0.5.2 (or older)
//! database and upgrades it with 0.5.2's own idempotent steps, and a database it created
//! or touched opens in Python 0.5.2 (a rollback) with identical schema text, row formats
//! and JSON spellings. `tests/golden/` holds the Python-made fixtures and the scripts that
//! check both directions.
//!
//! Everything is synchronous. [`Database`] is `Send + Sync`; share it as
//! `Arc<Database>` and call it from `spawn_blocking`. See [`database`] for the connection
//! model (one writer + a small read pool, WAL, lock retry) and each module's doc for the
//! 0.5.2 behaviour it reproduces and the few quirks it fixes:
//!
//! | module | tables |
//! |---|---|
//! | [`users`] | `users` (+ `users_version`) |
//! | [`tokens`] | `auth_tokens` |
//! | [`invites`] | `invites` |
//! | [`audit`] | `audit_logs` |
//! | [`uploads`] | `volume_uploads` (volume and series ownership) |
//! | [`ocr`] | `ocr_sidecars` |
//! | [`identities`] | `volume_identities` |
//! | [`series`] | `series_facts`, `series_entry_cache`, `catalog_series`, `community_details` |
//! | [`schema`] | DDL, migrations, `schema_version` |

pub mod audit;
pub mod database;
pub mod error;
pub mod identities;
pub mod invites;
pub mod ocr;
pub mod pyfmt;
pub mod pytime;
pub mod schema;
pub mod series;
pub mod tokens;
pub mod uploads;
pub mod users;
pub mod validation;

pub use audit::{
    AUDIT_PAGE_MAX, AUDIT_PAGE_SIZE, AUDIT_PROGRESS_TYPE, AUDIT_RETENTION_DAYS, AuditDetails,
    AuditEvent, AuditFacets, AuditPage, AuditQuery, NewAuditEvent,
};
pub use database::{Database, DbOptions};
pub use error::{DbError, Result};
pub use invites::{
    INVALID_INVITE_MESSAGE, INVITABLE_ROLES, Invite, InviteInfo, InviteStatus, parse_duration,
};
pub use ocr::OcrSidecar;
pub use schema::SCHEMA_VERSION;
pub use series::{CatalogSeries, CommunityDetails, SeriesFacts};
pub use tokens::{TOKEN_TOUCH_SECONDS, TokenKind, token_hash};
pub use uploads::{fold_series_title_key, layer_sidecar_volume_path, normalize_volume_key};
pub use users::{KeepAdmin, LAST_ADMIN_MESSAGE, User, UserStatus, normalize_role};
pub use validation::{validate_password, validate_username};

#[cfg(test)]
pub(crate) mod testutil {
    use crate::{Database, DbOptions};

    /// A fresh database in a temp dir, with a cheap bcrypt cost.
    pub fn temp_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::open_with(
            dir.path().join("mokuro.db"),
            &DbOptions {
                bcrypt_cost: 4,
                ..DbOptions::default()
            },
        )
        .expect("open");
        (dir, db)
    }
}
