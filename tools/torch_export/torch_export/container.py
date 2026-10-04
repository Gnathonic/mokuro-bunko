"""Run a Linux build inside the glibc-2.28 build container (docker/Dockerfile).

The default for every Linux target: packages compiled by the host toolchain of a
rolling-release distro need that distro's glibc/libstdc++ (checks.py). The container
needs no GPU (GPU targets compile against fake devices, fakegpu.py).

Mounts: the tool sources (read-only), the 0.5.2 reference sources (read-only), the
Hugging Face cache (read-only), the work dir (venvs, caches, cuda-home), the output dir,
and, for ROCm targets, ROCM_HOME (read-only; HIP headers for the wrapper). Venvs are
created inside the container on first use from requirements-<variant>.txt.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

IMAGE = "bunko-torch-export:0.1"
HERE = Path(__file__).resolve().parent
TOOLS = HERE.parents[1]  # tools/


def _docker(*a: str, **k) -> subprocess.CompletedProcess:
    return subprocess.run(["docker", *a], **k)


def ensure_image() -> None:
    if _docker("image", "inspect", IMAGE, capture_output=True).returncode == 0:
        return
    ctx = HERE.parent / "docker"
    uv = shutil.which("uv")
    if not uv:
        raise SystemExit("uv is needed on the host to build the container image")
    shutil.copy2(uv, ctx / "uv")
    try:
        r = _docker("build", "-t", IMAGE, str(ctx))
    finally:
        (ctx / "uv").unlink(missing_ok=True)
    if r.returncode:
        raise SystemExit("docker build failed")


def run_build(argv: list[str], variant: str, out: Path, work: Path, backend: str) -> int:
    """``python -m torch_export build <argv>`` in the container; returns its exit code."""
    ensure_image()
    cw = work / "container"
    (cw / "home").mkdir(parents=True, exist_ok=True)
    out.mkdir(parents=True, exist_ok=True)
    hf = Path(os.environ.get("HF_HOME", Path.home() / ".cache/huggingface"))
    from .toolchain import REF_052

    venv = f"/work/venv-{variant}"
    req = f"/src/tools/torch_export/requirements-{variant}.txt"
    # (re)install when the requirements file changed since the venv was made
    setup = (f"cmp -s {req} {venv}/.requirements || (rm -rf {venv} && uv venv -q {venv} && VIRTUAL_ENV={venv} "
             f"uv pip install -q --python {venv}/bin/python --index-strategy unsafe-best-match -r {req} && cp {req} {venv}/.requirements)")
    vols = [
        f"{TOOLS}:/src/tools:ro", f"{REF_052}:/ref052:ro", f"{hf}:/hf:ro", f"{cw}:/work", f"{out.resolve()}:/out",
    ]
    uvc = Path(os.environ.get("UV_CACHE_DIR", Path.home() / ".cache/uv"))
    if uvc.is_dir():
        vols.append(f"{uvc}:/uvcache")
    env = {
        "HOME": "/work/home", "TORCH_EXPORT_WORK": "/work", "MOKURO_REF_052": "/ref052", "HF_HOME": "/hf",
        "HF_HUB_OFFLINE": "1", "HF_MODULES_CACHE": "/work/hf-modules", "UV_CACHE_DIR": "/uvcache",
        "TORCH_EXPORT_IN_CONTAINER": "1", "PYTHONDONTWRITEBYTECODE": "1",
    }
    for k in ("TORCH_EXPORT_INDUCTOR_OPTS", "TORCH_EXPORT_FUSE", "TORCH_EXPORT_SKIP_CHECKS", "TORCH_EXPORT_ROCM_ARCHS", "TORCH_EXPORT_NO_FLASH"):
        if k in os.environ:
            env[k] = os.environ[k]
    if backend == "rocm":
        rocm = os.environ.get("ROCM_HOME", "/opt/rocm")
        vols.append(f"{Path(rocm).resolve()}:/opt/rocm:ro")
        env["ROCM_HOME"] = "/opt/rocm"
    args = list(argv)
    i = args.index("--out") if "--out" in args else -1
    if i >= 0:
        args[i + 1] = "/out"
    else:
        args += ["--out", "/out"]
    # host network: the default bridge has no working DNS on some hosts (venv setup downloads)
    cmd = ["run", "--rm", "--network", "host", "--user", f"{os.getuid()}:{os.getgid()}", "--workdir", "/src/tools/torch_export"]
    for v in vols:
        cmd += ["-v", v]
    for k, v in env.items():
        cmd += ["-e", f"{k}={v}"]
    script = f"set -e; {setup}; exec {venv}/bin/python -m torch_export " + " ".join(f"'{a}'" for a in args)
    cmd += [IMAGE, "bash", "-c", script]
    print("+ docker", " ".join(cmd[:3]), "...", IMAGE, file=sys.stderr, flush=True)
    return _docker(*cmd).returncode
