//! First-run web setup wizard (0.5.2 `setup/api.py`, spec config-cli-ops §9).
//!
//! "Setup needed" means no account with role `admin` exists (any status). Once an admin
//! is seen the answer is cached for the life of the process.
//!
//! # Who may run setup
//!
//! 0.5.2 allowed only loopback requests: the TCP peer must be loopback, and the client
//! address resolved through trusted proxies must be loopback too (so a local reverse
//! proxy forwarding a public client is refused). That rule stays, and it made the
//! wizard unreachable for a server in Docker or on a NAS, opened from another computer.
//!
//! New in 0.7: a **one-time setup code**. While no admin exists the server makes a new
//! code at every start ([`SetupFlag::issue_code`]) and prints it in its log
//! ([`crate::app::announce_setup`]); it lives in memory only. `GET /setup` from another
//! computer answers a page asking for it; `POST /setup/code` checks it (constant time;
//! [`ATTEMPTS_PER_IP`] tries per client address, one more every [`REFILL`]; after
//! [`ROTATE_AFTER`] wrong codes from everyone within a minute the code is replaced and
//! the new one logged, so nobody can lock the owner out) and answers a random setup-session cookie ([`SESSION_COOKIE`], `Path=/setup`,
//! `HttpOnly; SameSite=Strict`, one hour), which the unchanged wizard page and its
//! `/setup/api/*` calls carry. A script may send the code as `X-Setup-Code` instead
//! (same limits). The code and every session die as soon as an admin exists (the wizard,
//! `admin add-user`). The code is never written to a response.

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
use http::{HeaderMap, HeaderValue, StatusCode, header};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// The header a script may send the setup code in.
pub const SETUP_CODE_HEADER: &str = "x-setup-code";
/// The cookie a right code earns (a random session id, never the code).
pub const SESSION_COOKIE: &str = "mokuro_setup_session";
/// 0.7 betas kept a token here; removed at startup.
pub const LEGACY_TOKEN_FILE: &str = ".setup-token";

/// Code attempts a client address may make at once; one more every [`REFILL`]
/// (5 a minute). Self-healing: an address that waits gets its tries back.
pub const ATTEMPTS_PER_IP: u32 = 5;
pub const REFILL: Duration = Duration::from_secs(12);
/// Wrong codes from everyone together within [`ROTATE_WINDOW`] that make the server
/// replace the code (a new one in the log): a guessing run spread over many addresses
/// starts over, and nobody is locked out.
pub const ROTATE_AFTER: usize = 30;
pub const ROTATE_WINDOW: Duration = Duration::from_secs(60);
/// How long a setup session lasts.
pub const SESSION_TTL: Duration = Duration::from_secs(3600);
/// Sessions kept at once (the oldest goes first).
const MAX_SESSIONS: usize = 16;
/// Client addresses the limiter remembers; past it, those whose tries are all back
/// are forgotten, then the least recently seen (memory stays bounded whatever the
/// number of addresses).
const MAX_TRACKED: usize = 1024;

/// Crockford's base32: no I, L, O or U (50 bits in 10 characters).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const CODE_LEN: usize = 10;

pub const REMOTE_NEEDS_CODE: &str = "Setup from another computer needs the one-time setup code: open /setup in a browser and enter the code printed in the server log at startup (see the server log for the setup code)";
const NO_CODE_YET: &str =
    "No setup code is active: restart the server and see its log for the setup code";

/// First-run state, shared by every clone of the deps.
#[derive(Clone, Default)]
pub struct SetupFlag(Arc<Inner>);

#[derive(Default)]
struct Inner {
    complete: AtomicBool,
    /// The current code, normalized (10 characters of [`ALPHABET`]).
    code: Mutex<Option<String>>,
    /// Session id → when it expires.
    sessions: Mutex<HashMap<String, Instant>>,
    limiter: Mutex<Limiter>,
}

/// One client address's tries: a token bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    /// When `tokens` was last brought up to date.
    at: Instant,
}

impl Bucket {
    fn refill(&mut self, now: Instant) {
        let gained = now.saturating_duration_since(self.at).as_secs_f64() / REFILL.as_secs_f64();
        self.tokens = (self.tokens + gained).min(f64::from(ATTEMPTS_PER_IP));
        self.at = now;
    }

