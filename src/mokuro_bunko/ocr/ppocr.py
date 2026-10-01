# This Source Code Form is subject to the terms of the Mozilla Public License,
# v. 2.0. If a copy of the MPL was not distributed with this file, You can
# obtain one at https://mozilla.org/MPL/2.0/.
"""PP-OCRv6 manga models: line detection and recognition on onnxruntime.

Why this module exists
----------------------
``Kellenok/PP-OCRv6_manga`` (Apache-2.0) is a PP-OCRv6 *tiny* DBNet detector
and a *small* SVTR-LCNet CTC recognizer fine-tuned on manga. Together they are
23 MB, read a manga page in well under a second on a CPU, and -- unlike every
other detector this project ships -- return **line-level, truly rotated**
rectangles: a slanted shout is a slanted quad, not the axis-aligned box around
it. Good line quads are what text placement in the reader depends on;
characters are then laid out on a uniform grid inside each quad, which is
right because Japanese lettering is fixed-pitch.

The same two models have to read two very different kinds of page:

* manga pages -- a few dozen short lines, large type; and
* scanned novel pages -- 15-20 columns of ~40 characters with ruby, glyphs
  about 60 px at 1925x2800, columns separated by thin gutters.

That is why the detector picks its working scale from the page (see
:func:`needs_fine_pass`), why crops are never padded into batches (see
``REC_MAX_PAD_WASTE``) and why only lines longer than a full novel column are
read in overlapping windows (see :func:`window_spans` / :func:`stitch_windows`).

Conventions
-----------
* Images are BGR ``uint8`` arrays (OpenCV order), like the models were trained.
* A **quad** is ``float32[4, 2]`` in page pixels, ordered as the corners of the
  line in its own upright reading frame: top-left, top-right, bottom-right,
  bottom-left. For a tilted line the quad is tilted; it is never the
  axis-aligned bounding box. :func:`order_quad` is the single place that
  decides the order.
* ``angle`` is the tilt of that frame in degrees, ``(-45, 45]``, positive =
  clockwise on screen (y grows downward) -- the sign CSS ``rotate()`` uses.
* ``vertical`` means the reading axis is the quad's top-to-bottom edge.

Raw page JSON (:func:`page_to_json`, also what the CLI writes and what the
geometry tests use as fixtures)::

    {"format": "ppocr-lines/1", "width": 1925, "height": 2800,
     "detector": {"side": 1120, "passes": [...], ...},
     "lines": [{"quad": [[x, y] * 4], "score": 0.91, "text": "...",
                "conf": 0.97, "char_confs": [...], "vertical": true,
                "angle": -0.4}]}

Standalone on purpose (stdlib + numpy; OpenCV, onnxruntime and
huggingface_hub imported lazily): the OCR processor copies it next to
``engine_runner.py`` and the detector adapters, which import it by path from
the engines environment where ``mokuro_bunko`` is not installed. The server
process can import it without any of those packages.

CLI::

    python -m mokuro_bunko.ocr.ppocr --image page.webp --out lines.json \
        [--side 1120] [--tile auto|off|force] [--models DIR] [--no-recognize]
"""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
import time
from collections.abc import Iterable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, cast

try:  # numpy lives in the engines environment, not necessarily in the server's
    import numpy as np
except ImportError:  # pragma: no cover - exercised only in a bare server install
    np = cast(Any, None)

REPO_ID = "Kellenok/PP-OCRv6_manga"
# Pinned so a silent upstream re-upload cannot change OCR output under us.
# v0.2 (2026-09-28): a retrained detector and recognizer, drop-in for v0.1
# (same architectures, sizes, dictionary and speed).
REPO_REVISION = "ba1d479e8a61a20e8318c9758c73fbbbd290b98d"
MODELS_ENV = "MOKURO_PPOCR_MODELS"
DOWNLOAD_ENV = "MOKURO_PPOCR_DOWNLOAD"
PRECISION_ENV = "MOKURO_PPOCR_PRECISION"
THREADS_ENV = "MOKURO_PPOCR_THREADS"

DICT_FILE = "ppocrv6_dict.txt"
MODEL_FILES: dict[str, tuple[str, str]] = {
    # precision -> (detector, recognizer), paths as laid out in the HF repo.
    "fp32": ("det/manga_det_v0.2.onnx", "rec/manga_rec_v0.2.onnx"),
    "fp16": ("det/manga_det_v0.2_fp16.onnx", "rec/manga_rec_v0.2_fp16.onnx"),
}
# Measured on v0.1 with onnxruntime on CPU (4 threads): the FP16 files take float32
# input and are cast back per op, so they are SLOWER than FP32 (0.34 vs 0.30 s
# per page over 6 manga + 5 novel pages; detector alone 53 vs 42 ms) and read
# 19 of 322 lines differently. FP16 only buys a smaller download (11 vs 23 MB).
DEFAULT_PRECISION = "fp32"

FORMAT_ID = "ppocr-lines/1"

# --- detector ---------------------------------------------------------------
# v0.2's best detector input on the authors' benchmark (README Table 1:
# end-to-end CER 10.89% at 1120 vs 11.34% at 960, the demo app's size), and
# on the owner's own run of v0.2: 1120 beat 960, which beat 1280. On v0.1,
# 960 lost small lines on 2800-px scans and this was 1280.
DEFAULT_SIDE = 1120
SIDE_MULTIPLE = 32
# Small web pages gain from some enlargement, but past 1.5x the detector only
# sees interpolation blur and starts boxing screentone.
MAX_UPSCALE = 1.5
IMAGENET_MEAN = (0.485, 0.456, 0.406)
IMAGENET_STD = (0.229, 0.224, 0.225)
DB_THRESH = 0.15
DB_BOX_THRESH = 0.25
# What the authors run every v0.2 benchmark with. On v0.1 it clipped an end
# glyph on ~3% of our bench's lines and this was 1.5; `recover_clipped_ends`
# now gives such a line its last bracket or stop back.
DB_UNCLIP_RATIO = 1.4
DB_MIN_SIDE = 3.0
DB_MAX_CANDIDATES = 3000

# Dense-page policy (see needs_fine_pass). Thickness is a line's short side in
# DETECTOR pixels, taken over the page's LONG lines only (length >= 10x
# thickness: prose columns). Measured on 1925x2800 novel scans: at ~30 px
# (side 1280) every column and ruby run is found; at 22 px a two-page spread
# loses 5 of 80 lines; at <= 19 px the ruby disappears and columns fragment.
# Manga bubbles are 17-24 px at side 1280 and read fine there -- a finer pass
# only adds empty boxes on screentone -- but they have almost no lines that
# long, which is what keeps them on the single fast pass.
FINE_THICKNESS_PX = 24.0
DENSE_LENGTH_RATIO = 10.0
FINE_MIN_LINES = 6
# A long box this many times thicker than the median is probably two columns
# fused into one. Not seen with this detector so far (it fragments before it
# fuses), but it is the failure DBNet is known for and the check is free.
FUSED_THICKNESS_RATIO = 1.7
FUSED_MIN_COUNT = 2
# The fine pass aims for this glyph thickness in detector pixels (what side
# 1280 gives a single novel page). Not native scale: at 64 px per glyph the
# detector starts splitting columns at sentence ends and clips trailing
# punctuation (4 of 26 column ends on the test page, none at 30 px)...
FINE_TARGET_THICKNESS_PX = 32.0
# ...but never enlarges the page, and never feeds the network more than this
# many pixels at once: beyond it the page is tiled.
MAX_DETECTOR_PIXELS = 2816 * 2048
TILE_SIZE = 1536
TILE_OVERLAP = 256
# How close (detector px) to a tile's inner edge a box must end to count as
# cut by it; see clipped_by_tile.
TILE_EDGE_MARGIN = 8.0
# Pages this elongated (webtoon strips) are tiled from the start: squeezing
# their long side into DEFAULT_SIDE would leave nothing to read.
STRIP_ASPECT = 2.5

# --- recognizer -------------------------------------------------------------
REC_HEIGHT = 48
# The CTC head emits one timestep per 8 px of input width (T = W // 8).
REC_STRIDE = 8
REC_MIN_WIDTH = 16
REC_MAX_BATCH = 16
# Share of a batch that may be padding. ZERO by default -- only crops of the
# same input width share a batch -- because padding changes what the model
# reads: its SVTR neck attends across the whole width. Measured on 177 lines
# (2 novel + 3 manga pages) against one-crop-at-a-time decoding: 0 texts
# changed at 0.0, 6 at 0.10, 10 at 0.25 (zero padding; edge/white/median
# padding changed 42-55), and on CPU the padded batches were not even faster
# (0.68 s vs 0.79 s). A line's text must not depend on its page neighbours.
REC_MAX_PAD_WASTE = 0.0
# Very long lines are read in overlapping windows (px at height 48, multiples
# of REC_STRIDE). This is a bound on memory and latency, NOT an accuracy fix:
# the network takes any width and holds up well beyond what it was trained on.
# Measured on novel columns (~40 glyphs, ~1950 px): whole-line vs 960-px
# windows disagree on 17 of 157 columns, always on a weak glyph, and the whole
# line is right more often (it keeps a leading "「" or a "――" that a window
# loses; the windows' one win is き/キ). Joining 2-3 columns into one
# 3900-5850 px crop changes 0.3-0.5% of characters against the columns read
# alone, the same for whole-line, 1600- and 2000-px windows (960: slightly
# worse). So a full column is read whole, and only a line longer than
# REC_WINDOW + REC_WINDOW_OVERLAP is split. The 192-px overlap (4 glyphs)
# lets choose_cut find a gap both windows agree on.
# (An earlier run that seemed to show whole lines dropping their final "。"
# was really showing batch padding doing it -- see REC_MAX_PAD_WASTE.)
REC_WINDOW = 2000
REC_WINDOW_OVERLAP = 192
# Timesteps at each window edge never used for stitching: a glyph cut by the
# window border is read unreliably there.
REC_WINDOW_GUARD = 3

