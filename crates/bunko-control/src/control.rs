//! [`Control`]: one running instance as the control API sees it — the state the
//! status is built from, the pause, the stop switch. The instance keeps it current
//! (connection, queue size, problems); the HTTP layer only reads it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use crate::activity::{Activity, Changes};
use crate::pause::{PauseCtl, PauseError};
use crate::types::{
    BackendView, LibraryView, PauseBody, PauseMode, Problem, Role, State, Stats, Status,
    UpdateView, Urls,
};

/// GPU busy (%) and CPU cores busy right now, from whatever the platform offers.
pub trait LoadProbe: Send + Sync {
    fn sample(&self) -> (Option<f64>, Option<f64>);
}

/// Where a processor stands with its library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPhase {
    Connecting,
    Connected,
    /// Unreachable or dropped; it retries by itself.
    Disconnected,
    /// The library refused the login: nothing will work until someone acts.
    Refused,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ControlError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotHere(String),
    #[error(
        "this instance was not started by the tray; stop it with its service manager (or Ctrl+C where it runs)"
    )]
    NotManaged,
}

impl From<PauseError> for ControlError {
    fn from(e: PauseError) -> Self {
        ControlError::BadRequest(e.to_string())
    }
}

/// What an instance says about itself.
#[derive(Debug, Clone)]
pub struct ControlConfig {
    pub role: Role,
    /// The machine's name (`processor.name`, the host name).
    pub name: String,
    pub version: String,
    /// Where `.control.json`, `.pause.json` and `.stats.json` live.
    pub storage: PathBuf,
    /// Started by the tray ([`crate::types::MANAGED_ENV`]): `stop` is allowed.
    pub managed: bool,
    /// This instance reads OCR itself (a processor; a full-build server): pause and
    /// activity apply.
    pub ocr: bool,
}

impl ControlConfig {
    pub fn new(
        role: Role,
        name: impl Into<String>,
        version: impl Into<String>,
        storage: &Path,
    ) -> Self {
        ControlConfig {
            role,
            name: name.into(),
            version: version.into(),
            storage: storage.to_path_buf(),
            managed: managed_from_env(),
            ocr: role != Role::Gui,
        }
    }
}

/// [`crate::types::MANAGED_ENV`] is set to something other than empty / `0`.
pub fn managed_from_env() -> bool {
    std::env::var(crate::types::MANAGED_ENV)
        .map(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(false)
}

/// `MOKURO_CONTROL=off|0|false` turns the listener off.
pub fn enabled_from_env() -> bool {
    !std::env::var(crate::types::ENABLE_ENV)
        .map(|v| {
            matches!(
                v.to_ascii_lowercase().as_str(),
                "off" | "0" | "false" | "no"
            )
        })
        .unwrap_or(false)
}

struct LinkState {
    phase: LinkPhase,
    library: LibraryView,
}

struct Inner {
    config: ControlConfig,
    changes: Changes,
    pause: Option<PauseCtl>,
    activity: Option<Arc<Activity>>,
    link: Mutex<LinkState>,
    backend: Mutex<BackendView>,
    problems: Mutex<Vec<Problem>>,
    /// The automatic update's state and the problems it raised (kept apart from the
    /// doctor's, which `set_problems` replaces wholesale).
    update: Mutex<(Option<UpdateView>, Vec<Problem>)>,
    urls: Mutex<Urls>,
    load: Mutex<Option<Arc<dyn LoadProbe>>>,
    stop: Mutex<Option<CancellationToken>>,
}

/// The handle every part of an instance shares (cheap to clone).
#[derive(Clone)]
pub struct Control(Arc<Inner>);

impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Control")
            .field("role", &self.0.config.role)
            .field("storage", &self.0.config.storage)
            .finish()
    }
}

impl Control {
    /// Must run inside a tokio runtime (the pause timer and the change relay start here).
    pub fn new(config: ControlConfig) -> Control {
        let changes = Changes::default();
        let (pause, activity) = if config.ocr {
            (
                Some(PauseCtl::open(&config.storage)),
                Some(Activity::new(&config.storage, changes.clone())),
            )
        } else {
            (None, None)
        };
        if let Some(p) = &pause {
            let mut rx = p.subscribe();
            let changes = changes.clone();
            tokio::spawn(async move {
                while rx.changed().await.is_ok() {
                    changes.bump();
                }
            });
        }
        let phase = match config.role {
            Role::Processor => LinkPhase::Connecting,
            _ => LinkPhase::Connected,
        };
        Control(Arc::new(Inner {
            urls: Mutex::new(Urls {
                dashboard: "/app/dashboard".into(),
                library: None,
                logs_dir: Some(config.storage.join("logs").display().to_string()),
            }),
            config,
            changes,
            pause,
            activity,
            link: Mutex::new(LinkState {
                phase,
                library: LibraryView::default(),
            }),
            backend: Mutex::new(BackendView::default()),
            problems: Mutex::new(Vec::new()),
            update: Mutex::new((None, Vec::new())),
            load: Mutex::new(None),
            stop: Mutex::new(None),
        }))
    }

