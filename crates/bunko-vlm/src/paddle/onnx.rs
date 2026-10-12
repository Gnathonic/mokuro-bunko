//! paddle-manga on ONNX Runtime (feature `onnx-vlm`; deferred in 0.7, see
//! `docs/rust-port/TORCH-BACKEND.md`). Host side: `smart_resize` + Pillow bicubic,
//! patchify, the vision graph once per crop, the constant chat prompt, M-RoPE positions,
//! left padding, and a batched greedy decode with a token cap per row (the runner's
//! truncation, ER:1913-1919).

use half::f16;
use ort::value::DynValue;

use super::{
    BASE_REPO, BASE_REVISION, BATCH, DEFAULT_MAX_NEW_TOKENS, EOS, IMAGE_TOKEN, KV_HEADS, LORA_REPO,
    LORA_REVISION, MROPE, PATCH, PREFIX, PaddleAssets, SIDE, SUFFIX, T_HD, V_HALF, smart_resize,
};
use crate::crop::{LINE_MARGIN_EM, Quad, SECOND_MARGIN_EM, paddle_quad_crop};
use crate::detok::Detokenizer;
use crate::gpudec::{Call, SharedDecoder};
use crate::image::{Bgr, Rgb};
use crate::npy::Npy;
use crate::pyfmt::py_strip;
use crate::resample::{Filter, resize};
use crate::runtime::{Provider, SessionFactory, SessionOptions, SharedSession, WebGpuGuard};
use crate::tensor::{FloatKind, argmax_rows, floats, halfs, i64s, to_f32};
use crate::{CropSet, Precision, Recognizer, RecognizerInfo, VlmError, read_flat};

/// Input slots of the decoder graph.
#[derive(Clone, Copy, Debug)]
enum DecIn {
    Embeds,
    Cos,
    Sin,
    Bias,
    Past(usize),
}

/// One crop ready for the vision graph.
struct Prepped {
    pixels: Vec<f32>,
    gh: usize,
    gw: usize,
}

/// The paddle-manga recognizer. One instance (two sessions) serves every thread.
pub struct PaddleManga {
    /// The exported graphs; None when the WebGPU graphs replace them.
    vision: Option<SharedSession>,
    decoder: Option<SharedSession>,
    gpu: Option<super::gpu::PaddleGpu>,
    provider: Provider,
    vis_in: Vec<&'static str>,
    pix_kind: FloatKind,
    vis_aux_kind: FloatKind,
    dec_kind: FloatKind,
    dec_in: Vec<DecIn>,
    n_kv: usize,
    embed: Vec<f32>,
    d_model: usize,
    vocab: usize,
    detok: Detokenizer,
    text_inv: [f32; T_HD / 2],
    info: RecognizerInfo,
}

