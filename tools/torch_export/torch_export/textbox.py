"""The text-box recognizers as weightless packages bound to the authors' own weights.

nar-small (muscgab, branch r16) is one graph, role ``model``: the authors' Apache-2.0
``model.py`` at the pin, wrapped as ``NarGraph`` (docs/rust-port/TEXTBOX-GPU.md §7.3).
Unlike hayai and paddle, nothing is rewritten into ``weights-<group>.safetensors``: every
constant of the package is ``<prefix><key>`` of the authors' unmodified
``model.safetensors``, which the host fetches from the authors' repository at the pinned
commit and binds at load (package metadata ``bunko.weights`` maps FQN -> key). The
exporter refuses to write a package whose constants are not all keys of that file with
the same shape and dtype, and refuses the weights-inside layouts
(``TORCH_EXPORT_WIN_EMBED``, ``TORCH_EXPORT_CPU_EMBED``) outright.
"""

from __future__ import annotations

import hashlib
import importlib.util
import os
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

import torch
import torch.nn.functional as F
from torch import nn
from torch.export import Dim

from .graphs import Graph, Graphs, _to

IO = "textbox-1"  # package I/O id (bunko.io), checked by the cdylib's GraphSet::load
MAX_BATCH = 64  # the package's largest batch (bunko.max_batch)
EMBED_VARS = ("TORCH_EXPORT_WIN_EMBED", "TORCH_EXPORT_CPU_EMBED")
SNAPSHOT_VAR = "TORCH_EXPORT_NAR_SNAPSHOT"


@dataclass(frozen=True)
class AuthorsWeights:
    """The authors' weights file a package binds, pinned by commit, sha256 and size."""

    repo: str
    revision: str
    file: str
    sha256: str
    size: int
    prefix: dict[str, str]  # role -> the FQN prefix its constants carry before the file's key


NAR_PIN = AuthorsWeights("muscgab/manga-ocr-nar-small", "8cc14c152ccb21da3aa88fa51a92766537c12da2", "model.safetensors",
                         "bc7418e27d476f86bb5984b45974d69cf9f7623eff6b9b57012df2c8fc9920a3", 217035140, {"model": "net."})

# engines whose packages bind the authors' weights (build.py refuses embedding for these
# before it builds anything)
PINS: dict[str, AuthorsWeights] = {"nar-small": NAR_PIN}


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while b := f.read(1 << 24):
            h.update(b)
    return h.hexdigest()


def check_pin(path: Path, pin: AuthorsWeights) -> None:
    """Refuse any file but the pinned one (size first: it is free)."""
    size = path.stat().st_size
    if size != pin.size:
        raise SystemExit(f"{path}: {size} bytes, pinned {pin.size} ({pin.repo}@{pin.revision} {pin.file})")
    got = _sha256(path)
    if got != pin.sha256:
        raise SystemExit(f"{path}: sha256 {got}, pinned {pin.sha256} ({pin.repo}@{pin.revision} {pin.file})")


def authors_weight_map(fqns: list[str], prefix: str, stored: dict[str, tuple[tuple[int, ...], str]],
                       wanted: dict[str, tuple[tuple[int, ...], str]]) -> dict[str, str]:
    """constant FQN -> the authors' key (the FQN minus ``prefix``).

    ``stored``: the weights file's header, key -> (shape, safetensors dtype name);
    ``wanted``: the exported program's constants by the same key. Every constant must be
    a key of the file with the same shape and dtype: anything else would be a weight left
    inside the package, or a binding that reads the wrong bytes."""
    out = {}
    for fqn in fqns:
        if not fqn.startswith(prefix):
            raise SystemExit(f"{fqn} is not under {prefix!r}: a constant that is not one of the authors' weights")
        key = fqn[len(prefix):]
        if key not in stored:
            raise SystemExit(f"{fqn} has no key {key} in the authors' weights file")
        if key not in wanted:
            raise SystemExit(f"{fqn}: no shape or dtype for {key} in the exported program")
        (s_shape, s_dt), (w_shape, w_dt) = stored[key], wanted[key]
        if tuple(s_shape) != tuple(w_shape):
            raise SystemExit(f"{fqn}: shape {tuple(w_shape)} in the graph, {tuple(s_shape)} in the authors' file")
        if s_dt != w_dt:
            raise SystemExit(f"{fqn}: dtype {w_dt} in the graph, {s_dt} in the authors' file")
        out[fqn] = key
    return out


def refuse_embedding(env: Mapping[str, str]) -> None:
    """The authors' weights never go inside a package (their licence is not ours to grant)."""
    for k in EMBED_VARS:
        if env.get(k, "") not in ("", "0"):
            raise SystemExit(f"{k}={env[k]}: text-box packages are weightless; "
                             "the authors' weights are never put inside a package")


