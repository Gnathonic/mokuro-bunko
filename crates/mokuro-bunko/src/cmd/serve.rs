//! `serve`: load the config, apply the flags that were passed, hand over to `crate::serve`.

use super::Ctx;
use crate::cfgfile;
use crate::cli::ServeArgs;
use crate::out::{CmdResult, Fail};

pub fn run(ctx: &Ctx, mut args: ServeArgs) -> CmdResult {
    let mut config = cfgfile::load_effective(&ctx.config_path)?;
    // Fix over 0.5.2 (spec §4.1, Q7): a flag that was passed always wins, even when it
    // equals the built-in default; one that was not passed never touches the config.
    let overrides = [
        ("server.host", args.host.clone()),
        ("server.port", args.port.map(|p| p.to_string())),
        ("ocr.backend", args.ocr.clone()),
        ("ocr.generations", args.generations.clone()),
    ];
    for (key, value) in overrides {
        if let Some(v) = value {
            config.set_by_dotted_key(key, &v).map_err(Fail::from)?;
        }
    }
    args.verbose = ctx.verbose;
    if ctx.verbose {
        println!("Verbose mode enabled");
        println!("Storage path: {}", config.storage.base_path.display());
    }
    crate::serve::run(args, config, ctx.config_path.clone()).map_err(Fail::from)
}
