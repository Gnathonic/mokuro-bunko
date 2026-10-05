//! PP-OCRv6 manga: DBNet line detection and CTC recognition on ONNX Runtime, plus
//! the page-level policies of `ppocr.py` (spec §3–§5).
//!
//! [`PpOcr`] owns the two shared sessions and is `Sync`: any number of threads may
//! read pages with it at once. [`PpocrPageReader`] adds the page-level passes that
//! need the line layout (joins, end probes, votes) through [`LayoutHooks`], which
//! the `bunko-layout` crate implements.

mod contours;
mod ctc;
#[cfg(test)]
mod cv_golden;
mod detect;
pub mod difflib;
mod fillpoly;
pub mod geometry;
mod marks;
mod minrect;
mod reader;

use std::path::Path;
use std::sync::Arc;

pub use ctc::{IDEOGRAPHIC_SPACE, MISSING_GLYPH, REC_HEIGHT, recognizer_width};
pub use detect::{DEFAULT_SIDE, detector_input_size};
pub use geometry::Quad;
pub use reader::{LayoutHooks, NoLayout, PpocrPageReader, ReadLines, page_to_json};

use crate::error::{Error, Result};
use crate::image::{
    BgrImage, ImageView, get_perspective_transform, rotate90_ccw, warp_perspective_cubic,
};
use crate::lines::{DetectorInfo, DetectorPass};
use crate::py;
use crate::runtime::{Model, RuntimeOptions};
use ctc::{RecTensor, Step};
use detect::ProbMap;
use geometry::{
    order_quad, quad_angle, quad_is_vertical, quad_size, quad_thickness, slice_quad, union_quad,
    widen_quad,
};

/// A PP-OCR line in page pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub quad: Quad,
    pub score: f64,
    pub text: String,
    pub conf: f64,
    pub char_confs: Vec<f64>,
}

impl Line {
    pub fn new(quad: Quad, score: f64) -> Self {
        Self {
            quad,
            score,
            text: String::new(),
            conf: 0.0,
            char_confs: Vec::new(),
        }
    }

    pub fn vertical(&self) -> bool {
        quad_is_vertical(&self.quad)
    }

    pub fn angle(&self) -> f64 {
        quad_angle(&self.quad)
    }
}

/// `tile` policy of the detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileMode {
    /// One pass, then a finer one for dense pages; strips tiled from the start.
    Auto,
    /// One pass at `side`.
    Off,
    /// Always tile at native scale.
    Force,
}

impl TileMode {
    fn as_str(&self) -> &'static str {
        match self {
            TileMode::Auto => "auto",
            TileMode::Off => "off",
            TileMode::Force => "force",
        }
    }
}

/// `(text, confidence, per-character confidences)` of one crop.
pub type Read = (String, f64, Vec<f64>);

/// CTC classes: index 0 is the blank, the last one a space (`load_vocab`).
pub fn load_vocab(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    let mut symbols: Vec<String> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
        .collect();
    if symbols.last().is_some_and(|s| s.is_empty()) {
        symbols.pop();
    }
    let mut vocab = Vec::with_capacity(symbols.len() + 2);
    vocab.push(String::new());
    vocab.extend(symbols);
    vocab.push(" ".to_string());
    Ok(vocab)
}

/// Deskewed crop of a line, turned so the text reads left to right (`crop_line`).
pub fn crop_line(bgr: ImageView<'_>, quad: &Quad) -> BgrImage {
    let (width, height) = quad_size(quad);
    let w = (py::round_int(width).max(2)) as usize;
    let h = (py::round_int(height).max(2)) as usize;
    let target = [
        [0.0, 0.0],
        [w as f32, 0.0],
        [w as f32, h as f32],
        [0.0, h as f32],
    ];
    let crop = match get_perspective_transform(quad, &target) {
        Some(m) => warp_perspective_cubic(bgr, &m, w, h),
        None => BgrImage::zeros(w, h),
    };
    if h > w { rotate90_ccw(&crop) } else { crop }
}

