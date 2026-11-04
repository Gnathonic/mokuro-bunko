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
    /// `client_principals`, `client_mix_principals`, and `slow_requests` (at most
    /// [`SLOW_DAYS`]).
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
    /// A new salt period began: the anonymous ids' salt was replaced.
    pub salt_rotated: bool,
}

const DAY: i64 = 86400;

impl AnalyticsDb {
    /// Roll up, then prune, as of `now_s` (epoch seconds), in one transaction.
    pub fn maintain(&self, now_s: i64, retention: &Retention) -> Result<MaintainStats> {
        let (stats, salt) = self.write(|tx| {
            let days_rolled = rollup(tx.conn, now_s)?;
            let pruned = prune(tx.conn, now_s, retention)?;
            let salt = rotate_salt(tx.conn, now_s, retention.ip_days)?;
            let stats = MaintainStats {
                days_rolled,
                pruned,
                salt_rotated: salt.is_some(),
            };
            Ok((stats, salt))
        })?;
        // Only once the new salt is stored: a rolled-back rotation keeps the old one.
        if let Some(salt) = salt {
            self.set_salt(salt);
        }
        Ok(stats)
    }
}

/// Replace the salt when its period (`ip_days` UTC days) is over: the old one is
/// overwritten, so the ids it made, which outlive the raw IPs in the daily tables,
/// can no longer be matched to an address by hashing candidates. The first upkeep of
/// a file only records the period its salt belongs to.
fn rotate_salt(conn: &Connection, now_s: i64, ip_days: u32) -> Result<Option<[u8; 32]>> {
    let epoch = now_s.div_euclid(DAY).div_euclid(i64::from(ip_days.max(1)));
    let stored: Option<i64> =
        super::meta_get(conn, keys::SALT_EPOCH)?.and_then(|v| v.trim().parse().ok());
    let set = |key: &str, value: &str| -> Result<()> {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    };
    match stored {
        Some(e) if e == epoch => Ok(None),
        None => {
            set(keys::SALT_EPOCH, &epoch.to_string())?;
            Ok(None)
        }
        Some(_) => {
            let salt = super::new_salt();
            set(keys::SALT, &hex::encode(salt))?;
            set(keys::SALT_EPOCH, &epoch.to_string())?;
            Ok(Some(salt))
        }
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
        // Errors and failed sign-ins (their count only: no IP, no username) outlive
        // their hourly rows, so long ranges do not read as a quiet server.
        let error_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM errors_hourly WHERE hour >= ?1 AND hour < ?2",
            params![h0, h1],
            |r| r.get(0),
        )?;
        if error_rows > 0 {
            conn.execute("DELETE FROM errors_daily WHERE day = ?1", [day])?;
            conn.execute(
                "INSERT INTO errors_daily (day, protocol, method, category, status, count) \
                 SELECT ?1, protocol, method, category, status, SUM(count) FROM errors_hourly \
                 WHERE hour >= ?2 AND hour < ?3 GROUP BY protocol, method, category, status",
                params![day, h0, h1],
            )?;
        }
        let failures: Option<i64> = conn.query_row(
            "SELECT SUM(count) FROM auth_failures WHERE hour >= ?1 AND hour < ?2",
            params![h0, h1],
            |r| r.get(0),
        )?;
        if let Some(n) = failures {
            conn.execute(
                "INSERT INTO auth_failures_daily (day, count) VALUES (?1, ?2) \
                 ON CONFLICT (day) DO UPDATE SET count = excluded.count",
                params![day, n],
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
    // The next rollup rebuilds its days from `rolled - 1` on, from their hourly rows:
    // those hours are never pruned, or a short `hourly_days` (1 or 2) would rebuild a
    // day from what is left of it and lose the rest. Neither are a not-yet-rolled
    // day's (it would vanish).
    let hour_cut = (now_hour - i64::from(r.hourly_days) * 24).min((rolled - 1) * 24);
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
    del(
        "DELETE FROM client_mix_principals WHERE day < ?1",
        ip_day_cut,
    )?;
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

    /// The hourly upkeep, as the flush task runs it, under every short retention: a
    /// rolled day keeps its whole total (it used to be rebuilt from the hours pruning
    /// had left of it).
    #[test]
    fn short_hourly_retention_never_erodes_a_rolled_day() {
        for hourly_days in [1u32, 2, 3] {
            let (_d, db) = temp_db();
            let day = 20_000i64;
            let k = KindDims {
                protocol: "dav",
                method: "GET",
                category: "volume",
            };
            let mut h = [0u64; LATENCY_BUCKETS];
            h[2] = 1;
            db.write(|tx| {
                for hour in 0..24 {
                    tx.add_traffic(day * 24 + hour, &D, &c(1))?;
                    tx.add_latency(day * 24 + hour, &k, &h, &h)?;
                }
                Ok(())
            })
            .unwrap();
            let r = Retention {
                hourly_days,
                ..Retention::default()
            };
            let one_day = Span {
                granularity: Granularity::Day,
                from: day,
                to: day,
            };
            for hour in (day + 1) * 24..(day + 6) * 24 {
                db.maintain(hour * 3600 + 60, &r).unwrap();
                let total = db
                    .traffic(&one_day, None, false, &TrafficFilter::default())
                    .unwrap()
                    .first()
                    .map_or(0, |t| t.counts.requests);
                assert_eq!(total, 24, "hourly_days={hourly_days}, at hour {hour}");
                let lat = db.latency(&one_day).unwrap();
                assert_eq!(lat[0].total[2], 24, "latency, hourly_days={hourly_days}");
            }
            // The day's hours do go in the end: retention is only stretched to the two
            // days the rollup may still rebuild.
            let left: i64 = db
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT COUNT(*) FROM traffic_hourly WHERE hour < ?1",
                        [(day + 1) * 24],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
            assert_eq!(left, 0, "hourly_days={hourly_days}");
        }
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

    /// The salt is replaced once its `ip_days` period is over, and the old one is
    /// gone from the file: ids made with it can no longer be matched to an address.
    #[test]
    fn the_salt_is_replaced_every_ip_days_and_the_old_one_deleted() {
        let (_d, db) = temp_db();
        let r = Retention::default();
        let first = db.salt();
        let day = 20_010i64; // 20010 / 30 = 667: a period starts on day 20010
        let stats = db.maintain(day * DAY + 60, &r).unwrap();
        assert!(
            !stats.salt_rotated,
            "the first upkeep only notes the period"
        );
        assert_eq!(db.salt(), first);
        // Later in the same period: kept.
        let stats = db.maintain((day + 29) * DAY, &r).unwrap();
        assert!(!stats.salt_rotated);
        assert_eq!(db.salt(), first);
        // The next period: a new salt, stored, and the old one nowhere in `meta`.
        let stats = db.maintain((day + 30) * DAY, &r).unwrap();
        assert!(stats.salt_rotated);
        let second = db.salt();
        assert_ne!(second, first);
        let stored = db.meta_get(keys::SALT).unwrap().unwrap();
        assert_eq!(stored, hex::encode(second));
        let old = hex::encode(first);
        let holding: i64 = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM meta WHERE value LIKE ?1",
                    [format!("%{old}%")],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(holding, 0);
        assert_eq!(
            db.meta_i64(keys::SALT_EPOCH).unwrap(),
            Some((day + 30) / 30)
        );
    }

    /// A download counts once most of the file went out (resumes add up); a
    /// one-byte range probe does not.
    #[test]
    fn only_completed_downloads_count() {
        let (_d, db) = temp_db();
        let day = 20_000i64;
        let row = |principal: &'static str, bytes: u64| DownloadRow {
            day,
            principal,
            volume_key: "S/v.cbz",
            series_key: "s",
            series_title: "S",
            bytes,
            size: 1000,
        };
        db.write(|tx| {
            tx.add_download(&row("a:000000000001", 1))?;
            tx.add_download(&row("a:000000000002", 1))?;
            tx.add_download(&row("u:alice", 1000))?;
            tx.add_download(&row("u:bob", 600))?;
            tx.add_download(&row("u:bob", 400))
        })
        .unwrap();
        assert_eq!(db.download_totals(day, day).unwrap(), (2, 2));
        let s = db.series_downloads(day, day).unwrap();
        assert_eq!(
            (s[0].downloads, s[0].downloaders, s[0].volumes, s[0].bytes),
            (2, 2, 1, 2002)
        );
        assert_eq!(
            db.downloaders_by_volume("s").unwrap(),
            [("S/v.cbz".to_string(), 2)]
        );
        assert_eq!(db.downloaders_by_series().unwrap()[0].2, 2);
    }

    /// Errors and failed sign-ins outlive their hourly (and IP) rows as daily counts:
    /// 1000 failures 40 days ago and 500 errors 100 days ago still show on long ranges.
    #[test]
    fn errors_and_failed_sign_ins_outlive_their_hourly_rows() {
        let (_d, db) = temp_db();
        let today = 20_100i64;
        let k = KindDims {
            protocol: "dav",
            method: "PROPFIND",
            category: "metadata",
        };
        db.write(|tx| {
            let h = (today - 40) * 24 + 3;
            tx.add_auth_failures(h, "192.0.2.9", "admin", 1000)?;
            tx.add_traffic(h, &D, &c(1))?;
            let h = (today - 100) * 24 + 5;
            tx.add_errors(h, &k, 401, 500)?;
            tx.add_traffic(h, &D, &c(1))
        })
        .unwrap();
        // The upkeep ran every day (the rollup goes on from where it left off).
        for day in (today - 100)..=today {
            db.maintain(day * DAY + 7200, &Retention::default())
                .unwrap();
        }
        let count = |t: &str| -> i64 {
            db.read(|c| Ok(c.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| r.get(0))?))
                .unwrap()
        };
        assert_eq!(count("auth_failures"), 0, "the IP rows are gone");
        assert_eq!(count("errors_hourly"), 0, "the hourly rows are gone");
        let all = Span {
            granularity: Granularity::Day,
            from: today - 365,
            to: today,
        };
        assert_eq!(db.auth_failure_total(&all).unwrap(), 1000);
        assert_eq!(db.auth_failure_series(&all).unwrap(), [(today - 40, 1000)]);
        let errors = db.errors(&all, 10).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!((errors[0].status, errors[0].count), (401, 500));
        assert_eq!(errors[0].method, "PROPFIND");
    }
}
