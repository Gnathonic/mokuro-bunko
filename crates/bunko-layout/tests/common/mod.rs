//! Shared helpers for the golden tests: fixture loading and value conversion.
#![allow(dead_code)]

use std::io::Read;
use std::path::PathBuf;

use bunko_layout::json::Value;
use bunko_layout::reconcile::{Reconciled, Source};
use bunko_layout::records::{RawLine, RawPage};

pub fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
}

/// Parse a fixture with the crate's own (correctly rounded) JSON parser.
pub fn load(rel: &str) -> Value {
    let path = golden_dir().join(rel);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let text = if rel.ends_with(".gz") {
        let mut s = String::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_string(&mut s)
            .expect("gunzip");
        s
    } else {
        String::from_utf8(bytes).expect("utf-8")
    };
    Value::parse(&text).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

pub fn load_crate_json(rel: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    Value::parse(&text).expect("json")
}

pub fn arr(v: &Value) -> &Vec<Value> {
    v.as_array().unwrap_or_else(|| panic!("not an array: {v}"))
}

pub fn s(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}"))
}

pub fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

pub fn i(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        _ => panic!("not an int: {v}"),
    }
}

pub fn b(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        _ => panic!("not a bool: {v}"),
    }
}

pub fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key)
        .unwrap_or_else(|| panic!("missing key {key} in {}", short(v)))
}

pub fn short(v: &Value) -> String {
    let t = v.to_string();
    if t.chars().count() > 300 {
        t.chars().take(300).collect::<String>() + "..."
    } else {
        t
    }
}

/// Bit-exact float equality (NaN == NaN, -0.0 != 0.0).
pub fn same(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan())
}

pub fn opt_f(v: &Value) -> Option<f64> {
    match v {
        Value::Null => None,
        other => Some(f(other)),
    }
}

pub fn reconciled_from(v: &Value) -> Reconciled {
    let v = v.get("__reconciled__").unwrap_or(v);
    Reconciled {
        text: s(get(v, "text")).to_string(),
        vlm: s(get(v, "vlm")).to_string(),
        ctc: s(get(v, "ctc")).to_string(),
        agreement: opt_f(get(v, "agreement")),
        source: if s(get(v, "source")) == "ctc" {
            Source::Ctc
        } else {
            Source::Merged
        },
        notes: arr(get(v, "notes"))
            .iter()
            .map(|n| s(n).to_string())
            .collect(),
        second: match get(v, "second") {
            Value::Null => None,
            x => Some(s(x).to_string()),
        },
        engine_only: b(get(v, "engine_only")),
        confirmed: b(get(v, "confirmed")),
    }
}

/// Compare a Reconciled with its expected state; Err describes the first difference.
pub fn check_reconciled(got: &Reconciled, want: &Value) -> Result<(), String> {
    let w = reconciled_from(want);
    let agree = match (got.agreement, w.agreement) {
        (None, None) => true,
        (Some(a), Some(b)) => same(a, b),
        _ => false,
    };
    if got.text != w.text
        || got.vlm != w.vlm
        || got.ctc != w.ctc
        || !agree
        || got.source != w.source
        || got.notes != w.notes
        || got.second != w.second
        || got.engine_only != w.engine_only
        || got.confirmed != w.confirmed
    {
        return Err(format!("got {got:?}\nwant {w:?}"));
    }
    Ok(())
}

/// The unrounded real pages (`inputs/real_pages.json`), by name.
pub fn real_pages() -> Vec<(String, RawPage)> {
    let v = load("inputs/real_pages.json");
    arr(get(&v, "pages"))
        .iter()
        .map(|p| (s(get(p, "name")).to_string(), RawPage::from_value(p)))
        .collect()
}

/// A 0.5.2 test fixture page (copied from 0.5.2's `tests/fixtures/ppocr/<name>.json`
/// into `tests/golden/fixtures/ppocr/`).
pub fn fixture_page(name: &str) -> Value {
    load_crate_json(&format!("tests/golden/fixtures/ppocr/{name}.json"))
}

/// A line record as the Python golden script's `as_lines` reads it
/// (`vertical` defaults to True there).
pub fn line_as_python(v: &Value) -> RawLine {
    let mut line = RawLine::from_value(v);
    if v.get("vertical").is_none() {
        line.vertical = true;
    }
    line
}
