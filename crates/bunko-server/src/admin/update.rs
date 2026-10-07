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
//! * `POST /_admin/api/update/settings` → `{auto, check}`: turn automatic updates
//!   (`update.auto`) and background checks on or off; saved to the config file.
//!
//! [`UpdateService`] also checks in the background every 12 h while `update.check` (or
//! `update.auto`) is on, and logs when a newer release appears. With `update.auto` on
//! and a self-managed install it installs a newer release by itself
//! ([`UpdateService::try_auto`]): new OCR claims stop everywhere, the claims in flight
//! and the uploads finish, the [`ReleaseInstaller`] fetches and switches the release
//! (binary + backend pack + models), and the restart hook restarts the server. Docker
//! and package-managed installs only report (a `fail` problem: the owner must act).

use bunko_control::{Problem, UpdateView};
use bunko_core::Config;
use bunko_update::auto::{Blocked, InstallFailure, ReleaseInstaller, Retry};
use bunko_update::{InstallKind, UpdateStatus};
use futures_util::future::BoxFuture;
use parking_lot::{Mutex, RwLock};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// Background check period, and the age after which a cached status is re-checked.
pub const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
/// Delay before the first background check (keeps it off the startup path).
pub const FIRST_CHECK_AFTER: Duration = Duration::from_secs(60);
/// How often a waiting automatic update looks for its quiet moment.
const QUIET_POLL: Duration = Duration::from_secs(5);

/// `MOKURO_UPDATE_CHECK_SECONDS`: the background check period (and at most the first
/// delay), for tests and mirrors that publish often. Default 12 h (first after 60 s).
pub fn check_period() -> (Duration, Duration) {
    match std::env::var("MOKURO_UPDATE_CHECK_SECONDS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
    {
        Some(s) => {
            let d = Duration::from_secs(s);
            (d.min(FIRST_CHECK_AFTER), d)
        }
        None => (FIRST_CHECK_AFTER, CHECK_EVERY),
    }
}

/// What the automatic update needs to know about the rest of the server.
pub trait Quiet: Send + Sync + 'static {
    /// Stop (`true`) or allow again (`false`) new OCR claims on every machine.
    fn drain(&self, on: bool);
    /// What a restart would break right now (OCR claims in flight, uploads), or None.
    fn busy(&self) -> BoxFuture<'_, Option<String>>;
}

/// Wired by `serve_router` / the binary: without it `update.auto` only reports.
pub struct AutoDeps {
    pub installer: Arc<dyn ReleaseInstaller>,
    pub quiet: Arc<dyn Quiet>,
    /// Restart the server gracefully into the installed release.
    pub restart: Arc<dyn Fn() + Send + Sync>,
    /// Where `.update-blocked.json` lives (the storage base path).
    pub storage: PathBuf,
}

