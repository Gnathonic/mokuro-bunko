"""fp32 -> fp16 graph conversion with fp32 islands, and the activation audit behind it.

A full-graph fp16 conversion runs RMSNorm's ``x**2 -> mean -> +eps -> sqrt ->
1/x`` in fp16: ``x**2`` overflows to inf once ``|x| > 255.9`` and the norm then
returns 0 for that row. torch autocast keeps norms and softmax in fp32 for
exactly that reason. :func:`convert` does the same: the RMSNorm chain (found
structurally, see :func:`rmsnorm_nodes`) and every ``Softmax`` stay fp32, with
casts at their edges; everything else (MatMul, LayerNormalization -- whose ORT
kernel accumulates in fp32 anyway --, the MLPs) becomes fp16. Graph inputs and
outputs become fp16 too (``keep_io_types=False``), except integer index inputs.

:func:`audit` measures the max |value| of every float tensor of the fp32
graph over real inputs, which is what decides whether the islands are needed.
"""

from __future__ import annotations

from collections import defaultdict
from typing import Any

import numpy as np

FP16_MAX = 65504.0


def _consumers(graph: Any) -> dict[str, list[Any]]:
    out: dict[str, list[Any]] = defaultdict(list)
    for node in graph.node:
        for name in node.input:
            out[name].append(node)
    return out


def rmsnorm_nodes(graph: Any) -> list[list[Any]]:
    """Every ``Pow(x,2) -> ReduceMean -> Add(eps) -> Sqrt -> Reciprocal -> Mul(x, .)`` chain.

    (torch's ``F.rms_norm`` decomposition as the dynamo exporter writes it at
    opset 20/21; a ``Div(1, sqrt)`` in place of ``Reciprocal`` is accepted too.)
    """
    cons = _consumers(graph)
    chains = []
    for pow_node in (n for n in graph.node if n.op_type == "Pow"):
        x = pow_node.input[0]
        chain = [pow_node]
        cur = pow_node
        ok = True
        for want in (("ReduceMean",), ("Add",), ("Sqrt",), ("Reciprocal", "Div")):
            nxt = [n for n in cons[cur.output[0]] if n.op_type in want]
            if len(nxt) != 1:
                ok = False
                break
            cur = nxt[0]
            chain.append(cur)
        if not ok:
            continue
        mul = [n for n in cons[cur.output[0]] if n.op_type == "Mul" and x in n.input]
        if len(mul) != 1:
            continue
        chain.append(mul[0])
        chains.append(chain)
    return chains


def convert(model: Any, *, fp32_islands: bool = True) -> tuple[Any, dict[str, int]]:
    """fp16 copy of ``model`` (which is modified in place and returned)."""
    import warnings

    from onnxconverter_common import float16

    graph = model.graph
    for i, node in enumerate(graph.node):  # the block list works by node name
        if not node.name:
            node.name = f"_unnamed_{i}"
    block: list[str] = []
    stats = {"rmsnorm_chains": 0, "softmax": 0}
    if fp32_islands:
        chains = rmsnorm_nodes(graph)
        stats["rmsnorm_chains"] = len(chains)
        block += [n.name for chain in chains for n in chain]
        soft = [n.name for n in graph.node if n.op_type == "Softmax"]
        stats["softmax"] = len(soft)
        block += soft
    with warnings.catch_warnings():
        # "the float32 number 4.7e-08 will be truncated to 1e-07": the converter
        # clamps |w| < 1e-7 to +-1e-7 instead of an f16 subnormal; harmless here
        # (parity measured) but hundreds of lines of noise.
        warnings.filterwarnings("ignore", message="the float32 number")
        converted = float16.convert_float_to_float16(
            model,
            keep_io_types=False,
            disable_shape_infer=True,
            node_block_list=block or None,
        )
    stats["pow_total"] = sum(1 for n in graph.node if n.op_type == "Pow")
    return converted, stats


class Audit:
    """Running max |x| per float tensor of an fp32 graph (every intermediate exposed as an output)."""

    def __init__(self, path, threads: int = 0) -> None:
        import onnx
        import onnxruntime as ort

        model = onnx.load(str(path), load_external_data=True)
        model = onnx.shape_inference.infer_shapes(model)
        kinds = {vi.name: vi.type.tensor_type.elem_type for vi in model.graph.value_info}
        self.op_of: dict[str, str] = {}
        have = {o.name for o in model.graph.output}
        for node in model.graph.node:
            for name in node.output:
                if name and name not in have and kinds.get(name) == onnx.TensorProto.FLOAT:
                    model.graph.output.append(onnx.helper.make_tensor_value_info(name, onnx.TensorProto.FLOAT, None))
                    self.op_of[name] = node.op_type
        so = ort.SessionOptions()
        so.graph_optimization_level = ort.GraphOptimizationLevel.ORT_DISABLE_ALL
        if threads:
            so.intra_op_num_threads = threads
        self.sess = ort.InferenceSession(model.SerializeToString(), so, providers=["CPUExecutionProvider"])
        self.names = [o.name for o in self.sess.get_outputs()]
        self.n_real = len(have)
        self.max: dict[str, float] = defaultdict(float)

    def run(self, feed: dict[str, np.ndarray]) -> list[np.ndarray]:
        outs = self.sess.run(None, feed)
        for name, val in zip(self.names[self.n_real :], outs[self.n_real :], strict=True):
            if val.size:
                # values <= -1e8 are the attention masks' -1e9 sentinel (-inf in fp16, by design)
                finite = val[np.isfinite(val) & (val > -1e8)]
                if finite.size:
                    self.max[name] = max(self.max[name], float(np.abs(finite).max()))
        return outs[: self.n_real]

    def report(self) -> dict[str, Any]:
        by_op: dict[str, float] = defaultdict(float)
        for name, v in self.max.items():
            by_op[self.op_of[name]] = max(by_op[self.op_of[name]], v)
        worst = sorted(((v, n) for n, v in self.max.items()), reverse=True)
        over = [(n, self.op_of[n], v) for v, n in worst if v > FP16_MAX]
        return {
            "max_abs_by_op": dict(sorted(by_op.items())),
            "over_fp16_max": over[:40],
            "n_over_fp16_max": len(over),
            "top": [(n, self.op_of[n], v) for v, n in worst[:15]],
        }
