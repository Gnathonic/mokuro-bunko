"""Contract §2: a series folder compiles into the reader's volume entries."""

from __future__ import annotations

import gzip
import hashlib
import json
import os
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.metadata import compiler as compiler_module
from mokuro_bunko.metadata.compiler import (
    SeriesFolder,
    cached_missing_pages,
    cached_mokuro_sha256,
    cached_page_count,
    compile_series_volumes,
    iter_series_folders,
)
from mokuro_bunko.metadata.schema import SeriesFacts, SeriesIndexData, dump_series_file


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
            {"img_path": "000.jpg", "blocks": [{"lines": ["世界", "abc"]}]},
            {"img_path": "001.jpg", "blocks": [{"lines": ["ねこ"]}]},
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

    def test_integer_spine_width_is_not_widened_to_a_float(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(spine_width=250)), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.spine_width == 250
        assert type(entry.spine_width) is int  # noqa: E721 — widening to float is the bug

        # The reader's JSON.stringify(250) is "250", never "250.0"; bunko must
        # not republish a whole-number spine_width with a trailing float zero.
        dumped = dump_series_file(
            series_title="Dr Stone",
            facts=SeriesFacts(),
            index=SeriesIndexData(),
            volumes=[entry],
        )
        assert b'"spine_width":250,' in dumped
        assert b"250.0" not in dumped

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

    def test_bad_gzip_bytes_degrade_to_image_only(self, library: Path) -> None:
        # A `.mokuro.gz` that isn't gzip at all (not just bad JSON).
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)
        (series / "Volume 01.mokuro.gz").write_bytes(b"not gzip data at all")
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""
        assert entry.page_count == 3
        assert entry.character_count == 0
        assert entry.volume_uuid == "38d6c0d6-1bef-4134-a339-a1e254c6"

    def test_truncated_gzip_degrades_to_image_only(self, library: Path) -> None:
        # A valid gzip header/stream cut off mid-body (not just bad bytes).
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)
        full = gzip.compress(json.dumps(mokuro_payload()).encode("utf-8"))
        (series / "Volume 01.mokuro.gz").write_bytes(full[: len(full) // 2])
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""
        assert entry.page_count == 3
        assert entry.character_count == 0
        assert entry.volume_uuid == "38d6c0d6-1bef-4134-a339-a1e254c6"

    def test_non_list_pages_falls_back_to_archive_image_count(self, library: Path) -> None:
        # Valid JSON, but `pages` isn't a list: version/uuid still come from
        # the (otherwise well-formed) sidecar; page/char counts degrade.
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz", pages=3)
        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(pages="not a list")), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == "0.2.2"
        assert entry.volume_uuid == "cfb5220c-57db-4008-9f44-e659d794e381"
        assert entry.page_count == 3   # images in the archive, not len("not a list")
        assert entry.character_count == 0

    def test_list_of_junk_pages_counts_zero_chars_without_raising(self, library: Path) -> None:
        # `pages` is a list (so its length is trusted for page_count), but its
        # entries are not page objects: character counting must not crash.
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz", pages=3)
        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(pages=["junk", 123, None, "x", 4.5])), encoding="utf-8"
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == "0.2.2"
        assert entry.volume_uuid == "cfb5220c-57db-4008-9f44-e659d794e381"
        assert entry.page_count == 5   # len(pages), not the archive's image count
        assert entry.character_count == 0

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
                # A post-11b row shape: mokuro_size/mokuro_modified must be
                # present (even a deliberately wrong value works) or this
                # row reads as a pre-11b legacy row and MISSES instead of
                # hitting — see TestFreshnessStamps's dedicated miss/restamp
                # test for that path.
                "mokuro_size": 111,
                "mokuro_modified": 222,
                # Same rule for `matched_page_count`, added later still: a row
                # without the key is a legacy row and must miss.
                "matched_page_count": 777,
            },
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}",
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.page_count == 999
        assert entry.matched_page_count == 777
        assert entry.mokuro_version == "cached"
        assert entry.mokuro_size == 111
        assert entry.mokuro_modified == 222

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


