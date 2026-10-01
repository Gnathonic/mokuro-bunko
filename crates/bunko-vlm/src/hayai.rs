//! hayai-nova (`JustANormalTinkerer/hayai-ocr-v2.5-nova`) on ONNX Runtime (spec §5).
//!
//! Host side of the exported graphs: NaFlex preprocessing (Pillow bilinear to the patch
//! budget), the position-table resize, projector gather indices, interleaved-pair RoPE
//! tables, masks, and the greedy decode loop of `nova_generate` (ER:1644) with the KV
//! cache passed from one step to the next as ONNX Runtime values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ort::value::DynValue;

use crate::crop::{Quad, hayai_line_crops};
use crate::detok::Detokenizer;
use crate::image::{Bgr, Rgb};
use crate::npy::Npy;
use crate::pyfmt::py_strip;
use crate::resample::{Filter, aa_weights, resize};
use crate::runtime::{SessionFactory, SessionOptions, SharedSession};
use crate::tensor::{FloatKind, argmax_rows, floats, i64s, to_f32};
use crate::{CropSet, Precision, Recognizer, RecognizerInfo, VlmError, read_flat};

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

const PATCH: usize = 16;
const PATCH_DIM: usize = PATCH * PATCH * 3;
const BOS: u32 = 16001;
const EOS: u32 = 16002;
const PAD: u32 = 16000;
const NEG: f32 = -1e9;
const D_AXIS: usize = 32;
const KV_HEADS: usize = 2;
const HEAD_DIM: usize = 64;

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

type PosCache = Mutex<HashMap<(usize, usize), Arc<Vec<f32>>>>;

/// Which decoder input a graph slot is.
#[derive(Clone, Copy, Debug)]
enum DecIn {
    Embeds,
    Mask,
    Cos,
    Sin,
    /// past KV, fed from decoder output `.0`
    Past(usize),
}

/// The hayai-nova recognizer. One instance (two sessions) serves every thread.
pub struct HayaiNova {
    vision: SharedSession,
    decoder: SharedSession,
    vis_in: Vec<&'static str>,
    vis_kind: FloatKind,
    dec_kind: FloatKind,
    dec_in: Vec<DecIn>,
    n_kv: usize,
    pos_grid: Vec<f32>,
    grid: usize,
    pos_dim: usize,
    pos_cache: PosCache,
    emb: Vec<f32>,
    d_model: usize,
    vocab: usize,
    detok: Detokenizer,
    budget: usize,
    text_cos: Vec<f32>,
    text_sin: Vec<f32>,
    info: RecognizerInfo,
}

