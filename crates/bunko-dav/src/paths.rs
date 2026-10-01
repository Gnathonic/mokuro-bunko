//! Virtual DAV paths: decoding, classification, physical mapping, hrefs.
//!
//! The DAV tree (0.5.2 `PathMapper`, spec §8.1):
//!
//! | virtual | physical |
//! |---|---|
//! | `/` | none (virtual, one member `mokuro-reader`) |
//! | `/mokuro-reader` | none (virtual merged view) |
//! | `/mokuro-reader/{volume-data,profiles,goals}.json` | `users/<username>/<name>` |
//! | `/mokuro-reader/<rel>` | `library/<rel>` (resolved, must stay inside) |
//! | anything else (incl. `/inbox`) | nothing |

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

pub const READER_ROOT: &str = "mokuro-reader";

/// Root `.json` files that belong to ONE user (exact, case-sensitive names), in the sorted
/// order 0.5.2 lists them.
pub const PER_USER_FILES: [&str; 3] = ["goals.json", "profiles.json", "volume-data.json"];

pub fn is_per_user_name(name: &str) -> bool {
    PER_USER_FILES.contains(&name)
}

/// Percent-decode a request path to UTF-8 (spec §3.1). `None` for invalid UTF-8 or a NUL
/// byte (0.5.2 answered a bare 500; 400 here).
pub fn decode_request_path(raw: &str) -> Option<String> {
    let bytes: Vec<u8> = percent_decode_str(raw).collect();
    if bytes.contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// `"/" + path.strip("/")`, the normalisation every 0.5.2 path helper applies.
pub fn normalize(path: &str) -> String {
    let trimmed = path.trim_matches('/');
    let mut out = String::with_capacity(trimmed.len() + 1);
    out.push('/');
    out.push_str(trimmed);
    out
}

/// What a (normalised) virtual path is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Root,
    ReaderRoot,
    /// `/mokuro-reader/<per-user name>`
    Progress(&'static str),
    /// `/mokuro-reader/<rel>` (rel non-empty, not a per-user name)
    Library(String),
    /// `/inbox` and below: mapped by 0.5.2 but never served.
    Inbox,
    Other,
}

/// The class of a path for MOVE/COPY (0.5.2 `PathMapper.get_path_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathClass {
    Root,
    ReaderRoot,
    Progress,
    Library,
    Inbox,
    Unknown,
}

pub fn classify(path: &str) -> Target {
    let norm = normalize(path);
    if norm == "/" {
        return Target::Root;
    }
    let reader = format!("/{READER_ROOT}");
    if norm == reader {
        return Target::ReaderRoot;
    }
    if let Some(rel) = norm.strip_prefix(&format!("{reader}/")) {
        if let Some(name) = PER_USER_FILES.iter().find(|n| **n == rel) {
            return Target::Progress(name);
        }
        return Target::Library(rel.to_string());
    }
    if norm == "/inbox" || norm.starts_with("/inbox/") {
        return Target::Inbox;
    }
    Target::Other
}

pub fn class_of(path: &str) -> PathClass {
    match classify(path) {
        Target::Root => PathClass::Root,
        Target::ReaderRoot => PathClass::ReaderRoot,
        Target::Progress(_) => PathClass::Progress,
        Target::Library(_) => PathClass::Library,
        Target::Inbox => PathClass::Inbox,
        Target::Other => PathClass::Unknown,
    }
}

/// WsgiDAV `get_uri_parent`: `None` for the root, else the parent with a trailing `/`.
pub fn uri_parent(path: &str) -> Option<String> {
    if path.trim().is_empty() || path.trim() == "/" {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    let cut = trimmed.rfind('/')?;
    Some(format!("{}/", &trimmed[..cut]))
}

/// WsgiDAV `get_uri_name`: the last segment.
pub fn uri_name(path: &str) -> &str {
    path.trim_matches('/').rsplit('/').next().unwrap_or("")
}

/// WsgiDAV `join_uri`.
pub fn join_uri(base: &str, name: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        name.trim_start_matches('/')
    )
}

