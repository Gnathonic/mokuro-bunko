"""Compiled files are published atomically and only when they changed."""

from __future__ import annotations

import os
from pathlib import Path

import pytest

from mokuro_bunko.metadata.files import MetadataWriteBusy, write_if_changed
from mokuro_bunko.webdav.resources import _PATH_WRITE_LOCKS


@pytest.fixture(autouse=True)
def _clean_global_locks() -> None:
    """The registry is module-global; keep tests independent."""
    _PATH_WRITE_LOCKS._locks.clear()


class TestWriteIfChanged:
    def test_creates_a_missing_file(self, tmp_path: Path) -> None:
        target = tmp_path / "series.json"
        assert write_if_changed(target, b'{"version":2}') is True
        assert target.read_bytes() == b'{"version":2}'

    def test_creates_missing_parent_directories(self, tmp_path: Path) -> None:
        target = tmp_path / "Dr Stone" / "series.json"
        assert write_if_changed(target, b"{}") is True
        assert target.read_bytes() == b"{}"

    def test_identical_bytes_leave_the_file_untouched(self, tmp_path: Path) -> None:
        target = tmp_path / "catalog.json"
        write_if_changed(target, b"same")
        before = target.stat()
        os.utime(target, (before.st_atime, before.st_mtime - 60))
        stamped = target.stat().st_mtime

        assert write_if_changed(target, b"same") is False
        assert target.stat().st_mtime == stamped

    def test_changed_bytes_are_rewritten(self, tmp_path: Path) -> None:
        target = tmp_path / "catalog.json"
        write_if_changed(target, b"one")
        assert write_if_changed(target, b"two") is True
        assert target.read_bytes() == b"two"

    def test_no_temporary_files_are_left_behind(self, tmp_path: Path) -> None:
        target = tmp_path / "catalog.json"
        write_if_changed(target, b"one")
        write_if_changed(target, b"two")
        assert [p.name for p in tmp_path.iterdir()] == ["catalog.json"]

    def test_a_locked_path_raises_instead_of_writing(self, tmp_path: Path) -> None:
        target = tmp_path / "series.json"
        write_if_changed(target, b"one")
        assert _PATH_WRITE_LOCKS.acquire(target)
        try:
            with pytest.raises(MetadataWriteBusy):
                write_if_changed(target, b"two")
        finally:
            _PATH_WRITE_LOCKS.release(target)
        assert target.read_bytes() == b"one"

    def test_a_locked_ancestor_also_blocks(self, tmp_path: Path) -> None:
        folder = tmp_path / "Dr Stone"
        folder.mkdir()
        target = folder / "series.json"
        assert _PATH_WRITE_LOCKS.acquire(folder)
        try:
            with pytest.raises(MetadataWriteBusy):
                write_if_changed(target, b"{}")
        finally:
            _PATH_WRITE_LOCKS.release(folder)
        assert not target.exists()
