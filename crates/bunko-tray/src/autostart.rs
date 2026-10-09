//! "Start at login": the tray's own autostart entry.
//!
//! * Linux: `$XDG_CONFIG_HOME/autostart/mokuro-bunko-tray.desktop` (XDG Autostart),
//!   `Exec=<mokuro-bunko> tray`.
//! * macOS: `~/Library/LaunchAgents/io.github.gnathonic.mokuro-bunko-tray.plist`
//!   (RunAtLoad; loaded at the next login), `<app>/Contents/MacOS/mokuro-bunko tray`.
//! * Windows: `Mokuro Bunko.lnk` in the Startup folder — the same shortcut
//!   `install.ps1 -Startup` creates — to `Mokuro Bunko.exe`.
//!
//! The file names are the ones of the separate `mokuro-bunko-tray` program of 0.7.0-beta.2
//! and earlier, so an existing entry is found (and rewritten by the update, see the
//! mokuro-bunko crate's `migrate`).

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

/// What a login item runs to start the tray of the program `exe`: `(program, args)`.
/// Linux and macOS: `<exe> tray`. Windows: `Mokuro Bunko.exe` (no console window), which
/// is `exe` itself or, for the command line `bin\mokuro-bunko.exe`, the one above it.
pub fn launch_command(exe: &Path) -> (PathBuf, Vec<String>) {
    if cfg!(windows) {
        let gui = paths::WINDOWS_GUI_EXE;
        if exe.file_name().is_some_and(|n| n.eq_ignore_ascii_case(gui)) {
            return (exe.to_path_buf(), Vec::new());
        }
        if let Some(root) = exe.parent().and_then(Path::parent)
            && root.join(gui).is_file()
        {
            return (root.join(gui), Vec::new());
        }
    }
    (exe.to_path_buf(), vec!["tray".into()])
}

/// Turn the entry on (starting the tray of the program `exe`) or off.
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
    let (exe, args) = launch_command(exe);
    let exe = exe.as_path();
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
    if !args.is_empty() {
        link.set_arguments(Some(args.join(" ")));
    }
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

/// A freedesktop `.desktop` entry that starts the tray of the program `exe`
/// (`autostart`: the autostart variant).
pub fn desktop_entry(exe: &Path, autostart: bool) -> String {
    let (exe, args) = launch_command(exe);
    let mut exec = desktop_exec_quote(&exe.to_string_lossy());
    for a in &args {
        exec.push(' ');
        exec.push_str(&desktop_exec_quote(a));
    }
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
    let (exe, args) = launch_command(exe);
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml(a)))
        .collect();
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
         {args}\
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
        set(&env, Path::new("/opt/mb/mokuro-bunko"), true).unwrap();
        assert!(is_enabled(&env));
        let text = std::fs::read_to_string(entry_path(&env).unwrap()).unwrap();
        if cfg!(target_os = "macos") {
            assert!(
                text.contains("<string>/opt/mb/mokuro-bunko</string>\n    <string>tray</string>\n"),
                "{text}"
            );
        } else {
            assert!(
                text.contains("\nExec=/opt/mb/mokuro-bunko tray\n"),
                "{text}"
            );
        }
        set(&env, Path::new("/x"), false).unwrap();
        assert!(!is_enabled(&env));
        set(&env, Path::new("/x"), false).unwrap();
    }

    #[test]
    fn plist_escapes() {
        let p = launchd_plist(Path::new(
            "/Applications/R&D/Mokuro Bunko.app/Contents/MacOS/mokuro-bunko",
        ));
        assert!(p.contains("R&amp;D"));
        assert!(p.contains(LAUNCHD_LABEL));
        assert!(p.contains("<string>tray</string>"), "{p}");
    }
}
