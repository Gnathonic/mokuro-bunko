//! The background OCR backend install of `serve` and `processor serve` (full build).
//!
//! Getting up and serving never waits for the OCR backend: the pack for this machine's
//! GPU (or the CPU) is gigabytes and the models more. When local OCR is on and an
//! enabled generation needs the libtorch backend that is not installed, the instance
//! starts serving at once and runs `install-ocr --if-needed` as a child process (the
//! same detection and install as the command; its environment changes stay in the
//! child), reading its progress events ([`crate::cmd::install_ocr::EVENT_PREFIX`]):
//!
//! * the state (`bunko_proto::OcrInstall`) goes to the control API (`status.install`:
//!   the tray, the dashboard), the admin panel (through `bunko_server`'s
//!   [`BackgroundInstall`]) and, for a processor, the library (`availability` reason
//!   `installing`); the log gets a line per stage and every 10%;
//! * when it ends the libtorch backend may be looked for again in this process
//!   ([`bunko_engines::torch::forget_failure`]) and [`BackgroundInstall::finished`] is
//!   bumped: the server (re)starts its local OCR, a processor registers again;
//! * a failure is a problem ("needs you", with a retry: `POST /control/ocr-install`,
//!   the admin panel's OCR card); one that may pass by itself is retried after 5, 15
//!   and 60 minutes;
//! * `MOKURO_OCR_AUTO_INSTALL=false`: never by itself; the missing backend is a problem
//!   with an Install button instead.
//!
//! One install at a time: this process runs one child at most, and the child holds
//! `<backends>/.install.lock` (a processor on the same storage, `install-ocr` by hand).
//! An install cut short (a restart) starts over at the next start: the downloads resume
//! from their `.part` files, checked by sha256 before use.
//!
//! The setup wizard's OCR choices (a variant, a folder to install from, no models)
//! reach the install through `<backends>/.install-request.json` ([`InstallRequest`]),
//! removed once an install with them succeeded.
//!
//! The Vulkan pack is optional: its runtimes are only more benchmark candidates beside
//! the libtorch pack, so a machine whose libtorch pack serves never waits for it. It is
//! fetched by its own child (`install-ocr --variant vulkan --no-models`) that publishes
//! no state while it runs, so a processor stays available and the library keeps giving
//! it work. A failed fetch is a warning with a retry button and is remembered in
//! `<backends>/.optional-pack.json` ([`ExtraAttempt`]): the next automatic fetch waits
//! 5, 15 and 60 minutes, then a day, and a release that has nothing to download waits a
//! day at once. A processor registers again at each reconnect, and without that record
//! each registration would fetch again. A fetch that succeeds ends like an install (the
//! machine registers again once, with the new runtimes).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bunko_control::{Control, Problem, Severity};
use bunko_proto::OcrInstall;
use bunko_server::ocr::BackgroundInstall;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::cmd::install_ocr::{self, EVENT_PREFIX, Need};
use crate::ocr_target::{OcrTarget, Role};

/// The wizard's OCR choices for the next background install.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InstallRequest {
    /// `auto`, `cpu`, `cu130`, `rocm7.1` (None: what `ocr.backend` picks).
    pub variant: Option<String>,
    /// A folder holding the pack archive (and the models) to install from.
    pub from: Option<PathBuf>,
    pub no_models: bool,
    pub force: bool,
}

impl InstallRequest {
    pub const FILE: &'static str = ".install-request.json";

    pub fn path(backends: &Path) -> PathBuf {
        backends.join(Self::FILE)
    }

    pub fn read(backends: &Path) -> Option<InstallRequest> {
        serde_json::from_str(&std::fs::read_to_string(Self::path(backends)).ok()?).ok()
    }

    pub fn write(&self, backends: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(backends)?;
        std::fs::write(
            Self::path(backends),
            serde_json::to_string_pretty(self).unwrap_or_default(),
        )
    }

    /// The `install-ocr` options it adds.
    pub fn args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(v) = self.variant.as_deref().filter(|v| !v.is_empty()) {
            a.extend(["--variant".to_string(), v.to_string()]);
        }
        if let Some(f) = &self.from {
            a.extend(["--from".to_string(), f.display().to_string()]);
        }
        if self.no_models {
            a.push("--no-models".into());
        }
        if self.force {
            a.push("--force".into());
        }
        a
    }
}

/// Whose install: the library server's (its config file; `serve --ocr` passed on) or
/// a processor's (its processor.yaml).
#[derive(Debug, Clone)]
pub enum Who {
    Library {
        config_path: PathBuf,
        backend: Option<String>,
    },
    Processor {
        config: PathBuf,
    },
}

/// Waits before the automatic retries of a failure that may pass by itself.
const RETRY_AFTER: [Duration; 3] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(60 * 60),
];

/// The wait between automatic fetches of an optional pack once [`RETRY_AFTER`] is used
/// up, and after a failure that asking again soon cannot fix.
const EXTRA_RETRY_DAILY: Duration = Duration::from_secs(24 * 60 * 60);

/// The last failed fetch of an optional pack (the Vulkan pack), kept in the backends
/// directory so that neither a reconnect nor a restart fetches it again before
/// `next_at`. Another release or target starts afresh: its release may have the pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraAttempt {
    pub variant: String,
    pub bunko_version: String,
    pub target: String,
    /// Failed fetches in a row.
    pub failures: u32,
    /// No automatic fetch before this (Unix seconds).
    pub next_at: u64,
    pub message: String,
}

impl ExtraAttempt {
    pub const FILE: &'static str = ".optional-pack.json";

    /// The attempt recorded for `variant` by this release on this target.
    pub fn read(backends: &Path, variant: &str) -> Option<ExtraAttempt> {
        let a: ExtraAttempt =
            serde_json::from_str(&std::fs::read_to_string(backends.join(Self::FILE)).ok()?).ok()?;
        (a.variant == variant
            && a.bunko_version == bunko_core::VERSION
            && a.target == bunko_update::TARGET)
            .then_some(a)
    }

    fn write(&self, backends: &Path) {
        let _ = std::fs::create_dir_all(backends);
        if let Err(e) = std::fs::write(
            backends.join(Self::FILE),
            serde_json::to_string_pretty(self).unwrap_or_default(),
        ) {
            warn!(
                "OCR backend: could not record the failed fetch of the optional {} pack ({}: {e}); it is retried at the next start",
                self.variant,
                backends.join(Self::FILE).display()
            );
        }
    }

    fn clear(backends: &Path) {
        let _ = std::fs::remove_file(backends.join(Self::FILE));
    }
}

