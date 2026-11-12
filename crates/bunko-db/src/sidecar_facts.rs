//! What the OCR census read off each sidecar file, `sidecar_facts` (0.8): a cache, one
//! row per library-relative path, valid for exactly one version of that file (its size,
//! its mtime in ns and the store's content version, `''` on a local library). The facts
//! themselves are opaque JSON here. A row is replaced when its file is read at a new
//! version, and the server prunes the rows of files that are gone or changed; a row is
//! never trusted for any other version of its file.

use crate::database::Database;
use crate::error::Result;
use rusqlite::{OptionalExtension, params};

/// The version of a file a [`SidecarFacts`] row was read at.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SidecarFactsKey {
    /// Library-relative path, exactly as the folder lists it (primary key).
    pub sidecar_path: String,
    pub size: i64,
    pub mtime_ns: i64,
    /// The store's content version; `''` on a local library.
    pub version: String,
}

/// One row: the facts read at one version of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarFacts {
    pub key: SidecarFactsKey,
    /// Opaque to the database (the server's JSON).
    pub facts: String,
}

impl Database {
    /// The row of `sidecar_path`, whatever version it was read at (the caller compares).
    pub fn get_sidecar_facts(&self, sidecar_path: &str) -> Result<Option<SidecarFacts>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT sidecar_path, size, mtime_ns, version, facts FROM sidecar_facts \
                     WHERE sidecar_path = ?",
                )?
                .query_row([sidecar_path], |r| {
                    Ok(SidecarFacts {
                        key: SidecarFactsKey {
                            sidecar_path: r.get(0)?,
                            size: r.get(1)?,
                            mtime_ns: r.get(2)?,
                            version: r.get(3)?,
                        },
                        facts: r.get(4)?,
                    })
                })
                .optional()?)
        })
    }

    /// A row read at exactly this content version, under any path: the same file under
    /// another name. The caller asks only where the store's version names a file's
    /// bytes and stays with the file through a rename (Drive, OneDrive, MEGA), never
    /// where it is an mtime and a size that two files can share. Never for a local file
    /// (`version` empty: nothing ties two paths' bytes together).
    pub fn sidecar_facts_of_version(
        &self,
        version: &str,
        size: i64,
        mtime_ns: i64,
    ) -> Result<Option<SidecarFacts>> {
        if version.is_empty() {
            return Ok(None);
        }
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT sidecar_path, size, mtime_ns, version, facts FROM sidecar_facts \
                     WHERE version = ? AND size = ? AND mtime_ns = ? LIMIT 1",
                )?
                .query_row(params![version, size, mtime_ns], |r| {
                    Ok(SidecarFacts {
                        key: SidecarFactsKey {
                            sidecar_path: r.get(0)?,
                            size: r.get(1)?,
                            mtime_ns: r.get(2)?,
                            version: r.get(3)?,
                        },
                        facts: r.get(4)?,
                    })
                })
                .optional()?)
        })
    }

    /// Upsert the facts of a file just read (replacing those of any other version).
    pub fn record_sidecar_facts(&self, row: &SidecarFacts) -> Result<()> {
        self.write(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO sidecar_facts (sidecar_path, size, mtime_ns, version, \
                 facts, read_at) VALUES (?, ?, ?, ?, ?, datetime('now'))",
                params![
                    row.key.sidecar_path,
                    row.key.size,
                    row.key.mtime_ns,
                    row.key.version,
                    row.facts
                ],
            )?;
            Ok(())
        })
    }

    /// The key of every row (what a prune compares with the files).
    pub fn sidecar_facts_keys(&self) -> Result<Vec<SidecarFactsKey>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT sidecar_path, size, mtime_ns, version FROM sidecar_facts \
                     ORDER BY sidecar_path",
                )?
                .query_map([], |r| {
                    Ok(SidecarFactsKey {
                        sidecar_path: r.get(0)?,
                        size: r.get(1)?,
                        mtime_ns: r.get(2)?,
                        version: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The row of `key` now names `new_path` (the same file renamed), while it is still
    /// that version; a row already at `new_path` gives way. Returns whether it moved.
    pub fn rename_sidecar_facts(&self, key: &SidecarFactsKey, new_path: &str) -> Result<bool> {
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE OR REPLACE sidecar_facts SET sidecar_path = ? WHERE sidecar_path = ? \
                 AND size = ? AND mtime_ns = ? AND version = ?",
                params![
                    new_path,
                    key.sidecar_path,
                    key.size,
                    key.mtime_ns,
                    key.version
                ],
            )? > 0)
        })
    }

    /// Delete the rows of `keys`, each only while it is still that version (a row
    /// recorded since, at another version, stays). Returns how many went.
    pub fn forget_sidecar_facts(&self, keys: &[SidecarFactsKey]) -> Result<usize> {
        if keys.is_empty() {
            return Ok(0);
        }
        self.write(|conn| {
            let mut stmt = conn.prepare_cached(
                "DELETE FROM sidecar_facts WHERE sidecar_path = ? AND size = ? AND mtime_ns = ? \
                 AND version = ?",
            )?;
            let mut n = 0;
            for k in keys {
                n += stmt.execute(params![k.sidecar_path, k.size, k.mtime_ns, k.version])?;
            }
            Ok(n)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_db;

    fn key(path: &str, size: i64, version: &str) -> SidecarFactsKey {
        SidecarFactsKey {
            sidecar_path: path.into(),
            size,
            mtime_ns: 1_700_000_000_123_456_789,
            version: version.into(),
        }
    }

    #[test]
    fn one_row_per_path_replaced_by_a_new_version() {
        let (_dir, db) = temp_db();
        assert_eq!(db.get_sidecar_facts("S/v.mokuro").unwrap(), None);
        let old = SidecarFacts {
            key: key("S/v.mokuro", 10, "c1"),
            facts: "{\"a\":1}".into(),
        };
        db.record_sidecar_facts(&old).unwrap();
        assert_eq!(
            db.get_sidecar_facts("S/v.mokuro").unwrap(),
            Some(old.clone())
        );
        let new = SidecarFacts {
            key: key("S/v.mokuro", 12, "c2"),
            facts: "{\"a\":2}".into(),
        };
        db.record_sidecar_facts(&new).unwrap();
        assert_eq!(
            db.get_sidecar_facts("S/v.mokuro").unwrap(),
            Some(new.clone())
        );
        assert_eq!(db.sidecar_facts_keys().unwrap(), vec![new.key.clone()]);
        // Found by its version under any path; never a local file's (no version).
        let k = &new.key;
        assert_eq!(
            db.sidecar_facts_of_version("c2", k.size, k.mtime_ns)
                .unwrap(),
            Some(new.clone())
        );
        assert_eq!(
            db.sidecar_facts_of_version("c1", k.size, k.mtime_ns)
                .unwrap(),
            None
        );
        let local = SidecarFacts {
            key: key("S/w.mokuro", 3, ""),
            facts: "{}".into(),
        };
        db.record_sidecar_facts(&local).unwrap();
        assert_eq!(
            db.sidecar_facts_of_version("", 3, local.key.mtime_ns)
                .unwrap(),
            None
        );
        db.forget_sidecar_facts(&[local.key]).unwrap();
        // Renamed: the row follows, replacing one at the new name.
        let other = SidecarFacts {
            key: key("S/V.mokuro", 1, "c0"),
            facts: "{}".into(),
        };
        db.record_sidecar_facts(&other).unwrap();
        assert!(db.rename_sidecar_facts(&new.key, "S/V.mokuro").unwrap());
        let moved = SidecarFacts {
            key: SidecarFactsKey {
                sidecar_path: "S/V.mokuro".into(),
                ..new.key.clone()
            },
            facts: new.facts.clone(),
        };
        assert_eq!(
            db.get_sidecar_facts("S/V.mokuro").unwrap(),
            Some(moved.clone())
        );
        assert_eq!(db.sidecar_facts_keys().unwrap(), vec![moved.key.clone()]);
        assert!(
            !db.rename_sidecar_facts(&new.key, "S/x.mokuro").unwrap(),
            "gone"
        );
        let new = moved;
        // Forgetting the old version leaves the new one; forgetting the new one empties it.
        assert_eq!(db.forget_sidecar_facts(&[old.key]).unwrap(), 0);
        assert_eq!(db.sidecar_facts_keys().unwrap().len(), 1);
        assert_eq!(db.forget_sidecar_facts(&[new.key]).unwrap(), 1);
        assert!(db.sidecar_facts_keys().unwrap().is_empty());
    }
}
