//! The two vision-language line recognizers of mokuro-bunko 0.7: **hayai-nova** (SigLIP2
//! NaFlex + small decoder) and **paddle-manga** (PaddleOCR-VL-1.6 with the manga LoRA
//! merged). Spec: `docs/rust-port/spec/ocr-recognizers.md`.
//!
//! What every backend shares (always built, no ONNX Runtime):
//! - [`crop`]: the line crops each recognizer reads (§3), from a [`Bgr`] page and a quad.
//! - [`Recognizer`], [`RecognizerInfo`], [`CropSet`], [`read_flat`]: the recognizer seam.
//! - the model facts and host helpers: [`hayai`], [`paddle`], [`detok`], [`npy`],
//!   [`resample`], [`pyfmt`].
//!
//! 0.7 runs the recognizers on libtorch (`bunko-torch`, loaded at run time by
//! `bunko-engines`). The ONNX Runtime recognizers are kept behind the non-default
//! **`onnx-vlm`** feature (deferred, not built for release):
//! - [`HayaiNova`], [`PaddleManga`] (§5, §6) behind the [`Recognizer`] trait.
//! - [`runtime`]: session construction ([`SessionFactory`]) and the shared-session runner.
//! - [`RecognizerCache`]: one recognizer per (engine, assets, device, precision), shared by
//!   any number of worker threads.

pub mod crop;
pub mod detok;
pub mod device;
pub mod hayai;
pub mod image;
pub mod npy;
pub mod paddle;
pub mod pyfmt;
pub mod resample;
#[cfg(feature = "onnx-vlm")]
pub mod runtime;
pub mod warp;

#[cfg(feature = "onnx-vlm")]
mod cache;
#[cfg(feature = "onnx-vlm")]
mod tensor;

use std::fmt;

#[cfg(feature = "onnx-vlm")]
pub use cache::{EngineKey, RecognizerCache};
pub use crop::Quad;
pub use device::Device;
pub use hayai::HayaiAssets;
#[cfg(feature = "onnx-vlm")]
pub use hayai::HayaiNova;
pub use image::{Bgr, Rgb};
pub use paddle::PaddleAssets;
#[cfg(feature = "onnx-vlm")]
pub use paddle::PaddleManga;
#[cfg(feature = "onnx-vlm")]
pub use runtime::{
    OrtSessionFactory, Provider, SessionFactory, SessionOptions, SharedSession, Sharing,
};

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum VlmError {
    #[error("{0}: {1}")]
    Io(String, #[source] std::io::Error),
    /// A model asset (graph, table, tokenizer) is missing or malformed.
    #[error("model asset: {0}")]
    Asset(String),
    /// The inference runtime refused (load, run, provider).
    #[error("runtime: {0}")]
    Runtime(String),
    /// Bad caller input (lengths, devices).
    #[error("{0}")]
    Config(String),
    /// A crop the recognizer cannot read (paddle: aspect ratio over 200).
    #[error("{0}")]
    Crop(String),
}

/// Numeric format of a recognizer: which exported graph files are loaded (spec §7.2).
/// The ONNX exports come in fp32 and fp16; the libtorch packages also in bf16 (0.5.2's
/// torch ran all three).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Precision {
    Fp32,
    Bf16,
    Fp16,
}

impl Precision {
    pub fn as_str(self) -> &'static str {
        match self {
            Precision::Fp32 => "fp32",
            Precision::Bf16 => "bf16",
            Precision::Fp16 => "fp16",
        }
    }
}

impl fmt::Display for Precision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Precision {
    type Err = VlmError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fp32" => Ok(Precision::Fp32),
            "bf16" => Ok(Precision::Bf16),
            "fp16" => Ok(Precision::Fp16),
            _ => Err(VlmError::Config(format!(
                "precision not available here: {s}"
            ))),
        }
    }
}

/// The crops of one detected line. Its text is the concatenation of the crops' texts
/// (hayai-nova chunks long lines; paddle-manga always gives one crop).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CropSet {
    pub crops: Vec<Rgb>,
}

impl CropSet {
    pub fn one(crop: Rgb) -> Self {
        Self { crops: vec![crop] }
    }
}