impl std::fmt::Debug for PaddleManga {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaddleManga")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl PaddleManga {
    pub fn load(
        factory: &dyn SessionFactory,
        assets: &PaddleAssets,
        opts: &SessionOptions,
    ) -> Result<Self, VlmError> {
        let gpu_dir = match opts.device {
            crate::Device::Gpu(_) => assets.gpu_dir.as_ref(),
            crate::Device::Cpu => None,
        };
        let gpu = gpu_dir
            .map(|d| super::gpu::PaddleGpu::load(factory, d, opts))
            .transpose()?;
        let vision = match gpu {
            Some(_) => None,
            None => Some(factory.open(&assets.vision, opts)?),
        };
        let mut vis_in = Vec::new();
        let mut pix_kind = FloatKind::F32;
        let mut vis_aux_kind = FloatKind::F32;
        let vin: Vec<(usize, String)> = vision
            .iter()
            .flat_map(|v| {
                v.input_names()
                    .map(str::to_owned)
                    .enumerate()
                    .collect::<Vec<_>>()
            })
            .collect();
        for (i, n) in vin.iter().map(|(i, n)| (*i, n.as_str())) {
            let vision = vision
                .as_ref()
                .ok_or_else(|| VlmError::Runtime("no vision".into()))?;
            let name = match n {
                "pixel_values" => {
                    pix_kind = FloatKind::of(vision.input_type(i), "paddle pixel_values")?;
                    "pixel_values"
                }
                "pos_idx" => "pos_idx",
                "pos_w" => {
                    vis_aux_kind = FloatKind::of(vision.input_type(i), "paddle pos_w")?;
                    "pos_w"
                }
                "cos" => "cos",
                "sin" => "sin",
                "merge_idx" => "merge_idx",
                other => {
                    return Err(VlmError::Asset(format!(
                        "paddle vision: unknown input {other}"
                    )));
                }
            };
            vis_in.push(name);
        }
        let (decoder, dec_in, n_kv, dec_kind) = match &gpu {
            Some(g) => (None, Vec::new(), 0, g.decoder.kind()),
            None => {
                let decoder = factory.open(&assets.decoder, opts)?;
                let outs: Vec<String> = decoder.output_names().map(str::to_owned).collect();
                if outs.first().map(String::as_str) != Some("logits") {
                    return Err(VlmError::Asset(
                        "paddle decoder: first output is not logits".into(),
                    ));
                }
                let mut dec_in = Vec::new();
                for n in decoder.input_names() {
                    dec_in.push(match n {
                        "inputs_embeds" => DecIn::Embeds,
                        "cos" => DecIn::Cos,
                        "sin" => DecIn::Sin,
                        "bias" => DecIn::Bias,
                        p if p.starts_with("past_") => {
                            let want = p.replacen("past_", "present_", 1);
                            let j = outs.iter().position(|o| *o == want).ok_or_else(|| {
                                VlmError::Asset(format!("paddle decoder: no output {want}"))
                            })?;
                            DecIn::Past(j)
                        }
                        other => {
                            return Err(VlmError::Asset(format!(
                                "paddle decoder: unknown input {other}"
                            )));
                        }
                    });
                }
                let n_kv = dec_in
                    .iter()
                    .filter(|d| matches!(d, DecIn::Past(_)))
                    .count();
                let dec_kind = FloatKind::of(decoder.input_type(0), "paddle decoder input")?;
                (Some(decoder), dec_in, n_kv, dec_kind)
            }
        };
        let emb = Npy::load(&assets.embed)?;
        let (vocab, d_model) = match emb.shape[..] {
            [a, b] => (a, b),
            _ => return Err(VlmError::Asset("embed.npy is not 2-D".into())),
        };
        // the f16 table, or the f32 one (the libtorch packs' host file) kept as is: f16
        // would round its smallest values
        let embed = emb.into_f32_widened()?;
        let detok = Detokenizer::from_file(&assets.tokenizer)?;
        let text_inv: [f32; T_HD / 2] =
            std::array::from_fn(|i| 1.0 / 500_000f32.powf((2 * i) as f32 / T_HD as f32));
        let precision = match dec_kind {
            FloatKind::F32 => Precision::Fp32,
            FloatKind::F16 => Precision::Fp16,
        };
        let info = RecognizerInfo {
            engine: "paddle-manga",
            recognizer: LORA_REPO,
            repos: vec![(BASE_REPO, BASE_REVISION), (LORA_REPO, LORA_REVISION)],
            precision,
            device: opts.device,
            second_read: true,
            token_caps: true,
            default_max_tokens: DEFAULT_MAX_NEW_TOKENS,
            batch: BATCH,
            patch_budget: None,
        };
        tracing::info!(device = %opts.device, %precision, kv = n_kv, specials = detok.special_count(), "paddle-manga loaded");
        Ok(Self {
            vision,
            decoder,
            gpu,
            provider: opts.provider,
            vis_in,
            pix_kind,
            vis_aux_kind,
            dec_kind,
            dec_in,
            n_kv,
            embed,
            d_model,
            vocab,
            detok,
            text_inv,
            info,
        })
    }

