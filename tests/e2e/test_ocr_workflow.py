"""End-to-end tests for OCR workflow.

Tests the full pipeline: upload to inbox → OCR processing → library output.
"""

from __future__ import annotations

import gzip
import json
import shutil
import tempfile
import threading
import time
import uuid
import zipfile
from collections.abc import Generator
from pathlib import Path
from typing import TYPE_CHECKING
from unittest.mock import patch

import pytest

if TYPE_CHECKING:
    pass


@pytest.fixture
def storage_dir() -> Generator[Path, None, None]:
    """Create a temporary storage directory with inbox and library."""
    temp_dir = Path(tempfile.mkdtemp())
    inbox = temp_dir / "inbox"
    library = temp_dir / "library"
    inbox.mkdir()
    library.mkdir()
    yield temp_dir
    shutil.rmtree(temp_dir)


@pytest.fixture
def sample_cbz(storage_dir: Path) -> Path:
    """Create a sample CBZ file for testing."""
    cbz_path = storage_dir / "test_manga.cbz"
    with zipfile.ZipFile(cbz_path, "w") as zf:
        # Create a minimal CBZ with fake image files
        zf.writestr("page_001.jpg", b"fake image data 1")
        zf.writestr("page_002.jpg", b"fake image data 2")
        zf.writestr("page_003.jpg", b"fake image data 3")
    return cbz_path


@pytest.fixture
def sample_manga_folder(storage_dir: Path) -> Path:
    """Create a sample manga folder for testing."""
    manga_path = storage_dir / "test_manga_folder"
    manga_path.mkdir()
    (manga_path / "page_001.jpg").write_bytes(b"fake image data 1")
    (manga_path / "page_002.jpg").write_bytes(b"fake image data 2")
    return manga_path


