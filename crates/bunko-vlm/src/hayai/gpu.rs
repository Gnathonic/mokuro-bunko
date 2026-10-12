//! hayai-nova's GPU-resident decode on the shared WebGPU decoder graph (`crate::gpudec`,
//! built by `tools/webgpu_export`; docs/rust-port/VULKAN.md).
//!
//! Prefill: the vision tokens and bos as one call, the vision keys masked exactly like
//! the exported decoder's prefill mask (image tokens do not see bos; invalid vision tokens
//! are nobody's keys). Its keys become the batch's `ctx`; decoding then grows `gen` one
//! token per step.

use super::{BOS, D_AXIS, EOS, HEAD_DIM, HayaiNova, KV_HEADS, MAX_NEW_TOKENS, NEG, PAD, freqs};
use crate::VlmError;
use crate::gpudec::{Call, SharedDecoder};
use crate::pyfmt::py_strip;
use crate::runtime::{SessionFactory, SessionOptions};
use crate::tensor::FloatKind;

pub(crate) struct GpuDecode(SharedDecoder);

impl GpuDecode {
    pub(crate) fn load(
        factory: &dyn SessionFactory,
        decoder: &std::path::Path,
        opts: &SessionOptions,
    ) -> Result<Self, VlmError> {
        let d = SharedDecoder::load(factory, decoder, opts, 12, KV_HEADS, HEAD_DIM)?;
        tracing::info!(decoder = %decoder.display(), "hayai-nova: GPU-resident decoder");
        Ok(Self(d))
    }

    pub(crate) fn kind(&self) -> FloatKind {
        self.0.kind()
    }

    pub(crate) fn decode(
        &self,
        nova: &HayaiNova,
        b: usize,
        vis: Vec<f32>,
        outs: &[(usize, usize, usize, usize)],
        valid: &[usize],
        m: usize,
    ) -> Result<Vec<String>, VlmError> {
        // The allocator outlives every device tensor of this call (declared first).
        let alloc = self.0.allocator()?;
        let dm = nova.d_model;
        let l0 = m + 1;
        let mut x = vec![0f32; b * l0 * dm];
        let bos = &nova.emb[BOS as usize * dm..(BOS as usize + 1) * dm];
        for i in 0..b {
            x[i * l0 * dm..(i * l0 + m) * dm].copy_from_slice(&vis[i * m * dm..(i + 1) * m * dm]);
            x[(i * l0 + m) * dm..(i * l0 + m + 1) * dm].copy_from_slice(bos);
        }
        drop(vis);
        let key_bias: Vec<f32> = (0..b)
            .flat_map(|i| (0..m).map(move |j| (i, j)))
            .map(|(i, j)| if j >= valid[i] { NEG } else { 0.0 })
            .collect();
        // gen_bias (b, 1, l0, 1 + l0): the dummy key, then the exported prefill mask.
        let w = 1 + l0;
        let mut gen_bias = vec![0f32; b * l0 * w];
        for i in 0..b {
            for q in 0..l0 {
                let row = &mut gen_bias[(i * l0 + q) * w..(i * l0 + q + 1) * w];
                row[0] = NEG;
                if q < m {
                    row[1 + m] = NEG;
                }
                for j in 0..m {
                    row[1 + j] += key_bias[i * m + j];
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
                for (kk, &fk) in f.iter().enumerate() {
                    let (ay, ax) = (y * fk, xx * fk);
                    cos[row + kk] = ay.cos();
                    sin[row + kk] = ay.sin();
                    cos[row + 16 + kk] = ax.cos();
                    sin[row + 16 + kk] = ax.sin();
                }
            }
            let row = (i * l0 + m) * D_AXIS;
            cos[row..row + D_AXIS].copy_from_slice(&nova.text_cos[..D_AXIS]);
            sin[row..row + D_AXIS].copy_from_slice(&nova.text_sin[..D_AXIS]);
        }
        let dummy_ctx = self.0.dummy("ctx", b)?;
        let dummy_gen = self.0.dummy("gen", b)?;
        let (first, grow) = self.0.run(
            alloc.as_ref(),
            Call {
                b,
                s: l0,
                x,
                cos,
                sin,
                ctx: &dummy_ctx,
                ctx_bias: vec![NEG; b * l0],
                grow: &dummy_gen,
                gen_bias,
            },
            dm,
        )?;
        drop(dummy_ctx);
        let ctx = SharedDecoder::into_ctx(grow);
        // ctx keys: [dummy, vision tokens, bos]
        let ctx_bias: Vec<f32> = (0..b)
            .flat_map(|i| {
                std::iter::once(NEG)
                    .chain(key_bias[i * m..(i + 1) * m].iter().copied())
                    .chain(std::iter::once(0.0))
            })
            .collect();
        let mut grow = dummy_gen;
        let mut nxt = first;
        let (eos, pad) = (i64::from(EOS), i64::from(PAD));
        let mut toks: Vec<Vec<i64>> = nxt.iter().map(|&t| vec![t]).collect();
        let mut live: Vec<bool> = nxt.iter().map(|&t| t != eos && t != pad).collect();
        for step in 1..MAX_NEW_TOKENS {
            if !live.iter().any(|&l| l) {
                break;
            }
            let t = SharedDecoder::len(&grow);
            let mut x = Vec::with_capacity(b * dm);
            for &id in &nxt {
                let id = id as usize;
                x.extend_from_slice(&nova.emb[id * dm..(id + 1) * dm]);
            }
            let row = step * D_AXIS;
            let cos: Vec<f32> = (0..b)
                .flat_map(|_| nova.text_cos[row..row + D_AXIS].iter().copied())
                .collect();
            let sin: Vec<f32> = (0..b)
                .flat_map(|_| nova.text_sin[row..row + D_AXIS].iter().copied())
                .collect();
            let gen_bias: Vec<f32> = (0..b)
                .flat_map(|_| std::iter::once(NEG).chain(std::iter::repeat_n(0.0, t)))
                .collect();
            let (got, grown) = self.0.run(
                alloc.as_ref(),
                Call {
                    b,
                    s: 1,
                    x,
                    cos,
                    sin,
                    ctx: &ctx,
                    ctx_bias: ctx_bias.clone(),
                    grow: &grow,
                    gen_bias,
                },
                dm,
            )?;
            grow = grown;
            for i in 0..b {
                nxt[i] = if live[i] { got[i] } else { pad };
                if live[i] {
                    toks[i].push(nxt[i]);
                }
                live[i] = live[i] && nxt[i] != eos && nxt[i] != pad;
            }
        }
        drop((ctx, grow));
        Ok(toks
            .into_iter()
            .map(|t| {
                let ids: Vec<u32> = t
                    .into_iter()
                    .filter(|&t| t != eos && t != pad)
                    .map(|t| t as u32)
                    .collect();
                py_strip(&nova.detok.decode(&ids)).to_owned()
            })
            .collect())
    }
}
