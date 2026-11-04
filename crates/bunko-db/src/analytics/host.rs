//! The host (design §8): process memory, CPU, connections and requests in flight, per
//! minute and per hour. The foundation creates the tables; the sampler (bunko-server
//! `analytics::host`) writes them through the statements here.
//!
//! The sampler upserts the current minute after every 15 s sample (so a crash loses at
//! most one sample) and re-derives that minute's hour from its minutes in the same
//! transaction, so `host_hourly` is current too and needs no separate rollup pass.

use super::{AnalyticsDb, AnalyticsTx, int, uint};
use crate::error::Result;
use rusqlite::{OptionalExtension, Row, params};

pub(crate) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS host_minute (
        minute INTEGER PRIMARY KEY, rss_avg INTEGER, rss_max INTEGER, cpu_pct REAL,
        connections_max INTEGER NOT NULL DEFAULT 0, inflight_max INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE TABLE IF NOT EXISTS host_hourly (
        hour INTEGER PRIMARY KEY, rss_avg INTEGER, rss_max INTEGER, cpu_avg REAL,
        cpu_max REAL, connections_max INTEGER NOT NULL DEFAULT 0,
        inflight_max INTEGER NOT NULL DEFAULT 0
    )",
];

/// One `host_minute` row. `None`: the platform has no reader for it (or the minute's
/// only sample had no CPU interval yet).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostMinute {
    /// Epoch seconds / 60.
    pub minute: i64,
    pub rss_avg: Option<u64>,
    pub rss_max: Option<u64>,
    /// Percent of one core.
    pub cpu_pct: Option<f64>,
    pub connections_max: u64,
    pub inflight_max: u64,
}

/// One `host_hourly` row.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostHour {
    /// Epoch seconds / 3600.
    pub hour: i64,
    pub rss_avg: Option<u64>,
    pub rss_max: Option<u64>,
    pub cpu_avg: Option<f64>,
    pub cpu_max: Option<f64>,
    pub connections_max: u64,
    pub inflight_max: u64,
}

const MINUTE_COLUMNS: &str =
    "minute, rss_avg, rss_max, cpu_pct, connections_max, inflight_max FROM host_minute";

fn minute_row(r: &Row<'_>) -> rusqlite::Result<HostMinute> {
    Ok(HostMinute {
        minute: r.get(0)?,
        rss_avg: r.get::<_, Option<i64>>(1)?.map(uint),
        rss_max: r.get::<_, Option<i64>>(2)?.map(uint),
        cpu_pct: r.get(3)?,
        connections_max: uint(r.get(4)?),
        inflight_max: uint(r.get(5)?),
    })
}

impl AnalyticsTx<'_> {
    /// Write the minute (its average and CPU replace what an earlier sample of it
    /// wrote; its peaks only grow, so a restart within the minute keeps the earlier
    /// process's) and recompute its hour from that hour's minutes.
    pub fn put_host_minute(&self, m: &HostMinute) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO host_minute (minute, rss_avg, rss_max, cpu_pct, connections_max, \
                 inflight_max) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (minute) DO UPDATE SET rss_avg = excluded.rss_avg, \
                 rss_max = CASE WHEN rss_max IS NULL THEN excluded.rss_max \
                 WHEN excluded.rss_max IS NULL THEN rss_max \
                 ELSE MAX(rss_max, excluded.rss_max) END, cpu_pct = excluded.cpu_pct, \
                 connections_max = MAX(connections_max, excluded.connections_max), \
                 inflight_max = MAX(inflight_max, excluded.inflight_max)",
            )?
            .execute(params![
                m.minute,
                m.rss_avg.map(int),
                m.rss_max.map(int),
                m.cpu_pct,
                int(m.connections_max),
                int(m.inflight_max)
            ])?;
        self.roll_host_hour(m.minute.div_euclid(60))
    }

    /// `host_hourly` for `hour`, from its `host_minute` rows (none: nothing written).
    pub fn roll_host_hour(&self, hour: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO host_hourly (hour, rss_avg, rss_max, cpu_avg, cpu_max, \
                 connections_max, inflight_max) \
                 SELECT ?1, CAST(AVG(rss_avg) AS INTEGER), MAX(rss_max), AVG(cpu_pct), \
                 MAX(cpu_pct), MAX(connections_max), MAX(inflight_max) \
                 FROM host_minute WHERE minute >= ?2 AND minute < ?3 HAVING COUNT(*) > 0 \
                 ON CONFLICT (hour) DO UPDATE SET rss_avg = excluded.rss_avg, \
                 rss_max = excluded.rss_max, cpu_avg = excluded.cpu_avg, \
                 cpu_max = excluded.cpu_max, connections_max = excluded.connections_max, \
                 inflight_max = excluded.inflight_max",
            )?
            .execute(params![hour, hour * 60, hour * 60 + 60])?;
        Ok(())
    }
}