/// How long the next automatic fetch of an optional pack waits after `failures` failed
/// fetches in a row: [`RETRY_AFTER`], then a day. A release with nothing to download
/// for it, or a failure only the owner can fix, waits a day at once.
fn extra_wait(failures: u32, lasting: bool) -> Duration {
    if lasting {
        return EXTRA_RETRY_DAILY;
    }
    RETRY_AFTER
        .get(failures.saturating_sub(1) as usize)
        .copied()
        .unwrap_or(EXTRA_RETRY_DAILY)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Inner {
    who: Who,
    exe: PathBuf,
    view: watch::Sender<Option<OcrInstall>>,
    finished: watch::Sender<u64>,
    running: AtomicBool,
    control: Mutex<Option<Control>>,
    /// Automatic retries made since the last success or manual start.
    retries: Mutex<usize>,
    /// Bumped by each start: a pending automatic retry of an older one is void.
    epoch: std::sync::atomic::AtomicU64,
    /// Waits instead of [`RETRY_AFTER`] (tests).
    retry_after: Vec<Duration>,
    /// What the machine lacks ([`probe`]; tests stand in for the hardware and models).
    probe: Probe,
    /// The optional pack's fetch runs. It is not `running`: nothing waits for it.
    extra_running: AtomicBool,
    /// The optional pack's last failed fetch while it stands (read from the backends
    /// directory when this process has none yet).
    extra_failed: Mutex<Option<ExtraAttempt>>,
    /// When the pending automatic fetch of the optional pack is due (Unix seconds).
    extra_timer: Mutex<Option<u64>>,
}

/// What [`Installer`] reads to decide: the pack check, and whether model files are
/// missing.
pub(crate) struct Lack {
    pub need: Need,
    /// Model files the engines in use need are missing (only read when the pack fits).
    pub models: bool,
    /// The libtorch variant this machine wants (the label of a models-only fetch).
    pub variant: String,
}

type Probe = Arc<dyn Fn(&OcrTarget) -> Lack + Send + Sync>;

/// The machine as it is: its hardware, its packs, its model store.
fn probe(target: &OcrTarget) -> Lack {
    let hw = crate::hwdetect::detect();
    let backends = target.backends_dir();
    let need = install_ocr::need(target, &hw);
    Lack {
        models: matches!(need, Need::Satisfied(_) | Need::Extra { .. })
            && install_ocr::models_wanted(target, &backends),
        need,
        variant: install_ocr::wanted_choice(target, &hw, &backends)
            .variant
            .to_string(),
    }
}

/// The background installer (cheap to clone).
#[derive(Clone)]
pub struct Installer(Arc<Inner>);

impl Installer {
    pub fn new(who: Who) -> Installer {
        Self::with_exe(
            who,
            std::env::current_exe().unwrap_or_else(|_| PathBuf::from("mokuro-bunko")),
        )
    }

    /// With another program in the child's place (tests).
    pub fn with_exe(who: Who, exe: PathBuf) -> Installer {
        Installer(Arc::new(Inner {
            who,
            exe,
            view: watch::channel(None).0,
            finished: watch::channel(0).0,
            running: AtomicBool::new(false),
            control: Mutex::new(None),
            retries: Mutex::new(0),
            epoch: std::sync::atomic::AtomicU64::new(0),
            retry_after: RETRY_AFTER.to_vec(),
            probe: Arc::new(probe),
            extra_running: AtomicBool::new(false),
            extra_failed: Mutex::new(None),
            extra_timer: Mutex::new(None),
        }))
    }

    /// Another reading of what the machine lacks (tests).
    #[cfg(test)]
    pub(crate) fn with_probe(
        self,
        probe: impl Fn(&OcrTarget) -> Lack + Send + Sync + 'static,
    ) -> Installer {
        let inner = Arc::try_unwrap(self.0).ok().expect("not shared yet");
        Installer(Arc::new(Inner {
            probe: Arc::new(probe),
            ..inner
        }))
    }

    /// Shorter automatic-retry waits (tests).
    #[cfg(test)]
    pub fn with_retry_after(self, waits: Vec<Duration>) -> Installer {
        let inner = Arc::try_unwrap(self.0).ok().expect("not shared yet");
        Installer(Arc::new(Inner {
            retry_after: waits,
            ..inner
        }))
    }

    /// The state for the control API, the admin panel and a processor's availability.
    pub fn subscribe(&self) -> watch::Receiver<Option<OcrInstall>> {
        self.0.view.subscribe()
    }

    /// Mirror the state into the control API (`status.install`, problems) and let
    /// `POST /control/ocr-install` start a retry.
    pub fn attach_control(&self, control: &Control) {
        let me = self.clone();
        control.set_install_trigger(Arc::new(move || me.start_manual()));
        // Under the publish lock: the view copied here cannot be older than one a
        // running install publishes meanwhile.
        let mut slot = self.0.control.lock();
        *slot = Some(control.clone());
        let now = self.0.view.borrow().clone();
        control.set_install(now.clone(), self.problems(now.as_ref()));
    }

    /// The OCR target as the config files say now (a setting saved since the start
    /// counts).
    fn target(&self) -> Result<OcrTarget, String> {
        match &self.0.who {
            Who::Library {
                config_path,
                backend,
            } => {
                let mut config =
                    crate::cfgfile::load_effective_quiet(config_path).map_err(|e| e.to_string())?;
                if let Some(b) = backend {
                    config.ocr.backend = b.clone();
                }
                Ok(OcrTarget {
                    role: Role::Library,
                    storage: config.storage.base_path.clone(),
                    processor_config: None,
                    library: Some(config),
                    reason: String::new(),
                })
            }
            Who::Processor { config } => {
                let c = bunko_processor::load_processor_config(config)
                    .map_err(|e| format!("{}: {e}", config.display()))?;
                Ok(OcrTarget {
                    role: Role::Processor,
                    storage: c.processor.storage,
                    processor_config: Some(config.clone()),
                    library: None,
                    reason: String::new(),
                })
            }
        }
    }

    /// At the start: install in the background when it is needed and allowed; when it
    /// is needed but automatic installs are off, say so (a problem with an Install
    /// button). Returns whether an install runs now. Call inside the tokio runtime.
    pub fn start_if_needed(&self) -> bool {
        if self.0.running.load(Ordering::SeqCst) {
            return true;
        }
        let (auto, note) = install_ocr::auto_install_setting(|n| std::env::var(n).ok());
        if let Some(n) = note {
            info!("{n}");
        }
        match self.decide() {
            Ok(Decision::Install {
                models_only: true, ..
            }) if auto => {
                info!(
                    "OCR models are missing for the engines in use; fetching them in the background"
                );
                self.spawn();
                true
            }
            Ok(Decision::Install {
                reason, request, ..
            }) if auto => {
                info!(
                    "OCR backend: {reason}; installing it in the background{}",
                    if request {
                        " with the setup's choices"
                    } else {
                        ""
                    }
                );
                self.spawn();
                true
            }
            Ok(Decision::Install {
                models_only: true, ..
            }) => {
                info!(
                    "OCR models are missing; automatic installs are off (MOKURO_OCR_AUTO_INSTALL=false): they download when an engine first needs them"
                );
                false
            }
            Ok(Decision::Install { variant, .. }) => {
                let text = format!(
                    "No OCR backend installed for the enabled OCR generations (the {variant} pack); automatic installs are off (MOKURO_OCR_AUTO_INSTALL=false)"
                );
                warn!(
                    "{text}: hayai-nova and paddle-manga cannot run here until it is installed ('mokuro-bunko install-ocr', or Install in the dashboard / admin panel)"
                );
                self.publish(Some(OcrInstall {
                    state: OcrInstall::MISSING.into(),
                    stage: "missing".into(),
                    variant: Some(variant),
                    message: Some(text),
                    needs_owner: true,
                    action: Some(
                        "Install it from the dashboard or the admin panel (OCR), or run 'mokuro-bunko install-ocr'.".into(),
                    ),
                    since: Some(now()),
                    ..OcrInstall::default()
                }));
                false
            }
            // The backend serves: the optional pack is fetched on the side, and the
            // machine waits for nothing (only for a first fetch of its one GPU route).
            Ok(Decision::Extra {
                variant,
                reason,
                route,
            }) => {
                if auto {
                    self.start_extra(variant, reason, false, route)
                } else {
                    tracing::debug!(
                        "OCR backend: the optional {variant} pack is not installed ({reason}); automatic installs are off"
                    );
                    false
                }
            }
            Ok(Decision::Nothing(why)) => {
                tracing::debug!("OCR backend: {why}");
                self.drop_stale_extra();
                false
            }
            Err(e) => {
                warn!("OCR backend: could not check whether it is installed: {e}");
                false
            }
        }
    }

    /// Start (or retry) by hand: whatever the automatic-install setting says, and
    /// without waiting for an earlier failed fetch of the optional pack. Returns
    /// whether an install the machine waits for runs now (its end bumps
    /// [`BackgroundInstall::finished`]); a fetch of the optional pack on the side is
    /// not one ([`Self::fetching_extra`]).
    pub fn start_manual(&self) -> Result<bool, String> {
        self.start_asked(true)
    }

    /// A setting changed (local processing turned on, another backend preference): the
    /// backend installs as if asked for, but the optional pack is fetched only as an
    /// automatic fetch is (automatic installs on, no earlier failure to wait for). A
    /// toggle is not a retry, and treating it as one fetched the pack again at once on
    /// every save. Returns what [`Self::start_manual`] does.
    pub fn start_for_settings(&self) -> Result<bool, String> {
        self.start_asked(false)
    }

    fn start_asked(&self, owner: bool) -> Result<bool, String> {
        *self.0.retries.lock() = 0;
        if self.0.running.load(Ordering::SeqCst) {
            return Ok(true);
        }
        match self.decide()? {
            Decision::Install { variant, .. } => {
                info!("OCR backend: installing the {variant} pack (asked for)");
                self.spawn();
                Ok(true)
            }
            Decision::Extra {
                variant,
                reason,
                route,
            } => {
                // The backend is in place: a failure of an earlier install is over.
                self.drop_stale_failure();
                let (auto, _) = install_ocr::auto_install_setting(|n| std::env::var(n).ok());
                if !owner && !auto {
                    return Ok(false);
                }
                Ok(self.start_extra(variant, reason, owner, route))
            }
            Decision::Nothing(why) => {
                // Nothing to do any more (installed meanwhile, or OCR turned off).
                self.drop_stale_failure();
                self.drop_stale_extra();
                info!("OCR backend: nothing to install ({why})");
                Ok(false)
            }
        }
    }

    /// The optional pack's fetch runs on the side (nothing waits for it).
    pub fn fetching_extra(&self) -> bool {
        self.0.extra_running.load(Ordering::SeqCst)
    }

    /// The optional pack is in place after all (installed by hand, as with
    /// `install-ocr --variant vulkan`): its recorded failure no longer stands, so the
    /// warning goes. It ends as a fetch that worked does, so a processor registers
    /// again with the new runtimes and the server's own OCR restarts with them.
    fn drop_stale_extra(&self) {
        let Ok(target) = self.target() else {
            return;
        };
        let backends = target.backends_dir();
        let standing = self
            .0
            .extra_failed
            .lock()
            .clone()
            .or_else(|| ExtraAttempt::read(&backends, install_ocr::VULKAN));
        let Some(a) = standing else {
            return;
        };
        let Some(pack) = install_ocr::already_fits(&target.backends_dirs(), &a.variant) else {
            return;
        };
        ExtraAttempt::clear(&backends);
        *self.0.extra_failed.lock() = None;
        self.refresh_problems();
        info!(
            "OCR backend: the optional {} pack is installed now ({pack}); its runtimes are benchmark candidates",
            a.variant
        );
        if self.0.running.load(Ordering::SeqCst) || self.0.extra_running.load(Ordering::SeqCst) {
            return; // That run publishes its own end.
        }
        self.publish(Some(OcrInstall {
            state: OcrInstall::DONE.into(),
            stage: "done".into(),
            percent: Some(100),
            variant: Some(a.variant.clone()),
            since: Some(now()),
            ..OcrInstall::default()
        }));
        if let Some(c) = self.0.control.lock().as_ref() {
            c.set_backend_pack(crate::control::pack_name(&target.backends_dirs()));
        }
        self.0.finished.send_modify(|n| *n += 1);
    }

    /// A failed or missing backend that no longer stands: drop it and say the install
    /// ended, so whatever waited for the backend starts with what is installed now.
    fn drop_stale_failure(&self) {
        let stale = self
            .0
            .view
            .borrow()
            .as_ref()
            .is_some_and(|v| v.failed() || v.state == OcrInstall::MISSING);
        if stale {
            self.publish(None);
            self.0.finished.send_modify(|n| *n += 1);
        }
    }

    fn decide(&self) -> Result<Decision, String> {
        let target = self.target()?;
        let backends = target.backends_dir();
        let request = InstallRequest::read(&backends);
        let lack = (self.0.probe)(&target);
        match lack.need {
            Need::Off(why) => Ok(Decision::Nothing(format!("local OCR is off: {why}"))),
            Need::Install { variant, reason } => {
                let variant = request
                    .as_ref()
                    .and_then(|r| r.variant.clone())
                    .filter(|v| v != "auto")
                    .unwrap_or(variant);
                Ok(Decision::Install {
                    reason: format!("the {variant} pack is not installed ({reason})"),
                    variant,
                    request: request.is_some(),
                    models_only: false,
                })
            }
            // The wizard asked for something the pack in place may not be (another
            // variant, a reinstall): the child decides.
            Need::Satisfied(_) | Need::Extra { .. } if request.is_some() => Ok(Decision::Install {
                variant: request
                    .as_ref()
                    .and_then(|r| r.variant.clone())
                    .unwrap_or_else(|| "auto".into()),
                reason: "the setup picked the OCR backend".into(),
                request: true,
                models_only: false,
            }),
            // The pack is in place but models are missing (the last install's failed,
            // or the library asked for another engine): the child fetches them.
            Need::Satisfied(_) | Need::Extra { .. } if lack.models => Ok(Decision::Install {
                variant: lack.variant,
                reason: "OCR models are missing for the engines in use".into(),
                request: false,
                models_only: true,
            }),
            Need::Extra {
                variant,
                reason,
                route,
                ..
            } => Ok(Decision::Extra {
                variant,
                reason,
                route,
            }),
            Need::Satisfied(why) => Ok(Decision::Nothing(why)),
        }
    }

    /// The child's command line: `[--config X] install-ocr --if-needed [--processor] ...`.
    /// The optional pack's child installs that pack alone (`--variant X --no-models`);
    /// the backend's child leaves it out (it is fetched on its own afterwards).
    fn command(&self, extra: Option<&str>) -> Result<ChildCommand, String> {
        let target = self.target()?;
        let backends = target.backends_dir();
        let mut args = Vec::new();
        let mut envs = vec![
            ("NO_COLOR".to_string(), "1".to_string()),
            (install_ocr::EVENTS_ENV.to_string(), "1".to_string()),
            ("MOKURO_PROGRESS_STEP".to_string(), "10".to_string()),
            // Decided here (automatic and allowed, or asked for by hand).
            ("MOKURO_OCR_AUTO_INSTALL".to_string(), "true".to_string()),
        ];
        match &self.0.who {
            Who::Library {
                config_path,
                backend,
            } => {
                args.extend(["--config".to_string(), config_path.display().to_string()]);
                args.extend(["install-ocr".to_string(), "--if-needed".to_string()]);
                if let Some(b) = backend {
                    envs.push(("MOKURO_OCR_BACKEND".into(), b.clone()));
                }
            }
            Who::Processor { config } => {
                args.extend([
                    "install-ocr".to_string(),
                    "--if-needed".to_string(),
                    "--processor".to_string(),
                ]);
                let abs = std::path::absolute(config).unwrap_or_else(|_| config.clone());
                envs.push(("MOKURO_PROCESSOR_CONFIG".into(), abs.display().to_string()));
            }
        }
        match extra {
            Some(variant) => {
                args.extend([
                    "--variant".to_string(),
                    variant.to_string(),
                    "--no-models".to_string(),
                ]);
            }
            None => {
                envs.push((install_ocr::NO_EXTRAS_ENV.to_string(), "1".to_string()));
                if let Some(r) = InstallRequest::read(&backends) {
                    args.extend(r.args());
                }
            }
        }
        Ok((args, envs, backends))
    }

    fn spawn(&self) {
        if self.0.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let epoch = self.0.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let since = now();
        self.publish(Some(OcrInstall {
            state: OcrInstall::RUNNING.into(),
            stage: "checking".into(),
            since: Some(since.clone()),
            ..OcrInstall::default()
        }));
        let me = self.clone();
        let run = async move {
            let outcome = me.run_child(since, None).await;
            me.finish(outcome, epoch);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(run);
            }
            Err(_) => {
                warn!("OCR backend install: no runtime to run it in");
                self.0.running.store(false, Ordering::SeqCst);
            }
        }
    }

    /// Run `install-ocr` to its end. `extra`: the optional pack's child, whose progress
    /// is only logged (no state: nothing waits for it).
    async fn run_child(&self, since: String, extra: Option<&str>) -> Outcome {
        let (args, envs, backends) = match self.command(extra) {
            Ok(c) => c,
            Err(e) => {
                return Outcome::Failed {
                    message: e,
                    needs_owner: true,
                    action: None,
                    lasting: false,
                };
            }
        };
        info!("OCR install: mokuro-bunko {}", args.join(" "));
        let mut cmd = tokio::process::Command::new(&self.0.exe);
        cmd.args(&args)
            .envs(envs)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        {
            // No console window for the child of a GUI-started server.
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Outcome::Failed {
                    message: format!("could not start {}: {e}", self.0.exe.display()),
                    needs_owner: true,
                    action: None,
                    lasting: false,
                };
            }
        };
        let mut tracker = Tracker::new(since);
        if let Some(v) = extra {
            tracker.label = format!("OCR install of the optional {v} pack");
        }
        let tracker = Arc::new(Mutex::new(tracker));
        let publish = extra.is_none();
        let out = child
            .stdout
            .take()
            .map(|s| self.pump(s, tracker.clone(), false, publish));
        let err = child
            .stderr
            .take()
            .map(|s| self.pump(s, tracker.clone(), true, publish));
        let status = child.wait().await;
        for h in [out, err].into_iter().flatten() {
            let _ = h.await;
        }
        let t = tracker.lock();
        match status {
            Ok(s) if s.success() => {
                if extra.is_none() {
                    let _ = std::fs::remove_file(InstallRequest::path(&backends));
                }
                Outcome::Done {
                    variant: t.view.variant.clone(),
                    pack: t.view.pack.clone(),
                }
            }
            Ok(s) => Outcome::Failed {
                message: t
                    .failure
                    .clone()
                    .or_else(|| t.last_error.clone())
                    .unwrap_or_else(|| format!("install-ocr exited with {s}")),
                needs_owner: t.needs_owner,
                action: t.action.clone(),
                lasting: t.lasting,
            },
            Err(e) => Outcome::Failed {
                message: format!("install-ocr: {e}"),
                needs_owner: false,
                action: None,
                lasting: false,
            },
        }
    }

    fn pump<R: AsyncRead + Unpin + Send + 'static>(
        &self,
        stream: R,
        tracker: Arc<Mutex<Tracker>>,
        stderr: bool,
        publish: bool,
    ) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let changed = {
                    let mut t = tracker.lock();
                    t.line(&line, stderr)
                };
                if let Some(view) = changed.filter(|_| publish) {
                    me.publish(Some(view));
                }
            }
        })
    }

    fn finish(&self, outcome: Outcome, epoch: u64) {
        // The libtorch backend may be looked for again in this process.
        bunko_engines::torch::forget_failure();
        let done = matches!(outcome, Outcome::Done { .. });
        match outcome {
            Outcome::Done { variant, pack } => {
                *self.0.retries.lock() = 0;
                info!(
                    "OCR install: done{}; local OCR picks it up now",
                    pack.as_deref()
                        .map(|p| format!(" ({p})"))
                        .unwrap_or_default()
                );
                self.publish(Some(OcrInstall {
                    state: OcrInstall::DONE.into(),
                    stage: "done".into(),
                    percent: Some(100),
                    variant,
                    pack,
                    since: Some(now()),
                    ..OcrInstall::default()
                }));
            }
            Outcome::Failed {
                message,
                needs_owner,
                action,
                ..
            } => {
                let retry = {
                    let mut r = self.0.retries.lock();
                    let wait = (!needs_owner)
                        .then(|| self.0.retry_after.get(*r).copied())
                        .flatten();
                    if wait.is_some() {
                        *r += 1;
                    }
                    wait
                };
                tracing::error!(
                    "OCR install failed: {message}{}",
                    match retry {
                        Some(w) => format!("; trying again in {} min", w.as_secs().div_ceil(60)),
                        None => String::new(),
                    }
                );
                let mut text = message.clone();
                if let Some(w) = retry {
                    text = format!("{text} (trying again in {} min)", w.as_secs().div_ceil(60));
                }
                self.publish(Some(OcrInstall {
                    state: OcrInstall::FAILED.into(),
                    stage: "failed".into(),
                    message: Some(text),
                    needs_owner,
                    action,
                    since: Some(now()),
                    ..OcrInstall::default()
                }));
                if let Some(wait) = retry {
                    let me = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(wait).await;
                        if me.0.epoch.load(Ordering::SeqCst) == epoch
                            && !me.0.running.load(Ordering::SeqCst)
                        {
                            info!("OCR install: trying again");
                            let _ = me.start_retry();
                        }
                    });
                }
            }
        }
        self.0.running.store(false, Ordering::SeqCst);
        // The pack in use, as the status shows it.
        if let Some(c) = self.0.control.lock().as_ref()
            && let Ok(t) = self.target()
        {
            c.set_backend_pack(crate::control::pack_name(&t.backends_dirs()));
        }
        // Done or not: the local OCR (re)starts with what is installed now.
        self.0.finished.send_modify(|n| *n += 1);
        // The install left the optional pack out: fetch it now. Nothing else would
        // start it before a processor registers again, and a server never does.
        if done {
            let me = self.clone();
            tokio::task::spawn_blocking(move || me.start_extra_if_wanted());
        }
    }

    /// Fetch the optional pack when the backend is in place, it is missing, and
    /// automatic installs are on. Nothing else: an install of the backend that ended
    /// without its pack must not start again from here.
    fn start_extra_if_wanted(&self) {
        let (auto, _) = install_ocr::auto_install_setting(|n| std::env::var(n).ok());
        if auto
            && let Ok(Decision::Extra {
                variant,
                reason,
                route,
            }) = self.decide()
        {
            self.start_extra(variant, reason, false, route);
        }
    }

    /// An automatic retry (the retries count goes on).
    fn start_retry(&self) -> Result<bool, String> {
        match self.decide()? {
            Decision::Install { .. } => {
                self.spawn();
                Ok(true)
            }
            Decision::Extra {
                variant,
                reason,
                route,
            } => {
                // The backend was put in place meanwhile: the install ends as below,
                // and the optional pack is fetched on the side.
                self.publish(None);
                self.0.finished.send_modify(|n| *n += 1);
                info!("OCR backend: nothing left to install for the backend");
                Ok(self.start_extra(variant, reason, false, route))
            }
            Decision::Nothing(why) => {
                // Nothing to do any more (installed by hand meanwhile, or OCR turned
                // off). The failure no longer stands, and this ends the install as a
                // run does: the OCR that waited for the backend starts with what is
                // installed now.
                self.publish(None);
                self.0.finished.send_modify(|n| *n += 1);
                info!("OCR backend: nothing left to install ({why})");
                Ok(false)
            }
        }
    }

    /// Fetch the optional pack `variant` in the background unless an earlier failure
    /// says to wait (`manual`: the owner asked, so no wait). Nothing is published
    /// while it runs: a processor stays available, and the server's own OCR does not
    /// wait. Only when the pack is the machine's one GPU route (`route`) and no fetch
    /// of it failed yet does the machine wait for it as for an install, so that it is
    /// not benchmarked on the CPU meanwhile. Returns whether such a fetch, one the
    /// machine waits for, runs now: its end bumps [`BackgroundInstall::finished`].
    fn start_extra(&self, variant: String, reason: String, manual: bool, route: bool) -> bool {
        let backends = match self.target() {
            Ok(t) => t.backends_dir(),
            Err(e) => {
                warn!("OCR backend: could not check the optional {variant} pack: {e}");
                return false;
            }
        };
        let mut failed_before = false;
        if manual {
            ExtraAttempt::clear(&backends);
            *self.0.extra_failed.lock() = None;
            self.refresh_problems();
        } else if let Some(a) = self.extra_attempt(&backends, &variant) {
            if a.next_at > unix_now() {
                tracing::debug!(
                    "OCR backend: the optional {variant} pack is not installed ({}); the next automatic fetch is due at {}",
                    a.message,
                    rfc3339(a.next_at)
                );
                self.schedule_extra(a.next_at);
                return false;
            }
            failed_before = true;
        }
        if self.0.extra_running.swap(true, Ordering::SeqCst) {
            // A fetch runs already: the machine waits for it if it holds the machine.
            return route && self.0.running.load(Ordering::SeqCst);
        }
        // After a failure the machine runs on the CPU, and a later try goes on the side.
        let holds = route && !failed_before && !self.0.running.swap(true, Ordering::SeqCst);
        if holds {
            info!(
                "OCR backend: fetching the {variant} pack, this machine's GPU route ({reason}); OCR waits for it"
            );
            self.publish(Some(OcrInstall {
                state: OcrInstall::RUNNING.into(),
                stage: "checking".into(),
                variant: Some(variant.clone()),
                since: Some(now()),
                ..OcrInstall::default()
            }));
        } else {
            info!(
                "OCR backend: fetching the optional {variant} pack in the background ({reason}); OCR runs without it meanwhile"
            );
        }
        let me = self.clone();
        let fetch = variant.clone();
        let run = async move {
            let variant = fetch;
            let outcome = me.run_child(now(), Some(&variant)).await;
            let me2 = me.clone();
            let ended =
                tokio::task::spawn_blocking(move || me2.finish_extra(&variant, outcome, holds))
                    .await;
            if ended.is_err() {
                me.0.extra_running.store(false, Ordering::SeqCst);
                if holds {
                    me.0.running.store(false, Ordering::SeqCst);
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(run);
                holds
            }
            Err(_) => {
                warn!("OCR backend: no runtime to fetch the optional {variant} pack in");
                self.0.extra_running.store(false, Ordering::SeqCst);
                if holds {
                    self.0.running.store(false, Ordering::SeqCst);
                    self.publish(None);
                }
                false
            }
        }
    }

    /// The optional pack's fetch ended. A pack that is now in place ends like an
    /// install (the machine registers again with its runtimes). Anything else is a
    /// warning and a wait: it publishes no state, so nothing registers again, and the
    /// next registration does not fetch again. A fetch the machine waited for
    /// (`holds`) ends either way, so the machine goes on, on the CPU after a failure.
    fn finish_extra(&self, variant: &str, outcome: Outcome, holds: bool) {
        let target = self.target();
        // Exit 0 is not proof: only a pack the backend check now finds counts. An end
        // without one would register again and fetch again, without end.
        let still_missing = match &target {
            Ok(t) => matches!(
                (self.0.probe)(t).need,
                Need::Extra { variant: ref v, .. } if v == variant
            ),
            Err(_) => true,
        };
        let backends = target.as_ref().ok().map(|t| t.backends_dir());
        let failure = match outcome {
            Outcome::Done { pack, .. } if !still_missing => {
                if let Some(b) = &backends {
                    ExtraAttempt::clear(b);
                }
                *self.0.extra_failed.lock() = None;
                self.refresh_problems();
                info!(
                    "OCR backend: the optional {variant} pack is installed; its runtimes are benchmark candidates now"
                );
                self.0.extra_running.store(false, Ordering::SeqCst);
                // An install of the backend that runs meanwhile publishes its own end.
                if holds || !self.0.running.load(Ordering::SeqCst) {
                    self.publish(Some(OcrInstall {
                        state: OcrInstall::DONE.into(),
                        stage: "done".into(),
                        percent: Some(100),
                        variant: Some(variant.to_string()),
                        pack,
                        since: Some(now()),
                        ..OcrInstall::default()
                    }));
                    if holds {
                        self.0.running.store(false, Ordering::SeqCst);
                    }
                    if let Some(c) = self.0.control.lock().as_ref()
                        && let Ok(t) = &target
                    {
                        c.set_backend_pack(crate::control::pack_name(&t.backends_dirs()));
                    }
                    self.0.finished.send_modify(|n| *n += 1);
                }
                return;
            }
            Outcome::Done { .. } => (
                format!("install-ocr ended, but no usable {variant} pack is in place"),
                true,
            ),
            Outcome::Failed {
                message,
                needs_owner,
                lasting,
                ..
            } => (message, lasting || needs_owner),
        };
        let (message, lasting) = failure;
        let failures = backends
            .as_deref()
            .and_then(|b| self.extra_attempt(b, variant))
            .map_or(0, |a| a.failures)
            + 1;
        let wait = extra_wait(failures, lasting);
        let attempt = ExtraAttempt {
            variant: variant.to_string(),
            bunko_version: bunko_core::VERSION.to_string(),
            target: bunko_update::TARGET.to_string(),
            failures,
            next_at: unix_now() + wait.as_secs(),
            message: message.clone(),
        };
        if let Some(b) = &backends {
            attempt.write(b);
        }
        warn!(
            "OCR backend: the optional {variant} pack was not installed ({message}); OCR runs without it. The next automatic fetch is at {} (or retry from the dashboard or the admin panel)",
            rfc3339(attempt.next_at)
        );
        let next_at = attempt.next_at;
        *self.0.extra_failed.lock() = Some(attempt);
        self.refresh_problems();
        self.0.extra_running.store(false, Ordering::SeqCst);
        if holds {
            // The machine waited for it: it goes on without it (the warning stands),
            // as after an install that ended. A processor registers again on the CPU,
            // and that registration waits for the next automatic fetch.
            self.publish(None);
            self.0.running.store(false, Ordering::SeqCst);
            self.0.finished.send_modify(|n| *n += 1);
        }
        self.schedule_extra(next_at);
    }

    /// The optional pack's standing failure: this process's, else the one recorded in
    /// the backends directory (an earlier run).
    fn extra_attempt(&self, backends: &Path, variant: &str) -> Option<ExtraAttempt> {
        let mut slot = self.0.extra_failed.lock();
        if slot.as_ref().is_some_and(|a| a.variant == variant) {
            return slot.clone();
        }
        let read = ExtraAttempt::read(backends, variant);
        if read.is_some() {
            *slot = read.clone();
            drop(slot);
            self.refresh_problems();
        }
        read
    }

    /// Check again at `at` (Unix seconds): the optional pack's next automatic fetch,
    /// for a machine that stays connected that long. One timer at a time.
    fn schedule_extra(&self, at: u64) {
        {
            let mut t = self.0.extra_timer.lock();
            if *t == Some(at) {
                return;
            }
            *t = Some(at);
        }
        let Ok(h) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let me = self.clone();
        h.spawn(async move {
            tokio::time::sleep(Duration::from_secs(at.saturating_sub(unix_now()))).await;
            {
                let mut t = me.0.extra_timer.lock();
                if *t != Some(at) {
                    return; // Superseded by a later failure.
                }
                *t = None;
            }
            let _ = tokio::task::spawn_blocking(move || me.start_if_needed()).await;
        });
    }

    /// The problems of `view`, and the optional pack's standing failure (a warning).
    fn problems(&self, view: Option<&OcrInstall>) -> Vec<Problem> {
        let mut out = problems_of(view);
        if let Some(a) = self.0.extra_failed.lock().as_ref() {
            out.push(extra_problem(a));
        }
        out
    }

    /// Tell the control API the problems again (the view is unchanged).
    fn refresh_problems(&self) {
        let control = self.0.control.lock();
        if let Some(c) = control.as_ref() {
            let view = self.0.view.borrow().clone();
            c.set_install(view.clone(), self.problems(view.as_ref()));
        }
    }

    fn publish(&self, view: Option<OcrInstall>) {
        // One publish at a time, and the control before the watch: whoever wakes on
        // the watch (the problems watcher, a test) reads a control status that already
        // has this view, and two publishes from different threads cannot leave the
        // control with the older one.
        let control = self.0.control.lock();
        if let Some(c) = control.as_ref() {
            c.set_install(view.clone(), self.problems(view.as_ref()));
        }
        self.0.view.send_replace(view);
    }
}

