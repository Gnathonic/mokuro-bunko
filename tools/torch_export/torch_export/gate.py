"""A text-box package's crop gate on a test machine, and the files it compares.

    python -m torch_export gate-bundle --out BUILD -e nar-small -p fp16 -t TARGET --crops DIR
                                      --reference FILE --fp32 FILE --commit C --dest DIR
    python -m torch_export gate-run BUNDLE          (what the bundle's run.sh calls)
    python -m torch_export compare-reads A B        texts of two reads files, texts_gate
    python -m torch_export block-preds READS BLOCKS OUT
                                                    crop texts -> paired.py's {key, pred} lines

``gate-bundle`` stages one self-contained directory per target (TEXTBOX-GPU.md §20.4): the
package and its ``build.json`` under ``package/<engine>/<precision>/<target>/``, the crop
images, the two expected files (``--reference``: the authors' output at this precision,
``check_parity.py --precision fp16``; ``--fp32``: the authors' fp32 output), the
exporter's sources at ``--commit`` (``git archive`` of tools/torch_export,
tools/textbox_export and tools/onnx_export), ``run.sh``, ``bundle.json`` and a
``SHA256SUMS`` over all of it. The authors' weights are never in a bundle: the test
machine reads them from its own Hugging Face cache (or ``TORCH_EXPORT_NAR_SNAPSHOT``),
and ``verify`` checks their pin.

``gate-run`` reads every crop at batch 1, 8 and 32 (``verify --reads``, one process each)
and passes when all three checks pass, each with at most ``floor(0.001 x crops)``
differing texts (``verify.texts_gate``):

  1. ``reference``: the batch-32 read against ``--reference`` (verify's own verdict);
  2. ``fp32``:      the batch-32 read against ``--fp32``;
  3. ``batches``:   every pair of the three reads (b1-b8, b1-b32, b8-b32).

It writes ``results/reads-b<N>.jsonl`` and ``results/summary.json`` (with crops/s per
batch: the verify loop, crop preparation on the CPU included).
"""

from __future__ import annotations

import hashlib
import io
import itertools
import json
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path
from typing import Callable

from .verify import IMAGE_SUFFIXES, check_textbox_meta, package_meta, texts_gate, weights_inside

REPO_ROOT = Path(__file__).resolve().parents[3]
SOURCES = ("tools/torch_export", "tools/textbox_export", "tools/onnx_export")
BATCHES = (1, 8, 32)
GATED_BATCH = 32
LIMIT = 0.001

RUN_SH = """#!/usr/bin/env bash
# The crop gate of one text-box package on this machine (TEXTBOX-GPU.md §20.4): every crop
# read at batch 1, 8 and 32, checked against the authors' output at this precision, against
# the authors' fp32 output, and pairwise across the batch sizes (bundle.json says which).
#
#   PYTHON=<a python with tools/torch_export's requirements for this target> ./run.sh
#
# The authors' weights are not in this bundle: TORCH_EXPORT_NAR_SNAPSHOT=<their snapshot>,
# or the Hugging Face cache (HF_HUB_OFFLINE=0 fetches it once; verify checks the pin).
# Run it only while the GPU is idle, under the lock:  flock -w 3600 ~/bench-gpu.lock ./run.sh
# Every file is checked against SHA256SUMS before the run and again after it.
# Exit 0: every check passed; 1: a check failed; other: the bundle or the run is broken.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"
sha256sum -c --quiet SHA256SUMS || { echo "the bundle's files differ from SHA256SUMS: not run" >&2; exit 3; }
rc=0
(cd src/tools/torch_export && "${PYTHON:-python3}" -m torch_export gate-run "$here") || rc=$?
sha256sum -c --quiet SHA256SUMS || { echo "the bundle's files changed during the run" >&2; exit 3; }
echo "results: $here/results/summary.json (exit $rc)"
exit "$rc"
"""


def _texts(path: Path) -> dict[str, str]:
    out = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            if line.strip():
                r = json.loads(line)
                out[r["file"]] = r["text"]
    return out


def compare_reads(a: Path, b: Path, limit: float = LIMIT, within: bool = False) -> dict:
    """Two reads files (``{"file", ..., "text"}`` lines: ``verify --reads``, the Rust parity
    test's ``MOKURO_NAR_PARITY_READS``, ``check_parity.py --expected``) crop by crop:
    ``{"crops", "differ": [file, ...], "limit", "passed"}``, passing with at most
    ``floor(crops x limit)`` differing texts. Both must name the same crops; with
    ``within``, ``a``'s crops are compared and ``b`` must have each of them."""
    ta, tb = _texts(a), _texts(b)
    sides = [(a, b, ta.keys() - tb.keys())] + ([] if within else [(b, a, tb.keys() - ta.keys())])
    for x, y, tx in sides:
        if tx:
            raise SystemExit(f"{len(tx)} crops only in {x} (not in {y}), first {sorted(tx)[:3]}")
    differ = sorted(f for f in ta if ta[f] != tb[f])
    return {"crops": len(ta), "differ": differ, "limit": int(len(ta) * limit),
            "passed": texts_gate(len(differ), len(ta), limit)}


