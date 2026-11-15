"""The AnimeText detector's builds, gate bundles and release stage (plan 04 Task 10).

    python -m torch_export detector-check [--out BUILD] [-t T1,T2]
                                    every target's build record and package (below)
    python -m torch_export detector-pages --list FILE [--odd DIR] [--min N] SRC...
                                    the gate's page list, written once (SRC: page
                                    directories or list files; --odd: derived odd canvases)
    python -m torch_export detector-gate-bundle [--out BUILD] -t TARGET --pages LIST --onnx FILE --dest DIR
    python -m torch_export detector-gate-run BUNDLE   (what the bundle's run.sh calls)
    python -m torch_export detector-stage [--out BUILD] --stage DIR --passed T1,T2 [--verified T=CARD ...]
                                    [--licence-file FILE]

``detector-check`` reads ``BUILD/animetext-blocks/fp32/<target>/`` for each target of
``TARGETS`` (decision L8; ``-t`` names fewer) and refuses a build record whose package
changed since, a package with its weights inside, metadata other than the ONNX route's
(``bunko.io = detect-1``, ``bunko.weights_format = onnx``, the pin's sha256 and size,
``bunko.ultralytics = ULTRALYTICS_VERSION``, ``bunko.max_batch``), a constant without an
ONNX initializer, a constant count other than the record's, a package built from a
modified tree, and targets built from different exporter commits.

``detector-gate-bundle`` stages one self-contained directory per target for a test
machine: the package and its ``build.json`` under ``package/animetext-blocks/fp32/<target>/``,
the gate's pages (copied, renamed ``pages/NNNN.<ext>`` in list order) with
``pages.txt``, the pinned ONNX file under ``onnx/``, the exporter's sources at the
packages' commit (``git archive`` of tools/torch_export and tools/onnx_export),
``run.sh``, ``bundle.json`` and a ``SHA256SUMS`` over all of it.

``detector-gate-run`` runs ``verify`` (the page gate: identical block sets, corners
within 0.01 page px, no crop edge moved) and times the package at batch 1, 4 and 8 on
one canvas (``--bench-batches``), then writes ``results/summary.json``. Exit 0 only on
a pass.

``detector-stage`` writes the ``detector-torch-v1`` stage (flat assets, the manifest,
``NOTICE.md``, the AGPL-3.0 text as ``LICENSE``) over the targets that may ship
(``ship_targets``: decision L1 and L2): every target whose gate passed (``--passed``),
and each target without a test card once every verified neighbour of it passed. A
target with a test card whose gate did not pass is left out, so that GPU detects on the
CPU. ``publish_detector.sh STAGE`` then checks the stage and prints the command that
publishes it; this module uploads nothing.
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path
from typing import Callable, Iterable, Mapping

from . import detector as D

REPO_ROOT = Path(__file__).resolve().parents[3]
ENGINE = "animetext-blocks"
PRECISION = "fp32"
RELEASE = "detector-torch-v1"
LICENCE = "AGPL-3.0"
SOURCES = ("tools/torch_export", "tools/onnx_export")
MIN_PAGES = 300
BENCH_BATCHES = (1, 4, 8)

# Decision L8: the targets every detector release is built for.
TARGETS = ("linux-rocm-gfx1030", "linux-rocm-gfx1100", "linux-rocm-gfx1101", "linux-rocm-gfx1102",
           "linux-rocm-gfx1200", "linux-rocm-gfx1201", "linux-cuda-sm_75", "linux-cuda-sm_80",
           "windows-cuda-sm_75", "windows-cuda-sm_80")
# Decision L2: a target with no test card ships only once every verified neighbour of it
# passed, from the same exporter commit. The targets not listed here have a test card.
NEIGHBOURS: dict[str, tuple[str, ...]] = {
    **{f"linux-rocm-{g}": ("linux-rocm-gfx1201", "linux-rocm-gfx1030") for g in ("gfx1100", "gfx1101", "gfx1102", "gfx1200")},
    "linux-cuda-sm_75": ("linux-cuda-sm_80",),
    "windows-cuda-sm_75": ("windows-cuda-sm_80",),
}

RUN_SH = """#!/usr/bin/env bash
# The AnimeText page gate of one detector package on this machine (plan 04 Task 10): every
# page of pages.txt through the package on this GPU and through ONNX Runtime on the CPU,
# one page a call; then the package timed at batch 1, 4 and 8 on one canvas.
#
#   PYTHON=<a python with torch for this target, numpy, pillow, onnx and onnxruntime> ./run.sh
#
# Run it only while the GPU is idle, under the lock:  flock -w 3600 ~/bench-gpu.lock ./run.sh
# Every file is checked against SHA256SUMS before the run and again after it.
# Exit 0: the gate passed; 1: it failed (results/summary.json names the pages); other: the
# bundle or the run is broken.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here"
sha256sum -c --quiet SHA256SUMS || { echo "the bundle's files differ from SHA256SUMS: not run" >&2; exit 3; }
rc=0
(cd src/tools/torch_export && "${PYTHON:-python3}" -m torch_export detector-gate-run "$here") || rc=$?
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