class TestOCRProcessor:
    """Tests for the OCR processor."""

    def test_processor_detects_cbz(self, storage_dir: Path, sample_cbz: Path) -> None:
        """Test processor can identify CBZ files."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        assert processor.is_processable(sample_cbz)

    def test_processor_detects_folder(
        self, storage_dir: Path, sample_manga_folder: Path
    ) -> None:
        """Test processor can identify manga folders."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        assert processor.is_processable(sample_manga_folder)

    def test_processor_rejects_non_manga(self, storage_dir: Path) -> None:
        """Test processor rejects non-manga files."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        text_file = storage_dir / "readme.txt"
        text_file.write_text("not a manga")
        assert not processor.is_processable(text_file)

    def test_processor_runs_mokuro(self, storage_dir: Path, sample_cbz: Path) -> None:
        """Test processor invokes mokuro (mocked)."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            sidecar = output_dir / f"{input_path.stem}.mokuro.gz"
            with gzip.open(sidecar, "wt", encoding="utf-8") as f:
                json.dump({"title": "test", "volume": input_path.stem}, f)
            return True

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run) as mock_mokuro:

            # Copy CBZ to inbox
            inbox_path = storage_dir / "inbox" / sample_cbz.name
            shutil.copy(sample_cbz, inbox_path)

            result = processor.process(inbox_path)

            assert result is True
            mock_mokuro.assert_called_once()

    def test_processor_moves_to_library(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Test processor moves processed files to library."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            sidecar = output_dir / f"{input_path.stem}.mokuro.gz"
            with gzip.open(sidecar, "wt", encoding="utf-8") as f:
                json.dump({"title": "test", "volume": input_path.stem}, f)
            return True

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):

            # Copy CBZ to inbox
            inbox_path = storage_dir / "inbox" / sample_cbz.name
            shutil.copy(sample_cbz, inbox_path)

            processor.process(inbox_path)

            # File should be in library now
            library_path = storage_dir / "library" / sample_cbz.name
            assert library_path.exists()
            assert not inbox_path.exists()

    def test_processor_creates_mokuro_file(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Test processor creates .mokuro.gz file."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)

        # Mock mokuro to create the output file
        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            # Simulate mokuro creating output
            mokuro_file = output_dir / f"{input_path.stem}.mokuro.gz"
            with gzip.open(mokuro_file, "wt", encoding="utf-8") as f:
                json.dump({"title": "test", "volume": input_path.stem}, f)
            return True

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):
            inbox_path = storage_dir / "inbox" / sample_cbz.name
            shutil.copy(sample_cbz, inbox_path)

            processor.process(inbox_path)

            # Check .mokuro.gz exists in library
            mokuro_path = storage_dir / "library" / f"{sample_cbz.stem}.mokuro.gz"
            assert mokuro_path.exists()

    def test_processor_handles_failure(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Test processor handles mokuro failure gracefully."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)

        with patch.object(processor, "_run_mokuro") as mock_mokuro:
            mock_mokuro.return_value = False

            inbox_path = storage_dir / "inbox" / sample_cbz.name
            shutil.copy(sample_cbz, inbox_path)

            result = processor.process(inbox_path)

            assert result is False
            # File should remain in inbox on failure
            assert inbox_path.exists()

    def test_processor_with_custom_callback(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Test processor calls status callback."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        messages: list[str] = []

        def callback(msg: str) -> None:
            messages.append(msg)

        processor = OCRProcessor(storage_dir, status_callback=callback)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            (output_dir / f"{input_path.stem}.mokuro.gz").write_bytes(b"mock")
            return True

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):

            inbox_path = storage_dir / "inbox" / sample_cbz.name
            shutil.copy(sample_cbz, inbox_path)

            processor.process(inbox_path)

            assert len(messages) > 0

    def test_library_cbz_processed_in_temp_workspace(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Test library CBZ OCR runs from a temp workspace copy."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        library_cbz = storage_dir / "library" / sample_cbz.name
        shutil.copy(sample_cbz, library_cbz)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            assert ".processing" in str(input_path.parent)
            assert input_path != library_cbz
            sidecar = output_dir / f"{input_path.stem}.mokuro.gz"
            with gzip.open(sidecar, "wt", encoding="utf-8") as f:
                json.dump({"title": "test", "volume": input_path.stem}, f)
            return True

        with (
            patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run),
            patch.object(processor, "ensure_thumbnail", return_value=True),
        ):
            result = processor.process_library_cbz(library_cbz)

        assert result is True
        assert (storage_dir / "library" / f"{sample_cbz.stem}.mokuro.gz").exists()

    def test_processor_attempts_thumbnail_generation(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Test processor attempts thumbnail generation for processed CBZ."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        inbox_path = storage_dir / "inbox" / sample_cbz.name
        shutil.copy(sample_cbz, inbox_path)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            sidecar = output_dir / f"{input_path.stem}.mokuro.gz"
            with gzip.open(sidecar, "wt", encoding="utf-8") as f:
                json.dump({"title": "test", "volume": input_path.stem}, f)
            return True

        with (
            patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run),
            patch.object(processor, "ensure_thumbnail", return_value=True) as mock_thumb,
        ):
            result = processor.process(inbox_path)

        assert result is True
        mock_thumb.assert_called_once_with(storage_dir / "library" / sample_cbz.name)

    def test_library_ocr_imports_sidecar_when_mokuro_exits_error(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """If mokuro exits non-zero but sidecar exists, import and continue."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        library_cbz = storage_dir / "library" / sample_cbz.name
        shutil.copy(sample_cbz, library_cbz)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            (output_dir / f"{input_path.stem}.mokuro").write_text(json.dumps({"title": "test", "volume": input_path.stem}), encoding="utf-8")
            return False

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):
            result = processor.process_library_ocr(library_cbz)

        assert result is True
        assert (storage_dir / "library" / f"{sample_cbz.stem}.mokuro").exists()

    def test_library_ocr_imports_nested_sidecar_when_mokuro_exits_error(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Nested sidecar output in workspace is imported on fallback."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        library_cbz = storage_dir / "library" / sample_cbz.name
        shutil.copy(sample_cbz, library_cbz)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            nested = output_dir / input_path.stem
            nested.mkdir(parents=True, exist_ok=True)
            (nested / f"{input_path.stem}.mokuro").write_text(json.dumps({"title": "test", "volume": input_path.stem}), encoding="utf-8")
            return False

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):
            result = processor.process_library_ocr(library_cbz)

        assert result is True
        assert (storage_dir / "library" / f"{sample_cbz.stem}.mokuro").exists()

    def test_library_ocr_normalizes_sidecar_series_metadata_from_parent_folder(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Series metadata should come from the source series folder, not temp workspace."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        series_dir = storage_dir / "library" / "Trigun"
        series_dir.mkdir(parents=True, exist_ok=True)
        library_cbz = series_dir / "Trigun 01.cbz"
        shutil.copy(sample_cbz, library_cbz)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            sidecar = output_dir / f"{input_path.stem}.mokuro"
            sidecar.write_text(json.dumps({
                "title": "Trigun 01_rn9c9swa",
                "volume": "wrong",
                "title_uuid": "junk",
            }), encoding="utf-8")
            return True

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):
            result = processor.process_library_ocr(library_cbz)

        assert result is True
        sidecar_path = series_dir / "Trigun 01.mokuro"
        assert sidecar_path.exists()
        data = json.loads(sidecar_path.read_text(encoding="utf-8"))
        assert data["title"] == "Trigun"
        assert data["volume"] == "Trigun 01"
        assert data["title_uuid"] == str(uuid.uuid5(uuid.NAMESPACE_DNS, "Trigun"))


class TestOCRWatcher:
    """Tests for the inbox watcher."""

    def test_watcher_detects_new_file(self, storage_dir: Path, sample_cbz: Path) -> None:
        """Test watcher detects new files in inbox."""
        from mokuro_bunko.ocr.watcher import InboxWatcher

        detected_files: list[Path] = []

        def on_new_file(path: Path) -> None:
            detected_files.append(path)

        inbox = storage_dir / "inbox"
        watcher = InboxWatcher(
            inbox, on_new_file=on_new_file, settle_time=0.2
        )

        # Start watcher in background
        watcher_thread = threading.Thread(target=watcher.start, daemon=True)
        watcher_thread.start()

        try:
            # Give watcher time to initialize
            time.sleep(0.3)

            # Copy file to inbox
            shutil.copy(sample_cbz, inbox / sample_cbz.name)

            # Wait for detection with retries
            for _ in range(20):
                if detected_files:
                    break
                time.sleep(0.1)

            assert len(detected_files) == 1
            assert detected_files[0].name == sample_cbz.name
        finally:
            watcher.stop()
            watcher_thread.join(timeout=2.0)

    def test_watcher_ignores_partial_files(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Test watcher waits for file to be fully written."""
        from mokuro_bunko.ocr.watcher import InboxWatcher

        detected_files: list[Path] = []

        def on_new_file(path: Path) -> None:
            detected_files.append(path)

        inbox = storage_dir / "inbox"
        watcher = InboxWatcher(
            inbox, on_new_file=on_new_file, settle_time=0.3
        )

        watcher_thread = threading.Thread(target=watcher.start, daemon=True)
        watcher_thread.start()

        try:
            time.sleep(0.2)

            # Create partial file
            partial_path = inbox / "partial.cbz"
            with open(partial_path, "wb") as f:
                f.write(b"partial")
                f.flush()
                # File not closed yet - watcher should wait

            # Short wait - file should not be detected yet
            time.sleep(0.2)

            # Finish writing
            with open(partial_path, "ab") as f:
                f.write(b" content")

            # Wait for settle time
            time.sleep(0.5)

            assert len(detected_files) == 1
        finally:
            watcher.stop()
            watcher_thread.join(timeout=2.0)

    def test_watcher_processes_existing_files(
        self, storage_dir: Path, sample_cbz: Path
    ) -> None:
        """Test watcher processes files already in inbox on startup."""
        from mokuro_bunko.ocr.watcher import InboxWatcher

        detected_files: list[Path] = []

        def on_new_file(path: Path) -> None:
            detected_files.append(path)

        inbox = storage_dir / "inbox"

        # Put file in inbox before starting watcher
        shutil.copy(sample_cbz, inbox / sample_cbz.name)

        watcher = InboxWatcher(
            inbox, on_new_file=on_new_file, process_existing=True,
            settle_time=0.2
        )

        watcher_thread = threading.Thread(target=watcher.start, daemon=True)
        watcher_thread.start()

        try:
            # Wait for detection with retries
            for _ in range(20):
                if detected_files:
                    break
                time.sleep(0.1)
            assert len(detected_files) == 1
        finally:
            watcher.stop()
            watcher_thread.join(timeout=2.0)

    def test_watcher_stops_cleanly(self, storage_dir: Path) -> None:
        """Test watcher can be stopped."""
        from mokuro_bunko.ocr.watcher import InboxWatcher

        inbox = storage_dir / "inbox"
        watcher = InboxWatcher(inbox, on_new_file=lambda p: None)

        watcher_thread = threading.Thread(target=watcher.start, daemon=True)
        watcher_thread.start()

        time.sleep(0.2)
        watcher.stop()
        watcher_thread.join(timeout=2.0)

        assert not watcher_thread.is_alive()


class TestOCRWorkflow:
    """End-to-end tests for the complete OCR workflow."""

    def test_full_workflow(self, storage_dir: Path, sample_cbz: Path) -> None:
        """Test complete upload → process → library workflow."""
        from mokuro_bunko.ocr.processor import OCRProcessor
        from mokuro_bunko.ocr.watcher import InboxWatcher

        inbox = storage_dir / "inbox"
        library = storage_dir / "library"
        processed_files: list[Path] = []

        processor = OCRProcessor(storage_dir)

        # Mock mokuro to simulate success
        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            mokuro_file = output_dir / f"{input_path.stem}.mokuro.gz"
            with gzip.open(mokuro_file, "wt", encoding="utf-8") as f:
                json.dump({"title": "test", "volume": input_path.stem}, f)
            return True

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):

            def on_new_file(path: Path) -> None:
                result = processor.process(path)
                if result:
                    processed_files.append(path)

            watcher = InboxWatcher(
                inbox, on_new_file=on_new_file, settle_time=0.2
            )

            watcher_thread = threading.Thread(target=watcher.start, daemon=True)
            watcher_thread.start()

            try:
                time.sleep(0.3)

                # Upload file to inbox
                shutil.copy(sample_cbz, inbox / sample_cbz.name)

                # Wait for processing with retries
                for _ in range(30):
                    if processed_files:
                        break
                    time.sleep(0.1)

                # Verify results
                assert len(processed_files) == 1
                assert (library / sample_cbz.name).exists()
                assert (library / f"{sample_cbz.stem}.mokuro.gz").exists()
                assert not (inbox / sample_cbz.name).exists()
            finally:
                watcher.stop()
                watcher_thread.join(timeout=2.0)

    def test_multiple_files_queued(self, storage_dir: Path) -> None:
        """Test processing multiple files in queue."""
        from mokuro_bunko.ocr.processor import OCRProcessor
        from mokuro_bunko.ocr.watcher import InboxWatcher

        inbox = storage_dir / "inbox"
        library = storage_dir / "library"
        processed_count = [0]

        processor = OCRProcessor(storage_dir)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            mokuro_file = output_dir / f"{input_path.stem}.mokuro.gz"
            with gzip.open(mokuro_file, "wt", encoding="utf-8") as f:
                json.dump({"title": "test", "volume": input_path.stem}, f)
            return True

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):

            def on_new_file(path: Path) -> None:
                if processor.process(path):
                    processed_count[0] += 1

            watcher = InboxWatcher(inbox, on_new_file=on_new_file)

            watcher_thread = threading.Thread(target=watcher.start, daemon=True)
            watcher_thread.start()

            try:
                time.sleep(0.3)

                # Create multiple CBZ files
                for i in range(3):
                    cbz_path = inbox / f"manga_{i}.cbz"
                    with zipfile.ZipFile(cbz_path, "w") as zf:
                        zf.writestr("page.jpg", b"test")

                # Wait for all to process
                time.sleep(2.0)

                assert processed_count[0] == 3
                assert len(list(library.glob("*.cbz"))) == 3
                assert len(list(library.glob("*.mokuro.gz"))) == 3
            finally:
                watcher.stop()
                watcher_thread.join(timeout=2.0)

    def test_error_recovery(self, storage_dir: Path, sample_cbz: Path) -> None:
        """Test workflow recovers from processing errors."""
        from mokuro_bunko.ocr.processor import OCRProcessor
        from mokuro_bunko.ocr.watcher import InboxWatcher

        inbox = storage_dir / "inbox"
        library = storage_dir / "library"
        error_count = [0]
        success_count = [0]

        processor = OCRProcessor(storage_dir)

        def mock_mokuro_run(input_path: Path, output_dir: Path, **kwargs: object) -> bool:
            # First file fails, second succeeds
            if "fail" in input_path.name:
                return False
            mokuro_file = output_dir / f"{input_path.stem}.mokuro.gz"
            with gzip.open(mokuro_file, "wt", encoding="utf-8") as f:
                json.dump({"title": "test", "volume": input_path.stem}, f)
            return True

        with patch.object(processor, "_run_mokuro", side_effect=mock_mokuro_run):

            def on_new_file(path: Path) -> None:
                if processor.process(path):
                    success_count[0] += 1
                else:
                    error_count[0] += 1

            watcher = InboxWatcher(inbox, on_new_file=on_new_file)

            watcher_thread = threading.Thread(target=watcher.start, daemon=True)
            watcher_thread.start()

            try:
                time.sleep(0.3)

                # Create one that fails
                fail_path = inbox / "fail_manga.cbz"
                with zipfile.ZipFile(fail_path, "w") as zf:
                    zf.writestr("page.jpg", b"test")

                # Create one that succeeds
                ok_path = inbox / "ok_manga.cbz"
                with zipfile.ZipFile(ok_path, "w") as zf:
                    zf.writestr("page.jpg", b"test")

                time.sleep(1.5)

                assert error_count[0] == 1
                assert success_count[0] == 1
                # Failed file should remain in inbox
                assert fail_path.exists()
                # Successful file should be in library
                assert (library / "ok_manga.cbz").exists()
            finally:
                watcher.stop()
                watcher_thread.join(timeout=2.0)


