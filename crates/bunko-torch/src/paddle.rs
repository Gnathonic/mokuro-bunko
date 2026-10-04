//! paddle-manga over three AOTInductor packages (vision once per crop, decoder prefill,
//! decoder step with the KV cache as inputs), the model loaded in the package precision
//! as 0.5.2 loaded it. Host side as bunko-vlm's ONNX recognizer: `smart_resize` + Pillow
//! bicubic, patchify, the constant chat prompt, M-RoPE positions, left padding, and a
//! batched greedy decode with a token cap per row.

use std::path::Path;
use std::sync::Mutex;

use bunko_vlm::Rgb;
use bunko_vlm::detok::Detokenizer;
use bunko_vlm::npy::Npy;
use bunko_vlm::paddle::{
    BATCH, DEFAULT_MAX_NEW_TOKENS, EOS, MROPE, PATCH, PREFIX, SIDE, SUFFIX, T_HD, V_HALF,
    smart_resize,
};
use bunko_vlm::pyfmt::py_strip;
use bunko_vlm::resample::{Filter, resize};
use tch::Tensor;

use crate::TorchError;
use crate::aoti::{Aoti, Placement, TPrec, dev_f32, dev_i64, host_i64};
use crate::package::{PackageSet, Weights, split_decoder_out};

/// The host files of paddle-manga (the graphs come as a [`PackageSet`]).
pub struct PaddleFiles<'a> {
    /// The input embedding table, f32 (or f16), cast to the package precision on load
    /// (the published fp32 table cast like this is bit-identical to the model's own).
    /// Not needed when the packages' weights carry it (`bunko.alias.host.embed_tokens`).
    pub embeddings: Option<&'a Path>,
    pub tokenizer: &'a Path,
}

/// The vision graph's host inputs for one crop.
struct VisionPrep {
    n: usize,
    pixels: Vec<f32>,
    pos_idx: Vec<i64>,
    pos_w: Vec<f32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
    merge: Vec<i64>,
    hb2: usize,
    wb2: usize,
}

pub struct TorchPaddle {
    vision: Aoti,
    prefill: Aoti,
    step: Aoti,
    io: u32,
    // The packages' shared weights (bound in place: kept alive with them).
    _weights: Option<Weights>,
    dev: tch::Device,
    prec: TPrec,
    embed: Tensor,
    d_model: i64,
    detok: Detokenizer,
    text_inv: [f32; T_HD / 2],
    gpu_lock: Mutex<()>,
}

// SAFETY: as `TorchHayai`: tensors are read-only after load, device work is under
// `gpu_lock`.
unsafe impl Send for TorchPaddle {}
unsafe impl Sync for TorchPaddle {}

