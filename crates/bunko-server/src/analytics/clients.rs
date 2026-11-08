//! The client mix (design §9): which app, client, browser and OS a request came from,
//! hand-rolled from `Origin` and `User-Agent` (no UA database).
//!
//! - **app** says what kind of program made the request. `reader-hosted` is a request
//!   whose `Origin` is the configured reader (`catalog.reader_url`'s origin, or one of
//!   the hosted `https://` origins of the default CORS list; never a literal here).
//!   `reader-other` is a Mokuro Reader build served from another origin (a dev server,
//!   a self-hosted copy): a cross-origin browser request to what the reader uses, the
//!   WebDAV tree and `/login/api/*`. The reader sends no header of its own, so its
//!   `Origin` is its only mark. `bunko-web` is Bunko's own pages and their API calls
//!   (same origin, or no `Origin`, from a browser). `webdav-client` is a DAV request
//!   from a program that is not a browser (rclone, davfs2, the Windows mini-redirector,
//!   Finder, ...). `api-client` is any other program calling the JSON API or the
//!   pages, or another web app calling cross-origin. `unknown` is a bot, or no user
//!   agent outside WebDAV.
//! - **client**, **browser** and **os** come from the user agent alone, so they are
//!   cached per user agent (a small two-generation map: browsers and DAV clients send
//!   a handful of distinct strings, a flood of new ones only costs the parse).
//!
//! [`Clients::classify`] runs once per request outside the collector's lock and
//! allocates only when it caches a user agent it has not seen.

use crate::core::Core;
use bunko_core::config::CorsConfig;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::OnceLock;

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

/// The `app` values.
pub mod app {
    pub const READER_HOSTED: &str = "reader-hosted";
    pub const READER_OTHER: &str = "reader-other";
    pub const BUNKO_WEB: &str = "bunko-web";
    pub const WEBDAV_CLIENT: &str = "webdav-client";
    pub const API_CLIENT: &str = "api-client";
    pub const UNKNOWN: &str = "unknown";
    pub const ALL: [&str; 6] = [
        READER_HOSTED,
        READER_OTHER,
        BUNKO_WEB,
        WEBDAV_CLIENT,
        API_CLIENT,
        UNKNOWN,
    ];
}

/// The `client` values.
pub mod client {
    pub const BROWSER: &str = "browser";
    pub const RCLONE: &str = "rclone";
    pub const DAVFS2: &str = "davfs2";
    pub const WINDOWS_WEBDAV: &str = "windows-webdav";
    pub const MACOS_FINDER: &str = "macos-finder";
    pub const CYBERDUCK: &str = "cyberduck";
    pub const GVFS: &str = "gvfs";
    pub const KODI: &str = "kodi";
    pub const NEXTCLOUD: &str = "nextcloud";
    pub const WINSCP: &str = "winscp";
    pub const CURL: &str = "curl";
    pub const WGET: &str = "wget";
    pub const PYTHON: &str = "python";
    pub const GO: &str = "go";
    pub const NODE: &str = "node";
    pub const BOT: &str = "bot";
    pub const OTHER: &str = "other";
    pub const NONE: &str = "none";
}

/// The `browser` values.
pub mod browser {
    pub const FIREFOX: &str = "firefox";
    pub const CHROME: &str = "chrome";
    pub const EDGE: &str = "edge";
    pub const SAFARI: &str = "safari";
    pub const OPERA: &str = "opera";
    pub const SAMSUNG: &str = "samsung";
    pub const OTHER: &str = "other";
    pub const NOT_A_BROWSER: &str = "-";
}

/// The `os` values.
pub mod os {
    pub const WINDOWS: &str = "windows";
    pub const MACOS: &str = "macos";
    pub const IOS: &str = "ios";
    pub const ANDROID: &str = "android";
    pub const LINUX: &str = "linux";
    pub const CHROMEOS: &str = "chromeos";
    pub const OTHER: &str = "other";
    pub const UNKNOWN: &str = "-";
}

