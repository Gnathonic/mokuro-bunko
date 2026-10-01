"""Write ``models.json``: every release file with its size, sha256, licence and provenance.

The Rust processor reads this to know what to download for an engine, where
from, and how to verify it (ARCHITECTURE.md §7). It is written next to the
artifacts and published as an asset of the same release. Missing or
unexpected files are errors: the manifest describes exactly one complete set.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from . import EXPORT_TOOL_VERSION, RELEASE_TAG
from .common import DEFAULT_OUT, log, sha256, write_json
from .hayai import export as HE
from .paddle import export as PE
from .pins import ENGINE_SOURCES, PPOCR, PPOCR_FILES

URL = "https://github.com/Gnathonic/mokuro-bunko/releases/download/{tag}/{file}"
MANIFEST = "models.json"
FORMAT = "mokuro-bunko-models/1"


def expected(out: Path) -> list[dict]:
    """(engine, file, role, precision, part_of) for the complete set, in a stable order."""
    rows: list[dict] = []

    def onnx(engine: str, fname: str, role: str, precision: str) -> None:
        rows.append({"engine": engine, "file": fname, "kind": "onnx", "role": role, "precision": precision})
        for data in sorted(out.glob(fname + ".data*")):
            rows.append({"engine": engine, "file": data.name, "kind": "onnx-external-data", "role": role,
                         "precision": precision, "part_of": fname})

    for prec in ("fp32", "fp16"):
        for role in ("vision", "decoder"):
            onnx(HE.ENGINE, HE.name(role, prec), role, prec)
    rows += [
        {"engine": HE.ENGINE, "file": HE.POS_TABLE, "kind": "npy", "role": "pos_table", "precision": "fp32"},
        {"engine": HE.ENGINE, "file": HE.TOKEN_EMBEDDINGS, "kind": "npy", "role": "token_embeddings", "precision": "fp32"},
        {"engine": HE.ENGINE, "file": HE.TOKENIZER, "kind": "tokenizer", "role": "tokenizer"},
        {"engine": HE.ENGINE, "file": HE.CONFIG, "kind": "config", "role": "config"},
    ]
    for prec in ("fp32", "fp16"):
        for role in ("vision", "decoder"):
            onnx(PE.ENGINE, PE.name(role, prec), role, prec)
        rows.append({"engine": PE.ENGINE, "file": PE.embed_name(prec), "kind": "npy", "role": "embed", "precision": prec})
    rows += [
        {"engine": PE.ENGINE, "file": PE.TOKENIZER, "kind": "tokenizer", "role": "tokenizer"},
        {"engine": PE.ENGINE, "file": PE.CONFIG, "kind": "config", "role": "config"},
    ]
    roles = {"det": "detector", "rec": "recognizer", "dict": "dictionary"}
    for flat, repo_path in PPOCR_FILES.items():
        role = next(v for k, v in roles.items() if f"_{k}" in flat)
        rows.append({"engine": "ppocr-manga", "file": flat, "kind": "onnx" if flat.endswith(".onnx") else "dictionary",
                     "role": role, "precision": "fp32" if flat.endswith(".onnx") else None,
                     "source_path": f"{PPOCR.repo}@{PPOCR.revision}:{repo_path}", "exported": False})
    return rows


def parity_summary(out: Path) -> dict:
    """The gate results from ``_parity/*_parity.json`` (written by the check_parity scripts)."""
    summary = {}
    for f in sorted((out / "_parity").glob("*_parity.json")):
        r = json.loads(f.read_text(encoding="utf-8"))
        eng = {"pass": r["pass"], "crops": r["n"], "reference": r["reference"], "runtime": "onnxruntime CPU EP"}
        for prec in ("fp32", "fp16"):
            if prec not in r:
                continue
            v = r[prec]
            if "exact" in v:
                eng[prec] = f"{v['exact']}/{v['total']} exact, CER {v['cer_edits']}/{v['cer_chars']}"
            else:
                eng[prec] = {k: f"{x['exact']}/{x['total']} exact, CER {x['cer_edits']}/{x['cer_chars']}" for k, x in v.items()}
        summary[r["engine"]] = eng
    return summary


def build(out: Path, tag: str = RELEASE_TAG, allow_unverified: bool = False) -> dict:
    parity = parity_summary(out)
    unverified = [e for e in ("hayai-nova", "paddle-manga") if not parity.get(e, {}).get("pass")]
    if unverified and not allow_unverified:
        raise SystemExit(f"no passing parity report for {unverified}: run check_parity first (or --allow-unverified)")
    rows = expected(out)
    names = {r["file"] for r in rows}
    missing = [n for n in sorted(names) if not (out / n).is_file()]
    if missing:
        raise SystemExit(f"missing from {out}: {missing}")
    extra = sorted(p.name for p in out.iterdir() if p.is_file() and p.name not in names and p.name != MANIFEST)
    if extra:
        raise SystemExit(f"unexpected files in {out} (not in the manifest): {extra}")
    files = []
    for r in rows:
        p = out / r["file"]
        srcs = ENGINE_SOURCES[r["engine"]]
        entry = {
            "engine": r["engine"],
            "file": r["file"],
            "url": URL.format(tag=tag, file=r["file"]),
            "size": p.stat().st_size,
            "sha256": sha256(p),
            "licence": " AND ".join(sorted({s.licence for s in srcs})),
            "sources": [{"repo": s.repo, "revision": s.revision, "licence": s.licence} for s in srcs],
            "export_tool_version": EXPORT_TOOL_VERSION,
        }
        entry.update({k: v for k, v in r.items() if k not in ("engine", "file") and v is not None})
        files.append(entry)
        log(f"  {entry['size']:>13,}  {entry['sha256']}  {entry['file']}")
    manifest = {
        "format": FORMAT,
        "release": tag,
        "export_tool": "tools/onnx_export (mokuro-bunko-onnx-export)",
        "export_tool_version": EXPORT_TOOL_VERSION,
        "parity": parity,
        "files": files,
    }
    write_json(out / MANIFEST, manifest)
    big = max(files, key=lambda f: f["size"])
    log(f"wrote {out / MANIFEST}: {len(files)} files, {sum(f['size'] for f in files):,} bytes; largest {big['file']} {big['size']:,}")
    return manifest


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--tag", default=RELEASE_TAG)
    ap.add_argument("--allow-unverified", action="store_true", help="write it even without passing parity reports")
    a = ap.parse_args(argv)
    build(a.out, a.tag, a.allow_unverified)
    return 0


if __name__ == "__main__":
    sys.exit(main())
