"""hayai-nova graphs for the WebGPU backend: the vision tower + projector, and the shared decoder.

Same math as `tools/onnx_export/onnx_export/hayai/modules.py` (the parity reference), with
fused residual norms (SkipLayerNormalization), FastGelu for gelu-tanh, GatherND for the
projector gather, and the 1/8 attention scale folded into the query weights (a power of
two: bit-identical). Attention stays decomposed: the EP's MultiHeadAttention (flash) is
faster but not fp32-exact (18/3203 lines differ on a full volume).
"""

from __future__ import annotations

import os

import numpy as np
from onnx import TensorProto, helper

from .graph import Decoder, G, Layer, decoder_graph

REPO = "JustANormalTinkerer/hayai-ocr-v2.5-nova"
REVISION = "e46d79138499600564f810d44ab6bdea7230dee1"
NH, DM = 12, 768


def load_model():
    from transformers import AutoModel

    os.environ.setdefault("HF_HUB_OFFLINE", "1")
    return AutoModel.from_pretrained(REPO, revision=REVISION, trust_remote_code=True).eval()


def _t(p):
    return p.detach().float().numpy()


def decoder(model, splits=None) -> Decoder:
    dec = model.decoder
    layers = []
    for layer in dec.layers:
        at, ffn = layer.attn, layer.ffn
        layers.append(Layer(
            attn_norm=_t(layer.attn_norm.weight), ffn_norm=_t(layer.ffn_norm.weight),
            wq=_t(at.w_q.weight).T, wk=_t(at.w_k.weight).T, wv=_t(at.w_v.weight).T, wo=_t(at.w_o.weight).T,
            wg=_t(ffn.w_gate.weight).T, wu=_t(ffn.w_up.weight).T, wd=_t(ffn.w_down.weight).T,
            q_norm=_t(at.q_norm.weight), k_norm=_t(at.k_norm.weight),
            attn_res=_t(layer.attn_res_scale), ffn_res=_t(layer.ffn_res_scale),
        ))
    at = dec.layers[0].attn
    eps = {dec.layers[0].attn_norm.eps, dec.layers[0].ffn_norm.eps, dec.final_norm.eps}
    assert len(eps) == 1, eps
    return Decoder(
        layers=layers, final_norm=_t(dec.final_norm.weight), head=_t(dec.output_head.weight).T,
        heads=at.h_q, kv_heads=at.h_kv, head_dim=at.d_head, eps=eps.pop(), qk_eps=at.q_norm.eps,
        interleaved=True, scale=0.125, splits=splits or {"D": 8, "F": 16, "head": 1},
    )


