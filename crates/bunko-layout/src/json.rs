//! An ordered JSON value that reads and writes exactly like Python's `json`.
//!
//! Two reasons it exists instead of `serde_json::Value`:
//!
//! * key ORDER is part of the sidecar's bytes (the server rewrites a sidecar
//!   keeping every key where it was, `processor.py:899-984`), and the
//!   workspace's `serde_json` has no `preserve_order`;
//! * Python writes floats with `repr` (`1e-05`, `1e+16`) and the default
//!   `serde_json` float parser is only best-effort (not correctly rounded),
//!   so a sidecar or a raw dump would not survive a read/write unchanged.
//!
//! [`Value::dumps`] reproduces `json.dumps(obj, ensure_ascii=False)` with the
//! given separators; [`Value::parse`] reproduces `json.loads` (duplicate keys:
//! the last value wins, at the first key's position, like a `dict`).

use std::fmt;

use crate::py::float_repr;

/// A JSON value with Python's types: ints and floats are distinct, objects
/// keep insertion order.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    /// An integer outside `i64`, kept as its decimal digits (Python ints are
    /// unbounded and round-trip exactly).
    BigInt(String),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

/// The two separator styles bunko writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Separators {
    /// `json.dump`'s default with no indent: `", "` and `": "` (the runner).
    Default,
    /// `separators=(",", ":")` (the server's normalised rewrite).
    Compact,
}

impl Separators {
    fn item(self) -> &'static str {
        match self {
            Separators::Default => ", ",
            Separators::Compact => ",",
        }
    }
    fn key(self) -> &'static str {
        match self {
            Separators::Default => ": ",
            Separators::Compact => ":",
        }
    }
}

/// Why a document is not JSON Python would load.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid JSON at byte {offset}: {message}")]
pub struct ParseError {
    pub offset: usize,
    pub message: String,
}

impl Value {
    /// An empty object.
    pub fn object() -> Value {
        Value::Object(Vec::new())
    }

    pub fn str(s: impl Into<String>) -> Value {
        Value::Str(s.into())
    }

    /// The value under `key` of an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Object(items) => items.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Python `d[key] = value`: replace in place, or append at the end.
    /// No-op on a non-object.
    pub fn set(&mut self, key: &str, value: Value) {
        if let Value::Object(items) = self {
            if let Some(slot) = items.iter_mut().find(|(k, _)| k == key) {
                slot.1 = value;
            } else {
                items.push((key.to_string(), value));
            }
        }
    }

    /// Python `d.setdefault(key, value)`.
    pub fn set_default(&mut self, key: &str, value: Value) {
        if self.get(key).is_none() {
            self.set(key, value);
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(v) => Some(v),
            _ => None,
        }
    }

    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }

    /// The number as f64 (`float(x)` of an int or float).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::BigInt(s) => s.parse().ok(),
            Value::Float(f) => Some(*f),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    /// Python truthiness (used by the `x.get(k) or default` idiom).
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::BigInt(_) => true,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        }
    }

    /// `json.dumps(self, ensure_ascii=False, separators=...)`.
    pub fn dumps(&self, sep: Separators) -> String {
        let mut out = String::new();
        self.write_to(&mut out, sep);
        out
    }

    fn write_to(&self, out: &mut String, sep: Separators) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::BigInt(s) => out.push_str(s),
            Value::Float(f) => out.push_str(&float_json(*f)),
            Value::Str(s) => write_string(out, s),
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(sep.item());
                    }
                    item.write_to(out, sep);
                }
                out.push(']');
            }
            Value::Object(items) => {
                out.push('{');
                for (i, (k, v)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(sep.item());
                    }
                    write_string(out, k);
                    out.push_str(sep.key());
                    v.write_to(out, sep);
                }
                out.push('}');
            }
        }
    }

    /// `json.loads(text)`.
    pub fn parse(text: &str) -> Result<Value, ParseError> {
        let mut p = Parser {
            s: text.as_bytes(),
            text,
            i: 0,
        };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != p.s.len() {
            return Err(p.err("extra data"));
        }
        Ok(v)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.dumps(Separators::Default))
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::Str(s)
    }
}
impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}
impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}
impl From<f64> for Value {
    fn from(f: f64) -> Value {
        Value::Float(f)
    }
}

