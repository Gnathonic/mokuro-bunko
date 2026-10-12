"""A minimal ncnn graph writer: layers in order, blobs by name, weights in a ``.bin``.

ncnn needs an explicit ``Split`` for a blob read by several layers; :meth:`Graph.save`
inserts them. Weights follow ncnn's ModelBin rules: ``tagged`` arrays (InnerProduct /
Gemm weights) start with a 4-byte zero flag (= raw fp32), ``raw`` ones (biases, norm
affines, MemoryData) do not.
"""

from __future__ import annotations

import zlib
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np


@dataclass
class Layer:
    type: str
    name: str
    inputs: list[str]
    outputs: list[str]
    params: dict = field(default_factory=dict)
    weights: list[tuple[np.ndarray, bool]] = field(default_factory=list)  # (array, tagged)


def _param(k: int, v) -> str:
    if isinstance(v, (list, tuple)):
        vals = ",".join(_scalar(x) for x in v)
        return f"{-23300 - k}={len(v)},{vals}"
    if isinstance(v, str):
        return f'{k}="{v}"'
    return f"{k}={_scalar(v)}"


def _scalar(x) -> str:
    if isinstance(x, float):
        s = f"{x:.9e}"
        return s
    return str(int(x))


class Graph:
    def __init__(self, linear: str = "gemm") -> None:
        self.layers: list[Layer] = []
        self._n = 0
        # "bngemm" (bunko-ncnn's custom layer), "gemm" (ncnn Gemm, constant B) or "innerproduct"
        self.linear_kind = linear

    def _name(self, base: str) -> str:
        self._n += 1
        return f"{base}_{self._n}"

    def add(self, type_: str, inputs, params=None, weights=None, outputs=None, n_out=1, name=None) -> list[str]:
        inputs = [inputs] if isinstance(inputs, str) else list(inputs)
        name = name or self._name(type_.lower())
        if outputs is None:
            outputs = [name] if n_out == 1 else [f"{name}.{i}" for i in range(n_out)]
        self.layers.append(Layer(type_, name, inputs, list(outputs), params or {}, weights or []))
        return list(outputs)

    def one(self, *a, **k) -> str:
        return self.add(*a, **k)[0]

    # ---- helpers -------------------------------------------------------------------
    def input(self, name: str) -> str:
        return self.one("Input", [], outputs=[name], name=name)

    def linear(self, x: str, w: np.ndarray, b: np.ndarray | None = None, name=None, outputs=None) -> str:
        """y = x @ w.T + b for a 2-D x (rows, in); w is torch's (out, in)."""
        w = np.ascontiguousarray(w, np.float32)
        if self.linear_kind == "bngemm":  # bunko-ncnn's own tiled GEMM layer (shim)
            n, k = w.shape
            ws = [(w, False)]
            if b is not None:
                ws.append((np.ascontiguousarray(b, np.float32), False))
            # weight id: equal weights in two graphs (prefill and step) share one GPU copy
            wid = zlib.crc32(w.tobytes(), zlib.crc32(ws[-1][0].tobytes()) if b is not None else 0)
            wid = wid - (1 << 32) if wid >= 1 << 31 else wid
            return self.one("BnGemm", x, {0: n, 1: k, 2: int(b is not None), 3: wid}, ws, name=name, outputs=outputs)
        if self.linear_kind == "gemm":
            n, k = w.shape
            p = {0: 1.0, 1: 1.0, 2: 0, 3: 1, 4: 0, 5: 1, 6: int(b is not None), 7: 0, 8: n, 9: k}
            ws = [(w, True)]
            if b is not None:
                p[10] = 4  # C broadcast over rows: one bias per output column
                ws.append((np.ascontiguousarray(b, np.float32), True))
            return self.one("Gemm", x, p, ws, name=name, outputs=outputs)
        ws = [(w, True)]
        if b is not None:
            ws.append((np.ascontiguousarray(b, np.float32), False))
        return self.one("InnerProduct", x, {0: w.shape[0], 1: int(b is not None), 2: w.size}, ws, name=name,
                        outputs=outputs)

    def layernorm(self, x: str, g: np.ndarray, b: np.ndarray, eps: float) -> str:
        return self.one("LayerNorm", x, {0: g.size, 1: float(eps), 2: 1},
                        [(np.asarray(g, np.float32), False), (np.asarray(b, np.float32), False)])

    def rmsnorm(self, x: str, g: np.ndarray, eps: float) -> str:
        return self.one("RMSNorm", x, {0: g.size, 1: float(eps), 2: 1}, [(np.asarray(g, np.float32), False)])

    def reshape(self, x: str, w=-233, h=-233, c=-233, d=-233) -> str:
        p = {0: w, 1: h, 2: c}
        if d != -233:
            p[11] = d
        return self.one("Reshape", x, p)

    def permute(self, x: str, order: int) -> str:
        return self.one("Permute", x, {0: order})

    def binary(self, a: str, b: str, op: int) -> str:
        return self.one("BinaryOp", [a, b], {0: op})

    def attn(self, q: str, k: str, v: str, scale: float, mask: str | None = None, mask_div: int = 1) -> str:
        """bunko-ncnn's BnAttn: softmax(scale * q k^T [+ mask]) v per head, query rows in
        chunks (bounded memory), GQA by channel (kv channel = q channel / (q.c / k.c)),
        mask channel = q channel / mask_div."""
        ins = [q, k, v] + ([mask] if mask else [])
        return self.one("BnAttn", ins, {0: float(scale), 1: int(mask is not None), 2: mask_div})

    def save_bins_shared(self, other: "Graph", param_a: Path, param_b: Path, binf: Path) -> None:
        """Save two graphs whose weights are the same arrays in the same order into one .bin."""
        import tempfile

        with tempfile.TemporaryDirectory(dir=binf.parent) as t:
            ba, bb = Path(t) / "a.bin", Path(t) / "b.bin"
            self.save(param_a, ba)
            other.save(param_b, bb)
            if ba.read_bytes() != bb.read_bytes():
                raise ValueError(f"{param_a.name} and {param_b.name} do not carry the same weights")
            ba.replace(binf)

    def slice(self, x: str, sizes: list[int], axis: int) -> list[str]:
        return self.add("Slice", x, {0: sizes, 1: axis}, n_out=len(sizes))

    # ---- output --------------------------------------------------------------------
    def save(self, param: Path, binf: Path) -> None:
        consumers: dict[str, int] = {}
        for L in self.layers:
            for i in L.inputs:
                consumers[i] = consumers.get(i, 0) + 1
        # rename each consumer's reference of a multiply-read blob to a split output
        out_layers: list[Layer] = []
        pending: dict[str, list[str]] = {}
        for L in self.layers:
            ins = []
            for i in L.inputs:
                if consumers.get(i, 0) > 1:
                    ins.append(pending[i].pop(0))
                else:
                    ins.append(i)
            out_layers.append(Layer(L.type, L.name, ins, L.outputs, L.params, L.weights))
            for o in L.outputs:
                n = consumers.get(o, 0)
                if n > 1:
                    outs = [f"{o}#{k}" for k in range(n)]
                    out_layers.append(Layer("Split", f"split_{o}", [o], outs))
                    pending[o] = list(outs)
        blobs = set()
        for L in out_layers:
            blobs.update(L.outputs)
        lines = ["7767517", f"{len(out_layers)} {len(blobs)}"]
        with open(binf, "wb") as fb:
            for L in out_layers:
                ps = " ".join(_param(k, v) for k, v in L.params.items())
                lines.append(f"{L.type:<16} {L.name:<24} {len(L.inputs)} {len(L.outputs)} "
                             f"{' '.join(L.inputs)} {' '.join(L.outputs)} {ps}".rstrip())
                for arr, tagged in L.weights:
                    if tagged:
                        fb.write(b"\0\0\0\0")
                    fb.write(np.ascontiguousarray(arr, np.float32).tobytes())
        param.write_text("\n".join(lines) + "\n")
