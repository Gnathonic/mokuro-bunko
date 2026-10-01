//! paddle-manga (PaddleOCR-VL-1.6 + `sorryhyun/paddleocr-vl-1.6-manga-lora`, merged) on
//! ONNX Runtime (spec §6).
//!
//! Host side: `smart_resize` + Pillow bicubic, patchify, the vision graph once per crop,
//! the constant chat prompt, M-RoPE positions, left padding, and a batched greedy decode
//! with a token cap per row (the runner's truncation, ER:1913-1919).

use std::path::{Path, PathBuf};

use half::f16;
use ort::value::DynValue;

use crate::crop::{LINE_MARGIN_EM, Quad, SECOND_MARGIN_EM, paddle_quad_crop};
use crate::detok::Detokenizer;
use crate::image::{Bgr, Rgb};
use crate::npy::Npy;
use crate::pyfmt::{py_strip, round_half_even};
use crate::resample::{Filter, resize};
use crate::runtime::{SessionFactory, SessionOptions, SharedSession};
use crate::tensor::{FloatKind, argmax_rows, floats, halfs, i64s, to_f32};
use crate::{CropSet, Precision, Recognizer, RecognizerInfo, VlmError, read_flat};

pub const BASE_REPO: &str = "PaddlePaddle/PaddleOCR-VL-1.6";
pub const BASE_REVISION: &str = "c5630abae1d940eafe0697512a0325494b02ab42";
pub const LORA_REPO: &str = "sorryhyun/paddleocr-vl-1.6-manga-lora";
pub const LORA_REVISION: &str = "26292839d1469c14212a12a1e01b5b1fe01bff15";

/// Crops per generation batch (`PADDLE_BATCH`).
pub const BATCH: usize = 12;
/// Token cap when the caller gives none (`DEFAULT_MAX_NEW_TOKENS`).
pub const DEFAULT_MAX_NEW_TOKENS: u32 = 64;

const PATCH: usize = 14;
const FACTOR: usize = 28;
const MIN_PIXELS: usize = 112_896;
const MAX_PIXELS: usize = 1_003_520;
const SIDE: usize = 27;
const V_HALF: usize = 36; // vision head dim 72 / 2
const T_HD: usize = 128;
const MROPE: [usize; 3] = [16, 24, 24];
const EOS: u32 = 2;
const IMAGE_TOKEN: i64 = 100_295;
/// `<|begin_of_sentence|>User: <|IMAGE_START|>` (spec §6.6).
pub const PREFIX: [i64; 5] = [100_273, 2969, 93963, 93919, 101_305];
/// `<|IMAGE_END|>OCR:\nAssistant:\n`.
pub const SUFFIX: [i64; 8] = [101_306, 93972, 2497, 93963, 23, 92267, 93963, 23];
const KV_HEADS: usize = 2;

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
    vision: SharedSession,
    decoder: SharedSession,
    vis_in: Vec<&'static str>,
    pix_kind: FloatKind,
    vis_aux_kind: FloatKind,
    dec_kind: FloatKind,
    dec_in: Vec<DecIn>,
    n_kv: usize,
    embed: Vec<f16>,
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
        let vision = factory.open(&assets.vision, opts)?;
        let decoder = factory.open(&assets.decoder, opts)?;
        let mut vis_in = Vec::new();
        let mut pix_kind = FloatKind::F32;
        let mut vis_aux_kind = FloatKind::F32;
        for (i, n) in vision.input_names().enumerate() {
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

        let emb = Npy::load(&assets.embed)?;
        let (vocab, d_model) = match emb.shape[..] {
            [a, b] => (a, b),
            _ => return Err(VlmError::Asset("embed.npy is not 2-D".into())),
        };
        let embed = emb.into_f16()?;
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
            token_caps: true,
            default_max_tokens: DEFAULT_MAX_NEW_TOKENS,
            batch: BATCH,
            patch_budget: None,
        };
        tracing::info!(device = %opts.device, %precision, kv = n_kv, specials = detok.special_count(), "paddle-manga loaded");
        Ok(Self {
            vision,
            decoder,
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
        let out = to_f32(&self.vision.run(&refs, Vec::new())?[0])?;
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
        out.extend(
            self.embed[id * self.d_model..(id + 1) * self.d_model]
                .iter()
                .map(|h| h.to_f32()),
        );
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
        // per-row prompt embeddings and positions
        let mut rows: Vec<(Vec<f32>, Vec<[i64; 3]>)> = Vec::with_capacity(b);
        for crop in crops {
            let p = Self::prep(crop)?;
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
        let alloc = self.decoder.device_allocator()?;
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
        for group in order.chunks(BATCH) {
            let gc: Vec<&Rgb> = group.iter().map(|&i| crops[i]).collect();
            let gcaps: Vec<u32> = group.iter().map(|&i| caps[i]).collect();
            for (&i, t) in group.iter().zip(self.generate(&gc, &gcaps)?) {
                texts[i] = t;
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
