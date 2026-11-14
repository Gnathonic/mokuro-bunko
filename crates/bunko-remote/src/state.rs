//! `state.sqlite` (spec §5.1, D7): what must survive a resync, an index rebuild and
//! a restart. The `meta` table (instance id, the lease's keys), the `ledger` of
//! versions Bunko wrote, moved or trashed, the `outbox` of write ops, `submission`
//! rows for foreign `series.json` versions, `conflict` rows, and `local_row`s
//! (staging files that are never published, such as `.nocover`).
//!
//! The `series_queue` table holds what the store's series-file sink last landed for each
//! series (`docs/design/queue-file-v2.md` §6.7), so a restart rewrites nothing that did
//! not change.
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
  state TEXT, seen_at INTEGER, facts_only INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS conflict(id INTEGER PRIMARY KEY, kind TEXT, path TEXT, pid TEXT, detail TEXT,
  first_seen INTEGER, resolved INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS local_row(path TEXT PRIMARY KEY, kind TEXT NOT NULL, anchor_pid TEXT,
  anchor_cver TEXT, file TEXT);
CREATE TABLE IF NOT EXISTS merge_group(id TEXT PRIMARY KEY, scope TEXT NOT NULL, stem INTEGER NOT NULL,
  parent TEXT NOT NULL, winner TEXT NOT NULL, losers TEXT NOT NULL, origin TEXT NOT NULL, state TEXT NOT NULL,
  created_at INTEGER NOT NULL, finished_at INTEGER, moved INTEGER NOT NULL DEFAULT 0,
  trashed INTEGER NOT NULL DEFAULT 0, blocked INTEGER NOT NULL DEFAULT 0,
  no_trash INTEGER NOT NULL DEFAULT 0, note TEXT);
CREATE TABLE IF NOT EXISTS merge_block(conflict_id INTEGER PRIMARY KEY, this TEXT NOT NULL,
  that TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS series_queue(series_fold TEXT PRIMARY KEY, folder TEXT NOT NULL,
  rev INTEGER NOT NULL, content_sha TEXT NOT NULL, jobs TEXT NOT NULL, done TEXT NOT NULL,
  last_write_ms INTEGER NOT NULL);
";

/// Columns added after a table first shipped: `(table, column, definition)`. `SCHEMA`
/// creates a new database with them; an older one gets each missing column added (an
/// additive change, nothing rewritten).
const ADDED_COLUMNS: &[(&str, &str, &str)] =
    &[("submission", "facts_only", "INTEGER NOT NULL DEFAULT 0")];

fn add_missing_columns(conn: &Connection) -> rusqlite::Result<()> {
    for (table, column, def) in ADDED_COLUMNS {
        let has: bool = conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
            ))?
            .exists([column])?;
        if !has {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {def}"))?;
        }
    }
    Ok(())
}

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

/// One `submission` row (§9.3): a foreign `series.json` version, or a facts-only
/// series from `catalog.json` at first adoption.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionRow {
    pub id: i64,
    /// The `series.json` path it is for (`<Series>/series.json`).
    pub path: String,
    pub pid: String,
    pub cver: String,
    pub sha256: Option<String>,
    /// `fetching` | `queued` | `applied` | `refused`.
    pub state: String,
    /// Apply only the version's facts (`volumes` and `spine_offset` dropped): a duplicate
    /// or case merge's losing copy, which readers never saw at its path. Recorded when
    /// the merge opens the row, so it holds whatever the index shows when it is answered.
    pub facts_only: bool,
}

impl SubmissionRow {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SubmissionRow> {
        Ok(SubmissionRow {
            id: r.get(0)?,
            path: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            pid: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            cver: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            sha256: r.get(4)?,
            state: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
            facts_only: r.get(6)?,
        })
    }
}

/// One unresolved `conflict` row, as the admin panel lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub id: i64,
    pub kind: String,
    pub path: String,
    pub pid: Option<String>,
    pub detail: String,
    /// Unix ms.
    pub first_seen: i64,
    /// `case-merge-blocked`: the losing spelling's paths ("Keep this copy").
    pub this: Vec<String>,
    /// `case-merge-blocked`: the winner's paths ("Keep that copy").
    pub that: Vec<String>,
}

