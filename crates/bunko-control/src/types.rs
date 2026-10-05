//! The control API's JSON (docs/rust-port/GUI.md §2): what `GET /control/status`
//! answers, what `POST /control/pause` takes, and `.control.json`. Plain serde types,
//! shared by the instances (feature `runtime`) and the tray (no features).

use serde::{Deserialize, Serialize};

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
/// The env var a supervisor (the tray) sets on instances it starts: `POST /control/stop`
/// then stops them. Unset: the instance is a service or was started by hand, and stop
/// is refused.
pub const MANAGED_ENV: &str = "MOKURO_CONTROL_MANAGED";
/// `MOKURO_CONTROL=off` starts no control listener (Docker, tests).
pub const ENABLE_ENV: &str = "MOKURO_CONTROL";
/// The cookie `/app/login?t=<token>` sets. A per-port variant
/// `bunko_control_<port>` is accepted too (cookies do not separate ports).
pub const COOKIE: &str = "bunko_control";

/// Read `<storage>/.control.json` (None: absent or unreadable).
pub fn read_control_file(storage: &std::path::Path) -> Option<ControlFile> {
    let text = std::fs::read_to_string(storage.join(CONTROL_FILE)).ok()?;
    serde_json::from_str(&text).ok()
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
}
