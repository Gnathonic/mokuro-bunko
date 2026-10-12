//! `processor service`: start the processor with the machine, written from the
//! running binary itself (its own path, the absolute path of the processor.yaml it
//! was given). 0.5.2 `processor/service.py`, plus macOS.
//!
//! * Linux: a systemd USER unit `~/.config/systemd/user/mokuro-bunko-processor.service`
//!   (`$XDG_CONFIG_HOME` respected), `systemctl --user daemon-reload` + `enable --now`.
//! * Windows: `mokuro-bunko-processor.cmd` in the user's Startup folder (no admin
//!   needed), started at once in its own minimized window.
//! * macOS: a launchd agent `~/Library/LaunchAgents/<label>.plist`, bootstrapped into
//!   the user's GUI domain.
//!
//! SIGTERM stops the processor cleanly everywhere: the volumes it held go back to the
//! library's queue unrecorded.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config::home_dir;

pub const UNIT_NAME: &str = "mokuro-bunko-processor.service";
pub const STARTUP_NAME: &str = "mokuro-bunko-processor.cmd";
pub const LAUNCHD_LABEL: &str = "io.github.gnathonic.mokuro-bunko-processor";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ServiceError(pub String);

fn fail(message: impl Into<String>) -> ServiceError {
    ServiceError(message.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    SystemdUser,
    WindowsStartup,
    Launchd,
}

/// The kind this platform uses, if it can be installed here.
pub fn kind() -> Option<ServiceKind> {
    if cfg!(windows) {
        return Some(ServiceKind::WindowsStartup);
    }
    if cfg!(target_os = "macos") {
        return which("launchctl").then_some(ServiceKind::Launchd);
    }
    if cfg!(target_os = "linux") && which("systemctl") && Path::new("/run/systemd/system").is_dir()
    {
        return Some(ServiceKind::SystemdUser);
    }
    None
}

fn which(program: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

/// The binary that is running now (what the service runs).
pub fn entry_point() -> Result<PathBuf, ServiceError> {
    std::env::current_exe()
        .map_err(|e| fail(format!("could not find this program's own path: {e}")))
}

fn absolute(config: &Path) -> PathBuf {
    std::fs::canonicalize(config)
        .or_else(|_| std::path::absolute(config))
        .unwrap_or_else(|_| config.to_path_buf())
}

fn one_line(value: &str) -> Result<&str, ServiceError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(fail(format!(
            "{value:?} holds a line break; move it to a path without one"
        )));
    }
    Ok(value)
}

/// One ExecStart word, quoted the way systemd reads it (its `%` specifiers and `$`
/// variables escaped).
fn systemd_word(value: &str) -> Result<String, ServiceError> {
    let value = one_line(value)?.replace('%', "%%").replace('$', "$$");
    if value.chars().any(char::is_whitespace) || value.contains('"') {
        return Ok(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        ));
    }
    Ok(value)
}

