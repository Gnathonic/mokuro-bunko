//! Request accounting (design §4.4, §5): traffic and latency by hour and day, errors,
//! principals, client IPs, auth failures, the slowest requests, volume downloads and
//! archive uploads. The collector's flush writes these; the admin API reads them.

use super::{AnalyticsDb, AnalyticsTx, int, uint};
use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS traffic_hourly (
        hour INTEGER NOT NULL, protocol TEXT NOT NULL, method TEXT NOT NULL,
        category TEXT NOT NULL, detail TEXT NOT NULL, status_class INTEGER NOT NULL,
        requests INTEGER NOT NULL DEFAULT 0, bytes_out INTEGER NOT NULL DEFAULT 0,
        est_bytes_out INTEGER NOT NULL DEFAULT 0, bytes_in INTEGER NOT NULL DEFAULT 0,
        aborted INTEGER NOT NULL DEFAULT 0, aborted_bytes INTEGER NOT NULL DEFAULT 0,
        resumed INTEGER NOT NULL DEFAULT 0, not_modified INTEGER NOT NULL DEFAULT 0,
        saved_bytes INTEGER NOT NULL DEFAULT 0, gzip_raw_bytes INTEGER NOT NULL DEFAULT 0,
        gzip_sent_bytes INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (hour, protocol, method, category, detail, status_class)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS traffic_daily (
        day INTEGER NOT NULL, protocol TEXT NOT NULL, method TEXT NOT NULL,
        category TEXT NOT NULL, detail TEXT NOT NULL, status_class INTEGER NOT NULL,
        requests INTEGER NOT NULL DEFAULT 0, bytes_out INTEGER NOT NULL DEFAULT 0,
        est_bytes_out INTEGER NOT NULL DEFAULT 0, bytes_in INTEGER NOT NULL DEFAULT 0,
        aborted INTEGER NOT NULL DEFAULT 0, aborted_bytes INTEGER NOT NULL DEFAULT 0,
        resumed INTEGER NOT NULL DEFAULT 0, not_modified INTEGER NOT NULL DEFAULT 0,
        saved_bytes INTEGER NOT NULL DEFAULT 0, gzip_raw_bytes INTEGER NOT NULL DEFAULT 0,
        gzip_sent_bytes INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, protocol, method, category, detail, status_class)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS latency_hourly (
        hour INTEGER NOT NULL, protocol TEXT NOT NULL, method TEXT NOT NULL,
        category TEXT NOT NULL, headers_hist TEXT NOT NULL, total_hist TEXT NOT NULL,
        PRIMARY KEY (hour, protocol, method, category)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS latency_daily (
        day INTEGER NOT NULL, protocol TEXT NOT NULL, method TEXT NOT NULL,
        category TEXT NOT NULL, headers_hist TEXT NOT NULL, total_hist TEXT NOT NULL,
        PRIMARY KEY (day, protocol, method, category)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS errors_hourly (
        hour INTEGER NOT NULL, protocol TEXT NOT NULL, method TEXT NOT NULL,
        category TEXT NOT NULL, status INTEGER NOT NULL, count INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (hour, protocol, method, category, status)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS errors_daily (
        day INTEGER NOT NULL, protocol TEXT NOT NULL, method TEXT NOT NULL,
        category TEXT NOT NULL, status INTEGER NOT NULL, count INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, protocol, method, category, status)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS auth_failures_daily (
        day INTEGER PRIMARY KEY, count INTEGER NOT NULL DEFAULT 0
    )",
    "CREATE TABLE IF NOT EXISTS principal_daily (
        day INTEGER NOT NULL, principal TEXT NOT NULL, category TEXT NOT NULL,
        requests INTEGER NOT NULL DEFAULT 0, bytes_out INTEGER NOT NULL DEFAULT 0,
        bytes_in INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, principal, category)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS principal_seen (
        principal TEXT PRIMARY KEY, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS anon_ips (
        day INTEGER NOT NULL, principal TEXT NOT NULL, ip TEXT NOT NULL,
        requests INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, principal, ip)
    ) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS idx_anon_ips_principal ON anon_ips (principal, day)",
    "CREATE TABLE IF NOT EXISTS ip_daily (
        day INTEGER NOT NULL, ip TEXT NOT NULL, requests INTEGER NOT NULL DEFAULT 0,
        bytes_out INTEGER NOT NULL DEFAULT 0, errors INTEGER NOT NULL DEFAULT 0,
        auth_failures INTEGER NOT NULL DEFAULT 0, principals TEXT NOT NULL DEFAULT '[]',
        user_agent TEXT, country TEXT,
        PRIMARY KEY (day, ip)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS auth_failures (
        hour INTEGER NOT NULL, ip TEXT NOT NULL, attempted_username TEXT NOT NULL,
        count INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (hour, ip, attempted_username)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS slow_requests (
        id INTEGER PRIMARY KEY, at INTEGER NOT NULL, method TEXT NOT NULL,
        path TEXT NOT NULL, status INTEGER NOT NULL, ms INTEGER NOT NULL,
        principal TEXT NOT NULL, ip TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_slow_requests_at ON slow_requests (at)",
    "CREATE TABLE IF NOT EXISTS volume_downloads (
        day INTEGER NOT NULL, principal TEXT NOT NULL, volume_key TEXT NOT NULL,
        series_key TEXT NOT NULL, series_title TEXT NOT NULL,
        bytes INTEGER NOT NULL DEFAULT 0, size INTEGER NOT NULL DEFAULT 0,
        completed INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, principal, volume_key)
    ) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS idx_volume_downloads_series ON volume_downloads (series_key, day)",
    "CREATE INDEX IF NOT EXISTS idx_volume_downloads_day ON volume_downloads (day)",
    "CREATE TABLE IF NOT EXISTS uploads_daily (
        day INTEGER NOT NULL, username TEXT NOT NULL,
        volumes INTEGER NOT NULL DEFAULT 0, bytes INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, username)
    ) WITHOUT ROWID",
];

/// The counters of one `traffic_*` row (design §4.4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrafficCounts {
    pub requests: u64,
    /// Body bytes the server handed hyper.
    pub bytes_out: u64,
    /// Bytes nginx sent for an `X-Accel-Redirect` answer (estimated from the file).
    pub est_bytes_out: u64,
    pub bytes_in: u64,
    pub aborted: u64,
    /// Body bytes sent by the responses that were aborted.
    pub aborted_bytes: u64,
    /// `Range` requests of a volume.
    pub resumed: u64,
    pub not_modified: u64,
    /// The size of the files 304s did not send.
    pub saved_bytes: u64,
    /// Uncompressed bytes of the gzip-sent PROPFIND listings...
    pub gzip_raw_bytes: u64,
    /// ...and what they cost on the wire.
    pub gzip_sent_bytes: u64,
}

