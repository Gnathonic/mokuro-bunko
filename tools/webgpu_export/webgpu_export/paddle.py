"""paddle-manga's shared decoder graph for the WebGPU backend (ERNIE-4.5 text model).

Same math as `tools/onnx_export/onnx_export/paddle/modules.py` (DecoderWrap): RMSNorm
(eps 1e-5), rotate-half RoPE (M-RoPE angles come from the host as per-token tables), GQA
16/2 heads of 128, SwiGLU, last-position logits. The attention scale (1/sqrt(128)) is
applied to the scores like SDPA does (not a power of two, so it is not folded).
The vision graph is the exported one (`paddle-manga_vision_fp32.onnx`), unchanged.
"""

from __future__ import annotations

import sys
from pathlib import Path

import numpy as np

from .graph import Decoder, Layer, decoder_graph

ONNX_EXPORT = Path(__file__).resolve().parents[2] / "onnx_export"


def load_model():
    sys.path.insert(0, str(ONNX_EXPORT))
    from onnx_export.paddle import export as E  # the 0.5.2 runner's loading (LoRA merge, tower)

    model, _proc = E.load_model("float32")
    return model


def _t(p):
    return p.detach().float().numpy()


def decoder(model, splits=None) -> Decoder:
    lm = model.model.language_model
    layers = []
    for L in lm.layers:
        a, mlp = L.self_attn, L.mlp
        assert a.q_proj.bias is None and a.o_proj.bias is None
        layers.append(Layer(
            attn_norm=_t(L.input_layernorm.weight), ffn_norm=_t(L.post_attention_layernorm.weight),
            wq=_t(a.q_proj.weight).T, wk=_t(a.k_proj.weight).T, wv=_t(a.v_proj.weight).T, wo=_t(a.o_proj.weight).T,
            wg=_t(mlp.gate_proj.weight).T, wu=_t(mlp.up_proj.weight).T, wd=_t(mlp.down_proj.weight).T,
        ))
    a = lm.layers[0].self_attn
    eps = {L.input_layernorm.variance_epsilon for L in lm.layers} | {lm.norm.variance_epsilon}
    assert len(eps) == 1, eps
    cfg = a.config
    return Decoder(
        layers=layers, final_norm=_t(lm.norm.weight), head=_t(model.lm_head.weight).T,
        heads=cfg.num_attention_heads, kv_heads=cfg.num_key_value_heads, head_dim=a.head_dim, eps=eps.pop(),
        interleaved=False, scale=float(a.scaling), splits=splits or {"D": 8, "F": 16, "head": 1},
    )


def build(dt=np.float32):
    m = load_model()
    return {"decoder": decoder_graph(decoder(m), dt, "paddle_decoder_webgpu")}


# ---------------------------------------------------------------- the layered vision tower
#
# The EP frees a run's buffers only when the run ends, so one vision graph holds every
# layer's attention scores at once: N=2592 patches (a big crop) peaks at 12 GB. The tower
# therefore runs layer by layer, a separate run each, with the weights as graph INPUTS
# (uploaded to the device once, by running `pv_weights`): no weight is stored twice.
# Up to WHOLE_MAX patches a layer is one run (`pv_layer`); above it the attention runs per
# query chunk (`pv_qkv`, `pv_attn` per CHUNK queries, `pv_post`), so a run's scores stay
# at heads x CHUNK x N. Same math as VisionWrap (modules.py): rows are independent and the
# softmax still sees every key, so chunking does not change any value.

V_HEADS, V_HD, V_D = 16, 72, 1152
WHOLE_MAX = 1024  # patches up to which a layer is one run
CHUNK = 512  # queries per attention run above it (a big crop: 5 GB peak at N=5000, 7.5 GB at 1024)