/// The systemd user unit.
pub fn render_user_unit(config: &Path, exe: &Path) -> Result<String, ServiceError> {
    let exec = [
        systemd_word(&exe.to_string_lossy())?,
        "processor".into(),
        "serve".into(),
        "--config".into(),
        systemd_word(&absolute(config).to_string_lossy())?,
    ]
    .join(" ");
    Ok(format!(
        "[Unit]\n\
         Description=Mokuro Bunko OCR processor\n\
         Documentation=https://github.com/Gnathonic/mokuro-bunko/blob/main/docs/deployment.md#remote-ocr-processors\n\
         # The processor dials out to the library; it needs the network, not a port.\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         # SIGTERM stops it cleanly: the volumes it held go back to the library's\n\
         # queue unrecorded. It reconnects by itself when the library restarts.\n\
         Restart=on-failure\n\
         RestartSec=10s\n\
         TimeoutStopSec=60s\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

pub fn user_unit_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"));
    base.join("systemd").join("user")
}

fn run(program: &str, args: &[&str]) -> Result<String, ServiceError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| fail(format!("`{program} {}` failed: {e}", args.join(" "))))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let text = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        return Err(fail(format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            text.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Write the unit, reload the user manager, enable + start it. Returns the unit's
/// path and whether the account lingers (a user service of an account that does not
/// linger stops at logout and does not start at boot).
pub fn install_user_unit(text: &str) -> Result<(PathBuf, bool), ServiceError> {
    let dir = user_unit_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| fail(format!("could not create {}: {e}", dir.display())))?;
    let path = dir.join(UNIT_NAME);
    std::fs::write(&path, text)
        .map_err(|e| fail(format!("could not write {}: {e}", path.display())))?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", "--now", UNIT_NAME])?;
    let user = std::env::var("USER").unwrap_or_else(|_| {
        home_dir()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let linger = run("loginctl", &["show-user", &user, "-p", "Linger"])
        .map(|o| o.contains("Linger=yes"))
        .unwrap_or(false);
    Ok((path, linger))
}

fn cmd_quote(value: &str) -> Result<String, ServiceError> {
    // `%` doubled: a .cmd file expands `%VAR%` even inside quotes.
    Ok(format!(
        "\"{}\"",
        one_line(value)?.replace('%', "%%").replace('"', "\"\"")
    ))
}

/// The Windows Startup entry (CRLF, as cmd expects).
pub fn render_windows_startup(config: &Path, exe: &Path) -> Result<String, ServiceError> {
    Ok(format!(
        "@echo off\r\n\
         rem mokuro-bunko OCR processor: started at logon. Delete this file to stop that.\r\n\
         start \"mokuro-bunko processor\" /min {} processor serve --config {}\r\n",
        cmd_quote(&exe.to_string_lossy())?,
        cmd_quote(&absolute(config).to_string_lossy())?
    ))
}

pub fn startup_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join("AppData").join("Roaming"));
    base.join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join("Startup")
}

/// Write the Startup entry and start it now; its path.
pub fn install_windows_startup(config: &Path, exe: &Path) -> Result<PathBuf, ServiceError> {
    let dir = startup_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| fail(format!("could not create {}: {e}", dir.display())))?;
    let path = dir.join(STARTUP_NAME);
    std::fs::write(&path, render_windows_startup(config, exe)?)
        .map_err(|e| fail(format!("could not write {}: {e}", path.display())))?;
    let mut command = Command::new("cmd");
    command.arg("/c").arg(&path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    command.spawn().map_err(|e| {
        fail(format!(
            "wrote {} but could not start it now: {e}",
            path.display()
        ))
    })?;
    Ok(path)
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn launch_agents_dir() -> PathBuf {
    home_dir().join("Library").join("LaunchAgents")
}

pub fn launchd_log() -> PathBuf {
    home_dir()
        .join("Library")
        .join("Logs")
        .join("mokuro-bunko-processor.log")
}

/// The macOS launchd agent.
pub fn render_launchd_plist(config: &Path, exe: &Path) -> Result<String, ServiceError> {
    let args = [
        exe.to_string_lossy().into_owned(),
        "processor".into(),
        "serve".into(),
        "--config".into(),
        absolute(config).to_string_lossy().into_owned(),
    ];
    let mut program = String::new();
    for arg in &args {
        program.push_str(&format!("    <string>{}</string>\n", xml(one_line(arg)?)));
    }
    let log = xml(&launchd_log().to_string_lossy());
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key>\n\
         \x20 <string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array>\n\
         {program}\
         \x20 </array>\n\
         \x20 <key>RunAtLoad</key>\n\
         \x20 <true/>\n\
         \x20 <key>KeepAlive</key>\n\
         \x20 <dict>\n\
         \x20   <key>SuccessfulExit</key>\n\
         \x20   <false/>\n\
         \x20 </dict>\n\
         \x20 <key>ThrottleInterval</key>\n\
         \x20 <integer>10</integer>\n\
         \x20 <key>StandardOutPath</key>\n\
         \x20 <string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key>\n\
         \x20 <string>{log}</string>\n\
         </dict>\n\
         </plist>\n"
    ))
}

/// Write the agent and (re)load it into the user's GUI domain; its path.
pub fn install_launchd(text: &str) -> Result<PathBuf, ServiceError> {
    let dir = launch_agents_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| fail(format!("could not create {}: {e}", dir.display())))?;
    if let Some(logs) = launchd_log().parent() {
        let _ = std::fs::create_dir_all(logs);
    }
    let path = dir.join(format!("{LAUNCHD_LABEL}.plist"));
    std::fs::write(&path, text)
        .map_err(|e| fail(format!("could not write {}: {e}", path.display())))?;
    let uid = run("id", &["-u"])?.trim().to_string();
    let domain = format!("gui/{uid}");
    let _ = run(
        "launchctl",
        &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
    );
    run(
        "launchctl",
        &["bootstrap", &domain, &path.to_string_lossy()],
    )?;
    Ok(path)
}

/// What `processor service` (without `--install`) prints: where the file would go,
/// and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    pub kind: ServiceKind,
    pub path: PathBuf,
    pub text: String,
}

/// Render this platform's service file for `config` (running this binary).
pub fn render(config: &Path) -> Result<Rendered, ServiceError> {
    let exe = entry_point()?;
    render_for(platform_kind(), config, &exe)
}

fn platform_kind() -> ServiceKind {
    if cfg!(windows) {
        ServiceKind::WindowsStartup
    } else if cfg!(target_os = "macos") {
        ServiceKind::Launchd
    } else {
        ServiceKind::SystemdUser
    }
}

