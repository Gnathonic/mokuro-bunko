"""Detector adapter: comic-text-detector, as shipped inside the ``mokuro`` package.

LICENSE NOTE: comic-text-detector and its weights are GPL-3.0. This adapter
imports that code, so the running adapter process is a GPL-governed
combination. It is executed in its own process by the OCR engines runner and
exchanges only JSON files with it; nothing in the server imports it. This
file itself remains under the repository's MPL-2.0, which permits inclusion
in a GPL larger work (MPL-2.0 section 3.3). Install it only when you accept
that: ``mokuro-bunko install-ocr --detector ctd``.

Emits blocks with per-line quads, identical to what mokuro itself produces.
"""

from __future__ import annotations

import argparse
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
        file_sha256,
        run_adapter,
        wanted_device,
    )
except ImportError:  # imported as a package module (tests, mypy)
    from mokuro_bunko.ocr.detectors._common import (
        AdapterError,
        AdapterOutput,
        DetectFn,
        file_sha256,
        run_adapter,
        wanted_device,
    )

# comic-text-detector's weights are not on the Hugging Face Hub, so there is
# no ``revision=`` to pass: the ``mokuro`` package downloads them once from an
# immutable GitHub release asset. That URL is the pin, and the digest is how
# this adapter enforces it -- which matters more here than for the Hub
# detectors, because a ``.pt`` is a pickle and loading it runs whatever it
# contains. Verified before ``TextDetector`` touches the file, and reported as
# the weights that produced the sidecar.
#
# TO BUMP: change the URL to the new release asset, download it, read the
# release notes, put its sha256 here and note it in the CHANGELOG.
WEIGHTS_URL = (
    "https://github.com/zyddnys/manga-image-translator/releases/download/"
    "beta-0.2.1/comictextdetector.pt"
)
WEIGHTS_SHA256 = "1f90fa60aeeb1eb82e2ac1167a66bf139a8a61b8780acd351ead55268540cccb"


def setup(args: argparse.Namespace, out: AdapterOutput) -> DetectFn:
    """Verify the checkpoint, load it once, and hand back the per-page read."""
    import cv2
    import numpy as np
    import torch
    from comic_text_detector.inference import TextDetector
    from mokuro.cache import cache
    from PIL import Image

    # ``--device`` is where the generation put this stage; without it, the
    # probe this adapter has always done.
    device = wanted_device(args, tag="ctd") or ("cuda" if torch.cuda.is_available() else "cpu")
    if device.startswith("cuda") and not torch.cuda.is_available():
        raise AdapterError(f"--device {device} was asked for, but torch reports no GPU here")
    t0 = time.time()
    weights_path = Path(cache.comic_text_detector)
    digest = file_sha256(weights_path)
    if digest != WEIGHTS_SHA256:
        raise AdapterError(
            f"{weights_path} is not the pinned checkpoint: "
            f"sha256 {digest} != {WEIGHTS_SHA256}. A .pt is a pickle, so it is not "
            f"loaded. Delete the file to re-fetch it from {WEIGHTS_URL}, or update "
            "WEIGHTS_SHA256 if you meant to change checkpoints."
        )
    detector = TextDetector(model_path=weights_path, input_size=1024, device=device, act="leaky")
    out.weights({WEIGHTS_URL: f"sha256:{digest}"})
    print(
        f"[detector:ctd] loaded sha256:{digest[:12]} in {time.time() - t0:.1f}s on {device}",
        flush=True,
    )

    def detect(page: Path) -> tuple[dict[str, Any], str]:
        with Image.open(page) as im:
            bgr = cv2.cvtColor(np.array(im.convert("RGB")), cv2.COLOR_RGB2BGR)
        _mask, _mask_refined, blks = detector(bgr, refine_mode=1, keep_undetected_mask=True)
        blocks = []
        for blk in blks:
            lines = [
                [[float(x), float(y)] for x, y in line] for line in blk.lines_array().tolist()
            ]
            blocks.append(
                {
                    "box": [int(v) for v in blk.xyxy],
                    "vertical": bool(blk.vertical),
                    "font_size": int(blk.font_size),
                    "lines": lines,
                }
            )
        h, w = bgr.shape[:2]
        payload = {"img_width": int(w), "img_height": int(h), "blocks": blocks}
        return payload, f"blocks={len(blocks)}"

    detect.device = device  # type: ignore[attr-defined]
    return detect


def main(argv: Sequence[str] | None = None) -> int:
    # _common is untyped where it is imported as a script (mypy sees Any).
    return cast(int, run_adapter("ctd", "comic-text-detector adapter (GPL-3.0)", setup, argv))


if __name__ == "__main__":
    sys.exit(main())
