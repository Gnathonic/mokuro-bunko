"""paddle-manga parity: the 0.5.2 torch recognizer vs the exported graphs on ORT (CPU EP).

Reference: ``PaddleMangaRecognizer(device="cpu", precision="fp32", fold=False)``
from the 0.5.2 reference checkout's ``src/mokuro_bunko/ocr/engine_runner.py`` (``MOKURO_REF_052``). Candidate: the
torch-free host in ``host.py`` over ``paddle-manga_*_{fp32,fp16}.onnx``.

Two passes over the crop set: the default token cap (64, every crop), and
tight per-crop caps (2..15 tokens; the set's lines are 1..17 characters) so
the per-row truncation of a batch run to its longest cap, and the
(cap, area)-sorted batch composition, are exercised.

Fails (exit 1) unless the fp32 graphs reproduce every reference text exactly
in both passes (100/100 + 100/100 on the default crop set). fp16 is reported.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

from PIL import Image

from ..common import DEFAULT_OUT, DEFAULT_PADDLE_CROPS, edit_distance, exact, import_runner, log, write_json
from . import export as E
from .host import load_host


def load_crops(root: Path) -> tuple[list[str], list[Image.Image]]:
    names = sorted(json.loads((root / "index.json").read_text(encoding="utf-8")))
    return names, [Image.open(root / n).convert("RGB") for n in names]


def tight_caps(n: int) -> list[int]:
    # deterministic; small enough that many rows stop at their cap, not at EOS
    return [2 + (i * 5) % 14 for i in range(n)]


def torch_reference(crops, caps_sets: dict, cache: Path, fresh: bool) -> dict[str, list[str]]:
    """Reference texts per caps set, cached in ``cache`` with the caps they were read at."""
    old = json.loads(cache.read_text(encoding="utf-8")) if cache.exists() and not fresh else {}
    ref, rec = {}, None
    for key, caps in caps_sets.items():
        if key in old and old[key].get("caps") == caps:
            ref[key] = old[key]
            continue
        if rec is None:
            rec = import_runner().PaddleMangaRecognizer(device="cpu", precision="fp32", fold=False)
        t0 = time.time()
        ref[key] = {"caps": caps, "texts": rec(crops, caps)}
        log(f"[paddle] torch reference ({key}): {len(crops)} crops in {time.time() - t0:.0f}s")
    write_json(cache, ref)
    return {k: v["texts"] for k, v in ref.items()}


def score(ref: list[str], hyp: list[str]) -> dict:
    same, diffs = exact(ref, hyp)
    edits = sum(edit_distance(h, r) for r, h in zip(ref, hyp, strict=True))
    return {"exact": same, "total": len(ref), "cer_edits": edits, "cer_chars": sum(len(r) for r in ref), "diffs": diffs}


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--crops", type=Path, default=DEFAULT_PADDLE_CROPS, help="dir with *.png + index.json")
    ap.add_argument("--threads", type=int, default=0)
    ap.add_argument("--fresh-ref", action="store_true")
    ap.add_argument("--precision", choices=("fp32", "fp16"), action="append")
    a = ap.parse_args(argv)

    names, crops = load_crops(a.crops)
    caps_sets = {"cap64": None, "tight_caps": tight_caps(len(crops))}
    pdir = a.out / "_parity"
    pdir.mkdir(parents=True, exist_ok=True)
    ref = torch_reference(crops, caps_sets, pdir / "paddle-manga_ref_torch_fp32.json", a.fresh_ref)
    report: dict = {"engine": E.ENGINE, "crops": str(a.crops), "n": len(crops),
                    "reference": "0.5.2 PaddleMangaRecognizer torch CPU fp32, fold=False"}
    for prec in a.precision or ("fp32", "fp16"):
        host = load_host(a.out, prec, a.threads)
        report[prec] = {}
        for key, caps in caps_sets.items():
            t0 = time.perf_counter()
            texts = host(crops, caps)
            spc = (time.perf_counter() - t0) / len(crops)
            r = score(ref[key], texts)
            r["ms_per_crop_cpu"] = round(1000 * spc, 1)
            report[prec][key] = r
            write_json(pdir / f"paddle-manga_ort_{prec}_{key}.json", texts)
            log(f"[paddle] ORT {prec} {key}: exact {r['exact']}/{r['total']}  CER {r['cer_edits']}/{r['cer_chars']}  {r['ms_per_crop_cpu']} ms/crop")
            for i, rt, ht in r["diffs"]:
                log(f"    {names[i]}: torch={rt!r} ort={ht!r}")
    ok = "fp32" in report and all(r["exact"] == len(crops) for r in report["fp32"].values())
    report["pass"] = ok
    write_json(pdir / "paddle-manga_parity.json", report)
    log(f"[paddle] parity {'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
