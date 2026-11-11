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
//! the client's spelling: that is the case-preserving half. Except for a personal or
//! curated layer name (`V.edit.mokuro`, `V.curated.mokuro`): it never takes the spelling
//! of a listed entry that is one name with it but no such layer (`V.ed\u{200B}it.mokuro`,
//! `V.EDIT.mokuro`), so auth and DAV classify the layer the request names, an account's
//! own correction staying its own.
//!
//! A folder that already holds two or more spellings of one name (only a case-sensitive
//! disk or a remote store can) is addressed through ONE of them, whatever the request
//! spells, the exact spelling of another included: the store's case winner (a remote
//! store's oldest spelling, the one its case merge keeps), else the byte-smallest. The
//! others are shadowed. Such a case-variant group is frozen until it is merged by hand
//! ([`bunko_core::fold_groups`], [`frozen`]): every write to a member, the winner
//! included, to a file rule v1 gives (or would give) an archive of it, or to a folder
//! holding one answers 409 (`crate::methods`); reads still reach the winner.
//!
//! The per-user names at the reader root (`history`, `goals.json`, ...) are reserved in
//! every spelling: a request for `/mokuro-reader/History/x` is rewritten to the canonical
//! `history/x`, the caller's own tree, and never resolves into the library.
//!
//! Renaming to fix the case stays possible, as it is on NTFS: a MOVE whose destination is
//! a case-variant of its own source keeps the destination's spelling (rewriting it to the
//! source's would make it a move onto itself).

use std::path::{Path, PathBuf};

use bunko_core::fold_groups::{FoldGroups, Frozen};
use bunko_core::reader_tree;
use bunko_store::fs;
use http::{Uri, request::Parts};

use crate::paths::{self, READER_ROOT};

pub use bunko_store::fs::same_file;

/// The spelling-insensitive form two names collide under: the one filename fold
/// ([`bunko_core::names::fold_name`]): canonical normalization (a decomposed and a
/// composed spelling are one name, as on APFS) and full Unicode case folding (`ß` and
/// `ss`, final sigma, the Kelvin and long-s signs, as Linux casefold has them), so Bunko
/// joins every pair of names any host it runs on joins.
pub fn fold_name(name: &str) -> String {
    bunko_core::names::fold_name(name)
}

/// `parent`'s listed UTF-8 names with whether each is a folder (a name that is not
/// UTF-8 can never be what a UTF-8 request spells). At the library root (`top`) the
/// per-user names (`history`, `Goals.json`, any spelling) are left out: they are never
/// library entries. `None` when `parent` cannot be listed.
fn listing(parent: &Path, top: bool) -> Option<Vec<(String, bool)>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(parent).ok()?.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if top && reader_tree::is_per_user_root_name(&name) {
            continue;
        }
        let dir = entry.file_type().is_ok_and(|t| t.is_dir());
        out.push((name, dir));
    }
    Some(out)
}

/// The fold groups of `parent`'s listing `entries`
/// ([`bunko_core::fold_groups::FoldGroups`]): which names no request reaches (every
/// spelling of a name but the one [`on_disk_name`] picks, and every file rule v1 gives
/// to a shadowed archive) and which the freeze holds (no write touches them until the
/// case variants are merged by hand).
fn groups_in(parent: &Path, entries: &[(String, bool)]) -> FoldGroups {
    let store = bunko_store::store_for(parent);
    FoldGroups::new(entries.iter().map(|(n, d)| (n.as_str(), *d)), |fold| {
        store.as_ref()?.case_winner(parent, fold)
    })
}

/// The names of `entries` (`parent`'s listing) no request reaches
/// ([`FoldGroups::hidden`]). No listing shows them, no request reads, writes or deletes
/// them, and no new name is made beside them.
fn hidden_in(parent: &Path, entries: &[(String, bool)]) -> std::collections::HashSet<String> {
    groups_in(parent, entries).hidden().clone()
}

