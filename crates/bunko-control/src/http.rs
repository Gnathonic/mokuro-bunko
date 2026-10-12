//! The local control listener (GUI.md §2): `127.0.0.1:<ephemeral>` only, a random
//! bearer token in `<storage>/.control.json` (0600), removed on a clean exit.
//!
//! Routes: `GET /control/status`, `POST /control/pause`, `POST /control/resume`,
//! `GET /control/events` (SSE), `POST /control/stop`, `POST /control/login-code`, plus
//! the app pages a caller mounts (stream G2's `/app` router). Without an app router the
//! listener serves `/app/login?c=<code>` itself (the cookie exchange); with one, the
//! app router answers it (the same exchange, see [`login`]).
//!
//! Every `/control` request needs `Authorization: Bearer <token>` or the cookie
//! [`COOKIE`] (or `bunko_control_<port>`) holding the token; `Host` must name a
//! loopback address (no DNS rebinding); a cookie-authenticated state change must come
//! with this listener's own `Origin` (SameSite does not separate localhost ports).
//!
//! The token never goes into a URL: a browser signs in with a single-use code
//! ([`LoginCodes`]) that a bearer-token holder asks for (`POST /control/login-code`),
//! since a URL ends up in the browser launcher's command line, the history and the
//! terminal.

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::Stream;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::control::{Control, ControlError};
use crate::file::{remove_control_file, write_control_file};
use crate::types::{COOKIE, ControlFile, PauseBody, Role};

/// SSE: at most one `status` event per this long.
pub const EVENTS_MIN_GAP: Duration = Duration::from_millis(500);
/// SSE: re-read the status this often even without a change signal (rates decay,
/// GPU load moves).
pub const EVENTS_REFRESH: Duration = Duration::from_secs(5);

/// A fresh control token: 32 random bytes, hex.
pub fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// How long a sign-in code works.
pub const LOGIN_CODE_TTL: Duration = Duration::from_secs(300);
/// At most this many codes wait to be used; minting another drops the oldest.
pub const LOGIN_CODES_MAX: usize = 64;

/// Single-use sign-in codes for `/app/login?c=<code>`: 16 random bytes (hex), good
/// once and for [`LOGIN_CODE_TTL`]. One store per listener, shared with the app
/// router (clones share it).
#[derive(Clone, Default)]
pub struct LoginCodes(Arc<parking_lot::Mutex<Vec<(String, Instant)>>>);

impl LoginCodes {
    /// A fresh code.
    pub fn mint(&self) -> String {
        use rand::RngCore;
        let mut bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut bytes);
        let code = hex::encode(bytes);
        let now = Instant::now();
        let mut codes = self.0.lock();
        codes.retain(|(_, at)| now.duration_since(*at) < LOGIN_CODE_TTL);
        if codes.len() >= LOGIN_CODES_MAX {
            codes.remove(0);
        }
        codes.push((code.clone(), now));
        code
    }

    /// Use up `code`: true once, if it was minted here and has not expired.
    pub fn redeem(&self, code: &str) -> bool {
        self.redeem_at(code, Instant::now())
    }

    fn redeem_at(&self, code: &str, now: Instant) -> bool {
        let mut codes = self.0.lock();
        codes.retain(|(_, at)| now.saturating_duration_since(*at) < LOGIN_CODE_TTL);
        // Compare against every code (constant time each), then remove the match.
        let mut found = None;
        for (i, (c, _)) in codes.iter().enumerate() {
            if token_matches(code, c) {
                found = Some(i);
            }
        }
        found.map(|i| codes.remove(i)).is_some()
    }
}

