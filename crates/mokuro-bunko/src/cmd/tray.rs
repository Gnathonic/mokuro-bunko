//! `tray`: the desktop tray (crate bunko-tray). `mokuro-bunko` with no arguments runs it
//! in a desktop session (`no_args.rs`); on Windows `mokuro-bunko.exe` is the build
//! for the GUI subsystem (no console window) and `mokuro-bunko-cli.exe` the console one.

use super::Ctx;
use crate::cli::TrayArgs;
use crate::out::{CmdResult, Fail};

pub fn run(_ctx: &Ctx, args: TrayArgs) -> CmdResult {
    let exe = bunko_update::current_exe().map_err(|e| Fail::Error(e.to_string()))?;
    bunko_tray::run(
        bunko_tray::Options {
            storages: args.storages,
            supervise: !args.no_supervise,
            log_stderr: args.log_stderr,
        },
        exe,
    )
    .map_err(|e| Fail::Error(format!("{e:#}")))
}
