//! The text-box recognizers on ONNX Runtime (feature `nc-models`):
//! **manga-ocr-charspot-small** and **manga-ocr-nar-small** (muscgab). Their weights are
//! CC BY-NC-SA 4.0; the inference code they are published with is Apache-2.0, and what
//! is here follows that code's pre- and post-processing.
//!
//! Both were trained on whole text blocks and read them better than single lines (on a
//! 1,000-box sample of JMangaBench_Read, read as blocks charspot got 93.5% of the boxes
//! exactly right, fed the lines a line detector found in the same boxes 86.0%). So they
//! are [`BlockReader`]s, and the pipeline hands them blocks:
//!
//! * with a line detector, each block of the first layout, once; the text is then
//!   shared out to the block's lines, by where charspot's characters are and for
//!   nar-small by aligning its text with the line reader's ([`share_characters`],
//!   [`share_text`]);
//! * with a block detector, the detector's boxes as they are; the lines are the ones
//!   the recognizer finds in each ([`lines_of`]; nar-small knows no positions, so its
//!   block is one line).
//!
//! Both read one upright crop of text and take no prompt and no decoder loop:
//!
//! * charspot finds every character in the crop, classifies it and puts the characters
//!   in reading order, all inside its graph: a call returns the characters with their
//!   boxes ([`CharRead`]). Furigana is left out by the model.
//! * nar-small writes the whole text in one pass, at most [`NAR_SLOTS`] - 1 characters
//!   (the model of the repo's `r16` branch; the first release had 65 slots). Its graph
//!   holds no weights: they come from the author's `model.safetensors`, handed to ONNX
//!   Runtime from memory ([`bunko_ocr::runtime::Model::load_with`]).
//!
//! On the whole of JMangaBench_Read v1.3.3 (7,789 boxes; Composite = 1 - NED averaged
//! over printed and non-printed boxes, then the share of boxes exactly right): the
//! benchmark's own crops read as they are, charspot 97.17 / 92.37% and nar-small r16
//! 98.12 / 93.86%; through this pipeline, each crop on a page with a 32 px border,
//! 93.72 / 85.95% and 95.84 / 90.58% with the ppocr-manga detector's blocks, and
//! 95.08 / 87.98% and 96.73 / 92.05% with animetext-blocks (MODELS.md §8).
//!
//! The crop is scaled to fit a grid (aspect kept, Pillow's bicubic), put in the grid's
//! top-left corner and the rest filled with the median colour of the scaled crop's
//! border.

use std::path::Path;
#[cfg(feature = "torch")]
use std::sync::Arc;

use bunko_layout::difflib::SequenceMatcher;
use bunko_ocr::models::NAR_WEIGHTS_LOCATION;
use bunko_ocr::runtime::{ExternalFile, Model, RuntimeOptions, TensorBuf};
#[cfg(feature = "torch")]
use bunko_torch::abi::{CharEntry, CharsRead};
use bunko_vlm::crop::paddle_quad_crop;
use bunko_vlm::textbox::*;
use bunko_vlm::{
    Bgr, BlockCrop, BlockReader, CropSet, Device, LineRead, Precision, Quad, Recognizer,
    RecognizerInfo, Rgb, VlmError, read_flat,
};

/// How far a line's crop reaches past its quad, in line thicknesses: the models were
/// trained on tight text boxes.
pub const LINE_MARGIN_EM: f64 = 0.1;

// --- charspot ----------------------------------------------------------------

/// One character charspot found.
#[derive(Debug, Clone, PartialEq)]
pub struct CharBox {
    /// The character; a combined `!?` glyph is two or three.
    pub text: String,
    /// `[x0, y0, x1, y1]` in pixels of the crop that was read.
    pub bbox: [f32; 4],
    /// The character-centre score.
    pub score: f32,
}

/// What charspot read in a crop: its characters in reading order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CharRead {
    pub chars: Vec<CharBox>,
}

impl CharRead {
    pub fn text(&self) -> String {
        self.chars.iter().map(|c| c.text.as_str()).collect()
    }
}

/// manga-ocr-charspot-small.
pub struct Charspot {
    model: Model,
    /// Class id → what it writes.
    classes: Vec<String>,
}

impl std::fmt::Debug for Charspot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Charspot")
            .field("model", &self.model)
            .finish()
    }
}

impl Charspot {
    pub fn load(model: &Path, vocabulary: &Path, opts: &RuntimeOptions) -> Result<Self, VlmError> {
        let mut classes = load_characters(vocabulary)?;
        if classes.len() > CHARSPOT_CLASSES {
            return Err(VlmError::Asset(format!(
                "vocabulary {}: {} characters, more than {CHARSPOT_CLASSES}",
                vocabulary.display(),
                classes.len()
            )));
        }
        classes.resize(CHARSPOT_CLASSES, String::new());
        // The tate-chu-yoko glyphs: every pair, then every triple, of `!` and `?`.
        let marks = ['!', '?'];
        for a in marks {
            for b in marks {
                classes.push([a, b].iter().collect());
            }
        }
        for a in marks {
            for b in marks {
                for c in marks {
                    classes.push([a, b, c].iter().collect());
                }
            }
        }
        let model = Model::load(model, opts).map_err(|e| VlmError::Runtime(e.to_string()))?;
        Ok(Self { model, classes })
    }

