"""python -m torch_export <command>

  targets                         list the compile targets
  build    -e ENGINE -p PREC -t TARGET [--out DIR] [--io 1|2] [--no-precast] [--host]
                                  one engine x precision x target. Linux targets run in
                                  the glibc-2.28 build container (docker; --host: here);
                                  Windows/macOS targets in this interpreter on that OS
  matrix   -e E1,E2 -p P1,P2 -t T1,T2 [--out DIR] [--force]
                                  every combination, each in a fresh process (container, or
                                  the venv its target needs: $TORCH_EXPORT_WORK/venv-<variant>
                                  or $TORCH_EXPORT_VENV_<VARIANT>); skips finished ones
  manifest [--out DIR] [--tag TAG] [--base-url URL] [--flat DIR]
                                  torch-models.json over everything under --out
  textbox-manifest -e E1,E2 [--out DIR] [--tag TAG] [--licence L] [--base-url URL] [--flat DIR]
                                  <tag>.json of the packages that bind their authors'
                                  weights (textbox-torch-v1): rows of those engines only
  verify OUT ENGINE PREC TARGET [...]
                                  python -m torch_export.verify (text-box engines:
                                  --crops DIR --expected FILE, the crop-level gate)
  gate-bundle --out BUILD -e ENGINE -p PREC -t TARGET --crops DIR --reference FILE --fp32 FILE
              --commit C --dest DIR
                                  one target's self-contained crop-gate bundle (package, crops,
                                  expected files, these sources at C, run.sh, SHA256SUMS; gate.py)
  gate-run BUNDLE                 the bundle's checks (what its run.sh runs); exit 0 = passed
  compare-reads A B [--limit L]   differing texts of two reads files; exit 0 within floor(L x n)
  block-preds READS BLOCKS OUT    crop texts joined per page key: paired.py's {key, pred} lines
  refuse-embedded DIR...          exit 1 on the first .pt2 with its weights inside
                                  (*_embed.pt2, or an entry > pinned weights / 4)
  fetch-windows-cross             download the pinned Windows wheels + llvm-mingw used to
                                  cross-compile windows-cuda-* targets on Linux
  check-isa PATH...               AVX-512 (zmm) instruction count of each .pt2's
                                  native code (0 = runs on any x86-64-v3 host)

Environment: TORCH_EXPORT_WORK (default ~/.cache/mokuro-bunko-demo/tmp/torch-export:
tmp/, inductor-cache/, cuda-home/, out/), MOKURO_REF_052 (0.5.2 checkout; pins are
checked against it), HF_HUB_OFFLINE (default 1: models come from the HF cache),
TORCH_EXPORT_CPU_EMBED=1 (CPU targets: the old frozen layout, weights inside the package),
TORCH_EXPORT_WIN_EMBED=1 (cross-built Windows GPU targets: weights inside),
TORCH_EXPORT_OBJDUMP (the GNU objdump the checks run; default objdump).
"""

from __future__ import annotations

import argparse
import itertools
import json
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ONNX_EXPORT = HERE.parents[1] / "onnx_export"


def _imports() -> None:
    sys.path.insert(0, str(ONNX_EXPORT))


def cmd_targets(_a) -> int:
    from .targets import TARGETS

    for t in TARGETS.values():
        print(f"{t.name:28} {t.backend:5} torch {t.torch_variant:8} {','.join(t.precisions)}")
    return 0


def _containerized(t, a) -> bool:
    return t.os == "linux" and not getattr(a, "host", False) and os.environ.get("TORCH_EXPORT_IN_CONTAINER") != "1"


def _build_argv(e: str, p: str, tn: str, a) -> list[str]:
    argv = ["build", "-e", e, "-p", p, "-t", tn, "--out", str(a.out), "--io", str(a.io)]
    if getattr(a, "no_precast", False):
        argv.append("--no-precast")
    return argv


def cmd_build(a) -> int:
    from .targets import get, host_os

    t = get(a.target)
    if _containerized(t, a):
        from . import container
        from .toolchain import WORK

        return container.run_build(_build_argv(a.engine, a.precision, a.target, a), t.torch_variant, Path(a.out), WORK, t.backend)
    cross = t.os == "windows" and host_os() == "linux" and t.gpu
    if t.os != host_os() and not cross:
        raise SystemExit(f"{t.name} must be built on {t.os} (this is {host_os()})")
    from . import toolchain

    toolchain.prepare(t)
    _imports()
    from onnx_export.common import REPO_ROOT
    from onnx_export.pins import check_pins

    check_pins(REPO_ROOT)
    from .build import build

    rec = build(a.engine, a.precision, t, Path(a.out), io=a.io, precast=not a.no_precast)
    print(json.dumps({k: rec[k] for k in ("engine", "precision", "target", "total_s")}))
    return 0