def textbox_meta(pin: AuthorsWeights, max_batch: int) -> dict[str, str]:
    """Package metadata (build.py adds the ``bunko.`` prefix)."""
    return {"io": IO, "source": f"{pin.repo}@{pin.revision}", "weights_sha256": pin.sha256,
            "weights_size": str(pin.size), "max_batch": str(max_batch)}


def _export_nar():
    """tools/textbox_export/export_nar.py (the CPU graph's exporter): its pins and loader."""
    path = Path(__file__).resolve().parents[2] / "textbox_export" / "export_nar.py"
    name = "bunko_textbox_export_nar"
    if name not in sys.modules:
        spec = importlib.util.spec_from_file_location(name, path)
        mod = importlib.util.module_from_spec(spec)
        sys.modules[name] = mod
        spec.loader.exec_module(mod)
    return sys.modules[name]


def drop_recomputed_buffers(net: nn.Module) -> list[str]:
    """Remove the model's non-persistent buffers (nar: ``rope_cos``, ``rope_sin`` and the
    vision rotary ``inv_freq``), which ``NarGraph`` computes in the graph instead. Export
    lifts every buffer, used or not; without them the exported constants are exactly the
    authors' tensors, so the weight map can refuse anything else. Returns their names."""
    gone = []
    for mname, m in net.named_modules():
        for b in sorted(m._non_persistent_buffers_set):
            del m._buffers[b]
            m._non_persistent_buffers_set.discard(b)
            gone.append(f"{mname}.{b}" if mname else b)
    return gone


