//! Text rules of the page-level passes (`ppocr.py:933-1077`, spec §5.2–§5.4):
//! clipped-mark recovery from end probes, the doubted-kanji vote, and the seam
//! glyph count of joined column pieces. Strings are compared per code point, as
//! Python does.

use super::ctc::{IDEOGRAPHIC_SPACE, MISSING_GLYPH, is_kanji_char};
use super::difflib::{Tag, opcodes};

pub const PROBE_OPENERS: &str = "「『（〈《【〔";
pub const PROBE_CLOSERS: &str = "、。」』）〉》】〕！？";
pub const PROBE_THIN_OPENERS: &str = "一―";
pub const VOTE_MAX_CONF: f64 = 0.8;

fn cs(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn starts_with(text: &[char], rest: &[char]) -> bool {
    text.len() >= rest.len() && &text[..rest.len()] == rest
}

fn ends_with(text: &[char], rest: &[char]) -> bool {
    text.len() >= rest.len() && &text[text.len() - rest.len()..] == rest
}

fn repeats_start(text: &[char], rest: &[char]) -> bool {
    starts_with(text, rest) || (rest.len() >= 2 && starts_with(text, &rest[..rest.len() - 1]))
}

fn repeats_end(text: &[char], rest: &[char]) -> bool {
    ends_with(text, rest) || (rest.len() >= 2 && ends_with(text, &rest[1..]))
}

/// Python `text[:1] in MARKS`: an empty prefix is "in" any string.
fn head_in(text: &[char], marks: &str) -> bool {
    text.first().is_none_or(|c| marks.contains(*c))
}

fn tail_in(text: &[char], marks: &str) -> bool {
    text.last().is_none_or(|c| marks.contains(*c))
}

/// The opening mark the start probe found in front of `text`, or "".
pub fn clipped_opener(text: &str, probe: &str, thin: bool) -> String {
    let (t, p) = (cs(text), cs(probe));
    if p.len() < 2 {
        return String::new();
    }
    let first = p[0];
    let is_opener = PROBE_OPENERS.contains(first);
    let is_thin = PROBE_THIN_OPENERS.contains(first);
    if !(is_opener || (thin && is_thin)) {
        return String::new();
    }
    if is_opener && head_in(&t, PROBE_OPENERS) {
        return String::new();
    }
    if is_thin && t.len() >= 2 && t[0] == first && t[1] == first {
        return String::new();
    }
    if repeats_start(&t, &p[1..]) {
        first.to_string()
    } else {
        String::new()
    }
}

/// The closing mark the end probe found after `text`, or "".
pub fn clipped_closer(text: &str, probe: &str) -> String {
    let (t, p) = (cs(text), cs(probe));
    let Some(&last) = p.last() else {
        return String::new();
    };
    if p.len() < 2 || !PROBE_CLOSERS.contains(last) || tail_in(&t, PROBE_CLOSERS) {
        return String::new();
    }
    if repeats_end(&t, &p[..p.len() - 1]) {
        last.to_string()
    } else {
        String::new()
    }
}

/// `(opener, closer)` found by ONE probe spanning a whole short line.
pub fn clipped_marks(text: &str, probe: &str) -> (String, String) {
    let (t, p) = (cs(text), cs(probe));
    let opener = match p.first() {
        Some(c) if PROBE_OPENERS.contains(*c) && !head_in(&t, PROBE_OPENERS) => c.to_string(),
        _ => String::new(),
    };
    let closer = match p.last() {
        Some(c) if PROBE_CLOSERS.contains(*c) && !tail_in(&t, PROBE_CLOSERS) => c.to_string(),
        _ => String::new(),
    };
    for (head, tail) in [
        (opener.as_str(), closer.as_str()),
        (opener.as_str(), ""),
        ("", closer.as_str()),
    ] {
        if (!head.is_empty() || !tail.is_empty()) && format!("{head}{text}{tail}") == probe {
            return (head.to_string(), tail.to_string());
        }
    }
    (String::new(), String::new())
}

/// Positions of `text` → positions of `other` over `equal` opcodes and same-length
/// `replace` opcodes.
fn aligned(text: &[char], other: &[char]) -> std::collections::HashMap<usize, usize> {
    let mut map = std::collections::HashMap::new();
    for (tag, i1, i2, j1, j2) in opcodes(text, other) {
        if tag == Tag::Equal || (tag == Tag::Replace && i2 - i1 == j2 - j1) {
            for (i, j) in (i1..i2).zip(j1..j2) {
                map.insert(i, j);
            }
        }
    }
    map
}

/// `vote_characters`: settle doubted kanji by the other reads. Returns the new text,
/// confidences and the number of characters changed.
pub fn vote_characters(
    text: &str,
    confs: &[f64],
    others: &[(String, Vec<f64>)],
) -> (String, Vec<f64>, usize) {
    let t = cs(text);
    if others.is_empty() || confs.len() != t.len() {
        return (text.to_string(), confs.to_vec(), 0);
    }
    let other_chars: Vec<Vec<char>> = others.iter().map(|(o, _)| cs(o)).collect();
    let maps: Vec<_> = other_chars.iter().map(|o| aligned(&t, o)).collect();
    let mut chars = t.clone();
    let mut out_confs = confs.to_vec();
    let mut changed = 0;
    let space = IDEOGRAPHIC_SPACE.chars().next();
    let geta = MISSING_GLYPH.chars().next();
    for (i, &conf) in confs.iter().enumerate() {
        if conf >= VOTE_MAX_CONF || Some(chars[i]) == geta || Some(chars[i]) == space {
            continue;
        }
        let mut votes: Vec<(char, f64)> = Vec::new();
        for ((oc, (_, oconf)), m) in other_chars.iter().zip(others).zip(&maps) {
            if let Some(&j) = m.get(&i)
                && j < oconf.len()
            {
                votes.push((oc[j], oconf[j]));
            }
        }
        if votes.len() != others.len() {
            continue;
        }
        let winner = votes[0].0;
        if votes.iter().any(|v| v.0 != winner) {
            continue;
        }
        let support = votes.iter().map(|v| v.1).sum::<f64>() / votes.len() as f64;
        if winner == chars[i]
            || support < conf
            || !(is_kanji_char(winner) && is_kanji_char(chars[i]))
        {
            continue;
        }
        chars[i] = winner;
        out_confs[i] = support;
        changed += 1;
    }
    (chars.into_iter().collect(), out_confs, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openers_and_closers() {
        assert_eq!(clipped_opener("お願い、やめて", "「お願】", false), "「");
        assert_eq!(clipped_opener("「お願い", "「「お", false), "");
        assert_eq!(clipped_opener("瞬で決着", "一瞬で", true), "一");
        assert_eq!(clipped_opener("瞬で決着", "一瞬で", false), "");
        assert_eq!(clipped_closer("やめて", "めて。"), "。");
        assert_eq!(clipped_closer("やめて。", "めて。"), "");
        assert_eq!(
            clipped_marks("いや", "「いや」"),
            ("「".to_string(), "」".to_string())
        );
        assert_eq!(
            clipped_marks("いや", "「いや"),
            ("「".to_string(), String::new())
        );
        assert_eq!(
            clipped_marks("いや", "いや"),
            (String::new(), String::new())
        );
    }

    #[test]
    fn vote_needs_both_reads() {
        let others = vec![
            ("頷く".to_string(), vec![0.9, 0.9]),
            ("頷く".to_string(), vec![0.7, 0.9]),
        ];
        let (t, c, n) = vote_characters("鎮く", &[0.5, 0.99], &others);
        assert_eq!((t.as_str(), n), ("頷く", 1));
        assert!((c[0] - 0.8).abs() < 1e-12);
        let split = vec![
            ("頷く".to_string(), vec![0.9, 0.9]),
            ("鎮く".to_string(), vec![0.7, 0.9]),
        ];
        assert_eq!(vote_characters("鎮く", &[0.5, 0.99], &split).2, 0);
    }
}
