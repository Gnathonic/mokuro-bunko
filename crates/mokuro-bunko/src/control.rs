//! The local control API of `serve` and `processor serve` (docs/rust-port/GUI.md §2;
//! stream G1): what both mount on top of `bunko-control` — the listener with the
//! desktop app pages (`gui::app`), the problems poller (the doctor's checks), the
//! backend pack's name.
//!
//! `MOKURO_CONTROL=off` turns it off; a failure to start it is logged and the instance
//! runs on without it (the tray then shows "not running").

use bunko_control::{Control, ControlListener, ControlServer, Problem, Role};
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How often the doctor's checks re-run for `problems`.
pub const PROBLEMS_EVERY: Duration = Duration::from_secs(120);

/// The machine's name in the status (the host name).
pub fn hostname() -> String {
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

/// Bind `127.0.0.1:<ephemeral>`, mount the app pages, write `.control.json`. None (and
/// a warning) when that fails: another live instance owns the storage, no loopback.
pub async fn start(
    control: &Control,
    config_path: PathBuf,
    processor_config: Option<PathBuf>,
) -> Option<ControlServer> {
    let listener = match ControlListener::bind().await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("Control API not started: binding 127.0.0.1 failed: {e}");
            return None;
        }
    };
    let app = crate::gui::app(
        control.role(),
        listener.token(),
        listener.login_codes(),
        config_path,
        processor_config,
    );
    match listener.serve(control.clone(), Some(app)) {
        Ok(server) => Some(server),
        Err(e) => {
            tracing::warn!("Control API not started: {e}");
            None
        }
    }
}

/// Keep `problems` current: `check` now and every [`PROBLEMS_EVERY`] on a blocking
/// thread, until `stop`; also at once whenever `again` changes (an OCR install ended).
pub fn watch_problems(
    control: Control,
    stop: CancellationToken,
    again: Option<tokio::sync::watch::Receiver<u64>>,
    check: impl Fn() -> Vec<Problem> + Send + Sync + 'static,
) {
    let check = std::sync::Arc::new(check);
    tokio::spawn(async move {
        let mut again = again;
        loop {
            let run = check.clone();
            if let Ok(problems) = tokio::task::spawn_blocking(move || run()).await {
                control.set_problems(problems);
            }
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = tokio::time::sleep(PROBLEMS_EVERY) => {}
                changed = async {
                    match again.as_mut() {
                        Some(rx) => rx.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if changed.is_err() {
                        again = None;
                    }
                }
            }
        }
    });
}

/// The server's library queue for the status (`library.queue_pending`), every few
/// seconds until `stop`.
pub fn watch_queue(
    control: Control,
    url: String,
    ocr: bunko_server::ocr::OcrControl,
    stop: CancellationToken,
) {
    tokio::spawn(async move {
        loop {
            let asked = ocr
                .ask(|s| {
                    let bench = s.paused_for_benchmark().map(|b| {
                        let generation =
                            b["generation"].as_str().unwrap_or("an engine").to_string();
                        let machine = b["processor"].as_str().unwrap_or("").to_string();
                        (generation, machine)
                    });
                    (s.pending_jobs().len() as u64, bench)
                })
                .await;
            let (pending, bench) = match asked {
                Some((p, b)) => (Some(p), b),
                None => (None, None),
            };
            let activity = bench.map(|(generation, machine)| {
                format!(
                    "Measuring {generation}'s speed on {} before its first volume (once per engine and machine; a few minutes)",
                    if machine.is_empty() || machine == "local" {
                        "this machine".to_string()
                    } else {
                        machine
                    }
                )
            });
            control.set_library(bunko_control::LibraryView {
                url: Some(url.clone()),
                connected: true,
                queue_pending: pending,
                error: None,
                activity,
            });
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(3)) => {}
            }
        }
    });
}

/// The library's own address (`server.host` 0.0.0.0 / :: shown as localhost).
pub fn library_url(config: &bunko_core::Config) -> String {
    let scheme = if config.ssl.enabled { "https" } else { "http" };
    let host = match config.server.host.as_str() {
        "0.0.0.0" | "::" | "" => "localhost",
        h => h,
    };
    format!("{scheme}://{host}:{}", config.server.port)
}

/// The backend pack the OCR runtime opens from `dirs` (`MOKURO_TORCH_PACK`, else
/// discovery order): "<variant> for <release> (<name>)", else its directory name.
#[cfg(feature = "ocr")]
pub fn pack_name(dirs: &[PathBuf]) -> Option<String> {
    use bunko_update::backend::{PACK_JSON, PackManifest};
    let dir = std::env::var_os(bunko_engines::torch::PACK_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| bunko_engines::torch::discover_all(dirs).into_iter().next())?;
    std::fs::read(dir.join(PACK_JSON))
        .ok()
        .and_then(|b| PackManifest::parse(&b).ok())
        .map(|m| pack_label(&m))
        .or_else(|| dir.file_name().map(|n| n.to_string_lossy().into_owned()))
}

/// `rocm7.1 for 0.7.0 (torch-rocm7.1-2.13.0)`: which release the pack belongs
/// to, so an OCR error is traceable to one version.
#[cfg(feature = "ocr")]
pub fn pack_label(m: &bunko_update::backend::PackManifest) -> String {
    let release = if m.bunko_version.trim().is_empty() {
        "a development build".to_string()
    } else {
        m.bunko_version.trim().to_string()
    };
    format!("{} for {release} ({})", m.variant, m.name)
}

/// The role's control config (`ocr`: this instance reads OCR itself).
pub fn config(
    role: Role,
    name: String,
    storage: &std::path::Path,
    ocr: bool,
) -> bunko_control::ControlConfig {
    let mut c = bunko_control::ControlConfig::new(role, name, bunko_core::VERSION, storage);
    c.ocr = ocr;
    c
}
