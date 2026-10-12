"""PP-OCRv6 manga detector/recognizer: already ONNX upstream, copied byte for byte from the pin."""

from __future__ import annotations

import argparse
import shutil
from pathlib import Path

from .common import DEFAULT_OUT, REPO_ROOT, log, sha256, snapshot_file
from .pins import PPOCR, PPOCR_FILES, check_pins


def run(out: Path) -> list[Path]:
    check_pins(REPO_ROOT)
    out.mkdir(parents=True, exist_ok=True)
    written = []
    for flat, repo_path in PPOCR_FILES.items():
        src = snapshot_file(PPOCR, repo_path)
        dst = out / flat
        shutil.copyfile(src, dst)
        if sha256(src) != sha256(dst):
            raise RuntimeError(f"copy of {repo_path} differs")
        log(f"[ppocr] {PPOCR.repo}@{PPOCR.revision[:12]}:{repo_path} -> {flat} ({dst.stat().st_size:,} bytes)")
        written.append(dst)
    return written


def main(argv: list[str] | None = None) -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, default=DEFAULT_OUT)
    run(ap.parse_args(argv).out)


if __name__ == "__main__":
    main()