/// The installer has something to say about the backend (installing, failed, missing):
/// the doctor's "no backend pack" problem would only repeat it.
pub fn speaks_for_backend(installer: Option<&Installer>) -> bool {
    installer
        .and_then(|i| i.0.view.borrow().clone())
        .is_some_and(|v| v.state != OcrInstall::DONE)
}

/// The warning a failed fetch of an optional pack raises: OCR works, a benchmark
/// candidate is missing. Its retry is the install's (`POST /control/ocr-install`).
fn extra_problem(a: &ExtraAttempt) -> Problem {
    Problem {
        severity: Severity::Warn,
        text: format!(
            "The optional {} OCR backend pack is not installed: {}. OCR runs without it; the next automatic try is at {}.",
            a.variant,
            a.message,
            rfc3339(a.next_at)
        ),
        hint: Some(format!(
            "Retry from the dashboard or the admin panel (OCR), or run 'mokuro-bunko install-ocr --variant {}'.",
            a.variant
        )),
        kind: Some(Problem::KIND_OCR_INSTALL.into()),
    }
}

/// The problem a state raises: a failure, or a missing backend nobody will install.
pub fn problems_of(view: Option<&OcrInstall>) -> Vec<Problem> {
    let Some(v) = view else {
        return Vec::new();
    };
    if !(v.failed() || v.state == OcrInstall::MISSING) {
        return Vec::new();
    }
    vec![Problem {
        severity: Severity::Fail,
        text: if v.failed() {
            v.summary()
        } else {
            v.message
                .clone()
                .unwrap_or_else(|| "No OCR backend installed".into())
        },
        hint: Some(v.action.clone().unwrap_or_else(|| {
            "Retry from the dashboard or the admin panel (OCR), or run 'mokuro-bunko install-ocr'; the log has the details.".into()
        })),
        kind: Some(Problem::KIND_OCR_INSTALL.into()),
    }]
}

