"""The AnimeText block detector (YOLO12-x) as one weightless package bound to the ONNX file.

docs/rust-port/TEXTBOX-GPU.md §7.5. The graph is ultralytics' own ``DetectionModel``
("yolo12x.yaml", one class), fused as the ONNX file was exported, with the detect head in
export mode (``[B, 5, N]`` candidates, no NMS: the host's ``blocks_from_output`` does the
rest). Every constant of the package is an initializer of the mirrored
``animetext_yolo12x.onnx`` under its own name, which the runtime binds at load
(``bunko.weights`` maps FQN -> initializer name, ``bunko.weights_format = onnx``). Two
names differ from the state dict: the ONNX export folded each A2C2f residual scale
``gamma`` (``[C]``) into a ``[1, C, 1, 1]`` initializer; the graph keeps ``gamma`` in that
shape (``x + gamma * y``) so those initializers bind byte for byte (``GAMMA_ALIASES``).

ultralytics is a build-time dependency only (``requirements-detector.txt``, installed with
``pip install --no-deps --target <dir>``); put that directory on ``PYTHONPATH`` or in
``TORCH_EXPORT_DETECTOR_PYLIB``. The builder refuses any other ultralytics than
``ULTRALYTICS_VERSION``, any ONNX file but the pinned one (``TORCH_EXPORT_ANIMETEXT_ONNX``,
else the Hugging Face cache at the pin), an initializer that is not FLOAT ``raw_data``,
and any precision but fp32.

The package is compiled Ultralytics model code (AGPL-3.0) and binds GPL-3.0 weights: it
ships only in the pre-release ``detector-torch-v1`` for ``gpl-models`` builds.
"""

from __future__ import annotations

import os
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Mapping

import torch
from torch import nn
from torch.export import Dim

from .graphs import Graph, Graphs

IO = "detect-1"  # package I/O id (bunko.io), checked by the cdylib's GraphSet::load
MAX_BATCH = 8  # the package's largest batch (bunko.max_batch)
STRIDE, MAX_CELLS = 32, 20  # canvas sides: 32 x 1..20 (at most 640)
PREFIX = "net."  # AnimeTextGraph.net: the FQN prefix before each state-dict key
ONNX_VAR = "TORCH_EXPORT_ANIMETEXT_ONNX"
PYLIB_VAR = "TORCH_EXPORT_DETECTOR_PYLIB"
EMBED_VARS = ("TORCH_EXPORT_WIN_EMBED", "TORCH_EXPORT_CPU_EMBED")
FLOAT = 1  # onnx TensorProto.FLOAT

# Step 0 of plan 04 Task 3 (2026-11-13): under 8.3.105 (the ONNX file's own producer) the
# 425 state-dict tensors map to the initializers as under 8.4.212, but its fuse() leaves 410
# fused conv weights as non-leaf tensors that require grad, and aoti_compile_and_package
# (torch 2.13) stops with "Cannot assign non-leaf Tensor to parameter 'weight'". 8.4.212
# is the version the spike measured (422 package constants, identical block sets 30/30
# at b1, b4 and b8 on the RX 9070 XT).
ULTRALYTICS_VERSION = "8.4.212"

# A2C2f residual scales: state-dict key -> the ONNX initializer it was folded into
GAMMA_ALIASES = {"model.6.gamma": "onnx::Mul_2760", "model.8.gamma": "onnx::Mul_2777"}


@dataclass(frozen=True)
class OnnxPin:
    """The ONNX file a package binds, pinned by source commit, sha256 and size (a ``Pin``
    for ``manifest.build_textbox_manifest``: it has ``repo`` and ``revision``)."""

    repo: str
    revision: str
    file: str
    sha256: str
    size: int


ANIMETEXT_PIN = OnnxPin(repo="deepghs/AnimeText_yolo", revision="a180c191bfdb9f0e31b57e7de567e7b6bac50f84",
                        file="animetext_yolo12x.onnx",
                        sha256="5dd2bb9f3bd1562d4a2207e38ccbfa4c81b8161552b3120cd0f7936399fe1fdd", size=236662175)
# the file's path in the source repository (the mirror renames it to ANIMETEXT_PIN.file)
ANIMETEXT_REPO_PATH = "yolo12x_animetext/model.onnx"

# engines whose packages bind an ONNX file's initializers (build.py, verify.py)
PINS: dict[str, OnnxPin] = {"animetext-blocks": ANIMETEXT_PIN}


