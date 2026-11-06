//! The content seal's interop vectors, `interop/contract/seal/*.json`: what Bunko's
//! [`bunko_layout::seal`] makes of each document, for Mokuro Reader (and any other tool)
//! to check its own seal against. `BUNKO_BLESS=1` rewrites them; otherwise they are
//! compared with what this build computes, and each one is checked on its own.
//!
//! A vector is `{description, document, seal_input, seal, is_sealed}`: `document` is a
//! sidecar's exact text (a string, so Python's float spellings and the key order
//! survive), `seal_input` the canonical text the seal hashes and `seal` the seal it
//! gives (both `null` when it has none), `is_sealed` whether the document's own seal
//! checks.

use std::path::{Path, PathBuf};

use bunko_layout::json::{Separators, Value};
use bunko_layout::seal::{LEGACY_SEAL_KEY, SEAL_KEY, is_sealed, seal_input, seal_of};

const PLACEHOLDER: &str = "<SEAL>";

/// A sidecar as the runner writes it (Python's default separators), after the server
/// stamped the layer's `generation` into its block.
const RUNNER: &str = r#"{"version": "0.2.5", "title": "シリーズ", "title_uuid": "0b5c1b5e-0000-5000-8000-000000000001", "volume": "シリーズ 01", "volume_uuid": "0b5c1b5e-0000-5000-8000-000000000002", "chars": 8, "ocr_engine": {"id": "hayai-nova", "detector": "ppocr-manga", "patch_budget": 512, "revision": 2, "created": "2026-10-29T00:00:00Z", "generator": "mokuro-bunko 0.8.0", "generation": "nova", "seal": "<SEAL>"}, "pages": [{"version": "0.2.5", "img_width": 1200, "img_height": 1700, "img_path": "001.jpg", "blocks": [{"box": [812, 104, 880, 402], "vertical": true, "font_size": 31.5, "lines_coords": [[[848.0, 104.0], [880.0, 104.0], [880.0, 402.0], [848.0, 402.0]], [[812.0, 104.5], [845.25, 104.5], [845.25, 300.0], [812.0, 300.0]]], "lines": ["橋を渡ろう", "よし！"]}]}, {"version": "0.2.5", "img_width": 1200, "img_height": 1700, "img_path": "002.jpg", "blocks": []}]}"#;

/// Every float spelling Python's `json.dumps` writes, and integers past 2^53.
const FLOATS: &str = r#"{"version": "0.2.5", "title": "T", "volume": "V", "ocr_engine": {"id": "mokuro", "seal": "<SEAL>"}, "pages": [{"img_path": "001.jpg", "img_width": 1200, "img_height": 1700, "blocks": [{"box": [1e-05, 1e+16, 0.30000000000000004, -0.0], "font_size": 24.0, "lines_coords": [[[5e-324, 1.7976931348623157e+308], [1.2345678901234568e+17, 1e+21], [1e-07, 333333333.3333333], [1e-06, 1e+20]]], "lines": ["数"], "extra": [9007199254740993, 12345678901234567890, -0, 1.0, 4.5, -2.5]}]}]}"#;

/// Keys whose UTF-16 order differs from their code point order (an astral emoji
/// before U+FF61), uppercase before lowercase, the empty key; pages before the block.
const KEYS: &str = r#"{"pages": [{"｡": 1, "😀": 2, "é": 3, "a": 4, "B": 5, "": 6, "blocks": []}], "ocr_engine": {"seal": "<SEAL>", "ｚ": true, "id": "hayai-nova", "Z": null}}"#;

/// What a string escape looks like canonically: C0 controls escaped (lowercase hex),
/// U+2028, U+007F, `/` and non-ASCII literal; a `あ` escape in the file is `あ`.
const STRINGS: &str = r#"{"ocr_engine": {"id": "hayai-nova", "seal": "<SEAL>"}, "pages": [{"blocks": [{"lines": ["\u001f\u2028\u007f/\"\\\b\f\n\r\t\u0000", "\u3042い😀", "「橋」…"]}]}]}"#;

fn sealed(template: &str) -> String {
    let stamp = seal_of(&Value::parse(template).unwrap()).expect("a sealable template");
    template.replace(PLACEHOLDER, &stamp)
}

/// A rename the way a tool that re-serialises the file does it: new names and uuids,
/// keys and spacing of its own. The content is untouched, so the seal still checks.
fn renamed_text(text: &str, series: &str, volume: &str) -> String {
    let Value::Object(items) = Value::parse(text.trim_start_matches('\u{feff}')).unwrap() else {
        panic!("not an object");
    };
    let mut items: Vec<(String, Value)> = items
        .into_iter()
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "title" | "volume" | "title_uuid" | "volume_uuid"
            )
        })
        .collect();
    items.reverse();
    items.push(("title".into(), Value::str(series)));
    items.push(("volume".into(), Value::str(volume)));
    items.push((
        "title_uuid".into(),
        Value::str("11111111-1111-5111-8111-111111111111"),
    ));
    items.push((
        "volume_uuid".into(),
        Value::str("22222222-2222-5222-8222-222222222222"),
    ));
    items.push(("chars".into(), Value::Int(1234)));
    items.push(("spine_width".into(), Value::Float(13.25)));
    Value::Object(items).dumps(Separators::Default)
}

