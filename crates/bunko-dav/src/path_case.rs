//! Library paths behave as on NTFS: case-insensitive, case-preserving (0.5.3
//! `webdav/path_case.py` and `middleware/path_case.py`).
//!
//! The library has to stay valid on the most restrictive filesystem bunko runs on. On
//! NTFS (and APFS) `Kingdom/` and `kingdom/` are one folder, so a library holding both
//! cannot be copied to, synced with, or served from Windows. On a case-sensitive host they
//! are two folders, and an upload spelled `kingdom/` used to create the second one beside
//! the first (prod 2026-10-06: the catalog then showed one of them and hid the other's
//! volumes).
//!
//! [`PathCase::rewrite_request`] closes that at the edge, before anything else reads the
//! request path: every segment of a library request path that names something already on
//! disk takes the on-disk spelling, so a PUT, MKCOL, PROPFIND or GET for
//! `kingdom/Vol 80.cbz` lands in (or reads) `Kingdom/`, and every layer below -- the auth
//! gate's ownership checks, the series.json PUT, the DAV handler, the database rows keyed
//! by library path -- sees one spelling for one file. A segment that names nothing on disk
//! keeps the client's spelling: that is the case-preserving half.
//!
//! Renaming to fix the case stays possible, as it is on NTFS: a MOVE whose destination is
//! a case-variant of its own source keeps the destination's spelling (rewriting it to the
//! source's would make it a move onto itself).

use std::path::{Path, PathBuf};

use http::{Uri, request::Parts};
use unicode_normalization::UnicodeNormalization;

use crate::paths::{self, READER_ROOT};

/// The spelling-insensitive form two names collide under: NFC (so a decomposed and a
/// composed spelling collide, as on APFS), then a full Unicode lowercase -- the two steps
/// the series-identity fold applies, minus its whitespace collapsing, which no filesystem
/// does to a name.
pub fn fold_name(name: &str) -> String {
    name.nfc().collect::<String>().to_lowercase()
}

