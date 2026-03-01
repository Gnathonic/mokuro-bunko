"""Unit tests for OCR progress metric behavior."""

from __future__ import annotations

from pathlib import Path

from mokuro_bunko.ocr.processor import OCRProcessor


def test_progress_metrics_finalizing_at_total_pages(tmp_path: Path) -> None:
    """Progress reaches 100% and switches to finalizing when done == total."""
    processor = OCRProcessor(storage_path=tmp_path)
    percent, eta_seconds, status = processor._progress_metrics(
        done=195,
        total_images=195,
        elapsed=120.0,
    )
    assert percent == 100
    assert eta_seconds == 0
    assert status == "finalizing"


def test_progress_metrics_running_below_total_pages(tmp_path: Path) -> None:
    """Progress remains running below completion."""
    processor = OCRProcessor(storage_path=tmp_path)
    percent, eta_seconds, status = processor._progress_metrics(
        done=97,
        total_images=195,
        elapsed=60.0,
    )
    assert percent is not None and 0 < percent < 100
    assert isinstance(eta_seconds, int)
    assert eta_seconds > 0
    assert status == "running"