impl TrafficCounts {
    pub fn add(&mut self, o: &TrafficCounts) {
        self.requests += o.requests;
        self.bytes_out += o.bytes_out;
        self.est_bytes_out += o.est_bytes_out;
        self.bytes_in += o.bytes_in;
        self.aborted += o.aborted;
        self.aborted_bytes += o.aborted_bytes;
        self.resumed += o.resumed;
        self.not_modified += o.not_modified;
        self.saved_bytes += o.saved_bytes;
        self.gzip_raw_bytes += o.gzip_raw_bytes;
        self.gzip_sent_bytes += o.gzip_sent_bytes;
    }

    fn from_row(r: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<TrafficCounts> {
        let g = |i: usize| -> rusqlite::Result<u64> {
            Ok(uint(r.get::<_, Option<i64>>(first + i)?.unwrap_or(0)))
        };
        Ok(TrafficCounts {
            requests: g(0)?,
            bytes_out: g(1)?,
            est_bytes_out: g(2)?,
            bytes_in: g(3)?,
            aborted: g(4)?,
            aborted_bytes: g(5)?,
            resumed: g(6)?,
            not_modified: g(7)?,
            saved_bytes: g(8)?,
            gzip_raw_bytes: g(9)?,
            gzip_sent_bytes: g(10)?,
        })
    }
}

const COUNT_COLUMNS: &str = "requests, bytes_out, est_bytes_out, bytes_in, aborted, \
     aborted_bytes, resumed, not_modified, saved_bytes, gzip_raw_bytes, gzip_sent_bytes";
const SUM_COLUMNS: &str = "SUM(requests), SUM(bytes_out), SUM(est_bytes_out), SUM(bytes_in), \
     SUM(aborted), SUM(aborted_bytes), SUM(resumed), SUM(not_modified), SUM(saved_bytes), \
     SUM(gzip_raw_bytes), SUM(gzip_sent_bytes)";

/// The dimensions of a `traffic_*` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrafficDims<'a> {
    pub protocol: &'a str,
    pub method: &'a str,
    pub category: &'a str,
    pub detail: &'a str,
    /// `status / 100`.
    pub status_class: u16,
}

/// The dimensions of a `latency_*` / `errors_hourly` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindDims<'a> {
    pub protocol: &'a str,
    pub method: &'a str,
    pub category: &'a str,
}

/// Upper bounds (ms) of the latency histogram's buckets; a 17th catches the rest.
pub const LATENCY_BOUNDS_MS: [u64; 16] = [
    1, 2, 5, 10, 20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 30000, 60000, 300000,
];
pub const LATENCY_BUCKETS: usize = LATENCY_BOUNDS_MS.len() + 1;
/// Counts per bucket of [`LATENCY_BOUNDS_MS`] (stored as a JSON array).
pub type Histogram = [u64; LATENCY_BUCKETS];

/// The bucket of a duration of `ms` milliseconds (bucket `i` holds `bounds[i-1] < ms <=
/// bounds[i]`).
pub fn latency_bucket(ms: u64) -> usize {
    LATENCY_BOUNDS_MS
        .iter()
        .position(|b| ms <= *b)
        .unwrap_or(LATENCY_BOUNDS_MS.len())
}

pub fn merge_histogram(into: &mut Histogram, from: &Histogram) {
    for (a, b) in into.iter_mut().zip(from) {
        *a += b;
    }
}

/// The `p` quantile (0..=1) of a histogram in ms, interpolated linearly inside its
/// bucket; the open last bucket answers its lower bound. `None` when it is empty.
pub fn histogram_quantile(h: &Histogram, p: f64) -> Option<f64> {
    let total: u64 = h.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = (p.clamp(0.0, 1.0) * total as f64).max(1.0);
    let mut seen = 0u64;
    for (i, &n) in h.iter().enumerate() {
        if n == 0 {
            continue;
        }
        if (seen + n) as f64 >= rank {
            let lo = if i == 0 { 0 } else { LATENCY_BOUNDS_MS[i - 1] } as f64;
            let Some(&hi) = LATENCY_BOUNDS_MS.get(i) else {
                return Some(lo);
            };
            let within = (rank - seen as f64) / n as f64;
            return Some(lo + (hi as f64 - lo) * within);
        }
        seen += n;
    }
    LATENCY_BOUNDS_MS.last().map(|b| *b as f64)
}

fn hist_json(h: &Histogram) -> String {
    serde_json::to_string(&h[..]).unwrap_or_else(|_| "[]".into())
}

/// A stored histogram; a short, long or unreadable one reads as what fits (zeros).
pub(crate) fn parse_hist(text: &str) -> Histogram {
    let mut h = [0u64; LATENCY_BUCKETS];
    if let Ok(v) = serde_json::from_str::<Vec<u64>>(text) {
        for (slot, n) in h.iter_mut().zip(v) {
            *slot = n;
        }
    }
    h
}

/// `principal_daily` counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrincipalCounts {
    pub requests: u64,
    /// Counted plus estimated (accel) bytes.
    pub bytes_out: u64,
    pub bytes_in: u64,
}

impl PrincipalCounts {
    pub fn add(&mut self, o: &PrincipalCounts) {
        self.requests += o.requests;
        self.bytes_out += o.bytes_out;
        self.bytes_in += o.bytes_in;
    }
}

/// Most principals one `ip_daily` row lists.
pub const IP_PRINCIPALS_MAX: usize = 8;

/// One `ip_daily` increment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IpCounts {
    pub requests: u64,
    pub bytes_out: u64,
    pub errors: u64,
    pub auth_failures: u64,
    /// At most [`IP_PRINCIPALS_MAX`]; merged with the stored list (first ones kept).
    pub principals: Vec<String>,
    /// The last user agent seen (replaces the stored one when set).
    pub user_agent: Option<String>,
    pub country: Option<String>,
}

/// One `slow_requests` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlowRequest {
    /// Epoch seconds the request started.
    pub at: i64,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ms: u64,
    pub principal: String,
    pub ip: String,
}

/// The slowest requests kept per hour.
pub const SLOW_PER_HOUR: usize = 20;

/// One `volume_downloads` increment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRow<'a> {
    pub day: i64,
    pub principal: &'a str,
    pub volume_key: &'a str,
    pub series_key: &'a str,
    pub series_title: &'a str,
    pub bytes: u64,
    /// The whole file (`Served.size`).
    pub size: u64,
}

/// The anonymous principal that stands for every new anonymous visitor of a day past
/// [`ANON_PER_DAY`]: their traffic still counts, but no longer one row each.
pub const ANON_OTHER: &str = "a:other";

