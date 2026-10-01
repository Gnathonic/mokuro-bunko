//! First-run web setup wizard (0.5.2 `setup/api.py`, spec config-cli-ops §9).
//!
//! "Setup needed" means no account with role `admin` exists (any status). Once an admin
//! is seen the answer is cached for the life of the process.
//!
//! # Who may run setup
//!
//! 0.5.2 allowed only loopback requests: the TCP peer must be loopback, and the client
//! address resolved through trusted proxies must be loopback too (so a local reverse
//! proxy forwarding a public client is refused). That made the wizard unreachable from
//! the host's browser under Docker bridge networking, where the peer is the bridge
//! gateway.
//!
//! New in 0.7: a **one-time setup token** also passes the gate. The token is
//! `MOKURO_SETUP_TOKEN` (read when [`AccountsDeps`](super::AccountsDeps) is built) or
//! the contents of `<storage>/.setup-token`, which the server creates and logs at
//! startup while no admin exists ([`ensure_setup_token`]). A request presents it as
//! `?token=<t>`, an `X-Setup-Token: <t>` header, or the `mokuro_setup_token` cookie.
//! Opening `/setup?token=<t>` sets that cookie (`Path=/setup; HttpOnly;
//! SameSite=Strict`, one hour), so the unchanged wizard page's own `fetch()` calls to
//! `/setup/api/*` carry it. The token file is deleted when setup completes.

use super::AccountsDeps;
use super::util::{
    Client, JsonBody, blocking, db_failed, json_error, json_response, read_body,
    serve_page_json_errors,
};
use crate::http::client_ip::is_loopback;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use axum::routing::{get, post};
use bunko_core::{Role, StorageLayout};
use bunko_db::pyfmt::strip;
use bunko_db::{Database, DbError, UserStatus, validate_password, validate_username};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, header};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{info, warn};

pub const SETUP_TOKEN_ENV: &str = "MOKURO_SETUP_TOKEN";
pub const SETUP_TOKEN_FILE: &str = ".setup-token";
pub const SETUP_TOKEN_HEADER: &str = "x-setup-token";
pub const SETUP_TOKEN_COOKIE: &str = "mokuro_setup_token";

const LOCAL_ONLY: &str = "Setup is only allowed from localhost";
const VALID_MODES: [&str; 4] = ["disabled", "self", "invite", "approval"];

/// First-run state, shared by every clone of the deps.
#[derive(Clone, Default)]
pub struct SetupFlag {
    complete: Arc<AtomicBool>,
    /// `MOKURO_SETUP_TOKEN`, captured once.
    pub env_token: Option<String>,
}

impl SetupFlag {
    pub fn from_env() -> Self {
        let env_token = std::env::var(SETUP_TOKEN_ENV)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        SetupFlag {
            complete: Arc::default(),
            env_token,
        }
    }

    /// No admin account exists (blocking: reads the users table until one is seen).
    pub fn needs_setup(&self, db: &Database) -> bunko_db::Result<bool> {
        if self.complete.load(Ordering::Acquire) {
            return Ok(false);
        }
        let has_admin = db.list_users(None)?.iter().any(|u| u.role == Role::Admin);
        if has_admin {
            self.complete.store(true, Ordering::Release);
        }
        Ok(!has_admin)
    }

    fn mark_complete(&self) {
        self.complete.store(true, Ordering::Release);
    }
}

fn token_path(layout: &StorageLayout) -> PathBuf {
    layout.base.join(SETUP_TOKEN_FILE)
}

/// For startup while no admin exists: the token to log. `None` when
/// `MOKURO_SETUP_TOKEN` is set (the operator already knows it). Otherwise the existing
/// `<storage>/.setup-token`, or a new random one written there (mode 0600 on Unix).
pub fn ensure_setup_token(layout: &StorageLayout) -> std::io::Result<Option<String>> {
    if std::env::var(SETUP_TOKEN_ENV).is_ok_and(|t| !t.trim().is_empty()) {
        return Ok(None);
    }
    let path = token_path(layout);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let t = existing.trim();
        if !t.is_empty() {
            return Ok(Some(t.to_string()));
        }
    }
    let token = random_token();
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write as _;
    opts.open(&path)?.write_all(token.as_bytes())?;
    Ok(Some(token))
}

