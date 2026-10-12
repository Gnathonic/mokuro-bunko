"""ONNX graph builder and the shared decoder graph of the WebGPU recognizers.

One decoder graph serves prefill and every decode step (one copy of the weights on disk
and on the GPU). It reads two KV regions per layer:

  ctx  (fixed for a batch: the prompt's keys/values, written by the prefill call)
  gen  (grows: the keys/values of the tokens generated so far, plus this call's tokens)

  inputs   x        (B, s, D)       embeddings of this call's s tokens
           cos/sin  (B, s, R)       RoPE angle tables per token (R = rotary pairs)
           pos      (B, s) int64    row index into the flattened (B*s, R) tables
           ctx_bias (B, 1, s, C)    additive bias over the ctx keys
           gen_bias (B, 1, s, t+s)  additive bias over the gen keys and this call's keys
           ctx_kT{i} (B, kv, hd, C), ctx_v{i} (B, kv, C, hd)
           gen_kT{i} (B, kv, hd, t), gen_v{i} (B, kv, t, hd)
  outputs  next     (B,) int64      greedy token after each row's LAST position
           gen_kT{i}_o (B, kv, hd, t+s), gen_v{i}_o (B, kv, t+s, hd)

Prefill = one call with a masked one-key dummy for ctx and gen and the whole prompt as
x; its gen outputs become the batch's ctx, and decoding restarts gen from the dummy.
Nothing is ever copied back; only token ids leave the GPU.

Kernels: split-K linears for small row counts (the EP tiles a 16-row GEMM onto a
handful of workgroups; split-K is deterministic, no atomics), com.microsoft
RotaryEmbedding / SimplifiedLayerNormalization / SkipSimplifiedLayerNormalization, GQA
by grouping heads (no repeat copies), and a two-stage argmax (the EP's ArgMax runs one
thread per row).
"""

from __future__ import annotations

from dataclasses import dataclass, field

import numpy as np
from onnx import TensorProto, helper, numpy_helper

OPSET = 20
SPLIT_ROWS = 64  # rows up to which linears use split-K (decode steps: batch x 1 token)


