//! The [`Database`] handle: connections, pragmas, lock retry and transactions.
//!
//! # Concurrency design
//!
//! 0.5.2 serialised every call — reads included — through one connection and one mutex,
//! and held that mutex across bcrypt. Here:
//!
//! - **one writer connection** behind a mutex. Every write runs in a `BEGIN IMMEDIATE`
//!   transaction, so a method's read-modify-write is atomic against other threads AND
//!   other processes (Python's implicit deferred transactions were atomic only within the
//!   process), and a write never hits `SQLITE_BUSY_SNAPSHOT` half-way through (which no
//!   amount of retrying a statement fixes);
//! - **a small pool of read connections** (default 2, `PRAGMA query_only`), which WAL lets
//!   run alongside the writer and each other. With `read_connections = 0` reads share the
//!   writer connection (the 0.5.2 model);
//! - bcrypt runs outside every lock.
//!
//! All methods take `&self`; `Database` is `Send + Sync`, meant to be shared as
//! `Arc<Database>` and called from `spawn_blocking`.
//!
//! # Lock retry (spec §1.4)
//!
//! 0.5.2 retried each statement and the `COMMIT` up to `lock_retries` attempts while
//! SQLite said "database is locked", sleeping `delay`, `2*delay`, ... between attempts, on
//! top of `busy_timeout`. Here the same schedule wraps `BEGIN IMMEDIATE` (the only point a
//! write can meet another writer's lock once the write lock is held), `COMMIT`, and every
//! read (a read closure has no effects, so re-running it whole is safe). Only
//! `SQLITE_BUSY` is retried, not `SQLITE_LOCKED` — the same "database is locked" test.
//!
//! # Memory
//!
//! `cache_size_kib` (default 8 MiB) is a budget split across all connections, so adding
//! readers does not multiply the page cache. `mmap` stays off (SQLite's default).

use crate::error::{DbError, Result};
use crate::schema;
use bunko_core::config::DatabaseConfig;
use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, ErrorCode, OpenFlags};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How a [`Database`] is opened. Built from the `database:` config section with
/// [`DbOptions::from`]; the fields 0.5.2 lacked have defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct DbOptions {
    /// `PRAGMA busy_timeout` (clamped to >= 100, as `configure_connection`).
    pub busy_timeout_ms: u32,
    /// Attempts per lock-sensitive step (clamped to >= 1).
    pub lock_retries: u32,
    /// First back-off sleep, doubled after each failed attempt (clamped to >= 0.001).
    pub retry_initial_delay_seconds: f64,
    /// Page-cache budget for all connections together, KiB.
    pub cache_size_kib: u32,
    /// Read-only connections in the pool; 0 = reads use the writer connection.
    pub read_connections: usize,
    /// bcrypt cost for new hashes. 0.5.2 uses 12; lower it only in tests.
    pub bcrypt_cost: u32,
}

impl Default for DbOptions {
    fn default() -> Self {
        DbOptions {
            busy_timeout_ms: 5000,
            lock_retries: 5,
            retry_initial_delay_seconds: 0.05,
            cache_size_kib: 8192,
            read_connections: 2,
            bcrypt_cost: crate::validation::BCRYPT_COST,
        }
    }
}

impl From<&DatabaseConfig> for DbOptions {
    fn from(cfg: &DatabaseConfig) -> Self {
        DbOptions {
            busy_timeout_ms: cfg.busy_timeout_ms,
            lock_retries: cfg.lock_retries,
            retry_initial_delay_seconds: cfg.retry_initial_delay_seconds,
            ..DbOptions::default()
        }
    }
}

struct Reader {
    conn: Connection,
    busy_timeout_ms: u32,
}

/// A `mokuro.db` handle. See the module docs for the connection model.
pub struct Database {
    path: PathBuf,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Reader>>,
    reader_available: Condvar,
    reader_count: usize,
    busy_timeout_ms: AtomicU32,
    lock_retries: AtomicU32,
    retry_delay_bits: AtomicU64,
    pub(crate) bcrypt_cost: u32,
    pub(crate) users_version: AtomicU64,
    pub(crate) last_audit_prune: Mutex<Option<Instant>>,
    /// See `users.rs` `dummy_hash`: made on the first failed lookup.
    pub(crate) dummy_hash: std::sync::OnceLock<String>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("path", &self.path)
            .field("read_connections", &self.reader_count)
            .finish_non_exhaustive()
    }
}

