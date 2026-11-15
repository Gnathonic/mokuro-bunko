"""charspot-small's host decode, dumped crop by crop for Bunko's ignored
``charspot_decode_parity`` test (the Rust port in ``bunko_vlm::textbox`` against the
authors' ``core.py``, TEXTBOX-GPU.md §20.2).

Per crop (the first N of a folder): the authors' preprocessing, their ``Dense`` network
(fp32, CPU, alone on its own canvas), the crop's stride-8 maps, ``core.detect`` on them,
the authors' ``Tail`` on the kept boxes and ``core.chain`` on its scores. One JSON line
per crop:

    {"file", "height": h8, "width": w8,            the crop's own map region, in cells
     "heat": [h8*w8], "box": [4*h8*w8], "ruby": [h8*w8],   channel-major, row-major
     "exp": "f64" | "numpy",
     "detected": null | {"boxes": [[x0, y0, x1, y1]], "scores": [...], "geo": [[...]]},
     "pair": [n*n] row-major, "start": [n], "end": [n], "seq": [...]}

Every number is an f32 value written exactly (as the f64 it converts to), so a reader
that parses f64 and casts to f32 gets the same bits. ``detected`` null means no
character (``pair``, ``start``, ``end`` and ``seq`` are then empty).

``exp``: ``core.detect`` takes the box sizes as ``np.exp`` of f32 values, which numpy
computes with its own vectorised f32 exp; that differs from the correctly rounded f32
exp in the last bit on some inputs and from machine to machine. Bunko computes exp in
f64 and rounds to f32 (the same on every platform), so by default ``core.detect`` runs
here with exactly that exp (``np.exp`` of the f64 value, cast to f32), and the dump says
``"exp": "f64"``; everything else is the authors' code as written. ``--numpy-exp`` keeps
numpy's own exp. The line also says whether numpy's own exp would have given the same
boxes, scores and geometry (``"numpy_exp_same"``).

usage: python charspot_dump.py --crops DIR --out FILE.jsonl [--n 1000] [--snapshot DIR] [--numpy-exp]
"""
from __future__ import annotations

import argparse
import contextlib
import json
import sys
import time
import types
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "torch_export"))
from torch_export import textbox as TB  # noqa: E402  (the pins and the authors' networks)

IMAGE_SUFFIXES = (".png", ".jpg", ".jpeg")


@contextlib.contextmanager
def f64_exp(core):
    """``core.np`` as numpy, but ``exp`` of an f32 array computed in f64 and rounded to f32."""
    import numpy as np

    class _Numpy(types.ModuleType):
        def __getattr__(self, name):
            return getattr(np, name)

    def exp(x, *a, **kw):
        x = np.asarray(x)
        if x.dtype == np.float32 and not a and not kw:
            return np.exp(x.astype(np.float64)).astype(np.float32)
        return np.exp(x, *a, **kw)

    shim = _Numpy("numpy_f64_exp")
    shim.exp = exp
    saved = core.np
    core.np = shim
    try:
        yield
    finally:
        core.np = saved


def _same(a, b) -> bool:
    import numpy as np

    if a is None or b is None:
        return a is None and b is None
    return all(np.array_equal(x.view(np.uint32), y.view(np.uint32)) for x, y in zip(a, b))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--crops", required=True, type=Path)
    ap.add_argument("--out", required=True, type=Path)
    ap.add_argument("--n", type=int, default=1000, help="the first N crops by file name (0: all)")
    ap.add_argument("--snapshot")
    ap.add_argument("--numpy-exp", action="store_true", help="core.detect with numpy's own f32 exp")
    ap.add_argument("--threads", type=int, default=8)
    a = ap.parse_args()

    import torch

    torch.set_num_threads(a.threads)
    snap = TB.charspot_snapshot(a.snapshot)
    TB.check_charspot(snap, [TB.CHARSPOT_PIN.file, "config.json", "manga_ocr_charspot_small/core.py",
                             "manga_ocr_charspot_small/model.py"])
    core, _ = TB.charspot_modules(snap)
    net = TB.load_charspot(snap)
    files = sorted(p for p in a.crops.iterdir() if p.suffix.lower() in IMAGE_SUFFIXES)
    files = files[: a.n] if a.n else files
    if not files:
        sys.exit(f"no crops in {a.crops}")
    t0 = time.perf_counter()
    found = numpy_same = 0
    a.out.parent.mkdir(parents=True, exist_ok=True)
    with open(a.out, "w", encoding="utf-8") as f, torch.no_grad():
        for p in files:
            x, _ = core.preprocess(core.to_rgb(p))
            H, W = x.shape[1:]
            heat, box, ruby, emb = net.dense(torch.from_numpy(x)[None].float(), torch.tensor([[H, W]]))
            h8, w8 = H // 8, W // 8
            heat, box, ruby = (t[0, :, :h8, :w8].numpy() for t in (heat, box, ruby))
            stock = core.detect(heat, box, ruby)
            if a.numpy_exp:
                det = stock
            else:
                with f64_exp(core):
                    det = core.detect(heat, box, ruby)
            numpy_same += _same(det, stock)
            row = {"file": p.name, "height": h8, "width": w8, "heat": heat.ravel().tolist(),
                   "box": box.ravel().tolist(), "ruby": ruby.ravel().tolist(),
                   "exp": "numpy" if a.numpy_exp else "f64", "numpy_exp_same": _same(det, stock),
                   "detected": None, "pair": [], "start": [], "end": [], "seq": []}
            if det is not None:
                found += 1
                kb, sc, geo = det
                t = net.tail
                ids, pair, st, en = t(t.pooled(emb, torch.from_numpy(kb)[None]), torch.from_numpy(geo)[None],
                                      torch.ones(1, len(kb), dtype=torch.bool))
                pair, st, en = pair[0].numpy(), st[0].numpy(), en[0].numpy()
                row.update({"detected": {"boxes": kb.tolist(), "scores": sc.tolist(), "geo": geo.tolist()},
                            "pair": pair.ravel().tolist(), "start": st.tolist(), "end": en.tolist(),
                            "seq": [int(k) for k in core.chain(pair, st, en)]})
            f.write(json.dumps(row) + "\n")
    print(f"{len(files)} crops ({found} with characters; numpy's own exp gives the same decode on {numpy_same}) "
          f"in {time.perf_counter() - t0:.1f}s -> {a.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
