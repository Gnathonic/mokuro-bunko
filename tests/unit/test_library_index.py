"""Unit tests for shared library index cache."""

from __future__ import annotations

import os
from pathlib import Path

from mokuro_bunko.library_index import LibraryIndexCache


def test_library_index_builds_series_and_pending_sets(tmp_path: Path) -> None:
    """Index should capture series, volumes, pending OCR, and pending thumbs."""
    library = tmp_path / "library"
    series = library / "Series A"
    series.mkdir(parents=True)

    (series / "v01.cbz").write_bytes(b"cbz")
    (series / "v02.cbz").write_bytes(b"cbz")
    (series / "v02.mokuro.gz").write_text("sidecar", encoding="utf-8")
    (series / "v02.webp").write_bytes(b"webp")
    (series / "v01.nocover").write_text("", encoding="utf-8")

    index = LibraryIndexCache(library, ttl=60.0)
    snapshot = index.get_snapshot()

    assert len(snapshot.series) == 1
    assert snapshot.series[0].name == "Series A"
    assert [v.name for v in snapshot.series[0].volumes] == ["v01", "v02"]
    assert snapshot.pending_ocr == (("Series A", "v01"),)
    assert snapshot.pending_thumbnails == 0


def test_library_index_invalidate_forces_rescan(tmp_path: Path) -> None:
    """Invalidate should drop stale data and include new files."""
    library = tmp_path / "library"
    series = library / "Series B"
    series.mkdir(parents=True)
    (series / "v01.cbz").write_bytes(b"cbz")

    index = LibraryIndexCache(library, ttl=999.0)
    first = index.get_snapshot()
    assert first.pending_ocr == (("Series B", "v01"),)

    (series / "v01.mokuro").write_text("done", encoding="utf-8")
    index.invalidate()
    second = index.get_snapshot()
    assert second.pending_ocr == ()


def test_library_index_scans_nested_directories(tmp_path: Path) -> None:
    """Recursive walk should include nested series paths."""
    library = tmp_path / "library"
    nested = library / "Group" / "Series C"
    nested.mkdir(parents=True)
    (nested / "v03.cbz").write_bytes(b"cbz")

    index = LibraryIndexCache(library, ttl=60.0)
    snapshot = index.get_snapshot()

    assert len(snapshot.series) == 1
    assert snapshot.series[0].name == "Group/Series C"
    assert snapshot.pending_ocr == (("Group/Series C", "v03"),)


def test_library_index_pending_ocr_is_fifo_by_created_time(tmp_path: Path) -> None:
    """Pending OCR list should be FIFO by created-time proxy (mtime fallback)."""
    library = tmp_path / "library"
    series = library / "Series D"
    series.mkdir(parents=True)
    first = series / "v01.cbz"
    second = series / "v02.cbz"
    first.write_bytes(b"cbz")
    second.write_bytes(b"cbz")

    # Force deterministic ordering in tests via mtime.
    os.utime(first, (1000, 1000))
    os.utime(second, (2000, 2000))

    index = LibraryIndexCache(library, ttl=60.0)
    snapshot = index.get_snapshot()
    assert snapshot.pending_ocr == (("Series D", "v01"), ("Series D", "v02"))
