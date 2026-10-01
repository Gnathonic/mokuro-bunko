//! JSON exactly as Python's `json` module reads and writes it.
//!
//! Two reasons not to use serde_json here:
//!
//! * **Reading** `.mokuro` sidecars and DB columns must accept what
//!   `json.loads` accepts, or a sidecar 0.5.2 parsed (uuid, page count, hash)
//!   would degrade to image-only: `NaN`/`Infinity`/`-Infinity`, duplicate keys
//!   (last value wins, first position kept), integers of any size.
//! * **Writing** must be byte-identical to `json.dumps`: Python float `repr`
//!   (`12.0`, `1e-05`, `1e+16`), int-vs-float preserved, `ensure_ascii` and
//!   separator variants.
//!
//! Deliberate deviation: a lone UTF-16 surrogate escape (`"\ud800"`), which
//! Python keeps as a lone surrogate code point, cannot live in a Rust
//! `String`; it is decoded as U+FFFD. Only the bytes of `volume_uuid` /
//! `version` strings of such a (hand-damaged) sidecar can differ from 0.5.2's
//! output; the sidecar hash is taken over the raw bytes and is unaffected.

use std::cmp::Ordering;
use std::fmt::Write as _;

/// A JSON number as Python holds it: `int` (arbitrary precision) or `float`.
#[derive(Debug, Clone)]
pub enum JsonNum {
    Int(i64),
    /// An integer outside `i64`, as its canonical decimal text (`-` sign, no
    /// leading zeros) — exactly Python's `repr` of the parsed `int`.
    BigInt(Box<str>),
    Float(f64),
}

impl JsonNum {
    /// Python truthiness (`0`, `0.0`, `-0.0` are falsy).
    pub fn is_truthy(&self) -> bool {
        match self {
            JsonNum::Int(value) => *value != 0,
            JsonNum::BigInt(_) => true,
            JsonNum::Float(value) => *value != 0.0,
        }
    }

    /// `value > 0` in Python.
    pub fn is_positive(&self) -> bool {
        match self {
            JsonNum::Int(value) => *value > 0,
            JsonNum::BigInt(text) => !text.starts_with('-'),
            JsonNum::Float(value) => *value > 0.0,
        }
    }

    /// `math.isfinite(value)`; a big int beyond float range raises
    /// `OverflowError` in Python, reported here as `None`.
    pub fn is_finite(&self) -> Option<bool> {
        match self {
            JsonNum::Int(_) => Some(true),
            JsonNum::BigInt(text) => {
                let value: f64 = text.parse().unwrap_or(f64::INFINITY);
                value.is_finite().then_some(true)
            }
            JsonNum::Float(value) => Some(value.is_finite()),
        }
    }

    /// The value as a float (`float(x)`), lossy for big ints.
    pub fn as_f64(&self) -> f64 {
        match self {
            JsonNum::Int(value) => *value as f64,
            JsonNum::BigInt(text) => text.parse().unwrap_or(f64::NAN),
            JsonNum::Float(value) => *value,
        }
    }

    pub fn is_int(&self) -> bool {
        !matches!(self, JsonNum::Float(_))
    }

    /// Exact integer text of an int, or of an integral float.
    fn exact_integer_text(&self) -> Option<String> {
        match self {
            JsonNum::Int(value) => Some(value.to_string()),
            JsonNum::BigInt(text) => Some(text.to_string()),
            JsonNum::Float(value) => {
                (value.is_finite() && value.fract() == 0.0).then(|| {
                    // `{:.0}` prints the exact decimal expansion of an integral f64.
                    let text = format!("{value:.0}");
                    if text == "-0" { "0".to_owned() } else { text }
                })
            }
        }
    }

    /// Python `==` between numbers (`12 == 12.0`, exact for big values).
    pub fn py_eq(&self, other: &JsonNum) -> bool {
        match (self, other) {
            (JsonNum::Float(a), JsonNum::Float(b)) => a == b,
            (JsonNum::Int(a), JsonNum::Int(b)) => a == b,
            _ => match (self.exact_integer_text(), other.exact_integer_text()) {
                (Some(a), Some(b)) => a == b,
                _ => false,
            },
        }
    }
}