def _venv_python(variant: str) -> str:
    from .toolchain import WORK

    v = os.environ.get(f"TORCH_EXPORT_VENV_{variant.replace('.', '').upper()}", str(WORK / f"venv-{variant}"))
    exe = Path(v) / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python")
    if not exe.exists():
        raise SystemExit(f"no venv for {variant} at {v}: uv venv --python 3.12 {v} && "
                         f"VIRTUAL_ENV={v} uv pip install --index-strategy unsafe-best-match -r {HERE.parent}/requirements-{variant}.txt")
    return str(exe)


def cmd_matrix(a) -> int:
    from .targets import get

    out = Path(a.out)
    fails = []
    for e, p, tn in itertools.product(a.engine.split(","), a.precision.split(","), a.target.split(",")):
        t = get(tn)
        if p not in t.precisions:
            continue
        done = out / e / p / tn / "build.json"
        if done.exists() and not a.force:
            print(f"skip {e} {p} {tn} (built)", flush=True)
            continue
        if _containerized(t, a):
            from . import container
            from .toolchain import WORK

            rc = container.run_build(_build_argv(e, p, tn, a), t.torch_variant, out, WORK, t.backend)
        else:
            cmd = [_venv_python(t.torch_variant), "-m", "torch_export", *_build_argv(e, p, tn, a), "--host"]
            print("+", " ".join(cmd), flush=True)
            rc = subprocess.run(cmd, cwd=HERE.parent).returncode
        r = subprocess.CompletedProcess([], rc)
        if r.returncode:
            fails.append((e, p, tn))
    if fails:
        print("FAILED:", fails, file=sys.stderr)
    return 1 if fails else 0


def cmd_fetch_windows_cross(_a) -> int:
    from .toolchain import fetch_windows_cross

    fetch_windows_cross()
    return 0


def cmd_manifest(a) -> int:
    from .manifest import build_manifest

    build_manifest(Path(a.out), a.tag, a.base_url, Path(a.flat) if a.flat else None)
    return 0


def cmd_textbox_manifest(a) -> int:
    from .manifest import build_textbox_manifest
    from .textbox import PINS

    build_textbox_manifest(Path(a.out), a.tag, tuple(a.engine.split(",")), PINS, a.licence, a.base_url,
                           Path(a.flat) if a.flat else None)
    return 0


def cmd_refuse_embedded(a) -> int:
    from .verify import pinned_size, refuse_embedded

    for d in a.dirs:
        n = refuse_embedded(Path(d), pinned_size)
        print(f"{d}: {n} packages, none with weights inside")
    return 0


def cmd_gate_bundle(a) -> int:
    from .gate import stage_bundle

    stage_bundle(Path(a.out), a.engine, a.precision, a.target, Path(a.crops), Path(a.reference), Path(a.fp32),
                 a.commit, Path(a.dest))
    return 0


def cmd_gate_run(a) -> int:
    from .gate import run_gate

    s = run_gate(Path(a.bundle))
    print(json.dumps({k: s[k] for k in ("target", "crops_per_s", "passed")}))
    for name, c in [("reference", s["checks"]["reference"]), ("fp32", s["checks"]["fp32"]),
                    *s["checks"]["batches"].items()]:
        d, n = c.get("differ") or [], c.get("crops")
        detail = c.get("error") or f"{len(d)} of {n} differ, first {d[:3]}"
        print(f"{name:10} {'pass' if c['passed'] else 'FAIL'}  {detail}")
    return 0 if s["passed"] else 1


def cmd_compare_reads(a) -> int:
    from .gate import compare_reads

    r = compare_reads(Path(a.a), Path(a.b), a.limit)
    print(json.dumps(r, ensure_ascii=False))
    return 0 if r["passed"] else 1


def cmd_block_preds(a) -> int:
    from .gate import block_preds

    n = block_preds(Path(a.reads), Path(a.blocks), Path(a.out))
    print(f"wrote {a.out}: {n} keys", file=sys.stderr)
    return 0


