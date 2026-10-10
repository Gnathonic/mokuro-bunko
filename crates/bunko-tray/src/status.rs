//! The control API's documents (GUI.md §2): `<storage>/.control.json` and
//! `GET /control/status`. Every field is optional on the way in: the tray must keep
//! working against an older or newer instance that adds or drops fields.

use serde::{Deserialize, Serialize};

/// `<storage>/.control.json`, written by every long-running instance.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ControlFile {
    pub role: String,
    pub pid: u32,
    pub port: u16,
    pub token: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub started_at: String,
}

impl ControlFile {
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Status {
    pub role: String,
    pub version: String,
    pub name: String,
    /// working | idle | paused | pausing | connecting | disconnected | error | setup
    pub state: String,
    pub pause: Pause,
    pub library: Option<Library>,
    pub current: Vec<Current>,
    pub stats: Option<Stats>,
    pub backend: Option<Backend>,
    pub problems: Vec<Problem>,
    pub urls: Urls,
    /// Started by a tray (`MOKURO_CONTROL_MANAGED`): `POST /control/stop` works.
    pub managed: bool,
    /// Whether pause/resume apply (false: a lite server, `gui`). Older instances may
    /// not send it; then the role decides.
    pub can_pause: Option<bool>,
    /// The automatic update (added in 0.7; absent from older instances).
    pub update: Option<UpdateInfo>,
    /// The background OCR backend install (added in 0.7).
    pub install: Option<InstallInfo>,
}

/// `status.install`: the background OCR backend install. Every field optional.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct InstallInfo {
    /// running | done | failed | missing
    pub state: String,
    pub stage: String,
    pub percent: Option<u32>,
}

impl InstallInfo {
    /// `Installing OCR backend: 42%` while one runs (None otherwise: a failure is a
    /// problem, shown as one).
    pub fn line(&self) -> Option<String> {
        if self.state != "running" {
            return None;
        }
        let what = if self.stage == "models" {
            "Installing OCR models"
        } else {
            "Installing OCR backend"
        };
        Some(match self.percent {
            Some(p) => format!("{what}: {}%", p.min(100)),
            None => format!("{what}…"),
        })
    }
}

/// `status.update`: where the instance's automatic update stands. Every field is
/// optional so an older or newer instance never breaks the tray.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct UpdateInfo {
    /// idle | available | waiting | downloading | installing | restarting | updated |
    /// failed | blocked | off
    pub state: String,
    pub version: Option<String>,
    pub from: Option<String>,
    pub message: Option<String>,
    pub auto: Option<bool>,
    /// RFC 3339: when this state began.
    pub since: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Pause {
    pub mode: Option<String>,
    pub until: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Library {
    pub url: Option<String>,
    pub connected: Option<bool>,
    pub queue_pending: Option<f64>,
    /// Why it is not connected.
    pub error: Option<String>,
    /// What holds the queue now (0.7: an engine's first-run speed measurement).
    pub activity: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Current {
    pub volume: String,
    pub engine: Option<String>,
    pub precision: Option<String>,
    pub device: Option<String>,
    pub pages_done: Option<f64>,
    pub pages_total: Option<f64>,
    pub pages_per_second: Option<f64>,
    pub eta_seconds: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Stats {
    pub today: Option<Counts>,
    pub total: Option<Counts>,
    pub rate_pages_per_minute: Option<f64>,
    pub gpu_busy_percent: Option<f64>,
    pub cpu_cores_busy: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Counts {
    pub volumes: Option<f64>,
    pub pages: Option<f64>,
    pub busy_seconds: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Backend {
    pub pack: Option<String>,
    pub devices: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Problem {
    /// warn | fail
    pub severity: String,
    pub text: String,
    pub hint: Option<String>,
    /// What raised it (`update`: an automatic update that needs its owner).
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Urls {
    pub dashboard: Option<String>,
    pub library: Option<String>,
    pub logs_dir: Option<String>,
}

impl Status {
    pub fn is_paused(&self) -> bool {
        self.state == "paused"
    }

    pub fn is_pausing(&self) -> bool {
        self.state == "pausing"
    }

    /// Whether this instance runs OCR work the tray can pause (a processor, or a library
    /// server with local OCR; the `gui` instance has nothing to pause).
    pub fn can_pause(&self) -> bool {
        self.can_pause.unwrap_or_else(|| {
            matches!(self.role.as_str(), "processor" | "server") && self.state != "setup"
        })
    }

    pub fn library_url(&self) -> Option<&str> {
        self.urls
            .library
            .as_deref()
            .or(self.library.as_ref().and_then(|l| l.url.as_deref()))
            .filter(|u| !u.is_empty())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The example from GUI.md §2, verbatim.
    pub(crate) const CONTRACT_EXAMPLE: &str = r#"{ "role": "processor", "version": "0.7.0", "name": "beast",
  "state": "working",
  "pause": {"mode": null, "until": null, "reason": null},
  "library": {"url": "https://lib.example", "connected": true, "queue_pending": 12},
  "current": [{"volume": "Series/Vol 01", "engine": "hayai-nova", "precision": "bf16",
               "device": "gpu:0 RTX 4090", "pages_done": 37, "pages_total": 196,
               "pages_per_second": 4.2, "eta_seconds": 38}],
  "stats": {"today": {"volumes": 3, "pages": 512, "busy_seconds": 900},
            "total": {"volumes": 41, "pages": 7310},
            "rate_pages_per_minute": 252, "gpu_busy_percent": 71, "cpu_cores_busy": 6.5},
  "backend": {"pack": "torch-cu130-2.13.0", "devices": ["gpu:0 RTX 4090 sm_89"]},
  "problems": [{"severity": "warn", "text": "x", "hint": "y"}],
  "urls": {"dashboard": "/app/dashboard", "library": "https://lib.example", "logs_dir": "/path"} }"#;

    #[test]
    fn parses_the_contract_example() {
        let s: Status = serde_json::from_str(CONTRACT_EXAMPLE).unwrap();
        assert_eq!(s.role, "processor");
        assert_eq!(s.current[0].pages_done, Some(37.0));
        assert_eq!(s.stats.as_ref().unwrap().cpu_cores_busy, Some(6.5));
        assert_eq!(s.library_url(), Some("https://lib.example"));
        assert!(s.can_pause());
    }

    #[test]
    fn reads_the_update_and_problem_kind() {
        let s: Status = serde_json::from_str(
            r#"{"role":"server","update":{"state":"updated","version":"0.7.1","from":"0.7.0","since":"2026-10-05T10:00:00Z"},
                "problems":[{"severity":"fail","text":"t","hint":"h","kind":"update"}]}"#,
        )
        .unwrap();
        let u = s.update.unwrap();
        assert_eq!(
            (u.state.as_str(), u.version.as_deref()),
            ("updated", Some("0.7.1"))
        );
        assert_eq!(u.from.as_deref(), Some("0.7.0"));
        assert_eq!(s.problems[0].kind.as_deref(), Some("update"));
        // Older instances send neither.
        let old: Status = serde_json::from_str(CONTRACT_EXAMPLE).unwrap();
        assert!(old.update.is_none() && old.problems[0].kind.is_none());
    }

    #[test]
    fn tolerates_missing_and_extra_fields() {
        let s: Status = serde_json::from_str(r#"{"role":"gui","state":"setup","new":1}"#).unwrap();
        assert!(!s.can_pause());
        assert!(s.current.is_empty());
        let c: ControlFile =
            serde_json::from_str(r#"{"role":"server","pid":7,"port":4100,"token":"t"}"#).unwrap();
        assert_eq!(c.base_url(), "http://127.0.0.1:4100");
    }
}
