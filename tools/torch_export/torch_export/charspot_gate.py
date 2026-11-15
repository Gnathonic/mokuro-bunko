"""charspot-small's fp32 crop gate on a test machine: the bundle, its run, the NOTICE text.

    python -m torch_export.charspot_gate bundle --out BUILD -t TARGET --crops DIR --expected FILE
                                                --commit C --dest DIR
    python -m torch_export.charspot_gate fetch      the authors' files at the pin, checked
    python -m torch_export.charspot_gate run BUNDLE (what the bundle's run.sh calls)
    python -m torch_export.charspot_gate notice --commit C --manifest FILE
                                                    the charspot section of the release NOTICE.md

``bundle`` stages one self-contained directory per target (TEXTBOX-GPU.md §20.3, plan 03
Task 6 Step 4): ``dense.pt2``, ``tail.pt2`` and their ``build.json`` under
``package/charspot-small/fp32/<target>/``, the crop images, the expected output
(``--expected``: ``tools/textbox_export/charspot_reference.py``'s lines, the authors'
ONNX model on the CPU, staged as ``expected/charspot-fp32-expected.jsonl``), the
exporter's sources at ``--commit`` (``git archive``; the commit must hold this module),
``run.sh``, ``bundle.json`` and a ``SHA256SUMS`` over all of it. Everything is checked
before anything is written. The authors' weights are never in a bundle: ``run.sh`` fetches
their repository at the pin into the test machine's Hugging Face cache (or reads
``TORCH_EXPORT_CHARSPOT_SNAPSHOT``) and checks every pinned sha256, ``model.safetensors``'
included, before anything runs.

``run`` reads every crop at canvas batch 1, 8, 32 and 64 (``verify``, one process each)
and passes only when every batch reads every crop ``same_chars`` as the expected output
(same text and order, boxes within 0.5 px). fp32 has no other tolerance, and a target
that fails is left out of the manifest (decision L1). It writes ``results/summary.json``
with each batch's crops/s (the verify loop, crop preparation on the CPU included), its
largest box move and torch's peak reserved GPU memory.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path
from typing import Callable

from .verify import IMAGE_SUFFIXES, read_expected_chars, weights_inside

ENGINE = "charspot-small"
PRECISION = "fp32"
ROLES = ("dense", "tail")
REPO_ROOT = Path(__file__).resolve().parents[3]
SOURCES = ("tools/torch_export", "tools/textbox_export", "tools/onnx_export")
# the canvas batch: 64 is the runtime's own cap (charspot_batch_cap), 1 a crop alone
BATCHES = (1, 8, 32, 64)
EXPECTED = "charspot-fp32-expected.jsonl"

RUN_SH = """#!/usr/bin/env bash
# charspot-small's fp32 crop gate on this machine (TEXTBOX-GPU.md §20.3): every crop read at
# canvas batch 1, 8, 32 and 64, each checked against the authors' ONNX model on the CPU (same
# text and order, boxes within 0.5 px). bundle.json says what is in the bundle.
#
#   PYTHON=<a python with tools/torch_export's requirements for this target> ./run.sh
#
# The authors' weights are not in this bundle: the first step fetches their repository at the
# pin into the Hugging Face cache (or reads TORCH_EXPORT_CHARSPOT_SNAPSHOT) and checks every
# pinned sha256, model.safetensors' included, before anything runs.
# Run it only while the GPU is idle, under the lock:  flock -w 3600 ~/bench-gpu.lock ./run.sh
# Every file is checked against SHA256SUMS before the run and again after it.
# Exit 0: every batch passed; 1: a batch failed; other: the bundle or the run is broken.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"
py="${PYTHON:-python3}"
sha256sum -c --quiet SHA256SUMS || { echo "the bundle's files differ from SHA256SUMS: not run" >&2; exit 3; }
(cd src/tools/torch_export && "$py" -m torch_export.charspot_gate fetch) || {
  echo "the authors' files at the pin could not be fetched or do not match their sha256: not run" >&2; exit 3; }