def cmd_check_isa(a) -> int:
    from .checks import zmm_count

    rc = 0
    for p in a.paths:
        for f, n in zmm_count(Path(p)).items():
            print(f"{n:8} zmm  {f}")
            rc |= n > 0
    return rc


def main(argv=None) -> int:
    from . import IO_VERSION
    from .toolchain import WORK

    argv = sys.argv[1:] if argv is None else list(argv)
    if argv[:1] == ["verify"]:
        from .verify import main as verify_main

        return verify_main(argv[1:])
    ap = argparse.ArgumentParser(prog="torch_export", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("targets").set_defaults(fn=cmd_targets)
    for name, fn in (("build", cmd_build), ("matrix", cmd_matrix)):
        s = sub.add_parser(name)
        s.add_argument("-e", "--engine", required=True)
        s.add_argument("-p", "--precision", required=True)
        s.add_argument("-t", "--target", required=True)
        s.add_argument("--out", default=str(WORK / "out"))
        s.add_argument("--io", type=int, default=IO_VERSION, choices=(1, 2))
        s.add_argument("--host", action="store_true",
                       help="Linux targets: compile with this host's toolchain instead of the glibc-2.28 container "
                            "(packages then need this host's glibc; the portability check fails them)")
        s.add_argument("--no-precast", action="store_true", help="hayai: keep fp32 weights under autocast (shootout)")
        if name == "matrix":
            s.add_argument("--force", action="store_true")
        s.set_defaults(fn=fn)
    s = sub.add_parser("manifest")
    s.add_argument("--out", default=str(WORK / "out"))
    s.add_argument("--tag", default="torch-models-v1")
    s.add_argument("--base-url", default=None)
    s.add_argument("--flat", default=None, help="also hard-link every asset under its flat release name into DIR")
    s.set_defaults(fn=cmd_manifest)
    s = sub.add_parser("textbox-manifest")
    s.add_argument("-e", "--engine", required=True, help="comma-separated, e.g. nar-small,charspot-small")
    s.add_argument("--out", default=str(WORK / "out"))
    s.add_argument("--tag", default="textbox-torch-v1")
    s.add_argument("--licence", default="Apache-2.0")
    s.add_argument("--base-url", default=None)
    s.add_argument("--flat", default=None, help="also hard-link every asset under its flat release name into DIR")
    s.set_defaults(fn=cmd_textbox_manifest)
    s = sub.add_parser("gate-bundle")
    s.add_argument("--out", required=True, help="the build tree (<engine>/<precision>/<target>/build.json)")
    s.add_argument("-e", "--engine", required=True)
    s.add_argument("-p", "--precision", required=True)
    s.add_argument("-t", "--target", required=True)
    s.add_argument("--crops", required=True)
    s.add_argument("--reference", required=True, help="the authors' output at this precision (check_parity.py)")
    s.add_argument("--fp32", required=True, help="the authors' fp32 output (check_parity.py)")
    s.add_argument("--commit", required=True, help="the exporter commit the package was built from")
    s.add_argument("--dest", required=True, help="a new directory")
    s.set_defaults(fn=cmd_gate_bundle)
    s = sub.add_parser("gate-run")
    s.add_argument("bundle")
    s.set_defaults(fn=cmd_gate_run)
    s = sub.add_parser("compare-reads")
    s.add_argument("a")
    s.add_argument("b")
    s.add_argument("--limit", type=float, default=0.001)
    s.set_defaults(fn=cmd_compare_reads)
    s = sub.add_parser("block-preds")
    s.add_argument("reads")
    s.add_argument("blocks")
    s.add_argument("out")
    s.set_defaults(fn=cmd_block_preds)
    s = sub.add_parser("refuse-embedded")
    s.add_argument("dirs", nargs="+")
    s.set_defaults(fn=cmd_refuse_embedded)
    sub.add_parser("verify", add_help=False)  # listed for --help; main() hands it to verify.main
    sub.add_parser("fetch-windows-cross").set_defaults(fn=cmd_fetch_windows_cross)
    s = sub.add_parser("check-isa")
    s.add_argument("paths", nargs="+")
    s.set_defaults(fn=cmd_check_isa)
    a = ap.parse_args(argv)
    return a.fn(a)


if __name__ == "__main__":
    sys.exit(main())
