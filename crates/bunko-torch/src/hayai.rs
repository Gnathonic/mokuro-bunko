//! hayai-nova over three AOTInductor packages (vision, decoder prefill, decoder step with
//! the KV cache as inputs). Host preprocessing is bunko-vlm's ONNX recognizer's (NaFlex
//! resize, patchify, position-table resize, projector gather, RoPE tables, masks); the
//! KV cache, embeddings and argmax stay on the device. Graphs are traced from the 0.5.2
//! torch modules under `torch.autocast` (`tools/torch_export`), so every precision reads
//! what 0.5.2 read on the same device.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bunko_vlm::Rgb;
use bunko_vlm::detok::Detokenizer;
use bunko_vlm::hayai::{
    BATCH, BOS, D_AXIS, EOS, MAX_NEW_TOKENS, NEG, PAD, PATCH, PATCH_DIM, freqs, size_for_budget,
};
use bunko_vlm::npy::Npy;
use bunko_vlm::pyfmt::py_strip;
use bunko_vlm::resample::{Filter, aa_weights, resize};
use tch::{Kind, Tensor};

use crate::TorchError;
use crate::aoti::{Aoti, Placement, TPrec, dev_f32, dev_i64, host_i64};
use crate::package::{PackageSet, Weights, split_decoder_out};

const BOS_I: i64 = BOS as i64;
const EOS_I: i64 = EOS as i64;
const PAD_I: i64 = PAD as i64;

/// The patch budget the packages are compiled for (the vision graph's patch axis is
/// static).
pub const PACKAGE_BUDGET: usize = 512;

/// The host files of hayai-nova (the graphs come as a [`PackageSet`]).
pub struct HayaiFiles<'a> {
    pub pos_table: &'a Path,
    pub token_embeddings: &'a Path,
    pub tokenizer: &'a Path,
}

/// Resized position tables by (hp, wp) patch grid.
type PosCache = Mutex<HashMap<(usize, usize), Arc<Vec<f32>>>>;

pub struct TorchHayai {
    vision: Aoti,
    prefill: Aoti,
    step: Aoti,
    io: u32,
    // The packages' shared weights (bound in place: kept alive with them).
    _weights: Option<Weights>,
    dev: tch::Device,
    emb: Tensor,
    pos_grid: Vec<f32>,
    grid: usize,
    pos_dim: usize,
    pos_cache: PosCache,
    d_model: i64,
    detok: Detokenizer,
    budget: usize,
    text_cos: Tensor,
    text_sin: Tensor,
    gpu_lock: Mutex<()>,
}

// SAFETY: every tensor field is read-only after load and only used under `gpu_lock`
// (libtorch tensors are reference-counted handles; reads from several threads are fine).
unsafe impl Send for TorchHayai {}
unsafe impl Sync for TorchHayai {}