/// `float.__repr__` as `json` writes it (`NaN`, `Infinity` spelled the
/// JavaScript way).
fn float_json(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        }
    } else {
        float_repr(f)
    }
}

/// `json.encoder.py_encode_basestring` (ensure_ascii=False): `"`, `\` and the
/// C0 controls are escaped, nothing else (not `/`, not U+2028).
fn write_string(out: &mut String, s: &str) {
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
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn err(&self, message: &str) -> ParseError {
        ParseError {
            offset: self.i,
            message: message.to_string(),
        }
    }

    /// Python's json whitespace: space, tab, newline, carriage return.
    fn ws(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> bool {
        if self.s[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self) -> Result<Value, ParseError> {
        let Some(&b) = self.s.get(self.i) else {
            return Err(self.err("expecting value"));
        };
        match b {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Value::Str(self.string()?)),
            b'n' if self.eat("null") => Ok(Value::Null),
            b't' if self.eat("true") => Ok(Value::Bool(true)),
            b'f' if self.eat("false") => Ok(Value::Bool(false)),
            b'N' if self.eat("NaN") => Ok(Value::Float(f64::NAN)),
            b'I' if self.eat("Infinity") => Ok(Value::Float(f64::INFINITY)),
            b'-' if self.s[self.i..].starts_with(b"-Infinity") => {
                self.i += "-Infinity".len();
                Ok(Value::Float(f64::NEG_INFINITY))
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.err("expecting value")),
        }
    }

    fn object(&mut self) -> Result<Value, ParseError> {
        self.i += 1;
        let mut items: Vec<(String, Value)> = Vec::new();
        self.ws();
        if self.s.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Ok(Value::Object(items));
        }
        loop {
            self.ws();
            if self.s.get(self.i) != Some(&b'"') {
                return Err(self.err("expecting property name enclosed in double quotes"));
            }
            let key = self.string()?;
            self.ws();
            if self.s.get(self.i) != Some(&b':') {
                return Err(self.err("expecting ':' delimiter"));
            }
            self.i += 1;
            self.ws();
            let v = self.value()?;
            if let Some(slot) = items.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = v;
            } else {
                items.push((key, v));
            }
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(Value::Object(items));
                }
                _ => return Err(self.err("expecting ',' delimiter")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, ParseError> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.s.get(self.i) == Some(&b']') {
            self.i += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value()?);
            self.ws();
            match self.s.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.err("expecting ',' delimiter")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let Some(h) = self.text.get(self.i..self.i + 4) else {
            return Err(self.err("invalid \\uXXXX escape"));
        };
        let v = u32::from_str_radix(h, 16).map_err(|_| self.err("invalid \\uXXXX escape"))?;
        self.i += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.i += 1;
        let mut out = String::new();
        loop {
            let start = self.i;
            while self.i < self.s.len()
                && !matches!(self.s[self.i], b'"' | b'\\')
                && self.s[self.i] >= 0x20
            {
                self.i += 1;
            }
            out.push_str(&self.text[start..self.i]);
            match self.s.get(self.i) {
                Some(b'"') => {
                    self.i += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.i += 1;
                    let Some(&e) = self.s.get(self.i) else {
                        return Err(self.err("unterminated string"));
                    };
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xd800..0xdc00).contains(&cp)
                                && self.s[self.i..].starts_with(b"\\u")
                            {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xdc00..0xe000).contains(&lo) {
                                    cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00);
                                } else {
                                    self.i = save;
                                }
                            }
                            match char::from_u32(cp) {
                                Some(c) => out.push(c),
                                // Python keeps a lone surrogate; a Rust string cannot.
                                None => return Err(self.err("lone surrogate in \\u escape")),
                            }
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                Some(_) => return Err(self.err("invalid control character")),
                None => return Err(self.err("unterminated string")),
            }
        }
    }

    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.i;
        if self.s[self.i] == b'-' {
            self.i += 1;
        }
        let int_start = self.i;
        match self.s.get(self.i) {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                    self.i += 1;
                }
            }
            _ => return Err(self.err("expecting value")),
        }
        let _ = int_start;
        let mut is_float = false;
        if self.s.get(self.i) == Some(&b'.')
            && self.s.get(self.i + 1).is_some_and(|b| b.is_ascii_digit())
        {
            is_float = true;
            self.i += 1;
            while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                self.i += 1;
            }
        }
        if matches!(self.s.get(self.i), Some(b'e' | b'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.s.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.s.get(self.i).is_some_and(|b| b.is_ascii_digit()) {
                is_float = true;
                while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
                    self.i += 1;
                }
            } else {
                self.i = save;
            }
        }
        let lit = &self.text[start..self.i];
        if is_float {
            lit.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| self.err("bad number"))
        } else {
            match lit.parse::<i64>() {
                Ok(v) => Ok(Value::Int(v)),
                Err(_) => {
                    // Python normalises "-0" style only for ints that fit; big
                    // literals have no leading zeros by the grammar.
                    Ok(Value::BigInt(lit.to_string()))
                }
            }
        }
    }
}