    fn prep(crop: &Rgb) -> Result<Prepped, VlmError> {
        let (hb, wb) = smart_resize(crop.height, crop.width)?;
        let img = resize(crop, wb, hb, Filter::Bicubic);
        let (gh, gw) = (hb / PATCH, wb / PATCH);
        let mut pixels = vec![0f32; gh * gw * 3 * PATCH * PATCH];
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
        Ok(Prepped { pixels, gh, gw })
    }

    /// Image embeddings of one crop, `(gh/2 * gw/2, 1024)` as f32 (spec §6.4, §6.5).
    fn vision_embeds(&self, p: Prepped) -> Result<Vec<f32>, VlmError> {
        let (gh, gw) = (p.gh, p.gw);
        let n = gh * gw;
        let taps = |pos: usize, len: usize| -> (usize, usize, f64) {
            let src = if len > 1 {
                pos as f64 * (SIDE - 1) as f64 / (len - 1) as f64
            } else {
                0.0
            };
            let fl = src.floor();
            let i0 = (fl.max(0.0) as usize).min(SIDE - 1);
            let i1 = (i0 + 1).min(SIDE - 1);
            (i0, i1, src - fl)
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
        let (hb, wb) = (gh / 2, gw / 2);
        let mut merge = Vec::with_capacity(n);
        for bi in 0..hb {
            for bj in 0..wb {
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
        if let Some(g) = &self.gpu {
            return g
                .vision
                .forward(n, p.pixels, pos_idx, pos_w, cos, sin, merge);
        }
        let vision = self
            .vision
            .as_ref()
            .ok_or_else(|| VlmError::Runtime("paddle vision not loaded".into()))?;
        let mut pixels = Some(p.pixels);
        let mut pos_idx = Some(pos_idx);
        let mut pos_w = Some(pos_w);
        let mut cos = Some(cos);
        let mut sin = Some(sin);
        let mut merge = Some(merge);
        let mut feeds = Vec::with_capacity(6);
        for name in &self.vis_in {
            let ak = self.vis_aux_kind;
            feeds.push(match *name {
                "pixel_values" => floats(
                    self.pix_kind,
                    &[n, 3, PATCH, PATCH],
                    pixels.take().unwrap_or_default(),
                )?,
                "pos_idx" => i64s(&[n, 4], pos_idx.take().unwrap_or_default())?,
                "pos_w" => floats(ak, &[n, 4], pos_w.take().unwrap_or_default())?,
                "cos" => floats(ak, &[n, 2 * V_HALF], cos.take().unwrap_or_default())?,
                "sin" => floats(ak, &[n, 2 * V_HALF], sin.take().unwrap_or_default())?,
                _ => i64s(&[n], merge.take().unwrap_or_default())?,
            });
        }
        let refs: Vec<&DynValue> = feeds.iter().collect();
        let out = to_f32(&vision.run(&refs, Vec::new())?[0])?;
        if out.len() != (n / 4) * self.d_model {
            return Err(VlmError::Runtime(format!(
                "vision returned {} values for {} patches",
                out.len(),
                n
            )));
        }
        Ok(out)
    }

    fn embed_row(&self, id: usize, out: &mut Vec<f32>) {
        out.extend_from_slice(&self.embed[id * self.d_model..(id + 1) * self.d_model]);
    }

    /// cos/sin rows (128 each) for M-RoPE position (t, h, w) (spec §6.7).
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

    /// One batch: greedy decode with a token cap per row; texts stripped, batch order.
    fn generate(&self, crops: &[&Rgb], caps: &[u32]) -> Result<Vec<String>, VlmError> {
        let b = crops.len();
        let dm = self.d_model;
        let neg = match self.dec_kind {
            FloatKind::F32 => f32::MIN,
            FloatKind::F16 => -65504.0,
        };
        // host preprocessing first: another engine thread may run on the GPU meanwhile
        let prepped = crops
            .iter()
            .map(|c| Self::prep(c))
            .collect::<Result<Vec<_>, _>>()?;
        // WebGPU: one context per process, not thread-safe (`runtime::WebGpuGuard`); held
        // until this batch's last device tensor is gone (declared before them).
        let _webgpu = (self.provider == Provider::WebGpu).then(WebGpuGuard::lock);
        // per-row prompt embeddings and positions
        let mut rows: Vec<(Vec<f32>, Vec<[i64; 3]>)> = Vec::with_capacity(b);
        for p in prepped {
            let (hb, wb) = (p.gh / 2, p.gw / 2);
            let img = self.vision_embeds(p)?;
            let n_img = hb * wb;
            let mut e = Vec::with_capacity((PREFIX.len() + n_img + SUFFIX.len()) * dm);
            let mut pos = Vec::with_capacity(PREFIX.len() + n_img + SUFFIX.len());
            for (j, &id) in PREFIX.iter().enumerate() {
                self.embed_row(id as usize, &mut e);
                pos.push([j as i64; 3]);
            }
            e.extend_from_slice(&img);
            let p0 = PREFIX.len() as i64;
            for bi in 0..hb {
                for bj in 0..wb {
                    pos.push([p0, p0 + bi as i64, p0 + bj as i64]);
                }
            }
            let next = p0 + hb.max(wb) as i64;
            for (m, &id) in SUFFIX.iter().enumerate() {
                self.embed_row(id as usize, &mut e);
                pos.push([next + m as i64; 3]);
            }
            debug_assert_eq!(IMAGE_TOKEN, 100_295);
            rows.push((e, pos));
        }
        let s = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
        let mut x = Vec::with_capacity(b * s * dm);
        let mut cos = Vec::with_capacity(b * s * T_HD);
        let mut sin = Vec::with_capacity(b * s * T_HD);
        let mut valid = vec![false; b * s];
        let mut base = vec![0i64; b];
        for (i, (e, pos)) in rows.iter().enumerate() {
            let pad = s - pos.len();
            for _ in 0..pad {
                self.embed_row(0, &mut x);
                self.mrope(0, 0, 0, &mut cos, &mut sin);
            }
            x.extend_from_slice(e);
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
        if let Some(g) = &self.gpu {
            return self.decode_gpu(&g.decoder, b, s, x, cos, sin, valid, &base, caps, neg);
        }
        let decoder = self
            .decoder
            .as_ref()
            .ok_or_else(|| VlmError::Runtime("paddle decoder not loaded".into()))?;
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

        let dk = self.dec_kind;
        // Device-resident KV for CUDA only (WebGPU: see hayai's `gpu.rs`).
        let alloc = if decoder.provider() == Provider::WebGpu {
            None
        } else {
            decoder.device_allocator()?
        };
        let mut past: Vec<DynValue> = Vec::with_capacity(self.n_kv);
        for _ in 0..self.n_kv {
            past.push(floats(dk, &[b, KV_HEADS, 0, T_HD], Vec::new())?);
        }
        let run = |inp: [&DynValue; 4],
                   past: &mut Vec<DynValue>,
                   total: usize|
         -> Result<Vec<u32>, VlmError> {
            let feeds: Vec<&DynValue> = self
                .dec_in
                .iter()
                .map(|d| match d {
                    DecIn::Embeds => inp[0],
                    DecIn::Cos => inp[1],
                    DecIn::Sin => inp[2],
                    DecIn::Bias => inp[3],
                    DecIn::Past(j) => &past[*j - 1],
                })
                .collect();
            let mut pre: Vec<Option<DynValue>> = vec![None];
            if let Some(a) = &alloc {
                for _ in 0..self.n_kv {
                    pre.push(Some(crate::tensor::alloc(
                        a,
                        dk,
                        &[b, KV_HEADS, total, T_HD],
                    )?));
                }
            }
            let mut out = decoder.run(&feeds, pre)?;
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

        let inputs = [
            floats(dk, &[b, s, dm], x)?,
            floats(dk, &[b, s, T_HD], cos)?,
            floats(dk, &[b, s, T_HD], sin)?,
            floats(dk, &[b, 1, s, s], bias)?,
        ];
        let mut tok = run(
            [&inputs[0], &inputs[1], &inputs[2], &inputs[3]],
            &mut past,
            s,
        )?;
        drop(inputs);
        let max_new = caps.iter().copied().max().unwrap_or(DEFAULT_MAX_NEW_TOKENS) as usize;
        let mut out: Vec<Vec<u32>> = vec![Vec::new(); b];
        let mut done: Vec<bool> = caps.iter().map(|&c| c == 0).collect();
        let mut keyvalid = valid;
        let mut total = s;
        for step in 0..max_new {
            for i in 0..b {
                if done[i] {
                    continue;
                }
                if tok[i] == EOS {
                    done[i] = true;
                } else {
                    out[i].push(tok[i]);
                    // Tokens past a row's own cap are discarded (ER:1916), so the row can stop.
                    if out[i].len() >= caps[i] as usize {
                        done[i] = true;
                    }
                }
            }
            if done.iter().all(|&d| d) || step + 1 == max_new {
                break;
            }
            let mut xs = Vec::with_capacity(b * dm);
            let mut cs = Vec::with_capacity(b * T_HD);
            let mut sn = Vec::with_capacity(b * T_HD);
            let mut nb = Vec::with_capacity(b * (total + 1));
            for i in 0..b {
                let t = if done[i] { 0 } else { tok[i] as usize };
                self.embed_row(t, &mut xs);
                let p = base[i] + step as i64;
                self.mrope(p, p, p, &mut cs, &mut sn);
                nb.extend(
                    keyvalid[i * total..(i + 1) * total]
                        .iter()
                        .map(|&v| if v { 0.0 } else { neg }),
                );
                nb.push(0.0);
            }
            // grow the key-validity rows by one valid slot
            let mut kv2 = Vec::with_capacity(b * (total + 1));
            for i in 0..b {
                kv2.extend_from_slice(&keyvalid[i * total..(i + 1) * total]);
                kv2.push(true);
            }
            keyvalid = kv2;
            total += 1;
            let x = match dk {
                FloatKind::F16 => {
                    halfs(&[b, 1, dm], xs.iter().map(|&v| f16::from_f32(v)).collect())?
                }
                FloatKind::F32 => floats(dk, &[b, 1, dm], xs)?,
            };
            let inputs = [
                x,
                floats(dk, &[b, 1, T_HD], cs)?,
                floats(dk, &[b, 1, T_HD], sn)?,
                floats(dk, &[b, 1, 1, total], nb)?,
            ];
            tok = run(
                [&inputs[0], &inputs[1], &inputs[2], &inputs[3]],
                &mut past,
                total,
            )?;
        }
        Ok(out
            .into_iter()
            .zip(caps)
            .map(|(mut t, &cap)| {
                t.truncate(cap as usize);
                py_strip(&self.detok.decode(&t)).to_owned()
            })
            .collect())
    }

    /// The batch on the WebGPU decoder graph (`crate::gpudec`): same masks, positions and
    /// greedy loop as the exported decoder's, the KV cache on the device throughout.
    #[allow(clippy::too_many_arguments)]
    fn decode_gpu(
        &self,
        g: &SharedDecoder,
        b: usize,
        s: usize,
        x: Vec<f32>,
        cos: Vec<f32>,
        sin: Vec<f32>,
        valid: Vec<bool>,
        base: &[i64],
        caps: &[u32],
        neg: f32,
    ) -> Result<Vec<String>, VlmError> {
        // The allocator outlives every device tensor of this batch (declared first).
        let alloc = g.allocator()?;
        let dm = self.d_model;
        let half = |v: &[f32]| -> Vec<f32> {
            v.chunks_exact(T_HD)
                .flat_map(|r| r[..T_HD / 2].iter().copied())
                .collect()
        };
        // prefill: gen_bias (b, 1, s, 1 + s) = the dummy key, then the exported mask
        let w = 1 + s;
        let mut gen_bias = vec![neg; b * s * w];
        for i in 0..b {
            for q in 0..s {
                let row = &mut gen_bias[(i * s + q) * w..(i * s + q + 1) * w];
                for k in 0..s {
                    if (k <= q && valid[i * s + k]) || k == q {
                        row[1 + k] = 0.0;
                    }
                }
            }
        }
        let dummy_ctx = g.dummy("ctx", b)?;
        let dummy_gen = g.dummy("gen", b)?;
        // the prompt in chunks (bounded attention scores per run): every query keeps
        // exactly its keys and mask row
        let step = super::gpu::prefill_chunk(b, s + 1);
        let (cos, sin) = (half(&cos), half(&sin));
        let mut first = Vec::new();
        let mut acc: Option<crate::gpudec::Kv> = None;
        for a in (0..s).step_by(step) {
            let e = (a + step).min(s);
            let c = e - a;
            let rows = |v: &[f32], width: usize| -> Vec<f32> {
                (0..b)
                    .flat_map(|i| v[(i * s + a) * width..(i * s + e) * width].iter().copied())
                    .collect()
            };
            // gen keys so far: [dummy, tokens 0..e)
            let mut gb = Vec::with_capacity(b * c * (1 + e));
            for i in 0..b {
                for q in a..e {
                    gb.extend_from_slice(&gen_bias[(i * s + q) * w..(i * s + q) * w + 1 + e]);
                }
            }
            let (nx, gr) = g.run(
                alloc.as_ref(),
                Call {
                    b,
                    s: c,
                    x: rows(&x, dm),
                    cos: rows(&cos, T_HD / 2),
                    sin: rows(&sin, T_HD / 2),
                    ctx: &dummy_ctx,
                    ctx_bias: vec![neg; b * c],
                    grow: acc.as_ref().unwrap_or(&dummy_gen),
                    gen_bias: gb,
                },
                dm,
            )?;
            first = nx;
            acc = Some(gr);
        }
        let grow = acc.ok_or_else(|| VlmError::Runtime("empty prompt".into()))?;
        drop(dummy_ctx);
        let ctx = SharedDecoder::into_ctx(grow);
        let ctx_bias: Vec<f32> = (0..b)
            .flat_map(|i| {
                std::iter::once(neg).chain(
                    valid[i * s..(i + 1) * s]
                        .iter()
                        .map(move |&v| if v { 0.0 } else { neg }),
                )
            })
            .collect();
        let mut grow = dummy_gen;
        let mut tok: Vec<u32> = first.iter().map(|&t| t as u32).collect();
        let max_new = caps.iter().copied().max().unwrap_or(DEFAULT_MAX_NEW_TOKENS) as usize;
        let mut out: Vec<Vec<u32>> = vec![Vec::new(); b];
        let mut done: Vec<bool> = caps.iter().map(|&c| c == 0).collect();
        for step in 0..max_new {
            for i in 0..b {
                if done[i] {
                    continue;
                }
                if tok[i] == EOS {
                    done[i] = true;
                } else {
                    out[i].push(tok[i]);
                    if out[i].len() >= caps[i] as usize {
                        done[i] = true;
                    }
                }
            }
            if done.iter().all(|&d| d) || step + 1 == max_new {
                break;
            }
            let t = SharedDecoder::len(&grow);
            let mut xs = Vec::with_capacity(b * dm);
            let mut cs = Vec::with_capacity(b * T_HD);
            let mut sn = Vec::with_capacity(b * T_HD);
            for i in 0..b {
                let id = if done[i] { 0 } else { tok[i] as usize };
                self.embed_row(id, &mut xs);
                let p = base[i] + step as i64;
                self.mrope(p, p, p, &mut cs, &mut sn);
            }
            let gen_bias: Vec<f32> = (0..b)
                .flat_map(|_| std::iter::once(neg).chain(std::iter::repeat_n(0.0, t)))
                .collect();
            let (got, grown) = g.run(
                alloc.as_ref(),
                Call {
                    b,
                    s: 1,
                    x: xs,
                    cos: half(&cs),
                    sin: half(&sn),
                    ctx: &ctx,
                    ctx_bias: ctx_bias.clone(),
                    grow: &grow,
                    gen_bias,
                },
                dm,
            )?;
            grow = grown;
            tok = got.iter().map(|&t| t as u32).collect();
        }
        drop((ctx, grow));
        Ok(out
            .into_iter()
            .zip(caps)
            .map(|(mut t, &cap)| {
                t.truncate(cap as usize);
                py_strip(&self.detok.decode(&t)).to_owned()
            })
            .collect())
    }

    /// Reads crops with one token cap each: sorted by (cap, area, index), batches of
    /// [`BATCH`], results in input order (spec §6.11).
    pub fn read_crops(&self, crops: &[&Rgb], caps: &[u32]) -> Result<Vec<String>, VlmError> {
        if caps.len() != crops.len() {
            return Err(VlmError::Config(format!(
                "{} token caps for {} crops",
                caps.len(),
                crops.len()
            )));
        }
        let mut order: Vec<usize> = (0..crops.len()).collect();
        let area = |i: usize| (crops[i].width * crops[i].height) as f64;
        order.sort_by(|&a, &b| {
            caps[a]
                .cmp(&caps[b])
                .then(area(a).total_cmp(&area(b)))
                .then(a.cmp(&b))
        });
        let mut texts = vec![String::new(); crops.len()];
        for batch in order.chunks(BATCH) {
            // WebGPU: a batch of big crops is split so rows x prompt^2 stays bounded (rows
            // are independent: the texts do not change)
            let mut groups: Vec<Vec<usize>> = Vec::new();
            if self.gpu.is_some() {
                let mut cost = 0usize;
                for &i in batch {
                    let p = super::gpu::prompt_len(crops[i].height, crops[i].width);
                    let s2 = p.saturating_mul(p);
                    match groups.last_mut() {
                        Some(g)
                            if (g.len() + 1).saturating_mul(cost.max(s2))
                                <= super::gpu::BATCH_COST =>
                        {
                            g.push(i);
                            cost = cost.max(s2);
                        }
                        _ => {
                            groups.push(vec![i]);
                            cost = s2;
                        }
                    }
                }
            } else {
                groups.push(batch.to_vec());
            }
            for group in groups {
                let gc: Vec<&Rgb> = group.iter().map(|&i| crops[i]).collect();
                let gcaps: Vec<u32> = group.iter().map(|&i| caps[i]).collect();
                for (&i, t) in group.iter().zip(self.generate(&gc, &gcaps)?) {
                    texts[i] = t;
                }
            }
        }
        Ok(texts)
    }
}

impl Recognizer for PaddleManga {
    fn info(&self) -> &RecognizerInfo {
        &self.info
    }

    fn crop(&self, page: &Bgr, quad: &Quad, _vertical: bool) -> CropSet {
        CropSet::one(paddle_quad_crop(page, quad, LINE_MARGIN_EM))
    }

    fn second_crop(&self, page: &Bgr, quad: &Quad, _vertical: bool) -> Option<CropSet> {
        Some(CropSet::one(paddle_quad_crop(page, quad, SECOND_MARGIN_EM)))
    }

    fn read(&self, lines: &[CropSet], caps: Option<&[u32]>) -> Result<Vec<String>, VlmError> {
        read_flat(lines, caps, DEFAULT_MAX_NEW_TOKENS, |crops, caps| {
            self.read_crops(crops, caps)
        })
    }
}
