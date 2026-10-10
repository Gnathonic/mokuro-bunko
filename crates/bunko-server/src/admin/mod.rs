//! The admin panel: `/_admin` (static SPA) and every `/_admin/api/*` endpoint
//! (0.5.2 `admin/api.py`, spec db-auth-admin §18, web-frontend-contract §5.8).
//!
//! Routing is done by hand rather than with one axum route per endpoint because 0.5.2's
//! contract is not a REST router's: the role gate runs before any matching (an inviter
//! asking for `/api/users` gets 403, not 404), an unknown path *or* a wrong method on a
//! known one is `404 {"error":"API endpoint not found"}`, and every answer is JSON.
//!
//! The OCR half of the panel is served through the [`ocr::OcrAdmin`] trait so the OCR
//! subsystem can be plugged in later; [`ocr::NoOcr`] is the "this process runs no OCR"
//! stand-in. Updates (new in 0.7) live in [`update`].

// Handlers return `Result<_, Response>` so an early refusal is just `?`/`return Err`.
#![allow(clippy::result_large_err)]

mod accounts;
mod audit;
mod machine;
pub mod ocr;
mod settings;
mod status;
pub mod update;

use crate::core::{Core, RequestCtx};
use crate::http::static_files;
use crate::ops::dyndns::DynDnsService;
use crate::ops::tunnel::TunnelService;
use axum::Router;
use axum::body::Body;
use axum::extract::{FromRef, FromRequestParts, Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use bunko_core::Role;
use bunko_db::Database;
use http::{HeaderValue, Method, StatusCode, header};
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub use machine::CHOOSABLE_BACKENDS;
pub use ocr::{NoOcr, OcrAdmin};
pub use update::{AutoDeps, Quiet, UpdateService, UpdateSource};

/// Max JSON body (0.5.2 `MAX_JSON_BODY_BYTES`).
pub const MAX_JSON_BODY_BYTES: usize = 65_536;

/// Called with `(username, reason)` after an account was deleted, disabled or given a
/// non-processor role: cut off any processor that account has connected, now.
pub type DropProcessors = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// Called once an update has been installed: restart the server gracefully.
pub type Restart = Arc<dyn Fn() + Send + Sync>;

/// What the admin module needs.
pub struct AdminDeps {
    pub core: Core,
    pub db: Arc<Database>,
    /// The OCR half of the panel ([`NoOcr`] when this process runs no OCR).
    pub ocr: Arc<dyn OcrAdmin>,
    /// `None`: the tunnel endpoints answer "not available", as 0.5.2 without a service.
    pub tunnel: Option<TunnelService>,
    /// Reconfigured live on `PUT /api/settings/dyndns`.
    pub dyndns: Option<DynDnsService>,
    /// Release checks and one-click updates; `None` hides the feature (404).
    pub updates: Option<UpdateService>,
    pub drop_processors: Option<DropProcessors>,
    /// `None`: an applied update says `restarting: false` and waits for a manual restart.
    pub restart: Option<Restart>,
    /// The machine this server runs on (the "This server" tab); `None`: 404.
    pub machine: Option<Arc<dyn crate::machine::Machine>>,
}

#[derive(Clone)]
pub(crate) struct AdminState(Arc<AdminInner>);

pub(crate) struct AdminInner {
    pub deps: AdminDeps,
    /// 0.5.2 `_config_lock`: one read-modify-save of the config at a time.
    pub config_lock: parking_lot::Mutex<()>,
    /// When each long-running "This server" action last started (a short cooldown).
    pub machine_actions:
        parking_lot::Mutex<std::collections::HashMap<&'static str, std::time::Instant>>,
}

impl std::ops::Deref for AdminState {
    type Target = AdminInner;
    fn deref(&self) -> &AdminInner {
        &self.0
    }
}

impl FromRef<AdminState> for Core {
    fn from_ref(s: &AdminState) -> Core {
        s.deps.core.clone()
    }
}

impl AdminState {
    pub fn core(&self) -> &Core {
        &self.deps.core
    }
    pub fn db(&self) -> Arc<Database> {
        self.deps.db.clone()
    }
    pub fn ocr(&self) -> Arc<dyn OcrAdmin> {
        self.deps.ocr.clone()
    }
    pub fn drop_processors(&self, username: &str, reason: &str) {
        if let Some(f) = &self.deps.drop_processors {
            // A registry never fails an admin edit (0.5.2 swallowed its errors too).
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(username, reason)));
            if r.is_err() {
                tracing::warn!("dropping the processors of '{username}' panicked");
            }
        }
    }
}

