"""Parity gate for the graph `export_nar.py` writes: on N crops, ONNX Runtime (CPU) running
the graph with the author's `model.safetensors` handed over from memory (as Bunko does)
must give the same output ids as the author's own PyTorch code in fp32, crop for crop.
Both read the inputs the author's `preprocess.py` makes.

It also writes the ids (and the text they spell) per crop as JSON lines, the expected
values of Bunko's ignored parity test, which reads the same crops with its own
preprocessing:

    MOKURO_NAR_PARITY_CROPS=<crops dir> MOKURO_NAR_PARITY_EXPECTED=<expected.jsonl> \\
    MOKURO_NAR_GRAPH=<graph> MOKURO_NAR_WEIGHTS=<model.safetensors> MOKURO_NAR_VOCAB=<vocabulary> \\
      cargo test -p bunko-engines --features nc-models --release --lib -- --ignored nar_parity

usage: python check_parity.py --graph GRAPH --crops DIR [--n 200] [--snapshot DIR] [--expected OUT.jsonl]
Exits non-zero unless every crop's ids are identical.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import export_nar as E  # noqa: E402  (the pins)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--graph", required=True)
    ap.add_argument("--crops", required=True, help="a directory of crop images (*.png, *.jpg)")
    ap.add_argument("--n", type=int, default=200, help="the first N crops by file name")
    ap.add_argument("--snapshot")
    ap.add_argument("--expected", help="write the ids per crop here (JSON lines)")
    ap.add_argument("--threads", type=int, default=8)
    a = ap.parse_args()

    import numpy as np
    import onnxruntime as ort
    import torch
    from PIL import Image

    snap = E.snapshot_dir(a.snapshot)
    E.check_pins(snap)
    torch.set_num_threads(a.threads)
    net = E.load_net(snap)  # the author's code, fp32, CPU
    from manga_ocr_nar_small.preprocess import empty_inputs, preprocess

    chars = json.loads((snap / "nar_vocabulary_6817.json").read_text(encoding="utf-8"))["characters"]

    weights = (snap / "model.safetensors").read_bytes()
    so = ort.SessionOptions()
    so.intra_op_num_threads = a.threads
    so.add_external_initializers_from_files_in_memory([E.WEIGHTS_LOCATION], [weights], [len(weights)])
    sess = ort.InferenceSession(a.graph, so, providers=["CPUExecutionProvider"])
    del weights

    files = sorted(p for p in Path(a.crops).iterdir() if p.suffix.lower() in (".png", ".jpg", ".jpeg"))[: a.n]
    if not files:
        sys.exit(f"no crops in {a.crops}")
    same, diffs, rows = 0, [], []
    t_ref = t_ort = 0.0
    for p in files:
        with Image.open(p) as im:
            img = im.convert("RGB")
        d = empty_inputs()
        preprocess(img, d)
        h, w = int(d["H"][0]), int(d["W"][0])
        t0 = time.perf_counter()
        with torch.no_grad():
            ref = net(torch.from_numpy(d["P"])[None], torch.from_numpy(d["V"] > 0.5)[None],
                      torch.from_numpy(d["R"]).long()[None], torch.from_numpy(d["C"]).long()[None],
                      torch.tensor([h]), torch.tensor([w]))[0].numpy()
        t1 = time.perf_counter()
        got = sess.run(["ids"], {"P": d["P"], "V": d["V"], "R": d["R"], "C": d["C"],
                                 "H": np.array([h], np.int32), "W": np.array([w], np.int32)})[0]
        t2 = time.perf_counter()
        t_ref += t1 - t0
        t_ort += t2 - t1
        if got.shape == (E.SLOTS,) and np.array_equal(ref, got):
            same += 1
        else:
            diffs.append((p.name, int((ref != got).sum()) if ref.shape == got.shape else -1))
        ids = [int(i) for i in got]
        text = "".join(chars[i] for i in (ids[: ids.index(len(chars))] if len(chars) in ids else ids) if i < len(chars))
        rows.append({"file": p.name, "ids": ids, "text": text})
    n = len(files)
    print(f"{n} crops: ids identical {same}/{n}; differing {diffs[:10]}; "
          f"torch {t_ref / n * 1e3:.0f} ms/crop, ORT {t_ort / n * 1e3:.0f} ms/crop")
    if a.expected:
        with open(a.expected, "w", encoding="utf-8") as f:
            for r in rows:
                f.write(json.dumps(r, ensure_ascii=False) + "\n")
    if same != n:
        sys.exit(1)


if __name__ == "__main__":
    main()
