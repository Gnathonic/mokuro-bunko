//! Furigana removal (`line_layout.py:813-986`).

use std::collections::{BTreeMap, HashMap};

use crate::py::{fcmp, max2, median, min2};
use crate::script::{glyph_count, has_kanji, is_ruby_script};

use super::consts::*;
use super::line::{Line, Quad, box_distance, dominant_vertical, overlap};

/// A removed furigana run, kept so a later format can emit `<ruby>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Ruby {
    /// Index of the ruby line in the raw list.
    pub line: usize,
    /// Index of the base line it annotates.
    pub base: usize,
    pub text: String,
    pub quad: Quad,
    /// The annotated stretch of the base, as fractions of its length.
    pub span: (f64, f64),
    /// That stretch on the base text's character grid, `[start, end)`.
    pub chars: (i64, i64),
}

/// `(pitch, em)` of the page's column lattice, `None` when it has none.
pub fn column_lattice(lines: &[&Line]) -> Option<(f64, f64)> {
    let long: Vec<&Line> = lines.iter().copied().filter(|l| l.length() >= LONG_LINE_EM * l.thickness()).collect();
    if long.is_empty() {
        return None;
    }
    let vertical = dominant_vertical(long.iter().copied());
    let long: Vec<&Line> = long.into_iter().filter(|l| l.vertical == vertical).collect();
    if long.len() < BODY_MIN_LONG_COLUMNS {
        return None;
    }
    let theta = median(long.iter().map(|l| l.angle))?;
    let em = median(long.iter().map(|l| l.thickness()))?;
    let mut centres: Vec<f64> = Vec::new();
    for l in lines {
        if l.vertical != vertical || is_ruby_script(&l.text) {
            continue;
        }
        let (_, _, c0, c1) = l.main_cross(theta, vertical);
        if max2(c1 - c0, em) / min2(c1 - c0, em) <= BODY_SIZE_RATIO {
            centres.push((c0 + c1) / 2.0);
        }
    }
    centres.sort_by(|a, b| fcmp(*a, *b));
    let steps: Vec<f64> = centres
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| 0.5 * em < *d && *d < 3.0 * em)
        .collect();
    if steps.len() < BODY_MIN_LONG_COLUMNS {
        return None;
    }
    Some((median(steps)?, em))
}

/// The thickness ratio this line must stay under to be ruby; `None` = never.
fn ruby_candidacy(line: &Line, lattice_em: Option<f64>) -> Option<f64> {
    if is_ruby_script(&line.text) {
        return Some(FURIGANA_MAX_THICKNESS_RATIO);
    }
    if let Some(em) = lattice_em
        && line.thickness() <= FURIGANA_LATTICE_MAX_BODY_RATIO * em
    {
        return Some(FURIGANA_MAX_THICKNESS_RATIO);
    }
    if glyph_count(&line.text) > FURIGANA_UNREADABLE_MAX_GLYPHS {
        return None;
    }
    if line.conf < LOW_CONFIDENCE {
        return Some(FURIGANA_MAX_THICKNESS_RATIO);
    }
    Some(if lattice_em.is_some() { FURIGANA_UNREADABLE_MAX_RATIO } else { FURIGANA_TINY_MAX_RATIO })
}

