//! The client mix (design §9): which app, client, browser and OS a request came from,
//! hand-rolled from `Origin` and `User-Agent` (no UA database).
//!
//! FOUNDATION STUB (area B4 fills it in): [`Clients::classify`] answers
//! [`ClientInfo::UNKNOWN`] for every request and [`api::clients`] says "not collected
//! yet". The collector already calls `classify` once per request (outside its lock)
//! and folds the answer into `clients_daily` / `client_principals`; `origins_daily`
//! counts every cross-origin `Origin` on its own.

use crate::core::Core;

/// What a request's client was. Every field is one of the fixed values of design §9
/// (`app`: `reader-hosted`, `reader-other`, `bunko-web`, `webdav-client`, `api-client`,
/// `unknown`; `client`: `browser` or a named client, `other`, `none`; `browser` and
/// `os`: a name, `other` or `-`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientInfo {
    pub app: &'static str,
    pub client: &'static str,
    pub browser: &'static str,
    pub os: &'static str,
}

impl ClientInfo {
    pub const UNKNOWN: ClientInfo = ClientInfo {
        app: "unknown",
        client: "other",
        browser: "-",
        os: "-",
    };
}

/// The request facts classification reads. `user_agent` is already cut to 256 chars.
#[derive(Debug, Clone, Copy)]
pub struct ClientRequest<'a> {
    pub user_agent: Option<&'a str>,
    pub origin: Option<&'a str>,
    /// The `Host` header (same-origin pages compare `Origin` with it).
    pub host: Option<&'a str>,
    /// [`super::classify`]'s protocol (`dav`, `api`, `web`, `processor`).
    pub protocol: &'static str,
    /// The request path, percent-decoded.
    pub path: &'a str,
}

/// The classifier. Reads the reader origin from the live config (`catalog.reader_url`,
/// the CORS defaults), so a settings change applies to the next request.
pub struct Clients {
    #[allow(dead_code)] // B4: the reader origins come from the live config.
    core: Core,
}

impl Clients {
    pub fn new(core: Core) -> Clients {
        Clients { core }
    }

    /// Cheap and synchronous: called once per request, outside the collector's lock.
    pub fn classify(&self, _r: &ClientRequest<'_>) -> ClientInfo {
        ClientInfo::UNKNOWN
    }
}

pub mod api {
    use crate::analytics::Analytics;
    use crate::analytics::api::{ApiQuery, not_collected};
    use axum::response::Response;

    /// `GET /_admin/api/analytics/clients` (design §10).
    pub async fn clients(a: &Analytics, q: &ApiQuery) -> Response {
        not_collected(a, q).await
    }
}
