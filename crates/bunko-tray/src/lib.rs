//! The desktop tray of mokuro-bunko (GUI.md §5), run by `mokuro-bunko tray` (and on
//! Windows by `Mokuro Bunko.exe`, on macOS by opening the app).
//!
//! The logic lives in plain modules (discovery, the control API client, the menu
//! model, supervision, autostart) so it is testable without a desktop; `app` glues it
//! to the platform's tray: a StatusNotifierItem over D-Bus on Linux, tray-icon + muda
//! menus on a tao event loop on Windows and macOS.

pub mod app;
pub mod autostart;
pub mod autoupdate;
pub mod client;
pub mod discover;
pub mod icons;
pub mod launch;
pub mod model;
pub mod monitor;
pub mod notify;
pub mod paths;
#[cfg(target_os = "linux")]
pub mod session;
pub mod status;
pub mod supervise;
pub mod trayconf;
pub mod updates;

use anyhow::{Context, Result};
use paths::{Layout, ProcessEnv};
use std::path::PathBuf;

/// `mokuro-bunko tray`'s options.
#[derive(Debug, Clone)]
pub struct Options {
    /// Also look for an instance in these storages.
    pub storages: Vec<PathBuf>,
    /// Start the instances `tray.json` lists (false: only show and control what runs).
    pub supervise: bool,
    /// Log to stderr instead of the log file.
    pub log_stderr: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            storages: Vec::new(),
            supervise: true,
            log_stderr: false,
        }
    }
}

fn init_logging(
    opts: &Options,
    layout: &Layout,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("MOKURO_TRAY_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if opts.log_stderr {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .try_init();
        return None;
    }
    let dir = layout.writable_log_dir(&ProcessEnv);
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("mokuro-bunko-tray")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .try_init();
    Some(guard)
}

/// Run the tray until Quit. `exe` is this program; the instances it starts run the
/// `mokuro-bunko` command line found by [`paths::cli_exe`]. `first` runs once logging is
/// up and this tray holds the one-tray-per-user lock (the program's migration step).
pub fn run(opts: Options, exe: PathBuf, first: impl FnOnce()) -> Result<()> {
    let exe_dir = exe
        .parent()
        .map(PathBuf::from)
        .context("the tray executable has no folder")?;
    let layout = Layout::detect(&exe_dir);
    let _guard = init_logging(&opts, &layout);
    tracing::info!(
        "mokuro-bunko tray {} starting ({})",
        env!("CARGO_PKG_VERSION"),
        exe.display()
    );

    // One tray per user and data folder: a second start just exits.
    let lock_dir = layout.writable_log_dir(&ProcessEnv);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_dir.join(".tray.lock"))
        .with_context(|| format!("opening {}", lock_dir.join(".tray.lock").display()))?;
    match fs4::FileExt::try_lock(&lock) {
        Ok(()) => {}
        Err(fs4::TryLockError::WouldBlock) => {
            tracing::info!("another mokuro-bunko tray is running; exiting");
            return Ok(());
        }
        Err(fs4::TryLockError::Error(e)) => {
            tracing::warn!("could not lock the tray lock file: {e}")
        }
    }

    first();
    let cli = paths::cli_exe(&exe, &ProcessEnv);
    match &cli {
        Some(c) => tracing::info!("mokuro-bunko command line: {}", c.display()),
        None => tracing::warn!("no mokuro-bunko command line found next to the tray"),
    }
    app::run(app::Setup {
        opts,
        exe,
        layout,
        cli,
        lock,
    })
}
