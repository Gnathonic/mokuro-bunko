//! Python / numpy semantics that the 0.5.2 code relies on and a port must reproduce
//! (spec `ocr-ppocr-layout.md` Appendix A).

/// Python `round(x, ndigits)`: correctly rounded on the exact binary value, ties to
/// even. Rust's `{:.N}` formatting is exact with the same tie rule, so format and
/// parse back (never `(x * 100).round() / 100`, which double-rounds).
pub fn round_to(x: f64, ndigits: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.ndigits$}").parse().unwrap_or(x)
}

/// Python `int(round(x))` for the values this crate meets (finite, well inside i64).
pub fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Python `str.isspace()` for one character: Unicode White_Space plus U+001C..U+001F,
/// which Rust's `char::is_whitespace` lacks.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()` (no argument).
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// Python `len(str)`: code points.
pub fn len(s: &str) -> usize {
    s.chars().count()
}

/// numpy's pairwise summation of a contiguous f64 array (`np.sum`, `np.mean`).
pub fn np_sum(a: &[f64]) -> f64 {
    const BLOCK: usize = 128;
    let n = a.len();
    if n < 8 {
        let mut res = 0.0;
        for v in a {
            res += *v;
        }
        res
    } else if n <= BLOCK {
        let mut r = [0.0f64; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - (n % 8) {
            for j in 0..8 {
                r[j] += a[i + j];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        res
    } else {
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        np_sum(&a[..n2]) + np_sum(&a[n2..])
    }
}

/// `np.mean` of an f64 array (NaN for an empty one, as numpy).
pub fn np_mean(a: &[f64]) -> f64 {
    np_sum(a) / a.len() as f64
}

/// numpy's pairwise summation for float32 arrays (accumulates in f32).
pub fn np_sum_f32(a: &[f32]) -> f32 {
    const BLOCK: usize = 128;
    let n = a.len();
    if n < 8 {
        let mut res = 0.0f32;
        for v in a {
            res += *v;
        }
        res
    } else if n <= BLOCK {
        let mut r = [0.0f32; 8];
        r.copy_from_slice(&a[..8]);
        let mut i = 8;
        while i < n - (n % 8) {
            for j in 0..8 {
                r[j] += a[i + j];
            }
            i += 8;
        }
        let mut res = ((r[0] + r[1]) + (r[2] + r[3])) + ((r[4] + r[5]) + (r[6] + r[7]));
        while i < n {
            res += a[i];
            i += 1;
        }
        res
    } else {
        let mut n2 = n / 2;
        n2 -= n2 % 8;
        np_sum_f32(&a[..n2]) + np_sum_f32(&a[n2..])
    }
}

/// `np.average(values, weights=weights)` in f64.
pub fn np_average(values: &[f64], weights: &[f64]) -> f64 {
    let products: Vec<f64> = values.iter().zip(weights).map(|(v, w)| v * w).collect();
    np_sum(&products) / np_sum(weights)
}

/// `statistics.median` / `np.median`: the mean of the two middle values for even n.
/// `None` for an empty input.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_matches_python() {
        assert_eq!(round_to(2.675, 2), 2.67);
        assert_eq!(round_to(0.125, 2), 0.12);
        assert_eq!(round_to(0.375, 2), 0.38);
        assert_eq!(round_to(5e-5, 4), 0.0001);
        assert_eq!(round_to(1.5e-4, 4), 0.0001);
        assert_eq!(round_to(-0.125, 2), -0.12);
        assert_eq!(round_int(2.5), 2);
        assert_eq!(round_int(3.5), 4);
        assert_eq!(round_int(-0.5), 0);
    }

    #[test]
    fn strip_like_python() {
        assert_eq!(strip("\u{3000} a\u{1f}\n"), "a");
        assert_eq!(strip("\u{200b}a"), "\u{200b}a");
        assert_eq!(len("「あ」"), 3);
    }

    #[test]
    fn pairwise_sum_blocks() {
        let a: Vec<f64> = (0..300).map(|i| 0.1 * i as f64).collect();
        let naive: f64 = a.iter().sum();
        assert!((np_sum(&a) - naive).abs() < 1e-9);
        assert_eq!(median(&[3.0, 1.0, 2.0, 10.0]), Some(2.5));
    }
}
