"""Host-side inputs of the ncnn hayai graphs, per crop (numpy only).

Preprocessing is ts_check.py's / bunko-vlm's (NaFlex resize, patchify, position-table
resize, 2x2 projector gather, RoPE tables, masks) without the batch padding: the vision
graph runs one crop at its own patch count.
"""

from __future__ import annotations

import math

import numpy as np
from PIL import Image

PATCH, D_AXIS, NEG, MAX_NEW, BUDGET = 16, 32, -1e9, 96, 512
BOS, EOS, PAD = 16001, 16002, 16000


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


def aa_weights(inp: int, out: int) -> np.ndarray:
    scale = inp / out
    fs = max(scale, 1.0)
    w = np.zeros((out, inp), np.float64)
    for i in range(out):
        center = scale * (i + 0.5)
        xmin, xmax = max(int(center - fs + 0.5), 0), min(int(center + fs + 0.5), inp)
        ws = np.array([max(0.0, 1.0 - abs((j - center + 0.5) / fs)) for j in range(xmin, xmax)])
        if ws.sum() > 0:
            ws /= ws.sum()
        w[i, xmin:xmax] = ws
    return w.astype(np.float32)


class PosCache:
    def __init__(self, table: np.ndarray) -> None:
        n = math.isqrt(table.shape[0])
        self.grid = table.reshape(n, n, -1).astype(np.float32)
        self.cache: dict = {}

    def get(self, h: int, w: int) -> np.ndarray:
        if (h, w) not in self.cache:
            wy, wx = aa_weights(self.grid.shape[0], h), aa_weights(self.grid.shape[1], w)
            self.cache[(h, w)] = np.einsum("hy,yxc,wx->hwc", wy, self.grid, wx).reshape(h * w, -1)
        return self.cache[(h, w)]


def freqs() -> np.ndarray:
    return 1.0 / (10000.0 ** (np.arange(0, D_AXIS, 2, dtype=np.float32) / D_AXIS))


def text_freqs(n: int):
    """(n, 32) cos/sin of text positions 0..n-1 in the interleaved-pair layout's half form."""
    a = np.outer(np.arange(n, dtype=np.float32), freqs())
    a = np.concatenate([a, a], -1)
    return np.cos(a), np.sin(a)


def crop_inputs(im: Image.Image, pos: PosCache) -> dict:
    """One crop's vision inputs: pv (P,768), pos (P,768), sel (4M,P); grid (hp,wp,ho,wo)."""
    th, tw = size_for_budget(im.height, im.width, BUDGET)
    arr = (np.asarray(im.resize((tw, th), Image.BILINEAR), np.float32) * (1 / 255) - 0.5) / 0.5
    hp, wp = th // PATCH, tw // PATCH
    pv = arr.reshape(hp, PATCH, wp, PATCH, 3).transpose(0, 2, 1, 3, 4).reshape(hp * wp, -1)
    ho, wo = -(-hp // 2), -(-wp // 2)
    m = ho * wo
    sel = np.zeros((4 * m, hp * wp), np.float32)
    y, x = np.divmod(np.arange(m), wo)
    for k, (dy, dx) in enumerate(((0, 0), (0, 1), (1, 0), (1, 1))):
        idx = np.minimum(2 * y + dy, hp - 1) * wp + np.minimum(2 * x + dx, wp - 1)
        sel[np.arange(m) * 4 + k, idx] = 1.0
    return {"pv": np.ascontiguousarray(pv), "pos": np.ascontiguousarray(pos.get(hp, wp)), "sel": sel,
            "grid": (hp, wp, ho, wo)}


def prefill_inputs(grids: list[tuple[int, int, int, int]]):
    """Per batch: m (padded vision tokens), and per crop its mask (L0,L0), cos/sin (L0,32).

    Same values as the batched torch prefill (ts_check.host_inputs) row by row."""
    m = max(ho * wo for *_, ho, wo in grids)
    l0 = m + 1
    f = freqs()
    out = []
    for *_, ho, wo in grids:
        valid = ho * wo
        mask = np.zeros((l0, l0), np.float32)
        mask[:m, m] = NEG
        mask[:, valid:m] += NEG
        cos, sin = np.ones((l0, D_AXIS), np.float32), np.zeros((l0, D_AXIS), np.float32)
        yy, xx = np.divmod(np.arange(valid), wo)
        ay, ax = np.outer(yy.astype(np.float32), f), np.outer(xx.astype(np.float32), f)
        a = np.concatenate([ay, ax], -1)
        cos[:valid], sin[:valid] = np.cos(a), np.sin(a)
        out.append((mask, cos, sin))
    return m, out
