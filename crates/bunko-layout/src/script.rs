//! `line_layout`'s script rules: character classes, ruby script, and the
//! one-for-one text fixes written to the sidecar (`normalize_text`).
//!
//! These predicates differ from `line_reconcile`'s on purpose (different
//! kanji ranges); keep them separate (spec §6.2).

use crate::py::{is_ascii, is_space};

pub const OPENING_BRACKETS: &str = "「『（(〈《【〔［[｢";
pub const SENTENCE_ENDINGS: &str = "。．.！？!?」』）)〉》】〕］]｣…‥―—";
/// Emphasis dots (bouten) are set in the ruby position and read as dots/commas.
pub const BOUTEN: &str = "・･、，,.﹅﹆丶ヽ゛゜'`";
pub const SMALL_KANA_HOSTS: &str = "キシチニヒミリギジヂビピテデフヴきしちにひみりぎじぢびぴてでふ";
pub const LONG_VOWEL: char = 'ー';
pub const DASH: char = '―';

/// CJK ideograph (incl. extension A, compatibility, and the 々〆〇 marks).
pub fn is_kanji(ch: char) -> bool {
    let o = ch as u32;
    (0x4E00..=0x9FFF).contains(&o)
        || (0x3400..=0x4DBF).contains(&o)
        || (0xF900..=0xFAFF).contains(&o)
        || (0x20000..=0x2FFFF).contains(&o)
        || "々〆〇".contains(ch)
}

/// Hiragana, katakana (full or half width), the prolonged-sound mark.
pub fn is_kana(ch: char) -> bool {
    let o = ch as u32;
    (0x3041..=0x309F).contains(&o)
        || (0x30A0..=0x30FF).contains(&o)
        || (0xFF66..=0xFF9F).contains(&o)
}

pub fn has_kanji(text: &str) -> bool {
    text.chars().any(is_kanji)
}

/// Visible glyphs: whitespace is a recognizer artefact inside a ruby run.
pub fn glyph_count(text: &str) -> usize {
    text.chars().filter(|c| !is_space(*c)).count()
}

/// Full-width katakana, including the prolonged-sound mark.
pub fn is_katakana(ch: char) -> bool {
    (0x30A1..=0x30FF).contains(&(ch as u32))
}

pub fn is_hiragana(ch: char) -> bool {
    (0x3041..=0x3096).contains(&(ch as u32))
}

fn kana_lookalike(ch: char) -> Option<char> {
    match ch {
        '夕' => Some('タ'),
        '力' => Some('カ'),
        '卜' => Some('ト'),
        _ => None,
    }
}

fn hiragana_lookalike(ch: char) -> Option<char> {
    match ch {
        'へ' => Some('ヘ'),
        'べ' => Some('ベ'),
        'ぺ' => Some('ペ'),
        'り' => Some('リ'),
        'き' => Some('キ'),
        _ => None,
    }
}

fn small_kana(ch: char) -> Option<char> {
    match ch {
        'ャ' => Some('ヤ'),
        'ュ' => Some('ユ'),
        'ョ' => Some('ヨ'),
        'ゃ' => Some('や'),
        'ゅ' => Some('ゆ'),
        'ょ' => Some('よ'),
        _ => None,
    }
}

/// Katakana the recognizer wrote as the kanji that looks the same ("夕イル").
pub fn fix_kana_lookalikes(text: &[char]) -> Vec<char> {
    let mut out = text.to_vec();
    if !text.iter().any(|c| kana_lookalike(*c).is_some()) {
        return out;
    }
    for i in 0..text.len().saturating_sub(1) {
        if let Some(k) = kana_lookalike(text[i])
            && is_katakana(text[i + 1])
            && (i == 0 || !is_kanji(text[i - 1]))
        {
            out[i] = k;
        }
    }
    out
}

