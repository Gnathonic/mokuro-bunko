"""The watcher: which events fire, what path they carry, how they route."""

from __future__ import annotations

from pathlib import Path

import pytest

from mokuro_bunko.middleware import fs_watcher
from mokuro_bunko.middleware.fs_watcher import classify_change


class TestClassifyChange:
    LIB = Path("/srv/data/library")

    def test_volume_file_inside_a_series_maps_to_that_series(self) -> None:
        assert classify_change(self.LIB, str(self.LIB / "Dr Stone" / "Volume 01.cbz")) == (
            "series",
            "Dr Stone",
        )

    def test_nested_file_still_maps_to_the_top_level_series(self) -> None:
        assert classify_change(self.LIB, str(self.LIB / "Dr Stone" / "extras" / "x.webp")) == (
            "series",
            "Dr Stone",
        )

    def test_top_level_entry_is_a_library_level_change(self) -> None:
        # A series folder itself appearing/disappearing needs the full pass
        # (it is what prunes deleted series from the catalog).
        assert classify_change(self.LIB, str(self.LIB / "Dr Stone")) == ("library", None)

    def test_library_root_itself_is_a_library_level_change(self) -> None:
        assert classify_change(self.LIB, str(self.LIB)) == ("library", None)

    def test_path_outside_the_root_is_a_library_level_change(self) -> None:
        assert classify_change(self.LIB, "/somewhere/else/file.cbz") == ("library", None)

    def test_generated_thumbnails_are_ignored(self) -> None:
        assert classify_change(self.LIB, str(self.LIB / "thumbnails" / "x.webp")) == (
            "ignore",
            None,
        )


@pytest.mark.skipif(not fs_watcher.WATCHDOG_AVAILABLE, reason="watchdog not installed")
class TestHandlerPathDelivery:
    class _Event:
        def __init__(self, src_path: str, dest_path: str | None = None) -> None:
            self.src_path = src_path
            self.dest_path = dest_path
            self.is_directory = False

    def _handler(self) -> tuple[object, list[str]]:
        seen: list[str] = []
        return fs_watcher._LibraryEventHandler(seen.append), seen

    def test_created_relevant_file_delivers_its_path(self) -> None:
        handler, seen = self._handler()
        handler.on_created(self._Event("/lib/Dr Stone/Volume 01.cbz"))
        assert seen == ["/lib/Dr Stone/Volume 01.cbz"]

    def test_created_tmp_file_is_filtered(self) -> None:
        handler, seen = self._handler()
        handler.on_created(self._Event("/lib/Dr Stone/.Volume 01.cbz.upload-x.tmp"))
        assert seen == []

    def test_atomic_upload_rename_delivers_the_destination(self) -> None:
        handler, seen = self._handler()
        handler.on_moved(
            self._Event("/lib/Dr Stone/.Volume 01.cbz.upload-x.tmp", "/lib/Dr Stone/Volume 01.cbz")
        )
        assert seen == ["/lib/Dr Stone/Volume 01.cbz"]

    def test_cross_series_move_delivers_both_sides(self) -> None:
        handler, seen = self._handler()
        handler.on_moved(self._Event("/lib/A/v.cbz", "/lib/B/v.cbz"))
        assert seen == ["/lib/A/v.cbz", "/lib/B/v.cbz"]

    def test_deleted_relevant_file_delivers_its_path(self) -> None:
        handler, seen = self._handler()
        handler.on_deleted(self._Event("/lib/Dr Stone/Volume 01.mokuro"))
        assert seen == ["/lib/Dr Stone/Volume 01.mokuro"]
