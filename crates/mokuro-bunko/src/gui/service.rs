//! "Start with the machine" for either role: a systemd user unit (Linux), a launchd
//! agent (macOS) or a Startup-folder entry (Windows), all per user, no admin rights.
//!
//! The processor's file is exactly `processor service`'s (bunko-processor renders it);
//! the library server gets the same kind of file running `mokuro-bunko -c <config>
//! serve`. Writing the file and starting it are separate: the pages can write the
//! file only (`start: false`), and say how to enable it.

use super::Role;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const SERVER_UNIT: &str = "mokuro-bunko.service";
pub const SERVER_STARTUP: &str = "mokuro-bunko-server.cmd";
pub const SERVER_LAUNCHD: &str = "io.github.gnathonic.mokuro-bunko";
#[cfg(feature = "ocr")]
pub const PROCESSOR_UNIT: &str = "mokuro-bunko-processor.service";
#[cfg(feature = "ocr")]
pub const PROCESSOR_STARTUP: &str = "mokuro-bunko-processor.cmd";
#[cfg(feature = "ocr")]
pub const PROCESSOR_LAUNCHD: &str = "io.github.gnathonic.mokuro-bunko-processor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    SystemdUser,
    WindowsStartup,
    Launchd,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::SystemdUser => "systemd-user",
            Kind::WindowsStartup => "windows-startup",
            Kind::Launchd => "launchd",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Kind::SystemdUser => "a systemd user service",
            Kind::WindowsStartup => "an entry in your Startup folder",
            Kind::Launchd => "a launchd agent",
        }
    }
}

/// This platform's kind (what would be written; whether it can be started is
/// [`can_start`]).
pub fn platform_kind() -> Kind {
    if cfg!(windows) {
        Kind::WindowsStartup
    } else if cfg!(target_os = "macos") {
        Kind::Launchd
    } else {
        Kind::SystemdUser
    }
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// The service manager is there to start it now.
pub fn can_start(kind: Kind) -> bool {
    match kind {
        Kind::WindowsStartup => true,
        Kind::Launchd => on_path("launchctl"),
        Kind::SystemdUser => on_path("systemctl") && Path::new("/run/systemd/system").is_dir(),
    }
}

fn home() -> PathBuf {
    bunko_core::storage::home_dir()
}

pub fn user_unit_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("systemd")
        .join("user")
}

pub fn startup_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("AppData").join("Roaming"))
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
}

fn one_line(value: &str) -> Result<&str, String> {
    if value.contains('\n') || value.contains('\r') {
        return Err(format!(
            "{value:?} holds a line break; move it to a path without one"
        ));
    }
    Ok(value)
}

