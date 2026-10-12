"""Python reference hosts for the WebGPU graphs (the Rust host's twin; used by `verify`).

Preprocessing, prompts and positions are `onnx_export`'s (0.5.2 math); only the decode
plumbing differs: one shared decoder graph, ctx/gen KV regions on the device, a masked
one-key dummy where a region would be empty (see `graph.py`).
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "onnx_export"))
os.environ.setdefault("MOKURO_REF_052", str(Path(__file__).resolve().parents[3]))

from onnx_export.hayai import host as HH  # noqa: E402
from onnx_export.paddle import host as PH  # noqa: E402


def session(path, ep="webgpu", threads=0):
    import onnxruntime as ort

    so = ort.SessionOptions()
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    if threads:
        so.intra_op_num_threads = threads
    provs = [("WebGpuExecutionProvider", {}), "CPUExecutionProvider"] if ep == "webgpu" else ["CPUExecutionProvider"]
    return ort.InferenceSession(str(path), so, providers=provs)


class Decode:
    """Runs the shared decoder graph; ctx/gen values stay wherever the EP keeps them."""

    def __init__(self, sess, ep, layers):
        self.s, self.dev, self.layers = sess, ("webgpu" if ep == "webgpu" else "cpu"), layers
        self.outs = [o.name for o in sess.get_outputs()]

    def run(self, x, cos, sin, ctx, ctx_bias, gen, gen_bias):
        b, s = x.shape[:2]
        io = self.s.io_binding()
        for k, v in (("x", x), ("cos", cos), ("sin", sin), ("ctx_bias", ctx_bias), ("gen_bias", gen_bias),
                     ("pos", np.arange(b * s, dtype=np.int64).reshape(b, s))):
            io.bind_cpu_input(k, np.ascontiguousarray(v))
        for kv in (ctx, gen):
            for k, v in kv.items():
                (io.bind_cpu_input(k, v) if isinstance(v, np.ndarray) else io.bind_ortvalue_input(k, v))
        io.bind_output("next", "cpu")
        for n in self.outs[1:]:
            io.bind_output(n, self.dev, 0)
        self.s.run_with_iobinding(io)
        o = io.get_outputs()
        return o[0].numpy().astype(np.int64), {n[:-2]: v for n, v in zip(self.outs[1:], o[1:])}

    def dummy(self, prefix, b, kv, hd):
        z_t = np.zeros((b, kv, hd, 1), np.float32)
        z_v = np.zeros((b, kv, 1, hd), np.float32)
        return {k: v for i in range(self.layers) for k, v in ((f"{prefix}_kT{i}", z_t), (f"{prefix}_v{i}", z_v))}


def _ctx_from(gen):
    return {k.replace("gen_", "ctx_"): v for k, v in gen.items()}


class Hayai:
    def __init__(self, models: Path, graphs: Path, ep="webgpu", rows=16, threads=0):
        from tokenizers import Tokenizer

        cfg = json.loads((models / "hayai-nova_config.json").read_text())
        sp = cfg["special"]
        self.bos, self.eos, self.pad = sp["bos"], sp["eos"], sp["pad"]
        self.tok = Tokenizer.from_file(str(models / "hayai-nova_tokenizer.json"))
        self.pos = HH.PosCache(np.load(models / "hayai-nova_pos_table.npy"))
        self.emb = np.load(models / "hayai-nova_token_embeddings.npy")
        self.cos_t, self.sin_t = HH.text_freqs(HH.MAX_NEW + 1)
        self.vis = session(graphs / "hayai-nova_webgpu_vision_fp32.onnx", ep, threads)
        self.dec = Decode(session(graphs / "hayai-nova_webgpu_decoder_fp32.onnx", ep, threads), ep, 12)
        self.vin = [i.name for i in self.vis.get_inputs()]
        self.rows = rows

    def __call__(self, images, batch=HH.BATCH):
        out = []
        for s in range(0, len(images), batch):
            out.extend(self._gen(images[s : s + batch]))
        return out

    def _gen(self, images):
        NEG = HH.NEG
        pv, pmask, shapes = HH.preprocess(images, 512)
        posb, idx, tok_valid, valid, new_shapes, M = HH.vision_inputs(pv, pmask, shapes, self.pos)
        b = pv.shape[0]
        parts = []
        for r in range(0, b, self.rows):
            feed = dict(zip(self.vin, [pv[r : r + self.rows], pmask[r : r + self.rows], posb[r : r + self.rows],
                                       idx[r : r + self.rows], tok_valid[r : r + self.rows]]))
            parts.append(self.vis.run(None, feed)[0].astype(np.float32))
        vis = np.concatenate(parts, 0)
        L0 = M + 1
        x = np.concatenate([vis, np.broadcast_to(self.emb[self.bos], (b, 1, vis.shape[-1]))], 1)
        key_bias = np.where(np.arange(M)[None] >= valid[:, None], NEG, 0.0).astype(np.float32)
        mask = np.zeros((b, 1, L0, L0), np.float32)
        mask[:, :, :M, M] = NEG
        mask[:, :, :, :M] += key_bias[:, None, None, :]
        cos = np.ones((b, L0, HH.D_AXIS), np.float32)
        sin = np.zeros((b, L0, HH.D_AXIS), np.float32)
        for i, (ho, wo) in enumerate(new_shapes):
            c, s_ = HH.vis_freqs(ho, wo)
            n = min(ho * wo, M)
            cos[i, :n], sin[i, :n] = c[:n], s_[:n]
        cos[:, M], sin[:, M] = self.cos_t[0], self.sin_t[0]
        negcol = lambda s: np.full((b, 1, s, 1), NEG, np.float32)
        nxt, gen = self.dec.run(x, cos, sin, self.dec.dummy("ctx", b, 2, 64), negcol(L0), self.dec.dummy("gen", b, 2, 64),
                                np.concatenate([negcol(L0), mask], -1))
        ctx = _ctx_from(gen)
        ctx_bias = np.concatenate([np.full((b, 1), NEG, np.float32), key_bias, np.zeros((b, 1), np.float32)], 1)[:, None, None, :]
        gen = self.dec.dummy("gen", b, 2, 64)
        toks = np.full((b, HH.MAX_NEW + 1), self.pad, np.int64)
        toks[:, 0], toks[:, 1] = self.bos, nxt
        live = (nxt != self.eos) & (nxt != self.pad)
        t = 1
        for step in range(1, HH.MAX_NEW):
            if not live.any():
                break
            gb = np.zeros((b, 1, 1, t + 1), np.float32)
            gb[..., 0] = NEG
            got, gen = self.dec.run(self.emb[nxt][:, None, :], np.broadcast_to(self.cos_t[step], (b, 1, HH.D_AXIS)),
                                    np.broadcast_to(self.sin_t[step], (b, 1, HH.D_AXIS)), ctx, ctx_bias, gen, gb)
            t += 1
            nxt = np.where(live, got, self.pad)
            toks[:, step + 1] = nxt
            live = live & (nxt != self.eos) & (nxt != self.pad)
        return [self.tok.decode([t for t in seq[1:].tolist() if t not in (self.eos, self.pad)], skip_special_tokens=True).strip()
                for seq in toks]


# The EP frees a run's buffers only when the run ends, so a prefill call holds every
# layer's attention scores: rows x heads x tokens x keys, ~6 tensors of it per layer.
ATTN_ELEMS = 2_300_000  # rows*heads*tokens*keys per prefill call (~1 GB over 18 layers)
BATCH_COST = 12 * 256 * 256  # rows x prompt^2 per decode batch (big crops get smaller batches)


def prompt_len(h: int, w: int) -> int:
    """paddle-manga's prompt tokens for a crop of h x w pixels (prefix + image + suffix)."""
    rh, rw = PH.smart_resize(h, w)
    return 5 + (rh // 28) * (rw // 28) + 8


def prefill_chunk(rows: int, keys: int, heads: int = 16) -> int:
    return max(16, ATTN_ELEMS // max(1, rows * heads * keys))


class LayeredVision:
    """paddle-manga's vision tower run layer by layer (`paddle.vision_graphs`)."""

    def __init__(self, graphs: Path, ep="webgpu", threads=0):
        import onnxruntime as ort

        self.dev = "webgpu" if ep == "webgpu" else "cpu"
        S = lambda n: session(graphs / f"paddle-manga_webgpu_{n}_fp32.onnx", ep, threads)
        self.s = {n: S(n) for n in ("embed", "layer", "qkv", "attn", "post", "tail")}
        meta = json.loads((graphs / "paddle-manga_webgpu_vision.json").read_text())
        self.layers, self.whole_max, self.chunk = meta["layers"], meta["whole_max"], meta["chunk"]
        ws = S("weights")
        io = ws.io_binding()
        names = [o.name for o in ws.get_outputs()]
        for n in names:
            io.bind_output(n, self.dev, 0)
        ws.run_with_iobinding(io)
        self.w = dict(zip(names, io.get_outputs()))
        del ws
        self.ins = {k: [i.name for i in v.get_inputs()] for k, v in self.s.items()}
        self.ort = ort

    def _run(self, name, feed, out_dev=True):
        sess = self.s[name]
        io = sess.io_binding()
        for k in self.ins[name]:
            v = feed[k]
            (io.bind_cpu_input(k, np.ascontiguousarray(v)) if isinstance(v, np.ndarray) else io.bind_ortvalue_input(k, v))
        outs = [o.name for o in sess.get_outputs()]
        for o in outs:
            io.bind_output(o, self.dev if out_dev else "cpu", 0)
        sess.run_with_iobinding(io)
        return dict(zip(outs, io.get_outputs()))

    def __call__(self, pv, idx, w, cos, sin, merge):
        n = pv.shape[0]
        x = self._run("embed", {"pixel_values": pv, "pos_idx": idx, "pos_w": w, **self.w})["x"]
        for i in range(self.layers):
            lw = {k.split(".", 1)[1]: v for k, v in self.w.items() if k.startswith(f"L{i}.")}
            if n <= self.whole_max:
                x = self._run("layer", {"x": x, "cos": cos, "sin": sin, **lw})["x_out"]
                continue
            qkv = self._run("qkv", {"x": x, "cos": cos, "sin": sin, **lw})
            o = np.concatenate([
                self._run("attn", {"q": qkv["q"], "kT": qkv["kT"], "v": qkv["v"], "start": np.array([a], np.int64),
                                   "end": np.array([min(a + self.chunk, n)], np.int64)}, out_dev=False)["o"].numpy()
                for a in range(0, n, self.chunk)])
            x = self._run("post", {"x": x, "o": o, **lw})["x_out"]
        return self._run("tail", {"x": x, "merge_idx": merge, **self.w}, out_dev=False)["image_embeds"].numpy()


class Paddle:
    def __init__(self, models: Path, graphs: Path, ep="webgpu", threads=0, vision=None):
        from tokenizers import Tokenizer

        cfg = json.loads((models / "paddle-manga_config.json").read_text())
        self.prefix, self.suffix = list(cfg["prompt"]["prefix"]), list(cfg["prompt"]["suffix"])
        self.tok = Tokenizer.from_file(str(models / "paddle-manga_tokenizer.json"))
        self.embed = np.load(models / "paddle-manga_embed_fp32.npy")
        if (graphs / "paddle-manga_webgpu_vision.json").exists() and vision is None:
            self.layered = LayeredVision(graphs, ep, threads)
        else:
            self.layered = None
            self.vis = session(vision or models / "paddle-manga_vision_fp32.onnx", ep, threads)
        self.dec = Decode(session(graphs / "paddle-manga_webgpu_decoder_fp32.onnx", ep, threads), ep, PH.N_LAYERS)
        self.neg = float(np.finfo(np.float32).min)

    def __call__(self, crops, max_tokens=None):
        caps = [int(c) for c in max_tokens] if max_tokens is not None else [PH.DEFAULT_MAX_NEW_TOKENS] * len(crops)
        areas = [float(c.size[0] * c.size[1]) for c in crops]
        texts = [""] * len(crops)
        for batch in PH.plan_batches(areas, caps):
            # rows are independent: a batch of big crops is split so rows x prompt^2 stays bounded
            sub, cost = [], 0
            groups = []
            for i in batch:
                s2 = prompt_len(crops[i].size[1], crops[i].size[0]) ** 2
                if sub and (len(sub) + 1) * max(cost, s2) > BATCH_COST:
                    groups.append(sub)
                    sub, cost = [], 0
                sub.append(i)
                cost = max(cost, s2)
            groups.append(sub)
            for grp in groups:
                toks = self.generate([PH.preprocess(crops[i]) for i in grp], [caps[i] for i in grp])
                for i, t in zip(grp, toks, strict=True):
                    texts[i] = self.tok.decode(t[: caps[i]], skip_special_tokens=True).strip()
        return texts

    def generate(self, pixel_list, caps):
        B = len(pixel_list)
        seqs, embs, pos_rows, bases = [], [], [], []
        for pv, (gh, gw) in pixel_list:
            idx, w, c, s_, merge = PH.vision_aux(gh, gw)
            if self.layered is not None:
                img = self.layered(pv, idx, w, c, s_, merge)
            else:
                img = self.vis.run(None, {"pixel_values": pv, "pos_idx": idx, "pos_w": w, "cos": c, "sin": s_, "merge_idx": merge})[0]
            ids = self.prefix + [PH.IMAGE_TOKEN] * img.shape[0] + self.suffix
            e = self.embed[np.asarray(ids)].astype(np.float32)
            e[len(self.prefix) : len(self.prefix) + img.shape[0]] = img
            hb, wb = gh // PH.MERGE, gw // PH.MERGE
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
            x[b, : S - n] = self.embed[PH.PAD]
        cos, sin = PH.mrope_cos_sin(pos3)
        allow = np.tril(np.ones((S, S), bool))[None] & valid[:, None, :]
        allow |= np.eye(S, dtype=bool)[None]
        bias = np.where(allow, 0.0, self.neg).astype(np.float32)[:, None]
        negcol = lambda s: np.full((B, 1, s, 1), self.neg, np.float32)
        dctx = self.dec.dummy("ctx", B, 2, 128)
        gen = self.dec.dummy("gen", B, 2, 128)
        step_ = prefill_chunk(B, S + 1)
        for a in range(0, S, step_):  # the prompt in chunks: same keys and masks per query
            b_ = min(a + step_, S)
            tok, gen = self.dec.run(x[:, a:b_], cos[:, a:b_, :64], sin[:, a:b_, :64], dctx, negcol(b_ - a), gen,
                                    np.concatenate([negcol(b_ - a), bias[:, :, a:b_, :b_]], -1))
        ctx = _ctx_from(gen)
        ctx_bias = np.concatenate([np.full((B, 1), self.neg, np.float32), np.where(valid, 0.0, self.neg).astype(np.float32)], 1)[:, None, None, :]
        gen = self.dec.dummy("gen", B, 2, 128)
        out = [[] for _ in range(B)]
        done = np.array([c == 0 for c in caps])
        base = np.array(bases)
        max_new = max(caps)
        t = 1
        for step in range(max_new):
            for b in range(B):
                if not done[b]:
                    if tok[b] == PH.EOS:
                        done[b] = True
                    else:
                        out[b].append(int(tok[b]))
                        if len(out[b]) >= caps[b]:
                            done[b] = True
            if done.all() or step == max_new - 1:
                break
            t_in = np.where(done, PH.PAD, tok)
            p = (base + step)[None, :, None].repeat(3, 0)
            c, s_ = PH.mrope_cos_sin(p)
            gb = np.zeros((B, 1, 1, t + 1), np.float32)
            gb[..., 0] = self.neg
            tok, gen = self.dec.run(self.embed[t_in][:, None, :].astype(np.float32), c[..., :64], s_[..., :64], ctx, ctx_bias, gen, gb)
            t += 1
        return out