/// Most anonymous visitors a UTC day keeps rows of their own for (design §3). The
/// daily tables are kept forever and anyone can make up new addresses; past this a
/// day's newcomers fold into [`ANON_OTHER`], so a flood adds a bounded number of rows
/// per day.
pub const ANON_PER_DAY: u64 = 10_000;

/// A download is complete once 95 % of the file was sent (over every request of the
/// day: resumed downloads add up).
pub const COMPLETED_FRACTION: f64 = 0.95;

impl AnalyticsTx<'_> {
    pub fn add_traffic(&self, hour: i64, d: &TrafficDims<'_>, c: &TrafficCounts) -> Result<()> {
        let mut stmt = self.conn.prepare_cached(
            "INSERT INTO traffic_hourly (hour, protocol, method, category, detail, status_class, \
             requests, bytes_out, est_bytes_out, bytes_in, aborted, aborted_bytes, resumed, \
             not_modified, saved_bytes, gzip_raw_bytes, gzip_sent_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) \
             ON CONFLICT (hour, protocol, method, category, detail, status_class) DO UPDATE SET \
             requests = requests + excluded.requests, bytes_out = bytes_out + excluded.bytes_out, \
             est_bytes_out = est_bytes_out + excluded.est_bytes_out, \
             bytes_in = bytes_in + excluded.bytes_in, aborted = aborted + excluded.aborted, \
             aborted_bytes = aborted_bytes + excluded.aborted_bytes, \
             resumed = resumed + excluded.resumed, not_modified = not_modified + excluded.not_modified, \
             saved_bytes = saved_bytes + excluded.saved_bytes, \
             gzip_raw_bytes = gzip_raw_bytes + excluded.gzip_raw_bytes, \
             gzip_sent_bytes = gzip_sent_bytes + excluded.gzip_sent_bytes",
        )?;
        stmt.execute(params![
            hour,
            d.protocol,
            d.method,
            d.category,
            d.detail,
            d.status_class,
            int(c.requests),
            int(c.bytes_out),
            int(c.est_bytes_out),
            int(c.bytes_in),
            int(c.aborted),
            int(c.aborted_bytes),
            int(c.resumed),
            int(c.not_modified),
            int(c.saved_bytes),
            int(c.gzip_raw_bytes),
            int(c.gzip_sent_bytes),
        ])?;
        Ok(())
    }

    /// Merge two histograms (time to headers, time to body end) into an hour's row.
    pub fn add_latency(
        &self,
        hour: i64,
        d: &KindDims<'_>,
        headers: &Histogram,
        total: &Histogram,
    ) -> Result<()> {
        let stored: Option<(String, String)> = self
            .conn
            .prepare_cached(
                "SELECT headers_hist, total_hist FROM latency_hourly \
                 WHERE hour = ?1 AND protocol = ?2 AND method = ?3 AND category = ?4",
            )?
            .query_row(params![hour, d.protocol, d.method, d.category], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let (mut h, mut t) = match stored {
            Some((h, t)) => (parse_hist(&h), parse_hist(&t)),
            None => ([0; LATENCY_BUCKETS], [0; LATENCY_BUCKETS]),
        };
        merge_histogram(&mut h, headers);
        merge_histogram(&mut t, total);
        self.conn
            .prepare_cached(
                "INSERT OR REPLACE INTO latency_hourly \
                 (hour, protocol, method, category, headers_hist, total_hist) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?
            .execute(params![
                hour,
                d.protocol,
                d.method,
                d.category,
                hist_json(&h),
                hist_json(&t)
            ])?;
        Ok(())
    }

    pub fn add_errors(&self, hour: i64, d: &KindDims<'_>, status: u16, count: u64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO errors_hourly (hour, protocol, method, category, status, count) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (hour, protocol, method, category, status) \
                 DO UPDATE SET count = count + excluded.count",
            )?
            .execute(params![
                hour,
                d.protocol,
                d.method,
                d.category,
                status,
                int(count)
            ])?;
        Ok(())
    }

    pub fn add_principal(
        &self,
        day: i64,
        principal: &str,
        category: &str,
        c: &PrincipalCounts,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO principal_daily (day, principal, category, requests, bytes_out, bytes_in) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT (day, principal, category) DO UPDATE SET \
                 requests = requests + excluded.requests, bytes_out = bytes_out + excluded.bytes_out, \
                 bytes_in = bytes_in + excluded.bytes_in",
            )?
            .execute(params![
                day,
                principal,
                category,
                int(c.requests),
                int(c.bytes_out),
                int(c.bytes_in)
            ])?;
        Ok(())
    }

    /// Widen a principal's `[first_seen, last_seen]` (epoch seconds).
    pub fn see_principal(&self, principal: &str, first: i64, last: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO principal_seen (principal, first_seen, last_seen) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (principal) DO UPDATE SET \
                 first_seen = MIN(first_seen, excluded.first_seen), \
                 last_seen = MAX(last_seen, excluded.last_seen)",
            )?
            .execute(params![principal, first, last])?;
        Ok(())
    }

    pub fn add_anon_ip(&self, day: i64, principal: &str, ip: &str, requests: u64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO anon_ips (day, principal, ip, requests) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (day, principal, ip) DO UPDATE SET requests = requests + excluded.requests",
            )?
            .execute(params![day, principal, ip, int(requests)])?;
        Ok(())
    }

    pub fn add_ip(&self, day: i64, ip: &str, c: &IpCounts) -> Result<()> {
        let stored: Option<String> = self
            .conn
            .prepare_cached("SELECT principals FROM ip_daily WHERE day = ?1 AND ip = ?2")?
            .query_row(params![day, ip], |r| r.get(0))
            .optional()?;
        let mut principals: Vec<String> = stored
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for p in &c.principals {
            if principals.len() >= IP_PRINCIPALS_MAX {
                break;
            }
            if !principals.contains(p) {
                principals.push(p.clone());
            }
        }
        let principals = serde_json::to_string(&principals).unwrap_or_else(|_| "[]".into());
        self.conn
            .prepare_cached(
                "INSERT INTO ip_daily (day, ip, requests, bytes_out, errors, auth_failures, \
                 principals, user_agent, country) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                 ON CONFLICT (day, ip) DO UPDATE SET requests = requests + excluded.requests, \
                 bytes_out = bytes_out + excluded.bytes_out, errors = errors + excluded.errors, \
                 auth_failures = auth_failures + excluded.auth_failures, \
                 principals = excluded.principals, \
                 user_agent = COALESCE(excluded.user_agent, user_agent), \
                 country = COALESCE(excluded.country, country)",
            )?
            .execute(params![
                day,
                ip,
                int(c.requests),
                int(c.bytes_out),
                int(c.errors),
                int(c.auth_failures),
                principals,
                c.user_agent,
                c.country
            ])?;
        Ok(())
    }

    pub fn add_auth_failures(&self, hour: i64, ip: &str, username: &str, count: u64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO auth_failures (hour, ip, attempted_username, count) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (hour, ip, attempted_username) DO UPDATE SET count = count + excluded.count",
            )?
            .execute(params![hour, ip, username, int(count)])?;
        Ok(())
    }

    /// Add slow requests, then keep only the [`SLOW_PER_HOUR`] slowest of each hour they
    /// fall in.
    pub fn add_slow(&self, rows: &[SlowRequest]) -> Result<()> {
        let mut hours: Vec<i64> = Vec::new();
        {
            let mut stmt = self.conn.prepare_cached(
                "INSERT INTO slow_requests (at, method, path, status, ms, principal, ip) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for r in rows {
                stmt.execute(params![
                    r.at,
                    r.method,
                    r.path,
                    r.status,
                    int(r.ms),
                    r.principal,
                    r.ip
                ])?;
                let h = r.at.div_euclid(3600);
                if !hours.contains(&h) {
                    hours.push(h);
                }
            }
        }
        for h in hours {
            self.conn
                .prepare_cached(
                    "DELETE FROM slow_requests WHERE at >= ?1 AND at < ?2 AND id NOT IN \
                     (SELECT id FROM slow_requests WHERE at >= ?1 AND at < ?2 \
                      ORDER BY ms DESC, id ASC LIMIT ?3)",
                )?
                .execute(params![h * 3600, (h + 1) * 3600, SLOW_PER_HOUR as i64])?;
        }
        Ok(())
    }

    /// Add to a (day, principal, volume) download; `completed` is decided on the day's
    /// total ([`COMPLETED_FRACTION`]) and never goes back.
    pub fn add_download(&self, row: &DownloadRow<'_>) -> Result<()> {
        let done = |bytes: u64, size: u64| bytes as f64 >= COMPLETED_FRACTION * size as f64;
        self.conn
            .prepare_cached(
                "INSERT INTO volume_downloads (day, principal, volume_key, series_key, series_title, \
                 bytes, size, completed) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (day, principal, volume_key) DO UPDATE SET \
                 series_key = excluded.series_key, series_title = excluded.series_title, \
                 completed = MAX(completed, (bytes + excluded.bytes) >= ?9 * MAX(size, excluded.size)), \
                 bytes = bytes + excluded.bytes, size = MAX(size, excluded.size)",
            )?
            .execute(params![
                row.day,
                row.principal,
                row.volume_key,
                row.series_key,
                row.series_title,
                int(row.bytes),
                int(row.size),
                done(row.bytes, row.size),
                COMPLETED_FRACTION
            ])?;
        Ok(())
    }

    /// Distinct anonymous principals with rows on `day` (the flush's [`ANON_PER_DAY`]
    /// check; a range over the `(day, principal, ...)` key).
    pub fn anon_principals_on(&self, day: i64) -> Result<u64> {
        Ok(uint(
            self.conn
                .prepare_cached(
                    "SELECT COUNT(DISTINCT principal) FROM principal_daily \
                     WHERE day = ?1 AND principal >= 'a:' AND principal < 'a;'",
                )?
                .query_row([day], |r| r.get(0))?,
        ))
    }

    /// Does `principal` have a row on `day` already?
    pub fn principal_known_on(&self, day: i64, principal: &str) -> Result<bool> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT 1 FROM principal_daily WHERE day = ?1 AND principal = ?2 LIMIT 1",
            )?
            .query_row(params![day, principal], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn add_uploads(&self, day: i64, username: &str, volumes: u64, bytes: u64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO uploads_daily (day, username, volumes, bytes) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (day, username) DO UPDATE SET volumes = volumes + excluded.volumes, \
                 bytes = bytes + excluded.bytes",
            )?
            .execute(params![day, username, int(volumes), int(bytes)])?;
        Ok(())
    }
}

