//! The library's shape (design §7): the latest file-type scan, a daily growth
//! snapshot, the page formats inside archives, and the series list the dormant-series
//! query runs against. The scanner (bunko-server `analytics::library`) writes them;
//! the admin API reads them.
//!
//! `archive_pages.pages` is the archive's page-image count, or [`UNREADABLE`] when its
//! zip directory could not be read (a `.cbr`/`.rar` that is really RAR, a damaged
//! file): such a row still carries its size and mtime, so the archive is not tried
//! again until it changes.

use super::{AnalyticsDb, AnalyticsTx, int, uint};
use crate::Database;
use crate::error::Result;
use rusqlite::{OptionalExtension, params};

pub(crate) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS library_scan (
        id INTEGER PRIMARY KEY CHECK (id = 1), scanned_at INTEGER NOT NULL, json TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS library_daily (
        day INTEGER PRIMARY KEY, series INTEGER NOT NULL DEFAULT 0,
        volumes INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0, json TEXT
    )",
    // `formats`: JSON `{ext: count}`.
    "CREATE TABLE IF NOT EXISTS archive_pages (
        volume_key TEXT PRIMARY KEY, size INTEGER NOT NULL, mtime INTEGER NOT NULL,
        pages INTEGER NOT NULL DEFAULT 0, formats TEXT NOT NULL DEFAULT '{}',
        scanned_at INTEGER NOT NULL
    )",
    // The series of the latest scan (one row per `fold_series_title_key`), replaced
    // whole by each scan: what "dormant" is measured against without parsing the scan
    // JSON of a 10k-series library on every request.
    "CREATE TABLE IF NOT EXISTS library_series (
        series_key TEXT PRIMARY KEY, title TEXT NOT NULL,
        volumes INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0
    ) WITHOUT ROWID",
];

/// `archive_pages.pages` of an archive whose zip directory could not be read.
pub const UNREADABLE: i64 = -1;

/// `meta` key: archives the latest scan found still to read for page formats (kept
/// current as the reads land, so a restart reports what is really left).
pub const PAGES_PENDING_KEY: &str = "library_pages_pending";

/// One `library_daily` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryDay {
    pub day: i64,
    pub series: u64,
    pub volumes: u64,
    pub bytes: u64,
    pub json: Option<String>,
}

/// One `library_series` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySeries {
    pub series_key: String,
    pub title: String,
    pub volumes: u64,
    pub bytes: u64,
}

/// One `archive_pages` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePages {
    pub volume_key: String,
    pub size: u64,
    /// Modification time, epoch nanoseconds.
    pub mtime: i64,
    /// Page images, or [`UNREADABLE`].
    pub pages: i64,
    /// JSON `{format: pages}`.
    pub formats: String,
    pub scanned_at: i64,
}

/// The page formats over every read archive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageFormatTotals {
    /// Archives whose directory was read.
    pub scanned: u64,
    pub unreadable: u64,
    /// `(format, pages, archives holding it)`, most pages first.
    pub formats: Vec<(String, u64, u64)>,
}

/// One `uploads_daily` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadDay {
    pub day: i64,
    pub username: String,
    pub volumes: u64,
    pub bytes: u64,
}

/// A volume from the main database's `volume_uploads` (who uploaded what, when).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeUpload {
    pub volume_key: String,
    pub username: String,
    /// Epoch seconds (`None` when the stored text is not a date).
    pub uploaded_at: Option<i64>,
}

