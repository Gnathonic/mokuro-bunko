"""The three graphs per engine (vision, decoder prefill, decoder step) as exportable modules.

The arithmetic is the onnx_export functional modules' (``tools/onnx_export``: same
weights, same host-computed inputs as spec §5/§6), driven the way the 0.5.2 runner
drives the model at each precision:

  hayai-nova   fp32 weights traced under ``torch.autocast(dtype)`` -- what 0.5.2 runs.
               ``precast`` stores every weight autocast would cast (Linear / Conv) in
               the autocast dtype instead: autocast's cast of an fp32 weight is a
               round-to-nearest-even ``.to(dtype)``, so the arithmetic is unchanged and
               the per-call weight casts (and half the weight bytes) disappear.
  paddle-manga model loaded in the target dtype (0.5.2 ``PaddleMangaRecognizer``).

Graph I/O (``io``):

  v1  prefill/step return ``(logits, *present)`` (the shootout contract).
  v2  prefill returns ``(next_ids i64 (b,), live bool (b,), *present)``; step takes
      ``live`` after its four v1 inputs and returns the same triple.  ``next_ids`` is
      ``where(live, argmax(logits), fill)`` on the logits the v1 graph would have
      returned (same dtype, first index on ties, like ``torch.argmax``) and
      ``live_out = live & next != eos [& next != pad]``.  The host loop no longer
      touches logits; it only needs ``live.any()`` to stop.

Each builder returns ``Graphs``: the modules, example inputs, dynamic shapes, the
names of the weights each module owns (for the shared weights blob) and host tables.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Callable

import torch
from torch import nn
from torch.export import Dim

DT = {"fp32": torch.float32, "bf16": torch.bfloat16, "fp16": torch.float16}
ROLES = ("vision", "prefill", "step")

HAYAI_EOS, HAYAI_PAD = 16002, 16000
PADDLE_EOS, PADDLE_FILL = 2, 0


@dataclass
class Graph:
    role: str
    module: nn.Module
    args: tuple
    dynamic_shapes: Any


@dataclass
class Graphs:
    engine: str
    precision: str
    io: int
    model: nn.Module  # the source model (its parameter names are the weight keys)
    graphs: list[Graph]
    # files written next to the packages (name -> writer(path))
    host_files: dict[str, Callable[[Path], None]] = field(default_factory=dict)
    # extra weights the host loop reads from the blob: blob key -> source parameter name
    host_weights: dict[str, str] = field(default_factory=dict)
    meta: dict[str, str] = field(default_factory=dict)
    # weights the graph wrappers own (not model parameters), by blob key
    extra: dict[str, torch.Tensor] = field(default_factory=dict)


class Autocast(nn.Module):
    """Runs ``inner`` under autocast (no-op for fp32), hands out dense outputs."""

    def __init__(self, inner: nn.Module, dtype: torch.dtype, device_type: str):
        super().__init__()
        self.inner, self.dtype, self.dt = inner, dtype, device_type

    def run(self, *a):
        if self.dtype == torch.float32:
            return self.inner(*a)
        with torch.autocast(self.dt, dtype=self.dtype):
            return self.inner(*a)

    def forward(self, *a):
        out = self.run(*a)
        # AOTI bakes the example strides in: hand out dense KV so the next step's
        # inputs have the layout the step graph was compiled for.
        return tuple(o.contiguous() for o in out) if isinstance(out, tuple) else out


def _next(logits: torch.Tensor, live: torch.Tensor | None, fill: int, stop: tuple[int, ...]):
    nxt = torch.argmax(logits, dim=-1)
    if live is not None:
        nxt = torch.where(live, nxt, torch.full_like(nxt, fill))
    alive = nxt != stop[0]
    for s in stop[1:]:
        alive = alive & (nxt != s)
    if live is not None:
        alive = alive & live
    return nxt, alive


class FusedNovaDecoder(nn.Module):
    """NovaDecoder (onnx_export/hayai/modules.py) with q|k|v and gate|up as one GEMM each
    (what inductor's freezing does with concat-linear; weights concatenated once here).
    Same arithmetic per output element; only the GEMM shapes change."""

    def __init__(self, model):
        super().__init__()
        from onnx_export.hayai.modules import NovaDecoder

        base = NovaDecoder(model)
        self.final_norm, self.head = base.final_norm, base.head
        # register only what the fused forward uses (unused q/k/v/gate/up weights would
        # otherwise stay graph constants)
        self.layers = nn.ModuleList()
        self.qkv = nn.ParameterList()
        self.gu = nn.ParameterList()
        self.shape = []
        for layer in base.layers:
            at, f = layer.attn, layer.ffn
            self.layers.append(nn.ModuleDict({"attn_norm": layer.attn_norm, "q_norm": at.q_norm, "k_norm": at.k_norm,
                                              "w_o": at.w_o, "ffn_norm": layer.ffn_norm, "w_down": f.w_down}))
            self.layers[-1].attn_res_scale = layer.attn_res_scale
            self.layers[-1].ffn_res_scale = layer.ffn_res_scale
            self.shape.append((at.h_q, at.h_kv, at.d_head))
            self.qkv.append(nn.Parameter(torch.cat([at.w_q.weight, at.w_k.weight, at.w_v.weight], 0).detach(), requires_grad=False))
            self.gu.append(nn.Parameter(torch.cat([f.w_gate.weight, f.w_up.weight], 0).detach(), requires_grad=False))

    def forward(self, embeds, mask, cos, sin, *past):
        import torch.nn.functional as F
        from onnx_export.hayai.modules import rope

        x = embeds
        present = []
        for i, L in enumerate(self.layers):
            h_q, h_kv, d = self.shape[i]
            h = L["attn_norm"](x)
            b, s, _ = h.shape
            q, k, v = F.linear(h, self.qkv[i]).split([h_q * d, h_kv * d, h_kv * d], -1)
            q = L["q_norm"](q.reshape(b, s, h_q, d))
            k = L["k_norm"](k.reshape(b, s, h_kv, d))
            v = v.reshape(b, s, h_kv, d)
            q, k = rope(q, cos, sin), rope(k, cos, sin)
            k, v = k.transpose(1, 2), v.transpose(1, 2)
            if past:
                k = torch.cat([past[2 * i], k], 2); v = torch.cat([past[2 * i + 1], v], 2)
            present += [k, v]
            rep = h_q // h_kv
            kr, vr = k.repeat_interleave(rep, 1), v.repeat_interleave(rep, 1)
            o = F.scaled_dot_product_attention(q.transpose(1, 2), kr, vr, attn_mask=mask)
            x = x + L.attn_res_scale * L["w_o"](o.transpose(1, 2).reshape(b, s, -1))
            g, u = F.linear(L["ffn_norm"](x), self.gu[i]).chunk(2, -1)
            x = x + L.ffn_res_scale * L["w_down"](F.silu(g) * u)
        logits = self.head(self.final_norm(x[:, -1]))
        return (logits, *present)


class HayaiDecoder(Autocast):
    """prefill (no ``live``) / step (``live`` after the 4 inputs) for io v2."""

    def __init__(self, inner, dtype, device_type, step: bool):
        super().__init__(inner, dtype, device_type)
        self.step = step

    def forward(self, embeds, mask, cos, sin, *rest):
        live, past = (rest[0], rest[1:]) if self.step else (None, ())
        if self.dtype == torch.float32:
            logits, *present = self.inner(embeds, mask, cos, sin, *past)
            nxt, alive = _next(logits, live, HAYAI_PAD, (HAYAI_EOS, HAYAI_PAD))
        else:
            with torch.autocast(self.dt, dtype=self.dtype):
                logits, *present = self.inner(embeds, mask, cos, sin, *past)
                nxt, alive = _next(logits, live, HAYAI_PAD, (HAYAI_EOS, HAYAI_PAD))
        return (nxt, alive, *(p.contiguous() for p in present))


def _to(model: nn.Module, device: torch.device) -> nn.Module:
    p = next(model.parameters())
    return model if p.device.type == device.type else model.to(device)


def precast_autocast_weights(model: nn.Module, dtype: torch.dtype) -> list[str]:
    """Store the weights autocast would cast (Linear/Conv weight+bias) in ``dtype``."""
    if dtype == torch.float32:
        return []
    done = []
    for name, m in model.named_modules():
        if isinstance(m, (nn.Linear, nn.Conv1d, nn.Conv2d, nn.Conv3d)):
            m.to(dtype)
            done.append(name)
    return done


def hayai(prec: str, device: torch.device, io: int, model=None, precast: bool = True, fuse: bool = False) -> Graphs:
    from onnx_export.hayai.export import load_model, special_ids
    from onnx_export.hayai.modules import NovaDecoder, NovaVision

    tok = None
    if model is None:
        model, tok = load_model()
    model = model.eval()
    dt = DT[prec]
    if precast:
        precast_autocast_weights(model, dt)
    model = _to(model, device)
    vis = Autocast(NovaVision(model).eval(), dt, device.type)
    g = torch.Generator().manual_seed(0)
    b, P, M = 3, 512, 130
    ex = (torch.randn(b, P, 768, generator=g), torch.ones(b, P), torch.randn(b, P, 768, generator=g),
          torch.randint(0, P, (b, M, 4), generator=g), torch.ones(b, M))
    ex = tuple(t.to(device) for t in ex)
    B, MM = Dim("b", min=1, max=64), Dim("M", min=2, max=2048)
    # P (patch budget) is static: one package per budget (512 = the runner's default).
    graphs = [Graph("vision", vis, ex, (({0: B}, {0: B}, {0: B}, {0: B, 1: MM}, {0: B, 1: MM}),))]
    s = 9
    exd = (torch.randn(b, s, 512, generator=g), torch.zeros(b, 1, s, s), torch.randn(b, s, 32, generator=g),
           torch.randn(b, s, 32, generator=g))
    exd = tuple(t.to(device) for t in exd)
    S = Dim.DYNAMIC
    pre_dyn = ({0: B, 1: S}, {0: B, 2: S, 3: S}, {0: B, 1: S}, {0: B, 1: S})
    # KV dtypes under autocast: K comes out f32 (q/k-norm + rope), V in the autocast dtype
    kdt, vdt = (torch.float32, dt) if dt != torch.float32 else (torch.float32, torch.float32)
    l = 7
    past = [torch.randn(b, 2, l, 64, generator=g).to(device, kdt if i % 2 == 0 else vdt) for i in range(24)]
    exs = (torch.randn(b, 1, 512, generator=g).to(device), torch.zeros(b, 1, 1, l + 1, device=device),
           torch.randn(b, 1, 32, generator=g).to(device), torch.randn(b, 1, 32, generator=g).to(device))
    L = Dim.DYNAMIC
    step_dyn = [{0: B}, {0: B, 3: Dim.DYNAMIC}, {0: B}, {0: B}]
    if io == 1:
        dec = Autocast(NovaDecoder(model).eval(), dt, device.type)
        graphs.append(Graph("prefill", dec, exd, (pre_dyn,)))
        graphs.append(Graph("step", dec, (*exs, *past), (tuple(step_dyn + [{0: B, 2: L}] * 24),)))
    else:
        live = torch.ones(b, dtype=torch.bool, device=device)
        D = FusedNovaDecoder(model).eval() if fuse else NovaDecoder(model).eval()
        graphs.append(Graph("prefill", HayaiDecoder(D, dt, device.type, False), exd, pre_dyn))
        graphs.append(Graph("step", HayaiDecoder(D, dt, device.type, True),
                            (*exs, live, *past), (*step_dyn, ({0: B}, *[{0: B, 2: L}] * 24))))
    out = Graphs("hayai-nova", prec, io, model, graphs)
    if io == 2 and fuse:
        out.extra = {f"fused.decoder.{n}": t for n, t in D.named_parameters() if n.startswith(("qkv.", "gu."))}
        out.meta["fused"] = "qkv,gate_up"
    if tok is not None:
        sp = special_ids(tok)
        assert (sp["eos"], sp["pad"]) == (HAYAI_EOS, HAYAI_PAD), sp
        out.meta["special"] = json.dumps(sp)
    return out


class FusedPaddleDecoder(nn.Module):
    """onnx_export/paddle/modules.py DecoderWrap with q|k|v and gate|up as one GEMM each."""

    def __init__(self, model):
        super().__init__()
        from onnx_export.paddle.modules import DecoderWrap

        base = DecoderWrap(model)
        self.norm, self.lm_head = base.norm, base.lm_head
        self.nh, self.nkv, self.hd, self.scaling = base.nh, base.nkv, base.hd, base.scaling
        self.layers = nn.ModuleList()
        self.qkv = nn.ParameterList()
        self.gu = nn.ParameterList()
        for L in base.layers:
            a, m = L.self_attn, L.mlp
            self.layers.append(nn.ModuleDict({"input_layernorm": L.input_layernorm, "o_proj": a.o_proj,
                                              "post_attention_layernorm": L.post_attention_layernorm,
                                              "down_proj": m.down_proj, "act_fn": m.act_fn}))
            self.qkv.append(nn.Parameter(torch.cat([a.q_proj.weight, a.k_proj.weight, a.v_proj.weight], 0).detach(), requires_grad=False))
            self.gu.append(nn.Parameter(torch.cat([m.gate_proj.weight, m.up_proj.weight], 0).detach(), requires_grad=False))

    def forward(self, inputs_embeds, cos, sin, bias, *past):
        import torch.nn.functional as F
        from onnx_export.paddle.modules import rot_half

        b, s, _ = inputs_embeds.shape
        x = inputs_embeds
        cos = cos.unsqueeze(1).to(x.dtype); sin = sin.unsqueeze(1).to(x.dtype)
        rep = self.nh // self.nkv
        presents = []
        nq, nk = self.nh * self.hd, self.nkv * self.hd
        for i, L in enumerate(self.layers):
            h = L["input_layernorm"](x)
            q, k, v = F.linear(h, self.qkv[i]).split([nq, nk, nk], -1)
            q = q.reshape(b, s, self.nh, self.hd).transpose(1, 2)
            k = k.reshape(b, s, self.nkv, self.hd).transpose(1, 2)
            v = v.reshape(b, s, self.nkv, self.hd).transpose(1, 2)
            q = q * cos + rot_half(q) * sin; k = k * cos + rot_half(k) * sin
            k = torch.cat([past[2 * i], k], dim=2); v = torch.cat([past[2 * i + 1], v], dim=2)
            presents += [k, v]
            t = k.shape[2]
            kk = k[:, :, None].expand(b, self.nkv, rep, t, self.hd).reshape(b, self.nh, t, self.hd)
            vv = v[:, :, None].expand(b, self.nkv, rep, t, self.hd).reshape(b, self.nh, t, self.hd)
            o = F.scaled_dot_product_attention(q, kk, vv, attn_mask=bias.to(x.dtype), scale=self.scaling)
            x = x + L["o_proj"](o.transpose(1, 2).reshape(b, s, -1))
            g, u = F.linear(L["post_attention_layernorm"](x), self.gu[i]).chunk(2, -1)
            x = x + L["down_proj"](L["act_fn"](g) * u)
        x = self.norm(x[:, -1:, :])
        return (self.lm_head(x)[:, 0, :].float(), *presents)


class PaddlePrefill(nn.Module):
    def __init__(self, dw, n_layers: int, io: int):
        super().__init__()
        self.dw, self.n, self.io = dw, n_layers, io

    def forward(self, x, c, si, b):
        e = x.new_zeros(x.shape[0], 2, 0, 128)
        logits, *present = self.dw(x, c, si, b, *([e] * (2 * self.n)))
        present = [p.contiguous() for p in present]
        if self.io == 1:
            return (logits.contiguous(), *present)
        nxt, alive = _next(logits, None, PADDLE_FILL, (PADDLE_EOS,))
        return (nxt, alive, *present)


class PaddleStep(nn.Module):
    def __init__(self, dw, io: int):
        super().__init__()
        self.dw, self.io = dw, io

    def forward(self, x, c, si, b, *rest):
        if self.io == 1:
            return tuple(o.contiguous() for o in self.dw(x, c, si, b, *rest))
        live, past = rest[0], rest[1:]
        logits, *present = self.dw(x, c, si, b, *past)
        nxt, alive = _next(logits, live, PADDLE_FILL, (PADDLE_EOS,))
        return (nxt, alive, *(p.contiguous() for p in present))


def paddle(prec: str, device: torch.device, io: int, model=None, fuse: bool = False, precast: bool = True) -> Graphs:
    from onnx_export.paddle import host as H
    from onnx_export.paddle.export import load_model, prompt_ids
    from onnx_export.paddle.modules import DecoderWrap, VisionWrap

    name = {"fp32": "float32", "bf16": "bfloat16", "fp16": "float16"}[prec]
    meta = {}
    if model is None:
        model, proc = load_model(name)
        pre, suf = prompt_ids(proc)
        meta["prompt"] = json.dumps({"prefix": pre, "suffix": suf})
    model = _to(model.eval(), device)
    dt = DT[prec]
    vw = VisionWrap(model).eval()
    dw = (FusedPaddleDecoder(model) if fuse and io == 2 else DecoderWrap(model)).eval()
    g = torch.Generator().manual_seed(0)
    gh, gw = 4, 20
    aux = H.vision_aux(gh, gw)
    vargs = (torch.randn(gh * gw, 3, 14, 14, generator=g).to(dt), *(torch.from_numpy(a) for a in aux))
    vargs = tuple(t.to(device) for t in vargs)
    N = Dim("N4", min=4, max=2048)
    graphs = [Graph("vision", vw, vargs, ({0: 4 * N},) * 6)]
    B, S, P = 2, 7, 5
    Bd, Sd, Ld = Dim("b", min=1, max=64), Dim.DYNAMIC, Dim.DYNAMIC
    x = torch.randn(B, S, 1024, generator=g).to(device, dt)
    cs = torch.randn(B, S, 128, generator=g).to(device, dt)
    sn = torch.randn(B, S, 128, generator=g).to(device, dt)
    bias = torch.zeros(B, 1, S, S, dtype=dt, device=device)
    graphs.append(Graph("prefill", PaddlePrefill(dw, H.N_LAYERS, io), (x, cs, sn, bias),
                        ({0: Bd, 1: Sd}, {0: Bd, 1: Sd}, {0: Bd, 1: Sd}, {0: Bd, 2: Sd, 3: Sd})))
    past = [torch.randn(B, 2, P, 128, generator=g).to(device, dt) for _ in range(2 * H.N_LAYERS)]
    xs, c1, s1 = x[:, :1].contiguous(), cs[:, :1].contiguous(), sn[:, :1].contiguous()
    b1 = torch.zeros(B, 1, 1, P + 1, dtype=dt, device=device)
    dyn = [{0: Bd}, {0: Bd}, {0: Bd}, {0: Bd, 3: Dim.DYNAMIC}]
    if io == 1:
        graphs.append(Graph("step", PaddleStep(dw, io), (xs, c1, s1, b1, *past), tuple(dyn + [tuple([{0: Bd, 2: Ld}] * (2 * H.N_LAYERS))])))
    else:
        live = torch.ones(B, dtype=torch.bool, device=device)
        graphs.append(Graph("step", PaddleStep(dw, io), (xs, c1, s1, b1, live, *past),
                            (*dyn, ({0: Bd}, *[{0: Bd, 2: Ld}] * (2 * H.N_LAYERS)))))
    out = Graphs("paddle-manga", prec, io, model, graphs, meta=meta)
    if fuse and io == 2:
        out.extra = {f"fused.decoder.{n}": t for n, t in dw.named_parameters() if n.startswith(("qkv.", "gu."))}
        out.meta["fused"] = "qkv,gate_up"
    # the host loop's input-embedding table: from the blob, in the model dtype
    out.host_weights["host.embed_tokens"] = "model.language_model.embed_tokens.weight"
    return out


BUILDERS = {"hayai-nova": hayai, "paddle-manga": paddle}