/// `[3, 48, W]` recognizer input: `(x / 255 - 0.5) / 0.5` on BGR.
fn recognizer_tensor(crop: &BgrImage) -> RecTensor {
    let width = recognizer_width(crop.width(), crop.height());
    let resized = crate::image::resize_linear(crop.view(), width, REC_HEIGHT);
    let plane = width * REC_HEIGHT;
    let mut data = vec![0.0f32; 3 * plane];
    for (i, px) in resized.as_raw().as_chunks::<3>().0.iter().enumerate() {
        for c in 0..3 {
            data[c * plane + i] = (px[c] as f32 / 255.0 - 0.5) / 0.5;
        }
    }
    RecTensor { width, data }
}

/// Map `f` over `items` on up to `threads` scoped threads, keeping order.
fn par_map<T: Sync, U: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> U + Sync) -> Vec<U> {
    let threads = threads.max(1).min(items.len().max(1));
    if threads <= 1 || items.len() < 4 {
        return items.iter().map(f).collect();
    }
    let chunk = items.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|c| s.spawn(|| c.iter().map(&f).collect::<Vec<U>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    })
}

/// The detector + recognizer pair and the page policies.
#[derive(Clone)]
pub struct PpOcr {
    det: Arc<Model>,
    rec: Arc<Model>,
    vocab: Arc<Vec<String>>,
    side: u32,
    tile: TileMode,
    /// Threads for crop/tensor preparation (CPU work outside ORT).
    prep_threads: usize,
}

impl std::fmt::Debug for PpOcr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PpOcr")
            .field("det", &self.det)
            .field("rec", &self.rec)
            .field("side", &self.side)
            .finish()
    }
}

impl PpOcr {
    /// Load the models (one session pair per call; share the result with `clone`).
    pub fn load(
        detector: &Path,
        recognizer: &Path,
        dictionary: &Path,
        opts: &RuntimeOptions,
    ) -> Result<Self> {
        let det = Arc::new(Model::load(detector, opts)?);
        let rec = Arc::new(Model::load(recognizer, opts)?);
        let vocab = Arc::new(load_vocab(dictionary)?);
        Ok(Self::from_models(det, rec, vocab))
    }

    /// From already loaded models (e.g. shared with another engine).
    pub fn from_models(det: Arc<Model>, rec: Arc<Model>, vocab: Arc<Vec<String>>) -> Self {
        let prep_threads = std::thread::available_parallelism()
            .map(|n| n.get().min(8))
            .unwrap_or(4);
        Self {
            det,
            rec,
            vocab,
            side: DEFAULT_SIDE,
            tile: TileMode::Auto,
            prep_threads,
        }
    }

    /// Detector side limit and tile policy (the runner always uses 1120 / auto).
    pub fn with_policy(mut self, side: u32, tile: TileMode) -> Self {
        self.side = side;
        self.tile = tile;
        self
    }

    /// Threads used for crops and tensors outside ONNX Runtime.
    pub fn with_prep_threads(mut self, threads: usize) -> Self {
        self.prep_threads = threads.max(1);
        self
    }

    pub fn detector(&self) -> &Model {
        &self.det
    }

    pub fn recognizer(&self) -> &Model {
        &self.rec
    }

    pub fn vocab(&self) -> &[String] {
        &self.vocab
    }

    // -- detection --------------------------------------------------------

    /// One network run: `[(page quad, score, thickness in detector px)]`.
    fn detect_region(
        &self,
        bgr: ImageView<'_>,
        size: (usize, usize),
        origin: (usize, usize),
    ) -> Result<Vec<(Quad, f64, f64)>> {
        let (w, h) = (bgr.width(), bgr.height());
        let tensor = detect::detector_tensor(bgr, size);
        let (shape, prob) = self.det.run_f32(&[1, 3, size.1, size.0], tensor)?;
        let (ph, pw) = match shape.as_slice() {
            [_, _, ph, pw] => (*ph, *pw),
            _ => {
                return Err(Error::ModelOutput(format!(
                    "detector output shape {shape:?}"
                )));
            }
        };
        let map = ProbMap {
            width: pw,
            height: ph,
            data: &prob[..pw * ph],
        };
        let (sx, sy) = (
            (w as f64 / size.0 as f64) as f32,
            (h as f64 / size.1 as f64) as f32,
        );
        let (ox, oy) = (origin.0 as f32, origin.1 as f32);
        Ok(detect::db_postprocess(&map)
            .into_iter()
            .map(|(q, score)| {
                let thickness = quad_thickness(&q);
                let mut page = q;
                for p in &mut page {
                    p[0] = p[0] * sx + ox;
                    p[1] = p[1] * sy + oy;
                }
                (page, score, thickness)
            })
            .collect())
    }

