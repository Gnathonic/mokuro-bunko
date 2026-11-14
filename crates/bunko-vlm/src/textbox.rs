//! The preprocessing of the text-box recognizers, **manga-ocr-charspot-small** and
//! **manga-ocr-nar-small** (muscgab), shared by the ONNX Runtime route
//! (`bunko-engines`, CPU) and the libtorch route (`bunko-torch`, GPU), so both build
//! bit-identical tensors. It follows the pre- and post-processing of the Apache-2.0
//! inference code the models are published with.
//!
//! The crop is scaled to fit a grid (aspect kept, Pillow's bicubic), put in the grid's
//! top-left corner and the rest filled with the median colour of the scaled crop's
//! border.

use std::ops::Range;
use std::path::Path;

use crate::resample::{Filter, resize};
use crate::{Rgb, VlmError};

/// charspot: the side of a grid cell in pixels, and how many cells a crop may take.
pub const CHARSPOT_CELL: usize = 16;
pub const CHARSPOT_BUDGET: usize = 256;
/// charspot's classes: the vocabulary padded to this many, then the `!?` glyphs.
pub const CHARSPOT_CLASSES: usize = 6818;
/// nar-small: the side of a patch, how many patches a crop may take, and the output
/// positions (the last may be the end mark).
pub const NAR_PATCH: usize = 14;
pub const NAR_BUDGET: usize = 324;
pub const NAR_SLOTS: usize = 161;
/// The normalisation, digit for digit as the model's package writes it.
pub const NAR_MEAN: [f32; 3] = [0.481_454_66, 0.457_827_5, 0.408_210_73];
#[allow(clippy::excessive_precision)]
pub const NAR_STD: [f32; 3] = [0.268_629_54, 0.261_302_58, 0.275_777_11];

fn asset(what: &str, path: &Path, e: impl std::fmt::Display) -> VlmError {
    VlmError::Asset(format!("{what} {}: {e}", path.display()))
}

/// The `characters` list of a vocabulary file.
pub fn load_characters(path: &Path) -> Result<Vec<String>, VlmError> {
    let text =
        std::fs::read_to_string(path).map_err(|e| VlmError::Io(path.display().to_string(), e))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| asset("vocabulary", path, e))?;
    let chars: Option<Vec<String>> = value["characters"].as_array().map(|a| {
        a.iter()
            .map(|c| c.as_str().unwrap_or_default().to_string())
            .collect()
    });
    chars
        .filter(|c| !c.is_empty())
        .ok_or_else(|| asset("vocabulary", path, "no characters"))
}

/// The largest scale at which a `height` x `width` crop, cut into 14 px cells, takes
/// no more than `budget` cells: `(scale, rows, columns)`. Both models size their input
/// with this search (charspot then draws 16 px cells).
pub fn fit_grid(height: usize, width: usize, budget: usize) -> (f64, usize, usize) {
    let cells = |v: usize, s: f64| (v as f64 * s / 14.0).ceil() as usize;
    let (mut lo, mut hi) = (1e-4f64, 1000.0f64);
    for _ in 0..60 {
        let mid = (lo + hi) / 2.0;
        if cells(height, mid) * cells(width, mid) <= budget {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo, cells(height, lo), cells(width, lo))
}

/// Scale `crop` to `scaled` and put it in the top-left corner of a `canvas` sized
/// image filled with the median colour of the scaled crop's border.
pub fn on_canvas(crop: &Rgb, scaled: (usize, usize), canvas: (usize, usize)) -> Rgb {
    let (rw, rh) = scaled;
    let small = resize(crop, rw, rh, Filter::Bicubic);
    // The top and bottom rows and the left and right columns, corners twice.
    let mut fill = [0u8; 3];
    for (c, out) in fill.iter_mut().enumerate() {
        let mut border: Vec<u8> = Vec::with_capacity(2 * (rw + rh));
        for y in [0, rh - 1] {
            border.extend((0..rw).map(|x| small.data[(y * rw + x) * 3 + c]));
        }
        for x in [0, rw - 1] {
            border.extend((0..rh).map(|y| small.data[(y * rw + x) * 3 + c]));
        }
        border.sort_unstable();
        // An even count: the mean of the two middle values, cut to a whole number.
        let mid = border.len() / 2;
        *out = ((u16::from(border[mid - 1]) + u16::from(border[mid])) / 2) as u8;
    }
    let (cw, ch) = canvas;
    let mut data = Vec::with_capacity(cw * ch * 3);
    for _ in 0..cw * ch {
        data.extend_from_slice(&fill);
    }
    for y in 0..rh {
        let row = &small.data[y * rw * 3..(y + 1) * rw * 3];
        data[y * cw * 3..y * cw * 3 + rw * 3].copy_from_slice(row);
    }
    Rgb {
        width: cw,
        height: ch,
        data,
    }
}

fn round(x: f64) -> usize {
    x.round_ties_even().max(0.0) as usize
}

/// Whether a crop has no pixels (a block box of zero width or height): it reads `""`
/// without a model call.
pub fn empty(crop: &Rgb) -> bool {
    crop.width == 0 || crop.height == 0
}

// --- charspot ----------------------------------------------------------------
//
// Attribution (Apache License 2.0, section 4). The charspot code in this section
// (`charspot_canvas`, the decode `charspot_detect` / `charspot_nms`, the reading-order
// `charspot_chain`, the class table `charspot_classes`, and the canvas batching beside
// them) is a port to Rust of `manga_ocr_charspot_small/core.py` and
// `manga_ocr_charspot_small/backends/cuda.py` from the Hugging Face repository
// `muscgab/manga-ocr-charspot-small` at revision
// `72ca29202673880ca9c67b1c02c107535be4472b`, which are licensed under the Apache
// License, Version 2.0 (https://www.apache.org/licenses/LICENSE-2.0). Changed from the
// original: rewritten in Rust, the numpy arithmetic kept in the same dtypes, and the
// batched NMS, chain and canvas grouping run per crop on the host. This notice stays
// with the port. (The model's weights are CC BY-NC-SA 4.0 and are not part of this
// code.)

/// charspot's input for a crop: the canvas, and the factors that take canvas pixels
/// back to crop pixels (`x`, `y`).
pub fn charspot_canvas(crop: &Rgb) -> (Rgb, (f32, f32)) {
    let (s, rows, cols) = fit_grid(crop.height, crop.width, CHARSPOT_BUDGET);
    let s = s * CHARSPOT_CELL as f64 / 14.0;
    let rw = round(crop.width as f64 * s).clamp(1, cols * CHARSPOT_CELL);
    let rh = round(crop.height as f64 * s).clamp(1, rows * CHARSPOT_CELL);
    let canvas = on_canvas(crop, (rw, rh), (cols * CHARSPOT_CELL, rows * CHARSPOT_CELL));
    let back = (
        (crop.width as f64 / rw as f64) as f32,
        (crop.height as f64 / rh as f64) as f32,
    );
    (canvas, back)
}

/// A character centre is a heat value strictly above this (`core.HEAT_THR`).
pub const CHARSPOT_HEAT_THR: f32 = 0.3;
/// A kept box removes another whose IoU with it is at least this (`core.NMS_IOU`).
pub const CHARSPOT_NMS_IOU: f32 = 0.2;
/// A box whose furigana score is at least this is dropped (`core.RUBY_THR`).
pub const CHARSPOT_RUBY_THR: f32 = 0.5;
/// The dense heads' stride in canvas pixels.
pub const CHARSPOT_STRIDE: usize = 8;

/// One crop's dense maps on its canvas: channel-major `height x width` cells at stride
/// 8; `heat` and `ruby` one channel each, `boxes` four (centre offsets `x`, `y` in
/// cells, then the log sizes `w`, `h` in cells).
#[derive(Debug, Clone, Copy)]
pub struct DenseMaps<'a> {
    pub heat: &'a [f32],
    pub boxes: &'a [f32],
    pub ruby: &'a [f32],
    pub height: usize,
    pub width: usize,
}

