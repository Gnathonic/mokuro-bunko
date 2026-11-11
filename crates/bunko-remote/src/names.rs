//! Names and effective mtimes (spec §5.2, D3, D5).

use std::collections::HashSet;

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::backend::RNode;

/// Provider name bytes as an index name: lossy UTF-8, then NFC.
pub fn nfc(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).nfc().collect()
}

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

/// The spelling-insensitive form two names collide under: the one filename fold (full
/// Unicode case folding and canonical normalization, [`bunko_core::names::fold_name`],
/// the same rule as bunko-dav's `fold_name`).
pub use bunko_core::names::fold_name as fold;

/// A path-id provider's pid (a `/`-separated path, WebDAV) as a server that folds
/// spelling would compare it: every segment under [`fold`]. Two pids that fold alike
/// may be one resource there (the old and the new spelling of a case-only rename).
pub fn fold_path_pid(pid: &str) -> String {
    pid.split('/').map(fold).collect::<Vec<_>>().join("/")
}

/// `H(pid‖cver)`: the first 8 bytes (big-endian) of `sha256(pid ‖ 0x00 ‖ cver)`.
pub fn version_salt(pid: &str, cver: &str) -> u64 {
    let mut h = Sha256::new();
    h.update(pid.as_bytes());
    h.update([0u8]);
    h.update(cver.as_bytes());
    let d = h.finalize();
    u64::from_be_bytes(d[..8].try_into().expect("sha256 is 32 bytes"))
}

/// The effective mtime (D3): `trunc_G(base) + (H(pid‖cver) mod G)`, in ns.
///
/// The salt stays below G, so the listed timestamp truncated to G (and so the reader's
/// `int(mtime)`) is unchanged, while two versions within one G still differ.
pub fn effective_mtime_ns(base_ms: i64, granularity_ns: i64, pid: &str, cver: &str) -> i128 {
    let g = i128::from(granularity_ns.max(1));
    let base = i128::from(base_ms) * 1_000_000;
    let trunc = base - base.rem_euclid(g);
    trunc + i128::from(version_salt(pid, cver)) % g
}

/// Order root candidates canonically (D5, the reader's rule): a root whose parent is
/// the account's top folder `top` first, then the oldest created, then the smallest
/// pid (byte order). The first is the write root. Duplicate pids are dropped.
pub fn sort_roots_canonical(v: &mut Vec<RNode>, top: Option<&str>) {
    let below_top = |n: &RNode| u8::from(top.is_none_or(|t| n.parent.as_deref() != Some(t)));
    v.sort_by(|a, b| {
        (below_top(a), a.created_ms, &a.pid).cmp(&(below_top(b), b.created_ms, &b.pid))
    });
    let mut seen = HashSet::new();
    v.retain(|n| seen.insert(n.pid.clone()));
}

/// Per-user files a duplicate-folder merge never trashes: both copies move into the
/// canonical folder (their contents merge by key, so neither copy is a loser).
/// Compared folded.
pub const KEEP_BOTH_FOLDED: [&str; 3] = ["volume-data.json", "goals.json", "positions.json"];

/// Bunko's status files: kept by a merge like [`KEEP_BOTH_FOLDED`], but by exact name
/// (Bunko writes only this spelling).
pub const CONTROL_FILES: [&str; 2] = [".mokuro-bunko.json", ".mokuro-queue.json"];

/// A series' OCR queue file (queue file v2): the lease holder's status writer
/// regenerates it, so a duplicate-folder merge keeps the newer copy and trashes the
/// other (it is not kept twice).
pub use bunko_core::names::SERIES_QUEUE_FILE;

/// The leftover root queue file of a 0.8 pre-release (queue file v2 §3): never written
/// any more; the lease holder deletes it.
pub const ROOT_QUEUE_FILE: &str = ".mokuro-queue.json";

/// `rel` names a series' queue file: two segments or more, the last folding to
/// [`SERIES_QUEUE_FILE`].
pub fn is_series_queue_path(rel: &crate::index::RelPath) -> bool {
    let segs = rel.segments();
    segs.len() >= 2
        && segs
            .last()
            .is_some_and(|n| bunko_core::names::is_fixed_name(n, SERIES_QUEUE_FILE))
}

