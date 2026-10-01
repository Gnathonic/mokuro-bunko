"""Harvest REAL PP-OCR line records from real pages (run once; needs onnxruntime).

    ~/.cache/mokuro-bunko-demo/engines-env/bin/python harvest_pages.py

Runs the 0.5.2 ``PPOcrPageReader.read_lines`` (detect, read, join, probe,
vote) from this worktree's ``src/`` on pages of archives in ~/Downloads and
writes the UNROUNDED lines -- exactly the ``ppocr.Line`` objects the reader
hands to layout/reconcile -- to ``inputs/real_pages.json``. Floats are written
with repr, so they load back bit-exact. The archives themselves are not
copied (copyrighted, and far too big); only the line records are kept.
"""

from __future__ import annotations

import json
import sys
import zipfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
SRC = HERE.parents[3] / "src" / "mokuro_bunko" / "ocr"
sys.path.insert(0, str(SRC))

import cv2  # noqa: E402
import numpy as np  # noqa: E402

import engine_runner  # noqa: E402

DL = Path.home() / "Downloads"
# (archive, page numbers in natural order, label)
PICKS = [
    ("D049-213 One-Punch Man - One-Punch Man 20.cbz", [9, 31, 66], "opm"),
    ("Dr Stone (HD Scan) - Dr Stone 01.cbz", [6, 21, 44], "drstone"),
    ("Kono Subarashii Sekai ni Shukufuku wo! (Upscaled) - [渡真仁, 暁なつめ, 三嶋くろね] この素晴らしい世界に祝福を！ 01.cbz", [8, 27], "konosuba"),
    ("Isakku - イサック_第01巻_mokuro.cbz", [12, 40], "isakku"),
    ("魔のものたちは企てる - 魔のものたちは企てる ３.cbz", [10, 30], "manomono"),
    ("To LOVEる 1 とらぶる- 01.cbz", [15], "tolove"),
    ("#Zombie Sagashitemasu - -Zombie-Sagashitemasu-01.cbz", [20], "zombie"),
    ("The Relation of Alimentation and Disease - James Henry Salisbury.cbz", [20], "salisbury"),
]
NOVEL_DIR = DL / "13DL.ME_Bakemonogatari VOL 01-21" / "13DL.ME_Bakemonogatari v01"
NOVEL_PAGES = [12, 40, 41]

EXTS = (".jpg", ".jpeg", ".png", ".webp", ".avif")


def natural(names):
    from natsort import natsorted

    return list(natsorted(names))


def line_record(line):
    return {
        "quad": [[float(x), float(y)] for x, y in line.quad],
        "score": float(line.score),
        "text": line.text,
        "conf": float(line.conf),
        "vertical": bool(line.vertical),
        "angle": float(line.angle),
        "char_confs": [float(c) for c in line.char_confs],
    }


def main() -> None:
    reader = engine_runner.PPOcrPageReader()
    pages = []

    def run(name, data):
        img = cv2.imdecode(np.frombuffer(data, np.uint8), cv2.IMREAD_COLOR)
        lines, info, _first = reader.read_lines(img)
        h, w = img.shape[:2]
        pages.append(
            {
                "name": name,
                "width": int(w),
                "height": int(h),
                "detector": info,
                "lines": [line_record(ln) for ln in lines],
            }
        )
        print(name, len(lines), "lines", flush=True)

    for archive, picks, label in PICKS:
        path = DL / archive
        if not path.exists():
            print("missing", archive)
            continue
        with zipfile.ZipFile(path) as zf:
            names = natural([n for n in zf.namelist() if n.lower().endswith(EXTS) and "__MACOSX" not in n])
            for k in picks:
                if k < len(names):
                    run(f"{label}-{k:03d}", zf.read(names[k]))
    if NOVEL_DIR.exists():
        files = natural([p.name for p in NOVEL_DIR.iterdir() if p.suffix.lower() in EXTS])
        for k in NOVEL_PAGES:
            if k < len(files):
                run(f"bakemono-{k:03d}", (NOVEL_DIR / files[k]).read_bytes())
    out = HERE / "inputs" / "real_pages.json"
    out.parent.mkdir(exist_ok=True)
    out.write_text(json.dumps({"pages": pages}, ensure_ascii=False, default=engine_runner.json_default), encoding="utf-8")
    print("wrote", out, out.stat().st_size)


if __name__ == "__main__":
    main()
