//! The client mix and geography (design §9): requests per (app, client, browser, os,
//! protocol), the principals behind each app and each such combination, `Origin`s,
//! and countries. The collector's flush writes these (the upserts below); the clients
//! and geo API read them back (the queries at the end).

use super::{AnalyticsDb, AnalyticsTx, int, uint};
use crate::error::Result;
use rusqlite::params;

pub(crate) const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS clients_daily (
        day INTEGER NOT NULL, app TEXT NOT NULL, client TEXT NOT NULL, browser TEXT NOT NULL,
        os TEXT NOT NULL, protocol TEXT NOT NULL, requests INTEGER NOT NULL DEFAULT 0,
        bytes_out INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, app, client, browser, os, protocol)
    ) WITHOUT ROWID",
    // IP retention (`retention.ip_days`): an anonymous principal stands for an IP.
    "CREATE TABLE IF NOT EXISTS client_principals (
        day INTEGER NOT NULL, app TEXT NOT NULL, principal TEXT NOT NULL,
        PRIMARY KEY (day, app, principal)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS origins_daily (
        day INTEGER NOT NULL, origin TEXT NOT NULL, requests INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, origin)
    ) WITHOUT ROWID",
    // IP retention, like `client_principals`: who was behind each (app, client,
    // browser, os, protocol), so every breakdown can count distinct principals.
    "CREATE TABLE IF NOT EXISTS client_mix_principals (
        day INTEGER NOT NULL, app TEXT NOT NULL, client TEXT NOT NULL, browser TEXT NOT NULL,
        os TEXT NOT NULL, protocol TEXT NOT NULL, principal TEXT NOT NULL,
        PRIMARY KEY (day, app, client, browser, os, protocol, principal)
    ) WITHOUT ROWID",
    "CREATE TABLE IF NOT EXISTS geo_daily (
        day INTEGER NOT NULL, country TEXT NOT NULL, requests INTEGER NOT NULL DEFAULT 0,
        bytes_out INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, country)
    ) WITHOUT ROWID",
];

/// Distinct origins kept per day; the rest add to [`OTHER_ORIGIN`].
pub const ORIGINS_PER_DAY: i64 = 500;
pub const OTHER_ORIGIN: &str = "(other)";

/// The key of a `clients_daily` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientDims<'a> {
    pub app: &'a str,
    pub client: &'a str,
    pub browser: &'a str,
    pub os: &'a str,
    pub protocol: &'a str,
}

impl AnalyticsTx<'_> {
    pub fn add_client(
        &self,
        day: i64,
        d: &ClientDims<'_>,
        requests: u64,
        bytes_out: u64,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO clients_daily (day, app, client, browser, os, protocol, requests, bytes_out) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                 ON CONFLICT (day, app, client, browser, os, protocol) DO UPDATE SET \
                 requests = requests + excluded.requests, bytes_out = bytes_out + excluded.bytes_out",
            )?
            .execute(params![
                day,
                d.app,
                d.client,
                d.browser,
                d.os,
                d.protocol,
                int(requests),
                int(bytes_out)
            ])?;
        Ok(())
    }

    pub fn add_client_principal(&self, day: i64, app: &str, principal: &str) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO client_principals (day, app, principal) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![day, app, principal])?;
        Ok(())
    }

    /// Count requests from `origin` on `day`; past [`ORIGINS_PER_DAY`] distinct origins
    /// a new one counts as [`OTHER_ORIGIN`].
    pub fn add_origin(&self, day: i64, origin: &str, requests: u64) -> Result<()> {
        let known: bool = self
            .conn
            .prepare_cached(
                "SELECT EXISTS (SELECT 1 FROM origins_daily WHERE day = ?1 AND origin = ?2)",
            )?
            .query_row(params![day, origin], |r| r.get(0))?;
        let origin = if known {
            origin
        } else {
            let n: i64 = self
                .conn
                .prepare_cached(
                    "SELECT COUNT(*) FROM origins_daily WHERE day = ?1 AND origin != ?2",
                )?
                .query_row(params![day, OTHER_ORIGIN], |r| r.get(0))?;
            if n >= ORIGINS_PER_DAY {
                OTHER_ORIGIN
            } else {
                origin
            }
        };
        self.conn
            .prepare_cached(
                "INSERT INTO origins_daily (day, origin, requests) VALUES (?1, ?2, ?3) \
                 ON CONFLICT (day, origin) DO UPDATE SET requests = requests + excluded.requests",
            )?
            .execute(params![day, origin, int(requests)])?;
        Ok(())
    }

    /// `principal` used the (app, client, browser, os, protocol) of `d` on `day`.
    pub fn add_client_mix_principal(
        &self,
        day: i64,
        d: &ClientDims<'_>,
        principal: &str,
    ) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO client_mix_principals \
                 (day, app, client, browser, os, protocol, principal) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?
            .execute(params![
                day, d.app, d.client, d.browser, d.os, d.protocol, principal
            ])?;
        Ok(())
    }

    pub fn add_geo(&self, day: i64, country: &str, requests: u64, bytes_out: u64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO geo_daily (day, country, requests, bytes_out) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (day, country) DO UPDATE SET requests = requests + excluded.requests, \
                 bytes_out = bytes_out + excluded.bytes_out",
            )?
            .execute(params![day, country, int(requests), int(bytes_out)])?;
        Ok(())
    }
}

