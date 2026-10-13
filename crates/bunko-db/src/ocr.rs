//! OCR sidecar provenance, `ocr_sidecars` (spec §10): who wrote each sidecar file on disk
//! and with what. One row per file, replaced whole on a re-run, deleted with the file.
//!
//! `engine`/`detector`/`precision` are opaque strings: rows written by 0.5.2 may name
//! engines and detectors this build no longer ships (mokuro, ctd, rtdetr, animetext);
//! they round-trip untouched. Reproduced: `record_ocr_sidecar` keys by the path as given
//! (0.5.2 did not strip it there) while the readers/deleters strip `/`; prefix matching
//! is exact (`substr`), and a folder rename uses `UPDATE OR REPLACE`.
//!
//! `written_sha256` (0.8, the only column 0.5.x lacks) is the sha256 of the exact bytes
//! Bunko installed; a row 0.5.x writes leaves it NULL.

use crate::database::Database;
use crate::error::Result;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

/// 0.5.2 `OcrSidecarRow`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OcrSidecar {
    /// Library-relative sidecar path (primary key).
    pub sidecar_path: String,
    /// Library-relative `.cbz` path of its archive.
    pub volume_key: String,
    pub generation_id: String,
    pub generation_name: String,
    /// `local` for this server, else the processor's name.
    pub machine: String,
    /// The processor's login (`None` for this server).
    pub account: Option<String>,
    pub engine: Option<String>,
    pub detector: Option<String>,
    pub precision: Option<String>,
    pub runner_build: Option<String>,
    pub pages: Option<i64>,
    pub failed_pages: Option<i64>,
    pub archive_size: Option<i64>,
    pub archive_mtime_ns: Option<i64>,
    /// `YYYY-MM-DD HH:MM:SS` UTC; set by the database on record (ignored on input).
    pub written_at: String,
    /// Lowercase hex sha256 of the exact bytes Bunko installed at this path (0.8; `None`
    /// for a row an older build or a 0.5 rollback wrote). The proof the generation
    /// upgrade asks for before it treats the file as Bunko's own output.
    pub written_sha256: Option<String>,
}

impl OcrSidecar {
    fn from_row(r: &Row<'_>) -> rusqlite::Result<OcrSidecar> {
        Ok(OcrSidecar {
            sidecar_path: r.get(0)?,
            volume_key: r.get(1)?,
            generation_id: r.get(2)?,
            generation_name: r.get(3)?,
            machine: r.get(4)?,
            account: r.get(5)?,
            engine: r.get(6)?,
            detector: r.get(7)?,
            precision: r.get(8)?,
            runner_build: r.get(9)?,
            pages: r.get(10)?,
            failed_pages: r.get(11)?,
            archive_size: r.get(12)?,
            archive_mtime_ns: r.get(13)?,
            written_at: r.get(14)?,
            written_sha256: r.get(15)?,
        })
    }
}

const OCR_COLUMNS: &str = "sidecar_path, volume_key, generation_id, generation_name, machine, \
    account, engine, detector, precision, runner_build, pages, failed_pages, archive_size, \
    archive_mtime_ns, written_at, written_sha256";

