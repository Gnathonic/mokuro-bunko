//! The one ordering rule of the OCR queue (`ocr/job_order.py`).
//!
//! 1. **Generation order**: every job of a row before any job of the row
//!    below it (the operator's list order, `generation_rank`).
//! 2. **Round-robin across series** within a row: every series' first
//!    pending volume, then every series' second, …
//! 3. **Reading order inside a series**, series in name order, each row's
//!    round continuing after the series it served last (`last_served`).
//!
//! Both orders are a natural sort ("Volume 2" before "Volume 10", "第二巻"
//! before "第十巻"), reproduced character for character from 0.5.2: NFKC,
//! Python's full case fold, Python's `\d` and `str.isalnum` (tables generated
//! from CPython 3.12, see `tools/gen_unicode_tables.py`).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use unicode_normalization::UnicodeNormalization;

use crate::unicode_tables::{ALNUM, CASEFOLD, DECIMAL};

/// An arbitrary-size non-negative integer, as its decimal digits without
/// leading zeros ("0" for zero). Python's `int` has no upper bound, and a
/// run of 25 digits in a file name must still compare as a number.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BigNum(String);

impl BigNum {
    /// From ASCII decimal digits (leading zeros allowed).
    pub fn from_digits(digits: &str) -> Self {
        let trimmed = digits.trim_start_matches('0');
        BigNum(if trimmed.is_empty() {
            "0".to_owned()
        } else {
            trimmed.to_owned()
        })
    }

    /// The decimal digits.
    pub fn digits(&self) -> &str {
        &self.0
    }
}

impl From<u64> for BigNum {
    fn from(v: u64) -> Self {
        BigNum(v.to_string())
    }
}

