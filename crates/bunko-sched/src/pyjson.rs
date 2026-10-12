//! `json.dumps(obj, ensure_ascii=False, indent=2)`, byte for byte, plus the
//! atomic write every 0.5.2 state file uses.
//!
//! `.ocr-failures.json`, `.ocr-congestion.json` and `.ocr-bench.json` are read
//! by people, by `doctor`, and by a 0.5.2 server after a downgrade, so they are
//! written exactly as Python wrote them: two-space indent, `", "`-free item
//! separators with newlines, `": "` between key and value, non-ASCII raw,
//! floats in `repr()` form (`1e-05`, `1e+16`, `3.0`), key order as inserted.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::Value;

use crate::py::float_repr;

/// `json.dumps(value, ensure_ascii=False, indent=2)`.
pub fn dumps_indent2(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out
}

/// `json.dumps(value, ensure_ascii=False)` (one line, `", "` / `": "`).
pub fn dumps_compact_py(value: &Value) -> String {
    let mut out = String::new();
    write_inline(&mut out, value);
    out
}

fn write_value(out: &mut String, value: &Value, level: usize) {
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, level + 1);
                write_value(out, item, level + 1);
            }
            newline(out, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, level + 1);
                write_str(out, key);
                out.push_str(": ");
                write_value(out, item, level + 1);
            }
            newline(out, level);
            out.push('}');
        }
        scalar => write_scalar(out, scalar),
    }
}

fn write_inline(out: &mut String, value: &Value) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_inline(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_str(out, key);
                out.push_str(": ");
                write_inline(out, item);
            }
            out.push('}');
        }
        scalar => write_scalar(out, scalar),
    }
}

fn newline(out: &mut String, level: usize) {
    out.push('\n');
    for _ in 0..level {
        out.push_str("  ");
    }
}

fn write_scalar(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if n.is_f64() {
                let f = n.as_f64().unwrap_or(0.0);
                if f.is_nan() {
                    out.push_str("NaN");
                } else if f.is_infinite() {
                    out.push_str(if f > 0.0 { "Infinity" } else { "-Infinity" });
                } else {
                    out.push_str(&float_repr(f));
                }
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_str(out, s),
        Value::Array(_) | Value::Object(_) => write_inline(out, value),
    }
}

/// `json.encoder.py_encode_basestring` (the `ensure_ascii=False` escaper).
fn write_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Write `text` to `<path>.tmp`, then rename it over `path` — the pattern
/// every 0.5.2 state file uses, so a reader never sees half a file.
pub fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    let mut name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_os_string();
    name.push(".tmp");
    let tmp = path.with_file_name(name);
    fs::write(&tmp, text.as_bytes())?;
    fs::rename(&tmp, path)
}

/// Read a JSON object file; `None` for a missing, unreadable, non-UTF-8,
/// malformed or non-object file (0.5.2 treats them all as "nothing").
pub fn read_object(path: &Path) -> Option<serde_json::Map<String, Value>> {
    let bytes = fs::read(path).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    match serde_json::from_str::<Value>(&text).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn layout_matches_python() {
        let v =
            json!({"a": [1, 2.5, {"b": null}], "c": {}, "d": [], "é": "x\ny\u{1}\"", "f": 1e-05});
        let want = "{\n  \"a\": [\n    1,\n    2.5,\n    {\n      \"b\": null\n    }\n  ],\n  \"c\": {},\n  \"d\": [],\n  \"é\": \"x\\ny\\u0001\\\"\",\n  \"f\": 1e-05\n}";
        assert_eq!(dumps_indent2(&v), want);
    }
}