impl TorchPaddle {
    pub fn load(
        pkgs: PackageSet,
        files: &PaddleFiles<'_>,
        prec: TPrec,
        at: Placement,
    ) -> Result<Self, TorchError> {
        let dev = at.dev;
        if let Some(pr) = pkgs.metadata("bunko.prompt") {
            let v: serde_json::Value = serde_json::from_str(&pr)
                .map_err(|e| TorchError::Load(format!("bad bunko.prompt: {e}")))?;
            let ids = |k: &str| -> Vec<i64> {
                v[k].as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
                    .unwrap_or_default()
            };
            if ids("prefix") != PREFIX || ids("suffix") != SUFFIX {
                return Err(TorchError::Load(format!(
                    "the package's prompt {pr} is not paddle-manga's"
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
        let embed = match weights.as_ref().and_then(|w| w.alias("host.embed_tokens")) {
            // Already the model's table in the model dtype, on the device.
            Some(t) => t.shallow_clone(),
            None => {
                let path = files.embeddings.ok_or_else(|| {
                    TorchError::Arg(
                        "paddle-manga needs `embeddings` (no table in the weights)".into(),
                    )
                })?;
                let e = Npy::load(path)?;
                let (vocab, d_model) = match e.shape[..] {
                    [a, b] => (a as i64, b as i64),
                    _ => return Err(TorchError::Load("paddle embeddings are not 2-D".into())),
                };
                // Cast on the host, then move: no f32 copy of the table on the device.
                Tensor::from_slice(&e.into_f32_widened()?)
                    .view([vocab, d_model])
                    .to_kind(prec.kind())
                    .to_device(dev)
            }
        };
        if embed.kind() != prec.kind() {
            return Err(TorchError::Load(format!(
                "paddle embeddings are {:?}, the packages are {}",
                embed.kind(),
                prec.as_str()
            )));
        }
        let d_model = embed.size().get(1).copied().unwrap_or(0);
        let detok = Detokenizer::from_file(files.tokenizer)?;
        let text_inv = std::array::from_fn(|i| 1.0 / 500_000f32.powf((2 * i) as f32 / T_HD as f32));
        Ok(Self {
            vision,
            prefill,
            step,
            io,
            _weights: weights,
            dev,
            prec,
            embed,
            d_model,
            detok,
            text_inv,
            gpu_lock: Mutex::new(()),
        })
    }

    pub fn batch(&self) -> usize {
        BATCH
    }

    fn mrope(&self, t: i64, h: i64, w: i64, cos: &mut Vec<f32>, sin: &mut Vec<f32>) {
        let mut ang = [0f32; T_HD / 2];
        let pos = [t as f32, h as f32, w as f32];
        let mut o = 0;
        for (axis, &sz) in MROPE.iter().enumerate() {
            for (k, a) in ang.iter_mut().enumerate().skip(o).take(sz) {
                *a = pos[axis] * self.text_inv[k];
            }
            o += sz;
        }
        for _ in 0..2 {
            cos.extend(ang.iter().map(|a| a.cos()));
            sin.extend(ang.iter().map(|a| a.sin()));
        }
    }

    /// The vision graph's host inputs for one crop (no device work: runs in parallel).
    fn vision_prep(crop: &Rgb) -> Result<VisionPrep, TorchError> {
        let (hb, wb) = smart_resize(crop.height, crop.width)?;
        let img = resize(crop, wb, hb, Filter::Bicubic);
        let (gh, gw) = (hb / PATCH, wb / PATCH);
        let n = gh * gw;
        let mut pixels = vec![0f32; n * 3 * PATCH * PATCH];
        for r in 0..gh {
            for c in 0..gw {
                let base = (r * gw + c) * 3 * PATCH * PATCH;
                for iy in 0..PATCH {
                    for ix in 0..PATCH {
                        let s = ((r * PATCH + iy) * wb + c * PATCH + ix) * 3;
                        for ch in 0..3 {
                            let v = f32::from(img.data[s + ch]);
                            pixels[base + (ch * PATCH + iy) * PATCH + ix] = (v / 255.0 - 0.5) / 0.5;
                        }
                    }
                }
            }
        }
        let taps = |pos: usize, len: usize| -> (usize, usize, f64) {
            let src = if len > 1 {
                pos as f64 * (SIDE - 1) as f64 / (len - 1) as f64
            } else {
                0.0
            };
            let fl = src.floor();
            let i0 = (fl.max(0.0) as usize).min(SIDE - 1);
            (i0, (i0 + 1).min(SIDE - 1), src - fl)
        };
        let inv: [f32; V_HALF / 2] =
            std::array::from_fn(|i| 1.0 / 10000f32.powf((2 * i) as f32 / V_HALF as f32));
        let mut pos_idx = Vec::with_capacity(n * 4);
        let mut pos_w = Vec::with_capacity(n * 4);
        let mut cos = Vec::with_capacity(n * 2 * V_HALF);
        let mut sin = Vec::with_capacity(n * 2 * V_HALF);
        for r in 0..gh {
            let (r0, r1, fr) = taps(r, gh);
            for c in 0..gw {
                let (c0, c1, fc) = taps(c, gw);
                pos_idx.extend(
                    [
                        r0 * SIDE + c0,
                        r0 * SIDE + c1,
                        r1 * SIDE + c0,
                        r1 * SIDE + c1,
                    ]
                    .map(|v| v as i64),
                );
                let (rw0, rw1, cw0, cw1) = (1.0 - fr, fr, 1.0 - fc, fc);
                pos_w.extend([rw0 * cw0, rw0 * cw1, rw1 * cw0, rw1 * cw1].map(|v| v as f32));
                let mut ang = [0f32; V_HALF];
                for (k, &f) in inv.iter().enumerate() {
                    ang[k] = r as f32 * f;
                    ang[V_HALF / 2 + k] = c as f32 * f;
                }
                for _ in 0..2 {
                    cos.extend(ang.iter().map(|a| a.cos()));
                    sin.extend(ang.iter().map(|a| a.sin()));
                }
            }
        }
        let (hb2, wb2) = (gh / 2, gw / 2);
        let mut merge = Vec::with_capacity(n);
        for bi in 0..hb2 {
            for bj in 0..wb2 {
                merge.extend(
                    [
                        (2 * bi) * gw + 2 * bj,
                        (2 * bi) * gw + 2 * bj + 1,
                        (2 * bi + 1) * gw + 2 * bj,
                        (2 * bi + 1) * gw + 2 * bj + 1,
                    ]
                    .map(|v| v as i64),
                );
            }
        }
        Ok(VisionPrep {
            n,
            pixels,
            pos_idx,
            pos_w,
            cos,
            sin,
            merge,
            hb2,
            wb2,
        })
    }

    /// Image embeddings of one prepared crop on the device, `(gh/2*gw/2, 1024)` in the
    /// model dtype.
    fn vision_embeds(&self, v: &VisionPrep) -> Result<(Tensor, usize, usize), TorchError> {
        let ni = v.n as i64;
        let dev = self.dev;
        let out = self.vision.run(&[
            &dev_f32(&v.pixels, &[ni, 3, PATCH as i64, PATCH as i64], dev)
                .to_kind(self.prec.kind()),
            &dev_i64(&v.pos_idx, &[ni, 4], dev),
            &dev_f32(&v.pos_w, &[ni, 4], dev),
            &dev_f32(&v.cos, &[ni, 2 * V_HALF as i64], dev),
            &dev_f32(&v.sin, &[ni, 2 * V_HALF as i64], dev),
            &dev_i64(&v.merge, &[ni], dev),
        ])?;
        Ok((
            out.into_iter()
                .next()
                .ok_or_else(|| TorchError::Run("no vision output".into()))?,
            v.hb2,
            v.wb2,
        ))
    }

    pub fn generate_ids(&self, crops: &[&Rgb], caps: &[u32]) -> Result<Vec<Vec<u32>>, TorchError> {
        let _g = self.gpu_lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ng = tch::no_grad_guard();
        let b = crops.len();
        let dev = self.dev;
        let kind = self.prec.kind();
        let dm = self.d_model;
        let neg = self.prec.finfo_min();
        let pre_ids = dev_i64(&PREFIX, &[PREFIX.len() as i64], dev);
        let suf_ids = dev_i64(&SUFFIX, &[SUFFIX.len() as i64], dev);
        let pre_e = self.embed.index_select(0, &pre_ids);
        let suf_e = self.embed.index_select(0, &suf_ids);
        let mut rows: Vec<(Tensor, Vec<[i64; 3]>)> = Vec::with_capacity(b);
        // Crops are prepared on host threads while the device runs the vision graph of
        // the ones before (in crop order: the same tensors as one-by-one).
        // On the CPU the crops' preparation shares the cores. On a GPU paddle-manga is
        // device-bound and helper threads measured ~1% slower (4090): prepare in line.
        let helpers = if self.dev == tch::Device::Cpu { 8 } else { 0 };
        crate::par::map_ordered(
            crops,
            helpers,
            |crop| Self::vision_prep(crop),
            |_, prep| {
                let (img, hb, wb) = self.vision_embeds(&prep?)?;
                let e = Tensor::cat(&[&pre_e, &img.to_kind(kind), &suf_e], 0);
                let mut pos = Vec::new();
                for j in 0..PREFIX.len() {
                    pos.push([j as i64; 3]);
                }
                let p0 = PREFIX.len() as i64;
                for bi in 0..hb {
                    for bj in 0..wb {
                        pos.push([p0, p0 + bi as i64, p0 + bj as i64]);
                    }
                }
                let next = p0 + hb.max(wb) as i64;
                for m in 0..SUFFIX.len() {
                    pos.push([next + m as i64; 3]);
                }
                rows.push((e, pos));
                Ok::<(), TorchError>(())
            },
        )?;
        let s = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
        let pad_row = self.embed.get(0).view([1, dm]);
        let mut xs = Vec::with_capacity(b);
        let mut cos = Vec::with_capacity(b * s * T_HD);
        let mut sin = Vec::with_capacity(b * s * T_HD);
        let mut valid = vec![false; b * s];
        let mut base = vec![0i64; b];
        for (i, (e, pos)) in rows.iter().enumerate() {
            let pad = s - pos.len();
            let mut parts: Vec<Tensor> = Vec::new();
            if pad > 0 {
                parts.push(pad_row.expand([pad as i64, dm], false));
                for _ in 0..pad {
                    self.mrope(0, 0, 0, &mut cos, &mut sin);
                }
            }
            parts.push(e.shallow_clone());
            xs.push(Tensor::cat(&parts, 0));
            for p in pos {
                self.mrope(p[0], p[1], p[2], &mut cos, &mut sin);
            }
            valid[i * s + pad..(i + 1) * s].fill(true);
            base[i] = pos
                .iter()
                .flat_map(|p| p.iter().copied())
                .max()
                .unwrap_or(0)
                + 1;
        }
        drop(rows);
        let x = Tensor::stack(&xs, 0).contiguous();
        drop(xs);
        let mut bias = vec![0f32; b * s * s];
        for i in 0..b {
            for q in 0..s {
                for k in 0..s {
                    let allow = (k <= q && valid[i * s + k]) || k == q;
                    if !allow {
                        bias[(i * s + q) * s + k] = neg;
                    }
                }
            }
        }
        let (bi, si) = (b as i64, s as i64);
        let out = self.prefill.run(&[
            &x,
            &dev_f32(&cos, &[bi, si, T_HD as i64], dev).to_kind(kind),
            &dev_f32(&sin, &[bi, si, T_HD as i64], dev).to_kind(kind),
            &dev_f32(&bias, &[bi, 1, si, si], dev).to_kind(kind),
        ])?;
        let (first, _, mut past) = split_decoder_out(out, self.io)?;
        let mut tok = host_i64(&first)?;
        let max_new = caps.iter().copied().max().unwrap_or(DEFAULT_MAX_NEW_TOKENS) as usize;
        let mut out: Vec<Vec<u32>> = vec![Vec::new(); b];
        let mut done: Vec<bool> = caps.iter().map(|&c| c == 0).collect();
        let mut keyvalid = valid;
        let mut total = s;
        for step in 0..max_new {
            if crate::runtime::shutting_down() {
                return Err(TorchError::Run("the process is exiting".into()));
            }
            for i in 0..b {
                if done[i] {
                    continue;
                }
                if tok[i] as u32 == EOS {
                    done[i] = true;
                } else {
                    out[i].push(tok[i] as u32);
                    if out[i].len() >= caps[i] as usize {
                        done[i] = true;
                    }
                }
            }
            if done.iter().all(|&d| d) || step + 1 == max_new {
                break;
            }
            let ids: Vec<i64> = (0..b).map(|i| if done[i] { 0 } else { tok[i] }).collect();
            let xs = self
                .embed
                .index_select(0, &dev_i64(&ids, &[bi], dev))
                .view([bi, 1, dm]);
            let mut cs = Vec::with_capacity(b * T_HD);
            let mut sn = Vec::with_capacity(b * T_HD);
            let mut nb = Vec::with_capacity(b * (total + 1));
            for i in 0..b {
                let p = base[i] + step as i64;
                self.mrope(p, p, p, &mut cs, &mut sn);
                nb.extend(
                    keyvalid[i * total..(i + 1) * total]
                        .iter()
                        .map(|&v| if v { 0.0 } else { neg }),
                );
                nb.push(0.0);
            }
            let mut kv2 = Vec::with_capacity(b * (total + 1));
            for i in 0..b {
                kv2.extend_from_slice(&keyvalid[i * total..(i + 1) * total]);
                kv2.push(true);
            }
            keyvalid = kv2;
            total += 1;
            let c = dev_f32(&cs, &[bi, 1, T_HD as i64], dev).to_kind(kind);
            let sv = dev_f32(&sn, &[bi, 1, T_HD as i64], dev).to_kind(kind);
            let bb = dev_f32(&nb, &[bi, 1, 1, total as i64], dev).to_kind(kind);
            // I/O v2: the rows still generating (the host's caps decide, so the mask is
            // the host's); finished rows come back as token 0 and are not read.
            let live_t = (self.io >= 2).then(|| {
                let live: Vec<bool> = done.iter().map(|d| !d).collect();
                Tensor::from_slice(&live).to_device(dev)
            });
            let mut ins: Vec<&Tensor> = vec![&xs, &c, &sv, &bb];
            if let Some(l) = &live_t {
                ins.push(l);
            }
            ins.extend(past.iter());
            let o = self.step.run(&ins)?;
            drop(ins);
            let (next, _, p) = split_decoder_out(o, self.io)?;
            past = p;
            tok = host_i64(&next)?;
        }
        Ok(out
            .into_iter()
            .zip(caps)
            .map(|(mut t, &cap)| {
                t.truncate(cap as usize);
                t
            })
            .collect())
    }

    pub fn read_crops(&self, crops: &[&Rgb], caps: &[u32]) -> Result<Vec<String>, TorchError> {
        let mut order: Vec<usize> = (0..crops.len()).collect();
        let area = |i: usize| (crops[i].width * crops[i].height) as f64;
        order.sort_by(|&a, &b| {
            caps[a]
                .cmp(&caps[b])
                .then(area(a).total_cmp(&area(b)))
                .then(a.cmp(&b))
        });
        let mut texts = vec![String::new(); crops.len()];
        for group in order.chunks(BATCH) {
            let gc: Vec<&Rgb> = group.iter().map(|&i| crops[i]).collect();
            let gcaps: Vec<u32> = group.iter().map(|&i| caps[i]).collect();
            for (&i, ids) in group.iter().zip(self.generate_ids(&gc, &gcaps)?) {
                texts[i] = py_strip(&self.detok.decode(&ids)).to_owned();
            }
        }
        Ok(texts)
    }
}
