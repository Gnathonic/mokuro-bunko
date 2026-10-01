//! The bits of CPython 3.12 semantics the layout's output depends on.
//!
//! The sidecar is compared byte for byte with what 0.5.2 wrote, and every
//! integer in it comes out of float geometry, so "close" is not good enough:
//! a coordinate that lands on the other side of a `.5` moves a box by a pixel.
//! These helpers reproduce Python's results bit for bit (spec Appendix A).

use std::cmp::Ordering;

/// Python `round(x, ndigits)` for `ndigits >= 0`: correctly rounded on the
/// exact binary value, ties to even, as `_Py_dg_dtoa` mode 3 does.
///
/// `(x * 100).round() / 100` is wrong here (`round(2.675, 2) == 2.67`).
pub fn round_digits(x: f64, ndigits: u32) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    if ndigits > 22 {
        // Every f64 with this many digits is already "rounded" for our purposes
        // (CPython only short-cuts above ~323, but nothing here asks for > 4).
        return x;
    }
    let negative = x.is_sign_negative();
    let bits = x.abs().to_bits();
    let exp_bits = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & ((1u64 << 52) - 1);
    let (mant, exp) = if exp_bits == 0 {
        (frac, -1074i64)
    } else {
        (frac | (1u64 << 52), exp_bits - 1075)
    };
    if exp >= 0 {
        // An integer already.
        return x;
    }
    let k = (-exp) as u32;
    let pow10 = 10u128.pow(ndigits);
    let num = u128::from(mant) * pow10;
    let q = if k >= 127 {
        // num < 2^(53+73) and half = 2^(k-1) >= 2^126: always rounds down to 0.
        0u128
    } else {
        let q = num >> k;
        let rem = num - (q << k);
        let half = 1u128 << (k - 1);
        if rem > half || (rem == half && q & 1 == 1) {
            q + 1
        } else {
            q
        }
    };
    let magnitude = if q <= (1u128 << 53) {
        // Both operands exact, so the one IEEE division is correctly rounded:
        // the same double `strtod("<q>e-<n>")` gives.
        q as f64 / 10f64.powi(ndigits as i32)
    } else {
        // Unreachable for the magnitudes the layout sees; exact fallback.
        format!("{q}e-{ndigits}").parse::<f64>().unwrap_or(f64::NAN)
    };
    if negative { -magnitude } else { magnitude }
}

/// Python `int(round(x))`: round half to even, as an integer.
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Python 3.12 `sum()` over floats (start `0`): Neumaier-compensated.
///
/// CPython 3.12 changed float `sum()` to compensated summation, so a sum of
/// four corners is not `a + b + c + d`.
pub fn sum<I: IntoIterator<Item = f64>>(values: I) -> f64 {
    let mut iter = values.into_iter();
    let Some(first) = iter.next() else {
        return 0.0;
    };
    // `0 + first` (int + float) is exact; -0.0 becomes 0.0 as in Python.
    let mut f = 0.0 + first;
    let mut c = 0.0f64;
    for x in iter {
        let t = f + x;
        if f.abs() >= x.abs() {
            c += (f - t) + x;
        } else {
            c += (x - t) + f;
        }
        f = t;
    }
    if c != 0.0 && c.is_finite() {
        f += c;
    }
    f
}

/// Python `math.hypot(x, y)`: CPython 3.12's `vector_norm`, which is not libm's
/// `hypot` and can differ from it in the last bit.
pub fn hypot(x: f64, y: f64) -> f64 {
    let mut vec = [x.abs(), y.abs()];
    let mut max = 0.0f64;
    let mut found_nan = false;
    for v in vec {
        found_nan |= v.is_nan();
        if v > max {
            max = v;
        }
    }
    vector_norm(&mut vec, max, found_nan)
}

