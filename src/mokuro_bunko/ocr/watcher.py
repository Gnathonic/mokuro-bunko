"""Inbox folder watcher for OCR processing.

Monitors the inbox directory for new manga files and triggers processing.
"""

from __future__ import annotations

import json
import logging
import os
import shutil
import tempfile
import threading
import time
from collections import deque
from collections.abc import Callable
from pathlib import Path
from typing import Any

try:
    from watchdog.events import FileSystemEvent, FileSystemEventHandler
    from watchdog.observers import Observer

    WATCHDOG_AVAILABLE = True
except ImportError:
    WATCHDOG_AVAILABLE = False
    Observer = None  # type: ignore
    FileSystemEventHandler = object  # type: ignore
    FileSystemEvent = None  # type: ignore


logger = logging.getLogger(__name__)

# Global pointer to the running OCR worker for API introspection and control.
CURRENT_OCR_WORKER: OCRWorker | None = None


class InboxWatcher:
    """Watches an inbox directory for new files to process."""

    def __init__(
        self,
        inbox_path: Path,
        on_new_file: Callable[[Path], None],
        settle_time: float = 1.0,
        poll_interval: float = 5.0,
        process_existing: bool = False,
    ) -> None:
        """Initialize the inbox watcher.

        Args:
            inbox_path: Path to the inbox directory to watch.
            on_new_file: Callback called when a new file is ready.
            settle_time: Time to wait after file creation before processing.
                        This ensures files are fully written.
            poll_interval: How often to poll for changes (fallback mode).
            process_existing: Whether to process files already in inbox on startup.
        """
        self.inbox_path = inbox_path
        self.on_new_file = on_new_file
        self.settle_time = settle_time
        self.poll_interval = poll_interval
        self.process_existing = process_existing

        self._running = False
        self._stop_event = threading.Event()
        self._pending_files: dict[Path, float] = {}
        self._processed_files: set[Path] = set()
        self._lock = threading.Lock()

        # Use watchdog if available, otherwise fall back to polling
        self._use_watchdog = WATCHDOG_AVAILABLE
        self._observer: Observer | None = None  # type: ignore

    def start(self) -> None:
        """Start watching the inbox directory.

        This method blocks until stop() is called.
        """
        if not self.inbox_path.exists():
            self.inbox_path.mkdir(parents=True, exist_ok=True)

        self._running = True
        self._stop_event.clear()

        # Process existing files if requested
        if self.process_existing:
            self._scan_existing_files()

        if self._use_watchdog:
            self._start_watchdog()
        else:
            self._start_polling()

    def stop(self) -> None:
        """Stop watching the inbox directory."""
        self._running = False
        self._stop_event.set()

        if self._observer is not None:
            self._observer.stop()
            self._observer.join(timeout=5.0)
            self._observer = None

    def _scan_existing_files(self) -> None:
        """Scan inbox for existing files and queue them for processing."""
        if not self.inbox_path.exists():
            return

        for item in self.inbox_path.iterdir():
            if item.name.startswith("."):
                continue
            with self._lock:
                if item not in self._processed_files:
                    self._pending_files[item] = time.time()

    def _start_watchdog(self) -> None:
        """Start watching using watchdog library."""
        handler = _InboxEventHandler(self)
        self._observer = Observer()
        self._observer.schedule(handler, str(self.inbox_path), recursive=False)
        self._observer.start()

        # Process pending files loop
        while not self._stop_event.is_set():
            self._process_pending()
            self._stop_event.wait(timeout=0.1)

    def _start_polling(self) -> None:
        """Start watching using polling fallback."""
        known_files: set[Path] = set()

        while not self._stop_event.is_set():
            try:
                current_files = set(self.inbox_path.iterdir())

                # Find new files
                new_files = current_files - known_files
                for path in new_files:
                    if not path.name.startswith("."):
                        self._on_file_created(path)

                known_files = current_files

            except Exception as e:
                logger.error(f"Error polling inbox: {e}")

            # Process pending files
            self._process_pending()

            self._stop_event.wait(timeout=self.poll_interval)

    def _on_file_created(self, path: Path) -> None:
        """Handle a new file being created.

        Args:
            path: Path to the created file.
        """
        with self._lock:
            if path not in self._processed_files:
                self._pending_files[path] = time.time()
                logger.debug(f"File detected: {path}")

    def _on_file_modified(self, path: Path) -> None:
        """Handle a file being modified.

        Args:
            path: Path to the modified file.
        """
        with self._lock:
            if path in self._pending_files:
                # Reset settle timer
                self._pending_files[path] = time.time()

    def _process_pending(self) -> None:
        """Process files that have settled."""
        current_time = time.time()
        ready_files: list[Path] = []

        with self._lock:
            # Find files that have settled
            for path, created_time in list(self._pending_files.items()):
                if current_time - created_time >= self.settle_time:
                    if path.exists():
                        ready_files.append(path)
                        self._processed_files.add(path)
                    del self._pending_files[path]

        # Process ready files
        for path in ready_files:
            try:
                logger.info(f"Processing file: {path}")
                self.on_new_file(path)
            except Exception as e:
                logger.error(f"Error processing {path}: {e}")


