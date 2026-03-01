"""Unit tests for thumbnail generation behavior."""

from __future__ import annotations

from io import BytesIO
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.ocr.processor import OCRProcessor


def test_thumbnail_preserves_aspect_and_bounds(tmp_path: Path) -> None:
    """Generated thumbnail fits within 250x350 without stretching."""
    Image = pytest.importorskip("PIL.Image")

    storage = tmp_path
    cbz_path = storage / "sample.cbz"

    # Create a wide source image to detect cropping/stretching.
    img_bytes = BytesIO()
    Image.new("RGB", (1000, 500), color=(200, 50, 50)).save(img_bytes, format="JPEG")

    with zipfile.ZipFile(cbz_path, "w") as zf:
        zf.writestr("001.jpg", img_bytes.getvalue())

    processor = OCRProcessor(storage_path=storage)
    assert processor.ensure_thumbnail(cbz_path) is True

    thumb_path = cbz_path.with_suffix(".webp")
    assert thumb_path.exists()

    with Image.open(thumb_path) as thumb:
        assert thumb.width <= 250
        assert thumb.height <= 350
        # Ratio should remain close to original 2.0.
        ratio = thumb.width / thumb.height
        assert abs(ratio - 2.0) < 0.05
