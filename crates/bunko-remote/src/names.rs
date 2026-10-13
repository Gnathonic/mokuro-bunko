//! Names and effective mtimes (spec §5.2, D3, D5).

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

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

/// The spelling-insensitive form two names collide under: NFC, then a full Unicode
/// lowercase (the same rule as bunko-dav's `fold_name`).
pub fn fold(name: &str) -> String {
    name.nfc().collect::<String>().to_lowercase()
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

/// A name Bunko reserves on the provider for its own transient files: the WebDAV
/// driver's staged uploads (`.<name>.bunko-<op>.tmp`) and its probe leftovers
/// (`.bunko-probe-*`). Compared folded, because a case-insensitive provider resolves
/// any spelling to the same node. The store never lists one, and its shim never
/// creates, moves or removes one.
pub fn is_reserved_name(seg: &str) -> bool {
    let s = fold(seg);
    s.starts_with(".bunko-probe-")
        || (s.starts_with('.') && s.ends_with(".tmp") && s.contains(".bunko-"))
}
