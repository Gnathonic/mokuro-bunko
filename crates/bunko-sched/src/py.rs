//! Python 3.12 semantics the 0.5.2 numbers depend on.
//!
//! Every ETA, percentage and verdict the UI shows went through `round()`,
//! `int()`, `sum()`, `f"{x:.0f}"` or `datetime.isoformat()` in 0.5.2. Those
//! are not the obvious Rust operations (`round()` is half-to-even, `sum()` of
//! floats is compensated since 3.12, `fromtimestamp` rounds to microseconds
//! half-to-even), so they live here once, and every module uses these.
//!
//! The `as_*` helpers mirror the defensive `_as_int` / `_as_float` /
//! `_number` parsers of the Python modules over a `serde_json::Value`: a bool
//! is never a number, anything that is not a number is `None`.

use serde_json::{Map, Value};

/// A JSON object, as the loosely typed cards and records are.
pub type Object = Map<String, Value>;

/// `round(x)` for a float: nearest integer, ties to even, as an `i64`.
///
/// Out-of-range values saturate (Python would return a big int or raise).
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// `round(x, ndigits)` for a float: correctly rounded on the exact binary
/// value, ties to even — which is exactly what Rust's fixed-precision
/// formatter does, so the formatted string is parsed back.
pub fn round_to(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.ndigits$}").parse().unwrap_or(x)
}

/// `f"{x:.Nf}"`: fixed notation, correctly rounded, ties to even.
pub fn fmt_fixed(x: f64, ndigits: usize) -> String {
    if x.is_nan() {
        return "nan".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    format!("{x:.ndigits$}")
}

/// `f"{x:.0%}"`.
pub fn fmt_percent0(x: f64) -> String {
    format!("{}%", fmt_fixed(x * 100.0, 0))
}

/// `sum(floats)` as CPython 3.12 computes it: Neumaier-compensated.
pub fn sum(values: impl IntoIterator<Item = f64>) -> f64 {
    let mut total = 0.0f64;
    let mut c = 0.0f64;
    for x in values {
        let t = total + x;
        if total.abs() >= x.abs() {
            c += (total - t) + x;
        } else {
            c += (x - t) + total;
        }
        total = t;
    }
    if c != 0.0 && c.is_finite() {
        total += c;
    }
    total
}

/// `sum(values) / len(values)`, 0.0 for none (`congestion._mean`).
pub fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        sum(values.iter().copied()) / values.len() as f64
    }
}

/// `statistics.median` of whole numbers, as `int(round(median))`.
pub fn median_int(values: &[i64]) -> Option<i64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    if n % 2 == 1 {
        Some(sorted[n / 2])
    } else {
        // Python: (a + b) / 2 is a true division (a float), then round().
        let a = sorted[n / 2 - 1] as f64;
        let b = sorted[n / 2] as f64;
        Some(round_int((a + b) / 2.0))
    }
}

/// `repr(float)`: the shortest string that round-trips, laid out the way
/// CPython does (fixed between 1e-4 and 1e16, else `1e+16` / `1e-05`).
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf" } else { "-inf" }.to_owned();
    }
    // Rust's `{:e}` gives the same shortest round-trip digits, e.g. "-1.2345e-7".
    let sci = format!("{x:e}");
    let (sign, rest) = match sci.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", sci.as_str()),
    };
    let (mantissa, exp) = rest.split_once('e').unwrap_or((rest, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let decpt = exp + 1;
    let n = digits.len() as i32;
    let body = if decpt <= -4 || decpt > 16 {
        let mut m = digits[..1].to_owned();
        if digits.len() > 1 {
            m.push('.');
            m.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs())
    } else if decpt <= 0 {
        format!("0.{}{digits}", "0".repeat((-decpt) as usize))
    } else if decpt >= n {
        format!("{digits}{}.0", "0".repeat((decpt - n) as usize))
    } else {
        format!(
            "{}.{}",
            &digits[..decpt as usize],
            &digits[decpt as usize..]
        )
    };
    format!("{sign}{body}")
}

/// `_as_float`: a JSON number as `f64`; bools and everything else are None.
pub fn as_float(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// `_as_int`: a JSON number truncated towards zero; bools and the rest None.
pub fn as_int(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else {
                n.as_f64().map(|f| f.trunc() as i64)
            }
        }
        _ => None,
    }
}

/// `engine_runner._number`: a JSON number as `f64`, 0.0 for anything else.
pub fn number_or_zero(value: Option<&Value>) -> f64 {
    as_float(value).unwrap_or(0.0)
}

