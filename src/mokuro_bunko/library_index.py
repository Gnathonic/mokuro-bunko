"""Shared cached index of the library filesystem tree."""

from __future__ import annotations

import os
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


@dataclass(frozen=True)
class VolumeSnapshot:
    """Indexed metadata for a single logical volume stem."""

    name: str
    has_cbz: bool
    has_mokuro: bool
    has_mokuro_gz: bool
    cover: Optional[str]


@dataclass(frozen=True)
class SeriesSnapshot:
    """Indexed metadata for a single series folder."""

    name: str
    cover: Optional[str]
    volumes: tuple[VolumeSnapshot, ...]


@dataclass(frozen=True)
class LibrarySnapshot:
    """Immutable snapshot returned by the shared library index."""

    series: tuple[SeriesSnapshot, ...]
    pending_ocr: tuple[tuple[str, str], ...]
    pending_thumbnails: int

    def series_by_name(self, name: str) -> Optional[SeriesSnapshot]:
        """Return a named series snapshot when present."""
        for series in self.series:
            if series.name == name:
                return series
        return None


class LibraryIndexCache:
    """Time-based cached scanner for `storage/library`."""

    def __init__(self, library_path: Path, ttl: float = 30.0) -> None:
        self.library_path = library_path
        self.ttl = ttl
        self._lock = threading.Lock()
        self._snapshot: Optional[LibrarySnapshot] = None
        self._snapshot_time = 0.0

    def invalidate(self) -> None:
        """Drop current snapshot so next read rescans the filesystem."""
        with self._lock:
            self._snapshot = None
            self._snapshot_time = 0.0

    def get_snapshot(self) -> LibrarySnapshot:
        """Return a recent snapshot, rescanning when stale."""
        now = time.monotonic()
        with self._lock:
            if self._snapshot is not None and (now - self._snapshot_time) < self.ttl:
                return self._snapshot

        snapshot = self._scan_library()
        with self._lock:
            self._snapshot = snapshot
            self._snapshot_time = time.monotonic()
            return snapshot

    def _scan_library(self) -> LibrarySnapshot:
        """Scan the entire library tree once (recursive walk) and build an immutable snapshot."""
        if not self.library_path.is_dir():
            return LibrarySnapshot(series=(), pending_ocr=(), pending_thumbnails=0)

        series_items: list[SeriesSnapshot] = []
        pending_ocr: list[tuple[float, str, str]] = []
        pending_thumbnails = 0

        try:
            library_str = str(self.library_path)
            for dirpath, dirnames, filenames_list in os.walk(library_str):
                # Ignore hidden subdirectories while still traversing non-hidden paths.
                dirnames[:] = [d for d in sorted(dirnames) if not d.startswith(".")]
                filenames = set(filenames_list)
                current_dir = Path(dirpath)
                try:
                    series_name = current_dir.relative_to(self.library_path).as_posix()
                except ValueError:
                    continue

                if series_name.startswith("."):
                    continue

                volume_names: set[str] = set()
                standalone_cover: Optional[str] = None

                for file_name in sorted(filenames):
                    lower_name = file_name.lower()
                    if lower_name.endswith(".cbz"):
                        volume_names.add(file_name[:-len(".cbz")])
                    elif lower_name.endswith(".mokuro.gz"):
                        volume_names.add(file_name[:-len(".mokuro.gz")])
                    elif lower_name.endswith(".mokuro"):
                        volume_names.add(file_name[:-len(".mokuro")])
                    elif lower_name.endswith(".webp") and standalone_cover is None:
                        standalone_cover = file_name

                volumes: list[VolumeSnapshot] = []
                series_cover: Optional[str] = None

                for volume_name in sorted(volume_names):
                    has_cbz = f"{volume_name}.cbz" in filenames
                    has_mokuro = f"{volume_name}.mokuro" in filenames
                    has_mokuro_gz = f"{volume_name}.mokuro.gz" in filenames
                    has_webp = f"{volume_name}.webp" in filenames
                    cover = f"{series_name}/{volume_name}.webp" if has_webp else None

                    if series_cover is None and cover is not None:
                        series_cover = cover

                    volumes.append(
                        VolumeSnapshot(
                            name=volume_name,
                            has_cbz=has_cbz,
                            has_mokuro=has_mokuro,
                            has_mokuro_gz=has_mokuro_gz,
                            cover=cover,
                        )
                    )

                    if has_cbz and not has_mokuro and not has_mokuro_gz:
                        cbz_path = current_dir / f"{volume_name}.cbz"
                        pending_ocr.append((self._created_timestamp(cbz_path), series_name, volume_name))
                    if has_cbz and f"{volume_name}.webp" not in filenames and f"{volume_name}.nocover" not in filenames:
                        pending_thumbnails += 1

                if series_cover is None and standalone_cover is not None:
                    series_cover = f"{series_name}/{standalone_cover}"

                if volumes:
                    series_items.append(
                        SeriesSnapshot(
                            name=series_name,
                            cover=series_cover,
                            volumes=tuple(volumes),
                        )
                    )
        except OSError:
            return LibrarySnapshot(series=(), pending_ocr=(), pending_thumbnails=0)

        pending_ocr.sort(key=lambda item: (item[0], item[1], item[2]))
        return LibrarySnapshot(
            series=tuple(series_items),
            pending_ocr=tuple((series_name, volume_name) for _, series_name, volume_name in pending_ocr),
            pending_thumbnails=pending_thumbnails,
        )

    @staticmethod
    def _created_timestamp(path: Path) -> float:
        """Best-effort creation time for FIFO ordering."""
        try:
            st = path.stat()
        except OSError:
            return 0.0
        birth = getattr(st, "st_birthtime", None)
        if birth is not None:
            return float(birth)
        return float(st.st_mtime)
