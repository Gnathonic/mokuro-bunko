//! cloudsim: one HTTP server that plays Google Drive v3, Microsoft Graph (OneDrive)
//! and MEGA for tests, with a control API for tokens, faults, seeding and accounting
//! (spec `docs/design/interop-testing.md` §6).
//!
//! Each provider lives under its own path prefix, so one port serves all three:
//!
//! - Drive: `{base}/google` (a driver's `api_base`; uploads under `{base}/google/upload`);
//! - OneDrive: `{base}/graph/v1.0`, with the pre-authenticated download, upload-session
//!   and copy-monitor URLs under `{preauth}/graph/`, a second listener (another
//!   origin, as Microsoft's storage hosts are);
//! - MEGA: the gateway `{base}/mega/` (`cs`, `sc`, wait and transfer URLs), real
//!   node crypto (the mock lives in `bunko-mega` and is mounted here);
//! - OAuth refresh: `{base}/oauth/google/token`, `{base}/oauth/microsoft/token`;
//! - the control API: `{base}/_sim/` ([`control`]).
//!
//! Every response carries permissive CORS, so a browser (or jsdom) can call it.
//!
//! State is per (provider, account). The library form ([`start`]) serves on
//! `127.0.0.1:0` inside a test's runtime; the binary serves the same [`router`].
//!
//! The simulator never panics on a request: a client mistake is a 4xx answer plus a
//! line in [`CloudSim::violations`], and dropping a [`CloudSim`] with violations left
//! fails the test.

pub mod accounts;
pub mod control;
pub mod drive;
pub mod log;
pub mod mega;
pub mod oauth;
pub mod onedrive;
pub mod violations;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use parking_lot::{Mutex, MutexGuard};
use serde::Serialize;
use sha2::{Digest, Sha256};

use accounts::TokenTable;
pub use accounts::{Provider, Tokens};
pub use drive::{DriveHandle, DriveState};
pub use log::LogEntry;
pub use mega::MegaHandle;
pub use onedrive::{OneDriveHandle, OneDriveState};
use violations::Violations;

/// A stored file, as `GET /_sim/files` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

impl FileInfo {
    fn of(path: String, data: &[u8]) -> FileInfo {
        FileInfo {
            path,
            size: data.len() as u64,
            sha256: hex::encode(Sha256::digest(data)),
        }
    }
}

/// Uploads in one request can be large (a whole volume in a simple upload).
const BODY_LIMIT: usize = 1 << 30;