def block_preds(reads: Path, blocks: Path, out: Path) -> int:
    """paired.py's input from crop texts: one ``{"key", "pred", "n_blocks"}`` line per row
    of ``blocks`` (``{"key", "crops": [...]}``, JMangaBench_Read's block crops), the
    block texts joined in block order (m5 ``join_blocks.py``'s rule). Returns the rows."""
    texts = _texts(reads)
    rows = []
    with open(blocks, encoding="utf-8") as f:
        for line in f:
            if line.strip():
                r = json.loads(line)
                missing = [c for c in r["crops"] if c not in texts]
                if missing:
                    raise SystemExit(f"{reads}: no text for {missing[:3]} (key {r['key']})")
                rows.append(json.dumps({"key": r["key"], "pred": "".join(texts[c] for c in r["crops"]),
                                        "n_blocks": len(r["crops"])}, ensure_ascii=False) + "\n")
    Path(out).write_text("".join(rows), encoding="utf-8")
    return len(rows)


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def _commit(repo: Path, commit: str) -> str:
    r = subprocess.run(["git", "-C", str(repo), "rev-parse", "--verify", "--quiet", f"{commit}^{{commit}}"],
                       capture_output=True, text=True)
    if r.returncode:
        raise SystemExit(f"{commit}: not a commit in {repo}")
    return r.stdout.strip()


def stage_bundle(out: Path, engine: str, precision: str, target: str, crops: Path, reference: Path, fp32: Path,
                 commit: str, dest: Path, repo: Path = REPO_ROOT) -> Path:
    """Stage ``dest`` (which must not exist) as the gate bundle of one built package
    (module docstring). Everything is checked before anything is written: the package
    binds its authors' weights the way its precision says, holds none of them, and maps
    every constant it has; each expected file has a row for every crop; ``commit`` is a
    commit of ``repo``."""
    from . import graphs as G
    from .textbox import PINS

    pin = PINS.get(engine)
    if pin is None:
        raise SystemExit(f"{engine}: not a text-box engine (those are {sorted(PINS)})")
    dest = Path(dest)
    if dest.exists():
        raise SystemExit(f"{dest}: exists; a bundle is staged once, into a new directory")
    tdir = Path(out) / engine / precision / target
    rec_path = tdir / "build.json"
    if not rec_path.is_file():
        raise SystemExit(f"{rec_path}: no build record")
    rec = json.loads(rec_path.read_text())
    pkgs = {}
    for role in G.ROLES[engine]:
        g = rec.get("graphs", {}).get(role)
        if g is None:
            raise SystemExit(f"{rec_path}: no graph {role}")
        p = tdir / g["file"]
        if g.get("sha256") and _sha256(p) != g["sha256"]:
            raise SystemExit(f"{p}: sha256 differs from {rec_path}")
        if weights_inside(p, pin.size):
            raise SystemExit(f"{p}: weights inside the package")
        meta = package_meta(p)
        check_textbox_meta(meta, precision, pin, p)
        bound = json.loads(meta.get("bunko.weights", "{}"))
        if not bound:
            raise SystemExit(f"{p}: no bunko.weights map: its constants are not bound to the authors' {pin.file}")
        if g.get("constants") != len(bound):
            raise SystemExit(f"{p}: {g.get('constants')} constants, {len(bound)} in bunko.weights")
        pkgs[role] = p
    images = sorted(p for p in Path(crops).iterdir() if p.suffix.lower() in IMAGE_SUFFIXES)
    if not images:
        raise SystemExit(f"no crops in {crops}")
    if Path(reference).name == Path(fp32).name:
        raise SystemExit(f"--reference and --fp32 are both named {Path(fp32).name}")
    for f in (reference, fp32):
        rows = _texts(f)
        missing = [p.name for p in images if p.name not in rows]
        if missing:
            raise SystemExit(f"{f}: no row for {len(missing)} crops, first {missing[:3]}")
    sha = _commit(repo, commit)
    if rec.get("commit") not in (None, sha):
        raise SystemExit(f"{rec_path}: built from {rec['commit']}, not {sha}")
    tar = subprocess.run(["git", "-C", str(repo), "archive", "--format=tar", sha, *SOURCES],
                         capture_output=True, check=True).stdout

    part = dest.with_name(dest.name + ".partial")
    if part.exists():
        shutil.rmtree(part)
    try:
        pdir = part / "package" / engine / precision / target
        pdir.mkdir(parents=True)
        shutil.copy2(rec_path, pdir / "build.json")
        for p in pkgs.values():
            shutil.copy2(p, pdir / p.name)
        (part / "crops").mkdir()
        for p in images:
            # copies, never hard links: a bundle edited in place must not change the crop set
            shutil.copy2(p, part / "crops" / p.name)
        (part / "expected").mkdir()
        for f in (reference, fp32):
            shutil.copy2(f, part / "expected" / Path(f).name)
        with tarfile.open(fileobj=io.BytesIO(tar)) as t:
            t.extractall(part / "src", filter="data")
        (part / "run.sh").write_text(RUN_SH)
        (part / "run.sh").chmod(0o755)
        info = {"engine": engine, "precision": precision, "target": target, "commit": sha,
                "crops": len(images), "batches": list(BATCHES), "gated_batch": GATED_BATCH, "limit": LIMIT,
                "device": "cuda", "reference": f"expected/{Path(reference).name}",
                "fp32": f"expected/{Path(fp32).name}",
                "packages": {role: {"file": p.name, "sha256": _sha256(p)} for role, p in pkgs.items()}}
        (part / "bundle.json").write_text(json.dumps(info, indent=1) + "\n")
        files = sorted(p for p in part.rglob("*") if p.is_file())
        (part / "SHA256SUMS").write_text("".join(f"{_sha256(p)}  {p.relative_to(part).as_posix()}\n"
                                                 for p in files))
        part.rename(dest)
    except BaseException:
        shutil.rmtree(part, ignore_errors=True)
        raise
    print(f"staged {dest}: {len(images)} crops, {len(files) + 1} files, commit {sha[:12]}", file=sys.stderr)
    return dest


