"""Golden bytes for the two compiled documents (contract §2 and §3)."""

from __future__ import annotations

import pytest

from mokuro_bunko.metadata.schema import (
    FACTLESS_UPDATED_AT,
    SeriesFacts,
    SeriesIndexData,
    VolumeEntry,
    dump_catalog_file,
    dump_series_file,
)

DR_STONE = SeriesFacts(
    external_ids={"anilist": 98416, "mal": 103897},
    titles={"native": "Dr.STONE", "romaji": "Dr. STONE"},
    synonyms=("ドクターストーン",),
    tag="HD Scan",
    unit="volumes",
    updated_at="2026-08-18T19:36:24.324Z",
)


class TestSeriesFacts:
    def test_empty_facts_are_factless_at_the_epoch(self) -> None:
        facts = SeriesFacts()
        assert not facts.has_facts()
        assert facts.updated_at == FACTLESS_UPDATED_AT == "1970-01-01T00:00:00.000Z"

    def test_any_single_fact_makes_it_factful(self) -> None:
        assert SeriesFacts(tag="HD Scan").has_facts()
        assert SeriesFacts(unit="chapters").has_facts()
        assert SeriesFacts(external_ids={"anilist": 1}).has_facts()
        assert SeriesFacts(titles={"native": "x"}).has_facts()
        assert SeriesFacts(synonyms=("x",)).has_facts()

    def test_blank_strings_are_not_facts(self) -> None:
        assert not SeriesFacts(tag="   ").has_facts()
        assert not SeriesFacts(synonyms=("", "  ")).has_facts()

    def test_has_facts_can_disagree_with_the_written_payload(self) -> None:
        """`has_facts()` checks the RAW fields; `_facts_payload` filters
        through the ID_KEYS/TITLE_KEYS/TRACKING_UNITS allowlists. A record
        holding only an unrecognised external-id provider and a
        non-canonical `unit` string is factful here (it has an opinion, so
        it must still win a facts merge and carry a real `updated_at`) even
        though every field the FILE actually writes ends up empty. Pinned so
        a later merge-rules task keys off `has_facts()` / `updated_at`,
        never off "does the payload look non-empty".
        """
        facts = SeriesFacts(
            external_ids={"kitsune": 7},
            unit="chapters-ish",
            updated_at="2026-08-18T19:36:24.324Z",
        )
        assert facts.has_facts()
        data = dump_series_file(
            series_title="S", facts=facts, index=SeriesIndexData(), volumes=[]
        ).decode("utf-8")
        assert data == (
            '{"version":2,"series_title":"S","external_ids":{},"titles":{},'
            '"synonyms":[],"updated_at":"2026-08-18T19:36:24.324Z","volumes":[]}'
        )


