//! Library paths are case-insensitive and case-preserving on every host (0.5.3
//! `webdav/path_case.py` and `middleware/path_case.py`; design 0.8 D5).
//!
//! Bunko works to the lowest common denominator of the filesystems it runs on. On NTFS
//! and APFS `Kingdom/` and `kingdom/` are one folder, so the library behaves that way
//! everywhere: Linux (case-sensitive or casefold), macOS, Windows, SMB mounts and every
//! remote store. A name that differs only in case from an existing entry addresses that
//! entry, and Bunko never creates a second spelling. Nothing here probes the host: the
//! answer is computed from the folder listing alone, so it is the same on every host.
//!
//! [`PathCase::rewrite_request`] closes that at the edge, before anything else reads the
//! request path: every segment of a library request path that names something already in
//! the library takes its listed spelling, so a PUT, MKCOL, PROPFIND or GET for
//! `kingdom/Vol 13.cbz` lands in (or reads) `Kingdom/`, and every layer below -- the auth
//! gate's ownership checks, the series.json PUT, the DAV handler, the database rows keyed
//! by library path -- sees one spelling for one file. A segment that names nothing keeps
//! the client's spelling: that is the case-preserving half.
//!
//! A folder that already holds two or more spellings of one name (only a case-sensitive
//! disk or a remote store can) is addressed through ONE of them, whatever the request
//! spells, the exact spelling of another included: the store's case winner (a remote
//! store's oldest spelling, the one its case merge keeps), else the byte-smallest. The
//! others are shadowed.
//!
//! The per-user names at the reader root (`history`, `goals.json`, ...) are reserved in
//! every spelling: a request for `/mokuro-reader/History/x` is rewritten to the canonical
//! `history/x`, the caller's own tree, and never resolves into the library.
//!
//! Renaming to fix the case stays possible, as it is on NTFS: a MOVE whose destination is
//! a case-variant of its own source keeps the destination's spelling (rewriting it to the
//! source's would make it a move onto itself).

use std::path::{Path, PathBuf};

use bunko_core::reader_tree;
use bunko_store::fs;
use http::{Uri, request::Parts};

use crate::paths::{self, READER_ROOT};

pub use bunko_store::fs::same_file;

/// The spelling-insensitive form two names collide under: NFC (so a decomposed and a
/// composed spelling collide, as on APFS), then a full Unicode lowercase
/// ([`bunko_core::sidecar_names::fold`], the one filename fold). Rare names may fold
/// otherwise than a filesystem's own table does (`ß`/`ss`, final sigma, the Kelvin
/// sign, the long s).
pub fn fold_name(name: &str) -> String {
    bunko_core::sidecar_names::fold(name)
}

/// The one listed spelling `name` resolves to in `parent`, or `None` when nothing there
/// folds like it.
///
/// Every spelling, the exact one included, resolves to a single winner: when two or more
/// listed names fold alike (only a case-sensitive disk or a remote store can hold them),
/// the store's [`case_winner`] when it is listed (a remote store's oldest spelling, the
/// one its case merge keeps), else the byte-smallest. The other spellings are never
/// addressed. The listing is always scanned: no probe, no exact fast path.
///
/// At the library root (`top`) a name that folds to a per-user name (`History`,
/// `Goals.json`) is never a library entry.
///
/// [`case_winner`]: bunko_store::LibraryStore::case_winner
pub(crate) fn on_disk_name(parent: &Path, name: &str, top: bool) -> Option<String> {
    if top && reader_tree::is_per_user_root_name(name) {
        return None;
    }
    let target = fold_name(name);
    let mut variants: Vec<String> = Vec::new();
    for entry in fs::read_dir(parent).ok()?.flatten() {
        let entry_name = entry.file_name();
        // A name that is not UTF-8 can never be what a (UTF-8) request spells.
        let Some(entry_name) = entry_name.to_str() else {
            continue;
        };
        if top && reader_tree::is_per_user_root_name(entry_name) {
            continue;
        }
        if entry_name == name || fold_name(entry_name) == target {
            variants.push(entry_name.to_string());
        }
    }
    pick_winner(parent, &target, variants)
}

