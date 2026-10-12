"""paddle-manga (PaddleOCR-VL 1.6 + the manga LoRA, merged) as ncnn graphs, fp32.

    python -m ncnn_export.paddle --out DIR      (export env: torch + transformers + peft)

* ``vision`` (one crop): ``pv`` (588, N) patches, ``pos`` (1152, N) the interpolated
  position embedding rows (host: 4 taps of ``pos_table.f32``), ``cos``/``sin`` (36, N) the
  2D RoPE halves, ``sel`` (N, 4M) the 2x2 merge as a 0/1 matrix -> ``vis`` (1024, M).
* ``prefill`` (b crops, left-padded to S): ``x`` (1024, b*S), ``mask`` (S, S, b) additive
  per row, ``cos``/``sin`` (64, b*S) M-RoPE halves -> ``ids`` (b), ``k{i}``/``v{i}``
  (128, S, 2b).
* ``step``: ``x`` (1024, b), ``mask`` (T, 1, b), ``cos``/``sin`` (64, b), ``pk{i}``/``pv{i}``
  (128, T-1, 2b) -> ``ids`` (b), ``nk{i}``/``nv{i}`` (128, T, 2b).

Attention is bunko-ncnn's ``BnAttn`` (query rows in chunks: a crop of 5120 patches would
need 1.7 GB for the whole score matrix), linears ``BnGemm``, the argmax ``BnArgmax``
(103,424 logits per row stay on the GPU). prefill and step share ``decoder.bin``.
Arithmetic as ``tools/onnx_export/onnx_export/paddle/modules.py`` in fp32; q|k|v and
gate|up fused.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np

from .graph import Graph

D_V, H_V, HD_V = 1152, 16, 72
D_T, H_Q, H_KV, HD_T, FFN = 1024, 16, 2, 128, 3072


def _np(t) -> np.ndarray:
    return t.detach().float().cpu().numpy()


def vision(model) -> Graph:
    vm = model.model.visual.vision_model
    pj = model.model.projector
    g = Graph("bngemm")
    pv, pos, cos, sin, sel = (g.input(n) for n in ("pv", "pos", "cos", "sin", "sel"))
    pe = vm.embeddings.patch_embedding
    h = g.binary(g.linear(pv, _np(pe.weight).reshape(D_V, -1), _np(pe.bias)), pos, 0)
    for L in vm.encoder.layers:
        a = L.self_attn
        x = g.layernorm(h, _np(L.layer_norm1.weight), _np(L.layer_norm1.bias), L.layer_norm1.eps)
        w = np.concatenate([_np(a.q_proj.weight), _np(a.k_proj.weight), _np(a.v_proj.weight)])
        b = np.concatenate([_np(a.q_proj.bias), _np(a.k_proj.bias), _np(a.v_proj.bias)])
        q, k, v = g.slice(g.linear(x, w, b), [D_V, D_V, D_V], 1)
        q, k, v = (g.permute(g.reshape(t, w=HD_V, h=H_V, c=-1), 2) for t in (q, k, v))  # (72, N, 16)
        q = g.one("RotaryEmbed", [q, cos, sin], {0: 0})
        k = g.one("RotaryEmbed", [k, cos, sin], {0: 0})
        o = g.reshape(g.permute(g.attn(q, k, v, float(a.scaling)), 2), w=D_V, h=-1)
        h = g.binary(h, g.linear(o, _np(a.out_proj.weight), _np(a.out_proj.bias)), 0)
        m = L.mlp
        x = g.layernorm(h, _np(L.layer_norm2.weight), _np(L.layer_norm2.bias), L.layer_norm2.eps)
        x = g.one("GELU", g.linear(x, _np(m.fc1.weight), _np(m.fc1.bias)), {0: 1})  # GELUTanh
        h = g.binary(h, g.linear(x, _np(m.fc2.weight), _np(m.fc2.bias)), 0)
    h = g.layernorm(h, _np(vm.post_layernorm.weight), _np(vm.post_layernorm.bias), vm.post_layernorm.eps)
    h = g.layernorm(h, _np(pj.pre_norm.weight), _np(pj.pre_norm.bias), pj.pre_norm.eps)
    # merge: rows (t, k) of the gather -> (M, 4*1152) patch-major, as x[merge].reshape(-1, 4608)
    gth = g.reshape(g.one("Gemm", [sel, h], {0: 1.0, 1: 1.0, 2: 0, 3: 0}), w=4 * D_V, h=-1)
    x = g.one("GELU", g.linear(gth, _np(pj.linear_1.weight), _np(pj.linear_1.bias)), {0: 0})
    g.linear(x, _np(pj.linear_2.weight), _np(pj.linear_2.bias), outputs=["vis"], name="vis")
    return g


def decoder(model, step: bool) -> Graph:
    lm = model.model.language_model
    g = Graph("bngemm")
    x, mask, cos, sin = (g.input(n) for n in ("x", "mask", "cos", "sin"))

    def heads(t, n):  # (rows, n*128) -> (128, rows, n): RoPE per row
        return g.permute(g.reshape(t, w=HD_T, h=n, c=-1), 2)

    def to_attn(t, n):  # (128, b*S, n) -> (128, S, b, n) -> (128, S, n, b) -> (128, S, n*b)
        t = g.one("Reshape", [t, mask], {6: f"{HD_T},1h,-1,{n}"})
        return g.reshape(g.permute(t, 6), w=0, h=0, c=-1)

    for i, L in enumerate(lm.layers):
        a, m = L.self_attn, L.mlp
        h = g.rmsnorm(x, _np(L.input_layernorm.weight), L.input_layernorm.variance_epsilon)
        w = np.concatenate([_np(a.q_proj.weight), _np(a.k_proj.weight), _np(a.v_proj.weight)])
        q, k, v = g.slice(g.linear(h, w), [H_Q * HD_T, H_KV * HD_T, H_KV * HD_T], 1)
        q = to_attn(g.one("RotaryEmbed", [heads(q, H_Q), cos, sin], {0: 0}), H_Q)
        k = to_attn(g.one("RotaryEmbed", [heads(k, H_KV), cos, sin], {0: 0}), H_KV)
        v = to_attn(heads(v, H_KV), H_KV)
        if step:
            k = g.one("Concat", [g.input(f"pk{i}"), k], {0: 1}, outputs=[f"nk{i}"], name=f"nk{i}")
            v = g.one("Concat", [g.input(f"pv{i}"), v], {0: 1}, outputs=[f"nv{i}"], name=f"nv{i}")
        else:
            k = g.one("Noop", k, outputs=[f"k{i}"], name=f"k{i}")
            v = g.one("Noop", v, outputs=[f"v{i}"], name=f"v{i}")
        o = g.attn(q, k, v, float(a.scaling), mask=mask, mask_div=H_Q)  # (128, S, 16b)
        o = g.reshape(g.permute(g.reshape(o, w=0, h=0, d=H_Q, c=-1), 2), w=H_Q * HD_T, h=-1)
        x = g.binary(x, g.linear(o, _np(a.o_proj.weight)), 0)
        h = g.rmsnorm(x, _np(L.post_attention_layernorm.weight), L.post_attention_layernorm.variance_epsilon)
        gt, up = g.slice(g.linear(h, np.concatenate([_np(m.gate_proj.weight), _np(m.up_proj.weight)])),
                         [FFN, FFN], 1)
        x = g.binary(x, g.linear(g.binary(g.one("Swish", gt), up, 2), _np(m.down_proj.weight)), 0)
    if not step:  # each crop's last position: (1024, b*S) -> (1024, S, b) -> (1024, 1, b) -> (1024, b)
        x = g.one("Reshape", [x, mask], {6: f"{D_T},1h,-1"})
        x = g.reshape(g.one("Crop", x, {9: [-1], 10: [2 ** 30], 11: [1]}), w=D_T, h=-1)
    x = g.rmsnorm(x, _np(lm.norm.weight), lm.norm.variance_epsilon)
    logits = g.linear(x, _np(model.lm_head.weight), outputs=["logits"], name="logits")
    g.one("BnArgmax", logits, outputs=["ids"], name="ids")
    return g


def main(argv=None) -> int:
    import sys

    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args(argv)
    repo = Path(__file__).resolve().parents[3]
    sys.path.insert(0, str(repo / "tools" / "onnx_export"))
    from onnx_export.paddle.export import load_model, prompt_ids

    model, proc = load_model("float32")
    model = model.eval()
    a.out.mkdir(parents=True, exist_ok=True)
    vision(model).save(a.out / "vision.param", a.out / "vision.bin")
    decoder(model, False).save_bins_shared(decoder(model, True), a.out / "prefill.param", a.out / "step.param",
                                           a.out / "decoder.bin")
    lm = model.model.language_model
    _np(lm.embed_tokens.weight).astype(np.float32).tofile(a.out / "embeddings.f32")
    pos = model.model.visual.vision_model.embeddings.position_embedding.weight
    _np(pos).astype(np.float32).tofile(a.out / "pos_table.f32")
    pre, suf = prompt_ids(proc)
    (a.out / "ncnn.json").write_text(json.dumps({
        "engine": "paddle-manga", "precision": "fp32", "format": 2,
        "prompt": {"prefix": pre, "suffix": suf}, "vocab": int(model.lm_head.weight.shape[0]),
        "embeddings": list(lm.embed_tokens.weight.shape), "pos_table": list(pos.shape)}) + "\n")
    print("wrote", a.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
