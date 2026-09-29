"""OCR engine and detector registries.

Pure data: which engines and detectors exist, what each needs installed, and
what it can be asked for. Nothing here imports torch or any engine package,
so it is safe to import from the server process.

**No engine owns a file name.** A sidecar's name comes from the GENERATION
that produced it (``ocr/generations.py``), because two generations may run
the same engine with different detectors and must write different files.
"""

from __future__ import annotations

from dataclasses import dataclass

MOKURO_ENGINE = "mokuro"

# ``engine_runner.ROAD_SERVED``, spelled here rather than imported: this
# module is pure data and the runner is a file the server copies about. A unit
# test holds the two together.
SERVED_ROAD = "served"
# ``python -m`` this, in the mokuro environment, to get the fork's serve mode.
MOKURO_SERVE_MODULE = "mokuro.serve"


@dataclass(frozen=True)
class EngineSpec:
    """Static description of one OCR engine."""

    id: str
    label: str
    # Hugging Face repo of the recognizer (informational, written into output).
    recognizer: str
    # True when the engine lives in the mokuro venv rather than the engines one.
    uses_mokuro_env: bool = False
    # The road this engine takes through the runner, when the engine FIXES it
    # rather than the engine/detector pair deciding (``engine_runner.
    # page_road``). Only the served engines do: they are a process of their
    # own whatever detector a row names. None means "ask the pair".
    road: str | None = None
    # The module of that process: the runner spawns ``<mokuro python> -m
    # <module>`` once a session and streams pages through it. None means the
    # engine is not served -- and for a mokuro-env engine that is the old
    # one-volume-one-invocation CLI, which is also the fallback when the
    # installed package turns out not to have the module (see
    # ``OCRProcessor.serves_pages``).
    serve_module: str | None = None
    # Detector built into the engine. Such an engine reads the page with its
    # own detector whatever a generation's ``detector`` says (the two models were trained
    # as a pair, and its layout step needs that detector's line quads); the
    # detector's extra packages are what the engine needs installed.
    detector: str | None = None
    # True when the recognizer reads each crop at a chosen patch budget, so
    # a generation's ``patch_budget`` applies to it (see PATCH_BUDGETS). Only
    # the SigLIP2-NaFlex recognizers expose such a knob.
    patch_budget: bool = False
    # Why this recognizer can only run on the CPU, in one sentence, or "" when
    # it can go wherever the row puts it (``pools.stage_device``). The sentence
    # is what the UI shows beside a locked Device select, so it says WHY.
    cpu_only_reason: str = ""

    @property
    def cpu_only(self) -> bool:
        """True when a generation may not put this recognizer on a card."""
        return bool(self.cpu_only_reason)


ENGINES: dict[str, EngineSpec] = {
    # SERVED, not monolithic: the fork's ``python -m mokuro.serve`` holds one
    # model open for a whole session and takes pages one at a time, so a
    # mokuro row joins the runner on a road of its own instead of paying a
    # process start, a model load and a full extraction a volume. A package
    # without that module (a ``MOKURO_BUNKO_MOKURO_SPEC`` override) falls back
    # to the one-volume CLI, which is the only thing left on that path.
    MOKURO_ENGINE: EngineSpec(
        id=MOKURO_ENGINE,
        label="mokuro (manga-ocr)",
        recognizer="kha-white/manga-ocr-base",
        uses_mokuro_env=True,
        road=SERVED_ROAD,
        serve_module=MOKURO_SERVE_MODULE,
    ),
    # hayai-ocr v2.5 "Nova": a ~150M SigLIP2-NaFlex vision tower and a
    # 12-layer decoder, read straight through transformers
    # (``trust_remote_code``) rather than through the ``hayai_ocr`` PyPI
    # package, which pins the v2 repo and hardcodes ``max_num_patches=256``.
    # It is the ONLY hayai engine: v2 was withdrawn once
    # Nova at 512 patches was judged better than it on the hard pages --
    # display lettering, sound effects, art slices -- that decide whether a
    # second engine is worth running at all.
    #
    # DO NOT rename this id now that v2 is gone: a generation named after it
    # (the default) writes ``Volume.hayai-nova.mokuro``, which readers have
    # already imported as a layer.
    "hayai-nova": EngineSpec(
        id="hayai-nova",
        label="hayai-ocr v2.5 Nova",
        recognizer="JustANormalTinkerer/hayai-ocr-v2.5-nova",
        patch_budget=True,
    ),
    "paddle-manga": EngineSpec(
        id="paddle-manga",
        label="PaddleOCR-VL 1.6 manga LoRA",
        recognizer="sorryhyun/paddleocr-vl-1.6-manga-lora",
    ),
    # Detector AND recognizer in one: a PP-OCRv6 DBNet + SVTR-CTC pair
    # fine-tuned on manga (Apache-2.0, 23 MB, onnxruntime on the CPU). Lines
    # come back as rotated quads, ruby as lines of its own; line_layout.py
    # removes the ruby and groups the rest into bubbles and paragraphs. The
    # one engine here that reads scanned novel pages.
    "ppocr-manga": EngineSpec(
        id="ppocr-manga",
        label="PP-OCRv6 manga (CTC, CPU)",
        recognizer="Kellenok/PP-OCRv6_manga",
        detector="ppocr-manga",
        cpu_only_reason="PP-OCRv6's CTC recognizer runs on the CPU (onnxruntime)",
    ),
}