impl BackgroundInstall for Installer {
    fn view(&self) -> Option<OcrInstall> {
        self.0.view.borrow().clone()
    }

    fn start(&self) -> Result<bool, String> {
        self.start_manual()
    }

    fn start_for_settings(&self) -> Result<bool, String> {
        Installer::start_for_settings(self)
    }

    fn finished(&self) -> watch::Receiver<u64> {
        self.0.finished.subscribe()
    }

    /// A pack this release opens that serves the machine is in place, or none is needed
    /// (what `install-ocr --if-needed` checks). One that failed to replace another
    /// leaves the old in place: still ready.
    fn backend_ready(&self) -> bool {
        let Ok(target) = self.target() else {
            return true;
        };
        // An optional pack (the Vulkan pack) missing is ready: OCR runs without it.
        !matches!((self.0.probe)(&target).need, Need::Install { .. })
    }
}

/// The child's arguments, environment, and the backends directory.
type ChildCommand = (Vec<String>, Vec<(String, String)>, PathBuf);

enum Decision {
    Install {
        variant: String,
        reason: String,
        request: bool,
        /// The pack is in place: only models are fetched.
        models_only: bool,
    },
    /// The backend is in place; an optional pack beside it is missing (`route`: it is
    /// the machine's one GPU route, [`Need::Extra`]).
    Extra {
        variant: String,
        reason: String,
        route: bool,
    },
    Nothing(String),
}

