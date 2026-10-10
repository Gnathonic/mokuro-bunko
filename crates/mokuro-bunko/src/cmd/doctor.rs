//! `doctor`: environment checks with PASS/WARN/FAIL rows and fix hints (0.5.2
//! `doctor_cli.py`). The Python and OCR-venv checks are replaced by build flavor, ONNX
//! Runtime and model checks; the disk threshold is retuned for a Rust install (no 8 GB
//! Python environment). Exit 1 on any FAIL.
//!
//! On a processor machine (`--processor`, or a processor.yaml and no library
//! configuration; [`crate::ocr_target`]) it checks the processor instead: its
//! processor.yaml, the backend pack `processor serve` will open (its own storage, then
//! the library's) and the models in its storage.

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
    /// Worth knowing, nothing to fix: not counted as a warning.
    Info,
    Warn,
    Fail,
}

struct Check {
    status: Status,
    label: &'static str,
    detail: String,
    hint: Option<String>,
    /// A warning here, but in a running instance's status a `fail` problem of kind
    /// `update` (the tray flags it and notifies): something only the owner can do.
    needs_you: bool,
}

impl Check {
    fn pass(label: &'static str, detail: impl Into<String>) -> Check {
        Check {
            status: Status::Pass,
            label,
            detail: detail.into(),
            hint: None,
            needs_you: false,
        }
    }
    #[cfg_attr(not(feature = "ocr"), allow(dead_code))]
    fn info(label: &'static str, detail: impl Into<String>) -> Check {
        Check {
            status: Status::Info,
            label,
            detail: detail.into(),
            hint: None,
            needs_you: false,
        }
    }
    fn warn(label: &'static str, detail: impl Into<String>, hint: Option<String>) -> Check {
        Check {
            status: Status::Warn,
            label,
            detail: detail.into(),
            hint,
            needs_you: false,
        }
    }
    #[cfg_attr(not(feature = "ocr"), allow(dead_code))]
    fn warn_needs_you(
        label: &'static str,
        detail: impl Into<String>,
        hint: Option<String>,
    ) -> Check {
        Check {
            needs_you: true,
            ..Check::warn(label, detail, hint)
        }
    }
    fn fail(label: &'static str, detail: impl Into<String>, hint: Option<String>) -> Check {
        Check {
            status: Status::Fail,
            label,
            detail: detail.into(),
            hint,
            needs_you: false,
        }
    }
}

pub fn run(ctx: &Ctx, processor: bool) -> CmdResult {
    println!(
        "mokuro-bunko {} - environment diagnostics\n",
        bunko_core::VERSION
    );

    #[cfg(feature = "ocr")]
    {
        let processor_machine = processor
            || (!crate::machine::library_configured(&ctx.config_path)
                && crate::machine::find_processor_config().is_some());
        if processor_machine {
            let mut results = vec![check_build()];
            results.extend(processor_checks(ctx, processor));
            results.extend(check_tray_libs());
            return report(&results);
        }
    }
    #[cfg(not(feature = "ocr"))]
    if processor {
        println!("--processor: this is the lite build, which has no processor.\n");
    }

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
        {
            let target = crate::ocr_target::OcrTarget {
                role: crate::ocr_target::Role::Library,
                storage: storage.clone(),
                processor_config: None,
                // `ocr.backend` decides which pack fits (check_backend).
                library: Some(config.clone()),
                reason: String::new(),
            };
            results.extend(library_ocr_checks(&target, config));
            if let Some(p) = crate::machine::find_processor_config() {
                results.push(Check::pass(
                    "Processor",
                    format!(
                        "this machine also has {}; check it with: mokuro-bunko doctor --processor",
                        p.display()
                    ),
                ));
            }
        }
        results.push(check_disk(storage));
        results.push(check_port(&config.server.host, config.server.port));
        results.push(check_failures(storage));
    }
    results.extend(check_tray_libs());
    report(&results)
}