/// An object, in insertion order (Python `dict` semantics).
#[derive(Debug, Clone, Default)]
pub struct JsonObject(pub Vec<(String, JsonValue)>);

impl JsonObject {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.iter().any(|(k, _)| k == key)
    }

    /// `d[key] = value`: replaces in place, else appends.
    pub fn insert(&mut self, key: impl Into<String>, value: JsonValue) {
        let key = key.into();
        if let Some(slot) = self.0.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = value;
        } else {
            self.0.push((key, value));
        }
    }

    /// `d.pop(key, None)`.
    pub fn remove(&mut self, key: &str) -> Option<JsonValue> {
        let index = self.0.iter().position(|(k, _)| k == key)?;
        Some(self.0.remove(index).1)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &JsonValue)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Python dict `==`: same keys, `py_eq` values, order ignored.
    pub fn py_eq(&self, other: &JsonObject) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .all(|(key, value)| other.get(key).is_some_and(|theirs| value.py_eq(theirs)))
    }
}

/// A parsed JSON value.
#[derive(Debug, Clone)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Num(JsonNum),
    Str(String),
    Array(Vec<JsonValue>),
    Object(JsonObject),
}

impl JsonValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&JsonObject> {
        match self {
            JsonValue::Object(object) => Some(object),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(items) => Some(items),
            _ => None,
        }
    }

    /// A JSON number that is NOT a bool (Python's `isinstance(x, (int, float))
    /// and not isinstance(x, bool)`).
    pub fn as_num(&self) -> Option<&JsonNum> {
        match self {
            JsonValue::Num(num) => Some(num),
            _ => None,
        }
    }

    /// Python truthiness.
    pub fn is_truthy(&self) -> bool {
        match self {
            JsonValue::Null => false,
            JsonValue::Bool(value) => *value,
            JsonValue::Num(num) => num.is_truthy(),
            JsonValue::Str(text) => !text.is_empty(),
            JsonValue::Array(items) => !items.is_empty(),
            JsonValue::Object(object) => !object.is_empty(),
        }
    }

    /// Python `==` (`True == 1`, `1 == 1.0`, dicts order-insensitive).
    pub fn py_eq(&self, other: &JsonValue) -> bool {
        match (self, other) {
            (JsonValue::Null, JsonValue::Null) => true,
            (JsonValue::Str(a), JsonValue::Str(b)) => a == b,
            (JsonValue::Array(a), JsonValue::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.py_eq(y))
            }
            (JsonValue::Object(a), JsonValue::Object(b)) => a.py_eq(b),
            (JsonValue::Bool(a), JsonValue::Bool(b)) => a == b,
            (JsonValue::Bool(a), JsonValue::Num(n)) | (JsonValue::Num(n), JsonValue::Bool(a)) => {
                n.py_eq(&JsonNum::Int(i64::from(*a)))
            }
            (JsonValue::Num(a), JsonValue::Num(b)) => a.py_eq(b),
            _ => false,
        }
    }
}

impl From<&str> for JsonValue {
    fn from(value: &str) -> Self {
        JsonValue::Str(value.to_owned())
    }
}

impl From<String> for JsonValue {
    fn from(value: String) -> Self {
        JsonValue::Str(value)
    }
}

impl From<i64> for JsonValue {
    fn from(value: i64) -> Self {
        JsonValue::Num(JsonNum::Int(value))
    }
}

impl From<JsonNum> for JsonValue {
    fn from(value: JsonNum) -> Self {
        JsonValue::Num(value)
    }
}

impl From<JsonObject> for JsonValue {
    fn from(value: JsonObject) -> Self {
        JsonValue::Object(value)
    }
}

