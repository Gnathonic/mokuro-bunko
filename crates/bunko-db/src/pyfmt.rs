//! Byte-exact reproductions of the few Python formatting functions whose output 0.5.2
//! stores in `mokuro.db` or shows to users.
//!
//! - [`dumps`]: `json.dumps` with its `ensure_ascii` and separator options. The audit log's
//!   `details` column is `json.dumps(d, separators=(",",":"), ensure_ascii=True)`, and the
//!   audit search matches the ASCII-escaped spelling, so old (Python-written) and new
//!   (Rust-written) rows must be spelled identically. serde_json does not escape non-ASCII
//!   and prints floats differently (`1e-5` vs Python's `1e-05`), hence this module.
//! - [`repr_str`]: Python `repr()` of a string, for the audit date error message.
//! - [`is_space`] / [`strip`]: Python `str.isspace()` / `str.strip()`.
//! - [`truthy`]: Python truthiness of a decoded JSON value.
//!
//! Object key order is whatever the [`serde_json::Map`] iterates in (sorted unless the
//! `preserve_order` feature is on). Callers that need insertion order use
//! [`crate::AuditDetails`], which keeps its own order.

use serde_json::Value;
use std::fmt::Write as _;

/// The `json.dumps` options 0.5.2 uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonStyle {
    pub ensure_ascii: bool,
    pub item_separator: &'static str,
    pub key_separator: &'static str,
}

impl JsonStyle {
    /// `json.dumps(x, separators=(",", ":"), ensure_ascii=True)` — audit details, cursors.
    pub const COMPACT_ASCII: JsonStyle = JsonStyle {
        ensure_ascii: true,
        item_separator: ",",
        key_separator: ":",
    };
    /// `json.dumps(x)` — `community_details.tags/genres`.
    pub const DEFAULT_ASCII: JsonStyle = JsonStyle {
        ensure_ascii: true,
        item_separator: ", ",
        key_separator: ": ",
    };
    /// `json.dumps(x, ensure_ascii=False)` — `series_facts` JSON columns, `entry_json`.
    pub const DEFAULT_UNICODE: JsonStyle = JsonStyle {
        ensure_ascii: false,
        item_separator: ", ",
        key_separator: ": ",
    };
}

/// `json.dumps(value, ...)` in the given style.
pub fn dumps(value: &Value, style: JsonStyle) -> String {
    let mut out = String::new();
    write_value(&mut out, value, style);
    out
}

/// A JSON string literal (with quotes) as `json.dumps(s, ensure_ascii=...)` writes it.
pub fn dumps_str(s: &str, ensure_ascii: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    write_str(&mut out, s, ensure_ascii);
    out
}

pub(crate) fn write_value(out: &mut String, value: &Value, style: JsonStyle) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_str(out, s, style.ensure_ascii),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item_separator);
                }
                write_value(out, item, style);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (key, item)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(style.item_separator);
                }
                write_str(out, key, style.ensure_ascii);
                out.push_str(style.key_separator);
                write_value(out, item, style);
            }
            out.push('}');
        }
    }
}

fn write_number(out: &mut String, n: &serde_json::Number) {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
    } else if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
    } else if let Some(f) = n.as_f64() {
        out.push_str(&float_repr(f));
    } else {
        // arbitrary_precision numbers: their own text is already JSON.
        out.push_str(&n.to_string());
    }
}

