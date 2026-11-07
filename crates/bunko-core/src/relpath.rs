//! Library paths: [`RelPath`], NFC segments under a library root, each one a name
//! that can be exactly one path segment on every host ([`is_safe_segment`]).
//!
//! Store-agnostic: the remote store's index addresses its entries with it, and the
//! shared case-merge planner plans in it.

use std::fmt;

/// A provider name that can be one library path segment. Provider names are
/// untrusted (Drive and MEGA allow `..`, `a/b` or control characters), so the empty
/// name, `.`, `..`, anything holding `/`, `\`, `:` (Windows drive prefixes and NTFS
/// streams) or NUL, any other ASCII control character, and anything that does not parse
/// as exactly one normal path component are refused. Lossy-UTF-8 replacement characters are allowed: the raw
/// bytes stay in `node.name_raw` for addressing.
pub fn is_safe_segment(name: &str) -> bool {
    use std::path::{Component, Path};
    if name.is_empty()
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c == ':' || c.is_ascii_control())
    {
        return false;
    }
    if cfg!(windows) && is_windows_device_name(name) {
        return false;
    }
    // Exactly one normal component on this platform: no drive or UNC prefix (Windows
    // `C:x` would replace the base on `join`), no root, nothing that parses as more.
    let mut parts = Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(part)), None) if part == std::ffi::OsStr::new(name)
    )
}

/// `CON`, `PRN`, `AUX`, `NUL`, `COM0`-`COM9`, `LPT0`-`LPT9`, `CONIN$`, `CONOUT$`, with
/// or without an extension and trailing dots or spaces, in any case. On Windows a path
/// ending in one of these opens a device, so a remote entry with such a name is
/// refused there. Names NTFS merely forbids (`<>"|?*`, a trailing dot) are allowed:
/// they cannot alias another path, the remote store never writes a file to the local
/// disk under its cloud name, and `?` is common in real titles.
pub fn is_windows_device_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end_matches([' ', '.']);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (upper.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.as_bytes()[3].is_ascii_digit())
}

/// A library path under the merged root: NFC segments, `[]` for the root itself.
///
/// Every segment passes [`is_safe_segment`] (no `..`, `.`, separators or control
/// characters): the constructors refuse anything else, so a `RelPath` can always be
/// joined under the store root without aliasing or escaping it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(Vec<String>);

impl RelPath {
    /// The merged root.
    pub fn root() -> RelPath {
        RelPath(Vec::new())
    }

    /// Split a `/`-separated path; empty segments (doubled or edge slashes) are
    /// dropped. `None` when any segment is unsafe (`..`, `.`, `\`, control chars).
    pub fn parse(s: &str) -> Option<RelPath> {
        let mut p = RelPath::root();
        for seg in s.split('/').filter(|x| !x.is_empty()) {
            p.push_checked(seg).ok()?;
        }
        Some(p)
    }

    /// A path from segments; `None` when any segment is unsafe.
    pub fn from_segments<I, S>(segs: I) -> Option<RelPath>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut p = RelPath::root();
        for seg in segs {
            p.push_checked(seg).ok()?;
        }
        Some(p)
    }

    /// Append one segment if it is safe; otherwise hand it back unchanged.
    pub fn push_checked(&mut self, seg: impl Into<String>) -> Result<(), String> {
        let seg = seg.into();
        if is_safe_segment(&seg) {
            self.0.push(seg);
            Ok(())
        } else {
            Err(seg)
        }
    }

    /// This path with one more segment, if it is safe.
    pub fn join(&self, seg: impl Into<String>) -> Option<RelPath> {
        let mut p = self.clone();
        p.push_checked(seg).ok()?;
        Some(p)
    }

    pub fn segments(&self) -> &[String] {
        &self.0
    }

    pub fn into_segments(self) -> Vec<String> {
        self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}