class NarGraph(nn.Module):
    """The authors' prepare() + forward() (model.py at r16), with every derived constant
    computed in the graph from the weights (so the package's constants are exactly the
    safetensors tensors) and the non-persistent RoPE buffers recomputed from code.
    I/O as Bunko's ONNX graph plus a batch dim: P [B,324,588] f32, V [B,324] f32,
    R/C [B,324] i32, H/W [B] i32 -> ids [B,161] i64."""

    def __init__(self, net):
        super().__init__()
        self.net = net
        m = sys.modules[type(net).__module__]  # the authors' model.py, imported by load_net
        self._rope, self._rotate_half = m._rope, m.rotate_half
        self._enc = (m.ENC_DIM, m.ENC_HEADS, m.ENC_FFN)

    def forward(self, P, V, R, C, H, W):
        net = self.net
        _rope, rotate_half = self._rope, self._rotate_half
        ENC_DIM, ENC_HEADS, ENC_FFN = self._enc
        vit = net.encoder.vit
        dt = net.slot_pos.dtype
        d = P.device
        # --- prepare(), in-graph ---
        fc13 = [torch.cat((b.mlp.fc1.weight, b.mlp.fc3.weight), 0) for b in vit.blocks]
        # _Rotary(32): inv_freq buffer (fp32 at init, cast to dt by net.to(dtype)), table in dt, then .float()
        inv_freq = (1.0 / (10000.0 ** (torch.arange(0, 32, 2, dtype=torch.float, device=d) / 32))).to(dt)
        rope_table = torch.outer(torch.arange(325, device=d, dtype=dt), inv_freq).float()
        alpha = net.memory.depth_logits.softmax(-1).to(dt)
        hd_dec = net.blocks[0].hd
        inv = 1.0 / (10000 ** (torch.arange(0, hd_dec // 2, 2, device=d).float() / (hd_dec // 2)))
        ang = torch.outer(torch.arange(net.slots, device=d).float(), inv)
        cos_d = torch.cat((ang, ang), -1).cos()[None, None].to(dt)
        sin_d = torch.cat((ang, ang), -1).sin()[None, None].to(dt)
        s0 = (net.slot_pos + net.tok.weight[6818])[None]
        blk, c = net.blocks[0], net.cond[0]
        _, S0, Dm0 = s0.shape
        q, k, v = blk.qkv(c.n1(s0)).chunk(3, -1)
        q, k, v = blk.sq(blk.split(q)), blk.sk(blk.split(k)), blk.split(v)
        att = F.scaled_dot_product_attention(_rope(q, cos_d, sin_d, blk.hd // 2), _rope(k, cos_d, sin_d, blk.hd // 2), v)
        s_after_sa0 = s0 + c.s1 * blk.o1(att.transpose(1, 2).reshape(1, S0, Dm0))
        # --- inputs as the authors' PyTorch backend feeds them ---
        P = P.to(dt)
        V = V > 0.5
        R, C, H, W = R.long(), C.long(), H.long(), W.long()
        # --- forward(), verbatim except the constants above ---
        B, N, _ = P.shape
        pe = vit.patch_embed.patchifier
        x = pe.norm(F.linear(P, pe.proj.weight.flatten(1), pe.proj.bias))
        emb = torch.stack((rope_table[R], rope_table[C]), -1).flatten(-2)
        cos = torch.cat((emb.cos(), emb.cos()), -1)[:, :, None]
        sin = torch.cat((emb.sin(), emb.sin()), -1)[:, :, None]
        mask = V[:, None, None, :]
        hd = ENC_DIM // ENC_HEADS
        states = [x]
        for blk, w13 in zip(vit.blocks, fc13):
            q, k, v = blk.attn.qkv(blk.norm1(x)).view(B, N, 3, ENC_HEADS, hd).unbind(2)
            q = (q.float() * cos + rotate_half(q.float()) * sin).to(x.dtype)
            k = (k.float() * cos + rotate_half(k.float()) * sin).to(x.dtype)
            att = F.scaled_dot_product_attention(q.transpose(1, 2), k.transpose(1, 2), v.transpose(1, 2), attn_mask=mask)
            x = x + blk.attn.proj(att.transpose(1, 2).reshape(B, N, ENC_DIM))
            g, u = F.linear(blk.norm2(x), w13).split(ENC_FFN, -1)
            x = x + blk.mlp.fc2(F.silu(g) * u)
            states.append(x)
        states[-1] = vit.post_trunk_norm(x)
        mem = net.memory
        h12 = states[-1]
        base = mem.bridge(h12) + mem.pos(mem.fourier(R, C, H, W).to(h12.dtype))
        Hn = torch.stack([nm(s) for nm, s in zip(mem.norms, states[:-1])], 0)
        f = torch.einsum("jl,lbnd->jbnd", alpha, Hn)
        mems = base[None] + mem.gamma[:, None, None] * mem.wo(f)
        mems = torch.cat((mem.null.expand(mems.shape[0], B, -1, -1), mems), 2)
        kvs = [net.blocks[j].memory_kv(m) for j, m in enumerate(mems.unbind(0))]
        km = torch.cat((torch.ones(B, 2, dtype=torch.bool, device=V.device), V), 1)[:, None, None, :]
        s = s_after_sa0.expand(B, -1, -1)
        S, Dm = s.shape[1], s.shape[2]
        for i in range(len(net.blocks)):
            blk, c = net.blocks[i], net.cond[i]
            if i > 0:
                q, k, v = blk.qkv(c.n1(s)).chunk(3, -1)
                q, k, v = blk.sq(blk.split(q)), blk.sk(blk.split(k)), blk.split(v)
                half = blk.hd // 2
                att = F.scaled_dot_product_attention(_rope(q, cos_d, sin_d, half), _rope(k, cos_d, sin_d, half), v)
                s = s + c.s1 * blk.o1(att.transpose(1, 2).reshape(B, S, Dm))
            q = blk.cq(blk.split(blk.q(c.n2(s))))
            att = F.scaled_dot_product_attention(q, *kvs[i], attn_mask=km)
            s = s + c.s2 * blk.o2(att.transpose(1, 2).reshape(B, S, Dm))
            g, u = blk.gate_up(c.n3(s)).chunk(2, -1)
            s = s + c.s3 * blk.down(F.silu(g) * u)
        hidden = net.final_norms[0](s)
        return (F.linear(hidden, net.tok.weight[:6818]) + net.bias).argmax(-1)


def nar_small(prec: str, device: torch.device, io: int, model=None, **kw) -> Graphs:
    """nar-small r16 as one graph (role ``model``), dynamic batch 1..MAX_BATCH.

    ``model`` None: the authors' repository at the pin, from ``TORCH_EXPORT_NAR_SNAPSHOT``
    or the Hugging Face cache (fetched once), every pinned file checked. ``io`` and the
    hayai/paddle options in ``kw`` do not apply (the package's I/O id is ``textbox-1``)."""
    if prec != "fp32":
        raise SystemExit(f"nar-small: no {prec} build yet (fp32 only)")
    snap = None
    if model is None:
        E = _export_nar()
        snap = E.snapshot_dir(os.environ.get(SNAPSHOT_VAR))
        E.check_pins(snap)
        model = E.load_net(snap)
    drop_recomputed_buffers(model)
    model = _to(model.eval(), device)
    wrap = NarGraph(model).eval()
    b, n = 3, 324
    ex = (torch.zeros(b, n, 588), torch.ones(b, n), torch.zeros(b, n, dtype=torch.int32),
          torch.zeros(b, n, dtype=torch.int32), torch.ones(b, dtype=torch.int32), torch.ones(b, dtype=torch.int32))
    ex = tuple(t.to(device) for t in ex)
    B = Dim("b", min=1, max=MAX_BATCH)
    graph = Graph("model", wrap, ex, ({0: B},) * 6)
    return Graphs("nar-small", prec, io, model, [graph], authors_weights=NAR_PIN,
                  authors_file=snap / NAR_PIN.file if snap is not None else None, max_batch=MAX_BATCH)
