//! The control API's JSON (docs/rust-port/GUI.md §2): what `GET /control/status`
//! answers, what `POST /control/pause` takes, and `.control.json`. Plain serde types,
//! shared by the instances (feature `runtime`) and the tray (no features).

use serde::{Deserialize, Serialize};

pub use bunko_proto::OcrInstall;

/// Which kind of instance answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// `mokuro-bunko serve`: the library server (and its local OCR).
    Server,
    /// `mokuro-bunko processor serve`.
    Processor,
    /// `mokuro-bunko gui`: the app pages with nothing running yet.
    Gui,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Processor => "processor",
            Role::Gui => "gui",
        }
    }
}

/// `pause.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseMode {
    /// Stop taking work; what is running finishes and uploads.
    AfterVolume,
    /// Stop now; held volumes go back to the library's queue at once.
    Now,
}

impl PauseMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PauseMode::AfterVolume => "after_volume",
            PauseMode::Now => "now",
        }
    }
}

/// `POST /control/pause` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseBody {
    pub mode: PauseMode,
    /// RFC 3339 (any offset); the pause lifts itself then. Null: until resumed.
    #[serde(default)]
    pub until: Option<String>,
    /// `user` (default) or `schedule`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `status.pause`: all null while not paused.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PauseView {
    pub mode: Option<PauseMode>,
    /// RFC 3339, UTC (`2026-10-04T18:00:00Z`).
    pub until: Option<String>,
    pub reason: Option<String>,
    /// When the pause began (RFC 3339, UTC).
    #[serde(default)]
    pub since: Option<String>,
}

/// `status.library`: the library this instance works for (a server: itself).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LibraryView {
    pub url: Option<String>,
    pub connected: bool,
    /// Volumes the library still owes OCR (the server knows; a processor: null).
    pub queue_pending: Option<u64>,
    /// Why it is not connected (login refused, unreachable, ...).
    #[serde(default)]
    pub error: Option<String>,
}

/// One volume this machine is reading now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CurrentVolume {
    /// `Series/Vol 01`.
    pub volume: String,
    pub engine: Option<String>,
    pub precision: Option<String>,
    /// `gpu:0 RTX 4090`, `cpu`.
    pub device: Option<String>,
    pub pages_done: u32,
    pub pages_total: u32,
    pub pages_per_second: Option<f64>,
    pub eta_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayStats {
    pub volumes: u64,
    pub pages: u64,
    pub busy_seconds: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TotalStats {
    pub volumes: u64,
    pub pages: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub today: DayStats,
    pub total: TotalStats,
    /// Pages read over the last minute.
    pub rate_pages_per_minute: f64,
    pub gpu_busy_percent: Option<f64>,
    pub cpu_cores_busy: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BackendView {
    /// The OCR backend pack in use (`torch-rocm-2.13.0`), if any.
    pub pack: Option<String>,
    /// `gpu:0 RTX 4090 sm_89`, `cpu AMD Ryzen 9 x86_64`.
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    pub severity: Severity,
    pub text: String,
    #[serde(default)]
    pub hint: Option<String>,
    /// What raised it, when a tray should treat it specially: `update` (an automatic
    /// update that needs its owner: the tray notifies once per distinct text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl Problem {
    pub const KIND_UPDATE: &'static str = "update";
    /// The background OCR backend install failed (retry from the dashboard or the
    /// admin panel).
    pub const KIND_OCR_INSTALL: &'static str = "ocr-install";

    /// A `fail` problem of kind `update`: something only the owner can fix.
    pub fn update_needs_you(text: impl Into<String>, hint: impl Into<String>) -> Problem {
        Problem {
            severity: Severity::Fail,
            text: text.into(),
            hint: Some(hint.into()),
            kind: Some(Self::KIND_UPDATE.into()),
        }
    }

    /// A `warn` problem of kind `update` (it retries by itself).
    pub fn update_warning(text: impl Into<String>, hint: Option<String>) -> Problem {
        Problem {
            severity: Severity::Warn,
            text: text.into(),
            hint,
            kind: Some(Self::KIND_UPDATE.into()),
        }
    }
}

/// `status.update`: the automatic update of this instance (binary, OCR backend pack,
/// models). Added in 0.7; absent from older instances.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateView {
    /// `idle`, `available` (found, auto off or not installable here), `waiting` (for a
    /// quiet moment: the running volume finishes first), `downloading`, `installing`,
    /// `restarting`, `updated` (this run is the result of one), `failed` (retries
    /// later), `blocked` (needs its owner: see `problems`).
    pub state: String,
    /// The version (or `backend pack <name>`) being installed / just installed.
    #[serde(default)]
    pub version: Option<String>,
    /// The version it came from (`updated`).
    #[serde(default)]
    pub from: Option<String>,
    /// What happened, in a sentence.
    #[serde(default)]
    pub message: Option<String>,
    /// Automatic updates are on here (`update.auto` / `auto_update`).
    #[serde(default)]
    pub auto: bool,
    /// When this state began (RFC 3339, UTC).
    #[serde(default)]
    pub since: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Urls {
    /// Path on the control listener (`/app/dashboard`).
    pub dashboard: String,
    /// The library's web address, if known.
    pub library: Option<String>,
    pub logs_dir: Option<String>,
}

/// `status.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Working,
    Idle,
    Paused,
    /// Paused after the running volume(s): they are still finishing.
    Pausing,
    Connecting,
    Disconnected,
    Error,
    /// `gui`: nothing set up / running.
    Setup,
}

/// `GET /control/status`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub role: Role,
    pub version: String,
    pub name: String,
    pub state: State,
    pub pause: PauseView,
    pub library: LibraryView,
    pub current: Vec<CurrentVolume>,
    pub stats: Stats,
    pub backend: BackendView,
    pub problems: Vec<Problem>,
    pub urls: Urls,
    /// The instance was started by the tray: `POST /control/stop` works.
    #[serde(default)]
    pub managed: bool,
    /// Pause/resume apply here (false: a lite server, which reads no OCR itself, or
    /// `gui`).
    #[serde(default)]
    pub can_pause: bool,
    /// The automatic update (0.7): null when this instance has nothing to say.
    #[serde(default)]
    pub update: Option<UpdateView>,
    /// The background OCR backend install (0.7): null when none ran since the start.
    #[serde(default)]
    pub install: Option<OcrInstall>,
}

