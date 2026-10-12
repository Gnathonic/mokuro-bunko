"""Assemble the Vulkan model release (docs/rust-port/VULKAN.md §3).

    python make_release.py --out <flat dir> --json <vulkan_models.json> \
        --engine hayai-nova --ncnn <dir> --webgpu <dir> --golden <hayai-nova.json> \
        [--release vulkan-models-v1]

Hard-links each file into <flat dir> under its release asset name
(`vulkan_<engine>_<runtime>_<file>`, `vulkan_selftest_<engine>.json`) and writes the
release description crates/bunko-ocr/src/vulkan_models.json (the schema of
torch_models.json) that the model store compiles in. The flat directory is then uploaded
as the release's assets.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path

REPO = "https://github.com/Gnathonic/mokuro-bunko/releases/download"
SOURCES = {
    "hayai-nova": [
        {"repo": "JustANormalTinkerer/hayai-ocr-v2.5-nova",
         "revision": "e46d79138499600564f810d44ab6bdea7230dee1", "licence": "Apache-2.0"},
        {"repo": "google/siglip2-base-patch16-naflex",
         "revision": "b53b807d3a2d5e2b3911292f2d69e5341cdc064c", "licence": "Apache-2.0"},
    ],
    "paddle-manga": [
        {"repo": "PaddlePaddle/PaddleOCR-VL-1.6",
         "revision": "c5630abae1d940eafe0697512a0325494b02ab42", "licence": "Apache-2.0"},
        {"repo": "sorryhyun/paddleocr-vl-1.6-manga-lora",
         "revision": "26292839d1469c14212a12a1e01b5b1fe01bff15", "licence": "Apache-2.0"},
    ],
}
# What each runtime loads per engine (anything else in the directory is left out).
# ncnn graphs format 2 (tools/ncnn_export): prefill and step share decoder.bin.
NCNN_COMMON = ["vision.param", "vision.bin", "prefill.param", "step.param", "decoder.bin", "ncnn.json"]
FILES = {
    "ncnn": {
        "hayai-nova": NCNN_COMMON + ["token_embeddings.f32"],
        "paddle-manga": NCNN_COMMON + ["embeddings.f32", "pos_table.f32"],
    },
    # bunko_vlm::HayaiGpuGraphs::{VISION,DECODER}, bunko_vlm::paddle::webgpu_files()
    # (tools/webgpu_export build). crates/bunko-vulkan/tests/manifest.rs checks the Rust
    # side's lists against the description this writes.
    "webgpu": {
        "hayai-nova": ["hayai-nova_webgpu_vision_fp32.onnx", "hayai-nova_webgpu_decoder_fp32.onnx"],
        "paddle-manga": [f"paddle-manga_webgpu_{p}_fp32.onnx" for p in
                         ["weights", "embed", "layer", "qkv", "attn", "post", "tail", "decoder"]]
        + ["paddle-manga_webgpu_weights_fp32.onnx.data", "paddle-manga_webgpu_vision.json"],
    },
}


def sha256(p: Path) -> str:
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def place(src: Path, dest: Path):
    if dest.exists():
        dest.unlink()
    try:
        os.link(src.resolve(), dest)
    except OSError:
        import shutil
        shutil.copyfile(src, dest)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--json", type=Path, required=True)
    ap.add_argument("--engine", default="hayai-nova")
    ap.add_argument("--ncnn", type=Path)
    ap.add_argument("--webgpu", type=Path)
    ap.add_argument("--golden", type=Path, required=True)
    ap.add_argument("--release", default="vulkan-models-v1")
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    files = []

    def add(src: Path, file: str, id_: str, role: str, runtime):
        place(src, a.out / file)
        files.append({
            "engine": a.engine, "file": file, "url": f"{REPO}/{a.release}/{file}",
            "size": src.stat().st_size, "sha256": sha256(src), "licence": "Apache-2.0",
            "sources": SOURCES[a.engine], "role": role, "precision": "fp32",
            "id": id_, "path": id_ if role != "selftest" else f"{id_}.json",
            "kind": f"vulkan-{runtime}" if runtime else "vulkan-selftest",
        })

    for runtime, d in (("ncnn", a.ncnn), ("webgpu", a.webgpu)):
        if d is None:
            continue
        for f in FILES[runtime].get(a.engine, []):
            src = d / f
            if not src.is_file():
                raise SystemExit(f"{src} is missing")
            id_ = f"vulkan/{a.engine}/{runtime}/{f}"
            add(src, f"vulkan_{a.engine}_{runtime}_{f}", id_, f"{runtime}-graph", runtime)
    add(a.golden, f"vulkan_selftest_{a.engine}.json", f"vulkan/selftest/{a.engine}", "selftest", None)
    # Keep the other engines' entries of an existing description.
    old = []
    if a.json.is_file():
        old = [f for f in json.loads(a.json.read_text()).get("files", []) if f["engine"] != a.engine]
    a.json.write_text(json.dumps({"release": a.release, "files": old + files}, indent=1) + "\n")
    total = sum(f["size"] for f in files)
    print(f"{len(files)} files, {total / 1e9:.2f} GB -> {a.out}; {a.json}")


if __name__ == "__main__":
    main()
