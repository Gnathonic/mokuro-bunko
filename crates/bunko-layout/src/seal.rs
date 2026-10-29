//! The seal on a sidecar Bunko installs: D4's proof, carried by the file itself.
//!
//! `ocr_engine.seal` is `"v1:"` + the sha256 of the RFC 8785 canonical JSON
//! ([`crate::canon`]) of `{"ocr_engine": <the block without its seal>, "pages": <the
//! pages>}`. It covers the CONTENT the model made and the block that names the model,
//! nothing else: the series and volume names, uuids, `chars`, `spine_width`,
//! `version`, an `edited` block and the key order and spacing are outside it. So it
//! survives a rename (a reader or Bunko rewriting the names), a re-serialisation (any
//! tool, any JSON library) and a move to another machine, and any Bunko or reader can
//! check it with no database of its own. An edit of a page or of the block breaks it.
//!
//! The algorithm is public, so a seal can be recomputed on purpose. That gains nothing:
//! whoever can rewrite the file can also replace it outright. The seal guards against
//! accidents, chiefly a person's corrections being taken for model output.
//!
//! It replaces the byte seal `ocr_engine.content_sha256` (the sha256 of the exact bytes),
//! which no release shipped: that key proves nothing and is dropped when Bunko seals.
//! Interop vectors: `interop/contract/seal/*.json` (`tests/seal_vectors.rs`).

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::canon::jcs;
use crate::json::{Separators, Value};
use crate::sidecar::{SidecarError, write_atomic};

/// The key of the seal inside the `ocr_engine` block.
pub const SEAL_KEY: &str = "seal";

/// The unshipped byte seal's key: never part of the seal's input, removed on sealing.
pub const LEGACY_SEAL_KEY: &str = "content_sha256";

/// The version prefix of a seal's value.
pub const SEAL_PREFIX: &str = "v1:";

/// The canonical text the seal hashes: the JCS of `{ocr_engine: the block without
/// [`SEAL_KEY`] and [`LEGACY_SEAL_KEY`], pages}`. `None` with no `ocr_engine` object, no
/// `pages` array, or a NaN or infinity in either (no JSON form, so no seal).
pub fn seal_input(data: &Value) -> Option<String> {
    let Some(Value::Object(block)) = data.get("ocr_engine") else {
        return None;
    };
    let pages = data.get("pages").filter(|p| p.as_array().is_some())?;
    let block = block
        .iter()
        .filter(|(k, _)| k != SEAL_KEY && k != LEGACY_SEAL_KEY)
        .cloned()
        .collect();
    jcs(&Value::Object(vec![
        ("ocr_engine".to_string(), Value::Object(block)),
        ("pages".to_string(), pages.clone()),
    ]))
}

/// The seal `data` would carry (`"v1:<64 hex>"`), or `None` as for [`seal_input`].
pub fn seal_of(data: &Value) -> Option<String> {
    let input = seal_input(data)?;
    Some(format!(
        "{SEAL_PREFIX}{}",
        hex::encode(Sha256::digest(input.as_bytes()))
    ))
}

/// Seal `data` in place and return its compact text: the byte seal goes, the block's
/// [`SEAL_KEY`] is set. `None` (and `data` untouched) when [`seal_input`] has none.
pub fn seal(data: &mut Value) -> Option<String> {
    let stamp = seal_of(data)?;
    if let Some(Value::Object(block)) = data.get_mut("ocr_engine") {
        block.retain(|(k, _)| k != LEGACY_SEAL_KEY);
    }
    data.get_mut("ocr_engine")?.set(SEAL_KEY, Value::Str(stamp));
    Some(data.dumps(Separators::Compact))
}

/// Seal a sidecar file in place (compact, atomic). `Ok(false)` when it cannot carry a
/// seal (no `ocr_engine` object or `pages` array, or a non-finite number), left untouched.
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

