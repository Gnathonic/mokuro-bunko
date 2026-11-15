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

The fp16 reference (TEXTBOX-GPU.md §11, the expected texts of the fp16 package's gate):

    python check_parity.py --precision fp16 --device cuda --crops DIR --n N --expected OUT.jsonl [--batch 48]

runs the authors' own `PyTorchBackend(device="cuda", fp16=True)` (their GPU default) over the
crops, `--batch` per call (their default `max_batch`, 48), and writes the same JSON lines. No
graph and nothing to compare: it is the reference `python -m torch_export.verify` reads.
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import export_nar as E  # noqa: E402  (the pins)

# the authors' backend the fp16 reference runs, at the same pin (sha256 from the
# repository's own SHA256SUMS); export_nar.PINS does not fetch it
REFERENCE_PINS = {
    "manga_ocr_nar_small/backends/__init__.py": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    "manga_ocr_nar_small/backends/pytorch.py": "03359cd002ee6e7da4d19e68ae8b9fc12c84350855510f15d4e67ba50d96aa4c",
}


def rows_of(files, ids_per_file, chars) -> list[dict]:
    rows = []
    for p, ids in zip(files, ids_per_file):
        ids = [int(i) for i in ids]
        text = "".join(chars[i] for i in (ids[: ids.index(len(chars))] if len(chars) in ids else ids) if i < len(chars))
        rows.append({"file": p.name, "ids": ids, "text": text})
    return rows


def write_expected(path, rows) -> None:
    with open(path, "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")


def fp16_reference(a, snap: Path, files) -> None:
    """The authors' PyTorchBackend(fp16=True) over ``files``, ``a.batch`` crops per call."""
    import numpy as np
    from PIL import Image

    missing = [rel for rel in REFERENCE_PINS if not (snap / rel).is_file()]
    if missing and a.snapshot:
        sys.exit(f"{a.snapshot}: no {missing[0]} (the authors' backend at the pin)")
    if missing:
        from huggingface_hub import snapshot_download
        snapshot_download(E.REPO, revision=E.REVISION, allow_patterns=list(REFERENCE_PINS))
    for rel, want in REFERENCE_PINS.items():
        got = E.sha256(snap / rel)
        if got != want:
            sys.exit(f"{rel}: sha256 {got}, pinned {want}")
    import torch
    from manga_ocr_nar_small.backends.pytorch import PyTorchBackend

    torch.set_num_threads(a.threads)
    be = PyTorchBackend(snap, device="cuda", fp16=True, max_batch=a.batch)
    if be.dt != torch.float16:
        sys.exit(f"the authors' backend runs {be.dt}, not float16")
    chars = json.loads((snap / "nar_vocabulary_6817.json").read_text(encoding="utf-8"))["characters"]
    t0 = time.perf_counter()
    ids = []
    for i in range(0, len(files), a.batch):
        arrs = []
        for p in files[i:i + a.batch]:
            with Image.open(p) as im:
                arrs.append(np.asarray(im.convert("RGB"), np.uint8))
        ids.extend(be.ids(arrs))
    dt = time.perf_counter() - t0
    print(f"{len(files)} crops through the authors' fp16 PyTorchBackend, batch {a.batch}: "
          f"{dt / len(files) * 1e3:.1f} ms/crop")
    if a.expected:
        write_expected(a.expected, rows_of(files, ids, chars))


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--graph", help="the ONNX graph (fp32); not used by --precision fp16")
    ap.add_argument("--crops", required=True, help="a directory of crop images (*.png, *.jpg)")
    ap.add_argument("--n", type=int, default=200, help="the first N crops by file name")
    ap.add_argument("--snapshot")
    ap.add_argument("--expected", help="write the ids per crop here (JSON lines)")
    ap.add_argument("--threads", type=int, default=8)
    ap.add_argument("--precision", choices=("fp32", "fp16"), default="fp32",
                    help="fp16: the authors' PyTorchBackend(fp16=True) reference, no graph")
    ap.add_argument("--device", choices=("cpu", "cuda"), default="cpu", help="fp16 runs on cuda")
    ap.add_argument("--batch", type=int, default=48, help="fp16: crops per call (the authors' max_batch)")
    a = ap.parse_args()
    if a.precision == "fp32" and (a.graph is None or a.device != "cpu"):
        ap.error("fp32 compares --graph (ONNX Runtime, CPU) with the authors' code on the CPU: "
                 "--graph is required and --device is cpu")
    if a.precision == "fp16" and a.device != "cuda":
        ap.error("--precision fp16 runs the authors' GPU mode: --device cuda")

    if a.precision == "fp16":
        snap = E.snapshot_dir(a.snapshot)
        E.check_pins(snap)
        sys.path.insert(0, str(snap))
        files = sorted(p for p in Path(a.crops).iterdir() if p.suffix.lower() in (".png", ".jpg", ".jpeg"))[: a.n]
        if not files:
            sys.exit(f"no crops in {a.crops}")
        fp16_reference(a, snap, files)
        return

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
        rows.extend(rows_of([p], [got], chars))
    n = len(files)
    print(f"{n} crops: ids identical {same}/{n}; differing {diffs[:10]}; "
          f"torch {t_ref / n * 1e3:.0f} ms/crop, ORT {t_ort / n * 1e3:.0f} ms/crop")
    if a.expected:
        write_expected(a.expected, rows)
    if same != n:
        sys.exit(1)


if __name__ == "__main__":
    main()
