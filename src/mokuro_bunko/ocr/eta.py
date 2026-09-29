"""How fast a generation reads pages, and when each queued volume will be done.

Two things live here, and nothing else does:

* :class:`RateModel` -- one pages-per-second number per generation ROW, taken
  from the best evidence this machine has, newest first;
* :func:`plan_queue` -- a lane simulation that turns that rate, the queue in
  run order and the sessions already open into a finishing time for every
  item, plus one for the whole queue.

Both are pure arithmetic over a few hundred items, so the status endpoint
recomputes them on every poll rather than caching a prediction that would go
stale the moment a page lands.

## The clock (ADDENDUM 9)

A rate is ALWAYS pages over the time between page EMISSIONS. Nothing that
happens before the first page of a volume comes out -- interpreter start,
imports, model load, detector spawn, pipeline fill -- may enter a rate, and
nothing may divide by an elapsed time that contains it. That was the old
``elapsed / done`` bug: on a fast engine the model load was most of the
elapsed time, so the first ETA of every volume was several times too long and
only crept towards the truth as the load was amortised away.

Startup is not ignored, it is reported SEPARATELY, and charged exactly where
it is paid: once per session (Addendum 2), which in the queue simulation
means once per lane that switches to a row it is not already serving.

## Where a rate comes from, in order

a. ``session`` -- volumes this process has already finished on that row,
   as an EWMA of pages/second with ``alpha = 0.5`` (the newest volume is half
   the answer). One completed volume is enough to use it: it is this machine,
   this engine, this session, this library.
b. ``history`` -- the congestion file's recent runs of that row, from earlier
   sessions on the same machine. Pooled (total pages over total seconds), so a
   long volume counts for more than a short one.
c. ``bench`` -- the row's saved benchmark ``best.pages_per_second``, measured
   on this machine's own pages under Addendum 9's emission clock.
d. ``volume`` -- the volume IN FLIGHT, once at least
   :data:`MIN_INFLIGHT_PAGES` of its pages have landed. Never a source on its
   own while one of (a)-(c) exists: it is BLENDED into whichever of them is
   active, with a weight that grows as the volume proves itself, so a volume
   that is slower than the row's history corrects its own ETA instead of
   waiting for the row's average to catch up.

The blend is done in SECONDS PER PAGE rather than in pages per second, because
that is the quantity an ETA multiplies by, and because the harmonic direction
errs towards the slower of the two -- the honest way round for a number a
person is waiting on.
"""

from __future__ import annotations

import json
import math
import statistics
import threading
import time
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.throughput import RECENT_VOLUMES, Throughput, throughput_of

# The weight the newest completed volume carries in the session EWMA. Half:
# a machine whose load changes (a benchmark, a big upload, another job) is
# believed within two volumes, and one freak volume still cannot own the
# number.
SESSION_ALPHA = 0.5

# Pages a volume must have emitted before its OWN rate is worth anything. The
# first few pages out of a pipeline come out at the fill rate, not the
# steady-state rate -- the same reason the benchmark discards its fill window.
MIN_INFLIGHT_PAGES = 4

# What the prior (the session/history/bench rate) is worth, in pages of
# in-flight evidence. At this many observed pages the volume's own rate and
# the prior weigh the same; at four pages the volume is worth a fifth.
BLEND_PRIOR_PAGES = 16.0

# The same idea one level up: how many pages this SESSION must have finished
# before its own EWMA outweighs the machine's older evidence (the congestion
# history, or the saved benchmark). Roughly a third of a manga volume, so one
# real volume settles it and a handful of four-page ones do not.
SESSION_PRIOR_PAGES = 64.0

# What a session start costs when nothing has ever measured one. Marked rough
# wherever it is shown, because it is a guess and the UI must not print a
# guess as a measurement.
DEFAULT_STARTUP_SECONDS = 20.0

# How long a file-backed source (congestion history, saved benchmarks) is
# reused before being read again. The status page polls every few seconds and
# these files change once a volume.
CACHE_TTL_SECONDS = 5.0

# The most a volume's fixed cost may be believed to be. A pipeline that takes
# longer than a minute to produce a page after it has been handed a volume is
# not paying fill, it is broken, and an intercept that large would swamp every
# short volume in the queue.
MAX_LATENCY_SECONDS = 60.0

# How many recent (pages, seconds) pairs the latency fit keeps per row. Beyond
# eight the EWMA weight is under half a percent and the pair cannot move the
# answer.
LATENCY_SAMPLES = 8

CONGESTION_FILE = ".ocr-congestion.json"
BENCH_FILE = ".ocr-bench.json"

SOURCE_SESSION = "session"
# The only volume this session has finished is the one it OPENED with, whose
# seconds carry the pipeline fill that `startup` already charges separately.
# Still better than no measurement of this machine at all, and named apart so
# that nothing downstream reads it as a steady-state rate.
SOURCE_SESSION_OPENING = "session (opening volume)"
SOURCE_HISTORY = "history"
SOURCE_BENCH = "bench"
SOURCE_VOLUME = "volume"
# Appended to a source whose rate is the SLOPE of a fitted cost line rather
# than a pooled rate, so the API and the page can tell the two apart: one is
# the marginal cost of a page beside a fixed cost, the other is an average
# with that fixed cost already baked into it.
FIT_SUFFIX = " (fit)"

# Startup sources, in the same priority order.
STARTUP_SESSION = "session"
STARTUP_BENCH = "bench"
STARTUP_DEFAULT = "default"


def iso_utc(epoch: float) -> str:
    """An epoch as the ISO-8601 Z string the API sends.

    UTC, always: the server never formats a local time. Which zone a person
    reads this in is the browser's business and only the browser knows it.
    """
    return (
        datetime.fromtimestamp(epoch, tz=UTC).isoformat(timespec="seconds").replace("+00:00", "Z")
    )


def emission_rate(pages_done: int, seconds_since_first: float) -> float | None:
    """Pages per second between the FIRST and the latest emission, or None.

    ``(M - 1) / (t_last - t_first)`` -- Addendum 9's rate, which is the only
    formula in this file that turns a count and a duration into a speed. The
    minus one is not a rounding nicety: ``M`` emissions span ``M - 1``
    intervals, and dividing by the span of a window that begins AT the first
    page is how a one-page window reports an infinite rate.
    """
    if pages_done < 2 or seconds_since_first <= 0:
        return None
    rate = (pages_done - 1) / seconds_since_first
    return rate if rate > 0 else None


@dataclass(frozen=True)
class VolumeCostFit:
    """What a volume costs: a fixed part, and a part per page.

    BOTH halves come from the same least-squares line, and they have to. A
    POOLED rate -- total pages over total seconds -- already has every
    volume's fixed cost amortised across its pages, so adding an intercept on
    top of one charges the pipeline fill TWICE.

    Measured on the served road, from four real runs: the fit said
    ``1.33 s + 0.0657 s/page`` and reproduced three volumes as 1.59 / 2.12 /
    2.90 s against actuals of 1.26 / 2.28 / 2.78. The same intercept over the
    same runs' pooled 7.17 pages/s gave 1.89 / 3.00 / 4.67 -- up to 1.7x
    over, all of it fill counted a second time.

    So where a fit exists it supplies the RATE as well as the latency, and
    the pooled emission-window rate is what stands in when it does not.
    """

    latency_seconds: float
    seconds_per_page: float

    @property
    def pages_per_second(self) -> float:
        return 1.0 / self.seconds_per_page


