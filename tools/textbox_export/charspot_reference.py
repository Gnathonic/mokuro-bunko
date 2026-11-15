"""charspot-small's expected output: the authors' own CPU backend (``OnnxBackend``, ONNX
Runtime fp32, one crop at a time, ``muscgab/manga-ocr-charspot-small`` at the pin) on
the first N crops of a folder, written as JSON lines

    {"file": "<crop name>", "text": "...", "chars": [[c, x0, y0, x1, y1, score], ...]}

in reading order, boxes in crop pixels. The numbers are the authors' ``core.result``
before it rounds (box x0..y1 to 0.1 px, the score to 4 places), so a 0.5 px gate is
measured against the exact values. This is the reference of
``python -m torch_export.verify OUT charspot-small fp32 TARGET --crops DIR --expected FILE``
and of Bunko's ignored ``charspot_torch_parity`` test (TEXTBOX-GPU.md §20.1).

usage: python charspot_reference.py --crops DIR --out FILE.jsonl [--n N] [--snapshot DIR] [--threads 8]
       (default snapshot: the Hugging Face cache at the pin, ``HF_HUB_OFFLINE=1`` works once fetched)
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "torch_export"))
from torch_export import textbox as TB  # noqa: E402  (the pins and the unrounded result)

IMAGE_SUFFIXES = (".png", ".jpg", ".jpeg")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--crops", required=True, type=Path, help="a directory of crop images (*.png, *.jpg)")
    ap.add_argument("--out", required=True, type=Path, help="the JSON lines to write")
    ap.add_argument("--n", type=int, default=None, help="only the first N crops by file name")
    ap.add_argument("--snapshot", help="the authors' repository at the pin (default: the Hugging Face cache)")
    ap.add_argument("--threads", type=int, default=8)
    a = ap.parse_args()

    import numpy as np

    snap = TB.charspot_snapshot(a.snapshot)
    TB.check_charspot(snap, ["model.onnx", "config.json", "vocab.json", "manga_ocr_charspot_small/core.py",
                             "manga_ocr_charspot_small/backends/onnx.py"])
    core, _ = TB.charspot_modules(snap)
    from manga_ocr_charspot_small.backends.onnx import OnnxBackend

    ort = OnnxBackend(snap, threads=a.threads)
    files = sorted(p for p in a.crops.iterdir() if p.suffix.lower() in IMAGE_SUFFIXES)
    files = files[: a.n] if a.n else files
    if not files:
        sys.exit(f"no crops in {a.crops}")
    t0 = time.perf_counter()
    a.out.parent.mkdir(parents=True, exist_ok=True)
    with open(a.out, "w", encoding="utf-8") as f:
        for p in files:
            # OnnxBackend.read_one, with charspot_result in place of the rounding core.result
            x, fxy = core.preprocess(core.to_rgb(p))
            boxes, scores, ids, order = ort.sess.run(None, {"img": x[None].astype(np.float32)})
            r = (TB.charspot_result(order.tolist(), ids, boxes, scores, fxy, ort.out_str) if len(order)
                 else {"text": "", "chars": []})
            f.write(json.dumps({"file": p.name, **r}, ensure_ascii=False) + "\n")
    dt = time.perf_counter() - t0
    print(f"{len(files)} crops in {dt:.1f}s ({len(files) / dt:.1f} crops/s) -> {a.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
