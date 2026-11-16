//! Series folder owners (0.8, `series_folder_owners`): who controls a top-level library
//! folder `/mokuro-reader/<Series>/`.
//!
//! The account whose request created a folder owns it for good; a folder that existed
//! before the table gets the one uploader of its tracked volumes, once
//! ([`Database::finish_series_folder_adoption`]). Records are keyed by the series-identity
//! key ([`fold_series_title_key`]: the filename fold, trimmed, whitespace runs collapsed),
//! so `Dr Stone/`, `dr  stone/`, `STRASSE/` beside `Straße/` and an NFD spelling are one
//! folder. Owner names compare ignoring ASCII case, as account names do. The key is
//! asked again at every open ([`rekey_series_folder_owners`]): a record keyed by an
//! older key moves to its current one, and of records that come to share a key the
//! earliest recorded is kept, the others reported ([`Database::series_owner_merges`]).
//!
//! A record says nothing about whether its folder still exists: the server checks the
//! listing before it acts on one (a folder can go without a hook: a crash, a delete in
//! the cloud while offline), and a later creator of the folder replaces it.

use crate::database::Database;
use crate::error::Result;
use crate::uploads::fold_series_title_key;
use rusqlite::{Connection, OptionalExtension, params};

/// The audit action recording a series owner record the rekey merged into another.
pub const SERIES_OWNER_MERGED: &str = "series_owner_merged";

/// One owned folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesFolderOwner {
    pub series_key: String,
    /// A spelling of the folder (the one recorded), to report.
    pub folder_name: String,
    pub owner: String,
}

/// The record key of a folder name, `None` for a name that folds to nothing.
fn key_of(folder: &str) -> Option<String> {
    let key = fold_series_title_key(folder.trim_matches('/'));
    (!key.is_empty()).then_some(key)
}

fn owner_at(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT owner_username FROM series_folder_owners WHERE series_key = ?")?
        .query_row([key], |r| r.get(0))
        .optional()?)
}

fn put(conn: &Connection, key: &str, folder: &str, user: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO series_folder_owners (series_key, folder_name, owner_username) \
         VALUES (?, ?, ?) ON CONFLICT(series_key) DO UPDATE SET \
         folder_name = excluded.folder_name, owner_username = excluded.owner_username, \
         recorded_at = datetime('now')",
        params![key, folder.trim_matches('/'), user],
    )?;
    Ok(())
}

/// Does a part joining a series owned by `existing` agree on its owner? Its own record
/// decides; without one, the uploaders of its tracked volumes must all be that owner
/// (none at all agrees). A series with no owner stays without one.
fn joined_part_agrees(existing: Option<&str>, moving: Option<&str>, uploaders: &[String]) -> bool {
    let Some(existing) = existing else {
        return false;
    };
    match moving {
        Some(m) => m.eq_ignore_ascii_case(existing),
        None => uploaders.iter().all(|u| u.eq_ignore_ascii_case(existing)),
    }
}

fn forget(conn: &Connection, key: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM series_folder_owners WHERE series_key = ?",
        [key],
    )? > 0)
}

/// A series owner record the rekey at open ([`rekey_series_folder_owners`]) merged into
/// an earlier one of the same series key: `folder_name`'s record by `owner` went, the
/// record of `kept_folder` by `kept_owner` stayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesOwnerMerge {
    pub folder_name: String,
    pub owner: String,
    pub kept_folder: String,
    pub kept_owner: String,
}

