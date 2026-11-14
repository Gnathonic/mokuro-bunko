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

Text-box engines (``textbox.PINS``: nar-small, charspot-small) take the crop-level gate
here instead:

    python -m torch_export.verify <out> nar-small fp32|fp16 <target> --crops DIR --expected FILE [--n N] [--batch B]
                                  [--reads FILE]

binds the authors' pinned ``model.safetensors`` by ``bunko.weights`` (cast to F16 first
when the package says ``bunko.weights_dtype = fp16``, as the host does), feeds every crop
(``--n``: the first N by name) as the authors' ``preprocess.py`` prepares it, ``--batch``
(32) at a time, and compares with ``--expected`` (the JSON lines
``tools/textbox_export/check_parity.py --expected`` writes). fp32 compares the ids, up
to and including the first end id, and exits 0 only when every crop is identical (fp32
has no tolerance). fp16 compares the decoded texts and exits 0 when at most
``floor(0.001 x crops)`` differ (``texts_gate``, TEXTBOX-GPU.md §11). ``--reads FILE`` writes
what the package read in ``--expected``'s own form (``{"file","ids","text"}`` lines), so a
read at one batch size is another's ``--expected`` and ``python -m torch_export
compare-reads`` pairs any two (``gate.py``). Any package whose constants live
inside it is refused before it is loaded (``weights_inside``), and
``python -m torch_export refuse-embedded DIR`` checks a whole build or staging tree.

