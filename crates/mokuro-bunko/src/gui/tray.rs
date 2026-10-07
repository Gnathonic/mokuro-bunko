//! "Run from the tray when I log in": tray-managed running (GUI.md §5 "As built (G3)").
//!
//! * `tray.json` next to the default `config.yaml` (portable: `data\tray.json`) lists
//!   the instances the tray starts and supervises:
//!   `{"managed":[{"role":"server"|"processor","args":[...]}],"notifications":false}`.
//!   Same format and defaults as `bunko-tray`'s `trayconf.rs` (re-declared here: that
//!   crate links GTK, which the CLI must not). The tray reads it when it starts.
//! * The tray's own login item, as the tray's "Start at login" writes it
//!   (`bunko-tray/src/autostart.rs`): XDG autostart `.desktop`, a LaunchAgent, or the
//!   Startup-folder shortcut `Mokuro Bunko.lnk`.
//!
//! A role runs either from the tray or as a service, never both (two copies would fight
//! over the port / the processor's storage lock): the pages offer to remove the other.

use super::Role;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DESKTOP_NAME: &str = "mokuro-bunko-tray.desktop";
pub const LAUNCHD_LABEL: &str = "io.github.gnathonic.mokuro-bunko-tray";
pub const WINDOWS_LINK: &str = "Mokuro Bunko.lnk";

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

/// The tray program's name.
pub fn tray_name() -> &'static str {
    if cfg!(windows) {
        "mokuro-bunko-tray.exe"
    } else {
        "mokuro-bunko-tray"
    }
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

/// The tray program that ships with this one: next to it, in the macOS bundle next to
/// it, or on `PATH`.
pub fn tray_exe(exe: &Path) -> Option<PathBuf> {
    let dir = exe.parent()?;
    let mut candidates = vec![
        dir.join(tray_name()),
        dir.join("mokuro-bunko.app/Contents/MacOS")
            .join(tray_name()),
        dir.join("Mokuro Bunko.app/Contents/MacOS")
            .join(tray_name()),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|d| d.join(tray_name())));
    }
    candidates.into_iter().find(|p| p.is_file())
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

pub fn manages(conf: &TrayConfig, role: &str) -> bool {
    conf.managed.iter().any(|m| m.role == role)
}

// --- the tray's login item --------------------------------------------------------

/// The file whose presence means the tray starts at login.
pub fn autostart_path() -> Option<PathBuf> {
    let home = bunko_core::storage::home_dir();
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .filter(|v| !v.is_empty())
            .map(|_| super::service::startup_dir().join(WINDOWS_LINK))
    } else if cfg!(target_os = "macos") {
        Some(
            home.join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
        )
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        Some(base.join("autostart").join(DESKTOP_NAME))
    }
}

/// Add (or with `false` remove) the tray's login item pointing at `tray`.
pub fn set_autostart(tray: &Path, enabled: bool) -> Result<Option<PathBuf>, String> {
    let path = autostart_path().ok_or("no APPDATA folder")?;
    if !enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(Some(path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("could not remove {}: {e}", path.display())),
        };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    write_entry(&path, tray)?;
    Ok(Some(path))
}

#[cfg(windows)]
fn write_entry(path: &Path, exe: &Path) -> Result<(), String> {
    let mut link = mslnk::ShellLink::new(exe)
        .map_err(|e| format!("could not make a shortcut to {}: {e}", exe.display()))?;
    if let Some(dir) = exe.parent() {
        link.set_working_dir(Some(dir.to_string_lossy().into_owned()));
    }
    link.set_name(Some("Mokuro Bunko".into()));
    link.create_lnk(path)
        .map_err(|e| format!("could not write {}: {e}", path.display()))
}

#[cfg(not(windows))]
fn write_entry(path: &Path, exe: &Path) -> Result<(), String> {
    let text = if cfg!(target_os = "macos") {
        launchd_plist(exe)
    } else {
        desktop_entry(exe)
    };
    std::fs::write(path, text).map_err(|e| format!("could not write {}: {e}", path.display()))
}

/// The XDG autostart entry (bunko-tray `autostart::desktop_entry(exe, true)`).
#[cfg_attr(windows, allow(dead_code))]
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Mokuro Bunko\n\
         GenericName=Manga library tray\n\
         Comment=Status, pause and settings of the mokuro-bunko library and OCR processor\n\
         Exec={}\n\
         Icon=mokuro-bunko\n\
         Terminal=false\n\
         Categories=Utility;\n\
         StartupNotify=false\n\
         X-GNOME-Autostart-enabled=true\n\
         X-KDE-autostart-after=panel\n",
        desktop_exec_quote(&exe.to_string_lossy())
    )
}