    /// The characters of one upright crop, in reading order.
    pub fn read(&self, crop: &Rgb) -> Result<CharRead, VlmError> {
        if empty(crop) {
            return Ok(CharRead::default());
        }
        let (canvas, (fx, fy)) = charspot_canvas(crop);
        let (w, h) = (canvas.width, canvas.height);
        // `[1, 3, H, W]`, red plane first, the bytes as they are (0..255).
        let plane = w * h;
        let mut x = vec![0.0f32; 3 * plane];
        for (i, px) in canvas.data.as_chunks::<3>().0.iter().enumerate() {
            for c in 0..3 {
                x[c * plane + i] = f32::from(px[c]);
            }
        }
        let run = |e: bunko_ocr::Error| VlmError::Runtime(format!("charspot: {e}"));
        let out = self
            .model
            .run_named(
                vec![("img", TensorBuf::f32(&[1, 3, h, w], x))],
                &["boxes", "scores", "ids", "order"],
            )
            .map_err(run)?;
        let bad = || VlmError::Runtime("charspot returned tensors of another type".into());
        let boxes = out[0].as_f32().ok_or_else(bad)?;
        let scores = out[1].as_f32().ok_or_else(bad)?;
        let ids = out[2].as_i64().ok_or_else(bad)?;
        let order = out[3].as_i64().ok_or_else(bad)?;
        let n = scores.len();
        if boxes.len() != 4 * n || ids.len() != n {
            return Err(VlmError::Runtime(format!(
                "charspot returned {} box values and {} ids for {n} characters",
                boxes.len(),
                ids.len()
            )));
        }
        let mut chars = Vec::with_capacity(order.len());
        for &k in order {
            let k = usize::try_from(k).ok().filter(|k| *k < n).ok_or_else(|| {
                VlmError::Runtime(format!("charspot orders character {k} of {n}"))
            })?;
            let b = &boxes[4 * k..4 * k + 4];
            chars.push(CharBox {
                text: usize::try_from(ids[k])
                    .ok()
                    .and_then(|id| self.classes.get(id))
                    .cloned()
                    .unwrap_or_default(),
                bbox: [b[0] * fx, b[1] * fy, b[2] * fx, b[3] * fy],
                score: scores[k],
            });
        }
        Ok(CharRead { chars })
    }
}

// --- nar-small ---------------------------------------------------------------

/// manga-ocr-nar-small.
pub struct NarSmall {
    model: Model,
    characters: Vec<String>,
}

impl std::fmt::Debug for NarSmall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NarSmall")
            .field("model", &self.model)
            .finish()
    }
}

impl NarSmall {
    /// `model` is the weightless graph, `weights` the author's `model.safetensors` (any
    /// name, any folder: it reaches ONNX Runtime from memory).
    pub fn load(
        model: &Path,
        weights: &Path,
        vocabulary: &Path,
        opts: &RuntimeOptions,
    ) -> Result<Self, VlmError> {
        let characters = load_characters(vocabulary)?;
        let external = [ExternalFile {
            location: NAR_WEIGHTS_LOCATION.into(),
            path: weights.to_path_buf(),
        }];
        let model = Model::load_with(model, &external, opts)
            .map_err(|e| VlmError::Runtime(e.to_string()))?;
        Ok(Self { model, characters })
    }

    /// The text of one upright crop.
    pub fn read(&self, crop: &Rgb) -> Result<String, VlmError> {
        if empty(crop) {
            return Ok(String::new());
        }
        Ok(nar_text(&self.ids(crop)?, &self.characters))
    }

    /// The model's output for one crop: an id per output slot.
    pub fn ids(&self, crop: &Rgb) -> Result<Vec<i64>, VlmError> {
        if empty(crop) {
            return Ok(Vec::new());
        }
        let input = nar_input(crop);
        let feat = 3 * NAR_PATCH * NAR_PATCH;
        let out = self
            .model
            .run_named(
                vec![
                    ("P", TensorBuf::f32(&[NAR_BUDGET, feat], input.patches)),
                    ("V", TensorBuf::f32(&[NAR_BUDGET], input.valid)),
                    ("R", TensorBuf::i32(&[NAR_BUDGET], input.rows)),
                    ("C", TensorBuf::i32(&[NAR_BUDGET], input.cols)),
                    ("H", TensorBuf::i32(&[1], vec![input.grid.0 as i32])),
                    ("W", TensorBuf::i32(&[1], vec![input.grid.1 as i32])),
                ],
                &["ids"],
            )
            .map_err(|e| VlmError::Runtime(format!("nar-small: {e}")))?;
        let ids: Vec<i64> = match &out[0].data {
            bunko_ocr::runtime::Elements::I32(v) => v.iter().map(|&i| i64::from(i)).collect(),
            bunko_ocr::runtime::Elements::I64(v) => v.clone(),
            _ => {
                return Err(VlmError::Runtime(
                    "nar-small returned ids of another type".into(),
                ));
            }
        };
        if ids.len() != NAR_SLOTS {
            return Err(VlmError::Runtime(format!(
                "nar-small returned {} ids, not {NAR_SLOTS}: a graph of another revision",
                ids.len()
            )));
        }
        Ok(ids)
    }
}

