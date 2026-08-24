"""Contract §2: a series folder compiles into the reader's volume entries."""

from __future__ import annotations

import gzip
import json
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.metadata.compiler import (
    SeriesFolder,
    compile_series_volumes,
    iter_series_folders,
)


def write_cbz(path: Path, pages: int = 2) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as archive:
        for index in range(pages):
            archive.writestr(f"{index:03d}.jpg", b"fake image bytes")
        archive.writestr("notes.txt", b"not an image")


def mokuro_payload(**overrides: object) -> dict[str, object]:
    payload: dict[str, object] = {
        "version": "0.2.2",
        "title": "v01_h3rbbi_d",
        "title_uuid": "f944ebce-5b9e-41f0-b15f-1e637ee157f7",
        "volume": "v01",
        "volume_uuid": "cfb5220c-57db-4008-9f44-e659d794e381",
        "pages": [
            {"blocks": [{"lines": ["世界", "abc"]}]},
            {"blocks": [{"lines": ["ねこ"]}]},
        ],
    }
    payload.update(overrides)
    return payload


@pytest.fixture
def library(tmp_path: Path) -> Path:
    root = tmp_path / "library"
    root.mkdir()
    return root


class TestIterSeriesFolders:
    def test_lists_top_level_folders_that_hold_an_archive(self, library: Path) -> None:
        write_cbz(library / "Dr Stone" / "Volume 01.cbz")
        write_cbz(library / "Aria" / "v1.cbz")
        (library / "Empty").mkdir()
        (library / ".hidden").mkdir()
        write_cbz(library / ".hidden" / "x.cbz")
        write_cbz(library / "loose.cbz")
        assert [folder.title for folder in iter_series_folders(library)] == ["Aria", "Dr Stone"]

    def test_nested_folders_are_not_series(self, library: Path) -> None:
        write_cbz(library / "Dr Stone" / "extras" / "bonus.cbz")
        assert iter_series_folders(library) == []

    def test_a_missing_library_is_empty_not_an_error(self, tmp_path: Path) -> None:
        assert iter_series_folders(tmp_path / "nope") == []


class TestCompileVolumes:
    def test_reads_uuid_pages_version_and_counts_chars_itself(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz")
        (series / "Volume 01.mokuro").write_text(
            json.dumps(mokuro_payload()), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.volume_uuid == "cfb5220c-57db-4008-9f44-e659d794e381"
        assert entry.volume_title == "Volume 01"   # the .cbz stem, not the .mokuro's "volume"
        assert entry.page_count == 2
        assert entry.character_count == 4          # 世界 + ねこ, "abc" ignored
        assert entry.mokuro_version == "0.2.2"
        assert entry.spine_width is None
        assert entry.archive_size == (series / "Volume 01.cbz").stat().st_size

    def test_prefers_an_explicit_chars_total_when_the_file_carries_one(
        self, library: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(chars=13247)), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.character_count == 13247

    def test_carries_the_readers_spine_width_extension(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(spine_width=250.5)), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.spine_width == 250.5

    def test_reads_gzipped_sidecars(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        with gzip.open(series / "v1.mokuro.gz", "wt", encoding="utf-8") as handle:
            json.dump(mokuro_payload(), handle)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == "0.2.2"
        assert entry.page_count == 2

    def test_image_only_volume_gets_an_empty_version_and_a_derived_uuid(
        self, library: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""
        assert entry.character_count == 0
        assert entry.page_count == 3               # images in the archive, notes.txt ignored
        # The same uuid the reader's placeholder derives, so progress attaches.
        assert entry.volume_uuid == "38d6c0d6-1bef-4134-a339-a1e254c6"

    def test_a_corrupt_sidecar_degrades_to_image_only(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)
        (series / "Volume 01.mokuro").write_text("{ this is not json", encoding="utf-8")
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""
        assert entry.page_count == 3
        assert entry.volume_uuid == "38d6c0d6-1bef-4134-a339-a1e254c6"

    def test_a_sidecar_without_an_archive_is_not_a_volume(self, library: Path) -> None:
        series = library / "Dr Stone"
        series.mkdir()
        (series / "ghost.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        assert compile_series_volumes(SeriesFolder("Dr Stone", series)) == []

    def test_entries_come_back_in_natural_order(self, library: Path) -> None:
        series = library / "Dr Stone"
        for name in ("Volume 10", "Volume 2", "Volume 1"):
            write_cbz(series / f"{name}.cbz")
        titles = [e.volume_title for e in compile_series_volumes(SeriesFolder("Dr Stone", series))]
        assert titles == ["Volume 1", "Volume 2", "Volume 10"]

    def test_cover_sidecars_and_markers_are_not_volumes(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.webp").write_bytes(b"fake webp")
        (series / "v2.nocover").touch()
        (series / "series.json").write_text("{}", encoding="utf-8")
        assert [e.volume_title for e in compile_series_volumes(SeriesFolder("Dr Stone", series))] == [
            "v1"
        ]


class TestEntryCache:
    def test_a_cached_entry_is_used_instead_of_reparsing(
        self, library: Path, tmp_path: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")

        # Seed a deliberately wrong entry against the real stats: if the
        # compiler consults the cache, this is what comes back.
        cbz_stat = (series / "v1.cbz").stat()
        sidecar_stat = (series / "v1.mokuro").stat()
        database.put_cached_volume_entry(
            "Dr Stone/v1.cbz",
            "dr stone",
            {
                "volume_uuid": "cached",
                "volume_title": "v1",
                "page_count": 999,
                "character_count": 888,
                "mokuro_version": "cached",
            },
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}",
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.page_count == 999
        assert entry.mokuro_version == "cached"

    def test_a_changed_sidecar_invalidates_the_cache_and_is_rewritten(
        self, library: Path, tmp_path: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")

        first = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert first[0].character_count == 4

        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(chars=500)), encoding="utf-8"
        )
        second = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert second[0].character_count == 500

        # And the fresh value is what the cache now holds.
        cbz_stat = (series / "v1.cbz").stat()
        sidecar_stat = (series / "v1.mokuro").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz",
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}",
        )
        assert cached is not None
        assert cached["character_count"] == 500
