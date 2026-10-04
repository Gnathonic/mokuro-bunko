#!/usr/bin/env python3
"""Drive an installed OCR backend pack through its C ABI (bunko_torch::abi), no
mokuro-bunko needed: bt_init -> bt_devices -> bt_load -> bt_read over a crop set ->
compare with a reference. Used to find a pack's minimal file set empirically
(run it under LD_DEBUG=libs) and to smoke-test packs inside Docker images.

    probe_pack.py <pack dir> <engine> <graphs dir> <precision> <device> \
        --models <models-v1 dir> --crops <dir of PNGs> [--ref ref.json] [--n 40]

<graphs dir> holds {vision,prefill,step}.pt2 or <short>_<prec>_{vision,prefill,step}.pt2
(the shootout layout). Needs Pillow.
"""

import argparse
import ctypes
import json
import os
import sys
import time

from PIL import Image


class BtCrop(ctypes.Structure):
    _fields_ = [("data", ctypes.c_void_p), ("width", ctypes.c_uint32), ("height", ctypes.c_uint32)]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("pack")
    ap.add_argument("engine", choices=["hayai-nova", "paddle-manga"])
    ap.add_argument("graphs")
    ap.add_argument("precision")
    ap.add_argument("device")
    ap.add_argument("--models", required=True)
    ap.add_argument("--crops", required=True)
    ap.add_argument("--ref")
    ap.add_argument("--n", type=int, default=40)
    ap.add_argument("--threads", type=int, default=0)
    ap.add_argument("--batch", type=int, default=8)
    a = ap.parse_args()

    pack = json.load(open(os.path.join(a.pack, "pack.json")))
    lib = ctypes.CDLL(os.path.join(a.pack, pack["library"]), mode=ctypes.RTLD_LOCAL)
    err = ctypes.c_char_p()
    out = ctypes.c_char_p()
    lib.bt_abi_version.restype = ctypes.c_uint32
    lib.bt_load.restype = ctypes.c_void_p
    lib.bt_load.argtypes = [ctypes.c_char_p] * 5 + [ctypes.POINTER(ctypes.c_char_p)]
    lib.bt_read.argtypes = [
        ctypes.c_void_p,
        ctypes.POINTER(BtCrop),
        ctypes.c_size_t,
        ctypes.c_void_p,
        ctypes.POINTER(ctypes.c_char_p),
        ctypes.POINTER(ctypes.c_char_p),
    ]
    lib.bt_free.argtypes = [ctypes.c_void_p]

    def fail(what):
        sys.exit(f"{what}: {err.value.decode() if err.value else '?'}")

    print("abi", lib.bt_abi_version(), "pack abi", pack["abi"])
    cfg = json.dumps({"lib_dir": os.path.join(a.pack, pack.get("lib_dir", "lib"))}).encode()
    if lib.bt_init(cfg, ctypes.byref(err)) != 0:
        fail("bt_init")
    if lib.bt_devices(ctypes.byref(out), ctypes.byref(err)) != 0:
        fail("bt_devices")
    print("devices", out.value.decode())

    short = "hayai" if a.engine == "hayai-nova" else "paddle"
    p = a.precision

    def graph(role):
        plain = os.path.join(a.graphs, f"{role}.pt2")
        return plain if os.path.exists(plain) else os.path.join(a.graphs, f"{short}_{p}_{role}.pt2")

    m = a.models
    if a.engine == "hayai-nova":
        opts = {
            "tokenizer": f"{m}/hayai-nova_tokenizer.json",
            "pos_table": f"{m}/hayai-nova_pos_table.npy",
            "embeddings": f"{m}/hayai-nova_token_embeddings.npy",
        }
    else:
        emb = f"{m}/paddle-manga_embed_{'fp16' if p == 'fp16' else 'fp32'}.npy"
        opts = {"tokenizer": f"{m}/paddle-manga_tokenizer.json", "embeddings": emb}
    opts.update(vision=graph("vision"), prefill=graph("prefill"), step=graph("step"), threads=a.threads)
    t0 = time.time()
    h = lib.bt_load(a.engine.encode(), a.graphs.encode(), p.encode(), a.device.encode(),
                    json.dumps(opts).encode(), ctypes.byref(err))
    if not h:
        fail("bt_load")
    print(f"loaded in {time.time() - t0:.1f}s")

    names = sorted(f for f in os.listdir(a.crops) if f.endswith(".png"))[: a.n]
    texts = []
    t0 = time.time()
    for i in range(0, len(names), a.batch):
        imgs = [Image.open(os.path.join(a.crops, n)).convert("RGB") for n in names[i:i + a.batch]]
        bufs = [im.tobytes() for im in imgs]
        crops = (BtCrop * len(imgs))(*[
            BtCrop(ctypes.cast(ctypes.c_char_p(b), ctypes.c_void_p), im.width, im.height)
            for b, im in zip(bufs, imgs)
        ])
        if lib.bt_read(h, crops, len(imgs), None, ctypes.byref(out), ctypes.byref(err)) != 0:
            fail("bt_read")
        texts += json.loads(out.value.decode())
    dt = time.time() - t0
    print(f"{len(texts)} crops in {dt:.1f}s ({len(texts) / dt:.1f} crops/s)")
    if a.ref:
        ref = json.load(open(a.ref))
        if isinstance(ref, dict):  # {"texts": {"c000.png": "..."}}
            ref = [ref["texts"].get(n) for n in names]
        ref = ref[: len(texts)]
        same = sum(x == y for x, y in zip(texts, ref))
        print(f"parity {same}/{len(texts)} vs {os.path.basename(a.ref)}")
        for x, y in list(zip(texts, ref))[:3]:
            print("  ", x, "|", y)
    lib.bt_free(h)
    print("PROBE OK")


if __name__ == "__main__":
    main()