/// `throughput._number`: like [`as_float`] but NaN is None too.
pub fn number_not_nan(value: Option<&Value>) -> Option<f64> {
    as_float(value).filter(|f| !f.is_nan())
}

/// Python truthiness of a JSON value.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `int(value)` for a value read from a JSON file, where Python would accept
/// a number, a bool or a numeric string. Anything Python would raise on gives
/// `None` (the caller supplies the fallback 0.5.2 never needed).
pub fn py_int(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Bool(b)) => Some(i64::from(*b)),
        Some(Value::Number(_)) => as_int(value),
        Some(Value::String(s)) => s.trim().replace('_', "").parse::<i64>().ok(),
        _ => None,
    }
}

/// `float(value)` for a value read from a JSON file (number, bool, numeric
/// string); `None` where Python would raise.
pub fn py_float(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => {
            let t = s.trim().to_ascii_lowercase();
            match t.as_str() {
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                "nan" | "+nan" | "-nan" => Some(f64::NAN),
                _ => t.replace('_', "").parse::<f64>().ok(),
            }
        }
        _ => None,
    }
}

/// `str(value)` / `f"{value}"` for a JSON value.
pub fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_owned(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => py_repr(other),
    }
}

/// `repr()` of a JSON value as Python would print the loaded object.
pub fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(n) => {
            if n.is_f64() {
                float_repr(n.as_f64().unwrap_or(0.0))
            } else {
                n.to_string()
            }
        }
        Value::String(s) => {
            let quote = if s.contains('\'') && !s.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut out = String::new();
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
                    c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                        out.push_str(&format!("\\x{:02x}", c as u32));
                    }
                    c => out.push(c),
                }
            }
            out.push(quote);
            out
        }
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", py_repr(&Value::String(k.clone())), py_repr(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// First `n` characters (code points) of `s`, as Python's `s[:n]`.
pub fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A finite float as a JSON number (non-finite ones become null, which is
/// what a reader can parse; 0.5.2 would have written `NaN`/`Infinity`).
pub fn float_value(x: f64) -> Value {
    serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
}

/// `Some(x)` as a JSON float, `None` as null.
pub fn opt_float_value(x: Option<f64>) -> Value {
    x.map_or(Value::Null, float_value)
}

/// `Some(i)` as a JSON int, `None` as null.
pub fn opt_int_value(x: Option<i64>) -> Value {
    x.map_or(Value::Null, Value::from)
}

/// `Some(s)` as a JSON string, `None` as null.
pub fn opt_str_value(x: Option<&str>) -> Value {
    x.map_or(Value::Null, |s| Value::String(s.to_owned()))
}

// --- time ------------------------------------------------------------------

/// `datetime.fromtimestamp(epoch, UTC).isoformat(timespec="seconds")` with
/// `+00:00` written as `Z`: `YYYY-MM-DDTHH:MM:SSZ`.
///
/// `fromtimestamp` first rounds to whole microseconds (half to even), so an
/// epoch a hair under a second boundary lands on the next second, exactly as
/// 0.5.2 printed it.
pub fn iso_utc(epoch: f64) -> String {
    let (seconds, _micros) = timestamp_to_seconds_micros(epoch);
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60
    )
}

