//! `state.sqlite` (spec §5.1, D7): what must survive a resync, an index rebuild and
//! a restart. The `meta` table (instance id, the lease's keys), the `ledger` of
//! versions Bunko wrote, moved or trashed, the `outbox` of write ops, `submission`
//! rows for foreign `series.json` versions, `conflict` rows, and `local_row`s
//! (staging files that are never published, such as `.nocover`).
//!
//! The `own` set — `(pid, cver)` pairs the ledger holds with `echo_only = 0` — is
//! kept in memory as well, because every metadata answer consults it.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::cache::now_ms;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS ledger(pid TEXT NOT NULL, cver TEXT NOT NULL, parent_pid TEXT, name TEXT,
  sha256 TEXT, op_id TEXT, at INTEGER,
  trashed INTEGER NOT NULL DEFAULT 0,
  echo_only INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(pid, cver));
CREATE TABLE IF NOT EXISTS outbox(op_id TEXT PRIMARY KEY, seq INTEGER UNIQUE NOT NULL, kind TEXT NOT NULL,
  dest TEXT NOT NULL, src TEXT, mode TEXT NOT NULL, origin TEXT NOT NULL, durability TEXT NOT NULL,
  base_cver TEXT, target_pid TEXT, parent_pid TEXT, name TEXT, staged TEXT, sha256 TEXT, size INTEGER,
  coalesce_key TEXT, depends_on TEXT, prealloc_id TEXT, resume TEXT, group_id TEXT,
  state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0, next_at INTEGER NOT NULL DEFAULT 0,
  last_error TEXT, result_pid TEXT, result_cver TEXT, superseded_pid TEXT, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS submission(id INTEGER PRIMARY KEY, path TEXT, pid TEXT, cver TEXT, sha256 TEXT,
  state TEXT, seen_at INTEGER);
CREATE TABLE IF NOT EXISTS conflict(id INTEGER PRIMARY KEY, kind TEXT, path TEXT, pid TEXT, detail TEXT,
  first_seen INTEGER, resolved INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS local_row(path TEXT PRIMARY KEY, kind TEXT NOT NULL, anchor_pid TEXT,
  anchor_cver TEXT, file TEXT);
";

/// One ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRow {
    pub pid: String,
    pub cver: String,
    pub parent_pid: Option<String>,
    pub name: String,
    pub sha256: Option<String>,
    pub op_id: Option<String>,
    pub trashed: bool,
    /// Bunko moved or trashed a file it did not write: echo matching only.
    pub echo_only: bool,
}

/// A conflict row's kind (§5.1 vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictRow {
    LostWrite,
    FailedOp,
    Discarded,
    /// The provider stored other bytes than Bunko uploaded (size or hash differ).
    Corrupt,
}

impl ConflictRow {
    fn as_str(self) -> &'static str {
        match self {
            ConflictRow::LostWrite => "lost-write",
            ConflictRow::FailedOp => "failed-op",
            ConflictRow::Discarded => "discarded",
            ConflictRow::Corrupt => "corrupt",
        }
    }
}

/// The durable state database.
pub struct StateDb {
    conn: Arc<Mutex<Connection>>,
    own: Mutex<HashSet<(String, String)>>,
}

fn db_io(e: rusqlite::Error) -> std::io::Error {
    std::io::Error::other(e)
}

