"""Golden bytes for the two compiled documents (contract §2 and §3)."""

from __future__ import annotations

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
