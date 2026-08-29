"""Unit tests for resumable OCR behaviour."""

from __future__ import annotations

import os
import time
from pathlib import Path

from mokuro_bunko.config import Config, OcrConfig
from mokuro_bunko.ocr.processor import OCRProcessor


def test_processor_defaults_ocr_config(tmp_path: Path) -> None:
    """Omitting ocr_config yields defaults rather than None."""
    processor = OCRProcessor(storage_path=tmp_path)
    assert processor.ocr_config.use_cache is True
    assert processor.ocr_config.timeout_minimum_seconds == 3600


def test_processor_accepts_ocr_config(tmp_path: Path) -> None:
    """A supplied config is stored for later use."""
    processor = OCRProcessor(
        storage_path=tmp_path,
        ocr_config=OcrConfig(use_cache=False, timeout_per_page_seconds=30),
    )
    assert processor.ocr_config.use_cache is False
    assert processor.ocr_config.timeout_per_page_seconds == 30


def test_workspace_is_stable_for_same_volume(tmp_path: Path) -> None:
    """The same volume resolves to the same workspace, so retries resume."""
    processor = OCRProcessor(storage_path=tmp_path)
    cbz = tmp_path / "library" / "Some Series" / "01.cbz"
    cbz.parent.mkdir(parents=True)
    cbz.touch()

    first = processor._workspace_for_volume(cbz)
    second = processor._workspace_for_volume(cbz)

    assert first == second
    assert first.is_dir()
    assert first.parent == tmp_path / ".processing"


def test_workspace_differs_between_volumes(tmp_path: Path) -> None:
    """Different volumes must not share a workspace."""
    processor = OCRProcessor(storage_path=tmp_path)
    library = tmp_path / "library" / "Some Series"
    library.mkdir(parents=True)
    first_cbz = library / "01.cbz"
    second_cbz = library / "02.cbz"
    first_cbz.touch()
    second_cbz.touch()

    assert processor._workspace_for_volume(first_cbz) != processor._workspace_for_volume(
        second_cbz
    )


def test_workspace_handles_non_ascii_titles(tmp_path: Path) -> None:
    """Japanese series names must not break path construction."""
    processor = OCRProcessor(storage_path=tmp_path)
    cbz = tmp_path / "library" / "天国大魔境" / "第01巻.cbz"
    cbz.parent.mkdir(parents=True)
    cbz.touch()

    workspace = processor._workspace_for_volume(cbz)

    assert workspace.is_dir()
    assert workspace.name.isalnum()


def test_release_workspace_removes_on_success(tmp_path: Path) -> None:
    """A completed volume's workspace is reclaimed."""
    processor = OCRProcessor(storage_path=tmp_path)
    workspace = tmp_path / ".processing" / "abc123"
    workspace.mkdir(parents=True)
    (workspace / "page.jpg").touch()

    processor._release_workspace(workspace, succeeded=True)

    assert not workspace.exists()


def test_release_workspace_retains_on_failure(tmp_path: Path) -> None:
    """A failed volume keeps its cache so the retry resumes."""
    processor = OCRProcessor(storage_path=tmp_path)
    workspace = tmp_path / ".processing" / "abc123"
    workspace.mkdir(parents=True)
    (workspace / "page.jpg").touch()

    processor._release_workspace(workspace, succeeded=False)

    assert workspace.exists()
    assert (workspace / "page.jpg").exists()


def test_release_workspace_tolerates_missing_directory(tmp_path: Path) -> None:
    """Cleanup must never raise from the finally block."""
    processor = OCRProcessor(storage_path=tmp_path)
    processor._release_workspace(tmp_path / ".processing" / "gone", succeeded=True)


def test_command_omits_no_cache_when_caching_enabled(tmp_path: Path) -> None:
    """Caching on means mokuro may reuse per-page results."""
    processor = OCRProcessor(storage_path=tmp_path, ocr_config=OcrConfig(use_cache=True))
    cmd = processor._build_mokuro_command(tmp_path / "vol")
    assert "--no_cache" not in cmd
    assert "--disable_confirmation" in cmd


def test_command_includes_no_cache_when_caching_disabled(tmp_path: Path) -> None:
    """Caching off forces a full re-OCR."""
    processor = OCRProcessor(storage_path=tmp_path, ocr_config=OcrConfig(use_cache=False))
    cmd = processor._build_mokuro_command(tmp_path / "vol")
    assert "--no_cache" in cmd


def test_timeout_uses_minimum_for_short_volumes(tmp_path: Path) -> None:
    """Short volumes keep the previous one-hour allowance."""
    processor = OCRProcessor(storage_path=tmp_path)
    assert processor._hard_timeout_seconds(0) == 3600
    assert processor._hard_timeout_seconds(59) == 3600
    assert processor._hard_timeout_seconds(60) == 3600


def test_timeout_scales_with_page_count(tmp_path: Path) -> None:
    """A 228-page volume gets 228 minutes, not 60."""
    processor = OCRProcessor(storage_path=tmp_path)
    assert processor._hard_timeout_seconds(228) == 228 * 60


def test_timeout_honours_configured_budget(tmp_path: Path) -> None:
    """Both the per-page budget and the floor are configurable."""
    processor = OCRProcessor(
        storage_path=tmp_path,
        ocr_config=OcrConfig(timeout_per_page_seconds=10, timeout_minimum_seconds=100),
    )
    assert processor._hard_timeout_seconds(5) == 100
    assert processor._hard_timeout_seconds(50) == 500


def test_sweep_removes_aged_workspaces(tmp_path: Path) -> None:
    """Workspaces older than the retention window are reclaimed."""
    processor = OCRProcessor(
        storage_path=tmp_path, ocr_config=OcrConfig(workspace_retention_days=7)
    )
    stale = tmp_path / ".processing" / "stale"
    stale.mkdir(parents=True)
    eight_days_ago = time.time() - (8 * 86400)
    os.utime(stale, (eight_days_ago, eight_days_ago))

    removed = processor.sweep_stale_workspaces()

    assert removed == 1
    assert not stale.exists()


def test_sweep_keeps_recent_workspaces(tmp_path: Path) -> None:
    """A workspace from a recent failure must survive to allow the retry."""
    processor = OCRProcessor(
        storage_path=tmp_path, ocr_config=OcrConfig(workspace_retention_days=7)
    )
    fresh = tmp_path / ".processing" / "fresh"
    fresh.mkdir(parents=True)

    removed = processor.sweep_stale_workspaces()

    assert removed == 0
    assert fresh.exists()


def test_sweep_handles_missing_processing_root(tmp_path: Path) -> None:
    """A server that has never run OCR has no .processing directory."""
    processor = OCRProcessor(storage_path=tmp_path)
    assert processor.sweep_stale_workspaces() == 0


def test_admin_ocr_payload_round_trips() -> None:
    """Settings the admin panel writes are readable back from to_dict."""
    config = Config()
    config.ocr.use_cache = False
    config.ocr.timeout_per_page_seconds = 45
    config.ocr.timeout_minimum_seconds = 1800
    config.ocr.workspace_retention_days = 3

    data = config.to_dict()["ocr"]

    assert data == {
        "backend": "auto",
        "poll_interval": 30,
        "use_cache": False,
        "timeout_per_page_seconds": 45,
        "timeout_minimum_seconds": 1800,
        "workspace_retention_days": 3,
    }