impl AnalyticsTx<'_> {
    pub fn set_library_scan(&self, scanned_at: i64, json: &str) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO library_scan (id, scanned_at, json) VALUES (1, ?1, ?2) \
                 ON CONFLICT (id) DO UPDATE SET scanned_at = excluded.scanned_at, json = excluded.json",
            )?
            .execute(params![scanned_at, json])?;
        Ok(())
    }

    /// The day's growth snapshot: the latest scan of the day wins.
    pub fn set_library_day(&self, d: &LibraryDay) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO library_daily (day, series, volumes, bytes, json) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT (day) DO UPDATE SET series = excluded.series, volumes = excluded.volumes, \
                 bytes = excluded.bytes, json = excluded.json",
            )?
            .execute(params![
                d.day,
                int(d.series),
                int(d.volumes),
                int(d.bytes),
                d.json
            ])?;
        Ok(())
    }

    /// Replace the series list with `rows` (a scan's whole result).
    pub fn replace_library_series(&self, rows: &[LibrarySeries]) -> Result<()> {
        self.conn.execute("DELETE FROM library_series", [])?;
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO library_series (series_key, title, volumes, bytes) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (series_key) DO UPDATE SET volumes = volumes + excluded.volumes, \
             bytes = bytes + excluded.bytes",
        )?;
        for r in rows {
            stmt.execute(params![r.series_key, r.title, int(r.volumes), int(r.bytes)])?;
        }
        Ok(())
    }

    pub fn put_archive_pages(&self, r: &ArchivePages) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO archive_pages (volume_key, size, mtime, pages, formats, scanned_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (volume_key) DO UPDATE SET \
                 size = excluded.size, mtime = excluded.mtime, pages = excluded.pages, \
                 formats = excluded.formats, scanned_at = excluded.scanned_at",
            )?
            .execute(params![
                r.volume_key,
                int(r.size),
                r.mtime,
                r.pages,
                r.formats,
                r.scanned_at
            ])?;
        Ok(())
    }

    pub fn delete_archive_pages(&self, keys: &[String]) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare_cached("DELETE FROM archive_pages WHERE volume_key = ?1")?;
        for k in keys {
            stmt.execute([k])?;
        }
        Ok(())
    }
}

impl AnalyticsDb {
    /// `(scanned_at, json)` of the latest scan.
    pub fn library_scan(&self) -> Result<Option<(i64, String)>> {
        self.read(|c| {
            Ok(
                c.prepare_cached("SELECT scanned_at, json FROM library_scan WHERE id = 1")?
                    .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?,
            )
        })
    }

    /// Growth snapshots of days `from..=to`, plus the last one before `from` (first,
    /// when there is one) so a chart can carry it into days without a scan.
    pub fn library_days(&self, from_day: i64, to_day: i64) -> Result<Vec<LibraryDay>> {
        let row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<LibraryDay> {
            Ok(LibraryDay {
                day: r.get(0)?,
                series: uint(r.get(1)?),
                volumes: uint(r.get(2)?),
                bytes: uint(r.get(3)?),
                json: r.get(4)?,
            })
        };
        self.read(|c| {
            let mut out: Vec<LibraryDay> = c
                .prepare_cached(
                    "SELECT day, series, volumes, bytes, json FROM library_daily \
                     WHERE day < ?1 ORDER BY day DESC LIMIT 1",
                )?
                .query_row([from_day], row)
                .optional()?
                .into_iter()
                .collect();
            let mut stmt = c.prepare_cached(
                "SELECT day, series, volumes, bytes, json FROM library_daily \
                 WHERE day >= ?1 AND day <= ?2 ORDER BY day",
            )?;
            for r in stmt.query_map(params![from_day, to_day], row)? {
                out.push(r?);
            }
            Ok(out)
        })
    }

    /// `(size, mtime)` of each key's `archive_pages` row, in order (`None`: no row).
    pub fn archive_stamps(&self, keys: &[&str]) -> Result<Vec<Option<(u64, i64)>>> {
        self.read(|c| {
            let mut stmt =
                c.prepare_cached("SELECT size, mtime FROM archive_pages WHERE volume_key = ?1")?;
            keys.iter()
                .map(|k| {
                    Ok(stmt
                        .query_row([k], |r| Ok((uint(r.get(0)?), r.get(1)?)))
                        .optional()?)
                })
                .collect()
        })
    }

