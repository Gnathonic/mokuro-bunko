//! Path predicates the authorisation table is written in (0.5.2 `middleware/auth.py`,
//! `metadata/paths.py`). Paths are percent-decoded UTF-8.

use bunko_core::reader_tree;
pub use bunko_core::reader_tree::READER_ROOT;
use bunko_core::sidecar_names::{self, LayerClass};

fn normalized(path: &str) -> String {
    format!("/{}", path.trim_matches('/'))
}

fn reader_relative(path: &str) -> Option<String> {
    let p = normalized(path);
    p.strip_prefix("/mokuro-reader/").map(str::to_string)
}

/// The caller's own tree: a root file (`goals.json`, ...) or `history/**`.
pub fn is_progress_path(path: &str) -> bool {
    reader_relative(path).is_some_and(|r| reader_tree::is_per_user_rel(&r))
}

/// `history` or below it: the account's reading history.
pub fn is_history_path(path: &str) -> bool {
    reader_relative(path).is_some_and(|r| reader_tree::is_history_rel(&r))
}

pub fn is_library_path(path: &str) -> bool {
    reader_relative(path)
        .is_some_and(|r| !r.trim_matches('/').is_empty() && !reader_tree::is_per_user_rel(&r))
}

pub fn is_inbox_path(path: &str) -> bool {
    path == "/inbox" || path.starts_with("/inbox/")
}

pub fn is_admin_path(path: &str) -> bool {
    path.starts_with("/_admin")
}

pub fn is_processor_path(path: &str) -> bool {
    path == "/_processor" || path.starts_with("/_processor/")
}

pub fn is_invites_admin_api_path(path: &str) -> bool {
    path == "/_admin/api/invites" || path.starts_with("/_admin/api/invites/")
}

/// `posixpath.normpath("/" + p.strip("/"))`.
pub fn posix_normpath(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    format!("/{}", parts.join("/"))
}

fn library_relative_normalized(path: &str) -> Option<String> {
    let n = posix_normpath(&format!("/{}", path.trim_matches('/')));
    let rel = n.strip_prefix("/mokuro-reader/")?;
    if rel.is_empty() || reader_tree::is_per_user_rel(rel) {
        return None;
    }
    Some(rel.to_string())
}

/// The library-relative path of a library request path, its empty and `.` segments
/// dropped (`/mokuro-reader//S/./V.cbz` → `S/V.cbz`), as DAV resolves it: every rule
/// that reads segments reads them from here, so no spelling of a path slips past one.
pub fn library_rel(path: &str) -> Option<String> {
    library_relative_normalized(path)
}

/// [`library_rel`] as a request path again (`/mokuro-reader/S/V.cbz`).
pub fn normalized_library_path(path: &str) -> Option<String> {
    library_rel(path).map(|rel| format!("/{READER_ROOT}/{rel}"))
}

/// The series folder a library path is INSIDE: its first segment, when it has more.
pub fn series_folder_of(path: &str) -> Option<String> {
    let rel = library_rel(path)?;
    rel.split_once('/').map(|(folder, _)| folder.to_string())
}

/// The library path itself when it is a top-level entry (one segment).
pub fn top_level_folder(path: &str) -> Option<String> {
    library_rel(path).filter(|rel| !rel.contains('/'))
}

/// The [`LayerClass`] of the layer id a library file's name carries
/// (`S/V.edit.mokuro` → Personal), the id exact as written (rule v1). Listing-free:
/// the name alone cannot tell a layer from a dotted volume's primary, which
/// `AuthBackend::is_volume_primary` asks the folder about.
pub fn layer_class_of_path(path: &str) -> Option<LayerClass> {
    layer_id_of_path(path).map(|id| sidecar_names::layer_class(&id))
}

