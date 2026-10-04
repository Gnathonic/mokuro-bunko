"""Compile AOTInductor GPU packages for a GPU this machine does not have (or has but
must not use): answers A1 of docs/rust-port/TORCH-BACKEND.md.

torch 2.13's AOTI asks the live device three things: its properties (to pick the
Triton target and heuristics), the Triton driver's current target, and -- in the
compile-time "autotune block" -- to launch every kernel once so the chosen config's
binary lands in ``CudaKernelParamCache``. ``install()`` answers the first two with
the target's numbers and replaces the autotune block with "compile every kernel for
the target, keep the heuristics' first config, record it, never launch". Nothing is
loaded onto a GPU; no driver (libcuda / libamdhip64 device) is needed.

Consequences, by design: no benchmarking anywhere (pad_mm off, no autotuning), so
packages are deterministic; kernels use the heuristics' first config. Call
``install()`` AFTER ``torch.export`` (export runs in its own FakeTensorMode) and
before ``aoti_compile_and_package``. Pinned to torch 2.13.0 / triton 3.7.1 internals.
"""

from __future__ import annotations

import types

import torch


def install(backend: str, arch: str, sms: int = 46) -> None:
    hip = backend == "rocm"
    if hip:
        # inductor's cc on ROCm is the gfx name; HIP reports gfx1030 as major 10 minor 3
        major, minor, cc = int(arch[3:-2]), int(arch[-2], 16), arch
    else:
        major, minor = int(arch) // 10, int(arch) % 10
        cc = major * 10 + minor
    props = types.SimpleNamespace(
        name=f"fake {arch}", major=major, minor=minor, multi_processor_count=sms,
        regs_per_multiprocessor=65536, max_threads_per_multi_processor=1536 if (major, minor) >= (8, 6) else 2048,
        max_threads_per_block=1024, warp_size=32, total_memory=8 << 30, gcnArchName=arch if hip else "",
        L2_cache_size=4 << 20, shared_memory_per_block_optin=101376 if not hip else 65536, uuid="fake")
    c = torch.cuda

    # Pattern tables (sdpa fusion, pad_mm, ...) are traced once with example tensors on the
    # graph's device; trace them on cpu (the patterns are device-agnostic fx graphs).
    from torch._inductor.fx_passes import joint_graph, post_grad

    for mod in (joint_graph, post_grad):
        orig = mod.lazy_init

        def wrapped(input_device=None, *a, _orig=orig, **k):
            avail = c.is_available
            c.is_available = lambda: False
            try:
                return _orig(torch.device("cpu"), *a, **k)
            finally:
                c.is_available = avail

        mod.lazy_init = wrapped

    # joint-graph constant folding evaluates small constant subgraphs (scalar_tensor, full,
    # pointwise) with REAL tensors on the graph's device to find uniform values; it only
    # keeps the value (the replacement full() takes the device from the fake tensor), so
    # evaluate them on the cpu.
    from torch.utils._python_dispatch import TorchDispatchMode

    class _OnCpu(TorchDispatchMode):
        def __torch_dispatch__(self, func, types, args=(), kwargs=None):
            kwargs = dict(kwargs or {})
            dev = kwargs.get("device")
            if dev is not None and torch.device(dev).type == "cuda":
                kwargs["device"] = torch.device("cpu")
            return func(*args, **kwargs)

    folder = joint_graph.UniformValueConstantFolder
    orig_run = folder.run

    def run_on_cpu(self, *a, **k):
        with _OnCpu():
            return orig_run(self, *a, **k)

    folder.run = run_on_cpu

    rng = torch.zeros(16, dtype=torch.uint8)
    for k, v in {
        "is_available": lambda: True, "device_count": lambda: 1, "current_device": lambda: 0,
        "get_device_capability": lambda device=None: (major, minor),
        "get_device_properties": lambda device=None: props,
        "get_device_name": lambda device=None: props.name,
        "is_bf16_supported": lambda including_emulation=True: hip or major >= 8,
        "synchronize": lambda device=None: None, "_lazy_init": lambda: None, "set_device": lambda d: None,
        "_exchange_device": lambda i: 0, "_maybe_exchange_device": lambda i: 0,
        "get_rng_state": lambda device="cuda": rng.clone(), "set_rng_state": lambda state, device="cuda": None,
        "get_rng_state_all": lambda: [rng.clone()], "set_rng_state_all": lambda s: None,
    }.items():
        setattr(c, k, v)
    c.random.get_rng_state = c.get_rng_state
    c.random.set_rng_state = c.set_rng_state

    from torch._dynamo import device_interface as di

    class _NoGuard:
        def __init__(self, *a, **k): ...
        def __enter__(self): return self
        def __exit__(self, *a): return False

    I = di.CudaInterface
    for k, v in {
        "get_device_properties": lambda device=None: props, "get_compute_capability": lambda device=None: cc,
        "is_available": lambda: True, "device_count": lambda: 1, "current_device": lambda: 0,
        "set_device": lambda d: None, "synchronize": lambda device=None: None,
        "is_bf16_supported": lambda including_emulation=False: hip or major >= 8,
        "exchange_device": lambda i: 0, "maybe_exchange_device": lambda i: 0,
    }.items():
        setattr(I, k, staticmethod(v))
    I.device = _NoGuard

    # Triton: kernels are compiled for the target, never loaded or launched here; the
    # driver only has to say what the target is.
    from triton.backends.compiler import GPUTarget
    from triton.compiler.compiler import CompiledKernel
    from triton.runtime.driver import driver as tdriver

    CompiledKernel._init_handles = lambda self: None
    if hip:
        from triton.backends.amd.driver import HIPDriver as Base
    else:
        from triton.backends.nvidia.driver import CudaDriver as Base

    class FakeDriver(Base):
        def __init__(self):
            self.utils = None
            self.launcher_cls = None
            self.get_current_device = lambda: 0
            self.set_current_device = lambda d: None
            self.get_current_stream = lambda d=None: 0

        def get_current_target(self):
            return GPUTarget("hip" if hip else "cuda", cc, 32)

        def get_active_torch_device(self):
            return torch.device("cuda", 0)

    fake = FakeDriver()
    tdriver._default = fake
    tdriver._active = fake

    # constants are FakeTensors (no storage): dedupe them by identity
    import torch._inductor.graph as G
    import torch._inductor.utils as U
    from torch._subclasses.fake_tensor import FakeTensor

    orig_same = U.is_same_tensor

    def is_same_tensor(data, value):
        if isinstance(data, FakeTensor) or isinstance(value, FakeTensor):
            return data is value
        return orig_same(data, value)

    U.is_same_tensor = is_same_tensor
    G.is_same_tensor = is_same_tensor

    import torch._inductor.config as cfg
    from torch._inductor.codegen import wrapper as W

    cfg.compile_threads = 1  # compile in this process (subprocess workers would not see the patches)
    W.PythonWrapperCodegen.generate_and_run_autotune_block = _compile_without_launch


def _compile_without_launch(self) -> None:
    """Stands in for the compile-time autotune block."""
    from torch._inductor.runtime.triton_heuristics import CachingAutotuner

    scope: dict = {}
    exec(self.kernel_autotune_defs.getvalue() + "\nasync_compile.wait(globals())\n", scope)
    for k in scope.values():
        if not isinstance(k, CachingAutotuner):
            continue
        if not k.launchers:
            k.precompile()
        # no GPU, no benchmark: the heuristics' first config
        k.save_gpu_kernel(None, k.launchers[0])
