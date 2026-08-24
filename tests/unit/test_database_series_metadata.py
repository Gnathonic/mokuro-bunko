"""Series facts + compiled-entry cache storage (schema v3)."""

from __future__ import annotations

from pathlib import Path

import pytest

from mokuro_bunko.database import Database, SeriesFactsRow


def make_row(**overrides: object) -> SeriesFactsRow:
    row: SeriesFactsRow = {
        "series_key": "dr stone",
        "series_title": "Dr Stone",
        "external_ids": {"anilist": 98416},
        "titles": {"native": "Dr.STONE"},
        "synonyms": ["ドクターストーン"],
        "tag": "HD Scan",
        "unit": "volumes",
        "facts_updated_at": "2026-08-18T19:36:24.324Z",
        "spine_offset": 12.5,
        "volume_offsets": {"u1": -40},
        "updated_by": "alice",
        "updated_at": "",
    }
    row.update(overrides)  # type: ignore[typeddict-item]
    return row


@pytest.fixture
def db(tmp_path: Path) -> Database:
    return Database(tmp_path / "test.db")


class TestSchema:
    def test_schema_version_is_three(self, db: Database) -> None:
        assert Database.SCHEMA_VERSION == 3
        with db._connection() as conn:
            assert conn.execute("SELECT version FROM schema_version").fetchone()[0] == 3

    def test_upgrading_an_existing_database_adds_the_tables(self, tmp_path: Path) -> None:
        path = tmp_path / "old.db"
        Database(path).create_user("alice", "password123", "registered")
        upgraded = Database(path)
        assert upgraded.get_user("alice") is not None
        assert upgraded.list_series_facts() == []

    def test_a_v2_database_gains_the_tables_in_place(self, tmp_path: Path) -> None:
        """Simulate the real upgrade: no metadata tables, version stamped 2."""
        path = tmp_path / "v2.db"
        old = Database(path)
        old.create_user("alice", "password123", "registered")
        with old._connection() as conn:
            conn.execute("DROP TABLE series_facts")
            conn.execute("DROP TABLE series_entry_cache")
            conn.execute("UPDATE schema_version SET version = 2")

        upgraded = Database(path)
        assert upgraded.get_user("alice") is not None
        assert upgraded.list_series_facts() == []
        with upgraded._connection() as conn:
            assert conn.execute("SELECT version FROM schema_version").fetchone()[0] == 3
        upgraded.put_series_facts(make_row())
        assert upgraded.get_series_facts("dr stone") is not None

    def test_reopening_a_v3_database_keeps_its_rows(self, tmp_path: Path) -> None:
        path = tmp_path / "again.db"
        first = Database(path)
        first.put_series_facts(make_row())
        first.put_cached_volume_entry("Dr Stone/v01.cbz", "dr stone", {"volume_uuid": "u1"}, 1, 1.0, "")
        reopened = Database(path)
        assert reopened.get_series_facts("dr stone") is not None
        assert reopened.get_cached_volume_entry("Dr Stone/v01.cbz", 1, 1.0, "") == {
            "volume_uuid": "u1"
        }


class TestSeriesFacts:
    def test_round_trips_every_field(self, db: Database) -> None:
        db.put_series_facts(make_row())
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["series_title"] == "Dr Stone"
        assert stored["external_ids"] == {"anilist": 98416}
        assert stored["titles"] == {"native": "Dr.STONE"}
        assert stored["synonyms"] == ["ドクターストーン"]
        assert stored["tag"] == "HD Scan"
        assert stored["unit"] == "volumes"
        assert stored["facts_updated_at"] == "2026-08-18T19:36:24.324Z"
        assert stored["spine_offset"] == 12.5
        assert stored["volume_offsets"] == {"u1": -40}
        assert stored["updated_by"] == "alice"
        assert stored["updated_at"]

    def test_missing_series_is_none(self, db: Database) -> None:
        assert db.get_series_facts("nothing") is None

    def test_put_replaces_the_row_wholesale(self, db: Database) -> None:
        db.put_series_facts(make_row())
        db.put_series_facts(
            make_row(tag=None, unit=None, spine_offset=None, volume_offsets={}, updated_by="bob")
        )
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["tag"] is None
        assert stored["unit"] is None
        assert stored["spine_offset"] is None
        assert stored["volume_offsets"] == {}
        assert stored["updated_by"] == "bob"

    def test_list_returns_every_series(self, db: Database) -> None:
        db.put_series_facts(make_row())
        db.put_series_facts(make_row(series_key="aria", series_title="Aria"))
        assert {row["series_key"] for row in db.list_series_facts()} == {"dr stone", "aria"}

    def test_corrupt_json_columns_degrade_to_empty(self, db: Database) -> None:
        db.put_series_facts(make_row())
        with db._connection() as conn:
            conn.execute("UPDATE series_facts SET titles = 'not json'")
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["titles"] == {}

    def test_the_facts_stamp_is_stored_verbatim(self, db: Database) -> None:
        """The validator owns the clock; the row keeps what it emitted."""
        db.put_series_facts(make_row(facts_updated_at="1970-01-01T00:00:00.000Z"))
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["facts_updated_at"] == "1970-01-01T00:00:00.000Z"