impl TorchHayai {
    pub fn load(
        pkgs: PackageSet,
        files: &HayaiFiles<'_>,
        _prec: TPrec,
        at: Placement,
        budget: usize,
    ) -> Result<Self, TorchError> {
        if budget != PACKAGE_BUDGET {
            return Err(TorchError::Arg(format!(
                "hayai-nova packages are compiled for patch budget {PACKAGE_BUDGET}, not {budget}"
            )));
        }
        let dev = at.dev;
        if let Some(sp) = pkgs.metadata("bunko.special") {
            let v: serde_json::Value = serde_json::from_str(&sp)
                .map_err(|e| TorchError::Load(format!("bad bunko.special: {e}")))?;
            if v["eos"] != EOS_I || v["pad"] != PAD_I {
                return Err(TorchError::Load(format!(
                    "the package's special tokens {sp} are not hayai-nova's"
                )));
            }
        }
        let PackageSet {
            vision,
            prefill,
            step,
            io,
            weights,
        } = pkgs;
        let pos = Npy::load(files.pos_table)?;
        let (np, pos_dim) = match pos.shape[..] {
            [a, b] => (a, b),
            _ => return Err(TorchError::Load("pos_table is not 2-D".into())),
        };
        let grid = (np as f64).sqrt() as usize;
        if grid * grid != np {
            return Err(TorchError::Load(format!(
                "pos_table has {np} rows, not a square"
            )));
        }
        let pos_grid = pos.into_f32()?;
        let e = Npy::load(files.token_embeddings)?;
        let (vocab, d_model) = match e.shape[..] {
            [a, b] => (a as i64, b as i64),
            _ => return Err(TorchError::Load("token_embeddings is not 2-D".into())),
        };
        let emb = dev_f32(&e.into_f32()?, &[vocab, d_model], dev);
        let detok = Detokenizer::from_file(files.tokenizer)?;
        let f = freqs();
        let mut tc = Vec::new();
        let mut ts = Vec::new();
        for n in 0..=MAX_NEW_TOKENS {
            for _ in 0..2 {
                for fi in f {
                    tc.push((n as f32 * fi).cos());
                    ts.push((n as f32 * fi).sin());
                }
            }
        }
        let rows = (MAX_NEW_TOKENS + 1) as i64;
        Ok(Self {
            vision,
            prefill,
            step,
            io,
            _weights: weights,
            dev,
            emb,
            pos_grid,
            grid,
            pos_dim,
            pos_cache: Mutex::new(HashMap::new()),
            d_model,
            detok,
            budget,
            text_cos: dev_f32(&tc, &[rows, D_AXIS as i64], dev),
            text_sin: dev_f32(&ts, &[rows, D_AXIS as i64], dev),
            gpu_lock: Mutex::new(()),
        })
    }

    pub fn batch(&self) -> usize {
        BATCH
    }

    /// The resized position table for an (hp, wp) patch grid, `(hp*wp, 768)`.
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

    /// Returns generated token ids per crop (EOS_I/PAD_I removed).
    pub fn generate_ids(&self, crops: &[&Rgb]) -> Result<Vec<Vec<u32>>, TorchError> {
        let b = crops.len();
        let p = self.budget;
        let pd = self.pos_dim;
        let dev = self.dev;
        // §5.3 preprocessing (bunko-vlm hayai.rs, verbatim), one crop per task: the
        // resize + patchify and the position-table rows of a crop only touch that crop's
        // slices, so the crops run in parallel and the bytes are the same as in order.
        let mut pv = vec![0f32; b * p * PATCH_DIM];
        let mut pmask = vec![0f32; b * p];
        let mut pos = vec![0f32; b * p * pd];
        let mut shapes = vec![(0usize, 0usize); b];
        let inv255 = (1.0f64 / 255.0) as f32;
        let tasks: Vec<_> = crops
            .iter()
            .zip(pv.chunks_mut(p * PATCH_DIM))
            .zip(pmask.chunks_mut(p))
            .zip(pos.chunks_mut(p * pd))
            .zip(shapes.iter_mut())
            .collect();
        crate::par::for_each(tasks, |((((crop, pv), pmask), pos), shape)| {
            let (th, tw) = size_for_budget(crop.height, crop.width, p);
            let img = resize(crop, tw, th, Filter::Bilinear);
            let (hp, wp) = (th / PATCH, tw / PATCH);
            for py in 0..hp {
                for px in 0..wp {
                    let base = (py * wp + px) * PATCH_DIM;
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
            pmask[..hp * wp].fill(1.0);
            let r = self.pos_for(hp, wp);
            let n = hp * wp;
            pos[..n * pd].copy_from_slice(&r);
            for k in n..p {
                pos[k * pd..(k + 1) * pd].copy_from_slice(&r[..pd]);
            }
            *shape = (hp, wp);
        });
        let outs: Vec<(usize, usize, usize, usize)> = shapes
            .iter()
            .map(|&(hp, wp)| (hp, wp, hp.div_ceil(2), wp.div_ceil(2)))
            .collect();
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
        let l0 = m + 1;
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
                    cos[row + k] = (y * fk).cos();
                    sin[row + k] = (y * fk).sin();
                    cos[row + 16 + k] = (xx * fk).cos();
                    sin[row + 16 + k] = (xx * fk).sin();
                }
            }
            let row = (i * l0 + m) * D_AXIS;
            for k in 0..D_AXIS {
                cos[row + k] = 1.0; // text position 0
                sin[row + k] = 0.0;
            }
        }