class TestDumpSeriesFile:
    def test_factless_series_with_one_volume(self) -> None:
        data = dump_series_file(
            series_title="Bakemonogatari",
            facts=SeriesFacts(),
            index=SeriesIndexData(),
            volumes=[
                VolumeEntry(
                    volume_uuid="cfb5220c-57db-4008-9f44-e659d794e381",
                    volume_title="v01",
                    page_count=187,
                    character_count=13247,
                    mokuro_version="0.2.2",
                    archive_size=1234,
                )
            ],
        )
        assert data.decode("utf-8") == (
            '{"version":2,"series_title":"Bakemonogatari","external_ids":{},"titles":{},'
            '"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z","volumes":['
            '{"volume_uuid":"cfb5220c-57db-4008-9f44-e659d794e381","volume_title":"v01",'
            '"page_count":187,"character_count":13247,"mokuro_version":"0.2.2",'
            '"archive_size":1234}]}'
        )

    def test_full_facts_offsets_and_natural_volume_order(self) -> None:
        data = dump_series_file(
            series_title="Dr Stone",
            facts=DR_STONE,
            index=SeriesIndexData(spine_offset=12.5, volume_offsets={"u10": -40, "u2": 0}),
            volumes=[
                VolumeEntry("u10", "Volume 10", 200, 10000, ""),
                VolumeEntry("u2", "Volume 2", 180, 9000, "0.2.2", spine_width=250.5,
                            archive_size=99),
            ],
        )
        assert data.decode("utf-8") == (
            '{"version":2,"series_title":"Dr Stone",'
            '"external_ids":{"anilist":98416,"mal":103897},'
            '"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},'
            '"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes",'
            '"spine_offset":12.5,"updated_at":"2026-08-18T19:36:24.324Z","volumes":['
            '{"volume_uuid":"u2","volume_title":"Volume 2","page_count":180,'
            '"character_count":9000,"mokuro_version":"0.2.2","spine_width":250.5,'
            '"archive_size":99},'
            '{"volume_uuid":"u10","volume_title":"Volume 10","page_count":200,'
            '"character_count":10000,"mokuro_version":"","offset":-40}]}'
        )

    def test_japanese_is_written_raw_not_escaped(self) -> None:
        data = dump_series_file(
            series_title="Dr Stone", facts=DR_STONE, index=SeriesIndexData(), volumes=[]
        )
        assert "ドクターストーン".encode() in data
        assert b"\\u30c9" not in data  # not `ensure_ascii`-escaped

    def test_unknown_ids_titles_and_units_never_reach_the_file(self) -> None:
        facts = SeriesFacts(
            external_ids={"anilist": 1, "kitsune": 7},
            titles={"native": "x", "klingon": "y"},
            updated_at="2026-08-18T19:36:24.324Z",
        )
        text = dump_series_file(
            series_title="S", facts=facts, index=SeriesIndexData(), volumes=[]
        ).decode("utf-8")
        assert '"external_ids":{"anilist":1}' in text
        assert '"titles":{"native":"x"}' in text
        assert "kitsune" not in text and "klingon" not in text

    def test_rebuild_of_unchanged_input_is_byte_identical(self) -> None:
        args = {
            "series_title": "Dr Stone",
            "facts": DR_STONE,
            "index": SeriesIndexData(spine_offset=12.5, volume_offsets={"u2": 3}),
            "volumes": [VolumeEntry("u2", "Volume 2", 180, 9000, "0.2.2")],
        }
        assert dump_series_file(**args) == dump_series_file(**args)  # type: ignore[arg-type]

    def test_invalid_spine_width_and_archive_size_are_dropped(self) -> None:
        # Ports the reader's `isSpineWidth`/`isArchiveSize`: a plain
        # truthiness check would keep these (both are nonzero in Python),
        # but neither is a usable measurement.
        volume = VolumeEntry(
            "u1", "Volume 1", 1, 1, "0.2.2", spine_width=-5.0, archive_size=-10
        )
        data = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(), volumes=[volume]
        ).decode("utf-8")
        assert "spine_width" not in data
        assert "archive_size" not in data

    def test_non_finite_spine_width_is_dropped(self) -> None:
        volume = VolumeEntry("u1", "Volume 1", 1, 1, "0.2.2", spine_width=float("inf"))
        data = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(), volumes=[volume]
        ).decode("utf-8")
        assert "spine_width" not in data

    def test_non_finite_numbers_are_refused_not_silently_written(self) -> None:
        # `allow_nan=False`: JSON has no NaN/Infinity token, so a value that
        # reaches this layer must fail loudly rather than produce invalid JSON.
        with pytest.raises(ValueError):
            dump_series_file(
                series_title="S",
                facts=SeriesFacts(),
                index=SeriesIndexData(spine_offset=float("nan")),
                volumes=[],
            )

    def test_integral_float_spine_offset_keeps_the_decimal_point(self) -> None:
        # Accepted divergence from `JSON.stringify`: callers (e.g. bunko's
        # config layer, which coerces offsets through `float()`) will
        # normally hand this an already-float value, so `12.0` ->
        # `"spine_offset":12.0` is the NORMAL path, not an edge case.
        # `JSON.stringify(12)` would write bare `12`, but both parse back to
        # the same number, so the byte divergence is accepted.
        data = dump_series_file(
            series_title="S",
            facts=SeriesFacts(),
            index=SeriesIndexData(spine_offset=12.0),
            volumes=[],
        )
        assert b'"spine_offset":12.0' in data

    def test_lone_surrogates_are_backslash_escaped_not_fatal(self) -> None:
        # Simulates a folder name that round-tripped through `os.scandir`
        # with `surrogateescape` (a non-UTF-8 byte becomes a lone low
        # surrogate in the resulting str), or an untrusted PUT body. Must
        # not abort compiling the rest of the catalog.
        name = "Dr\udcff"
        data = dump_series_file(
            series_title=name, facts=SeriesFacts(), index=SeriesIndexData(), volumes=[]
        )
        assert b"Dr\\udcff" in data