/// Where the library path `rel` (below `library`, as PathCase resolved it) meets the
/// freeze ([`bunko_core::fold_groups::frozen_along`]): a segment that is a member of a
/// case-variant group, or a file rule v1 gives (or would give, once written) to an
/// archive such a group freezes, in any folder on the way. Every write to it answers
/// 409. From the listings alone, so every host answers alike.
pub(crate) fn frozen(library: &Path, rel: &str) -> Option<Frozen> {
    bunko_core::fold_groups::frozen_along(rel, false, |folder| {
        let dir = if folder.is_empty() {
            library.to_path_buf()
        } else {
            library.join(folder)
        };
        let entries = listing(&dir, folder.is_empty())?;
        Some(groups_in(&dir, &entries))
    })
}

/// The one listed spelling `name` resolves to in `parent`, or `None` when nothing there
/// that a request may reach folds like it.
///
/// Every spelling, the exact one included, resolves to a single winner: when two or more
/// listed names fold alike (only a case-sensitive disk or a remote store can hold them),
/// the store's [`case_winner`] when it is listed (a remote store's oldest spelling, the
/// one its case merge keeps), else the byte-smallest. The other spellings, and the files
/// rule v1 gives to a shadowed archive ([`hidden_in`]), are never addressed. The listing
/// is always scanned: no probe, no exact fast path.
///
/// At the library root (`top`) a name that folds to a per-user name (`History`,
/// `Goals.json`) is never a library entry.
///
/// [`case_winner`]: bunko_store::LibraryStore::case_winner
pub(crate) fn on_disk_name(parent: &Path, name: &str, top: bool) -> Option<String> {
    if top && reader_tree::is_per_user_root_name(name) {
        return None;
    }
    let entries = listing(parent, top)?;
    reachable_in(parent, &entries, name)
}

/// [`on_disk_name`] over `entries`, `parent`'s [`listing`].
fn reachable_in(parent: &Path, entries: &[(String, bool)], name: &str) -> Option<String> {
    let target = fold_name(name);
    let variants: Vec<&str> = entries
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| *n == name || fold_name(n) == target)
        .collect();
    if variants.is_empty() {
        return None;
    }
    let hidden = hidden_in(parent, entries);
    variants
        .into_iter()
        .filter(|n| !hidden.contains(*n))
        .min()
        .map(str::to_string)
}

/// Does `parent` list `name` in another spelling than the reachable `name` itself: some
/// listed entry folds like it (a hidden one included), and `name` is not the entry a
/// request for it reaches? Then nothing may be created under `name` there, and nothing
/// spelled `name` is read, written or deleted: it would be a second spelling of an
/// existing name, or a hidden entry's. From the listing alone, so every host answers
/// alike. False when `parent` cannot be listed (or is gone: the write fails there).
pub(crate) fn spelled_otherwise(parent: &Path, name: &str, top: bool) -> bool {
    if top && reader_tree::is_per_user_root_name(name) {
        return false;
    }
    let Some(entries) = listing(parent, top) else {
        return false;
    };
    let target = fold_name(name);
    let any = entries
        .iter()
        .any(|(n, _)| n == name || fold_name(n) == target);
    any && reachable_in(parent, &entries, name).as_deref() != Some(name)
}

/// Is every segment of `relative` (below `base`) listed in its folder exactly as spelled,
/// as the entry a request for it reaches ([`on_disk_name`])? Whether a library path
/// exists, decided from the listings alone: a host that would open another spelling
/// (APFS, NTFS, casefold) answers as a case-sensitive one and a remote store do.
/// `top_reserved`: `base` is the library root. A `.` segment stays in its folder; a
/// `..` segment never is.
pub(crate) fn reachable_as_spelled(base: &Path, relative: &str, top_reserved: bool) -> bool {
    let mut parent = base.to_path_buf();
    for (position, segment) in relative
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .enumerate()
    {
        if segment == ".." {
            return false;
        }
        if on_disk_name(&parent, segment, top_reserved && position == 0).as_deref() != Some(segment)
        {
            return false;
        }
        parent.push(segment);
    }
    true
}

/// The names listed in `parent` (`entries`: name and whether it is a folder) that no
/// request reaches ([`hidden_in`]): no listing shows them, nothing is copied from or
/// over them, nothing deletes them.
pub(crate) fn hidden(
    parent: &Path,
    entries: &[(String, bool)],
) -> std::collections::HashSet<String> {
    hidden_in(parent, entries)
}