    /// Detect at `scale` (page → detector) over overlapping tiles.
    fn detect_tiled(&self, bgr: &BgrImage, scale: f64) -> Result<(Vec<(Quad, f64)>, usize)> {
        let (w, h) = (bgr.width(), bgr.height());
        let tile_px = ((detect::TILE_SIZE / scale) as usize).max(detect::SIDE_MULTIPLE * 8);
        let overlap_px = (detect::TILE_OVERLAP / scale) as usize;
        let grid = detect::tile_grid(w, h, tile_px, overlap_px);
        let (mut quads, mut scores, mut owners) = (Vec::new(), Vec::new(), Vec::new());
        for (idx, &(x0, y0, x1, y1)) in grid.iter().enumerate() {
            let region = bgr.region(x0, y0, x1, y1);
            let size = detect::scaled_size(x1 - x0, y1 - y0, scale);
            for (q, s, _) in self.detect_region(region, size, (x0, y0))? {
                if detect::clipped_by_tile(
                    &q,
                    (x0, y0, x1, y1),
                    (w, h),
                    overlap_px as f64,
                    detect::TILE_EDGE_MARGIN / scale,
                ) {
                    continue;
                }
                quads.push(q);
                scores.push(s);
                owners.push(idx);
            }
        }
        Ok((
            detect::merge_tile_lines(&quads, &scores, &owners),
            grid.len(),
        ))
    }

    /// Rotated line quads of a page with scores, and what was run.
    pub fn detect(&self, bgr: &BgrImage) -> Result<(Vec<Line>, DetectorInfo)> {
        let (w, h) = (bgr.width(), bgr.height());
        let mut info = DetectorInfo {
            side: self.side,
            tile: self.tile.as_str().into(),
            ..Default::default()
        };
        let pass = |scale: f64, tiles: usize, lines: usize, why: &str| DetectorPass {
            scale: py::round_to(scale, 4),
            tiles,
            lines,
            why: why.to_string(),
        };
        let strip = w.max(h) as f64 / w.min(h).max(1) as f64 >= detect::STRIP_ASPECT;
        if self.tile == TileMode::Force || (self.tile == TileMode::Auto && strip) {
            let scale = if self.tile == TileMode::Force {
                1.0
            } else {
                (self.side as f64 / (w.min(h) as f64 * 1.5)).min(1.0)
            };
            let (found, count) = self.detect_tiled(bgr, scale)?;
            info.passes.push(pass(scale, count, found.len(), "strip"));
            return Ok((sort_lines(found), info));
        }
        let size = detect::detector_input_size(w, h, self.side);
        let first = self.detect_region(bgr.view(), size, (0, 0))?;
        let first_scale = size.0 as f64 / w as f64;
        info.passes.push(pass(first_scale, 1, first.len(), "first"));
        let mut result: Vec<(Quad, f64)> = first.iter().map(|(q, s, _)| (*q, *s)).collect();
        if self.tile == TileMode::Auto {
            let thick: Vec<f64> = first.iter().map(|f| f.2).collect();
            let longs: Vec<f64> = first
                .iter()
                .map(|(q, _, _)| geometry::quad_length(q) * first_scale)
                .collect();
            if let Some(why) = detect::needs_fine_pass(&thick, &longs) {
                let scale = detect::fine_scale(
                    detect::dense_median(&thick, &longs).unwrap_or(0.0),
                    first_scale,
                );
                if scale > first_scale * 1.15 {
                    let fine = detect::scaled_size(w, h, scale);
                    let count;
                    if fine.0 * fine.1 <= detect::MAX_DETECTOR_PIXELS {
                        result = self
                            .detect_region(bgr.view(), fine, (0, 0))?
                            .into_iter()
                            .map(|(q, s, _)| (q, s))
                            .collect();
                        count = 1;
                    } else {
                        (result, count) = self.detect_tiled(bgr, scale)?;
                    }
                    info.passes.push(pass(scale, count, result.len(), &why));
                }
            }
        }
        Ok((sort_lines(result), info))
    }

