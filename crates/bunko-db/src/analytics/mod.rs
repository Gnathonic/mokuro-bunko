//! `analytics.sqlite`: the server's usage analytics (docs/design/analytics.md §5).
//!
//! A sibling of [`crate::Database`] in its own file beside `mokuro.db`, so it never
//! contends with the main writer, can be deleted to reset analytics, and never bloats
//! the main database. The same shape: one writer connection behind a mutex plus a
//! small pool of `query_only` readers that WAL lets run beside it; every method is
//! synchronous (call it from `spawn_blocking`). `synchronous=NORMAL`: a crash may lose
//! the last transactions (at most one flush window, design §4.6), never the file.
//!
//! All DDL is created on open, idempotently, one area per module, so the area code
//! only adds statements against tables that already exist:
//!
//! | module | tables |
//! |---|---|
//! | `mod` | `meta` (salt, schema version, `tracking_since`, rollup mark, counters) |
//! | [`traffic`] | `traffic_*`, `latency_*`, `errors_*`, `principal_*`, `anon_ips`, `ip_daily`, `auth_failures*`, `slow_requests`, `volume_downloads`, `uploads_daily` |
//! | [`reading`] | `read_positions`, `reading_daily`, `history_cursor`, `reading_pages`, `volume_progress`, `devices` |
//! | [`library`] | `library_scan`, `library_daily`, `archive_pages` |
//! | [`host`] | `host_minute`, `host_hourly` |
//! | [`crawl`] | `crawl_episodes` |
//! | [`clients`] | `clients_daily`, `client_principals`, `client_mix_principals`, `origins_daily`, `geo_daily` |
//!
//! Buckets are UTC integers: `hour` = epoch seconds / 3600, `day` = epoch seconds /
//! 86400. An area's writes go through [`AnalyticsDb::write`], whose closure gets an
//! [`AnalyticsTx`]; each area module adds its statements as `impl AnalyticsTx` (writes)
//! and `impl AnalyticsDb` (queries) blocks in its own file.

pub mod clients;
pub mod crawl;
pub mod host;
pub mod library;
mod maintain;
pub mod reading;
pub mod summary;
pub mod traffic;

pub use maintain::{MaintainStats, Retention};

use crate::error::{DbError, Result};
use parking_lot::{Condvar, Mutex, RwLock};
use rand::RngCore;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What an anonymous id is keyed by: an IPv4 address, the `/64` of an IPv6 one, or a
/// trusted proxy's other text as-is.
pub fn anon_network(ip: &str) -> std::borrow::Cow<'_, str> {
    match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(v6)) if v6.to_ipv4_mapped().is_none() => {
            let net = std::net::Ipv6Addr::from(v6.to_bits() & !u128::from(u64::MAX));
            format!("{net}/64").into()
        }
        _ => ip.into(),
    }
}