class TestFreshnessStamps:
    def test_mokuro_stamps_come_from_the_sidecars_own_stat(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        sidecar_stat = (series / "v1.mokuro").stat()
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_size == sidecar_stat.st_size
        assert entry.mokuro_modified == int(sidecar_stat.st_mtime)
        assert isinstance(entry.mokuro_modified, int)  # truncated, not the raw float

    def test_no_sidecar_means_no_mokuro_stamps(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)   # image-only, no .mokuro at all
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_size is None
        assert entry.mokuro_modified is None

    def test_a_corrupt_but_present_sidecar_still_gets_stamped(self, library: Path) -> None:
        # A stat is not a parse: a sidecar that fails to parse still has a
        # real mtime/size on disk, and that is exactly the freshness
        # information a client needs in order to know a retry is worthwhile.
        series = library / "Dr Stone"
        write_cbz(series / "Volume 01.cbz", pages=3)
        (series / "Volume 01.mokuro").write_text("{ this is not json", encoding="utf-8")
        sidecar_stat = (series / "Volume 01.mokuro").stat()
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""          # still degrades to image-only
        assert entry.mokuro_size == sidecar_stat.st_size
        assert entry.mokuro_modified == int(sidecar_stat.st_mtime)

    def test_cover_stamps_come_from_the_webp_sidecar(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.webp").write_bytes(b"fake webp bytes")
        cover_stat = (series / "v1.webp").stat()
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.cover_size == cover_stat.st_size
        assert entry.cover_modified == int(cover_stat.st_mtime)

    def test_no_cover_means_no_cover_stamps(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.nocover").touch()   # extraction was attempted and failed
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.cover_size is None
        assert entry.cover_modified is None

    def test_cover_stamp_is_fresh_even_on_a_cached_entry(
        self, library: Path, tmp_path: Path
    ) -> None:
        # `cover_size`/`cover_modified` are deliberately OUTSIDE the entry
        # cache (Decisions, 2026-08-24): the cache exists to skip re-parsing
        # the `.mokuro`, not to skip a stat(). A cover that appears well
        # after the entry was cached — exactly what happens when the cover
        # worker (Task 12) runs on its own schedule — must show up on the
        # very next compile, not wait for the archive or sidecar to change.
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")

        first = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert first[0].cover_size is None   # no cover yet; entry gets cached

        (series / "v1.webp").write_bytes(b"fake webp bytes")
        cover_stat = (series / "v1.webp").stat()

        second = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert second[0].cover_size == cover_stat.st_size
        assert second[0].cover_modified == int(cover_stat.st_mtime)
        # The rest of the (expensive) entry still came from the cache, not a
        # re-parse — proving the split didn't quietly disable the cache.
        assert second[0].mokuro_version == "0.2.2"

    def test_mokuro_stamps_round_trip_through_the_entry_cache(
        self, library: Path, tmp_path: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")
        sidecar_stat = (series / "v1.mokuro").stat()

        compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        cbz_stat = (series / "v1.cbz").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz",
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}",
        )
        assert cached is not None
        assert cached["mokuro_size"] == sidecar_stat.st_size
        assert cached["mokuro_modified"] == int(sidecar_stat.st_mtime)

        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.mokuro_size == sidecar_stat.st_size
        assert entry.mokuro_modified == int(sidecar_stat.st_mtime)

    def test_a_pre_stamps_cache_row_misses_and_restamps_instead_of_hitting_as_none(
        self, library: Path, tmp_path: Path
    ) -> None:
        # Review round 1, Finding 1: a `series_entry_cache` row written by a
        # bunko binary that predates this feature has no `mokuro_size`/
        # `mokuro_modified` keys in its `entry_json` at all — not present
        # with a `None` value, simply absent, because `_entry_to_dict` at
        # the time had never heard of them. `.get(...)` would silently
        # accept that row as a "fresh" hit with both stamps `None`,
        # indistinguishable from "no sidecar" even though the sidecar is
        # present and unchanged on disk (the cache-validation key — archive
        # + sidecar stat — still matches, since neither file was touched).
        # That never self-heals: `_compile_volume` is never invoked again
        # for this volume until something ELSE invalidates the cache key,
        # which may be never for a completed series. Required-key access
        # must instead raise `KeyError` on a legacy row -> `_entry_from_dict`
        # returns `None` -> cache MISS -> `_compile_volume` runs -> the row
        # is rewritten with real stamps, restamping exactly once.
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")
        cbz_stat = (series / "v1.cbz").stat()
        sidecar_stat = (series / "v1.mokuro").stat()
        sidecar_key = f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}"

        # A pre-11b row: the exact shape `_entry_to_dict` wrote before this
        # task, missing `mokuro_size`/`mokuro_modified` entirely (not `None`
        # values — the keys themselves are absent).
        database.put_cached_volume_entry(
            "Dr Stone/v1.cbz",
            "dr stone",
            {
                "volume_uuid": "cfb5220c-57db-4008-9f44-e659d794e381",
                "volume_title": "v1",
                "page_count": 2,
                "character_count": 4,
                "mokuro_version": "0.2.2",
                "spine_width": None,
                "archive_size": cbz_stat.st_size,
            },
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            sidecar_key,
        )

        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.mokuro_size == sidecar_stat.st_size
        assert entry.mokuro_modified == int(sidecar_stat.st_mtime)

        # And the cache row itself is now restamped, so the very next
        # compile is served from cache with real stamps too.
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz", cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
        )
        assert cached is not None
        assert cached["mokuro_size"] == sidecar_stat.st_size
        assert cached["mokuro_modified"] == int(sidecar_stat.st_mtime)