# --- glyphs the decoder skipped -------------------------------------------------
# The reader lays a line's characters out on a UNIFORM grid inside its quad, so
# a cell the decoder skipped shifts every later character off its glyph. Two
# things get skipped: the full-width space bunko setting puts after "！"/"？"
# (the model emits its space class for about one in ten), and a glyph the
# recognizer cannot name -- a kanji missing from its dictionary ("谺") or too
# rare for it ("訝") decodes as blank, silently. Both leave a hole of one cell
# between two CTC peaks, and the crop says which it was. Bench (12 Kingdoms,
# 40 pages, every hole of >= 1.5 pitches in a line of regular pitch): share of
# ink in the hole 0.000-0.015 for 24 spaces, 0.029-0.049 for the second half
# of 6 two-cell dashes, 0.23-0.40 for 5 dropped kanji; nothing in between.
# Lines of irregular pitch are left alone: ruby runs in generous boxes and
# Latin text have holes that mean nothing (pitch 2-4 timesteps, against 6-7
# for a full-size glyph at height 48).
GAP_MIN_GLYPHS = 6
GAP_MIN_PITCH = 5.0
GAP_MIN_RATIO = 1.5
GAP_MAX_FILL = 3
GAP_BLANK_MAX_INK = 0.02
GAP_DASH_MAX_INK = 0.12
GAP_GLYPH_MIN_INK = 0.2
# A pixel is ink when it is this far from the crop's median (= paper) in the
# recognizer's normalised units (-1..1): works for white-on-black lettering too.
GAP_INK_CONTRAST = 0.5
IDEOGRAPHIC_SPACE = "\u3000"
# The "geta" mark: what Japanese typesetting prints for a glyph it does not
# have. It keeps the grid honest and tells the reader a character is missing,
# which a silently shorter line never did.
MISSING_GLYPH = "〓"
DASHES = "―—ー"
# A Latin letter the recognizer itself doubted, wedged between kana or kanji,
# is a rare kanji it could not name (bench: "嚙む" -> "ｗむ" at 0.14, seven
# times in one novel). The same geta mark replaces it.
FOREIGN_MAX_CONF = 0.5

# --- second opinions on doubted characters -------------------------------------
# The recognizer is brittle on rare kanji -- exactly the words a learner looks
# up (bench: 膝 -> 滕, 睨 -> 脱, 頷 -> 鎮, 猶予 -> 猫予; 2-4 per dense page) --
# and its own per-character confidence knows: 9 of 13 checked misreads sat
# under 0.75, against 1% of all body characters under 0.8. A slightly WIDER
# crop often reads the glyph right, but is no better overall: read every line
# wider and as many new errors appear (鞘 -> 覇, 鋭 -> 锐) as old ones go. So
# only a doubted character is put to a vote, between the line as read and two
# wider crops, and it changes only when BOTH wider reads name the same other
# KANJI for a kanji, at least as confidently (on average) as the line's own
# read named its. Whole novel (292 pages, 15% of lines re-read, +0.1 s/page),
# judged against the stock sidecar's text: 41 kanji fixed (鎮 -> 頷, 脱 -> 睨,
# 者 -> 老, 猫予 -> 猶予, 間 -> 問), about 5 broken (咎め -> 答め, 畦 -> 睦),
# 19 undecidable and mostly right by eye (達姫 -> 達姐, 羽博 -> 羽搏).
# Rules that were measured and dropped: either wider read alone breaks as
# many as it fixes (抜 -> 拔, 湧 -> 涌, 詰 -> 話); without the confidence
# condition 膝 -- read right but doubted -- turned into 滕 five times; and
# votes on kana or punctuation mostly swap widths ("？" -> "?", キ -> き).
VOTE_MAX_CONF = 0.8
VOTE_MIN_GLYPHS = 4
# Extra crop on each side, across the line, as a share of its thickness.
VOTE_WIDEN = (0.06, 0.12)

# --- clipped line ends --------------------------------------------------------
# The detector boxes INK, and the first or last glyph of a line is often a
# mark with almost none: a vertical "「" is one thin corner, a "。" a small
# ring, and either sits half a cell away from its neighbour. Bench (12 novel
# pages, 190 columns): 5 of 18 opening brackets and the final "、"/"。" of 6
# columns ended up outside the box, so the text lost them. Reading every line
# with a padded crop is not the fix -- the same bench: a 1 em pad changed 35%
# of all lines, because on manga the pad holds the bubble's outline (read as
# "一") and on ruby the neighbouring column. Instead each end is PROBED: a
# short crop reaching one em beyond the box is read on its own, and the extra
# glyph is accepted only if it is one of the marks below AND the rest of the
# probe repeats the line's own text. Anything else changes nothing.
PROBE_PAD_EM = 1.0
PROBE_INSIDE_EM = 2.5
PROBE_MIN_GLYPHS = 2
PROBE_OPENERS = "「『（〈《【〔"
PROBE_CLOSERS = "、。」』）〉》】〕！？"
# Glyphs of one thin horizontal stroke have even less ink than a bracket, and
# the detector starts the box below them (bench: "一瞬で決着..." read as "瞬で
# 決着...", "一方は狒狒..." as "方は狒狒...", "――あれが来たら" as "―あれが..."). The
# line then also LOOKS indented and opens a false paragraph. They are probed
# for only where the caller says so -- columns of a novel's text body -- never
# on manga, where the same probe reads a bubble's outline as "一".
PROBE_THIN_OPENERS = "一―"
# An accepted mark grows the quad by this share of the line's glyph pitch:
# the detector, when it does catch such a mark, boxes its ink, which fills
# about half of the cell on the line's side.
PROBE_GROW_PITCH = 0.5
# ...and a thin stroke sits in the MIDDLE of its cell, so it takes the whole.
PROBE_GROW_PITCH_THIN = 1.0
# A joined column (see PPOcr.join_lines) replaces its pieces only when it is
# read at least this confidently and loses no glyph the pieces had.
JOIN_MIN_CONF = 0.6


# ---------------------------------------------------------------------------
# lazy imports
# ---------------------------------------------------------------------------


def _cv2() -> Any:
    import cv2  # noqa: PLC0415

    return cv2


def _require_numpy() -> None:
    if np is None:
        raise RuntimeError("mokuro_bunko.ocr.ppocr needs numpy (OCR engines environment)")


# ---------------------------------------------------------------------------
# model resolution
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class ModelPaths:
    """Where the three model files live on disk."""

    detector: Path
    recognizer: Path
    dictionary: Path
    precision: str
    # True when every file came from the pinned download at ``REPO_REVISION``.
    # False when any of them was found in a configured models directory, which
    # holds whatever was copied into it: the sidecar must not then claim the
    # commit (see ``engine_runner.PPOcrPageReader``).
    pinned: bool = True


def _env_flag(name: str, default: bool) -> bool:
    raw = os.environ.get(name)
    if raw is None or not raw.strip():
        return default
    return raw.strip().lower() not in ("0", "false", "no", "off")


def _find_local(models_dir: Path, rel: str) -> Path | None:
    """A model file in ``models_dir``: repo layout first, then flat."""
    for candidate in (models_dir / rel, models_dir / Path(rel).name):
        if candidate.is_file():
            return candidate
    return None


def resolve_models(
    models_dir: str | os.PathLike[str] | None = None,
    *,
    precision: str | None = None,
    download: bool | None = None,
) -> ModelPaths:
    """Locate the detector, recognizer and dictionary, downloading if allowed.

    ``models_dir`` (argument, else ``$MOKURO_PPOCR_MODELS``) is searched in the
    Hugging Face repo layout (``det/…``, ``rec/…``) and flat. Missing files are
    fetched from ``Kellenok/PP-OCRv6_manga`` into that directory -- or into
    the shared Hugging Face cache when no directory is configured -- unless
    ``download`` is False (argument, else ``$MOKURO_PPOCR_DOWNLOAD=0``). An
    offline server therefore fails with a message naming the files to copy
    instead of hanging on a network call.
    """
    precision = (precision or os.environ.get(PRECISION_ENV) or DEFAULT_PRECISION).lower()
    if precision not in MODEL_FILES:
        raise ValueError(f"unknown precision '{precision}' (known: {', '.join(MODEL_FILES)})")
    if download is None:
        download = _env_flag(DOWNLOAD_ENV, True)
    raw_dir = models_dir if models_dir is not None else os.environ.get(MODELS_ENV)
    base = Path(raw_dir).expanduser() if raw_dir else None

    wanted = (*MODEL_FILES[precision], DICT_FILE)
    found: list[Path] = []
    pinned = True
    for rel in wanted:
        local = _find_local(base, rel) if base is not None else None
        if local is None:
            if not download:
                where = base if base is not None else f"${MODELS_ENV} (unset)"
                raise FileNotFoundError(
                    f"PP-OCR manga model file '{rel}' not found in {where} and downloading "
                    f"is disabled; copy it from https://huggingface.co/{REPO_ID}"
                )
            local = _download(rel, base)
        else:
            # Whatever is in the directory, which nothing here can attribute
            # to a commit.
            pinned = False
        found.append(local)
    return ModelPaths(found[0], found[1], found[2], precision, pinned)


def _download(rel: str, base: Path | None) -> Path:
    from huggingface_hub import hf_hub_download  # noqa: PLC0415

    kwargs: dict[str, Any] = {"repo_id": REPO_ID, "filename": rel, "revision": REPO_REVISION}
    if base is not None:
        base.mkdir(parents=True, exist_ok=True)
        kwargs["local_dir"] = str(base)
    return Path(hf_hub_download(**kwargs))


def load_vocab(dictionary: str | os.PathLike[str]) -> list[str]:
    """CTC classes: index 0 is the blank, the last one a space.

    The file has one symbol per line; symbols can be whitespace-like, so only
    the line terminator is stripped (PaddleOCR ``use_space_char`` layout).
    """
    text = Path(dictionary).read_text(encoding="utf-8")
    symbols = [line.rstrip("\r") for line in text.split("\n")]
    if symbols and symbols[-1] == "":
        symbols.pop()
    return ["", *symbols, " "]


# ---------------------------------------------------------------------------
# quad geometry
# ---------------------------------------------------------------------------


