"""Shared cached index of the library filesystem tree."""

from __future__ import annotations

import os
import threading
import time
from dataclasses import dataclass
from pathlib import Path

from mokuro_bunko.ocr.generations import split_layer_sidecar


@dataclass(frozen=True)
class VolumeSnapshot:
    """Indexed metadata for a single logical volume stem."""

    name: str
    has_cbz: bool
    has_mokuro: bool
    has_mokuro_gz: bool
    cover: str | None
    # Layer postfixes OBSERVED beside the archive, sorted: the `<id>` of
    # every `<name>.<id>.mokuro[.gz]` whose id the reader would read as a
    # layer (e.g. ("hayai-nova",) for `<name>.hayai-nova.mokuro`).
    #
    # What is on disk, NOT what is configured. The index has no handle on
    # the settings and should not grow one: a generation may be renamed or
    # removed while its files stay, another server may have written a layer
    # this one never heard of, and a reader may have pushed an edit. The
    # config decides what is still pending (`OCRProcessor.missing_
    # generations`); this says what is there.
    sidecars: tuple[str, ...] = ()


@dataclass(frozen=True)
class SeriesSnapshot:
    """Indexed metadata for a single series folder."""

    name: str
    cover: str | None
    volumes: tuple[VolumeSnapshot, ...]


@dataclass(frozen=True)
class LibrarySnapshot:
    """Immutable snapshot returned by the shared library index."""

    series: tuple[SeriesSnapshot, ...]
    # (series, volume) of every archive without the reader-facing sidecar
    # (`Volume.mokuro`). MEMBERSHIP ONLY, in the walk's name order: it feeds
    # the health endpoint's count. It is not the OCR queue and says nothing
    # about processing order; the queue page gets that from the worker
    # (`OCRWorker.pending_jobs`, ordered by `ocr.job_order.order_jobs`).
    pending_ocr: tuple[tuple[str, str], ...]
    pending_thumbnails: int

    def series_by_name(self, name: str) -> SeriesSnapshot | None:
        """Return a named series snapshot when present."""
        for series in self.series:
            if series.name == name:
                return series
        return None


# A scan slower than this is not repeated for every change: after an
# invalidation the old snapshot is served until it is RESCAN_FACTOR of its own
# scans old. On a 12k-volume library on a network share a scan takes seconds,
# and eight OCR machines landing sidecars invalidated it several times a
# minute -- every reader paid for a fresh walk of the whole tree.
SLOW_SCAN_SECONDS = 0.1
RESCAN_FACTOR = 4.0


class LibraryIndexCache:
    """Time-based cached scanner for `storage/library`."""

    def __init__(self, library_path: Path, ttl: float = 30.0) -> None:
        self.library_path = library_path
        self.ttl = ttl
        self._lock = threading.Lock()
        self._snapshot: LibrarySnapshot | None = None
        self._snapshot_time = 0.0
        # Set by `invalidate`: the snapshot is known to be behind the disk.
        self._stale = False
        # How long the last scan took (see SLOW_SCAN_SECONDS).
        self._scan_seconds = 0.0
        # How many scans have produced a snapshot: a cheap "did it change?"
        # for the queue page's fingerprint.
        self.scans = 0

    def invalidate(self) -> None:
        """The library changed: the next read rescans -- soon, on a slow library.

        A cheap scan is redone on the next read. A slow one keeps serving the
        snapshot it has until that is `RESCAN_FACTOR` scans old, so a burst
        of changes costs one walk, not one per change.
        """
        with self._lock:
            self._stale = True
            if self._scan_seconds < SLOW_SCAN_SECONDS:
                self._snapshot = None
                self._snapshot_time = 0.0

    def get_snapshot(self) -> LibrarySnapshot:
        """Return a recent snapshot, rescanning when stale."""
        return self.get_snapshot_counted()[0]

    def get_snapshot_counted(self) -> tuple[LibrarySnapshot, int]:
        """`get_snapshot`, and the scan count that produced it -- read together.

        Under one lock, so a rescan by another thread can never pair a new
        count with an old snapshot (a pairing a fingerprint would then keep).
        """
        now = time.monotonic()
        with self._lock:
            if self._snapshot is not None:
                age = now - self._snapshot_time
                if not self._stale and age < self.ttl:
                    return self._snapshot, self.scans
                if self._stale and age < RESCAN_FACTOR * self._scan_seconds:
                    return self._snapshot, self.scans

        started = time.monotonic()
        snapshot = self._scan_library()
        with self._lock:
            self._scan_seconds = time.monotonic() - started
            self._snapshot = snapshot
            self._snapshot_time = time.monotonic()
            self._stale = False
            self.scans += 1
            return snapshot, self.scans

    def cached_snapshot(self) -> LibrarySnapshot | None:
        """The last snapshot, however old, without ever scanning (None: none yet)."""
        with self._lock:
            return self._snapshot

    def _scan_library(self) -> LibrarySnapshot:
        """Scan the entire library tree once (recursive walk) and build an immutable snapshot."""
        if not self.library_path.is_dir():
            return LibrarySnapshot(series=(), pending_ocr=(), pending_thumbnails=0)

        series_items: list[SeriesSnapshot] = []
        pending_ocr: list[tuple[str, str]] = []
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

                # Index logical volumes from CBZ files only, so sidecar-only stems
                # left behind by plain filesystem operations do not become phantom volumes.
                volume_names: set[str] = set()

                for file_name in sorted(filenames):
                    lower_name = file_name.lower()
                    if lower_name.endswith(".cbz"):
                        volume_names.add(file_name[:-len(".cbz")])

                # Every layer sidecar in this directory, grouped by the stem
                # it belongs to, in ONE pass: a hundred-volume folder would
                # otherwise re-read the whole name list once per volume.
                layers_by_stem: dict[str, set[str]] = {}
                for file_name in filenames:
                    split = split_layer_sidecar(file_name)
                    if split is None:
                        continue
                    # ``Volume 01.5.mokuro`` is volume 1.5's own OCR when that
                    # archive is here, not a layer called ``5`` of volume 1.
                    if f"{split[0]}.{split[1]}" in volume_names:
                        continue
                    layers_by_stem.setdefault(split[0], set()).add(split[1])

                volumes: list[VolumeSnapshot] = []
                series_cover: str | None = None

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
                            sidecars=tuple(sorted(layers_by_stem.get(volume_name, ()))),
                        )
                    )

                    if has_cbz and not has_mokuro and not has_mokuro_gz:
                        pending_ocr.append((series_name, volume_name))
                    if has_cbz and f"{volume_name}.webp" not in filenames and f"{volume_name}.nocover" not in filenames:
                        pending_thumbnails += 1

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

        return LibrarySnapshot(
            series=tuple(series_items),
            pending_ocr=tuple(pending_ocr),
            pending_thumbnails=pending_thumbnails,
        )