// --- queries --------------------------------------------------------------------------------

/// The bucket size of a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    Hour,
    Day,
}

impl Granularity {
    pub fn seconds(self) -> i64 {
        match self {
            Granularity::Hour => 3600,
            Granularity::Day => 86400,
        }
    }
}

/// A `traffic_*` column to group by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficDim {
    Protocol,
    Method,
    Category,
    Detail,
    StatusClass,
}

impl TrafficDim {
    fn column(self) -> &'static str {
        match self {
            TrafficDim::Protocol => "protocol",
            TrafficDim::Method => "method",
            TrafficDim::Category => "category",
            TrafficDim::Detail => "detail",
            TrafficDim::StatusClass => "CAST(status_class AS TEXT)",
        }
    }
}

/// Rows a traffic query keeps (`None`: any).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrafficFilter {
    pub protocol: Option<String>,
    pub category: Option<String>,
    pub method: Option<String>,
}

/// One aggregated traffic row: per bucket (`bucket` = its hour or day number) when
/// asked, per key of the grouping dimension (`""` without one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficAgg {
    pub bucket: Option<i64>,
    pub key: String,
    pub counts: TrafficCounts,
}

/// A query span: buckets `from..=to` of `granularity` (hour or day numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub granularity: Granularity,
    pub from: i64,
    pub to: i64,
}

impl Span {
    /// The hours the span covers, `from..=to`.
    pub fn hours(&self) -> (i64, i64) {
        match self.granularity {
            Granularity::Hour => (self.from, self.to),
            Granularity::Day => (self.from * 24, self.to * 24 + 23),
        }
    }

    /// The days the span touches, `from..=to`.
    pub fn days(&self) -> (i64, i64) {
        match self.granularity {
            Granularity::Hour => (self.from.div_euclid(24), self.to.div_euclid(24)),
            Granularity::Day => (self.from, self.to),
        }
    }

    /// Epoch seconds `[start, end)`.
    pub fn seconds(&self) -> (i64, i64) {
        let s = self.granularity.seconds();
        (self.from * s, (self.to + 1) * s)
    }
}

/// The rows of `hourly`/`daily` (same columns, `hour` vs `day` first) for a span, as one
/// SQL source with the bucket as `b`: daily rows for the days already rolled up, hourly
/// rows (bucketed by day) for the rest; hourly rows alone for an hourly span. Binds
/// `?1`..`?3` (`from`, `to`, rolled-through day).
fn span_source(span: &Span, hourly: &str, daily: &str, cols: &str) -> String {
    match span.granularity {
        Granularity::Hour => {
            // `?3` is bound for every span; named here so the statement takes it.
            format!(
                "SELECT hour AS b, {cols} FROM {hourly} WHERE hour >= ?1 AND hour <= ?2 AND ?3 = ?3"
            )
        }
        Granularity::Day => format!(
            "SELECT day AS b, {cols} FROM {daily} WHERE day >= ?1 AND day <= MIN(?2, ?3) \
             UNION ALL \
             SELECT hour / 24 AS b, {cols} FROM {hourly} \
             WHERE hour >= MAX(?1, ?3 + 1) * 24 AND hour < (?2 + 1) * 24"
        ),
    }
}

