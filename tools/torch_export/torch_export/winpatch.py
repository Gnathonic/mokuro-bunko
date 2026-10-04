"""torch 2.13 inductor fixes for NATIVE Windows (MSVC) AOTInductor builds, applied in
memory (the venv is not edited) and only when a Windows target is built on Windows.

Found by the shootout on pimax (fleet-nv/patch_inductor.py, which edited the venv):

1. The GPU wrapper codegen emits ``static __attribute__((noinline)) void`` for every
   Triton launcher: GCC-only syntax, MSVC rejects it. Rewritten to ``AOTI_NOINLINE static
   void`` (AOTI's portable macro) in the generated C++ before it is compiled.
2. The native-Windows CUDA wrapper calls cudaMalloc/cudaEvent*/cudaMemcpy, but inductor
   links ``cudart`` only for Linux->Windows cross builds: added for native CUDA builds.
3. Weights must stay out of the .dll (inductor would emit them as a multi-GB C++ byte
   array that MSVC never finishes compiling): build.py sets
   ``package_constants_on_disk_format=binary_blob`` for every Windows target.

Linux->Windows cross builds (build.py, ``cross_target_platform="windows"``) need none of
these: MinGW accepts the GCC attribute and inductor links cudart itself there.
"""

from __future__ import annotations

import sys


def install() -> None:
    if sys.platform != "win32":
        raise RuntimeError("winpatch is for native Windows builds only")
    from torch._inductor import codecache, cpp_builder

    old = "static __attribute__((noinline)) void"
    new = "AOTI_NOINLINE static void"
    compile_ = codecache.AotCodeCompiler.compile.__func__

    def compile(cls, graph, wrapper_code, kernel_code, *a, **k):
        return compile_(cls, graph, wrapper_code.replace(old, new), kernel_code.replace(old, new), *a, **k)

    codecache.AotCodeCompiler.compile = classmethod(compile)

    opts = cpp_builder.get_cpp_torch_device_options

    def device_options(device_type, aot_mode=False, compile_only=False):
        out = opts(device_type, aot_mode, compile_only)
        libraries = out[5]
        if device_type == "cuda" and "cudart" not in libraries:
            libraries.append("cudart")
        return out

    cpp_builder.get_cpp_torch_device_options = device_options