/// A `clients_daily` total over a range of days.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixRow {
    pub app: String,
    pub client: String,
    pub browser: String,
    pub os: String,
    pub protocol: String,
    pub requests: u64,
    pub bytes_out: u64,
}

/// A column of the client mix to count distinct principals by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientDim {
    App,
    Client,
    Browser,
    Os,
    Protocol,
}

impl ClientDim {
    fn column(self) -> &'static str {
        match self {
            ClientDim::App => "app",
            ClientDim::Client => "client",
            ClientDim::Browser => "browser",
            ClientDim::Os => "os",
            ClientDim::Protocol => "protocol",
        }
    }
}

impl AnalyticsDb {
    /// Requests and bytes per (app, client, browser, os, protocol) over days
    /// `from..=to`, most requests first.
    pub fn client_mix(&self, from_day: i64, to_day: i64) -> Result<Vec<MixRow>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT app, client, browser, os, protocol, SUM(requests), SUM(bytes_out) \
                 FROM clients_daily WHERE day BETWEEN ?1 AND ?2 \
                 GROUP BY app, client, browser, os, protocol ORDER BY 6 DESC",
            )?;
            let rows = st
                .query_map(params![from_day, to_day], |r| {
                    Ok(MixRow {
                        app: r.get(0)?,
                        client: r.get(1)?,
                        browser: r.get(2)?,
                        os: r.get(3)?,
                        protocol: r.get(4)?,
                        requests: uint(r.get(5)?),
                        bytes_out: uint(r.get(6)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Distinct principals per value of `dim` over days `from..=to` (only within the
    /// IP retention: older rows are pruned), optionally of one protocol.
    pub fn client_principal_counts(
        &self,
        from_day: i64,
        to_day: i64,
        dim: ClientDim,
        protocol: Option<&str>,
    ) -> Result<Vec<(String, u64)>> {
        let col = dim.column();
        self.read(|c| {
            let sql = format!(
                "SELECT {col}, COUNT(DISTINCT principal) FROM client_mix_principals \
                 WHERE day BETWEEN ?1 AND ?2 AND (?3 IS NULL OR protocol = ?3) GROUP BY {col}"
            );
            let mut st = c.prepare_cached(&sql)?;
            let rows = st
                .query_map(params![from_day, to_day, protocol], |r| {
                    Ok((r.get::<_, String>(0)?, uint(r.get(1)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// `(day, app, requests)` over days `from..=to`.
    pub fn app_requests_per_day(
        &self,
        from_day: i64,
        to_day: i64,
    ) -> Result<Vec<(i64, String, u64)>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT day, app, SUM(requests) FROM clients_daily \
                 WHERE day BETWEEN ?1 AND ?2 GROUP BY day, app",
            )?;
            let rows = st
                .query_map(params![from_day, to_day], |r| {
                    Ok((r.get(0)?, r.get(1)?, uint(r.get(2)?)))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// The `limit` origins with the most requests over days `from..=to`.
    pub fn top_origins(
        &self,
        from_day: i64,
        to_day: i64,
        limit: usize,
    ) -> Result<Vec<(String, u64)>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT origin, SUM(requests) AS n FROM origins_daily \
                 WHERE day BETWEEN ?1 AND ?2 GROUP BY origin ORDER BY n DESC, origin LIMIT ?3",
            )?;
            let rows = st
                .query_map(
                    params![from_day, to_day, i64::try_from(limit).unwrap_or(i64::MAX)],
                    |r| Ok((r.get(0)?, uint(r.get(1)?))),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// `(country, requests, bytes_out)` over days `from..=to`, most requests first.
    pub fn countries(&self, from_day: i64, to_day: i64) -> Result<Vec<(String, u64, u64)>> {
        self.read(|c| {
            let mut st = c.prepare_cached(
                "SELECT country, SUM(requests) AS n, SUM(bytes_out) FROM geo_daily \
                 WHERE day BETWEEN ?1 AND ?2 GROUP BY country ORDER BY n DESC, country",
            )?;
            let rows = st
                .query_map(params![from_day, to_day], |r| {
                    Ok((r.get(0)?, uint(r.get(1)?), uint(r.get(2)?)))
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

    #[test]
    fn origins_cap_per_day() {
        let (_d, db) = temp_db();
        db.write(|tx| {
            for i in 0..(ORIGINS_PER_DAY + 3) {
                tx.add_origin(1, &format!("https://o{i}.example.com"), 1)?;
            }
            // A known origin still counts as itself.
            tx.add_origin(1, "https://o0.example.com", 1)
        })
        .unwrap();
        let (n, other, o0): (i64, i64, i64) = db
            .read(|c| {
                Ok((
                    c.query_row("SELECT COUNT(*) FROM origins_daily", [], |r| r.get(0))?,
                    c.query_row(
                        "SELECT requests FROM origins_daily WHERE origin = '(other)'",
                        [],
                        |r| r.get(0),
                    )?,
                    c.query_row(
                        "SELECT requests FROM origins_daily WHERE origin = 'https://o0.example.com'",
                        [],
                        |r| r.get(0),
                    )?,
                ))
            })
            .unwrap();
        assert_eq!(n, ORIGINS_PER_DAY + 1);
        assert_eq!(other, 3);
        assert_eq!(o0, 2);
    }

    #[test]
    fn mix_principals_and_countries() {
        let (_d, db) = temp_db();
        let rclone = ClientDims {
            app: "webdav-client",
            client: "rclone",
            browser: "-",
            os: "-",
            protocol: "dav",
        };
        let hosted = ClientDims {
            app: "reader-hosted",
            client: "browser",
            browser: "firefox",
            os: "linux",
            protocol: "dav",
        };
        let web = ClientDims {
            protocol: "api",
            app: "bunko-web",
            ..hosted
        };
        db.write(|tx| {
            tx.add_client(10, &rclone, 5, 500)?;
            tx.add_client(11, &rclone, 1, 100)?;
            tx.add_client(11, &hosted, 7, 70)?;
            tx.add_client(11, &web, 2, 20)?;
            // Out of range.
            tx.add_client(12, &web, 100, 100)?;
            for (day, d, p) in [
                (10, &rclone, "a:1"),
                (11, &rclone, "a:1"),
                (11, &hosted, "u:x"),
                (11, &hosted, "u:x"),
                (11, &web, "u:x"),
                (11, &web, "u:y"),
            ] {
                tx.add_client_mix_principal(day, d, p)?;
            }
            tx.add_origin(11, "https://reader.example.com", 7)?;
            tx.add_origin(10, "http://localhost:5173", 1)?;
            tx.add_geo(10, "JP", 3, 30)?;
            tx.add_geo(11, "JP", 1, 10)?;
            tx.add_geo(11, "LAN", 9, 1)?;
            tx.add_geo(12, "DE", 9, 1)
        })
        .unwrap();
        let mix = db.client_mix(10, 11).unwrap();
        assert_eq!(mix.len(), 3);
        assert_eq!((mix[0].app.as_str(), mix[0].requests), ("reader-hosted", 7));
        let rc = mix.iter().find(|r| r.client == "rclone").unwrap();
        assert_eq!((rc.requests, rc.bytes_out), (6, 600));

        let by_client: std::collections::HashMap<_, _> = db
            .client_principal_counts(10, 11, ClientDim::Client, None)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(by_client["rclone"], 1);
        assert_eq!(by_client["browser"], 2);
        let dav: Vec<_> = db
            .client_principal_counts(10, 11, ClientDim::App, Some("dav"))
            .unwrap();
        assert_eq!(dav.len(), 2);
        assert!(dav.contains(&("reader-hosted".to_string(), 1)));
        let per_day = db.app_requests_per_day(10, 11).unwrap();
        assert!(per_day.contains(&(10, "webdav-client".to_string(), 5)));
        assert!(per_day.contains(&(11, "bunko-web".to_string(), 2)));
        assert_eq!(
            db.top_origins(10, 11, 1).unwrap(),
            [("https://reader.example.com".to_string(), 7)]
        );
        assert_eq!(
            db.countries(10, 11).unwrap(),
            [("LAN".to_string(), 9, 1), ("JP".to_string(), 4, 40)]
        );
    }
}
