//! `tray.json`: what the tray itself runs. Written by the setup wizard ("start with the
//! machine → with the tray") or `install.ps1`, read at tray start.
//!
//! ```json
//! { "managed": [ { "role": "processor",
//!                  "args": ["processor", "serve", "--config", "/home/a/.config/mokuro-bunko/processor.yaml"] },
//!                { "role": "server" } ],
//!   "notifications": false }
//! ```
//!
//! `args` defaults by role: `serve` for `server`, `processor serve --config
//! <default processor.yaml>` for `processor`. With no `managed` entries the tray only
//! shows and controls instances that something else (a service, a terminal) started.

use crate::paths::{self, Env};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct TrayConfig {
    pub managed: Vec<Managed>,
    /// Desktop notifications (library unreachable, OCR failure, update available).
    pub notifications: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Managed {
    /// `server` or `processor`.
    pub role: String,
    /// Arguments after the executable; empty = the role's default.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TrayConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{path}: unknown role {role:?} (expected server or processor)")]
    Role { path: PathBuf, role: String },
}

impl TrayConfig {
    /// `Ok(None)` when the file does not exist (nothing configured yet).
    pub fn load(path: &Path) -> Result<Option<TrayConfig>, TrayConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(TrayConfigError::Read {
                    path: path.into(),
                    source,
                });
            }
        };
        let conf: TrayConfig =
            serde_json::from_str(&text).map_err(|source| TrayConfigError::Parse {
                path: path.into(),
                source,
            })?;
        if let Some(m) = conf
            .managed
            .iter()
            .find(|m| !matches!(m.role.as_str(), "server" | "processor"))
        {
            return Err(TrayConfigError::Role {
                path: path.into(),
                role: m.role.clone(),
            });
        }
        Ok(Some(conf))
    }
}

impl Managed {
    /// The command-line arguments to start this instance with.
    pub fn command_args(&self, env: &dyn Env) -> Vec<String> {
        if !self.args.is_empty() {
            return self.args.clone();
        }
        match self.role.as_str() {
            "processor" => vec![
                "processor".into(),
                "serve".into(),
                "--config".into(),
                paths::processor_default_config(env)
                    .to_string_lossy()
                    .into_owned(),
            ],
            _ => vec!["serve".into()],
        }
    }

    /// The `--config` this entry passes, if any (where its storage is configured).
    pub fn config_arg(&self, env: &dyn Env) -> Option<PathBuf> {
        let args = self.command_args(env);
        args.iter()
            .position(|a| a == "--config")
            .and_then(|i| args.get(i + 1))
            .map(PathBuf::from)
    }

    pub fn label(&self) -> &'static str {
        role_label(&self.role)
    }
}

pub fn role_label(role: &str) -> &'static str {
    match role {
        "processor" => "Processor",
        "server" => "Library",
        "gui" => "Setup",
        _ => "mokuro-bunko",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::test_env::FakeEnv;

    #[test]
    fn load_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tray.json");
        assert!(TrayConfig::load(&path).unwrap().is_none());
        std::fs::write(
            &path,
            r#"{"managed":[{"role":"processor"},{"role":"server","args":["serve","--port","9000"]}]}"#,
        )
        .unwrap();
        let conf = TrayConfig::load(&path).unwrap().unwrap();
        assert!(!conf.notifications);
        let env = FakeEnv::default()
            .with("HOME", "/home/a")
            .with("LOCALAPPDATA", r"C:\L");
        let p = conf.managed[0].command_args(&env);
        assert_eq!(&p[..3], ["processor", "serve", "--config"]);
        assert!(p[3].ends_with("processor.yaml"));
        assert_eq!(
            conf.managed[0].config_arg(&env),
            Some(paths::processor_default_config(&env))
        );
        assert_eq!(
            conf.managed[1].command_args(&env),
            ["serve", "--port", "9000"]
        );
        assert_eq!(conf.managed[1].config_arg(&env), None);
        std::fs::write(&path, r#"{"managed":[{"role":"gpu"}]}"#).unwrap();
        assert!(TrayConfig::load(&path).is_err());
        std::fs::write(&path, "{").unwrap();
        assert!(TrayConfig::load(&path).is_err());
    }
}
