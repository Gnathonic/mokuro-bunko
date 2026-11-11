//! A library in a folder chosen by path (`storage.library.backend: folder`): the
//! checks around it. The folder is opened like any cloud folder
//! (`bunko_remote::folder::FolderBackend` behind a `RemoteStore`); what is particular
//! is only where it may be: never Bunko's own folder, nor a folder that holds it.
//!
//! The path is kept as written (`~` expanded, `.` and `..` removed: the store id's
//! account, [`bunko_remote::folder::account_id`]); the checks resolve whatever of it
//! exists, so a symlink or a second spelling cannot slip past them.

use std::path::{Component, Path, PathBuf};

use bunko_remote::auth::{CredentialStore, StoredCredential};

/// The `credentials.json` provider name of a folder store.
pub const PROVIDER: &str = "folder";

/// "Enter the folder's full path."
pub const EMPTY: &str = "Enter the folder's full path.";
/// A relative path.
pub const RELATIVE: &str = "Enter the full path: it starts with / or a drive letter.";

/// The folder library at `path`, checked against Bunko's own folder `base`: refused
/// when it is `base`, holds `base`, or lies inside `base` anywhere but in
/// `<base>/library` (allowed, so a library on this computer can become a folder library
/// where it is). Returns the path as the store id names it (absolute, `~` expanded,
/// lexically normalised, never resolved).
pub fn check_path(path: &Path, base: &Path) -> Result<PathBuf, String> {
    if path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty() {
        return Err(EMPTY.into());
    }
    let full = bunko_core::storage::expand_user(path);
    if !full.is_absolute() {
        return Err(RELATIVE.into());
    }
    let full = PathBuf::from(bunko_remote::folder::account_id(&full));
    let base_shown = base.display().to_string();
    let base_r = fold(&resolve(base));
    let lib_r = fold(&resolve(&base.join("library")));
    let path_r = fold(&resolve(&full));
    if path_r == base_r || (path_r.starts_with(&base_r) && !path_r.starts_with(&lib_r)) {
        return Err(format!(
            "That folder is part of Bunko's own folder ({base_shown}). Choose a folder outside it, or {}.",
            base.join("library").display()
        ));
    }
    if base_r.starts_with(&path_r) {
        return Err(format!(
            "That folder holds Bunko's own folder ({base_shown}). Choose the manga folder itself."
        ));
    }
    Ok(full)
}

/// What the connect Check says before the folder is opened: a file in its place, or
/// neither the folder nor its parent (a typo, a drive not connected). `None`: go on
/// (the folder is there, or only it is missing: "Create it?").
// unmounted: the library folder itself is a plain path on this computer.
pub fn precheck(path: &Path) -> Option<String> {
    match bunko_store::fs::metadata(path) {
        Ok(m) if m.is_dir() => None,
        Ok(_) => Some(format!("{} is a file, not a folder.", path.display())),
        Err(_) => match path.parent() {
            Some(parent) if !bunko_store::fs::is_dir(parent) => Some(missing_parent(path)),
            _ => None,
        },
    }
}

/// The Check's copy for a folder whose parent is missing too.
pub fn missing_parent(path: &Path) -> String {
    let parent = path
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    format!(
        "There is no folder at {}, and {parent} is missing too: check the path, or connect the drive.",
        path.display()
    )
}

/// The credentials stub of a folder store (§2.4): no secret, only the path, so the
/// machinery every cloud store has (the `use_local` refusal, Disconnect, an account
/// change) treats it like one.
pub fn stub(account: &str) -> StoredCredential {
    StoredCredential {
        provider: PROVIDER.into(),
        account_id: Some(account.to_string()),
        account_label: Some(account.to_string()),
        ..StoredCredential::default()
    }
}

/// Write the stub of store `id` when `cloud` has none (an env-only Docker setup never
/// ran `library connect`). Failures are logged: the store opens without it.
pub fn ensure_stub(cloud: &Path, id: &str, account: &str) {
    let creds = match CredentialStore::open(cloud) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("[STORAGE] The folder library's credentials entry: {e}");
            return;
        }
    };
    match creds.get(id) {
        Ok(Some(_)) => {}
        Ok(None) => {
            if let Err(e) = creds.put(id, stub(account)) {
                tracing::warn!(
                    "[STORAGE] Could not write the folder library's credentials entry: {e}"
                );
            }
        }
        Err(e) => tracing::warn!("[STORAGE] The folder library's credentials entry: {e}"),
    }
}

