"""Dump the hayai line crops' host inputs for the C++ bench, and score its output.

    python -m ncnn_export.dump crops OUT.bin [--limit N]
    python -m ncnn_export.dump score IDS.jsonl [--ref ref_torch_cpu_fp32.json]
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
from pathlib import Path

import numpy as np
from PIL import Image

from .host import PosCache, crop_inputs

CROPS = Path.home() / ".cache/mokuro-bunko-demo/onnx-spike/hayai"
MODELS = Path.home() / ".cache/mokuro-bunko-demo/models-v1"


def dump(out: Path, limit: int) -> None:
    man = json.loads((CROPS / "crops.json").read_text())[: limit or None]
    pos = PosCache(np.load(MODELS / "hayai-nova_pos_table.npy"))
    with open(out, "wb") as f:
        f.write(struct.pack("<i", len(man)))
        for c in man:
            ci = crop_inputs(Image.open(CROPS / "crops" / c["file"]).convert("RGB"), pos)
            hp, wp, *_ = ci["grid"]
            f.write(struct.pack("<ii", hp, wp))
            f.write(ci["pv"].astype(np.float32).tobytes())
            f.write(ci["pos"].astype(np.float32).tobytes())


def score(ids: Path, ref: Path) -> int:
    from tokenizers import Tokenizer

    tok = Tokenizer.from_file(str(MODELS / "hayai-nova_tokenizer.json"))
    texts = [tok.decode(json.loads(l), skip_special_tokens=True).strip() for l in ids.read_text().splitlines()]
    r = json.loads(ref.read_text())
    r = r["texts"] if isinstance(r, dict) else r
    r = r[: len(texts)]
    diffs = [(i, a, b) for i, (a, b) in enumerate(zip(r, texts)) if a != b]
    print(json.dumps({"ref": ref.name, "exact": len(texts) - len(diffs), "total": len(texts), "diffs": diffs[:8]},
                     ensure_ascii=False))
    return 0 if not diffs else 1


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("cmd", choices=["crops", "score"])
    ap.add_argument("path", type=Path)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--ref", type=Path, default=CROPS / "ref_torch_cpu_fp32.json")
    a = ap.parse_args(argv)
    if a.cmd == "crops":
        dump(a.path, a.limit)
        return 0
    return score(a.path, a.ref)


if __name__ == "__main__":
    sys.exit(main())
