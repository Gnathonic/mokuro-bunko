//! `volume_identities` (spec §10.4): the `volume_uuid` each archive's primary `.mokuro`
//! last carried, kept after that file is gone so a primary made again names the same
//! volume (readers' progress knows volumes by that id). Reproduced as 0.5.2, including the
//! write-only-when-changed upsert and exact-prefix folder operations.

use crate::database::Database;
use crate::error::Result;
use crate::pyfmt::{self, truthy};
use crate::uploads::normalize_volume_key;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value};

/// `_identity_from_entry`: the id a compiled entry published FROM ITS `.mokuro`, else
/// `None` — `volume_uuid` must be a non-blank string, and the entry must come from a
/// parsed sidecar (`mokuro_sha256` truthy, or `mokuro_size` present and `mokuro_version`
/// truthy for entries from before the hash).
pub fn identity_from_entry(entry: &Map<String, Value>) -> Option<String> {
    let Some(Value::String(uuid)) = entry.get("volume_uuid") else {
        return None;
    };
    if pyfmt::strip(uuid).is_empty() {
        return None;
    }
    let has_sha = entry.get("mokuro_sha256").is_some_and(truthy);
    let has_size = entry.get("mokuro_size").is_some_and(|v| !v.is_null());
    let has_version = entry.get("mokuro_version").is_some_and(truthy);
    (has_sha || (has_size && has_version)).then(|| uuid.clone())
}

/// `_upsert_volume_identity`: no write when the uuid is unchanged.
pub(crate) fn upsert_volume_identity(
    conn: &Connection,
    volume_key: &str,
    uuid: &str,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO volume_identities (volume_key, volume_uuid, recorded_at) \
         VALUES (?, ?, datetime('now')) \
         ON CONFLICT(volume_key) DO UPDATE SET \
         volume_uuid = excluded.volume_uuid, \
         recorded_at = excluded.recorded_at \
         WHERE volume_uuid != excluded.volume_uuid",
    )?
    .execute(params![volume_key, uuid])?;
    Ok(())
}

fn head(prefix: &str) -> Option<(String, i64)> {
    let p = prefix.trim_matches('/');
    if p.is_empty() {
        return None;
    }
    let h = format!("{p}/");
    let n = h.chars().count() as i64;
    Some((h, n))
}

impl Database {
    /// Keep the id this archive's primary `.mokuro` carries (newest wins). Keyed by the
    /// archive; a sidecar path maps to it.
    pub fn remember_volume_uuid(
        &self,
        library_relative_path: &str,
        volume_uuid: &str,
    ) -> Result<()> {
        let Some(key) = normalize_volume_key(library_relative_path) else {
            return Ok(());
        };
        if pyfmt::strip(volume_uuid).is_empty() {
            return Ok(());
        }
        self.write(|conn| upsert_volume_identity(conn, &key, volume_uuid))
    }

    /// The id this archive's primary last carried.
    pub fn remembered_volume_uuid(&self, library_relative_path: &str) -> Result<Option<String>> {
        let Some(key) = normalize_volume_key(library_relative_path) else {
            return Ok(None);
        };
        self.read(|conn| {
            Ok(conn
                .prepare_cached("SELECT volume_uuid FROM volume_identities WHERE volume_key = ?")?
                .query_row([&key], |r| r.get(0))
                .optional()?)
        })
    }

    /// The archive is gone: a new one starts fresh.
    pub fn forget_volume_uuid(&self, library_relative_path: &str) -> Result<()> {
        let Some(key) = normalize_volume_key(library_relative_path) else {
            return Ok(());
        };
        self.write(|conn| {
            conn.execute("DELETE FROM volume_identities WHERE volume_key = ?", [key])?;
            Ok(())
        })
    }

    /// Every archive under a folder (exact prefix).
    pub fn forget_volume_uuids_under_prefix(&self, library_prefix: &str) -> Result<usize> {
        let Some((head, n)) = head(library_prefix) else {
            return Ok(0);
        };
        self.write(|conn| {
            Ok(conn.execute(
                "DELETE FROM volume_identities WHERE substr(volume_key, 1, ?) = ?",
                params![n, head],
            )?)
        })
    }

    /// A folder moved: what its archives were known by follows.
    pub fn rename_volume_uuids_under_prefix(
        &self,
        old_prefix: &str,
        new_prefix: &str,
    ) -> Result<usize> {
        let (Some((old_head, n)), Some((new_head, _))) = (head(old_prefix), head(new_prefix))
        else {
            return Ok(0);
        };
        if old_head == new_head {
            return Ok(0);
        }
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE OR REPLACE volume_identities SET volume_key = ? || substr(volume_key, ?) \
                 WHERE substr(volume_key, 1, ?) = ?",
                params![new_head, n + 1, n, old_head],
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_db;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn identity_rules() {
        assert_eq!(
            identity_from_entry(&obj(json!({"volume_uuid": "u1", "mokuro_sha256": "abc"})))
                .as_deref(),
            Some("u1")
        );
        assert_eq!(
            identity_from_entry(&obj(
                json!({"volume_uuid": "u1", "mokuro_size": 0, "mokuro_version": "0.2"})
            ))
            .as_deref(),
            Some("u1")
        );
        assert!(
            identity_from_entry(&obj(
                json!({"volume_uuid": "u1", "mokuro_size": null, "mokuro_version": "0.2"})
            ))
            .is_none()
        );
        assert!(
            identity_from_entry(&obj(json!({"volume_uuid": "u1", "mokuro_sha256": ""}))).is_none()
        );
        assert!(
            identity_from_entry(&obj(json!({"volume_uuid": "  ", "mokuro_sha256": "x"}))).is_none()
        );
        assert!(
            identity_from_entry(&obj(json!({"volume_uuid": 5, "mokuro_sha256": "x"}))).is_none()
        );
    }

    #[test]
    fn remember_forget_rename() {
        let (_dir, db) = temp_db();
        db.remember_volume_uuid("S/V1.mokuro", "u1").unwrap();
        db.remember_volume_uuid("S/V2.cbz", "u2").unwrap();
        db.remember_volume_uuid("S/V3.cbz", " ").unwrap();
        db.remember_volume_uuid("S/readme.txt", "u9").unwrap();
        assert_eq!(
            db.remembered_volume_uuid("S/V1.cbz").unwrap().as_deref(),
            Some("u1")
        );
        assert_eq!(db.remembered_volume_uuid("S/V3.cbz").unwrap(), None);
        let stamp = |db: &Database| -> String {
            db.with_writer_connection(|c| {
                c.query_row(
                    "SELECT recorded_at FROM volume_identities WHERE volume_key='S/V1.cbz'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap()
        };
        db.with_writer_connection(|c| {
            c.execute("UPDATE volume_identities SET recorded_at = 'old'", [])
        })
        .unwrap();
        db.remember_volume_uuid("S/V1.cbz", "u1").unwrap();
        assert_eq!(stamp(&db), "old", "unchanged uuid: no write");
        db.remember_volume_uuid("S/V1.cbz", "u1b").unwrap();
        assert_ne!(stamp(&db), "old");
        assert_eq!(db.rename_volume_uuids_under_prefix("S", "T").unwrap(), 2);
        assert_eq!(
            db.remembered_volume_uuid("T/V2.cbz").unwrap().as_deref(),
            Some("u2")
        );
        db.forget_volume_uuid("T/V2.webp").unwrap();
        assert_eq!(db.remembered_volume_uuid("T/V2.cbz").unwrap(), None);
        assert_eq!(db.forget_volume_uuids_under_prefix("/T/").unwrap(), 1);
    }
}
