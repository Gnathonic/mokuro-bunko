"""Torch-free host side of hayai-nova over the exported graphs (the reference for the Rust host).

Spec: docs/rust-port/spec/ocr-recognizers.md §5.3-§5.11. Preprocessing is PIL
(the port's reference), not transformers' torchvision path.
"""

from __future__ import annotations

import json
import math
from pathlib import Path

import numpy as np
from PIL import Image

PATCH = 16
D_AXIS = 32
NEG = -1e9
MAX_NEW = 96  # HAYAI_NOVA_MAX_NEW_TOKENS
BATCH = 16  # HAYAI_NOVA_BATCH
N_LAYERS = 12


def size_for_budget(h: int, w: int, budget: int, eps: float = 1e-5) -> tuple[int, int]:
    def scaled(scale: float, size: int) -> int:
        return int(max(PATCH, math.ceil(size * scale / PATCH) * PATCH))

    lo, hi = eps / 10, 100.0
    while hi - lo >= eps:
        s = (lo + hi) / 2
        if (scaled(s, h) / PATCH) * (scaled(s, w) / PATCH) <= budget:
            lo = s
        else:
            hi = s
    return scaled(lo, h), scaled(lo, w)


def preprocess(images: list[Image.Image], budget: int = 512):
    b = len(images)
    pv = np.zeros((b, budget, PATCH * PATCH * 3), np.float32)
    mask = np.zeros((b, budget), np.float32)
    shapes = np.zeros((b, 2), np.int64)
    for i, im in enumerate(images):
        im = im.convert("RGB")
        th, tw = size_for_budget(im.height, im.width, budget)
        arr = np.asarray(im.resize((tw, th), Image.BILINEAR), np.float32)
        arr = (arr * (1 / 255) - 0.5) / 0.5
        ph, pw = th // PATCH, tw // PATCH
        pv[i, : ph * pw] = arr.reshape(ph, PATCH, pw, PATCH, 3).transpose(0, 2, 1, 3, 4).reshape(ph * pw, -1)
        mask[i, : ph * pw] = 1
        shapes[i] = (ph, pw)
    return pv, mask, shapes


def aa_weights(inp: int, out: int) -> np.ndarray:
    """torch ``antialias=True`` bilinear weights (align_corners=False), (out, inp), f32."""
    scale = inp / out
    fs = max(scale, 1.0)
    W = np.zeros((out, inp), np.float64)
    for i in range(out):
        center = scale * (i + 0.5)
        xmin = max(int(center - fs + 0.5), 0)
        xmax = min(int(center + fs + 0.5), inp)
        ws = np.array([max(0.0, 1.0 - abs((j - center + 0.5) / fs)) for j in range(xmin, xmax)])
        if ws.sum() > 0:
            ws /= ws.sum()
        W[i, xmin:xmax] = ws
    return W.astype(np.float32)


class PosCache:
    def __init__(self, table: np.ndarray) -> None:
        n = math.isqrt(table.shape[0])
        self.grid = table.reshape(n, n, -1).astype(np.float32)
        self.cache: dict[tuple[int, int], np.ndarray] = {}

    def get(self, h: int, w: int) -> np.ndarray:
        if (h, w) not in self.cache:
            wy, wx = aa_weights(self.grid.shape[0], h), aa_weights(self.grid.shape[1], w)
            self.cache[(h, w)] = np.einsum("hy,yxc,wx->hwc", wy, self.grid, wx).reshape(h * w, -1)
        return self.cache[(h, w)]


def _freqs() -> np.ndarray:
    return 1.0 / (10000.0 ** (np.arange(0, D_AXIS, 2, dtype=np.float32) / D_AXIS))


def vis_freqs(h: int, w: int):
    f = _freqs()
    fy = np.outer(np.arange(h, dtype=np.float32), f)
    fx = np.outer(np.arange(w, dtype=np.float32), f)
    a = np.concatenate([np.broadcast_to(fy[:, None], (h, w, 16)), np.broadcast_to(fx[None], (h, w, 16))], -1).reshape(h * w, -1)
    return np.cos(a), np.sin(a)


def text_freqs(n: int):
    a = np.outer(np.arange(n, dtype=np.float32), _freqs())
    a = np.concatenate([a, a], -1)
    return np.cos(a), np.sin(a)


