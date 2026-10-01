//! `natsort.natsorted` 8.4 with the default algorithm, as the 0.5.2 runner applies it
//! to `Path` objects (spec §2.3, checked against natsort itself):
//!
//! * the whole `str(path)` is the key (no path splitting), NFD-normalized first;
//! * it is split on `\d+` (Unicode decimal digits) and on single digit characters
//!   that are not decimals (`²`, `①`, ...), each of which becomes its own number;
//! * numbers compare by value (`001 == 1`), strings by code point (no case folding);
//!   a key starts with a string (`""` when the text starts with a number) and `""`
//!   separates adjacent numbers;
//! * the sort is stable.

use std::cmp::Ordering;

use unicode_normalization::UnicodeNormalization;

use crate::natsort_tables::{DECIMAL_RUNS, DIGIT_CHARS};

fn decimal_value(c: char) -> Option<u32> {
    let cp = c as u32;
    let i = DECIMAL_RUNS.partition_point(|r| r.1 < cp);
    let r = DECIMAL_RUNS.get(i)?;
    (r.0 <= cp && cp <= r.1).then(|| r.2 + (cp - r.0))
}

fn digit_value(c: char) -> Option<u32> {
    let cp = c as u32;
    DIGIT_CHARS
        .binary_search_by_key(&cp, |d| d.0)
        .ok()
        .map(|i| DIGIT_CHARS[i].1)
}

/// A number token: decimal digits without leading zeros (arbitrary size, like Python ints).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Num(Vec<u8>);

impl Num {
    fn from_digits(digits: impl Iterator<Item = u32>) -> Self {
        let mut v: Vec<u8> = digits.map(|d| d as u8).skip_while(|&d| d == 0).collect();
        v.shrink_to_fit();
        Num(v)
    }
}

impl Ord for Num {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for Num {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Tok {
    Str(String),
    Num(Num),
}

/// The natsort key of a string.
fn key(s: &str) -> Vec<Tok> {
    let s: String = s.nfd().collect();
    let mut raw: Vec<Tok> = Vec::new();
    let mut text = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(d) = decimal_value(c) {
            let mut digits = vec![d];
            while let Some(&n) = chars.peek() {
                match decimal_value(n) {
                    Some(v) => {
                        digits.push(v);
                        chars.next();
                    }
                    None => break,
                }
            }
            if !text.is_empty() {
                raw.push(Tok::Str(std::mem::take(&mut text)));
            }
            raw.push(Tok::Num(Num::from_digits(digits.into_iter())));
        } else if let Some(d) = digit_value(c) {
            if !text.is_empty() {
                raw.push(Tok::Str(std::mem::take(&mut text)));
            }
            raw.push(Tok::Num(Num::from_digits(std::iter::once(d))));
        } else {
            text.push(c);
        }
    }
    if !text.is_empty() {
        raw.push(Tok::Str(text));
    }
    // sep_inserter: "" before a leading number and between adjacent numbers.
    let mut out = Vec::with_capacity(raw.len() + 1);
    let mut prev_num = true; // a leading number gets a "" in front
    for t in raw {
        let is_num = matches!(t, Tok::Num(_));
        if is_num && prev_num {
            out.push(Tok::Str(String::new()));
        }
        prev_num = is_num;
        out.push(t);
    }
    out
}

/// Compare two strings the way natsort orders them.
pub fn compare(a: &str, b: &str) -> Ordering {
    key(a).cmp(&key(b))
}

/// Sort in natsort order (stable).
pub fn natsort<T, F: Fn(&T) -> &str>(items: &mut [T], text: F) {
    let mut keyed: Vec<(Vec<Tok>, usize)> = items
        .iter()
        .enumerate()
        .map(|(i, it)| (key(text(it)), i))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    let order: Vec<usize> = keyed.into_iter().map(|(_, i)| i).collect();
    apply_order(items, &order);
}

fn apply_order<T>(items: &mut [T], order: &[usize]) {
    // Permute in place following cycles: position k receives items[order[k]].
    let mut done = vec![false; items.len()];
    for start in 0..items.len() {
        if done[start] {
            continue;
        }
        let mut k = start;
        loop {
            done[k] = true;
            let src = order[k];
            if src == start {
                break;
            }
            items.swap(k, src);
            k = src;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_order() {
        let mut v = vec!["10.jpg", "9.jpg", "001.jpg", "1.jpg", "B.jpg", "a.jpg"];
        natsort(&mut v, |s| s);
        assert_eq!(
            v,
            vec!["001.jpg", "1.jpg", "9.jpg", "10.jpg", "B.jpg", "a.jpg"]
        );
        assert_eq!(compare("page-1.5", "page-1.10"), Ordering::Less);
        assert_eq!(compare("e\u{301}", "f"), Ordering::Less);
        assert_eq!(compare("é", "f"), Ordering::Less);
    }

    #[test]
    fn permutation_is_correct() {
        let mut v: Vec<String> = ["c", "a", "d", "b", "e"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        natsort(&mut v, |s| s.as_str());
        assert_eq!(v, vec!["a", "b", "c", "d", "e"]);
    }
}
