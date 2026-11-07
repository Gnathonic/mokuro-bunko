//! Series folder owners (0.8, `series_folder_owners`): who controls a top-level library
//! folder `/mokuro-reader/<Series>/`.
//!
//! The account whose request created a folder owns it for good; a folder that existed
//! before the table gets the one uploader of its tracked volumes, once
//! ([`Database::finish_series_folder_adoption`]). Records are keyed by the series-identity
//! fold ([`fold_series_title_key`]), so `Dr Stone/`, `dr  stone/` and an NFD spelling are
//! one folder. Owner names compare ignoring ASCII case, as account names do.
//!
//! A record says nothing about whether its folder still exists: the server checks the
//! listing before it acts on one (a folder can go without a hook: a crash, a delete in
//! the cloud while offline), and a later creator of the folder replaces it.

use crate::database::Database;
use crate::error::Result;
use crate::uploads::fold_series_title_key;
use rusqlite::{Connection, OptionalExtension, params};

/// [`Database::move_series_folder_owner`] inside the caller's transaction.
pub(crate) fn move_series_folder_owner_in(
    conn: &Connection,
    old: &str,
    new: &str,
    old_lives_on: bool,
    joins: Option<&[String]>,
) -> Result<()> {
    let (Some(old_key), Some(new_key)) = (key_of(old), key_of(new)) else {
        return Ok(());
    };
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
}

/// One owned folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesFolderOwner {
    pub series_key: String,
    /// A spelling of the folder (the one recorded), to report.
    pub folder_name: String,
    pub owner: String,
}

/// The record key of a folder name, `None` for a name that folds to nothing.
pub(crate) fn key_of(folder: &str) -> Option<String> {
    let key = fold_series_title_key(folder.trim_matches('/'));
    (!key.is_empty()).then_some(key)
}

pub(crate) fn owner_at(conn: &Connection, key: &str) -> Result<Option<String>> {
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

impl Database {
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
        self.write(|conn| move_series_folder_owner_in(conn, old, new, old_lives_on, joins))
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
