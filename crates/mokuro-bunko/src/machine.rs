//! What this machine is set up as: a library server (a `config.yaml`, or a
//! `MOKURO_STORAGE` deployment), a processor (a `processor.yaml`), or both.
//! Used by the OCR tools (`install-ocr`, `models`, `doctor`) to pick whose storage they
//! work on, and by the desktop app's pages.

use std::path::PathBuf;

/// The `processor.yaml` the desktop app writes by default: next to the library's
/// default `config.yaml` (`~/.config/mokuro-bunko/processor.yaml`,
/// `%LOCALAPPDATA%\mokuro-bunko\processor.yaml`).
pub fn default_processor_config() -> PathBuf {
    let config = bunko_core::storage::default_config_path();
    config
        .parent()
        .map(|d| d.join("processor.yaml"))
        .unwrap_or_else(|| PathBuf::from("processor.yaml"))
}

/// This machine's `processor.yaml`, if it has one: `MOKURO_PROCESSOR_CONFIG`, else the
/// desktop app's default location, else `./processor.yaml` (`processor setup`'s
/// default) — the first that exists.
pub fn find_processor_config() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MOKURO_PROCESSOR_CONFIG").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    [default_processor_config(), PathBuf::from("processor.yaml")]
        .into_iter()
        .find(|p| p.is_file())
        .map(|p| std::path::absolute(&p).unwrap_or(p))
}

/// A library server is configured here: its config file exists, or the deployment
/// names its storage through the environment (Docker's `MOKURO_STORAGE`). Decides
/// which role `install-ocr` / `doctor` / `models` serve (full build).
pub fn library_configured(config_path: &std::path::Path) -> bool {
    config_path.is_file()
        || std::env::var_os("MOKURO_STORAGE").is_some_and(|v| !v.is_empty())
        || std::env::var_os("MOKURO_STORAGE_BASE_PATH").is_some_and(|v| !v.is_empty())
}