impl Ord for BigNum {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for BigNum {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One run of a name. Python's `(0, value, fraction)` / `(1, 0, text)`:
/// a number sorts before text in the same place (variant order).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Token {
    /// A number; `fraction` is its decimal digits with trailing zeros
    /// stripped, compared as a string (10.25 < 10.5).
    Number { value: BigNum, fraction: String },
    /// Anything else, case-folded.
    Text(String),
}

/// `natural_key(text)`: tuple comparison = `Vec` comparison (a prefix first).
pub type NaturalKey = Vec<Token>;

const KANJI_DIGITS: [char; 10] = ['〇', '一', '二', '三', '四', '五', '六', '七', '八', '九'];
const COUNTERS: [char; 4] = ['巻', '話', '章', '集'];

fn kanji_digit(c: char) -> Option<u32> {
    KANJI_DIGITS.iter().position(|k| *k == c).map(|p| p as u32)
}

fn kanji_unit(c: char) -> Option<u64> {
    match c {
        '十' => Some(10),
        '百' => Some(100),
        '千' => Some(1000),
        _ => None,
    }
}

fn is_kanji_numeral(c: char) -> bool {
    kanji_digit(c).is_some() || kanji_unit(c).is_some()
}

/// `_kanji_value`: positional ("一〇" = 10) or multiplicative ("千九百" =
/// 1900, units strictly decreasing, at most one digit each); None otherwise.
pub fn kanji_value(run: &[char]) -> Option<BigNum> {
    if run.iter().all(|c| kanji_digit(*c).is_some()) {
        let digits: String = run
            .iter()
            .filter_map(|c| kanji_digit(*c))
            .map(|d| char::from(b'0' + d as u8))
            .collect();
        return Some(BigNum::from_digits(&digits));
    }
    let mut total: u64 = 0;
    let mut digit: Option<u64> = None;
    let mut last_unit: u64 = 10_000;
    for &c in run {
        if let Some(d) = kanji_digit(c) {
            if digit.is_some() || c == '〇' {
                return None;
            }
            digit = Some(u64::from(d));
        } else {
            let unit = kanji_unit(c)?;
            if unit >= last_unit {
                return None;
            }
            total += digit.unwrap_or(1) * unit;
            digit = None;
            last_unit = unit;
        }
    }
    Some(BigNum::from(total + digit.unwrap_or(0)))
}

/// Python's `str.isalnum()` for one character.
pub fn py_isalnum(c: char) -> bool {
    let cp = c as u32;
    ALNUM
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                Ordering::Less
            } else if lo > cp {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// Python's decimal value of a `\d` character (`int(ch)`), or None.
pub fn py_decimal(c: char) -> Option<u32> {
    let cp = c as u32;
    DECIMAL
        .binary_search_by(|&(lo, hi, _)| {
            if hi < cp {
                Ordering::Less
            } else if lo > cp {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .ok()
        .map(|i| {
            let (lo, _, v) = DECIMAL[i];
            v + (cp - lo)
        })
}

/// Python's `str.casefold()`.
pub fn py_casefold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match CASEFOLD.binary_search_by_key(&(c as u32), |&(cp, _)| cp) {
            Ok(i) => out.push_str(CASEFOLD[i].1),
            Err(_) => out.push(c),
        }
    }
    out
}

/// `_stands_as_number`: after 第, before a counter (巻話章集), or a whole
/// word of its own (no alphanumeric neighbour).
fn stands_as_number(text: &[char], start: usize, end: usize) -> bool {
    let before = if start > 0 {
        Some(text[start - 1])
    } else {
        None
    };
    let after = text.get(end).copied();
    if before == Some('第') || after.is_some_and(|a| COUNTERS.contains(&a)) {
        return true;
    }
    !before.is_some_and(py_isalnum) && !after.is_some_and(py_isalnum)
}

/// `natural_key(text)`: `NFKC(text).casefold()` split into number and text
/// runs (see the module docs).
pub fn natural_key(text: &str) -> NaturalKey {
    let normalized: String = text.nfkc().collect();
    let folded: Vec<char> = py_casefold(&normalized).chars().collect();
    let mut key = Vec::new();
    let mut text_from = 0usize;
    let push_number = |key: &mut NaturalKey, text_from: &mut usize, start, end, value, fraction| {
        if start > *text_from {
            key.push(Token::Text(folded[*text_from..start].iter().collect()));
        }
        key.push(Token::Number { value, fraction });
        *text_from = end;
    };
    let n = folded.len();
    let mut i = 0usize;
    while i < n {
        let c = folded[i];
        if py_decimal(c).is_some() {
            // `(?P<arabic>\d+)(?:\.(?P<fraction>\d+))?`
            let start = i;
            let mut j = i;
            let mut digits = String::new();
            while j < n {
                match py_decimal(folded[j]) {
                    Some(d) => digits.push(char::from_digit(d, 10).unwrap_or('0')),
                    None => break,
                }
                j += 1;
            }
            let mut fraction = String::new();
            if j + 1 < n && folded[j] == '.' && py_decimal(folded[j + 1]).is_some() {
                let mut k = j + 1;
                while k < n {
                    match py_decimal(folded[k]) {
                        Some(d) => fraction.push(char::from_digit(d, 10).unwrap_or('0')),
                        None => break,
                    }
                    k += 1;
                }
                j = k;
            }
            let fraction = fraction.trim_end_matches('0').to_owned();
            push_number(
                &mut key,
                &mut text_from,
                start,
                j,
                BigNum::from_digits(&digits),
                fraction,
            );
            i = j;
        } else if is_kanji_numeral(c) {
            let start = i;
            let mut j = i;
            while j < n && is_kanji_numeral(folded[j]) {
                j += 1;
            }
            if let Some(value) = kanji_value(&folded[start..j])
                && stands_as_number(&folded, start, j)
            {
                push_number(&mut key, &mut text_from, start, j, value, String::new());
            }
            // Otherwise the run is part of a word and stays in the text run.
            i = j;
        } else {
            i += 1;
        }
    }
    if text_from < n {
        key.push(Token::Text(folded[text_from..].iter().collect()));
    }
    key
}

/// `_name_key`: natural order, raw name as the deterministic tie-break.
pub fn name_key(name: &str) -> (NaturalKey, String) {
    (natural_key(name), name.to_owned())
}

/// Compare two names in the queue's natural order (raw name breaks ties).
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    name_key(a).cmp(&name_key(b))
}

/// `(series, volume, generation id)`: what the rule needs to know about a job.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct JobKey {
    pub series: String,
    pub volume: String,
    pub generation_id: String,
}

/// `order_jobs(jobs, generation_rank, key=…, last_served=…)`.
///
/// Pass ONLY jobs that can run now (not in flight, not attempted this scan,
/// not in failure backoff): positions are over the jobs passed in. A
/// generation missing from `generation_rank` runs after every ranked one,
/// unranked ones ordered by id.
pub fn order_jobs<T>(
    jobs: Vec<T>,
    generation_rank: &HashMap<String, usize>,
    key: impl Fn(&T) -> JobKey,
    last_served: &HashMap<String, String>,
) -> Vec<T> {
    let keyed: Vec<(JobKey, T)> = jobs.into_iter().map(|job| (key(&job), job)).collect();

    // One natural key per distinct name (the Python memoises the same way).
    let mut names: HashMap<String, (NaturalKey, String)> = HashMap::new();
    let mut nk = |s: &str| -> (NaturalKey, String) {
        names
            .entry(s.to_owned())
            .or_insert_with(|| name_key(s))
            .clone()
    };

    let mut volumes: HashMap<(String, String), HashSet<String>> = HashMap::new();
    for (k, _) in &keyed {
        volumes
            .entry((k.generation_id.clone(), k.series.clone()))
            .or_default()
            .insert(k.volume.clone());
    }
    let mut position: HashMap<(String, String, String), usize> = HashMap::new();
    for ((generation, series), set) in &volumes {
        let mut sorted: Vec<(NaturalKey, String)> = set.iter().map(|v| nk(v)).collect();
        sorted.sort();
        for (index, (_, volume)) in sorted.into_iter().enumerate() {
            position.insert((generation.clone(), series.clone(), volume), index);
        }
    }
    let cursors: HashMap<&str, (NaturalKey, String)> = last_served
        .iter()
        .map(|(g, s)| (g.as_str(), nk(s)))
        .collect();
    let unranked = generation_rank.len();

    type SortKey = (
        usize,
        String,
        usize,
        u8,
        (NaturalKey, String),
        (NaturalKey, String),
    );
    let mut decorated: Vec<(SortKey, T)> = keyed
        .into_iter()
        .map(|(k, job)| {
            let series_key = nk(&k.series);
            let wrapped = match cursors.get(k.generation_id.as_str()) {
                None => 0,
                Some(cursor) if series_key > *cursor => 0,
                Some(_) => 1,
            };
            let pos = position
                .get(&(k.generation_id.clone(), k.series.clone(), k.volume.clone()))
                .copied()
                .unwrap_or(0);
            let sort_key = (
                generation_rank
                    .get(&k.generation_id)
                    .copied()
                    .unwrap_or(unranked),
                k.generation_id.clone(),
                pos,
                wrapped,
                series_key,
                nk(&k.volume),
            );
            (sort_key, job)
        })
        .collect();
    decorated.sort_by(|a, b| a.0.cmp(&b.0));
    decorated.into_iter().map(|(_, job)| job).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(v: u64) -> Token {
        Token::Number {
            value: BigNum::from(v),
            fraction: String::new(),
        }
    }

    #[test]
    fn kanji_values() {
        let v = |s: &str| kanji_value(&s.chars().collect::<Vec<_>>()).map(|b| b.0);
        assert_eq!(v("一〇").as_deref(), Some("10"));
        assert_eq!(v("十一").as_deref(), Some("11"));
        assert_eq!(v("二十").as_deref(), Some("20"));
        assert_eq!(v("百二").as_deref(), Some("102"));
        assert_eq!(v("千九百").as_deref(), Some("1900"));
        assert_eq!(v("十十"), None);
        assert_eq!(v("二三十"), None);
        assert_eq!(v("〇十"), None);
    }

    #[test]
    fn natural_keys() {
        assert_eq!(
            natural_key("Vol 10"),
            vec![Token::Text("vol ".into()), num(10)]
        );
        assert_eq!(natural_key("第１０巻"), natural_key("第10巻"));
        assert_eq!(natural_key("第二巻"), natural_key("第2巻"));
        // Kanji inside a word stays text.
        assert_eq!(natural_key("一番"), vec![Token::Text("一番".into())]);
        assert!(natural_key("10.25") < natural_key("10.5"));
        assert!(natural_key("Vol 2") < natural_key("Vol 10"));
        assert!(natural_key("Vol 1") < natural_key("Vol A"));
        assert!(natural_key("Vol") < natural_key("Vol 1"));
        assert_eq!(natural_key("STRASSE"), natural_key("straße"));
    }

    #[test]
    fn round_robin_with_cursor() {
        let jobs = vec![("A", "1"), ("A", "2"), ("B", "1"), ("C", "1")];
        let rank: HashMap<String, usize> = [("g".to_owned(), 0)].into();
        let key = |j: &(&str, &str)| JobKey {
            series: j.0.into(),
            volume: j.1.into(),
            generation_id: "g".into(),
        };
        let none = HashMap::new();
        let order = order_jobs(jobs.clone(), &rank, key, &none);
        assert_eq!(order, vec![("A", "1"), ("B", "1"), ("C", "1"), ("A", "2")]);
        let served: HashMap<String, String> = [("g".to_owned(), "A".to_owned())].into();
        let order = order_jobs(jobs, &rank, key, &served);
        assert_eq!(order, vec![("B", "1"), ("C", "1"), ("A", "1"), ("A", "2")]);
    }
}
