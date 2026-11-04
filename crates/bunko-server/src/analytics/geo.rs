//! Rough geography (design §9): the country of a client IP from DB-IP "IP to Country
//! Lite" (CC BY 4.0), downloaded on demand when `analytics.geo.enabled` (opt-in).
//!
//! FOUNDATION STUB (area B4 fills it in): [`Geo::country`] answers `None` and the API
//! says "not collected yet". The collector already calls `country` once per request
//! (outside its lock) for `ip_daily.country`, `geo_daily` and the crawl observations.

use crate::analytics::Analytics;
use crate::core::Core;
use bunko_core::config::AnalyticsConfig;
use std::net::IpAddr;
use tokio_util::sync::CancellationToken;

/// An ISO 3166-1 alpha-2 code, or `LAN` (loopback, private, link-local). `Copy`, so the
/// per-request path never allocates for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Country {
    code: [u8; 3],
    len: u8,
}

impl Country {
    pub const LAN: Country = Country {
        code: *b"LAN",
        len: 3,
    };

    /// 2 or 3 ASCII letters, upper-cased; anything else is `None`.
    pub fn new(code: &str) -> Option<Country> {
        let b = code.trim().as_bytes();
        if !(2..=3).contains(&b.len()) || !b.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        let mut out = [0u8; 3];
        for (o, c) in out.iter_mut().zip(b) {
            *o = c.to_ascii_uppercase();
        }
        Some(Country {
            code: out,
            len: b.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        // Built from ASCII letters only (`new`, `LAN`).
        std::str::from_utf8(&self.code[..usize::from(self.len)]).unwrap_or("")
    }
}

impl std::fmt::Display for Country {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The lookup and its downloader.
pub struct Geo {
    #[allow(dead_code)] // B4: `<storage base>/analytics/country.mmdb`, the live settings.
    core: Core,
}

impl Geo {
    pub fn new(core: Core) -> Geo {
        Geo { core }
    }

    /// Start the downloader / monthly refresh (when enabled). Called once at start.
    pub fn start(&self, _a: &Analytics, _stop: CancellationToken) {}

    /// Synchronous and cheap (an in-memory LRU in front of the mmdb): the country of
    /// `ip`, `None` when off, not loaded or unknown.
    pub fn country(&self, _ip: &IpAddr) -> Option<Country> {
        None
    }

    /// Download the database again now (`POST .../geo/refresh`).
    pub fn refresh(&self) {}

    /// `analytics.geo` changed (enabled toggled, URL): start or stop accordingly.
    pub fn settings_changed(&self, _cfg: &AnalyticsConfig) {}
}

pub mod api {
    use crate::analytics::Analytics;
    use crate::analytics::api::{ApiQuery, not_collected};
    use axum::response::Response;

    /// `GET /_admin/api/analytics/geo` (design §10).
    pub async fn geo(a: &Analytics, q: &ApiQuery) -> Response {
        not_collected(a, q).await
    }

    /// `POST /_admin/api/analytics/geo/refresh`.
    pub async fn refresh(a: &Analytics) -> Response {
        a.geo().refresh();
        crate::admin::ok(serde_json::json!({"ok": false, "available": false}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_codes() {
        assert_eq!(Country::new("jp").unwrap().as_str(), "JP");
        assert_eq!(Country::LAN.as_str(), "LAN");
        assert!(Country::new("J").is_none());
        assert!(Country::new("J1").is_none());
        assert!(Country::new("ABCD").is_none());
    }
}
