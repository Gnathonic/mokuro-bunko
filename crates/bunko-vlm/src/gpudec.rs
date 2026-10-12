//! The shared decoder graph of the WebGPU recognizers (`tools/webgpu_export`, `graph.py`):
//! one graph serves the prefill call and every decode step, with the KV cache in device
//! memory the whole time.
//!
//! Two KV regions per layer: `ctx` (the prompt's keys/values, written by the prefill
//! call, then read in place) and `gen` (the generated tokens', grown by one column per
//! step). A region that would be empty holds a one-key zero dummy, masked out by its
//! bias. Per call only the new tokens' embeddings, their RoPE tables and the biases go
//! to the device, and only the next token ids come back.

use std::collections::HashMap;
use std::path::Path;

use ort::memory::Allocator;
use ort::value::DynValue;

use crate::VlmError;
use crate::runtime::{SessionFactory, SessionOptions, SharedSession};
use crate::tensor::{self, FloatKind, floats, i64s};

/// A layer's keys (transposed) and values of one region, by graph input name.
pub(crate) type Kv = HashMap<String, DynValue>;

pub(crate) struct SharedDecoder {
    session: SharedSession,
    inputs: Vec<String>,
    outputs: Vec<String>,
    kind: FloatKind,
    layers: usize,
    kv_heads: usize,
    head_dim: usize,
}

impl std::fmt::Debug for SharedDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedDecoder")
            .field("layers", &self.layers)
            .finish_non_exhaustive()
    }
}

/// One call's inputs (row-major f32; `s` tokens per row).
pub(crate) struct Call<'a> {
    pub b: usize,
    pub s: usize,
    /// (b, s, d) embeddings.
    pub x: Vec<f32>,
    /// (b, s, R) RoPE cos / sin, R = head_dim / 2.
    pub cos: Vec<f32>,
    pub sin: Vec<f32>,
    pub ctx: &'a Kv,
    /// (b, 1, s, C).
    pub ctx_bias: Vec<f32>,
    pub grow: &'a Kv,
    /// (b, 1, s, t + s).
    pub gen_bias: Vec<f32>,
}

impl SharedDecoder {
    pub(crate) fn load(
        factory: &dyn SessionFactory,
        path: &Path,
        opts: &SessionOptions,
        layers: usize,
        kv_heads: usize,
        head_dim: usize,
    ) -> Result<Self, VlmError> {
        let session = factory.open(path, opts)?;
        let inputs: Vec<String> = session.input_names().map(str::to_owned).collect();
        let outputs: Vec<String> = session.output_names().map(str::to_owned).collect();
        let want_in = 6 + 4 * layers;
        if outputs.first().map(String::as_str) != Some("next")
            || outputs.len() != 1 + 2 * layers
            || inputs.len() != want_in
        {
            return Err(VlmError::Asset(format!(
                "{}: not a WebGPU decoder graph of {layers} layers",
                path.display()
            )));
        }
        let kind = FloatKind::of(session.input_type(0), "decoder input")?;
        Ok(Self {
            session,
            inputs,
            outputs,
            kind,
            layers,
            kv_heads,
            head_dim,
        })
    }

    pub(crate) fn kind(&self) -> FloatKind {
        self.kind
    }

    /// The EP's device allocator. Every device tensor taken from it must be dropped
    /// before it (a tensor keeps only a raw pointer to its OrtAllocator).
    pub(crate) fn allocator(&self) -> Result<Option<Allocator>, VlmError> {
        self.session.device_allocator()
    }

    /// A region holding the masked one-key dummy (`prefix` = `ctx` or `gen`), in host
    /// memory (ONNX Runtime uploads it).
    pub(crate) fn dummy(&self, prefix: &str, b: usize) -> Result<Kv, VlmError> {
        let (kh, hd) = (self.kv_heads, self.head_dim);
        let mut kv = Kv::new();
        for i in 0..self.layers {
            kv.insert(
                format!("{prefix}_kT{i}"),
                floats(self.kind, &[b, kh, hd, 1], vec![0.0; b * kh * hd])?,
            );
            kv.insert(
                format!("{prefix}_v{i}"),
                floats(self.kind, &[b, kh, 1, hd], vec![0.0; b * kh * hd])?,
            );
        }
        Ok(kv)
    }

