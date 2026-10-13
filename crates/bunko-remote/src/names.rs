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
    // Exactly one normal component on this platform: no drive or UNC prefix (Windows
    // `C:x` would replace the base on `join`), no root, nothing that parses as more.
    let mut parts = Path::new(name).components();
    matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(part)), None) if part == std::ffi::OsStr::new(name)
    )
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