impl<T: Into<JsonValue>> From<Option<T>> for JsonValue {
    fn from(value: Option<T>) -> Self {
        value.map_or(JsonValue::Null, Into::into)
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Why a document failed to parse (`json.JSONDecodeError`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid JSON at byte {position}: {message}")]
pub struct JsonError {
    pub position: usize,
    pub message: &'static str,
}

/// Parse switches.
#[derive(Debug, Clone, Copy)]
pub struct ParseOptions {
    /// Accept `NaN`, `Infinity`, `-Infinity` (Python's default). The PUT
    /// validator turns this off (`parse_constant=_reject_constant`).
    pub allow_constants: bool,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            allow_constants: true,
        }
    }
}

/// Nesting beyond this is refused (Python raises `RecursionError` near 1000).
const MAX_DEPTH: usize = 512;

/// `json.loads(text)` with Python's default leniency.
pub fn parse(text: &str) -> Result<JsonValue, JsonError> {
    parse_with(text, ParseOptions::default())
}

/// `json.loads` with explicit options.
pub fn parse_with(text: &str, options: ParseOptions) -> Result<JsonValue, JsonError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text,
        pos: 0,
        options,
        depth: 0,
    };
    parser.skip_ws();
    let value = parser.value()?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("Extra data"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
    options: ParseOptions,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, message: &'static str) -> JsonError {
        JsonError {
            position: self.pos,
            message,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.bytes.get(self.pos) {
            self.pos += 1;
        }
    }

    fn starts_with(&self, literal: &str) -> bool {
        self.bytes[self.pos..].starts_with(literal.as_bytes())
    }

    fn value(&mut self) -> Result<JsonValue, JsonError> {
        let Some(&byte) = self.bytes.get(self.pos) else {
            return Err(self.error("Expecting value"));
        };
        match byte {
            b'"' => Ok(JsonValue::Str(self.string()?)),
            b'{' => self.object(),
            b'[' => self.array(),
            b'n' if self.starts_with("null") => {
                self.pos += 4;
                Ok(JsonValue::Null)
            }
            b't' if self.starts_with("true") => {
                self.pos += 4;
                Ok(JsonValue::Bool(true))
            }
            b'f' if self.starts_with("false") => {
                self.pos += 5;
                Ok(JsonValue::Bool(false))
            }
            b'N' if self.starts_with("NaN") => self.constant(3, f64::NAN),
            b'I' if self.starts_with("Infinity") => self.constant(8, f64::INFINITY),
            b'-' if self.starts_with("-Infinity") => self.constant(9, f64::NEG_INFINITY),
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.error("Expecting value")),
        }
    }

    fn constant(&mut self, len: usize, value: f64) -> Result<JsonValue, JsonError> {
        if !self.options.allow_constants {
            return Err(self.error("unsupported JSON constant"));
        }
        self.pos += len;
        Ok(JsonValue::Num(JsonNum::Float(value)))
    }

    fn enter(&mut self) -> Result<(), JsonError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        Ok(())
    }

    fn object(&mut self) -> Result<JsonValue, JsonError> {
        self.enter()?;
        self.pos += 1;
        let mut object = JsonObject::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Object(object));
        }
        loop {
            if self.bytes.get(self.pos) != Some(&b'"') {
                return Err(self.error("Expecting property name enclosed in double quotes"));
            }
            let key = self.string()?;
            self.skip_ws();
            if self.bytes.get(self.pos) != Some(&b':') {
                return Err(self.error("Expecting ':' delimiter"));
            }
            self.pos += 1;
            self.skip_ws();
            let value = self.value()?;
            object.insert(key, value);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("Expecting ',' delimiter")),
            }
        }
        self.depth -= 1;
        Ok(JsonValue::Object(object))
    }

    fn array(&mut self) -> Result<JsonValue, JsonError> {
        self.enter()?;
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_ws();
        if self.bytes.get(self.pos) == Some(&b']') {
            self.pos += 1;
            self.depth -= 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_ws();
            match self.bytes.get(self.pos) {
                Some(b',') => {
                    self.pos += 1;
                    self.skip_ws();
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("Expecting ',' delimiter")),
            }
        }
        self.depth -= 1;
        Ok(JsonValue::Array(items))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.error("Invalid \\uXXXX escape"))?;
        let mut value = 0u32;
        for &digit in digits {
            let nibble = (digit as char)
                .to_digit(16)
                .ok_or_else(|| self.error("Invalid \\uXXXX escape"))?;
            value = value * 16 + nibble;
        }
        self.pos += 4;
        Ok(value)
    }

    fn string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(&byte) = self.bytes.get(self.pos) {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // Splits only happen at ASCII bytes, so this is a char boundary.
            out.push_str(&self.text[start..self.pos]);
            match self.bytes.get(self.pos) {
                None => return Err(self.error("Unterminated string starting at")),
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let Some(&escape) = self.bytes.get(self.pos) else {
                        return Err(self.error("Unterminated string starting at"));
                    };
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let unit = self.hex4()?;
                            if (0xD800..0xDC00).contains(&unit) && self.starts_with("\\u") {
                                let save = self.pos;
                                self.pos += 2;
                                let low = self.hex4()?;
                                if (0xDC00..0xE000).contains(&low) {
                                    let cp = 0x10000 + ((unit - 0xD800) << 10) + (low - 0xDC00);
                                    out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                                    continue;
                                }
                                self.pos = save;
                            }
                            out.push(char::from_u32(unit).unwrap_or('\u{fffd}'));
                        }
                        _ => {
                            self.pos -= 1;
                            return Err(self.error("Invalid \\escape"));
                        }
                    }
                }
                Some(_) => return Err(self.error("Invalid control character at")),
            }
        }
    }

    fn number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.pos;
        if self.bytes[self.pos] == b'-' {
            self.pos += 1;
        }
        match self.bytes.get(self.pos) {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while let Some(b'0'..=b'9') = self.bytes.get(self.pos) {
                    self.pos += 1;
                }
            }
            _ => {
                self.pos = start;
                return Err(self.error("Expecting value"));
            }
        }
        let mut is_float = false;
        if self.bytes.get(self.pos) == Some(&b'.')
            && matches!(self.bytes.get(self.pos + 1), Some(b'0'..=b'9'))
        {
            is_float = true;
            self.pos += 1;
            while let Some(b'0'..=b'9') = self.bytes.get(self.pos) {
                self.pos += 1;
            }
        }
        if let Some(b'e' | b'E') = self.bytes.get(self.pos) {
            let mut probe = self.pos + 1;
            if let Some(b'+' | b'-') = self.bytes.get(probe) {
                probe += 1;
            }
            if matches!(self.bytes.get(probe), Some(b'0'..=b'9')) {
                is_float = true;
                self.pos = probe;
                while let Some(b'0'..=b'9') = self.bytes.get(self.pos) {
                    self.pos += 1;
                }
            }
        }
        let literal = &self.text[start..self.pos];
        if is_float {
            let value: f64 = literal.parse().map_err(|_| self.error("Expecting value"))?;
            return Ok(JsonValue::Num(JsonNum::Float(value)));
        }
        Ok(JsonValue::Num(int_from_literal(literal)))
    }
}

