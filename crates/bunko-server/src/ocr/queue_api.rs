//! The queue page (0.5.2 `queue/api.py`): `/queue` (the static page), `/queue/api/config`,
//! `/queue/api/status` (shaped per display level and viewer, ETag/304, `X-Queue-Auth`).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::get;
use http::{HeaderMap, HeaderValue, StatusCode, header};
use parking_lot::Mutex;
use serde_json::json;
use sha2::Digest;

use super::OcrControl;
use super::shape::{normalize_level, shape_status};
use crate::core::RequestCtx;

pub const AUTH_CACHE_SECONDS: u64 = 30;
pub const AUTH_FAIL_CACHE_SECONDS: u64 = 60;
pub const AUTH_CACHE_SIZE: usize = 256;
/// Bodies are rebuilt at most this often per (level, viewer kind).
pub const REBUILD_SECONDS: f64 = 1.0;

/// Who polls the status.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Viewer {
    pub role: Option<String>,
    pub failed: bool,
    pub limited: bool,
}

/// Basic-auth results by keyed header digest: `(expires, users_version, role)`.
type AuthCache = HashMap<[u8; 32], (Instant, u64, Option<String>)>;

struct Built {
    version: u64,
    at: Instant,
    etag: String,
    body: bytes::Bytes,
}

/// Per-process caches of the status endpoint.
pub struct StatusCache {
    key: [u8; 32],
    auth: Mutex<AuthCache>,
    built: Mutex<HashMap<(String, bool), Built>>,
    building: tokio::sync::Mutex<()>,
}

impl Default for StatusCache {
    fn default() -> Self {
        let mut key = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rng(), &mut key);
        StatusCache { key, auth: Mutex::new(HashMap::new()), built: Mutex::new(HashMap::new()), building: tokio::sync::Mutex::new(()) }
    }
}

pub fn router(ocr: OcrControl) -> Router {
    Router::new()
        .route("/queue", get(index))
        .route("/queue/", get(index))
        .route("/queue/api/config", get(config))
        .route("/queue/api/status", get(status))
        .route("/queue/{*file}", get(static_file))
        .with_state(ocr)
}

fn json_body(status: u16, body: &serde_json::Value) -> Response {
    let bytes = crate::ocr::pyjson::dumps(body, crate::ocr::pyjson::DEFAULT);
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

fn not_found() -> Response {
    let mut resp = Response::new(Body::from("Not found"));
    *resp.status_mut() = StatusCode::NOT_FOUND;
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    resp
}

async fn index() -> Response {
    crate::http::static_files::serve("queue", "index.html", Some("no-cache")).unwrap_or_else(not_found)
}

async fn static_file(Path(file): Path<String>) -> Response {
    if file.is_empty() || file.contains("..") {
        return not_found();
    }
    match crate::http::static_files::asset("queue", &file) {
        Some(f) => {
            let lower = file.to_lowercase();
            let ct = if lower.ends_with(".html") {
                "text/html; charset=utf-8"
            } else if lower.ends_with(".js") {
                "application/javascript; charset=utf-8"
            } else if lower.ends_with(".css") {
                "text/css; charset=utf-8"
            } else {
                "application/octet-stream"
            };
            let mut resp = Response::new(Body::from(f.data.into_owned()));
            resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
            resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            resp
        }
        None => not_found(),
    }
}

async fn config(State(ocr): State<OcrControl>) -> Response {
    let q = ocr.core().config.read().queue.clone();
    json_body(200, &json!({"show_in_nav": q.show_in_nav, "public_access": q.public_access, "display": normalize_level(&q.display)}))
}

/// `_viewer`: Bearer every time; Basic through a short cache keyed by a keyed hash of
/// the header (dropped when any account changes), checked against the DAV limiter.
async fn viewer(ocr: &OcrControl, headers: &HeaderMap, client_ip: &str) -> Viewer {
    let Some(db) = ocr.db().cloned() else { return Viewer::default() };
    let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_string) else {
        return Viewer::default();
    };
    let core = ocr.core().clone();
    if let Some(token) = auth.strip_prefix("Bearer ") {
        let token = token.trim().to_string();
        let backend = core.backend.clone();
        let user = tokio::task::spawn_blocking(move || backend.resolve_token(&token)).await.ok().flatten();
        return match user {
            Some(u) => Viewer { role: Some(u.role.as_str().to_string()), ..Viewer::default() },
            None => Viewer { failed: true, ..Viewer::default() },
        };
    }
    let cache = ocr.status_cache();
    let mut h = sha2::Sha256::new();
    h.update(cache.key);
    h.update(auth.as_bytes());
    let digest: [u8; 32] = h.finalize().into();
    let users_version = db.users_version();
    let now = Instant::now();
    if let Some((exp, ver, role)) = cache.auth.lock().get(&digest).cloned()
        && exp > now
        && ver == users_version
    {
        return Viewer { failed: role.is_none(), role, limited: false };
    }
    let remember = |role: Option<String>| {
        let ttl = Duration::from_secs(if role.is_some() { AUTH_CACHE_SECONDS } else { AUTH_FAIL_CACHE_SECONDS });
        let mut map = cache.auth.lock();
        if map.len() >= AUTH_CACHE_SIZE {
            map.retain(|_, v| v.0 > now);
            if map.len() >= AUTH_CACHE_SIZE {
                map.clear();
            }
        }
        map.insert(digest, (now + ttl, users_version, role));
    };
    let creds = match crate::auth::parse_basic(&auth) {
        Ok(Some(c)) => c,
        _ => {
            remember(None);
            return Viewer { failed: true, ..Viewer::default() };
        }
    };
    let key = format!("{client_ip}:{}", creds.0);
    if core.dav_limiter.allow(&key).is_err() {
        return Viewer { limited: true, ..Viewer::default() };
    }
    let backend = core.backend.clone();
    let user = tokio::task::spawn_blocking(move || backend.check_password(&creds.0, &creds.1)).await.ok().flatten();
    match user {
        Some(u) => {
            core.dav_limiter.record_success(&key);
            let role = u.role.as_str().to_string();
            remember(Some(role.clone()));
            Viewer { role: Some(role), ..Viewer::default() }
        }
        None => {
            core.dav_limiter.record_failure(&key);
            remember(None);
            Viewer { failed: true, ..Viewer::default() }
        }
    }
}