    pub fn config(&self) -> &ControlConfig {
        &self.0.config
    }

    pub fn role(&self) -> Role {
        self.0.config.role
    }

    pub fn storage(&self) -> &Path {
        &self.0.config.storage
    }

    pub fn changes(&self) -> &Changes {
        &self.0.changes
    }

    /// The pause the processor runtime obeys (None: nothing here to pause).
    pub fn pause_ctl(&self) -> Option<&PauseCtl> {
        self.0.pause.as_ref()
    }

    /// What the processor runtime reports into.
    pub fn activity(&self) -> Option<&Arc<Activity>> {
        self.0.activity.as_ref()
    }

    // --- what the instance keeps current ----------------------------------------------

    /// The library side of the status (url, connected, queue size).
    pub fn set_library(&self, library: LibraryView) {
        let mut l = self.0.link.lock();
        if l.library != library {
            l.library = library;
            drop(l);
            self.0.changes.bump();
        }
    }

    /// A processor's connection: the phase and, when not connected, why.
    pub fn set_link(&self, phase: LinkPhase, url: Option<&str>, error: Option<&str>) {
        let mut l = self.0.link.lock();
        let mut library = l.library.clone();
        if let Some(u) = url {
            library.url = Some(u.to_string());
        }
        library.connected = phase == LinkPhase::Connected;
        library.error = error.map(str::to_string);
        if l.phase != phase || l.library != library {
            l.phase = phase;
            l.library = library;
            drop(l);
            self.0.changes.bump();
        }
    }

    pub fn set_backend(&self, backend: BackendView) {
        *self.0.backend.lock() = backend;
        self.0.changes.bump();
    }

    /// The backend pack in use (`torch-rocm-2.13.0`), None without one.
    pub fn set_backend_pack(&self, pack: Option<String>) {
        let mut b = self.0.backend.lock();
        if b.pack != pack {
            b.pack = pack;
            drop(b);
            self.0.changes.bump();
        }
    }

    /// The devices this machine described (each registration / local start): the
    /// status's `backend.devices` and the labels of `current[].device`.
    pub fn set_devices(&self, devices: &[bunko_proto::Device]) {
        if let Some(a) = self.activity() {
            a.set_devices(devices);
        }
        let lines: Vec<String> = devices.iter().map(device_line).collect();
        let mut b = self.0.backend.lock();
        if b.devices != lines {
            b.devices = lines;
            drop(b);
            self.0.changes.bump();
        }
    }

    pub fn set_problems(&self, problems: Vec<Problem>) {
        let mut p = self.0.problems.lock();
        if *p != problems {
            *p = problems;
            drop(p);
            self.0.changes.bump();
        }
    }

    /// The automatic update's state (`status.update`).
    pub fn set_update(&self, view: Option<UpdateView>) {
        let mut u = self.0.update.lock();
        if u.0 != view {
            u.0 = view;
            drop(u);
            self.0.changes.bump();
        }
    }

    pub fn update_view(&self) -> Option<UpdateView> {
        self.0.update.lock().0.clone()
    }

    /// The problems the automatic update raised (listed after the doctor's).
    pub fn set_update_problems(&self, problems: Vec<Problem>) {
        let mut u = self.0.update.lock();
        if u.1 != problems {
            u.1 = problems;
            drop(u);
            self.0.changes.bump();
        }
    }

    pub fn set_urls(&self, urls: Urls) {
        *self.0.urls.lock() = urls;
        self.0.changes.bump();
    }

    /// The library's address for `urls.library` (and `library.url` when unset).
    pub fn set_library_url(&self, url: &str) {
        self.0.urls.lock().library = Some(url.to_string());
        let mut l = self.0.link.lock();
        if l.library.url.is_none() {
            l.library.url = Some(url.to_string());
        }
        drop(l);
        self.0.changes.bump();
    }

    pub fn set_load_probe(&self, probe: Arc<dyn LoadProbe>) {
        *self.0.load.lock() = Some(probe);
    }

    /// What `POST /control/stop` cancels (only honoured when managed).
    pub fn set_stop(&self, stop: CancellationToken) {
        *self.0.stop.lock() = Some(stop);
    }

    // --- what the API does ------------------------------------------------------------

