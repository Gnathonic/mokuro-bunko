"""Source checkpoints, pinned to commits, and their licences.

The recognizer pins are the ones 0.5.2 reads with (``REPO_REVISIONS`` in
``src/mokuro_bunko/ocr/engine_runner.py`` of the 0.5.2 reference checkout, ``MOKURO_REF_052``); the PP-OCR pin is ``REPO_REVISION``
in ``src/mokuro_bunko/ocr/ppocr.py``. :func:`check_pins` (run before every
export) holds them equal, so a pin bump in the runner cannot silently leave
the exports behind.

Licences were read off each repo's model card (``license:`` front matter) at
the pinned revision, or the nearest revision carrying a card; see MODELS.md.
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True)
class Source:
    repo: str
    revision: str
    licence: str


HAYAI = Source("JustANormalTinkerer/hayai-ocr-v2.5-nova", "e46d79138499600564f810d44ab6bdea7230dee1", "Apache-2.0")
SIGLIP2 = Source("google/siglip2-base-patch16-naflex", "b53b807d3a2d5e2b3911292f2d69e5341cdc064c", "Apache-2.0")
PADDLE_BASE = Source("PaddlePaddle/PaddleOCR-VL-1.6", "c5630abae1d940eafe0697512a0325494b02ab42", "Apache-2.0")
PADDLE_LORA = Source("sorryhyun/paddleocr-vl-1.6-manga-lora", "26292839d1469c14212a12a1e01b5b1fe01bff15", "Apache-2.0")
PPOCR = Source("Kellenok/PP-OCRv6_manga", "ba1d479e8a61a20e8318c9758c73fbbbd290b98d", "Apache-2.0")

# Which checkpoints each engine's artifacts are derived from.
ENGINE_SOURCES: dict[str, tuple[Source, ...]] = {
    "hayai-nova": (HAYAI, SIGLIP2),
    "paddle-manga": (PADDLE_BASE, PADDLE_LORA),
    "ppocr-manga": (PPOCR,),
}

# PP-OCRv6 manga files shipped as-is: release name -> path in the HF repo.
PPOCR_FILES: dict[str, str] = {
    "ppocr-manga_det_v0.2.onnx": "det/manga_det_v0.2.onnx",
    "ppocr-manga_rec_v0.2.onnx": "rec/manga_rec_v0.2.onnx",
    "ppocr-manga_dict.txt": "ppocrv6_dict.txt",
}


def runner_pins(repo_root) -> dict[str, str]:
    """``REPO_REVISIONS`` + the PP-OCR pin as the 0.5.2 sources state them (parsed, not imported)."""
    import ast
    from pathlib import Path

    ocr = Path(repo_root) / "src" / "mokuro_bunko" / "ocr"
    pins: dict[str, str] = {}
    tree = ast.parse((ocr / "engine_runner.py").read_text(encoding="utf-8"))
    for node in ast.walk(tree):
        if isinstance(node, ast.AnnAssign) and getattr(node.target, "id", "") == "REPO_REVISIONS":
            pins.update(ast.literal_eval(node.value))
    tree = ast.parse((ocr / "ppocr.py").read_text(encoding="utf-8"))
    consts = {}
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            try:
                consts[node.targets[0].id] = ast.literal_eval(node.value)
            except ValueError:
                pass
    pins[consts["REPO_ID"]] = consts["REPO_REVISION"]
    return pins


def check_pins(repo_root) -> None:
    """Raise if any pin here differs from the 0.5.2 runner's."""
    want = runner_pins(repo_root)
    have = {s.repo: s.revision for s in (HAYAI, SIGLIP2, PADDLE_BASE, PADDLE_LORA, PPOCR)}
    if want != have:
        raise SystemExit(f"pins differ from the runner's:\n runner: {want}\n export: {have}")