/// Receives every change of the automatic update's state (the control API's
/// `status.update` and its problems).
pub type Reporter = Arc<dyn Fn(Option<UpdateView>, Vec<Problem>) + Send + Sync>;

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
    auto: Mutex<Option<Arc<AutoDeps>>>,
    /// The automatic update as the status shows it, and the problems it raised.
    view: Mutex<(Option<UpdateView>, Vec<Problem>)>,
    reporter: Mutex<Option<Reporter>>,
    retry: Mutex<Retry>,
    /// "Check now" in the panel: look at the automatic update again at once.
    kick: tokio::sync::Notify,
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
                auto: Mutex::new(None),
                view: Mutex::new((None, Vec::new())),
                reporter: Mutex::new(None),
                retry: Mutex::new(Retry::default()),
                kick: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Let `update.auto` install (without this it only reports).
    pub fn set_auto(&self, deps: AutoDeps) {
        *self.inner.auto.lock() = Some(Arc::new(deps));
    }

    /// Where every change of the automatic update's state goes (the control API).
    pub fn set_reporter(&self, reporter: Reporter) {
        let (view, problems) = self.inner.view.lock().clone();
        reporter(view, problems);
        *self.inner.reporter.lock() = Some(reporter);
    }

    pub fn auto_enabled(&self) -> bool {
        self.inner.config.read().update.auto
    }

    /// The automatic update's state and problems.
    pub fn auto_view(&self) -> (Option<UpdateView>, Vec<Problem>) {
        self.inner.view.lock().clone()
    }

    /// Set what the status shows about the automatic update (also used at start to say
    /// "Updated to X" or "rolled back").
    pub fn set_view(&self, view: Option<UpdateView>, problems: Vec<Problem>) {
        {
            let mut v = self.inner.view.lock();
            if v.0 == view && v.1 == problems {
                return;
            }
            *v = (view.clone(), problems.clone());
        }
        let reporter = self.inner.reporter.lock().clone();
        if let Some(r) = reporter {
            r(view, problems);
        }
    }

    fn set_state(
        &self,
        state: &str,
        version: Option<&str>,
        message: Option<String>,
        problems: Vec<Problem>,
    ) {
        let old = self.inner.view.lock().0.clone();
        let same_state = old.as_ref().is_some_and(|o| o.state == state);
        let view = UpdateView {
            state: state.into(),
            version: version.map(str::to_string),
            from: (state == "updated")
                .then(|| old.as_ref().and_then(|o| o.from.clone()))
                .flatten(),
            message,
            auto: self.auto_enabled(),
            since: if same_state {
                old.and_then(|o| o.since)
            } else {
                Some(bunko_update::auto::now_rfc3339())
            },
        };
        self.set_view(Some(view), problems);
    }

    /// The real updater for this build (`flavor` is `full` or `lite`), from `update.*`.
    pub fn from_config(config: Arc<RwLock<Config>>, flavor: &str) -> Self {
        let updater = {
            let c = config.read();
            let (key, custom) = bunko_update::auto::release_key(&c.update.public_key);
            if custom {
                warn!(
                    "{}",
                    bunko_update::auto::custom_key_warning(
                        &key,
                        "update.public_key in the config file"
                    )
                );
            }
            bunko_update::Updater::new(
                c.update.manifest_url.clone(),
                c.update.channel.clone(),
                flavor,
            )
            .with_public_key(key)
        };
        Self::new(Arc::new(updater), config)
    }

    pub fn checks_enabled(&self) -> bool {
        self.inner.config.read().update.check
    }

    /// Re-evaluate the automatic update now (a setting changed, "Check now").
    pub fn kick(&self) {
        self.inner.kick.notify_one();
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
            let status = self.check_now().await;
            if refresh {
                // The automatic update looks at the new result at once.
                self.inner.kick.notify_one();
            }
            return status;
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
        // With the binary's installer (the release as one unit: binary, backend pack,
        // models), the same way the automatic update does; else the plain updater.
        let installer = self.inner.auto.lock().as_ref().map(|d| d.installer.clone());
        let version = match (installer, status.latest.clone()) {
            (Some(i), Some(latest)) => {
                let v = i
                    .install(latest)
                    .await
                    .map_err(|e| (500, format!("The update failed: {e}")))?;
                if let Some(deps) = self.inner.auto.lock().as_ref() {
                    Blocked::clear(&deps.storage);
                }
                v
            }
            _ => self
                .inner
                .source
                .apply()
                .await
                .map_err(|e| (500, format!("The update failed: {e}")))?,
        };
        info!("installed mokuro-bunko {version} (was {from})");
        Ok((from, version))
    }

    /// Check every [`CHECK_EVERY`] while `update.check` or `update.auto` is on, until
    /// `stop` fires; with `update.auto`, install what the check found
    /// ([`UpdateService::try_auto`]).
    pub fn start(&self, stop: CancellationToken) {
        let child = stop.child_token();
        *self.inner.stop.lock() = Some(child.clone());
        let me = self.clone();
        let handle = tokio::spawn(async move {
            let (first, period) = check_period();
            let mut wait = first;
            loop {
                let kicked = tokio::select! {
                    _ = child.cancelled() => break,
                    _ = tokio::time::sleep(wait) => false,
                    _ = me.inner.kick.notified() => true,
                };
                let status = if kicked {
                    me.cached().unwrap_or_else(unchecked_status)
                } else if me.checks_enabled() || me.auto_enabled() {
                    me.check_now().await
                } else {
                    wait = period;
                    continue;
                };
                me.try_auto(&status, &child).await;
                // A failed automatic update comes back when its retry is due.
                let retry_in =
                    me.inner.retry.lock().next_try().map(|t| {
                        t.saturating_duration_since(Instant::now()) + Duration::from_secs(1)
                    });
                wait = match retry_in {
                    Some(r) if me.auto_enabled() => r.min(period),
                    _ => period,
                };
            }
        });
        *self.inner.task.lock() = Some(handle);
    }

    /// With `update.auto` on: act on a check's result. A newer release on the channel is
    /// installed at a quiet moment (no OCR claim and no upload in flight; new claims are
    /// stopped meanwhile), then the server restarts. Docker and managed installs, a
    /// blocked (rolled back) version and a failing install only report.
    pub async fn try_auto(&self, status: &UpdateStatus, stop: &CancellationToken) {
        if !self.auto_enabled() {
            // Off: nothing is installed. "Updated"/"rolled back" from the start stays.
            let keep = self
                .inner
                .view
                .lock()
                .0
                .as_ref()
                .is_some_and(|v| matches!(v.state.as_str(), "updated" | "blocked"));
            if !keep {
                let state = if status.available {
                    "available"
                } else {
                    "idle"
                };
                self.set_state(state, status.latest.as_deref(), None, Vec::new());
            }
            return;
        }
        if let Some(e) = &status.error {
            let failures = {
                let mut r = self.inner.retry.lock();
                r.failed("check", e, Instant::now());
                r.failures()
            };
            let problems = if failures >= bunko_update::auto::RETRY_TELL_AFTER {
                vec![Problem::update_needs_you(
                    format!(
                        "Automatic updates: the release server has not answered {failures} times in a row: {e}"
                    ),
                    "Check the network, or update.manifest_url in config.yaml (a mirror).",
                )]
            } else {
                Vec::new()
            };
            self.set_state(
                "failed",
                None,
                Some(format!("could not check for updates: {e}")),
                problems,
            );
            return;
        }
        let Some(latest) = status.latest.clone().filter(|_| status.available) else {
            let keep_updated = self
                .inner
                .view
                .lock()
                .0
                .as_ref()
                .is_some_and(|v| matches!(v.state.as_str(), "updated" | "blocked"));
            if !keep_updated {
                self.set_state("idle", None, None, Vec::new());
            }
            return;
        };
        if !status.can_apply {
            let reason = cannot_apply_reason(status);
            info!(
                "mokuro-bunko {latest} is available; automatic update not possible here: {reason}"
            );
            self.set_state(
                "blocked",
                Some(&latest),
                Some(reason.clone()),
                vec![Problem::update_needs_you(
                    format!(
                        "mokuro-bunko {latest} is available, but this install cannot update itself"
                    ),
                    reason,
                )],
            );
            return;
        }
        let Some(deps) = self.inner.auto.lock().clone() else {
            self.set_state("available", Some(&latest), None, Vec::new());
            return;
        };
        if let Some(b) = Blocked::read(&deps.storage).filter(|b| b.blocks(&latest)) {
            self.set_state(
                "blocked",
                Some(&latest),
                Some(b.reason.clone()),
                vec![Problem::update_needs_you(
                    format!("The update to {latest} was rolled back: {}", b.reason),
                    format!(
                        "This machine stays on {}. Fix the cause, then install it by hand (admin panel → Updates, or 'mokuro-bunko update apply'); automatic updates skip {latest} until a newer release.",
                        bunko_core::VERSION
                    ),
                )],
            );
            return;
        }
        if !self.inner.retry.lock().may_try(&latest, Instant::now()) {
            return;
        }
        if self.inner.applying.swap(true, Ordering::SeqCst) {
            return;
        }
        let installed = self.auto_install(&latest, &deps, stop).await;
        if !installed {
            self.inner.applying.store(false, Ordering::SeqCst);
        }
    }

    /// Drain, install, restart. Returns whether the restart was asked for.
    async fn auto_install(&self, latest: &str, deps: &AutoDeps, stop: &CancellationToken) -> bool {
        info!(
            "Automatic update: mokuro-bunko {latest} found; installing it once nothing is running"
        );
        self.set_state(
            "waiting",
            Some(latest),
            Some(
                "new OCR work is held; installing once the running volumes and uploads finish"
                    .into(),
            ),
            Vec::new(),
        );
        deps.quiet.drain(true);
        let mut said = String::new();
        loop {
            if stop.is_cancelled() || !self.auto_enabled() {
                deps.quiet.drain(false);
                info!("Automatic update to {latest}: called off");
                self.set_state("available", Some(latest), None, Vec::new());
                return false;
            }
            match deps.quiet.busy().await {
                None => break,
                Some(what) => {
                    if what != said {
                        info!("Automatic update to {latest}: waiting for {what}");
                        self.set_state(
                            "waiting",
                            Some(latest),
                            Some(format!("waiting for {what}")),
                            Vec::new(),
                        );
                        said = what;
                    }
                }
            }
            tokio::select! {
                _ = stop.cancelled() => {}
                _ = tokio::time::sleep(QUIET_POLL) => {}
            }
        }
        info!("Automatic update: quiet; installing mokuro-bunko {latest}");
        self.set_state(
            "installing",
            Some(latest),
            Some("downloading and checking the release".into()),
            Vec::new(),
        );
        match deps.installer.install(latest.to_string()).await {
            Ok(version) => {
                self.inner.retry.lock().succeeded();
                info!(
                    "Automatic update: installed mokuro-bunko {version} (was {}); restarting",
                    bunko_core::VERSION
                );
                self.set_state(
                    "restarting",
                    Some(&version),
                    Some(format!("restarting into {version}")),
                    Vec::new(),
                );
                // An upload that began meanwhile finishes first (bounded).
                for _ in 0..12 {
                    if deps.quiet.busy().await.is_none() {
                        break;
                    }
                    tokio::time::sleep(QUIET_POLL).await;
                }
                (deps.restart)();
                true
            }
            Err(f) => {
                deps.quiet.drain(false);
                self.install_failed(latest, &f);
                false
            }
        }
    }

    fn install_failed(&self, latest: &str, f: &InstallFailure) {
        let (wait, keeps) = {
            let mut r = self.inner.retry.lock();
            let wait = r.failed(latest, &f.message, Instant::now());
            (wait, r.keeps_failing())
        };
        error!(
            "Automatic update to {latest} failed: {}; still running {}, trying again in {} min",
            f.message,
            bunko_core::VERSION,
            wait.as_secs().div_ceil(60)
        );
        let problems = if f.needs_owner {
            vec![Problem::update_needs_you(
                format!("The automatic update to {latest} needs you: {}", f.message),
                f.action
                    .clone()
                    .unwrap_or_else(|| "See the server log.".into()),
            )]
        } else if keeps {
            vec![Problem::update_needs_you(
                format!(
                    "The automatic update to {latest} keeps failing: {}",
                    f.message
                ),
                "See the server log; 'mokuro-bunko update apply' shows the error too.",
            )]
        } else {
            vec![Problem::update_warning(
                format!(
                    "The automatic update to {latest} failed (trying again later): {}",
                    f.message
                ),
                None,
            )]
        };
        self.set_state(
            if f.needs_owner { "blocked" } else { "failed" },
            Some(latest),
            Some(f.message.clone()),
            problems,
        );
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
        note: None,
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

    fn body(status: &UpdateStatus, updates: &super::UpdateService) -> Value {
        let mut v = serde_json::to_value(status).unwrap_or_else(|_| json!({}));
        if let Value::Object(m) = &mut v {
            m.insert("checks_enabled".into(), json!(updates.checks_enabled()));
            m.insert("applying".into(), json!(updates.is_applying()));
            // 0.7: automatic updates (`update.auto`) and where one stands.
            let (view, problems) = updates.auto_view();
            m.insert("auto".into(), json!(updates.auto_enabled()));
            m.insert("auto_state".into(), json!(view));
            m.insert("problems".into(), json!(problems));
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
        ok(body(&status, updates))
    }

    /// `POST /update/settings {auto?, check?}`: saved to the config file.
    pub async fn settings(s: &AdminState, req: &ApiRequest) -> Response {
        let Some(updates) = s.deps.updates.clone() else {
            return not_found();
        };
        let data = match req.json() {
            Ok(d) => d.clone(),
            Err(r) => return r,
        };
        let s2 = s.clone();
        let saved = blocking(move || {
            let _guard = s2.config_lock.lock();
            {
                let mut cfg = s2.core().config.write();
                if let Some(v) = data.get("auto") {
                    cfg.update.auto = bunko_db::pyfmt::truthy(v);
                }
                if let Some(v) = data.get("check") {
                    cfg.update.check = bunko_db::pyfmt::truthy(v);
                }
            }
            crate::admin::save_config(&s2)
        })
        .await;
        match saved {
            Ok(Ok(())) => {}
            Ok(Err(r)) | Err(r) => return r,
        }
        let (auto, check) = (updates.auto_enabled(), updates.checks_enabled());
        tracing::info!(
            "Updates: automatic install {}, background checks {} (admin panel)",
            if auto { "on" } else { "off" },
            if check { "on" } else { "off" }
        );
        // Look again now (turning auto on acts on the last check at once).
        updates.kick();
        ok(json!({"success": true, "auto": auto, "check": check}))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct Source {
        install: InstallKind,
        latest: &'static str,
    }

    impl UpdateSource for Source {
        fn check(&self) -> BoxFuture<'_, UpdateStatus> {
            Box::pin(async move {
                let available = bunko_update::auto::is_newer(self.latest, bunko_core::VERSION);
                UpdateStatus {
                    current: bunko_core::VERSION.into(),
                    latest: Some(self.latest.into()),
                    available,
                    notes_url: None,
                    can_apply: available && self.install.can_apply(),
                    install: self.install.clone(),
                    docker_image: None,
                    checked_at: Some("now".into()),
                    error: None,
                    note: None,
                }
            })
        }
        fn apply(&self) -> BoxFuture<'_, Result<String, String>> {
            Box::pin(async { Err("not this way".into()) })
        }
    }

    #[derive(Default)]
    struct Log(Mutex<Vec<String>>);

    impl Log {
        fn push(&self, s: impl Into<String>) {
            self.0.lock().push(s.into());
        }
        fn get(&self) -> Vec<String> {
            self.0.lock().clone()
        }
    }

    struct Installer {
        log: Arc<Log>,
        fail: bool,
    }

    impl ReleaseInstaller for Installer {
        fn install(&self, version: String) -> BoxFuture<'static, Result<String, InstallFailure>> {
            self.log.push(format!("install {version}"));
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(InstallFailure::retry("the download failed"))
                } else {
                    Ok(version)
                }
            })
        }
    }

    /// Busy for the first `busy` looks, then quiet.
    struct FakeQuiet {
        log: Arc<Log>,
        busy: AtomicUsize,
    }

    impl Quiet for FakeQuiet {
        fn drain(&self, on: bool) {
            self.log.push(format!("drain {on}"));
        }
        fn busy(&self) -> BoxFuture<'_, Option<String>> {
            let left = self.busy.load(Ordering::SeqCst);
            if left > 0 {
                self.busy.fetch_sub(1, Ordering::SeqCst);
                self.log.push("busy");
            }
            Box::pin(
                async move { (left > 0).then(|| "1 OCR volume(s) in flight (tower)".to_string()) },
            )
        }
    }

    fn service(
        auto: bool,
        install: InstallKind,
        fail: bool,
        busy: usize,
    ) -> (UpdateService, Arc<Log>, tempfile::TempDir) {
        let mut config = Config::default();
        config.update.auto = auto;
        let svc = UpdateService::new(
            Arc::new(Source {
                install,
                latest: "99.0.0",
            }),
            Arc::new(RwLock::new(config)),
        );
        let log = Arc::new(Log::default());
        let dir = tempfile::tempdir().unwrap();
        let l = log.clone();
        svc.set_auto(AutoDeps {
            installer: Arc::new(Installer {
                log: log.clone(),
                fail,
            }),
            quiet: Arc::new(FakeQuiet {
                log: log.clone(),
                busy: AtomicUsize::new(busy),
            }),
            restart: Arc::new(move || l.push("restart")),
            storage: dir.path().to_path_buf(),
        });
        (svc, log, dir)
    }

    fn self_managed() -> InstallKind {
        InstallKind::SelfManaged {
            exe: "/opt/bunko/mokuro-bunko".into(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn auto_off_installs_nothing() {
        let (svc, log, _d) = service(false, self_managed(), false, 0);
        let status = svc.check_now().await;
        svc.try_auto(&status, &CancellationToken::new()).await;
        assert!(log.get().is_empty(), "{:?}", log.get());
        assert_eq!(svc.auto_view().0.unwrap().state, "available");
    }

    /// Nothing published yet is not a failed check: no backoff, no problem for the owner.
    #[tokio::test(start_paused = true)]
    async fn nothing_published_is_not_a_failure() {
        let (svc, log, _d) = service(true, self_managed(), false, 0);
        let status = UpdateStatus {
            install: self_managed(),
            note: Some("No release has been published yet.".into()),
            ..unchecked_status()
        };
        for _ in 0..5 {
            svc.try_auto(&status, &CancellationToken::new()).await;
        }
        assert!(log.get().is_empty(), "{:?}", log.get());
        let (view, problems) = svc.auto_view();
        assert!(problems.is_empty(), "{problems:?}");
        assert_ne!(view.map(|v| v.state).as_deref(), Some("failed"));
    }

    #[tokio::test(start_paused = true)]
    async fn newer_release_waits_for_quiet_then_installs_and_restarts() {
        let (svc, log, _d) = service(true, self_managed(), false, 2);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        svc.set_reporter(Arc::new(move |v: Option<UpdateView>, _| {
            if let Some(v) = v {
                s2.lock().push(v.state);
            }
        }));
        let status = svc.check_now().await;
        svc.try_auto(&status, &CancellationToken::new()).await;
        assert_eq!(
            log.get(),
            ["drain true", "busy", "busy", "install 99.0.0", "restart"],
            "drain first, install only once quiet"
        );
        let states = seen.lock().clone();
        assert!(
            states.ends_with(&["waiting".into(), "installing".into(), "restarting".into()]),
            "{states:?}"
        );
        assert!(svc.is_applying(), "no second attempt while restarting");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_install_keeps_running_and_backs_off() {
        let (svc, log, _d) = service(true, self_managed(), true, 0);
        let status = svc.check_now().await;
        svc.try_auto(&status, &CancellationToken::new()).await;
        assert_eq!(log.get(), ["drain true", "install 99.0.0", "drain false"]);
        let (view, problems) = svc.auto_view();
        assert_eq!(view.unwrap().state, "failed");
        assert_eq!(problems[0].severity, bunko_control::Severity::Warn);
        assert!(!svc.is_applying());
        // Not again before the backoff is over, whatever triggers it.
        svc.try_auto(&status, &CancellationToken::new()).await;
        assert_eq!(log.get().len(), 3, "{:?}", log.get());
    }

    #[tokio::test(start_paused = true)]
    async fn docker_and_blocked_versions_only_report() {
        let (svc, log, _d) = service(true, InstallKind::Docker, false, 0);
        let status = svc.check_now().await;
        svc.try_auto(&status, &CancellationToken::new()).await;
        assert!(log.get().is_empty());
        let (view, problems) = svc.auto_view();
        assert_eq!(view.unwrap().state, "blocked");
        assert_eq!(problems[0].severity, bunko_control::Severity::Fail);
        assert_eq!(problems[0].kind.as_deref(), Some("update"));

        let (svc, log, dir) = service(true, self_managed(), false, 0);
        Blocked {
            version: "99.0.0".into(),
            reason: "its OCR backend failed to load on this machine: x".into(),
            at: "t".into(),
        }
        .write(dir.path())
        .unwrap();
        let status = svc.check_now().await;
        svc.try_auto(&status, &CancellationToken::new()).await;
        assert!(log.get().is_empty(), "a rolled-back version is not retried");
        assert!(
            svc.auto_view().1[0]
                .text
                .starts_with("The update to 99.0.0 was rolled back")
        );
    }
}
