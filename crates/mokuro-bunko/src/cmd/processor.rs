//! `processor serve|setup|status|service` (full build): run this machine as a remote OCR
//! processor for a library (spec remote-processors.md; 0.5.2 `processor/cli.py`).
//!
//! TODO(orchestrator): bunko-processor exposes no runtime entry points yet (only
//! `config::load_processor_config`). Wire each arm below:
//! * `Serve`  -> init `crate::logging::init_server(<processor storage>, verbose)`, then the
//!   processor's run loop (SIGINT/SIGTERM = clean stop, reconnect with backoff).
//! * `Setup`  -> the setup wizard (check the account, write processor.yaml, install the
//!   service unless `--no-service`); `--no-install` is accepted and ignored (no venvs).
//! * `Status` -> describe the last status file under the processor storage.
//! * `Service`-> print (or with `--install`, install) the systemd user unit / Windows
//!   Startup entry running `<this exe> processor serve --config <abs path>`.

use super::Ctx;
use crate::cli::ProcessorCmd;
use crate::out::{CmdResult, Fail};

pub fn run(ctx: &Ctx, cmd: ProcessorCmd) -> CmdResult {
    let _ = ctx;
    let name = match &cmd {
        ProcessorCmd::Serve { .. } => "serve",
        ProcessorCmd::Setup(_) => "setup",
        ProcessorCmd::Status { .. } => "status",
        ProcessorCmd::Service { .. } => "service",
    };
    Err(Fail::msg(format!(
        "processor {name} is not wired into this build yet"
    )))
}
