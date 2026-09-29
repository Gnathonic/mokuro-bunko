"""A stub of the OCR-generations HTTP contract, for driving the real pages.

The admin and queue pages are plain files with no build step, so the honest
way to look at them is to serve THEM — not a copy — against a server that
answers exactly the shapes `GET/PUT /api/ocr/generations` and
`GET /queue/api/status` are specified to answer. Everything here is a
fixture: no config, no database, no OCR.

Used by `test_ocr_generations.py` and by the screenshot driver.
"""

from __future__ import annotations

import hashlib
import json
import re
import threading
from copy import deepcopy
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, unquote

from mokuro_bunko.queue.shape import shape_status

WEB_ROOT = Path(__file__).resolve().parents[2] / "src" / "mokuro_bunko"

# ---------------------------------------------------------------------------
# Precision modes: one per row, applying to every machine. The server resolves
# each mode on each machine's card and sends the answer (`precision_on`); the
# page never decides a precision of its own. This table stands in for the
# runner's policy with just enough of it for the page to be driven.
# ---------------------------------------------------------------------------

ALL_MODES = ["auto-accuracy", "auto-balanced", "auto-speed", "fp32", "bf16", "fp16"]
MOKURO_MODES = [mode for mode in ALL_MODES if mode != "bf16"]
DEFAULT_MODE = "auto-accuracy"
PRECISION_MODE_LABELS = {
    "auto-accuracy": "Auto: accuracy",
    "auto-balanced": "Auto: balanced",
    "auto-speed": "Auto: speed",
    "fp32": "fp32 only",
    "bf16": "bf16 only (cards that support it)",
    "fp16": "fp16 only (GPUs)",
}

# What each machine's card runs: this server and tower have cards that run
# bf16 and fp16; box is a CPU-only mini PC, fp32 alone.
MACHINE_FORMATS: dict[str, set[str]] = {
    "local": {"bf16", "fp16", "fp32"},
    "tower": {"bf16", "fp16", "fp32"},
    "box": {"fp32"},
}


def resolve_mode(engine: str, mode: str, formats: set[str]) -> dict[str, Any]:
    """One machine's answer for one mode, in the shape `precision_on` carries."""
    if mode == "auto-accuracy":
        precision = "bf16" if engine == "hayai-nova" and "bf16" in formats else "fp32"
        return {"precision": precision, "eligible": True, "why": "auto-accuracy"}
    if mode in ("auto-balanced", "auto-speed"):
        order = ["bf16", "fp16", "fp32"] if mode == "auto-speed" else ["bf16", "fp32"]
        if engine == "mokuro":
            order = [p for p in order if p != "bf16"] or ["fp32"]
            if mode == "auto-balanced":
                order = ["fp16", "fp32"]
        first = next(p for p in order if p in formats)
        entry = {"precision": first, "eligible": True,
                 "why": "not benchmarked yet: first supported candidate"}
        if len([p for p in order if p in formats]) > 1:
            # Two candidates to choose between and no pick yet: benchmarked
            # before this machine's next volume.
            entry["bench"] = "pending"
        return entry
    if mode in formats:
        return {"precision": mode, "eligible": True, "why": mode + " only"}
    return {"precision": None, "eligible": False, "why": mode + " not supported"}


def precision_on(engine: str) -> dict[str, dict[str, dict[str, Any]]]:
    """What EVERY mode resolves to on every machine, for a row on `engine`."""
    modes = MOKURO_MODES if engine == "mokuro" else ALL_MODES
    return {
        machine: {mode: resolve_mode(engine, mode, formats) for mode in modes}
        for machine, formats in MACHINE_FORMATS.items()
    }


# The labels are the REAL ones the server ships, and they are long. A fixture
# with short ids for labels hid the fact that a closed select shows
# "PaddleOCR-VL" and nothing else; these cannot hide it again.
CATALOG: dict[str, Any] = {
    "engines": [
        # Addendum 8: the mokuro engines are SERVED -- a process of their own,
        # in an environment of their own, that finds its own text. NOT
        # monolithic: they go through the runner like every other row.
        # `precision_modes`: the modes the row's one precision select offers
        # for that engine (mokuro never runs bf16; ppocr-manga fixes its own).
        {"id": "mokuro", "precision": True, "precision_modes": MOKURO_MODES, "devices": "any", "label": "mokuro (manga-ocr)", "monolithic": False, "served": True, "own_environment": True, "own_detector": None, "patch_budget": False},
        {"id": "hayai-nova", "precision": True, "precision_modes": ALL_MODES, "devices": "any", "label": "hayai-ocr v2.5 Nova", "monolithic": False, "served": False, "own_environment": False, "own_detector": None, "patch_budget": True},
        {"id": "paddle-manga", "precision": True, "precision_modes": ALL_MODES, "devices": "any", "label": "PaddleOCR-VL 1.6 manga LoRA", "monolithic": False, "served": False, "own_environment": False, "own_detector": None, "patch_budget": False},
        {"id": "ppocr-manga", "precision": False, "precision_modes": [], "label": "PP-OCRv6 manga (CTC, CPU)", "monolithic": False, "served": False, "own_environment": False, "own_detector": "ppocr-manga", "patch_budget": False, "devices": ["cpu"]},
    ],
    "detectors": [
        {"id": "ppocr-manga", "label": "PP-OCRv6 manga line detector (Kellenok)", "devices": ["cpu"]},
        {"id": "ctd", "label": "comic-text-detector (via mokuro)", "devices": "any"},
    ],
    # Addendum 7: one host with one card, probed once in the engines env.
    "devices": [
        {"id": "auto", "label": "Auto — GPU 0 when available"},
        {"id": "cpu", "label": "AMD Ryzen 9 7950X (16 cores)"},
        {"id": "gpu:0", "label": "GPU 0 — AMD Radeon RX 9070 XT (16 GB)"},
    ],
    "patch_budgets": [256, 384, 512],
    # One precision MODE per row, for every machine (`ocr/precision.py`).
    "precision_modes": [{"id": mode, "label": PRECISION_MODE_LABELS[mode]} for mode in ALL_MODES],
    "precision_default": DEFAULT_MODE,
    "name_pattern": "^[a-z0-9][a-z0-9-]{0,31}$",
    "reserved_names": ["original", "gcv"],
    "reserved_prefixes": ["tr-"],
}