/// `/_admin`, `/_admin/` and `/_admin/<anything>`.
pub fn router(deps: AdminDeps) -> Router {
    let state = AdminState(Arc::new(AdminInner {
        deps,
        config_lock: parking_lot::Mutex::new(()),
        machine_actions: parking_lot::Mutex::new(std::collections::HashMap::new()),
    }));
    Router::new()
        .route("/_admin", any(entry))
        .route("/_admin/", any(entry))
        .route("/_admin/{*rest}", any(entry))
        .with_state(state)
}

// --- responses ---------------------------------------------------------------------------

/// A JSON response with the given status.
pub(crate) fn json_response(status: u16, body: &Value) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec());
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    resp
}

pub(crate) fn ok(body: Value) -> Response {
    json_response(200, &body)
}

/// `{"error": msg}` with `status`.
pub(crate) fn error(status: u16, msg: impl Into<String>) -> Response {
    json_response(status, &json!({"error": msg.into()}))
}

pub(crate) fn not_found() -> Response {
    error(404, "API endpoint not found")
}

/// An unexpected failure (database, I/O): logged, answered 500 with a JSON body.
pub(crate) fn internal(what: &str, e: impl std::fmt::Display) -> Response {
    tracing::error!("admin: {what}: {e}");
    error(500, format!("{what}: {e}"))
}

/// Run blocking work (SQLite, files, OCR control calls) off the async workers.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Response> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| internal("worker failed", e))
}

// --- request ------------------------------------------------------------------------------

/// The parts of an API request every handler may need.
pub(crate) struct ApiRequest {
    pub ctx: RequestCtx,
    pub method: Method,
    /// Raw query string (`a=1&b=2`), empty when absent.
    pub query: String,
    /// `_parse_json_body`: `{}` for an empty body; `Err(message)` for a body that is too
    /// large, not JSON or not a JSON object. Only read for POST/PUT.
    pub body: Result<Map<String, Value>, String>,
}

impl ApiRequest {
    pub fn actor(&self) -> Option<String> {
        self.ctx.identity.username().map(str::to_string)
    }

    /// The body, or the 400 every handler answers for an unreadable one.
    pub fn json(&self) -> Result<&Map<String, Value>, Response> {
        self.body.as_ref().map_err(|e| error(400, e.clone()))
    }
}

async fn read_body(headers: &http::HeaderMap, body: Body) -> Result<Map<String, Value>, String> {
    const TOO_LARGE: &str = "Request body too large";
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if declared == Some(0) {
        return Ok(Map::new());
    }
    if declared.is_some_and(|n| n > MAX_JSON_BODY_BYTES as u64) {
        return Err(TOO_LARGE.into());
    }
    let bytes = axum::body::to_bytes(body, MAX_JSON_BODY_BYTES)
        .await
        .map_err(|_| TOO_LARGE.to_string())?;
    if bytes.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(m)) => Ok(m),
        // 0.5.2 accepted a non-object and crashed later (500); spec §18.1 says 400.
        _ => Err("Invalid JSON body".into()),
    }
}

/// Percent-decode one path segment (WSGI hands 0.5.2 a decoded `PATH_INFO`).
pub(crate) fn decode_segment(seg: &str) -> String {
    percent_encoding::percent_decode_str(seg)
        .decode_utf8_lossy()
        .into_owned()
}

