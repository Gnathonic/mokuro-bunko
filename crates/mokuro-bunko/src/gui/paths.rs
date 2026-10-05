//! Where this machine's files are, for the pages: the processor's config, the storage
//! of each role, their logs, and the private directory `gui` falls back to.

use std::path::{Path, PathBuf};

/// The `processor.yaml` the pages read and write: the one this machine has
/// ([`crate::machine::find_processor_config`]), else the default location.
pub fn processor_config_path() -> PathBuf {
    crate::machine::find_processor_config().unwrap_or_else(crate::machine::default_processor_config)
}

/// The library server's storage (`storage.base_path`, env overrides applied).
pub fn server_storage(config_path: &Path) -> PathBuf {
    bunko_core::config::load_config(Some(config_path))
        .map(|c| c.storage.base_path)
        .unwrap_or_else(|_| bunko_core::storage::default_storage_path())
}

/// The processor's storage (`processor.storage`; the default without a config).
pub fn processor_storage(processor_config: &Path) -> PathBuf {
    #[cfg(feature = "ocr")]
    {
        if processor_config.is_file()
            && let Ok(c) = bunko_processor::load_processor_config(processor_config)
        {
            return c.processor.storage;
        }
        bunko_processor::config::default_storage_path()
    }
    #[cfg(not(feature = "ocr"))]
    {
        let _ = processor_config;
        bunko_core::storage::default_storage_path().with_file_name("mokuro-bunko-processor")
    }
}

/// Where `gui` keeps its `.control.json` when both storages are taken by running
/// instances: a directory only this user can reach (never the shared temp directory,
/// where another user could plant a control file for the tray to trust).
/// `$XDG_RUNTIME_DIR/mokuro-bunko-gui` on Linux when set, else
/// `<local data dir>/mokuro-bunko/gui`. The tray looks in the same place.
pub fn gui_fallback_storage() -> PathBuf {
    gui_fallback_storage_from(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        bunko_core::storage::default_storage_path(),
    )
}

/// [`gui_fallback_storage`] from `XDG_RUNTIME_DIR` and the default storage
/// (`<local data dir>/mokuro-bunko`).
fn gui_fallback_storage_from(runtime_dir: Option<PathBuf>, default_storage: PathBuf) -> PathBuf {
    match runtime_dir {
        Some(run) if cfg!(target_os = "linux") && run.is_absolute() => run.join("mokuro-bunko-gui"),
        _ => default_storage.join("gui"),
    }
}

/// Create `dir` readable by this user only (0700 on Unix; on Windows the per-user
/// local data directory is already private).
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// A file name a listing produced: one path component, no tricks.
pub fn plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && !name.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `gui` fallback is private to the user, never under the shared temp dir.
    #[test]
    fn gui_fallback_is_not_in_temp() {
        let tmp = std::env::temp_dir();
        let got = gui_fallback_storage();
        assert!(!got.starts_with(&tmp), "{}", got.display());
        let data = PathBuf::from("/home/a/.local/share/mokuro-bunko");
        assert_eq!(
            gui_fallback_storage_from(None, data.clone()),
            data.join("gui")
        );
        let run = gui_fallback_storage_from(Some("/run/user/1000".into()), data.clone());
        if cfg!(target_os = "linux") {
            assert_eq!(run, PathBuf::from("/run/user/1000/mokuro-bunko-gui"));
        } else {
            assert_eq!(run, data.join("gui"));
        }
        assert_eq!(
            gui_fallback_storage_from(Some("relative".into()), data.clone()),
            data.join("gui")
        );

        let dir = tempfile::tempdir().unwrap();
        let made = dir.path().join("a/gui");
        create_private_dir(&made).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&made).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn plain_names_only() {
        assert!(plain_file_name("mokuro-bunko.log"));
        assert!(plain_file_name("server.log.1"));
        for bad in ["", ".", "..", "../x", "a/b", "a\\b", "C:x", "a\0"] {
            assert!(!plain_file_name(bad), "{bad:?}");
        }
    }
}
