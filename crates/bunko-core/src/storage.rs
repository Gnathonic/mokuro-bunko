//! On-disk layout under `storage.base_path`, identical to 0.5.2.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageLayout {
    pub base: PathBuf,
}

impl StorageLayout {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }
    /// The shared manga library (served at the WebDAV root's `mokuro-reader/`).
    pub fn library(&self) -> PathBuf {
        self.base.join("library")
    }
    pub fn inbox(&self) -> PathBuf {
        self.base.join("inbox")
    }
    pub fn users(&self) -> PathBuf {
        self.base.join("users")
    }
    pub fn thumbnails(&self) -> PathBuf {
        self.library().join("thumbnails")
    }
    pub fn database(&self) -> PathBuf {
        self.base.join("mokuro.db")
    }
    pub fn logs(&self) -> PathBuf {
        self.base.join("logs")
    }
    /// Downloaded ONNX models and runtimes (new in 0.7).
    pub fn models(&self) -> PathBuf {
        self.base.join("models")
    }

    pub fn ensure_directories(&self) -> io::Result<()> {
        std::fs::create_dir_all(self.library())?;
        std::fs::create_dir_all(self.inbox())?;
        std::fs::create_dir_all(self.users())?;
        std::fs::create_dir_all(self.thumbnails())?;
        Ok(())
    }

    /// Fail unless `path` is an existing directory we can create files in.
    pub fn assert_writable_dir(path: &Path, label: &str) -> Result<(), String> {
        if !path.exists() {
            return Err(format!("Required directory does not exist ({label}): {}", path.display()));
        }
        if !path.is_dir() {
            return Err(format!("Required path is not a directory ({label}): {}", path.display()));
        }
        let probe = path.join(".mokuro-write-test");
        std::fs::write(&probe, b"ok")
            .and_then(|_| std::fs::remove_file(&probe))
            .map_err(|_| format!("Directory is not writable ({label}): {}", path.display()))
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        env_path("USERPROFILE").unwrap_or_else(|| PathBuf::from("."))
    }
    #[cfg(not(windows))]
    {
        env_path("HOME").unwrap_or_else(|| PathBuf::from("."))
    }
}

/// `~/.local/share/mokuro-bunko` (XDG_DATA_HOME), or `%LOCALAPPDATA%\mokuro-bunko`.
pub fn default_storage_path() -> PathBuf {
    if cfg!(windows) {
        env_path("LOCALAPPDATA").unwrap_or_else(|| home_dir().join("AppData").join("Local")).join("mokuro-bunko")
    } else {
        env_path("XDG_DATA_HOME").unwrap_or_else(|| home_dir().join(".local").join("share")).join("mokuro-bunko")
    }
}

/// `~/.config/mokuro-bunko/config.yaml` (XDG_CONFIG_HOME), or `%LOCALAPPDATA%\mokuro-bunko\config.yaml`.
pub fn default_config_path() -> PathBuf {
    let base = if cfg!(windows) {
        env_path("LOCALAPPDATA").unwrap_or_else(|| home_dir().join("AppData").join("Local"))
    } else {
        env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home_dir().join(".config"))
    };
    base.join("mokuro-bunko").join("config.yaml")
}

/// Python's `Path.expanduser()` for a leading `~` / `~/`.
pub fn expand_user(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return home_dir();
    }
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        return home_dir().join(rest);
    }
    path.to_path_buf()
}
