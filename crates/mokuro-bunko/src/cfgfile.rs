//! Which config file the CLI uses, and the two ways it is read.
//!
//! * [`load_effective`]: file + `MOKURO_*` environment overrides — what the server would
//!   run with. Used by every command that only *reads* settings.
//! * [`load_for_write`]: the file alone. Used by every command that rewrites the file
//!   (`config set`, `cors-add/-remove`, `ssl enable/disable`, `dyndns setup/enable/
//!   disable`, the tunnel's CORS prompt). Fix over 0.5.2, which saved the env-overridden
//!   object and so baked e.g. Docker's `MOKURO_STORAGE=/data` or nginx-accel's backend
//!   `MOKURO_PORT=8081` into `config.yaml` on the next save (spec config-cli-ops §1.5, Q2).

use bunko_core::config::{self, Config, ConfigError};
use std::path::{Path, PathBuf};

/// `-c/--config`, else `MOKURO_CONFIG` (both handled by clap), else the default path.
pub fn resolve(cli: Option<&Path>) -> PathBuf {
    cli.filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(bunko_core::storage::default_config_path)
}

pub fn load_effective(path: &Path) -> Result<Config, ConfigError> {
    let config = load_effective_quiet(path)?;
    print_warnings(&config);
    Ok(config)
}

/// [`load_effective`] without printing the loader's warnings: for `serve`, which logs
/// them (console and log file) once logging is up.
pub fn load_effective_quiet(path: &Path) -> Result<Config, ConfigError> {
    config::load_config(Some(path))
}

/// The file's own settings (defaults when it does not exist), without env overrides.
pub fn load_for_write(path: &Path) -> Result<Config, ConfigError> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value: serde_json::Value = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        parse_yaml(&text).map_err(|message| ConfigError::Yaml {
            path: path.to_path_buf(),
            message,
        })?
    };
    Config::from_value(&value)
}

/// Same YAML dialect as bunko-core's `load_config` (serde_yaml_ng into a JSON value).
fn parse_yaml(text: &str) -> Result<serde_json::Value, String> {
    serde_yaml_ng::from_str(text).map_err(|e| e.to_string())
}

pub fn save(config: &Config, path: &Path) -> Result<(), ConfigError> {
    config::save_config(config, Some(path))
}

/// Notes about settings the loader migrated (e.g. a 0.5 `mokuro` generation row).
fn print_warnings(config: &Config) {
    for w in &config.warnings {
        eprintln!("Warning: {w}");
    }
}
