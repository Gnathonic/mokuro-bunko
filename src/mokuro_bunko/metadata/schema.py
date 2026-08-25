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
import math
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
        """Does this say anything shareable? (client: `hasSeriesFacts`)

        Checks the RAW fields, not the allowlisted payload `_facts_payload`
        writes: a record holding only an unrecognised external-id provider or
        a non-canonical `unit` string is still factful here — it has an
        opinion, so it must win a facts merge and carry a real `updated_at` —
        even though `_facts_payload` drops those same values as unknown and
        the file it produces can end up with every facts field empty. That
        disagreement is intentional (pinned by
        `test_has_facts_can_disagree_with_the_written_payload`): a merge rule
        must key off `has_facts()` / `updated_at`, never off "does the
        payload look non-empty".
        """
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
    mokuro_size: int | None = None
    mokuro_modified: int | None = None
    cover_size: int | None = None
    cover_modified: int | None = None


def _is_spine_width(value: float | None) -> bool:
    """A usable spine width: a positive finite number of pixels.

    Port of the client's `isSpineWidth` (`series-file.ts`). A plain
    truthiness check would keep a negative width or NaN — both are truthy in
    Python — as a real measurement.
    """
    return value is not None and math.isfinite(value) and value > 0


def _is_archive_size(value: int | None) -> bool:
    """A usable archive size: a positive whole number of bytes.

    Port of the client's `isArchiveSize` (`series-file.ts`). `archive_size`
    is typed `int` here, so unlike the client (whose numbers are always
    floats) there is no separate "is it a whole number" check to make.
    """
    return value is not None and value > 0


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
    """Compact, UTF-8, unescaped — the client's `JSON.stringify` output.

    `allow_nan=False`: JSON has no `NaN`/`Infinity`/`-Infinity` token, so a
    non-finite number reaching this layer must fail loudly rather than
    silently produce invalid JSON (Python's default encoder would otherwise
    accept it and emit the literal identifier).

    `backslashreplace` on encode: a lone surrogate — half a broken UTF-16
    pair, as `os.scandir`'s `surrogateescape` error handler produces for a
    non-UTF-8 folder name, or as an untrusted PUT body can carry — cannot be
    encoded to strict UTF-8 and would otherwise abort compiling the whole
    document. `backslashreplace` writes it as its literal `\\uXXXX` escape
    instead, which is byte-identical to what a modern JS engine's
    `JSON.stringify` emits for the same unpaired surrogate: ES2019's
    "well-formed JSON.stringify" change replaced raw unpaired surrogates in
    its output with exactly this escape sequence.
    """
    text = json.dumps(payload, ensure_ascii=False, separators=(",", ":"), allow_nan=False)
    return text.encode("utf-8", "backslashreplace")


def _dedup_by_uuid(volumes: Sequence[VolumeEntry]) -> list[VolumeEntry]:
    """First occurrence per `volume_uuid` wins.

    Mirrors `parseSeriesFile`'s `seen.has(entry.volume_uuid)` skip, so a
    document this writes never contains something the reader's own parser
    would silently reduce further — bunko's output survives the reader's
    parse unchanged. A backstop, not a merge decision: by the time entries
    reach this dumb serializer, an upstream merge step should already have
    resolved which copy of a duplicated volume wins.
    """
    seen: set[str] = set()
    deduped: list[VolumeEntry] = []
    for volume in volumes:
        if volume.volume_uuid in seen:
            continue
        seen.add(volume.volume_uuid)
        deduped.append(volume)
    return deduped


def _dedup_by_series_key(
    entries: Sequence[tuple[str, SeriesFacts]],
) -> list[tuple[str, SeriesFacts]]:
    """First occurrence per normalized series key wins.

    Mirrors `parseCatalogFile`'s `seen.has(key)` skip, for the same reason
    `_dedup_by_uuid` mirrors `parseSeriesFile`'s.
    """
    seen: set[str] = set()
    deduped: list[tuple[str, SeriesFacts]] = []
    for series_title, facts in entries:
        key = normalize_series_key(series_title)
        if key in seen:
            continue
        seen.add(key)
        deduped.append((series_title, facts))
    return deduped


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
    # Tiebreak on the raw title: `natural_sort_key` folds case/accents, so
    # distinct titles ("Volume 1" / "volume 1") can tie — without a secondary
    # key, Python's stable sort would let whichever volume came first in the
    # input come first in the file, making the bytes depend on input order.
    ordered_volumes = sorted(
        _dedup_by_uuid(volumes),
        key=lambda item: (natural_sort_key(item.volume_title), item.volume_title),
    )
    for volume in ordered_volumes:
        entry: dict[str, Any] = {
            "volume_uuid": volume.volume_uuid,
            "volume_title": volume.volume_title,
            "page_count": volume.page_count,
            "character_count": volume.character_count,
            "mokuro_version": volume.mokuro_version,
        }
        if _is_spine_width(volume.spine_width):
            entry["spine_width"] = volume.spine_width
        if _is_archive_size(volume.archive_size):
            entry["archive_size"] = volume.archive_size
        # `is not None`, not truthy: 0 is a real (if practically impossible)
        # stat value and must round-trip, unlike a missing spine_width/size.
        if volume.mokuro_size is not None:
            entry["mokuro_size"] = volume.mokuro_size
        if volume.mokuro_modified is not None:
            entry["mokuro_modified"] = volume.mokuro_modified
        if volume.cover_size is not None:
            entry["cover_size"] = volume.cover_size
        if volume.cover_modified is not None:
            entry["cover_modified"] = volume.cover_modified
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
    # Same tiebreak reasoning as `dump_series_file`'s volume order; dedup
    # (below) collapses ties keyed on the normalized title down to one entry
    # in practice, but the compound key keeps the sort itself total.
    ordered = sorted(
        _dedup_by_series_key(entries),
        key=lambda item: (normalize_series_key(item[0]), item[0]),
    )
    series: list[dict[str, Any]] = []
    newest = FACTLESS_UPDATED_AT
    for series_title, facts in ordered:
        entry: dict[str, Any] = {"series_title": series_title}
        entry.update(_facts_payload(facts))
        entry["updated_at"] = facts.updated_at
        newest = max(newest, facts.updated_at)
        series.append(entry)
    return _dumps({"version": 1, "updated_at": newest, "series": series})
