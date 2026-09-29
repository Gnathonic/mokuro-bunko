"""Benchmark and tune one generation, on this machine, on these pages.

Every number a person is shown about a row's speed has to have been measured
HERE -- on the operator's own hardware, on pages out of their own library --
rather than inferred from a developer's box. That is what this runs:

1. sample real pages from the library (32 by default, spread across series
   and volumes, extracted to a scratch directory);
2. take the machine being measured, and only that one: the volumes running
   on it are stopped (cancelled without a failure or a backoff: they go back
   in the queue and start over, on another machine meanwhile or on this one
   afterwards) and nothing new starts there until the benchmark is done --
   because numbers taken beside a running OCR job measure a machine nobody
   has, and a benchmark must not wait behind a 200-page volume;
3. for a composed engine, run ``engine_runner.py --bench``: ONE process that
   loads the models once (timed = startup), warms up, and then runs trial
   passes over the same pages at different pool widths, following the
   pipeline's own bottleneck verdict;
4. for a row that runs behind its own command line (a monolithic engine, or
   a served one whose package has no serve module) run it once over the
   sample and time it -- there are no stages to size, so nothing to tune;
5. compose the events into the bench object the admin panel renders, persist
   the last finished one per row, and let the queue go.

This module owns the state machine, the sample, the host description and the
persistence. The SEARCH -- which stage to widen, when a step paid for itself
-- belongs to the runner, which is the only thing that can see the pipeline
while it runs.
"""

from __future__ import annotations

import csv
import itertools
import json
import logging
import os
import platform
import re
import shutil
import subprocess
import tempfile
import threading
import time
import zipfile
from collections.abc import Callable, Iterable, Iterator, Mapping, Sequence
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.ocr.devices import (
    DEVICE_AUTO,
    PROBE_SOURCE,
    DeviceCatalog,
    cached_catalog,
    catalog_from_processor,
    parse_probe,
)
from mokuro_bunko.ocr.engine_runner import DEFAULT_PRECISION_MODE, PRECISIONS
from mokuro_bunko.ocr.generations import GenerationConfigError, GenerationSpec, parse_bench_spec
from mokuro_bunko.ocr.remote.profiles import LOCAL_PROFILE, POOL_AUTO
from mokuro_bunko.ocr.utilization import (
    UtilizationSampler,
    sampler_for,
)
from mokuro_bunko.ocr.utilization import (
    first_gpu_device as utilization_device,
)

if TYPE_CHECKING:
    from mokuro_bunko.ocr.processor import OCRProcessor
    from mokuro_bunko.ocr.watcher import OCRWorker

logger = logging.getLogger(__name__)

BENCH_FILE = ".ocr-bench.json"

# Pages sampled when the request does not say. Enough that a trial is longer
# than the noise between them and short enough that eight trials plus a model
# load fit inside a coffee.
DEFAULT_SAMPLE_PAGES = 32
MIN_SAMPLE_PAGES = 4
MAX_SAMPLE_PAGES = 512

# Passed to the runner, which enforces them; the server only stops waiting.
BENCH_MAX_TRIALS = 8
BENCH_BUDGET_SECONDS = 900.0

# The volume the estimates are quoted for. The admin page says the same
# number in its own copy (`BENCH_VOLUME_PAGES` in admin.js).
BENCH_VOLUME_PAGES = 200

# ADDENDUM 9, the server's copy of the runner's rule: a rate read over a
# window shorter than this is reported, flagged, and never decided on. The
# monolithic path cannot loop a pass without paying the model load again, so
# it is the one that flags most often -- and the served road (ADDENDUM 8),
# not a longer sample, is the fix for that.
BENCH_SHORT_WINDOW_SECONDS = 10.0

# How long the whole run may take beyond the runner's own budget before the
# server decides the runner is not coming back. Generous: the budget bounds
# the TRIALS, and a model load plus a warm-up pass happen before the first
# one is counted.
BENCH_SLACK_SECONDS = 1800.0

# How long the queue is given to go quiet before the benchmark gives up. A
# volume already running is not killed for a benchmark, so this is however
# long one volume can take.
QUEUE_HOLD_TIMEOUT = 3600.0

IMAGE_SUFFIXES = {".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif", ".tif", ".tiff"}

_UNSAFE_NAME = re.compile(r"[^A-Za-z0-9._-]+")

# A client-minted key for a row that has never been saved: the UI mints one
# per unsaved row and keeps it for the row's life on the page. Its result is
# held in memory for the life of THIS PROCESS only -- never written to
# `.ocr-bench.json` -- and is gone after a restart, exactly like the row it
# measured.
# Draft (unsaved-row) results kept in memory; older ones are evicted.
MAX_DRAFT_RESULTS = 32
DRAFT_KEY_RE = re.compile(r"^draft-[a-z0-9-]{1,24}\Z")


def is_draft_key(key: str) -> bool:
    """True when ``key`` is a client-minted draft key, not a saved generation id."""
    return bool(DRAFT_KEY_RE.match(key))


class BenchError(Exception):
    """A benchmark request that cannot be honoured, with its HTTP status.

    ``row``/``field`` are only ever set for a spec validation failure (a spec
    is not part of a list, so ``row`` is always None there); every other
    refusal (an unknown key, a missing environment, an empty library, a 409)
    leaves them None.
    """

    def __init__(
        self,
        status: int,
        message: str,
        *,
        row: int | None = None,
        field: str | None = None,
    ) -> None:
        super().__init__(message)
        self.status = status
        self.message = message
        self.row = row
        self.field = field


def bench_path(storage_path: Path) -> Path:
    return storage_path / BENCH_FILE


def _now_iso() -> str:
    return datetime.now(tz=UTC).isoformat(timespec="seconds").replace("+00:00", "Z")


def _natural_key(name: str) -> tuple[Any, ...]:
    return tuple(int(part) if part.isdigit() else part.lower() for part in re.split(r"(\d+)", name))


# --- the host ----------------------------------------------------------


def _physical_cores() -> int | None:
    """Cores, not threads: what a pool width is really competing for."""
    try:
        text = Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
    except OSError:
        text = ""
    if text:
        pairs: set[tuple[str, str]] = set()
        physical = core = None
        for line in text.splitlines():
            if line.startswith("physical id"):
                physical = line.split(":", 1)[-1].strip()
            elif line.startswith("core id"):
                core = line.split(":", 1)[-1].strip()
                if physical is not None:
                    pairs.add((physical, core))
        if pairs:
            return len(pairs)
    try:
        import os as _os

        count = _os.cpu_count()
    except Exception:  # pragma: no cover - defensive
        count = None
    return count


def _cpu_model() -> str:
    try:
        text = Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
        for line in text.splitlines():
            if line.lower().startswith("model name"):
                return line.split(":", 1)[-1].strip()
    except OSError:
        pass
    return platform.processor() or platform.machine() or "unknown CPU"


def _first_line(output: str) -> str | None:
    lines = output.strip().splitlines()
    return lines[0].strip() if lines and lines[0].strip() else None