/// `GET /queue/api/status`.
async fn status(State(ocr): State<OcrControl>, ctx: RequestCtx, headers: HeaderMap) -> Response {
    let who = viewer(&ocr, &headers, &ctx.client_ip).await;
    let (public, level) = {
        let c = ocr.core().config.read();
        (c.queue.public_access, normalize_level(&c.queue.display).to_string())
    };
    if !public && who.role.is_none() {
        return json_body(401, &json!({"error": "Authentication required"}));
    }
    let admin = who.role.as_deref() == Some("admin");
    let Some((etag, body)) = build(&ocr, &level, admin).await else {
        return json_body(503, &json!({"error": "OCR is not running"}));
    };
    let mut extra: Vec<(&'static str, HeaderValue)> = vec![
        ("etag", HeaderValue::from_str(&etag).unwrap_or(HeaderValue::from_static("\"\""))),
        ("cache-control", HeaderValue::from_static("private, no-cache")),
        ("vary", HeaderValue::from_static("Authorization")),
    ];
    if who.failed {
        extra.push(("x-queue-auth", HeaderValue::from_static("failed")));
    } else if who.limited {
        extra.push(("x-queue-auth", HeaderValue::from_static("limited")));
    }
    let inm = headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).unwrap_or("");
    let not_modified = inm.split(',').any(|t| t.trim() == etag);
    let mut resp = if not_modified {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::NOT_MODIFIED;
        r
    } else {
        let mut r = Response::new(Body::from(body.clone()));
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
        r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(body.len()));
        r
    };
    for (k, v) in extra {
        resp.headers_mut().insert(k, v);
    }
    resp
}

/// The current body and ETag for (level, admin): rebuilt when the scheduler's page
/// version moved, at most once a second; single-flight.
pub async fn build(ocr: &OcrControl, level: &str, admin: bool) -> Option<(String, bytes::Bytes)> {
    let cache = ocr.status_cache();
    let slot = (level.to_string(), admin);
    let version = ocr.ask(|s| s.page_version()).await?;
    let fresh = |c: &HashMap<(String, bool), Built>| {
        c.get(&slot).filter(|b| b.version == version || b.at.elapsed().as_secs_f64() < REBUILD_SECONDS).map(|b| (b.etag.clone(), b.body.clone()))
    };
    if let Some(hit) = fresh(&cache.built.lock()) {
        return Some(hit);
    }
    let _guard = cache.building.lock().await;
    if let Some(hit) = fresh(&cache.built.lock()) {
        return Some(hit);
    }
    let lvl = level.to_string();
    let (version, payload) = ocr
        .ask(move |s| {
            let raw = s.raw_status();
            let version = s.page_version();
            let shaped = shape_status(&raw, &lvl, admin, &mut s.public_names);
            (version, shaped)
        })
        .await?;
    let text = crate::ocr::pyjson::dumps(&payload, crate::ocr::pyjson::SORTED_DEFAULT);
    let digest = hex::encode(sha2::Sha256::digest(text.as_bytes()));
    let etag = format!("\"{level}-{}-{}\"", if admin { "a" } else { "v" }, &digest[..20]);
    let body = bytes::Bytes::from(text);
    cache.built.lock().insert(slot, Built { version, at: Instant::now(), etag: etag.clone(), body: body.clone() });
    Some((etag, body))
}
