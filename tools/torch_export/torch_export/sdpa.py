"""Pin each scaled_dot_product_attention to the kernel the TARGET GPU would pick.

``aten.scaled_dot_product_attention`` is a composite: at trace time it asks the live
device which fused kernel to use (``_fused_sdp_choice``, C++ ``select_sdp_backend``) and
falls back to the unfused math path when there is no device -- a package compiled
without the GPU would then run attention as bmm+softmax (measured: 285 vs 377 crops/s
for hayai bf16 on an RTX 4090), and with different numerics than 0.5.2's eager run.

``pin(ep, target)`` re-traces the exported program with a decomposition of the op that
replays the composite (torch 2.13 ``attention.cpp``) for the backend the target's
selection rules give (CUDA: ``cuda/sdp_utils.cpp``; ROCm: probed on real cards, see
ROCM_RULES). The same rules are checked against real devices by ``sdpa_probe.py``.
"""

from __future__ import annotations

import math
import os

import torch

aten = torch.ops.aten
HALF = (torch.float16, torch.bfloat16)


def cuda_backend(sm: int, dtype: torch.dtype, has_mask: bool, hd_q: int, hd_v: int, flash: bool = True) -> str:
    """torch 2.13 cu130 select_sdp_backend for dense, inference-mode, non-causal, no-dropout calls.
    ``flash=False``: the Windows wheels are built without flash attention (USE_FLASH_ATTENTION
    off; measured on pimax: "USE_FLASH_ATTENTION was not enabled for build")."""
    major = sm // 10
    if major in (9, 10) and dtype in HALF and hd_q == hd_v and hd_q <= 256 and hd_q % 8 == 0:
        return "cudnn"  # check_prefer_cudnn_attention: cuDNN >= 9.15.1 first on sm9x/sm10x
    if flash and not has_mask and dtype in HALF and 80 <= sm <= 121 and hd_q == hd_v and hd_q <= 256 and hd_q % 8 == 0:
        return "flash"
    if 50 <= sm <= 121 and (dtype != torch.bfloat16 or sm >= 80):
        align = 8 if dtype in HALF else 4
        if hd_q % align == 0 and hd_v % align == 0:
            return "efficient"
    return "math"


def _aotriton(dtype, has_mask, hd_q, hd_v):
    # same outcome as CUDA sm80+ for our shapes (probed: gfx1201 == sm_89 on all 30 cases)
    return cuda_backend(89, dtype, has_mask, hd_q, hd_v)


def _math(dtype, has_mask, hd_q, hd_v):
    return "math"


# ROCm: fused SDPA only where the wheel's AOTriton has images (torch/lib/aotriton.images:
# gfx110x, gfx115x, gfx120x, gfx90a, gfx942, gfx950); elsewhere (RDNA2 gfx103x) math.
# Probed with torch 2.13.0+rocm7.1 (python -m torch_export.sdpa_probe): gfx1201 (RX 9070 XT)
# and gfx1030 (RX 6900 XT). gfx110x is assumed non-experimental (not probed: no card).
ROCM_RULES: dict[str, object] = {
    **{a: _aotriton for a in ("gfx1100", "gfx1101", "gfx1102", "gfx1150", "gfx1151", "gfx1200", "gfx1201")},
    **{a: _math for a in ("gfx1030", "gfx1031", "gfx1032", "gfx1034", "gfx1035")},
}


def rocm_backend(arch: str, dtype: torch.dtype, has_mask: bool, hd_q: int, hd_v: int) -> str:
    rule = ROCM_RULES.get(arch)
    if rule is None:
        raise SystemExit(f"no probed SDPA rules for {arch}: run sdpa_probe.py on such a card and add them")
    return rule(dtype, has_mask, hd_q, hd_v)  # type: ignore[operator]


def _aligned(t: torch.Tensor, alignment: int) -> bool:
    for i in range(t.dim() - 1):
        s = t.stride(i)
        if not isinstance(s, int) or s % alignment:
            return False
    return t.stride(-1) == 1


def _decomp(choose):
    def sdpa(q, k, v, attn_mask=None, dropout_p=0.0, is_causal=False, scale=None, enable_gqa=False):
        assert dropout_p == 0.0 and not is_causal and not enable_gqa
        backend = choose(q.dtype, attn_mask is not None, q.shape[-1], v.shape[-1])
        if attn_mask is not None and attn_mask.dtype == torch.bool:
            attn_mask = torch.where(attn_mask, 0.0, float("-inf")).to(q.dtype)  # convert_boolean_attn_mask
        if backend == "efficient":
            if attn_mask is not None:
                if not _aligned(attn_mask, 8):  # preprocess_mask / pad_bias<8>
                    n = attn_mask.shape[-1]
                    attn_mask = torch.nn.functional.pad(attn_mask, (0, 8 - n % 8))[..., :n]
                attn_mask = attn_mask.expand(q.shape[0], q.shape[1], q.shape[2], k.shape[2])
            return aten._scaled_dot_product_efficient_attention(q, k, v, attn_mask, False, 0.0, False, scale=scale)[0]
        if backend == "flash":
            og = scale if scale is not None else 1.0 / math.sqrt(q.shape[-1])
            return aten._scaled_dot_product_flash_attention(q, k, v, 0.0, False, False, scale=og)[0]
        if backend == "cudnn":
            return aten._scaled_dot_product_cudnn_attention(q, k, v, attn_mask, False, 0.0, False, False, scale=scale)[0]
        return aten._scaled_dot_product_attention_math(q, k, v, attn_mask, 0.0, False, None, scale=scale)[0]

    return sdpa


def pin(ep, target):
    if target.backend == "cuda":
        sm = int(target.arch)
        # experiment knob: TORCH_EXPORT_NO_FLASH=1 builds a Linux package with Windows' choices
        flash = target.os != "windows" and os.environ.get("TORCH_EXPORT_NO_FLASH") != "1"
        choose = lambda dt, m, hq, hv: cuda_backend(sm, dt, m, hq, hv, flash)  # noqa: E731
    else:
        choose = lambda dt, m, hq, hv: rocm_backend(target.arch, dt, m, hq, hv)  # noqa: E731
    return ep.run_decompositions({aten.scaled_dot_product_attention.default: _decomp(choose)})