pub(crate) fn rfc3339(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

type Accounts<T> = Mutex<HashMap<String, Arc<Mutex<T>>>>;

/// The account the fixed test token (`valid_token`) belongs to.
pub const DEFAULT_ACCOUNT: &str = "default";

/// An account state's fixed bearer token (the one the mock tests have always used).
trait FixedToken {
    fn fixed_token(&self) -> &str;
}

impl FixedToken for DriveState {
    fn fixed_token(&self) -> &str {
        &self.valid_token
    }
}

impl FixedToken for OneDriveState {
    fn fixed_token(&self) -> &str {
        &self.valid_token
    }
}

struct Inner {
    /// `http://host:port`, no trailing slash.
    base: String,
    /// Where OneDrive's pre-authenticated URLs point: a second listener (another
    /// origin), or `base` when there is none.
    preauth: String,
    drive: Accounts<DriveState>,
    onedrive: Accounts<OneDriveState>,
    tokens: Mutex<TokenTable>,
    mega: mega::MegaHub,
    log: log::Log,
    violations: Violations,
}

/// The simulator's state, shared by the router and every handle.
#[derive(Clone)]
pub struct SimState(Arc<Inner>);

fn account<T: Default>(map: &Accounts<T>, name: &str) -> Arc<Mutex<T>> {
    map.lock().entry(name.to_string()).or_default().clone()
}

impl SimState {
    /// State for a server reachable at `base` (`http://host:port`).
    pub fn new(base: impl Into<String>) -> Self {
        let base = base.into();
        SimState::with_preauth(base.clone(), base)
    }

    /// State for a server reachable at `base`, with a second listener at `preauth`
    /// (another origin) for OneDrive's pre-authenticated URLs.
    pub fn with_preauth(base: impl Into<String>, preauth: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        let preauth = preauth.into().trim_end_matches('/').to_string();
        SimState(Arc::new(Inner {
            preauth,
            mega: mega::MegaHub::new(format!("{base}{}/", mega::PREFIX)),
            base,
            drive: Mutex::default(),
            onedrive: Mutex::default(),
            tokens: Mutex::default(),
            log: log::Log::default(),
            violations: Violations::default(),
        }))
    }

    pub fn base(&self) -> &str {
        &self.0.base
    }

    /// Where OneDrive's pre-authenticated URLs point (see [`SimState::with_preauth`]).
    pub fn preauth_base(&self) -> &str {
        &self.0.preauth
    }

    pub(crate) fn violation(&self, line: String) {
        self.0.violations.push(line);
    }

    /// Pull MEGA's violations into the shared list.
    fn gather(&self) {
        for v in self.0.mega.take_violations() {
            self.0.violations.push(v);
        }
    }

    pub fn violations(&self) -> Vec<String> {
        self.gather();
        self.0.violations.list()
    }

    pub fn take_violations(&self) -> Vec<String> {
        self.gather();
        self.0.violations.take()
    }

    pub fn log(&self) -> &log::Log {
        &self.0.log
    }

    /// The MEGA account `name`, created empty (no password login) on first use.
    pub fn mega(&self, name: &str) -> MegaHandle {
        self.0.mega.account(name)
    }

    /// Create the folders along each path and put the bytes at its end.
    pub fn seed(&self, p: Provider, account: &str, files: &[(String, Vec<u8>)]) {
        for (path, data) in files {
            match p {
                Provider::Google => {
                    self.drive(account)
                        .lock()
                        .seed(path, Bytes::copy_from_slice(data));
                }
                Provider::Microsoft => {
                    self.onedrive(account)
                        .lock()
                        .seed(path, Bytes::copy_from_slice(data));
                }
                Provider::Mega => {
                    self.mega(account).seed(path, data);
                }
            }
        }
    }

    /// Every live file of the account, by path.
    pub fn files(&self, p: Provider, account: &str) -> Vec<FileInfo> {
        match p {
            Provider::Google => self
                .drive(account)
                .lock()
                .files()
                .into_iter()
                .map(|(path, d)| FileInfo::of(path, &d))
                .collect(),
            Provider::Microsoft => self
                .onedrive(account)
                .lock()
                .files()
                .into_iter()
                .map(|(path, d)| FileInfo::of(path, &d))
                .collect(),
            Provider::Mega => self
                .mega(account)
                .files()
                .into_iter()
                .map(|(path, d)| FileInfo::of(path, &d))
                .collect(),
        }
    }

    /// Forget every account, token, log entry and violation.
    pub fn reset(&self) {
        self.0.drive.lock().clear();
        self.0.onedrive.lock().clear();
        *self.0.tokens.lock() = TokenTable::default();
        self.0.mega.reset();
        self.0.log.clear();
        self.0.violations.take();
    }

    /// The Drive account `name`, created empty on first use.
    pub fn drive(&self, name: &str) -> DriveHandle {
        DriveHandle(account(&self.0.drive, name))
    }

    /// The OneDrive account `name`, created empty on first use.
    pub fn onedrive(&self, name: &str) -> OneDriveHandle {
        OneDriveHandle(account(&self.0.onedrive, name))
    }

    /// The tokens the simulator issued.
    pub fn tokens(&self) -> MutexGuard<'_, TokenTable> {
        self.0.tokens.lock()
    }

    /// The account a bearer token opens: an issued token while it is live (an
    /// expired, revoked or other provider's token opens nothing), else the `default`
    /// account when it is that account's fixed `valid_token`.
    fn for_token<T: Default + FixedToken>(
        &self,
        p: Provider,
        map: &Accounts<T>,
        token: &str,
    ) -> Option<(String, Arc<Mutex<T>>)> {
        let issued = {
            let t = self.0.tokens.lock();
            t.knows(token).then(|| t.resolve(p, token))
        };
        if let Some(name) = issued {
            let name = name?;
            let st = account(map, &name);
            return Some((name, st));
        }
        let st = map.lock().get(DEFAULT_ACCOUNT).cloned()?;
        let fixed = st.lock().fixed_token() == token;
        fixed.then(|| (DEFAULT_ACCOUNT.to_string(), st))
    }

    pub(crate) fn drive_for_token(&self, token: &str) -> Option<(String, Arc<Mutex<DriveState>>)> {
        self.for_token(Provider::Google, &self.0.drive, token)
    }

    pub(crate) fn onedrive_for_token(
        &self,
        token: &str,
    ) -> Option<(String, Arc<Mutex<OneDriveState>>)> {
        self.for_token(Provider::Microsoft, &self.0.onedrive, token)
    }

    pub(crate) fn drive_existing(&self, name: &str) -> Option<(String, Arc<Mutex<DriveState>>)> {
        let st = self.0.drive.lock().get(name).cloned()?;
        Some((name.to_string(), st))
    }

    pub(crate) fn onedrive_existing(
        &self,
        name: &str,
    ) -> Option<(String, Arc<Mutex<OneDriveState>>)> {
        let st = self.0.onedrive.lock().get(name).cloned()?;
        Some((name.to_string(), st))
    }
}