/// What a recognizer is, for the sidecar's `ocr_engine` and for scheduling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecognizerInfo {
    /// Engine id: `hayai-nova` or `paddle-manga`.
    pub engine: &'static str,
    /// The `ocr_engine.recognizer` repo.
    pub recognizer: &'static str,
    /// Source weights, `repo → commit` (`ocr_engine.weights`).
    pub repos: Vec<(&'static str, &'static str)>,
    pub precision: Precision,
    pub device: Device,
    /// Whether per-line token caps are honoured (paddle-manga).
    pub token_caps: bool,
    /// Whether [`Recognizer::second_crop`] gives crops (paddle-manga), i.e. whether a
    /// caller must keep the page image after the first-read crops are cut.
    pub second_read: bool,
    /// Generated-token limit when no cap is given.
    pub default_max_tokens: u32,
    /// Crops per generation batch.
    pub batch: usize,
    /// hayai-nova's `max_num_patches` (`ocr_engine.patch_budget`).
    pub patch_budget: Option<u32>,
}

/// A line recognizer. Implementations are `Send + Sync`: worker threads share one.
pub trait Recognizer: Send + Sync {
    fn info(&self) -> &RecognizerInfo;

    /// The first-read crops of one line (spec §3.1).
    fn crop(&self, page: &Bgr, quad: &Quad, vertical: bool) -> CropSet;

    /// The wider second-read crop, for recognizers that take a second read (paddle-manga).
    fn second_crop(&self, page: &Bgr, quad: &Quad, vertical: bool) -> Option<CropSet>;

    /// Reads lines. `caps[i]` caps the generated tokens of every crop of line `i`
    /// (honoured only when [`RecognizerInfo::token_caps`]). Returns one string per line,
    /// in input order: each crop's decoded text with Python `strip()`, no NFKC, concatenated.
    fn read(&self, lines: &[CropSet], caps: Option<&[u32]>) -> Result<Vec<String>, VlmError>;
}

/// Flattens line crop sets to `(crop, owner line)` and joins per-crop texts back per line:
/// `read_crops` gets every crop with its token cap and returns one text per crop.
pub fn read_flat(
    lines: &[CropSet],
    caps: Option<&[u32]>,
    default_cap: u32,
    read_crops: impl FnOnce(&[&Rgb], &[u32]) -> Result<Vec<String>, VlmError>,
) -> Result<Vec<String>, VlmError> {
    if let Some(c) = caps
        && c.len() != lines.len()
    {
        return Err(VlmError::Config(format!(
            "{} token caps for {} lines",
            c.len(),
            lines.len()
        )));
    }
    let mut crops = Vec::new();
    let mut owner = Vec::new();
    let mut crop_caps = Vec::new();
    for (i, set) in lines.iter().enumerate() {
        for c in &set.crops {
            crops.push(c);
            owner.push(i);
            crop_caps.push(caps.map_or(default_cap, |c| c[i]));
        }
    }
    let texts = if crops.is_empty() {
        Vec::new()
    } else {
        read_crops(&crops, &crop_caps)?
    };
    if texts.len() != crops.len() {
        return Err(VlmError::Runtime(format!(
            "recognizer returned {} strings for {} crops",
            texts.len(),
            crops.len()
        )));
    }
    let mut out = vec![String::new(); lines.len()];
    for (k, t) in owner.into_iter().zip(texts) {
        out[k].push_str(&t);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send_sync<T: Send + Sync>() {}

    #[test]
    fn recognizers_are_shareable() {
        #[cfg(feature = "onnx-vlm")]
        {
            send_sync::<HayaiNova>();
            send_sync::<PaddleManga>();
            send_sync::<SharedSession>();
            send_sync::<RecognizerCache>();
        }
        send_sync::<std::sync::Arc<dyn Recognizer>>();
    }

    #[test]
    fn precision_names() {
        for p in [Precision::Fp32, Precision::Bf16, Precision::Fp16] {
            assert_eq!(p.as_str().parse::<Precision>().unwrap(), p);
        }
        assert!("int8".parse::<Precision>().is_err());
    }

    #[test]
    fn read_flat_joins_chunks_and_checks_caps() {
        let px = Rgb::new(1, 1);
        let lines = vec![
            CropSet {
                crops: vec![px.clone(), px.clone()],
            },
            CropSet::default(),
            CropSet::one(px),
        ];
        let out = read_flat(&lines, Some(&[5, 6, 7]), 64, |crops, caps| {
            assert_eq!(caps, &[5, 5, 7]);
            Ok((0..crops.len()).map(|i| format!("t{i}")).collect())
        })
        .unwrap();
        assert_eq!(out, vec!["t0t1", "", "t2"]);
        assert!(read_flat(&lines, Some(&[1]), 64, |_, _| Ok(vec![])).is_err());
        assert!(read_flat(&lines, None, 64, |_, _| Ok(vec![])).is_err());
    }
}