/// `a:` + the first 12 hex digits of `sha256(salt || network)` (`ip` canonical; see
/// [`anon_network`]): an anonymous visitor's id (design §3), and what a crawl episode
/// keeps of its address once `retention.ip_days` is over.
pub fn anon_id(salt: &[u8; 32], ip: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(salt);
    h.update(anon_network(ip).as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(14);
    out.push_str("a:");
    out.push_str(&hex::encode(&digest[..6]));
    out
}

/// Is `s` what [`anon_id`] makes (`a:` and 12 lowercase hex digits)?
pub fn is_anon_id(s: &str) -> bool {
    s.len() == 14
        && s.starts_with("a:")
        && s.bytes()
            .skip(2)
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// SQLite `GLOB` matching exactly what [`anon_id`] makes.
pub(crate) const ANON_ID_GLOB: &str = "a:[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]";

/// The analytics database's file name, beside `mokuro.db`.
pub const FILE_NAME: &str = "analytics.sqlite";

/// `meta.analytics_schema_version`. Bookkeeping only: every DDL statement is
/// idempotent and runs on each open.
pub const ANALYTICS_SCHEMA_VERSION: i64 = 1;

/// `meta` keys this module owns.
pub mod keys {
    /// 32 random bytes (hex) for the anonymous principals' hash (design §3).
    pub const SALT: &str = "salt";
    /// The salt period [`SALT`] belongs to: UTC day / `retention.ip_days`. A new
    /// period gets a new salt and the old one is gone.
    pub const SALT_EPOCH: &str = "salt_epoch";
    pub const SCHEMA_VERSION: &str = "analytics_schema_version";
    /// Epoch seconds of the first flush that wrote any traffic.
    pub const TRACKING_SINCE: &str = "tracking_since";
    /// The last UTC day whose hourly rows were rolled into the daily tables.
    pub const ROLLED_THROUGH_DAY: &str = "rolled_through_day";
    /// Records the collector could not keep (a full map in a flush window), all time.
    pub const DROPPED_TOTAL: &str = "dropped_total";
}

const META_DDL: &str =
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)";

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Page cache per connection, KiB (a small file; the main DB keeps the big budget).
const CACHE_KIB: i64 = 2048;
const READERS: usize = 2;
/// The `-wal` file is cut back to this after a checkpoint (a big prune or retention
/// change would otherwise leave it at its high-water size until the server stops).
const JOURNAL_SIZE_LIMIT: i64 = 8 << 20;
/// Files up to this size get `PRAGMA quick_check` when opened: a damaged table page
/// passes the open (the DDL reads only the schema) and would fail every flush after.
pub const QUICK_CHECK_MAX_BYTES: u64 = 256 << 20;

/// The `analytics.sqlite` handle. See the module docs.
pub struct AnalyticsDb {
    path: Option<PathBuf>,
    writer: Mutex<Connection>,
    readers: Mutex<Vec<Connection>>,
    reader_available: Condvar,
    reader_count: usize,
    /// The current salt; [`AnalyticsDb::maintain`] replaces it (and its stored copy)
    /// once its period is over.
    salt: RwLock<[u8; 32]>,
}

impl std::fmt::Debug for AnalyticsDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnalyticsDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// The open write transaction [`AnalyticsDb::write`] hands its closure. Each area adds
/// its upserts to it (`impl AnalyticsTx<'_>` in the area's module).
pub struct AnalyticsTx<'a> {
    pub(crate) conn: &'a Connection,
}

fn open_connection(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.pragma_update(None, "cache_size", -CACHE_KIB)?;
    Ok(conn)
}

