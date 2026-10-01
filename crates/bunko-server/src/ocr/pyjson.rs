//! Python `json.dumps` spellings the wire and the state files depend on: `sort_keys`,
//! `ensure_ascii`, and the two separator styles. Floats use Python's `repr`.

use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub struct Style {
    pub sort_keys: bool,
    pub ensure_ascii: bool,
    /// `(",", ":")` when true, else Python's default `(", ", ": ")`.
    pub compact: bool,
}

/// `json.dumps(v, sort_keys=True)` (default separators, ASCII escapes).
pub const SORTED_DEFAULT: Style = Style {
    sort_keys: true,
    ensure_ascii: true,
    compact: false,
};
/// `json.dumps(v, separators=(",", ":"))`.
pub const COMPACT: Style = Style {
    sort_keys: false,
    ensure_ascii: true,
    compact: true,
};
/// `json.dumps(v, sort_keys=True, separators=(",", ":"))`.
pub const SORTED_COMPACT: Style = Style {
    sort_keys: true,
    ensure_ascii: true,
    compact: true,
};
/// `json.dumps(v)`.
pub const DEFAULT: Style = Style {
    sort_keys: false,
    ensure_ascii: true,
    compact: false,
};

pub fn dumps(value: &Value, style: Style) -> String {
    let mut out = String::new();
    write(&mut out, value, style);
    out
}

fn write(out: &mut String, value: &Value, style: Style) {
    let (item, kv) = if style.compact {
        (",", ":")
    } else {
        (", ", ": ")
    };
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                out.push_str(&bunko_sched::py::float_repr(n.as_f64().unwrap_or(0.0)));
            } else {
                out.push_str(&n.to_string());
            }
        }
        Value::String(s) => write_str(out, s, style.ensure_ascii),
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                write(out, v, style);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut keys: Vec<&String> = map.keys().collect();
            if style.sort_keys {
                // Python sorts by code point; Rust's str order is the same (UTF-8 byte
                // order equals code point order).
                keys.sort();
            }
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(item);
                }
                write_str(out, k, style.ensure_ascii);
                out.push_str(kv);
                write(out, &map[k], style);
            }
            out.push('}');
        }
    }
}

fn write_str(out: &mut String, s: &str, ensure_ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ensure_ascii && (c as u32) > 0x7f => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spellings() {
        let v = json!({"b": 1, "a": [1.0, "\u{e9}"], "c": null});
        assert_eq!(
            dumps(&v, SORTED_DEFAULT),
            "{\"a\": [1.0, \"\\u00e9\"], \"b\": 1, \"c\": null}"
        );
        assert_eq!(
            dumps(&v, COMPACT),
            "{\"b\":1,\"a\":[1.0,\"\\u00e9\"],\"c\":null}"
        );
    }
}
