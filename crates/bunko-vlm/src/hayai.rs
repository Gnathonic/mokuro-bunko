//! hayai-nova (`JustANormalTinkerer/hayai-ocr-v2.5-nova`, spec §5): the model's facts
//! (repos, special tokens, sizes), its export files and the NaFlex target size, shared
//! by every backend. The ONNX Runtime recognizer ([`HayaiNova`]) is behind the
//! `onnx-vlm` feature; 0.7 runs hayai-nova on libtorch (`bunko-torch`).

use std::path::{Path, PathBuf};

use crate::Precision;

#[cfg(feature = "onnx-vlm")]
mod onnx;
#[cfg(feature = "onnx-vlm")]
pub use onnx::HayaiNova;

pub const REPO: &str = "JustANormalTinkerer/hayai-ocr-v2.5-nova";
pub const REVISION: &str = "e46d79138499600564f810d44ab6bdea7230dee1";
pub const VISION_REPO: &str = "google/siglip2-base-patch16-naflex";
pub const VISION_REVISION: &str = "b53b807d3a2d5e2b3911292f2d69e5341cdc064c";

/// Crops per generation call, in input order (`HAYAI_NOVA_BATCH`).
pub const BATCH: usize = 16;
/// Generated tokens per crop at most (`HAYAI_NOVA_MAX_NEW_TOKENS`).
pub const MAX_NEW_TOKENS: usize = 96;
/// Default `max_num_patches` (`--patches`).
pub const DEFAULT_PATCH_BUDGET: usize = 512;

/// NaFlex patch side, in pixels.
pub const PATCH: usize = 16;
/// Values per flattened patch (16 x 16 x RGB).
pub const PATCH_DIM: usize = PATCH * PATCH * 3;
pub const BOS: u32 = 16001;
pub const EOS: u32 = 16002;
pub const PAD: u32 = 16000;
/// The additive mask value of the exported graphs.
pub const NEG: f32 = -1e9;
/// RoPE width per axis (two axes for image tokens, interleaved pairs).
pub const D_AXIS: usize = 32;
/// Decoder KV heads and head size.
pub const KV_HEADS: usize = 2;
pub const HEAD_DIM: usize = 64;

/// Files of one hayai-nova export.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HayaiAssets {
    pub vision: PathBuf,
    pub decoder: PathBuf,
    /// f32 (256, 768) SigLIP2 position table.
    pub pos_table: PathBuf,
    /// f32 (16004, 512) decoder input embeddings.
    pub token_embeddings: PathBuf,
    /// The model repo's `tokenizer.json`.
    pub tokenizer: PathBuf,
}

impl HayaiAssets {
    /// The spike's layout: `<dir>/nova_{vision,decoder}[_fp16].onnx`, `<dir>/pos_table.npy`,
    /// `<dir>/token_embeddings.npy`.
    pub fn in_dir(dir: &Path, tokenizer: &Path, precision: Precision) -> Self {
        let sfx = match precision {
            Precision::Fp32 => "",
            Precision::Fp16 => "_fp16",
            // There is no bf16 ONNX export: this names a file that does not exist.
            Precision::Bf16 => "_bf16",
        };
        Self {
            vision: dir.join(format!("nova_vision{sfx}.onnx")),
            decoder: dir.join(format!("nova_decoder{sfx}.onnx")),
            pos_table: dir.join("pos_table.npy"),
            token_embeddings: dir.join("token_embeddings.npy"),
            tokenizer: tokenizer.to_path_buf(),
        }
    }
}

/// `1 / 10000^(arange(0, 32, 2, f32) / 32)`, all f32 like the numpy host.
pub fn freqs() -> [f32; D_AXIS / 2] {
    std::array::from_fn(|i| 1.0 / 10000f32.powf((2 * i) as f32 / D_AXIS as f32))
}

/// NaFlex target size for a patch budget (`size_for_budget`, spec §5.3).
pub fn size_for_budget(h: usize, w: usize, budget: usize) -> (usize, usize) {
    let scaled = |s: f64, n: usize| -> usize {
        ((n as f64 * s / PATCH as f64).ceil() * PATCH as f64).max(PATCH as f64) as usize
    };
    let (mut lo, mut hi) = (1e-6f64, 100.0f64);
    while hi - lo >= 1e-5 {
        let s = (lo + hi) / 2.0;
        if (scaled(s, h) / PATCH) * (scaled(s, w) / PATCH) <= budget {
            lo = s;
        } else {
            hi = s;
        }
    }
    (scaled(lo, h), scaled(lo, w))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_sizes() {
        // a 64x716 vertical strip at 512 patches
        let (th, tw) = size_for_budget(716, 64, 512);
        assert_eq!(th % 16, 0);
        assert_eq!(tw % 16, 0);
        assert!((th / 16) * (tw / 16) <= 512);
        // tiny crops are scaled up to at least one patch
        assert_eq!(size_for_budget(3, 3, 512).0 % 16, 0);
        let (a, b) = size_for_budget(64, 64, 256);
        assert_eq!((a, b), (256, 256));
    }
}
