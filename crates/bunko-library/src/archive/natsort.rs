//! `natsort.natsorted(paths)` (natsort 8.4, default `ns.INT | ns.UNSIGNED`),
//! the page order 0.5.2's OCR runner used (`engine_runner.reading_order`).
//!
//! The key: the path string NFD-normalized, split into runs of decimal digits,
//! single non-decimal digit characters (`①`, `²`), and text; numbers by value,
//! text by code point; an empty string inserted before a leading number and
//! between two adjacent numbers so the tuple always alternates text/number.

use std::cmp::Ordering;

use crate::pyunicode;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    /// Canonical decimal digits of the value.
    Number(String),
}

impl Ord for Part {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Part::Text(a), Part::Text(b)) => a.cmp(b),
            (Part::Number(a), Part::Number(b)) => pyunicode::cmp_decimal_strings(a, b),
            // Never compared in practice: keys alternate text/number by
            // construction. Python would raise TypeError; any fixed answer will do.
            (Part::Text(_), Part::Number(_)) => Ordering::Greater,
            (Part::Number(_), Part::Text(_)) => Ordering::Less,
        }
    }
}

impl PartialOrd for Part {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A natsort key; compare with `Ord`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NatsortKey(Vec<Part>);

/// `natsort_key(path)`.
pub fn natsort_key(path: &str) -> NatsortKey {
    let normalized = pyunicode::nfd(path);
    let mut components: Vec<Part> = Vec::new();
    let mut text = String::new();
    let mut digits = String::new();
    let flush_text = |text: &mut String, out: &mut Vec<Part>| {
        if !text.is_empty() {
            out.push(Part::Text(std::mem::take(text)));
        }
    };
    let flush_digits = |digits: &mut String, out: &mut Vec<Part>| {
        if !digits.is_empty() {
            out.push(Part::Number(pyunicode::decimal_run_value(digits)));
            digits.clear();
        }
    };
    for c in normalized.chars() {
        if pyunicode::is_decimal(c) {
            flush_text(&mut text, &mut components);
            digits.push(c);
        } else if let Some(value) = pyunicode::digit_not_decimal(c) {
            flush_text(&mut text, &mut components);
            flush_digits(&mut digits, &mut components);
            components.push(Part::Number(value.to_string()));
        } else {
            flush_digits(&mut digits, &mut components);
            text.push(c);
        }
    }
    flush_text(&mut text, &mut components);
    flush_digits(&mut digits, &mut components);

    // `sep_inserter`.
    let mut key = Vec::with_capacity(components.len() + 1);
    let mut previous_number = false;
    for (index, part) in components.into_iter().enumerate() {
        let number = matches!(part, Part::Number(_));
        if number && (index == 0 || previous_number) {
            key.push(Part::Text(String::new()));
        }
        previous_number = number;
        key.push(part);
    }
    NatsortKey(key)
}

/// `natsorted(items)`: a stable sort by [`natsort_key`].
pub fn natsorted<T>(items: &mut [T], path_of: impl Fn(&T) -> &str) {
    // `sort_by_cached_key` is stable (equal keys keep their input order).
    items.sort_by_cached_key(|item| natsort_key(path_of(item)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_like_natsort() {
        let mut paths = vec!["x/p1.jpg", "p10.jpg", "①.jpg", "p9.jpg"];
        natsorted(&mut paths, |p| p);
        assert_eq!(paths, vec!["①.jpg", "p9.jpg", "p10.jpg", "x/p1.jpg"]);
    }

    #[test]
    fn stable_on_ties() {
        let mut paths = vec!["p01.jpg", "p1.jpg", "p001.jpg"];
        natsorted(&mut paths, |p| p);
        assert_eq!(paths, vec!["p01.jpg", "p1.jpg", "p001.jpg"]);
    }
}