/// The Exec key's quoting rules (Desktop Entry Specification, "The Exec key").
#[cfg_attr(windows, allow(dead_code))]
fn desktop_exec_quote(arg: &str) -> String {
    let needs = arg
        .chars()
        .any(|c| c.is_whitespace() || "\"'\\><~|&;$*?#()`".contains(c));
    let escaped = arg.replace('%', "%%");
    if !needs {
        return escaped;
    }
    let mut out = String::from("\"");
    for c in escaped.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The tray's LaunchAgent (bunko-tray `autostart::launchd_plist`).
#[cfg_attr(windows, allow(dead_code))]
pub fn launchd_plist(exe: &Path) -> String {
    let xml = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         \x20   <string>{}</string>\n\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         \x20 <key>LimitLoadToSessionType</key>\n\
         \x20 <string>Aqua</string>\n\
         \x20 <key>ProcessType</key>\n\
         \x20 <string>Interactive</string>\n\
         </dict>\n\
         </plist>\n",
        xml(&exe.to_string_lossy())
    )
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

/// A tray holds its lock (it runs for this user).
pub fn tray_running(exe: &Path) -> bool {
    lock_dirs(exe).iter().any(|d| {
        let Ok(f) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(d.join(".tray.lock"))
        else {
            return false;
        };
        matches!(
            fs4::FileExt::try_lock(&f),
            Err(fs4::TryLockError::WouldBlock)
        )
        // A lock we got is released when `f` drops.
    })
}

/// The pids of this user's running trays (Windows stops it by image name instead).
#[cfg(unix)]
pub fn tray_pids() -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: getuid has no failure mode.
        let uid = unsafe { libc::getuid() };
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir("/proc") else {
            return out;
        };
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let mine = std::os::unix::fs::MetadataExt::uid(&match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            }) == uid;
            let is_tray = std::fs::read_link(e.path().join("exe")).is_ok_and(|x| {
                x.file_name().is_some_and(|n| {
                    n == tray_name() || n.to_string_lossy() == format!("{} (deleted)", tray_name())
                })
            });
            if mine && is_tray && pid != std::process::id() {
                out.push(pid);
            }
        }
        out
    }
    #[cfg(target_os = "macos")]
    {
        let uid = unsafe { libc::getuid() };
        std::process::Command::new("pgrep")
            .args(["-U", &uid.to_string(), "-f", "/mokuro-bunko-tray$"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|l| l.trim().parse().ok())
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// Stop this user's tray (it reads tray.json only when it starts; its children stop
/// with it). Waits until its lock is free. Returns what was done.
pub fn stop_tray(exe: &Path) -> Result<Option<String>, String> {
    if !tray_running(exe) {
        return Ok(None);
    }
    #[cfg(unix)]
    let what = {
        let pids = tray_pids();
        for pid in &pids {
            // SAFETY: a plain signal to a process of this user.
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGTERM) };
        }
        format!(
            "pid {}",
            pids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    #[cfg(windows)]
    let what = {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/IM", tray_name()])
            .output();
        tray_name().to_string()
    };
    #[cfg(not(any(unix, windows)))]
    let what = String::new();
    for _ in 0..40 {
        if !tray_running(exe) {
            return Ok(Some(format!("Stopped the running tray ({what}).")));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err(format!(
        "the running tray ({what}) did not stop; choose Quit in its menu, then try again"
    ))
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

    #[test]
    fn autostart_files() {
        let e = desktop_entry(Path::new("/home/a b/bin/mokuro-bunko-tray"));
        assert!(
            e.contains("Exec=\"/home/a b/bin/mokuro-bunko-tray\"\n"),
            "{e}"
        );
        assert!(e.contains("X-GNOME-Autostart-enabled=true"));
        let p = launchd_plist(Path::new(
            "/Applications/R&D.app/Contents/MacOS/mokuro-bunko-tray",
        ));
        assert!(p.contains("R&amp;D") && p.contains(LAUNCHD_LABEL));
        assert_eq!(desktop_exec_quote("/x/100%"), "/x/100%%");
    }
}