fn vector_norm(vec: &mut [f64], max: f64, found_nan: bool) -> f64 {
    if max.is_infinite() {
        return max;
    }
    if found_nan {
        return f64::NAN;
    }
    if max == 0.0 || vec.len() <= 1 {
        return max;
    }
    let max_e = frexp_exp(max);
    if max_e < -1023 {
        for v in vec.iter_mut() {
            *v /= f64::MIN_POSITIVE;
        }
        return f64::MIN_POSITIVE * vector_norm(vec, max / f64::MIN_POSITIVE, found_nan);
    }
    let scale = ldexp1(-max_e);
    let mut csum = 1.0f64;
    let mut frac1 = 0.0f64;
    let mut frac2 = 0.0f64;
    for &v in vec.iter() {
        let x = v * scale;
        let (pr_hi, pr_lo) = dl_mul(x, x);
        let (sm_hi, sm_lo) = dl_fast_sum(csum, pr_hi);
        csum = sm_hi;
        frac1 += pr_lo;
        frac2 += sm_lo;
    }
    let mut h = (csum - 1.0 + (frac1 + frac2)).sqrt();
    let (pr_hi, pr_lo) = dl_mul(-h, h);
    let (sm_hi, sm_lo) = dl_fast_sum(csum, pr_hi);
    csum = sm_hi;
    frac1 += pr_lo;
    frac2 += sm_lo;
    let x = csum - 1.0 + (frac1 + frac2);
    h += x / (2.0 * h);
    h / scale
}

fn dl_mul(x: f64, y: f64) -> (f64, f64) {
    let z = x * y;
    (z, x.mul_add(y, -z))
}

fn dl_fast_sum(a: f64, b: f64) -> (f64, f64) {
    let x = a + b;
    let z = x - a;
    (x, b - z)
}

/// The exponent `e` of C `frexp(x)` (x = m * 2^e, 0.5 <= m < 1), x finite > 0.
fn frexp_exp(x: f64) -> i32 {
    let bits = x.to_bits();
    let exp_bits = ((bits >> 52) & 0x7ff) as i32;
    if exp_bits == 0 {
        // Subnormal: normalise.
        let frac = bits & ((1u64 << 52) - 1);
        let lz = frac.leading_zeros() as i32 - 12; // zeros inside the 52-bit field
        -1022 - lz
    } else {
        exp_bits - 1022
    }
}

/// `ldexp(1.0, e)` for the range vector_norm uses.
fn ldexp1(e: i32) -> f64 {
    if (-1022..=1023).contains(&e) {
        f64::from_bits(((e + 1023) as u64) << 52)
    } else if (-1074..-1022).contains(&e) {
        // subnormal powers of two (`powi` would underflow through 1/2^-e)
        f64::from_bits(1u64 << (e + 1074))
    } else if e > 1023 {
        f64::INFINITY
    } else {
        0.0
    }
}

/// C `pow(x, y)`, which is what CPython's `x ** y` calls for floats. The
/// exponent goes through `black_box` so LLVM cannot fold `pow(x, 2.0)` into
/// `x * x` (libm's `pow` does not promise to round like the multiplication).
pub fn pow(x: f64, y: f64) -> f64 {
    x.powf(std::hint::black_box(y))
}

/// `math.radians`: `x * (pi / 180)`, the constant folded first as CPython does.
pub fn radians(x: f64) -> f64 {
    x * (std::f64::consts::PI / 180.0)
}

/// `math.degrees`: `x * (180 / pi)`.
pub fn degrees(x: f64) -> f64 {
    x * (180.0 / std::f64::consts::PI)
}

/// Total order for sorting floats the way Python's `sorted` sees them (no NaN
/// reaches here; a NaN compares equal so the sort stays stable).
pub fn fcmp(a: f64, b: f64) -> Ordering {
    a.partial_cmp(&b).unwrap_or(Ordering::Equal)
}

/// `statistics.median` (mean of the middle two for even n). Empty -> None.
pub fn median<I: IntoIterator<Item = f64>>(values: I) -> Option<f64> {
    let mut data: Vec<f64> = values.into_iter().collect();
    if data.is_empty() {
        return None;
    }
    data.sort_by(|a, b| fcmp(*a, *b));
    let n = data.len();
    if n % 2 == 1 {
        Some(data[n / 2])
    } else {
        let i = n / 2;
        Some((data[i - 1] + data[i]) / 2.0)
    }
}

/// Python `min(a, b)`: `a` unless `b < a` (keeps the first on ties, and the
/// sign of a zero the way Python does).
pub fn min2(a: f64, b: f64) -> f64 {
    if b < a { b } else { a }
}

/// Python `max(a, b)`: `a` unless `b > a`.
pub fn max2(a: f64, b: f64) -> f64 {
    if b > a { b } else { a }
}

/// Python `min(iterable)` over a non-empty iterator.
pub fn min_of<I: IntoIterator<Item = f64>>(values: I) -> f64 {
    let mut iter = values.into_iter();
    let mut best = iter.next().unwrap_or(f64::NAN);
    for v in iter {
        if v < best {
            best = v;
        }
    }
    best
}