def _verify_process(bundle: Path, info: dict, batch: int, expected: Path, reads: Path) -> dict:
    """One ``verify --reads`` run in a process of its own (a loaded package is never
    released and loaded again in one process; on Windows verify ends its process)."""
    cmd = [sys.executable, "-m", "torch_export", "verify", str(bundle / "package"), info["engine"], info["precision"],
           info["target"], "--device", info.get("device", "cuda"), "--crops", str(bundle / "crops"),
           "--expected", str(expected), "--batch", str(batch), "--reads", str(reads)]
    print("+", " ".join(cmd), file=sys.stderr, flush=True)
    r = subprocess.run(cmd, cwd=bundle / "src" / "tools" / "torch_export", stdout=subprocess.PIPE, text=True)
    lines = [line for line in r.stdout.splitlines() if line.startswith("{")]
    if not lines:
        return {"error": f"verify exited {r.returncode} without a result", "passed": False}
    return {**json.loads(lines[-1]), "returncode": r.returncode}


def run_gate(bundle: Path, verify: Callable[[Path, dict, int, Path, Path], dict] = _verify_process) -> dict:
    """The bundle's three checks (module docstring); writes and returns the summary."""
    bundle = Path(bundle).resolve()
    info = json.loads((bundle / "bundle.json").read_text())
    results = bundle / "results"
    results.mkdir(exist_ok=True)
    reads = {b: results / f"reads-b{b}.jsonl" for b in info["batches"]}
    runs = {}
    for b, path in reads.items():
        path.unlink(missing_ok=True)
        runs[b] = verify(bundle, info, b, bundle / info["reference"], path)

    def compared(a: Path, b: Path, within: bool = False) -> dict:
        if not a.is_file() or not b.is_file():
            return {"error": f"no reads in {a if not a.is_file() else b}", "passed": False}
        return compare_reads(a, b, info["limit"], within)

    g = info["gated_batch"]
    ref = runs[g]
    checks = {
        "reference": {"crops": ref.get("crops"), "differ": ref.get("differ"), "passed": bool(ref.get("passed")),
                      **({"error": ref["error"]} if "error" in ref else {})},
        # an expected file may name more crops than the bundle has (staging checked it has them all)
        "fp32": compared(reads[g], bundle / info["fp32"], within=True),
        "batches": {f"b{x}-b{y}": compared(reads[x], reads[y]) for x, y in itertools.combinations(info["batches"], 2)},
    }
    passed = checks["reference"]["passed"] and checks["fp32"]["passed"] and all(
        c["passed"] for c in checks["batches"].values())
    summary = {k: info[k] for k in ("engine", "precision", "target", "commit", "crops")}
    summary.update({"checks": checks, "crops_per_s": {str(b): r.get("crops_per_s") for b, r in runs.items()},
                    "passed": passed})
    (results / "summary.json").write_text(json.dumps(summary, indent=1, ensure_ascii=False) + "\n")
    return summary
