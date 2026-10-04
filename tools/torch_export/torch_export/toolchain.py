"""Process environment for a compile, set BEFORE torch is imported.

CUDA: no system toolkit is needed. The cu130 venv's NVIDIA wheels (runtime, crt, cccl,
nvcc/fatbinary, nvvm; pinned in requirements-cu130.txt) are assembled into a CUDA_HOME
under the work dir, plus a link-time stub ``libcuda.so`` (SONAME libcuda.so.1, every
driver-API entry point of cuda.h returning an error) so the AOTI wrapper links on a
machine without the NVIDIA driver; at run time the real driver's libcuda.so.1 is used.

ROCm: ROCM_HOME (default /opt/rocm) provides clang/lld for the wrapper and the code
objects; the torch rocm7.1 wheel provides libamdhip64 / libtorch_hip.
"""

from __future__ import annotations

import os
import re
import subprocess
import sysconfig
from pathlib import Path

WORK = Path(os.environ.get("TORCH_EXPORT_WORK", Path.home() / ".cache/mokuro-bunko-demo/tmp/torch-export"))
REF_052 = Path(os.environ.get("MOKURO_REF_052", Path(__file__).resolve().parents[4] / "ref-0.5.2"))


def prepare(target, work: Path = WORK) -> dict[str, str]:
    env = {
        "TMPDIR": str(work / "tmp"),
        "TORCHINDUCTOR_CACHE_DIR": str(work / "inductor-cache" / target.name),
        "HF_HUB_OFFLINE": os.environ.get("HF_HUB_OFFLINE", "1"),
        "MOKURO_REF_052": str(REF_052),
        "TORCHINDUCTOR_FX_GRAPH_REMOTE_CACHE": "0",
        "TORCHINDUCTOR_AUTOGRAD_REMOTE_CACHE": "0",
    }
    for d in (env["TMPDIR"], env["TORCHINDUCTOR_CACHE_DIR"]):
        Path(d).mkdir(parents=True, exist_ok=True)
    if target.backend == "cuda":
        home = cuda_home(work)
        env.update({
            "CUDA_HOME": str(home), "CUDA_PATH": str(home), "CUDACXX": str(home / "bin" / "nvcc"),
            "LIBRARY_PATH": os.pathsep.join(filter(None, [str(home / "lib64" / "stubs"), os.environ.get("LIBRARY_PATH")])),
            "TORCH_CUDA_ARCH_LIST": f"{int(target.arch) // 10}.{int(target.arch) % 10}",
        })
        if target.os == "windows" and os.name != "nt":
            env.update(windows_cross(work))
    elif target.backend == "rocm":
        env["ROCM_HOME"] = os.environ.get("ROCM_HOME", "/opt/rocm")
        # experiment: TORCH_EXPORT_ROCM_ARCHS="gfx1030;gfx1201" + aot_inductor.emit_multi_arch_kernel
        # bundles code objects for several ISAs (compiled from the target's LLVM IR)
        env["PYTORCH_ROCM_ARCH"] = os.environ.get("TORCH_EXPORT_ROCM_ARCHS", target.arch)
    os.environ.update(env)
    return env


def cuda_home(work: Path) -> Path:
    """$work/cuda-home: the cu13 wheels' toolkit + the libcuda link stub (idempotent)."""
    src = Path(sysconfig.get_paths()["purelib"]) / "nvidia" / "cu13"
    if not (src / "bin" / "nvcc").exists() or not (src / "include" / "crt").exists():
        raise SystemExit(f"{src}: CUDA wheels incomplete; install requirements-cu130.txt")
    home = work / "cuda-home"
    lib = home / "lib64"
    stub = lib / "stubs" / "libcuda.so"
    if stub.exists():
        return home
    (lib / "stubs").mkdir(parents=True, exist_ok=True)
    for name in ("include", "bin", "nvvm"):
        if (src / name).exists() and not (home / name).exists():
            (home / name).symlink_to(src / name)
    for f in (src / "lib").iterdir():
        if not (lib / f.name).exists():
            (lib / f.name).symlink_to(f)
    if not (lib / "libcudart.so").exists():
        (lib / "libcudart.so").symlink_to("libcudart.so.13")
    pre = subprocess.run(["gcc", "-E", "-P", "-x", "c", str(src / "include" / "cuda.h"), f"-I{src / 'include'}"],
                         capture_output=True, text=True, check=True).stdout
    syms = sorted(set(re.findall(r"\bCUresult\s+(?:CUDAAPI\s+)?(cu\w+)\s*\(", pre)))
    c = home / "libcuda_stub.c"
    c.write_text("/* link-time stub: the NVIDIA driver provides the real libcuda.so.1 */\n"
                 + "".join(f"int {s}(void){{return 999;}}\n" for s in syms))
    subprocess.run(["gcc", "-shared", "-fPIC", "-Wl,-soname,libcuda.so.1", "-o", str(stub), str(c)], check=True)
    return home


WIN_TORCH_WHEEL = "torch-2.13.0+cu130-cp312-cp312-win_amd64.whl"
WIN_CUDART_WHEEL = "nvidia_cuda_runtime-13.0.96-py3-none-win_amd64.whl"
LLVM_MINGW = "llvm-mingw-20260922-ucrt-ubuntu-22.04-x86_64"


