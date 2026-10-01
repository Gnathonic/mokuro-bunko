//! Release checks and one-click updates (new in 0.7, ARCHITECTURE goal 4).
//!
//! * `GET /_admin/api/update[?refresh=1]` → the cached [`UpdateStatus`] plus
//!   `checks_enabled` and `applying`. A status older than [`CHECK_EVERY`] (or none) is
//!   re-checked on the spot when `update.check` is on; `refresh=1` always checks (the
//!   page's "Check now"). With checks off and no refresh, nothing is fetched.
//! * `POST /_admin/api/update/apply` → only when the status says `can_apply`: download,
//!   verify and install through [`UpdateSource::apply`], answer
//!   `{ok, success, version, restarting}`, then call the restart hook.
//!
//! [`UpdateService`] also checks in the background every 12 h while `update.check` is on,
//! and logs when a newer release appears.

use bunko_core::Config;
use bunko_update::{InstallKind, UpdateStatus};
use futures_util::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Background check period, and the age after which a cached status is re-checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
/// Delay before the first background check (keeps it off the startup path).
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);

/// Where release information comes from: [`bunko_update::Updater`] in the server, a fake
/// in tests.
pub trait UpdateSource: Send + Sync + 'static {
    /// Compare the latest release with this binary (never fails: errors go in `error`).
    fn check(&self) -> BoxFuture<'_, UpdateStatus>;
    /// Download, verify and install the latest release; returns the installed version.
    fn apply(&self) -> BoxFuture<'_, Result<String, String>>;
}

impl UpdateSource for bunko_update::Updater {
    fn check(&self) -> BoxFuture<'_, UpdateStatus> {
        Box::pin(bunko_update::Updater::check(self))
    }
    fn apply(&self) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            bunko_update::Updater::apply(self)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

#[derive(Clone)]
pub struct UpdateService {
    inner: Arc<Inner>,
}

struct Inner {
    source: Arc<dyn UpdateSource>,
    config: Arc<RwLock<Config>>,
    cached: Mutex<Option<(Instant, UpdateStatus)>>,
    /// One check at a time (a page refresh and the timer must not race).
    checking: tokio::sync::Mutex<()>,
    applying: AtomicBool,
    stop: Mutex<Option<CancellationToken>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl UpdateService {
    pub fn new(source: Arc<dyn UpdateSource>, config: Arc<RwLock<Config>>) -> Self {
        UpdateService {
            inner: Arc::new(Inner {
                source,
                config,
                cached: Mutex::new(None),
                checking: tokio::sync::Mutex::new(()),
                applying: AtomicBool::new(false),
                stop: Mutex::new(None),
                task: Mutex::new(None),
            }),
        }
    }

    /// The real updater for this build (`flavor` is `full` or `lite`), from `update.*`.
    pub fn from_config(config: Arc<RwLock<Config>>, flavor: &str) -> Self {
        let updater = {
            let c = config.read();
            bunko_update::Updater::new(
                c.update.manifest_url.clone(),
                c.update.channel.clone(),
                flavor,
            )
        };
        Self::new(Arc::new(updater), config)
    }

    pub fn checks_enabled(&self) -> bool {
        self.inner.config.read().update.check
    }

    pub fn is_applying(&self) -> bool {
        self.inner.applying.load(Ordering::SeqCst)
    }

    pub fn cached(&self) -> Option<UpdateStatus> {
        self.inner.cached.lock().as_ref().map(|(_, s)| s.clone())
    }

    /// Check now and cache the result.
    pub async fn check_now(&self) -> UpdateStatus {
        let _one = self.inner.checking.lock().await;
        let status = self.inner.source.check().await;
        if status.available {
            info!(
                "mokuro-bunko {} is available (running {})",
                status.latest.as_deref().unwrap_or("?"),
                status.current
            );
        } else if let Some(e) = &status.error {
            warn!("update check failed: {e}");
        }
        *self.inner.cached.lock() = Some((Instant::now(), status.clone()));
        status
    }

    /// The status the page shows: cached while fresh, re-checked when stale (if checks
    /// are on) or when `refresh` is asked.
    pub async fn status(&self, refresh: bool) -> UpdateStatus {
        let cached = self.inner.cached.lock().clone();
        let fresh = cached
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < CHECK_EVERY);
        if refresh || (!fresh && self.checks_enabled()) {
            return self.check_now().await;
        }
        cached.map(|(_, s)| s).unwrap_or_else(unchecked_status)
    }

    /// Install the latest release. `Err((status, message))` when it may not or failed.
    pub async fn apply(&self) -> Result<(String, String), (u16, String)> {
        if self.inner.applying.swap(true, Ordering::SeqCst) {
            return Err((409, "An update is already being applied".into()));
        }
        let result = self.apply_inner().await;
        if result.is_err() {
            self.inner.applying.store(false, Ordering::SeqCst);
        }
        result
    }