/// Bring every `series_folder_owners` record under its current series key
/// ([`key_of`]): a record keyed otherwise (an older key: before fold v2, `Straße/` and
/// `STRASSE/` were two series) moves to its key, and where several records come to
/// share one, the earliest recorded (`recorded_at`, then the oldest row) is kept and
/// every other is dropped, each logged and written to the audit log
/// ([`SERIES_OWNER_MERGED`], which doctor reports). Idempotent; inside the caller's
/// write transaction. A record whose folder name keys to nothing stays as it is.
/// Returns the merges made.
pub(crate) fn rekey_series_folder_owners(conn: &Connection) -> Result<Vec<SeriesOwnerMerge>> {
    type Row = (i64, String, String, String, String);
    let rows: Vec<Row> = conn
        .prepare(
            "SELECT rowid, series_key, folder_name, owner_username, recorded_at \
             FROM series_folder_owners ORDER BY recorded_at, rowid",
        )?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut by_key: std::collections::BTreeMap<String, Vec<&Row>> = Default::default();
    for row in &rows {
        if let Some(key) = key_of(&row.2) {
            by_key.entry(key).or_default().push(row);
        }
    }
    let mut merges = Vec::new();
    for (key, group) in by_key {
        let kept = group[0];
        if group.len() == 1 && kept.1 == key {
            continue;
        }
        for row in &group {
            conn.execute("DELETE FROM series_folder_owners WHERE rowid = ?", [row.0])?;
        }
        conn.execute(
            "INSERT INTO series_folder_owners (series_key, folder_name, owner_username, \
             recorded_at) VALUES (?, ?, ?, ?) ON CONFLICT(series_key) DO UPDATE SET \
             folder_name = excluded.folder_name, owner_username = excluded.owner_username, \
             recorded_at = excluded.recorded_at",
            params![key, kept.2, kept.3, kept.4],
        )?;
        for row in &group[1..] {
            let merge = SeriesOwnerMerge {
                folder_name: row.2.clone(),
                owner: row.3.clone(),
                kept_folder: kept.2.clone(),
                kept_owner: kept.3.clone(),
            };
            tracing::warn!(
                "[LIBRARY] Series owner records: {:?} (owned by {}) is one series with {:?} \
                 (owned by {}) under the one filename fold; the earlier record, {:?}'s, is kept",
                merge.folder_name,
                merge.owner,
                merge.kept_folder,
                merge.kept_owner,
                merge.kept_folder
            );
            let details = crate::audit::AuditDetails::new()
                .with("kept_folder", merge.kept_folder.as_str())
                .with("kept_owner", merge.kept_owner.as_str());
            conn.execute(
                "INSERT INTO audit_logs (actor_username, action, target_type, target_path, \
                 target_username, details) VALUES (NULL, ?, 'series_folder', ?, ?, ?)",
                params![
                    SERIES_OWNER_MERGED,
                    merge.folder_name,
                    merge.owner,
                    details.to_json()
                ],
            )?;
            merges.push(merge);
        }
    }
    Ok(merges)
}

/// The merges [`rekey_series_folder_owners`] recorded in the audit log (still within
/// its retention), oldest first.
fn recorded_merges(conn: &Connection) -> Result<Vec<SeriesOwnerMerge>> {
    let rows: Vec<(String, String, Option<String>)> = conn
        .prepare(
            "SELECT target_path, target_username, details FROM audit_logs \
             WHERE action = ? ORDER BY id",
        )?
        .query_map([SERIES_OWNER_MERGED], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get(2)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows
        .into_iter()
        .map(|(folder_name, owner, details)| {
            let details: serde_json::Value = details
                .as_deref()
                .and_then(|d| serde_json::from_str(d).ok())
                .unwrap_or_default();
            let field = |k: &str| details[k].as_str().unwrap_or_default().to_string();
            SeriesOwnerMerge {
                folder_name,
                owner,
                kept_folder: field("kept_folder"),
                kept_owner: field("kept_owner"),
            }
        })
        .collect())
}

/// [`Database::series_owner_merges`] of the database file at `path`, opened read-only
/// (doctor: nothing is created or migrated); empty when there is no such file.
pub fn series_owner_merges_at(path: &std::path::Path) -> Result<Vec<SeriesOwnerMerge>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let has_audit = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'audit_logs'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !has_audit {
        return Ok(Vec::new());
    }
    recorded_merges(&conn)
}

impl Database {
    /// The series owner records the rekey at open merged into another (doctor's "Series
    /// owner records merged by the rekey"), oldest first.
    pub fn series_owner_merges(&self) -> Result<Vec<SeriesOwnerMerge>> {
        self.read(recorded_merges)
    }

    /// The recorded owner of the series folder `folder` (any spelling).
    pub fn series_folder_owner(&self, folder: &str) -> Result<Option<String>> {
        let Some(key) = key_of(folder) else {
            return Ok(None);
        };
        self.read(|conn| owner_at(conn, &key))
    }