def fit_volume_cost(
    samples: Sequence[tuple[float, float]], *, alpha: float = SESSION_ALPHA
) -> VolumeCostFit | None:
    """Least squares over (pages, seconds): ``seconds = latency + pages * b``.

    A line through the pairs a row has really produced, newest weighted
    heaviest by the same decay the EWMA uses. The slope is the MARGINAL cost
    of a page -- what one more page adds to a volume already being read --
    and the intercept is what the volume costs before any of them: the
    pipeline filling and draining around it.

    Measured on a real library: a served mokuro row produced (4, 2.0 s),
    (12, 3.0 s), (24, 4.0 s). A pure page rate is 1.7-2.7 s out on the short
    ones; ``1.6 + pages / 10`` fits all three inside 0.2 s.

    None when the pairs cannot separate an intercept from a slope -- fewer
    than two distinct page counts, or a line that says pages cost nothing or
    cost negative time. The caller then falls back to a pooled rate and
    charges no latency, which is exactly how this behaved before the term
    existed.
    """
    pairs = [
        (float(pages), float(seconds))
        for pages, seconds in samples
        if pages > 0 and seconds > 0
    ]
    if len({pages for pages, _ in pairs}) < 2:
        return None
    last = len(pairs) - 1
    total = sum_x = sum_y = sum_xx = sum_xy = 0.0
    for index, (x, y) in enumerate(pairs):
        weight = (1.0 - alpha) ** (last - index)
        total += weight
        sum_x += weight * x
        sum_y += weight * y
        sum_xx += weight * x * x
        sum_xy += weight * x * y
    denominator = total * sum_xx - sum_x * sum_x
    if denominator <= 0:
        return None
    slope = (total * sum_xy - sum_x * sum_y) / denominator
    if slope <= 0:
        # The line says a longer volume is no slower, or faster. That is
        # noise, not a cost model, and an intercept read off it would be the
        # whole of a volume's time.
        return None
    intercept = (sum_y - slope * sum_x) / total
    # The clamp guards the intercept only; the slope is left as fitted,
    # because a clamp fires on data the line was never going to describe and
    # refitting around it would dress that up as a measurement.
    return VolumeCostFit(min(MAX_LATENCY_SECONDS, max(0.0, intercept)), slope)


def fit_latency(
    samples: Sequence[tuple[float, float]], *, alpha: float = SESSION_ALPHA
) -> float | None:
    """Just the fixed part of :func:`fit_volume_cost`, or None."""
    fit = fit_volume_cost(samples, alpha=alpha)
    return None if fit is None else fit.latency_seconds


@dataclass(frozen=True)
class RateEstimate:
    """One row's speed, its fixed per-volume cost, and how well both are known.

    A volume costs ``latency_seconds + pages / pages_per_second``. The second
    term is the emission-window rate (ADDENDUM 9's clock); the first is what
    the pipeline costs to fill and drain around the volume, which does not
    scale with its length and which a pure page rate therefore gets wrong on
    every short volume.
    """

    pages_per_second: float
    source: str
    volumes_observed: int = 0
    latency_seconds: float = 0.0

    def seconds_for(self, pages: float) -> float:
        """How long ``pages`` take to read. No fixed cost: see `volume_seconds`."""
        return float(pages) / self.pages_per_second

    def volume_seconds(self, pages: float) -> float:
        """A whole volume of ``pages``, fill and drain included."""
        return self.latency_seconds + self.seconds_for(pages)

    def as_dict(self) -> dict[str, Any]:
        return {
            "pages_per_second": round(self.pages_per_second, 4),
            "latency_seconds": round(self.latency_seconds, 2),
            "source": self.source,
            "volumes_observed": self.volumes_observed,
        }


@dataclass(frozen=True)
class StartupEstimate:
    """What opening a session for one row costs, and where that came from."""

    seconds: float
    source: str
    rough: bool = False

    def as_dict(self) -> dict[str, Any]:
        return {
            "seconds": round(self.seconds, 2),
            "source": self.source,
            "rough": self.rough,
        }


DEFAULT_STARTUP = StartupEstimate(DEFAULT_STARTUP_SECONDS, STARTUP_DEFAULT, rough=True)


@dataclass
class _SessionRate:
    """The EWMA of one row's completed volumes this process, and its weight.

    ``pages`` is the evidence behind the number, and it matters as much as
    the number does. Measured on a real library: two four- and twelve-page
    volumes at the head of a session are latency, not throughput -- the
    pipeline is still filling and each costs about as long as the next -- and
    an EWMA that replaced a thousand pages of history with them predicted the
    third volume five times too slow. A volume is a volume to the EWMA; a
    page is a page to the weight.
    """

    # The EWMA over STEADY-STATE volumes only -- the ones read through a
    # pipeline that was already full. A session's opening volume is not one:
    # its seconds contain the fill, which `startup` charges once per session,
    # so folding it in here makes every later volume look slower than it is.
    pages_per_second: float = 0.0
    steady: int = 0
    # Every completed volume, opening ones included. They really did read
    # those pages, so they count towards how far this session's evidence
    # outweighs the machine's older evidence, and towards what is reported.
    volumes: int = 0
    pages: float = 0.0
    # The opening volumes' own rate, kept apart and used only while the
    # session has finished nothing else: a fill-inflated measurement of THIS
    # machine still beats a benchmark taken on another day.
    opening_rate: float = 0.0
    # The raw pairs behind the number, newest last: an EWMA of rates cannot
    # tell a fixed per-volume cost from a slow page, and separating the two
    # needs the lengths as well as the times.
    samples: list[tuple[float, float]] = field(default_factory=list)
    # Wall clock of the newest volume folded in: how recent this evidence is,
    # for the queue page's speed readout ("processed anything recently").
    last_at: float = 0.0
    # Every completed volume's (pages, seconds), opening ones included, newest
    # last: what the machine really DELIVERED, for every speed a person is
    # shown (`ocr.throughput`). Never predicted with -- that is the fit's job.
    recent: list[tuple[float, float]] = field(default_factory=list)

    def add(self, pages: float, seconds: float, alpha: float, *, steady: bool = True) -> None:
        rate = pages / seconds
        self.last_at = time.time()
        self.volumes += 1
        self.pages += pages
        self.recent.append((pages, seconds))
        del self.recent[:-RECENT_VOLUMES]
        if not steady:
            self.opening_rate = (
                rate
                if self.opening_rate <= 0
                else alpha * rate + (1.0 - alpha) * self.opening_rate
            )
            return
        if self.steady == 0:
            self.pages_per_second = rate
        else:
            self.pages_per_second = alpha * rate + (1.0 - alpha) * self.pages_per_second
        self.steady += 1
        self.samples.append((pages, seconds))
        del self.samples[:-LATENCY_SAMPLES]


