//! "Start at login": the tray's own autostart entry.
//!
//! * Linux: `$XDG_CONFIG_HOME/autostart/mokuro-bunko-tray.desktop` (XDG Autostart).
//! * macOS: `~/Library/LaunchAgents/io.github.gnathonic.mokuro-bunko-tray.plist`
//!   (RunAtLoad; loaded at the next login).
//! * Windows: `Mokuro Bunko.lnk` in the Startup folder — the same shortcut
//!   `install.ps1 -Startup` creates.

use crate::paths::{self, Env};
use std::path::{Path, PathBuf};

pub const DESKTOP_NAME: &str = "mokuro-bunko-tray.desktop";
pub const LAUNCHD_LABEL: &str = "io.github.gnathonic.mokuro-bunko-tray";
pub const WINDOWS_LINK: &str = "Mokuro Bunko.lnk";

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct AutostartError(pub String);

/// The file whose presence means "starts at login".
pub fn entry_path(env: &dyn Env) -> Option<PathBuf> {
    if cfg!(windows) {
        env.path("APPDATA")
            .map(|a| crate::discover::startup_dir(&a).join(WINDOWS_LINK))
    } else if cfg!(target_os = "macos") {
        Some(
            paths::home(env)
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
        )
    } else {
        Some(paths::config_base(env).join("autostart").join(DESKTOP_NAME))
    }
}

pub fn is_enabled(env: &dyn Env) -> bool {
    entry_path(env).is_some_and(|p| p.exists())
}

/// Turn the entry on (pointing at `exe`, this tray) or off.
pub fn set(env: &dyn Env, exe: &Path, enabled: bool) -> Result<(), AutostartError> {
    let path = entry_path(env).ok_or_else(|| AutostartError("no APPDATA folder".into()))?;
    if !enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AutostartError(format!(
                "could not remove {}: {e}",
                path.display()
            ))),
        };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| AutostartError(format!("could not create {}: {e}", dir.display())))?;
    }
    write_entry(&path, exe)
}

#[cfg(windows)]
fn write_entry(path: &Path, exe: &Path) -> Result<(), AutostartError> {
    let mut link = mslnk::ShellLink::new(exe).map_err(|e| {
        AutostartError(format!(
            "could not make a shortcut to {}: {e}",
            exe.display()
        ))
    })?;
    if let Some(dir) = exe.parent() {
        link.set_working_dir(Some(dir.to_string_lossy().into_owned()));
    }
    link.set_name(Some("Mokuro Bunko".into()));
    link.create_lnk(path)
        .map_err(|e| AutostartError(format!("could not write {}: {e}", path.display())))
}

#[cfg(not(windows))]
fn write_entry(path: &Path, exe: &Path) -> Result<(), AutostartError> {
    let text = if cfg!(target_os = "macos") {
        launchd_plist(exe)
    } else {
        desktop_entry(exe, true)
    };
    std::fs::write(path, text)
        .map_err(|e| AutostartError(format!("could not write {}: {e}", path.display())))
}

/// A freedesktop `.desktop` entry for the tray (`autostart`: the autostart variant).
pub fn desktop_entry(exe: &Path, autostart: bool) -> String {
    let exec = desktop_exec_quote(&exe.to_string_lossy());
    let mut text = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Mokuro Bunko\n\
         GenericName=Manga library tray\n\
         Comment=Status, pause and settings of the mokuro-bunko library and OCR processor\n\
         Exec={exec}\n\
         Icon=mokuro-bunko\n\
         Terminal=false\n\
         Categories=Utility;\n\
         StartupNotify=false\n"
    );
    if autostart {
        text.push_str("X-GNOME-Autostart-enabled=true\nX-KDE-autostart-after=panel\n");
    }
    text
}

/// The Exec key's quoting rules (Desktop Entry Specification, "The Exec key").
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

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn launchd_plist(exe: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting() {
        assert_eq!(desktop_exec_quote("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(
            desktop_exec_quote("/home/a b/$x/100%"),
            "\"/home/a b/\\$x/100%%\""
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn toggles_the_entry() {
        use crate::paths::test_env::FakeEnv;
        let dir = tempfile::tempdir().unwrap();
        let env = FakeEnv::default()
            .with("HOME", dir.path())
            .with("XDG_CONFIG_HOME", dir.path().join("cfg"));
        assert!(!is_enabled(&env));
        set(&env, Path::new("/opt/mb/mokuro-bunko-tray"), true).unwrap();
        assert!(is_enabled(&env));
        let text = std::fs::read_to_string(entry_path(&env).unwrap()).unwrap();
        assert!(text.contains("/opt/mb/mokuro-bunko-tray"));
        set(&env, Path::new("/x"), false).unwrap();
        assert!(!is_enabled(&env));
        set(&env, Path::new("/x"), false).unwrap();
    }

    #[test]
    fn plist_escapes() {
        let p = launchd_plist(Path::new(
            "/Applications/R&D/mokuro-bunko.app/Contents/MacOS/mokuro-bunko-tray",
        ));
        assert!(p.contains("R&amp;D"));
        assert!(p.contains(LAUNCHD_LABEL));
    }
}
