//! The folder browser: the subfolders of a folder on this machine. The desktop app's
//! pickers use it (`/app/api/fs`), and so does Browse… for a library in a folder you
//! choose (`/setup/api/fs`, `/_admin/api/fs`), which answers callers on this machine
//! only: listing the server's disk to anyone else would show them what is on it.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// At most this many entries of one folder are looked at.
pub const MAX_ENTRIES: usize = 5000;

/// The refusal `/setup/api/fs` and `/_admin/api/fs` give a caller on another machine.
pub const REMOTE_REFUSAL: &str = "Browse works on this computer only: type the folder's path.";

/// The folder a `?path=` names: this user's home folder when empty, `~` expanded, and
/// a full path only (`Err("<path>: <why>")` otherwise).
pub fn requested(raw: Option<&str>) -> Result<PathBuf, String> {
    let raw = raw.map(str::trim).unwrap_or("");
    if raw.is_empty() {
        return Ok(bunko_core::storage::home_dir());
    }
    let p = bunko_core::storage::expand_user(Path::new(raw));
    if p.is_absolute() {
        Ok(p)
    } else {
        Err(format!(
            "{raw}: not a full path (it starts with / or a drive letter)"
        ))
    }
}

/// [`requested`] for the setup and admin pages: never a Windows network share or
/// device path (opening one makes Windows connect to that computer with this server's
/// credentials), refused before anything looks at it.
pub fn requested_over_http(raw: Option<&str>) -> Result<PathBuf, String> {
    let text = raw.map(str::trim).unwrap_or("");
    if cfg!(windows) && crate::storage::folder::windows_network_or_device(text) {
        return Err(format!(
            "{text}: a network share or a device; Browse shows this computer's own disks"
        ));
    }
    let p = requested(raw)?;
    #[cfg(windows)]
    if !matches!(
        p.components().next(),
        Some(std::path::Component::Prefix(pre)) if matches!(pre.kind(), std::path::Prefix::Disk(_))
    ) {
        return Err(format!(
            "{text}: a network share or a device; Browse shows this computer's own disks"
        ));
    }
    Ok(p)
}

/// The subfolders of `dir`, and the files whose extension is in `exts` (empty: none).
/// Dot folders are hidden, as are a drive's `$RECYCLE.BIN` and `System Volume
/// Information`. On Windows, `drives` lists the drives at a drive's root.
///
/// `{"path", "parent", "dirs", "files", "sep", "truncated", "drives"?}`.
// unmounted: the browser shows this machine's own disk, never a mounted library store.
#[allow(clippy::disallowed_methods)]
pub fn list_dir(dir: &Path, exts: &[String]) -> Result<Value, String> {
    let shown = dir.display().to_string();
    let dir = std::fs::canonicalize(dir).map_err(|e| format!("{shown}: {e}"))?;
    let dir = plain(dir);
    if !dir.is_dir() {
        return Err(format!("{} is a file, not a folder", dir.display()));
    }
    let root = dir.parent().is_none();
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut truncated = false;
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (i, e) in rd.flatten().enumerate() {
        if i >= MAX_ENTRIES {
            truncated = true;
            break;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || (root && ["$RECYCLE.BIN", "System Volume Information"].contains(&name.as_str()))
        {
            continue;
        }
        let Ok(ft) = e.file_type() else { continue };
        let is_dir = ft.is_dir() || (ft.is_symlink() && e.path().is_dir());
        if is_dir {
            dirs.push(name);
        } else if !exts.is_empty() {
            let ext = Path::new(&name)
                .extension()
                .map(|x| x.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if exts.iter().any(|x| x.eq_ignore_ascii_case(&ext)) {
                files.push(name);
            }
        }
    }
    dirs.sort_by_key(|a| a.to_lowercase());
    files.sort_by_key(|a| a.to_lowercase());
    let parent = dir.parent().map(|p| p.to_string_lossy().into_owned());
    let mut v = json!({
        "path": dir.to_string_lossy(),
        "parent": parent,
        "dirs": dirs,
        "files": files,
        "sep": std::path::MAIN_SEPARATOR.to_string(),
        "truncated": truncated,
    });
    if cfg!(windows) && root {
        v["drives"] = json!(drives());
    }
    Ok(v)
}

/// The drives that exist, `A:\` to `Z:\` (Windows).
// unmounted: drive roots of this machine.
#[allow(clippy::disallowed_methods)]
fn drives() -> Vec<String> {
    (b'A'..=b'Z')
        .map(|c| format!("{}:\\", c as char))
        .filter(|d| Path::new(d).is_dir())
        .collect()
}

/// `canonicalize` on Windows gives `\\?\D:\Manga` and `\\?\UNC\host\share`: the form
/// people type (and the library's store id uses) has no `\\?\`.
fn plain(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // unmounted: a scratch folder.
    #[allow(clippy::disallowed_methods)]
    fn picker_lists_folders_and_chosen_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Manga")).unwrap();
        std::fs::create_dir(dir.path().join("comics")).unwrap();
        std::fs::create_dir(dir.path().join(".hidden")).unwrap();
        std::fs::write(dir.path().join("cert.PEM"), "x").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        let v = list_dir(dir.path(), &[]).unwrap();
        assert_eq!(v["dirs"], json!(["comics", "Manga"]));
        assert_eq!(v["files"], json!([]));
        assert_eq!(v["truncated"], json!(false));
        assert!(v.get("drives").is_none() || cfg!(windows));
        let v = list_dir(dir.path(), &["pem".into()]).unwrap();
        assert_eq!(v["files"], json!(["cert.PEM"]));
        let e = list_dir(&dir.path().join("nope"), &[]).unwrap_err();
        assert!(e.contains("nope"), "{e}");
        let e = list_dir(&dir.path().join("notes.txt"), &[]).unwrap_err();
        assert!(e.ends_with("is a file, not a folder"), "{e}");
    }

    #[test]
    fn a_request_names_a_full_path() {
        assert_eq!(requested(None).unwrap(), bunko_core::storage::home_dir());
        assert_eq!(
            requested(Some("  ")).unwrap(),
            bunko_core::storage::home_dir()
        );
        assert!(requested(Some("manga")).unwrap_err().starts_with("manga: "));
        assert_eq!(
            requested(Some("~")).unwrap(),
            bunko_core::storage::home_dir()
        );
        let abs = if cfg!(windows) {
            r"D:\Manga"
        } else {
            "/srv/manga"
        };
        assert_eq!(requested(Some(abs)).unwrap(), PathBuf::from(abs));
    }

    #[test]
    fn verbatim_prefixes_go() {
        assert_eq!(
            plain(PathBuf::from(r"\\?\D:\Manga")),
            PathBuf::from(r"D:\Manga")
        );
        assert_eq!(
            plain(PathBuf::from(r"\\?\UNC\files.example.com\manga")),
            PathBuf::from(r"\\files.example.com\manga")
        );
        assert_eq!(
            plain(PathBuf::from("/srv/manga")),
            PathBuf::from("/srv/manga")
        );
    }
}
