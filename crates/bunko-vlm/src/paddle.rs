//! paddle-manga (PaddleOCR-VL-1.6 + `sorryhyun/paddleocr-vl-1.6-manga-lora`, merged; spec
//! §6): the model's facts (repos, prompt, sizes), its export files and `smart_resize`,
//! shared by every backend. The ONNX Runtime recognizer ([`PaddleManga`]) is behind the
//! `onnx-vlm` feature; 0.7 runs paddle-manga on libtorch (`bunko-torch`).

use std::path::{Path, PathBuf};

use crate::VlmError;
use crate::pyfmt::round_half_even;

#[cfg(feature = "onnx-vlm")]
mod onnx;
#[cfg(feature = "onnx-vlm")]
pub use onnx::PaddleManga;

pub const BASE_REPO: &str = "PaddlePaddle/PaddleOCR-VL-1.6";
pub const BASE_REVISION: &str = "c5630abae1d940eafe0697512a0325494b02ab42";
pub const LORA_REPO: &str = "sorryhyun/paddleocr-vl-1.6-manga-lora";
pub const LORA_REVISION: &str = "26292839d1469c14212a12a1e01b5b1fe01bff15";

/// Crops per generation batch (`PADDLE_BATCH`).
pub const BATCH: usize = 12;
/// Token cap when the caller gives none (`DEFAULT_MAX_NEW_TOKENS`).
pub const DEFAULT_MAX_NEW_TOKENS: u32 = 64;

/// Vision patch side, in pixels.
pub const PATCH: usize = 14;
/// `smart_resize` rounds sides to multiples of this (patch x merge).
pub const FACTOR: usize = 28;
pub const MIN_PIXELS: usize = 112_896;
pub const MAX_PIXELS: usize = 1_003_520;
/// Side of the learned vision position grid.
pub const SIDE: usize = 27;
/// Half the vision head size (72 / 2).
pub const V_HALF: usize = 36;
/// Text head size.
pub const T_HD: usize = 128;
/// M-RoPE sections (t, h, w).
pub const MROPE: [usize; 3] = [16, 24, 24];
pub const EOS: u32 = 2;
pub const IMAGE_TOKEN: i64 = 100_295;
/// `<|begin_of_sentence|>User: <|IMAGE_START|>` (spec §6.6).
pub const PREFIX: [i64; 5] = [100_273, 2969, 93963, 93919, 101_305];
/// `<|IMAGE_END|>OCR:\nAssistant:\n`.
pub const SUFFIX: [i64; 8] = [101_306, 93972, 2497, 93963, 23, 92267, 93963, 23];
/// Decoder KV heads.
pub const KV_HEADS: usize = 2;

/// Files of one paddle-manga export.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PaddleAssets {
    pub vision: PathBuf,
    pub decoder: PathBuf,
    /// f16 (103424, 1024) input token embeddings.
    pub embed: PathBuf,
    /// The base repo's `tokenizer.json`.
    pub tokenizer: PathBuf,
}

impl PaddleAssets {
    /// The spike's layout: `<dir>/{vision,decoder}.onnx(+.data)`, `<dir>/embed.npy`, where
    /// `dir` is `onnx_float32` or `onnx_float16`.
    pub fn in_dir(dir: &Path, tokenizer: &Path) -> Self {
        Self {
            vision: dir.join("vision.onnx"),
            decoder: dir.join("decoder.onnx"),
            embed: dir.join("embed.npy"),
            tokenizer: tokenizer.to_path_buf(),
        }
    }
}

/// `smart_resize` (transformers 5, `preprocessor_config.json` values): the crop's size
/// rounded to multiples of 28 within the pixel budget. Errors past aspect ratio 200.
pub fn smart_resize(h: usize, w: usize) -> Result<(usize, usize), VlmError> {
    let (mut h, mut w) = (h.max(1) as f64, w.max(1) as f64);
    let f = FACTOR as f64;
    if h < f {
        w = round_half_even(w * f / h) as f64;
        h = f;
    }
    if w < f {
        h = round_half_even(h * f / w) as f64;
        w = f;
    }
    if h.max(w) / h.min(w) > 200.0 {
        return Err(VlmError::Crop(format!(
            "absolute aspect ratio must be smaller than 200, got {}",
            h.max(w) / h.min(w)
        )));
    }
    let mut hb = round_half_even(h / f) as f64 * f;
    let mut wb = round_half_even(w / f) as f64 * f;
    if hb * wb > MAX_PIXELS as f64 {
        let beta = (h * w / MAX_PIXELS as f64).sqrt();
        hb = f.max((h / beta / f).floor() * f);
        wb = f.max((w / beta / f).floor() * f);
    } else if hb * wb < MIN_PIXELS as f64 {
        let beta = (MIN_PIXELS as f64 / (h * w)).sqrt();
        hb = (h * beta / f).ceil() * f;
        wb = (w * beta / f).ceil() * f;
    }
    Ok((hb as usize, wb as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_resize_cases() {
        // tiny crop: scaled up to the pixel floor
        let (h, w) = smart_resize(40, 300).unwrap();
        assert_eq!((h % 28, w % 28), (0, 0));
        assert!(h * w >= MIN_PIXELS);
        // huge crop: scaled down under the ceiling
        let (h, w) = smart_resize(3000, 2000).unwrap();
        assert!(h * w <= MAX_PIXELS);
        // a 10 px high line is first widened to 28 px high
        let (h, w) = smart_resize(10, 500).unwrap();
        assert_eq!((h % 28, w % 28), (0, 0));
        assert!(smart_resize(28, 28 * 201).is_err());
    }

    #[test]
    fn prompt_matches_spike_ids() {
        // prompt_ids.npy = [5, prefix..., suffix...]
        let all: Vec<i64> = PREFIX.iter().chain(SUFFIX.iter()).copied().collect();
        assert_eq!(
            all,
            vec![
                100273, 2969, 93963, 93919, 101305, 101306, 93972, 2497, 93963, 23, 92267, 93963,
                23
            ]
        );
    }
}
