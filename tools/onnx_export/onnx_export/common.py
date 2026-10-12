"""Shared plumbing: where things live, how an ONNX file is written, ORT sessions."""

from __future__ import annotations

import hashlib
import json
import os
import sys
from pathlib import Path
from typing import Any

from . import EXPORT_TOOL_VERSION, RELEASE_TAG
from .pins import Source

# The 0.5.2 Python sources (the reference runner and the pinned revisions) are no longer
# in this tree. Point MOKURO_REF_052 at a checkout of commit 199cff5 (release 0.5.2), e.g.
#   git worktree add --detach ../ref-0.5.2 199cff5
# The default is that sibling worktree.
_THIS_REPO = Path(__file__).resolve().parents[3]
REPO_ROOT = Path(os.environ.get("MOKURO_REF_052", _THIS_REPO.parent / "ref-0.5.2"))
if not (REPO_ROOT / "src/mokuro_bunko/ocr/engine_runner.py").is_file():
    raise SystemExit(
        f"0.5.2 reference sources not found at {REPO_ROOT}: set MOKURO_REF_052 to a checkout of "
        "commit 199cff5 (git worktree add --detach ../ref-0.5.2 199cff5)"
    )
DEFAULT_OUT = Path(os.environ.get("MOKURO_MODELS_OUT", Path.home() / ".cache/mokuro-bunko-demo/models-v1"))
SPIKE = Path.home() / ".cache/mokuro-bunko-demo/onnx-spike"
DEFAULT_HAYAI_CROPS = SPIKE / "hayai"  # crops/ + crops.json (220 line crops)
DEFAULT_PADDLE_CROPS = SPIKE / "paddle/crops"  # *.png + index.json (100 upright crops)

# GitHub release assets are capped at 2 GiB; stay well under it.
MAX_FILE_BYTES = 1_900_000_000
# Tensors at least this big go to the external data file (ONNX's own default).
EXTERNAL_THRESHOLD = 1024


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def sha256(path: Path, chunk: int = 1 << 24) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while block := f.read(chunk):
            h.update(block)
    return h.hexdigest()


def stage_dir(out: Path) -> Path:
    """Scratch space for raw exporter output, inside the output dir (same filesystem)."""
    d = out / "_stage"
    d.mkdir(parents=True, exist_ok=True)
    return d


def provenance(engine: str, role: str, precision: str, sources: tuple[Source, ...], extra: dict | None = None) -> dict[str, str]:
    """``metadata_props`` stamped into every exported graph (readable with ``ort`` at load)."""
    meta = {
        "mokuro.export_id": f"{RELEASE_TAG}/{engine}/{role}/{precision}",
        "mokuro.export_tool_version": EXPORT_TOOL_VERSION,
        "mokuro.engine": engine,
        "mokuro.role": role,
        "mokuro.precision": precision,
        "mokuro.sources": json.dumps({s.repo: s.revision for s in sources}, sort_keys=True),
    }
    meta.update(extra or {})
    return meta


def write_onnx(model: Any, path: Path, *, meta: dict[str, str], external: bool, max_bytes: int = MAX_FILE_BYTES) -> list[Path]:
    """Write ``model`` to ``path`` under its final (flat) release name; return every file written.

    ``external``: weights go to ``<name>.data`` beside it (``<name>.data.1``,
    ``.2``... once a file would pass ``max_bytes``), with the location recorded
    as the bare file name so the set loads from any directory it is copied to.
    Tensors are laid out in initializer order, 64-byte aligned, so the same
    graph always produces the same bytes.
    """
    import onnx
    from onnx.external_data_helper import uses_external_data

    del model.metadata_props[:]
    for k, v in sorted(meta.items()):
        model.metadata_props.add(key=k, value=v)
    path.parent.mkdir(parents=True, exist_ok=True)
    for stale in path.parent.glob(path.name + ".data*"):
        stale.unlink()
    written = [path]
    if external:
        handles: list[Any] = []
        sizes: list[int] = []

        def target(nbytes: int) -> int:
            if not handles or (sizes[-1] and sizes[-1] + nbytes > max_bytes):
                n = len(handles)
                p = path.parent / (path.name + ".data" + (f".{n}" if n else ""))
                handles.append(open(p, "wb"))
                sizes.append(0)
                written.append(p)
            return len(handles) - 1

        try:
            for t in _all_tensors(model):
                if uses_external_data(t):
                    raise ValueError(f"{t.name}: load the model with its external data first")
                if not t.HasField("raw_data") or len(t.raw_data) < EXTERNAL_THRESHOLD:
                    continue
                raw = t.raw_data
                i = target(len(raw))
                pad = (-sizes[i]) % 64
                handles[i].write(b"\0" * pad)
                sizes[i] += pad
                offset = sizes[i]
                handles[i].write(raw)
                sizes[i] += len(raw)
                del t.external_data[:]
                for k, v in (("location", Path(handles[i].name).name), ("offset", str(offset)), ("length", str(len(raw)))):
                    t.external_data.add(key=k, value=v)
                t.data_location = onnx.TensorProto.EXTERNAL
                t.ClearField("raw_data")
        finally:
            for h in handles:
                h.close()
    data = model.SerializeToString()
    if len(data) > max_bytes:
        raise ValueError(f"{path.name}: {len(data)} bytes in the graph file itself; export with external=True")
    path.write_bytes(data)
    for p in written:
        if p.stat().st_size > max_bytes:
            raise ValueError(f"{p.name} is {p.stat().st_size} bytes, over the {max_bytes} byte cap")
    return written


def _all_tensors(model: Any):
    from onnx.external_data_helper import _get_all_tensors

    return _get_all_tensors(model)


def load_onnx(path: Path) -> Any:
    import onnx

    return onnx.load(str(path), load_external_data=True)


def ort_session(path: Path, threads: int = 0) -> Any:
    import onnxruntime as ort

    so = ort.SessionOptions()
    so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_ENABLE_ALL
    if threads:
        so.intra_op_num_threads = threads
    so.inter_op_num_threads = 1
    return ort.InferenceSession(str(path), so, providers=["CPUExecutionProvider"])


def import_runner(repo_root: Path = REPO_ROOT) -> Any:
    """The 0.5.2 runner module (the torch reference the exports are checked against)."""
    ocr = str(repo_root / "src" / "mokuro_bunko" / "ocr")
    if ocr not in sys.path:
        sys.path.insert(0, ocr)
    import engine_runner  # noqa: PLC0415

    return engine_runner


def snapshot_file(source: Source, filename: str) -> Path:
    from huggingface_hub import hf_hub_download

    return Path(hf_hub_download(source.repo, filename, revision=source.revision))


def write_json(path: Path, obj: Any) -> None:
    path.write_text(json.dumps(obj, ensure_ascii=False, indent=1, sort_keys=False) + "\n", encoding="utf-8")


def exact(ref: list[str], hyp: list[str]) -> tuple[int, list[tuple[int, str, str]]]:
    diffs = [(i, a, b) for i, (a, b) in enumerate(zip(ref, hyp, strict=True)) if a != b]
    return len(ref) - len(diffs), diffs


def edit_distance(a: str, b: str) -> int:
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]
