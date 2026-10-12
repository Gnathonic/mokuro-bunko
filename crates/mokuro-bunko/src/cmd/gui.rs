//! `gui`: the desktop app with nothing running yet (first run, or reconfiguring a
//! stopped machine): the local control listener (bunko-control, role `gui`) with the
//! `/app` pages, then the browser at its one-time sign-in link.
//!
//! It exits on Ctrl+C, a little after the wizard hands the browser on to a server or
//! processor it started (`/app/api/handoff`), or when no page has talked to it for
//! [`IDLE_EXIT`] (the tab was closed). Double-clicking the program on Windows or
//! macOS runs this (`main.rs`).

use super::Ctx;
use crate::cli::GuiArgs;
use crate::gui::{self, AppState, Role};
use crate::out::{CmdResult, Fail};
use bunko_control::{Control, ControlConfig, ControlListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// No page request for this long: the tab is gone, exit.
pub const IDLE_EXIT: Duration = Duration::from_secs(600);

pub fn run(ctx: &Ctx, args: GuiArgs) -> CmdResult {
    crate::logging::init_console(ctx.verbose);
    let next = bunko_control::http::safe_next(Some(&args.open));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("gui")
        .build()?;
    runtime.block_on(async move {
        let storage = control_storage(&ctx.config_path)?;
        let name = hostname();
        let control = Control::new(ControlConfig::new(
            Role::Gui,
            name,
            bunko_core::VERSION,
            &storage,
        ));
        let listener = ControlListener::bind().await.map_err(Fail::from)?;
        let state = Arc::new(AppState::new(
            Role::Gui,
            listener.token().to_string(),
            listener.login_codes(),
            ctx.config_path.clone(),
            None,
        ));
        let app = gui::router(state.clone());
        let server = listener.serve(control, Some(app)).map_err(Fail::from)?;
        let url = server.login_url(Some(&next));
        println!("Mokuro Bunko desktop app: {url}");
        println!("(this window can stay in the background; Ctrl+C stops it)");
        if !args.no_browser
            && let Err(e) = open_browser(&url)
        {
            println!("Could not open the browser ({e}); open the address above.");
        }
        let idle = std::env::var("MOKURO_GUI_IDLE_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(IDLE_EXIT);
        let mut handoff = state.handoff.subscribe();
        loop {
            tokio::select! {
                _ = bunko_server::serve::shutdown_signal() => break,
                changed = handoff.changed() => {
                    if changed.is_err() || *handoff.borrow() {
                        println!("Handed over to the running instance; closing the setup app.");
                        break;
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(15)) => {
                    let quiet = gui::now_secs()
                        .saturating_sub(state.last_seen.load(Ordering::Relaxed));
                    let busy = state.jobs.all().iter().any(|j| j.state() == gui::jobs::JobState::Running);
                    if quiet >= idle.as_secs() && !busy {
                        println!("No page open for {} minutes; closing the setup app.", idle.as_secs() / 60);
                        break;
                    }
                }
            }
        }
        server.shutdown().await;
        Ok(())
    })
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this machine".into())
}

/// Where the `gui` instance writes its `.control.json`: the library's storage, else
/// the processor's, whichever has no live server/processor (never over theirs) and can
/// be written, else the first private per-user directory that can
/// ([`gui::paths::gui_fallback_storages`]). A default storage this user cannot write
/// (a `~/.local` owned by root) is skipped instead of failing: the wizard is where
/// that gets fixed.
fn control_storage(config_path: &std::path::Path) -> Result<PathBuf, Fail> {
    let candidates = [
        gui::paths::server_storage(config_path),
        gui::paths::processor_storage(&gui::paths::processor_config_path()),
    ];
    pick_control_storage(&candidates, &gui::paths::gui_fallback_storages())
}

fn pick_control_storage(candidates: &[PathBuf], fallbacks: &[PathBuf]) -> Result<PathBuf, Fail> {
    for dir in candidates {
        if let Some(f) = bunko_control::read_control_file(dir)
            && f.role != Role::Gui
            && gui::spawn::pid_alive(f.pid)
        {
            continue;
        }
        match gui::paths::ensure_writable(dir) {
            Ok(()) => return Ok(dir.clone()),
            Err(e) => tracing::warn!(
                "{} is not usable ({e}); trying the next folder",
                dir.display()
            ),
        }
    }
    let mut tried = Vec::new();
    for dir in fallbacks {
        match gui::paths::create_private_dir(dir).and_then(|()| gui::paths::ensure_writable(dir)) {
            Ok(()) => return Ok(dir.clone()),
            Err(e) => {
                tracing::warn!("{} is not usable ({e})", dir.display());
                tried.push(format!("{} ({e})", dir.display()));
            }
        }
    }
    Err(Fail::msg(format!(
        "no folder this user can write for the setup app: {}",
        tried.join("; ")
    )))
}

/// Open `url` in the default browser.
pub fn open_browser(url: &str) -> std::io::Result<()> {
    let mut cmd = if cfg!(windows) {
        // rundll32 takes the URL as one argument (cmd's `start` would split it at `&`).
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(all(test, unix))]
mod storage_tests {
    use super::*;

    /// A default storage under a folder this user cannot write (the owner's root-owned
    /// `~/.local`) is skipped, and the first writable private fallback is used.
    #[test]
    fn unwritable_candidates_are_skipped() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::getuid() } == 0 {
            return; // root writes anywhere
        }
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(".local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o555)).unwrap();
        let server = local.join("share/mokuro-bunko");
        let processor = local.join("share/mokuro-bunko-processor");
        let bad_fallback = local.join("share/mokuro-bunko/gui");
        let good_fallback = dir.path().join("config/mokuro-bunko/gui");
        let got = pick_control_storage(
            &[server.clone(), processor],
            &[bad_fallback.clone(), good_fallback.clone()],
        )
        .unwrap();
        assert_eq!(got, good_fallback);
        assert!(!server.exists());
        // Nothing usable at all: an error naming what was tried, not a panic.
        let err = pick_control_storage(&[server], &[bad_fallback]).unwrap_err();
        assert!(matches!(err, Fail::Error(ref m) if m.contains("no folder")));
        // A writable candidate is used as before.
        let ok = dir.path().join("lib");
        assert_eq!(
            pick_control_storage(std::slice::from_ref(&ok), &[]).unwrap(),
            ok
        );
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