    fn full(&self, now: Instant) -> bool {
        let mut b = *self;
        b.refill(now);
        b.tokens >= f64::from(ATTEMPTS_PER_IP)
    }
}

#[derive(Default)]
struct Limiter {
    per_ip: HashMap<String, Bucket>,
    /// The wrong codes of the last [`ROTATE_WINDOW`], from everyone.
    wrong: VecDeque<Instant>,
}

impl Limiter {
    /// Take one try for `key`; Err(seconds until the next one) when it has none.
    fn attempt(&mut self, key: &str, now: Instant) -> Result<(), u64> {
        if !self.per_ip.contains_key(key) && self.per_ip.len() >= MAX_TRACKED {
            self.per_ip.retain(|_, b| !b.full(now));
            if self.per_ip.len() >= MAX_TRACKED
                && let Some(oldest) = self
                    .per_ip
                    .iter()
                    .min_by_key(|(_, b)| b.at)
                    .map(|(k, _)| k.clone())
            {
                self.per_ip.remove(&oldest);
            }
        }
        let b = self.per_ip.entry(key.to_string()).or_insert(Bucket {
            tokens: f64::from(ATTEMPTS_PER_IP),
            at: now,
        });
        b.refill(now);
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            let wait = (1.0 - b.tokens) * REFILL.as_secs_f64();
            Err(wait.ceil().max(1.0) as u64)
        }
    }

    /// A wrong code: true when there were [`ROTATE_AFTER`] within [`ROTATE_WINDOW`]
    /// (the count starts over).
    fn wrong(&mut self, now: Instant) -> bool {
        while self
            .wrong
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= ROTATE_WINDOW)
        {
            self.wrong.pop_front();
        }
        self.wrong.push_back(now);
        if self.wrong.len() >= ROTATE_AFTER {
            self.wrong.clear();
            return true;
        }
        false
    }
}

/// The limiter's key for a client: its address as resolved (the socket peer, or what a
/// trusted proxy says), never a raw header a stranger chose; an IPv6 address by its /64
/// (one host's share of addresses).
fn limit_key(client: &Client) -> String {
    let ip = client.ip.parse::<std::net::IpAddr>().unwrap_or(client.peer);
    match ip {
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        v4 => v4.to_string(),
    }
}

/// What a request presenting credentials gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Allowed,
    Denied,
    /// Too many attempts: retry after this many seconds.
    Limited(u64),
}

impl SetupFlag {
    /// No admin account exists (blocking: reads the users table until one is seen).
    /// Seeing one ends the code and the sessions.
    pub fn needs_setup(&self, db: &Database) -> bunko_db::Result<bool> {
        if self.0.complete.load(Ordering::Acquire) {
            return Ok(false);
        }
        let has_admin = db.list_users(None)?.iter().any(|u| u.role == Role::Admin);
        if has_admin {
            self.mark_complete();
        }
        Ok(!has_admin)
    }

    /// Make a new setup code (replacing any earlier one) and return it for display
    /// (`XXXXX-XXXXX`). Called once at startup while no admin exists.
    pub fn issue_code(&self) -> String {
        use rand::RngCore as _;
        let mut bytes = [0u8; CODE_LEN];
        rand::rng().fill_bytes(&mut bytes);
        let code: String = bytes
            .iter()
            .map(|b| ALPHABET[(*b & 31) as usize] as char)
            .collect();
        *self.0.code.lock() = Some(code.clone());
        format_code(&code)
    }

    /// A code is active (setup is still open to other computers).
    pub fn has_code(&self) -> bool {
        self.0.code.lock().is_some()
    }

    fn mark_complete(&self) {
        self.0.complete.store(true, Ordering::Release);
        *self.0.code.lock() = None;
        self.0.sessions.lock().clear();
    }

    /// Check a presented code against the current one, counting the attempt.
    fn check_code(&self, key: &str, presented: &str) -> Gate {
        self.check_code_at(key, presented, Instant::now())
    }