    /// Length of a region (keys per row).
    pub(crate) fn len(kv: &Kv) -> usize {
        kv.get("gen_v0")
            .or_else(|| kv.get("ctx_v0"))
            .and_then(|v| v.shape().get(2).copied())
            .unwrap_or(0) as usize
    }

    /// Runs one call; returns each row's next token and the grown `gen` region.
    pub(crate) fn run(
        &self,
        alloc: Option<&Allocator>,
        c: Call<'_>,
        d_model: usize,
    ) -> Result<(Vec<i64>, Kv), VlmError> {
        let (b, s, k) = (c.b, c.s, self.kind);
        let (kh, hd) = (self.kv_heads, self.head_dim);
        let n_ctx = Self::len(c.ctx);
        let t = Self::len(c.grow);
        let check = |what: &str, got: usize, want: usize| {
            (got == want).then_some(()).ok_or_else(|| {
                VlmError::Runtime(format!(
                    "decoder call: {what} has {got} values, expected {want}"
                ))
            })
        };
        check("x", c.x.len(), b * s * d_model)?;
        check("cos", c.cos.len(), b * s * hd / 2)?;
        check("ctx_bias", c.ctx_bias.len(), b * s * n_ctx)?;
        check("gen_bias", c.gen_bias.len(), b * s * (t + s))?;
        let own = [
            ("x", floats(k, &[b, s, d_model], c.x)?),
            ("cos", floats(k, &[b, s, hd / 2], c.cos)?),
            ("sin", floats(k, &[b, s, hd / 2], c.sin)?),
            ("pos", i64s(&[b, s], (0..(b * s) as i64).collect())?),
            ("ctx_bias", floats(k, &[b, 1, s, n_ctx], c.ctx_bias)?),
            ("gen_bias", floats(k, &[b, 1, s, t + s], c.gen_bias)?),
        ];
        let feeds = self
            .inputs
            .iter()
            .map(|n| {
                own.iter()
                    .find(|(m, _)| m == n)
                    .map(|(_, v)| v)
                    .or_else(|| c.ctx.get(n))
                    .or_else(|| c.grow.get(n))
                    .ok_or_else(|| VlmError::Runtime(format!("decoder call: no value for {n}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let slots: Vec<Option<DynValue>> = match alloc {
            None => Vec::new(),
            Some(a) => self
                .outputs
                .iter()
                .map(|n| {
                    if n.starts_with("gen_kT") {
                        tensor::alloc(a, k, &[b, kh, hd, t + s]).map(Some)
                    } else if n.starts_with("gen_v") {
                        tensor::alloc(a, k, &[b, kh, t + s, hd]).map(Some)
                    } else {
                        Ok(None)
                    }
                })
                .collect::<Result<_, _>>()?,
        };
        let out = self.session.run(&feeds, slots)?;
        drop(feeds);
        let mut out = out.into_iter();
        let next = out
            .next()
            .ok_or_else(|| VlmError::Runtime("decoder returned nothing".into()))?;
        let next = next
            .try_extract_tensor::<i64>()
            .map_err(|e| VlmError::Runtime(e.to_string()))?
            .1
            .to_vec();
        if next.len() != b {
            return Err(VlmError::Runtime(format!(
                "decoder returned {} tokens for {b} rows",
                next.len()
            )));
        }
        let grow = self.outputs[1..]
            .iter()
            .map(|n| n.trim_end_matches("_o").to_owned())
            .zip(out)
            .collect();
        Ok((next, grow))
    }

    /// The prefill's grown `gen` region becomes the batch's `ctx`.
    pub(crate) fn into_ctx(grow: Kv) -> Kv {
        grow.into_iter()
            .map(|(k, v)| (k.replacen("gen_", "ctx_", 1), v))
            .collect()
    }
}
