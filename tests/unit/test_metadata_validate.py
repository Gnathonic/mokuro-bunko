"""Contract §6: validate the facts, keep the offsets verbatim, ignore the rest."""

from __future__ import annotations

import json

from mokuro_bunko.metadata.validate import parse_series_update


def payload(**overrides: object) -> bytes:
    body: dict[str, object] = {
        "version": 2,
        "series_title": "Dr Stone",
        "external_ids": {"anilist": 98416},
        "titles": {"native": "Dr.STONE"},
        "synonyms": ["ドクターストーン"],
        "updated_at": "2026-08-18T19:36:24.324Z",
        "volumes": [],
    }
    body.update(overrides)
    return json.dumps(body).encode("utf-8")


class TestFacts:
    def test_accepts_a_well_formed_update(self) -> None:
        update = parse_series_update(payload(tag="HD Scan", unit="volumes"))
        assert update is not None
        assert update.facts.external_ids == {"anilist": 98416}
        assert update.facts.titles == {"native": "Dr.STONE"}
        assert update.facts.synonyms == ("ドクターストーン",)
        assert update.facts.tag == "HD Scan"
        assert update.facts.unit == "volumes"
        assert update.facts.updated_at == "2026-08-18T19:36:24.324Z"
        assert update.facts.has_facts()

    def test_drops_unknown_providers_languages_and_units(self) -> None:
        update = parse_series_update(
            payload(
                external_ids={"anilist": 98416, "kitsune": 7, "mal": "103897", "bad": -1},
                titles={"native": "Dr.STONE", "klingon": "x", "romaji": "  "},
                unit="chapters-ish",
            )
        )
        assert update is not None
        assert update.facts.external_ids == {"anilist": 98416}
        assert update.facts.titles == {"native": "Dr.STONE"}
        assert update.facts.unit is None

    def test_drops_blank_synonyms_and_a_blank_tag(self) -> None:
        update = parse_series_update(payload(synonyms=["", "  ", "x", 5], tag="   "))
        assert update is not None
        assert update.facts.synonyms == ("x",)
        assert update.facts.tag is None

    def test_ignores_unknown_top_level_keys(self) -> None:
        update = parse_series_update(payload(read_count=9, tracking={"last_pushed": {}}))
        assert update is not None
        assert update.facts.has_facts()

    def test_a_factless_payload_keeps_its_own_stamp(self) -> None:
        update = parse_series_update(
            payload(external_ids={}, titles={}, synonyms=[], updated_at="2026-08-19T00:00:00.000Z")
        )
        assert update is not None
        assert not update.facts.has_facts()
        assert update.facts.updated_at == "2026-08-19T00:00:00.000Z"


class TestRejection:
    def test_rejects_non_json_and_non_objects(self) -> None:
        assert parse_series_update(b"not json") is None
        assert parse_series_update(b"[]") is None
        assert parse_series_update(b"") is None
        assert parse_series_update(b"\xff\xfe") is None

    def test_rejects_unknown_versions(self) -> None:
        assert parse_series_update(payload(version=3)) is None
        assert parse_series_update(payload(version="2")) is None
        # `True == 1` in Python; the client's strict `!==` rejects it, so we must.
        assert parse_series_update(payload(version=True)) is None

    def test_rejects_a_missing_or_unparsable_stamp(self) -> None:
        assert parse_series_update(payload(updated_at="Aug 16 2020")) is None
        assert parse_series_update(payload(updated_at=None)) is None

    def test_rejects_nan_and_infinity(self) -> None:
        assert parse_series_update(b'{"version":2,"updated_at":"2026-08-18T19:36:24.324Z",'
                                   b'"spine_offset":NaN}') is None
        assert parse_series_update(b'{"version":2,"updated_at":"2026-08-18T19:36:24.324Z",'
                                   b'"spine_offset":Infinity}') is None

    def test_clamps_a_far_future_stamp_instead_of_trusting_it(self) -> None:
        update = parse_series_update(
            payload(updated_at="2999-01-01T00:00:00.000Z"), now=1_800_000_000.0
        )
        assert update is not None
        assert update.facts.updated_at == "2027-01-15T08:00:00.000Z"


