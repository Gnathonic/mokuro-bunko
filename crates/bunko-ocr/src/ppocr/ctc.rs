//! CTC decoding and the recognizer's batching/windowing rules (`ppocr.py:1080-1348`,
//! spec §4.3–§4.7).
//!
//! The recognizer's `[T, 18710]` softmax rows are reduced right away to what every
//! later step reads — the argmax class, its probability and the blank probability —
//! so a 2000-px window costs 250 × 12 bytes instead of 250 × 75 kB.

use crate::py;

pub const REC_HEIGHT: usize = 48;
pub const REC_STRIDE: usize = 8;
pub const REC_MIN_WIDTH: usize = 16;
pub const REC_MAX_BATCH: usize = 16;
pub const REC_WINDOW: usize = 2000;
pub const REC_WINDOW_OVERLAP: usize = 192;
pub const REC_WINDOW_GUARD: usize = 3;

pub const GAP_MIN_GLYPHS: usize = 6;
pub const GAP_MIN_PITCH: f64 = 5.0;
pub const GAP_MIN_RATIO: f64 = 1.5;
pub const GAP_MAX_FILL: i64 = 3;
pub const GAP_BLANK_MAX_INK: f64 = 0.02;
pub const GAP_DASH_MAX_INK: f64 = 0.12;
pub const GAP_GLYPH_MIN_INK: f64 = 0.2;
pub const GAP_INK_CONTRAST: f32 = 0.5;
pub const IDEOGRAPHIC_SPACE: &str = "\u{3000}";
pub const MISSING_GLYPH: &str = "〓";
pub const DASHES: &str = "―—ー";
pub const FOREIGN_MAX_CONF: f64 = 0.5;

/// One timestep of recognizer output, reduced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    /// `argmax` over classes (first maximum wins).
    pub cls: u32,
    /// Probability of `cls`.
    pub conf: f32,
    /// Probability of the CTC blank (class 0).
    pub blank: f32,
}

/// Reduce `[T, classes]` probabilities to steps.
pub fn reduce_rows(probs: &[f32], classes: usize) -> Vec<Step> {
    probs
        .chunks_exact(classes)
        .map(|row| {
            let mut best = 0usize;
            let mut best_v = row[0];
            for (i, &v) in row.iter().enumerate().skip(1) {
                if v > best_v {
                    best = i;
                    best_v = v;
                }
            }
            Step {
                cls: best as u32,
                conf: best_v,
                blank: row[0],
            }
        })
        .collect()
}

/// Input width for a crop at height 48: aspect kept, rounded UP to the stride.
pub fn recognizer_width(crop_w: usize, crop_h: usize) -> usize {
    let raw = (REC_HEIGHT * crop_w) as f64 / crop_h.max(1) as f64;
    let width = (raw / REC_STRIDE as f64).ceil() as usize * REC_STRIDE;
    width.max(REC_MIN_WIDTH)
}

/// One decoded character.
#[derive(Debug, Clone, PartialEq)]
pub struct CtcChar {
    pub ch: String,
    pub conf: f64,
    pub t: i64,
}

/// Greedy CTC: a run of one class is one character with the run's strongest step.
pub fn ctc_greedy(steps: &[Step], vocab: &[String]) -> Vec<CtcChar> {
    let mut out: Vec<CtcChar> = Vec::new();
    let mut prev = 0u32;
    for (t, s) in steps.iter().enumerate() {
        if s.cls != 0 {
            if s.cls == prev && !out.is_empty() {
                let last = out.len() - 1;
                if s.conf as f64 > out[last].conf {
                    out[last].conf = s.conf as f64;
                    out[last].t = t as i64;
                }
            } else {
                let ch = vocab
                    .get(s.cls as usize)
                    .cloned()
                    .unwrap_or_else(|| "\u{fffd}".to_string());
                out.push(CtcChar {
                    ch,
                    conf: s.conf as f64,
                    t: t as i64,
                });
            }
        }
        prev = s.cls;
    }
    out
}

