//! Which virtual paths carry compiled metadata (`metadata/paths.py`), and how
//! a filesystem change routes to metadata work (`middleware/fs_watcher.py`).
//! Pure functions; the HTTP interception and the watcher live in the server.

use std::path::{Component, Path};

use crate::pyunicode;

pub const SERIES_FILE_NAME: &str = "series.json";
pub const CATALOG_FILE_NAME: &str = "catalog.json";
/// The WebDAV reader root (`PathMapper.READER_ROOT`).
pub const READER_ROOT: &str = "mokuro-reader";
/// Per-user files mapped into a user's private directory: never metadata.
pub const PER_USER_FILES: &[&str] = &["volume-data.json", "profiles.json", "goals.json"];

/// `posixpath.normpath("/" + path.strip("/"))`: lexical, no filesystem access.
pub fn normalize_virtual_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.trim_matches('/').split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The library-relative part of a `/mokuro-reader/...` path (normalized
/// first, so `//`, `.` and `..` aliases agree with the resolver), else `None`.
pub fn library_relative(virtual_path: &str) -> Option<String> {
    let normalized = normalize_virtual_path(virtual_path);
    let relative = normalized.strip_prefix(&format!("/{READER_ROOT}/"))?;
    if relative.is_empty() || PER_USER_FILES.contains(&relative) {
        return None;
    }
    Some(relative.to_owned())
}

/// True for the ROOT `catalog.json` only (case-insensitive).
pub fn is_catalog_file_path(virtual_path: &str) -> bool {
    library_relative(virtual_path)
        .is_some_and(|relative| pyunicode::lower(&relative) == CATALOG_FILE_NAME)
}

/// `/mokuro-reader/<Series>/series.json` -> `<Series>` (exactly one folder
/// level, file name case-insensitive, non-blank series).
pub fn series_title_from_series_file_path(virtual_path: &str) -> Option<String> {
    let relative = library_relative(virtual_path)?;
    let (head, tail) = relative.rsplit_once('/')?;
    if pyunicode::lower(tail) != SERIES_FILE_NAME
        || head.contains('/')
        || pyunicode::strip(head).is_empty()
    {
        return None;
    }
    Some(head.to_owned())
}

pub fn is_series_file_path(virtual_path: &str) -> bool {
    series_title_from_series_file_path(virtual_path).is_some()
}

/// Any file this server compiles, and therefore owns.
pub fn is_compiled_metadata_path(virtual_path: &str) -> bool {
    is_catalog_file_path(virtual_path) || is_series_file_path(virtual_path)
}

/// What a filesystem change under the library implies (`classify_change`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryChange {
    /// Recompile just this top-level series folder.
    Series(String),
    /// A top-level entry appeared/disappeared/moved (or the path could not be
    /// classified): run the full pass, which is what prunes deleted series.
    Library,
    /// Generated content that never feeds compilation.
    Ignore,
}

/// `classify_change(library_root, path)`.
pub fn classify_change(library_root: &Path, path: &Path) -> LibraryChange {
    let Ok(relative) = path.strip_prefix(library_root) else {
        return LibraryChange::Library;
    };
    let parts: Vec<Component<'_>> = relative.components().collect();
    if parts.len() < 2 {
        return LibraryChange::Library;
    }
    match parts[0].as_os_str().to_str() {
        Some("thumbnails") => LibraryChange::Ignore,
        Some(first) => LibraryChange::Series(first.to_owned()),
        None => LibraryChange::Library,
    }
}

/// Watcher relevance (`_is_relevant`): any directory event; files whose suffix
/// is `.cbz`, `.mokuro`, `.gz`, `.webp` (case-sensitive), or named `*.mokuro.gz`.
/// `.json` is deliberately not watched (the compiler's own writes).
pub fn is_relevant_change(path: &Path, is_directory: bool) -> bool {
    if is_directory {
        return true;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    matches!(
        crate::sidecar::py_suffix(name),
        ".cbz" | ".mokuro" | ".gz" | ".webp"
    ) || name.ends_with(".mokuro.gz")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_normalize() {
        assert!(is_catalog_file_path("/mokuro-reader//catalog.json"));
        assert!(is_catalog_file_path("/x/../mokuro-reader/CATALOG.json"));
        assert_eq!(
            series_title_from_series_file_path("/mokuro-reader/Dr Stone/./series.json").as_deref(),
            Some("Dr Stone")
        );
        assert!(!is_series_file_path("/mokuro-reader/A/B/series.json"));
        assert!(!is_series_file_path("/mokuro-reader/series.json"));
        assert!(!is_catalog_file_path("/mokuro-reader/A/catalog.json"));
        assert_eq!(library_relative("/mokuro-reader/volume-data.json"), None);
        assert_eq!(
            library_relative("/../mokuro-reader/x"),
            Some("x".to_owned())
        );
    }

    #[test]
    fn classify() {
        let root = Path::new("/lib");
        assert_eq!(
            classify_change(root, Path::new("/lib/A")),
            LibraryChange::Library
        );
        assert_eq!(
            classify_change(root, Path::new("/lib/A/v.cbz")),
            LibraryChange::Series("A".into())
        );
        assert_eq!(
            classify_change(root, Path::new("/lib/thumbnails/x.webp")),
            LibraryChange::Ignore
        );
        assert_eq!(
            classify_change(root, Path::new("/elsewhere/x")),
            LibraryChange::Library
        );
    }
}
