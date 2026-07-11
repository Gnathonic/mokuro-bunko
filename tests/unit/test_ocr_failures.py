"""Unit tests for OCR failure records, retry backoff, and log capture."""

from __future__ import annotations

import json
import sys
import time
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.ocr.processor import OcrFailure, OCRProcessor
from mokuro_bunko.ocr.watcher import OCRWorker


@pytest.fixture
def storage(temp_dir: Path) -> Path:
    """Storage root with a library directory."""
    (temp_dir / "library").mkdir(parents=True)
    return temp_dir


@pytest.fixture
def worker(storage: Path) -> OCRWorker:
    """OCR worker over the temp storage (never started)."""
    return OCRWorker(storage_path=storage, poll_interval=30.0)


def _make_cbz(path: Path) -> Path:
    """Create a minimal CBZ with one fake page."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as zf:
        zf.writestr("page_001.jpg", b"fake image data")
    return path


class TestFailureRecords:
    """Persisting and clearing .ocr-failures.json records."""

    def test_record_failure_creates_entry(self, worker: OCRWorker, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "Series A" / "Vol 01.cbz")
        worker.processor.last_failure = OcrFailure("boom", "C:/logs/vol.log")

        worker._record_ocr_failure(cbz)

        data = json.loads((storage / ".ocr-failures.json").read_text(encoding="utf-8"))
        entry = data[str(cbz.relative_to(storage / "library"))]
        assert entry["error"] == "boom"
        assert entry["attempts"] == 1
        assert entry["volume"] == "Vol 01"
        assert entry["series"] == "Series A"
        assert entry["log_file"] == "C:/logs/vol.log"

    def test_repeat_failures_increment_attempts(self, worker: OCRWorker, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        worker.processor.last_failure = OcrFailure("boom")

        worker._record_ocr_failure(cbz)
        worker._record_ocr_failure(cbz)
        worker._record_ocr_failure(cbz)

        data = json.loads((storage / ".ocr-failures.json").read_text(encoding="utf-8"))
        assert next(iter(data.values()))["attempts"] == 3

    def test_clear_failure_removes_entry_and_file(self, worker: OCRWorker, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        worker.processor.last_failure = OcrFailure("boom")
        worker._record_ocr_failure(cbz)

        worker._clear_ocr_failure(cbz)

        # File is removed once the last record is cleared.
        assert not (storage / ".ocr-failures.json").exists()

    def test_corrupt_failures_file_treated_as_empty(self, worker: OCRWorker, storage: Path) -> None:
        (storage / ".ocr-failures.json").write_text("{not json", encoding="utf-8")
        assert worker._load_failures() == {}


class TestRetryBackoff:
    """Backoff filtering in _ocr_candidates."""

    def test_retry_delay_grows_and_caps(self, worker: OCRWorker) -> None:
        assert worker._retry_delay_seconds(1) == 30.0
        assert worker._retry_delay_seconds(2) == 120.0
        assert worker._retry_delay_seconds(3) == 480.0
        assert worker._retry_delay_seconds(10) == 3600.0

    def test_recent_failure_is_skipped(self, worker: OCRWorker, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        worker.processor.last_failure = OcrFailure("boom")
        worker._record_ocr_failure(cbz)

        assert worker._ocr_candidates() == []

    def test_failure_retried_after_backoff(self, worker: OCRWorker, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        worker.processor.last_failure = OcrFailure("boom")
        worker._record_ocr_failure(cbz)

        # Age the record past the first backoff window.
        failures = worker._load_failures()
        key = next(iter(failures))
        failures[key]["last_attempt_at"] = time.time() - 60.0
        worker._save_failures(failures)

        assert worker._ocr_candidates() == [cbz]

    def test_replaced_file_resets_failure(self, worker: OCRWorker, storage: Path) -> None:
        cbz = _make_cbz(storage / "library" / "S" / "V.cbz")
        worker.processor.last_failure = OcrFailure("boom")
        worker._record_ocr_failure(cbz)

        # Backdate the failure so the file's mtime is newer (file "replaced").
        failures = worker._load_failures()
        key = next(iter(failures))
        failures[key]["last_attempt_at"] = cbz.stat().st_mtime - 10.0
        # Keep the backoff window active to prove the mtime reset wins.
        failures[key]["attempts"] = 10
        worker._save_failures(failures)

        assert worker._ocr_candidates() == [cbz]
        # Record was reset (dropped) for the replaced file.
        assert worker._load_failures() == {}


class TestErrorExtraction:
    """Parsing failure reasons out of captured mokuro logs."""

    def test_extracts_traceback_final_line(self, tmp_path: Path) -> None:
        log = tmp_path / "vol.log"
        log.write_text(
            "Processing pages...\n"
            "Traceback (most recent call last):\n"
            '  File "x.py", line 1, in <module>\n'
            "ValueError: Couldn't instantiate the backend tokenizer\n"
            "2026-07-09 | INFO | mokuro.run:run:146 - Processed successfully: 0/1\n",
            encoding="utf-8",
        )
        error = OCRProcessor._extract_mokuro_error(log)
        assert error is not None
        assert error.startswith("ValueError: Couldn't instantiate")

    def test_extracts_loguru_error_line(self, tmp_path: Path) -> None:
        log = tmp_path / "vol.log"
        log.write_text(
            "2026-07-09 19:28:48 | ERROR | mokuro.run:run:142 - Error while processing volume X\n",
            encoding="utf-8",
        )
        error = OCRProcessor._extract_mokuro_error(log)
        assert error == "Error while processing volume X"

    def test_extracts_custom_exception_from_traceback(self, tmp_path: Path) -> None:
        """Non-Error-suffixed exceptions (e.g. mokuro's InvalidImage) are caught."""
        log = tmp_path / "vol.log"
        log.write_text(
            "Traceback (most recent call last):\n"
            '  File "mokuro_generator.py", line 65, in process_volume\n'
            "    raise e\n"
            '  File "manga_page_ocr.py", line 50, in __call__\n'
            "    raise InvalidImage()\n"
            "mokuro.manga_page_ocr.InvalidImage: Animation file, Corrupted file or Unsupported type\n"
            "2026-07-09 20:43:25 | ERROR | mokuro.run:run:142 - Error while processing volume X\n",
            encoding="utf-8",
        )
        error = OCRProcessor._extract_mokuro_error(log)
        assert error is not None
        assert error.startswith("mokuro.manga_page_ocr.InvalidImage:")

    def test_no_error_lines_returns_none(self, tmp_path: Path) -> None:
        log = tmp_path / "vol.log"
        log.write_text("all fine\nProcessed successfully: 1/1\n", encoding="utf-8")
        assert OCRProcessor._extract_mokuro_error(log) is None

    def test_detects_mokuro_reported_failure(self, tmp_path: Path) -> None:
        log = tmp_path / "vol.log"
        log.write_text("... Processed successfully: 0/1\n", encoding="utf-8")
        assert OCRProcessor._mokuro_reported_failure(log) is True

    def test_success_summary_not_flagged(self, tmp_path: Path) -> None:
        log = tmp_path / "vol.log"
        log.write_text("... Processed successfully: 1/1\n", encoding="utf-8")
        assert OCRProcessor._mokuro_reported_failure(log) is False


class TestSubprocessCapture:
    """_run_mokuro captures subprocess output to a per-volume log."""

    def test_failed_run_writes_log_and_error(self, storage: Path) -> None:
        processor = OCRProcessor(storage, python_path=Path(sys.executable))
        volume = storage / "library" / "S" / "Broken Vol"
        volume.mkdir(parents=True)
        (volume / "page_001.jpg").write_bytes(b"fake")

        # mokuro is not installed in the test interpreter, so the subprocess
        # exits non-zero with a ModuleNotFoundError traceback.
        result = processor._run_mokuro(volume, storage / "out")

        assert result.ok is False
        assert result.log_path is not None
        assert result.log_path.exists()
        content = result.log_path.read_text(encoding="utf-8")
        assert "mokuro" in content
        assert result.error is not None
        assert "mokuro" in result.error.lower() or "Error" in result.error
