//! Python numeric/text semantics the 0.5.2 code relied on (spec §4.2).

/// Python's `str.isspace`: Unicode `White_Space` plus U+001C..U+001F, which Rust's
/// `char::is_whitespace` lacks.
pub fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python's `str.strip()` with no argument.
pub fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python's `round(x)` on a float: round half to even, as an i64.
pub fn round_half_even(x: f64) -> i64 {
    x.round_ties_even() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_like_python() {
        assert_eq!(py_strip("\u{1c} a b\u{3000}\n"), "a b");
        assert_eq!(py_strip("\u{200b}x"), "\u{200b}x"); // ZWSP is not whitespace in Python
    }

    #[test]
    fn bankers_rounding() {
        assert_eq!(round_half_even(0.5), 0);
        assert_eq!(round_half_even(1.5), 2);
        assert_eq!(round_half_even(2.5), 2);
        assert_eq!(round_half_even(-0.5), 0);
        assert_eq!(round_half_even(2.5000001), 3);
    }
}