/// Python `float.__repr__` (what `json.dumps` writes for a float; `NaN`/`Infinity` as
/// `json.dumps` spells them).
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    // Rust's `{:e}` is the shortest round-trip digit string, as Python's repr is; only the
    // layout differs.
    let sci = format!("{f:e}");
    let (sign, rest) = match sci.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", sci.as_str()),
    };
    let (mantissa, exp) = rest.split_once('e').unwrap_or((rest, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1;
    let mut out = String::from(sign);
    if decpt > -4 && decpt <= 16 {
        if decpt <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-decpt) as usize));
            out.push_str(&digits);
        } else if decpt as usize >= digits.len() {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', decpt as usize - digits.len()));
            out.push_str(".0");
        } else {
            out.push_str(&digits[..decpt as usize]);
            out.push('.');
            out.push_str(&digits[decpt as usize..]);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        let _ = write!(out, "e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    out
}

pub(crate) fn write_str(out: &mut String, s: &str, ensure_ascii: bool) {
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
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            // ensure_ascii escapes everything outside ' '..='~' (so DEL too).
            c if ensure_ascii && (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{:04x}", unit);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python `str.isspace()` for one character: Unicode White_Space plus the four ASCII
/// information separators U+001C..U+001F (bidi class B/S), which Rust does not count.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()` (no argument).
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python truthiness of a decoded JSON value.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python `repr(s)` for a `str`.
///
/// Printability follows `str.isprintable()` for ASCII/Latin-1 exactly and for the usual
/// invisible characters above (format controls, separators, unassigned-free surrogates
/// cannot occur); exotic non-printables elsewhere are shown literally. Only used in an
/// error message.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !is_printable(c) => {
                let n = c as u32;
                if n < 0x100 {
                    let _ = write!(out, "\\x{n:02x}");
                } else if n < 0x10000 {
                    let _ = write!(out, "\\u{n:04x}");
                } else {
                    let _ = write!(out, "\\U{n:08x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn is_printable(c: char) -> bool {
    let n = c as u32;
    if n == 0x20 {
        return true;
    }
    !(n < 0x20
        || (0x7f..=0xa0).contains(&n)
        || n == 0xad
        || (0x2000..=0x200f).contains(&n)
        || (0x2028..=0x202f).contains(&n)
        || (0x205f..=0x206f).contains(&n)
        || n == 0x1680
        || n == 0x180e
        || n == 0x3000
        || n == 0xfeff
        || (0xfff9..=0xfffb).contains(&n)
        || (0xe000..=0xf8ff).contains(&n)
        || n >= 0xf0000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_print_like_python_repr() {
        let cases: &[(f64, &str)] = &[
            (1.0, "1.0"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (1.5e300, "1.5e+300"),
            (2.5e-320, "2.5e-320"),
            (123.456, "123.456"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (-40.0, "-40.0"),
            (1234567890123456.7, "1234567890123456.8"),
            (12345678901234567.0, "1.2345678901234568e+16"),
        ];
        for (f, want) in cases {
            assert_eq!(float_repr(*f), *want, "{f}");
        }
    }

    #[test]
    fn compact_ascii_matches_python() {
        // Python: json.dumps(..., ensure_ascii=True, separators=(",",":"))
        let v = json!({"f": "\u{7f}\u{e9}\u{1F600}\n\u{1}"});
        assert_eq!(
            dumps(&v, JsonStyle::COMPACT_ASCII),
            r#"{"f":"\u007f\u00e9\ud83d\ude00\n\u0001"}"#
        );
        let v = json!([
            true,
            null,
            -0.0,
            1.5e300,
            2.5e-320,
            1e-5,
            123456789012345678_i64
        ]);
        assert_eq!(
            dumps(&v, JsonStyle::COMPACT_ASCII),
            "[true,null,-0.0,1.5e+300,2.5e-320,1e-05,123456789012345678]"
        );
    }

    #[test]
    fn default_unicode_matches_python() {
        let v = json!({"a": "\u{e9}\u{7f}\u{1f}", "b": [1, 2.0]});
        assert_eq!(
            dumps(&v, JsonStyle::DEFAULT_UNICODE),
            "{\"a\": \"\u{e9}\u{7f}\\u001f\", \"b\": [1, 2.0]}"
        );
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(
            repr_str("abc'\"\n\0\u{e9}\u{a0}\u{200b}\u{1F600}"),
            r#"'abc\'"\n\x00é\xa0\u200b😀'"#
        );
        assert_eq!(repr_str("it's"), "\"it's\"");
    }

    #[test]
    fn python_whitespace() {
        assert!(is_space('\u{1c}'));
        assert!(is_space('\u{3000}'));
        assert!(!is_space('\u{200b}'));
        assert_eq!(strip("\u{1f} a b\u{85}"), "a b");
    }
}