    fn check_code_at(&self, key: &str, presented: &str, now: Instant) -> Gate {
        if let Err(wait) = self.0.limiter.lock().attempt(key, now) {
            return Gate::Limited(wait);
        }
        let given = normalize_code(presented);
        let ok = match self.0.code.lock().as_deref() {
            Some(code) => constant_time_eq(code.as_bytes(), given.as_bytes()),
            None => false,
        };
        if ok {
            return Gate::Allowed;
        }
        warn!("setup: a wrong setup code was entered from {key}");
        if self.0.limiter.lock().wrong(now) && self.has_code() {
            let code = self.issue_code();
            warn!(
                "setup: {ROTATE_AFTER} wrong setup codes within {} s: the setup code changed. New setup code: {code}",
                ROTATE_WINDOW.as_secs()
            );
        }
        Gate::Denied
    }

    /// Start a session (after a right code): its id, for the cookie.
    fn new_session(&self) -> String {
        use base64::Engine as _;
        use rand::RngCore as _;
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let now = Instant::now();
        let mut s = self.0.sessions.lock();
        s.retain(|_, until| *until > now);
        while s.len() >= MAX_SESSIONS {
            let oldest = s
                .iter()
                .min_by_key(|(_, until)| **until)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => s.remove(&k),
                None => break,
            };
        }
        s.insert(id.clone(), now + SESSION_TTL);
        id
    }

    fn session_valid(&self, id: &str) -> bool {
        let now = Instant::now();
        let s = self.0.sessions.lock();
        s.iter()
            .any(|(k, until)| *until > now && constant_time_eq(k.as_bytes(), id.as_bytes()))
    }
}

/// `ABCDE12345` → `ABCDE-12345`.
fn format_code(code: &str) -> String {
    let (a, b) = code.split_at(code.len() / 2);
    format!("{a}-{b}")
}

/// What a person typed → the code's characters: upper case, no spaces or dashes, and
/// the letters Crockford's alphabet leaves out read as the digits they look like.
pub fn normalize_code(input: &str) -> String {
    input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect()
}

/// Delete the 0.7 betas' `<storage>/.setup-token` (it no longer opens anything).
pub fn remove_legacy_token(layout: &StorageLayout) {
    let path = layout.base.join(LEGACY_TOKEN_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => info!("removed {} (setup now uses a setup code)", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("could not remove {}: {e}", path.display()),
    }
}

/// Compare without an early exit, so timing does not leak the code (a length
/// difference is folded in too, not returned early).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        diff |= a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| {
            let (k, v) = c.trim().split_once('=')?;
            (k == SESSION_COOKIE).then(|| v.trim().to_string())
        })
}

/// 0.5.2 `_is_local_request`: loopback peer, and a loopback client address if proxy
/// headers name a different one. An unknown peer (no `ConnectInfo`) is not local.
fn is_local(client: &Client) -> bool {
    if !client.peer_known || !client.peer.is_loopback() {
        return false;
    }
    client.ip == client.peer.to_string() || is_loopback(&client.ip)
}