/// `<storage>/.control.json`: how a tray or browser finds a running instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlFile {
    pub role: Role,
    pub pid: u32,
    pub port: u16,
    pub token: String,
    pub version: String,
    /// RFC 3339, UTC.
    pub started_at: String,
    /// `http://127.0.0.1:<port>`.
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub managed: bool,
}

/// The file name under the instance's storage directory.
pub const CONTROL_FILE: &str = ".control.json";
/// Where a pause is kept across restarts.
pub const PAUSE_FILE: &str = ".pause.json";
/// Volumes and pages read on this machine (today / total).
pub const STATS_FILE: &str = ".stats.json";
/// What an automatic update left for the next start (`bunko_update::auto::Marker`).
pub const UPDATE_FILE: &str = ".update.json";
/// The env var a supervisor (the tray) sets on instances it starts: `POST /control/stop`
/// then stops them. Unset: the instance is a service or was started by hand, and stop
/// is refused.
pub const MANAGED_ENV: &str = "MOKURO_CONTROL_MANAGED";
/// `MOKURO_CONTROL=off` starts no control listener (Docker, tests).
pub const ENABLE_ENV: &str = "MOKURO_CONTROL";
/// The cookie `/app/login?c=<code>` sets (named `bunko_control_<port>` on a
/// listener's own port: cookies do not separate ports). Its value is the token.
pub const COOKIE: &str = "bunko_control";

/// Read `<storage>/.control.json` (None: absent, unreadable, or not safe to trust,
/// see [`read_private_file`]).
pub fn read_control_file(storage: &std::path::Path) -> Option<ControlFile> {
    let text = read_private_file(&storage.join(CONTROL_FILE))?;
    serde_json::from_str(&text).ok()
}

/// Read a file only another process of this user could have written. On Unix it must
/// be a regular file (not a symlink), owned by our effective uid, with no group or
/// other permission bits; otherwise None, as if it were absent. Another local user
/// could plant one in a shared directory and point the tray at their own port.
pub fn read_private_file(path: &std::path::Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let trusted = |m: &std::fs::Metadata| {
            m.file_type().is_file() && m.uid() == euid() && m.mode() & 0o077 == 0
        };
        let before = std::fs::symlink_metadata(path).ok()?;
        if !trusted(&before) {
            return None;
        }
        let mut file = std::fs::File::open(path).ok()?;
        // The file opened is the one checked (not swapped for a symlink in between).
        let opened = file.metadata().ok()?;
        if !trusted(&opened) || opened.dev() != before.dev() || opened.ino() != before.ino() {
            return None;
        }
        let mut text = String::new();
        file.read_to_string(&mut text).ok()?;
        Some(text)
    }
    #[cfg(not(unix))]
    {
        std::fs::read_to_string(path).ok()
    }
}

/// This process's effective uid.
#[cfg(unix)]
fn euid() -> u32 {
    // uid_t is a u32 on every Unix target Rust supports (libc is not a dependency here).
    unsafe extern "C" {
        safe fn geteuid() -> u32;
    }
    geteuid()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pause_body_and_status_shapes() {
        let b: PauseBody = serde_json::from_str(r#"{"mode":"after_volume","until":null}"#).unwrap();
        assert_eq!(b.mode, PauseMode::AfterVolume);
        assert!(serde_json::from_str::<PauseBody>(r#"{"mode":"later"}"#).is_err());
        let v = serde_json::to_value(PauseView::default()).unwrap();
        assert_eq!(v["mode"], serde_json::Value::Null);
        assert_eq!(serde_json::to_value(State::Pausing).unwrap(), "pausing");
        assert_eq!(serde_json::to_value(Role::Processor).unwrap(), "processor");
    }

    /// A planted control file is ignored: readable by others, or a symlink.
    #[cfg(unix)]
    #[test]
    fn control_file_must_be_private_and_ours() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONTROL_FILE);
        let text = r#"{"role":"server","pid":1,"port":4000,"token":"t","version":"0.7.0","started_at":"x"}"#;
        std::fs::write(&path, text).unwrap();
        let chmod = |mode| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
        chmod(0o600).unwrap();
        assert_eq!(read_control_file(dir.path()).unwrap().port, 4000);
        for mode in [0o644, 0o640, 0o604, 0o660] {
            chmod(mode).unwrap();
            assert!(read_control_file(dir.path()).is_none(), "{mode:o}");
        }
        chmod(0o600).unwrap();

        // A symlink to a private file of ours is still refused.
        let other = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&path, other.path().join(CONTROL_FILE)).unwrap();
        assert!(read_control_file(other.path()).is_none());
        assert!(read_control_file(dir.path()).is_some());
    }
}
