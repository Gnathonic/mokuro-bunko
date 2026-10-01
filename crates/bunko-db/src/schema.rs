//! Schema creation and the 0.5.2 migration steps (spec §2), run on every open.
//!
//! The DDL strings are 0.5.2's byte for byte, indentation included: SQLite keeps the
//! statement text in `sqlite_master.sql`, so a database created here is
//! indistinguishable from one Python created (pinned by the golden schema test).
//!
//! Every step is idempotent (`IF NOT EXISTS`, `PRAGMA table_info` probes), as in 0.5.2;
//! nothing is gated on `schema_version`, which is bookkeeping only. Differences:
//! - the whole run is ONE transaction (Python autocommitted DDL statement by statement),
//!   so an interrupted upgrade leaves the file as it was;
//! - `schema_version` is set to 6 unless it already holds a larger number (spec open
//!   question 2: never downgrade a newer build's marker; Python rewrites it to 6).

use crate::database::column_exists;
use crate::error::Result;
use crate::identities::identity_from_entry;
use crate::series::load_json_object;
use rusqlite::{Connection, OptionalExtension, params};

/// `Database.SCHEMA_VERSION` of 0.5.2.
pub const SCHEMA_VERSION: i64 = 6;

const SCHEMA_VERSION_DDL: &str = "
                CREATE TABLE IF NOT EXISTS schema_version (
                    version INTEGER PRIMARY KEY
                )
            ";

const USERS_DDL: &str = "
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
            ";

const INVITES_DDL: &str = "
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
            ";

const AUDIT_LOGS_DDL: &str = "
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
            ";

