//! Crawl episodes (design §8): one row per episode of a client IP touching a large part
//! of the catalog. The foundation creates the table (pruned by `retention.crawl_days`
//! on `last_at`); the detector (bunko-server `analytics::crawl`) writes it through the
//! statements here, inside the collector's flush transaction (`Detector::flush_into`).

use super::{AnalyticsDb, AnalyticsTx, int, uint};
use crate::error::Result;
use rusqlite::{Row, params};

pub(crate) const DDL: &[&str] = &[
    // `principals`: JSON array; `kind`: download | metadata | mixed; `status`: open | closed.
    "CREATE TABLE IF NOT EXISTS crawl_episodes (
        id INTEGER PRIMARY KEY, ip TEXT NOT NULL, country TEXT, principals TEXT NOT NULL DEFAULT '[]',
        client TEXT, user_agent TEXT, kind TEXT NOT NULL, started_at INTEGER NOT NULL,
        last_at INTEGER NOT NULL, series INTEGER NOT NULL DEFAULT 0,
        volumes INTEGER NOT NULL DEFAULT 0, catalog_series INTEGER NOT NULL DEFAULT 0,
        coverage REAL NOT NULL DEFAULT 0, bytes_out INTEGER NOT NULL DEFAULT 0,
        requests INTEGER NOT NULL DEFAULT 0, status TEXT NOT NULL DEFAULT 'open'
    )",
    "CREATE INDEX IF NOT EXISTS idx_crawl_episodes_ip ON crawl_episodes (ip)",
    "CREATE INDEX IF NOT EXISTS idx_crawl_episodes_last_at ON crawl_episodes (last_at)",
];

pub const STATUS_OPEN: &str = "open";
pub const STATUS_CLOSED: &str = "closed";

/// One `crawl_episodes` row (`id` 0: not written yet).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EpisodeRow {
    pub id: i64,
    pub ip: String,
    pub country: Option<String>,
    pub principals: Vec<String>,
    pub client: Option<String>,
    pub user_agent: Option<String>,
    /// `download`, `metadata` or `mixed`.
    pub kind: String,
    pub started_at: i64,
    pub last_at: i64,
    pub series: u64,
    pub volumes: u64,
    pub catalog_series: u64,
    pub coverage: f64,
    pub bytes_out: u64,
    pub requests: u64,
    /// [`STATUS_OPEN`] or [`STATUS_CLOSED`].
    pub status: String,
}

/// Episodes of one IP over a span (`GET .../crawlers` `by_ip`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EpisodesByIp {
    pub ip: String,
    /// The newest episode's country.
    pub country: Option<String>,
    pub episodes: u64,
    pub last_at: i64,
    pub bytes_out: u64,
}

fn principals_json(p: &[String]) -> String {
    serde_json::to_string(p).unwrap_or_else(|_| "[]".into())
}

const COLUMNS: &str = "id, ip, country, principals, client, user_agent, kind, started_at, \
     last_at, series, volumes, catalog_series, coverage, bytes_out, requests, status";

fn episode_row(r: &Row<'_>) -> rusqlite::Result<EpisodeRow> {
    let principals: String = r.get(3)?;
    Ok(EpisodeRow {
        id: r.get(0)?,
        ip: r.get(1)?,
        country: r.get(2)?,
        principals: serde_json::from_str(&principals).unwrap_or_default(),
        client: r.get(4)?,
        user_agent: r.get(5)?,
        kind: r.get(6)?,
        started_at: r.get(7)?,
        last_at: r.get(8)?,
        series: uint(r.get(9)?),
        volumes: uint(r.get(10)?),
        catalog_series: uint(r.get(11)?),
        coverage: r.get(12)?,
        bytes_out: uint(r.get(13)?),
        requests: uint(r.get(14)?),
        status: r.get(15)?,
    })
}