class RateModel:
    """Pages per second and session startup, per generation id.

    Thread-safe and cheap to ask: the two file-backed sources are re-read at
    most once every :data:`CACHE_TTL_SECONDS`, and the in-process sources are
    dictionaries. Every recording method is safe to call from a worker
    thread while a request thread reads.

    ``storage_path`` is the bunko storage root -- where ``.ocr-congestion.json``
    and ``.ocr-bench.json`` live. None (or a path with neither file) simply
    leaves those two sources empty, which is what a fresh server has.
    """

    def __init__(
        self,
        storage_path: Path | None = None,
        *,
        cache_ttl: float = CACHE_TTL_SECONDS,
        alpha: float = SESSION_ALPHA,
    ) -> None:
        self.storage_path = Path(storage_path) if storage_path is not None else None
        self._cache_ttl = float(cache_ttl)
        self._alpha = float(alpha)
        self._lock = threading.Lock()
        self._session: dict[str, _SessionRate] = {}
        self._startup: dict[str, float] = {}
        self._history: tuple[float, dict[str, Any]] | None = None
        self._bench: tuple[float, dict[str, Any]] | None = None

    # --- recording ------------------------------------------------------

    def record_volume(
        self,
        generation_id: str,
        pages: Any,
        seconds: Any,
        *,
        first_of_session: bool = False,
    ) -> None:
        """Fold one finished volume into this row's session rate.

        Both numbers come off a ``volume_done`` event, so both are parsed
        defensively and a pair that cannot make a rate is simply not one: a
        readout must never be why a finished volume raises.

        ``first_of_session`` marks the volume a session OPENED with. Its
        seconds contain the pipeline filling behind it, and that cost is
        already charged once per session as `startup`, so it is not a
        steady-state sample: it is kept out of the rate's EWMA and out of the
        latency fit alike. Measured: fitting from the first two volumes of a
        session over-predicted the third by 3.2x, the mirror of the bug that
        made the latency term necessary, and leaving the opening volume in
        the pooled rate left the same volume 2.1x over.

        Its PAGES still count: it really did read them, so it moves
        ``volumes_observed`` and the weight that decides how far this
        session's evidence outranks the machine's older evidence. And while
        it is the only volume the session has finished, its rate is still
        used -- a fill-inflated measurement of this machine beats a
        benchmark from another day -- under a source that says so.
        """
        page_count = _as_float(pages)
        elapsed = _as_float(seconds)
        if page_count is None or elapsed is None or page_count <= 0 or elapsed <= 0:
            return
        with self._lock:
            entry = self._session.get(generation_id)
            if entry is None:
                entry = _SessionRate()
                self._session[generation_id] = entry
            entry.add(page_count, elapsed, self._alpha, steady=not first_of_session)

    def record_startup(self, generation_id: str, seconds: Any) -> None:
        """Note what this row's last session cost to open (a ``ready`` event)."""
        value = _as_float(seconds)
        if value is None or value < 0:
            return
        with self._lock:
            self._startup[generation_id] = value

    def forget(self, generation_id: str) -> None:
        """Drop a row's in-process measurements (it was removed or re-specced).

        On every machine: a processor's evidence for the row is filed under
        ``<id>@<name>`` and describes the same recipe, so it goes too.
        """
        prefix = f"{generation_id}@"
        with self._lock:
            for store in (self._session, self._startup):
                for key in [k for k in store if k == generation_id or k.startswith(prefix)]:
                    del store[key]

    # --- asking ---------------------------------------------------------

    def machines_with_evidence(self, generation_id: str, *, within: float) -> list[str]:
        """The machines that finished a volume of this row in the last ``within`` s.

        ``"local"`` for this server's own evidence (filed under the row's id),
        the processor's name for one filed under ``<id>@<name>``. Newest first.
        """
        cutoff = time.time() - within
        prefix = f"{generation_id}@"
        found: list[tuple[float, str]] = []
        with self._lock:
            for key, entry in self._session.items():
                if entry.volumes <= 0 or entry.last_at < cutoff:
                    continue
                if key == generation_id:
                    found.append((entry.last_at, "local"))
                elif key.startswith(prefix):
                    found.append((entry.last_at, key[len(prefix):]))
        return [name for _at, name in sorted(found, reverse=True)]

    def throughput(self, key: str) -> Throughput | None:
        """What the machine filed under ``key`` really delivered this process.

        Pages over wall seconds of its recent finished volumes -- never the
        fitted slope `rate_on` prices the queue with (see `ocr.throughput`).
        ``key`` is a `rate_on` machine key: the row's id for this server,
        ``<id>@<name>`` for a processor.
        """
        with self._lock:
            entry = self._session.get(key)
            if entry is None or not entry.recent:
                return None
            samples = list(entry.recent)
            last_at = entry.last_at or None
        return throughput_of(samples, last_at=last_at)

    def rate(
        self,
        generation_id: str,
        *,
        observed_pages: int = 0,
        observed_seconds: float = 0.0,
    ) -> RateEstimate | None:
        """This row's pages per second, or None when nothing has measured it.

        ``observed_pages`` / ``observed_seconds`` describe the volume in
        flight (pages emitted since its first emission, and the seconds since
        that first emission). They are blended in once the volume has proved
        itself over :data:`MIN_INFLIGHT_PAGES`; with no prior at all they
        become the answer, because a measured rate beats no rate.
        """
        return self._with_inflight(
            self._base_rate(generation_id), observed_pages, observed_seconds
        )

    def rate_on(
        self,
        generation_id: str,
        machine_key: str | None,
        *,
        machine_prior: RateEstimate | None = None,
        observed_pages: int = 0,
        observed_seconds: float = 0.0,
    ) -> RateEstimate | None:
        """This row's pages per second on ONE machine, or None.

        ``machine_key`` is the key that machine's evidence is filed under:
        the row's own id (or None) for this server, ``<id>@<name>`` for a
        processor. A processor's numbers never move this server's rate, but
        a queue with a processor's lanes in it still has to price them, so a
        machine is asked, in order:

        1. its OWN evidence -- its sessions, over ``machine_prior`` (its own
           saved benchmark, which only the caller can read);
        2. this server's measurements of the row (session, then history);
        3. any other machine's session evidence for the row;
        4. the row's saved benchmark.

        For THIS server (``machine_key`` None or the row's own id) its own
        chain is `rate`'s -- session, history, the row's saved benchmark --
        with ``machine_prior`` (its auto-benchmark, kept in its profile) as
        the last prior when the row has no saved benchmark.

        Without 2-4 a server that runs no OCR of its own, or a processor
        that has not finished a volume yet, would leave every queued volume
        "not measured yet" when a real measurement of the same recipe exists.
        The volume in flight blends in exactly as it does in `rate`.
        """
        return self._with_inflight(
            self._machine_base(generation_id, machine_key, machine_prior),
            observed_pages,
            observed_seconds,
        )

    def _with_inflight(
        self, base: RateEstimate | None, observed_pages: int, observed_seconds: float
    ) -> RateEstimate | None:
        """``base`` with the volume in flight blended in once it has proved itself."""
        live = emission_rate(observed_pages, observed_seconds)
        if live is None or observed_pages < MIN_INFLIGHT_PAGES:
            return base
        if base is None:
            # The volume in flight says nothing about a fixed per-volume cost
            # -- it is one volume -- so there is none to claim.
            return RateEstimate(live, SOURCE_VOLUME, 0, 0.0)
        weight = observed_pages / (observed_pages + BLEND_PRIOR_PAGES)
        # Blended as SECONDS PER PAGE: that is what the ETA multiplies by, and
        # it leans towards the slower of the two rather than away from it.
        # The latency rides through untouched: the emissions of one volume
        # measure its pages, not what a volume costs before its first page.
        blended_spp = weight * (1.0 / live) + (1.0 - weight) * (1.0 / base.pages_per_second)
        return RateEstimate(
            1.0 / blended_spp,
            f"{base.source}+{SOURCE_VOLUME}",
            base.volumes_observed,
            base.latency_seconds,
        )

    def latency(self, generation_id: str) -> float:
        """This row's fixed per-volume cost in seconds; 0.0 when unfittable.

        Always the latency of the SAME cost model the rate came from -- see
        :class:`VolumeCostFit` for why the two can never be taken from
        different places.
        """
        base = self._base_rate(generation_id)
        return base.latency_seconds if base is not None else 0.0

    def startup(self, generation_id: str) -> StartupEstimate:
        """What opening a session for this row costs. Never None: see the default."""
        with self._lock:
            measured = self._startup.get(generation_id)
        if measured is not None:
            return StartupEstimate(measured, STARTUP_SESSION)
        saved = _as_float((self._bench_row(generation_id) or {}).get("startup_seconds"))
        if saved is not None and saved > 0:
            return StartupEstimate(saved, STARTUP_BENCH)
        return DEFAULT_STARTUP

    def startup_on(
        self,
        generation_id: str,
        machine_key: str | None,
        *,
        machine_prior: float | None = None,
    ) -> StartupEstimate:
        """What opening a session for this row costs on ONE machine.

        That machine's measured startup, else its own benchmark's
        (``machine_prior``), else the row's as :meth:`startup` answers it.
        """
        if machine_key is None or machine_key == generation_id:
            found = self.startup(generation_id)
            if found.source == STARTUP_DEFAULT and machine_prior is not None and machine_prior > 0:
                # This server's own profile benchmark, after the row's saved one.
                return StartupEstimate(float(machine_prior), STARTUP_BENCH)
            return found
        with self._lock:
            measured = self._startup.get(machine_key)
        if measured is not None:
            return StartupEstimate(measured, STARTUP_SESSION)
        if machine_prior is not None and machine_prior > 0:
            return StartupEstimate(float(machine_prior), STARTUP_BENCH)
        return self.startup(generation_id)

    def report(self, generation_id: str) -> dict[str, Any]:
        """``{pages_per_second, latency_seconds, source, volumes_observed}`` + startup."""
        estimate = self.rate(generation_id)
        return {
            "rate": estimate.as_dict() if estimate is not None else None,
            "startup": self.startup(generation_id).as_dict(),
        }

    # --- sources --------------------------------------------------------

    def _base_rate(self, generation_id: str) -> RateEstimate | None:
        """(a) this session, over (b) the history, over (c) the benchmark.

        "Over", not "instead of": the session takes the number as far as its
        PAGES have earned, against whichever of (b)/(c) exists. With no prior
        it is the whole answer from its first volume; with one, a couple of
        short volumes move the number without owning it, and a real volume or
        two does own it. Without that, the head of a session -- where the
        pipeline is still filling and a four-page volume costs nearly what a
        twenty-four-page one does -- replaced a thousand measured pages.
        """
        return self._over_prior(
            generation_id,
            self._history_rate(generation_id) or self._bench_rate(generation_id),
        )

    def _machine_base(
        self,
        generation_id: str,
        machine_key: str | None,
        machine_prior: RateEstimate | None,
    ) -> RateEstimate | None:
        """`rate_on` before the volume in flight: the chain its docstring lists."""
        if machine_key is None or machine_key == generation_id:
            return self._over_prior(
                generation_id,
                self._history_rate(generation_id)
                or self._bench_rate(generation_id)
                or machine_prior,
            )
        own = self._over_prior(machine_key, machine_prior)
        if own is not None:
            return own
        here = self._over_prior(generation_id, self._history_rate(generation_id))
        if here is not None:
            return here
        other = self._other_machines(generation_id, exclude=machine_key)
        if other is not None:
            return other
        return self._bench_rate(generation_id)

    def _other_machines(self, generation_id: str, *, exclude: str) -> RateEstimate | None:
        """The session rate of the machine that has read the most of this row.

        Only ``<id>@<name>`` keys of THIS row: another row's speed says
        nothing about this one, whatever machine measured it.
        """
        prefix = f"{generation_id}@"
        with self._lock:
            candidates = [
                (entry.pages, key)
                for key, entry in self._session.items()
                if key.startswith(prefix) and key != exclude and entry.volumes > 0
            ]
        for _pages, key in sorted(candidates, reverse=True):
            found = self._over_prior(key, None)
            if found is not None:
                return found
        return None

    def _over_prior(self, key: str, prior: RateEstimate | None) -> RateEstimate | None:
        """The session filed under ``key``, over ``prior`` (see `_base_rate`)."""
        with self._lock:
            entry = self._session.get(key)
            session = self._session_rate(entry)
            session_pages = entry.pages if entry is not None else 0.0
        if session is None:
            return prior
        if prior is None:
            return session
        weight = session_pages / (session_pages + SESSION_PRIOR_PAGES)
        # Both halves of the cost model blend, and by the same weight: a
        # source that could not fit a line reports no fixed cost, so its
        # share of the intercept is nothing, which is what it measured.
        blended = weight * (1.0 / session.pages_per_second) + (1.0 - weight) * (
            1.0 / prior.pages_per_second
        )
        latency = (
            weight * session.latency_seconds + (1.0 - weight) * prior.latency_seconds
        )
        return RateEstimate(
            1.0 / blended,
            session.source if weight >= 0.5 else f"{session.source}+{prior.source}",
            session.volumes_observed,
            latency,
        )

    def _session_rate(self, entry: _SessionRate | None) -> RateEstimate | None:
        """This session's own cost model: the fit, else the EWMA, else the opener.

        Called under the lock. Where a line can be fitted it supplies the rate
        AND the latency together (:class:`VolumeCostFit`); the pooled EWMA is
        the fallback, and carries no latency because nothing separated one.
        The opening volume is the last resort and says so in its source: a
        number taken while the pipeline was still filling must not be read as
        the speed of one that is full.
        """
        if entry is None or entry.volumes == 0:
            return None
        fit = fit_volume_cost(entry.samples, alpha=self._alpha)
        if fit is not None:
            return RateEstimate(
                fit.pages_per_second,
                SOURCE_SESSION + FIT_SUFFIX,
                entry.volumes,
                fit.latency_seconds,
            )
        if entry.steady > 0 and entry.pages_per_second > 0:
            return RateEstimate(entry.pages_per_second, SOURCE_SESSION, entry.volumes, 0.0)
        if entry.opening_rate > 0:
            return RateEstimate(
                entry.opening_rate, SOURCE_SESSION_OPENING, entry.volumes, 0.0
            )
        return None

    def _history_pairs(
        self, generation_id: str, *, steady_only: bool = False
    ) -> list[tuple[float, float]]:
        """This row's recorded runs as (pages, seconds), oldest first.

        ``volume_pages``/``volume_seconds`` when the record has them -- the
        runner's own count and timing of the volume -- and the pipeline's
        ``pages``/``elapsed`` otherwise, which is all a record written before
        that carries. The distinction is not pedantic: an ``item`` is a unit
        of stage work, so on the served road a four-page volume recorded
        sixteen of them, and a rate fitted from items is four times a lie.

        ``steady_only`` drops the runs flagged ``volume_first`` -- the volume
        each session opened with, whose time includes the pipeline filling
        behind it. Both the rate and the latency fit want the steady-state
        runs; the caller decides what to do when there are none.
        """
        runs = self._congestion().get(generation_id)
        if not isinstance(runs, list):
            return []
        pairs: list[tuple[float, float]] = []
        for run in runs:
            if not isinstance(run, Mapping):
                continue
            if steady_only and run.get("volume_first"):
                # The volume a session opened with: its time includes the
                # pipeline filling behind it, which `startup` already charges
                # once per session. An intercept fitted through it bills every
                # later volume for that fill a second time, and a rate pooled
                # with it makes every later volume look slower than it is.
                continue
            pages = _as_float(run.get("volume_pages")) or _as_float(run.get("pages"))
            seconds = _as_float(run.get("volume_seconds")) or _as_float(run.get("elapsed"))
            if pages and seconds and pages > 0 and seconds > 0:
                pairs.append((pages, seconds))
        return pairs

    def _history_rate(self, generation_id: str) -> RateEstimate | None:
        """This row's recorded runs as a cost model: the fit, else pooled.

        Steady-state runs by preference, and the opening ones only when there
        is nothing else -- the same rule the session follows, for the same
        reason, applied to what was persisted. Where a line fits, its slope is
        the rate and its intercept the latency; where it does not, the pooled
        rate stands in and there is no latency to claim.
        """
        pairs = self._history_pairs(generation_id, steady_only=True)
        if not pairs:
            pairs = self._history_pairs(generation_id)
        if not pairs:
            return None
        fit = fit_volume_cost(pairs, alpha=self._alpha)
        if fit is not None:
            return RateEstimate(
                fit.pages_per_second,
                SOURCE_HISTORY + FIT_SUFFIX,
                len(pairs),
                fit.latency_seconds,
            )
        pages = sum(pair[0] for pair in pairs)
        seconds = sum(pair[1] for pair in pairs)
        if seconds <= 0:
            return None
        return RateEstimate(pages / seconds, SOURCE_HISTORY, len(pairs), 0.0)

    def _bench_rate(self, generation_id: str) -> RateEstimate | None:
        row = self._bench_row(generation_id)
        if row is None:
            return None
        for block in (row.get("best"), row.get("baseline")):
            if isinstance(block, Mapping):
                rate = _as_float(block.get("pages_per_second"))
                if rate is not None and rate > 0:
                    # A benchmark reports a page rate and nothing about what a
                    # volume costs before its first page, so it claims none.
                    return RateEstimate(rate, SOURCE_BENCH, 0, 0.0)
        return None

    def _bench_row(self, generation_id: str) -> Mapping[str, Any] | None:
        row = self._bench_file().get(generation_id)
        return row if isinstance(row, Mapping) else None

    def _congestion(self) -> dict[str, Any]:
        return self._cached("_history", CONGESTION_FILE)

    def _bench_file(self) -> dict[str, Any]:
        return self._cached("_bench", BENCH_FILE)

    def _cached(self, slot: str, filename: str) -> dict[str, Any]:
        """One of the two JSON files, re-read at most once per TTL.

        Never raises: a missing, half-written or corrupt diagnostic file means
        "this source has nothing to say", not an error on a status poll.
        """
        now = time.monotonic()
        with self._lock:
            cached: tuple[float, dict[str, Any]] | None = getattr(self, slot)
            if cached is not None and now - cached[0] <= self._cache_ttl:
                return cached[1]
        data: dict[str, Any] = {}
        if self.storage_path is not None:
            try:
                raw = json.loads((self.storage_path / filename).read_text(encoding="utf-8"))
                if isinstance(raw, dict):
                    data = raw
            except (OSError, json.JSONDecodeError, UnicodeDecodeError, ValueError):
                data = {}
        with self._lock:
            setattr(self, slot, (now, data))
        return data