/// The characters charspot keeps in a crop, in NMS keep order: boxes
/// `[x0, y0, x1, y1]` in canvas pixels, their centre scores, and the geometry the tail
/// graph reads (centre `x`, `y`, width, height over the lower median of the larger
/// sides).
#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    pub boxes: Vec<[f32; 4]>,
    pub scores: Vec<f32>,
    pub geo: Vec<[f32; 4]>,
}

/// `exp` of an f32, correctly rounded to f32 (through f64), so it is the same on every
/// platform. numpy's own vectorised f32 `exp` is up to 2 ulp from this; the boxes then
/// differ in the last bit at most, far inside the route's 0.5 px tolerance.
fn exp_f32(x: f32) -> f32 {
    (f64::from(x).exp()) as f32
}

/// numpy's `clip(x, lo, None)`, which keeps a NaN.
fn clip_low(x: f32, lo: f32) -> f32 {
    if x < lo { lo } else { x }
}

/// numpy's `maximum` / `minimum`, which keep a NaN.
fn np_max(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else {
        a.max(b)
    }
}

fn np_min(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        f32::NAN
    } else {
        a.min(b)
    }
}

/// numpy's `argmax`: the first index of the largest value, or of the first NaN.
fn argmax(values: impl IntoIterator<Item = f32>) -> usize {
    let mut best = (0, f32::NEG_INFINITY);
    for (i, v) in values.into_iter().enumerate() {
        if v.is_nan() {
            return i;
        }
        if i == 0 || v > best.1 {
            best = (i, v);
        }
    }
    best.0
}