/// A conflict row's kind (§5.1 vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictRow {
    LostWrite,
    FailedOp,
    Discarded,
    /// The provider stored other bytes than Bunko uploaded (size or hash differ).
    Corrupt,
    /// A case merge left a group (or a folder) in the losing spelling (§5.9).
    CaseMergeBlocked,
    /// A name that has duplicates after a case-merge move (a reader upload raced it).
    DupFile,
    /// Duplicate folders a duplicate merge keeps meeting again: left alone (§5.9).
    DupRecreated,
}

impl ConflictRow {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictRow::LostWrite => "lost-write",
            ConflictRow::FailedOp => "failed-op",
            ConflictRow::Discarded => "discarded",
            ConflictRow::Corrupt => "corrupt",
            ConflictRow::CaseMergeBlocked => "case-merge-blocked",
            ConflictRow::DupFile => "dup-file",
            ConflictRow::DupRecreated => "dup-recreated",
        }
    }
}

/// One `series_queue` row: what the series-file sink last landed for a series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesQueueRow {
    /// The series' fold key (primary key).
    pub series_fold: String,
    /// The folder spelling the file was written in.
    pub folder: String,
    pub rev: u64,
    /// sha256 (hex) of the body without `at`, `rev` and `clock_at`.
    pub content_sha: String,
    /// The file's jobs: `{"at", "clock_at", "rows", "vols"}` as JSON.
    pub jobs: String,
    /// Its `done` tail as JSON.
    pub done: String,
    /// When it committed, unix ms.
    pub last_write_ms: i64,
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
        add_missing_columns(&conn).map_err(db_io)?;
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

    // ---- the series-file sink (queue file v2 §6.7) --------------------------------

    /// Every `series_queue` row.
    pub fn series_queue_rows(&self) -> Vec<SeriesQueueRow> {
        self.with("series_queue_rows", |c| {
            c.prepare(
                "SELECT series_fold, folder, rev, content_sha, jobs, done, last_write_ms \
                 FROM series_queue ORDER BY series_fold",
            )?
            .query_map([], |r| {
                Ok(SeriesQueueRow {
                    series_fold: r.get(0)?,
                    folder: r.get(1)?,
                    rev: r.get::<_, i64>(2)?.max(0) as u64,
                    content_sha: r.get(3)?,
                    jobs: r.get(4)?,
                    done: r.get(5)?,
                    last_write_ms: r.get(6)?,
                })
            })?
            .collect()
        })
        .unwrap_or_default()
    }

    /// Record (replace) a series' row.
    pub fn series_queue_put(&self, row: &SeriesQueueRow) -> std::io::Result<()> {
        self.with("series_queue_put", |c| {
            c.execute(
                "INSERT OR REPLACE INTO series_queue(series_fold, folder, rev, content_sha, jobs, \
                 done, last_write_ms) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    row.series_fold,
                    row.folder,
                    i64::try_from(row.rev).unwrap_or(i64::MAX),
                    row.content_sha,
                    row.jobs,
                    row.done,
                    row.last_write_ms
                ],
            )
            .map(|_| ())
        })
    }

    /// Forget a series' row.
    pub fn series_queue_del(&self, series_fold: &str) -> std::io::Result<()> {
        self.with("series_queue_del", |c| {
            c.execute(
                "DELETE FROM series_queue WHERE series_fold = ?1",
                [series_fold],
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

    /// [`StateDb::conflict_add`], unless an unresolved row of that kind and path
    /// exists; returns the row's id (new or existing).
    pub fn conflict_once(
        &self,
        kind: ConflictRow,
        path: &str,
        pid: Option<&str>,
        detail: &str,
    ) -> Option<i64> {
        self.with("conflict_once", |c| {
            let found: Option<i64> = c
                .query_row(
                    "SELECT id FROM conflict WHERE kind = ?1 AND path = ?2 AND resolved = 0",
                    params![kind.as_str(), path],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = found {
                return Ok(id);
            }
            c.execute(
                "INSERT INTO conflict(kind, path, pid, detail, first_seen) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![kind.as_str(), path, pid, detail, now_ms()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .ok()
    }

    /// Every unresolved conflict with its id.
    pub fn conflict_rows(&self) -> Vec<Conflict> {
        self.with("conflict_rows", |c| {
            c.prepare(
                "SELECT c.id, c.kind, c.path, c.pid, c.detail, c.first_seen, b.this, b.that \
                 FROM conflict c LEFT JOIN merge_block b ON b.conflict_id = c.id \
                 WHERE c.resolved = 0 ORDER BY c.id",
            )?
            .query_map([], |r| {
                let list = |s: Option<String>| -> Vec<String> {
                    s.and_then(|j| serde_json::from_str(&j).ok())
                        .unwrap_or_default()
                };
                Ok(Conflict {
                    id: r.get(0)?,
                    kind: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    path: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    pid: r.get(3)?,
                    detail: r.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    first_seen: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    this: list(r.get(6)?),
                    that: list(r.get(7)?),
                })
            })?
            .collect()
        })
        .unwrap_or_default()
    }

    /// Mark every unresolved conflict of one of `kinds` at the library path `path`
    /// (ASCII case aside) resolved; returns how many.
    pub fn conflicts_resolve_at(&self, kinds: &[ConflictRow], path: &str) -> usize {
        kinds
            .iter()
            .map(|k| {
                self.with("conflicts_resolve_at", |c| {
                    c.execute(
                        "UPDATE conflict SET resolved = 1 \
                         WHERE kind = ?1 AND lower(path) = lower(?2) AND resolved = 0",
                        params![k.as_str(), path],
                    )
                })
                .unwrap_or(0)
            })
            .sum()
    }

    /// Mark a conflict resolved.
    pub fn conflict_resolve(&self, id: i64) {
        let _ = self.with("conflict_resolve", |c| {
            c.execute("UPDATE conflict SET resolved = 1 WHERE id = ?1", [id])
        });
    }

    /// A `case-merge-blocked` row with its two sides (library paths): `this` (the
    /// losing spelling's files) and `that` (the winner's), in one transaction, unless
    /// an unresolved row for `path` exists. Returns the row's id.
    pub fn merge_block_add(
        &self,
        path: &str,
        pid: Option<&str>,
        detail: &str,
        this: &[String],
        that: &[String],
    ) -> Option<i64> {
        let j = |v: &[String]| serde_json::to_string(v).unwrap_or_else(|_| "[]".into());
        let kind = ConflictRow::CaseMergeBlocked.as_str();
        self.with("merge_block_add", |c| {
            let tx = c.transaction()?;
            let found: Option<i64> = tx
                .query_row(
                    "SELECT id FROM conflict WHERE kind = ?1 AND path = ?2 AND resolved = 0",
                    params![kind, path],
                    |r| r.get(0),
                )
                .optional()?;
            let id = match found {
                Some(id) => id,
                None => {
                    tx.execute(
                        "INSERT INTO conflict(kind, path, pid, detail, first_seen) VALUES(?1, ?2, ?3, ?4, ?5)",
                        params![kind, path, pid, detail, now_ms()],
                    )?;
                    let id = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT OR REPLACE INTO merge_block(conflict_id, this, that) VALUES(?1, ?2, ?3)",
                        params![id, j(this), j(that)],
                    )?;
                    id
                }
            };
            tx.commit()?;
            Ok(id)
        })
        .ok()
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

    /// The submission row for `(path, pid, cver)`, created in state `fetching` when
    /// there is none. `facts_only` (a merge's losing copy, [`SubmissionRow::facts_only`])
    /// marks a new row, and an existing one still owed an answer. `(id, state, created)`.
    pub fn submission_open(
        &self,
        path: &str,
        pid: &str,
        cver: &str,
        facts_only: bool,
    ) -> Option<(i64, String, bool)> {
        self.with("submission_open", |c| {
            let tx = c.transaction()?;
            let found: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id, state FROM submission WHERE path = ?1 AND pid = ?2 AND cver = ?3",
                    params![path, pid, cver],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let out = match found {
                Some((id, state)) => {
                    if facts_only && matches!(state.as_str(), "fetching" | "queued") {
                        tx.execute("UPDATE submission SET facts_only = 1 WHERE id = ?1", [id])?;
                    }
                    (id, state, false)
                }
                None => {
                    tx.execute(
                        "INSERT INTO submission(path, pid, cver, state, seen_at, facts_only) \
                         VALUES(?1, ?2, ?3, 'fetching', ?4, ?5)",
                        params![path, pid, cver, now_ms(), facts_only],
                    )?;
                    (tx.last_insert_rowid(), "fetching".to_string(), true)
                }
            };
            tx.commit()?;
            Ok(out)
        })
        .ok()
    }

    /// One submission row.
    pub fn submission_get(&self, id: i64) -> Option<SubmissionRow> {
        self.with("submission_get", |c| {
            c.query_row(
                "SELECT id, path, pid, cver, sha256, state, facts_only FROM submission WHERE id = ?1",
                [id],
                SubmissionRow::from_row,
            )
            .optional()
        })
        .ok()
        .flatten()
    }

    /// Rows still owed an answer (`fetching` or `queued`), oldest first.
    pub fn submissions_pending(&self) -> Vec<SubmissionRow> {
        self.with("submissions_pending", |c| {
            c.prepare(
                "SELECT id, path, pid, cver, sha256, state, facts_only FROM submission \
                 WHERE state IN ('fetching', 'queued') ORDER BY id",
            )?
            .query_map([], SubmissionRow::from_row)?
            .collect()
        })
        .unwrap_or_default()
    }

    /// The bytes of submission `id` are pinned (`queued`, with their sha256).
    pub fn submission_fetched(&self, id: i64, sha256: &str) {
        let _ = self.with("submission_fetched", |c| {
            c.execute(
                "UPDATE submission SET state = 'queued', sha256 = ?2 WHERE id = ?1",
                params![id, sha256],
            )
        });
    }

    pub fn submission_set(&self, id: i64, state: &str) {
        let _ = self.with("submission_set", |c| {
            c.execute(
                "UPDATE submission SET state = ?2 WHERE id = ?1",
                params![id, state],
            )
        });
    }

    /// Forget a row whose version was superseded before its bytes were read.
    pub fn submission_delete(&self, id: i64) {
        let _ = self.with("submission_delete", |c| {
            c.execute("DELETE FROM submission WHERE id = ?1", [id])
        });
    }

    /// `path` (a `series.json`) has a submission not yet answered: Compiler ops for
    /// its series stay HELD (§9.3).
    pub fn submission_unanswered(&self, path: &str) -> bool {
        self.with("submission_unanswered", |c| {
            c.query_row(
                "SELECT 1 FROM submission WHERE path = ?1 AND state IN ('fetching', 'queued') LIMIT 1",
                [path],
                |_| Ok(()),
            )
            .optional()
        })
        .ok()
        .flatten()
        .is_some()
    }

    /// `cver` of `path` was captured as a submission (its bytes pinned, merged or
    /// refused): a Compiler `Replace` base that counts as merged under D8.
    pub fn submission_base_ok(&self, path: &str, cver: &str) -> bool {
        self.with("submission_base_ok", |c| {
            c.query_row(
                "SELECT 1 FROM submission WHERE path = ?1 AND cver = ?2 \
                 AND state IN ('queued', 'applied', 'refused') LIMIT 1",
                params![path, cver],
                |_| Ok(()),
            )
            .optional()
        })
        .ok()
        .flatten()
        .is_some()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A `state.sqlite` from before `submission.facts_only` opens, keeps its rows (as
    /// whole submissions), and records the mark from then on.
    #[test]
    fn an_older_submission_table_gains_facts_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE submission(id INTEGER PRIMARY KEY, path TEXT, pid TEXT, cver TEXT, \
                 sha256 TEXT, state TEXT, seen_at INTEGER);
                 INSERT INTO submission(path, pid, cver, state, seen_at) \
                 VALUES('S/series.json', 'p1', 'c1', 'queued', 1);",
            )
            .unwrap();
        }
        let db = StateDb::open(&path).unwrap();
        let old = db.submissions_pending();
        assert_eq!(old.len(), 1);
        assert!(!old[0].facts_only);
        let (id, _, created) = db
            .submission_open("S/series.json", "p2", "c2", true)
            .unwrap();
        assert!(created);
        assert!(db.submission_get(id).unwrap().facts_only);
        // A merge reaching a row still owed an answer marks it too.
        let (id, _, created) = db
            .submission_open("S/series.json", "p1", "c1", true)
            .unwrap();
        assert!(!created);
        assert!(db.submission_get(id).unwrap().facts_only);
        drop(db);
        // Opening again adds nothing twice.
        let db = StateDb::open(&path).unwrap();
        assert_eq!(db.submissions_pending().len(), 2);
    }
}