/// `p` with whatever of it exists resolved (symlinks, `..`, the on-disk spelling where
/// the filesystem gives it), the rest appended as written.
// unmounted: Bunko's own folder and the library folder are plain paths.
fn resolve(p: &Path) -> PathBuf {
    let p = PathBuf::from(bunko_remote::folder::account_id(p));
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.as_path();
    loop {
        if let Ok(c) = bunko_store::fs::canonicalize(cur) {
            let mut out = strip_verbatim(c);
            for part in rest.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cur.file_name(), cur.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p,
        }
    }
}

/// Windows' `canonicalize` answers `\\?\C:\…`; the paths compared are plain.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    p
}

/// Paths compared without case: library paths follow NTFS rules on every host, and a
/// folder that differs from Bunko's own only in case is refused rather than risked.
fn fold(p: &Path) -> PathBuf {
    // Every component, the drive letter included (`c:` is `C:`).
    p.components()
        .map(|c: Component<'_>| -> std::ffi::OsString {
            c.as_os_str().to_string_lossy().to_lowercase().into()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_path_refuses_bunkos_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        bunko_store::fs::create_dir_all(base.join("library")).unwrap();
        bunko_store::fs::create_dir_all(dir.path().join("manga")).unwrap();
        // Outside: taken, as written.
        let manga = dir.path().join("manga");
        assert_eq!(check_path(&manga, &base).unwrap(), manga);
        // A missing folder outside is fine too ("Create it?" comes later).
        assert!(check_path(&dir.path().join("new/manga"), &base).is_ok());
        // <base>/library and inside it: allowed.
        assert!(check_path(&base.join("library"), &base).is_ok());
        assert!(check_path(&base.join("library/sub"), &base).is_ok());
        // The base itself, anything else inside it, and a folder holding it: refused.
        for (p, says) in [
            (base.clone(), "part of Bunko's own folder"),
            (base.join("cloud"), "part of Bunko's own folder"),
            (base.join("users/x"), "part of Bunko's own folder"),
            (base.join("library/../cloud"), "part of Bunko's own folder"),
            (dir.path().to_path_buf(), "holds Bunko's own folder"),
        ] {
            let e = check_path(&p, &base).unwrap_err();
            assert!(e.contains(says), "{p:?}: {e}");
        }
        // Another spelling of the base is the base.
        let upper = dir.path().join("BUNKO").join("cloud");
        assert!(check_path(&upper, &base).is_err());
        assert_eq!(check_path(Path::new(""), &base).unwrap_err(), EMPTY);
        assert_eq!(check_path(Path::new("manga"), &base).unwrap_err(), RELATIVE);
    }

    #[cfg(unix)]
    #[test]
    fn check_path_sees_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("bunko");
        bunko_store::fs::create_dir_all(base.join("cloud")).unwrap();
        let link = dir.path().join("sneaky");
        std::os::unix::fs::symlink(base.join("cloud"), &link).unwrap();
        assert!(check_path(&link, &base).is_err());
    }

    #[test]
    fn precheck_names_a_file_and_a_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        bunko_store::fs::write(&file, b"x").unwrap();
        assert!(precheck(&file).unwrap().contains("is a file, not a folder"));
        assert_eq!(precheck(dir.path()), None);
        assert_eq!(precheck(&dir.path().join("new")), None);
        let e = precheck(&dir.path().join("gone/manga")).unwrap();
        assert!(e.contains("is missing too"), "{e}");
    }

    #[test]
    fn ensure_stub_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let cloud = dir.path().join("cloud");
        ensure_stub(&cloud, "0123456789abcdef", "/srv/manga");
        let creds = CredentialStore::open(&cloud).unwrap();
        let c = creds.get("0123456789abcdef").unwrap().unwrap();
        assert_eq!(c.provider, "folder");
        assert_eq!(c.account_id.as_deref(), Some("/srv/manga"));
        assert_eq!(c.account_label.as_deref(), Some("/srv/manga"));
        assert_eq!(c.webdav_password, None);
        // An entry on file is left alone.
        ensure_stub(&cloud, "0123456789abcdef", "/elsewhere");
        let c = creds.get("0123456789abcdef").unwrap().unwrap();
        assert_eq!(c.account_id.as_deref(), Some("/srv/manga"));
    }
}
