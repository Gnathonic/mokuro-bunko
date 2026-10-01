//! Path predicates the authorisation table is written in (0.5.2 `middleware/auth.py`,
//! `metadata/paths.py`). Paths are percent-decoded UTF-8.

pub const READER_ROOT: &str = "mokuro-reader";
pub const PER_USER_FILES: &[&str] = &["volume-data.json", "profiles.json", "goals.json"];

fn normalized(path: &str) -> String {
    format!("/{}", path.trim_matches('/'))
}

fn reader_relative(path: &str) -> Option<String> {
    let p = normalized(path);
    p.strip_prefix("/mokuro-reader/").map(str::to_string)
}

pub fn is_progress_file(path: &str) -> bool {
    reader_relative(path).is_some_and(|r| PER_USER_FILES.contains(&r.as_str()))
}

pub fn is_library_path(path: &str) -> bool {
    reader_relative(path).is_some_and(|r| !r.is_empty() && !PER_USER_FILES.contains(&r.as_str()))
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
    if rel.is_empty() || PER_USER_FILES.contains(&rel) {
        return None;
    }
    Some(rel.to_string())
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
    let decoded = percent_encoding::percent_decode_str(header).decode_utf8().ok()?.into_owned();
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
        assert!(is_progress_file("/mokuro-reader/volume-data.json"));
        assert!(!is_progress_file("/mokuro-reader/Volume-Data.json"));
        assert!(is_library_path("/mokuro-reader/Volume-Data.json"));
        assert!(!is_library_path("/mokuro-reader/"));
        assert!(!is_library_path("/mokuro-reader/profiles.json"));
    }

    #[test]
    fn compiled_metadata() {
        assert!(is_catalog_file_path("/mokuro-reader/catalog.json"));
        assert!(is_catalog_file_path("/mokuro-reader//CATALOG.json"));
        assert!(is_catalog_file_path("/mokuro-reader/x/../catalog.json"));
        assert!(!is_catalog_file_path("/mokuro-reader/S/catalog.json"));
        assert_eq!(series_title_from_series_file_path("/mokuro-reader/One Piece/series.json").as_deref(), Some("One Piece"));
        assert_eq!(series_title_from_series_file_path("/mokuro-reader/./One Piece/Series.JSON").as_deref(), Some("One Piece"));
        assert!(series_title_from_series_file_path("/mokuro-reader/a/b/series.json").is_none());
        assert!(series_title_from_series_file_path("/mokuro-reader/ /series.json").is_none());
    }

    #[test]
    fn destination() {
        assert_eq!(destination_path("http://h:1/mokuro-reader/a%20b/c.cbz").as_deref(), Some("/mokuro-reader/a b/c.cbz"));
        assert_eq!(destination_path("/x/y").as_deref(), Some("/x/y"));
        assert_eq!(destination_path("").as_deref(), None);
    }
}