/// `urllib.parse.parse_qs`: `+` is a space, percent-escapes decoded, pairs without `=`
/// and blank values dropped.
pub(crate) fn parse_qs(query: &str) -> Vec<(String, String)> {
    let decode = |s: &str| decode_segment(&s.replace('+', " "));
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let v = decode(v);
            (!v.is_empty()).then(|| (decode(k), v))
        })
        .collect()
}

/// First value of `name`, stripped; empty → None (0.5.2 `one`).
pub(crate) fn query_one(q: &[(String, String)], name: &str) -> Option<String> {
    q.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| bunko_db::pyfmt::strip(v).to_string())
        .filter(|v| !v.is_empty())
}

// --- entry and dispatch --------------------------------------------------------------------

async fn entry(State(state): State<AdminState>, req: Request) -> Response {
    let path = req.uri().path().to_string();
    let sub = path.strip_prefix("/_admin").unwrap_or("/");
    let sub = if sub.is_empty() { "/" } else { sub };
    if !sub.starts_with("/api/") {
        return serve_static(sub);
    }
    let sub = sub.to_string();
    let (mut parts, body) = req.into_parts();
    let ctx = match RequestCtx::from_request_parts(&mut parts, &state).await {
        Ok(c) => c,
        Err(never) => match never {},
    };
    // 0.5.2 `_can_access_api`: a second gate behind the auth middleware's.
    let invites = sub == "/api/invites" || sub.starts_with("/api/invites/");
    let role = ctx.identity.role();
    if invites {
        if !matches!(role, Role::Admin | Role::Inviter) {
            return error(403, "Admin or inviter access required");
        }
    } else if role != Role::Admin {
        return error(403, "Admin access required");
    }
    // CSRF (new in 0.7): a state-changing call must come from this server's pages or an
    // allowed CORS origin, and a body must be declared JSON (see `http::csrf`).
    let refusal = {
        let cfg = state.core().config.read();
        crate::http::csrf::check(&parts.method, &parts.headers, &cfg.cors)
    };
    if let Some((status, msg)) = refusal {
        return error(status, msg);
    }
    let method = parts.method.clone();
    let body = if method == Method::POST || method == Method::PUT {
        read_body(&parts.headers, body).await
    } else {
        Ok(Map::new())
    };
    let req = ApiRequest {
        ctx,
        method,
        query: parts.uri.query().unwrap_or("").to_string(),
        body,
    };
    dispatch(&state, &sub["/api/".len()..], req).await
}