/// The winner among `variants` (listed spellings of the fold `target` in `parent`).
pub(crate) fn pick_winner(parent: &Path, target: &str, variants: Vec<String>) -> Option<String> {
    if variants.len() > 1
        && let Some(w) = bunko_store::store_for(parent).and_then(|s| s.case_winner(parent, target))
        && variants.contains(&w)
    {
        return Some(w);
    }
    variants.into_iter().min()
}

/// `relative` (below `base`) with every existing segment in its listed spelling
/// ([`on_disk_name`]); segments past the first one that names nothing keep the requested
/// spelling. `top_reserved`: `base` is the library root, where per-user names are never
/// library entries. `.`/`..` segments are never resolved through (the path is returned
/// unchanged); empty segments are carried through.
#[allow(dead_code)] // the overlay and history lookups use it next
pub(crate) fn resolve_existing(base: &Path, relative: &str, top_reserved: bool) -> String {
    resolve(base, relative, None, top_reserved)
}

fn resolve(base: &Path, relative: &str, keep: Option<&str>, top_reserved: bool) -> String {
    let mut parts: Vec<String> = relative.split('/').map(str::to_string).collect();
    if parts.iter().any(|p| p == "." || p == "..") {
        return relative.to_string();
    }
    let named: Vec<usize> = (0..parts.len()).filter(|&i| !parts[i].is_empty()).collect();
    let keep = keep.map(|k| k.trim_matches('/'));
    let mut parent = base.to_path_buf();
    let mut resolved: Vec<String> = Vec::new();
    for (position, &index) in named.iter().enumerate() {
        let part = parts[index].clone();
        let Some(on_disk) = on_disk_name(&parent, &part, top_reserved && position == 0) else {
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

/// Maps a library-relative path onto the spelling already in the library (0.5.3
/// `LibraryPathCanonicalizer`).
#[derive(Debug, Clone)]
pub struct LibraryPathCanonicalizer {
    library: PathBuf,
}

impl LibraryPathCanonicalizer {
    pub fn new(library: impl Into<PathBuf>) -> Self {
        Self {
            library: library.into(),
        }
    }

    pub fn library(&self) -> &Path {
        &self.library
    }

    /// `relative` with every existing segment in its listed spelling (see
    /// [`on_disk_name`] for which one, when a folder lists several).
    ///
    /// `keep_last_if_variant_of` is a MOVE's (already canonical) source: when the last
    /// segment resolves to that very entry, the client is renaming it and the requested
    /// spelling is kept. Traversal (`.`/`..` segments) is never resolved through: it is
    /// refused downstream. Empty segments (a trailing or doubled slash) are carried
    /// through untouched so the request keeps its exact shape.
    pub fn canonicalize(&self, relative: &str, keep_last_if_variant_of: Option<&str>) -> String {
        resolve(&self.library, relative, keep_last_if_variant_of, true)
    }
}

const LIBRARY_PREFIX: &str = "/mokuro-reader/";

/// The library-relative part of a decoded request path, or `None` for anything that is
/// not a library path (the roots, the caller's own tree, other trees).
fn library_relative(path: &str) -> Option<&str> {
    debug_assert_eq!(LIBRARY_PREFIX, format!("/{READER_ROOT}/"));
    let relative = path.strip_prefix(LIBRARY_PREFIX)?;
    if relative.trim_matches('/').is_empty() || reader_tree::is_per_user_rel(relative) {
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
#[allow(clippy::disallowed_methods)] // fixtures are written with std::fs
mod tests {
    use super::*;

    #[test]
    fn folds_like_python() {
        assert_eq!(fold_name("Kingdom"), "kingdom");
        // Decomposed É (E + U+0301) folds with the composed é.
        assert_eq!(fold_name("POKE\u{301}MON"), fold_name("Pok\u{e9}mon"));
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
}
