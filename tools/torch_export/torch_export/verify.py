"""Load built packages on THIS machine's device and compare them with eager torch.

    python -m torch_export.verify <out> <engine> <precision> <target> [--device cuda|cpu]

Binds the shared weights exactly as the Rust runtime will (one tensor per weights-file
key on the device, ``load_constants(user_managed=True)`` per package via the
``bunko.weights`` map), runs vision -> prefill -> 8 decode steps on a fixed random batch
and reports, per graph, the max |difference| against the eager modules the packages were
exported from (same precast weights, same autocast). Token ids must match exactly.
The crop-level parity gate is the Rust harness (ltbench) on the real crop sets.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path


def load_weights(pdir: Path, device):
    from safetensors import safe_open

    w = {}
    for f in sorted(pdir.glob("weights-*.safetensors")):
        with safe_open(str(f), "pt") as s:
            for k in s.keys():
                w[k] = s.get_tensor(k).to(device)
    return w


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


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("out", type=Path)
    ap.add_argument("engine")
    ap.add_argument("precision")
    ap.add_argument("target")
    ap.add_argument("--device", default="cuda")
    a = ap.parse_args(argv)
    from .__main__ import _imports

    _imports()
    import torch

    from . import graphs as G

    dev = torch.device(a.device, 0) if a.device == "cuda" else torch.device("cpu")
    pdir = a.out / a.engine / a.precision
    tdir = pdir / a.target
    weights = load_weights(pdir, dev) if any(pdir.glob("weights-*.safetensors")) else {}
    t0 = time.time()
    pk = {}
    for role in G.ROLES:
        pk[role], meta = load_pkg(tdir / f"{role}.pt2", weights, dev.index if dev.type == "cuda" else -1)
    io = int(meta["bunko.io"])
    print(f"loaded 3 packages + {len(weights)} weights in {time.time() - t0:.1f}s (io v{io})", file=sys.stderr)
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
    print(json.dumps(res))
    return 0 if all(r["ids_equal"] for r in res.values()) else 1


if __name__ == "__main__":
    sys.exit(main())
