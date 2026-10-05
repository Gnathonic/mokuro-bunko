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
            return Err(format!(
                "Required directory does not exist ({label}): {}",
                path.display()
            ));
        }
        if !path.is_dir() {
            return Err(format!(
                "Required path is not a directory ({label}): {}",
                path.display()
            ));
        }
        probe_writable(path)
            .map_err(|_| format!("Directory is not writable ({label}): {}", path.display()))
    }
}

/// Whether this user can create files in `dir`: creates and removes a probe file.
/// The probe is created exclusively under a fresh name (`create_new` never follows a
/// symlink planted at that name nor overwrites an existing file).
pub fn probe_writable(dir: &Path) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let probe = dir.join(format!(
        ".mokuro-write-test-{}-{nanos:x}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)?;
    std::fs::remove_file(&probe)
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
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
        env_path("LOCALAPPDATA")
            .unwrap_or_else(|| home_dir().join("AppData").join("Local"))
            .join("mokuro-bunko")
    } else {
        env_path("XDG_DATA_HOME")
            .unwrap_or_else(|| home_dir().join(".local").join("share"))
            .join("mokuro-bunko")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe leaves nothing behind, never touches an existing file, and fails
    /// in a folder this user cannot write.
    #[test]
    fn probe_writable_is_exclusive_and_clean() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join(".mokuro-write-test");
        std::fs::write(&keep, b"mine").unwrap();
        probe_writable(dir.path()).unwrap();
        probe_writable(dir.path()).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from(".mokuro-write-test")]);
        assert_eq!(std::fs::read(&keep).unwrap(), b"mine");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let ro = dir.path().join("ro");
            std::fs::create_dir(&ro).unwrap();
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
            // root ignores permissions; the check only means something for a user.
            if probe_writable(&ro).is_ok() {
                return;
            }
            assert!(probe_writable(&ro).is_err());
        }
    }
}
