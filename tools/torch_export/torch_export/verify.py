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

The AnimeText detector (``detector.PINS``: animetext-blocks) takes the page-level gate:

    python -m torch_export.verify <out> animetext-blocks fp32 <target> --pages DIR|LIST --onnx FILE

letterboxes every page as the host does (``canvas``), runs it through the package (its
constants bound from the pinned ONNX file by ``bunko.weights``) and through ONNX Runtime
on the CPU, one page a call, turns both outputs into blocks with one port of the host's
``blocks_from_output`` and compares them (``verify_detector``). Exit 0 only when every
page has the same block set, every matched corner is within 0.01 page px and no crop
edge (``crop_edges``) moved. ``--bench-batches 1,4,8`` then times the package at each
batch size on the gate's largest canvas (``detector_bench``, ``pages_per_s``); the gate
loop itself reports ``package_ms_per_page`` (one page a call, the copy back and the first
run at each canvas shape included).
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
PAGE_SUFFIXES = (".png", ".jpg", ".jpeg", ".webp", ".avif", ".bmp")
WORST_CORNER = 0.01  # page px: the detector gate's largest corner difference


def same_ids(got: list[int], want: list[int], end: int) -> bool:
    """Equal up to and including the first ``end`` in ``want`` (what a text is read
    from); ids past it are padding the reader never looks at. No end id: all of them."""
    n = want.index(end) + 1 if end in want else len(want)
    return len(got) >= n and list(got[:n]) == list(want[:n])


def crop_edges(b) -> tuple[int, int, int, int]:
    """The pixels a block's crop starts and ends at (as ``page.rs`` crops):
    ``(floor(x0), floor(y0), ceil(x1), ceil(y1))``."""
    import math

    return math.floor(b[0]), math.floor(b[1]), math.ceil(b[2]), math.ceil(b[3])


def package_constants(pt2: Path) -> list[str]:
    """The constant FQNs a ``.pt2`` asks for at load (``constants_info_[i].original_fqn``
    in its wrapper source), in order."""
    import re

    with zipfile.ZipFile(pt2) as z:
        srcs = [n for n in z.namelist() if n.endswith(".wrapper.cpp")]
        if len(srcs) != 1:
            raise SystemExit(f"{pt2}: {len(srcs)} wrapper sources; cannot list the package's constants")
        text = z.read(srcs[0]).decode("utf-8", "replace")
    found = re.findall(r'constants_info_\[(\d+)\]\.original_fqn = "([^"]*)"', text)
    return [f for _, f in sorted(found, key=lambda x: int(x[0]))]


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
    """The pinned weights file's size of ``engine`` (the authors' weights, or the ONNX
    file a detector binds); the smallest pinned size for an engine without a pin (the
    strictest limit)."""
    from .detector import PINS as DETECTOR_PINS
    from .textbox import PINS

    pins = {**PINS, **DETECTOR_PINS}
    return pins[engine].size if engine in pins else min(p.size for p in pins.values())


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


# --- the AnimeText detector: page-level gate -------------------------------------------

SIDE, STRIDE, PAD_BYTE = 640, 32, 114  # bunko-ocr animetext.rs
MIN_SCORE, OVERLAP, PARTS, INSIDE, MIN_SIDE = 0.3, 0.5, 2, 0.8, 4.0


