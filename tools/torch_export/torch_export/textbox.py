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

charspot-small (muscgab, ``CHARSPOT_PIN``) is two graphs, roles ``dense`` and ``tail``,
over the authors' Apache-2.0 ``model.py`` at the pin, bound the same way to the authors'
``model.safetensors`` (TEXTBOX-GPU.md §7.4). fp32 only.
"""

from __future__ import annotations

import hashlib
import importlib.util
import json
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
# charspot-small's file holds both networks under "dense." and "tail."; the graphs carry
# them one level deeper ("dense.dense.<k>", "tail.tail.<k>"), so the FQN minus the role's
# prefix is the file's own key
CHARSPOT_PIN = AuthorsWeights("muscgab/manga-ocr-charspot-small", "72ca29202673880ca9c67b1c02c107535be4472b",
                              "model.safetensors", "3dec28460e10353b8a12d0e6d252bff9b892ec2d12ffb7a3d81ae02532963286",
                              43860012, {"dense": "dense.", "tail": "tail."})

# engines whose packages bind the authors' weights (build.py refuses embedding for these
# before it builds anything)
PINS: dict[str, AuthorsWeights] = {"nar-small": NAR_PIN, "charspot-small": CHARSPOT_PIN}


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


# --- charspot-small -------------------------------------------------------------------
#
# Two graphs over the authors' Apache-2.0 ``model.py`` at CHARSPOT_PIN (TEXTBOX-GPU.md
# §7.4): ``dense`` (the masked dense network: a crop padded onto a larger canvas reads as
# it does alone) and ``tail`` (pooled box features, classes, reading-order scores). The
# decode between them (heatmap, NMS, furigana, geometry) and the reading-order chain run
# on the host: the authors' ``core.py`` here and in verify, its Rust port in Bunko.

CHARSPOT_SNAPSHOT_VAR = "TORCH_EXPORT_CHARSPOT_SNAPSHOT"
# sha256 of the authors' files the exporter, verify and tools/textbox_export read, from the
# repository's own SHA256SUMS at the pin (model.onnx: the CPU reference only)
CHARSPOT_FILES = {
    "model.safetensors": CHARSPOT_PIN.sha256,
    "model.onnx": "05fb79760f8e3dbc989b61f23a34c46b511818a2166573ce7e41d994f4403843",
    "config.json": "fbab49bfbfee9e9fee22e74288202a7928e268e9b03de185aa9e206614b0a65c",
    "vocab.json": "9f72fc7852b1432bcb61b3b94b2e4cd8abef52b01e9051550a6a3070e2b314d5",
    "LICENSE": "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30",
    "LICENSE_MODEL": "e66c269d4819aaab34b49ef5220c4ddab6756f21bb5180761a4eb8561f2b7bbd",
    "NOTICE.md": "6f81dc2287f230c2d1329dd819552313d2e920e64d820c36f2a514073a449d35",
    "manga_ocr_charspot_small/__init__.py": "8693db4d83b64d8e2fdd994901c3c7ae2af6135041e0ecbcdcf98460ab48e1c7",
    "manga_ocr_charspot_small/core.py": "a907e94f21b40641027d014dfea36df6489997809dd053dca3987d4965650a65",
    "manga_ocr_charspot_small/model.py": "4ec554862b56171bbd85a42b7e27d88621b24617a5ddbbffd2091dfe421e7895",
    "manga_ocr_charspot_small/gpu_preprocess.py": "391647938bce2d5cc425097ff4fd14fa270c6b68ac9026cf0cc60dcd6af8a598",
    "manga_ocr_charspot_small/backends/__init__.py": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    "manga_ocr_charspot_small/backends/onnx.py": "2da0c938e3f4c623969cdfb39f1279a20e8e09fecfb088850d67e4e5e7f70ca9",
    "manga_ocr_charspot_small/backends/cuda.py": "60cbe33a615265ccdd6f85aa580d14f6526d26e60499f2fe68cf537f0a708235",
}
CHARSPOT_MAX_CELLS = 256  # a canvas side, in 16-px cells (the authors' ladder ends at 256)
# The dense graph's smallest canvas side, in cells. Exported with 1, torch.export guards
# the graph to >= 2 anyway (a 1-cell side makes size-1 dimensions, which it specialises),
# so it is declared as 2: the host puts a crop with a 1-cell side (the ladder's first
# rung; none of JMangaBench_Read's 7,929 crops has one) on a 2-cell canvas, which the
# masked network reads as the crop alone (measured on the CPU: max |diff| 9e-5 on emb,
# float noise).
CHARSPOT_MIN_CELLS = 2
CHARSPOT_MAX_ROWS = 512  # tail rows per call (the authors' ROWS_PER_TAIL)
CHARSPOT_MAX_CHARS = 1024  # characters per tail row: 256 cells hold at most 1,024 stride-8 centres
CHARSPOT_TAIL_BUDGET = 1 << 18  # rows x characters² per tail call (the authors' tail_budget)


def charspot_snapshot(arg: str | None = None) -> Path:
    """The authors' repository at CHARSPOT_PIN: ``arg``, else ``TORCH_EXPORT_CHARSPOT_SNAPSHOT``,
    else the Hugging Face cache (fetched once at the pinned revision)."""
    arg = arg or os.environ.get(CHARSPOT_SNAPSHOT_VAR)
    if arg:
        return Path(arg)
    from huggingface_hub import snapshot_download

    return Path(snapshot_download(CHARSPOT_PIN.repo, revision=CHARSPOT_PIN.revision, allow_patterns=list(CHARSPOT_FILES)))


def check_charspot(snap: Path, files=None) -> None:
    """Refuse a snapshot whose files are not the pinned ones (``files``: the names to check,
    default all of CHARSPOT_FILES)."""
    check_pin(snap / CHARSPOT_PIN.file, CHARSPOT_PIN)
    for rel in files if files is not None else CHARSPOT_FILES:
        if rel == CHARSPOT_PIN.file:
            continue
        got = _sha256(snap / rel)
        if got != CHARSPOT_FILES[rel]:
            raise SystemExit(f"{snap / rel}: sha256 {got}, pinned {CHARSPOT_FILES[rel]} "
                             f"({CHARSPOT_PIN.repo}@{CHARSPOT_PIN.revision})")


def charspot_modules(snap: Path):
    """The authors' ``core`` and ``model`` modules from the snapshot."""
    if str(snap) not in sys.path:
        sys.path.insert(0, str(snap))
    from manga_ocr_charspot_small import core, model

    return core, model


