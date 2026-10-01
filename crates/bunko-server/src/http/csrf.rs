//! Cross-site request forgery guards for the JSON APIs that change state.
//!
//! A browser sends credentials it holds (cookies, a cached HTTP Basic login) with a
//! cross-site form post or a "simple" `fetch` without asking the page's origin. Two rules
//! close that door without touching legitimate clients (the web UIs always send
//! `Content-Type: application/json` from the server's own origin):
//!
//! - a request with a body must say `Content-Type: application/json` (415 otherwise):
//!   forms and simple requests can only send `text/plain`, `multipart/form-data` or
//!   `application/x-www-form-urlencoded`, and a JSON content type forces a CORS
//!   preflight the server only grants to `cors.allowed_origins`;
//! - a request whose `Origin` is neither this server nor an allowed CORS origin is
//!   refused (403), covering body-less `DELETE`/`POST` too. No `Origin` (curl, the CLI,
//!   same-origin `GET`s in older browsers) passes.

use bunko_core::config::CorsConfig;
use http::{HeaderMap, Method, header};

/// Why a request was refused: `(status, message)`.
pub type Refusal = (u16, &'static str);

pub const NOT_JSON: &str = "Content-Type must be application/json";
pub const CROSS_ORIGIN: &str = "Cross-origin request refused";

/// Does the request carry a body (a non-zero `Content-Length`, or chunked)?
pub fn has_body(headers: &HeaderMap) -> bool {
    if headers.contains_key(header::TRANSFER_ENCODING) {
        return true;
    }
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .is_some_and(|n| n > 0)
}

/// Is the declared media type `application/json` (parameters such as `charset` allowed)?
pub fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/json"))
}

/// The JSON rule alone: `Some(415)` for a body that is not declared JSON.
pub fn require_json(headers: &HeaderMap) -> Option<Refusal> {
    (has_body(headers) && !is_json(headers)).then_some((415, NOT_JSON))
}

/// `host[:port]` of an `Origin` value, lowercased, with the scheme's default port
/// dropped; `None` for `null` or anything unparseable.
fn origin_authority(origin: &str) -> Option<String> {
    let (scheme, rest) = origin.split_once("://")?;
    let authority = rest.split('/').next()?.to_ascii_lowercase();
    if authority.is_empty() {
        return None;
    }
    let default = match scheme.to_ascii_lowercase().as_str() {
        "http" => ":80",
        "https" => ":443",
        _ => "",
    };
    Some(match authority.strip_suffix(default) {
        Some(bare) if !default.is_empty() => bare.to_string(),
        _ => authority,
    })
}

/// `Host` (or the first `X-Forwarded-Host`) of the request, lowercased, default ports
/// dropped.
fn request_hosts(headers: &HeaderMap) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |v: &str| {
        let v = v.trim().to_ascii_lowercase();
        let v = v
            .strip_suffix(":80")
            .or_else(|| v.strip_suffix(":443"))
            .map(str::to_string)
            .unwrap_or(v);
        if !v.is_empty() {
            out.push(v);
        }
    };
    if let Some(h) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) {
        push(h);
    }
    if let Some(h) = headers
        .get("x-forwarded-host")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
    {
        push(h);
    }
    out
}

/// `host` and `port` of `host[:port]` (`[v6]:port` too).
fn split_port(authority: &str) -> (&str, Option<&str>) {
    match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && (!host.contains(':') || host.ends_with(']')) =>
        {
            (host, Some(port))
        }
        _ => (authority, None),
    }
}

/// Does the `Origin` authority name this request's host? A `Host` without a port (a
/// proxy's `proxy_set_header Host $host` drops it) matches any port of that host name.
fn same_authority(origin: &str, host: &str) -> bool {
    if origin == host {
        return true;
    }
    let (o_host, _) = split_port(origin);
    match split_port(host) {
        (h, None) => h == o_host,
        _ => false,
    }
}

/// The `Origin` rule: `Some(403)` when an `Origin` is present and is neither this
/// server's own (`Host` / `X-Forwarded-Host`) nor allowed by the CORS settings.
pub fn check_origin(headers: &HeaderMap, cors: &CorsConfig) -> Option<Refusal> {
    let origin = headers.get(header::ORIGIN)?;
    let Ok(origin) = origin.to_str() else {
        return Some((403, CROSS_ORIGIN));
    };
    if let Some(authority) = origin_authority(origin)
        && request_hosts(headers)
            .iter()
            .any(|host| same_authority(&authority, host))
    {
        return None;
    }
    if cors.is_origin_allowed(origin) {
        return None;
    }
    Some((403, CROSS_ORIGIN))
}