class TestMatchedPageCount:
    """`matched_page_count`: how many of the sidecar's pages the archive has."""

    def test_a_complete_volume_matches_every_page(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.page_count == 2
        assert entry.matched_page_count == 2
        assert entry.missing_pages == 0

    def test_a_page_with_no_image_in_the_archive_is_missing(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz", pages=1)          # only 000.jpg
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.page_count == 2
        assert entry.matched_page_count == 1
        assert entry.missing_pages == 1

    def test_a_changed_extension_still_matches(self, library: Path) -> None:
        series = library / "Dr Stone"
        series.mkdir(parents=True)
        with zipfile.ZipFile(series / "v1.cbz", "w") as archive:
            archive.writestr("000.webp", b"image")
            archive.writestr("001.webp", b"image")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.matched_page_count == 2

    def test_os_junk_and_the_embedded_cover_are_not_pages(self, library: Path) -> None:
        series = library / "Dr Stone"
        series.mkdir(parents=True)
        with zipfile.ZipFile(series / "v1.cbz", "w") as archive:
            archive.writestr("000.jpg", b"image")
            archive.writestr("001.jpg", b"image")
            archive.writestr("__MACOSX/._000.jpg", b"junk")
            archive.writestr("v1.webp", b"the volume's own cover sidecar")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        # Both pages matched and neither excluded entry became an extra image
        # that could have triggered the count-based fallback.
        assert entry.matched_page_count == 2

    def test_a_corrupt_archive_matches_nothing_and_is_damaged(self, library: Path) -> None:
        series = library / "Dr Stone"
        series.mkdir(parents=True)
        (series / "v1.cbz").write_bytes(b"this is not a zip file")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.page_count == 2
        assert entry.matched_page_count == 0
        assert entry.missing_pages == 2

    def test_an_image_only_volume_is_never_damaged(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz", pages=5)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.page_count == 5
        assert entry.matched_page_count == 5
        assert entry.missing_pages == 0

    def test_a_sidecar_that_names_no_images_claims_nothing(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(
            json.dumps(mokuro_payload(pages=[{"blocks": []}, {"blocks": []}])),
            encoding="utf-8",
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.page_count == 2
        assert entry.matched_page_count is None
        assert entry.missing_pages == 0

    def test_an_unreadable_archive_is_unknown_and_is_not_cached(
        self, library: Path, tmp_path: Path
    ) -> None:
        """A permissions failure is not damage, and must not be remembered as
        damage: the entry claims nothing AND no cache row is written, so the
        next pass asks the filesystem again instead of serving the shrug."""
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")
        cbz_stat = (series / "v1.cbz").stat()
        sidecar_stat = (series / "v1.mokuro").stat()

        (series / "v1.cbz").chmod(0o000)
        try:
            [entry] = compile_series_volumes(
                SeriesFolder("Dr Stone", series), database=database
            )
        finally:
            (series / "v1.cbz").chmod(0o644)
        assert entry.matched_page_count is None
        assert entry.missing_pages == 0
        assert (
            database.get_cached_volume_entry(
                "Dr Stone/v1.cbz",
                cbz_stat.st_size,
                cbz_stat.st_mtime,
                f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}",
            )
            is None
        )

        # Nothing was cached, so the recovered archive is seen on the next pass.
        [healed] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert healed.matched_page_count == 2

    def test_a_legacy_cache_row_is_backfilled_not_served(
        self, library: Path, tmp_path: Path
    ) -> None:
        """The backfill: a row written before this field existed has no key for
        it, so it misses and the volume is recompiled with a real count."""
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")
        cbz_stat = (series / "v1.cbz").stat()
        sidecar_stat = (series / "v1.mokuro").stat()
        sidecar_key = f"v1.mokuro:{sidecar_stat.st_size}:{sidecar_stat.st_mtime}"
        database.put_cached_volume_entry(
            "Dr Stone/v1.cbz",
            "dr stone",
            {
                "volume_uuid": "cached-uuid",
                "volume_title": "v1",
                "page_count": 999,
                "character_count": 888,
                "mokuro_version": "cached",
                "spine_width": None,
                "archive_size": 1,
                "mokuro_size": 111,
                "mokuro_modified": 222,
                # No `matched_page_count` — this is the legacy shape.
            },
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            sidecar_key,
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.page_count == 2               # recompiled, not the cached 999
        assert entry.matched_page_count == 2
        assert database.get_cached_volume_entry(
            "Dr Stone/v1.cbz", cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
        )["matched_page_count"] == 2


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sidecar_key_of(path: Path) -> str:
    stat = path.stat()
    return f"{path.name}:{stat.st_size}:{stat.st_mtime}"


class TestMokuroSha256:
    """`mokuro_sha256`: the primary sidecar's JSON bytes, hashed as stored (gunzipped)."""

    def test_hashes_the_plain_sidecars_bytes(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        raw = json.dumps(mokuro_payload(), ensure_ascii=False).encode("utf-8")
        (series / "v1.mokuro").write_bytes(raw)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_sha256 == sha256_hex(raw)
        assert len(entry.mokuro_sha256) == 64
        assert entry.mokuro_sha256 == entry.mokuro_sha256.lower()

    def test_the_same_json_hashes_the_same_plain_or_gzipped(self, library: Path) -> None:
        raw = json.dumps(mokuro_payload(), ensure_ascii=False).encode("utf-8")
        plain = library / "Plain"
        write_cbz(plain / "v1.cbz")
        (plain / "v1.mokuro").write_bytes(raw)
        packed = library / "Packed"
        write_cbz(packed / "v1.cbz")
        # Two different gzip framings of the same JSON (header mtime, level):
        # the hash is of the JSON, never of the compressed file.
        (packed / "v1.mokuro.gz").write_bytes(gzip.compress(raw, compresslevel=9, mtime=1))
        [from_plain] = compile_series_volumes(SeriesFolder("Plain", plain))
        [from_gz] = compile_series_volumes(SeriesFolder("Packed", packed))
        assert from_gz.mokuro_sha256 == from_plain.mokuro_sha256 == sha256_hex(raw)

        (packed / "v1.mokuro.gz").write_bytes(gzip.compress(raw, compresslevel=1, mtime=2))
        [again] = compile_series_volumes(SeriesFolder("Packed", packed))
        assert again.mokuro_sha256 == sha256_hex(raw)

    def test_the_bytes_are_hashed_as_stored_not_re_serialized(self, library: Path) -> None:
        # Pretty-printed and compact forms of the same object are different
        # files to a reader, so they hash differently.
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        pretty = json.dumps(mokuro_payload(), indent=2).encode("utf-8")
        (series / "v1.mokuro").write_bytes(pretty)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_sha256 == sha256_hex(pretty)
        assert entry.mokuro_sha256 != sha256_hex(json.dumps(mokuro_payload()).encode("utf-8"))

    def test_an_image_only_volume_has_no_hash(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz", pages=3)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_sha256 is None
        dumped = dump_series_file(
            series_title="Dr Stone", facts=SeriesFacts(), index=SeriesIndexData(), volumes=[entry]
        )
        assert b"mokuro_sha256" not in dumped

    @pytest.mark.parametrize(
        ("name", "body"),
        [
            ("v1.mokuro", b"{ this is not json"),
            ("v1.mokuro", b"[1, 2, 3]"),
            ("v1.mokuro", b"\xff\xfe not utf-8"),
            ("v1.mokuro.gz", b"not gzip data at all"),
        ],
    )
    def test_a_sidecar_that_does_not_parse_has_no_hash(
        self, library: Path, name: str, body: bytes
    ) -> None:
        # It degrades the volume to image-only: there is no OCR a reader could
        # install from it, so nothing for a reader to compare.
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / name).write_bytes(body)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == ""
        assert entry.mokuro_sha256 is None

    def test_a_truncated_gzip_has_no_hash(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        full = gzip.compress(json.dumps(mokuro_payload()).encode("utf-8"))
        (series / "v1.mokuro.gz").write_bytes(full[: len(full) // 2])
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_sha256 is None

    def test_an_ocr_layer_is_never_hashed_as_the_primary(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        layer = json.dumps(mokuro_payload(version="layer")).encode("utf-8")
        (series / "v1.hayai-nova.mokuro").write_bytes(layer)
        (series / "v1.paddle-manga.mokuro.gz").write_bytes(gzip.compress(layer))
        # Only layers: the volume has no primary, so no hash at all.
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_sha256 is None

        primary = json.dumps(mokuro_payload()).encode("utf-8")
        (series / "v1.mokuro").write_bytes(primary)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_sha256 == sha256_hex(primary)

    def test_plain_beats_gzip_like_the_rest_of_the_entry(self, library: Path) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        plain = json.dumps(mokuro_payload(version="plain")).encode("utf-8")
        packed = json.dumps(mokuro_payload(version="gz")).encode("utf-8")
        (series / "v1.mokuro").write_bytes(plain)
        (series / "v1.mokuro.gz").write_bytes(gzip.compress(packed))
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series))
        assert entry.mokuro_version == "plain"
        assert entry.mokuro_sha256 == sha256_hex(plain)

    def test_rewriting_the_primary_changes_the_hash_through_the_cache(
        self, library: Path, tmp_path: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        first_bytes = json.dumps(mokuro_payload()).encode("utf-8")
        (series / "v1.mokuro").write_bytes(first_bytes)
        database = Database(tmp_path / "test.db")
        [first] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert first.mokuro_sha256 == sha256_hex(first_bytes)

        # A re-OCR: a new primary of the same length, mtime pushed forward so
        # the change cannot hide inside the filesystem's timestamp grain.
        second_bytes = first_bytes.replace(b"0.2.2", b"0.2.3")
        assert len(second_bytes) == len(first_bytes)
        (series / "v1.mokuro").write_bytes(second_bytes)
        stat = (series / "v1.mokuro").stat()
        os.utime(series / "v1.mokuro", ns=(stat.st_atime_ns, stat.st_mtime_ns + 5_000_000_000))
        [second] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert second.mokuro_sha256 == sha256_hex(second_bytes)
        assert second.mokuro_sha256 != first.mokuro_sha256

    def test_the_hash_round_trips_through_the_entry_cache_without_a_read(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        raw = json.dumps(mokuro_payload()).encode("utf-8")
        (series / "v1.mokuro").write_bytes(raw)
        database = Database(tmp_path / "test.db")
        compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)

        cbz_stat = (series / "v1.cbz").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz",
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            sidecar_key_of(series / "v1.mokuro"),
        )
        assert cached is not None
        assert cached["mokuro_sha256"] == sha256_hex(raw)

        def no_read(path: Path) -> object:
            raise AssertionError(f"a warm cache must not read {path}")

        monkeypatch.setattr(compiler_module, "_load_sidecar", no_read)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.mokuro_sha256 == sha256_hex(raw)

    def test_an_image_only_row_records_none_and_is_never_filled(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        database = Database(tmp_path / "test.db")
        compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        cbz_stat = (series / "v1.cbz").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz", cbz_stat.st_size, cbz_stat.st_mtime, ""
        )
        assert cached is not None
        assert "mokuro_sha256" in cached and cached["mokuro_sha256"] is None

        def no_read(path: Path) -> object:
            raise AssertionError(f"nothing to read for an image-only volume: {path}")

        monkeypatch.setattr(compiler_module, "_load_sidecar", no_read)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.mokuro_sha256 is None


def _seed_legacy_row(database: Database, series: Path, **fields: object) -> str:
    """A cache row exactly as a pre-hash bunko wrote it: every key but the hash."""
    cbz_stat = (series / "v1.cbz").stat()
    sidecar = series / "v1.mokuro"
    sidecar_key = sidecar_key_of(sidecar) if sidecar.exists() else ""
    row: dict[str, object] = {
        "volume_uuid": "cached-uuid",
        "volume_title": "v1",
        "page_count": 999,
        "matched_page_count": 990,
        "character_count": 888,
        "mokuro_version": "cached",
        "spine_width": None,
        "archive_size": 1,
        "mokuro_size": 111,
        "mokuro_modified": 222,
    }
    row.update(fields)
    database.put_cached_volume_entry(
        "Dr Stone/v1.cbz", "dr stone", row, cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
    )
    return sidecar_key


class TestHashFill:
    """Rows cached before the hash existed are completed once, from the sidecar alone."""

    def test_a_legacy_row_gets_its_hash_without_a_recompile(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        raw = json.dumps(mokuro_payload()).encode("utf-8")
        (series / "v1.mokuro").write_bytes(raw)
        database = Database(tmp_path / "test.db")
        sidecar_key = _seed_legacy_row(database, series)

        def no_archive(path: Path) -> object:
            raise AssertionError(f"a hash fill must not open the archive: {path}")

        monkeypatch.setattr(compiler_module, "_archive_image_names", no_archive)
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        # Everything else is still the cached row's (no recompile)...
        assert entry.page_count == 999
        assert entry.matched_page_count == 990
        assert entry.mokuro_version == "cached"
        # ...and the hash is the sidecar's.
        assert entry.mokuro_sha256 == sha256_hex(raw)

        cbz_stat = (series / "v1.cbz").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz", cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
        )
        assert cached is not None
        assert cached["mokuro_sha256"] == sha256_hex(raw)
        assert cached["page_count"] == 999

    def test_a_filled_row_is_never_read_again(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")
        _seed_legacy_row(database, series)

        reads: list[Path] = []
        real_load = compiler_module._load_sidecar

        def counting_load(path: Path) -> object:
            reads.append(path)
            return real_load(path)

        monkeypatch.setattr(compiler_module, "_load_sidecar", counting_load)
        compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert reads == [series / "v1.mokuro"]

    def test_the_fill_hashes_the_gunzipped_primary(
        self, library: Path, tmp_path: Path
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        raw = json.dumps(mokuro_payload()).encode("utf-8")
        (series / "v1.mokuro.gz").write_bytes(gzip.compress(raw))
        (series / "v1.hayai-nova.mokuro").write_text("{}", encoding="utf-8")
        database = Database(tmp_path / "test.db")
        cbz_stat = (series / "v1.cbz").stat()
        database.put_cached_volume_entry(
            "Dr Stone/v1.cbz",
            "dr stone",
            {
                "volume_uuid": "u",
                "volume_title": "v1",
                "page_count": 2,
                "matched_page_count": 2,
                "character_count": 4,
                "mokuro_version": "0.2.2",
                "mokuro_size": 1,
                "mokuro_modified": 1,
            },
            cbz_stat.st_size,
            cbz_stat.st_mtime,
            sidecar_key_of(series / "v1.mokuro.gz"),
        )
        [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.mokuro_sha256 == sha256_hex(raw)

    def test_fill_hashes_false_serves_the_row_and_leaves_it_for_the_pass(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        raw = json.dumps(mokuro_payload()).encode("utf-8")
        (series / "v1.mokuro").write_bytes(raw)
        database = Database(tmp_path / "test.db")
        sidecar_key = _seed_legacy_row(database, series)

        def no_read(path: Path) -> object:
            raise AssertionError(f"a request-path compile must not read {path}")

        with monkeypatch.context() as patch:
            patch.setattr(compiler_module, "_load_sidecar", no_read)
            [entry] = compile_series_volumes(
                SeriesFolder("Dr Stone", series), database=database, fill_hashes=False
            )
        assert entry.page_count == 999                # served from the cache
        assert entry.mokuro_sha256 is None
        cbz_stat = (series / "v1.cbz").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz", cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
        )
        assert cached is not None and "mokuro_sha256" not in cached

        [filled] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert filled.mokuro_sha256 == sha256_hex(raw)

    def test_an_unreadable_sidecar_is_retried_not_stored_as_no_hash(
        self, library: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        raw = json.dumps(mokuro_payload()).encode("utf-8")
        (series / "v1.mokuro").write_bytes(raw)
        database = Database(tmp_path / "test.db")
        sidecar_key = _seed_legacy_row(database, series)

        # The share hiccups for one pass.
        with monkeypatch.context() as patch:
            patch.setattr(compiler_module, "_load_sidecar", lambda path: (None, None))
            [entry] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert entry.mokuro_sha256 is None
        cbz_stat = (series / "v1.cbz").stat()
        cached = database.get_cached_volume_entry(
            "Dr Stone/v1.cbz", cbz_stat.st_size, cbz_stat.st_mtime, sidecar_key
        )
        assert cached is not None and "mokuro_sha256" not in cached

        [healed] = compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert healed.mokuro_sha256 == sha256_hex(raw)

    def test_cache_only_readers_still_hit_a_legacy_row(
        self, library: Path, tmp_path: Path
    ) -> None:
        # The OCR worker and the queue ETA read the cache without compiling; a
        # row without the hash is otherwise current and must keep answering.
        series = library / "Dr Stone"
        write_cbz(series / "v1.cbz")
        (series / "v1.mokuro").write_text(json.dumps(mokuro_payload()), encoding="utf-8")
        database = Database(tmp_path / "test.db")
        _seed_legacy_row(database, series)
        assert cached_page_count(database, library, series / "v1.cbz") == 999
        assert cached_missing_pages(database, library, series / "v1.cbz") == 9
        assert cached_mokuro_sha256(database, library, series / "v1.cbz") is None

        compile_series_volumes(SeriesFolder("Dr Stone", series), database=database)
        assert cached_mokuro_sha256(database, library, series / "v1.cbz") == sha256_hex(
            (series / "v1.mokuro").read_bytes()
        )
