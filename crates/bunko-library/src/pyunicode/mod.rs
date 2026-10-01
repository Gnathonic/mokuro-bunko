//! Python 3.12 string semantics the 0.5.2 compiler relied on.
//!
//! Folding keys (`normalize_series_key`), placeholder uuids and the natural
//! sort all went through CPython's own Unicode database (UCD 15.0). Rust's
//! std tracks a newer Unicode version, so the case/space/digit tables here are
//! generated from the reference interpreter (`scripts/gen_unicode_tables.py`)
//! instead of using `char::to_lowercase` and friends. Normalization forms come
//! from `unicode-normalization` (a newer UCD than 15.0: only characters
//! assigned after 15.0 can differ, and none of them occur in real titles).

#[rustfmt::skip]
mod tables;

use unicode_normalization::UnicodeNormalization;

pub use tables::UNIDATA_VERSION;

fn lookup_str(table: &'static [(u32, &'static str)], c: char) -> Option<&'static str> {
    table
        .binary_search_by_key(&(c as u32), |&(cp, _)| cp)
        .ok()
        .map(|index| table[index].1)
}

fn in_ranges(table: &[(u32, u32)], c: char) -> bool {
    let cp = c as u32;
    let index = table.partition_point(|&(start, _)| start <= cp);
    index > 0 && cp <= table[index - 1].1
}

/// `str.isspace()` (also what `str.strip()` and `re`'s `\s` use).
pub fn is_space(c: char) -> bool {
    tables::SPACE.binary_search(&(c as u32)).is_ok()
}

/// `str.isdecimal()` for one character (what `re`'s `\d` matches).
pub fn decimal_value(c: char) -> Option<u8> {
    let cp = c as u32;
    let table = tables::DECIMAL;
    let index = table.partition_point(|&(start, _, _)| start <= cp);
    if index == 0 {
        return None;
    }
    let (start, end, base) = table[index - 1];
    // Values inside one run count up from `base` (at most 9), so the sum fits.
    (cp <= end).then(|| base + (cp - start) as u8)
}

/// `str.isdecimal()` for one character.
pub fn is_decimal(c: char) -> bool {
    decimal_value(c).is_some()
}

/// `unicodedata.digit(c)` for a character that is a digit but NOT a decimal
/// (superscripts, circled numbers...).
pub fn digit_not_decimal(c: char) -> Option<u8> {
    let table = tables::DIGIT_NOT_DECIMAL;
    table
        .binary_search_by_key(&(c as u32), |&(cp, _)| cp)
        .ok()
        .map(|index| table[index].1)
}

/// `unicodedata.combining(c) != 0`.
pub fn is_combining(c: char) -> bool {
    in_ranges(tables::COMBINING, c)
}

fn is_cased(c: char) -> bool {
    in_ranges(tables::CASED, c)
}

fn is_case_ignorable(c: char) -> bool {
    in_ranges(tables::CASE_IGNORABLE, c)
}

/// CPython's `handle_capital_sigma`: is the `Σ` at `chars[index]` word-final?
fn sigma_is_final(chars: &[char], index: usize) -> bool {
    let before = chars[..index]
        .iter()
        .rev()
        .find(|&&c| !is_case_ignorable(c));
    let mut final_sigma = before.is_some_and(|&c| is_cased(c));
    if final_sigma {
        let after = chars[index + 1..].iter().find(|&&c| !is_case_ignorable(c));
        final_sigma = after.is_none_or(|&c| !is_cased(c));
    }
    final_sigma
}

/// `str.lower()`.
pub fn lower(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    if !text.contains('\u{3a3}') {
        for c in text.chars() {
            match lookup_str(tables::LOWER, c) {
                Some(mapped) => out.push_str(mapped),
                None => out.push(c),
            }
        }
        return out;
    }
    let chars: Vec<char> = text.chars().collect();
    for (index, &c) in chars.iter().enumerate() {
        if c == '\u{3a3}' {
            out.push(if sigma_is_final(&chars, index) {
                '\u{3c2}'
            } else {
                '\u{3c3}'
            });
        } else {
            match lookup_str(tables::LOWER, c) {
                Some(mapped) => out.push_str(mapped),
                None => out.push(c),
            }
        }
    }
    out
}

/// `str.casefold()`.
pub fn casefold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match lookup_str(tables::CASEFOLD, c) {
            Some(mapped) => out.push_str(mapped),
            None => out.push(c),
        }
    }
    out
}

/// `str.strip()` with no arguments.
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `re.sub(r"\s+", " ", text)`.
pub fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if is_space(c) {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `unicodedata.normalize("NFC", text)`.
pub fn nfc(text: &str) -> String {
    text.nfc().collect()
}

/// `unicodedata.normalize("NFD", text)`.
pub fn nfd(text: &str) -> String {
    text.nfd().collect()
}

/// `unicodedata.normalize("NFKD", text)`.
pub fn nfkd(text: &str) -> String {
    text.nfkd().collect()
}

/// Python's `int(s)` for a run of decimal digits (any script), as a canonical
/// ASCII digit string without leading zeros (`"0"` for zero). Arbitrary
/// precision: digit runs in titles are compared by value, never overflow.
pub fn decimal_run_value(run: &str) -> String {
    let digits: String = run
        .chars()
        .filter_map(decimal_value)
        .map(|value| char::from(b'0' + value))
        .collect();
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Compare two canonical non-negative decimal strings (see [`decimal_run_value`]) by value.
pub fn cmp_decimal_strings(a: &str, b: &str) -> std::cmp::Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_follows_python_including_final_sigma() {
        assert_eq!(lower("ΟΔΟΣ"), "οδος");
        assert_eq!(lower("ΟΔΟΣ ΑΣ"), "οδος ας");
        assert_eq!(lower("Σ"), "σ");
        assert_eq!(lower("İ"), "i\u{307}");
        assert_eq!(lower("Dr STONE"), "dr stone");
    }

    #[test]
    fn casefold_is_full_folding() {
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(casefold("ﬁ"), "fi");
    }

    #[test]
    fn whitespace_matches_python_isspace() {
        assert!(is_space('\u{1c}'));
        assert!(is_space('\u{85}'));
        assert!(!is_space('\u{feff}'));
        assert_eq!(strip("\u{3000} a b \u{85}"), "a b");
        assert_eq!(collapse_whitespace("a \t\n b"), "a b");
    }

    #[test]
    fn digits() {
        assert_eq!(decimal_value('٣'), Some(3));
        assert_eq!(decimal_value('①'), None);
        assert_eq!(digit_not_decimal('①'), Some(1));
        assert_eq!(decimal_run_value("007"), "7");
        assert_eq!(decimal_run_value("000"), "0");
        assert_eq!(decimal_run_value("１２"), "12");
    }
}
