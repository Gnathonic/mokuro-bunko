//! On-disk layout under `storage.base_path`, identical to 0.5.2.

use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageLayout {
    pub base: PathBuf,
    /// The library lives in a cloud folder (`storage.library.backend` is not `local`):
    /// its root is a synthetic mount point that is never created on disk.
    cloud_library: bool,
    /// A cloud library's synthetic root, `<base>/cloud/<store-id>/library` (spec D2),
    /// once the server knows which store it opened.
    library_root: Option<PathBuf>,
}

/// The directory name under `cloud/` of a cloud library whose store is not known yet
/// (not connected, or its credentials are missing): nothing is created on disk for it.
pub const UNCONNECTED_STORE: &str = "unconnected";

impl StorageLayout {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            cloud_library: false,
            library_root: None,
        }
    }
    /// The same layout for a library in a cloud folder (`cloud`) or on this disk.
    pub fn with_cloud_library(mut self, cloud: bool) -> Self {
        self.cloud_library = cloud;
        self
    }
    /// A cloud library's synthetic root (the store's mount point). Ignored for a local
    /// library.
    pub fn with_library_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.library_root = Some(root.into());
        self
    }
    pub fn cloud_library(&self) -> bool {
        self.cloud_library
    }
    /// The shared manga library (served at the WebDAV root's `mokuro-reader/`).
    ///
    /// For a cloud library this is the store's synthetic root
    /// `<base>/cloud/<store-id>/library` ([`with_library_root`](Self::with_library_root)),
    /// or `<base>/cloud/unconnected/library` before a store is known. It never exists on
    /// disk: every access goes through the mounted store (spec D2).
    pub fn library(&self) -> PathBuf {
        if !self.cloud_library {
            return self.base.join("library");
        }
        match &self.library_root {
            Some(root) => root.clone(),
            None => self.cloud_dir().join(UNCONNECTED_STORE).join("library"),
        }
    }
    /// A cloud library's own local directory `<base>/cloud/<store-id>/` (index, state,
    /// caches, spool); `None` for a local library.
    pub fn cloud_store_dir(&self) -> Option<PathBuf> {
        if !self.cloud_library {
            return None;
        }
        self.library().parent().map(Path::to_path_buf)
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
    /// A cloud library's local state: index, state, caches, spool, credentials (new in 0.8).
    pub fn cloud_dir(&self) -> PathBuf {
        self.base.join("cloud")
    }

    /// The directories the server needs. A cloud library gets no `library/` or
    /// `library/thumbnails/` on disk.
    pub fn ensure_directories(&self) -> io::Result<()> {
        if self.cloud_library {
            std::fs::create_dir_all(&self.base)?;
            std::fs::create_dir_all(self.inbox())?;
            std::fs::create_dir_all(self.users())?;
            return Ok(());
        }
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

/// The Windows portable zip: `PORTABLE.txt` next to `exe_dir`'s program means its
/// defaults live in `<exe_dir>\data`.
pub fn portable_data_in(exe_dir: &Path) -> Option<PathBuf> {
    exe_dir
        .join("PORTABLE.txt")
        .is_file()
        .then(|| exe_dir.join("data"))
}

/// The `data\` folder of a Windows portable copy (`PORTABLE.txt` next to the running
/// program), the same folder `_env.cmd` points `run.bat` and `doctor.bat` at, so a
/// command typed in that folder (`mokuro-bunko.exe install-ocr`, as its README.txt
/// says) uses the portable data too. Never on other platforms.
pub fn portable_data_dir() -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let exe = std::env::current_exe().ok()?;
    portable_data_in(exe.parent()?)
}

/// `~/.local/share/mokuro-bunko` (XDG_DATA_HOME), or `%LOCALAPPDATA%\mokuro-bunko`
/// (a portable copy's `data\` folder: [`portable_data_dir`]).
pub fn default_storage_path() -> PathBuf {
    if let Some(data) = portable_data_dir() {
        return data;
    }
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

/// `~/.config/mokuro-bunko/config.yaml` (XDG_CONFIG_HOME), or `%LOCALAPPDATA%\mokuro-bunko\config.yaml`
/// (a portable copy's `data\config.yaml`: [`portable_data_dir`]).
pub fn default_config_path() -> PathBuf {
    if let Some(data) = portable_data_dir() {
        return data.join("config.yaml");
    }
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

    /// `PORTABLE.txt` next to the program puts the defaults in its `data\` folder.
    #[test]
    fn portable_data_needs_the_marker_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(portable_data_in(dir.path()), None);
        std::fs::create_dir(dir.path().join("PORTABLE.txt")).unwrap();
        assert_eq!(
            portable_data_in(dir.path()),
            None,
            "a folder is not the marker"
        );
        std::fs::remove_dir(dir.path().join("PORTABLE.txt")).unwrap();
        std::fs::write(dir.path().join("PORTABLE.txt"), "").unwrap();
        assert_eq!(portable_data_in(dir.path()), Some(dir.path().join("data")));
        // The test binary itself is no portable copy.
        assert_eq!(portable_data_dir(), None);
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// A local library gets `library/` and `library/thumbnails/` as before.
    #[test]
    fn local_layout_creates_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path().join("data"));
        assert!(!layout.cloud_library());
        layout.ensure_directories().unwrap();
        assert_eq!(names(&layout.base), ["inbox", "library", "users"]);
        assert_eq!(names(&layout.library()), ["thumbnails"]);
    }

    /// A cloud library never gets `library/` on disk; the rest is still created.
    #[test]
    fn cloud_layout_skips_the_library() {
        let dir = tempfile::tempdir().unwrap();
        let layout = StorageLayout::new(dir.path().join("data")).with_cloud_library(true);
        assert!(layout.cloud_library());
        layout.ensure_directories().unwrap();
        assert_eq!(names(&layout.base), ["inbox", "users"]);
        assert_eq!(layout.cloud_dir(), layout.base.join("cloud"));
        // The library root is a synthetic mount point under the store's directory.
        assert_eq!(
            layout.library(),
            layout.base.join("cloud/unconnected/library")
        );
        let root = layout.base.join("cloud/0123456789abcdef/library");
        let layout = layout.with_library_root(root);
        assert_eq!(
            layout.library(),
            layout.base.join("cloud/0123456789abcdef/library")
        );
        assert_eq!(
            layout.cloud_store_dir(),
            Some(layout.base.join("cloud/0123456789abcdef"))
        );
        assert_eq!(
            layout.thumbnails(),
            layout
                .base
                .join("cloud/0123456789abcdef/library/thumbnails")
        );
        layout.ensure_directories().unwrap();
        assert_eq!(names(&layout.base), ["inbox", "users"]);
        // A local layout ignores the root.
        let local = StorageLayout::new(dir.path().join("data")).with_library_root("/elsewhere");
        assert_eq!(local.library(), local.base.join("library"));
        assert_eq!(local.cloud_store_dir(), None);
    }

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
