"""Export manga-ocr-nar-small (muscgab, branch r16) as an ONNX graph that holds no
trained weights: every weight entry of the graph points at its byte range inside the
author's unmodified `model.safetensors`, which the program fetches from the author's
repository at the pinned commit. Only the graph is ours to publish (release
`textbox-graphs-v1`); the weights are never rewritten or rehosted.

How: the author's own model code (Apache-2.0, `manga_ocr_nar_small/model.py` at the pin)
is loaded with the weights and the RMSNorm eps values of `config.json`
(`reparam.norm_eps`, which the author's `load` applies to each module), and traced with
its `prepare()` step inside the graph, so every constant derived from the weights is
computed at run time from the weight entries. It is exported without constant folding
and without merging initializers of equal value (the exporter would otherwise fold the
three untrained, all-ones `cond.{1,2,3}.s3` into `cond.0.s3`, a graph that holds only
for these values), then each initializer is turned into an external entry: location
`model.safetensors`, offset = 8 + header length + the tensor's start in the safetensors
header.

Checks (any failure stops the export):
  * the pinned files have their pinned sha256;
  * every initializer maps onto one safetensors tensor of the same dtype and shape: no
    weight is left inside the graph (181 external entries, one per tensor the code
    reads; the file's 4 tensors the code never reads are not referenced);
  * the inputs and output are the ones Bunko feeds (P [324, 588], V [324] f32;
    R, C [324], H, W [1] int32; ids [161] int64);
  * the graph does not depend on the weights' values: exported again from the same
    network with every parameter replaced by random numbers, it is byte for byte the
    same file (so its built-in constants, the RoPE tables, come from the code alone).

The output is deterministic: two runs give the same bytes (see README.md).

usage: python export_nar.py [--snapshot DIR] --out DIR
       (default snapshot: the Hugging Face cache, `HF_HUB_OFFLINE=1` works once fetched)
"""
from __future__ import annotations

import argparse
import copy
import hashlib
import io
import json
import math
import struct
import sys
from pathlib import Path

REPO = "muscgab/manga-ocr-nar-small"
REVISION = "8cc14c152ccb21da3aa88fa51a92766537c12da2"  # branch r16
GRAPH_NAME = "nar-small_r16_graph.onnx"
WEIGHTS_LOCATION = "model.safetensors"  # the name the graph's external entries use
# sha256 of the files the export reads, from the repository's own SHA256SUMS at the pin.
PINS = {
    "model.safetensors": "bc7418e27d476f86bb5984b45974d69cf9f7623eff6b9b57012df2c8fc9920a3",
    "config.json": "9a55c454e4a54cfa807ae58ba0f1025ffd2f1f5cfa775faeadc3b3f693f876c3",
    "nar_vocabulary_6817.json": "9f1059427b54216c869dbf1244157f86bef74b1a0061c46a6001402c6f0c1dbf",
    "manga_ocr_nar_small/model.py": "0912975a39f41fe15cf6a1abb83726b8965f58d0e3e44dbe9a2ce3a5cea0f585",
    "manga_ocr_nar_small/preprocess.py": "5d7f638a035c711086e84041774ab9bce41a3554aa17c1fe7c6054bb0bfc1681",
    "LICENSE": "cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30",
    "LICENSE_MODEL": "e66c269d4819aaab34b49ef5220c4ddab6756f21bb5180761a4eb8561f2b7bbd",
}
WEIGHTS_SIZE = 217_035_140
EXTERNAL_ENTRIES = 181
SLOTS = 161
INPUTS = [("P", [324, 588], "FLOAT"), ("V", [324], "FLOAT"), ("R", [324], "INT32"),
          ("C", [324], "INT32"), ("H", [1], "INT32"), ("W", [1], "INT32")]
OUTPUT = ("ids", [SLOTS], "INT64")
OPSET = 17


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def snapshot_dir(arg: str | None) -> Path:
    if arg:
        return Path(arg)
    from huggingface_hub import snapshot_download
    return Path(snapshot_download(REPO, revision=REVISION, allow_patterns=list(PINS)))


def check_pins(snap: Path) -> None:
    for rel, want in PINS.items():
        got = sha256(snap / rel)
        if got != want:
            sys.exit(f"{rel}: sha256 {got}, pinned {want}")
    size = (snap / "model.safetensors").stat().st_size
    if size != WEIGHTS_SIZE:
        sys.exit(f"model.safetensors: {size} bytes, pinned {WEIGHTS_SIZE}")


