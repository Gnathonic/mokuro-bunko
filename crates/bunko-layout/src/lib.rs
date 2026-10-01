//! mokuro-bunko's OCR output stage, ported from 0.5.2 Python and kept
//! byte-compatible with it:
//!
//! * [`records`] — the per-page line records the PP-OCR reader produces
//!   (`ppocr-lines/1`), and the rounding the layout runs on;
//! * [`layout`] — `line_layout.py`: lines -> mokuro blocks (orientation, ruby
//!   removal, text bodies, roles, merging, paragraphs, reading order, boxes);
//! * [`reconcile`] — `line_reconcile.py`: an engine (VLM) read merged with the
//!   CTC read, over an exact port of `difflib.SequenceMatcher` ([`difflib`]);
//! * [`road`] — the reconciled road's pure half (`ReconciledPageReader`):
//!   glyph room, engine-only verdicts, seam trim, raw dump;
//! * [`sidecar`] — page/volume dicts, the `ocr_engine` block, the runner's
//!   atomic write and the server's normalising rewrite;
//! * [`json`] / [`py`] — Python's `json` and float semantics, which the
//!   byte-for-byte output depends on.
//!
//! Pure and synchronous: no I/O except writing a sidecar.
//!
//! # Known deviations
//!
//! * NFKC (`reconcile::fold`) comes from `unicode-normalization`, which tracks
//!   a newer Unicode than Python 3.12's 15.0; characters added since then may
//!   fold differently (spec Q7).
//! * `math.atan2/cos/sin` and `**` go to the platform libm, as CPython's do;
//!   golden parity is established on Linux/glibc.
//! * A JSON string holding a lone surrogate (`"\ud800"`) loads in Python but
//!   cannot be a Rust `String`; [`json::Value::parse`] rejects it.

pub mod difflib;
pub mod json;
pub mod layout;
pub mod py;
pub mod reconcile;
pub mod records;
pub mod road;
pub mod script;
pub mod sidecar;

pub use json::{Separators, Value};
pub use layout::{Block, Body, Kind, PageLayout, Ruby, column_pieces, layout_page};
pub use reconcile::{Reconciled, reconcile_line, settle_disputes};
pub use records::{RawLine, RawPage};
pub use road::EngineRoad;
pub use script::normalize_text;
pub use sidecar::{
    FinishedPage, LayerStamp, Normalization, OcrEngine, Page, VolumeHeader, build_volume, finish_page,
    layout_page_dict, normalize_sidecar, normalize_sidecar_file, write_sidecar,
};