class CharspotNet(nn.Module):
    """The authors' two networks under one root: its state_dict keys are the weights
    file's own keys (``dense.<k>``, ``tail.<k>``)."""

    def __init__(self, dense: nn.Module, tail: nn.Module):
        super().__init__()
        self.dense, self.tail = dense, tail


def load_charspot(snap: Path) -> CharspotNet:
    """The authors' ``build(cfg, n_out)`` with the pinned weights, fp32, CPU (strict load:
    every key of the file is one of the networks' tensors and the reverse)."""
    from safetensors.torch import load_file

    _, model = charspot_modules(snap)
    cfg = json.loads((snap / "config.json").read_text(encoding="utf-8"))
    w = load_file(str(snap / CHARSPOT_PIN.file))
    n_out = w["tail.cls.weight"].shape[0]
    if n_out != cfg["n_out"]:
        raise SystemExit(f"{n_out} output classes in the weights, config.json says {cfg['n_out']}")
    net = CharspotNet(*model.build(cfg, n_out))
    net.load_state_dict(w, strict=True)
    return net.eval()


class DenseGraph(nn.Module):
    """``dense(img [B,3,16h,16w] f32 raw 0..255, hw [B,2] i64 valid px) -> heat [B,1,2h,2w],
    box [B,4,..], ruby [B,1,..], emb [B,144,..]``: the authors' masked ``Dense.forward``."""

    def __init__(self, net: CharspotNet):
        super().__init__()
        self.dense = nn.ModuleDict({"dense": net.dense})  # FQN dense.dense.<k>

    def forward(self, img, hw):
        return self.dense["dense"](img, hw)