/// The display name of a value of `dim` (`app`, `client`, `browser`, `os`), as the
/// admin UI names it.
pub fn label(dim: &str, key: &str) -> String {
    let s = match (dim, key) {
        ("app", app::READER_HOSTED) => "Mokuro Reader (hosted)",
        ("app", app::READER_OTHER) => "Mokuro Reader (other origin)",
        ("app", app::BUNKO_WEB) => "Bunko web pages",
        ("app", app::WEBDAV_CLIENT) => "WebDAV clients",
        ("app", app::API_CLIENT) => "API clients",
        ("app", app::UNKNOWN) => "Unknown",
        ("client", client::BROWSER) => "Browser",
        ("client", client::RCLONE) => "rclone",
        ("client", client::DAVFS2) => "davfs2",
        ("client", client::WINDOWS_WEBDAV) => "Windows WebDAV",
        ("client", client::MACOS_FINDER) => "macOS Finder",
        ("client", client::CYBERDUCK) => "Cyberduck",
        ("client", client::GVFS) => "GNOME Files (gvfs)",
        ("client", client::KODI) => "Kodi",
        ("client", client::NEXTCLOUD) => "Nextcloud",
        ("client", client::WINSCP) => "WinSCP",
        ("client", client::CURL) => "curl",
        ("client", client::WGET) => "wget",
        ("client", client::PYTHON) => "Python",
        ("client", client::GO) => "Go",
        ("client", client::NODE) => "Node.js",
        ("client", client::BOT) => "Crawler bots",
        ("client", client::OTHER) => "Other",
        ("client", client::NONE) => "No user agent",
        ("browser", browser::FIREFOX) => "Firefox",
        ("browser", browser::CHROME) => "Chrome",
        ("browser", browser::EDGE) => "Edge",
        ("browser", browser::SAFARI) => "Safari",
        ("browser", browser::OPERA) => "Opera",
        ("browser", browser::SAMSUNG) => "Samsung Internet",
        ("browser", browser::OTHER) => "Other",
        ("browser", browser::NOT_A_BROWSER) => "Not a browser",
        ("os", os::WINDOWS) => "Windows",
        ("os", os::MACOS) => "macOS",
        ("os", os::IOS) => "iOS",
        ("os", os::ANDROID) => "Android",
        ("os", os::LINUX) => "Linux",
        ("os", os::CHROMEOS) => "ChromeOS",
        ("os", os::OTHER) => "Other",
        ("os", os::UNKNOWN) => "Unknown",
        _ => return key.to_string(),
    };
    s.to_string()
}

/// What the user agent alone says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Agent {
    pub client: &'static str,
    pub browser: &'static str,
    pub os: &'static str,
}

/// User agents kept per cache generation (two generations: at most twice this).
const AGENT_CACHE: usize = 512;

#[derive(Default)]
struct AgentCache {
    current: HashMap<Box<str>, Agent>,
    previous: HashMap<Box<str>, Agent>,
}

impl AgentCache {
    fn get(&mut self, ua: &str) -> Option<Agent> {
        if let Some(a) = self.current.get(ua) {
            return Some(*a);
        }
        let a = *self.previous.get(ua)?;
        self.put(ua, a);
        Some(a)
    }

    fn put(&mut self, ua: &str, a: Agent) {
        if self.current.len() >= AGENT_CACHE {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(ua.into(), a);
    }
}

/// The classifier. Reads the reader origin from the live config (`catalog.reader_url`,
/// the CORS defaults), so a settings change applies to the next request.
pub struct Clients {
    core: Core,
    agents: Mutex<AgentCache>,
}

impl Clients {
    pub fn new(core: Core) -> Clients {
        Clients {
            core,
            agents: Mutex::new(AgentCache::default()),
        }
    }

    /// Cheap and synchronous: called once per request, outside the collector's lock.
    pub fn classify(&self, r: &ClientRequest<'_>) -> ClientInfo {
        let agent = self.agent(r.user_agent);
        ClientInfo {
            app: self.app_of(r, &agent),
            client: agent.client,
            browser: agent.browser,
            os: agent.os,
        }
    }

    fn agent(&self, ua: Option<&str>) -> Agent {
        let Some(ua) = ua.map(str::trim).filter(|u| !u.is_empty()) else {
            return parse_agent(None);
        };
        if let Some(a) = self.agents.lock().get(ua) {
            return a;
        }
        let a = parse_agent(Some(ua));
        self.agents.lock().put(ua, a);
        a
    }