    pub fn status(&self) -> Status {
        let c = &self.0.config;
        let pause = self.0.pause.as_ref().map(|p| p.view()).unwrap_or_default();
        let (phase, library) = {
            let l = self.0.link.lock();
            (l.phase, l.library.clone())
        };
        let activity = self.0.activity.as_deref();
        let current = activity.map(|a| a.current()).unwrap_or_default();
        let held = activity.map(|a| a.held()).unwrap_or(0);
        let sessions = activity.map(|a| a.sessions()).unwrap_or(0);
        let state = if c.role == Role::Gui {
            State::Setup
        } else if phase == LinkPhase::Refused {
            State::Error
        } else if pause.mode.is_some() {
            if held > 0 {
                State::Pausing
            } else {
                State::Paused
            }
        } else if phase == LinkPhase::Connecting {
            State::Connecting
        } else if phase == LinkPhase::Disconnected {
            State::Disconnected
        } else if held > 0 || sessions > 0 {
            State::Working
        } else {
            State::Idle
        };
        let (today, total) = activity.map(|a| a.counts()).unwrap_or_default();
        let probe = self.0.load.lock().clone();
        let (gpu, cpu) = probe.map(|p| p.sample()).unwrap_or((None, None));
        Status {
            role: c.role,
            version: c.version.clone(),
            name: c.name.clone(),
            state,
            pause,
            library,
            current,
            stats: Stats {
                today,
                total,
                rate_pages_per_minute: activity.map(|a| a.pages_last_minute()).unwrap_or(0.0),
                gpu_busy_percent: gpu,
                cpu_cores_busy: cpu,
            },
            backend: self.0.backend.lock().clone(),
            problems: {
                let mut p = self.0.problems.lock().clone();
                p.extend(self.0.update.lock().1.iter().cloned());
                p
            },
            urls: self.0.urls.lock().clone(),
            managed: c.managed,
            can_pause: self.0.pause.is_some(),
            update: self.0.update.lock().0.clone(),
        }
    }

    pub fn pause(&self, body: &PauseBody) -> Result<Status, ControlError> {
        let ctl = self.0.pause.as_ref().ok_or_else(|| {
            ControlError::NotHere(match self.0.config.role {
                Role::Gui => "nothing is running here to pause".into(),
                _ => "this server reads no OCR itself (lite build or local processing off); pause its processors instead".into(),
            })
        })?;
        ctl.pause(body.mode, body.until.as_deref(), body.reason.as_deref())?;
        Ok(self.status())
    }

    /// Shorthand for [`Control::pause`] from code (the tray presets, tests).
    pub fn pause_mode(&self, mode: PauseMode, until: Option<&str>) -> Result<Status, ControlError> {
        self.pause(&PauseBody {
            mode,
            until: until.map(str::to_string),
            reason: None,
        })
    }

    pub fn resume(&self) -> Result<Status, ControlError> {
        let ctl = self
            .0
            .pause
            .as_ref()
            .ok_or_else(|| ControlError::NotHere("nothing is running here to resume".into()))?;
        ctl.resume();
        Ok(self.status())
    }

    /// `POST /control/stop`: only an instance the tray started stops this way.
    pub fn stop(&self) -> Result<(), ControlError> {
        if !self.0.config.managed {
            return Err(ControlError::NotManaged);
        }
        match self.0.stop.lock().as_ref() {
            Some(t) => {
                tracing::info!("Stopping: asked by the tray (POST /control/stop)");
                t.cancel();
                Ok(())
            }
            None => Err(ControlError::NotHere(
                "this instance cannot be stopped from here".into(),
            )),
        }
    }
}

/// `gpu:0 AMD Radeon RX 9070 XT (gfx1201, 16 GB) · rocm`, `cpu CPU (32 threads) · x86_64`:
/// the id, the label, then the provider and architecture the label does not already say
/// (a CPU's provider is `cpu`, never worth repeating).
pub fn device_line(d: &bunko_proto::Device) -> String {
    let extra: Vec<&str> = d
        .provider
        .iter()
        .filter(|p| p.as_str() != "cpu")
        .chain(d.arch.iter())
        .map(String::as_str)
        .filter(|x| !d.label.contains(x))
        .collect();
    if extra.is_empty() {
        format!("{} {}", d.id, d.label)
    } else {
        format!("{} {} · {}", d.id, d.label, extra.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, label: &str, provider: &str, arch: &str) -> bunko_proto::Device {
        bunko_proto::Device {
            id: id.into(),
            label: label.into(),
            formats: Vec::new(),
            provider: Some(provider.into()),
            arch: Some(arch.into()),
        }
    }

    #[test]
    fn device_lines_say_each_thing_once() {
        assert_eq!(
            device_line(&device(
                "gpu:0",
                "AMD Radeon RX 9070 XT (gfx1201, 16 GB)",
                "rocm",
                "gfx1201"
            )),
            "gpu:0 AMD Radeon RX 9070 XT (gfx1201, 16 GB) · rocm"
        );
        assert_eq!(
            device_line(&device("cpu", "CPU (32 threads)", "cpu", "x86_64")),
            "cpu CPU (32 threads) · x86_64"
        );
        assert_eq!(
            device_line(&device("gpu:1", "RTX 4090", "cuda", "sm_89")),
            "gpu:1 RTX 4090 · cuda, sm_89"
        );
    }

    #[tokio::test]
    async fn an_idle_rate_is_plain_zero() {
        let dir = tempfile::tempdir().unwrap();
        let c = Control::new(ControlConfig::new(Role::Processor, "box", "0", dir.path()));
        let json = serde_json::to_string(&c.status().stats).unwrap();
        assert!(json.contains("\"rate_pages_per_minute\":0.0"), "{json}");
    }
}