class TestOCRProcessorPythonPath:
    """Tests for OCR processor with isolated Python environment."""

    def test_processor_uses_isolated_python(self, storage_dir: Path) -> None:
        """Test processor uses isolated Python from OCR env."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        mock_python = Path("/mock/ocr-env/bin/python")
        processor = OCRProcessor(storage_dir, python_path=mock_python)

        assert processor.python_path == mock_python

    def test_processor_auto_detects_python(self, storage_dir: Path) -> None:
        """Test processor auto-detects Python from installer."""
        from mokuro_bunko.ocr.installer import OCRInstaller
        from mokuro_bunko.ocr.processor import OCRProcessor

        with patch.object(OCRInstaller, "get_python_executable") as mock_get:
            mock_get.return_value = Path("/detected/python")

            processor = OCRProcessor(storage_dir)

            # Should use system python as fallback if no OCR env
            assert processor.python_path is not None


class TestOCRProcessorFormats:
    """Tests for supported input formats."""

    def test_supports_cbz(self, storage_dir: Path) -> None:
        """Test CBZ format is supported."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        cbz = storage_dir / "test.cbz"
        with zipfile.ZipFile(cbz, "w") as zf:
            zf.writestr("page.jpg", b"test")
        assert processor.is_processable(cbz)

    def test_supports_cbr(self, storage_dir: Path) -> None:
        """Test CBR format is supported."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        # CBR is just a renamed RAR file
        cbr = storage_dir / "test.cbr"
        cbr.write_bytes(b"fake rar")
        assert processor.is_processable(cbr)

    def test_supports_zip(self, storage_dir: Path) -> None:
        """Test ZIP format is supported."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        zip_file = storage_dir / "test.zip"
        with zipfile.ZipFile(zip_file, "w") as zf:
            zf.writestr("page.jpg", b"test")
        assert processor.is_processable(zip_file)

    def test_supports_directory(self, storage_dir: Path) -> None:
        """Test directory format is supported."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        manga_dir = storage_dir / "test_manga"
        manga_dir.mkdir()
        (manga_dir / "page.jpg").write_bytes(b"test")
        assert processor.is_processable(manga_dir)

    def test_rejects_text_file(self, storage_dir: Path) -> None:
        """Test text files are rejected."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        txt = storage_dir / "test.txt"
        txt.write_text("not manga")
        assert not processor.is_processable(txt)

    def test_rejects_empty_directory(self, storage_dir: Path) -> None:
        """Test empty directories are rejected."""
        from mokuro_bunko.ocr.processor import OCRProcessor

        processor = OCRProcessor(storage_dir)
        empty_dir = storage_dir / "empty"
        empty_dir.mkdir()
        assert not processor.is_processable(empty_dir)
