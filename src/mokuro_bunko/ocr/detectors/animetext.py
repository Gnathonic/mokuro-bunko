"""Detector adapter: deepghs/AnimeText_yolo (YOLO12, GPL-3.0) via onnxruntime.

LICENSE NOTE: the AnimeText weights are GPL-3.0 and the repository is gated
on Hugging Face (accept its terms and log in; the engines environment reads
``HF_TOKEN`` or the token saved by ``hf auth login``). The adapter runs in
its own process and exchanges only JSON with the runner. Install with
``mokuro-bunko install-ocr --detector animetext``.

Why it exists: on the project bench it found the most text overall (display
titles on covers, sound effects) while ignoring decoration the others boxed.
Its one habit is emitting a box around two bubbles alongside each bubble's
own box; ``drop_containers`` keeps the parts.

Emits one block per detected text region with a single line quad; vertical
from aspect ratio, font size from ink runs. Set ``ANIMETEXT_VARIANT`` to use
another size (yolo12n/s/m/l/x, default x).
"""

from __future__ import annotations

import argparse
import os
import sys
import time
from collections.abc import Sequence
from pathlib import Path
from typing import Any, cast

try:
    from _common import (
        AdapterError,
        AdapterOutput,
        DetectFn,
        block_quad,
        dedupe_boxes,
        drop_containers,
        estimate_font_size,
        run_adapter,
        wanted_device,
    )
except ImportError:  # imported as a package module (tests, mypy)
    from mokuro_bunko.ocr.detectors._common import (
        AdapterError,
        AdapterOutput,
        DetectFn,
        block_quad,
        dedupe_boxes,
        drop_containers,
        estimate_font_size,
        run_adapter,
        wanted_device,
    )

REPO = "deepghs/AnimeText_yolo"
# Pinned like every other repo this project resolves: the
# ONNX file a moving ``main`` hands back can change under a self-hoster, and
# the sidecar's ``ocr_engine.weights`` names what boxed the text. The pin
# covers every variant, since they share the repo.
REVISION = "a180c191bfdb9f0e31b57e7de567e7b6bac50f84"
DEFAULT_VARIANT = "yolo12x_animetext"
CONFIDENCE = 0.3
NMS_IOU = 0.5


def letterbox_scale(width: int, height: int, size: int) -> float:
    """Scale that fits a page into a ``size``×``size`` canvas, top-left aligned."""
    return size / max(width, height)


def decode_yolo(
    output: Any, scale: float, width: int, height: int, conf: float
) -> tuple[list[list[int]], list[float]]:
    """Turn a YOLO ONNX output of shape (4+classes, N) into page-pixel boxes."""
    boxes: list[list[int]] = []
    scores: list[float] = []
    for row in output:
        cx, cy, bw, bh = (float(v) for v in row[:4])
        score = float(max(row[4:]))
        if score < conf:
            continue
        x1 = max(0, int((cx - bw / 2) / scale))
        y1 = max(0, int((cy - bh / 2) / scale))
        x2 = min(width, int(round((cx + bw / 2) / scale)))
        y2 = min(height, int(round((cy + bh / 2) / scale)))
        if x2 - x1 < 4 or y2 - y1 < 4:
            continue
        boxes.append([x1, y1, x2, y2])
        scores.append(score)
    return boxes, scores


def setup(args: argparse.Namespace, out: AdapterOutput) -> DetectFn:
    """Resolve and open the pinned ONNX session once; hand back the per-page read."""
    import numpy as np
    import onnxruntime as ort
    from huggingface_hub import hf_hub_download
    from PIL import Image

    variant = os.environ.get("ANIMETEXT_VARIANT", DEFAULT_VARIANT)
    t0 = time.time()
    model_path = hf_hub_download(REPO, f"{variant}/model.onnx", revision=REVISION)
    # ``--device`` picks the execution provider: the CPU one alone when the
    # CPU was asked for, the GPU one first otherwise. A card asked for on a
    # runtime built without a GPU provider is refused rather than quietly run
    # on the CPU, because then the number beside it would be a lie.
    device = wanted_device(args, tag="animetext")
    gpu_providers = ["CUDAExecutionProvider", "ROCMExecutionProvider", "MIGraphXExecutionProvider"]
    available = ort.get_available_providers()
    if device == "cpu":
        providers = ["CPUExecutionProvider"]
    else:
        providers = [*gpu_providers, "CPUExecutionProvider"]
        if device is not None and not any(p in available for p in gpu_providers):
            raise AdapterError(
                f"--device {device} was asked for, but this onnxruntime has no GPU "
                f"execution provider (it offers {', '.join(available)})"
            )
    session = ort.InferenceSession(model_path, providers=[p for p in providers if p in available])
    inp = session.get_inputs()[0]
    size = int(inp.shape[2]) if isinstance(inp.shape[2], int) else 640
    out.weights({REPO: REVISION})
    print(
        f"[detector:animetext] loaded {variant} from {REPO}@{REVISION[:12]} "
        f"in {time.time() - t0:.1f}s (providers={session.get_providers()}, input={size})",
        flush=True,
    )

    def detect(page: Path) -> tuple[dict[str, Any], str]:
        img = Image.open(page).convert("RGB")
        width, height = img.size
        scale = letterbox_scale(width, height, size)
        resized = img.resize(
            (max(1, int(round(width * scale))), max(1, int(round(height * scale))))
        )
        canvas = Image.new("RGB", (size, size), (114, 114, 114))
        canvas.paste(resized, (0, 0))
        x = np.asarray(canvas, dtype=np.float32).transpose(2, 0, 1)[None] / 255.0
        raw = session.run(None, {inp.name: x})[0]
        raw = raw[0].T if raw.ndim == 3 and raw.shape[1] < raw.shape[2] else raw[0]
        boxes, scores = decode_yolo(raw, scale, width, height, CONFIDENCE)
        keep = dedupe_boxes(boxes, scores, NMS_IOU)
        boxes = [boxes[i] for i in keep]
        scores = [scores[i] for i in keep]
        keep = drop_containers(boxes)
        boxes = [boxes[i] for i in keep]
        gray = img.convert("L")
        blocks = []
        for x1, y1, x2, y2 in boxes:
            vertical = (y2 - y1) >= (x2 - x1) * 0.8
            blocks.append(
                {
                    "box": [x1, y1, x2, y2],
                    "vertical": vertical,
                    "font_size": estimate_font_size(gray.crop((x1, y1, x2, y2)), vertical),
                    "lines": [block_quad([x1, y1, x2, y2])],
                }
            )
        blocks.sort(key=lambda b: (b["box"][1] // 200, -b["box"][0]))
        payload = {"img_width": width, "img_height": height, "blocks": blocks}
        return payload, f"blocks={len(blocks)}"

    # onnxruntime picks the provider; the session says which one it really got.
    detect.device = (  # type: ignore[attr-defined]
        "gpu" if any(p != "CPUExecutionProvider" for p in session.get_providers()) else "cpu"
    )
    return detect


def main(argv: Sequence[str] | None = None) -> int:
    # _common is untyped where it is imported as a script (mypy sees Any).
    return cast(int, run_adapter("animetext", "AnimeText YOLO12 detector (GPL-3.0)", setup, argv))


if __name__ == "__main__":
    sys.exit(main())
