"""Standalone OCR engine runner: one volume, one engine, one mokuro-format file.

This file is executed by the *engines* virtual environment's Python (torch,
transformers 5, peft), never imported by the server. It is copied
into the processing workspace together with the detector adapters and run by
path, so it must stay self-contained: standard library plus what the engines
environment provides.

Pipeline per volume:
  detector adapter (separate process, see detectors/README.md)
      -> per-page JSON: blocks with line quads, vertical flag, font size
  crops per line     -> engine-specific (see ``CROP_MODES``)
  recognizer         -> one string per crop
  page dict          -> {version, img_width, img_height, blocks: [...]}

The ``ppocr-manga`` engine (see ``LINE_ENGINES``) is the exception: detector
and recognizer are one model pair read in THIS process (``ppocr.py``), and
the lines they return are grouped into blocks by ``line_layout.py``:
  page image -> rotated line quads + text -> ruby removed, bubbles and
  paragraphs in reading order -> page dict

Another engine on the ``ppocr-manga`` DETECTOR (``LAYOUT_DETECTORS``) takes the
same road and then reads every text line a second time with its own
recognizer; ``line_reconcile.py`` merges the two reads per line:
  ... -> line quads + CTC text -> deskewed line crops -> engine text ->
  merged text (engine's kana/kanji, CTC's brackets, widths and blank cells)
  -> same layout -> page dict

Nothing here places characters INSIDE a line: a line is a quad and its text,
and a reader lays that text on a uniform grid along it.

Usage:
  engine_runner.py --engine hayai-nova --detector ctd --input <dir> --output <file>.mokuro \
      --cache-dir <dir> [--patches 512] [--volume-uuid U] [--generator S]

The functions that build page/volume dicts take the detector output and the
recognizer as plain data/callables so they are unit-tested with fakes and
without torch.
"""

from __future__ import annotations

import argparse
import contextlib
import functools
import importlib
import itertools
import json
import math
import multiprocessing
import os
import queue
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import unicodedata
import uuid
import zipfile
from collections import deque
from collections.abc import Callable, Generator, Iterable, Iterator, Mapping, Sequence
from pathlib import Path
from typing import Any, NamedTuple, cast

# As close to "this process started" as an imported module can stand: the
# interpreter plus this file's stdlib-only imports are all that has happened
# before it. Every "first page after X s" a benchmark reports is measured from
# here, and NOTHING measured from here is ever added to a rate (ADDENDUM 9).
PROCESS_STARTED = time.monotonic()

IMAGE_EXTENSIONS = (".jpg", ".jpeg", ".png", ".webp", ".avif")

# mokuro file-format version written into the output. Readers key format
# handling off this; 0.2.x is the current volume schema.
MOKURO_FORMAT_VERSION = "0.2.5"

# Crop geometry each engine expects. "line" reproduces mokuro's crops: the
# line quad is deskewed to a fixed text height, vertical lines rotated to
# horizontal, long lines split into chunks. "upright" keeps the line in page
# orientation with a margin, which is what the PaddleOCR-VL manga LoRA was
# trained on.
CROP_MODES: dict[str, str] = {
    "hayai-nova": "line",
    "paddle-manga": "upright",
}

RECOGNIZER_REPOS: dict[str, str] = {
    "hayai-nova": "JustANormalTinkerer/hayai-ocr-v2.5-nova",
    "paddle-manga": "sorryhyun/paddleocr-vl-1.6-manga-lora",
    "ppocr-manga": "Kellenok/PP-OCRv6_manga",
}
# Deliberately NOT here: the served engines' recognizers. This file never
# resolves them -- their own process does, in its own environment -- so
# pinning one here would be a claim about weights nothing in this process
# loaded, and the sidecar a served engine writes names no weights at all
# (it is pure upstream mokuro, as its own CLI writes it).

# hayai-nova is SigLIP2 NaFlex on the vision side and its repo ships no
# preprocessor config, so the processor is loaded from the stock SigLIP2 repo.
HAYAI_VISION_REPO = "google/siglip2-base-patch16-naflex"

# EVERY Hugging Face repo this file resolves, pinned to a commit.
#
# Two reasons, and the first is not about reproducibility: the hayai-nova and
# PaddleOCR-VL repos are loaded with ``trust_remote_code=True``, so their own
# Python runs in this process. Resolving them to a moving ``main`` would mean
# a push to someone else's repo silently changes the code we execute on a
# self-hoster's machine. Pinning makes that an explicit, reviewable bump.
# (``ppocr.py`` pins its own repo the same way, in ``REPO_REVISION``.)
# The second reason is that the sidecar can then say which weights read it:
# every ``revision`` passed below is stamped into ``ocr_engine.weights``.
#
# TO BUMP A PIN: fetch the repo at the new commit into a scratch cache
#   HF_HOME=<scratch> hf download <repo> --revision <sha>
# read its diff (for a trust_remote_code repo, read the *.py* diff -- that is
# the code you are agreeing to run), re-run the engine over a bench volume and
# compare the sidecar, then change the sha here and note it in the CHANGELOG.
# Never pin to a tag or a branch: both move.
REPO_REVISIONS: dict[str, str] = {
    "JustANormalTinkerer/hayai-ocr-v2.5-nova": "e46d79138499600564f810d44ab6bdea7230dee1",
    "google/siglip2-base-patch16-naflex": "b53b807d3a2d5e2b3911292f2d69e5341cdc064c",
    "sorryhyun/paddleocr-vl-1.6-manga-lora": "26292839d1469c14212a12a1e01b5b1fe01bff15",
    "PaddlePaddle/PaddleOCR-VL-1.6": "c5630abae1d940eafe0697512a0325494b02ab42",
}
# Not here, and deliberately: the DETECTORS. Each adapter is a separate script
# run in its own process (that is the licence boundary), so it cannot import
# this table and carries its own pin instead -- ``REVISION`` in
# ``detectors/animetext.py``, ``WEIGHTS_SHA256``
# in ``detectors/ctd.py`` (not a Hub repo), ``REPO_REVISION`` in ``ppocr.py``
# for ``detectors/ppocr_manga.py``. Each reports what it loaded back through
# the adapter contract (``detectors/_common.WEIGHTS_FILE``), which
# ``run_detector`` merges into ``ocr_engine.weights``, so a sidecar names what
# boxed its text as well as what read it.


# ---------------------------------------------------------------------------
# THE LOGGING SEAM. Every human-readable line this file produces goes through
# :func:`log`, and nothing in this file writes to stdout any other way.
#
# WHY IT IS A SEAM AND NOT A ``print``. The runner has two faces now. The
# single-volume CLI still writes its lines to stdout, verbatim, because the
# server's per-volume log is that stdout and its parser reads those lines
# (down to "Processed successfully: 1/1"). ``--serve`` and ``--bench`` put a
# JSON PROTOCOL on stdout instead, so a stray line there is not noise -- it is
# a parse error in the server. In those modes the lines go to the session log
# file, and every line ATTRIBUTABLE TO A VOLUME goes to that volume's own log
# as well, so the server keeps getting the per-volume log it has always had.
#
# Attribution is per THREAD, not per call: a stage worker binds the volume of
# the page it is holding (:func:`_attributed`), so a warning raised deep inside
# the page pipeline lands in the right volume's log without every function
# in the file having to carry a volume around. The driver binds it around the
# lines it prints for a page. A line from neither -- a detector adapter's own
# logging between pages, a model load -- is a session line and goes only to
# the session log.
# ---------------------------------------------------------------------------


class RunnerLog:
    """Where a human-readable line goes.

    One stream for the session (stdout, or the session log file), plus an
    optional per-thread volume handle. ``write`` never raises: a log that
    cannot be written must not be why a volume fails.
    """

    def __init__(self) -> None:
        self._stream: Any = None  # None means "sys.stdout, whatever it is now"
        self._owned: Any = None
        self._lock = threading.RLock()
        self._local = threading.local()

    # -- where the session's lines go --------------------------------------

    def to_file(self, path: Path) -> None:
        """Send the session's lines to ``path`` (append) instead of stdout."""
        path.parent.mkdir(parents=True, exist_ok=True)
        handle = path.open("a", encoding="utf-8", errors="replace")
        with self._lock:
            self._release()
            self._stream = handle
            self._owned = handle

    def to_stdout(self) -> None:
        """Back to stdout: what the single-volume CLI does, and the default."""
        with self._lock:
            self._release()
            self._stream = None

    def _release(self) -> None:
        if self._owned is not None:
            with contextlib.suppress(OSError, ValueError):
                self._owned.close()
            self._owned = None

    # -- attributing a line to a volume ------------------------------------

    def bind(self, handle: Any) -> Any:
        """Attribute this thread's lines to ``handle``; returns what it replaced."""
        previous = getattr(self._local, "handle", None)
        self._local.handle = handle
        return previous

    @property
    def volume(self) -> Any:
        """The volume handle this thread is bound to, if any."""
        return getattr(self._local, "handle", None)

    # -- one line ----------------------------------------------------------

    def write(self, message: str) -> None:
        text = f"{message}\n"
        handle = self.volume
        with self._lock:
            stream = self._stream if self._stream is not None else sys.stdout
            self._put(stream, text)
            if handle is not None and handle is not stream:
                self._put(handle, text)

    @staticmethod
    def _put(stream: Any, text: str) -> None:
        try:
            stream.write(text)
            stream.flush()
        except (OSError, ValueError):  # a closed log is not a failed volume
            pass


LOG = RunnerLog()


def log(message: str) -> None:
    """Print one human-readable line, wherever this run's lines go."""
    LOG.write(message)


def _attributed(run: Callable[[Any, Any], Any]) -> Callable[[Any, Any], Any]:
    """A stage callable that logs into the log of the volume its page came from.

    The binding is the worker THREAD's for the duration of the call, which is
    what lets a warning from deep inside a stage reach the right volume's log
    without the code that raised it knowing volumes exist.
    """

    def staged(job: Any, payload: Any) -> Any:
        previous = LOG.bind(getattr(getattr(job, "volume", None), "log", None))
        try:
            return run(job, payload)
        finally:
            LOG.bind(previous)

    return staged


def pinned(repo: str) -> str:
    """The commit a repo is pinned to, or raise: a new repo must be pinned."""
    try:
        return REPO_REVISIONS[repo]
    except KeyError:
        raise RuntimeError(
            f"Hugging Face repo {repo!r} has no pinned revision in REPO_REVISIONS; "
            "add its commit sha rather than resolving a moving branch"
        ) from None


# ``max_num_patches`` the NaFlex recognizers may read a crop at. Mirrors
# ``engines.PATCH_BUDGETS`` / ``DEFAULT_PATCH_BUDGET``; this file is copied
# into the OCR workspace and run from an environment that does not have
# mokuro_bunko installed, so it cannot import them (as with CROP_MODES). The
# measured cost of the setting is documented beside those constants.
PATCH_BUDGETS: tuple[int, ...] = (256, 384, 512)
DEFAULT_PATCH_BUDGET = 512
# Engines ``--patches`` actually reaches (``engines.EngineSpec.patch_budget``).
PATCH_BUDGET_ENGINES: frozenset[str] = frozenset({"hayai-nova"})

# ---------------------------------------------------------------------------
# PRECISION: what a recognizer computes in (``--precision``).
#
# A generation row carries ONE precision MODE, for every machine that runs it:
#
#   auto-accuracy  (the default) the first format in the engine's list that
#                  this device supports. Fixed: no benchmark decides it.
#   auto-balanced  the engine's candidate list for "a little accuracy for a
#   auto-speed     lot of speed" / "the fastest format the card runs well":
#                  the machine's automatic benchmark tries every SUPPORTED
#                  candidate on the same sample and keeps the fastest; within
#                  PRECISION_TIE of each other the earlier one (the more
#                  accurate) wins. Until a machine has benchmarked the row it
#                  runs the first supported candidate. The library sends the
#                  pick (``--precision-pick``); the runner never measures one
#                  outside ``--bench``.
#   fp32/bf16/fp16 forced: that format only. A device that does not support
#                  it cannot run the row -- the runner refuses at start
#                  (:class:`PrecisionUnavailable`) and the library never
#                  offers such a machine the row.
#
# "Supported" is a RUNTIME probe of the device (:func:`supported_formats`),
# never a list of architectures: fp32 always; fp16 on any GPU (CUDA/ROCm);
# bf16 where torch says the device can run it (an RX 6000 says yes and
# emulates it -- slower than fp32, which is exactly what the benchmark of a
# balanced/speed row finds out, and a forced bf16 there is the user's call).
# The CPU is therefore always fp32.
#
# The candidate lists (:data:`PRECISION_POLICY`) are the owner's, from a
# review of every line that read differently from fp32 on the same card
# (1,330 black-and-white pages, ~10,800 lines per engine; lines per 10,000):
#
#                 RTX 4090 (sm_89)  RX 9070 XT (gfx1201)  RX 6900 XT (gfx1030)
#   hayai-nova    fp16   5.5        8.3                   ~7
#                 bf16  47          45                    ~53
#   paddle-manga  fp16   9.2       10.1                   ~6.5
#                 bf16  61          56
#
# fp32 itself read identically on every card tested. Judged per differing
# line: hayai-nova's bf16 was MORE accurate than fp32 (its own training
# format) and fp16 the confirmable loser; paddle-manga is most accurate in
# fp32, close in bf16 and much faster there. mokuro's manga-ocr in bf16 was
# much worse than fp16 (2.89% vs 0.48% CER over 60k lines), so mokuro never
# runs bf16 (the fork has no switch for it either: ``--fp16`` only); it is
# fast enough in fp32 that only Speed trades that away.
# ---------------------------------------------------------------------------
PRECISION_AUTO = "auto"  # the legacy spelling of the default mode
PRECISION_BF16 = "bf16"
PRECISION_FP16 = "fp16"
PRECISION_FP32 = "fp32"
# The formats a recognizer can compute in.
PRECISIONS: tuple[str, ...] = (PRECISION_BF16, PRECISION_FP16, PRECISION_FP32)

MODE_ACCURACY = "auto-accuracy"
MODE_BALANCED = "auto-balanced"
MODE_SPEED = "auto-speed"
PRECISION_MODES: tuple[str, ...] = (
    MODE_ACCURACY, MODE_BALANCED, MODE_SPEED, PRECISION_FP32, PRECISION_BF16, PRECISION_FP16,
)
DEFAULT_PRECISION_MODE = MODE_ACCURACY
AUTO_PRECISION_MODES: frozenset[str] = frozenset({MODE_ACCURACY, MODE_BALANCED, MODE_SPEED})
FORCED_PRECISION_MODES: frozenset[str] = frozenset({PRECISION_FP32, PRECISION_BF16, PRECISION_FP16})
# The modes a benchmark decides, per machine.
BENCHED_PRECISION_MODES: frozenset[str] = frozenset({MODE_BALANCED, MODE_SPEED})
# Two candidates this close in pages a second are a tie, and a tie goes to
# the one earlier in the list: the more accurate.
PRECISION_TIE = 0.05

# THE ONE SOURCE OF TRUTH: engine -> mode -> candidate formats, in order of
# preference (see the study above for every entry that is not plain fp32).
PRECISION_POLICY: dict[str, dict[str, tuple[str, ...]]] = {
    "hayai-nova": {
        # bf16 tested MORE accurate than fp32 on hayai-nova.
        MODE_ACCURACY: (PRECISION_BF16, PRECISION_FP32),
        MODE_BALANCED: (PRECISION_BF16, PRECISION_FP32),
        MODE_SPEED: (PRECISION_BF16, PRECISION_FP16, PRECISION_FP32),
    },
    "paddle-manga": {
        # fp32 is the clear winner on accuracy; bf16 is close and much faster.
        MODE_ACCURACY: (PRECISION_FP32,),
        MODE_BALANCED: (PRECISION_BF16, PRECISION_FP32),
        MODE_SPEED: (PRECISION_BF16, PRECISION_FP16, PRECISION_FP32),
    },
    "mokuro": {
        # Fast even in fp32. Speed skips bf16: manga-ocr in bf16 measured
        # 2.89% CER against fp16's 0.48% (60k lines).
        MODE_ACCURACY: (PRECISION_FP32,),
        MODE_BALANCED: (PRECISION_FP32,),
        MODE_SPEED: (PRECISION_FP16, PRECISION_FP32),
    },
}
# The engines a row's mode reaches. Every other engine fixes its own
# precision (ppocr-manga: onnxruntime on the CPU) and ignores the mode.
PRECISION_ENGINES: frozenset[str] = frozenset(PRECISION_POLICY)
# The formats an engine can run at all, where that is not all three: the
# mokuro fork switches fp16 on and nothing else.
ENGINE_FORMATS: dict[str, tuple[str, ...]] = {"mokuro": (PRECISION_FP16, PRECISION_FP32)}
# Where a torch recognizer's precision is set in THIS process (a served
# engine's is its own process's, set by a flag at its start).
TORCH_PRECISION_ENGINES: frozenset[str] = frozenset({"hayai-nova", "paddle-manga"})

# The marker every precision refusal carries, so whoever reads a runner's
# fatal error can tell "this device cannot run the row's format" (give the
# volume back, it is nobody's failure) from a real failure.
PRECISION_REFUSAL = "precision not available here"


class PrecisionUnavailable(RuntimeError):
    """A forced format this device does not support: the runner will not start."""


def normalize_precision_mode(value: object) -> str:
    """A mode as stored: the default for nothing, ``auto`` read as the default."""
    if value is None:
        return DEFAULT_PRECISION_MODE
    text = str(value).strip().lower()
    if not text or text == PRECISION_AUTO:
        return DEFAULT_PRECISION_MODE
    if text not in PRECISION_MODES:
        raise ValueError(
            f"{value!r} is not a precision mode (one of {', '.join(PRECISION_MODES)})"
        )
    return text


def engine_formats(engine: str) -> tuple[str, ...]:
    """The formats ``engine`` can compute in at all."""
    return ENGINE_FORMATS.get(engine, PRECISIONS)


def engine_modes(engine: str) -> tuple[str, ...]:
    """The modes a row on ``engine`` may be set to (none: the engine fixes it)."""
    if engine not in PRECISION_ENGINES:
        return ()
    formats = engine_formats(engine)
    return tuple(m for m in PRECISION_MODES if m not in FORCED_PRECISION_MODES or m in formats)


def mode_candidates(engine: str, mode: str) -> tuple[str, ...]:
    """The formats ``mode`` may run ``engine`` at, in order of preference."""
    if engine not in PRECISION_ENGINES:
        return ()
    if mode in FORCED_PRECISION_MODES:
        return (mode,) if mode in engine_formats(engine) else ()
    return PRECISION_POLICY[engine][mode]


class ModeResolution(NamedTuple):
    """What one mode comes to on one device.

    ``precision`` None with ``eligible`` True: the engine fixes its own, or
    the device's formats were never reported (its runner decides at start).
    ``usable`` is the candidates this device supports, in order.
    """

    precision: str | None
    eligible: bool
    why: str
    usable: tuple[str, ...] = ()


def resolve_mode(
    engine: str,
    mode: str,
    supported: Iterable[str] | None,
    *,
    pick: str | None = None,
    pick_why: str = "",
) -> ModeResolution:
    """What ``mode`` runs ``engine`` at on a device supporting ``supported``.

    ``supported`` None: nobody reported what the device supports (a processor
    older than the probe). It then counts as fp32-only for a forced mode, and
    an auto mode is left to that machine's own runner at start.

    ``pick`` is a benchmark's choice for a balanced/speed row on this device,
    honoured only while it is still one of the usable candidates.
    """
    mode = normalize_precision_mode(mode)
    if engine not in PRECISION_ENGINES:
        return ModeResolution(None, True, "fixed by the engine")
    if mode in FORCED_PRECISION_MODES:
        if mode not in engine_formats(engine):
            return ModeResolution(None, False, f"{engine} does not run {mode}")
        known = frozenset(supported) if supported is not None else frozenset({PRECISION_FP32})
        if mode in known:
            return ModeResolution(mode, True, mode, (mode,))
        if supported is None:
            return ModeResolution(None, False, "card not reported")
        return ModeResolution(None, False, f"{mode} not supported")
    candidates = mode_candidates(engine, mode)
    if supported is None:
        return ModeResolution(None, True, "decided at start (card not reported)")
    known = frozenset(supported) | {PRECISION_FP32}
    usable = tuple(c for c in candidates if c in known)
    if mode not in BENCHED_PRECISION_MODES or len(usable) == 1:
        return ModeResolution(usable[0], True, mode, usable)
    if pick in usable:
        return ModeResolution(pick, True, pick_why or "benchmark", usable)
    return ModeResolution(usable[0], True, "not benchmarked yet: first supported candidate", usable)


def pick_precision(
    trials: Sequence[tuple[str, float]], usable: Sequence[str]
) -> tuple[str, str] | None:
    """A benchmark's pick among ``usable`` from its ``(format, pages/s)`` trials.

    The fastest wins; within :data:`PRECISION_TIE` of it, the candidate
    earliest in ``usable`` (the more accurate) is kept instead. Returns
    ``(format, why)``, or None when no usable candidate was measured.
    """
    rates = {fmt: float(rate) for fmt, rate in trials if fmt in usable and rate > 0}
    if not rates:
        return None
    fastest = max(rates.values())
    chosen = next(
        fmt for fmt in usable if fmt in rates and rates[fmt] >= fastest * (1 - PRECISION_TIE)
    )
    others = [f for f in usable if f in rates and f != chosen]
    detail = ", ".join(f"{f} {rates[f]:.2f} p/s" for f in others)
    verb = "beat" if all(rates[chosen] > rates[f] for f in others) else "tied with"
    why = f"benchmark: {chosen} {rates[chosen]:.2f} p/s {verb} {detail}" if others else (
        f"benchmark: {chosen} {rates[chosen]:.2f} p/s"
    )
    return chosen, why


def supported_formats(torch: Any, device: str) -> frozenset[str]:
    """What THIS device can compute in, asked of torch at run time.

    fp32 always; on a GPU fp16 too, and bf16 when torch says the device runs
    it (``torch.cuda.is_bf16_supported``, asked with that device current).
    A probe that fails is the CPU's answer: fp32 alone is never wrong.
    """
    if device == DEVICE_CPU or not str(device).startswith("cuda"):
        return frozenset({PRECISION_FP32})
    formats = {PRECISION_FP32, PRECISION_FP16}
    try:
        index = int(str(device).split(":", 1)[1]) if ":" in str(device) else 0
        with torch.cuda.device(index):
            if torch.cuda.is_bf16_supported():
                formats.add(PRECISION_BF16)
    except Exception:  # noqa: BLE001 - see the docstring
        pass
    return frozenset(formats)


def resolve_precision(
    engine: str,
    requested: str,
    *,
    supported: Iterable[str],
    pick: str | None = None,
    pick_why: str = "",
) -> tuple[str, str]:
    """``(precision, why)`` for a recognizer on a device, or a refusal.

    Raises :class:`PrecisionUnavailable` (its message carries
    :data:`PRECISION_REFUSAL`) for a forced format the device does not
    support; ``ValueError`` for a mode that is not one.
    """
    mode = normalize_precision_mode(requested)
    resolved = resolve_mode(engine, mode, supported, pick=pick, pick_why=pick_why)
    if not resolved.eligible or resolved.precision is None:
        raise PrecisionUnavailable(
            f"{PRECISION_REFUSAL}: {engine} is asked for {mode}, and this device "
            f"cannot run it ({resolved.why})"
        )
    why = mode if resolved.why == mode else f"{mode}; {resolved.why}"
    return resolved.precision, why


def served_formats(on_card: bool) -> frozenset[str]:
    """What a served engine's process can compute in: its ``--fp16`` on a card."""
    return frozenset({PRECISION_FP32, PRECISION_FP16}) if on_card else frozenset({PRECISION_FP32})


def fp32_master(model: Any) -> dict[str, Any]:
    """A copy of a model's floating-point PARAMETERS, on the CPU.

    Taken from a model loaded in fp32, and what every later cast starts from
    (:func:`cast_from_master`): trying bf16 and then fp16 never casts from a
    cast, and nothing is reloaded from disk.
    """
    return {
        name: tensor.detach().to("cpu", copy=True)
        for name, tensor in model.named_parameters()
        if tensor.is_floating_point()
    }


def cast_from_master(model: Any, master: Mapping[str, Any], dtype: Any) -> None:
    """Every floating parameter := its master copy cast to ``dtype``, where it lives."""
    for name, tensor in model.named_parameters():
        source = master.get(name)
        if source is not None:
            tensor.data = source.to(device=tensor.device, dtype=dtype)


def torch_dtype(torch: Any, precision: str) -> Any:
    """The torch dtype a resolved precision names."""
    return {
        PRECISION_BF16: torch.bfloat16,
        PRECISION_FP16: torch.float16,
        PRECISION_FP32: torch.float32,
    }[precision]


def recognizer_precision(
    torch: Any,
    engine: str,
    requested: str,
    device: str,
    *,
    pick: str | None = None,
    pick_why: str = "",
) -> str:
    """The precision a recognizer loading on ``device`` computes in.

    Logs the one line that says which precision and why, e.g.
    ``hayai-nova precision: bf16 (auto-accuracy)``.
    """
    precision, why = resolve_precision(
        engine, requested, supported=supported_formats(torch, device), pick=pick,
        pick_why=pick_why,
    )
    log(f"[runner] {engine} precision: {precision} ({why})")
    return precision


# Engines that read a page's LINES themselves -- detector and recognizer in
# one -- mapped to the detector id recorded in the sidecar. They run in this
# process instead of going through a detector adapter, because nothing calls
# for the boundary and it would cost real work:
#
# * the adapter process exists to contain a licence (detectors/README.md);
#   both PP-OCRv6 manga models and everything they import are Apache-2.0 or
#   more permissive, so there is nothing to contain;
# * the adapter contract carries geometry only, so a second process would
#   decode every page a second time (40-60 ms of a 0.15-0.4 s page) and load
#   onnxruntime twice;
# * line_layout needs what the contract drops: the recognizer's confidence
#   decides whether a one-glyph ruby run or a stray mark is believed.
#
# ``--detector`` is ignored for these engines.
LINE_ENGINES: dict[str, str] = {"ppocr-manga": "ppocr-manga"}

# Engines that are a SERVE PROCESS of their own: this runner spawns
# ``<their python> -m <module> <args>`` once a session, streams the pages of
# volume after volume into it and reads page JSON back (see ``ROAD_SERVED``).
# Nothing of the engine runs in THIS process, so the interpreter is not this
# one either -- it is the one ``--mokuro-python`` names, which is the mokuro
# virtual environment's.
#
# Mapped to ``(module, extra args)``. The args are the engine's own knobs;
# ``--num_workers`` is added from the ``mokuro`` stage's Workers cell, because
# on this road that cell IS the fork's pipeline pool rather than a pool of
# ours (one process holds one model).
SERVED_ENGINES: dict[str, tuple[str, tuple[str, ...]]] = {
    "mokuro": ("mokuro.serve", ()),
}
# The serve process's switch to half precision (the fork's ``--fp16``; it
# has none for bf16). Added when the row's mode resolves to fp16 there.
SERVED_FP16_FLAG = "--fp16"

ENGINE_IDS: tuple[str, ...] = (*CROP_MODES, *LINE_ENGINES, *SERVED_ENGINES)

# The adapter contract's weights file (``detectors/_common.WEIGHTS_FILE``):
# one JSON object per detector run, beside the per-page JSON, mapping every
# model source the adapter loaded to the revision or digest it was pinned to.
# Duplicated rather than imported, like CROP_MODES: this file runs from an
# environment without mokuro_bunko installed.
DETECTOR_WEIGHTS_FILE = "_weights.json"

# Detector adapter scripts (shipped next to this file under detectors/).
DETECTOR_SCRIPTS: dict[str, str] = {
    "ctd": "ctd.py",
    "animetext": "animetext.py",
    "ppocr-manga": "ppocr_manga.py",
}

# Detectors whose lines the runner reads IN THIS PROCESS, the way the line
# engine of the same name does (``ReconciledPageReader``): detection, the CTC
# read, joined column pieces and recovered brackets come from ``ppocr.py``,
# the configured engine then reads each text line's crop, and ``line_layout``
# groups the merged lines. The adapter script of such a detector stays a valid
# standalone contract adapter, but the runner does not start it: it would
# hand over bare quads -- no CTC read to reconcile with, column pieces not
# joined (bench temp00097: a column boxed as two overlapping quads came out
# of the engine as "...装飾、なのに" + "飾、なのにど"), ruby still to be read.
LAYOUT_DETECTORS: frozenset[str] = frozenset({"ppocr-manga"})

# Detectors whose "lines" are whole text blocks (one quad per bubble). Their
# quads must never be deskewed as if they were single columns: a multi-column
# bubble squeezed to one 64px column is unreadable. Both recognizers read a
# whole upright bubble well, so block-level detectors always get upright crops.
BLOCK_LEVEL_DETECTORS: frozenset[str] = frozenset({"animetext"})


def select_crop(engine: str, detector: str) -> tuple[str, float]:
    """Return (crop mode, margin) for an engine/detector pair.

    "line" is mokuro-style deskewing (only meaningful for line-level
    detectors); "upright" keeps page orientation. hayai-nova wants tight crops
    (margin 0) so neighbouring furigana stays out; the Paddle LoRA was trained
    with a 12% margin. On a ``LAYOUT_DETECTORS`` detector the Paddle crops are
    "quad": the line's own rotated rectangle, deskewed, with a margin in ems
    (``make_quad_crop_fn``; the margin returned here is then unused).
    """
    mode = CROP_MODES[engine]
    if detector in BLOCK_LEVEL_DETECTORS:
        mode = "upright"
    elif detector in LAYOUT_DETECTORS and mode == "upright":
        mode = "quad"
    margin = UPRIGHT_MARGIN if engine == "paddle-manga" and mode == "upright" else 0.0
    return mode, margin


PADDLE_BASE_REPO = "PaddlePaddle/PaddleOCR-VL-1.6"
UPRIGHT_MARGIN = 0.12
# Margin of a LINE crop, in ems (the line's thickness). The LoRA's crops were
# padded by 12% of the LONGER side, which on its training lines (a few glyphs
# of SFX) is a fraction of an em. On a 40-glyph novel column the same rule is
# five glyphs of margin: three neighbouring columns on either side, and along
# the column enough of the next piece to read it twice. So the rule is capped
# in ems, where it is what the model saw.
LINE_MARGIN_EM = 0.25
# Margin of the SECOND read of a line the two recognizers differ on (see
# ``line_reconcile.settle_disputes``). Bench, four novel pages: neither margin
# reads better -- 0.25 em lost 3 closing marks and misread 1 kanji that 0.5 em
# got, 0.5 em misread 2 kanji that 0.25 em got -- but they fail on DIFFERENT
# lines, which is what makes the second one a useful witness.
SECOND_MARGIN_EM = 0.5
# The page's body pitch -- one glyph cell of its running text -- is the median
# thickness of the line quads the CTC recognizer read something in. It is what
# tells a line quad from a quad that is no line: a hand-lettered panel comes
# back as one blob over ten columns, and both ``line_reconcile.line_cells``
# and its ``engine_only_verdict`` need to know that. Under this many read
# lines a page has no pitch worth measuring, and the rule stays off -- the
# behaviour of every page before there was a pitch to measure.
PITCH_MIN_CONF = 0.5
PITCH_MIN_LINES = 3
# A quad's COMPANY: lines the CTC recognizer read that run parallel to it
# nearby (:func:`parallel_neighbours`), which is how printed ruby and a small
# printed kana are told from the hatching the detector scores the same
# (``line_reconcile.engine_only_verdict``). Same axis, within this many
# degrees, centres within this many thicknesses -- a few columns of a body, or
# the line above and below.
NEIGHBOUR_ANGLE = 15.0
NEIGHBOUR_REACH = 3.0
# Smallest crop side the LoRA was trained on; thinner lines are scaled up.
MIN_CROP_SIDE = 16
# ``max_new_tokens`` when nothing is known about the crop (a whole bubble).
DEFAULT_MAX_NEW_TOKENS = 64
# Crops per generate() call. Line crops are small (a novel column is ~270
# image tokens), so memory is not the limit; one runaway holds its whole batch
# until the token cap, which is what keeps this moderate.
PADDLE_BATCH = 12
# Crops per Nova generate() call. Its generate() preallocates a KV cache of
# (batch, heads, vision tokens + max_new_tokens, head dim) and runs every row
# of the batch until the LAST one finishes, so a big batch is paid for by the
# short lines in it; a page's lines are few enough that this is plenty.
HAYAI_NOVA_BATCH = 16
# Nova's generate() default is 128; a deskewed line crop never holds that
# many glyphs, and the cap is what a runaway repetition costs.
HAYAI_NOVA_MAX_NEW_TOKENS = 96

# Same knobs mokuro's MangaPageOcr uses.
TEXT_HEIGHT = 64
MAX_RATIO_VERTICAL = 16
MAX_RATIO_HORIZONTAL = 8
ANCHOR_WINDOW = 2

# Type of the callable turning a list of RGB crops (PIL images) into strings.
Recognizer = Callable[[list[Any]], list[str]]
# Type of the callable turning (img_bgr, block dict, line_idx) into crops.
CropFn = Callable[[Any, dict[str, Any], int], list[Any]]


# --------------------------------------------------------------------------
# Pure helpers (no torch / no cv2 imports)
# --------------------------------------------------------------------------


def _natural_key(path: Path) -> list[object]:
    """Fallback natural sort key when the natsort package is unavailable."""
    parts = re.split(r"(\d+)", path.as_posix())
    return [int(p) if p.isdigit() else p.lower() for p in parts]


def _blank_from(source: Any) -> dict[str, Any] | None:
    try:
        from PIL import Image

        with Image.open(source) as img:
            width, height = img.size
    except Exception:
        return None
    return {
        "version": MOKURO_FORMAT_VERSION,
        "img_width": int(width),
        "img_height": int(height),
        "blocks": [],
    }


def blank_page(image_path: Path) -> dict[str, Any] | None:
    """An empty page record for an image the engine failed on, or None.

    Only the image's size is needed, read from its header. None when even that
    cannot be read (a corrupt file): then there is nothing honest to write.
    """
    return _blank_from(image_path)


def blank_page_bytes(data: bytes) -> dict[str, Any] | None:
    """:func:`blank_page` for a page that only ever existed in memory."""
    import io

    return _blank_from(io.BytesIO(data))


def list_pages(input_dir: Path) -> list[Path]:
    """Return page image paths relative to ``input_dir`` in reading order.

    Mirrors mokuro: recursive, same extensions, natural sort.
    """
    rel_paths = [
        p.relative_to(input_dir)
        for p in input_dir.glob("**/*")
        if p.is_file() and p.suffix.lower() in IMAGE_EXTENSIONS
    ]
    try:
        from natsort import natsorted

        return list(natsorted(rel_paths))
    except ImportError:
        return sorted(rel_paths, key=_natural_key)


def json_default(obj: Any) -> Any:
    """JSON encoder hook for numpy scalars and arrays (mirrors mokuro's)."""
    tolist = getattr(obj, "tolist", None)
    if callable(tolist):
        return tolist()
    item = getattr(obj, "item", None)
    if callable(item):
        return item()
    raise TypeError(f"Object of type {type(obj).__name__} is not JSON serializable")


def dump_json(obj: Any, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as f:
        json.dump(obj, f, ensure_ascii=False, default=json_default)


def normalize_text(text: str) -> str:
    """NFKC-normalize and strip, as the Paddle manga LoRA card prescribes."""
    return unicodedata.normalize("NFKC", text).strip()


def upright_crop_bounds(
    width: int, height: int, line_pts: Sequence[Sequence[float]], margin: float = UPRIGHT_MARGIN
) -> tuple[int, int, int, int]:
    """``(left, top, right, bottom)`` of the axis-aligned crop of a line quad.

    The quad's bounding box is expanded by ``margin`` per side and clipped to
    the page. Callers that need to map crop pixels back to page pixels use
    ``(left, top)`` as the crop's origin.
    """
    xs = [float(p[0]) for p in line_pts]
    ys = [float(p[1]) for p in line_pts]
    x1, x2 = min(xs), max(xs)
    y1, y2 = min(ys), max(ys)
    mx = (x2 - x1) * margin
    my = (y2 - y1) * margin
    left = max(0, int(x1 - mx))
    top = max(0, int(y1 - my))
    right = min(width, int(round(x2 + mx)) + 1)
    bottom = min(height, int(round(y2 + my)) + 1)
    if right <= left:
        right = min(width, left + 1)
    if bottom <= top:
        bottom = min(height, top + 1)
    return left, top, right, bottom


def upright_line_crop(
    img: Any, line_pts: Sequence[Sequence[float]], margin: float = UPRIGHT_MARGIN
) -> Any:
    """Axis-aligned crop of a line polygon expanded by ``margin`` per side.

    ``img`` is any HxWx3 array supporting numpy-style slicing.
    """
    left, top, right, bottom = upright_crop_bounds(
        img.shape[1], img.shape[0], line_pts, margin=margin
    )
    return img[top:bottom, left:right]


def quad_extents(quad: Sequence[Sequence[float]], vertical: bool) -> tuple[float, float]:
    """``(main, cross)`` extents of a line quad, the way the reader computes them.

    The main extent runs along the reading axis (top to bottom for vertical
    text, left to right for horizontal): the length of the vector between the
    midpoints of the two edges crossing it.
    """
    pts = [(float(p[0]), float(p[1])) for p in quad[:4]]
    if len(pts) < 4:
        return 0.0, 0.0
    mid = [((pts[i][0] + pts[(i + 1) % 4][0]) / 2, (pts[i][1] + pts[(i + 1) % 4][1]) / 2) for i in range(4)]
    vec_v = (mid[2][0] - mid[0][0], mid[2][1] - mid[0][1])
    vec_h = (mid[1][0] - mid[3][0], mid[1][1] - mid[3][1])
    len_v = (vec_v[0] ** 2 + vec_v[1] ** 2) ** 0.5
    len_h = (vec_h[0] ** 2 + vec_h[1] ** 2) ** 0.5
    return (len_v, len_h) if vertical else (len_h, len_v)


def body_pitch(lines: Sequence[Any]) -> float:
    """The page's glyph cell: median thickness of the lines the CTC read text in.

    Lines it read nothing in are left out -- those are the blobs and the line
    art this measurement exists to recognize -- and so are the ones it doubts
    (``PITCH_MIN_CONF``). Display lettering and the odd blob are outvoted by
    the median; a page with fewer than ``PITCH_MIN_LINES`` read lines has no
    pitch, and returns 0.0 for "do not use it".
    """
    thickness = [
        quad_extents(line.quad, bool(line.vertical))[1]
        for line in lines
        if str(getattr(line, "text", "") or "").strip()
        and float(getattr(line, "conf", 0.0) or 0.0) >= PITCH_MIN_CONF
    ]
    thickness = [t for t in thickness if t > 0]
    if len(thickness) < PITCH_MIN_LINES:
        return 0.0
    return float(statistics.median(thickness))


def parallel_neighbours(lines: Sequence[Any]) -> list[int]:
    """Per line: how many lines the CTC recognizer READ run parallel to it nearby.

    The company a quad keeps, which is what tells printed ruby and a small
    printed kana from the hatching and the motion marks the detector scores
    the same (``line_reconcile.engine_only_verdict``, its ``body`` rule). A
    neighbour counts when the CTC recognizer got text out of it -- so it is
    certainly lettering -- when it reads along the same axis at the same angle
    (``NEIGHBOUR_ANGLE``), and when its centre is within ``NEIGHBOUR_REACH``
    thicknesses of this quad's. Art is mostly alone on its patch of page.
    """
    geo: list[tuple[float, float, float]] = []
    for line in lines:
        thick = quad_extents(line.quad, bool(line.vertical))[1]
        pts = [(float(p[0]), float(p[1])) for p in line.quad[:4]]
        count = max(1, len(pts))
        geo.append((thick, sum(p[0] for p in pts) / count, sum(p[1] for p in pts) / count))
    read = [bool(str(getattr(line, "text", "") or "").strip()) for line in lines]
    out: list[int] = []
    for i, line in enumerate(lines):
        thick, cx, cy = geo[i]
        near = 0
        for j, other in enumerate(lines):
            if j == i or not read[j]:
                continue
            if bool(other.vertical) != bool(line.vertical):
                continue
            if abs(float(other.angle) - float(line.angle)) > NEIGHBOUR_ANGLE:
                continue
            other_thick, ox, oy = geo[j]
            if math.hypot(cx - ox, cy - oy) <= NEIGHBOUR_REACH * max(thick, other_thick, 1.0):
                near += 1
        out.append(near)
    return out


def load_detection(path: Path) -> dict[str, Any]:
    """Read and sanity-check one detector page JSON (see detectors/README.md)."""
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, dict) or "blocks" not in data:
        raise ValueError(f"malformed detection file: {path}")
    blocks = []
    for blk in data["blocks"]:
        lines = [[[float(x), float(y)] for x, y in quad] for quad in blk.get("lines", [])]
        if not lines:
            continue
        blocks.append(
            {
                "box": [int(v) for v in blk["box"]],
                "vertical": bool(blk.get("vertical", True)),
                "font_size": int(blk.get("font_size", 0))
                or max(8, int(lines[0][1][0] - lines[0][0][0])),
                "lines": lines,
            }
        )
    return {
        "img_width": int(data.get("img_width", 0)),
        "img_height": int(data.get("img_height", 0)),
        "blocks": blocks,
    }


def ocr_page(
    img: Any,
    blocks: Sequence[dict[str, Any]],
    crop_fn: CropFn,
    recognize: Recognizer,
    version: str,
) -> dict[str, Any]:
    """Build one mokuro page dict from detector blocks and a recognizer.

    ``blocks`` follow the detector JSON schema (``box``, ``vertical``,
    ``font_size``, ``lines`` as quads). ``crop_fn`` returns the crop(s) for
    one line; when a line yields several chunks their texts are concatenated
    in order, exactly like mokuro does. All crops of a page go to the
    recognizer as one batch.

    Split in two because the two halves belong to different stages: the read is
    the recognizer and holds the device, the build is numpy and JSON and does
    not. Calling both is exactly what the serial path always did.
    """
    return ocr_page_build(
        img,
        blocks,
        ocr_page_read(img, blocks, crop_fn, recognize),
        version=version,
    )


def ocr_page_read(
    img: Any,
    blocks: Sequence[dict[str, Any]],
    crop_fn: CropFn,
    recognize: Recognizer,
) -> dict[tuple[int, int], tuple[list[Any], list[str]]]:
    """The engine stage of a page: every crop of it through the recognizer, once.

    Returns the crops and their texts per ``(block, line)``; a line read as
    several chunks keeps them in reading order. All of a page's crops go as ONE
    batch, which is what makes the recognizer's padding -- and therefore what it
    reads -- independent of how the pipeline is scheduled.
    """
    crops: list[Any] = []
    # (block index, line index) of each crop, in crop order.
    owners: list[tuple[int, int]] = []
    for block_idx, blk in enumerate(blocks):
        for line_idx in range(len(blk["lines"])):
            for crop in crop_fn(img, blk, line_idx):
                crops.append(crop)
                owners.append((block_idx, line_idx))

    texts = recognize(crops) if crops else []
    if len(texts) != len(crops):
        raise RuntimeError(f"recognizer returned {len(texts)} strings for {len(crops)} crops")

    per_line: dict[tuple[int, int], tuple[list[Any], list[str]]] = {}
    for owner, crop, text in zip(owners, crops, texts, strict=True):
        line_crops, line_texts = per_line.setdefault(owner, ([], []))
        line_crops.append(crop)
        line_texts.append(text)
    return per_line


def ocr_page_build(
    img: Any,
    blocks: Sequence[dict[str, Any]],
    per_line: Mapping[tuple[int, int], tuple[list[Any], list[str]]],
    *,
    version: str,
) -> dict[str, Any]:
    """The post stage of a page: assemble the blocks.

    Pure CPU and no model at all, so a pool of these can run behind the
    recognizer.
    """
    height, width = img.shape[0], img.shape[1]
    page: dict[str, Any] = {
        "version": version,
        "img_width": int(width),
        "img_height": int(height),
        "blocks": [],
    }
    for block_idx, blk in enumerate(blocks):
        line_count = len(blk["lines"])
        texts_by_line = [per_line.get((block_idx, li), ([], []))[1] for li in range(line_count)]
        entry: dict[str, Any] = {
            "box": [int(v) for v in blk["box"]],
            "vertical": bool(blk["vertical"]),
            "font_size": int(blk["font_size"]),
            "lines_coords": [[[float(x), float(y)] for x, y in quad] for quad in blk["lines"]],
            "lines": ["".join(line_texts) for line_texts in texts_by_line],
        }
        page["blocks"].append(entry)
    return page


def build_volume(
    pages: Sequence[tuple[str, dict[str, Any]]],
    *,
    version: str,
    title: str,
    volume: str,
    title_uuid: str,
    volume_uuid: str,
    # Not dict[str, str]: it carries ``patch_budget`` (int) and ``weights``
    # (repo -> commit) alongside the plain string fields. None leaves the key
    # OUT, which is what an engine that writes pure upstream mokuro means: a
    # served mokuro's sidecar is the one its own CLI would have written, key
    # for key, and the server stamps a non-primary layer itself.
    engine_meta: dict[str, Any] | None,
) -> dict[str, Any]:
    """Assemble the volume-level mokuro dict from (img_path, page) pairs."""
    out: dict[str, Any] = {
        "version": version,
        "title": title,
        "title_uuid": title_uuid,
        "volume": volume,
        "volume_uuid": volume_uuid,
    }
    if engine_meta is not None:
        out["ocr_engine"] = dict(engine_meta)
    out["pages"] = []
    for img_path, page in pages:
        entry = dict(page)
        entry["img_path"] = img_path.replace("\\", "/")
        out["pages"].append(entry)
    return out


def layout_page_dict(
    raw_page: dict[str, Any], layout: Any, version: str
) -> tuple[dict[str, Any], Any]:
    """One mokuro page from raw OCR lines, via ``line_layout``.

    ``raw_page`` is ``{"width", "height", "lines": [{"quad", "text", "score",
    "conf"}]}`` (what ``ppocr.page_to_json`` writes); ``layout`` is the
    ``line_layout`` module. Returns the page and the full layout result, whose
    ``groups`` say which input line each output line came from.

    Blocks of kind ``noise`` -- a lone glyph the recognizer itself doubted,
    attached to no column -- are left out of the page: on an illustration they
    are screentone and pen strokes read as "ノ" or "1", and a reader would
    offer them as selectable text. They stay in the returned layout (and in
    the raw line dump) for anyone debugging a page.
    """
    result = layout.layout_page(raw_page)
    page: dict[str, Any] = {
        "version": version,
        "img_width": int(raw_page.get("width") or 0),
        "img_height": int(raw_page.get("height") or 0),
        "blocks": [
            block
            for block, kind in zip(result.blocks, result.kinds, strict=True)
            if kind != "noise"
        ],
    }
    return page, result


def chunk_cut_points(
    density: Sequence[float], width: int, num_chunks: int, anchor_window: int
) -> list[int]:
    """Cut columns for splitting a long line, at ink minima near equal anchors.

    ``density`` is per-column ink (any scale). Mirrors mokuro's approach:
    anchors at equal spacing, each moved to the lowest-density column within
    ``anchor_window`` pixels.
    """
    if num_chunks <= 1 or width <= 0:
        return []
    cuts = []
    for k in range(1, num_chunks):
        anchor = int(round(width * k / num_chunks))
        lo = max(0, anchor - anchor_window // 2)
        hi = min(width, anchor + anchor_window // 2)
        if hi <= lo:
            cuts.append(anchor)
            continue
        best = min(range(lo, hi), key=lambda i: density[i])
        cuts.append(best)
    return cuts


# --------------------------------------------------------------------------
# Engine-backed pieces (import torch / cv2 lazily)
# --------------------------------------------------------------------------


def _pick_device() -> str:
    import torch

    if torch.cuda.is_available():
        return "cuda"
    return "cpu"


def imread_bgr(path: Path) -> Any:
    """Decode an image to a BGR array (first frame for animated formats)."""
    return _imdecode(path)


def imdecode_bgr(data: bytes) -> Any:
    """:func:`imread_bgr` for bytes that never reached the disk.

    Same decoder, same conversion, so the array is the one the extracted file
    would have produced -- the whole point of reading pages out of an archive.
    """
    import io

    return _imdecode(io.BytesIO(data))


def _imdecode(source: Any) -> Any:
    import cv2
    import numpy as np
    from PIL import Image

    with Image.open(source) as im:
        return cv2.cvtColor(np.array(im.convert("RGB")), cv2.COLOR_RGB2BGR)


def warp_line(
    img: Any, quad: Sequence[Sequence[float]], vertical: bool, text_height: int = TEXT_HEIGHT
) -> Any:
    """Deskew a line quad to a fixed glyph size, keeping its orientation.

    Vertical lines become a column ``text_height`` wide, horizontal lines a
    strip ``text_height`` tall. This is the crop mokuro hands manga-ocr
    (deskewed, upright, no margin, so neighbouring furigana is excluded).
    """
    import cv2
    import numpy as np

    src = np.array(quad, dtype=np.float32)
    mid = (src[[1, 2, 3, 0]] + src) / 2
    vec_v = mid[2] - mid[0]
    vec_h = mid[1] - mid[3]
    ratio = float(np.linalg.norm(vec_v) / max(1e-6, np.linalg.norm(vec_h)))
    if vertical:
        w = int(text_height)
        h = max(1, int(round(text_height * ratio)))
    else:
        h = int(text_height)
        w = max(1, int(round(text_height / max(1e-6, ratio))))
    dst = np.array([[0, 0], [w - 1, 0], [w - 1, h - 1], [0, h - 1]], dtype=np.float32)
    matrix = cv2.getPerspectiveTransform(src, dst)
    return cv2.warpPerspective(img, matrix, (w, h))


def split_long_line(strip: Any, max_ratio: int, text_height: int = TEXT_HEIGHT) -> list[Any]:
    """Split a horizontal strip whose width/height exceeds ``max_ratio``.

    Cut points sit at ink minima (Gaussian-smoothed column darkness) near
    equally spaced anchors, so chunks break between glyphs.
    """
    import cv2
    import numpy as np

    h, w = strip.shape[:2]
    ratio = w / max(1, h)
    if ratio <= max_ratio:
        return [strip]
    num_chunks = int(np.ceil(ratio / max_ratio))
    gray = cv2.cvtColor(strip, cv2.COLOR_BGR2GRAY)
    ink = (255 - gray).astype(np.float32)
    density = ink.sum(axis=0)
    kernel = cv2.getGaussianKernel(text_height * 2, text_height / 8).ravel()
    density = np.convolve(density, kernel, "same")
    cuts = chunk_cut_points(density.tolist(), w, num_chunks, ANCHOR_WINDOW * text_height)
    return [c for c in np.split(strip, cuts, axis=1) if c.shape[1] > 0]


def make_line_crop_fn() -> CropFn:
    """mokuro-style crops: deskewed strips, vertical rotated, long lines chunked."""
    import cv2
    from PIL import Image

    def crop_fn(img: Any, blk: dict[str, Any], line_idx: int) -> list[Any]:
        vertical = bool(blk["vertical"])
        region = warp_line(img, blk["lines"][line_idx], vertical)
        # Chunking works on a horizontal strip; columns are rotated for the
        # split and rotated back so the recognizer sees upright text.
        strip = cv2.rotate(region, cv2.ROTATE_90_COUNTERCLOCKWISE) if vertical else region
        max_ratio = MAX_RATIO_VERTICAL if vertical else MAX_RATIO_HORIZONTAL
        chunks = split_long_line(strip, max_ratio)
        if vertical:
            chunks = [cv2.rotate(c, cv2.ROTATE_90_CLOCKWISE) for c in chunks]
        return [Image.fromarray(cv2.cvtColor(c, cv2.COLOR_BGR2RGB)) for c in chunks]

    return crop_fn


def make_upright_crop_fn(margin: float = UPRIGHT_MARGIN) -> CropFn:
    import cv2
    from PIL import Image

    def crop_fn(img: Any, blk: dict[str, Any], line_idx: int) -> list[Any]:
        crop = upright_line_crop(img, blk["lines"][line_idx], margin=margin)
        return [Image.fromarray(cv2.cvtColor(crop, cv2.COLOR_BGR2RGB))]

    return crop_fn


def line_margin_px(quad: Sequence[Sequence[float]], margin_em: float = LINE_MARGIN_EM) -> float:
    """Margin of a line crop in pixels (see ``LINE_MARGIN_EM``)."""
    main, cross = quad_extents(quad, True)
    return min(UPRIGHT_MARGIN * max(main, cross), margin_em * min(main, cross))


def padded_quad(quad: Sequence[Sequence[float]], pad: float) -> list[list[float]]:
    """``quad`` grown by ``pad`` pixels on every side, along its OWN axes.

    Corners are top-left, top-right, bottom-right, bottom-left in the line's
    upright frame (the detector's order), so a tilted line gets a tilted
    margin and its deskewed crop holds the same ink an upright line's would.
    """
    pts = [(float(p[0]), float(p[1])) for p in quad[:4]]

    def unit(a: tuple[float, float], b: tuple[float, float]) -> tuple[float, float]:
        dx, dy = b[0] - a[0], b[1] - a[1]
        norm = (dx * dx + dy * dy) ** 0.5 or 1.0
        return dx / norm, dy / norm

    ux, uy = unit(pts[0], pts[1])
    vx, vy = unit(pts[0], pts[3])
    signs = ((-1, -1), (1, -1), (1, 1), (-1, 1))
    return [
        [x + pad * (su * ux + sv * vx), y + pad * (su * uy + sv * vy)]
        for (x, y), (su, sv) in zip(pts, signs, strict=True)
    ]


def make_quad_crop_fn(margin_em: float = LINE_MARGIN_EM) -> CropFn:
    """Deskewed line crops with an em margin, orientation kept (a column stays a column).

    What the manga LoRA was trained on (``minAreaRect`` deskew, padded,
    orientation preserved, min side 16 px), with the padding rule that suits
    a line: see ``LINE_MARGIN_EM``. An axis-aligned crop of a tilted line
    would take in its neighbours instead.
    """
    import cv2
    import numpy as np
    from PIL import Image

    def crop_fn(img: Any, blk: dict[str, Any], line_idx: int) -> list[Any]:
        quad = blk["lines"][line_idx]
        src = np.array(padded_quad(quad, line_margin_px(quad, margin_em)), dtype=np.float32)
        width = float(np.linalg.norm(src[1] - src[0]))
        height = float(np.linalg.norm(src[3] - src[0]))
        scale = max(1.0, MIN_CROP_SIDE / max(1.0, min(width, height)))
        w, h = max(2, int(round(width * scale))), max(2, int(round(height * scale)))
        dst = np.array([[0, 0], [w, 0], [w, h], [0, h]], dtype=np.float32)
        matrix = cv2.getPerspectiveTransform(src, dst)
        crop = cv2.warpPerspective(
            img, matrix, (w, h), flags=cv2.INTER_CUBIC, borderMode=cv2.BORDER_REPLICATE
        )
        return [Image.fromarray(cv2.cvtColor(crop, cv2.COLOR_BGR2RGB))]

    return crop_fn


def plan_generation_batches(
    areas: Sequence[float], caps: Sequence[int], batch_size: int
) -> list[list[int]]:
    """Crop indices grouped for batched generation.

    A batch runs until its LONGEST member is done, and is padded to its
    largest image, so like goes with like: by token cap first (it is the
    line's length), by area within a cap.
    """
    order = sorted(range(len(areas)), key=lambda i: (caps[i], areas[i], i))
    size = max(1, int(batch_size))
    return [order[k : k + size] for k in range(0, len(order), size)]


# --------------------------------------------------------------------------
# loading the models
# --------------------------------------------------------------------------


# Two threads may not import the torch stack at once.
#
# ``transformers`` and ``peft`` import each other's namespaces at module
# level. Python's import lock is PER MODULE, so a second thread doing
# ``from transformers import AutoModel`` while the first is still inside
# ``import transformers`` gets the half-built module and an ImportError --
# not a hang, a plain failure. Loading the recognizer on a background thread
# (:class:`DeferredRecognizer`) puts two threads in that position, and the
# failure was seen on the first real run: "cannot import name 'AutoModel'
# from 'transformers'".
#
# So the IMPORTS are serialized and the LOADS are not, which is the right way
# round: importing the stack is a couple of seconds, loading a VLM is ten.
_MODEL_IMPORT_LOCK = threading.Lock()


def import_model_stack(*modules: str) -> None:
    """Import the torch/transformers modules a loader is about to use, once.

    Called first thing by everything that loads a model in this file. After
    it returns, the ``from x import y`` lines that follow are dictionary
    lookups on a fully-initialised module, whichever thread is running them.
    """
    with _MODEL_IMPORT_LOCK:
        for name in modules:
            importlib.import_module(name)


def load_sibling(name: str) -> Any:
    """Import a module staged next to this file, by path.

    The runner runs outside the package (the engines environment does not
    have ``mokuro_bunko`` installed); the processor copies ``ppocr.py``,
    ``line_layout.py`` and ``line_reconcile.py`` into the workspace beside it.
    """
    import importlib  # noqa: PLC0415

    here = str(Path(__file__).resolve().parent)
    if here not in sys.path:
        sys.path.insert(0, here)
    return importlib.import_module(name)


# torch's CPU thread pool while a recognizer runs on a CARD. The default is one
# thread per core, and with the model on the GPU the CPU side of a call (the
# image processor, the decoding loop's bookkeeping) is small work split into
# parallel regions whose barriers wait on the slowest thread -- harmless on
# an idle host, ruinous beside a neighbour that has those cores. Measured on
# tower (RTX 4090, 48 CPU threads; hayai-nova + ppocr-manga, 669 pages, no
# OMP_NUM_THREADS; one thread against 77d2304, the commit before the cap):
# beside 24 busy-spinning processes 2.33 -> 7.6 pages/s (GPU samples at 0%
# busy 58% -> 2-4%), beside 12 7.21-7.22 -> 8.60-8.62, idle 9.02-9.12 ->
# 9.02-9.07 on 1181-1198 -> 846-848 runner CPU-seconds; OMP_NUM_THREADS=4
# held the same as 1 beside half its CPUs (-13% to -14% against -73% at the
# default pool). Four, not one, because of the workstation (RX 9070 XT, 32 CPU
# threads, same 669 pages, detect=2): idle, one thread read 4.55 / 4.57
# pages/s where four read 4.80 and the default pool 4.77 (the card is ~79%
# busy there, so the CPU side of each call is on the critical path); beside
# 20 spinners one read 3.60, four 3.45, the default pool 1.49. It is applied
# to torch alone, so it reaches no detector process; a served engine gets
# its own cap through its environment (:func:`served_thread_env`).
GPU_ENGINE_TORCH_THREADS = 4
# An operator's own thread count, which the cap never overrides.
TORCH_THREAD_ENV: tuple[str, ...] = ("OMP_NUM_THREADS", "MKL_NUM_THREADS")
# The served ``mokuro`` engine is a process of its own, so
# its cap is an environment variable set when it is started. Measured on tower
# (RTX 4090, 48 CPU threads; mokuro served in fp16, 3 runs each): beside 24
# busy-spinning processes 29.43 / 29.25 / 29.73 pages/s at torch's default
# pool against 46.31 / 46.60 / 46.40 with OMP_NUM_THREADS=1; beside 12, 46.9
# against 47.3; idle 45.52 against 47.04. OMP alone, as measured: MKL takes
# its count from it when MKL_NUM_THREADS is unset. One thread, as measured;
# four is unmeasured for this engine.
SERVED_ENGINE_TORCH_THREADS = 1
SERVED_THREAD_CAP_ENV = {"OMP_NUM_THREADS": str(SERVED_ENGINE_TORCH_THREADS)}
# The count this thread last gave torch (see :func:`hold_torch_threads`).
_TORCH_THREADS = threading.local()


def cap_torch_threads(torch: Any, device: str) -> int | None:
    """A small torch CPU pool for a recognizer on a card (``GPU_ENGINE_TORCH_THREADS``).

    Called at the END of a recognizer's ``__init__``, so the load itself --
    weights, paddle's LoRA merge -- still runs on every core. Returns the
    count the recognizer then holds each calling thread to
    (:func:`hold_torch_threads`), or None when it holds none: on the CPU the
    pool IS the recognizer's compute and is left alone, as is a count the
    operator set (``TORCH_THREAD_ENV``).
    """
    if device == DEVICE_CPU:
        return None
    set_by = next((name for name in TORCH_THREAD_ENV if os.environ.get(name)), None)
    if set_by is not None:
        log(f"[runner] torch CPU threads: {set_by}={os.environ[set_by]} (as set; engine on {device})")
        return None
    torch.set_num_threads(GPU_ENGINE_TORCH_THREADS)
    log(f"[runner] torch CPU threads: {GPU_ENGINE_TORCH_THREADS} (engine on {device})")
    return GPU_ENGINE_TORCH_THREADS


def served_thread_env(force_cpu: bool, environ: Mapping[str, str]) -> dict[str, str]:
    """The thread cap a served engine is started under; ``{}`` for none.

    :data:`SERVED_THREAD_CAP_ENV` unless the engine is forced onto the CPU
    (where the pool IS its compute) or the operator set a count of their own
    (:data:`TORCH_THREAD_ENV`, which the child inherits as set). ``auto`` is
    capped too -- the live rows are ``auto`` and the fork puts them on the
    card -- and :meth:`OpenPipeline._open_served_road` restarts the process
    uncapped if its ready line says it landed on the CPU after all.
    """
    if force_cpu or any(environ.get(name) for name in TORCH_THREAD_ENV):
        return {}
    return dict(SERVED_THREAD_CAP_ENV)


def hold_torch_threads(torch: Any, count: int | None) -> None:
    """Give THIS thread torch's cap, once; called at the top of each recognizer call.

    ``torch.set_num_threads`` sets the calling thread's OpenMP/MKL count and
    a global that another thread only takes up lazily, at its first ATen
    ``parallel_for`` -- never at a BLAS call. Probed on torch 2.13.0+rocm7.1
    (MKL, OpenMP backend) with the cap set from another thread: a thread
    that had computed before kept 16 threads, and so did a fresh one whose
    first op was a matmul. The recognizer loads on the ``ocr-load`` thread
    and is called from the engine stage's workers, so each of those gives
    itself the count before its first computation here.
    """
    if count is None or getattr(_TORCH_THREADS, "count", None) == count:
        return
    torch.set_num_threads(count)
    _TORCH_THREADS.count = count


class HayaiNovaRecognizer:
    """hayai-ocr v2.5 "Nova", driven through transformers directly.

    The ``hayai_ocr`` PyPI package pins the v2 repo (and hardcodes
    ``max_num_patches=256``), so there is no helper to go through: the model
    card's own quickstart is the API, and it is what this reproduces. Three
    pieces have to line up, and none of them is ours to choose:

    * the vision side is stock SigLIP2 NaFlex, so the processor comes from
      ``google/siglip2-base-patch16-naflex`` and NOT from the model repo,
      which ships no preprocessor config;
    * the decoding loop is the repo's own (``trust_remote_code``), not the
      transformers one: it wants ``spatial_shapes`` and the ``tokenizer``
      itself, and it returns decoded STRINGS rather than token ids. It runs
      as :func:`nova_generate`, the repo's greedy loop with a batch's
      projector padding masked, because the repo's ``generate()`` leaves it
      visible and a line then read differently beside longer neighbours;
    * ``repetition_penalty`` stays 1.0 -- the repo's own default, which
      ``nova_generate`` keeps by having no penalty at all. Above 1.0 it
      would eat legitimate CJK reduplication (ドキドキ), which the card is
      explicit about.

    ``patches`` is ``max_num_patches``: NaFlex fits the crop to that budget
    keeping its aspect ratio, so it sets the resolution the line is read at.
    At the shipped batch of 16 the whole 256..512 span is 111 MiB of VRAM
    (786 -> 897 MiB peak) and +27% of this recognizer's wall clock -- see
    ``engines.PATCH_BUDGETS`` for the measured table.

    Both repos are pinned (``REPO_REVISIONS``): the model repo runs its own
    Python here under ``trust_remote_code``, and the processor repo decides
    how a crop is turned into patches.
    """

    # What each calling thread holds torch to (:func:`cap_torch_threads`).
    torch_threads: int | None = None
    # The autocast precision (``--precision``, resolved in ``__init__``);
    # fp32 (autocast off) until then.
    precision: str = PRECISION_FP32

    def __init__(
        self,
        patches: int = DEFAULT_PATCH_BUDGET,
        *,
        fold: bool = True,
        device: str | None = None,
        precision: str = DEFAULT_PRECISION_MODE,
        pick: str | None = None,
        pick_why: str = "",
    ) -> None:
        import_model_stack("torch", "transformers")
        import torch
        from transformers import AutoModel, AutoProcessor, PreTrainedTokenizerFast

        self.patches = int(patches)
        # Where the row put this stage (``pools.stage_device``), as torch
        # spells it; ``None`` is the probe this has always done.
        self.device = torch_device(device) if device else _pick_device()
        # The AUTOCAST dtype on a card (the weights stay as loaded), from the
        # row's mode (:data:`PRECISION_POLICY`) -- not the repo's own fp16.
        self.precision = recognizer_precision(
            torch, "hayai-nova", precision, self.device, pick=pick, pick_why=pick_why
        )
        repo = RECOGNIZER_REPOS["hayai-nova"]
        revision, vision_revision = pinned(repo), pinned(HAYAI_VISION_REPO)
        log(
            f"[runner] loading {repo}@{revision[:12]} on {self.device} "
            f"(max_num_patches={self.patches})"
        )
        model = AutoModel.from_pretrained(repo, revision=revision, trust_remote_code=True)
        self.model = model.to(self.device).eval()
        self.tokenizer = PreTrainedTokenizerFast.from_pretrained(repo, revision=revision)
        self.processor = AutoProcessor.from_pretrained(HAYAI_VISION_REPO, revision=vision_revision)
        # What actually read the page, for the sidecar's ``ocr_engine``.
        self.repos: dict[str, str] = {repo: revision, HAYAI_VISION_REPO: vision_revision}
        self.fold = bool(fold)
        self.torch = torch
        self.batch_size = max(1, int(HAYAI_NOVA_BATCH))
        self.torch_threads = cap_torch_threads(torch, self.device)

    def set_precision(self, name: str) -> None:
        """Autocast to ``name`` from the next call on. Nothing is reloaded or cast.

        For the benchmark's precision trials: the weights never leave fp32.
        """
        if name not in PRECISIONS:
            raise ValueError(f"{name!r} is not a precision to switch to")
        self.precision = PRECISION_FP32 if self.device == DEVICE_CPU else name

    def release_master(self) -> None:
        """Nothing to release: the weights never left fp32."""

    @property
    def amp_dtype(self) -> Any:
        """What the loop autocasts to, or None for no autocast (fp32, the CPU)."""
        if self.precision == PRECISION_FP32 or self.device == DEVICE_CPU:
            return None
        return torch_dtype(self.torch, self.precision)

    def __call__(self, crops: list[Any]) -> list[str]:
        hold_torch_threads(self.torch, self.torch_threads)
        if not crops:
            return []
        texts: list[str] = []
        for start in range(0, len(crops), self.batch_size):
            texts.extend(self._generate(list(crops[start : start + self.batch_size])))
        return texts

    def _generate(self, batch: list[Any]) -> list[str]:
        inputs = self.processor(images=batch, max_num_patches=self.patches, return_tensors="pt")
        with self.torch.inference_mode():
            # NOT self.model.generate(): the repo's generate() zero-pads every
            # row of a batch to the longest one after the DSCProjector and never
            # masks that padding, so a line's text depended on its page
            # neighbours (21 of 1,137 crops, 10 of 927 sidecar lines, against
            # one crop per call). nova_generate is the same loop with the
            # padding masked out; no repetition_penalty (1.0, as for CJK it must).
            out = nova_generate(
                self.model,
                self.tokenizer,
                inputs["pixel_values"].to(self.device),
                inputs["pixel_attention_mask"].to(self.device),
                inputs["spatial_shapes"].to(self.device),
                max_new_tokens=HAYAI_NOVA_MAX_NEW_TOKENS,
                precision=self.precision,
            )
        return [normalize_text(str(t)) if self.fold else str(t).strip() for t in out]


# The hayai-nova commit whose ``modeling_hayai.py`` ``generate()`` the loop in
# :func:`nova_generate` copies. A test holds it equal to the pin in
# ``REPO_REVISIONS``: moving the pin means diffing the new ``generate()``
# against that loop (and re-running tests/unit/test_hayai_nova_batching.py's
# torch half on the new file) before moving this.
NOVA_GENERATE_REVISION = "e46d79138499600564f810d44ab6bdea7230dee1"


def nova_key_bias(valid: Any, m_vision: int, total: int) -> Any:
    """``(b, 1, 1, total)`` additive bias: -1e9 on each row's projector padding.

    Slot ``j < m_vision`` is padding for row ``i`` when ``j >= valid[i]`` (that
    row's own ``h/2 * w/2`` token count); text slots (``j >= m_vision``) are left
    to the causal mask.
    """
    import torch

    slots = torch.arange(total, device=valid.device)[None, :]
    pad = (slots >= valid[:, None]) & (slots < m_vision)
    bias = torch.zeros((valid.shape[0], 1, 1, total), dtype=torch.float32, device=valid.device)
    return bias.masked_fill_(pad[:, None, None, :], -1e9)


def nova_generate(
    model: Any,
    tokenizer: Any,
    pixel_values: Any,
    pixel_attention_mask: Any,
    spatial_shapes: Any,
    *,
    max_new_tokens: int,
    precision: str = PRECISION_FP16,
) -> list[str]:
    """hayai-nova's own ``generate()`` (``modeling_hayai.py`` at
    ``NOVA_GENERATE_REVISION``), greedy, with the batch's projector padding
    masked from every attention.

    Identical to the repo's loop for a batch of one; for a batch, each row now
    reads exactly as it would alone (fp32: bit-for-bit the same texts; fp16:
    up to GEMM rounding). Measured on 1,137 real line crops: the repo's loop
    read 21 of them differently in a batch of 16 than alone, this one none,
    for +4-5% of the recognizer's time on CUDA; on the RX 9070 XT, where the
    card bounds the pipeline, ~5% of throughput (669 pages: 4.91 / 5.10 ->
    4.77 pages/s at the default torch pool). Re-check against the repo's
    ``generate()`` whenever the model revision pin moves.
    """
    import torch

    device = pixel_values.device
    b = pixel_values.size(0)
    bos_id = tokenizer.bos_token_id or 1
    eos_id = tokenizer.eos_token_id or 2
    pad_id = tokenizer.pad_token_id or eos_id
    dec = model.decoder
    rope = sys.modules[type(model).__module__].compute_batch_2d_mrope_freqs
    # The repo's own precision rule is fp16 autocast on a card and none
    # elsewhere; ``precision`` (``--precision``) may name another autocast
    # dtype, and fp32 turns it off.
    amp = device.type == "cuda" and precision != PRECISION_FP32
    amp_dtype = torch_dtype(torch, precision) if amp else torch.float32
    with torch.autocast(device_type=device.type, dtype=amp_dtype, enabled=amp):
        vision = model.vision_encoder(
            pixel_values=pixel_values,
            pixel_attention_mask=pixel_attention_mask,
            spatial_shapes=spatial_shapes,
        )
        tokens, new_shapes = dec.projector(vision.last_hidden_state, spatial_shapes)
        m_vision = tokens.size(1)
        bos = dec.token_embeddings(torch.full((b, 1), bos_id, dtype=torch.long, device=device))
        x = torch.cat([tokens, bos], dim=1)
        total = m_vision + max_new_tokens + 1
        key_bias = nova_key_bias((new_shapes[:, 0] * new_shapes[:, 1]).to(device), m_vision, total)
        prefill_mask = dec.generate_block_causal_mask(m_vision, 1, device) + key_bias[..., : m_vision + 1]
        cos_v, sin_v = rope(new_shapes, m_vision, 1, d_head=64, device=device)
        freqs = 1.0 / (10000.0 ** (torch.arange(0, 32, 2, device=device).float() / 32))
        steps = torch.arange(max_new_tokens + 1, device=device, dtype=torch.float32)
        text_freqs = torch.cat([torch.outer(steps, freqs), torch.outer(steps, freqs)], dim=-1)
        cos_t, sin_t = torch.cos(text_freqs), torch.sin(text_freqs)
        cache = {
            i: tuple(
                torch.zeros((b, layer.attn.h_kv, total, layer.attn.d_head), dtype=x.dtype, device=device)
                for _ in range(2)
            )
            for i, layer in enumerate(dec.layers)
        }
        for i, layer in enumerate(dec.layers):
            x = layer(x, mask=prefill_mask, cos_sin=(cos_v, sin_v), kv_cache=cache, layer_idx=i, cache_seqlens=0)
        seqlen = x.size(1)
        nxt = torch.argmax(dec.output_head(dec.final_norm(x[:, -1:]))[:, -1, :], dim=-1)
        out = torch.full((b, max_new_tokens + 1), pad_id, dtype=torch.long, device=device)
        out[:, 0], out[:, 1] = bos_id, nxt
        live = (nxt != eos_id) & (nxt != pad_id)
        for step in range(1, max_new_tokens):
            if not live.any():
                break
            xs = dec.token_embeddings(nxt.unsqueeze(1))
            cs = (cos_t[step].expand(b, 1, -1), sin_t[step].expand(b, 1, -1))
            for i, layer in enumerate(dec.layers):
                xs = layer(xs, mask=key_bias[..., : seqlen + 1], cos_sin=cs, kv_cache=cache,
                           layer_idx=i, cache_seqlens=seqlen)
            seqlen += 1
            logits = dec.output_head(dec.final_norm(xs))[:, -1, :]
            nxt = torch.argmax(logits, dim=-1) * live + pad_id * (~live)
            out[:, step + 1] = nxt
            live = live & (nxt != eos_id) & (nxt != pad_id)
    return [
        tokenizer.decode([t for t in seq.tolist()[1:] if t not in (eos_id, pad_id)], skip_special_tokens=True)
        for seq in out
    ]


def linear_patch_embedding(model: Any) -> int:
    """Swap the vision tower's patch ``Conv2d`` for the same weights as a matmul.

    The tower embeds ``(patches, 3, 14, 14)`` with a convolution whose kernel
    IS the patch (kernel == stride, no padding): one dot product per patch,
    i.e. a linear layer. As a convolution its batch dimension is the number
    of patches on the page, a new value for nearly every call, and MIOpen
    (ROCm) searches for a kernel per shape: measured on an RX 9070 XT, 23-33 s
    of search for each batch of four to six line crops -- 60-110 s a page --
    against 0.1 s once the shape is known. The matmul has no such search and
    gives the same numbers. Returns how many layers were swapped.
    """
    import torch

    class PatchLinear(torch.nn.Module):  # type: ignore[misc]
        def __init__(self, conv: Any) -> None:
            super().__init__()
            self.weight, self.bias = conv.weight, conv.bias

        def forward(self, patches: Any) -> Any:
            out = torch.nn.functional.linear(
                patches.flatten(1), self.weight.flatten(1), self.bias
            )
            return out[:, :, None, None]  # the conv's (N, C, 1, 1)

    swapped = 0
    for name, module in list(model.named_modules()):
        if not (name.endswith("patch_embedding") and isinstance(module, torch.nn.Conv2d)):
            continue
        whole_patch = tuple(module.kernel_size) == tuple(module.stride)
        unpadded = module.padding in ("valid", (0, 0)) and tuple(module.dilation) == (1, 1)
        if whole_patch and unpadded and module.groups == 1:
            parent = model.get_submodule(name.rsplit(".", 1)[0]) if "." in name else model
            setattr(parent, name.rsplit(".", 1)[-1], PatchLinear(module))
            swapped += 1
    return swapped


class PaddleMangaRecognizer:
    """PaddleOCR-VL-1.6 with the manga LoRA merged, a page's crops in batches.

    ``use_cache=True`` is passed explicitly: the base model's shipped
    generation config says ``false``, and without the KV cache every new
    token re-runs the whole prompt (the LoRA's card: same text, ~10x slower).

    ``fold`` NFKC-normalizes the output, as the LoRA's card prescribes for a
    text compared with its training targets. A caller that reconciles the
    read with a second one wants the characters as generated instead: the
    model does write ``…``, and folding it to ``...`` only has to be undone.
    """

    # Callers that know a crop's glyph room pass ``max_tokens`` (one per crop).
    token_caps = True
    # What each calling thread holds torch to (:func:`cap_torch_threads`).
    torch_threads: int | None = None

    def __init__(
        self,
        batch_size: int = PADDLE_BATCH,
        *,
        fold: bool = True,
        device: str | None = None,
        precision: str = DEFAULT_PRECISION_MODE,
        pick: str | None = None,
        pick_why: str = "",
    ) -> None:
        import_model_stack("torch", "transformers", "peft", "safetensors.torch")
        import torch
        from huggingface_hub import hf_hub_download
        from peft import PeftModel
        from safetensors.torch import load_file
        from transformers import AutoModelForImageTextToText, AutoProcessor

        # Where the row put this stage; ``None`` is the probe this has always
        # done. The precision follows the DEVICE, not the host: an engine
        # pinned to the CPU reads in float32 on a machine with a card. On a
        # card it is what the row's mode resolves to there (:data:`PRECISION_POLICY`).
        self.device = torch_device(device) if device else _pick_device()
        self.precision = recognizer_precision(
            torch, "paddle-manga", precision, self.device, pick=pick, pick_why=pick_why
        )
        dtype = torch_dtype(torch, self.precision)
        repo = RECOGNIZER_REPOS["paddle-manga"]
        revision, base_revision = pinned(repo), pinned(PADDLE_BASE_REPO)
        log(
            f"[runner] loading {PADDLE_BASE_REPO}@{base_revision[:12]} + "
            f"{repo}@{revision[:12]} on {self.device} ({dtype})"
        )
        model = AutoModelForImageTextToText.from_pretrained(
            PADDLE_BASE_REPO,
            revision=base_revision,
            dtype=dtype,
            attn_implementation="sdpa",
        )
        model = PeftModel.from_pretrained(model, repo, revision=revision).merge_and_unload()
        tower = load_file(hf_hub_download(repo, "tower.safetensors", revision=revision))
        tower = {k: v.to(dtype) for k, v in tower.items()}
        result = model.load_state_dict(tower, strict=False)
        if result.unexpected_keys:
            raise RuntimeError(f"unexpected tower keys: {result.unexpected_keys[:5]}")
        linear_patch_embedding(model)
        self.model = model.to(self.device).eval()
        self.processor = AutoProcessor.from_pretrained(PADDLE_BASE_REPO, revision=base_revision)
        # What actually read the page, for the sidecar's ``ocr_engine``.
        self.repos: dict[str, str] = {PADDLE_BASE_REPO: base_revision, repo: revision}
        self.fold = bool(fold)
        # Decoder-only generation continues from the last position, so a
        # batch is padded on the left (as the LoRA was trained).
        self.processor.tokenizer.padding_side = "left"
        messages = [
            {"role": "user", "content": [{"type": "image"}, {"type": "text", "text": "OCR:"}]}
        ]
        self.prompt = self.processor.apply_chat_template(
            messages, add_generation_prompt=True, tokenize=False
        )
        self.torch = torch
        self.batch_size = max(1, int(batch_size))
        self.torch_threads = cap_torch_threads(torch, self.device)
        # The fp32 weights every benchmark cast starts from, taken on the
        # first :meth:`set_precision` and dropped by :meth:`release_master`.
        self._master: dict[str, Any] | None = None

    def set_precision(self, name: str) -> None:
        """Re-cast the weights to ``name``, always from the fp32 master copy.

        For the benchmark's precision trials: the model is loaded in fp32, the
        first switch keeps an fp32 copy of its weights on the CPU, and every
        cast after that starts from the copy -- never from the last cast, and
        never from disk. On the CPU it stays fp32.
        """
        if name not in PRECISIONS:
            raise ValueError(f"{name!r} is not a precision to switch to")
        if self.device == DEVICE_CPU or name == self.precision:
            return
        if self._master is None:
            if self.precision != PRECISION_FP32:
                raise RuntimeError(
                    f"paddle-manga was loaded in {self.precision}; only an fp32 load "
                    "can be re-cast exactly"
                )
            self._master = fp32_master(self.model)
        with self.torch.no_grad():
            cast_from_master(self.model, self._master, torch_dtype(self.torch, name))
        self.precision = name

    def release_master(self) -> None:
        """Drop the fp32 copy once the precision is settled."""
        self._master = None

    def __call__(self, crops: list[Any], max_tokens: Sequence[int] | None = None) -> list[str]:
        hold_torch_threads(self.torch, self.torch_threads)
        caps = (
            [int(c) for c in max_tokens]
            if max_tokens is not None
            else [DEFAULT_MAX_NEW_TOKENS] * len(crops)
        )
        if len(caps) != len(crops):
            raise ValueError(f"{len(caps)} token caps for {len(crops)} crops")
        areas = [float(crop.size[0] * crop.size[1]) for crop in crops]
        texts = [""] * len(crops)
        for batch in plan_generation_batches(areas, caps, self.batch_size):
            for index, text in zip(batch, self._generate(batch, crops, caps), strict=True):
                texts[index] = text
        return texts

    def _generate(
        self, batch: Sequence[int], crops: Sequence[Any], caps: Sequence[int]
    ) -> list[str]:
        inputs = self.processor(
            text=[self.prompt] * len(batch),
            images=[crops[i] for i in batch],
            return_tensors="pt",
            padding=True,
        ).to(self.device)
        with self.torch.inference_mode():
            out = self.model.generate(
                **inputs,
                max_new_tokens=max(caps[i] for i in batch),
                do_sample=False,
                use_cache=True,
            )
        prompt_len = inputs["input_ids"].shape[-1]
        texts = []
        for row, index in enumerate(batch):
            # The batch ran to its longest member's cap; each line keeps its own.
            tokens = out[row][prompt_len : prompt_len + caps[index]]
            decoded = self.processor.tokenizer.decode(tokens, skip_special_tokens=True)
            texts.append(normalize_text(decoded) if self.fold else decoded.strip())
        return texts


# ---------------------------------------------------------------------------
# The staged page pipeline
# ---------------------------------------------------------------------------
#
# This file runs the COMPOSED engines: the ones where we hold the seam between
# a detector and one or more recognizers. (The monolithic road -- ``mokuro``,
# which detects and recognizes behind its own CLI in its own environment --
# never arrives here: ``processor`` sends ``EngineSpec.uses_mokuro_env``
# engines to ``_run_mokuro`` instead. There is no seam to schedule there, and
# nothing below tries.)
#
# ONE VOLUME, ONE PIPELINE. Every stage of a page has a POOL OF ITS OWN SIZE,
# and a BOUNDED QUEUE sits between every pair of stages -- including both sides
# of the stage that holds the GPU. This is not ``ocr.concurrency``, which runs
# several VOLUMES at once: that costs a second copy of every model and makes a
# single volume slower, and it is a different knob for a different problem.
#
# Every seam we hold is a place where one device waits on another. Run in
# lockstep, a page's CPU work and its GPU work take turns idling: sampling the
# card at 1 Hz through a paddle-manga volume gave 99 5 99 99 4 16 98 19 99 95
# 97 4 -- bimodal, flat out or nearly stopped, never steady. Each trough is the
# card waiting on a thread that is not the card.
#
# So a page is described as an ordered STAGE GRAPH (:data:`STAGE_GRAPHS`), each
# stage naming the device it occupies and how wide it may go, and one generic
# scheduler (:class:`StagePipeline`) runs whatever graph it is handed. The
# scheduler never looks at an engine id.
#
# WHERE THE STAGE BOUNDARIES GO. The stage holding the recognizer holds ONLY
# the recognizer: the boxing, the CTC read and the crops are the stage in front
# of it, the layout and raw dump the stage behind it, and
# both have pools of their own. It is never first or last, because a stage at
# the end has no output queue -- so it can neither be seen to be BLOCKED (the
# signal that what comes after it is too narrow) nor hand its device back when
# it has run ahead.
#
# BACKPRESSURE IS THE MECHANISM AND THE DIAGNOSTIC. Queue operations block on
# condition variables; they never spin (measured: a producer parked for a whole
# second on a full queue burned 0.038 ms of CPU), so a GPU-side producer that
# has run ahead gives the FLOPs back rather than burning them waiting. And
# since every wait is timed, the queues say where the time went: a queue that
# sits EMPTY means the stage behind it is starved (widen its feeder); a queue
# that sits FULL means the stage filling it is blocked (widen its drain). The
# per-volume summary and the live stats file print both, per queue.
#
# SIZING is off measured cost, RELATIVE to the stage that sets the pace
# (:func:`plan_stage_workers`), with the host budget as a per-stage ceiling
# rather than a pot the stages share -- and every width and capacity is
# settable by hand (``--stage-workers``, ``--queue-capacity``). The derivation
# is a starting point; the queue counters are what should correct it. Note
# that the cost model has already been wrong once here: it sized paddle-manga's
# CPU stage at ONE worker (0.29 s of CPU hiding inside 0.82 s of GPU) and the
# trace above is what that width actually did to the card.
#
# WHAT IS NOT SPLIT, and why. Detection and the CTC read stay one stage, though
# the CTC read is 2.4x the detector (105.1 ms against 43.4 ms a page; measured
# over 24 pages at 4 threads, 173.3 ms of CPU a page, of which detector 43.4 ms
# (25%) and CTC 105.1 ms (61%), and only 50.0 ms of the CTC inside
# ``read_page``). Two stages of one page each would CAP the win at 1/0.61 =
# 1.65x and leave the detector idle 60% of the time; one stage running whole
# pages in parallel has no such ceiling, because a worker holds a ``PPOcr`` and
# a ``PPOcr`` is BOTH onnxruntime sessions -- K workers are K detector sessions
# and K CTC sessions at once. Measured end to end on 24 real pages,
# ``ppocr-manga`` (0% GPU, so this is the pure CPU win): 5.8 s serial, 3.0 s at
# four workers -- 1.93x, past what a detect|CTC split could ever reach.
#
# Nor are paddle-manga's two GPU batches split into two stages: which lines get
# the second read is decided by RECONCILING the first, so the second batch
# cannot be issued until the first has come back and been merged (the crops and
# ``reconcile_line`` between them are 2.4 and 0.8 ms of a manga page, 16.9 and
# 6.4 of a novel one). They stay in one stage with the reconcile between them.
#
# What the tail costs, measured with the VLM stubbed out so only CPU is left
# (12 manga pages at 14 lines a page; 12 novel-prose pages at 32):
#
#   engine / pages        detect+CTC+join+probes+layout#1   everything after
#   paddle-manga manga            293.7 ms                     10.6 ms  (3.5%)
#   paddle-manga novel           1545.6 ms                     56.8 ms  (3.5%)
#   hayai-nova   manga            189.2 ms                      9.9 ms  (5.0%)
#   hayai-nova   novel            798.1 ms                     33.7 ms  (4.0%)
#
# The tail is small, which is why it was once left on the GPU's thread. It is
# its own pooled stage now anyway: 3.5% of a page is 3.5% of the card, the
# stage has to exist for the engine to have an output queue at all, and if it
# ever does become the bottleneck the queue behind the engine will say so.
#
# NOTHING ABOUT THE OUTPUT CHANGES. Pages come out in input order (rebuilt at
# the sink from a sequence number), each page's lines are reconciled only
# against its own detection, ``review.json`` is appended by the driver in page
# order rather than by whichever post worker finished first, and a failure
# anywhere in the graph surfaces at that page's place in the order -- so the
# per-page ``blank_page()`` fallback catches it exactly as it always has. The
# fully serial path survives behind ``--cpu-workers 0``, and is the shape every
# byte-identity test pins the output against.

# Per-session intra-op threads. Kept at ``ppocr``'s own default deliberately:
# neither model scales with threads (detector 4->8 threads is 1.15x; the CTC
# recognizer 1.26x isolated and 1.02x over a whole page, because the detector
# regresses as the recognizer improves; whole-page CPU at 1/4/8/16 threads is
# 459.8/173.3/170.0/204.5 ms). Threads are also not ours to set -- they are
# ``MOKURO_PPOCR_THREADS`` -- and 24 bench pages came out with identical quads
# at 1, 2, 4 and 8 of them, so this is a budgeting number, not a behavioural one.
SESSION_THREADS = 4
# Pages in the CPU stage at once -- the cap, not the default. Sessions are what
# scales: the detector A/B-interleaved on a 32-core host at the real input
# shape (896x1280, ~76% of the library), median of five 48-page runs, against
# an OCR job that was running throughout:
#
#   K x threads   OS threads   pages/s   vs 1x4   CPU s/page
#     1 x 4            4         15.1     1.00x      0.287
#     2 x 4            8         20.8     1.38x      0.400
#     3 x 4           12         21.9     1.45x      0.541
#     4 x 4           16         21.3     1.41x      0.686
#     6 x 2           12         21.5     1.42x      0.520
#     8 x 1            8         21.1     1.40x      0.382
#    12 x 1           12         20.1     1.33x      0.567
#
# The same sweep re-run once the other job ended, on an idle host, is the one
# that settles it: 1x4 17.2 pages/s, 2x4 23.6 (1.37x), 3x4 25.8 (1.50x), 4x4
# 25.9 (1.50x), 6x4 24.5 (1.42x), 8x4 23.4 (1.36x). The DETECTOR really does
# plateau at four sessions and get worse after. The whole CPU stage does
# better, because the CTC half scales where the detector does not -- 24 real
# pages end to end, ``ppocr-manga``, idle host:
#
#   workers   manga            novel prose
#     0       4.8 s  1.00x     16.8 s  1.00x
#     1       4.8 s  1.00x     16.6 s  1.01x   (no GPU stage to overlap)
#     2       3.1 s  1.55x      9.7 s  1.73x
#     3       2.6 s  1.85x      8.0 s  2.10x
#     4       2.5 s  1.92x      7.3 s  2.30x
#     6       2.7 s  1.78x      9.1 s  1.85x
#     8       2.9 s  1.66x      9.5 s  1.77x
#
# Four is the measured turning point on both, and past it throughput FALLS. So
# this is a measured ceiling on the derivation below, not the derivation: it
# only bites where the host is big enough to afford more, and an explicit
# width -- ``--cpu-workers``, ``--stage-workers``, or either environment
# variable -- is taken verbatim, cap included.
CPU_WORKERS_MAX = 4
# Concurrent OCR jobs the budget assumes when nothing says otherwise. Matches
# the shipped ``ocr.concurrency`` default of 1.
#
# A runner is a separate process and cannot see its siblings, so THIS IS A
# CONTRACT: a server that raises ``ocr.concurrency`` must export
# ``MOKURO_OCR_JOBS`` with it (one line in the subprocess env), or every job
# will budget for the whole host. The startup log says which number was used,
# so a violation is visible rather than silent.
CPU_DEFAULT_JOBS = 1
# Default capacity of the queue a stage fills, in pages: the width of the
# stage that fills it. An item here can hold a decoded page image (~14 MB for
# a 1700x2800 scan), so CAPACITY IS MEMORY -- this is the whole reason a
# 292-page volume stays flat, and the reason the default is a handful of slots
# rather than a comfortable buffer. ``--queue-capacity`` moves it per queue.
QUEUE_CAPACITY_PER_WORKER = 1
# Capacity of the queue that FEEDS a device-bound stage, in pages. That
# stage's model is still loading while the volume starts
# (:class:`DeferredRecognizer` loads it on a thread so detection does not wait
# for it), and the point of the window is that the stage before it fills a
# queue instead of blocking on a one-slot hand-off. Four pages of a decoded
# image is about 56 MB -- the entire memory cost of the overlap.
LOAD_WINDOW_SLOTS = 4
# Headroom on a pooled stage's derived width, for PAGE-TO-PAGE VARIANCE.
#
# ``ceil(cost / pace)`` sizes a stage on its MEAN page, and a mean-sized pool
# starves the stage after it on every page above the mean -- a worker inside a
# long page has stopped producing, and a pool of one has then stopped
# altogether. What decides is the slow page, so the width is derived from a
# high percentile rather than the mean, and this is that percentile expressed
# as a multiple of it.
#
# MEASURED, on the stage the rule sizes: the reconciled road's detect stage
# (ppocr-manga detector + CTC read) over 24 real manga pages is 156.3 ms a
# page at the mean, 291.9 ms at p90 and 296.5 at p95 -- p90/mean = 1.87, and
# the slowest page is 3.11x the mean. 2.0 is that, rounded up. (A detector
# adapter's own pages vary far less -- ctd over the same volume is 1.18x at
# p90 -- but that stage is pinned to one by the GPU rule below anyway.)
#
# It is also what makes the rule land on the widths the idle measurements
# found: reconciled/paddle-manga detect 1 (0.225 s upstream against 0.915 s on
# the card: 0.02% engine idle at width 1, and widening it measured WORSE), and
# reconciled/hayai-nova detect 3 (0.225 s against 0.177 s: 25.7% idle at 1,
# 5.2% at 2, 0.8% at 3). Without headroom the same rule gives hayai-nova 2,
# where the card is still idle a twentieth of the time.
#
# This replaced FEED_FLOOR = 2, a floor on every pooled stage beside a
# device-bound one. The verifier measured it: at paddle-manga's ratio it
# changed card occupancy by 0.1 points (96.3% at width 1 against 96.2% at the
# floored [2,1,2]) and cost a core. A floor cannot tell the two engines apart;
# a percentile can, because it is still relative to what the stage costs.
STAGE_WIDTH_HEADROOM = 2.0

CPU_WORKERS_ENV = "MOKURO_OCR_CPU_WORKERS"
CPU_JOBS_ENV = "MOKURO_OCR_JOBS"
STAGE_WORKERS_ENV = "MOKURO_OCR_STAGE_WORKERS"
QUEUE_CAPACITY_ENV = "MOKURO_OCR_QUEUE_CAPACITY"
STAGE_DEVICE_ENV = "MOKURO_OCR_STAGE_DEVICE"
# The key a bare number is filed under: "every stage", the shape
# ``--cpu-workers`` has always had.
ALL_STAGES = "*"
# Where the live queue/pool numbers are published for another process to read.
# Never inside ``--cache-dir``: the server counts the JSON files there to know
# how many pages are done, and a stats file would be counted as a page.
PIPELINE_STATS_ENV = "MOKURO_OCR_PIPELINE_STATS"
PIPELINE_STATS_FILE = "pipeline.json"
# How often the live file is rewritten while a volume runs.
PIPELINE_STATS_INTERVAL = 2.0

# The kernel's CPU pressure (PSI): how much of the time some runnable task on
# this host was waiting for a CPU. A volume read while a NEIGHBOUR loads the
# machine is not this machine's speed (perf diagnosis F5: the same volume at
# 12.6 pages/s at night and 4.05 beside three CPU jobs), so each volume
# reports the pressure over its own window, and the server does not learn a
# contended volume as the machine's rate.
PSI_CPU = Path("/proc/pressure/cpu")


def _psi_some(field: str) -> float | None:
    """One field of the ``some`` line of /proc/pressure/cpu, or None."""
    try:
        text = PSI_CPU.read_text(encoding="ascii")
    except (OSError, UnicodeDecodeError):
        return None
    for line in text.splitlines():
        if line.startswith("some "):
            for part in line.split()[1:]:
                key, _, value = part.partition("=")
                if key == field:
                    try:
                        return float(value)
                    except ValueError:
                        return None
    return None


def cpu_pressure_total() -> float | None:
    """Microseconds, since boot, that some task here waited for a CPU."""
    return _psi_some("total")


def cpu_pressure_now() -> float | None:
    """The share of the last 10 s some task here waited for a CPU (0-1)."""
    avg10 = _psi_some("avg10")
    return None if avg10 is None else round(avg10 / 100.0, 3)


# What the NEIGHBOURS did with the host's CPU, which the pressure above cannot
# say. Once a recognizer on a card keeps torch to a small pool
# (``GPU_ENGINE_TORCH_THREADS``), the runner barely waits for a CPU even
# beside a heavy neighbour: measured on tower with a neighbour on half its
# CPUs, the pressure read 0.29 before that cap and ~0.01-0.015 after it (one
# thread), a GPU neighbour 0.01-0.02, and the library's line (0.6) was only
# crossed near full saturation; on the workstation beside 20 spinners the host's
# pressure read 0.41 at the default pool, 0.13 at one thread, 0.16 at four. So each volume also reports the share of the host's CPU
# time over its window that went to processes that are NOT an OCR runner or
# anything under one: /proc/stat's busy time minus the CPU of every runner's
# process tree -- this one's, a sibling session's (another job on this host
# is this machine's normal work, not a neighbour), their detector processes
# and serve children. 0.5 is half the host's CPUs kept busy by others.
PROC = Path("/proc")
# A process whose command line runs this script is an OCR runner.
RUNNER_SCRIPT = Path(__file__).name
# How far back the live ``stats`` event's share looks, like PSI's avg10, and
# how often it is re-sampled: a sample walks /proc (30-45 ms on a desktop with
# ~900 processes), so the live figure moves every few seconds, not every tick.
HOST_SHARE_WINDOW = 10.0
HOST_SHARE_EVERY = 5.0
# The kernel's thread spawner: its children are kernel threads.
KTHREADD_PID = 2


def host_cpu_jiffies(proc: Path | None = None) -> tuple[int, int] | None:
    """(busy, total) clock ticks of every CPU here since boot, or None.

    Busy is user + nice + system + irq + softirq + steal (guest time is
    already inside user); total adds idle and iowait.
    """
    try:
        first = ((proc or PROC) / "stat").read_text(encoding="ascii").splitlines()[0]
    except (OSError, UnicodeDecodeError, IndexError):
        return None
    parts = first.split()
    if not parts or parts[0] != "cpu":
        return None
    try:
        values = [int(value) for value in parts[1:9]]
    except ValueError:
        return None
    user, nice, system, idle, iowait, irq, softirq, steal = values + [0] * (8 - len(values))
    busy = user + nice + system + irq + softirq + steal
    return busy, busy + idle + iowait


# One process of an OCR runner's tree in a sample: (pid, start time in clock
# ticks after boot) -> (its lifetime ticks, its parent pid). The start time
# tells a reused pid from the process that held it before.
RunnerTicks = dict[tuple[int, int], tuple[int, int]]
# (host busy, host total, the runners' processes) -- see :func:`host_sample`.
HostSample = tuple[int, int, RunnerTicks]


def ocr_tree_processes(proc: Path | None = None) -> RunnerTicks | None:
    """Every OCR runner here and everything under it, by process, or None.

    A runner is a process whose argv names ``RUNNER_SCRIPT`` (this process
    always is one) -- by its command line, never its name, which is its main
    thread's and which a library may rename; its tree is every descendant,
    whatever it runs. A process's ticks are its own plus its reaped
    children's (``utime`` to ``cstime``), so a detector that exits mid-window
    is still counted, in its parent. Kernel threads (children of kthreadd,
    pid 2) have no command line and are not read for one.
    """
    root = proc or PROC
    try:
        entries = [entry for entry in root.iterdir() if entry.name.isdigit()]
    except OSError:
        return None
    parent: dict[int, int] = {}
    ticks: dict[int, int] = {}
    started: dict[int, int] = {}
    runners: set[int] = {os.getpid()} if proc is None else set()
    needle = RUNNER_SCRIPT.encode()
    for entry in entries:
        pid = int(entry.name)
        try:
            stat = (entry / "stat").read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        fields = stat.rpartition(")")[2].split()
        if len(fields) < 20:
            continue
        try:
            parent[pid] = int(fields[1])
            ticks[pid] = sum(int(value) for value in fields[11:15])
            started[pid] = int(fields[19])
        except ValueError:
            continue
        if pid == KTHREADD_PID or parent[pid] == KTHREADD_PID:
            continue
        try:
            argv = (entry / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        if any(arg.endswith(needle) for arg in argv):
            runners.add(pid)
    children: dict[int, list[int]] = {}
    for pid, ppid in parent.items():
        children.setdefault(ppid, []).append(pid)
    ours: set[int] = set()
    stack = [pid for pid in runners if pid in ticks]
    while stack:
        pid = stack.pop()
        if pid in ours:
            continue
        ours.add(pid)
        stack.extend(children.get(pid, ()))
    return {(pid, started[pid]): (ticks[pid], parent[pid]) for pid in ours}


def ocr_tree_jiffies(proc: Path | None = None) -> int | None:
    """CPU clock ticks of every OCR runner here and everything under it, or None."""
    processes = ocr_tree_processes(proc)
    if processes is None:
        return None
    return sum(ticks for ticks, _ in processes.values())


def host_sample(proc: Path | None = None) -> HostSample | None:
    """(host busy, host total, OCR runners' processes) now, or None."""
    host = host_cpu_jiffies(proc)
    ours = ocr_tree_processes(proc)
    if host is None or ours is None:
        return None
    return host[0], host[1], ours


def runner_ticks_between(before: RunnerTicks, after: RunnerTicks) -> int:
    """What the OCR runners' trees used between two samples, in clock ticks.

    Not the difference of two sums: a sample holds each process's LIFETIME
    ticks, so a sibling runner that exits inside the window (another
    session, an autobench runner, a local slot's job) would take its whole
    lifetime out of the second sum and read as a neighbour's work. Instead
    each process is followed by (pid, start time): one in both samples
    counts its growth -- which takes in children it reaped meanwhile -- and
    one that started inside the window counts all it has. A process that
    exited is found in its nearest ancestor that is still here: its ticks
    flowed into that ancestor's reaped-children counters, which already
    include what it had before the window, so that part is taken back off
    once. One with no surviving ancestor (a sibling runner, reaped by
    whatever started it) left nothing behind here; only its last ticks
    inside the window are unaccounted for, and read as others'. (A process
    orphaned out of the tree while an ancestor lives on is taken off too,
    though nothing reaped it: rare, and it errs toward "host busy".)
    """
    used = 0
    for key, (ticks, _) in after.items():
        previous = before.get(key)
        used += ticks if previous is None else max(0, ticks - previous[0])
    by_pid = {key[0]: key for key in before}
    for key, (ticks, ppid) in before.items():
        if key in after:
            continue
        seen = {key[0]}
        while ppid in by_pid and ppid not in seen:
            seen.add(ppid)
            ancestor = by_pid[ppid]
            if ancestor in after:
                used -= ticks
                break
            ppid = before[ancestor][1]
    return max(0, used)


def other_cpu_share(before: HostSample | None, after: HostSample | None) -> float | None:
    """The share of the host's CPU (0-1) others used between two samples."""
    if before is None or after is None:
        return None
    total = after[1] - before[1]
    if total <= 0:
        return None
    others = (after[0] - before[0]) - runner_ticks_between(before[2], after[2])
    return round(max(0.0, min(1.0, others / total)), 3)

# What a stage occupies while it runs. Only the log and the summary read it;
# it is here because a graph that does not say which device a stage holds
# cannot be scheduled by anything but the engine that wrote it.
#
# A device id is ``auto``, ``cpu`` or ``gpu:<n>`` -- bunko's public spelling,
# whatever the vendor is: torch calls a card ``cuda:<n>`` on ROCm as well as on
# CUDA, and that translation (:func:`torch_device`) happens here, at the edge
# where a model is actually placed. ``gpu`` on its own is the same card without
# an index, which is what ``auto`` means on a host that has one.
DEVICE_CPU = "cpu"
DEVICE_GPU = "gpu"
DEVICE_AUTO = "auto"
GPU_DEVICE_PREFIX = "gpu:"
TORCH_GPU_PREFIX = "cuda:"
# A hand-written index above this is a typo, not a machine: refused before it
# reaches torch, where it would be a late and unhelpful crash.
MAX_GPU_INDEX = 15
# A width of :data:`SERIAL`: no pool of its own. The stage is FUSED into the
# stage before it and runs on that stage's worker; when every stage is SERIAL
# there are no threads at all and the pipeline is the old serial path.
SERIAL = 0
# ``StageSpec.max_workers``: one worker, structurally. A stage holding a single
# GPU model is this -- widening it would mean a second copy of the model, which
# is a different decision from widening a pool of CPU sessions.
DEVICE_BOUND = 1
# ``StageSpec.max_workers``: as many pages at once as this run's budget allows.
POOLED = None


def device_is_gpu(device: str) -> bool:
    """True when this device id names a card, whatever its index or spelling."""
    text = str(device or "")
    return (
        text == DEVICE_GPU
        or text.startswith(GPU_DEVICE_PREFIX)
        or text == "cuda"
        or text.startswith(TORCH_GPU_PREFIX)
    )


def device_index(device: str) -> int:
    """Which card a device id names; 0 for a card that does not say."""
    text = str(device or "")
    _, _, tail = text.partition(":")
    try:
        return int(tail)
    except ValueError:
        return 0


def parse_device(value: str) -> str:
    """One device id, normalised to ``auto``, ``cpu`` or ``gpu:<n>``.

    Accepts torch's own ``cuda``/``cuda:<n>`` spelling as well, because the
    runner's CLI is typed by hand and by a server that knows torch's names --
    both mean the same card, and one spelling reaches the models.
    """
    text = str(value or "").strip().lower()
    if not text:
        raise ValueError("a device is required (auto, cpu or gpu:<n>)")
    if text in (DEVICE_AUTO, DEVICE_CPU):
        return text
    if text in (DEVICE_GPU, "cuda"):
        return f"{GPU_DEVICE_PREFIX}0"
    for prefix in (GPU_DEVICE_PREFIX, TORCH_GPU_PREFIX):
        if text.startswith(prefix):
            tail = text[len(prefix) :]
            if tail.isdigit() and int(tail) <= MAX_GPU_INDEX:
                return f"{GPU_DEVICE_PREFIX}{int(tail)}"
            raise ValueError(
                f"{value!r} does not name a device: a card is gpu:<n>, "
                f"n between 0 and {MAX_GPU_INDEX}"
            )
    raise ValueError(f"{value!r} does not name a device (auto, cpu or gpu:<n>)")


def torch_device(device: str) -> str:
    """The same device as torch spells it: ``cpu`` or ``cuda:<n>``."""
    if device_is_gpu(device):
        return f"{TORCH_GPU_PREFIX}{device_index(device)}"
    return DEVICE_CPU


def _same_device(reported: str, wanted: str) -> bool:
    """Whether what a detector says it loaded is what it was asked for.

    Loose on purpose: an adapter reports what its own library calls the place
    (``cuda:0``, ``gpu``, ``cpu``), and only the CPU/card distinction plus the
    index are ours to check.
    """
    got, want = str(reported or "cpu"), str(wanted or "")
    if device_is_gpu(want):
        return device_is_gpu(got) and (
            ":" not in got or device_index(got) == device_index(want)
        )
    return not device_is_gpu(got)


def resolve_device(device: str, *, gpu: bool, cpu_only: bool = False) -> str:
    """What ``auto`` means here: card 0 when there is one, else the CPU.

    ``cpu_only`` is a model that cannot leave the CPU (the ppocr pair is
    onnxruntime): it resolves to the CPU whatever was asked, because the
    refusal belongs at the edit that asked, not at the placement.
    """
    if cpu_only:
        return DEVICE_CPU
    asked = parse_device(device or DEVICE_AUTO)
    if asked != DEVICE_AUTO:
        return asked
    return f"{GPU_DEVICE_PREFIX}0" if gpu else DEVICE_CPU


class StageSpec(NamedTuple):
    """One stage of a page: what it is, what it occupies, what it costs.

    ``key`` is the stable short name the user tunes it by (``--stage-workers
    detect=4``), the queues are named after and the live stats file reports;
    ``name`` is the human sentence for a log line.

    ``seconds`` is what a page spends in this stage, measured. It is SIZING
    DATA and nothing else -- it never touches what is written -- and it is what
    lets one rule size a stage RELATIVE to its neighbours instead of carving a
    global budget up (:func:`plan_stage_workers`). A stage whose cost depends on
    the engine rather than the road overrides it in :data:`ENGINE_STAGE_SECONDS`.

    ``max_workers`` is a hard structural ceiling on top of that, and the only
    limit an explicit width cannot argue with. :data:`DEVICE_BOUND` means one
    worker because there is one model on one device; :data:`POOLED` means only
    the host limits it.

    Note that a pool of ONE is not the serial path: it is one page at a time on
    a thread of its own, between two queues, which is what lets it overlap the
    stages either side of it. The serial path is every stage at width
    :data:`SERIAL`, and it is a fallback, not the default.
    """

    key: str
    name: str
    device: str
    max_workers: int | None
    seconds: float


# The roads a page can take through this file. Which one an engine/detector
# pair takes is :func:`page_road`; what each costs is measured in the section
# header above.
ROAD_LINE = "line"
ROAD_RECONCILED = "reconciled"
ROAD_ADAPTER = "adapter"
# An engine that IS a process: pages in, page JSON out, one model load for the
# whole session (``SERVED_ENGINES``). It joins the other roads rather than
# staying the one engine that pays a process start, a model load and a full
# extraction a volume.
ROAD_SERVED = "served"

# Stage keys, so a graph, the per-engine cost table and a user's
# ``--stage-workers`` cannot drift apart. ONE SET ACROSS EVERY ROAD: the
# adapter road's first stage used to be ``decode`` because the detector had
# already run; now it runs the detector, so it is ``detect`` like the others
# and ``--stage-workers detect=2`` means the same thing wherever it is typed.
STAGE_DETECT = "detect"
STAGE_LAYOUT = "layout"
STAGE_ENGINE = "engine"
STAGE_POST = "post"
# The served road's two of its own: ``feed`` puts the page where the serve
# process can read it, ``mokuro`` is the process itself -- the fork's own page
# pipeline, with one model on one device and a worker pool of its own
# (``--num_workers``), which is what the pools table shows it as. It is also
# what a MONOLITHIC row's one stage is called, for a mokuro package with no
# serve module: the same pipeline, reached by a command line instead of a pipe.
STAGE_FEED = "feed"
STAGE_MOKURO = "mokuro"

# THE STAGE GRAPHS. A composed engine gets pipelining by naming a road here
# and handing :func:`page_stages` one callable per stage; the scheduler
# (:class:`StagePipeline`) never looks at an engine id, so adding an engine
# or a detector is a line in this table, not a change to the scheduler. Nor
# does a stage's BACKEND reach the scheduler: a stage that moved to the GPU
# would change its ``device`` and its ``seconds`` here and nothing else.
#
# WHERE THE BOUNDARIES GO. The stage that holds the GPU holds ONLY the GPU work
# it can: everything before it that is CPU (boxing, the CTC read, the crops)
# and everything after it that is CPU (the layout, the raw dump) is its own
# stage with its own pool, so the card is not waiting on a
# thread doing numpy. That is why every road here has the engine stage in the
# MIDDLE -- a queue feeding it and a queue draining it -- rather than at the
# end where it was. A stage at the end has no output queue, so it can neither
# be blocked (which is the signal that the stage after it is too narrow) nor
# hand its device back when it runs ahead.
#
# What is NOT split, and why: detection and the CTC read stay one stage, though
# the CTC read is 2.4x the detector (105.1 ms against 43.4 ms a page). Two
# stages of one page each would cap the win at 1/0.61 = 1.65x and leave the
# detector idle 60% of the time; one stage running whole pages in parallel has
# no such ceiling, because a worker holds a ``PPOcr`` and a ``PPOcr`` is BOTH
# sessions -- K workers are K detector sessions and K CTC sessions at once.
# Measured end to end, that is 1.92x (manga) and 2.30x (novel prose).
#
# Nor are paddle-manga's two GPU batches split into two stages: which lines get
# the second read is decided by RECONCILING the first, so the second batch
# cannot be issued until the first has come back and been merged. They stay in
# one stage with the reconcile between them.
STAGE_GRAPHS: dict[str, tuple[StageSpec, ...]] = {
    # ppocr-manga alone: its own detector and CTC recognizer, then the layout.
    # No GPU stage at all, so this is two CPU pools and nothing to hide behind.
    ROAD_LINE: (
        StageSpec(STAGE_DETECT, "detect + CTC read", DEVICE_CPU, POOLED, 0.19),
        StageSpec(STAGE_LAYOUT, "layout + dump", DEVICE_CPU, POOLED, 0.011),
    ),
    # An engine reading the ppocr-manga detector's lines. detect boxes,
    # CTC-reads, joins and probes; engine crops, runs its batches and
    # reconciles the two reads; post lays the page out and writes the dump.
    ROAD_RECONCILED: (
        # 0.225 s a page on manga, 0.88 s on novel prose (measured over 40 and
        # 12 real pages). Manga is the figure to size on: see the note under
        # ENGINE_STAGE_SECONDS.
        StageSpec(STAGE_DETECT, "detect + CTC read", DEVICE_CPU, POOLED, 0.225),
        StageSpec(STAGE_ENGINE, "engine read + reconcile", DEVICE_GPU, DEVICE_BOUND, 0.30),
        StageSpec(STAGE_POST, "layout + dump", DEVICE_CPU, POOLED, 0.011),
    ),
    # A detector adapter behind its process boundary (detectors/README.md).
    # The ``detect`` stage OWNS one of those processes: it sends it this page,
    # waits for the answer and reads the detection back off disk with the
    # image -- so detection is a streaming stage like any other rather than a
    # whole-volume pass that finished before the recognizer was built. The
    # engine crops and recognizes; post assembles the blocks into the page.
    ROAD_ADAPTER: (
        StageSpec(STAGE_DETECT, "detect + read page", DEVICE_CPU, POOLED, 0.25),
        StageSpec(STAGE_ENGINE, "engine read", DEVICE_GPU, DEVICE_BOUND, 0.30),
        # 0.0004 s: this stage is dict assembly and nothing else -- 0.038 ms a
        # page through ctd, measured over 24 real
        # manga pages. It declared 0.02 s while it also PLACED CHARACTERS; with
        # that gone, 0.02 s would be 500x what the assembly costs. (The
        # reconciled road's post keeps 0.011 s because it really does lay the
        # page out and write the dump: 3.65 ms a page on the same 24 pages,
        # against 3.40 ms for the line road's identical `layout` stage, which
        # is declared at the same 0.011 s and is untouched here.)
        StageSpec(STAGE_POST, "assemble", DEVICE_CPU, POOLED, 0.0004),
    ),
    # An engine behind its own SERVE PROCESS (``SERVED_ENGINES``). The stage
    # boundaries are the pipe: ``feed`` puts the page's bytes where the
    # process can open them (a few ms of zip read and one write; when the
    # pages are already a directory on disk it hands the path straight on),
    # ``mokuro`` waits for that page's JSON, ``post`` keeps it.
    #
    # ``mokuro`` is DEVICE_BOUND at 1 because the process holds ONE model --
    # but a width of one here is not one page at a time: the stage does not
    # SEND the page, it waits for a page the ``feed`` stage already sent, and
    # the process is kept as full as its own ``window`` allows. So the queue
    # in front of it is the pages in flight INSIDE the engine, which is why it
    # is sized to that window and why its blocked%/starved% read the way the
    # other roads' do: full means the model is the bottleneck, empty means the
    # feed is.
    #
    # Its Workers cell is not a pool width either -- it is the fork's
    # ``--num_workers``, the engine's own CPU-side pipeline.
    ROAD_SERVED: (
        # 4 ms: read one member out of the .cbz and write it into the
        # workspace. A page already on disk costs nothing at all.
        StageSpec(STAGE_FEED, "spool the page", DEVICE_CPU, POOLED, 0.004),
        # 0.17 s a page: 24 real manga pages through mokuro (fp16) on an
        # RX 9070 XT in 4.1 s, the whole of it inside the process. It is what
        # the other two are sized against and the only figure here that moves
        # with the engine -- and being device-bound, it sets the pace rather
        # than buying itself workers.
        StageSpec(STAGE_MOKURO, "serve process", DEVICE_GPU, DEVICE_BOUND, 0.17),
        StageSpec(STAGE_POST, "assemble", DEVICE_CPU, POOLED, 0.0002),
    ),
}

# What a stage costs when that depends on the ENGINE rather than the road.
# Seconds a page on real manga pages, measured as the time a page spends IN
# THE STAGE (which is what sizes a pool) rather than time on the card:
# paddle-manga 0.915 s (2.035 s on novel prose -- it fires two batches a page,
# being the only engine with quad crops), hayai-nova 0.177 s (0.410 s on
# prose). The upstream detect+CTC stage they are sized against is 0.225 s
# (0.88 s on prose).
#
# The manga figure is the one to keep here: prose costs the ENGINE more than
# it costs the stage feeding it, so a width derived from manga covers prose
# too (measured: hayai-nova needs 3 on manga and 2 on prose).
#
# Sizing only -- a wrong number here costs cores, never correctness, and the
# startup log prints what it derived, the summary what it cost.
ENGINE_STAGE_SECONDS: dict[str, dict[str, float]] = {
    "paddle-manga": {STAGE_ENGINE: 0.915},
    "hayai-nova": {STAGE_ENGINE: 0.177},
}

# What a page costs in the ADAPTER road's detect stage: the adapter's own
# detection, plus reading its JSON back and decoding the page (8.1 ms of it,
# measured on 1080x1536 pages -- the 0.05 s the graph used to declare for the
# old decode-only stage was 6x high). Measured per detector on the live card;
# a detector not named here falls back to the graph's figure.
DETECTOR_STAGE_SECONDS: dict[str, float] = {}

# Detectors that take the card when the host has one. ``ppocr-manga``'s DBNet
# is onnxruntime on the CPU by design (see ``ppocr.py``); the others are torch
# or a CUDA execution provider. The adapter reports the device it really got
# when it comes up, and the runner logs a warning if the two disagree.
GPU_DETECTORS: frozenset[str] = frozenset({"ctd", "animetext"})

# Of those, the ones whose card is reached through an onnxruntime EXECUTION
# PROVIDER rather than torch. torch seeing a card says nothing about them: a
# CPU-only onnxruntime wheel (what pip installs by default) beside a ROCm or
# CUDA torch is an ordinary host, and on it such a detector can only run on the
# CPU. Asking it for ``--device cuda:0`` there is a startup failure
# (``detectors/animetext.py`` refuses rather than silently running elsewhere),
# so where such a model goes is decided by :func:`ort_gpu_providers`, not by
# :func:`host_has_gpu`. Every other onnxruntime model here (the ppocr pair) is
# CPU-only by design and needs no probe.
ORT_GPU_DETECTORS: frozenset[str] = frozenset({"animetext"})

# The onnxruntime execution providers that put a model on a card -- the ones
# ``detectors/animetext.py`` asks for, in its order.
ORT_GPU_PROVIDERS: tuple[str, ...] = (
    "CUDAExecutionProvider",
    "ROCMExecutionProvider",
    "MIGraphXExecutionProvider",
)

# Engines whose recognizer cannot leave the CPU. ``ppocr-manga``'s SVTR-CTC is
# onnxruntime, like its detector.
CPU_ONLY_ENGINES: frozenset[str] = frozenset({"ppocr-manga"})

# The stages that HOLD A MODEL, and are therefore the only ones a device may be
# chosen for. ``post``/``layout``/``feed`` are assembly, JSON and a file copy:
# CPU work with nothing to place. Read from here, never hardcoded at a call site
# -- a road that grows a stage changes this table and nothing else.
#
# ``mokuro`` is here for the served road: the model is in the serve process
# rather than in this one, but it is still ONE model on ONE device, and the
# device the row chooses is what that process is started with (``--force_cpu``
# / ``CUDA_VISIBLE_DEVICES``, :func:`mokuro_placement`). So the served road's
# device-bearing stages are ``mokuro`` and nothing else: ``detect``/``engine``
# are not stages of it at all and a row naming one is refused.
MODEL_STAGES: tuple[str, ...] = (STAGE_DETECT, STAGE_ENGINE, STAGE_MOKURO)


class MokuroPlacement(NamedTuple):
    """How a device id reaches a mokuro process, whichever path spawns it.

    ONE source of truth for the translation, because there are two callers and
    they must not drift: the runner starting ``python -m mokuro.serve`` for a
    served row (ADDENDUM 8), and the server running the one-volume ``python -m
    mokuro`` CLI for a package with no serve module
    (``OCRProcessor._mokuro_placement``). The same flags, the same variables,
    the same meaning either way -- which is the point of the row's Device
    select saying one thing whichever path the volume ends up taking.
    """

    force_cpu: bool
    env: dict[str, str]

    @property
    def flags(self) -> list[str]:
        """The placement as command-line flags (the CLI path appends these)."""
        return ["--force_cpu"] if self.force_cpu else []


def mokuro_placement(device: str) -> MokuroPlacement:
    """A device id as the mokuro process takes it (ADDENDUM 7's MONOLITHIC bullet).

    * ``cpu`` -> ``--force_cpu``;
    * ``gpu:<n>`` -> ``CUDA_VISIBLE_DEVICES``/``HIP_VISIBLE_DEVICES`` in that
      process only. mokuro has no device index of its own, so the card is
      chosen by hiding the others -- and BOTH variables are set because which
      one a torch build reads depends on whether it is a CUDA or a ROCm build,
      and setting the other is harmless;
    * ``auto`` (or nothing at all) -> nothing: the fork picks, as it always has.
    """
    asked = str(device or "")
    if asked == DEVICE_CPU:
        return MokuroPlacement(force_cpu=True, env={})
    if device_is_gpu(asked):
        index = str(device_index(asked))
        return MokuroPlacement(
            force_cpu=False,
            env={"CUDA_VISIBLE_DEVICES": index, "HIP_VISIBLE_DEVICES": index},
        )
    return MokuroPlacement(force_cpu=False, env={})


def model_stages(road: str) -> tuple[str, ...]:
    """The stages of this road a device may be chosen for, in road order."""
    return tuple(spec.key for spec in STAGE_GRAPHS[road] if spec.key in MODEL_STAGES)


def stage_is_cpu_only(road: str, key: str, *, engine: str = "", detector: str = "") -> bool:
    """Whether this stage's model can only ever run on the CPU.

    The ppocr pair is onnxruntime both halves, so every stage that holds it is
    CPU-only: the line road's ``detect`` (which is also the CTC read), the
    reconciled road's ``detect``, and the ``ppocr-manga`` adapter. A stage with
    no model at all answers False -- it is not a device choice either way.
    """
    if key == STAGE_ENGINE:
        return engine in CPU_ONLY_ENGINES
    if key != STAGE_DETECT:
        return False
    if road in (ROAD_LINE, ROAD_RECONCILED):
        return True
    return detector not in GPU_DETECTORS


def stage_needs_ort_gpu(road: str, key: str, *, detector: str = "", engine: str = "") -> bool:
    """Whether this stage's model reaches a card only through onnxruntime.

    Only the adapter road's ``detect`` with an :data:`ORT_GPU_DETECTORS`
    detector today. ``engine`` is accepted so a recognizer that joins them is
    one table away, not a new signature.
    """
    del engine
    return road == ROAD_ADAPTER and key == STAGE_DETECT and detector in ORT_GPU_DETECTORS


@functools.lru_cache(maxsize=1)
def ort_gpu_providers() -> tuple[str, ...] | None:
    """The GPU execution providers THIS environment's onnxruntime offers.

    ``()`` is an answer -- a CPU-only wheel -- and puts every
    :func:`stage_needs_ort_gpu` stage on the CPU. ``None`` is no answer (no
    onnxruntime importable here at all): nothing is decided on it, and the
    detector that needs it fails on its own terms.
    """
    try:
        import onnxruntime
    except Exception:
        return None
    try:
        offered = list(onnxruntime.get_available_providers())
    except Exception:
        return None
    return tuple(p for p in ORT_GPU_PROVIDERS if p in offered)


@functools.lru_cache(maxsize=1)
def _gpu_count() -> int:
    """How many cards this environment can see; 0 without torch or a card."""
    try:
        import torch

        return int(torch.cuda.device_count()) if torch.cuda.is_available() else 0
    except Exception:
        return 0


@functools.lru_cache(maxsize=1)
def host_has_gpu() -> bool:
    """Does this host have a card for a model to sit on? Asked once.

    No torch, no card: this is asked by the PLANNER, which runs wherever the
    runner is imported (the tests, a host without the engines environment),
    and a missing import there means "nothing of ours is on a GPU".
    """
    try:
        return _pick_device() == "cuda"
    except Exception:
        return False


def road_specs(
    road: str,
    *,
    detector: str = "",
    engine: str = "",
    gpu: bool | None = None,
    devices: Mapping[str, str] | None = None,
    ort_gpu: bool | None = None,
) -> tuple[StageSpec, ...]:
    """A road's declared stages, with what THIS run puts on each one resolved.

    The graph above is what a road is; where its MODEL-BEARING stages sit is
    the row's to say (``pools.stage_device``, Addendum 7): ``detect``,
    ``engine`` and the served road's ``mokuro`` each carry a device id, and an
    absent key is ``auto`` -- card 0 where there is one, else the CPU, which is
    what this function did on its own before there was anything to ask. A model
    that cannot leave the CPU (the ppocr pair) resolves to the CPU whatever is
    asked; the refusal of a GPU for it belongs to whoever validated the edit.

    On the SERVED road the model is not in this process at all, so ``auto``
    here is a placeholder: the caller passes ``gpu=False`` rather than paying
    an ``import torch`` to guess, and :meth:`OpenPipeline._fit_to_window`
    replaces it with the device the engine says on its ready line. An EXPLICIT
    device is not a guess and is kept as asked -- it is what the process was
    started with.

    A stage on the GPU is never pooled wide by derivation
    (:func:`plan_stage_workers`): a second process is a second HIP context and
    a second copy of the model in VRAM, which is the user's decision, not the
    planner's. Which is also why the choice pays: a detector moved to the CPU
    becomes poolable, and leaves the whole card to the engine.

    ``ort_gpu`` is whether the host's onnxruntime can reach a card (see
    :func:`ort_gpu_providers`). False makes every :func:`stage_needs_ort_gpu`
    stage CPU-only HERE, by the same rule as the ppocr pair -- ``auto`` and a
    pin alike resolve to the CPU, and ``ready`` says so. None decides nothing:
    the caller that could not ask does not get to guess.

    Every other stage is what the graph declares. ``post``/``layout`` are
    layout, assembly and JSON: CPU work everywhere, with no device to choose.
    """
    specs = STAGE_GRAPHS[road]
    has_gpu = host_has_gpu() if gpu is None else gpu
    asked = dict(devices or {})
    out: list[StageSpec] = []
    for spec in specs:
        if spec.key not in MODEL_STAGES:
            out.append(spec)
            continue
        device = resolve_device(
            asked.get(spec.key, DEVICE_AUTO),
            gpu=has_gpu,
            cpu_only=stage_is_cpu_only(road, spec.key, engine=engine, detector=detector)
            or (
                ort_gpu is False
                and stage_needs_ort_gpu(road, spec.key, detector=detector, engine=engine)
            ),
        )
        seconds = (
            DETECTOR_STAGE_SECONDS.get(detector, spec.seconds)
            if road == ROAD_ADAPTER and spec.key == STAGE_DETECT
            else spec.seconds
        )
        out.append(spec._replace(device=device, seconds=seconds))
    return tuple(out)


def stage_seconds(engine: str, spec: StageSpec) -> float:
    """What a page costs in this stage: the engine's figure, else the road's."""
    return ENGINE_STAGE_SECONDS.get(engine, {}).get(spec.key, spec.seconds)


def page_road(engine: str, detector: str) -> str:
    """Which page pipeline an engine/detector pair takes.

    Not a taxonomy of engines: the same recognizer takes the reconciled road
    behind a layout detector and the adapter road behind any other.
    """
    if engine in SERVED_ENGINES:
        return ROAD_SERVED
    if engine in LINE_ENGINES:
        return ROAD_LINE
    if detector in LAYOUT_DETECTORS:
        return ROAD_RECONCILED
    return ROAD_ADAPTER


def plan_stage_workers(
    engine: str, specs: Sequence[StageSpec], *, budget: int, cap: int | None = CPU_WORKERS_MAX
) -> list[int]:
    """How wide to run each stage, sized against the stage that sets the pace.

    Each stage is sized RELATIVE to its neighbours, not out of one global
    number: a stage costing ``c`` seconds a page needs ``ceil(c / pace)``
    workers to keep up with a pipeline whose period is ``pace``, so the CTC
    stage costing 2.4x the detector asks for 2.4x the workers, whatever the
    host is. ``budget`` and ``cap`` only clamp the answer afterwards; they do
    not decide the ratio.

    The pace is:

    * the cost of the DEVICE-BOUND stage where there is one (the GPU model can
      only go so fast, so nothing upstream of it needs to go faster);
    * otherwise the most expensive pooled stage running at its own ceiling --
      it IS the pipeline, and everything else is sized against it. ppocr-manga
      is this case: 0% GPU, so detect takes the whole ceiling and the layout,
      at a hundredth of its cost, takes one.

    Two things sit on top of the ratio:

    * :data:`STAGE_WIDTH_HEADROOM`, because a stage sized on its MEAN page
      stops producing on every page above the mean;
    * a stage on the GPU is never widened by derivation. Its width is one,
      whatever it costs, because a second worker there is a second copy of a
      model in VRAM -- a decision for whoever knows the card, taken with
      ``--stage-workers detect=2``, not for a cost ratio.

    ``budget`` of 0 is the serial fallback and fuses every stage onto the
    caller's thread.
    """
    if budget <= 0:
        return [SERIAL for _ in specs]
    costs = [stage_seconds(engine, spec) for spec in specs]
    ceilings = [
        max(1, min(budget, cap if cap is not None else budget, spec.max_workers or budget))
        if spec.max_workers != DEVICE_BOUND
        else DEVICE_BOUND
        for spec in specs
    ]
    bound = max(
        (c for spec, c in zip(specs, costs, strict=True) if spec.max_workers == DEVICE_BOUND),
        default=0.0,
    )
    pooled = [
        (i, c)
        for i, (spec, c) in enumerate(zip(specs, costs, strict=True))
        if spec.max_workers != DEVICE_BOUND
    ]
    if bound > 0:
        pace = bound
    else:
        # The leading pooled stage at its own ceiling sets the period.
        lead = max((c for _, c in pooled), default=0.0)
        lead_ceiling = max(
            (ceilings[i] for i, c in pooled if c == lead),
            default=1,
        )
        pace = lead / lead_ceiling if lead > 0 and lead_ceiling > 0 else 0.0
    plan = list(ceilings)
    for index, spec in enumerate(specs):
        if spec.max_workers == DEVICE_BOUND or device_is_gpu(spec.device):
            # One model on one device, derived. Only an explicit width, and
            # only where ``max_workers`` allows it, puts a second one there.
            plan[index] = min(1, ceilings[index]) or 1
            continue
        need = (
            math.ceil(STAGE_WIDTH_HEADROOM * costs[index] / pace) if pace > 0 else ceilings[index]
        )
        plan[index] = max(1, min(need, ceilings[index]))
    return plan


class Stage(NamedTuple):
    """A declared stage bound to its callable, its pool width and its queue."""

    spec: StageSpec
    run: Callable[[Any, Any], Any]
    workers: int
    # Capacity of the queue this stage FILLS -- its output queue, and the input
    # queue of whatever comes next. The last stage's is the sink the driver
    # drains. Capacity is memory: an item here can hold a decoded page.
    capacity: int = 1


def stage_widths(
    engine: str,
    road: str,
    *,
    budget: int,
    forced: int | None = None,
    workers: Mapping[str, int] | None = None,
    specs: Sequence[StageSpec] | None = None,
) -> list[int]:
    """How wide each of a road's stages runs for this engine.

    Derived per stage from what that stage costs (:func:`plan_stage_workers`),
    then overridden by anything the user said. An EXPLICIT width always wins
    over the derivation, the host budget and the measured plateau, because
    tuning by hand is the point of it -- in order of precedence:

    1. ``workers[<stage key>]`` -- ``--stage-workers detect=4``
    2. ``workers["*"]`` or ``forced`` -- ``--cpu-workers 4``, every stage alike
    3. the derivation

    A stage's own ``max_workers`` still holds against all three: that is a
    structural limit (one model on one device; one page's image a worker), not
    a tuning number.

    "EVERY STAGE ALIKE" MEANS EVERY CPU STAGE. ``--cpu-workers 4`` has always
    been a number of CPU workers, and a stage that holds a model on the GPU is
    not one: four there is four HIP contexts and four copies of the model in
    VRAM, asked for by somebody who was talking about cores. Such a stage
    keeps its derived width unless it is NAMED (``--stage-workers detect=2``),
    which is unambiguous. ``0`` is the exception and means what it always did:
    the serial fallback, every stage on the caller's thread.

    ``specs`` is this run's resolved graph (:func:`road_specs`); without it
    the road's declared one is used, which is what a caller asking about a
    road in the abstract means.
    """
    specs = STAGE_GRAPHS[road] if specs is None else tuple(specs)
    overrides = dict(workers or {})
    everywhere = overrides.pop(ALL_STAGES, forced)
    if everywhere is None:
        widths = plan_stage_workers(engine, specs, budget=budget)
    elif everywhere <= 0:
        widths = [SERIAL for _ in specs]
    else:
        derived = plan_stage_workers(engine, specs, budget=budget)
        widths = [
            got if device_is_gpu(spec.device) else everywhere
            for spec, got in zip(specs, derived, strict=True)
        ]
    out = []
    for spec, width in zip(specs, widths, strict=True):
        want = overrides.get(spec.key, width)
        ceiling = spec.max_workers
        out.append(max(0, want) if ceiling is None else min(max(0, want), ceiling))
    if road == ROAD_SERVED:
        # NO SERIAL FALLBACK HERE, whoever asks for it. A served engine may
        # hold a whole window of pages before it answers any of them (its
        # last OCR batch waits for the end of the volume), so a pipeline that
        # walks one page at a time through it would be waiting for a page it
        # has not been allowed to send. A pool of one is not the serial path
        # and is enough: the stage that waits is not the stage that sends.
        out = [max(1, width) for width in out]
    return out


def stage_capacities(
    road: str,
    widths: Sequence[int],
    *,
    capacities: Mapping[str, int] | None = None,
    specs: Sequence[StageSpec] | None = None,
) -> list[int]:
    """How deep the queue each stage fills may get, in pages.

    Default: one slot per worker of the stage that fills it, so a stage's pool
    can always hand off what it has finished and start the next page without
    running the volume into memory. An item can hold a decoded page image
    (~14 MB), so this is the knob that decides what a run costs in RAM --
    ``--queue-capacity post=8``, or a bare number for every queue.

    The queue FEEDING a device-bound stage gets :data:`LOAD_WINDOW_SLOTS`
    instead, because that stage's model is still loading when the volume
    starts (:class:`DeferredRecognizer`) and the stage before it should spend
    that window filling the queue rather than blocked on a slot. Four pages of
    runway at ~14 MB a page is ~56 MB, which is the whole cost of the change.
    """
    specs = STAGE_GRAPHS[road] if specs is None else tuple(specs)
    asked = dict(capacities or {})
    everywhere = asked.get(ALL_STAGES)
    out = []
    for index, (spec, width) in enumerate(zip(specs, widths, strict=True)):
        default = max(1, width * QUEUE_CAPACITY_PER_WORKER)
        drain = specs[index + 1] if index + 1 < len(specs) else None
        if drain is not None and drain.max_workers == DEVICE_BOUND:
            default = max(default, LOAD_WINDOW_SLOTS)
        want = asked.get(spec.key, everywhere if everywhere is not None else default)
        out.append(max(1, want))
    return out


def page_stages(
    road: str,
    runs: Sequence[Callable[[Any, Any], Any]],
    *,
    engine: str,
    budget: int,
    forced: int | None = None,
    workers: Mapping[str, int] | None = None,
    capacities: Mapping[str, int] | None = None,
    specs: Sequence[StageSpec] | None = None,
) -> list[Stage]:
    """Bind a road's declared graph to the callables that do its work.

    One callable per declared stage, in order, each taking ``(item, payload
    from the stage before)``; widths come from :func:`stage_widths` and queue
    capacities from :func:`stage_capacities`.
    """
    specs = STAGE_GRAPHS[road] if specs is None else tuple(specs)
    if len(specs) != len(runs):
        raise RuntimeError(
            f"road {road!r} declares {len(specs)} stages but was given {len(runs)} callables"
        )
    widths = stage_widths(engine, road, budget=budget, forced=forced, workers=workers, specs=specs)
    caps = stage_capacities(road, widths, capacities=capacities, specs=specs)
    return [
        Stage(spec, run, width, cap)
        for spec, run, width, cap in zip(specs, runs, widths, caps, strict=True)
    ]


class Outcome(NamedTuple):
    """What one stage made of one page: a value, or the failure that replaced it.

    Carried instead of letting the exception fly, because the stages run on
    other threads: a raise there would end the whole volume, and a page that
    fails must only cost that page (see ``blank_page``).
    """

    value: Any
    error: BaseException | None

    def unwrap(self) -> Any:
        """The value, or re-raise what the stage raised, traceback and all."""
        if self.error is not None:
            raise self.error
        return self.value


# ---------------------------------------------------------------------------
# THE SCHEDULER. One volume, one pipeline; one POOL a stage, sized on its own;
# a BOUNDED QUEUE between every pair of stages, including both sides of the
# stage that holds the GPU.
#
# The mechanism and the diagnostic are the same thing. A stage's workers take
# from their input queue and put into their output queue, and both operations
# BLOCK on a condition variable -- they never spin, so a worker that is waiting
# is off the CPU and a GPU-side producer that has run ahead hands the card back
# to whoever is behind it. Which means the queues answer "where is the time
# going?" by themselves:
#
#   the queue in front of a stage sits EMPTY   -> that stage is STARVED; widen
#                                                 the stage that fills it
#   the queue behind a stage sits FULL         -> that stage is BLOCKED; widen
#                                                 the stage that drains it
#
# Both are counted in seconds, per queue, alongside time-weighted mean depth
# and the high-water mark (:class:`StageQueue`), and printed per volume and
# published live (:meth:`StagePipeline.snapshot`) so a person -- or the server
# polling the run -- can read the bottleneck off the numbers instead of
# guessing it from a model. That matters because the model HAS been wrong here:
# paddle-manga was sized as GPU-bound (0.29 s of CPU hiding inside 0.82 s of
# GPU, therefore one CPU worker), and a 1 Hz rocm-smi trace of a real volume
# came back 99 5 99 99 4 16 98 19 99 95 97 4 -- the card idle in a third of the
# samples. Nothing here decides who is right; it makes it measurable.
#
# Order. The stages are per page and share nothing, so pages may finish out of
# order inside the pipeline; the DRIVER reassembles input order at the sink
# with a sequence number. The window is bounded by what can be in flight
# (workers plus queue capacity), and the items it holds are finished page
# dicts, not images, so the reassembly is not where memory goes.
#
# Failure. A page that raises in any stage carries its :class:`Outcome` through
# the remaining stages untouched and surfaces at its own place in the driver's
# loop, where ``blank_page()`` has always caught it.
# ---------------------------------------------------------------------------

# What a queue hands a consumer when there is nothing more coming.
_SENTINEL = object()


class QueueReport(NamedTuple):
    """One queue's story over a run. Seconds and depths, no interpretation.

    Every field but ``mean_depth`` and ``depth`` is a CUMULATIVE COUNTER from
    the start of the pipeline, so two snapshots subtract: a pipeline that
    stays open across several volumes can report each volume as the delta
    between the snapshot taken when it started and the one taken when its
    last page came out. ``depth_seconds`` is the raw integral ``mean_depth``
    is derived from, and is here so that the mean can be differenced too --
    a ratio cannot.
    """

    name: str
    capacity: int
    depth: int
    max_depth: int
    mean_depth: float
    depth_seconds: float
    puts: int
    gets: int
    blocked_seconds: float
    blocked_events: int
    starved_seconds: float
    starved_events: int

    @property
    def fill(self) -> float:
        """Mean depth as a fraction of capacity: 1.0 is a queue that is always full."""
        return self.mean_depth / self.capacity if self.capacity > 0 else 0.0

    def since(self, earlier: QueueReport, elapsed: float) -> QueueReport:
        """What this queue did SINCE ``earlier``, over ``elapsed`` seconds.

        Every counter differences. ``mean_depth`` is re-derived from the
        differenced integral rather than subtracted, because a ratio cannot
        be; ``depth`` and ``max_depth`` are the live/high-water values and
        stay as they are (a window's high-water mark is not knowable from two
        snapshots, and claiming otherwise would understate it).
        """
        depth_seconds = self.depth_seconds - earlier.depth_seconds
        return self._replace(
            mean_depth=depth_seconds / elapsed if elapsed > 0 else 0.0,
            depth_seconds=depth_seconds,
            puts=self.puts - earlier.puts,
            gets=self.gets - earlier.gets,
            blocked_seconds=self.blocked_seconds - earlier.blocked_seconds,
            blocked_events=self.blocked_events - earlier.blocked_events,
            starved_seconds=self.starved_seconds - earlier.starved_seconds,
            starved_events=self.starved_events - earlier.starved_events,
        )

    def as_dict(self) -> dict[str, Any]:
        return {**self._asdict(), "fill": round(self.fill, 3)}


class StageQueue:
    """A bounded hand-off between two stage pools, instrumented.

    Capacity is memory: an item here can hold a decoded page image (~14 MB for
    a 1700x2800 scan), so the depth this allows is what keeps a 292-page volume
    flat. It is a knob (``--queue-capacity``), defaulting to the width of the
    stage that fills it.

    Blocking is real blocking. ``put`` into a full queue waits on a condition
    until a consumer takes something; ``get`` from an empty one waits until a
    producer puts. Neither polls, so a parked worker holds no core and no
    stream -- which is the point on the GPU side, where a recognizer thread
    that has run ahead of the post stage must give the FLOPs back rather than
    spin on them.

    Both waits are timed and counted, and every depth change integrates depth
    over wall time, so ``report()`` can say how full this ran and how long each
    side spent waiting on the other.
    """

    def __init__(self, name: str, capacity: int) -> None:
        if capacity < 1:
            raise ValueError(f"queue {name!r} needs a capacity of at least 1, got {capacity}")
        self.name = name
        self.capacity = int(capacity)
        self._items: deque[Any] = deque()
        self._lock = threading.Lock()
        # Two conditions on one lock: a producer waiting for room never wakes
        # on a producer's put, and a consumer waiting for work never wakes on
        # a consumer's get.
        self._room = threading.Condition(self._lock)
        self._work = threading.Condition(self._lock)
        self._finished = False  # nothing more will be put
        self._closed = False  # torn down; drop what is held and wake everyone
        self.max_depth = 0
        self.puts = 0
        self.gets = 0
        self.blocked_seconds = 0.0
        self.blocked_events = 0
        self.starved_seconds = 0.0
        self.starved_events = 0
        self._depth_seconds = 0.0
        self._last_change = time.monotonic()

    def _accrue(self) -> None:
        """Integrate the depth held since the last change. Call with the lock."""
        now = time.monotonic()
        self._depth_seconds += len(self._items) * (now - self._last_change)
        self._last_change = now

    def put(self, item: Any) -> bool:
        """Hand an item on, waiting for room. False once the queue is closed."""
        with self._lock:
            if len(self._items) >= self.capacity and not self._closed:
                started = time.monotonic()
                while len(self._items) >= self.capacity and not self._closed:
                    self._room.wait()
                self.blocked_seconds += time.monotonic() - started
                self.blocked_events += 1
            if self._closed:
                return False
            self._accrue()
            self._items.append(item)
            self.puts += 1
            self.max_depth = max(self.max_depth, len(self._items))
            self._work.notify()
            return True

    def get(self) -> Any:
        """The next item, waiting for one. :data:`_SENTINEL` when there are no more."""
        with self._lock:
            if not self._items and not self._finished and not self._closed:
                started = time.monotonic()
                while not self._items and not self._finished and not self._closed:
                    self._work.wait()
                self.starved_seconds += time.monotonic() - started
                self.starved_events += 1
            if self._closed or not self._items:
                return _SENTINEL
            self._accrue()
            item = self._items.popleft()
            self.gets += 1
            self._room.notify()
            return item

    def finish(self) -> None:
        """No more items will be put; waiting consumers may stop."""
        with self._lock:
            self._finished = True
            self._work.notify_all()

    def close(self) -> None:
        """Tear down: drop what is held and wake every waiter to leave."""
        with self._lock:
            self._closed = True
            self._accrue()
            self._items.clear()
            self._room.notify_all()
            self._work.notify_all()

    @property
    def depth(self) -> int:
        with self._lock:
            return len(self._items)

    def report(self, elapsed: float) -> QueueReport:
        with self._lock:
            self._accrue()
            depth_seconds = self._depth_seconds
            depth = len(self._items)
            return QueueReport(
                name=self.name,
                capacity=self.capacity,
                depth=depth,
                max_depth=self.max_depth,
                mean_depth=depth_seconds / elapsed if elapsed > 0 else 0.0,
                depth_seconds=depth_seconds,
                puts=self.puts,
                gets=self.gets,
                blocked_seconds=self.blocked_seconds,
                blocked_events=self.blocked_events,
                starved_seconds=self.starved_seconds,
                starved_events=self.starved_events,
            )


class StageReport(NamedTuple):
    """One stage's story over a run: how wide, how busy, how much it waited."""

    key: str
    name: str
    device: str
    workers: int
    items: int
    busy_seconds: float
    # From this stage's own queues: how long its workers spent unable to hand
    # work on, and how long they spent with nothing to take.
    blocked_seconds: float
    starved_seconds: float
    utilisation: float  # busy / (workers * elapsed): 1.0 is a saturated pool
    # One model on one device: this stage CANNOT be widened, so a readout must
    # never answer "widen it" (see ``pipeline_verdict``).
    device_bound: bool = False

    def since(self, earlier: StageReport, elapsed: float) -> StageReport:
        """What this stage did SINCE ``earlier``, over ``elapsed`` seconds."""
        busy = self.busy_seconds - earlier.busy_seconds
        workers = max(1, self.workers)
        return self._replace(
            items=self.items - earlier.items,
            busy_seconds=busy,
            blocked_seconds=self.blocked_seconds - earlier.blocked_seconds,
            starved_seconds=self.starved_seconds - earlier.starved_seconds,
            utilisation=busy / (workers * elapsed) if elapsed > 0 else 0.0,
        )

    def as_dict(self) -> dict[str, Any]:
        return {**self._asdict(), "utilisation": round(self.utilisation, 3)}


class PipelineReport(NamedTuple):
    """Everything a run's queues and pools measured, and what it points at."""

    elapsed: float
    items: int
    stages: tuple[StageReport, ...]
    queues: tuple[QueueReport, ...]

    def bottleneck(self) -> StageReport | None:
        """The stage nothing is waiting for: the busiest pool, per worker.

        Utilisation is what the queues on either side of a stage agree on -- a
        stage that is neither starved nor blocked is working, and the one
        working hardest is the one setting the pipeline's period. Ties and near
        ties are real, so the LINES are the evidence and this is the pointer.
        """
        ran = [stage for stage in self.stages if stage.items]
        return max(ran, key=lambda s: s.utilisation) if ran else None

    def since(self, earlier: PipelineReport | None) -> PipelineReport:
        """This report minus an earlier one: one volume's share of a session.

        A pipeline that stays open across volumes counts CUMULATIVELY, so a
        volume's numbers are the difference between the snapshot taken when
        the volume before it finished and the one taken when it did. The
        windows tile the session: every second and every page belongs to
        exactly one of them, and summing them gives the session back.

        The stages and queues must line up -- it is the same pipeline -- so a
        snapshot from a DIFFERENT pipeline (``--bench`` rebuilding its pools)
        is not an earlier one and is refused by returning ``self``.
        """
        if earlier is None:
            return self
        if [s.key for s in self.stages] != [s.key for s in earlier.stages] or [
            q.name for q in self.queues
        ] != [q.name for q in earlier.queues]:
            return self
        elapsed = max(0.0, self.elapsed - earlier.elapsed)
        return PipelineReport(
            elapsed=elapsed,
            items=self.items - earlier.items,
            stages=tuple(
                now.since(was, elapsed) for now, was in zip(self.stages, earlier.stages, strict=True)
            ),
            queues=tuple(
                now.since(was, elapsed) for now, was in zip(self.queues, earlier.queues, strict=True)
            ),
        )

    def lines(self) -> list[str]:
        """The summary, one line a stage and one a queue, plus the verdict."""
        out = [
            f"pipeline over {self.items} page(s) in {self.elapsed:.1f}s "
            f"({self.elapsed / self.items:.2f}s a page)"
            if self.items
            else f"pipeline ran {self.elapsed:.1f}s over no pages"
        ]
        for stage in self.stages:
            # Width 0 is a stage with no pool of its own: it ran on the thread
            # of the stage before it (or, when every stage is 0, on the
            # caller's). Saying "x0" would read as "it did not run".
            width = f"x{stage.workers}" if stage.workers else "fused"
            share = "of its pool" if stage.workers else "of one thread"
            out.append(
                f"  stage {stage.key:<7} {stage.device} {width:<5} "
                f"busy {stage.busy_seconds:7.1f}s ({stage.utilisation * 100:5.1f}% {share}) "
                f"starved {stage.starved_seconds:7.1f}s blocked {stage.blocked_seconds:7.1f}s "
                f"[{stage.name}]"
            )
        for q in self.queues:
            out.append(
                f"  queue {q.name:<16} cap {q.capacity} "
                f"mean depth {q.mean_depth:5.2f} ({q.fill * 100:5.1f}% full) max {q.max_depth} "
                f"producers blocked {q.blocked_seconds:7.1f}s / "
                f"consumers starved {q.starved_seconds:7.1f}s"
            )
        worst = self.bottleneck()
        if worst is not None and not self.queues:
            out.append(
                f"  bottleneck: {worst.key} ({worst.utilisation * 100:.0f}% of the one "
                f"thread) -- this run was the serial fallback; give the stages pools "
                f"to overlap them"
            )
        elif worst is not None:
            out.append(
                f"  bottleneck: {worst.key} ({worst.utilisation * 100:.0f}% of a pool of "
                f"{worst.workers}) -- widen it to go faster, or narrow the stages "
                f"waiting on it to free cores"
            )
        return out

    def as_dict(self) -> dict[str, Any]:
        worst = self.bottleneck()
        return {
            "elapsed_seconds": round(self.elapsed, 3),
            "items": self.items,
            "stages": [stage.as_dict() for stage in self.stages],
            "queues": [q.as_dict() for q in self.queues],
            "bottleneck": worst.key if worst is not None else None,
        }


# ---------------------------------------------------------------------------
# READING THE NUMBERS. What the pools and queues measured, compacted to one row
# a stage, and the ONE thing it points at.
#
# This lives HERE, in the runner, and the server's ``ocr/pipeline_stats.py``
# re-exports it, because both sides need the same reading and there must be
# exactly one of it: ``--bench`` follows the verdict to decide which stage to
# widen next, and the queue page and the congestion history show the same
# sentence for the same numbers. A second copy would drift the moment either
# side was tuned. (The import only goes one way: the server imports the
# runner, never the reverse -- the runner is executed by path from the
# engines environment and has no package around it.)
# ---------------------------------------------------------------------------

# No verdict before this many pages have come out of the pipeline.
#
# A pipeline's FILL is all starvation by construction -- every stage but the
# first is waiting for its feeder to produce anything at all -- so a verdict
# read during it names the wrong stage every time. What ends the fill is
# PAGES, not seconds: it lasts one page per stage per worker, and after that
# the counters are steady state. Eight is comfortably past the fill of every
# graph here (at most three stages, at most four wide) and makes the readout
# work on a fast run and on a short volume, which a wall-clock gate did not:
# a 20-second floor silenced two correctly-diagnosed starved runs (7.2 s and
# 14.5 s) that had already read 40 and 100 pages.
MIN_PAGES = 8

# A stage waiting this share of its pool's time on a neighbour is a signal.
# Below it, the wait is the ordinary slack of a pipeline whose stages do not
# divide evenly into each other.
WAIT_PCT = 15.0

# A stage this busy with nobody waiting on it is the pipeline's period.
BUSY_PCT = 85.0


def summarize(raw: Any) -> dict[str, Any] | None:
    """One row a stage, plus the bottleneck and the verdict. None if unreadable.

    ``raw`` is whatever was in the file: it is parsed defensively, because a
    newer or older runner may have written it and a reader of a diagnostic
    file must not assume a schema it did not verify.
    """
    if not isinstance(raw, dict):
        return None
    stages_raw = raw.get("stages")
    if not isinstance(stages_raw, list) or not stages_raw:
        return None

    elapsed = _number(raw.get("elapsed_seconds"))
    queues = {
        q["name"]: q
        for q in raw.get("queues", [])
        if isinstance(q, dict) and isinstance(q.get("name"), str)
    }

    stages: list[dict[str, Any]] = []
    for entry in stages_raw:
        row = _stage_row(entry, queues, elapsed)
        if row is not None:
            stages.append(row)
    if not stages:
        return None

    keys = {stage["key"] for stage in stages}
    bottleneck = raw.get("bottleneck")
    summary = {
        "elapsed_seconds": round(elapsed, 1),
        "items": int(_number(raw.get("items"))),
        "stages": stages,
        # The runner's own pick, kept only when it names a stage we are
        # showing -- a pointer the page can highlight, never a claim on its
        # own. The rows underneath it are the evidence.
        "bottleneck": bottleneck if isinstance(bottleneck, str) and bottleneck in keys else None,
    }
    summary["verdict"] = pipeline_verdict(summary)
    return summary


def _stage_row(
    entry: Any, queues: dict[str, dict[str, Any]], elapsed: float
) -> dict[str, Any] | None:
    """One stage as the page reads it, or None when the entry is unusable."""
    if not isinstance(entry, dict):
        return None
    key = entry.get("key")
    if not isinstance(key, str) or not key:
        return None
    workers = int(_number(entry.get("workers")))

    # A stage of width 0 has no pool of its own: it was FUSED onto the stage
    # before it and runs on that stage's worker, sharing its queues. Its busy
    # time is its own (every stage meters itself), but its waits are the
    # leader's, and reporting them twice would read as two stages waiting.
    fused = workers <= 0
    pool_seconds = max(workers, 1) * elapsed
    outbound = _outbound_queue(queues, key)

    return {
        "key": key,
        "name": entry.get("name") if isinstance(entry.get("name"), str) else key,
        "device": entry.get("device") if isinstance(entry.get("device"), str) else "cpu",
        "workers": workers,
        "fused": fused,
        # One model on one device. The runner says so; a reader that cannot
        # see the flag assumes not, which only costs it a silence.
        "device_bound": bool(entry.get("device_bound")),
        "items": int(_number(entry.get("items"))),
        "busy_pct": _pct(entry.get("busy_seconds"), pool_seconds),
        "blocked_pct": None if fused else _pct(entry.get("blocked_seconds"), pool_seconds),
        "starved_pct": None if fused else _pct(entry.get("starved_seconds"), pool_seconds),
        "queue": _queue_row(outbound),
    }


def _outbound_queue(queues: dict[str, dict[str, Any]], key: str) -> dict[str, Any] | None:
    """The queue this stage FILLS, by the runner's ``<stage>-><next>`` naming.

    The stage a queue is named for is the one that puts into it, so the queue
    beside a stage is the one its own backpressure lives in: full means this
    stage is blocked, empty means the stage after it is starved. A fused stage
    names no queue (its leader does), and so gets none.
    """
    prefix = f"{key}->"
    for name, row in queues.items():
        if name.startswith(prefix):
            return row
    return None


def _queue_row(queue: dict[str, Any] | None) -> dict[str, Any] | None:
    if queue is None:
        return None
    capacity = int(_number(queue.get("capacity")))
    mean_depth = _number(queue.get("mean_depth"))
    return {
        "name": str(queue.get("name", "")),
        "capacity": capacity,
        "mean_depth": round(mean_depth, 2),
        "max_depth": int(_number(queue.get("max_depth"))),
        "fill_pct": round(100.0 * mean_depth / capacity, 1) if capacity > 0 else 0.0,
    }


def pipeline_verdict(summary: dict[str, Any]) -> str | None:
    """One line naming one thing to do, or None when nothing is supported.

    The reading, in the order it is tried:

    1. A stage **starved** on its input queue is waiting for the stage that
       FILLS it: widen that one. (Not the first stage -- what feeds it is the
       volume itself, and no pool widens that.)
    2. A stage **blocked** on its output queue is waiting for the stage that
       DRAINS it: widen that one. This is what a GPU stage running ahead of
       post-processing looks like, and the widening is what gives the card
       its FLOPs back. (Not the last stage -- what drains it is the driver
       writing the sidecar, which is not a stage.)
    3. Nobody widenable waiting, one stage saturated: that stage sets the
       period -- and when it is the one holding the card, THAT IS THE ANSWER
       rather than a fault. It is stated as a fact, with no knob attached,
       because there is no knob: one model on one device.

    **Waiting propagates, so the biggest number is not the culprit.** A slow
    detector starves the engine, and the engine then starves post *harder*
    than it is itself starved -- post is simply last in a queue of waiting.
    So starvation is read from the SOURCE end (the most upstream starved
    stage, whose own feeder is not also waiting) and blocking from the SINK
    end (the most downstream blocked stage, whose own drain is not also
    waiting). Picking the largest wait instead would have named `engine` on
    a real `paddle-manga` shape where the answer was `detect`.

    **A device-bound or saturated feeder is never blamed.** The commonest
    HEALTHY shape here is a GPU-bound run: the engine is 98% busy and post,
    costing 11 ms a page against its 915, is starved ~100% of the time BY
    CONSTRUCTION. Nothing is wrong and nothing can be widened -- the engine is
    one model on one card -- so the old "post starved 100% ... widen engine"
    was both wrong and unactionable. A feeder that is merely SATURATED is
    still named, because a CPU pool at 97% is exactly the one to widen: what
    disqualifies a stage is being unwidenable, not being busy.

    None whenever too few pages have come through to read (:data:`MIN_PAGES`),
    when the run had no queues at all (``--cpu-workers 0``: the serial
    fallback has nothing to be backed up), when every stage is waiting on the
    one before it (the volume itself is the limit; no pool widens a disk), or
    when no stage waits on another and none is saturated. A balanced pipeline
    HAS no verdict, and inventing one would send the user to widen something
    that is costing them nothing.
    """
    reading = _read_stages(summary)
    return reading[0] if reading is not None else None


def widen_target(summary: dict[str, Any]) -> str | None:
    """Which stage the verdict says to widen -- the KEY, not the sentence.

    ``--bench`` follows the verdict rather than sweeping a grid, so it needs
    the stage the sentence names. Derived here, with the sentence, rather than
    parsed back out of it: a reader that scraped "widen (\\w+)" would go
    silently wrong the day the wording changed.

    None when there is no verdict, and also when the verdict is a FACT with
    no knob attached ("it sets the pace and cannot be widened") -- naming a
    stage there would send the search to buy a second copy of a model on a
    card that is already the limit.
    """
    reading = _read_stages(summary)
    return reading[1] if reading is not None else None


def _read_stages(summary: dict[str, Any]) -> tuple[str, str | None] | None:
    """``(the sentence, the stage to widen)``, or None when nothing is supported."""
    stages = summary.get("stages") or []
    if not any(stage.get("queue") for stage in stages):
        return None
    segments = _segments(stages)
    if not segments or min(segment["items"] for segment in segments) < MIN_PAGES:
        return None

    candidates = [
        candidate
        for candidate in (_starved_candidate(segments), _blocked_candidate(segments))
        if candidate is not None
    ]
    if candidates:
        best = max(candidates, key=lambda candidate: candidate[0])
        return best[1], best[2]

    busiest = max(segments, key=lambda segment: segment["busy_pct"], default=None)
    if busiest is None or busiest["busy_pct"] < BUSY_PCT:
        return None
    width = busiest["workers"] or 1
    if busiest["device_bound"]:
        return (
            f"{busiest['key']} busy {busiest['busy_pct']:.0f}% on the "
            f"{busiest['device']} — it sets the pace and cannot be widened"
        ), None
    return (
        f"{busiest['key']} busy {busiest['busy_pct']:.0f}% of {width} "
        f"worker{'s' if width != 1 else ''} — widen {busiest['key']}"
    ), busiest["key"]


def _segments(stages: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """The rows regrouped into POOLS, which is what the queues connect.

    A fused stage has no pool and no queues of its own: it runs on the worker
    of the stage before it, inside that stage's hand-offs. So the queue
    neighbour of a stage is NOT ``stages[index - 1]`` -- with ``engine`` fused
    into ``detect``, post's feeder is detect, and reading the neighbour by
    index found a stage with no queues at all and gave up. Grouping first
    makes the walk right by construction instead of by a ``continue``.
    """
    segments: list[dict[str, Any]] = []
    for stage in stages:
        if stage.get("fused") and segments:
            leader = segments[-1]
            leader["key"] = f"{leader['key']}+{stage.get('key', '')}"
            leader["busy_pct"] = min(100.0, leader["busy_pct"] + _number(stage.get("busy_pct")))
            leader["device_bound"] = leader["device_bound"] or bool(stage.get("device_bound"))
            leader["items"] = max(leader["items"], int(_number(stage.get("items"))))
            continue
        segments.append(
            {
                "key": str(stage.get("key", "")),
                "device": str(stage.get("device", "cpu")),
                "workers": int(_number(stage.get("workers"))),
                "items": int(_number(stage.get("items"))),
                "busy_pct": _number(stage.get("busy_pct")),
                "blocked_pct": _number(stage.get("blocked_pct")),
                "starved_pct": _number(stage.get("starved_pct")),
                "device_bound": bool(stage.get("device_bound")),
            }
        )
    return segments


def _starved_candidate(segments: list[dict[str, Any]]) -> tuple[float, str, str] | None:
    """The most UPSTREAM pool starved by a feeder that widening would help.

    Walking from the source is what finds the start of the ripple rather than
    its far end. A feeder that is waiting too is not the culprit -- it is one
    more link in the same chain -- so it is skipped, and a run where every
    stage waits on the one before it (the volume reads slower than it OCRs)
    produces no candidate at all. Nor is a DEVICE-BOUND feeder: being starved
    by the card is the expected shape of a GPU-bound run, not a fault with a
    fix, and "widen the engine" names a knob that is clamped to one.

    A feeder that is merely SATURATED is still the answer -- a CPU pool at
    97% is exactly the pool to widen. What decides is whether widening it is
    possible, not how busy it is.
    """
    for index in range(1, len(segments)):
        stage, feeder = segments[index], segments[index - 1]
        starved = stage["starved_pct"]
        if starved < WAIT_PCT or feeder["starved_pct"] >= WAIT_PCT:
            continue
        if feeder["device_bound"]:
            continue
        return (
            starved,
            f"{stage['key']} starved {starved:.0f}% waiting on "
            f"{feeder['key']} — widen {feeder['key']}",
            feeder["key"],
        )
    return None


def _blocked_candidate(segments: list[dict[str, Any]]) -> tuple[float, str, str] | None:
    """The most DOWNSTREAM pool blocked by a drain that is not itself blocked.

    The mirror of :func:`_starved_candidate`, walked from the sink: the pool
    nearest the end that cannot hand its work on is the one whose drain is
    really too narrow. The last stage is never a candidate -- what drains it
    is the driver writing the sidecar, and widening that is not a knob -- and
    neither is a drain that cannot be widened.
    """
    for index in range(len(segments) - 2, -1, -1):
        stage, drain = segments[index], segments[index + 1]
        blocked = stage["blocked_pct"]
        if blocked < WAIT_PCT or drain["blocked_pct"] >= WAIT_PCT:
            continue
        if drain["device_bound"]:
            continue
        return (
            blocked,
            f"{stage['key']} blocked {blocked:.0f}% waiting on "
            f"{drain['key']} — widen {drain['key']}",
            drain["key"],
        )
    return None


def _number(value: Any) -> float:
    """``value`` as a float, or 0.0 for anything that is not one."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return 0.0
    return float(value)


def _pct(seconds: Any, pool_seconds: float) -> float:
    """``seconds`` as a percentage of a pool's time, clamped to 0-100."""
    if pool_seconds <= 0:
        return 0.0
    return round(min(100.0, max(0.0, 100.0 * _number(seconds) / pool_seconds)), 1)


class _StageMeter:
    """A stage's own counters, written by every worker in its pool."""

    __slots__ = ("busy_seconds", "items", "lock")

    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.busy_seconds = 0.0
        self.items = 0

    def record(self, seconds: float) -> None:
        with self.lock:
            self.busy_seconds += seconds
            self.items += 1

    def read(self) -> tuple[float, int]:
        with self.lock:
            return self.busy_seconds, self.items


class _Segment(NamedTuple):
    """One pool and the stages it runs back to back on its own threads.

    A stage of width :data:`SERIAL` has no pool of its own: it is FUSED into
    the stage before it and runs on that stage's worker, with no queue and no
    hand-off between them. Fusing every stage is the serial fallback.
    """

    stages: tuple[Stage, ...]
    workers: int
    capacity: int

    @property
    def key(self) -> str:
        return self.stages[0].spec.key


def fuse_stages(stages: Sequence[Stage]) -> list[_Segment]:
    """Group stages into the pools that will run them.

    A width of :data:`SERIAL` means "no pool of my own": the stage joins the
    one before it, and the two run back to back on one worker with no queue
    between them. When EVERY stage is SERIAL there is nothing to join and the
    pipeline takes the fallback path instead. The first stage cannot fuse
    backwards, so :class:`StagePipeline` gives it a pool of one before it gets
    here.

    A STRUCTURAL CEILING SURVIVES FUSING. Joining a stage to the segment
    before it runs it at THAT segment's width, so a stage declaring
    ``max_workers=1`` -- one model on one device -- cannot join a pool of
    four: that would be four threads inside one transformer's ``generate()``,
    which is neither reentrant nor affordable in VRAM. ``--stage-workers
    engine=0`` asked for exactly that and got it (measured: four concurrent
    callers in a stage declared ``max_workers=1``). Such a stage gets a pool
    of its own instead, at its ceiling, which is the nearest thing to what was
    asked for that is safe.
    """
    segments: list[_Segment] = []
    for stage in stages:
        ceiling = stage.spec.max_workers
        fits = segments and (ceiling is None or segments[-1].workers <= ceiling)
        if stage.workers <= SERIAL and fits:
            last = segments[-1]
            segments[-1] = last._replace(stages=(*last.stages, stage))
            continue
        width = max(1, stage.workers)
        if stage.workers <= SERIAL and ceiling is not None:
            width = max(1, ceiling)
        segments.append(_Segment(stages=(stage,), workers=width, capacity=stage.capacity))
    # The last stage in a segment is the one whose output leaves it, so its
    # declared capacity is the segment's output queue.
    return [seg._replace(capacity=max(1, seg.stages[-1].capacity)) for seg in segments]


class StagePipeline:
    """One volume through one pipeline: a pool a stage, a bounded queue between.

    ``stages`` are :class:`Stage` records -- a declared :class:`StageSpec`, the
    callable that runs it, the width of its pool and the capacity of the queue
    it fills. :meth:`run` yields ``(item, outcome)`` in INPUT order.

    Every stage of width :data:`SERIAL` is the degenerate path: no threads, no
    queues, each page walked through every stage on the caller's own thread, in
    order. That is the fallback ``--cpu-workers 0`` asks for, and the shape
    every byte-identity test pins the output against.
    """

    # Slots of runway in front of the first stage. The source is a list of
    # paths, so this costs nothing and only stops the first pool waiting on the
    # feeder thread for its next page.
    SOURCE_EXTRA = 2

    def __init__(
        self,
        stages: Sequence[Stage],
        *,
        source_capacity: int | None = None,
        in_flight_limit: int | None = None,
    ) -> None:
        # A stage's structural ceiling is enforced HERE as well as where the
        # widths are derived, because it is a property of the stage and not of
        # how it was asked for: one model on one device stays one model on one
        # device however the Stage records were built.
        planned = [
            stage
            if stage.spec.max_workers is None or stage.workers <= stage.spec.max_workers
            else stage._replace(workers=stage.spec.max_workers)
            for stage in stages
        ]
        self.serial = all(stage.workers <= SERIAL for stage in planned)
        if not self.serial and planned and planned[0].workers <= SERIAL:
            # Nothing to fuse backwards into: the first stage gets a pool of
            # one rather than silently running on the feeder thread, which
            # would make the feeder both producer and consumer.
            planned[0] = planned[0]._replace(workers=1)
        self.stages = tuple(planned)
        self.meters = {stage.spec.key: _StageMeter() for stage in self.stages}
        self.queues: list[StageQueue] = []
        self._threads: list[threading.Thread] = []
        self._source_capacity = source_capacity
        self._in_flight_limit = in_flight_limit
        self._tickets = _Tickets(1)
        self._started = time.monotonic()
        self._elapsed: float | None = None
        self._ran = False
        self._closed = False
        self._source_error: BaseException | None = None
        # A worker that died abnormally, rather than a page that merely failed.
        # Set by _work, raised by _drain, so a lost worker cannot pass for a
        # short volume.
        self._worker_error: BaseException | None = None
        self._queue_of: dict[str, tuple[StageQueue, StageQueue]] = {}

    # -- running -----------------------------------------------------------

    def run(self, items: Iterable[Any]) -> Generator[tuple[Any, Outcome], None, None]:
        """Yield ``(item, outcome)`` in input order, stages overlapped across items.

        One pipeline, one run: the queues, the pools and the counters are this
        run's, and a second call would spawn a second set of threads behind a
        closed teardown. Build another pipeline for another volume.
        """
        if self._ran:
            raise RuntimeError("this pipeline has already run; build another")
        self._ran = True
        self._started = time.monotonic()
        if self.serial:
            return self._run_serial(items)
        return self._run_pooled(items)

    def _run_serial(self, items: Iterable[Any]) -> Generator[tuple[Any, Outcome], None, None]:
        try:
            for item in items:
                outcome = Outcome(item, None)
                for stage in self.stages:
                    outcome = self._apply(stage, item, outcome)
                yield item, outcome
        finally:
            self._elapsed = time.monotonic() - self._started

    def _run_pooled(self, items: Iterable[Any]) -> Generator[tuple[Any, Outcome], None, None]:
        segments = fuse_stages(self.stages)
        first = segments[0]
        source_cap = self._source_capacity or first.workers + self.SOURCE_EXTRA
        queues = [StageQueue(f"in->{first.key}", source_cap)]
        for index, segment in enumerate(segments):
            nxt = segments[index + 1].key if index + 1 < len(segments) else "out"
            queues.append(StageQueue(f"{segment.key}->{nxt}", segment.capacity))
        self.queues = queues
        self._tickets = _Tickets(
            self._in_flight_limit
            or source_cap
            + sum(segment.capacity for segment in segments)
            + sum(segment.workers for segment in segments)
        )
        for index, segment in enumerate(segments):
            inq, outq = queues[index], queues[index + 1]
            for stage in segment.stages:
                self._queue_of[stage.spec.key] = (inq, outq)
            remaining = _Countdown(segment.workers)
            for worker in range(segment.workers):
                self._spawn(
                    f"ocr-{segment.key}-{worker}", self._work, segment, inq, outq, remaining
                )
        self._spawn("ocr-feed", self._feed, items, queues[0])
        try:
            yield from self._drain(queues[-1])
            if self._source_error is not None:
                raise self._source_error
        finally:
            self.close()

    def _spawn(self, name: str, target: Callable[..., None], *args: Any) -> None:
        # Daemon threads: a stage wedged on a model must never be what keeps
        # the runner alive after the parent has killed it. close() still joins
        # them, so a clean shutdown is still a clean shutdown.
        thread = threading.Thread(target=target, args=args, name=name, daemon=True)
        self._threads.append(thread)
        thread.start()

    def _feed(self, items: Iterable[Any], out: StageQueue) -> None:
        try:
            for seq, item in enumerate(items):
                # A ticket first: it is the ceiling on pages in flight
                # ANYWHERE, the driver's reorder buffer included.
                if not self._tickets.take():
                    return
                if not out.put((seq, item, Outcome(item, None))):
                    self._tickets.give()
                    return
        except BaseException as e:  # a source that dies must not hang the drain
            self._source_error = e
        finally:
            out.finish()

    def _work(
        self, segment: _Segment, inq: StageQueue, outq: StageQueue, remaining: _Countdown
    ) -> None:
        try:
            while True:
                entry = inq.get()
                if entry is _SENTINEL:
                    return
                seq, item, outcome = entry
                for stage in segment.stages:
                    outcome = self._apply(stage, item, outcome)
                if not outq.put((seq, item, outcome)):
                    return
        except BaseException as e:  # noqa: BLE001
            # A worker must never die quietly. Its finally below decrements the
            # countdown, and the last one out finishes the output queue -- so a
            # worker lost here would end the drain CLEANLY on a short volume with
            # no error anywhere. Record it; _drain raises it instead of returning
            # a truncated page list.
            self._worker_error = e
        finally:
            # The last worker out closes the gate behind the whole pool.
            if remaining.decrement() == 0:
                outq.finish()

    def _apply(self, stage: Stage, item: Any, outcome: Outcome) -> Outcome:
        if outcome.error is not None:
            return outcome  # a page that already failed falls through untouched
        started = time.monotonic()
        try:
            return Outcome(stage.run(item, outcome.value), None)
        except BaseException as e:  # noqa: BLE001
            # BaseException, not Exception: Outcome.error is typed for it, and
            # anything escaping here kills the worker (see _work). Carried, not
            # raised -- one page must not end the volume.
            return Outcome(None, e)
        finally:
            self.meters[stage.spec.key].record(time.monotonic() - started)

    def _drain(self, sink: StageQueue) -> Iterator[tuple[Any, Outcome]]:
        """Input order, rebuilt at the sink from the sequence numbers.

        A page leaving here gives its ticket back, which is what lets another
        into the pipeline -- so the buffer this holds is bounded by the same
        number every queue is, and cannot grow to the length of the volume
        behind one slow page.
        """
        held: dict[int, tuple[Any, Outcome]] = {}
        want = 0
        while True:
            if want in held:
                entry = held.pop(want)
                want += 1
                self._tickets.give()
                yield entry
                continue
            got = sink.get()
            if got is _SENTINEL:
                break
            seq, item, outcome = got
            held[seq] = (item, outcome)
        while want in held:
            entry = held.pop(want)
            want += 1
            self._tickets.give()
            yield entry
        # A lost worker ends the drain exactly like a finished one, so the short
        # volume has to be caught HERE or it passes silently.
        if self._worker_error is not None:
            raise self._worker_error

    # How long a teardown may wait for its threads ALTOGETHER. One deadline,
    # not one per thread: a per-thread timeout means K wedged workers cost K
    # times the wait, and a caller cancelling a volume would sit through
    # minutes of it (measured: 30.00 s for one wedged worker, 30 K for K).
    CLOSE_TIMEOUT = 30.0

    def close(self) -> None:
        """Stop everything and join it. Idempotent; safe from the driver only."""
        if self._closed:
            return
        self._closed = True
        self._elapsed = time.monotonic() - self._started
        self._tickets.close()
        for q in self.queues:
            q.close()
        deadline = time.monotonic() + self.CLOSE_TIMEOUT
        for thread in self._threads:
            thread.join(timeout=max(0.0, deadline - time.monotonic()))
        # Whatever is still alive is a daemon thread wedged in a model; it
        # cannot keep the process up, and saying so beats waiting on it.
        self._threads = [t for t in self._threads if t.is_alive()]

    # -- what it measured --------------------------------------------------

    @property
    def elapsed(self) -> float:
        return self._elapsed if self._elapsed is not None else time.monotonic() - self._started

    def report(self) -> PipelineReport:
        elapsed = self.elapsed
        queues = tuple(q.report(elapsed) for q in self.queues)
        by_name = {q.name: q for q in queues}
        stages = []
        items = 0
        for stage in self.stages:
            key = stage.spec.key
            busy, count = self.meters[key].read()
            inq, outq = self._queue_of.get(key, (None, None))
            workers = max(1, stage.workers)
            stages.append(
                StageReport(
                    key=key,
                    name=stage.spec.name,
                    device=stage.spec.device,
                    workers=stage.workers,
                    items=count,
                    busy_seconds=busy,
                    blocked_seconds=by_name[outq.name].blocked_seconds if outq else 0.0,
                    starved_seconds=by_name[inq.name].starved_seconds if inq else 0.0,
                    utilisation=busy / (workers * elapsed) if elapsed > 0 else 0.0,
                    device_bound=stage.spec.max_workers == DEVICE_BOUND,
                )
            )
            items = max(items, count)
        return PipelineReport(elapsed=elapsed, items=items, stages=tuple(stages), queues=queues)

    def snapshot(self) -> dict[str, Any]:
        """The live numbers, JSON-safe, for whoever is watching the run."""
        return self.report().as_dict()


class _Tickets:
    """A ceiling on the pages in flight ANYWHERE, including the reorder buffer.

    The queues bound what waits between two stages, but they do not bound what
    the driver is HOLDING: a page stuck in the last stage while the rest of the
    volume streams past it would pile finished pages up in the reassembly
    buffer, one per page of the volume. So a page takes a ticket at the feeder
    and gives it back when it is yielded, and the feeder blocks on a condition
    when there are none left -- the same real blocking the queues use, one step
    further upstream.

    The default ceiling is the pipeline's own natural width (every queue slot
    plus every worker), so this never bites on a healthy run; it is what makes
    "memory stays flat over a 292-page volume" true of the pathological one.
    """

    def __init__(self, limit: int) -> None:
        self._limit = max(1, limit)
        self._held = 0
        self._cv = threading.Condition()
        self._closed = False

    def take(self) -> bool:
        with self._cv:
            while self._held >= self._limit and not self._closed:
                self._cv.wait()
            if self._closed:
                return False
            self._held += 1
            return True

    def give(self) -> None:
        with self._cv:
            self._held = max(0, self._held - 1)
            self._cv.notify()

    def close(self) -> None:
        with self._cv:
            self._closed = True
            self._cv.notify_all()

    @property
    def held(self) -> int:
        with self._cv:
            return self._held


class _Countdown:
    """How many workers of a pool are still running."""

    __slots__ = ("_lock", "_left")

    def __init__(self, count: int) -> None:
        self._lock = threading.Lock()
        self._left = count

    def decrement(self) -> int:
        with self._lock:
            self._left -= 1
            return self._left


def staged_pipeline(items: Iterable[Any], stages: Sequence[Stage]) -> Iterator[tuple[Any, Outcome]]:
    """Run ``items`` through ``stages``, yielding ``(item, outcome)`` in input order.

    The one-shot form of :class:`StagePipeline`, for callers that want the
    stream and not the numbers.
    """
    return StagePipeline(stages).run(items)


def _env_int(name: str, default: int) -> int:
    """A positive integer from the environment, or ``default``."""
    raw = os.environ.get(name, "").strip()
    return int(raw) if raw.isdigit() and int(raw) > 0 else default


def physical_cpu_count() -> int:
    """Cores that can actually run a session at once -- NOT ``os.cpu_count()``.

    SMT siblings measured worth nothing for this work: the CTC stage peaks at
    exactly the physical core count (1044 crops/s at 16, on a host whose
    ``os.cpu_count()`` is 32), and 2x oversubscription costs 20%. Sizing pools
    off the logical count therefore buys twice the workers for no throughput,
    so count unique thread-sibling groups from sysfs and fall back to the
    logical count only where that is unreadable (containers, non-Linux).
    """
    try:
        siblings = Path("/sys/devices/system/cpu").glob(
            "cpu[0-9]*/topology/thread_siblings_list"
        )
        groups = {path.read_text().strip() for path in siblings}
        if groups:
            return len(groups)
    except OSError:
        pass
    return os.cpu_count() or 4


def host_worker_budget(
    cpus: int | None = None, *, jobs: int = 0, threads: int = SESSION_THREADS
) -> int:
    """This run's share of the host: the most workers ANY stage may have.

    The cores, less one for the driving thread (crops, layout, feeding the
    GPU), split between the OCR jobs that may run at once, divided by the
    threads a session takes. Which stage gets how much of it is
    :func:`plan_stage_workers`; this only says how much there is.

    ``jobs`` defaults to ``MOKURO_OCR_JOBS``, then to :data:`CPU_DEFAULT_JOBS`,
    so a runner never assumes it owns the machine. Never zero: one worker is
    still a worker, and on a GPU engine that one thread is what keeps the card
    from waiting.
    """
    cpus = cpus or physical_cpu_count()
    jobs = jobs or _env_int(CPU_JOBS_ENV, CPU_DEFAULT_JOBS)
    share = max(1, (cpus - 1) // max(1, jobs))
    return max(1, share // max(1, threads))


def resolve_cpu_workers(requested: int | None) -> int | None:
    """An explicit width for every pooled stage, or None to derive per stage.

    ``--cpu-workers`` first, then ``MOKURO_OCR_CPU_WORKERS``. ``0`` from either
    is a real answer and the only way to ask for the serial fallback: no
    threads, no queues, every stage on the one thread.
    """
    if requested is not None:
        return max(0, requested)
    raw = os.environ.get(CPU_WORKERS_ENV, "").strip()
    return int(raw) if raw.isdigit() else None


def parse_stage_setting(raw: str | None) -> dict[str, int]:
    """``"detect=4,post=2"`` -> ``{"detect": 4, "post": 2}``; ``"3"`` -> every stage.

    The shape both ``--stage-workers`` and ``--queue-capacity`` take, on the
    command line and in the environment. A bare number is filed under
    :data:`ALL_STAGES`, so "three everywhere" stays one short word. An unknown
    stage name is refused rather than ignored: a typo that silently changed
    nothing would be worse than a failed run.
    """
    if raw is None:
        return {}
    text = raw.strip()
    if not text:
        return {}
    known = {spec.key for specs in STAGE_GRAPHS.values() for spec in specs}
    out: dict[str, int] = {}
    for part in text.split(","):
        piece = part.strip()
        if not piece:
            continue
        name, sep, number = piece.partition("=")
        key = name.strip() if sep else ALL_STAGES
        value = number.strip() if sep else name.strip()
        if key not in known and key != ALL_STAGES:
            raise ValueError(
                f"unknown stage {key!r}: the stages are {', '.join(sorted(known))} "
                f"(or a bare number for all of them)"
            )
        try:
            out[key] = int(value)
        except ValueError:
            raise ValueError(f"{piece!r} is not a stage setting (want e.g. 'detect=4')") from None
        if out[key] < 0:
            raise ValueError(f"{piece!r}: a stage setting cannot be negative")
    return out


def resolve_stage_setting(requested: str | None, env_name: str) -> dict[str, int]:
    """A per-stage setting: the command line first, then the environment.

    Same precedence ``--cpu-workers`` has had: what the caller said wins over
    what the environment said, and neither is merged with the other -- a
    command line that names one stage means the environment's other stages are
    not also applied, because half-applied tuning is the hardest kind to read.
    """
    if requested is not None and requested.strip():
        return parse_stage_setting(requested)
    return parse_stage_setting(os.environ.get(env_name))


def parse_stage_devices(raw: str | None) -> dict[str, str]:
    """``"detect=cpu,engine=cuda:1"`` -> ``{"detect": "cpu", "engine": "gpu:1"}``.

    The shape ``--stage-device`` takes, the same ``k=v`` list as
    ``--stage-workers`` -- but only for stages that HOLD A MODEL, and with no
    bare form: "everything on the GPU" is not a thing to ask for, because
    ``post`` has nothing to put there. An unknown stage or an unreadable device
    is refused rather than ignored, for the reason every other typo here is:
    tuning that silently did not happen is worse than a failed run.
    """
    if raw is None:
        return {}
    text = raw.strip()
    if not text:
        return {}
    out: dict[str, str] = {}
    for part in text.split(","):
        piece = part.strip()
        if not piece:
            continue
        name, sep, value = piece.partition("=")
        if not sep:
            raise ValueError(
                f"{piece!r} is not a stage device (want e.g. 'detect=cpu'): a device is "
                f"chosen per stage, and only {', '.join(MODEL_STAGES)} hold a model"
            )
        key = name.strip()
        if key not in MODEL_STAGES:
            raise ValueError(
                f"unknown model stage {key!r}: a device may be chosen for "
                f"{', '.join(MODEL_STAGES)}"
            )
        try:
            out[key] = parse_device(value)
        except ValueError as e:
            raise ValueError(f"{piece!r}: {e}") from None
    return out


def resolve_stage_devices(requested: str | None, env_name: str) -> dict[str, str]:
    """Where each model goes: the command line first, then the environment."""
    if requested is not None and requested.strip():
        return parse_stage_devices(requested)
    return parse_stage_devices(os.environ.get(env_name))


def pipeline_stats_path(args: argparse.Namespace, detect_dir: Path) -> Path:
    """Where the live pool/queue numbers are published.

    ``--stats-file``, then ``$MOKURO_OCR_PIPELINE_STATS``, then beside the
    detector dumps. NEVER under ``--cache-dir``: the server counts the JSON
    files there to know how many pages are done, and this one would be counted
    as a page.
    """
    chosen = getattr(args, "stats_file", None) or os.environ.get(PIPELINE_STATS_ENV, "").strip()
    return Path(chosen) if chosen else detect_dir / PIPELINE_STATS_FILE


def write_pipeline_stats(path: Path, pipeline: StagePipeline) -> None:
    """Publish the pipeline's live numbers, atomically. Never fatal."""
    publish_pipeline_stats(path, pipeline.snapshot())


def publish_pipeline_stats(path: Path, snapshot: dict[str, Any]) -> None:
    """Publish a snapshot, atomically. Never fatal.

    A run must not die because a stats file could not be written, and a reader
    must never see half a file -- so it is written beside and renamed over.

    Takes the snapshot rather than the pipeline, because a session publishes
    each VOLUME's share of the counters (the difference between two of them)
    and not the whole session's totals -- the congestion history is per run.
    """
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(path.name + ".tmp")
        dump_json(snapshot, tmp)
        tmp.replace(path)
    except OSError as e:
        log(f"[runner] WARN could not write {path}: {e}")


class VolumePaths(NamedTuple):
    """Where one volume's pages come from and where its artefacts go.

    Carried BY THE PAGE rather than captured in the stage callables, because
    a stage is a model and a model outlives a volume: the pipeline this file
    builds is opened for an (engine, detector) and could be fed the pages of
    one volume and then the next without rebuilding it. A stage that closed
    over ``input_dir`` could not be.
    """

    input_dir: Path
    detect_dir: Path
    cache_dir: Path
    # Which volume this is, for a pipeline holding the pages of several at
    # once: the server's job id in ``--serve``, "" for the single-volume CLI.
    id: str = ""
    # This volume's own log file, open, or None when the run's lines go
    # straight to stdout (the CLI). See :class:`RunnerLog`.
    log: Any = None


class PageJob(NamedTuple):
    """One page of one volume: the item that flows through the pipeline.

    ``blob`` is the page's ENCODED bytes when they came out of an archive and
    were never written to disk (``--serve`` on a road whose detector runs in
    this process). Then ``image`` names where the page WOULD be -- it is still
    the page's identity, and ``rel`` is still what the sidecar is keyed by --
    but nothing reads it: :meth:`decode` decodes the bytes in memory.

    ``seq`` and ``last`` are the SOURCE's order, stamped where the pages are
    read and therefore exact: ``seq`` counts pages through this run (across
    volumes, never reset), ``last`` marks a volume's final page. The pipeline
    rebuilds input order at its sink by itself and needs neither -- but a
    stage that talks to something ORDERED does: the served road's engine takes
    a volume's pages strictly in order and has to be told where a volume ends,
    and its ``feed`` stage is a pool, so the pages reach it out of order. A
    source that does not stamp them leaves ``seq`` at -1, which that road
    refuses rather than guesses at.
    """

    volume: VolumePaths
    rel: Path
    blob: bytes | None = None
    seq: int = -1
    last: bool = False

    @property
    def image(self) -> Path:
        return self.volume.input_dir / self.rel

    @property
    def detection(self) -> Path:
        """Where this page's detector JSON goes, and is read back from."""
        return (self.volume.detect_dir / self.rel).with_suffix(".json")

    @property
    def dump(self) -> Path:
        """Where what the models saw goes: beside the detections, not in the cache."""
        return (self.volume.detect_dir / self.rel).with_suffix(".json")

    @property
    def cache(self) -> Path:
        """The per-page progress JSON the server counts."""
        return (self.volume.cache_dir / self.rel).with_suffix(".json")

    def decode(self) -> Any:
        """This page as a BGR array, from memory when it never hit the disk.

        The two roads decode the same bytes with the same decoder, so a page
        read out of an archive and the same page extracted to a directory
        produce the same array -- which is what byte-identity rests on.
        """
        if self.blob is None:
            return imread_bgr(self.image)
        return imdecode_bgr(self.blob)

    def blank(self) -> dict[str, Any] | None:
        """The empty page record for a page that failed, or None if unreadable."""
        if self.blob is None:
            return blank_page(self.image)
        return blank_page_bytes(self.blob)

    def __str__(self) -> str:  # what the log lines print
        return str(self.rel)


class DetectedPage(NamedTuple):
    """A page after the detect stage: the image, and what the detector made of it.

    Carried from the producer to the consumer, so it holds the decoded image
    too -- the recognizer's crops come out of the same pixels the detector
    boxed, and decoding twice would cost more than the buffer does.
    """

    image: Any
    lines: list[Any]
    info: dict[str, Any]
    first: Any


class ReadPage(NamedTuple):
    """A page after the engine stage, on its way to the layout.

    Everything the post stage needs and nothing the GPU stage still holds: the
    detected lines with their merged text, the per-line reconcile results, and
    how long the engine itself took. It travels as one value per page, so no
    two pages can cross their line pairings however many of them are in
    flight. The crops stay in the engine stage and are dropped with it.
    """

    detected: DetectedPage
    targets: list[int]
    settled: list[Any]
    pitch: float
    engine_seconds: float


class PageResult(NamedTuple):
    """What a finished page carries out of the last stage.

    The page dict the sidecar is built from, the raw dump written beside it,
    and -- on the reconciled road -- the per-page reconcile tally and the lines
    an editor should look at. The driver appends those to ``review`` IN PAGE
    ORDER, which is why they are returned rather than appended here: the post
    stage is a pool, and ``review.json`` is read as an ordered list.
    """

    page: dict[str, Any]
    raw: dict[str, Any] | None = None
    tally: dict[str, Any] | None = None
    doubtful: list[dict[str, Any]] | None = None
    engine_seconds: float = 0.0


class PPOcrPool:
    """One ``ppocr`` engine per CPU-stage worker, sharing nothing.

    An engine is BOTH onnxruntime sessions -- the detector and the CTC
    recognizer (``PPOcr._det`` and ``PPOcr._rec``) -- so a pool of K is K
    detector sessions and K recognizer sessions, and the stage's two models
    are widened by the one knob. That is deliberate: the CTC recognizer is the
    bigger half (61% of the stage against the detector's 25%) and scales the
    same way, by sessions (2.44x at four) rather than by threads (1.26x at
    eight). Neither model is big enough for K copies to matter: ~23 MB the pair.

    Splitting a single page's CTC batch across sessions is deliberately NOT
    done. ``recognize_crops`` batches a page's crops by width and pads each
    batch to its widest member, so cutting the batch up would change the
    padding and could change what is read.

    Each engine also keeps its own ``last_detect_info``, so two pages can be in
    the stage at once without sharing a byte of mutable state.

    The first engine is the reader's own, so a pool of one is the object graph
    the serial path had.
    """

    def __init__(self, first: Any, make: Callable[[], Any], size: int) -> None:
        self.size = max(1, int(size))
        self._make = make
        self._idle: queue.LifoQueue[Any] = queue.LifoQueue()
        self._idle.put(first)
        for _ in range(self.size - 1):
            self._idle.put(make())

    def resize(self, size: int) -> None:
        """Grow or shrink BETWEEN runs -- never while a page holds a lease.

        ``--bench`` rebuilds its pools a trial. Building the sessions HERE
        rather than on first use means the trial that asked for them is not
        the one that pays for them, and the ones already built are kept: only
        the difference costs anything.
        """
        size = max(1, int(size))
        while self.size < size:
            self._idle.put(self._make())
            self.size += 1
        while self.size > size:
            self._idle.get()
            self.size -= 1

    @contextlib.contextmanager
    def lease(self) -> Iterator[Any]:
        """Borrow an engine for one page. Never blocks while workers <= size."""
        engine = self._idle.get()
        try:
            yield engine
        finally:
            self._idle.put(engine)



class PPOcrPageReader:
    """The ``ppocr-manga`` engine: a page image in, a mokuro page out.

    Detection and recognition are ``ppocr.PPOcr`` (onnxruntime, CPU); blocks,
    reading order and ruby removal are ``line_layout``. ``raw`` of the last
    page is kept so the caller can dump what the models saw before layout.
    """

    def __init__(self, ppocr: Any | None = None, layout: Any | None = None) -> None:
        self.ppocr = ppocr if ppocr is not None else load_sibling("ppocr")
        self.layout = layout if layout is not None else load_sibling("line_layout")
        started = time.time()
        self.engine = self.ppocr.PPOcr()
        # ``ppocr.py`` pins its own repo; report it the same way a recognizer
        # reports its own, for the sidecar's ``ocr_engine.weights``. A test
        # double for ``ppocr`` need not carry the constants.
        repo_id = getattr(self.ppocr, "REPO_ID", None)
        repo_revision = getattr(self.ppocr, "REPO_REVISION", None)
        # ...but only when the files really came from that pinned download: a
        # configured ``$MOKURO_PPOCR_MODELS`` directory holds whatever was
        # copied into it, and a commit claimed for it would be a lie.
        pinned_files = getattr(getattr(self.engine, "models", None), "pinned", True)
        self.repos: dict[str, str] = (
            {str(repo_id): str(repo_revision)} if pinned_files and repo_id and repo_revision else {}
        )
        log(
            f"[runner] PP-OCRv6 manga models resolved in {time.time() - started:.1f}s "
            f"({self.engine.models.precision}, threads={self.engine.threads})"
        )
        self.raw: dict[str, Any] = {}
        self.ruby_count = 0

    def clone_engine(self) -> Any:
        """Another engine exactly like this reader's, for a second detect worker.

        Same model files, same thread count, same side and tile policy: a
        pooled detect stage has to read a page the way the serial one would,
        and the files are resolved once rather than per worker.
        """
        return self.ppocr.PPOcr(
            self.engine.models,
            threads=self.engine.threads,
            side=self.engine.side,
            tile=self.engine.tile,
        )

    def read_lines(
        self, img: Any, engine: Any | None = None
    ) -> tuple[list[Any], dict[str, Any], Any]:
        """Detect and read a page's lines: ``(lines, detector info, first layout)``.

        The first layout is the one run to tell ruby from text before the
        probes; its ``ruby`` and ``kinds`` index the returned lines.

        ``engine`` is the ``ppocr`` session pair to read with -- one lent by a
        :class:`PPOcrPool` when the detect stage is pooled, the reader's own
        otherwise. Nothing else here is instance state, so two calls on two
        engines can run at once.
        """
        engine = engine if engine is not None else self.engine
        height, width = img.shape[0], img.shape[1]
        lines = engine.read_page(img)
        info = dict(engine.last_detect_info)
        raw = self.ppocr.page_to_json(lines, width, height, detector=info)
        # 1. Pieces of one printed column are read again as one line: geometry
        #    finds them, the recognizer decides whether the joined read stands.
        #    First, because a piece's end is not a line end to be probed.
        pieces = self.layout.column_pieces(raw)
        if pieces:
            joined = engine.join_lines(img, lines, pieces)
            info["joined"] = len(lines) - len(joined)
            lines = joined
            raw = self.ppocr.page_to_json(lines, width, height, detector=info)
        # 2. Brackets and stops the detector's boxes left out. Ruby is excluded
        #    (its probes would pick up the neighbouring column's marks), so the
        #    layout runs once just to say which lines are ruby -- and which are
        #    columns of a text body, where a thin first glyph ("一", "―") is
        #    probed for too; on manga that probe reads bubble outlines.
        first = self.layout.layout_page(raw)
        ruby = {run.line for run in first.ruby}
        in_body = set().union(*(body.members for body in first.bodies))
        text_lines = [(i, line) for i, line in enumerate(lines) if i not in ruby]
        info["recovered_ends"] = engine.recover_clipped_ends(
            img, [line for _, line in text_lines], thin=[i in in_body for i, _ in text_lines]
        )
        # 3. Characters the recognizer itself doubted are put to a vote against
        #    two wider crops (rare kanji, mostly). After the probes, which
        #    compare their reads with the line's text as first read.
        info["second_opinions"] = engine.second_opinions(img, [ln for _, ln in text_lines])
        return lines, info, first

    def finish(
        self, lines: Sequence[Any], info: dict[str, Any], img: Any, version: str
    ) -> tuple[dict[str, Any], Any, dict[str, Any]]:
        """Lay the lines out: ``(page, layout result, raw dump)``.

        The raw dump is RETURNED, and only returned. This runs on a POOL, so
        two pages can be inside it at once: anything written to the reader
        here would be whichever page happened to finish last, read back by
        whoever asked next. The single-page callers keep their ``self.raw``
        and ``self.ruby_count`` -- :meth:`read_detected` sets them, and only
        :meth:`__call__` goes through there.
        """
        height, width = img.shape[0], img.shape[1]
        raw = self.ppocr.page_to_json(lines, width, height, detector=info)
        page, result = layout_page_dict(raw, self.layout, version)
        # Removed ruby stays with the raw dump, tied to the base characters it
        # glosses: the reading is evidence about a doubtful kanji, and a later
        # format can show it.
        raw["ruby"] = [
            {"line": run.line, "base": run.base, "text": run.text, "chars": list(run.chars)}
            for run in result.ruby
        ]
        return page, result, raw

    def detect_page(self, img: Any, engine: Any | None = None) -> DetectedPage:
        """The stage that can run ahead: everything before the next model.

        Pure with respect to the reader -- it touches the lent ``engine`` and
        module-level functions only -- so a pool of workers can be in here at
        once while the consumer is in :meth:`read_detected` for an earlier page.
        """
        lines, info, first = self.read_lines(img, engine)
        return DetectedPage(img, lines, info, first)

    def layout_detected(self, detected: DetectedPage, version: str) -> PageResult:
        """The stage after detection: lay the page out and hand back the dump too.

        Pure with respect to the reader, so the layout stage can run on a pool
        of its own while the detect pool is several pages ahead.
        """
        page, _result, raw = self.finish(detected.lines, detected.info, detected.image, version)
        return PageResult(page=page, raw=raw)

    def read_detected(self, detected: DetectedPage, version: str) -> dict[str, Any]:
        """Finish a detected page. The whole tail of it, for a single-page caller.

        THE only place the reader's debug state is written, and a single-page
        entry point: a pooled stage calls :meth:`layout_detected` or
        :meth:`finish_read` directly and leaves the reader alone, so two pages
        in flight cannot race over ``self.raw``.
        """
        result = self.layout_detected(detected, version)
        self.raw = result.raw or {}
        self.ruby_count = len(self.raw.get("ruby") or [])
        return result.page

    def __call__(self, img: Any, version: str) -> dict[str, Any]:
        return self.read_detected(self.detect_page(img), version)


class ReconciledPageReader(PPOcrPageReader):
    """Another engine's recognizer on the ``ppocr-manga`` detector's lines.

    The page is read the ``ppocr-manga`` way first (:meth:`read_lines`), which
    settles everything that is geometry: one quad per printed column, ends
    grown over a recovered bracket, ruby told from text. The engine then reads
    a deskewed crop of every line that is not ruby, and ``line_reconcile``
    merges its read with the CTC one (the engine's kana and kanji; the CTC
    read's brackets, printed widths and blank cells; the CTC read outright
    when the engine ran away or said nothing). Lines the two still differ on
    are read once more from a wider crop (``second_crop_fn``), and two reads
    out of three settle each difference. Ruby keeps its CTC text: it is
    removed by the layout either way, and on a novel page it is a third of the
    lines.

    The per-line reads and how they were settled go into the raw dump
    (``raw["reconcile"]``, and ``vlm``/``ctc``/``agreement`` on each line).
    The sidecar gets the merged text only.
    """

    def __init__(
        self,
        recognize: Recognizer,
        crop_fn: CropFn,
        *,
        ppocr: Any | None = None,
        layout: Any | None = None,
        reconcile: Any | None = None,
        second_crop_fn: CropFn | None = None,
    ) -> None:
        super().__init__(ppocr=ppocr, layout=layout)
        # Two model pairs read this page; the sidecar names both.
        self.repos = {**self.repos, **getattr(recognize, "repos", {})}
        self.recognize = recognize
        self.crop_fn = crop_fn
        self.second_crop_fn = second_crop_fn
        self.reconcile = reconcile if reconcile is not None else load_sibling("line_reconcile")
        self.engine_seconds = 0.0

    def engine_read(self, detected: DetectedPage) -> ReadPage:
        """The GPU stage: crop, read with the engine, merge with the CTC read.

        The detect stage (inherited) has already boxed, CTC-read, joined and
        probed the lines, on another thread and an earlier page. This is the
        stage that holds the recognizer, so it is deliberately the SHORTEST
        thing that has to: the two GPU batches with the reconcile that decides
        the second one's membership between them, and nothing else. The layout
        and the dump are the post stage's, so the card is not waiting on numpy.

        Nothing here is instance state -- everything it makes travels in the
        returned :class:`ReadPage` -- so the stage could be widened to a second
        recognizer without the two pages meeting.
        """
        img, lines, info, first = detected
        skip = {run.line for run in first.ruby}
        in_body = set().union(*(body.members for body in first.bodies))
        targets = [i for i in range(len(lines)) if i not in skip]
        blocks = [self._line_block(lines[i]) for i in targets]
        pitch = body_pitch(lines)
        neighbours = parallel_neighbours(lines)
        cells = [
            self.reconcile.line_cells(*quad_extents(blk["lines"][0], blk["vertical"]), pitch)
            for blk in blocks
        ]

        started = time.time()
        texts = self._read(img, blocks, cells, self.crop_fn, range(len(blocks)))
        settled = [
            self.reconcile.reconcile_line(
                texts[k],
                self.layout.normalize_text(lines[i].text.strip()),
                cells[k],
                thin=i in in_body,
                ctc_conf=float(lines[i].conf),
                # one entry a character, or ``reconcile_line`` ignores them
                ctc_char_confs=list(getattr(lines[i], "char_confs", None) or []),
            )
            for k, i in enumerate(targets)
        ]
        if self.second_crop_fn is not None:
            doubted = [k for k, line in enumerate(settled) if self.reconcile.needs_second_read(line)]
            second = self._read(img, blocks, cells, self.second_crop_fn, doubted)
            for k in doubted:
                settled[k] = self.reconcile.settle_disputes(settled[k], second[k], cells[k])
        # Carried in the ReadPage, never stashed on the reader: this runs on
        # a pool, and a reader-held figure would be whichever page was last.
        engine_seconds = time.time() - started
        for k, (i, result) in enumerate(zip(targets, settled, strict=True)):
            main, em = quad_extents(blocks[k]["lines"][0], blocks[k]["vertical"])
            if result.engine_only:
                # Nothing but the engine read this line. Whether that is
                # lettering or line art is decided on the quad -- above all on
                # the DETECTOR's own score for it -- and the rule that decided
                # is written into the dump either way.
                keep, why = self.reconcile.engine_only_verdict(
                    result,
                    cells=cells[k],
                    det_score=float(lines[i].score),
                    main=main,
                    thickness=em,
                    pitch=pitch,
                    neighbours=neighbours[i],
                )
                result.notes.append(why)
                if not keep:
                    # Low confidence alone does not keep such a line off the
                    # page: the layout takes a doubted quad that is collinear
                    # with a column for a piece of it. So the line keeps its
                    # CTC text (usually none, and then the layout drops it);
                    # the engine's reads stay in the dump.
                    result.text = self.layout.normalize_text(lines[i].text.strip())
                    result.notes.append("dropped")
                    continue
                # The layout files a line the CTC recognizer doubted under
                # noise; one the detector vouches for is kept.
                lines[i].text = result.text
                lines[i].conf = max(float(lines[i].conf), self.reconcile.CONFIRMED_CONF)
                continue
            lines[i].text = result.text
        self._trim_repeats(lines, targets, settled, img)
        return ReadPage(
            detected=detected,
            targets=targets,
            settled=settled,
            pitch=pitch,
            engine_seconds=engine_seconds,
        )

    def finish_read(self, read: ReadPage, version: str) -> PageResult:
        """The post stage: lay the merged lines out and build the dump.

        Every byte of this is CPU, so it belongs off the thread holding the
        recognizer. It touches only what ``read`` carries (bar the debug
        ``self.raw`` the layout leaves behind), so a pool of these can run
        while the engine is several pages further on.
        """
        img, lines, info, _first = read.detected
        page, _layout_result, raw = self.finish(lines, info, img, version)
        for i, result in zip(read.targets, read.settled, strict=True):
            raw["lines"][i].update(result.to_json())
        raw["reconcile"] = {
            **self.reconcile.page_summary(read.settled),
            "body_pitch": round(read.pitch, 1),
        }
        return PageResult(
            page=page,
            raw=raw,
            tally=raw["reconcile"],
            doubtful=doubtful_lines(raw),
            engine_seconds=read.engine_seconds,
        )

    def layout_detected(self, detected: DetectedPage, version: str) -> PageResult:
        """The whole tail of a reconciled page, for a single-page caller."""
        return self.finish_read(self.engine_read(detected), version)

    def _read(
        self,
        img: Any,
        blocks: Sequence[dict[str, Any]],
        cells: Sequence[int],
        crop_fn: CropFn,
        which: Iterable[int],
    ) -> dict[int, str]:
        """The engine's read of lines ``which``: the text per line, in one batch.

        A line cropped as several chunks (hayai-nova's long lines) is their texts
        joined. Each crop's token budget follows from its line's glyph room.
        """
        crops: list[Any] = []
        owners: list[int] = []
        for k in which:
            for crop in crop_fn(img, blocks[k], 0):
                crops.append(crop)
                owners.append(k)
        caps = [self.reconcile.token_cap(cells[k]) for k in owners]
        if not crops:
            texts: list[str] = []
        elif getattr(self.recognize, "token_caps", False):
            texts = self.recognize(crops, max_tokens=caps)  # type: ignore[call-arg]
        else:
            texts = self.recognize(crops)
        if len(texts) != len(crops):
            raise RuntimeError(f"recognizer returned {len(texts)} strings for {len(crops)} crops")
        text_by_line: dict[int, str] = {}
        for k, text in zip(owners, texts, strict=True):
            text_by_line[k] = text_by_line.get(k, "") + text
        return text_by_line

    @staticmethod
    def _line_block(line: Any) -> dict[str, Any]:
        """One detected line in the shape the crop functions take."""
        quad = [[float(x), float(y)] for x, y in line.quad]
        return {"lines": [quad], "vertical": bool(line.vertical)}

    def _trim_repeats(
        self, lines: Sequence[Any], targets: Sequence[int], settled: Sequence[Any], img: Any
    ) -> None:
        """Pieces of one column that stayed apart must not both carry the seam.

        ``join_lines`` reads such pieces as one line when that read holds up;
        when it does not they stay two quads, which overlap along the column
        (the detector's unclip grows both over the cut), and each crop adds
        its margin. The engine reads whatever ink its crop holds, so the
        later piece starts with the glyphs the earlier one ended on. Only as
        many glyphs as that shared stretch has room for are ever taken off.
        """
        height, width = img.shape[0], img.shape[1]
        raw = self.ppocr.page_to_json(lines, width, height, detector={})
        by_line = dict(zip(targets, settled, strict=True))
        for group in self.layout.column_pieces(raw):
            vertical = bool(lines[group[0]].vertical)
            axis = 1 if vertical else 0
            spans = sorted(
                (
                    min(float(p[axis]) for p in lines[i].quad),
                    max(float(p[axis]) for p in lines[i].quad),
                    i,
                )
                for i in group
            )
            for (_, end, before), (start, _, after) in zip(spans, spans[1:], strict=False):
                if before not in by_line or after not in by_line:
                    continue
                quad = [[float(x), float(y)] for x, y in lines[after].quad]
                _main, em = quad_extents(quad, vertical)
                shared = (end - start) + 2 * line_margin_px(quad)
                if shared <= 0 or em <= 0:
                    continue
                room = int(shared / em + 0.5) + 1
                count = self.reconcile.overlap_repeat(lines[before].text, lines[after].text, room)
                if count:
                    lines[after].text = lines[after].text.strip()[count:]
                    by_line[after].text = lines[after].text
                    by_line[after].notes.append("seam")


REVIEW_FILE = "review.json"


def doubtful_lines(raw: dict[str, Any]) -> list[dict[str, Any]]:
    """Lines of a reconciled page an editor should look at first.

    Two independent recognizers differing on a glyph is the best cue there is
    for a wrong kanji (bench: 竈 came out 竜 from the engine and 籠 from the CTC
    read -- both wrong, and the merged line gives no sign of it). A line that
    fell back to the CTC read after a runaway counts too. Only lines that
    carry text; ruby and dropped lines were never compared.
    """
    out: list[dict[str, Any]] = []
    for index, line in enumerate(raw.get("lines") or []):
        agreement = line.get("agreement")
        fell_back = line.get("source") == "ctc" and "runaway" in (line.get("notes") or [])
        if not line.get("text") or not (fell_back or (agreement is not None and agreement < 1)):
            continue
        entry = {key: line[key] for key in ("quad", "text", "ctc", "vlm") if key in line}
        if "vlm_second" in line:
            entry["vlm_second"] = line["vlm_second"]
        out.append({"line": index, **entry, "agreement": agreement, "notes": line.get("notes")})
    return out


def load_recognizer(
    engine: str,
    *,
    fold: bool = True,
    patches: int = DEFAULT_PATCH_BUDGET,
    device: str | None = None,
    precision: str = DEFAULT_PRECISION_MODE,
    pick: str | None = None,
    pick_why: str = "",
) -> Recognizer:
    """The engine's recognizer; ``fold=False`` keeps the text as generated (see Paddle's).

    ``patches`` reaches only the engines whose recognizer has a patch budget
    (``PATCH_BUDGET_ENGINES``); every other engine ignores it. ``precision``
    is the row's mode and ``pick``/``pick_why`` a benchmark's choice
    (:func:`resolve_precision`); each recognizer reports
    the one it resolved to as ``precision``, for the sidecar.

    Every recognizer returned here carries a ``repos`` dict of the Hugging
    Face repos it loaded and the commit each was pinned to, which is what the
    sidecar's ``ocr_engine.weights`` is built from.
    """
    if engine == "hayai-nova":
        return HayaiNovaRecognizer(
            patches, fold=fold, device=device, precision=precision, pick=pick, pick_why=pick_why
        )
    if engine == "paddle-manga":
        return PaddleMangaRecognizer(
            fold=fold, device=device, precision=precision, pick=pick, pick_why=pick_why
        )
    raise ValueError(f"unsupported engine: {engine}")


def recognizer_takes_token_caps(engine: str) -> bool:
    """Does this engine's recognizer take per-crop ``max_tokens``?

    A CLASS fact, so it can be answered while the model is still loading --
    :class:`DeferredRecognizer` has to expose ``token_caps`` before there is
    an instance to ask.
    """
    classes = {"hayai-nova": HayaiNovaRecognizer, "paddle-manga": PaddleMangaRecognizer}
    return bool(getattr(classes.get(engine), "token_caps", False))


class DeferredRecognizer:
    """A recognizer that is still loading, standing in for one that is not.

    Loading the VLM is ~10 s of import, weights and a LoRA merge, and it used
    to happen BEFORE the pipeline existed: nothing detected, nothing decoded,
    the card idle, and on the adapter road the whole volume's detection had
    already run serially before that. Now the load runs on a thread of its
    own from t=0, the pipeline starts immediately, and the first page to reach
    the engine stage is the one that waits -- by which time the detect stage
    has filled its queue (:data:`LOAD_WINDOW_SLOTS`).

    Everything a caller needs BEFORE the model exists is answered without it:
    ``token_caps`` is a class fact. Everything that needs the model itself --
    a call, ``repos`` for the sidecar -- waits on the load and re-raises what
    it raised, so a broken install fails the run with its real error instead
    of quietly blanking every page.
    """

    def __init__(self, engine: str, load: Callable[[], Recognizer]) -> None:
        self.engine = engine
        self.token_caps = recognizer_takes_token_caps(engine)
        # Empty until the load lands: ``ReconciledPageReader`` copies this at
        # construction, and the sidecar's weights are collected from
        # :meth:`repos` after the run instead.
        self.repos: dict[str, str] = {}
        # What the recognizer resolved ``--precision`` to, once it has loaded.
        self.precision: str | None = None
        self.seconds = 0.0
        self.error: BaseException | None = None
        self._value: Recognizer | None = None
        self._done = threading.Event()
        self._thread = threading.Thread(target=self._load, args=(load,), name="ocr-load", daemon=True)
        self._thread.start()

    def _load(self, load: Callable[[], Recognizer]) -> None:
        started = time.time()
        try:
            value = load()
        except BaseException as e:  # noqa: BLE001 - re-raised in the caller's thread
            self.error = e
        else:
            self._value = value
            self.repos = dict(getattr(value, "repos", {}))
            self.precision = getattr(value, "precision", None)
            log(
                f"[runner] {self.engine} ready after {time.time() - started:.1f}s "
                "(loaded while the pipeline ran)"
            )
        finally:
            self.seconds = time.time() - started
            self._done.set()

    def wait(self) -> Recognizer:
        """The recognizer, once it is there; what the load raised if it is not."""
        self._done.wait()
        if self.error is not None:
            raise self.error
        if self._value is None:  # pragma: no cover - set together with error
            raise RuntimeError(f"recognizer {self.engine} never loaded")
        return self._value

    @property
    def loaded(self) -> bool:
        return self._done.is_set() and self.error is None

    def weights(self) -> dict[str, str]:
        """The repos it resolved, waiting for the load. For the sidecar."""
        self.wait()
        return dict(self.repos)

    def __call__(self, crops: list[Any], max_tokens: Sequence[int] | None = None) -> list[str]:
        recognize = self.wait()
        if max_tokens is None:
            return recognize(crops)
        return recognize(crops, max_tokens=max_tokens)  # type: ignore[call-arg]


# Most copies of a recognizer ``--stage-workers engine=N`` loads on one card.
# Each is a process with its own CUDA context and model (~1.6 GB of VRAM for
# hayai-nova 512 on an RTX 4090), and tower measured 14.0 pages/s at three and
# 14.3 at four: past a handful, a copy buys nothing and costs a card's memory.
MAX_ENGINE_COPIES = 8


class RecognizerPool:
    """``copies`` recognizers of one engine on one card, one per engine worker.

    One copy of a GPU recognizer leaves most of the card idle: hayai-nova
    decodes a batch one token at a time through every layer, thousands of
    small kernels a page, and the card finishes each long before the next
    arrives. Several copies, each driven by its own engine worker, fill those
    gaps -- in the session, each copy is an :class:`EngineProcess` (copies in
    ONE process measured no better than two: see there).

    The copies load one after another (two ``trust_remote_code`` loads racing
    would build the same dynamic module twice), and each joins the pool the
    moment it lands, so the first page waits for ONE load, as it does with a
    single recognizer. A call borrows whichever copy is free and gives it
    back; a copy is never called by two threads at once. A copy after the
    first that fails to load is logged and the pool runs with the ones it
    has; a first copy that fails fails the run, exactly like
    :class:`DeferredRecognizer`. A copy whose process ends mid-run is dropped
    (that page fails); when the last one goes, the run ends with its error.
    """

    def __init__(self, engine: str, load: Callable[[], Recognizer], copies: int) -> None:
        self.engine = engine
        self.copies = max(1, copies)
        self.token_caps = recognizer_takes_token_caps(engine)
        self.repos: dict[str, str] = {}
        # What the copies resolved ``--precision`` to (every copy loads the same).
        self.precision: str | None = None
        self.seconds = 0.0
        self.error: BaseException | None = None
        self.members: list[Recognizer] = []
        self._live = 0
        self._lock = threading.Lock()
        self._closed = False
        self._free: queue.SimpleQueue[Recognizer | None] = queue.SimpleQueue()
        self._first = threading.Event()
        self._done = threading.Event()
        self._thread = threading.Thread(target=self._load, args=(load,), name="ocr-load", daemon=True)
        self._thread.start()

    def _load(self, load: Callable[[], Recognizer]) -> None:
        started = time.time()
        try:
            for index in range(self.copies):
                if self._closed:
                    return
                try:
                    value = load()
                except BaseException as e:  # noqa: BLE001 - see the class docstring
                    if index == 0:
                        self.error = e
                        self._free.put(None)  # wakes a waiting caller into the error
                        return
                    log(f"[runner] WARN {self.engine} copy {index + 1} did not load ({e}); running {index}")
                    return
                if self._closed:
                    _close_quietly(value)
                    return
                self.members.append(value)
                with self._lock:
                    self._live += 1
                if index == 0:
                    self.repos = dict(getattr(value, "repos", {}))
                    self.precision = getattr(value, "precision", None)
                    self._first.set()
                self._free.put(value)
                log(
                    f"[runner] {self.engine} copy {index + 1}/{self.copies} ready after "
                    f"{time.time() - started:.1f}s (loaded while the pipeline ran)"
                )
        finally:
            self.seconds = time.time() - started
            self._first.set()
            self._done.set()

    def wait(self) -> Recognizer:
        """The first copy, once it is there; what its load raised if it is not."""
        self._first.wait()
        if self.error is not None:
            raise self.error
        return self.members[0]

    @property
    def loaded(self) -> bool:
        return self._first.is_set() and self.error is None

    def weights(self) -> dict[str, str]:
        """The repos it resolved (every copy loads the same), for the sidecar."""
        self.wait()
        return dict(self.repos)

    def __call__(self, crops: list[Any], max_tokens: Sequence[int] | None = None) -> list[str]:
        member = self._free.get()
        if member is None:
            self._free.put(None)
            raise cast("BaseException", self.error)
        try:
            if max_tokens is None:
                texts = member(crops)
            else:
                texts = member(crops, max_tokens=max_tokens)  # type: ignore[call-arg]
        except EngineProcessGone as e:
            with self._lock:
                self._live -= 1
                last = self._live <= 0 and self._done.is_set()
            log(f"[runner] ERROR {e}; {max(self._live, 0)} {self.engine} cop(ies) left")
            if last:
                self.error = e
                self._free.put(None)  # every waiting caller gets the error
            raise
        except BaseException:
            self._free.put(member)
            raise
        self._free.put(member)
        return texts

    def close(self) -> None:
        """Stop every copy that runs in a process of its own (one still loading too)."""
        self._closed = True
        for member in list(self.members):
            _close_quietly(member)


def _close_quietly(member: Any) -> None:
    close = getattr(member, "close", None)
    if close is not None:
        with contextlib.suppress(Exception):
            close()


class EngineProcessGone(RuntimeError):
    """An engine process that ended; its copy is out of the pool for good."""


def _engine_process_main(
    conn: Any, engine: str, load_kwargs: dict[str, Any], load: Callable[..., Recognizer]
) -> None:
    """One recognizer copy in a process of its own: load, say so, then read on request.

    The protocol is pickled messages both ways on one pipe. Out: ``("ready",
    repos, seconds, precision)`` or ``("error", exception)`` once, then ``("ok", texts)``
    or ``("err", exception)`` per request. In: ``(crops, max_tokens)``, or
    ``None`` / the end of the pipe to stop.
    """
    started = time.time()
    try:
        recognize = load(engine, **load_kwargs)
    except BaseException as e:  # noqa: BLE001 - sent to the parent, which raises it
        conn.send(("error", _picklable(e)))
        return
    conn.send(
        (
            "ready",
            dict(getattr(recognize, "repos", {})),
            time.time() - started,
            getattr(recognize, "precision", None),
        )
    )
    while True:
        try:
            request = conn.recv()
        except (EOFError, OSError):
            return
        if request is None:
            return
        crops, max_tokens = request
        try:
            if max_tokens is None:
                texts = recognize(crops)
            else:
                texts = recognize(crops, max_tokens=max_tokens)  # type: ignore[call-arg]
        except BaseException as e:  # noqa: BLE001 - one bad batch is the caller's page
            conn.send(("err", _picklable(e)))
            continue
        conn.send(("ok", texts))


def _picklable(error: BaseException) -> BaseException:
    import pickle  # noqa: PLC0415

    try:
        pickle.loads(pickle.dumps(error))
    except Exception:  # noqa: BLE001
        return RuntimeError(f"{type(error).__name__}: {error}")
    return error


class EngineProcess:
    """A recognizer copy running in a process of its own, called like one in this one.

    Several copies in ONE process do not add up: measured on tower (RTX 4090,
    hayai-nova 512 + ppocr-manga, detect=4, 669 pages) one copy read 9.30-9.45
    pages/s, two 10.71 and three 10.49 with the card 37-41% busy, although the
    interpreter lock was held only 48-63% of the time. The same copies in
    processes of their own read 13.45 and 16.08, the card 64% / 79% busy. So
    each copy of a :class:`RecognizerPool` is one of these: the engine worker
    that borrows it sends the page's crops over a pipe and waits, and the
    model, its CUDA context and its token loop are the child's alone.

    Started with ``spawn`` (a forked child would inherit this process's
    threads and whatever CUDA state it has). The child's stdout is this
    process's fd 1, which a serving runner has already pointed at its log.
    """

    def __init__(
        self,
        engine: str,
        load_kwargs: dict[str, Any],
        index: int = 0,
        *,
        load: Callable[..., Recognizer] | None = None,
    ) -> None:
        # ``load`` is pickled by reference into the child, so it must be a
        # module-level function; :func:`load_recognizer` unless a test says.
        self.engine = engine
        self.index = index
        context = multiprocessing.get_context("spawn")
        self._conn, child = context.Pipe()
        self._process = context.Process(
            target=_engine_process_main,
            args=(child, engine, load_kwargs, load or load_recognizer),
            name=f"ocr-engine-proc-{index}",
            daemon=True,
        )
        self._process.start()
        child.close()
        try:
            message = self._conn.recv()
        except (EOFError, OSError):
            self.close()
            raise EngineProcessGone(
                f"{engine} engine process {index} ended while loading "
                f"(exit {self._process.exitcode})"
            ) from None
        if message[0] == "error":
            self.close()
            raise message[1]
        _, self.repos, self.seconds, self.precision = message
        self.pid = self._process.pid

    def __call__(self, crops: list[Any], max_tokens: Sequence[int] | None = None) -> list[str]:
        try:
            self._conn.send((crops, None if max_tokens is None else list(max_tokens)))
            kind, value = self._conn.recv()
        except (EOFError, OSError) as e:
            raise EngineProcessGone(
                f"{self.engine} engine process {self.index} (pid {self._process.pid}) ended "
                f"(exit {self._process.exitcode})"
            ) from e
        if kind == "err":
            raise value
        return cast("list[str]", value)

    def close(self) -> None:
        with contextlib.suppress(OSError, ValueError):
            self._conn.send(None)
        self._process.join(5.0)
        if self._process.is_alive():
            self._process.kill()
            self._process.join(5.0)
        with contextlib.suppress(OSError):
            self._conn.close()


def load_detector_weights(detect_dir: Path) -> dict[str, str]:
    """What the detector adapter reported loading, for ``ocr_engine.weights``.

    Part of the adapter contract (``detectors/_common.WEIGHTS_FILE``): the
    adapter runs in its own process, so a return value is not available and
    it leaves one small JSON file beside the pages instead. An adapter that
    wrote nothing readable reported nothing, and the sidecar then claims
    nothing -- the same rule the recognizers follow.
    """
    path = detect_dir / DETECTOR_WEIGHTS_FILE
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    if not isinstance(data, dict):
        return {}
    return {str(repo): str(revision) for repo, revision in data.items()}


# ---------------------------------------------------------------------------
# THE DETECTOR, AS A STAGE.
#
# The adapters stay in their own processes -- that boundary is a LICENCE
# boundary (``detectors/README.md``: ``ctd.py`` combines with GPL-3.0 code, and
# nothing in the server or this runner may import it) -- but a process is not
# the same thing as a pass over the volume. It used to be both: ``run_detector``
# ran the adapter over every page and only then was the recognizer even
# constructed, so on the owner's live shape (paddle-manga + ctd) 22.0 s of every
# 47 s volume was detection and model loading with the card doing nothing.
#
# So the adapter now has a SERVE MODE (``detectors/_common.serve_pages``): load
# the model once, then one request a line on stdin -- the image to read and the
# JSON to write, both absolute -- and one reply a line on stdout. A detect
# worker owns one such process and asks it for the page it is holding; the
# pipeline's own queues do the rest. A worker only asks for its next page once
# its last one was ACCEPTED downstream, so a detector that runs ahead of the
# recognizer parks in ``put`` -- which, when the detector is on the same card,
# is how the recognizer gets the FLOPs back.
#
# A served process is told nothing about a VOLUME: every request stands alone,
# so the same process can serve one volume's pages and then the next. Nothing
# uses that yet -- this CLI is handed one volume -- and it is shaped so that
# something can (see :class:`PageJob`).
#
# A detector that dies or wedges must cost its page and nothing else: EOF on
# its stdout fails that page, a per-page timeout kills a wedged one, and a
# bounded number of respawns keeps a crash loop from eating the volume.
# ---------------------------------------------------------------------------

# Must match ``detectors/_common.SERVE_PREFIX``. The adapter is run by path in
# a process that never imports this package, so the constant is duplicated
# rather than shared; a drift shows up as "the detector never became ready",
# which the timeout below turns into an error rather than a hang.
DETECTOR_REPLY_PREFIX = "@@detect "
DETECTOR_REPLY_READY = "ready"
DETECTOR_REPLY_OK = "ok"
DETECTOR_REPLY_FAIL = "fail"
# A reply is ``<kind> <note>\t<page>``: the page LAST, because page names have
# spaces in them ("Some Series 20 - 101.webp") and splitting the reply on
# spaces tore one in half -- which read as a protocol desync and killed a
# healthy detector once a page.
DETECTOR_REPLY_SEP = "\t"

# How long one page may take in a detector subprocess before it is presumed
# wedged and killed. Generous: the slowest measured adapter is ~1 s a page,
# and a box swapping or a cold page cache can be far worse than that without
# being broken.
DETECT_PAGE_TIMEOUT = 300.0
# ...and how long the FIRST answer may take, which includes resolving and
# loading the model (a cold Hugging Face download, on a first run).
DETECT_LOAD_TIMEOUT = 1800.0
DETECT_TIMEOUT_ENV = "MOKURO_OCR_DETECT_TIMEOUT"
# How many times the pool may replace a dead detector process. A model that
# cannot load fails the same way every time, so this is deliberately small: it
# is here for a one-off crash on one page, not to grind through a broken
# install for an hour.
DETECTOR_RESPAWN_LIMIT = 2


class DetectorError(RuntimeError):
    """The detector subprocess could not answer: it died, wedged or desynced."""


class DetectorPageError(RuntimeError):
    """The detector answered, and the answer is that this page failed."""


class DetectorProcess:
    """One detector adapter subprocess in serve mode, and the page it is on.

    One page at a time: the protocol is one request, one reply on a pipe, so
    the width of the detect stage is a number of PROCESSES, not of callers
    into one.

    Nothing is killed to shut it down in the ordinary case -- closing stdin is
    the signal, and it is the same signal the adapter gets when the runner
    dies without a chance to say anything, which is what keeps an orphan from
    outliving a killed parent.
    """

    def __init__(
        self,
        detector: str,
        script: Path,
        *,
        index: int = 0,
        page_timeout: float = DETECT_PAGE_TIMEOUT,
        load_timeout: float = DETECT_LOAD_TIMEOUT,
        device: str = "",
    ) -> None:
        self.detector = detector
        self.script = Path(script)
        self.index = int(index)
        self.page_timeout = float(page_timeout)
        self.load_timeout = float(load_timeout)
        # Where this row said the detector's model goes. Passed to the adapter
        # as ``--device`` (in torch's spelling, which is what an adapter gives
        # its own library); "" is the adapter's own probe, unchanged.
        self.wanted_device = str(device or "")
        self.proc: subprocess.Popen[str] | None = None
        self.device = ""
        # What it reported loading, off the ready line. Not a file in some
        # volume's directory: this process outlives any one volume.
        self.weights: dict[str, str] = {}
        self.ready = False
        self.pages = 0
        self._eof = False
        self._started = 0.0
        self._replies: queue.Queue[str | None] = queue.Queue()
        self._ready_event = threading.Event()
        self._pump: threading.Thread | None = None
        self._lock = threading.Lock()

    # -- lifetime ----------------------------------------------------------

    def start(self) -> None:
        """Spawn it. Returns as soon as the process exists: the model loads
        in it while this process gets on with loading the recognizer."""
        # No --input and no --output-dir: every request carries the image to
        # read and the file to write, so this process is not tied to a volume.
        cmd = [sys.executable, str(self.script), "--serve"]
        if self.wanted_device:
            cmd += ["--device", torch_device(self.wanted_device)]
        self._started = time.monotonic()
        self.proc = subprocess.Popen(  # noqa: S603
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        self._pump = threading.Thread(
            target=self._pump_stdout,
            name=f"ocr-detect-{self.detector}-{self.index}",
            daemon=True,
        )
        self._pump.start()

    def _pump_stdout(self) -> None:
        """Split the child's one output stream into replies and log lines.

        Everything that is not a reply is the adapter's own logging (or a
        library's), and it belongs in this volume's log exactly as it did when
        the adapter inherited the runner's stdout. A reply is found ANYWHERE in
        the line, so a library writing a partial line to stderr cannot eat one.
        """
        stream = self.proc.stdout if self.proc is not None else None
        try:
            for line in stream or ():
                at = line.find(DETECTOR_REPLY_PREFIX)
                if at < 0:
                    log(line.rstrip("\n"))
                    continue
                if at > 0:
                    log(line[:at].rstrip("\n"))
                # rstrip, not strip: everything after the separator is a page
                # path, verbatim.
                reply = line[at + len(DETECTOR_REPLY_PREFIX) :].rstrip("\r\n")
                kind, _, rest = reply.partition(" ")
                if kind == DETECTOR_REPLY_READY:
                    device, _, weights = rest.partition(DETECTOR_REPLY_SEP)
                    self.device = device.strip()
                    self.weights = _detector_weights(weights)
                    self.ready = True
                    log(
                        f"[runner] detector {self.detector}#{self.index} ready on "
                        f"{self.device or 'cpu'} after {time.monotonic() - self._started:.1f}s"
                    )
                    if self.wanted_device and not _same_device(
                        self.device, self.wanted_device
                    ):
                        # Asked for one place, loaded in another: the numbers
                        # this run produces are not about the placement that
                        # was requested, and silence would hide that.
                        log(
                            f"[runner] WARNING detector {self.detector}#{self.index} was "
                            f"asked for {self.wanted_device} and reports "
                            f"{self.device or 'cpu'}"
                        )
                    self._ready_event.set()
                    continue
                self._replies.put(reply)
        except (OSError, ValueError):  # the pipe went away under us
            pass
        finally:
            self._eof = True
            self._ready_event.set()
            self._replies.put(None)

    def close(self, timeout: float = 10.0) -> None:
        """Stop it: EOF first, then terminate, then kill. Never leaves a child."""
        proc, self.proc = self.proc, None
        if proc is None:
            return
        with contextlib.suppress(OSError, ValueError):
            if proc.stdin is not None:
                proc.stdin.close()
        for stop in (None, proc.terminate, proc.kill):
            if stop is not None:
                stop()
            try:
                proc.wait(timeout=timeout if stop is None else 5.0)
                break
            except subprocess.TimeoutExpired:
                continue
        with contextlib.suppress(OSError, ValueError):
            if proc.stdout is not None:
                proc.stdout.close()
        if self._pump is not None:
            self._pump.join(timeout=5.0)

    def kill(self) -> None:
        """Stop it NOW, for a process that is wedged inside a page."""
        proc = self.proc
        if proc is not None:
            with contextlib.suppress(OSError):
                proc.kill()
        self.close(timeout=5.0)

    # -- one page ----------------------------------------------------------

    def detect(self, image: Path, destination: Path) -> str:
        """Box one page and write its JSON where told; returns the progress note.

        Both paths are absolute and travel WITH the request, so the same
        process can be asked for a page of one volume and then a page of the
        next -- which is what lets a pipeline outlive the volume that opened
        it.

        :class:`DetectorPageError` means the page failed and the process is
        fine; :class:`DetectorError` means the process is not, and the caller
        retires it.
        """
        page = str(Path(image).resolve())
        request = f"{page}{DETECTOR_REPLY_SEP}{Path(destination).resolve()}"
        with self._lock:
            proc = self.proc
            if proc is None or proc.stdin is None:
                raise DetectorError(f"detector {self.detector} is not running")
            if not self.ready:
                self._await_ready()
            try:
                proc.stdin.write(request + "\n")
                proc.stdin.flush()
            except (OSError, ValueError) as e:
                raise DetectorError(f"detector {self.detector} closed its input ({e})") from e
            reply = self._await_reply(page)
            self.pages += 1
            return reply

    def await_ready(self) -> None:
        """Block until this process has loaded its model, or raise.

        The same wait :meth:`detect` does before its first page, asked for on
        its own so a SESSION can say it is ready only when it really is.
        """
        self._await_ready()

    def _await_ready(self) -> None:
        self._ready_event.wait(timeout=self.load_timeout)
        if self.ready:
            return
        if self._eof:
            raise DetectorError(
                f"detector {self.detector} exited before it was ready "
                f"(exit code {self.proc.poll() if self.proc else None})"
            )
        raise DetectorError(
            f"detector {self.detector} did not load within {self.load_timeout:.0f}s"
        )

    def _await_reply(self, rel: str) -> str:
        try:
            reply = self._replies.get(timeout=self.page_timeout)
        except queue.Empty:
            raise DetectorError(
                f"detector {self.detector} did not answer for {rel} "
                f"within {self.page_timeout:.0f}s"
            ) from None
        if reply is None:
            raise DetectorError(
                f"detector {self.detector} died on {rel} "
                f"(exit code {self.proc.poll() if self.proc else None})"
            )
        kind, _, rest = reply.partition(" ")
        note, sep, page = rest.partition(DETECTOR_REPLY_SEP)
        if not sep or page != rel:
            raise DetectorError(f"detector {self.detector} answered {reply!r}, not for {rel!r}")
        if kind == DETECTOR_REPLY_OK:
            return note
        if kind == DETECTOR_REPLY_FAIL:
            raise DetectorPageError(note or "detector reported a failure")
        raise DetectorError(f"detector {self.detector} sent {reply!r}")


class DetectorPool:
    """K detector subprocesses, one per worker of the detect stage.

    A worker takes one for the page it is holding and gives it back, exactly
    like :class:`PPOcrPool` -- the stage's pool pulling from its input queue IS
    the distribution, so nothing here has to round-robin anything.

    Every process is spawned at once, before anything else this run loads, so
    K model loads and the recognizer's all overlap instead of queueing.

    A process that dies or wedges is retired and, up to
    :data:`DETECTOR_RESPAWN_LIMIT` times, replaced. The page it was on fails,
    in its own place, through the same ``blank_page`` path a bad image has
    always taken. When the last process is gone, every remaining page fails the
    same way rather than the volume hanging.
    """

    def __init__(
        self,
        detector: str,
        size: int,
        *,
        script: Path | None = None,
        respawns: int = DETECTOR_RESPAWN_LIMIT,
        page_timeout: float | None = None,
        device: str = "",
    ) -> None:
        self.detector = detector
        # Every member of a pool is the same model on the same device: the
        # width is how many copies, the device is where they are.
        self.device = str(device or "")
        self.script = Path(script) if script is not None else self._find_script(detector)
        if not self.script.is_file():
            raise FileNotFoundError(f"detector adapter missing: {self.script}")
        self.size = max(1, int(size))
        self.page_timeout = (
            float(page_timeout)
            if page_timeout is not None
            else float(_env_int(DETECT_TIMEOUT_ENV, int(DETECT_PAGE_TIMEOUT)))
        )
        self._respawns_left = max(0, int(respawns))
        self._lock = threading.Lock()
        # What the members reported loading, kept here as well as on them: a
        # process that is retired or closed must not take the only record of
        # what produced this volume's detections with it.
        self._reported: dict[str, str] = {}
        # Every process this pool owns, idle or leased, so close() can reach
        # the one a worker is inside as well as the ones waiting.
        self._live: list[DetectorProcess] = []
        self._closed = False
        self._idle: queue.LifoQueue[DetectorProcess | None] = queue.LifoQueue()
        for index in range(self.size):
            self._idle.put(self._spawn(index))

    @staticmethod
    def _find_script(detector: str) -> Path:
        return Path(__file__).parent / "detectors" / DETECTOR_SCRIPTS[detector]

    def _spawn(self, index: int) -> DetectorProcess:
        member = DetectorProcess(
            self.detector,
            self.script,
            index=index,
            page_timeout=self.page_timeout,
            device=self.device,
        )
        member.start()
        with self._lock:
            self._live.append(member)
        return member

    @contextlib.contextmanager
    def lease(self) -> Iterator[DetectorProcess]:
        """Borrow a process for one page; retire it if it does not come back well."""
        member = self._idle.get()
        if member is None:  # the pool is empty and staying that way
            self._idle.put(None)
            raise DetectorError(
                f"no {self.detector} detector process left "
                f"(all {self.size} died and {DETECTOR_RESPAWN_LIMIT} respawns are spent)"
            )
        try:
            yield member
        except DetectorPageError:
            self._idle.put(member)  # the page failed; the process is fine
            raise
        except BaseException:
            self._retire(member)
            raise
        else:
            self._idle.put(member)

    def detect(self, image: Path, destination: Path) -> str:
        """One page through one of the pool's processes."""
        with self.lease() as member:
            return member.detect(image, destination)

    def wait(self) -> None:
        """Block until every member has loaded its model, or raise."""
        with self._lock:
            members = list(self._live)
        for member in members:
            member.await_ready()

    def resize(self, size: int) -> None:
        """Grow or shrink BETWEEN runs -- never while a page holds a lease.

        Growing spawns the difference and nothing else: a process that is
        already up, with its model loaded, is kept. Shrinking retires the idle
        ones. ``--bench`` widens and narrows this stage a trial, and a pool
        that rebuilt itself each time would spend the whole benchmark loading
        detector models.
        """
        size = max(1, int(size))
        while self.size < size:
            self._idle.put(self._spawn(self.size))
            self.size += 1
        while self.size > size:
            member = self._idle.get()
            if member is None:  # the pool is empty and staying that way
                self._idle.put(None)
                return
            with self._lock:
                self._reported.update(member.weights)
                if member in self._live:
                    self._live.remove(member)
            member.close()
            self.size -= 1

    @property
    def weights(self) -> dict[str, str]:
        """What the members reported loading, for the sidecar. See the ready line.

        Survives their teardown: the sidecar is written after the pipeline
        and its detectors have been shut down.
        """
        with self._lock:
            merged = dict(self._reported)
            for member in self._live:
                merged.update(member.weights)
            return merged

    def _retire(self, member: DetectorProcess) -> None:
        member.kill()
        with self._lock:
            self._reported.update(member.weights)
            if member in self._live:
                self._live.remove(member)
            replace = self._respawns_left > 0 and not self._closed
            if replace:
                self._respawns_left -= 1
            left, empty = self._respawns_left, not self._live
        if replace:
            log(
                f"[runner] WARN detector {self.detector}#{member.index} died; "
                f"starting another ({left} replacement(s) left)"
            )
            self._idle.put(self._spawn(member.index))
            return
        log(
            f"[runner] WARN detector {self.detector}#{member.index} died and will not "
            "be replaced; the pages it would have read will fail"
        )
        if empty:
            self._idle.put(None)  # wake every waiter with the bad news, forever

    @property
    def devices(self) -> list[str]:
        """What each member reported it is running on, once it is up."""
        with self._lock:
            return [m.device for m in self._live]

    def close(self) -> None:
        """Stop every process. Idempotent, and never leaves one behind."""
        with self._lock:
            if self._closed:
                return
            self._closed = True
            members, self._live = list(self._live), []
            for member in members:
                self._reported.update(member.weights)
        self._idle.put(None)
        for member in members:
            member.close()


def _detector_weights(raw: str) -> dict[str, str]:
    """The weights an adapter reported on its ready line, or nothing."""
    try:
        data = json.loads(raw) if raw.strip() else {}
    except ValueError:
        return {}
    return {str(k): str(v) for k, v in data.items()} if isinstance(data, dict) else {}


# ---------------------------------------------------------------------------
# A SESSION AND A VOLUME. The pipeline above is opened for an (engine,
# detector, pool widths) and nothing else; a VOLUME is a stream of
# pages with somewhere to put a sidecar. Those are two different lifetimes,
# and this is where they are kept apart.
#
# Why they were ever one: the runner was invoked once per volume, so opening
# the models and reading the volume were the same function. Measured, that
# cost ~10.5 s of import and model load per (volume, generation) -- longer
# than a fast engine spends READING a small volume. So the pipeline is opened
# once per SESSION (:class:`OpenPipeline`) and volumes stream through it
# (:class:`Session`), and the single-volume CLI is one session with one volume
# in it -- literally the same code, so what a session writes and what the CLI
# writes cannot drift.
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# THE SERVED ENGINE. An engine that IS a process: pages in on its stdin, one
# page of JSON out on its stdout, ONE model load for the whole session.
#
# It is the same shape as the detector adapters -- a JSON-line protocol over a
# pipe, process lifetime = pipe lifetime -- and it exists for the same reason
# the sessions do: a fast engine can spend longer starting up and loading its
# model than it spends reading the volume. mokuro measured ~10.5 s of import
# and model load against ~0.04 s a page.
#
# THREE THINGS MAKE IT MORE THAN A PIPE.
#
# * ORDER. The engine takes a volume's pages strictly in order from 0 and is
#   told where the volume ends, because that is what lets it form the OCR
#   batches the single-volume CLI forms and write the same page. The stage
#   that spools the pages is a POOL, so they arrive here out of order: the
#   sender reassembles the source's numbering (``PageJob.seq``) and is the
#   only thing that writes to the process.
# * THE WINDOW. The engine says on its ready line how many pages it can hold.
#   Sent minus answered never exceeds it -- that bound IS the queue on its
#   input side, and it is what keeps the model fed without letting a 300-page
#   volume into it at once.
# * FAILURE IS TWO THINGS. A page it cannot read is that page's failure and
#   the volume carries on (the sink blanks it, as it does on every road). The
#   process GOING is not a page's failure at all: every page in flight is lost
#   and the session is over, so ``error`` is set and the driver ends the run
#   with it rather than writing a volume of blank pages.
# ---------------------------------------------------------------------------

# How long to wait for the ``ready`` line. It covers a model load, and on a
# cold Hugging Face cache that is a download.
SERVE_READY_TIMEOUT = 1800.0
# How long a close may wait for the process to drain its last pages and exit.
SERVE_CLOSE_TIMEOUT = 120.0


class ServedPageError(RuntimeError):
    """One page the serve process could not read (its ``page_failed``)."""


class ServedEngineError(RuntimeError):
    """The serve process is not there: it never came up, or it is gone."""


class _ServedPage:
    """One page handed to the serve process, and what came back for it."""

    __slots__ = ("seq", "path", "owned", "index", "_done", "result", "error")

    def __init__(self, seq: int, path: Path | None, *, owned: bool) -> None:
        self.seq = seq
        self.path = path
        # Ours to delete once the answer is in: a page spooled out of an
        # archive is, a page that was already a file on disk is not.
        self.owned = owned
        self.index = -1
        self._done = threading.Event()
        self.result: dict[str, Any] | None = None
        self.error: BaseException | None = None

    def settle(self, *, result: dict[str, Any] | None = None, error: BaseException | None = None) -> None:
        self.result = result
        self.error = error
        self._done.set()

    def wait(self) -> dict[str, Any]:
        """This page's JSON, or raise what happened to it instead."""
        self._done.wait()
        if self.error is not None:
            raise self.error
        return self.result or {}


class _ServedVolume:
    """A volume the serve process has been told to ``begin``."""

    __slots__ = ("id", "sent", "pages")

    def __init__(self, volume_id: str) -> None:
        self.id = volume_id
        # Pages SENT for this volume: the next one's index, which the engine
        # requires to be exactly this (strictly increasing from 0).
        self.sent = 0
        self.pages: dict[int, _ServedPage] = {}


class ServedEngine:
    """``<python> -m <module>``: one model, volume after volume, page by page.

    The protocol is the fork's (``mokuro/serve.py``): ``begin`` / ``page`` /
    ``end`` / ``quit`` in, ``ready`` / ``page`` / ``page_failed`` /
    ``volume_done`` / ``fatal`` out, one JSON object a line each way. stdout
    is the protocol and nothing else; stderr is the engine's own logging and
    is INHERITED, so it lands wherever the caller's already does (the session
    log's stderr file under ``--serve``, the volume's log under the CLI).

    Two threads: one reading events and settling the pages waiting on them,
    one sending. The caller's stage threads only ever :meth:`submit` and
    :meth:`wait`.
    """

    def __init__(
        self,
        python: str | Path,
        module: str,
        args: Sequence[str] = (),
        *,
        num_workers: int | None = None,
        force_cpu: bool = False,
        env: Mapping[str, str] | None = None,
    ) -> None:
        command = [str(python), "-m", module, *args]
        if num_workers is not None:
            # The engine's OWN pipeline pool, not a pool of ours: on this road
            # the stage's Workers cell means this.
            command += ["--num_workers", str(int(num_workers))]
        if force_cpu:
            command.append("--force_cpu")
        self.command = command
        self._env = dict(env) if env is not None else None
        self.proc: subprocess.Popen[str] | None = None
        self.ready: dict[str, Any] = {}
        self.error: BaseException | None = None
        self.cwd: Path | None = None
        self.window = 1
        self._lock = threading.Lock()
        self._cv = threading.Condition(self._lock)
        self._write_lock = threading.Lock()
        self._ready = threading.Event()
        # seq -> (volume id, is the volume's last page, the page or None for
        # one that will never be sent). Held until the sender's turn reaches
        # it, which is what rebuilds the source's order.
        self._queued: dict[int, tuple[str, bool, _ServedPage | None]] = {}
        self._next = 0
        self._outstanding = 0
        # Volumes the engine has been told to begin, oldest first. It runs
        # them one at a time and answers every page of one before the first
        # page of the next, so the oldest is the one an event belongs to.
        self._volumes: deque[_ServedVolume] = deque()
        self._open: _ServedVolume | None = None
        self._stopping = False
        self._closed = False
        self._pump: threading.Thread | None = None
        self._sender: threading.Thread | None = None

    # -- starting -----------------------------------------------------------

    @property
    def version(self) -> str:
        return str(self.ready.get("version") or "")

    @property
    def device(self) -> str:
        return str(self.ready.get("device") or "")

    def start(self) -> dict[str, Any]:
        """Spawn the process and return its ``ready`` event, or raise."""
        # A CLEAN working directory, always. ``python -m`` puts the working
        # directory first on the import path, so a scratch directory with a
        # .py file in it can shadow a real module inside the engine (a run was
        # lost to a stray transformers.py doing exactly that).
        self.cwd = Path(tempfile.mkdtemp(prefix="mokuro-serve-"))
        log(f"[runner] serve: {' '.join(self.command)}")
        try:
            self.proc = subprocess.Popen(  # noqa: S603
                self.command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                # stderr is INHERITED on purpose: the engine's loguru and tqdm
                # belong in the log the caller already opened.
                cwd=str(self.cwd),
                env=self._env,
                text=True,
                encoding="utf-8",
                errors="replace",
                bufsize=1,
            )
        except OSError as e:
            self._discard_cwd()
            raise ServedEngineError(f"could not start {self.command[0]} -m {self.command[2]}: {e}") from e
        self._pump = threading.Thread(target=self._read_events, name="ocr-serve-events", daemon=True)
        self._pump.start()
        if not self._ready.wait(SERVE_READY_TIMEOUT):
            self.kill()
            raise ServedEngineError(f"no ready line from the serve process in {SERVE_READY_TIMEOUT:.0f}s")
        if self.error is not None:
            raise ServedEngineError(str(self.error))
        self.window = max(1, int(self.ready.get("window") or 1))
        log(
            f"[runner] serve ready: version={self.version} device={self.device} "
            f"num_workers={self.ready.get('num_workers')} window={self.window}"
        )
        self._sender = threading.Thread(target=self._send_pages, name="ocr-serve-send", daemon=True)
        self._sender.start()
        return dict(self.ready)

    # -- the stages' two calls ---------------------------------------------

    def submit(self, job: PageJob, path: Path, *, owned: bool) -> _ServedPage:
        """Hand a spooled page over. Never raises: a dead engine settles it."""
        page = _ServedPage(job.seq, path, owned=owned)
        self._offer(job, page)
        return page

    def skip(self, job: PageJob, error: BaseException) -> _ServedPage:
        """This page never reached the spool: fail it, and let the order past it."""
        page = _ServedPage(job.seq, None, owned=False)
        page.settle(error=error)
        self._offer(job, None)
        return page

    def _offer(self, job: PageJob, page: _ServedPage | None) -> None:
        if job.seq < 0:
            raise RuntimeError(
                "the served road needs the source's page numbering; this PageJob has none"
            )
        with self._cv:
            if self.error is not None or self._closed:
                gone = self.error or ServedEngineError("the serve process is closed")
                if page is not None:
                    self._drop(page)
                    page.settle(error=gone)
                return
            self._queued[job.seq] = (job.volume.id, job.last, page)
            self._cv.notify_all()

    # -- sending ------------------------------------------------------------

    def _send_pages(self) -> None:
        """The source's order, restored, within the engine's window."""
        while True:
            with self._cv:
                while True:
                    if self._stopping or self.error is not None:
                        return
                    entry = self._queued.get(self._next)
                    # A skipped page costs no window: nothing is sent for it.
                    if entry is not None and (entry[2] is None or self._outstanding < self.window):
                        break
                    self._cv.wait()
                del self._queued[self._next]
                self._next += 1
                volume_id, last, page = entry
                ops: list[dict[str, Any]] = []
                if page is not None:
                    if self._open is None:
                        self._open = _ServedVolume(volume_id)
                        self._volumes.append(self._open)
                        ops.append({"op": "begin", "volume": volume_id})
                    volume = self._open
                    page.index = volume.sent
                    volume.sent += 1
                    volume.pages[page.index] = page
                    self._outstanding += 1
                    ops.append({"op": "page", "index": page.index, "path": str(page.path)})
                if last and self._open is not None:
                    ops.append({"op": "end"})
                    self._open = None
            self._write(ops)

    def _write(self, ops: Sequence[Mapping[str, Any]]) -> None:
        if not ops:
            return
        proc = self.proc
        if proc is None or proc.stdin is None:
            self._gone("the serve process has no stdin")
            return
        payload = "".join(json.dumps(dict(op)) + "\n" for op in ops)
        try:
            with self._write_lock:
                proc.stdin.write(payload)
                proc.stdin.flush()
        except (OSError, ValueError) as e:
            self._gone(f"the serve process stopped reading: {e}")

    # -- reading ------------------------------------------------------------

    def _read_events(self) -> None:
        proc = cast("subprocess.Popen[str]", self.proc)
        stream = proc.stdout
        try:
            for line in stream or ():
                text = line.strip()
                if not text:
                    continue
                try:
                    event = json.loads(text)
                except ValueError:
                    log(f"[runner] WARN serve: unreadable line: {text[:200]}")
                    continue
                if not isinstance(event, dict):
                    log(f"[runner] WARN serve: event is not an object: {text[:200]}")
                    continue
                self._event(event)
        except (OSError, ValueError) as e:
            log(f"[runner] WARN serve: its stdout ended: {e}")
        code = proc.poll()
        self._gone(
            f"the serve process ended (exit {code})" if code is not None else "the serve process ended"
        )

    def _event(self, event: Mapping[str, Any]) -> None:
        kind = event.get("event")
        if kind == "ready":
            self.ready = dict(event)
            self._ready.set()
        elif kind == "page":
            self._settle(event.get("index"), result=event.get("result"))
        elif kind == "page_failed":
            self._settle(
                event.get("index"),
                error=ServedPageError(
                    f"{event.get('stage') or 'serve'}: {event.get('error') or 'page failed'}"
                ),
            )
        elif kind == "volume_done":
            self._volume_done(event)
        elif kind == "fatal":
            self._gone(f"the serve process reported a fatal error: {event.get('error')}")
        else:
            log(f"[runner] WARN serve: unknown event {kind!r}")

    def _settle(
        self,
        index: Any,
        *,
        result: dict[str, Any] | None = None,
        error: BaseException | None = None,
    ) -> None:
        with self._cv:
            volume = self._volumes[0] if self._volumes else None
            page = volume.pages.pop(index, None) if volume is not None else None
            if page is None:
                log(f"[runner] WARN serve: an answer for page {index!r} nobody is waiting on")
                return
            self._outstanding = max(0, self._outstanding - 1)
            self._cv.notify_all()
        self._drop(page)
        page.settle(result=result, error=error)

    def _volume_done(self, event: Mapping[str, Any]) -> None:
        with self._cv:
            volume = self._volumes.popleft() if self._volumes else None
            stranded = list(volume.pages.values()) if volume is not None else []
            if volume is not None:
                volume.pages.clear()
            self._outstanding = max(0, self._outstanding - len(stranded))
            self._cv.notify_all()
        log(
            f"[runner] serve volume_done volume={event.get('volume')!r} "
            f"pages={event.get('pages')} failed={event.get('failed')} "
            f"seconds={event.get('seconds')}"
        )
        for page in stranded:
            self._drop(page)
            page.settle(error=ServedPageError("the volume ended without an answer for this page"))

    # -- going away ---------------------------------------------------------

    def _gone(self, message: str, *, crash: bool = True) -> None:
        """Nothing more will come back: settle everything, once."""
        with self._cv:
            if crash and self.error is None:
                self.error = ServedEngineError(message)
            gone = self.error or ServedEngineError(message)
            self._stopping = True
            queued, self._queued = self._queued, {}
            volumes, self._volumes = list(self._volumes), deque()
            self._open = None
            self._outstanding = 0
            self._ready.set()
            self._cv.notify_all()
        stranded = [page for volume in volumes for page in volume.pages.values()]
        stranded += [page for _vid, _last, page in queued.values() if page is not None]
        for page in stranded:
            self._drop(page)
            page.settle(error=gone)

    def close(self, timeout: float = SERVE_CLOSE_TIMEOUT) -> None:
        """End the open volume, quit, and wait. Idempotent."""
        with self._cv:
            if self._closed:
                return
            self._closed = True
            self._stopping = True
            open_volume = self._open is not None
            self._open = None
            self._cv.notify_all()
        if self._sender is not None:
            self._sender.join(timeout=5.0)
        proc = self.proc
        if proc is not None and proc.poll() is None:
            self._write(([{"op": "end"}] if open_volume else []) + [{"op": "quit"}])
            with contextlib.suppress(Exception):
                if proc.stdin is not None:
                    proc.stdin.close()
            try:
                proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                log("[runner] WARN serve: it did not exit; killing it")
                self.kill()
        if self._pump is not None:
            self._pump.join(timeout=10.0)
        self._gone("the session closed before this page was answered", crash=False)
        self._discard_cwd()

    def kill(self) -> None:
        proc = self.proc
        if proc is not None and proc.poll() is None:
            with contextlib.suppress(Exception):
                proc.kill()
            with contextlib.suppress(Exception):
                proc.wait(timeout=10.0)
        self._discard_cwd()

    def _drop(self, page: _ServedPage) -> None:
        """The spool copy is gone the moment its answer is in."""
        if page.owned and page.path is not None:
            with contextlib.suppress(OSError):
                page.path.unlink()

    def _discard_cwd(self) -> None:
        if self.cwd is not None:
            shutil.rmtree(self.cwd, ignore_errors=True)
            self.cwd = None


class ServedPrecisionTarget:
    """A served engine as the benchmark's precision trials see it.

    Its precision is the serve process's ``--fp16``, so switching is a
    restart of that process -- never of this runner, its detector or its
    pools. What it supports is what a card gives the fork: fp16 and fp32.
    """

    def __init__(self, pipe: OpenPipeline) -> None:
        self.pipe = pipe

    @property
    def precision(self) -> str | None:
        return self.pipe.served_precision

    def supported(self) -> frozenset[str]:
        return served_formats(True)

    def set_precision(self, name: str) -> None:
        if name not in (PRECISION_FP16, PRECISION_FP32):
            raise ValueError(f"the served engine does not run {name}")
        if name == self.pipe.served_precision:
            return
        restart = self.pipe._served_restart
        if restart is None:
            raise RuntimeError("the served engine cannot be restarted")
        log(f"[runner] bench: restarting the serve process in {name}")
        restart(name == PRECISION_FP16)

    def release_master(self) -> None:
        """Nothing kept: the process holds only the format it was started in."""


class SessionConfig(NamedTuple):
    """What fixes what a pipeline IS. Nothing here is about a volume."""

    engine: str
    detector: str
    patches: int = DEFAULT_PATCH_BUDGET
    generator: str | None = None
    stage_workers: str | None = None
    queue_capacity: str | None = None
    stage_device: str | None = None
    cpu_workers: int | None = None
    # The interpreter a SERVED engine's process is spawned with: its own
    # environment's, never this one's (``--mokuro-python``).
    mokuro_python: str | None = None
    # ``--precision``: the row's precision MODE (:func:`resolve_precision`),
    # and a benchmark's pick for a balanced/speed mode (``--precision-pick``)
    # with its reason. Every copy of the recognizer loads with them.
    precision: str = DEFAULT_PRECISION_MODE
    precision_pick: str | None = None
    precision_why: str = ""

    @classmethod
    def from_args(cls, args: argparse.Namespace) -> SessionConfig:
        return cls(
            engine=args.engine,
            detector=args.detector,
            patches=args.patches,
            generator=args.generator,
            stage_workers=getattr(args, "stage_workers", None),
            queue_capacity=getattr(args, "queue_capacity", None),
            stage_device=getattr(args, "stage_device", None),
            cpu_workers=getattr(args, "cpu_workers", None),
            mokuro_python=getattr(args, "mokuro_python", None),
            precision=normalize_precision_mode(getattr(args, "precision", None)),
            precision_pick=getattr(args, "precision_pick", None) or None,
            precision_why=str(getattr(args, "precision_why", "") or ""),
        )


class OpenPipeline:
    """The models, the pools and the stage graph for one session.

    Built once, then fed pages -- of one volume from the CLI, of volume after
    volume in ``--serve``. Everything a stage needs about the page it holds
    travels on the :class:`PageJob`, so nothing here closes over a volume;
    that is what lets the pipeline outlive one.

    ``page_cap`` is the single-volume CLI's edge guard and ONLY its: never
    more workers than there are pages to feed them, so a four-page volume
    does not pay for four onnxruntime sessions it will use once. A session
    fed volume after volume has no such number -- sizing it on the first
    volume that happened to arrive would be sizing the whole queue on it --
    so it passes None.
    """

    def __init__(
        self,
        config: SessionConfig,
        *,
        page_cap: int | None = None,
        intro: str = "",
    ) -> None:
        self.config = config
        self.engine = config.engine
        line_engine = config.engine in LINE_ENGINES
        # A served engine detects behind its own command line: naming one of
        # ours would be a guess, and nothing on that road reads it.
        served_engine = config.engine in SERVED_ENGINES
        self.detector = (
            ""
            if served_engine
            else (LINE_ENGINES[config.engine] if line_engine else config.detector)
        )
        budget = f" patches={config.patches}" if config.engine in PATCH_BUDGET_ENGINES else ""
        log(f"[runner] engine={config.engine} detector={self.detector}{budget}{intro}")
        self.road = page_road(config.engine, self.detector)
        # Where this run puts each MODEL: ``--stage-device detect=cpu`` (or
        # ``mokuro=gpu:1`` on the served road, or the environment), with
        # ``auto`` -- card 0 where there is one -- for anything not named.
        # Raises ValueError on a typo, which the caller turns into an exit code
        # or a ``fatal`` event, because a device asked for and silently not
        # honoured is a benchmark measuring a lie.
        self.device_overrides = resolve_stage_devices(config.stage_device, STAGE_DEVICE_ENV)
        normalize_precision_mode(config.precision)  # ValueError on a typo
        # What this run puts on each stage: above all which device holds the
        # detector and which holds the recognizer. A SERVED road is asked with
        # ``gpu=False`` and not probed: nothing of the engine is in THIS
        # process, the engine names its own device on its ready line
        # (:meth:`_fit_to_window` puts that here before anything is built), and
        # the probe is an ``import torch`` -- 15+ seconds of a ROCm build, once
        # a session, to answer a question the engine answers itself. That is
        # only what ``auto`` falls back to: an explicit ``mokuro=cpu`` /
        # ``mokuro=gpu:<n>`` is kept as asked and is what the process is
        # STARTED with. It changes no width either way: the engine stage is
        # device-bound at 1 whatever it sits on.
        # A detector whose card is an onnxruntime execution provider goes on
        # the card only if THIS environment's onnxruntime has one: asked here,
        # once, because torch seeing a card says nothing about it (a CPU-only
        # wheel beside a CUDA torch is an ordinary host). A pin to a card it
        # cannot reach is run on the CPU with one warning rather than dying
        # before ready -- the ppocr pair's rule, for the same reason: the
        # refusal belongs at the edit that asked (the library refuses it where
        # it knows), the placement is reported on ``ready`` as it really is,
        # and a stale pin must not strike the row off this machine every scan.
        self.ort_gpu: bool | None = None
        if any(
            stage_needs_ort_gpu(self.road, key, detector=self.detector, engine=self.engine)
            for key in MODEL_STAGES
        ):
            providers = ort_gpu_providers()
            self.ort_gpu = None if providers is None else bool(providers)
            pinned = self.device_overrides.get(STAGE_DETECT, "")
            if self.ort_gpu is False and device_is_gpu(pinned):
                log(
                    f"[runner] WARN detect was pinned to {pinned}, but this onnxruntime "
                    f"has no GPU execution provider for the {self.detector} detector; "
                    "running it on the CPU"
                )
        self.specs = road_specs(
            self.road,
            detector=self.detector,
            engine=self.engine,
            gpu=False if self.road == ROAD_SERVED else None,
            devices=self.device_overrides,
            ort_gpu=self.ort_gpu,
        )
        forced = resolve_cpu_workers(config.cpu_workers)
        jobs = _env_int(CPU_JOBS_ENV, CPU_DEFAULT_JOBS)
        host_budget = host_worker_budget(jobs=jobs)
        if page_cap is not None:
            host_budget = min(host_budget, page_cap)
            if forced is not None:
                forced = min(forced, page_cap)
        self.forced = forced
        self.host_budget = host_budget
        # Raises ValueError on a typo; the caller turns that into an exit code
        # or a ``fatal`` event.
        self.stage_overrides = resolve_stage_setting(config.stage_workers, STAGE_WORKERS_ENV)
        self.queue_overrides = resolve_stage_setting(config.queue_capacity, QUEUE_CAPACITY_ENV)
        # ``--stage-workers engine=N`` on a card is N copies of the model on
        # it (:class:`RecognizerPool`), so the stage's one-model ceiling
        # becomes N -- asked for by name, never derived.
        self.engine_copies = self._engine_copies()
        if self.engine_copies > 1:
            self.specs = tuple(
                spec._replace(max_workers=self.engine_copies) if spec.key == STAGE_ENGINE else spec
                for spec in self.specs
            )
        self.widths, self.caps = self.plan()
        # ONLY the detect stage leases a ppocr session or a detector process,
        # so only its width decides how many this run builds -- widening
        # ``post`` must not buy four more sessions that nothing ever leases.
        self.detect_width = self._detect_width(self.widths)
        tuned = []
        if forced is not None:
            tuned.append(f"--cpu-workers {forced}")
        if self.stage_overrides:
            tuned.append(
                "--stage-workers " + ",".join(f"{k}={v}" for k, v in self.stage_overrides.items())
            )
        if self.queue_overrides:
            tuned.append(
                "--queue-capacity "
                + ",".join(f"{k}={v}" for k, v in self.queue_overrides.items())
            )
        if self.device_overrides:
            tuned.append(
                "--stage-device " + ",".join(f"{k}={v}" for k, v in self.device_overrides.items())
            )
        if config.precision != DEFAULT_PRECISION_MODE and config.engine in PRECISION_ENGINES:
            tuned.append(f"--precision {config.precision}")
        log(
            f"[runner] pipeline: {self.graph_line()}; budget {host_budget} "
            f"({os.cpu_count()} cores / {jobs} job(s) / {SESSION_THREADS} threads a session)"
            + (f"; set by hand: {'; '.join(tuned)}" if tuned else "")
        )
        # The recognizer, loading on a thread of its own from here on. The
        # pipeline starts without it and the first page to reach the engine
        # stage waits; everything before that stage -- the detector process,
        # the CTC read, the page decode -- runs while it loads.
        self.loader: DeferredRecognizer | RecognizerPool | None = None
        self.detectors: DetectorPool | None = None
        self.reader: Any = None
        self.ppocr: PPOcrPool | None = None
        self.served: ServedEngine | None = None
        # What a served engine computes in, decided at its start (``--fp16``),
        # and how to start it again at another one (the benchmark's trials).
        self.served_precision: str | None = None
        self._served_restart: Callable[[bool], None] | None = None
        try:
            self.runs = self._open_road()
        except BaseException:
            # Half an open pipeline still has detector processes in it. They
            # would exit on their own when this one does (stdin EOF is the
            # signal) -- but a caller that catches this and carries on would
            # be left with K orphaned models on the card.
            self.close()
            raise
        if self.served is not None:
            self._fit_to_window()
        self.pipeline = self.build(self.stage_overrides, self.queue_overrides)

    # -- the plan -----------------------------------------------------------

    def plan(self) -> tuple[list[int], list[int]]:
        """How wide each stage runs and how deep the queue it fills may get."""
        widths = stage_widths(
            self.engine,
            self.road,
            budget=self.host_budget,
            forced=self.forced,
            workers=self.stage_overrides,
            specs=self.specs,
        )
        caps = stage_capacities(
            self.road, widths, capacities=self.queue_overrides, specs=self.specs
        )
        return widths, caps

    def _engine_copies(self) -> int:
        """How many copies of the recognizer this session loads (one unless asked)."""
        asked = self.stage_overrides.get(STAGE_ENGINE, 1)
        if asked <= 1 or not any(spec.key == STAGE_ENGINE for spec in self.specs):
            return 1
        if not device_is_gpu(self.engine_device):
            # On the CPU the pool of threads IS the recognizer's compute;
            # copies there would split it, not add to it.
            return 1
        if asked > MAX_ENGINE_COPIES:
            log(f"[runner] engine={asked} asks for more copies than {MAX_ENGINE_COPIES}; loading {MAX_ENGINE_COPIES}")
            return MAX_ENGINE_COPIES
        return asked

    def _pick_kwargs(self) -> dict[str, Any]:
        """A benchmark's pick for the recognizer's load, only when there is one."""
        if self.config.precision_pick is None:
            return {}
        return {"pick": self.config.precision_pick, "pick_why": self.config.precision_why}

    def _recognizer_loader(self, **load_kwargs: Any) -> DeferredRecognizer | RecognizerPool:
        """The session's recognizer: one in this process, or N engine processes."""
        if self.engine_copies <= 1:
            return DeferredRecognizer(
                self.engine, functools.partial(load_recognizer, self.engine, **load_kwargs)
            )
        log(
            f"[runner] {self.engine}: {self.engine_copies} engine process(es) on {self.engine_device}"
        )
        started = itertools.count()

        def start_one() -> Recognizer:
            return cast("Recognizer", EngineProcess(self.engine, load_kwargs, next(started)))

        return RecognizerPool(self.engine, start_one, self.engine_copies)

    @property
    def engine_device(self) -> str:
        """Where the recognizer loads, resolved (``cpu``/``gpu:<n>``)."""
        return self._stage_device(STAGE_ENGINE)

    @property
    def detect_device(self) -> str:
        """Where the detector's model loads, resolved."""
        return self._stage_device(STAGE_DETECT)

    def _stage_device(self, key: str) -> str:
        return next((spec.device for spec in self.specs if spec.key == key), DEVICE_CPU)

    def _detect_width(self, widths: Sequence[int]) -> int:
        return next(
            (
                max(1, width)
                for spec, width in zip(self.specs, widths, strict=True)
                if spec.key == STAGE_DETECT
            ),
            1,
        )

    def graph_line(self) -> str:
        """The one-line shape of this pipeline, for the log and the protocol."""
        return " -> ".join(
            f"{spec.key} ({spec.device} x{width or 'fused'}, queue {cap})"
            for spec, width, cap in zip(self.specs, self.widths, self.caps, strict=True)
        )

    def stage_workers(self) -> dict[str, int]:
        return {spec.key: int(w) for spec, w in zip(self.specs, self.widths, strict=True)}

    def queue_capacity(self) -> dict[str, int]:
        return {spec.key: int(c) for spec, c in zip(self.specs, self.caps, strict=True)}

    def stage_device(self) -> dict[str, str]:
        """Where each model-bearing stage is ACTUALLY running, resolved.

        What the ``ready``/``bench_ready`` events report, so a server never has
        to re-derive a placement it asked for: if ``auto`` found no card, this
        says ``cpu`` and the number beside it means what it says.
        """
        return {spec.key: spec.device for spec in self.specs if spec.key in MODEL_STAGES}

    # -- the road's callables ----------------------------------------------

    def _open_road(self) -> tuple[Callable[[Any, Any], Any], ...]:
        """Load the models and bind this road's stages to them."""
        if self.road == ROAD_SERVED:
            return self._open_served_road()
        if self.road in (ROAD_LINE, ROAD_RECONCILED):
            return self._open_ppocr_road()
        return self._open_adapter_road()

    def _open_served_road(self) -> tuple[Callable[[Any, Any], Any], ...]:
        """Start the engine's own process and bind the three stages to it.

        ``feed`` spools and SUBMITS; ``mokuro`` waits. The stage that waits is
        the one the graph calls device-bound, so the time the engine owes us
        is counted against the engine rather than against the assembly after
        it -- and because it is not the stage that sends, a width of one there
        still leaves the process as full as its window allows.
        """
        module, engine_args = SERVED_ENGINES[self.engine]
        python = self.config.mokuro_python
        if not python:
            raise ValueError(
                f"engine {self.engine!r} runs in an environment of its own; "
                "--mokuro-python must name that environment's interpreter"
            )
        # The Device cell of the ``mokuro`` stage, as the process takes it:
        # the same translation the one-volume CLI path makes, from the same
        # function, so the row's select means one thing on either path.
        #
        # From what the ROW ASKED, never from the resolved spec: this road is
        # planned with ``gpu=False`` because nothing of the engine is in this
        # process, so an unasked ``auto`` resolves to ``cpu`` as a PLACEHOLDER
        # until the ready line corrects it -- and turning that placeholder
        # into ``--force_cpu`` would make the guess true by forcing it.
        # ``auto`` means "the fork picks", which is no flag at all.
        placement = mokuro_placement(self.device_overrides.get(STAGE_MOKURO, DEVICE_AUTO))
        cap = served_thread_env(placement.force_cpu, os.environ)
        # The row's mode, resolved for the process before it starts -- its
        # one switch (``--fp16``) is a flag at the start. A card supports
        # fp16 and fp32 there (the fork has no bf16); the CPU fp32 alone.
        # Where it really landed is only known from the ready line, which is
        # checked against the mode again below.
        asked = self.config.precision
        pick, pick_why = self.config.precision_pick, self.config.precision_why
        guess, _why = resolve_precision(
            self.engine,
            asked,
            supported=served_formats(not placement.force_cpu),
            pick=pick,
            pick_why=pick_why,
        )
        if guess == PRECISION_FP16:
            engine_args = (*engine_args, SERVED_FP16_FLAG)

        def start(thread_env: Mapping[str, str]) -> ServedEngine:
            extra = {**placement.env, **thread_env}
            engine = ServedEngine(
                python,
                module,
                engine_args,
                # The Workers cell of the ``mokuro`` stage IS this, and only an
                # explicit one: the derived width of a device-bound stage is 1,
                # and passing that on as the engine's pipeline width would be
                # saying something nobody asked for.
                num_workers=self.stage_overrides.get(STAGE_MOKURO),
                force_cpu=placement.force_cpu,
                # The card is chosen by hiding the others, so the process needs
                # OUR environment plus those two variables -- never the two
                # alone, which would take its PATH and its venv away with them.
                env={**os.environ, **extra} if extra else None,
            )
            self.served = engine
            engine.start()
            return engine

        set_by = next((name for name in TORCH_THREAD_ENV if os.environ.get(name)), None)
        if cap:
            log(f"[runner] serve torch CPU threads: {' '.join(f'{k}={v}' for k, v in cap.items())}")
        elif set_by is not None:
            log(f"[runner] serve torch CPU threads: {set_by}={os.environ[set_by]} (as set)")
        env_used: Mapping[str, str] = cap
        served = start(cap)
        if cap and served.device in ("", DEVICE_CPU):
            # ``auto`` with no card (or a card the fork could not use): on the
            # CPU the pool is the engine's compute, so the cap must go. Only
            # the ready line knows where the model went, and no page has been
            # sent yet, so this costs one more model load and nothing else.
            log(
                f"[runner] serve: the engine loaded on {served.device or DEVICE_CPU}; "
                "restarting it on torch's default CPU pool"
            )
            served.close()
            env_used = {}
            served = start({})
        on_card = served.device not in ("", DEVICE_CPU)
        try:
            precision, why = resolve_precision(
                self.engine, asked, supported=served_formats(on_card), pick=pick,
                pick_why=pick_why,
            )
        except PrecisionUnavailable:
            served.close()
            raise
        if precision == PRECISION_FP16 and SERVED_FP16_FLAG not in engine_args:
            # Asked on a card it was not expected on (``auto`` placement):
            # the flag has to be there from the start.
            log(f"[runner] serve: the engine is on {served.device}; restarting it in fp16")
            served.close()
            engine_args = (*engine_args, SERVED_FP16_FLAG)
            served = start(cap)
        self.served_precision = precision
        log(f"[runner] {self.engine} precision: {precision} ({why})")

        def restart(fp16: bool) -> None:
            """The process again, with or without ``--fp16`` (the benchmark's trials)."""
            nonlocal engine_args
            base = tuple(arg for arg in engine_args if arg != SERVED_FP16_FLAG)
            engine_args = (*base, SERVED_FP16_FLAG) if fp16 else base
            cast("ServedEngine", self.served).close()
            start(env_used)
            self.served_precision = PRECISION_FP16 if fp16 else PRECISION_FP32

        self._served_restart = restart

        def feed_stage(job: PageJob, _payload: Any) -> _ServedPage:
            # The CURRENT process: a benchmark's precision trial may have
            # restarted it between two passes.
            served = cast("ServedEngine", self.served)
            if job.blob is None:
                # Already a file on disk (the single-volume CLI, the
                # benchmark's sample): the engine reads it where it is and
                # nothing of ours deletes it.
                return served.submit(job, job.image, owned=False)
            target = job.volume.input_dir / job.rel
            try:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(job.blob)
            except BaseException as e:  # noqa: BLE001 - this page's failure, not the volume's
                # The engine reads a volume's pages in the source's order, so
                # a page that never reaches the spool must still take its
                # turn: ``skip`` lets the order past it and fails it alone.
                return served.skip(job, e)
            return served.submit(job, target, owned=True)

        def mokuro_stage(job: PageJob, page: Any) -> dict[str, Any]:
            result = cast("_ServedPage", page).wait()
            if not isinstance(result, dict) or "blocks" not in result:
                raise RuntimeError(f"the serve process answered {job.rel} with no page")
            return result

        def post_stage(_job: PageJob, result: Any) -> PageResult:
            # The engine's page dict IS the page the sidecar carries: the
            # volume's assembly adds ``img_path`` to it and nothing else.
            return PageResult(page=cast("dict[str, Any]", result))

        return (feed_stage, mokuro_stage, post_stage)

    def _fit_to_window(self) -> None:
        """Size the queue in front of the engine to what the engine can hold.

        The pages in flight INSIDE the serve process are the ones waiting in
        that queue, so its capacity is the engine's own ``window`` -- never
        more (the protocol forbids it) and never less (see
        :meth:`_served_capacities`: the engine may hold a whole window of
        pages before it answers any of them).

        The device the graph GUESSED is replaced by the one the engine says it
        really got, which is the only honest answer: the process loaded the
        model, not this one. A device that was ASKED for is not a guess and
        stays as asked -- the index is ours and the process cannot see it
        (``CUDA_VISIBLE_DEVICES`` makes the chosen card its card 0) -- but the
        two disagreeing about CPU-or-card is worth a line in the log, the same
        way a detector adapter landing somewhere else is.
        """
        served = cast("ServedEngine", self.served)
        window = max(1, int(served.window))
        got_gpu = served.device not in ("", DEVICE_CPU)
        asked = self.device_overrides.get(STAGE_MOKURO, DEVICE_AUTO)
        if asked == DEVICE_AUTO:
            device = f"{GPU_DEVICE_PREFIX}0" if got_gpu else DEVICE_CPU
        else:
            device = resolve_device(asked, gpu=got_gpu)
            if device_is_gpu(device) != got_gpu:
                log(
                    f"[runner] WARN serve: asked for {device}, but the engine "
                    f"loaded on {served.device or DEVICE_CPU}"
                )
        self.specs = tuple(
            spec._replace(device=device) if spec.key == STAGE_MOKURO else spec
            for spec in self.specs
        )
        self.queue_overrides = self._served_capacities(self.queue_overrides)
        self.widths, self.caps = self.plan()
        log(f"[runner] pipeline: {self.graph_line()}; engine window {window} page(s)")

    def _open_ppocr_road(self) -> tuple[Callable[[Any, Any], Any], ...]:
        line_engine = self.engine in LINE_ENGINES
        reader: PPOcrPageReader
        if line_engine:
            reader = PPOcrPageReader()
        else:
            self.loader = self._recognizer_loader(
                fold=False,
                patches=self.config.patches,
                device=self.engine_device,
                precision=self.config.precision,
                **self._pick_kwargs(),
            )
            crop_mode, _margin = select_crop(self.engine, self.detector)
            quads = crop_mode == "quad"
            reader = ReconciledPageReader(
                self.loader,
                make_quad_crop_fn() if quads else make_line_crop_fn(),
                second_crop_fn=make_quad_crop_fn(SECOND_MARGIN_EM) if quads else None,
            )
        self.reader = reader
        pool = PPOcrPool(reader.engine, reader.clone_engine, self.detect_width)
        self.ppocr = pool
        log(
            f"[runner] ppocr sessions: {pool.size} x {reader.engine.threads} threads "
            f"(detector and CTC recognizer both)"
        )

        def detect_stage(job: PageJob, _payload: Any) -> DetectedPage:
            with pool.lease() as engine:
                return reader.detect_page(job.decode(), engine)

        if self.road == ROAD_LINE:

            def layout_stage(job: PageJob, detected: Any) -> PageResult:
                result = reader.layout_detected(detected, MOKURO_FORMAT_VERSION)
                # What the models saw before layout, beside the other
                # detectors' dumps and outside the progress-counted cache.
                dump_json(result.raw, job.dump)
                return result

            return (detect_stage, layout_stage)

        reconciled = cast("ReconciledPageReader", reader)

        def engine_stage(_job: PageJob, detected: Any) -> ReadPage:
            return reconciled.engine_read(detected)

        def post_stage(job: PageJob, read: Any) -> PageResult:
            result = reconciled.finish_read(read, MOKURO_FORMAT_VERSION)
            dump_json(result.raw, job.dump)
            return result

        return (detect_stage, engine_stage, post_stage)

    def _open_adapter_road(self) -> tuple[Callable[[Any, Any], Any], ...]:
        # BOTH model loads start here, before anything else, and overlap: K
        # detector subprocesses loading their model, and the recognizer
        # loading on a thread. Neither blocks the other and neither blocks the
        # pipeline; the first page waits on whichever stage it reaches first.
        detectors = open_detectors(
            self.detector, workers=self.detect_width, device=self.detect_device
        )
        self.detectors = detectors
        self.loader = self._recognizer_loader(
            patches=self.config.patches,
            device=self.engine_device,
            precision=self.config.precision,
            **self._pick_kwargs(),
        )
        loader = self.loader
        crop_mode, margin = select_crop(self.engine, self.detector)
        crop_fn = make_line_crop_fn() if crop_mode == "line" else make_upright_crop_fn(margin)

        def detect_stage(job: PageJob, _payload: Any) -> tuple[Any, dict[str, Any]]:
            """One page through this worker's detector process, then read back.

            The worker holds the page until its detection is written and then
            decodes it, so the process is free for the next page only once
            this one has been ACCEPTED by the queue in front of the engine --
            which is the backpressure: a detector that runs ahead parks here.

            Everything it needs is on the job, not in this closure: the page
            it is reading and the file it is writing travel with the request.
            The POOL is read off the pipeline each page rather than captured,
            so a benchmark trying the detector on another device can swap it
            (:meth:`move_detect`) without rebuilding the stages around it.
            """
            cast("DetectorPool", self.detectors).detect(job.image, job.detection)
            detection = load_detection(job.detection)
            return job.decode(), detection

        def adapter_engine_stage(_job: PageJob, prepared: Any) -> tuple[Any, Any, Any]:
            img, detection = prepared
            blocks = detection["blocks"]
            return img, blocks, ocr_page_read(img, blocks, crop_fn, cast("Recognizer", loader))

        def adapter_post_stage(_job: PageJob, read: Any) -> PageResult:
            img, blocks, per_line = read
            return PageResult(
                page=ocr_page_build(img, blocks, per_line, version=MOKURO_FORMAT_VERSION)
            )

        return (detect_stage, adapter_engine_stage, adapter_post_stage)

    # -- the pipeline itself -----------------------------------------------

    def _served_capacities(self, capacities: Mapping[str, int]) -> dict[str, int]:
        """The queue in front of a served engine holds exactly its window.

        Not "at most": the engine may hold a whole window of pages before it
        answers any (its last OCR batch waits for the end of the volume), so a
        shorter queue can leave both sides waiting for each other -- the
        engine for pages it has not been given, this process for answers it
        cannot get. A hand-set ``--queue-capacity feed=N`` is refused here for
        that reason, loudly rather than by hanging.
        """
        if self.served is None:
            return dict(capacities)
        window = max(1, int(self.served.window))
        asked = capacities.get(STAGE_FEED)
        if asked is not None and int(asked) != window:
            log(
                f"[runner] WARN queue-capacity {STAGE_FEED}={asked} ignored: the "
                f"{self.engine} process holds {window} page(s) and the queue in front "
                "of it is exactly that"
            )
        return {**capacities, STAGE_FEED: window}

    def build(self, workers: Mapping[str, int], capacities: Mapping[str, int]) -> StagePipeline:
        """A pipeline over this session's stages at the given widths.

        A :class:`StagePipeline` runs once, so ``--bench`` builds a new one a
        trial -- but only the THREADS and QUEUES are new. The models, the
        detector processes and the ppocr sessions behind the callables are the
        ones this object loaded, which is what makes a trial cost a pass over
        the pages rather than a pass plus a model load.
        """
        stages = page_stages(
            self.road,
            self.runs,
            engine=self.engine,
            budget=self.host_budget,
            forced=self.forced,
            workers=workers,
            capacities=self._served_capacities(capacities),
            specs=self.specs,
        )
        # Every stage logs into the log of the volume whose page it is holding.
        stages = [stage._replace(run=_attributed(stage.run)) for stage in stages]
        return StagePipeline(stages)

    def detect_devices(self) -> list[str]:
        """Devices the DETECTOR could be moved to, other than where it is.

        Only the adapter road: there the detector is a subprocess of its own,
        so moving it is closing K processes and opening K elsewhere. On the
        ppocr roads the detector runs in THIS process on the CPU by design
        (onnxruntime), and there is nowhere for it to go.
        """
        if self.road != ROAD_ADAPTER or self.detectors is None:
            return []
        if stage_is_cpu_only(self.road, STAGE_DETECT, engine=self.engine, detector=self.detector):
            return []
        here = self._stage_device(STAGE_DETECT)
        out = [DEVICE_CPU] if here != DEVICE_CPU else []
        if getattr(self, "ort_gpu", None) is False and stage_needs_ort_gpu(
            self.road, STAGE_DETECT, detector=self.detector, engine=self.engine
        ):
            # Its runtime cannot reach a card: a trial there would only fail.
            return out
        for index in range(_gpu_count()):
            candidate = f"{GPU_DEVICE_PREFIX}{index}"
            if candidate != here:
                out.append(candidate)
        return out

    def move_detect(self, device: str) -> None:
        """Put the detector's model on another device, in this same session.

        The recognizer -- the expensive load -- is untouched; only the K
        detector subprocesses are closed and reopened, and the widths are
        re-derived, because a pool of one on a card and a pool of three on the
        CPU are the same stage in two different shapes.
        """
        if self.detectors is None:
            raise RuntimeError("this road has no detector process to move")
        before = (self.specs, self.widths, self.caps, self.detect_width)
        self.specs = tuple(
            spec._replace(device=device) if spec.key == STAGE_DETECT else spec
            for spec in self.specs
        )
        self.widths, self.caps = self.plan()
        self.detect_width = self._detect_width(self.widths)
        previous = self.detectors
        try:
            self.detectors = open_detectors(
                self.detector, workers=self.detect_width, device=device
            )
        except BaseException:
            # A move that did not happen leaves the pipeline where it IS: the
            # placement ``bench_done`` reports is read off these specs, and a
            # device the detector never came up on must not be one of them.
            self.specs, self.widths, self.caps, self.detect_width = before
            raise
        previous.close()
        log(f"[runner] detector moved to {device} x{self.detect_width}")
        self.pipeline = self.build(self.stage_overrides, self.queue_overrides)

    def rebuild(self, widths: Sequence[int]) -> StagePipeline:
        """Re-pool at explicit per-stage widths, reusing every loaded model.

        The pools that hold a model or a session are RESIZED rather than
        rebuilt: a detector subprocess that is already up and a ppocr session
        that is already built are kept, the difference is created or closed,
        and the recognizer -- the expensive one -- is never touched.
        """
        workers = {spec.key: int(w) for spec, w in zip(self.specs, widths, strict=True)}
        detect = max(1, workers.get(STAGE_DETECT, 1))
        if self.detectors is not None:
            self.detectors.resize(detect)
        if self.ppocr is not None:
            self.ppocr.resize(detect)
        self.widths = list(widths)
        self.caps = stage_capacities(
            self.road, self.widths, capacities=self._served_capacities({}), specs=self.specs
        )
        self.detect_width = detect
        self.pipeline = self.build(workers, {})
        return self.pipeline

    # -- the models --------------------------------------------------------

    def wait_ready(self) -> None:
        """Block until every model this session holds has landed, or raise.

        A session announces itself READY, so a model that will never load is
        a fatal fact about the session rather than a volume of blank pages
        discovered one page at a time. The single-volume CLI does NOT wait:
        there the load overlaps the one volume it has, which is the whole
        reason :class:`DeferredRecognizer` exists.
        """
        if self.detectors is not None:
            self.detectors.wait()
        if self.loader is not None:
            self.loader.wait()

    def precision(self) -> str | None:
        """What the recognizer resolved ``--precision`` to, once it has loaded.

        A served engine's is decided at its start (its ``--fp16``). None for
        an engine that fixes its own (a line engine) and while it is loading.
        """
        if self.served_precision is not None:
            return self.served_precision
        if self.loader is None or not self.loader.loaded:
            return None
        return self.loader.precision

    def precision_target(self) -> Any:
        """The loaded recognizer the benchmark may re-cast, or None.

        Only a torch recognizer (:data:`TORCH_PRECISION_ENGINES`) ON A CARD
        -- the CPU supports fp32 alone -- loaded in THIS process: copies in
        processes of their own (``engine=N``) are never re-cast, and a
        benchmark never starts them.
        """
        if self.road == ROAD_SERVED:
            # Its one switch is a flag at the start: a trial at the other
            # format is a restart of the process (a model load, no more).
            served = self.served
            if (
                self.engine not in PRECISION_ENGINES
                or self._served_restart is None
                or served is None
                or served.device in ("", DEVICE_CPU)
            ):
                return None
            return ServedPrecisionTarget(self)
        if (
            self.engine not in TORCH_PRECISION_ENGINES
            or not device_is_gpu(self.engine_device)
            or not isinstance(self.loader, DeferredRecognizer)
        ):
            return None
        recognizer = self.loader.wait()
        return recognizer if hasattr(recognizer, "set_precision") else None

    def weights_so_far(self) -> dict[str, str]:
        """Repo -> pinned commit for everything that has reported one."""
        known = dict(self.detectors.weights) if self.detectors is not None else {}
        known.update(getattr(self.reader, "repos", {}) or {})
        if self.loader is not None and self.loader.loaded:
            known.update(self.loader.weights())
        return known

    def weights(self, detect_dir: Path) -> dict[str, str]:
        """What really read a volume, for its sidecar.

        Collected from the objects that loaded them rather than from a static
        table, so a sidecar can only claim weights that were really resolved.
        """
        if self.road == ROAD_ADAPTER:
            # A served detector reports its weights on its ready line rather
            # than into a volume's directory (it has none); the file is still
            # written here, because ``_weights.json`` beside the detections is
            # part of the adapter contract and anything reading it should keep
            # working.
            detector_weights = (
                self.detectors.weights if self.detectors else {}
            ) or load_detector_weights(detect_dir)
            if detector_weights:
                dump_json(detector_weights, detect_dir / DETECTOR_WEIGHTS_FILE)
            log(f"[runner] detector {self.detector} weights: {detector_weights or 'not reported'}")
            weights = dict(detector_weights)
        else:
            weights = dict(getattr(self.reader, "repos", {}))
        if self.loader is not None:
            weights = {**weights, **self.loader.weights()}
        return weights

    def close(self) -> None:
        """Stop the children. The pipeline's own teardown is the caller's."""
        if isinstance(self.loader, RecognizerPool):
            self.loader.close()
        if self.detectors is not None:
            self.detectors.close()
        if self.served is not None:
            self.served.close()


def open_detectors(detector: str, *, workers: int, device: str = "") -> DetectorPool:
    """Start the detector processes. The seam the tests replace.

    Takes no volume: a served detector is told each page and each destination
    one request at a time (see ``detectors/_common``), so a pool opened here
    can outlive the volume that opened it. ``device`` is where this row put the
    detector's model; every process is told it on its command line.
    """
    return DetectorPool(detector, workers, device=device)


# ---------------------------------------------------------------------------
# THE ARCHIVE IS THE SOURCE. A volume may arrive as a .cbz instead of a
# directory of pages, and then nobody extracts it: the feeder reads members
# out of it ONE AT A TIME, just ahead of the pipeline, and rolls from the last
# page of one archive into the first page of the next with no gap. What is in
# flight is bounded by the pipeline's tickets, so neither memory nor disk
# grows with the volume or with the queue.
#
# The page list must be what extraction + :func:`list_pages` gives, member for
# member and in the same order, or the sidecar differs from the one the
# extracted road writes -- which is the test.
# ---------------------------------------------------------------------------


def reading_order(rel_paths: Sequence[Path]) -> list[Path]:
    """Page paths in reading order: mokuro's natural sort."""
    try:
        from natsort import natsorted

        return list(natsorted(rel_paths))
    except ImportError:
        return sorted(rel_paths, key=_natural_key)


def extracted_name(member: str) -> str:
    """Where ``ZipFile.extractall`` would put this member, relative to the root.

    Mirrors ``ZipFile._extract_member``: separators normalised, any drive
    dropped, and every empty, ``.`` or ``..`` component removed. Not a
    nicety -- it is the only way the archive road and the extracted road can
    name the same page, and it is also what keeps a member called
    ``../../etc/x.webp`` from being written outside the workspace.
    """
    name = member.replace("/", os.path.sep)
    if os.path.altsep:
        name = name.replace(os.path.altsep, os.path.sep)
    name = os.path.splitdrive(name)[1]
    skip = ("", os.path.curdir, os.path.pardir)
    return os.path.sep.join(part for part in name.split(os.path.sep) if part not in skip)


def member_map(names: Iterable[str], stem: str) -> dict[Path, str]:
    """``{where it would land: the member to read}`` for a list of member names.

    The rule, and the ONLY copy of it: the same extension filter and the same
    exclusion ``OCRProcessor._extract_and_clean`` applies after extracting
    (the embedded top-level ``<stem>.webp`` some uploaders ship is a
    thumbnail, not a page). A caller reading an archive over a network has the
    names but not a ``ZipFile``, and must reach the same answer as one that
    does -- so it calls this.

    Nothing else is excluded. A file the extracted directory would have
    contained is a page here too, however odd it looks, because the two roads
    have to agree page for page. Two members that would land on the same path
    collapse to the LAST of them, which is the file extraction would have left
    there.
    """
    thumbnail = f"{stem}.webp"
    out: dict[Path, str] = {}
    for member in names:
        landed = extracted_name(member)
        if not landed or Path(landed).suffix.lower() not in IMAGE_EXTENSIONS:
            continue
        if landed == thumbnail:
            continue
        out[Path(landed)] = member
    return out


def archive_members(archive: Path, stem: str | None = None) -> dict[Path, str]:
    """``{where it would land: the member to read}`` for every page in a .cbz.

    ``stem`` is the LIBRARY archive's stem, which the thumbnail rule is keyed
    on; it defaults to this path's own. A remote processor's runner reads
    its archive as ``/proc/<pid>/fd/<n>``, whose stem is a number.
    """
    with zipfile.ZipFile(archive) as zf:
        names = [info.filename for info in zf.infolist() if not info.is_dir()]
    return member_map(names, stem if stem is not None else Path(archive).stem)


def archive_pages(archive: Path, stem: str | None = None) -> list[Path]:
    """The pages of a .cbz, in the order extracting it would have given."""
    return reading_order(list(archive_members(archive, stem)))


class ArchiveReader:
    """One open handle on one archive, read a page at a time.

    ``zipfile`` is not safe to read concurrently through one handle, and a
    handle per in-flight page would be a handle per worker -- so this is
    deliberately single-threaded and only the feeder ever touches it. Reading
    a member is a few milliseconds and must never be what the pipeline waits
    on; the feeder sitting one page ahead of the tickets is what makes sure.
    """

    def __init__(self, archive: Path, stem: str | None = None) -> None:
        self.archive = Path(archive)
        self._zip = zipfile.ZipFile(self.archive)
        names = [info.filename for info in self._zip.infolist() if not info.is_dir()]
        self._members = member_map(names, stem if stem is not None else self.archive.stem)

    def pages(self) -> list[Path]:
        return reading_order(list(self._members))

    def read(self, rel: Path) -> bytes:
        return self._zip.read(self._members[rel])

    def close(self) -> None:
        with contextlib.suppress(Exception):
            self._zip.close()


class VolumeRequest(NamedTuple):
    """One volume: where its pages come from, and where its artefacts go."""

    id: str
    output: Path
    cache_dir: Path
    detect_dir: Path
    input_dir: Path | None = None
    archive: Path | None = None
    workspace: Path | None = None
    log: Path | None = None
    title: str | None = None
    volume: str | None = None
    title_uuid: str | None = None
    volume_uuid: str | None = None
    # The library archive's stem, when the archive's own path is not named
    # after it (a remote processor's `/proc/<pid>/fd/<n>`): the thumbnail
    # rule, the fallback title and the "no page images" message use it.
    stem: str | None = None


class VolumeRun:
    """One volume's share of an open pipeline: its pages, and what came back."""

    def __init__(
        self,
        request: VolumeRequest,
        paths: VolumePaths,
        pages: Sequence[Path],
        *,
        reader: ArchiveReader | None = None,
        spooled: bool = False,
        handle: Any = None,
    ) -> None:
        self.request = request
        self.paths = paths
        self.pages = list(pages)
        self.total = len(self.pages)
        self.reader = reader
        # Pages written under the workspace for a subprocess detector to read,
        # and removed again once they have left the sink.
        self.spooled = spooled
        self.handle = handle
        self.results: list[tuple[str, dict[str, Any]]] = []
        self.review: list[dict[str, Any]] = []
        self.failed = 0
        self.seen = 0
        self.started = time.time()
        # The counters as they stood when this volume's FIRST PAGE ENTERED the
        # pipeline. Its share of them is the difference between that and where
        # they stand when its last page leaves. Volumes overlap inside the
        # pipeline, so these windows overlap too: they are each volume's own
        # stretch of the run, not a partition of the session.
        self.mark: PipelineReport | None = None
        # (PSI total in microseconds, wall clock) as this volume's first page
        # entered the pipeline: its CPU pressure is measured from here.
        self.pressure_mark: tuple[float, float] | None = None
        # ``host_sample()`` at the same moment: what the neighbours used of
        # the host's CPU is measured from here.
        self.host_mark: HostSample | None = None

    @property
    def complete(self) -> bool:
        return self.seen >= self.total


class VolumeFeed:
    """Pages of volume after volume, for one open pipeline.

    Blocks on the next volume when there is none, which parks the pipeline's
    feeder thread and nothing else -- every page already accepted keeps
    flowing through the stages behind it. :meth:`close` ends the stream.
    """

    def __init__(
        self,
        *,
        spool: bool,
        opening: Callable[[VolumeRun], None] | None = None,
    ) -> None:
        self.spool = bool(spool)
        self.opening = opening
        self._queue: queue.Queue[VolumeRun | None] = queue.Queue()
        # Pages handed out by this feeder, over the whole session. The number
        # a page carries is what an ORDERED consumer downstream reassembles by
        # (:class:`PageJob`), so it must never restart inside a session.
        self._seq = 0

    def submit(self, run: VolumeRun) -> None:
        self._queue.put(run)

    def close(self) -> None:
        """End the session: the end of the feed goes BEHIND every volume
        already accepted, so those are read to their last page first."""
        self._queue.put(None)

    def __iter__(self) -> Iterator[PageJob]:
        while True:
            run = self._queue.get()
            if run is None:
                return
            yield from self.pages_of(run)

    def pages_of(self, run: VolumeRun) -> Iterator[PageJob]:
        """One volume's pages, read just ahead of the pipeline.

        The volume is timed from HERE, and its share of the counters is
        marked from here -- not from when its op arrived: a server holding a
        lookahead submits the next volume long before the pipeline has room
        for it, and counting that wait as the volume's own time would make
        every volume but the first look slower the deeper the queue was.
        """
        run.started = time.time()
        if self.opening is not None:
            self.opening(run)
        final = len(run.pages) - 1
        if run.reader is None:
            for index, rel in enumerate(run.pages):
                yield PageJob(run.paths, rel, seq=self._take(), last=index == final)
            return
        try:
            for index, rel in enumerate(run.pages):
                seq, last = self._take(), index == final
                data: bytes | None
                try:
                    data = run.reader.read(rel)
                    if self.spool:
                        target = run.paths.input_dir / rel
                        target.parent.mkdir(parents=True, exist_ok=True)
                        target.write_bytes(data)
                        data = None
                except Exception as e:
                    # A corrupt member, or a workspace that will not take the
                    # page: EITHER costs this page and no other. The feeder
                    # raising here would end the SESSION, because it is the
                    # pipeline's source -- and one bad page must never be
                    # that.
                    previous = LOG.bind(run.handle)
                    try:
                        log(f"[runner] ERROR page {rel}: cannot be read from the archive: {e}")
                    finally:
                        LOG.bind(previous)
                    # Empty bytes fail in the first stage, in this page's own
                    # place, through the same blank-page path a corrupt image
                    # on disk has always taken.
                    yield PageJob(run.paths, rel, b"", seq=seq, last=last)
                    continue
                yield PageJob(run.paths, rel, data, seq=seq, last=last)
        finally:
            run.reader.close()

    def _take(self) -> int:
        """The next page number of this session. Only the feeder thread calls it."""
        seq = self._seq
        self._seq += 1
        return seq


# ---------------------------------------------------------------------------
# THE PROTOCOL. ``--serve`` and ``--bench`` put JSON on stdout, one object a
# line, and NOTHING else may ever reach it -- a stray line there is not noise,
# it is a parse error in the server. So the modes that speak it take fd 1 for
# themselves and point every other writer (this file's logging, a library's
# C-level printf, a child that inherited the descriptor) at the log file.
# ---------------------------------------------------------------------------


class Protocol:
    """stdout as a line-delimited JSON stream. Thread-safe; never raises."""

    def __init__(self, stream: Any) -> None:
        self._stream = stream
        self._lock = threading.Lock()

    def emit(self, event: str, **fields: Any) -> None:
        # ASCII-escaped: the stream is a pipe to another process and a volume
        # title is whatever the library called it. An escape cannot be
        # mis-decoded on the far side; a raw multi-byte character can.
        line = json.dumps({"event": event, **fields}, default=json_default)
        with self._lock:
            try:
                self._stream.write(line + "\n")
                self._stream.flush()
            except (OSError, ValueError):
                pass


def seize_stdout(log_path: Path) -> Any:
    """Take fd 1 for the protocol; send everything else to ``log_path``.

    Not just ``sys.stdout``: onnxruntime, torch and any child that inherited
    the descriptor write to FD 1, and a line from one of them lands in the
    middle of a JSON object. So fd 1 is duplicated away for the protocol and
    the original is pointed at the log -- which also catches ``print`` from
    anywhere in this process.

    fd 2 is left ALONE when the caller gave it somewhere of its own (the
    server sends it to ``<session log>.stderr``); taking it too would quietly
    empty a file somebody made on purpose. It is only redirected when it is
    the SAME open file as fd 1 -- a caller that merged stderr into the
    protocol pipe -- because then a traceback would corrupt the protocol.
    """
    log_path.parent.mkdir(parents=True, exist_ok=True)
    for stream in (sys.stdout, sys.stderr):
        with contextlib.suppress(Exception):
            stream.flush()
    merged = False
    try:
        one, two = os.fstat(1), os.fstat(2)
        merged = (one.st_dev, one.st_ino) == (two.st_dev, two.st_ino)
    except OSError:
        merged = False
    saved = os.dup(1)
    sink = os.open(str(log_path), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
    try:
        os.dup2(sink, 1)
        if merged:
            os.dup2(sink, 2)
    finally:
        os.close(sink)
    for stream in (sys.stdout, sys.stderr) if merged else (sys.stdout,):
        # A replaced stream (a test's capture, say) may not be a TextIOWrapper.
        reconfigure = getattr(stream, "reconfigure", None)
        with contextlib.suppress(Exception):
            if reconfigure is not None:
                reconfigure(line_buffering=True)
    return os.fdopen(saved, "w", encoding="utf-8", errors="replace", buffering=1)


# ---------------------------------------------------------------------------
# THE SESSION. One open pipeline, volumes streaming through it, and the sink
# that turns each volume's pages back into that volume's sidecar.
# ---------------------------------------------------------------------------


class Session:
    """Volumes through one open pipeline.

    Pages leave the pipeline in INPUT order (:meth:`StagePipeline._drain`) and
    volumes are fed in arrival order, so a volume's pages arrive here together
    and in page order -- which is what lets the sink group them while holding
    nothing but the volume it is assembling. Volumes still OVERLAP inside the
    pipeline: while this sink finishes volume N, the detect stage is already
    on volume N+1's pages.
    """

    def __init__(
        self,
        pipe: OpenPipeline,
        *,
        protocol: Protocol | None = None,
        stats_path: Path | None = None,
    ) -> None:
        self.pipe = pipe
        self.protocol = protocol
        self.stats_path = stats_path
        self.runs: dict[str, VolumeRun] = {}
        self.load_failed: BaseException | None = None
        self._published = 0.0
        # When the previous volume of this session ended (done or failed).
        # A volume's seconds are counted from no earlier than this
        # (`_volume_seconds`).
        self._previous_end: float | None = None
        self._page_started = time.time()
        # (wall clock, ``host_sample()``) at the recent ticks, oldest first:
        # the live share is measured against the one ~HOST_SHARE_WINDOW ago.
        self._host_ticks: deque[tuple[float, HostSample]] = deque()
        self._host_share: float | None = None

    # -- accepting a volume -------------------------------------------------

    def accept(self, request: VolumeRequest, pages: Sequence[Path] | None = None) -> VolumeRun:
        """Prepare a volume's directories, list its pages and register it.

        Raises for a source that cannot be read at all -- an archive that will
        not open. That is a failure of THAT volume and never of the session.
        """
        # The server hands over a workspace it has only just made, so none of
        # these need exist yet. The sidecar's own directory included: the
        # volume is written beside and renamed, and a rename needs somewhere
        # to rename into.
        request.cache_dir.mkdir(parents=True, exist_ok=True)
        request.detect_dir.mkdir(parents=True, exist_ok=True)
        request.output.parent.mkdir(parents=True, exist_ok=True)
        handle = None
        if request.log is not None:
            request.log.parent.mkdir(parents=True, exist_ok=True)
            handle = request.log.open("a", encoding="utf-8", errors="replace")
        reader: ArchiveReader | None = None
        spooled = False
        if request.archive is not None:
            reader = ArchiveReader(request.archive, request.stem)
            pages = reader.pages()
            # A subprocess detector must be handed a PATH, so those pages are
            # written under the workspace and removed again as they leave the
            # sink. Every other road decodes the bytes in memory and the page
            # never touches a disk at all.
            spooled = self.pipe.road == ROAD_ADAPTER
            # NEVER the cache directory as a fallback: the server counts the
            # files there to know how many pages are done.
            input_dir = request.workspace or (
                request.cache_dir.parent / "_pages" / (request.id or "volume")
            )
        else:
            input_dir = cast("Path", request.input_dir)
            if pages is None:
                pages = list_pages(input_dir)
        paths = VolumePaths(
            input_dir=input_dir,
            detect_dir=request.detect_dir,
            cache_dir=request.cache_dir,
            id=request.id,
            log=handle,
        )
        run = VolumeRun(request, paths, pages or [], reader=reader, spooled=spooled, handle=handle)
        self.runs[request.id] = run
        return run

    # -- the sink -----------------------------------------------------------

    def consume(self, stream: Iterator[tuple[Any, Outcome]], *, finish: bool) -> None:
        """Drain the pipeline, grouping pages by the volume they came from.

        ``finish`` says whether a volume is assembled HERE, the moment its
        last page arrives (``--serve``: the next volume is still running and
        this one is due now), or left to the caller (the single-volume CLI,
        whose numbers are final only once the pools are torn down, and whose
        log has printed them in that order since before there were sessions).
        """
        for job, outcome in stream:
            run = self.runs[job.volume.id]
            run.seen += 1
            previous = LOG.bind(run.handle)
            try:
                alive = self._page(run, job, outcome)
            finally:
                LOG.bind(previous)
            if not alive:
                return
            if finish and run.complete:
                self.finish(run)

    def _page(self, run: VolumeRun, job: PageJob, outcome: Outcome) -> bool:
        """One page out of the pipeline. False ends the run: the model is gone."""
        rel = job.rel
        try:
            # ``unwrap`` is where a failure in ANY stage surfaces, in this
            # page's place in the order, under the same guard that has always
            # caught a bad page.
            result = outcome.unwrap()
        except Exception as e:  # keep going; a bad page must not sink the volume
            served = self.pipe.served
            if served is not None and served.error is not None:
                # The engine's PROCESS is gone. Every page after this one
                # would fail the same way, so the run ends here with the real
                # error rather than writing a volume of blank pages; the
                # server blames the oldest volume and may open a new session.
                self.load_failed = served.error
                return False
            loader = self.pipe.loader
            if loader is not None and loader.error is not None:
                # A recognizer that failed to LOAD is not a bad page: every
                # page after it would fail the same way, so the run ends at
                # the first one with the real error instead of writing a
                # volume of blanks.
                self.load_failed = loader.error
                return False
            run.failed += 1
            log(f"[runner] ERROR page {rel}: {e}")
            # The message alone has never been enough to find the bug.
            log(traceback.format_exc())
            # Keep the page, empty: readers line a sidecar up with the volume
            # page by page, and one short of pages is refused whole.
            blank = job.blank()
            if blank is None:
                self._retire(run, job)
                return True
            result = PageResult(page=blank)
        page = result.page
        # review.json is read as an ordered list and the post stage is a pool,
        # so the entries are appended HERE, in page order, from what the stage
        # handed back.
        if result.tally:
            if result.doubtful:
                run.review.append({"page": rel.as_posix(), "lines": result.doubtful})
            log(
                f"[runner] reconcile {rel}: lines={result.tally['lines']} "
                f"full_agreement={result.tally['full_agreement']} "
                f"from_ctc={result.tally['from_ctc']} notes={result.tally['notes']} "
                f"engine={result.engine_seconds:.2f}s"
            )
        dump_json(page, job.cache)
        run.results.append((rel.as_posix(), page))
        log(
            f"[runner] page {run.seen}/{run.total} {rel} blocks={len(page['blocks'])} "
            f"({time.time() - self._page_started:.2f}s)"
        )
        self._page_started = time.time()
        self._retire(run, job)
        if self.protocol is not None:
            # A page an event: progress is the EVENT now, not a count of the
            # files in the cache directory, and a line a page is nothing
            # beside a page of OCR.
            self.protocol.emit("page", id=run.request.id, done=run.seen, total=run.total)
        self._tick(run)
        return True

    def _retire(self, run: VolumeRun, job: PageJob) -> None:
        """A page has left the sink: drop the copy the detector needed.

        Always inside this volume's own input directory, and nothing here
        has to check that: ``job.image`` is ``input_dir / rel``, and ``rel``
        is a member's extracted name (:func:`extracted_name`), which can
        never climb out of it.
        """
        if not run.spooled:
            return
        with contextlib.suppress(OSError):
            job.image.unlink()

    def _tick(self, run: VolumeRun) -> None:
        """The live numbers, for whoever is watching this run from outside."""
        if time.time() - self._published < PIPELINE_STATS_INTERVAL:
            return
        self._published = time.time()
        path = self.stats_path or run.request.detect_dir / PIPELINE_STATS_FILE
        write_pipeline_stats(path, self.pipe.pipeline)
        if self.protocol is not None:
            pressure = cpu_pressure_now()
            extra: dict[str, float] = {} if pressure is None else {"cpu_pressure": pressure}
            others = self._recent_other_cpu()
            if others is not None:
                extra["other_cpu"] = others
            self.protocol.emit("stats", pipeline=self.pipe.pipeline.snapshot(), **extra)

    def _recent_other_cpu(self) -> float | None:
        """What the neighbours used of the host's CPU over the last ~10 s."""
        ticks = self._host_ticks
        now = time.time()
        if ticks and now - ticks[-1][0] < HOST_SHARE_EVERY:
            return self._host_share
        sample = host_sample()
        if sample is None:
            return None
        ticks.append((now, sample))
        while len(ticks) > 2 and now - ticks[1][0] >= HOST_SHARE_WINDOW:
            ticks.popleft()
        self._host_share = other_cpu_share(ticks[0][1], sample) if len(ticks) > 1 else None
        return self._host_share

    # -- assembling a volume ------------------------------------------------

    def open_window(self, run: VolumeRun) -> None:
        """Mark the counters as this volume's first page enters the pipeline."""
        run.mark = self.pipe.pipeline.report()
        total = cpu_pressure_total()
        run.pressure_mark = (total, time.time()) if total is not None else None
        run.host_mark = host_sample()

    @staticmethod
    def volume_pressure(run: VolumeRun) -> float | None:
        """The share of this volume's own window some task waited for a CPU."""
        mark = run.pressure_mark
        total = cpu_pressure_total()
        if mark is None or total is None:
            return None
        wall = time.time() - mark[1]
        if wall <= 0:
            return None
        return round(max(0.0, min(1.0, (total - mark[0]) / 1e6 / wall)), 3)

    @staticmethod
    def volume_other_cpu(run: VolumeRun) -> float | None:
        """The share of the host's CPU others used over this volume's window."""
        return other_cpu_share(run.host_mark, host_sample())

    def measure(self, run: VolumeRun) -> PipelineReport:
        """This volume's share of the counters: its own stretch of the run.

        The difference between where the counters stood when its FIRST PAGE
        ENTERED the pipeline and where they stand now its last has left. Not
        a partition of the session -- volumes overlap inside the pipeline, so
        these windows overlap too -- but every window does contain the whole
        of its own volume, which the alternative (a contiguous window per
        volume, closing where the one before it closed) does not: a volume
        whose pages had all gone through before its predecessor's last page
        came out would measure an empty window and report nothing.
        """
        return self.pipe.pipeline.report().since(run.mark)

    def finish(self, run: VolumeRun, *, summary: bool = True) -> bool:
        """Write this volume's sidecar and say whether it landed.

        Everything the single-volume CLI writes, in the order it writes it:
        the summary lines, ``_weights.json``, the sidecar (beside and
        renamed), ``review.json``, and the two lines the server's log parser
        reads. A volume that produced nothing is a failed volume and writes no
        sidecar at all.

        ``summary`` is False only for the CLI, which has already printed those
        lines itself (it takes them after its pools are torn down, which is
        when its numbers are final).
        """
        share = self.measure(run)
        previous = LOG.bind(run.handle)
        try:
            return self._assemble(run, share, summary=summary)
        finally:
            LOG.bind(previous)

    def _assemble(self, run: VolumeRun, share: PipelineReport, *, summary: bool) -> bool:
        request = run.request
        if summary:
            publish_pipeline_stats(request.detect_dir / PIPELINE_STATS_FILE, share.as_dict())
            for line in share.lines():
                log(f"[runner] {line}")
        if not run.results:
            log("[runner] ERROR: every page failed")
            self._failed(run, "every page failed")
            return False
        weights = self.pipe.weights(request.detect_dir)
        name = request.stem or (request.archive.stem if request.archive is not None else "")
        if not name and request.input_dir is not None:
            name = request.input_dir.name
        served = self.pipe.served
        volume = build_volume(
            run.results,
            # A served engine wrote these pages and stamps them with its own
            # version; the volume header says the same thing its CLI would
            # have said, so the two files are the same file.
            version=served.version if served is not None else MOKURO_FORMAT_VERSION,
            title=request.title or name,
            volume=request.volume or name,
            title_uuid=request.title_uuid or str(uuid.uuid4()),
            volume_uuid=request.volume_uuid or str(uuid.uuid4()),
            # A served engine's file is its CLI's file, plus the one thing the
            # CLI never says: the precision it read at (``--fp16`` or not).
            engine_meta=(
                {"id": self.pipe.engine, "precision": precision}
                if (precision := self.pipe.precision()) is not None
                else None
            )
            if served is not None
            else {
                "id": self.pipe.engine,
                "recognizer": RECOGNIZER_REPOS[self.pipe.engine],
                "detector": self.pipe.detector,
                "generator": self.pipe.config.generator or "mokuro-bunko",
                # Only for the engines it means something to, so a sidecar
                # never claims a resolution its recognizer did not read at.
                **(
                    {"patch_budget": self.pipe.config.patches}
                    if self.pipe.engine in PATCH_BUDGET_ENGINES
                    else {}
                ),
                # The exact weights behind the text: repo -> resolved commit,
                # for every model this run loaded (a reconciled read names
                # both). Without it "recognizer: <repo>" is only a name, and a
                # repo's contents move.
                **({"weights": dict(weights)} if weights else {}),
                # What the recognizer computed in (``--precision``, resolved):
                # bf16 and fp16 can read a line differently from fp32.
                **({"precision": ran} if (ran := self.pipe.precision()) else {}),
            },
        )
        tmp_path = request.output.with_name(request.output.name + ".tmp")
        dump_json(volume, tmp_path)
        tmp_path.replace(request.output)
        if self.pipe.road == ROAD_RECONCILED:
            # One list a volume of the lines the two recognizers differ on, so
            # a proofreader does not have to open every page's dump to find
            # them. Beside the dumps, never in the sidecar (pure mokuro
            # format).
            listing = {
                "format": "ocr-review/1",
                "engine": self.pipe.engine,
                "detector": self.pipe.detector,
                "pages": run.review,
            }
            dump_json(listing, request.detect_dir / REVIEW_FILE)
        log(
            f"[runner] wrote {request.output} pages={len(run.results)} "
            f"failed_pages={run.failed} elapsed={time.time() - run.started:.1f}s"
        )
        # Same summary line mokuro prints, so the caller's log parser applies.
        log("Processed successfully: 1/1")
        self._close_run(run)
        seconds = self._volume_seconds(run)
        if self.protocol is not None:
            pressure = self.volume_pressure(run)
            others = self.volume_other_cpu(run)
            self.protocol.emit(
                "volume_done",
                id=request.id,
                pages=len(run.results),
                failed_pages=run.failed,
                seconds=round(seconds, 3),
                stats=share.as_dict(),
                **({} if pressure is None else {"cpu_pressure": pressure}),
                **({} if others is None else {"other_cpu": others}),
            )
        return True

    def fail(self, run: VolumeRun, error: str) -> None:
        """This volume is over before its pages were: say so and carry on."""
        previous = LOG.bind(run.handle)
        try:
            log(f"[runner] ERROR: {error}")
        finally:
            LOG.bind(previous)
        self._failed(run, error)

    def _volume_seconds(self, run: VolumeRun) -> float:
        """This volume's own seconds, ending now: they start at its first page's
        feed or at the end of the volume before it in this session, whichever
        is LATER.

        The feeder reaches a volume as soon as the feed queue has room -- which
        on a deep pipeline (138 pages on mokuro served) is while the previous
        volume is still in the engine -- so ``run.started`` alone charged every
        volume but the first with its predecessor's tail (live: 39.8 s recorded
        for 18.9 s of work). The volumes of a session now partition its time.
        """
        now = time.time()
        start = run.started
        if self._previous_end is not None:
            start = max(start, self._previous_end)
        self._previous_end = now if self._previous_end is None else max(self._previous_end, now)
        return max(0.0, now - start)

    def _failed(self, run: VolumeRun, error: str) -> None:
        # A failed volume held the pipeline too: the next one's time starts
        # after it.
        self._volume_seconds(run)
        self._close_run(run)
        if self.protocol is not None:
            self.protocol.emit("volume_failed", id=run.request.id, error=error)

    def _close_run(self, run: VolumeRun) -> None:
        if run.reader is not None:
            run.reader.close()
        if run.handle is not None:
            with contextlib.suppress(Exception):
                run.handle.flush()
                run.handle.close()
            run.handle = None
        self.runs.pop(run.request.id, None)


# ---------------------------------------------------------------------------
# THE SINGLE-VOLUME CLI. One session, one volume -- the same pieces
# ``--serve`` is built on, so what it writes and what a session writes cannot
# drift. Its stdout is unchanged, down to the last line: the server's
# per-volume log is this stdout and its parser reads those lines.
# ---------------------------------------------------------------------------


def run(args: argparse.Namespace) -> int:
    input_dir = Path(args.input)
    output_path = Path(args.output)
    cache_dir = Path(args.cache_dir)
    cache_dir.mkdir(parents=True, exist_ok=True)

    pages = list_pages(input_dir)
    if not pages:
        log(f"[runner] ERROR: no page images found under {input_dir}")
        return 1
    detect_dir = (
        Path(args.detect_dir) if args.detect_dir else output_path.parent / "_detect" / args.engine
    )
    try:
        pipe = OpenPipeline(
            SessionConfig.from_args(args),
            page_cap=len(pages),
            intro=f" pages={len(pages)} input={input_dir}",
        )
    except ValueError as e:
        log(f"[runner] ERROR: {e}")
        return 2
    stats_path = pipeline_stats_path(args, detect_dir)
    session = Session(pipe, stats_path=stats_path)
    volume = session.accept(
        VolumeRequest(
            id="cli",
            output=output_path,
            cache_dir=cache_dir,
            detect_dir=detect_dir,
            input_dir=input_dir,
            title=args.title,
            volume=args.volume,
            title_uuid=args.title_uuid,
            volume_uuid=args.volume_uuid,
        ),
        pages=pages,
    )
    pipeline = pipe.pipeline
    jobs = [
        PageJob(volume.paths, rel, seq=index, last=index == len(pages) - 1)
        for index, rel in enumerate(pages)
    ]
    stream = pipeline.run(jobs)
    with contextlib.ExitStack() as scope:
        # THE DETECTORS GO FIRST on the way out (callbacks run in reverse, so
        # this is the later registration). Stopping the pipeline first means
        # joining a worker that is blocked waiting for a detector's reply, and
        # a pool that is not closed yet still RESPAWNS the ones that die under
        # it: measured, an aborted run spent 13 s in teardown loading three
        # more copies of a model nobody would use. Killing the children first
        # fails every in-flight detect at once, and the joins are instant.
        # Registered here rather than in a ``finally`` so that ANY way out of
        # this loop -- an exception, a break, the parent ending the run --
        # takes the children with it.
        scope.callback(stream.close)
        scope.callback(pipe.close)
        session.consume(stream, finish=False)

    report = pipeline.report()
    write_pipeline_stats(stats_path, pipeline)
    for line in report.lines():
        log(f"[runner] {line}")

    if session.load_failed is not None:
        log(f"[runner] ERROR: {args.engine} failed to load: {session.load_failed}")
        log("".join(traceback.format_exception(session.load_failed)))
        return 1

    if not volume.results:
        log("[runner] ERROR: every page failed")
        return 1

    session.finish(volume, summary=False)
    return 0


# ---------------------------------------------------------------------------
# ``--serve``: ONE pipeline, volume after volume, protocol on stdout.
# ---------------------------------------------------------------------------


class ServeSession:
    """The ``--serve`` loop: ops in on stdin, events out on stdout."""

    def __init__(self, args: argparse.Namespace, protocol: Protocol) -> None:
        self.args = args
        self.protocol = protocol
        self.pipe: OpenPipeline | None = None
        self.session: Session | None = None
        self.feed: VolumeFeed | None = None
        self._closing = False
        self._lock = threading.Lock()
        # Set when a retired streamed op arrived (see `read_ops`).
        self._tripped = False

    # -- one op -------------------------------------------------------------

    def op_volume(self, op: Mapping[str, Any]) -> None:
        """Accept one volume, or fail exactly that volume.

        EVERY volume this is called for gets exactly one ``volume_started``
        and exactly one terminal event, whatever went wrong -- an archive that
        will not open, a field that is not there, a path that cannot be made.
        The server has a job waiting on each id, and a job it hears nothing
        about is a job that never ends.

        The one exception is an id it cannot key by: a missing one, or one
        already in this session. Answering those would tell the server
        something about the WRONG job, so they are refused in the log only.
        """
        session, feed = cast("Session", self.session), cast("VolumeFeed", self.feed)
        job_id = str(op.get("id") or "")
        if not job_id:
            log("[runner] ERROR a volume op needs an id: every event is keyed by it")
            return
        if job_id in session.runs:
            log(f"[runner] ERROR volume {job_id!r} is already in this session; ignored")
            return
        try:
            self._accept(op, job_id, session, feed)
        except Exception as e:
            log(f"[runner] ERROR volume {job_id}: {e}")
            log(traceback.format_exc())
            self.protocol.emit("volume_started", id=job_id, pages=0)
            self.protocol.emit("volume_failed", id=job_id, error=str(e))

    def _accept(
        self, op: Mapping[str, Any], job_id: str, session: Session, feed: VolumeFeed
    ) -> None:
        output = Path(str(op["output"]))
        detect_dir = (
            Path(str(op["detect_dir"]))
            if op.get("detect_dir")
            else output.parent / "_detect" / self.args.engine
        )
        if not op.get("archive") and not op.get("input"):
            raise ValueError(f"volume {job_id} names no archive and no input directory")
        request = VolumeRequest(
            id=job_id,
            output=output,
            cache_dir=Path(str(op["cache_dir"])),
            detect_dir=detect_dir,
            input_dir=Path(str(op["input"])) if op.get("input") else None,
            archive=Path(str(op["archive"])) if op.get("archive") else None,
            workspace=Path(str(op["workspace"])) if op.get("workspace") else None,
            log=Path(str(op["log"])) if op.get("log") else None,
            title=op.get("title"),
            volume=op.get("volume"),
            title_uuid=op.get("title_uuid"),
            volume_uuid=op.get("volume_uuid"),
            stem=Path(str(op["stem"])).name if op.get("stem") else None,
        )
        volume = session.accept(request)
        self.protocol.emit("volume_started", id=job_id, pages=volume.total)
        if not volume.pages:
            # Named by the LIBRARY's archive where there is one: a record
            # must never show a processor's `/proc/<pid>/fd/<n>`.
            where = (
                f"{request.stem}.cbz"
                if request.stem
                else (request.archive or request.input_dir)
            )
            session.fail(volume, f"no page images found in {where}")
            return
        feed.submit(volume)

    def _trip(self, op: Mapping[str, Any]) -> None:
        """A retired STREAMED op: the processor driving this runner is older.

        This runner takes archives only. A processor that still streams pages
        was not restarted when the code was updated under it (the deploy
        order is stop, update, start). It is told ONCE -- one `fatal`, and
        the op loop ends -- rather than failing every volume it sends: that
        way a mis-ordered deploy costs a record or two per row, not one per
        volume in the queue.
        """
        error = (
            "this runner takes archives only; the processor driving it is older than "
            "the runner — stop it, update it, start it again"
        )
        log(f"[runner] ERROR {op.get('op')!r} op ({op.get('pages') or 'no pages'}): {error}")
        self._tripped = True
        self.protocol.emit("fatal", error=error)

    def op_close(self) -> None:
        with self._lock:
            if self._closing:
                return
            self._closing = True
        if self.feed is not None:
            self.feed.close()

    def read_ops(self) -> None:
        """Ops arrive at any time; volumes are read in arrival order."""
        try:
            for line in sys.stdin:
                text = line.strip()
                if not text:
                    continue
                try:
                    op = json.loads(text)
                except ValueError:
                    log(f"[runner] WARN unreadable op: {text[:200]}")
                    continue
                kind = op.get("op")
                if kind == "close":
                    return
                if kind in ("page", "end") or (kind == "volume" and op.get("pages") == "stream"):
                    self._trip(op)
                    return
                if kind != "volume":
                    log(f"[runner] WARN unknown op {kind!r}")
                    continue
                if self._closing:
                    log(f"[runner] WARN volume {op.get('id')!r} arrived after close; ignored")
                    continue
                self.op_volume(op)
        except Exception as e:  # a broken stdin is EOF with a note
            log(f"[runner] WARN stdin ended: {e}")
        finally:
            self.op_close()

    # -- the session --------------------------------------------------------

    def serve(self) -> int:
        started = time.time()
        try:
            pipe = OpenPipeline(SessionConfig.from_args(self.args))
            self.pipe = pipe
            # A session ANNOUNCES itself: a model that will never load is a
            # fact about the session, said once, rather than a volume of
            # blank pages discovered a page at a time.
            pipe.wait_ready()
        except Exception as e:
            log(f"[runner] ERROR: {e}")
            log(traceback.format_exc())
            if self.pipe is not None:
                self.pipe.close()
            self.protocol.emit("fatal", error=str(e))
            return 1
        self.session = Session(pipe, protocol=self.protocol)
        self.feed = VolumeFeed(
            spool=pipe.road == ROAD_ADAPTER,
            opening=self.session.open_window,
        )
        self.protocol.emit(
            "ready",
            startup_seconds=round(time.time() - started, 3),
            weights=pipe.weights_so_far(),
            stage_workers=pipe.stage_workers(),
            queue_capacity=pipe.queue_capacity(),
            stage_device=pipe.stage_device(),
            pipeline=pipe.graph_line(),
        )
        log("[runner] session ready; waiting for volumes")
        reader = threading.Thread(target=self.read_ops, name="ocr-ops", daemon=True)
        reader.start()
        stream = pipe.pipeline.run(self.feed)
        try:
            with contextlib.ExitStack() as scope:
                # Reverse order: the FEED first, so a teardown does not wait
                # on a feeder parked on the next volume that is never coming;
                # then the detector children; then the stages.
                scope.callback(stream.close)
                scope.callback(pipe.close)
                scope.callback(self.feed.close)
                self.session.consume(stream, finish=True)
        except Exception as e:
            log(f"[runner] ERROR: the pipeline ended: {e}")
            log(traceback.format_exc())
            self._abandon(str(e))
            self.protocol.emit("fatal", error=str(e))
            return 1
        if self.session.load_failed is not None:
            error = f"{self.args.engine} failed to load: {self.session.load_failed}"
            log(f"[runner] ERROR: {error}")
            log("".join(traceback.format_exception(self.session.load_failed)))
            self._abandon(error)
            self.protocol.emit("fatal", error=error)
            return 1
        self._abandon("the session ended before this volume did")
        log("[runner] session closed")
        return 1 if self._tripped else 0

    def _abandon(self, error: str) -> None:
        """Whatever was accepted and never finished: fail it, once."""
        session = self.session
        if session is None:
            return
        for volume in list(session.runs.values()):
            session.fail(volume, error)


def serve(args: argparse.Namespace, *, stdout: Any = None) -> int:
    """One pipeline, volume after volume, until ``close`` or EOF.

    ``stdout`` is the seam the tests take: given one, the protocol is written
    there and fd 1 is left alone. Everything else -- the logging going to the
    session log and to each volume's log, the events, the ordering -- is the
    same code the real mode runs, because that is what there is to test.
    """
    protocol = Protocol(stdout if stdout is not None else seize_stdout(Path(args.session_log)))
    LOG.to_file(Path(args.session_log))
    if stdout is None:
        _die_cleanly()
    return ServeSession(args, protocol).serve()


def _die_cleanly() -> None:
    """SIGTERM unwinds instead of killing, so the children go with us.

    A detector adapter exits when its stdin closes, which happens when this
    process dies however it dies -- but an unwound exit closes the pipes NOW
    rather than when the kernel gets round to it, and joins the children.
    ``SystemExit`` on the main thread runs the ExitStack that owns them.
    """
    import signal

    def stop(_signum: int, _frame: Any) -> None:
        raise SystemExit(143)

    with contextlib.suppress(ValueError, OSError):
        signal.signal(signal.SIGTERM, stop)

# ---------------------------------------------------------------------------
# ``--bench``: the same process shape as a session, measuring itself.
#
# Load everything ONCE, warm up, then TRIALS -- full passes over the same
# sample pages, in the same order, with only the POOLS rebuilt between them.
# The recognizer is never reloaded; detector subprocesses and ppocr sessions
# are created or closed as widths change. So a trial costs a pass over the
# pages, and the search is affordable on a user's own machine over their own
# pages, which is the point: no number shown for a generation's speed is ever
# this dev box's.
#
# The search follows the pipeline's OWN verdict rather than a grid: widen the
# stage the numbers name, keep the step if it pays, stop when it stops paying,
# then give back what is not being used.
# ---------------------------------------------------------------------------

# A widening step must buy at least this much throughput to be kept.
BENCH_GAIN = 0.03
# A narrowing step may cost at most this much to be kept: the same speed for
# fewer cores is a win worth reporting.
BENCH_HOLD = 0.01
BENCH_MAX_TRIALS = 8
BENCH_BUDGET_SECONDS = 900.0
# Pages of the sample thrown away before the first trial, to pay for whatever
# a first page costs that a hundredth does not (lazy kernels, allocator
# growth, a cold page cache).
BENCH_WARMUP_PAGES = 4
BENCH_PROGRESS_INTERVAL = 1.0

# ADDENDUM 9. A trial re-feeds the same sample pages until the MEASURED
# WINDOW -- first emission after the fill to last emission, over every pass --
# is this long. A 3% decision taken over six seconds is a decision about
# noise, and the widths a benchmark hands back are kept for the life of a
# library.
BENCH_MIN_WINDOW_SECONDS = 20.0
# ... but a sample can be too small to reach that however it is fed, so the
# loop is bounded as well. The flag below is what the UI says when even that
# was not enough.
BENCH_MAX_PASSES = 8
# How many times one FEED may repeat the sample. A feed is one continuous run
# (see :meth:`BenchRun._pass`), so this bounds a single trial's length: 64 x a
# 32-page sample is 2048 pages, far past the 20 s the window needs on any
# engine worth benchmarking, and it stops a pathological rate estimate from
# asking for a feed that never ends.
BENCH_MAX_REPEAT = 64
# A feed's own window is taken as its steady-state rate only when it spans
# at least this much of the feed. Below it the feed emitted in a burst, its
# window says nothing about throughput, and only the wall clock is left.
BENCH_WINDOW_IS_THE_FEED = 0.25
# Under this, the number is reported and flagged and NEVER decided on.
BENCH_SHORT_WINDOW_SECONDS = 10.0


def bench_fill(pages: int) -> int:
    """Emissions discarded as pipeline fill: the first ``min(8, N/4)``.

    A fill is all starvation by construction -- every stage but the first is
    waiting for something to exist -- and it is a ONE-TIME cost exactly like
    the model load, so it is skipped rather than averaged in. It is counted
    in EMISSIONS, not seconds, and only ever comes off the head of the FIRST
    pass (ADDENDUM 9).
    """
    return min(MIN_PAGES, max(0, pages // 4))


class BenchWindow(NamedTuple):
    """A trial's rate, and the window it is a rate over.

    ADDENDUM 9: a benchmark is timed by the instants its per-page results
    LEAVE the pipeline, and by nothing else. Model load, imports, detector
    spawn and pipeline fill all happen before or around those instants and
    none of them can enter this.
    """

    pages_per_second: float
    window_seconds: float
    pages_measured: int
    passes: int
    short_window: bool
    first_emission_at: float | None
    last_emission_at: float | None

    def as_dict(self) -> dict[str, Any]:
        return {
            "window_seconds": round(self.window_seconds, 3),
            "pages_measured": self.pages_measured,
            "passes": self.passes,
            "short_window": self.short_window,
            "first_emission_at": (
                None if self.first_emission_at is None else round(self.first_emission_at, 3)
            ),
            "last_emission_at": (
                None if self.last_emission_at is None else round(self.last_emission_at, 3)
            ),
        }


def bench_window(emissions: Sequence[float], *, fill: int, passes: int = 1) -> BenchWindow:
    """The rate of ``emissions`` (monotonic seconds), fill skipped.

    ``(M - 1) / (t_last - t_first)`` over the measured set, and not
    ``M / (t_last - t_first)``: M emissions delimit M-1 intervals, and
    dividing the count by the span of its own interior would overstate the
    rate by M/(M-1) -- 14% on eight pages.

    The window is the SPAN OF THE MEASURED SET, so a burst cannot shrink it:
    a hundred emissions inside one microsecond contribute their count to M
    and nothing at all to the span, which is what the passes above are for.
    A window that is still degenerate after them yields no rate (0.0) rather
    than the six-figure ones the old post-fill window produced -- and either
    way ``short_window`` says the number must not be decided on.
    """
    measured = list(emissions[max(0, fill) :])
    first = measured[0] if measured else None
    last = measured[-1] if measured else None
    window = (last - first) if (first is not None and last is not None) else 0.0
    rate = (len(measured) - 1) / window if window > 0 and len(measured) > 1 else 0.0
    return BenchWindow(
        pages_per_second=rate,
        window_seconds=max(0.0, window),
        pages_measured=len(measured),
        passes=max(0, passes),
        short_window=window < BENCH_SHORT_WINDOW_SECONDS,
        first_emission_at=first,
        last_emission_at=last,
    )


def merge_reports(reports: Sequence[PipelineReport]) -> PipelineReport:
    """Several passes' counters as one reading.

    A trial is now however many passes it took to fill the window, and the
    verdict has to be about the trial rather than about whichever pass
    happened to be last. Every counter here is cumulative-from-zero per pass
    (a pass builds a fresh :class:`StagePipeline`), so they ADD -- and the
    two ratios, ``utilisation`` and ``mean_depth``, are re-derived from the
    sums rather than averaged, because a ratio cannot be.
    """
    usable = [report for report in reports if report is not None]
    if not usable:
        raise ValueError("no reports to merge")
    if len(usable) == 1:
        return usable[0]
    elapsed = sum(report.elapsed for report in usable)
    stages: list[StageReport] = []
    for index, first in enumerate(usable[0].stages):
        busy = sum(report.stages[index].busy_seconds for report in usable)
        workers = max(1, first.workers)
        stages.append(
            first._replace(
                items=sum(report.stages[index].items for report in usable),
                busy_seconds=busy,
                blocked_seconds=sum(report.stages[index].blocked_seconds for report in usable),
                starved_seconds=sum(report.stages[index].starved_seconds for report in usable),
                utilisation=busy / (workers * elapsed) if elapsed > 0 else 0.0,
            )
        )
    queues: list[QueueReport] = []
    for index, first_queue in enumerate(usable[0].queues):
        depth_seconds = sum(report.queues[index].depth_seconds for report in usable)
        queues.append(
            first_queue._replace(
                depth=usable[-1].queues[index].depth,
                max_depth=max(report.queues[index].max_depth for report in usable),
                mean_depth=depth_seconds / elapsed if elapsed > 0 else 0.0,
                depth_seconds=depth_seconds,
                puts=sum(report.queues[index].puts for report in usable),
                gets=sum(report.queues[index].gets for report in usable),
                blocked_seconds=sum(report.queues[index].blocked_seconds for report in usable),
                blocked_events=sum(report.queues[index].blocked_events for report in usable),
                starved_seconds=sum(report.queues[index].starved_seconds for report in usable),
                starved_events=sum(report.queues[index].starved_events for report in usable),
            )
        )
    return PipelineReport(
        elapsed=elapsed,
        items=sum(report.items for report in usable),
        stages=tuple(stages),
        queues=tuple(queues),
    )


class BenchTrial(NamedTuple):
    """One full pass over the sample at one set of pool widths."""

    n: int
    note: str
    stage_workers: dict[str, int]
    queue_capacity: dict[str, int]
    # Where each model-bearing stage ran for this trial: a width means
    # nothing without it ("detect ×3" is a different pipeline on the CPU and
    # on a card).
    stage_device: dict[str, str]
    seconds: float
    # The rate and the window it was read over (ADDENDUM 9). `seconds` above
    # is the wall time of the trial's passes and is informational: it
    # includes the fill and the gaps between passes, and nothing compares it.
    window: BenchWindow
    accepted: bool
    verdict: str | None
    bottleneck: str | None
    stages: list[dict[str, Any]]
    queues: list[dict[str, Any]]
    # The full reading this trial produced (:func:`summarize`), kept for the
    # SEARCH and never emitted: the trial object the server shows carries the
    # percentages a person reads, and the search needs the flags underneath
    # them (which stage is fused, which cannot be widened at all).
    reading: Any = None
    # The precision the recognizer ran at in this trial, where it has one
    # (a torch recognizer): a record, never something the search varies.
    precision: str | None = None

    @property
    def pages_per_second(self) -> float:
        return self.window.pages_per_second

    @property
    def short_window(self) -> bool:
        return self.window.short_window

    def as_dict(self) -> dict[str, Any]:
        return {
            "n": self.n,
            "note": self.note,
            "stage_workers": self.stage_workers,
            "queue_capacity": self.queue_capacity,
            "stage_device": self.stage_device,
            "seconds": round(self.seconds, 3),
            "pages_per_second": round(self.pages_per_second, 4),
            **self.window.as_dict(),
            "accepted": self.accepted,
            "verdict": self.verdict,
            "bottleneck": self.bottleneck,
            "stages": self.stages,
            "queues": self.queues,
            **({"precision": self.precision} if self.precision else {}),
        }


def pipe_index(pipe: OpenPipeline, key: str) -> int:
    """Where this stage sits in the pipeline's own order."""
    return [spec.key for spec in pipe.specs].index(key)


def bench_rows(report: PipelineReport) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    """A trial's stage and queue rows, and nothing the caller has to re-derive.

    The same rows the congestion history stores, read the same way the queue
    page reads them (:func:`summarize`), so a trial and a real run say the
    same thing about the same pipeline.
    """
    summary = summarize(report.as_dict()) or {"stages": []}
    stages = [
        {
            "key": row["key"],
            "workers": row["workers"],
            "busy_pct": int(round(row["busy_pct"])),
            "starved_pct": int(round(row["starved_pct"] or 0)),
            "blocked_pct": int(round(row["blocked_pct"] or 0)),
        }
        for row in summary["stages"]
    ]
    queues = [
        {
            "name": q.name,
            "capacity": q.capacity,
            "mean_depth": round(q.mean_depth, 2),
            "max_depth": q.max_depth,
        }
        for q in report.queues
    ]
    return stages, queues


def target_formats(target: Any) -> frozenset[str]:
    """What a precision target's device supports: its own answer, else torch's."""
    own = getattr(target, "supported", None)
    if callable(own):
        return frozenset(own())
    return supported_formats(target.torch, target.device)


class BenchRun:
    """``--bench``: trials over a sample, and the widths they argue for."""

    def __init__(self, args: argparse.Namespace, protocol: Protocol) -> None:
        self.args = args
        self.protocol = protocol
        self.max_trials = max(1, int(getattr(args, "bench_max_trials", BENCH_MAX_TRIALS)))
        self.budget = float(getattr(args, "bench_budget_seconds", BENCH_BUDGET_SECONDS))
        self.pipe: OpenPipeline | None = None
        self.scratch: Path | None = None
        self.sample: list[Path] = []
        self.trials: list[BenchTrial] = []
        # Pages handed to the pipeline over the whole benchmark (see _pass).
        self._seq = 0
        # The precision the recognizer runs at once the benchmark has settled
        # it; None where the engine fixes its own. The row's MODE decides
        # whether the benchmark varies it: a balanced/speed mode with more
        # than one supported candidate gets one trial per candidate
        # (:meth:`precision_phase`), every other mode runs as it resolves.
        self.precision: str | None = None
        self.mode = normalize_precision_mode(getattr(args, "precision", None))
        # ``--bench-precision-only``: the candidate trials at the pools as
        # given, and nothing else -- no width search, no placement. What a
        # machine with hand-set pools is benchmarked with for its pick.
        self.precision_only = bool(getattr(args, "bench_precision_only", False))
        self.precision_trials: list[int] = []
        self.precision_why = ""
        # Armed here as well as in :meth:`run`, because a deadline defaulting
        # to "already past" would make a search driven any other way do
        # nothing at all, and do it silently.
        self._deadline = time.monotonic() + self.budget

    # -- one pass over the sample ------------------------------------------

    def _pass(
        self,
        pages: Sequence[Path],
        widths: Sequence[int],
        *,
        trial: int,
        emissions: list[float] | None = None,
        fill: int = 0,
        pass_index: int = 1,
        repeat: int = 1,
    ) -> tuple[float, list[float], PipelineReport]:
        """One pass over ``pages`` at these widths: (wall seconds, emissions, report).

        ADDENDUM 9: the only clock is the instant each page's result LEAVES
        the pipeline -- the same instant its JSON would be written -- and
        every one of them is recorded. What is done with them belongs to
        :meth:`_measure`; nothing is averaged, skipped or divided here.

        ``repeat`` feeds the sample that many times AS ONE CONTINUOUS FEED --
        one job list, one pipeline run, no boundary in the middle. That is
        the difference between measuring the engine and measuring the seams
        around it: a served engine (ADDENDUM 8) holds its last partial batch
        until the volume ends, so N separate begin/end cycles emit N bursts
        and a rate read across them is reading the gaps BETWEEN volumes. Fed
        as one long volume, the batches fire continuously and only the final
        partial one bursts.

        Each repetition gets its own detection and cache directory, because
        the same page twice in flight is two jobs writing one file otherwise.

        ``emissions`` is the trial's running list so that a progress event
        can report the rate over everything measured SO FAR, across passes,
        rather than over this pass alone.
        """
        pipe = cast("OpenPipeline", self.pipe)
        scratch = cast("Path", self.scratch)
        pipeline = pipe.rebuild(widths)
        jobs: list[PageJob] = []
        for index in range(max(1, repeat)):
            paths = VolumePaths(
                input_dir=Path(self.args.input),
                detect_dir=scratch / "detect" / f"{pass_index}-{index}",
                cache_dir=scratch / "cache" / f"{pass_index}-{index}",
                id="bench",
            )
            jobs += [PageJob(paths, rel) for rel in pages]
        # ONE volume, however many repetitions it is: to anything that frames
        # volumes (the served road's engine) a pass is a single begin/end, and
        # that is the whole point of the repeat -- N framed volumes would emit
        # N bursts and the window would be reading the gaps between them. The
        # numbering runs on across passes as well, so an ordered consumer never
        # sees it go backwards.
        base = self._seq
        self._seq += len(jobs)
        jobs = [
            job._replace(seq=base + index, last=index == len(jobs) - 1)
            for index, job in enumerate(jobs)
        ]
        so_far = emissions if emissions is not None else []
        mine: list[float] = []
        stream = pipeline.run(jobs)
        started = time.monotonic()
        last = started
        announced = 0.0
        failed = 0
        with contextlib.ExitStack() as scope:
            scope.callback(stream.close)
            for _job, outcome in stream:
                if outcome.error is not None:
                    failed += 1
                last = time.monotonic()
                mine.append(last - PROCESS_STARTED)
                so_far.append(last - PROCESS_STARTED)
                if trial and last - announced >= BENCH_PROGRESS_INTERVAL:
                    announced = last
                    running = bench_window(so_far, fill=fill, passes=pass_index)
                    self.protocol.emit(
                        "bench_progress",
                        trial=trial,
                        pass_index=pass_index,
                        pages_done=len(mine),
                        pages=len(jobs),
                        stage_workers=self._map(widths),
                        pages_per_second=round(running.pages_per_second, 4),
                        window_seconds=round(running.window_seconds, 3),
                        pages_measured=running.pages_measured,
                    )
        report = pipeline.report()
        seconds = max(1e-9, last - started)
        if failed:
            log(f"[runner] WARN {failed} of {len(jobs)} sample pages failed at these widths")
        return seconds, mine, report

    def _sized_repeat(self, pages: int, feed: Sequence[float], seconds: float, was: int) -> int:
        """How many times to feed the sample so ONE feed fills the window.

        Sized from the STEADY-STATE rate where the feed produced one, and
        from its wall clock where it did not. The difference matters: a short
        feed spends a real share of its wall time filling the pipeline, so
        pages-over-wall-time understates the rate a longer feed will run at
        and the sized feed comes out short -- which costs a whole extra feed.
        The window of the same feed already has the fill taken off it.

        The window is only believed when it actually spans the feed. A feed
        that emitted in a burst has a window covering a fraction of its work
        and a rate in the thousands (that is the whole defect this design is
        about), and sizing from that would ask for a feed that never ends;
        there the wall clock is the only honest estimate left.
        """
        done = len(feed)
        wall_rate = done / seconds if seconds > 0 and done else 0.0
        window = bench_window(feed, fill=bench_fill(pages))
        rate = wall_rate
        if (
            window.window_seconds >= BENCH_WINDOW_IS_THE_FEED * seconds
            and window.pages_per_second > wall_rate
        ):
            rate = window.pages_per_second
        if rate <= 0:
            return min(BENCH_MAX_REPEAT, max(was + 1, was * 2))
        wanted = bench_fill(pages) + int(BENCH_MIN_WINDOW_SECONDS * 1.2 * rate) + 1
        needed = -(-wanted // max(1, pages))  # ceil
        return max(was + 1, min(BENCH_MAX_REPEAT, needed))

    def _measure(
        self,
        pages: Sequence[Path],
        widths: Sequence[int],
        *,
        trial: int,
    ) -> tuple[float, BenchWindow, PipelineReport]:
        """Feed the sample until the measured window is worth deciding on.

        ADDENDUM 9: the same pages are re-fed (there is no OCR cache to warm,
        and the runner writes nothing for what it reads here) until the window
        reaches :data:`BENCH_MIN_WINDOW_SECONDS`, or until
        :data:`BENCH_MAX_PASSES` feeds have run.

        The re-feed is ONE LONGER FEED, not more feeds: the first is the
        sample once, and if that was too short the next repeats the sample
        inside a single continuous run, sized from what the first one
        measured. A road that holds its last batch until the feed ends (a
        served engine, ADDENDUM 8) emits one burst per FEED, so chaining
        feeds would put the gaps between them inside the window -- while one
        long feed bursts only at its very end, where it is one page in
        hundreds.

        The window is that one feed's own emissions whenever that feed
        reached the minimum. Only when no single feed ever does -- a road
        that bursts everything however long the feed -- does it fall back to
        spanning every feed, and then the rate is (M-1) over that whole span,
        which still cannot be the burst's rate. That fallback understates (it
        has the gaps in it) and it is flagged when short, which is the right
        direction for a number nothing may be decided on.
        """
        per_feed: list[list[float]] = []
        reports: list[PipelineReport] = []
        repeats: list[int] = []
        seconds = 0.0
        feeds = 0
        repeat = 1
        fill = bench_fill(len(pages))
        while feeds < BENCH_MAX_PASSES:
            feeds += 1
            elapsed, mine, report = self._pass(
                pages,
                widths,
                trial=trial,
                emissions=[stamp for feed in per_feed for stamp in feed],
                fill=fill,
                pass_index=feeds,
                repeat=repeat,
            )
            seconds += elapsed
            per_feed.append(mine)
            repeats.append(repeat)
            reports.append(report)
            window = bench_window(mine, fill=fill, passes=repeat)
            if window.window_seconds >= BENCH_MIN_WINDOW_SECONDS:
                break
            if time.monotonic() >= self._deadline:
                log(
                    f"[runner] bench: out of time after {feeds} feed(s); the window is "
                    f"{window.window_seconds:.1f}s"
                )
                break
            grown = self._sized_repeat(len(pages), mine, elapsed, repeat)
            if grown <= repeat:
                break
            log(
                f"[runner] bench: a {window.window_seconds:.1f}s window over "
                f"{len(mine)} page(s) is too short -- feeding the sample x{grown} "
                "as one run"
            )
            repeat = grown
        window = bench_window(per_feed[-1], fill=fill, passes=repeats[-1])
        if window.window_seconds < BENCH_MIN_WINDOW_SECONDS and len(per_feed) > 1:
            # No single feed was ever long enough: span them all rather than
            # report the last burst.
            window = bench_window(
                [stamp for feed in per_feed for stamp in feed],
                fill=fill,
                passes=sum(repeats),
            )
        if window.short_window:
            log(
                f"[runner] bench: WARN the measured window is only "
                f"{window.window_seconds:.1f}s over {window.pages_measured} pages after "
                f"{feeds} feed(s) -- too short to decide anything on"
            )
        return seconds, window, merge_reports(reports)

    def _map(self, widths: Sequence[int]) -> dict[str, int]:
        pipe = cast("OpenPipeline", self.pipe)
        return {spec.key: int(w) for spec, w in zip(pipe.specs, widths, strict=True)}

    def _trial(self, widths: Sequence[int], *, note: str) -> BenchTrial:
        n = len(self.trials) + 1
        log(f"[runner] bench trial {n} ({note}): {self._map(widths)}")
        seconds, window, report = self._measure(self.sample, widths, trial=n)
        stages, queues = bench_rows(report)
        summary = summarize(report.as_dict())
        pipe = cast("OpenPipeline", self.pipe)
        trial = BenchTrial(
            n=n,
            note=note,
            stage_workers=self._map(widths),
            queue_capacity={
                spec.key: int(cap) for spec, cap in zip(pipe.specs, pipe.caps, strict=True)
            },
            stage_device=pipe.stage_device(),
            seconds=seconds,
            window=window,
            accepted=True,
            verdict=(summary or {}).get("verdict"),
            bottleneck=(summary or {}).get("bottleneck"),
            stages=stages,
            queues=queues,
            reading=summary,
            precision=self.precision,
        )
        log(
            f"[runner] bench trial {n}: {window.pages_per_second:.3f} pages/s over "
            f"{window.window_seconds:.1f}s of emissions ({window.pages_measured} pages, "
            f"{window.passes} pass(es), {seconds:.1f}s wall)"
            + (" SHORT WINDOW" if window.short_window else "")
            + f"; verdict: {trial.verdict or 'balanced'}"
        )
        self.trials.append(trial)
        return trial

    def _decidable(self, candidate: BenchTrial, against: BenchTrial) -> bool:
        """May these two trials be COMPARED at all? (ADDENDUM 9)

        A rate read over a window shorter than
        :data:`BENCH_SHORT_WINDOW_SECONDS` is noise, so a change is never
        accepted on one -- neither on a short candidate (its number could be
        anything) nor against a short incumbent (the bar it has to clear
        could be anything). The trial is still measured, still emitted and
        still shown; it just cannot move the widths.
        """
        if not candidate.short_window and not against.short_window:
            return True
        which = "the candidate" if candidate.short_window else "the trial it is measured against"
        log(
            f"[runner] bench: trial {candidate.n} cannot decide anything -- {which} has a "
            f"{min(candidate.window.window_seconds, against.window.window_seconds):.1f}s "
            "window; add pages to the sample"
        )
        return False

    def _emit(self, trial: BenchTrial, accepted: bool) -> BenchTrial:
        settled = trial._replace(accepted=accepted)
        self.trials[trial.n - 1] = settled
        self.protocol.emit("bench_trial", **settled.as_dict())
        return settled

    # -- the search ---------------------------------------------------------

    def _ceiling(self, index: int) -> int:
        pipe = cast("OpenPipeline", self.pipe)
        spec = pipe.specs[index]
        structural = pipe.host_budget if spec.max_workers is None else spec.max_workers
        return max(1, min(structural, pipe.host_budget))

    def _tunable(self, widths: Sequence[int]) -> bool:
        return any(width < self._ceiling(i) for i, width in enumerate(widths))

    def _out_of_time(self) -> bool:
        return time.monotonic() >= self._deadline or len(self.trials) >= self.max_trials

    def search(
        self, derived: Sequence[int], start: BenchTrial | None = None
    ) -> tuple[BenchTrial, BenchTrial, list[int]]:
        """The width search from ``derived``: (baseline, best, widths).

        ``start`` is a trial already measured AT those widths (the precision
        trial that won), which is then the baseline instead of a fresh one.
        """
        pipe = cast("OpenPipeline", self.pipe)
        keys = [spec.key for spec in pipe.specs]
        widths = list(derived)
        baseline = start if start is not None else self._emit(
            self._trial(widths, note="auto"), True
        )
        best = baseline
        seen = {tuple(widths)}

        # 1. WIDEN what the pipeline says is in the way, while it pays.
        while not self._out_of_time():
            target = widen_target(best.reading or {})
            if target is None or target not in keys:
                log(f"[runner] bench: nothing left to widen ({target or 'balanced'})")
                break
            index = keys.index(target)
            if widths[index] >= self._ceiling(index):
                log(f"[runner] bench: {target} is already at its ceiling {self._ceiling(index)}")
                break
            candidate = list(widths)
            candidate[index] += 1
            if tuple(candidate) in seen:
                break
            seen.add(tuple(candidate))
            trial = self._trial(candidate, note=f"widen {target} to {candidate[index]}")
            if not self._decidable(trial, best):
                self._emit(trial, False)
                break
            if trial.pages_per_second >= best.pages_per_second * (1 + BENCH_GAIN):
                self._emit(trial, True)
                widths, best = candidate, trial
                continue
            self._emit(trial, False)
            log(
                f"[runner] bench: widening {target} bought "
                f"{trial.pages_per_second / best.pages_per_second - 1:+.1%}; keeping "
                f"{best.stage_workers}"
            )
            break

        # 2. NARROW what is not being used, while throughput HOLDS. Every
        #    narrowing is judged against the PEAK, not against the step before
        #    it, so a chain of "within 1%" steps cannot walk the pipeline down.
        peak = best.pages_per_second
        peak_trial = best
        for index in sorted(range(len(widths)), key=lambda i: -widths[i]):
            while widths[index] > 1 and not self._out_of_time():
                candidate = list(widths)
                candidate[index] -= 1
                if tuple(candidate) in seen:
                    break
                seen.add(tuple(candidate))
                trial = self._trial(
                    candidate, note=f"narrow {keys[index]} to {candidate[index]}"
                )
                # Giving a core back is a CHANGE, so it needs a window that
                # can tell "holds within 1%" from "is 40% slower" -- the same
                # bar a widening has to clear.
                if not self._decidable(trial, peak_trial):
                    self._emit(trial, False)
                    break
                if trial.pages_per_second < peak * (1 - BENCH_HOLD):
                    self._emit(trial, False)
                    break
                self._emit(trial, True)
                widths, best = candidate, trial
        return baseline, best, widths

    def precision_phase(self, widths: Sequence[int]) -> BenchTrial | None:
        """One trial per supported candidate of a balanced/speed mode; keep the fastest.

        Only for a torch recognizer on a card, loaded in fp32 for this very
        purpose (``pipe.precision_target()``): every candidate is cast from
        the fp32 master copy -- never from the last cast, never reloaded --
        and measured with ONE trial at ``widths``, on the same sample, like
        any other trial. The fastest wins; within :data:`PRECISION_TIE` the
        earlier candidate (the more accurate) wins (:func:`pick_precision`).
        This is what finds a card's EMULATED bf16 slower than its fp32, with
        no list of cards anywhere.

        With one supported candidate there is nothing to try: the recognizer
        is set to it and no trial is spent. Returns the winning trial (the
        width search's baseline) or None.
        """
        pipe = cast("OpenPipeline", self.pipe)
        target = pipe.precision_target()
        if target is None:
            return None
        engine = str(getattr(self.args, "engine", "") or "")
        supported = target_formats(target)
        usable = resolve_mode(engine, self.mode, supported).usable
        if len(usable) < 2:
            chosen = usable[0] if usable else PRECISION_FP32
            self._settle_precision(target, chosen, self.mode)
            return None
        log(
            f"[runner] bench: precision trials -- {', '.join(usable)} ({self.mode}; "
            f"supported here: {', '.join(sorted(supported))}) at {self._map(widths)}"
        )
        measured: list[tuple[str, float]] = []
        by_format: dict[str, BenchTrial] = {}
        for candidate in usable:
            if measured and time.monotonic() >= self._deadline:
                log(f"[runner] bench: out of time before trying {candidate}")
                break
            try:
                target.set_precision(candidate)
            except Exception as e:  # noqa: BLE001 - a cast that fails costs the candidate
                log(f"[runner] bench: could not switch to {candidate}: {e}")
                continue
            self.precision = candidate
            trial = self._trial(widths, note=f"precision {candidate}")
            self.precision_trials.append(trial.n)
            by_format[candidate] = trial
            measured.append((candidate, trial.pages_per_second))
            log(f"[runner] bench: {candidate}: {trial.pages_per_second:.3f} pages/s")
        picked = pick_precision(measured, usable)
        if picked is None:
            self._settle_precision(target, usable[0], self.mode)
            return None
        chosen, why = picked
        for fmt, trial in by_format.items():
            self._emit(trial, fmt == chosen)
        self._settle_precision(target, chosen, f"{self.mode}; {why}")
        self.precision_why = why
        return self.trials[by_format[chosen].n - 1]

    def _settle_precision(self, target: Any, chosen: str, why: str) -> None:
        """Leave the recognizer at ``chosen``, its master copy released, and say so."""
        target.set_precision(chosen)
        release = getattr(target, "release_master", None)
        if release is not None:
            release()
        loader = getattr(self.pipe, "loader", None)
        if loader is not None:
            loader.precision = chosen
        self.precision = chosen
        log(f"[runner] {getattr(self.args, 'engine', '')} precision: {chosen} ({why})")

    def place(self, best: BenchTrial, widths: list[int]) -> tuple[BenchTrial, list[int]]:
        """Try the DETECTOR on every other device, and keep the best place.

        After the widths, because where a stage runs decides what its width
        can even be: on a card it is one model on one device, on the CPU it is
        a pool the host budget sizes. Each candidate is measured at ITS derived
        width (``cpu x3``), and a move that wins re-runs the width search on
        the winner -- the widths that suited the old placement argue nothing
        about the new one.

        The ENGINE's device is NOT explored: moving a VLM to the CPU is an
        order-of-magnitude decision a user makes on purpose, and with several
        cards it is a pin, not a search.
        """
        pipe = cast("OpenPipeline", self.pipe)
        candidates = pipe.detect_devices()
        if not candidates or self._out_of_time():
            return best, widths
        here = pipe._stage_device(STAGE_DETECT)
        champion, champion_widths, champion_device = best, widths, here
        for device in candidates:
            if self._out_of_time():
                break
            try:
                pipe.move_detect(device)
            except Exception as e:
                # A detector that refuses a device (a CPU-only adapter, a card
                # that will not load it) costs this candidate, not the run.
                log(f"[runner] bench: detect could not move to {device}: {e}")
                continue
            derived = list(pipe.widths)
            trial = self._trial(
                derived, note=f"detect on {device} x{derived[pipe_index(pipe, STAGE_DETECT)]}"
            )
            if not self._decidable(trial, champion):
                self._emit(trial, False)
                continue
            if trial.pages_per_second >= champion.pages_per_second * (1 + BENCH_GAIN):
                self._emit(trial, True)
                champion, champion_widths, champion_device = trial, derived, device
                continue
            self._emit(trial, False)
        if champion_device != here:
            # The winner is where the pipeline is right now only if it was the
            # LAST candidate tried; otherwise put it back.
            if pipe._stage_device(STAGE_DETECT) != champion_device:
                pipe.move_detect(champion_device)
            if not self._out_of_time():
                _, champion, champion_widths = self.search(list(pipe.widths))
        elif pipe._stage_device(STAGE_DETECT) != here:
            pipe.move_detect(here)
        return champion, champion_widths

    # -- the run ------------------------------------------------------------

    def run(self) -> int:
        self._deadline = time.monotonic() + self.budget
        sample_dir = Path(self.args.input)
        self.sample = list_pages(sample_dir)
        if not self.sample:
            log(f"[runner] ERROR: no page images found under {sample_dir}")
            self.protocol.emit("fatal", error=f"no page images found under {sample_dir}")
            return 1
        scratch = tempfile.mkdtemp(prefix="mokuro-bench-")
        self.scratch = Path(scratch)
        try:
            return self._run()
        finally:
            # Nothing a benchmark reads leaves anything behind: no sidecar, no
            # cache that outlives the run.
            shutil.rmtree(scratch, ignore_errors=True)
            if self.pipe is not None:
                self.pipe.close()

    def _run(self) -> int:
        started = time.monotonic()
        # The recognizer loads as a run would -- except for a balanced/speed
        # mode on a torch recognizer, whose candidates the benchmark tries
        # (:meth:`precision_phase`): that one loads in fp32, the exact weights
        # every candidate's cast starts from.
        config = SessionConfig.from_args(self.args)
        if self.mode in BENCHED_PRECISION_MODES and config.engine in TORCH_PRECISION_ENGINES:
            config = config._replace(precision=PRECISION_FP32, precision_pick=None)
        try:
            pipe = OpenPipeline(config)
            self.pipe = pipe
            pipe.wait_ready()
        except Exception as e:
            log(f"[runner] ERROR: {e}")
            log(traceback.format_exc())
            self.protocol.emit("fatal", error=str(e))
            return 1
        loaded = time.monotonic() - started
        derived = list(pipe.widths)
        # Where the spec asked for each model, resolved, so ``best`` can say
        # only what the search MOVED.
        asked_devices = pipe.stage_device()
        # A short discarded pass, so the first TRIAL is not the one paying for
        # whatever a first page costs that a hundredth does not.
        warm = self.sample[: min(len(self.sample), BENCH_WARMUP_PAGES)]
        try:
            _seconds, warm_emissions, _report = self._pass(warm, derived, trial=0)
        except Exception as e:
            log(f"[runner] ERROR: the warm-up pass failed: {e}")
            log(traceback.format_exc())
            self.protocol.emit("fatal", error=str(e))
            return 1
        # ADDENDUM 9: startup is "first page after X s" -- everything from
        # process start to the instant the first result came out, imports and
        # model load and detector spawn and fill together. It is reported
        # ONCE, it is informational, and no rate or estimate may add it.
        startup = (
            warm_emissions[0]
            if warm_emissions
            else time.monotonic() - PROCESS_STARTED
        )
        log(
            f"[runner] bench: first page after {startup:.1f}s "
            f"(of which {loaded:.1f}s was loading the models)"
        )
        tunable = self._tunable(derived) and not self.precision_only
        self.precision = pipe.precision()
        target = (
            pipe.precision_target()
            if self.mode in BENCHED_PRECISION_MODES and pipe.engine in PRECISION_ENGINES
            else None
        )
        phase = 0
        if target is not None:
            usable = resolve_mode(pipe.engine, self.mode, target_formats(target)).usable
            phase = len(usable) if len(usable) > 1 else 0
        # The precision trials are the phase's own: they never eat the width
        # search's trials (the time budget they share).
        self.max_trials += phase
        self.protocol.emit(
            "bench_ready",
            startup_seconds=round(startup, 3),
            model_load_seconds=round(loaded, 3),
            min_window_seconds=BENCH_MIN_WINDOW_SECONDS,
            pages=len(self.sample),
            tunable=tunable,
            max_trials=(self.max_trials - phase if tunable else 1) + phase,
            stage_keys=[spec.key for spec in pipe.specs],
            stage_device=pipe.stage_device(),
        )
        winner = self.precision_phase(derived) if target is not None else None
        if self.precision:
            log(f"[runner] bench: the recognizer runs at {self.precision} from here on")
        if not tunable:
            log(
                "[runner] bench: precision only; the pools stay as given"
                if self.precision_only
                else "[runner] bench: nothing on this road can be widened; one trial only"
            )
            baseline = winner or self._emit(self._trial(derived, note="auto"), True)
            best, widths = baseline, derived
        else:
            baseline, best, widths = self.search(derived, start=winner)
            best, widths = self.place(best, widths)
        keys = [spec.key for spec in pipe.specs]
        placed = pipe.stage_device()
        self.protocol.emit(
            "bench_done",
            baseline={
                "pages_per_second": round(baseline.pages_per_second, 4),
                "seconds_per_page": round(1.0 / baseline.pages_per_second, 4)
                if baseline.pages_per_second
                else None,
                **baseline.window.as_dict(),
            },
            best={
                "trial": best.n,
                # ONLY what differs from the derivation, so applying the
                # result is ``pools = best`` and ``{}`` means "auto already
                # wins". The capacities are derived FROM the widths, so a
                # width that changed does not have to pin its queue too.
                "stage_workers": {
                    keys[i]: widths[i] for i in range(len(widths)) if widths[i] != derived[i]
                },
                # Always empty, and deliberately: the search never CHOOSES a
                # capacity. Capacities are derived from the widths, so
                # applying ``stage_workers`` re-derives them, and pinning a
                # number the derivation would have produced anyway would
                # freeze it against a later change to the derivation.
                "queue_capacity": {},
                # Only the stages that ended up somewhere other than where the
                # spec put them, for the same reason the widths are: applying
                # the result is ``pools = best``.
                "stage_device": {
                    key: value
                    for key, value in placed.items()
                    if value != asked_devices.get(key, value)
                },
                "pages_per_second": round(best.pages_per_second, 4),
                "seconds_per_page": round(1.0 / best.pages_per_second, 4)
                if best.pages_per_second
                else None,
                "speedup": round(best.pages_per_second / baseline.pages_per_second, 4)
                if baseline.pages_per_second
                else 1.0,
                # The headline carries the window it was read over, so a
                # number the tuner was not allowed to decide on cannot be
                # shown as if it were one that it was.
                **best.window.as_dict(),
            },
            # What the recognizer ran at, and the mode it was benchmarked
            # for -- NOT part of ``best``: a precision is never a pool. For a
            # balanced/speed mode it is this machine's PICK, with every
            # candidate's trial (``precision_trials``) and why it won. Absent
            # where the engine fixes its own.
            **({"precision": self.precision} if self.precision else {}),
            **({"precision_mode": self.mode} if self.precision else {}),
            **(
                {
                    "precision_trials": [
                        {
                            "precision": self.trials[n - 1].precision,
                            "pages_per_second": round(self.trials[n - 1].pages_per_second, 4),
                            "chosen": self.trials[n - 1].precision == self.precision,
                        }
                        for n in self.precision_trials
                    ],
                    "precision_why": self.precision_why,
                }
                if self.precision_trials
                else {}
            ),
            peak_rss_mb=peak_rss_mb(),
            peak_vram_mb=peak_vram_mb(),
        )
        return 0


def peak_rss_mb() -> int | None:
    """This process and its children at their largest, in MiB, or None."""
    try:
        import resource

        selfmax = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        kids = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss
    except Exception:
        return None
    # ru_maxrss is KiB on Linux and bytes on macOS; this runner is Linux.
    return int((selfmax + kids) / 1024)


def peak_vram_mb() -> int | None:
    """Torch's peak allocation in THIS process, in MiB, or None.

    Detector subprocesses hold their own context and are NOT included -- a
    number that silently left out half the card would be worse than no
    number, so the log says so too. A road whose model lives in ANOTHER
    process entirely (a served engine, ADDENDUM 8: this process may not even
    import torch) therefore reports 0 or None, and 0 here means "nothing was
    allocated HERE", never "the card was idle" and never a failure. What the
    card was doing is the benchmark's GPU busy sample, not this.
    """
    try:
        import torch

        if not torch.cuda.is_available():
            return None
        peak = int(torch.cuda.max_memory_allocated() / (1024 * 1024))
    except Exception:
        return None
    log("[runner] peak VRAM is this process only; detector subprocesses are not counted")
    return peak


def bench(args: argparse.Namespace, *, stdout: Any = None) -> int:
    """Measure this engine on this host. ``stdout`` is the tests' seam."""
    protocol = Protocol(stdout if stdout is not None else seize_stdout(Path(args.session_log)))
    LOG.to_file(Path(args.session_log))
    if stdout is None:
        _die_cleanly()
    return BenchRun(args, protocol).run()

def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=(
            "Run one OCR engine over a volume, or hold one open as a session "
            "and stream volume after volume through it"
        )
    )
    parser.add_argument("--engine", required=True, choices=sorted(ENGINE_IDS))
    parser.add_argument("--detector", default="ppocr-manga", choices=sorted(DETECTOR_SCRIPTS))
    parser.add_argument(
        "--serve",
        action="store_true",
        help=(
            "hold ONE pipeline open and read volumes from stdin, one JSON "
            "object a line ({'op':'volume',...} / {'op':'close'}), writing "
            "events to stdout the same way. The models are loaded once for "
            "the whole session instead of once a volume, and the pipeline "
            "never drains at a volume boundary. Human-readable lines go to "
            "--session-log and to each volume's own log; stdout is protocol "
            "only. Requires --session-log; --input/--output/--cache-dir are "
            "then carried by each volume op instead"
        ),
    )
    parser.add_argument(
        "--bench",
        action="store_true",
        help=(
            "measure this engine on THIS host over --input, and tune its pool "
            "widths by following the pipeline's own bottleneck verdict. Loads "
            "once, warms up, then runs trials that rebuild only the pools. "
            "Writes nothing for the pages it reads. Protocol on stdout as "
            "--serve; requires --session-log"
        ),
    )
    parser.add_argument(
        "--session-log",
        dest="session_log",
        default=None,
        help="where a session's human-readable lines go (--serve and --bench)",
    )
    parser.add_argument("--input", default=None, help="directory of page images")
    parser.add_argument("--output", default=None, help="mokuro-format file to write")
    parser.add_argument(
        "--cache-dir", default=None, help="where per-page JSON goes (progress)"
    )
    parser.add_argument(
        "--detect-dir",
        default=None,
        help="where detector JSON goes (default: <output dir>/_detect)",
    )
    parser.add_argument(
        "--patches",
        type=int,
        default=DEFAULT_PATCH_BUDGET,
        choices=PATCH_BUDGETS,
        help=(
            "max_num_patches for the SigLIP2-NaFlex recognizer (hayai-nova): the "
            "resolution a line is read at -- 4, 5 or 6 patch rows across the "
            "glyph for a long column. Measured at the shipped batch of 16: "
            "256 = 786 MiB peak VRAM and 7.2 ms/crop, 384 = 841 MiB / 7.8 ms, "
            f"512 = 897 MiB / 9.1 ms (default: {DEFAULT_PATCH_BUDGET}); the whole "
            # argparse %-formats help strings, so a literal percent is doubled.
            "span is +8%% of a manga page and +9%% of a prose page. Lower it only "
            "to save time, never memory. Ignored by every other engine"
        ),
    )
    parser.add_argument(
        "--cpu-workers",
        dest="cpu_workers",
        type=int,
        default=None,
        help=(
            "pages allowed in the CPU stage (detection and the CTC read) at "
            "once, each on its own pair of onnxruntime sessions. Neither model "
            "scales with threads, so throughput comes from sessions. 1 still "
            "pipelines -- it runs the stage on its own thread, which is all a "
            "GPU-bound engine needs; 0 is the serial fallback, no threads at "
            f"all. Default: ${CPU_WORKERS_ENV}, else this host's share of its "
            f"cores ((cores - 1) / ${CPU_JOBS_ENV} concurrent jobs / "
            f"{SESSION_THREADS} threads a session, capped at {CPU_WORKERS_MAX})"
        ),
    )
    parser.add_argument(
        "--stage-workers",
        dest="stage_workers",
        default=None,
        help=(
            "width of each stage's pool, per stage: 'detect=4,post=2', or a "
            "bare number for all of them. Stages are detect, engine, layout, "
            "decode and post depending on the road (the startup line prints "
            "which). Beats the derivation, the host budget and the measured "
            "plateau; only a stage's structural limit (one model on one "
            "device) still holds. The per-volume summary says which stage was "
            "the bottleneck and which queue backed up, so this is the knob "
            f"that answers it. Default: ${STAGE_WORKERS_ENV}, else derived "
            "from what each stage costs a page"
        ),
    )
    parser.add_argument(
        "--queue-capacity",
        dest="queue_capacity",
        default=None,
        help=(
            "pages allowed to wait in the queue each stage fills, per stage: "
            "'detect=4,engine=2', or a bare number for all of them. A waiting "
            "page can hold a decoded image (~14 MB), so this is what a run "
            "costs in memory, and a full queue is what makes the stage before "
            "it wait -- which is the signal the summary reports. Default: "
            f"${QUEUE_CAPACITY_ENV}, else one slot per worker of the stage "
            "that fills it"
        ),
    )
    parser.add_argument(
        "--stage-device",
        dest="stage_device",
        default=None,
        help=(
            "which device each MODEL-BEARING stage runs on: "
            "'detect=cpu,engine=gpu:0' (torch's 'cuda:0' is accepted and means "
            f"the same card). The stages that take one are {', '.join(MODEL_STAGES)}; "
            "feed, post and layout are CPU work with no model to place. On the "
            "served road the one model is the serve process, so 'mokuro=cpu' "
            "starts it with --force_cpu and 'mokuro=gpu:1' hides the other "
            "cards from it. A stage on a card is one model on one device and "
            "derives to width 1, so moving the detector to the CPU is what "
            "makes it a pool and leaves the whole card to the engine. Default: "
            f"${STAGE_DEVICE_ENV}, else auto -- card 0 where there is one, else "
            "the CPU"
        ),
    )
    parser.add_argument(
        "--precision",
        default=DEFAULT_PRECISION_MODE,
        choices=(*PRECISION_MODES, PRECISION_AUTO),
        help=(
            "the row's precision MODE for the recognizer "
            f"({', '.join(sorted(PRECISION_ENGINES))}): the model's dtype for "
            "paddle-manga, the autocast dtype for hayai-nova, the serve "
            "process's --fp16 for mokuro. auto-accuracy takes the first format "
            "in the engine's list this device supports (PRECISION_POLICY); "
            "auto-balanced/auto-speed take --precision-pick when it is one of "
            "their supported candidates, else the first; fp32/bf16/fp16 force "
            "that format and refuse to start on a device that does not "
            "support it. The CPU supports fp32 only. --bench tries every "
            "supported candidate of a balanced/speed mode and picks the "
            "fastest (auto = auto-accuracy; default: auto-accuracy)"
        ),
    )
    parser.add_argument(
        "--precision-pick",
        dest="precision_pick",
        default=None,
        choices=PRECISIONS,
        help=(
            "the format this machine's benchmark picked for an auto-balanced/"
            "auto-speed row; ignored by any other mode, and by a device that "
            "no longer supports it"
        ),
    )
    parser.add_argument(
        "--precision-why",
        dest="precision_why",
        default="",
        help="why the pick was made (the benchmark's numbers), for the log line",
    )
    parser.add_argument(
        "--stats-file",
        dest="stats_file",
        default=None,
        help=(
            "where to publish the live pool/queue numbers as JSON, rewritten "
            f"every {PIPELINE_STATS_INTERVAL:.0f}s and once at the end "
            f"(default: ${PIPELINE_STATS_ENV}, else {PIPELINE_STATS_FILE} "
            "beside the detector dumps). Never put it under --cache-dir: the "
            "files there are counted as finished pages. Ignored by --serve, "
            "which publishes one file per volume beside that volume's dumps"
        ),
    )
    parser.add_argument(
        "--bench-max-trials",
        dest="bench_max_trials",
        type=int,
        default=BENCH_MAX_TRIALS,
        help=f"most trials a --bench run may take (default: {BENCH_MAX_TRIALS})",
    )
    parser.add_argument(
        "--bench-precision-only",
        dest="bench_precision_only",
        action="store_true",
        help=(
            "with --bench: only the precision trials of an auto-balanced/auto-speed "
            "mode, at the pools given by --stage-workers/--queue-capacity/--stage-device "
            "-- no width search and no placement"
        ),
    )
    parser.add_argument(
        "--bench-budget-seconds",
        dest="bench_budget_seconds",
        type=float,
        default=BENCH_BUDGET_SECONDS,
        help=f"wall-clock budget for a --bench run (default: {BENCH_BUDGET_SECONDS:.0f}s)",
    )
    parser.add_argument(
        "--mokuro-python",
        dest="mokuro_python",
        default=None,
        help=(
            "interpreter of the environment a SERVED engine lives in "
            f"({', '.join(sorted(SERVED_ENGINES))}). Such an engine is a "
            "process of its own -- this runner spawns it once a session, "
            "streams pages into it and reads page JSON back -- and its "
            "packages are not in this environment, so it cannot be this "
            "interpreter. Required by those engines and ignored by the rest"
        ),
    )
    parser.add_argument("--title", default=None)
    parser.add_argument("--volume", default=None)
    parser.add_argument("--title-uuid", default=None)
    parser.add_argument("--volume-uuid", default=None)
    parser.add_argument("--generator", default=None)
    return parser


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.serve and args.bench:
        parser.error("--serve and --bench are different modes; pick one")
    if args.engine in SERVED_ENGINES and not args.mokuro_python:
        parser.error(f"--mokuro-python is required by --engine {args.engine}")
    if args.serve or args.bench:
        if not args.session_log:
            parser.error("--serve and --bench write their logs to --session-log")
    if args.bench:
        if not args.input:
            parser.error("--bench needs --input: the sample pages to measure")
        if args.stage_workers or args.queue_capacity:
            # A benchmark that started from hand-set widths would be measuring
            # the hand, not the host: it explores FROM the derived widths.
            parser.error("--bench derives its own widths; --stage-workers/--queue-capacity")
    if not args.serve and not args.bench:
        missing = [
            flag
            for flag, value in (
                ("--input", args.input),
                ("--output", args.output),
                ("--cache-dir", args.cache_dir),
            )
            if not value
        ]
        if missing:
            parser.error(f"the following arguments are required: {', '.join(missing)}")
    return args


def apply_rocm_override() -> None:
    """An AMD card this ROCm build of torch has no kernels for runs as its
    family's built target (``rocm_gfx``) -- set before any device call, which
    is also before any engine or served mokuro process is spawned, so they
    inherit it. Nothing without torch, on CUDA, or when the user set it."""
    try:
        import torch
    except Exception:
        return
    try:
        value = load_sibling("rocm_gfx").apply_for_torch(torch)
    except Exception as e:  # never why a runner fails to start
        log(f"[runner] ROCm target check skipped: {e}")
        return
    if value:
        log(f"[runner] ROCm: this card is not in the torch build; HSA_OVERRIDE_GFX_VERSION={value}")


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    apply_rocm_override()
    if args.serve:
        return serve(args)
    if args.bench:
        return bench(args)
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