class TestFreshnessStamps:
    def test_all_four_present_sit_after_archive_size(self) -> None:
        data = dump_series_file(
            series_title="Bakemonogatari",
            facts=SeriesFacts(),
            index=SeriesIndexData(),
            volumes=[
                VolumeEntry(
                    volume_uuid="cfb5220c-57db-4008-9f44-e659d794e381",
                    volume_title="v01",
                    page_count=187,
                    character_count=13247,
                    mokuro_version="0.2.2",
                    archive_size=1234,
                    mokuro_size=45210,
                    mokuro_modified=1723996800,
                    cover_size=8192,
                    cover_modified=1723996900,
                )
            ],
        )
        assert data.decode("utf-8") == (
            '{"version":2,"series_title":"Bakemonogatari","external_ids":{},"titles":{},'
            '"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z","volumes":['
            '{"volume_uuid":"cfb5220c-57db-4008-9f44-e659d794e381","volume_title":"v01",'
            '"page_count":187,"character_count":13247,"mokuro_version":"0.2.2",'
            '"archive_size":1234,"mokuro_size":45210,"mokuro_modified":1723996800,'
            '"cover_size":8192,"cover_modified":1723996900}]}'
        )

    def test_stamps_sit_after_archive_size_and_before_offset(self) -> None:
        # Extends TestDumpSeriesFile.test_full_facts_offsets_and_natural_volume_order:
        # same fixture, "u2" gains stamps, "u10" keeps its trailing `offset`.
        data = dump_series_file(
            series_title="Dr Stone",
            facts=DR_STONE,
            index=SeriesIndexData(spine_offset=12.5, volume_offsets={"u10": -40, "u2": 0}),
            volumes=[
                VolumeEntry("u10", "Volume 10", 200, 10000, ""),
                VolumeEntry(
                    "u2", "Volume 2", 180, 9000, "0.2.2", spine_width=250.5,
                    archive_size=99, mokuro_size=15000, mokuro_modified=1700000100,
                    cover_size=4096, cover_modified=1700000200,
                ),
            ],
        )
        assert data.decode("utf-8") == (
            '{"version":2,"series_title":"Dr Stone",'
            '"external_ids":{"anilist":98416,"mal":103897},'
            '"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},'
            '"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes",'
            '"spine_offset":12.5,"updated_at":"2026-08-18T19:36:24.324Z","volumes":['
            '{"volume_uuid":"u2","volume_title":"Volume 2","page_count":180,'
            '"character_count":9000,"mokuro_version":"0.2.2","spine_width":250.5,'
            '"archive_size":99,"mokuro_size":15000,"mokuro_modified":1700000100,'
            '"cover_size":4096,"cover_modified":1700000200},'
            '{"volume_uuid":"u10","volume_title":"Volume 10","page_count":200,'
            '"character_count":10000,"mokuro_version":"","offset":-40}]}'
        )

    def test_a_zero_stamp_is_written_not_omitted(self) -> None:
        # Unlike spine_width/archive_size (truthy `> 0` checks), the four
        # stamps use `is not None`: a literal epoch mtime or an empty-file
        # size is 0, and a real (if practically impossible) stat value must
        # round-trip rather than silently vanish like a missing one would.
        volume = VolumeEntry(
            "u1", "v1", 1, 0, "", mokuro_size=0, mokuro_modified=0,
            cover_size=0, cover_modified=0,
        )
        text = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(), volumes=[volume]
        ).decode("utf-8")
        assert (
            '"mokuro_size":0,"mokuro_modified":0,"cover_size":0,"cover_modified":0'
        ) in text

    def test_stamps_are_omitted_not_nulled_when_absent(self) -> None:
        volume = VolumeEntry("u1", "Volume 1", 1, 1, "0.2.2")  # all four default None
        text = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(), volumes=[volume]
        ).decode("utf-8")
        for key in ("mokuro_size", "mokuro_modified", "cover_size", "cover_modified"):
            assert f'"{key}"' not in text
        assert "null" not in text


