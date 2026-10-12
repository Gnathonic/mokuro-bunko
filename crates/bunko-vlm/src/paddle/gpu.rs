//! paddle-manga on the WebGPU backend's graphs (`tools/webgpu_export`, `paddle.py`):
//! the vision tower run layer by layer, and the shared decoder graph (`crate::gpudec`).
//!
//! ONNX Runtime's WebGPU EP frees a run's buffers only when the run ends, so one graph
//! holds every layer's attention scores at once: one vision graph peaked at 12 GB on a
//! 2592-patch crop. Here every vision layer is its own run (its weights are graph inputs,
//! uploaded once and kept on the device), crops over `whole_max` patches run their
//! attention per query chunk, the prompt is prefilled in chunks, and batches of big crops
//! are split by rows x prompt^2. None of this changes a value: rows are independent and
//! every query still sees exactly its keys.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use ort::memory::Allocator;
use ort::value::DynValue;

use super::{KV_HEADS, N_LAYERS, T_HD, smart_resize};
use crate::VlmError;
use crate::gpudec::SharedDecoder;
use crate::runtime::{SessionFactory, SessionOptions, SharedSession, WebGpuGuard};
use crate::tensor::{self, FloatKind, floats, i64s, to_f32};

/// The vision graphs' file name for `part`.
pub fn file(part: &str) -> String {
    format!("paddle-manga_webgpu_{part}_fp32.onnx")
}

/// The layered vision tower's settings (`paddle-manga_webgpu_vision.json`).
pub const META: &str = "paddle-manga_webgpu_vision.json";

/// Elements (rows x heads x tokens x keys) one prefill call may hold per layer: ~1 GB
/// over the 18 layers of a run.
const ATTN_ELEMS: usize = 2_300_000;
/// rows x prompt^2 of one decode batch (a batch of big crops is split).
pub(crate) const BATCH_COST: usize = 12 * 256 * 256;

/// Prompt tokens of a crop of `h` x `w` pixels (prefix + image + suffix).
pub(crate) fn prompt_len(h: usize, w: usize) -> usize {
    smart_resize(h, w).map_or(usize::MAX, |(rh, rw)| 5 + (rh / 28) * (rw / 28) + 8)
}

/// Prompt tokens per prefill call for `rows` rows and `keys` keys per row.
pub(crate) fn prefill_chunk(rows: usize, keys: usize) -> usize {
    (ATTN_ELEMS / (rows * 16 * keys).max(1)).max(16)
}

struct Graph {
    s: SharedSession,
    ins: Vec<String>,
}

impl Graph {
    fn open(
        factory: &dyn SessionFactory,
        dir: &Path,
        part: &str,
        opts: &SessionOptions,
    ) -> Result<Self, VlmError> {
        let s = factory.open(&dir.join(file(part)), opts)?;
        let ins = s.input_names().map(str::to_owned).collect();
        Ok(Self { s, ins })
    }
}

/// The vision tower, layer by layer, its weights resident on the device.
pub(crate) struct LayeredVision {
    // Field order matters: the weights drop before their allocator.
    weights: HashMap<String, DynValue>,
    alloc: Mutex<Option<Allocator>>,
    embed: Graph,
    layer: Graph,
    qkv: Graph,
    attn: Graph,
    post: Graph,
    tail: Graph,
    layers: usize,
    whole_max: usize,
    chunk: usize,
}

impl Drop for LayeredVision {
    fn drop(&mut self) {
        // device tensors go back to the shared WebGPU context: under its lock
        let _g = WebGpuGuard::lock();
        self.weights.clear();
        if let Ok(mut a) = self.alloc.lock() {
            a.take();
        }
    }
}