/// WsgiDAV `is_equal_or_child_uri(parent, child)` on normalised paths.
pub fn is_equal_or_child(parent: &str, child: &str) -> bool {
    let p = format!("{}/", parent.trim_end_matches('/'));
    let c = format!("{}/", child.trim_end_matches('/'));
    c.starts_with(&p)
}

/// WsgiDAV href encoding: `quote(path, safe="/!*'(),$-_|.")` (plus Python's always-safe
/// `~`).
const HREF_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'!')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b',')
    .remove(b'$')
    .remove(b'-')
    .remove(b'_')
    .remove(b'|')
    .remove(b'.')
    .remove(b'~');

pub fn href(path: &str, collection: bool) -> String {
    let mut p = utf8_percent_encode(path, HREF_ESCAPE).to_string();
    if collection && !p.ends_with('/') {
        p.push('/');
    }
    p
}

/// `quote(s, safe="/")` (nginx X-Accel path, spec §8.10).
const ACCEL_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

pub fn quote_path(s: &str) -> String {
    utf8_percent_encode(s, ACCEL_ESCAPE).to_string()
}

/// Python `PurePath.suffix` (3.12): the last `.xxx` of the name, empty for dotfiles and a
/// trailing dot.
pub fn py_suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    }
}

/// Python `PurePath.stem`.
pub fn py_stem(name: &str) -> &str {
    let suffix = py_suffix(name);
    &name[..name.len() - suffix.len()]
}

/// A path's file name as UTF-8 (lossy only for names the request could never address).
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Resolve `rel` under the canonical directory `base`, following symlinks like Python's
/// `Path.resolve()` (realpath, non-strict), and require the result to stay inside `base`
/// (0.5.2 `safe_resolve_under`, spec §3.3). `base` must already be canonical.
pub fn resolve_under(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut pending: VecDeque<OsString> = split_rel(rel).into_iter().map(OsString::from).collect();
    let mut cur = base.to_path_buf();
    let mut links = 0u32;
    let mut probing = true;
    while let Some(comp) = pending.pop_front() {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            // After a missing component the rest is only joined lexically, so a `..`
            // there could step back out through a symlink the OS would follow
            // (`nope/../link/x`). Python's realpath resolved it; refuse instead.
            if !probing {
                return None;
            }
            cur.pop();
            continue;
        }
        cur.push(&comp);
        if !probing {
            continue;
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) if meta.file_type().is_symlink() => {
                links += 1;
                if links > 40 {
                    return None;
                }
                let target = std::fs::read_link(&cur).ok()?;
                cur.pop();
                let mut front: Vec<OsString> = Vec::new();
                if target.is_absolute() {
                    cur = PathBuf::new();
                }
                for c in target.components() {
                    match c {
                        Component::Prefix(_) | Component::RootDir => cur.push(c.as_os_str()),
                        Component::CurDir => {}
                        Component::ParentDir => front.push(OsString::from("..")),
                        Component::Normal(n) => front.push(n.to_os_string()),
                    }
                }
                for c in front.into_iter().rev() {
                    pending.push_front(c);
                }
            }
            Ok(_) => {}
            // Python keeps going lexically after the first missing component.
            Err(_) => probing = false,
        }
    }
    cur.starts_with(base).then_some(cur)
}

fn split_rel(rel: &str) -> Vec<&str> {
    #[cfg(windows)]
    {
        rel.split(['/', '\\']).collect()
    }
    #[cfg(not(windows))]
    {
        rel.split('/').collect()
    }
}

/// `rel` of `path` under `base` with `/` separators, or `None` when outside.
pub fn relative_posix(path: &Path, base: &Path) -> Option<String> {
    let rel = path.strip_prefix(base).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    Some(parts.join("/"))
}