    fn app_of(&self, r: &ClientRequest<'_>, agent: &Agent) -> &'static str {
        match r.origin.map(str::trim).filter(|o| !o.is_empty()) {
            // The config is read only for a cross-origin request.
            Some(origin) if cross_origin(origin, r.host) => {
                let cfg = self.core.config.read();
                cross_origin_app(origin, r, agent, &cfg.catalog.reader_url, &cfg.cors)
            }
            _ => local_app(r, agent),
        }
    }
}

/// The app of a request whose `Origin` is another origin than the server's.
fn cross_origin_app(
    origin: &str,
    r: &ClientRequest<'_>,
    agent: &Agent,
    reader_url: &str,
    cors: &CorsConfig,
) -> &'static str {
    if is_hosted_reader(origin, reader_url) {
        return app::READER_HOSTED;
    }
    if r.protocol == super::classify::PROTOCOL_DAV {
        return app::READER_OTHER;
    }
    // A browser on Bunko's own pages and APIs (its sign-in included) from an origin CORS
    // does not allow: no other web app can read those answers, so this is Bunko's own
    // page behind a proxy that rewrote `Host` and sent no `X-Forwarded-Host`.
    if agent.client == client::BROWSER && !cors.is_origin_allowed(origin) {
        return app::BUNKO_WEB;
    }
    if reader_api_path(r.path) {
        return app::READER_OTHER;
    }
    app::API_CLIENT
}

/// The app of a request with no `Origin`, or the server's own.
fn local_app(r: &ClientRequest<'_>, agent: &Agent) -> &'static str {
    match agent.client {
        client::BROWSER => app::BUNKO_WEB,
        client::BOT => app::UNKNOWN,
        _ if r.protocol == super::classify::PROTOCOL_DAV => app::WEBDAV_CLIENT,
        client::NONE => app::UNKNOWN,
        _ => app::API_CLIENT,
    }
}

/// Is `origin` (`scheme://host[:port]`) another origin than the `Host` the request
/// came to? The CSRF rule ([`crate::http::csrf::origin_names_host`]): a `Host` without
/// a port (a proxy's `proxy_set_header Host $host`) matches any port of that name. The
/// layer has already dropped an `Origin` naming `X-Forwarded-Host`. No `Host` (or an
/// opaque `null` origin): yes.
pub fn cross_origin(origin: &str, host: Option<&str>) -> bool {
    !host.is_some_and(|h| crate::http::csrf::origin_names_host(origin.trim(), h))
}

/// `scheme://authority` of a URL (the origin a browser would send), without a path.
fn origin_of(url: &str) -> Option<&str> {
    let url = url.trim();
    let after = url.find("://")? + 3;
    let end = url[after..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |i| after + i);
    (end > after).then(|| &url[..end])
}

/// The hosted reader origins of the default CORS list (its `https://` entries without
/// a wildcard; the `localhost` ones are development readers).
fn default_hosted_origins() -> &'static [String] {
    static HOSTED: OnceLock<Vec<String>> = OnceLock::new();
    HOSTED.get_or_init(|| {
        CorsConfig::default()
            .allowed_origins
            .into_iter()
            .filter(|o| o.starts_with("https://") && !o.contains('*'))
            .collect()
    })
}

/// Is `origin` the configured reader's (or a hosted default one)?
pub fn is_hosted_reader(origin: &str, reader_url: &str) -> bool {
    let origin = origin.trim().trim_end_matches('/');
    origin_of(reader_url).is_some_and(|r| r.eq_ignore_ascii_case(origin))
        || default_hosted_origins()
            .iter()
            .any(|d| d.trim_end_matches('/').eq_ignore_ascii_case(origin))
}

/// The JSON endpoints the reader calls besides WebDAV (`/login/api/me`, the token).
fn reader_api_path(path: &str) -> bool {
    path.starts_with("/login/api/") || path.starts_with("/catalog/api/manifest")
}

/// The longest prefix of a user agent that is looked at (they are cut to 256 chars
/// before; this bounds the lower-case copy on the stack).
const UA_SCAN: usize = 512;

/// A user agent and its ASCII-lower-cased copy (on the stack): each test is one
/// `str::contains`.
struct Ua<'a> {
    raw: &'a str,
    lower: &'a str,
}

impl Ua<'_> {
    /// Case-sensitive (UA product tokens have a fixed spelling).
    fn has(&self, token: &str) -> bool {
        self.raw.contains(token)
    }

    /// Any case; `token` is lower case.
    fn has_ci(&self, token: &str) -> bool {
        self.lower.contains(token)
    }
}

/// Programs that name themselves first in their user agent: prefix (any case), the
/// client, and the OS the program implies when the agent does not name one.
const NAMED_PREFIXES: &[(&str, &str, Option<&str>)] = &[
    ("rclone/", client::RCLONE, None),
    ("davfs2/", client::DAVFS2, Some(os::LINUX)),
    (
        "microsoft-webdav-miniredir",
        client::WINDOWS_WEBDAV,
        Some(os::WINDOWS),
    ),
    ("webdavfs/", client::MACOS_FINDER, Some(os::MACOS)),
    ("webdavlib/", client::MACOS_FINDER, Some(os::MACOS)),
    ("cyberduck/", client::CYBERDUCK, None),
    ("mountain duck/", client::CYBERDUCK, None),
    ("gvfs/", client::GVFS, Some(os::LINUX)),
    ("kodi/", client::KODI, None),
    ("xbmc/", client::KODI, None),
    ("winscp/", client::WINSCP, Some(os::WINDOWS)),
    ("curl/", client::CURL, None),
    ("wget/", client::WGET, None),
    ("python", client::PYTHON, None),
    ("aiohttp/", client::PYTHON, None),
    ("go-http-client/", client::GO, None),
    ("node-fetch", client::NODE, None),
    ("axios/", client::NODE, None),
    ("undici", client::NODE, None),
    ("got (", client::NODE, None),
];

/// Marks of a crawler (lower case): `Googlebot/2.1`, `bingbot/2.0;`, `(... +http://...)`.
const BOT_MARKS: &[&str] = &[
    "bot/",
    "bot;",
    "bot)",
    "bot-",
    "crawler",
    "spider",
    "slurp",
    "facebookexternalhit",
    "ia_archiver",
    "+http",
    "zgrab",
    "masscan",
    "censysinspect",
];