# --- the lane simulation --------------------------------------------------


@dataclass
class Lane:
    """One session slot: what it is serving, and when it frees.

    ``generation_id`` is the row the lane is on RIGHT NOW. A lane moving to a
    different row closes its session and opens another, which is the one place
    a startup is charged; a lane staying on its row pays nothing, which is what
    makes a warm session visible in the numbers.
    """

    generation_id: str | None = None
    free_in: float = 0.0
    used: bool = False
    # Whose hardware this lane is (``lane_machines``): the rates and the
    # startup it is priced with are THAT machine's. None when the caller
    # names no machines, which prices every lane alike.
    machine: str | None = None


def _ask(fn: Callable[..., Any], generation_id: str, machine: str | None, **kwargs: Any) -> Any:
    """Call a rate/startup callback, naming the machine only when there is one.

    A caller that names no lanes' machines passes callbacks that take the
    row alone, and they are called exactly as they always were.
    """
    if machine is None:
        return fn(generation_id, **kwargs)
    return fn(generation_id, machine=machine, **kwargs)


@dataclass
class QueuePlan:
    """Everything one status poll needs to say when things will be done."""

    running: list[dict[str, Any]] = field(default_factory=list)
    pending: list[dict[str, Any]] = field(default_factory=list)
    done_in: float | None = None
    done_at: str | None = None