pub fn render_for(kind: ServiceKind, config: &Path, exe: &Path) -> Result<Rendered, ServiceError> {
    Ok(match kind {
        ServiceKind::SystemdUser => Rendered {
            kind,
            path: user_unit_dir().join(UNIT_NAME),
            text: render_user_unit(config, exe)?,
        },
        ServiceKind::WindowsStartup => Rendered {
            kind,
            path: startup_dir().join(STARTUP_NAME),
            text: render_windows_startup(config, exe)?,
        },
        ServiceKind::Launchd => Rendered {
            kind,
            path: launch_agents_dir().join(format!("{LAUNCHD_LABEL}.plist")),
            text: render_launchd_plist(config, exe)?,
        },
    })
}

/// What an install did, for the person and the setup summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub kind: ServiceKind,
    pub path: PathBuf,
    /// Lines to print.
    pub messages: Vec<String>,
    /// The setup summary's `Running:` text.
    pub running: String,
    /// The setup summary's `Logs:` text.
    pub logs: String,
}

/// Install and start the service for `config`.
pub fn install(config: &Path) -> Result<Installed, ServiceError> {
    let kind = kind().ok_or_else(|| {
        fail("this machine has no supported service manager (systemd user units, launchd, or a Windows Startup folder); run `processor serve` yourself")
    })?;
    let exe = entry_point()?;
    match kind {
        ServiceKind::SystemdUser => {
            let (path, linger) = install_user_unit(&render_user_unit(config, &exe)?)?;
            let mut messages = vec![format!(
                "Installed and started {} ({})",
                UNIT_NAME,
                path.display()
            )];
            if !linger {
                let user = std::env::var("USER").unwrap_or_default();
                messages.push(format!(
                    "This account does not linger: the processor stops when you log out and does not start at boot. To change that: loginctl enable-linger {user}"
                ));
            }
            let logs = format!("journalctl --user -u {UNIT_NAME} -f");
            messages.push(format!("Its log: {logs}"));
            Ok(Installed {
                kind,
                path,
                messages,
                running: format!("yes, as the systemd user service {UNIT_NAME}"),
                logs,
            })
        }
        ServiceKind::WindowsStartup => {
            let path = install_windows_startup(config, &exe)?;
            Ok(Installed {
                kind,
                messages: vec![format!(
                    "Added {}: the processor starts, minimized, at every logon. Started it now in its own window.",
                    path.display()
                )],
                path,
                running: format!(
                    "yes, in its own minimized window; at every logon from {STARTUP_NAME}"
                ),
                logs: "its window".to_string(),
            })
        }
        ServiceKind::Launchd => {
            let text = render_launchd_plist(config, &exe)?;
            let path = install_launchd(&text)?;
            let log = launchd_log();
            Ok(Installed {
                kind,
                messages: vec![format!(
                    "Installed and started the launchd agent {} ({})",
                    LAUNCHD_LABEL,
                    path.display()
                )],
                path,
                running: format!("yes, as the launchd agent {LAUNCHD_LABEL}"),
                logs: format!("tail -f {}", log.display()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// systemd units exist on Linux only; elsewhere these POSIX paths are not absolute.
    #[cfg(unix)]
    #[test]
    fn unit_quotes_and_escapes() {
        let text = render_user_unit(
            Path::new("/srv/my config/p%1.yaml"),
            Path::new("/opt/bunko/mokuro-bunko"),
        )
        .unwrap();
        assert!(
            text.contains("ExecStart=/opt/bunko/mokuro-bunko processor serve --config \"/srv/my config/p%%1.yaml\"\n"),
            "{text}"
        );
        assert!(text.contains("WantedBy=default.target"));
        assert!(render_user_unit(Path::new("/a\nb"), Path::new("/x")).is_err());
    }

    #[test]
    fn windows_entry_is_crlf_and_quoted() {
        let text = render_windows_startup(
            Path::new("C:/p/100%.yaml"),
            Path::new("C:/Program Files/mokuro-bunko.exe"),
        )
        .unwrap();
        assert!(text.starts_with("@echo off\r\n"));
        assert!(text.contains("\"C:/Program Files/mokuro-bunko.exe\" processor serve --config"));
        assert!(text.contains("100%%.yaml"));
        assert!(text.ends_with("\r\n"));
    }

    #[test]
    fn plist_lists_the_arguments() {
        let text = render_launchd_plist(
            Path::new("/Users/me/p&q.yaml"),
            Path::new("/usr/local/bin/mokuro-bunko"),
        )
        .unwrap();
        assert!(text.contains("<string>/usr/local/bin/mokuro-bunko</string>"));
        assert!(text.contains("p&amp;q.yaml"));
        assert!(text.contains(LAUNCHD_LABEL));
    }
}