ENGINE_IDS: tuple[str, ...] = tuple(ENGINES)


@dataclass(frozen=True)
class DetectorSpec:
    """Static description of one text detector (used by the non-mokuro engines)."""

    id: str
    label: str
    license: str
    # Standalone adapter script shipped in mokuro_bunko/ocr/detectors/.
    script: str
    # Extra pip packages the engines environment needs for this detector.
    extra_packages: tuple[str, ...] = ()
    # Import that proves the extras are installed (None: nothing extra).
    probe_import: str | None = None
    # True when the detector emits per-line polygons (else one line per block).
    line_level: bool = False
    # Runs the detector adapter in a separate process from the recognizer.
    # Always true today; kept explicit because it is the license boundary.
    isolated: bool = True
    # Why this detector can only run on the CPU, in one sentence, or "" when a
    # generation may put it on a card (``pools.stage_device``).
    cpu_only_reason: str = ""

    @property
    def cpu_only(self) -> bool:
        """True when a generation may not put this detector on a card."""
        return bool(self.cpu_only_reason)


DETECTORS: dict[str, DetectorSpec] = {
    # The detector half of the ``ppocr-manga`` engine, usable on its own: the
    # only detector here that returns rotated LINE quads (and ruby as lines of
    # its own). With it the runner reads the page in-process, lets the
    # configured engine re-read every text line and merges the two reads
    # (line_reconcile.py) before line_layout.py groups them.
    "ppocr-manga": DetectorSpec(
        id="ppocr-manga",
        label="PP-OCRv6 manga line detector (Kellenok)",
        license="Apache-2.0",
        script="ppocr_manga.py",
        extra_packages=("onnxruntime",),
        probe_import="onnxruntime",
        line_level=True,
        cpu_only_reason="the PP-OCRv6 detector runs on the CPU (onnxruntime)",
    ),
    "ctd": DetectorSpec(
        id="ctd",
        label="comic-text-detector (via mokuro)",
        license="GPL-3.0",
        script="ctd.py",
        extra_packages=("mokuro",),
        probe_import="comic_text_detector",
        line_level=True,
    ),
    "animetext": DetectorSpec(
        id="animetext",
        label="AnimeText YOLO12-x (deepghs)",
        license="GPL-3.0",
        script="animetext.py",
        extra_packages=("onnxruntime",),
        probe_import="onnxruntime",
    ),
}

DETECTOR_IDS: tuple[str, ...] = tuple(DETECTORS)
DEFAULT_DETECTOR = "ppocr-manga"

# Detectors kept in the tree but taken out of service, each with the sentence
# a row naming it is refused with. Out of service means: offered by no
# catalog (the admin panel's, a processor's), accepted by no ``--detector``
# flag, and refused in a generation row wherever one is parsed -- the config
# file, the admin panel, a benchmark spec, a row a library sends a processor.
# The spec above, the adapter script and the runner's support all stay, so
# bringing one back is deleting its entry here.
DISABLED_DETECTORS: dict[str, str] = {
    "animetext": (
        "detector 'animetext' is disabled for now (its output in use was poor, "
        "and it is parked until that is understood)"
    ),
}

# What a row, a catalog or a command line may name.
OFFERED_DETECTOR_IDS: tuple[str, ...] = tuple(
    detector for detector in DETECTOR_IDS if detector not in DISABLED_DETECTORS
)

