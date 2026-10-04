//! `models list|download|verify` (full build): the ONNX model store under
//! `<storage>/models/` (ARCHITECTURE §7), over `bunko-ocr`'s `ModelStore` and its
//! builtin manifest (PP-OCR pinned on Hugging Face, hayai-nova / paddle-manga from the
//! `models-v1` release). `MOKURO_MODELS_DIR` names a directory used instead of the store
//! for the files it holds; `MOKURO_MODELS_DOWNLOAD=0` forbids downloads.

use super::Ctx;
use crate::cfgfile;
use crate::cli::ModelsCmd;
use crate::out::{CmdResult, Fail};
use bunko_engines::{Backend, EngineConfig, models};
use bunko_ocr::models::ModelStore;
use std::path::PathBuf;

pub fn run(ctx: &Ctx, cmd: ModelsCmd) -> CmdResult {
    crate::logging::init_console(ctx.verbose);
    match cmd {
        ModelsCmd::List => {
            let dir = models_dir(ctx)?;
            println!("Models directory: {}", dir.display());
            list(&store(dir))
        }
        ModelsCmd::Download { engine } => download(ctx, engine.as_deref()),
        ModelsCmd::Verify => verify(&store(models_dir(ctx)?)),
    }
}

fn models_dir(ctx: &Ctx) -> Result<PathBuf, Fail> {
    Ok(cfgfile::load_effective(&ctx.config_path)?
        .storage
        .layout()
        .models())
}

