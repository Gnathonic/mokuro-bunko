"""Compile targets: which torch build, which device code, which host-CPU code.

A target names what a package runs on: ``<os>-<backend>-<arch>``.

  linux-cuda-sm_75 … sm_120   CUDA, one SASS arch + its PTX (``emit_multi_arch_kernel``):
                              the PTX lets the driver JIT the kernels for any NEWER arch.
  linux-rocm-gfx1030 …        ROCm, one ISA (gfx targets are not forward compatible).
  linux-cpu-x86_64-v3         CPU kernels for AVX2 (+FMA, BMI, …) hosts, fp32 only.
  linux-cpu-x86_64-v4bf16     CPU, AVX-512 + AVX512_BF16 (Zen 4/5, Sapphire Rapids+): bf16.
  windows-cuda-sm_XX          as linux-cuda, but the wrapper is a PE .dll (built on Windows).
  macos-cpu-arm64             Apple silicon CPU (built on a Mac).

GPU packages never contain weights: every target of one engine x precision shares the
same ``weights.safetensors`` (see build.py). Their host code (the AOTI wrapper) is
compiled for x86-64-v3 so one package runs on any AVX2 host (a package built with the
default ``-march=native`` on a Zen 5 SIGILLs on a Zen 3: the shootout's lily failure).
CPU packages are frozen (oneDNN weight prepacking is a large CPU win) and so carry
their own weights.
"""

from __future__ import annotations

import platform
from dataclasses import dataclass, field

X86_V3 = {"cpp.march": "x86-64-v3", "cpp.simdlen": 256}


@dataclass(frozen=True)
class Target:
    name: str
    os: str  # linux | windows | macos
    backend: str  # cuda | rocm | cpu
    arch: str  # "86" (sm), "gfx1030", "x86-64-v3", "arm64"
    precisions: tuple[str, ...]
    torch_variant: str  # the torch build that compiles it: cu130 | rocm7.1 | cpu
    inductor: dict = field(default_factory=dict)

    @property
    def device(self) -> str:
        return "cpu" if self.backend == "cpu" else "cuda"

    @property
    def gpu(self) -> bool:
        return self.backend != "cpu"

    @property
    def sm(self) -> tuple[int, int]:
        assert self.backend == "cuda"
        n = int(self.arch)
        return n // 10, n % 10


def _cuda(os_: str, sm: str) -> Target:
    precs = ("fp32", "bf16", "fp16") if int(sm) >= 80 else ("fp32", "fp16")  # no bf16 before Ampere
    host = X86_V3
    return Target(f"{os_}-cuda-sm_{sm}", os_, "cuda", sm, precs, "cu130",
                  {**host, "aot_inductor.emit_multi_arch_kernel": True})


def _rocm(gfx: str) -> Target:
    return Target(f"linux-rocm-{gfx}", "linux", "rocm", gfx, ("fp32", "bf16", "fp16"), "rocm7.1", dict(X86_V3))


TARGETS: dict[str, Target] = {}
for _os in ("linux", "windows"):
    for _sm in ("75", "80", "86", "89", "90", "120"):
        t = _cuda(_os, _sm)
        TARGETS[t.name] = t
for _gfx in ("gfx1030", "gfx1100", "gfx1101", "gfx1102", "gfx1200", "gfx1201"):
    t = _rocm(_gfx)
    TARGETS[t.name] = t
# CPU kernels read the OpenMP thread count at run time (else the compile host's core
# count is baked into every `#pragma omp parallel num_threads(N)`)
CPU = {"freezing": True, "cpp.dynamic_threads": True}
TARGETS["linux-cpu-x86_64-v3"] = Target("linux-cpu-x86_64-v3", "linux", "cpu", "x86-64-v3", ("fp32",), "cpu",
                                        {**X86_V3, **CPU})
TARGETS["linux-cpu-x86_64-v4bf16"] = Target(
    "linux-cpu-x86_64-v4bf16", "linux", "cpu", "x86-64-v4", ("bf16",), "cpu",
    {"cpp.march": "x86-64-v4", "cpp.simdlen": 512, **CPU})
TARGETS["windows-cpu-x86_64-v3"] = Target("windows-cpu-x86_64-v3", "windows", "cpu", "x86-64-v3", ("fp32",), "cpu", dict(CPU))
TARGETS["macos-cpu-arm64"] = Target("macos-cpu-arm64", "macos", "cpu", "arm64", ("fp32",), "cpu", dict(CPU))


def host_os() -> str:
    return {"Linux": "linux", "Windows": "windows", "Darwin": "macos"}[platform.system()]


def get(name: str) -> Target:
    try:
        return TARGETS[name]
    except KeyError:
        raise SystemExit(f"unknown target {name!r}; known: {', '.join(TARGETS)}") from None
