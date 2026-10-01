"""Generate the PP-OCR golden fixtures for bunko-ocr from the Python 0.5.2 reader.

Run with an interpreter that has onnxruntime 1.30, opencv-python-headless 5.0,
numpy, pillow 12.3 and natsort 8.4 (see the crate's tests/golden/README note in
gen script header), e.g.::

    ~/.cache/mokuro-bunko-demo/ref052-ocr/bin/python \
        crates/bunko-ocr/tests/golden/gen_ppocr_golden.py

Reads the sample archives in ~/Downloads (never writes there). Writes, per page in
ppocr_pages.json, ``ppocr/<id>.json`` with:

* ``first``: ``page_to_json`` right after ``PPOcr.read_page`` (detection +
  recognition, no page-level passes);
* ``final``: ``page_to_json`` after ``PPOcrPageReader.read_lines`` (joins, end
  probes, second opinions) -- what the layout consumes;
* ``timing``: seconds for read_page and read_lines at the thread count used.

Also writes ``archive_pages.json`` (the runner's page list of every archive used)
and ``natsort_cases.json`` (natsort 8.4 orderings).
"""

from __future__ import annotations

import json
import os
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
OCR_SRC = ROOT / "src" / "mokuro_bunko" / "ocr"
SNAPSHOT = Path.home() / (
    ".cache/huggingface/hub/models--Kellenok--PP-OCRv6_manga/snapshots/"
    "ba1d479e8a61a20e8318c9758c73fbbbd290b98d"
)
DOWNLOADS = Path.home() / "Downloads"
THREADS = int(os.environ.get("GOLDEN_THREADS", "4"))

os.environ["MOKURO_PPOCR_MODELS"] = str(SNAPSHOT)
os.environ["MOKURO_PPOCR_THREADS"] = str(THREADS)
os.environ["MOKURO_PPOCR_DOWNLOAD"] = "0"
sys.path.insert(0, str(OCR_SRC))

import engine_runner  # noqa: E402
import line_layout  # noqa: E402
import ppocr  # noqa: E402


def load_page(entry: dict) -> tuple[object, str]:
    """The page image of an entry: one archive page, optionally cropped, or a
    ``compose`` grid of pages (rows of page indices, concatenated as is)."""
    import io

    import numpy as np

    archive = DOWNLOADS / entry["archive"]
    reader = engine_runner.ArchiveReader(archive)
    pages = reader.pages()

    def decode(index: int) -> object:
        return engine_runner._imdecode(io.BytesIO(reader.read(pages[index])))

    if "compose" in entry:
        rows = [np.concatenate([decode(i) for i in row], axis=1) for row in entry["compose"]]
        img = np.ascontiguousarray(np.concatenate(rows, axis=0))
        rel = "+".join(pages[i].as_posix() for row in entry["compose"] for i in row)
    else:
        rel = pages[entry["page"]].as_posix()
        img = decode(entry["page"])
    reader.close()
    if "crop" in entry:
        x0, y0, x1, y1 = entry["crop"]
        img = img[y0:y1, x0:x1].copy()
    return img, rel


def main() -> int:
    listing_only = "--listing-only" in sys.argv
    only = {a for a in sys.argv[1:] if not a.startswith("--")}
    entries = json.loads((HERE / "ppocr_pages.json").read_text(encoding="utf-8"))
    reader = engine_runner.PPOcrPageReader(ppocr, line_layout)
    out_dir = HERE / "ppocr"
    out_dir.mkdir(exist_ok=True)
    for entry in entries:
        if listing_only or (only and entry["id"] not in only):
            continue
        img, rel = load_page(entry)
        h, w = img.shape[:2]
        t0 = time.perf_counter()
        lines = reader.engine.read_page(img)
        t1 = time.perf_counter()
        first = ppocr.page_to_json(lines, w, h, detector=dict(reader.engine.last_detect_info))
        t2 = time.perf_counter()
        final_lines, info, _ = reader.read_lines(img)
        t3 = time.perf_counter()
        final = ppocr.page_to_json(final_lines, w, h, detector=info)
        doc = {
            "id": entry["id"],
            "archive": entry["archive"],
            "page_index": entry.get("page"),
            "compose": entry.get("compose"),
            "member": rel,
            "crop": entry.get("crop"),
            "threads": THREADS,
            "timing": {"read_page": round(t1 - t0, 4), "read_lines": round(t3 - t2, 4)},
            "first": first,
            "final": final,
        }
        (out_dir / f"{entry['id']}.json").write_text(
            json.dumps(doc, ensure_ascii=False, indent=1), encoding="utf-8"
        )
        print(f"{entry['id']}: {w}x{h} {len(first['lines'])} -> {len(final['lines'])} lines "
              f"read_page {t1 - t0:.2f}s read_lines {t3 - t2:.2f}s", flush=True)

    if not only:
        listing = {}
        archives = {e["archive"] for e in entries}
        for pattern in ("*.cbz", "*/*.cbz", "*/*/*.cbz"):
            archives |= {p.relative_to(DOWNLOADS).as_posix() for p in DOWNLOADS.glob(pattern)}
        for archive in sorted(archives):
            try:
                pages = engine_runner.archive_pages(DOWNLOADS / archive)
            except Exception as e:  # noqa: BLE001 - a broken sample is just skipped
                print(f"skip {archive}: {e}")
                continue
            names = [p.as_posix() for p in pages]
            listing[archive] = {
                "count": len(names),
                "sha256": __import__("hashlib").sha256("\n".join(names).encode()).hexdigest(),
                "head": names[:5],
            }
        (HERE / "archive_pages.json").write_text(
            json.dumps(listing, ensure_ascii=False, indent=0), encoding="utf-8"
        )
        from natsort import natsorted
        cases = [
            ["001", "1", "01", "3", "9", "10", "１２", "B", "_a", "a 2/1", "a.b/1", "a.jpg", "a/1",
             "a/2", "b", "e", "é", "é", "page-1.5", "page-1.10", "x9y99", "x10y2", "x10y10",
             "z", "ば", "は", "ぱ", "ひ", "が/2.jpg", "か/10.jpg", "ガ", "カ", "Ｚ", "ｚ", "Å", "Å",
             "٣.png", "١٠.png", "page٢.png", "file²", "file2", "file₃", "Ⅻ", "x 07 y", "x7y", "",
             "vol 1/p 10.webp", "vol 1/p 9.webp", "vol 10/p 1.webp", "vol 2/p 1.webp"],
        ]
        out = []
        for case in cases:
            sorted_ = [str(p) for p in natsorted([Path(c) if c else c for c in case if c])]
            out.append({"input": [c for c in case if c], "sorted": sorted_})
        (HERE / "natsort_cases.json").write_text(
            json.dumps(out, ensure_ascii=False, indent=0), encoding="utf-8"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