impl LayeredVision {
    pub(crate) fn load(
        factory: &dyn SessionFactory,
        dir: &Path,
        opts: &SessionOptions,
    ) -> Result<Self, VlmError> {
        let meta: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(META))
                .map_err(|e| VlmError::Io(dir.join(META).display().to_string(), e))?,
        )
        .map_err(|e| VlmError::Asset(format!("{META}: {e}")))?;
        let num = |k: &str| -> Result<usize, VlmError> {
            meta[k]
                .as_u64()
                .map(|v| v as usize)
                .ok_or_else(|| VlmError::Asset(format!("{META}: no {k}")))
        };
        let (layers, whole_max, chunk) = (num("layers")?, num("whole_max")?, num("chunk")?);
        let g = |p| Graph::open(factory, dir, p, opts);
        let embed = g("embed")?;
        // upload: one run of the weights graph, every output preallocated on the device
        // from a long-lived session's allocator; the weights session (and its own copy of
        // the weights) is dropped right after
        let _g = WebGpuGuard::lock();
        let alloc = embed.s.device_allocator()?;
        let wg = factory.open(&dir.join(file("weights")), opts)?;
        let names: Vec<String> = wg.output_names().map(str::to_owned).collect();
        let slots = match &alloc {
            Some(a) => wg
                .session()
                .outputs()
                .iter()
                .map(|o| {
                    let dims: Vec<usize> = o
                        .dtype()
                        .tensor_shape()
                        .map(|s| s.iter().map(|&d| d.max(0) as usize).collect())
                        .unwrap_or_default();
                    tensor::alloc(a, FloatKind::F32, &dims).map(Some)
                })
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        let out = wg.run(&[], slots)?;
        let weights = names.into_iter().zip(out).collect();
        drop(wg);
        Ok(Self {
            weights,
            alloc: Mutex::new(alloc),
            embed,
            layer: g("layer")?,
            qkv: g("qkv")?,
            attn: g("attn")?,
            post: g("post")?,
            tail: g("tail")?,
            layers,
            whole_max,
            chunk,
        })
    }

    /// Runs `g` with inputs looked up by name: `own` first, then layer `layer`'s weights
    /// (`L<i>.<name>`), then the shared weights. Outputs: device tensors of the given
    /// shapes (in output order), or host values where the shape is None.
    fn run(
        &self,
        g: &Graph,
        own: &[(&str, &DynValue)],
        layer: Option<usize>,
        out_shapes: &[Option<Vec<usize>>],
    ) -> Result<Vec<DynValue>, VlmError> {
        let feeds = g
            .ins
            .iter()
            .map(|n| {
                own.iter()
                    .find(|(k, _)| k == n)
                    .map(|(_, v)| *v)
                    .or_else(|| layer.and_then(|i| self.weights.get(&format!("L{i}.{n}"))))
                    .or_else(|| self.weights.get(n))
                    .ok_or_else(|| VlmError::Runtime(format!("paddle vision: no value for {n}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let a = self.alloc.lock().unwrap_or_else(|p| p.into_inner());
        let slots = match a.as_ref() {
            Some(a) => out_shapes
                .iter()
                .map(|s| {
                    s.as_ref()
                        .map(|s| tensor::alloc(a, FloatKind::F32, s))
                        .transpose()
                })
                .collect::<Result<Vec<_>, _>>()?,
            None => Vec::new(),
        };
        drop(a);
        g.s.run(&feeds, slots)
    }

    /// Image embeddings `(n/4, 1024)` of one crop of `n` patches. The caller holds the
    /// WebGPU lock.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn forward(
        &self,
        n: usize,
        pixels: Vec<f32>,
        pos_idx: Vec<i64>,
        pos_w: Vec<f32>,
        cos: Vec<f32>,
        sin: Vec<f32>,
        merge: Vec<i64>,
    ) -> Result<Vec<f32>, VlmError> {
        const D: usize = 1152;
        let k = FloatKind::F32;
        let pv = floats(k, &[n, 3, 14, 14], pixels)?;
        let pi = i64s(&[n, 4], pos_idx)?;
        let pw = floats(k, &[n, 4], pos_w)?;
        let cos = floats(k, &[n, 72], cos)?;
        let sin = floats(k, &[n, 72], sin)?;
        let xs = Some(vec![n, D]);
        let mut x = self
            .run(
                &self.embed,
                &[("pixel_values", &pv), ("pos_idx", &pi), ("pos_w", &pw)],
                None,
                std::slice::from_ref(&xs),
            )?
            .remove(0);
        for i in 0..self.layers {
            x = if n <= self.whole_max {
                self.run(
                    &self.layer,
                    &[("x", &x), ("cos", &cos), ("sin", &sin)],
                    Some(i),
                    std::slice::from_ref(&xs),
                )?
                .remove(0)
            } else {
                let mut qkv = self
                    .run(
                        &self.qkv,
                        &[("x", &x), ("cos", &cos), ("sin", &sin)],
                        Some(i),
                        &[
                            Some(vec![16, n, 72]),
                            Some(vec![16, 72, n]),
                            Some(vec![16, n, 72]),
                        ],
                    )?
                    .into_iter();
                let (q, kt, v) = (qkv.next(), qkv.next(), qkv.next());
                let (Some(q), Some(kt), Some(v)) = (q, kt, v) else {
                    return Err(VlmError::Runtime(
                        "paddle vision: qkv returned too few outputs".into(),
                    ));
                };
                let mut o = Vec::with_capacity(n * D);
                for a in (0..n).step_by(self.chunk) {
                    let s = i64s(&[1], vec![a as i64])?;
                    let e = i64s(&[1], vec![(a + self.chunk).min(n) as i64])?;
                    let r = self.run(
                        &self.attn,
                        &[
                            ("q", &q),
                            ("kT", &kt),
                            ("v", &v),
                            ("start", &s),
                            ("end", &e),
                        ],
                        None,
                        &[None],
                    )?;
                    o.extend(to_f32(&r[0])?);
                }
                drop((q, kt, v));
                let o = floats(k, &[n, D], o)?;
                self.run(
                    &self.post,
                    &[("x", &x), ("o", &o)],
                    Some(i),
                    std::slice::from_ref(&xs),
                )?
                .remove(0)
            };
        }
        let merge = i64s(&[n], merge)?;
        let out = self.run(
            &self.tail,
            &[("x", &x), ("merge_idx", &merge)],
            None,
            &[None],
        )?;
        to_f32(&out[0])
    }
}

/// paddle-manga's WebGPU graphs: the layered vision tower and the shared decoder.
pub(crate) struct PaddleGpu {
    pub(crate) vision: LayeredVision,
    pub(crate) decoder: SharedDecoder,
}

impl PaddleGpu {
    pub(crate) fn load(
        factory: &dyn SessionFactory,
        dir: &Path,
        opts: &SessionOptions,
    ) -> Result<Self, VlmError> {
        let decoder = SharedDecoder::load(
            factory,
            &dir.join(file("decoder")),
            opts,
            N_LAYERS,
            KV_HEADS,
            T_HD,
        )?;
        let vision = LayeredVision::load(factory, dir, opts)?;
        tracing::info!(dir = %dir.display(), "paddle-manga: WebGPU graphs (layered vision, shared decoder)");
        Ok(Self { vision, decoder })
    }
}