class TestIndexFields:
    def test_offsets_are_preserved_verbatim_never_clamped(self) -> None:
        update = parse_series_update(
            payload(
                spine_offset=9999,
                volumes=[{"volume_uuid": "u1", "offset": -12345.5}],
            )
        )
        assert update is not None
        assert update.spine_offset == 9999
        assert update.spine_offset_present is True
        assert update.volume_offsets == {"u1": -12345.5}

    def test_an_absent_spine_offset_is_silence_not_a_reset(self) -> None:
        update = parse_series_update(payload())
        assert update is not None
        assert update.spine_offset_present is False
        assert update.spine_offset is None

    def test_a_listed_volume_without_an_offset_is_recorded_as_listed(self) -> None:
        update = parse_series_update(
            payload(volumes=[{"volume_uuid": "u1"}, {"volume_uuid": "u2", "offset": 4}])
        )
        assert update is not None
        assert update.listed_uuids == frozenset({"u1", "u2"})
        assert update.volume_offsets == {"u2": 4}

    def test_everything_else_in_a_volume_entry_is_discarded(self) -> None:
        update = parse_series_update(
            payload(
                volumes=[
                    {
                        "volume_uuid": "u1",
                        "volume_title": "LIES",
                        "page_count": 99999,
                        "character_count": 1,
                        "mokuro_version": "9.9",
                        "archive_size": 5,
                    }
                ]
            )
        )
        assert update is not None
        assert update.listed_uuids == frozenset({"u1"})
        assert update.volume_offsets == {}

    def test_junk_volume_entries_are_skipped_individually(self) -> None:
        update = parse_series_update(
            payload(volumes=["nope", {"volume_uuid": ""}, {"volume_uuid": "u1", "offset": "x"},
                             {"volume_uuid": "u2", "offset": 3}])
        )
        assert update is not None
        assert update.listed_uuids == frozenset({"u1", "u2"})
        assert update.volume_offsets == {"u2": 3}

    def test_a_huge_integer_offset_cannot_crash_the_parse(self) -> None:
        # 310 digits. `float()` raises OverflowError on this one (309 is fine),
        # and the reader's `JSON.parse` turns it into `Infinity`, which its own
        # finite check then drops. Same answer here: absent, not an exception.
        huge = 10**309
        update = parse_series_update(
            payload(spine_offset=huge, volumes=[{"volume_uuid": "u1", "offset": huge}])
        )
        assert update is not None
        assert update.spine_offset is None
        assert update.spine_offset_present is False
        assert update.listed_uuids == frozenset({"u1"})
        assert update.volume_offsets == {}

    def test_an_integer_spine_offset_stays_an_integer(self) -> None:
        update = parse_series_update(payload(spine_offset=-40))
        assert update is not None
        assert update.spine_offset == -40
        # Verbatim: a `float()` here would republish an untouched `-40` as
        # `-40.0`, changing bytes the client versions its cache on.
        assert not isinstance(update.spine_offset, float)

    def test_a_zero_offset_is_absence_not_a_value(self) -> None:
        # Both levels: `parseSeriesFile` and `parseVolumeEntry` each drop a
        # falsy offset at the file boundary, so a reset arrives as "no value".
        update = parse_series_update(
            payload(spine_offset=0, volumes=[{"volume_uuid": "u1", "offset": 0}])
        )
        assert update is not None
        assert update.spine_offset_present is False
        assert update.spine_offset is None
        assert update.listed_uuids == frozenset({"u1"})
        assert update.volume_offsets == {}

    def test_a_duplicate_volume_uuid_keeps_the_first_entry(self) -> None:
        later_offset = parse_series_update(
            payload(volumes=[{"volume_uuid": "u1"}, {"volume_uuid": "u1", "offset": 4}])
        )
        assert later_offset is not None
        assert later_offset.listed_uuids == frozenset({"u1"})
        assert later_offset.volume_offsets == {}

        later_offsetless = parse_series_update(
            payload(volumes=[{"volume_uuid": "u1", "offset": 4}, {"volume_uuid": "u1"}])
        )
        assert later_offsetless is not None
        assert later_offsetless.volume_offsets == {"u1": 4}

    def test_a_non_list_volumes_key_is_not_fatal(self) -> None:
        update = parse_series_update(payload(volumes="nope"))
        assert update is not None
        assert update.listed_uuids == frozenset()