/// `core.detect` over the crop's own `h8 x w8` region in the top-left corner of its
/// canvas maps: the centres above the threshold in row-major order, their boxes
/// (centres `(j + dx) * 8` in f64, sizes `exp(s) * 8` in f32, then cast to f32), NMS
/// over all of them, then the furigana dropped. `None` when nothing is kept.
pub fn charspot_detect(maps: &DenseMaps, h8: usize, w8: usize) -> Option<Detected> {
    let (h8, w8) = (h8.min(maps.height), w8.min(maps.width));
    let plane = maps.height * maps.width;
    let stride = CHARSPOT_STRIDE as f64;
    let mut cells = Vec::new();
    let mut boxes = Vec::new();
    let mut scores = Vec::new();
    for i in 0..h8 {
        for j in 0..w8 {
            let k = i * maps.width + j;
            let score = maps.heat[k];
            if score > CHARSPOT_HEAT_THR {
                let ch = |c: usize| maps.boxes[c * plane + k];
                let cx = (j as f64 + f64::from(ch(0))) * stride;
                let cy = (i as f64 + f64::from(ch(1))) * stride;
                let w = exp_f32(ch(2)) * CHARSPOT_STRIDE as f32;
                let h = exp_f32(ch(3)) * CHARSPOT_STRIDE as f32;
                let (hw, hh) = (f64::from(w / 2.0), f64::from(h / 2.0));
                boxes.push([
                    (cx - hw) as f32,
                    (cy - hh) as f32,
                    (cx + hw) as f32,
                    (cy + hh) as f32,
                ]);
                scores.push(score);
                cells.push(k);
            }
        }
    }
    if cells.is_empty() {
        return None;
    }
    let keep: Vec<usize> = charspot_nms(&boxes, &scores)
        .into_iter()
        .filter(|&k| maps.ruby[cells[k]] < CHARSPOT_RUBY_THR)
        .collect();
    if keep.is_empty() {
        return None;
    }
    let kb: Vec<[f32; 4]> = keep.iter().map(|&k| boxes[k]).collect();
    let sc: Vec<f32> = keep.iter().map(|&k| scores[k]).collect();
    // The lower median of each box's larger side.
    let mut sides: Vec<f32> = kb
        .iter()
        .map(|b| np_max(b[2] - b[0], b[3] - b[1]))
        .collect();
    sides.sort_by(f32::total_cmp);
    let u = sides[(sides.len() - 1) / 2];
    let geo = kb
        .iter()
        .map(|b| {
            [
                (b[0] + b[2]) / 2.0 / u,
                (b[1] + b[3]) / 2.0 / u,
                clip_low(b[2] - b[0], 1.0) / u,
                clip_low(b[3] - b[1], 1.0) / u,
            ]
        })
        .collect();
    Some(Detected {
        boxes: kb,
        scores: sc,
        geo,
    })
}

/// `core.nms`: boxes by score, descending, equal scores in their order (numpy's stable
/// sort); each kept box removes every later one whose IoU with it is at least 0.2
/// (`1e-6` added to the union, in f32) or whose centre lies within a quarter of the
/// kept box's sides of its centre. The kept boxes' indices, in keep order.
pub fn charspot_nms(boxes: &[[f32; 4]], scores: &[f32]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..boxes.len().min(scores.len())).collect();
    // `argsort(-s, kind="stable")`: NaN scores last, as numpy sorts them.
    order.sort_by(|&a, &b| {
        let (x, y) = (-scores[a], -scores[b]);
        match (x.is_nan(), y.is_nan()) {
            (false, false) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            (a, b) => a.cmp(&b),
        }
    });
    let area = |b: &[f32; 4]| clip_low(b[2] - b[0], 0.0) * clip_low(b[3] - b[1], 0.0);
    let suppresses = |a: &[f32; 4], b: &[f32; 4]| {
        let inter = clip_low(np_min(a[2], b[2]) - np_max(a[0], b[0]), 0.0)
            * clip_low(np_min(a[3], b[3]) - np_max(a[1], b[1]), 0.0);
        let iou = inter / (area(a) + area(b) - inter + 1e-6);
        let (acx, acy) = ((a[0] + a[2]) / 2.0, (a[1] + a[3]) / 2.0);
        let (bcx, bcy) = ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0);
        let (hx, hy) = ((a[2] - a[0]) * 0.25, (a[3] - a[1]) * 0.25);
        let inside = (bcx - acx).abs() <= hx && (bcy - acy).abs() <= hy;
        // numpy's `~(iou < thr)`: a NaN IoU suppresses too.
        iou >= CHARSPOT_NMS_IOU || iou.is_nan() || inside
    };
    let mut alive = vec![true; order.len()];
    let mut keep = Vec::new();
    for i in 0..order.len() {
        if !alive[i] {
            continue;
        }
        let kept = &boxes[order[i]];
        keep.push(order[i]);
        for (j, live) in alive.iter_mut().enumerate().skip(i + 1) {
            if *live && suppresses(kept, &boxes[order[j]]) {
                *live = false;
            }
        }
    }
    keep
}

/// `core.chain`: the reading order of `n = start.len()` characters (`pair` row-major
/// `n x n`, `pair[a * n + b]` the score of `b` following `a`). Start at the best start
/// score, follow the best unused successor, and restart at the best unused start when
/// `end[cur]` is strictly greater than that successor's score. Ties go to the first
/// index, as numpy's `argmax`.
pub fn charspot_chain(pair: &[f32], start: &[f32], end: &[f32]) -> Vec<usize> {
    let n = start.len();
    if n == 0 {
        return Vec::new();
    }
    let mut used = vec![false; n];
    let mut cur = argmax(start.iter().copied());
    let mut seq = Vec::with_capacity(n);
    seq.push(cur);
    used[cur] = true;
    while seq.len() < n {
        let row = &pair[cur * n..(cur + 1) * n];
        let next = |k: usize| if used[k] { f32::NEG_INFINITY } else { row[k] };
        let mut j = argmax((0..n).map(next));
        if end[cur] > next(j) {
            let rest: Vec<usize> = (0..n).filter(|&k| !used[k]).collect();
            j = rest[argmax(rest.iter().map(|&k| start[k]))];
        }
        seq.push(j);
        used[j] = true;
        cur = j;
    }
    seq
}