/// May this request run setup: local, a live setup session, or the code in
/// `X-Setup-Code` (which counts as an attempt).
fn gate_of(deps: &AccountsDeps, client: &Client, parts: &Parts) -> Gate {
    if is_local(client) {
        return Gate::Allowed;
    }
    if session_cookie(&parts.headers).is_some_and(|s| deps.setup.session_valid(&s)) {
        return Gate::Allowed;
    }
    match parts
        .headers
        .get(SETUP_CODE_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
    {
        Some(code) => deps.setup.check_code(&limit_key(client), code),
        None => Gate::Denied,
    }
}

/// Blocking `needs_setup`; a DB failure answers 500.
async fn needs_setup(deps: &AccountsDeps) -> Result<bool, Response> {
    let (flag, db) = (deps.setup.clone(), deps.db.clone());
    match blocking(move || flag.needs_setup(&db)).await? {
        Ok(v) => Ok(v),
        Err(e) => Err(db_failed("setup check", &e)),
    }
}

fn refusal(gate: Gate) -> Response {
    match gate {
        Gate::Limited(wait) => {
            let mut r = json_error(
                429,
                &format!("Too many setup code attempts. Retry in {wait}s"),
            );
            if let Ok(v) = HeaderValue::from_str(&wait.to_string()) {
                r.headers_mut().insert(header::RETRY_AFTER, v);
            }
            r
        }
        _ => json_error(403, REMOTE_NEEDS_CODE),
    }
}

/// The JSON gate the setup API and files apply while setup is needed.
async fn gate(deps: &AccountsDeps, client: &Client, parts: &Parts) -> Option<Response> {
    match needs_setup(deps).await {
        Err(resp) => Some(resp),
        Ok(true) => match gate_of(deps, client, parts) {
            Gate::Allowed => None,
            g => Some(refusal(g)),
        },
        Ok(false) => None,
    }
}

pub fn routes() -> Router<AccountsDeps> {
    Router::new()
        .route("/setup/api/status", get(status))
        .route("/setup/api/options", get(options))
        .route("/setup/api/complete", post(complete))
        .route("/setup/code", post(code))
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

/// `GET /setup/api/options`: what the wizard's later steps offer here: the OCR step
/// (the machine's hardware, `null` without one), and the answers the environment
/// already gives (`MOKURO_*` variables win over the file at every start).
async fn options(State(d): State<AccountsDeps>, client: Client, parts: Parts) -> Response {
    if let Some(resp) = gate(&d, &client, &parts).await {
        return resp;
    }
    let machine = d.machine.clone();
    let ocr = match machine {
        Some(m) => blocking(move || m.setup_options())
            .await
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    let (registration, ssl) = {
        let c = d.core.config.read();
        (c.registration.mode.clone(), c.ssl.enabled)
    };
    json_response(
        200,
        json!({
            "ocr": ocr,
            "pinned": {
                "registration": env_pin("MOKURO_REGISTRATION_MODE").map(|_| registration),
                "ssl": env_pin("MOKURO_SSL_ENABLED").map(|_| ssl),
            },
        }),
    )
}

/// The value of a `MOKURO_*` variable that sets a key at every start (None: unset).
fn env_pin(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

async fn index(State(d): State<AccountsDeps>, client: Client, parts: Parts) -> Response {
    match needs_setup(&d).await {
        Err(resp) => return resp,
        Ok(true) => match gate_of(&d, &client, &parts) {
            Gate::Allowed => {}
            Gate::Denied => return code_page(StatusCode::OK, None, d.setup.has_code()),
            Gate::Limited(wait) => {
                return code_page(
                    StatusCode::TOO_MANY_REQUESTS,
                    Some(format!("Too many attempts. Try again in {wait} seconds.")),
                    true,
                );
            }
        },
        Ok(false) => {}
    }
    serve_page_json_errors("setup", "index.html", "Not found", Some("no-cache"))
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
    // The code page's own stylesheet.
    if file != "setup.css"
        && let Some(resp) = gate(&d, &client, &parts).await
    {
        return resp;
    }
    serve_page_json_errors("setup", &file, "Not found", Some("no-cache"))
}

/// `POST /setup/code` (a form: `code=XXXXX-XXXXX`): a right code earns a setup session
/// and goes on to the wizard.
async fn code(State(d): State<AccountsDeps>, client: Client, body: Body) -> Response {
    let to_setup = || {
        let mut r = Response::new(Body::empty());
        *r.status_mut() = StatusCode::SEE_OTHER;
        r.headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static("/setup"));
        r
    };
    match needs_setup(&d).await {
        Err(resp) => return resp,
        Ok(false) => return to_setup(),
        Ok(true) => {}
    }
    if is_local(&client) {
        return to_setup();
    }
    let bytes = match axum::body::to_bytes(body, 4096).await {
        Ok(b) => b,
        Err(_) => return code_page(StatusCode::PAYLOAD_TOO_LARGE, None, d.setup.has_code()),
    };
    let presented = form_value(&bytes, "code").unwrap_or_default();
    if presented.trim().is_empty() {
        return code_page(
            StatusCode::BAD_REQUEST,
            Some("Enter the setup code.".into()),
            d.setup.has_code(),
        );
    }
    match d.setup.check_code(&limit_key(&client), &presented) {
        Gate::Allowed => {
            info!("setup: the setup code was entered from {}", client.ip);
            let id = d.setup.new_session();
            let secure = if d.core.config.read().ssl.enabled {
                "; Secure"
            } else {
                ""
            };
            let cookie = format!(
                "{SESSION_COOKIE}={id}; Path=/setup; Max-Age={}; HttpOnly; SameSite=Strict{secure}",
                SESSION_TTL.as_secs()
            );
            let mut r = to_setup();
            if let Ok(v) = HeaderValue::from_str(&cookie) {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            r
        }
        Gate::Denied => code_page(
            StatusCode::FORBIDDEN,
            Some("That is not the setup code. Check the server log and try again.".into()),
            d.setup.has_code(),
        ),
        Gate::Limited(wait) => {
            let mut r = code_page(
                StatusCode::TOO_MANY_REQUESTS,
                Some(format!("Too many attempts. Try again in {wait} seconds.")),
                true,
            );
            if let Ok(v) = HeaderValue::from_str(&wait.to_string()) {
                r.headers_mut().insert(header::RETRY_AFTER, v);
            }
            r
        }
    }
}

/// `name`'s value in an `application/x-www-form-urlencoded` body.
fn form_value(body: &[u8], name: &str) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?;
    text.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (k == name).then(|| {
            percent_encoding::percent_decode_str(&v.replace('+', " "))
                .decode_utf8_lossy()
                .into_owned()
        })
    })
}

/// The page asking another computer for the setup code. `message` is ours (never what
/// the client sent).
fn code_page(status: StatusCode, message: Option<String>, has_code: bool) -> Response {
    let note = match (message, has_code) {
        (Some(m), _) => format!(r#"<div class="form-error" role="alert">{m}</div>"#),
        (None, false) => format!(r#"<div class="form-error" role="alert">{NO_CODE_YET}.</div>"#),
        (None, true) => String::new(),
    };
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Setup code - Mokuro Bunko</title>
    <link rel="stylesheet" href="/_static/shared.css">
    <link rel="stylesheet" href="/setup/setup.css">
</head>
<body>
    <div class="setup-container">
        <div class="setup-card card">
            <h1 class="setup-title">Set up Mokuro Bunko</h1>
            <p class="setup-description">To create the admin account from another computer, enter the one-time setup code.
            It is in the server log, printed at startup in a line beginning <code>First run:</code>
            (for Docker: <code>docker logs &lt;container&gt;</code>; on Unraid: the container's log).</p>
            <form method="post" action="/setup/code" autocomplete="off">
                <div class="form-group">
                    <label for="setup-code" class="form-label">Setup code</label>
                    <input type="text" id="setup-code" name="code" class="form-input" required autofocus
                           maxlength="32" spellcheck="false" autocapitalize="characters" placeholder="XXXXX-XXXXX">
                    <p class="form-hint">On the server itself, open this page at localhost: no code is needed there.</p>
                </div>
                {note}
                <div class="setup-actions">
                    <button type="submit" class="btn btn--primary btn--lg">Continue</button>
                </div>
            </form>
        </div>
    </div>
</body>
</html>
"#
    );
    let mut r = Response::new(Body::from(html));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// `POST /setup/api/complete` `{admin: {username, password}, registration: {mode}}`.
async fn complete(
    State(d): State<AccountsDeps>,
    client: Client,
    parts: Parts,
    body: Body,
) -> Response {
    // One completion at a time: two concurrent requests must not both see "no admin
    // yet" and create two admins.
    static COMPLETING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one_at_a_time = COMPLETING.lock().await;
    match needs_setup(&d).await {
        Ok(true) => {}
        Ok(false) => {
            // 0.5.2: a non-local caller is refused before "already completed".
            if !is_local(&client) {
                return json_error(403, REMOTE_NEEDS_CODE);
            }
            return json_error(400, "Setup already completed");
        }
        Err(resp) => return resp,
    }
    match gate_of(&d, &client, &parts) {
        Gate::Allowed => {}
        g => return refusal(g),
    }
    if let Some((status, msg)) = crate::http::csrf::require_json(&parts.headers) {
        return json_error(status, msg);
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
    // The other answers are checked before anything is written: a mistake in them
    // must not leave an admin behind with the rest of the setup undone.
    let plan = match SetupPlan::parse(&data, d.machine.is_some()) {
        Ok(p) => p,
        Err(msg) => return json_error(400, &msg),
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
    // The admin exists: the code and the sessions end now.
    d.setup.mark_complete();
    let mode = data
        .get("registration")
        .and_then(|r| r.get("mode"))
        .and_then(Value::as_str);
    let applied = {
        let mut c = d.core.config.write();
        if let Some(mode) = mode.filter(|m| VALID_MODES.contains(m)) {
            c.registration.mode = mode.to_string();
        }
        plan.apply(&mut c)
    };
    let core = d.core.clone();
    let saved = blocking(move || core.save_config()).await;
    // The admin exists either way; 0.5.2 answered 500 here, leaving the wizard stuck on
    // "Setup already completed" for a retry.
    if let Ok(Err(e)) = saved {
        warn!("setup: could not save the config: {e}");
    }
    let snapshot = d.core.config.read().clone();
    if applied.dyndns
        && let Some(service) = &d.dyndns
    {
        service.configure(snapshot.dyndns.clone());
        service.start();
    }
    // OCR on this machine: the install of its backend starts (when automatic installs
    // are on) and its progress shows in the admin panel.
    let mut ocr = Value::Null;
    if let (Some(m), true) = (d.machine.clone(), plan.ocr.is_some()) {
        let backend_changed = applied.backend_changed;
        ocr = blocking(move || m.ocr_changed(&snapshot, backend_changed, true))
            .await
            .unwrap_or(Value::Null);
    }
    // HTTPS starts with the next start: restart now (after this answer goes out).
    let restarting = applied.ssl && d.restart.is_some();
    if restarting && let Some(restart) = d.restart.clone() {
        info!("setup: HTTPS turned on; restarting the server");
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            restart();
        });
    }
    json_response(
        201,
        json!({
            "success": true,
            "message": "Setup completed successfully",
            "notes": applied.notes,
            "ocr": ocr,
            "restarting": restarting,
            "https": applied.ssl,
        }),
    )
}

const VALID_MODES: [&str; 4] = ["disabled", "self", "invite", "approval"];

/// The setup's answers beyond the admin account and the registration mode, checked
/// before anything is written.
#[derive(Debug, Default)]
struct SetupPlan {
    dyndns: Option<bunko_core::config::DynDnsConfig>,
    /// `Some(ssl)`: HTTPS on, with this certificate.
    ssl: Option<bunko_core::config::SslConfig>,
    cors: Vec<String>,
    /// OCR on this machine, and the backend it prefers (None: leave it).
    ocr: Option<(bool, Option<String>)>,
    access: String,
}

/// What [`SetupPlan::apply`] changed.
#[derive(Debug, Default)]
struct Applied {
    dyndns: bool,
    ssl: bool,
    backend_changed: bool,
    notes: Vec<String>,
}

impl SetupPlan {
    fn parse(
        data: &serde_json::Map<String, Value>,
        has_machine: bool,
    ) -> Result<SetupPlan, String> {
        use bunko_core::config::{DYNDNS_PROVIDERS, DynDnsConfig, SslConfig};
        let mut plan = SetupPlan::default();
        let empty = serde_json::Map::new();
        let remote = match data.get("remote") {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(m)) => m,
            Some(_) => return Err("remote: an object".into()),
        };
        let text = |m: &serde_json::Map<String, Value>, k: &str| {
            m.get(k)
                .and_then(Value::as_str)
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        plan.access = text(remote, "access");
        if !["", "lan", "cloudflare", "dyndns", "reverse-proxy"].contains(&plan.access.as_str()) {
            return Err(format!("remote access: {}?", plan.access));
        }
        if plan.access == "dyndns" {
            let d = match remote.get("dyndns") {
                Some(Value::Object(m)) => m,
                _ => &empty,
            };
            let provider = match text(d, "provider") {
                p if p.is_empty() => "duckdns".to_string(),
                p => p,
            };
            if !DYNDNS_PROVIDERS.contains(&provider.as_str()) {
                return Err(format!("Dynamic DNS provider: {provider}?"));
            }
            let (domain, token, url) = (text(d, "domain"), text(d, "token"), text(d, "update_url"));
            if domain.is_empty() || token.is_empty() {
                return Err("Dynamic DNS needs the domain and the token".into());
            }
            if provider == "generic" && url.is_empty() {
                return Err("Dynamic DNS: the generic provider needs an update URL".into());
            }
            plan.dyndns = Some(DynDnsConfig {
                enabled: true,
                update_url: if provider == "generic" {
                    url
                } else {
                    String::new()
                },
                provider,
                token,
                domain,
                ..DynDnsConfig::default()
            });
        }
        let ssl = match remote.get("ssl") {
            Some(Value::Object(m)) => m,
            _ => &empty,
        };
        plan.ssl = match text(ssl, "mode").as_str() {
            "" | "off" => None,
            _ if env_pin("MOKURO_SSL_ENABLED").is_some() => {
                return Err("HTTPS is set by MOKURO_SSL_ENABLED here".into());
            }
            "self-signed" => Some(SslConfig {
                enabled: true,
                auto_cert: true,
                ..SslConfig::default()
            }),
            "files" => {
                let (cert, key) = (text(ssl, "cert_file"), text(ssl, "key_file"));
                for (label, p) in [("certificate", &cert), ("private key", &key)] {
                    if p.is_empty() || !std::path::Path::new(p).is_file() {
                        return Err(format!(
                            "HTTPS: the {label} file {p} does not exist on the server"
                        ));
                    }
                }
                Some(SslConfig {
                    enabled: true,
                    auto_cert: false,
                    cert_file: cert,
                    key_file: key,
                })
            }
            other => return Err(format!("HTTPS: {other}?")),
        };
        if let Some(Value::Array(list)) = remote.get("cors_origins") {
            for o in list.iter().filter_map(Value::as_str).map(str::trim) {
                if o.is_empty() {
                    continue;
                }
                if !(o.starts_with("http://") || o.starts_with("https://")) || o.ends_with('/') {
                    return Err(format!("{o}: an origin is http(s)://host[:port], no path"));
                }
                plan.cors.push(o.to_string());
            }
        }
        if has_machine && let Some(Value::Object(o)) = data.get("ocr") {
            let on = o.get("on").and_then(Value::as_bool).unwrap_or(false);
            let backend = match o.get("backend").and_then(Value::as_str) {
                None | Some("") => None,
                Some(b) if crate::admin::CHOOSABLE_BACKENDS.contains(&b) => Some(b.to_string()),
                Some(b) => return Err(format!("OCR backend: {b}?")),
            };
            plan.ocr = Some((on, backend));
        }
        Ok(plan)
    }

    fn apply(&self, c: &mut bunko_core::Config) -> Applied {
        let mut a = Applied::default();
        if let Some(d) = &self.dyndns {
            c.dyndns = d.clone();
            a.dyndns = true;
        }
        if let Some(ssl) = &self.ssl {
            c.ssl = ssl.clone();
            a.ssl = true;
        }
        for o in &self.cors {
            if !c.cors.allowed_origins.iter().any(|x| x == o) {
                c.cors.allowed_origins.push(o.clone());
            }
        }
        if let Some((on, backend)) = &self.ocr {
            if env_pin("MOKURO_OCR_LOCAL_PROCESSING").is_none() {
                c.ocr.local_processing = *on;
            }
            if let Some(b) = backend
                && env_pin("MOKURO_OCR_BACKEND").is_none()
                && *b != c.ocr.backend
            {
                c.ocr.backend = b.clone();
                a.backend_changed = true;
            }
        }
        match self.access.as_str() {
            "cloudflare" => a
                .notes
                .push("Start the Cloudflare tunnel in Connectivity.".into()),
            "reverse-proxy" => a.notes.push(format!(
                "Point your reverse proxy at port {}.",
                c.server.port
            )),
            _ => {}
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(
            header::COOKIE,
            "x=1; mokuro_setup_session=abc ; y=2".parse().unwrap(),
        );
        assert_eq!(session_cookie(&h), Some("abc".into()));
        assert_eq!(
            form_value(b"a=1&code=ab+c%2D1", "code"),
            Some("ab c-1".into())
        );
        assert_eq!(form_value(b"codes=1", "code"), None);
    }

    #[test]
    fn codes_are_ten_unambiguous_characters() {
        let flag = SetupFlag::default();
        let shown = flag.issue_code();
        assert_eq!(shown.len(), 11);
        assert_eq!(&shown[5..6], "-");
        let raw = normalize_code(&shown);
        assert_eq!(raw.len(), CODE_LEN);
        assert!(raw.bytes().all(|b| ALPHABET.contains(&b)));
        assert!(!raw.contains(['I', 'L', 'O', 'U']));
        // 32 symbols x 10 = 50 bits; a new code each time.
        assert_ne!(normalize_code(&flag.issue_code()), raw);
        assert_eq!(normalize_code(" abc-de oIl "), "ABCDE011");
    }

    #[test]
    fn code_compare_and_limits() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        let flag = SetupFlag::default();
        let code = flag.issue_code();
        assert_eq!(flag.check_code("1.2.3.4", "nope"), Gate::Denied);
        assert_eq!(
            flag.check_code("1.2.3.4", &code.to_lowercase()),
            Gate::Allowed
        );
        for _ in 0..3 {
            assert_eq!(flag.check_code("1.2.3.4", "nope"), Gate::Denied);
        }
        // The sixth attempt is refused, even with the right code.
        assert!(matches!(
            flag.check_code("1.2.3.4", &code),
            Gate::Limited(_)
        ));
        // Others still get their tries.
        assert_eq!(flag.check_code("5.6.7.8", &code), Gate::Allowed);
    }

    /// An attacker's burst costs the attacker's tries only for a while: the right code
    /// works again once its tries came back.
    #[test]
    fn a_burst_does_not_stop_a_later_right_code() {
        let flag = SetupFlag::default();
        let code = flag.issue_code();
        let t0 = Instant::now();
        for i in 0..20 {
            let g = flag.check_code_at("6.6.6.6", "WRONGWRONG", t0 + Duration::from_millis(i));
            assert!(matches!(g, Gate::Denied | Gate::Limited(_)), "{g:?}");
        }
        assert!(matches!(
            flag.check_code_at("6.6.6.6", &code, t0 + Duration::from_secs(1)),
            Gate::Limited(_)
        ));
        // The same address, after a refill period: allowed again.
        assert_eq!(
            flag.check_code_at("6.6.6.6", &code, t0 + REFILL + Duration::from_secs(1)),
            Gate::Allowed
        );
    }

    /// Wrong codes spread over many addresses never lock setup: they replace the code
    /// (the old one stops working, the new one works).
    #[test]
    fn many_wrong_codes_rotate_the_code_instead_of_locking() {
        let flag = SetupFlag::default();
        let old = flag.issue_code();
        let t0 = Instant::now();
        for i in 0..ROTATE_AFTER {
            let ip = format!("10.1.{}.{}", i / 200, i % 200);
            assert_eq!(flag.check_code_at(&ip, "WRONGWRONG", t0), Gate::Denied);
        }
        let new = format_code(flag.0.code.lock().as_deref().unwrap());
        assert_ne!(normalize_code(&new), normalize_code(&old), "rotated");
        assert_eq!(flag.check_code_at("192.0.2.9", &old, t0), Gate::Denied);
        assert_eq!(flag.check_code_at("192.0.2.10", &new, t0), Gate::Allowed);
    }

    /// The limiter forgets addresses: bounded however many there are.
    #[test]
    fn the_limiter_stays_bounded() {
        let mut l = Limiter::default();
        let now = Instant::now();
        for i in 0..10_000u32 {
            let ip = std::net::Ipv4Addr::from(0x0a00_0000 + i).to_string();
            let _ = l.attempt(&ip, now);
            assert!(l.per_ip.len() <= MAX_TRACKED);
        }
        assert!(l.per_ip.len() <= MAX_TRACKED);
        // A new address still gets its tries.
        assert!(l.attempt("192.0.2.1", now).is_ok());
    }

    #[test]
    fn limiter_keys() {
        let c = |ip: &str| Client {
            peer: "203.0.113.5".parse().unwrap(),
            ip: ip.into(),
            peer_known: true,
        };
        assert_eq!(limit_key(&c("203.0.113.5")), "203.0.113.5");
        // Not an address (a trusted proxy's odd header): the peer.
        assert_eq!(limit_key(&c("not-an-ip")), "203.0.113.5");
        // IPv6 by its /64.
        assert_eq!(
            limit_key(&c("2001:db8:1:2:aaaa::1")),
            limit_key(&c("2001:db8:1:2:bbbb::9"))
        );
    }

    #[test]
    fn completion_ends_the_code_and_the_sessions() {
        let flag = SetupFlag::default();
        let code = flag.issue_code();
        let s = flag.new_session();
        assert!(flag.session_valid(&s));
        assert!(!flag.session_valid("other"));
        flag.mark_complete();
        assert!(!flag.has_code());
        assert!(!flag.session_valid(&s));
        assert_eq!(flag.check_code("9.9.9.9", &code), Gate::Denied);
    }
}