impl AnalyticsTx<'_> {
    /// Write `e`: a new row when `e.id` is 0 or that row is gone (a rolled-back flush,
    /// pruning), else replace that row's values. Returns the row's id.
    pub fn put_crawl_episode(&self, e: &EpisodeRow) -> Result<i64> {
        let principals = principals_json(&e.principals);
        let counts = [
            int(e.series),
            int(e.volumes),
            int(e.catalog_series),
            int(e.bytes_out),
            int(e.requests),
        ];
        let values = params![
            e.ip,
            e.country,
            principals,
            e.client,
            e.user_agent,
            e.kind,
            e.started_at,
            e.last_at,
            counts[0],
            counts[1],
            counts[2],
            e.coverage,
            counts[3],
            counts[4],
            e.status,
        ];
        if e.id > 0 {
            let mut with_id: Vec<&dyn rusqlite::ToSql> = values.to_vec();
            with_id.push(&e.id);
            let changed = self
                .conn
                .prepare_cached(
                    "UPDATE crawl_episodes SET ip = ?1, country = ?2, principals = ?3, \
                     client = ?4, user_agent = ?5, kind = ?6, started_at = ?7, last_at = ?8, \
                     series = ?9, volumes = ?10, catalog_series = ?11, coverage = ?12, \
                     bytes_out = ?13, requests = ?14, status = ?15 WHERE id = ?16",
                )?
                .execute(with_id.as_slice())?;
            if changed > 0 {
                return Ok(e.id);
            }
        }
        self.conn
            .prepare_cached(
                "INSERT INTO crawl_episodes (ip, country, principals, client, user_agent, kind, \
                 started_at, last_at, series, volumes, catalog_series, coverage, bytes_out, \
                 requests, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )?
            .execute(values)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close every open episode except `keep` (a previous run's: its detector state is
    /// gone, so nothing would ever close it). Returns how many.
    pub fn close_crawl_episodes_except(&self, keep: &[i64]) -> Result<u64> {
        let keep = serde_json::to_string(keep).unwrap_or_else(|_| "[]".into());
        Ok(self
            .conn
            .prepare_cached(
                "UPDATE crawl_episodes SET status = 'closed' WHERE status = 'open' \
                 AND id NOT IN (SELECT value FROM json_each(?1))",
            )?
            .execute([keep])? as u64)
    }
}

impl AnalyticsDb {
    /// Episodes active at some point in `[from_s, to_s)` (as the overview counts them),
    /// newest first, at most `limit`.
    pub fn crawl_episodes(&self, from_s: i64, to_s: i64, limit: u32) -> Result<Vec<EpisodeRow>> {
        self.read(|c| {
            let sql = format!(
                "SELECT {COLUMNS} FROM crawl_episodes WHERE last_at >= ?1 AND started_at < ?2 \
                 ORDER BY last_at DESC, id DESC LIMIT ?3"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt
                .query_map(params![from_s, to_s, limit], episode_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Per IP over the same span: how many episodes, the last activity, bytes sent.
    pub fn crawl_episodes_by_ip(&self, from_s: i64, to_s: i64) -> Result<Vec<EpisodesByIp>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT ip, COUNT(*), MAX(last_at), COALESCE(SUM(bytes_out), 0), \
                 (SELECT country FROM crawl_episodes c2 WHERE c2.ip = c1.ip \
                  ORDER BY last_at DESC LIMIT 1) \
                 FROM crawl_episodes c1 WHERE last_at >= ?1 AND started_at < ?2 \
                 GROUP BY ip ORDER BY COUNT(*) DESC, MAX(last_at) DESC",
            )?;
            let rows = stmt
                .query_map(params![from_s, to_s], |r| {
                    Ok(EpisodesByIp {
                        ip: r.get(0)?,
                        episodes: uint(r.get(1)?),
                        last_at: r.get(2)?,
                        bytes_out: uint(r.get(3)?),
                        country: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Episodes started per UTC day, `from_day..=to_day` (days without one left out).
    pub fn crawl_episodes_per_day(&self, from_day: i64, to_day: i64) -> Result<Vec<(i64, u64)>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT started_at / 86400 AS d, COUNT(*) FROM crawl_episodes \
                 WHERE started_at >= ?1 AND started_at < ?2 GROUP BY d ORDER BY d",
            )?;
            let rows = stmt
                .query_map(params![from_day * 86400, (to_day + 1) * 86400], |r| {
                    Ok((r.get(0)?, uint(r.get(1)?)))
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

    fn episode(ip: &str, started: i64, last: i64) -> EpisodeRow {
        EpisodeRow {
            ip: ip.into(),
            country: Some("LAN".into()),
            principals: vec!["a:0123456789ab".into(), "u:alice".into()],
            client: Some("rclone".into()),
            user_agent: Some("rclone/v1.68.1".into()),
            kind: "download".into(),
            started_at: started,
            last_at: last,
            series: 30,
            volumes: 120,
            catalog_series: 40,
            coverage: 0.75,
            bytes_out: 1 << 30,
            requests: 400,
            status: STATUS_OPEN.into(),
            ..EpisodeRow::default()
        }
    }

    #[test]
    fn episodes_insert_update_and_read_back() {
        let (_dir, db) = temp_db();
        let day = 20_000 * 86400;
        let mut e = episode("203.0.113.5", day + 100, day + 200);
        let id = db.write(|tx| tx.put_crawl_episode(&e)).unwrap();
        assert!(id > 0);
        e.id = id;
        e.series = 35;
        e.last_at = day + 300;
        e.status = STATUS_CLOSED.into();
        assert_eq!(db.write(|tx| tx.put_crawl_episode(&e)).unwrap(), id);
        let rows = db.crawl_episodes(day, day + 86400, 100).unwrap();
        assert_eq!(rows, vec![e.clone()]);
        // A row that vanished (pruned, or a rolled-back insert) is written anew.
        let mut ghost = e.clone();
        ghost.id = 9999;
        let new_id = db.write(|tx| tx.put_crawl_episode(&ghost)).unwrap();
        assert_ne!(new_id, 9999);
        assert_eq!(db.crawl_episodes(day, day + 86400, 100).unwrap().len(), 2);
        assert_eq!(db.crawl_episodes(day, day + 86400, 1).unwrap().len(), 1);
        // Outside the span: ended before it, or started after it.
        assert!(
            db.crawl_episodes(day + 400, day + 500, 10)
                .unwrap()
                .is_empty()
        );
        assert!(db.crawl_episodes(0, day + 100, 10).unwrap().is_empty());
    }

    #[test]
    fn by_ip_per_day_and_closing_a_previous_runs_episodes() {
        let (_dir, db) = temp_db();
        let day = 20_000i64;
        let rows = [
            episode("203.0.113.5", day * 86400 + 10, day * 86400 + 20),
            episode(
                "203.0.113.5",
                (day + 1) * 86400 + 10,
                (day + 1) * 86400 + 20,
            ),
            episode(
                "198.51.100.7",
                (day + 1) * 86400 + 30,
                (day + 1) * 86400 + 40,
            ),
        ];
        let ids: Vec<i64> = db
            .write(|tx| rows.iter().map(|e| tx.put_crawl_episode(e)).collect())
            .unwrap();
        let by_ip = db
            .crawl_episodes_by_ip(day * 86400, (day + 2) * 86400)
            .unwrap();
        assert_eq!(by_ip.len(), 2);
        assert_eq!(by_ip[0].ip, "203.0.113.5");
        assert_eq!(by_ip[0].episodes, 2);
        assert_eq!(by_ip[0].bytes_out, 2 << 30);
        assert_eq!(by_ip[0].last_at, (day + 1) * 86400 + 20);
        assert_eq!(by_ip[0].country.as_deref(), Some("LAN"));
        assert_eq!(
            db.crawl_episodes_per_day(day, day + 2).unwrap(),
            vec![(day, 1), (day + 1, 2)]
        );
        let closed = db
            .write(|tx| tx.close_crawl_episodes_except(&ids[2..]))
            .unwrap();
        assert_eq!(closed, 2);
        let all = db.crawl_episodes(0, i64::MAX, 10).unwrap();
        let open: Vec<i64> = all
            .iter()
            .filter(|e| e.status == STATUS_OPEN)
            .map(|e| e.id)
            .collect();
        assert_eq!(open, vec![ids[2]]);
        assert_eq!(all[0].principals, vec!["a:0123456789ab", "u:alice"]);
    }
}
