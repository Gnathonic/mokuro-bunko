//! The hash memo (`<state_dir>/folder-memo.sqlite`): the sha256 of every version Bunko
//! wrote, keyed by pid and cver. A listing carries no hash, so without it a crash
//! after an upload's rename could prove the file Bunko's own only by downloading it
//! again (4 MiB at most). The hash is attached to `upload`/`copy` results, `stat` and
//! `find_named` only, never to listings: a file changed in place with the same mtime
//! and size would then fail every later download's hash check.
//!
//! Best effort throughout: a memo that cannot be opened or written means no hashes.
//! Rows are keyed by `(pid, cver)` so an upload that loses its rename never overwrites
//! the proof of the version that is there.

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) const MEMO_FILE: &str = "folder-memo.sqlite";

pub(crate) struct Memo {
    conn: Mutex<Option<Connection>>,
}

/// `pid` itself or anything below it.
const UNDER: &str = "(pid = ?1 OR substr(pid, 1, length(?1) + 1) = ?1 || '/')";

impl Memo {
    pub(crate) fn open(state_dir: &Path) -> Memo {
        let conn = std::fs::create_dir_all(state_dir)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                let c = Connection::open(state_dir.join(MEMO_FILE)).map_err(|e| e.to_string())?;
                c.execute_batch(
                    "PRAGMA journal_mode = WAL;
                     PRAGMA synchronous = NORMAL;
                     CREATE TABLE IF NOT EXISTS memo (
                         pid TEXT NOT NULL,
                         cver TEXT NOT NULL,
                         sha256 TEXT NOT NULL,
                         PRIMARY KEY (pid, cver)
                     );",
                )
                .map_err(|e| e.to_string())?;
                Ok(c)
            });
        let conn = match conn {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("folder: the hash memo is off: {e}");
                None
            }
        };
        Memo {
            conn: Mutex::new(conn),
        }
    }

    fn with<T>(&self, what: &str, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
        let g = self.conn.lock();
        let c = g.as_ref()?;
        match f(c) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("folder: hash memo {what}: {e}");
                None
            }
        }
    }

    /// The sha256 (hex) Bunko wrote as `pid` at `cver`.
    pub(crate) fn get(&self, pid: &str, cver: &str) -> Option<String> {
        self.with("read", |c| {
            c.query_row(
                "SELECT sha256 FROM memo WHERE pid = ?1 AND cver = ?2",
                params![pid, cver],
                |r| r.get(0),
            )
            .optional()
        })
        .flatten()
    }

    pub(crate) fn put(&self, pid: &str, cver: &str, sha256_hex: &str) {
        self.with("write", |c| {
            c.execute(
                "INSERT OR REPLACE INTO memo (pid, cver, sha256) VALUES (?1, ?2, ?3)",
                params![pid, cver, sha256_hex],
            )
        });
    }

    /// An upload did not land: its row goes.
    pub(crate) fn forget(&self, pid: &str, cver: &str) {
        self.with("forget", |c| {
            c.execute(
                "DELETE FROM memo WHERE pid = ?1 AND cver = ?2",
                params![pid, cver],
            )
        });
    }

    /// An upload landed: older versions' rows of `pid` go.
    pub(crate) fn keep_only(&self, pid: &str, cver: &str) {
        self.with("prune", |c| {
            c.execute(
                "DELETE FROM memo WHERE pid = ?1 AND cver <> ?2",
                params![pid, cver],
            )
        });
    }

    /// A move: rows at or below `from` follow to `to`.
    pub(crate) fn rebase(&self, from: &str, to: &str) {
        if from.is_empty() {
            return;
        }
        self.with("rebase", |c| {
            c.execute(
                &format!(
                    "UPDATE OR REPLACE memo SET pid = ?2 || substr(pid, length(?1) + 1) WHERE {UNDER}"
                ),
                params![from, to],
            )
        });
    }

    /// A trash or delete: rows at or below `pid` go.
    pub(crate) fn drop_tree(&self, pid: &str) {
        if pid.is_empty() {
            return;
        }
        self.with("drop", |c| {
            c.execute(&format!("DELETE FROM memo WHERE {UNDER}"), params![pid])
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_follow_moves_and_go_with_trash() {
        let d = tempfile::tempdir().unwrap();
        let m = Memo::open(d.path());
        m.put("S/a.cbz", "1-3", "aa");
        m.put("S/b.cbz", "1-3", "bb");
        m.put("Sx/c.cbz", "1-3", "cc");
        m.put("S/a.cbz", "2-3", "a2");
        m.keep_only("S/a.cbz", "2-3");
        assert_eq!(m.get("S/a.cbz", "1-3"), None);
        assert_eq!(m.get("S/a.cbz", "2-3").as_deref(), Some("a2"));
        m.rebase("S", "T");
        assert_eq!(m.get("T/a.cbz", "2-3").as_deref(), Some("a2"));
        assert_eq!(m.get("T/b.cbz", "1-3").as_deref(), Some("bb"));
        assert_eq!(m.get("Sx/c.cbz", "1-3").as_deref(), Some("cc"));
        m.drop_tree("T");
        assert_eq!(m.get("T/b.cbz", "1-3"), None);
        assert_eq!(m.get("Sx/c.cbz", "1-3").as_deref(), Some("cc"));
        m.forget("Sx/c.cbz", "1-3");
        assert_eq!(m.get("Sx/c.cbz", "1-3"), None);
    }
}