/// A merge moves every copy of `name` in, never trashing one (same-name collision
/// rule). History `.events` files and `profiles.json` are not kept twice: the newer
/// write wins, as the reader's own import does.
pub fn dedup_keeps_both(name: &str) -> bool {
    KEEP_BOTH_FOLDED.contains(&fold(name).as_str()) || CONTROL_FILES.contains(&name)
}

/// The local folder driver's in-folder trash (D16).
pub const FOLDER_TRASH_DIR: &str = ".mokuro-bunko-trash";

/// The suffix every file the local folder driver trashes gets, so no reader lists it.
pub const TRASHED_SUFFIX: &str = ".bunko-trashed";

/// A name Bunko reserves on the provider for its own transient files: the WebDAV
/// driver's staged uploads (`.<name>.bunko-<op>.tmp`), the folder driver's
/// (`.bunko-<op>.tmp`) and its case-rename stops (`.bunko-case-*`), probe leftovers
/// (`.bunko-probe-*`), and the folder driver's trash (`.mokuro-bunko-trash`, and
/// `*.bunko-trashed` files). Compared folded, because a case-insensitive provider
/// resolves any spelling to the same node. The store never lists one, and its shim
/// never creates, moves or removes one.
pub fn is_reserved_name(seg: &str) -> bool {
    let s = fold(seg);
    s.starts_with(".bunko-probe-")
        || s.starts_with(".bunko-case-")
        || (s.starts_with('.') && s.ends_with(".tmp") && s.contains(".bunko-"))
        || s == FOLDER_TRASH_DIR
        || s.ends_with(TRASHED_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(pid: &str, parent: &str, created_ms: i64) -> RNode {
        RNode {
            pid: pid.into(),
            parent: Some(parent.into()),
            name_raw: b"mokuro-reader".to_vec(),
            is_dir: true,
            size: 0,
            base_mtime_ms: 0,
            created_ms,
            version: String::new(),
            cver: String::new(),
            pin: None,
            hash: None,
            trashed: false,
        }
    }

    fn pids(v: &[RNode]) -> Vec<&str> {
        v.iter().map(|n| n.pid.as_str()).collect()
    }

    #[test]
    fn sort_roots_canonical_puts_top_level_first_then_oldest_then_smallest_pid() {
        let mut v = vec![
            root("nested-old", "some-folder", 1),
            root("top-new", "top", 30),
            root("top-b", "top", 20),
            root("top-a", "top", 20),
            root("top-new", "top", 30),
        ];
        sort_roots_canonical(&mut v, Some("top"));
        assert_eq!(pids(&v), ["top-a", "top-b", "top-new", "nested-old"]);
        // The account top unknown: age, then pid.
        sort_roots_canonical(&mut v, None);
        assert_eq!(pids(&v), ["nested-old", "top-a", "top-b", "top-new"]);
    }

    #[test]
    fn reserved_names_cover_every_driver_internal_name() {
        for n in [
            ".bunko-probe-x1",
            ".Vol 1.cbz.bunko-17.tmp",
            ".bunko-0123456789abcdef.tmp",
            ".bunko-case-77.tmp",
            ".BUNKO-CASE-77",
            ".mokuro-bunko-trash",
            ".Mokuro-Bunko-Trash",
            "Vol 1.cbz.bunko-trashed",
            "catalog.json.BUNKO-TRASHED",
        ] {
            assert!(is_reserved_name(n), "{n}");
        }
        for n in [
            "Vol 1.cbz",
            ".mokuro-bunko.json",
            ".mokuro-queue.json",
            "bunko-case.txt",
            ".mokuro-bunko-trash.txt",
            "x.bunko-trashed.cbz",
            "a.tmp",
        ] {
            assert!(!is_reserved_name(n), "{n}");
        }
    }

    #[test]
    fn dedup_keeps_both_folds_the_user_files_and_matches_control_files_exactly() {
        for n in ["volume-data.json", "Goals.JSON", "POSITIONS.json"] {
            assert!(dedup_keeps_both(n), "{n}");
        }
        for n in [".mokuro-bunko.json", ".mokuro-queue.json"] {
            assert!(dedup_keeps_both(n), "{n}");
        }
        for n in [
            ".Mokuro-Bunko.json",
            ".mokuro-queue.JSON",
            "profiles.json",
            "series.json",
            "2026-10.events",
            "volume-data.json.bak",
        ] {
            assert!(!dedup_keeps_both(n), "{n}");
        }
    }
}