    // -- recognition ------------------------------------------------------

    /// Read crops that already read left to right; all windows of all crops are
    /// batched together by width.
    pub fn recognize_crops(&self, crops: &[BgrImage]) -> Result<Vec<Read>> {
        let tensors: Vec<RecTensor> = par_map(crops, self.prep_threads, recognizer_tensor);
        self.recognize_tensors(&tensors)
    }

    /// Crop `quads` from `bgr` and read them (crops prepared in parallel).
    pub fn recognize_quads(&self, bgr: &BgrImage, quads: &[Quad]) -> Result<Vec<Read>> {
        let view = bgr.view();
        let tensors: Vec<RecTensor> = par_map(quads, self.prep_threads, |q| {
            recognizer_tensor(&crop_line(view, q))
        });
        self.recognize_tensors(&tensors)
    }

    fn recognize_tensors(&self, tensors: &[RecTensor]) -> Result<Vec<Read>> {
        let stride = ctc::REC_STRIDE;
        let mut pieces: Vec<(usize, (usize, usize))> = Vec::new();
        for (i, t) in tensors.iter().enumerate() {
            for span in ctc::window_spans(t.width / stride) {
                pieces.push((i, span));
            }
        }
        let widths: Vec<usize> = pieces.iter().map(|(_, (s, e))| (e - s) * stride).collect();
        let mut probs: Vec<Vec<Step>> = vec![Vec::new(); pieces.len()];
        let classes = self.vocab.len();
        for batch in ctc::plan_batches(&widths) {
            let bw = batch.iter().map(|&k| widths[k]).max().unwrap_or(0);
            let n = batch.len();
            let mut x = vec![0.0f32; n * 3 * REC_HEIGHT * bw];
            for (row, &k) in batch.iter().enumerate() {
                let (i, (s, e)) = pieces[k];
                let t = &tensors[i];
                for c in 0..3 {
                    for y in 0..REC_HEIGHT {
                        let src = (c * REC_HEIGHT + y) * t.width;
                        let dst = ((row * 3 + c) * REC_HEIGHT + y) * bw;
                        x[dst..dst + (e - s) * stride]
                            .copy_from_slice(&t.data[src + s * stride..src + e * stride]);
                    }
                }
            }
            let (shape, out) = self.rec.run_f32(&[n, 3, REC_HEIGHT, bw], x)?;
            let (tt, cc) = match shape.as_slice() {
                [_, tt, cc] => (*tt, *cc),
                _ => {
                    return Err(Error::ModelOutput(format!(
                        "recognizer output shape {shape:?}"
                    )));
                }
            };
            if cc != classes {
                return Err(Error::ModelOutput(format!(
                    "recognizer has {cc} classes, dictionary {classes}"
                )));
            }
            for (row, &k) in batch.iter().enumerate() {
                let steps = (widths[k] / stride).min(tt);
                let base = row * tt * cc;
                probs[k] = ctc::reduce_rows(&out[base..base + steps * cc], cc);
            }
        }
        let mut results = Vec::with_capacity(tensors.len());
        let mut k = 0;
        for (i, t) in tensors.iter().enumerate() {
            let start = k;
            while k < pieces.len() && pieces[k].0 == i {
                k += 1;
            }
            let full = if k - start == 1 {
                std::mem::take(&mut probs[start])
            } else {
                let spans: Vec<(usize, usize)> = pieces[start..k].iter().map(|p| p.1).collect();
                ctc::stitch_windows(&probs[start..k], &spans)
            };
            let decoded = ctc::ctc_greedy(&full, &self.vocab);
            let conf = if decoded.is_empty() {
                0.0
            } else {
                py::np_mean(&decoded.iter().map(|c| c.conf).collect::<Vec<_>>())
            };
            let chars = ctc::fill_gaps(ctc::doubt_foreign_glyphs(decoded), t);
            let text: String = chars.iter().map(|c| c.ch.as_str()).collect();
            results.push((text, conf, chars.iter().map(|c| c.conf).collect()));
        }
        Ok(results)
    }