fn edit_block(text: &str, key: &str, value: Value) -> String {
    let mut v = Value::parse(text).unwrap();
    v.get_mut("ocr_engine").unwrap().set(key, value);
    v.dumps(Separators::Compact)
}

/// The vectors: name, description, document.
fn vectors() -> Vec<(&'static str, &'static str, String)> {
    let runner = sealed(RUNNER);
    let stamp = seal_of(&Value::parse(&runner).unwrap()).unwrap();
    let upper = runner.replace(&stamp, &format!("v1:{}", stamp[3..].to_uppercase()));
    let legacy_only = {
        let mut v = Value::parse(&runner).unwrap();
        let engine = v.get_mut("ocr_engine").unwrap();
        if let Value::Object(items) = engine {
            items.retain(|(k, _)| k != SEAL_KEY);
        }
        engine.set(LEGACY_SEAL_KEY, Value::str("0".repeat(64)));
        v.dumps(Separators::Compact)
    };
    vec![
        (
            "minimal",
            "The smallest sealable document: a block and a pages array.",
            sealed(r#"{"ocr_engine":{"id":"hayai-nova","seal":"<SEAL>"},"pages":[]}"#),
        ),
        (
            "runner-output",
            "A layer as Bunko installs it: the runner's text (Python separators), names and uuids normalised, generation stamped, then sealed.",
            runner.clone(),
        ),
        (
            "python-floats",
            "Python's float spellings (1e-05, 1e+16, -0.0, 24.0, 5e-324, ...) and integers past 2^53, which canonicalise to what JavaScript's JSON.parse then String() gives.",
            sealed(FLOATS),
        ),
        (
            "key-order",
            "Keys sort by UTF-16 code units (an astral emoji before U+FF61); pages before the block in the file.",
            sealed(KEYS),
        ),
        (
            "string-escapes",
            "Strings canonicalise like JSON.stringify: C0 controls escaped, U+2028, U+007F, '/' and non-ASCII literal.",
            sealed(STRINGS),
        ),
        (
            "renamed",
            "runner-output renamed and re-serialised: names, uuids, chars, spine_width, key order and spacing are outside the seal.",
            renamed_text(&runner, "改名されたシリーズ", "改名されたシリーズ 第1巻"),
        ),
        (
            "bom",
            "runner-output behind a UTF-8 byte order mark, which is not content.",
            format!("\u{feff}{runner}"),
        ),
        (
            "edited-line",
            "runner-output with one OCR line corrected and the seal kept: the seal no longer checks.",
            runner.replace("橋を渡ろう", "橋を渡った"),
        ),
        (
            "edited-block",
            "runner-output with the block's revision changed: the block is inside the seal.",
            edit_block(&runner, "revision", Value::Int(3)),
        ),
        (
            "legacy-byte-seal",
            "Only the unshipped byte seal ocr_engine.content_sha256: not a seal, and never part of the seal's input.",
            legacy_only,
        ),
        (
            "legacy-beside-seal",
            "A content_sha256 left beside a valid seal is outside it.",
            edit_block(&runner, LEGACY_SEAL_KEY, Value::str("x")),
        ),
        (
            "uppercase-seal",
            "The right digest in uppercase hex: a seal is lowercase, so this one does not check.",
            upper,
        ),
        (
            "non-finite",
            "A NaN in the pages has no JSON form: no seal can be made, and none checks.",
            runner.replace("31.5", "NaN"),
        ),
        (
            "overflow",
            "Numbers past the double range (1e400, a 400-digit integer) read as Infinity: no seal, though JSON.stringify would write them as null.",
            runner.replace("31.5", "1e400").replace(
                "\"blocks\": []",
                &format!("\"blocks\": [], \"n\": 1{}", "0".repeat(400)),
            ),
        ),
        (
            "no-engine",
            "No ocr_engine block: nothing to seal.",
            r#"{"version": "0.2.5", "title": "T", "volume": "V", "pages": []}"#.to_string(),
        ),
    ]
}

fn contract_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("BUNKO_CONTRACT_DIR") {
        return PathBuf::from(dir);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interop/contract")
}

/// A JSON string, with the characters an editor would hide written as escapes.
fn string(s: &str) -> String {
    Value::str(s)
        .dumps(Separators::Compact)
        .replace('\u{feff}', "\\ufeff")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .replace('\u{7f}', "\\u007f")
}

fn opt(s: Option<String>) -> String {
    s.map_or("null".to_string(), |s| string(&s))
}

fn render(description: &str, document: &str) -> String {
    let data = Value::parse(document.trim_start_matches('\u{feff}')).ok();
    let input = data.as_ref().and_then(seal_input);
    let stamp = data.as_ref().and_then(seal_of);
    format!(
        "{{\n  \"description\": {},\n  \"document\": {},\n  \"seal_input\": {},\n  \"seal\": {},\n  \"is_sealed\": {}\n}}\n",
        string(description),
        string(document),
        opt(input),
        opt(stamp),
        is_sealed(document.as_bytes()),
    )
}

#[test]
fn seal_vectors() {
    let dir = contract_dir().join("seal");
    let bless = std::env::var("BUNKO_BLESS").is_ok_and(|v| v == "1");
    let vectors = vectors();
    let mut failures = Vec::new();
    if bless {
        std::fs::create_dir_all(&dir).unwrap();
    }
    for (name, description, document) in &vectors {
        let path = dir.join(format!("{name}.json"));
        let want = render(description, document);
        if bless {
            std::fs::write(&path, &want).unwrap();
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(have) if have.replace("\r\n", "\n") == want => {}
            Ok(have) => failures.push(format!(
                "{name}.json differs:\n--- committed\n{have}\n+++ now\n{want}"
            )),
            Err(_) => failures.push(format!("{name}.json is missing; it would be:\n{want}")),
        }
    }
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let file = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = file.strip_suffix(".json") else {
            continue;
        };
        if vectors.iter().any(|(n, _, _)| *n == stem) {
            continue;
        }
        if bless {
            std::fs::remove_file(e.path()).unwrap();
        } else {
            failures.push(format!("{file} is committed but no longer made"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} seal vector(s) differ:\n\n{}\n\nrun BUNKO_BLESS=1 cargo test -p bunko-layout --test seal_vectors to accept",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Each committed vector, read the way another tool would: the document parsed from
/// the vector, its seal recomputed and checked.
#[test]
fn every_committed_vector_checks() {
    if std::env::var("BUNKO_BLESS").is_ok_and(|v| v == "1") {
        return; // seal_vectors is writing them
    }
    let dir = contract_dir().join("seal");
    let mut n = 0;
    for e in std::fs::read_dir(&dir).unwrap().flatten() {
        let text = std::fs::read_to_string(e.path()).unwrap();
        let v = Value::parse(&text).unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        let document = v.get("document").and_then(Value::as_str).unwrap();
        let data = Value::parse(document.trim_start_matches('\u{feff}')).ok();
        let field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        assert_eq!(
            data.as_ref().and_then(seal_input),
            field("seal_input"),
            "{name}"
        );
        assert_eq!(data.as_ref().and_then(seal_of), field("seal"), "{name}");
        assert_eq!(
            Some(&Value::Bool(is_sealed(document.as_bytes()))),
            v.get("is_sealed"),
            "{name}"
        );
        n += 1;
    }
    assert!(n >= 15, "only {n} vectors under {}", dir.display());
}

/// The vectors say what they claim to.
#[test]
fn the_vectors_cover_what_they_name() {
    let by_name = |name: &str| {
        vectors()
            .into_iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, _, d)| d)
            .unwrap()
    };
    for sealed in [
        "minimal",
        "runner-output",
        "python-floats",
        "key-order",
        "string-escapes",
        "renamed",
        "bom",
        "legacy-beside-seal",
    ] {
        assert!(is_sealed(by_name(sealed).as_bytes()), "{sealed}");
    }
    for broken in [
        "edited-line",
        "edited-block",
        "legacy-byte-seal",
        "uppercase-seal",
        "non-finite",
        "overflow",
        "no-engine",
    ] {
        assert!(!is_sealed(by_name(broken).as_bytes()), "{broken}");
    }
    let seal =
        |name: &str| seal_of(&Value::parse(by_name(name).trim_start_matches('\u{feff}')).unwrap());
    assert_eq!(seal("renamed"), seal("runner-output"));
    assert_eq!(seal("bom"), seal("runner-output"));
    assert_eq!(seal("legacy-byte-seal"), seal("runner-output"));
    assert!(seal("edited-line").is_some());
    assert_ne!(seal("edited-line"), seal("runner-output"));
    assert_eq!(seal("non-finite"), None);
    assert_eq!(seal("overflow"), None);
    // Each overflow alone already refuses the seal, not only the pair together.
    let one = |from: &str, to: &str| {
        let text = by_name("runner-output").replace(from, to);
        seal_of(&Value::parse(&text).unwrap())
    };
    assert_eq!(one("31.5", "1e400"), None);
    assert_eq!(one("31.5", &format!("1{}", "0".repeat(400))), None);
    assert_eq!(seal("no-engine"), None);
}

#[test]
fn a_renamed_sealed_document_is_still_sealed() {
    let text = sealed(RUNNER);
    let renamed = renamed_text(&text, "別の名前", "別の名前 02");
    assert_ne!(renamed, text);
    assert!(is_sealed(renamed.as_bytes()));
    // Twice over, from the renamed copy.
    assert!(is_sealed(
        renamed_text(&renamed, "三つ目", "三つ目 03").as_bytes()
    ));
}
