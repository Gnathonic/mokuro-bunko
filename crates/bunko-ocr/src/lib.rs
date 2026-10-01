//! OCR for mokuro-bunko 0.7: ONNX Runtime engines and everything around them.
//!
//! * [`runtime`] — sessions with execution-provider preference and CPU fallback,
//!   the device catalog;
//! * [`models`] — the model manifest and the verified, resumable model store;
//! * [`image`] — page decoding and the OpenCV primitives the models depend on,
//!   reproduced bit-for-bit where it matters, plus the cover thumbnail;
//! * [`pages`] — which archive members are pages, in which order, read one by one;
//! * [`ppocr`] — PP-OCRv6 manga detection + recognition and the page-level passes;
//! * [`lines`] — the raw line records (`ppocr-lines/1`) the layout consumes.
//!
//! Everything is synchronous; callers on an async runtime use `spawn_blocking` or
//! dedicated threads. Behaviour follows mokuro-bunko 0.5.2
//! (`docs/rust-port/spec/ocr-ppocr-layout.md`).

pub mod error;
pub mod image;
pub mod lines;
pub mod models;
pub mod models_release;
pub mod natsort;
mod natsort_tables;
pub mod pages;
pub mod ppocr;
pub mod py;
pub mod runtime;

pub use error::{Error, Result};