pub(crate) fn parse_agent(ua: Option<&str>) -> Agent {
    let Some(raw) = ua.map(str::trim).filter(|u| !u.is_empty()) else {
        return Agent {
            client: client::NONE,
            browser: browser::NOT_A_BROWSER,
            os: os::UNKNOWN,
        };
    };
    let raw = match raw.char_indices().nth(UA_SCAN) {
        Some((i, _)) => &raw[..i],
        None => raw,
    };
    let mut buf = [0u8; UA_SCAN * 4];
    let n = raw.len().min(buf.len());
    buf[..n].copy_from_slice(&raw.as_bytes()[..n]);
    buf[..n].make_ascii_lowercase();
    // Lower-casing ASCII keeps UTF-8 valid, and `raw` was cut at a char boundary.
    let lower = std::str::from_utf8(&buf[..n]).unwrap_or("");
    let ua = Ua { raw, lower };
    let named = NAMED_PREFIXES
        .iter()
        .find(|(p, _, _)| lower.starts_with(p))
        .map(|(_, c, implied)| (*c, *implied))
        .or_else(|| {
            // Node's own fetch says just "node".
            if lower == "node" {
                Some((client::NODE, None))
            } else if ua.has_ci("mirall/") || ua.has_ci("nextcloud") {
                // The desktop client ("Mozilla/5.0 (Windows) mirall/3.10 (Nextcloud,
                // ...)") and the mobile apps ("Nextcloud-android/3.27").
                Some((client::NEXTCLOUD, None))
            } else {
                None
            }
        });
    if let Some((c, implied)) = named {
        return Agent {
            client: c,
            browser: browser::NOT_A_BROWSER,
            os: os_of(&ua).or(implied).unwrap_or(os::UNKNOWN),
        };
    }
    if BOT_MARKS.iter().any(|m| ua.has_ci(m)) {
        return Agent {
            client: client::BOT,
            browser: browser::NOT_A_BROWSER,
            os: os::UNKNOWN,
        };
    }
    let engine =
        ua.has("AppleWebKit/") || ua.has("Gecko/") || ua.has("Trident/") || ua.has("Presto/");
    if (raw.starts_with("Mozilla/") || raw.starts_with("Opera/")) && engine {
        return Agent {
            client: client::BROWSER,
            browser: browser_of(&ua),
            os: os_of(&ua).unwrap_or(os::OTHER),
        };
    }
    Agent {
        client: client::OTHER,
        browser: browser::NOT_A_BROWSER,
        os: os_of(&ua).unwrap_or(os::UNKNOWN),
    }
}