/// A username that can be a single directory name under `users/` (0.5.2 additionally
/// resolves and checks containment; usernames are `^[a-zA-Z0-9_-]{3,32}$` anyway).
pub fn safe_username(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

/// 0.5.2 compiled-metadata predicate (`metadata/paths.py`): `catalog.json` at the library
/// root or `<one folder>/series.json`, after lexical normalisation.
pub fn is_compiled_metadata_path(path: &str) -> bool {
    let norm = lexical_normpath(path);
    let Some(rel) = norm.strip_prefix(&format!("/{READER_ROOT}/")) else {
        return false;
    };
    if rel.is_empty() || is_per_user_name(rel) {
        return false;
    }
    if rel.eq_ignore_ascii_case("catalog.json") {
        return true;
    }
    let parts: Vec<&str> = rel.split('/').collect();
    parts.len() == 2 && !parts[0].trim().is_empty() && parts[1].eq_ignore_ascii_case("series.json")
}

/// `posixpath.normpath("/" + p.strip("/"))`.
fn lexical_normpath(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// The staging files of in-flight uploads (`.<name>.upload-<random>.tmp`); hidden from
/// listings (spec Q3).
pub fn is_staging_name(name: &str) -> bool {
    name.starts_with('.') && name.ends_with(".tmp") && name.contains(".upload-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_paths() {
        assert_eq!(classify("/"), Target::Root);
        assert_eq!(classify(""), Target::Root);
        assert_eq!(classify("/mokuro-reader/"), Target::ReaderRoot);
        assert_eq!(
            classify("/mokuro-reader/goals.json"),
            Target::Progress("goals.json")
        );
        assert_eq!(
            classify("/mokuro-reader/Goals.json"),
            Target::Library("Goals.json".into())
        );
        assert_eq!(
            classify("/mokuro-reader/S/v.cbz/"),
            Target::Library("S/v.cbz".into())
        );
        assert_eq!(classify("/inbox/x"), Target::Inbox);
        assert_eq!(classify("/foo"), Target::Other);
    }

    #[test]
    fn hrefs_and_suffixes() {
        assert_eq!(
            href("/mokuro-reader/Series Ω/Vol #1.cbz", false),
            "/mokuro-reader/Series%20%CE%A9/Vol%20%231.cbz"
        );
        assert_eq!(href("/mokuro-reader", true), "/mokuro-reader/");
        assert_eq!(href("/", true), "/");
        assert_eq!(
            quote_path("Series Ω/Vol #1.cbz"),
            "Series%20%CE%A9/Vol%20%231.cbz"
        );
        assert_eq!(py_suffix("a.tar.gz"), ".gz");
        assert_eq!(py_suffix(".hidden"), "");
        assert_eq!(py_suffix("a."), "");
        assert_eq!(py_stem("Vol 1.cbz"), "Vol 1");
        assert_eq!(
            uri_parent("/mokuro-reader/x.cbz"),
            Some("/mokuro-reader/".into())
        );
        assert_eq!(uri_parent("/mokuro-reader/"), Some("/".into()));
        assert_eq!(uri_parent("/"), None);
    }

    #[test]
    fn compiled_paths() {
        assert!(is_compiled_metadata_path("/mokuro-reader/catalog.json"));
        assert!(is_compiled_metadata_path("/mokuro-reader//Catalog.JSON"));
        assert!(is_compiled_metadata_path("/mokuro-reader/S/series.json"));
        assert!(is_compiled_metadata_path(
            "/mokuro-reader/x/../S/./series.json"
        ));
        assert!(!is_compiled_metadata_path("/mokuro-reader/S/T/series.json"));
        assert!(!is_compiled_metadata_path("/mokuro-reader/S/catalog.json"));
        assert!(!is_compiled_metadata_path(
            "/mokuro-reader/volume-data.json"
        ));
    }

    #[test]
    fn resolve_contains() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("lib")).unwrap();
        let lib = base.join("lib");
        std::fs::create_dir(lib.join("S")).unwrap();
        assert_eq!(resolve_under(&lib, "S/x.cbz"), Some(lib.join("S/x.cbz")));
        assert_eq!(resolve_under(&lib, "../escape"), None);
        assert_eq!(resolve_under(&lib, "S/../../lib/S"), Some(lib.join("S")));
        assert_eq!(resolve_under(&lib, ""), Some(lib.clone()));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&base, lib.join("out")).unwrap();
            assert_eq!(resolve_under(&lib, "out/lib/S"), Some(lib.join("S")));
            assert_eq!(resolve_under(&lib, "out"), None);
            std::os::unix::fs::symlink("S", lib.join("inside")).unwrap();
            assert_eq!(
                resolve_under(&lib, "inside/v.cbz"),
                Some(lib.join("S/v.cbz"))
            );
        }
    }
}
