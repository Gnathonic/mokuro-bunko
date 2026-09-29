"""A slow library scan is not redone for every change.

Eight OCR machines landing sidecars invalidated the index several times a
minute, and on a 12k-volume library on a network share every reader then
walked the whole tree again.
"""

from __future__ import annotations

from pathlib import Path

from mokuro_bunko.library_index import RESCAN_FACTOR, LibraryIndexCache


def _cache(tmp_path: Path) -> tuple[LibraryIndexCache, list[int]]:
    (tmp_path / "S").mkdir()
    (tmp_path / "S" / "V.cbz").write_bytes(b"x")
    cache = LibraryIndexCache(tmp_path, ttl=30.0)
    scans: list[int] = []
    real = cache._scan_library

    def counted():  # type: ignore[no-untyped-def]
        scans.append(1)
        return real()

    cache._scan_library = counted  # type: ignore[method-assign]
    return cache, scans


def test_a_cheap_scan_is_redone_after_every_change(tmp_path: Path) -> None:
    cache, scans = _cache(tmp_path)
    cache.get_snapshot()
    cache.invalidate()
    cache.get_snapshot()
    assert len(scans) == 2


def test_a_slow_scan_serves_its_snapshot_through_a_burst_of_changes(tmp_path: Path) -> None:
    cache, scans = _cache(tmp_path)
    cache.get_snapshot()
    cache._scan_seconds = 7.0  # the live library's walk
    for _ in range(20):
        cache.invalidate()
        cache.get_snapshot()
    assert len(scans) == 1


def test_a_stale_slow_snapshot_is_rescanned_once_old_enough(tmp_path: Path) -> None:
    cache, scans = _cache(tmp_path)
    cache.get_snapshot()
    cache._scan_seconds = 7.0
    cache.invalidate()
    cache._snapshot_time -= RESCAN_FACTOR * 7.0 + 1
    cache.get_snapshot()
    assert len(scans) == 2
