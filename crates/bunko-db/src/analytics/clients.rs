//! The client mix and geography (design §9): requests per (app, client, browser, os,
//! protocol), the principals behind each app, `Origin`s, and countries. The collector's
//! flush writes these (the upserts below); the clients and geo areas add their queries
//! here.

use super::{AnalyticsTx, int};
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
}
