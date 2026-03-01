"""Filesystem watcher for PROPFIND cache invalidation.

Watches the library directory for new/deleted files that affect PROPFIND
responses (CBZ archives, mokuro sidecars, thumbnails) and triggers a
debounced cache refresh.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Callable, Optional

try:
    from watchdog.events import FileSystemEvent, FileSystemEventHandler
    from watchdog.observers import Observer

    WATCHDOG_AVAILABLE = True
except ImportError:
    WATCHDOG_AVAILABLE = False
    Observer = None  # type: ignore
    FileSystemEventHandler = object  # type: ignore
    FileSystemEvent = None  # type: ignore

# Extensions that matter for PROPFIND responses (existence, not content).
_RELEVANT_SUFFIXES = frozenset({".cbz", ".mokuro", ".gz", ".webp"})


def _is_relevant(path_str: str, is_directory: bool) -> bool:
    """Return True if this event should trigger a cache refresh."""
    if is_directory:
        return True
    p = Path(path_str)
    # .mokuro.gz has suffix .gz; check the combined stem too
    if p.suffix in _RELEVANT_SUFFIXES:
        return True
    if p.name.endswith(".mokuro.gz"):
        return True
    return False


class LibraryWatcher:
    """Watch the library directory and call *on_change* for relevant filesystem events."""

    def __init__(
        self,
        watch_path: Path,
        on_change: Callable[[], None],
    ) -> None:
        self.watch_path = watch_path
        self.on_change = on_change
        self._observer: Optional[Observer] = None  # type: ignore

    def start(self) -> None:
        if not WATCHDOG_AVAILABLE:
            print(
                "[FS-WATCHER] watchdog not installed; filesystem watching disabled",
                file=sys.stderr,
                flush=True,
            )
            return

        self.watch_path.mkdir(parents=True, exist_ok=True)

        handler = _LibraryEventHandler(self.on_change)
        self._observer = Observer()
        self._observer.schedule(handler, str(self.watch_path), recursive=True)
        self._observer.daemon = True
        self._observer.start()
        print(
            f"[FS-WATCHER] Watching {self.watch_path}",
            file=sys.stderr,
            flush=True,
        )

    def stop(self) -> None:
        if self._observer is not None:
            self._observer.stop()
            self._observer.join(timeout=5.0)
            self._observer = None


if WATCHDOG_AVAILABLE:

    class _LibraryEventHandler(FileSystemEventHandler):  # type: ignore
        def __init__(self, on_change: Callable[[], None]) -> None:
            super().__init__()
            self._on_change = on_change

        def on_created(self, event: FileSystemEvent) -> None:  # type: ignore
            if _is_relevant(event.src_path, event.is_directory):
                self._on_change()

        def on_deleted(self, event: FileSystemEvent) -> None:  # type: ignore
            if _is_relevant(event.src_path, event.is_directory):
                self._on_change()

        def on_moved(self, event: FileSystemEvent) -> None:  # type: ignore
            src_relevant = _is_relevant(event.src_path, event.is_directory)
            dest_relevant = _is_relevant(
                getattr(event, "dest_path", event.src_path),
                event.is_directory,
            )
            if src_relevant or dest_relevant:
                self._on_change()
