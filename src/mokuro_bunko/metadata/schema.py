"""The two compiled documents, and the one place that turns them into bytes.

Shapes are fixed by the reader client (`src/lib/metadata/series-file.ts`,
`catalog-file.ts`); its parsers ignore unknown keys and key order, but the
BYTES matter here for a different reason: clients version their caches on the
file's size/mtime (contract §4), so a rebuild that changed nothing must
produce exactly the same bytes. Hence a fixed key order, a fixed volume order,
and no wall-clock stamps anywhere.
"""

from __future__ import annotations

import json
from collections.abc import Sequence
from dataclasses import dataclass, field
from typing import Any

from mokuro_bunko.metadata.reader_compat import natural_sort_key, normalize_series_key

#: The stamp of a document whose facts come from nowhere. It must never be
#: "now": every merge takes the newest facts stamp, so a freshly stamped empty
#: file would beat every real link. The epoch loses every comparison, which is
#: exactly what "no opinion" means.
FACTLESS_UPDATED_AT = "1970-01-01T00:00:00.000Z"

ID_KEYS: tuple[str, ...] = ("anilist", "mal")
TITLE_KEYS: tuple[str, ...] = ("native", "romaji", "english")
TRACKING_UNITS: tuple[str, ...] = ("volumes", "chapters")


@dataclass(frozen=True)
class SeriesFacts:
    """The shareable half of a series: what `catalog.json` carries verbatim."""

    external_ids: dict[str, int] = field(default_factory=dict)
    titles: dict[str, str] = field(default_factory=dict)
    synonyms: tuple[str, ...] = ()
    tag: str | None = None
    unit: str | None = None
    updated_at: str = FACTLESS_UPDATED_AT

    def has_facts(self) -> bool:
        """Does this say anything shareable? (client: `hasSeriesFacts`)"""
        return bool(
            self.external_ids
            or self.titles
            or any(synonym.strip() for synonym in self.synonyms)
            or (self.tag or "").strip()
            or self.unit
        )


@dataclass(frozen=True)
class SeriesIndexData:
    """Shelf alignment: INDEX data, never facts, never moves the facts stamp."""

    spine_offset: float | None = None
    volume_offsets: dict[str, float] = field(default_factory=dict)


@dataclass(frozen=True)
class VolumeEntry:
    """One compiled volume. Offsets are applied at dump time, by uuid."""

    volume_uuid: str
    volume_title: str
    page_count: int
    character_count: int
    mokuro_version: str
    spine_width: float | None = None
    archive_size: int | None = None


def _facts_payload(facts: SeriesFacts) -> dict[str, Any]:
    """Facts in canonical key order, unknown providers/languages dropped."""
    payload: dict[str, Any] = {
        "external_ids": {
            key: facts.external_ids[key] for key in ID_KEYS if key in facts.external_ids
        },
        "titles": {key: facts.titles[key] for key in TITLE_KEYS if facts.titles.get(key)},
        "synonyms": list(facts.synonyms),
    }
    tag = (facts.tag or "").strip()
    if tag:
        payload["tag"] = tag
    if facts.unit in TRACKING_UNITS:
        payload["unit"] = facts.unit
    return payload


def _dumps(payload: Any) -> bytes:
    """Compact, UTF-8, unescaped — the client's `JSON.stringify` output."""
    return json.dumps(payload, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def dump_series_file(
    *,
    series_title: str,
    facts: SeriesFacts,
    index: SeriesIndexData,
    volumes: Sequence[VolumeEntry],
) -> bytes:
    """Serialize `<Series>/series.json` (contract §2)."""
    payload: dict[str, Any] = {"version": 2, "series_title": series_title}
    payload.update(_facts_payload(facts))
    # A zero offset is a deliberate reset on the client and is never written.
    if index.spine_offset:
        payload["spine_offset"] = index.spine_offset
    payload["updated_at"] = facts.updated_at

    entries: list[dict[str, Any]] = []
    for volume in sorted(volumes, key=lambda item: natural_sort_key(item.volume_title)):
        entry: dict[str, Any] = {
            "volume_uuid": volume.volume_uuid,
            "volume_title": volume.volume_title,
            "page_count": volume.page_count,
            "character_count": volume.character_count,
            "mokuro_version": volume.mokuro_version,
        }
        if volume.spine_width:
            entry["spine_width"] = volume.spine_width
        if volume.archive_size:
            entry["archive_size"] = volume.archive_size
        offset = index.volume_offsets.get(volume.volume_uuid)
        if offset:
            entry["offset"] = offset
        entries.append(entry)
    payload["volumes"] = entries
    return _dumps(payload)


def dump_catalog_file(entries: Sequence[tuple[str, SeriesFacts]]) -> bytes:
    """Serialize the root `catalog.json` (contract §3).

    The file's own `updated_at` is the NEWEST entry stamp, never the clock: it
    is informational (the merge key is per entry), and a wall-clock value would
    change the bytes on every rebuild and have every client re-download a file
    that did not change.
    """
    ordered = sorted(entries, key=lambda item: normalize_series_key(item[0]))
    series: list[dict[str, Any]] = []
    newest = FACTLESS_UPDATED_AT
    for series_title, facts in ordered:
        entry: dict[str, Any] = {"series_title": series_title}
        entry.update(_facts_payload(facts))
        entry["updated_at"] = facts.updated_at
        newest = max(newest, facts.updated_at)
        series.append(entry)
    return _dumps({"version": 1, "updated_at": newest, "series": series})