/// Constant-time comparison of a presented token with ours.
pub fn token_matches(presented: &str, ours: &str) -> bool {
    let (a, b) = (presented.as_bytes(), ours.as_bytes());
    if a.len() != b.len() || ours.is_empty() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Host` names this machine's loopback interface.
pub fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
}

fn cookie_holds(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .filter(|(k, _)| *k == COOKIE || k.strip_prefix(COOKIE).is_some_and(|r| r.starts_with('_')))
        .any(|(_, v)| token_matches(v, token))
}

/// Why a request is refused, or None to let it through.
pub fn check_request(
    method: &Method,
    headers: &HeaderMap,
    token: &str,
) -> Option<(StatusCode, &'static str)> {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !is_loopback_host(host) {
        return Some((StatusCode::FORBIDDEN, "this API is for this machine only"));
    }
    if bearer(headers).is_some_and(|t| token_matches(t, token)) {
        return None;
    }
    if !cookie_holds(headers, token) {
        return Some((
            StatusCode::UNAUTHORIZED,
            "the control token is required (Authorization: Bearer, from .control.json)",
        ));
    }
    let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");
        if origin != format!("http://{host}") {
            return Some((StatusCode::FORBIDDEN, "cross-origin request refused"));
        }
    }
    None
}

fn json_error(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}

#[derive(Clone)]
struct AppState {
    control: Control,
    token: String,
    codes: LoginCodes,
    stop: CancellationToken,
}

async fn guard(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if let Some((status, why)) = check_request(req.method(), req.headers(), &st.token) {
        return json_error(status, why);
    }
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn control_error(e: ControlError) -> Response {
    let status = match e {
        ControlError::BadRequest(_) => StatusCode::BAD_REQUEST,
        ControlError::NotHere(_) | ControlError::NotManaged => StatusCode::CONFLICT,
    };
    json_error(status, &e.to_string())
}

async fn status(State(st): State<AppState>) -> Response {
    axum::Json(st.control.status()).into_response()
}

async fn pause(
    State(st): State<AppState>,
    body: Result<axum::Json<PauseBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let body = match body {
        Ok(b) => b.0,
        Err(e) => return json_error(StatusCode::BAD_REQUEST, &e.body_text()),
    };
    match st.control.pause(&body) {
        Ok(s) => axum::Json(s).into_response(),
        Err(e) => control_error(e),
    }
}

async fn resume(State(st): State<AppState>) -> Response {
    match st.control.resume() {
        Ok(s) => axum::Json(s).into_response(),
        Err(e) => control_error(e),
    }
}

async fn stop(State(st): State<AppState>) -> Response {
    match st.control.stop() {
        Ok(()) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({"stopping": true})),
        )
            .into_response(),
        Err(e) => control_error(e),
    }
}

/// `POST /control/ocr-install`: start (or retry) the background OCR backend install.
/// 202 `{installing: true|false, status}`; 409 when this instance has none.
async fn ocr_install(State(st): State<AppState>) -> Response {
    match st.control.start_install() {
        Ok(running) => (
            StatusCode::ACCEPTED,
            axum::Json(serde_json::json!({"installing": running, "status": st.control.status()})),
        )
            .into_response(),
        Err(e) => control_error(e),
    }
}

/// `status` events: one at once, then one per change (at most every
/// [`EVENTS_MIN_GAP`]), re-read every [`EVENTS_REFRESH`]; an unchanged status is not
/// sent twice. Ends when the listener stops.
fn status_stream(
    control: Control,
    stop: CancellationToken,
) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    struct S {
        control: Control,
        rx: tokio::sync::watch::Receiver<u64>,
        stop: CancellationToken,
        last: Option<String>,
        first: bool,
    }
    let rx = control.changes().subscribe();
    futures_util::stream::unfold(
        S {
            control,
            rx,
            stop,
            last: None,
            first: true,
        },
        |mut s| async move {
            loop {
                if !s.first {
                    tokio::select! {
                        _ = s.stop.cancelled() => return None,
                        changed = s.rx.changed() => if changed.is_err() { return None },
                        _ = tokio::time::sleep(EVENTS_REFRESH) => {}
                    }
                    // Coalesce a burst (page events) into one status.
                    tokio::select! {
                        _ = s.stop.cancelled() => return None,
                        _ = tokio::time::sleep(EVENTS_MIN_GAP) => {}
                    }
                }
                s.first = false;
                s.rx.borrow_and_update();
                let text = serde_json::to_string(&s.control.status()).unwrap_or_default();
                if s.last.as_deref() == Some(text.as_str()) {
                    continue;
                }
                s.last = Some(text.clone());
                return Some((Ok(SseEvent::default().event("status").data(text)), s));
            }
        },
    )
}

/// `POST /control/login-code`: a single-use code for `/app/login?c=`. Bearer only: a
/// page signed in with the cookie cannot mint codes.
async fn login_code(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !bearer(&headers).is_some_and(|t| token_matches(t, &st.token)) {
        return json_error(
            StatusCode::UNAUTHORIZED,
            "a sign-in code needs Authorization: Bearer (from .control.json)",
        );
    }
    axum::Json(serde_json::json!({
        "code": st.codes.mint(),
        "expires_in": LOGIN_CODE_TTL.as_secs(),
    }))
    .into_response()
}

async fn events(State(st): State<AppState>) -> Response {
    Sse::new(status_stream(st.control.clone(), st.stop.clone()))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

#[derive(Deserialize)]
pub struct LoginQuery {
    /// A single-use code from `POST /control/login-code`.
    pub c: Option<String>,
    pub next: Option<String>,
}

/// A page to go to after login: under `/app/`, never another host.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n)
            if n.starts_with("/app/")
                && !n.starts_with("//")
                && !n.contains('\\')
                && !n.contains("://") =>
        {
            n.to_string()
        }
        _ => "/app/".to_string(),
    }
}

/// `GET /app/login?c=<code>&next=/app/...`: the one-time link the tray / `gui` opens.
/// Uses up the code, sets the cookie (`bunko_control_<port>`, HttpOnly,
/// SameSite=Strict, Path=/; its value is the token) and redirects to `next`.
pub fn login(headers: &HeaderMap, query: &LoginQuery, token: &str, codes: &LoginCodes) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !is_loopback_host(host) {
        return (StatusCode::FORBIDDEN, "this page is for this machine only").into_response();
    }
    if !query.c.as_deref().is_some_and(|c| codes.redeem(c)) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            "<!DOCTYPE html><meta charset=utf-8><title>Mokuro Bunko</title>\
             <link rel=stylesheet href=\"/_static/shared.css\">\
             <body style=\"font-family:sans-serif;padding:2rem\"><h1>Mokuro Bunko</h1>\
             <p>This sign-in link is not valid (any more). Open the page again from the tray, \
             or run <code>mokuro-bunko gui</code>.</p></body>",
        )
            .into_response();
    }
    // Per port: cookies do not separate ports, and the tray may open the pages of two
    // instances (server and processor) in one browser.
    let port = host.rsplit_once(':').map(|(_, p)| p).unwrap_or("");
    let name = if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
        format!("{COOKIE}_{port}")
    } else {
        COOKIE.to_string()
    };
    let cookie = format!("{name}={token}; HttpOnly; SameSite=Strict; Path=/");
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, safe_next(query.next.as_deref()))
        .header(header::SET_COOKIE, cookie)
        .header(header::CACHE_CONTROL, "no-store")
        .header("Referrer-Policy", "no-referrer")
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn login_route(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LoginQuery>,
) -> Response {
    login(&headers, &q, &st.token, &st.codes)
}

/// The `/control` routes (and, without an app router, `/app/login`).
fn router(state: AppState, app: Option<Router>) -> Router {
    let control = Router::new()
        .route("/control/status", get(status))
        .route("/control/pause", post(pause))
        .route("/control/resume", post(resume))
        .route("/control/events", get(events))
        .route("/control/stop", post(stop))
        .route("/control/ocr-install", post(ocr_install))
        .route("/control/login-code", post(login_code))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state.clone());
    match app {
        Some(app) => control.merge(app),
        None => control.merge(
            Router::new()
                .route("/app/login", get(login_route))
                .with_state(state),
        ),
    }
}

/// A bound loopback port and its token, before anything is served or written: build
/// the app router with [`ControlListener::token`], then [`ControlListener::serve`].
pub struct ControlListener {
    listener: TcpListener,
    token: String,
    codes: LoginCodes,
    port: u16,
}

impl ControlListener {
    /// `127.0.0.1:0` — never any other address.
    pub async fn bind() -> std::io::Result<ControlListener> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await?;
        let port = listener.local_addr()?.port();
        Ok(ControlListener {
            listener,
            token: new_token(),
            codes: LoginCodes::default(),
            port,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// The sign-in codes this listener accepts (an app router answering
    /// `/app/login` takes a clone).
    pub fn login_codes(&self) -> LoginCodes {
        self.codes.clone()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Serve `/control` (+ `app`), write `.control.json`. The control's stop token, if
    /// any, is what `POST /control/stop` cancels (see [`Control::set_stop`]).
    ///
    /// Refused (`AlreadyExists`) while another live instance owns this storage's
    /// `.control.json`: its file is left alone. A `gui` (setup) instance's file is the
    /// exception: a server or processor starting on that storage takes it over (the
    /// setup app hands over to it).
    pub fn serve(self, control: Control, app: Option<Router>) -> std::io::Result<ControlServer> {
        let storage = control.storage().to_path_buf();
        if let Some(other) = crate::file::live_instance(&storage)
            && (other.role != Role::Gui || control.role() == Role::Gui)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "another mokuro-bunko {} (pid {}) already serves the control API for {}",
                    other.role.as_str(),
                    other.pid,
                    storage.display()
                ),
            ));
        }
        let url = self.url();
        let stop = CancellationToken::new();
        let file = ControlFile {
            role: control.role(),
            pid: std::process::id(),
            port: self.port,
            token: self.token.clone(),
            version: control.config().version.clone(),
            started_at: crate::pause::rfc3339(chrono::Utc::now()),
            url: url.clone(),
            managed: control.config().managed,
        };
        write_control_file(&storage, &file)?;
        let app = router(
            AppState {
                control,
                token: self.token.clone(),
                codes: self.codes.clone(),
                stop: stop.clone(),
            },
            app,
        );
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            let served = axum::serve(self.listener, app)
                .with_graceful_shutdown(async move { shutdown.cancelled().await })
                .await;
            if let Err(e) = served {
                tracing::warn!("the control listener failed: {e}");
            }
        });
        tracing::info!(
            "Control API on {url} (token in {})",
            storage.join(crate::types::CONTROL_FILE).display()
        );
        Ok(ControlServer {
            storage,
            token: self.token,
            codes: self.codes,
            port: self.port,
            url,
            stop,
            task: Some(task),
        })
    }
}

/// A serving control listener. [`ControlServer::shutdown`] (or dropping it) removes
/// `.control.json`.
pub struct ControlServer {
    storage: PathBuf,
    token: String,
    codes: LoginCodes,
    port: u16,
    url: String,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl ControlServer {
    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The browser's one-time sign-in link with a fresh code (`next` defaults to
    /// `/app/`).
    pub fn login_url(&self, next: Option<&str>) -> String {
        let mut url = format!("{}/app/login?c={}", self.url, self.codes.mint());
        if let Some(n) = next {
            url.push_str("&next=");
            url.push_str(&percent_encode(n));
        }
        url
    }

    /// Stop serving (open event streams end) and remove `.control.json`.
    pub async fn shutdown(mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let abort = task.abort_handle();
            if tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .is_err()
            {
                abort.abort();
            }
        }
        remove_control_file(&self.storage, &self.token);
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        self.stop.cancel();
        remove_control_file(&self.storage, &self.token);
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn auth_rules() {
        let t = "tok";
        let host = ("host", "127.0.0.1:4000");
        assert_eq!(
            check_request(&Method::GET, &headers(&[host]), t).map(|r| r.0),
            Some(StatusCode::UNAUTHORIZED)
        );
        assert!(
            check_request(
                &Method::POST,
                &headers(&[host, ("authorization", "Bearer tok")]),
                t
            )
            .is_none()
        );
        assert!(
            check_request(
                &Method::GET,
                &headers(&[host, ("authorization", "Bearer no")]),
                t
            )
            .is_some()
        );
        // Cookie, plain or per-port.
        assert!(
            check_request(
                &Method::GET,
                &headers(&[host, ("cookie", "x=1; bunko_control=tok")]),
                t
            )
            .is_none()
        );
        assert!(
            check_request(
                &Method::GET,
                &headers(&[host, ("cookie", "bunko_control_4000=tok")]),
                t
            )
            .is_none()
        );
        assert!(
            check_request(
                &Method::GET,
                &headers(&[host, ("cookie", "bunko_controlx=tok")]),
                t
            )
            .is_some()
        );
        // A cookie POST needs our own Origin.
        let cookie = ("cookie", "bunko_control=tok");
        assert_eq!(
            check_request(&Method::POST, &headers(&[host, cookie]), t).map(|r| r.0),
            Some(StatusCode::FORBIDDEN)
        );
        assert!(
            check_request(
                &Method::POST,
                &headers(&[host, cookie, ("origin", "http://127.0.0.1:4000")]),
                t
            )
            .is_none()
        );
        assert!(
            check_request(
                &Method::POST,
                &headers(&[host, cookie, ("origin", "http://127.0.0.1:4001")]),
                t
            )
            .is_some()
        );
        // DNS rebinding.
        assert_eq!(
            check_request(
                &Method::GET,
                &headers(&[
                    ("host", "evil.example:4000"),
                    ("authorization", "Bearer tok")
                ]),
                t
            )
            .map(|r| r.0),
            Some(StatusCode::FORBIDDEN)
        );
        assert!(!is_loopback_host("127.0.0.1.evil:1"));
        assert!(is_loopback_host("[::1]:1"));
        assert!(!token_matches("", ""));
        assert_eq!(safe_next(Some("//evil")), "/app/");
        assert_eq!(safe_next(Some("/app/dashboard")), "/app/dashboard");
    }

    #[test]
    fn login_codes_work_once_and_expire() {
        let codes = LoginCodes::default();
        let a = codes.mint();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!codes.redeem("0".repeat(32).as_str()));
        assert!(!codes.redeem(""));
        assert!(codes.redeem(&a));
        assert!(!codes.redeem(&a), "single use");

        // Expired: refused (and dropped).
        let b = codes.mint();
        let later = Instant::now() + LOGIN_CODE_TTL + Duration::from_secs(1);
        assert!(!codes.redeem_at(&b, later));
        assert!(!codes.redeem(&b));

        // Bounded: the oldest code goes first.
        let first = codes.mint();
        let rest: Vec<String> = (0..LOGIN_CODES_MAX).map(|_| codes.mint()).collect();
        assert_eq!(codes.0.lock().len(), LOGIN_CODES_MAX);
        assert!(!codes.redeem(&first));
        assert!(rest.iter().all(|c| codes.redeem(c)));
    }
}