class TailGraph(nn.Module):
    """``tail(emb [R,144,H8,W8], boxes [R,N,4] input px, geo [R,N,4], mask [R,N] bool) ->
    ids [R,N] i64, pair [R,N,N], start [R,N], end [R,N]``: the authors' ``Tail.pooled`` then
    ``Tail.forward``. ``pooled``'s sample points (the non-persistent ``qx``/``qy`` buffers,
    which are not in the weights file) are computed in the graph: 0.25/0.75 exactly."""

    def __init__(self, net: CharspotNet):
        super().__init__()
        self.tail = nn.ModuleDict({"tail": net.tail})  # FQN tail.tail.<k>

    def forward(self, emb, boxes, geo, mask):
        t = self.tail["tail"]
        q = torch.arange(4, device=boxes.device)
        qx = (q % 2).to(boxes.dtype) * 0.5 + 0.25  # [0.25, 0.75, 0.25, 0.75]
        qy = (q // 2).to(boxes.dtype) * 0.5 + 0.25  # [0.25, 0.25, 0.75, 0.75]
        # Tail.pooled, verbatim but for qx/qy
        B, N = boxes.shape[:2]
        H8, W8 = emb.shape[-2:]
        px = boxes[..., 0:1] + qx * (boxes[..., 2:3] - boxes[..., 0:1])
        py = boxes[..., 1:2] + qy * (boxes[..., 3:4] - boxes[..., 1:2])
        grid = torch.stack((px / (W8 * 8) * 2 - 1, py / (H8 * 8) * 2 - 1), -1)
        s = F.grid_sample(emb, grid, mode="bilinear", align_corners=False)
        e = t.pool(s.permute(0, 2, 3, 1).reshape(B, N, -1))
        return t(e, geo, mask)


def charspot_small(prec: str, device: torch.device, io: int, model=None, **kw) -> Graphs:
    """charspot-small as two graphs, ``dense`` (B 1..64, canvas sides 2..256 cells) and
    ``tail`` (R 1..512 rows, N 2..1024 characters), fp32 only (the authors' fp16 differs on
    3/100 texts). ``model`` None: the authors' repository at the pin (``charspot_snapshot``),
    every pinned file the export reads checked. ``io`` and ``kw`` do not apply."""
    if prec != "fp32":
        raise SystemExit(f"charspot-small: no {prec} build (fp32 only)")
    snap = None
    if model is None:
        snap = charspot_snapshot()
        check_charspot(snap, [CHARSPOT_PIN.file, "config.json", "manga_ocr_charspot_small/model.py"])
        model = load_charspot(snap)
    # (none left on a copy of a model this builder already had: build.py exports a copy)
    gone = set(drop_recomputed_buffers(model))
    if not gone <= {"tail.qx", "tail.qy"}:
        raise SystemExit(f"charspot-small: non-persistent buffers {sorted(gone)}; the graph computes only tail.qx and tail.qy")
    model = _to(model.eval(), device)
    dense, tail = DenseGraph(model).eval(), TailGraph(model).eval()
    # three crops on a 6 x 4 cell canvas (one padded), as the authors' batches make them
    img = torch.zeros(3, 3, 16 * 6, 16 * 4, device=device)
    hw = torch.tensor([[96, 64], [80, 48], [96, 32]], dtype=torch.int64, device=device)
    B = Dim("b", min=1, max=MAX_BATCH)
    Hc = Dim("hc", min=CHARSPOT_MIN_CELLS, max=CHARSPOT_MAX_CELLS)
    Wc = Dim("wc", min=CHARSPOT_MIN_CELLS, max=CHARSPOT_MAX_CELLS)
    emb_dim = model.dense.emb.out_channels
    r, n = 3, 5
    emb = torch.zeros(r, emb_dim, 12, 8, device=device)
    boxes = (torch.arange(r * n * 4, dtype=torch.float32, device=device) % 40).reshape(r, n, 4)
    geo = torch.ones(r, n, 4, device=device)
    mask = torch.ones(r, n, dtype=torch.bool, device=device)
    R, N = Dim("r", min=1, max=CHARSPOT_MAX_ROWS), Dim("n", min=2, max=CHARSPOT_MAX_CHARS)
    H8, W8 = Dim("h8", min=2, max=2 * CHARSPOT_MAX_CELLS), Dim("w8", min=2, max=2 * CHARSPOT_MAX_CELLS)
    graphs = [Graph("dense", dense, (img, hw), ({0: B, 2: 16 * Hc, 3: 16 * Wc}, {0: B})),
              Graph("tail", tail, (emb, boxes, geo, mask), ({0: R, 2: H8, 3: W8}, {0: R, 1: N}, {0: R, 1: N}, {0: R, 1: N}))]
    return Graphs("charspot-small", prec, io, model, graphs, authors_weights=CHARSPOT_PIN,
                  authors_file=snap / CHARSPOT_PIN.file if snap is not None else None, max_batch=MAX_BATCH)


def charspot_result(seq, ids, kb, sc, fx_fy, out_str) -> dict:
    """The authors' ``core.result`` without its rounding: ``{"text", "chars": [[c, x0, y0, x1,
    y1, score], ...]}`` in reading order, boxes in crop pixels (f32 box times the f64 factor,
    as ``core.result`` computes them before it rounds)."""
    fx, fy = fx_fy
    chars = []
    for k in seq:
        x0, y0, x1, y1 = (float(v) for v in kb[k])
        chars.append([out_str[int(ids[k])], x0 * fx, y0 * fy, x1 * fx, y1 * fy, float(sc[k])])
    return {"text": "".join(c[0] for c in chars), "chars": chars}


def charspot_tail_parts(counts: list[int], budget: int = CHARSPOT_TAIL_BUDGET, rows_max: int = CHARSPOT_MAX_ROWS) -> list[list[int]]:
    """The authors' tail parts (``backends/cuda.py`` ``infer``): rows with characters, by
    count descending (stable); a part always takes its first row, then more while it has
    fewer than ``rows_max`` rows and ``(rows + 1) * n0² <= budget``. Indices into ``counts``.
    (Bunko's ``bunko_vlm::textbox::tail_parts`` is the same rule.)"""
    rows = sorted((i for i, c in enumerate(counts) if c > 0), key=lambda i: -counts[i])
    parts, k = [], 0
    while k < len(rows):
        nc, j = counts[rows[k]], k + 1
        while j < len(rows) and j - k < rows_max and (j - k + 1) * nc * nc <= budget:
            j += 1
        part = rows[k:k + max(1, j - k)]
        parts.append(part)
        k += len(part)
    return parts