# ``max_num_patches`` a SigLIP2-NaFlex recognizer may read a crop at, and the
# main quality/cost dial of the ``hayai-nova`` engine. NaFlex fits the crop to
# the 16x16 patch grid that packs closest to the budget (keeping its aspect
# ratio) and then zero-pads to exactly the budget, so the budget IS the
# resolution a line is read at: on a long vertical column it buys 4, 5 or 6
# patch rows across the stroke.
#
# Measured (RX 9070 XT, 3386 real line crops, shipped batch of 16):
#
#   budget | peak VRAM | ms/crop | rendered size of a 64x1079 px column
#     256  |   786 MiB |     7.2 | 1024 x 64 px  (4 patch rows)
#     384  |   841 MiB |     7.8 | 1216 x 80 px  (5 patch rows)
#     512  |   897 MiB |     9.1 | 1360 x 96 px  (6 patch rows)
#
# i.e. the WHOLE span is 111 MiB (~12% of the process footprint, and it
# cannot OOM a card that can load the 600 MiB of weights) and +27% of the
# recognizer's time. End to end, once the detector pass that has to run
# anyway is counted, that is +8% of a MANGA page (0.354 -> 0.383 s) and +9%
# of a PROSE page (0.981 -> 1.074 s) -- a novel page carries more lines, so
# more of it is recognizer time. Cost is linear in the budget and independent
# of crop shape (pixel_values is padded to the budget either way).
#
# DEFAULT 512, NOT the model card's own 384, and 384 must never become the
# default: on the card's JMangaBench_Mixed table v2.5 at 384 is a REGRESSION
# on hayai v2.1 (crop CER 3.65% vs 3.23%, EM 79.15% vs 79.67%) and only 512
# beats it (3.10% / 80.68%). Dense vertical lettering and scanned novel
# columns are exactly the case the card reserves 512 for ("dense slices drop
# CER from 7.43% to 2.70%"), and 111 MiB is not a price worth trading real
# accuracy on long columns for.
PATCH_BUDGETS: tuple[int, ...] = (256, 384, 512)
DEFAULT_PATCH_BUDGET = 512


def get_patch_budget(patch_budget: object) -> int:
    """Validate a patch budget, raising ValueError when it is not selectable."""
    try:
        value = int(str(patch_budget).strip())
    except (TypeError, ValueError):
        raise ValueError(f"Invalid OCR patch budget {patch_budget!r}") from None
    if value not in PATCH_BUDGETS:
        known = ", ".join(str(budget) for budget in PATCH_BUDGETS)
        raise ValueError(f"Unknown OCR patch budget '{patch_budget}' (known: {known})")
    return value


def uses_patch_budget(engine_id: str) -> bool:
    """True when a generation's ``patch_budget`` reaches this recognizer."""
    return get_engine(engine_id).patch_budget


def get_detector(detector_id: str) -> DetectorSpec:
    """Return the spec for a detector id, raising ValueError when unknown."""
    try:
        return DETECTORS[detector_id]
    except KeyError:
        known = ", ".join(OFFERED_DETECTOR_IDS)
        raise ValueError(f"Unknown OCR detector '{detector_id}' (known: {known})") from None


def get_engine(engine_id: str) -> EngineSpec:
    """Return the spec for an engine id, raising ValueError when unknown."""
    try:
        return ENGINES[engine_id]
    except KeyError:
        known = ", ".join(ENGINE_IDS)
        raise ValueError(f"Unknown OCR engine '{engine_id}' (known: {known})") from None


# Backend names (``OCRBackend`` values) under which torch runs on a GPU.
GPU_BACKENDS: frozenset[str] = frozenset({"cuda", "rocm", "mps"})


def backend_is_gpu(backend: str | None) -> bool | None:
    """Whether a backend name means a GPU; None when the name does not say.

    ``auto``, ``skip`` and ``unknown`` name no device: what the engines run
    on is then for the caller to find out.
    """
    if backend in GPU_BACKENDS:
        return True
    if backend == "cpu":
        return False
    return None


@dataclass(frozen=True)
class GpuUse:
    """Which OCR environments run their torch on a GPU.

    Per environment, because the two are installed separately and either
    install can fall back to a CPU torch on its own: mokuro on a GPU next to
    a CPU hayai is a real host. Built from the backend each environment
    REPORTS, never from the one that was merely selected
    (``OcrControl.resolve_gpu``). Reported by the admin panel and warned
    about at startup; it decides nothing about the queue, whose order is the
    generations list.
    """

    mokuro_env: bool = False
    engines_env: bool = False


def uses_mokuro_env(engine_id: str) -> bool:
    """True for engines whose packages live in the mokuro environment."""
    return get_engine(engine_id).uses_mokuro_env


def serve_module(engine_id: str) -> str | None:
    """The module of this engine's serve process, or None if it has none."""
    return get_engine(engine_id).serve_module
