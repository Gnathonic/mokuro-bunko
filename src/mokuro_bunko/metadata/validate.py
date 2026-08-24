"""The untrusted boundary: a scoped user's `series.json` PUT (contract §6).

Anyone with an account can send this, so every field is re-validated. Only the
FACTS are validated as facts; the `volumes` array is the client's own index,
which bunko does not trust at all — the single thing read out of it is each
entry's `offset`, matched by `volume_uuid`. The body's `series_title` is
ignored for the same reason: the request URL names the series folder, and that
is the only name the interception acts on.

The alignment numbers (`spine_offset`, per-entry `offset`) are stored VERBATIM
— the value as JSON delivered it, not a coercion of it. Bunko deliberately does
not clamp or range-check them either: every reader clamps on parse (±50 % /
±500 px), so one side owns the range rule and the two can never disagree about
what a stored value means. Verbatim also means an integer nudge republishes as
`-40`, not `-40.0`; the compiled bytes are a cache key (contract §4), so a value
nobody edited must not come back changed.

Mirrors the client's `parseSeriesFile` (`src/lib/metadata/series-file.ts`) and
the `sanitize*` helpers it calls, minus the fields bunko compiles itself. Two
places where the mirror is deliberately imperfect:

- An `external_ids` value of `98416.0` is rejected here and accepted there
  (`Number.isInteger` is true for a whole float). Unreachable from a real
  client: `JSON.stringify` writes a whole number without a fractional part, so
  only a hand-built body can hold that spelling, and this side is the stricter
  one.
- `0` at both offset levels is ABSENCE here, matching what `parseSeriesFile`
  and `parseVolumeEntry` do at the file boundary (both drop a falsy offset).
  The reader's "a `0` is a real value, the deliberate reset" rule lives one
  layer further in, in its own store, and is none of bunko's business: a reset
  reaches this side as an omitted field, which is exactly how it is read.
"""

from __future__ import annotations

import json
import math
from dataclasses import dataclass
from typing import Any, TypeGuard

from mokuro_bunko.metadata.reader_compat import normalize_updated_at
from mokuro_bunko.metadata.schema import ID_KEYS, TITLE_KEYS, TRACKING_UNITS, SeriesFacts


@dataclass(frozen=True)
class SeriesUpdate:
    """A validated update REQUEST — not a file, and not authoritative."""

    facts: SeriesFacts
    spine_offset: float | None
    #: Absence is silence (inherit what is stored); presence replaces.
    spine_offset_present: bool
    volume_offsets: dict[str, float]
    #: Volumes the payload named at all. An entry listed WITHOUT an offset is a
    #: positive statement ("this volume has no nudge") and clears a stored one.
    listed_uuids: frozenset[str]


def _reject_constant(name: str) -> Any:
    raise ValueError(f"unsupported JSON constant: {name}")


def _is_offset(value: Any) -> TypeGuard[float]:
    """A usable alignment number: real, finite, and not the falsy reset.

    A `TypeGuard` rather than a plain `bool` so the value it vouches for can be
    stored exactly as it arrived — narrowing is the only reason it exists, since
    coercing with `float()` is precisely what must not happen here.

    `math.isfinite` is what rules out `NaN`/`Infinity`, and it is called inside
    a `try` because a large enough JSON *integer* literal has no float at all:
    `{"spine_offset": 1e309-ish as 310 digits}` is legal JSON that Python parses
    to an exact `int`, and converting it raises `OverflowError` rather than
    returning `inf`. The reader's `JSON.parse` yields `Infinity` for that same
    body and its finite check drops it, so dropping it is also the parity
    answer — the point is that it must not escape as an exception from a
    routine whose whole contract is "never raise on foreign input".

    `True` is an `int` in Python and is excluded; `0` is excluded as absence
    (see the module docstring).
    """
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return False
    try:
        return math.isfinite(value) and value != 0
    except OverflowError:
        return False


def _facts_from(raw: dict[str, Any], updated_at: str) -> SeriesFacts:
    external_ids: dict[str, int] = {}
    raw_ids = raw.get("external_ids")
    if isinstance(raw_ids, dict):
        for key in ID_KEYS:
            value = raw_ids.get(key)
            if isinstance(value, int) and not isinstance(value, bool) and value > 0:
                external_ids[key] = value

    titles: dict[str, str] = {}
    raw_titles = raw.get("titles")
    if isinstance(raw_titles, dict):
        for key in TITLE_KEYS:
            value = raw_titles.get(key)
            if isinstance(value, str) and value.strip():
                titles[key] = value

    raw_synonyms = raw.get("synonyms")
    synonyms = tuple(
        value
        for value in (raw_synonyms if isinstance(raw_synonyms, list) else [])
        if isinstance(value, str) and value.strip()
    )

    raw_tag = raw.get("tag")
    tag = raw_tag.strip() if isinstance(raw_tag, str) and raw_tag.strip() else None

    raw_unit = raw.get("unit")
    unit = raw_unit if raw_unit in TRACKING_UNITS else None

    return SeriesFacts(
        external_ids=external_ids,
        titles=titles,
        synonyms=synonyms,
        tag=tag,
        unit=unit,
        updated_at=updated_at,
    )


def parse_series_update(payload: bytes, *, now: float | None = None) -> SeriesUpdate | None:
    """Validate a PUT body. `None` means "reject with an ordinary error"."""
    try:
        decoded = json.loads(payload.decode("utf-8"), parse_constant=_reject_constant)
    except (UnicodeDecodeError, ValueError):
        return None
    if not isinstance(decoded, dict):
        return None
    # `True == 1` in Python, so a bare `in (1, 2)` would admit `"version": true`
    # where the client's strict `!==` rejects it.
    version = decoded.get("version")
    if isinstance(version, bool) or version not in (1, 2):
        return None

    updated_at = normalize_updated_at(decoded.get("updated_at"), now=now)
    if updated_at is None:
        return None

    raw_spine = decoded.get("spine_offset")
    spine_offset: float | None = raw_spine if _is_offset(raw_spine) else None
    spine_offset_present = spine_offset is not None

    volume_offsets: dict[str, float] = {}
    listed: set[str] = set()
    raw_volumes = decoded.get("volumes")
    if isinstance(raw_volumes, list):
        for raw_entry in raw_volumes:
            if not isinstance(raw_entry, dict):
                continue
            uuid = raw_entry.get("volume_uuid")
            if not isinstance(uuid, str) or not uuid.strip():
                continue
            # First entry wins, exactly as `parseSeriesFile` dedupes: a repeated
            # uuid is malformed either way, and the two sides must not disagree
            # about which of the twins they kept.
            if uuid in listed:
                continue
            listed.add(uuid)
            offset = raw_entry.get("offset")
            if _is_offset(offset):
                volume_offsets[uuid] = offset

    return SeriesUpdate(
        facts=_facts_from(decoded, updated_at),
        spine_offset=spine_offset,
        spine_offset_present=spine_offset_present,
        volume_offsets=volume_offsets,
        listed_uuids=frozenset(listed),
    )