def package_meta(pt2: Path) -> dict:
    """The metadata a ``.pt2`` carries (its one ``*.wrapper_metadata.json``)."""
    try:
        with zipfile.ZipFile(pt2) as z:
            names = [n for n in z.namelist() if n.endswith(".wrapper_metadata.json")]
            if len(names) != 1:
                raise SystemExit(f"{pt2}: {len(names)} metadata files")
            return json.loads(z.read(names[0]))
    except zipfile.BadZipFile:
        raise SystemExit(f"{pt2}: not a package (no zip)") from None


def check_build(out: Path, target: str, pin: D.OnnxPin = D.ANIMETEXT_PIN) -> dict:
    """One target's detector build, checked (module docstring):
    ``{"target", "dir", "file", "sha256", "size", "constants", "commit", "ultralytics", "torch"}``."""
    from .verify import package_constants, weights_inside

    if target not in TARGETS:
        raise SystemExit(f"{target}: not a detector target (decision L8: {', '.join(TARGETS)})")
    tdir = Path(out) / ENGINE / PRECISION / target
    rec_path = tdir / "build.json"
    if not rec_path.is_file():
        raise SystemExit(f"{rec_path}: no build record")
    rec = json.loads(rec_path.read_text())
    for field, want in (("engine", ENGINE), ("precision", PRECISION), ("target", target)):
        if rec.get(field) != want:
            raise SystemExit(f"{rec_path}: {field} {rec.get(field)!r}, not {want!r}")
    graphs = rec.get("graphs", {})
    if set(graphs) != {"detector"}:
        raise SystemExit(f"{rec_path}: graphs {sorted(graphs)}, the detector has one (detector)")
    g = graphs["detector"]
    p = tdir / g.get("file", "")
    if g.get("file") != "detector.pt2" or not p.is_file():
        raise SystemExit(f"{rec_path}: no detector.pt2 beside it")
    digest = _sha256(p)
    if g.get("sha256") != digest:
        raise SystemExit(f"{p}: sha256 differs from {rec_path} (rebuilt without a new record?)")
    if weights_inside(p, pin.size):
        raise SystemExit(f"{p}: weights inside the package")
    meta = package_meta(p)
    want = {"bunko.io": D.IO, "bunko.weights_format": "onnx", "bunko.weights_sha256": pin.sha256,
            "bunko.weights_size": str(pin.size), "bunko.ultralytics": D.ULTRALYTICS_VERSION,
            "bunko.max_batch": str(D.MAX_BATCH), "bunko.engine": ENGINE, "bunko.precision": PRECISION,
            "bunko.target": target}
    for key, value in want.items():
        if meta.get(key) != value:
            raise SystemExit(f"{p}: {key} is {meta.get(key)!r}, not {value!r}")
    bound = json.loads(meta.get("bunko.weights") or "{}")
    fqns = package_constants(p)
    unbound = sorted(set(fqns) - set(bound))
    if not fqns or unbound:
        raise SystemExit(f"{p}: constants without an ONNX initializer: {unbound[:5]} ({len(unbound)} of {len(fqns)})")
    if g.get("constants") != len(fqns):
        raise SystemExit(f"{p}: {len(fqns)} constants, {g.get('constants')} in {rec_path}")
    commit = meta.get("bunko.exporter_commit", "")
    if not commit or commit == "unknown" or commit.endswith("-dirty") or rec.get("exporter_dirty", True):
        raise SystemExit(f"{p}: built from a modified or unknown tree (exporter commit {commit!r}); "
                         "a release is built from a committed, clean tools/torch_export")
    if rec.get("exporter_commit") != commit:
        raise SystemExit(f"{p}: exporter commit {commit}, {rec.get('exporter_commit')!r} in {rec_path}")
    return {"target": target, "dir": tdir, "file": p, "sha256": digest, "size": p.stat().st_size,
            "constants": len(fqns), "commit": commit, "ultralytics": meta["bunko.ultralytics"],
            "torch": meta.get("bunko.torch", rec.get("torch", ""))}