impl std::fmt::Debug for HayaiNova {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HayaiNova")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

fn freqs() -> [f32; D_AXIS / 2] {
    // 1 / 10000^(arange(0, 32, 2, f32) / 32), all f32 like the numpy host
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

impl HayaiNova {
    /// Loads both graphs and the host tables. `budget` is `max_num_patches` (256, 384, 512).
    pub fn load(
        factory: &dyn SessionFactory,
        assets: &HayaiAssets,
        opts: &SessionOptions,
        budget: usize,
    ) -> Result<Self, VlmError> {
        if !(16..=4096).contains(&budget) {
            return Err(VlmError::Config(format!(
                "patch budget {budget} out of range"
            )));
        }
        let vision = factory.open(&assets.vision, opts)?;
        let decoder = factory.open(&assets.decoder, opts)?;

        let mut vis_in = Vec::new();
        for n in vision.input_names() {
            vis_in.push(match n {
                "pixel_values" => "pixel_values",
                "pixel_mask" => "pixel_mask",
                "pos" => "pos",
                "gather_idx" => "gather_idx",
                "tok_valid" => "tok_valid",
                other => {
                    return Err(VlmError::Asset(format!(
                        "hayai vision: unknown input {other}"
                    )));
                }
            });
        }
        let vis_kind = FloatKind::of(vision.input_type(0), "hayai vision input")?;

        let outs: Vec<String> = decoder.output_names().map(str::to_owned).collect();
        let mut dec_in = Vec::new();
        for n in decoder.input_names() {
            dec_in.push(match n {
                "embeds" => DecIn::Embeds,
                "mask" => DecIn::Mask,
                "cos" => DecIn::Cos,
                "sin" => DecIn::Sin,
                p if p.starts_with("past_") => {
                    let want = p.replacen("past_", "present_", 1);
                    let j = outs.iter().position(|o| *o == want).ok_or_else(|| {
                        VlmError::Asset(format!("hayai decoder: no output {want}"))
                    })?;
                    DecIn::Past(j)
                }
                other => {
                    return Err(VlmError::Asset(format!(
                        "hayai decoder: unknown input {other}"
                    )));
                }
            });
        }
        let n_kv = dec_in
            .iter()
            .filter(|d| matches!(d, DecIn::Past(_)))
            .count();
        let dec_kind = FloatKind::of(decoder.input_type(0), "hayai decoder input")?;
        if outs.first().map(String::as_str) != Some("logits") {
            return Err(VlmError::Asset(
                "hayai decoder: first output is not logits".into(),
            ));
        }

        let pos = Npy::load(&assets.pos_table)?;
        let (np, pos_dim) = match pos.shape[..] {
            [a, b] => (a, b),
            _ => return Err(VlmError::Asset("pos_table is not 2-D".into())),
        };
        let grid = (np as f64).sqrt() as usize;
        if grid * grid != np {
            return Err(VlmError::Asset(format!(
                "pos_table has {np} rows, not a square"
            )));
        }
        let pos_grid = pos.into_f32()?;
        let emb = Npy::load(&assets.token_embeddings)?;
        let (vocab, d_model) = match emb.shape[..] {
            [a, b] => (a, b),
            _ => return Err(VlmError::Asset("token_embeddings is not 2-D".into())),
        };
        let emb = emb.into_f32()?;
        let detok = Detokenizer::from_file(&assets.tokenizer)?;

        let f = freqs();
        let mut text_cos = Vec::with_capacity((MAX_NEW_TOKENS + 1) * D_AXIS);
        let mut text_sin = Vec::with_capacity((MAX_NEW_TOKENS + 1) * D_AXIS);
        for n in 0..=MAX_NEW_TOKENS {
            for _half in 0..2 {
                for fi in f {
                    let a = n as f32 * fi;
                    text_cos.push(a.cos());
                    text_sin.push(a.sin());
                }
            }
        }

        let precision = match vis_kind {
            FloatKind::F32 => Precision::Fp32,
            FloatKind::F16 => Precision::Fp16,
        };
        let info = RecognizerInfo {
            engine: "hayai-nova",
            recognizer: REPO,
            repos: vec![(REPO, REVISION), (VISION_REPO, VISION_REVISION)],
            precision,
            device: opts.device,
            token_caps: false,
            default_max_tokens: MAX_NEW_TOKENS as u32,
            batch: BATCH,
            patch_budget: Some(budget as u32),
        };
        tracing::info!(
            device = %opts.device, %precision, budget, vision_inputs = ?vis_in, kv = n_kv,
            "hayai-nova loaded"
        );
        Ok(Self {
            vision,
            decoder,
            vis_in,
            vis_kind,
            dec_kind,
            dec_in,
            n_kv,
            pos_grid,
            grid,
            pos_dim,
            pos_cache: Mutex::new(HashMap::new()),
            emb,
            d_model,
            vocab,
            detok,
            budget,
            text_cos,
            text_sin,
            info,
        })
    }

    /// The resized position table for an (hp, wp) patch grid, `(hp*wp, 768)` (spec §5.4).
    fn pos_for(&self, hp: usize, wp: usize) -> Arc<Vec<f32>> {
        if let Some(v) = self
            .pos_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&(hp, wp))
        {
            return Arc::clone(v);
        }
        let (g, d) = (self.grid, self.pos_dim);
        let wy = aa_weights(g, hp);
        let wx = aa_weights(g, wp);
        let mut r = vec![0f32; hp * wp * d];
        for h in 0..hp {
            for w in 0..wp {
                let out = &mut r[(h * wp + w) * d..(h * wp + w + 1) * d];
                for y in 0..g {
                    let a = wy[h * g + y];
                    if a == 0.0 {
                        continue;
                    }
                    for x in 0..g {
                        let b = wx[w * g + x];
                        if b == 0.0 {
                            continue;
                        }
                        let row = &self.pos_grid[(y * g + x) * d..(y * g + x + 1) * d];
                        for (o, &gv) in out.iter_mut().zip(row) {
                            *o += a * gv * b;
                        }
                    }
                }
            }
        }
        let r = Arc::new(r);
        self.pos_cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((hp, wp), Arc::clone(&r));
        r
    }

    /// One generation call over up to [`BATCH`] crops; texts stripped, in order.
    fn generate(&self, crops: &[&Rgb]) -> Result<Vec<String>, VlmError> {
        let b = crops.len();
        let p = self.budget;
        let dm = self.d_model;
        let pd = self.pos_dim;

        // §5.3 preprocessing
        let mut pv = vec![0f32; b * p * PATCH_DIM];
        let mut pmask = vec![0f32; b * p];
        let mut shapes = Vec::with_capacity(b);
        let inv255 = (1.0f64 / 255.0) as f32;
        for (i, crop) in crops.iter().enumerate() {
            let (th, tw) = size_for_budget(crop.height, crop.width, p);
            let img = resize(crop, tw, th, Filter::Bilinear);
            let (hp, wp) = (th / PATCH, tw / PATCH);
            for py in 0..hp {
                for px in 0..wp {
                    let patch = py * wp + px;
                    let base = (i * p + patch) * PATCH_DIM;
                    for iy in 0..PATCH {
                        for ix in 0..PATCH {
                            let s = ((py * PATCH + iy) * tw + px * PATCH + ix) * 3;
                            for c in 0..3 {
                                let v = f32::from(img.data[s + c]);
                                pv[base + (iy * PATCH + ix) * 3 + c] = (v * inv255 - 0.5) / 0.5;
                            }
                        }
                    }
                }
            }
            pmask[i * p..i * p + hp * wp].fill(1.0);
            shapes.push((hp, wp));
        }

        // §5.4/§5.5 position rows and projector gather
        let mut pos = vec![0f32; b * p * pd];
        let mut outs = Vec::with_capacity(b);
        for (i, &(hp, wp)) in shapes.iter().enumerate() {
            let r = self.pos_for(hp, wp);
            let n = hp * wp;
            let dst = &mut pos[i * p * pd..(i + 1) * p * pd];
            dst[..n * pd].copy_from_slice(&r);
            for k in n..p {
                dst[k * pd..(k + 1) * pd].copy_from_slice(&r[..pd]);
            }
            outs.push((hp, wp, hp.div_ceil(2), wp.div_ceil(2)));
        }
        let m = outs.iter().map(|o| o.2 * o.3).max().unwrap_or(0);
        let mut gather = vec![0i64; b * m * 4];
        let mut tok_valid = vec![0f32; b * m];
        let mut valid = vec![0usize; b];
        for (i, &(hp, wp, ho, wo)) in outs.iter().enumerate() {
            for t in 0..ho * wo {
                let (y, x) = (t / wo, t % wo);
                for (k, (dy, dx)) in [(0, 0), (0, 1), (1, 0), (1, 1)].into_iter().enumerate() {
                    gather[(i * m + t) * 4 + k] =
                        ((2 * y + dy).min(hp - 1) * wp + (2 * x + dx).min(wp - 1)) as i64;
                }
                tok_valid[i * m + t] = 1.0;
            }
            valid[i] = ho * wo;
        }

        // vision graph
        let vk = self.vis_kind;
        let mut feeds: Vec<DynValue> = Vec::with_capacity(5);
        for name in &self.vis_in {
            feeds.push(match *name {
                "pixel_values" => floats(vk, &[b, p, PATCH_DIM], std::mem::take(&mut pv))?,
                "pixel_mask" => floats(vk, &[b, p], std::mem::take(&mut pmask))?,
                "pos" => floats(vk, &[b, p, pd], std::mem::take(&mut pos))?,
                "gather_idx" => i64s(&[b, m, 4], std::mem::take(&mut gather))?,
                _ => floats(vk, &[b, m], std::mem::take(&mut tok_valid))?,
            });
        }
        let refs: Vec<&DynValue> = feeds.iter().collect();
        let vis = to_f32(&self.vision.run(&refs, Vec::new())?[0])?;
        drop(feeds);
        if vis.len() != b * m * dm {
            return Err(VlmError::Runtime(format!(
                "vision returned {} values, expected {}",
                vis.len(),
                b * m * dm
            )));
        }

        // §5.7/§5.8 prefill inputs
        let l0 = m + 1;
        let mut x = vec![0f32; b * l0 * dm];
        let bos = &self.emb[BOS as usize * dm..(BOS as usize + 1) * dm];
        for i in 0..b {
            x[i * l0 * dm..(i * l0 + m) * dm].copy_from_slice(&vis[i * m * dm..(i + 1) * m * dm]);
            x[(i * l0 + m) * dm..(i * l0 + m + 1) * dm].copy_from_slice(bos);
        }
        let key_bias: Vec<f32> = (0..b)
            .flat_map(|i| (0..m).map(move |j| (i, j)))
            .map(|(i, j)| if j >= valid[i] { NEG } else { 0.0 })
            .collect();
        let mut mask = vec![0f32; b * l0 * l0];
        for i in 0..b {
            let mi = &mut mask[i * l0 * l0..(i + 1) * l0 * l0];
            for q in 0..l0 {
                if q < m {
                    mi[q * l0 + m] = NEG;
                }
                for j in 0..m {
                    mi[q * l0 + j] += key_bias[i * m + j];
                }
            }
        }
        let f = freqs();
        let mut cos = vec![1f32; b * l0 * D_AXIS];
        let mut sin = vec![0f32; b * l0 * D_AXIS];
        for (i, &(_, _, ho, wo)) in outs.iter().enumerate() {
            for t in 0..(ho * wo).min(m) {
                let (y, xx) = ((t / wo) as f32, (t % wo) as f32);
                let row = (i * l0 + t) * D_AXIS;
                for (k, &fk) in f.iter().enumerate() {
                    let (ay, ax) = (y * fk, xx * fk);
                    cos[row + k] = ay.cos();
                    sin[row + k] = ay.sin();
                    cos[row + 16 + k] = ax.cos();
                    sin[row + 16 + k] = ax.sin();
                }
            }
            let row = (i * l0 + m) * D_AXIS;
            cos[row..row + D_AXIS].copy_from_slice(&self.text_cos[..D_AXIS]);
            sin[row..row + D_AXIS].copy_from_slice(&self.text_sin[..D_AXIS]);
        }

        let dk = self.dec_kind;
        let mut past: Vec<DynValue> = Vec::with_capacity(self.n_kv);
        for _ in 0..self.n_kv {
            past.push(floats(dk, &[b, KV_HEADS, 0, HEAD_DIM], Vec::new())?);
        }
        let alloc = self.decoder.device_allocator()?;
        let mut seqlen = 0usize; // past length
        let mut step_in = (
            floats(dk, &[b, l0, dm], x)?,
            floats(dk, &[b, 1, l0, l0], mask)?,
            floats(dk, &[b, l0, D_AXIS], cos)?,
            floats(dk, &[b, l0, D_AXIS], sin)?,
        );
        let mut s = l0;
        let run_step = |inp: &(DynValue, DynValue, DynValue, DynValue),
                        past: &mut Vec<DynValue>,
                        seqlen: usize,
                        s: usize|
         -> Result<Vec<u32>, VlmError> {
            let feeds: Vec<&DynValue> = self
                .dec_in
                .iter()
                .map(|d| match d {
                    DecIn::Embeds => &inp.0,
                    DecIn::Mask => &inp.1,
                    DecIn::Cos => &inp.2,
                    DecIn::Sin => &inp.3,
                    DecIn::Past(j) => &past[*j - 1],
                })
                .collect();
            let mut pre: Vec<Option<DynValue>> = vec![None];
            if let Some(a) = &alloc {
                for _ in 0..self.n_kv {
                    pre.push(Some(crate::tensor::alloc(
                        a,
                        dk,
                        &[b, KV_HEADS, seqlen + s, HEAD_DIM],
                    )?));
                }
            }
            let mut out = self.decoder.run(&feeds, pre)?;
            drop(feeds);
            let logits = to_f32(&out[0])?;
            if logits.len() != b * self.vocab {
                return Err(VlmError::Runtime(format!(
                    "decoder returned {} logits for {b} rows of {}",
                    logits.len(),
                    self.vocab
                )));
            }
            *past = out.split_off(1);
            Ok(argmax_rows(&logits, self.vocab))
        };

        // the greedy loop (§5.9)
        let mut nxt = run_step(&step_in, &mut past, seqlen, s)?;
        seqlen += s;
        let mut toks: Vec<Vec<u32>> = nxt.iter().map(|&t| vec![t]).collect();
        let mut live: Vec<bool> = nxt.iter().map(|&t| t != EOS && t != PAD).collect();
        s = 1;
        for step in 1..MAX_NEW_TOKENS {
            if !live.iter().any(|&l| l) {
                break;
            }
            let mut xs = Vec::with_capacity(b * dm);
            for &t in &nxt {
                xs.extend_from_slice(&self.emb[t as usize * dm..(t as usize + 1) * dm]);
            }
            let mut m_step = vec![0f32; b * (seqlen + 1)];
            for i in 0..b {
                m_step[i * (seqlen + 1)..i * (seqlen + 1) + m]
                    .copy_from_slice(&key_bias[i * m..(i + 1) * m]);
            }
            let row = step * D_AXIS;
            let cs: Vec<f32> = (0..b)
                .flat_map(|_| self.text_cos[row..row + D_AXIS].iter().copied())
                .collect();
            let sn: Vec<f32> = (0..b)
                .flat_map(|_| self.text_sin[row..row + D_AXIS].iter().copied())
                .collect();
            step_in = (
                floats(dk, &[b, 1, dm], xs)?,
                floats(dk, &[b, 1, 1, seqlen + 1], m_step)?,
                floats(dk, &[b, 1, D_AXIS], cs)?,
                floats(dk, &[b, 1, D_AXIS], sn)?,
            );
            let got = run_step(&step_in, &mut past, seqlen, s)?;
            seqlen += 1;
            for i in 0..b {
                nxt[i] = if live[i] { got[i] } else { PAD };
                if live[i] {
                    toks[i].push(nxt[i]);
                }
                live[i] = live[i] && nxt[i] != EOS && nxt[i] != PAD;
            }
        }
        Ok(toks
            .into_iter()
            .map(|t| {
                let ids: Vec<u32> = t.into_iter().filter(|&t| t != EOS && t != PAD).collect();
                py_strip(&self.detok.decode(&ids)).to_owned()
            })
            .collect())
    }

    /// Reads crops (not line sets) in consecutive batches of [`BATCH`], input order.
    pub fn read_crops(&self, crops: &[&Rgb]) -> Result<Vec<String>, VlmError> {
        let mut out = Vec::with_capacity(crops.len());
        for chunk in crops.chunks(BATCH) {
            out.extend(self.generate(chunk)?);
        }
        Ok(out)
    }
}

impl Recognizer for HayaiNova {
    fn info(&self) -> &RecognizerInfo {
        &self.info
    }

    fn crop(&self, page: &Bgr, quad: &Quad, vertical: bool) -> CropSet {
        CropSet {
            crops: hayai_line_crops(page, quad, vertical),
        }
    }

    fn second_crop(&self, _page: &Bgr, _quad: &Quad, _vertical: bool) -> Option<CropSet> {
        None
    }

    fn read(&self, lines: &[CropSet], caps: Option<&[u32]>) -> Result<Vec<String>, VlmError> {
        // hayai-nova takes no token caps (ER:1591): every crop may use all 96 tokens.
        let _ = caps;
        read_flat(lines, None, MAX_NEW_TOKENS as u32, |crops, _| {
            self.read_crops(crops)
        })
    }
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