        let _g = self.gpu_lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ng = tch::no_grad_guard();
        let (bi, pi, mi, dm) = (b as i64, p as i64, m as i64, self.d_model);
        let vis = self.vision.run(&[
            &dev_f32(&pv, &[bi, pi, PATCH_DIM as i64], dev),
            &dev_f32(&pmask, &[bi, pi], dev),
            &dev_f32(&pos, &[bi, pi, pd as i64], dev),
            &dev_i64(&gather, &[bi, mi, 4], dev),
            &dev_f32(&tok_valid, &[bi, mi], dev),
        ])?;
        let vis = vis[0].to_kind(Kind::Float);
        let bos = self
            .emb
            .get(BOS_I)
            .view([1, 1, dm])
            .expand([bi, 1, dm], false);
        let x = Tensor::cat(&[vis, bos], 1).contiguous();
        let l0i = l0 as i64;
        let out = self.prefill.run(&[
            &x,
            &dev_f32(&mask, &[bi, 1, l0i, l0i], dev),
            &dev_f32(&cos, &[bi, l0i, D_AXIS as i64], dev),
            &dev_f32(&sin, &[bi, l0i, D_AXIS as i64], dev),
        ])?;
        let (first, mut live_t, mut past) = split_decoder_out(out, self.io)?;
        let mut nxt = host_i64(&first)?;
        let mut seqlen = l0;
        let mut toks: Vec<Vec<u32>> = nxt.iter().map(|&t| vec![t as u32]).collect();
        let mut live: Vec<bool> = nxt.iter().map(|&t| t != EOS_I && t != PAD_I).collect();
        let kb = dev_f32(&key_bias, &[bi, mi], dev);
        for step in 1..MAX_NEW_TOKENS {
            if crate::runtime::shutting_down() {
                return Err(TorchError::Run("the process is exiting".into()));
            }
            if !live.iter().any(|&l| l) {
                break;
            }
            let ids = dev_i64(&nxt, &[bi], dev);
            let xs = self.emb.index_select(0, &ids).view([bi, 1, dm]);
            let tail = Tensor::zeros([bi, (seqlen + 1 - m) as i64], (Kind::Float, dev));
            let ms = Tensor::cat(&[&kb, &tail], 1).view([bi, 1, 1, (seqlen + 1) as i64]);
            let cs = self
                .text_cos
                .get(step as i64)
                .view([1, 1, D_AXIS as i64])
                .expand([bi, 1, D_AXIS as i64], false)
                .contiguous();
            let sn = self
                .text_sin
                .get(step as i64)
                .view([1, 1, D_AXIS as i64])
                .expand([bi, 1, D_AXIS as i64], false)
                .contiguous();
            let mut ins: Vec<&Tensor> = vec![&xs, &ms, &cs, &sn];
            if let Some(l) = &live_t {
                ins.push(l);
            }
            ins.extend(past.iter());
            let out = self.step.run(&ins)?;
            drop(ins);
            let (got, lt, p) = split_decoder_out(out, self.io)?;
            (live_t, past) = (lt, p);
            seqlen += 1;
            // I/O v2 already masks finished rows to PAD; the host rule below is the same.
            let got = host_i64(&got)?;
            for i in 0..b {
                nxt[i] = if live[i] { got[i] } else { PAD_I };
                if live[i] {
                    toks[i].push(nxt[i] as u32);
                }
                live[i] = live[i] && nxt[i] != EOS_I && nxt[i] != PAD_I;
            }
        }
        Ok(toks
            .into_iter()
            .map(|t| {
                t.into_iter()
                    .filter(|&t| t as i64 != EOS_I && t as i64 != PAD_I)
                    .collect()
            })
            .collect())
    }

    pub fn read_crops(&self, crops: &[&Rgb]) -> Result<Vec<String>, TorchError> {
        let mut out = Vec::with_capacity(crops.len());
        for chunk in crops.chunks(BATCH) {
            for ids in self.generate_ids(chunk)? {
                out.push(py_strip(&self.detok.decode(&ids)).to_owned());
            }
        }
        Ok(out)
    }
}