const VOLUME_UPLOADS_DDL: &str = "
                CREATE TABLE IF NOT EXISTS volume_uploads (
                    volume_key TEXT PRIMARY KEY,
                    uploader_username TEXT NOT NULL,
                    uploaded_at TEXT NOT NULL DEFAULT (datetime('now')),
                    last_modified_by TEXT,
                    last_modified_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            ";

/// `spine_offset` is NUMERIC on purpose: an integer nudge (`-40`) stays an integer.
const SERIES_FACTS_DDL: &str = "
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
            ";

const SERIES_ENTRY_CACHE_DDL: &str = "
                CREATE TABLE IF NOT EXISTS series_entry_cache (
                    volume_key TEXT PRIMARY KEY,
                    series_key TEXT NOT NULL,
                    entry_json TEXT NOT NULL,
                    cbz_size INTEGER NOT NULL,
                    cbz_mtime REAL NOT NULL,
                    sidecar_key TEXT NOT NULL DEFAULT '',
                    computed_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            ";

const COMMUNITY_DETAILS_DDL: &str = "
                CREATE TABLE IF NOT EXISTS community_details (
                    series_key TEXT PRIMARY KEY,
                    score REAL,
                    tags TEXT NOT NULL DEFAULT '[]',
                    genres TEXT NOT NULL DEFAULT '[]',
                    source TEXT NOT NULL,
                    fetched_at TEXT NOT NULL
                )
            ";

const CATALOG_SERIES_DDL: &str = "
                CREATE TABLE IF NOT EXISTS catalog_series (
                    series_key TEXT PRIMARY KEY,
                    folder_name TEXT NOT NULL,
                    cover_path TEXT,
                    volume_count INTEGER NOT NULL,
                    latest_volume_modified REAL NOT NULL DEFAULT 0,
                    total_pages INTEGER NOT NULL DEFAULT 0,
                    total_chars INTEGER NOT NULL DEFAULT 0,
                    missing_pages INTEGER NOT NULL DEFAULT 0,
                    damaged_volumes INTEGER NOT NULL DEFAULT 0,
                    scanned_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            ";

const AUTH_TOKENS_DDL: &str = "
                CREATE TABLE IF NOT EXISTS auth_tokens (
                    token_hash TEXT PRIMARY KEY,
                    username TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    label TEXT NOT NULL DEFAULT '',
                    created_at REAL NOT NULL,
                    expires_at REAL NOT NULL,
                    last_used_at REAL NOT NULL
                )
            ";

const AUTH_TOKENS_INDEX: &str = "
                CREATE INDEX IF NOT EXISTS idx_auth_tokens_username
                ON auth_tokens(username)
            ";

const OCR_SIDECARS_DDL: &str = "
                CREATE TABLE IF NOT EXISTS ocr_sidecars (
                    sidecar_path TEXT PRIMARY KEY,
                    volume_key TEXT NOT NULL,
                    generation_id TEXT NOT NULL,
                    generation_name TEXT NOT NULL,
                    machine TEXT NOT NULL,
                    account TEXT,
                    engine TEXT,
                    detector TEXT,
                    precision TEXT,
                    runner_build TEXT,
                    pages INTEGER,
                    failed_pages INTEGER,
                    archive_size INTEGER,
                    archive_mtime_ns INTEGER,
                    written_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            ";

const OCR_SIDECARS_INDEX: &str = "
                CREATE INDEX IF NOT EXISTS idx_ocr_sidecars_volume
                ON ocr_sidecars(volume_key)
            ";

const VOLUME_IDENTITIES_DDL: &str = "
                CREATE TABLE IF NOT EXISTS volume_identities (
                    volume_key TEXT PRIMARY KEY,
                    volume_uuid TEXT NOT NULL,
                    recorded_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
            ";

/// Step 6 indexes, in 0.5.2's order. `idx_audit_created_at` is named in an `INDEXED BY`
/// hint by the audit query, so it must exist under exactly this name.
const INDEXES: &[&str] = &[
    "
                CREATE INDEX IF NOT EXISTS idx_users_username
                ON users(username)
            ",
    "
                CREATE INDEX IF NOT EXISTS idx_invites_code
                ON invites(code)
            ",
    "
                CREATE INDEX IF NOT EXISTS idx_audit_created_at
                ON audit_logs(created_at DESC)
            ",
    "
                CREATE INDEX IF NOT EXISTS idx_audit_actor
                ON audit_logs(actor_username)
            ",
    "
                CREATE INDEX IF NOT EXISTS idx_audit_type_created
                ON audit_logs(target_type, created_at)
            ",
    "
                CREATE INDEX IF NOT EXISTS idx_audit_action_created
                ON audit_logs(action, created_at)
            ",
    "
                CREATE INDEX IF NOT EXISTS idx_audit_actor_created
                ON audit_logs(actor_username, created_at)
            ",
    "
                CREATE INDEX IF NOT EXISTS idx_volume_uploads_uploader
                ON volume_uploads(uploader_username)
            ",
    "
                CREATE INDEX IF NOT EXISTS idx_series_entry_cache_series
                ON series_entry_cache(series_key)
            ",
];

/// 0.5.2 `_init_schema`, step for step (spec §2.4). Runs inside the caller's write
/// transaction.
pub(crate) fn init(conn: &Connection) -> Result<()> {
    // 1. Tables up to and including ocr_sidecars.
    for ddl in [
        SCHEMA_VERSION_DDL,
        USERS_DDL,
        INVITES_DDL,
        AUDIT_LOGS_DDL,
        VOLUME_UPLOADS_DDL,
        SERIES_FACTS_DDL,
        SERIES_ENTRY_CACHE_DDL,
        COMMUNITY_DETAILS_DDL,
        CATALOG_SERIES_DDL,
        AUTH_TOKENS_DDL,
        AUTH_TOKENS_INDEX,
        OCR_SIDECARS_DDL,
        OCR_SIDECARS_INDEX,
    ] {
        conn.execute(ddl, [])?;
    }

    // 2. volume_identities, with a one-time backfill for a database that existed before
    //    the table did.
    let version_row: Option<i64> = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .optional()?;
    let had_identities = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'volume_identities'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    conn.execute(VOLUME_IDENTITIES_DDL, [])?;
    if version_row.is_some() && !had_identities {
        backfill_volume_identities(conn)?;
    }

    // 3-5. Columns older databases lack.
    for column in ["missing_pages", "damaged_volumes"] {
        if !column_exists(conn, "catalog_series", column)? {
            conn.execute(
                &format!(
                    "ALTER TABLE catalog_series ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"
                ),
                [],
            )?;
        }
    }
    if !column_exists(conn, "users", "notes")? {
        conn.execute(
            "ALTER TABLE users ADD COLUMN notes TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    if !column_exists(conn, "invites", "invited_by")? {
        conn.execute("ALTER TABLE invites ADD COLUMN invited_by TEXT", [])?;
    }

    // 6. Indexes.
    for ddl in INDEXES {
        conn.execute(ddl, [])?;
    }

    // 7. Role rename (writer -> uploader).
    conn.execute(
        "UPDATE users SET role = 'uploader' WHERE role = 'writer'",
        [],
    )?;
    conn.execute(
        "UPDATE invites SET role = 'uploader' WHERE role = 'writer'",
        [],
    )?;

    // 8. Version bookkeeping.
    let current: Option<i64> = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .optional()?;
    match current {
        None => {
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?)",
                params![SCHEMA_VERSION],
            )?;
        }
        Some(v) if v > SCHEMA_VERSION => {
            tracing::warn!(
                found = v,
                ours = SCHEMA_VERSION,
                "mokuro.db is marked by a newer build; leaving its schema_version alone"
            );
        }
        Some(_) => {
            conn.execute(
                "UPDATE schema_version SET version = ?",
                params![SCHEMA_VERSION],
            )?;
        }
    }
    Ok(())
}

fn backfill_volume_identities(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("SELECT volume_key, entry_json FROM series_entry_cache")?;
    let rows: Vec<(String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, crate::series::opt_text(r, 1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (volume_key, entry_json) in rows {
        let entry = load_json_object(entry_json.as_deref());
        if let Some(uuid) = identity_from_entry(&entry) {
            conn.execute(
                "INSERT OR IGNORE INTO volume_identities (volume_key, volume_uuid) VALUES (?, ?)",
                params![volume_key, uuid],
            )?;
        }
    }
    Ok(())
}
