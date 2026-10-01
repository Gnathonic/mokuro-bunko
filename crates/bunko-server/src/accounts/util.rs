//! Helpers shared by the account modules: JSON answers, bounded JSON bodies, the
//! client address, blocking DB calls and the per-module static-file rules.

use super::AccountsDeps;
use crate::core::RequestCtx;
use crate::http::client_ip::canonical;
use crate::http::static_files;
use axum::body::Body;
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::response::{IntoResponse, Response};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use tracing::error;

/// 0.5.2 `MAX_JSON_BODY_BYTES`.
pub const MAX_JSON_BODY_BYTES: usize = 64 * 1024;

/// A JSON answer (`Content-Type: application/json`, as 0.5.2 sent it).
pub fn json_response(status: u16, value: Value) -> Response {
    let body = serde_json::to_vec(&value).unwrap_or_default();
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp
}

pub fn json_error(status: u16, message: &str) -> Response {
    json_response(status, json!({ "error": message }))
}

pub fn internal_error() -> Response {
    json_error(500, "Internal server error")
}

/// `204 No Content` with an `Allow` header (0.5.2 `_handle_options`).
pub fn options_response(allow: &'static str) -> Response {
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().insert(header::ALLOW, HeaderValue::from_static(allow));
    resp
}

/// Run blocking work (SQLite, bcrypt, fs) off the async workers. A panic in `f`
/// becomes a 500.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, Response> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        error!("account task failed: {e}");
        internal_error()
    })
}

/// A database call's failure, logged; the caller answers 500.
pub fn db_failed(what: &str, e: &bunko_db::DbError) -> Response {
    error!("{what}: {e}");
    internal_error()
}

/// The request body as 0.5.2 read it: by declared `Content-Length`, capped at 64 KiB.
pub enum JsonBody {
    /// No body (0.5.2: `Content-Length` 0 or missing).
    Empty,
    /// Over `MAX_JSON_BODY_BYTES` (413).
    TooLarge,
    Bytes(bytes::Bytes),
}

pub async fn read_body(headers: &HeaderMap, body: Body) -> JsonBody {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_JSON_BODY_BYTES as u64) {
        return JsonBody::TooLarge;
    }
    if declared == Some(0) {
        return JsonBody::Empty;
    }
    match axum::body::to_bytes(body, MAX_JSON_BODY_BYTES).await {
        Ok(b) if b.is_empty() => JsonBody::Empty,
        Ok(b) => JsonBody::Bytes(b),
        Err(_) => JsonBody::TooLarge,
    }
}

/// Parse a JSON object (Python `json.loads(body.decode("utf-8"))` followed by dict
/// access). `None` for invalid UTF-8/JSON and for non-object JSON, which 0.5.2 turned
/// into a 500 and the port answers as a 400.
pub fn parse_object(bytes: &[u8]) -> Option<serde_json::Map<String, Value>> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// A string field, or `None` when absent, null or not a string.
pub fn str_field<'a>(m: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    m.get(key).and_then(Value::as_str)
}

/// The TCP peer and the client address behind trusted proxies (0.5.2 `REMOTE_ADDR` and
/// `get_client_ip`). Unlike [`RequestCtx`] it never authenticates.
#[derive(Debug, Clone)]
pub struct Client {
    pub peer: IpAddr,
    pub ip: String,
    /// The connection's address was known (`ConnectInfo`); without it `peer` is a
    /// loopback placeholder, which the setup gate must not trust.
    pub peer_known: bool,
}

impl Client {
    pub fn of(deps: &AccountsDeps, parts: &Parts) -> Client {
        let peer_known = parts.extensions.get::<ConnectInfo<SocketAddr>>().is_some();
        let peer = canonical(RequestCtx::peer_of(parts));
        let ip = deps.core.proxies.read().client_ip_text(peer, &parts.headers);
        Client { peer, ip, peer_known }
    }

    /// The `ip:username` limiter key every 0.5.2 login surface uses.
    pub fn limiter_key(&self, username: &str) -> String {
        format!("{}:{username}", self.ip)
    }
}

impl FromRequestParts<AccountsDeps> for Client {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, deps: &AccountsDeps) -> Result<Self, Self::Rejection> {
        Ok(Client::of(deps, parts))
    }
}

/// `..` segments, absolute paths and backslashes never name an embedded asset.
pub fn is_traversal(file: &str) -> bool {
    file.starts_with('/') || file.contains('\\') || file.split('/').any(|s| s == "..")
}

/// Login/account static rules: `index.html` for the bare prefix, 403 text `Forbidden`
/// for traversal, 404 text `Not found`, `Cache-Control: no-cache`.
pub fn serve_page_text_errors(module: &str, file: &str) -> Response {
    let file = if file.is_empty() || file == "/" { "index.html" } else { file };
    if is_traversal(file) {
        return text(403, "Forbidden");
    }
    static_files::serve(module, file, Some("no-cache")).unwrap_or_else(|| text(404, "Not found"))
}

/// Home/setup/registration static rules: JSON 404 `{"error": <not_found>}` for
/// traversal and missing files.
pub fn serve_page_json_errors(module: &str, file: &str, not_found: &str, cache: Option<&'static str>) -> Response {
    if file.is_empty() || is_traversal(file) {
        return json_error(404, not_found);
    }
    static_files::serve(module, file, cache).unwrap_or_else(|| json_error(404, not_found))
}

/// A plain-text answer (0.5.2 `_error_response` of the page modules).
pub fn text(status: u16, message: &'static str) -> Response {
    let mut resp = Response::new(Body::from(message));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    resp
}

/// `Too many failed attempts. Retry in Ns`.
pub fn limited_message(retry: u64) -> String {
    format!("Too many failed attempts. Retry in {retry}s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal() {
        assert!(is_traversal("../x"));
        assert!(is_traversal("a/../x"));
        assert!(is_traversal("/etc/passwd"));
        assert!(is_traversal("a\\b"));
        assert!(!is_traversal("styles.css"));
        assert!(!is_traversal("a..b.css"));
    }
}