impl AnalyticsDb {
    /// Open (creating if needed) `path`: WAL, `synchronous=NORMAL`, every area's DDL,
    /// the salt, and the read pool.
    pub fn open(path: impl AsRef<Path>) -> Result<AnalyticsDb> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| DbError::CreateDir {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let writer = open_connection(&path)?;
        writer.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get::<_, String>(0))?;
        writer.pragma_update(None, "synchronous", "NORMAL")?;
        writer.pragma_update(None, "journal_size_limit", JOURNAL_SIZE_LIMIT)?;
        let small = std::fs::metadata(&path).is_ok_and(|m| m.len() <= QUICK_CHECK_MAX_BYTES);
        if small {
            quick_check(&writer)?;
        }
        let mut readers = Vec::with_capacity(READERS);
        let salt = init(&writer)?;
        for _ in 0..READERS {
            let conn = open_connection(&path)?;
            conn.pragma_update(None, "query_only", true)?;
            readers.push(conn);
        }
        Ok(AnalyticsDb {
            path: Some(path),
            writer: Mutex::new(writer),
            readers: Mutex::new(readers),
            reader_available: Condvar::new(),
            reader_count: READERS,
            salt: RwLock::new(salt),
        })
    }

    /// A database that lives in memory only (reads share the writer): the fallback when
    /// the file cannot be opened, and tests.
    pub fn open_in_memory() -> Result<AnalyticsDb> {
        let writer = Connection::open_in_memory()?;
        let salt = init(&writer)?;
        Ok(AnalyticsDb {
            path: None,
            writer: Mutex::new(writer),
            readers: Mutex::new(Vec::new()),
            reader_available: Condvar::new(),
            reader_count: 0,
            salt: RwLock::new(salt),
        })
    }

    /// The file (`None` in memory).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The salt of the anonymous principals' hash: 32 random bytes, replaced every
    /// `retention.ip_days` (the hourly upkeep), the old one deleted. An anonymous id
    /// can therefore be traced back to its address (by whoever holds the file) only
    /// while the salt that made it lives, never longer than the raw IPs are kept.
    pub fn salt(&self) -> [u8; 32] {
        *self.salt.read()
    }

    pub(crate) fn set_salt(&self, salt: [u8; 32]) {
        *self.salt.write() = salt;
    }

    /// Run `f` in one write transaction (`BEGIN IMMEDIATE` ... `COMMIT`); an error rolls
    /// everything back, and so does a panic: a transaction left open on the writer
    /// would fail every later `BEGIN IMMEDIATE` and hold the reads that share it.
    pub fn write<T>(&self, f: impl FnOnce(&AnalyticsTx<'_>) -> Result<T>) -> Result<T> {
        let conn = self.writer.lock();
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let open = OpenWrite { conn: &conn };
        f(&AnalyticsTx { conn: open.conn }).and_then(|v| {
            open.conn
                .execute_batch("COMMIT")
                .map(|()| v)
                .map_err(DbError::from)
        })
        // `open` drops here (or on unwind) and rolls back whatever did not commit.
    }

    /// Run the read-only `f` on a pooled reader (the writer when there is no pool).
    /// The reader goes back to the pool however `f` ends, a panic included: a lost
    /// reader would leave every later read waiting for it.
    pub(crate) fn read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        if self.reader_count == 0 {
            return f(&self.writer.lock());
        }
        let conn = {
            let mut pool = self.readers.lock();
            loop {
                if let Some(c) = pool.pop() {
                    break c;
                }
                self.reader_available.wait(&mut pool);
            }
        };
        let lent = Lent {
            db: self,
            conn: Some(conn),
        };
        f(lent.conn.as_ref().expect("lent until dropped"))
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        self.read(|c| meta_get(c, key))
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.write(|tx| tx.meta_set(key, value))
    }

    /// `meta` as an integer (`None` when missing or not a number).
    pub fn meta_i64(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.meta_get(key)?.and_then(|v| v.trim().parse().ok()))
    }

    /// Epoch seconds of the first flush that recorded traffic (`None`: nothing yet).
    pub fn tracking_since(&self) -> Result<Option<i64>> {
        self.meta_i64(keys::TRACKING_SINCE)
    }
}

impl AnalyticsTx<'_> {
    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        meta_get(self.conn, key)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Set `key` only when it has no value yet.
    pub fn meta_set_once(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }

    /// Add `by` to an integer `meta` value (missing = 0).
    pub fn meta_add(&self, key: &str, by: i64) -> Result<()> {
        let now: i64 = self
            .meta_get(key)?
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        self.meta_set(key, &now.saturating_add(by).to_string())
    }
}

fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

/// `PRAGMA quick_check`: anything but `ok` is a damaged file (`SQLITE_CORRUPT`, so
/// [`DbError::is_corrupt`] tells it).
fn quick_check(conn: &Connection) -> Result<()> {
    let first: String = conn.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
    if first == "ok" {
        return Ok(());
    }
    Err(DbError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
        Some(format!("quick_check: {first}")),
    )))
}

impl AnalyticsDb {
    /// Write the WAL back into the file and cut it to [`JOURNAL_SIZE_LIMIT`] (after
    /// the hourly upkeep, whose prune may be the biggest write of the day).
    pub fn checkpoint(&self) -> Result<()> {
        if self.path.is_some() {
            self.writer
                .lock()
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        }
        Ok(())
    }
}