def vision_weights(model) -> dict:
    vm = model.model.visual.vision_model
    pj = model.model.projector
    w = {
        "patch_w": _t(vm.embeddings.patch_embedding.weight).reshape(V_D, -1).T.copy(),  # (588, 1152)
        "patch_b": _t(vm.embeddings.patch_embedding.bias),
        "pos_table": _t(vm.embeddings.position_embedding.weight),  # (729, 1152)
        "post_w": _t(vm.post_layernorm.weight), "post_b": _t(vm.post_layernorm.bias),
        "pre_w": _t(pj.pre_norm.weight), "pre_b": _t(pj.pre_norm.bias),
        "l1_w": _t(pj.linear_1.weight).T.copy(), "l1_b": _t(pj.linear_1.bias),
        "l2_w": _t(pj.linear_2.weight).T.copy(), "l2_b": _t(pj.linear_2.bias),
    }
    for i, L in enumerate(vm.encoder.layers):
        a = L.self_attn
        assert a.num_heads == V_HEADS and abs(float(a.scaling) - V_HD ** -0.5) < 1e-12
        for k, m in (("q", a.q_proj), ("k", a.k_proj), ("v", a.v_proj), ("o", a.out_proj), ("fc1", L.mlp.fc1), ("fc2", L.mlp.fc2)):
            w[f"L{i}.{k}_w"] = _t(m.weight).T.copy()
            w[f"L{i}.{k}_b"] = _t(m.bias)
        for k, m in (("ln1", L.layer_norm1), ("ln2", L.layer_norm2)):
            w[f"L{i}.{k}_w"] = _t(m.weight)
            w[f"L{i}.{k}_b"] = _t(m.bias)
    eps = {"ln": vm.encoder.layers[0].layer_norm1.eps, "post": vm.post_layernorm.eps, "pre": pj.pre_norm.eps}
    return w, eps, len(vm.encoder.layers), float(vm.encoder.layers[0].self_attn.scaling)


def _vi(name, dims, tp=None):
    from onnx import TensorProto, helper

    return helper.make_tensor_value_info(name, tp or TensorProto.FLOAT, dims)


LAYER_W = ["ln1_w", "ln1_b", "q_w", "q_b", "k_w", "k_b", "v_w", "v_b", "o_w", "o_b", "ln2_w", "ln2_b", "fc1_w", "fc1_b", "fc2_w", "fc2_b"]