/// charspot's class table (`core.load_vocab`): the vocabulary's characters padded with
/// `""` to [`CHARSPOT_CLASSES`], then the tate-chu-yoko glyphs, every pair and then
/// every triple of `!` and `?`. An error when the vocabulary is longer than that.
pub fn charspot_classes(mut characters: Vec<String>) -> Result<Vec<String>, String> {
    if characters.len() > CHARSPOT_CLASSES {
        return Err(format!(
            "{} characters, more than {CHARSPOT_CLASSES}",
            characters.len()
        ));
    }
    characters.resize(CHARSPOT_CLASSES, String::new());
    let marks = ['!', '?'];
    for a in marks {
        for b in marks {
            characters.push([a, b].iter().collect());
        }
    }
    for a in marks {
        for b in marks {
            for c in marks {
                characters.push([a, b, c].iter().collect());
            }
        }
    }
    Ok(characters)
}

/// The canvas sides charspot's crops are batched on, in cells (`backends/cuda.py`'s
/// `LADDER`): a crop's canvas is zero-padded up to the first of these on each side.
pub const CHARSPOT_LADDER: [usize; 17] = [
    1, 2, 3, 4, 6, 8, 11, 14, 20, 28, 40, 56, 80, 112, 160, 196, 256,
];
/// The cost of one more dense call, in padded cells (`MERGE_OVERHEAD`): two canvas
/// groups merge onto one larger canvas when that saves more than it pads.
pub const CHARSPOT_MERGE_OVERHEAD: usize = 1024;
/// Crops per dense call at most (the authors' `max_batch` on a GPU of 4 GB or more).
pub const CHARSPOT_MAX_BATCH: usize = 64;
/// Rows x characters² per tail call at most (`tail_budget`): it bounds the tail's
/// `[rows, n, n, d]` pair tensor.
pub const CHARSPOT_TAIL_BUDGET: usize = 1 << 18;
/// Rows per tail call at most (`ROWS_PER_TAIL`).
pub const CHARSPOT_ROWS_PER_TAIL: usize = 512;

/// The canvas a crop of `h_px x w_px` is batched on (`backends/cuda.py`'s `canvas`):
/// each side `16 * v` for the first ladder value `v >= px / 16` (floor division, as
/// the authors'). charspot's own canvases are whole cells of at most 256 cells, so the
/// ladder always has a value; past it (which the authors' code would not accept) the
/// side stays `16 * (px / 16)`.
pub fn charspot_canvas_size(h_px: usize, w_px: usize) -> (usize, usize) {
    let up = |px: usize| {
        let cells = px / CHARSPOT_CELL;
        CHARSPOT_CELL
            * CHARSPOT_LADDER
                .iter()
                .copied()
                .find(|&v| v >= cells)
                .unwrap_or(cells)
    };
    (up(h_px), up(w_px))
}