async fn unknown(
    axum::extract::State(sim): axum::extract::State<SimState>,
    req: Request,
) -> Response {
    sim.violation(format!(
        "{} {}: no such endpoint",
        req.method(),
        req.uri().path()
    ));
    (StatusCode::NOT_FOUND, "cloudsim: no such endpoint").into_response()
}

const EXPOSE: &str =
    "Location, ETag, Range, Content-Range, Retry-After, X-Hashcash, Content-Length";

/// Permissive CORS: the request's `Origin` (or `*`) is allowed every method and header
/// it asks for; preflights are answered here.
async fn cors(req: Request, next: Next) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .cloned()
        .unwrap_or(HeaderValue::from_static("*"));
    if req.method() == Method::OPTIONS
        && let Some(m) = req
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_METHOD)
            .cloned()
    {
        let hdrs = req
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .cloned()
            .unwrap_or(HeaderValue::from_static("*"));
        let mut r = StatusCode::NO_CONTENT.into_response();
        let h = r.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, m);
        h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, hdrs);
        h.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
        h.insert(header::VARY, HeaderValue::from_static("Origin"));
        return r;
    }
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSE),
    );
    h.append(header::VARY, HeaderValue::from_static("Origin"));
    r
}

/// Every provider and the control API on one router.
pub fn router(sim: SimState) -> Router {
    let mega = sim.0.mega.router();
    Router::new()
        .route("/google/{*rest}", any(drive::handle))
        .route("/graph/{*rest}", any(onedrive::handle))
        .route("/oauth/{provider}/token", post(oauth::token))
        .nest_service(mega::PREFIX, mega)
        .nest("/_sim", control::router())
        .fallback(unknown)
        .layer(from_fn_with_state(sim.clone(), log::record))
        .with_state(sim)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .layer(from_fn(cors))
}

/// A running simulator on `127.0.0.1`, served by tasks on the current runtime: the
/// main port, and a second one (another origin) that OneDrive's pre-authenticated URLs
/// point at.
pub struct CloudSim {
    /// `http://127.0.0.1:PORT`, no trailing slash.
    pub base: String,
    /// `http://127.0.0.1:PORT2`, the second listener.
    pub preauth_base: String,
    state: SimState,
    _tasks: [tokio::task::JoinHandle<()>; 2],
}