def vision_graphs(model) -> dict:
    """{name: ModelProto}: pv_weights (outputs every weight), pv_embed, pv_layer, pv_qkv,
    pv_attn, pv_post, pv_tail. Layer graphs take the layer's weights as inputs named
    LAYER_W (the host binds L<i>.<name>)."""
    from onnx import TensorProto, helper

    from .graph import G

    W, eps, n_layers, scale = vision_weights(model)
    out = {}
    # the weights, as outputs (run once at load, bound to the device)
    g = G()
    outs = []
    for k in W:
        g.nodes.append(helper.make_node("Identity", [g.init(W[k].astype(np.float32), "w")], [k]))
        outs.append(_vi(k, list(W[k].shape)))
    out["pv_weights"] = g.model("paddle_vision_weights", [], outs)
    shp = lambda k: list(W[k if k in W else f"L0.{k}"].shape)

    def layer_inputs():
        return [_vi(k, shp(k)) for k in LAYER_W]

    # embed: patches + bilinear position embedding
    g = G()
    pv = g.op("Reshape", ["pixel_values", g.i64([-1, 588])])
    x = g.op("Add", [g.op("MatMul", [pv, "patch_w"]), "patch_b"])
    pos = g.op("Gather", ["pos_table", "pos_idx"], axis=0)  # (N,4,1152)
    pos = g.op("ReduceSum", [g.op("Mul", [pos, g.op("Unsqueeze", ["pos_w", g.i64([2])])]), g.i64([1])], keepdims=0)
    g.nodes.append(helper.make_node("Add", [x, pos], ["x"]))
    out["pv_embed"] = g.model("paddle_vision_embed", [
        _vi("pixel_values", ["N", 3, 14, 14]), _vi("pos_idx", ["N", 4], TensorProto.INT64), _vi("pos_w", ["N", 4]),
        _vi("patch_w", shp("patch_w")), _vi("patch_b", shp("patch_b")), _vi("pos_table", shp("pos_table"))],
        [_vi("x", ["N", V_D])])

    def qkv(g):
        """x, cos, sin -> q (H,N,hd), kT (H,hd,N), v (H,N,hd); q pre-scaled? no: scores are scaled."""
        h = g.op("LayerNormalization", ["x", "ln1_w", "ln1_b"], axis=-1, epsilon=float(eps["ln"]))
        cos_c = g.op("Slice", ["cos", g.i64([0]), g.i64([V_HD // 2]), g.i64([1])])
        sin_c = g.op("Slice", ["sin", g.i64([0]), g.i64([V_HD // 2]), g.i64([1])])
        n_dim = g.op("Shape", ["x"], start=0, end=1)
        posid = g.op("Unsqueeze", [g.op("Range", [g.i64(0), g.op("Squeeze", [n_dim]), g.i64(1)]), g.i64([0])])  # (1,N)
        hs = g.op("Concat", [n_dim, g.i64([V_HEADS, V_HD])], axis=0)
        rope = lambda t: g.op("RotaryEmbedding", [g.op("Unsqueeze", [t, g.i64([0])]), posid, cos_c, sin_c],
                              domain="com.microsoft", interleaved=0, num_heads=V_HEADS)
        q = g.op("Add", [g.op("MatMul", [h, "q_w"]), "q_b"])
        k = g.op("Add", [g.op("MatMul", [h, "k_w"]), "k_b"])
        v = g.op("Add", [g.op("MatMul", [h, "v_w"]), "v_b"])
        q = g.op("Transpose", [g.op("Reshape", [rope(q), hs])], perm=[1, 0, 2])  # (H,N,hd)
        kT = g.op("Transpose", [g.op("Reshape", [rope(k), hs])], perm=[1, 2, 0])  # (H,hd,N)
        v = g.op("Transpose", [g.op("Reshape", [v, hs])], perm=[1, 0, 2])
        return q, kT, v

    def attn(g, q, kT, v):
        s = g.op("Mul", [g.op("MatMul", [q, kT]), g.c(scale)])
        o = g.op("MatMul", [g.op("Softmax", [s], axis=-1), v])  # (H,c,hd)
        return g.op("Reshape", [g.op("Transpose", [o], perm=[1, 0, 2]), g.i64([-1, V_D])])

    def post(g, o, xname="x"):
        h2, _m, _s, x1 = g.op("SkipLayerNormalization", [g.op("MatMul", [o, "o_w"]), xname, "ln2_w", "ln2_b", "o_b"],
                              n_out=4, domain="com.microsoft", epsilon=float(eps["ln"]))
        f = g.op("FastGelu", [g.op("MatMul", [h2, "fc1_w"]), "fc1_b"], domain="com.microsoft")
        f = g.op("Add", [g.op("MatMul", [f, "fc2_w"]), "fc2_b"])
        g.nodes.append(helper.make_node("Add", [x1, f], ["x_out"]))

    xin = [_vi("x", ["N", V_D]), _vi("cos", ["N", V_HD]), _vi("sin", ["N", V_HD])]
    g = G()
    post(g, attn(g, *qkv(g)))
    out["pv_layer"] = g.model("paddle_vision_layer", xin + layer_inputs(), [_vi("x_out", ["N", V_D])])
    g = G()
    q, kT, v = qkv(g)
    for src, dst in ((q, "q"), (kT, "kT"), (v, "v")):
        g.out(src, dst)
    out["pv_qkv"] = g.model("paddle_vision_qkv", xin + layer_inputs(),
                            [_vi("q", [V_HEADS, "N", V_HD]), _vi("kT", [V_HEADS, V_HD, "N"]), _vi("v", [V_HEADS, "N", V_HD])])
    g = G()
    qc = g.op("Slice", ["q", "start", "end", g.i64([1])])  # this run's queries
    g.out(attn(g, qc, "kT", "v"), "o")
    out["pv_attn"] = g.model("paddle_vision_attn", [
        _vi("q", [V_HEADS, "N", V_HD]), _vi("kT", [V_HEADS, V_HD, "N"]), _vi("v", [V_HEADS, "N", V_HD]),
        _vi("start", [1], TensorProto.INT64), _vi("end", [1], TensorProto.INT64)], [_vi("o", ["c", V_D])])
    g = G()
    post(g, "o")
    out["pv_post"] = g.model("paddle_vision_post", [_vi("x", ["N", V_D]), _vi("o", ["N", V_D])] + layer_inputs(),
                             [_vi("x_out", ["N", V_D])])
    # tail: post norm, projector norm, 2x2 merge, MLP
    g = G()
    x = g.op("LayerNormalization", ["x", "post_w", "post_b"], axis=-1, epsilon=float(eps["post"]))
    x = g.op("LayerNormalization", [x, "pre_w", "pre_b"], axis=-1, epsilon=float(eps["pre"]))
    x = g.op("Reshape", [g.op("Gather", [x, "merge_idx"], axis=0), g.i64([-1, 4 * V_D])])
    x = g.op("Gelu", [g.op("Add", [g.op("MatMul", [x, "l1_w"]), "l1_b"])])
    g.nodes.append(helper.make_node("Add", [g.op("MatMul", [x, "l2_w"]), "l2_b"], ["image_embeds"]))
    out["pv_tail"] = g.model("paddle_vision_tail", [_vi("x", ["N", V_D]), _vi("merge_idx", ["N"], TensorProto.INT64)]
                             + [_vi(k, shp(k)) for k in ("post_w", "post_b", "pre_w", "pre_b", "l1_w", "l1_b", "l2_w", "l2_b")],
                             [_vi("image_embeds", ["M", 1024])])
    out["_meta"] = {"layers": n_layers, "whole_max": WHOLE_MAX, "chunk": CHUNK}
    return out


def build_all(dt=np.float32):
    m = load_model()
    g = vision_graphs(m)
    g["decoder"] = decoder_graph(decoder(m), dt, "paddle_decoder_webgpu")
    return g