impl serde::Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(i) => serializer.serialize_i64(*i),
            Value::BigInt(s) => match s.parse::<i128>() {
                Ok(v) => serializer.serialize_i128(v),
                Err(_) => serializer.serialize_f64(s.parse().unwrap_or(f64::NAN)),
            },
            Value::Float(f) => serializer.serialize_f64(*f),
            Value::Str(s) => serializer.serialize_str(s),
            Value::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Value::Object(items) => {
                let mut map = serializer.serialize_map(Some(items.len()))?;
                for (k, v) in items {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for Value {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Value;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_none<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_some<D2: serde::Deserializer<'de>>(self, d: D2) -> Result<Value, D2::Error> {
                serde::Deserialize::deserialize(d)
            }
            fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
                Ok(Value::Bool(b))
            }
            fn visit_i64<E>(self, i: i64) -> Result<Value, E> {
                Ok(Value::Int(i))
            }
            fn visit_u64<E>(self, u: u64) -> Result<Value, E> {
                Ok(i64::try_from(u)
                    .map(Value::Int)
                    .unwrap_or_else(|_| Value::BigInt(u.to_string())))
            }
            fn visit_f64<E>(self, f: f64) -> Result<Value, E> {
                Ok(Value::Float(f))
            }
            fn visit_str<E>(self, s: &str) -> Result<Value, E> {
                Ok(Value::Str(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> Result<Value, E> {
                Ok(Value::Str(s))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Value, A::Error> {
                let mut items = Vec::new();
                while let Some(v) = seq.next_element()? {
                    items.push(v);
                }
                Ok(Value::Array(items))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Value, A::Error> {
                let mut out = Value::object();
                while let Some((k, v)) = map.next_entry::<String, Value>()? {
                    out.set(&k, v);
                }
                Ok(out)
            }
        }
        deserializer.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dumps_like_python() {
        let v =
            Value::parse(r#"{"a": [1, 2.5, 1e-05, true, null], "b": "x\"\\\n\u0001/é", "a": 3}"#)
                .unwrap();
        assert_eq!(
            v.dumps(Separators::Default),
            r#"{"a": 3, "b": "x\"\\\n\u0001/é"}"#
        );
        assert_eq!(
            v.dumps(Separators::Compact),
            r#"{"a":3,"b":"x\"\\\n\u0001/é"}"#
        );
        let f = Value::parse("[1E5, -0.0, 1e16, 123456789012345678901234567890]").unwrap();
        assert_eq!(
            f.dumps(Separators::Default),
            "[100000.0, -0.0, 1e+16, 123456789012345678901234567890]"
        );
    }

    #[test]
    fn rejects_trailing_data_and_controls() {
        assert!(Value::parse("{} x").is_err());
        assert!(Value::parse("\"a\u{1}\"").is_err());
        assert!(Value::parse("01").is_err());
    }
}
