//! `doctor`: environment checks with PASS/WARN/FAIL rows and fix hints (0.5.2
//! `doctor_cli.py`). The Python and OCR-venv checks are replaced by build flavor, ONNX
//! Runtime and model checks; the disk threshold is retuned for a Rust install (no 8 GB
//! Python environment). Exit 1 on any FAIL.

use super::Ctx;
use crate::FLAVOR;
use crate::out::{CmdResult, Color, Fail, style};
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
        Check {
            status: Status::Pass,
            label,
            detail: detail.into(),
            hint: None,
        }
    }
    fn warn(label: &'static str, detail: impl Into<String>, hint: Option<String>) -> Check {
        Check {
            status: Status::Warn,
            label,
            detail: detail.into(),
            hint,
        }
    }
    fn fail(label: &'static str, detail: impl Into<String>, hint: Option<String>) -> Check {
        Check {
            status: Status::Fail,
            label,
            detail: detail.into(),
            hint,
        }
    }
}

pub fn run(ctx: &Ctx) -> CmdResult {
    println!(
        "mokuro-bunko {} - environment diagnostics\n",
        bunko_core::VERSION
    );

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
        results.push(check_backend(storage));
        #[cfg(feature = "ocr")]
        results.push(check_models(&config.storage.layout().models()));
        #[cfg(feature = "ocr")]
        if let Some(c) = check_packages(config) {
            results.push(c);
        }
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
        println!(
            " {}  {}: {}",
            style(&format!("{word:<4}"), color, true),
            r.label,
            r.detail
        );
        if let (Some(hint), false) = (&r.hint, r.status == Status::Pass) {
            println!("        -> {hint}");
        }
    }

    let failures = results.iter().filter(|r| r.status == Status::Fail).count();
    let warnings = results.iter().filter(|r| r.status == Status::Warn).count();
    println!();
    if failures > 0 {
        println!(
            "{}",
            style(
                &format!("{failures} problem(s) found - see FAIL lines above."),
                Color::Red,
                true
            )
        );
        return Err(Fail::Exit(1));
    }
    if warnings > 0 {
        println!(
            "{}",
            style(
                &format!("OK with {warnings} warning(s) - see WARN lines above."),
                Color::Yellow,
                false
            )
        );
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
    let exists = if path.exists() {
        ""
    } else {
        " (not found; using defaults)"
    };
    (
        Check::pass(
            "Config",
            format!("{}{exists} - storage: {}", path.display(), base.display()),
        ),
        Some(config),
    )
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

/// The OCR backend pack for this machine (install-ocr): present, complete, host
/// libraries there.
#[cfg(feature = "ocr")]
fn check_backend(storage: &Path) -> Check {
    use super::install_ocr::{backends_dir, missing_system_libs, pack_complete};
    use bunko_update::backend::{PACK_JSON, PackManifest};
    let hw = crate::hwdetect::detect();
    let want = crate::hwdetect::choose(&hw, bunko_update::TARGET);
    let root = backends_dir(storage);
    // The pack the OCR runtime will open: MOKURO_TORCH_PACK, else bunko-engines'
    // discovery order (a GPU pack whose driver is present, then cpu).
    let pinned = std::env::var_os(bunko_engines::torch::PACK_ENV)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let Some(dir) = pinned
        .clone()
        .or_else(|| bunko_engines::torch::discover(&root).into_iter().next())
    else {
        return Check::warn(
            "OCR backend",
            format!(
                "no backend pack installed in {} ({})",
                root.display(),
                want.reason
            ),
            Some(format!(
                "Run 'mokuro-bunko install-ocr' (installs the {} pack). Without it hayai-nova and paddle-manga cannot run here; ppocr-manga and remote processors still work.",
                want.variant
            )),
        );
    };
    let how = if pinned.is_some() {
        "MOKURO_TORCH_PACK"
    } else {
        "installed"
    };
    let m = match std::fs::read(dir.join(PACK_JSON))
        .map_err(|e| e.to_string())
        .and_then(|b| PackManifest::parse(&b).map_err(|e| e.to_string()))
    {
        Ok(m) => m,
        Err(e) => {
            return Check::fail(
                "OCR backend",
                format!("{} ({how}): {e}", dir.display()),
                Some("Run 'mokuro-bunko install-ocr --force'.".into()),
            );
        }
    };
    let detail = format!(
        "{} in use ({how}) at {}; this machine: {}",
        m.name,
        dir.display(),
        want.reason
    );
    if !pack_complete(&dir, &m) {
        return Check::fail(
            "OCR backend",
            format!("{detail}; files are missing or damaged"),
            Some("Run 'mokuro-bunko install-ocr --force'.".into()),
        );
    }
    let missing = missing_system_libs(&m.requires.system_libs);
    if !missing.is_empty() {
        return Check::warn(
            "OCR backend",
            format!("{detail}; missing host libraries: {}", missing.join(", ")),
            Some(
                "Install them with the system package manager (install-ocr prints the package names)."
                    .into(),
            ),
        );
    }
    // A GPU pack also runs on the CPU; only a GPU this pack cannot drive is worth a word.
    if want.variant != "cpu" && m.variant != want.variant {
        return Check::warn(
            "OCR backend",
            format!(
                "{detail}; the {} pack would use this machine's GPU",
                want.variant
            ),
            Some(format!(
                "mokuro-bunko install-ocr --variant {}",
                want.variant
            )),
        );
    }
    Check::pass("OCR backend", detail)
}

/// The compiled libtorch packages the enabled generations need on this machine's
/// device (`EnginePipeline::package_status_for`, each row's precision mode), and what
/// the backend set for its runtime (an RX 6600's `HSA_OVERRIDE_GFX_VERSION`). FAIL
/// when a row's package (with the weights it binds) or one of its host files is not on
/// disk (OCR would have to download it first), or nothing here can run it; None when no
/// enabled row uses a recognizer or no backend pack is installed (the OCR backend check
/// reports that).
#[cfg(feature = "ocr")]
fn check_packages(config: &bunko_core::Config) -> Option<Check> {
    use bunko_engines::models;
    let rows: Vec<(String, String)> = config
        .ocr
        .generations
        .iter()
        .filter(|g| g.enabled && g.retired.is_none())
        .filter(|g| g.engine == models::HAYAI || g.engine == models::PADDLE)
        .map(|g| (g.engine.clone(), g.precision.clone()))
        .collect();
    if rows.is_empty() {
        return None;
    }
    let pipeline = bunko_engines::EnginePipeline::new(bunko_engines::EngineConfig::new(
        config.storage.layout().models(),
        bunko_engines::Backend::parse(config.ocr.effective_backend()),
    ));
    // No backend pack: the OCR backend check already says so (and how to install one).
    let tb = pipeline.torch().ok()?;
    let notes: Vec<String> = tb
        .pack
        .env
        .iter()
        .filter(|(k, _)| k == "HSA_OVERRIDE_GFX_VERSION")
        .map(|(k, v)| format!("{k}={v} set (the card runs its family's ROCm target)"))
        .collect();
    let (mut present, mut missing, mut cannot) = (Vec::new(), Vec::new(), Vec::new());
    for (engine, mode) in &rows {
        match pipeline.package_status_for(engine, mode) {
            Ok(st) if st.ready() => present.push(format!(
                "{engine} {} on {} ({})",
                st.need.precision, st.need.device, st.need.label
            )),
            Ok(st) => {
                let mut what = Vec::new();
                if st.package.is_none() {
                    what.push(format!("package: one of {}", st.need.targets.join(", ")));
                }
                if !st.missing_host_files.is_empty() {
                    what.push(st.missing_host_files.join(", "));
                }
                missing.push(format!(
                    "{engine} {} on {} ({})",
                    st.need.precision,
                    st.need.device,
                    what.join("; ")
                ))
            }
            Err(e) => cannot.push(format!("{engine}: {e}")),
        }
    }
    let mut detail = Vec::new();
    if !present.is_empty() {
        detail.push(format!("present: {}", present.join("; ")));
    }
    if !missing.is_empty() {
        detail.push(format!("NOT DOWNLOADED: {}", missing.join("; ")));
    }
    if !cannot.is_empty() {
        detail.push(format!("NOT RUNNABLE HERE: {}", cannot.join("; ")));
    }
    detail.extend(notes);
    let detail = detail.join("; ");
    Some(if !cannot.is_empty() {
        Check::fail(
            "Compiled packages",
            detail,
            Some("Run 'mokuro-bunko install-ocr' for this machine's OCR backend; if it is installed, the models release has no package for this device.".into()),
        )
    } else if !missing.is_empty() {
        Check::fail(
            "Compiled packages",
            detail,
            Some("Run: mokuro-bunko models download".into()),
        )
    } else {
        Check::pass("Compiled packages", detail)
    })
}

#[cfg(feature = "ocr")]
fn check_models(models_dir: &Path) -> Check {
    // What the engines would use: the store (and `MOKURO_MODELS_DIR`), PP-OCR plus every
    // engine's fp32 set; missing files are fetched when a session first needs them.
    let config =
        bunko_engines::EngineConfig::new(models_dir.to_path_buf(), bunko_engines::Backend::Auto);
    let store = config.store();
    let ids = bunko_engines::models::download_plan(None, false);
    let missing: Vec<&str> = ids
        .iter()
        .copied()
        .filter(|id| store.locate(id).is_none())
        .collect();
    if missing.is_empty() {
        return Check::pass("Models", models_dir.display().to_string());
    }
    let bytes: u64 = missing
        .iter()
        .filter_map(|id| store.manifest().get(id))
        .map(|f| f.size)
        .sum();
    let detail = format!(
        "{} of {} files not downloaded yet ({:.1} GB) under {}",
        missing.len(),
        ids.len(),
        bytes as f64 / 1e9,
        models_dir.display()
    );
    if store.can_download() {
        Check::warn(
            "Models",
            detail,
            Some("Run: mokuro-bunko models download   (or start the server once; OCR downloads them on first use)".into()),
        )
    } else {
        Check::warn(
            "Models",
            format!("{detail}; downloads are off (MOKURO_MODELS_DOWNLOAD)"),
            Some("Copy the model files into the models directory, or allow downloads.".into()),
        )
    }
}

fn check_disk(storage: &Path) -> Check {
    let free = match fs4::available_space(storage) {
        Ok(f) => f,
        Err(e) => return Check::warn("Disk space", format!("could not check: {e}"), None),
    };
    let detail = format!(
        "{:.1} GB free at {}",
        free as f64 / (1024.0 * 1024.0 * 1024.0),
        storage.display()
    );
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