enum Outcome {
    Done {
        variant: Option<String>,
        pack: Option<String>,
    },
    Failed {
        message: String,
        needs_owner: bool,
        action: Option<String>,
        /// Asking again soon finds the same (the release is not published).
        lasting: bool,
    },
}

fn now() -> String {
    chrono_like_now()
}

/// Unix seconds as RFC 3339 UTC, seconds.
fn rfc3339(secs: u64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs as i64)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

/// RFC 3339 UTC, seconds.
fn chrono_like_now() -> String {
    let t = time::OffsetDateTime::now_utc();
    t.format(&time::format_description::well_known::Rfc3339)
        .map(|s| {
            // Seconds precision, `Z`.
            match s.find('.') {
                Some(dot) => format!("{}Z", &s[..dot]),
                None => s,
            }
        })
        .unwrap_or_default()
}

/// The child's output turned into the state, and what the log gets.
struct Tracker {
    view: OcrInstall,
    /// When the log last got a progress line, and its percent.
    logged: Option<(String, u32, Instant)>,
    failure: Option<String>,
    needs_owner: bool,
    action: Option<String>,
    last_error: Option<String>,
    /// The models stage's files and packages: (done, of how many).
    files: Option<(u64, u64)>,
    /// The child's `failed` event said asking again soon finds the same.
    lasting: bool,
    /// What the log lines start with.
    label: String,
}

impl Tracker {
    fn new(since: String) -> Tracker {
        Tracker {
            view: OcrInstall {
                state: OcrInstall::RUNNING.into(),
                stage: "checking".into(),
                since: Some(since),
                ..OcrInstall::default()
            },
            logged: None,
            failure: None,
            needs_owner: false,
            action: None,
            last_error: None,
            files: None,
            lasting: false,
            label: "OCR install".into(),
        }
    }

    /// One line of the child's output: the new state when it changed.
    fn line(&mut self, line: &str, stderr: bool) -> Option<OcrInstall> {
        if let Some(json) = line.strip_prefix(EVENT_PREFIX) {
            let v: serde_json::Value = serde_json::from_str(json).ok()?;
            return self.event(&v);
        }
        let text = line.trim_end();
        if text.trim().is_empty() {
            return None;
        }
        if stderr || text.starts_with("Error:") {
            self.last_error = Some(text.trim_start_matches("Error:").trim().to_string());
        }
        // A model file downloading (bunko-ocr's log): "downloading NAME: a / b MB (p%), ...".
        if self.view.stage == "models"
            && let Some((name, done, total, pct)) = parse_model_download(text)
        {
            // The file's own progress; which file of how many says the message.
            self.view.message = Some(match self.files {
                Some((done, total)) => {
                    format!("file {} of {total}: {name}", (done + 1).min(total))
                }
                None => format!("downloading {name}"),
            });
            self.view.done_bytes = Some(done);
            self.view.total_bytes = Some(total);
            self.view.percent = Some(pct);
            self.log_progress();
            return Some(self.view.clone());
        }
        info!("{}: {}", self.label, strip_log_prefix(text));
        None
    }

    fn event(&mut self, v: &serde_json::Value) -> Option<OcrInstall> {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let n = |k: &str| v.get(k).and_then(|x| x.as_u64());
        let stage = s("stage").unwrap_or_default();
        if stage == "failed" {
            self.failure = s("message");
            self.needs_owner = v.get("needs_owner").and_then(|x| x.as_bool()) == Some(true);
            self.action = s("action");
            self.lasting = v.get("lasting").and_then(|x| x.as_bool()) == Some(true);
            return None;
        }
        if let Some(var) = s("variant") {
            self.view.variant = Some(var);
        }
        if let Some(p) = s("pack") {
            self.view.pack = Some(p);
        }
        let new_stage = stage != self.view.stage;
        if new_stage && !stage.is_empty() {
            self.view.stage = stage.clone();
            self.view.percent = None;
            self.view.done_bytes = None;
            self.view.total_bytes = None;
            self.view.message = s("message");
        }
        if let (Some(done), Some(total)) = (n("done"), n("total")) {
            self.view.percent =
                Some(((done * 100).checked_div(total).unwrap_or(100)).min(100) as u32);
            if s("unit").as_deref() == Some("files") {
                self.files = Some((done, total));
                self.view.done_bytes = None;
                self.view.total_bytes = None;
            } else {
                self.view.done_bytes = Some(done);
                self.view.total_bytes = Some(total);
            }
        }
        if let Some(l) = s("label") {
            self.view.message = Some(match (s("unit").as_deref(), self.files) {
                (Some("files"), Some((done, total))) => format!("{done} of {total} done: {l}"),
                _ => l,
            });
        }
        if new_stage {
            info!("{}: {}", self.label, stage_text(&self.view));
            self.logged = Some((
                self.view.stage.clone(),
                self.view.percent.unwrap_or(0),
                Instant::now(),
            ));
        } else {
            self.log_progress();
        }
        Some(self.view.clone())
    }

    /// Every 10% (or 30 s) of a stage, a line in the log.
    fn log_progress(&mut self) {
        let pct = self.view.percent.unwrap_or(0);
        let due = match &self.logged {
            Some((stage, last, at)) if *stage == self.view.stage => {
                pct / 10 > last / 10 || at.elapsed() >= Duration::from_secs(30)
            }
            _ => true,
        };
        if due {
            info!("{}: {}", self.label, stage_text(&self.view));
            self.logged = Some((self.view.stage.clone(), pct, Instant::now()));
        }
    }
}

/// `downloading the backend pack mokuro-bunko-backend-…: 42% (1.2 of 2.9 GB)`.
pub fn stage_text(v: &OcrInstall) -> String {
    let what = match v.stage.as_str() {
        "checking" => "checking what this machine needs".to_string(),
        "waiting" => "waiting for another OCR install on this machine".to_string(),
        "downloading" => format!(
            "downloading the {}backend pack{}",
            v.variant
                .as_deref()
                .map(|x| format!("{x} "))
                .unwrap_or_default(),
            v.pack
                .as_deref()
                .map(|p| format!(" {p}"))
                .unwrap_or_default()
        ),
        "unpacking" => "unpacking and checking the backend pack".to_string(),
        "libraries" => "fetching NVIDIA's CUDA libraries".to_string(),
        "installed" => "backend pack installed".to_string(),
        "models" => format!(
            "fetching the OCR models{}",
            v.message
                .as_deref()
                .map(|m| format!(" ({m})"))
                .unwrap_or_default()
        ),
        other => other.to_string(),
    };
    let mut out = what;
    if let Some(p) = v.percent {
        out.push_str(&format!(": {p}%"));
    }
    if let (Some(d), Some(t)) = (v.done_bytes, v.total_bytes) {
        out.push_str(&format!(" ({} of {})", size(d), size(t)));
    }
    out
}