/// Python `max(iterable)` over a non-empty iterator.
pub fn max_of<I: IntoIterator<Item = f64>>(values: I) -> f64 {
    let mut iter = values.into_iter();
    let mut best = iter.next().unwrap_or(f64::NAN);
    for v in iter {
        if v > best {
            best = v;
        }
    }
    best
}

/// Python `str.isspace()` for one character: Unicode White_Space plus the
/// four information separators U+001C..U+001F, which Rust's
/// `char::is_whitespace` leaves out.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()` (no argument).
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python `str.isascii()` (true for the empty string).
pub fn is_ascii(s: &str) -> bool {
    s.is_ascii()
}

/// `ch.isascii() and ch.isalnum()`.
pub fn is_ascii_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}

/// Python `repr(float)`: shortest round-trip digits, exponent form when the
/// decimal point sits before the 4th leading zero or past 16 digits.
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "nan".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    if x == 0.0 {
        return if x.is_sign_negative() {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        };
    }
    // Rust's `{:e}` gives the SHORTEST round-trip length, but when several
    // strings of that length round-trip it may not pick the one dtoa mode 0
    // picks (the closest to the exact value). So take the length from it and
    // the digits from the exact, correctly rounded formatting at that length,
    // keeping the shortest form only if that one does not round-trip.
    let shortest = format!("{:e}", x.abs());
    let n_digits = shortest
        .split_once('e')
        .map(|(m, _)| m.chars().filter(char::is_ascii_digit).count())
        .unwrap_or(1);
    let exact = format!("{:.*e}", n_digits.saturating_sub(1), x.abs());
    let sci = if exact.parse::<f64>().ok() == Some(x.abs()) {
        exact
    } else {
        shortest
    };
    let (mant, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let n = digits.len() as i32;
    let decpt = exp + 1;
    let mut out = String::new();
    if x < 0.0 {
        out.push('-');
    }
    if decpt <= -4 || decpt > 16 {
        out.push_str(&digits[..1]);
        if n > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let e = decpt - 1;
        out.push('e');
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", e.abs()));
    } else if decpt <= 0 {
        out.push_str("0.");
        for _ in 0..(-decpt) {
            out.push('0');
        }
        out.push_str(&digits);
    } else if decpt >= n {
        out.push_str(&digits);
        for _ in 0..(decpt - n) {
            out.push('0');
        }
        out.push_str(".0");
    } else {
        out.push_str(&digits[..decpt as usize]);
        out.push('.');
        out.push_str(&digits[decpt as usize..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_digits_matches_python_examples() {
        assert_eq!(round_digits(2.675, 2), 2.67);
        assert_eq!(round_digits(0.125, 2), 0.12);
        assert_eq!(round_digits(0.375, 2), 0.38);
        assert_eq!(round_digits(-0.125, 2), -0.12);
        assert_eq!(round_digits(1.0005, 3), 1.0);
        assert_eq!(round_digits(-0.0001, 2).to_bits(), (-0.0f64).to_bits());
        assert_eq!(round_digits(1571.3456, 2), 1571.35);
        assert_eq!(round_digits(0.99995, 4), 1.0);
        assert_eq!(round_digits(0.99994999, 4), 0.9999);
    }

    #[test]
    fn float_repr_matches_python() {
        assert_eq!(float_repr(1.0), "1.0");
        assert_eq!(float_repr(0.1), "0.1");
        assert_eq!(float_repr(1e-5), "1e-05");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1234567890123456.0), "1234567890123456.0");
        assert_eq!(float_repr(1.5e16), "1.5e+16");
        assert_eq!(float_repr(-2.5), "-2.5");
        assert_eq!(float_repr(123.456), "123.456");
        assert_eq!(float_repr(1e22), "1e+22");
        assert_eq!(float_repr(5e-324), "5e-324");
    }

    #[test]
    fn python_sum_is_compensated() {
        // naive left-to-right gives 0.0 here
        assert_eq!(sum([1e100, 1.0, -1e100]), 1.0);
        assert_eq!(sum([0.1, 0.2]), 0.1 + 0.2);
    }

    #[test]
    fn space_includes_information_separators() {
        assert!(is_space('\u{1f}'));
        assert!(is_space('\u{3000}'));
        assert!(!is_space('\u{200b}'));
        assert_eq!(strip("\u{3000} a \u{1c}"), "a");
    }
}