/// A JSON integer literal as Python's `int` holds it.
fn int_from_literal(literal: &str) -> JsonNum {
    if let Ok(value) = literal.parse::<i64>() {
        return JsonNum::Int(value);
    }
    // JSON integers have no leading zeros, so the literal is already canonical.
    JsonNum::BigInt(literal.into())
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// `json.dumps` switches.
#[derive(Debug, Clone, Copy)]
pub struct DumpOptions {
    pub ensure_ascii: bool,
    pub item_separator: &'static str,
    pub key_separator: &'static str,
    pub allow_nan: bool,
}

impl DumpOptions {
    /// `json.dumps(x, ensure_ascii=False, separators=(",", ":"), allow_nan=False)`
    /// — the compiled metadata files.
    pub const COMPACT_UTF8: DumpOptions = DumpOptions {
        ensure_ascii: false,
        item_separator: ",",
        key_separator: ":",
        allow_nan: false,
    };
    /// `json.dumps(x, ensure_ascii=False)` — the DB's JSON columns and cache rows.
    pub const DEFAULT_UTF8: DumpOptions = DumpOptions {
        ensure_ascii: false,
        item_separator: ", ",
        key_separator: ": ",
        allow_nan: true,
    };
    /// `json.dumps(x)` — Python's default (ASCII-escaped, spaced), used by the
    /// catalog/home JSON APIs.
    pub const DEFAULT: DumpOptions = DumpOptions {
        ensure_ascii: true,
        item_separator: ", ",
        key_separator: ": ",
        allow_nan: true,
    };
}

/// `allow_nan=False` met a non-finite float.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Out of range float values are not JSON compliant")]
pub struct NonFiniteError;

/// `json.dumps(value, ...)`.
pub fn dumps(value: &JsonValue, options: DumpOptions) -> Result<String, NonFiniteError> {
    let mut out = String::new();
    write_value(&mut out, value, options)?;
    Ok(out)
}

fn write_value(
    out: &mut String,
    value: &JsonValue,
    options: DumpOptions,
) -> Result<(), NonFiniteError> {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(true) => out.push_str("true"),
        JsonValue::Bool(false) => out.push_str("false"),
        JsonValue::Num(num) => write_num(out, num, options.allow_nan)?,
        JsonValue::Str(text) => write_str(out, text, options.ensure_ascii),
        JsonValue::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(options.item_separator);
                }
                write_value(out, item, options)?;
            }
            out.push(']');
        }
        JsonValue::Object(object) => {
            out.push('{');
            for (index, (key, item)) in object.0.iter().enumerate() {
                if index > 0 {
                    out.push_str(options.item_separator);
                }
                write_str(out, key, options.ensure_ascii);
                out.push_str(options.key_separator);
                write_value(out, item, options)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_num(out: &mut String, num: &JsonNum, allow_nan: bool) -> Result<(), NonFiniteError> {
    match num {
        JsonNum::Int(value) => {
            let _ = write!(out, "{value}");
        }
        JsonNum::BigInt(text) => out.push_str(text),
        JsonNum::Float(value) => {
            if value.is_finite() {
                out.push_str(&float_repr(*value));
            } else if !allow_nan {
                return Err(NonFiniteError);
            } else if value.is_nan() {
                out.push_str("NaN");
            } else if *value > 0.0 {
                out.push_str("Infinity");
            } else {
                out.push_str("-Infinity");
            }
        }
    }
    Ok(())
}

/// A JSON string literal, escaped as `json.dumps` does.
pub fn write_str(out: &mut String, text: &str, ensure_ascii: bool) {
    out.push('"');
    for c in text.chars() {
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
            c if ensure_ascii && !(' '..='~').contains(&c) => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Python's `repr(float)` for a finite value: shortest round-trip digits,
/// fixed notation for decimal exponents in `[-4, 16)`, else `1e-05` / `1.5e+16`
/// style, always a `.0` on integral fixed output.
pub fn float_repr(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    // `{:e}` gives the shortest round-trip digits: `d[.ddd]e<exp>`. When two
    // strings of that length round-trip, Python (David Gay's dtoa) takes the
    // one nearest the exact value, ties to even; Rust's precision mode rounds
    // the exact value exactly that way, so prefer it whenever it round-trips.
    let shortest = format!("{value:e}");
    let length = shortest
        .split('e')
        .next()
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_digit)
        .count();
    let nearest = format!("{value:.*e}", length.saturating_sub(1));
    let sci = if nearest.parse::<f64>() == Ok(value) {
        nearest
    } else {
        shortest
    };
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (negative, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, mantissa),
    };
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let decpt = exponent + 1; // position of the decimal point after the first digit
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if -4 < decpt && decpt <= 16 {
        if decpt <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', (-decpt) as usize));
            out.push_str(&digits);
        } else {
            let point = decpt as usize;
            if digits.len() <= point {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', point - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..point]);
                out.push('.');
                out.push_str(&digits[point..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let shown = decpt - 1;
        let sign = if shown < 0 { '-' } else { '+' };
        let _ = write!(out, "e{sign}{:02}", shown.unsigned_abs());
    }
    out
}

/// Python `str < str` (code point order) — Rust's `str` ordering on UTF-8 is
/// the same order, spelled out for the readers of the sort keys.
pub fn py_str_cmp(a: &str, b: &str) -> Ordering {
    a.cmp(b)
}

/// `_load_json_object(raw, fallback)` from `database.py`: an object (or list)
/// column, falling back on anything else rather than failing.
pub fn load_json_object(raw: Option<&str>) -> JsonObject {
    match raw.map(parse) {
        Some(Ok(JsonValue::Object(object))) => object,
        _ => JsonObject::new(),
    }
}

/// `_load_json_object(raw, [])`.
pub fn load_json_array(raw: Option<&str>) -> Vec<JsonValue> {
    match raw.map(parse) {
        Some(Ok(JsonValue::Array(items))) => items,
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_repr_matches_python() {
        let cases = [
            (12.0, "12.0"),
            (12.5, "12.5"),
            (1e-5, "1e-05"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1234567890123456.0, "1234567890123456.0"),
            (1.5e16, "1.5e+16"),
            (-40.0, "-40.0"),
            (0.1, "0.1"),
            (1700000000.123456, "1700000000.123456"),
            (1e22, "1e+22"),
            (5e-324, "5e-324"),
            (-0.0, "-0.0"),
            (123456789012345678.0, "1.2345678901234568e+17"),
        ];
        for (value, expected) in cases {
            assert_eq!(float_repr(value), expected, "{value}");
        }
    }

    #[test]
    fn lenient_parse() {
        let value = parse(r#"{"a": 1, "a": 2, "b": NaN, "c": -Infinity, "d": 1e400, "e": 123456789012345678901234567890}"#).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 5);
        assert!(object.get("a").unwrap().py_eq(&JsonValue::from(2)));
        assert_eq!(object.0[0].0, "a");
        assert!(
            matches!(object.get("d"), Some(JsonValue::Num(JsonNum::Float(v))) if v.is_infinite())
        );
        assert!(matches!(
            object.get("e"),
            Some(JsonValue::Num(JsonNum::BigInt(_)))
        ));
        assert!(
            parse_with(
                "NaN",
                ParseOptions {
                    allow_constants: false
                }
            )
            .is_err()
        );
        assert!(parse("[1,]").is_err());
        assert!(parse("\u{feff}{}").is_err());
        assert!(parse("\"a\nb\"").is_err());
        assert_eq!(
            parse(&format!(r#""{0}d83d{0}de00""#, "\\u"))
                .unwrap()
                .as_str(),
            Some("\u{1f600}")
        );
    }

    #[test]
    fn dumps_variants() {
        let value = parse(&format!(
            r#"{{"k": ["{0}65e5{0}672c", 1.0, 2, null, true, "{0}007f{0}2028"]}}"#,
            "\\u"
        ))
        .unwrap();
        assert_eq!(
            dumps(&value, DumpOptions::COMPACT_UTF8).unwrap(),
            "{\"k\":[\"日本\",1.0,2,null,true,\"\u{7f}\u{2028}\"]}"
        );
        assert_eq!(
            dumps(&value, DumpOptions::DEFAULT).unwrap(),
            format!(
                r#"{{"k": ["{0}65e5{0}672c", 1.0, 2, null, true, "{0}007f{0}2028"]}}"#,
                "\\u"
            )
        );
        assert!(
            dumps(
                &JsonValue::Num(JsonNum::Float(f64::NAN)),
                DumpOptions::COMPACT_UTF8
            )
            .is_err()
        );
    }

    #[test]
    fn python_equality() {
        assert!(JsonNum::Int(12).py_eq(&JsonNum::Float(12.0)));
        assert!(
            !JsonNum::BigInt("1000000000000000000000000000000".into()).py_eq(&JsonNum::Float(1e30))
        );
        assert!(JsonValue::Bool(true).py_eq(&JsonValue::from(1)));
    }
}
