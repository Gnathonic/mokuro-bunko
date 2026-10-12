"""Export hayai-nova (vision tower + projector, decoder with KV cache) and its host tables.

Writes into ``out``:
  hayai-nova_vision_fp32.onnx / _fp16.onnx     §5.6 of spec/ocr-recognizers.md
  hayai-nova_decoder_fp32.onnx / _fp16.onnx    §5.9
  hayai-nova_pos_table.npy                     f32 (256, 768)
  hayai-nova_token_embeddings.npy              f32 (16004, 512)
  hayai-nova_tokenizer.json                    the model repo's, byte for byte
  hayai-nova_config.json                       special ids + the loop's constants
"""

from __future__ import annotations

import argparse
import shutil
import time
from pathlib import Path

import numpy as np

from ..common import DEFAULT_OUT, REPO_ROOT, load_onnx, log, provenance, snapshot_file, stage_dir, write_json, write_onnx
from ..pins import ENGINE_SOURCES, HAYAI, check_pins

ENGINE = "hayai-nova"
POS_TABLE = f"{ENGINE}_pos_table.npy"
TOKEN_EMBEDDINGS = f"{ENGINE}_token_embeddings.npy"
TOKENIZER = f"{ENGINE}_tokenizer.json"
CONFIG = f"{ENGINE}_config.json"
OPSET = 20


def name(role: str, precision: str) -> str:
    return f"{ENGINE}_{role}_{precision}.onnx"


def load_model():
    import torch
    from transformers import AutoModel, PreTrainedTokenizerFast

    torch.manual_seed(0)
    model = AutoModel.from_pretrained(HAYAI.repo, revision=HAYAI.revision, trust_remote_code=True).eval()
    tok = PreTrainedTokenizerFast.from_pretrained(HAYAI.repo, revision=HAYAI.revision)
    return model, tok


def export_fp32(model, stage: Path) -> tuple[Path, Path]:
    import torch
    from torch.export import Dim

    from .modules import NovaDecoder, NovaVision

    vis, dec = NovaVision(model).eval(), NovaDecoder(model).eval()
    g = torch.Generator().manual_seed(0)
    b, P, M = 3, 512, 130
    ex = (
        torch.randn(b, P, 768, generator=g),
        torch.ones(b, P),
        torch.randn(b, P, 768, generator=g),
        torch.randint(0, P, (b, M, 4), generator=g),
        torch.ones(b, M),
    )
    B, PP, MM = Dim("b", min=1, max=64), Dim("P", min=2, max=4096), Dim("M", min=2, max=2048)
    vpath = stage / "hayai_vision.onnx"
    t0 = time.time()
    torch.onnx.export(
        vis, ex, str(vpath), dynamo=True, external_data=False, opset_version=OPSET, optimize=True,
        input_names=["pixel_values", "pixel_mask", "pos", "gather_idx", "tok_valid"], output_names=["vis_tokens"],
        dynamic_shapes=({0: B, 1: PP}, {0: B, 1: PP}, {0: B, 1: PP}, {0: B, 1: MM}, {0: B, 1: MM}),
    )
    log(f"[hayai] vision exported in {time.time() - t0:.0f}s")

    S, L = Dim("s", min=1, max=4096), Dim("L", min=0, max=8192)
    s, l = 5, 7
    past = []
    for _ in range(12):
        past += [torch.randn(b, 2, l, 64, generator=g), torch.randn(b, 2, l, 64, generator=g)]
    exd = (torch.randn(b, s, 512, generator=g), torch.zeros(b, 1, s, l + s), torch.randn(b, s, 32, generator=g), torch.randn(b, s, 32, generator=g), *past)
    names_p = [n for i in range(12) for n in (f"past_k{i}", f"past_v{i}")]
    names_o = [n for i in range(12) for n in (f"present_k{i}", f"present_v{i}")]
    dyn = [{0: B, 1: S}, {0: B, 2: S, 3: Dim.AUTO}, {0: B, 1: S}, {0: B, 1: S}, tuple([{0: B, 2: L}] * 24)]
    dpath = stage / "hayai_decoder.onnx"
    t0 = time.time()
    torch.onnx.export(
        dec, exd, str(dpath), dynamo=True, external_data=False, opset_version=OPSET, optimize=True,
        input_names=["embeds", "mask", "cos", "sin", *names_p], output_names=["logits", *names_o],
        dynamic_shapes=tuple(dyn),
    )
    log(f"[hayai] decoder exported in {time.time() - t0:.0f}s")
    return vpath, dpath


