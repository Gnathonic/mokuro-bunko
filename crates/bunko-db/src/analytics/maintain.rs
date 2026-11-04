//! Hourly upkeep (design §4.6): roll complete days of hourly rows into the daily
//! tables, then prune what the retention settings let go. Daily aggregates are kept
//! forever.

use super::traffic::{Histogram, LATENCY_BUCKETS, merge_histogram, parse_hist};
use super::{AnalyticsDb, keys};
use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// How long each kind of row is kept, in days (`analytics.retention`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Rows with a raw client IP: `ip_daily`, `anon_ips`, `auth_failures`,
    /// `client_principals`, and `slow_requests` (at most [`SLOW_DAYS`]).
    pub ip_days: u32,
    /// Hourly traffic, latency, errors and host rows.
    pub hourly_days: u32,
    pub host_minute_days: u32,
    pub crawl_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            ip_days: 30,
            hourly_days: 90,
            host_minute_days: 7,
            crawl_days: 90,
        }
    }
}

/// The slowest requests are kept a week (or less, under a shorter IP retention).
pub const SLOW_DAYS: u32 = 7;

/// What one [`AnalyticsDb::maintain`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MaintainStats {
    /// Days (re)computed in the daily tables.
    pub days_rolled: u32,
    /// Rows deleted.
    pub pruned: u64,
}

const DAY: i64 = 86400;

impl AnalyticsDb {
    /// Roll up, then prune, as of `now_s` (epoch seconds), in one transaction.
    pub fn maintain(&self, now_s: i64, retention: &Retention) -> Result<MaintainStats> {
        self.write(|tx| {
            let days_rolled = rollup(tx.conn, now_s)?;
            let pruned = prune(tx.conn, now_s, retention)?;
            Ok(MaintainStats {
                days_rolled,
                pruned,
            })
        })
    }
}

/// Compute the daily rows of every complete day from the day before the last one
/// rolled (late flushes land in it: a request is counted in the hour it started)
/// through yesterday. Recomputed from the hourly rows, so it can run any number of
/// times; a day with no hourly rows left keeps whatever daily rows it has.
fn rollup(conn: &Connection, now_s: i64) -> Result<u32> {
    let yesterday = now_s.div_euclid(DAY) - 1;
    let rolled: Option<i64> =
        super::meta_get(conn, keys::ROLLED_THROUGH_DAY)?.and_then(|v| v.trim().parse().ok());
    let first_hour: Option<i64> = conn
        .query_row("SELECT MIN(hour) FROM traffic_hourly", [], |r| r.get(0))
        .optional()?
        .flatten();
    let first_lat: Option<i64> = conn
        .query_row("SELECT MIN(hour) FROM latency_hourly", [], |r| r.get(0))
        .optional()?
        .flatten();
    let first_day = match (first_hour, first_lat) {
        (Some(a), Some(b)) => Some(a.min(b).div_euclid(24)),
        (a, b) => a.or(b).map(|h| h.div_euclid(24)),
    };
    let Some(first_day) = first_day else {
        return Ok(0);
    };
    let start = match rolled {
        Some(r) => (r - 1).max(first_day),
        None => first_day,
    };
    let mut days = 0;
    for day in start..=yesterday {
        let (h0, h1) = (day * 24, day * 24 + 24);
        let traffic_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM traffic_hourly WHERE hour >= ?1 AND hour < ?2",
            params![h0, h1],
            |r| r.get(0),
        )?;
        if traffic_rows > 0 {
            conn.execute("DELETE FROM traffic_daily WHERE day = ?1", [day])?;
            conn.execute(
                "INSERT INTO traffic_daily (day, protocol, method, category, detail, status_class, \
                 requests, bytes_out, est_bytes_out, bytes_in, aborted, aborted_bytes, resumed, \
                 not_modified, saved_bytes, gzip_raw_bytes, gzip_sent_bytes) \
                 SELECT ?1, protocol, method, category, detail, status_class, SUM(requests), \
                 SUM(bytes_out), SUM(est_bytes_out), SUM(bytes_in), SUM(aborted), \
                 SUM(aborted_bytes), SUM(resumed), SUM(not_modified), SUM(saved_bytes), \
                 SUM(gzip_raw_bytes), SUM(gzip_sent_bytes) FROM traffic_hourly \
                 WHERE hour >= ?2 AND hour < ?3 \
                 GROUP BY protocol, method, category, detail, status_class",
                params![day, h0, h1],
            )?;
        }
        let mut merged: Vec<((String, String, String), Histogram, Histogram)> = Vec::new();
        {
            let mut stmt = conn.prepare_cached(
                "SELECT protocol, method, category, headers_hist, total_hist FROM latency_hourly \
                 WHERE hour >= ?1 AND hour < ?2",
            )?;
            let mut rows = stmt.query(params![h0, h1])?;
            while let Some(r) = rows.next()? {
                let key: (String, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
                let h = parse_hist(&r.get::<_, String>(3)?);
                let t = parse_hist(&r.get::<_, String>(4)?);
                match merged.iter_mut().find(|(k, _, _)| *k == key) {
                    Some((_, mh, mt)) => {
                        merge_histogram(mh, &h);
                        merge_histogram(mt, &t);
                    }
                    None => merged.push((key, h, t)),
                }
            }
        }
        if !merged.is_empty() {
            conn.execute("DELETE FROM latency_daily WHERE day = ?1", [day])?;
            let mut ins = conn.prepare_cached(
                "INSERT INTO latency_daily (day, protocol, method, category, headers_hist, total_hist) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for ((p, m, c), h, t) in &merged {
                ins.execute(params![day, p, m, c, json(h), json(t)])?;
            }
        }
        days += 1;
    }
    if yesterday >= start {
        let mark = rolled.map_or(yesterday, |r| r.max(yesterday));
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![keys::ROLLED_THROUGH_DAY, mark.to_string()],
        )?;
    }
    Ok(days)
}