fn systemd_word(value: &str) -> Result<String, String> {
    let value = one_line(value)?.replace('%', "%%").replace('$', "$$");
    if value.chars().any(char::is_whitespace) || value.contains('"') {
        return Ok(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    Ok(value)
}

fn cmd_quote(value: &str) -> Result<String, String> {
    Ok(format!(
        "\"{}\"",
        one_line(value)?.replace('%', "%%").replace('"', "\"\"")
    ))
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What would be written: where, and the text.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub kind: Kind,
    pub path: PathBuf,
    pub text: String,
    /// The unit / label / file name.
    pub name: String,
}

fn absolute(p: &Path) -> PathBuf {
    std::fs::canonicalize(p)
        .or_else(|_| std::path::absolute(p))
        .unwrap_or_else(|_| p.to_path_buf())
}

/// The library server's file: `exe -c <config> serve`.
pub fn render_server(kind: Kind, config: &Path, exe: &Path) -> Result<Rendered, String> {
    let config = absolute(config);
    let exe_s = exe.to_string_lossy().into_owned();
    let cfg_s = config.to_string_lossy().into_owned();
    Ok(match kind {
        Kind::SystemdUser => Rendered {
            kind,
            path: user_unit_dir().join(SERVER_UNIT),
            name: SERVER_UNIT.into(),
            text: format!(
                "[Unit]\n\
                 Description=Mokuro Bunko library server\n\
                 Documentation=https://github.com/Gnathonic/mokuro-bunko/blob/main/docs/deployment.md\n\
                 Wants=network-online.target\n\
                 After=network-online.target\n\
                 \n\
                 [Service]\n\
                 Type=simple\n\
                 ExecStart={} -c {} serve\n\
                 Restart=on-failure\n\
                 RestartSec=10s\n\
                 TimeoutStopSec=60s\n\
                 \n\
                 [Install]\n\
                 WantedBy=default.target\n",
                systemd_word(&exe_s)?,
                systemd_word(&cfg_s)?
            ),
        },
        Kind::WindowsStartup => Rendered {
            kind,
            path: startup_dir().join(SERVER_STARTUP),
            name: SERVER_STARTUP.into(),
            text: format!(
                "@echo off\r\n\
                 rem mokuro-bunko library server: started at logon. Delete this file to stop that.\r\n\
                 start \"mokuro-bunko\" /min {} -c {} serve\r\n",
                cmd_quote(&exe_s)?,
                cmd_quote(&cfg_s)?
            ),
        },
        Kind::Launchd => {
            let log = home().join("Library").join("Logs").join("mokuro-bunko.log");
            let mut program = String::new();
            for arg in [exe_s.as_str(), "-c", cfg_s.as_str(), "serve"] {
                program.push_str(&format!("    <string>{}</string>\n", xml(one_line(arg)?)));
            }
            let log = xml(&log.to_string_lossy());
            Rendered {
                kind,
                path: home()
                    .join("Library")
                    .join("LaunchAgents")
                    .join(format!("{SERVER_LAUNCHD}.plist")),
                name: SERVER_LAUNCHD.into(),
                text: format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                     <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
                     <plist version=\"1.0\">\n<dict>\n\
                     \x20 <key>Label</key>\n  <string>{SERVER_LAUNCHD}</string>\n\
                     \x20 <key>ProgramArguments</key>\n  <array>\n{program}  </array>\n\
                     \x20 <key>RunAtLoad</key>\n  <true/>\n\
                     \x20 <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n\
                     \x20 <key>StandardOutPath</key>\n  <string>{log}</string>\n\
                     \x20 <key>StandardErrorPath</key>\n  <string>{log}</string>\n\
                     </dict>\n</plist>\n"
                ),
            }
        }
    })
}

/// The processor's file, as `processor service` writes it.
#[cfg(feature = "ocr")]
pub fn render_processor(kind: Kind, config: &Path, exe: &Path) -> Result<Rendered, String> {
    use bunko_processor::service as ps;
    let k = match kind {
        Kind::SystemdUser => ps::ServiceKind::SystemdUser,
        Kind::WindowsStartup => ps::ServiceKind::WindowsStartup,
        Kind::Launchd => ps::ServiceKind::Launchd,
    };
    let r = ps::render_for(k, config, exe).map_err(|e| e.to_string())?;
    let name = match kind {
        Kind::SystemdUser => PROCESSOR_UNIT,
        Kind::WindowsStartup => PROCESSOR_STARTUP,
        Kind::Launchd => PROCESSOR_LAUNCHD,
    };
    Ok(Rendered {
        kind,
        path: r.path,
        text: r.text,
        name: name.into(),
    })
}

pub fn render(role: Role, config: &Path, exe: &Path) -> Result<Rendered, String> {
    let kind = platform_kind();
    match role {
        Role::Processor => {
            #[cfg(feature = "ocr")]
            {
                render_processor(kind, config, exe)
            }
            #[cfg(not(feature = "ocr"))]
            {
                let _ = (config, exe, kind);
                Err("this is the lite build: it has no processor".into())
            }
        }
        _ => render_server(kind, config, exe),
    }
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("`{program} {}` failed: {e}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Write the file; with `start`, also enable and start it. Returns what happened.
pub fn install(r: &Rendered, start: bool) -> Result<Vec<String>, String> {
    if let Some(dir) = r.path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(&r.path, &r.text)
        .map_err(|e| format!("could not write {}: {e}", r.path.display()))?;
    let mut messages = vec![format!("Wrote {}", r.path.display())];
    if !start {
        messages.push(match r.kind {
            Kind::SystemdUser => format!(
                "Not started. To start it now and at every login: systemctl --user daemon-reload && systemctl --user enable --now {}",
                r.name
            ),
            Kind::Launchd => format!(
                "Not started. It starts at your next login, or now with: launchctl bootstrap gui/$(id -u) {}",
                r.path.display()
            ),
            Kind::WindowsStartup => "It starts at your next logon.".into(),
        });
        return Ok(messages);
    }
    if !can_start(r.kind) {
        return Err(format!(
            "wrote {}, but this machine has no running service manager to start it",
            r.path.display()
        ));
    }
    match r.kind {
        Kind::SystemdUser => {
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", "--now", &r.name])?;
            messages.push(format!("Enabled and started {}", r.name));
            let user = std::env::var("USER").unwrap_or_default();
            let linger = run("loginctl", &["show-user", &user, "-p", "Linger"])
                .map(|o| o.contains("Linger=yes"))
                .unwrap_or(false);
            if !linger {
                messages.push(format!(
                    "This account does not linger: it stops when you log out and does not start at boot. To change that: loginctl enable-linger {user}"
                ));
            }
        }
        Kind::Launchd => {
            let uid = run("id", &["-u"])?.trim().to_string();
            let domain = format!("gui/{uid}");
            let _ = run("launchctl", &["bootout", &format!("{domain}/{}", r.name)]);
            run(
                "launchctl",
                &["bootstrap", &domain, &r.path.to_string_lossy()],
            )?;
            messages.push(format!("Loaded the launchd agent {}", r.name));
        }
        Kind::WindowsStartup => {
            let mut command = Command::new("cmd");
            command.arg("/c").arg(&r.path);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0000_0200 | 0x0000_0008);
            }
            command
                .spawn()
                .map_err(|e| format!("wrote {} but could not start it: {e}", r.path.display()))?;
            messages.push("Started it now in its own minimized window.".into());
        }
    }
    Ok(messages)
}

/// Whether the file is there, and what the service manager says of it.
pub fn state(r: &Rendered) -> (bool, Option<String>) {
    let written = r.path.is_file();
    let enabled = match r.kind {
        Kind::SystemdUser
            if written
                && can_start(r.kind)
                && run(
                    "systemctl",
                    &["--user", "show", "-p", "FragmentPath", "--value", &r.name],
                )
                .is_ok_and(|p| Path::new(p.trim()) == r.path) =>
        {
            Command::new("systemctl")
                .args(["--user", "is-enabled", &r.name])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        }
        _ => None,
    };
    (written, enabled)
}

/// Remove the file (after stopping and disabling a systemd unit).
pub fn remove(r: &Rendered) -> Result<Vec<String>, String> {
    let mut messages = Vec::new();
    // Only a unit the user manager loaded from this very file is disabled (a file
    // written under another XDG_CONFIG_HOME must not touch the real one).
    if r.kind == Kind::SystemdUser
        && can_start(r.kind)
        && run(
            "systemctl",
            &["--user", "show", "-p", "FragmentPath", "--value", &r.name],
        )
        .is_ok_and(|p| Path::new(p.trim()) == r.path)
        && run("systemctl", &["--user", "disable", "--now", &r.name]).is_ok()
    {
        messages.push(format!("Stopped and disabled {}", r.name));
    }
    if r.kind == Kind::Launchd
        && can_start(r.kind)
        && let Ok(uid) = run("id", &["-u"])
    {
        let _ = run(
            "launchctl",
            &["bootout", &format!("gui/{}/{}", uid.trim(), r.name)],
        );
    }
    if r.path.is_file() {
        std::fs::remove_file(&r.path)
            .map_err(|e| format!("could not remove {}: {e}", r.path.display()))?;
        messages.push(format!("Removed {}", r.path.display()));
    }
    Ok(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_files() {
        let r = render_server(
            Kind::SystemdUser,
            Path::new("/srv/my cfg/config.yaml"),
            Path::new("/opt/mokuro-bunko"),
        )
        .unwrap();
        assert!(
            r.text
                .contains("ExecStart=/opt/mokuro-bunko -c \"/srv/my cfg/config.yaml\" serve\n"),
            "{}",
            r.text
        );
        assert!(r.path.ends_with("systemd/user/mokuro-bunko.service"));
        let w = render_server(
            Kind::WindowsStartup,
            Path::new("C:/b/100%.yaml"),
            Path::new("C:/Program Files/mokuro-bunko.exe"),
        )
        .unwrap();
        assert!(w.text.contains("\"C:/Program Files/mokuro-bunko.exe\" -c"));
        assert!(w.text.contains("100%%.yaml"));
        let l = render_server(Kind::Launchd, Path::new("/a&b.yaml"), Path::new("/x")).unwrap();
        assert!(l.text.contains("a&amp;b.yaml"));
        assert!(render_server(Kind::SystemdUser, Path::new("/a\nb"), Path::new("/x")).is_err());
    }

    #[test]
    fn write_only_does_not_start() {
        let dir = tempfile::tempdir().unwrap();
        let r = Rendered {
            kind: Kind::SystemdUser,
            path: dir.path().join("u").join(SERVER_UNIT),
            text: "[Unit]\n".into(),
            name: SERVER_UNIT.into(),
        };
        let msgs = install(&r, false).unwrap();
        assert!(r.path.is_file());
        assert!(msgs.iter().any(|m| m.contains("Not started")));
        assert!(state(&r).0);
        // Removing a unit that is not enabled only deletes the file... but on a
        // machine with systemd it would also try `disable`: skip that part here.
        std::fs::remove_file(&r.path).unwrap();
        assert!(!state(&r).0);
    }
}