/// The layer id a library file's name carries (`curated-2` for
/// `S/V.curated-2.mokuro`), listing-free and exact-case like
/// [`layer_class_of_path`].
pub fn layer_id_of_path(path: &str) -> Option<String> {
    let rel = library_relative_normalized(path)?;
    let name = rel.rsplit('/').next()?;
    let (_, id, _) = sidecar_names::split_layer(name)?;
    Some(id.to_string())
}

/// The ROOT `catalog.json` only (ASCII case-insensitive).
pub fn is_catalog_file_path(path: &str) -> bool {
    library_relative_normalized(path).is_some_and(|r| r.eq_ignore_ascii_case("catalog.json"))
}

/// `/mokuro-reader/<Series>/series.json` → `<Series>`.
pub fn series_title_from_series_file_path(path: &str) -> Option<String> {
    let rel = library_relative_normalized(path)?;
    let (head, tail) = rel.rsplit_once('/')?;
    if !tail.eq_ignore_ascii_case("series.json") || head.contains('/') || head.trim().is_empty() {
        return None;
    }
    Some(head.to_string())
}

pub fn is_series_file_path(path: &str) -> bool {
    series_title_from_series_file_path(path).is_some()
}

pub fn is_compiled_metadata_path(path: &str) -> bool {
    is_catalog_file_path(path) || is_series_file_path(path)
}

