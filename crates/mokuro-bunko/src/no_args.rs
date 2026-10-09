//! `mokuro-bunko` with no arguments starts the app, from any launcher:
//!
//! * in a desktop session (Windows, macOS, Linux with a display and a session bus) the
//!   tray, in the foreground: it runs what is configured, and on a machine with nothing
//!   set up opens the setup wizard in the browser (`mokuro-bunko tray`);
//! * headless (no display, an SSH session, a server): what is configured, in the
//!   foreground: `serve` for a library, `processor serve` for a processor, both when
//!   both are; with nothing set up, the terminal setup (`setup`) when there is a terminal
//!   to answer it, else a short message.
//!
//! `--help` / `help` print the help, and every explicit command works as before:
//! Docker's entrypoint, the systemd units, launchd agents and scripts all name theirs.
//! `MOKURO_DESKTOP=0|1` overrides the desktop detection (tests, odd sessions).

use std::path::{Path, PathBuf};

pub const NOTHING_SET_UP: &str = "Nothing is set up on this machine yet: run `mokuro-bunko setup` (or `mokuro-bunko gui` for the setup pages in a browser). `mokuro-bunko --help` lists the commands.";

pub const IN_THE_TRAY: &str =
    "Mokuro Bunko is running in the tray; `mokuro-bunko --help` lists the commands";

/// What a start without arguments does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Tray,
    Serve,
    Processor(PathBuf),
    Both(PathBuf),
    Setup,
    Message,
}

/// The rules (see the module docs), from what was found.
pub fn plan(
    desktop: bool,
    has_tray: bool,
    library: bool,
    processor: Option<PathBuf>,
    terminal: bool,
) -> Plan {
    if desktop && has_tray {
        return Plan::Tray;
    }
    match (library, processor) {
        (true, Some(p)) => Plan::Both(p),
        (true, None) => Plan::Serve,
        (false, Some(p)) => Plan::Processor(p),
        (false, None) if terminal => Plan::Setup,
        (false, None) => Plan::Message,
    }
}

/// What `main` runs.
pub enum Start {
    /// Parse these arguments.
    Run(Vec<String>),
    /// `serve` here and `processor serve --config <this>` as a child.
    Both(PathBuf),
    /// Print [`NOTHING_SET_UP`] and exit 0.
    Message,
}

pub fn start() -> Start {
    // As `-c` would: `MOKURO_CONFIG`, else the default config.yaml.
    let config = crate::cfgfile::resolve(
        std::env::var_os("MOKURO_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .as_deref(),
    );
    // A processor needs the full build; the lite build cannot run one.
    let processor = if cfg!(feature = "ocr") {
        crate::machine::find_processor_config()
    } else {
        None
    };
    let terminal = {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    };
    let p = plan(
        desktop_session(),
        cfg!(feature = "tray"),
        crate::machine::library_configured(&config),
        processor,
        terminal,
    );
    let argv = |a: &[&str]| -> Start {
        Start::Run(
            std::iter::once("mokuro-bunko")
                .chain(a.iter().copied())
                .map(str::to_string)
                .collect(),
        )
    };
    match p {
        Plan::Tray => {
            if !crate::windows_gui_subsystem() {
                eprintln!("{IN_THE_TRAY}");
            }
            argv(&["tray"])
        }
        Plan::Serve => argv(&["serve"]),
        Plan::Processor(c) => argv(&["processor", "serve", "--config", &c.to_string_lossy()]),
        Plan::Both(c) => Start::Both(c),
        Plan::Setup => argv(&["setup"]),
        Plan::Message => Start::Message,
    }
}

/// A desktop to show a tray on: `MOKURO_DESKTOP` if set; Windows always; macOS unless
/// this is an SSH session; elsewhere a display (`DISPLAY`/`WAYLAND_DISPLAY`) and a
/// session bus (`DBUS_SESSION_BUS_ADDRESS`, or `$XDG_RUNTIME_DIR/bus`).
pub fn desktop_session() -> bool {
    let var = |v: &str| std::env::var_os(v).filter(|x| !x.is_empty());
    if let Some(v) = var("MOKURO_DESKTOP") {
        return v != "0";
    }
    if cfg!(windows) {
        return true;
    }
    if cfg!(target_os = "macos") {
        return var("SSH_CONNECTION").is_none() && var("SSH_TTY").is_none();
    }
    let display = var("DISPLAY").is_some() || var("WAYLAND_DISPLAY").is_some();
    let bus = var("DBUS_SESSION_BUS_ADDRESS").is_some()
        || var("XDG_RUNTIME_DIR").is_some_and(|d| Path::new(&d).join("bus").exists());
    display && bus
}

/// Start `processor serve --config <config>` as a child of this process (same terminal);
/// it is stopped when this process ends (Linux: also if it dies).
pub fn spawn_processor(config: &Path) -> Option<std::process::Child> {
    let exe = bunko_update::current_exe().ok()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["processor", "serve", "--config"]).arg(config);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: prctl is async-signal-safe; nothing else runs between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    match cmd.spawn() {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("Error: could not start the processor: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rules() {
        let p = || Some(PathBuf::from("/c/processor.yaml"));
        // A desktop: the tray, whatever is set up (it runs that).
        assert_eq!(plan(true, true, false, None, true), Plan::Tray);
        assert_eq!(plan(true, true, true, p(), false), Plan::Tray);
        // Headless (or the lite build, which has no tray): what is configured.
        assert_eq!(plan(false, true, true, None, false), Plan::Serve);
        assert_eq!(plan(true, false, true, None, false), Plan::Serve);
        assert_eq!(
            plan(false, true, false, p(), false),
            Plan::Processor(PathBuf::from("/c/processor.yaml"))
        );
        assert_eq!(
            plan(false, true, true, p(), true),
            Plan::Both(PathBuf::from("/c/processor.yaml"))
        );
        // Nothing set up: the terminal setup, or a message without a terminal.
        assert_eq!(plan(false, true, false, None, true), Plan::Setup);
        assert_eq!(plan(false, true, false, None, false), Plan::Message);
    }
}
