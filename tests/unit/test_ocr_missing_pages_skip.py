"""Volumes uploaded short of pages get their primary OCR and no additional layers.

The flag is the metadata pass's own `.mokuro`-against-archive cross-check,
cached per volume in `series_entry_cache`; the OCR worker only ever READS it.
"""

from __future__ import annotations

import json
import os
import zipfile
from pathlib import Path

import pytest

from mokuro_bunko.database import Database
from mokuro_bunko.metadata.compiler import (
    SeriesFolder,
    cached_missing_pages,
    compile_series_volumes,
)
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.processor import OCRProcessor

ROWS = [
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {"name": "hayai-nova-ctd", "engine": "hayai-nova", "detector": "ctd"},
    {"name": "ppocr-manga", "engine": "ppocr-manga"},
]


def make_volume(series: Path, title: str, *, images: int, pages: int) -> Path:
    """A `.cbz` holding `images` pictures beside a `.mokuro` naming `pages` of them."""
    series.mkdir(parents=True, exist_ok=True)
    cbz = series / f"{title}.cbz"
    with zipfile.ZipFile(cbz, "w") as zf:
        for n in range(images):
            zf.writestr(f"p{n:03d}.jpg", b"not really a jpeg")
    sidecar = {
        "version": "0.2.1",
        "title": series.name,
        "title_uuid": "t",
        "volume": title,
        "volume_uuid": f"uuid-{title}",
        "pages": [
            {"img_path": f"p{n:03d}.jpg", "img_width": 10, "img_height": 10, "blocks": []}
            for n in range(pages)
        ],
        "chars": 0,
    }
    (series / f"{title}.mokuro").write_text(json.dumps(sidecar), encoding="utf-8")
    return cbz


@pytest.fixture
def library(tmp_path: Path) -> Path:
    path = tmp_path / "storage" / "library"
    path.mkdir(parents=True)
    return path


@pytest.fixture
def database(tmp_path: Path) -> Database:
    return Database(tmp_path / "storage" / "mokuro.db")


def metadata_pass(database: Database, library: Path, series: str) -> None:
    compile_series_volumes(SeriesFolder(title=series, path=library / series), database=database)


def processor_for(library: Path, database: Database) -> OCRProcessor:
    processor = OCRProcessor(
        storage_path=library.parent, generations=parse_generation_list(ROWS)
    )
    processor.missing_pages_lookup = lambda cbz: cached_missing_pages(database, library, cbz)
    return processor


class TestCachedMissingPages:
    def test_a_short_volume_reads_as_short_a_whole_one_as_zero(
        self, library: Path, database: Database
    ) -> None:
        short = make_volume(library / "Series", "Vol 01", images=7, pages=10)
        whole = make_volume(library / "Series", "Vol 02", images=10, pages=10)
        metadata_pass(database, library, "Series")
        assert cached_missing_pages(database, library, short) == 3
        assert cached_missing_pages(database, library, whole) == 0

    def test_before_any_metadata_pass_nothing_is_known_to_be_short(
        self, library: Path, database: Database
    ) -> None:
        short = make_volume(library / "Series", "Vol 01", images=7, pages=10)
        assert cached_missing_pages(database, library, short) == 0

    def test_replacing_the_archive_drops_the_flag_without_anyone_clearing_it(
        self, library: Path, database: Database
    ) -> None:
        short = make_volume(library / "Series", "Vol 01", images=7, pages=10)
        metadata_pass(database, library, "Series")
        assert cached_missing_pages(database, library, short) == 3

        # The user's fix: upload the whole volume over it.
        make_volume(library / "Series", "Vol 01", images=10, pages=10)
        os.utime(short, (1_900_000_000, 1_900_000_000))
        # Stale entry: not "short", not anything, until the next pass recompiles it...
        assert cached_missing_pages(database, library, short) == 0
        metadata_pass(database, library, "Series")
        # ...and then it is simply a whole volume.
        assert cached_missing_pages(database, library, short) == 0

    def test_a_path_outside_a_top_level_series_folder_is_never_short(
        self, library: Path, database: Database, tmp_path: Path
    ) -> None:
        nested = make_volume(library / "A" / "B", "Vol 01", images=1, pages=5)
        elsewhere = make_volume(tmp_path / "other", "Vol 01", images=1, pages=5)
        assert cached_missing_pages(database, library, nested) == 0
        assert cached_missing_pages(database, library, elsewhere) == 0


class TestTheSchedulerSkipsAdditionalLayers:
    def test_a_short_volume_is_owed_no_additional_layers(
        self, library: Path, database: Database
    ) -> None:
        short = make_volume(library / "Series", "Vol 01", images=7, pages=10)
        whole = make_volume(library / "Series", "Vol 02", images=10, pages=10)
        metadata_pass(database, library, "Series")
        processor = processor_for(library, database)

        assert [row.name for row in processor.missing_generations(whole)] == [
            "hayai-nova-ctd",
            "ppocr-manga",
        ]
        assert processor.missing_generations(short) == []
        assert [row.name for row in processor.skipped_generations(short)] == [
            "hayai-nova-ctd",
            "ppocr-manga",
        ]
        assert processor.skipped_generations(whole) == []

    def test_a_volume_whose_mokuro_went_away_is_owed_every_row(
        self, library: Path, database: Database
    ) -> None:
        # Flagged on a previous pass, then its primary sidecar went away: our
        # own primary is read from the archive, so nothing is short any more.
        short = make_volume(library / "Series", "Vol 01", images=7, pages=10)
        processor = processor_for(library, database)
        processor.missing_pages_lookup = lambda _cbz: 3
        short.with_suffix(".mokuro").unlink()
        assert [row.name for row in processor.missing_generations(short)] == [
            "mokuro", "hayai-nova-ctd", "ppocr-manga",
        ]

    def test_a_replaced_volume_gets_its_layers(self, library: Path, database: Database) -> None:
        short = make_volume(library / "Series", "Vol 01", images=7, pages=10)
        metadata_pass(database, library, "Series")
        processor = processor_for(library, database)
        assert processor.missing_generations(short) == []

        make_volume(library / "Series", "Vol 01", images=10, pages=10)
        os.utime(short, (1_900_000_000, 1_900_000_000))
        assert [row.name for row in processor.missing_generations(short)] == [
            "hayai-nova-ctd",
            "ppocr-manga",
        ]

    def test_without_a_lookup_nothing_is_ever_skipped(self, library: Path) -> None:
        short = make_volume(library / "Series", "Vol 01", images=7, pages=10)
        processor = OCRProcessor(
            storage_path=library.parent, generations=parse_generation_list(ROWS)
        )
        assert len(processor.missing_generations(short)) == 2

    def test_a_lookup_that_raises_never_stalls_the_queue(self, library: Path) -> None:
        short = make_volume(library / "Series", "Vol 01", images=7, pages=10)
        processor = OCRProcessor(
            storage_path=library.parent, generations=parse_generation_list(ROWS)
        )

        def broken(_cbz: Path) -> int:
            raise RuntimeError("database is locked")

        processor.missing_pages_lookup = broken
        assert processor.pages_short(short) == 0
        assert len(processor.missing_generations(short)) == 2