fn json(h: &[u64; LATENCY_BUCKETS]) -> String {
    serde_json::to_string(&h[..]).unwrap_or_else(|_| "[]".into())
}

fn prune(conn: &Connection, now_s: i64, r: &Retention) -> Result<u64> {
    let today = now_s.div_euclid(DAY);
    let now_hour = now_s.div_euclid(3600);
    let rolled: i64 = super::traffic::rolled_through(conn)?;
    // Hourly rows of a day not rolled up yet are never pruned (their day would vanish).
    let hour_cut = (now_hour - i64::from(r.hourly_days) * 24).min((rolled + 1) * 24);
    let ip_day_cut = today - i64::from(r.ip_days);
    let slow_cut = now_s - i64::from(r.ip_days.min(SLOW_DAYS)) * DAY;
    let crawl_cut = now_s - i64::from(r.crawl_days) * DAY;
    let minute_cut = now_s.div_euclid(60) - i64::from(r.host_minute_days) * 1440;
    let mut n = 0u64;
    let mut del = |sql: &str, cut: i64| -> Result<()> {
        n += conn.execute(sql, [cut])? as u64;
        Ok(())
    };
    del("DELETE FROM traffic_hourly WHERE hour < ?1", hour_cut)?;
    del("DELETE FROM latency_hourly WHERE hour < ?1", hour_cut)?;
    del("DELETE FROM errors_hourly WHERE hour < ?1", hour_cut)?;
    del("DELETE FROM host_hourly WHERE hour < ?1", hour_cut)?;
    del("DELETE FROM ip_daily WHERE day < ?1", ip_day_cut)?;
    del("DELETE FROM anon_ips WHERE day < ?1", ip_day_cut)?;
    del("DELETE FROM client_principals WHERE day < ?1", ip_day_cut)?;
    del("DELETE FROM auth_failures WHERE hour < ?1", ip_day_cut * 24)?;
    del("DELETE FROM slow_requests WHERE at < ?1", slow_cut)?;
    del("DELETE FROM crawl_episodes WHERE last_at < ?1", crawl_cut)?;
    del("DELETE FROM host_minute WHERE minute < ?1", minute_cut)?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::testutil::temp_db;
    use crate::analytics::traffic::*;

    const D: TrafficDims<'static> = TrafficDims {
        protocol: "dav",
        method: "GET",
        category: "volume",
        detail: "full",
        status_class: 2,
    };

    fn c(n: u64) -> TrafficCounts {
        TrafficCounts {
            requests: n,
            bytes_out: n * 10,
            ..Default::default()
        }
    }

    #[test]
    fn rollup_then_queries_see_each_row_once() {
        let (_d, db) = temp_db();
        let day = 20_000i64;
        let k = KindDims {
            protocol: "dav",
            method: "GET",
            category: "volume",
        };
        let mut h = [0u64; LATENCY_BUCKETS];
        h[3] = 1;
        db.write(|tx| {
            tx.add_traffic(day * 24 + 1, &D, &c(1))?;
            tx.add_traffic(day * 24 + 23, &D, &c(2))?;
            tx.add_traffic((day + 1) * 24 + 2, &D, &c(4))?;
            tx.add_latency(day * 24 + 1, &k, &h, &h)?;
            tx.add_latency(day * 24 + 5, &k, &h, &h)
        })
        .unwrap();
        let span = Span {
            granularity: Granularity::Day,
            from: day,
            to: day + 1,
        };
        let per_day = |db: &AnalyticsDb| {
            let mut v: Vec<(i64, u64)> = db
                .traffic(&span, None, true, &TrafficFilter::default())
                .unwrap()
                .into_iter()
                .map(|r| (r.bucket.unwrap(), r.counts.requests))
                .collect();
            v.sort();
            v
        };
        assert_eq!(per_day(&db), [(day, 3), (day + 1, 4)], "before the rollup");
        // `now` is in day+1: day is complete and rolled, day+1 is not.
        let now = (day + 1) * DAY + 3 * 3600;
        let stats = db.maintain(now, &Retention::default()).unwrap();
        assert_eq!(stats.days_rolled, 1);
        assert_eq!(per_day(&db), [(day, 3), (day + 1, 4)], "after it");
        let daily: i64 = db
            .read(|c| Ok(c.query_row("SELECT SUM(requests) FROM traffic_daily", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(daily, 3);
        let lat = db.latency(&span).unwrap();
        assert_eq!(lat[0].total[3], 2);
        // Again: idempotent (the last rolled day is recomputed, not added twice).
        db.maintain(now, &Retention::default()).unwrap();
        assert_eq!(per_day(&db), [(day, 3), (day + 1, 4)]);
        assert_eq!(db.latency(&span).unwrap()[0].total[3], 2);
    }

    #[test]
    fn prune_keeps_daily_rows_and_drops_old_ip_rows() {
        let (_d, db) = temp_db();
        let day = 20_000i64;
        db.write(|tx| {
            tx.add_traffic(day * 24, &D, &c(1))?;
            tx.add_ip(
                day,
                "192.0.2.5",
                &IpCounts {
                    requests: 1,
                    ..Default::default()
                },
            )?;
            tx.add_anon_ip(day, "a:000000000000", "192.0.2.5", 1)?;
            tx.add_auth_failures(day * 24, "192.0.2.5", "x", 1)?;
            tx.add_slow(&[SlowRequest {
                at: day * DAY,
                method: "GET".into(),
                path: "/".into(),
                status: 200,
                ms: 5,
                principal: "a:000000000000".into(),
                ip: "192.0.2.5".into(),
            }])
        })
        .unwrap();
        // 100 days later: hourly (90) and IP rows (30) are gone, the daily row stays.
        let now = (day + 100) * DAY;
        let stats = db.maintain(now, &Retention::default()).unwrap();
        assert!(stats.pruned >= 5, "{stats:?}");
        let count = |t: &str| -> i64 {
            db.read(|c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))?))
                .unwrap()
        };
        for t in [
            "traffic_hourly",
            "ip_daily",
            "anon_ips",
            "auth_failures",
            "slow_requests",
        ] {
            assert_eq!(count(t), 0, "{t}");
        }
        assert_eq!(count("traffic_daily"), 1);
        let span = Span {
            granularity: Granularity::Day,
            from: day,
            to: day,
        };
        let total = db
            .traffic(&span, None, false, &TrafficFilter::default())
            .unwrap();
        assert_eq!(total[0].counts.requests, 1);
    }

    #[test]
    fn unrolled_hours_are_never_pruned() {
        let (_d, db) = temp_db();
        let day = 20_000i64;
        db.write(|tx| tx.add_traffic(day * 24, &D, &c(1))).unwrap();
        // Retention 0 hourly days, but the day is today: not rolled, so not pruned.
        let r = Retention {
            hourly_days: 0,
            ..Retention::default()
        };
        db.maintain(day * DAY + 7200, &r).unwrap();
        let n: i64 = db
            .read(|c| Ok(c.query_row("SELECT COUNT(*) FROM traffic_hourly", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(n, 1);
    }
}