def order_quad(points: Any) -> Any:
    """Order four rectangle corners as the line's upright reading frame.

    Returns top-left, top-right, bottom-right, bottom-left, where "top" is
    the edge a reader sees on top once the line is turned upright by the
    smallest rotation: of the four cyclic orders (clockwise on screen) the
    one whose first edge points most nearly along +x wins. A column leaning
    20 degrees therefore keeps its narrow top edge first, and a line of
    horizontal text leaning 20 degrees keeps its long edge first -- the true
    angle is preserved, never snapped.

    PaddleOCR's own ordering (sort by x, then by y) breaks exactly here: on a
    leaning column it starts from the wrong corner and the crop comes out
    mirrored or rotated.

    KNOWN LIMIT: a rectangle does not say which of its sides is "up", and
    there is no orientation classifier, so text tilted MORE than 45 degrees
    is taken for the other orientation at the complementary angle. Measured
    with synthetic lines through the whole pipeline: everything within +-40
    degrees reads correctly; past 45, horizontal text tilted clockwise and
    vertical text tilted counter-clockwise still read (flagged as the other
    orientation), the other two come out empty or as confident garbage
    ("いい日" at 0.76). On the bench 0 of 11,943 lines with a reliable angle
    tilt more than 30 degrees, so this waits for a real case; the fix is to
    read both frames of a steep line and keep the better, and it needs
    ``line_layout.canonical_quad`` (same rule) to then trust the given order.
    """
    _require_numpy()
    pts = np.asarray(points, dtype=np.float32).reshape(4, 2)
    centre = pts.mean(axis=0)
    # Clockwise on screen (y down) = increasing atan2 of the offset.
    order = np.argsort(np.arctan2(pts[:, 1] - centre[1], pts[:, 0] - centre[0]))
    pts = pts[order]
    best, best_dx = 0, -math.inf
    for start in range(4):
        edge = pts[(start + 1) % 4] - pts[start]
        length = float(np.hypot(edge[0], edge[1]))
        dx = float(edge[0]) / length if length > 0 else -math.inf
        if dx > best_dx + 1e-6:
            best, best_dx = start, dx
    return np.roll(pts, -best, axis=0)


def quad_size(quad: Any) -> tuple[float, float]:
    """(width, height) of an ordered quad in its own frame."""
    q = np.asarray(quad, dtype=np.float32)
    width = (np.linalg.norm(q[1] - q[0]) + np.linalg.norm(q[2] - q[3])) / 2
    height = (np.linalg.norm(q[3] - q[0]) + np.linalg.norm(q[2] - q[1])) / 2
    return float(width), float(height)


def quad_angle(quad: Any) -> float:
    """Tilt of an ordered quad in degrees, positive clockwise on screen."""
    q = np.asarray(quad, dtype=np.float32)
    top = (q[1] - q[0]) + (q[2] - q[3])
    return float(math.degrees(math.atan2(float(top[1]), float(top[0]))))


def quad_is_vertical(quad: Any) -> bool:
    """True when the reading axis is the quad's top-to-bottom edge.

    Same rule the recognizer crop uses (taller than wide), so the flag and the
    crop rotation can never disagree.
    """
    width, height = quad_size(quad)
    return height > width


def quad_thickness(quad: Any) -> float:
    """Short side: the glyph size of the line."""
    return min(quad_size(quad))


def rect_to_quad(centre: Sequence[float], size: Sequence[float], angle_deg: float) -> Any:
    """Corners of a rotated rectangle (``cv2.boxPoints`` without OpenCV)."""
    _require_numpy()
    half_w, half_h = size[0] / 2.0, size[1] / 2.0
    c, s = math.cos(math.radians(angle_deg)), math.sin(math.radians(angle_deg))
    corners = [(-half_w, -half_h), (half_w, -half_h), (half_w, half_h), (-half_w, half_h)]
    return np.array(
        [[centre[0] + x * c - y * s, centre[1] + x * s + y * c] for x, y in corners],
        dtype=np.float32,
    )


def unclip_distance(width: float, height: float, ratio: float) -> float:
    """DB's unclip offset for a rectangle: ``area * ratio / perimeter``."""
    perimeter = 2.0 * (width + height)
    return width * height * ratio / perimeter if perimeter > 0 else 0.0


# ---------------------------------------------------------------------------
# detector pre/post-processing
# ---------------------------------------------------------------------------


def detector_input_size(
    width: int,
    height: int,
    side: int = DEFAULT_SIDE,
    *,
    max_upscale: float = MAX_UPSCALE,
) -> tuple[int, int]:
    """(width, height) the page is resized to: longest side ~``side``, /32."""
    scale = min(side / max(width, height), max_upscale)
    new_w = max(SIDE_MULTIPLE, int(round(width * scale / SIDE_MULTIPLE)) * SIDE_MULTIPLE)
    new_h = max(SIDE_MULTIPLE, int(round(height * scale / SIDE_MULTIPLE)) * SIDE_MULTIPLE)
    return new_w, new_h


def detector_tensor(bgr: Any, size: tuple[int, int]) -> Any:
    """Resize + ImageNet-normalise a BGR image into a ``[1, 3, H, W]`` tensor.

    The mean/std are applied in BGR channel order, without an RGB swap: that
    is what PaddleOCR's pipeline does and what these weights were tuned with.
    """
    cv2 = _cv2()
    resized = cv2.resize(bgr, size, interpolation=cv2.INTER_LINEAR)
    x = resized.astype(np.float32) / 255.0
    x = (x - np.array(IMAGENET_MEAN, np.float32)) / np.array(IMAGENET_STD, np.float32)
    return np.ascontiguousarray(x.transpose(2, 0, 1)[None])


def db_postprocess(
    prob: Any,
    *,
    thresh: float = DB_THRESH,
    box_thresh: float = DB_BOX_THRESH,
    unclip_ratio: float = DB_UNCLIP_RATIO,
    min_side: float = DB_MIN_SIDE,
    max_candidates: int = DB_MAX_CANDIDATES,
) -> list[tuple[Any, float]]:
    """DBNet probability map -> ``[(ordered quad, score)]`` in map pixels.

    Threshold, contours, minimum-area rectangle, score = mean probability
    inside the contour polygon, unclip, drop anything whose short side is
    under ``min_side``.

    The unclip is done in closed form. PaddleOCR offsets the rectangle with
    pyclipper (round joins) by ``d = area * ratio / perimeter`` and takes the
    minimum-area rectangle of the result; for a rectangle that is exactly the
    same rectangle grown by ``d`` on every side, minus pyclipper's integer
    rounding. Doing it directly keeps sub-pixel geometry and spares the
    engines environment two native dependencies (the equivalence is pinned by
    a test that runs when pyclipper is installed).
    """
    cv2 = _cv2()
    prob = np.asarray(prob, dtype=np.float32)
    bitmap = (prob > thresh).astype(np.uint8) * 255
    contours, _ = cv2.findContours(bitmap, cv2.RETR_LIST, cv2.CHAIN_APPROX_SIMPLE)
    out: list[tuple[Any, float]] = []
    for contour in contours[:max_candidates]:
        (cx, cy), (w, h), angle = cv2.minAreaRect(contour)
        if min(w, h) < min_side:
            continue
        score = _contour_score(prob, contour)
        if score < box_thresh:
            continue
        grow = 2.0 * unclip_distance(w, h, unclip_ratio)
        w, h = w + grow, h + grow
        if min(w, h) < min_side + 2:
            continue
        out.append((order_quad(rect_to_quad((cx, cy), (w, h), angle)), float(score)))
    return out


def _contour_score(prob: Any, contour: Any) -> float:
    cv2 = _cv2()
    x0, y0, bw, bh = cv2.boundingRect(contour)
    mask = np.zeros((bh, bw), np.uint8)
    cv2.fillPoly(mask, [(contour.reshape(-1, 2) - [x0, y0]).astype(np.int32)], 1)
    return float(cv2.mean(prob[y0 : y0 + bh, x0 : x0 + bw], mask)[0])


# ---------------------------------------------------------------------------
# dense pages: pass policy and tiling
# ---------------------------------------------------------------------------