/// The katakana a hiragana lookalike at `text[i]` should be, if context says so.
fn katakana_for(text: &[char], i: usize) -> Option<char> {
    let ch = text[i];
    let prev = if i > 0 { Some(text[i - 1]) } else { None };
    let after = &text[(i + 1).min(text.len())..(i + 3).min(text.len())];
    let next_kata = !after.is_empty() && is_katakana(after[0]);
    if prev.is_some_and(is_katakana) && next_kata {
        return hiragana_lookalike(ch);
    }
    if ch == 'き' {
        let two_before = &text[i.saturating_sub(2)..i];
        if two_before.len() == 2 && two_before.iter().all(|c| is_katakana(*c)) && !next_kata {
            return Some('キ');
        }
        return None;
    }
    if "へべぺ".contains(ch) && next_kata {
        if !prev.is_some_and(|p| is_hiragana(p) || is_kanji(p)) {
            return hiragana_lookalike(ch);
        }
        // `not is_kanji(prev)` with prev present here (the branch above took
        // the no-prev case).
        if ch != 'へ' && !prev.is_some_and(is_kanji) && after.len() == 2 && is_katakana(after[1]) {
            return hiragana_lookalike(ch);
        }
    }
    None
}

/// Runs of "ー" that can only be a dash become "―".
fn fix_dashes(text: &[char]) -> Vec<char> {
    let mut out = text.to_vec();
    if !text.contains(&LONG_VOWEL) {
        return out;
    }
    let mut i = 0;
    while i < text.len() {
        if text[i] != LONG_VOWEL {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < text.len() && text[j] == LONG_VOWEL {
            j += 1;
        }
        let prev = if i > 0 { Some(text[i - 1]) } else { None };
        let nxt = text.get(j).copied();
        let dash = match prev {
            None => true,
            Some(p) if !is_kana(p) || p == DASH => true,
            Some(p) if is_hiragana(p) => {
                let runs_on = nxt.is_some_and(|n| {
                    is_kana(n) || is_kanji(n) || is_space(n) || "」』。".contains(n)
                });
                j - i >= 2 && runs_on
            }
            Some(_) => false,
        };
        if dash {
            for c in &mut out[i..j] {
                *c = DASH;
            }
        }
        i = j;
    }
    out
}

/// A line's text as written to the sidecar: the recognizer's systematic slips
/// undone. Every rule maps one character to one character.
pub fn normalize_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let text = fix_kana_lookalikes(&chars);
    let mut out = text.clone();
    let n = text.len();
    for (i, &ch) in text.iter().enumerate() {
        if hiragana_lookalike(ch).is_some() {
            out[i] = katakana_for(&text, i).unwrap_or(ch);
        } else if let Some(full) = small_kana(ch)
            && i > 0
            && (is_kana(text[i - 1]) || is_kanji(text[i - 1]))
        {
            if !SMALL_KANA_HOSTS.contains(text[i - 1]) {
                out[i] = full;
            }
        } else if ch == '—' && n > 1 {
            let mut around = String::new();
            if i > 0 {
                around.push(text[i - 1]);
            }
            if i + 1 < n {
                around.push(text[i + 1]);
            }
            if !is_ascii(&around) {
                out[i] = DASH;
            }
        } else if ch == ' '
            && 0 < i
            && i + 1 < n
            && !text[i - 1].is_ascii()
            && !text[i + 1].is_ascii()
        {
            out[i] = '\u{3000}';
        }
    }
    fix_dashes(&out).into_iter().collect()
}

/// Could this text be a ruby run or a run of emphasis dots?
pub fn is_ruby_script(text: &str) -> bool {
    let stripped = text.trim_start_matches(['。', '．']);
    let mut glyphs = stripped.chars().filter(|c| !is_space(*c)).peekable();
    if glyphs.peek().is_none() {
        return false;
    }
    glyphs.all(|c| is_kana(c) || BOUTEN.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_examples() {
        assert_eq!(normalize_text("夕イル"), "タイル");
        assert_eq!(normalize_text("全力パンチ"), "全力パンチ");
        assert_eq!(normalize_text("ケイきを"), "ケイキを");
        assert_eq!(normalize_text("べルト"), "ベルト");
        assert_eq!(normalize_text("ーあれ"), "―あれ");
        assert_eq!(normalize_text("すげーー！"), "すげーー！");
        assert_eq!(normalize_text("何かにーーそれ"), "何かに――それ");
    }
}
