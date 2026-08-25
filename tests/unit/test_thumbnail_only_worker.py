"""Covers are generated even when the OCR backend is disabled (contract §8)."""

from __future__ import annotations

import zipfile
from io import BytesIO
from pathlib import Path

import pytest

from mokuro_bunko.ocr.watcher import OCRWorker


def make_cbz(path: Path) -> None:
    Image = pytest.importorskip("PIL.Image")
    path.parent.mkdir(parents=True, exist_ok=True)
    buffer = BytesIO()
    Image.new("RGB", (600, 900), color=(30, 90, 150)).save(buffer, format="JPEG")
    with zipfile.ZipFile(path, "w") as archive:
        archive.writestr("001.jpg", buffer.getvalue())


def test_thumbnail_only_worker_never_starts_the_ocr_loop(tmp_path: Path) -> None:
    worker = OCRWorker(storage_path=tmp_path, poll_interval=0.1, thumbnails_only=True)
    worker.start(background=True)
    try:
        assert worker._ocr_thread is None
        assert worker._thumb_thread is not None
        assert worker._thumb_thread.is_alive()
    finally:
        worker.stop()


def test_thumbnail_only_worker_still_generates_covers(tmp_path: Path) -> None:
    cbz = tmp_path / "library" / "Dr Stone" / "Volume 01.cbz"
    make_cbz(cbz)

    worker = OCRWorker(storage_path=tmp_path, poll_interval=0.1, thumbnails_only=True)
    worker._scan_thumbnails_once()

    assert cbz.with_suffix(".webp").exists()


def test_a_normal_worker_still_starts_both_loops(tmp_path: Path) -> None:
    worker = OCRWorker(storage_path=tmp_path, poll_interval=0.1)
    worker.start(background=True)
    try:
        assert worker._ocr_thread is not None
        assert worker._thumb_thread is not None
    finally:
        worker.stop()