/// Delete `<storage>/.setup-token` (done when setup completes).
pub fn remove_setup_token(layout: &StorageLayout) {
    let path = token_path(layout);
    match std::fs::remove_file(&path) {
        Ok(()) => info!("setup complete; removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("could not remove {}: {e}", path.display()),
    }
}

/// 32 random bytes as URL-safe base64 (43 characters).
fn random_token() -> String {
    use base64::Engine as _;
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Compare without an early exit, so timing does not leak the token.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The setup tokens currently accepted.
fn accepted_tokens(deps: &AccountsDeps) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(t) = &deps.setup.env_token {
        out.push(t.clone());
    }
    if let Ok(t) = std::fs::read_to_string(token_path(&deps.core.layout)) {
        let t = t.trim();
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out
}

fn query_token(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == "token").then(|| {
            percent_encoding::percent_decode_str(&v.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        })
    })
}

fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            let (k, v) = c.trim().split_once('=')?;
            (k == SETUP_TOKEN_COOKIE).then(|| v.trim().to_string())
        })
}

/// Every token the request presents (query, header, cookie).
fn presented_tokens(parts: &Parts) -> Vec<String> {
    let header = parts
        .headers
        .get(SETUP_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string());
    [
        query_token(parts.uri.query()),
        header,
        cookie_token(&parts.headers),
    ]
    .into_iter()
    .flatten()
    .filter(|t| !t.is_empty())
    .collect()
}

fn token_matches(accepted: &[String], presented: &str) -> bool {
    accepted
        .iter()
        .any(|t| constant_time_eq(t.as_bytes(), presented.as_bytes()))
}

/// 0.5.2 `_is_local_request`: loopback peer, and a loopback client address if proxy
/// headers name a different one. An unknown peer (no `ConnectInfo`) is not local.
fn is_local(client: &Client) -> bool {
    if !client.peer_known || !client.peer.is_loopback() {
        return false;
    }
    client.ip == client.peer.to_string() || is_loopback(&client.ip)
}

/// May this request run setup: local, or carrying a valid setup token.
fn allowed(deps: &AccountsDeps, client: &Client, parts: &Parts) -> bool {
    if is_local(client) {
        return true;
    }
    let presented = presented_tokens(parts);
    if presented.is_empty() {
        return false;
    }
    let accepted = accepted_tokens(deps);
    presented.iter().any(|p| token_matches(&accepted, p))
}

/// Blocking `needs_setup`; a DB failure answers 500.
async fn needs_setup(deps: &AccountsDeps) -> Result<bool, Response> {
    let (flag, db) = (deps.setup.clone(), deps.db.clone());
    match blocking(move || flag.needs_setup(&db)).await? {
        Ok(v) => Ok(v),
        Err(e) => Err(db_failed("setup check", &e)),
    }
}

/// The 403 gate the setup pages and status apply while setup is needed.
async fn gate(deps: &AccountsDeps, client: &Client, parts: &Parts) -> Option<Response> {
    match needs_setup(deps).await {
        Err(resp) => Some(resp),
        Ok(true) if !allowed(deps, client, parts) => Some(json_error(403, LOCAL_ONLY)),
        Ok(_) => None,
    }
}

pub fn routes() -> Router<AccountsDeps> {
    Router::new()
        .route("/setup/api/status", get(status))
        .route("/setup/api/complete", post(complete))
        .route("/setup", get(index))
        .route("/setup/", get(index))
        .route("/setup/{*file}", get(file))
}

async fn status(State(d): State<AccountsDeps>, client: Client, parts: Parts) -> Response {
    if let Some(resp) = gate(&d, &client, &parts).await {
        return resp;
    }
    match needs_setup(&d).await {
        Ok(needed) => json_response(200, json!({ "needs_setup": needed })),
        Err(resp) => resp,
    }
}

async fn index(State(d): State<AccountsDeps>, client: Client, parts: Parts) -> Response {
    if let Some(resp) = gate(&d, &client, &parts).await {
        return resp;
    }
    let mut resp = serve_page_json_errors("setup", "index.html", "Not found", Some("no-cache"));
    // A valid `?token=` is remembered for the page's own API calls.
    if let Some(t) =
        query_token(parts.uri.query()).filter(|t| token_matches(&accepted_tokens(&d), t))
    {
        let cookie = format!(
            "{SETUP_TOKEN_COOKIE}={t}; Path=/setup; Max-Age=3600; HttpOnly; SameSite=Strict"
        );
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            resp.headers_mut().insert(header::SET_COOKIE, v);
        }
    }
    resp
}

