//! Cross-area reads for the overview and the popular-series table (design §10): what
//! the reading and crawl areas' tables hold, summed. Empty tables read as zeros, so
//! these answer before those areas collect anything.

use super::{AnalyticsDb, uint};
use crate::error::Result;
use rusqlite::params;

/// Reading over a span of days (design §6 definitions).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReadingTotals {
    /// (user, volume, day) rows.
    pub reads: u64,
    pub readers: u64,
    pub minutes: f64,
    pub pages: u64,
}

/// A series' reading over a span of days.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesReading {
    pub series_key: String,
    pub title: String,
    pub readers: u64,
    pub reads: u64,
    pub pages: u64,
    pub minutes: f64,
}

impl AnalyticsDb {
    pub fn reading_totals(&self, from_day: i64, to_day: i64) -> Result<ReadingTotals> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT COUNT(*), COUNT(DISTINCT username), COALESCE(SUM(dwell_ms), 0), \
                 COALESCE(SUM(pages), 0) FROM reading_daily WHERE day >= ?1 AND day <= ?2",
            )?
            .query_row(params![from_day, to_day], |r| {
                Ok(ReadingTotals {
                    reads: uint(r.get(0)?),
                    readers: uint(r.get(1)?),
                    minutes: r.get::<_, i64>(2)? as f64 / 60000.0,
                    pages: uint(r.get(3)?),
                })
            })?)
        })
    }

    /// Reading per series in days `from..=to` (rows without a series are left out).
    pub fn series_reading(&self, from_day: i64, to_day: i64) -> Result<Vec<SeriesReading>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT series_key, MAX(COALESCE(series_title, '')), COUNT(DISTINCT username), \
                 COUNT(*), COALESCE(SUM(pages), 0), COALESCE(SUM(dwell_ms), 0) \
                 FROM reading_daily WHERE day >= ?1 AND day <= ?2 AND series_key != '' \
                 GROUP BY series_key",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(SeriesReading {
                        series_key: r.get(0)?,
                        title: r.get(1)?,
                        readers: uint(r.get(2)?),
                        reads: uint(r.get(3)?),
                        pages: uint(r.get(4)?),
                        minutes: r.get::<_, i64>(5)? as f64 / 60000.0,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Crawl episodes active in `[from_s, to_s)` (open or closed).
    pub fn crawl_episodes_between(&self, from_s: i64, to_s: i64) -> Result<u64> {
        self.read(|c| {
            Ok(uint(
                c.prepare_cached(
                    "SELECT COUNT(*) FROM crawl_episodes WHERE last_at >= ?1 AND started_at < ?2",
                )?
                .query_row(params![from_s, to_s], |r| r.get(0))?,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::analytics::testutil::temp_db;

    #[test]
    fn empty_area_tables_read_as_zeros() {
        let (_d, db) = temp_db();
        let t = db.reading_totals(0, 100).unwrap();
        assert_eq!((t.reads, t.readers, t.pages), (0, 0, 0));
        assert!(db.series_reading(0, 100).unwrap().is_empty());
        assert_eq!(db.crawl_episodes_between(0, 100).unwrap(), 0);
        db.read(|c| {
            c.execute_batch(
                "INSERT INTO reading_daily (day, username, volume_uuid, series_key, series_title, pages, dwell_ms) \
                 VALUES (5, 'alice', 'u1', 's', 'S', 10, 120000), (6, 'bob', 'u2', 's', 'S', 4, 60000)",
            )
            .map_err(Into::into)
        })
        .unwrap_err(); // the read pool is query_only
        db.write(|tx| {
            tx.conn.execute_batch(
                "INSERT INTO reading_daily (day, username, volume_uuid, series_key, series_title, pages, dwell_ms) \
                 VALUES (5, 'alice', 'u1', 's', 'S', 10, 120000), (6, 'bob', 'u2', 's', 'S', 4, 60000)",
            )?;
            Ok(())
        })
        .unwrap();
        let t = db.reading_totals(0, 100).unwrap();
        assert_eq!((t.reads, t.readers, t.pages), (2, 2, 14));
        assert_eq!(t.minutes, 3.0);
        let s = db.series_reading(0, 100).unwrap();
        assert_eq!(s[0].readers, 2);
    }
}