/// If `cand` is furigana for `base`: the annotated span, as base fractions.
pub fn ruby_of(cand: &Line, base: &Line, pitch: Option<f64>, max_ratio: f64) -> Option<(f64, f64)> {
    let theta = if base.angle_reliable() { base.angle } else { 0.0 };
    let vertical = base.vertical;
    let (bm0, bm1, bc0, bc1) = base.main_cross(theta, vertical);
    let (cm0, cm1, cc0, cc1) = cand.main_cross(theta, vertical);
    let base_t = bc1 - bc0;
    let cand_t = cc1 - cc0;
    if base_t <= 0.0 || cand_t <= 0.0 {
        return None;
    }
    let ratio = cand_t / base_t;
    let side = if vertical { 1.0 } else { -1.0 };
    let offset = side * ((cc0 + cc1) / 2.0 - (bc0 + bc1) / 2.0);
    if offset < FURIGANA_MIN_CENTRE_OFFSET_EM * base_t {
        return None;
    }
    let gap = if vertical { cc0 - bc1 } else { bc0 - cc1 };
    if gap >= FURIGANA_MAX_GAP_EM * base_t {
        return None;
    }
    if overlap(bm0, bm1, cm0, cm1) < FURIGANA_MIN_MAIN_OVERLAP * (cm1 - cm0) {
        return None;
    }
    let thin = ratio <= max_ratio;
    let readable = is_ruby_script(&cand.text);
    let generous_box = readable && ratio <= FURIGANA_GENEROUS_MAX_THICKNESS_RATIO;
    let glyphs = glyph_count(&cand.text);
    let base_glyphs = glyph_count(&base.text);
    let small_glyphs = generous_box
        && glyphs >= 2
        && base_glyphs >= 1
        && (cm1 - cm0) / glyphs as f64 <= FURIGANA_MAX_GLYPH_PITCH_RATIO * (bm1 - bm0) / base_glyphs as f64;
    let on_lattice_gap =
        generous_box && pitch.is_some_and(|p| offset <= FURIGANA_LATTICE_MAX_PITCH_FRACTION * p);
    if !(thin || small_glyphs || on_lattice_gap) {
        return None;
    }
    let length = bm1 - bm0;
    let start = min2(max2((cm0 - bm0) / length, 0.0), 1.0);
    let end = min2(max2((cm1 - bm0) / length, 0.0), 1.0);
    Some((start, end))
}

/// Split `lines` into kept line positions and removed ruby runs.
///
/// Returns the positions (into `lines`) of the kept lines and the ruby runs
/// sorted by line index.
pub fn filter_furigana(lines: &[Line]) -> (Vec<usize>, Vec<Ruby>) {
    let refs: Vec<&Line> = lines.iter().collect();
    let lattice = column_lattice(&refs);
    let (pitch, lattice_em) = match lattice {
        Some((p, e)) => (Some(p), Some(e)),
        None => (None, None),
    };
    let limits: HashMap<usize, Option<f64>> = lines.iter().map(|l| (l.index, ruby_candidacy(l, lattice_em))).collect();
    let mut ruby: BTreeMap<usize, Ruby> = BTreeMap::new();
    for final_pass in [false, true] {
        let bases: Vec<&Line> = lines
            .iter()
            .filter(|l| has_kanji(&l.text) && !ruby.contains_key(&l.index) && (limits[&l.index].is_none() || final_pass))
            .collect();
        for cand in lines {
            let Some(limit) = limits[&cand.index] else {
                continue;
            };
            if ruby.contains_key(&cand.index) {
                continue;
            }
            let mut best: Option<(f64, usize, &Line, (f64, f64))> = None;
            for base in &bases {
                if base.index == cand.index {
                    continue;
                }
                let Some(span) = ruby_of(cand, base, pitch, limit) else {
                    continue;
                };
                let dist = box_distance(cand, base);
                let better = match &best {
                    None => true,
                    Some((bd, bi, _, _)) => dist < *bd || (dist == *bd && base.index < *bi),
                };
                if better {
                    best = Some((dist, base.index, base, span));
                }
            }
            let Some((_, _, base_line, span)) = best else {
                continue;
            };
            let n = base_line.text.chars().count() as f64;
            let chars = (
                min2(n, (span.0 * n + 1e-6).floor()) as i64,
                min2(n, (span.1 * n - 1e-6).ceil()) as i64,
            );
            ruby.insert(
                cand.index,
                Ruby { line: cand.index, base: base_line.index, text: cand.text.clone(), quad: cand.quad, span, chars },
            );
        }
    }
    let kept = (0..lines.len()).filter(|&i| !ruby.contains_key(&lines[i].index)).collect();
    (kept, ruby.into_values().collect())
}
