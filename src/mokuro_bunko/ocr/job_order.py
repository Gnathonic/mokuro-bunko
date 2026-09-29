"""The one ordering rule of the OCR queue.

Pure: no filesystem, no clock, no engine registry. The background worker
calls :func:`order_jobs` to decide what runs next and the queue page shows
the very list the worker computed, so what the owner sees IS the processing
order instead of a second sort that happens to agree with it.

The rule, most significant first:

1. **Generation order.** Every job of a generation runs before any job of the
   one below it in ``ocr.generations``, so the whole library gets the first
   row's OCR layer before the second row starts. The order is the operator's
   list order and nothing else (``generation_rank``): rearranging the rows IS
   how the queue is prioritised, and the same order decides which job keeps
   normal OS priority.
2. **Round-robin across series.** Within a generation: every series' first
   pending volume, then every series' second, and so on, so one long series
   cannot hold up all the others.
3. **Reading order inside a series**; series visited in name order, each
   generation's round continuing after the series it served last
   (``last_served``: the worker's in-memory cursor, empty again after a
   restart). Both orders are a natural sort ("Volume 2" before "Volume 10",
   "第二巻" before "第十巻").
"""

from __future__ import annotations

import functools
import re
import unicodedata
from collections.abc import Callable, Iterable, Mapping
from typing import TypeVar, cast

T = TypeVar("T")

# (series, volume, generation id): what the rule needs to know about a job.
# The third element is the GENERATION, never the engine: two rows may run the
# same engine, and keyed by engine they would merge into one round-robin --
# a series pending on both would take positions 0 and 1 of the same round
# instead of each row taking its own turn.
JobKey = tuple[str, str, str]
# One element per run of the name, all of one shape so that a number is never
# compared with a string: (0, integer part, fraction digits) for a number,
# (1, 0, text) for anything else.
NaturalKey = tuple[tuple[int, int, str], ...]

_KANJI_DIGITS = {ch: value for value, ch in enumerate("〇一二三四五六七八九")}
_KANJI_UNITS = {"十": 10, "百": 100, "千": 1000}
_KANJI_NUMERALS = "".join(_KANJI_DIGITS) + "".join(_KANJI_UNITS)
# What a kanji numeral counts when it is a volume number ("三巻", "十一話").
_COUNTERS = "巻話章集"
# A decimal is ONE number ("2.5"); a "." with no digit after it is punctuation.
_NUMBER_RUNS = re.compile(
    rf"(?P<arabic>\d+)(?:\.(?P<fraction>\d+))?|(?P<kanji>[{_KANJI_NUMERALS}]+)"
)


def _kanji_value(run: str) -> int | None:
    """Value of a run of kanji numerals, or None when it is not a numeral.

    Two spellings: positional digits ("一〇" = 10, "二五" = 25) and the usual
    multiplicative one ("十一" = 11, "二十" = 20, "百二" = 102, "千九百" = 1900),
    where the units must get smaller from left to right and each takes at
    most one digit ("十十" and "二三十" are not numbers).
    """
    if all(ch in _KANJI_DIGITS for ch in run):
        return int("".join(str(_KANJI_DIGITS[ch]) for ch in run))
    total = 0
    digit: int | None = None
    last_unit = 10_000
    for ch in run:
        if ch in _KANJI_DIGITS:
            if digit is not None or ch == "〇":
                return None
            digit = _KANJI_DIGITS[ch]
        else:
            unit = _KANJI_UNITS[ch]
            if unit >= last_unit:
                return None
            total += (1 if digit is None else digit) * unit
            digit, last_unit = None, unit
    return total + (digit or 0)


def _stands_as_number(text: str, start: int, end: int) -> bool:
    """True when the kanji run ``text[start:end]`` is where a number would be.

    Kanji numerals are also ordinary letters (一番, 十字架, 三国志), so the run
    alone proves nothing. It counts as a number after 第, before a volume
    counter (巻 話 章 集), or when it is a whole word of its own: nothing but
    the ends of the name, spaces or punctuation around it ("鬼滅の刃 十一").
    """
    before = text[start - 1] if start else ""
    after = text[end] if end < len(text) else ""
    if before == "第" or (after and after in _COUNTERS):
        return True
    return not before.isalnum() and not after.isalnum()