// --- a block's text, line by line ---------------------------------------------

fn cross(o: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Whether `p` lies in the convex `quad` (its border included).
fn inside(quad: &Quad, p: [f64; 2]) -> bool {
    let turns: Vec<f64> = (0..4)
        .map(|k| cross(quad[k], quad[(k + 1) % 4], p))
        .collect();
    turns.iter().all(|t| *t >= 0.0) || turns.iter().all(|t| *t <= 0.0)
}

/// The distance from `p` to the border of `quad`.
fn distance(quad: &Quad, p: [f64; 2]) -> f64 {
    (0..4)
        .map(|k| {
            let (a, b) = (quad[k], quad[(k + 1) % 4]);
            let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
            let len2 = dx * dx + dy * dy;
            let t = if len2 > 0.0 {
                (((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2).clamp(0.0, 1.0)
            } else {
                0.0
            };
            ((p[0] - a[0] - t * dx).powi(2) + (p[1] - a[1] - t * dy).powi(2)).sqrt()
        })
        .fold(f64::INFINITY, f64::min)
}

/// How far outside a line's quad a character's centre may lie and still be the line's,
/// in line thicknesses: a mark the detector's box cut off, not the next block's text.
pub const STRAY_REACH_EM: f64 = 0.5;

/// The thickness of a line's quad: its shorter side.
fn thickness(quad: &Quad) -> f64 {
    let side = |a: [f64; 2], b: [f64; 2]| ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
    side(quad[0], quad[1]).min(side(quad[0], quad[3]))
}

/// The characters of a block read, line by line: each character goes to the line whose
/// quad holds its centre, or else to the nearest one within [`STRAY_REACH_EM`] (a mark
/// the detector's box cut off), and keeps the model's reading order within its line. A
/// character further from every line is another block's (crops of neighbouring blocks
/// overlap) and is left out. One string per quad.
pub fn share_characters(read: &CharRead, lines: &[Quad]) -> Vec<String> {
    let mut out = vec![String::new(); lines.len()];
    for c in &read.chars {
        let centre = [
            f64::from(c.bbox[0] + c.bbox[2]) / 2.0,
            f64::from(c.bbox[1] + c.bbox[3]) / 2.0,
        ];
        let line = lines.iter().position(|q| inside(q, centre)).or_else(|| {
            (0..lines.len())
                .map(|i| (i, distance(&lines[i], centre)))
                .filter(|(i, d)| *d <= STRAY_REACH_EM * thickness(&lines[*i]))
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        });
        if let Some(line) = line {
            out[line].push_str(&c.text);
        }
    }
    out
}

/// A block's text cut into its lines by aligning it with what the line reader read in
/// each (`hints`, in the block's line order): a line's share ends where the last
/// character of its hint is matched, and text between two matches is divided in
/// proportion to the unmatched hint characters on either side of the line break. One
/// string per hint; with a single line, or no hint text at all, the first line gets it.
pub fn share_text(text: &str, hints: &[String]) -> Vec<String> {
    let mut out = vec![String::new(); hints.len()];
    if hints.is_empty() {
        return out;
    }
    let a: Vec<char> = text.chars().collect();
    let b: Vec<char> = hints.iter().flat_map(|h| h.chars()).collect();
    if hints.len() == 1 || b.is_empty() {
        out[0] = text.to_string();
        return out;
    }
    // Matching runs `(start in text, start in hints, length)`, closed by an empty one
    // at both ends.
    let blocks = SequenceMatcher::new(&a, &b).matching_blocks();
    let mut cuts = Vec::with_capacity(hints.len() + 1);
    cuts.push(0usize);
    let mut at = 0usize;
    for hint in &hints[..hints.len() - 1] {
        at += hint.chars().count();
        // The run the break falls in, or the gap it falls in between two runs.
        let (mut a_end, mut b_end) = (0usize, 0usize);
        let mut cut = None;
        for m in &blocks {
            if at <= m.b {
                let (a_gap, b_gap) = (m.a - a_end, m.b - b_end);
                let share = if b_gap == 0 {
                    0
                } else {
                    ((at - b_end) as f64 / b_gap as f64 * a_gap as f64).round() as usize
                };
                cut = Some(a_end + share.min(a_gap));
                break;
            }
            if at <= m.b + m.size {
                cut = Some(m.a + (at - m.b));
                break;
            }
            (a_end, b_end) = (m.a + m.size, m.b + m.size);
        }
        let cut = cut.unwrap_or(a.len()).max(*cuts.last().unwrap_or(&0));
        cuts.push(cut.min(a.len()));
    }
    cuts.push(a.len());
    for (line, span) in out.iter_mut().zip(cuts.windows(2)) {
        *line = a[span[0]..span[1]].iter().collect();
    }
    out
}

/// How far across its line a character may sit from the one before and still follow it,
/// and how far back along the line, both in character sizes.
const LINE_DRIFT: f32 = 0.5;
const LINE_BACK: f32 = 0.25;

/// The lines of a block read, from where its characters are: in the model's reading
/// order a character continues the line while it follows the one before along the
/// line (down a column, or along a row) without leaving it sideways; the first step of
/// a line says which way it runs. A line's box is the box of its characters.
pub fn lines_of(read: &CharRead) -> Vec<LineRead> {
    struct Open {
        chars: Vec<usize>,
        vertical: Option<bool>,
    }
    let centre = |c: &CharBox| ((c.bbox[0] + c.bbox[2]) / 2.0, (c.bbox[1] + c.bbox[3]) / 2.0);
    let size = |c: &CharBox| ((c.bbox[2] - c.bbox[0]) + (c.bbox[3] - c.bbox[1])) / 2.0;
    // Which way `b` follows `a`, if it does: down (`true`) or along (`false`).
    let step = |a: &CharBox, b: &CharBox| -> Option<bool> {
        let s = size(a).max(size(b)).max(1.0);
        let ((ax, ay), (bx, by)) = (centre(a), centre(b));
        let down = (bx - ax).abs() <= LINE_DRIFT * s && by - ay > -LINE_BACK * s;
        let along = (by - ay).abs() <= LINE_DRIFT * s && bx - ax > -LINE_BACK * s;
        match (down, along) {
            (true, false) => Some(true),
            (false, true) => Some(false),
            // Almost on top of each other: whichever way it moved more.
            (true, true) => Some((by - ay).abs() >= (bx - ax).abs()),
            (false, false) => None,
        }
    };
    let mut lines: Vec<Open> = Vec::new();
    for (i, c) in read.chars.iter().enumerate() {
        let follows = lines.last().and_then(|line| {
            let prev = &read.chars[*line.chars.last()?];
            step(prev, c).filter(|way| line.vertical.is_none_or(|v| v == *way))
        });
        match (follows, lines.last_mut()) {
            (Some(way), Some(line)) => {
                line.vertical = Some(way);
                line.chars.push(i);
            }
            _ => lines.push(Open {
                chars: vec![i],
                vertical: None,
            }),
        }
    }
    // A line of one character runs the way most of the block's lines do, or down.
    let (down, along) = lines.iter().fold((0, 0), |(d, a), l| match l.vertical {
        Some(true) => (d + 1, a),
        Some(false) => (d, a + 1),
        None => (d, a),
    });
    lines
        .into_iter()
        .map(|line| {
            let mut bbox = [f32::INFINITY, f32::INFINITY, 0.0f32, 0.0f32];
            let (mut text, mut confs) = (String::new(), Vec::new());
            for &i in &line.chars {
                let c = &read.chars[i];
                bbox = [
                    bbox[0].min(c.bbox[0]),
                    bbox[1].min(c.bbox[1]),
                    bbox[2].max(c.bbox[2]),
                    bbox[3].max(c.bbox[3]),
                ];
                text.push_str(&c.text);
                // A combined `!?` glyph is several characters with one score.
                confs.extend(std::iter::repeat_n(c.score, c.text.chars().count()));
            }
            LineRead {
                text,
                bbox,
                vertical: line.vertical.unwrap_or(down >= along),
                confs,
            }
        })
        .collect()
}

/// nar-small's text read from a whole crop: one line, the whole crop (it knows no
/// positions); no line when it read nothing.
fn whole_crop_line(text: String, crop: &Rgb) -> Vec<LineRead> {
    if text.is_empty() {
        return Vec::new();
    }
    vec![LineRead {
        text,
        bbox: [0.0, 0.0, crop.width as f32, crop.height as f32],
        vertical: crop.height >= crop.width,
        confs: Vec::new(),
    }]
}

impl BlockReader for TextboxRecognizer {
    fn read_lines(&self, crops: &[Rgb]) -> Result<Vec<Vec<LineRead>>, VlmError> {
        #[cfg(feature = "torch")]
        if let Reader::Torch(r) = &self.reader {
            let reads = self.read_all(r.as_ref(), &crops.iter().collect::<Vec<_>>())?;
            return Ok(reads
                .into_iter()
                .zip(crops)
                .map(|(read, crop)| match read.chars {
                    None => whole_crop_line(read.text, crop),
                    Some(_) => lines_of(&char_read(&read)),
                })
                .collect());
        }
        crops
            .iter()
            .map(|crop| {
                Ok(match &self.reader {
                    Reader::Charspot(m) => lines_of(&m.read(crop)?),
                    Reader::Nar(m) => whole_crop_line(m.read(crop)?, crop),
                    #[cfg(feature = "torch")]
                    Reader::Torch(_) => unreachable!("read in one call above"),
                })
            })
            .collect()
    }

    fn read_blocks(&self, blocks: &[BlockCrop]) -> Result<Vec<Vec<String>>, VlmError> {
        #[cfg(feature = "torch")]
        if let Reader::Torch(r) = &self.reader {
            let images: Vec<&Rgb> = blocks.iter().map(|b| &b.image).collect();
            let reads = self.read_all(r.as_ref(), &images)?;
            return Ok(reads
                .into_iter()
                .zip(blocks)
                .map(|(read, block)| match read.chars {
                    None => share_text(&read.text, &block.hints),
                    Some(_) => share_characters(&char_read(&read), &block.lines),
                })
                .collect());
        }
        blocks
            .iter()
            .map(|block| {
                Ok(match &self.reader {
                    Reader::Charspot(m) => share_characters(&m.read(&block.image)?, &block.lines),
                    Reader::Nar(m) => share_text(&m.read(&block.image)?, &block.hints),
                    #[cfg(feature = "torch")]
                    Reader::Torch(_) => unreachable!("read in one call above"),
                })
            })
            .collect()
    }
}

// --- the GPU back-end ----------------------------------------------------------

/// A back-end that reads many whole text-box crops in one call and answers one
/// [`CharsRead`] per crop, in crop order: the libtorch pack's `bt_read_chars`
/// ([`crate::torch::TorchTextbox`]). It splits the crops into batches itself and reads
/// `""` for an empty crop.
#[cfg(feature = "torch")]
pub trait CharsReader: Send + Sync {
    fn read_chars(&self, crops: &[&Rgb]) -> Result<Vec<CharsRead>, VlmError>;
}

#[cfg(feature = "torch")]
impl CharsReader for crate::torch::TorchTextbox {
    fn read_chars(&self, crops: &[&Rgb]) -> Result<Vec<CharsRead>, VlmError> {
        crate::torch::TorchTextbox::read_chars(self, crops)
    }
}

/// The characters of a crop the pack read, as charspot's ONNX route gives them: each
/// `(c, x0, y0, x1, y1, score)` a [`CharBox`]. No characters (nar-small) is no box.
#[cfg(feature = "torch")]
fn char_read(r: &CharsRead) -> CharRead {
    CharRead {
        chars: r
            .chars
            .iter()
            .flatten()
            .map(|CharEntry(c, x0, y0, x1, y1, s)| CharBox {
                text: c.clone(),
                bbox: [*x0, *y0, *x1, *y1],
                score: *s,
            })
            .collect(),
    }
}

// --- the recognizer seam -----------------------------------------------------

enum Reader {
    Charspot(Charspot),
    Nar(NarSmall),
    /// The GPU route: a page's crops in one call.
    #[cfg(feature = "torch")]
    Torch(Arc<dyn CharsReader>),
}

/// A text-box recognizer behind the pipeline's [`Recognizer`] seam. The pipeline reads
/// with its [`BlockReader`]; the line methods read one line's upright crop each, for a
/// caller that has no blocks.
pub struct TextboxRecognizer {
    reader: Reader,
    info: RecognizerInfo,
}

impl TextboxRecognizer {
    /// `engine` is `charspot-small` or `nar-small`; `weights` is the weights file of a
    /// graph that holds none (nar-small's). `source` is the pinned `(repo, revision)`
    /// the files came from, when they are the pinned files.
    pub fn load(
        engine: &str,
        model: &Path,
        weights: Option<&Path>,
        vocabulary: &Path,
        opts: &RuntimeOptions,
        source: Option<(&'static str, &'static str)>,
    ) -> Result<Self, VlmError> {
        let (reader, engine, recognizer) = match engine {
            "charspot-small" => (
                Reader::Charspot(Charspot::load(model, vocabulary, opts)?),
                "charspot-small",
                "muscgab/manga-ocr-charspot-small",
            ),
            "nar-small" => (
                Reader::Nar(NarSmall::load(
                    model,
                    weights.ok_or_else(|| {
                        VlmError::Config("nar-small needs its weights file".into())
                    })?,
                    vocabulary,
                    opts,
                )?),
                "nar-small",
                "muscgab/manga-ocr-nar-small",
            ),
            other => {
                return Err(VlmError::Config(format!(
                    "{other} is no text-box recognizer"
                )));
            }
        };
        Ok(Self {
            reader,
            info: RecognizerInfo {
                engine,
                recognizer,
                repos: source.into_iter().collect(),
                precision: Precision::Fp32,
                device: Device::Cpu,
                token_caps: false,
                second_read: false,
                default_max_tokens: 0,
                batch: 1,
                patch_budget: None,
            },
        })
    }

    /// A text-box recognizer on a back-end that reads a page's crops in one call (the
    /// libtorch pack). `engine` is `charspot-small` or `nar-small`: it names the engine
    /// in `info` and in errors; the rest of `info` (device, precision, batch) is the
    /// caller's.
    #[cfg(feature = "torch")]
    pub fn with_reader(
        engine: &'static str,
        reader: Arc<dyn CharsReader>,
        info: RecognizerInfo,
    ) -> TextboxRecognizer {
        Self {
            reader: Reader::Torch(reader),
            info: RecognizerInfo { engine, ..info },
        }
    }

    /// One [`CharsRead`] per crop from the back-end, or an error naming both counts: a
    /// pack that answers too few or too many would put texts on the wrong blocks.
    #[cfg(feature = "torch")]
    fn read_all(&self, r: &dyn CharsReader, crops: &[&Rgb]) -> Result<Vec<CharsRead>, VlmError> {
        if crops.is_empty() {
            return Ok(Vec::new());
        }
        let reads = r.read_chars(crops)?;
        if reads.len() != crops.len() {
            return Err(VlmError::Runtime(format!(
                "{}: the back-end returned {} results for {} crops",
                self.info.engine,
                reads.len(),
                crops.len()
            )));
        }
        Ok(reads)
    }

    /// The text of each upright crop, in order: one call on the GPU back-end, a crop at a
    /// time on ONNX Runtime.
    fn read_crops(&self, crops: &[&Rgb]) -> Result<Vec<String>, VlmError> {
        match &self.reader {
            Reader::Charspot(m) => crops.iter().map(|c| m.read(c).map(|r| r.text())).collect(),
            Reader::Nar(m) => crops.iter().map(|c| m.read(c)).collect(),
            #[cfg(feature = "torch")]
            Reader::Torch(r) => Ok(self
                .read_all(r.as_ref(), crops)?
                .into_iter()
                .map(|read| read.text)
                .collect()),
        }
    }
}

impl Recognizer for TextboxRecognizer {
    fn info(&self) -> &RecognizerInfo {
        &self.info
    }

    fn block_reader(&self) -> Option<&dyn BlockReader> {
        Some(self)
    }

    fn verbatim(&self) -> bool {
        true
    }

    fn crop(&self, page: &Bgr, quad: &Quad, _vertical: bool) -> CropSet {
        CropSet::one(paddle_quad_crop(page, quad, LINE_MARGIN_EM))
    }

    fn second_crop(&self, _page: &Bgr, _quad: &Quad, _vertical: bool) -> Option<CropSet> {
        None
    }

    fn read(&self, lines: &[CropSet], _caps: Option<&[u32]>) -> Result<Vec<String>, VlmError> {
        read_flat(lines, None, 0, |crops, _| self.read_crops(crops))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(text: &str, x: f32, y: f32) -> CharBox {
        CharBox {
            text: text.into(),
            bbox: [x - 5.0, y - 5.0, x + 5.0, y + 5.0],
            score: 0.9,
        }
    }

    fn column(x0: f64, x1: f64) -> Quad {
        [[x0, 0.0], [x1, 0.0], [x1, 100.0], [x0, 100.0]]
    }

    #[test]
    fn characters_go_to_the_line_that_holds_them() {
        // Two columns read right to left, and the model's order runs down the right
        // one, then down the left one.
        let lines = [column(60.0, 80.0), column(20.0, 40.0)];
        let read = CharRead {
            chars: vec![
                ch("今", 70.0, 10.0),
                ch("日", 70.0, 30.0),
                ch("は", 30.0, 10.0),
                // A mark just below the left column's box: still that line's.
                ch("!?", 31.0, 108.0),
                // Between the columns, nearer the right one.
                ch("、", 52.0, 50.0),
                // A column of the next block, further than half a line away: not ours.
                ch("別", 0.0, 50.0),
            ],
        };
        assert_eq!(share_characters(&read, &lines), ["今日、", "は!?"]);
        // The right column alone: the left column's characters are too far from it.
        assert_eq!(share_characters(&read, &lines[..1]), ["今日、"]);
        assert!(share_characters(&read, &[]).is_empty());
        assert_eq!(share_characters(&CharRead::default(), &lines), ["", ""]);
    }

    #[test]
    fn characters_in_reading_order_make_the_blocks_lines() {
        // Two columns read right to left (three and two characters), then a row.
        let read = CharRead {
            chars: vec![
                ch("今", 70.0, 10.0),
                ch("日", 70.0, 22.0),
                ch("は", 71.0, 34.0),
                ch("晴", 50.0, 10.0),
                ch("れ", 50.0, 22.0),
                ch("O", 10.0, 80.0),
                ch("K", 22.0, 80.0),
                ch("!?", 34.0, 81.0),
            ],
        };
        let lines = lines_of(&read);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["今日は", "晴れ", "OK!?"]);
        assert_eq!(
            lines.iter().map(|l| l.vertical).collect::<Vec<_>>(),
            [true, true, false]
        );
        assert_eq!(lines[0].bbox, [65.0, 5.0, 76.0, 39.0]);
        // One confidence per character of the text, a combined glyph's for each.
        assert_eq!(lines[2].confs.len(), 4);
        // A single character: a line that runs like the others (here: down).
        let lone = lines_of(&CharRead {
            chars: vec![ch("あ", 10.0, 10.0)],
        });
        assert_eq!((lone.len(), lone[0].vertical), (1, true));
        assert!(lines_of(&CharRead::default()).is_empty());
    }

    fn hints(h: &[&str]) -> Vec<String> {
        h.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_blocks_text_is_cut_where_its_lines_end() {
        // The line reader's text is the same: cut at its line ends.
        assert_eq!(
            share_text(
                "今日はいい天気ですね",
                &hints(&["今日は", "いい天気", "ですね"])
            ),
            ["今日は", "いい天気", "ですね"]
        );
        // The line reader misread a character inside a line (天 as 夭): the cuts stay.
        assert_eq!(
            share_text(
                "今日はいい天気ですね",
                &hints(&["今日は", "いい夭気", "ですね"])
            ),
            ["今日は", "いい天気", "ですね"]
        );
        // It misread the last character of a line (気 as 汽) and missed the next (で):
        // both unmatched characters lie where the line reader has one, on the earlier
        // line's side of the break, so the earlier line takes them. Nothing is lost,
        // and the text reads the same line after line.
        assert_eq!(
            share_text(
                "今日はいい天気ですね",
                &hints(&["今日は", "いい天汽", "すね"])
            ),
            ["今日は", "いい天気で", "すね"]
        );
        // It missed one of two equal characters at the start of a line: the match runs
        // on across the break, and the cut stays where the earlier line's hint ends.
        assert_eq!(
            share_text("今日はいい天気", &hints(&["今日は", "い天気"])),
            ["今日は", "いい天気"]
        );
        // Text the line reader has nothing for goes with the lines around it; nothing
        // is lost.
        let cut = share_text("あいうえおかきくけこ", &hints(&["あい", "", "けこ"]));
        assert_eq!(cut.concat(), "あいうえおかきくけこ");
        assert_eq!(cut[0], "あい");
        assert_eq!(cut[2], "うえおかきくけこ");
        // One line, or no hints at all: the first line gets the text.
        assert_eq!(share_text("あいう", &hints(&["x"])), ["あいう"]);
        assert_eq!(share_text("あいう", &hints(&["", ""])), ["あいう", ""]);
        assert!(share_text("あいう", &[]).is_empty());
        assert_eq!(share_text("", &hints(&["あ", "い"])), ["", ""]);
    }

    #[cfg(feature = "torch")]
    mod torch_back_end {
        use super::*;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// A back-end that counts its calls and answers `out(i)` for the `i`th crop.
        struct Fake {
            calls: AtomicUsize,
            out: fn(usize) -> CharsRead,
        }

        impl CharsReader for Fake {
            fn read_chars(&self, crops: &[&Rgb]) -> Result<Vec<CharsRead>, VlmError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok((0..crops.len()).map(self.out).collect())
            }
        }

        /// A back-end that answers one entry too few: a mismatched pack.
        struct Short;

        impl CharsReader for Short {
            fn read_chars(&self, crops: &[&Rgb]) -> Result<Vec<CharsRead>, VlmError> {
                Ok(vec![
                    CharsRead {
                        text: "x".into(),
                        chars: None
                    };
                    crops.len().saturating_sub(1)
                ])
            }
        }

        fn crop(w: usize, h: usize) -> Rgb {
            Rgb {
                width: w,
                height: h,
                data: vec![9; w * h * 3],
            }
        }

        fn test_info() -> RecognizerInfo {
            RecognizerInfo {
                engine: "nar-small",
                recognizer: "muscgab/manga-ocr-nar-small",
                repos: Vec::new(),
                precision: Precision::Fp32,
                device: Device::Gpu(0),
                token_caps: false,
                second_read: false,
                default_max_tokens: 0,
                batch: 64,
                patch_budget: None,
            }
        }

        fn fake(out: fn(usize) -> CharsRead) -> Arc<Fake> {
            Arc::new(Fake {
                calls: 0.into(),
                out,
            })
        }

        #[test]
        fn a_page_is_one_call_on_the_torch_back_end() {
            let fake = fake(|i| CharsRead {
                text: format!("t{i}"),
                chars: None,
            });
            let r = TextboxRecognizer::with_reader("nar-small", fake.clone(), test_info());
            let lines = r
                .read_lines(&[crop(10, 40), crop(30, 10), crop(5, 5)])
                .unwrap();
            assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[1][0].text, "t1");
            assert_eq!(lines[1][0].bbox, [0.0, 0.0, 30.0, 10.0]);
            assert!(!lines[1][0].vertical);
            assert!(lines[0][0].vertical);
            assert_eq!(r.info().device, Device::Gpu(0));
        }

        #[test]
        fn results_keep_crop_order_across_chunks() {
            let fake = fake(|i| CharsRead {
                text: i.to_string(),
                chars: None,
            });
            let r = TextboxRecognizer::with_reader("nar-small", fake, test_info());
            let crops: Vec<Rgb> = (0..130).map(|_| crop(8, 8)).collect();
            let lines = r.read_lines(&crops).unwrap();
            assert_eq!(lines.len(), 130);
            assert_eq!(lines[129][0].text, "129");
            assert_eq!(lines[64][0].text, "64");
        }

        #[test]
        fn an_empty_text_is_no_line() {
            let fake = fake(|_| CharsRead {
                text: String::new(),
                chars: None,
            });
            let r = TextboxRecognizer::with_reader("nar-small", fake, test_info());
            assert!(r.read_lines(&[crop(10, 10)]).unwrap()[0].is_empty());
        }

        #[test]
        fn a_blocks_text_is_shared_out_in_one_call() {
            let fake = fake(|i| CharsRead {
                text: if i == 0 {
                    "今日はいい天気".into()
                } else {
                    "あ".into()
                },
                chars: None,
            });
            let r = TextboxRecognizer::with_reader("nar-small", fake.clone(), test_info());
            let block = |hints: &[&str]| BlockCrop {
                image: crop(20, 40),
                lines: vec![column(0.0, 10.0); hints.len()],
                hints: hints.iter().map(|h| h.to_string()).collect(),
            };
            let out = r
                .read_blocks(&[block(&["今日は", "いい天気"]), block(&["x"])])
                .unwrap();
            assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
            assert_eq!(out, [vec!["今日は", "いい天気"], vec!["あ"]]);
        }

        #[test]
        fn characters_from_the_pack_make_lines_and_share_out() {
            // charspot's answer: two characters down a column, boxes in crop pixels.
            let fake = fake(|_| CharsRead {
                text: "今日".into(),
                chars: Some(vec![
                    CharEntry("今".into(), 65.0, 5.0, 75.0, 15.0, 0.9),
                    CharEntry("日".into(), 65.0, 25.0, 75.0, 35.0, 0.8),
                ]),
            });
            let r = TextboxRecognizer::with_reader("charspot-small", fake, test_info());
            assert_eq!(r.info().engine, "charspot-small");
            let lines = r.read_lines(&[crop(80, 40)]).unwrap();
            assert_eq!(lines[0].len(), 1);
            assert_eq!(lines[0][0].text, "今日");
            assert_eq!(lines[0][0].bbox, [65.0, 5.0, 75.0, 35.0]);
            assert_eq!(lines[0][0].confs, [0.9, 0.8]);
            let block = BlockCrop {
                image: crop(80, 40),
                lines: vec![column(60.0, 80.0), column(20.0, 40.0)],
                hints: vec![String::new(), String::new()],
            };
            assert_eq!(r.read_blocks(&[block]).unwrap(), [vec!["今日", ""]]);
        }

        #[test]
        fn the_line_methods_read_every_crop_in_one_call() {
            let fake = fake(|i| CharsRead {
                text: format!("l{i}"),
                chars: None,
            });
            let r = TextboxRecognizer::with_reader("nar-small", fake.clone(), test_info());
            let sets = [CropSet::one(crop(4, 4)), CropSet::one(crop(6, 6))];
            assert_eq!(r.read(&sets, None).unwrap(), ["l0", "l1"]);
            assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn a_short_answer_is_an_error_naming_both_counts() {
            let r = TextboxRecognizer::with_reader("nar-small", Arc::new(Short), test_info());
            let e = r.read_lines(&[crop(4, 4), crop(4, 4)]).unwrap_err();
            assert!(e.to_string().contains("1 results for 2 crops"), "{e}");
            assert!(r.read(&[CropSet::one(crop(4, 4))], None).is_err());
        }

        #[test]
        fn a_pack_character_is_a_char_box() {
            let read = CharsRead {
                text: "!?".into(),
                chars: Some(vec![CharEntry("!?".into(), 1.0, 2.0, 3.0, 4.0, 0.5)]),
            };
            assert_eq!(
                char_read(&read),
                CharRead {
                    chars: vec![CharBox {
                        text: "!?".into(),
                        bbox: [1.0, 2.0, 3.0, 4.0],
                        score: 0.5,
                    }]
                }
            );
            assert_eq!(
                char_read(&CharsRead {
                    text: String::new(),
                    chars: None
                }),
                CharRead::default()
            );
        }
    }

    /// The r16 graph with the author's weights gives the ids the author's PyTorch code
    /// gives (`tools/textbox_export/check_parity.py` writes them), crop for crop,
    /// through this module's own preprocessing:
    ///
    /// ```text
    /// MOKURO_NAR_PARITY_CROPS=<crops> MOKURO_NAR_PARITY_EXPECTED=<expected.jsonl> \
    /// MOKURO_NAR_GRAPH=<graph.onnx> MOKURO_NAR_WEIGHTS=<model.safetensors> \
    /// MOKURO_NAR_VOCAB=<nar_vocabulary_6817.json> \
    ///   cargo test -p bunko-engines --features nc-models --release --lib -- --ignored nar_parity
    /// ```
    #[test]
    #[ignore = "needs the model files and crops (env vars)"]
    fn nar_parity_with_the_authors_code() {
        let var = |k: &str| {
            std::path::PathBuf::from(
                std::env::var_os(k).unwrap_or_else(|| panic!("{k} is not set")),
            )
        };
        let opts = RuntimeOptions {
            intra_threads: 8,
            ..Default::default()
        };
        let nar = NarSmall::load(
            &var("MOKURO_NAR_GRAPH"),
            &var("MOKURO_NAR_WEIGHTS"),
            &var("MOKURO_NAR_VOCAB"),
            &opts,
        )
        .unwrap();
        let crops = var("MOKURO_NAR_PARITY_CROPS");
        let expected = std::fs::read_to_string(var("MOKURO_NAR_PARITY_EXPECTED")).unwrap();
        let (mut same, mut differ) = (0, Vec::new());
        for line in expected.lines().filter(|l| !l.trim().is_empty()) {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            let file = row["file"].as_str().unwrap();
            let want: Vec<i64> = row["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_i64().unwrap())
                .collect();
            let img = image::open(crops.join(file)).unwrap().to_rgb8();
            let crop = Rgb {
                width: img.width() as usize,
                height: img.height() as usize,
                data: img.into_raw(),
            };
            let got = nar.ids(&crop).unwrap();
            assert_eq!(got.len(), NAR_SLOTS);
            if got == want && nar.read(&crop).unwrap() == row["text"].as_str().unwrap() {
                same += 1;
            } else {
                differ.push(file.to_string());
            }
        }
        assert!(same > 0, "no crops");
        assert!(differ.is_empty(), "{same} identical; differ: {differ:?}");
    }
}