/// The authors' `CudaBackend.batches` without the CUDA-graph canvases: the crops
/// (`shapes`, canvas `(height, width)` in pixels) grouped by [`charspot_canvas_size`]
/// in first-seen order; then, while any pair of groups gains from it, the pair with
/// the largest gain (the first such pair on a tie) merges onto the larger of each side,
/// where `cost(hw, n) = 1024 + n * h * w / 256` (in f64, as the authors' Python) and a
/// merge never holds more than `max_batch` crops; then each group cut into chunks of
/// `max_batch`. Each batch: the crops' indices and the canvas `(height, width)`.
pub fn canvas_batches(
    shapes: &[(usize, usize)],
    max_batch: usize,
) -> Vec<(Vec<usize>, (usize, usize))> {
    let max_batch = max_batch.max(1);
    let mut groups: Vec<((usize, usize), Vec<usize>)> = Vec::new();
    for (i, &(h, w)) in shapes.iter().enumerate() {
        let hw = charspot_canvas_size(h, w);
        match groups.iter_mut().find(|(g, _)| *g == hw) {
            Some((_, idx)) => idx.push(i),
            None => groups.push((hw, vec![i])),
        }
    }
    let cell2 = (CHARSPOT_CELL * CHARSPOT_CELL) as f64;
    let cost = |hw: (usize, usize), n: usize| {
        CHARSPOT_MERGE_OVERHEAD as f64 + (n * hw.0 * hw.1) as f64 / cell2
    };
    loop {
        let mut best: Option<(f64, usize, usize, (usize, usize))> = None;
        for a in 0..groups.len() {
            for b in a + 1..groups.len() {
                let ((ha, ia), (hb, ib)) = (&groups[a], &groups[b]);
                if ia.len() + ib.len() > max_batch {
                    continue;
                }
                let m = (ha.0.max(hb.0), ha.1.max(hb.1));
                let gain = cost(*ha, ia.len()) + cost(*hb, ib.len()) - cost(m, ia.len() + ib.len());
                if gain > 0.0 && best.is_none_or(|(g, ..)| gain > g) {
                    best = Some((gain, a, b, m));
                }
            }
        }
        let Some((_, a, b, m)) = best else { break };
        let (_, moved) = groups.remove(b);
        groups[a].0 = m;
        groups[a].1.extend(moved);
    }
    groups
        .into_iter()
        .flat_map(|(hw, idx)| {
            idx.chunks(max_batch)
                .map(|c| (c.to_vec(), hw))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The crops' rows grouped into tail calls, as the authors' `infer` does
/// (`backends/cuda.py`): the rows with characters (`counts[i] > 0`), most characters
/// first (a stable sort, so equal counts keep their order); each part takes its first
/// row whatever its size, then the next rows while it holds fewer than `rows_max` and
/// `(rows + 1) * n0²` stays within `budget`, `n0` the part's first count. A row with
/// more than 512 characters (whose `n0²` alone is over 2^18) is a part of its own and
/// still runs. Each part: indices into `counts`.
pub fn tail_parts(counts: &[usize], budget: usize, rows_max: usize) -> Vec<Vec<usize>> {
    let mut rows: Vec<usize> = (0..counts.len()).filter(|&i| counts[i] > 0).collect();
    rows.sort_by_key(|&i| std::cmp::Reverse(counts[i]));
    let mut parts = Vec::new();
    let mut rest = &rows[..];
    while let Some(&first) = rest.first() {
        let n0 = counts[first];
        let mut len = 1;
        while len < rest.len()
            && len < rows_max
            && (len + 1).saturating_mul(n0).saturating_mul(n0) <= budget
        {
            len += 1;
        }
        parts.push(rest[..len].to_vec());
        rest = &rest[len..];
    }
    parts
}

/// The characters a tail part's boxes, geometry and mask are padded to: `max(2, n0)`,
/// since the tail graph takes `N >= 2`. The padding is geometry `1.0` and mask `false`,
/// as the authors pad (`geo = torch.ones`, `mask = torch.zeros`).
pub fn charspot_tail_width(n0: usize) -> usize {
    n0.max(2)
}

/// The crops per dense call: [`CHARSPOT_MAX_BATCH`] when nothing caps it (`None`, the
/// package's own maximum), else the cap (a small GPU's) within `1..=64`.
pub fn charspot_batch_cap(max_batch: Option<u32>) -> usize {
    match max_batch {
        None => CHARSPOT_MAX_BATCH,
        Some(n) => usize::try_from(n)
            .unwrap_or(usize::MAX)
            .clamp(1, CHARSPOT_MAX_BATCH),
    }
}

// --- nar-small ---------------------------------------------------------------

/// nar-small's inputs for a crop: the patches `[324, 588]` (each a 14 x 14 patch, red
/// plane first, normalised), which of them are crop (`V`), their rows and columns, and
/// the grid's size `(rows, columns)`.
#[derive(Clone, Debug, PartialEq)]
pub struct NarInput {
    pub patches: Vec<f32>,
    pub valid: Vec<f32>,
    pub rows: Vec<i32>,
    pub cols: Vec<i32>,
    pub grid: (usize, usize),
}

/// nar-small's inputs for one non-empty crop.
pub fn nar_input(crop: &Rgb) -> NarInput {
    let (s, rows, cols) = fit_grid(crop.height, crop.width, NAR_BUDGET);
    let rw = round(crop.width as f64 * s).clamp(1, cols * NAR_PATCH);
    let rh = round(crop.height as f64 * s).clamp(1, rows * NAR_PATCH);
    let canvas = on_canvas(crop, (rw, rh), (cols * NAR_PATCH, rows * NAR_PATCH));
    let feat = 3 * NAR_PATCH * NAR_PATCH;
    let mut patches = vec![0.0f32; NAR_BUDGET * feat];
    let (mut valid, mut prow, mut pcol) = (
        vec![0.0f32; NAR_BUDGET],
        vec![0i32; NAR_BUDGET],
        vec![0i32; NAR_BUDGET],
    );
    for r in 0..rows {
        for c in 0..cols {
            let n = r * cols + c;
            valid[n] = 1.0;
            prow[n] = r as i32;
            pcol[n] = c as i32;
            let patch = &mut patches[n * feat..(n + 1) * feat];
            for ch in 0..3 {
                for py in 0..NAR_PATCH {
                    for px in 0..NAR_PATCH {
                        let v = canvas.data
                            [((r * NAR_PATCH + py) * canvas.width + c * NAR_PATCH + px) * 3 + ch];
                        patch[(ch * NAR_PATCH + py) * NAR_PATCH + px] =
                            (f32::from(v) / 255.0 - NAR_MEAN[ch]) / NAR_STD[ch];
                    }
                }
            }
        }
    }
    NarInput {
        patches,
        valid,
        rows: prow,
        cols: pcol,
        grid: (rows, cols),
    }
}

/// nar-small's inputs for a list of crops: the indices of the non-empty crops, in order,
/// and their inputs. Empty crops get none: they read `""` without a model call, and
/// keep their place through the indices.
pub fn nar_inputs(crops: &[&Rgb]) -> (Vec<usize>, Vec<NarInput>) {
    crops
        .iter()
        .enumerate()
        .filter(|(_, c)| !empty(c))
        .map(|(i, c)| (i, nar_input(c)))
        .unzip()
}

/// A batch of nar-small inputs, row-major: `patches` `[len, 324, 588]`, `valid`,
/// `rows`, `cols` `[len, 324]`, and each crop's grid as `heights` / `widths` `[len]`.
#[derive(Clone, Debug, PartialEq)]
pub struct NarBatch {
    pub len: usize,
    pub patches: Vec<f32>,
    pub valid: Vec<f32>,
    pub rows: Vec<i32>,
    pub cols: Vec<i32>,
    pub heights: Vec<i32>,
    pub widths: Vec<i32>,
}

/// The inputs back to back, in their order.
pub fn nar_batch(inputs: &[&NarInput]) -> NarBatch {
    let len = inputs.len();
    let feat = 3 * NAR_PATCH * NAR_PATCH;
    let mut b = NarBatch {
        len,
        patches: Vec::with_capacity(len * NAR_BUDGET * feat),
        valid: Vec::with_capacity(len * NAR_BUDGET),
        rows: Vec::with_capacity(len * NAR_BUDGET),
        cols: Vec::with_capacity(len * NAR_BUDGET),
        heights: Vec::with_capacity(len),
        widths: Vec::with_capacity(len),
    };
    for i in inputs {
        b.patches.extend_from_slice(&i.patches);
        b.valid.extend_from_slice(&i.valid);
        b.rows.extend_from_slice(&i.rows);
        b.cols.extend_from_slice(&i.cols);
        b.heights.push(i.grid.0 as i32);
        b.widths.push(i.grid.1 as i32);
    }
    b
}

/// The text of one crop's ids: the end mark is the id after the last character and the
/// text stops there; an id outside the vocabulary writes nothing.
pub fn nar_text(ids: &[i64], characters: &[String]) -> String {
    let end = characters.len() as i64;
    ids.iter()
        .take_while(|&&i| i != end)
        .filter_map(|&i| usize::try_from(i).ok().and_then(|i| characters.get(i)))
        .map(String::as_str)
        .collect()
}

/// `0..n` cut into consecutive ranges of at most `max` (a package's largest batch).
pub fn batches(n: usize, max: usize) -> Vec<Range<usize>> {
    let max = max.max(1);
    (0..n)
        .step_by(max)
        .map(|start| start..(start + max).min(n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nar_text_stops_at_the_end_id() {
        let chars: Vec<String> = ["a", "b"].map(String::from).to_vec();
        assert_eq!(nar_text(&[1, 7, 0, 2, 1], &chars), "ba");
        assert_eq!(nar_text(&[], &chars), "");
    }

    #[test]
    fn batches_split_at_the_package_max() {
        assert_eq!(batches(130, 64), vec![0..64, 64..128, 128..130]);
        assert_eq!(batches(64, 64), vec![0..64]);
        assert!(batches(0, 64).is_empty());
    }

    #[test]
    fn nar_batch_skips_empty_crops() {
        let blank = Rgb {
            width: 0,
            height: 0,
            data: vec![],
        };
        let ink = Rgb {
            width: 20,
            height: 40,
            data: vec![30; 20 * 40 * 3],
        };
        assert!(empty(&blank));
        assert!(!empty(&ink));
        let (kept, ins) = nar_inputs(&[&blank, &ink, &blank]);
        assert_eq!(kept, vec![1]);
        assert_eq!(ins.len(), 1);
        assert_eq!(ins[0], nar_input(&ink));
        // A box of zero width (or height) only is empty too.
        let line = Rgb {
            width: 0,
            height: 5,
            data: vec![],
        };
        assert_eq!(nar_inputs(&[&line, &ink]).0, vec![1]);
    }

    #[test]
    fn a_batch_is_its_inputs_back_to_back() {
        let a = nar_input(&Rgb {
            width: 20,
            height: 40,
            data: vec![200; 20 * 40 * 3],
        });
        let b = nar_input(&Rgb {
            width: 90,
            height: 30,
            data: vec![10; 90 * 30 * 3],
        });
        let nb = nar_batch(&[&a, &b]);
        assert_eq!(nb.len, 2);
        assert_eq!(nb.patches.len(), 2 * NAR_BUDGET * 3 * NAR_PATCH * NAR_PATCH);
        assert_eq!(&nb.patches[..a.patches.len()], &a.patches[..]);
        assert_eq!(&nb.valid[NAR_BUDGET..], &b.valid[..]);
        assert_eq!(nb.heights, vec![a.grid.0 as i32, b.grid.0 as i32]);
        assert_eq!(nb.widths, vec![a.grid.1 as i32, b.grid.1 as i32]);
    }

    #[test]
    fn the_grid_is_the_largest_that_fits_the_budget() {
        // A square crop fills the budget exactly: 16 x 16 cells, 18 x 18 patches.
        let (_, rows, cols) = fit_grid(100, 100, CHARSPOT_BUDGET);
        assert_eq!((rows, cols), (16, 16));
        let (_, rows, cols) = fit_grid(100, 100, NAR_BUDGET);
        assert_eq!((rows, cols), (18, 18));
        // A column four times as tall as wide.
        let (s, rows, cols) = fit_grid(400, 100, CHARSPOT_BUDGET);
        assert!(
            rows * cols <= CHARSPOT_BUDGET && rows > 3 * cols,
            "{rows}x{cols}"
        );
        // One step further would not fit.
        let cells = |v: usize, s: f64| (v as f64 * s / 14.0).ceil() as usize;
        assert!(cells(400, s * 1.01) * cells(100, s * 1.01) > CHARSPOT_BUDGET);
    }

    #[test]
    fn the_canvas_holds_the_crop_in_the_corner_on_its_border_median() {
        // 4 x 2, all border: red 10/20/30/40 on top, 50/60/70/80 below.
        let mut data = Vec::new();
        for v in [10u8, 20, 30, 40, 50, 60, 70, 80] {
            data.extend_from_slice(&[v, 0, 255]);
        }
        let crop = Rgb {
            width: 4,
            height: 2,
            data,
        };
        let canvas = on_canvas(&crop, (4, 2), (6, 3));
        assert_eq!((canvas.width, canvas.height), (6, 3));
        assert_eq!(canvas.px(0, 0), [10, 0, 255]);
        assert_eq!(canvas.px(3, 1), [80, 0, 255]);
        // Rows 10..40 and 50..80, columns 10,50 and 40,80: the two middle values of the
        // twelve are 40 and 50.
        assert_eq!(canvas.px(5, 0), [45, 0, 255]);
        assert_eq!(canvas.px(0, 2), [45, 0, 255]);
    }

    #[test]
    fn charspot_boxes_come_back_in_crop_pixels() {
        let crop = Rgb::new(300, 600);
        let (canvas, (fx, fy)) = charspot_canvas(&crop);
        assert_eq!(canvas.width % CHARSPOT_CELL, 0);
        assert_eq!(canvas.height % CHARSPOT_CELL, 0);
        assert!(canvas.width / CHARSPOT_CELL * (canvas.height / CHARSPOT_CELL) <= CHARSPOT_BUDGET);
        // The crop keeps its shape and lies inside the canvas: a canvas pixel is the
        // same number of crop pixels both ways, within the rounding of the scaled size.
        assert!((fx / fy - 1.0).abs() < 0.02, "{fx} {fy}");
        assert!(300.0 / fx <= canvas.width as f32 + 0.5);
        assert!(600.0 / fy <= canvas.height as f32 + 0.5);
    }

    #[test]
    fn nar_patches_are_laid_out_row_by_row_and_padded() {
        let crop = Rgb::new(140, 70);
        let input = nar_input(&crop);
        let (rows, cols) = input.grid;
        assert!(
            rows * cols <= NAR_BUDGET && cols == 2 * rows,
            "{rows}x{cols}"
        );
        let n = rows * cols;
        assert_eq!(input.valid[..n], vec![1.0; n][..]);
        assert!(input.valid[n..].iter().all(|v| *v == 0.0));
        assert_eq!((input.rows[cols], input.cols[cols]), (1, 0));
        assert_eq!(
            (input.rows[cols - 1], input.cols[cols - 1]),
            (0, cols as i32 - 1)
        );
        // A black pixel, normalised, red first.
        assert!((input.patches[0] - (0.0 - NAR_MEAN[0]) / NAR_STD[0]).abs() < 1e-6);
        assert!(
            (input.patches[2 * NAR_PATCH * NAR_PATCH] - (0.0 - NAR_MEAN[2]) / NAR_STD[2]).abs()
                < 1e-6
        );
        assert!(input.patches[n * 588..].iter().all(|v| *v == 0.0));
    }

    // --- charspot's decode ---------------------------------------------------

    fn maps<'a>(
        heat: &'a [f32],
        boxes: &'a [f32],
        ruby: &'a [f32],
        h: usize,
        w: usize,
    ) -> DenseMaps<'a> {
        DenseMaps {
            heat,
            boxes,
            ruby,
            height: h,
            width: w,
        }
    }
    // 2 x 3 cells; centres at +0.5 cell, sizes exp(0) * 8 = 8 px
    const H: [f32; 6] = [0.9, 0.1, 0.0, 0.0, 0.5, 0.31];
    fn bx() -> Vec<f32> {
        [vec![0.5; 6], vec![0.5; 6], vec![0.0; 6], vec![0.0; 6]].concat()
    }

    #[test]
    fn three_centres_three_characters() {
        let b = bx();
        let d = charspot_detect(&maps(&H, &b, &[0.0; 6], 2, 3), 2, 3).unwrap();
        assert_eq!(
            d.boxes,
            vec![
                [0.0, 0.0, 8.0, 8.0],
                [8.0, 8.0, 16.0, 16.0],
                [16.0, 8.0, 24.0, 16.0]
            ]
        );
        assert_eq!(d.scores, vec![0.9, 0.5, 0.31]);
        assert_eq!(
            d.geo,
            vec![
                [0.5, 0.5, 1.0, 1.0],
                [1.5, 1.5, 1.0, 1.0],
                [2.5, 1.5, 1.0, 1.0]
            ]
        );
    }

    #[test]
    fn furigana_is_dropped() {
        let b = bx();
        let mut ruby = [0.0f32; 6];
        ruby[4] = 0.6;
        let d = charspot_detect(&maps(&H, &b, &ruby, 2, 3), 2, 3).unwrap();
        assert_eq!(d.boxes, vec![[0.0, 0.0, 8.0, 8.0], [16.0, 8.0, 24.0, 16.0]]);
    }

    #[test]
    fn nothing_above_the_threshold_is_no_characters() {
        let b = bx();
        assert!(charspot_detect(&maps(&[0.3; 6], &b, &[0.0; 6], 2, 3), 2, 3).is_none());
        assert!(charspot_detect(&maps(&H, &b, &[0.5; 6], 2, 3), 2, 3).is_none());
    }

    #[test]
    fn overlap_and_centre_rules() {
        assert_eq!(
            charspot_nms(
                &[
                    [0.0, 0.0, 10.0, 10.0],
                    [1.0, 1.0, 11.0, 11.0],
                    [30.0, 0.0, 40.0, 10.0]
                ],
                &[0.9, 0.8, 0.7]
            ),
            vec![0, 2]
        );
        assert_eq!(
            charspot_nms(
                &[[12.0, 12.0, 14.0, 14.0], [0.0, 0.0, 20.0, 20.0]],
                &[0.8, 0.9]
            ),
            vec![1]
        );
    }

    #[test]
    fn equal_scores_keep_the_first() {
        assert_eq!(
            charspot_nms(
                &[[0.0, 0.0, 10.0, 10.0], [1.0, 1.0, 11.0, 11.0]],
                &[0.5, 0.5]
            ),
            vec![0]
        );
    }

    #[test]
    fn the_chain_follows_the_best_successor() {
        let pair = [0.0, 0.0, 0.5, 0.7, 0.0, 0.3, 0.1, 0.2, 0.0];
        assert_eq!(
            charspot_chain(&pair, &[0.1, 0.9, 0.2], &[0.0, 0.0, 0.0]),
            vec![1, 0, 2]
        );
        assert_eq!(
            charspot_chain(&pair, &[0.1, 0.9, 0.2], &[0.0, 0.8, 0.0]),
            vec![1, 2, 0]
        );
    }

    #[test]
    fn an_end_tie_continues_the_chain() {
        let pair = [0.0, 0.0, 0.5, 0.7, 0.0, 0.3, 0.1, 0.2, 0.0];
        assert_eq!(
            charspot_chain(&pair, &[0.1, 0.9, 0.2], &[0.0, 0.7, 0.0]),
            vec![1, 0, 2]
        );
    }

    #[test]
    fn the_class_table() {
        let t = charspot_classes(vec!["a".into(), "b".into()]).unwrap();
        assert_eq!(t.len(), 6830);
        assert_eq!(
            (
                &t[0][..],
                &t[2][..],
                &t[6818][..],
                &t[6821][..],
                &t[6822][..],
                &t[6829][..]
            ),
            ("a", "", "!!", "??", "!!!", "???")
        );
        assert!(charspot_classes(vec![String::new(); 6819]).is_err());
    }

    // --- charspot's canvas batches and tail parts --------------------------------

    #[test]
    fn canvases_follow_the_ladder() {
        assert_eq!(charspot_canvas_size(32, 48), (32, 48));
        assert_eq!(charspot_canvas_size(80, 16), (96, 16));
    }

    #[test]
    fn a_cheap_merge_is_made() {
        // apart 1024 + 6 and 1024 + 9, merged 1024 + 18: a gain of 1021
        assert_eq!(
            canvas_batches(&[(32, 48), (48, 48)], 64),
            vec![(vec![0, 1], (48, 48))]
        );
    }

    #[test]
    fn a_merge_that_costs_more_is_not_made() {
        // apart 1024 + 256 and 1024 + 256; merged onto 4096 x 4096: 1024 + 2 * 65536
        assert_eq!(
            canvas_batches(&[(16, 4096), (4096, 16)], 64),
            vec![(vec![0], (16, 4096)), (vec![1], (4096, 16))]
        );
    }

    #[test]
    fn batches_stop_at_64() {
        let b = canvas_batches(&vec![(32, 32); 70], 64);
        assert_eq!(b.iter().map(|(i, _)| i.len()).collect::<Vec<_>>(), [64, 6]);
        assert_eq!(b[1].0[0], 64);
    }

    #[test]
    fn tail_parts_skip_empty_rows() {
        assert_eq!(
            tail_parts(&[3, 10, 0, 5], 1 << 18, 512),
            vec![vec![1, 3, 0]]
        );
        assert_eq!(
            tail_parts(&[3, 10, 0, 5], 250, 512),
            vec![vec![1, 3], vec![0]]
        );
        assert!(tail_parts(&[0, 0], 1 << 18, 512).is_empty());
    }

    #[test]
    fn a_long_row_still_gets_a_part() {
        // 600² = 360,000 > 2^18: the first row is taken anyway, alone
        assert_eq!(tail_parts(&[3, 600], 1 << 18, 512), vec![vec![1], vec![0]]);
        assert_eq!(tail_parts(&[513], 1 << 18, 512), vec![vec![0]]);
        // 362² * 2 = 262,088 <= 2^18 still pairs; 363² * 2 = 263,538 does not
        assert_eq!(tail_parts(&[362, 362], 1 << 18, 512), vec![vec![0, 1]]);
        assert_eq!(
            tail_parts(&[363, 363], 1 << 18, 512),
            vec![vec![0], vec![1]]
        );
        // the row cap alone: 513 one-character rows make 512 + 1
        let p = tail_parts(&vec![1; 513], 1 << 18, 512);
        assert_eq!(p.iter().map(Vec::len).collect::<Vec<_>>(), [512, 1]);
    }

    #[test]
    fn a_tail_part_is_at_least_two_wide() {
        assert_eq!(charspot_tail_width(1), 2);
        assert_eq!(charspot_tail_width(2), 2);
        assert_eq!(charspot_tail_width(600), 600);
    }

    #[test]
    fn a_small_gpu_caps_the_canvas_batch() {
        assert_eq!(charspot_batch_cap(None), 64);
        assert_eq!(charspot_batch_cap(Some(8)), 8);
        assert_eq!(charspot_batch_cap(Some(128)), 64);
        assert_eq!(charspot_batch_cap(Some(0)), 1);
    }
}