async fn dispatch(s: &AdminState, api_path: &str, req: ApiRequest) -> Response {
    let seg: Vec<&str> = api_path.split('/').collect();
    let m = req.method.clone();
    let get = m == Method::GET;
    let post = m == Method::POST;
    let put = m == Method::PUT;
    let delete = m == Method::DELETE;
    match seg.as_slice() {
        ["users"] if get => accounts::list_users(s).await,
        ["users"] if post => accounts::create_user(s, &req).await,
        ["users", u] if delete => accounts::delete_user(s, &req, &decode_segment(u)).await,
        ["users", u, "notes"] if put => accounts::update_notes(s, &req, &decode_segment(u)).await,
        ["users", u, "role"] if put => accounts::change_role(s, &req, &decode_segment(u)).await,
        ["users", u, "approve"] if post => {
            accounts::approve_user(s, &req, &decode_segment(u)).await
        }
        ["users", u, "disable"] if post => {
            accounts::disable_user(s, &req, &decode_segment(u)).await
        }

        ["invites"] if get => accounts::list_invites(s).await,
        ["invites"] if post => accounts::create_invite(s, &req).await,
        ["invites", code] if delete => {
            accounts::delete_invite(s, &req, &decode_segment(code)).await
        }

        ["audit"] if get => audit::list(s, &req).await,

        ["settings"] if get => settings::get(s).await,
        ["settings", "registration"] if put => settings::registration(s, &req).await,
        ["settings", "cors"] if put => settings::cors(s, &req).await,
        ["settings", "catalog"] if put => settings::catalog(s, &req).await,
        ["settings", "queue"] if put => settings::queue(s, &req).await,
        ["settings", "ocr"] if put => settings::ocr(s, &req).await,
        ["settings", "dyndns"] if put => settings::dyndns(s, &req).await,

        ["ocr", "generations", "stats"] if get => ocr::http::stats(s).await,
        ["ocr", "generations"] if get => ocr::http::list(s).await,
        ["ocr", "generations"] if put => ocr::http::replace(s, &req).await,
        ["ocr", "generations", "derive"] if post => ocr::http::derive(s, &req).await,
        ["ocr", "devices", "refresh"] if post => ocr::http::refresh_devices(s).await,
        ["ocr", "generations", middle @ .., "pools"] if put && !middle.is_empty() => {
            ocr::http::pools(s, &req, &decode_segment(&middle.join("/"))).await
        }
        ["ocr", "generations", middle @ .., "bench"] if !middle.is_empty() => {
            ocr::http::bench(s, &req, &decode_segment(&middle.join("/"))).await
        }
        ["ocr", ..] => ocr::http::other(s, &req, &seg).await,

        ["status"] if get => status::status(s).await,
        ["processors"] if get => ocr::http::processors(s).await,

        ["tunnel", "status"] if get => status::tunnel_status(s),
        ["tunnel", "start"] if post => status::tunnel_start(s),
        ["tunnel", "stop"] if post => status::tunnel_stop(s).await,

        ["dyndns", "status"] if get => status::dyndns_status(s),
        ["dyndns", "start"] if post => status::dyndns_start(s),
        ["dyndns", "stop"] if post => status::dyndns_stop(s),
        ["dyndns", "test"] if post => status::dyndns_test(s).await,

        ["machine"] if get => machine::overview(s).await,
        ["machine", "ocr"] if put => machine::ocr(s, &req).await,
        ["machine", "install"] if post => machine::install(s, &req).await,
        ["machine", "remove"] if post => machine::remove(s).await,
        ["machine", "jobs"] if post => machine::start_job(s, &req).await,
        ["machine", "jobs", id] if get => machine::job(s, &req, id).await,
        ["machine", "logs"] if get => machine::logs(s, &req).await,

        ["update"] if get => update::http::get(s, &req).await,
        ["update", "apply"] if post => update::http::apply(s, &req).await,
        ["update", "settings"] if post => update::http::settings(s, &req).await,

        _ => not_found(),
    }
}

/// 0.5.2 `_handle_static`: `/` is `index.html`, traversal 403, a missing file falls back
/// to `index.html` (SPA routing), `Cache-Control: no-cache`. Any method.
fn serve_static(sub: &str) -> Response {
    let file = sub.trim_start_matches('/');
    let file = if file.is_empty() { "index.html" } else { file };
    let decoded = decode_segment(file);
    if decoded.split(['/', '\\']).any(|p| p == "..") || decoded.contains('\0') {
        return text(403, "Forbidden");
    }
    static_files::serve("admin", &decoded, Some("no-cache"))
        .or_else(|| static_files::serve("admin", "index.html", Some("no-cache")))
        .unwrap_or_else(|| text(404, "Not found"))
}

fn text(status: u16, body: &'static str) -> Response {
    let mut resp = (
        StatusCode::from_u16(status).unwrap_or(StatusCode::NOT_FOUND),
        body,
    )
        .into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    resp
}

/// Persist the live config after an admin edit, then re-apply the parts other modules
/// cache (the trusted-proxy list). Called with `config_lock` held.
pub(crate) fn save_config(s: &AdminState) -> Result<(), Response> {
    s.core()
        .save_config()
        .map_err(|e| internal("could not save the config", e))?;
    s.core().refresh_proxies();
    Ok(())
}
