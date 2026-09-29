"""Stage the engine runner where the engines environment can execute it.

The runner, the modules it imports and the detector adapters are all run or
imported BY PATH from the engines environment, which does not have
``mokuro_bunko`` installed -- and which may be reading out of a zipapp. So
they are copied out to disk.

They are copied ONCE per content hash of those files, into a stable directory
under ``<storage>/.processing/``. Not into each volume's workspace, which is
what this used to do: a session keeps one runner open across many volumes and
there is no workspace that outlives them, and the per-volume path was paying
eleven file copies for a set of files that only change when the server is
upgraded.

Its own module, and not part of the processor, because the session holds a
staged build for the life of its subprocess and the processor imports the
session.
"""

from __future__ import annotations

import hashlib
import importlib.resources
import os
import shutil
import threading
from pathlib import Path

# The files the staged runner is made of. `engine_runner.py` is the script;
# the rest land beside it and are imported by adding its own directory to
# `sys.path` (the `ppocr_manga` detector adapter finds `ppocr.py` there too,
# one directory above itself).
RUNNER_MODULES: tuple[str, ...] = (
    "engine_runner.py",
    "ppocr.py",
    "line_layout.py",
    "line_reconcile.py",
    "rocm_gfx.py",
)

# One directory per content hash, named with this prefix so a prune can tell
# a staged build from a volume workspace beside it.
RUNNER_STAGE_PREFIX = "runner-"

# Staged runner directories a session or a job is using right now, by path.
# A directory in here is never pruned: the runner imports its detector
# adapters by path, lazily, so deleting the copy a live subprocess was
# started from would break it mid-volume.
_staged_in_use: dict[Path, int] = {}
_stage_lock = threading.Lock()


def _runner_sources() -> list[tuple[str, str]]:
    """``(relative path, text)`` of every file the staged runner is made of."""
    ocr_pkg = importlib.resources.files("mokuro_bunko.ocr")
    sources = [
        (name, ocr_pkg.joinpath(name).read_text(encoding="utf-8")) for name in RUNNER_MODULES
    ]
    for entry in sorted(ocr_pkg.joinpath("detectors").iterdir(), key=lambda e: e.name):
        if entry.name.endswith(".py"):
            sources.append((f"detectors/{entry.name}", entry.read_text(encoding="utf-8")))
    return sources


def _digest(sources: list[tuple[str, str]]) -> str:
    digest = hashlib.sha256()
    for name, text in sources:
        digest.update(name.encode("utf-8"))
        digest.update(b"\0")
        digest.update(text.encode("utf-8"))
        digest.update(b"\0")
    return digest.hexdigest()[:16]


def runner_digest() -> str:
    """The content hash a build of the runner is staged under, as it reads NOW.

    The hash half of :func:`stage_runner`, on its own: a processor pins the
    build it started with and compares this against it at every session
    open, to say -- once -- that the code on disk has moved on under it.
    Reads the same eleven small files staging does, and writes nothing.
    """
    return _digest(_runner_sources())


def stage_runner(storage_path: Path) -> Path:
    """Stage the engine runner once per content hash; return its script path.

    ``<storage>/.processing/runner-<hash>/engine_runner.py``, with the
    modules it imports beside it and the detector adapters under
    ``detectors/``. The hash is over the CONTENT of those files, so an
    unchanged server stages nothing after the first time and an upgraded one
    stages into a new directory rather than rewriting files a running
    subprocess may still import.

    Staged copies of other builds are pruned in the same pass, except any a
    live session or job is still using (:data:`_staged_in_use`).
    """
    sources = _runner_sources()
    stage_root = storage_path / ".processing"
    target = stage_root / f"{RUNNER_STAGE_PREFIX}{_digest(sources)}"
    runner_path = target / "engine_runner.py"
    with _stage_lock:
        if not runner_path.is_file():
            target.mkdir(parents=True, exist_ok=True)
            (target / "detectors").mkdir(parents=True, exist_ok=True)
            for name, text in sources:
                # Written to a temporary name and moved into place, so a
                # second process staging the same build concurrently never
                # exposes a half-written module to an interpreter.
                path = target / name
                tmp = path.with_name(f".{path.name}.tmp{os.getpid()}")
                tmp.write_text(text, encoding="utf-8")
                os.replace(tmp, path)
        _prune_staged_runners(stage_root, keep=target)
    return runner_path


def _prune_staged_runners(stage_root: Path, *, keep: Path) -> None:
    """Remove staged runners of other builds that nothing is using."""
    try:
        entries = list(stage_root.iterdir())
    except OSError:
        return
    for entry in entries:
        if not entry.is_dir() or not entry.name.startswith(RUNNER_STAGE_PREFIX):
            continue
        if entry == keep or _staged_in_use.get(entry):
            continue
        shutil.rmtree(entry, ignore_errors=True)


def pin_runner(storage_path: Path) -> Path:
    """Stage the runner and hold it for the life of this process.

    What a PROCESSOR does once, at start (design section 5.1): every session
    and benchmark it opens runs this build until the process exits, however
    the code on disk moves meanwhile -- the bridge that drives the runner was
    imported once too, and the two must stay the same version. The hold is
    never released, so no prune can ever take the build away.
    """
    runner = stage_runner(storage_path)
    hold_staged_runner(runner)
    return runner


def hold_staged_runner(runner_path: Path) -> None:
    """Mark a staged runner as in use, so pruning leaves it alone."""
    with _stage_lock:
        directory = runner_path.parent
        _staged_in_use[directory] = _staged_in_use.get(directory, 0) + 1


def release_staged_runner(runner_path: Path) -> None:
    """Undo one :func:`hold_staged_runner`."""
    with _stage_lock:
        directory = runner_path.parent
        remaining = _staged_in_use.get(directory, 0) - 1
        if remaining > 0:
            _staged_in_use[directory] = remaining
        else:
            _staged_in_use.pop(directory, None)
