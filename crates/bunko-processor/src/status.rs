//! The status a running processor writes, and what `processor status` reads back
//! (`<storage>/processor-status.json`, 0.5.2 `processor/status.py`).

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

pub fn status_path(storage: &Path) -> PathBuf {
    storage.join("processor-status.json")
}

/// The states `serve` writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connected,
    Unreachable,
    Refused,
    Disconnected,
    Stopped,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Connected => "connected",
            State::Unreachable => "unreachable",
            State::Refused => "refused",
            State::Disconnected => "disconnected",
            State::Stopped => "stopped",
        }
    }
}

/// Last-known state, written atomically (via `.tmp`); failures are ignored.
pub fn write_status(
    storage: &Path,
    state: State,
    library: &str,
    name: Option<&str>,
    error: Option<&str>,
) {
    let updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let mut payload = Map::new();
    payload.insert("updated_at".into(), json!(updated_at));
    payload.insert("state".into(), json!(state.as_str()));
    payload.insert("library".into(), json!(library));
    if let Some(name) = name {
        payload.insert("name".into(), json!(name));
    }
    if let Some(error) = error {
        payload.insert("error".into(), json!(error));
    }
    let path = status_path(storage);
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(storage)?;
        let tmp = path.with_file_name("processor-status.json.tmp");
        let text = serde_json::to_string_pretty(&Value::Object(payload)).unwrap_or_default();
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        tracing::debug!("could not write {}: {e}", path.display());
    }
}

pub fn read_status(storage: &Path) -> Map<String, Value> {
    std::fs::read_to_string(status_path(storage))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default()
}

fn truthy(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        Value::Number(n) if n.as_f64() == Some(0.0) => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// `never connected`, or `<state> to <library>[ — N session(s)]`.
pub fn describe(status: &Map<String, Value>) -> String {
    if status.is_empty() {
        return "never connected".to_string();
    }
    let state = truthy(status.get("state")).unwrap_or_else(|| "unknown".into());
    let library = truthy(status.get("library")).unwrap_or_else(|| "?".into());
    let detail = truthy(status.get("sessions"))
        .map(|n| format!(" — {n} session(s)"))
        .unwrap_or_default();
    format!("{state} to {library}{detail}")
}

/// What `processor status` prints.
pub fn status_line(storage: &Path) -> String {
    describe(&read_status(storage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_describe() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(status_line(dir.path()), "never connected");
        write_status(
            dir.path(),
            State::Connected,
            "https://lib",
            Some("tower"),
            None,
        );
        let s = read_status(dir.path());
        assert_eq!(s["name"], "tower");
        assert_eq!(status_line(dir.path()), "connected to https://lib");
        let mut with = s.clone();
        with.insert("sessions".into(), json!(2));
        assert_eq!(describe(&with), "connected to https://lib — 2 session(s)");
    }
}