impl AnalyticsDb {
    /// `host_minute` rows with `from <= minute <= to`, oldest first.
    pub fn host_minutes(&self, from: i64, to: i64) -> Result<Vec<HostMinute>> {
        self.read(|c| {
            let sql = format!(
                "SELECT {MINUTE_COLUMNS} WHERE minute >= ?1 AND minute <= ?2 ORDER BY minute"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt
                .query_map(params![from, to], minute_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The newest `host_minute` row, if any.
    pub fn host_latest_minute(&self) -> Result<Option<HostMinute>> {
        self.read(|c| {
            let sql = format!("SELECT {MINUTE_COLUMNS} ORDER BY minute DESC LIMIT 1");
            Ok(c.prepare_cached(&sql)?
                .query_row([], minute_row)
                .optional()?)
        })
    }

    /// `host_hourly` rows with `from <= hour <= to`, oldest first.
    pub fn host_hours(&self, from: i64, to: i64) -> Result<Vec<HostHour>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT hour, rss_avg, rss_max, cpu_avg, cpu_max, connections_max, inflight_max \
                 FROM host_hourly WHERE hour >= ?1 AND hour <= ?2 ORDER BY hour",
            )?;
            let rows = stmt
                .query_map(params![from, to], |r| {
                    Ok(HostHour {
                        hour: r.get(0)?,
                        rss_avg: r.get::<_, Option<i64>>(1)?.map(uint),
                        rss_max: r.get::<_, Option<i64>>(2)?.map(uint),
                        cpu_avg: r.get(3)?,
                        cpu_max: r.get(4)?,
                        connections_max: uint(r.get(5)?),
                        inflight_max: uint(r.get(6)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::testutil::temp_db;

    fn minute(m: i64, rss: u64, cpu: Option<f64>, conns: u64) -> HostMinute {
        HostMinute {
            minute: m,
            rss_avg: Some(rss),
            rss_max: Some(rss + 10),
            cpu_pct: cpu,
            connections_max: conns,
            inflight_max: 1,
        }
    }

    #[test]
    fn minutes_upsert_and_roll_into_their_hour() {
        let (_dir, db) = temp_db();
        let h = 500_000i64;
        db.write(|tx| {
            tx.put_host_minute(&minute(h * 60, 100, Some(2.0), 3))?;
            // A later sample of the same minute replaces it.
            tx.put_host_minute(&minute(h * 60, 200, Some(4.0), 5))?;
            tx.put_host_minute(&minute(h * 60 + 59, 400, None, 1))?;
            tx.put_host_minute(&minute(h * 60 + 60, 999, Some(50.0), 9))
        })
        .unwrap();
        let mins = db.host_minutes(h * 60, h * 60 + 59).unwrap();
        assert_eq!(mins.len(), 2);
        assert_eq!(mins[0].rss_avg, Some(200));
        assert_eq!(mins[0].connections_max, 5);
        assert_eq!(mins[1].cpu_pct, None);
        let hours = db.host_hours(h, h + 1).unwrap();
        assert_eq!(hours.len(), 2);
        let first = hours[0];
        assert_eq!(first.hour, h);
        assert_eq!(first.rss_avg, Some(300));
        assert_eq!(first.rss_max, Some(410));
        // AVG and MAX skip the minute without a CPU reading.
        assert_eq!(first.cpu_avg, Some(4.0));
        assert_eq!(first.cpu_max, Some(4.0));
        assert_eq!(first.connections_max, 5);
        assert_eq!(hours[1].rss_avg, Some(999));
        assert_eq!(
            db.host_latest_minute().unwrap().unwrap().minute,
            h * 60 + 60
        );
    }

    #[test]
    fn an_unsupported_platform_stores_nulls_not_zeros() {
        let (_dir, db) = temp_db();
        db.write(|tx| {
            tx.put_host_minute(&HostMinute {
                minute: 60,
                connections_max: 2,
                ..HostMinute::default()
            })
        })
        .unwrap();
        let m = db.host_minutes(0, 100).unwrap();
        assert_eq!((m[0].rss_avg, m[0].cpu_pct), (None, None));
        let h = db.host_hours(0, 10).unwrap();
        assert_eq!(
            (h[0].rss_avg, h[0].cpu_avg, h[0].connections_max),
            (None, None, 2)
        );
        let (_d2, empty) = temp_db();
        assert!(empty.host_latest_minute().unwrap().is_none());
    }

    /// A restart within a minute: the new process's first sample (fresh peaks) keeps
    /// the old process's peaks for that minute and its hour.
    #[test]
    fn a_restart_in_the_same_minute_keeps_the_earlier_peaks() {
        let (_dir, db) = temp_db();
        let m = 500_000i64 * 60 + 7;
        db.write(|tx| {
            tx.put_host_minute(&HostMinute {
                minute: m,
                rss_avg: Some(400),
                rss_max: Some(500),
                cpu_pct: Some(9.0),
                connections_max: 40,
                inflight_max: 12,
            })?;
            tx.put_host_minute(&HostMinute {
                minute: m,
                rss_avg: Some(60),
                rss_max: Some(60),
                cpu_pct: Some(1.0),
                connections_max: 0,
                inflight_max: 0,
            })
        })
        .unwrap();
        let row = db.host_minutes(m, m).unwrap()[0];
        assert_eq!(
            (row.rss_max, row.connections_max, row.inflight_max),
            (Some(500), 40, 12)
        );
        let hour = db.host_hours(m / 60, m / 60).unwrap()[0];
        assert_eq!((hour.rss_max, hour.connections_max), (Some(500), 40));
    }
}
