//! Volume ownership (`volume_uploads`) and the series ownership derived from it (spec §9).
//!
//! Reproduced: volume keys are the library-relative `.cbz` path, case-preserving and
//! byte-exact; only an ARCHIVE upload creates ownership (a sidecar only stamps
//! `last_modified_*` of an existing row); the original uploader survives re-uploads and
//! renames; OCR layer files (`<stem>.<layer>.mokuro[.gz]`) belong to `<stem>.cbz`; series
//! ownership compares folder names by the reader's fold (NFC, trim, collapse whitespace,
//! lowercase — not casefold); a series is editable only by the sole owner of every tracked
//! volume, never when untracked.
//!
//! Choices:
//! - `forget_volume_uploads_under_prefix` matches the folder prefix EXACTLY
//!   (`substr(volume_key, 1, n) = 'prefix/'`), as the OCR tables already do. 0.5.2 used an
//!   unescaped, ASCII-case-insensitive `LIKE 'prefix/%'`, which also deleted rows of
//!   other folders (`Dr_Stone/` matched `Dr Stone/` and `dr stone/`) — spec open question
//!   4, fixed.
//! - `record_volume_upload` has no `existed_before` argument: 0.5.2's two branches were
//!   identical (open question 13).

use crate::database::Database;
use crate::error::Result;
use crate::pyfmt;
use rusqlite::{OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};
use unicode_normalization::UnicodeNormalization;

/// Case-insensitive (Python `lower()`) suffix test that returns the text before the
/// suffix, slicing the ORIGINAL string by the suffix's character count as Python does.
fn strip_suffix_ci<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let n = suffix.chars().count();
    let tail: Vec<(usize, char)> = s.char_indices().rev().take(n).collect();
    if tail.len() != n {
        return None;
    }
    for ((_, c), want) in tail.iter().zip(suffix.chars().rev()) {
        let mut lower = c.to_lowercase();
        if lower.next() != Some(want) || lower.next().is_some() {
            return None;
        }
    }
    Some(&s[..tail.last().map_or(s.len(), |(i, _)| *i)])
}

/// 0.5.2 `normalize_volume_key_from_library_relative`: the `.cbz` key a library-relative
/// path belongs to (`S/V1.mokuro.gz` -> `S/V1.cbz`), or `None` for other files.
pub fn normalize_volume_key(path: &str) -> Option<String> {
    let cleaned = path.trim_matches('/');
    if cleaned.is_empty() {
        return None;
    }
    if strip_suffix_ci(cleaned, ".cbz").is_some() {
        return Some(cleaned.to_string());
    }
    for suffix in [".mokuro.gz", ".mokuro", ".webp", ".nocover"] {
        if let Some(stem) = strip_suffix_ci(cleaned, suffix) {
            return Some(format!("{stem}.cbz"));
        }
    }
    None
}

/// `S/Vol 1.cbz` for the OCR layer file `S/Vol 1.hayai-nova.mokuro[.gz]`, else `None`.
/// The layer id has 1-32 of `[a-z0-9-]` with at least one letter, so a decimal volume
/// number (`Vol 01.5.mokuro`) is not a layer.
pub fn layer_sidecar_volume_path(path: &str) -> Option<String> {
    let name = path.strip_suffix(".gz").unwrap_or(path);
    let middle = name.strip_suffix(".mokuro")?;
    let cut = middle.rfind('.')?;
    if cut == 0 {
        return None;
    }
    let layer = &middle[cut + 1..];
    let ok = (1..=32).contains(&layer.len())
        && layer
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && layer.bytes().any(|b| b.is_ascii_lowercase());
    if !ok || middle[cut..].contains('/') {
        return None;
    }
    Some(format!("{}.cbz", &middle[..cut]))
}

