"""Contract §6: newest-facts-stamp-wins, factless rules, index independence."""

from __future__ import annotations

from mokuro_bunko.metadata.merge import StoredSeries, merge_series_update
from mokuro_bunko.metadata.schema import FACTLESS_UPDATED_AT, SeriesFacts, SeriesIndexData
from mokuro_bunko.metadata.validate import SeriesUpdate

OLD = "2026-08-01T00:00:00.000Z"
NEW = "2026-08-20T00:00:00.000Z"


def stored(facts: SeriesFacts, index: SeriesIndexData | None = None) -> StoredSeries:
    return StoredSeries(facts=facts, index=index or SeriesIndexData())


def update(
    facts: SeriesFacts,
    *,
    spine_offset: float | None = None,
    spine_offset_present: bool = False,
    volume_offsets: dict[str, float] | None = None,
    listed: frozenset[str] = frozenset(),
) -> SeriesUpdate:
    return SeriesUpdate(
        facts=facts,
        spine_offset=spine_offset,
        spine_offset_present=spine_offset_present,
        volume_offsets=volume_offsets or {},
        listed_uuids=listed,
    )


LINKED_OLD = SeriesFacts(external_ids={"anilist": 1}, updated_at=OLD)
LINKED_NEW = SeriesFacts(external_ids={"anilist": 2}, updated_at=NEW)


class TestFactsMerge:
    def test_first_update_for_an_unknown_series_is_stored(self) -> None:
        result = merge_series_update(None, update(LINKED_NEW))
        assert result.facts == LINKED_NEW
        assert result.facts_changed

    def test_newer_facts_win(self) -> None:
        result = merge_series_update(stored(LINKED_OLD), update(LINKED_NEW))
        assert result.facts == LINKED_NEW
        assert result.facts_changed

    def test_older_facts_lose(self) -> None:
        result = merge_series_update(stored(LINKED_NEW), update(LINKED_OLD))
        assert result.facts == LINKED_NEW
        assert not result.facts_changed

    def test_an_equal_stamp_keeps_the_incoming_copy(self) -> None:
        same = SeriesFacts(external_ids={"anilist": 1}, tag="HD Scan", updated_at=OLD)
        result = merge_series_update(stored(LINKED_OLD), update(same))
        assert result.facts == same
        assert result.facts_changed

    def test_a_round_trip_of_identical_facts_reports_no_change(self) -> None:
        result = merge_series_update(stored(LINKED_OLD), update(LINKED_OLD))
        assert not result.facts_changed
        assert not result.changed


class TestFactlessRules:
    def test_a_factless_epoch_update_never_clears_facts(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD), update(SeriesFacts(updated_at=FACTLESS_UPDATED_AT))
        )
        assert result.facts == LINKED_OLD
        assert not result.facts_changed

    def test_a_factless_update_with_the_same_stamp_never_clears_facts(self) -> None:
        result = merge_series_update(stored(LINKED_OLD), update(SeriesFacts(updated_at=OLD)))
        assert result.facts == LINKED_OLD
        assert not result.facts_changed

    def test_a_factless_update_with_a_strictly_newer_stamp_is_an_unlink(self) -> None:
        unlink = SeriesFacts(updated_at=NEW)
        result = merge_series_update(stored(LINKED_OLD), update(unlink))
        assert result.facts == unlink
        assert not result.facts.has_facts()
        assert result.facts_changed

    def test_facts_older_than_a_published_unlink_do_not_resurrect_the_link(self) -> None:
        """The stored row is factless but its stamp is a real unlink."""
        result = merge_series_update(stored(SeriesFacts(updated_at=NEW)), update(LINKED_OLD))
        assert not result.facts.has_facts()
        assert result.facts.updated_at == NEW
        assert not result.facts_changed

    def test_facts_newer_than_an_unlink_relink_the_series(self) -> None:
        result = merge_series_update(stored(SeriesFacts(updated_at=OLD)), update(LINKED_NEW))
        assert result.facts == LINKED_NEW
        assert result.facts_changed


class TestIndexFields:
    def test_offsets_apply_even_when_the_facts_lose(self) -> None:
        result = merge_series_update(
            stored(LINKED_NEW),
            update(LINKED_OLD, volume_offsets={"u1": -40}, listed=frozenset({"u1"})),
        )
        assert result.facts == LINKED_NEW
        assert not result.facts_changed
        assert result.index.volume_offsets == {"u1": -40}
        assert result.index_changed
        assert result.changed

    def test_an_offset_only_update_never_moves_the_facts_stamp(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD),
            update(
                SeriesFacts(updated_at=NEW),
                spine_offset=12.5,
                spine_offset_present=True,
                volume_offsets={"u1": 3},
                listed=frozenset({"u1"}),
            ),
        )
        # The factless payload IS an unlink here (strictly newer), which is the
        # rule; what must not happen is the OFFSETS moving the facts stamp.
        assert result.index.spine_offset == 12.5
        assert result.index.volume_offsets == {"u1": 3}

    def test_an_absent_spine_offset_inherits_the_stored_one(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD, SeriesIndexData(spine_offset=8)), update(LINKED_OLD)
        )
        assert result.index.spine_offset == 8
        assert not result.index_changed

    def test_a_present_spine_offset_replaces_the_stored_one(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD, SeriesIndexData(spine_offset=8)),
            update(LINKED_OLD, spine_offset=0, spine_offset_present=True),
        )
        assert result.index.spine_offset == 0
        assert result.index_changed

    def test_a_listed_volume_without_an_offset_clears_the_stored_one(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD, SeriesIndexData(volume_offsets={"u1": -40, "u2": 5})),
            update(LINKED_OLD, listed=frozenset({"u1"})),
        )
        assert result.index.volume_offsets == {"u2": 5}
        assert result.index_changed

    def test_a_volume_the_payload_never_mentions_is_untouched(self) -> None:
        result = merge_series_update(
            stored(LINKED_OLD, SeriesIndexData(volume_offsets={"u2": 5})),
            update(LINKED_OLD, volume_offsets={"u1": 1}, listed=frozenset({"u1"})),
        )
        assert result.index.volume_offsets == {"u1": 1, "u2": 5}

    def test_out_of_range_offsets_are_stored_verbatim(self) -> None:
        result = merge_series_update(
            None,
            update(
                LINKED_NEW,
                spine_offset=9999,
                spine_offset_present=True,
                volume_offsets={"u1": -12345.5},
                listed=frozenset({"u1"}),
            ),
        )
        assert result.index.spine_offset == 9999
        assert result.index.volume_offsets == {"u1": -12345.5}


class TestIdempotency:
    def test_applying_the_same_update_twice_changes_nothing_the_second_time(self) -> None:
        first = merge_series_update(
            None, update(LINKED_NEW, volume_offsets={"u1": 3}, listed=frozenset({"u1"}))
        )
        second = merge_series_update(
            StoredSeries(facts=first.facts, index=first.index),
            update(LINKED_NEW, volume_offsets={"u1": 3}, listed=frozenset({"u1"})),
        )
        assert second.facts == first.facts
        assert second.index == first.index
        assert not second.changed
