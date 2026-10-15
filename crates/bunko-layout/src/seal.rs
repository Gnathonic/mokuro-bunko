//! The seal on a sidecar Bunko installs: D4's proof, carried by the file itself.
//!
//! `ocr_engine.content_sha256` is the sha256 of the file's exact bytes with that value
//! written as 64 zeros. Any Bunko can check it (a library moved to another machine, a
//! new instance on the same cloud folder), with no database of its own. Any change to
//! the bytes breaks it: an edit, a reformat, a tool that re-serialises the JSON while
//! keeping the `ocr_engine` block. Copying a sealed file elsewhere keeps it, as the
//! bytes are unchanged.
//!
//! The algorithm is public, so a seal can be recomputed on purpose. That gains nothing:
//! whoever can rewrite the file can also replace it outright. The seal guards against
//! accidents, chiefly a person's corrections being taken for model output.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::json::{Separators, Value};
use crate::sidecar::{SidecarError, write_atomic};

/// The key of the seal inside the `ocr_engine` block.
pub const SEAL_KEY: &str = "content_sha256";

const ZEROS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn prefix() -> String {
    format!("\"{SEAL_KEY}\":\"")
}

/// The sealed compact text of `data`, or `None` when it has no `ocr_engine` object (or
/// the seal's text would not be found exactly once). `data`'s seal is set to match.
pub fn seal(data: &mut Value) -> Option<String> {
    let engine = data.get_mut("ocr_engine").filter(|e| e.is_object())?;
    engine.set(SEAL_KEY, Value::str(ZEROS));
    let text = data.dumps(Separators::Compact);
    let placeholder = format!("{}{ZEROS}\"", prefix());
    if text.matches(&placeholder).count() != 1 {
        if let Some(Value::Object(items)) = data.get_mut("ocr_engine") {
            items.retain(|(k, _)| k != SEAL_KEY);
        }
        return None;
    }
    let digest = hex::encode(Sha256::digest(text.as_bytes()));
    if let Some(engine) = data.get_mut("ocr_engine") {
        engine.set(SEAL_KEY, Value::str(digest.clone()));
    }
    Some(text.replacen(&placeholder, &format!("{}{digest}\"", prefix()), 1))
}

/// Seal a sidecar file in place (compact, atomic). `Ok(false)` when it has no
/// `ocr_engine` object (left untouched: there is nothing to seal into).
pub fn seal_sidecar_file(path: &Path) -> Result<bool, SidecarError> {
    let text = fs::read_to_string(path).map_err(|source| SidecarError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut data = Value::parse(&text).map_err(|source| SidecarError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let Some(sealed) = seal(&mut data) else {
        return Ok(false);
    };
    write_atomic(path, &sealed)?;
    Ok(true)
}

/// Whether `bytes` carry a valid seal: one `"content_sha256":"<64 hex>"`, inside the
/// `ocr_engine` block, equal to the sha256 of the bytes with it zeroed.
pub fn is_sealed(bytes: &[u8]) -> bool {
    let prefix = prefix();
    let p = prefix.as_bytes();
    let mut found = None;
    let mut i = 0;
    while let Some(at) = find(&bytes[i..], p).map(|o| o + i) {
        if found.is_some() {
            return false;
        }
        found = Some(at + p.len());
        i = at + p.len();
    }
    let Some(start) = found else {
        return false;
    };
    let Some(digest) = bytes.get(start..start + 64) else {
        return false;
    };
    if bytes.get(start + 64) != Some(&b'"')
        || !digest
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return false;
    }
    let mut zeroed = bytes.to_vec();
    zeroed[start..start + 64].copy_from_slice(ZEROS.as_bytes());
    if hex::encode(Sha256::digest(&zeroed)).as_bytes() != digest {
        return false;
    }
    // The one occurrence must be the block's own key, not text that looks like it.
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|t| Value::parse(t).ok())
        .and_then(|v| {
            v.get("ocr_engine")
                .and_then(|e| e.get(SEAL_KEY))
                .and_then(Value::as_str)
                .map(|s| s.as_bytes() == digest)
        })
        .unwrap_or(false)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(engine: bool) -> Value {
        let mut items = vec![
            ("version".to_string(), Value::str("0.2.5")),
            ("title".to_string(), Value::str("T")),
        ];
        if engine {
            let mut e = Value::object();
            e.set("id", Value::str("hayai-nova"));
            items.push(("ocr_engine".to_string(), e));
        }
        items.push((
            "pages".to_string(),
            Value::Array(vec![Value::str("橋を渡ろう")]),
        ));
        Value::Object(items)
    }

    #[test]
    fn a_sealed_document_checks_and_any_change_breaks_it() {
        let mut d = doc(true);
        let text = seal(&mut d).expect("sealed");
        assert!(is_sealed(text.as_bytes()), "{text}");
        assert_eq!(d.dumps(Separators::Compact), text, "data carries the seal");
        // An edit to the text, the stamp kept.
        let edited = text.replace("橋を渡ろう", "橋を渡った");
        assert!(!is_sealed(edited.as_bytes()));
        // A reformat with the same content.
        let mut parsed = Value::parse(&text).unwrap();
        let pretty = parsed.dumps(Separators::Default);
        assert!(!is_sealed(pretty.as_bytes()));
        // Re-sealing the edit makes it sealed again (on purpose, not by accident).
        let mut again = Value::parse(&edited).unwrap();
        assert!(is_sealed(seal(&mut again).unwrap().as_bytes()));
        // Sealing twice gives the same bytes.
        assert_eq!(seal(&mut parsed).unwrap(), text);
    }

    #[test]
    fn no_engine_block_no_seal() {
        let mut d = doc(false);
        assert_eq!(seal(&mut d), None);
        assert!(!is_sealed(d.dumps(Separators::Compact).as_bytes()));
    }

    #[test]
    fn a_lookalike_in_the_text_is_not_a_seal() {
        // The seal key quoted inside OCR text is escaped, so it never matches; a
        // second real occurrence (outside the block) voids the seal.
        let mut d = doc(true);
        let text = seal(&mut d).unwrap();
        let mut v = Value::parse(&text).unwrap();
        v.set(SEAL_KEY, Value::str(ZEROS));
        assert!(!is_sealed(v.dumps(Separators::Compact).as_bytes()));
        let moved = text.replacen("\"ocr_engine\":{", "\"other\":{", 1);
        assert!(!is_sealed(moved.as_bytes()));
    }

    #[test]
    fn a_sealed_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Vol 1.mokuro");
        fs::write(&p, doc(true).dumps(Separators::Compact)).unwrap();
        assert!(seal_sidecar_file(&p).unwrap());
        assert!(is_sealed(&fs::read(&p).unwrap()));
        let q = dir.path().join("plain.mokuro");
        fs::write(&q, doc(false).dumps(Separators::Compact)).unwrap();
        assert!(!seal_sidecar_file(&q).unwrap());
    }
}