/// The series-identity fold (`_fold_series_title_key` = the reader's
/// `normalize_volume_title_key`): NFC, trim, collapse whitespace runs to one space,
/// lowercase.
pub fn fold_series_title_key(title: &str) -> String {
    let normalized: String = title.nfc().collect();
    let mut out = String::with_capacity(normalized.len());
    let mut in_space = false;
    for c in pyfmt::strip(&normalized).chars() {
        if pyfmt::is_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out.to_lowercase()
}

impl Database {
    /// Record an upload/edit of a library file. An archive creates ownership (keeping the
    /// first uploader on conflict); a sidecar only stamps an existing row.
    pub fn record_volume_upload(&self, library_relative_path: &str, username: &str) -> Result<()> {
        let Some(key) = normalize_volume_key(library_relative_path) else {
            return Ok(());
        };
        let is_archive = strip_suffix_ci(library_relative_path.trim_matches('/'), ".cbz").is_some();
        self.write(|conn| {
            if is_archive {
                conn.execute(
                    "INSERT INTO volume_uploads (volume_key, uploader_username, last_modified_by, \
                     last_modified_at) VALUES (?, ?, ?, datetime('now')) \
                     ON CONFLICT(volume_key) DO UPDATE SET \
                     last_modified_by = excluded.last_modified_by, \
                     last_modified_at = datetime('now')",
                    params![key, username, username],
                )?;
            } else {
                conn.execute(
                    "UPDATE volume_uploads SET last_modified_by = ?, \
                     last_modified_at = datetime('now') WHERE volume_key = ?",
                    params![username, key],
                )?;
            }
            Ok(())
        })
    }

    /// The uploader of the volume a library-relative path belongs to.
    pub fn get_volume_owner(&self, library_relative_path: &str) -> Result<Option<String>> {
        let Some(key) = normalize_volume_key(library_relative_path) else {
            return Ok(None);
        };
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT uploader_username FROM volume_uploads WHERE volume_key = ?",
                )?
                .query_row([&key], |r| r.get(0))
                .optional()?)
        })
    }

    /// True when `username` uploaded the volume this `/mokuro-reader/...` path belongs to
    /// (layer files included). The reader root and top-level folders never qualify.
    pub fn can_user_delete_library_path(&self, username: &str, virtual_path: &str) -> Result<bool> {
        let Some(rest) = virtual_path.strip_prefix("/mokuro-reader/") else {
            return Ok(false);
        };
        let relative = rest.trim_matches('/');
        if relative.is_empty() || (!relative.contains('/') && !relative.contains('.')) {
            return Ok(false);
        }
        let mut owner = self.get_volume_owner(relative)?;
        if owner.is_none()
            && let Some(parent) = layer_sidecar_volume_path(relative)
        {
            owner = self.get_volume_owner(&parent)?;
        }
        Ok(owner.as_deref() == Some(username))
    }

    /// `(folder, uploader)` of every tracked volume inside a folder.
    fn volume_upload_folder_owners(&self) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT DISTINCT uploader_username, volume_key FROM volume_uploads",
                )?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })?;
        Ok(rows
            .into_iter()
            .filter_map(|(user, key)| {
                key.split_once('/')
                    .map(|(folder, _)| (folder.to_string(), user))
            })
            .collect())
    }

    /// Distinct uploaders of a series folder's tracked volumes (folded name equality).
    pub fn series_owners(&self, series_title: &str) -> Result<BTreeSet<String>> {
        let prefix = series_title.trim_matches('/');
        if prefix.is_empty() {
            return Ok(BTreeSet::new());
        }
        let target = fold_series_title_key(prefix);
        Ok(self
            .volume_upload_folder_owners()?
            .into_iter()
            .filter(|(folder, _)| fold_series_title_key(folder) == target)
            .map(|(_, user)| user)
            .collect())
    }

    /// True when `username` owns EVERY tracked volume of the series folder (and there is
    /// at least one).
    pub fn can_user_edit_series(&self, username: &str, series_title: &str) -> Result<bool> {
        let owners = self.series_owners(series_title)?;
        Ok(owners.len() == 1 && owners.contains(username))
    }

    /// Every raw folder spelling of the series `username` may edit, sorted by code point
    /// (`/login/api/me` `ownedSeries`). One pass over the table.
    pub fn list_series_owned_by(&self, username: &str) -> Result<Vec<String>> {
        let mut by_key: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
        for (folder, owner) in self.volume_upload_folder_owners()? {
            let entry = by_key.entry(fold_series_title_key(&folder)).or_default();
            entry.0.insert(owner);
            entry.1.insert(folder);
        }
        let owned: BTreeSet<String> = by_key
            .into_values()
            .filter(|(owners, _)| owners.len() == 1 && owners.contains(username))
            .flat_map(|(_, folders)| folders)
            .collect();
        Ok(owned.into_iter().collect())
    }

    /// Delete the ownership row of the volume this path belongs to.
    pub fn forget_volume_upload(&self, library_relative_path: &str) -> Result<()> {
        let Some(key) = normalize_volume_key(library_relative_path) else {
            return Ok(());
        };
        self.write(|conn| {
            conn.execute("DELETE FROM volume_uploads WHERE volume_key = ?", [key])?;
            Ok(())
        })
    }

    /// Delete the ownership rows of every volume under a folder (exact prefix); how many.
    pub fn forget_volume_uploads_under_prefix(&self, library_prefix: &str) -> Result<usize> {
        let prefix = library_prefix.trim_matches('/');
        if prefix.is_empty() {
            return Ok(0);
        }
        let head = format!("{prefix}/");
        self.write(|conn| {
            Ok(conn.execute(
                "DELETE FROM volume_uploads WHERE substr(volume_key, 1, ?) = ?",
                params![head.chars().count() as i64, head],
            )?)
        })
    }

    /// Move an ownership row when a volume's path changes (uploader and upload time kept).
    pub fn rename_volume_upload(
        &self,
        old_library_relative: &str,
        new_library_relative: &str,
    ) -> Result<()> {
        let (Some(old_key), Some(new_key)) = (
            normalize_volume_key(old_library_relative),
            normalize_volume_key(new_library_relative),
        ) else {
            return Ok(());
        };
        if old_key == new_key {
            return Ok(());
        }
        self.write(|conn| {
            let row: Option<(String, String, Option<String>)> = conn
                .query_row(
                    "SELECT uploader_username, uploaded_at, last_modified_by FROM volume_uploads \
                     WHERE volume_key = ?",
                    [&old_key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((uploader, uploaded_at, last_by)) = row else {
                return Ok(());
            };
            conn.execute(
                "INSERT INTO volume_uploads (volume_key, uploader_username, uploaded_at, \
                 last_modified_by, last_modified_at) VALUES (?, ?, ?, ?, datetime('now')) \
                 ON CONFLICT(volume_key) DO UPDATE SET \
                 uploader_username = excluded.uploader_username, \
                 uploaded_at = excluded.uploaded_at, \
                 last_modified_by = excluded.last_modified_by, \
                 last_modified_at = datetime('now')",
                params![new_key, uploader, uploaded_at, last_by],
            )?;
            conn.execute(
                "DELETE FROM volume_uploads WHERE volume_key = ?",
                [&old_key],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_db;

    #[test]
    fn volume_keys() {
        assert_eq!(
            normalize_volume_key("/S/V1.cbz/").as_deref(),
            Some("S/V1.cbz")
        );
        assert_eq!(
            normalize_volume_key("S/V1.CBZ").as_deref(),
            Some("S/V1.CBZ")
        );
        assert_eq!(
            normalize_volume_key("S/V1.mokuro.gz").as_deref(),
            Some("S/V1.cbz")
        );
        assert_eq!(
            normalize_volume_key("S/V1.MOKURO").as_deref(),
            Some("S/V1.cbz")
        );
        assert_eq!(
            normalize_volume_key("S/V1.webp").as_deref(),
            Some("S/V1.cbz")
        );
        assert_eq!(
            normalize_volume_key("S/V1.nocover").as_deref(),
            Some("S/V1.cbz")
        );
        assert_eq!(
            normalize_volume_key("S/V1.hayai.mokuro").as_deref(),
            Some("S/V1.hayai.cbz")
        );
        // Kelvin sign lowercases to ASCII 'k', as in Python.
        assert_eq!(
            normalize_volume_key("S/V1.mo\u{212a}uro").as_deref(),
            Some("S/V1.cbz")
        );
        assert_eq!(normalize_volume_key("S/notes.txt"), None);
        assert_eq!(normalize_volume_key("//"), None);
    }

    #[test]
    fn layers() {
        assert_eq!(
            layer_sidecar_volume_path("S/Vol 1.hayai-nova.mokuro").as_deref(),
            Some("S/Vol 1.cbz")
        );
        assert_eq!(
            layer_sidecar_volume_path("S/Vol 1.hayai-nova.mokuro.gz").as_deref(),
            Some("S/Vol 1.cbz")
        );
        assert_eq!(layer_sidecar_volume_path("S/Vol 01.5.mokuro"), None);
        assert_eq!(layer_sidecar_volume_path("S/Vol 1.mokuro"), None);
        assert_eq!(layer_sidecar_volume_path("S/Vol 1.Hayai.mokuro"), None);
        assert_eq!(layer_sidecar_volume_path(".x.mokuro"), None);
        assert_eq!(layer_sidecar_volume_path("a.b/c.mokuro"), None);
    }

    #[test]
    fn folds() {
        assert_eq!(fold_series_title_key("  Dr   Stone "), "dr stone");
        assert_eq!(
            fold_series_title_key("Cafe\u{301}"),
            fold_series_title_key("Caf\u{e9}")
        );
        assert_ne!(
            fold_series_title_key("Stra\u{df}e"),
            fold_series_title_key("STRASSE")
        );
        assert_eq!(fold_series_title_key("A\u{3000}\u{1c}B"), "a b");
    }

    #[test]
    fn ownership() {
        let (_dir, db) = temp_db();
        db.record_volume_upload("S/V1.cbz", "alice").unwrap();
        db.record_volume_upload("S/V1.cbz", "bob").unwrap();
        assert_eq!(
            db.get_volume_owner("S/V1.mokuro").unwrap().as_deref(),
            Some("alice")
        );
        db.record_volume_upload("T/V1.mokuro", "bob").unwrap();
        assert_eq!(
            db.get_volume_owner("T/V1.cbz").unwrap(),
            None,
            "a sidecar never creates ownership"
        );
        assert!(
            db.can_user_delete_library_path("alice", "/mokuro-reader/S/V1.cbz")
                .unwrap()
        );
        assert!(
            db.can_user_delete_library_path("alice", "/mokuro-reader/S/V1.webp")
                .unwrap()
        );
        assert!(
            db.can_user_delete_library_path("alice", "/mokuro-reader/S/V1.hayai-nova.mokuro.gz")
                .unwrap()
        );
        assert!(
            !db.can_user_delete_library_path("bob", "/mokuro-reader/S/V1.cbz")
                .unwrap()
        );
        assert!(
            !db.can_user_delete_library_path("alice", "/mokuro-reader/S")
                .unwrap()
        );
        assert!(
            !db.can_user_delete_library_path("alice", "/other/S/V1.cbz")
                .unwrap()
        );
        let last_by: String = db
            .with_writer_connection(|c| {
                c.query_row("SELECT last_modified_by FROM volume_uploads", [], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(last_by, "bob");
    }

    #[test]
    fn series_ownership() {
        let (_dir, db) = temp_db();
        db.record_volume_upload("Dr Stone/V1.cbz", "alice").unwrap();
        db.record_volume_upload("dr  stone/V2.cbz", "alice")
            .unwrap();
        db.record_volume_upload("Mixed/V1.cbz", "alice").unwrap();
        db.record_volume_upload("Mixed/V2.cbz", "bob").unwrap();
        db.record_volume_upload("loose.cbz", "alice").unwrap();
        db.record_volume_upload("Caf\u{e9}/V1.cbz", "alice")
            .unwrap();
        assert!(db.can_user_edit_series("alice", "DR STONE").unwrap());
        assert!(
            !db.can_user_edit_series("alice", "Dr_Stone").unwrap(),
            "no wildcard matching"
        );
        assert!(!db.can_user_edit_series("alice", "Mixed").unwrap());
        assert!(!db.can_user_edit_series("bob", "Mixed").unwrap());
        assert!(!db.can_user_edit_series("alice", "Untracked").unwrap());
        assert!(!db.can_user_edit_series("alice", "").unwrap());
        assert!(
            db.can_user_edit_series("alice", "Cafe\u{301}").unwrap(),
            "NFD title, NFC folder"
        );
        assert_eq!(
            db.list_series_owned_by("alice").unwrap(),
            ["Caf\u{e9}", "Dr Stone", "dr  stone"]
        );
        assert!(db.list_series_owned_by("bob").unwrap().is_empty());
    }

    #[test]
    fn forget_and_rename() {
        let (_dir, db) = temp_db();
        db.record_volume_upload("Dr Stone/V1.cbz", "alice").unwrap();
        db.record_volume_upload("Dr_Stone/V1.cbz", "bob").unwrap();
        db.record_volume_upload("dr stone/V1.cbz", "carol").unwrap();
        assert_eq!(
            db.forget_volume_uploads_under_prefix("/Dr_Stone/").unwrap(),
            1,
            "exact prefix only"
        );
        assert_eq!(
            db.get_volume_owner("Dr Stone/V1.cbz").unwrap().as_deref(),
            Some("alice")
        );
        assert_eq!(db.forget_volume_uploads_under_prefix("/").unwrap(), 0);
        db.rename_volume_upload("Dr Stone/V1.cbz", "Dr Stone/Vol 1.cbz")
            .unwrap();
        assert_eq!(db.get_volume_owner("Dr Stone/V1.cbz").unwrap(), None);
        assert_eq!(
            db.get_volume_owner("Dr Stone/Vol 1.cbz")
                .unwrap()
                .as_deref(),
            Some("alice")
        );
        db.rename_volume_upload("nope.cbz", "x.cbz").unwrap();
        db.forget_volume_upload("Dr Stone/Vol 1.mokuro").unwrap();
        assert_eq!(db.get_volume_owner("Dr Stone/Vol 1.cbz").unwrap(), None);
    }
}