/// `S/Vol 1` of the archive key `S/Vol 1.cbz` (`.cbz` in any case).
fn archive_stem(key: &str) -> Option<String> {
    let cut = key.len().checked_sub(4)?;
    (key.is_char_boundary(cut) && key[cut..].eq_ignore_ascii_case(".cbz"))
        .then(|| key[..cut].to_string())
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
    /// Upsert the row of one sidecar just written (`written_at` = now).
    pub fn record_ocr_sidecar(&self, row: &OcrSidecar) -> Result<()> {
        self.write(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO ocr_sidecars (sidecar_path, volume_key, generation_id, \
                 generation_name, machine, account, engine, detector, precision, runner_build, \
                 pages, failed_pages, archive_size, archive_mtime_ns, written_sha256, written_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now'))",
                params![
                    row.sidecar_path,
                    row.volume_key,
                    row.generation_id,
                    row.generation_name,
                    row.machine,
                    row.account,
                    row.engine,
                    row.detector,
                    row.precision,
                    row.runner_build,
                    row.pages,
                    row.failed_pages,
                    row.archive_size,
                    row.archive_mtime_ns,
                    row.written_sha256
                ],
            )?;
            Ok(())
        })
    }

    /// The row of one library-relative sidecar path, or `None` (unknown producer).
    pub fn get_ocr_sidecar(&self, sidecar_path: &str) -> Result<Option<OcrSidecar>> {
        let path = sidecar_path.trim_matches('/');
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {OCR_COLUMNS} FROM ocr_sidecars WHERE sidecar_path = ?"
                ))?
                .query_row([path], OcrSidecar::from_row)
                .optional()?)
        })
    }

    /// Every row, oldest write first.
    pub fn list_ocr_sidecars(&self) -> Result<Vec<OcrSidecar>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {OCR_COLUMNS} FROM ocr_sidecars ORDER BY written_at, rowid"
                ))?
                .query_map([], OcrSidecar::from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// `(generation_id, volume_key, machine)` of every row, oldest write first.
    pub fn ocr_sidecar_producers(&self) -> Result<Vec<(String, String, String)>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT generation_id, volume_key, machine FROM ocr_sidecars \
                     ORDER BY written_at, rowid",
                )?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The sidecar at this path is gone (or replaced); rows deleted.
    pub fn forget_ocr_sidecar(&self, sidecar_path: &str) -> Result<usize> {
        let path = sidecar_path.trim_matches('/');
        self.write(|conn| {
            Ok(conn.execute("DELETE FROM ocr_sidecars WHERE sidecar_path = ?", [path])?)
        })
    }

    /// Every row of one archive (library-relative `.cbz` path).
    pub fn forget_ocr_sidecars_of_volume(&self, volume_key: &str) -> Result<usize> {
        let key = volume_key.trim_matches('/');
        self.write(|conn| Ok(conn.execute("DELETE FROM ocr_sidecars WHERE volume_key = ?", [key])?))
    }

    /// Every row under a folder (exact prefix).
    pub fn forget_ocr_sidecars_under_prefix(&self, library_prefix: &str) -> Result<usize> {
        let Some((head, n)) = head(library_prefix) else {
            return Ok(0);
        };
        self.write(|conn| {
            Ok(conn.execute(
                "DELETE FROM ocr_sidecars WHERE substr(sidecar_path, 1, ?) = ?",
                params![n, head],
            )?)
        })
    }

    /// One archive moved (a case merge, spec §5.9): the rows of `old_volume_key` follow
    /// it to `new_volume_key`, each `sidecar_path` re-spelled under the new archive's
    /// stem (colliding rows are replaced). The per-volume form of
    /// [`Database::rename_ocr_sidecars_under_prefix`]: a merge never moves a whole folder
    /// prefix, because blocked volumes may stay behind.
    pub fn rename_ocr_sidecars_of_volume(
        &self,
        old_volume_key: &str,
        new_volume_key: &str,
    ) -> Result<usize> {
        let (old_key, new_key) = (
            old_volume_key.trim_matches('/'),
            new_volume_key.trim_matches('/'),
        );
        let (Some(old_stem), Some(new_stem)) = (archive_stem(old_key), archive_stem(new_key))
        else {
            return Ok(0);
        };
        if old_key == new_key || old_key.is_empty() || new_key.is_empty() {
            return Ok(0);
        }
        let n = old_stem.chars().count() as i64;
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE OR REPLACE ocr_sidecars SET \
                 sidecar_path = ? || substr(sidecar_path, ?), volume_key = ? \
                 WHERE volume_key = ? AND substr(sidecar_path, 1, ?) = ?",
                params![new_stem, n + 1, new_key, old_key, n, old_stem],
            )?)
        })
    }

    /// One sidecar file moved (a case merge moving a layer in under the winner's
    /// stem): its row follows, now belonging to `new_volume_key`.
    pub fn rename_ocr_sidecar(
        &self,
        old_path: &str,
        new_path: &str,
        new_volume_key: &str,
    ) -> Result<usize> {
        let (old, new, key) = (
            old_path.trim_matches('/'),
            new_path.trim_matches('/'),
            new_volume_key.trim_matches('/'),
        );
        if old == new || old.is_empty() || new.is_empty() {
            return Ok(0);
        }
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE OR REPLACE ocr_sidecars SET sidecar_path = ?, volume_key = ? \
                 WHERE sidecar_path = ?",
                params![new, key, old],
            )?)
        })
    }

    /// A folder moved with its sidecars: the rows follow (colliding rows are replaced).
    pub fn rename_ocr_sidecars_under_prefix(
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
                "UPDATE OR REPLACE ocr_sidecars SET \
                 sidecar_path = ? || substr(sidecar_path, ?), \
                 volume_key = ? || substr(volume_key, ?) \
                 WHERE substr(sidecar_path, 1, ?) = ?",
                params![new_head, n + 1, new_head, n + 1, n, old_head],
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_db;

    fn row(path: &str, volume: &str, gen_id: &str) -> OcrSidecar {
        OcrSidecar {
            sidecar_path: path.into(),
            volume_key: volume.into(),
            generation_id: gen_id.into(),
            generation_name: "Hayai".into(),
            machine: "local".into(),
            detector: Some("ctd".into()),
            pages: Some(10),
            archive_mtime_ns: Some(1_700_000_000_123_456_789),
            ..Default::default()
        }
    }

    #[test]
    fn record_get_list_forget() {
        let (_dir, db) = temp_db();
        let mut first = row("S/V1.mokuro", "S/V1.cbz", "g1");
        first.written_sha256 = Some("ab".repeat(32));
        db.record_ocr_sidecar(&first).unwrap();
        db.record_ocr_sidecar(&row("S/V1.hayai.mokuro.gz", "S/V1.cbz", "g2"))
            .unwrap();
        db.record_ocr_sidecar(&row("T/V1.mokuro", "T/V1.cbz", "g1"))
            .unwrap();
        let got = db.get_ocr_sidecar("/S/V1.mokuro/").unwrap().unwrap();
        assert_eq!(got.detector.as_deref(), Some("ctd"));
        assert_eq!(got.archive_mtime_ns, Some(1_700_000_000_123_456_789));
        assert_eq!(got.written_at.len(), 19);
        assert_eq!(got.written_sha256, Some("ab".repeat(32)));
        assert_eq!(db.list_ocr_sidecars().unwrap().len(), 3);
        assert_eq!(
            db.ocr_sidecar_producers().unwrap()[0],
            (
                "g1".to_string(),
                "S/V1.cbz".to_string(),
                "local".to_string()
            )
        );
        let mut again = row("S/V1.mokuro", "S/V1.cbz", "g9");
        again.account = Some("proc".into());
        db.record_ocr_sidecar(&again).unwrap();
        let again = db.get_ocr_sidecar("S/V1.mokuro").unwrap().unwrap();
        assert_eq!(again.generation_id, "g9");
        assert_eq!(again.written_sha256, None, "the row is replaced whole");
        assert_eq!(
            db.list_ocr_sidecars().unwrap().len(),
            3,
            "replaced, not added"
        );
        assert_eq!(db.forget_ocr_sidecar("/T/V1.mokuro").unwrap(), 1);
        assert_eq!(db.forget_ocr_sidecars_of_volume("S/V1.cbz/").unwrap(), 2);
    }

    #[test]
    fn prefixes() {
        let (_dir, db) = temp_db();
        let mut proven = row("Dr Stone/V1.mokuro", "Dr Stone/V1.cbz", "g");
        proven.written_sha256 = Some("cd".repeat(32));
        db.record_ocr_sidecar(&proven).unwrap();
        db.record_ocr_sidecar(&row("Dr_Stone/V1.mokuro", "Dr_Stone/V1.cbz", "g"))
            .unwrap();
        db.record_ocr_sidecar(&row("New/V1.mokuro", "New/V1.cbz", "old"))
            .unwrap();
        assert_eq!(
            db.rename_ocr_sidecars_under_prefix("Dr Stone", "New")
                .unwrap(),
            1
        );
        let moved = db.get_ocr_sidecar("New/V1.mokuro").unwrap().unwrap();
        assert_eq!(moved.volume_key, "New/V1.cbz");
        assert_eq!(moved.generation_id, "g", "the colliding row was replaced");
        assert_eq!(
            moved.written_sha256,
            Some("cd".repeat(32)),
            "its proof moves too"
        );
        assert_eq!(db.rename_ocr_sidecars_under_prefix("X", "X").unwrap(), 0);
        assert_eq!(db.forget_ocr_sidecars_under_prefix("Dr_Stone").unwrap(), 1);
        assert_eq!(db.forget_ocr_sidecars_under_prefix("").unwrap(), 0);
        assert_eq!(db.list_ocr_sidecars().unwrap().len(), 1);
    }

    #[test]
    fn per_volume_renames_leave_the_folder_alone() {
        let (_dir, db) = temp_db();
        for (path, vol) in [
            ("kingdom/Vol 1.mokuro", "kingdom/Vol 1.cbz"),
            ("kingdom/Vol 1.hayai.mokuro.gz", "kingdom/Vol 1.cbz"),
            ("kingdom/Vol 2.mokuro", "kingdom/Vol 2.cbz"),
            ("kingdom/Vol 10.mokuro", "kingdom/Vol 10.cbz"),
        ] {
            db.record_ocr_sidecar(&row(path, vol, "g")).unwrap();
        }
        assert_eq!(
            db.rename_ocr_sidecars_of_volume("kingdom/Vol 1.cbz", "Kingdom/Vol 1.cbz")
                .unwrap(),
            2
        );
        let moved = db
            .get_ocr_sidecar("Kingdom/Vol 1.hayai.mokuro.gz")
            .unwrap()
            .unwrap();
        assert_eq!(moved.volume_key, "Kingdom/Vol 1.cbz");
        assert!(
            db.get_ocr_sidecar("kingdom/Vol 2.mokuro")
                .unwrap()
                .is_some()
        );
        assert!(
            db.get_ocr_sidecar("kingdom/Vol 10.mokuro")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            db.rename_ocr_sidecar(
                "kingdom/Vol 2.mokuro",
                "Kingdom/Vol 2.mokuro",
                "Kingdom/Vol 2.cbz"
            )
            .unwrap(),
            1
        );
        let one = db.get_ocr_sidecar("Kingdom/Vol 2.mokuro").unwrap().unwrap();
        assert_eq!(one.volume_key, "Kingdom/Vol 2.cbz");
        assert_eq!(
            db.rename_ocr_sidecars_of_volume("a.zip", "b.zip").unwrap(),
            0
        );
    }
}