charspot-small (roles ``dense`` and ``tail``) takes the same command without ``--reads``,
with ``--expected`` the JSON lines ``tools/textbox_export/charspot_reference.py`` writes
(the authors' ONNX model on the CPU) and ``--batch`` the canvas batch (default 32, at most 64): both packages
bound to the authors' ``model.safetensors``, the authors' ``core.detect`` / ``core.chain``
between them (``charspot_read``), and every crop must be ``same_chars`` (same text and
order, boxes within 0.5 px). Exit 0 only when every crop is.
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


def text_of(ids: list[int], chars: list[str]) -> str:
    """The text ``ids`` spell: the characters before the first end id (``len(chars)``),
    as ``tools/textbox_export/check_parity.py`` and the host decode them."""
    end = len(chars)
    ids = list(ids)
    ids = ids[:ids.index(end)] if end in ids else ids
    return "".join(chars[i] for i in ids if 0 <= i < end)


def package_meta(pt2: Path) -> dict[str, str]:
    """A ``.pt2``'s metadata as the loader's ``get_metadata`` returns it, read from the
    archive without loading it (``<role>/data/aotinductor/<model>/*.wrapper_metadata.json``,
    where AOTInductor writes it on every OS); ``{}`` when the archive has none."""
    with zipfile.ZipFile(pt2) as z:
        names = sorted(n for n in z.namelist() if n.endswith("_metadata.json") and "/data/aotinductor/" in n)
        wrapper = [n for n in names if n.endswith(".wrapper_metadata.json")]
        return json.loads(z.read((wrapper or names)[0])) if names else {}


def check_textbox_meta(meta: dict, precision: str, pin, where) -> None:
    """Refuse (SystemExit) a package that binds its authors' weights another way than its
    precision says: an fp32 package binds them as stored (no ``bunko.weights_dtype``),
    any other says that precision there (the host casts the file to it at load and the
    cdylib refuses anything else), and a ``bunko.weights_sha256`` must be the pin's."""
    want = None if precision == "fp32" else precision
    got = meta.get("bunko.weights_dtype")
    if got != want:
        raise SystemExit(f"{where}: bunko.weights_dtype is {got!r}, a {precision} package says {want!r}")
    sha = meta.get("bunko.weights_sha256")
    if sha is not None and getattr(pin, "sha256", sha) != sha:
        raise SystemExit(f"{where}: bunko.weights_sha256 is {sha}, not the pinned {pin.sha256}")


def texts_gate(differ: int, crops: int, limit: float = 0.001) -> bool:
    """fp16's crop gate: at most ``floor(crops x limit)`` texts differ (7 of 7,929)."""
    return differ <= int(crops * limit)


def same_chars(got: dict, want: dict, tol: float = 0.5) -> bool:
    """charspot's gate: the same text, the same characters in the same order, and every box
    coordinate within ``tol`` px (``chars``: ``[[c, x0, y0, x1, y1, score], ...]``, the
    score is not compared)."""
    if got["text"] != want["text"] or len(got["chars"]) != len(want["chars"]):
        return False
    for g, w in zip(got["chars"], want["chars"]):
        # written so that a NaN coordinate fails
        if g[0] != w[0] or not all(abs(float(a) - float(b)) <= tol for a, b in zip(g[1:5], w[1:5])):
            return False
    return True


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


def cast_weights(weights: dict, keys, weights_dtype: str | None) -> None:
    """The host's load-time cast (bunko-torch ``Weights::load``), in place: with
    ``weights_dtype`` "fp16" every floating tensor of ``keys`` becomes float16, integer
    and bool tensors stay as stored. In place so the caller keeps the cast tensors alive
    (``load_constants(user_managed=True)`` binds their memory, it copies nothing)."""
    import torch

    if weights_dtype is None:
        return
    if weights_dtype != "fp16":
        raise SystemExit(f"bunko.weights_dtype {weights_dtype}: this runtime casts to fp16 only")
    for k in keys:
        t = weights[k]
        if t.is_floating_point() and t.dtype != torch.float16:
            weights[k] = t.to(torch.float16)


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
        cast_weights(weights, {wm[f] for f in fqns}, meta.get("bunko.weights_dtype"))
        m.loader.load_constants({f: weights[wm[f]] for f in fqns}, False, True, True)
    return m, meta


def _verify_textbox(out: Path, engine: str, precision: str, target: str, crops: Path, expected: Path,
                    device: str, n: int | None = None, batch: int = 32, reads: Path | None = None):
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
    if G.ROLES[engine] == ("dense", "tail"):
        if reads is not None:
            raise SystemExit(f"{engine}: --reads writes ids, which {engine} has none of")
        return _verify_charspot(out, precision, target, crops, expected, device, n, batch)
    if G.ROLES[engine] != ("model",):
        raise SystemExit(f"{engine}: no crop gate for the roles {G.ROLES[engine]} yet")
    pkg = Path(out) / engine / precision / target / "model.pt2"
    if not pkg.is_file():
        raise SystemExit(f"{pkg}: no such package")
    if weights_inside(pkg, pin.size):
        raise SystemExit(f"{pkg}: weights inside the package")
    if precision not in ("fp32", "fp16"):
        raise SystemExit(f"{engine}: no crop gate for {precision} (fp32: ids, fp16: texts)")
    if not 1 <= batch <= TB.MAX_BATCH:
        raise SystemExit(f"batch {batch}: the package takes 1..{TB.MAX_BATCH} crops per call")
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

    chars = json.loads((snap / "nar_vocabulary_6817.json").read_text(encoding="utf-8"))["characters"]
    end = len(chars)
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
    check_textbox_meta(meta, precision, pin, pkg)
    if precision == "fp16":
        def same(got, ids):
            return text_of(got, chars) == text_of(ids, chars)
    else:
        def same(got, ids):
            return same_ids(got, ids, end)
    identical, differ, rows = 0, [], []
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
                if reads is not None:
                    rows.append(json.dumps({"file": p.name, "ids": got, "text": text_of(got, chars)},
                                           ensure_ascii=False) + "\n")
                if same(got, want[p.name]):
                    identical += 1
                else:
                    differ.append(p.name)
    took = time.time() - t0
    print(f"{len(files)} crops in {took:.1f}s, batch {batch}", file=sys.stderr)
    if reads is not None:
        Path(reads).parent.mkdir(parents=True, exist_ok=True)
        Path(reads).write_text("".join(rows), encoding="utf-8")
    passed = texts_gate(len(differ), len(files)) if precision == "fp16" else identical == len(files)
    res = {"crops": len(files), "identical": identical, "differ": differ, "batch": batch,
           "compared": "texts" if precision == "fp16" else "ids", "passed": passed,
           # the whole loop: crops decoded and prepared on the CPU, then read
           "crops_per_s": round(len(files) / took, 1) if took > 0 else None}
    return res, [m, weights]


def charspot_batches(shapes: list[tuple[int, int]], max_batch: int, snap: Path) -> list[tuple[list[int], tuple[int, int]]]:
    """The authors' canvas batches (``CudaBackend.batches`` at the pin, without CUDA graph
    canvases): ``[(crop indices, (H, W) px)]``."""
    if str(snap) not in sys.path:
        sys.path.insert(0, str(snap))
    from manga_ocr_charspot_small.backends.cuda import CudaBackend

    cb = CudaBackend.__new__(CudaBackend)  # batches() reads only these two
    cb.graphs, cb.max_batch = False, max_batch
    return cb.batches(shapes)


def charspot_read(paths: list[Path], dense, tail, core, out_str: list[str], device, max_batch: int,
                  snap: Path) -> list[dict]:
    """charspot-small through two graphs (packages or the eager ``DenseGraph`` /
    ``TailGraph``) with the authors' decode between them, as Bunko's ``TorchCharspot``
    runs it: crops prepared by ``core.preprocess``, canvas batches of at most
    ``max_batch``, ``dense`` per batch, ``core.detect`` per crop on the host, the batch's
    rows in the authors' tail parts, each padded to ``max(2, n0)`` characters (geo 1, mask
    false), ``tail`` per part, ``core.chain`` per crop. A canvas side under
    ``CHARSPOT_MIN_CELLS`` cells is widened to it. -> ``textbox.charspot_result`` per
    crop, in order."""
    import numpy as np
    import torch

    from . import textbox as TB

    pre = [core.preprocess(core.to_rgb(p)) for p in paths]  # (uint8 [3, H, W], (fx, fy))
    out: list[dict | None] = [None] * len(paths)
    least = 16 * TB.CHARSPOT_MIN_CELLS
    for idx, (H, W) in charspot_batches([tuple(x.shape[1:]) for x, _ in pre], max_batch, snap):
        H, W = max(H, least), max(W, least)  # the dense graph's smallest canvas side
        x = torch.zeros(len(idx), 3, H, W, dtype=torch.float32)
        hw = torch.empty(len(idx), 2, dtype=torch.int64)
        for r, i in enumerate(idx):
            a = pre[i][0]
            x[r, :, :a.shape[1], :a.shape[2]] = torch.from_numpy(a)
            hw[r, 0], hw[r, 1] = a.shape[1], a.shape[2]
        heat, box, ruby, emb = dense(x.to(device), hw.to(device))
        maps = torch.cat((heat, box, ruby), 1).cpu().numpy()
        dets = []
        for r in range(len(idx)):
            m = maps[r, :, :int(hw[r, 0]) // 8, :int(hw[r, 1]) // 8]
            dets.append(core.detect(m[0:1], m[1:5], m[5:6]))
        nk = [0 if d is None else len(d[0]) for d in dets]
        for r, i in enumerate(idx):
            if nk[r] == 0:
                out[i] = {"text": "", "chars": []}
        for part in TB.charspot_tail_parts(nk):
            n0 = nk[part[0]]
            if n0 > TB.CHARSPOT_MAX_CHARS:
                # cannot happen for a prepared crop (256 cells hold 1,024 centres); Bunko
                # reads such a crop on the CPU route
                raise SystemExit(f"{paths[idx[part[0]]]}: {n0} characters, more than the tail graph's {TB.CHARSPOT_MAX_CHARS}")
            width = max(2, n0)
            kb = np.zeros((len(part), width, 4), np.float32)
            geo = np.ones((len(part), width, 4), np.float32)
            mask = np.zeros((len(part), width), bool)
            for p, r in enumerate(part):
                kb[p, :nk[r]], _, geo[p, :nk[r]] = dets[r]
                mask[p, :nk[r]] = True
            rows = torch.tensor(part, dtype=torch.int64, device=emb.device)
            ids, pair, st, en = (t.cpu().numpy() for t in tail(emb.index_select(0, rows), *(
                torch.from_numpy(a).to(device) for a in (kb, geo, mask))))
            for p, r in enumerate(part):
                n = nk[r]
                seq = core.chain(pair[p, :n, :n], st[p, :n], en[p, :n])
                kb_r, sc_r, _ = dets[r]
                out[idx[r]] = TB.charspot_result(seq, ids[p, :n], kb_r, sc_r, pre[idx[r]][1], out_str)
    return out


def read_expected_chars(expected: Path) -> dict[str, dict]:
    """``tools/textbox_export/charspot_reference.py``'s JSON lines, by file name."""
    want = {}
    with open(expected, encoding="utf-8") as f:
        for line in f:
            if line.strip():
                r = json.loads(line)
                want[r["file"]] = r
    return want


def _verify_charspot(out: Path, precision: str, target: str, crops: Path, expected: Path, device: str,
                     n: int | None = None, batch: int = 32):
    """charspot-small's crop gate: both packages bound to the authors' pinned weights,
    ``charspot_read`` over the crops, every crop ``same_chars`` as the authors' ONNX model
    on the CPU (``--expected``: ``charspot_reference.py``'s output)."""
    import torch
    from safetensors import safe_open

    from . import textbox as TB

    pin = TB.CHARSPOT_PIN
    tdir = Path(out) / "charspot-small" / precision / target
    pkgs = {role: tdir / f"{role}.pt2" for role in ("dense", "tail")}
    for p in pkgs.values():  # both refused before either is loaded
        if not p.is_file():
            raise SystemExit(f"{p}: no such package")
        if weights_inside(p, pin.size):
            raise SystemExit(f"{p}: weights inside the package")
    want = read_expected_chars(expected)
    files = sorted(p for p in Path(crops).iterdir() if p.suffix.lower() in IMAGE_SUFFIXES)
    files = files[:n] if n else files
    if not files:
        raise SystemExit(f"no crops in {crops}")
    missing = [p.name for p in files if p.name not in want]
    if missing:
        raise SystemExit(f"{expected}: no expected characters for {len(missing)} crops, first {missing[:3]}")

    snap = TB.charspot_snapshot()
    TB.check_charspot(snap, [pin.file, "config.json", "vocab.json", "manga_ocr_charspot_small/core.py",
                             "manga_ocr_charspot_small/backends/cuda.py"])
    core, _ = TB.charspot_modules(snap)
    cfg = json.loads((snap / "config.json").read_text(encoding="utf-8"))
    out_str = core.load_vocab(snap / cfg["vocab"], cfg["n_cls"])
    dev = torch.device(device, 0) if device == "cuda" else torch.device("cpu")
    weights = {}
    with safe_open(str(snap / pin.file), "pt") as s:
        for k in s.keys():
            weights[k] = s.get_tensor(k).to(dev)
    held = []
    for role, p in pkgs.items():
        m, meta = load_pkg(p, weights, dev.index if dev.type == "cuda" else -1)
        held.append(m)
        for key, value in (("bunko.io", TB.IO), ("bunko.weights_sha256", pin.sha256), ("bunko.role", role)):
            if meta.get(key) != value:
                raise SystemExit(f"{p}: {key} is {meta.get(key)!r}, not {value!r}")
        if not json.loads(meta.get("bunko.weights", "{}")):
            raise SystemExit(f"{p}: no bunko.weights map: its constants are not bound to the authors' {pin.file}")
    if dev.type == "cuda":
        torch.cuda.reset_peak_memory_stats(dev)
    t0 = time.time()
    with torch.no_grad():
        got = charspot_read(files, held[0], held[1], core, out_str, dev, batch, snap)
    took = time.time() - t0
    identical, differ, moved = 0, [], 0.0
    for p, g in zip(files, got):
        if same_chars(g, want[p.name]):
            identical += 1
            moved = max([moved, *(abs(float(a) - float(b)) for gc, wc in zip(g["chars"], want[p.name]["chars"])
                                  for a, b in zip(gc[1:5], wc[1:5]))])
        else:
            differ.append(p.name)
    print(f"{len(files)} crops in {took:.1f}s, canvas batch {batch}", file=sys.stderr)
    by_name = {p.name: g for p, g in zip(files, got)}
    for name in differ[:5]:
        print(f"  {name}: {by_name[name]['text']!r} / expected {want[name]['text']!r}", file=sys.stderr)
    res = {"crops": len(files), "identical": identical, "differ": differ, "batch": batch,
           "passed": identical == len(files),
           # the whole loop: crops decoded and prepared on the CPU, both graphs, the decode
           "crops_per_s": round(len(files) / took, 1) if took > 0 else None,
           # over the crops that read the same
           "largest_move_px": moved,
           # torch's caching allocator during the read (the bound weights included)
           "torch_peak_reserved_mb": round(torch.cuda.max_memory_reserved(dev) / 2**20) if dev.type == "cuda"
           else None}
    return res, [*held, weights]


def verify_textbox(out: Path, engine: str, precision: str, target: str, crops: Path, expected: Path,
                   device: str, n: int | None = None, batch: int = 32, reads: Path | None = None) -> dict:
    """A text-box package against the authors' output, crop by crop:
    ``{"crops": n, "identical": k, "differ": [file, ...], "passed": bool, ...}`` (fp32:
    ids, every crop; fp16: texts, ``texts_gate``). Refuses (SystemExit) a package with
    weights inside before loading it."""
    res, held = _verify_textbox(out, engine, precision, target, crops, expected, device, n, batch, reads)
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
    ap.add_argument("--batch", type=int, default=32,
                    help="text-box engines: crops per call (charspot: the canvas batch, at most 64)")
    ap.add_argument("--reads", type=Path, default=None,
                    help="nar-small: write what the package read here ({file, ids, text} lines)")
    a = ap.parse_args(argv)
    from .__main__ import _imports

    _imports()
    from .textbox import PINS

    if a.engine in PINS:
        if a.crops is None or a.expected is None:
            raise SystemExit(f"{a.engine}: --crops and --expected are required")
        res, held = _verify_textbox(a.out, a.engine, a.precision, a.target, a.crops, a.expected, a.device, a.n,
                                    a.batch, a.reads)
        print(json.dumps(res), flush=True)
        return _finish(0 if res["passed"] else 1, held)
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