def onnx_weight_map(fqns: list[str], prefix: str, inits: Mapping[str, tuple[tuple[int, ...], int, bool]],
                    shapes: Mapping[str, tuple[int, ...]] | None = None) -> dict[str, str]:
    """constant FQN -> ONNX initializer name (the FQN minus ``prefix``, or its
    ``GAMMA_ALIASES`` entry).

    ``inits``: the ONNX file's initializers, name -> (shape, data_type, held as
    ``raw_data``); ``shapes`` (optional): the graph's shape of each FQN. Refuses a
    constant with no initializer, one that is not FLOAT, one not held as raw bytes in the
    file (external data, or typed fields such as ``float_data``, which the runtime does
    not read) and one whose shape differs: anything else would leave a weight inside the
    package or bind the wrong bytes."""
    out = {}
    for fqn in fqns:
        if not fqn.startswith(prefix):
            raise SystemExit(f"{fqn} is not under {prefix!r}: a constant that is not one of the model's weights")
        key = fqn[len(prefix):]
        name = GAMMA_ALIASES.get(key, key)
        if name not in inits:
            raise SystemExit(f"{fqn}: the ONNX file has no initializer {name}")
        shape, dtype, raw = inits[name]
        if dtype != FLOAT:
            raise SystemExit(f"{fqn}: initializer {name} is not FLOAT (data_type {dtype})")
        if not raw:
            raise SystemExit(f"{fqn}: initializer {name} is not held as raw_data in the file")
        if shapes is not None:
            want = tuple(shapes[fqn])
            if tuple(shape) != want:
                raise SystemExit(f"{fqn}: shape {want} in the graph, {tuple(shape)} for {name} in the ONNX file")
        out[fqn] = name
    return out


def onnx_initializers(path: Path) -> dict[str, tuple[tuple[int, ...], int, bool]]:
    """name -> (shape, data_type, held as raw_data) of every initializer, without reading
    any external data file."""
    import onnx

    m = onnx.load(str(path), load_external_data=False)
    out = {}
    for t in m.graph.initializer:
        raw = t.HasField("raw_data") and t.data_location != onnx.TensorProto.EXTERNAL
        if t.name in out:
            raise SystemExit(f"{path}: initializer {t.name} twice")
        out[t.name] = (tuple(int(d) for d in t.dims), int(t.data_type), raw)
    return out


def onnx_arrays(path: Path, names) -> dict:
    """name -> numpy array of the named initializers (a copy, writable)."""
    import onnx
    from onnx import numpy_helper

    want = set(names)
    m = onnx.load(str(path), load_external_data=False)
    out = {t.name: numpy_helper.to_array(t).copy() for t in m.graph.initializer if t.name in want}
    missing = sorted(want - set(out))
    if missing:
        raise SystemExit(f"{path}: no initializers {missing[:5]}")
    return out


def refuse_embedding(env: Mapping[str, str]) -> None:
    """The detector's weights (GPL-3.0, the authors') never go inside a package."""
    for k in EMBED_VARS:
        if env.get(k, "") not in ("", "0"):
            raise SystemExit(f"{k}={env[k]}: detector packages are weightless; "
                             "the ONNX file's weights are never put inside a package")


def detector_meta(pin: OnnxPin, max_batch: int = MAX_BATCH) -> dict[str, str]:
    """Package metadata (build.py adds the ``bunko.`` prefix)."""
    return {"io": IO, "weights_format": "onnx", "weights_sha256": pin.sha256, "weights_size": str(pin.size),
            "source": f"{pin.repo}@{pin.revision}", "ultralytics": ULTRALYTICS_VERSION, "max_batch": str(max_batch)}


def onnx_path(env: Mapping[str, str] | None = None) -> Path:
    """The pinned ONNX file: ``TORCH_EXPORT_ANIMETEXT_ONNX``, else the Hugging Face cache at
    the pin (``HF_HOME``, default ~/.cache/huggingface). Not checked here (``check_pin``)."""
    env = os.environ if env is None else env
    if env.get(ONNX_VAR):
        return Path(env[ONNX_VAR])
    hf = Path(env.get("HF_HOME", Path.home() / ".cache" / "huggingface"))
    repo = "models--" + ANIMETEXT_PIN.repo.replace("/", "--")
    p = hf / "hub" / repo / "snapshots" / ANIMETEXT_PIN.revision / ANIMETEXT_REPO_PATH
    if not p.is_file():
        raise SystemExit(f"no AnimeText ONNX file: set {ONNX_VAR} to the mirrored {ANIMETEXT_PIN.file} "
                         f"({ANIMETEXT_PIN.repo}@{ANIMETEXT_PIN.revision} {ANIMETEXT_REPO_PATH}); not at {p}")
    return p


def check_pin(path: Path, pin: OnnxPin = ANIMETEXT_PIN) -> None:
    """Refuse any file but the pinned one (size first: it is free)."""
    from .textbox import check_pin as check

    check(Path(path), pin)


def _ultralytics():
    """The pinned ultralytics, imported offline and without touching the user's settings."""
    pylib = os.environ.get(PYLIB_VAR)
    if pylib and pylib not in sys.path:
        sys.path.insert(0, pylib)
    os.environ.setdefault("YOLO_OFFLINE", "1")
    # ultralytics writes (and rewrites) a settings file on import: keep it out of $HOME
    os.environ.setdefault("YOLO_CONFIG_DIR", str(Path(tempfile.gettempdir()) / "torch_export-ultralytics"))
    try:
        import ultralytics
    except ImportError:
        raise SystemExit(f"ultralytics {ULTRALYTICS_VERSION} is not importable: install requirements-detector.txt "
                         f"with pip install --no-deps --target DIR, then PYTHONPATH=DIR or {PYLIB_VAR}=DIR") from None
    if ultralytics.__version__ != ULTRALYTICS_VERSION:
        raise SystemExit(f"ultralytics {ultralytics.__version__} is installed; the detector is built with "
                         f"{ULTRALYTICS_VERSION} only ({ultralytics.__file__})")
    return ultralytics