@functools.lru_cache(maxsize=1 << 16)
def natural_key(text: str) -> NaturalKey:
    """Sort key that reads the numbers in a name as numbers.

    "Volume 2" sorts before "Volume 10", zero padding is ignored ("08" is
    8), letters compare case-insensitively, and the name is NFKC-normalised
    first so full-width digits and letters ("第１０巻") count as their ASCII
    forms. A decimal is one number wherever it stands, so "第2.5巻" falls
    between "第2巻" and "第3巻" (fraction digits compare as a decimal
    fraction: 10.25 before 10.5). Kanji numerals count as numbers, on the
    same scale as digits ("第二巻" = "第2巻"), where a number would stand and
    stay text inside a word: see :func:`_stands_as_number`.

    Each run is tagged with its kind, so a number is never compared with a
    string; a number sorts before text in the same place ("Vol 1" before
    "Vol A") and a name sorts before any longer name it starts.

    Names that differ only in case, width, zero padding or numeral script
    share a key; callers that need a total order add the raw name as a
    tie-break (see :func:`order_jobs`).

    Memoised: the queue is re-ordered after every job and on every upload,
    over the same few thousand names, and the key is a pure function of the
    name (an immutable tuple, safe to share).
    """
    folded = unicodedata.normalize("NFKC", text).casefold()
    key: list[tuple[int, int, str]] = []
    text_from = 0  # start of the text run being collected

    def number(match: re.Match[str], value: int, fraction: str = "") -> None:
        nonlocal text_from
        if match.start() > text_from:
            key.append((1, 0, folded[text_from : match.start()]))
        key.append((0, value, fraction))
        text_from = match.end()

    for match in _NUMBER_RUNS.finditer(folded):
        kanji = match["kanji"]
        if kanji is None:
            # Digit by digit: "\d" also matches decimal digits of other scripts.
            digits = "".join(str(int(ch)) for ch in match["fraction"] or "")
            number(match, int(match["arabic"]), digits.rstrip("0"))
            continue
        value = _kanji_value(kanji)
        if value is not None and _stands_as_number(folded, match.start(), match.end()):
            number(match, value)
        # Otherwise the run is part of a word and stays in the text run.
    if text_from < len(folded):
        key.append((1, 0, folded[text_from:]))
    return tuple(key)


def _name_key(name: str) -> tuple[NaturalKey, str]:
    """Natural order, raw name as the deterministic tie-break."""
    return natural_key(name), name


def order_jobs(
    jobs: Iterable[T],
    generation_rank: Mapping[str, int],
    *,
    key: Callable[[T], JobKey] | None = None,
    last_served: Mapping[str, str] | None = None,
) -> list[T]:
    """Return ``jobs`` in the order the worker processes them.

    Args:
        jobs: The pending jobs. Pass ONLY jobs that can run now: leave out
            the one in flight and those waiting out a failure backoff (see
            "position" below for why that matters).
        generation_rank: Generation id -> its position in ``ocr.generations``
            among the enabled rows, 0 = runs first. A generation missing from
            the mapping runs after every ranked one.
        key: Maps a job to its ``(series, volume, generation id)``. Defaults
            to the job itself being such a tuple.
        last_served: Generation id -> the series that generation served most
            recently. The series visit order of that generation starts just
            AFTER it and wraps around.

    Sort key: ``(generation rank, position of the volume within its series,
    series, volume)``.

    **Position** is the index of the volume among the jobs *passed in* for
    the same series and generation, in natural order: that is, among the
    series' PENDING volumes for that generation, not among all its volumes.
    A series with
    only volume 7 left therefore has volume 7 at position 0 and takes part in
    the first round, rather than waiting for a seventh round behind every
    other series. For the same reason a volume in failure backoff must not
    be passed in: it would occupy its series' turn without being runnable,
    and excluded, the series' next volume simply takes the turn.

    **Why ``last_served``.** The worker recomputes the queue after every job
    (so a newly uploaded volume gets its fast layer next). Positions are
    over pending volumes, so once series A's first volume is done its second
    volume is at position 0 again; recomputed naively, A (first by name)
    would be served over and over. Starting each round after the series
    served last passes the turn on, and makes the order stable under
    execution: run the head of the list, record its series, recompute, and
    the rest of the list is unchanged. That is what lets the queue page show
    a list and the worker follow it.
    """
    get_key: Callable[[T], JobKey] = key if key is not None else (lambda job: cast(JobKey, job))
    keyed = [(get_key(job), job) for job in jobs]

    volumes: dict[tuple[str, str], set[str]] = {}
    for (series, volume, generation), _ in keyed:
        volumes.setdefault((generation, series), set()).add(volume)
    position = {
        (generation, series, volume): index
        for (generation, series), names in volumes.items()
        for index, volume in enumerate(sorted(names, key=_name_key))
    }

    cursors = {
        generation: _name_key(series) for generation, series in (last_served or {}).items()
    }
    unranked = len(generation_rank)

    def sort_key(item: tuple[JobKey, T]) -> tuple[object, ...]:
        (series, volume, generation), _ = item
        series_key = _name_key(series)
        cursor = cursors.get(generation)
        wrapped = 0 if cursor is None or series_key > cursor else 1
        return (
            generation_rank.get(generation, unranked),
            generation,
            position[(generation, series, volume)],
            wrapped,
            series_key,
            _name_key(volume),
        )

    return [job for _, job in sorted(keyed, key=sort_key)]