fn size(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1} GB", n as f64 / 1e9)
    } else {
        format!("{:.0} MB", n as f64 / 1e6)
    }
}

/// `2026-…Z  INFO downloading x.pt2: 120 / 900 MB (13%), 40.1 MB/s` → (name, bytes
/// done, bytes total, percent).
fn parse_model_download(line: &str) -> Option<(String, u64, u64, u32)> {
    let at = line.find("downloading ")?;
    let rest = &line[at + "downloading ".len()..];
    let (name, rest) = rest.split_once(": ")?;
    let (done, rest) = rest.split_once(" / ")?;
    let (total, rest) = rest.split_once(" MB (")?;
    let (pct, _) = rest.split_once("%)")?;
    Some((
        name.to_string(),
        done.trim().parse::<u64>().ok()? * 1_000_000,
        total.trim().parse::<u64>().ok()? * 1_000_000,
        pct.trim().parse::<f64>().ok()?.round().clamp(0.0, 100.0) as u32,
    ))
}

/// The child's own log lines carry a timestamp and level: keep the message.
fn strip_log_prefix(line: &str) -> &str {
    for level in [" INFO ", " WARN ", " ERROR ", " DEBUG "] {
        if let Some(i) = line.find(level)
            && i < 40
        {
            return line[i + level.len()..].trim_start();
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_become_the_state() {
        let mut t = Tracker::new("x".into());
        let v = t
            .line(
                r#"@@ocr-install {"stage":"downloading","variant":"cu130","pack":"p.tar.zst","done":0,"total":2000}"#,
                false,
            )
            .unwrap();
        assert_eq!(v.stage, "downloading");
        assert_eq!(v.variant.as_deref(), Some("cu130"));
        assert_eq!(v.percent, Some(0));
        let v = t
            .line(
                r#"@@ocr-install {"stage":"downloading","label":"p.tar.zst [1/1]","done":500,"total":2000}"#,
                false,
            )
            .unwrap();
        assert_eq!(v.percent, Some(25));
        assert_eq!((v.done_bytes, v.total_bytes), (Some(500), Some(2000)));
        assert_eq!(v.pack.as_deref(), Some("p.tar.zst"));
        // A new stage starts its own progress; models count files, not bytes.
        let v = t
            .line(
                r#"@@ocr-install {"stage":"models","label":"ppocr","done":1,"total":4,"unit":"files"}"#,
                false,
            )
            .unwrap();
        assert_eq!(
            (v.stage.as_str(), v.percent, v.total_bytes),
            ("models", Some(25), None)
        );
        // bunko-ocr's download line inside the models stage.
        let v = t
            .line(
                "2026-10-09T12:00:00.000Z  INFO downloading hayai.pt2: 450 / 900 MB (50%), 40.0 MB/s",
                false,
            )
            .unwrap();
        assert_eq!(v.percent, Some(50));
        assert_eq!(v.total_bytes, Some(900_000_000));
        assert_eq!(v.message.as_deref(), Some("file 2 of 4: hayai.pt2"));
        // Plain lines are only logged; a failure event is kept for the end.
        assert!(t.line("Installed /x/backends/torch-cpu", false).is_none());
        assert!(
            t.line(
                r#"@@ocr-install {"stage":"failed","message":"no space","needs_owner":true,"action":"Free disk space"}"#,
                false
            )
            .is_none()
        );
        assert_eq!(t.failure.as_deref(), Some("no space"));
        assert!(t.needs_owner);
        t.line("Error: something else", true);
        assert_eq!(t.last_error.as_deref(), Some("something else"));
    }

    #[test]
    fn stage_lines() {
        let v = OcrInstall {
            state: "running".into(),
            stage: "downloading".into(),
            percent: Some(42),
            done_bytes: Some(1_200_000_000),
            total_bytes: Some(2_900_000_000),
            variant: Some("cu130".into()),
            ..OcrInstall::default()
        };
        assert_eq!(
            stage_text(&v),
            "downloading the cu130 backend pack: 42% (1.2 GB of 2.9 GB)"
        );
        assert_eq!(
            strip_log_prefix("2026-10-09T12:00:00Z  INFO Fetching x"),
            "Fetching x"
        );
    }

    #[test]
    fn request_args() {
        let r = InstallRequest {
            variant: Some("cpu".into()),
            from: Some(PathBuf::from("/media/ocr")),
            no_models: true,
            force: false,
        };
        assert_eq!(
            r.args(),
            ["--variant", "cpu", "--from", "/media/ocr", "--no-models"]
        );
        let dir = tempfile::tempdir().unwrap();
        r.write(dir.path()).unwrap();
        assert_eq!(InstallRequest::read(dir.path()), Some(r));
    }

    /// A stand-in for `install-ocr`: prints `script` (sh) and exits with its status.
    #[cfg(unix)]
    fn fake_exe(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("fake-install-ocr");
        std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    fn library(dir: &Path) -> Who {
        let config = dir.join("config.yaml");
        std::fs::write(
            &config,
            format!("storage:\n  base_path: {}\n", dir.join("storage").display()),
        )
        .unwrap();
        Who::Library {
            config_path: config,
            backend: None,
        }
    }

    #[cfg(unix)]
    async fn wait_state(rx: &mut watch::Receiver<Option<OcrInstall>>, state: &str) -> OcrInstall {
        let found = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(v) = rx.borrow_and_update().clone()
                    && v.state == state
                {
                    return v;
                }
                rx.changed().await.unwrap();
            }
        })
        .await;
        found.unwrap_or_else(|_| panic!("no {state} state; last: {:?}", rx.borrow()))
    }

    /// The child's progress becomes the state while it runs; its success says done,
    /// bumps `finished` and drops the wizard's request.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_that_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(
            dir.path(),
            r#"echo '@@ocr-install {"stage":"downloading","variant":"cpu","pack":"p","done":0,"total":100}'
sleep 1
echo '@@ocr-install {"stage":"downloading","done":60,"total":100}'
echo 'Installed /x'
sleep 1
exit 0"#,
        );
        let inst = Installer::with_exe(library(dir.path()), exe);
        let backends = dir.path().join("storage").join("backends");
        InstallRequest {
            variant: Some("cpu".into()),
            ..InstallRequest::default()
        }
        .write(&backends)
        .unwrap();
        let mut rx = inst.subscribe();
        let mut done = BackgroundInstall::finished(&inst);
        inst.spawn();
        assert!(inst.0.running.load(Ordering::SeqCst));
        inst.spawn(); // a second start while one runs: nothing more
        let v = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if let Some(v) = rx.borrow_and_update().clone()
                    && v.percent == Some(60)
                {
                    return v;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert_eq!(v.state, "running");
        assert_eq!(v.variant.as_deref(), Some("cpu"));
        let v = wait_state(&mut rx, "done").await;
        assert_eq!(v.pack.as_deref(), Some("p"));
        tokio::time::timeout(Duration::from_secs(5), done.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            !InstallRequest::path(&backends).exists(),
            "request consumed"
        );
        assert!(!inst.0.running.load(Ordering::SeqCst));
    }

    /// A failure is the state and, through the control API, a problem; one that may
    /// pass by itself is retried.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_child_that_fails_raises_a_problem_and_retries() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        let exe = fake_exe(
            dir.path(),
            &format!(
                r#"echo x >> {}
echo '@@ocr-install {{"stage":"downloading","done":0,"total":100}}'
echo '@@ocr-install {{"stage":"failed","message":"network down","needs_owner":false}}'
echo 'Error: network down' >&2
exit 1"#,
                count.display()
            ),
        );
        let inst = Installer::with_exe(library(dir.path()), exe)
            .with_retry_after(vec![Duration::from_secs(3)]);
        let control = Control::new(bunko_control::ControlConfig::new(
            bunko_control::Role::Server,
            "box",
            "0",
            dir.path(),
        ));
        inst.attach_control(&control);
        let mut rx = inst.subscribe();
        inst.spawn();
        let v = wait_state(&mut rx, "failed").await;
        assert!(v.message.unwrap().starts_with("network down"));
        // Read before the automatic retry (3 s) starts another run.
        let status = control.status();
        assert_eq!(status.install.unwrap().state, "failed");
        assert_eq!(
            status.problems[0].kind.as_deref(),
            Some(Problem::KIND_OCR_INSTALL)
        );
        // The automatic retry: decide() finds the backend still missing.
        let retried = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let n = std::fs::read_to_string(&count)
                    .unwrap_or_default()
                    .lines()
                    .count();
                if n >= 2 {
                    return n;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(retried.is_ok(), "retried once");
        let v = wait_state(&mut rx, "failed").await;
        assert!(
            !v.message.unwrap().contains("trying again"),
            "no more retries"
        );
    }

    /// The backend was put in place by hand while the server's own install kept
    /// failing (`mokuro-bunko install-ocr` in a terminal, as the failure says to), or
    /// local OCR was turned off: the automatic retry finds nothing left to install.
    /// It drops the failure and says the install ended, so the OCR that waited for
    /// the backend starts.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retry_with_nothing_left_to_install_ends_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(
            dir.path(),
            r#"echo '@@ocr-install {"stage":"failed","message":"network down","needs_owner":false}'
