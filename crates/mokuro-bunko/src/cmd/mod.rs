//! One module per command group.

pub mod admin;
pub mod config;
pub mod doctor;
pub mod dyndns;
pub mod gui;
pub mod healthcheck;
pub mod install_ocr;
#[cfg(feature = "ocr")]
pub mod models;
#[cfg(feature = "ocr")]
pub mod processor;
pub mod serve;
pub mod setup;
pub mod ssl;
#[cfg(feature = "tray")]
pub mod tray;
pub mod tunnel;
pub mod update;

use std::path::PathBuf;

/// What every command gets from the global options.
pub struct Ctx {
    /// `-c` / `MOKURO_CONFIG` / the default path, resolved.
    pub config_path: PathBuf,
    pub verbose: bool,
    /// `-c` / `MOKURO_CONFIG` exactly as given (to pass on when re-executing).
    pub cli_config: Option<PathBuf>,
}