    /// Record `user` as the owner of `folder`, replacing any record of its series.
    pub fn record_series_folder_owner(&self, folder: &str, user: &str) -> Result<()> {
        let Some(key) = key_of(folder) else {
            return Ok(());
        };
        self.write(|conn| put(conn, &key, folder, user))
    }

    /// Forget the owner of `folder`'s series; whether there was one.
    pub fn forget_series_folder_owner(&self, folder: &str) -> Result<bool> {
        let Some(key) = key_of(folder) else {
            return Ok(false);
        };
        self.write(|conn| forget(conn, &key))
    }

    /// A folder was renamed from `old` to `new`. The same series (a case-only rename)
    /// keeps its record under the new spelling. Otherwise the record moves to `new`'s
    /// series, except when `new` joins a folder of that series already listed
    /// (`joins`: the uploaders of the tracked volumes the moved folder holds): the
    /// joined series keeps its owner only when the moved part was that account's too,
    /// by its record, or, having none, by all of its tracked volumes (S1's merge rule,
    /// OWN-5). `old`'s record stays when another spelling of it is still listed
    /// (`old_lives_on`).
    pub fn move_series_folder_owner(
        &self,
        old: &str,
        new: &str,
        old_lives_on: bool,
        joins: Option<&[String]>,
    ) -> Result<()> {
        let (Some(old_key), Some(new_key)) = (key_of(old), key_of(new)) else {
            return Ok(());
        };
        self.write(|conn| {
            let moving = owner_at(conn, &old_key)?;
            if old_key == new_key {
                if moving.is_some() && !old_lives_on {
                    conn.execute(
                        "UPDATE series_folder_owners SET folder_name = ? WHERE series_key = ?",
                        params![new.trim_matches('/'), new_key],
                    )?;
                }
                return Ok(());
            }
            match (moving.as_deref(), joins) {
                (moving, Some(uploaders)) => {
                    let existing = owner_at(conn, &new_key)?;
                    if !joined_part_agrees(existing.as_deref(), moving, uploaders) {
                        forget(conn, &new_key)?;
                    }
                }
                (None, None) => {
                    forget(conn, &new_key)?;
                }
                (Some(m), None) => put(conn, &new_key, new, m)?,
            }
            if !old_lives_on {
                forget(conn, &old_key)?;
            }
            Ok(())
        })
    }

    /// A folder with no record of its own (one moved up from deeper in the library)
    /// joined the listed series `new`, holding tracked volumes of `uploaders`: the
    /// series keeps its owner only when they are all that owner's (OWN-5).
    pub fn series_folder_joined(&self, new: &str, uploaders: &[String]) -> Result<()> {
        let Some(key) = key_of(new) else {
            return Ok(());
        };
        self.write(|conn| {
            let existing = owner_at(conn, &key)?;
            if !joined_part_agrees(existing.as_deref(), None, uploaders) {
                forget(conn, &key)?;
            }
            Ok(())
        })
    }