pub(crate) fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(err, _) if err.code == ErrorCode::DatabaseBusy)
}

fn open_connection(path: &Path, busy_timeout_ms: u32, cache_kib: u32) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(Duration::from_millis(u64::from(busy_timeout_ms)))?;
    conn.pragma_update(None, "cache_size", -i64::from(cache_kib))?;
    Ok(conn)
}

impl Database {
    /// Open (creating if needed) the database at `path` with default options.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        Self::open_with(path, &DbOptions::default())
    }

    /// Open (creating if needed) the database at `path`, bring its schema up to date
    /// (every 0.5.2 migration step, idempotently) and open the read pool.
    pub fn open_with(path: impl AsRef<Path>, options: &DbOptions) -> Result<Database> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let busy = options.busy_timeout_ms.max(100);
        let per_conn_kib =
            (options.cache_size_kib / (1 + options.read_connections as u32)).max(256);
        let writer = open_connection(&path, busy, per_conn_kib)?;
        let db = Database {
            path,
            writer: Mutex::new(writer),
            readers: Mutex::new(Vec::new()),
            reader_available: Condvar::new(),
            reader_count: options.read_connections,
            busy_timeout_ms: AtomicU32::new(busy),
            lock_retries: AtomicU32::new(options.lock_retries.max(1)),
            retry_delay_bits: AtomicU64::new(
                options.retry_initial_delay_seconds.max(0.001).to_bits(),
            ),
            bcrypt_cost: options.bcrypt_cost.clamp(4, 31),
            users_version: AtomicU64::new(0),
            last_audit_prune: Mutex::new(None),
            dummy_hash: std::sync::OnceLock::new(),
        };
        {
            let writer = db.writer.lock();
            db.retry(|| {
                writer
                    .pragma_update_and_check(None, "journal_mode", "WAL", |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(DbError::from)
            })?;
        }
        db.write(schema::init)?;
        let mut readers = Vec::with_capacity(options.read_connections);
        for _ in 0..options.read_connections {
            let conn = open_connection(&db.path, busy, per_conn_kib)?;
            conn.pragma_update(None, "query_only", true)?;
            readers.push(Reader {
                conn,
                busy_timeout_ms: busy,
            });
        }
        *db.readers.lock() = readers;
        Ok(db)
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 0.5.2 `configure_connection`: apply runtime tuning, clamped to sane minimums
    /// (busy timeout >= 100 ms, retries >= 1, delay >= 0.001 s). `None` leaves a value.
    pub fn configure(
        &self,
        busy_timeout_ms: Option<u32>,
        lock_retries: Option<u32>,
        retry_initial_delay_seconds: Option<f64>,
    ) -> Result<()> {
        if let Some(ms) = busy_timeout_ms {
            let ms = ms.max(100);
            self.busy_timeout_ms.store(ms, Ordering::SeqCst);
            // Readers pick the new value up at their next checkout.
            self.writer
                .lock()
                .busy_timeout(Duration::from_millis(u64::from(ms)))?;
        }
        if let Some(n) = lock_retries {
            self.lock_retries.store(n.max(1), Ordering::SeqCst);
        }
        if let Some(d) = retry_initial_delay_seconds {
            let d = if d.is_nan() { 0.001 } else { d.max(0.001) };
            self.retry_delay_bits.store(d.to_bits(), Ordering::SeqCst);
        }
        Ok(())
    }

    /// [`Database::configure`] from the `database:` config section (what `create_app`
    /// does for the server's and the OCR worker's handles).
    pub fn apply_config(&self, cfg: &DatabaseConfig) -> Result<()> {
        self.configure(
            Some(cfg.busy_timeout_ms),
            Some(cfg.lock_retries),
            Some(cfg.retry_initial_delay_seconds),
        )
    }

    pub fn busy_timeout_ms(&self) -> u32 {
        self.busy_timeout_ms.load(Ordering::SeqCst)
    }

    pub fn lock_retries(&self) -> u32 {
        self.lock_retries.load(Ordering::SeqCst)
    }

    pub fn retry_initial_delay_seconds(&self) -> f64 {
        f64::from_bits(self.retry_delay_bits.load(Ordering::SeqCst))
    }

    /// The busy timeout the writer connection actually has (`PRAGMA busy_timeout`).
    pub fn live_busy_timeout_ms(&self) -> Result<i64> {
        Ok(self
            .writer
            .lock()
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))?)
    }

    /// Run `op` up to `lock_retries` times while it fails with "database is locked",
    /// sleeping `delay`, `2*delay`, ... in between (0.5.2 `_RetryingConnection.execute`).
    pub(crate) fn retry<T>(&self, mut op: impl FnMut() -> Result<T>) -> Result<T> {
        let attempts = self.lock_retries();
        let mut delay = self.retry_initial_delay_seconds();
        let mut attempt = 0;
        loop {
            match op() {
                Err(e) if e.is_busy() && attempt + 1 < attempts => {
                    tracing::debug!(attempt, delay, "database is locked; retrying");
                    std::thread::sleep(Duration::from_secs_f64(delay));
                    delay *= 2.0;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// Run `f` in one write transaction (`BEGIN IMMEDIATE` ... `COMMIT`); any error rolls
    /// everything back (0.5.2 `_connection()` around a block that writes).
    pub(crate) fn write<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let conn = self.writer.lock();
        self.retry(|| conn.execute_batch("BEGIN IMMEDIATE").map_err(DbError::from))?;
        let outcome = f(&conn).and_then(|value| {
            self.retry(|| conn.execute_batch("COMMIT").map_err(DbError::from))
                .map(|()| value)
        });
        if outcome.is_err()
            && !conn.is_autocommit()
            && let Err(e) = conn.execute_batch("ROLLBACK")
        {
            tracing::warn!(error = %e, "rollback failed");
        }
        outcome
    }

    /// Run the read-only closure `f` on a pooled read connection (or the writer when the
    /// pool is empty), retried whole while the database is locked.
    pub(crate) fn read<T>(&self, mut f: impl FnMut(&Connection) -> Result<T>) -> Result<T> {
        if self.reader_count == 0 {
            let conn = self.writer.lock();
            return self.retry(|| f(&conn));
        }
        let guard = self.checkout();
        let conn = &guard
            .reader
            .as_ref()
            .expect("reader present until drop")
            .conn; // invariant: Some until Drop
        self.retry(|| f(conn))
    }

    fn checkout(&self) -> ReaderGuard<'_> {
        let mut pool = self.readers.lock();
        let mut reader = loop {
            if let Some(r) = pool.pop() {
                break r;
            }
            self.reader_available.wait(&mut pool);
        };
        drop(pool);
        let want = self.busy_timeout_ms();
        if reader.busy_timeout_ms != want
            && reader
                .conn
                .busy_timeout(Duration::from_millis(u64::from(want)))
                .is_ok()
        {
            reader.busy_timeout_ms = want;
        }
        ReaderGuard {
            db: self,
            reader: Some(reader),
        }
    }

    /// Raw access to the writer connection, outside any transaction. For tests and
    /// one-off maintenance only.
    #[doc(hidden)]
    pub fn with_writer_connection<T>(&self, f: impl FnOnce(&Connection) -> T) -> T {
        f(&self.writer.lock())
    }
}

struct ReaderGuard<'a> {
    db: &'a Database,
    reader: Option<Reader>,
}

impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        if let Some(reader) = self.reader.take() {
            self.db.readers.lock().push(reader);
            self.db.reader_available.notify_one();
        }
    }
}

/// `PRAGMA table_info` column probe.
pub(crate) fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quick() -> DbOptions {
        DbOptions {
            bcrypt_cost: 4,
            ..DbOptions::default()
        }
    }

    #[test]
    fn database_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Database>();
    }

    #[test]
    fn creates_parent_dirs_and_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/mokuro.db");
        let db = Database::open_with(&path, &quick()).unwrap();
        assert!(path.exists());
        let mode: String = db
            .with_writer_connection(|c| c.query_row("PRAGMA journal_mode", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(db.live_busy_timeout_ms().unwrap(), 5000);
    }

    #[test]
    fn configure_clamps_and_applies() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with(dir.path().join("m.db"), &quick()).unwrap();
        db.configure(Some(1), Some(0), Some(0.0)).unwrap();
        assert_eq!(db.busy_timeout_ms(), 100);
        assert_eq!(db.lock_retries(), 1);
        assert_eq!(db.retry_initial_delay_seconds(), 0.001);
        db.configure(Some(250), None, None).unwrap();
        assert_eq!(db.live_busy_timeout_ms().unwrap(), 250);
        assert_eq!(db.lock_retries(), 1);
    }

    #[test]
    fn write_waits_out_an_external_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let db = Database::open_with(&path, &quick()).unwrap();
        db.configure(Some(100), Some(5), Some(0.05)).unwrap();
        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(250));
            other.execute_batch("COMMIT").unwrap();
        });
        // 100 ms busy timeout per attempt + 50/100/200 ms sleeps outlast the 250 ms lock.
        db.log_audit_event(&crate::NewAuditEvent::new("probe"))
            .unwrap();
        releaser.join().unwrap();
    }

    #[test]
    fn write_gives_up_after_the_retries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let db = Database::open_with(&path, &quick()).unwrap();
        db.configure(Some(100), Some(2), Some(0.01)).unwrap();
        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let err = db
            .log_audit_event(&crate::NewAuditEvent::new("probe"))
            .unwrap_err();
        assert!(err.is_busy(), "{err}");
        other.execute_batch("ROLLBACK").unwrap();
        // The handle is still usable afterwards.
        db.log_audit_event(&crate::NewAuditEvent::new("probe"))
            .unwrap();
    }

    #[test]
    fn readers_run_while_a_write_is_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.db");
        let db = Database::open_with(&path, &quick()).unwrap();
        db.create_user(
            "alice",
            "password123",
            bunko_core::Role::Admin,
            crate::UserStatus::Active,
            "",
        )
        .unwrap();
        let other = Connection::open(&path).unwrap();
        other
            .execute_batch("BEGIN IMMEDIATE; UPDATE users SET notes = 'x'")
            .unwrap();
        // WAL: the pending write does not block a reader, which sees the committed state.
        assert_eq!(db.get_user("alice").unwrap().unwrap().notes, "");
        other.execute_batch("COMMIT").unwrap();
        assert_eq!(db.get_user("alice").unwrap().unwrap().notes, "x");
    }

    #[test]
    fn single_connection_mode_works() {
        let dir = tempfile::tempdir().unwrap();
        let opts = DbOptions {
            read_connections: 0,
            ..quick()
        };
        let db = Database::open_with(dir.path().join("m.db"), &opts).unwrap();
        db.create_user(
            "alice",
            "password123",
            bunko_core::Role::Admin,
            crate::UserStatus::Active,
            "",
        )
        .unwrap();
        assert!(
            db.authenticate_user("alice", "password123")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn shared_across_threads() {
        let dir = tempfile::tempdir().unwrap();
        let db =
            std::sync::Arc::new(Database::open_with(dir.path().join("m.db"), &quick()).unwrap());
        db.create_user(
            "alice",
            "password123",
            bunko_core::Role::Admin,
            crate::UserStatus::Active,
            "",
        )
        .unwrap();
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let db = db.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        let (token, _) = db
                            .create_auth_token("alice", crate::TokenKind::Web, "", None)
                            .unwrap();
                        assert!(db.resolve_auth_token(&token).unwrap().is_some());
                        let path = format!("/t{t}/{i}");
                        db.log_audit_event(&crate::NewAuditEvent::new("upload").target_path(&path))
                            .unwrap();
                        db.query_audit_events(&crate::AuditQuery::default())
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let page = db
            .query_audit_events(&crate::AuditQuery::default())
            .unwrap();
        assert_eq!(page.total, Some(200));
    }
}
