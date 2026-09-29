"""AMD GPUs that PyTorch's ROCm wheels were not built for.

The wheels carry kernels for a list of GPU targets (``gfx1030``,
``gfx1100``, ...). A card whose own target is missing from the list -- an
RX 6600 is ``gfx1032`` -- is still reported "available", and then dumps
core at its first kernel launch. Its FAMILY's built target runs it: the
runtime is told to treat the card as that target with
``HSA_OVERRIDE_GFX_VERSION`` (``10.3.0`` for ``gfx103x``). Nothing here is a
list of cards: the card's target comes from the kernel (the kfd topology in
sysfs), the build's targets from torch itself, and an override is set only
when the family's ``...0`` target is in the build.

The variable must be in the environment before the ROCm runtime starts,
which is at torch's first device call -- not at ``import torch``: the build's
targets (``torch._C._cuda_getArchFlags()``) can be read in between. This
module imports nothing but the standard library, because the installer
also runs its source inside the OCR environments' own interpreters.
"""

from __future__ import annotations

import os
from collections.abc import MutableMapping
from pathlib import Path

OVERRIDE_VAR = "HSA_OVERRIDE_GFX_VERSION"


def gfx_name(version: int) -> str | None:
    """``100302`` (the kernel's ``gfx_target_version``) -> ``gfx1032``."""
    if version <= 0:
        return None
    major, minor, stepping = version // 10000, (version // 100) % 100, version % 100
    return f"gfx{major}{minor}{stepping:x}"


def _topology_versions(sysfs: Path) -> list[int]:
    nodes = sysfs / "class" / "kfd" / "kfd" / "topology" / "nodes"
    versions: list[int] = []
    try:
        entries = sorted(nodes.iterdir(), key=lambda p: p.name)
    except OSError:
        return versions
    for node in entries:
        try:
            text = (node / "properties").read_text(encoding="utf-8")
        except OSError:
            continue
        for line in text.splitlines():
            key, _, value = line.partition(" ")
            if key == "gfx_target_version" and value.strip().isdigit():
                versions.append(int(value.strip()))
    return versions


def device_targets(sysfs: Path = Path("/sys")) -> list[str]:
    """The GPU targets the kernel's ROCm driver sees (CPU nodes left out)."""
    return [name for name in map(gfx_name, _topology_versions(sysfs)) if name]


def amd_gpu_present(sysfs: Path = Path("/sys"), kfd: Path = Path("/dev/kfd")) -> bool:
    """An AMD GPU the ROCm driver can run: all PyTorch's ROCm wheels need.

    The wheels bring their own runtime; no system ROCm is involved.
    """
    return kfd.exists() and bool(device_targets(sysfs))


def override_for(targets: list[str], arch_flags: str) -> str | None:
    """The ``HSA_OVERRIDE_GFX_VERSION`` these cards need with this build, if any."""
    built = set(arch_flags.split())
    for target in targets:
        if target in built:
            continue
        digits = target[len("gfx"):]
        if len(digits) < 3:
            continue
        major, minor = digits[:-2], digits[-2]
        family = f"gfx{major}{minor}0"
        if family in built:
            return f"{int(major)}.{int(minor, 16)}.0"
    return None


def apply_override(
    environ: MutableMapping[str, str], arch_flags: str, sysfs: Path = Path("/sys")
) -> str | None:
    """Set the override in ``environ`` when needed; never over the user's own."""
    if environ.get(OVERRIDE_VAR):
        return None
    value = override_for(device_targets(sysfs), arch_flags)
    if value is not None:
        environ[OVERRIDE_VAR] = value
    return value


def apply_for_torch(torch: object) -> str | None:
    """For a ROCm build of ``torch`` (imported, device not yet touched)."""
    version = getattr(torch, "version", None)
    if not getattr(version, "hip", None):
        return None
    try:
        flags = torch._C._cuda_getArchFlags()  # type: ignore[attr-defined]
    except Exception:
        return None
    return apply_override(os.environ, str(flags or ""))
