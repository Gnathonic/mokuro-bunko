"""Build and verify the WebGPU backend's graphs.

  python -m webgpu_export build  --engine hayai|paddle|all --out DIR [--mha]
  python -m webgpu_export verify --engine hayai|paddle --out DIR --models MODELS_V1 [--ep cpu|webgpu] [--limit N]

`build` needs the export environment (torch, transformers, peft, onnx) and the pinned
model snapshots in the Hugging Face cache; the bytes are reproducible (sequential names,
no timestamps). `verify` needs onnxruntime (CPU) or onnxruntime-webgpu, and reads the
spike crops and their torch fp32 texts from ~/.cache/mokuro-bunko-demo/onnx-spike and
MODELS_V1/_parity.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
import time
from pathlib import Path

import numpy as np

EXTERNAL_OVER = 1_500_000_000  # bytes: larger graphs keep their weights in <name>.data


def save(model, path: Path) -> list[Path]:
    import onnx

    path.parent.mkdir(parents=True, exist_ok=True)
    data = path.with_name(path.name + ".data")
    big = model.ByteSize() > EXTERNAL_OVER
    if data.exists():
        data.unlink()
    onnx.save_model(model, str(path), save_as_external_data=big, all_tensors_to_one_file=True,
                    location=data.name, size_threshold=1024)
    return [path, data] if big else [path]


def sha(p: Path) -> str:
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def build(a) -> None:
    written = []
    if a.engine in ("hayai", "all"):
        from . import hayai

        g = hayai.build(mha=a.mha)
        written += save(g["vision"], a.out / ("hayai-nova_webgpu_vision_fp32" + ("_mha" if a.mha else "") + ".onnx"))
        written += save(g["decoder"], a.out / "hayai-nova_webgpu_decoder_fp32.onnx")
    if a.engine in ("paddle", "all"):
        from . import paddle

        g = paddle.build_all()
        meta = g.pop("_meta")
        for k, m in g.items():
            written += save(m, a.out / f"paddle-manga_webgpu_{k.removeprefix('pv_')}_fp32.onnx")
        (a.out / "paddle-manga_webgpu_vision.json").write_text(json.dumps(meta) + "\n")
    for p in written:
        print(json.dumps({"file": p.name, "bytes": p.stat().st_size, "sha256": sha(p)}), flush=True)


def verify(a) -> None:
    from PIL import Image

    from . import host

    spike = Path.home() / ".cache/mokuro-bunko-demo/onnx-spike"
    if a.engine == "hayai":
        man = json.loads((spike / "hayai/crops.json").read_text(encoding="utf-8"))
        crops = [Image.open(spike / "hayai/crops" / m["file"]).convert("RGB") for m in man]
        ref = json.loads((spike / "hayai/ref_torch_cpu_fp32.json").read_text(encoding="utf-8"))
        ref = ref.get("texts", list(ref.values())) if isinstance(ref, dict) else ref
        rec = host.Hayai(a.models, a.out, a.ep, threads=a.threads)
    else:
        root = spike / "paddle/crops"
        names = sorted(json.loads((root / "index.json").read_text(encoding="utf-8")))
        crops = [Image.open(root / n).convert("RGB") for n in names]
        ref = json.loads((a.models / "_parity/paddle-manga_ref_torch_fp32.json").read_text(encoding="utf-8"))["cap64"]["texts"]
        rec = host.Paddle(a.models, a.out, a.ep, threads=a.threads)
    if a.limit:
        crops, ref = crops[: a.limit], ref[: a.limit]
    t = time.time()
    texts = rec(crops)
    exact = sum(x == y for x, y in zip(texts, ref))
    diffs = [(i, r, h) for i, (r, h) in enumerate(zip(ref, texts)) if r != h][:8]
    print(json.dumps({"engine": a.engine, "ep": a.ep, "exact": exact, "n": len(crops), "seconds": round(time.time() - t, 1),
                      "diffs": diffs}, ensure_ascii=False), flush=True)
    if exact != len(crops):
        sys.exit(1)


def main(argv=None) -> None:
    ap = argparse.ArgumentParser(prog="webgpu_export", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build")
    b.add_argument("--engine", choices=("hayai", "paddle", "all"), default="all")
    b.add_argument("--out", type=Path, required=True)
    b.add_argument("--mha", action="store_true", help="hayai vision with MultiHeadAttention (faster, NOT fp32-exact)")
    v = sub.add_parser("verify")
    v.add_argument("--engine", choices=("hayai", "paddle"), required=True)
    v.add_argument("--out", type=Path, required=True)
    v.add_argument("--models", type=Path, required=True)
    v.add_argument("--ep", choices=("cpu", "webgpu"), default="cpu")
    v.add_argument("--limit", type=int, default=0)
    v.add_argument("--threads", type=int, default=0)
    a = ap.parse_args(argv)
    np.set_printoptions(precision=4)
    {"build": build, "verify": verify}[a.cmd](a)


if __name__ == "__main__":
    main()
