"""wincheck.py <prec> <dir with unpacked vision/ prefill/ step/> <weights dir | embed:<dir>> <out.json>

Windows-only test driver (run with a torch 2.13+cu130 Windows venv: torch, numpy, safetensors,
tokenizers, pillow). Expects next to it: tools/onnx_export (or this repo's tools/), models/
(paddle-manga_tokenizer.json, paddle-manga_config.json), spike/paddle/crops/ (+ index.json).
This is the reference for the runtime's Windows weight binding (bunko.bind = "pairs").

Drives cross-built Windows paddle packages through the wrappers' C ABI only (ctypes):
CreateWithDevice, UpdateUserManagedConstantBufferPairs (shared weights, no std::unordered_map
across the MSVC/MinGW boundary), Run. Reads the 100 paddle crops with the reference numpy host
(tools/onnx_export paddle/host.py). Prints texts, crops/s and peak VRAM.
"""
import ctypes, json, os, sys, time
from pathlib import Path

import numpy as np
import torch

HERE = Path(__file__).resolve().parent
for _p in (HERE / "tools" / "onnx_export", HERE.parents[1] / "onnx_export"):
    if _p.is_dir():
        sys.path.insert(0, str(_p))
from onnx_export.paddle import host as H  # noqa: E402

os.add_dll_directory(str(Path(torch.__file__).parent / "lib"))
DEV = torch.device("cuda", 0)


_get = ctypes.pythonapi.PyCapsule_GetPointer
_get.restype, _get.argtypes = ctypes.c_void_p, [ctypes.py_object, ctypes.c_char_p]
_new = ctypes.pythonapi.PyCapsule_New
_new.restype, _new.argtypes = ctypes.py_object, [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_void_p]


def handles(tensors):
    """New AtenTensorHandles (raw pointers) for tensors."""
    return [_get(c, None) for c in torch._C._aoti.unsafe_alloc_void_ptrs_from_tensors(tensors)]


def steal(ptrs):
    return torch._C._aoti.alloc_tensors_by_stealing_from_void_ptrs([_new(p, None, None) for p in ptrs])


class Entry(ctypes.Structure):
    _fields_ = [("name", ctypes.c_char_p), ("handle", ctypes.c_void_p)]


class Pkg:
    def __init__(self, d: Path, weights: dict):
        model = next(d.rglob("data/aotinductor/model"))
        pyd = next(model.glob("*.wrapper.pyd"))
        self.lib = L = ctypes.CDLL(str(pyd))
        self.h = ctypes.c_void_p()
        self._chk(L.AOTInductorModelContainerCreateWithDevice(ctypes.byref(self.h), ctypes.c_size_t(1), b"cuda", str(model).encode()))
        meta = {}
        for f in model.glob("*wrapper_metadata.json"):
            meta.update(json.loads(f.read_text()))
        wmap = json.loads(meta.get("bunko.weights", "{}"))
        n = ctypes.c_size_t()
        self._chk(L.AOTInductorModelContainerGetNumConstants(self.h, ctypes.byref(n)))
        self.bound = 0
        blob = next(model.glob("*_weights.blob"), None)
        if blob is not None:
            data = blob.read_bytes()
            self._chk(L.AOTInductorModelUpdateConstantsFromBlob(self.h, ctypes.c_char_p(data)))
            del data
        if wmap:
            names, tensors = [], []
            for i in range(n.value):
                nm, fq = ctypes.c_char_p(), ctypes.c_char_p()
                self._chk(L.AOTInductorModelContainerGetConstantName(self.h, ctypes.c_size_t(i), ctypes.byref(nm)))
                self._chk(L.AOTInductorModelContainerGetConstantOriginalFQN(self.h, ctypes.c_size_t(i), ctypes.byref(fq)))
                names.append(nm.value)
                tensors.append(weights[wmap[fq.value.decode()]])
            self.keep = (names, tensors, handles(tensors))
            arr = (Entry * len(names))(*[Entry(a, b) for a, b in zip(names, self.keep[2])])
            self._chk(L.AOTInductorModelContainerUpdateUserManagedConstantBufferPairs(
                self.h, arr, ctypes.c_size_t(len(names)), ctypes.c_bool(False), ctypes.c_bool(True)))
            self.bound = len(names)
        no = ctypes.c_size_t()
        self._chk(L.AOTInductorModelContainerGetNumOutputs(self.h, ctypes.byref(no)))
        self.nout = no.value

    def _chk(self, rc):
        if rc != 0:
            raise RuntimeError(f"AOTI call failed: {rc}")

    def __call__(self, *inputs):
        hs = handles([t.contiguous() for t in inputs])
        ins = (ctypes.c_void_p * len(hs))(*hs)
        outs = (ctypes.c_void_p * self.nout)()
        stream = ctypes.c_void_p(torch.cuda.current_stream().cuda_stream)
        self._chk(self.lib.AOTInductorModelContainerRun(self.h, ins, ctypes.c_size_t(len(hs)), outs, ctypes.c_size_t(self.nout), stream, None))
        return steal([outs[i] for i in range(self.nout)])