def _run_probe(
    command: Sequence[str],
    timeout: float,
    pick: Callable[[str], str | None] = _first_line,
) -> str | None:
    """``pick`` of the command's output (its first line by default), or None."""
    try:
        result = subprocess.run(  # noqa: S603 - fixed argv
            list(command),
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    return pick(result.stdout or "")


_GPU_PROBE = 'import torch;print(torch.cuda.get_device_name(0) if torch.cuda.is_available() else "")'


def cpu_label() -> str:
    """The CPU as a person would name it, with its core count."""
    cores = _physical_cores()
    cpu = _cpu_model()
    if cores:
        cpu = f"{cpu} ({cores} core{'s' if cores != 1 else ''})"
    return cpu


def probe_devices(engines_python: Path | None) -> DeviceCatalog:
    """Every device a model may be placed on, asked ONCE of the engines env.

    The same subprocess shape as the host probe above, and for the same
    reason: the server's own environment has no torch, and the cards that
    matter are the ones the OCR environment can see. Without that environment
    (not installed yet, or the probe failed) the answer is the fallback
    catalog -- ``auto`` and ``cpu``, refusing nothing.
    """
    if engines_python is None:
        return DeviceCatalog(cpu_label=cpu_label())
    payload = _run_probe([str(engines_python), "-c", PROBE_SOURCE], timeout=120.0)
    return parse_probe(payload, cpu_label=cpu_label())


def describe_host(backend: str | None, engines_python: Path | None) -> dict[str, Any]:
    """CPU, GPU and backend, as plainly as this platform will say them.

    The GPU is asked of TORCH IN THE ENGINES ENVIRONMENT first, because that
    is the device the OCR actually runs on -- a host with two cards, or with
    a ``HIP_VISIBLE_DEVICES`` set for the server, would otherwise be told
    about a card its OCR never touches. ``rocm-smi``/``nvidia-smi`` are the
    fallback, and ``null`` is an honest answer.
    """
    cpu = cpu_label()
    gpu: str | None = None
    if engines_python is not None:
        gpu = _run_probe([str(engines_python), "-c", _GPU_PROBE], timeout=120.0)
    if not gpu:
        gpu = _run_probe(
            ["nvidia-smi", "--query-gpu=name", "--format=csv,noheader"], timeout=10.0
        )
    if not gpu:
        gpu = _run_probe(
            ["rocm-smi", "--showproductname", "--csv"], timeout=10.0, pick=_card_series
        )
    return {"cpu": cpu, "gpu": gpu or None, "backend": backend}


def _card_series(output: str) -> str | None:
    """The first card's "Card Series" out of ``rocm-smi --showproductname --csv``.

    That output opens with a header row (``device,Card Series,...``), so its
    first line is never the name.
    """
    rows = csv.DictReader(line for line in output.splitlines() if line.strip())
    for row in rows:
        name = (row.get("Card Series") or "").strip()
        if name:
            return name
    return None


# --- the sample --------------------------------------------------------


class BenchSample:
    """Pages pulled out of the library into a scratch directory.

    Spread across series AND volumes on purpose: a benchmark taken on 32
    consecutive pages of one volume measures that volume's art, not the
    library's, and the pages a manga OCR is slow on are not evenly
    distributed inside a book.
    """

    def __init__(self, directory: Path, pages: int, volumes: int) -> None:
        self.directory = directory
        self.pages = pages
        self.volumes = volumes

    def to_dict(self) -> dict[str, Any]:
        return {"pages": self.pages, "volumes": self.volumes}

    def cleanup(self) -> None:
        shutil.rmtree(self.directory, ignore_errors=True)


def _archives_round_robin(library: Path) -> list[Path]:
    """Every archive, series taking turns, each series in reading order."""
    by_series: dict[str, list[Path]] = {}
    for path in sorted(library.rglob("*.cbz")):
        if not path.is_file():
            continue
        try:
            series = path.parent.relative_to(library).as_posix()
        except ValueError:
            series = path.parent.name
        by_series.setdefault(series, []).append(path)
    for volumes in by_series.values():
        volumes.sort(key=lambda p: _natural_key(p.name))
    ordered: list[Path] = []
    index = 0
    names = sorted(by_series)
    while True:
        added = False
        for series in names:
            volumes = by_series[series]
            if index < len(volumes):
                ordered.append(volumes[index])
                added = True
        if not added:
            break
        index += 1
    return ordered


def _archive_pages(archive: Path) -> list[str]:
    """Image members of an archive, in the order the runner would read them."""
    try:
        with zipfile.ZipFile(archive, "r") as zf:
            names = [
                name
                for name in zf.namelist()
                if not name.endswith("/") and Path(name).suffix.lower() in IMAGE_SUFFIXES
            ]
    except (zipfile.BadZipFile, OSError):
        return []
    # The embedded thumbnail an uploader names after the archive is not a
    # page, exactly as `_extract_and_clean` treats it.
    thumb = f"{archive.stem}.webp"
    names = [name for name in names if name != thumb]
    names.sort(key=_natural_key)
    return names


# Pages skipped at each end of a volume that has enough of them: the cover,
# the colour insert and the title page at the front, the afterword and ads at
# the back. They carry a fraction of a story page's text, and a sample of them
# measured the engines 2.6-3.4x faster than the same rows ran live.
SAMPLE_EDGE_PAGES = 2
# Pages read from each volume the sample visits. Several interior pages of a
# few volumes, not one page each of many: with one page per volume (32+
# volumes) every page was a cover.
SAMPLE_PAGES_PER_VOLUME = 4


def _spread(count: int, total: int) -> list[int]:
    """``count`` indices spread evenly through the INTERIOR of ``total`` items.

    Each is the midpoint of its share (``(i + 0.5) * step``), never the start
    of it, and up to `SAMPLE_EDGE_PAGES` are left off each end when the
    volume has pages enough to spare them -- so one page of a volume is
    never its cover. Every page when ``count`` asks for all of them.
    """
    if total <= 0 or count <= 0:
        return []
    if count >= total:
        return list(range(total))
    edge = min(SAMPLE_EDGE_PAGES, (total - count) // 2)
    span = total - 2 * edge
    step = span / count
    return sorted(
        {edge + min(span - 1, int((index + 0.5) * step)) for index in range(count)}
    )


def build_sample(storage_path: Path, pages: int) -> BenchSample:
    """Extract ``pages`` real pages into a scratch directory under ``.processing``."""
    library = storage_path / "library"
    archives = _archives_round_robin(library) if library.is_dir() else []
    if not archives:
        raise BenchError(
            400,
            "there are no volumes in the library to benchmark with — upload one first, "
            "then the numbers are measured on your own pages",
        )
    wanted = max(MIN_SAMPLE_PAGES, min(MAX_SAMPLE_PAGES, int(pages)))
    # A few volumes, several interior pages each (`SAMPLE_PAGES_PER_VOLUME`),
    # series taking turns; a volume too short to give its share leaves the
    # rest to the next ones. Read lazily: only the volumes visited are opened.
    def readable() -> Iterator[tuple[Path, list[str]]]:
        for archive in archives:
            names = _archive_pages(archive)
            if names:
                yield archive, names

    contents = readable()
    first = next(contents, None)
    if first is None:
        raise BenchError(
            400,
            "the library's archives have no readable pages to benchmark with",
        )
    processing = storage_path / ".processing"
    processing.mkdir(parents=True, exist_ok=True)
    directory = Path(tempfile.mkdtemp(prefix="bench-sample-", dir=str(processing)))

    # More from each volume only when the library has too few to go round.
    per_archive = max(SAMPLE_PAGES_PER_VOLUME, -(-wanted // len(archives)))
    extracted = 0
    volumes = 0
    try:
        for archive, names in itertools.chain([first], contents):
            if extracted >= wanted:
                break
            take = min(per_archive, wanted - extracted)
            indices = _spread(take, len(names))
            if not indices:
                continue
            wrote = 0
            with zipfile.ZipFile(archive, "r") as zf:
                for index in indices:
                    member = names[index]
                    suffix = Path(member).suffix.lower()
                    safe = _UNSAFE_NAME.sub("_", archive.stem)[:40]
                    target = directory / f"{extracted:04d}_{safe}{suffix}"
                    try:
                        with zf.open(member) as source, target.open("wb") as sink:
                            shutil.copyfileobj(source, sink)
                    except (KeyError, OSError, zipfile.BadZipFile):
                        continue
                    extracted += 1
                    wrote += 1
            if wrote:
                volumes += 1
    except Exception:
        shutil.rmtree(directory, ignore_errors=True)
        raise
    if extracted == 0:
        shutil.rmtree(directory, ignore_errors=True)
        raise BenchError(
            400, "no pages could be read out of the library's archives to benchmark with"
        )
    return BenchSample(directory, extracted, volumes)


# --- the clock (ADDENDUM 9) ---------------------------------------------


def ocr_json_emissions(workspace: Path) -> list[float]:
    """When each per-page JSON under ``<workspace>/_ocr`` was written, sorted.

    These ARE the monolithic road's emission timestamps: the fork writes one
    of these files per page as that page's result comes back out of its
    pipeline (``write_page_json``), which is the same instant a composed
    road would emit its ``page`` event. Nanosecond mtimes, because a fast
    engine puts several pages inside one millisecond and a resolution coarser
    than the gaps would manufacture the very burst this design is about.
    """
    root = workspace / "_ocr"
    if not root.is_dir():
        return []
    stamps: list[float] = []
    for path in root.rglob("*.json"):
        try:
            stamps.append(path.stat().st_mtime_ns / 1e9)
        except OSError:  # pragma: no cover - a file removed under us
            continue
    return sorted(stamps)


def emission_window(emissions: Sequence[float], *, passes: int = 1) -> dict[str, Any]:
    """The rate of a set of emission instants, with the pipeline fill skipped.

    The server's copy of the runner's ``bench_window``/``bench_fill``: the
    runner lives in another virtual environment (torch, no mokuro-bunko) and
    cannot be imported here, and one arithmetic in two places is cheaper than
    a shared import that would drag the whole runner in. The rules are the
    contract's, not either file's: fill = first ``min(8, N/4)``, rate =
    ``(M - 1) / (t_last - t_first)`` over the rest.
    """
    ordered = sorted(emissions)
    fill = min(8, max(0, len(ordered) // 4))
    measured = ordered[fill:]
    first = measured[0] if measured else None
    last = measured[-1] if measured else None
    window = (last - first) if (first is not None and last is not None) else 0.0
    return {
        "pages_per_second": (len(measured) - 1) / window
        if window > 0 and len(measured) > 1
        else 0.0,
        "window_seconds": max(0.0, window),
        "pages_measured": len(measured),
        "passes": passes,
        "short_window": window < BENCH_SHORT_WINDOW_SECONDS,
        "first_emission_at": first if first is not None else 0.0,
        "last_emission_at": last if last is not None else 0.0,
    }


def _window_utilization(
    sampler: UtilizationSampler, spawned: float, trial: Mapping[str, Any]
) -> dict[str, float | None]:
    """A trial's GPU/CPU means, over ITS window and no other second.

    The runner reports its window in seconds since its own process start, so
    the spawn instant is what puts it on this process's clock.
    """
    first = _as_float(trial.get("first_emission_at"))
    last = _as_float(trial.get("last_emission_at"))
    if first is None or last is None:
        return {"gpu_busy_pct": None, "cpu_busy_pct": None}
    return sampler.means(spawned + first, spawned + last)


def _link_pages(source: Path, workspace: Path) -> Path:
    """The sample's pages inside ``workspace``, hard-linked where possible.

    mokuro writes its ``_ocr`` cache beside the volume it is handed, so the
    pages have to live somewhere this run owns and can throw away. Hard links
    cost nothing (same filesystem: both are under ``<storage>/.processing``)
    and a copy is the fallback for a filesystem that will not have them.
    """
    pages = workspace / source.name
    pages.mkdir(parents=True, exist_ok=True)
    for path in sorted(source.iterdir()):
        if not path.is_file():
            continue
        target = pages / path.name
        try:
            os.link(path, target)
        except OSError:
            shutil.copy2(path, target)
    return pages


# --- the state machine --------------------------------------------------


def _spec_payload(spec: GenerationSpec) -> dict[str, Any]:
    """The engine/detector/patch_budget/pools a benchmark measured.

    Never ``name``/``primary``/``enabled``/``id`` -- a benchmark is a
    recipe, not a row's identity. This is what the bench object's own
    ``spec`` field echoes back to the caller.
    """
    data: dict[str, Any] = {"engine": spec.engine}
    if spec.detector is not None:
        data["detector"] = spec.detector
    data["patch_budget"] = spec.patch_budget
    # The row's precision mode: the machine benchmarks at it (and, for a
    # balanced/speed mode, tries its candidates), so it rides along.
    if spec.precision_applies and spec.precision != DEFAULT_PRECISION_MODE:
        data["precision"] = spec.precision
    data["pools"] = spec.pools.to_dict()
    return data


# The machine a benchmark measures when none is named: this server's own.
LOCAL_BENCH = "local"


class _BenchRun:
    """One benchmark, from the request that made it to the object it became."""

    def __init__(
        self,
        key: str,
        measured: GenerationSpec,
        pages: int,
        *,
        draft: bool,
        spec: Mapping[str, Any],
        processor: str = LOCAL_BENCH,
        entry: Any = None,
        autobench: bool = False,
        on_done: Callable[[str, Any], None] | None = None,
        precision_only: bool = False,
    ) -> None:
        self.key = key
        # Only the precision trials of a balanced/speed mode, at the pools
        # as given (a machine whose pools a person set): its result stores
        # the pick and never touches a pool.
        self.precision_only = precision_only
        # Which machine measures it: LOCAL_BENCH, or a connected processor
        # by name, whose registry entry rides along (`entry`). What it
        # measures is that machine's, and so is where its result is kept:
        # the row's own `.ocr-bench.json` for this server, the processor's
        # profile otherwise -- never one machine's number in the other's
        # place.
        self.processor = processor
        self.entry = entry
        # Asked for by the worker's auto-bench (spec section 4), which also
        # APPLIES the best widths it finds to that processor's profile, and
        # wants to hear how it ended, whatever that was: `on_done(state,
        # entry)`, with the registration it actually ran on.
        self.autobench = autobench
        self.on_done = on_done
        # What is actually measured: the parsed spec, or the saved row when
        # no spec was sent. Never a row's identity (name/primary/enabled) --
        # see `generations.parse_bench_spec`.
        self.measured = measured
        self.requested_pages = pages
        self.draft = draft
        self.lock = threading.Lock()
        self.cancelled = False
        self.session: Any | None = None
        self.process: subprocess.Popen[Any] | None = None
        # Set once this run reaches a terminal state, so `cancel()` can wait
        # for just THIS run instead of the line thread's entire future.
        self.done_event = threading.Event()
        self.data: dict[str, Any] = {
            "state": "queued",
            "key": key,
            "generation": key,
            # Which machine measured this. "local" is the library server's
            # own hardware; anything else is a connected processor's name,
            # and `host` below is THAT machine's.
            "processor": processor,
            # Queued by the worker's auto-bench rather than by a person: the
            # admin page says so, so a Cancel is not pressed by mistake.
            "autobench": autobench,
            "precision_only": precision_only,
            "spec": dict(spec),
            "started_at": _now_iso(),
            "finished_at": None,
            "waiting_for_queue": True,
            "sample": None,
            "host": None,
            "tunable": not measured.monolithic,
            "progress": None,
            "startup_seconds": None,
            "trials": [],
            "baseline": None,
            "best": None,
            # What the recognizer ran at (an engine the row's precision mode
            # reaches), the mode it was measured for and, for a balanced/speed
            # mode, one trial per supported candidate.
            "precision": None,
            "precision_mode": None,
            "precision_trials": None,
            "precision_why": None,
            "peak_rss_mb": None,
            "peak_vram_mb": None,
            "estimates": None,
            "preempted": [],
            "error": None,
        }

    def snapshot(self) -> dict[str, Any]:
        with self.lock:
            data: dict[str, Any] = json.loads(json.dumps(self.data))
        if data.get("state") != "running":
            data["progress"] = None
        return data

    def update(self, **fields: Any) -> None:
        with self.lock:
            self.data.update(fields)

    def cancel(self) -> None:
        with self.lock:
            self.cancelled = True
            session, process = self.session, self.process
        if session is not None:
            session.kill()
        if process is not None and process.poll() is None:
            try:
                process.kill()
            except OSError:  # pragma: no cover - defensive
                pass


class BenchService:
    """A FIFO queue of benchmarks, one running on each MACHINE at a time.

    A benchmark answers "should I commit to this row?" and so measures a
    SPEC -- the row as it is edited in the admin UI, saved or not -- rather
    than only a saved row (ADDENDUM 5). Benchmarks QUEUE rather than
    refusing a second request with 409 (ADDENDUM 6).

    Each machine -- this server's own hardware, or a processor -- has a LINE
    of its own: its benchmarks run one after another, and benchmarks of
    different machines run at the same time, because one card's measurement
    says nothing about, and costs nothing on, a card in another room. A
    machine is held and pre-empted ONCE, when the first benchmark of its
    line starts (see `OCRWorker.preempt_for_bench`), and released as soon as
    no benchmark of it is left -- never later: so the volumes it interrupted
    do not restart between two of its own benchmarks, and it goes back to
    work while other machines are still being measured. (Measured live with
    one line for every machine: three processors' autobenchmarks of a new
    row ran one after another, and each machine stayed held until the LAST
    of them ended -- one card idle for 4.5 minutes behind the others'.)

    ``self._queue`` stays the one FIFO list of every machine's runs, in the
    order they were asked for; a run is RUNNING when it is the first of its
    machine's. One thread per machine with a line (``self._lines``), started
    on its first request and ended when the line drains; see `_run_line`.
    """

    def __init__(
        self,
        storage_path: Path,
        *,
        worker: Callable[[], OCRWorker | None],
        generations: Callable[[], Sequence[GenerationSpec]],
        backend: Callable[[], str | None] = lambda: None,
        environment_problem: Callable[[GenerationSpec], str | None] = lambda row: None,
        remaining_pages: Callable[[GenerationSpec], int | None] = lambda row: None,
        log: Callable[[str], None] | None = None,
        processors: Callable[[], Sequence[Any]] = lambda: (),
        profiles: Any = None,
    ) -> None:
        self.storage_path = Path(storage_path)
        # The connected processors (`ProcessorRegistry.entries`) a benchmark
        # may be run on, and where their results are kept
        # (`ProcessorProfiles`). Both default to "none": a server with no
        # registry benchmarks only itself.
        self._processors = processors
        self._profiles = profiles
        self._worker = worker
        self._generations = generations
        self._backend = backend
        self._environment_problem = environment_problem
        self._remaining_pages = remaining_pages
        self._log = log or (lambda message: logger.info("%s", message))
        self._cv = threading.Condition()
        # FIFO of every machine's runs. The first run OF EACH MACHINE is the
        # head of that machine's line -- running, or about to be as soon as
        # its hold takes -- the rest are waiting their turn behind it.
        self._queue: list[_BenchRun] = []
        # The last result of each (key, machine), whatever it was (done,
        # failed or cancelled), kept for the life of the PROCESS. This is
        # what makes a draft key's result readable at all (it is never
        # written to disk), and what lets a GET show a failed/cancelled
        # attempt of a saved id before a later success overwrites it on disk.
        # Per MACHINE: one row can be measured on several, and one machine's
        # result read as another's would put a 48-core box's widths in a
        # 4-core box's table.
        self._recent: dict[tuple[str, str], dict[str, Any]] = {}
        # Machine -> the thread running its line; present while that line has
        # (or is just finishing) runs. Read and changed under `_cv` only.
        self._lines: dict[str, threading.Thread] = {}
        self._host: dict[str, Any] | None = None

    # --- requests -------------------------------------------------------

    def enqueue(
        self,
        key: str,
        spec: Mapping[str, Any] | None,
        pages: int | None = None,
        processor: str = LOCAL_BENCH,
        *,
        autobench: bool = False,
        on_done: Callable[[str, Any], None] | None = None,
        precision_only: bool = False,
    ) -> dict[str, Any]:
        """Validate and queue a benchmark of ``spec`` (or the saved row), by ``key``.

        ``processor`` is the machine to measure (spec section 5): this
        server's own hardware (``"local"``, the default) or a connected
        processor by name, whose catalog must be able to run the spec.

        409 only when `key` ITSELF is already queued or running on that same
        machine -- a benchmark of a different row, or of the same row on
        another machine, queues freely behind it. Everything that can be said
        at once (an unusable spec, an unknown key or machine, a missing
        environment, an empty library) is said before anything is queued or
        the OCR queue is touched.
        """
        processor = str(processor or LOCAL_BENCH)
        entry = None
        devices = cached_catalog()
        if processor != LOCAL_BENCH:
            entry = next(
                (
                    candidate
                    for candidate in self._processors()
                    if not getattr(candidate, "local", False)
                    and getattr(candidate, "name", None) == processor
                    and not getattr(candidate, "dropped", False)
                ),
                None,
            )
            if entry is None:
                raise BenchError(400, f"no processor called {processor!r} is connected")
            devices = catalog_from_processor(entry.catalog)
        draft = is_draft_key(key)
        saved_row = self._row(key)
        if not draft and saved_row is None:
            raise BenchError(400, f"there is no generation {key!r} to benchmark")
        if spec is not None:
            try:
                # The same catalog a PUT is held to -- the catalog of the
                # machine that will run it: a benchmark on a card that
                # machine does not have would fail minutes later, in a log.
                measured = parse_bench_spec(spec, devices=devices)
            except GenerationConfigError as e:
                raise BenchError(400, str(e), row=None, field=e.field) from None
        elif saved_row is not None:
            measured = saved_row
        else:
            raise BenchError(
                400,
                f"there is no generation {key!r} to benchmark — send a spec to measure one "
                "that is not saved yet",
            )
        worker = self._worker()
        if worker is None:
            raise BenchError(400, "OCR is disabled in this server process")
        if entry is not None:
            from mokuro_bunko.ocr.remote.scheduler import catalog_can_run

            reason = catalog_can_run(entry.catalog, measured)
            if reason is not None:
                raise BenchError(400, f"{processor} cannot run this row: {reason}")
            if measured.monolithic:
                # This server gives such a row one timed run of its own CLI
                # (`_run_monolithic`); a processor has only the runner's
                # ``--bench``, which needs a staged pipeline to tune and
                # fails on a row that has none. Said now, before anything
                # is held, packed or persisted.
                raise BenchError(
                    400,
                    f"{self._display_name(key)} reads each volume behind its own command "
                    f"line: it has no pipeline for {processor} to benchmark",
                )
        else:
            if not getattr(worker, "local_processing", True):
                raise BenchError(
                    400,
                    "this server runs no OCR of its own (ocr.local_processing is off); "
                    "choose a connected processor to benchmark on",
                )
            problem = self._environment_problem(measured)
            if problem:
                raise BenchError(400, problem)
            from mokuro_bunko.ocr.precision import row_refusal

            refused = row_refusal(measured, devices)
            if refused is not None:
                # Not eligible for the row's precision mode: never benchmarked
                # for it, as it is never offered its volumes.
                raise BenchError(400, f"this server cannot run this row: {refused}")
        if pages is not None and (
            not isinstance(pages, int)
            or isinstance(pages, bool)
            or not MIN_SAMPLE_PAGES <= pages <= MAX_SAMPLE_PAGES
        ):
            raise BenchError(
                400,
                f"pages must be a whole number between {MIN_SAMPLE_PAGES} and "
                f"{MAX_SAMPLE_PAGES}",
            )
        # Refused here rather than discovered inside the run: an empty
        # library is the caller's mistake, and holding the queue and loading
        # a model before saying so would be a slow way to say it.
        library = self.storage_path / "library"
        if not library.is_dir() or not any(library.rglob("*.cbz")):
            raise BenchError(
                400,
                "there are no volumes in the library to benchmark with — upload one first, "
                "then the numbers are measured on your own pages",
            )
        run = _BenchRun(
            key,
            measured,
            int(pages or DEFAULT_SAMPLE_PAGES),
            draft=draft,
            spec=_spec_payload(measured),
            processor=processor,
            entry=entry,
            autobench=autobench,
            on_done=on_done,
            precision_only=precision_only,
        )
        with self._cv:
            existing = self._find(key, processor)
            if existing is not None and existing.data.get("state") in ("queued", "running"):
                raise BenchError(
                    409,
                    f"a benchmark of {self._display_name(key)} is already queued or running "
                    + ("" if processor == LOCAL_BENCH else f"on {processor} ")
                    + "— re-posting the same row is a no-op",
                )
            self._queue.append(run)
            line = self._lines.get(processor)
            if line is None or not line.is_alive():
                # A line that is ending re-checks for runs under `_cv` before
                # it leaves `_lines` (see `_run_line`), so a run appended while
                # it is still listed is picked up by it, never stranded.
                line = threading.Thread(
                    target=self._run_line, args=(processor,),
                    name=f"ocr-bench-line-{processor}", daemon=True,
                )
                self._lines[processor] = line
                line.start()
            self._cv.notify_all()
        return self.get(key, processor)

    def get(self, key: str, processor: str | None = None) -> dict[str, Any]:
        """This key's live or last-known benchmark ON ONE MACHINE, else idle.

        ``processor`` is the machine -- this server's own (``"local"``, also
        what None means, the same default as `enqueue`) or a processor by
        name -- because one row can be queued on several, and the worker
        queues auto-benchmarks of its own: a read that picked whichever run
        came first showed another machine's benchmark as this one's.

        Always carries the CURRENT global ``queue`` (so a page can show the
        whole list from any one row) and, while ``key`` sits in it, its live
        ``position`` in ITS MACHINE's line (0 = running or about to be):
        another machine's benchmarks run beside it, not ahead of it.
        """
        processor = str(processor or LOCAL_BENCH)
        with self._cv:
            order = [run.key for run in self._queue]
            run = self._find(key, processor)
            if run is not None:
                data = run.snapshot()
                data["position"] = self._line(processor).index(run)
                data["queue"] = {"running": order[0] if order else None, "queued": order[1:]}
                return data
            recent = self._recent.get((key, processor))
            queue_now = {"running": order[0] if order else None, "queued": order[1:]}
        if recent is not None:
            data = dict(recent)
            data["position"] = None
            data["queue"] = queue_now
            return data
        if not is_draft_key(key) and processor == LOCAL_BENCH:
            # `.ocr-bench.json` is this server's; a processor's finished
            # benchmarks are kept in its profile, never here.
            saved = self.saved(key)
            if saved is not None:
                data = dict(saved)
                data["position"] = None
                data["queue"] = queue_now
                return data
        return {"state": "idle", "generation": key, "key": key, "queue": queue_now}

    def cancel(self, key: str, processor: str | None = None) -> dict[str, Any]:
        """Cancel this key's queued or running benchmark on ONE machine.

        ``processor`` as for `get`: None is this server's own. A queued one
        is removed at once (state ``cancelled``, the ones behind it move up);
        the running one is killed and the line's own loop starts the next as
        soon as it notices.
        """
        processor = str(processor or LOCAL_BENCH)
        with self._cv:
            run = self._find(key, processor)
            if run is None or run.data.get("state") not in ("queued", "running"):
                raise BenchError(
                    400, "there is no benchmark of this generation queued or running to cancel"
                )
            is_head = self._head(processor) is run
            if not is_head:
                self._queue.remove(run)
                self._cv.notify_all()
        if not is_head:
            self._finish(run, "cancelled")
            return run.snapshot()
        run.cancel()
        run.done_event.wait(timeout=60.0)
        return self.get(key, processor)

    def saved(self, generation_id: str) -> dict[str, Any] | None:
        """The last FINISHED (persisted) benchmark of a saved row, from disk."""
        return self._load().get(generation_id)

    def saved_summaries(self, rows: Sequence[GenerationSpec]) -> dict[str, dict[str, Any]]:
        """``{id: bench without trials}`` for the generations table."""
        history = self._load()
        summaries: dict[str, dict[str, Any]] = {}
        for row in rows:
            entry = history.get(row.id)
            if not isinstance(entry, dict):
                continue
            trimmed = dict(entry)
            trimmed.pop("trials", None)
            trimmed["progress"] = None
            summaries[row.id] = trimmed
        return summaries

    def paused_for_benchmark(self) -> dict[str, Any] | None:
        """``{"key", "generation", "queued"}`` while a line holds the OCR queue, else None.

        ``generation`` is the row's NAME for a saved id, or the draft key
        itself -- whatever the Queue page can show without knowing what a
        draft key is. With several machines measured at once this names the
        EARLIEST-asked running one; ``queued`` is how many MORE are left,
        on any machine, running beside it or waiting.
        """
        with self._cv:
            if not self._queue:
                return None
            head = self._queue[0]
            return {
                "key": head.key,
                "generation": self._display_name(head.key),
                "queued": len(self._queue) - 1,
                # Which machine is being measured: "local", or a processor's
                # name. Only that machine's queue is held (spec section 3
                # rule 5); every other keeps working.
                "processor": head.processor,
            }

    def configuring(self) -> dict[str, dict[str, Any]]:
        """``{machine: {"key", "generation", "auto"}}`` for each machine with a line.

        What the head of each machine's line measures -- running, or about
        to as soon as its hold takes -- so the queue page's card for that
        machine says what it is configuring instead of reading as idle.
        ``auto`` is an automatic benchmark (a new row, or a new machine,
        measured before it runs there); ``generation`` is the row's NAME,
        or the draft key itself for an unsaved spec.
        """
        with self._cv:
            heads: dict[str, _BenchRun] = {}
            for run in self._queue:
                heads.setdefault(run.processor, run)
            return {
                machine: {
                    "key": run.key,
                    "generation": self._display_name(run.key),
                    "auto": bool(run.autobench),
                }
                for machine, run in heads.items()
            }

    # --- persistence ----------------------------------------------------

    def _load(self) -> dict[str, dict[str, Any]]:
        try:
            data = json.loads(bench_path(self.storage_path).read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, UnicodeDecodeError):
            return {}
        if not isinstance(data, dict):
            return {}
        return {str(k): v for k, v in data.items() if isinstance(v, dict)}

    def _save(self, generation_id: str, result: Mapping[str, Any]) -> None:
        """Keep one finished result, pruned to the rows that still exist.

        Never called for a draft key (`_finish` guards it): a draft's result
        lives only in `_recent`, for the life of the process.
        """
        history = self._load()
        history[generation_id] = dict(result)
        known = {row.id for row in self._generations()}
        known.add(generation_id)
        history = {key: value for key, value in history.items() if key in known}
        path = bench_path(self.storage_path)
        try:
            tmp = path.with_name(path.name + ".tmp")
            tmp.write_text(json.dumps(history, ensure_ascii=False, indent=2), encoding="utf-8")
            os.replace(tmp, path)
        except OSError as e:  # pragma: no cover - disk trouble
            logger.warning("Could not persist the OCR benchmark result: %s", e)

    def prune(self, known_ids: Iterable[str]) -> None:
        """Drop the results of rows that no longer exist."""
        keep = {str(value) for value in known_ids}
        history = self._load()
        pruned = {key: value for key, value in history.items() if key in keep}
        if pruned == history:
            return
        path = bench_path(self.storage_path)
        try:
            if not pruned:
                if path.exists():
                    path.unlink()
                return
            tmp = path.with_name(path.name + ".tmp")
            tmp.write_text(json.dumps(pruned, ensure_ascii=False, indent=2), encoding="utf-8")
            os.replace(tmp, path)
        except OSError as e:  # pragma: no cover - disk trouble
            logger.warning("Could not prune the OCR benchmark results: %s", e)

    # --- helpers --------------------------------------------------------

    def _row(self, generation_id: str) -> GenerationSpec | None:
        for row in self._generations():
            if row.id == generation_id:
                return row
        return None

    def _display_name(self, key: str) -> str:
        """A saved row's NAME, or the key itself (a draft has no row)."""
        row = self._row(key)
        return row.name if row is not None else key

    def _line(self, processor: str) -> list[_BenchRun]:
        """One machine's runs still in the queue, in order. Caller holds ``self._cv``."""
        return [run for run in self._queue if run.processor == processor]

    def _head(self, processor: str) -> _BenchRun | None:
        """The run at the head of one machine's line. Caller holds ``self._cv``."""
        return next((run for run in self._queue if run.processor == processor), None)

    def _find(self, key: str, processor: str | None = None) -> _BenchRun | None:
        """The run for ``key`` still in the queue (running or waiting its turn).

        On one machine when ``processor`` names it, else on any. Caller
        holds ``self._cv``.
        """
        for run in self._queue:
            if run.key == key and (processor is None or run.processor == processor):
                return run
        return None

    def host(self, engines_python: Path | None) -> dict[str, Any]:
        """The host description, probed once per process (torch is slow to import)."""
        if self._host is None:
            self._host = describe_host(self._backend(), engines_python)
        return dict(self._host)

    # --- running --------------------------------------------------------

    def _run_line(self, machine: str) -> None:
        """One machine's line-thread: runs its benchmarks until none is left.

        The machine is held and pre-empted ONCE, when the first benchmark of
        the line starts (`OCRWorker.preempt_for_bench`), and released ONCE,
        when no benchmark of it is left, whatever ran in between and however
        many benchmarks it was. Only the first run carries `preempted` --
        nothing claims anything else on it while it stays held, so there is
        nothing left to interrupt for the second benchmark onward. Other
        machines' lines run beside this one and never hold this machine
        (spec section 3 rule 5).

        The thread leaves `_lines` only under `_cv` and only after finding
        the line empty, so a run `enqueue` appends meanwhile is either seen
        here (and run, after one more hold) or starts a new thread.
        """
        while True:
            worker = self._worker()
            if worker is None:  # pragma: no cover - defensive; enqueue() already checked
                with self._cv:
                    stuck = self._line(machine)
                    self._queue = [run for run in self._queue if run.processor != machine]
                for waiting in stuck:
                    waiting.update(error="OCR is disabled in this server process")
                    self._finish(waiting, "failed")
            else:
                held = False
                try:
                    while True:
                        with self._cv:
                            run = self._head(machine)
                        if run is None:
                            break
                        if not held and not run.cancelled:
                            _quiet, preempted = worker.preempt_for_bench(
                                timeout=QUEUE_HOLD_TIMEOUT, processor=machine
                            )
                            held = True
                            run.update(preempted=preempted)
                        self._run_one(run, worker)
                        with self._cv:
                            if run in self._queue:
                                self._queue.remove(run)
                            self._cv.notify_all()
                finally:
                    if held:
                        worker.release_queue(processor=machine)
            with self._cv:
                if self._head(machine) is None:
                    if self._lines.get(machine) is threading.current_thread():
                        del self._lines[machine]
                    return

    def _run_one(self, run: _BenchRun, worker: OCRWorker) -> None:
        """Sample, then measure -- the OCR queue is already held by the line."""
        sample: BenchSample | None = None
        try:
            if run.cancelled:
                self._finish(run, "cancelled")
                return
            run.update(waiting_for_queue=False, state="running")
            sample = build_sample(self.storage_path, run.requested_pages)
            run.update(sample=sample.to_dict())
            processor = worker.processor
            if run.processor != LOCAL_BENCH:
                # THAT machine's host, not this one's: the estimate has to
                # read "on tower (RTX 4090)" (spec section 5).
                run.update(host=dict(getattr(run.entry, "host", None) or {}))
                self._run_remote(run, sample)
                return
            run.update(host=self.host(processor.device_probe_python()))
            # The SAME question the queue asks (``runs_mokuro_cli``): a row
            # that streams its pages into one process is benchmarked through
            # the runner like any other, and only a row that really is one
            # invocation a volume gets the single timed run.
            if processor.runs_mokuro_cli(run.measured):
                self._run_monolithic(run, processor, sample)
            else:
                self._run_composed(run, processor, sample)
        except BenchError as e:
            run.update(error=e.message)
            self._finish(run, "failed")
        except Exception as e:  # pragma: no cover - defensive
            logger.exception("OCR benchmark failed")
            run.update(error=str(e))
            self._finish(run, "failed")
        finally:
            if sample is not None:
                sample.cleanup()

    def _finish(self, run: _BenchRun, state: str) -> None:
        """Settle one run: persist (if it is one to keep) BEFORE the state is visible.

        A caller polling `get()`/`saved()` for "done" must never observe it
        before the write to `.ocr-bench.json` has actually landed -- so the
        fields are folded into a snapshot and saved FIRST, and only then does
        `run.update` make that state visible to a reader.
        """
        if state == "failed" and run.cancelled:
            # A cancel kills the runner, and whichever path notices the end
            # first -- often the read loop, blocked on its next event, reading
            # the runner's own exit -- would call it a failure. It was asked
            # for: it is a cancel, and the error its death produced is noise.
            state = "cancelled"
            run.update(error=None)
        finished_at = _now_iso()
        if state == "done" and not run.draft:
            snapshot = run.snapshot()
            snapshot.update(
                state=state, finished_at=finished_at, progress=None, waiting_for_queue=False
            )
            if run.processor == LOCAL_BENCH:
                if not run.precision_only:
                    # A precision-only run is this machine's PICK, kept in its
                    # profile below; the row's own record keeps whatever a
                    # whole benchmark (a person's, or the last) found.
                    self._save(run.key, snapshot)
                if run.autobench:
                    # This server's auto-benchmark (a row nobody configured
                    # by hand): what it found is this machine's own profile,
                    # exactly as a processor's is -- never the config.
                    self._persist_remote(run, snapshot, profile=LOCAL_PROFILE)
            else:
                # A processor's number is THAT machine's: into its profile,
                # never over the row's own `.ocr-bench.json`, which is this
                # server's and which the queue's rates are read from.
                self._persist_remote(run, snapshot)
        run.update(state=state, finished_at=finished_at, progress=None, waiting_for_queue=False)
        with self._cv:
            self._recent[(run.key, run.processor)] = run.snapshot()
            # Draft results are memory only, and a long-lived server sees a
            # draft key for every row someone ever tried out: keep the newest
            # few so this cannot grow without bound.
            drafts = [k for k in self._recent if is_draft_key(k[0])]
            for stale in drafts[: max(0, len(drafts) - MAX_DRAFT_RESULTS)]:
                del self._recent[stale]
        run.done_event.set()
        self._log(
            f"Benchmark of {self._display_name(run.key)}"
            + ("" if run.processor == LOCAL_BENCH else f" on {run.processor}")
            + f": {state}"
            + (f" ({run.data.get('error')})" if run.data.get("error") else "")
        )
        if run.on_done is not None:
            try:
                run.on_done(state, run.entry)
            except Exception:  # pragma: no cover - a listener never breaks the line
                logger.exception("the benchmark's listener failed")

    def _estimates(self, run: _BenchRun, pages_per_second: float | None) -> dict[str, Any]:
        """What the measured speed means for a volume, and for what is left.

        ADDENDUM 9: ``200 / pages_per_second`` and NOTHING ELSE. Startup is
        reported once, separately, as "first page after X s" -- a session
        pays it once for a whole queue of volumes (ADDENDUM 2), so folding it
        into a per-volume estimate overstated every volume after the first,
        and folding it into a rate would have let a slow model load make a
        fast engine look slow.

        ``remaining_pages`` comes from page counts the server already knows
        (the metadata cache and the library index): a benchmark must not open
        every archive in the library to answer a question about how long the
        queue is.
        """
        remaining = self._remaining_pages(run.measured)
        volume_seconds: int | None = None
        remaining_seconds: int | None = None
        if pages_per_second and pages_per_second > 0:
            volume_seconds = int(round(BENCH_VOLUME_PAGES / pages_per_second))
            if remaining is not None:
                remaining_seconds = int(round(remaining / pages_per_second))
        return {
            "volume_200_pages_seconds": volume_seconds,
            "remaining_pages": remaining,
            "remaining_seconds": remaining_seconds,
        }

    @staticmethod
    def _placement_to_apply(run: _BenchRun, best: Mapping[str, Any]) -> dict[str, str]:
        """``best.stage_device`` as a whole table, the way ``pools`` holds one.

        The runner is GIVEN the spec's placement (``--stage-device``: where
        the models go is part of the question) but not its widths, so its
        ``best.stage_workers`` is already a whole table while its
        ``best.stage_device`` names only the stages its search MOVED. Applied
        as it stands, a pin the run was measured with -- ``detect: cpu`` --
        silently became ``auto``, and ``auto`` on another machine is another
        placement (the paddle-manga-animetext incident: a card the detector's
        onnxruntime could not reach).

        So: the spec's pins, then what the search moved. And a pin the runner
        did NOT honour (it reports where each model really came up on
        ``bench_ready``) is replaced by where it ran, because a benchmark must
        never persist a placement it did not measure.
        """
        placed = {
            str(k): str(v)
            for k, v in ((run.data.get("host") or {}).get("devices") or {}).items()
        }
        moved = {str(k): str(v) for k, v in (best.get("stage_device") or {}).items()}
        table: dict[str, str] = {}
        for key, pin in run.measured.pools.stage_device.items():
            ran = placed.get(key)
            explicit = pin not in ("", DEVICE_AUTO)
            table[key] = ran if explicit and ran and ran != pin else pin
        table.update(moved)
        return table

    @staticmethod
    def _workers_to_apply(
        run: _BenchRun, best: Mapping[str, Any], ran: Mapping[str, Any] | None
    ) -> dict[str, Any]:
        """``best.stage_workers`` as a whole table: the search's widths, then
        what each width the spec pins really ran at.

        The runner is never GIVEN the spec's widths (``open_bench``: it
        measures the machine, not the tuning), so its ``best.stage_workers``
        names only the widths that differ from the DERIVED ones. A width the
        row pins and the search left alone ran derived, never at the pin --
        applied as it stands (``{}`` for a derived winner) the stored pools
        were no opinion (`profiles.holds_pools`), and the machine ran the
        row's pin: a width the benchmark never measured.

        So each such pin reads what the winning trial (``ran``) ran it at:
        the pin itself when the derivation landed on it (the spec WAS what
        was measured), `POOL_AUTO` -- derived there -- otherwise, or when the
        runner did not say.
        """
        table: dict[str, Any] = {
            str(k): int(v) for k, v in (best.get("stage_workers") or {}).items()
        }
        widths = ran if isinstance(ran, Mapping) else {}
        for key, pin in run.measured.pools.stage_workers.items():
            if key in table:
                continue
            try:
                same = widths.get(key) is not None and int(widths[key]) == int(pin)
            except (TypeError, ValueError):
                same = False
            table[key] = int(pin) if same else POOL_AUTO
        return table

    @staticmethod
    def _capacity_to_apply(
        run: _BenchRun, best: Mapping[str, Any], ran: Mapping[str, Any] | None
    ) -> dict[str, Any]:
        """``best.queue_capacity`` as a whole table: any capacity the runner
        named, then what each capacity the spec pins really ran at.

        The search never chooses a capacity -- it derives them from the
        widths, and its ``best.queue_capacity`` is always ``{}`` -- and, like
        the widths, the runner is never GIVEN the spec's capacities
        (``open_bench``). So a capacity the row pins ran derived, never at
        the pin, and copying the pin into the result stored a capacity the
        benchmark never measured (``{engine: 4, post: 4}`` for runs that ran
        engine queue 1); it also hid every capacity difference from
        ``same_as_spec``. Read as a whole table, the runner's ``{}`` instead
        dropped the pins outright.

        So each such pin reads what the winning trial (``ran``) ran it at --
        the rule `_workers_to_apply` applies to widths: the pin itself when
        the derivation landed on it, `POOL_AUTO` otherwise, or when the
        runner did not say.
        """
        table: dict[str, Any] = {
            str(k): int(v) for k, v in (best.get("queue_capacity") or {}).items()
        }
        capacities = ran if isinstance(ran, Mapping) else {}
        for key, pin in run.measured.pools.queue_capacity.items():
            if key in table:
                continue
            try:
                same = capacities.get(key) is not None and int(capacities[key]) == int(pin)
            except (TypeError, ValueError):
                same = False
            table[key] = int(pin) if same else POOL_AUTO
        return table

    def _same_as_spec(self, run: _BenchRun, best: Mapping[str, Any]) -> bool:
        """True when applying the best widths would change nothing.

        Each of ``best``'s tables is, by the time this runs, what a row's
        ``pools`` holds for it: ``stage_workers`` the search's widths with the
        spec's pins as they ran (`_workers_to_apply`: a pin the derivation
        did not land on is `POOL_AUTO`, which no pin equals),
        ``queue_capacity`` the same for capacities (`_capacity_to_apply`),
        and ``stage_device`` completed from the measured spec
        (`_placement_to_apply`). So the comparison is
        the three maps -- against the MEASURED spec's pools, not necessarily
        what is saved.
        """
        pools = run.measured.pools
        workers = {
            str(k): (v if v == POOL_AUTO else int(v))
            for k, v in (best.get("stage_workers") or {}).items()
        }
        capacity = {
            str(k): (v if v == POOL_AUTO else int(v))
            for k, v in (best.get("queue_capacity") or {}).items()
        }
        devices = {str(k): str(v) for k, v in (best.get("stage_device") or {}).items()}
        return (
            workers == dict(pools.stage_workers)
            and capacity == dict(pools.queue_capacity)
            and devices == dict(pools.stage_device)
        )

    def _sampler(self, device: str | None) -> UtilizationSampler:
        """The utilization sampler for a run. The seam the tests replace."""
        return sampler_for(device)

    def _run_composed(
        self, run: _BenchRun, processor: OCRProcessor, sample: BenchSample
    ) -> None:
        """``engine_runner.py --bench``: one process, many trials, one answer."""
        log_path = self.storage_path / "logs" / "ocr" / f"bench.{run.measured.name}.log"
        session = processor.open_bench(
            run.measured, sample.directory, log_path, precision_only=run.precision_only
        )
        with run.lock:
            run.session = session
        # The runner times itself in seconds since ITS process start, so a
        # trial's window is only locatable on this clock from the instant the
        # process was spawned. Taken BEFORE the spawn, so the window is never
        # placed earlier than it really was.
        spawned = time.monotonic()
        sampler = self._sampler(utilization_device(run.measured.pools)).start()
        try:
            self._read_composed(run, session, sampler, spawned, sample)
        finally:
            sampler.stop()

    def _run_remote(self, run: _BenchRun, sample: BenchSample) -> None:
        """Pack the sample, hand it over, and read the same events back.

        The sample is packed as a ``.cbz`` under ``<storage>/.processing/``
        -- NOT inside the served library tree -- and the processor pulls it
        from a route of its own, exactly the way it pulls a volume. Reusing
        the archive path means the pages a benchmark measures arrive the
        same way the pages a volume measures do.
        """
        from mokuro_bunko.ocr.remote.registry import BENCH_ID_PREFIX
        from mokuro_bunko.ocr.remote.session import RemoteBench

        bid = f"{BENCH_ID_PREFIX}{run.key}"
        # The machine as it is connected NOW: one that reconnected since the
        # benchmark was queued has a new registration, and the old one would
        # be a stream nobody reads.
        current = next(
            (
                candidate
                for candidate in self._processors()
                if not getattr(candidate, "local", False)
                and getattr(candidate, "name", None) == run.processor
                and not getattr(candidate, "dropped", False)
            ),
            None,
        )
        if current is None:
            raise BenchError(400, f"{run.processor} is no longer connected")
        run.entry = current
        archive = self._pack_sample(sample, bid)
        try:
            bench = RemoteBench(
                run.entry,
                bid=bid,
                spec=_spec_payload(run.measured),
                sample_url=f"/_processor/{run.entry.processor_id}/bench/{bid}/sample",
                pages=sample.pages,
                precision_only=run.precision_only,
            )
            with run.lock:
                run.session = bench
            # No LOCAL utilization sampler: the GPU that matters is the
            # processor's, and it stamps its own busy% on each trial.
            sampler = self._sampler(None)
            try:
                self._read_composed(run, bench, sampler, time.monotonic(), sample)
            finally:
                sampler.stop()
        finally:
            archive.unlink(missing_ok=True)

    def _pack_sample(self, sample: BenchSample, bid: str) -> Path:
        """The sample as one stored ``.cbz``, where only a processor can reach it."""
        from mokuro_bunko.ocr.remote.library_api import bench_sample_filename

        processing = self.storage_path / ".processing"
        processing.mkdir(parents=True, exist_ok=True)
        archive = processing / bench_sample_filename(bid)
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_STORED) as zf:
            for page in sorted(sample.directory.iterdir()):
                if page.is_file():
                    zf.write(page, page.name)
        return archive

    def _persist_remote(
        self, run: _BenchRun, result: Mapping[str, Any], *, profile: str | None = None
    ) -> None:
        """A finished benchmark, into that machine's profile.

        The processor's (``run.processor``), or this server's own when the
        worker's auto-bench measured it here (``profile=LOCAL_PROFILE``).
        The bench summary always; the best widths too when the worker's
        auto-bench asked for it (spec section 4: "applies `best`, and only
        then offers it volumes"). A draft is never kept, here as locally.
        """
        if self._profiles is None or is_draft_key(run.key):
            return
        name = profile or run.processor
        best = dict(result.get("best") or {})
        recipe = run.measured.output_affecting()
        self._profiles.set_bench(
            name,
            run.key,
            {
                "pages_per_second": best.get("pages_per_second"),
                "window_seconds": best.get("window_seconds"),
                "gpu_busy_pct": best.get("gpu_busy_pct"),
                "cpu_busy_pct": best.get("cpu_busy_pct"),
                "startup_seconds": result.get("startup_seconds"),
                "host": dict(result.get("host") or {}),
                "at": result.get("finished_at") or _now_iso(),
                # What the recognizer ran at and the mode it was measured for;
                # for a balanced/speed mode that is this machine's PICK, with
                # every candidate's trial -- what the row runs at here until
                # the mode, the candidates or the runner change
                # (`profiles.stale_bench_reason`).
                **({"precision": result["precision"]} if result.get("precision") else {}),
                **(
                    {"precision_mode": result["precision_mode"]}
                    if result.get("precision_mode")
                    else {}
                ),
                **(
                    {
                        "precision_trials": list(result["precision_trials"]),
                        "precision_why": str(result.get("precision_why") or ""),
                    }
                    if result.get("precision_trials")
                    else {}
                ),
            },
            recipe=recipe,
        )
        if run.autobench and not run.precision_only and not best.get("same_as_spec"):
            # Only into a pair that has no pools yet: pools an admin saved
            # for this machine while its benchmark ran are that person's
            # decision, and `best` would silently replace them wholesale.
            # (A table `best` leaves empty is no opinion there,
            # `profiles.machine_pools`: the row's own runs for it.)
            #
            # And only when the benchmark CHANGED something: a best that is
            # the measured spec is nothing to apply, and writing it would
            # freeze today's row table on this machine, out of reach of the
            # next edit of the row.
            #
            # Never a precision: that is the row's mode (a balanced/speed
            # pick is kept with the benchmark above, not in the pools).
            pools: dict[str, Any] = {
                "stage_workers": dict(best.get("stage_workers") or {}),
                "queue_capacity": dict(best.get("queue_capacity") or {}),
                "stage_device": dict(best.get("stage_device") or {}),
            }
            self._profiles.set_pools(
                name,
                run.key,
                pools,
                recipe=recipe,
                keep_existing=True,
                # Written by a benchmark, not a person (`POOLS_AUTOBENCH`).
                autobench=True,
            )

    def _read_composed(
        self,
        run: _BenchRun,
        session: Any,
        sampler: UtilizationSampler,
        spawned: float,
        sample: BenchSample,
    ) -> None:
        if not session.start():
            event = session.poll_event(timeout=5.0)
            run.update(error=str((event or {}).get("error") or "the runner would not start"))
            self._finish(run, "failed")
            return
        deadline = time.monotonic() + BENCH_BUDGET_SECONDS + BENCH_SLACK_SECONDS
        trials: list[dict[str, Any]] = []
        fatal: str | None = None
        done = False
        while True:
            if run.cancelled:
                session.kill()
                session.wait(timeout=10.0)
                self._finish(run, "cancelled")
                return
            if time.monotonic() > deadline:
                session.kill()
                session.wait(timeout=10.0)
                run.update(error="the benchmark ran past its time budget and was stopped")
                self._finish(run, "failed")
                return
            event = session.poll_event(timeout=1.0)
            if event is None:
                continue
            kind = event.get("event")
            if kind == "bench_ready":
                run.update(
                    startup_seconds=_as_float(event.get("startup_seconds")),
                    tunable=bool(event.get("tunable", True)),
                )
                # What the run is ACTUALLY on, as the runner resolved it: a
                # number measured with the detector on the CPU is about a
                # different machine from the same row on the card.
                placed = event.get("stage_device")
                if isinstance(placed, dict):
                    host = dict(run.data.get("host") or {})
                    host["devices"] = {str(k): str(v) for k, v in placed.items()}
                    run.update(host=host)
                if event.get("pages"):
                    sample_data = dict(run.data.get("sample") or {})
                    sample_data["pages"] = int(event["pages"])
                    run.update(sample=sample_data)
                run.update(
                    progress={
                        "trial": 0,
                        "max_trials": int(event.get("max_trials") or BENCH_MAX_TRIALS),
                        "pages_done": 0,
                        "pages": int(event.get("pages") or sample.pages),
                        "stage_workers": {},
                        "pages_per_second": None,
                    }
                )
            elif kind == "bench_progress":
                progress = dict(run.data.get("progress") or {})
                progress.update(
                    {
                        "trial": event.get("trial"),
                        "pages_done": event.get("pages_done"),
                        "pages": event.get("pages", progress.get("pages")),
                        "stage_workers": event.get("stage_workers") or {},
                        "pages_per_second": event.get("pages_per_second"),
                        # A trial is now however many passes it takes to fill
                        # the window, so a bar that only showed pages of one
                        # pass would run out and stop meaning anything.
                        "pass_index": event.get("pass_index"),
                        "window_seconds": event.get("window_seconds"),
                        "pages_measured": event.get("pages_measured"),
                    }
                )
                progress.setdefault("max_trials", BENCH_MAX_TRIALS)
                run.update(progress=progress)
            elif kind == "bench_trial":
                trial = {key: value for key, value in event.items() if key != "event"}
                # A LOCAL run's trial carries no busy% (the runner does not
                # measure it), so this fills both. A REMOTE run's trial
                # already carries the numbers sampled on the machine that ran
                # it, and those are the only ones that mean anything.
                measured_here = _window_utilization(sampler, spawned, trial)
                trial.update(
                    {k: v for k, v in measured_here.items() if trial.get(k) is None}
                )
                trials.append(trial)
                run.update(trials=list(trials))
            elif kind == "bench_done":
                best = dict(event.get("best") or {})
                winner = next((t for t in trials if t.get("n") == best.get("trial")), None)
                best["stage_workers"] = self._workers_to_apply(
                    run, best, (winner or {}).get("stage_workers")
                )
                best["stage_device"] = self._placement_to_apply(run, best)
                best["queue_capacity"] = self._capacity_to_apply(
                    run, best, (winner or {}).get("queue_capacity")
                )
                # A precision is never a pool: what the recognizer ran at, the
                # mode it was measured for and -- for a balanced/speed mode --
                # its precision trials are kept beside ``best``, never in it.
                ran = event.get("precision") or best.get("precision")
                for stale in ("precision", "precision_auto", "precision_ran", "card_family"):
                    best.pop(stale, None)
                if (
                    run.measured.precision_applies
                    and isinstance(ran, str)
                    and ran in PRECISIONS
                ):
                    run.update(precision=ran)
                    mode = event.get("precision_mode")
                    if isinstance(mode, str) and mode:
                        run.update(precision_mode=mode)
                    tried = event.get("precision_trials")
                    if isinstance(tried, list) and tried:
                        run.update(
                            precision_trials=[dict(t) for t in tried if isinstance(t, dict)],
                            precision_why=str(event.get("precision_why") or ""),
                        )
                best["same_as_spec"] = self._same_as_spec(run, best)
                # The headline shows the winning trial's numbers, so it shows
                # that trial's utilization too -- means over a DIFFERENT
                # trial's window would be a number about another pipeline.
                for trial in trials:
                    if trial.get("n") == best.get("trial"):
                        best.setdefault("gpu_busy_pct", trial.get("gpu_busy_pct"))
                        best.setdefault("cpu_busy_pct", trial.get("cpu_busy_pct"))
                        break
                baseline = dict(event.get("baseline") or {})
                run.update(
                    baseline=baseline or None,
                    best=best,
                    peak_rss_mb=event.get("peak_rss_mb"),
                    peak_vram_mb=event.get("peak_vram_mb"),
                    estimates=self._estimates(run, _as_float(best.get("pages_per_second"))),
                    progress=None,
                )
                done = True
            elif kind == "fatal":
                fatal = str(event.get("error") or "the runner reported a fatal error")
            elif kind == "exit":
                code = event.get("returncode")
                if not done:
                    detail = fatal or session.stderr_tail()
                    run.update(
                        error=(
                            f"the {run.measured.name} benchmark ended"
                            + (f" with status {code}" if code not in (None, 0) else "")
                            + (f": {detail}" if detail else " before it produced a result")
                        )
                    )
                    self._finish(run, "failed")
                    return
                self._finish(run, "done")
                return

    def _run_monolithic(
        self, run: _BenchRun, processor: OCRProcessor, sample: BenchSample
    ) -> None:
        """The mokuro CLI over the sample pages, timed BY WHAT IT WROTE.

        One trial, no stages, ``tunable: false``: mokuro detects and
        recognizes behind its own command line, so there is no pipeline to
        size and no widening to offer.

        ADDENDUM 9: the number is read off the per-page JSON files the fork
        writes as each page comes out of its pipeline -- the CLI path's
        emission timestamps -- and NOT off a stopwatch around the process.
        The stopwatch is what reported 1.0 pages/s for an engine measured at
        ~13 pages/s served: 28 pages / 28.01 s, of which more than half was
        the interpreter, the torch import, the model load and the worker
        spawn.

        The pages are LINKED INTO THE WORKSPACE first, because mokuro writes
        its ``_ocr`` cache beside the volume it is given: run over the sample
        directory directly, those files land next to the sample instead of in
        this run's workspace, where neither the progress poll nor this clock
        would ever see them.
        """
        run.update(tunable=False, startup_seconds=None)
        workspace = Path(
            tempfile.mkdtemp(prefix="bench-mokuro-", dir=str(self.storage_path / ".processing"))
        )
        sampler = self._sampler(utilization_device(run.measured.pools)).start()
        try:
            run.update(
                progress={
                    "trial": 1,
                    "max_trials": 1,
                    "pages_done": 0,
                    "pages": sample.pages,
                    "stage_workers": {},
                    "pages_per_second": None,
                }
            )
            pages_dir = _link_pages(sample.directory, workspace)
            # Both clocks, taken together, so file mtimes (wall) can be read
            # on the sampler's clock (monotonic).
            start_wall = time.time()
            start = time.monotonic()
            result = processor._run_mokuro(
                pages_dir,
                workspace,
                total_images=sample.pages,
                generation=run.measured,
            )
            wall_seconds = max(time.monotonic() - start, 1e-6)
            if run.cancelled:
                self._finish(run, "cancelled")
                return
            if not result.ok:
                run.update(error=result.error or "the mokuro run failed")
                self._finish(run, "failed")
                return
            emissions = ocr_json_emissions(workspace)
            if len(emissions) < 2:
                run.update(
                    error=(
                        "the mokuro run wrote no per-page results to time it by — "
                        "nothing to measure"
                    )
                )
                self._finish(run, "failed")
                return
            window = emission_window(emissions, passes=1)
            pps = window["pages_per_second"]
            # "First page after X s", the one figure the model load is allowed
            # to appear in -- and it is never added to anything below.
            run.update(startup_seconds=round(max(0.0, emissions[0] - start_wall), 3))
            offset = start - start_wall
            utilization = sampler.means(
                window["first_emission_at"] + offset, window["last_emission_at"] + offset
            )
            trial = {
                "n": 1,
                "note": "single run",
                "stage_workers": {},
                "queue_capacity": {},
                "seconds": round(wall_seconds, 2),
                "pages_per_second": round(pps, 4),
                "window_seconds": round(window["window_seconds"], 3),
                "pages_measured": window["pages_measured"],
                "passes": 1,
                "short_window": window["short_window"],
                "accepted": True,
                "verdict": None,
                "bottleneck": None,
                "stages": [],
                "queues": [],
                **utilization,
            }
            summary = {
                "pages_per_second": round(pps, 4),
                "seconds_per_page": round(1.0 / pps, 4) if pps > 0 else None,
                "window_seconds": round(window["window_seconds"], 3),
                "pages_measured": window["pages_measured"],
                "passes": 1,
                "short_window": window["short_window"],
            }
            run.update(
                trials=[trial],
                baseline=dict(summary),
                best={
                    "trial": 1,
                    "stage_workers": {},
                    "queue_capacity": {},
                    "speedup": 1.0,
                    "same_as_spec": True,
                    **summary,
                    **utilization,
                },
                estimates=self._estimates(run, pps),
                progress=None,
            )
            self._finish(run, "done")
        finally:
            sampler.stop()
            shutil.rmtree(workspace, ignore_errors=True)


def _as_float(value: Any) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)