/// Whether `bytes` (a sidecar, a UTF-8 BOM allowed) carry a valid seal: an
/// `ocr_engine.seal` of the form `v1:<64 lowercase hex>` equal to the one its content
/// gives.
pub fn is_sealed(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let Some(data) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|t| Value::parse(t).ok())
    else {
        return false;
    };
    let Some(stamp) = data
        .get("ocr_engine")
        .and_then(|e| e.get(SEAL_KEY))
        .and_then(Value::as_str)
    else {
        return false;
    };
    let well_formed = stamp.strip_prefix(SEAL_PREFIX).is_some_and(|d| {
        d.len() == 64
            && d.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    well_formed && seal_of(&data).as_deref() == Some(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidecar::{LayerStamp, Normalization, normalize_sidecar};

    /// A sidecar the way the runner writes one: Python's default separators.
    const DOC: &str = r#"{"version": "0.2.5", "title": "T", "title_uuid": "tu", "volume": "V1", "volume_uuid": "vu", "chars": 5, "ocr_engine": {"id": "hayai-nova", "revision": 2, "detector": "ppocr-manga", "patch_budget": 512}, "pages": [{"img_path": "001.jpg", "img_width": 1200, "img_height": 1700, "blocks": [{"box": [10, 20, 30.5, 40], "vertical": true, "font_size": 24.0, "lines_coords": [[[10.0, 20.0], [30.5, 20.0], [30.5, 40.0], [10.0, 40.0]]], "lines": ["橋を渡ろう"]}]}]}"#;

    fn doc() -> Value {
        Value::parse(DOC).unwrap()
    }

    fn sealed_text() -> String {
        seal(&mut doc()).expect("sealed")
    }

    fn reseal(text: &str, edit: impl FnOnce(&mut Value)) -> bool {
        let mut v = Value::parse(text).unwrap();
        edit(&mut v);
        is_sealed(v.dumps(Separators::Compact).as_bytes())
    }

    #[test]
    fn a_sealed_document_checks() {
        let mut d = doc();
        let text = seal(&mut d).expect("sealed");
        assert!(is_sealed(text.as_bytes()), "{text}");
        assert_eq!(d.dumps(Separators::Compact), text, "data carries the seal");
        let stamp = d.get("ocr_engine").and_then(|e| e.get(SEAL_KEY)).unwrap();
        let stamp = stamp.as_str().unwrap();
        assert!(stamp.starts_with("v1:") && stamp.len() == 67, "{stamp}");
        assert!(
            !is_sealed(DOC.as_bytes()),
            "the runner's bytes are not sealed"
        );
    }

    #[test]
    fn what_lies_outside_the_seal_can_change() {
        let text = sealed_text();
        for (key, value) in [
            ("title", Value::str("Renamed series")),
            ("volume", Value::str("Renamed volume")),
            ("title_uuid", Value::str("other")),
            ("volume_uuid", Value::str("other")),
            ("chars", Value::Int(99)),
            ("spine_width", Value::Float(12.5)),
            ("version", Value::str("0.2.1")),
        ] {
            assert!(reseal(&text, |v| v.set(key, value)), "{key}");
        }
    }

    #[test]
    fn a_reformat_or_key_reorder_keeps_the_seal() {
        let text = sealed_text();
        let pretty = Value::parse(&text).unwrap().dumps(Separators::Default);
        assert!(is_sealed(pretty.as_bytes()));
        let reordered = match Value::parse(&text).unwrap() {
            Value::Object(mut items) => {
                items.reverse();
                if let Some((_, Value::Object(engine))) =
                    items.iter_mut().find(|(k, _)| k == "ocr_engine")
                {
                    engine.reverse();
                }
                Value::Object(items)
            }
            _ => unreachable!(),
        };
        assert!(is_sealed(reordered.dumps(Separators::Compact).as_bytes()));
        let bom = format!("\u{feff}{text}");
        assert!(is_sealed(bom.as_bytes()), "a BOM is not content");
        // A float written the other way is the same number.
        let respelled = text.replace("\"font_size\":24.0", "\"font_size\":24");
        assert_ne!(respelled, text);
        assert!(is_sealed(respelled.as_bytes()));
    }

    #[test]
    fn an_edit_of_the_pages_or_the_block_breaks_it() {
        let text = sealed_text();
        assert!(!is_sealed(
            text.replace("橋を渡ろう", "橋を渡った").as_bytes()
        ));
        assert!(!is_sealed(text.replace("30.5", "31.5").as_bytes()));
        assert!(!reseal(&text, |v| {
            v.get_mut("ocr_engine")
                .unwrap()
                .set("id", Value::str("mokuro"))
        }));
        assert!(!reseal(&text, |v| {
            v.get_mut("ocr_engine")
                .unwrap()
                .set("revision", Value::Int(3))
        }));
        assert!(!reseal(&text, |v| {
            v.get_mut("ocr_engine")
                .unwrap()
                .set("generation", Value::str("nova"))
        }));
        // Re-sealing the edit makes it sealed again (on purpose, not by accident).
        let edited = text.replace("橋を渡ろう", "橋を渡った");
        let mut again = Value::parse(&edited).unwrap();
        assert!(is_sealed(seal(&mut again).unwrap().as_bytes()));
    }

    #[test]
    fn a_malformed_seal_is_no_seal() {
        let text = sealed_text();
        let stamp = text.split("\"seal\":\"").nth(1).unwrap()[..67].to_string();
        for bad in [
            stamp.replacen("v1:", "v2:", 1),
            stamp.to_uppercase().replacen("V1:", "v1:", 1),
            stamp[3..].to_string(),
            format!("{stamp}0"),
        ] {
            assert!(!is_sealed(text.replace(&stamp, &bad).as_bytes()), "{bad}");
        }
        // The seal moved out of the block is not the block's seal.
        assert!(!reseal(&text, |v| {
            let s = v.get("ocr_engine").unwrap().get(SEAL_KEY).unwrap().clone();
            if let Some(Value::Object(e)) = v.get_mut("ocr_engine") {
                e.retain(|(k, _)| k != SEAL_KEY);
            }
            v.set(SEAL_KEY, s);
        }));
        assert!(!is_sealed(b"not json"));
        assert!(!is_sealed(b"[]"));
    }

    #[test]
    fn the_unshipped_byte_seal_is_not_a_seal_and_goes_when_sealing() {
        let mut v = doc();
        v.get_mut("ocr_engine")
            .unwrap()
            .set(LEGACY_SEAL_KEY, Value::str("0".repeat(64)));
        let legacy = v.dumps(Separators::Compact);
        assert!(
            !is_sealed(legacy.as_bytes()),
            "content_sha256 alone proves nothing"
        );
        let text = seal(&mut v).unwrap();
        assert!(!text.contains(LEGACY_SEAL_KEY), "{text}");
        assert_eq!(text, sealed_text(), "the old key never reaches the seal");
        // One left beside a valid seal is outside it.
        assert!(reseal(&text, |v| {
            v.get_mut("ocr_engine")
                .unwrap()
                .set(LEGACY_SEAL_KEY, Value::str("x"))
        }));
    }

    #[test]
    fn no_engine_block_or_no_pages_no_seal() {
        let mut v = doc();
        if let Value::Object(items) = &mut v {
            items.retain(|(k, _)| k != "ocr_engine");
        }
        let before = v.clone();
        assert_eq!(seal(&mut v), None);
        assert_eq!(v, before, "left untouched");
        let mut v = doc();
        v.set("ocr_engine", Value::str("hayai-nova"));
        assert_eq!(seal(&mut v), None);
        let mut v = doc();
        if let Value::Object(items) = &mut v {
            items.retain(|(k, _)| k != "pages");
        }
        assert_eq!(seal(&mut v), None);
        let mut v = doc();
        v.set("pages", Value::object());
        assert_eq!(seal(&mut v), None);
        assert_eq!(seal(&mut Value::Array(vec![])), None);
    }

    #[test]
    fn a_non_finite_number_in_the_pages_means_no_seal() {
        let mut v = Value::parse(&DOC.replace("24.0", "NaN")).unwrap();
        // (NaN != NaN, so compare the text.)
        let before = v.dumps(Separators::Default);
        assert_eq!(seal(&mut v), None);
        assert_eq!(v.dumps(Separators::Default), before, "left untouched");
        assert_eq!(seal_input(&v), None);
        let mut v =
            Value::parse(&DOC.replace("\"revision\": 2", "\"revision\": Infinity")).unwrap();
        assert_eq!(seal(&mut v), None);
    }

    #[test]
    fn sealing_twice_is_byte_identical() {
        let text = sealed_text();
        let mut again = Value::parse(&text).unwrap();
        assert_eq!(seal(&mut again).unwrap(), text);
        // And from a reformatted copy, the same seal.
        let mut pretty =
            Value::parse(&Value::parse(&text).unwrap().dumps(Separators::Default)).unwrap();
        assert_eq!(seal(&mut pretty).unwrap(), text);
    }

    #[test]
    fn the_seal_input_is_the_block_without_its_seals_and_the_pages() {
        let mut v = doc();
        v.get_mut("ocr_engine")
            .unwrap()
            .set(LEGACY_SEAL_KEY, Value::str("x"));
        let input = seal_input(&v).unwrap();
        assert!(
            input.starts_with(
                r#"{"ocr_engine":{"detector":"ppocr-manga","id":"hayai-nova","patch_budget":512,"revision":2},"pages":[{"blocks":[{"box":[10,20,30.5,40],"font_size":24,"#
            ),
            "{input}"
        );
        seal(&mut v).unwrap();
        assert_eq!(
            seal_input(&v).unwrap(),
            input,
            "the seal is outside its own input"
        );
    }

    /// Critique C11: the server stamps `generation` into a layer's block when it
    /// normalises the file, so the seal is computed after that, never before.
    #[test]
    fn the_seal_is_taken_after_normalisation_stamps_the_block() {
        let norm = Normalization {
            series_name: "Series".into(),
            volume: "V1".into(),
            volume_uuid: "vu".into(),
            stamp: Some(LayerStamp {
                engine: "hayai-nova".into(),
                generator: "mokuro-bunko 0.8.0".into(),
                generation: "nova".into(),
            }),
        };
        // Sealed first, normalised after: the stamp lands inside the sealed block.
        let mut early = doc();
        seal(&mut early).unwrap();
        assert!(normalize_sidecar(&mut early, &norm));
        assert!(!is_sealed(early.dumps(Separators::Compact).as_bytes()));
        // Normalised first (what collect does), then sealed.
        let mut v = doc();
        assert!(normalize_sidecar(&mut v, &norm));
        let text = seal(&mut v).unwrap();
        assert!(is_sealed(text.as_bytes()));
        assert!(text.contains("\"generation\":\"nova\""));
        // A primary install (no stamp) only rewrites names, which the seal leaves out.
        let mut primary = doc();
        seal(&mut primary).unwrap();
        assert!(normalize_sidecar(
            &mut primary,
            &Normalization {
                stamp: None,
                ..norm
            }
        ));
        assert!(is_sealed(primary.dumps(Separators::Compact).as_bytes()));
    }

    #[test]
    fn a_sealed_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Vol 1.mokuro");
        fs::write(&p, DOC).unwrap();
        assert!(seal_sidecar_file(&p).unwrap());
        assert!(is_sealed(&fs::read(&p).unwrap()));
        assert_eq!(fs::read_to_string(&p).unwrap(), sealed_text());
        let q = dir.path().join("plain.mokuro");
        let mut plain = doc();
        if let Value::Object(items) = &mut plain {
            items.retain(|(k, _)| k != "ocr_engine");
        }
        let plain = plain.dumps(Separators::Compact);
        fs::write(&q, &plain).unwrap();
        assert!(!seal_sidecar_file(&q).unwrap());
        assert_eq!(fs::read_to_string(&q).unwrap(), plain, "left untouched");
    }
}