/// Both rules for a state-changing method (`POST`, `PUT`, `PATCH`, `DELETE`); other
/// methods pass.
pub fn check(method: &Method, headers: &HeaderMap, cors: &CorsConfig) -> Option<Refusal> {
    if !matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return None;
    }
    check_origin(headers, cors).or_else(|| require_json(headers))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        m
    }

    fn cors(origins: &[&str]) -> CorsConfig {
        CorsConfig {
            enabled: true,
            allowed_origins: origins.iter().map(|s| s.to_string()).collect(),
            ..CorsConfig::default()
        }
    }

    #[test]
    fn json_rule() {
        let c = cors(&[]);
        let post = Method::POST;
        assert_eq!(check(&post, &h(&[("content-length", "0")]), &c), None);
        assert_eq!(check(&post, &h(&[]), &c), None);
        assert_eq!(
            check(&post, &h(&[("content-length", "2")]), &c),
            Some((415, NOT_JSON))
        );
        assert_eq!(
            check(
                &post,
                &h(&[("content-length", "2"), ("content-type", "text/plain")]),
                &c
            ),
            Some((415, NOT_JSON))
        );
        assert_eq!(
            check(
                &post,
                &h(&[
                    ("transfer-encoding", "chunked"),
                    ("content-type", "application/x-www-form-urlencoded")
                ]),
                &c
            ),
            Some((415, NOT_JSON))
        );
        assert_eq!(
            check(
                &post,
                &h(&[
                    ("content-length", "2"),
                    ("content-type", "Application/JSON; charset=utf-8")
                ]),
                &c
            ),
            None
        );
        assert_eq!(
            check(&Method::GET, &h(&[("content-length", "2")]), &c),
            None
        );
    }

    #[test]
    fn origin_rule() {
        let c = cors(&["http://localhost:*", "https://reader.example"]);
        let del = Method::DELETE;
        let host = ("host", "library.example:8080");
        assert_eq!(check(&del, &h(&[host]), &c), None);
        assert_eq!(
            check(
                &del,
                &h(&[host, ("origin", "http://library.example:8080")]),
                &c
            ),
            None
        );
        assert_eq!(
            check(&del, &h(&[host, ("origin", "https://evil.example")]), &c),
            Some((403, CROSS_ORIGIN))
        );
        assert_eq!(
            check(&del, &h(&[host, ("origin", "null")]), &c),
            Some((403, CROSS_ORIGIN))
        );
        assert_eq!(
            check(&del, &h(&[host, ("origin", "http://localhost:5173")]), &c),
            None
        );
        assert_eq!(
            check(&del, &h(&[host, ("origin", "https://reader.example")]), &c),
            None
        );
        // Default ports, case, and a reverse proxy's forwarded host.
        assert_eq!(
            check(
                &del,
                &h(&[
                    ("host", "Library.Example"),
                    ("origin", "https://library.example:443")
                ]),
                &c
            ),
            None
        );
        assert_eq!(
            check(
                &del,
                &h(&[
                    ("host", "127.0.0.1:8080"),
                    ("x-forwarded-host", "manga.example"),
                    ("origin", "https://manga.example")
                ]),
                &c
            ),
            None
        );
        // A proxy that forwards `Host` without its port: any port of that host name.
        assert_eq!(
            check(
                &del,
                &h(&[
                    ("host", "manga.example"),
                    ("origin", "https://manga.example:8443")
                ]),
                &c
            ),
            None
        );
        assert_eq!(
            check(
                &del,
                &h(&[
                    ("host", "manga.example:8443"),
                    ("origin", "https://manga.example:9999")
                ]),
                &c
            ),
            Some((403, CROSS_ORIGIN))
        );
        assert_eq!(
            check(
                &del,
                &h(&[("host", "[::1]:8080"), ("origin", "http://[::1]:8080")]),
                &c
            ),
            None
        );
        assert_eq!(
            check(
                &del,
                &h(&[
                    ("host", "manga.example"),
                    ("origin", "https://manga.example.evil")
                ]),
                &c
            ),
            Some((403, CROSS_ORIGIN))
        );
        // CORS disabled: only the server's own origin.
        let off = CorsConfig {
            enabled: false,
            ..c.clone()
        };
        assert_eq!(
            check(&del, &h(&[host, ("origin", "http://localhost:5173")]), &off),
            Some((403, CROSS_ORIGIN))
        );
    }
}