exit 1"#,
        );
        let inst = Installer::with_exe(library(dir.path()), exe)
            .with_retry_after(vec![Duration::from_secs(2)]);
        let mut rx = inst.subscribe();
        let mut done = BackgroundInstall::finished(&inst);
        inst.spawn();
        wait_state(&mut rx, "failed").await;
        // The failed run's own end.
        tokio::time::timeout(Duration::from_secs(5), done.changed())
            .await
            .unwrap()
            .unwrap();
        // By the time of the retry nothing is left to install.
        std::fs::write(
            dir.path().join("config.yaml"),
            format!(
                "storage:\n  base_path: {}\nocr:\n  local_processing: false\n",
                dir.path().join("storage").display()
            ),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(20), done.changed())
            .await
            .expect("the retry that found nothing to install did not say it ended")
            .unwrap();
        assert!(rx.borrow().is_none(), "the failure is dropped");
    }

    /// The machine of the field report: its cu130 pack in place, the optional Vulkan pack
    /// missing (NVIDIA on Linux wants it as a benchmark candidate). None where this
    /// platform has no Vulkan pack.
    #[cfg(unix)]
    fn main_pack_in_place(dir: &Path, script: &str) -> Option<Installer> {
        use crate::cmd::install_ocr::tests::{fake_pack, nvidia};
        crate::hwdetect::vulkan_wanted("auto", &nvidia(), bunko_update::TARGET)?;
        let who = library(dir);
        fake_pack(&dir.join("storage").join("backends"), "cu130");
        Some(
            Installer::with_exe(who, fake_exe(dir, script)).with_probe(|t| Lack {
                need: install_ocr::need(t, &nvidia()),
                models: false,
                variant: "cu130".into(),
            }),
        )
    }

    #[cfg(unix)]
    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        let r = tokio::time::timeout(Duration::from_secs(20), async {
            while !ok() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(r.is_ok(), "timed out waiting for {what}");
    }

    #[cfg(unix)]
    fn lines(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// The optional Vulkan pack cannot be fetched (its release is unpublished) while
    /// the cu130 pack serves. The machine never says "installing" (a library would
    /// give it no work), the failure is a warning, and neither a reconnect nor another
    /// registration fetches it again: only the owner's retry does.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_optional_pack_that_cannot_be_fetched_never_holds_the_machine() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let Some(inst) = main_pack_in_place(
            dir.path(),
            &format!(
                r#"echo "$@" >> {}
echo '@@ocr-install {{"stage":"checking","variant":"vulkan"}}'
echo '@@ocr-install {{"stage":"failed","message":"https://example.com/release.json: no release has been published yet (https://example.com/release.json does not exist)","needs_owner":false,"lasting":true}}'
exit 1"#,
                calls.display()
            ),
        ) else {
            return;
        };
        let control = Control::new(bunko_control::ControlConfig::new(
            bunko_control::Role::Processor,
            "box",
            "0",
            dir.path(),
        ));
        inst.attach_control(&control);
        let rx = inst.subscribe();
        let finished = BackgroundInstall::finished(&inst);
        // The first registration.
        inst.start_if_needed();
        assert!(
            !rx.borrow().as_ref().is_some_and(|v| v.running()),
            "a machine whose OCR backend works says it is installing: {:?}",
            rx.borrow()
        );
        until("the Vulkan fetch to fail", || {
            control
                .status()
                .problems
                .iter()
                .any(|p| p.severity == Severity::Warn && p.text.contains("vulkan"))
        })
        .await;
        assert_eq!(lines(&calls).len(), 1);
        assert!(
            lines(&calls)[0].contains("--variant vulkan"),
            "{:?}",
            lines(&calls)
        );
        // Nothing a library or a processor session reacts to: no state, no end.
        assert!(rx.borrow().is_none(), "{:?}", rx.borrow());
        assert!(!rx.has_changed().unwrap());
        assert!(!finished.has_changed().unwrap());
        assert!(
            !control
                .status()
                .problems
                .iter()
                .any(|p| p.severity == Severity::Fail),
            "{:?}",
            control.status().problems
        );
        // An unpublished release waits a day, kept beside the packs.
        let kept =
            ExtraAttempt::read(&dir.path().join("storage").join("backends"), "vulkan").unwrap();
        assert_eq!(kept.failures, 1);
        assert!(kept.next_at >= unix_now() + 23 * 3600, "{kept:?}");
        // Reconnects and registrations: no new fetch.
        for _ in 0..3 {
            inst.start_if_needed();
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(lines(&calls).len(), 1, "fetched again at a registration");
        // A restart remembers it (the backends directory keeps the attempt).
        let again = Installer::with_exe(library(dir.path()), dir.path().join("fake-install-ocr"))
            .with_probe(|t| Lack {
                need: install_ocr::need(t, &crate::cmd::install_ocr::tests::nvidia()),
                models: false,
                variant: "cu130".into(),
            });
        again.start_if_needed();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(lines(&calls).len(), 1, "fetched again after a restart");
        // A settings change (local processing turned on, a backend saved) is no retry:
        // it waits as a registration does.
        assert!(!inst.start_for_settings().unwrap());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(lines(&calls).len(), 1, "fetched again at a settings change");
        // The owner's retry fetches it at once, and says no install runs: nothing may
        // wait for the end of a fetch on the side.
        assert!(!inst.start_manual().unwrap());
        assert!(inst.fetching_extra());
        until("the owner's retry", || lines(&calls).len() == 2).await;
        assert!(!rx.borrow().as_ref().is_some_and(|v| v.running()));
    }

    /// Local processing turned on (or another backend saved) on a server whose backend
    /// is in place and whose optional Vulkan pack is missing. The start answered "an
    /// install runs" for the fetch on the side, so the server waited for an end that a
    /// failed fetch never announces, and its own OCR stayed stopped until a restart.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn turning_ocr_on_never_waits_for_the_optional_pack() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let Some(inst) = main_pack_in_place(
            dir.path(),
            &format!(
                r#"echo "$@" >> {}
echo '@@ocr-install {{"stage":"failed","message":"network down","needs_owner":false}}'
exit 1"#,
                calls.display()
            ),
        ) else {
            return;
        };
        let control = Control::new(bunko_control::ControlConfig::new(
            bunko_control::Role::Server,
            "box",
            "0",
            dir.path(),
        ));
        inst.attach_control(&control);
        let finished = BackgroundInstall::finished(&inst);
        // What `apply_config` and the backend preference's save call.
        assert!(
            !BackgroundInstall::start_for_settings(&inst).unwrap(),
            "the server would wait for the optional pack"
        );
        until("the fetch to fail", || {
            control
                .status()
                .problems
                .iter()
                .any(|p| p.severity == Severity::Warn && p.text.contains("vulkan"))
        })
        .await;
        assert_eq!(lines(&calls).len(), 1);
        assert!(!finished.has_changed().unwrap());
        // The admin panel's Retry is the same answer.
        assert!(!BackgroundInstall::start(&inst).unwrap());
    }

    /// An installer whose machine has only the CPU libtorch pack and wants the Vulkan
    /// pack as its one GPU route (`ocr.backend: vulkan`; AMD or Intel on Windows). The
    /// probe reads the Vulkan pack from the backends directory.
    #[cfg(unix)]
    fn vulkan_route(dir: &Path, script: &str) -> Installer {
        let who = library(dir);
        crate::cmd::install_ocr::tests::fake_pack(&dir.join("storage").join("backends"), "cpu");
        Installer::with_exe(who, fake_exe(dir, script)).with_probe(|t| {
            let have = install_ocr::already_fits(&t.backends_dirs(), "cpu").unwrap_or_default();
            let need =
                if install_ocr::already_fits(&t.backends_dirs(), install_ocr::VULKAN).is_some() {
                    Need::Satisfied(have)
                } else {
                    Need::Extra {
                        variant: install_ocr::VULKAN.into(),
                        reason: "ocr.backend: vulkan".into(),
                        have,
                        route: true,
                    }
                };
            Lack {
                need,
                models: false,
                variant: "cpu".into(),
            }
        })
    }

    /// Where the Vulkan pack is the machine's one GPU route, its first fetch is waited
    /// for as an install is: the machine says it is installing, so it is not
    /// benchmarked on the CPU meanwhile. A failure ends that wait (the machine runs on
    /// the CPU), and the next registration does not fetch again.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_vulkan_only_machine_waits_for_its_first_fetch_once() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let inst = vulkan_route(
            dir.path(),
            &format!(
                r#"echo "$@" >> {}
sleep 0.3
echo '@@ocr-install {{"stage":"failed","message":"no release has been published yet","needs_owner":false,"lasting":true}}'
exit 1"#,
                calls.display()
            ),
        );
        let rx = inst.subscribe();
        let mut finished = BackgroundInstall::finished(&inst);
        assert!(
            inst.start_if_needed(),
            "the GPU route's fetch is not waited for"
        );
        assert!(rx.borrow().as_ref().is_some_and(|v| v.running()));
        tokio::time::timeout(Duration::from_secs(10), finished.changed())
            .await
            .expect("the wait never ended")
            .unwrap();
        assert!(rx.borrow().is_none(), "{:?}", rx.borrow());
        assert!(!BackgroundInstall::running(&inst));
        // The registration that follows runs on the CPU and fetches nothing.
        assert!(!inst.start_if_needed());
        assert!(!inst.start_for_settings().unwrap());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(lines(&calls).len(), 1);
        assert!(!finished.has_changed().unwrap());
        assert!(rx.borrow().is_none());
    }

    /// The GPU route's fetch that works ends like an install: done, and the machine
    /// registers again with its Vulkan runtimes.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_vulkan_only_machine_that_gets_its_pack_ends_as_an_install() {
        let dir = tempfile::tempdir().unwrap();
        let pack = dir.path().join("storage/backends/vulkan-test");
        let json = serde_json::json!({
            "format": 1, "name": "vulkan-test", "variant": "vulkan", "torch": "",
            "target": bunko_update::TARGET, "os": "linux", "arch": "x86_64", "abi": 1,
            "library": "", "bunko_version": bunko_core::VERSION, "files": []
        });
        let inst = vulkan_route(
            dir.path(),
            &format!(
                r#"mkdir -p {pack}
echo '{json}' > {pack}/pack.json
echo '@@ocr-install {{"stage":"installed","variant":"vulkan","pack":"vulkan-test"}}'
exit 0"#,
                pack = pack.display(),
            ),
        );
        let mut rx = inst.subscribe();
        let mut finished = BackgroundInstall::finished(&inst);
        assert!(BackgroundInstall::start_for_settings(&inst).unwrap());
        let v = wait_state(&mut rx, "done").await;
        assert_eq!(v.variant.as_deref(), Some("vulkan"));
        tokio::time::timeout(Duration::from_secs(5), finished.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(!BackgroundInstall::running(&inst));
        assert!(!inst.start_if_needed());
    }

    /// The optional pack installed another way after a failed fetch (`install-ocr
    /// --variant vulkan` by hand): the next look drops the warning and the recorded
    /// failure, and ends as a fetch that worked, so a processor registers again with
    /// the new runtimes. The warning used to stay until a restart.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_optional_pack_installed_by_hand_clears_its_warning() {
        let dir = tempfile::tempdir().unwrap();
        let Some(inst) = main_pack_in_place(
            dir.path(),
            r#"echo '@@ocr-install {"stage":"failed","message":"network down","needs_owner":false}'
exit 1"#,
        ) else {
            return;
        };
        let control = Control::new(bunko_control::ControlConfig::new(
            bunko_control::Role::Processor,
            "box",
            "0",
            dir.path(),
        ));
        inst.attach_control(&control);
        let mut rx = inst.subscribe();
        let mut finished = BackgroundInstall::finished(&inst);
        let warned = || {
            control
                .status()
                .problems
                .iter()
                .any(|p| p.text.contains("optional vulkan"))
        };
        inst.start_if_needed();
        until("the fetch to fail", warned).await;
        let backends = dir.path().join("storage").join("backends");
        assert!(backends.join(ExtraAttempt::FILE).is_file());
        assert!(!finished.has_changed().unwrap());
        // `install-ocr --variant vulkan` by hand.
        crate::cmd::install_ocr::tests::fake_pack(&backends, install_ocr::VULKAN);
        inst.start_if_needed();
        assert!(!warned(), "{:?}", control.status().problems);
        assert!(!backends.join(ExtraAttempt::FILE).exists());
        assert_eq!(
            wait_state(&mut rx, "done").await.variant.as_deref(),
            Some("vulkan")
        );
        tokio::time::timeout(Duration::from_secs(5), finished.changed())
            .await
            .unwrap()
            .unwrap();
        // Once: nothing stands any more.
        inst.start_if_needed();
        assert!(!finished.has_changed().unwrap());
    }

    /// The optional pack fetched: the machine registers again once (its runtimes are
    /// new benchmark candidates), and then nothing is left to fetch.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_optional_pack_that_installs_ends_once() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let pack = dir.path().join("storage/backends/vulkan-test");
        let json = serde_json::json!({
            "format": 1, "name": "vulkan-test", "variant": "vulkan", "torch": "",
            "target": bunko_update::TARGET, "os": "linux", "arch": "x86_64", "abi": 1,
            "library": "", "bunko_version": bunko_core::VERSION, "files": []
        });
        let Some(inst) = main_pack_in_place(
            dir.path(),
            &format!(
                r#"echo "$@" >> {calls}
mkdir -p {pack}
echo '{json}' > {pack}/pack.json
echo '@@ocr-install {{"stage":"installed","variant":"vulkan","pack":"vulkan-test"}}'
exit 0"#,
                calls = calls.display(),
                pack = pack.display(),
            ),
        ) else {
            return;
        };
        let mut rx = inst.subscribe();
        let mut finished = BackgroundInstall::finished(&inst);
        inst.start_if_needed();
        assert!(!rx.borrow().as_ref().is_some_and(|v| v.running()));
        let v = wait_state(&mut rx, "done").await;
        assert_eq!(v.variant.as_deref(), Some("vulkan"));
        tokio::time::timeout(Duration::from_secs(5), finished.changed())
            .await
            .unwrap()
            .unwrap();
        // The registration that follows finds nothing to fetch.
        assert!(!inst.start_if_needed());
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(lines(&calls).len(), 1);
        assert!(!finished.has_changed().unwrap());
    }

    /// The backend's install leaves the optional pack out (nothing should wait for
    /// it), and the optional pack's fetch follows once that install is done, even with
    /// no registration to start it (a library server's own OCR).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_optional_pack_follows_the_backend_install() {
        use crate::cmd::install_ocr::tests::nvidia;
        if crate::hwdetect::vulkan_wanted("auto", &nvidia(), bunko_update::TARGET).is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let backends = dir.path().join("storage").join("backends");
        let pack = backends.join("torch-cu130-test");
        let json = serde_json::json!({
            "format": 1, "name": "torch-cu130-test", "variant": "cu130", "torch": "2.13.0",
            "target": bunko_update::TARGET, "os": "linux", "arch": "x86_64", "abi": 1,
            "library": "", "bunko_version": bunko_core::VERSION, "files": []
        });
        let exe = fake_exe(
            dir.path(),
            &format!(
                r#"echo "extras=${{MOKURO_INSTALL_NO_EXTRAS:-}} $@" >> {calls}
case "$*" in
*vulkan*) echo '@@ocr-install {{"stage":"failed","message":"network down","needs_owner":false}}'; exit 1;;
esac
mkdir -p {pack}
echo '{json}' > {pack}/pack.json
echo '@@ocr-install {{"stage":"installed","variant":"cu130","pack":"torch-cu130-test"}}'
exit 0"#,
                calls = calls.display(),
                pack = pack.display(),
            ),
        );
        let inst = Installer::with_exe(library(dir.path()), exe).with_probe(|t| Lack {
            need: install_ocr::need(t, &nvidia()),
            models: false,
            variant: "cu130".into(),
        });
        let mut rx = inst.subscribe();
        assert!(inst.start_if_needed(), "the backend is missing");
        assert!(rx.borrow().as_ref().is_some_and(|v| v.running()));
        wait_state(&mut rx, "done").await;
        until("the optional pack's fetch", || lines(&calls).len() == 2).await;
        let calls = lines(&calls);
        assert!(
            calls[0].starts_with("extras=1 ") && !calls[0].contains("vulkan"),
            "{calls:?}"
        );
        assert!(
            calls[1].starts_with("extras= ") && calls[1].contains("--variant vulkan"),
            "{calls:?}"
        );
        // Its failure leaves the finished install as it is.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(rx.borrow().as_ref().unwrap().state, OcrInstall::DONE);
    }

    /// A fetch of the optional pack that ends well but leaves no usable pack is a
    /// failure: an end would register the machine again, and that registration would
    /// fetch again, without end.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_optional_pack_that_is_still_missing_after_its_fetch_does_not_loop() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let Some(inst) = main_pack_in_place(
            dir.path(),
            &format!(
                "echo \"$@\" >> {}\necho 'Warning: the Vulkan backend pack was not installed'\nexit 0",
                calls.display()
            ),
        ) else {
            return;
        };
        let rx = inst.subscribe();
        let finished = BackgroundInstall::finished(&inst);
        inst.start_if_needed();
        until("the fetch", || lines(&calls).len() == 1).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        for _ in 0..3 {
            inst.start_if_needed();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(lines(&calls).len(), 1);
        assert!(rx.borrow().is_none(), "{:?}", rx.borrow());
        assert!(!finished.has_changed().unwrap());
    }

    #[test]
    fn optional_pack_fetches_wait_longer_after_each_failure() {
        let min = |m: u64| Duration::from_secs(m * 60);
        assert_eq!(extra_wait(1, false), min(5));
        assert_eq!(extra_wait(2, false), min(15));
        assert_eq!(extra_wait(3, false), min(60));
        assert_eq!(extra_wait(4, false), EXTRA_RETRY_DAILY);
        assert_eq!(extra_wait(40, false), EXTRA_RETRY_DAILY);
        // Nothing published (or nothing the machine can do): a day at once.
        assert_eq!(extra_wait(1, true), EXTRA_RETRY_DAILY);
        // Another release starts afresh.
        let dir = tempfile::tempdir().unwrap();
        let a = ExtraAttempt {
            variant: "vulkan".into(),
            bunko_version: bunko_core::VERSION.into(),
            target: bunko_update::TARGET.into(),
            failures: 2,
            next_at: 1,
            message: "x".into(),
        };
        a.write(dir.path());
        assert_eq!(ExtraAttempt::read(dir.path(), "vulkan"), Some(a.clone()));
        assert_eq!(ExtraAttempt::read(dir.path(), "cpu"), None);
        ExtraAttempt {
            bunko_version: "0.0.1".into(),
            ..a
        }
        .write(dir.path());
        assert_eq!(ExtraAttempt::read(dir.path(), "vulkan"), None);
    }

    #[test]
    fn failures_and_missing_backends_are_problems() {
        assert!(problems_of(None).is_empty());
        let running = OcrInstall {
            state: "running".into(),
            ..OcrInstall::default()
        };
        assert!(problems_of(Some(&running)).is_empty());
        let failed = OcrInstall {
            state: "failed".into(),
            message: Some("network down".into()),
            ..OcrInstall::default()
        };
        let p = problems_of(Some(&failed));
        assert_eq!(p[0].severity, Severity::Fail);
        assert_eq!(p[0].text, "OCR backend install failed: network down");
        assert_eq!(p[0].kind.as_deref(), Some("ocr-install"));
        let missing = OcrInstall {
            state: OcrInstall::MISSING.into(),
            message: Some("No OCR backend installed".into()),
            ..OcrInstall::default()
        };
        assert_eq!(problems_of(Some(&missing)).len(), 1);
    }
}