class TestSeriesFileDeterminism:
    def test_tied_natural_sort_keys_break_on_raw_title_text(self) -> None:
        # "volume 1" / "Volume 1" fold to the identical `natural_sort_key`;
        # only the compound (key, raw title) tiebreak makes the byte order
        # independent of input order — a bare stable sort would let whichever
        # one came first in `volumes` come first in the file.
        lower = VolumeEntry("u-lower", "volume 1", 1, 1, "0.2.2")
        upper = VolumeEntry("u-upper", "Volume 1", 1, 1, "0.2.2")
        forward = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(),
            volumes=[lower, upper],
        )
        backward = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(),
            volumes=[upper, lower],
        )
        assert forward == backward
        text = forward.decode("utf-8")
        # "Volume 1" < "volume 1" byte-wise (uppercase sorts first).
        assert text.index('"u-upper"') < text.index('"u-lower"')

    def test_duplicate_volume_uuid_keeps_only_the_first_occurrence(self) -> None:
        # Mirrors `parseSeriesFile`'s `seen.has(entry.volume_uuid)` skip, so
        # a document this writes never contains something the reader's own
        # parser would silently reduce further.
        first = VolumeEntry("dup", "First Copy", 10, 100, "0.2.2")
        second = VolumeEntry("dup", "Second Copy", 20, 200, "0.2.2")
        data = dump_series_file(
            series_title="S", facts=SeriesFacts(), index=SeriesIndexData(),
            volumes=[first, second],
        ).decode("utf-8")
        assert data.count('"volume_uuid":"dup"') == 1
        assert "First Copy" in data
        assert "Second Copy" not in data


class TestDumpCatalogFile:
    def test_entries_sorted_by_key_with_factless_series_included(self) -> None:
        data = dump_catalog_file([("Dr Stone", DR_STONE), ("Aria", SeriesFacts())])
        assert data.decode("utf-8") == (
            '{"version":1,"updated_at":"2026-08-18T19:36:24.324Z","series":['
            '{"series_title":"Aria","external_ids":{},"titles":{},"synonyms":[],'
            '"updated_at":"1970-01-01T00:00:00.000Z"},'
            '{"series_title":"Dr Stone","external_ids":{"anilist":98416,"mal":103897},'
            '"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},'
            '"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes",'
            '"updated_at":"2026-08-18T19:36:24.324Z"}]}'
        )

    def test_file_stamp_is_the_newest_entry_stamp_not_the_clock(self) -> None:
        first = dump_catalog_file([("Aria", SeriesFacts())])
        second = dump_catalog_file([("Aria", SeriesFacts())])
        assert first == second
        assert b'"updated_at":"1970-01-01T00:00:00.000Z","series"' in first

    def test_an_empty_library_still_produces_a_catalog(self) -> None:
        assert dump_catalog_file([]).decode("utf-8") == (
            '{"version":1,"updated_at":"1970-01-01T00:00:00.000Z","series":[]}'
        )

    def test_volume_data_never_leaks_into_the_catalog(self) -> None:
        text = dump_catalog_file([("Dr Stone", DR_STONE)]).decode("utf-8")
        # Substring-only check on "volumes" self-trips on DR_STONE's
        # `unit: "volumes"` fact value; assert on the array KEY instead.
        assert '"volumes":' not in text
        assert "spine_offset" not in text


class TestCatalogFileDeterminism:
    def test_reversed_input_order_produces_identical_bytes(self) -> None:
        entries = [("Zeta", SeriesFacts()), ("Alpha", SeriesFacts())]
        forward = dump_catalog_file(entries)
        backward = dump_catalog_file(list(reversed(entries)))
        assert forward == backward

    def test_duplicate_normalized_series_key_keeps_only_the_first(self) -> None:
        # Mirrors `parseCatalogFile`'s `seen.has(key)` skip, same reasoning
        # as the series-file volume dedup.
        first = ("Dr Stone", SeriesFacts(tag="First", updated_at="2026-08-18T19:36:24.324Z"))
        second = (
            "  dr   stone  ",
            SeriesFacts(tag="Second", updated_at="2026-08-19T00:00:00.000Z"),
        )
        data = dump_catalog_file([first, second]).decode("utf-8")
        assert data.count('"series_title"') == 1
        assert '"tag":"First"' in data
        assert "Second" not in data
