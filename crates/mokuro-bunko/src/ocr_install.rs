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
        *self.0.control.lock() = Some(control.clone());
        let me = self.clone();
        control.set_install_trigger(Arc::new(move || me.start_manual()));
        let now = self.0.view.borrow().clone();
        self.publish(now);
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
        let (auto, note) = install_ocr::auto_install_setting(|n| std::env::var(n).ok());
        if let Some(n) = note {
            info!("{n}");
        }
        match self.decide() {
            Ok(Decision::Install {
                variant,
                reason,
                request,
            }) if auto => {
                info!(
                    "OCR backend: the {variant} pack is not installed ({reason}); installing it in the background{}",
                    if request {
                        " with the setup's choices"
                    } else {
                        ""
                    }
                );
                self.spawn();
                true
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
            Ok(Decision::Nothing(why)) => {
                tracing::debug!("OCR backend: {why}");
                false
            }
            Err(e) => {
                warn!("OCR backend: could not check whether it is installed: {e}");
                false
            }
        }
    }

    /// Start (or retry) by hand: whatever the automatic-install setting says.
    pub fn start_manual(&self) -> Result<bool, String> {
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
            Decision::Nothing(why) => {
                // Nothing to do any more (installed meanwhile, or OCR turned off).
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
                info!("OCR backend: nothing to install ({why})");
                Ok(false)
            }
        }
    }

    fn decide(&self) -> Result<Decision, String> {
        let target = self.target()?;
        let backends = target.backends_dir();
        let request = InstallRequest::read(&backends);
        let hw = crate::hwdetect::detect();
        match install_ocr::need(&target, &hw) {
            Need::Off(why) => Ok(Decision::Nothing(format!("local OCR is off: {why}"))),
            Need::Install { variant, reason } => Ok(Decision::Install {
                variant: request
                    .as_ref()
                    .and_then(|r| r.variant.clone())
                    .filter(|v| v != "auto")
                    .unwrap_or(variant),
                reason,
                request: request.is_some(),
            }),
            // The wizard asked for something the pack in place may not be (another
            // variant, a reinstall): the child decides.
            Need::Satisfied(_) if request.is_some() => Ok(Decision::Install {
                variant: request
                    .as_ref()
                    .and_then(|r| r.variant.clone())
                    .unwrap_or_else(|| "auto".into()),
                reason: "the setup's OCR choices".into(),
                request: true,
            }),
            Need::Satisfied(why) => Ok(Decision::Nothing(why)),
        }
    }

    /// The child's command line: `[--config X] install-ocr --if-needed [--processor] ...`.
    fn command(&self) -> Result<ChildCommand, String> {
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
        if let Some(r) = InstallRequest::read(&backends) {
            args.extend(r.args());
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
            let outcome = me.run_child(since).await;
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

    async fn run_child(&self, since: String) -> Outcome {
        let (args, envs, backends) = match self.command() {
            Ok(c) => c,
            Err(e) => {
                return Outcome::Failed {
                    message: e,
                    needs_owner: true,
                    action: None,
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
                };
            }
        };
        let tracker = Arc::new(Mutex::new(Tracker::new(since)));
        let out = child
            .stdout
            .take()
            .map(|s| self.pump(s, tracker.clone(), false));
        let err = child
            .stderr
            .take()
            .map(|s| self.pump(s, tracker.clone(), true));
        let status = child.wait().await;
        for h in [out, err].into_iter().flatten() {
            let _ = h.await;
        }
        let t = tracker.lock();
        match status {
            Ok(s) if s.success() => {
                let _ = std::fs::remove_file(InstallRequest::path(&backends));
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
            },
            Err(e) => Outcome::Failed {
                message: format!("install-ocr: {e}"),
                needs_owner: false,
                action: None,
            },
        }
    }

    fn pump<R: AsyncRead + Unpin + Send + 'static>(
        &self,
        stream: R,
        tracker: Arc<Mutex<Tracker>>,
        stderr: bool,
    ) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let changed = {
                    let mut t = tracker.lock();
                    t.line(&line, stderr)
                };
                if let Some(view) = changed {
                    me.publish(Some(view));
                }
            }
        })
    }

    fn finish(&self, outcome: Outcome, epoch: u64) {
        // The libtorch backend may be looked for again in this process.
        bunko_engines::torch::forget_failure();
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
    }

    /// An automatic retry (the retries count goes on).
    fn start_retry(&self) -> Result<bool, String> {
        match self.decide()? {
            Decision::Install { .. } => {
                self.spawn();
                Ok(true)
            }
            Decision::Nothing(_) => {
                self.publish(None);
                Ok(false)
            }
        }
    }

    fn publish(&self, view: Option<OcrInstall>) {
        self.0.view.send_replace(view.clone());
        if let Some(c) = self.0.control.lock().as_ref() {
            c.set_install(view.clone(), problems_of(view.as_ref()));
        }
    }
}

/// The installer has something to say about the backend (installing, failed, missing):
/// the doctor's "no backend pack" problem would only repeat it.
pub fn speaks_for_backend(installer: Option<&Installer>) -> bool {
    installer
        .and_then(|i| i.0.view.borrow().clone())
        .is_some_and(|v| v.state != OcrInstall::DONE)
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

    fn finished(&self) -> watch::Receiver<u64> {
        self.0.finished.subscribe()
    }
}

/// The child's arguments, environment, and the backends directory.
type ChildCommand = (Vec<String>, Vec<(String, String)>, PathBuf);

enum Decision {
    Install {
        variant: String,
        reason: String,
        request: bool,
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
    },
}

fn now() -> String {
    chrono_like_now()
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
        info!("OCR install: {}", strip_log_prefix(text));
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
            info!("OCR install: {}", stage_text(&self.view));
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
            info!("OCR install: {}", stage_text(&self.view));
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
            .with_retry_after(vec![Duration::from_millis(200)]);
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
