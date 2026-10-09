//! `tray`: the desktop tray (crate bunko-tray), in this program since 0.7.0-beta.3.
//! On macOS opening the app runs it, on Windows `Mokuro Bunko.exe` (the same program
//! built for the GUI subsystem: no console window) does (`main.rs`).

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
        // A first start after an update from a release with the separate tray program
        // finishes moving its login items and files over (crate::migrate).
        || {
            let _ = crate::migrate::run(crate::migrate::Caller::Tray);
        },
    )
    .map_err(|e| Fail::Error(format!("{e:#}")))
}