/// CPython's `_PyTime_ObjectToTimeval(..., ROUND_HALF_EVEN)`.
fn timestamp_to_seconds_micros(epoch: f64) -> (i64, i64) {
    let mut intpart = epoch.trunc();
    let mut floatpart = (epoch - intpart) * 1e6;
    let rounded = floatpart.round();
    floatpart = if (floatpart - rounded).abs() == 0.5 {
        2.0 * (floatpart / 2.0).round()
    } else {
        rounded
    };
    if floatpart >= 1e6 {
        floatpart -= 1e6;
        intpart += 1.0;
    } else if floatpart < 0.0 {
        floatpart += 1e6;
        intpart -= 1.0;
    }
    (intpart as i64, floatpart as i64)
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (year, month, day) → days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `datetime.fromisoformat(text.replace("Z", "+00:00")).timestamp()`.
///
/// Accepts `YYYY-MM-DD` optionally followed by `T`/space and
/// `HH[:MM[:SS[.fff…]]]` and an offset `±HH[:MM[:SS]]` or `Z`. A string
/// without an offset is read as UTC (0.5.2 read it as the server's local
/// time; every ETA it wrote carries `Z`). `None` where Python raises.
pub fn parse_iso_timestamp(text: &str) -> Option<f64> {
    let text = text.replace('Z', "+00:00");
    let b = text.as_bytes();
    let digits = |s: &[u8]| -> Option<i64> {
        if s.is_empty() || !s.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(s).ok()?.parse().ok()
    };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let year = digits(&b[0..4])?;
    let month = digits(&b[5..7])? as u32;
    let day = digits(&b[8..10])? as u32;
    if year < 1 || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let mut seconds = days_from_civil(year, month, day) * 86_400;
    let mut micros: i64 = 0;
    let rest = &b[10..];
    if !rest.is_empty() {
        // One separator character, then the time.
        let time = std::str::from_utf8(&rest[1..]).ok()?;
        let (clock, offset) = match time.find(['+', '-']) {
            Some(i) => (&time[..i], Some(&time[i..])),
            None => (time, None),
        };
        let (hms, frac) = match clock.split_once('.') {
            Some((a, f)) => (a, Some(f)),
            None => (clock, None),
        };
        let parts: Vec<&str> = hms.split(':').collect();
        if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) {
            return None;
        }
        let h = digits(parts[0].as_bytes())?;
        let mi = if parts.len() > 1 {
            digits(parts[1].as_bytes())?
        } else {
            0
        };
        let s = if parts.len() > 2 {
            digits(parts[2].as_bytes())?
        } else {
            0
        };
        if h > 23 || mi > 59 || s > 59 {
            return None;
        }
        if let Some(f) = frac {
            if parts.len() != 3 || f.is_empty() || !f.bytes().all(|c| c.is_ascii_digit()) {
                return None;
            }
            let six: String = f.chars().chain(std::iter::repeat('0')).take(6).collect();
            micros = six.parse().ok()?;
        }
        seconds += h * 3600 + mi * 60 + s;
        if let Some(off) = offset {
            let sign = if off.starts_with('-') { -1 } else { 1 };
            let parts: Vec<&str> = off[1..].split(':').collect();
            if parts.is_empty() || parts.len() > 3 || parts.iter().any(|p| p.len() != 2) {
                return None;
            }
            let oh = digits(parts[0].as_bytes())?;
            let om = if parts.len() > 1 {
                digits(parts[1].as_bytes())?
            } else {
                0
            };
            let os = if parts.len() > 2 {
                digits(parts[2].as_bytes())?
            } else {
                0
            };
            if oh > 23 || om > 59 || os > 59 {
                return None;
            }
            seconds -= sign * (oh * 3600 + om * 60 + os);
        }
    }
    Some((seconds as f64 * 1e6 + micros as f64) / 1e6)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_is_half_even() {
        assert_eq!(round_int(0.5), 0);
        assert_eq!(round_int(1.5), 2);
        assert_eq!(round_int(2.5), 2);
        assert_eq!(round_int(-2.5), -2);
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(0.125, 2), 0.12);
        assert_eq!(fmt_fixed(84.5, 0), "84");
        assert_eq!(fmt_percent0(0.605), "60%");
    }

    #[test]
    fn float_repr_matches_python() {
        for (x, want) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (0.1, "0.1"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (1e-5, "1e-05"),
            (123.456, "123.456"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (1727000000.123456, "1727000000.123456"),
            (12345678901234567.0, "1.2345678901234568e+16"),
        ] {
            assert_eq!(float_repr(x), want, "{x}");
        }
    }

    #[test]
    fn neumaier_sum() {
        assert_eq!(sum([0.1; 10]), 1.0);
        assert_eq!(sum([1e100, 1.0, -1e100]), 1.0);
    }

    #[test]
    fn iso_round_trip() {
        assert_eq!(iso_utc(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_700_000_000.0), "2023-11-14T22:13:20Z");
        assert_eq!(iso_utc(1.9999996), "1970-01-01T00:00:02Z");
        assert_eq!(iso_utc(1.9999994), "1970-01-01T00:00:01Z");
        assert_eq!(
            parse_iso_timestamp("2023-11-14T22:13:20Z"),
            Some(1_700_000_000.0)
        );
        assert_eq!(
            parse_iso_timestamp("2023-11-14T23:13:20+01:00"),
            Some(1_700_000_000.0)
        );
        assert_eq!(parse_iso_timestamp("garbage"), None);
        assert_eq!(parse_iso_timestamp("2023-02-30"), None);
    }

    #[test]
    fn medians() {
        assert_eq!(median_int(&[]), None);
        assert_eq!(median_int(&[3, 1, 2]), Some(2));
        assert_eq!(median_int(&[1, 2]), Some(2)); // 1.5 -> 2
        assert_eq!(median_int(&[2, 3]), Some(2)); // 2.5 -> 2
    }
}
