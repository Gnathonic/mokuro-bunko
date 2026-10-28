//! cloudsim: one HTTP server that plays Google Drive v3, Microsoft Graph (OneDrive)
//! and MEGA for tests, with a control API for tokens, faults, seeding and accounting
//! (spec `docs/design/interop-testing.md` §6).
//!
//! Each provider lives under its own path prefix, so one port serves all three:
//!
//! - Drive: `{base}/google` (a driver's `api_base`; uploads under `{base}/google/upload`);
//! - OneDrive: `{base}/graph/v1.0`, with the pre-authenticated download, upload-session
//!   and copy-monitor URLs under `{base}/graph/`;
//!
//! State is per (provider, account). The library form ([`start`]) serves on
//! `127.0.0.1:0` inside a test's runtime; the binary serves the same [`router`].
//!
//! The simulator never panics on a request: a client mistake is a 4xx answer plus a
//! line in [`CloudSim::violations`], and dropping a [`CloudSim`] with violations left
//! fails the test.

pub mod drive;
pub mod onedrive;
pub mod violations;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use parking_lot::Mutex;

pub use drive::{DriveHandle, DriveState};
pub use onedrive::{OneDriveHandle, OneDriveState};
use violations::Violations;

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

struct Inner {
    /// `http://host:port`, no trailing slash.
    base: String,
    drive: Accounts<DriveState>,
    onedrive: Accounts<OneDriveState>,
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
        SimState(Arc::new(Inner {
            base: base.into().trim_end_matches('/').to_string(),
            drive: Mutex::default(),
            onedrive: Mutex::default(),
            violations: Violations::default(),
        }))
    }

    pub fn base(&self) -> &str {
        &self.0.base
    }

    pub(crate) fn violation(&self, line: String) {
        self.0.violations.push(line);
    }

    pub fn violations(&self) -> Vec<String> {
        self.0.violations.list()
    }

    pub fn take_violations(&self) -> Vec<String> {
        self.0.violations.take()
    }

    /// The Drive account `name`, created empty on first use.
    pub fn drive(&self, name: &str) -> DriveHandle {
        DriveHandle(account(&self.0.drive, name))
    }

    /// The OneDrive account `name`, created empty on first use.
    pub fn onedrive(&self, name: &str) -> OneDriveHandle {
        OneDriveHandle(account(&self.0.onedrive, name))
    }

    pub(crate) fn drive_for_token(&self, token: &str) -> Option<(String, Arc<Mutex<DriveState>>)> {
        let accounts: Vec<_> = self
            .0
            .drive
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        accounts
            .into_iter()
            .find(|(_, st)| st.lock().valid_token == token)
    }

    pub(crate) fn onedrive_for_token(
        &self,
        token: &str,
    ) -> Option<(String, Arc<Mutex<OneDriveState>>)> {
        let accounts: Vec<_> = self
            .0
            .onedrive
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        accounts
            .into_iter()
            .find(|(_, st)| st.lock().valid_token == token)
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

/// Every provider and the control API on one router.
pub fn router(sim: SimState) -> Router {
    Router::new()
        .route("/google/{*rest}", any(drive::handle))
        .route("/graph/{*rest}", any(onedrive::handle))
        .fallback(unknown)
        .with_state(sim)
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
}

/// A running simulator on `127.0.0.1:{port}`, served by a task on the current runtime.
pub struct CloudSim {
    /// `http://127.0.0.1:PORT`, no trailing slash.
    pub base: String,
    state: SimState,
    _task: tokio::task::JoinHandle<()>,
}

/// Start a simulator on a free local port.
pub async fn start() -> CloudSim {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let base = format!("http://{}", listener.local_addr().expect("local addr"));
    let state = SimState::new(base.clone());
    let app = router(state.clone());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    CloudSim {
        base,
        state,
        _task: task,
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
