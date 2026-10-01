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
        for file in store.manifest().engine_files(engine) {
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
    // fp16 graphs are only worth fetching where a GPU provider is compiled in.
    let gpu = bunko_ocr::runtime::ep_compiled().len() > 1;
    let ids = models::download_plan(engine, gpu);
    let total: u64 = ids
        .iter()
        .filter(|id| store.locate(id).is_none())
        .filter_map(|id| store.manifest().get(id))
        .map(|f| f.size)
        .sum();
    println!(
        "Fetching {} into {} ({} to download)",
        engine.unwrap_or("every engine"),
        store.options().root.display(),
        mb(total)
    );
    let mut failed = Vec::new();
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
                failed.push(id);
            }
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Fail::msg(format!(
            "{} model file(s) could not be fetched",
            failed.len()
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