/// The browser family. Chromium-based browsers name Chrome and Safari too, so the
/// rarer marks are looked for first.
fn browser_of(ua: &Ua<'_>) -> &'static str {
    let any = |tokens: &[&str]| tokens.iter().any(|t| ua.has(t));
    if ua.has("SamsungBrowser/") {
        browser::SAMSUNG
    } else if any(&["Edg/", "EdgA/", "EdgiOS/", "Edge/"]) {
        browser::EDGE
    } else if any(&["OPR/", "OPX/", "OPT/", "OPiOS/", "Opera"]) {
        browser::OPERA
    } else if any(&["Firefox/", "FxiOS/"]) {
        browser::FIREFOX
    } else if any(&["Chrome/", "CriOS/", "Chromium/"]) {
        browser::CHROME
    } else if ua.has("Safari/") || (ua.has("AppleWebKit/") && any(&["iPhone", "iPad", "Macintosh"]))
    {
        browser::SAFARI
    } else {
        browser::OTHER
    }
}

/// The OS family, when the agent names one. Android and ChromeOS agents say Linux,
/// iOS ones say "like Mac OS X": the narrower marks first.
fn os_of(ua: &Ua<'_>) -> Option<&'static str> {
    let any = |tokens: &[&str]| tokens.iter().any(|t| ua.has(t));
    if ua.has_ci("windows") {
        Some(os::WINDOWS)
    } else if any(&["iPhone", "iPad", "iPod", "iOS"]) {
        Some(os::IOS)
    } else if ua.has_ci("android") {
        Some(os::ANDROID)
    } else if ua.has("CrOS") {
        Some(os::CHROMEOS)
    } else if any(&["Macintosh", "Mac OS X", "Darwin", "macOS"]) {
        Some(os::MACOS)
    } else if any(&["Linux", "Ubuntu", "Fedora"]) {
        Some(os::LINUX)
    } else if any(&["BSD", "SunOS"]) {
        Some(os::OTHER)
    } else {
        None
    }
}

pub mod api {
    //! `GET /_admin/api/analytics/clients` (design §10): the mix by app, client,
    //! browser, OS and protocol (requests, bytes sent, distinct principals), the
    //! WebDAV clients on their own, requests by app per UTC day, and the top
    //! cross-origin `Origin`s.

    use super::{app, label};
    use crate::analytics::Analytics;
    use crate::analytics::api::{ApiQuery, RangeSpec, answer, protocol_label, query, time_series};
    use crate::analytics::classify::PROTOCOL_DAV;
    use axum::response::Response;
    use bunko_db::analytics::clients::{ClientDim, MixRow};
    use bunko_db::analytics::traffic::{Granularity, Span};
    use serde_json::{Value, json};
    use std::collections::HashMap;

    /// Origins listed.
    const ORIGINS_LIMIT: usize = 50;

    /// The range as UTC days (`clients_daily` has one row per day whatever the
    /// range's granularity).
    pub(crate) fn daily(range: &RangeSpec) -> RangeSpec {
        let (d0, d1) = range.days();
        RangeSpec {
            span: Span {
                granularity: Granularity::Day,
                from: d0,
                to: d1,
            },
            from: d0 * 86400,
            ..*range
        }
    }

    /// `{key, label, requests, bytes_out, principals}` rows of one dimension, most
    /// requests first.
    fn rows_by(
        mix: &[MixRow],
        dim: &str,
        key_of: impl Fn(&MixRow) -> &str,
        principals: &HashMap<String, u64>,
    ) -> Vec<Value> {
        let mut sums: HashMap<&str, (u64, u64)> = HashMap::new();
        for r in mix {
            let s = sums.entry(key_of(r)).or_default();
            s.0 += r.requests;
            s.1 += r.bytes_out;
        }
        let mut rows: Vec<(&str, (u64, u64))> = sums.into_iter().collect();
        rows.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
        rows.into_iter()
            .map(|(k, (requests, bytes_out))| {
                let label = if dim == "protocol" {
                    protocol_label(k)
                } else {
                    label(dim, k)
                };
                json!({
                    "key": k,
                    "label": label,
                    "requests": requests,
                    "bytes_out": bytes_out,
                    "principals": principals.get(k).copied().unwrap_or(0),
                })
            })
            .collect()
    }

    /// `GET /_admin/api/analytics/clients` (design §10).
    pub async fn clients(a: &Analytics, q: &ApiQuery) -> Response {
        let db = a.db().clone();
        let range = daily(&q.range);
        let ip_days = a.config().retention.ip_days;
        let reader_url = a.core().config.read().catalog.reader_url.clone();
        let body = query("analytics clients", move || {
            let (d0, d1) = range.days();
            let mix = db.client_mix(d0, d1)?;
            let p = |dim, protocol| -> bunko_db::Result<HashMap<String, u64>> {
                Ok(db
                    .client_principal_counts(d0, d1, dim, protocol)?
                    .into_iter()
                    .collect())
            };
            let dav: Vec<MixRow> = mix
                .iter()
                .filter(|r| r.protocol == PROTOCOL_DAV)
                .cloned()
                .collect();
            let apps = rows_by(&mix, "app", |r| &r.app, &p(ClientDim::App, None)?);
            let clients = rows_by(&mix, "client", |r| &r.client, &p(ClientDim::Client, None)?);
            let browsers = rows_by(
                &mix,
                "browser",
                |r| &r.browser,
                &p(ClientDim::Browser, None)?,
            );
            let oses = rows_by(&mix, "os", |r| &r.os, &p(ClientDim::Os, None)?);
            let protocols = rows_by(
                &mix,
                "protocol",
                |r| &r.protocol,
                &p(ClientDim::Protocol, None)?,
            );
            let dav_clients = rows_by(
                &dav,
                "client",
                |r| &r.client,
                &p(ClientDim::Client, Some(PROTOCOL_DAV))?,
            );
            let dav_apps = rows_by(
                &dav,
                "app",
                |r| &r.app,
                &p(ClientDim::App, Some(PROTOCOL_DAV))?,
            );
            let origins: Vec<Value> = db
                .top_origins(d0, d1, ORIGINS_LIMIT)?
                .into_iter()
                .map(|(origin, requests)| {
                    let hosted = super::is_hosted_reader(&origin, &reader_url);
                    json!({"origin": origin, "requests": requests, "hosted_reader": hosted})
                })
                .collect();
            // Every app with traffic in range, in the fixed order, so its colour and
            // legend place never move between ranges.
            let order: Vec<(String, String)> = app::ALL
                .iter()
                .filter(|k| mix.iter().any(|r| r.app == **k))
                .map(|k| (k.to_string(), label("app", k)))
                .collect();
            let by_app = time_series(&range, &order, db.app_requests_per_day(d0, d1)?);
            let (requests, bytes_out) = mix
                .iter()
                .fold((0u64, 0u64), |(n, b), r| (n + r.requests, b + r.bytes_out));
            Ok(json!({
                "apps": apps,
                "clients": clients,
                "browsers": browsers,
                "os": oses,
                "protocols": protocols,
                "dav_clients": dav_clients,
                "dav_apps": dav_apps,
                "origins": origins,
                "by_app": by_app,
                "totals": {"requests": requests, "bytes_out": bytes_out},
                "principals_days": ip_days,
                "series_granularity": "day",
            }))
        })
        .await;
        match body {
            Ok(body) => answer(a, q, body),
            Err(r) => r,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(ua: &str) -> (&'static str, &'static str, &'static str) {
        let a = parse_agent(Some(ua));
        (a.client, a.browser, a.os)
    }

    // User agents as the clients' own documentation and source send them.
    #[test]
    fn webdav_clients() {
        let cases = [
            ("rclone/v1.66.0", (client::RCLONE, "-", "-")),
            ("davfs2/1.7.0 neon/0.33.0", (client::DAVFS2, "-", os::LINUX)),
            (
                "Microsoft-WebDAV-MiniRedir/10.0.19045",
                (client::WINDOWS_WEBDAV, "-", os::WINDOWS),
            ),
            (
                "WebDAVFS/3.0.0 (03008000) Darwin/22.1.0 (x86_64)",
                (client::MACOS_FINDER, "-", os::MACOS),
            ),
            ("WebDAVLib/1.3", (client::MACOS_FINDER, "-", os::MACOS)),
            (
                "Cyberduck/8.7.1.40000 (Mac OS X/13.4) (x86_64)",
                (client::CYBERDUCK, "-", os::MACOS),
            ),
            (
                "Cyberduck/8.7.1.40000 (Windows 10/10.0.19045) (amd64)",
                (client::CYBERDUCK, "-", os::WINDOWS),
            ),
            (
                "Mountain Duck/4.15.3.21456 (Mac OS X/14.1.1) (arm64)",
                (client::CYBERDUCK, "-", os::MACOS),
            ),
            ("gvfs/1.50.2", (client::GVFS, "-", os::LINUX)),
            (
                "Kodi/20.2 (X11; Linux x86_64) App_Bitness/64 Version/20.2-(20.2.0)-Git:20230629-5f418d0b13",
                (client::KODI, "-", os::LINUX),
            ),
            (
                "Mozilla/5.0 (Windows) mirall/3.10.1 (build 20231214) (Nextcloud, windows-10.0.19045 ClientArchitecture: x86_64 OsArchitecture: x86_64)",
                (client::NEXTCLOUD, "-", os::WINDOWS),
            ),
            (
                "Mozilla/5.0 (Android) Nextcloud-android/3.27.0",
                (client::NEXTCLOUD, "-", os::ANDROID),
            ),
            (
                "Mozilla/5.0 (iOS) Nextcloud-iOS/4.9.1",
                (client::NEXTCLOUD, "-", os::IOS),
            ),
            (
                "WinSCP/6.1.2 neon/0.32.5",
                (client::WINSCP, "-", os::WINDOWS),
            ),
        ];
        for (ua, want) in cases {
            assert_eq!(agent(ua), want, "{ua}");
        }
    }

    #[test]
    fn tools_and_libraries() {
        let cases = [
            ("curl/8.5.0", client::CURL),
            ("Wget/1.21.4", client::WGET),
            ("python-requests/2.31.0", client::PYTHON),
            ("Python-urllib/3.11", client::PYTHON),
            ("python-httpx/0.27.0", client::PYTHON),
            ("Python/3.11 aiohttp/3.9.1", client::PYTHON),
            ("Go-http-client/1.1", client::GO),
            ("Go-http-client/2.0", client::GO),
            ("node", client::NODE),
            (
                "node-fetch/1.0 (+https://github.com/bitinn/node-fetch)",
                client::NODE,
            ),
            ("axios/1.6.7", client::NODE),
            ("undici", client::NODE),
            ("okhttp/4.12.0", client::OTHER),
            ("mokuro-bunko-processor/0.8.0", client::OTHER),
        ];
        for (ua, want) in cases {
            assert_eq!(parse_agent(Some(ua)).client, want, "{ua}");
        }
        assert_eq!(agent(""), (client::NONE, "-", "-"));
        assert_eq!(parse_agent(None).client, client::NONE);
    }

    #[test]
    fn bots() {
        for ua in [
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "Mozilla/5.0 (Linux; Android 6.0.1; Nexus 5X Build/MMB29P) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.6099.216 Mobile Safari/537.36 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "Mozilla/5.0 (compatible; bingbot/2.0; +http://www.bing.com/bingbot.htm)",
            "Mozilla/5.0 (compatible; YandexBot/3.0; +http://yandex.com/bots)",
            "DuckDuckBot/1.1; (+http://duckduckgo.com/duckduckbot.html)",
            "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; GPTBot/1.2; +https://openai.com/gptbot)",
            "Mozilla/5.0 (compatible; AhrefsBot/7.0; +http://ahrefs.com/robot/)",
            "Mozilla/5.0 (Linux; Android 5.0) AppleWebKit/537.36 (KHTML, like Gecko) Mobile Safari/537.36 (compatible; Bytespider; spider-feedback@bytedance.com)",
            "facebookexternalhit/1.1 (+http://www.facebook.com/externalhit_uatext.php)",
            "Mozilla/5.0 (compatible; Yahoo! Slurp; http://help.yahoo.com/help/us/ysearch/slurp)",
        ] {
            assert_eq!(parse_agent(Some(ua)).client, client::BOT, "{ua}");
        }
        // A phone whose model name ends in "bot" is still a browser.
        assert_eq!(
            agent(
                "Mozilla/5.0 (Linux; Android 10; CUBOT X30) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/119.0.0.0 Mobile Safari/537.36"
            ),
            (client::BROWSER, browser::CHROME, os::ANDROID)
        );
    }

    #[test]
    fn browsers() {
        let cases = [
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                (browser::CHROME, os::WINDOWS),
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Edg/120.0.0.0",
                (browser::EDGE, os::WINDOWS),
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0",
                (browser::FIREFOX, os::LINUX),
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:121.0) Gecko/20100101 Firefox/121.0",
                (browser::FIREFOX, os::MACOS),
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Safari/605.1.15",
                (browser::SAFARI, os::MACOS),
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1",
                (browser::SAFARI, os::IOS),
            ),
            (
                "Mozilla/5.0 (iPad; CPU OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/120.0.6099.119 Mobile/15E148 Safari/604.1",
                (browser::CHROME, os::IOS),
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/121.0 Mobile/15E148 Safari/605.1.15",
                (browser::FIREFOX, os::IOS),
            ),
            (
                "Mozilla/5.0 (Linux; Android 14; SM-S918B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/23.0 Chrome/115.0.0.0 Mobile Safari/537.36",
                (browser::SAMSUNG, os::ANDROID),
            ),
            (
                "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36",
                (browser::CHROME, os::ANDROID),
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 OPR/106.0.0.0",
                (browser::OPERA, os::WINDOWS),
            ),
            (
                "Mozilla/5.0 (X11; CrOS x86_64 14541.0.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
                (browser::CHROME, os::CHROMEOS),
            ),
            (
                "Mozilla/5.0 (X11; FreeBSD amd64; rv:121.0) Gecko/20100101 Firefox/121.0",
                (browser::FIREFOX, os::OTHER),
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/120.0.0.0 Safari/537.36",
                (browser::CHROME, os::LINUX),
            ),
        ];
        for (ua, (b, o)) in cases {
            assert_eq!(agent(ua), (client::BROWSER, b, o), "{ua}");
        }
        // KDE's KIO says Mozilla but names no engine: not a browser.
        assert_eq!(
            agent("Mozilla/5.0 (X11; Linux x86_64) KIO/5.115 dolphin/23.08.4"),
            (client::OTHER, "-", os::LINUX)
        );
    }

    #[test]
    fn origins() {
        assert!(!cross_origin(
            "http://localhost:8080",
            Some("localhost:8080")
        ));
        assert!(!cross_origin("https://Example.com", Some("example.com")));
        assert!(!cross_origin(
            "https://example.com",
            Some("example.com:443")
        ));
        assert!(!cross_origin("http://example.com:80", Some("example.com")));
        // A proxy's `Host $host` drops the port: any port of that name is this server,
        // as the CSRF rule has it.
        assert!(!cross_origin(
            "https://example.com:8443",
            Some("example.com")
        ));
        assert!(cross_origin(
            "https://example.com:8443",
            Some("example.com:8080")
        ));
        assert!(!cross_origin(
            "http://[2001:db8::1]:8080",
            Some("[2001:db8::1]")
        ));
        assert!(cross_origin("http://example.com", Some("example.net")));
        assert!(cross_origin("null", Some("example.com")));
        assert!(cross_origin("https://example.com", None));
        assert_eq!(
            origin_of("https://reader.example.com/path?x"),
            Some("https://reader.example.com")
        );
        assert_eq!(
            origin_of("http://reader.test:5173"),
            Some("http://reader.test:5173")
        );
        assert_eq!(origin_of("reader.test"), None);
        assert!(is_hosted_reader(
            "https://reader.example.com",
            "https://reader.example.com/"
        ));
        assert!(!is_hosted_reader(
            "https://other.example.com",
            "https://reader.example.com/"
        ));
        // The default CORS list's hosted origin counts, its localhost wildcards do not.
        let d = &default_hosted_origins()[0];
        assert!(is_hosted_reader(d, "https://reader.example.com"));
        assert!(!is_hosted_reader(
            "http://localhost:5173",
            "https://reader.example.com"
        ));
    }

    fn app(
        ua: Option<&str>,
        origin: Option<&str>,
        protocol: &'static str,
        path: &str,
    ) -> &'static str {
        let reader_url = "https://reader.example.com/";
        let cors = CorsConfig::default();
        let r = ClientRequest {
            user_agent: ua,
            origin,
            host: Some("bunko.example.net"),
            protocol,
            path,
        };
        let agent = parse_agent(ua);
        match origin {
            Some(o) if cross_origin(o, r.host) => {
                cross_origin_app(o, &r, &agent, reader_url, &cors)
            }
            _ => local_app(&r, &agent),
        }
    }

    #[test]
    fn apps() {
        const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        const FIREFOX: &str =
            "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";
        let hosted = Some("https://reader.example.com");
        let dev = Some("http://localhost:5173");
        let own = Some("https://bunko.example.net");
        let elsewhere = Some("https://app.example.com");
        // The configured reader, on WebDAV and its JSON calls.
        assert_eq!(
            app(Some(CHROME), hosted, "dav", "/mokuro-reader/"),
            app::READER_HOSTED
        );
        assert_eq!(
            app(Some(CHROME), hosted, "api", "/login/api/me"),
            app::READER_HOSTED
        );
        // The hosted origin of the default CORS list, whatever reader_url says.
        let default = default_hosted_origins()[0].clone();
        assert_eq!(
            app(Some(CHROME), Some(&default), "dav", "/mokuro-reader/x.cbz"),
            app::READER_HOSTED
        );
        // A reader build on another origin.
        assert_eq!(
            app(Some(FIREFOX), dev, "dav", "/mokuro-reader/positions.json"),
            app::READER_OTHER
        );
        assert_eq!(
            app(Some(FIREFOX), dev, "api", "/login/api/token"),
            app::READER_OTHER
        );
        // Bunko's own pages: no Origin (GETs) or its own (writes).
        assert_eq!(app(Some(FIREFOX), None, "web", "/catalog"), app::BUNKO_WEB);
        assert_eq!(
            app(Some(FIREFOX), own, "api", "/_admin/api/settings"),
            app::BUNKO_WEB
        );
        assert_eq!(
            app(Some(FIREFOX), None, "dav", "/mokuro-reader/s/v.webp"),
            app::BUNKO_WEB
        );
        // Another allowed web app calling the JSON API; a disallowed origin on Bunko's
        // pages is its own page behind a proxy that rewrote Host.
        assert_eq!(
            app(Some(FIREFOX), dev, "api", "/catalog/api/library"),
            app::API_CLIENT
        );
        assert_eq!(
            app(Some(FIREFOX), elsewhere, "api", "/_admin/api/status"),
            app::BUNKO_WEB
        );
        // Bunko's own sign-in and account calls (nav.js, setup.js, account.js): from its
        // own origin on another port behind a port-dropping proxy, and from an origin a
        // Host-rewriting proxy hid, they are Bunko's pages, not a reader.
        let own_port = Some("https://bunko.example.net:8443");
        for path in ["/login/api/token", "/login/api/me"] {
            assert_eq!(app(Some(FIREFOX), own_port, "api", path), app::BUNKO_WEB);
            assert_eq!(app(Some(FIREFOX), elsewhere, "api", path), app::BUNKO_WEB);
        }
        // WebDAV clients, named or not, and with no user agent.
        for ua in [
            Some("rclone/v1.66.0"),
            Some("davfs2/1.7.0 neon/0.33.0"),
            Some("Microsoft-WebDAV-MiniRedir/10.0.19045"),
            Some("curl/8.5.0"),
            Some("SomeSyncTool/2.0"),
            None,
        ] {
            assert_eq!(
                app(ua, None, "dav", "/mokuro-reader/"),
                app::WEBDAV_CLIENT,
                "{ua:?}"
            );
        }
        // Programs on the JSON API and pages; bots and silent agents are unknown.
        assert_eq!(
            app(
                Some("python-requests/2.31.0"),
                None,
                "api",
                "/catalog/api/library"
            ),
            app::API_CLIENT
        );
        assert_eq!(
            app(
                Some("mokuro-bunko-processor/0.8.0"),
                None,
                "processor",
                "/_processor/ws"
            ),
            app::API_CLIENT
        );
        assert_eq!(app(None, None, "web", "/"), app::UNKNOWN);
        assert_eq!(
            app(
                Some("Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)"),
                None,
                "dav",
                "/mokuro-reader/s/v.cbz"
            ),
            app::UNKNOWN
        );
    }

    /// `cargo test -p bunko-server --lib clients::tests::classify_cost -- --ignored --nocapture`
    #[test]
    #[ignore = "a measurement, run on demand"]
    fn classify_cost() {
        let uas = [
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.2 Mobile/15E148 Safari/604.1",
            "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0",
            "rclone/v1.66.0",
            "Microsoft-WebDAV-MiniRedir/10.0.19045",
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
        ];
        let n = 200_000;
        let t = std::time::Instant::now();
        for i in 0..n {
            std::hint::black_box(parse_agent(Some(std::hint::black_box(uas[i % uas.len()]))));
        }
        let parse = t.elapsed().as_nanos() as f64 / n as f64;
        let mut cache = AgentCache::default();
        for ua in uas {
            cache.put(ua, parse_agent(Some(ua)));
        }
        let lock = Mutex::new(cache);
        let t = std::time::Instant::now();
        for i in 0..n {
            std::hint::black_box(lock.lock().get(std::hint::black_box(uas[i % uas.len()])));
        }
        let cached = t.elapsed().as_nanos() as f64 / n as f64;
        println!("parse_agent {parse:.0} ns/call (uncached), cache hit {cached:.0} ns/call");
    }

    #[test]
    fn cache_generations_stay_bounded() {
        let mut c = AgentCache::default();
        let a = parse_agent(Some("curl/8.5.0"));
        for i in 0..(AGENT_CACHE * 5) {
            c.put(&format!("agent-{i}"), a);
        }
        assert!(c.current.len() <= AGENT_CACHE && c.previous.len() <= AGENT_CACHE);
        // A recent one is found, an old one is gone.
        assert_eq!(c.get(&format!("agent-{}", AGENT_CACHE * 5 - 1)), Some(a));
        assert_eq!(c.get("agent-0"), None);
    }
}