pub(crate) fn rolled_through(conn: &Connection) -> Result<i64> {
    Ok(super::meta_get(conn, super::keys::ROLLED_THROUGH_DAY)?
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(-1))
}

/// Latency of one kind of request over a span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyAgg {
    pub protocol: String,
    pub method: String,
    pub category: String,
    pub headers: Histogram,
    pub total: Histogram,
}

/// Errors of one kind and status over a span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorAgg {
    pub status: u16,
    pub protocol: String,
    pub method: String,
    pub category: String,
    pub count: u64,
}

/// A principal's traffic over a span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalAgg {
    pub principal: String,
    pub totals: PrincipalCounts,
    pub by_category: Vec<(String, PrincipalCounts)>,
    pub last_seen: Option<i64>,
}

/// A series' downloads over a span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesDownloads {
    pub series_key: String,
    pub title: String,
    /// (day, principal, volume) rows.
    pub downloads: u64,
    pub downloaders: u64,
    pub bytes: u64,
    /// Distinct volumes downloaded.
    pub volumes: u64,
}

/// An IP's auth failures over a span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthFailureIp {
    pub ip: String,
    pub count: u64,
    pub usernames: Vec<String>,
    /// From `ip_daily` (geo), when known.
    pub country: Option<String>,
}

impl AnalyticsDb {
    /// Traffic over `span`, grouped by `by` (one row per key) and, when `per_bucket`,
    /// by bucket too.
    pub fn traffic(
        &self,
        span: &Span,
        by: Option<TrafficDim>,
        per_bucket: bool,
        filter: &TrafficFilter,
    ) -> Result<Vec<TrafficAgg>> {
        self.read(|c| {
            let rolled = rolled_through(c)?;
            let source = span_source(
                span,
                "traffic_hourly",
                "traffic_daily",
                &format!("protocol, method, category, detail, status_class, {COUNT_COLUMNS}"),
            );
            let key = by.map(TrafficDim::column).unwrap_or("''");
            let bucket = if per_bucket { "b" } else { "NULL" };
            let mut group = Vec::new();
            if per_bucket {
                group.push("b");
            }
            if by.is_some() {
                group.push(key);
            }
            let group_by = if group.is_empty() {
                String::new()
            } else {
                format!("GROUP BY {}", group.join(", "))
            };
            let sql = format!(
                "SELECT {bucket}, {key}, {SUM_COLUMNS} FROM ({source}) \
                 WHERE (?4 IS NULL OR protocol = ?4) AND (?5 IS NULL OR category = ?5) \
                 AND (?6 IS NULL OR method = ?6) {group_by}"
            );
            let mut stmt = c.prepare_cached(&sql)?;
            let rows = stmt
                .query_map(
                    params![
                        span.from,
                        span.to,
                        rolled,
                        filter.protocol,
                        filter.category,
                        filter.method
                    ],
                    |r| {
                        Ok(TrafficAgg {
                            bucket: r.get(0)?,
                            key: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                            counts: TrafficCounts::from_row(r, 2)?,
                        })
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // A sum over no rows is one row of NULLs.
            Ok(rows
                .into_iter()
                .filter(|r| r.counts != TrafficCounts::default() || r.bucket.is_some())
                .collect())
        })
    }

    /// Latency histograms per (protocol, method, category) over `span`.
    pub fn latency(&self, span: &Span) -> Result<Vec<LatencyAgg>> {
        self.read(|c| {
            let rolled = rolled_through(c)?;
            let source = span_source(
                span,
                "latency_hourly",
                "latency_daily",
                "protocol, method, category, headers_hist, total_hist",
            );
            let mut stmt = c.prepare_cached(&format!(
                "SELECT protocol, method, category, headers_hist, total_hist FROM ({source})"
            ))?;
            let mut out: Vec<LatencyAgg> = Vec::new();
            let mut rows = stmt.query(params![span.from, span.to, rolled])?;
            while let Some(r) = rows.next()? {
                let (p, m, cat): (String, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
                let h = parse_hist(&r.get::<_, String>(3)?);
                let t = parse_hist(&r.get::<_, String>(4)?);
                match out
                    .iter_mut()
                    .find(|a| a.protocol == p && a.method == m && a.category == cat)
                {
                    Some(a) => {
                        merge_histogram(&mut a.headers, &h);
                        merge_histogram(&mut a.total, &t);
                    }
                    None => out.push(LatencyAgg {
                        protocol: p,
                        method: m,
                        category: cat,
                        headers: h,
                        total: t,
                    }),
                }
            }
            Ok(out)
        })
    }

    /// Error counts over `span`, most frequent first: the hourly rows, and past the
    /// hourly retention the daily ones (kept forever).
    pub fn errors(&self, span: &Span, limit: usize) -> Result<Vec<ErrorAgg>> {
        self.read(|c| {
            let rolled = rolled_through(c)?;
            let source = span_source(
                span,
                "errors_hourly",
                "errors_daily",
                "protocol, method, category, status, count",
            );
            let mut stmt = c.prepare_cached(&format!(
                "SELECT status, protocol, method, category, SUM(count) AS n FROM ({source}) \
                 GROUP BY status, protocol, method, category \
                 ORDER BY n DESC, status ASC LIMIT ?4"
            ))?;
            let rows = stmt
                .query_map(params![span.from, span.to, rolled, limit as i64], |r| {
                    Ok(ErrorAgg {
                        status: r.get(0)?,
                        protocol: r.get(1)?,
                        method: r.get(2)?,
                        category: r.get(3)?,
                        count: uint(r.get(4)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Auth failures per bucket of `span` (only buckets that have any): the hourly
    /// rows, and past the IP retention the daily totals (no IP, no username; kept
    /// forever).
    pub fn auth_failure_series(&self, span: &Span) -> Result<Vec<(i64, u64)>> {
        self.read(|c| {
            let rolled = rolled_through(c)?;
            let source = span_source(span, "auth_failures", "auth_failures_daily", "count");
            let mut stmt = c.prepare_cached(&format!(
                "SELECT b, SUM(count) FROM ({source}) GROUP BY b ORDER BY b"
            ))?;
            let rows = stmt
                .query_map(params![span.from, span.to, rolled], |r| {
                    Ok((r.get(0)?, uint(r.get(1)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The IPs with the most auth failures in hours `from..=to`, with the usernames they
    /// tried (at most 10 each) and their country when geo knew it.
    pub fn auth_failure_top_ips(
        &self,
        from_hour: i64,
        to_hour: i64,
        limit: usize,
    ) -> Result<Vec<AuthFailureIp>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT ip, SUM(count) AS n FROM auth_failures WHERE hour >= ?1 AND hour <= ?2 \
                 GROUP BY ip ORDER BY n DESC, ip ASC LIMIT ?3",
            )?;
            let ips = stmt
                .query_map(params![from_hour, to_hour, limit as i64], |r| {
                    Ok((r.get::<_, String>(0)?, uint(r.get(1)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::with_capacity(ips.len());
            for (ip, count) in ips {
                let usernames = c
                    .prepare_cached(
                        "SELECT attempted_username, SUM(count) AS n FROM auth_failures \
                         WHERE hour >= ?1 AND hour <= ?2 AND ip = ?3 AND attempted_username != '' \
                         GROUP BY attempted_username ORDER BY n DESC LIMIT 10",
                    )?
                    .query_map(params![from_hour, to_hour, ip], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                let country: Option<String> = c
                    .prepare_cached(
                        "SELECT country FROM ip_daily WHERE ip = ?1 AND country IS NOT NULL \
                         ORDER BY day DESC LIMIT 1",
                    )?
                    .query_row([&ip], |r| r.get(0))
                    .optional()?;
                out.push(AuthFailureIp {
                    ip,
                    count,
                    usernames,
                    country,
                });
            }
            Ok(out)
        })
    }

    /// The usernames tried most in failed logins in hours `from..=to`.
    pub fn auth_failure_top_usernames(
        &self,
        from_hour: i64,
        to_hour: i64,
        limit: usize,
    ) -> Result<Vec<(String, u64)>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT attempted_username, SUM(count) AS n FROM auth_failures \
                 WHERE hour >= ?1 AND hour <= ?2 AND attempted_username != '' \
                 GROUP BY attempted_username ORDER BY n DESC, attempted_username ASC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![from_hour, to_hour, limit as i64], |r| {
                    Ok((r.get(0)?, uint(r.get(1)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Slowest requests that started in `[from_s, to_s)`.
    pub fn slow_requests(&self, from_s: i64, to_s: i64, limit: usize) -> Result<Vec<SlowRequest>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT at, method, path, status, ms, principal, ip FROM slow_requests \
                 WHERE at >= ?1 AND at < ?2 ORDER BY ms DESC, id ASC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![from_s, to_s, limit as i64], |r| {
                    Ok(SlowRequest {
                        at: r.get(0)?,
                        method: r.get(1)?,
                        path: r.get(2)?,
                        status: r.get(3)?,
                        ms: uint(r.get(4)?),
                        principal: r.get(5)?,
                        ip: r.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Principals by bytes out over days `from..=to` (processors included), heaviest
    /// first, with their per-category split and `last_seen`. Two passes over the range
    /// whatever `limit` is: the top principals, then every category row of theirs.
    pub fn principals(
        &self,
        from_day: i64,
        to_day: i64,
        limit: usize,
    ) -> Result<Vec<PrincipalAgg>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT principal, SUM(requests), SUM(bytes_out) AS b, SUM(bytes_in) \
                 FROM principal_daily WHERE day >= ?1 AND day <= ?2 \
                 GROUP BY principal ORDER BY b DESC, principal ASC LIMIT ?3",
            )?;
            let mut out: Vec<PrincipalAgg> = stmt
                .query_map(params![from_day, to_day, limit as i64], |r| {
                    Ok(PrincipalAgg {
                        principal: r.get(0)?,
                        totals: PrincipalCounts {
                            requests: uint(r.get(1)?),
                            bytes_out: uint(r.get(2)?),
                            bytes_in: uint(r.get(3)?),
                        },
                        by_category: Vec::new(),
                        last_seen: None,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let at: std::collections::HashMap<String, usize> = out
                .iter()
                .enumerate()
                .map(|(i, p)| (p.principal.clone(), i))
                .collect();
            let mut split = c.prepare_cached(
                "SELECT principal, category, SUM(requests), SUM(bytes_out) AS b, SUM(bytes_in) \
                 FROM principal_daily WHERE day >= ?1 AND day <= ?2 AND principal IN \
                 (SELECT principal FROM principal_daily WHERE day >= ?1 AND day <= ?2 \
                  GROUP BY principal ORDER BY SUM(bytes_out) DESC, principal ASC LIMIT ?3) \
                 GROUP BY principal, category",
            )?;
            let mut rows = split.query(params![from_day, to_day, limit as i64])?;
            while let Some(r) = rows.next()? {
                let principal: String = r.get(0)?;
                if let Some(&i) = at.get(&principal) {
                    out[i].by_category.push((
                        r.get(1)?,
                        PrincipalCounts {
                            requests: uint(r.get(2)?),
                            bytes_out: uint(r.get(3)?),
                            bytes_in: uint(r.get(4)?),
                        },
                    ));
                }
            }
            let mut seen =
                c.prepare_cached("SELECT last_seen FROM principal_seen WHERE principal = ?1")?;
            for p in &mut out {
                p.by_category.sort_by(|a, b| {
                    b.1.bytes_out
                        .cmp(&a.1.bytes_out)
                        .then_with(|| a.0.cmp(&b.0))
                });
                p.last_seen = seen.query_row([&p.principal], |r| r.get(0)).optional()?;
            }
            Ok(out)
        })
    }

    /// Distinct principals active in days `from..=to` whose id starts with each of
    /// `u:`, `a:` and `p:`.
    pub fn principal_kinds(&self, from_day: i64, to_day: i64) -> Result<(u64, u64, u64)> {
        self.read(|c| {
            let n = |prefix: &str| -> Result<u64> {
                Ok(uint(
                    c.prepare_cached(
                        "SELECT COUNT(DISTINCT principal) FROM principal_daily \
                     WHERE day >= ?1 AND day <= ?2 AND substr(principal, 1, 2) = ?3",
                    )?
                    .query_row(params![from_day, to_day, prefix], |r| r.get(0))?,
                ))
            };
            Ok((n("u:")?, n("a:")?, n("p:")?))
        })
    }

    /// Distinct accounts and anonymous visitors (not processors) active in days
    /// `from..=to`: DAU/WAU/MAU.
    pub fn active_principals(&self, from_day: i64, to_day: i64) -> Result<u64> {
        self.read(|c| {
            Ok(uint(
                c.prepare_cached(
                    "SELECT COUNT(DISTINCT principal) FROM principal_daily \
                     WHERE day >= ?1 AND day <= ?2 AND substr(principal, 1, 2) != 'p:'",
                )?
                .query_row(params![from_day, to_day], |r| r.get(0))?,
            ))
        })
    }

    /// Distinct accounts and anonymous visitors active per day of `from..=to` (only days
    /// with any).
    pub fn active_per_day(&self, from_day: i64, to_day: i64) -> Result<Vec<(i64, u64)>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT day, COUNT(DISTINCT principal) FROM principal_daily \
                 WHERE day >= ?1 AND day <= ?2 AND substr(principal, 1, 2) != 'p:' \
                 GROUP BY day ORDER BY day",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok((r.get(0)?, uint(r.get(1)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The IPs an anonymous principal was seen from since `from_day` (the IP retention
    /// keeps them), most requests first.
    pub fn anon_ips_of(&self, principal: &str, from_day: i64, limit: usize) -> Result<Vec<String>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT ip, SUM(requests) AS n FROM anon_ips WHERE principal = ?1 AND day >= ?2 \
                 GROUP BY ip ORDER BY n DESC, ip ASC LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(params![principal, from_day, limit as i64], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
    }

    /// `(downloads, downloaders)` in days `from..=to`: completed downloads only (a
    /// `Range: bytes=0-0` probe is no download).
    pub fn download_totals(&self, from_day: i64, to_day: i64) -> Result<(u64, u64)> {
        self.read(|c| {
            Ok(c.prepare_cached(
                "SELECT COUNT(*), COUNT(DISTINCT principal) FROM volume_downloads \
                 WHERE day >= ?1 AND day <= ?2 AND completed",
            )?
            .query_row(params![from_day, to_day], |r| {
                Ok((uint(r.get(0)?), uint(r.get(1)?)))
            })?)
        })
    }

    /// Downloads per series in days `from..=to`, most downloaded first: completed
    /// downloads, their downloaders and distinct volumes; `bytes` is every byte sent,
    /// partial ones included. The title is the most recent spelling seen.
    pub fn series_downloads(&self, from_day: i64, to_day: i64) -> Result<Vec<SeriesDownloads>> {
        self.read(|c| {
            let mut stmt = c.prepare_cached(
                "SELECT series_key, \
                 (SELECT v2.series_title FROM volume_downloads v2 WHERE v2.series_key = v.series_key \
                  AND v2.day >= ?1 AND v2.day <= ?2 ORDER BY v2.day DESC LIMIT 1), \
                 SUM(completed) AS n, COUNT(DISTINCT CASE WHEN completed THEN principal END), \
                 SUM(bytes), COUNT(DISTINCT CASE WHEN completed THEN volume_key END) \
                 FROM volume_downloads v WHERE day >= ?1 AND day <= ?2 AND series_key != '' \
                 GROUP BY series_key ORDER BY n DESC, series_key ASC",
            )?;
            let rows = stmt
                .query_map(params![from_day, to_day], |r| {
                    Ok(SeriesDownloads {
                        series_key: r.get(0)?,
                        title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        downloads: uint(r.get(2)?),
                        downloaders: uint(r.get(3)?),
                        bytes: uint(r.get(4)?),
                        volumes: uint(r.get(5)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Auth failures over `span` (see [`AnalyticsDb::auth_failure_series`]).
    pub fn auth_failure_total(&self, span: &Span) -> Result<u64> {
        Ok(self.auth_failure_series(span)?.iter().map(|(_, n)| n).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::testutil::temp_db;

    const D: TrafficDims<'static> = TrafficDims {
        protocol: "dav",
        method: "GET",
        category: "volume",
        detail: "full",
        status_class: 2,
    };

    fn counts(requests: u64, bytes_out: u64) -> TrafficCounts {
        TrafficCounts {
            requests,
            bytes_out,
            ..Default::default()
        }
    }

    #[test]
    fn traffic_upserts_add_up() {
        let (_d, db) = temp_db();
        db.write(|tx| {
            tx.add_traffic(10, &D, &counts(1, 100))?;
            tx.add_traffic(10, &D, &counts(2, 50))?;
            tx.add_traffic(
                11,
                &TrafficDims {
                    method: "PROPFIND",
                    category: "metadata",
                    detail: "propfind_1",
                    ..D
                },
                &counts(1, 7),
            )
        })
        .unwrap();
        let span = Span {
            granularity: Granularity::Hour,
            from: 10,
            to: 11,
        };
        let all = db
            .traffic(&span, None, false, &TrafficFilter::default())
            .unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].counts.requests, 4);
        assert_eq!(all[0].counts.bytes_out, 157);
        let by = db
            .traffic(
                &span,
                Some(TrafficDim::Method),
                true,
                &TrafficFilter::default(),
            )
            .unwrap();
        assert_eq!(by.len(), 2);
        assert!(
            by.iter()
                .any(|r| r.bucket == Some(10) && r.key == "GET" && r.counts.bytes_out == 150)
        );
        assert!(
            by.iter()
                .any(|r| r.bucket == Some(11) && r.key == "PROPFIND")
        );
        let only = db
            .traffic(
                &span,
                Some(TrafficDim::Detail),
                false,
                &TrafficFilter {
                    category: Some("metadata".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].key, "propfind_1");
        // An empty span answers no rows.
        let none = db
            .traffic(
                &Span {
                    granularity: Granularity::Hour,
                    from: 0,
                    to: 5,
                },
                None,
                false,
                &TrafficFilter::default(),
            )
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn histograms_bucket_and_merge() {
        assert_eq!(latency_bucket(0), 0);
        assert_eq!(latency_bucket(1), 0);
        assert_eq!(latency_bucket(2), 1);
        assert_eq!(latency_bucket(300000), 15);
        assert_eq!(latency_bucket(300001), 16);
        let mut h = [0u64; LATENCY_BUCKETS];
        h[latency_bucket(15)] = 10; // (10, 20]
        assert_eq!(histogram_quantile(&h, 0.5), Some(15.0));
        assert_eq!(histogram_quantile(&[0; LATENCY_BUCKETS], 0.5), None);
        let (_d, db) = temp_db();
        let k = KindDims {
            protocol: "dav",
            method: "GET",
            category: "volume",
        };
        db.write(|tx| {
            tx.add_latency(5, &k, &h, &h)?;
            tx.add_latency(5, &k, &h, &h)
        })
        .unwrap();
        let l = db
            .latency(&Span {
                granularity: Granularity::Hour,
                from: 5,
                to: 5,
            })
            .unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].headers[latency_bucket(15)], 20);
        assert_eq!(parse_hist("not json"), [0; LATENCY_BUCKETS]);
    }

    #[test]
    fn downloads_complete_over_the_day_and_slow_keeps_twenty() {
        let (_d, db) = temp_db();
        let row = |bytes| DownloadRow {
            day: 3,
            principal: "u:alice",
            volume_key: "S/v.cbz",
            series_key: "s",
            series_title: "S",
            bytes,
            size: 1000,
        };
        db.write(|tx| tx.add_download(&row(500))).unwrap();
        let completed = |db: &AnalyticsDb| -> i64 {
            db.read(
                |c| Ok(c.query_row("SELECT completed FROM volume_downloads", [], |r| r.get(0))?),
            )
            .unwrap()
        };
        assert_eq!(completed(&db), 0);
        db.write(|tx| tx.add_download(&row(460))).unwrap();
        assert_eq!(completed(&db), 1);
        assert_eq!(db.download_totals(0, 10).unwrap(), (1, 1));
        let s = db.series_downloads(0, 10).unwrap();
        assert_eq!(s[0].bytes, 960);
        assert_eq!(s[0].title, "S");

        let rows: Vec<SlowRequest> = (0..30)
            .map(|i| SlowRequest {
                at: 7200 + i,
                method: "GET".into(),
                path: format!("/p{i}"),
                status: 200,
                ms: i as u64,
                principal: "u:a".into(),
                ip: "192.0.2.1".into(),
            })
            .collect();
        db.write(|tx| tx.add_slow(&rows)).unwrap();
        let kept = db.slow_requests(0, 100000, 100).unwrap();
        assert_eq!(kept.len(), SLOW_PER_HOUR);
        assert_eq!(kept[0].ms, 29);
        assert_eq!(kept.last().unwrap().ms, 10);
    }

    #[test]
    fn ip_rows_merge_principals_up_to_eight() {
        let (_d, db) = temp_db();
        for i in 0..12 {
            db.write(|tx| {
                tx.add_ip(
                    1,
                    "198.51.100.7",
                    &IpCounts {
                        requests: 1,
                        principals: vec![format!("a:{i:012}")],
                        user_agent: Some(format!("ua{i}")),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        }
        let (n, principals, ua): (i64, String, String) = db
            .read(|c| {
                Ok(c.query_row(
                    "SELECT requests, principals, user_agent FROM ip_daily",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?)
            })
            .unwrap();
        assert_eq!(n, 12);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&principals)
                .unwrap()
                .len(),
            8
        );
        assert_eq!(ua, "ua11");
    }

    #[test]
    fn principals_and_auth_failures_read_back() {
        let (_d, db) = temp_db();
        db.write(|tx| {
            let c = |b| PrincipalCounts {
                requests: 1,
                bytes_out: b,
                bytes_in: 0,
            };
            tx.add_principal(2, "u:alice", "volume", &c(100))?;
            tx.add_principal(2, "u:alice", "metadata", &c(10))?;
            tx.add_principal(2, "a:0123456789ab", "volume", &c(50))?;
            tx.add_principal(2, "p:worker", "processor", &c(500))?;
            tx.see_principal("u:alice", 100, 200)?;
            tx.see_principal("u:alice", 50, 150)?;
            tx.add_anon_ip(2, "a:0123456789ab", "203.0.113.9", 3)?;
            tx.add_auth_failures(48, "203.0.113.9", "bob", 2)?;
            tx.add_auth_failures(49, "203.0.113.9", "", 1)?;
            tx.add_auth_failures(49, "198.51.100.1", "eve", 1)
        })
        .unwrap();
        let p = db.principals(0, 5, 10).unwrap();
        assert_eq!(p[0].principal, "p:worker");
        let alice = p.iter().find(|r| r.principal == "u:alice").unwrap();
        assert_eq!(alice.totals.bytes_out, 110);
        assert_eq!(alice.by_category[0].0, "volume");
        assert_eq!(alice.last_seen, Some(200));
        assert_eq!(db.principal_kinds(0, 5).unwrap(), (1, 1, 1));
        assert_eq!(db.active_principals(0, 5).unwrap(), 2);
        assert_eq!(
            db.anon_ips_of("a:0123456789ab", 0, 5).unwrap(),
            ["203.0.113.9"]
        );
        let hours = Span {
            granularity: Granularity::Hour,
            from: 0,
            to: 100,
        };
        assert_eq!(db.auth_failure_total(&hours).unwrap(), 4);
        let top = db.auth_failure_top_ips(0, 100, 5).unwrap();
        assert_eq!(top[0].ip, "203.0.113.9");
        assert_eq!(top[0].count, 3);
        assert_eq!(top[0].usernames, ["bob"]);
        let users = db.auth_failure_top_usernames(0, 100, 5).unwrap();
        assert_eq!(users[0], ("bob".to_string(), 2));
        let series = db
            .auth_failure_series(&Span {
                granularity: Granularity::Day,
                from: 2,
                to: 2,
            })
            .unwrap();
        assert_eq!(series, [(2, 4)]);
    }

    /// The heaviest principals with their per-category split: the cost does not grow
    /// with how many are asked for (it used to be one scan of the range per principal).
    #[test]
    fn principals_split_by_category_in_one_pass() {
        let (_d, db) = temp_db();
        db.write(|tx| {
            for day in 0..60i64 {
                for p in 0..600u64 {
                    let id = format!("u:p{p:03}");
                    for (cat, bytes) in [("volume", p * 10), ("cover", p)] {
                        tx.add_principal(
                            day,
                            &id,
                            cat,
                            &PrincipalCounts {
                                requests: 1,
                                bytes_out: bytes,
                                bytes_in: 0,
                            },
                        )?;
                    }
                }
            }
            tx.see_principal("u:p599", 5, 50)
        })
        .unwrap();
        let time = |limit: usize| {
            let t = std::time::Instant::now();
            let rows = db.principals(0, 59, limit).unwrap();
            (t.elapsed(), rows)
        };
        time(5);
        let (few, rows5) = time(5);
        let (many, rows) = time(500);
        assert_eq!(rows.len(), 500);
        assert_eq!(rows5[..], rows[..5]);
        let top = &rows[0];
        assert_eq!(top.principal, "u:p599");
        assert_eq!(top.totals.bytes_out, 60 * 599 * 11);
        assert_eq!(top.last_seen, Some(50));
        assert_eq!(
            top.by_category
                .iter()
                .map(|(c, n)| (c.as_str(), n.bytes_out, n.requests))
                .collect::<Vec<_>>(),
            [("volume", 60 * 5990, 60), ("cover", 60 * 599, 60)]
        );
        assert!(rows[1].last_seen.is_none());
        assert!(
            many < few * 10 + std::time::Duration::from_millis(50),
            "500 principals took {many:?}, 5 took {few:?}"
        );
    }
}