def windows_cross(work: Path) -> dict[str, str]:
    """Cross-compiling the Windows wrapper .dll on Linux (torch 2.13
    ``aot_inductor.cross_target_platform="windows"``): a MinGW-w64 g++ (llvm-mingw),
    the Windows torch wheel's import libraries (torch_cpu.lib, torch_cuda.lib: the
    wrapper uses only their stable C shim) and the Windows CUDA runtime wheel
    (cuda.lib, cudart.lib, cudart64_13.dll). Fetched once into $work/win/ (see
    fetch_windows_cross)."""
    win = work / "win"
    mingw = win / LLVM_MINGW / "bin"
    cuda = win / "cuda" / "nvidia" / "cu13"
    lib = win / "torchlib"
    missing = [p for p in (mingw / "x86_64-w64-mingw32-g++", cuda / "lib" / "x64" / "cuda.lib", lib / "torch_cpu.lib") if not p.exists()]
    if missing:
        raise SystemExit(f"Windows cross toolchain missing ({missing[0]}): python -m torch_export fetch-windows-cross")
    x64 = cuda / "bin" / "x64"
    if not x64.exists():
        x64.symlink_to("x86_64")  # inductor looks for bin/x64/cudart64_*.dll
    # cuda.lib (driver API) is a hybrid MSVC library pulling LIBCMT/OLDNAMES: give the
    # MinGW linker a plain import library of nvcuda.dll instead (same entry points as cuda.h)
    imp = cuda / "lib" / "x64" / "libcuda.a"
    if not imp.exists():
        src = Path(sysconfig.get_paths()["purelib"]) / "nvidia" / "cu13" / "include"
        pre = subprocess.run(["gcc", "-E", "-P", "-x", "c", str(src / "cuda.h"), f"-I{src}"],
                             capture_output=True, text=True, check=True).stdout
        syms = sorted(set(re.findall(r"\bCUresult\s+(?:CUDAAPI\s+)?(cu\w+)\s*\(", pre)))
        d = imp.with_name("nvcuda.def")
        d.write_text("LIBRARY nvcuda.dll\nEXPORTS\n" + "".join(f"  {x}\n" for x in syms))
        subprocess.run([str(mingw / "x86_64-w64-mingw32-dlltool"), "-d", str(d), "-l", str(imp), "-D", "nvcuda.dll"], check=True)
    # inductor drives "x86_64-w64-mingw32-g++" with GCC-only flags; llvm-mingw is clang:
    # a wrapper drops the flags clang rejects
    wrap = win / "wrap"
    wrap.mkdir(exist_ok=True)
    for tool in ("g++", "gcc"):
        w = wrap / f"x86_64-w64-mingw32-{tool}"
        w.write_text("#!/bin/sh\n# drop GCC-only flags inductor passes\nfor a in \"$@\"; do shift; case \"$a\" in "
                     "-fno-tree-loop-vectorize|-fexcess-precision=fast) ;; *) set -- \"$@\" \"$a\";; esac; done\n"
                     f"exec {mingw}/x86_64-w64-mingw32-{tool} \"$@\"\n")
        w.chmod(0o755)
    return {"PATH": f"{wrap}{os.pathsep}{mingw}{os.pathsep}{os.environ['PATH']}", "WINDOWS_CUDA_HOME": str(cuda),
            "TORCH_EXPORT_WIN_TORCHLIB": str(lib)}


def fetch_windows_cross(work: Path = WORK) -> None:
    """Download the pinned Windows wheels + llvm-mingw into $work/win (dev machine only)."""
    import tarfile
    import urllib.request
    import zipfile

    win = work / "win"
    win.mkdir(parents=True, exist_ok=True)
    pins = {
        WIN_TORCH_WHEEL: "https://download.pytorch.org/whl/cu130/torch-2.13.0%2Bcu130-cp312-cp312-win_amd64.whl",
        WIN_CUDART_WHEEL: "https://files.pythonhosted.org/packages/py3/n/nvidia-cuda-runtime/" + WIN_CUDART_WHEEL,
        LLVM_MINGW + ".tar.xz": f"https://github.com/mstorsjo/llvm-mingw/releases/download/20260922/{LLVM_MINGW}.tar.xz",
    }
    for name, url in pins.items():
        if not (win / name).exists():
            print(f"fetching {url}", flush=True)
            urllib.request.urlretrieve(url, win / name)
    with zipfile.ZipFile(win / WIN_TORCH_WHEEL) as z:
        for n in z.namelist():
            if n.startswith("torch/lib/") and n.endswith(".lib"):
                (win / "torchlib").mkdir(exist_ok=True)
                (win / "torchlib" / Path(n).name).write_bytes(z.read(n))
    with zipfile.ZipFile(win / WIN_CUDART_WHEEL) as z:
        z.extractall(win / "cuda", [n for n in z.namelist() if n.startswith("nvidia/cu13/")])
    if not (win / LLVM_MINGW).exists():
        with tarfile.open(win / (LLVM_MINGW + ".tar.xz")) as t:
            t.extractall(win)
