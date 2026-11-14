"""Load built packages on THIS machine's device and compare them with eager torch.

    python -m torch_export.verify <out> <engine> <precision> <target> [--device cuda|cpu] [--bench N]

Binds the shared weights exactly as the Rust runtime will (one tensor per weights-file
key on the device, ``load_constants(user_managed=True)`` per package via the
``bunko.weights`` map), runs vision -> prefill -> 8 decode steps on a fixed random batch
and reports, per graph, the max |difference| against the eager modules the packages were
exported from (same precast weights, same autocast). Token ids must match exactly.
``--bench N`` also times N more runs of each package (median/min ms, same inputs) and
reports the process's peak resident memory -- for comparing two builds of one target on
one machine (e.g. weightless vs ``TORCH_EXPORT_CPU_EMBED=1``) where no OCR run is possible.
The crop-level parity gate is the Rust harness (ltbench) on the real crop sets.

Text-box engines (``textbox.PINS``: nar-small) take the crop-level gate here instead:

    python -m torch_export.verify <out> nar-small fp32 <target> --crops DIR --expected FILE [--n N]

binds the authors' pinned ``model.safetensors`` by ``bunko.weights``, feeds every crop
(``--n``: the first N by name) as the authors' ``preprocess.py`` prepares it, 32 at a
time, and compares the ids, up to and including the first end id, with ``--expected``
(the JSON lines ``tools/textbox_export/check_parity.py --expected`` writes). Exit 0 only
when every crop is identical (fp32 has no tolerance). Any package whose constants live
inside it is refused before it is loaded (``weights_inside``), and
``python -m torch_export refuse-embedded DIR`` checks a whole build or staging tree.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import time
import zipfile
from pathlib import Path
from typing import Callable

# A package holds its weights when one entry is bigger than this share of the pinned
# weights file. Measured on gfx1201: a weightless text-box package's wrapper.so is 1-5%
# of the authors' weights (nar fp32 2,385,376 B of 217,035,140 B); the spike's packages
# with weights inside 39-71% (nar fp16 110,900,568 B; charspot dense 31,187,272 B of
# 43,860,012 B).
EMBED_FRACTION = 4
IMAGE_SUFFIXES = (".png", ".jpg", ".jpeg")


def same_ids(got: list[int], want: list[int], end: int) -> bool:
    """Equal up to and including the first ``end`` in ``want`` (what a text is read
    from); ids past it are padding the reader never looks at. No end id: all of them."""
    n = want.index(end) + 1 if end in want else len(want)
    return len(got) >= n and list(got[:n]) == list(want[:n])


def weights_inside(pt2: Path, weights_size: int) -> bool:
    """True when an entry of the ``.pt2`` zip is bigger than ``weights_size //
    EMBED_FRACTION``: constants built into the package instead of bound at load."""
    limit = weights_size // EMBED_FRACTION
    with zipfile.ZipFile(pt2) as z:
        return any(i.file_size > limit for i in z.infolist())


def _engine_of(root: Path, pt2: Path) -> str:
    """``<engine>/<precision>/<target>/<role>.pt2`` under ``root``, or the flat release
    name ``<engine>_<precision>_<target>_<role>.pt2``; "" when neither applies."""
    rel = pt2.relative_to(root).parts
    if len(rel) >= 4:
        return rel[-4]
    if "_" in pt2.name:
        return pt2.name.split("_", 1)[0]
    return ""


def refuse_embedded(root: Path, weights_size_of: Callable[[str], int]) -> int:
    """Refuse (SystemExit) the first ``.pt2`` under ``root`` that carries weights inside:
    the spike's ``*_embed.pt2`` whatever its size, any package ``weights_inside`` flags
    against ``weights_size_of(engine)``, and any file that is not a zip. Returns how
    many packages were checked."""
    root = Path(root)
    if not root.is_dir():
        raise SystemExit(f"{root}: no such directory")
    n = 0
    for p in sorted(root.rglob("*.pt2")):
        if p.name.endswith("_embed.pt2"):
            raise SystemExit(f"{p}: a package built with its weights inside (*_embed.pt2); never staged or uploaded")
        try:
            inside = weights_inside(p, weights_size_of(_engine_of(root, p)))
        except zipfile.BadZipFile:
            raise SystemExit(f"{p}: not a package (no zip), so its constants cannot be checked") from None
        if inside:
            raise SystemExit(f"{p}: weights inside the package")
        n += 1
    return n


def pinned_size(engine: str) -> int:
    """The pinned weights file's size of ``engine``; the smallest pinned size for an
    engine without a pin (the strictest limit)."""
    from .textbox import PINS

    return PINS[engine].size if engine in PINS else min(p.size for p in PINS.values())


def load_weights(pdir: Path, device):
    from safetensors import safe_open

    w = {}
    for f in sorted(pdir.glob("weights-*.safetensors")):
        with safe_open(str(f), "pt") as s:
            for k in s.keys():
                w[k] = s.get_tensor(k).to(device)
    return w


def peak_rss_mb() -> float:
    """Peak resident set (Linux/macOS) or peak working set (Windows) of this process."""
    if sys.platform == "win32":
        import ctypes
        from ctypes import wintypes

        class PMC(ctypes.Structure):
            _fields_ = [("cb", wintypes.DWORD), ("PageFaultCount", wintypes.DWORD),
                        ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
                        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
                        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                        ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t)]

        c = PMC()
        c.cb = ctypes.sizeof(c)
        k32 = ctypes.WinDLL("kernel32")
        k32.GetCurrentProcess.restype = wintypes.HANDLE
        info = ctypes.WinDLL("psapi").GetProcessMemoryInfo
        info.argtypes = [wintypes.HANDLE, ctypes.POINTER(PMC), wintypes.DWORD]
        info.restype = wintypes.BOOL
        if not info(k32.GetCurrentProcess(), ctypes.byref(c), c.cb):
            return -1.0
        return round(c.PeakWorkingSetSize / 2**20, 1)
    import resource

    r = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return round(r / 2**20 if sys.platform == "darwin" else r / 2**10, 1)


def load_pkg(path: Path, weights: dict, device_index: int):
    from torch._inductor import aoti_load_package

    m = aoti_load_package(str(path), device_index=device_index)
    meta = m.loader.get_metadata()
    wm = json.loads(meta.get("bunko.weights", "{}"))
    if wm:
        fqns = m.loader.get_constant_fqns()
        missing = [f for f in fqns if f not in wm]
        if missing:
            raise SystemExit(f"{path}: constants without a weights key: {missing[:5]}")
        m.loader.load_constants({f: weights[wm[f]] for f in fqns}, False, True, True)
    return m, meta


def _verify_textbox(out: Path, engine: str, precision: str, target: str, crops: Path, expected: Path,
                    device: str, n: int | None = None, batch: int = 32):
    """``verify_textbox``'s work; also returns what it holds loaded (package, weights), so
    ``main`` can end the process before releasing it (Windows, see ``_finish``)."""
    import torch
    from PIL import Image
    from safetensors import safe_open

    from . import graphs as G
    from . import textbox as TB

    pin = TB.PINS.get(engine)
    if pin is None:
        raise SystemExit(f"{engine}: not a text-box engine (those are {sorted(TB.PINS)})")
    if G.ROLES[engine] != ("model",):
        raise SystemExit(f"{engine}: no crop gate for the roles {G.ROLES[engine]} yet")
    pkg = Path(out) / engine / precision / target / "model.pt2"
    if not pkg.is_file():
        raise SystemExit(f"{pkg}: no such package")
    if weights_inside(pkg, pin.size):
        raise SystemExit(f"{pkg}: weights inside the package")
    want = {}
    with open(expected, encoding="utf-8") as f:
        for line in f:
            if line.strip():
                r = json.loads(line)
                want[r["file"]] = r["ids"]
    files = sorted(p for p in Path(crops).iterdir() if p.suffix.lower() in IMAGE_SUFFIXES)
    files = files[:n] if n else files
    if not files:
        raise SystemExit(f"no crops in {crops}")
    missing = [p.name for p in files if p.name not in want]
    if missing:
        raise SystemExit(f"{expected}: no expected ids for {len(missing)} crops, first {missing[:3]}")

    E = TB._export_nar()
    snap = E.snapshot_dir(os.environ.get(TB.SNAPSHOT_VAR))
    E.check_pins(snap)
    TB.check_pin(snap / pin.file, pin)
    sys.path.insert(0, str(snap))
    from manga_ocr_nar_small.preprocess import empty_inputs, preprocess

    end = len(json.loads((snap / "nar_vocabulary_6817.json").read_text(encoding="utf-8"))["characters"])
    dev = torch.device(device, 0) if device == "cuda" else torch.device("cpu")
    weights = {}
    with safe_open(str(snap / pin.file), "pt") as s:
        for k in s.keys():
            weights[k] = s.get_tensor(k).to(dev)
    m, meta = load_pkg(pkg, weights, dev.index if dev.type == "cuda" else -1)
    for key, value in (("bunko.io", TB.IO), ("bunko.weights_sha256", pin.sha256)):
        if meta.get(key) != value:
            raise SystemExit(f"{pkg}: {key} is {meta.get(key)!r}, not {value!r}")
    if not json.loads(meta.get("bunko.weights", "{}")):
        raise SystemExit(f"{pkg}: no bunko.weights map: its constants are not bound to the authors' {pin.file}")
    identical, differ = 0, []
    t0 = time.time()
    with torch.no_grad():
        for i in range(0, len(files), batch):
            chunk = files[i:i + batch]
            buf = empty_inputs(len(chunk))
            for j, p in enumerate(chunk):
                with Image.open(p) as im:
                    preprocess(im.convert("RGB"), {k: v[j] if k in ("P", "V", "R", "C") else v[j:j + 1]
                                                   for k, v in buf.items()})
            ids = m(*(torch.from_numpy(buf[k]).to(dev) for k in ("P", "V", "R", "C", "H", "W"))).cpu().tolist()
            for p, got in zip(chunk, ids):
                if same_ids(got, want[p.name], end):
                    identical += 1
                else:
                    differ.append(p.name)
    print(f"{len(files)} crops in {time.time() - t0:.1f}s, batch {batch}", file=sys.stderr)
    return {"crops": len(files), "identical": identical, "differ": differ}, [m, weights]


def verify_textbox(out: Path, engine: str, precision: str, target: str, crops: Path, expected: Path,
                   device: str, n: int | None = None) -> dict:
    """A text-box package against the authors' ids, crop by crop:
    ``{"crops": n, "identical": k, "differ": [file, ...]}``. Refuses (SystemExit) a
    package with weights inside before loading it."""
    res, held = _verify_textbox(out, engine, precision, target, crops, expected, device, n)
    _release(held)
    return res


def _release(held: list) -> None:
    # free the packages before the weights they bind (the runtime's order: PackageSet
    # drops its graphs before its Weights): ``held`` lists packages first, one at a time
    import gc

    for i, h in enumerate(held):
        if isinstance(h, dict):
            h.clear()
        held[i] = None
        del h
        gc.collect()


def _finish(rc: int, held: list) -> int:
    if sys.platform == "win32" and os.environ.get("TORCH_EXPORT_VERIFY_TEARDOWN") != "1":
        # end here, before the packages are destroyed: on the Windows runner destroying a
        # loaded package ended the process with 0xC0000005 after the result was printed
        # (TORCH_EXPORT_VERIFY_TEARDOWN=1 goes on, to observe it)
        import ctypes

        sys.stdout.flush()
        sys.stderr.flush()
        k32 = ctypes.WinDLL("kernel32")
        k32.GetCurrentProcess.restype = ctypes.c_void_p
        k32.TerminateProcess.argtypes = [ctypes.c_void_p, ctypes.c_uint]
        k32.TerminateProcess(k32.GetCurrentProcess(), rc)
    _release(held)
    print("packages released", file=sys.stderr, flush=True)
    return rc


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out", type=Path)
    ap.add_argument("engine")
    ap.add_argument("precision")
    ap.add_argument("target")
    ap.add_argument("--device", default="cuda")
    ap.add_argument("--bench", type=int, default=0, help="time N more runs of each package")
    ap.add_argument("--crops", type=Path, help="text-box engines: a directory of crop images")
    ap.add_argument("--expected", type=Path, help="text-box engines: check_parity.py --expected JSON lines")
    ap.add_argument("--n", type=int, default=None, help="text-box engines: only the first N crops by name")
    a = ap.parse_args(argv)
    from .__main__ import _imports

    _imports()
    from .textbox import PINS

    if a.engine in PINS:
        if a.crops is None or a.expected is None:
            raise SystemExit(f"{a.engine}: --crops and --expected are required")
        res, held = _verify_textbox(a.out, a.engine, a.precision, a.target, a.crops, a.expected, a.device, a.n)
        print(json.dumps(res), flush=True)
        return _finish(0 if res["identical"] == res["crops"] else 1, held)
    import torch

    from . import graphs as G

    dev = torch.device(a.device, 0) if a.device == "cuda" else torch.device("cpu")
    pdir = a.out / a.engine / a.precision
    tdir = pdir / a.target
    weights = load_weights(pdir, dev) if any(pdir.glob("weights-*.safetensors")) else {}
    t0 = time.time()
    pk = {}
    for role in G.ROLES[a.engine]:
        pk[role], meta = load_pkg(tdir / f"{role}.pt2", weights, dev.index if dev.type == "cuda" else -1)
    io = int(meta["bunko.io"])
    load_s = time.time() - t0
    print(f"loaded 3 packages + {len(weights)} weights in {load_s:.1f}s (io v{io})", file=sys.stderr)
    gs = G.BUILDERS[a.engine](a.precision, dev, io)
    res = {}
    with torch.no_grad():
        for g in gs.graphs:
            ref = g.module(*g.args)
            got = pk[g.role](*g.args)
            ref = ref if isinstance(ref, (tuple, list)) else (ref,)
            got = got if isinstance(got, (tuple, list)) else (got,)
            diffs = []
            for r, o in zip(ref, got):
                if r.dtype in (torch.int64, torch.bool):
                    diffs.append(0.0 if torch.equal(r, o) else float("inf"))
                else:
                    diffs.append((r.float() - o.float()).abs().max().item())
            res[g.role] = {"outputs": len(got), "max_abs_diff": max(diffs), "ids_equal": all(d != float("inf") for d in diffs)}
            if a.bench:
                ts = []
                for _ in range(a.bench):
                    if dev.type == "cuda":
                        torch.cuda.synchronize()
                    t1 = time.perf_counter()
                    pk[g.role](*g.args)
                    if dev.type == "cuda":
                        torch.cuda.synchronize()
                    ts.append((time.perf_counter() - t1) * 1e3)
                ts.sort()
                res[g.role].update({"ms_median": round(ts[len(ts) // 2], 2), "ms_min": round(ts[0], 2)})
    extra = {"load_s": round(load_s, 2), "weights": len(weights)}
    if a.bench:
        extra["peak_rss_mb"] = peak_rss_mb()
    print(json.dumps({**res, **extra}), flush=True)
    rc = 0 if all(r["ids_equal"] for r in res.values()) else 1
    return _finish(rc, [pk, weights])


if __name__ == "__main__":
    sys.exit(main())