class G:
    """Graph under construction; node and initializer names are sequential (reproducible)."""

    def __init__(self, dt=np.float32):
        self.nodes, self.inits, self.n = [], [], 0
        self.dt = dt
        self.tp = TensorProto.FLOAT16 if dt == np.float16 else TensorProto.FLOAT

    def name(self, p):
        self.n += 1
        return f"{p}_{self.n}"

    def init(self, arr, p="w"):
        nm = self.name(p)
        self.inits.append(numpy_helper.from_array(np.ascontiguousarray(arr), nm))
        return nm

    def c(self, arr, p="w"):
        return self.init(np.asarray(arr).astype(self.dt), p)

    def i64(self, vals, p="c"):
        return self.init(np.array(vals, np.int64), p)

    def op(self, t, ins, n_out=1, p=None, domain=None, **attrs):
        outs = [self.name(p or t.lower()) for _ in range(n_out)]
        self.nodes.append(helper.make_node(t, ins, outs, domain=domain, **attrs))
        return outs[0] if n_out == 1 else outs

    def out(self, src, name):
        self.nodes.append(helper.make_node("Identity", [src], [name]))

    def linear(self, x, w, s=1):
        """x (r, K) @ w (K, N). With s > 1: split-K over s chunks when r <= SPLIT_ROWS
        (decode steps), a plain MatMul otherwise (prefill: the s partial products of a
        long prompt would cost s times the output in VRAM, and the EP frees a run's
        buffers only when the run ends). Both branches read the same weight (the split
        view is a Reshape of it); split-K is deterministic (no atomics)."""
        k, n = w.shape
        wn = self.c(w)
        if s <= 1 or k % s:
            return self.op("MatMul", [x, wn])
        rows = self.op("Shape", [x], start=0, end=1)
        cond = self.op("Squeeze", [self.op("LessOrEqual", [rows, self.i64([SPLIT_ROWS])])])
        y_split, y_plain = self.name("y_split"), self.name("y_plain")
        sub = G(self.dt)
        sub.n = self.n
        x3 = sub.op("Reshape", [x, sub.i64([-1, s, k // s])])
        xt = sub.op("Transpose", [x3], perm=[1, 0, 2])
        # the split view's shape comes from x at run time, so constant folding cannot
        # turn the Reshape into a second copy of the weight
        kk = sub.op("Div", [sub.op("Shape", [x], start=1, end=2), sub.i64([s])])
        ws = sub.op("Reshape", [wn, sub.op("Concat", [sub.i64([s]), kk, sub.i64([-1])], axis=0)])
        p = sub.op("MatMul", [xt, ws])
        sub.nodes.append(helper.make_node("ReduceSum", [p, sub.i64([0])], [y_split], keepdims=0))
        self.n = sub.n
        then_g = helper.make_graph(sub.nodes, self.name("split"), [], [helper.make_tensor_value_info(y_split, self.tp, None)], sub.inits)
        else_g = helper.make_graph([helper.make_node("MatMul", [x, wn], [y_plain])], self.name("plain"), [],
                                   [helper.make_tensor_value_info(y_plain, self.tp, None)])
        return self.op("If", [cond], then_branch=then_g, else_branch=else_g)

    def sln(self, x, w, eps):
        return self.op("SimplifiedLayerNormalization", [x, self.c(w)], axis=-1, epsilon=float(eps), stash_type=1)

    def skip_sln(self, x, skip, w, eps):
        """(norm(x + skip), x + skip)."""
        y, _m, _r, s = self.op("SkipSimplifiedLayerNormalization", [x, skip, self.c(w)], n_out=4,
                               domain="com.microsoft", epsilon=float(eps))
        return y, s

    def argmax2(self, logits, out, n):
        """First-max argmax over the last axis of (r, n): pad to C*C with -inf, per-chunk
        first max, then the first chunk holding the row max (chunks keep index order, so
        this is torch.argmax / ArgMax(select_last_index=0))."""
        c = int(np.ceil(np.sqrt(n)))
        neg = self.init(np.array(-np.inf if self.dt == np.float32 else -65504.0, self.dt), "neg")
        x = self.op("Pad", [logits, self.i64([0, 0, 0, c * c - n]), neg], mode="constant")
        x = self.op("Reshape", [x, self.i64([-1, c, c])])
        a1 = self.op("ArgMax", [x], axis=2, keepdims=1)
        v1 = self.op("ReduceMax", [x, self.i64([2])], keepdims=1)
        a2 = self.op("ArgMax", [v1], axis=1, keepdims=1)
        inner = self.op("GatherElements", [a1, a2], axis=1)
        flat = self.op("Add", [self.op("Mul", [a2, self.i64([c])]), inner])
        self.nodes.append(helper.make_node("Reshape", [flat, self.i64([-1])], [out]))

    def model(self, name, inputs, outputs):
        graph = helper.make_graph(self.nodes, name, inputs, outputs, self.inits)
        m = helper.make_model(graph, opset_imports=[helper.make_opsetid("", OPSET), helper.make_opsetid("com.microsoft", 1)],
                              producer_name="mokuro-bunko webgpu_export")
        m.ir_version = 10
        return m


@dataclass
class Layer:
    attn_norm: np.ndarray
    ffn_norm: np.ndarray
    wq: np.ndarray  # (D, nh*hd)
    wk: np.ndarray
    wv: np.ndarray
    wo: np.ndarray
    wg: np.ndarray  # (D, F)
    wu: np.ndarray
    wd: np.ndarray  # (F, D)
    q_norm: np.ndarray | None = None  # per-head RMSNorm weight (hd,)
    k_norm: np.ndarray | None = None
    attn_res: np.ndarray | None = None  # residual scale (D,)
    ffn_res: np.ndarray | None = None


@dataclass
class Decoder:
    layers: list[Layer]
    final_norm: np.ndarray
    head: np.ndarray  # (D, V)
    heads: int
    kv_heads: int
    head_dim: int
    eps: float  # attn/ffn/final norm epsilon
    qk_eps: float = 1e-6
    interleaved: bool = False  # RoPE pairs (x[2i], x[2i+1]) instead of halves
    scale: float = 0.0  # attention scale; 0 = 1/sqrt(head_dim)
    splits: dict = field(default_factory=lambda: {"D": 8, "F": 16, "head": 1})


def decoder_graph(d: Decoder, dt=np.float32, name="decoder"):
    g = G(dt)
    tp = g.tp
    D = d.layers[0].wq.shape[0]
    nh, kv, hd = d.heads, d.kv_heads, d.head_dim
    grp = nh // kv
    rot = hd // 2  # RotaryEmbedding cache width (rotary pairs)
    scale = d.scale or 1.0 / np.sqrt(hd)
    exact_fold = float(scale) in (0.5, 0.25, 0.125, 0.0625)  # a power of two: folded into q exactly
    inputs = [
        helper.make_tensor_value_info("x", tp, ["B", "s", D]),
        helper.make_tensor_value_info("cos", tp, ["B", "s", rot]),
        helper.make_tensor_value_info("sin", tp, ["B", "s", rot]),
        helper.make_tensor_value_info("pos", TensorProto.INT64, ["B", "s"]),
        helper.make_tensor_value_info("ctx_bias", tp, ["B", 1, "s", "C"]),
        helper.make_tensor_value_info("gen_bias", tp, ["B", 1, "s", "T"]),
    ]
    outputs = [helper.make_tensor_value_info("next", TensorProto.INT64, ["B"])]
    for i in range(len(d.layers)):
        inputs += [helper.make_tensor_value_info(f"ctx_kT{i}", tp, ["B", kv, hd, "C"]),
                   helper.make_tensor_value_info(f"ctx_v{i}", tp, ["B", kv, "C", hd])]
    for i in range(len(d.layers)):
        inputs += [helper.make_tensor_value_info(f"gen_kT{i}", tp, ["B", kv, hd, "t"]),
                   helper.make_tensor_value_info(f"gen_v{i}", tp, ["B", kv, "t", hd])]
        outputs += [helper.make_tensor_value_info(f"gen_kT{i}_o", tp, ["B", kv, hd, "T"]),
                    helper.make_tensor_value_info(f"gen_v{i}_o", tp, ["B", kv, "T", hd])]
    shp_r = g.i64([-1, rot])
    cos_c = g.op("Reshape", ["cos", shp_r])
    sin_c = g.op("Reshape", ["sin", shp_r])
    b_dim = g.op("Shape", ["x"], start=0, end=1)
    s_dim = g.op("Shape", ["x"], start=1, end=2)
    cat = lambda *parts: g.op("Concat", list(parts), axis=0)
    shp_bsx = lambda last: cat(b_dim, s_dim, g.i64(last))
    shp_bkgs = lambda last: cat(b_dim, g.i64([kv, grp]), s_dim, g.i64(last))
    shp_q = cat(b_dim, s_dim, g.i64([nh, hd]))
    shp_qg = cat(b_dim, g.i64([kv, -1, hd]))  # (B, kv, grp*s, hd)
    shp_k = cat(b_dim, s_dim, g.i64([kv, hd]))
    shp_rows = g.i64([-1, D])
    ctx_bias = g.op("Unsqueeze", ["ctx_bias", g.i64([1])])  # (B,1,1,s,C): broadcast over kv, grp
    gen_bias = g.op("Unsqueeze", ["gen_bias", g.i64([1])])
    x = g.op("Reshape", ["x", shp_rows])  # (B*s, D)
    h = g.sln(x, d.layers[0].attn_norm, d.eps)
    resid = x
    sp = d.splits
    for i, L in enumerate(d.layers):
        qkv = g.linear(h, np.concatenate([L.wq * (scale if exact_fold and L.q_norm is None else 1.0), L.wk, L.wv], 1), sp["D"])
        q, k, v = g.op("Split", [qkv, g.i64([nh * hd, kv * hd, kv * hd])], n_out=3, axis=-1)
        if L.q_norm is not None:
            q = g.sln(g.op("Reshape", [q, g.i64([-1, hd])]), L.q_norm, d.qk_eps)
            k = g.sln(g.op("Reshape", [k, g.i64([-1, hd])]), L.k_norm, d.qk_eps)
        q = g.op("RotaryEmbedding", [g.op("Reshape", [q, shp_bsx([nh * hd])]), "pos", cos_c, sin_c],
                 domain="com.microsoft", interleaved=int(d.interleaved), num_heads=nh)
        k = g.op("RotaryEmbedding", [g.op("Reshape", [k, shp_bsx([kv * hd])]), "pos", cos_c, sin_c],
                 domain="com.microsoft", interleaved=int(d.interleaved), num_heads=kv)
        if exact_fold and L.q_norm is not None:
            q = g.op("Mul", [q, g.c(scale)])  # after the q norm; exact (power of two)
        # q (B,s,nh,hd) -> (B,nh,s,hd) -> (B,kv,grp*s,hd); heads kvi*grp+gi share kv head kvi
        qg = g.op("Reshape", [g.op("Transpose", [g.op("Reshape", [q, shp_q])], perm=[0, 2, 1, 3]), shp_qg])
        kT_new = g.op("Transpose", [g.op("Reshape", [k, shp_k])], perm=[0, 2, 3, 1])  # (B,kv,hd,s)
        v_new = g.op("Transpose", [g.op("Reshape", [v, shp_k])], perm=[0, 2, 1, 3])  # (B,kv,s,hd)
        gkT = g.op("Concat", [f"gen_kT{i}", kT_new], axis=3)
        gv = g.op("Concat", [f"gen_v{i}", v_new], axis=2)
        g.out(gkT, f"gen_kT{i}_o")
        g.out(gv, f"gen_v{i}_o")
        s_ctx = g.op("MatMul", [qg, f"ctx_kT{i}"])
        s_gen = g.op("MatMul", [qg, gkT])
        if not exact_fold:
            s_ctx = g.op("Mul", [s_ctx, g.c(scale)])
            s_gen = g.op("Mul", [s_gen, g.c(scale)])
        s_ctx = g.op("Add", [g.op("Reshape", [s_ctx, shp_bkgs([-1])]), ctx_bias])  # (B,kv,grp,s,C)
        s_gen = g.op("Add", [g.op("Reshape", [s_gen, shp_bkgs([-1])]), gen_bias])
        p = g.op("Softmax", [g.op("Concat", [s_ctx, s_gen], axis=-1)], axis=-1)
        n_ctx = g.op("Shape", [f"ctx_v{i}"], start=2, end=3)
        n_gen = g.op("Shape", [gv], start=2, end=3)
        p_ctx, p_gen = g.op("Split", [p, g.op("Concat", [n_ctx, n_gen], axis=0)], n_out=2, axis=-1)
        p_ctx = g.op("Reshape", [p_ctx, cat(b_dim, g.i64([kv, -1]), n_ctx)])  # (B,kv,grp*s,C)
        p_gen = g.op("Reshape", [p_gen, cat(b_dim, g.i64([kv, -1]), n_gen)])
        o = g.op("Add", [g.op("MatMul", [p_ctx, f"ctx_v{i}"]), g.op("MatMul", [p_gen, gv])])  # (B,kv,grp*s,hd)
        o = g.op("Reshape", [o, cat(b_dim, g.i64([nh]), s_dim, g.i64([hd]))])
        o = g.op("Reshape", [g.op("Transpose", [o], perm=[0, 2, 1, 3]), g.i64([-1, nh * hd])])
        o = g.linear(o, L.wo, sp["D"])
        if L.attn_res is not None:
            o = g.op("Mul", [o, g.c(L.attn_res)])
        h2, resid = g.skip_sln(o, resid, L.ffn_norm, d.eps)
        gu = g.linear(h2, np.concatenate([L.wg, L.wu], 1), sp["D"])
        gt, up = g.op("Split", [gu, g.i64([L.wg.shape[1], L.wu.shape[1]])], n_out=2, axis=-1)
        a = g.op("Mul", [g.op("Mul", [gt, g.op("Sigmoid", [gt])]), up])  # silu(gate) * up
        f = g.linear(a, L.wd, sp["F"])
        if L.ffn_res is not None:
            f = g.op("Mul", [f, g.c(L.ffn_res)])
        nxt = d.layers[i + 1].attn_norm if i + 1 < len(d.layers) else d.final_norm
        h, resid = g.skip_sln(f, resid, nxt, d.eps)
    # last position of every row -> head -> argmax
    last = g.op("Gather", [g.op("Reshape", [h, shp_bsx([D])]), g.op("Sub", [s_dim, g.i64([1])])], axis=1)  # (B,1,D)
    logits = g.linear(g.op("Reshape", [last, shp_rows]), d.head, sp["head"])
    g.argmax2(logits, "next", d.head.shape[1])
    return g.model(name, inputs, outputs)


def rope_tables(n: int, inv: np.ndarray, duplicate: bool):
    """cos/sin (n, R) for positions 0..n-1: angle = pos * inv (f32), R = len(inv) (or 2x)."""
    a = (np.arange(n, dtype=np.float32)[:, None] * inv[None]).astype(np.float32)
    if duplicate:
        a = np.concatenate([a, a], 1)
    return np.cos(a).astype(np.float32), np.sin(a).astype(np.float32)
