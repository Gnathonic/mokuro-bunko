"""Build the packages of one engine x precision x target.

Layout under ``--out`` (mirrors ``<storage>/models/torch/``):

    <engine>/<precision>/weights-<group>.safetensors      GPU targets: shared by every
                                                          GPU target and by the 3 graphs
    <engine>/<precision>/<target>/{vision,prefill,step}.pt2
    <engine>/<precision>/<target>/build.json              what was built, how, sha256s

Every package is compiled weightless (``package_constants_in_so=False``, no freezing);
at load the runtime binds each package's constants from the weights files
(user-managed, no copy): package metadata ``bunko.weights`` maps every constant FQN of
the package to a key of the weights files. The weights files of an engine x precision
are the same bytes for every target, GPU or CPU (the build checks it).
``TORCH_EXPORT_CPU_EMBED=1`` rebuilds a CPU target in the old layout instead: frozen
(oneDNN/MKL-prepacked weights folded into the graph) with its weights inside, as
``TORCH_EXPORT_WIN_EMBED=1`` does for cross-built Windows GPU targets.

Engines that bind the authors' own weights file (``textbox.PINS``: nar-small,
charspot-small) write no weights files: ``bunko.weights`` maps each constant to its key in
the authors' pinned file, which must hold every constant with the same shape and dtype,
no constant is written into the code (``always_keep_tensor_constants``), and both embed
switches are refused. Engines that bind an ONNX file's initializers (``detector.PINS``:
animetext-blocks, ``bunko.weights_format = onnx``) are built the same way against the
pinned ONNX file; after compiling, every constant the package asks for must be in the map
(``verify.package_constants``).

``build.json`` records the exporter's commit (``exporter_commit``; ``exporter_dirty`` when
tools/torch_export had uncommitted changes); detector packages carry it as
``bunko.exporter_commit`` too, which ``publish_detector.sh`` reads.

Weight groups: ``vision`` (the vision graph's weights) and ``decoder`` (prefill+step,
plus the host loop's tables), each < 1.9 GB (GitHub's asset cap is 2 GiB).
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import subprocess
import sys
import time
from pathlib import Path

from . import IO_VERSION, TOOL_VERSION
from .targets import Target

MAX_FILE_BYTES = 1_900_000_000


def log(*a) -> None:
    print("[torch_export]", *a, file=sys.stderr, flush=True)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while b := f.read(1 << 24):
            h.update(b)
    return h.hexdigest()


def exporter_commit() -> tuple[str, bool]:
    """(commit, dirty) of tools/torch_export: ``TORCH_EXPORT_COMMIT`` / ``TORCH_EXPORT_DIRTY``
    when set (the container has no git checkout), else git; ("unknown", True) without git."""
    if os.environ.get("TORCH_EXPORT_COMMIT"):
        return os.environ["TORCH_EXPORT_COMMIT"], os.environ.get("TORCH_EXPORT_DIRTY", "1") != "0"
    here = Path(__file__).resolve().parents[1]
    try:
        git = ["git", "-C", str(here)]
        commit = subprocess.run([*git, "rev-parse", "HEAD"], capture_output=True, text=True, check=True).stdout.strip()
        status = subprocess.run([*git, "status", "--porcelain", "--", "."], capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return "unknown", True
    return commit, bool(status.strip())


def _canonical(model, extra: dict | None = None) -> dict[int, str]:
    """id(tensor) -> the first name the model gives it (ties keep one name)."""
    out: dict[int, str] = {id(t): n for n, t in (extra or {}).items()}
    for n, t in list(model.named_parameters(remove_duplicate=False)) + list(model.named_buffers(remove_duplicate=False)):
        out.setdefault(id(t), n)
    return out


def _weight_map(graph, canon: dict[int, str], ep) -> dict[str, str]:
    """constant FQN of the exported graph -> canonical (model) name."""
    mod = graph.module
    by_name = dict(mod.named_parameters(remove_duplicate=False))
    by_name.update(dict(mod.named_buffers(remove_duplicate=False)))
    m = {}
    for spec in ep.graph_signature.input_specs:
        if spec.target is None or spec.kind.name not in ("PARAMETER", "BUFFER", "CONSTANT_TENSOR"):
            continue
        t = by_name.get(spec.target)
        if t is None or id(t) not in canon:
            raise SystemExit(f"{graph.role}: constant {spec.target} ({spec.kind.name}) is not a model weight")
        m[spec.target] = canon[id(t)]
    return m


def _group(role: str) -> str:
    return "vision" if role == "vision" else "decoder"


_ST_DTYPE = {"float32": "F32", "bfloat16": "BF16", "float16": "F16", "int64": "I64", "int32": "I32", "bool": "BOOL",
             "uint8": "U8", "int8": "I8", "float64": "F64"}


def save_safetensors(tensors: dict, path: Path, metadata: dict[str, str]) -> None:
    """A byte-reproducible safetensors file (the library's own writer orders the
    metadata map at random). Tensors: widest dtype first, then by name, no holes."""
    import torch

    items = sorted(tensors.items(), key=lambda kv: (-kv[1].element_size(), kv[0]))
    header: dict = {"__metadata__": dict(sorted(metadata.items()))}
    off = 0
    blobs = []
    for k, t in items:
        t = t.detach().contiguous().cpu()
        b = t.view(torch.uint8).numpy().tobytes() if t.numel() else b""
        header[k] = {"dtype": _ST_DTYPE[str(t.dtype).removeprefix("torch.")], "shape": list(t.shape), "data_offsets": [off, off + len(b)]}
        off += len(b)
        blobs.append(b)
    h = json.dumps(header, separators=(",", ":")).encode()
    h += b" " * (-len(h) % 8)
    with open(path, "wb") as f:
        f.write(len(h).to_bytes(8, "little"))
        f.write(h)
        for b in blobs:
            f.write(b)


def write_weights(real_model, maps: dict[str, dict[str, str]], host_weights: dict[str, str], dst: Path, extra: dict | None = None) -> dict:
    """weights-<group>.safetensors from the real (CPU) model; returns {file: info}."""
    named = dict(extra or {})
    for n, t in list(real_model.named_parameters(remove_duplicate=False)) + list(real_model.named_buffers(remove_duplicate=False)):
        named.setdefault(n, t)
    groups: dict[str, dict] = {}
    for role, m in maps.items():
        for key in m.values():
            groups.setdefault(_group(role), {})[key] = named[key]
    for alias, src in host_weights.items():
        groups.setdefault("decoder", {})[src] = named[src]
    info = {}
    dst.mkdir(parents=True, exist_ok=True)
    for g, tensors in sorted(groups.items()):
        p = dst / f"weights-{g}.safetensors"
        meta = {"bunko.tool": TOOL_VERSION, "bunko.group": g}
        if g == "decoder":
            meta.update({f"bunko.alias.{a}": s for a, s in host_weights.items()})
        tmp = p.with_suffix(".tmp")
        save_safetensors(tensors, tmp, meta)
        new = sha256(tmp)
        if p.exists() and sha256(p) != new:
            raise SystemExit(f"{p} exists with different bytes: weights must be identical for every target")
        os.replace(tmp, p)
        size = p.stat().st_size
        if size > MAX_FILE_BYTES:
            raise SystemExit(f"{p}: {size} bytes > {MAX_FILE_BYTES}: shard this group")
        info[p.name] = {"size": size, "sha256": new, "tensors": len(tensors)}
        log(f"  {p.name}: {len(tensors)} tensors, {size / 1e6:.1f} MB")
    return info


def inductor_options(t: Target, role: str, meta: dict[str, str]) -> dict:
    o = {"max_autotune": False, "aot_inductor.metadata": meta}
    if t.gpu or not _cpu_embed():
        # weightless: the constants stay the model's own tensors (no freezing: it folds
        # derived constants -- oneDNN/MKL-prepacked weights on the CPU -- into the graph,
        # which no shared weights file could then provide)
        o.update({
            "freezing": False,
            "aot_inductor.package_constants_in_so": False,
            "aot_inductor.package_constants_on_disk_format": None,
        })
        if t.gpu:
            o["shape_padding"] = False  # pad_mm benchmarks on the device
    else:
        # TORCH_EXPORT_CPU_EMBED=1: frozen, prepacked weights inside the package, out of
        # the shared library (inductor would emit them as a multi-GB C++ byte array)
        o.update({"freezing": True, "aot_inductor.package_constants_on_disk_format": "binary_blob"})
    if t.os == "windows" and t.gpu and _win_embed():
        # GPU: weightless like Linux unless TORCH_EXPORT_WIN_EMBED=1 (in-package blob)
        o["aot_inductor.package_constants_on_disk_format"] = "binary_blob"
    if t.os == "windows" and sys.platform != "win32":
        # cross-compiled with MinGW against the Windows wheel's import libraries; the
        # wrapper calls only libtorch's stable C shim (no C++ ABI shared with MSVC)
        o.update({"aot_inductor.cross_target_platform": "windows",
                  "aot_inductor.precompile_headers": False,  # gch + link flags: clang rejects
                  "aot_inductor.aoti_shim_library_path": os.environ["TORCH_EXPORT_WIN_TORCHLIB"]})
    o.update(t.inductor)
    o.update(json.loads(os.environ.get("TORCH_EXPORT_INDUCTOR_OPTS", "{}")))
    return o


def build(engine: str, precision: str, t: Target, out: Path, io: int = IO_VERSION, precast: bool = True) -> dict:
    import torch

    from . import detector as D
    from . import graphs as G
    from . import textbox as T

    if precision not in t.precisions:
        raise SystemExit(f"{t.name} does not build {precision} (has {t.precisions})")
    if engine in T.PINS:
        T.refuse_embedding(os.environ)  # before anything is built or written
    if engine in D.PINS:
        D.refuse_embedding(os.environ)
    want = f"2.13.0+{t.torch_variant}"
    if t.gpu and torch.__version__ != want:
        raise SystemExit(f"{t.name} needs torch {want}, this is {torch.__version__}")
    if not torch.__version__.startswith("2.13.0"):
        raise SystemExit(f"torch 2.13.0 required, this is {torch.__version__}")
    pdir = out / engine / precision
    tdir = pdir / t.name
    tdir.mkdir(parents=True, exist_ok=True)
    t0 = time.time()
    fuse = os.environ.get("TORCH_EXPORT_FUSE", "1") == "1"
    kw = {"precast": precast, "fuse": fuse}
    build_fn = G.BUILDERS[engine]
    commit, dirty = exporter_commit()
    record: dict = {"engine": engine, "precision": precision, "target": t.name, "io": io, "tool": TOOL_VERSION,
                    "torch": torch.__version__, "precast": precast if engine == "hayai-nova" else None, "fused": fuse and io == 2,
                    "exporter_commit": commit, "exporter_dirty": dirty, "graphs": {}}

    if t.gpu and os.environ.get("TORCH_EXPORT_REAL_DEVICE") == "1":
        # experiment: export + autotune on this machine's real GPU (must BE the target)
        gs = build_fn(precision, torch.device("cuda"), io, **kw)
        with torch.no_grad():
            eps = {g.role: torch.export.export(g.module, g.args, dynamic_shapes=g.dynamic_shapes, strict=False) for g in gs.graphs}
        if gs.authors_weights:
            maps = _authors_maps(gs, eps, gs.authors_file)
        elif gs.onnx_weights:
            maps = _onnx_maps(gs, eps, gs.onnx_file)
        else:
            canon = _canonical(gs.model, gs.extra)
            maps = {g.role: _weight_map(g, canon, eps[g.role]) for g in gs.graphs}
            record["weights"] = write_weights(gs.model, maps, gs.host_weights, pdir, gs.extra)
        record["real_device"] = torch.cuda.get_device_name()
        meta_common = dict(gs.meta)
    elif t.gpu:
        # 1) the real model on the CPU: weights + host tables
        real = build_fn(precision, torch.device("cpu"), io, **kw)
        # 2) a fake copy on the target device for export (no GPU touched)
        from torch._subclasses.fake_tensor import FakeTensorMode

        fm = FakeTensorMode(allow_non_fake_inputs=True)
        # torch.autocast("cuda") silently disables itself when no CUDA device is present:
        # keep it on (the export must trace the target's autocast arithmetic)
        import torch.cuda.amp.common as amp_common

        amp_common.amp_definitely_not_available = lambda: False
        torch.cuda.is_bf16_supported = lambda *a, **k: True
        import copy

        fake_src = copy.deepcopy(real.model)
        with fm:
            fake_src = fake_src.to("cuda")
            gs = build_fn(precision, torch.device("cuda"), io, model=fake_src, **{**kw, "precast": False})
            eps = {g.role: torch.export.export(g.module, g.args, dynamic_shapes=g.dynamic_shapes, strict=False) for g in gs.graphs}
        if gs.authors_weights:
            maps = _authors_maps(gs, eps, real.authors_file)
        elif gs.onnx_weights:
            maps = _onnx_maps(gs, eps, real.onnx_file)
        else:
            canon = _canonical(fake_src, gs.extra)
            maps = {g.role: _weight_map(g, canon, eps[g.role]) for g in gs.graphs}
            record["weights"] = write_weights(real.model, maps, real.host_weights, pdir, real.extra)
        meta_common = {**real.meta}
        win_cross = t.os == "windows" and sys.platform != "win32" and _win_embed()
        if win_cross:
            # libtorch's MSVC-built loader hands load_constants() a std::unordered_map, which a
            # MinGW (libc++) wrapper cannot read: cross-built Windows packages carry their
            # weights as an in-package blob instead (filled from the real weights below)
            named = dict(real.extra)
            for n, tt in list(real.model.named_parameters(remove_duplicate=False)) + list(real.model.named_buffers(remove_duplicate=False)):
                named.setdefault(n, tt)
            real_by_role = {r: {fqn: named[key].detach().contiguous() for fqn, key in m.items()} for r, m in maps.items()}
        # a detector's few non-weight constants (YOLO's strides) get their real values back:
        # inductor reads them to fold them into the graph (the package then holds no such
        # constant, which _check_bound confirms)
        keep = _real_tiny_constants(eps, maps, real.model) if gs.onnx_weights else set()
        del real
        _refake_constants(eps, keep)
        from . import sdpa

        eps = {r: sdpa.pin(ep, t) for r, ep in eps.items()}
        _refake_constants(eps, keep)
        record["sdpa"] = "pinned to the target's rules (sdpa.py)"
        from . import fakegpu

        fakegpu.install(t.backend, t.arch)
    else:
        gs = build_fn(precision, torch.device("cpu"), io, **kw)
        with torch.no_grad():
            eps = {g.role: torch.export.export(g.module, g.args, dynamic_shapes=g.dynamic_shapes, strict=False) for g in gs.graphs}
        if gs.authors_weights:
            maps = _authors_maps(gs, eps, gs.authors_file)
        elif gs.onnx_weights:
            maps = _onnx_maps(gs, eps, gs.onnx_file)
        elif _cpu_embed():
            maps = {g.role: {} for g in gs.graphs}
        else:
            # the same weights files as every GPU target of this engine x precision
            canon = _canonical(gs.model, gs.extra)
            maps = {g.role: _weight_map(g, canon, eps[g.role]) for g in gs.graphs}
            record["weights"] = write_weights(gs.model, maps, gs.host_weights, pdir, gs.extra)
        record["cpu_embed"] = _cpu_embed()
        meta_common = dict(gs.meta)

    if gs.authors_weights:
        pin = gs.authors_weights
        meta_common.update(T.textbox_meta(pin, gs.max_batch, gs.weights_dtype))  # bunko.io = textbox-1 overrides str(io)
        record["io"] = meta_common["io"]
        record["fused"] = None  # the authors' graph as written: no hayai/paddle GEMM fusion
        record["authors_weights"] = {"repo": pin.repo, "revision": pin.revision, "file": pin.file,
                                     "sha256": pin.sha256, "size": pin.size, "cast": gs.weights_dtype}
    elif gs.onnx_weights:
        pin = gs.onnx_weights
        meta_common.update(D.detector_meta(pin, gs.max_batch))  # bunko.io = detect-1 overrides str(io)
        meta_common["exporter_commit"] = commit + ("-dirty" if dirty else "")
        record["io"] = meta_common["io"]
        record["fused"] = None
        record["ultralytics"] = D.ULTRALYTICS_VERSION
        record["onnx_weights"] = {"repo": pin.repo, "revision": pin.revision, "file": pin.file,
                                  "sha256": pin.sha256, "size": pin.size}

    if t.os == "windows" and sys.platform == "win32":
        from . import winpatch

        winpatch.install()

    from torch._inductor import aoti_compile_and_package

    from . import checks

    baseline = checks.libtorch_baseline() if t.os == "linux" else {}
    record["abi_baseline"] = baseline

    win_embed = t.gpu and t.os == "windows" and sys.platform != "win32" and _win_embed()
    if win_embed:
        _real_constants_for_blob()
    for g in gs.graphs:
        path = tdir / f"{g.role}.pt2"
        if win_embed:
            _REAL.clear()
            _REAL.update(real_by_role[g.role])
            maps[g.role] = {}  # weights inside the package: nothing for the runtime to bind
        meta = {
            "bunko.engine": engine, "bunko.role": g.role, "bunko.precision": precision, "bunko.target": t.name,
            "bunko.io": str(io), "bunko.tool": TOOL_VERSION, "bunko.torch": torch.__version__,
            "bunko.weights": json.dumps(maps[g.role], sort_keys=True, separators=(",", ":")),
            **{f"bunko.{k}": v for k, v in meta_common.items()},
        }
        if t.os == "windows" and maps[g.role]:
            # libtorch's MSVC loader passes load_constants() a std::unordered_map the MinGW
            # (libc++) wrapper cannot read: bind through the wrapper's C-ABI export
            # AOTInductorModelContainerUpdateUserManagedConstantBufferPairs instead
            meta["bunko.bind"] = "pairs"
        opts = inductor_options(t, g.role, meta)
        if gs.authors_weights:
            # inductor otherwise writes a 1-D constant of <= 8 values (charspot's heat, box,
            # ruby, pout, start and end biases) into the code as numbers: a weight inside the
            # package, and under fake tensors (GPU targets) no values to write at all. Kept,
            # every constant is bound from the authors' file at load (bunko.weights).
            opts["always_keep_tensor_constants"] = True
        t1 = time.time()
        # the archive's internal folder is named after the file: build as <role>.pt2 in a
        # scratch dir so packages unpack to <role>/... (not <role>.tmp/...)
        (tdir / ".partial").mkdir(exist_ok=True)
        tmp = tdir / ".partial" / path.name
        if gs.onnx_weights:
            with _no_inlined_weights(set(maps[g.role])):
                aoti_compile_and_package(eps[g.role], package_path=str(tmp), inductor_configs=opts)
        else:
            aoti_compile_and_package(eps[g.role], package_path=str(tmp), inductor_configs=opts)
        problems = []
        execstack_fixed = 0
        if t.os == "linux":
            execstack_fixed = checks.clear_execstack(tmp)
            problems += checks.execstack_problems(tmp)
            need, bad = checks.abi_report(tmp, baseline)
            problems += [f"needs {b} (libtorch's own baseline)" for b in bad]
        else:
            need = {}
        if t.os == "windows" and sys.platform != "win32":
            checks.windows_extension(tmp)
        if t.os == "macos":
            checks.macos_relocatable(tmp)
        if t.os in ("linux", "macos"):
            problems += checks.host_path_problems(tmp)
        if t.arch == "x86-64-v3" or t.gpu:
            z = sum(checks.zmm_count(tmp).values())
            if z:
                problems.append(f"{z} AVX-512 (zmm) instructions in an x86-64-v3 package")
        n_consts = len(maps[g.role])
        if gs.onnx_weights:
            n_consts = _check_bound(tmp, maps[g.role], gs.onnx_weights)
        if problems and os.environ.get("TORCH_EXPORT_SKIP_CHECKS") != "1":
            raise SystemExit(f"{t.name}/{g.role}: not portable: {'; '.join(problems)} "
                             "(build Linux packages in the container: python -m torch_export build ... without --host)")
        checks.normalize(tmp)
        os.replace(tmp, path)
        (tdir / ".partial").rmdir()
        record["graphs"][g.role] = {"abi_needs": need, "check_problems": problems, "execstack_cleared": execstack_fixed,"file": path.name, "size": path.stat().st_size, "sha256": sha256(path),
                                     "compile_s": round(time.time() - t1, 1), "constants": n_consts,
                                     "inductor": {k: v for k, v in opts.items() if k != "aot_inductor.metadata"}}
        log(f"  {t.name}/{path.name}: {path.stat().st_size / 1e6:.1f} MB in {time.time() - t1:.0f}s")
    record["total_s"] = round(time.time() - t0, 1)
    (tdir / "build.json").write_text(json.dumps(record, indent=1, default=str) + "\n")
    return record


def _authors_maps(gs, eps: dict, path: Path | None) -> dict[str, dict[str, str]]:
    """role -> {constant FQN: key of the authors' weights file}; refuses a constant the
    pinned file does not hold with the same shape and dtype (so no weight stays inside),
    or, for a package the host binds cast (``gs.weights_dtype``), with the same shape and
    the dtype of that cast."""
    import torch
    from safetensors import safe_open

    from .textbox import CASTS, authors_weight_map, check_pin, refuse_embedding

    refuse_embedding(os.environ)
    pin = gs.authors_weights
    if path is None:
        raise SystemExit(f"{gs.engine}: the builder did not say where the authors' {pin.file} is")
    check_pin(path, pin)
    with safe_open(str(path), framework="pt") as f:
        stored = {k: (tuple(f.get_slice(k).get_shape()), f.get_slice(k).get_dtype()) for k in f.keys()}
    maps = {}
    for g in gs.graphs:
        if g.role not in pin.prefix:
            raise SystemExit(f"{gs.engine}/{g.role}: no FQN prefix for this role in the pin")
        prefix, ep = pin.prefix[g.role], eps[g.role]
        consts = {}
        for spec in ep.graph_signature.input_specs:
            if spec.target is None or spec.kind.name not in ("PARAMETER", "BUFFER", "CONSTANT_TENSOR"):
                continue
            consts[spec.target] = ep.state_dict.get(spec.target, ep.constants.get(spec.target))
        wanted = {fqn[len(prefix):]: (tuple(t.shape), _ST_DTYPE[str(t.dtype).removeprefix("torch.")])
                  for fqn, t in consts.items() if fqn.startswith(prefix) and isinstance(t, torch.Tensor)}
        cast = CASTS[gs.weights_dtype] if gs.weights_dtype is not None else None
        maps[g.role] = authors_weight_map(list(consts), prefix, stored, wanted, cast=cast)
        how = f", cast to {cast} at load" if cast else ""
        log(f"  {g.role}: {len(maps[g.role])} constants bound from the authors' {pin.file} ({pin.repo}@{pin.revision[:8]}){how}")
    return maps


def _onnx_maps(gs, eps: dict, path: Path | None) -> dict[str, dict[str, str]]:
    """role -> {constant FQN: ONNX initializer name}; refuses a constant the pinned ONNX
    file does not hold as a FLOAT raw initializer of the same shape. A lifted tensor
    constant that is no initializer (YOLO's ``stride``, three numbers the graph folds) is
    left out of the map; ``_check_bound`` refuses the package if it asks for one."""
    from .detector import PREFIX, check_pin, onnx_initializers, onnx_weight_map, refuse_embedding

    refuse_embedding(os.environ)
    pin = gs.onnx_weights
    if path is None:
        raise SystemExit(f"{gs.engine}: the builder did not say where {pin.file} is")
    check_pin(path, pin)
    inits = onnx_initializers(path)
    maps = {}
    for g in gs.graphs:
        ep = eps[g.role]
        shapes, skipped = {}, []
        for spec in ep.graph_signature.input_specs:
            if spec.target is None or spec.kind.name not in ("PARAMETER", "BUFFER", "CONSTANT_TENSOR"):
                continue
            t = ep.state_dict.get(spec.target, ep.constants.get(spec.target))
            key = spec.target[len(PREFIX):] if spec.target.startswith(PREFIX) else None
            if spec.kind.name == "CONSTANT_TENSOR" and key not in inits and t is not None and t.numel() <= 16:
                skipped.append(spec.target)
                continue
            shapes[spec.target] = tuple(t.shape)
        maps[g.role] = onnx_weight_map(list(shapes), PREFIX, inits, shapes)
        log(f"  {g.role}: {len(maps[g.role])} constants bound from {pin.file} ({pin.repo}@{pin.revision[:8]})"
            + (f"; not weights: {skipped}" if skipped else ""))
    return maps


@contextlib.contextmanager
def _no_inlined_weights(bound: set[str]):
    """Inductor inlines a 1-d constant of at most 8 elements into the generated code
    (``GraphLowering.can_inline_constant``): for a weight that would put its values inside
    the package (YOLO's one-class ``cv3.*.2.bias``), and under the fake device it has no
    values to read. While this is active, every constant named in ``bound`` stays a
    package constant, bound at load like the others. Pinned to torch 2.13's
    ``GraphLowering.get_attr``."""
    import torch
    from torch._inductor import graph as ig

    orig = ig.GraphLowering.get_attr

    def get_attr(self, target, args, kwargs):
        if target in bound:
            value = ig.getattr_recursive(self.module, target)
            if isinstance(value, torch.Tensor):
                return self.add_tensor_constant(value, target)
        return orig(self, target, args, kwargs)

    ig.GraphLowering.get_attr = get_attr
    try:
        yield
    finally:
        ig.GraphLowering.get_attr = orig


def _check_bound(pt2: Path, wmap: dict[str, str], pin) -> int:
    """The package's constants, all bound from the ONNX file by ``wmap``: refuses a
    package that asks for a constant outside the map (it would hold it inside or fail to
    load) or carries its weights inside. Returns how many constants it binds."""
    from .verify import package_constants, weights_inside

    fqns = package_constants(pt2)
    unbound = sorted(set(fqns) - set(wmap))
    if unbound:
        raise SystemExit(f"{pt2}: constants without an ONNX initializer: {unbound[:5]} ({len(unbound)})")
    if weights_inside(pt2, pin.size):
        raise SystemExit(f"{pt2}: weights inside the package")
    return len(fqns)


def _real_tiny_constants(eps: dict, maps: dict[str, dict[str, str]], real_model, most: int = 16) -> set[str]:
    """Put the real (CPU) value back into each lifted tensor constant that is no weight
    (not in its role's map) and has at most ``most`` elements: under the fake mode it has
    no data, and inductor reads such a constant to fold it. The FQN is
    ``<prefix><attribute path on the model>``; returns the names set."""
    import torch

    from .detector import PREFIX

    done = set()
    for r, ep in eps.items():
        for k, v in list(ep.constants.items()):
            if k in maps[r] or not isinstance(v, torch.Tensor) or v.numel() > most:
                continue
            if not k.startswith(PREFIX):
                raise SystemExit(f"{r}: constant {k} is not under {PREFIX!r}")
            val = real_model
            for part in k[len(PREFIX):].split("."):
                val = val[int(part)] if part.isdigit() else getattr(val, part)
            if not isinstance(val, torch.Tensor) or tuple(val.shape) != tuple(v.shape) or val.dtype != v.dtype:
                raise SystemExit(f"{r}: constant {k}: no real tensor of shape {tuple(v.shape)} on the model")
            ep.constants[k] = val.detach().clone()
            done.add(k)
    return done


def _refake_constants(eps: dict, keep: set[str] = frozenset()) -> None:
    """Export puts the example inputs in its own FakeTensorMode; move the (fake) weights
    into that mode too, or inductor sees two modes. Constants named in ``keep`` stay as
    they are."""
    import torch
    from torch._subclasses.fake_tensor import FakeTensor

    for ep in eps.values():
        mode = next(n.meta["val"].fake_mode for n in ep.graph.nodes
                    if n.op == "placeholder" and isinstance(n.meta.get("val"), FakeTensor))

        def refake(v):
            meta = torch.empty_strided(v.shape, v.stride(), dtype=v.dtype, device="meta")
            return FakeTensor(mode, meta, v.device)

        for k in list(ep.state_dict):
            v = ep.state_dict[k]
            ep.state_dict[k] = torch.nn.Parameter(refake(v), requires_grad=False) if isinstance(v, torch.nn.Parameter) else refake(v)
        for k in list(ep.constants):
            if isinstance(ep.constants[k], torch.Tensor) and k not in keep:
                ep.constants[k] = refake(ep.constants[k])


_REAL: dict = {}


def _real_constants_for_blob() -> None:
    """Serialize the real (CPU) weights where inductor would read its FakeTensor constants."""
    from torch._inductor.graph import GraphLowering

    orig = GraphLowering.get_original_value_of_constant

    def real_value(self, name):
        # only the blob writer (codecache's per-constant `_worker`) gets the real bytes;
        # codegen keeps seeing the (fake) device tensor so constants stay on the GPU
        if sys._getframe(1).f_code.co_name != "_worker":
            return orig(self, name)
        fqn = self.allocated_constant_name[name]
        if fqn in _REAL:
            return _REAL[fqn]
        raise SystemExit(f"windows blob: constant {name} ({fqn}) has no real weight; known e.g. {list(_REAL)[:3]}")

    GraphLowering.get_original_value_of_constant = real_value


def _cpu_embed() -> bool:
    """CPU targets: frozen with the (prepacked) weights inside the package (the
    pre-2026-10-08 layout) instead of binding the shared weights files."""
    return os.environ.get("TORCH_EXPORT_CPU_EMBED") == "1"


def _win_embed() -> bool:
    """Cross-built Windows GPU packages: weights inside (the pre-2026-10-03 layout) instead of
    shared weights bound through the C-ABI Pairs entry point."""
    return os.environ.get("TORCH_EXPORT_WIN_EMBED") == "1"
