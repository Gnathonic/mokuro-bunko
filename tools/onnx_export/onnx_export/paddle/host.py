"""Torch-free host side of paddle-manga over the exported graphs (the reference for the Rust host).

Spec: docs/rust-port/spec/ocr-recognizers.md §6.3-§6.11 (PIL bicubic
preprocessing, aux tables, prompt assembly, M-RoPE, left-padded batches,
greedy loop with per-row token caps, ``tokenizers`` decode).
"""

from __future__ import annotations

import json
import math
from pathlib import Path

import numpy as np
from PIL import Image

PATCH, MERGE, MIN_PIX, MAX_PIX = 14, 2, 112896, 1003520
V_HD, SIDE = 72, 27
T_HD, ROPE_THETA, MROPE = 128, 500000.0, (16, 24, 24)
N_LAYERS, N_KV = 18, 2
EOS, PAD, IMAGE_TOKEN = 2, 0, 100295
BATCH = 12  # PADDLE_BATCH
DEFAULT_MAX_NEW_TOKENS = 64


def smart_resize(h: int, w: int, factor: int = 28, min_pixels: int = MIN_PIX, max_pixels: int = MAX_PIX) -> tuple[int, int]:
    if h < factor:
        w = round((w * factor) / h)
        h = factor
    if w < factor:
        h = round((h * factor) / w)
        w = factor
    if max(h, w) / min(h, w) > 200:
        raise ValueError("absolute aspect ratio must be smaller than 200")
    hb, wb = round(h / factor) * factor, round(w / factor) * factor
    if hb * wb > max_pixels:
        beta = math.sqrt((h * w) / max_pixels)
        hb = max(factor, math.floor(h / beta / factor) * factor)
        wb = max(factor, math.floor(w / beta / factor) * factor)
    elif hb * wb < min_pixels:
        beta = math.sqrt(min_pixels / (h * w))
        hb = math.ceil(h * beta / factor) * factor
        wb = math.ceil(w * beta / factor) * factor
    return hb, wb


def preprocess(img: Image.Image):
    """(N,3,14,14) f32 patches in raster order, and the (gh, gw) grid."""
    w, h = img.size
    rh, rw = smart_resize(h, w)
    a = np.asarray(img.convert("RGB").resize((rw, rh), Image.BICUBIC), dtype=np.float32)
    a = (a / 255.0 - 0.5) / 0.5
    gh, gw = rh // PATCH, rw // PATCH
    a = a.transpose(2, 0, 1).reshape(3, gh, PATCH, gw, PATCH).transpose(1, 3, 0, 2, 4).reshape(gh * gw, 3, PATCH, PATCH)
    return np.ascontiguousarray(a), (gh, gw)


def _axis_taps(pos: np.ndarray, n: int):  # bilinear, align_corners=True
    src = pos * (SIDE - 1) / (n - 1) if n > 1 else np.zeros_like(pos, dtype=np.float64)
    i0 = np.clip(np.floor(src).astype(np.int64), 0, SIDE - 1)
    i1 = np.clip(i0 + 1, 0, SIDE - 1)
    f = src - np.floor(src)
    return i0, i1, 1.0 - f, f