    /// Detect, then recognize every line (batched across the page).
    pub fn read_page(&self, bgr: &BgrImage) -> Result<(Vec<Line>, DetectorInfo)> {
        let (mut lines, info) = self.detect(bgr)?;
        let quads: Vec<Quad> = lines.iter().map(|l| l.quad).collect();
        for (line, (text, conf, confs)) in lines.iter_mut().zip(self.recognize_quads(bgr, &quads)?)
        {
            line.text = text;
            line.conf = conf;
            line.char_confs = confs;
        }
        Ok((lines, info))
    }

    // -- page-level passes ------------------------------------------------

    /// Re-read groups of lines that are pieces of one printed line; a group is
    /// replaced by its joined read when that read is confident and loses no glyph.
    pub fn join_lines(
        &self,
        bgr: &BgrImage,
        lines: &[Line],
        groups: &[Vec<usize>],
    ) -> Result<Vec<Line>> {
        if groups.is_empty() {
            return Ok(lines.to_vec());
        }
        let quads: Vec<Quad> = groups
            .iter()
            .map(|g| union_quad(&g.iter().map(|&i| lines[i].quad).collect::<Vec<_>>()))
            .collect();
        let reads = self.recognize_quads(bgr, &quads)?;
        let mut replaced: std::collections::HashMap<usize, Option<Line>> =
            std::collections::HashMap::new();
        for ((group, quad), (text, conf, confs)) in groups.iter().zip(&quads).zip(reads) {
            let mut wanted: i64 = group
                .iter()
                .map(|&i| py::len(py::strip(&lines[i].text)).max(1) as i64)
                .sum();
            let members: Vec<&Line> = group.iter().map(|&i| &lines[i]).collect();
            wanted -= shared_seam_glyphs(&members, quad) as i64;
            if conf < reader::JOIN_MIN_CONF || (py::len(py::strip(&text)) as i64) < wanted {
                continue;
            }
            let score = py::np_mean(&group.iter().map(|&i| lines[i].score).collect::<Vec<_>>());
            let Some(&first) = group.iter().min() else {
                continue;
            };
            replaced.insert(
                first,
                Some(Line {
                    quad: *quad,
                    score,
                    text,
                    conf,
                    char_confs: confs,
                }),
            );
            for &i in group {
                if i != first {
                    replaced.insert(i, None);
                }
            }
        }
        Ok(lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| match replaced.remove(&i) {
                Some(new) => new,
                None => Some(l.clone()),
            })
            .collect())
    }

    /// Give lines back the bracket or stop their box left out (end probes).
    /// `targets[k]` indexes `lines`; `thin[k]` also allows the thin openers.
    pub fn recover_clipped_ends(
        &self,
        bgr: &BgrImage,
        lines: &mut [Line],
        targets: &[usize],
        thin: &[bool],
    ) -> Result<usize> {
        let owners: Vec<(usize, bool)> = targets
            .iter()
            .zip(thin)
            .filter(|(i, _)| py::len(py::strip(&lines[**i].text)) >= reader::PROBE_MIN_GLYPHS)
            .map(|(i, t)| (*i, *t))
            .collect();
        let probes: Vec<Vec<Quad>> = owners
            .iter()
            .map(|(i, _)| reader::probe_quads(&lines[*i].quad))
            .collect();
        let flat: Vec<Quad> = probes.iter().flatten().copied().collect();
        let mut reads = self.recognize_quads(bgr, &flat)?.into_iter();
        let mut recovered = 0;
        for ((i, thin_ok), quads) in owners.iter().zip(&probes) {
            let Some((head, head_conf, _)) = reads.next() else {
                break;
            };
            let line = &mut lines[*i];
            let (opener, closer, tail_conf);
            if quads.len() == 1 {
                tail_conf = head_conf;
                let (mut o, c) = marks::clipped_marks(&line.text, py::strip(&head));
                if o.is_empty() && c.is_empty() && *thin_ok {
                    o = marks::clipped_opener(&line.text, py::strip(&head), true);
                }
                opener = o;
                closer = c;
            } else {
                let Some((tail, tc, _)) = reads.next() else {
                    break;
                };
                tail_conf = tc;
                opener = marks::clipped_opener(&line.text, py::strip(&head), *thin_ok);
                closer = marks::clipped_closer(&line.text, py::strip(&tail));
            }
            if opener.is_empty() && closer.is_empty() {
                continue;
            }
            let length = geometry::quad_length(&line.quad);
            let pitch = length / py::len(&line.text).max(1) as f64;
            let is_thin = !opener.is_empty() && marks::PROBE_THIN_OPENERS.contains(opener.as_str());
            let head_grow = if is_thin {
                reader::PROBE_GROW_PITCH_THIN
            } else {
                reader::PROBE_GROW_PITCH
            };
            let start = if opener.is_empty() {
                0.0
            } else {
                -head_grow * pitch
            };
            let end = length
                + if closer.is_empty() {
                    0.0
                } else {
                    reader::PROBE_GROW_PITCH * pitch
                };
            line.quad = order_quad(&slice_quad(&line.quad, start, end));
            line.text = format!("{opener}{}{closer}", line.text);
            let mut confs = Vec::with_capacity(line.char_confs.len() + 2);
            if !opener.is_empty() {
                confs.push(head_conf);
            }
            confs.extend_from_slice(&line.char_confs);
            if !closer.is_empty() {
                confs.push(tail_conf);
            }
            line.char_confs = confs;
            recovered += py::len(&opener) + py::len(&closer);
        }
        Ok(recovered)
    }

    /// Put the doubted characters of `lines[targets]` to a vote against two wider
    /// crops. Returns how many characters changed.
    pub fn second_opinions(
        &self,
        bgr: &BgrImage,
        lines: &mut [Line],
        targets: &[usize],
    ) -> Result<usize> {
        let doubted: Vec<usize> = targets
            .iter()
            .copied()
            .filter(|&i| {
                let l = &lines[i];
                let n = py::len(&l.text);
                n >= reader::VOTE_MIN_GLYPHS
                    && l.char_confs.len() == n
                    && l.char_confs.iter().copied().fold(f64::INFINITY, f64::min)
                        < marks::VOTE_MAX_CONF
            })
            .collect();
        if doubted.is_empty() {
            return Ok(0);
        }
        let quads: Vec<Quad> = doubted
            .iter()
            .flat_map(|&i| reader::VOTE_WIDEN.iter().map(move |&s| (i, s)))
            .map(|(i, s)| widen_quad(&lines[i].quad, s))
            .collect();
        let reads = self.recognize_quads(bgr, &quads)?;
        let per = reader::VOTE_WIDEN.len();
        let mut changed = 0;
        for (k, &i) in doubted.iter().enumerate() {
            let others: Vec<(String, Vec<f64>)> = reads[k * per..(k + 1) * per]
                .iter()
                .map(|(t, _, c)| (t.clone(), c.clone()))
                .collect();
            let line = &mut lines[i];
            let (text, confs, n) = marks::vote_characters(&line.text, &line.char_confs, &others);
            line.text = text;
            line.char_confs = confs;
            changed += n;
        }
        Ok(changed)
    }
}

/// Glyphs two overlapping pieces of one line both read at their seam.
fn shared_seam_glyphs(pieces: &[&Line], union: &Quad) -> usize {
    let frame = geometry::frame(union);
    let mut spans: Vec<(f64, f64, Vec<char>)> = pieces
        .iter()
        .map(|p| {
            let (lo, hi) = geometry::project(&p.quad, &frame);
            (lo, hi, py::strip(&p.text).chars().collect())
        })
        .collect();
    spans.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    spans
        .windows(2)
        .filter(|w| {
            let (_, end, before) = &w[0];
            let (start, _, after) = &w[1];
            start < end && !before.is_empty() && !after.is_empty() && before.last() == after.first()
        })
        .count()
}

/// Stable order by `(-centre.x, centre.y)` (not reading order).
fn sort_lines(found: Vec<(Quad, f64)>) -> Vec<Line> {
    let mut lines: Vec<(f64, f64, Line)> = found
        .into_iter()
        .map(|(q, s)| {
            let c = geometry::centre(&q);
            (-(c[0] as f64), c[1] as f64, Line::new(q, s))
        })
        .collect();
    lines.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    });
    lines.into_iter().map(|(_, _, l)| l).collect()
}