# Five rows, one per case the UI has to survive: a SERVED primary whose middle
# stage is a process with the fork's own worker pool, two rows on one engine
# with different detectors, a healthy GPU-bound row whose numbers support no
# verdict, and a disabled row whose engine brings its own detector and never
# touches a GPU.
GENERATIONS: list[dict[str, Any]] = [
    {
        "id": "g-1",
        "name": "mokuro",
        "primary": True,
        "enabled": True,
        "engine": "mokuro",
        "detector": None,
        "patch_budget": None,
        "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
        "sidecar": "<Volume>.mokuro",
        "effective_detector": None,
        "detector_locked": True,
        "patch_budget_applies": False,
        "precision_applies": False,
        "road": "served",
        # The served road (Addendum 8): a CPU pool spooling the page, the
        # serve process itself, a CPU pool assembling the sidecar. The middle
        # stage is where Addendum 7's Device select lives, and its Workers cell
        # is the fork's own --num_workers rather than a pool of ours -- which
        # is what `workers_means: "engine"` says.
        "stages": [
            {"key": "feed", "name": "spool the page", "device": "cpu",
             "max_workers": None, "derived_workers": 2, "derived_capacity": 4,
             "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool"},
            {"key": "mokuro", "name": "serve process", "device": "gpu:0",
             "max_workers": 1, "derived_workers": 1, "derived_capacity": 4,
             "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None,
             "workers_means": "engine"},
            {"key": "post", "name": "assemble", "device": "cpu",
             "max_workers": None, "derived_workers": 2, "derived_capacity": 4,
             "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool"},
        ],
        "volumes_done": 34,
        "volumes_total": 34,
        # The primary row is never skipped: a flagged volume already HAS the
        # sidecar the flag was computed from.
        "volumes_skipped": 0,
        "congestion": None,
    },
    {
        "id": "g-2",
        "name": "hayai-nova-ctd",
        "primary": False,
        "enabled": True,
        "engine": "hayai-nova",
        "detector": "ctd",
        "patch_budget": 512,
        "pools": {"stage_workers": {"detect": 2}, "queue_capacity": {}, "stage_device": {}},
        "sidecar": "<Volume>.hayai-nova-ctd.mokuro",
        "effective_detector": "ctd",
        "detector_locked": False,
        "patch_budget_applies": True,
        "precision_applies": True,
        "road": "adapter",
        "stages": [
            {"key": "detect", "name": "read page + detection", "device": "cpu", "max_workers": None, "derived_workers": 1, "derived_capacity": 2, "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None, "workers_means": "pool"},
            {"key": "engine", "name": "engine read", "device": "gpu:0", "max_workers": 1, "derived_workers": 1, "derived_capacity": 1, "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None, "workers_means": "copies"},
            {"key": "post", "name": "assemble + place", "device": "cpu", "max_workers": None, "derived_workers": 2, "derived_capacity": 4, "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool"},
        ],
        "volumes_done": 31,
        "volumes_total": 34,
        # One volume in this library arrived short of its pages, so this row
        # steps over it and can never reach 34.
        "volumes_skipped": 1,
        "congestion": {
            "runs": 3,
            "last_run_at": "2026-09-21T08:15:00Z",
            "verdict": "engine starved 38% waiting on detect — widen detect",
            "bottleneck": "detect",
            "stages": [
                {"key": "detect", "workers": 2, "busy_pct": 97, "starved_pct": 0, "blocked_pct": 2},
                {"key": "engine", "workers": 1, "busy_pct": 59, "starved_pct": 38, "blocked_pct": 3},
                {"key": "post", "workers": 2, "busy_pct": 21, "starved_pct": 76, "blocked_pct": 0},
            ],
            "queues": [
                {"name": "in->detect", "capacity": 2, "mean_depth": 1.9, "max_depth": 2},
                {"name": "detect->engine", "capacity": 1, "mean_depth": 0.1, "max_depth": 1},
                {"name": "engine->post", "capacity": 4, "mean_depth": 0.2, "max_depth": 2},
            ],
        },
    },
    {
        "id": "g-3",
        "name": "hayai-nova-ppocr-manga",
        "primary": False,
        "enabled": True,
        "engine": "hayai-nova",
        "detector": "ppocr-manga",
        "patch_budget": 256,
        "pools": {"stage_workers": {"detect": 4, "post": 2}, "queue_capacity": {"detect": 4}, "stage_device": {}},
        "sidecar": "<Volume>.hayai-nova-ppocr-manga.mokuro",
        "effective_detector": "ppocr-manga",
        "detector_locked": False,
        "patch_budget_applies": True,
        "precision_applies": True,
        "road": "adapter",
        "stages": [
            {"key": "detect", "name": "read page + detection", "device": "cpu", "max_workers": 8, "derived_workers": 2, "derived_capacity": 4, "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None, "workers_means": "pool"},
            {"key": "engine", "name": "engine read", "device": "gpu:0", "max_workers": 1, "derived_workers": 1, "derived_capacity": 1, "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None, "workers_means": "copies"},
            {"key": "post", "name": "assemble + place", "device": "cpu", "max_workers": None, "derived_workers": 2, "derived_capacity": 4, "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool"},
        ],
        "volumes_done": 12,
        "volumes_total": 34,
        # No `volumes_skipped` at all: a server that predates the field.
        "congestion": {
            "runs": 5,
            "last_run_at": "2026-09-20T22:40:00Z",
            "verdict": "detect ×4 at 96% — widen it",
            "bottleneck": "detect",
            "stages": [
                {"key": "detect", "workers": 4, "busy_pct": 96, "starved_pct": 1, "blocked_pct": 4},
                {"key": "engine", "workers": 1, "busy_pct": 71, "starved_pct": 26, "blocked_pct": 3},
                {"key": "post", "workers": 2, "busy_pct": 33, "starved_pct": 61, "blocked_pct": 0},
            ],
            "queues": [
                {"name": "detect->engine", "capacity": 4, "mean_depth": 0.4, "max_depth": 3},
                {"name": "engine->post", "capacity": 4, "mean_depth": 0.1, "max_depth": 1},
            ],
        },
    },
    {
        "id": "g-4",
        "name": "paddle-manga",
        "primary": False,
        "enabled": True,
        "engine": "paddle-manga",
        "detector": "ppocr-manga",
        "patch_budget": None,
        "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
        "sidecar": "<Volume>.paddle-manga.mokuro",
        "effective_detector": "ppocr-manga",
        "detector_locked": False,
        "patch_budget_applies": False,
        "precision_applies": True,
        "road": "adapter",
        "stages": [
            {"key": "detect", "name": "read page + detection", "device": "cpu", "max_workers": None, "derived_workers": 2, "derived_capacity": 4, "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None, "workers_means": "pool"},
            {"key": "engine", "name": "engine read", "device": "gpu:0", "max_workers": 1, "derived_workers": 1, "derived_capacity": 2, "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None, "workers_means": "copies"},
            {"key": "post", "name": "assemble + place", "device": "cpu", "max_workers": None, "derived_workers": 2, "derived_capacity": 4, "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool"},
        ],
        "volumes_done": 34,
        "volumes_total": 34,
        "volumes_skipped": 0,
        "congestion": {
            "runs": 4,
            "last_run_at": "2026-09-21T06:02:00Z",
            "verdict": None,
            "bottleneck": "engine",
            "stages": [
                {"key": "detect", "workers": 2, "busy_pct": 54, "starved_pct": 9, "blocked_pct": 37},
                {"key": "engine", "workers": 1, "busy_pct": 88, "starved_pct": 6, "blocked_pct": 6},
                {"key": "post", "workers": 2, "busy_pct": 49, "starved_pct": 50, "blocked_pct": 1},
            ],
            "queues": [
                {"name": "detect->engine", "capacity": 2, "mean_depth": 1.6, "max_depth": 2},
                {"name": "engine->post", "capacity": 4, "mean_depth": 0.3, "max_depth": 2},
            ],
        },
    },
    {
        "id": "g-5",
        "name": "ppocr-manga",
        "primary": False,
        "enabled": False,
        "engine": "ppocr-manga",
        "detector": None,
        "patch_budget": None,
        "pools": {"stage_workers": {}, "queue_capacity": {"layout": 8}, "stage_device": {}},
        "sidecar": "<Volume>.ppocr-manga.mokuro",
        "effective_detector": "ppocr-manga",
        "detector_locked": True,
        "patch_budget_applies": False,
        "precision_applies": False,
        "road": "line",
        "stages": [
            {"key": "detect", "name": "line detection", "device": "cpu", "max_workers": None, "derived_workers": 4, "derived_capacity": 8, "devices_allowed": ["auto", "cpu"], "device_locked_reason": "the PP-OCRv6 detector runs on the CPU (onnxruntime)", "workers_means": "pool"},
            {"key": "layout", "name": "line read + layout", "device": "cpu", "max_workers": None, "derived_workers": 4, "derived_capacity": 8, "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool"},
        ],
        "volumes_done": 32,
        "volumes_total": 34,
        "volumes_skipped": 2,
        "congestion": {
            "runs": 2,
            "last_run_at": "2026-09-19T11:30:00Z",
            "verdict": None,
            "bottleneck": "layout",
            "stages": [
                {"key": "detect", "workers": 4, "busy_pct": 41, "starved_pct": 4, "blocked_pct": 55},
                {"key": "layout", "workers": 4, "busy_pct": 58, "starved_pct": 41, "blocked_pct": 1},
            ],
            "queues": [
                {"name": "detect->layout", "capacity": 8, "mean_depth": 6.8, "max_depth": 8},
            ],
        },
    },
]

# ---------------------------------------------------------------------------
# Benchmark & tune fixtures (ADDENDA 5 & 6).
#
# A benchmark now measures a SPEC (the row as edited, saved or not) under a
# KEY -- the generation id, or a client-minted draft id for an unsaved row --
# and benchmarks QUEUE: several keys can be posted at once and run one at a
# time, FIFO, while the OCR queue is held and pre-empted once for the whole
# line. The contract's bench object, in each shape the UI has to survive: a
# tunable result whose trials tell the tuning story, a monolithic one that is
# all the documented nulls at once, one where auto already wins, a failure, a
# cancellation, and the pausing -> running -> done sequence a poll walks
# through for whichever key is at the head of the line.
# ---------------------------------------------------------------------------

def _ago(**delta: float) -> str:
    """An ISO stamp that far in the past, RELATIVE TO NOW.

    The page prints these as "measured 2 days ago", so a fixture with a date
    written into it says something different every day and the tests that
    read it rot quietly. These do not.
    """
    return (datetime.now(timezone.utc) - timedelta(**delta)).strftime("%Y-%m-%dT%H:%M:%SZ")


HOST_FULL = {
    "cpu": "AMD Ryzen 9 7950X (16 cores)",
    "gpu": "AMD Radeon RX 9070 XT",
    "backend": "rocm",
}

# A host that cannot name a GPU: the documented null, not an empty string.
HOST_CPU_ONLY = {"cpu": "Intel Core i5-8250U (4 cores)", "gpu": None, "backend": "cpu"}


def _trial(n, note, workers, capacity, seconds, pps, accepted, verdict, bottleneck, stages,
           window=21.0, passes=2, gpu=None, cpu=None):
    """One trial, with the ADDENDUM 9 window its rate was read over.

    `seconds` is the trial's wall time (fill, passes and the gaps between
    them); `window_seconds` is the span of the page EMISSIONS inside it, and
    it is the only one anything is computed from.
    """
    return {
        "n": n, "note": note, "stage_workers": workers, "queue_capacity": capacity,
        "seconds": seconds, "pages_per_second": pps, "accepted": accepted,
        "window_seconds": window, "pages_measured": int(round(pps * window)) + 1,
        "passes": passes, "short_window": window < 10,
        "gpu_busy_pct": gpu, "cpu_busy_pct": cpu,
        "verdict": verdict, "bottleneck": bottleneck, "stages": stages,
        "queues": [{"name": "detect->engine", "capacity": capacity.get("detect", 1),
                    "mean_depth": 0.1, "max_depth": 1}],
    }


def _stages(detect_busy, engine_starved, detect_workers):
    return [
        {"key": "detect", "workers": detect_workers, "busy_pct": detect_busy, "starved_pct": 0, "blocked_pct": 2},
        {"key": "engine", "workers": 1, "busy_pct": 100 - engine_starved - 3, "starved_pct": engine_starved, "blocked_pct": 3},
        {"key": "post", "workers": 1, "busy_pct": 21, "starved_pct": 76, "blocked_pct": 0},
    ]


# The spec a benchmark measured: the same shape the PUT body's row carries,
# minus id/name/primary/enabled. Baked into the DONE fixtures below so a test
# that never goes through a real POST still gets a spec that matches the row
# it names -- and one fixture (BENCH_STALE_SPEC) deliberately does not, to
# drive the "measured with different settings" notice.
SPEC_G1 = {"engine": "mokuro", "detector": None, "patch_budget": None, "precision": DEFAULT_MODE,
           "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}}
SPEC_G2 = {"engine": "hayai-nova", "detector": "ctd", "patch_budget": 512, "precision": DEFAULT_MODE,
           "pools": {"stage_workers": {"detect": 2}, "queue_capacity": {}, "stage_device": {}}}
SPEC_G3 = {"engine": "hayai-nova", "detector": "ppocr-manga", "patch_budget": 256, "precision": DEFAULT_MODE,
           "pools": {"stage_workers": {"detect": 4, "post": 2}, "queue_capacity": {"detect": 4}, "stage_device": {}}}
SPEC_G4 = {"engine": "paddle-manga", "detector": "ppocr-manga", "patch_budget": None, "precision": DEFAULT_MODE,
           "pools": {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}}

# Every bench object carries `queue` (the FIFO around it) and `preempted`
# (what it interrupted). A fixture embedded directly in the generations list
# (never having gone through the live queue machinery) is settled and alone:
# nothing running, nothing interrupted.
_IDLE_QUEUE = {"running": None, "queued": []}

BENCH_TUNED: dict[str, Any] = {
    "state": "done",
    "generation": "g-2",
    "key": "g-2",
    "spec": SPEC_G2,
    "queue": dict(_IDLE_QUEUE),
    "preempted": [],
    "started_at": _ago(days=2, minutes=7),
    "finished_at": _ago(days=2),
    "waiting_for_queue": False,
    "sample": {"pages": 32, "volumes": 6},
    "host": HOST_FULL,
    "tunable": True,
    "progress": None,
    "startup_seconds": 11.2,
    "trials": [
        _trial(1, "auto", {"detect": 1, "engine": 1, "post": 1}, {"detect": 1, "engine": 1, "post": 1},
               21.3, 1.50, True, "engine starved 38% waiting on detect — widen detect", "detect",
               _stages(97, 38, 1)),
        _trial(2, "detect ×2", {"detect": 2, "engine": 1, "post": 1}, {"detect": 1, "engine": 1, "post": 1},
               16.6, 1.92, True, "engine starved 19% waiting on detect — widen detect", "detect",
               _stages(94, 19, 2)),
        _trial(3, "detect ×3", {"detect": 3, "engine": 1, "post": 1}, {"detect": 1, "engine": 1, "post": 1},
               15.2, 2.10, True, None, "engine", _stages(71, 4, 3), gpu=88.0, cpu=41.0),
        _trial(4, "detect ×4", {"detect": 4, "engine": 1, "post": 1}, {"detect": 1, "engine": 1, "post": 1},
               15.1, 2.11, False, "0.5% for a whole core — not worth it", "engine", _stages(55, 3, 4)),
    ],
    "baseline": {"pages_per_second": 1.50, "seconds_per_page": 0.667},
    "best": {"trial": 3, "stage_workers": {"detect": 3}, "queue_capacity": {},
             "pages_per_second": 2.10, "seconds_per_page": 0.476, "speedup": 1.40,
             "same_as_spec": False, "window_seconds": 21.0, "pages_measured": 45,
             "passes": 2, "short_window": False, "gpu_busy_pct": 88.0, "cpu_busy_pct": 41.0},
    "peak_rss_mb": 2410,
    "peak_vram_mb": 3120,
    # 200 / 2.10 -- the model load is NOT in it (ADDENDUM 9).
    "estimates": {"volume_200_pages_seconds": 95, "remaining_pages": 5230, "remaining_seconds": 7810},
    "error": None,
}

# The same run as the list carries it: the trials are the one thing stripped.
BENCH_TUNED_SUMMARY = {k: v for k, v in BENCH_TUNED.items() if k != "trials"}

# The same finished run, but the row has since been edited: the spec it
# measured is no longer the spec on screen. Drives "measured with different
# settings" without touching the row itself.
BENCH_STALE_SPEC: dict[str, Any] = dict(
    BENCH_TUNED,
    spec=dict(SPEC_G2, detector="ppocr-manga", patch_budget=256),
)

# A monolithic engine on a machine that can say nothing about its GPU, its
# VRAM, its RSS, or how much of the library is left: every documented null in
# one object.
BENCH_MONOLITHIC: dict[str, Any] = {
    "state": "done",
    "generation": "g-1",
    "key": "g-1",
    "spec": SPEC_G1,
    "queue": dict(_IDLE_QUEUE),
    "preempted": [],
    "started_at": _ago(hours=13, minutes=2),
    "finished_at": _ago(hours=13),
    "waiting_for_queue": False,
    "sample": {"pages": 32, "volumes": 4},
    "host": HOST_CPU_ONLY,
    "tunable": False,
    "progress": None,
    "startup_seconds": 6.4,
    "trials": [
        {"n": 1, "note": "single run", "stage_workers": {}, "queue_capacity": {},
         "seconds": 118.0, "pages_per_second": 0.27, "window_seconds": 85.2,
         "pages_measured": 24, "passes": 1, "short_window": False,
         "gpu_busy_pct": None, "cpu_busy_pct": 63.0, "accepted": True,
         "verdict": None, "bottleneck": None, "stages": [], "queues": []},
    ],
    "baseline": {"pages_per_second": 0.27, "seconds_per_page": 3.69},
    "best": {"trial": 1, "stage_workers": {}, "queue_capacity": {},
             "pages_per_second": 0.27, "seconds_per_page": 3.69, "speedup": 1,
             "same_as_spec": True, "window_seconds": 85.2, "pages_measured": 24,
             "passes": 1, "short_window": False,
             "gpu_busy_pct": None, "cpu_busy_pct": 63.0},
    "peak_rss_mb": None,
    "peak_vram_mb": None,
    # 200 / 0.27, with no startup term: this engine pays its load per volume
    # and that is said as its own fact, not folded in here.
    "estimates": {"volume_200_pages_seconds": 741, "remaining_pages": None, "remaining_seconds": None},
    "error": None,
}

BENCH_MONOLITHIC_SUMMARY = {k: v for k, v in BENCH_MONOLITHIC.items() if k != "trials"}

# Tuned, and the widths it found are the ones the row already has.
BENCH_APPLIED: dict[str, Any] = dict(
    BENCH_TUNED,
    generation="g-3",
    key="g-3",
    spec=SPEC_G3,
    best=dict(BENCH_TUNED["best"], stage_workers={"detect": 4, "post": 2},
              queue_capacity={"detect": 4}, same_as_spec=True),
)

# Nothing to widen: auto was already the fastest this machine manages.
BENCH_AUTO_BEST: dict[str, Any] = dict(
    BENCH_TUNED,
    generation="g-4",
    key="g-4",
    spec=SPEC_G4,
    trials=[
        _trial(1, "auto", {"detect": 2, "engine": 1, "post": 2}, {"detect": 4, "engine": 1, "post": 4},
               18.0, 1.78, True, None, "engine", _stages(54, 6, 2)),
        _trial(2, "detect ×3", {"detect": 3, "engine": 1, "post": 2}, {"detect": 4, "engine": 1, "post": 4},
               18.2, 1.76, False, "no gain — the engine was already the limit", "engine", _stages(44, 5, 3)),
    ],
    best={"trial": 1, "stage_workers": {}, "queue_capacity": {},
          "pages_per_second": 1.78, "seconds_per_page": 0.562, "speedup": 1.0,
          "same_as_spec": True, "window_seconds": 20.2, "pages_measured": 37,
          "passes": 2, "short_window": False, "gpu_busy_pct": 94.0, "cpu_busy_pct": 22.0},
    baseline={"pages_per_second": 1.78, "seconds_per_page": 0.562},
    estimates={"volume_200_pages_seconds": 112, "remaining_pages": 5230, "remaining_seconds": 2938},
    peak_rss_mb=1890,
    peak_vram_mb=2048,
)

# ADDENDUM 9: the sample was too small for its pages to come out over a
# window worth comparing, so nothing was tuned and the page has to say so
# instead of printing the rate as if it were a finding.
BENCH_SHORT_WINDOW: dict[str, Any] = dict(
    BENCH_TUNED,
    trials=[
        _trial(1, "auto", {"detect": 1, "engine": 1, "post": 1},
               {"detect": 1, "engine": 1, "post": 1},
               9.4, 3.2, True, None, "engine", _stages(71, 4, 1),
               window=1.9, passes=8, gpu=31.0, cpu=18.0),
        _trial(2, "detect ×2", {"detect": 2, "engine": 1, "post": 1},
               {"detect": 1, "engine": 1, "post": 1},
               9.1, 41.0, False, None, "engine", _stages(44, 5, 2),
               window=0.4, passes=8, gpu=29.0, cpu=19.0),
    ],
    baseline={"pages_per_second": 3.2, "seconds_per_page": 0.31,
              "window_seconds": 1.9, "pages_measured": 7, "passes": 8,
              "short_window": True},
    best={"trial": 1, "stage_workers": {}, "queue_capacity": {},
          "pages_per_second": 3.2, "seconds_per_page": 0.31, "speedup": 1.0,
          "same_as_spec": True, "window_seconds": 1.9, "pages_measured": 7,
          "passes": 8, "short_window": True, "gpu_busy_pct": 31.0,
          "cpu_busy_pct": 18.0},
    estimates={"volume_200_pages_seconds": 63, "remaining_pages": 5230,
               "remaining_seconds": 1634},
)

BENCH_FAILED: dict[str, Any] = {
    "state": "failed",
    "generation": "g-2",
    "key": "g-2",
    "spec": SPEC_G2,
    "queue": dict(_IDLE_QUEUE),
    "preempted": [],
    "started_at": _ago(minutes=4),
    "finished_at": _ago(minutes=3),
    "waiting_for_queue": False,
    "sample": {"pages": 32, "volumes": 6},
    "host": HOST_FULL,
    "tunable": True,
    "progress": None,
    "startup_seconds": None,
    "trials": [],
    "baseline": None,
    "best": None,
    "peak_rss_mb": None,
    "peak_vram_mb": None,
    "estimates": None,
    "error": "engine runner exited with code 1: HIP out of memory",
}

BENCH_CANCELLED: dict[str, Any] = dict(BENCH_FAILED, state="cancelled", error=None)

# What a benchmark that starts the queue interrupts: two volumes mid-OCR, the
# same pair `QUEUE_STATUS_NEW.current_jobs` carries.
PREEMPTED_SAMPLE: list[dict[str, str]] = [
    {"generation": "hayai-nova-ctd", "volume": "Volume 07"},
    {"generation": "ppocr-manga", "volume": "Volume 12"},
]


def bench_idle(gid: str) -> dict[str, Any]:
    return {"state": "idle", "generation": gid}


def bench_queued(gid: str) -> dict[str, Any]:
    """Waiting behind the running one: nothing measured yet, `position` is
    filled in by the caller (it depends on where in the line `gid` sits)."""
    return {
        "state": "queued", "generation": gid,
        "started_at": None, "finished_at": None,
        "waiting_for_queue": False,
        "sample": {"pages": 32, "volumes": 6},
        "host": HOST_FULL, "tunable": True, "progress": None,
        "startup_seconds": None, "trials": [], "baseline": None, "best": None,
        "peak_rss_mb": None, "peak_vram_mb": None, "estimates": None, "error": None,
    }


def bench_pausing(gid: str) -> dict[str, Any]:
    """The instant a key reaches the head of the line: already `state:
    "running"` and `position: 0` (ADDENDUM 6), but the seconds it takes to
    end whatever OCR jobs were running have not elapsed yet."""
    return dict(bench_queued(gid), state="running", waiting_for_queue=True,
                started_at=_ago(seconds=1))


def bench_running(gid: str, trial: int, workers: dict[str, int], done: int, pps: float) -> dict[str, Any]:
    return dict(
        bench_queued(gid),
        state="running",
        started_at=_ago(minutes=2),
        progress={"trial": trial, "max_trials": 8, "pages_done": done, "pages": 32,
                  "stage_workers": workers, "pages_per_second": pps},
        startup_seconds=11.2,
    )


# The sequence a poll walks for whichever key is at the head of the line: it
# pauses the OCR queue, the model loads and the trials run, then it lands on
# the tuned result. Installed for any key that becomes head without a script
# of its own (`StubState.bench_on_post`).
BENCH_RUN_SEQUENCE: list[dict[str, Any]] = [
    bench_pausing("g-2"),
    bench_running("g-2", 1, {"detect": 1, "engine": 1, "post": 1}, 12, 1.48),
    bench_running("g-2", 3, {"detect": 3, "engine": 1, "post": 1}, 25, 2.06),
    BENCH_TUNED,
]

GENERATIONS[0]["bench"] = BENCH_MONOLITHIC_SUMMARY
GENERATIONS[1]["bench"] = BENCH_TUNED_SUMMARY
GENERATIONS[2]["bench"] = None
GENERATIONS[3]["bench"] = None
GENERATIONS[4]["bench"] = None

# The row's precision mode, and what every mode resolves to on each machine
# -- for the rows whose engine takes one (ppocr-manga fixes its own).
for _row in GENERATIONS:
    if _row["engine"] in ("mokuro", "hayai-nova", "paddle-manga"):
        _row["precision"] = DEFAULT_MODE
        _row["precision_on"] = precision_on(_row["engine"])
        _row["precision_hold"] = None
# Each machine's LIFETIME count of each row: this server's own (`local_runs`,
# null while it has read none) and each processor's (`processor_runs`). The
# pages a minute beside them on a card come from the Processors card's speed.
GENERATIONS[0]["local_runs"] = {"volumes": 9, "pages": 1710, "seconds": 4795.0,
                                "pages_per_second": 0.36}
GENERATIONS[0]["processor_runs"] = {
    "tower": {"volumes": 20, "pages": 3800, "seconds": 149.0, "pages_per_second": 25.5},
}
GENERATIONS[1]["local_runs"] = None
GENERATIONS[1]["processor_runs"] = {
    "tower": {"volumes": 20, "pages": 3780, "seconds": 515.0, "pages_per_second": 7.3},
}
# What the History line counts: each machine's sidecars of the row ON DISK
# now, from the provenance table (`volumes_by_machine`). Exact, so lower than
# the lifetime counts above (re-runs) and summing to at most `volumes_done`.
for _row in GENERATIONS:
    _row["volumes_by_machine"] = {}
GENERATIONS[0]["volumes_by_machine"] = {"local": 7, "tower": 18}
GENERATIONS[1]["volumes_by_machine"] = {"tower": 17}

# This server has benchmarked paddle-manga's balanced mode: fp32 won.
GENERATIONS[3]["precision_on"]["local"]["auto-balanced"] = {
    "precision": "fp32", "eligible": True,
    "why": "benchmark: fp32 0.57 p/s beat bf16 0.32 p/s",
    "trials": [{"precision": "bf16", "pages_per_second": 0.32},
               {"precision": "fp32", "pages_per_second": 0.57}],
    "bench": "done",
}

SETTINGS: dict[str, Any] = {
    "registration": {"mode": "self", "default_role": "registered"},
    "cors": {"enabled": True, "allowed_origins": []},
    "catalog": {"enabled": True, "use_as_homepage": False, "reader_url": "https://reader.mokuro.app"},
    "queue": {"show_in_nav": True, "public_access": True, "display": "normal"},
    "ocr": {"backend": "auto", "poll_interval": 30},
    "ocr_runtime": {
        "configured_backend": "auto",
        "installed": True,
        "installed_backend": "rocm",
        "env_path": "/srv/mokuro/.venvs/mokuro",
        "engines_env_path": "/srv/mokuro/.venvs/engines",
        "supported_backends": ["cpu", "rocm"],
        "active_generations": ["mokuro", "hayai-nova-ctd", "hayai-nova-ppocr-manga", "paddle-manga"],
        "cli_hint": "Use `mokuro-bunko serve --ocr <auto|cuda|rocm|cpu|skip>` to change backend.",
        "driver_hint": "",
    },
}

# The Environment block's states, each one a real answer a server gives. They
# are fixtures rather than a matrix in the test so the screenshot driver can
# put any of them on screen.
#
# `available: false` is what the server sends when it cannot look: it knows
# NOTHING about installation, which is not the same as "not installed".
RUNTIME_UNAVAILABLE: dict[str, Any] = {"available": False}

RUNTIME_SKIP: dict[str, Any] = {"available": False, "configured_backend": "skip"}

RUNTIME_NOT_INSTALLED: dict[str, Any] = {
    "available": True,
    "configured_backend": "auto",
    "installed": False,
    "installed_backend": None,
    "env_path": "/srv/mokuro/.venvs/mokuro",
    "engines_env_path": "/srv/mokuro/.venvs/engines",
    "engines_installed": False,
    "supported_backends": ["cpu", "rocm"],
    "generations": [{"id": "g-1", "name": "mokuro"}, {"id": "g-2", "name": "hayai-nova-ctd"}],
    "cli_hint": "Use `mokuro-bunko serve --ocr <auto|cuda|rocm|cpu|skip>`.",
    "driver_hint": "",
}

# What is configured is not what is installed: both have to be said, and
# neither may be claimed to be the other.
RUNTIME_MISMATCH: dict[str, Any] = dict(
    RUNTIME_NOT_INSTALLED,
    configured_backend="cuda",
    installed=True,
    installed_backend="rocm",
    engines_installed=True,
    detector_ready=False,
)

PIPELINE_RUNNING = {
    "verdict": "engine starved 41% waiting on detect — widen detect",
    "bottleneck": "detect",
    "stages": [
        {"key": "detect", "name": "read page + detection", "device": "cpu", "workers": 2, "busy_pct": 98.2, "starved_pct": 0.4, "blocked_pct": 1.1,
         "queue": {"name": "detect->engine", "capacity": 1, "mean_depth": 0.08, "max_depth": 1}},
        {"key": "engine", "name": "engine read", "device": "gpu:0", "workers": 1, "busy_pct": 57.5, "starved_pct": 41.2, "blocked_pct": 1.3,
         "queue": {"name": "engine->post", "capacity": 4, "mean_depth": 0.22, "max_depth": 2}},
        {"key": "post", "name": "assemble + place", "device": "cpu", "workers": 2, "busy_pct": 18.9, "starved_pct": 80.6, "blocked_pct": 0.0,
         "queue": {"name": "post->out", "capacity": 4, "mean_depth": 0.05, "max_depth": 1}},
    ],
}

PIPELINE_BALANCED = {
    "verdict": None,
    "bottleneck": "layout",
    "stages": [
        {"key": "detect", "name": "line detection", "device": "cpu", "workers": 4, "busy_pct": 62.0, "starved_pct": 3.0, "blocked_pct": 35.0,
         "queue": {"name": "detect->layout", "capacity": 8, "mean_depth": 5.1, "max_depth": 8}},
        {"key": "layout", "name": "line read + layout", "device": "cpu", "workers": 4, "busy_pct": 71.5, "starved_pct": 27.0, "blocked_pct": 1.5,
         "queue": {"name": "layout->out", "capacity": 8, "mean_depth": 0.4, "max_depth": 3}},
    ],
}

QUEUE_STATUS_NEW: dict[str, Any] = {
    "backend": "rocm",
    "generations": [
        {"id": "g-1", "name": "mokuro", "engine": "mokuro", "detector": None},
        {"id": "g-2", "name": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd"},
        {"id": "g-3", "name": "hayai-nova-ppocr-manga", "engine": "hayai-nova", "detector": "ppocr-manga"},
        {"id": "g-4", "name": "paddle-manga", "engine": "paddle-manga", "detector": "ppocr-manga"},
    ],
    "current_jobs": [
        {
            "series": "Dr STONE", "volume": "Volume 07", "status": "running",
            "percent": 62, "done_pages": 119, "total_pages": 192, "eta_seconds": 214,
            "generation": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd",
            "pipeline": PIPELINE_RUNNING, "machine": "local", "slot": 0, "started_at": 100.0,
            "rate_pages_per_second": 0.56, "rate_source": "session", "latency_seconds": 2.4,
        },
        {
            "series": "よこしまビジネスの実験",
            "volume": "Volume 12 — a very long volume title that has to wrap somewhere",
            "status": "running",
            "percent": 8, "done_pages": 15, "total_pages": 188, "eta_seconds": 5400,
            "generation": "ppocr-manga", "engine": "ppocr-manga", "detector": "ppocr-manga",
            "pipeline": PIPELINE_BALANCED, "machine": "local", "slot": 1, "started_at": 101.0,
        },
    ],
    "speed": [
        {"generation": "hayai-nova-ctd", "generation_id": "g-2",
         "machines": [{"machine": "local", "pages_per_minute": 33.6, "volumes": 6,
                       "lanes": 1, "working": True}],
         "combined_pages_per_minute": 33.6},
    ],
    "pending_ocr": [
        {"series": "Dr STONE", "volume": "Volume 08", "generation": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd"},
        {"series": "Dr STONE", "volume": "Volume 07", "generation": "paddle-manga", "engine": "paddle-manga", "detector": "ppocr-manga", "attempts": 1},
        {"series": "よこしまビジネスの実験", "volume": "Volume 13", "generation": "mokuro", "engine": "mokuro", "detector": None},
    ],
    "pending_thumbnails": 2,
    "failed": [
        {
            "series": "Dr STONE", "volume": "Volume 03", "attempts": 2,
            "generation": "paddle-manga", "engine": "paddle-manga", "detector": "ppocr-manga",
            "error": "engine runner exited with code 1: CUDA out of memory",
            "log_file": "/srv/mokuro/logs/ocr/Dr STONE_Volume 03.paddle-manga.log",
        }
    ],
    # Volumes uploaded short of the pages their .mokuro names: every
    # non-primary generation steps over them, and no amount of waiting fixes
    # it. One entry carries `page_count` and one does not.
    "skipped_missing_pages": [
        {
            "series": "Dr STONE", "volume": "Volume 05",
            "missing_pages": 1, "page_count": 194,
            "generations": ["hayai-nova-ctd", "hayai-nova-ppocr-manga", "paddle-manga"],
        },
        {
            "series": "よこしまビジネスの実験", "volume": "Volume 02",
            "missing_pages": 12,
            "generations": ["hayai-nova-ctd"],
        },
    ],
}

# Two volumes of ONE generation in flight at once: the model stayed open, the
# next volume's pages are already in the pipeline while the last of this one
# drains. They report their own progress and SHARE the pipeline -- the same
# object, byte for byte, because there is only one.
QUEUE_STATUS_SESSION: dict[str, Any] = dict(
    deepcopy(QUEUE_STATUS_NEW),
    current_jobs=[
        {
            "series": "Dr STONE", "volume": "Volume 07", "status": "finalizing",
            "percent": 98, "done_pages": 189, "total_pages": 192, "eta_seconds": 8,
            "generation": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd",
            "pipeline": deepcopy(PIPELINE_RUNNING), "machine": "local", "slot": 0,
            "started_at": 100.0,
        },
        {
            "series": "Dr STONE", "volume": "Volume 08", "status": "starting",
            "percent": 0, "done_pages": 0, "total_pages": 186, "eta_seconds": 900,
            "generation": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd",
            "pipeline": deepcopy(PIPELINE_RUNNING), "machine": "local", "slot": 0,
            "started_at": 101.0,
        },
    ],
)

# This server's own hardware, as the server probes it once (`describe_host`,
# the probe a processor registers with) and sends as CPU and GPU only.
LOCAL_HOST: dict[str, Any] = {"cpu": "Ryzen 9 7950X (16 cores)", "gpu": "Radeon RX 7900 XTX"}

# Remote OCR processors (spec section 6): who is connected, who was refused.
PROCESSORS: dict[str, Any] = {
    "processors": [
        {"processor_id": "local", "name": "this server", "label": "this server",
         "username": "", "host": dict(LOCAL_HOST), "catalog": {}, "max_sessions": 0, "sessions": 0,
         "connected_since": 0, "last_seen": 0, "installing": False, "local": True},
        {"processor_id": "p1", "name": "tower", "label": "tower (RTX 4090)",
         "username": "tower",
         "host": {"cpu": "Threadripper (48 cores)", "gpu": "RTX 4090", "backend": "cuda"},
         # What a processor registers (`processor.cli._catalog`): its own
         # probe's `DeviceCatalog.entries()`, rendered on tower, and each
         # card's formats.
         "catalog": {"engines": ["mokuro", "hayai-nova"], "detectors": ["ctd"],
                     "devices": [
                         {"id": "auto", "label": "Auto — GPU 0 when available"},
                         {"id": "cpu",
                          "label": "AMD Ryzen Threadripper 9960X 24-Cores (48 cores)"},
                         {"id": "gpu:0", "label": "GPU 0 — NVIDIA GeForce RTX 4090 (25 GB)"},
                     ],
                     "gpus": [{"index": 0, "formats": {"bf16": True, "fp16": True}}],
                     "serves_mokuro": True},
         "max_sessions": 2, "sessions": 1,
         "connected_since": 1790000000.0, "last_seen": 1790000100.0,
         "installing": False, "local": False},
        {"processor_id": "p2", "name": "box", "label": "box", "username": "box",
         "host": {"cpu": "N100 (4 cores)", "gpu": None, "backend": "cpu"},
         "catalog": {}, "max_sessions": 1, "sessions": 0,
         "connected_since": 1790000050.0, "last_seen": 1790000060.0,
         "installing": True, "local": False},
    ],
    "failed_logins": [
        {"username": "typo", "reason": "invalid credentials from 203.0.113.7",
         "at": 1790000050.0},
        {"username": "<img src=x onerror=alert(1)>", "reason": "invalid credentials",
         "at": 1790000040.0},
    ],
    "last_disconnect": None,
    "local_processing": True,
    "processing_hold": None,
    # Per machine, per generation: REAL throughput (pages of recent finished
    # volumes over their wall time), the volumes behind it, the machine's own
    # benchmark and when it last ran the layer. Each machine carries its
    # hardware: this server's from the probe, a connected processor's from its
    # registration, an offline one's as it last registered
    # (``processors/<name>.json``); None when not known.
    "speed": [
        {"name": "local", "local": True, "connected": True, "host": dict(LOCAL_HOST),
         "layers": [
            {"generation_id": "g-1", "generation": "mokuro", "pages_per_minute": 21.4,
             "volumes": 9, "last_at": 1790000000.0, "bench_pages_per_minute": 24.0},
        ]},
        {"name": "tower", "local": False, "connected": True,
         "host": {"cpu": "Threadripper (48 cores)", "gpu": "RTX 4090"}, "layers": [
            {"generation_id": "g-1", "generation": "mokuro", "pages_per_minute": 1529.0,
             "volumes": 20, "last_at": 1790000100.0, "bench_pages_per_minute": 2933.0},
            {"generation_id": "g-2", "generation": "hayai-nova-ctd", "pages_per_minute": 440.0,
             "volumes": 20, "last_at": 1790000090.0, "bench_pages_per_minute": 659.2},
        ]},
        {"name": "old-laptop", "local": False, "connected": False,
         "host": {"cpu": "i5-8250U (4 cores)", "gpu": "MX150"}, "layers": [
            {"generation_id": "g-1", "generation": "mokuro", "pages_per_minute": 6.2,
             "volumes": 3, "last_at": 1789000000.0, "bench_pages_per_minute": None},
        ]},
    ],
}

# A processor whose name and hardware are as long as real ones get: the
# registry keeps names up to MAX_PROCESSOR_NAME and a laptop GPU's marketing
# name is this long. The Processors table has to wrap it, never cut it.
PROCESSOR_LONG_NAME: dict[str, Any] = {
    "processor_id": "p3", "name": "very-long-processor-hostname-01",
    "label": "very-long-processor-hostname-01 (NVIDIA GeForce RTX 4090 Laptop GPU)",
    "username": "laptop",
    "host": {"cpu": "AMD Ryzen 9 7945HX with Radeon Graphics (32 cores)",
             "gpu": "NVIDIA GeForce RTX 4090 Laptop GPU", "backend": "cuda"},
    "catalog": {"engines": ["mokuro"], "detectors": []}, "max_sessions": 2, "sessions": 2,
    "connected_since": 1790000070.0, "last_seen": 1790000100.0,
    "installing": False, "local": False,
}

SPEED_LONG_NAME: dict[str, Any] = {
    "name": "very-long-processor-hostname-01", "local": False, "connected": True,
    "host": {"cpu": "AMD Ryzen 9 7945HX with Radeon Graphics (32 cores)",
             "gpu": "NVIDIA GeForce RTX 4090 Laptop GPU"},
    "layers": [
        {"generation_id": "g-1", "generation": "mokuro", "pages_per_minute": 812.0,
         "volumes": 14, "last_at": 1790000080.0, "bench_pages_per_minute": 1020.0},
        {"generation_id": "g-3", "generation": "paddle-manga-experimental",
         "pages_per_minute": 96.5, "volumes": 4, "last_at": 1790000060.0,
         "bench_pages_per_minute": None},
    ],
}


def processors_with_long_name() -> dict[str, Any]:
    """`PROCESSORS` plus the long-named machine, connected, with numbers."""
    out = deepcopy(PROCESSORS)
    out["processors"].append(deepcopy(PROCESSOR_LONG_NAME))
    out["speed"].insert(2, deepcopy(SPEED_LONG_NAME))
    return out


QUEUE_STATUS_REMOTE: dict[str, Any] = dict(
    deepcopy(QUEUE_STATUS_SESSION),
    processing_hold=None,
    current_jobs=[
        dict(deepcopy(QUEUE_STATUS_SESSION["current_jobs"][0]),
             processor="tower (RTX 4090)", machine="tower")
    ],
)

QUEUE_STATUS_NO_PROCESSOR: dict[str, Any] = dict(
    deepcopy(QUEUE_STATUS_SESSION),
    current=None,
    current_jobs=[],
    processing_hold={
        "reason": "no-processor",
        "since": 1790000000.0,
        "last": {"name": "tower", "disconnected_at": 1790000200.0},
    },
)

# Everything the ETA readout can show, at once, on a FIXED clock. Every
# instant here is 2026-09-22 UTC and the browser test pins the page's
# `Date.now()` to 04:30 UTC that day, so the rendered strings are a statement
# about the formatting rather than about when the test happened to run.
#
# In Asia/Tokyo (UTC+9) these read 14:32, 14:07, 14:40 and 10:00 the NEXT
# day, and the queue ends at 15:48.
QUEUE_STATUS_ETA: dict[str, Any] = dict(
    deepcopy(QUEUE_STATUS_NEW),
    current_jobs=[
        {
            "series": "Dr STONE", "volume": "Volume 07", "status": "running",
            "percent": 62, "done_pages": 119, "total_pages": 192,
            "eta_seconds": 130, "eta_at": "2026-09-22T05:32:00Z",
            "rate_pages_per_second": 0.56, "rate_source": "session",
            "generation": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd",
            "machine": "local", "slot": 0, "started_at": 100.0,
        },
        {
            # Submitted, models still loading. No page has come out, so the
            # card says what the load costs rather than extrapolating a rate
            # from how long it has been waiting.
            "series": "Dr STONE", "volume": "Volume 09", "status": "starting",
            "percent": 0, "done_pages": 0, "total_pages": 186,
            "eta_seconds": 420, "eta_at": "2026-09-22T05:55:00Z",
            "startup_seconds": 20, "startup_rough": True,
            "generation": "ppocr-manga", "engine": "ppocr-manga", "detector": "ppocr-manga",
            "machine": "local", "slot": 1, "started_at": 101.0,
        },
        {
            # A session's lookahead volume: nothing of its OWN has come out
            # either, but the pipeline is warm (no startup left) and the one
            # ahead of it has a measured rate, so its turn has a time.
            "series": "Dr STONE", "volume": "Volume 11", "status": "starting",
            "percent": 0, "done_pages": 0, "total_pages": 190,
            "eta_seconds": 1920, "eta_at": "2026-09-22T06:02:00Z",
            "startup_seconds": None, "startup_rough": False,
            "generation": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd",
            "machine": "local", "slot": 0, "started_at": 102.0,
        },
    ],
    pending_ocr=[
        {
            "series": "Dr STONE", "volume": "Volume 08", "generation": "hayai-nova-ctd",
            "engine": "hayai-nova", "detector": "ctd",
            "pages": 186, "eta_seconds": 2220, "eta_at": "2026-09-22T05:07:00Z",
            "rate_source": "session", "rough": False, "reason": None,
        },
        {
            # The metadata pass has not compiled this one, so its length is
            # the median of the queue: the page says so with its own mark.
            "series": "Dr STONE", "volume": "Volume 10", "generation": "hayai-nova-ctd",
            "engine": "hayai-nova", "detector": "ctd",
            "pages": 190, "eta_seconds": 4200, "eta_at": "2026-09-22T05:40:00Z",
            "rate_source": "session", "rough": True, "reason": None,
        },
        {
            "series": "よこしまビジネスの実験", "volume": "Volume 13", "generation": "mokuro",
            "engine": "mokuro", "detector": None,
            "pages": 200, "eta_seconds": 73800, "eta_at": "2026-09-23T01:00:00Z",
            "rate_source": "bench", "rough": False, "reason": None,
        },
        {
            "series": "よこしまビジネスの実験", "volume": "Volume 14",
            "generation": "paddle-manga", "engine": "paddle-manga", "detector": "ppocr-manga",
            "pages": 200, "eta_seconds": None, "eta_at": None,
            "rate_source": None, "rough": False,
            "reason": "nothing has measured how fast paddle-manga reads a page yet",
        },
    ],
    queue_done_at="2026-09-22T06:48:00Z",
)

def two_machines_status(now: datetime | None = None) -> dict[str, Any]:
    """Two machines working, one with a volume on deck: the screenshot fixture.

    Instants are relative to ``now`` so the bars really move on screen. The
    error text, log path and hardware label are what an ADMIN is sent; the
    stub strips them for a visitor with the server's own `shape_status`.
    """
    now = now or datetime.now(timezone.utc)

    def at(seconds: float) -> str:
        return (now + timedelta(seconds=seconds)).strftime("%Y-%m-%dT%H:%M:%SZ")

    return {
        "backend": "rocm",
        "generations": deepcopy(QUEUE_STATUS_NEW["generations"]),
        "current_jobs": [
            {
                "series": "Dr STONE", "volume": "Volume 07", "status": "running",
                "percent": 62, "done_pages": 119, "total_pages": 192,
                "eta_seconds": 130, "eta_at": at(130),
                "generation": "hayai-nova-ctd", "generation_id": "g-2",
                "engine": "hayai-nova", "detector": "ctd",
                "machine": "tower", "processor": "tower (RTX 4090)", "slot": 2,
                "started_at": 100.0, "rate_pages_per_second": 0.8,
                "rate_source": "session", "latency_seconds": 2.4,
                "pipeline": deepcopy(PIPELINE_RUNNING),
            },
            {
                "series": "Dr STONE", "volume": "Volume 08", "status": "starting",
                "percent": 0, "done_pages": 0, "total_pages": 186,
                "eta_seconds": 380, "eta_at": at(380),
                "generation": "hayai-nova-ctd", "generation_id": "g-2",
                "engine": "hayai-nova", "detector": "ctd",
                "machine": "tower", "processor": "tower (RTX 4090)", "slot": 2,
                "started_at": 101.0,
            },
            {
                "series": "よこしまビジネスの実験", "volume": "Volume 12",
                "status": "running", "percent": 8, "done_pages": 15, "total_pages": 188,
                "eta_seconds": 520, "eta_at": at(520),
                "generation": "mokuro", "generation_id": "g-1",
                "engine": "mokuro", "detector": None,
                "machine": "local", "slot": 0, "started_at": 99.0,
                "rate_pages_per_second": 0.35, "rate_source": "history+volume",
                "latency_seconds": 1.1, "pipeline": deepcopy(PIPELINE_BALANCED),
            },
        ],
        "pending_ocr": [
            {"series": "Dr STONE", "volume": "Volume 09", "generation": "hayai-nova-ctd",
             "generation_id": "g-2", "engine": "hayai-nova", "detector": "ctd",
             "pages": 190, "eta_at": at(620), "eta_seconds": 620, "rough": False,
             "reason": None, "rate_source": "session", "latency_seconds": 2.4},
            {"series": "よこしまビジネスの実験", "volume": "Volume 13", "generation": "mokuro",
             "generation_id": "g-1", "engine": "mokuro", "detector": None,
             "pages": None, "eta_at": at(1100), "eta_seconds": 1100, "rough": True,
             "reason": None, "rate_source": "history", "latency_seconds": 1.1},
            {"series": "Dr STONE", "volume": "Volume 03", "generation": "paddle-manga",
             "generation_id": "g-4", "engine": "paddle-manga", "detector": "ppocr-manga",
             "pages": 200, "eta_at": at(2400), "eta_seconds": 2400, "rough": False,
             "attempts": 2, "rate_source": "bench", "latency_seconds": None,
             "reason": None},
        ],
        "queue_done_at": at(2400),
        "pending_thumbnails": 1,
        "failed": [
            {"series": "Dr STONE", "volume": "Volume 03", "attempts": 2,
             "generation": "paddle-manga", "engine": "paddle-manga", "detector": "ppocr-manga",
             "error": "engine runner exited with code 1: CUDA out of memory",
             "log_file": "/srv/mokuro/logs/ocr/Dr STONE_Volume 03.paddle-manga.log",
             "last_attempt_at": 1790000000.0},
        ],
        "skipped_missing_pages": [],
        "processing_hold": None,
        # Both machines stay connected whatever they are doing: each keeps a
        # card of a fixed size.
        "connected_machines": [{"machine": "local", "slots": 1},
                               {"machine": "tower", "slots": 1}],
        # The server's RAW report: real throughput per machine (what the
        # admin panel breaks down), shaped to one combined line per layer at
        # `detailed` and to nothing at all below it.
        "speed": [
            {"generation": "hayai-nova-ctd", "generation_id": "g-2",
             "machines": [{"machine": "tower", "pages_per_minute": 48.0, "volumes": 12,
                           "lanes": 1, "working": True},
                          {"machine": "local", "pages_per_minute": 21.0, "volumes": 4,
                           "lanes": 0, "working": False}],
             "combined_pages_per_minute": 48.0},
            {"generation": "mokuro", "generation_id": "g-1",
             "machines": [{"machine": "local", "pages_per_minute": 21.0, "volumes": 9,
                           "lanes": 1, "working": True},
                          {"machine": "tower", "pages_per_minute": 66.0, "volumes": 20,
                           "lanes": 0, "working": False}],
             "combined_pages_per_minute": 21.0},
        ],
    }


def machine_phase(status: dict[str, Any], phase: str, now: datetime | None = None) -> dict[str, Any]:
    """`two_machines_status` with tower in one PHASE: running, loading, waiting, idle.

    Drives the "a card never changes size between jobs" check and the
    screenshots of each state.
    """
    now = now or datetime.now(timezone.utc)
    out = deepcopy(status)
    tower = [job for job in out["current_jobs"] if job.get("machine") == "tower"]
    rest = [job for job in out["current_jobs"] if job.get("machine") != "tower"]
    if phase == "idle":
        out["current_jobs"] = rest
        return out
    active = tower[0]
    if phase == "loading":
        active.update(status="starting", percent=0, done_pages=0, eta_at=None,
                      eta_seconds=None, startup_seconds=20, startup_rough=True,
                      session_ready=False,
                      volume="Volume 10 — a volume title long enough to be cut off at a phone's width")
        out["current_jobs"] = [active] + rest
    elif phase == "waiting":
        # The session is warm; its processor is still fetching the archive.
        active.update(status="starting", percent=0, done_pages=0, startup_seconds=None,
                      session_ready=True, delivered=False, volume="Volume 11")
        out["current_jobs"] = [active] + rest
    return out


def _ordinal(n: int) -> str:
    if 10 <= n % 100 <= 20:
        suffix = "th"
    else:
        suffix = {1: "st", 2: "nd", 3: "rd"}.get(n % 10, "th")
    return f"{n}{suffix}"


class StubState:
    """Everything a test wants to change between page loads.

    Benchmarks (ADDENDA 5 & 6) are a FIFO: `bench_order` holds every key
    (generation id or client draft id) currently queued or running, index 0
    always the running one. Posting a new key appends it; a key already in
    the line answers 409. The head's own states are a scripted walk (pausing
    -> running trials -> done), exactly like before, but now generalized to
    whichever key is at index 0 -- and when it settles, the next key in line
    is promoted and its own script starts fresh.
    """

    def __init__(self) -> None:
        self.generations = deepcopy(GENERATIONS)
        self.catalog = deepcopy(CATALOG)
        self.queue_status = deepcopy(QUEUE_STATUS_NEW)
        # `queue.status` is RAW -- what `QueueAPI.raw_status` assembles; the
        # stub shapes it with the server's own `shape_status` for this level
        # and viewer, so the page is tested against the real redaction.
        self.queue_level = "normal"
        self.queue_admin = False
        # (status code, If-None-Match sent) per status poll, and whether each
        # carried an Authorization header.
        self.status_polls: list[tuple[int, str | None]] = []
        self.status_auth: list[bool] = []
        # True: answer every status poll that carries credentials as the
        # server answers a stored login that no longer works.
        self.queue_auth_failed = False
        self.settings = deepcopy(SETTINGS)
        # `/_admin/api/processors`, and what the generations list says about
        # them: none by default, so every page looks as it did before
        # processors existed.
        self.processors = deepcopy(PROCESSORS)
        self.gen_processors: list[dict[str, Any]] = []
        self.gen_local_processing = True
        # Every PUT of one processor's pools, and every derive asked FOR one.
        self.pools_puts: list[tuple[str, dict[str, Any]]] = []
        self.derive_processors: list[str | None] = []
        self.users: list[dict[str, Any]] = []
        self.role_puts: list[tuple[str, dict[str, Any]]] = []
        self.user_posts: list[dict[str, Any]] = []
        # False = answer every bench request 404, the way a server that
        # predates benchmarks does.
        self.bench_supported = True
        # None = echo the PUT back as a success; a dict = answer it verbatim
        # with `put_status`.
        self.put_response: dict[str, Any] | None = None
        self.put_status = 200
        self.last_put: dict[str, Any] | None = None
        # Every spec POSTed to /ocr/generations/derive, in order: what a test
        # diffs to prove the table follows an edit without a save.
        self.derived: list[dict[str, Any]] = []
        # What derive answers, by (engine, detector) -- else the stages of the
        # first saved row with that engine, else a plain adapter road.
        self.derive_stages: dict[str, list[dict[str, Any]]] = {}

        # The FIFO: order[0] is running (or pausing the OCR queue to start),
        # order[1:] are queued behind it, position = index.
        self.bench_order: list[str] = []
        # The head's own walk of raw (undecorated) states -- every GET on the
        # head pops one and the last one sticks, which is what a finished
        # benchmark does. Installed fresh whenever a key becomes head.
        self.bench_scripts: dict[str, list[dict[str, Any]]] = {}
        # What a key gets installed as ITS script when it becomes head,
        # instead of the default BENCH_RUN_SEQUENCE.
        self.bench_on_post: dict[str, list[dict[str, Any]]] = {}
        # The last POST body (spec + pages) received for a key -- what a test
        # diffs against the row's own PUT body to prove they agree.
        self.bench_specs: dict[str, dict[str, Any]] = {}
        # A key's last SETTLED (done/failed/cancelled) raw state, kept after
        # it leaves the line -- a real id's `done` result is what a real
        # server would have persisted to `.ocr-bench.json`; a draft key's is
        # memory-only and `simulate_server_restart()` drops it.
        self.bench_settled: dict[str, dict[str, Any]] = {}
        # The key whose POST triggered the current pre-emption (the first one
        # to join an empty line); carries `PREEMPTED_SAMPLE` until the whole
        # line empties out again. Every OTHER key in the same session reports
        # no preemption of its own -- the queue was held once, not per key.
        self.bench_preempted_for: str | None = None
        self.bench_preempted_list: list[dict[str, str]] = PREEMPTED_SAMPLE
        # Set to answer every POST with this instead (409, 400) -- bypasses
        # the FIFO logic entirely, for a test that wants an exact refusal.
        self.bench_post_response: dict[str, Any] | None = None
        self.bench_post_status = 202
        self.bench_posts: list[tuple[str, dict[str, Any]]] = []
        self.bench_deletes: list[str] = []
        # Every GET a key's bench endpoint received, in order: what a test
        # reads to see whether the page settled or is still asking.
        self.bench_gets: list[str] = []
        # The machine every bench GET and DELETE named (`?processor=`), or
        # None when it named none -- what a test reads to see that a read or
        # a Cancel is ONE machine's.
        self.bench_get_machines: list[tuple[str, str | None]] = []
        self.bench_delete_machines: list[tuple[str, str | None]] = []
        # The machine each key was POSTed for, echoed on its answers the way
        # the real server's run snapshot carries `processor`.
        self.bench_machines: dict[str, str] = {}

    def generations_body(self) -> dict[str, Any]:
        generations = deepcopy(self.generations)
        for row in generations:
            # Every stage's options in THIS server's words, and each
            # processor's own table in its own (`processor_stages`), as the
            # server works them out for the GET.
            row["stages"] = with_device_options(
                row.get("stages") or [], machine_devices(self, "local")
            )
            if self.gen_processors and "processor_stages" not in row:
                row["processor_stages"] = {
                    p["name"]: derive_body(self, machine_spec(row, p["name"]), p["name"])["stages"]
                    for p in self.gen_processors
                }
        body: dict[str, Any] = {"generations": generations, "catalog": self.catalog}
        if self.gen_processors:
            body["processors"] = self.gen_processors
            body["local_processing"] = self.gen_local_processing
        return body

    # ---- bench: decoration ------------------------------------------------

    def _generation_name(self, key: str) -> str:
        row = next((g for g in self.generations if g.get("id") == key), None)
        return (row.get("name") if row else None) or key

    @staticmethod
    def _bench_slot(key: str, machine: str | None) -> str:
        """Where one key's benchmark on one machine lives in this stub: the
        bare key for this server's own (every existing test's), `key@name`
        for a processor's -- the real server keys them by (key, machine) too,
        so one row can be benchmarked on several machines at once."""
        return key if machine in (None, "", "local") else f"{key}@{machine}"

    @staticmethod
    def _bench_key(slot: str) -> str:
        return slot.split("@", 1)[0]

    def _decorate(self, key: str, raw: dict[str, Any]) -> dict[str, Any]:
        obj = deepcopy(raw)
        obj["key"] = self._bench_key(key)
        obj["queue"] = {
            "running": self.bench_order[0] if self.bench_order else None,
            "queued": list(self.bench_order[1:]),
        }
        if key in self.bench_order:
            # Still part of the live line: `position` and `preempted` are
            # both CURRENT facts, recomputed on every read.
            obj["position"] = self.bench_order.index(key)
            obj["preempted"] = (
                deepcopy(self.bench_preempted_list) if key == self.bench_preempted_for else []
            )
        else:
            # Settled (or never run): `preempted` is a fact about that ONE
            # run, frozen into the raw object when it settled (see
            # `bench_get`/`bench_delete`) -- it must not silently change on a
            # later poll just because something else is holding the queue now.
            obj.setdefault("position", None)
            obj.setdefault("preempted", [])
        posted = self.bench_specs.get(key) or {}
        if key in self.bench_machines:
            obj.setdefault("processor", self.bench_machines[key])
        obj.setdefault("spec", posted.get("spec"))
        obj.setdefault("generation", self._generation_name(self._bench_key(key)))
        return obj

    # ---- bench: the FIFO ---------------------------------------------------

    def _activate_head(self, key: str) -> None:
        self.bench_scripts[key] = deepcopy(
            self.bench_on_post.get(key) or self.bench_on_post.get(self._bench_key(key))
            or BENCH_RUN_SEQUENCE
        )

    def _advance_head(self, key: str) -> dict[str, Any]:
        script = self.bench_scripts.get(key)
        if not script:
            self._activate_head(key)
            script = self.bench_scripts[key]
        raw = script.pop(0) if len(script) > 1 else deepcopy(script[0])
        # A shared default script (BENCH_RUN_SEQUENCE) carries a fixture
        # generation id from whatever row it was written for; when a
        # DIFFERENT key borrows it (a draft, or any other row with no script
        # of its own), the label must still name the key actually running.
        raw["generation"] = self._generation_name(self._bench_key(key))
        return raw

    def _promote_next(self) -> None:
        if self.bench_order:
            self._activate_head(self.bench_order[0])
        else:
            self.bench_preempted_for = None

    def bench_get(self, gid: str, machine: str | None = None) -> dict[str, Any]:
        key = self._bench_slot(gid, machine)
        if key in self.bench_order:
            idx = self.bench_order.index(key)
            if idx == 0:
                raw = self._advance_head(key)
                obj = self._decorate(key, raw)
                if raw.get("state") in ("done", "failed", "cancelled"):
                    # Freeze what THIS run actually interrupted before it
                    # leaves the line -- `obj["preempted"]` was just computed
                    # while `key` was still active, which is the one moment
                    # that fact is knowable.
                    self.bench_settled[key] = dict(raw, preempted=obj["preempted"])
                    self.bench_order.pop(0)
                    self._promote_next()
                return obj
            return self._decorate(key, bench_queued(key))
        if key in self.bench_settled:
            return self._decorate(key, self.bench_settled[key])
        return self._decorate(key, bench_idle(key))

    def bench_post(self, gid: str, body: dict[str, Any]) -> tuple[dict[str, Any], int]:
        self.bench_posts.append((gid, body))
        if self.bench_post_response is not None:
            return self.bench_post_response, self.bench_post_status
        key = self._bench_slot(gid, body.get("processor"))
        if key in self.bench_order:
            idx = self.bench_order.index(key)
            where = "running" if idx == 0 else f"queued ({_ordinal(idx)} in line)"
            return (
                {"error": f"A benchmark for this generation is already {where}.", "key": gid},
                409,
            )
        self.bench_specs[key] = body
        self.bench_machines[key] = str(body.get("processor") or "local")
        was_empty = not self.bench_order
        self.bench_order.append(key)
        if was_empty:
            self.bench_preempted_for = key
            self._activate_head(key)
            raw = self._advance_head(key)
            return self._decorate(key, raw), 202
        return self._decorate(key, bench_queued(key)), 202

    def bench_delete(self, gid: str, machine: str | None = None) -> dict[str, Any]:
        self.bench_deletes.append(gid)
        key = self._bench_slot(gid, machine)
        if key in self.bench_order:
            idx = self.bench_order.index(key)
            # Compute what THIS run interrupted while it is still in the
            # line, then freeze it into the stored settled record.
            preempted = deepcopy(self.bench_preempted_list) if key == self.bench_preempted_for else []
            cancelled = dict(deepcopy(BENCH_CANCELLED), generation=self._generation_name(gid),
                              preempted=preempted)
            self.bench_settled[key] = cancelled
            self.bench_order.pop(idx)
            if idx == 0:
                self._promote_next()
            elif not self.bench_order:
                self.bench_preempted_for = None
            return self._decorate(key, cancelled)
        if key in self.bench_settled:
            return self._decorate(key, self.bench_settled[key])
        return self._decorate(key, bench_idle(key))

    # ---- bench: test controls ----------------------------------------------

    def script_bench(
        self, gid: str, states: list[dict[str, Any]], preempted: bool = False,
        machine: str | None = None,
    ) -> None:
        """Force `gid` to the head of the line with this exact script, ahead
        of whatever else is queued -- for a test that wants a benchmark
        already going (or already settled) the instant the page loads.
        `preempted=True` also claims this run as the one that pre-empted the
        OCR queue, so its `preempted` field carries `bench_preempted_list`.
        `machine` puts it on that processor rather than this server."""
        gid = self._bench_slot(gid, machine)
        if machine not in (None, "", "local"):
            self.bench_machines[gid] = machine
        if gid in self.bench_order:
            self.bench_order.remove(gid)
        self.bench_order.insert(0, gid)
        self.bench_scripts[gid] = deepcopy(states)
        if preempted:
            self.bench_preempted_for = gid

    def seed_bench_queue(self, keys: list[str]) -> None:
        """Put several keys in the line at once, in order, as if each had
        been posted in turn -- the head starts its default script (or
        whatever `bench_on_post` names for it), the rest simply wait."""
        self.bench_order = list(keys)
        if keys:
            self._activate_head(keys[0])
            self.bench_preempted_for = keys[0]

    def finish_head_bench(self, result: dict[str, Any] | None = None) -> str | None:
        """The explicit 'advance the line' control: skip straight to a
        finished head instead of walking its whole trial script, settle it,
        and promote whatever is next."""
        if not self.bench_order:
            return None
        key = self.bench_order[0]
        raw = deepcopy(result) if result is not None else deepcopy(BENCH_TUNED)
        raw.setdefault("generation", self._generation_name(key))
        self.bench_settled[key] = raw
        self.bench_order.pop(0)
        self._promote_next()
        return key

    def simulate_server_restart(self) -> None:
        """The queue is memory-only and does not survive; a real id's `done`
        result would have been written to `.ocr-bench.json` and does, so only
        draft-keyed results (never persisted) are dropped."""
        self.bench_order = []
        self.bench_scripts = {}
        self.bench_specs = {}
        self.bench_preempted_for = None
        self.bench_settled = {
            k: v for k, v in self.bench_settled.items() if not k.startswith("draft-")
        }

    def paused_for_benchmark(self) -> dict[str, Any] | None:
        if not self.bench_order:
            return None
        key = self.bench_order[0]
        return {
            "key": key,
            "generation": self._generation_name(key),
            "queued": len(self.bench_order) - 1,
        }


_STATIC = {
    "/_static/shared.css": ("static/shared.css", "text/css"),
    "/_static/nav.js": ("static/nav.js", "application/javascript"),
    "/_admin/": ("admin/web/index.html", "text/html"),
    "/_admin/index.html": ("admin/web/index.html", "text/html"),
    "/_admin/admin.js": ("admin/web/admin.js", "application/javascript"),
    "/_admin/styles.css": ("admin/web/styles.css", "text/css"),
    "/queue/": ("queue/web/index.html", "text/html"),
    "/queue/index.html": ("queue/web/index.html", "text/html"),
    "/queue/queue.js": ("queue/web/queue.js", "application/javascript"),
    "/queue/styles.css": ("queue/web/styles.css", "text/css"),
}


_BENCH_PATH = re.compile(r"^/_admin/api/ocr/generations/([^/]+)/bench$")
_POOLS_PATH = re.compile(r"^/_admin/api/ocr/generations/([^/]+)/pools$")
_ROLE_PATH = re.compile(r"^/_admin/api/users/([^/]+)/role$")


def machine_devices(state: StubState, machine: str | None) -> list[dict[str, Any]]:
    """ONE machine's devices, as the server names them for its Device select.

    This server's probe; a processor's registration, with its own host line
    naming a CPU it left unnamed; plain ``CPU`` where nobody said
    (`DeviceCatalog.label_for` / `for_machine`).
    """
    if machine in (None, "local"):
        return list(state.catalog["devices"])
    found = next((p for p in state.gen_processors if p["name"] == machine), None)
    catalog = (found or {}).get("catalog") or {}
    host = (found or {}).get("host") or {}
    entries = [dict(e) for e in catalog.get("devices") or []]
    if not entries:
        entries = [{"id": "auto", "label": "Auto"}, {"id": "cpu", "label": "CPU"}]
    for entry in entries:
        if entry["id"] == "cpu" and entry.get("label", "CPU") == "CPU" and host.get("cpu"):
            entry["label"] = host["cpu"]
    return entries


def with_device_options(
    stages: list[dict[str, Any]], devices: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    """Stages whose Device select offers THESE devices, labelled as they are."""
    ids = [e["id"] for e in devices]
    labels = {e["id"]: e.get("label") or e["id"] for e in devices}
    auto = next((i for i in ids if i.startswith("gpu:")), "cpu")
    out = []
    for stage in stages:
        stage = dict(stage)
        allowed = stage.get("devices_allowed") or []
        if allowed and allowed != ["auto", "cpu"]:
            allowed = ids
        stage["devices_allowed"] = allowed
        if stage.get("device", "").startswith("gpu:") and stage["device"] not in ids:
            stage["device"] = auto
        stage["device_options"] = [
            {"id": i, "label": ("Auto → " + ("GPU " + auto[4:] if auto != "cpu" else "CPU"))
             if i == "auto" else labels.get(i, "GPU " + i[4:] if i.startswith("gpu:") else "CPU")}
            for i in allowed
        ]
        out.append(stage)
    return out


def machine_spec(row: dict[str, Any], machine: str) -> dict[str, Any]:
    """The row as ONE processor runs it: its stored device table over the row's."""
    stored = ((row.get("processor_pools") or {}).get(machine) or {}).get("stage_device")
    pools = deepcopy(row.get("pools") or {})
    if stored:
        pools["stage_device"] = dict(stored)
    return {**row, "pools": pools}


def derive_body(
    state: StubState, spec: dict[str, Any], machine: str | None = None
) -> dict[str, Any]:
    """The stages this spec WOULD have ON ``machine``, as the server answers them."""
    body = _derive_stages(state, spec)
    body["stages"] = with_device_options(body["stages"], machine_devices(state, machine))
    return body


def _derive_stages(state: StubState, spec: dict[str, Any]) -> dict[str, Any]:
    """The stages this spec WOULD have, the way the server answers them.

    The rules the page depends on, and no more: a model-bearing stage sits
    where the spec puts it (``auto`` = the card), a stage on a card is one
    model on one device (except a recognizer's engine stage, whose Workers
    cell is its number of copies), and a CPU stage is a pool.
    """
    engine = str(spec.get("engine") or "")
    detector = str(spec.get("detector") or "ppocr-manga")
    scripted = state.derive_stages.get(engine + "/" + detector)
    if scripted is not None:
        return {"road": "adapter", "stages": deepcopy(scripted)}
    devices = ((spec.get("pools") or {}).get("stage_device") or {})
    catalog_engine = next((e for e in state.catalog["engines"] if e["id"] == engine), {})
    if catalog_engine.get("monolithic"):
        return {
            "road": None,
            "stages": [
                {"key": "mokuro", "name": "the fork's page pipeline",
                 "device": devices.get("mokuro", "gpu:0"), "max_workers": None,
                 "derived_workers": None, "derived_capacity": None,
                 "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None,
                 "workers_means": "engine"},
            ],
        }
    if catalog_engine.get("served"):
        # The served road: the Device select is on the middle stage, and its
        # Workers cell is the engine's own pool, so it follows the choice
        # without being fixed at one the way a pool of ours would be.
        chosen = devices.get("mokuro", "auto")
        return {
            "road": "served",
            "stages": [
                {"key": "feed", "name": "spool the page", "device": "cpu",
                 "max_workers": None, "derived_workers": 2, "derived_capacity": 4,
                 "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool"},
                {"key": "mokuro", "name": "serve process",
                 "device": "gpu:0" if chosen == "auto" else chosen,
                 "max_workers": 1, "derived_workers": 1, "derived_capacity": 4,
                 "devices_allowed": ["auto", "cpu", "gpu:0"], "device_locked_reason": None,
                 "workers_means": "engine"},
                {"key": "post", "name": "assemble", "device": "cpu",
                 "max_workers": None, "derived_workers": 2, "derived_capacity": 4,
                 "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool"},
            ],
        }
    cpu_only = catalog_engine.get("devices") == ["cpu"]
    stages = []
    for key, name in (("detect", "read page + detection"), ("engine", "engine read")):
        chosen = devices.get(key, "auto")
        # This fixture host runs its detectors on the CPU and its recognizer
        # on the card, which is what the saved rows above show; an explicit
        # choice moves either one.
        auto = "gpu:0" if key == "engine" else "cpu"
        device = "cpu" if cpu_only else (auto if chosen == "auto" else chosen)
        on_card = device.startswith("gpu")
        stages.append({
            "key": key, "name": name, "device": device,
            "max_workers": 1 if key == "engine" else None,
            "derived_workers": 1 if on_card or key == "engine" else 3,
            "derived_capacity": 2,
            "devices_allowed": ["auto", "cpu"] if cpu_only else ["auto", "cpu", "gpu:0"],
            "device_locked_reason": ("this recognizer runs on the CPU" if cpu_only else None),
            # A recognizer on a card is run as N copies of the model, which
            # is what its Workers cell sets; every other stage is a pool.
            "workers_means": "copies" if key == "engine" and on_card else "pool",
        })
    stages.append({
        "key": "post", "name": "assemble", "device": "cpu", "max_workers": None,
        "derived_workers": 2, "derived_capacity": 4,
        "devices_allowed": [], "device_locked_reason": None, "workers_means": "pool",
    })
    return {"road": "adapter", "stages": stages}


def make_handler(state: StubState):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *args: Any) -> None:  # noqa: D102 - quiet
            pass

        def _bench_id(self, path: str) -> str | None:
            match = _BENCH_PATH.match(path)
            return unquote(match.group(1)) if match else None

        def _machine(self) -> str | None:
            """The `?processor=` a bench request named, or None."""
            query = parse_qs(self.path.partition("?")[2])
            named = query.get("processor")
            return named[0] if named else None

        def _send(self, code: int, body: bytes, content_type: str) -> None:
            self.send_response(code)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def _json(self, payload: Any, code: int = 200) -> None:
            self._send(code, json.dumps(payload).encode(), "application/json")

        def _no_bench(self) -> None:
            """Exactly what the shipped server answers for an unknown route."""
            self._json({"error": "API endpoint not found"}, 404)

        def do_GET(self) -> None:  # noqa: N802
            path = self.path.split("?")[0]
            gid = self._bench_id(path)
            if gid is not None:
                state.bench_gets.append(gid)
                state.bench_get_machines.append((gid, self._machine()))
                if not state.bench_supported:
                    self._no_bench()
                    return
                self._json(state.bench_get(gid, self._machine()))
                return
            if path in _STATIC:
                rel, ctype = _STATIC[path]
                data = (WEB_ROOT / rel).read_bytes()
                charset = "; charset=utf-8" if ctype != "image/png" else ""
                self._send(200, data, ctype + charset)
                return
            if path == "/_admin/api/ocr/generations":
                self._json(state.generations_body())
                return
            if path == "/_admin/api/settings":
                self._json(state.settings)
                return
            if path == "/_admin/api/users":
                self._json({"users": state.users})
                return
            if path == "/_admin/api/processors":
                self._json(state.processors)
                return
            if path == "/_admin/api/invites":
                self._json({"invites": []})
                return
            if path == "/_admin/api/audit":
                self._json({"events": []})
                return
            if path == "/queue/api/config":
                self._json({"show_in_nav": True, "public_access": True})
                return
            if path == "/queue/api/status":
                # `paused_for_benchmark` reflects the live FIFO on every read.
                raw = dict(state.queue_status)
                raw["paused_for_benchmark"] = state.paused_for_benchmark()
                payload = shape_status(raw, state.queue_level, admin=state.queue_admin)
                body = json.dumps(payload, sort_keys=True).encode()
                etag = f'"{hashlib.sha256(body).hexdigest()[:16]}"'
                sent = self.headers.get("If-None-Match")
                has_auth = bool(self.headers.get("Authorization"))
                state.status_auth.append(has_auth)
                failed = has_auth and state.queue_auth_failed
                if sent == etag:
                    state.status_polls.append((304, sent))
                    self.send_response(304)
                    self.send_header("ETag", etag)
                    if failed:
                        self.send_header("X-Queue-Auth", "failed")
                    self.end_headers()
                    return
                state.status_polls.append((200, sent))
                self.send_response(200)
                if failed:
                    self.send_header("X-Queue-Auth", "failed")
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("ETag", etag)
                self.end_headers()
                self.wfile.write(body)
                return
            self._send(404, b"not found", "text/plain")

        def do_POST(self) -> None:  # noqa: N802
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            path = self.path.split("?")[0]
            gid = self._bench_id(path)
            if gid is not None:
                if not state.bench_supported:
                    self._no_bench()
                    return
                try:
                    body = json.loads(raw.decode() or "{}")
                except ValueError:
                    body = {}
                obj, status = state.bench_post(gid, body)
                self._json(obj, status)
                return
            if path == "/_admin/api/ocr/generations/derive":
                try:
                    body = json.loads(raw.decode() or "{}")
                except ValueError:
                    body = {}
                spec = body.get("spec") or {}
                state.derived.append(deepcopy(spec))
                state.derive_processors.append(body.get("processor"))
                self._json(derive_body(state, spec, body.get("processor")))
                return
            if path == "/_admin/api/users":
                try:
                    body = json.loads(raw.decode() or "{}")
                except ValueError:
                    body = {}
                state.user_posts.append(body)
                self._json({"success": True, "user": body}, 201)
                return
            if path == "/_admin/api/ocr/devices/refresh":
                self._json({"success": True, "devices": state.catalog["devices"]})
                return
            self._send(404, b"not found", "text/plain")

        def do_DELETE(self) -> None:  # noqa: N802
            path = self.path.split("?")[0]
            gid = self._bench_id(path)
            if gid is not None:
                state.bench_delete_machines.append((gid, self._machine()))
                if not state.bench_supported:
                    self._no_bench()
                    return
                self._json(state.bench_delete(gid, self._machine()))
                return
            self._send(404, b"not found", "text/plain")

        def do_PUT(self) -> None:  # noqa: N802
            length = int(self.headers.get("Content-Length") or 0)
            raw = self.rfile.read(length) if length else b"{}"
            path = self.path.split("?")[0]
            if path == "/_admin/api/ocr/generations":
                state.last_put = json.loads(raw.decode())
                if state.put_response is not None:
                    self._json(state.put_response, state.put_status)
                    return
                # Success echoes the saved list back in the GET shape, with
                # server-minted ids for rows that arrived without one.
                saved = []
                for i, row in enumerate(state.last_put.get("generations", [])):
                    before = next((g for g in state.generations if g.get("id") == row.get("id")), {})
                    merged = deepcopy(before)
                    merged.update(row)
                    merged.setdefault("id", f"g-new-{i + 1}")
                    name = merged.get("name", "")
                    merged["sidecar"] = "<Volume>.mokuro" if merged.get("primary") else f"<Volume>.{name}.mokuro"
                    saved.append(merged)
                state.generations = saved
                body = state.generations_body()
                body.update({"success": True, "applied": True, "installing": False,
                             "restart_required": False, "reason": ""})
                self._json(body)
                return
            if path == "/_admin/api/settings/ocr":
                self._json({"success": True, "applied": True, "installing": False,
                            "restart_required": False, "reason": ""})
                return
            if path == "/_admin/api/settings/queue":
                # Applied live, like the server: the queue page's next poll
                # is shaped at the new level.
                body = json.loads(raw.decode() or "{}")
                state.settings["queue"].update(body)
                if body.get("display"):
                    state.queue_level = body["display"]
                self._json({"success": True, "queue": state.settings["queue"]})
                return
            pools = _POOLS_PATH.match(path)
            if pools is not None:
                body = json.loads(raw.decode() or "{}")
                state.pools_puts.append((unquote(pools.group(1)), body))
                self._json({"success": True, "pools": body.get("pools") or {}})
                return
            role = _ROLE_PATH.match(path)
            if role is not None:
                body = json.loads(raw.decode() or "{}")
                state.role_puts.append((unquote(role.group(1)), body))
                self._json({"success": True, "user": {"username": role.group(1), **body}})
                return
            self._send(404, b"not found", "text/plain")

    return Handler


class StubServer:
    def __init__(self, state: StubState | None = None) -> None:
        self.state = state or StubState()
        self._server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(self.state))
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    @property
    def url(self) -> str:
        host, port = self._server.server_address[:2]
        return f"http://{host}:{port}"

    def __enter__(self) -> StubServer:
        self._thread.start()
        return self

    def __exit__(self, *exc: Any) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)
