//! Home page, `/api/stats`, `/api/health` and the `GET /` decision (0.5.2
//! `home/api.py` and the `GET /` branch of `setup/api.py`; spec metadata-catalog §11,
//! web-frontend-contract §2).

use super::AccountsDeps;
use super::util::{blocking, json_error, json_response, options_response, serve_page_json_errors};
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bunko_db::UserStatus;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

/// Library volume counts (the library index implements this).
pub trait LibraryCounts: Send + Sync {
    /// Sum of volumes over every series of the index snapshot. `Err` reports
    /// `library_status: "error"` (health) or 0 (stats).
    fn total_volumes(&self) -> Result<u64, String>;

    /// Volumes without a primary sidecar (`<stem>.mokuro` or `.mokuro.gz`): the index
    /// snapshot's `pending_ocr`, which is what 0.5.3's health `ocr.pending` counts. `Err`
    /// (or a source that does not count them) reports `pending: null`.
    fn pending_ocr(&self) -> Result<u64, String> {
        Err("not counted".into())
    }
}

/// The OCR side of `/api/health` (the OCR scheduler implements this).
pub trait HealthSource: Send + Sync {
    /// The `ocr` object: 0.5.2 `{"backend", "worker_alive", "pending", "failed"}` (plus
    /// 0.7's `queued_jobs`), or `Value::Null`. `pending` is filled in from the library
    /// index by the health handler. May do small blocking reads; it is called off the
    /// async workers.
    fn ocr_health(&self) -> Value;
}

/// 0.5.2's `ocr` block when OCR is off (`backend: skip`).
fn no_ocr() -> Value {
    json!({"backend": "skip", "worker_alive": null, "pending": null, "failed": 0})
}

pub fn routes() -> Router<AccountsDeps> {
    Router::new()
        .route(
            "/api/health",
            get(health).options(preflight).fallback(method_not_allowed),
        )
        .route(
            "/api/stats",
            get(stats).options(preflight).fallback(method_not_allowed),
        )
        .route("/_home/{*file}", get(file))
}

async fn preflight() -> Response {
    options_response("GET, OPTIONS")
}

async fn method_not_allowed() -> Response {
    json_error(405, "Method not allowed")
}

async fn file(Path(file): Path<String>) -> Response {
    serve_page_json_errors("home", &file, "File not found", Some("no-cache"))
}

fn count_users(d: &AccountsDeps) -> bunko_db::Result<u64> {
    Ok(d.db
        .list_users(None)?
        .iter()
        .filter(|u| u.status != UserStatus::Deleted)
        .count() as u64)
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `GET /api/stats`: never fails; a broken source counts as 0.
async fn stats(State(d): State<AccountsDeps>) -> Response {
    let counts = blocking(move || {
        let users = count_users(&d).unwrap_or_else(|e| {
            warn!("stats: user count failed: {e}");
            0
        });
        let volumes = d
            .library
            .as_ref()
            .map(|l| l.total_volumes().unwrap_or(0))
            .unwrap_or(0);
        (users, volumes)
    })
    .await;
    let (users, volumes) = counts.unwrap_or((0, 0));
    json_response(
        200,
        json!({
            "total_users": users,
            "total_volumes": volumes,
            "total_pages_read": 0,
            "total_characters_read": 0,
            "total_reading_time_seconds": 0,
            "total_reading_time_formatted": "0s",
            "last_updated": epoch_seconds(),
        }),
    )
}

/// `GET /api/health`: always 200; `status` is `degraded` when a probe failed.
async fn health(State(d): State<AccountsDeps>) -> Response {
    let uptime = d.core.started_at.elapsed().as_secs();
    let body = blocking(move || {
        let mut healthy = true;
        let (db_status, total_users) = match count_users(&d) {
            Ok(n) => ("ok", json!(n)),
            Err(e) => {
                warn!("health: user count failed: {e}");
                healthy = false;
                ("error", Value::Null)
            }
        };
        let (library_status, total_volumes) = match &d.library {
            None => ("unavailable", Value::Null),
            Some(l) => match l.total_volumes() {
                Ok(n) => ("ok", json!(n)),
                Err(e) => {
                    warn!("health: library count failed: {e}");
                    healthy = false;
                    ("error", Value::Null)
                }
            },
        };
        let mut ocr = d
            .health
            .as_ref()
            .map(|h| h.ocr_health())
            .unwrap_or_else(no_ocr);
        // 0.5.3's `pending`: volumes without a primary sidecar, from the library index
        // (not the scheduler's waiting jobs: those are `queued_jobs`). OCR off: null.
        if let Value::Object(m) = &mut ocr
            && m.contains_key("pending")
            && m.get("backend").and_then(Value::as_str) != Some("skip")
        {
            let pending = d
                .library
                .as_ref()
                .and_then(|l| l.pending_ocr().ok())
                .map_or(Value::Null, Value::from);
            m.insert("pending".into(), pending);
        }
        json!({
            "status": if healthy { "ok" } else { "degraded" },
            "uptime_seconds": uptime,
            "db_status": db_status,
            "library_status": library_status,
            "total_users": total_users,
            "total_volumes": total_volumes,
            "ocr": ocr,
        })
    })
    .await;
    match body {
        Ok(v) => json_response(200, v),
        Err(resp) => resp,
    }
}

/// 0.5.2 `is_browser_request`: `Accept` with `text/html`; else not a known WebDAV
/// client User-Agent; else not a `Depth` header containing the text "Depth" (0.5.2's
/// test, which in practice never matches, is reproduced as is).
pub fn is_browser_request(headers: &HeaderMap) -> bool {
    let text = |name: header::HeaderName| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    if text(header::ACCEPT).contains("text/html") {
        return true;
    }
    let agent = text(header::USER_AGENT).to_lowercase();
    const DAV_CLIENTS: [&str; 9] = [
        "davfs",
        "cadaver",
        "cyberduck",
        "webdav",
        "gvfs",
        "nautilus",
        "finder",
        "microsoft-webdav",
        "litmus",
    ];
    if DAV_CLIENTS.iter().any(|c| agent.contains(c)) {
        return false;
    }
    let depth = headers
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    !depth.contains("Depth")
}

fn redirect(location: &'static str) -> Response {
    let mut resp = StatusCode::FOUND.into_response();
    resp.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_static(location));
    resp
}