class Backend:
    def __init__(self, root: Path, weights: dict, dt, tdt):
        self.vis, self.pre, self.stp = (Pkg(root / r, weights) for r in ("vision", "prefill", "step"))
        self.dt = self.vdt = dt
        self.tdt = tdt

    def vision(self, pv, idx, w, cos, sin, merge):
        t = lambda a, d=None: torch.from_numpy(np.ascontiguousarray(a)).to(DEV, d)  # noqa: E731
        o = self.vis(t(pv, self.tdt), t(idx), t(w), t(cos), t(sin), t(merge))[0]
        return o.float().cpu().numpy()

    def decoder(self, x, cos, sin, bias, past):
        t = lambda a: torch.from_numpy(np.ascontiguousarray(a)).to(DEV, self.tdt)  # noqa: E731
        if len(past) == 0 or past[0].shape[2] == 0:
            out = self.pre(t(x), t(cos), t(sin), t(bias))
        else:
            live = torch.ones(x.shape[0], dtype=torch.bool, device=DEV)
            out = self.stp(t(x), t(cos), t(sin), t(bias), live, *past)
        nxt = out[0].cpu().numpy()
        logits = np.zeros((x.shape[0], 103424), np.float32)  # the host only takes argmax
        logits[np.arange(x.shape[0]), nxt] = 1.0
        return logits, list(out[2:])


def main():
    prec, root, wdir, outp = sys.argv[1], Path(sys.argv[2]), sys.argv[3], Path(sys.argv[4])
    from safetensors import safe_open

    tdt = {"fp32": torch.float32, "bf16": torch.bfloat16, "fp16": torch.float16}[prec]
    weights = {}
    # "embed:<dir>": packages carry their weights; read only the host embedding table
    only_embed = wdir.startswith("embed:")
    wd = Path(wdir.removeprefix("embed:"))
    for f in sorted(wd.glob("weights-*.safetensors")):
        with safe_open(str(f), "pt") as s:
            for k in s.keys():
                if not only_embed:
                    weights[k] = s.get_tensor(k).to(DEV)
                if k == "model.language_model.embed_tokens.weight":
                    embed = s.get_tensor(k).float().numpy()
    import threading

    free0, total = torch.cuda.mem_get_info()
    peak = [0]
    stop = threading.Event()

    def sample():
        while not stop.is_set():
            f, _ = torch.cuda.mem_get_info()
            peak[0] = max(peak[0], total - f)
            time.sleep(0.1)

    threading.Thread(target=sample, daemon=True).start()
    torch.cuda.reset_peak_memory_stats()
    t0 = time.time()
    be = Backend(root, weights, np.float16 if prec == "fp16" else np.float32, tdt)
    load_s = time.time() - t0
    cfg = json.loads((HERE / "models" / "paddle-manga_config.json").read_text(encoding="utf-8"))
    host = H.PaddleHost(be, embed, HERE / "models" / "paddle-manga_tokenizer.json", cfg)
    from PIL import Image

    cdir = HERE / "spike" / "paddle" / "crops"
    names = sorted(json.loads((cdir / "index.json").read_text(encoding="utf-8")))
    crops = [Image.open(cdir / n).convert("RGB") for n in names]
    host(crops[:12])
    torch.cuda.synchronize()
    t1 = time.time()
    texts = host(crops)
    torch.cuda.synchronize()
    dt = time.time() - t1
    stats = {"prec": prec, "bound": [be.vis.bound, be.pre.bound, be.stp.bound], "load_s": round(load_s, 2),
             "crops_per_s": round(len(crops) / dt, 2), "peak_alloc_mb": round(torch.cuda.max_memory_allocated() / 2**20),
             "peak_reserved_mb": round(torch.cuda.max_memory_reserved() / 2**20),
             "device_used_before_mb": round((total - free0) / 2**20), "device_used_peak_mb": round(peak[0] / 2**20)}
    stop.set()
    json.dump({"stats": stats, "texts": texts, "by_name": dict(zip(names, texts))}, open(outp, "w", encoding="utf-8"), ensure_ascii=False)
    print(json.dumps(stats))


main()