def _a2c2f_forward(self, x):
    """A2C2f.forward with ``gamma`` already ``[1, C, 1, 1]`` (ultralytics views its ``[C]``
    gamma per call: a model being constructed, before ``build_model`` reshapes it, still
    has that one, and gets the same view)."""
    y = [self.cv1(x)]
    y.extend(m(y[-1]) for m in self.m)
    y = self.cv2(torch.cat(y, 1))
    if self.gamma is None:
        return y
    g = self.gamma if self.gamma.dim() == 4 else self.gamma.view(-1, self.gamma.shape[0], 1, 1)
    return x + g * y


def _patch_a2c2f():
    from ultralytics.nn.modules.block import A2C2f

    A2C2f.forward = _a2c2f_forward
    return A2C2f


def build_model() -> nn.Module:
    """YOLO12-x, one class, fused, every A2C2f ``gamma`` a ``[1, C, 1, 1]`` parameter, no
    parameter requiring grad (random weights: ``load_onnx_weights`` fills them)."""
    _ultralytics()
    from ultralytics.nn.tasks import DetectionModel

    a2c2f = _patch_a2c2f()
    m = DetectionModel("yolo12x.yaml", nc=1, verbose=False).eval()
    m.fuse(verbose=False)
    for mod in m.modules():
        if isinstance(mod, a2c2f) and getattr(mod, "gamma", None) is not None:
            mod.gamma = nn.Parameter(mod.gamma.detach().reshape(1, -1, 1, 1).clone(), requires_grad=False)
    for p in m.parameters():
        p.requires_grad_(False)
    _head_for_export(m)
    return m


def _head_for_export(m: nn.Module) -> None:
    det = m.model[-1]
    det.export, det.dynamic, det.format = True, True, "onnx"


def load_onnx_weights(m: nn.Module, path: Path) -> dict[str, str]:
    """Load every state-dict tensor strictly from the ONNX initializer of the same name
    (the gammas from their aliases); returns the key -> initializer map."""
    sd = m.state_dict()
    inits = onnx_initializers(path)
    wm = onnx_weight_map([PREFIX + k for k in sd], PREFIX, inits, {PREFIX + k: tuple(v.shape) for k, v in sd.items()})
    arrays = onnx_arrays(path, wm.values())
    m.load_state_dict({fqn[len(PREFIX):]: torch.from_numpy(arrays[name]) for fqn, name in wm.items()}, strict=True)
    return {fqn[len(PREFIX):]: name for fqn, name in wm.items()}


class AnimeTextGraph(nn.Module):
    """images ``[B, 3, H, W]`` f32 0..1 (RGB, the host's letterboxed canvas) -> candidates
    ``[B, 5, N]`` (centre x, centre y, width, height, score in canvas pixels)."""

    def __init__(self, net: nn.Module):
        super().__init__()
        self.net = net

    def forward(self, images):
        out = self.net(images)
        return out[0] if isinstance(out, (tuple, list)) else out


def animetext_blocks(prec: str, device: torch.device, io: int, model=None, **kw) -> Graphs:
    """AnimeText YOLO12-x as one graph (role ``detector``), batch 1..MAX_BATCH, canvas
    sides 32 x 1..20.

    ``model`` None: built with the pinned ultralytics and loaded from the pinned ONNX file
    (``onnx_path``, checked). ``io`` and the hayai/paddle options in ``kw`` do not apply
    (the package's I/O id is ``detect-1``)."""
    if prec != "fp32":
        raise SystemExit(f"animetext-blocks: no {prec} build (fp32 only)")
    _ultralytics()
    _patch_a2c2f()  # also for a model handed in (a copy of one built here)
    # TF32 off, as the spike and the runtime (bt_init) run it
    torch.backends.cuda.matmul.allow_tf32 = False
    torch.backends.cudnn.allow_tf32 = False
    path = None
    if model is None:
        path = onnx_path()
        check_pin(path)
        model = build_model()
        load_onnx_weights(model, path)
    _head_for_export(model)
    p = next(model.parameters())
    if p.device.type != device.type:
        model = model.to(device)
    wrap = AnimeTextGraph(model).eval()
    ex = (torch.full((2, 3, 640, 448), 114.0 / 255.0, device=device),)
    B = Dim("b", min=1, max=MAX_BATCH)
    H, W = Dim("hc", min=1, max=MAX_CELLS), Dim("wc", min=1, max=MAX_CELLS)
    graph = Graph("detector", wrap, ex, ({0: B, 2: STRIDE * H, 3: STRIDE * W},))
    return Graphs("animetext-blocks", prec, io, model, [graph], max_batch=MAX_BATCH,
                  onnx_weights=ANIMETEXT_PIN, onnx_file=path)
