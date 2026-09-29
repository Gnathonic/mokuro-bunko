"""Detector adapter: Kellenok/PP-OCRv6_manga DBNet (Apache-2.0) via onnxruntime.

LICENSE NOTE: the weights (a fine-tune of PaddlePaddle's PP-OCRv6 tiny
detector) and everything this adapter imports are Apache-2.0 or more
permissive, so unlike ``ctd`` there is nothing here the process boundary has
to contain. It still runs in its own process because that is the adapter
contract, and because it keeps onnxruntime out of the recognizer's process.

Why it exists: it is the only detector here that returns LINE-level boxes as
true rotated rectangles -- a slanted shout comes back as a slanted quad, the
ruby beside a column as its own thin line -- and it does so in ~0.1 s per page
on a CPU. It is also the only one that holds up on scanned novel pages
(15-20 columns of ~40 glyphs); see ``../ppocr.py`` for the scale policy.

The engine runner does NOT start this adapter for ``--detector ppocr-manga``:
it needs the CTC read, the joined columns and the recovered brackets as well,
so it runs the same models in its own process (see
``engine_runner.LAYOUT_DETECTORS``). The adapter is the contract-shaped way to
get this detector's geometry for anything else.

Output: ONE BLOCK PER LINE, in the contract's shape. Grouping lines into
bubbles/paragraphs and setting ruby aside are geometry-only steps that belong
to whoever consumes the lines, not to the process that runs the network; the
adapter stays a thin, faithful dump of what the detector saw. Each block
carries two optional keys the contract allows readers to ignore:

- ``angle``: tilt of the line in degrees, positive clockwise on screen;
- ``score``: the detector's confidence for the line.

``lines[0]`` is the rotated quad (top-left, top-right, bottom-right,
bottom-left of the line in its own upright frame), ``box`` its axis-aligned
bounds clipped to the page.

Models: ``$MOKURO_PPOCR_MODELS`` or the Hugging Face cache, downloaded on
first use unless ``MOKURO_PPOCR_DOWNLOAD=0``. ``MOKURO_PPOCR_SIDE`` and
``MOKURO_PPOCR_TILE`` (auto/off/force) override the detector defaults.
"""

from __future__ import annotations

import argparse
import importlib
import math
import os
import sys
import time
from collections.abc import Sequence
from pathlib import Path
from typing import Any, cast

try:
    from _common import AdapterOutput, DetectFn, run_adapter, wanted_device
except ImportError:  # imported as a package module (tests, mypy)
    from mokuro_bunko.ocr.detectors._common import (
        AdapterOutput,
        DetectFn,
        run_adapter,
        wanted_device,
    )


def load_ppocr() -> Any:
    """The ``ppocr`` module: from the package, else by path.

    In the processing workspace the adapters sit in ``detectors/`` and
    ``ppocr.py`` one directory up, next to the runner -- the same relative
    place it has inside the package, so one path rule covers both.
    """
    try:
        from mokuro_bunko.ocr import ppocr  # noqa: PLC0415

        return ppocr
    except ImportError:
        parent = str(Path(__file__).resolve().parent.parent)
        if parent not in sys.path:
            sys.path.insert(0, parent)
        return importlib.import_module("ppocr")


def model_weights(ppocr: Any, engine: Any) -> dict[str, str]:
    """The pinned repo behind this engine's models, for the sidecar.

    Empty when the files did not come from that pinned download (a
    ``$MOKURO_PPOCR_MODELS`` directory holds whatever was copied into it):
    weights are only ever claimed from what really resolved them. Mirrors
    ``engine_runner.PPOcrPageReader``, which reads the same models in the
    runner's own process.
    """
    if not getattr(getattr(engine, "models", None), "pinned", True):
        return {}
    repo = getattr(ppocr, "REPO_ID", None)
    revision = getattr(ppocr, "REPO_REVISION", None)
    return {str(repo): str(revision)} if repo and revision else {}


def line_block(
    quad: Sequence[Sequence[float]],
    *,
    vertical: bool,
    thickness: float,
    angle: float,
    score: float,
    width: int,
    height: int,
) -> dict[str, Any]:
    """One contract block for one detected line.

    ``font_size`` is the line's thickness: the detector's boxes hug the glyphs
    (about 0.05-0.1 of a glyph of slack), so thickness is the glyph size the
    contract asks for.
    """
    xs = [float(p[0]) for p in quad]
    ys = [float(p[1]) for p in quad]
    box = [
        max(0, int(math.floor(min(xs)))),
        max(0, int(math.floor(min(ys)))),
        min(width, int(math.ceil(max(xs)))),
        min(height, int(math.ceil(max(ys)))),
    ]
    return {
        "box": box,
        "vertical": bool(vertical),
        "font_size": max(8, int(round(thickness))),
        "lines": [[[round(float(x), 2), round(float(y), 2)] for x, y in quad]],
        "angle": round(float(angle), 2),
        "score": round(float(score), 4),
    }


def setup(args: argparse.Namespace, out: AdapterOutput) -> DetectFn:
    """Resolve the models once; hand back the per-page detection."""
    # This detector is onnxruntime on the CPU by design (see ``../ppocr.py``),
    # so a card asked for is refused here, in one sentence, rather than
    # accepted and quietly ignored.
    wanted_device(args, cpu_only=True, tag="ppocr-manga")
    ppocr = load_ppocr()
    side_env = os.environ.get("MOKURO_PPOCR_SIDE", "").strip()
    side = int(side_env) if side_env.isdigit() else ppocr.DEFAULT_SIDE
    tile = os.environ.get("MOKURO_PPOCR_TILE", "auto").strip().lower() or "auto"
    t0 = time.time()
    engine = ppocr.PPOcr(side=side, tile=tile)
    # ``ppocr.py`` pins its own repo (``REPO_REVISION``); report what it
    # resolved, but only when the files really came from that pinned
    # download -- a ``$MOKURO_PPOCR_MODELS`` directory holds whatever was
    # copied into it, and the sidecar must not claim a commit for it.
    out.weights(model_weights(ppocr, engine))
    print(
        f"[detector:ppocr-manga] models resolved in {time.time() - t0:.1f}s "
        f"({engine.models.precision}, side={side}, tile={tile}, threads={engine.threads})",
        flush=True,
    )

    def detect(page: Path) -> tuple[dict[str, Any], str]:
        bgr = ppocr.imread_bgr(page)
        height, width = bgr.shape[:2]
        blocks = [
            line_block(
                line.quad.tolist(),
                vertical=line.vertical,
                thickness=ppocr.quad_thickness(line.quad),
                angle=line.angle,
                score=line.score,
                width=width,
                height=height,
            )
            for line in engine.detect(bgr)
        ]
        payload = {"img_width": width, "img_height": height, "blocks": blocks}
        passes = len(engine.last_detect_info.get("passes", []))
        return payload, f"lines={len(blocks)} passes={passes}"

    # onnxruntime on the CPU, deliberately: see ``../ppocr.py``.
    detect.device = "cpu"  # type: ignore[attr-defined]
    return detect


def main(argv: Sequence[str] | None = None) -> int:
    # _common is untyped where it is imported as a script (mypy sees Any).
    return cast(
        int, run_adapter("ppocr-manga", "PP-OCRv6 manga line detector (Apache-2.0)", setup, argv)
    )


if __name__ == "__main__":
    sys.exit(main())
