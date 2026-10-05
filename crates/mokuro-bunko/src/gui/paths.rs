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

/// Where `gui` keeps its `.control.json` when neither storage can take it (taken by a
/// running instance, or not writable — e.g. a `~/.local` owned by root), in order of
/// preference: directories only this user can reach, never the shared temp
/// directory (where another user could plant a control file for the tray to trust).
/// `$XDG_RUNTIME_DIR/mokuro-bunko-gui` on Linux when set, `<local data dir>/
/// mokuro-bunko/gui`, on macOS `~/Library/Application Support/mokuro-bunko/gui`, then
/// `<config dir>/mokuro-bunko/gui`. The tray looks in the same places
/// (`bunko_tray::paths::gui_fallback_storages`).
pub fn gui_fallback_storages() -> Vec<PathBuf> {
    gui_fallback_storages_from(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        bunko_core::storage::default_storage_path(),
        bunko_core::storage::home_dir(),
        bunko_core::storage::default_config_path(),
    )
}

fn gui_fallback_storages_from(
    runtime_dir: Option<PathBuf>,
    default_storage: PathBuf,
    home: PathBuf,
    default_config: PathBuf,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(run) = runtime_dir
        && cfg!(target_os = "linux")
        && run.is_absolute()
    {
        out.push(run.join("mokuro-bunko-gui"));
    }
    out.push(default_storage.join("gui"));
    if cfg!(target_os = "macos") {
        out.push(home.join("Library/Application Support/mokuro-bunko/gui"));
    }
    if let Some(dir) = default_config.parent() {
        out.push(dir.join("gui"));
    }
    out.dedup();
    out
}

/// Whether this user can create files in `dir`, creating it (and its parents) first.
pub fn ensure_writable(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".mokuro-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"ok")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Whether `dir` could be used as a storage, without creating anything: it is
/// writable, or its nearest existing parent is. `Err` names the folder in the way.
pub fn storage_blocker(dir: &Path) -> Result<(), PathBuf> {
    let mut p = dir;
    loop {
        if p.exists() {
            return if dir_writable(p) {
                Ok(())
            } else {
                Err(p.to_path_buf())
            };
        }
        match p.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => p = parent,
            _ => return Ok(()),
        }
    }
}

fn dir_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".mokuro-write-test-{}", std::process::id()));
    match std::fs::write(&probe, b"ok") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// `~/x` for paths under the home folder (what the pages show).
pub fn tilde(path: &Path) -> String {
    let home = bunko_core::storage::home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.display()),
        Ok(_) => "~".into(),
        Err(_) => path.display().to_string(),
    }
}

/// Who owns `dir`, when it is not this user ("owned by root").
fn owner_note(dir: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let uid = std::fs::metadata(dir).ok()?.uid();
        // SAFETY: getuid cannot fail.
        let me = unsafe { libc::getuid() };
        if uid == me {
            return Some("its permissions do not allow writing".into());
        }
        if uid == 0 {
            return Some("owned by root".into());
        }
        Some(format!("owned by user id {uid}"))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        None
    }
}

/// A storage folder to propose when the default one cannot be used:
/// `~/Library/Application Support/<name>` on macOS, `~/<name>` elsewhere.
pub fn alternative_storage(default: &Path) -> Option<PathBuf> {
    let name = default.file_name()?;
    let home = bunko_core::storage::home_dir();
    let candidates = if cfg!(target_os = "macos") {
        vec![
            home.join("Library/Application Support").join(name),
            home.join(name),
        ]
    } else {
        vec![home.join(name)]
    };
    candidates
        .into_iter()
        .find(|c| c != default && storage_blocker(c).is_ok())
}

/// What the wizard says about a storage folder: whether it can be used, why not, a
/// writable alternative and the one-line fix.
pub fn storage_check(dir: &Path) -> serde_json::Value {
    match storage_blocker(dir) {
        Ok(()) => serde_json::json!({"path": dir, "writable": true}),
        Err(blocker) if !blocker.is_dir() => serde_json::json!({
            "path": dir,
            "writable": false,
            "blocker": blocker,
            "problem": format!("{} is a file, not a folder", tilde(&blocker)),
            "suggestion": alternative_storage(dir),
            "fix": null,
        }),
        Err(blocker) => {
            let why = owner_note(&blocker)
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            let home = bunko_core::storage::home_dir();
            // `sudo chown -R "$USER" ~/.local`: the top folder under home that is in the way.
            let fix = blocker.strip_prefix(&home).ok().and_then(|rest| {
                let top = rest.components().next()?;
                (cfg!(unix)).then(|| {
                    format!(
                        "sudo chown -R \"$USER\" {}",
                        tilde(&home.join(top.as_os_str()))
                    )
                })
            });
            serde_json::json!({
                "path": dir,
                "writable": false,
                "blocker": blocker,
                "problem": format!("{} isn't writable by you{why}", tilde(&blocker)),
                "suggestion": alternative_storage(dir),
                "fix": fix,
            })
        }
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

    /// The `gui` fallbacks are private to the user, never under the shared temp dir.
    #[test]
    fn gui_fallbacks_are_not_in_temp() {
        let tmp = std::env::temp_dir();
        for got in gui_fallback_storages() {
            assert!(!got.starts_with(&tmp), "{}", got.display());
        }
        let data = PathBuf::from("/home/a/.local/share/mokuro-bunko");
        let home = PathBuf::from("/home/a");
        let cfg = PathBuf::from("/home/a/.config/mokuro-bunko/config.yaml");
        let got = gui_fallback_storages_from(None, data.clone(), home.clone(), cfg.clone());
        assert_eq!(got[0], data.join("gui"));
        assert_eq!(
            got.last(),
            Some(&PathBuf::from("/home/a/.config/mokuro-bunko/gui"))
        );
        if cfg!(target_os = "macos") {
            assert!(got.contains(&home.join("Library/Application Support/mokuro-bunko/gui")));
        }
        let run = gui_fallback_storages_from(
            Some("/run/user/1000".into()),
            data.clone(),
            home.clone(),
            cfg.clone(),
        );
        if cfg!(target_os = "linux") {
            assert_eq!(run[0], PathBuf::from("/run/user/1000/mokuro-bunko-gui"));
        } else {
            assert_eq!(run[0], data.join("gui"));
        }
        let rel = gui_fallback_storages_from(Some("relative".into()), data.clone(), home, cfg);
        assert_eq!(rel[0], data.join("gui"));

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

    /// A folder under a read-only parent is reported, with the parent named.
    #[cfg(unix)]
    #[test]
    fn unwritable_storage_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        // root ignores permissions: nothing to check then.
        if unsafe { libc::getuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join(".local");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o555)).unwrap();
        let storage = local.join("share/mokuro-bunko");
        assert_eq!(storage_blocker(&storage), Err(local.clone()));
        assert!(ensure_writable(&storage).is_err());
        let v = storage_check(&storage);
        assert_eq!(v["writable"], false);
        assert!(
            v["problem"]
                .as_str()
                .unwrap()
                .contains("isn't writable by you"),
            "{v}"
        );
        let ok = dir.path().join("elsewhere/lib");
        assert!(storage_blocker(&ok).is_ok());
        assert_eq!(storage_check(&ok)["writable"], true);
        ensure_writable(&ok).unwrap();
        std::fs::set_permissions(&local, std::fs::Permissions::from_mode(0o755)).unwrap();
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