impl StateDb {
    /// Open (or create) `path` in WAL mode.
    pub fn open(path: &Path) -> std::io::Result<StateDb> {
        let conn = Connection::open(path).map_err(db_io)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_io)?;
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(db_io)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(db_io)?;
        conn.execute_batch(SCHEMA).map_err(db_io)?;
        let own: HashSet<(String, String)> = conn
            .prepare("SELECT pid, cver FROM ledger WHERE echo_only = 0 AND trashed = 0")
            .and_then(|mut s| {
                s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()
            })
            .map_err(db_io)?;
        Ok(StateDb {
            conn: Arc::new(Mutex::new(conn)),
            own: Mutex::new(own),
        })
    }

    /// The connection (the lease keeps its keys in `meta`).
    pub fn conn(&self) -> &Arc<Mutex<Connection>> {
        &self.conn
    }

    pub(crate) fn with<T>(
        &self,
        what: &str,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>,
    ) -> std::io::Result<T> {
        f(&mut self.conn.lock()).map_err(|e| {
            tracing::warn!("state db {what}: {e}");
            db_io(e)
        })
    }

    pub fn meta_get(&self, k: &str) -> Option<String> {
        self.with("meta_get", |c| {
            c.query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
                .optional()
        })
        .ok()
        .flatten()
    }

    pub fn meta_set(&self, k: &str, v: &str) -> std::io::Result<()> {
        self.with("meta_set", |c| {
            c.execute(
                "INSERT INTO meta(k, v) VALUES(?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                params![k, v],
            )
            .map(|_| ())
        })
    }

    /// This install's id, created on first use.
    pub fn instance_id(&self) -> std::io::Result<String> {
        if let Some(id) = self.meta_get("instance_id") {
            return Ok(id);
        }
        let id = random_id();
        self.meta_set("instance_id", &id)?;
        Ok(id)
    }

    // ---- the ledger -----------------------------------------------------------------

    /// Record a version Bunko wrote (or moved / trashed): replaces the row.
    pub fn ledger_put(&self, row: &LedgerRow) -> std::io::Result<()> {
        self.with("ledger_put", |c| {
            c.execute(
                "INSERT OR REPLACE INTO ledger(pid, cver, parent_pid, name, sha256, op_id, at, trashed, echo_only) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    row.pid,
                    row.cver,
                    row.parent_pid,
                    row.name,
                    row.sha256,
                    row.op_id,
                    now_ms(),
                    row.trashed,
                    row.echo_only
                ],
            )
            .map(|_| ())
        })?;
        let key = (row.pid.clone(), row.cver.clone());
        let mut own = self.own.lock();
        if !row.echo_only && !row.trashed {
            own.insert(key);
        } else {
            own.remove(&key);
        }
        Ok(())
    }

    /// Every row of `pid`.
    pub fn ledger_rows(&self, pid: &str) -> Vec<LedgerRow> {
        self.with("ledger_rows", |c| {
            c.prepare(
                "SELECT pid, cver, parent_pid, name, sha256, op_id, trashed, echo_only FROM ledger WHERE pid = ?1",
            )?
            .query_map([pid], |r| {
                Ok(LedgerRow {
                    pid: r.get(0)?,
                    cver: r.get(1)?,
                    parent_pid: r.get(2)?,
                    name: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    sha256: r.get(4)?,
                    op_id: r.get(5)?,
                    trashed: r.get(6)?,
                    echo_only: r.get(7)?,
                })
            })?
            .collect()
        })
        .unwrap_or_default()
    }

    /// `pid` was moved or renamed by Bunko: its rows follow (`new_pid` on path-id
    /// providers; `new_cver` when the move changed it).
    pub fn ledger_move(
        &self,
        pid: &str,
        new_pid: &str,
        new_cver: Option<&str>,
        parent_pid: Option<&str>,
        name: &str,
    ) -> std::io::Result<()> {
        let rows = self.ledger_rows(pid);
        self.with("ledger_move", |c| {
            let tx = c.transaction()?;
            for r in &rows {
                tx.execute(
                    "DELETE FROM ledger WHERE pid = ?1 AND cver = ?2",
                    params![r.pid, r.cver],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO ledger(pid, cver, parent_pid, name, sha256, op_id, at, trashed, echo_only) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        new_pid,
                        new_cver.unwrap_or(&r.cver),
                        parent_pid,
                        name,
                        r.sha256,
                        r.op_id,
                        now_ms(),
                        r.trashed,
                        r.echo_only
                    ],
                )?;
            }
            tx.commit()
        })?;
        let mut own = self.own.lock();
        for r in rows {
            if own.remove(&(r.pid.clone(), r.cver.clone())) {
                own.insert((
                    new_pid.to_string(),
                    new_cver.map_or(r.cver.clone(), str::to_string),
                ));
            }
        }
        Ok(())
    }

    /// Path-id providers: every ledger row under the moved folder `old` follows it.
    pub fn ledger_rebase(&self, old: &str, new: &str) -> std::io::Result<()> {
        let n = old.chars().count() as i64;
        self.with("ledger_rebase", |c| {
            c.execute(
                "UPDATE OR REPLACE ledger SET pid = ?2 || substr(pid, ?3 + 1), \
                 parent_pid = CASE WHEN parent_pid = ?1 OR substr(parent_pid, 1, ?3 + 1) = ?1 || '/' \
                   THEN ?2 || substr(parent_pid, ?3 + 1) ELSE parent_pid END \
                 WHERE substr(pid, 1, ?3 + 1) = ?1 || '/'",
                params![old, new, n],
            )
            .map(|_| ())
        })?;
        let prefix = format!("{old}/");
        let mut own = self.own.lock();
        let moved: Vec<(String, String)> = own
            .iter()
            .filter(|(p, _)| p.starts_with(&prefix))
            .cloned()
            .collect();
        for (p, c) in moved {
            own.remove(&(p.clone(), c.clone()));
            own.insert((format!("{new}{}", &p[old.len()..]), c));
        }
        Ok(())
    }

    /// `pid` (every version) went to the trash or was deleted by Bunko.
    pub fn ledger_trash(&self, pid: &str) -> std::io::Result<()> {
        self.with("ledger_trash", |c| {
            c.execute("UPDATE ledger SET trashed = 1 WHERE pid = ?1", [pid])
                .map(|_| ())
        })?;
        self.own.lock().retain(|(p, _)| p != pid);
        Ok(())
    }

    /// `(pid, cver)` is a version Bunko wrote (not an `echo_only` row).
    pub fn is_own(&self, pid: &str, cver: &str) -> bool {
        self.own
            .lock()
            .contains(&(pid.to_string(), cver.to_string()))
    }

    /// `(pid, cver)` is a file version Bunko wrote *and proved* (D16): a ledger row
    /// that is not `echo_only` or trashed and records the sha256 of the bytes Bunko
    /// staged — written only when the provider's hash, a pinned re-download, or the
    /// write response's size and hash showed those bytes landed. Folders, lease
    /// markers and moved or trashed foreign files never qualify. The only versions
    /// Bunko may delete permanently.
    pub fn is_own_proven(&self, pid: &str, cver: &str) -> bool {
        self.is_own(pid, cver)
            && self
                .with("is_own_proven", |c| {
                    c.query_row(
                        "SELECT 1 FROM ledger WHERE pid = ?1 AND cver = ?2 AND echo_only = 0 \
                         AND trashed = 0 AND sha256 IS NOT NULL AND length(sha256) = 64",
                        params![pid, cver],
                        |_| Ok(()),
                    )
                    .optional()
                })
                .ok()
                .flatten()
                .is_some()
    }

    /// The feed's upsert matches a ledger row exactly (D20).
    pub fn is_echo(&self, pid: &str, parent: Option<&str>, name: &str, cver: &str) -> bool {
        self.with("is_echo", |c| {
            c.query_row(
                "SELECT 1 FROM ledger WHERE pid = ?1 AND cver = ?2 AND name = ?3 AND parent_pid IS ?4",
                params![pid, cver, name, parent],
                |_| Ok(()),
            )
            .optional()
        })
        .ok()
        .flatten()
        .is_some()
    }

    /// The removal of `pid` at `cver` is Bunko's own trash or delete.
    pub fn is_own_delete(&self, pid: &str, cver: &str) -> bool {
        self.with("is_own_delete", |c| {
            c.query_row(
                "SELECT 1 FROM ledger WHERE pid = ?1 AND cver = ?2 AND trashed = 1",
                params![pid, cver],
                |_| Ok(()),
            )
            .optional()
        })
        .ok()
        .flatten()
        .is_some()
    }

    /// A ledger row for `pid` written within `within` (a full sync's listing may
    /// predate it).
    pub fn ledger_recent(&self, pid: &str, within: Duration) -> bool {
        let since = now_ms() - within.as_millis() as i64;
        self.with("ledger_recent", |c| {
            c.query_row(
                "SELECT 1 FROM ledger WHERE pid = ?1 AND at >= ?2 AND trashed = 0 LIMIT 1",
                params![pid, since],
                |_| Ok(()),
            )
            .optional()
        })
        .ok()
        .flatten()
        .is_some()
    }

    // ---- conflicts and submissions ----------------------------------------------------

    pub fn conflict_add(&self, kind: ConflictRow, path: &str, pid: Option<&str>, detail: &str) {
        let _ = self.with("conflict_add", |c| {
            c.execute(
                "INSERT INTO conflict(kind, path, pid, detail, first_seen) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![kind.as_str(), path, pid, detail, now_ms()],
            )
        });
    }

    /// `(kind, path, detail)` of every unresolved conflict.
    pub fn conflicts(&self) -> Vec<(String, String, String)> {
        self.with("conflicts", |c| {
            c.prepare("SELECT kind, path, detail FROM conflict WHERE resolved = 0 ORDER BY id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
        .unwrap_or_default()
    }

    /// A captured foreign `series.json` version; returns its id.
    pub fn submission_add(&self, path: &str, pid: &str, cver: &str, sha256: &str) -> Option<i64> {
        self.with("submission_add", |c| {
            c.execute(
                "INSERT INTO submission(path, pid, cver, sha256, state, seen_at) VALUES(?1, ?2, ?3, ?4, 'queued', ?5)",
                params![path, pid, cver, sha256, now_ms()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .ok()
    }

    pub fn submission_set(&self, id: i64, state: &str) {
        let _ = self.with("submission_set", |c| {
            c.execute(
                "UPDATE submission SET state = ?2 WHERE id = ?1",
                params![id, state],
            )
        });
    }

    /// `(path, cver, state)` of every submission.
    pub fn submissions(&self) -> Vec<(String, String, String)> {
        self.with("submissions", |c| {
            c.prepare("SELECT path, cver, state FROM submission ORDER BY id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
        .unwrap_or_default()
    }

    // ---- local rows (staging files) ---------------------------------------------------

    pub fn local_put(&self, path: &str, kind: &str, file: &str) {
        let _ = self.with("local_put", |c| {
            c.execute(
                "INSERT OR REPLACE INTO local_row(path, kind, file) VALUES(?1, ?2, ?3)",
                params![path, kind, file],
            )
        });
    }

    pub fn local_del(&self, path: &str) {
        let _ = self.with("local_del", |c| {
            c.execute("DELETE FROM local_row WHERE path = ?1", [path])
        });
    }

    /// `(path, file)` of every row of `kind`.
    pub fn local_rows(&self, kind: &str) -> Vec<(String, String)> {
        self.with("local_rows", |c| {
            c.prepare("SELECT path, file FROM local_row WHERE kind = ?1 AND file IS NOT NULL")?
                .query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        })
        .unwrap_or_default()
    }
}

/// 32 hex digits that differ between installs and runs.
pub fn random_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut h = Sha256::new();
    h.update(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
            .to_le_bytes(),
    );
    h.update(std::process::id().to_le_bytes());
    h.update(SEQ.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let local = 0u8;
    h.update((&local as *const u8 as usize).to_le_bytes());
    hex::encode(&h.finalize()[..16])
}