    /// Every record, by folder name (code point order).
    pub fn list_series_folder_owners(&self) -> Result<Vec<SeriesFolderOwner>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT series_key, folder_name, owner_username FROM series_folder_owners \
                     ORDER BY folder_name",
                )?
                .query_map([], |r| {
                    Ok(SeriesFolderOwner {
                        series_key: r.get(0)?,
                        folder_name: r.get(1)?,
                        owner: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The folders `user` owns (its name in any ASCII case), by folder name.
    pub fn series_folders_owned_by(&self, user: &str) -> Result<Vec<SeriesFolderOwner>> {
        Ok(self
            .list_series_folder_owners()?
            .into_iter()
            .filter(|r| r.owner.eq_ignore_ascii_case(user))
            .collect())
    }

    /// The folders another account owns, by folder name: `user` may not add to them.
    pub fn series_folders_closed_to(&self, user: &str) -> Result<Vec<SeriesFolderOwner>> {
        Ok(self
            .list_series_folder_owners()?
            .into_iter()
            .filter(|r| !r.owner.eq_ignore_ascii_case(user))
            .collect())
    }

    /// The folders that existed before the owners table have not been given theirs yet.
    pub fn series_folder_adoption_pending(&self) -> Result<bool> {
        self.read(|conn| {
            Ok(conn
                .query_row("SELECT 1 FROM series_folder_adoption", [], |_| Ok(()))
                .optional()?
                .is_none())
        })
    }

    /// Record the adopted `(folder, owner)` pairs and mark adoption done, in one
    /// transaction (nothing when it was done meanwhile); how many were recorded. A
    /// series that got a record meanwhile (a folder an editor created while adoption
    /// waited) keeps it: its creator owns it.
    pub fn finish_series_folder_adoption(&self, owners: &[(String, String)]) -> Result<usize> {
        self.write(|conn| {
            let done = conn
                .query_row("SELECT 1 FROM series_folder_adoption", [], |_| Ok(()))
                .optional()?
                .is_some();
            if done {
                return Ok(0);
            }
            let mut recorded = 0;
            for (folder, user) in owners {
                if let Some(key) = key_of(folder) {
                    recorded += conn.execute(
                        "INSERT INTO series_folder_owners (series_key, folder_name, \
                         owner_username) VALUES (?, ?, ?) ON CONFLICT(series_key) DO NOTHING",
                        params![key, folder.trim_matches('/'), user],
                    )?;
                }
            }
            conn.execute(
                "INSERT INTO series_folder_adoption (id, done_at) VALUES (1, datetime('now'))",
                [],
            )?;
            Ok(recorded)
        })
    }

    /// `(volume_key, uploader)` of every tracked volume (adoption's input).
    pub fn tracked_volume_uploaders(&self) -> Result<Vec<(String, String)>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT volume_key, uploader_username FROM volume_uploads ORDER BY volume_key",
                )?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::temp_db;

    /// Records keyed before fold v2 (`straße` and `strasse` were two series) are rekeyed
    /// at the next open, in place: the earliest record of a series is kept, the other
    /// is reported (logged, audited, `series_owner_merges`), and a later open changes
    /// nothing.
    #[test]
    fn the_rekey_at_open_keeps_the_earliest_record() {
        let (dir, db) = temp_db();
        let path = dir.path().join("mokuro.db");
        db.write(|conn| {
            for (key, folder, owner, at) in [
                ("stra\u{df}e", "Stra\u{df}e", "alice", "2026-01-02 00:00:00"),
                ("strasse", "STRASSE", "bob", "2026-01-01 00:00:00"),
                ("dr stone", "Dr Stone", "carol", "2026-01-03 00:00:00"),
                ("x\u{200b}y", "X\u{200B}Y", "dave", "2026-01-04 00:00:00"),
            ] {
                conn.execute(
                    "INSERT INTO series_folder_owners (series_key, folder_name, \
                     owner_username, recorded_at) VALUES (?, ?, ?, ?)",
                    rusqlite::params![key, folder, owner, at],
                )?;
            }
            Ok(())
        })
        .unwrap();
        assert!(db.series_owner_merges().unwrap().is_empty());
        drop(db);
        let db = crate::Database::open(&path).unwrap();
        let rows = db.list_series_folder_owners().unwrap();
        let got: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|r| {
                (
                    r.series_key.as_str(),
                    r.folder_name.as_str(),
                    r.owner.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("dr stone", "Dr Stone", "carol"),
                ("strasse", "STRASSE", "bob"),
                ("xy", "X\u{200B}Y", "dave"),
            ]
        );
        assert_eq!(
            db.series_folder_owner("Stra\u{df}e").unwrap().as_deref(),
            Some("bob")
        );
        let merged = vec![super::SeriesOwnerMerge {
            folder_name: "Stra\u{df}e".into(),
            owner: "alice".into(),
            kept_folder: "STRASSE".into(),
            kept_owner: "bob".into(),
        }];
        assert_eq!(db.series_owner_merges().unwrap(), merged);
        drop(db);
        // Read-only, as doctor reads it; and a later open merges nothing more.
        assert_eq!(super::series_owner_merges_at(&path).unwrap(), merged);
        let db = crate::Database::open(&path).unwrap();
        assert_eq!(db.series_owner_merges().unwrap(), merged);
        assert_eq!(db.list_series_folder_owners().unwrap().len(), 3);
        assert!(
            super::series_owner_merges_at(&dir.path().join("none.db"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn one_record_for_every_spelling_of_a_series() {
        let (_dir, db) = temp_db();
        db.record_series_folder_owner("Dr Stone", "alice").unwrap();
        for spelling in ["Dr Stone", "dr  stone", " DR STONE ", "/Dr Stone/"] {
            assert_eq!(
                db.series_folder_owner(spelling).unwrap().as_deref(),
                Some("alice"),
                "{spelling}"
            );
        }
        db.record_series_folder_owner("Cafe\u{301}", "bob").unwrap();
        assert_eq!(
            db.series_folder_owner("Caf\u{e9}").unwrap().as_deref(),
            Some("bob")
        );
        assert_eq!(db.series_folder_owner("Dr_Stone").unwrap(), None);
        assert_eq!(db.series_folder_owner("").unwrap(), None);
        db.record_series_folder_owner("", "bob").unwrap();
        assert_eq!(db.list_series_folder_owners().unwrap().len(), 2);
        // A later record replaces the series' one (its spelling too).
        db.record_series_folder_owner("dr stone", "carol").unwrap();
        let rows = db.list_series_folder_owners().unwrap();
        let stone = rows.iter().find(|r| r.series_key == "dr stone").unwrap();
        assert_eq!(
            (stone.folder_name.as_str(), stone.owner.as_str()),
            ("dr stone", "carol")
        );
        assert!(db.forget_series_folder_owner("DR STONE").unwrap());
        assert!(!db.forget_series_folder_owner("DR STONE").unwrap());
        assert_eq!(db.series_folder_owner("Dr Stone").unwrap(), None);
    }

    #[test]
    fn owned_and_closed_ignore_the_case_of_the_name() {
        let (_dir, db) = temp_db();
        db.record_series_folder_owner("Zeta", "Alice").unwrap();
        db.record_series_folder_owner("Alpha", "alice").unwrap();
        db.record_series_folder_owner("Mid", "bob").unwrap();
        let names = |rows: Vec<super::SeriesFolderOwner>| -> Vec<String> {
            rows.into_iter().map(|r| r.folder_name).collect()
        };
        assert_eq!(
            names(db.series_folders_owned_by("ALICE").unwrap()),
            ["Alpha", "Zeta"]
        );
        assert_eq!(
            names(db.series_folders_closed_to("alice").unwrap()),
            ["Mid"]
        );
        assert_eq!(
            names(db.series_folders_closed_to("carol").unwrap()),
            ["Alpha", "Mid", "Zeta"]
        );
        assert!(db.series_folders_owned_by("carol").unwrap().is_empty());
    }

    #[test]
    fn a_record_follows_its_folder() {
        let (_dir, db) = temp_db();
        let owner = |f: &str| db.series_folder_owner(f).unwrap();
        // A case-only rename keeps the record under the new spelling.
        db.record_series_folder_owner("Kingdom", "alice").unwrap();
        db.move_series_folder_owner("Kingdom", "KINGDOM", false, None)
            .unwrap();
        let rows = db.list_series_folder_owners().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].folder_name, "KINGDOM");
        // Another series: the record moves.
        db.move_series_folder_owner("KINGDOM", "Kingdom (2006)", false, None)
            .unwrap();
        assert_eq!(owner("Kingdom"), None);
        assert_eq!(owner("kingdom (2006)").as_deref(), Some("alice"));
        // A spelling of the old series still listed keeps its record.
        db.record_series_folder_owner("One", "bob").unwrap();
        db.move_series_folder_owner("one", "Two", true, None)
            .unwrap();
        assert_eq!(owner("One").as_deref(), Some("bob"));
        assert_eq!(owner("Two").as_deref(), Some("bob"));
        // Joining an existing folder: an owner only when both had the same one.
        db.record_series_folder_owner("Three", "BOB").unwrap();
        db.move_series_folder_owner("Two", "three", false, Some(&[]))
            .unwrap();
        assert_eq!(owner("Three").as_deref(), Some("BOB"));
        assert_eq!(owner("Two"), None);
        db.record_series_folder_owner("Four", "carol").unwrap();
        db.move_series_folder_owner("Three", "Four", false, Some(&[]))
            .unwrap();
        assert_eq!(owner("Four"), None, "two owners: none");
        // An unowned folder moved over a stale record of its new name clears it.
        db.record_series_folder_owner("Five", "carol").unwrap();
        db.move_series_folder_owner("Unowned", "Five", false, None)
            .unwrap();
        assert_eq!(owner("Five"), None);
    }

    /// S1's merge rule for a folder that joins a listed series (OWN-5): the series
    /// keeps its owner only when the moved part was that account's, by its record or,
    /// having none, by every tracked volume it holds.
    #[test]
    fn a_joining_folder_keeps_an_owner_only_when_it_agrees() {
        let (_dir, db) = temp_db();
        let owner = |f: &str| db.series_folder_owner(f).unwrap();
        let users = |u: &[&str]| u.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // No record, nothing tracked (an empty folder): the owner stays.
        db.record_series_folder_owner("Six", "alice").unwrap();
        db.move_series_folder_owner("Loose", "six", false, Some(&[]))
            .unwrap();
        assert_eq!(owner("Six").as_deref(), Some("alice"));
        // No record, only the owner's volumes: it stays.
        db.move_series_folder_owner("Loose", "six", false, Some(&users(&["ALICE"])))
            .unwrap();
        assert_eq!(owner("Six").as_deref(), Some("alice"));
        // Another account's volume in it: none.
        db.move_series_folder_owner("Loose", "six", false, Some(&users(&["alice", "bob"])))
            .unwrap();
        assert_eq!(owner("Six"), None);
        // Moved up from deeper in the library.
        db.record_series_folder_owner("Seven", "alice").unwrap();
        db.series_folder_joined("seven", &users(&["alice"]))
            .unwrap();
        assert_eq!(owner("Seven").as_deref(), Some("alice"));
        db.series_folder_joined("seven", &[]).unwrap();
        assert_eq!(owner("Seven").as_deref(), Some("alice"));
        db.series_folder_joined("seven", &users(&["bob"])).unwrap();
        assert_eq!(owner("Seven"), None);
    }

    #[test]
    fn adoption_is_recorded_once_and_atomically() {
        let (_dir, db) = temp_db();
        // A new database tracks no volume: there is nothing to adopt.
        assert!(!db.series_folder_adoption_pending().unwrap());
        assert_eq!(
            db.finish_series_folder_adoption(&[("A".into(), "alice".into())])
                .unwrap(),
            0,
            "already done"
        );
        assert_eq!(db.series_folder_owner("A").unwrap(), None);
        db.with_writer_connection(|c| c.execute("DELETE FROM series_folder_adoption", []))
            .unwrap();
        assert!(db.series_folder_adoption_pending().unwrap());
        // Created while adoption waited: its creator keeps it.
        db.record_series_folder_owner("C", "carol").unwrap();
        let owners = [
            ("A".to_string(), "alice".to_string()),
            ("B".to_string(), "bob".to_string()),
            ("c".to_string(), "alice".to_string()),
        ];
        assert_eq!(db.finish_series_folder_adoption(&owners).unwrap(), 2);
        assert!(!db.series_folder_adoption_pending().unwrap());
        assert_eq!(db.series_folder_owner("b").unwrap().as_deref(), Some("bob"));
        assert_eq!(
            db.series_folder_owner("C").unwrap().as_deref(),
            Some("carol")
        );
        assert_eq!(db.finish_series_folder_adoption(&owners).unwrap(), 0);
    }

    #[test]
    fn tracked_uploaders_are_adoptions_input() {
        let (_dir, db) = temp_db();
        db.record_volume_upload("S/V1.cbz", "alice").unwrap();
        db.record_volume_upload("loose.cbz", "bob").unwrap();
        db.record_volume_upload("S/V1.mokuro", "bob").unwrap();
        assert_eq!(
            db.tracked_volume_uploaders().unwrap(),
            [
                ("S/V1.cbz".to_string(), "alice".to_string()),
                ("loose.cbz".to_string(), "bob".to_string())
            ]
        );
    }
}