def special_ids(tok) -> dict:
    # The runner's own fallbacks (nova_generate): bos or 1, eos or 2, pad or eos.
    bos = tok.bos_token_id or 1
    eos = tok.eos_token_id or 2
    pad = tok.pad_token_id or eos
    return {"bos": bos, "eos": eos, "pad": pad}


def run(out: Path, fp16_full_variant: bool = False) -> list[Path]:
    import json

    from .. import fp16 as F16

    check_pins(REPO_ROOT)
    out.mkdir(parents=True, exist_ok=True)
    stage = stage_dir(out)
    model, tok = load_model()
    sources = ENGINE_SOURCES[ENGINE]
    written: list[Path] = []

    vm = model.vision_encoder.vision_model if hasattr(model.vision_encoder, "vision_model") else model.vision_encoder
    pos = vm.embeddings.position_embedding.weight.detach().float().numpy()
    emb = model.decoder.token_embeddings.weight.detach().float().numpy()
    assert pos.shape == (256, 768) and emb.shape[1] == 512, (pos.shape, emb.shape)
    np.save(out / POS_TABLE, np.ascontiguousarray(pos, dtype="<f4"))
    np.save(out / TOKEN_EMBEDDINGS, np.ascontiguousarray(emb, dtype="<f4"))
    shutil.copyfile(snapshot_file(HAYAI, "tokenizer.json"), out / TOKENIZER)
    tj = json.loads((out / TOKENIZER).read_text(encoding="utf-8"))
    special = special_ids(tok)
    write_json(out / CONFIG, {
        "engine": ENGINE,
        "special": special,
        "skip_ids": sorted(a["id"] for a in tj["added_tokens"] if a["special"]),
        "vocab_size": int(emb.shape[0]),
        "patch": 16, "patch_budgets": [256, 384, 512], "default_patch_budget": 512,
        "hidden": 512, "vision_hidden": 768, "layers": 12, "kv_heads": 2, "head_dim": 64, "rope_axis_dim": 32,
        "rope_theta": 10000.0, "mask_neg": -1e9,
        "max_new_tokens": 96, "batch": 16,
        "normalize": {"mean": 0.5, "std": 0.5, "resample": "bilinear"},
    })
    written += [out / POS_TABLE, out / TOKEN_EMBEDDINGS, out / TOKENIZER, out / CONFIG]

    vpath, dpath = export_fp32(model, stage)
    del model
    for role, src in (("vision", vpath), ("decoder", dpath)):
        m = load_onnx(src)
        written += write_onnx(m, out / name(role, "fp32"), meta=provenance(ENGINE, role, "fp32", sources), external=False)
        m16, stats = F16.convert(load_onnx(src), fp32_islands=True)
        log(f"[hayai] {role} fp16: kept in fp32 {stats}")
        written += write_onnx(m16, out / name(role, "fp16"), external=False, meta=provenance(
            ENGINE, role, "fp16", sources,
            {"mokuro.fp16": "onnxconverter-common float16; RMSNorm chains + Softmax kept fp32", "mokuro.fp16_islands": json.dumps(stats)}))
        if fp16_full_variant:  # comparison only, never published
            mf, _ = F16.convert(load_onnx(src), fp32_islands=False)
            write_onnx(mf, stage / f"hayai_{role}_fp16full.onnx", meta=provenance(ENGINE, role, "fp16-full", sources), external=False)
    for p in written:
        log(f"[hayai] wrote {p.name} ({p.stat().st_size:,} bytes)")
    return written


def main(argv: list[str] | None = None) -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    ap.add_argument("--fp16-full-variant", action="store_true", help="also write a full-fp16 graph to _stage/ for comparison")
    a = ap.parse_args(argv)
    run(a.out, a.fp16_full_variant)


if __name__ == "__main__":
    main()