/// The processor's checks: its config, the pack `processor serve` opens, its models
/// and compiled packages, its disk.
#[cfg(feature = "ocr")]
fn processor_checks(ctx: &Ctx, flag: bool) -> Vec<Check> {
    let mut results = Vec::new();
    let target = match crate::ocr_target::resolve(ctx, true) {
        Ok(t) => t,
        Err(e) => {
            let msg = match e {
                Fail::Error(m) => m,
                Fail::Exit(c) => format!("exit {c}"),
            };
            results.push(Check::fail(
                "Processor config",
                msg,
                Some("Fix processor.yaml, or set it up again: mokuro-bunko processor setup".into()),
            ));
            return results;
        }
    };
    let why = if flag {
        String::new()
    } else {
        " (a processor.yaml and no library configuration here)".to_string()
    };
    results.push(match &target.processor_config {
        Some(p) => Check::pass(
            "Processor config",
            format!("{}{why} - storage: {}", p.display(), target.storage.display()),
        ),
        None => Check::warn(
            "Processor config",
            format!(
                "no processor.yaml found (MOKURO_PROCESSOR_CONFIG, {}, ./processor.yaml); default storage: {}",
                crate::machine::default_processor_config().display(),
                target.storage.display()
            ),
            Some("Set the processor up: mokuro-bunko processor setup (or the desktop app's Processor wizard).".into()),
        ),
    });
    results.push(check_backend(&target));
    results.push(check_models(
        &target.models_dir(),
        &super::models::planned_ids(&super::models::wanted_rows(&target, None), false),
        " --processor",
    ));
    if let Some(c) = check_packages(&target, &bunko_core::Config::default()) {
        results.push(c);
    }
    if let Some(c) = check_processor_storage(&target.storage) {
        results.push(c);
    }
    results.push(check_disk(&target.storage));
    results
}