/// Start a simulator on two free local ports.
pub async fn start() -> CloudSim {
    let bind = || async {
        tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 127.0.0.1:0")
    };
    let (main, second) = (bind().await, bind().await);
    let base = format!("http://{}", main.local_addr().expect("local addr"));
    let preauth_base = format!("http://{}", second.local_addr().expect("local addr"));
    let state = SimState::with_preauth(base.clone(), preauth_base.clone());
    let serve = |l: tokio::net::TcpListener| {
        let app = router(state.clone());
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        })
    };
    let tasks = [serve(main), serve(second)];
    CloudSim {
        base,
        preauth_base,
        state,
        _tasks: tasks,
    }
}

impl CloudSim {
    pub fn state(&self) -> &SimState {
        &self.state
    }

    /// `GDriveConfig.api_base` for this simulator.
    pub fn drive_api_base(&self) -> String {
        format!("{}{}", self.base, drive::PREFIX)
    }

    /// `OneDriveConfig.api_base` for this simulator.
    pub fn graph_api_base(&self) -> String {
        format!("{}{}/v1.0", self.base, onedrive::PREFIX)
    }

    pub fn drive(&self, account: &str) -> DriveHandle {
        self.state.drive(account)
    }

    pub fn onedrive(&self, account: &str) -> OneDriveHandle {
        self.state.onedrive(account)
    }

    /// A new grant (access and refresh token) for `account`.
    pub fn token(&self, provider: Provider, account: &str) -> Tokens {
        self.state.tokens().issue(provider, account)
    }

    /// Kill the grant behind `token`: API calls 401, refresh answers `invalid_grant`.
    pub fn revoke(&self, token: &str) -> bool {
        self.state.tokens().revoke(token)
    }

    /// Expire an access token: API calls with it 401, its refresh token still works.
    pub fn expire(&self, token: &str) -> bool {
        self.state.tokens().expire(token)
    }

    /// `OAuthClient.token_url` for this simulator.
    pub fn token_url(&self, provider: Provider) -> String {
        format!("{}/oauth/{}/token", self.base, provider.as_str())
    }

    /// `ClientConfig.api_origin` (megajs `gateway`) for this simulator.
    pub fn mega_origin(&self) -> String {
        format!("{}{}/", self.base, mega::PREFIX)
    }

    /// The MEGA account `account`, created empty on first use.
    pub fn mega(&self, account: &str) -> MegaHandle {
        self.state.mega(account)
    }

    /// Enable password login for the MEGA account `account`.
    pub fn mega_account(&self, account: &str, email: &str, password: &str) {
        self.state.mega(account).set_account(email, password, None);
    }

    pub fn seed(&self, provider: Provider, account: &str, files: &[(String, Vec<u8>)]) {
        self.state.seed(provider, account, files);
    }

    pub fn files(&self, provider: Provider, account: &str) -> Vec<FileInfo> {
        self.state.files(provider, account)
    }

    pub fn log(&self) -> Vec<LogEntry> {
        self.state.log().list()
    }

    pub fn reset(&self) {
        self.state.reset();
    }

    /// Refreshes the token endpoints answered so far.
    pub fn refreshes(&self) -> usize {
        self.state.tokens().refreshes()
    }

    pub fn violations(&self) -> Vec<String> {
        self.state.violations()
    }

    /// The violations so far, cleared (for a test that provokes one on purpose).
    pub fn take_violations(&self) -> Vec<String> {
        self.state.take_violations()
    }

    /// Panics listing the violations, if there are any.
    pub fn assert_clean(&self) {
        let v = self.state.take_violations();
        assert!(v.is_empty(), "cloudsim violations:\n  {}", v.join("\n  "));
    }
}

impl Drop for CloudSim {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.assert_clean();
        }
    }
}
