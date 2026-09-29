"""Real throughput: pages completed over the wall seconds they took.

Every speed a PERSON is shown -- the queue page's per-layer line, a running
volume's rate at ``detailed``, the admin panel's per-processor table -- is
this and nothing else: the pages of the volumes a machine really finished,
divided by the seconds those volumes really took (the ``volume_done`` event's
``pages`` / ``seconds``).

Where each volume's END is known too, the seconds are the WALL CLOCK the
volumes' windows (``end - seconds`` to ``end``) cover together, not their
sum. A pipelined session has the next volume in flight before the last one
is out, so summed seconds count every overlap twice: measured, the admin
panel's Processors card read 7.3-7.8 pages/s against an 11.0 benchmark on an
idle tower, 14% of summed seconds overlap on average and 12-50% in bursts.
Time between volumes, when nothing was running, is in no window and is not
counted either: this is how fast the machine reads while it reads.

It is deliberately NOT what `ocr.eta` predicts with. The ETA model fits
``seconds = latency + pages * b`` and uses ``1 / b`` as its page rate: the
MARGINAL cost of one more page, which is the right thing to multiply a
remaining page count by and the wrong thing to call a speed. On a machine
whose volumes carry a large fixed cost, that slope reads several times what
the machine actually delivers (measured: a fitted ~70 pages/s beside a real
~25), so a display built on it promised throughput no volume ever saw.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass
from typing import Any

# How many of a machine's most recent finished volumes its throughput is
# averaged over: enough to smooth one odd volume, few enough to follow a
# change of pools or hardware within an evening's work.
RECENT_VOLUMES = 20


@dataclass(frozen=True)
class Throughput:
    """Pages over seconds of ``volumes`` finished volumes, newest at ``last_at``."""

    pages: float
    seconds: float
    volumes: int
    last_at: float | None = None

    @property
    def pages_per_second(self) -> float:
        return self.pages / self.seconds

    @property
    def pages_per_minute(self) -> float:
        return self.pages_per_second * 60.0

    def as_dict(self) -> dict[str, Any]:
        return {
            "pages_per_minute": round(self.pages_per_minute, 1),
            "volumes": self.volumes,
            "last_at": self.last_at,
        }


def _number(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    return number if number == number else None


def covered_seconds(windows: Iterable[tuple[float, float]]) -> float:
    """How much of the clock the ``(start, end)`` windows cover together."""
    total = 0.0
    span: tuple[float, float] | None = None
    for start, end in sorted(windows):
        if span is not None and start <= span[1]:
            span = (span[0], max(span[1], end))
            continue
        if span is not None:
            total += span[1] - span[0]
        span = (start, end)
    if span is not None:
        total += span[1] - span[0]
    return total


def throughput_of(
    samples: Iterable[Sequence[Any]], *, last_at: float | None = None
) -> Throughput | None:
    """``sum(pages)`` over the wall seconds of the volumes given.

    Each sample is ``(pages, seconds)`` or ``(pages, seconds, end)``. Volumes
    with an ``end`` are placed on the clock as ``[end - seconds, end]`` and
    count the time their windows cover together (see the module docstring);
    one without is counted for its own seconds, as though it overlapped
    nothing. Samples that cannot be a volume (no pages, no time) are
    skipped; None when nothing is left.
    """
    pages = loose = 0.0
    windows: list[tuple[float, float]] = []
    volumes = 0
    for sample in samples:
        p, s = _number(sample[0]), _number(sample[1])
        if p is None or s is None or p <= 0 or s <= 0:
            continue
        end = _number(sample[2]) if len(sample) > 2 else None
        pages += p
        volumes += 1
        if end is None:
            loose += s
        else:
            windows.append((end - s, end))
    seconds = loose + covered_seconds(windows)
    if volumes == 0 or seconds <= 0:
        return None
    return Throughput(pages, seconds, volumes, last_at)


def records_throughput(
    records: Iterable[Any], *, limit: int = RECENT_VOLUMES
) -> Throughput | None:
    """The throughput of stored congestion records that carry a volume's own
    ``volume_pages`` / ``volume_seconds`` (the pipeline's item counters are
    not pages, so a record without the pair is skipped), newest ``limit``,
    each placed on the clock by its ``at`` -- stamped when the volume was
    done."""
    usable = [
        run
        for run in records
        if isinstance(run, Mapping)
        and _number(run.get("volume_pages"))
        and _number(run.get("volume_seconds"))
    ][-limit:]
    ats = [_number(run.get("at")) for run in usable]
    last = max((at for at in ats if at is not None), default=None)
    return throughput_of(
        (
            (run.get("volume_pages"), run.get("volume_seconds"), run.get("at"))
            for run in usable
        ),
        last_at=last,
    )


def profile_throughput(runs: Mapping[str, Any] | None) -> Throughput | None:
    """A processor profile's ``runs`` entry as a throughput.

    Its ``recent`` volumes when it keeps them, each placed on the clock by
    its ``at`` (stamped when the volume was done); else the congestion
    records' own volume pairs (a profile written before ``recent`` existed);
    else the cumulative ``pages`` / ``seconds`` over every volume it ever
    finished, which have no clock to place them on and are summed. Every one
    of them is pages over seconds read -- none is a fitted rate.
    """
    if not isinstance(runs, Mapping):
        return None
    last_at = _number(runs.get("last_at"))
    recent = runs.get("recent")
    if isinstance(recent, list):
        rows = [r for r in recent if isinstance(r, Mapping)][-RECENT_VOLUMES:]
        ats = [_number(r.get("at")) for r in rows]
        found = throughput_of(
            ((r.get("pages"), r.get("seconds"), r.get("at")) for r in rows),
            last_at=max((a for a in ats if a is not None), default=last_at),
        )
        if found is not None:
            return found
    found = records_throughput(runs.get("congestion") or [])
    if found is not None:
        return Throughput(
            found.pages, found.seconds, found.volumes, last_at or found.last_at
        )
    pages, seconds = _number(runs.get("pages")), _number(runs.get("seconds"))
    volumes = _number(runs.get("volumes"))
    if pages and seconds and pages > 0 and seconds > 0:
        return Throughput(pages, seconds, int(volumes or 1), last_at)
    return None