def check_builds(out: Path, targets: Iterable[str], pin: D.OnnxPin = D.ANIMETEXT_PIN) -> tuple[str, list[dict]]:
    """``check_build`` for each target; all from one exporter commit. Returns (commit, builds)."""
    builds = [check_build(out, t, pin) for t in targets]
    if not builds:
        raise SystemExit("no detector targets")
    commits = sorted({b["commit"] for b in builds})
    if len(commits) != 1:
        raise SystemExit(f"the targets come from {len(commits)} exporter commits {commits}: "
                         + ", ".join(f"{b['target']} {b['commit'][:12]}" for b in builds)
                         + "; one release is built from one exporter commit")
    return commits[0], builds


def ship_targets(passed: Iterable[str], built: Iterable[str]) -> list[str]:
    """The built targets that may ship (decisions L1, L2): those whose gate passed, and
    each target without a test card once every verified neighbour of it passed. A target
    with a test card whose gate did not pass is left out."""
    passed, built = set(passed), list(built)
    for t in passed | set(built):
        if t not in TARGETS:
            raise SystemExit(f"{t}: not a detector target (decision L8: {', '.join(TARGETS)})")
    absent = sorted(passed - set(built))
    if absent:
        raise SystemExit(f"passed but not built: {absent}")
    return [t for t in TARGETS if t in built and (t in passed or (t in NEIGHBOURS and set(NEIGHBOURS[t]) <= passed))]