def lanes_from_running(
    running: Sequence[Mapping[str, Any]],
    *,
    lane_count: int,
    rate_for: Callable[..., RateEstimate | None],
    startup_for: Callable[..., StartupEstimate],
    now: float,
    lane_machines: Sequence[str] | None = None,
) -> tuple[list[Lane], list[dict[str, Any]]]:
    """Turn the running jobs into occupied lanes, and price each of them.

    A SESSION holds a lookahead of two volumes, so one slot can report two
    running jobs -- they share one pipeline and one stream of pages, so the
    lane's remaining time is the SUM of their remaining pages over the row's
    rate, not the larger of two independent jobs. Jobs are grouped by the
    ``slot`` the worker stamps on them; a job with no slot (an older worker)
    gets a lane of its own, which is exactly what one slot per job meant.

    With ``lane_machines`` every lane belongs to a machine, and a slot's
    jobs take a lane of the ``machine`` their card names and are priced
    with that machine's rate: a processor's volume occupies a processor's
    lane, never this server's.
    """
    if lane_machines:
        lanes = [Lane(machine=str(machine)) for machine in lane_machines]
    else:
        lanes = [Lane() for _ in range(max(1, int(lane_count)))]
    priced: list[dict[str, Any]] = []
    by_slot: dict[Any, list[dict[str, Any]]] = {}
    for index, job in enumerate(running):
        entry = dict(job)
        slot = entry.get("slot")
        key = slot if isinstance(slot, int) else f"job-{index}"
        by_slot.setdefault(key, []).append(entry)
        priced.append(entry)

    taken: set[int] = set()
    for _key, jobs in sorted(by_slot.items(), key=_slot_sort_key):
        wanted = next(
            (job["machine"] for job in jobs if isinstance(job.get("machine"), str)), None
        )
        position = _free_lane(lanes, taken, wanted)
        if position is None:
            # More slots reporting than there are lanes (a setting lowered
            # while jobs ran, a processor that left mid-volume). Give them
            # lanes rather than silently folding two sessions into one.
            lanes.append(Lane(machine=wanted if lane_machines else None))
            position = len(lanes) - 1
        taken.add(position)
        lane = lanes[position]
        # Where the volume really runs, when its card says: a card naming a
        # machine that has no lane of its own left is still that machine's.
        machine = (wanted or lane.machine) if lane_machines else None
        # In order: the volumes of one session are read one after another
        # through the same stream of pages, so the second one finishes after
        # the first, not beside it. Walking them in order is what turns a
        # lookahead volume from "starting up" for two minutes into a
        # finishing time it can be held to.
        cursor = 0.0
        known = True
        generation_id: str | None = None
        for index, entry in enumerate(jobs):
            generation_id = entry.get("generation_id") or generation_id
            if not known:
                # The volume ahead of this one on the lane could not be
                # priced, so neither can this one: it starts when that one
                # ends, and that is what is unknown. Carrying on with the
                # old cursor would have quietly predicted it as if the
                # volume ahead of it took no time at all.
                _no_prediction(entry, "waiting behind a volume that cannot be timed")
                continue
            finish = _price_running(
                entry,
                rate_for=rate_for,
                startup_for=startup_for,
                now=now,
                machine=machine,
                offset=cursor,
                # Only the volume at the head of the lane is waiting on a
                # model load; by the time the one behind it starts, the
                # session has been open for a whole volume.
                charge_startup=index == 0,
            )
            if finish is None:
                known = False
                continue
            cursor = finish
        remaining = cursor
        lane.generation_id = generation_id
        lane.used = True
        lane.free_in = remaining if known else float("inf")
    return lanes, priced