rc=0
(cd src/tools/torch_export && "$py" -m torch_export.charspot_gate run "$here") || rc=$?
sha256sum -c --quiet SHA256SUMS || { echo "the bundle's files changed during the run" >&2; exit 3; }
echo "results: $here/results/summary.json (exit $rc)"
exit "$rc"
"""


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def _meta(pt2: Path) -> dict[str, str]:
    """A ``.pt2``'s metadata, read from the archive without loading it (AOTInductor writes
    it as ``<role>/data/aotinductor/<model>/*.wrapper_metadata.json`` on every OS); ``{}``
    when there is none."""
    with zipfile.ZipFile(pt2) as z:
        names = sorted(n for n in z.namelist() if n.endswith("_metadata.json") and "/data/aotinductor/" in n)
        wrapper = [n for n in names if n.endswith(".wrapper_metadata.json")]
        return json.loads(z.read((wrapper or names)[0])) if names else {}


def check_package(p: Path, role: str, constants) -> None:
    """Refuse (SystemExit) a charspot package that holds weights, is not ``role`` at fp32 of
    I/O ``textbox-1``, binds weights other than the pin's, or leaves a constant unbound
    (``constants``: the count ``build.json`` recorded)."""
    from .textbox import CHARSPOT_PIN as pin, IO

    if weights_inside(p, pin.size):
        raise SystemExit(f"{p}: weights inside the package")
    meta = _meta(p)
    for key, want in (("bunko.engine", ENGINE), ("bunko.role", role), ("bunko.io", IO),
                      ("bunko.weights_sha256", pin.sha256)):
        if meta.get(key) != want:
            raise SystemExit(f"{p}: {key} is {meta.get(key)!r}, not {want!r}")
    if meta.get("bunko.precision", PRECISION) != PRECISION or meta.get("bunko.weights_dtype") is not None:
        raise SystemExit(f"{p}: not an fp32 package bound to the authors' fp32 file")
    bound = json.loads(meta.get("bunko.weights", "{}"))
    if not bound:
        raise SystemExit(f"{p}: no bunko.weights map: its constants are not bound to the authors' {pin.file}")
    if constants != len(bound):
        raise SystemExit(f"{p}: {constants} constants, {len(bound)} in bunko.weights")


def _commit(repo: Path, commit: str) -> str:
    r = subprocess.run(["git", "-C", str(repo), "rev-parse", "--verify", "--quiet", f"{commit}^{{commit}}"],
                       capture_output=True, text=True)
    if r.returncode:
        raise SystemExit(f"{commit}: not a commit in {repo}")
    return r.stdout.strip()


def stage_bundle(out: Path, target: str, crops: Path, expected: Path, commit: str, dest: Path,
                 repo: Path = REPO_ROOT) -> Path:
    """Stage ``dest`` (which must not exist) as the gate bundle of the charspot packages
    built for ``target`` under ``out`` (module docstring)."""
    from .textbox import CHARSPOT_PIN as pin

    dest = Path(dest)
    if dest.exists():
        raise SystemExit(f"{dest}: exists; a bundle is staged once, into a new directory")
    tdir = Path(out) / ENGINE / PRECISION / target
    rec_path = tdir / "build.json"
    if not rec_path.is_file():
        raise SystemExit(f"{rec_path}: no build record")
    rec = json.loads(rec_path.read_text())
    pkgs = {}
    for role in ROLES:
        g = rec.get("graphs", {}).get(role)
        if g is None:
            raise SystemExit(f"{rec_path}: no graph {role}")
        p = tdir / g["file"]
        if not p.is_file():
            raise SystemExit(f"{p}: no such package")
        if g.get("sha256") and _sha256(p) != g["sha256"]:
            raise SystemExit(f"{p}: sha256 differs from {rec_path}")
        check_package(p, role, g.get("constants"))
        pkgs[role] = p
    images = sorted(p for p in Path(crops).iterdir() if p.suffix.lower() in IMAGE_SUFFIXES)
    if not images:
        raise SystemExit(f"no crops in {crops}")
    want = read_expected_chars(Path(expected))
    missing = [p.name for p in images if p.name not in want]
    if missing:
        raise SystemExit(f"{expected}: no expected characters for {len(missing)} crops, first {missing[:3]}")
    sha = _commit(Path(repo), commit)
    built = rec.get("exporter_commit", rec.get("commit"))
    if built not in (None, sha):
        raise SystemExit(f"{rec_path}: built from {built}, not {sha}")
    if rec.get("exporter_dirty"):
        raise SystemExit(f"{rec_path}: built with uncommitted exporter changes; build again from {sha[:12]}")
    if built is None:
        print(f"{rec_path} records no exporter commit; bundling the sources at {sha[:12]} as given",
              file=sys.stderr)
    tar = subprocess.run(["git", "-C", str(repo), "archive", "--format=tar", sha, *SOURCES],
                         capture_output=True, check=True).stdout

    part = dest.with_name(dest.name + ".partial")
    if part.exists():
        shutil.rmtree(part)
    try:
        pdir = part / "package" / ENGINE / PRECISION / target
        pdir.mkdir(parents=True)
        shutil.copy2(rec_path, pdir / "build.json")
        for p in pkgs.values():
            shutil.copy2(p, pdir / p.name)
        (part / "crops").mkdir()
        for p in images:
            # copies, never hard links: a bundle edited in place must not change the crop set
            shutil.copy2(p, part / "crops" / p.name)
        (part / "expected").mkdir()
        shutil.copy2(expected, part / "expected" / EXPECTED)
        with tarfile.open(fileobj=io.BytesIO(tar)) as t:
            t.extractall(part / "src", filter="data")
        (part / "run.sh").write_text(RUN_SH)
        (part / "run.sh").chmod(0o755)
        info = {"engine": ENGINE, "precision": PRECISION, "target": target, "commit": sha,
                "crops": len(images), "batches": list(BATCHES), "device": "cuda",
                "expected": f"expected/{EXPECTED}",
                "weights": {"repo": pin.repo, "revision": pin.revision, "file": pin.file,
                            "sha256": pin.sha256, "size": pin.size},
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


def fetch() -> Path:
    """The authors' repository at the pin, every pinned file checked (SystemExit on any
    difference)."""
    from . import textbox as TB

    snap = TB.charspot_snapshot()
    TB.check_charspot(snap)
    print(f"{TB.CHARSPOT_PIN.repo}@{TB.CHARSPOT_PIN.revision}: {len(TB.CHARSPOT_FILES)} files match their "
          f"sha256 in {snap}", file=sys.stderr)
    return snap


def _verify_process(bundle: Path, info: dict, batch: int, expected: Path) -> dict:
    """One ``verify`` run in a process of its own (a loaded package is never released and
    loaded again in one process; on Windows verify ends its process)."""
    cmd = [sys.executable, "-m", "torch_export", "verify", str(bundle / "package"), ENGINE, PRECISION,
           info["target"], "--device", info.get("device", "cuda"), "--crops", str(bundle / "crops"),
           "--expected", str(expected), "--batch", str(batch)]
    print("+", " ".join(cmd), file=sys.stderr, flush=True)
    r = subprocess.run(cmd, cwd=bundle / "src" / "tools" / "torch_export", stdout=subprocess.PIPE, text=True)
    lines = [line for line in r.stdout.splitlines() if line.startswith("{")]
    if not lines:
        return {"error": f"verify exited {r.returncode} without a result", "passed": False}
    return {**json.loads(lines[-1]), "returncode": r.returncode}


def run_gate(bundle: Path, verify: Callable[[Path, dict, int, Path], dict] = _verify_process) -> dict:
    """Every batch of the bundle against its expected output (module docstring); writes
    and returns the summary."""
    bundle = Path(bundle).resolve()
    info = json.loads((bundle / "bundle.json").read_text())
    results = bundle / "results"
    results.mkdir(exist_ok=True)
    batches = {}
    for b in info["batches"]:
        r = verify(bundle, info, b, bundle / info["expected"])
        # passing: every crop of the bundle read the same, and verify itself said so
        r["passed"] = bool(r.get("passed", True) and r.get("returncode", 0) == 0 and "error" not in r
                           and r.get("crops") == info["crops"] and r.get("identical") == info["crops"])
        batches[str(b)] = r
    summary = {k: info[k] for k in ("engine", "precision", "target", "commit", "crops")}
    summary.update({"batches": batches,
                    "crops_per_s": {b: r.get("crops_per_s") for b, r in batches.items()},
                    "passed": all(r["passed"] for r in batches.values())})
    (results / "summary.json").write_text(json.dumps(summary, indent=1, ensure_ascii=False) + "\n")
    return summary


def notice(commit: str, rows: list[dict]) -> str:
    """The charspot-small section of the release's ``NOTICE.md``: the packages (``rows``:
    the release manifest's charspot rows), the authors' Apache-2.0 code they are compiled
    from, the exporter and its commit, and the weights they bind, which they do not hold."""
    from .textbox import CHARSPOT_FILES as files, CHARSPOT_PIN as pin

    url = f"https://huggingface.co/{pin.repo}"
    pkgs = "\n".join(f"| `{r['file']}` | {r['size']:,} bytes | sha256 `{r['sha256']}` |" for r in rows)
    code = ", ".join(f"`{f}` (sha256 `{files[f]}`)" for f in ("manga_ocr_charspot_small/model.py",
                                                              "manga_ocr_charspot_small/core.py",
                                                              "manga_ocr_charspot_small/backends/cuda.py"))
    return f"""## charspot-small (manga-ocr-charspot-small by muscgab)

Two graphs per GPU target, `dense` (detection heads and embeddings) and `tail` (classes and
reading-order scores), compiled with PyTorch AOTInductor for the libtorch route. **The
packages hold no weights.** Each binds, at load, the tensors of the authors' own
`model.safetensors`, which the program downloads from the authors' repository.

| package | size | |
|---|---|---|
{pkgs}

| | |
|---|---|
| source code | {url}, revision `{pin.revision}`: {code} |
| weights they bind | `{pin.file}` of that revision, {pin.size:,} bytes, sha256 `{pin.sha256}`: {url}/resolve/{pin.revision}/{pin.file} (not included) |
| made with | mokuro-bunko's `tools/torch_export` at commit `{commit}`: `tools/torch_export/torch_export/textbox.py` (`charspot_small`, `DenseGraph`, `TailGraph`) and `tools/torch_export/torch_export/build.py` |

### Licences

- **The packages** are compiled from the authors' model code, licensed under the
  Apache License 2.0 (`LICENSE` in the source repository; the licence's text is this
  release's `LICENSE`), and are offered under the same licence. They were changed from it: the networks are split in two
  graphs, a padded crop is masked so it reads as alone, and the decode between the graphs
  (`core.py`) runs on the host, ported to Rust in mokuro-bunko's `crates/bunko-vlm`.
- **The weights** (`{pin.file}`, not included) are licensed by their authors under
  **CC BY-NC-SA 4.0** (`LICENSE_MODEL` in the source repository): non-commercial use only,
  with attribution, and adaptations under the same licence. Commercial use needs the
  authors' permission. Running the packages means using those weights under that licence.
  See the source repository's `NOTICE.md` for the provenance of the weights and their
  training data.
"""


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="python -m torch_export.charspot_gate", description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("bundle", help="stage one target's gate bundle")
    s.add_argument("--out", required=True, type=Path, help="the build tree (charspot-small/fp32/<target>/build.json)")
    s.add_argument("-t", "--target", required=True)
    s.add_argument("--crops", required=True, type=Path)
    s.add_argument("--expected", required=True, type=Path, help="charspot_reference.py's output over the crops")
    s.add_argument("--commit", required=True, help="the exporter commit the packages were built from")
    s.add_argument("--dest", required=True, type=Path, help="a new directory")
    sub.add_parser("fetch", help="the authors' repository at the pin, every pinned sha256 checked")
    s = sub.add_parser("run", help="the bundle's gate (what run.sh runs); exit 0 = passed")
    s.add_argument("bundle", type=Path)
    s = sub.add_parser("notice", help="print the charspot section of the release NOTICE.md")
    s.add_argument("--commit", required=True)
    s.add_argument("--manifest", required=True, type=Path, help="the release json (textbox-manifest's output)")
    a = ap.parse_args(argv)
    if a.cmd == "bundle":
        stage_bundle(a.out, a.target, a.crops, a.expected, a.commit, a.dest)
        return 0
    if a.cmd == "fetch":
        fetch()
        return 0
    if a.cmd == "run":
        s = run_gate(a.bundle)
        print(json.dumps({k: s[k] for k in ("target", "crops_per_s", "passed")}))
        for b, r in s["batches"].items():
            d = r.get("differ") or []
            detail = r.get("error") or (f"{r.get('identical')} of {r.get('crops')} the same, {len(d)} differ, "
                                        f"first {d[:3]}, largest box move {r.get('largest_move_px')} px")
            print(f"batch {b:>3} {'pass' if r['passed'] else 'FAIL'}  {detail}")
        return 0 if s["passed"] else 1
    rows = [r for r in json.loads(a.manifest.read_text())["files"] if r["file"].startswith(f"{ENGINE}_")]
    if not rows:
        raise SystemExit(f"{a.manifest}: no {ENGINE} rows")
    print(notice(a.commit, rows), end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