def vision_inputs(pv, mask, shapes, pos: PosCache):
    b, P, _ = pv.shape
    posb = np.zeros((b, P, 768), np.float32)
    outs = []
    for i, (hp, wp) in enumerate(shapes.tolist()):
        r = pos.get(hp, wp)
        posb[i, : hp * wp] = r
        posb[i, hp * wp :] = r[0]
        outs.append((hp, wp, -(-hp // 2), -(-wp // 2)))
    M = max(ho * wo for *_, ho, wo in outs)
    idx = np.zeros((b, M, 4), np.int64)
    valid = np.zeros((b,), np.int64)
    for i, (hp, wp, ho, wo) in enumerate(outs):
        y, x = np.divmod(np.arange(ho * wo), wo)
        for k, (dy, dx) in enumerate(((0, 0), (0, 1), (1, 0), (1, 1))):
            idx[i, : ho * wo, k] = np.minimum(2 * y + dy, hp - 1) * wp + np.minimum(2 * x + dx, wp - 1)
        valid[i] = ho * wo
    tok_valid = (np.arange(M)[None] < valid[:, None]).astype(np.float32)
    return posb, idx, tok_valid, valid, [(ho, wo) for *_, ho, wo in outs], M


class OrtBackend:
    """The two exported graphs on ORT's CPU EP; I/O cast to whatever dtype each file declares."""

    def __init__(self, vision_path: Path, decoder_path: Path, threads: int = 0, audit: dict | None = None) -> None:
        from ..common import ort_session

        self.v = ort_session(vision_path, threads)
        self.d = ort_session(decoder_path, threads)
        self.vin = [i.name for i in self.v.get_inputs()]
        self.din = [i.name for i in self.d.get_inputs()]
        self.vdt = np.float16 if "float16" in self.v.get_inputs()[0].type else np.float32
        self.ddt = np.float16 if "float16" in self.d.get_inputs()[0].type else np.float32
        self.audit = audit or {}

    def vision(self, pv, pm, pos, idx, tv):
        f = self.vdt
        feed = dict(zip(self.vin, [pv.astype(f), pm.astype(f), pos.astype(f), idx, tv.astype(f)], strict=True))
        if "vision" in self.audit:
            return self.audit["vision"].run(feed)[0].astype(np.float32)
        return self.v.run(None, feed)[0].astype(np.float32)

    def decode(self, x, mask, cos, sin, past):
        f = self.ddt
        if past is None:
            past = [np.zeros((x.shape[0], 2, 0, 64), f)] * (2 * N_LAYERS)
        with np.errstate(over="ignore"):  # the -1e9 mask becomes -inf in f16, by design (spec §5.8)
            feed = dict(zip(self.din, [x.astype(f), mask.astype(f), cos.astype(f), sin.astype(f), *past], strict=True))
        out = self.audit["decoder"].run(feed) if "decoder" in self.audit else self.d.run(None, feed)
        return out[0].astype(np.float32), out[1:]


class NovaHost:
    def __init__(self, backend, pos_table, embed_table, tokenizer_json: Path, special: dict, budget: int = 512) -> None:
        from tokenizers import Tokenizer

        self.be, self.pos, self.emb = backend, PosCache(pos_table), embed_table
        self.tok = Tokenizer.from_file(str(tokenizer_json))
        self.bos, self.eos, self.pad = special["bos"], special["eos"], special["pad"]
        self.budget = budget
        self.cos_t, self.sin_t = text_freqs(MAX_NEW + 1)

    def __call__(self, images: list[Image.Image], batch: int = BATCH) -> list[str]:
        out: list[str] = []
        for s in range(0, len(images), batch):
            out.extend(self._generate(images[s : s + batch]))
        return out

    def _generate(self, images):
        pv, pmask, shapes = preprocess(images, self.budget)
        posb, idx, tok_valid, valid, new_shapes, M = vision_inputs(pv, pmask, shapes, self.pos)
        b = pv.shape[0]
        vis = self.be.vision(pv, pmask, posb, idx, tok_valid)
        x = np.concatenate([vis, np.broadcast_to(self.emb[self.bos], (b, 1, vis.shape[-1]))], 1).astype(np.float32)
        key_bias = np.where(np.arange(M)[None] >= valid[:, None], NEG, 0.0).astype(np.float32)
        L0 = M + 1
        mask = np.zeros((b, 1, L0, L0), np.float32)
        mask[:, :, :M, M] = NEG
        mask[:, :, :, :M] += key_bias[:, None, None, :]
        cos = np.ones((b, L0, D_AXIS), np.float32)
        sin = np.zeros((b, L0, D_AXIS), np.float32)
        for i, (ho, wo) in enumerate(new_shapes):
            c, s_ = vis_freqs(ho, wo)
            n = min(ho * wo, M)
            cos[i, :n], sin[i, :n] = c[:n], s_[:n]
        cos[:, M], sin[:, M] = self.cos_t[0], self.sin_t[0]
        logits, past = self.be.decode(x, mask, cos, sin, None)
        nxt = logits.argmax(-1)
        toks = np.full((b, MAX_NEW + 1), self.pad, np.int64)
        toks[:, 0], toks[:, 1] = self.bos, nxt
        live = (nxt != self.eos) & (nxt != self.pad)
        seqlen = L0
        for step in range(1, MAX_NEW):
            if not live.any():
                break
            xs = self.emb[nxt][:, None, :].astype(np.float32)
            m = np.zeros((b, 1, 1, seqlen + 1), np.float32)
            m[:, 0, 0, :M] = key_bias
            cs = np.broadcast_to(self.cos_t[step], (b, 1, D_AXIS)).astype(np.float32)
            sn = np.broadcast_to(self.sin_t[step], (b, 1, D_AXIS)).astype(np.float32)
            logits, past = self.be.decode(xs, m, cs, sn, past)
            seqlen += 1
            nxt = np.where(live, logits.argmax(-1), self.pad)
            toks[:, step + 1] = nxt
            live = live & (nxt != self.eos) & (nxt != self.pad)
        return [
            self.tok.decode([t for t in seq[1:].tolist() if t not in (self.eos, self.pad)], skip_special_tokens=True).strip()
            for seq in toks
        ]


def load_host(out: Path, precision: str, threads: int = 0, audit: dict | None = None, budget: int = 512) -> NovaHost:
    from . import export as E

    be = OrtBackend(out / E.name("vision", precision), out / E.name("decoder", precision), threads, audit)
    cfg = json.loads((out / E.CONFIG).read_text(encoding="utf-8"))
    return NovaHost(be, np.load(out / E.POS_TABLE), np.load(out / E.TOKEN_EMBEDDINGS), out / E.TOKENIZER, cfg["special"], budget)