def _free_lane(lanes: Sequence[Lane], taken: set[int], machine: str | None) -> int | None:
    """The first lane no running slot holds yet, of ``machine`` when it has one."""
    free = [index for index in range(len(lanes)) if index not in taken]
    if machine is not None:
        for index in free:
            if lanes[index].machine == machine:
                return index
    return free[0] if free else None


def _slot_sort_key(item: tuple[Any, list[dict[str, Any]]]) -> tuple[int, str]:
    key = item[0]
    return (0, f"{key:09d}") if isinstance(key, int) else (1, str(key))


def _price_running(
    entry: dict[str, Any],
    *,
    rate_for: Callable[..., RateEstimate | None],
    startup_for: Callable[..., StartupEstimate],
    now: float,
    machine: str | None = None,
    offset: float = 0.0,
    charge_startup: bool = True,
) -> float | None:
    """Fill one running job's ETA fields in place; return when its lane frees.

    ``offset`` is how long the volumes ahead of this one on the same lane
    still need; the answer is measured from ``now``, not from ``offset``, so
    a caller can chain them. None when the job cannot be priced -- no rate
    for its row, or no page count yet -- in which case the fields say so
    rather than guessing.
    """
    generation_id = str(entry.get("generation_id") or "")
    done = _as_int(entry.get("done_pages")) or 0
    total = _as_int(entry.get("total_pages"))
    first_page_at = _as_float(entry.get("first_page_at"))
    observed_seconds = max(0.0, now - first_page_at) if first_page_at else 0.0
    estimate: RateEstimate | None = _ask(
        rate_for, generation_id, machine,
        observed_pages=done, observed_seconds=observed_seconds,
    )
    entry["rate_pages_per_second"] = (
        round(estimate.pages_per_second, 4) if estimate is not None else None
    )
    entry["latency_seconds"] = (
        round(estimate.latency_seconds, 2) if estimate is not None else None
    )
    entry["rate_source"] = estimate.source if estimate is not None else None

    status = entry.get("status")
    if status in ("error", "done"):
        # Finished, well or badly. It holds its lane for no further seconds,
        # which is not the same as being unpredictable: a brief error card
        # must not take the whole queue's finishing time down with it.
        entry["eta_at"] = None
        return offset

    if done <= 0:
        # Nothing has come out of THIS volume yet, so its own emissions say
        # nothing and no elapsed time may be divided into a rate -- that is
        # the bug this module exists to remove. What is left is the prior:
        # the model load still to pay (when this volume is the one waiting on
        # it) plus the whole volume at the row's measured rate.
        startup: StartupEstimate = _ask(startup_for, generation_id, machine)
        started_at = _as_float(entry.get("session_started_at")) or _as_float(
            entry.get("started_at")
        )
        spent = max(0.0, now - started_at) if started_at else 0.0
        # A session that has said it is ready owes no load, whatever the
        # estimate thinks is left of it.
        charge = charge_startup and entry.get("session_ready") is not True
        left = max(0.0, startup.seconds - spent) if charge else 0.0
        entry["status"] = "starting"
        entry["startup_seconds"] = int(round(left)) if left >= 1.0 else None
        entry["startup_rough"] = startup.rough
        if estimate is None or total is None:
            entry["eta_seconds"] = None
            entry["eta_at"] = None
            return None
        # No page has landed, so the pipeline has yet to fill around this
        # volume: it pays the fixed cost as well as the pages.
        finish = offset + left + estimate.volume_seconds(total)
        entry["eta_seconds"] = int(round(finish))
        entry["eta_at"] = iso_utc(now + finish)
        return finish

    if total is not None and done >= total:
        entry["status"] = "finalizing"
        entry["percent"] = 100
        entry["eta_seconds"] = 0
        entry["eta_at"] = iso_utc(now + offset)
        return offset

    entry["status"] = "running"
    if total is None or estimate is None:
        entry["eta_seconds"] = None
        entry["eta_at"] = None
        return None
    # A page is out, so the fill has been paid: what is left is pages only.
    finish = offset + estimate.seconds_for(total - done)
    entry["eta_seconds"] = int(round(finish))
    entry["eta_at"] = iso_utc(now + finish)
    return finish