    /// Up to `limit` `archive_pages` keys after `after`, in key order (a bounded walk
    /// over every row, for dropping the archives that are gone).
    pub fn archive_page_keys_after(&self, after: &str, limit: usize) -> Result<Vec<String>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT volume_key FROM archive_pages WHERE volume_key > ?1 ORDER BY volume_key LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![after, limit as i64], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
    }

    pub fn archive_pages_of(&self, volume_key: &str) -> Result<Option<ArchivePages>> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT volume_key, size, mtime, pages, formats, scanned_at FROM archive_pages \
                 WHERE volume_key = ?1",
            )?
            .query_row([volume_key], |r| {
                Ok(ArchivePages {
                    volume_key: r.get(0)?,
                    size: uint(r.get(1)?),
                    mtime: r.get(2)?,
                    pages: r.get(3)?,
                    formats: r.get(4)?,
                    scanned_at: r.get(5)?,
                })
            })
            .optional()?)
        })
    }

    /// Pages per image format over every readable archive, and the archive counts.
    pub fn page_format_totals(&self) -> Result<PageFormatTotals> {
        self.read(|c| {
            let (scanned, unreadable): (i64, i64) = c
                .prepare_cached(
                    "SELECT COALESCE(SUM(pages >= 0), 0), COALESCE(SUM(pages < 0), 0) FROM archive_pages",
                )?
                .query_row([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            let mut stmt = c.prepare_cached(
                "SELECT j.key, SUM(j.value), COUNT(*) FROM archive_pages a, json_each(a.formats) j \
                 WHERE a.pages >= 0 AND json_valid(a.formats) GROUP BY j.key \
                 ORDER BY SUM(j.value) DESC, j.key",
            )?;
            let formats = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        uint(r.get(1)?),
                        uint(r.get(2)?),
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(PageFormatTotals {
                scanned: uint(scanned),
                unreadable: uint(unreadable),
                formats,
            })
        })
    }

    /// `uploads_daily` rows of days `from..=to`.
    pub fn uploads_between(&self, from_day: i64, to_day: i64) -> Result<Vec<UploadDay>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT day, username, volumes, bytes FROM uploads_daily \
                 WHERE day >= ?1 AND day <= ?2 ORDER BY day, username",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(UploadDay {
                        day: r.get(0)?,
                        username: r.get(1)?,
                        volumes: uint(r.get(2)?),
                        bytes: uint(r.get(3)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The series of the latest scan with no download and no reading on or after
    /// `since_day`: `(how many, the first `limit` by volumes)`.
    pub fn dormant_series(
        &self,
        since_day: i64,
        limit: usize,
    ) -> Result<(u64, Vec<LibrarySeries>)> {
        const WHERE: &str = "WHERE NOT EXISTS (SELECT 1 FROM volume_downloads d \
             WHERE d.series_key = s.series_key AND d.day >= ?1) \
             AND NOT EXISTS (SELECT 1 FROM reading_daily r \
             WHERE r.series_key = s.series_key AND r.day >= ?1)";
        self.read(|c| {
            let count: i64 = c
                .prepare_cached(&format!("SELECT COUNT(*) FROM library_series s {WHERE}"))?
                .query_row([since_day], |r| r.get(0))?;
            let mut stmt = c.prepare_cached(&format!(
                "SELECT series_key, title, volumes, bytes FROM library_series s {WHERE} \
                 ORDER BY volumes DESC, title LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(params![since_day, limit as i64], |r| {
                    Ok(LibrarySeries {
                        series_key: r.get(0)?,
                        title: r.get(1)?,
                        volumes: uint(r.get(2)?),
                        bytes: uint(r.get(3)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok((uint(count), rows))
        })
    }

    /// How many series the latest scan listed.
    pub fn library_series_count(&self) -> Result<u64> {
        self.read(|c| {
            Ok(uint(
                c.prepare_cached("SELECT COUNT(*) FROM library_series")?
                    .query_row([], |r| r.get(0))?,
            ))
        })
    }
}

/// `uploaded_at` (SQLite `datetime('now')` text, UTC) as epoch seconds.
const UPLOADED_AT_S: &str = "CAST(strftime('%s', uploaded_at) AS INTEGER)";

impl Database {
    /// Volumes first uploaded on days `from..=to` (UTC), per uploader: `(day,
    /// username, volumes)`. The main database keeps who uploaded each volume that is
    /// still in the library, whatever the client (web UI, reader, any WebDAV client),
    /// so this covers the days before analytics existed.
    pub fn volume_uploads_per_day(
        &self,
        from_day: i64,
        to_day: i64,
    ) -> Result<Vec<(i64, String, u64)>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "SELECT {UPLOADED_AT_S} / 86400 AS day, uploader_username, COUNT(*) \
                 FROM volume_uploads WHERE day IS NOT NULL AND day >= ?1 AND day <= ?2 \
                 GROUP BY day, uploader_username ORDER BY day, uploader_username"
            ))?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok((r.get(0)?, r.get(1)?, uint(r.get(2)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The UTC day of the oldest recorded upload.
    pub fn first_volume_upload_day(&self) -> Result<Option<i64>> {
        self.read(|c| {
            Ok(c.prepare_cached(&format!(
                "SELECT MIN({UPLOADED_AT_S}) / 86400 FROM volume_uploads"
            ))?
            .query_row([], |r| r.get::<_, Option<i64>>(0))?)
        })
    }

    /// The `limit` most recently uploaded volumes.
    pub fn recent_volume_uploads(&self, limit: usize) -> Result<Vec<VolumeUpload>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(&format!(
                "SELECT volume_key, uploader_username, {UPLOADED_AT_S} FROM volume_uploads \
                 ORDER BY uploaded_at DESC, volume_key LIMIT ?1"
            ))?;
            let rows = stmt
                .query_map([limit as i64], |r| {
                    Ok(VolumeUpload {
                        volume_key: r.get(0)?,
                        username: r.get(1)?,
                        uploaded_at: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::temp_db;
    use super::*;

    fn pages(key: &str, pages: i64, formats: &str) -> ArchivePages {
        ArchivePages {
            volume_key: key.into(),
            size: 10,
            mtime: 5,
            pages,
            formats: formats.into(),
            scanned_at: 1,
        }
    }

    #[test]
    fn scan_daily_and_series_round_trip() {
        let (_d, db) = temp_db();
        assert!(db.library_scan().unwrap().is_none());
        db.write(|tx| {
            tx.set_library_scan(100, r#"{"a":1}"#)?;
            tx.set_library_scan(200, r#"{"a":2}"#)?;
            for (day, v) in [(10, 1), (12, 3), (12, 4), (15, 9)] {
                tx.set_library_day(&LibraryDay {
                    day,
                    series: 1,
                    volumes: v,
                    bytes: v * 100,
                    json: None,
                })?;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(
            db.library_scan().unwrap(),
            Some((200, r#"{"a":2}"#.to_string()))
        );
        let days = db.library_days(11, 14).unwrap();
        // The last snapshot before the range comes first; the day's latest scan wins.
        assert_eq!(
            days.iter().map(|d| (d.day, d.volumes)).collect::<Vec<_>>(),
            [(10, 1), (12, 4)]
        );
        let series = |k: &str, v: u64| LibrarySeries {
            series_key: k.into(),
            title: k.to_uppercase(),
            volumes: v,
            bytes: v,
        };
        db.write(|tx| tx.replace_library_series(&[series("a", 1), series("b", 2)]))
            .unwrap();
        db.write(|tx| tx.replace_library_series(&[series("b", 2), series("c", 3)]))
            .unwrap();
        assert_eq!(db.library_series_count().unwrap(), 2);
    }

    #[test]
    fn page_formats_sum_readable_archives_and_count_unreadable_ones() {
        let (_d, db) = temp_db();
        db.write(|tx| {
            tx.put_archive_pages(&pages("S/a.cbz", 3, r#"{"jpeg":2,"png":1}"#))?;
            tx.put_archive_pages(&pages("S/b.cbz", 4, r#"{"jpeg":4}"#))?;
            tx.put_archive_pages(&pages("S/c.cbr", UNREADABLE, "{}"))?;
            Ok(())
        })
        .unwrap();
        let t = db.page_format_totals().unwrap();
        assert_eq!((t.scanned, t.unreadable), (2, 1));
        assert_eq!(
            t.formats,
            [("jpeg".to_string(), 6, 2), ("png".to_string(), 1, 1)]
        );
        assert_eq!(
            db.archive_stamps(&["S/a.cbz", "S/zzz.cbz"]).unwrap(),
            [Some((10, 5)), None]
        );
        assert_eq!(
            db.archive_page_keys_after("", 2).unwrap(),
            ["S/a.cbz", "S/b.cbz"]
        );
        assert_eq!(
            db.archive_page_keys_after("S/b.cbz", 2).unwrap(),
            ["S/c.cbr"]
        );
        db.write(|tx| tx.delete_archive_pages(&["S/a.cbz".into()]))
            .unwrap();
        let t = db.page_format_totals().unwrap();
        assert_eq!(t.formats, [("jpeg".to_string(), 4, 1)]);
        assert!(db.archive_pages_of("S/a.cbz").unwrap().is_none());
    }

    #[test]
    fn dormant_series_have_no_recent_download_or_read() {
        let (_d, db) = temp_db();
        let series = |k: &str, v: u64| LibrarySeries {
            series_key: k.into(),
            title: k.into(),
            volumes: v,
            bytes: 0,
        };
        db.write(|tx| {
            tx.replace_library_series(&[
                series("downloaded", 1),
                series("read", 2),
                series("old", 3),
                series("never", 4),
            ])?;
            tx.conn.execute_batch(
                "INSERT INTO volume_downloads (day, principal, volume_key, series_key, series_title) \
                 VALUES (100, 'u:a', 'downloaded/v.cbz', 'downloaded', 'downloaded'), \
                        (10, 'u:a', 'old/v.cbz', 'old', 'old'); \
                 INSERT INTO reading_daily (day, username, volume_uuid, series_key) \
                 VALUES (95, 'a', 'x', 'read');",
            )?;
            Ok(())
        })
        .unwrap();
        let (n, rows) = db.dormant_series(90, 10).unwrap();
        assert_eq!(n, 2);
        assert_eq!(
            rows.iter()
                .map(|r| r.series_key.as_str())
                .collect::<Vec<_>>(),
            ["never", "old"]
        );
        let (n, rows) = db.dormant_series(90, 1).unwrap();
        assert_eq!((n, rows.len()), (2, 1));
    }

    #[test]
    fn uploads_read_per_day() {
        let (_d, db) = temp_db();
        db.write(|tx| {
            tx.add_uploads(5, "alice", 2, 200)?;
            tx.add_uploads(6, "bob", 1, 50)?;
            tx.add_uploads(9, "bob", 1, 50)
        })
        .unwrap();
        let rows = db.uploads_between(5, 6).unwrap();
        assert_eq!(
            rows.iter()
                .map(|u| (u.day, u.username.as_str(), u.volumes))
                .collect::<Vec<_>>(),
            [(5, "alice", 2), (6, "bob", 1)]
        );
    }

    #[test]
    fn main_database_uploads_per_day_and_recent() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_with(
            dir.path().join("mokuro.db"),
            &crate::DbOptions {
                bcrypt_cost: 4,
                ..Default::default()
            },
        )
        .unwrap();
        db.record_volume_upload("S/V1.cbz", "alice").unwrap();
        db.record_volume_upload("S/V2.cbz", "alice").unwrap();
        db.record_volume_upload("T/V1.cbz", "bob").unwrap();
        db.write(|c| {
            c.execute_batch(
                "UPDATE volume_uploads SET uploaded_at = '1970-01-03 10:00:00' WHERE volume_key = 'S/V1.cbz'",
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(db.first_volume_upload_day().unwrap(), Some(2));
        assert_eq!(
            db.volume_uploads_per_day(0, 5).unwrap(),
            [(2, "alice".to_string(), 1)]
        );
        let today = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            / 86400;
        let now = db.volume_uploads_per_day(today - 1, today + 1).unwrap();
        assert_eq!(now.iter().map(|r| r.2).sum::<u64>(), 2);
        let recent = db.recent_volume_uploads(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert!(recent.iter().all(|r| r.volume_key != "S/V1.cbz"));
        assert_eq!(
            db.recent_volume_uploads(10).unwrap()[2].uploaded_at,
            Some(2 * 86400 + 10 * 3600)
        );
    }
}