fn single_char(s: &str) -> Option<char> {
    let mut it = s.chars();
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// Kana or CJK ideograph.
pub fn is_japanese(s: &str) -> bool {
    single_char(s)
        .is_some_and(|c| matches!(c as u32, 0x3041..=0x30FF | 0x3400..=0x9FFF | 0xF900..=0xFAFF))
}

pub fn is_kanji_char(c: char) -> bool {
    matches!(c as u32, 0x3400..=0x9FFF | 0xF900..=0xFAFF)
}

fn is_latin(s: &str) -> bool {
    single_char(s).is_some_and(|c| {
        c.is_ascii_alphabetic() || matches!(c as u32, 0xFF21..=0xFF3A | 0xFF41..=0xFF5A)
    })
}

/// Low-confidence Latin letters between Japanese glyphs become `〓`.
pub fn doubt_foreign_glyphs(chars: Vec<CtcChar>) -> Vec<CtcChar> {
    let mut out = chars;
    for i in 1..out.len().saturating_sub(1) {
        if out[i].conf < FOREIGN_MAX_CONF
            && is_latin(&out[i].ch)
            && is_japanese(&out[i - 1].ch)
            && is_japanese(&out[i + 1].ch)
        {
            out[i].ch = MISSING_GLYPH.to_string();
        }
    }
    out
}

/// The recognizer input a line was decoded from: `[3, 48, W]` float32, CHW.
pub struct RecTensor {
    pub width: usize,
    pub data: Vec<f32>,
}

/// Give a line back the cells the decoder skipped (spec §4.7).
pub fn fill_gaps(chars: Vec<CtcChar>, tensor: &RecTensor) -> Vec<CtcChar> {
    if chars.len() < 2 {
        return chars;
    }
    let steps: Vec<i64> = chars.windows(2).map(|w| w[1].t - w[0].t).collect();
    let dashes_only = chars.len() < GAP_MIN_GLYPHS;
    let pitch = if dashes_only {
        (REC_HEIGHT / REC_STRIDE) as f64
    } else {
        py::median(&steps.iter().map(|&s| s as f64).collect::<Vec<_>>()).unwrap_or(0.0)
    };
    if pitch < GAP_MIN_PITCH || !steps.iter().any(|&s| s as f64 > GAP_MIN_RATIO * pitch) {
        return chars;
    }
    let w = tensor.width;
    let plane = REC_HEIGHT * w;
    let grey: Vec<f32> = (0..plane)
        .map(|i| (tensor.data[i] + tensor.data[plane + i] + tensor.data[2 * plane + i]) / 3.0)
        .collect();
    let med = median_f32(&grey);
    let ink: Vec<bool> = grey
        .iter()
        .map(|&g| (g - med).abs() > GAP_INK_CONTRAST)
        .collect();
    let mut out = vec![chars[0].clone()];
    for pair in chars.windows(2) {
        let (prev, nxt) = (&pair[0], &pair[1]);
        let step = nxt.t - prev.t;
        if step as f64 > GAP_MIN_RATIO * pitch {
            let lo = (prev.t as f64 + pitch / 2.0) as i64;
            let hi = (nxt.t as f64 - pitch / 2.0) as i64;
            let (c0, c1) = slice_bounds(lo * REC_STRIDE as i64, (hi + 1) * REC_STRIDE as i64, w);
            let share = if c1 > c0 {
                let mut n = 0usize;
                for row in 0..REC_HEIGHT {
                    n += ink[row * w + c0..row * w + c1]
                        .iter()
                        .filter(|&&b| b)
                        .count();
                }
                n as f64 / (REC_HEIGHT * (c1 - c0)) as f64
            } else {
                1.0
            };
            let count = GAP_MAX_FILL.min((py::round_int(step as f64 / pitch) - 1).max(1));
            let dash = if DASHES.contains(prev.ch.as_str()) {
                Some(prev)
            } else if DASHES.contains(nxt.ch.as_str()) {
                Some(nxt)
            } else {
                None
            };
            let fill: Option<(String, f64)> = match dash {
                Some(d) if (GAP_BLANK_MAX_INK..GAP_DASH_MAX_INK).contains(&share) => {
                    Some((d.ch.clone(), d.conf))
                }
                _ if dashes_only => None,
                _ if share < GAP_BLANK_MAX_INK => Some((IDEOGRAPHIC_SPACE.to_string(), 1.0)),
                _ if share >= GAP_GLYPH_MIN_INK => Some((MISSING_GLYPH.to_string(), 0.0)),
                _ => None,
            };
            if let Some((ch, conf)) = fill {
                for k in 0..count {
                    let t = prev.t + py::round_int(((k + 1) * step) as f64 / (count + 1) as f64);
                    out.push(CtcChar {
                        ch: ch.clone(),
                        conf,
                        t,
                    });
                }
            }
        }
        out.push(nxt.clone());
    }
    out
}

/// numpy slice `[a:b]` bounds on an axis of length `n`.
fn slice_bounds(a: i64, b: i64, n: usize) -> (usize, usize) {
    let n = n as i64;
    let norm = |v: i64| if v < 0 { (v + n).max(0) } else { v.min(n) };
    let (a, b) = (norm(a), norm(b));
    (a as usize, b.max(a) as usize)
}

/// `np.median` of a float32 array: the mean of the middle two in float32.
fn median_f32(values: &[f32]) -> f32 {
    let mut v = values.to_vec();
    let n = v.len();
    if n == 0 {
        return f32::NAN;
    }
    let mid = n / 2;
    let (_, m, _) = v.select_nth_unstable_by(mid, f32::total_cmp);
    let upper = *m;
    if n % 2 == 1 {
        return upper;
    }
    let lower = v[..mid].iter().copied().fold(f32::MIN, f32::max);
    (lower + upper) / 2.0
}

/// `(start, end)` timestep spans covering `total` with the window overlap.
pub fn window_spans(total: usize) -> Vec<(usize, usize)> {
    let window = REC_WINDOW / REC_STRIDE;
    let overlap = REC_WINDOW_OVERLAP / REC_STRIDE;
    if total <= window + overlap {
        return vec![(0, total)];
    }
    let step = window - overlap;
    let count = ((total - overlap) as f64 / step as f64).ceil() as usize;
    let step_f = (total - window) as f64 / (count - 1) as f64;
    (0..count)
        .map(|i| {
            let r = py::round_int(i as f64 * step_f) as usize;
            (r, r + window)
        })
        .collect()
}

/// `(class, first, last)` of every non-blank run, in full-line timesteps.
fn class_runs(steps: &[Step], offset: usize) -> Vec<(u32, usize, usize)> {
    let mut runs: Vec<(u32, usize, usize)> = Vec::new();
    for (t, s) in steps.iter().enumerate() {
        if s.cls == 0 {
            continue;
        }
        let at = offset + t;
        match runs.last_mut() {
            Some(last) if last.0 == s.cls && last.2 + 1 == at => last.2 = at,
            _ => runs.push((s.cls, at, at)),
        }
    }
    runs
}

/// Timestep in `[lo, hi]` at which to switch from `left` to `right`.
fn choose_cut(left: &[Step], right: &[Step], lo: usize, hi: usize) -> usize {
    let runs_l = class_runs(left, lo);
    let runs_r = class_runs(right, lo);
    let centre = (lo + hi) as f64 / 2.0;
    let mut best: Option<((i32, f64), usize)> = None;
    for cut in lo..=hi {
        if runs_l
            .iter()
            .chain(runs_r.iter())
            .any(|&(_, first, last)| first < cut && cut <= last)
        {
            continue;
        }
        let before = |runs: &[(u32, usize, usize)]| {
            runs.iter()
                .filter(|r| r.2 < cut)
                .map(|r| r.0)
                .collect::<Vec<_>>()
        };
        let after = |runs: &[(u32, usize, usize)]| {
            runs.iter()
                .filter(|r| r.1 >= cut)
                .map(|r| r.0)
                .collect::<Vec<_>>()
        };
        let score =
            (before(&runs_l) == before(&runs_r)) as i32 + (after(&runs_l) == after(&runs_r)) as i32;
        let key = (score, -(cut as f64 - centre).abs());
        let better = match &best {
            None => true,
            Some((k, _)) => key.0 > k.0 || (key.0 == k.0 && key.1 > k.1),
        };
        if better {
            best = Some((key, cut));
        }
    }
    if let Some((_, cut)) = best {
        return cut;
    }
    // argmax of min(blank_l, blank_r), first maximum.
    let mut arg = 0;
    let mut best_v = f32::MIN;
    for (i, (l, r)) in left.iter().zip(right).enumerate() {
        let v = l.blank.min(r.blank);
        if v > best_v {
            best_v = v;
            arg = i;
        }
    }
    lo + arg
}

/// Join per-window steps into one line.
pub fn stitch_windows(windows: &[Vec<Step>], spans: &[(usize, usize)]) -> Vec<Step> {
    let total = spans.last().map_or(0, |s| s.1);
    let mut out = vec![
        Step {
            cls: 0,
            conf: 0.0,
            blank: 0.0
        };
        total
    ];
    let guard = REC_WINDOW_GUARD;
    let mut start_at = 0usize;
    for (idx, (win, &(s, e))) in windows.iter().zip(spans).enumerate() {
        let win = &win[..(e - s).min(win.len())];
        let cut = if idx + 1 < spans.len() {
            let next_s = spans[idx + 1].0;
            let (mut lo, mut hi) = (next_s + guard, e.saturating_sub(guard));
            if hi <= lo {
                lo = next_s;
                hi = e;
            }
            let other = &windows[idx + 1];
            choose_cut(
                &win[lo - s..hi - s],
                &other[lo - next_s..hi - next_s],
                lo,
                hi,
            )
        } else {
            e
        };
        if cut > start_at {
            out[start_at..cut].copy_from_slice(&win[start_at - s..cut - s]);
        }
        start_at = cut;
    }
    out
}

/// Batches of piece indices: widest first (stable), only equal widths together
/// (`REC_MAX_PAD_WASTE = 0`), at most 16 per batch.
pub fn plan_batches(widths: &[usize]) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..widths.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(widths[i]));
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for i in order {
        if let Some(&first) = current.first()
            && (current.len() >= REC_MAX_BATCH || widths[first] != widths[i])
        {
            batches.push(std::mem::take(&mut current));
        }
        current.push(i);
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(cls: u32, conf: f32) -> Step {
        Step {
            cls,
            conf,
            blank: 1.0 - conf,
        }
    }

    #[test]
    fn greedy_merges_runs() {
        let vocab: Vec<String> = ["", "a", "b", " "].iter().map(|s| s.to_string()).collect();
        let steps = [st(1, 0.5), st(1, 0.9), st(0, 0.9), st(1, 0.7), st(2, 0.6)];
        let out = ctc_greedy(&steps, &vocab);
        assert_eq!(out.iter().map(|c| c.ch.as_str()).collect::<String>(), "aab");
        assert_eq!(out[0].t, 1);
        assert_eq!(out[0].conf, 0.9f32 as f64);
    }

    #[test]
    fn spans_and_widths() {
        assert_eq!(window_spans(274), vec![(0, 274)]);
        let s = window_spans(600);
        assert_eq!(s.first(), Some(&(0, 250)));
        assert_eq!(s.last().map(|x| x.1), Some(600));
        assert_eq!(recognizer_width(100, 48), 104);
        assert_eq!(recognizer_width(2, 48), 16);
        assert_eq!(
            plan_batches(&[16, 32, 16, 32]),
            vec![vec![1, 3], vec![0, 2]]
        );
    }

    #[test]
    fn foreign_glyph() {
        let c = |ch: &str, conf: f64| CtcChar {
            ch: ch.into(),
            conf,
            t: 0,
        };
        let out = doubt_foreign_glyphs(vec![c("あ", 0.9), c("w", 0.2), c("む", 0.9)]);
        assert_eq!(out[1].ch, MISSING_GLYPH);
    }
}