/// `parent`'s listing (UTF-8 names, each with whether it is a folder; at the library
/// root, `top`, without the per-user names) and its fold groups. `None` when it cannot
/// be listed.
pub(crate) fn listed_groups(parent: &Path, top: bool) -> Option<(Vec<(String, bool)>, FoldGroups)> {
    let entries = listing(parent, top)?;
    let groups = groups_in(parent, &entries);
    Some((entries, groups))
}

/// `parent`'s listing (UTF-8 names, each with whether it is a folder) and its
/// [`hidden`] names. Empty when it cannot be listed.
pub(crate) fn listed_with_hidden(
    parent: &Path,
) -> (Vec<(String, bool)>, std::collections::HashSet<String>) {
    let entries = listing(parent, false).unwrap_or_default();
    let hidden = hidden_in(parent, &entries);
    (entries, hidden)
}

/// The first case-variant group in the folder `dir` or any folder below it (symlinked
/// folders not followed), with the folder that lists it. A DELETE, MOVE or COPY of the
/// folder whole, or onto it, is refused while it holds one: the group is frozen.
pub(crate) fn frozen_inside(dir: &Path) -> Option<(PathBuf, bunko_core::fold_groups::Group)> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Some(entries) = listing(&d, false) else {
            continue;
        };
        if let Some(group) = groups_in(&d, &entries).groups().first() {
            return Some((d, group.clone()));
        }
        for (name, is_dir) in entries {
            let child = d.join(&name);
            if is_dir && !fs::symlink_metadata(&child).is_ok_and(|m| m.file_type().is_symlink()) {
                stack.push(child);
            }
        }
    }
    None
}

/// `relative` (below `base`) with every existing segment in its listed spelling
/// ([`on_disk_name`]); segments past the first one that names nothing keep the requested
/// spelling. `top_reserved`: `base` is the library root, where per-user names are never
/// library entries. A `.` segment is carried through (it stays in its folder); a path
/// with a `..` segment is returned unchanged; empty segments are carried through.
pub(crate) fn resolve_existing(base: &Path, relative: &str, top_reserved: bool) -> String {
    resolve(base, relative, None, top_reserved)
}

/// The library entry `relative` (a stored path, below the library root `library`) leads
/// to through the listings, in its listed spelling: each segment the winner of the names
/// that fold like it ([`on_disk_name`]). `None` when a segment names nothing, or is
/// spelled exactly like a listed entry no request reaches (the path is that hidden
/// entry's, never the winner's). No probe: the same on every host.
pub(crate) fn listed_entry(library: &Path, relative: &str) -> Option<String> {
    let mut parent = library.to_path_buf();
    let mut out: Vec<String> = Vec::new();
    for (position, segment) in relative.split('/').filter(|s| !s.is_empty()).enumerate() {
        if segment == "." || segment == ".." {
            return None;
        }
        let top = position == 0;
        if top && reader_tree::is_per_user_root_name(segment) {
            return None;
        }
        let entries = listing(&parent, top)?;
        let winner = reachable_in(&parent, &entries, segment)?;
        if winner != segment && entries.iter().any(|(n, _)| n == segment) {
            return None;
        }
        parent.push(&winner);
        out.push(winner);
    }
    Some(out.join("/"))
}

/// Is `requested` a personal or curated layer name (`V.edit.mokuro`, `V.curated.mokuro`,
/// exact as rule v1 reads ids: [`sidecar_names::reserved_layer_of`]) that the listed
/// entry `listed` of `parent`, one name with it by the fold, is not
/// (`V.ed\u{200B}it.mokuro`, `V.EDIT.mokuro`, made by hand or before 0.8)? Then a
/// request for the layer keeps its own spelling: it is an account's own correction or
/// the shared layer, never that entry (whose bytes another account wrote). From the
/// listing.
///
/// [`sidecar_names::reserved_layer_of`]: bunko_core::sidecar_names::reserved_layer_of
fn layer_spelled_otherwise(parent: &Path, requested: &str, listed: &str) -> bool {
    use bunko_core::sidecar_names::{LayerClass, layer_class, reserved_layer_of, split_layer};
    if !split_layer(requested).is_some_and(|(_, id, _)| layer_class(id) != LayerClass::Other) {
        return false;
    }
    let files = crate::personal::names_in(parent);
    let refs: Vec<&str> = files.iter().map(String::as_str).collect();
    let class = reserved_layer_of(requested, &refs);
    class.is_some() && reserved_layer_of(listed, &refs) != class
}