def dense_median(thicknesses: Sequence[float], lengths: Sequence[float]) -> float | None:
    """Median thickness of a page's long lines, or None when it has too few.

    Long = at least ``DENSE_LENGTH_RATIO`` glyphs. Ruby, page numbers and
    manga bubbles are short, so they neither dilute the median of a prose
    page nor make a manga page look like one.
    """
    body = sorted(
        t
        for t, length in zip(thicknesses, lengths, strict=True)
        if length >= DENSE_LENGTH_RATIO * t
    )
    if len(body) < FINE_MIN_LINES:
        return None
    return body[len(body) // 2]


def needs_fine_pass(thicknesses: Sequence[float], lengths: Sequence[float]) -> str | None:
    """Reason a first-pass result calls for a finer second pass, or None.

    ``thicknesses`` / ``lengths`` are the short / long sides of the first-pass
    boxes in DETECTOR pixels. A page of prose columns is too dense for the
    working scale when its typical column is thinner than
    ``FINE_THICKNESS_PX``, or when several long boxes are
    ``FUSED_THICKNESS_RATIO`` times thicker than the median (neighbouring
    columns fused into one box). Manga pages trip neither -- they do not have
    enough long lines to be judged at all -- and keep the single fast pass.
    """
    median = dense_median(thicknesses, lengths)
    if median is None:
        return None
    if median < FINE_THICKNESS_PX:
        return f"median column thickness {median:.1f}px < {FINE_THICKNESS_PX:.0f}px"
    fused = sum(
        1
        for t, length in zip(thicknesses, lengths, strict=True)
        if length >= DENSE_LENGTH_RATIO * median and t >= FUSED_THICKNESS_RATIO * median
    )
    if fused >= FUSED_MIN_COUNT:
        return f"{fused} boxes >= {FUSED_THICKNESS_RATIO}x median thickness {median:.1f}px"
    return None


def fine_scale(median_thickness_px: float, first_scale: float) -> float:
    """Page->detector scale of the fine pass.

    Brings the typical glyph to ``FINE_TARGET_THICKNESS_PX`` but never
    enlarges the page: a scan only has the detail it has.
    """
    if median_thickness_px <= 0:
        return 1.0
    return min(1.0, first_scale * FINE_TARGET_THICKNESS_PX / median_thickness_px)


def tile_grid(
    width: int, height: int, tile: int = TILE_SIZE, overlap: int = TILE_OVERLAP
) -> list[tuple[int, int, int, int]]:
    """Overlapping ``(x0, y0, x1, y1)`` tiles covering a ``width x height`` image.

    Tiles are spread evenly so the last one is not a sliver; an axis that
    already fits in one tile is not split.
    """

    def starts(extent: int) -> list[int]:
        if extent <= tile:
            return [0]
        count = math.ceil((extent - overlap) / (tile - overlap))
        step = (extent - tile) / (count - 1)
        return [int(round(i * step)) for i in range(count)]

    return [
        (x, y, min(x + tile, width), min(y + tile, height))
        for y in starts(height)
        for x in starts(width)
    ]


def clipped_by_tile(
    quad: Any,
    tile: tuple[int, int, int, int],
    page_size: tuple[int, int],
    overlap: float,
    margin: float = TILE_EDGE_MARGIN,
) -> bool:
    """True for a box the tile border cut in a way its neighbour tile repairs.

    A box that ends at an INNER edge of its tile (one that is not the page
    border) is incomplete. "At" means within ``margin``: DBNet's probability
    fades a few pixels before the image border, so a cut box stops 3-5 px
    short of it. Two cases:

    * it reaches back from that edge by less than HALF the tile overlap -- a
      column sliced lengthwise, a glyph cut in half, a short line ending at
      the seam. The neighbouring tile sees all of it, well clear of its own
      border, so this copy is dropped; kept, it would survive as a sliver
      beside the real line or drag the merged line sideways;
    * it reaches back further -- a long line crossing the seam. It is kept
      and joined to its other half by :func:`merge_tile_lines`.
    """
    q = np.asarray(quad, dtype=np.float32)
    x0, y0, x1, y1 = tile
    width, height = page_size
    lo_x, lo_y = float(q[:, 0].min()), float(q[:, 1].min())
    hi_x, hi_y = float(q[:, 0].max()), float(q[:, 1].max())
    reach_x, reach_y = hi_x - lo_x, hi_y - lo_y
    touches = (
        (x0 > 0 and lo_x <= x0 + margin and reach_x < overlap / 2)
        or (x1 < width and hi_x >= x1 - margin and reach_x < overlap / 2)
        or (y0 > 0 and lo_y <= y0 + margin and reach_y < overlap / 2)
        or (y1 < height and hi_y >= y1 - margin and reach_y < overlap / 2)
    )
    return bool(touches)


@dataclass
class _Frame:
    """A quad described in its own axes, for the seam-merge comparisons."""

    centre: Any
    axis: Any  # unit vector along the LONG side
    normal: Any
    half_len: float
    half_thick: float


def _frame(quad: Any) -> _Frame:
    q = np.asarray(quad, dtype=np.float32)
    width, height = quad_size(q)
    across = (q[1] - q[0]) + (q[2] - q[3])
    down = (q[3] - q[0]) + (q[2] - q[1])
    axis, normal = (down, across) if height >= width else (across, down)
    axis = axis / (np.linalg.norm(axis) or 1.0)
    normal = normal / (np.linalg.norm(normal) or 1.0)
    return _Frame(q.mean(axis=0), axis, normal, max(width, height) / 2, min(width, height) / 2)


def quad_iou(a: Any, b: Any) -> float:
    """IoU of two convex quads."""
    cv2 = _cv2()
    qa = np.asarray(a, dtype=np.float32)
    qb = np.asarray(b, dtype=np.float32)
    area_a, area_b = cv2.contourArea(qa), cv2.contourArea(qb)
    if area_a <= 0 or area_b <= 0:
        return 0.0
    inter, _ = cv2.intersectConvexConvex(qa, qb)
    union = area_a + area_b - inter
    return float(inter / union) if union > 0 else 0.0


def same_line(a: Any, b: Any, *, iou: float = 0.5) -> bool:
    """True when two quads from different tiles are one printed line.

    Either they are the same box seen twice (IoU), or they are the two halves
    of a line the tile border cut: same direction, same thickness, on one
    axis, and OVERLAPPING along it. The overlap requirement is what keeps two
    separate lines that merely follow each other on one axis (a column above
    another column) apart: every tile that sees the gap between them reports
    two boxes whose extents never overlap.
    """
    if quad_iou(a, b) >= iou:
        return True
    fa, fb = _frame(a), _frame(b)
    if fa.half_len < fb.half_len:
        fa, fb = fb, fa
    thick_a, thick_b = fa.half_thick * 2, fb.half_thick * 2
    if not 0.7 <= thick_a / max(thick_b, 1e-6) <= 1.0 / 0.7:
        return False
    # Squarish boxes have no reliable direction; only the IoU test applies.
    if fb.half_len < 1.5 * fb.half_thick:
        return False
    if abs(float(np.dot(fa.axis, fb.axis))) < math.cos(math.radians(6.0)):
        return False
    offset = fb.centre - fa.centre
    if abs(float(np.dot(offset, fa.normal))) > 0.35 * min(thick_a, thick_b):
        return False
    along = abs(float(np.dot(offset, fa.axis)))
    overlap = fa.half_len + fb.half_len - along
    return overlap > 0.5 * min(thick_a, thick_b)


def union_quad(quads: Sequence[Any]) -> Any:
    """Smallest rectangle, in the longest quad's frame, holding all of them.

    Thickness comes from the quads' own (length-weighted) thickness rather
    than from the union of their corners, so a slight angle difference
    between the halves does not fatten the merged line.
    """
    frames = [_frame(q) for q in quads]
    ref = max(frames, key=lambda f: f.half_len)
    pts = np.concatenate([np.asarray(q, dtype=np.float32) for q in quads]) - ref.centre
    along = pts @ ref.axis
    weights = np.array([f.half_len for f in frames], dtype=np.float32)
    centres = np.array([float(np.dot(f.centre - ref.centre, ref.normal)) for f in frames])
    mid_n = float(np.average(centres, weights=weights))
    half_thick = float(np.average([f.half_thick for f in frames], weights=weights))
    lo, hi = float(along.min()), float(along.max())
    centre = ref.centre + ref.axis * (lo + hi) / 2 + ref.normal * mid_n
    a, n = ref.axis * (hi - lo) / 2, ref.normal * half_thick
    return order_quad([centre - a - n, centre + a - n, centre + a + n, centre - a + n])


def merge_tile_lines(
    quads: Sequence[Any], scores: Sequence[float], tiles: Sequence[int]
) -> list[tuple[Any, float]]:
    """Merge per-tile detections into page lines.

    ``tiles[i]`` is the index of the tile that produced ``quads[i]``; boxes of
    one tile are never merged with each other (the detector already decided
    they are separate). Groups are built with union-find so a line crossing
    several tiles becomes one quad. Score: the length-weighted mean.
    """
    n = len(quads)
    parent = list(range(n))

    def find(i: int) -> int:
        while parent[i] != i:
            parent[i] = parent[parent[i]]
            i = parent[i]
        return i

    bounds = [
        (float(q[:, 0].min()), float(q[:, 1].min()), float(q[:, 0].max()), float(q[:, 1].max()))
        for q in (np.asarray(q, dtype=np.float32) for q in quads)
    ]
    for i in range(n):
        for j in range(i + 1, n):
            if tiles[i] == tiles[j]:
                continue
            bi, bj = bounds[i], bounds[j]
            if bi[2] < bj[0] or bj[2] < bi[0] or bi[3] < bj[1] or bj[3] < bi[1]:
                continue
            if find(i) != find(j) and same_line(quads[i], quads[j]):
                parent[find(j)] = find(i)

    groups: dict[int, list[int]] = {}
    for i in range(n):
        groups.setdefault(find(i), []).append(i)
    merged: list[tuple[Any, float, frozenset[int]]] = []
    for members in groups.values():
        seen_by = frozenset(tiles[m] for m in members)
        if len(members) == 1:
            quad, score = np.asarray(quads[members[0]], np.float32), float(scores[members[0]])
        else:
            lengths = [max(quad_size(quads[m])) for m in members]
            score = float(np.average([scores[m] for m in members], weights=lengths))
            quad = union_quad([quads[m] for m in members])
        merged.append((quad, score, seen_by))
    return [(quad, score) for quad, score, _ in _drop_fragments(merged)]


def _drop_fragments(
    lines: Sequence[tuple[Any, float, frozenset[int]]], inside: float = 0.6
) -> list[tuple[Any, float, frozenset[int]]]:
    """Remove boxes that lie inside a line another tile saw whole.

    Near a seam a tile reports bits of a line its border mangled -- a sliver
    of a column sliced lengthwise, a fragment of one -- that end too far from
    the edge for :func:`clipped_by_tile` and are too thin to pass as the same
    line. They sit inside the full line found by the neighbouring tile. A box
    is only dropped in favour of a line seen by a tile that did NOT produce
    the box: nesting reported by one tile is the detector's own opinion and
    is left alone (a single pass over these pages yields none).
    """
    cv2 = _cv2()
    areas = [float(cv2.contourArea(np.asarray(q, np.float32))) for q, _, _ in lines]
    keep = []
    for i, (quad, _, seen_by) in enumerate(lines):
        fragment = False
        for j, (other, _, other_seen_by) in enumerate(lines):
            if i == j or areas[j] <= areas[i] or areas[i] <= 0 or other_seen_by <= seen_by:
                continue
            shared, _ = cv2.intersectConvexConvex(
                np.asarray(quad, np.float32), np.asarray(other, np.float32)
            )
            if shared / areas[i] >= inside:
                fragment = True
                break
        if not fragment:
            keep.append(lines[i])
    return keep


# ---------------------------------------------------------------------------
# recognizer pre/post-processing
# ---------------------------------------------------------------------------


def crop_line(bgr: Any, quad: Any) -> Any:
    """Deskewed crop of a line, turned so the text reads left to right.

    The quad is warped to an upright rectangle; a crop taller than wide (a
    vertical column) is then rotated 90 degrees COUNTER-clockwise, which is
    how the recognizer was trained to see vertical text: the top of the column
    ends up on the left, glyphs lying on their side.
    """
    cv2 = _cv2()
    q = np.asarray(quad, dtype=np.float32)
    width, height = quad_size(q)
    w, h = max(2, int(round(width))), max(2, int(round(height)))
    target = np.array([[0, 0], [w, 0], [w, h], [0, h]], dtype=np.float32)
    matrix = cv2.getPerspectiveTransform(q, target)
    crop = cv2.warpPerspective(
        bgr, matrix, (w, h), flags=cv2.INTER_CUBIC, borderMode=cv2.BORDER_REPLICATE
    )
    if h > w:
        crop = cv2.rotate(crop, cv2.ROTATE_90_COUNTERCLOCKWISE)
    return crop


def slice_quad(quad: Any, start: float, end: float) -> Any:
    """The stretch ``[start, end]`` of a line quad, in pixels along its reading axis.

    Measured from the line's start edge (top of a column, left of a row);
    ``start`` may be negative and ``end`` may exceed the line's length, which
    is how a probe reaches past the box and how an accepted mark grows it.
    """
    q = np.asarray(quad, dtype=np.float32)
    width, height = quad_size(q)
    if height > width:
        axis = ((q[3] - q[0]) + (q[2] - q[1])) / 2
        axis = axis / (np.linalg.norm(axis) or 1.0)
        return np.array(
            [q[0] + axis * start, q[1] + axis * start, q[1] + axis * end, q[0] + axis * end],
            dtype=np.float32,
        )
    axis = ((q[1] - q[0]) + (q[2] - q[3])) / 2
    axis = axis / (np.linalg.norm(axis) or 1.0)
    return np.array(
        [q[0] + axis * start, q[0] + axis * end, q[3] + axis * end, q[3] + axis * start],
        dtype=np.float32,
    )


def widen_quad(quad: Any, share: float) -> Any:
    """``quad`` grown ACROSS its reading axis by ``share`` of its thickness per side."""
    q = np.asarray(quad, dtype=np.float32)
    width, height = quad_size(q)
    if height > width:
        side = (q[1] - q[0]) + (q[2] - q[3])
        side = side / (np.linalg.norm(side) or 1.0) * width * share
        return np.array([q[0] - side, q[1] + side, q[2] + side, q[3] - side], dtype=np.float32)
    side = (q[3] - q[0]) + (q[2] - q[1])
    side = side / (np.linalg.norm(side) or 1.0) * height * share
    return np.array([q[0] - side, q[1] - side, q[2] + side, q[3] + side], dtype=np.float32)


def _aligned(text: str, other: str) -> dict[int, int]:
    """Positions of ``text`` -> positions of ``other`` where the two line up.

    Equal stretches and same-length replacements only: where one read has a
    glyph more than the other there is no telling which character answers
    which.
    """
    from difflib import SequenceMatcher  # noqa: PLC0415

    mapping: dict[int, int] = {}
    matcher = SequenceMatcher(None, text, other, autojunk=False)
    for tag, i1, i2, j1, j2 in matcher.get_opcodes():
        if tag == "equal" or (tag == "replace" and i2 - i1 == j2 - j1):
            mapping.update(zip(range(i1, i2), range(j1, j2), strict=True))
    return mapping


def vote_characters(
    text: str, confs: Sequence[float], others: Sequence[tuple[str, Sequence[float]]]
) -> tuple[str, list[float], int]:
    """Settle doubted characters of ``text`` by the ``others`` (see ``VOTE_MAX_CONF``).

    A kanji under ``VOTE_MAX_CONF`` is replaced when every other read names
    the same different kanji at its place, their mean confidence no lower
    than its own; that mean becomes its confidence. Returns ``(text,
    confidences, characters changed)``.
    """
    if not others or len(confs) != len(text):
        return text, list(confs), 0
    maps = [_aligned(text, other) for other, _ in others]
    chars, out_confs, changed = list(text), list(confs), 0
    for i, conf in enumerate(confs):
        if conf >= VOTE_MAX_CONF or chars[i] in (MISSING_GLYPH, IDEOGRAPHIC_SPACE):
            continue
        votes = [
            (other[m[i]], oc[m[i]])
            for (other, oc), m in zip(others, maps, strict=True)
            if i in m and m[i] < len(oc)
        ]
        names = {ch for ch, _ in votes}
        if len(votes) != len(others) or len(names) != 1:
            continue
        winner = names.pop()
        support = float(sum(c for _, c in votes) / len(votes))
        if winner == chars[i] or support < conf or not (_is_kanji(winner) and _is_kanji(chars[i])):
            continue
        chars[i] = winner
        out_confs[i] = support
        changed += 1
    return "".join(chars), out_confs, changed


def probe_quads(quad: Any) -> list[Any]:
    """The end probes of a line: ``[start probe, end probe]``, or one for both.

    Each reaches ``PROBE_PAD_EM`` beyond the box and ``PROBE_INSIDE_EM`` into
    it (ems = the line's thickness), so the probes of a page have the same
    aspect and share recognizer batches. A line too short for two probes that
    do not overlap gets a single one over its whole length, padded at both
    ends: half the work for the many short lines (ruby runs) of a page.
    """
    length, em = max(quad_size(quad)), quad_thickness(quad)
    pad, inside = PROBE_PAD_EM * em, PROBE_INSIDE_EM * em
    if length <= 2 * inside:
        return [slice_quad(quad, -pad, length + pad)]
    return [slice_quad(quad, -pad, inside), slice_quad(quad, length - inside, length + pad)]


def _repeats_start(text: str, rest: str) -> bool:
    """Does the inner part of a start probe repeat the line's first glyphs?

    The probe's inner end falls wherever 2.5 em happens to land, usually
    THROUGH a glyph, and half a glyph reads as anything (bench temp00070:
    probe "「お願】" for the line "お願い、やめて!!」"). So the probe's last glyph
    may disagree -- as long as at least one whole glyph before it agrees.
    """
    return text.startswith(rest) or (len(rest) >= 2 and text.startswith(rest[:-1]))


def _repeats_end(text: str, rest: str) -> bool:
    return text.endswith(rest) or (len(rest) >= 2 and text.endswith(rest[1:]))


def clipped_opener(text: str, probe: str, *, thin: bool = False) -> str:
    """The opening mark the start probe found in front of ``text``, or ''.

    Accepted only when the probe reads as that mark followed by the line's
    own first glyphs -- the probe sees the same ink as the full read, so a
    probe that disagrees about the glyphs it shares is not trusted about the
    one it adds. ``thin`` also accepts ``PROBE_THIN_OPENERS`` (see there).
    """
    marks = PROBE_OPENERS + (PROBE_THIN_OPENERS if thin else "")
    if len(probe) < 2 or probe[0] not in marks:
        return ""
    if probe[0] in PROBE_OPENERS and text[:1] in PROBE_OPENERS:
        return ""
    # A dash may be doubled ("――" is the usual form) but not tripled.
    if probe[0] in PROBE_THIN_OPENERS and text[:2] == probe[0] * 2:
        return ""
    return probe[0] if _repeats_start(text, probe[1:]) else ""


def clipped_closer(text: str, probe: str) -> str:
    """The closing mark the end probe found after ``text``, or '' (see above)."""
    if len(probe) < 2 or probe[-1] not in PROBE_CLOSERS or text[-1:] in PROBE_CLOSERS:
        return ""
    return probe[-1] if _repeats_end(text, probe[:-1]) else ""


def clipped_marks(text: str, probe: str) -> tuple[str, str]:
    """``(opener, closer)`` found by ONE probe spanning a whole short line.

    Same rule as the two-probe case: the probe must be the line's own text
    with nothing but the recovered marks around it.
    """
    opener = probe[:1] if probe[:1] in PROBE_OPENERS and text[:1] not in PROBE_OPENERS else ""
    closer = probe[-1:] if probe[-1:] in PROBE_CLOSERS and text[-1:] not in PROBE_CLOSERS else ""
    for head, tail in ((opener, closer), (opener, ""), ("", closer)):
        if (head or tail) and f"{head}{text}{tail}" == probe:
            return head, tail
    return "", ""


def shared_seam_glyphs(pieces: Sequence[Line], union: Any) -> int:
    """How many glyphs the pieces of one line read TWICE, once on each side of a cut.

    The detector sometimes cuts a column at a stop, and the unclip then grows
    both boxes over it (bench: 15-40 px of overlap), so both pieces read the
    mark: "...毛並み。" and "。―まるで...". Read as one line the mark appears
    once, which is right -- and one glyph shorter than the pieces together.
    A seam counts only when the boxes really overlap along the reading axis
    AND the glyphs on either side of it are the same; two pieces that merely
    follow each other share no ink, whatever they read.
    """
    frame = _frame(union)
    spans = []
    for piece in pieces:
        along = (np.asarray(piece.quad, dtype=np.float32) - frame.centre) @ frame.axis
        spans.append((float(along.min()), float(along.max()), piece.text.strip()))
    spans.sort(key=lambda s: s[:2])
    shared = 0
    for (_, end, before), (start, _, after) in zip(spans, spans[1:], strict=False):
        if start < end and before and after and before[-1] == after[0]:
            shared += 1
    return shared


def recognizer_width(crop_width: int, crop_height: int) -> int:
    """Input width for a crop at height 48: aspect kept, a multiple of 8.

    Rounded UP to the CTC stride so every window of a long line starts on a
    timestep boundary and the last glyph never loses its trailing timestep.
    """
    raw = REC_HEIGHT * crop_width / max(1, crop_height)
    width = int(math.ceil(raw / REC_STRIDE)) * REC_STRIDE
    return max(REC_MIN_WIDTH, width)


def recognizer_tensor(crop: Any) -> Any:
    """``float32[3, 48, W]``: height 48, ``(x / 255 - 0.5) / 0.5`` on BGR."""
    cv2 = _cv2()
    h, w = crop.shape[:2]
    resized = cv2.resize(crop, (recognizer_width(w, h), REC_HEIGHT), interpolation=cv2.INTER_LINEAR)
    if resized.ndim == 2:
        resized = np.repeat(resized[:, :, None], 3, axis=2)
    x = (resized.astype(np.float32) / 255.0 - 0.5) / 0.5
    return np.ascontiguousarray(x.transpose(2, 0, 1))


def plan_batches(
    widths: Sequence[int],
    *,
    max_batch: int = REC_MAX_BATCH,
    max_waste: float = REC_MAX_PAD_WASTE,
) -> list[list[int]]:
    """Group crop indices into batches of similar width.

    Widest first; a batch is closed when it is full or when padding the next
    (narrower) crop up to the batch width would push the padded share of the
    batch past ``max_waste``. Every index appears exactly once.
    """
    order = sorted(range(len(widths)), key=lambda i: -widths[i])
    batches: list[list[int]] = []
    current: list[int] = []
    used = 0
    for i in order:
        if current:
            batch_w = widths[current[0]]
            total = batch_w * (len(current) + 1)
            waste = 1.0 - (used + widths[i]) / total
            if len(current) >= max_batch or waste > max_waste:
                batches.append(current)
                current, used = [], 0
        current.append(i)
        used += widths[i]
    if current:
        batches.append(current)
    return batches


@dataclass(frozen=True)
class CtcChar:
    """One decoded character: its class confidence and peak timestep."""

    char: str
    conf: float
    t: int


def ctc_greedy(probs: Any, vocab: Sequence[str], *, length: int | None = None) -> list[CtcChar]:
    """Greedy CTC decode of ``[T, classes]`` probabilities.

    Class 0 is the blank; a run of one class is one character (its confidence
    and position are those of the run's strongest timestep); the same class
    again after a blank is a new character. ``length`` limits decoding to the
    first timesteps, which is how batch padding is ignored.
    """
    probs = np.asarray(probs)
    if length is not None:
        probs = probs[:length]
    if probs.shape[0] == 0:
        return []
    classes = probs.argmax(axis=1)
    confs = probs[np.arange(probs.shape[0]), classes]
    out: list[CtcChar] = []
    prev = 0
    for t, (cls, conf) in enumerate(zip(classes.tolist(), confs.tolist(), strict=True)):
        if cls != 0:
            if cls == prev and out:
                if conf > out[-1].conf:
                    out[-1] = CtcChar(out[-1].char, conf, t)
            else:
                char = vocab[cls] if cls < len(vocab) else "�"
                out.append(CtcChar(char, conf, t))
        prev = cls
    return out


def _is_japanese(ch: str) -> bool:
    """Kana or CJK ideograph (what a stray Latin letter cannot sit between)."""
    o = ord(ch)
    return 0x3041 <= o <= 0x30FF or 0x3400 <= o <= 0x9FFF or 0xF900 <= o <= 0xFAFF


def _is_kanji(ch: str) -> bool:
    return 0x3400 <= ord(ch) <= 0x9FFF or 0xF900 <= ord(ch) <= 0xFAFF


def _is_latin(ch: str) -> bool:
    return ch.isascii() and ch.isalpha() or 0xFF21 <= ord(ch) <= 0xFF5A and ch.isalpha()


def doubt_foreign_glyphs(chars: Sequence[CtcChar]) -> list[CtcChar]:
    """Low-confidence Latin letters between Japanese glyphs become ``MISSING_GLYPH``.

    See ``FOREIGN_MAX_CONF``. A confident letter, or one beside another letter
    or a digit, is left alone ("Ａ級", "ｗｗｗ").
    """
    out = list(chars)
    for i in range(1, len(out) - 1):
        ch = out[i]
        if (
            ch.conf < FOREIGN_MAX_CONF
            and _is_latin(ch.char)
            and _is_japanese(out[i - 1].char)
            and _is_japanese(out[i + 1].char)
        ):
            out[i] = CtcChar(MISSING_GLYPH, ch.conf, ch.t)
    return out


def fill_gaps(chars: Sequence[CtcChar], tensor: Any) -> list[CtcChar]:
    """Give a line back the cells the decoder skipped (see ``GAP_MIN_GLYPHS``).

    ``tensor`` is the recognizer input the characters were decoded from
    (``[3, 48, W]``, one CTC timestep per ``REC_STRIDE`` columns). A hole
    between two peaks is filled by what its ink says: none = a full-width
    space; a little, next to a dash = the other half of a two-cell dash; a
    glyph's worth = ``MISSING_GLYPH``. Anything in between changes nothing.
    Inserted characters carry confidence 1.0 (space), the dash's own, or 0.0.
    """
    if len(chars) < 2:
        return list(chars)
    steps = np.diff([c.t for c in chars])
    # A short line ("――嫌だ。") has too few steps for a pitch of its own. Its
    # crop is one glyph high, so a cell is about REC_HEIGHT wide; on that
    # footing only the dash rule is trusted.
    dashes_only = len(chars) < GAP_MIN_GLYPHS
    pitch = REC_HEIGHT / REC_STRIDE if dashes_only else float(np.median(steps))
    if pitch < GAP_MIN_PITCH or not (steps > GAP_MIN_RATIO * pitch).any():
        return list(chars)
    grey = np.asarray(tensor, dtype=np.float32).mean(axis=0)
    ink = np.abs(grey - float(np.median(grey))) > GAP_INK_CONTRAST
    out = [chars[0]]
    for prev, nxt in zip(chars, chars[1:], strict=False):
        step = nxt.t - prev.t
        if step > GAP_MIN_RATIO * pitch:
            lo, hi = int(prev.t + pitch / 2), int(nxt.t - pitch / 2)
            hole = ink[:, lo * REC_STRIDE : (hi + 1) * REC_STRIDE]
            share = float(hole.mean()) if hole.size else 1.0
            count = min(GAP_MAX_FILL, max(1, int(round(step / pitch)) - 1))
            dash = prev if prev.char in DASHES else nxt if nxt.char in DASHES else None
            fill: tuple[str, float] | None = None
            if GAP_BLANK_MAX_INK <= share < GAP_DASH_MAX_INK and dash is not None:
                fill = (dash.char, dash.conf)
            elif dashes_only:
                pass
            elif share < GAP_BLANK_MAX_INK:
                fill = (IDEOGRAPHIC_SPACE, 1.0)
            elif share >= GAP_GLYPH_MIN_INK:
                fill = (MISSING_GLYPH, 0.0)
            if fill is not None:
                for k in range(count):
                    t = prev.t + int(round((k + 1) * step / (count + 1)))
                    out.append(CtcChar(fill[0], fill[1], t))
        out.append(nxt)
    return out


def window_spans(
    total: int,
    window: int = REC_WINDOW // REC_STRIDE,
    overlap: int = REC_WINDOW_OVERLAP // REC_STRIDE,
) -> list[tuple[int, int]]:
    """``(start, end)`` timestep spans that cover ``total`` with ``overlap``.

    One span when the line fits in a window and a bit (a short tail window
    would see too little context to be worth a second run).
    """
    if total <= window + overlap:
        return [(0, total)]
    step = window - overlap
    count = math.ceil((total - overlap) / step)
    # Spread evenly so the last window is as wide as the others.
    step_f = (total - window) / (count - 1)
    return [(int(round(i * step_f)), int(round(i * step_f)) + window) for i in range(count)]


def _class_runs(classes: Sequence[int], offset: int) -> list[tuple[int, int, int]]:
    """``(class, first, last)`` of every non-blank run, in full-line timesteps."""
    runs: list[tuple[int, int, int]] = []
    for t, cls in enumerate(classes):
        if cls == 0:
            continue
        if runs and runs[-1][0] == cls and runs[-1][2] == offset + t - 1:
            runs[-1] = (cls, runs[-1][1], offset + t)
        else:
            runs.append((cls, offset + t, offset + t))
    return runs


def choose_cut(left: Any, right: Any, lo: int, hi: int) -> int:
    """Timestep in ``[lo, hi]`` at which to switch from one window to the next.

    ``left`` / ``right`` are the two windows' probabilities over the shared
    timesteps ``lo..hi-1``; the result ``cut`` means "``left`` before ``cut``,
    ``right`` from ``cut`` on".

    A blank in both windows is not enough: the windows see a glyph a few
    timesteps apart, so a gap in both can still have the glyph BEFORE it in
    one window and AFTER it in the other, and the character is read twice (or
    not at all). A cut is therefore only trusted when the windows agree on
    what surrounds it: the same glyphs between ``lo`` and the cut, the same
    glyphs between the cut and ``hi``. Both agreeing beats one (a glyph sliced
    by a window border is often misread on that side only); ties go to the
    middle of the overlap, furthest from both borders. Cuts inside a glyph of
    either window are never candidates. With no candidate at all (solid ink
    across the overlap) the most blank-like timestep is used.
    """
    left, right = np.asarray(left), np.asarray(right)
    runs_l = _class_runs(left.argmax(axis=1).tolist(), lo)
    runs_r = _class_runs(right.argmax(axis=1).tolist(), lo)
    centre = (lo + hi) / 2
    best: tuple[tuple[int, float], int] | None = None
    for cut in range(lo, hi + 1):
        if any(first < cut <= last for _, first, last in (*runs_l, *runs_r)):
            continue
        before = [[c for c, _, last in runs if last < cut] for runs in (runs_l, runs_r)]
        after = [[c for c, first, _ in runs if first >= cut] for runs in (runs_l, runs_r)]
        key = (int(before[0] == before[1]) + int(after[0] == after[1]), -abs(cut - centre))
        if best is None or key > best[0]:
            best = (key, cut)
    if best is not None:
        return best[1]
    return lo + int(np.argmax(np.minimum(left[:, 0], right[:, 0])))


def stitch_windows(
    windows: Sequence[Any], spans: Sequence[tuple[int, int]], *, guard: int = REC_WINDOW_GUARD
) -> Any:
    """Join per-window CTC probabilities into one ``[T, classes]`` matrix.

    Windows are aligned by construction (each starts on a timestep boundary
    of the full crop), so stitching is choosing where to switch from one
    window to the next inside their overlap (:func:`choose_cut`), and the
    joined matrix is decoded once like any other line. ``guard`` timesteps at
    each window's edge are kept out of the choice because a glyph sliced by
    the window border is unreliable.
    """
    total = spans[-1][1]
    out = np.zeros((total, windows[0].shape[1]), dtype=np.float32)
    start_at = 0
    for idx, (win, (s, e)) in enumerate(zip(windows, spans, strict=True)):
        win = np.asarray(win)[: e - s]
        if idx + 1 < len(spans):
            next_s = spans[idx + 1][0]
            lo, hi = next_s + guard, e - guard
            if hi <= lo:
                lo, hi = next_s, e
            other = np.asarray(windows[idx + 1])
            cut = choose_cut(win[lo - s : hi - s], other[lo - next_s : hi - next_s], lo, hi)
        else:
            cut = e
        out[start_at:cut] = win[start_at - s : cut - s]
        start_at = cut
    return out


# ---------------------------------------------------------------------------
# results
# ---------------------------------------------------------------------------


@dataclass
class Line:
    """One detected (and possibly recognized) text line in page pixels."""

    quad: Any
    score: float
    text: str = ""
    conf: float = 0.0
    char_confs: list[float] = field(default_factory=list)

    @property
    def vertical(self) -> bool:
        return quad_is_vertical(self.quad)

    @property
    def angle(self) -> float:
        return quad_angle(self.quad)


def sort_lines(lines: Iterable[Line]) -> list[Line]:
    """Stable, layout-agnostic order: right to left, then top to bottom.

    Real reading order is the block merger's business; this only makes the
    raw JSON deterministic and roughly readable for a vertical page.
    """

    def key(line: Line) -> tuple[float, float]:
        centre = np.asarray(line.quad, dtype=np.float32).mean(axis=0)
        return (-float(centre[0]), float(centre[1]))

    return sorted(lines, key=key)


def page_to_json(
    lines: Sequence[Line],
    width: int,
    height: int,
    *,
    detector: dict[str, Any] | None = None,
    compact: bool = False,
) -> dict[str, Any]:
    """The raw page JSON (format in the module docstring).

    ``compact`` is the fixture flavour: coordinates rounded to 0.1 px, no
    per-character confidences.
    """
    digits = 1 if compact else 2
    out_lines = []
    for line in lines:
        entry: dict[str, Any] = {
            "quad": [[round(float(x), digits), round(float(y), digits)] for x, y in line.quad],
            "score": round(line.score, 4),
            "text": line.text,
            "conf": round(line.conf, 4),
            "vertical": line.vertical,
            "angle": round(line.angle, 2),
        }
        if not compact:
            entry["char_confs"] = [round(c, 4) for c in line.char_confs]
        out_lines.append(entry)
    page: dict[str, Any] = {"format": FORMAT_ID, "width": int(width), "height": int(height)}
    if detector is not None:
        page["detector"] = detector
    page["lines"] = out_lines
    return page


def lines_from_json(page: dict[str, Any]) -> list[Line]:
    """Inverse of :func:`page_to_json` (fixtures -> ``Line`` objects)."""
    _require_numpy()
    return [
        Line(
            quad=np.asarray(entry["quad"], dtype=np.float32),
            score=float(entry.get("score", 0.0)),
            text=str(entry.get("text", "")),
            conf=float(entry.get("conf", 0.0)),
            char_confs=[float(c) for c in entry.get("char_confs", [])],
        )
        for entry in page.get("lines", [])
    ]


# ---------------------------------------------------------------------------
# the model-facing class
# ---------------------------------------------------------------------------


class PPOcr:
    """The two onnxruntime sessions plus the page-level policies.

    CPU only by design: the models are tiny (a manga page is ~0.3 s on four
    threads) and onnxruntime has no provider for the AMD GPUs this project's
    hosts tend to have, so there is nothing to gain from device plumbing.
    Sessions are created lazily, so an adapter that only detects never loads
    the recognizer.
    """

    def __init__(
        self,
        models: ModelPaths | None = None,
        *,
        threads: int | None = None,
        side: int = DEFAULT_SIDE,
        tile: str = "auto",
    ) -> None:
        _require_numpy()
        if tile not in ("auto", "off", "force"):
            raise ValueError(f"tile must be auto, off or force, got '{tile}'")
        self.models = models if models is not None else resolve_models()
        env_threads = os.environ.get(THREADS_ENV, "").strip()
        self.threads = threads or (int(env_threads) if env_threads.isdigit() else 0) or 4
        self.side = side
        self.tile = tile
        self._det: Any = None
        self._rec: Any = None
        self._vocab: list[str] | None = None
        # Filled by detect(): what was run and why (goes into the page JSON).
        self.last_detect_info: dict[str, Any] = {}

    # -- sessions ----------------------------------------------------------

    def _session(self, path: Path) -> Any:
        import onnxruntime as ort  # noqa: PLC0415

        options = ort.SessionOptions()
        options.intra_op_num_threads = self.threads
        options.log_severity_level = 3
        return ort.InferenceSession(str(path), options, providers=["CPUExecutionProvider"])

    @property
    def vocab(self) -> list[str]:
        if self._vocab is None:
            self._vocab = load_vocab(self.models.dictionary)
        return self._vocab

    def _run_detector(self, tensor: Any) -> Any:
        if self._det is None:
            self._det = self._session(self.models.detector)
        name = self._det.get_inputs()[0].name
        return self._det.run(None, {name: tensor})[0][0, 0]

    def _run_recognizer(self, batch: Any) -> Any:
        if self._rec is None:
            self._rec = self._session(self.models.recognizer)
        name = self._rec.get_inputs()[0].name
        return self._rec.run(None, {name: batch})[0]

    # -- detection ---------------------------------------------------------

    def _detect_region(
        self, bgr: Any, size: tuple[int, int], origin: tuple[int, int] = (0, 0)
    ) -> list[tuple[Any, float, float]]:
        """One network run: ``[(page quad, score, thickness in detector px)]``."""
        h, w = bgr.shape[:2]
        prob = self._run_detector(detector_tensor(bgr, size))
        sx, sy = w / size[0], h / size[1]
        found = []
        for quad, score in db_postprocess(prob):
            thickness = quad_thickness(quad)
            page_quad = quad * np.array([sx, sy], np.float32) + np.array(origin, np.float32)
            # Scaling x and y separately (sizes are rounded to /32) can skew a
            # tilted rectangle by a hair; ordering is unaffected.
            found.append((page_quad.astype(np.float32), score, thickness))
        return found

    def _detect_tiled(self, bgr: Any, scale: float) -> tuple[list[tuple[Any, float]], int]:
        """Detect at ``scale`` (page->detector) over overlapping tiles."""
        h, w = bgr.shape[:2]
        tile_px = max(SIDE_MULTIPLE * 8, int(TILE_SIZE / scale))
        overlap_px = int(TILE_OVERLAP / scale)
        grid = tile_grid(w, h, tile_px, overlap_px)
        quads: list[Any] = []
        scores: list[float] = []
        owners: list[int] = []
        for idx, (x0, y0, x1, y1) in enumerate(grid):
            region = bgr[y0:y1, x0:x1]
            size = _scaled_size(x1 - x0, y1 - y0, scale)
            for quad, score, _ in self._detect_region(region, size, (x0, y0)):
                if clipped_by_tile(
                    quad, (x0, y0, x1, y1), (w, h), overlap_px, TILE_EDGE_MARGIN / scale
                ):
                    continue
                quads.append(quad)
                scores.append(score)
                owners.append(idx)
        return merge_tile_lines(quads, scores, owners), len(grid)

    def detect(self, bgr: Any, *, side: int | None = None, tile: str | None = None) -> list[Line]:
        """Rotated line quads of a page, in page pixels, with scores.

        ``tile``: ``off`` = one pass at ``side``; ``auto`` (default) = that
        pass, then a finer one when :func:`needs_fine_pass` says the page is
        too dense for it (tiled only when the finer scale would exceed
        ``MAX_DETECTOR_PIXELS``), and tiles from the start for strips;
        ``force`` = always tile at native scale.
        """
        side = side or self.side
        tile = tile or self.tile
        h, w = bgr.shape[:2]
        info: dict[str, Any] = {"side": side, "tile": tile, "passes": []}
        self.last_detect_info = info

        strip = max(w, h) / max(1, min(w, h)) >= STRIP_ASPECT
        if tile == "force" or (tile == "auto" and strip):
            # Strips keep the scale a normal page of their width would get.
            scale = 1.0 if tile == "force" else min(1.0, side / (min(w, h) * 1.5))
            found, count = self._detect_tiled(bgr, scale)
            info["passes"].append(
                {"scale": round(scale, 4), "tiles": count, "lines": len(found), "why": "strip"}
            )
            return sort_lines(Line(q, s) for q, s in found)

        size = detector_input_size(w, h, side)
        first = self._detect_region(bgr, size)
        first_scale = size[0] / w
        info["passes"].append(
            {"scale": round(first_scale, 4), "tiles": 1, "lines": len(first), "why": "first"}
        )
        result = [(q, s) for q, s, _ in first]
        if tile == "auto":
            thick = [t for _, _, t in first]
            longs = [max(quad_size(q)) * first_scale for q, _, _ in first]
            why = needs_fine_pass(thick, longs)
            if why:
                scale = fine_scale(dense_median(thick, longs) or 0.0, first_scale)
                if scale > first_scale * 1.15:
                    fine_size = _scaled_size(w, h, scale)
                    if fine_size[0] * fine_size[1] <= MAX_DETECTOR_PIXELS:
                        result = [(q, s) for q, s, _ in self._detect_region(bgr, fine_size)]
                        count = 1
                    else:
                        result, count = self._detect_tiled(bgr, scale)
                    info["passes"].append(
                        {
                            "scale": round(scale, 4),
                            "tiles": count,
                            "lines": len(result),
                            "why": why,
                        }
                    )
        return sort_lines(Line(q, s) for q, s in result)

    # -- recognition -------------------------------------------------------

    def recognize_crops(self, crops: Sequence[Any]) -> list[tuple[str, float, list[float]]]:
        """``(text, confidence, per-character confidences)`` per crop.

        Crops must already read left to right (see :func:`crop_line`). Long
        crops are split into overlapping windows; all windows of all crops are
        batched together by width.
        """
        tensors = [recognizer_tensor(c) for c in crops]
        # (crop index, span) per network input, then batches over those.
        pieces: list[tuple[int, tuple[int, int]]] = []
        for i, tensor in enumerate(tensors):
            for span in window_spans(tensor.shape[2] // REC_STRIDE):
                pieces.append((i, span))
        widths = [(e - s) * REC_STRIDE for _, (s, e) in pieces]
        probs: list[Any] = [None] * len(pieces)
        for batch in plan_batches(widths):
            batch_w = max(widths[k] for k in batch)
            # Zero = mid-grey after normalisation, what PaddleOCR pads its
            # training batches with. Unused at the default REC_MAX_PAD_WASTE.
            x = np.zeros((len(batch), 3, REC_HEIGHT, batch_w), dtype=np.float32)
            for row, k in enumerate(batch):
                i, (s, e) = pieces[k]
                x[row, :, :, : widths[k]] = tensors[i][:, :, s * REC_STRIDE : e * REC_STRIDE]
            out = self._run_recognizer(x)
            for row, k in enumerate(batch):
                probs[k] = out[row, : widths[k] // REC_STRIDE]

        results: list[tuple[str, float, list[float]]] = []
        for i in range(len(crops)):
            mine = [k for k, (owner, _) in enumerate(pieces) if owner == i]
            spans = [pieces[k][1] for k in mine]
            full = (
                probs[mine[0]]
                if len(mine) == 1
                else stitch_windows([probs[k] for k in mine], spans)
            )
            decoded = ctc_greedy(full, self.vocab)
            # The line's confidence is the recognizer's: over what it decoded,
            # not over the cells filled in afterwards.
            conf = float(np.mean([c.conf for c in decoded])) if decoded else 0.0
            chars = fill_gaps(doubt_foreign_glyphs(decoded), tensors[i])
            text = "".join(c.char for c in chars)
            results.append((text, conf, [c.conf for c in chars]))
        return results

    def recognize(self, bgr: Any, quad: Any) -> tuple[str, float, list[float]]:
        """Read one line: ``(text, confidence, per-character confidences)``."""
        return self.recognize_crops([crop_line(bgr, quad)])[0]

    def read_page(
        self, bgr: Any, *, side: int | None = None, tile: str | None = None
    ) -> list[Line]:
        """Detect, then recognize every line (batched across the page)."""
        lines = self.detect(bgr, side=side, tile=tile)
        texts = self.recognize_crops([crop_line(bgr, line.quad) for line in lines])
        for line, (text, conf, confs) in zip(lines, texts, strict=True):
            line.text, line.conf, line.char_confs = text, conf, confs
        return lines

    def recover_clipped_ends(
        self, bgr: Any, lines: Sequence[Line], thin: Sequence[bool] | None = None
    ) -> int:
        """Give lines back the bracket or stop the detector's box left out.

        See ``PROBE_PAD_EM``. Lines are changed in place -- text, quad and
        per-character confidences together, so the quad still spans exactly
        the glyphs of the text. Returns how many marks were recovered.
        ``thin[i]`` opens line ``i`` to ``PROBE_THIN_OPENERS`` as well.

        Not part of :meth:`read_page`, because the caller has to leave ruby
        out and only the layout knows what is ruby: a ruby run's probe reaches
        into the column beside it and comes back with THAT column's bracket or
        stop (bench: "いや" -> "「いや", "やみ" -> "やみ。"), after which the run
        no longer reads as kana and is kept as body text.
        """
        flags = list(thin) if thin is not None else [False] * len(lines)
        owners = [
            (ln, flag)
            for ln, flag in zip(lines, flags, strict=True)
            if len(ln.text.strip()) >= PROBE_MIN_GLYPHS
        ]
        probes = [probe_quads(ln.quad) for ln, _ in owners]
        reads = iter(self.recognize_crops([crop_line(bgr, q) for quads in probes for q in quads]))
        recovered = 0
        for (line, thin_ok), quads in zip(owners, probes, strict=True):
            head, head_conf, _ = next(reads)
            if len(quads) == 1:
                tail_conf = head_conf
                opener, closer = clipped_marks(line.text, head.strip())
                if not opener and not closer and thin_ok:
                    opener = clipped_opener(line.text, head.strip(), thin=True)
            else:
                tail, tail_conf, _ = next(reads)
                opener = clipped_opener(line.text, head.strip(), thin=thin_ok)
                closer = clipped_closer(line.text, tail.strip())
            if not opener and not closer:
                continue
            length = max(quad_size(line.quad))
            pitch = length / max(1, len(line.text))
            is_thin = bool(opener) and opener in PROBE_THIN_OPENERS
            head_grow = PROBE_GROW_PITCH_THIN if is_thin else PROBE_GROW_PITCH
            line.quad = order_quad(
                slice_quad(
                    line.quad,
                    -head_grow * pitch if opener else 0.0,
                    length + (PROBE_GROW_PITCH * pitch if closer else 0.0),
                )
            )
            line.text = f"{opener}{line.text}{closer}"
            line.char_confs = [
                *([head_conf] if opener else []),
                *line.char_confs,
                *([tail_conf] if closer else []),
            ]
            recovered += len(opener) + len(closer)
        return recovered

    def second_opinions(self, bgr: Any, lines: Sequence[Line]) -> int:
        """Put the doubted characters of ``lines`` to a vote (``VOTE_MAX_CONF``).

        Lines are changed in place; returns how many characters changed. Like
        :meth:`recover_clipped_ends` it is the caller's to leave ruby out: a
        wider crop of a ruby run holds the column beside it.
        """
        doubted = [
            ln
            for ln in lines
            if len(ln.text) >= VOTE_MIN_GLYPHS
            and len(ln.char_confs) == len(ln.text)
            and min(ln.char_confs) < VOTE_MAX_CONF
        ]
        if not doubted:
            return 0
        reads = self.recognize_crops(
            [crop_line(bgr, widen_quad(ln.quad, share)) for ln in doubted for share in VOTE_WIDEN]
        )
        changed = 0
        for k, line in enumerate(doubted):
            others = [
                (text, confs) for text, _, confs in reads[k * len(VOTE_WIDEN) :][: len(VOTE_WIDEN)]
            ]
            line.text, line.char_confs, count = vote_characters(line.text, line.char_confs, others)
            changed += count
        return changed

    def join_lines(
        self, bgr: Any, lines: Sequence[Line], groups: Sequence[Sequence[int]]
    ) -> list[Line]:
        """Re-read groups of lines that are pieces of ONE printed line.

        ``groups`` hold indices into ``lines`` (``line_layout.column_pieces``
        finds them: the detector cut a column at a dash it did not box, or
        split off a first glyph it then could not read). Each group is read
        again as one quad spanning its pieces, and replaces them when that
        read is confident and no shorter than the pieces' texts together
        (an unread piece counting as one glyph, a mark two overlapping pieces
        both read counting once) -- one line per printed line
        is what the reader's uniform character grid needs. A group that reads
        worse joined stays as it was. The result
        keeps the input order, a joined line standing where its first piece
        stood.
        """
        if not groups:
            return list(lines)
        quads = [union_quad([lines[i].quad for i in group]) for group in groups]
        reads = self.recognize_crops([crop_line(bgr, quad) for quad in quads])
        replaced: dict[int, Line | None] = {}
        for group, quad, (text, conf, confs) in zip(groups, quads, reads, strict=True):
            # A piece the recognizer read as nothing must now add a glyph,
            # or joining it only stretched the quad over blank paper or art.
            wanted = sum(max(1, len(lines[i].text.strip())) for i in group)
            wanted -= shared_seam_glyphs([lines[i] for i in group], quad)
            if conf < JOIN_MIN_CONF or len(text.strip()) < wanted:
                continue
            score = float(np.mean([lines[i].score for i in group]))
            first = min(group)
            replaced[first] = Line(quad, score, text, conf, confs)
            for i in group:
                if i != first:
                    replaced[i] = None
        out: list[Line] = []
        for i, line in enumerate(lines):
            new = replaced.get(i, line)
            if new is not None:
                out.append(new)
        return out


def _scaled_size(width: int, height: int, scale: float) -> tuple[int, int]:
    return (
        max(SIDE_MULTIPLE, int(round(width * scale / SIDE_MULTIPLE)) * SIDE_MULTIPLE),
        max(SIDE_MULTIPLE, int(round(height * scale / SIDE_MULTIPLE)) * SIDE_MULTIPLE),
    )


def imread_bgr(path: str | os.PathLike[str]) -> Any:
    """Read any page image (webp/avif included, via Pillow) as BGR uint8."""
    from PIL import Image  # noqa: PLC0415

    _require_numpy()
    with Image.open(path) as im:
        rgb = np.asarray(im.convert("RGB"))
    return np.ascontiguousarray(rgb[:, :, ::-1])


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="PP-OCRv6 manga: one page -> raw lines JSON")
    parser.add_argument("--image", required=True, help="page image")
    parser.add_argument("--out", required=True, help="output JSON path")
    parser.add_argument("--side", type=int, default=DEFAULT_SIDE, help="detector side limit")
    parser.add_argument("--tile", choices=("auto", "off", "force"), default="auto")
    parser.add_argument("--models", default=None, help=f"models directory (else ${MODELS_ENV})")
    parser.add_argument("--precision", choices=tuple(MODEL_FILES), default=None)
    parser.add_argument("--threads", type=int, default=None)
    parser.add_argument("--no-recognize", action="store_true", help="detect only")
    parser.add_argument("--compact", action="store_true", help="fixture flavour (rounded)")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    engine = PPOcr(
        resolve_models(args.models, precision=args.precision),
        threads=args.threads,
        side=args.side,
        tile=args.tile,
    )
    bgr = imread_bgr(args.image)
    started = time.perf_counter()
    lines = engine.detect(bgr) if args.no_recognize else engine.read_page(bgr)
    elapsed = time.perf_counter() - started
    height, width = bgr.shape[:2]
    info = dict(engine.last_detect_info, seconds=round(elapsed, 3), threads=engine.threads)
    page = page_to_json(lines, width, height, detector=info, compact=args.compact)
    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(page, ensure_ascii=False, indent=1), encoding="utf-8")
    print(f"[ppocr] {args.image}: {len(lines)} lines in {elapsed:.2f}s -> {out}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