/// The path part of a `Destination` header (absolute URI or bare path, percent-encoded).
pub fn destination_path(header: &str) -> Option<String> {
    let decoded = percent_encoding::percent_decode_str(header)
        .decode_utf8()
        .ok()?
        .into_owned();
    let path = if let Some(rest) = decoded.split_once("://").map(|(_, r)| r) {
        match rest.find('/') {
            Some(i) => rest[i..].to_string(),
            None => String::new(),
        }
    } else {
        decoded
    };
    let path = path.split(['?', '#']).next().unwrap_or("").to_string();
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_and_library() {
        assert!(is_progress_path("/mokuro-reader/volume-data.json"));
        assert!(!is_progress_path("/mokuro-reader/Volume-Data.json"));
        assert!(is_library_path("/mokuro-reader/Volume-Data.json"));
        assert!(!is_library_path("/mokuro-reader/"));
        assert!(!is_library_path("/mokuro-reader/profiles.json"));
        // The reader's position index is the user's own, like their progress.
        assert!(is_progress_path("/mokuro-reader/positions.json"));
        assert!(!is_library_path("/mokuro-reader/positions.json"));
    }

    #[test]
    fn history_paths() {
        for p in [
            "/mokuro-reader/history",
            "/mokuro-reader/history/",
            "/mokuro-reader/history/d/2026-10.events",
        ] {
            assert!(is_progress_path(p), "{p}");
            assert!(is_history_path(p), "{p}");
            assert!(!is_library_path(p), "{p}");
        }
        assert!(is_progress_path("/mokuro-reader/goals.json"));
        assert!(!is_history_path("/mokuro-reader/goals.json"));
        for p in ["/mokuro-reader/History/v.cbz", "/mokuro-reader/historyx"] {
            assert!(!is_progress_path(p), "{p}");
            assert!(!is_history_path(p), "{p}");
            assert!(is_library_path(p), "{p}");
        }
        assert!(!is_compiled_metadata_path(
            "/mokuro-reader/history/series.json"
        ));
    }

    #[test]
    fn layer_classes() {
        let class = layer_class_of_path;
        assert_eq!(
            class("/mokuro-reader/S/Vol 1.5.edit.mokuro.gz"),
            Some(LayerClass::Personal)
        );
        assert_eq!(
            class("/mokuro-reader/S/V.edit-12.MOKURO"),
            Some(LayerClass::Personal)
        );
        assert_eq!(
            class("/mokuro-reader/S/V.curated.mokuro"),
            Some(LayerClass::Curated)
        );
        // Neither id is reserved: hand-made ids like any other.
        for p in [
            "/mokuro-reader/V.pre-upgrade-2.mokuro",
            "/mokuro-reader/S/V.previous-ocr.mokuro",
        ] {
            assert_eq!(class(p), Some(LayerClass::Other), "{p}");
        }
        assert_eq!(
            class("/mokuro-reader/S/V.curated-2.mokuro"),
            Some(LayerClass::Reserved)
        );
        assert_eq!(
            class("/mokuro-reader/S/V.hayai.mokuro"),
            Some(LayerClass::Other)
        );
        // Exact ids: `EDIT` is no layer id.
        assert_eq!(class("/mokuro-reader/S/V.EDIT.mokuro"), None);
        for p in [
            "/mokuro-reader/S/V.mokuro",
            "/mokuro-reader/S/V.edit.cbz",
            "/mokuro-reader/history/V.edit.mokuro",
            "/mokuro-reader/volume-data.json",
            "/elsewhere/V.edit.mokuro",
        ] {
            assert_eq!(class(p), None, "{p}");
        }
    }

    #[test]
    fn compiled_metadata() {
        assert!(is_catalog_file_path("/mokuro-reader/catalog.json"));
        assert!(is_catalog_file_path("/mokuro-reader//CATALOG.json"));
        assert!(is_catalog_file_path("/mokuro-reader/x/../catalog.json"));
        assert!(!is_catalog_file_path("/mokuro-reader/S/catalog.json"));
        assert_eq!(
            series_title_from_series_file_path("/mokuro-reader/One Piece/series.json").as_deref(),
            Some("One Piece")
        );
        assert_eq!(
            series_title_from_series_file_path("/mokuro-reader/./One Piece/Series.JSON").as_deref(),
            Some("One Piece")
        );
        assert!(series_title_from_series_file_path("/mokuro-reader/a/b/series.json").is_none());
        assert!(series_title_from_series_file_path("/mokuro-reader/ /series.json").is_none());
        // The reading-history tree is the account's own, never compiled metadata.
        assert!(!is_series_file_path("/mokuro-reader/history/series.json"));
        assert!(is_series_file_path("/mokuro-reader/History/series.json"));
    }

    /// C4: the segment helpers read the path as DAV resolves it.
    #[test]
    fn series_folders_of_any_spelling_of_a_path() {
        for p in [
            "/mokuro-reader/S/V.cbz",
            "/mokuro-reader//S/V.cbz",
            "/mokuro-reader/./S/V.cbz",
            "/mokuro-reader/S//V.cbz",
            "/mokuro-reader/S/./V.cbz",
            "//mokuro-reader/S/V.cbz/",
        ] {
            assert_eq!(series_folder_of(p).as_deref(), Some("S"), "{p}");
            assert_eq!(top_level_folder(p), None, "{p}");
            assert_eq!(
                normalized_library_path(p).as_deref(),
                Some("/mokuro-reader/S/V.cbz"),
                "{p}"
            );
        }
        for p in [
            "/mokuro-reader/S",
            "/mokuro-reader//S/",
            "/mokuro-reader/./S",
        ] {
            assert_eq!(top_level_folder(p).as_deref(), Some("S"), "{p}");
            assert_eq!(series_folder_of(p), None, "{p}");
        }
        assert_eq!(
            series_folder_of("/mokuro-reader/A/B/V.cbz").as_deref(),
            Some("A")
        );
        for p in [
            "/mokuro-reader/",
            "/mokuro-reader/history/d/x",
            "/elsewhere/S/V.cbz",
        ] {
            assert_eq!(series_folder_of(p), None, "{p}");
            assert_eq!(top_level_folder(p), None, "{p}");
        }
    }

    #[test]
    fn destination() {
        assert_eq!(
            destination_path("http://h:1/mokuro-reader/a%20b/c.cbz").as_deref(),
            Some("/mokuro-reader/a b/c.cbz")
        );
        assert_eq!(destination_path("/x/y").as_deref(), Some("/x/y"));
        assert_eq!(destination_path("").as_deref(), None);
    }
}