    async fn apply_inner(&self) -> Result<(String, String), (u16, String)> {
        let status = self.status(false).await;
        let status = if status.checked_at.is_none() {
            self.check_now().await
        } else {
            status
        };
        if !status.available {
            return Err((409, "No newer release is available".into()));
        }
        if !status.can_apply {
            return Err((409, cannot_apply_reason(&status)));
        }
        let from = status.current.clone();
        let version = self
            .inner
            .source
            .apply()
            .await
            .map_err(|e| (500, format!("The update failed: {e}")))?;
        info!("installed mokuro-bunko {version} (was {from})");
        Ok((from, version))
    }

    /// Check every [`CHECK_EVERY`] while `update.check` is on, until `stop` fires.
    pub fn start(&self, stop: CancellationToken) {
        let child = stop.child_token();
        *self.inner.stop.lock() = Some(child.clone());
        let me = self.clone();
        let handle = tokio::spawn(async move {
            let mut wait = FIRST_CHECK_AFTER;
            loop {
                tokio::select! {
                    _ = child.cancelled() => break,
                    _ = tokio::time::sleep(wait) => {}
                }
                if me.checks_enabled() {
                    me.check_now().await;
                }
                wait = CHECK_EVERY;
            }
        });
        *self.inner.task.lock() = Some(handle);
    }

    pub async fn stop(&self) {
        if let Some(t) = self.inner.stop.lock().take() {
            t.cancel();
        }
        let task = self.inner.task.lock().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

/// What the page shows before any check (no network contact).
fn unchecked_status() -> UpdateStatus {
    UpdateStatus {
        current: bunko_core::VERSION.into(),
        latest: None,
        available: false,
        notes_url: None,
        install: InstallKind::detect(),
        can_apply: false,
        docker_image: None,
        checked_at: None,
        error: None,
    }
}

/// Why an available update cannot be applied from the panel.
pub fn cannot_apply_reason(status: &UpdateStatus) -> String {
    match &status.install {
        InstallKind::Docker => match &status.docker_image {
            Some(image) => {
                format!("This server runs in Docker: pull {image} and recreate the container")
            }
            None => {
                "This server runs in Docker: pull the new image and recreate the container".into()
            }
        },
        InstallKind::Managed { by } => {
            format!("This installation is managed by {by}; update it there")
        }
        InstallKind::Mobile => "Update the app through its store".into(),
        InstallKind::SelfManaged { .. } => {
            "There is no download of this release for this platform".into()
        }
    }
}

pub(super) mod http {
    use super::UpdateStatus;
    use crate::admin::{
        AdminState, ApiRequest, blocking, error, not_found, ok, parse_qs, query_one,
    };
    use axum::response::Response;
    use bunko_db::{AuditDetails, NewAuditEvent};
    use serde_json::{Value, json};
    use std::time::Duration;

    /// How long the restart waits so the apply response reaches the browser first.
    const RESTART_DELAY: Duration = Duration::from_millis(750);

    fn body(status: &UpdateStatus, checks_enabled: bool, applying: bool) -> Value {
        let mut v = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
        if let Value::Object(m) = &mut v {
            m.insert("checks_enabled".into(), json!(checks_enabled));
            m.insert("applying".into(), json!(applying));
            if status.available && !status.can_apply {
                m.insert(
                    "cannot_apply_reason".into(),
                    json!(super::cannot_apply_reason(status)),
                );
            }
        }
        v
    }

    pub async fn get(s: &AdminState, req: &ApiRequest) -> Response {
        let Some(updates) = &s.deps.updates else {
            return not_found();
        };
        let q = parse_qs(&req.query);
        let refresh = query_one(&q, "refresh")
            .is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"));
        let status = updates.status(refresh).await;
        ok(body(
            &status,
            updates.checks_enabled(),
            updates.is_applying(),
        ))
    }

    pub async fn apply(s: &AdminState, req: &ApiRequest) -> Response {
        let Some(updates) = &s.deps.updates else {
            return not_found();
        };
        let (from, version) = match updates.apply().await {
            Ok(v) => v,
            Err((status, msg)) => return error(status, msg),
        };
        let db = s.db();
        let actor = req.actor();
        let (f, v) = (from.clone(), version.clone());
        // An audit failure must not hide that the binary was replaced.
        if let Ok(Err(e)) = blocking(move || {
            db.log_audit_event(
                &NewAuditEvent::new("admin_update_server")
                    .actor(actor.as_deref())
                    .target_type("server")
                    .details(AuditDetails::new().with("from", f).with("to", v)),
            )
        })
        .await
        {
            tracing::warn!("could not audit the update: {e}");
        }
        let restarting = match &s.deps.restart {
            Some(restart) => {
                let restart = restart.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(RESTART_DELAY).await;
                    restart();
                });
                true
            }
            None => false,
        };
        ok(
            json!({"ok": true, "success": true, "version": version, "previous": from, "restarting": restarting}),
        )
    }
}
