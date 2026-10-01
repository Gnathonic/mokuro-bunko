//! The `ppocr-manga` page reader (`PPOcrPageReader.read_lines`,
//! `engine_runner.py:4687-4730`, spec §5): read the page, re-read joined column
//! pieces, probe line ends, vote on doubted kanji — and the raw page JSON
//! (`page_to_json`) the layout consumes.
//!
//! The passes need two answers from the line layout (which lines are pieces of one
//! column; which are ruby, which belong to a text body). Those come through
//! [`LayoutHooks`], implemented by `bunko-layout`, so this crate does not depend on
//! it. The layout always sees the *rounded* raw page, as in 0.5.2.

use std::collections::BTreeSet;

use super::geometry::{Quad, quad_size, slice_quad};
use super::{Line, PpOcr};
use crate::error::Result;
use crate::image::BgrImage;
use crate::lines::{DetectorInfo, FORMAT_ID, RawLine, RawPage};
use crate::py;

pub const JOIN_MIN_CONF: f64 = 0.6;
pub const PROBE_PAD_EM: f64 = 1.0;
pub const PROBE_INSIDE_EM: f64 = 2.5;
pub const PROBE_MIN_GLYPHS: usize = 2;
pub const PROBE_GROW_PITCH: f64 = 0.5;
pub const PROBE_GROW_PITCH_THIN: f64 = 1.0;
pub const VOTE_MIN_GLYPHS: usize = 4;
pub const VOTE_WIDEN: [f64; 2] = [0.06, 0.12];

/// The end probes of a line: one over the whole line when it is short, else a
/// start and an end probe.
pub fn probe_quads(q: &Quad) -> Vec<Quad> {
    let (w, h) = quad_size(q);
    let (length, em) = (w.max(h), w.min(h));
    let (pad, inside) = (PROBE_PAD_EM * em, PROBE_INSIDE_EM * em);
    if length <= 2.0 * inside {
        return vec![slice_quad(q, -pad, length + pad)];
    }
    vec![
        slice_quad(q, -pad, inside),
        slice_quad(q, length - inside, length + pad),
    ]
}

/// What the page-level passes need from the line layout.
pub trait LayoutHooks {
    /// The first layout of a page (kept and returned to the caller, which the
    /// reconciled engines reuse).
    type Layout;

    /// `line_layout.column_pieces(raw)`: groups of raw line indices (ascending) that
    /// are pieces of one printed column or row.
    fn column_pieces(&self, raw: &RawPage) -> Vec<Vec<usize>>;

    /// `line_layout.layout_page(raw)`.
    fn layout(&self, raw: &RawPage) -> Self::Layout;

    /// Indices of the lines the layout took for ruby (`{run.line for run in ruby}`).
    fn ruby_lines(&self, layout: &Self::Layout) -> BTreeSet<usize>;

    /// Indices of the lines that are members of a text body (`∪ body.members`).
    fn body_lines(&self, layout: &Self::Layout) -> BTreeSet<usize>;
}

/// No layout: no joins, no ruby, no bodies (probes and votes still run on every
/// line). For tools that only want detection + recognition.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLayout;

impl LayoutHooks for NoLayout {
    type Layout = ();

    fn column_pieces(&self, _raw: &RawPage) -> Vec<Vec<usize>> {
        Vec::new()
    }

    fn layout(&self, _raw: &RawPage) {}

    fn ruby_lines(&self, _layout: &()) -> BTreeSet<usize> {
        BTreeSet::new()
    }

    fn body_lines(&self, _layout: &()) -> BTreeSet<usize> {
        BTreeSet::new()
    }
}

/// `ppocr.page_to_json`: lines → the raw page, rounded as Python rounds.
pub fn page_to_json(
    lines: &[Line],
    width: usize,
    height: usize,
    detector: Option<DetectorInfo>,
) -> RawPage {
    let out = lines
        .iter()
        .map(|l| {
            let mut quad = [[0.0f64; 2]; 4];
            for (dst, src) in quad.iter_mut().zip(&l.quad) {
                *dst = [
                    py::round_to(src[0] as f64, 2),
                    py::round_to(src[1] as f64, 2),
                ];
            }
            RawLine {
                quad,
                score: py::round_to(l.score, 4),
                text: l.text.clone(),
                conf: py::round_to(l.conf, 4),
                vertical: l.vertical(),
                angle: py::round_to(l.angle(), 2),
                char_confs: l.char_confs.iter().map(|&c| py::round_to(c, 4)).collect(),
            }
        })
        .collect();
    RawPage {
        format: FORMAT_ID.into(),
        width: width as u32,
        height: height as u32,
        detector,
        lines: out,
    }
}

/// The result of [`PpocrPageReader::read_lines`].
#[derive(Debug, Clone)]
pub struct ReadLines<L> {
    /// The page's lines after joins, probes and votes, unrounded.
    pub lines: Vec<Line>,
    pub info: DetectorInfo,
    /// The first layout (run to tell ruby and bodies apart); its indices refer to
    /// `lines`.
    pub first: L,
}

impl<L> ReadLines<L> {
    /// The raw page the layout consumes.
    pub fn raw(&self, width: usize, height: usize) -> RawPage {
        page_to_json(&self.lines, width, height, Some(self.info.clone()))
    }
}

/// The `ppocr-manga` engine for a page image.
#[derive(Debug, Clone)]
pub struct PpocrPageReader {
    engine: PpOcr,
    /// The model files are the pinned upstream revision (the sidecar may claim it).
    pinned: bool,
}

impl PpocrPageReader {
    pub fn new(engine: PpOcr, pinned: bool) -> Self {
        Self { engine, pinned }
    }

    pub fn engine(&self) -> &PpOcr {
        &self.engine
    }

    /// `{repo: revision}` for the sidecar's `ocr_engine.weights`, empty when the files
    /// are not the pinned download.
    pub fn repos(&self) -> Vec<(String, String)> {
        if self.pinned {
            vec![(
                crate::models::PPOCR_REPO.into(),
                crate::models::PPOCR_REVISION.into(),
            )]
        } else {
            Vec::new()
        }
    }

    /// Detect and read a page's lines, then the page-level passes (§5.1).
    pub fn read_lines<H: LayoutHooks>(
        &self,
        img: &BgrImage,
        hooks: &H,
    ) -> Result<ReadLines<H::Layout>> {
        let (w, h) = (img.width(), img.height());
        let (mut lines, mut info) = self.engine.read_page(img)?;
        let raw = page_to_json(&lines, w, h, Some(info.clone()));
        // 1. pieces of one printed column are read again as one line
        let pieces = hooks.column_pieces(&raw);
        let raw = if pieces.is_empty() {
            raw
        } else {
            let joined = self.engine.join_lines(img, &lines, &pieces)?;
            info.joined = Some(lines.len() - joined.len());
            lines = joined;
            page_to_json(&lines, w, h, Some(info.clone()))
        };
        // 2. brackets and stops the boxes left out; ruby excluded
        let first = hooks.layout(&raw);
        let ruby = hooks.ruby_lines(&first);
        let in_body = hooks.body_lines(&first);
        let targets: Vec<usize> = (0..lines.len()).filter(|i| !ruby.contains(i)).collect();
        let thin: Vec<bool> = targets.iter().map(|i| in_body.contains(i)).collect();
        info.recovered_ends = Some(
            self.engine
                .recover_clipped_ends(img, &mut lines, &targets, &thin)?,
        );
        // 3. doubted characters put to a vote
        info.second_opinions = Some(self.engine.second_opinions(img, &mut lines, &targets)?);
        Ok(ReadLines { lines, info, first })
    }
}