def vision_graph(model, dt=np.float32, mha=False):
    vm = model.vision_encoder.vision_model if hasattr(model.vision_encoder, "vision_model") else model.vision_encoder
    pj = model.decoder.projector
    g = G(dt)
    c = g.c
    tp = g.tp
    inputs = [
        helper.make_tensor_value_info("pixel_values", tp, ["b", "P", 768]),
        helper.make_tensor_value_info("pixel_mask", tp, ["b", "P"]),
        helper.make_tensor_value_info("pos", tp, ["b", "P", 768]),
        helper.make_tensor_value_info("gather_idx", TensorProto.INT64, ["b", "M", 4]),
        helper.make_tensor_value_info("tok_valid", tp, ["b", "M"]),
    ]
    outputs = [helper.make_tensor_value_info("vis_tokens", tp, ["b", "M", 512])]
    pe = vm.embeddings.patch_embedding
    h = g.op("Add", [g.op("Add", [g.op("MatMul", ["pixel_values", c(_t(pe.weight).T)]), c(_t(pe.bias))]), "pos"])
    kb = g.op("Where", [g.op("Greater", ["pixel_mask", c(0.5)]), c(0.0), c(-1e9)])  # (b,P)
    kb = g.op("Unsqueeze", [kb, g.i64([1, 2])])  # (b,1,1,P)
    if mha:
        p_dim = g.op("Shape", ["pixel_mask"], start=1, end=2)
        bias = g.op("Expand", [kb, g.op("Concat", [g.i64([1, 1]), p_dim, p_dim], axis=0)])
    layers = vm.encoder.layers
    ln = lambda m: (_t(m.weight), _t(m.bias), m.eps)
    l1 = ln(layers[0].layer_norm1)
    x = g.op("LayerNormalization", [h, c(l1[0]), c(l1[1])], axis=-1, epsilon=l1[2])
    resid = h
    hs = g.i64([0, 0, NH, DM // NH])
    for i, layer in enumerate(layers):
        a = layer.self_attn
        assert a.num_heads == NH and float(a.scale) == 0.125, (a.num_heads, a.scale)
        wq, wk, wv = _t(a.q_proj.weight).T, _t(a.k_proj.weight).T, _t(a.v_proj.weight).T
        bq, bk, bv = _t(a.q_proj.bias), _t(a.k_proj.bias), _t(a.v_proj.bias)
        if mha:
            qkv = g.op("MatMul", [x, c(np.concatenate([wq, wk, wv], 1))])
            q, k, v = g.op("Split", [qkv, g.i64([DM, DM, DM])], n_out=3, axis=-1)
            o = g.op("MultiHeadAttention", [q, k, v, c(np.concatenate([bq, bk, bv])), "", bias],
                     domain="com.microsoft", num_heads=NH, scale=0.125)
        else:
            # three MatMuls (no Split copies); the 1/8 scale folded into the query
            # projection: a power of two, so bit-identical to scaling the scores
            q = g.op("Add", [g.op("MatMul", [x, c(wq * 0.125)]), c(bq * 0.125)])
            k = g.op("Add", [g.op("MatMul", [x, c(wk)]), c(bk)])
            v = g.op("Add", [g.op("MatMul", [x, c(wv)]), c(bv)])
            q = g.op("Transpose", [g.op("Reshape", [q, hs])], perm=[0, 2, 1, 3])
            kT = g.op("Transpose", [g.op("Reshape", [k, hs])], perm=[0, 2, 3, 1])
            v = g.op("Transpose", [g.op("Reshape", [v, hs])], perm=[0, 2, 1, 3])
            sc = g.op("Add", [g.op("MatMul", [q, kT]), kb])
            o = g.op("MatMul", [g.op("Softmax", [sc], axis=-1), v])
            o = g.op("Reshape", [g.op("Transpose", [o], perm=[0, 2, 1, 3]), g.i64([0, 0, DM])])
        o = g.op("MatMul", [o, c(_t(a.out_proj.weight).T)])
        l2 = ln(layer.layer_norm2)
        x2, _m, _s, resid = g.op("SkipLayerNormalization", [o, resid, c(l2[0]), c(l2[1]), c(_t(a.out_proj.bias))],
                                 n_out=4, domain="com.microsoft", epsilon=l2[2])
        mlp = layer.mlp
        f = g.op("FastGelu", [g.op("MatMul", [x2, c(_t(mlp.fc1.weight).T)]), c(_t(mlp.fc1.bias))], domain="com.microsoft")
        f = g.op("MatMul", [f, c(_t(mlp.fc2.weight).T)])
        nxt = ln(layers[i + 1].layer_norm1) if i + 1 < len(layers) else ln(vm.post_layernorm)
        x, _m, _s, resid = g.op("SkipLayerNormalization", [f, resid, c(nxt[0]), c(nxt[1]), c(_t(mlp.fc2.bias))],
                                n_out=4, domain="com.microsoft", epsilon=nxt[2])
    # x = post_layernorm(h). Projector: the 2x2 patch gather (channel-major like pixel_unshuffle).
    b_dim = g.op("Shape", ["gather_idx"], start=0, end=1)
    m_dim = g.op("Shape", ["gather_idx"], start=1, end=2)
    idx = g.op("Reshape", ["gather_idx", g.op("Concat", [b_dim, g.i64([-1, 1])], axis=0)])
    gth = g.op("GatherND", [x, idx], batch_dims=1)
    gth = g.op("Reshape", [gth, g.op("Concat", [b_dim, m_dim, g.i64([4, DM])], axis=0)])
    gth = g.op("Transpose", [gth], perm=[0, 1, 3, 2])
    gth = g.op("Reshape", [gth, g.op("Concat", [b_dim, m_dim, g.i64([4 * DM])], axis=0)])
    gth = g.op("Mul", [gth, g.op("Unsqueeze", ["tok_valid", g.i64([2])])])
    pn = ln(pj.norm)
    y = g.op("LayerNormalization", [gth, c(pn[0]), c(pn[1])], axis=-1, epsilon=pn[2])
    y = g.op("Gelu", [g.op("MatMul", [y, c(_t(pj.mlp[0].weight).T)])])
    y = g.op("MatMul", [y, c(_t(pj.mlp[2].weight).T)])
    g.nodes.append(helper.make_node("SimplifiedLayerNormalization", [y, c(_t(pj.out_norm.weight))], ["vis_tokens"],
                                    axis=-1, epsilon=float(pj.out_norm.eps), stash_type=1))
    return g.model("hayai_vision_webgpu", inputs, outputs)


def build(dt=np.float32, mha=False):
    m = load_model()
    return {"vision": vision_graph(m, dt, mha), "decoder": decoder_graph(decoder(m), dt, "hayai_decoder_webgpu")}