/// Every area's DDL, the schema version and the salt, in one transaction.
fn init(conn: &Connection) -> Result<[u8; 32]> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let out = (|| -> Result<[u8; 32]> {
        conn.execute(META_DDL, [])?;
        for ddl in traffic::DDL
            .iter()
            .chain(reading::DDL)
            .chain(library::DDL)
            .chain(host::DDL)
            .chain(crawl::DDL)
            .chain(clients::DDL)
        {
            conn.execute(ddl, [])?;
        }
        let tx = AnalyticsTx { conn };
        tx.meta_set(keys::SCHEMA_VERSION, &ANALYTICS_SCHEMA_VERSION.to_string())?;
        let salt = match tx.meta_get(keys::SALT)?.and_then(|h| parse_salt(&h)) {
            Some(s) => s,
            None => {
                let s = new_salt();
                tx.meta_set(keys::SALT, &hex::encode(s))?;
                s
            }
        };
        Ok(salt)
    })();
    match out {
        Ok(salt) => {
            conn.execute_batch("COMMIT")?;
            Ok(salt)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// 32 new random bytes for the salt.
pub(crate) fn new_salt() -> [u8; 32] {
    let mut s = [0u8; 32];
    rand::rng().fill_bytes(&mut s);
    s
}

pub(crate) fn parse_salt(hex_text: &str) -> Option<[u8; 32]> {
    let bytes = hex::decode(hex_text.trim()).ok()?;
    bytes.try_into().ok()
}

/// Saturating `u64` → SQLite INTEGER.
pub(crate) fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// SQLite INTEGER → `u64` (negative = 0).
pub(crate) fn uint(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// The writer's open transaction: rolled back when dropped unless it committed, so an
/// error and a panic in the closure end the same way.
struct OpenWrite<'a> {
    conn: &'a Connection,
}

impl Drop for OpenWrite<'_> {
    fn drop(&mut self) {
        if !self.conn.is_autocommit()
            && let Err(e) = self.conn.execute_batch("ROLLBACK")
        {
            tracing::warn!(error = %e, "analytics: rollback failed");
        }
    }
}

/// A pooled reader out of [`AnalyticsDb::read`]'s pool, put back when dropped.
struct Lent<'a> {
    db: &'a AnalyticsDb,
    conn: Option<Connection>,
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        // A closure that panicked mid-transaction must not leave its snapshot open.
        if !conn.is_autocommit()
            && let Err(e) = conn.execute_batch("ROLLBACK")
        {
            tracing::warn!(error = %e, "analytics: a reader's rollback failed");
        }
        self.db.readers.lock().push(conn);
        self.db.reader_available.notify_one();
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::AnalyticsDb;

    pub fn temp_db() -> (tempfile::TempDir, AnalyticsDb) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = AnalyticsDb::open(dir.path().join(super::FILE_NAME)).expect("open");
        (dir, db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A read whose closure panics gives its reader back: as many panics as there are
    /// readers used to leave every later read waiting forever.
    #[test]
    fn a_panicking_read_gives_its_reader_back() {
        let (_dir, db) = testutil::temp_db();
        assert!(db.reader_count > 0);
        let db = std::sync::Arc::new(db);
        for _ in 0..db.reader_count {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                db.read(|c| -> Result<()> {
                    c.execute_batch("BEGIN")?;
                    let _: i64 = c.query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))?;
                    panic!("a read closure panics");
                })
            }));
            assert!(caught.is_err());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = db.clone();
        std::thread::spawn(move || {
            let _ = tx.send(reader.meta_get(keys::SCHEMA_VERSION).map(|v| v.is_some()));
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("a read after the panics completes");
        assert!(got.unwrap());
        assert_eq!(db.readers.lock().len(), db.reader_count);
        // The panicked closure's transaction did not stay open on its reader.
        for c in db.readers.lock().iter() {
            assert!(c.is_autocommit());
        }
    }

    /// A write whose closure panics rolls its transaction back: it used to stay open on
    /// the writer, and every later `BEGIN IMMEDIATE` failed ("cannot start a
    /// transaction within a transaction").
    #[test]
    fn a_panicking_write_leaves_no_transaction_open() {
        for (_dir, db) in [
            {
                let (d, db) = testutil::temp_db();
                (Some(d), db)
            },
            (None, AnalyticsDb::open_in_memory().unwrap()),
        ] {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                db.write(|tx| -> Result<()> {
                    tx.meta_set("panicked", "yes")?;
                    panic!("a write closure panics");
                })
            }));
            assert!(caught.is_err());
            assert!(db.writer.lock().is_autocommit());
            db.meta_set("after", "ok").expect("a write after the panic");
            assert_eq!(db.meta_get("after").unwrap().as_deref(), Some("ok"));
            // The panicked write's change was rolled back, not committed by the next one.
            assert_eq!(db.meta_get("panicked").unwrap(), None);
        }
    }

    #[test]
    fn opens_wal_normal_with_every_table_and_a_stable_salt() {
        let (dir, db) = testutil::temp_db();
        let path = dir.path().join(FILE_NAME);
        let (mode, sync): (String, i64) = {
            let w = db.writer.lock();
            (
                w.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .unwrap(),
                w.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap(),
            )
        };
        assert_eq!(mode, "wal");
        assert_eq!(sync, 1, "NORMAL");
        let tables: Vec<String> = db
            .read(|c| {
                let mut s =
                    c.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
                let rows = s
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok(rows)
            })
            .unwrap();
        for t in [
            "meta",
            "traffic_hourly",
            "traffic_daily",
            "latency_hourly",
            "latency_daily",
            "errors_hourly",
            "principal_daily",
            "principal_seen",
            "anon_ips",
            "ip_daily",
            "auth_failures",
            "slow_requests",
            "volume_downloads",
            "uploads_daily",
            "read_positions",
            "reading_daily",
            "history_cursor",
            "reading_pages",
            "volume_progress",
            "devices",
            "library_scan",
            "library_daily",
            "archive_pages",
            "host_minute",
            "host_hourly",
            "crawl_episodes",
            "clients_daily",
            "client_principals",
            "client_mix_principals",
            "origins_daily",
            "geo_daily",
        ] {
            assert!(tables.iter().any(|n| n == t), "missing {t}: {tables:?}");
        }
        let salt = db.salt();
        assert_ne!(salt, [0u8; 32]);
        assert_eq!(
            db.meta_get(keys::SCHEMA_VERSION).unwrap().as_deref(),
            Some("1")
        );
        drop(db);
        // Idempotent: a second open keeps the salt and every row.
        let again = AnalyticsDb::open(&path).unwrap();
        assert_eq!(again.salt(), salt);
    }

    #[test]
    fn in_memory_works_and_write_rolls_back_on_error() {
        let db = AnalyticsDb::open_in_memory().unwrap();
        db.meta_set("a", "1").unwrap();
        let err = db.write(|tx| {
            tx.meta_set("a", "2")?;
            Err::<(), _>(DbError::Invalid("stop".into()))
        });
        assert!(err.is_err());
        assert_eq!(db.meta_get("a").unwrap().as_deref(), Some("1"));
        db.write(|tx| tx.meta_add("n", 5)).unwrap();
        db.write(|tx| tx.meta_add("n", 2)).unwrap();
        assert_eq!(db.meta_i64("n").unwrap(), Some(7));
        db.write(|tx| tx.meta_set_once("once", "x")).unwrap();
        db.write(|tx| tx.meta_set_once("once", "y")).unwrap();
        assert_eq!(db.meta_get("once").unwrap().as_deref(), Some("x"));
    }

    /// A big write (the prune of a retention change) leaves no high-water `-wal`
    /// behind once the upkeep checkpoints.
    #[test]
    fn the_wal_is_cut_back_after_a_big_write() {
        let (dir, db) = testutil::temp_db();
        let wal = dir.path().join(format!("{FILE_NAME}-wal"));
        let big = "x".repeat(4000);
        db.write(|tx| {
            for i in 0..6000 {
                tx.meta_set(&format!("k{i}"), &big)?;
            }
            Ok(())
        })
        .unwrap();
        db.write(|tx| {
            tx.conn
                .execute("DELETE FROM meta WHERE key LIKE 'k%'", [])?;
            Ok(())
        })
        .unwrap();
        // About 24 MiB went through the WAL.
        db.checkpoint().unwrap();
        db.meta_set("after", "1").unwrap();
        let after = std::fs::metadata(&wal).unwrap().len();
        assert!(
            after <= JOURNAL_SIZE_LIMIT as u64,
            "-wal left at {after} bytes"
        );
    }
}
