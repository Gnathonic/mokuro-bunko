"""Export paddle-manga (PaddleOCR-VL-1.6 + merged manga LoRA + LoRA vision tower).

The model is loaded exactly as the 0.5.2 runner's ``PaddleMangaRecognizer``
does (base in the target dtype, LoRA merged with ``merge_and_unload``, the
LoRA repo's ``tower.safetensors`` over the vision tower, patch conv as a
matmul), once in fp32 and once in fp16 -- the fp16 graphs are exported from
the fp16-loaded model, i.e. the arithmetic 0.5.2 ran on a card at fp16
(transformers' RMSNorm upcasts to fp32 internally, so the norms stay fp32 in
the graph).

Writes into ``out``:
  paddle-manga_vision_{fp32,fp16}.onnx (+ .onnx.data)     §6.5, one image per call
  paddle-manga_decoder_{fp32,fp16}.onnx (+ .onnx.data)    §6.9, KV cache in/out
  paddle-manga_embed_fp32.npy / _fp16.npy                 input token embeddings (103424, 1024)
  paddle-manga_tokenizer.json                             the base repo's, byte for byte
  paddle-manga_config.json                                prompt ids, special ids, constants
"""

from __future__ import annotations

import argparse
import gc
import shutil
import time
from pathlib import Path

import numpy as np

from ..common import DEFAULT_OUT, REPO_ROOT, load_onnx, log, provenance, snapshot_file, stage_dir, write_json, write_onnx
from ..pins import ENGINE_SOURCES, PADDLE_BASE, PADDLE_LORA, check_pins
from . import host as H

ENGINE = "paddle-manga"
TOKENIZER = f"{ENGINE}_tokenizer.json"
CONFIG = f"{ENGINE}_config.json"
OPSET = 21
# The ids 0.5.2's chat template produces around the image tokens (spec §6.6);
# re-derived from the processor on every export and checked against these.
EXPECTED_PREFIX = [100273, 2969, 93963, 93919, 101305]
EXPECTED_SUFFIX = [101306, 93972, 2497, 93963, 23, 92267, 93963, 23]


def name(role: str, precision: str) -> str:
    return f"{ENGINE}_{role}_{precision}.onnx"


def embed_name(precision: str) -> str:
    return f"{ENGINE}_embed_{precision}.npy"


def load_model(dtype_name: str):
    """``PaddleMangaRecognizer.__init__`` (engine_runner.py), minus the runner plumbing."""
    import torch
    from huggingface_hub import hf_hub_download
    from peft import PeftModel
    from safetensors.torch import load_file
    from transformers import AutoModelForImageTextToText, AutoProcessor

    from ..common import import_runner

    dtype = getattr(torch, dtype_name)
    model = AutoModelForImageTextToText.from_pretrained(PADDLE_BASE.repo, revision=PADDLE_BASE.revision, dtype=dtype, attn_implementation="sdpa")
    model = PeftModel.from_pretrained(model, PADDLE_LORA.repo, revision=PADDLE_LORA.revision).merge_and_unload()
    tower = load_file(hf_hub_download(PADDLE_LORA.repo, "tower.safetensors", revision=PADDLE_LORA.revision))
    tower = {k: v.to(dtype) for k, v in tower.items()}
    result = model.load_state_dict(tower, strict=False)
    if result.unexpected_keys:
        raise RuntimeError(f"unexpected tower keys: {result.unexpected_keys[:5]}")
    swapped = import_runner().linear_patch_embedding(model)
    assert swapped == 1, swapped
    model = model.eval()
    proc = AutoProcessor.from_pretrained(PADDLE_BASE.repo, revision=PADDLE_BASE.revision)
    proc.tokenizer.padding_side = "left"
    return model, proc


def prompt_ids(proc) -> tuple[list[int], list[int]]:
    from PIL import Image

    msgs = [{"role": "user", "content": [{"type": "image"}, {"type": "text", "text": "OCR:"}]}]
    prompt = proc.apply_chat_template(msgs, add_generation_prompt=True, tokenize=False)
    ids = proc(text=[prompt], images=[Image.new("RGB", (336, 336), "white")], return_tensors="np")["input_ids"][0].tolist()
    first = ids.index(H.IMAGE_TOKEN)
    last = len(ids) - ids[::-1].index(H.IMAGE_TOKEN)
    prefix, suffix = ids[:first], ids[last:]
    if (prefix, suffix) != (EXPECTED_PREFIX, EXPECTED_SUFFIX):
        raise RuntimeError(f"prompt ids changed: {prefix} / {suffix}")
    return prefix, suffix