def input_size(width: int, height: int) -> tuple[float, int, int, int, int]:
    """bunko-ocr ``animetext::input_size``: (scale, scaled w, scaled h, canvas w, canvas h)."""
    scale = SIDE / max(width, height, 1)

    def fit(v):  # Python's round() is round-half-even, as py::round_int
        return min(max(round(v * scale), 1), SIDE)

    w, h = fit(width), fit(height)
    return scale, w, h, -(-w // STRIDE) * STRIDE, -(-h // STRIDE) * STRIDE


def _area_weights(n: int, src: int):
    """bunko-ocr ``fit``'s spans as an ``[n, src]`` matrix: each output pixel's share of
    each source pixel it covers."""
    import math

    import numpy as np

    f = src / n
    m = np.zeros((n, src), np.float64)
    for i in range(n):
        a, b = i * f, min((i + 1) * f, src)
        i0, i1 = math.floor(a), max(min(math.ceil(b), src), 1)
        m[i, i0:i1] = 1.0
        m[i, i0] = 1.0 - (a - i0)
        if i1 - 1 > i0:
            m[i, i1 - 1] = 1.0 - (i1 - b)
    return m


def canvas(rgb):
    """A page (``[h, w, 3]`` uint8 RGB) on the detector's canvas, as bunko-ocr
    ``animetext::canvas`` makes it: ``(chw f32 0..1, scale, (page w, page h))``, or None for
    a page of zero width or height. A shrunk page is area-averaged (``fit``); a page
    stretched up is resampled bilinearly by PIL (the gate compares the two routes on the
    same canvas, so the resampler only has to give a realistic page)."""
    import numpy as np

    ph, pw = rgb.shape[:2]
    if pw == 0 or ph == 0:
        return None
    scale, w, h, cw, ch = input_size(pw, ph)
    if w >= pw or h >= ph:
        from PIL import Image

        small = np.asarray(Image.fromarray(rgb).resize((w, h), Image.BILINEAR))
    else:
        wy, wx = _area_weights(h, ph), _area_weights(w, pw)
        norm = wy.sum(1)[:, None] * wx.sum(1)[None, :]
        small = np.stack([wy @ rgb[:, :, c].astype(np.float64) @ wx.T / norm for c in range(3)], -1)
        small = np.floor(small + 0.5).clip(0, 255).astype(np.uint8)
    c = np.full((ch, cw, 3), PAD_BYTE, np.uint8)
    c[:h, :w] = small
    return (c.transpose(2, 0, 1).astype(np.float32) / np.float32(255.0)), scale, (pw, ph)


def _area(b):
    return max(0.0, b[2] - b[0]) * max(0.0, b[3] - b[1])


def _inter(a, b):
    return max(0.0, min(a[2], b[2]) - max(a[0], b[0])) * max(0.0, min(a[3], b[3]) - max(a[1], b[1]))


def blocks_from_output(out, scale: float, page: tuple[int, int]) -> list[tuple[list[float], float]]:
    """bunko-ocr ``animetext::blocks_from_output`` on a ``[5, N]`` output, in f32:
    ``[(bbox [x0, y0, x1, y1] in page px, score)]``."""
    import numpy as np

    out = np.asarray(out, np.float32)
    pw, ph, s = np.float32(page[0]), np.float32(page[1]), np.float32(scale)
    two = np.float32(2.0)
    found = []
    for i in range(out.shape[1]):
        cx, cy, w, h, score = out[:, i]
        if score < np.float32(MIN_SCORE):
            continue
        b = [float(np.clip((cx - w / two) / s, 0, pw)), float(np.clip((cy - h / two) / s, 0, ph)),
             float(np.clip((cx + w / two) / s, 0, pw)), float(np.clip((cy + h / two) / s, 0, ph))]
        if b[2] - b[0] >= MIN_SIDE and b[3] - b[1] >= MIN_SIDE:
            found.append((b, float(score)))
    found.sort(key=lambda t: -t[1])  # stable, as sort_by
    kept = []
    for b in found:
        if not any((lambda sh, un: un > 0 and sh / un > OVERLAP)(_inter(k[0], b[0]), _area(k[0]) + _area(b[0]) - _inter(k[0], b[0]))
                   for k in kept):
            kept.append(b)

    def wraps(o):
        return sum(1 for i in kept if 0 < _area(i[0]) < _area(o[0]) and _inter(i[0], o[0]) / _area(i[0]) >= INSIDE) >= PARTS

    kept = [b for b in kept if not wraps(b)]

    def part(i):
        a = _area(i[0])
        return any(_area(o[0]) > a > 0 and _inter(i[0], o[0]) / a >= INSIDE for o in kept)

    return [b for b in kept if not part(b)]


def compare_blocks(ref, got) -> tuple[bool, float, int]:
    """(same block set, largest corner difference, crop edges moved). Blocks pair up by
    the largest overlap (each used once); a set differs when the counts differ or a pair
    overlaps by IoU 0.9 or less."""
    if len(ref) != len(got):
        return False, float("inf"), 0
    used, worst, moved = set(), 0.0, 0
    for b, _ in ref:
        best = max(((_inter(b, g[0]) / (_area(b) + _area(g[0]) - _inter(b, g[0]) + 1e-12), j)
                    for j, g in enumerate(got) if j not in used), default=(0.0, -1))
        if best[0] <= 0.9:
            return False, float("inf"), 0
        used.add(best[1])
        g = got[best[1]][0]
        worst = max(worst, max(abs(x - y) for x, y in zip(b, g)))
        moved += sum(x != y for x, y in zip(crop_edges(b), crop_edges(g)))
    return True, worst, moved


def page_files(pages: Path) -> list[Path]:
    """A directory's page images (recursively, sorted), or a list file's (one path a line,
    relative to the list's directory; blank lines and ``#`` comments skipped)."""
    pages = Path(pages)
    if pages.is_dir():
        files = sorted(p for p in pages.rglob("*") if p.suffix.lower() in PAGE_SUFFIXES)
    elif pages.is_file():
        files = []
        for line in pages.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if line and not line.startswith("#"):
                p = Path(line)
                files.append(p if p.is_absolute() else pages.parent / p)
        absent = [str(p) for p in files if not p.is_file()]
        if absent:
            raise SystemExit(f"{pages}: {len(absent)} listed pages do not exist, first {absent[:3]}")
    else:
        raise SystemExit(f"{pages}: no such directory or page list")
    if not files:
        raise SystemExit(f"{pages}: no pages")
    return files


def batches_arg(text: str) -> tuple[int, ...]:
    """``--bench-batches``: comma-separated batch sizes, each 1 or more."""
    b = tuple(int(x) for x in text.split(",") if x.strip())
    if not b or min(b) < 1:
        raise ValueError(f"batch sizes {text!r}: each must be 1 or more")
    return b


def detector_bench(m, dev, x, batches, runs: int = 20, warmup: int = 3) -> dict[str, float]:
    """Pages a second of the package ``m`` at each batch size: ``x`` (one ``[3, H, W]``
    canvas) repeated ``b`` times, ``warmup`` untimed runs, then ``runs`` timed ones, each
    synchronised. Refuses a batch over the package's ``bunko.max_batch``."""
    import torch

    from .detector import MAX_BATCH

    over = [b for b in batches if b > MAX_BATCH]
    if over:
        raise SystemExit(f"batch {over}: over the package's bunko.max_batch ({MAX_BATCH})")

    def sync():
        if dev.type == "cuda":
            torch.cuda.synchronize(dev)

    res = {}
    with torch.no_grad():
        for b in batches:
            xb = torch.from_numpy(x[None]).to(dev).expand(b, *x.shape).contiguous()
            for _ in range(warmup):
                m(xb)
            sync()
            t0 = time.perf_counter()
            for _ in range(runs):
                m(xb)
            sync()
            res[str(b)] = round(b * runs / (time.perf_counter() - t0), 2)
    return res


def _verify_detector(out: Path, target: str, pages: Path, onnx_file: Path, device: str, engine: str = "animetext-blocks",
                     bench_batches: tuple[int, ...] = ()):
    """``verify_detector``'s work; also returns what it holds loaded (package, weights)."""
    import numpy as np
    import onnxruntime as ort
    import torch
    from PIL import Image

    from . import detector as D

    pin = D.PINS.get(engine)
    if pin is None:
        raise SystemExit(f"{engine}: not a detector engine (those are {sorted(D.PINS)})")
    pkg = Path(out) / engine / "fp32" / target / "detector.pt2"
    if not pkg.is_file():
        raise SystemExit(f"{pkg}: no such package")
    if weights_inside(pkg, pin.size):
        raise SystemExit(f"{pkg}: weights inside the package")
    files = page_files(pages)
    D.check_pin(onnx_file, pin)
    torch.backends.cuda.matmul.allow_tf32 = False
    torch.backends.cudnn.allow_tf32 = False
    dev = torch.device(device, 0) if device == "cuda" else torch.device("cpu")
    with zipfile.ZipFile(pkg) as z:
        meta_src = [n for n in z.namelist() if n.endswith(".wrapper_metadata.json")]
        meta = json.loads(z.read(meta_src[0])) if len(meta_src) == 1 else {}
    wm = json.loads(meta.get("bunko.weights", "{}"))
    for key, value in (("bunko.io", D.IO), ("bunko.weights_format", "onnx"), ("bunko.weights_sha256", pin.sha256),
                       ("bunko.ultralytics", D.ULTRALYTICS_VERSION)):
        if meta.get(key) != value:
            raise SystemExit(f"{pkg}: {key} is {meta.get(key)!r}, not {value!r}")
    unbound = sorted(set(package_constants(pkg)) - set(wm))
    if unbound:
        raise SystemExit(f"{pkg}: constants without an ONNX initializer: {unbound[:5]}")
    arrays = D.onnx_arrays(onnx_file, set(wm.values()))
    weights = {name: torch.from_numpy(a).to(dev) for name, a in arrays.items()}
    del arrays
    m, _ = load_pkg(pkg, weights, dev.index if dev.type == "cuda" else -1)
    so = ort.SessionOptions()
    sess = ort.InferenceSession(str(onnx_file), so, providers=["CPUExecutionProvider"])
    res = {"pages": 0, "identical_sets": 0, "worst_corner": 0.0, "edges_moved": 0, "blocks": 0, "differ": []}
    t0 = time.time()
    package_s, timed, largest = 0.0, 0, None
    with torch.no_grad():
        for p in files:
            with Image.open(p) as im:
                rgb = np.asarray(im.convert("RGB"))
            res["pages"] += 1
            c = canvas(rgb)
            if c is None:  # no blocks on either route
                res["identical_sets"] += 1
                continue
            x, scale, page = c
            ref = sess.run(["output0"], {"images": x[None]})[0][0]
            t1 = time.perf_counter()
            got = m(torch.from_numpy(x[None]).to(dev))
            got = (got[0] if isinstance(got, (tuple, list)) else got)[0].float().cpu().numpy()
            package_s, timed = package_s + time.perf_counter() - t1, timed + 1
            if largest is None or x.shape[1] * x.shape[2] > largest.shape[1] * largest.shape[2]:
                largest = x
            rb, gb = blocks_from_output(ref, scale, page), blocks_from_output(got, scale, page)
            same, worst, moved = compare_blocks(rb, gb)
            res["blocks"] += len(rb)
            if same:
                res["identical_sets"] += 1
                res["worst_corner"] = max(res["worst_corner"], worst)
                res["edges_moved"] += moved
            if not same or worst > WORST_CORNER or moved:
                name = str(p.relative_to(pages)) if Path(pages).is_dir() else str(p)
                res["differ"].append({"page": name, "onnx": rb, "package": gb})
    print(f"{res['pages']} pages in {time.time() - t0:.1f}s, one a call", file=sys.stderr)
    res["package_ms_per_page"] = round(package_s * 1e3 / timed, 2) if timed else None
    if bench_batches and largest is not None:
        res["bench_canvas"] = list(largest.shape[1:])
        res["pages_per_s"] = detector_bench(m, dev, largest, bench_batches)
    return res, [m, weights]


def detector_passes(res: dict) -> bool:
    return (res["identical_sets"] == res["pages"] and res["worst_corner"] <= WORST_CORNER
            and res["edges_moved"] == 0)


def verify_detector(out: Path, target: str, pages: Path, onnx: Path, device: str) -> dict:
    """The AnimeText package against ONNX Runtime on the CPU, page by page:
    ``{"pages", "identical_sets", "worst_corner", "edges_moved", "blocks", "differ"}``
    (``blocks``: the ONNX route's block count over all pages; ``differ``: the pages that
    fail, with both block sets). Passing (``detector_passes``):
    ``identical_sets == pages``, ``worst_corner <= 0.01``, ``edges_moved == 0``. Refuses
    (SystemExit) a package with weights inside before loading it."""
    res, held = _verify_detector(out, target, pages, onnx, device)
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
    ap.add_argument("--pages", type=Path, help="detector: a directory of page images, or a list file of them")
    ap.add_argument("--onnx", type=Path, help="detector: the pinned ONNX file the package binds")
    ap.add_argument("--bench-batches", type=batches_arg, default=(),
                    help="detector: after the gate, pages/s at these batch sizes (e.g. 1,4,8) on the largest canvas")
    a = ap.parse_args(argv)
    from .__main__ import _imports

    _imports()
    from .detector import PINS as DETECTOR_PINS
    from .textbox import PINS

    if a.engine in DETECTOR_PINS:
        if a.pages is None or a.onnx is None:
            raise SystemExit(f"{a.engine}: --pages and --onnx are required")
        if a.precision != "fp32":
            raise SystemExit(f"{a.engine}: fp32 only")
        res, held = _verify_detector(a.out, a.target, a.pages, a.onnx, a.device, a.engine, a.bench_batches)
        print(json.dumps({**res, "differ": res["differ"][:3], "differ_pages": [d["page"] for d in res["differ"]]}), flush=True)
        return _finish(0 if detector_passes(res) else 1, held)

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