fn store(dir: PathBuf) -> ModelStore {
    EngineConfig::new(dir, Backend::Auto).store()
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn list(store: &ModelStore) -> CmdResult {
    if let Some(dir) = &store.options().override_dir {
        println!("Override directory (MOKURO_MODELS_DIR): {}", dir.display());
    }
    let mut missing = 0u64;
    for engine in models::ENGINES {
        println!("{engine}:");
        let ids = models::list_ids(engine);
        for file in ids.iter().filter_map(|id| store.manifest().get(id)) {
            let state = match store.locate(&file.id) {
                Some(path) => format!("present  {}", path.display()),
                None => {
                    missing += file.size;
                    "missing".to_string()
                }
            };
            println!("  {:<34} {:>10}  {}", file.id, mb(file.size), state);
        }
    }
    // The compiled libtorch packages this machine's default rows need.
    let pipeline = bunko_engines::EnginePipeline::new(EngineConfig::new(
        store.options().root.clone(),
        Backend::Auto,
    ));
    println!(
        "compiled packages ({}):",
        bunko_ocr::models::torch_release_name()
    );
    for engine in [models::HAYAI, models::PADDLE] {
        match pipeline.package_status(engine) {
            Ok(bunko_engines::PackageStatus {
                need,
                package: Some(dir),
                ..
            }) => println!(
                "  {engine:<14} {} on {} ({})  present  {}",
                need.precision,
                need.device,
                need.label,
                dir.display()
            ),
            Ok(st) => println!(
                "  {engine:<14} {} on {} ({})  missing  (one of {})",
                st.need.precision,
                st.need.device,
                st.need.label,
                st.need.targets.join(", ")
            ),
            Err(e) => println!("  {engine:<14} not runnable here: {e}"),
        }
    }
    if missing > 0 {
        println!(
            "Missing: {} (downloaded on first use{}; or run 'mokuro-bunko models download')",
            mb(missing),
            if store.can_download() {
                ""
            } else {
                " — but downloads are off here"
            }
        );
    }
    Ok(())
}

/// Also used by the deprecated `install-ocr`.
pub fn download(ctx: &Ctx, engine: Option<&str>) -> CmdResult {
    if let Some(e) = engine.filter(|e| !models::ENGINES.contains(e)) {
        return Err(Fail::msg(format!(
            "Unknown engine '{e}' (expected one of: {})",
            models::ENGINES.join(", ")
        )));
    }
    let dir = models_dir(ctx)?;
    let store = store(dir);
    bunko_engines::runtime::init();
    // What to fetch for: `--engine` (its default row), else the enabled generations
    // (each at its precision mode), else (no config) every engine's default row.
    let rows: Vec<(String, String)> = match (engine, cfgfile::load_effective(&ctx.config_path)) {
        (Some(e), _) => vec![(e.to_string(), "auto-accuracy".to_string())],
        (None, Ok(cfg)) => cfg
            .ocr
            .generations
            .iter()
            .filter(|g| g.enabled && g.retired.is_none())
            .map(|g| (g.engine.clone(), g.precision.clone()))
            .collect(),
        (None, Err(_)) => models::ENGINES
            .iter()
            .map(|e| (e.to_string(), "auto-accuracy".to_string()))
            .collect(),
    };
    let recognizer_rows: Vec<(&str, &str)> = rows
        .iter()
        .filter(|(e, _)| e == models::HAYAI || e == models::PADDLE)
        .map(|(e, m)| (e.as_str(), m.as_str()))
        .collect();
    // Device-independent files: PP-OCR (every engine reads lines with it) and the host
    // files of the recognizer engines those rows use.
    let gpu = bunko_ocr::runtime::ep_compiled().len() > 1;
    let mut ids: Vec<&'static str> = Vec::new();
    for e in models::ENGINES {
        let wanted = e == models::PPOCR || recognizer_rows.iter().any(|(r, _)| *r == e);
        if !wanted {
            continue;
        }
        for id in models::download_plan(Some(e), gpu) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let total: u64 = ids
        .iter()
        .filter(|id| store.locate(id).is_none())
        .filter_map(|id| store.manifest().get(id))
        .map(|f| f.size)
        .sum();
    println!(
        "Fetching {} into {} ({} to download)",
        engine.unwrap_or("the enabled generations"),
        store.options().root.display(),
        mb(total)
    );
    // Every failure is reported at the end; one file failing does not stop the others.
    let mut failed: Vec<String> = Vec::new();
    for id in ids {
        match store.ensure(id) {
            Ok(r) => println!(
                "  {id:<34} {}{}",
                if r.verified {
                    "ok"
                } else {
                    "present (not the manifest's bytes)"
                },
                if store.options().override_dir.is_some() {
                    format!("  {}", r.path.display())
                } else {
                    String::new()
                }
            ),
            Err(e) => {
                println!("  {id:<34} FAILED: {e}");
                failed.push(id.to_string());
            }
        }
    }
    if !recognizer_rows.is_empty() {
        // The compiled libtorch packages depend on this machine's devices: the backend
        // pack (install-ocr) says which.
        let pipeline = bunko_engines::EnginePipeline::new(EngineConfig::new(
            store.options().root.clone(),
            Backend::Auto,
        ));
        match pipeline.torch() {
            Err(e) => {
                println!(
                    "  compiled packages: FAILED: the OCR backend is not installed ({e}); run 'mokuro-bunko install-ocr'"
                );
                failed.push("compiled packages (no OCR backend)".into());
            }
            Ok(_) => {
                for (e, r) in pipeline.prefetch_rows(&recognizer_rows) {
                    match r {
                        Ok(p) => {
                            println!(
                                "  {e:<14} {} on {} ({}): {} [{}]",
                                p.need.precision,
                                p.need.device,
                                p.need.label,
                                p.package.display(),
                                p.target
                            );
                            if let Some(why) = &p.need.fallback {
                                println!("  {e:<14} note: running on the CPU: {why}");
                            }
                        }
                        Err(err) => {
                            println!("  {e:<14} FAILED: {err}");
                            failed.push(format!("{e} compiled package"));
                        }
                    }
                }
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Fail::msg(format!(
            "{} item(s) could not be fetched: {}",
            failed.len(),
            failed.join(", ")
        )))
    }
}

fn verify(store: &ModelStore) -> CmdResult {
    let (mut ok, mut bad, mut absent) = (0, 0, 0);
    for file in &store.manifest().files {
        match store.verify(&file.id) {
            Ok(Some((path, true))) => {
                ok += 1;
                println!("  OK        {:<34} {}", file.id, path.display());
            }
            Ok(Some((path, false))) => {
                bad += 1;
                println!("  MISMATCH  {:<34} {}", file.id, path.display());
            }
            Ok(None) => absent += 1,
            Err(e) => {
                bad += 1;
                println!("  ERROR     {:<34} {e}", file.id);
            }
        }
    }
    println!("{ok} verified, {bad} bad, {absent} not downloaded");
    if bad > 0 {
        Err(Fail::msg(
            "some model files do not match the manifest; delete them and run 'mokuro-bunko models download'",
        ))
    } else {
        Ok(())
    }
}
