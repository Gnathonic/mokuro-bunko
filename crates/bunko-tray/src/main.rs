//! `mokuro-bunko-tray`: the desktop tray (GUI.md §5). A separate executable from the
//! CLI so that only it links the desktop libraries (GTK 3 on Linux).
// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;

use anyhow::{Context, Result};
use bunko_tray::paths::{self, Layout, ProcessEnv};
use std::path::PathBuf;

const HELP: &str = "\
mokuro-bunko-tray: status, pause/resume and settings for mokuro-bunko

Usage: mokuro-bunko-tray [--storage DIR]... [--no-supervise] [--log-stderr]

Shows every mokuro-bunko instance on this machine (library server, OCR processor,
setup) in the system tray, found through <storage>/.control.json, and starts the
instances tray.json lists when none is running.

Options:
  --storage DIR     also look for an instance in DIR (repeatable)
  --no-supervise    do not start anything, only show and control what runs
  --log-stderr      log to stderr instead of the log file
  -V, --version     print the version
  -h, --help        print this help
";

pub struct Options {
    pub storages: Vec<PathBuf>,
    pub supervise: bool,
    pub log_stderr: bool,
}

fn parse_args() -> Result<Option<Options>> {
    let mut opts = Options {
        storages: Vec::new(),
        supervise: true,
        log_stderr: false,
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(a) = args.next() {
        match a.to_str() {
            Some("-h" | "--help") => {
                print!("{HELP}");
                return Ok(None);
            }
            Some("-V" | "--version") => {
                println!("mokuro-bunko-tray {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            Some("--storage") => opts
                .storages
                .push(args.next().context("--storage needs a directory")?.into()),
            Some("--no-supervise") => opts.supervise = false,
            Some("--log-stderr") => opts.log_stderr = true,
            // Old macOS versions pass -psn_… to apps the Finder starts.
            Some(s) if s.starts_with("-psn_") => {}
            _ => anyhow::bail!("unknown argument {a:?} (see --help)"),
        }
    }
    Ok(Some(opts))
}

fn init_logging(
    opts: &Options,
    layout: &Layout,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let filter = tracing_subscriber::EnvFilter::try_from_env("MOKURO_TRAY_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    if opts.log_stderr {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
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
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .init();
    Some(guard)
}

fn main() -> Result<()> {
    let Some(opts) = parse_args()? else {
        return Ok(());
    };
    let exe = std::env::current_exe().context("finding the tray's own path")?;
    let exe_dir = exe
        .parent()
        .map(PathBuf::from)
        .context("the tray executable has no folder")?;
    let layout = Layout::detect(&exe_dir);
    let _guard = init_logging(&opts, &layout);
    tracing::info!(
        "mokuro-bunko-tray {} starting ({})",
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
            tracing::info!("another mokuro-bunko-tray is running; exiting");
            return Ok(());
        }
        Err(fs4::TryLockError::Error(e)) => {
            tracing::warn!("could not lock the tray lock file: {e}")
        }
    }

    let cli = paths::cli_exe(&exe_dir, &ProcessEnv);
    match &cli {
        Some(c) => tracing::info!("mokuro-bunko executable: {}", c.display()),
        None => tracing::warn!("no mokuro-bunko executable found next to the tray or on PATH"),
    }
    app::run(app::Setup {
        opts,
        exe,
        layout,
        cli,
        lock,
    })
}