def export_graphs(model, dtype_name: str, stage: Path) -> tuple[Path, Path]:
    import torch

    from .modules import DecoderWrap, VisionWrap

    tdt = getattr(torch, dtype_name)
    vw, dw = VisionWrap(model).eval(), DecoderWrap(model).eval()
    g = torch.Generator().manual_seed(0)
    gh, gw = 4, 20
    aux = H.vision_aux(gh, gw)
    vargs = (torch.randn(gh * gw, 3, 14, 14, generator=g).to(tdt), *(torch.from_numpy(a) for a in aux))
    N = torch.export.Dim("N", min=16, max=8192)
    vpath = stage / f"paddle_vision_{dtype_name}.onnx"
    t0 = time.time()
    torch.onnx.export(
        vw, vargs, str(vpath), dynamo=True, external_data=True, opset_version=OPSET,
        input_names=["pixel_values", "pos_idx", "pos_w", "cos", "sin", "merge_idx"], output_names=["image_embeds"],
        dynamic_shapes=({0: N}, {0: N}, {0: N}, {0: N}, {0: N}, {0: N}),
    )
    log(f"[paddle] {dtype_name} vision exported in {time.time() - t0:.0f}s")
    B, S, P = 2, 7, 5
    x = torch.randn(B, S, 1024, generator=g).to(tdt)
    cos = torch.randn(B, S, 128, generator=g).to(tdt)
    sin = torch.randn(B, S, 128, generator=g).to(tdt)
    bias = torch.zeros(B, 1, S, P + S, dtype=tdt)
    past = [torch.randn(B, 2, P, 128, generator=g).to(tdt) for _ in range(2 * H.N_LAYERS)]
    names_in = ["inputs_embeds", "cos", "sin", "bias"] + [f"past_{kv}_{i}" for i in range(H.N_LAYERS) for kv in ("k", "v")]
    names_out = ["logits"] + [f"present_{kv}_{i}" for i in range(H.N_LAYERS) for kv in ("k", "v")]
    A = torch.export.Dim.AUTO
    ds = [{0: A, 1: A}, {0: A, 1: A}, {0: A, 1: A}, {0: A, 2: A, 3: A}, tuple([{0: A, 2: A}] * (2 * H.N_LAYERS))]
    dpath = stage / f"paddle_decoder_{dtype_name}.onnx"
    t0 = time.time()
    torch.onnx.export(
        dw, (x, cos, sin, bias, *past), str(dpath), dynamo=True, external_data=True, opset_version=OPSET,
        input_names=names_in, output_names=names_out, dynamic_shapes=tuple(ds),
    )
    log(f"[paddle] {dtype_name} decoder exported in {time.time() - t0:.0f}s")
    return vpath, dpath


def run(out: Path, precisions: tuple[str, ...] = ("fp32", "fp16")) -> list[Path]:
    import json

    check_pins(REPO_ROOT)
    out.mkdir(parents=True, exist_ok=True)
    stage = stage_dir(out)
    sources = ENGINE_SOURCES[ENGINE]
    written: list[Path] = []
    emb32 = None
    for prec in precisions:
        dtype_name = {"fp32": "float32", "fp16": "float16"}[prec]
        model, proc = load_model(dtype_name)
        emb = model.model.language_model.embed_tokens.weight.detach()
        if prec == "fp32":
            emb32 = emb.float().numpy().copy()
            prefix, suffix = prompt_ids(proc)
            shutil.copyfile(snapshot_file(PADDLE_BASE, "tokenizer.json"), out / TOKENIZER)
            tj = json.loads((out / TOKENIZER).read_text(encoding="utf-8"))
            write_json(out / CONFIG, {
                "engine": ENGINE,
                "prompt": {"prefix": prefix, "suffix": suffix, "image_token": H.IMAGE_TOKEN},
                "special": {"eos": H.EOS, "pad": H.PAD},
                "skip_ids": sorted(a["id"] for a in tj["added_tokens"] if a["special"]),
                "vocab_size": int(emb32.shape[0]), "hidden": int(emb32.shape[1]),
                "layers": H.N_LAYERS, "kv_heads": H.N_KV, "head_dim": H.T_HD,
                "rope_theta": H.ROPE_THETA, "mrope_section": list(H.MROPE),
                "vision": {"patch": H.PATCH, "merge": H.MERGE, "pos_side": H.SIDE, "head_dim": H.V_HD, "rope_theta": 10000.0,
                           "min_pixels": H.MIN_PIX, "max_pixels": H.MAX_PIX, "resample": "bicubic", "mean": 0.5, "std": 0.5},
                "default_max_new_tokens": H.DEFAULT_MAX_NEW_TOKENS, "batch": H.BATCH,
                "bias_neg": {"fp32": float(np.finfo(np.float32).min), "fp16": float(np.finfo(np.float16).min)},
            })
            written += [out / TOKENIZER, out / CONFIG]
            np.save(out / embed_name("fp32"), np.ascontiguousarray(emb32, dtype="<f4"))
            written.append(out / embed_name("fp32"))
        else:
            e16 = emb.numpy()
            if emb32 is not None and not np.array_equal(e16, emb32.astype(np.float16)):
                raise RuntimeError("fp16 embedding is not the fp32 one rounded")
            np.save(out / embed_name("fp16"), np.ascontiguousarray(e16, dtype="<f2"))
            written.append(out / embed_name("fp16"))
        vpath, dpath = export_graphs(model, dtype_name, stage)
        del model, proc
        gc.collect()
        for role, src in (("vision", vpath), ("decoder", dpath)):
            m = load_onnx(src)
            extra = {"mokuro.fp16": "exported from the fp16-loaded torch model"} if prec == "fp16" else None
            written += write_onnx(m, out / name(role, prec), meta=provenance(ENGINE, role, prec, sources, extra), external=True)
            del m
            gc.collect()
    for p in written:
        log(f"[paddle] wrote {p.name} ({p.stat().st_size:,} bytes)")
    return written


def main(argv: list[str] | None = None) -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--precision", choices=("fp32", "fp16"), action="append")
    a = ap.parse_args(argv)
    run(a.out, tuple(a.precision or ("fp32", "fp16")))


if __name__ == "__main__":
    main()
