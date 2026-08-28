"""Filesystem watcher for PROPFIND cache invalidation.

Watches the library directory for new/deleted files that affect PROPFIND
responses (CBZ archives, mokuro sidecars, thumbnails) and triggers a
debounced cache refresh.
"""

from __future__ import annotations

import os
import sys
from collections.abc import Callable
from pathlib import Path

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


def classify_change(library_root: Path, path_str: str) -> tuple[str, str | None]:
    """Route one filesystem change to the metadata work it implies.

    Returns ``("series", <folder name>)`` for a file inside a series folder
    (recompile just that series), ``("library", None)`` for anything at or
    above the top level — a series folder appearing, disappearing or moving
    needs the full pass, which is what prunes deleted series from the
    catalog — and ``("ignore", None)`` for generated content that never
    feeds compilation. Unclassifiable paths fall back to ``"library"``:
    a needless full pass is cheap, a missed change is not.
    """
    try:
        relative = Path(path_str).relative_to(library_root)
    except ValueError:
        return ("library", None)
    parts = relative.parts
    if len(parts) < 2:
        return ("library", None)
    # Generated per-series thumbnails live under library/thumbnails/; they
    # are PROPFIND-visible but carry nothing the metadata compiler reads.
    if parts[0] == "thumbnails":
        return ("ignore", None)
    return ("series", parts[0])


class LibraryWatcher:
    """Watch the library directory and call *on_change* with each relevant changed path."""

    def __init__(
        self,
        watch_path: Path,
        on_change: Callable[[str], None],
    ) -> None:
        self.watch_path = watch_path
        self.on_change = on_change
        self._observer: Observer | None = None  # type: ignore

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

    class _LibraryEventHandler(FileSystemEventHandler):
        def __init__(self, on_change: Callable[[str], None]) -> None:
            super().__init__()
            self._on_change = on_change

        def on_created(self, event: FileSystemEvent) -> None:
            path = os.fsdecode(event.src_path)
            if _is_relevant(path, event.is_directory):
                self._on_change(path)

        def on_deleted(self, event: FileSystemEvent) -> None:
            path = os.fsdecode(event.src_path)
            if _is_relevant(path, event.is_directory):
                self._on_change(path)

        def on_moved(self, event: FileSystemEvent) -> None:
            # Each relevant side gets its own callback: a cross-series move
            # changes two series; an atomic upload's tmp→final rename has an
            # irrelevant source and delivers only the destination.
            src = os.fsdecode(event.src_path)
            dest = os.fsdecode(getattr(event, "dest_path", event.src_path))
            if _is_relevant(src, event.is_directory):
                self._on_change(src)
            if dest != src and _is_relevant(dest, event.is_directory):
                self._on_change(dest)