def safetensors_spans(path: Path) -> dict[str, tuple[str, list[int], int, int]]:
    """tensor name -> (dtype, shape, absolute offset, length)."""
    with open(path, "rb") as f:
        n = struct.unpack("<Q", f.read(8))[0]
        header = json.loads(f.read(n))
    header.pop("__metadata__", None)
    base = 8 + n
    return {k: (v["dtype"], v["shape"], base + v["data_offsets"][0], v["data_offsets"][1] - v["data_offsets"][0])
            for k, v in header.items()}


def load_net(snap: Path):
    sys.path.insert(0, str(snap))
    import torch
    from manga_ocr_nar_small.model import MangaOcrNarSmallNet

    net = MangaOcrNarSmallNet.load(snap, device="cpu", dtype=torch.float32)
    cfg = json.loads((snap / "config.json").read_text(encoding="utf-8"))
    mods = dict(net.named_modules())
    for name, eps in cfg["reparam"]["norm_eps"].items():  # `load` applies them; make sure
        if mods[name].eps != eps:
            sys.exit(f"{name}: eps {mods[name].eps}, config.json says {eps}")
    if net.slots != SLOTS:
        sys.exit(f"{net.slots} output slots, expected {SLOTS}")
    return net


def trace(net) -> bytes:
    """The network as ONNX, with `prepare()` inside the graph, no constant folding and
    no merging of equal initializers (`keep_initializers_as_inputs` turns that pass off;
    [`externalize`] takes the initializers out of the inputs again)."""
    import torch
    import torch.nn.functional as F

    class Wrap(torch.nn.Module):
        def __init__(self, net):
            super().__init__()
            self.net = net

        def forward(self, P, V, R, C, H, W):
            self.net.prepare()
            return self.net(P[None], V[None] > 0.5, R[None].long(), C[None].long(), H.long(), W.long())[0]

    def rms_norm(x, shape, weight=None, eps=None):  # opset 17 has no RMSNormalization
        y = x / torch.sqrt(x.pow(2).mean(-1, keepdim=True) + eps)
        return y * weight if weight is not None else y

    inputs = (torch.zeros(324, 588), torch.ones(324), torch.zeros(324, dtype=torch.int32),
              torch.zeros(324, dtype=torch.int32), torch.ones(1, dtype=torch.int32), torch.ones(1, dtype=torch.int32))
    buf = io.BytesIO()
    saved = F.rms_norm
    F.rms_norm = rms_norm
    try:
        torch.onnx.export(Wrap(net).eval(), inputs, buf, input_names=[n for n, _, _ in INPUTS],
                          output_names=[OUTPUT[0]], opset_version=OPSET, do_constant_folding=False, keep_initializers_as_inputs=True,
                          dynamo=False)
    finally:
        F.rms_norm = saved
    return buf.getvalue()


def externalize(onnx_bytes: bytes, spans) -> bytes:
    """Point every initializer at its tensor in model.safetensors; refuse any left inline."""
    import onnx
    from onnx import TensorProto

    m = onnx.load_from_string(onnx_bytes)
    names = {t.name for t in m.graph.initializer}
    kept = [v for v in m.graph.input if v.name not in names]
    del m.graph.input[:]
    m.graph.input.extend(kept)
    mapped, inline = 0, []
    for t in m.graph.initializer:
        key = t.name.removeprefix("net.")
        span = spans.get(key)
        if span is None or span[0] != "F32" or t.data_type != TensorProto.FLOAT or list(t.dims) != span[1]:
            inline.append((t.name, list(t.dims)))
            continue
        _, _, offset, length = span
        if length != 4 * math.prod(t.dims):
            inline.append((t.name, list(t.dims)))
            continue
        t.ClearField("raw_data")
        t.ClearField("float_data")
        t.data_location = TensorProto.EXTERNAL
        del t.external_data[:]
        for k, v in (("location", WEIGHTS_LOCATION), ("offset", str(offset)), ("length", str(length))):
            e = t.external_data.add()
            e.key, e.value = k, v
        mapped += 1
    if inline:
        sys.exit(f"{len(inline)} initializers are no safetensors tensor (they would be weights inside the graph): "
                 f"{inline[:10]}")
    if mapped != EXTERNAL_ENTRIES:
        sys.exit(f"{mapped} external entries, expected {EXTERNAL_ENTRIES}")
    return m.SerializeToString()