def plan_queue(
    running: Sequence[Mapping[str, Any]],
    pending: Sequence[Mapping[str, Any]],
    *,
    lane_count: int,
    rate_for: Callable[..., RateEstimate | None],
    startup_for: Callable[..., StartupEstimate],
    now: float,
    startup_every_volume: Callable[..., bool] | None = None,
    lane_machines: Sequence[str] | None = None,
    through: int | None = None,
    refusal_for: Callable[[str, str], str | None] | None = None,
    hold_for: Callable[[str], str | None] | None = None,
) -> QueuePlan:
    """Price the running jobs, then walk the queue across the lanes.

    ``through`` stops the walk once the item at that index is priced: the
    walk is sequential, so nothing after an item changes its finishing time,
    and a caller that wants one volume's ETAs (a manifest, an upload) need
    not price the rest of a long queue. The items after it are left out and
    ``done_at`` is None -- the queue's end was not computed.

    The queue is taken in the order it is given -- it is the scheduler's own
    list and a second opinion about the order here is how the page comes to
    disagree with the worker. Each item goes to the lane that frees first;
    a lane moving to a row it is not already serving pays that row's startup.

    ``startup_every_volume`` names the rows that pay it EVERY time: a row that
    still runs one volume per command line (an installed mokuro with no serve
    module) loads its model again for each volume, and charging it once per
    lane would under-count a backlog of small volumes by minutes.

    An item whose row has NO rate stops the prediction: it and everything
    behind it carry ``eta_at: null`` with a ``reason``, and so does
    ``done_at``. A queue with an unknown volume in it has an unknown end, and
    inventing one would be the ETA lying in exactly the way it used to.

    ``lane_machines`` names whose hardware each lane is. Every item is then
    priced with the rate, the startup and the every-volume rule of the
    machine whose lane takes it, the three callbacks being asked with
    ``machine=``. Without it every lane is priced alike, by row alone.

    ``refusal_for(generation_id, machine)`` says why a machine may NOT run a
    row (its precision mode, which it cannot run): such a lane never takes
    that row's items. An item no lane may take is HELD -- ``held: True``, no
    ETA, the reason ``hold_for(generation_id)`` gives -- and holds up nothing
    behind it: it will never run on these machines, so it takes no lane.
    """
    lanes, priced = lanes_from_running(
        running,
        lane_count=lane_count,
        rate_for=rate_for,
        startup_for=startup_for,
        now=now,
        lane_machines=lane_machines,
    )
    median = _median_pages(pending)
    planned: list[dict[str, Any]] = []
    # Set by the first item that could not be priced, to the sentence that
    # says WHY -- not just which row it was. Naming the row and then blaming
    # its speed reads as a lie when what was actually missing was a page
    # count, which is what a run of this against a real library showed.
    blocked_by: str | None = None

    for index, item in enumerate(pending):
        if through is not None and index > through:
            break
        entry = dict(item)
        generation_id = str(entry.get("generation_id") or "")
        name = str(entry.get("generation") or generation_id or "that generation")
        pages = _as_int(entry.get("pages"))
        rough = False
        if pages is None or pages <= 0:
            pages = median
            rough = True
        entry["pages"] = pages
        entry["rough"] = rough

        if blocked_by is not None:
            _no_prediction(entry, f"waiting behind a volume that cannot be timed: {blocked_by}")
            planned.append(entry)
            continue
        # Only the lanes whose machine may run the row at all.
        open_lanes = (
            [
                lane
                for lane in lanes
                if lane.machine is None or refusal_for(generation_id, lane.machine) is None
            ]
            if refusal_for is not None
            else lanes
        )
        if not open_lanes:
            _no_prediction(
                entry,
                (hold_for(generation_id) if hold_for is not None else None)
                or f"no connected machine can run {name}",
            )
            entry["held"] = True
            planned.append(entry)
            continue
        # The lane that FINISHES it first takes it, as the worker's own slots
        # do (`earliest_finish_claim`): its machine's rate is the one this
        # volume will be read at. Only when every lane's machine has a rate
        # for the row -- otherwise the worker falls back to first-come, and
        # so does this: the lane that frees first.
        lane = _earliest_finish_lane(
            open_lanes, generation_id, pages, rate_for, startup_for, startup_every_volume
        ) or min(open_lanes, key=lambda item: item.free_in)
        estimate: RateEstimate | None = _ask(rate_for, generation_id, lane.machine)
        if estimate is None:
            blocked_by = f"nothing has measured how fast {name} reads a page yet"
            _no_prediction(entry, blocked_by)
            planned.append(entry)
            continue
        if pages is None:
            blocked_by = "no volume in the queue has a known page count"
            _no_prediction(entry, blocked_by)
            planned.append(entry)
            continue

        if not math.isfinite(lane.free_in):
            # Every lane is held by a volume that cannot be priced -- one
            # whose page count the runner has not announced yet, say. When
            # the lane frees is unknown, so when this item runs is unknown,
            # and saying so is the whole contract.
            blocked_by = "a volume already running has not said how long it is yet"
            _no_prediction(entry, blocked_by)
            planned.append(entry)
            continue
        start = lane.free_in
        every_time = startup_every_volume is not None and bool(
            _ask(startup_every_volume, generation_id, lane.machine)
        )
        if every_time or lane.generation_id != generation_id:
            start += _ask(startup_for, generation_id, lane.machine).seconds
        # Every queued volume pays the fixed cost once: the pipeline fills and
        # drains around each of them, however short they are.
        seconds = start + estimate.volume_seconds(pages)
        lane.generation_id = generation_id
        lane.free_in = seconds
        lane.used = True
        entry["eta_seconds"] = int(round(seconds))
        entry["eta_at"] = iso_utc(now + seconds)
        entry["rate_source"] = estimate.source
        entry["latency_seconds"] = round(estimate.latency_seconds, 2)
        entry["reason"] = None
        planned.append(entry)

    finished = [lane.free_in for lane in lanes if lane.used]
    truncated = through is not None and through + 1 < len(pending)
    if (
        truncated
        or blocked_by is not None
        or not finished
        or not all(map(math.isfinite, finished))
    ):
        return QueuePlan(running=priced, pending=planned, done_in=None, done_at=None)
    done_in = max(finished)
    return QueuePlan(
        running=priced,
        pending=planned,
        done_in=done_in,
        done_at=iso_utc(now + done_in),
    )


def _job_identity(job: Mapping[str, Any]) -> tuple[Any, Any, Any]:
    """(series, volume, generation id): one job, wherever it is listed."""
    return (job.get("series"), job.get("volume"), job.get("generation_id"))


def _earliest_finish_lane(
    lanes: Sequence[Any],
    generation_id: str,
    pages: int | None,
    rate_for: Callable[..., RateEstimate | None],
    startup_for: Callable[..., StartupEstimate],
    startup_every_volume: Callable[..., bool] | None,
) -> Any:
    """`plan_queue`'s lane for one item: the one that finishes it first.

    None when that cannot be said -- no page count, a lane still held by a
    volume of unknown length, or a lane whose machine has no rate for the
    row -- and the caller takes the lane that frees first, as it always did.
    """
    if pages is None or not lanes:
        return None
    best: Any = None
    best_at = math.inf
    for item in lanes:
        if not math.isfinite(item.free_in):
            return None
        estimate = _ask(rate_for, generation_id, item.machine)
        if estimate is None:
            return None
        start = item.free_in
        every_time = startup_every_volume is not None and bool(
            _ask(startup_every_volume, generation_id, item.machine)
        )
        if every_time or item.generation_id != generation_id:
            start += _ask(startup_for, generation_id, item.machine).seconds
        at = start + estimate.volume_seconds(pages)
        if at < best_at:
            best, best_at = item, at
    return best


