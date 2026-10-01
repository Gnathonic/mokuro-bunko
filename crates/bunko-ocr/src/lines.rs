//! The raw line records a page reader produces: `ppocr-lines/1`.
//!
//! This is the JSON `ppocr.page_to_json` writes in 0.5.2 (spec §5.0) and the only
//! input of the line layout (`bunko-layout`). The values are already rounded the
//! way Python rounds them — quads and angle to 0.01, score/conf/char_confs to 4
//! decimals, half-even on the exact binary value — because the layout's thresholds
//! (`conf < 0.5`, `score >= 0.80`, ...) are evaluated on the rounded numbers.
//!
//! JSON shape (field order as Python writes it):
//!
//! ```json
//! {"format": "ppocr-lines/1", "width": 1925, "height": 2800,
//!  "detector": {"side": 1120, "tile": "auto",
//!               "passes": [{"scale": 0.399, "tiles": 1, "lines": 21, "why": "first"}],
//!               "joined": 1, "recovered_ends": 0, "second_opinions": 0},
//!  "lines": [{"quad": [[x, y], [x, y], [x, y], [x, y]], "score": 0.91, "text": "…",
//!             "conf": 0.97, "vertical": true, "angle": -0.4, "char_confs": [0.99, …]}]}
//! ```
//!
//! `detector` is debug information (absent when a caller passes none); `joined` is
//! present only when the page had column pieces to re-read, `recovered_ends` and
//! `second_opinions` only after the page-level passes ran.

use serde::{Deserialize, Serialize};

/// The `format` value of a raw page.
pub const FORMAT_ID: &str = "ppocr-lines/1";

/// One quad corner, `[x, y]` in page pixels.
pub type Point = [f64; 2];

/// One detected and recognized line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawLine {
    /// TL, TR, BR, BL of the line's own upright reading frame, rounded to 0.01 px.
    pub quad: [Point; 4],
    /// Detector score (mean probability inside the contour), 4 decimals.
    pub score: f64,
    /// Recognized text (may be empty).
    pub text: String,
    /// Mean CTC confidence of the decoded characters (before gap filling), 4 decimals.
    pub conf: f64,
    /// Reading axis is the quad's TL→BL edge (`height > width`).
    pub vertical: bool,
    /// Tilt in degrees, (-45, 45], positive = clockwise on screen, 2 decimals.
    pub angle: f64,
    /// Per-character confidences, one per `char` of `text`, 4 decimals.
    #[serde(default)]
    pub char_confs: Vec<f64>,
}

/// One pass of the detector (debug only).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectorPass {
    pub scale: f64,
    pub tiles: usize,
    pub lines: usize,
    pub why: String,
}

/// What the detector ran and what the page-level passes changed (debug only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DetectorInfo {
    pub side: u32,
    pub tile: String,
    pub passes: Vec<DetectorPass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovered_ends: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_opinions: Option<usize>,
}

/// A page of raw lines (`ppocr-lines/1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawPage {
    pub format: String,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detector: Option<DetectorInfo>,
    pub lines: Vec<RawLine>,
}
