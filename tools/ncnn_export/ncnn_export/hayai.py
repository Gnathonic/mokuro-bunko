"""hayai-nova as ncnn graphs, written straight from the checkpoint's weights.

    python -m ncnn_export.hayai --out DIR      (export env: torch + transformers)

Layouts (ncnn Mat: w innermost, then h, c; ncnn has no batch axis):

* ``vision`` (one crop, unpadded): ``pv`` (768, P) and ``pos`` (768, P) for its P = hp*wp
  patches, ``sel`` (P, 4M) the projector's 2x2 gather as a 0/1 matrix (one 1 per row:
  exact) -> ``vis`` (512, M). No padding, so no attention mask.
* ``prefill`` (the batch of b crops at once): ``x`` (512, b*L0) — each crop's vision
  tokens, zero padding to the batch's longest crop, then BOS — ``mask`` (L0, L0, 8b) per
  head, ``cos``/``sin`` (32, b*L0) -> ``ids`` (b) the first generated ids (in-graph
  argmax), ``k{i}``/``v{i}`` (64, L0, 2b): already the step's KV layout.
* ``step`` (batch folded into the heads: channel = row*heads + head, which is exactly
  ``repeat_interleave``'s query->KV head mapping): ``x`` (512, b), ``mask`` (L, 1, 8b),
  ``cos``/``sin`` (32, 1), ``pk{i}``/``pv{i}`` (64, L-1, 2b) -> ``ids`` (b),
  ``nk{i}``/``nv{i}`` (64, L, 2b). The KV cache is SDPA's own ``kv_cache`` concat, so it
  never leaves the device between steps.

prefill and step share ``decoder.bin`` (same weights, same order). Linears are
bunko-ncnn's ``BnGemm`` (weight id = CRC of the weights, so the two graphs' copies share
one GPU buffer), attention in the vision graph is ``BnAttn``, the argmax ``BnArgmax``.

Arithmetic is the 0.5.2 modules' (``tools/onnx_export/onnx_export/hayai/modules.py``)
in fp32, except: q|k|v and gate|up are one GEMM each, and the per-channel residual
scales are folded into ``w_o`` / ``w_down`` (text parity is the bar, not bit parity).
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np

from .graph import Graph

D_V, H_V, D_M, H_Q, H_KV, D_H, VOCAB = 768, 12, 512, 8, 2, 64, 16004
EPS_V, EPS_P, EPS_R = 1e-6, 1e-5, 1e-6


def _np(t) -> np.ndarray:
    return t.detach().float().cpu().numpy()


def vision(model, linear: str = "gemm") -> Graph:
    vm = model.vision_encoder.vision_model if hasattr(model.vision_encoder, "vision_model") else model.vision_encoder
    pj = model.decoder.projector
    g = Graph(linear)
    pv, pos, sel = g.input("pv"), g.input("pos"), g.input("sel")
    pe = vm.embeddings.patch_embedding
    h = g.binary(g.linear(pv, _np(pe.weight), _np(pe.bias)), pos, 0)
    for L in vm.encoder.layers:
        a = L.self_attn
        x = g.layernorm(h, _np(L.layer_norm1.weight), _np(L.layer_norm1.bias), EPS_V)
        w = np.concatenate([_np(a.q_proj.weight), _np(a.k_proj.weight), _np(a.v_proj.weight)])
        b = np.concatenate([_np(a.q_proj.bias), _np(a.k_proj.bias), _np(a.v_proj.bias)])
        q, k, v = g.slice(g.linear(x, w, b), [D_V, D_V, D_V], 1)
        q, k, v = (g.permute(g.reshape(t, w=D_H, h=H_V, c=-1), 2) for t in (q, k, v))
        o = g.attn(q, k, v, float(a.scale))
        o = g.reshape(g.permute(o, 2), w=D_V, h=-1)
        h = g.binary(h, g.linear(o, _np(a.out_proj.weight), _np(a.out_proj.bias)), 0)
        m = L.mlp
        x = g.layernorm(h, _np(L.layer_norm2.weight), _np(L.layer_norm2.bias), EPS_V)
        x = g.one("GELU", g.linear(x, _np(m.fc1.weight), _np(m.fc1.bias)), {0: 1})
        h = g.binary(h, g.linear(x, _np(m.fc2.weight), _np(m.fc2.bias)), 0)
    h = g.layernorm(h, _np(vm.post_layernorm.weight), _np(vm.post_layernorm.bias), EPS_V)
    # (4M, P) @ (P, 768) -> (4M, 768) rows (t, k) -> (M, 4, 768) -> (M, 768, 4) -> (M, 3072)
    gth = g.one("Gemm", [sel, h], {0: 1.0, 1: 1.0, 2: 0, 3: 0})
    gth = g.reshape(g.permute(g.reshape(gth, w=D_V, h=4, c=-1), 1), w=4 * D_V, h=-1)
    x = g.layernorm(gth, _np(pj.norm.weight), _np(pj.norm.bias), EPS_P)
    x = g.one("GELU", g.linear(x, _np(pj.mlp[0].weight)), {0: 0})
    x = g.linear(x, _np(pj.mlp[2].weight))
    g.one("RMSNorm", x, {0: D_M, 1: float(pj.out_norm.eps), 2: 1}, [(_np(pj.out_norm.weight), False)],
          outputs=["vis"], name="vis")
    return g


def _decoder_layer(g: Graph, L, x: str, heads_view, cos: str, sin: str, mask: str,
                   past: tuple[str, str] | None, kv_names: tuple[str, str], to_sdpa=lambda t, n: t):
    at = L.attn
    h = g.rmsnorm(x, _np(L.attn_norm.weight), EPS_R)
    w = np.concatenate([_np(at.w_q.weight), _np(at.w_k.weight), _np(at.w_v.weight)])
    q, k, v = g.slice(g.linear(h, w), [H_Q * D_H, H_KV * D_H, H_KV * D_H], 1)
    q = g.one("RotaryEmbed", [heads_view(q, H_Q, _np(at.q_norm.weight)), cos, sin], {0: 1})
    k = g.one("RotaryEmbed", [heads_view(k, H_KV, _np(at.k_norm.weight)), cos, sin], {0: 1})
    v = heads_view(v, H_KV, None)
    q, k, v = to_sdpa(q, H_Q), to_sdpa(k, H_KV), to_sdpa(v, H_KV)
    if past is None:
        k = g.one("Noop", k, outputs=[kv_names[0]], name=kv_names[0])
        v = g.one("Noop", v, outputs=[kv_names[1]], name=kv_names[1])
        o = g.one("SDPA", [q, k, v, mask], {5: 1, 6: D_H ** -0.5, 7: 0})
    else:
        o, _, _ = g.add("SDPA", [q, k, v, mask, *past], {5: 1, 6: D_H ** -0.5, 7: 1},
                        outputs=[g._name("sdpa"), *kv_names])
    o = heads_view(o, None, None)  # back to (512, rows)
    s_a = _np(L.attn_res_scale)[:, None]
    x = g.binary(x, g.linear(o, _np(at.w_o.weight) * s_a), 0)
    f = L.ffn
    h = g.rmsnorm(x, _np(L.ffn_norm.weight), EPS_R)
    gt, up = g.slice(g.linear(h, np.concatenate([_np(f.w_gate.weight), _np(f.w_up.weight)])), [2048, 2048], 1)
    h = g.binary(g.one("Swish", gt), up, 2)
    s_f = _np(L.ffn_res_scale)[:, None]
    return g.binary(x, g.linear(h, _np(f.w_down.weight) * s_f), 0)


def decoder(model, step: bool, linear: str = "gemm", batch: bool = False) -> Graph:
    """``step``: the decode step; else the prefill of one crop, or with ``batch`` of b
    crops at once: ``x`` (512, b*L0) rows crop-major, ``mask`` (L0, L0, 8b) per head,
    ``cos``/``sin`` (32, b*L0) -> ``logits`` (16004, b), ``k{i}``/``v{i}`` (64, L0, 2b),
    the step's KV layout. Token-wise ops run on all b*L0 rows; only attention regroups
    them per crop (the mask's dims carry L0 into the reshapes)."""
    dec = model.decoder
    g = Graph(linear)
    if step or batch:
        x = g.input("x")
    else:  # the crop's vision tokens straight from the vision graph, then padding + BOS
        x = g.one("Concat", [g.input("vis"), g.input("tail")], {0: 0})
    mask, cos, sin = g.input("mask"), g.input("cos"), g.input("sin")
    to_sdpa = lambda t, n: t  # noqa: E731

    if step:
        def heads_view(t, n, norm_w):
            if n is None:  # (8b, 1, 64) -> (512, b)
                return g.reshape(t, w=D_M, h=-1)
            t = g.reshape(t, w=D_H, h=1, c=-1)  # (b, n*64) -> (64, 1, n*b)
            return g.rmsnorm(t, norm_w, EPS_R) if norm_w is not None else t
    else:
        def heads_view(t, n, norm_w):
            if n is None:  # (8, L0, 64) -> (L0, 8, 64) -> (512, L0)
                return g.reshape(g.permute(t, 2), w=D_M, h=-1)
            t = g.reshape(t, w=D_H, h=n, c=-1)  # (L0, n*64) -> (64, n, L0)
            if norm_w is not None:
                t = g.rmsnorm(t, norm_w, EPS_R)
            return g.permute(t, 2)  # -> (64, L0, n)

    if batch:
        def heads_view(t, n, norm_w):
            if n is None:  # (64, L0, 8b) -> (64, L0, 8, b) -> (64, 8, L0, b) -> (512, b*L0)
                t = g.reshape(t, w=0, h=0, d=H_Q, c=-1)
                return g.reshape(g.permute(t, 2), w=D_M, h=-1)
            t = g.reshape(t, w=D_H, h=n, c=-1)  # (b*L0, n*64) -> (64, n, b*L0)
            if norm_w is not None:
                t = g.rmsnorm(t, norm_w, EPS_R)
            return g.permute(t, 2)  # -> (64, b*L0, n): RoPE per row

        def to_sdpa(t, n):  # (64, b*L0, n) -> (64, L0, b, n) -> (64, L0, n, b) -> (64, L0, n*b)
            t = g.one("Reshape", [t, mask], {6: f"64,1w,-1,{n}"})
            return g.reshape(g.permute(t, 6), w=0, h=0, c=-1)

    for i, L in enumerate(dec.layers):
        past = (g.input(f"pk{i}"), g.input(f"pv{i}")) if step else None
        names = (f"nk{i}", f"nv{i}") if step else (f"k{i}", f"v{i}")
        x = _decoder_layer(g, L, x, heads_view, cos, sin, mask, past, names, to_sdpa)
    if batch:  # each crop's last position: (512, b*L0) -> (512, L0, b) -> (512, 1, b) -> (512, b)
        x = g.one("Reshape", [x, mask], {6: "512,1w,-1"})
        x = g.reshape(g.one("Crop", x, {9: [-1], 10: [2 ** 30], 11: [1]}), w=D_M, h=-1)
    elif not step:  # the last position only
        x = g.one("Crop", x, {9: [-1], 10: [2 ** 30], 11: [0]})
    x = g.rmsnorm(x, _np(dec.final_norm.weight), EPS_R)
    logits = g.linear(x, _np(dec.output_head.weight), outputs=["logits"], name="logits")
    g.one("BnArgmax", logits, outputs=["ids"], name="ids")  # (b,) next ids, first index on ties
    return g


def main(argv=None) -> int:
    import sys

    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--linear", choices=["bngemm", "gemm", "innerproduct"], default="bngemm")
    a = ap.parse_args(argv)
    repo = Path(__file__).resolve().parents[3]
    sys.path.insert(0, str(repo / "tools" / "onnx_export"))
    from onnx_export.hayai.export import load_model

    model, _tok = load_model()
    model = model.eval()
    a.out.mkdir(parents=True, exist_ok=True)
    vision(model, a.linear).save(a.out / "vision.param", a.out / "vision.bin")
    # prefill (batched) and step carry the same weights in the same order: one decoder.bin
    pre, step = decoder(model, False, a.linear, batch=True), decoder(model, True, a.linear)
    pre.save_bins_shared(step, a.out / "prefill.param", a.out / "step.param", a.out / "decoder.bin")
    _np(model.decoder.token_embeddings.weight).astype(np.float32).tofile(a.out / "token_embeddings.f32")
    (a.out / "ncnn.json").write_text(json.dumps({"engine": "hayai-nova", "precision": "fp32", "format": 2}) + "\n")
    print("wrote", a.out)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