/// Python `str.swapcase` (close enough for a probe: any cased letter changes).
fn swapcase(name: &str) -> String {
    name.chars()
        .flat_map(|c| {
            if c.is_lowercase() {
                c.to_uppercase().collect::<Vec<_>>()
            } else if c.is_uppercase() {
                c.to_lowercase().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

/// Do `a` and `b` name one existing file or folder (Python `os.path.samefile`)? `false`
/// when either is missing.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        // Windows: the final path of an open handle carries the on-disk spelling.
        matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
    }
}

/// Whether the filesystem holding `path` tells names apart by case. Probed by asking for
/// `path` under its own name case-swapped: only a case-insensitive filesystem finds the
/// same directory there. A name with no cased letter (or one that cannot be probed) counts
/// as case-sensitive, which only costs a directory scan per unmatched segment.
pub fn is_case_sensitive(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return true;
    };
    let swapped = swapcase(name);
    if swapped == name {
        return true;
    }
    !same_file(path, &path.with_file_name(swapped))
}

/// Maps a library-relative path onto the spelling already on disk (0.5.3
/// `LibraryPathCanonicalizer`).
#[derive(Debug, Clone)]
pub struct LibraryPathCanonicalizer {
    library: PathBuf,
    case_sensitive: bool,
}

impl LibraryPathCanonicalizer {
    /// `case_sensitive: None` probes the filesystem holding `library`.
    pub fn new(library: impl Into<PathBuf>, case_sensitive: Option<bool>) -> Self {
        let library = library.into();
        let case_sensitive = case_sensitive.unwrap_or_else(|| is_case_sensitive(&library));
        Self {
            library,
            case_sensitive,
        }
    }

    pub fn library(&self) -> &Path {
        &self.library
    }

    pub fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    /// The name `name` is spelled with in `parent`, or `None` when absent. The exact
    /// spelling wins over a variant (a library that already holds both keeps both
    /// addressable); among variants only, the first in sorted order, so the choice is
    /// stable.
    fn on_disk_name(&self, parent: &Path, name: &str) -> Option<String> {
        if self.case_sensitive && std::fs::symlink_metadata(parent.join(name)).is_ok() {
            return Some(name.to_string());
        }
        let target = fold_name(name);
        let mut variants: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(parent).ok()?.flatten() {
            let entry_name = entry.file_name();
            // A name that is not UTF-8 can never be what a (UTF-8) request spells.
            let Some(entry_name) = entry_name.to_str() else {
                continue;
            };
            if entry_name == name {
                return Some(name.to_string());
            }
            if fold_name(entry_name) == target {
                variants.push(entry_name.to_string());
            }
        }
        variants.into_iter().min()
    }

    /// `relative` with every existing segment in its on-disk spelling.
    ///
    /// `keep_last_if_variant_of` is a MOVE's (already canonical) source: when the last
    /// segment resolves to that very entry, the client is renaming it and the requested
    /// spelling is kept. Traversal (`.`/`..` segments) is never resolved through: it is
    /// refused downstream. Empty segments (a trailing or doubled slash) are carried
    /// through untouched so the request keeps its exact shape.
    pub fn canonicalize(&self, relative: &str, keep_last_if_variant_of: Option<&str>) -> String {
        let mut parts: Vec<String> = relative.split('/').map(str::to_string).collect();
        if parts.iter().any(|p| p == "." || p == "..") {
            return relative.to_string();
        }
        let named: Vec<usize> = (0..parts.len()).filter(|&i| !parts[i].is_empty()).collect();
        let keep = keep_last_if_variant_of.map(|k| k.trim_matches('/'));
        let mut parent = self.library.clone();
        let mut resolved: Vec<String> = Vec::new();
        for (position, &index) in named.iter().enumerate() {
            let part = parts[index].clone();
            let Some(on_disk) = self.on_disk_name(&parent, &part) else {
                break;
            };
            let is_last = position + 1 == named.len();
            if is_last
                && on_disk != part
                && keep.is_some_and(|k| {
                    let mut candidate = resolved.join("/");
                    if !candidate.is_empty() {
                        candidate.push('/');
                    }
                    candidate.push_str(&on_disk);
                    candidate == k
                })
            {
                break;
            }
            parent.push(&on_disk);
            parts[index] = on_disk.clone();
            resolved.push(on_disk);
        }
        parts.join("/")
    }
}

const LIBRARY_PREFIX: &str = "/mokuro-reader/";

/// The library-relative part of a decoded request path, or `None` for anything that is
/// not a library path (the roots, the per-user files, other trees).
fn library_relative(path: &str) -> Option<&str> {
    debug_assert_eq!(LIBRARY_PREFIX, format!("/{READER_ROOT}/"));
    let relative = path.strip_prefix(LIBRARY_PREFIX)?;
    if relative.trim_matches('/').is_empty() || paths::is_per_user_name(relative) {
        return None;
    }
    Some(relative)
}

/// The request-edge rewrite (0.5.3 `PathCaseMiddleware`).
#[derive(Debug, Clone)]
pub struct PathCase {
    canonicalizer: LibraryPathCanonicalizer,
}

impl PathCase {
    pub fn new(canonicalizer: LibraryPathCanonicalizer) -> Self {
        Self { canonicalizer }
    }

    pub fn canonicalizer(&self) -> &LibraryPathCanonicalizer {
        &self.canonicalizer
    }

    /// A decoded request path in on-disk spelling (unchanged when it is not a library
    /// path). Blocking.
    pub fn canonical_path(&self, path: &str, keep_last_if_variant_of: Option<&str>) -> String {
        match library_relative(path) {
            None => path.to_string(),
            Some(relative) => format!(
                "{LIBRARY_PREFIX}{}",
                self.canonicalizer
                    .canonicalize(relative, keep_last_if_variant_of)
            ),
        }
    }

    /// Might [`PathCase::rewrite_request`] change this request? Cheap (no filesystem):
    /// lets a caller skip the blocking hop for everything else.
    pub fn may_rewrite(parts: &Parts) -> bool {
        let library = paths::decode_request_path(parts.uri.path())
            .is_some_and(|p| library_relative(&p).is_some());
        library
            || (matches!(parts.method.as_str(), "MOVE" | "COPY")
                && parts.headers.contains_key("destination"))
    }

    /// Rewrite the request path, and a MOVE/COPY `Destination`, to the on-disk spelling of
    /// whatever already exists. Blocking (directory scans).
    pub fn rewrite_request(&self, parts: &mut Parts) {
        // Not UTF-8: no name on disk is spelled that way, and the DAV layer refuses it.
        let Some(path) = paths::decode_request_path(parts.uri.path()) else {
            return;
        };
        let canonical = self.canonical_path(&path, None);
        if canonical != path
            && let Some(uri) = with_path(&parts.uri, &paths::quote_path(&canonical))
        {
            parts.uri = uri;
        }
        if matches!(parts.method.as_str(), "MOVE" | "COPY") {
            let keep = if parts.method.as_str() == "MOVE" {
                library_relative(&canonical)
            } else {
                None
            };
            self.rewrite_destination(parts, keep);
        }
    }

    /// Canonicalize the `Destination` header, split the way the DAV layer reads it.
    fn rewrite_destination(&self, parts: &mut Parts, keep: Option<&str>) {
        let Some(header) = parts
            .headers
            .get("destination")
            .and_then(|v| v.to_str().ok())
        else {
            return;
        };
        let path_start = match header.find("://") {
            Some(i) => match header[i + 3..].find('/') {
                Some(j) => i + 3 + j,
                None => return,
            },
            None => 0,
        };
        let path_end = header[path_start..]
            .find(['?', '#'])
            .map_or(header.len(), |k| path_start + k);
        let Some(path) = paths::decode_request_path(&header[path_start..path_end]) else {
            return;
        };
        let canonical = self.canonical_path(&path, keep);
        if canonical == path {
            return;
        }
        let rewritten = format!(
            "{}{}{}",
            &header[..path_start],
            paths::quote_path(&canonical),
            &header[path_end..]
        );
        if let Ok(value) = http::HeaderValue::from_str(&rewritten) {
            parts.headers.insert("destination", value);
        }
    }
}

/// `uri` with its path replaced (query kept).
fn with_path(uri: &Uri, encoded_path: &str) -> Option<Uri> {
    let path_and_query = match uri.query() {
        Some(q) => format!("{encoded_path}?{q}"),
        None => encoded_path.to_string(),
    };
    let mut up = uri.clone().into_parts();
    up.path_and_query = Some(path_and_query.parse().ok()?);
    Uri::from_parts(up).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_like_python() {
        assert_eq!(fold_name("Kingdom"), "kingdom");
        // Decomposed É (E + U+0301) folds with the composed é.
        assert_eq!(fold_name("POKE\u{301}MON"), fold_name("Pok\u{e9}mon"));
        assert_eq!(swapcase("Kingdom 第1巻"), "kINGDOM 第1巻");
    }

    #[test]
    fn library_relative_paths() {
        assert_eq!(
            library_relative("/mokuro-reader/Kingdom/"),
            Some("Kingdom/")
        );
        assert_eq!(library_relative("/mokuro-reader/"), None);
        assert_eq!(library_relative("/mokuro-reader//"), None);
        assert_eq!(library_relative("/mokuro-reader/volume-data.json"), None);
        assert_eq!(
            library_relative("/mokuro-reader/Volume-Data.json"),
            Some("Volume-Data.json")
        );
        assert_eq!(library_relative("/mokuro-reader"), None);
        assert_eq!(library_relative("/inbox/x"), None);
    }

    #[test]
    fn probes_this_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("Library");
        std::fs::create_dir(&lib).unwrap();
        // Whatever the host is, the probe must agree with a direct look.
        let insensitive = std::fs::metadata(dir.path().join("lIBRARY")).is_ok();
        assert_eq!(is_case_sensitive(&lib), !insensitive);
        assert!(is_case_sensitive(&dir.path().join("123")));
    }
}
