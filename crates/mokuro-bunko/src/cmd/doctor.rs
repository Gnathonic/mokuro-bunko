//! `doctor`: environment checks with PASS/WARN/FAIL rows and fix hints (0.5.2
//! `doctor_cli.py`). The Python and OCR-venv checks are replaced by build flavor, ONNX
//! Runtime and model checks; the disk threshold is retuned for a Rust install (no 8 GB
//! Python environment). Exit 1 on any FAIL.

use super::Ctx;
use crate::FLAVOR;
use crate::out::{Color, CmdResult, Fail, style};
use bunko_core::Config;
use std::net::TcpListener;
use std::path::{Path, PathBuf};

/// Free space under which `Disk space` warns.
const LOW_DISK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

struct Check {
    status: Status,
    label: &'static str,
    detail: String,
    hint: Option<String>,
}

impl Check {
    fn pass(label: &'static str, detail: impl Into<String>) -> Check {
        Check { status: Status::Pass, label, detail: detail.into(), hint: None }
    }
    fn warn(label: &'static str, detail: impl Into<String>, hint: Option<String>) -> Check {
        Check { status: Status::Warn, label, detail: detail.into(), hint }
    }
    fn fail(label: &'static str, detail: impl Into<String>, hint: Option<String>) -> Check {
        Check { status: Status::Fail, label, detail: detail.into(), hint }
    }
}

pub fn run(ctx: &Ctx) -> CmdResult {
    println!("mokuro-bunko {} - environment diagnostics\n", bunko_core::VERSION);

    let mut results = vec![check_build()];
    let (config_result, config) = check_config(&ctx.config_path);
    results.push(config_result);
    if let Some(c) = &config {
        for w in &c.warnings {
            results.push(Check::warn("Config", w.clone(), None));
        }
    }
    #[cfg(feature = "ocr")]
    results.push(check_onnx_runtime());
    if let Some(config) = &config {
        let storage = &config.storage.base_path;
        #[cfg(feature = "ocr")]
        results.push(check_models(&config.storage.layout().models()));
        results.push(check_disk(storage));
        results.push(check_port(&config.server.host, config.server.port));
        results.push(check_failures(storage));
    }

    for r in &results {
        let (word, color) = match r.status {
            Status::Pass => ("PASS", Color::Green),
            Status::Warn => ("WARN", Color::Yellow),
            Status::Fail => ("FAIL", Color::Red),
        };
        println!(" {}  {}: {}", style(&format!("{word:<4}"), color, true), r.label, r.detail);
        if let (Some(hint), false) = (&r.hint, r.status == Status::Pass) {
            println!("        -> {hint}");
        }
    }

    let failures = results.iter().filter(|r| r.status == Status::Fail).count();
    let warnings = results.iter().filter(|r| r.status == Status::Warn).count();
    println!();
    if failures > 0 {
        println!("{}", style(&format!("{failures} problem(s) found - see FAIL lines above."), Color::Red, true));
        return Err(Fail::Exit(1));
    }
    if warnings > 0 {
        println!("{}", style(&format!("OK with {warnings} warning(s) - see WARN lines above."), Color::Yellow, false));
        return Ok(());
    }
    println!("{}", style("All checks passed.", Color::Green, true));
    Ok(())
}

fn check_build() -> Check {
    let detail = format!("{FLAVOR} ({})", bunko_update::TARGET);
    if cfg!(feature = "ocr") {
        Check::pass("Build", detail)
    } else {
        Check::warn(
            "Build",
            format!("{detail} - OCR runs only on remote processors"),
            Some("Run 'mokuro-bunko processor serve' from a full build on another machine, or install the full build here.".into()),
        )
    }
}

fn check_config(path: &Path) -> (Check, Option<Config>) {
    let config = match bunko_core::config::load_config(Some(path)) {
        Ok(c) => c,
        Err(e) => {
            return (
                Check::fail(
                    "Config",
                    format!("{}: {e}", path.display()),
                    Some("Fix or delete the config file, then re-run 'mokuro-bunko setup'.".into()),
                ),
                None,
            );
        }
    };
    let base = config.storage.base_path.clone();
    let probe = base.join(".mokuro-doctor-probe");
    let writable = config
        .storage
        .layout()
        .ensure_directories()
        .and_then(|_| std::fs::write(&probe, "ok"))
        .and_then(|_| std::fs::remove_file(&probe));
    if let Err(e) = writable {
        return (
            Check::fail(
                "Storage",
                format!("{} is not writable: {e}", base.display()),
                Some("Point storage.base_path at a writable directory.".into()),
            ),
            Some(config),
        );
    }
    let exists = if path.exists() { "" } else { " (not found; using defaults)" };
    (Check::pass("Config", format!("{}{exists} - storage: {}", path.display(), base.display())), Some(config))
}

#[cfg(feature = "ocr")]
fn check_onnx_runtime() -> Check {
    match crate::ocr_probe::probe() {
        Ok(info) => Check::pass("ONNX Runtime", format!("{} - providers: {}", info.version, info.providers.join(", "))),
        Err(e) => Check::warn(
            "ONNX Runtime",
            e,
            Some("Local OCR is unavailable; remote processors still work. Reinstall this build or use the lite build.".into()),
        ),
    }
}

#[cfg(feature = "ocr")]
fn check_models(models_dir: &Path) -> Check {
    let manifest = models_dir.join("models.json");
    if manifest.is_file() {
        Check::pass("Models", models_dir.display().to_string())
    } else {
        Check::warn(
            "Models",
            format!("not downloaded (expected {})", manifest.display()),
            Some("Run: mokuro-bunko models download   (or start the server once; OCR downloads them on first use)".into()),
        )
    }
}

fn check_disk(storage: &Path) -> Check {
    let free = match fs4::available_space(storage) {
        Ok(f) => f,
        Err(e) => return Check::warn("Disk space", format!("could not check: {e}"), None),
    };
    let detail = format!("{:.1} GB free at {}", free as f64 / (1024.0 * 1024.0 * 1024.0), storage.display());
    if free < LOW_DISK_BYTES {
        Check::warn(
            "Disk space",
            detail,
            Some("OCR models need ~1-2 GB, plus room for uploads and the library itself.".into()),
        )
    } else {
        Check::pass("Disk space", detail)
    }
}

fn check_port(host: &str, port: u16) -> Check {
    let probe_host = match host {
        "0.0.0.0" | "" => "127.0.0.1",
        "::" => "::1",
        h => h,
    };
    match TcpListener::bind((probe_host, port)) {
        Ok(_) => Check::pass("Port", format!("{port} available on {probe_host}")),
        Err(_) => Check::warn(
            "Port",
            format!("{port} is in use on {probe_host} - is the server already running?"),
            Some("Stop the other process or change server.port in the config.".into()),
        ),
    }
}

fn check_failures(storage: &Path) -> Check {
    let count = std::fs::read_to_string(storage.join(".ocr-failures.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().map(|m| m.len()))
        .unwrap_or(0);
    if count == 0 {
        return Check::pass("Failed volumes", "none recorded");
    }
    let logs: PathBuf = storage.join("logs").join("ocr");
    Check::warn(
        "Failed volumes",
        format!("{count} volume(s) failing OCR (see the Queue page)"),
        Some(format!("Full per-volume logs: {}", logs.display())),
    )
}