if WATCHDOG_AVAILABLE:

    class _InboxEventHandler(FileSystemEventHandler):  # type: ignore
        """Watchdog event handler for inbox directory."""

        def __init__(self, watcher: InboxWatcher) -> None:
            super().__init__()
            self.watcher = watcher

        def on_created(self, event: FileSystemEvent) -> None:  # type: ignore
            """Handle file creation event."""
            if event.is_directory:
                return

            path = Path(event.src_path)
            if not path.name.startswith("."):
                self.watcher._on_file_created(path)

        def on_modified(self, event: FileSystemEvent) -> None:  # type: ignore
            """Handle file modification event."""
            if event.is_directory:
                return

            path = Path(event.src_path)
            if not path.name.startswith("."):
                self.watcher._on_file_modified(path)

        def on_moved(self, event: FileSystemEvent) -> None:  # type: ignore
            """Handle file move event (rename)."""
            if event.is_directory:
                return

            # Treat as new file at destination
            if hasattr(event, "dest_path"):
                path = Path(event.dest_path)
                if not path.name.startswith("."):
                    self.watcher._on_file_created(path)


class OCRWorker:
    """Background worker that combines watcher and processor."""

    def __init__(
        self,
        storage_path: Path,
        poll_interval: float = 30.0,
        processor_timeouts: dict[str, int] | None = None,
        status_callback: Callable[[str], None] | None = None,
    ) -> None:
        """Initialize the OCR worker.

        Args:
            storage_path: Base storage path.
            poll_interval: How often to poll for new files.
            status_callback: Optional callback for status messages.
        """
        from mokuro_bunko.ocr.processor import OCRProcessor

        self.storage_path = storage_path
        self.poll_interval = poll_interval
        self.status_callback = status_callback or (lambda msg: None)

        timeout_cfg = processor_timeouts or {}
        self.processor = OCRProcessor(
            storage_path=storage_path,
            status_callback=self.status_callback,
            progress_callback=self._on_progress,
            hard_timeout_seconds=int(timeout_cfg.get("hard_timeout_seconds", 3600)),
            no_progress_timeout_seconds=int(timeout_cfg.get("no_progress_timeout_seconds", 600)),
            finalizing_timeout_seconds=int(timeout_cfg.get("finalizing_timeout_seconds", 180)),
        )

        self.watcher: InboxWatcher | None = None
        self._ocr_thread: threading.Thread | None = None
        self._thumb_thread: threading.Thread | None = None
        self._running = False
        self._paused = False
        self._inflight_ocr: set[Path] = set()
        self._inflight_thumbs: set[Path] = set()
        self._progress_path = self.storage_path / ".ocr-progress.json"
        self._history_path = self.storage_path / ".ocr-history.jsonl"
        self._active_progress: dict[str, Any] | None = None
        self._history_lock = threading.Lock()
        self._lock = threading.Lock()

    def _append_history_event(self, event: dict[str, Any]) -> None:
        """Append OCR event to on-disk history and keep a bounded tail."""
        event_line = json.dumps(event, ensure_ascii=False)
        with self._history_lock:
            try:
                self._history_path.parent.mkdir(parents=True, exist_ok=True)
                with self._history_path.open("a", encoding="utf-8") as handle:
                    handle.write(event_line + "\n")
            except OSError:
                return
            self._trim_history_tail_locked(max_events=500)

    def _trim_history_tail_locked(self, max_events: int = 500) -> None:
        """Rewrite history file with only the most recent max_events lines."""
        try:
            with self._history_path.open("r", encoding="utf-8") as handle:
                tail = deque((line.rstrip("\n") for line in handle if line.strip()), maxlen=max_events)
        except OSError:
            return

        temp_path: Path | None = None
        try:
            with tempfile.NamedTemporaryFile(
                mode="w",
                encoding="utf-8",
                dir=str(self._history_path.parent),
                delete=False,
                prefix=".ocr-history.",
                suffix=".tmp",
            ) as handle:
                temp_path = Path(handle.name)
                if tail:
                    handle.write("\n".join(tail))
                    handle.write("\n")
            os.replace(str(temp_path), str(self._history_path))
        except OSError:
            if temp_path is not None:
                try:
                    temp_path.unlink(missing_ok=True)
                except OSError:
                    pass

    def _log(self, message: str) -> None:
        """Log a status message."""
        self.status_callback(message)

    def _on_new_file(self, path: Path) -> None:
        """Handle a new file from the watcher."""
        if self.processor.is_processable(path):
            self._log(f"New file detected: {path.name}")
            self.processor.process(path)
        else:
            self._log(f"Ignoring non-processable file: {path.name}")

    def _write_progress(self) -> None:
        """Persist active OCR progress for UI/API consumption."""
        if self._active_progress is None:
            try:
                if self._progress_path.exists():
                    self._progress_path.unlink()
            except OSError:
                pass
            return
        data = dict(self._active_progress)
        data["updated_at"] = time.time()
        try:
            self._progress_path.write_text(json.dumps(data), encoding="utf-8")
        except OSError:
            pass

    def _set_active_progress(self, data: dict[str, Any]) -> None:
        """Set current OCR progress state."""
        with self._lock:
            if self._active_progress is None:
                self._active_progress = {}
            self._active_progress.update(data)
            self._active_progress["active"] = True
            if "started_at" not in self._active_progress:
                self._active_progress["started_at"] = time.time()
            self._write_progress()

    def _clear_active_progress(self) -> None:
        """Clear current OCR progress state."""
        with self._lock:
            self._active_progress = None
            self._write_progress()

    def _on_progress(self, data: dict[str, Any]) -> None:
        """Receive progress events from OCR processor."""
        if data.get("status") == "done":
            self._append_history_event({
                "timestamp": time.time(),
                "status": "done",
                "series": data.get("series"),
                "volume": data.get("volume"),
                "relative_cbz": data.get("relative_cbz"),
                "percent": data.get("percent"),
            })
            self._set_active_progress(data)
            self._clear_active_progress()
            return
        if data.get("status") == "error":
            self._append_history_event({
                "timestamp": time.time(),
                "status": "error",
                "series": data.get("series"),
                "volume": data.get("volume"),
                "relative_cbz": data.get("relative_cbz"),
                "percent": data.get("percent"),
                "error": data.get("error"),
            })
            self._set_active_progress(data)
            # Keep last error snapshot briefly so UI can show failure.
            return
        self._set_active_progress(data)

    def _ocr_candidates(self) -> list[Path]:
        """Find library CBZ files missing mokuro sidecars."""
        library_path = self.storage_path / "library"
        if not library_path.exists():
            return []
        candidates = [
            p for p in library_path.rglob("*.cbz")
            if p.is_file()
            and self.processor.needs_mokuro_sidecar(p)
        ]
        candidates.sort(key=lambda p: self._fifo_sort_key(p, library_path))
        return candidates

    def _thumbnail_candidates(self) -> list[Path]:
        """Find library CBZ files missing thumbnails."""
        library_path = self.storage_path / "library"
        if not library_path.exists():
            return []
        return [
            p for p in library_path.rglob("*.cbz")
            if p.is_file() and self.processor.needs_thumbnail(p)
        ]

    @staticmethod
    def _fifo_sort_key(path: Path, library_path: Path) -> tuple[float, str]:
        """Sort by created time (best effort), then relative path for tie-breaks."""
        try:
            st = path.stat()
            created = getattr(st, "st_birthtime", None)
            if created is None:
                created = st.st_mtime
        except OSError:
            created = 0.0

        try:
            rel = path.relative_to(library_path).as_posix()
        except ValueError:
            rel = path.as_posix()
        return float(created), rel

    def _scan_ocr_once(self) -> None:
        """Process library CBZ files with missing mokuro sidecars."""
        ocr_candidates = self._ocr_candidates()
        if ocr_candidates:
            self._log(f"Found {len(ocr_candidates)} CBZ files missing mokuro sidecars")

        for path in ocr_candidates:
            with self._lock:
                if path in self._inflight_ocr:
                    continue
                self._inflight_ocr.add(path)
            try:
                rel_cbz = str(path.relative_to(self.storage_path / "library"))
                rel_series = str(path.parent.relative_to(self.storage_path / "library"))
                self._set_active_progress({
                    "series": rel_series,
                    "volume": path.stem,
                    "relative_cbz": rel_cbz,
                    "percent": 0,
                    "eta_seconds": None,
                    "status": "running",
                })
                self.processor.process_library_ocr(path)
            finally:
                with self._lock:
                    self._inflight_ocr.discard(path)
                self._clear_active_progress()

    def _scan_thumbnails_once(self) -> None:
        """Process library CBZ files with missing thumbnails."""
        thumb_candidates = self._thumbnail_candidates()
        if thumb_candidates:
            self._log(f"Found {len(thumb_candidates)} CBZ files missing thumbnails")

        for path in thumb_candidates:
            with self._lock:
                if path in self._inflight_thumbs:
                    continue
                self._inflight_thumbs.add(path)
            try:
                self.processor.process_library_thumbnail(path)
            finally:
                with self._lock:
                    self._inflight_thumbs.discard(path)

    def _wait_poll_interval(self) -> None:
        """Sleep for poll interval with stop checks."""
        for _ in range(max(1, int(self.poll_interval * 10))):
            if not self._running:
                break
            if self._paused:
                time.sleep(0.5)
                continue
            time.sleep(0.1)

    def pause(self) -> None:
        """Pause OCR work loops."""
        with self._lock:
            self._paused = True
        self._log("OCR worker paused")

    def resume(self) -> None:
        """Resume OCR work loops."""
        with self._lock:
            self._paused = False
        self._log("OCR worker resumed")

    def is_paused(self) -> bool:
        """Check if OCR worker is paused."""
        with self._lock:
            return self._paused

    def _run_ocr_loop(self) -> None:
        """Background OCR sidecar loop."""
        while self._running:
            if self.is_paused():
                time.sleep(0.5)
                continue
            try:
                self._scan_ocr_once()
            except Exception as e:
                self._log(f"OCR scan error: {e}")
            self._wait_poll_interval()

    def _run_thumbnail_loop(self) -> None:
        """Background thumbnail loop."""
        while self._running:
            if self.is_paused():
                time.sleep(0.5)
                continue
            try:
                self._scan_thumbnails_once()
            except Exception as e:
                self._log(f"Thumbnail scan error: {e}")
            self._wait_poll_interval()

    def _cleanup_old_processing(self, max_age_hours: int = 24) -> int:
        """Remove stale .processing directories older than max_age_hours."""
        processing_dir = self.storage_path / ".processing"
        if not processing_dir.is_dir():
            return 0

        cutoff = time.time() - (max_age_hours * 3600)
        removed = 0
        for child in processing_dir.iterdir():
            try:
                mtime = child.stat().st_mtime
                if mtime < cutoff:
                    if child.is_dir():
                        shutil.rmtree(child, ignore_errors=True)
                    else:
                        child.unlink(missing_ok=True)
                    removed += 1
                    self._log(f"Removed stale processing folder: {child}")
            except (OSError, ValueError):
                continue
        return removed

    def _remove_corrupt_sidecars(self) -> int:
        """Remove invalid mokuro sidecar files from library and return count."""
        library_path = self.storage_path / "library"
        removed = 0
        for path in sorted(library_path.rglob("*.mokuro*")):
            if not path.is_file():
                continue
            if not (path.name.endswith(".mokuro") or path.name.endswith(".mokuro.gz")):
                continue
            if self.processor.is_valid_mokuro_sidecar(path):
                continue
            try:
                path.unlink()
                removed += 1
                self._log(f"Removed corrupt mokuro sidecar: {path}")
            except OSError as e:
                self._log(f"Failed to remove corrupt sidecar {path}: {e}")
        return removed

    def start(self, background: bool = True) -> None:
        """Start the OCR worker.

        Args:
            background: If True, run in background thread.
        """
        library_path = self.storage_path / "library"
        library_path.mkdir(parents=True, exist_ok=True)

        removed = self._remove_corrupt_sidecars()
        if removed:
            self._log(f"Removed {removed} corrupt mokuro sidecar file(s) at startup")

        self._running = True
        self._log("OCR worker starting...")

        # Remove stale processing directories left by crashes or forced exits
        self._cleanup_old_processing()

        if background:
            self._ocr_thread = threading.Thread(
                target=self._run_ocr_loop,
                daemon=True,
                name="ocr-sidecar-worker",
            )
            self._thumb_thread = threading.Thread(
                target=self._run_thumbnail_loop,
                daemon=True,
                name="ocr-thumbnail-worker",
            )
            self._ocr_thread.start()
            self._thumb_thread.start()
            self._log("OCR worker started in background (sidecar + thumbnail loops)")
        else:
            self._thumb_thread = threading.Thread(
                target=self._run_thumbnail_loop,
                daemon=True,
                name="ocr-thumbnail-worker",
            )
            self._thumb_thread.start()
            self._run_ocr_loop()

    def stop(self) -> None:
        """Stop the OCR worker."""
        self._running = False
        if self._ocr_thread:
            self._ocr_thread.join(timeout=5.0)
        if self._thumb_thread:
            self._thumb_thread.join(timeout=5.0)
        self._log("OCR worker stopped")

    @property
    def is_running(self) -> bool:
        """Check if the worker is running."""
        return self._running