fn report(results: &[Check]) -> CmdResult {
    for r in results {
        let (word, color) = match r.status {
            Status::Pass => ("PASS", Color::Green),
            Status::Info => ("INFO", Color::Cyan),
            Status::Warn => ("WARN", Color::Yellow),
            Status::Fail => ("FAIL", Color::Red),
        };
        println!(
            " {}  {}: {}",
            style(&format!("{word:<4}"), color, true),
            r.label,
            r.detail
        );
        if let (Some(hint), false) = (&r.hint, matches!(r.status, Status::Pass | Status::Info)) {
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

/// Why a storage folder cannot be used and what to do: the folder in the way (e.g. a
/// `~/.local` owned by root), the one-line `chown`, a writable alternative.
fn unwritable_storage(base: &Path, e: &std::io::Error, key: &str) -> (String, String) {
    let check = crate::gui::paths::storage_check(base);
    let mut detail = format!("{} is not writable: {e}", base.display());
    if let Some(p) = check["problem"].as_str() {
        detail.push_str(&format!(" - {p}"));
    }
    let mut hint = format!("Point {key} at a writable directory.");
    if let Some(alt) = check["suggestion"].as_str() {
        hint.push_str(&format!(" For example: {alt}."));
    }
    if let Some(fix) = check["fix"].as_str() {
        hint.push_str(&format!(" Or fix the permissions: {fix}"));
    }
    (detail, hint)
}

/// A processor storage this user can create and write.
#[cfg(feature = "ocr")]
fn check_processor_storage(storage: &Path) -> Option<Check> {
    let e = crate::gui::paths::ensure_writable(storage).err()?;
    let (detail, hint) = unwritable_storage(storage, &e, "processor.storage in processor.yaml");
    Some(Check::fail("Processor storage", detail, Some(hint)))
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
    let writable = config
        .storage
        .layout()
        .ensure_directories()
        .and_then(|_| bunko_core::storage::probe_writable(&base));
    if let Err(e) = writable {
        let (detail, hint) = unwritable_storage(&base, &e, "storage.base_path");
        return (Check::fail("Storage", detail, Some(hint)), Some(config));
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

/// The library's own OCR: the backend pack, the models and compiled packages its enabled
/// generations need. With `ocr.local_processing` off (or `ocr.backend: skip`) this
/// server runs no OCR, so none of that is needed: one INFO line, no warnings.
#[cfg(feature = "ocr")]
fn library_ocr_checks(target: &crate::ocr_target::OcrTarget, config: &Config) -> Vec<Check> {
    if !config.processes_locally(true) {
        let why = if config.ocr.backend == "skip" {
            "ocr.backend is skip"
        } else {
            "ocr.local_processing is off"
        };
        return vec![Check::info(
            "Local OCR",
            format!(
                "not used ({why}): this server reads no volume itself, connected processors do; the OCR backend pack and models are only needed to turn it on"
            ),
        )];
    }
    let mut out = vec![
        check_backend(target),
        check_models(
            &config.storage.layout().models(),
            &super::models::planned_ids(&super::models::wanted_rows(target, None), false),
            "",
        ),
    ];
    out.extend(check_packages(target, config));
    out
}

/// The OCR backend pack for this machine (install-ocr): present, complete, host
/// libraries there. The pack is the one this role's OCR runtime will open (for a
/// processor: its own storage first, then the library's).
#[cfg(feature = "ocr")]
fn check_backend(target: &crate::ocr_target::OcrTarget) -> Check {
    use super::install_ocr::{missing_system_libs, pack_complete};
    use bunko_update::backend::{PACK_JSON, PackManifest};
    let hw = crate::hwdetect::detect();
    // What `install-ocr` (and the Docker images' automatic install) would pick: the
    // owner's `ocr.backend` on this hardware.
    let want = crate::hwdetect::preferred(&target.backend_preference(), &hw, bunko_update::TARGET);
    let dirs = target.backends_dirs();
    let flag = if target.role == crate::ocr_target::Role::Processor {
        " --processor"
    } else {
        ""
    };
    // The pack the OCR runtime will open: MOKURO_TORCH_PACK, else bunko-engines'
    // discovery order (directory by directory; a GPU pack whose driver is present,
    // then cpu).
    let pinned = std::env::var_os(bunko_engines::torch::PACK_ENV)
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let Some(dir) = pinned
        .clone()
        .or_else(|| bunko_engines::torch::discover_all(&dirs).into_iter().next())
    else {
        return Check::warn(
            "OCR backend",
            format!(
                "no backend pack installed in {} ({})",
                dirs.iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" or "),
                want.reason
            ),
            Some(format!(
                "Run 'mokuro-bunko install-ocr{flag}' (installs the {} pack). Without it hayai-nova and paddle-manga cannot run here; ppocr-manga and remote processors still work.",
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
                Some(format!("Run 'mokuro-bunko install-ocr{flag} --force'.")),
            );
        }
    };
    let detail = format!(
        "{} in use ({how}) at {}; this machine: {}",
        crate::control::pack_label(&m),
        dir.display(),
        want.reason
    );
    // A pack belongs to exactly one release.
    if !bunko_engines::torch::abi::same_release(&m.bunko_version, bunko_core::VERSION) {
        return Check::fail(
            "OCR backend",
            format!(
                "{detail}; the backend pack is from mokuro-bunko {}, this is {}: each release runs only its own pack",
                m.bunko_version,
                bunko_core::VERSION
            ),
            Some(format!(
                "Run 'mokuro-bunko install-ocr{flag}' (with automatic updates on, an update installs the right pack itself)."
            )),
        );
    }
    if !pack_complete(&dir, &m) {
        return Check::fail(
            "OCR backend",
            format!("{detail}; files are missing or damaged"),
            Some(format!("Run 'mokuro-bunko install-ocr{flag} --force'.")),
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
        // The GPU changed since the pack went in (the install recorded what the hardware
        // called for then): the owner should switch. Without a record, or when the
        // hardware called for this already (a deliberate choice), only a warning.
        let changed = dir
            .parent()
            .and_then(super::install_ocr::HardwareRecord::read)
            .is_some_and(|r| r.auto_variant != want.variant);
        let make = if changed {
            Check::warn_needs_you
        } else {
            Check::warn
        };
        return make(
            "OCR backend",
            format!(
                "{detail}; the {} pack would use this machine's GPU{}",
                want.variant,
                if changed {
                    " (the GPU changed since the pack was installed)"
                } else {
                    ""
                }
            ),
            Some(format!(
                "mokuro-bunko install-ocr{flag} --variant {}",
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
fn check_packages(
    target: &crate::ocr_target::OcrTarget,
    config: &bunko_core::Config,
) -> Option<Check> {
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
    let pipeline = bunko_engines::EnginePipeline::new(target.engine_config(
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
            Some(format!(
                "Run: mokuro-bunko models download{}",
                if target.role == crate::ocr_target::Role::Processor {
                    " --processor"
                } else {
                    ""
                }
            )),
        )
    } else {
        Check::pass("Compiled packages", detail)
    })
}

/// The model files `ids` (what `models download` fetches for this machine: PP-OCR and
/// the fp32 sets of the engines the enabled generations use, every engine on a
/// processor) in the store (and `MOKURO_MODELS_DIR`); missing files are fetched when a
/// session first needs them.
#[cfg(feature = "ocr")]
fn check_models(models_dir: &Path, ids: &[&str], flag: &str) -> Check {
    let config =
        bunko_engines::EngineConfig::new(models_dir.to_path_buf(), bunko_engines::Backend::Auto);
    let store = config.store();
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
            Some(format!(
                "Run: mokuro-bunko models download{flag}   (or start the server or processor once; OCR downloads them on first use)"
            )),
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

/// Set by the running server on the `doctor` it starts (its port is not "in use").
pub const SERVING_ENV: &str = "MOKURO_DOCTOR_FROM_SERVER";

fn check_port(host: &str, port: u16) -> Check {
    // Run by the server itself (the admin panel's Diagnostics): the port is its own.
    if std::env::var_os(SERVING_ENV).is_some() {
        return Check::pass("Port", format!("{port}: this server serves on it"));
    }
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

/// The doctor's WARN/FAIL rows that matter while an instance runs, as the control
/// API's `problems` (GUI.md §2; stream G1): the disk and (the library server) volumes
/// failing OCR. The same checks `doctor` prints, so the tray and `doctor` agree.
pub fn control_problems(storage: &Path, library: bool) -> Vec<bunko_control::Problem> {
    let mut checks = vec![check_disk(storage)];
    if library {
        checks.push(check_failures(storage));
    }
    checks.into_iter().filter_map(as_problem).collect()
}

/// The OCR backend pack check (the pack this role opens), as a control problem.
#[cfg(feature = "ocr")]
pub fn backend_problem(target: &crate::ocr_target::OcrTarget) -> Option<bunko_control::Problem> {
    as_problem(check_backend(target))
}

fn as_problem(c: Check) -> Option<bunko_control::Problem> {
    let severity = match c.status {
        Status::Pass | Status::Info => return None,
        _ if c.needs_you => bunko_control::Severity::Fail,
        Status::Warn => bunko_control::Severity::Warn,
        Status::Fail => bunko_control::Severity::Fail,
    };
    Some(bunko_control::Problem {
        severity,
        text: format!("{}: {}", c.label, c.detail),
        hint: c.hint,
        kind: c
            .needs_you
            .then(|| bunko_control::Problem::KIND_UPDATE.to_string()),
    })
}

/// The desktop tray on Linux (`mokuro-bunko tray`, GUI.md §5): a StatusNotifierItem,
/// so it needs no GTK or AppIndicator library, only the session D-Bus and a
/// StatusNotifier host (the panel's tray; on GNOME the AppIndicator extension). Checked
/// in a desktop session only: a server or processor never needs it.
fn check_tray_libs() -> Option<Check> {
    #[cfg(all(target_os = "linux", feature = "tray"))]
    {
        let set = |v: &str| std::env::var_os(v).is_some_and(|x| !x.is_empty());
        if !set("DISPLAY") && !set("WAYLAND_DISPLAY") {
            return None;
        }
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        Some(tray_host_check(&bunko_tray::session::host(), &desktop))
    }
    #[cfg(not(all(target_os = "linux", feature = "tray")))]
    {
        None
    }
}

#[cfg_attr(not(all(target_os = "linux", feature = "tray")), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
enum TrayHost {
    Ready,
    NoBus(String),
    NoWatcher,
    NoHost,
}

#[cfg(all(target_os = "linux", feature = "tray"))]
fn tray_host_check(host: &bunko_tray::session::Host, desktop: &str) -> Check {
    use bunko_tray::session::Host;
    tray_check(
        &match host {
            Host::Ready => TrayHost::Ready,
            Host::NoBus(e) => TrayHost::NoBus(e.clone()),
            Host::NoWatcher => TrayHost::NoWatcher,
            Host::NoHost => TrayHost::NoHost,
        },
        desktop,
    )
}

#[cfg_attr(not(all(target_os = "linux", feature = "tray")), allow(dead_code))]
fn tray_check(host: &TrayHost, desktop: &str) -> Check {
    let gnome = desktop.to_ascii_lowercase().contains("gnome");
    let hint = if gnome {
        "GNOME shows tray icons with the \"AppIndicator and KStatusNotifierItem Support\" extension: \
         Debian/Ubuntu: apt install gnome-shell-extension-appindicator; Fedora: dnf install \
         gnome-shell-extension-appindicator; Arch: pacman -S gnome-shell-extension-appindicator; \
         then enable it (gnome-extensions enable appindicatorsupport@rgcjonas.gmail.com) and log in again. \
         The server and processor do not need it."
            .to_string()
    } else {
        "Add the panel's system tray (status notifier) widget, or use a desktop with one \
         (KDE Plasma, Xfce, LXQt, Cinnamon, MATE, Budgie; GNOME with the AppIndicator extension). \
         The server and processor do not need it."
            .to_string()
    };
    match host {
        TrayHost::Ready => Check::pass(
            "Desktop tray",
            "session D-Bus and a StatusNotifier host found (the tray icon can show)",
        ),
        TrayHost::NoBus(e) => Check::warn(
            "Desktop tray",
            format!("no session D-Bus ({e}): `mokuro-bunko tray` cannot show its icon"),
            Some(
                "Run it inside your desktop session (it needs DBUS_SESSION_BUS_ADDRESS). The server and processor do not need it."
                    .into(),
            ),
        ),
        TrayHost::NoWatcher | TrayHost::NoHost => Check::warn(
            "Desktop tray",
            "no StatusNotifier host in this session: the tray icon cannot show".to_string(),
            Some(hint),
        ),
    }
}

#[cfg(test)]
mod tray_lib_tests {
    use super::*;

    #[test]
    fn names_what_the_tray_needs() {
        assert!(tray_check(&TrayHost::Ready, "KDE").status == Status::Pass);
        let c = tray_check(&TrayHost::NoWatcher, "ubuntu:GNOME");
        assert!(c.status == Status::Warn);
        let hint = c.hint.unwrap();
        assert!(
            hint.contains("gnome-shell-extension-appindicator"),
            "{hint}"
        );
        assert!(!hint.contains("gtk"), "{hint}");
        let c = tray_check(&TrayHost::NoHost, "XFCE");
        assert!(c.hint.unwrap().contains("system tray"));
        let c = tray_check(&TrayHost::NoBus("no address".into()), "KDE");
        assert!(c.detail.contains("session D-Bus"));
    }
}

#[cfg(all(test, feature = "ocr"))]
mod library_ocr_tests {
    use super::*;
    use crate::ocr_target::{OcrTarget, Role};
    use bunko_engines::models;

    fn target(storage: &Path, config: &Config) -> OcrTarget {
        OcrTarget {
            role: Role::Library,
            storage: storage.to_path_buf(),
            processor_config: None,
            library: Some(config.clone()),
            reason: String::new(),
        }
    }

    /// Regression (upgrade test): with `ocr.local_processing` off, the full build's doctor
    /// still said to run `install-ocr` and download models. Nothing here needs them.
    #[test]
    fn local_ocr_off_is_one_info_line() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.base_path = dir.path().to_path_buf();
        config.ocr.local_processing = false;
        let checks = library_ocr_checks(&target(dir.path(), &config), &config);
        assert_eq!(checks.len(), 1);
        assert!(checks[0].status == Status::Info);
        assert_eq!(checks[0].label, "Local OCR");
        assert!(checks[0].detail.contains("ocr.local_processing is off"));
        assert!(checks[0].hint.is_none());
        assert!(
            as_problem(library_ocr_checks(&target(dir.path(), &config), &config).remove(0))
                .is_none()
        );
        config.ocr.local_processing = true;
        config.ocr.backend = "skip".into();
        let checks = library_ocr_checks(&target(dir.path(), &config), &config);
        assert_eq!(checks.len(), 1);
        assert!(checks[0].detail.contains("ocr.backend is skip"));
        // On: the backend pack and the models are checked (and missing here).
        config.ocr.backend = "auto".into();
        let checks = library_ocr_checks(&target(dir.path(), &config), &config);
        let labels: Vec<&str> = checks.iter().map(|c| c.label).collect();
        assert_eq!(&labels[..2], ["OCR backend", "Models"]);
        assert!(checks.iter().all(|c| c.status != Status::Info));
    }

    /// Regression (upgrade test): after a clean install, doctor warned that paddle-manga's
    /// files were not downloaded although no enabled generation uses paddle-manga.
    #[test]
    fn models_are_checked_for_the_enabled_generations_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.storage.base_path = dir.path().to_path_buf();
        let paddle_only: Vec<&str> = models::download_plan(Some(models::PADDLE), false)
            .into_iter()
            .filter(|id| !models::download_plan(Some(models::HAYAI), false).contains(id))
            .collect();
        assert!(!paddle_only.is_empty());
        let t = target(dir.path(), &config);
        let ids =
            super::super::models::planned_ids(&super::super::models::wanted_rows(&t, None), false);
        assert!(ids.iter().all(|id| !paddle_only.contains(id)), "{ids:?}");
        let mut want = models::download_plan(Some(models::PPOCR), false);
        for id in models::download_plan(Some(models::HAYAI), false) {
            if !want.contains(&id) {
                want.push(id);
            }
        }
        want.sort();
        let mut got = ids.clone();
        got.sort();
        assert_eq!(got, want);
        // The models row counts exactly those files (unless MOKURO_MODELS_DIR has them).
        let c = check_models(dir.path(), &ids, "");
        assert!(
            c.status == Status::Pass || c.detail.contains(&format!(" of {} files", ids.len())),
            "{}",
            c.detail
        );
        // Enabled, paddle-manga's files are wanted too.
        let rows = bunko_core::generations::parse_generation_list(&serde_json::json!([
            {"name": "hayai", "engine": "hayai-nova", "primary": true},
            {"name": "paddle", "engine": "paddle-manga", "primary": false}
        ]))
        .unwrap()
        .rows;
        config.ocr.generations = rows;
        let ids = super::super::models::planned_ids(
            &super::super::models::wanted_rows(&target(dir.path(), &config), None),
            false,
        );
        assert!(paddle_only.iter().all(|id| ids.contains(id)));
        // A processor runs whatever its library asks for: every engine.
        let mut p = target(dir.path(), &Config::default());
        p.role = Role::Processor;
        let ids =
            super::super::models::planned_ids(&super::super::models::wanted_rows(&p, None), false);
        assert!(paddle_only.iter().all(|id| ids.contains(id)));
    }
}