async fn file(
    State(d): State<AccountsDeps>,
    client: Client,
    Path(file): Path<String>,
    parts: Parts,
) -> Response {
    // 0.5.2 passed `/setup/api/...` GETs on to WebDAV; nothing there serves them.
    if file.starts_with("api/") {
        return json_error(404, "Not found");
    }
    if let Some(resp) = gate(&d, &client, &parts).await {
        return resp;
    }
    serve_page_json_errors("setup", &file, "Not found", Some("no-cache"))
}

/// `POST /setup/api/complete` `{admin: {username, password}, registration: {mode}}`.
async fn complete(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    if !allowed(&d, &client, &parts) {
        return json_error(403, LOCAL_ONLY);
    }
    // One completion at a time: two concurrent requests must not both see "no admin
    // yet" and create two admins.
    static COMPLETING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one_at_a_time = COMPLETING.lock().await;
    match needs_setup(&d).await {
        Ok(true) => {}
        Ok(false) => return json_error(400, "Setup already completed"),
        Err(resp) => return resp,
    }
    let data = match read_body(&parts.headers, body).await {
        JsonBody::Empty => return json_error(400, "Empty body"),
        JsonBody::TooLarge => return json_error(413, "Request body too large"),
        // Non-object JSON (a 500 in 0.5.2) is refused like bad JSON.
        JsonBody::Bytes(b) => match super::util::parse_object(&b) {
            Some(m) => m,
            None => return json_error(400, "Invalid JSON"),
        },
    };
    let empty = serde_json::Map::new();
    let admin = match data.get("admin") {
        None => &empty,
        Some(Value::Object(m)) => m,
        Some(_) => return json_error(400, "Invalid JSON"),
    };
    let username = strip(admin.get("username").and_then(Value::as_str).unwrap_or("")).to_string();
    let password = admin
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(msg) = validate_username(&username) {
        return json_error(400, msg);
    }
    if let Some(msg) = validate_password(&password) {
        return json_error(400, msg);
    }
    let db = d.db.clone();
    match blocking(move || {
        db.create_user(&username, &password, Role::Admin, UserStatus::Active, "")
    })
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e @ (DbError::Invalid(_) | DbError::Conflict(_)))) => {
            return json_error(409, &e.to_string());
        }
        Ok(Err(e)) => return db_failed("setup admin", &e),
        Err(resp) => return resp,
    }
    let mode = data
        .get("registration")
        .and_then(|r| r.get("mode"))
        .and_then(Value::as_str);
    if let Some(mode) = mode.filter(|m| VALID_MODES.contains(m)) {
        d.core.config.write().registration.mode = mode.to_string();
    }
    let core = d.core.clone();
    let saved = blocking(move || {
        let saved = core.save_config();
        remove_setup_token(&core.layout);
        saved
    })
    .await;
    // The admin exists either way; 0.5.2 answered 500 here, leaving the wizard stuck on
    // "Setup already completed" for a retry.
    if let Ok(Err(e)) = saved {
        warn!("setup: could not save the config: {e}");
    }
    d.setup.mark_complete();
    json_response(
        201,
        json!({ "success": true, "message": "Setup completed successfully" }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_and_cookie_parsing() {
        assert_eq!(query_token(Some("a=1&token=ab%2Bc")), Some("ab+c".into()));
        assert_eq!(query_token(Some("tokens=x")), None);
        assert_eq!(query_token(None), None);
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "x=1; mokuro_setup_token=abc ; y=2".parse().unwrap(),
        );
        assert_eq!(cookie_token(&h), Some("abc".into()));
    }

    #[test]
    fn token_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert_eq!(random_token().len(), 43);
    }

    #[test]
    fn token_file_lifecycle() {
        // MOKURO_SETUP_TOKEN is not set in the test environment.
        if std::env::var(SETUP_TOKEN_ENV).is_ok() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(tmp.path());
        let t = ensure_setup_token(&layout).unwrap().unwrap();
        assert_eq!(
            ensure_setup_token(&layout).unwrap().unwrap(),
            t,
            "reused, not regenerated"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tmp.path().join(SETUP_TOKEN_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        remove_setup_token(&layout);
        assert!(!tmp.path().join(SETUP_TOKEN_FILE).exists());
    }
}