class TestOffsetNumbers:
    """Alignment numbers reach this table verbatim from a client PUT."""

    def test_an_integer_spine_offset_stays_an_integer(self, db: Database) -> None:
        db.put_series_facts(make_row(spine_offset=-40, volume_offsets={"u1": -40, "u2": 1.5}))
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["spine_offset"] == -40
        assert isinstance(stored["spine_offset"], int)
        assert stored["volume_offsets"] == {"u1": -40, "u2": 1.5}
        assert isinstance(stored["volume_offsets"]["u1"], int)

    def test_a_zero_spine_offset_is_kept(self, db: Database) -> None:
        db.put_series_facts(make_row(spine_offset=0))
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["spine_offset"] == 0

    def test_an_unstorable_spine_offset_degrades_to_none(self, db: Database) -> None:
        db.put_series_facts(make_row(spine_offset=10**310))
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["spine_offset"] is None

    def test_a_non_finite_spine_offset_degrades_to_none(self, db: Database) -> None:
        db.put_series_facts(make_row(spine_offset=float("inf")))
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["spine_offset"] is None

    def test_an_oversized_volume_offset_still_round_trips(self, db: Database) -> None:
        db.put_series_facts(make_row(volume_offsets={"u1": 10**310}))
        stored = db.get_series_facts("dr stone")
        assert stored is not None
        assert stored["volume_offsets"] == {"u1": 10**310}


class TestEntryCache:
    ENTRY = {"volume_uuid": "u1", "volume_title": "v01", "page_count": 187}

    def test_hit_only_when_every_stat_matches(self, db: Database) -> None:
        db.put_cached_volume_entry(
            "Dr Stone/v01.cbz", "dr stone", self.ENTRY, 1234, 100.5, "v01.mokuro:99:50.25"
        )
        assert db.get_cached_volume_entry(
            "Dr Stone/v01.cbz", 1234, 100.5, "v01.mokuro:99:50.25"
        ) == self.ENTRY
        assert db.get_cached_volume_entry(
            "Dr Stone/v01.cbz", 9999, 100.5, "v01.mokuro:99:50.25"
        ) is None
        assert db.get_cached_volume_entry(
            "Dr Stone/v01.cbz", 1234, 100.75, "v01.mokuro:99:50.25"
        ) is None
        assert db.get_cached_volume_entry("Dr Stone/v01.cbz", 1234, 100.5, "") is None
        assert db.get_cached_volume_entry("Other/v01.cbz", 1234, 100.5, "") is None

    def test_put_overwrites_a_stale_entry(self, db: Database) -> None:
        db.put_cached_volume_entry("Dr Stone/v01.cbz", "dr stone", self.ENTRY, 1, 1.0, "")
        db.put_cached_volume_entry(
            "Dr Stone/v01.cbz", "dr stone", {"volume_uuid": "u2"}, 2, 2.0, ""
        )
        assert db.get_cached_volume_entry("Dr Stone/v01.cbz", 2, 2.0, "") == {"volume_uuid": "u2"}

    def test_prune_drops_only_the_keys_not_kept(self, db: Database) -> None:
        db.put_cached_volume_entry("A/v1.cbz", "a", self.ENTRY, 1, 1.0, "")
        db.put_cached_volume_entry("A/v2.cbz", "a", self.ENTRY, 1, 1.0, "")
        db.put_cached_volume_entry("B/v1.cbz", "b", self.ENTRY, 1, 1.0, "")
        assert db.prune_series_entry_cache({"A/v1.cbz", "B/v1.cbz"}) == 1
        assert db.get_cached_volume_entry("A/v2.cbz", 1, 1.0, "") is None
        assert db.get_cached_volume_entry("A/v1.cbz", 1, 1.0, "") == self.ENTRY

    def test_prune_with_nothing_stale_touches_nothing(self, db: Database) -> None:
        db.put_cached_volume_entry("A/v1.cbz", "a", self.ENTRY, 1, 1.0, "")
        assert db.prune_series_entry_cache({"A/v1.cbz"}) == 0
        assert db.get_cached_volume_entry("A/v1.cbz", 1, 1.0, "") == self.ENTRY

    def test_a_corrupt_cache_row_is_a_miss(self, db: Database) -> None:
        db.put_cached_volume_entry("A/v1.cbz", "a", self.ENTRY, 1, 1.0, "")
        with db._connection() as conn:
            conn.execute("UPDATE series_entry_cache SET entry_json = 'not json'")
        assert db.get_cached_volume_entry("A/v1.cbz", 1, 1.0, "") is None
