//! Tray-managed running (GUI.md §5 "As built (G3)"): `tray.json` next to the default
//! `config.yaml` (portable: `data\tray.json`) lists the instances the tray starts and
//! supervises: `{"managed":[{"role":"server"|"processor","args":[...]}],"notifications":false}`.
//! Same format and defaults as `bunko-tray`'s `trayconf.rs` (written here so the lite
//! build, which has no tray, still reads and edits it). A running tray picks up a role
//! added to it. The chooser's two roles go here; "Start at login" is the tray's own
//! menu checkbox.

use super::Role;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct TrayConfig {
    pub managed: Vec<Managed>,
    pub notifications: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Managed {
    /// `server` or `processor`.
    pub role: String,
    /// Arguments after the executable; empty = the role's default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

/// The Windows portable layout (`PORTABLE.txt` next to the programs): its `data\`.
fn portable_data(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    dir.join("PORTABLE.txt").is_file().then(|| dir.join("data"))
}

/// Where the tray reads `tray.json` (bunko-tray `Layout::tray_config`).
pub fn config_path(exe: &Path) -> PathBuf {
    match portable_data(exe) {
        Some(d) => d.join("tray.json"),
        None => bunko_core::storage::default_config_path()
            .parent()
            .map(|d| d.join("tray.json"))
            .unwrap_or_else(|| PathBuf::from("tray.json")),
    }
}

/// How to start the tray of this program (`exe`): `mokuro-bunko tray`, on Windows the
/// app `mokuro-bunko.exe`. None in the lite build, which has no tray.
pub fn tray_command(exe: &Path) -> Option<(PathBuf, Vec<String>)> {
    #[cfg(feature = "tray")]
    {
        Some(bunko_tray::autostart::launch_command(exe))
    }
    #[cfg(not(feature = "tray"))]
    {
        let _ = exe;
        None
    }
}

/// No desktop to show a tray on (Linux without a display, an SSH session).
pub fn headless() -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        return false;
    }
    let set = |v: &str| std::env::var_os(v).is_some_and(|x| !x.is_empty());
    !set("DISPLAY") && !set("WAYLAND_DISPLAY")
}

pub fn load(path: &Path) -> Result<TrayConfig, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => {
            serde_json::from_str(&t).map_err(|e| format!("{} is not valid: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrayConfig::default()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

pub fn save(path: &Path, conf: &TrayConfig) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    let text = serde_json::to_string_pretty(conf).map_err(|e| e.to_string())? + "\n";
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// The entry for `role`: the server with its config (no args for the default one,
/// which is the tray's own default), the processor with `--config <its file>`.
pub fn entry(role: Role, config: &Path) -> Managed {
    let abs = std::path::absolute(config).unwrap_or_else(|_| config.to_path_buf());
    let text = abs.to_string_lossy().into_owned();
    match role {
        Role::Processor => Managed {
            role: "processor".into(),
            args: vec!["processor".into(), "serve".into(), "--config".into(), text],
        },
        _ => {
            let default = bunko_core::storage::default_config_path();
            let is_default = std::path::absolute(&default).unwrap_or(default) == abs;
            Managed {
                role: "server".into(),
                args: if is_default {
                    Vec::new()
                } else {
                    vec!["-c".into(), text, "serve".into()]
                },
            }
        }
    }
}

/// `conf` with `role` managed by `entry` (replacing that role's entry), or without it.
pub fn set_role(conf: &mut TrayConfig, role: &str, entry: Option<Managed>) {
    conf.managed.retain(|m| m.role != role);
    if let Some(e) = entry {
        conf.managed.push(e);
    }
}

#[cfg(test)]
fn manages(conf: &TrayConfig, role: &str) -> bool {
    conf.managed.iter().any(|m| m.role == role)
}

// --- is a tray running? -----------------------------------------------------------

/// The folders the tray keeps its `.tray.lock` in (bunko-tray `writable_log_dir`).
fn lock_dirs(exe: &Path) -> Vec<PathBuf> {
    let user = std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).unwrap_or_default();
    let first = match portable_data(exe) {
        Some(d) => d.join("logs"),
        None => std::env::var_os("MOKURO_STORAGE")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(bunko_core::storage::default_storage_path)
            .join("logs"),
    };
    let config_logs = bunko_core::storage::default_config_path()
        .parent()
        .map(|d| d.join("logs"))
        .unwrap_or_default();
    let mut dirs = vec![first, config_logs];
    // bunko-tray `last_resort_dir`: never the shared /tmp on Linux.
    if cfg!(target_os = "linux") {
        dirs.extend(
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .filter(|run| run.is_absolute())
                .map(|run| run.join("mokuro-bunko-tray")),
        );
    } else {
        dirs.push(std::env::temp_dir().join(format!("mokuro-bunko-tray-{user}")));
    }
    dirs
}

/// The tray lock in `dir` is held (by a running tray).
fn lock_held(dir: &Path) -> bool {
    let Ok(f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join(".tray.lock"))
    else {
        return false;
    };
    matches!(
        fs4::FileExt::try_lock(&f),
        Err(fs4::TryLockError::WouldBlock)
    )
    // A lock we got is released when `f` drops.
}

/// A tray holds its lock (it runs for this user).
pub fn tray_running(exe: &Path) -> bool {
    lock_dirs(exe).iter().any(|d| lock_held(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_json_round_trip_in_the_trays_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mokuro-bunko").join("tray.json");
        // install.ps1's file, as the tray reads it.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{ "managed": [ { "role": "server" } ] }"#).unwrap();
        let mut conf = load(&path).unwrap();
        assert!(manages(&conf, "server"));
        // An absolute path on this platform (entry() makes a relative one absolute).
        let cfg = if cfg!(windows) {
            r"C:\srv\p\processor.yaml"
        } else {
            "/srv/p/processor.yaml"
        };
        let p = entry(Role::Processor, Path::new(cfg));
        set_role(&mut conf, "processor", Some(p));
        save(&path, &conf).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "managed": [
                    {"role": "server"},
                    {"role": "processor",
                     "args": ["processor", "serve", "--config", cfg]}
                ],
                "notifications": false
            })
        );
        // Replacing and removing a role keeps the others.
        set_role(&mut conf, "server", None);
        save(&path, &conf).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.managed.len(), 1);
        assert_eq!(back.managed[0].role, "processor");
        assert!(!dir.path().join("mokuro-bunko/tray.json.tmp").exists());
        // A missing file is "nothing managed".
        assert_eq!(
            load(&dir.path().join("none.json")).unwrap(),
            TrayConfig::default()
        );
        assert!(
            load(&{
                std::fs::write(dir.path().join("bad.json"), "{").unwrap();
                dir.path().join("bad.json")
            })
            .is_err()
        );
    }

    #[test]
    fn server_entry_names_a_non_default_config() {
        let cfg = if cfg!(windows) {
            r"C:\srv\lib\config.yaml"
        } else {
            "/srv/lib/config.yaml"
        };
        let e = entry(Role::Server, Path::new(cfg));
        assert_eq!(e.args, vec!["-c", cfg, "serve"]);
        let d = entry(Role::Server, &bunko_core::storage::default_config_path());
        assert!(d.args.is_empty(), "{d:?}");
    }
}
