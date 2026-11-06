//! RFC 8785 JSON Canonicalization Scheme (JCS): the one text of a JSON value that
//! Bunko, Mokuro Reader and any other tool agree on, whatever separators, key order or
//! number spelling the file used. The content seal ([`crate::seal`]) hashes it.
//!
//! JCS is what `JSON.stringify` writes once every object's keys are sorted by their
//! UTF-16 code units, so a JavaScript reader needs no library to match it:
//!
//! * a number is written as ECMAScript's `Number::toString` ([`es_number`]), so an int
//!   and the float of the same value (`1`, `1.0`) are one number, and an integer past
//!   2^53 is the double JavaScript would read it as;
//! * a string escapes `"`, `\`, `\b \f \n \r \t` and the other C0 controls (`\u001f`,
//!   lowercase hex), nothing else (not `/`, not U+2028, not U+007F);
//! * no whitespace.
//!
//! NaN and the infinities have no JSON form, so a value holding one has no JCS.

use crate::json::{Value, write_string};
use crate::py::es_number;

/// The JCS text of `v`, or `None` when it holds a NaN or an infinity.
pub fn jcs(v: &Value) -> Option<String> {
    let mut out = String::new();
    write(v, &mut out)?;
    Some(out)
}

fn write(v: &Value, out: &mut String) -> Option<()> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        // Integers are JSON numbers like any other: past 2^53 they are written as the
        // double they round to, as JavaScript reads them.
        Value::Int(i) => out.push_str(&es_number(*i as f64)?),
        Value::BigInt(digits) => out.push_str(&es_number(digits.parse::<f64>().ok()?)?),
        Value::Float(f) => out.push_str(&es_number(*f)?),
        Value::Str(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out)?;
            }
            out.push(']');
        }
        Value::Object(items) => {
            // A parsed object has no duplicate keys (the last value won); one built in
            // code might, and keeps the last value the same way.
            let mut sorted: Vec<(Vec<u16>, &str, &Value)> = Vec::with_capacity(items.len());
            for (k, val) in items {
                let units: Vec<u16> = k.encode_utf16().collect();
                match sorted.iter_mut().find(|(_, key, _)| *key == k.as_str()) {
                    Some(slot) => slot.2 = val,
                    None => sorted.push((units, k, val)),
                }
            }
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            out.push('{');
            for (i, (_, k, val)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write(val, out)?;
            }
            out.push('}');
        }
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(text: &str) -> Option<String> {
        jcs(&Value::parse(text).unwrap())
    }

    #[test]
    fn numbers_are_written_as_javascript_does() {
        assert_eq!(
            canon("[1e21, 1e-7, 0.000001, 333333333.33333329, -0.0, 4.50, 1.0, 1, -0, 1e-05]")
                .unwrap(),
            "[1e+21,1e-7,0.000001,333333333.3333333,0,4.5,1,1,0,0.00001]"
        );
        // Past 2^53: the double JavaScript's JSON.parse reads (checked in Node).
        assert_eq!(
            canon("[9007199254740993, 12345678901234567890, 123456789012345680.0]").unwrap(),
            "[9007199254740992,12345678901234567000,123456789012345680]"
        );
    }

    #[test]
    fn non_finite_numbers_have_no_canonical_form() {
        assert_eq!(canon("[1, NaN]"), None);
        assert_eq!(canon(r#"{"a": {"b": Infinity}}"#), None);
        assert_eq!(canon("-Infinity"), None);
        // A BigInt past the double range is an infinity in JavaScript.
        assert_eq!(jcs(&Value::BigInt(format!("1{}", "0".repeat(400)))), None);
    }

    #[test]
    fn strings_escape_like_json_stringify() {
        assert_eq!(
            canon(r#""\u001f\u2028\u007f/\"\\\b\f\n\r\t\u0000日本語""#).unwrap(),
            "\"\\u001f\u{2028}\u{7f}/\\\"\\\\\\b\\f\\n\\r\\t\\u0000日本語\""
        );
    }

    #[test]
    fn keys_sort_by_utf16_code_units() {
        // Code point order would put U+FF61 before U+1F600; UTF-16 puts the surrogate
        // pair (0xD83D …) first, as JavaScript's sort does.
        assert_eq!(
            canon(r#"{"｡": 1, "😀": 2, "é": 3, "a": 4, "A": 5, "": 6}"#).unwrap(),
            r#"{"":6,"A":5,"a":4,"é":3,"😀":2,"｡":1}"#
        );
        // Nested objects too, and whitespace never survives.
        assert_eq!(
            canon("{ \"b\" : [ {\"y\":1, \"x\":2} ] , \"a\" : null }").unwrap(),
            r#"{"a":null,"b":[{"x":2,"y":1}]}"#
        );
    }

    #[test]
    fn duplicate_keys_keep_the_last_value() {
        assert_eq!(
            canon(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap(),
            r#"{"a":3,"b":2}"#
        );
        let built = Value::Object(vec![
            ("a".into(), Value::Int(1)),
            ("a".into(), Value::Int(2)),
        ]);
        assert_eq!(jcs(&built).unwrap(), r#"{"a":2}"#);
    }
}