/// The answer to `GET /` when it is not WebDAV's, in 0.5.2's order:
/// 1. setup needed and `Accept` has `text/html` → `302 /setup`;
/// 2. a browser and `catalog.enabled && catalog.use_as_homepage` → `302 /catalog/`;
/// 3. a browser → the home page;
///
/// `None` (every other method/path, and non-browser `GET /`) means: hand the request
/// to WebDAV.
pub async fn root_response(
    deps: &AccountsDeps,
    method: &Method,
    path: &str,
    headers: &HeaderMap,
) -> Option<Response> {
    if method != Method::GET || path != "/" {
        return None;
    }
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    if wants_html {
        let (flag, db) = (deps.setup.clone(), deps.db.clone());
        match blocking(move || flag.needs_setup(&db)).await {
            Ok(Ok(true)) => return Some(redirect("/setup")),
            Ok(Ok(false)) => {}
            // A locked DB must not turn the home page into a setup redirect.
            Ok(Err(e)) => warn!("setup check failed: {e}"),
            Err(_) => {}
        }
    }
    if !is_browser_request(headers) {
        return None;
    }
    let catalog_home = {
        let c = deps.core.config.read();
        c.catalog.enabled && c.catalog.use_as_homepage
    };
    if catalog_home {
        return Some(redirect("/catalog/"));
    }
    Some(serve_page_json_errors(
        "home",
        "index.html",
        "File not found",
        Some("no-cache"),
    ))
}

/// Middleware form of [`root_response`] for the app's outermost router:
/// `.layer(axum::middleware::from_fn_with_state(deps.clone(), root_middleware))`.
pub async fn root_middleware(
    State(deps): State<AccountsDeps>,
    req: Request,
    next: Next,
) -> Response {
    if let Some(resp) = root_response(&deps, req.method(), req.uri().path(), req.headers()).await {
        return resp;
    }
    next.run(req).await
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

    #[test]
    fn browser_heuristics() {
        assert!(is_browser_request(&h(&[(
            "accept",
            "text/html,application/xhtml+xml"
        )])));
        assert!(is_browser_request(&h(&[])));
        assert!(!is_browser_request(&h(&[("user-agent", "davfs2/1.5")])));
        assert!(!is_browser_request(&h(&[(
            "user-agent",
            "Microsoft-WebDAV-MiniRedir/10"
        )])));
        assert!(is_browser_request(&h(&[
            ("accept", "text/html"),
            ("user-agent", "davfs2")
        ])));
        // 0.5.2 quirk: a Depth header only counts when its value contains "Depth".
        assert!(is_browser_request(&h(&[("depth", "1")])));
        assert!(!is_browser_request(&h(&[("depth", "Depth")])));
    }
}
