//! `difflib.SequenceMatcher(None, a, b, autojunk=False)` on character sequences,
//! exactly as CPython does it (spec Appendix C): matching blocks and opcodes.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Replace,
    Delete,
    Insert,
    Equal,
}

/// `(tag, i1, i2, j1, j2)`.
pub type Opcode = (Tag, usize, usize, usize, usize);

fn find_longest_match(
    a: &[char],
    b2j: &HashMap<char, Vec<usize>>,
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0usize);
    let mut j2len: HashMap<usize, usize> = HashMap::new();
    for (i, ch) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut newj2len: HashMap<usize, usize> = HashMap::new();
        if let Some(js) = b2j.get(ch) {
            for &j in js {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j
                    .checked_sub(1)
                    .and_then(|p| j2len.get(&p))
                    .copied()
                    .unwrap_or(0)
                    + 1;
                newj2len.insert(j, k);
                if k > bestsize {
                    besti = i + 1 - k;
                    bestj = j + 1 - k;
                    bestsize = k;
                }
            }
        }
        j2len = newj2len;
    }
    (besti, bestj, bestsize)
}

/// `get_matching_blocks()`, including the final `(la, lb, 0)` sentinel.
pub fn matching_blocks(a: &[char], b: &[char]) -> Vec<(usize, usize, usize)> {
    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (j, ch) in b.iter().enumerate() {
        b2j.entry(*ch).or_default().push(j);
    }
    let (la, lb) = (a.len(), b.len());
    let mut queue = vec![(0usize, la, 0usize, lb)];
    let mut blocks: Vec<(usize, usize, usize)> = Vec::new();
    while let Some((alo, ahi, blo, bhi)) = queue.pop() {
        let (i, j, k) = find_longest_match(a, &b2j, alo, ahi, blo, bhi);
        if k > 0 {
            blocks.push((i, j, k));
            if alo < i && blo < j {
                queue.push((alo, i, blo, j));
            }
            if i + k < ahi && j + k < bhi {
                queue.push((i + k, ahi, j + k, bhi));
            }
        }
    }
    blocks.sort();
    let mut merged: Vec<(usize, usize, usize)> = Vec::new();
    let (mut i1, mut j1, mut k1) = (0usize, 0usize, 0usize);
    for (i2, j2, k2) in blocks {
        if i1 + k1 == i2 && j1 + k1 == j2 {
            k1 += k2;
        } else {
            if k1 > 0 {
                merged.push((i1, j1, k1));
            }
            (i1, j1, k1) = (i2, j2, k2);
        }
    }
    if k1 > 0 {
        merged.push((i1, j1, k1));
    }
    merged.push((la, lb, 0));
    merged
}

/// `get_opcodes()`.
pub fn opcodes(a: &[char], b: &[char]) -> Vec<Opcode> {
    let (mut i, mut j) = (0usize, 0usize);
    let mut out = Vec::new();
    for (ai, bj, size) in matching_blocks(a, b) {
        let tag = if i < ai && j < bj {
            Some(Tag::Replace)
        } else if i < ai {
            Some(Tag::Delete)
        } else if j < bj {
            Some(Tag::Insert)
        } else {
            None
        };
        if let Some(t) = tag {
            out.push((t, i, ai, j, bj));
        }
        i = ai + size;
        j = bj + size;
        if size > 0 {
            out.push((Tag::Equal, ai, i, bj, j));
        }
    }
    out
}

/// `ratio()`.
pub fn ratio(a: &[char], b: &[char]) -> f64 {
    let matches: usize = matching_blocks(a, b).iter().map(|m| m.2).sum();
    let total = a.len() + b.len();
    if total == 0 {
        1.0
    } else {
        2.0 * matches as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn opcodes_like_python() {
        // difflib.SequenceMatcher(None, "qabxcd", "abycdf", autojunk=False).get_opcodes()
        let ops = opcodes(&chars("qabxcd"), &chars("abycdf"));
        assert_eq!(
            ops,
            vec![
                (Tag::Delete, 0, 1, 0, 0),
                (Tag::Equal, 1, 3, 0, 2),
                (Tag::Replace, 3, 4, 2, 3),
                (Tag::Equal, 4, 6, 3, 5),
                (Tag::Insert, 6, 6, 5, 6),
            ]
        );
        assert!((ratio(&chars("abcd"), &chars("bcde")) - 0.75).abs() < 1e-12);
    }
}
