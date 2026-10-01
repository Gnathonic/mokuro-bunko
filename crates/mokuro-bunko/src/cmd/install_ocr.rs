//! `install-ocr`: deprecated alias kept for scripts (the Unraid entrypoint and the old
//! Windows setup script call `install-ocr --backend X`). OCR is built into the full
//! build, so there is nothing to install; on a full build this fetches the models
//! (`models download`). Always exits 0, as those scripts expect.

use super::Ctx;
use crate::cli::InstallOcrArgs;
use crate::out::CmdResult;

pub fn run(ctx: &Ctx, args: InstallOcrArgs) -> CmdResult {
    let _ = (
        &args.force,
        &args.backend,
        &args.list_backends,
        &args.engines,
        &args.detector,
    );
    println!(
        "install-ocr is deprecated: OCR is built into mokuro-bunko and needs no Python environment."
    );
    #[cfg(feature = "ocr")]
    {
        println!("Downloading the OCR models instead (mokuro-bunko models download)...");
        if let Err(e) = super::models::download(ctx, None) {
            let msg = match e {
                crate::out::Fail::Error(m) => m,
                crate::out::Fail::Exit(code) => format!("exit code {code}"),
            };
            eprintln!("Warning: model download failed: {msg}");
            eprintln!(
                "The server downloads models on first use; run 'mokuro-bunko models download' to retry."
            );
        }
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = ctx;
        println!(
            "This is the lite build: OCR runs on remote processors ('mokuro-bunko processor serve' on a full build)."
        );
    }
    Ok(())
}
