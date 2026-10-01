"""hayai-nova parity: the 0.5.2 torch recognizer vs the exported graphs on ORT (CPU EP).

Reference: ``HayaiNovaRecognizer(device="cpu", precision="fp32", fold=False)``
from this checkout's ``src/mokuro_bunko/ocr/engine_runner.py`` -- the texts the
reconciled road consumes (un-folded, ``str.strip()``ed). Candidate: the
torch-free host in ``host.py`` (PIL preprocessing, numpy tables, ``tokenizers``
decode) over ``hayai-nova_*_{fp32,fp16}.onnx``.

Fails (exit 1) unless fp32 reproduces every reference text exactly
(220/220 on the default crop set). fp16 is reported, not gated. With
``--audit`` it also measures max |activation| of every float tensor of the
fp32 graphs, the evidence for which ops the fp16 graphs keep in fp32.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

from PIL import Image

from ..common import DEFAULT_HAYAI_CROPS, DEFAULT_OUT, edit_distance, exact, import_runner, log, write_json
from . import export as E
from .host import load_host


def load_crops(root: Path) -> tuple[list[str], list[Image.Image]]:
    man = json.loads((root / "crops.json").read_text(encoding="utf-8"))
    names = [m["file"] for m in man]
    return names, [Image.open(root / "crops" / n).convert("RGB") for n in names]


def torch_reference(crops, cache: Path, fresh: bool) -> list[str]:
    if cache.exists() and not fresh:
        return json.loads(cache.read_text(encoding="utf-8"))
    er = import_runner()
    t0 = time.time()
    rec = er.HayaiNovaRecognizer(device="cpu", precision="fp32", fold=False)
    texts = rec(crops)
    log(f"[hayai] torch reference: {len(texts)} crops in {time.time() - t0:.0f}s")
    write_json(cache, texts)
    return texts


def score(ref: list[str], hyp: list[str]) -> dict:
    same, diffs = exact(ref, hyp)
    edits = sum(edit_distance(h, r) for r, h in zip(ref, hyp, strict=True))
    chars = sum(len(r) for r in ref)
    return {"exact": same, "total": len(ref), "cer_edits": edits, "cer_chars": chars, "diffs": diffs}


def run_ort(out: Path, precision: str, crops, threads: int, vision=None, decoder=None) -> tuple[list[str], float]:
    host = load_host(out, precision, threads)
    if vision is not None:  # a comparison variant from _stage/
        from .host import OrtBackend

        host.be = OrtBackend(vision, decoder, threads)
    t0 = time.perf_counter()
    texts = host(crops)
    return texts, (time.perf_counter() - t0) / len(crops)


def audit(out: Path, crops, threads: int) -> dict:
    from ..fp16 import Audit

    aud = {"vision": Audit(out / E.name("vision", "fp32"), threads), "decoder": Audit(out / E.name("decoder", "fp32"), threads)}
    host = load_host(out, "fp32", threads, audit=aud)
    t0 = time.time()
    host(crops, batch=4)
    log(f"[hayai] activation audit over {len(crops)} crops in {time.time() - t0:.0f}s")
    return {k: a.report() for k, a in aud.items()}


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT, help="directory holding the exported files")
    ap.add_argument("--crops", type=Path, default=DEFAULT_HAYAI_CROPS, help="dir with crops.json + crops/")
    ap.add_argument("--threads", type=int, default=0)
    ap.add_argument("--fresh-ref", action="store_true", help="recompute the torch reference even if cached")
    ap.add_argument("--audit", action="store_true", help="measure fp32 activation ranges (fp16 overflow evidence)")
    a = ap.parse_args(argv)

    names, crops = load_crops(a.crops)
    pdir = a.out / "_parity"
    pdir.mkdir(parents=True, exist_ok=True)
    ref = torch_reference(crops, pdir / "hayai-nova_ref_torch_fp32.json", a.fresh_ref)
    report: dict = {"engine": E.ENGINE, "crops": str(a.crops), "n": len(crops), "reference": "0.5.2 HayaiNovaRecognizer torch CPU fp32, fold=False"}
    for prec in ("fp32", "fp16"):
        texts, spc = run_ort(a.out, prec, crops, a.threads)
        r = score(ref, texts)
        r["ms_per_crop_cpu"] = round(1000 * spc, 1)
        report[prec] = r
        write_json(pdir / f"hayai-nova_ort_{prec}.json", texts)
        log(f"[hayai] ORT {prec}: exact {r['exact']}/{r['total']}  CER {r['cer_edits']}/{r['cer_chars']}  {r['ms_per_crop_cpu']} ms/crop")
        for i, rt, ht in r["diffs"]:
            log(f"    {names[i]}: torch={rt!r} ort={ht!r}")
    stage = a.out / "_stage"
    if (stage / "hayai_vision_fp16full.onnx").exists():
        texts, spc = run_ort(a.out, "fp16", crops, a.threads, stage / "hayai_vision_fp16full.onnx", stage / "hayai_decoder_fp16full.onnx")
        r = score(ref, texts)
        r["ms_per_crop_cpu"] = round(1000 * spc, 1)
        report["fp16_full_unpublished"] = r
        log(f"[hayai] ORT fp16 (full conversion, not published): exact {r['exact']}/{r['total']}  CER {r['cer_edits']}/{r['cer_chars']}")
    if a.audit:
        report["activation_audit"] = audit(a.out, crops, a.threads)
        for k, v in report["activation_audit"].items():
            log(f"[hayai] {k} max |x| by op: " + ", ".join(f"{op}={m:.4g}" for op, m in v["max_abs_by_op"].items()))
            log(f"[hayai] {k} tensors over fp16 max: {v['n_over_fp16_max']}; top: {v['top'][:5]}")
    ok = report["fp32"]["exact"] == len(crops)
    report["pass"] = ok
    write_json(pdir / "hayai-nova_parity.json", report)
    log(f"[hayai] parity {'PASS' if ok else 'FAIL'}: fp32 {report['fp32']['exact']}/{len(crops)} (gate), fp16 {report['fp16']['exact']}/{len(crops)} (reported)")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
