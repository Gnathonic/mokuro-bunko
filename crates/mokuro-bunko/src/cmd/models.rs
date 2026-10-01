//! `models list|download|verify` (full build): the ONNX model store under
//! `<storage>/models/` (ARCHITECTURE §7: a signed `models.json` lists url, size, sha256).
//!
//! TODO(orchestrator): bunko-ocr has no model-store API yet (no `crates/bunko-ocr/src/
//! models*` when this was written). Wire the three `store_*` functions below to it and
//! add `bunko-ocr` to this crate's `ocr` feature. Expected shapes:
//! * `list(models_dir) -> Vec<{id, engine, size, present: bool}>`
//! * `download(models_dir, engine: Option<&str>, progress)` — downloads + sha256-verifies
//! * `verify(models_dir) -> Vec<{id, ok: bool, detail}>`

use super::Ctx;
use crate::cfgfile;
use crate::cli::ModelsCmd;
use crate::out::{CmdResult, Fail};
use std::path::{Path, PathBuf};

const ENGINES: [&str; 3] = ["hayai-nova", "paddle-manga", "ppocr-manga"];

pub fn run(ctx: &Ctx, cmd: ModelsCmd) -> CmdResult {
    crate::logging::init_console(ctx.verbose);
    match cmd {
        ModelsCmd::List => {
            let dir = models_dir(ctx)?;
            println!("Models directory: {}", dir.display());
            store_list(&dir)
        }
        ModelsCmd::Download { engine } => download(ctx, engine.as_deref()),
        ModelsCmd::Verify => store_verify(&models_dir(ctx)?),
    }
}

fn models_dir(ctx: &Ctx) -> Result<PathBuf, Fail> {
    Ok(cfgfile::load_effective(&ctx.config_path)?
        .storage
        .layout()
        .models())
}

/// Also used by the deprecated `install-ocr`.
pub fn download(ctx: &Ctx, engine: Option<&str>) -> CmdResult {
    if let Some(e) = engine.filter(|e| !ENGINES.contains(e)) {
        return Err(Fail::msg(format!(
            "Unknown engine '{e}' (expected one of: {})",
            ENGINES.join(", ")
        )));
    }
    let dir = models_dir(ctx)?;
    store_download(&dir, engine)
}

fn store_list(_dir: &Path) -> CmdResult {
    Err(Fail::msg("models list is not wired into this build yet"))
}

fn store_download(_dir: &Path, _engine: Option<&str>) -> CmdResult {
    Err(Fail::msg(
        "models download is not wired into this build yet",
    ))
}

fn store_verify(_dir: &Path) -> CmdResult {
    Err(Fail::msg("models verify is not wired into this build yet"))
}