def check_io(graph: bytes) -> None:
    import onnx
    from onnx import TensorProto

    g = onnx.load_from_string(graph).graph

    def sig(v):
        t = v.type.tensor_type
        return v.name, [d.dim_value for d in t.shape.dim], TensorProto.DataType.Name(t.elem_type)

    got_in = [sig(v) for v in g.input]
    got_out = [sig(v) for v in g.output]
    want_in = [(n, s, t) for n, s, t in INPUTS]
    if got_in != want_in or got_out != [OUTPUT]:
        sys.exit(f"graph I/O {got_in} -> {got_out}, expected {want_in} -> {[OUTPUT]}")
    if any(t.data_location != TensorProto.EXTERNAL for t in g.initializer):
        sys.exit("an initializer is stored inside the graph")


def notice(graph_sha: str, graph_size: int) -> str:
    return f"""# {GRAPH_NAME}

The network of **manga-ocr-nar-small, branch r16** by muscgab, as an ONNX graph for
ONNX Runtime. **This file contains no trained weights.** Each of its {EXTERNAL_ENTRIES} weight
entries is a reference (offset and length) into the author's own `model.safetensors`,
which a program using the graph downloads from the author's repository and passes to
ONNX Runtime beside it. The only numbers stored in the graph are tables computed from
the model code (rotary position tables), not from training.

| | |
|---|---|
| graph | `{GRAPH_NAME}`, {graph_size:,} bytes, sha256 `{graph_sha}` |
| source code | https://huggingface.co/{REPO}, commit `{REVISION}` (branch r16): `manga_ocr_nar_small/model.py` (sha256 `{PINS["manga_ocr_nar_small/model.py"]}`) |
| weights it reads | `model.safetensors` of that commit, {WEIGHTS_SIZE:,} bytes, sha256 `{PINS["model.safetensors"]}`: https://huggingface.co/{REPO}/resolve/{REVISION}/model.safetensors |
| vocabulary | `nar_vocabulary_6817.json` of that commit, sha256 `{PINS["nar_vocabulary_6817.json"]}` |
| inputs | P [324, 588] float32, V [324] float32, R [324] int32, C [324] int32, H [1] int32, W [1] int32 (the author's `preprocess.py`) |
| output | ids [{SLOTS}] int64: argmax per output slot; the text ends at the first id 6817 |
| made with | `tools/textbox_export/export_nar.py` of mokuro-bunko (opset {OPSET}, no constant folding) |

## Licences

- **The graph** is a translation of the author's model code, which is licensed under the
  Apache License 2.0 (`LICENSE` in the source repository, copied here as
  `nar-small_r16_LICENSE`). The graph is offered under the same licence.
- **The weights** (`model.safetensors`, not included) are licensed by their author under
  **CC BY-NC-SA 4.0** (`LICENSE_MODEL` in the source repository): non-commercial use
  only, with attribution, and adaptations under the same licence. Commercial use needs
  the author's permission. Running the graph means using those weights under that
  licence. See the source repository's `NOTICE.md` for the provenance of the weights
  and their training data.

## Checks made at export

- every initializer is external and points at exactly one tensor of
  `model.safetensors` with the same type and shape; none is stored in the graph;
- exported again with every parameter replaced by random numbers, the graph is byte for
  byte the same, so nothing in it depends on the trained values;
- its output ids equal the author's PyTorch (fp32) code's on 200 crops
  (`tools/textbox_export/check_parity.py`).
"""


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--snapshot", help="the repository at the pinned commit (default: Hugging Face cache)")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    import torch

    torch.manual_seed(0)
    snap = snapshot_dir(a.snapshot)
    check_pins(snap)
    spans = safetensors_spans(snap / "model.safetensors")
    net = load_net(snap)

    graph = externalize(trace(net), spans)
    check_io(graph)

    # The same export from random parameters must give the same bytes.
    other = copy.deepcopy(net)
    g = torch.Generator().manual_seed(1)
    with torch.no_grad():
        for p in other.parameters():
            p.copy_(torch.randn(p.shape, generator=g, dtype=p.dtype))
    if externalize(trace(other), spans) != graph:
        sys.exit("the graph changes with the weights' values: something trained is inside it")

    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    (out / GRAPH_NAME).write_bytes(graph)
    digest = hashlib.sha256(graph).hexdigest()
    (out / "NOTICE.md").write_text(notice(digest, len(graph)), encoding="utf-8")
    (out / "nar-small_r16_LICENSE").write_bytes((snap / "LICENSE").read_bytes())
    print(f"{GRAPH_NAME}: {len(graph)} bytes, sha256 {digest}")
    print(f"external entries {EXTERNAL_ENTRIES} -> {WEIGHTS_LOCATION} (sha256 {PINS['model.safetensors']}); "
          "no weights inside; weight-independent")


if __name__ == "__main__":
    main()