# --- which machine takes a volume (earliest finish) ---------------------------

# A machine that is asking keeps a volume unless another would finish it
# clearly sooner: that one has to come and claim it (up to a slot's idle
# wait away), and a hair's advantage is not worth the wait or the churn.
# CAPPED: a margin proportional to the finish time grew with how far ahead
# the queue was booked -- ~70 s at a 700 s horizon -- and let the slowest
# card take a 27-page volume the fastest would have finished first (live,
# live, 01:35:51: tower ~700 s, rig-c ~770 s, rig-c took it).
EFT_MARGIN = 0.10
EFT_MARGIN_CAP_SECONDS = 5.0
EFT_SLACK_SECONDS = 2.0
# How far down the queue one claim walks before giving up on finding a
# volume for the asking machine.
EFT_LOOKAHEAD = 256


@dataclass
class EftLane:
    """One slot that could take a volume, as `earliest_finish_claim` sees it.

    ``free_in`` is when its current work ends (seconds from now; 0 = idle),
    ``warm`` the row its open session serves (no startup to read more of
    it), ``rows`` the rows it may be given at all -- its catalog, its
    benchmark, its backoffs, all decided by the caller.
    """

    key: Any
    machine: str
    free_in: float
    warm: str | None
    rows: frozenset[str]


@dataclass(frozen=True)
class EftLeft:
    """A volume the asking lane leaves to a lane that finishes it sooner."""

    job: Any
    to: Any
    there: float  # seconds from now until it would be done on ``to``
    here: float  # ... and on the asking lane
    starts: float  # seconds from now until ``to`` would start it (startup included)
    # Whether ``to`` has work to finish first -- its own in flight, or what
    # the walk gave it before this volume. A lane that is busy is expected
    # to come for it later; one that is idle now should come at once.
    busy_first: bool = False


@dataclass
class EftDecision:
    """``mine``: the first volume the walk gives the asking lane (None: none
    within the walk). ``left``: the volumes before it, that the asking lane
    could run, given to another lane that finishes them sooner."""

    mine: Any
    left: list[EftLeft] = field(default_factory=list)


def earliest_finish_claim(
    jobs: Sequence[tuple[Any, str, int | None]],
    lanes: Sequence[EftLane],
    asking: Any,
    *,
    rate_for: Callable[[str, str], RateEstimate | None],
    startup_for: Callable[[str, str], float],
    margin: float = EFT_MARGIN,
    margin_cap: float = EFT_MARGIN_CAP_SECONDS,
    slack: float = EFT_SLACK_SECONDS,
    limit: int = EFT_LOOKAHEAD,
) -> EftDecision | None:
    """Walk ``jobs`` in queue order, each to the lane that FINISHES it first.

    ``jobs`` are ``(job, row id, pages)`` in the scheduler's own order;
    ``asking`` is the key of the lane that is claiming. Each volume goes to
    the lane with the earliest finish -- its current work, then the row's
    startup unless the lane is warm on that row, then the volume at that
    machine's rate -- except that the asking lane keeps any volume it would
    finish within ``margin`` (at most ``margin_cap`` seconds) and ``slack``
    of the best other lane. The walk
    stops at the first volume given to the asking lane.

    This is list scheduling, not "is someone else faster": each lane's
    finish includes what the walk already gave it, so a slow machine is
    still fed further down a long queue, while a lone volume -- or the last
    few of a queue -- goes to the machine that finishes it first.

    None when the walk cannot be priced -- a lane that may take a row with
    no rate for it, or no volume with a known page count to price the
    unknown ones by -- so the caller keeps the plain first-come rule.
    """
    known = [pages for _, _, pages in jobs if isinstance(pages, int) and pages > 0]
    median = int(round(statistics.median(known))) if known else None
    me = next((item for item in lanes if item.key == asking), None)
    if me is None:
        return None
    # (free in, warm row) per lane, as the walk has booked it so far.
    state: dict[Any, tuple[float, str | None]] = {
        item.key: (float(item.free_in), item.warm) for item in lanes
    }
    decision = EftDecision(mine=None)
    rates: dict[tuple[str, str], RateEstimate | None] = {}

    def finish(item: EftLane, generation_id: str, pages: int) -> tuple[float, float] | None:
        """``(starts, done)`` for this volume on this lane, in seconds from now."""
        key = (generation_id, item.machine)
        if key not in rates:
            rates[key] = rate_for(generation_id, item.machine)
        rate = rates[key]
        if rate is None:
            return None
        free_in, warm = state[item.key]
        start = free_in if warm == generation_id else free_in + startup_for(
            generation_id, item.machine
        )
        return start, start + rate.volume_seconds(pages)

    for job, generation_id, pages in list(jobs)[: max(0, limit)]:
        if not isinstance(pages, int) or pages <= 0:
            if median is None:
                return None
            pages = median
        best: EftLane | None = None
        best_at = math.inf
        best_starts = 0.0
        best_busy = False
        mine_at: float | None = None
        for item in lanes:
            if generation_id not in item.rows:
                continue
            priced = finish(item, generation_id, pages)
            if priced is None:
                return None
            starts, at = priced
            if item.key == asking:
                mine_at = at
            elif at < best_at:
                best, best_at, best_starts = item, at, starts
                best_busy = state[item.key][0] > 0
        if mine_at is not None and (
            best is None or mine_at <= best_at + min(best_at * margin, margin_cap) + slack
        ):
            decision.mine = job
            return decision
        if best is None:
            continue
        if mine_at is not None:
            decision.left.append(
                EftLeft(
                    job=job, to=best.key, there=best_at, here=mine_at, starts=best_starts,
                    busy_first=best_busy,
                )
            )
        state[best.key] = (best_at, generation_id)
    return decision


def _no_prediction(entry: dict[str, Any], reason: str) -> None:
    entry["eta_seconds"] = None
    entry["eta_at"] = None
    entry["rate_source"] = None
    entry.setdefault("latency_seconds", None)
    entry["reason"] = reason


def _median_pages(pending: Sequence[Mapping[str, Any]]) -> int | None:
    """The middle page count of the volumes that HAVE one, else None.

    The median rather than the mean: one 900-page omnibus in a queue of
    200-page volumes must not stretch every unknown volume in the list.
    """
    known = [
        value
        for value in (_as_int(item.get("pages")) for item in pending)
        if value is not None and value > 0
    ]
    if not known:
        return None
    return int(round(statistics.median(known)))


def _as_int(value: Any) -> int | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return int(value)


def _as_float(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)
