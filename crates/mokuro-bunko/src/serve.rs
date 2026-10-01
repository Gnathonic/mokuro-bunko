//! `serve`: assembles and runs the server.
//!
//! STUB — the orchestrator replaces this file with the app assembly. The CLI has already
//! loaded the config (file + `MOKURO_*` env) and applied the explicitly passed
//! `--host/--port/--ocr/--generations` flags to it; `config_path` is the resolved file
//! path (`-c`, `MOKURO_CONFIG`, or the default) that the admin API saves to.
//! `args.verbose` carries the global `-v`. An `Err` prints `Error: <e>` and exits 1;
//! for 0.5.2's startup-validation exit code 2, print `Startup validation failed: <msg>`
//! and `std::process::exit(2)` from here.

use crate::cli::ServeArgs;
use bunko_core::Config;
use std::path::PathBuf;

pub fn run(args: ServeArgs, config: Config, config_path: PathBuf) -> anyhow::Result<()> {
    let _ = (args, config, config_path);
    anyhow::bail!("not wired yet")
}