def vision_aux(gh: int, gw: int):
    row = np.repeat(np.arange(gh), gw)
    col = np.tile(np.arange(gw), gh)
    r0, r1, rw0, rw1 = _axis_taps(row.astype(np.float64), gh)
    c0, c1, cw0, cw1 = _axis_taps(col.astype(np.float64), gw)
    idx = np.stack([r0 * SIDE + c0, r0 * SIDE + c1, r1 * SIDE + c0, r1 * SIDE + c1], 1)
    wts = np.stack([rw0 * cw0, rw0 * cw1, rw1 * cw0, rw1 * cw1], 1).astype(np.float32)
    inv = 1.0 / (10000.0 ** (np.arange(0, V_HD // 2, 2, dtype=np.float32) / (V_HD // 2)))
    fh = row[:, None].astype(np.float32) * inv
    fw = col[:, None].astype(np.float32) * inv
    f = np.concatenate([fh, fw], -1)
    f = np.concatenate([f, f], -1)
    hb, wb = gh // MERGE, gw // MERGE
    bi, bj = np.repeat(np.arange(hb), wb), np.tile(np.arange(wb), hb)
    merge = np.stack([(2 * bi) * gw + 2 * bj, (2 * bi) * gw + 2 * bj + 1, (2 * bi + 1) * gw + 2 * bj, (2 * bi + 1) * gw + 2 * bj + 1], 1).reshape(-1)
    return idx.astype(np.int64), wts, np.cos(f).astype(np.float32), np.sin(f).astype(np.float32), merge.astype(np.int64)


def mrope_cos_sin(pos3: np.ndarray):
    """pos3 (3,B,S) int -> cos, sin (B,S,128) f32."""
    inv = (1.0 / (ROPE_THETA ** (np.arange(0, T_HD, 2, dtype=np.float32) / T_HD))).astype(np.float32)
    fr = pos3[..., None].astype(np.float32) * inv
    parts, o = [], 0
    for i, s in enumerate(MROPE):
        parts.append(fr[i % 3, ..., o : o + s])
        o += s
    f = np.concatenate(parts, -1)
    f = np.concatenate([f, f], -1)
    return np.cos(f), np.sin(f)


def plan_batches(areas, caps, size: int = BATCH) -> list[list[int]]:
    order = sorted(range(len(areas)), key=lambda i: (caps[i], areas[i], i))
    return [order[k : k + size] for k in range(0, len(order), size)]


class OrtBackend:
    def __init__(self, vision_path: Path, decoder_path: Path, threads: int = 0) -> None:
        from ..common import ort_session

        self.vs = ort_session(vision_path, threads)
        self.ds = ort_session(decoder_path, threads)
        self.vdt = np.float16 if "float16" in self.vs.get_inputs()[0].type else np.float32
        self.dt = np.float16 if "float16" in self.ds.get_inputs()[0].type else np.float32
        self.din = [i.name for i in self.ds.get_inputs()]

    def vision(self, pv, idx, w, cos, sin, merge):
        feed = {"pixel_values": pv.astype(self.vdt), "pos_idx": idx, "pos_w": w, "cos": cos, "sin": sin, "merge_idx": merge}
        return self.vs.run(None, feed)[0].astype(np.float32)

    def decoder(self, x, cos, sin, bias, past):
        r = self.ds.run(None, dict(zip(self.din, [x, cos, sin, bias, *past], strict=True)))
        return r[0], r[1:]


class PaddleHost:
    def __init__(self, backend: OrtBackend, embed: np.ndarray, tokenizer_json: Path, cfg: dict) -> None:
        from tokenizers import Tokenizer

        self.be, self.embed = backend, embed
        self.prefix, self.suffix = list(cfg["prompt"]["prefix"]), list(cfg["prompt"]["suffix"])
        self.dtype = backend.dt
        self.neg = float(np.finfo(self.dtype).min)
        self.tok = Tokenizer.from_file(str(tokenizer_json))

    def __call__(self, crops: list[Image.Image], max_tokens: list[int] | None = None) -> list[str]:
        caps = [int(c) for c in max_tokens] if max_tokens is not None else [DEFAULT_MAX_NEW_TOKENS] * len(crops)
        areas = [float(c.size[0] * c.size[1]) for c in crops]
        texts = [""] * len(crops)
        for batch in plan_batches(areas, caps):
            toks = self.generate([preprocess(crops[i]) for i in batch], max(caps[i] for i in batch))
            for i, t in zip(batch, toks, strict=True):
                texts[i] = self.tok.decode(t[: caps[i]], skip_special_tokens=True).strip()
        return texts

    def generate(self, pixel_list, max_new_tokens: int) -> list[list[int]]:
        B = len(pixel_list)
        seqs, embs, pos_rows, bases = [], [], [], []
        for pv, (gh, gw) in pixel_list:
            img = self.be.vision(pv, *vision_aux(gh, gw))
            ids = self.prefix + [IMAGE_TOKEN] * img.shape[0] + self.suffix
            e = self.embed[np.asarray(ids)].astype(np.float32)
            e[len(self.prefix) : len(self.prefix) + img.shape[0]] = img
            hb, wb = gh // MERGE, gw // MERGE
            p0 = len(self.prefix)
            tp = np.arange(p0)
            vis = np.stack([np.full(hb * wb, p0), p0 + np.repeat(np.arange(hb), wb), p0 + np.tile(np.arange(wb), hb)])
            sp = p0 + max(hb, wb) + np.arange(len(self.suffix))
            pos = np.concatenate([np.stack([tp] * 3), vis, np.stack([sp] * 3)], 1)
            seqs.append(ids)
            embs.append(e)
            pos_rows.append(pos)
            bases.append(int(pos.max()) + 1)
        S = max(len(s) for s in seqs)
        x = np.zeros((B, S, embs[0].shape[1]), np.float32)
        pos3 = np.zeros((3, B, S), np.int64)
        valid = np.zeros((B, S), bool)
        for b in range(B):
            n = len(seqs[b])
            x[b, S - n :] = embs[b]
            pos3[:, b, S - n :] = pos_rows[b]
            valid[b, S - n :] = True
            x[b, : S - n] = self.embed[PAD]
        cos, sin = mrope_cos_sin(pos3)
        allow = np.tril(np.ones((S, S), bool))[None] & valid[:, None, :]
        allow |= np.eye(S, dtype=bool)[None]
        dt = self.dtype
        bias = np.where(allow, 0.0, self.neg).astype(dt)[:, None]
        past = [np.zeros((B, N_KV, 0, T_HD), dt) for _ in range(2 * N_LAYERS)]
        logits, past = self.be.decoder(x.astype(dt), cos.astype(dt), sin.astype(dt), bias, past)
        out: list[list[int]] = [[] for _ in range(B)]
        done = np.zeros(B, bool)
        keyvalid = valid.copy()
        base = np.array(bases)
        for step in range(max_new_tokens):
            tok = logits.argmax(-1)
            for b in range(B):
                if not done[b]:
                    if tok[b] == EOS:
                        done[b] = True
                    else:
                        out[b].append(int(tok[b]))
            if done.all() or step == max_new_tokens - 1:
                break
            tok = np.where(done, PAD, tok)
            xs = self.embed[tok][:, None, :].astype(dt)
            p = (base + step)[None, :, None].repeat(3, 0)
            cos, sin = mrope_cos_sin(p)
            keyvalid = np.concatenate([keyvalid, np.ones((B, 1), bool)], 1)
            bias = np.where(keyvalid, 0.0, self.neg).astype(dt)[:, None, None, :]
            logits, past = self.be.decoder(xs, cos.astype(dt), sin.astype(dt), bias, past)
        return out


def load_host(out: Path, precision: str, threads: int = 0, embed_precision: str | None = None) -> PaddleHost:
    from . import export as E

    be = OrtBackend(out / E.name("vision", precision), out / E.name("decoder", precision), threads)
    cfg = json.loads((out / E.CONFIG).read_text(encoding="utf-8"))
    embed = np.load(out / E.embed_name(embed_precision or precision))
    return PaddleHost(be, embed, out / E.TOKENIZER, cfg)