fn resolve(base: &Path, relative: &str, keep: Option<&str>, top_reserved: bool) -> String {
    let mut parts: Vec<String> = relative.split('/').map(str::to_string).collect();
    if parts.iter().any(|p| p == "..") {
        return relative.to_string();
    }
    // A `.` segment stays in its folder, as every handler resolves it: carried through
    // untouched, the segments around it resolved as if it were not there.
    let named: Vec<usize> = (0..parts.len())
        .filter(|&i| !parts[i].is_empty() && parts[i] != ".")
        .collect();
    let keep = keep.map(|k| k.trim_matches('/'));
    let mut parent = base.to_path_buf();
    let mut resolved: Vec<String> = Vec::new();
    for (position, &index) in named.iter().enumerate() {
        let part = parts[index].clone();
        let Some(on_disk) = on_disk_name(&parent, &part, top_reserved && position == 0) else {
            break;
        };
        let is_last = position + 1 == named.len();
        if is_last && on_disk != part && layer_spelled_otherwise(&parent, &part, &on_disk) {
            break;
        }
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
    /// spelling is kept. A `.` segment is carried through, the segments around it
    /// resolved; a path with a `..` segment is returned unchanged (refused downstream).
    /// Empty segments (a trailing or doubled slash) are carried through untouched so
    /// the request keeps its exact shape.
    pub fn canonicalize(&self, relative: &str, keep_last_if_variant_of: Option<&str>) -> String {
        resolve(&self.library, relative, keep_last_if_variant_of, true)
    }

    /// Is every segment of `relative` listed exactly so, as the entry a request reaches
    /// ([`reachable_as_spelled`])? What [`Self::canonicalize`] returns is, unless a
    /// segment names nothing or only a hidden entry (a case variant, or a hidden
    /// archive's own file): those are never served. From the listings alone.
    pub fn reaches(&self, relative: &str) -> bool {
        reachable_as_spelled(&self.library, relative, true)
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

/// `path` with its per-user first segment in the canonical spelling, when it is a
/// per-user path under the reader root spelled otherwise; `None` for anything else.
fn per_user_rewrite(path: &str) -> Option<String> {
    let relative = path.strip_prefix(LIBRARY_PREFIX)?;
    let canonical = reader_tree::canonical_per_user_rel(relative)?;
    (canonical != relative).then(|| format!("{LIBRARY_PREFIX}{canonical}"))
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
    /// path). A per-user name at the reader root spelled in another case (`History/`,
    /// `Goals.json`) takes its canonical spelling, so every layer, auth included, sees
    /// the caller's own entry. Blocking.
    pub fn canonical_path(&self, path: &str, keep_last_if_variant_of: Option<&str>) -> String {
        if let Some(per_user) = per_user_rewrite(path) {
            return per_user;
        }
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
            .is_some_and(|p| library_relative(&p).is_some() || per_user_rewrite(&p).is_some());
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
        // Per-user in any case.
        assert_eq!(library_relative("/mokuro-reader/Volume-Data.json"), None);
        assert_eq!(library_relative("/mokuro-reader/History/x"), None);
        assert_eq!(
            per_user_rewrite("/mokuro-reader/Volume-Data.json").as_deref(),
            Some("/mokuro-reader/volume-data.json")
        );
        assert_eq!(per_user_rewrite("/mokuro-reader/volume-data.json"), None);
        assert_eq!(per_user_rewrite("/mokuro-reader/S/History"), None);
        assert_eq!(library_relative("/mokuro-reader"), None);
        assert_eq!(library_relative("/inbox/x"), None);
    }
}