def write_page_list(sources: Iterable[Path], dest: Path, minimum: int = MIN_PAGES) -> list[Path]:
    """The gate's page list: every page of each source (a directory or a list file, as
    ``verify.page_files`` reads them), absolute, without repeats, in source order.
    Refuses fewer than ``minimum`` pages. Returns the pages."""
    from .verify import page_files

    seen, files = set(), []
    for src in sources:
        for p in page_files(Path(src)):
            p = p.resolve()
            if p not in seen:
                seen.add(p)
                files.append(p)
    if len(files) < minimum:
        raise SystemExit(f"{len(files)} pages; the gate needs at least {minimum}")
    dest = Path(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(f"# the AnimeText gate's {len(files)} pages (plan 04 Task 10), one a line\n"
                    + "".join(f"{p}\n" for p in files), encoding="utf-8")
    return files


def odd_pages(sources: list[Path], dest: Path) -> list[Path]:
    """Odd canvases from the first two pages of ``sources``: a two-page spread, a wide
    strip, a tall column, scraps of 3 x 2 and 31 x 17 px, a page at twice its size,
    pages of exactly 640 x 640 and of 641 x 639 px. PNG, named by what they are."""
    from PIL import Image

    if len(sources) < 2:
        raise SystemExit("odd canvases need two source pages")
    dest = Path(dest)
    dest.mkdir(parents=True, exist_ok=True)
    with Image.open(sources[0]) as a0, Image.open(sources[1]) as b0:
        a, b = a0.convert("RGB"), b0.convert("RGB")
    h = min(a.height, b.height)
    a2 = a.resize((max(1, round(a.width * h / a.height)), h), Image.BILINEAR)
    b2 = b.resize((max(1, round(b.width * h / b.height)), h), Image.BILINEAR)
    spread = Image.new("RGB", (a2.width + b2.width, h), (255, 255, 255))
    spread.paste(b2, (0, 0))  # right-to-left: the second page on the left
    spread.paste(a2, (b2.width, 0))
    made = {
        "spread": spread,
        "strip": a.crop((0, 0, a.width, max(1, a.height // 6))),
        "column": a.crop((0, 0, max(1, a.width // 8), a.height)),
        "scrap-3x2": a.resize((3, 2), Image.BILINEAR),
        "scrap-31x17": b.resize((31, 17), Image.BILINEAR),
        "double": b.resize((b.width * 2, b.height * 2), Image.BILINEAR),
        "square-640": a.resize((640, 640), Image.BILINEAR),
        "odd-641x639": b.resize((641, 639), Image.BILINEAR),
    }
    out = []
    for name, im in made.items():
        p = dest / f"odd-{name}.png"
        im.save(p)
        out.append(p)
    return out


def _commit(repo: Path, commit: str) -> str:
    r = subprocess.run(["git", "-C", str(repo), "rev-parse", "--verify", "--quiet", f"{commit}^{{commit}}"],
                       capture_output=True, text=True)
    if r.returncode:
        raise SystemExit(f"{commit}: not a commit in {repo}")
    return r.stdout.strip()


def stage_bundle(out: Path, target: str, pages: Path, onnx: Path, dest: Path, repo: Path = REPO_ROOT,
                 pin: D.OnnxPin = D.ANIMETEXT_PIN) -> Path:
    """Stage ``dest`` (which must not exist) as the gate bundle of one built target (module
    docstring). Everything is checked before anything is written: the build
    (``check_build``), the ONNX file's pin, every listed page, and the packages' commit
    in ``repo``."""
    from .verify import page_files

    dest = Path(dest)
    if dest.exists():
        raise SystemExit(f"{dest}: exists; a bundle is staged once, into a new directory")
    b = check_build(out, target, pin)
    files = page_files(Path(pages))
    D.check_pin(Path(onnx), pin)
    sha = _commit(repo, b["commit"])
    tar = subprocess.run(["git", "-C", str(repo), "archive", "--format=tar", sha, *SOURCES],
                         capture_output=True, check=True).stdout

    part = dest.with_name(dest.name + ".partial")
    if part.exists():
        shutil.rmtree(part)
    try:
        pdir = part / "package" / ENGINE / PRECISION / target
        pdir.mkdir(parents=True)
        shutil.copy2(b["dir"] / "build.json", pdir / "build.json")
        shutil.copy2(b["file"], pdir / "detector.pt2")
        (part / "pages").mkdir()
        listed = []
        for i, p in enumerate(files, 1):
            # copies, never hard links: a bundle edited in place must not change the page set
            name = f"pages/{i:04d}{p.suffix.lower()}"
            shutil.copy2(p, part / name)
            listed.append(name)
        (part / "pages.txt").write_text("".join(f"{n}\n" for n in listed), encoding="utf-8")
        (part / "onnx").mkdir()
        shutil.copy2(onnx, part / "onnx" / pin.file)
        with tarfile.open(fileobj=io.BytesIO(tar)) as t:
            t.extractall(part / "src", filter="data")
        (part / "run.sh").write_text(RUN_SH)
        (part / "run.sh").chmod(0o755)
        info = {"engine": ENGINE, "precision": PRECISION, "target": target, "commit": sha, "pages": len(listed),
                "page_list": "pages.txt", "onnx": f"onnx/{pin.file}", "onnx_sha256": pin.sha256,
                "device": "cuda", "bench_batches": list(BENCH_BATCHES),
                "package": {"file": "detector.pt2", "sha256": b["sha256"], "constants": b["constants"]}}
        (part / "bundle.json").write_text(json.dumps(info, indent=1) + "\n")
        sums = sorted(p for p in part.rglob("*") if p.is_file())
        (part / "SHA256SUMS").write_text("".join(f"{_sha256(p)}  {p.relative_to(part).as_posix()}\n" for p in sums))
        part.rename(dest)
    except BaseException:
        shutil.rmtree(part, ignore_errors=True)
        raise
    print(f"staged {dest}: {target}, {len(listed)} pages, {len(sums) + 1} files, commit {sha[:12]}", file=sys.stderr)
    return dest


def _verify_process(bundle: Path, info: dict) -> dict:
    """``verify`` on the bundle in a process of its own (on Windows verify ends its
    process); its last JSON line plus ``returncode``."""
    cmd = [sys.executable, "-m", "torch_export", "verify", str(bundle / "package"), info["engine"], info["precision"],
           info["target"], "--device", info.get("device", "cuda"), "--pages", str(bundle / info["page_list"]),
           "--onnx", str(bundle / info["onnx"]),
           "--bench-batches", ",".join(str(b) for b in info.get("bench_batches", BENCH_BATCHES))]
    print("+", " ".join(cmd), file=sys.stderr, flush=True)
    r = subprocess.run(cmd, cwd=bundle / "src" / "tools" / "torch_export", stdout=subprocess.PIPE, text=True)
    lines = [line for line in r.stdout.splitlines() if line.startswith("{")]
    if not lines:
        return {"error": f"verify exited {r.returncode} without a result", "returncode": r.returncode}
    return {**json.loads(lines[-1]), "returncode": r.returncode}


def run_gate(bundle: Path, verify: Callable[[Path, dict], dict] = _verify_process) -> dict:
    """The bundle's gate (module docstring); writes and returns ``results/summary.json``."""
    from .verify import detector_passes

    bundle = Path(bundle).resolve()
    info = json.loads((bundle / "bundle.json").read_text())
    results = bundle / "results"
    results.mkdir(exist_ok=True)
    r = verify(bundle, info)
    keys = ("pages", "identical_sets", "worst_corner", "edges_moved", "blocks")
    ok = "error" not in r and all(k in r for k in keys)
    passed = ok and r.get("returncode") == 0 and r["pages"] == info["pages"] and detector_passes(r)
    summary = {k: info[k] for k in ("engine", "precision", "target", "commit")}
    summary.update({k: r.get(k) for k in keys})
    summary.update({"differ_pages": r.get("differ_pages", []), "first_differ": (r.get("differ") or [None])[0],
                    "pages_per_s": r.get("pages_per_s"), "package_ms_per_page": r.get("package_ms_per_page"),
                    "passed": bool(passed)})
    if not ok:
        summary["error"] = r.get("error", f"verify gave no result (exit {r.get('returncode')})")
    elif r["pages"] != info["pages"]:
        summary["error"] = f"verify read {r['pages']} pages, the bundle has {info['pages']}"
    (results / "summary.json").write_text(json.dumps(summary, indent=1, ensure_ascii=False) + "\n")
    return summary


def agpl_text(licence_file: Path | None = None, search: Iterable[str] | None = None) -> str:
    """The AGPL-3.0 text: ``licence_file``, else the one ultralytics ``ULTRALYTICS_VERSION``
    ships in its dist-info (``TORCH_EXPORT_DETECTOR_PYLIB``, then ``sys.path``)."""
    if licence_file is None:
        dirs = [os.environ.get(D.PYLIB_VAR, "")] + list(sys.path if search is None else search)
        info = f"ultralytics-{D.ULTRALYTICS_VERSION}.dist-info"
        found = [Path(d) / info / sub for d in dirs if d for sub in ("licenses/LICENSE", "LICENSE")]
        found = [p for p in found if p.is_file()]
        if not found:
            raise SystemExit(f"no {info} on the path: --licence-file the AGPL-3.0 text, or set {D.PYLIB_VAR}")
        licence_file = found[0]
    text = Path(licence_file).read_text(encoding="utf-8")
    if "GNU AFFERO GENERAL PUBLIC LICENSE" not in text or "Version 3" not in text:
        raise SystemExit(f"{licence_file}: not the AGPL-3.0 text")
    return text


def notice(commit: str, builds: list[dict], verified: Mapping[str, str], pin: D.OnnxPin = D.ANIMETEXT_PIN) -> str:
    """``NOTICE.md`` of the release: what it holds, where every part comes from, which
    targets were verified on which card, and the licences."""
    torch = sorted({b["torch"].split("+")[0] for b in builds if b["torch"]})
    rows = []
    for b in builds:
        t = b["target"]
        how = (f"verified on {verified[t]}" if t in verified else
               "built from the same commit, no test card; its verified neighbours "
               + " and ".join(NEIGHBOURS.get(t, ())) + " passed")
        rows.append(f"| `{t}` | `{RELEASE_NAME.format(target=t)}` | {how} |")
    return f"""# {RELEASE}: AnimeText on a GPU

The AnimeText text-block detector (YOLO12-x, one class) compiled ahead of time with
AOTInductor (torch {", ".join(torch) or "2.13.0"}), one weightless `detector.pt2` per GPU target,
fp32. A pre-release, used only by mokuro-bunko builds with the `gpl-models` feature: a
GPU without a package here detects on the CPU through ONNX Runtime, with the same blocks.

| target | asset | gate |
|---|---|---|
{chr(10).join(rows)}

The gate, on every verified target: the same block set as ONNX Runtime on the CPU on
every page of at least {MIN_PAGES}, every corner within 0.01 page px, no crop edge moved.

## Where it comes from

- The graph is ultralytics {D.ULTRALYTICS_VERSION}'s `DetectionModel` ("yolo12x.yaml", one class),
  fused, with the detect head in export mode (raw candidates, no NMS). It is built by
  `tools/torch_export/torch_export/detector.py` (with `build.py` and `verify.py`) at
  commit {commit} of https://github.com/Gnathonic/mokuro-bunko.
- ultralytics {D.ULTRALYTICS_VERSION}: the ONNX file was exported by 8.3.105, but under 8.3.105
  the fused convolution weights are non-leaf tensors and the AOTInductor compile stops;
  {D.ULTRALYTICS_VERSION} rebuilds the same graph, and every package constant binds an initializer
  of the ONNX file under its own name.
- One change to Ultralytics code: `A2C2f.forward` is patched (`detector.py`,
  `_a2c2f_forward`) to keep each residual scale `gamma` as `[1, C, 1, 1]`, the shape
  the ONNX export folded it into, so those initializers bind byte for byte. The
  arithmetic is unchanged.
- The packages hold no weights. At load they bind the initializers of
  `{pin.file}` ({pin.size:,} bytes, sha256 `{pin.sha256}`), the
  authors' `{D.ANIMETEXT_REPO_PATH}` from {pin.repo}@{pin.revision}, mirrored in the
  `upstream-models-v1` release.

## Licences

- The packages are compiled Ultralytics model code: AGPL-3.0 (`LICENSE`). The
  corresponding source is the commit above together with ultralytics {D.ULTRALYTICS_VERSION} as published on PyPI.
- The weights they bind are not in this release; they keep their authors' licence
  (GPL-3.0), stated with the ONNX file.
"""


# the flat asset name build_textbox_manifest gives a target's package
RELEASE_NAME = f"{ENGINE}_{PRECISION}_{{target}}_detector.pt2"


def stage_release(out: Path, stage: Path, passed: Iterable[str], verified: Mapping[str, str] | None = None,
                  licence_file: Path | None = None, base_url: str | None = None,
                  pin: D.OnnxPin = D.ANIMETEXT_PIN) -> dict:
    """Write the ``detector-torch-v1`` stage (module docstring) into ``stage`` (new or
    empty): the packages of ``ship_targets(passed, built)`` under their flat names, the
    manifest, ``NOTICE.md`` and ``LICENSE``. ``verified`` maps a passed target to the
    card it passed on (default: "a test card"). Returns the manifest."""
    from .manifest import build_textbox_manifest

    out, stage = Path(out), Path(stage)
    if stage.exists() and any(stage.iterdir()):
        raise SystemExit(f"{stage}: not empty; a release is staged once, into a new directory")
    passed = list(passed)
    built = [t for t in TARGETS if (out / ENGINE / PRECISION / t / "build.json").is_file()]
    ship = ship_targets(passed, built)
    if not ship:
        raise SystemExit(f"no target may ship: passed {sorted(passed)}, built {built}")
    left = [t for t in built if t not in ship]
    if left:
        print(f"left out (L1/L2): {left}", file=sys.stderr)
    commit, builds = check_builds(out, ship, pin)
    verified = {t: (verified or {}).get(t, "a test card") for t in passed}
    licence = agpl_text(licence_file)

    # build_textbox_manifest takes every record under its tree: give it the shipping ones
    sel = stage.with_name(stage.name + ".select")
    if sel.exists():
        shutil.rmtree(sel)
    try:
        for b in builds:
            d = sel / ENGINE / PRECISION / b["target"]
            d.mkdir(parents=True)
            for name in ("build.json", "detector.pt2"):
                try:
                    os.link(b["dir"] / name, d / name)
                except OSError:
                    shutil.copy2(b["dir"] / name, d / name)
        m = build_textbox_manifest(sel, RELEASE, (ENGINE,), {ENGINE: pin}, LICENCE, base_url, stage)
    finally:
        shutil.rmtree(sel, ignore_errors=True)
    (stage / "NOTICE.md").write_text(notice(commit, builds, verified, pin), encoding="utf-8")
    (stage / "LICENSE").write_text(licence, encoding="utf-8")
    print(f"staged {stage}: {len(m['files'])} packages ({', '.join(ship)}), commit {commit[:12]}", file=sys.stderr)
    return m
