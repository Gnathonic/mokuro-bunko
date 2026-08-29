"""Merging an accepted update into the stored series state (contract §6).

Two independent merges happen here, and conflating them is the bug this file
exists to prevent:

* FACTS merge on the facts stamp — newest wins, ties keep the incoming copy
  (that is the same link round-tripping back), and a factless payload needs a
  STRICTLY newer stamp to win, because that is what an explicit unlink looks
  like and an epoch stamp means "no opinion".
* INDEX merge (the shelf alignment) by presence — a value the payload carries
  replaces the stored one, a value it omits is silence and inherits. It never
  touches the facts stamp, so a PUT carrying only offsets is still factless.

The facts rule below is the reader's `buildSeriesFile` rule rather than the
slightly looser `pickEntry` one: incoming facts must be at least as new as the
STORED STAMP even when the stored row is factless. bunko is the authority that
holds a published unlink, and a stale link arriving afterwards must not
resurrect it.
"""

from __future__ import annotations

from dataclasses import dataclass

from mokuro_bunko.metadata.schema import SeriesFacts, SeriesIndexData
from mokuro_bunko.metadata.validate import SeriesUpdate


@dataclass(frozen=True)
class StoredSeries:
    """What bunko currently holds for one series."""

    facts: SeriesFacts
    index: SeriesIndexData


@dataclass(frozen=True)
class MergeResult:
    facts: SeriesFacts
    index: SeriesIndexData
    facts_changed: bool
    index_changed: bool

    @property
    def changed(self) -> bool:
        """Did anything move? Drives "rewrite the files or not"."""
        return self.facts_changed or self.index_changed


def _merge_facts(stored: SeriesFacts | None, incoming: SeriesFacts) -> SeriesFacts:
    if stored is None:
        return incoming
    if incoming.has_facts():
        return incoming if incoming.updated_at >= stored.updated_at else stored
    # Factless: only a deliberate, strictly newer unlink wins.
    return incoming if incoming.updated_at > stored.updated_at else stored


def _merge_index(stored: SeriesIndexData | None, update: SeriesUpdate) -> SeriesIndexData:
    base = stored or SeriesIndexData()

    spine_offset = update.spine_offset if update.spine_offset_present else base.spine_offset

    offsets = dict(base.volume_offsets)
    for uuid in update.listed_uuids:
        if uuid in update.volume_offsets:
            offsets[uuid] = update.volume_offsets[uuid]
        else:
            # Listed without an offset: a positive statement that this volume
            # has no nudge, so a stored one is cleared.
            offsets.pop(uuid, None)
    return SeriesIndexData(spine_offset=spine_offset, volume_offsets=offsets)


def merge_series_update(stored: StoredSeries | None, update: SeriesUpdate) -> MergeResult:
    """Fold a validated update into the stored state."""
    facts = _merge_facts(stored.facts if stored else None, update.facts)
    index = _merge_index(stored.index if stored else None, update)
    return MergeResult(
        facts=facts,
        index=index,
        facts_changed=stored is None or facts != stored.facts,
        index_changed=stored is None or index != stored.index,
    )
