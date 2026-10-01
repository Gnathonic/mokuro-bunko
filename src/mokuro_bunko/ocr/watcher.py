"""Inbox folder watcher for OCR processing.

Monitors the inbox directory for new manga files and triggers processing.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import shutil
import threading
import time
import zipfile
from collections.abc import Callable, Collection, Mapping, Sequence
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import TYPE_CHECKING, Any, cast

from mokuro_bunko.logging_setup import get_ocr_log_dir
from mokuro_bunko.ocr.congestion import CongestionHistory, build_record, summarize_event_stats
from mokuro_bunko.ocr.devices import cached_catalog
from mokuro_bunko.ocr.engine_runner import (
    DEVICE_AUTO,
    DEVICE_CPU,
    STAGE_ENGINE,
    STAGE_MOKURO,
    resolve_device,
    stage_needs_ort_gpu,
)
from mokuro_bunko.ocr.eta import (
    EFT_LOOKAHEAD,
    SOURCE_BENCH,
    EftLane,
    EftLeft,
    QueuePlan,
    RateEstimate,
    RateModel,
    StartupEstimate,
    earliest_finish_claim,
    plan_queue,
)
from mokuro_bunko.ocr.generations import (
    GenerationPools,
    GenerationSpec,
    default_generations,
    enabled_generations,
    generation_by_id,
    local_environment_problem,
)
from mokuro_bunko.ocr.job_order import order_jobs
from mokuro_bunko.ocr.provenance import (
    ProvenanceRecorder,
    SidecarFacts,
    read_sidecar_facts,
    runner_build,
)
from mokuro_bunko.ocr.remote.profiles import (
    LOCAL_PROFILE,
    ProcessorProfiles,
    machine_pools,
    runner_pools,
)
from mokuro_bunko.ocr.remote.protocol import EVENTS_OPEN_SECONDS, EVENTS_SILENCE_SECONDS
from mokuro_bunko.ocr.session import OcrSession, SessionVolume
from mokuro_bunko.queue.state import QueueStateVersion

if TYPE_CHECKING:
    from mokuro_bunko.database import Database
    from mokuro_bunko.ocr.processor import OcrFailure, OCRProcessor
    from mokuro_bunko.ocr.remote.registry import ProcessorEntry, ProcessorRegistry

try:
    from watchdog.events import FileSystemEvent, FileSystemEventHandler
    from watchdog.observers import Observer

    WATCHDOG_AVAILABLE = True
except ImportError:
    WATCHDOG_AVAILABLE = False
    Observer = None  # type: ignore
    FileSystemEventHandler = object  # type: ignore
    FileSystemEvent = None  # type: ignore


logger = logging.getLogger(__name__)

# How long a slot with nothing to claim waits for another slot to free a
# volume. Only a safety net: a finishing job wakes every waiter, and a slot
# that missed a wakeup notices from the queue generation and retries at once.
SLOT_IDLE_WAIT_SECONDS = 5.0

# A volume left to a machine that finishes it sooner (`_eft_left`) goes back
# to whoever asks if that machine has not claimed it this long after it was
# predicted to start it: a prediction is not a promise, and a volume must
# never wait on a machine that is not coming.
EFT_CLAIM_GRACE_SECONDS = 15.0
# MOKURO_EFT_TRACE=1: log every earliest-finish decision (who asked, what it
# took, what it left to whom and until when, or why it fell back to
# first-come). Diagnostics only; off by default.
EFT_TRACE = os.environ.get("MOKURO_EFT_TRACE", "") not in ("", "0")

# How many volumes a session holds submitted-but-unfinished. Two, so the
# runner's feeder always has the NEXT archive to roll into as the last page
# of the current one leaves the pipeline -- a session whose lookahead is one
# stalls for a claim (a library walk) at every volume boundary, which is the
# gap sessions exist to remove. More than two buys nothing (the feeder reads
# one page ahead, not one volume ahead) and makes pre-emption dearer: a
# claimed volume is one an earlier row has to wait out.
SESSION_LOOKAHEAD = 2

# A session that has said nothing at all for this long is wedged. The runner
# reports `stats` about every 2 s while it works and a `page` per page, so
# ten minutes of silence is not a slow volume; it is a pipeline that stopped.
SESSION_WEDGE_SECONDS = 600.0

# How often a scan with processors attached looks for one that has just
# logged in, so it is given slots in the scan already running rather than in
# the next one. Also the longest a scan waits to notice its slots are done.
SLOT_SUPERVISE_SECONDS = 1.0

# Whose hardware a slot is when it is this machine's own.
LOCAL_SLOT = "local"

# How often a waiting slot wakes to re-check pre-emption, the queue hold and
# the stop flag while its session is busy.
SESSION_POLL_SECONDS = 1.0
# The queue page's pending list is cached for at least this many of its own
# walks (`OCRWorker._queue_max_age`).
QUEUE_CACHE_WALK_FACTOR = 4.0
# The library walk every claim and the queue page start from -- which jobs
# are owed at all -- is shared for this many of its own durations
# (`OCRWorker._walked_candidates`): about a minute where a walk takes 7 s,
# not at all where it takes milliseconds.
CANDIDATE_WALK_FACTOR = 8.0
# A walk faster than this is not shared at all: caching it saves nothing, and
# a small library then sees a file written straight to the disk at once.
CANDIDATE_WALK_CACHE_MIN_SECONDS = 0.1
# Per-archive memo tables (`_job_key`, `_rel_library_path`, `_archive_pages`):
# room for a large library, cleared whole if it ever outgrows them.
PATH_KEY_CACHE_MAX = 200_000

# Sessions of one generation that may die without completing a volume before
# that row is given up on for the rest of the scan. Two: one death is a
# crash, two in a row is a broken environment, and retrying a broken
# environment for a library of volumes burns the whole scan on model loads.
SESSION_CRASH_LIMIT = 2

# CPU pressure (PSI "some": the share of the time some task on the host was
# waiting for a CPU) at or above which a volume ran on a BUSY host, and its
# speed is not learned as that machine's (perf diagnosis F5). Measured on the
# desktop: 84-91 % beside three CPU jobs, when every row ran at a third of
# its night rate or worse; 35-43 % with one left, when paddle was back at its
# night rate. The line sits between the two.
CONTENDED_CPU_PRESSURE = 0.6

# The share of the host's CPU that OTHER processes -- not an OCR runner, not
# anything under one -- used over a volume's window (the runner's
# ``other_cpu``), at or above which the volume also ran on a busy host. The
# pressure above stopped seeing CPU neighbours once a recognizer on a card
# keeps torch to one thread: measured on tower, a neighbour on half its CPUs
# read ~0.01-0.015 of pressure after that cap (0.29 before), while it still
# cost -14% (-73% before). Half the host: a desktop's own browser and
# compositor are a few percent, so only a real CPU neighbour crosses it.
CONTENDED_OTHER_CPU = 0.5
# The event keys either reading arrives under.
BUSY_SIGNALS: tuple[str, ...] = ("cpu_pressure", "other_cpu")

# A processor's download breaker (design section 6.4): this many returned
# claims IN A ROW (every class but `changed`, which is about the file) hold
# that processor's slots for DOWNLOAD_BREAKER_HOLD seconds, doubling on each
# re-open up to DOWNLOAD_BREAKER_MAX_HOLD. A timed hold rather than "for the
# scan", because a scan drains the queue to exhaustion -- hours on a backlog
# -- and a download path that recovers in minutes must not idle a machine
# for hours.
DOWNLOAD_BREAKER_RETURNS = 3
DOWNLOAD_BREAKER_HOLD = 600.0
DOWNLOAD_BREAKER_MAX_HOLD = 3600.0

# Counted returns of ONE job, across scans, that record it as "download
# failed" (design section 6.5): what stops a file that downloads fine for
# nobody from being retried every scan forever.
DOWNLOAD_RETURN_LIMIT = 3

# The longest the library's own read of a returned archive may take before
# its disk is itself called the problem (design section 6.2).
OWN_COPY_READ_SECONDS = 60.0


def read_own_copy(path: Path, timeout: float = OWN_COPY_READ_SECONDS) -> str | None:
    """Read a whole file sequentially. None if it reads; else why not.

    What a processor cannot tell from the other end of a socket: a bad
    sector (every resume dies at the same byte) or a file the library itself
    cannot open (its server answers 500). Runs on a short-lived thread so a
    disk that hangs costs ``timeout``, not the session loop.
    """
    outcome: list[str | None] = []

    def read() -> None:
        at = 0
        try:
            with path.open("rb") as handle:
                while True:
                    chunk = handle.read(1 << 20)
                    if not chunk:
                        break
                    at += len(chunk)
        except OSError as e:
            outcome.append(f"{e} at byte {at:,}")
            return
        outcome.append(None)

    reader = threading.Thread(target=read, name="own-copy-read", daemon=True)
    reader.start()
    reader.join(timeout)
    if reader.is_alive() or not outcome:
        return f"the read did not finish in {timeout:.0f} s"
    return outcome[0]


#: `release_ocr_job`'s reason for a result whose archive went away (the one
#: log line such a discard gets).
_DISCARDED = "its archive was deleted or replaced while it ran; the result was discarded"


def _job_identity(entry: Mapping[str, Any]) -> tuple[Any, Any, Any]:
    """(series, volume, generation id): one job, wherever it is listed."""
    return (entry.get("series"), entry.get("volume"), entry.get("generation_id"))


@dataclass
class _DownloadBreaker:
    """One processor registration's run of archive downloads it gave back."""

    label: str
    consecutive: int = 0
    # Whether this registration has delivered an archive at all: until it
    # has, its path is unproven and a return says nothing about the JOB.
    proven: bool = False
    hold: float = DOWNLOAD_BREAKER_HOLD
    open_until: float = 0.0
    last_error: str = ""

    def is_open(self, now: float) -> bool:
        return now < self.open_until


@dataclass
class _StartBackoff:
    """A row whose runner keeps failing to START on one machine.

    Perf diagnosis F9: a generation that cannot start on a machine (an
    onnxruntime with no GPU provider asked for `cuda:0`) was retried every
    scan with no memory across scans -- 787 failed sessions in 2 h 21 m. The
    per-scan strike rule stops it for the rest of a scan; this spaces the
    attempts ACROSS scans, the way a failed volume's retries are spaced.
    ``signature`` is the row as that machine would run it (and which
    registration of it): change either and the next attempt is at once.
    """

    failures: int
    until: float
    error: str
    signature: str
    name: str


@dataclass
class _Returns:
    """A job's counted download returns, across scans (design section 6.5)."""

    count: int = 0
    machines: list[str] = field(default_factory=list)
    klass: str = ""
    error: str = ""
    machine: str = ""
    at: float = 0.0
    stamp: tuple[int, int] | None = None


def _as_int(value: Any) -> int | None:
    """``value`` as a whole number, or None for anything that is not one.

    Protocol numbers are parsed defensively: they come from a subprocess
    that may be a newer or older runner, and a progress readout must never
    be why an otherwise finished volume raises.
    """
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return int(value)


def _as_float(value: Any) -> float | None:
    """``value`` as a real number, or None. Same defensiveness as `_as_int`."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


@dataclass
class _SessionClock:
    """When one session began paying its startup, and when it finished.

    Shared by every volume in flight in that session, because the model load
    is paid ONCE for the session and not once per volume (Addendum 2). A
    volume that has emitted no page yet is "starting" only for as much of
    that one cost as is left.
    """

    started_at: float = field(default_factory=time.time)
    ready_at: float | None = None
    # Volumes this session has finished. The FIRST one pays for the pipeline
    # filling behind it -- a session cost, already charged once as `startup`
    # -- so it is kept out of the per-volume latency fit.
    completed: int = 0


@dataclass
class _SessionJob:
    """One volume submitted to a session and not yet reported on."""

    job: tuple[Path, str]
    # The row as it was AT SUBMIT: the output path and the log path in
    # `volume` were fixed from its name, so the collector must use the same
    # one however the settings have moved since.
    generation: GenerationSpec
    volume: SessionVolume
    started_at: float = field(default_factory=time.monotonic)
    total_pages: int | None = None
    done_pages: int = 0
    # Which slot is feeding this volume, and the session clock it shares with
    # the other volumes in flight there. Both ride on the progress card so a
    # request thread can group a session's volumes onto one lane and charge
    # its startup once.
    slot: int = 0
    clock: _SessionClock = field(default_factory=_SessionClock)
    # Epoch of this volume's FIRST page event. Its rate is measured from
    # here and from nowhere else (ADDENDUM 9).
    first_page_at: float | None = None
    # The slot that CLAIMED this volume. A processor that disconnects gives
    # its claims back at once, so by the time this session reports on the
    # volume it may already be somebody else's: every outcome is settled
    # through this slot, and an outcome for a claim it no longer holds is
    # dropped (`OCRWorker._take_for_settling`). None for a caller that
    # tracks no ownership.
    owner: _OcrSlot | None = None
    # Whose hardware is running it: LOCAL_SLOT, or the processor's name.
    # What its numbers are filed under -- a 4090's pages a second are not
    # this machine's, and must never move this machine's rate.
    hardware: str = LOCAL_SLOT
    # Whether the RUNNER has this volume: at submit for a local session; for
    # a remote one, once its processor says `fetch {state: ready}` (its
    # verified archive went down the runner's pipe) or the runner says
    # `volume_started`. Only a delivered volume can be blamed when a runner
    # dies (design section 6.3) -- one still downloading never met it.
    delivered: bool = False

    @property
    def rate_key(self) -> str:
        """The `RateModel` key this volume's numbers go under.

        The row's id on this machine, and ``<id>@<processor>`` on another:
        one model, but never one machine's evidence blended into another's
        (see `OCRWorker._rate_key`).
        """
        return OCRWorker._rate_key(self.generation.id, self.hardware)


@dataclass
class _OcrSlot:
    """One concurrent worker slot: its own processor, and the job it holds.

    A slot owns its `OCRProcessor` because the processor's `_active_process`,
    `_cancel_requested` and `last_failure` are single-valued. Sharing one
    across concurrent jobs would let a settings change kill another slot's
    subprocess, and let one job's failure reason be recorded against
    another's volume.
    """

    index: int
    processor: OCRProcessor
    # The job this slot is running as (cbz path, generation id), or None.
    # Written by the slot's own thread inside `OCRWorker.claim_next` /
    # `_run_ocr_job`, both under the worker's lock, so other threads may
    # read it under that lock.
    job: tuple[Path, str] | None = None
    # The GENERATION the job was claimed with, frozen at claim time. A
    # settings change that lands mid-run must not move the file this job is
    # writing (see `OCRProcessor.process_library_ocr`). For a slot running a
    # SESSION this is the row the session was opened for; the session's own
    # volumes each carry the row they were submitted with.
    generation: GenerationSpec | None = None
    # The runner this slot is keeping open, when it is running a session
    # rather than a one-volume subprocess. Read under the worker's lock by
    # `apply_settings`, which is the only other thread that touches it.
    session: OcrSession | None = None
    # Whose hardware this slot is: "local", or a processor id. The KEY a
    # disconnect requeues by -- a claim belongs to the slot that took it,
    # which may be a slot whose session has not opened yet.
    processor_id: str = LOCAL_SLOT
    # True when this slot's last claim found work but left all of it to
    # machines that finish it sooner (`OCRWorker._eft_left`): the slot waits
    # for them rather than leaving the scan, so it is still here if one of
    # them never comes for it.
    waiting_for_faster: bool = False
    # True while this slot's loop (`_run_ocr_slot`) is running. A slot whose
    # loop has ended claims nothing until it is started again, so it is no
    # lane to leave a volume to (`_eft_lanes`); `_supervise_slots` starts it
    # again once the queue has moved on from ``exited_generation``.
    running: bool = False
    exited_generation: int | None = None


class InboxWatcher:
    """Watches an inbox directory for new files to process."""

    def __init__(
        self,
        inbox_path: Path,
        on_new_file: Callable[[Path], None],
        settle_time: float = 1.0,
        poll_interval: float = 5.0,
        process_existing: bool = False,
    ) -> None:
        """Initialize the inbox watcher.

        Args:
            inbox_path: Path to the inbox directory to watch.
            on_new_file: Callback called when a new file is ready.
            settle_time: Time to wait after file creation before processing.
                        This ensures files are fully written.
            poll_interval: How often to poll for changes (fallback mode).
            process_existing: Whether to process files already in inbox on startup.
        """
        self.inbox_path = inbox_path
        self.on_new_file = on_new_file
        self.settle_time = settle_time
        self.poll_interval = poll_interval
        self.process_existing = process_existing

        self._running = False
        self._stop_event = threading.Event()
        self._pending_files: dict[Path, float] = {}
        self._processed_files: set[Path] = set()
        self._lock = threading.Lock()

        # Use watchdog if available, otherwise fall back to polling
        self._use_watchdog = WATCHDOG_AVAILABLE
        self._observer: Observer | None = None  # type: ignore

    def start(self) -> None:
        """Start watching the inbox directory.

        This method blocks until stop() is called.
        """
        if not self.inbox_path.exists():
            self.inbox_path.mkdir(parents=True, exist_ok=True)

        self._running = True
        self._stop_event.clear()

        # Process existing files if requested
        if self.process_existing:
            self._scan_existing_files()

        if self._use_watchdog:
            self._start_watchdog()
        else:
            self._start_polling()

    def stop(self) -> None:
        """Stop watching the inbox directory."""
        self._running = False
        self._stop_event.set()

        if self._observer is not None:
            self._observer.stop()
            self._observer.join(timeout=5.0)
            self._observer = None

    def _scan_existing_files(self) -> None:
        """Scan inbox for existing files and queue them for processing."""
        if not self.inbox_path.exists():
            return

        for item in self.inbox_path.iterdir():
            if item.name.startswith("."):
                continue
            with self._lock:
                if item not in self._processed_files:
                    self._pending_files[item] = time.time()

    def _start_watchdog(self) -> None:
        """Start watching using watchdog library."""
        handler = _InboxEventHandler(self)
        self._observer = Observer()
        self._observer.schedule(handler, str(self.inbox_path), recursive=False)
        self._observer.start()

        # Process pending files loop
        while not self._stop_event.is_set():
            self._process_pending()
            self._stop_event.wait(timeout=0.1)

    def _start_polling(self) -> None:
        """Start watching using polling fallback."""
        known_files: set[Path] = set()

        while not self._stop_event.is_set():
            try:
                current_files = set(self.inbox_path.iterdir())

                # Find new files
                new_files = current_files - known_files
                for path in new_files:
                    if not path.name.startswith("."):
                        self._on_file_created(path)

                known_files = current_files

            except Exception as e:
                logger.error(f"Error polling inbox: {e}")

            # Process pending files
            self._process_pending()

            self._stop_event.wait(timeout=self.poll_interval)

    def _on_file_created(self, path: Path) -> None:
        """Handle a new file being created.

        Args:
            path: Path to the created file.
        """
        with self._lock:
            if path not in self._processed_files:
                self._pending_files[path] = time.time()
                logger.debug(f"File detected: {path}")

    def _on_file_modified(self, path: Path) -> None:
        """Handle a file being modified.

        Args:
            path: Path to the modified file.
        """
        with self._lock:
            if path in self._pending_files:
                # Reset settle timer
                self._pending_files[path] = time.time()

    def _process_pending(self) -> None:
        """Process files that have settled."""
        current_time = time.time()
        ready_files: list[Path] = []

        with self._lock:
            # Find files that have settled
            for path, created_time in list(self._pending_files.items()):
                if current_time - created_time >= self.settle_time:
                    if path.exists():
                        ready_files.append(path)
                        self._processed_files.add(path)
                    del self._pending_files[path]

        # Process ready files
        for path in ready_files:
            try:
                logger.info(f"Processing file: {path}")
                self.on_new_file(path)
            except Exception as e:
                logger.error(f"Error processing {path}: {e}")


if WATCHDOG_AVAILABLE:

    class _InboxEventHandler(FileSystemEventHandler):
        """Watchdog event handler for inbox directory."""

        def __init__(self, watcher: InboxWatcher) -> None:
            super().__init__()
            self.watcher = watcher

        def on_created(self, event: FileSystemEvent) -> None:
            """Handle file creation event."""
            if event.is_directory:
                return

            path = Path(os.fsdecode(event.src_path))
            if not path.name.startswith("."):
                self.watcher._on_file_created(path)

        def on_modified(self, event: FileSystemEvent) -> None:
            """Handle file modification event."""
            if event.is_directory:
                return

            path = Path(os.fsdecode(event.src_path))
            if not path.name.startswith("."):
                self.watcher._on_file_modified(path)

        def on_moved(self, event: FileSystemEvent) -> None:
            """Handle file move event (rename)."""
            if event.is_directory:
                return

            # Treat as new file at destination
            if hasattr(event, "dest_path"):
                path = Path(os.fsdecode(event.dest_path))
                if not path.name.startswith("."):
                    self.watcher._on_file_created(path)


def _precision_refused(error: str | None) -> bool:
    """Did a runner refuse the row's forced precision (its device cannot run it)?"""
    from mokuro_bunko.ocr.engine_runner import PRECISION_REFUSAL

    return bool(error) and PRECISION_REFUSAL in str(error)


class OCRWorker:
    """Background worker that combines watcher and processor."""

    def __init__(
        self,
        storage_path: Path,
        poll_interval: float = 30.0,
        status_callback: Callable[[str], None] | None = None,
        thumbnails_only: bool = False,
        generations: Sequence[GenerationSpec] | None = None,
        engines_python_path: Path | None = None,
        concurrency: int = 1,
        missing_pages_lookup: Callable[[Path], int] | None = None,
        page_count_lookup: Callable[[Path], int | None] | None = None,
        sessions: bool = True,
        remote: ProcessorRegistry | None = None,
        local_processing: bool = True,
        autobench: bool = True,
        local_unavailable: Mapping[str, str] | None = None,
        database: Database | None = None,
    ) -> None:
        """Initialize the OCR worker.

        Args:
            storage_path: Base storage path.
            poll_interval: How often to poll for new files.
            status_callback: Optional callback for status messages.
            thumbnails_only: Run only the cover-generation loop. Cover
                sidecars are part of the metadata contract and must exist even
                on servers whose OCR backend is `skip`; generating them needs
                Pillow, never the mokuro environment.
            generations: The configured OCR recipes, IN RUN ORDER (default:
                one mokuro row). The queue runs them in that order: every
                volume gets the first row's sidecar before the second row
                starts, and the first row is the one that keeps normal OS
                priority.
            engines_python_path: Python of the engines environment; auto-
                detected when None.
            concurrency: How many jobs run at once (`ocr.concurrency`), each
                in its own slot with its own processor and subprocess. 1 is
                one job at a time and creates no threads of its own, which
                is what this worker has always done. Concurrent jobs are
                always on different volumes: see `claim_next`.
            sessions: Keep ONE runner open per generation and stream volumes
                through it (`ocr.sessions`, the default). False falls back to
                one subprocess per volume, which is what this worker did
                before sessions existed. Monolithic rows (the mokuro CLI) run
                one volume per invocation either way. A PROCESSOR's slot runs
                a session whatever this says: it has no per-volume road.
            remote: The connected processors (`ProcessorRegistry`), whose
                session capacity joins this machine's slots while they are
                logged in. None: this machine is the only hardware.
            local_processing: Whether THIS machine runs OCR at all
                (`ocr.local_processing`). Off, it has no slots of its own and
                the queue holds until a processor logs in.
            autobench: Benchmark a (row, processor) pair that has never been
                measured on that machine before offering it volumes
                (`ocr.autobench`, spec section 4).
            local_unavailable: What THIS server could not install
                (`generations.local_environment_problem`): the rows needing
                it stay in the queue for processors, and this server's own
                slots are never offered them.
            database: The app's database, where every sidecar written is
                recorded with the machine that wrote it and every result
                delivered is audited, written or rejected
                (`ocr.provenance`). None records nothing.
        """
        from mokuro_bunko.ocr.processor import OCRProcessor

        self.storage_path = storage_path
        self.poll_interval = poll_interval
        self.status_callback = status_callback or (lambda msg: None)
        self.thumbnails_only = thumbnails_only
        self.provenance: ProvenanceRecorder | None = (
            ProvenanceRecorder(database, Path(storage_path) / "library")
            if database is not None
            else None
        )

        self.processor = OCRProcessor(
            storage_path=storage_path,
            status_callback=self.status_callback,
            generations=generations,
            engines_python_path=engines_python_path,
            concurrency=concurrency,
        )
        self.generations: list[GenerationSpec] = list(self.processor.generations)
        self.concurrency = max(1, int(concurrency))
        self.sessions_enabled = bool(sessions)
        # Remote hardware, if any is connected, and whether THIS machine
        # processes at all. A weak library server sets local_processing off
        # and does nothing until a processor logs in.
        self.remote = remote
        self.local_processing = bool(local_processing)
        # This server's own "catalog" gaps: environments it could not
        # install. Replaced whole by `apply_settings`.
        self.local_unavailable: dict[str, str] = dict(local_unavailable or {})
        # When this worker started, for the "no processor connected since"
        # message on the queue page.
        self._started_at = time.time()
        # The slots the running scan is using -- this machine's and every
        # connected processor's -- so a benchmark or a settings change can
        # reach a remote session as well as a local one. Empty between scans.
        self._active_slots: list[_OcrSlot] = []
        # Claim -> the slot that took it, for every claim taken BY a slot.
        # A disconnect returns its processor's claims by this map, at once
        # and whatever state they are in, so the old slot's thread may still
        # try to settle one afterwards: `_take_for_settling` is what makes
        # that late settlement a no-op instead of a second outcome for a
        # volume somebody else now holds.
        self._claim_owner: dict[tuple[Path, str], _OcrSlot] = {}
        # Claims whose owning slot is recording an outcome right now (a
        # sidecar being installed, a failure being written). A disconnect
        # leaves these to that slot: taking one back mid-install would let
        # the next owner write the same file beside it.
        self._settling: set[tuple[Path, str]] = set()
        # Whether the last scan found the queue held for want of hardware,
        # so the log says so once rather than every poll interval.
        self._hold_logged = False
        # The metadata pass's "this volume was uploaded short of pages" flag,
        # asked per candidate; every slot's processor shares the one lookup.
        self.missing_pages_lookup = missing_pages_lookup
        self.processor.missing_pages_lookup = missing_pages_lookup
        # "How many pages has this archive?", answered from the metadata
        # pass's cache by whoever owns the database. It is what turns a
        # pending job into a number of seconds; without it every queued
        # volume falls back to the median of the ones that do have a count.
        self.page_count_lookup = page_count_lookup
        # How fast each row reads a page and what a session start costs.
        # Owned here because the evidence is: the `volume_done` events of
        # this process, the congestion history on disk, and the saved
        # benchmarks. Every slot's processor is handed the same one.
        self.rates = RateModel(storage_path)
        self.processor.rates = self.rates
        self.processor.run_recorder = self._record_local_run
        # Per-(row, processor) evidence: what each OTHER machine really
        # costs, its pools and its benchmark (`processors/<name>.json`).
        self.profiles = ProcessorProfiles(storage_path)
        # Spec section 4's auto-bench. `bench_service` is bound by the server
        # (a BenchService, or a zero-argument callable returning one) once
        # the admin API can build it; without one nothing is ever asked.
        self.autobench = bool(autobench)
        self.bench_service: Any = None
        # (processor, row id) pairs asked for this scan, pairs whose
        # benchmark could not be had (they then run untuned rather than
        # never), and requests recorded under the lock, fired outside it.
        self._autobench_asked: set[tuple[str, str]] = set()
        self._autobench_failed: set[tuple[str, str]] = set()
        self._autobench_wanted: list[tuple[Any, GenerationSpec]] = []
        # Pairs whose benchmark is queued or running now: never asked for
        # twice, and a scan waits for them rather than ending under them.
        self._autobench_inflight: set[tuple[str, str]] = set()
        # (processor, row id, stale pins) whose profile was set aside for
        # naming a card that machine no longer reports: said once, not per
        # claim (`_remote_pools`).
        # (processor, row, what was pinned): each stale pin is said once.
        self._stale_profiles_logged: set[tuple[str, str, tuple[object, ...]]] = set()

        self.watcher: InboxWatcher | None = None
        self._ocr_thread: threading.Thread | None = None
        self._thumb_thread: threading.Thread | None = None
        self._running = False
        # Set when an archive arrives over WebDAV (`archive_arrived`): the OCR
        # loop's poll wait ends early and it scans now, not a poll later.
        self._wake = threading.Event()
        # (cbz path, generation id) pairs currently being processed. Keyed
        # by the GENERATION throughout: two rows may run one engine, and an
        # engine-keyed set would have one row's run suppress the other's.
        self._inflight_ocr: set[tuple[Path, str]] = set()
        # Jobs already tried in the scan that is running (one try per scan).
        self._attempted_ocr: set[tuple[Path, str]] = set()
        # Jobs cancelled by a settings change: their outcome is not the
        # volume's fault and must not be recorded as a failure with a
        # backoff. Written when the kill is issued, read once by the job.
        self._cancelled_ocr: set[tuple[Path, str]] = set()
        # (size, mtime_ns) of each claimed job's archive AT CLAIM: its result
        # is only written beside the same file (`_archive_still_current`).
        # None when the archive could not be read at claim time.
        self._job_stamps: dict[tuple[Path, str], tuple[int, int] | None] = {}
        # Generation id -> series it served last: where that row's
        # round-robin resumes (see job_order.order_jobs). In memory only;
        # after a restart each row starts again from the first series.
        self._last_served: dict[str, str] = {}
        # (monotonic time, generation, pending_jobs() result): the queue page
        # polls every few seconds and must not rescan the library each time.
        # Bumping the generation drops the cache, including a result that a
        # poll was still computing when the queue changed.
        self._queue_cache: tuple[float, int, list[dict[str, Any]]] | None = None
        # How long the last `pending_jobs` walk took: the cache lives at
        # least QUEUE_CACHE_WALK_FACTOR of it (`_queue_max_age`).
        self._queue_walk_seconds = 0.0
        # The shared library walk: (monotonic time, epoch, owed jobs). Kept
        # current without re-walking -- a finished job leaves it, an arrival
        # joins it, a removal takes its volumes out -- and re-walked when it
        # ages out or the generations change (`_candidate_epoch`).
        self._candidate_walk: tuple[float, int, list[tuple[Path, str]]] | None = None
        self._candidate_epoch = 0
        self._candidate_walk_seconds = 0.0
        self._candidate_walk_lock = threading.Lock()
        self._path_keys: dict[Path, tuple[str, str]] = {}
        self._rel_paths: dict[Path, str] = {}
        self._queue_generation = 0
        # The queue page's state version (`queue.state`): bumped by every
        # change that page shows -- a claim, a page event, a volume done or
        # failed, the pending list moving -- so its status endpoint can answer
        # an unchanged poll with a 304 instead of recomputing. The server
        # swaps in the one `OcrControl` shares with the queue page.
        self.queue_state = QueueStateVersion()
        # Held while a poll computes the list, so that polls arriving on a
        # cold cache wait for that one library walk instead of each starting
        # their own. Never taken by the worker thread: a slow request must
        # not hold up the scan.
        self._queue_compute_lock = threading.Lock()
        # Set by stop(): ends the per-job re-scan loop between jobs.
        self._stop_requested = False
        # Hardware -> how many holders want it quiet: today only a
        # benchmark, whose numbers would be garbage measured beside a
        # running OCR job ON THE SAME MACHINE. Per hardware (LOCAL_SLOT, or a
        # processor's name) because a benchmark on this box says nothing
        # about a 4090 in another room, which keeps working (spec section 3
        # rule 5: the processor the bench is FOR is pre-empted). Held jobs
        # are never killed: claiming simply stops and the sessions close as
        # their accepted volumes finish. Counters, not flags, so two holders
        # cannot un-hold each other.
        self._holds: dict[str, int] = {}
        # (generation id, hardware) pairs given up on for the rest of THIS
        # scan, because two of their sessions in a row died without
        # completing a volume. Per hardware: one processor with a broken
        # environment says nothing about the others, and stopping the row
        # everywhere for it would idle every healthy card. Cleared at the
        # top of every scan: the next one is a fresh chance, and the usual
        # cause (a half-installed environment) is fixed between them.
        self._stopped_generations: set[tuple[str, str]] = set()
        # (generation id, hardware) -> consecutive sessions that died
        # without completing a volume. Reset by any session of that pair
        # that completes one.
        self._session_strikes: dict[tuple[str, str], int] = {}
        # Per processor REGISTRATION: its run of archive downloads given back
        # (design section 6.4). A processor restarted after a fix starts
        # clean; nothing here survives a library restart.
        self._breakers: dict[str, _DownloadBreaker] = {}
        # Per job: its counted download returns across scans (section 6.5),
        # and -- per scan -- the processors that gave it back, which are not
        # offered it again until the next scan (others may take it now).
        self._download_returns: dict[tuple[Path, str], _Returns] = {}
        self._returned_by: dict[tuple[Path, str], set[str]] = {}
        # (job, machine) pairs already logged as left to a warm session, or
        # to a machine that finishes it sooner.
        self._left_logged: set[tuple[tuple[Path, str], str]] = set()
        # job -> when a volume left to a faster machine stops waiting for it
        # (`_eft_left`, `EFT_CLAIM_GRACE_SECONDS`). Under the lock.
        self._eft_deadlines: dict[tuple[Path, str], float] = {}
        # (path, size, mtime) -> image entries in that archive (`_archive_pages`).
        self._archive_pages_cache: dict[tuple[str, int, int], int | None] = {}
        # (generation id, hardware) -> its runner's run of failed STARTS,
        # across scans; cleared by a session that becomes ready, and void
        # once the row as that machine runs it changes (`_StartBackoff`).
        self._start_backoff: dict[tuple[str, str], _StartBackoff] = {}
        # Every session currently open, so shutdown can be sure none is left
        # behind even if its slot thread is wedged.
        self._open_sessions: set[OcrSession] = set()
        # Monotonic counter behind the per-volume op ids a session uses.
        self._session_job_seq = 0
        self._inflight_thumbs: set[Path] = set()
        self._progress_path = self.storage_path / ".ocr-progress.json"
        self._failures_path = self.storage_path / ".ocr-failures.json"
        self._heartbeat_path = self.storage_path / ".ocr-heartbeat"
        # Per-generation congestion history: the last few completed runs of
        # each row, averaged into the admin table's Congestion column.
        self._congestion = CongestionHistory(self.storage_path)
        # Progress of every RUNNING job, keyed by it, in the order the jobs
        # started. One entry with one slot; the progress file keeps the
        # first one at the top level for readers that know only one job.
        self._active_progress: dict[tuple[Path, str], dict[str, Any]] = {}
        # A Condition, not a plain Lock: a slot with nothing to claim waits
        # on it for another slot to free a volume (`_run_ocr_slot`). Every
        # `with self._lock:` block reads the same as before.
        self._lock = threading.Condition()
        # Slot 0's processor IS self.processor: with the default
        # concurrency of 1 there is nothing else, and the inbox path and
        # the thumbnail loop keep using the one they always used. With local
        # processing off there are no local slots at all -- `self.processor`
        # still exists, for the inbox, the covers and every sidecar a
        # processor sends home, but it never runs a queue job.
        self._slots: list[_OcrSlot] = []
        if self.local_processing:
            self._slots.append(self._make_slot(0, self.processor))
            for index in range(1, self.concurrency):
                self._slots.append(self._make_slot(index, self._clone_processor()))

    def _log(self, message: str) -> None:
        """Log a status message."""
        self.status_callback(message)

    def _make_slot(self, index: int, processor: OCRProcessor) -> _OcrSlot:
        """Wrap a processor as a slot and bind its progress to the slot's job.

        Progress arrives from the subprocess poll loop, which knows nothing
        about the queue; the slot knows which job it is running.
        """
        slot = _OcrSlot(index=index, processor=processor)
        processor.rates = self.rates
        processor.run_recorder = self._record_local_run
        processor.progress_callback = lambda data: self._on_progress(slot, data)
        # Every runner this slot starts -- a session or a one-volume run --
        # runs the row as THIS machine should (`_local_run_row`).
        processor.run_row = self._local_run_row
        return slot

    def _clone_processor(self) -> OCRProcessor:
        """A second processor with the primary's exact settings.

        Built from the interpreter paths the primary already resolved, so
        no slot re-probes the environments and no two slots can end up
        running different Pythons.
        """
        from mokuro_bunko.ocr.processor import OCRProcessor

        clone = OCRProcessor(
            storage_path=self.storage_path,
            python_path=self.processor.python_path,
            status_callback=self.status_callback,
            generations=self.processor.generations,
            engines_python_path=self.processor.engines_python_path,
            concurrency=self.concurrency,
        )
        clone.missing_pages_lookup = self.missing_pages_lookup
        clone.rates = self.rates
        clone.run_recorder = self._record_local_run
        return clone

    def apply_settings(
        self,
        generations: Sequence[GenerationSpec],
        poll_interval: float | None = None,
        *,
        local_unavailable: Mapping[str, str] | None = None,
    ) -> None:
        """Change the generations list and poll interval without a restart.

        Takes effect from the next job: the candidate list is recomputed
        after every job, so a removed row stops queueing immediately and an
        added one joins the backlog.

        **What cancels a running job, and what does not.** The comparison is
        by the row's immutable `id`, never by its name or its engine:

        * removed, disabled, or an OUTPUT-affecting field changed (engine,
          detector, patch budget) -> cancel. The subprocess is
          producing a different recipe's output and must not be filed under
          the new one. Such a cancel is NOT the volume's failure and is not
          recorded as one (`_cancelled_ocr`).
        * renamed, made primary, moved up or down the list, or its pool
          sizes changed -> let it finish. The job carries the row it started
          with, so it lands under the name it started under; pool sizes
          provably never change what is written and apply from the next job.

        Each cancel is issued by the job's OWN slot, so the kill reaches the
        subprocess that belongs to it and no other slot's job is touched. A
        slot running a SESSION has several volumes in flight in one process
        and cannot cancel one of them: the whole session is killed, and its
        volumes all go back to the queue unrecorded, because every one of
        them was producing the recipe that just went away.
        """
        new_generations = list(generations) or default_generations()
        with self._lock:
            if local_unavailable is not None:
                self.local_unavailable = dict(local_unavailable)
            configured = self._slots_in_use()
        for slot in configured:
            slot.processor.configure(new_generations)
        if all(slot.processor is not self.processor for slot in configured):
            # Local processing off: the primary runs no slot, but it is still
            # the list every claim, cancel decision and processor slot reads.
            # Left unconfigured, an edit would reach nobody until a restart.
            self.processor.configure(new_generations)
        self.generations = list(self.processor.generations)
        if poll_interval is not None and poll_interval > 0:
            self.poll_interval = float(poll_interval)
        dropped: list[tuple[_OcrSlot, str, str]] = []
        killed: list[tuple[OcrSession, str, str]] = []
        with self._lock:
            # A settings change can make benchmarks stale (a row's precision
            # mode, its recipe) in the middle of a scan. "Already asked this
            # scan" would then keep the re-ask from ever being made, and every
            # machine would skip the row for the rest of the scan -- seen live:
            # generation 2 switched to auto-speed, generation 3 ran instead.
            # And a new configuration is a new chance for a benchmark that
            # failed under the old one, as a restart would be. In-flight
            # benchmarks are left alone: they settle as they would have.
            self._autobench_asked = set()
            self._autobench_failed = set()
            self._bump_queue_generation()
            # The run order and the generations' names are on the page.
            self.queue_state.bump()
            # Decided PER GENERATION and applied to every job of it that is in
            # flight, not per slot: one slot holds one job on the per-volume
            # path, but a slot keeping a runner open for a row holds several,
            # and all of them are producing the recipe that just went away.
            cancelled: dict[str, str] = {}
            # A processor's slots included: its session is producing the
            # recipe that went away just as surely as a local one is.
            slots = self._slots_in_use()
            for row in {
                spec.id: spec
                for spec in (slot.generation for slot in slots)
                if spec is not None
            }.values():
                reason = self._cancel_reason(row)
                if reason is not None:
                    cancelled[row.id] = reason
            for job in self._inflight_ocr:
                if job[1] in cancelled:
                    self._cancelled_ocr.add(job)
            for slot in slots:
                running = slot.generation
                if running is None or running.id not in cancelled:
                    continue
                if slot.session is not None:
                    killed.append((slot.session, running.name, cancelled[running.id]))
                elif slot.job is not None:
                    dropped.append((slot, running.name, cancelled[running.id]))
        for slot, name, reason in dropped:
            if slot.processor.cancel_active():
                self._log(f"Cancelled the running {name} job: {reason}")
        for session, name, reason in killed:
            if session.kill():
                self._log(f"Closed the open {name} session: {reason}")
        self._congestion.prune(row.id for row in self.generations)
        # Which rows a volume owes depends on the rows: the shared walk is
        # re-taken by the next claim.
        self._invalidate_candidates()
        self._prune_failure_records()
        self._log(
            "OCR settings applied (generations, in run order: "
            f"{', '.join(row.name for row in enabled_generations(self.generations))})"
        )

    def _cancel_reason(self, running: GenerationSpec) -> str | None:
        """Why a job of ``running`` must be killed now, or None to let it be."""
        current = generation_by_id(self.generations, running.id)
        if current is None:
            return "the generation was removed from settings"
        if not current.enabled:
            return "the generation was disabled"
        if current.output_affecting() != running.output_affecting():
            return "its engine, detector or patch budget changed"
        return None

    def _on_new_file(self, path: Path) -> None:
        """Handle a new file from the watcher."""
        if self.processor.is_processable(path):
            self._log(f"New file detected: {path.name}")
            self.processor.process(path)
        else:
            self._log(f"Ignoring non-processable file: {path.name}")

    def _bump_queue_generation(
        self, *, claimed: tuple[Path, str] | None = None, keep_cache: bool = False
    ) -> None:
        """The queue changed: drop (or update) the cached pending list.

        Called under the lock. A claim does not drop the cache: it takes the
        claimed job out of it, which is exactly what a recomputation would
        do, so the queue page -- which never walks the library itself --
        shows the right list the moment a job starts. ``keep_cache`` is the
        same for a change that leaves the list as it is (a job finishing).
        """
        cached = self._queue_cache
        self._queue_generation += 1
        if cached is not None and cached[1] == self._queue_generation - 1:
            if claimed is not None:
                series, volume, gen_id = self._job_key(claimed)
                jobs = [
                    entry
                    for entry in cached[2]
                    if (entry.get("series"), entry.get("volume"), entry.get("generation_id"))
                    != (series, volume, gen_id)
                ]
                self._queue_cache = (cached[0], self._queue_generation, jobs)
            elif keep_cache:
                self._queue_cache = (cached[0], self._queue_generation, cached[2])
        # Deliberately NOT a queue-page bump: every scan ticks this twice on an
        # idle queue, and the page showed nothing new each time. What the page
        # shows moves the version where it really changes -- the pending list
        # recomputed to something different (`pending_jobs`), a card or a
        # failure written, a settings change (`apply_settings`).

    def _write_progress(self) -> None:
        """Persist the running jobs for UI/API consumption (lock held).

        The file keeps the shape every reader already knows -- one flat job
        object with `active` -- and adds `jobs`, every running job in the
        order they started. The top level is the first of those, so a reader
        that knows nothing about slots (the catalog page, an older queue
        page) still sees a real running job rather than nothing.

        Written atomically, like the failures file: with several slots
        polling their subprocesses the file is rewritten several times a
        second, and a request thread reading it must never catch it
        half-written.
        """
        try:
            self._write_progress_file()
        finally:
            # Every running card change is one the queue page shows -- bumped
            # AFTER the write, so a poll that sees the new version reads the
            # new file.
            self.queue_state.bump()

    def _write_progress_file(self) -> None:
        if not self._active_progress:
            try:
                if self._progress_path.exists():
                    self._progress_path.unlink()
            except OSError:
                pass
            return
        now = time.time()
        jobs: list[dict[str, Any]] = []
        for entry in self._active_progress.values():
            data = dict(entry)
            data["updated_at"] = now
            jobs.append(data)
        payload = dict(jobs[0])
        payload["jobs"] = jobs
        try:
            tmp = self._progress_path.with_name(self._progress_path.name + ".tmp")
            tmp.write_text(json.dumps(payload), encoding="utf-8")
            os.replace(tmp, self._progress_path)
        except OSError:
            pass

    def _set_active_progress(self, job: tuple[Path, str], data: dict[str, Any]) -> None:
        """Set one running job's progress state."""
        with self._lock:
            entry = self._active_progress.get(job)
            if entry is None:
                entry = {"started_at": time.time()}
                self._active_progress[job] = entry
            entry.update(data)
            entry["active"] = True
            self._write_progress()

    def _set_owned_progress(
        self,
        job: tuple[Path, str],
        data: dict[str, Any],
        owner: _OcrSlot | None,
    ) -> None:
        """`_set_active_progress`, from the slot ``owner`` -- if it still holds the job.

        An update for a claim that slot no longer holds -- returned when its
        processor disconnected -- is dropped, under the same lock the return
        takes (the worker's lock is re-entrant): written after the return
        cleared the card, it would be a card nothing ever clears; written
        over the next owner's, it would be a lie about a volume somebody
        else is running. No owner: written as it always was.
        """
        with self._lock:
            if owner is not None and self._claim_owner.get(job) is not owner:
                return
            self._set_active_progress(job, data)

    def _clear_active_progress(self, job: tuple[Path, str]) -> None:
        """Drop one job's progress state; the other slots keep theirs."""
        with self._lock:
            self._active_progress.pop(job, None)
            self._write_progress()

    def _on_progress(self, slot: _OcrSlot, data: dict[str, Any]) -> None:
        """Receive progress events from one slot's OCR processor."""
        # Read without the lock: a slot's job is only ever written by the
        # slot's own thread, which is the thread emitting this.
        job = slot.job
        if job is None:
            # Not a queue job (an inbox upload through this processor):
            # there is no running job for it to be the progress of.
            return
        # Which slot this is: a lane in the queue prediction, and the only
        # way a request thread can tell two volumes sharing one session from
        # two volumes on two slots.
        data = {**data, "slot": slot.index}
        if data.get("status") == "done":
            self._set_owned_progress(job, data, slot)
            self._clear_active_progress(job)
            return
        if data.get("status") == "error":
            self._set_owned_progress(job, data, slot)
            # Keep last error snapshot briefly so UI can show failure.
            return
        self._set_owned_progress(job, data, slot)

    # --- Persistent failure records -------------------------------------
    #
    # Volumes that fail OCR are recorded in <storage>/.ocr-failures.json so
    # the Queue page can show them (with the reason and log path) and so the
    # scan loop can back off instead of retrying a broken volume forever.

    def _load_failures(self) -> dict[str, dict[str, Any]]:
        """Read the persisted failure records (empty dict when none)."""
        try:
            data = json.loads(self._failures_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return {}
        if not isinstance(data, dict):
            return {}
        return {k: v for k, v in data.items() if isinstance(v, dict)}

    def _save_failures(self, failures: dict[str, dict[str, Any]]) -> None:
        """Atomically persist failure records; delete the file when empty."""
        try:
            self._save_failures_file(failures)
        finally:
            self.queue_state.bump()

    def _save_failures_file(self, failures: dict[str, dict[str, Any]]) -> None:
        try:
            if not failures:
                if self._failures_path.exists():
                    self._failures_path.unlink()
                return
            tmp = self._failures_path.with_name(self._failures_path.name + ".tmp")
            tmp.write_text(json.dumps(failures, ensure_ascii=False, indent=2), encoding="utf-8")
            os.replace(tmp, self._failures_path)
        except OSError as e:
            logger.warning("Could not persist OCR failure records: %s", e)

    def _rel_library_path(self, path: Path) -> str:
        """Best-effort path relative to the library root (memoized like `_job_key`)."""
        known = self._rel_paths.get(path)
        if known is None:
            try:
                known = str(path.relative_to(self.storage_path / "library"))
            except ValueError:
                known = str(path)
            if len(self._rel_paths) >= PATH_KEY_CACHE_MAX:
                self._rel_paths.clear()
            self._rel_paths[path] = known
        return known

    @staticmethod
    def failure_key(rel_cbz: str, generation: GenerationSpec) -> str:
        """Key of a (volume, generation) failure record.

        The PRIMARY row keeps the bare relative path, so records written
        before any of this existed stay valid; every other row is suffixed
        with its NAME.

        Its name and not its id, deliberately. This record is read by a
        person -- it is what `.ocr-failures.json` holds, what the Queue page
        lists and what `mokuro-bunko doctor` counts -- and `Vol 3.cbz@g-4`
        says nothing to anyone. It also fails in the right direction: a
        renamed row re-OCRs the volume under its new name anyway, so a
        record about the file it used to fail to write is exactly the record
        that should stop applying. The stale key is swept by
        `_prune_failure_records` on the next settings change.
        """
        if generation.primary:
            return rel_cbz
        return f"{rel_cbz}@{generation.name}"

    def _prune_failure_records(self) -> None:
        """Drop records of generations that no longer exist under that name.

        A rename leaves `Vol 3.cbz@old-name` behind, and nothing would ever
        look it up again -- but the Queue page lists every record, so it
        would sit there as a failure of a row that is gone.

        Judged by the record's OWN `generation` field, never by splitting its
        key on `@`: a volume may be called `Vol@1.cbz`, and reading the tail
        of that key as a generation name would delete a real failure of the
        primary row -- taking its backoff with it, so a permanently broken
        volume would be retried at full rate after every settings edit. A
        record that names no generation is kept: it cannot be attributed, and
        leaving a stale row on the page is better than silently dropping a
        failure that is real.
        """
        names = {row.name for row in self.generations}
        library = self.storage_path / "library"

        def still_configured(entry: dict[str, Any]) -> bool:
            name = entry.get("generation")
            if not isinstance(name, str) or not name:
                return True
            return name in names

        def archive_still_there(entry: dict[str, Any]) -> bool:
            # A volume deleted while its OCR ran leaves a record of a failure
            # that can never be retried ("could not move the sidecar into the
            # library: no such directory" -- seen on the review server after
            # 32 series were removed mid-run). Judged by the record's own
            # series/volume fields, never by parsing its key; a record that
            # names neither is kept, as above.
            series, volume = entry.get("series"), entry.get("volume")
            if not isinstance(series, str) or not isinstance(volume, str) or not volume:
                return True
            folder = library / series if series else library
            return any((folder / f"{volume}{ext}").is_file() for ext in (".cbz", ".cbr", ".zip", ".rar"))

        with self._lock:
            failures = self._load_failures()
            kept = {
                key: entry
                for key, entry in failures.items()
                if still_configured(entry) and archive_still_there(entry)
            }
            if kept != failures:
                self._save_failures(kept)

    def _record_ocr_failure(
        self,
        path: Path,
        generation: GenerationSpec,
        failure: OcrFailure | None = None,
    ) -> None:
        """Persist a failure for this volume/generation.

        `failure` is the one the SLOT's processor recorded. Each slot has
        its own `last_failure`, so the reason written here is the reason
        this job failed for, not whatever another slot failed with in the
        meantime; the default is the primary processor's, for the callers
        that run a job on it.
        """
        rel_cbz = self._rel_library_path(path)
        try:
            rel_series = str(path.parent.relative_to(self.storage_path / "library"))
        except ValueError:
            rel_series = ""
        if failure is None:
            failure = self.processor.last_failure
        error = failure.error if failure else "unknown error"
        log_file = failure.log_file if failure else None
        key = self.failure_key(rel_cbz, generation)
        with self._lock:
            failures = self._load_failures()
            previous = failures.get(key, {})
            attempts = int(previous.get("attempts", 0)) + 1
            failures[key] = {
                "series": rel_series,
                "volume": path.stem,
                "generation": generation.name,
                "engine": generation.engine,
                "detector": generation.reported_detector,
                "error": error,
                "attempts": attempts,
                "last_attempt_at": time.time(),
                "log_file": log_file,
            }
            self._save_failures(failures)
        delay = int(self._retry_delay_seconds(attempts))
        self._log(
            f"OCR ({generation.name}) failed for {rel_cbz} "
            f"(attempt {attempts}, next retry in ~{delay}s): {error}"
        )

    def _clear_ocr_failure(self, path: Path, generation: GenerationSpec) -> None:
        """Drop the failure record for a volume/generation that succeeded."""
        key = self.failure_key(self._rel_library_path(path), generation)
        with self._lock:
            failures = self._load_failures()
            if key in failures:
                del failures[key]
                self._save_failures(failures)

    def _retry_delay_seconds(self, attempts: int) -> float:
        """Exponential backoff delay before retrying a failed volume."""
        # Clamp the exponent: 4.0 ** 512 overflows a float, and a volume
        # that fails every hour reaches that attempt count in ~3 weeks.
        exponent = min(max(0, attempts - 1), 16)
        return min(self.poll_interval * (4.0 ** exponent), 3600.0)

    def _touch_heartbeat(self) -> None:
        """Record OCR-loop liveness for the health endpoint."""
        try:
            self._heartbeat_path.write_text(str(time.time()), encoding="utf-8")
        except OSError:
            pass

    def generation_order(self) -> list[dict[str, Any]]:
        """The enabled generations in the order the queue runs them.

        Which is their order in the list: rearranging the rows IS how the
        queue is prioritised. The same order decides which running job keeps
        normal OS priority (`OCRProcessor.is_backlog_generation`).
        """
        return [
            {
                "id": row.id,
                "name": row.name,
                "engine": row.engine,
                "detector": row.reported_detector,
            }
            for row in enabled_generations(self.generations)
        ]

    def _generation_rank(self) -> dict[str, int]:
        """Generation id -> its position among the enabled rows (0 runs first)."""
        return {row.id: rank for rank, row in enumerate(enabled_generations(self.generations))}

    def _generation(self, gen_id: str) -> GenerationSpec | None:
        """The configured row with this id, or None once it is gone."""
        return generation_by_id(self.generations, gen_id)

    @staticmethod
    def _replaced_since_failure(path: Path, entry: dict[str, Any]) -> bool:
        """True when the archive changed after the recorded failed attempt."""
        try:
            mtime = path.stat().st_mtime
        except OSError:
            mtime = 0.0
        # 1s epsilon: filesystem timestamps and time.time() don't share
        # sub-second granularity on Windows.
        return mtime > float(entry.get("last_attempt_at", 0.0)) + 1.0

    def _eligible_ocr_jobs(self) -> tuple[list[tuple[Path, str]], list[str]]:
        """Find (CBZ, generation id) jobs that are missing and may run now.

        READ ONLY: the queue page calls this from request threads. Every
        enabled generation is checked per volume, and every one the volume
        lacks may run now -- no layer waits for the primary; only a supplied
        `.mokuro` found short of pages holds the layers back (see
        `OCRProcessor.missing_generations`). Jobs with persisted failure
        records are skipped until their backoff delay has elapsed; one whose
        file was replaced since the last attempt may run at once, and the
        key of its now stale record is returned as the second value for the
        worker to drop (`_drop_failure_records`). Unordered: see
        `_ocr_candidates`.
        """
        library_path = self.storage_path / "library"
        if not library_path.exists():
            return [], []
        candidates = self._walked_candidates()

        failures = self._load_failures()
        if not failures:
            return candidates, []
        now = time.time()
        eligible: list[tuple[Path, str]] = []
        stale_records: list[str] = []
        for path, gen_id in candidates:
            generation = self._generation(gen_id)
            if generation is None:
                continue
            key = self.failure_key(self._rel_library_path(path), generation)
            entry = failures.get(key)
            if entry is None:
                eligible.append((path, gen_id))
            elif self._replaced_since_failure(path, entry):
                # File replaced/updated since the failure: start fresh.
                stale_records.append(key)
                eligible.append((path, gen_id))
            elif now >= float(entry.get("last_attempt_at", 0.0)) + self._retry_delay_seconds(
                int(entry.get("attempts", 1))
            ):
                eligible.append((path, gen_id))
        return eligible, stale_records

    def _walk_candidates(self) -> list[tuple[Path, str]]:
        """Every (CBZ, generation id) the library owes, straight off the disk."""
        library_path = self.storage_path / "library"
        return [
            (p, generation.id)
            for p in library_path.rglob("*.cbz")
            if p.is_file()
            for generation in self.processor.missing_generations(p)
        ]

    def _walked_candidates(self) -> list[tuple[Path, str]]:
        """The owed jobs, from the shared walk while it is current.

        A walk stats every archive and every sidecar and asks the metadata
        cache about each volume: 7 s at 12k volumes on a network share. Every
        slot's claim and every queue-page computation used to take one; with
        eight machines claiming, they ran back to back and starved every
        request of the interpreter. One walk now serves them all, kept
        current in place (`_forget_candidate`, `archive_arrived`,
        `archive_removed`) and re-taken when it is older than
        `CANDIDATE_WALK_FACTOR` of its own walks or the generations changed:
        a walk that costs milliseconds is effectively not cached, so a file
        copied straight onto the disk is seen as soon as it ever was.
        Single-flight: a claim that finds it stale waits for the one walk
        under way.
        """
        with self._lock:
            cached = self._candidate_walk
            epoch = self._candidate_epoch
            walk_seconds = self._candidate_walk_seconds
            max_age = (
                CANDIDATE_WALK_FACTOR * walk_seconds
                if walk_seconds >= CANDIDATE_WALK_CACHE_MIN_SECONDS
                else 0.0
            )
        if cached is not None and cached[1] == epoch and time.monotonic() - cached[0] <= max_age:
            return list(cached[2])
        with self._candidate_walk_lock:
            with self._lock:
                cached = self._candidate_walk
                epoch = self._candidate_epoch
            if cached is not None and cached[1] == epoch and time.monotonic() - cached[0] <= max_age:
                return list(cached[2])
            started = time.monotonic()
            walked = self._walk_candidates()
            with self._lock:
                self._candidate_walk_seconds = time.monotonic() - started
                if self._candidate_epoch == epoch:
                    # Only if nothing invalidated it while it was being taken.
                    self._candidate_walk = (time.monotonic(), epoch, walked)
            return list(walked)

    def _invalidate_candidates(self) -> None:
        """The generations changed: the next claim walks the library again."""
        with self._lock:
            self._candidate_epoch += 1
            self._candidate_walk = None

    def _forget_candidate(self, job: tuple[Path, str]) -> None:
        """This job's sidecar exists now: it is owed no more."""
        with self._lock:
            cached = self._candidate_walk
            if cached is not None and job in cached[2]:
                self._candidate_walk = (cached[0], cached[1], [j for j in cached[2] if j != job])

    def _still_owed(self, job: tuple[Path, str], row: GenerationSpec) -> bool:
        """The claim's last look at the disk: archive there, sidecar not yet.

        The shared walk can be a minute old, and a sidecar can arrive
        by other roads (a reader's upload, a copy); three stats settle it for
        the one job being handed out.
        """
        path = job[0]
        if not path.is_file():
            return False
        plain, gz = row.sidecar_paths(path)
        return not plain.exists() and not gz.exists()

    def _drop_failure_records(self, keys: Collection[str]) -> None:
        """Delete failure records. The WORKER's write: never call it for a request."""
        with self._lock:
            current = self._load_failures()
            for key in keys:
                current.pop(key, None)
            self._save_failures(current)

    def _job_key(self, job: tuple[Path, str]) -> tuple[str, str, str]:
        """(series, volume, generation id) of a job, as `order_jobs` reads it.

        Memoized per archive path: ordering the queue asks it of every job
        several times, and `relative_to` on 12k paths was most of a claim.
        """
        path, gen_id = job
        known = self._path_keys.get(path)
        if known is None:
            try:
                series = path.parent.relative_to(self.storage_path / "library").as_posix()
            except ValueError:
                series = path.parent.as_posix()
            known = (series, path.stem)
            if len(self._path_keys) >= PATH_KEY_CACHE_MAX:
                self._path_keys.clear()
            self._path_keys[path] = known
        return known[0], known[1], gen_id

    def _ocr_candidates(
        self,
        exclude: Collection[tuple[Path, str]] = (),
        *,
        reset_stale_failures: bool = False,
    ) -> list[tuple[Path, str]]:
        """Runnable (CBZ, generation id) jobs in processing order.

        The order is `job_order.order_jobs`: the generations in LIST order
        (every volume gets the first row's layer before the second row
        starts), then round-robin across series, each series in natural
        reading order.
        `exclude` is removed BEFORE ordering, because a job that cannot be
        picked (in flight, already tried this scan) must not hold its series'
        turn in the round.

        `reset_stale_failures` deletes the failure records of archives that
        were replaced since they failed. Only the worker's scan passes it:
        listing the queue for a request must never write.
        """
        eligible, stale_records = self._eligible_ocr_jobs()
        if reset_stale_failures and stale_records:
            self._drop_failure_records(stale_records)
        jobs = [job for job in eligible if job not in exclude]
        with self._lock:
            last_served = dict(self._last_served)
        return order_jobs(
            jobs,
            self._generation_rank(),
            key=self._job_key,
            last_served=last_served,
        )

    def _upcoming_ocr_jobs(self, *, reset_stale_failures: bool = False) -> list[tuple[Path, str]]:
        """What the worker will run from here on, in order.

        The single source for both `claim_next` (which takes from its head)
        and the queue page (`pending_jobs`), so the page cannot disagree
        with the worker. Jobs blocked only by a volume another slot is
        working on ARE listed: they are genuinely pending, just not
        startable this instant, which is why `claim_next` filters them and
        this does not.
        The list is the same either way; `reset_stale_failures` only adds
        the worker's housekeeping write (see `_ocr_candidates`).
        """
        with self._lock:
            unavailable = self._inflight_ocr | self._attempted_ocr
        return self._ocr_candidates(exclude=unavailable, reset_stale_failures=reset_stale_failures)

    def skipped_missing_pages(self) -> list[dict[str, Any]]:
        """Volumes that get no additional OCR layers because they are short of pages.

        ``{"series", "volume", "missing_pages", "generations"}`` per volume, for
        the Queue page: a skip that nobody can see looks like a stuck queue.
        The flag is the metadata pass's own `.mokuro`-against-archive check
        (`OCRProcessor.pages_short`); the fix is to replace the file with a
        whole volume. Read only, request-thread safe.
        """
        library = self.storage_path / "library"
        if self.missing_pages_lookup is None or not library.is_dir():
            return []
        skipped: list[dict[str, Any]] = []
        for cbz in sorted(library.rglob("*.cbz")):
            rows = self.processor.skipped_generations(cbz)
            if not rows:
                continue
            skipped.append(
                {
                    "series": str(cbz.parent.relative_to(library)),
                    "volume": cbz.stem,
                    "missing_pages": self.processor.pages_short(cbz),
                    "generations": [row.name for row in rows],
                }
            )
        return skipped

    def pending_jobs(self, max_age: float = 5.0) -> list[dict[str, Any]]:
        """The queue as the queue page shows it: upcoming jobs, in order.

        Each entry is ``{"series", "volume", "generation", "engine",
        "detector"}``, plus ``"attempts"`` when the job is a retry of a
        recorded failure whose backoff has elapsed. The running job is not included (the progress file describes
        it); jobs still in backoff are not included either, the failure list
        shows those. Cached for `max_age` seconds and dropped whenever a job
        starts or ends, because the page polls far more often than the queue
        changes and every computation walks the library.

        Runs in REQUEST threads, so it is read only (it never resets a
        failure record: that is the scan's write) and single-flight: polls
        that find the cache cold wait for the one computation under way and
        are all served its result.
        """
        cached = self._cached_queue(max_age)
        if cached is not None:
            return cached
        with self._queue_compute_lock:
            # Whoever held the lock before us has most likely just filled it.
            cached = self._cached_queue(max_age)
            if cached is not None:
                return cached
            with self._lock:
                generation = self._queue_generation
            failures = self._load_failures()
            jobs: list[dict[str, Any]] = []
            walk_started = time.monotonic()
            for job in self._upcoming_ocr_jobs():
                entry = self._pending_entry(job, failures)
                if entry is not None:
                    jobs.append(entry)
            with self._lock:
                previous = self._queue_cache
                self._queue_walk_seconds = time.monotonic() - walk_started
                self._queue_cache = (time.monotonic(), generation, jobs)
            if previous is None or previous[2] != jobs:
                # A new upload, a backoff that ran out: the list the page
                # shows moved without any job starting or ending.
                self.queue_state.bump()
            return [dict(entry) for entry in jobs]

    def _pending_entry(
        self, job: tuple[Path, str], failures: Mapping[str, dict[str, Any]]
    ) -> dict[str, Any] | None:
        """One `pending_jobs` entry for ``job``, or None once its row is gone."""
        series, volume, gen_id = self._job_key(job)
        row = self._generation(gen_id)
        if row is None:
            return None
        entry: dict[str, Any] = {
            "series": series,
            "volume": volume,
            "generation": row.name,
            # The row's immutable id and the volume's length are what
            # turn this entry into a finishing time. `pages` is None
            # for a volume the metadata pass has not compiled yet;
            # the prediction then uses the median of the ones it knows
            # and says the number is rough.
            "generation_id": row.id,
            "engine": row.engine,
            "detector": row.reported_detector,
            # The metadata cache's count, else the zip directory's own (the
            # count the claim's earliest-finish walk uses): a volume just
            # uploaded has no compiled count yet, and without one it -- and
            # every item behind it -- went unpriced.
            "pages": self._known_pages(job[0]),
        }
        failure = failures.get(self.failure_key(self._rel_library_path(job[0]), row))
        # A record older than the archive is void (the scan drops it
        # when it gets there): the job is a fresh one, not a retry.
        if failure is not None and not self._replaced_since_failure(job[0], failure):
            entry["attempts"] = int(failure.get("attempts", 1))
        with self._lock:
            returned = self._download_returns.get(job)
            if returned is not None:
                # Given back by a processor that could not download
                # it: unrecorded, and shown so it does not just sit
                # there with no reason (design section 6.7).
                entry["returned"] = {
                    "count": returned.count,
                    "class": returned.klass,
                    "error": returned.error,
                    "machine": returned.machine,
                    "at": returned.at,
                }
        return entry

    def _page_count(self, cbz_path: Path) -> int | None:
        """How many pages this archive has, from the metadata cache only.

        None where nothing current is cached: a volume of unknown length has
        to read as unknown, never as a volume of no pages, or the queue
        prediction would quietly shorten itself by every volume it has not
        compiled yet. Never raises -- an estimate must not be why a status
        poll fails.
        """
        lookup = self.page_count_lookup
        if lookup is None:
            return None
        try:
            pages = lookup(cbz_path)
        except Exception:  # pragma: no cover - a cache read, never fatal
            return None
        return pages if isinstance(pages, int) and pages > 0 else None

    def _archive_pages(self, cbz_path: Path) -> int | None:
        """How many images this archive holds, read from its zip directory.

        For the claim's earliest-finish walk, where `_page_count` is not
        enough: that reads the metadata cache, which is compiled from the
        sidecars -- so the volumes just queued, whose sidecars are missing,
        had no length, the walk could not be priced, and 240 of 282 live
        claims fell back to first-come. The zip's central directory is a few
        milliseconds to read; the answer is kept per (path, size, mtime).
        None for an archive that cannot be read -- an estimate never fails a
        claim.
        """
        try:
            stat = cbz_path.stat()
        except OSError:
            return None
        key = (str(cbz_path), stat.st_size, stat.st_mtime_ns)
        if key in self._archive_pages_cache:
            return self._archive_pages_cache[key]
        from mokuro_bunko.metadata.reader_compat import is_image_extension, is_system_file

        try:
            with zipfile.ZipFile(cbz_path) as archive:
                count = sum(
                    1
                    for name in archive.namelist()
                    if not name.endswith("/")
                    and not is_system_file(name)
                    and is_image_extension(Path(name).suffix.lstrip("."))
                )
        except (OSError, zipfile.BadZipFile, ValueError):
            count = 0
        pages = count if count > 0 else None
        if len(self._archive_pages_cache) > PATH_KEY_CACHE_MAX:
            # Sized for a large library: at 4096 a 12k-volume queue cleared
            # it on every pass and re-read thousands of zips over the network.
            self._archive_pages_cache.clear()
        self._archive_pages_cache[key] = pages
        return pages

    def _known_pages(self, cbz_path: Path) -> int | None:
        """`_page_count`, else the archive's own count (`_archive_pages`)."""
        return self._page_count(cbz_path) or self._archive_pages(cbz_path)

    def queue_plan(
        self,
        running: Sequence[dict[str, Any]],
        pending: Sequence[dict[str, Any]],
        now: float | None = None,
        through: int | None = None,
    ) -> QueuePlan:
        """Price the running jobs and the whole queue, from live state.

        Recomputed on every status poll rather than cached: it is arithmetic
        over a few hundred items, and a finishing time written into a file
        three seconds ago is a finishing time that is three seconds wrong.

        The lanes are the session slots: ``ocr.concurrency`` of this
        machine's, plus each connected processor's ``max_sessions``. A lane
        that moves to a row it is not already serving pays that row's startup
        once -- which is what makes "this row already has a warm session"
        visible as the seconds it really saves.

        Each lane is priced by ITS machine (`_lane_rate`): a processor's
        evidence is filed under ``<row>@<name>`` and never moves this box's
        rate, so pricing every lane by the row alone left a server with no
        local OCR without a single ETA.
        """
        machines = self._lane_machines()
        rate_for, startup_for = self._lane_pricing()
        refusal_for, hold_for = self._lane_precision()
        return plan_queue(
            self._with_known_totals(running),
            pending,
            lane_count=max(1, len(machines)),
            lane_machines=machines,
            rate_for=rate_for,
            startup_for=startup_for,
            now=time.time() if now is None else now,
            startup_every_volume=self._startup_every_volume,
            through=through,
            refusal_for=refusal_for,
            hold_for=hold_for,
        )

    def _lane_precision(
        self,
    ) -> tuple[Callable[[str, str], str | None], Callable[[str], str | None]]:
        """``(refusal_for, hold_for)``: which machine may run a row's precision
        mode, and a held row's reason -- asked once per (row, machine) per plan."""
        from mokuro_bunko.ocr.precision import hold_reason

        answers: dict[tuple[str, str], str | None] = {}

        def refusal_for(generation_id: str, machine: str) -> str | None:
            key = (generation_id, machine)
            if key not in answers:
                row = self._generation(generation_id)
                answers[key] = (
                    None if row is None else self._machine_precision_refusal(machine, row)
                )
            return answers[key]

        def hold_for(generation_id: str) -> str | None:
            row = self._generation(generation_id)
            return None if row is None else hold_reason(row.precision)

        return refusal_for, hold_for

    def _with_known_totals(self, running: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
        """The running jobs, each with a page count to be priced by.

        A card has no ``total_pages`` until its runner announces the volume's
        length; until then the archive's own count (`_known_pages`) stands in,
        rather than leaving the job -- and everything queued behind its lane
        -- without a finishing time.
        """
        library = self.storage_path / "library"
        filled: list[dict[str, Any]] = []
        for job in running:
            entry = dict(job)
            series, volume = entry.get("series"), entry.get("volume")
            if (
                _as_int(entry.get("total_pages")) is None
                and isinstance(series, str)
                and isinstance(volume, str)
            ):
                entry["total_pages"] = self._known_pages(library / series / f"{volume}.cbz")
            filled.append(entry)
        return filled

    def volume_plan(
        self,
        cbz_path: Path,
        series: str,
        volume: str,
        owed: Sequence[GenerationSpec],
        running: Sequence[dict[str, Any]],
        pending: Sequence[dict[str, Any]] | None,
        *,
        max_items: int | None = None,
    ) -> list[dict[str, Any]]:
        """The queue plan's priced entries for one volume's jobs (and the running ones).

        ``pending`` is the scheduler's list (a stale snapshot will do; None
        when there is none). With no list at all, this volume's jobs that may
        run now are priced as the only queued ones. The walk stops at this
        volume's last job; a volume more than ``max_items`` down the queue is
        left unpriced.
        """

        running_keys = {_job_identity(job) for job in running}
        extra: list[dict[str, Any]] = []
        if pending is None:
            # No list at all: this volume's jobs that may run now are priced
            # as the only queued ones (a held-back one -- a backoff -- is not).
            runnable = {row.id for row in self.processor.missing_generations(cbz_path)}
            failures = self._load_failures()
            for row in owed:
                if (series, volume, row.id) in running_keys or row.id not in runnable:
                    continue
                if self.failure_key(self._rel_library_path(cbz_path), row) in failures:
                    continue
                extra.append(self._plan_entry(cbz_path, series, volume, row))
        items = self.plan_items([*(pending or []), *extra], running)
        ours = [
            index
            for index, entry in enumerate(items)
            if entry.get("series") == series and entry.get("volume") == volume
        ]
        if max_items is not None and ours and ours[-1] >= max_items:
            ours = []
        plan = self.queue_plan(
            running, items if ours else [], through=ours[-1] if ours else None
        )
        return [*plan.running, *plan.pending]

    def _plan_entry(
        self, cbz_path: Path, series: str, volume: str, row: GenerationSpec
    ) -> dict[str, Any]:
        return {
            "series": series,
            "volume": volume,
            "generation": row.name,
            "generation_id": row.id,
            "engine": row.engine,
            "detector": row.reported_detector,
            "pages": self._known_pages(cbz_path),
        }

    def plan_items(
        self, pending: Sequence[dict[str, Any]], running: Sequence[dict[str, Any]]
    ) -> list[dict[str, Any]]:
        """The queue the plan walks: ``pending`` without what runs.

        Every generation a volume is owed is a real job in the scheduler's
        list from the moment the volume is known, so there is nothing to add.
        The manifest (`volume_plan`) and the queue file
        (`OcrControl.queue_document`) both price this same list, so they
        agree on every volume.
        """
        running_keys = {_job_identity(job) for job in running}
        return [dict(entry) for entry in pending if _job_identity(entry) not in running_keys]

    def _lane_pricing(
        self,
    ) -> tuple[Callable[..., RateEstimate | None], Callable[..., StartupEstimate]]:
        """``(rate_for, startup_for)``: how fast a row reads, and starts, on a machine.

        One pair for the queue page's plan and the claim's earliest-finish
        walk alike, so the machine a volume is predicted on is the machine
        it is given to. A processor's saved benchmark is the prior until its
        real runs are in; each pair's benchmark is read once per call.
        """
        benches: dict[tuple[str, str], Mapping[str, Any] | None] = {}

        def rate_for(
            generation_id: str,
            machine: str | None = None,
            *,
            observed_pages: int = 0,
            observed_seconds: float = 0.0,
        ) -> RateEstimate | None:
            bench = self._machine_bench(generation_id, machine, benches)
            prior = _as_float((bench or {}).get("pages_per_second"))
            return self.rates.rate_on(
                generation_id,
                None if machine is None else self._rate_key(generation_id, machine),
                machine_prior=(
                    RateEstimate(prior, SOURCE_BENCH, 0, 0.0)
                    if prior is not None and prior > 0
                    else None
                ),
                observed_pages=observed_pages,
                observed_seconds=observed_seconds,
            )

        def startup_for(generation_id: str, machine: str | None = None) -> StartupEstimate:
            bench = self._machine_bench(generation_id, machine, benches)
            return self.rates.startup_on(
                generation_id,
                None if machine is None else self._rate_key(generation_id, machine),
                machine_prior=_as_float((bench or {}).get("startup_seconds")),
            )

        return rate_for, startup_for

    def _machine_bench(
        self,
        generation_id: str,
        machine: str | None,
        cache: dict[tuple[str, str], Mapping[str, Any] | None],
    ) -> Mapping[str, Any] | None:
        """A machine's own saved benchmark of this row (its profile), or None.

        Read once per status poll per pair (``cache``), and only for the
        row's CURRENT recipe: a benchmark of another pipeline is no
        evidence about this one. This machine's is its own profile's
        (`LOCAL_PROFILE`, what its autobench measured); the `RateModel`
        still prefers the row's `.ocr-bench.json`, this server's other
        benchmark record, where a person's later benchmark lands.
        """
        if machine is None:
            return None
        key = (generation_id, machine)
        if key not in cache:
            row = self._generation(generation_id)
            found = None
            if row is not None:
                try:
                    found = self.profiles.row(
                        LOCAL_PROFILE if machine == LOCAL_SLOT else machine,
                        generation_id,
                        recipe=row.output_affecting(),
                        mode=row.precision,
                        supported=self._machine_formats(machine, row),
                    )
                except Exception:  # pragma: no cover - an estimate never fails a poll
                    found = None
            cache[key] = found.bench if found is not None else None
        return cache[key]

    def _startup_every_volume(self, generation_id: str, machine: str | None = None) -> bool:
        """True for a row that loads its model again for every volume.

        The one-volume command line, which is what is left for an installed
        mokuro with no serve module: no session, so no session to amortise
        the load over. Only ever on THIS machine: a processor's slot always
        runs a session.
        """
        if machine is not None and machine != LOCAL_SLOT:
            return False
        row = self._generation(generation_id)
        if row is None:
            return False
        if not self._slots:
            # No local hardware: every lane is a processor's, and a
            # processor's slot always runs a session.
            return False
        return not self._session_row(row, self._slots[0])

    def last_pending(self) -> list[dict[str, Any]]:
        """The pending list for a queue-page build: cached whenever it is still valid.

        However OLD the cached list is, it is used: new uploads and elapsed
        backoffs are the background refresher's business (`refresh_pending`,
        which bumps the state version when the list moved). Only a list the
        queue itself invalidated -- a scan starting or ending, a hold, a
        settings change, a returned claim; not a claim or a finished job,
        which update it in place -- is recomputed here, once, by the one
        build in flight.
        """
        with self._lock:
            cached = self._queue_cache
            generation = self._queue_generation
        if cached is not None and cached[1] == generation:
            return [dict(entry) for entry in cached[2]]
        return self.pending_jobs()

    def owed_generations(self, cbz_path: Path) -> list[GenerationSpec]:
        """Every enabled row this volume will still get, primary first, then list order.

        `processor.missing_generations` -- every row may run now -- in the
        order a manifest lists them. A volume whose supplied `.mokuro` is
        short of pages gets no additional layers (`skipped_generations`), so
        those are not owed. Read only: a few stats per row.
        """
        rows = self.processor.missing_generations(cbz_path)
        return sorted(rows, key=lambda row: not row.primary)

    def owed_by_volume(self) -> dict[Path, list[GenerationSpec]]:
        """`owed_generations` for every volume at once, from the shared walk.

        For a caller that would otherwise ask each of thousands of volumes
        in turn -- the queue file lists every pending volume's rows.
        """
        owed: dict[Path, list[GenerationSpec]] = {}
        for path, gen_id in self._walked_candidates():
            row = self._generation(gen_id)
            if row is not None:
                owed.setdefault(path, []).append(row)
        return {path: sorted(rows, key=lambda row: not row.primary) for path, rows in owed.items()}

    @staticmethod
    def _archive_stamp(path: Path) -> tuple[int, int] | None:
        try:
            st = path.stat()
        except OSError:
            return None
        return (st.st_size, st.st_mtime_ns)

    def _note_claim(self, job: tuple[Path, str]) -> None:
        """Record the archive a claim was taken from. Called under the lock."""
        self._job_stamps[job] = self._archive_stamp(job[0])

    def _job_cancelled(self, job: tuple[Path, str]) -> bool:
        """Has a settings change, a pre-empt or a removal cancelled this claim?"""
        with self._lock:
            return job in self._cancelled_ocr

    def _restamp(self, job: tuple[Path, str]) -> None:
        """The runner is about to read (or now holds) this job's archive: that
        file is the one its result must be written beside. A local run opens
        it now; a processor's verified download is in its runner (the file
        may have been replaced between the claim and the fetch, and the fetch
        took the new one)."""
        with self._lock:
            if job in self._inflight_ocr:
                self._job_stamps[job] = self._archive_stamp(job[0])

    def _archive_still_current(self, job: tuple[Path, str]) -> bool:
        """Whether ``job``'s archive is still the file it was claimed from.

        Asked just before its sidecar is written: a result for an archive
        deleted or replaced meanwhile must not land beside whatever is (or is
        not) there now. A claim with no recorded stamp only needs the file.
        """
        with self._lock:
            return self._archive_still_current_locked(job)

    def archive_removed(self, path: Path) -> None:
        """An archive -- or a folder of them -- left the library over WebDAV.

        Its queued jobs leave the cached pending list at once (valid or
        stale; the next computation would not list them either), and its
        running jobs are cancelled with the same no-failure cancellation a
        settings change uses (`_cancel_stale_jobs`).
        """
        library = self.storage_path / "library"
        try:
            relative = path.resolve().relative_to(library.resolve())
        except (OSError, ValueError):
            return
        target = library / relative
        is_archive = target.suffix.lower() == ".cbz"

        def gone(job_path: Path) -> bool:
            return job_path == target if is_archive else target in job_path.parents

        self._cancel_stale_jobs(gone)
        with self._lock:
            walk = self._candidate_walk
            if walk is not None:
                self._candidate_walk = (
                    walk[0], walk[1], [job for job in walk[2] if not gone(job[0])]
                )
            cached = self._queue_cache
            if cached is None:
                return
            kept = [
                entry
                for entry in cached[2]
                if not gone(library / str(entry.get("series")) / f"{entry.get('volume')}.cbz")
            ]
            if len(kept) == len(cached[2]):
                return
            self._queue_cache = (cached[0], cached[1], kept)
        self.queue_state.bump()

    def _cancel_stale_jobs(self, belongs: Callable[[Path], bool]) -> None:
        """Cancel the running jobs of archives that are gone or replaced.

        Of the claims whose archive ``belongs``, those whose file is no longer
        the one claimed are marked cancelled BEFORE anything is killed (so
        `finish_ocr_job` records no failure and no backoff), exactly as
        `apply_settings` does. A one-volume subprocess is killed. A session
        is killed only when every volume it holds is one of these: killing
        it would otherwise throw away its other volumes' work, and a result
        that does come back is discarded at collection anyway.
        """
        with self._lock:
            stale = {
                job
                for job in self._inflight_ocr
                if belongs(job[0]) and not self._archive_still_current_locked(job)
            }
            if not stale:
                return
            self._cancelled_ocr |= stale
            dropped: list[_OcrSlot] = []
            killed: list[OcrSession] = []
            for slot in self._slots_in_use():
                if slot.session is not None:
                    held = {
                        job
                        for job, owner in self._claim_owner.items()
                        if owner is slot and job in self._inflight_ocr
                    }
                    if held and held <= stale:
                        killed.append(slot.session)
                elif slot.job in stale:
                    dropped.append(slot)
            self._bump_queue_generation(keep_cache=True)
        for slot in dropped:
            if slot.processor.cancel_active():
                self._log(f"Cancelled the running OCR of {slot.job[0].name if slot.job else '?'}: "
                          "its archive was deleted or replaced")
        for session in killed:
            if session.kill():
                self._log("Closed an OCR session whose only volumes were deleted or replaced")

    def _archive_still_current_locked(self, job: tuple[Path, str]) -> bool:
        """`_archive_still_current`, for a caller already holding the lock."""
        known = job in self._job_stamps
        claimed = self._job_stamps.get(job)
        current = self._archive_stamp(job[0])
        if current is None:
            return False
        return not known or claimed is None or claimed == current

    def archive_arrived(self, cbz_path: Path) -> None:
        """A ``.cbz`` was just written into the library over WebDAV: queue it now.

        Its runnable jobs join the CACHED pending list at the place
        `order_jobs` gives them -- the same order a recomputation would, over
        the few hundred cached entries, without walking the library -- and
        the OCR loop is woken so it claims at once instead of a poll later.
        With no valid cached list there is nothing to join: the next reader
        computes one from disk, which already has the file. The poll stays
        the backstop either way.

        Never blocks on OCR: one short hold of the worker's lock. While no
        machine can run anything (`processing_hold`) the loop is not woken.
        """
        library = self.storage_path / "library"
        try:
            relative = cbz_path.resolve().relative_to(library.resolve())
        except (OSError, ValueError):
            return
        cbz = library / relative
        if cbz.suffix != ".cbz":
            return
        # A replaced archive: whatever still runs on the old file is wasted.
        self._cancel_stale_jobs(lambda job_path: job_path == cbz)
        owed_now = [(cbz, row.id) for row in self.processor.missing_generations(cbz)]
        with self._lock:
            walk = self._candidate_walk
            if walk is not None:
                kept = [job for job in walk[2] if job[0] != cbz]
                self._candidate_walk = (walk[0], walk[1], kept + owed_now)
        with self._lock:
            cached = self._queue_cache
            generation = self._queue_generation
            unavailable = self._inflight_ocr | self._attempted_ocr
            last_served = dict(self._last_served)
        if cached is not None:
            # A STALE snapshot (a scan started or ended since) is updated too,
            # and kept marked stale: it is what a PUT prices against when a
            # fresh list cannot be had in time (`pending_within`).
            # Jobs as (series, volume, generation id) -- what `order_jobs`
            # sorts by -- straight from the cached entries: no path is built
            # or resolved for the few hundred of them.
            arrived_series, arrived_volume, _ = self._job_key((cbz, ""))
            by_key: dict[tuple[str, str, str], dict[str, Any]] = {}
            for entry in cached[2]:
                key = (entry.get("series"), entry.get("volume"), entry.get("generation_id"))
                if not all(isinstance(part, str) for part in key):
                    continue
                if key[0] == arrived_series and key[1] == arrived_volume:
                    continue  # replaced: its jobs are re-derived below
                by_key[key] = entry  # type: ignore[index]
            arrived = [
                (cbz, row.id)
                for row in self.processor.missing_generations(cbz)
                if (cbz, row.id) not in unavailable
            ]
            failures = self._load_failures() if arrived else {}
            for job in arrived:
                fresh = self._pending_entry(job, failures)
                if fresh is not None:
                    by_key[(arrived_series, arrived_volume, job[1])] = fresh
            ordered = order_jobs(by_key, self._generation_rank(), last_served=last_served)
            updated = [by_key[key] for key in ordered]
            with self._lock:
                # Only onto the list we read: anything that moved the queue
                # meanwhile dropped it, and its own recomputation stands.
                if self._queue_cache is cached and self._queue_generation == generation:
                    self._queue_cache = (cached[0], cached[1], updated)
                    changed = cached[1] == generation and updated != cached[2]
                else:
                    changed = False
            if changed:
                self.queue_state.bump()
        if self.processing_hold() is None:
            self._wake.set()

    def wake_requested(self) -> bool:
        """True while an arrival has asked the OCR loop to scan early."""
        return self._wake.is_set()

    def pending_within(self, wait: float) -> list[dict[str, Any]] | None:
        """The pending list if it can be had within ``wait`` seconds, else None.

        The cached list at once when there is one (however old: see
        `last_pending`); otherwise one computation is started and waited on
        for ``wait`` -- it finishes in the background either way and fills
        the cache for whoever asks next. When it does not finish in time the
        last snapshot stands in, stale as it is (an arrival has been joined
        to it: `archive_arrived`); None only when there has never been one.
        """
        with self._lock:
            cached = self._queue_cache
            generation = self._queue_generation
        if cached is not None and cached[1] == generation:
            return [dict(entry) for entry in cached[2]]
        result: list[list[dict[str, Any]]] = []
        if wait > 0:
            thread = threading.Thread(
                target=lambda: result.append(self.pending_jobs()),
                name="ocr-pending-price",
                daemon=True,
            )
            thread.start()
            thread.join(wait)
        if result:
            return result[0]
        with self._lock:
            stale = self._queue_cache
        return [dict(entry) for entry in stale[2]] if stale is not None else None

    def refresh_pending(self, max_age: float = 5.0) -> None:
        """Recompute the pending list if its cache has gone stale, else nothing.

        The queue page's cheap path: it asks this on every poll, and all it
        costs while the cache is good is one lock and a clock read. A stale
        cache is recomputed exactly as `pending_jobs` would, which bumps the
        state version when the list really moved.
        """
        with self._lock:
            cached = self._queue_cache
            generation = self._queue_generation
        if (
            cached is not None
            and cached[1] == generation
            and time.monotonic() - cached[0] <= self._queue_max_age(max_age)
        ):
            return
        self.pending_jobs(max_age)

    def speed_report(
        self,
        running: Sequence[Mapping[str, Any]] = (),
        *,
        within: float = 6 * 3600.0,
    ) -> list[dict[str, Any]]:
        """REAL pages per minute per OCR generation, per machine and combined.

        One entry per ENABLED row that some machine finished a volume of in
        the last ``within`` seconds (the `RateModel`'s evidence, filed per
        machine). Each machine's number is what it really delivered -- pages
        over wall seconds of its recent finished volumes
        (`RateModel.throughput`) -- never the fitted slope its lanes are
        PRICED with: that slope is the marginal cost of a page, and on a
        machine with a large per-volume cost it reads several times what any
        volume achieved.

        ``lanes`` is how many of the machine's lanes are READING the row right
        now (a volume still starting, or on deck, delivers nothing yet), and
        ``combined`` what those lanes add up to: each one at its machine's
        real rate. None when nothing is reading the row.
        """
        lanes: dict[tuple[str, str], int] = {}
        for job in running:
            gen_id = job.get("generation_id")
            if not isinstance(gen_id, str) or job.get("status") == "starting":
                continue
            machine = job.get("machine")
            key = (gen_id, machine if isinstance(machine, str) and machine else LOCAL_SLOT)
            lanes[key] = lanes.get(key, 0) + 1
        report: list[dict[str, Any]] = []
        for row in enabled_generations(self.generations):
            names = self.rates.machines_with_evidence(row.id, within=within)
            names += [m for (g, m) in lanes if g == row.id and m not in names]
            machines: list[dict[str, Any]] = []
            combined = 0.0
            for name in names:
                found = self.rates.throughput(self._rate_key(row.id, name))
                if found is None:
                    continue
                working = lanes.get((row.id, name), 0)
                machines.append(
                    {
                        "machine": name,
                        "pages_per_minute": round(found.pages_per_minute, 1),
                        "volumes": found.volumes,
                        "lanes": working,
                        "working": working > 0,
                    }
                )
                combined += found.pages_per_minute * working
            if not machines:
                continue
            report.append(
                {
                    "generation": row.name,
                    "generation_id": row.id,
                    "machines": machines,
                    "combined_pages_per_minute": round(combined, 1) if combined else None,
                }
            )
        return report

    def _queue_max_age(self, max_age: float) -> float:
        """``max_age``, stretched to a few walks' worth on a library that is slow to walk.

        The page polls every few seconds, and at 12k volumes on a network
        share one walk took 7 s: a 5 s cache meant walking back to back for
        as long as anybody watched. A start, an end or an arrival still
        invalidates the cache at once (``_queue_generation``), so what this
        delays is only a backoff running out.
        """
        if max_age <= 0:
            return max_age
        return max(max_age, QUEUE_CACHE_WALK_FACTOR * self._queue_walk_seconds)

    def _cached_queue(self, max_age: float) -> list[dict[str, Any]] | None:
        """A copy of the cached `pending_jobs` result, if it is still good."""
        max_age = self._queue_max_age(max_age)
        with self._lock:
            cached = self._queue_cache
            generation = self._queue_generation
        if (
            cached is None
            or cached[1] != generation
            or max_age <= 0
            or time.monotonic() - cached[0] > max_age
        ):
            return None
        return [dict(entry) for entry in cached[2]]

    def _thumbnail_candidates(self) -> list[Path]:
        """Find library CBZ files missing thumbnails."""
        library_path = self.storage_path / "library"
        if not library_path.exists():
            return []
        return [
            p
            for p in library_path.rglob("*.cbz")
            if p.is_file() and self.processor.needs_thumbnail(p)
        ]

    def claim_next(
        self,
        slot: _OcrSlot | None = None,
        generation_id: str | None = None,
    ) -> tuple[Path, str] | None:
        """Take the next runnable job off the queue, atomically.

        The ONE place a job is picked. Choosing it and marking it taken are
        a single critical section, so two slots arriving together cannot
        walk away with the same job: the loser skips it and takes the next
        one down the list.

        ``generation_id`` narrows the claim to ONE row without changing any
        of the rules below -- the order and the missing-pages rule are applied
        before it, so "the next job for this generation" is a filter on the
        same queue rather than a second queue. A caller that keeps one runner
        process open for a row asks that way; the scan asks for the next job
        of any row.

        **A JOB belongs to one slot at a time, a volume does not.** Its
        generations may run at once on different machines: each sidecar is
        stamped with the volume's own `volume_uuid` whichever lands first
        (`OCRProcessor.volume_uuid_for`), so no layer waits for the primary
        and two jobs on one volume cannot orphan each other. Row order is
        still the priority -- every volume's first generation is offered
        before any volume's second -- only no longer a dependency.

        The library walk stays OUTSIDE the lock: it reads every sidecar in
        the library, while the queue page's polls and the running jobs'
        progress writes take the same lock. Its result is only a proposal;
        the locked step re-checks each job against the live sets before
        taking it, so anything claimed meanwhile is simply skipped.
        """
        job, _ = self._claim(slot, generation_id)
        return job

    def claim_for_session(
        self,
        slot: _OcrSlot,
        generation_id: str,
    ) -> tuple[tuple[Path, str] | None, bool]:
        """One claim for an open session, plus whether an EARLIER row is waiting.

        ``(job or None, pre-empt)`` from ONE walk of the library, because a
        session asks both questions at every volume boundary and each answer
        on its own costs a full scan of every sidecar in the library.

        Pre-emption is row order asserting itself: a new upload needs the
        primary row's sidecar, and a session grinding through a lower row's
        backlog must stop submitting and hand the slot back. It is reported
        rather than acted on here -- the session finishes the volumes it has
        already accepted, which is the only boundary at which a session may
        be switched.
        """
        return self._claim(slot, generation_id, report_preempt=True)

    def _slot_refusal(self, slot: _OcrSlot, row: GenerationSpec) -> str | None:
        """Why this slot's hardware cannot run ``row``, or None.

        This server's slots answer from what it could not install first
        (`local_unavailable`) -- spec section 3 rule 2 for the library's
        own entry -- and then from its devices; a processor's slot from its
        catalog (`RemoteOCRProcessor.can_run`).
        """
        if self._slot_entry(slot) is None:
            problem = local_environment_problem(self.local_unavailable, row)
            if problem:
                return problem
        return slot.processor.can_run(row)

    def _engine_device(
        self,
        row: GenerationSpec,
        *,
        stage_device: Mapping[str, str] | None = None,
        gpu: bool | None = None,
    ) -> str:
        """Which device this row's MODEL sits on, resolved from its pools.

        The engine stage for a staged row, the one ``mokuro`` stage for a row
        whose engine IS a process of its own -- a serve process (ADDENDUM 8)
        or the one-volume command line it falls back to. In every case the
        thing that cannot be shared. Read off the row's OWN device stages, so
        a road whose model-bearing stage is called something else is not
        silently taken as ``auto``.

        By default the row's own table on THIS server's hardware. A
        processor's slot passes the placement that machine runs the row with
        (``stage_device``, from `_remote_pools`) and whether IT has a card
        (``gpu``): `auto` on a 4090 box is its card 0, whatever this box has.
        """
        keys = row.device_stage_keys
        key = STAGE_MOKURO if STAGE_MOKURO in keys else STAGE_ENGINE
        pins = row.pools.stage_device if stage_device is None else stage_device
        asked = pins.get(key, DEVICE_AUTO)
        return resolve_device(asked, gpu=cached_catalog().has_gpu if gpu is None else gpu)

    @staticmethod
    def _entry_has_gpu(entry: ProcessorEntry) -> bool:
        """Whether a processor reported any card at all."""
        from mokuro_bunko.ocr.devices import catalog_from_processor

        return catalog_from_processor(entry.catalog).has_gpu

    def _remote_engine_device(self, entry: ProcessorEntry, row: GenerationSpec) -> str:
        """`_engine_device` for a row as THAT processor runs it. Reads its profile."""
        pools = self._remote_pools(entry, row)
        return self._engine_device(
            row, stage_device=pools.get("stage_device") or {}, gpu=self._entry_has_gpu(entry)
        )

    def _devices_in_use(self, slot: _OcrSlot | None = None) -> set[str]:
        """The devices open sessions already hold ON THIS SLOT'S HARDWARE.

        Called under the lock. A session on another machine holds nothing
        here: `gpu:0` on a processor is not this server's `gpu:0`, so
        counting it would steer a local slot away from an idle card. A
        processor's session holds what it was OPENED with (its `row_spec`,
        the processor's own placement), resolved against that processor's
        cards -- never this server's.
        """
        mine = self._slot_entry(slot) if slot is not None else None
        devices = set()
        for session in self._open_sessions:
            if getattr(session, "entry", None) is not mine:
                continue
            row = getattr(session, "generation", None)
            if row is None:
                continue
            spec = getattr(session, "row_spec", None)
            if mine is not None and isinstance(spec, Mapping):
                pools = spec.get("pools") or {}
                devices.add(
                    self._engine_device(
                        row,
                        stage_device=pools.get("stage_device") or {},
                        gpu=self._entry_has_gpu(mine),
                    )
                )
            else:
                devices.add(self._engine_device(row))
        return devices

    def _in_device_order(
        self,
        proposed: Sequence[tuple[Path, str]],
        busy_devices: set[str],
        generation_id: str | None,
        device_of: Callable[[GenerationSpec], str] | None = None,
    ) -> list[tuple[Path, str]]:
        """``proposed``, with jobs for an idle device first, order kept.

        A stable partition, never a re-sort: row order still decides between
        two rows on the same device, and a slot asking for ONE generation
        (a session topping itself up) is not steered at all. ``device_of``
        is where the slot's own hardware puts a row (default: this server,
        by the row's own table).
        """
        jobs = list(proposed)
        if generation_id is not None or not busy_devices:
            return jobs
        placed = device_of or self._engine_device
        free: list[tuple[Path, str]] = []
        busy: list[tuple[Path, str]] = []
        for job in jobs:
            row = self._generation(job[1])
            target = busy if row is not None and placed(row) in busy_devices else free
            target.append(job)
        return free + busy

    def _claim(
        self,
        slot: _OcrSlot | None,
        generation_id: str | None,
        *,
        report_preempt: bool = False,
    ) -> tuple[tuple[Path, str] | None, bool]:
        hardware = self._slot_hardware(slot)
        with self._lock:
            if self._holds.get(hardware) or self._stop_requested:
                return None, False
            if slot is not None and self._slot_gone(slot):
                # Its processor left: whatever this slot claimed now would be
                # sent to a stream nobody reads.
                return None, False
            if slot is not None and self._breaker_open(slot.processor_id):
                # Its archive downloads keep failing: held for the breaker's
                # hold, then offered one claim to see (design section 6.4).
                return None, False
            unavailable = self._inflight_ocr | self._attempted_ocr
        proposed = self._ocr_candidates(exclude=unavailable, reset_stale_failures=True)
        rank = self._generation_rank()
        # Row id -> whether THIS slot's hardware can run it. A processor's
        # catalog is asked once per row per claim, not once per volume.
        runnable: dict[str, bool] = {}

        def offered(gen_id: str) -> bool:
            if slot is None:
                return True
            if gen_id not in runnable:
                row = self._generation(gen_id)
                runnable[gen_id] = row is not None and self._slot_refusal(slot, row) is None
            return runnable[gen_id]

        # Row id -> whether this processor must be benchmarked on it first
        # (spec section 4). Read OUTSIDE the lock -- each answer is a profile
        # read off disk -- and once per row per claim. So is `offered`: a
        # processor's answer reads its profile's placement for the row.
        # This server's own slots are asked the same question of ITS
        # profile: a row nobody configured by hand is measured here first.
        entry = self._slot_entry(slot) if slot is not None else None
        unmeasured: set[str] = set()
        # Row id -> the device the row's model would sit on on THIS
        # processor (its placement, its cards), for the device-spread order.
        remote_devices: dict[str, str] = {}
        for gen_id in {job[1] for job in proposed}:
            row = self._generation(gen_id)
            if row is None or slot is None or not offered(gen_id):
                continue
            if entry is not None:
                remote_devices[gen_id] = self._remote_engine_device(entry, row)
            if self.autobench and self.autobench_needed(entry, row):
                unmeasured.add(gen_id)

        def device_of(row: GenerationSpec) -> str:
            # Only rows this processor is offered were placed; the rest are
            # never claimed here, so where they would sit does not matter --
            # and working it out now would read a profile under the lock.
            return remote_devices.get(row.id, "")

        # Rows whose runner keeps failing to start on THIS machine, and is
        # still inside its backoff (read outside the lock: the signature can
        # read a profile off disk).
        backed_off = self._backed_off_rows(slot, hardware, {job[1] for job in proposed})
        # Rows with ONE volume left that a machine with a WARM session of the
        # row will finish sooner than this one could start a runner and read
        # it (perf diagnosis F7). Only for a slot opening a NEW session: a
        # session's own top-up never pays a start.
        left_to_warm = (
            self._rows_left_to_warm(slot, hardware, proposed)
            if slot is not None and generation_id is None
            else set()
        )
        # Volumes a machine that finishes them sooner should have (read
        # outside the lock: it prices every lane, reading profiles). A
        # session's own top-up is asked too, deliberately -- unlike
        # `left_to_warm`: its warmth is priced (no startup), so it loses a
        # volume only to a lane that finishes it sooner even so, and then it
        # takes the next volume the walk gives it; it ends only when the
        # walk gives it nothing, which is the end of a queue, where a slow
        # card's lookahead would otherwise hold a volume a fast one is idle
        # for.
        eft_skip, eft_left, eft_mine, eft_fresh = (
            self._eft_left(slot, hardware, proposed)
            if slot is not None
            else (set(), [], None, False)
        )

        with self._lock:
            if self._holds.get(hardware) or self._stop_requested:
                return None, False
            # Re-checked under the SAME lock `processor_disconnected` takes:
            # a claim either lands before the disconnect (and is returned by
            # it) or is never taken.
            if slot is not None and self._slot_gone(slot):
                return None, False
            preempt = False
            if report_preempt and generation_id is not None:
                mine = rank.get(generation_id)
                # Only work THIS slot could take counts as an earlier row
                # waiting: a processor that cannot run the earlier row would
                # otherwise close its session for it, re-open one for the
                # same row, and pay a model load per volume forever.
                preempt = mine is not None and any(
                    (job[1], hardware) not in self._stopped_generations
                    and job[1] not in backed_off
                    and job[1] not in unmeasured
                    and rank.get(job[1], mine) < mine
                    and offered(job[1])
                    for job in proposed
                )
            # Spec section 3 rule 1 (a warm session is offered its row's next
            # volume) is the session's OWN top-up: `claim_for_session` asks
            # for its row by id. A slot starting a NEW session gets no such
            # preference for a row a sibling slot has open. Each session runs
            # its own runner, so that row is a model load here too; and
            # jumping row order for it, with pre-emption, let two slots take
            # turns re-opening the later row -- each pre-empted after two
            # volumes, each re-opened while the other was still open -- so
            # the earlier row never started on that processor.
            # With several cards, the first row with work is not always the
            # best row to START: a slot opening a session prefers a row whose
            # ENGINE device has no session on it already, so two rows pinned
            # to two cards both run instead of queueing on one (Addendum 7).
            # With everything on one device this is exactly today's order.
            busy_devices = self._devices_in_use(slot)
            ordered = self._in_device_order(
                proposed, busy_devices, generation_id,
                device_of=device_of if entry is not None else None,
            )
            if eft_mine is not None:
                # The walk's own volume, whatever order the loop would use:
                # the loop's order (idle devices first) is not the walk's,
                # and its first volume the walk had left to NOBODY was taken
                # instead -- live, a 238-page paddle volume on the slowest
                # card, where the walk had given it a small hayai one.
                if generation_id is not None and eft_mine[1] != generation_id:
                    # A session topping itself up: the walk gives this
                    # machine another row's volume next, so this row's are
                    # all someone else's. It drains and switches rows.
                    ordered = []
                elif eft_mine in ordered:
                    ordered = [eft_mine] + [job for job in ordered if job != eft_mine]
            for job in ordered:
                if generation_id is not None and job[1] != generation_id:
                    continue
                if (job[1], hardware) in self._stopped_generations:
                    continue
                if job[1] in backed_off or job[1] in left_to_warm:
                    continue
                if job in eft_skip:
                    continue
                if job in self._inflight_ocr or job in self._attempted_ocr:
                    continue
                if slot is not None and slot.processor_id in self._returned_by.get(job, ()):
                    # This processor gave it back this scan; another may take
                    # it now, and this one again from the next scan.
                    continue
                row = self._generation(job[1])
                if row is None:
                    continue
                if not offered(job[1]):
                    # Spec section 3 rule 2: never offered a row its catalog
                    # cannot run. Skipped, not attempted: it stays pending
                    # for a slot that can.
                    continue
                if not self._still_owed(job, row):
                    # Done by another road since the shared walk was taken.
                    # (`self._lock` is re-entrant: a Condition's RLock.)
                    self._forget_candidate(job)
                    continue
                if job[1] in unmeasured:
                    # Spec section 4: measured on THIS machine first. The
                    # request is only RECORDED here -- enqueueing takes this
                    # same lock (`preempt_for_bench`) -- and fired by the
                    # slot's loop once `_claim` has returned.
                    self._want_autobench(entry, row)
                    continue
                self._attempted_ocr.add(job)
                self._inflight_ocr.add(job)
                self._note_claim(job)
                self._eft_deadlines.pop(job, None)
                if slot is not None:
                    slot.waiting_for_faster = False
                # This series has had its turn, whatever the outcome. Keyed
                # by the generation: one row finishing a series must not
                # advance the cursor deciding where another row resumes.
                self._last_served[job[1]] = self._job_key(job)[0]
                self._bump_queue_generation(claimed=job)
                if slot is not None:
                    slot.job = job
                    # The row as it is NOW: the job runs with this and is
                    # collected under it, whatever the settings become.
                    slot.generation = row
                    self._claim_owner[job] = slot
                return job, preempt
            waiting = bool(eft_skip & set(proposed))
            if slot is not None:
                slot.waiting_for_faster = waiting
            if waiting and eft_fresh:
                # The machine a volume was just left to may be asleep in its
                # idle wait. Only for a volume left for the FIRST time: every
                # claim leaving the same volumes again woke every waiting
                # slot, which walked the queue, left them again and woke the
                # rest -- ~12 claims a second through a whole live run.
                self._lock.notify_all()
        if waiting and slot is not None:
            self._log_left_for_faster(slot, hardware, eft_left)
        return None, preempt

    # --- the machine that finishes a volume first ------------------------------

    def _eft_left(
        self, slot: _OcrSlot, hardware: str, proposed: Sequence[tuple[Path, str]]
    ) -> tuple[set[tuple[Path, str]], list[EftLeft], tuple[Path, str] | None, bool]:
        """The volumes this slot leaves to a machine that finishes them sooner.

        Returns ``(skip, left, mine, fresh)``: the volumes to pass over, what
        was left to whom (for the log), the volume the walk gives THIS slot
        -- which the claim then takes first (None: the walk gave it none, or
        could not be priced) -- and whether any volume was left for the
        first time (the machines waiting are woken only then).

        `earliest_finish_claim` walks the queue in its own order across every
        lane of the running scan -- each priced by its own machine, its work
        in flight, and a startup unless it is warm on the row -- and this
        slot takes the first volume the walk gives IT. What it returns here
        are the volumes before that one it could have run: without this, the
        first idle machine to ask took the volume, and a lone 292-page novel
        went to a card that needed 331 s for it while one that needs ~35 s
        sat idle. Nothing is left when the lanes cannot all be priced (the
        first-come rule then stands), and nothing past its deadline
        (`EFT_CLAIM_GRACE_SECONDS`): a volume never waits on a machine that
        is not coming for it.
        """
        walked = list(proposed)[:EFT_LOOKAHEAD]
        if not walked:
            return set(), [], None, False
        rate_for, startup_for = self._lane_pricing()
        lanes = self._eft_lanes(slot, {job[1] for job in walked}, rate_for)
        if lanes is None:
            self._eft_trace(slot, "no lanes (first-come)", walked)
            return set(), [], None, False
        jobs = [(job, job[1], self._known_pages(job[0])) for job in walked]
        decision = earliest_finish_claim(
            jobs,
            lanes,
            id(slot),
            rate_for=lambda gen_id, machine: rate_for(gen_id, machine),
            startup_for=lambda gen_id, machine: startup_for(gen_id, machine).seconds,
        )
        if decision is None or not decision.left:
            self._eft_trace(
                slot,
                "unpriceable (first-come)" if decision is None
                else f"mine={self._eft_job_name(decision.mine)} left=none",
                walked, lanes,
            )
            return set(), [], decision.mine if decision is not None else None, False
        now = time.time()
        skip: set[tuple[Path, str]] = set()
        kept: list[EftLeft] = []
        fresh = False
        with self._lock:
            live = set(proposed)
            for job in [job for job in self._eft_deadlines if job not in live]:
                del self._eft_deadlines[job]
            for left in decision.left:
                predicted = now + max(0.0, left.starts) + EFT_CLAIM_GRACE_SECONDS
                known = self._eft_deadlines.get(left.job)
                fresh = fresh or known is None
                if left.busy_first:
                    # The machine it is left to has work to finish first, and
                    # when that ends moves as the queue moves -- another
                    # volume taken first, a slower page. A deadline fixed at
                    # the first prediction ran out while that machine was
                    # still busy, and handed a paddle volume to a card that
                    # needs ~7x as long (live: 01:35:51 left, 01:39:42 taken
                    # by the slowest box with the fast one 55% through its
                    # own). So it follows the CURRENT prediction.
                    deadline = predicted if known is None else max(known, predicted)
                else:
                    # Idle now: it should come at once. The clock that
                    # catches a machine that is not coming runs from here
                    # and is never pushed back.
                    deadline = predicted if known is None else min(known, predicted)
                self._eft_deadlines[left.job] = deadline
                if now < deadline:
                    skip.add(left.job)
                    kept.append(left)
        if EFT_TRACE:
            owners = {id(candidate): candidate for candidate in self._active_slots}
            self._eft_trace(
                slot,
                f"mine={self._eft_job_name(decision.mine)} left="
                + "; ".join(
                    f"{self._eft_job_name(left.job)}->"
                    f"{self._slot_label(owners[left.to]) if left.to in owners else '?'}"
                    f" there={left.there:.0f}s here={left.here:.0f}s starts={left.starts:.0f}s"
                    f" busy={left.busy_first}"
                    f" deadline={self._eft_deadlines.get(left.job, 0.0) - now:+.0f}s"
                    f"{'' if left.job in skip else ' EXPIRED'}"
                    for left in decision.left
                ),
                walked, lanes,
            )
        return skip, kept, decision.mine, fresh

    def _eft_job_name(self, job: Any) -> str:
        if not isinstance(job, tuple) or len(job) != 2:
            return "none"
        return f"{job[0].stem}[{self._generation_name(job[1])}]"

    def _eft_trace(
        self,
        slot: _OcrSlot,
        what: str,
        walked: Sequence[tuple[Path, str]],
        lanes: Sequence[EftLane] | None = None,
    ) -> None:
        """One MOKURO_EFT_TRACE line: who asked, its lanes, what it decided."""
        if not EFT_TRACE:
            return
        with self._lock:
            owners = {id(candidate): candidate for candidate in self._active_slots}
        lane_text = ", ".join(
            f"{self._slot_label(owners[lane.key]) if lane.key in owners else '?'}"
            f"(free={lane.free_in:.0f}s warm={lane.warm} rows={sorted(lane.rows)})"
            for lane in lanes or []
        )
        self._log(
            f"[eft] {self._slot_label(slot)} asked ({len(walked)} pending): {what} | "
            f"lanes: {lane_text or '-'}"
        )

    def _eft_lanes(
        self,
        asking: _OcrSlot,
        row_ids: Collection[str],
        rate_for: Callable[..., RateEstimate | None],
    ) -> list[EftLane] | None:
        """Every slot of the running scan that could take work now, priced.

        None when the asking slot is not one of them (no scan, a slot made
        for a test), when it is the only one (there is nobody to leave a
        volume to, and the profile reads below would be wasted on every
        claim), or when a lane's work in flight cannot be priced. Only the
        rows in ``row_ids`` -- the ones the walk will meet -- are asked
        about. A lane is
        left out when its machine is held, gone or behind an open breaker;
        its ``rows`` are the enabled rows it may be GIVEN -- its catalog,
        its benchmark done (an unmeasured pair is benchmarked first), its
        start backoffs, its stopped rows -- the same answers `_claim` gives
        that slot.
        """
        with self._lock:
            slots = list(self._active_slots)
            if len(slots) < 2 or not any(candidate is asking for candidate in slots):
                return None
            snapshot = []
            for candidate in slots:
                machine = self._slot_hardware(candidate)
                if self._holds.get(machine) or self._slot_gone(candidate):
                    continue
                if candidate is not asking and not candidate.running:
                    # Its loop has ended: it claims nothing until the
                    # supervisor starts it again, so nothing waits for it.
                    continue
                if candidate.processor_id != LOCAL_SLOT and self._breaker_open(
                    candidate.processor_id
                ):
                    continue
                owned = [
                    (job, dict(self._active_progress.get(job) or {}))
                    for job, owner in self._claim_owner.items()
                    if owner is candidate and job in self._inflight_ocr
                ]
                session = candidate.session
                warm = None
                if session is not None and not getattr(session, "closing", False):
                    row = getattr(session, "generation", None)
                    warm = getattr(row, "id", None)
                stopped = {gen for gen, hw in self._stopped_generations if hw == machine}
                snapshot.append((candidate, machine, owned, warm, stopped))
        rows = [row for row in enabled_generations(self.generations) if row.id in row_ids]
        lanes: list[EftLane] = []
        for candidate, machine, owned, warm, stopped in snapshot:
            free_in = 0.0
            for job, card in owned:
                eta = _as_float(card.get("eta_seconds"))
                if eta is None:
                    rate = rate_for(job[1], machine)
                    total = _as_int(card.get("total_pages")) or self._known_pages(job[0])
                    if rate is None or not total:
                        return None
                    done = _as_int(card.get("done_pages")) or 0
                    eta = rate.volume_seconds(max(0, total - done))
                free_in += max(0.0, eta)
            entry = self._slot_entry(candidate)
            backed_off = self._backed_off_rows(candidate, machine, [row.id for row in rows])
            allowed = set()
            for row in rows:
                if row.id in stopped or row.id in backed_off:
                    continue
                if self._slot_refusal(candidate, row) is not None:
                    continue
                if self.autobench and self.autobench_needed(entry, row):
                    continue
                allowed.add(row.id)
            lanes.append(
                EftLane(
                    key=id(candidate), machine=machine, free_in=free_in, warm=warm,
                    rows=frozenset(allowed),
                )
            )
        return lanes

    def _log_left_for_faster(
        self, slot: _OcrSlot, hardware: str, left: Sequence[EftLeft]
    ) -> None:
        """Say once per volume and machine why this one is waiting instead."""
        with self._lock:
            owners = {id(candidate): candidate for candidate in self._active_slots}
        for item in left:
            key = (item.job, hardware)
            if key in self._left_logged:
                continue
            self._left_logged.add(key)
            to = owners.get(item.to)
            self._log(
                f"Leaving {item.job[0].name} ({self._generation_name(item.job[1])}) to "
                f"{self._slot_label(to) if to is not None else 'another machine'}: done "
                f"there in ~{item.there:.0f}s; {self._slot_label(slot)} would take "
                f"~{item.here:.0f}s"
            )

    # --- a slow start for one small volume ----------------------------------

    @staticmethod
    def _session_hardware(session: Any) -> str:
        entry = getattr(session, "entry", None)
        return LOCAL_SLOT if entry is None or getattr(entry, "local", False) else str(entry.name)

    def _rows_left_to_warm(
        self, slot: _OcrSlot, hardware: str, proposed: Sequence[tuple[Path, str]]
    ) -> set[str]:
        """Rows this machine should not open a session for right now.

        A session start is paid once per session (7.6-23 s measured on the
        desktop), so opening one to read a single volume that another
        machine, with a session of that row already WARM, will finish sooner
        -- its own work in flight included -- buys nothing but the start
        (perf diagnosis F7: desktop mokuro (fp16) sessions carried 1.2 volumes
        each and spent 38.7 % of their time starting). Only a row with
        exactly one volume this machine could take; only on real evidence
        (a known page count and a rate for both machines). The volume stays
        pending, and the warm session's own top-up takes it.
        """
        now = time.time()
        with self._lock:
            unavailable = self._inflight_ocr | self._attempted_ocr
            warm: dict[str, set[str]] = {}
            for session in self._open_sessions:
                generation = getattr(session, "generation", None)
                if generation is None or getattr(session, "closing", False):
                    continue
                alive = getattr(session, "is_alive", None)
                if getattr(session, "killed", False) or (callable(alive) and not alive()):
                    continue
                machine = self._session_hardware(session)
                if machine == hardware or self._holds.get(machine):
                    continue
                entry = getattr(session, "entry", None)
                pid = getattr(entry, "processor_id", None)
                if isinstance(pid, str) and self._breakers.get(pid) is not None and (
                    self._breakers[pid].is_open(now)
                ):
                    continue
                warm.setdefault(generation.id, set()).add(machine)
            if not warm:
                return set()
            owners = {job: self._slot_hardware(owner) for job, owner in self._claim_owner.items()}
            cards = {job: dict(card) for job, card in self._active_progress.items()}
        open_jobs: dict[str, list[tuple[Path, str]]] = {}
        for job in proposed:
            if job[1] in warm and job not in unavailable:
                open_jobs.setdefault(job[1], []).append(job)
        leave: set[str] = set()
        for gen_id, jobs in open_jobs.items():
            if len(jobs) != 1:
                continue
            pages = self._page_count(jobs[0][0])
            mine_rate = self.rates.rate_on(gen_id, self._rate_key(gen_id, hardware))
            if not pages or mine_rate is None:
                continue
            mine = (
                self.rates.startup_on(gen_id, self._rate_key(gen_id, hardware)).seconds
                + mine_rate.volume_seconds(pages)
            )
            for machine in warm[gen_id]:
                rate = self.rates.rate_on(gen_id, self._rate_key(gen_id, machine))
                if rate is None:
                    continue
                ahead = 0.0
                for job, owner in owners.items():
                    if owner != machine or job[1] != gen_id:
                        continue
                    card = cards.get(job, {})
                    eta = _as_float(card.get("eta_seconds"))
                    if eta is None:
                        total = _as_int(card.get("total_pages")) or self._known_pages(job[0]) or 0
                        done = _as_int(card.get("done_pages")) or 0
                        eta = rate.volume_seconds(max(0, total - done))
                    ahead += max(0.0, eta)
                theirs = ahead + rate.volume_seconds(pages)
                if theirs < mine:
                    leave.add(gen_id)
                    key = (jobs[0], hardware)
                    if key not in self._left_logged:
                        self._left_logged.add(key)
                        self._log(
                            f"Leaving {jobs[0][0].name} ({self._generation_name(gen_id)}) to "
                            f"{machine}: its warm session reads it in ~{theirs:.0f}s; opening "
                            f"one on {'this server' if hardware == LOCAL_SLOT else hardware} "
                            f"would take ~{mine:.0f}s"
                        )
                    break
        return leave

    # --- runners that will not start ----------------------------------------

    def _start_signature(self, spec: Mapping[str, Any], processor_id: str) -> str:
        """The row as one machine runs it, and which registration of it."""
        raw = json.dumps([dict(spec), processor_id], sort_keys=True, default=str)
        return hashlib.sha1(raw.encode("utf-8")).hexdigest()[:16]

    def _slot_start_signature(self, slot: _OcrSlot | None, row: GenerationSpec) -> str:
        entry = self._slot_entry(slot) if slot is not None else None
        if entry is not None:
            return self._start_signature(self._remote_row_spec(entry, row), entry.processor_id)
        return self._start_signature(self._local_run_row(row).to_dict(), LOCAL_SLOT)

    def _backed_off_rows(
        self, slot: _OcrSlot | None, hardware: str, row_ids: Collection[str]
    ) -> set[str]:
        """The rows this machine may not start a runner for yet.

        An entry whose row -- as this machine would run it -- has changed
        since (a new detector, other pools or devices, a processor that
        re-registered after a fix) is void: the next attempt is at once.
        """
        with self._lock:
            pending = {
                gen_id: backoff
                for gen_id in row_ids
                if (backoff := self._start_backoff.get((gen_id, hardware))) is not None
            }
        if not pending:
            return set()
        now = time.time()
        out: set[str] = set()
        for gen_id, backoff in pending.items():
            row = self._generation(gen_id)
            if row is None:
                continue
            try:
                current = self._slot_start_signature(slot, row)
            except Exception:  # noqa: BLE001 - a signature never blocks a claim
                current = backoff.signature
            if current != backoff.signature:
                with self._lock:
                    if self._start_backoff.get((gen_id, hardware)) is backoff:
                        del self._start_backoff[(gen_id, hardware)]
                continue
            if now < backoff.until:
                out.add(gen_id)
        return out

    def _note_start_failure(
        self, generation: GenerationSpec, hardware: str, signature: str, error: str
    ) -> None:
        """A runner that never became ready: space out the next attempts.

        The same shape a failed volume's retries have,
        ``min(poll_interval * 4^(n-1), 1 h)``, counted per (row, machine)
        across scans. Reset by a session that becomes ready, or by the row
        changing as that machine would run it.
        """
        key = (generation.id, hardware)
        with self._lock:
            previous = self._start_backoff.get(key)
            failures = (
                previous.failures + 1
                if previous is not None and previous.signature == signature
                else 1
            )
            delay = self._retry_delay_seconds(failures)
            self._start_backoff[key] = _StartBackoff(
                failures=failures, until=time.time() + delay, error=error[:300],
                signature=signature, name=generation.name,
            )
            self._bump_queue_generation()
        where = "" if hardware == LOCAL_SLOT else f" on {hardware}"
        self._log(
            f"Not starting {generation.name}{where} again for {delay:.0f}s: its runner "
            f"could not start ({failures} time{'s' if failures != 1 else ''} in a row): {error}"
        )

    def _note_start_success(self, generation: GenerationSpec, hardware: str) -> None:
        with self._lock:
            gone = self._start_backoff.pop((generation.id, hardware), None)
            if gone is not None:
                self._bump_queue_generation()
        if gone is not None:
            where = "" if hardware == LOCAL_SLOT else f" on {hardware}"
            self._log(f"{generation.name}{where} started again after {gone.failures} failure(s)")

    def start_backoffs(self, hardware: str) -> list[dict[str, Any]]:
        """This machine's rows in a start backoff now, for the queue and admin cards."""
        now = time.time()
        with self._lock:
            rows = [
                (gen_id, backoff)
                for (gen_id, machine), backoff in self._start_backoff.items()
                if machine == hardware and backoff.until > now
            ]
        return [
            {"generation": self._generation_name(gen_id) or backoff.name,
             "until": backoff.until, "failures": backoff.failures, "error": backoff.error}
            for gen_id, backoff in rows
        ]

    # --- holding the queue still ----------------------------------------

    @property
    def queue_held(self) -> bool:
        """True while something has asked for ANY machine to stay quiet."""
        with self._lock:
            return any(count > 0 for count in self._holds.values())

    def hardware_held(self, processor: str = LOCAL_SLOT) -> bool:
        """True while this machine (LOCAL_SLOT, or a processor's name) is held."""
        with self._lock:
            return self._holds.get(processor, 0) > 0

    def _job_hardware(self, job: tuple[Path, str]) -> str:
        """Whose hardware a claimed job is on. Called under the lock.

        A claim taken without a slot (a caller that tracks no ownership)
        is this machine's.
        """
        return self._slot_hardware(self._claim_owner.get(job))

    def _inflight_on(self, processor: str) -> set[tuple[Path, str]]:
        """The claims held on one machine's hardware. Called under the lock."""
        return {job for job in self._inflight_ocr if self._job_hardware(job) == processor}

    def hold_queue(self, timeout: float = 900.0, processor: str = LOCAL_SLOT) -> bool:
        """Stop claiming on one machine and wait for its running volumes.

        True when that machine really is quiet, False when the wait ran out
        (the hold is still in place either way, and the caller must release
        it). Nothing is killed: sessions stop submitting and close as their
        accepted volumes finish, and a per-volume job runs to its end. See
        :meth:`preempt_for_bench` for the alternative that ends what is
        running instead of waiting for it. Other machines keep working.
        """
        deadline = time.monotonic() + max(0.0, timeout)
        with self._lock:
            self._holds[processor] = self._holds.get(processor, 0) + 1
            self._bump_queue_generation()
            self._lock.notify_all()
            while self._inflight_on(processor) and time.monotonic() < deadline:
                self._lock.wait(timeout=0.25)
            return not self._inflight_on(processor)

    def release_queue(self, processor: str = LOCAL_SLOT) -> None:
        """Undo one :meth:`hold_queue` (or :meth:`preempt_for_bench`) of a machine."""
        with self._lock:
            count = self._holds.get(processor, 0) - 1
            if count > 0:
                self._holds[processor] = count
            else:
                self._holds.pop(processor, None)
            self._bump_queue_generation()
            self._lock.notify_all()

    def preempt_for_bench(
        self, timeout: float = 900.0, processor: str = LOCAL_SLOT
    ) -> tuple[bool, list[dict[str, str]]]:
        """Hold ONE machine AND end every OCR job running on it right now.

        Spec section 3 rule 5: the machine the benchmark is for -- this
        server's own hardware (LOCAL_SLOT) or a processor, by name -- and
        no other. A benchmark on this box measures nothing about a card in
        another room, and stopping that card for it would only lose its
        work; a benchmark on a processor, likewise, leaves this box alone.

        A benchmark answers "should I commit to this row?" and must not sit
        behind a 200-page volume the queue just started -- so unlike
        :meth:`hold_queue`, this never waits for a running volume to finish
        on its own. It reuses the EXACT cancel-without-failure mechanism
        `apply_settings` uses when a row is removed (below: every job in
        flight on that machine is added to `_cancelled_ocr` BEFORE anything
        is killed, so `finish_ocr_job`'s `_cancelled_ocr` branch records no
        failure and no backoff for any of them), except unconditional: every
        open SESSION on it is killed and every per-volume subprocess is
        cancelled, not only the ones whose row changed.

        Jobs are also dropped from `_attempted_ocr`, which `apply_settings`
        does not need to do (its cancelled jobs stay hidden until the next
        scan resets that set): a benchmark holds its machine for as long as
        it runs, so without this the interrupted volumes would stay
        invisible to `pending_jobs` -- and to every OTHER machine, which may
        take them meanwhile -- for the whole benchmark.

        Returns ``(quiet, preempted)``: whether that machine really emptied
        within `timeout` (kills are near-instant; this mainly waits for the
        bookkeeping in `finish_ocr_job`/`release_ocr_job` to catch up), and
        the ``{"generation", "volume"}`` pairs this interrupted, for the
        bench object's own `preempted` list.
        """
        with self._lock:
            self._holds[processor] = self._holds.get(processor, 0) + 1
            self._bump_queue_generation()
            mine = self._inflight_on(processor)
            preempted = [
                {"generation": self._generation_name(job[1]), "volume": job[0].stem}
                for job in sorted(mine, key=lambda job: (str(job[0]), job[1]))
            ]
            self._cancelled_ocr |= mine
            self._attempted_ocr -= mine
            slots = [
                slot for slot in self._slots_in_use() if self._slot_hardware(slot) == processor
            ]
            killed_sessions = [slot.session for slot in slots if slot.session is not None]
            dropped_slots = [
                slot for slot in slots if slot.session is None and slot.job is not None
            ]
            self._lock.notify_all()
        for slot in dropped_slots:
            name = slot.generation.name if slot.generation is not None else "?"
            if slot.processor.cancel_active():
                self._log(f"Cancelled the running {name} job for a benchmark")
        for session in killed_sessions:
            self._log(f"Closed the open {session.generation.name} session for a benchmark")
            session.kill()
        deadline = time.monotonic() + max(0.0, timeout)
        with self._lock:
            while self._inflight_on(processor) and time.monotonic() < deadline:
                self._lock.wait(timeout=0.25)
            return not self._inflight_on(processor), preempted

    def _every_machine_held(self) -> bool:
        """True when no machine that could claim anything is free to.

        What decides whether a scan is worth starting: a scan's first act is
        to walk every sidecar in the library, and repeating that every poll
        interval while nothing may claim would be the loudest thing on the
        disk while something is trying to measure a machine.
        """
        entries = self._remote_entries()
        with self._lock:
            held = [
                self._holds.get(entry.name, 0) > 0 or self._breaker_open(entry.processor_id)
                for entry in entries
            ]
            if self._slots:
                held.append(self._holds.get(LOCAL_SLOT, 0) > 0)
            return bool(held) and all(held)

    def _generation_name(self, gen_id: str) -> str:
        """This generation's current name, or its id when it is already gone."""
        row = self._generation(gen_id)
        return row.name if row is not None else gen_id

    # --- processors ------------------------------------------------------

    def _remote_entries(self) -> list[ProcessorEntry]:
        """The processors that contribute slots: connected, remote, installed."""
        registry = self.remote
        if registry is None:
            return []
        return [
            entry for entry in registry.connected() if not entry.local and not entry.installing
        ]

    def _all_slots(self) -> list[_OcrSlot]:
        """Local slots plus one per connected processor's session capacity.

        Rebuilt on every scan rather than held, because a processor is
        present exactly while its stream is open: a slot for a processor
        that left would claim volumes nothing can run. (A scan also adds the
        slots of a processor that logs in while it runs: `_supervise_slots`.)
        """
        slots = list(self._slots)
        for entry in self._remote_entries():
            slots.extend(self._remote_slots(entry, first_index=len(slots)))
        return slots

    def _remote_slots(self, entry: ProcessorEntry, *, first_index: int) -> list[_OcrSlot]:
        """One slot per session this processor said it can hold."""
        return [
            self._make_remote_slot(first_index + n, entry)
            for n in range(max(1, entry.max_sessions))
        ]

    def _make_remote_slot(self, index: int, entry: ProcessorEntry) -> _OcrSlot:
        from mokuro_bunko.ocr.remote.registry import RemoteOCRProcessor

        registry = self.remote
        assert registry is not None, "a remote slot needs the registry it came from"
        processor = RemoteOCRProcessor(
            storage_path=self.storage_path,
            # The primary's interpreters, as `_clone_processor` passes them:
            # nothing about a remote slot should re-probe this machine's
            # environments -- it never runs them.
            python_path=self.processor.python_path,
            status_callback=self.status_callback,
            generations=list(self.generations),
            engines_python_path=self.processor.engines_python_path,
            concurrency=self.concurrency,
            entry=entry,
            registry=registry,
            library_path=self.storage_path / "library",
            row_spec_for=self._remote_row_spec,
        )
        processor.missing_pages_lookup = self.missing_pages_lookup
        processor.rates = self.rates
        slot = _OcrSlot(index=index, processor=processor, processor_id=entry.processor_id)
        processor.progress_callback = lambda data: self._on_progress(slot, data)
        return slot

    def _remote_row_spec(self, entry: ProcessorEntry, generation: GenerationSpec) -> dict[str, Any]:
        """The row as THIS processor should run it.

        `detect x3` tuned on a 16-core box means nothing on a 48-core one,
        so the pools and the devices come from that processor's profile
        when it has one for this row's CURRENT recipe (a profile tuned for
        another engine may name stages this road has not got). Everything
        else -- the id, the engine, the detector, the patch budget -- is the
        row's and never varies by machine, because it decides the OUTPUT.
        """
        spec = generation.to_dict()
        spec["pools"] = self._remote_pools(entry, generation)
        # This processor's benchmarked pick for a balanced/speed mode: the
        # runner there takes it while its device still supports it.
        pick, why = self._machine_pick(str(entry.name), generation)
        spec.pop("precision_pick", None)
        spec.pop("precision_why", None)
        if pick is not None:
            spec["precision_pick"] = pick
            spec["precision_why"] = why
        return spec

    def _remote_pools(
        self, entry: ProcessorEntry, generation: GenerationSpec
    ) -> dict[str, Any]:
        """The pools THIS processor runs this row with. Reads its profile.

        Its profile's, when it has an entry with pools for the row's
        current recipe; the row's own table otherwise -- also for each table
        that entry leaves empty, which says nothing about the machine
        (`profiles.machine_pools`; all three empty is no entry at all,
        `profiles.holds_pools`). A width or capacity the machine stored as
        ``auto`` goes out as absent: derived there. The ONE answer both
        the offer gate (`RemoteOCRProcessor.can_run`) and the session
        (`_remote_row_spec`) use, so what is checked is what is sent.

        A profile that pins a stage to a card the processor no longer
        reports (it was saved, or benchmarked, while the machine had two)
        is set aside for the row's own table, and said once in the log:
        sending it would fail the runner before it is ready and strike the
        row off that machine every scan, with nothing pointing at the
        profile that did it.
        """
        own = generation.pools.to_dict()
        profile = self.profiles.row(
            entry.name,
            generation.id,
            recipe=generation.output_affecting(),
            mode=generation.precision,
            supported=self._entry_formats(entry, generation),
        )
        if profile is None or not profile.pools:
            return self._reachable_placement(entry, generation, own)
        from mokuro_bunko.ocr.remote.scheduler import unreported_pins

        pools = runner_pools(machine_pools(profile.pools, own))

        stale = unreported_pins(entry.catalog, pools["stage_device"])
        if not stale:
            return self._reachable_placement(entry, generation, pools)
        marker = (str(entry.name), generation.id, tuple(stale))
        if marker not in self._stale_profiles_logged:
            self._stale_profiles_logged.add(marker)
            pins = ", ".join(f"{stage} to {device}" for stage, device in stale)
            self._log(
                f"{entry.label()}'s saved pools for {generation.name} pin {pins}, which it "
                f"no longer reports; running the row's own table there until they are "
                f"saved again for {entry.name}"
            )
        return self._reachable_placement(entry, generation, own)

    def _reachable_placement(
        self,
        entry: ProcessorEntry,
        generation: GenerationSpec,
        pools: dict[str, Any],
    ) -> dict[str, Any]:
        """``pools`` with every model that machine CANNOT put on a card on the CPU.

        A detector whose card is an onnxruntime execution provider
        (``engine_runner.ORT_GPU_DETECTORS``) goes on the CPU on a processor
        whose onnxruntime reported no GPU provider: ``auto`` because that is
        what it resolves to there, said explicitly so the session's placement
        is visible here and holds for a runner that could not ask its own
        onnxruntime; a pin to a card because running there would fail the
        runner before it is ready and strike the row off the machine every
        scan (said once in the log, naming the machine and the pin, like a
        stale card). A processor that never said is sent the pools as they
        are -- its own runner still asks.
        """
        from mokuro_bunko.ocr.devices import catalog_from_processor

        road = generation.road
        if road is None or catalog_from_processor(entry.catalog).ort_gpu is not False:
            return pools
        devices = dict(pools.get("stage_device") or {})
        moved: list[tuple[str, str]] = []
        for key in generation.device_stage_keys:
            if not stage_needs_ort_gpu(
                road, key, detector=generation.effective_detector, engine=generation.engine
            ):
                continue
            asked = str(devices.get(key) or DEVICE_AUTO)
            if asked not in (DEVICE_AUTO, DEVICE_CPU):
                moved.append((key, asked))
            devices[key] = DEVICE_CPU
        if moved:
            marker = (str(entry.name), generation.id, ("onnxruntime", *moved))
            if marker not in self._stale_profiles_logged:
                self._stale_profiles_logged.add(marker)
                pins = ", ".join(f"{stage} to {device}" for stage, device in moved)
                self._log(
                    f"{generation.name} pins {pins} on {entry.label()}, but its onnxruntime "
                    f"has no GPU execution provider; running it on the CPU there"
                )
        return {**pools, "stage_device": devices}

    @staticmethod
    def _rate_key(generation_id: str, hardware: str) -> str:
        """The `RateModel` key for a row on one machine.

        This machine's own evidence stays under the row's id, which is what
        the congestion history and the saved benchmarks are keyed by too.
        A processor's goes under ``<id>@<name>``: the same model answers for
        it, but a 4090's session rate never moves this box's, and the
        queue's ETAs for work that runs HERE are not priced off a card in
        another room.
        """
        return generation_id if hardware == LOCAL_SLOT else f"{generation_id}@{hardware}"

    def _record_local_run(
        self, generation: GenerationSpec, pages: int, seconds: float, *, contended: bool = False
    ) -> None:
        """One volume this server read, counted in its own profile."""
        try:
            self.profiles.record_run(
                LOCAL_PROFILE,
                generation.id,
                pages=int(pages),
                seconds=float(seconds),
                congestion=None,
                recipe=generation.output_affecting(),
                contended=contended,
            )
        except Exception:  # noqa: BLE001 - a count never fails a finished volume
            logger.exception("recording this server's run of %s failed", generation.name)

    def _session_processor_name(self, entry: _SessionJob) -> str | None:
        """Whose hardware ran this volume: a processor's name, or None locally."""
        return None if entry.hardware == LOCAL_SLOT else entry.hardware

    # --- auto-bench (spec section 4) ------------------------------------

    def autobench_needed(self, entry: Any, generation: GenerationSpec) -> bool:
        """Must this (row, machine) pair be benchmarked before it runs there?

        See `autobench_kind`, which also says WHICH benchmark.
        """
        return self.autobench_kind(entry, generation) is not None

    def autobench_kind(self, entry: Any, generation: GenerationSpec) -> str | None:
        """``"full"``, ``"precision"`` or None: which benchmark the pair needs first.

        ``entry`` is the processor, or None for THIS server's own hardware.

        ``"full"`` (spec section 4): a pair with NO PROFILE ENTRY for the
        row's current recipe -- a new processor, a new row, or a row whose
        output-affecting fields changed -- or a stale one is benchmarked
        first: widths, placement and, for a balanced/speed mode, the
        precision trials. ANY current entry answers no: one that holds only
        the pools an admin saved for that machine is somebody's decision
        about it, and benchmarking over it would replace that tuning. This
        server asks the same of its own profile (`LOCAL_PROFILE`) for a row
        nobody configured by hand; a row whose own table holds anything
        (`_local_row_configured`) is never width-tuned here.

        ``"precision"``: hand-set pools switch width tuning off, never the
        precision pick. A balanced/speed row on a machine that supports more
        than one of its candidates and has no current pick for it (none yet,
        or stale after a mode or candidate change) gets a PRECISION-ONLY
        benchmark: the candidate trials at the machine's pools exactly as
        configured, storing the pick and never a pool.

        A pair whose benchmark could not be had is not asked again (until a
        restart), because a row nothing can measure must still run -- on its
        first supported candidate, untuned and logged, never stuck. Nothing
        is ever needed without a bench service to ask.
        """
        if not self.autobench or self.bench_service is None:
            return None
        if generation.monolithic:
            # One volume, one invocation of its own command line: no stages,
            # no pools, nothing for a benchmark to tune -- and nothing a
            # processor's ``--bench`` can run. Asking would fail, and fail
            # again after every restart (the failure is remembered in memory).
            return None
        local = entry is None or getattr(entry, "local", False)
        if local and not self.local_processing:
            return None
        name = LOCAL_PROFILE if local else str(entry.name)
        if (name, generation.id) in self._autobench_failed:
            return None
        supported = (
            self._machine_formats(LOCAL_SLOT, generation)
            if local
            else self._entry_formats(entry, generation)
        )
        profile = self.profiles.row(
            name,
            generation.id,
            recipe=generation.output_affecting(),
            mode=generation.precision,
            supported=supported,
        )
        hand_set = self._local_row_configured(generation) if local else False
        # A benchmark that no longer describes the row's precision mode on
        # the machine -- another mode, other candidates, another format -- is
        # no measurement of it (`profiles.stale_bench_reason`): measured again.
        full = not hand_set and (profile is None or profile.stale_bench)
        kind = "full" if full else (
            "precision"
            if self._precision_pick_needed(generation, profile, supported)
            else None
        )
        if kind is None:
            return None
        # The same question the queue asks: a row this server reads behind
        # its own command line (a served engine whose package cannot serve
        # here) has no pipeline to tune either. Asked last: it probes the
        # interpreter once.
        return None if local and self.processor.runs_mokuro_cli(generation) else kind

    @staticmethod
    def _precision_pick_needed(
        generation: GenerationSpec, profile: Any, supported: frozenset[str] | None
    ) -> bool:
        """A balanced/speed row with candidates to choose between and no current pick."""
        from mokuro_bunko.ocr.engine_runner import BENCHED_PRECISION_MODES, resolve_mode
        from mokuro_bunko.ocr.precision import bench_pick

        if not generation.precision_applies or generation.precision not in BENCHED_PRECISION_MODES:
            return False
        if supported is None:
            return False  # nobody said what the device runs: its runner decides
        if len(resolve_mode(generation.engine, generation.precision, supported).usable) < 2:
            return False
        bench = profile.bench if profile is not None else None
        return bench_pick(bench, generation.precision)[0] is None

    def precision_bench_state(self, machine: str, generation: GenerationSpec) -> str | None:
        """Where a machine's precision pick for the row stands, for the admin card.

        ``"done"`` (a current pick), ``"pending"`` (it is benchmarked before
        its next volume), ``"off"`` (``ocr.autobench: false``) or
        ``"failed"`` (its benchmark could not be had); None when the row's
        mode picks nothing by benchmark on that machine.
        """
        from mokuro_bunko.ocr.engine_runner import BENCHED_PRECISION_MODES, resolve_mode
        from mokuro_bunko.ocr.precision import bench_pick

        if not generation.precision_applies or generation.precision not in BENCHED_PRECISION_MODES:
            return None
        supported = self._machine_formats(machine, generation)
        if supported is None:
            return None
        if len(resolve_mode(generation.engine, generation.precision, supported).usable) < 2:
            return None
        name = LOCAL_PROFILE if machine == LOCAL_SLOT else machine
        try:
            profile = self.profiles.row(
                name, generation.id, recipe=generation.output_affecting(),
                mode=generation.precision, supported=supported,
            )
        except Exception:  # noqa: BLE001 - a status read never fails a poll
            profile = None
        if bench_pick(profile.bench if profile is not None else None, generation.precision)[0]:
            return "done"
        if not self.autobench or self.bench_service is None:
            return "off"
        if (name, generation.id) in self._autobench_failed:
            return "failed"
        return "pending"

    @staticmethod
    def _local_row_configured(generation: GenerationSpec) -> bool:
        """Did somebody configure this row's pools BY HAND?

        Yes when its own table in the config holds any explicit value -- a
        width, a queue capacity, a device (``auto`` spelled out included) --
        or a precision other than ``auto``. That table is then what this
        server runs, exactly as written: no autobench, no profile.
        """
        return not generation.pools.is_empty()

    def _local_pools(self, generation: GenerationSpec) -> dict[str, Any] | None:
        """The pools THIS server runs an unconfigured row with, from its profile.

        None -- run the row's own table -- for a row configured by hand, and
        for one this server has no profile pools for under the row's current
        recipe (never measured, measured with nothing to change, or measured
        for another pipeline). Otherwise the profile's tables over the row's
        (`profiles.machine_pools`), as the runner reads them
        (`profiles.runner_pools`: a width stored ``auto`` goes out absent).

        A profile that pins a model to a card this server no longer reports
        is set aside, and said once: sent, it would fail the runner before
        it is ready, every scan, with nothing pointing at the profile.
        """
        if self._local_row_configured(generation):
            return None
        try:
            profile = self.profiles.row(
                LOCAL_PROFILE,
                generation.id,
                recipe=generation.output_affecting(),
                mode=generation.precision,
                supported=self._machine_formats(LOCAL_SLOT, generation),
            )
        except Exception:  # noqa: BLE001 - a profile read never stops a run
            return None
        if profile is None or not profile.pools:
            return None
        pools = runner_pools(machine_pools(profile.pools, generation.pools.to_dict()))
        catalog = cached_catalog()
        stale = sorted(
            (stage, device)
            for stage, device in pools["stage_device"].items()
            if device not in ("", DEVICE_AUTO, DEVICE_CPU) and not catalog.knows(device)
        )
        if not stale:
            return pools
        marker = (LOCAL_PROFILE, generation.id, tuple(stale))
        if marker not in self._stale_profiles_logged:
            self._stale_profiles_logged.add(marker)
            pins = ", ".join(f"{stage} to {device}" for stage, device in stale)
            self._log(
                f"This server's benchmarked pools for {generation.name} pin {pins}, which "
                f"it no longer reports; running the row's own table here until it is "
                f"configured or benchmarked again"
            )
        return None

    def _local_run_row(self, generation: GenerationSpec) -> GenerationSpec:
        """The row as THIS server's runners should run it.

        The row itself (the very object) when its own table decides; else a
        copy carrying this machine's profile pools. Only the pools differ --
        the id, the engine, the detector and the patch budget decide the
        OUTPUT and never vary by machine -- and the copy is only ever turned
        into a command line (`OCRProcessor.run_row`): sessions, claims and
        sidecars all keep the configured row.
        """
        pools = self._local_pools(generation)
        pick, why = self._machine_pick(LOCAL_SLOT, generation)
        if pools is None and pick is None:
            return generation
        run = generation if pick is None else replace(
            generation, precision_pick=pick, precision_why=why
        )
        if pools is None:
            return run
        return replace(
            run,
            pools=GenerationPools(
                stage_workers=dict(pools.get("stage_workers") or {}),
                queue_capacity=dict(pools.get("queue_capacity") or {}),
                stage_device=dict(pools.get("stage_device") or {}),
            ),
        )

    # --- precision modes ------------------------------------------------------

    def _machine_catalog(self, machine: str | None) -> Any:
        """The device catalog of a machine by lane name; None when it is not here."""
        from mokuro_bunko.ocr.devices import catalog_from_processor

        if machine is None or machine == LOCAL_SLOT:
            return cached_catalog()
        for entry in self._remote_entries():
            if str(entry.name) == machine:
                return catalog_from_processor(entry.catalog)
        return None

    def _machine_formats(self, machine: str | None, row: GenerationSpec) -> frozenset[str] | None:
        """What the machine's device for the row computes in, by its probe; None: not known."""
        from mokuro_bunko.ocr.precision import model_device

        catalog = self._machine_catalog(machine)
        if catalog is None:
            return None
        return cast("frozenset[str] | None", catalog.supported_for(model_device(row, None)))

    @staticmethod
    def _entry_formats(entry: Any, row: GenerationSpec) -> frozenset[str] | None:
        from mokuro_bunko.ocr.devices import catalog_from_processor
        from mokuro_bunko.ocr.precision import model_device

        if entry is None or getattr(entry, "local", False):
            return cached_catalog().supported_for(model_device(row, None))
        return catalog_from_processor(entry.catalog).supported_for(model_device(row, None))

    def _machine_pick(self, machine: str, row: GenerationSpec) -> tuple[str | None, str]:
        """This machine's benchmarked pick for the row's balanced/speed mode, if any."""
        from mokuro_bunko.ocr.engine_runner import BENCHED_PRECISION_MODES
        from mokuro_bunko.ocr.precision import bench_pick

        if not row.precision_applies or row.precision not in BENCHED_PRECISION_MODES:
            return None, ""
        try:
            found = self.profiles.row(
                LOCAL_PROFILE if machine == LOCAL_SLOT else machine,
                row.id,
                recipe=row.output_affecting(),
                mode=row.precision,
                supported=self._machine_formats(machine, row),
            )
        except Exception:  # noqa: BLE001 - a profile read never stops a run
            return None, ""
        return bench_pick(found.bench if found is not None else None, row.precision)

    def _machine_precision_refusal(self, machine: str | None, row: GenerationSpec) -> str | None:
        """Why this machine cannot run the row's precision mode, or None."""
        from mokuro_bunko.ocr.devices import catalog_from_processor
        from mokuro_bunko.ocr.precision import row_refusal

        if not row.precision_applies:
            return None
        if machine is None or machine == LOCAL_SLOT:
            return row_refusal(row, cached_catalog())
        for entry in self._remote_entries():
            if str(entry.name) == machine:
                pools = self._remote_pools(entry, row)
                return row_refusal(
                    row, catalog_from_processor(entry.catalog), pools.get("stage_device") or {}
                )
        return None

    def precision_holds(self) -> dict[str, str]:
        """Row id -> why it is held: a precision mode no connected machine can run.

        Only rows whose mode the machines are judged by at all (a forced
        format; an auto mode always has fp32 to fall back on), and only while
        some machine is here to judge -- with none, the queue's own
        ``no-processor`` hold says it.
        """
        from mokuro_bunko.ocr.precision import hold_reason, is_forced

        machines = ([LOCAL_SLOT] if self.local_processing else []) + [
            str(entry.name) for entry in self._remote_entries()
        ]
        if not machines:
            return {}
        held: dict[str, str] = {}
        for row in enabled_generations(self.generations):
            if not row.precision_applies or not is_forced(row.precision):
                continue
            if all(self._machine_precision_refusal(m, row) is not None for m in machines):
                held[row.id] = hold_reason(row.precision)
        return held

    def held_rows(self) -> list[dict[str, str]]:
        """`precision_holds`, by row name, for the queue page (admins)."""
        return [
            {"generation": self._generation_name(gen_id) or gen_id, "reason": reason}
            for gen_id, reason in self.precision_holds().items()
        ]

    @staticmethod
    def _autobench_name(entry: Any) -> str:
        """Whose profile a pair is filed under: the processor's name, or this server's."""
        if entry is None or getattr(entry, "local", False):
            return LOCAL_PROFILE
        return str(entry.name)

    @staticmethod
    def _autobench_label(entry: Any) -> str:
        if entry is None or getattr(entry, "local", False):
            return "this server"
        return str(entry.label())

    def _precision_bench_spec(self, entry: Any, generation: GenerationSpec) -> dict[str, Any]:
        """The row as that machine runs it, for its precision-only benchmark."""
        if entry is None or getattr(entry, "local", False):
            spec = replace(self._local_run_row(generation), precision_pick=None).to_dict()
        else:
            spec = self._remote_row_spec(entry, generation)
        for key in ("precision_pick", "precision_why"):
            spec.pop(key, None)
        return spec

    def _want_autobench(self, entry: Any, generation: GenerationSpec) -> None:
        """Record a wanted benchmark. NEVER enqueues here.

        `_claim` runs under `self._lock`, and `BenchService.enqueue` reaches
        `preempt_for_bench`, which takes the same lock from its own thread.
        The request is fired by `_drain_autobench_requests` once the lock is
        released. Asked once per pair per scan.
        """
        marker = (self._autobench_name(entry), generation.id)
        if marker in self._autobench_asked or marker in self._autobench_inflight:
            return
        self._autobench_asked.add(marker)
        self._autobench_inflight.add(marker)
        self._autobench_wanted.append((entry, generation))

    def _autobench_pending(self) -> bool:
        """Whether a benchmark this worker asked for is still to come.

        Called under the lock. A slot whose only work is a row waiting on
        its benchmark must wait for it rather than end the scan: the next
        scan would only ask for the same benchmark again.
        """
        return bool(self._autobench_inflight)

    def _bench(self) -> Any:
        service = self.bench_service
        if service is not None and not hasattr(service, "enqueue") and callable(service):
            service = service()
        return service

    def _drain_autobench_requests(self) -> None:
        """Queue the benchmarks `_claim` asked for, outside the worker lock."""
        with self._lock:
            wanted, self._autobench_wanted = self._autobench_wanted, []
        if not wanted:
            return
        bench = self._bench()
        for entry, row in wanted:
            name = self._autobench_name(entry)
            label = self._autobench_label(entry)
            if bench is None:
                self._autobench_settled(name, row.id, "unavailable")
                continue
            try:
                kind = self.autobench_kind(entry, row)
            except Exception:  # noqa: BLE001 - asked again at the next scan
                kind = "full"
            if kind is None:
                # Answered meanwhile (a pick landed, the row changed).
                self._autobench_settled(name, row.id, "done")
                continue
            precision_only = kind == "precision"
            self._log(
                f"Benchmarking {row.name}'s precision ({row.precision}) on {label} "
                "before it runs there; its pools stay as set"
                if precision_only
                else f"Benchmarking {row.name} on {label} before it runs there"
            )
            try:
                bench.enqueue(
                    # This server's own line is the bench's "local" (its
                    # LOCAL_BENCH, the same word as LOCAL_SLOT): its result
                    # lands in `.ocr-bench.json` and in `LOCAL_PROFILE`.
                    row.id,
                    # A precision-only benchmark measures the machine's pools
                    # exactly as it runs them; a whole one starts from the row.
                    self._precision_bench_spec(entry, row) if precision_only else None,
                    processor=LOCAL_SLOT if name == LOCAL_PROFILE else name,
                    autobench=True,
                    precision_only=precision_only,
                    # `ran_on` is the registration the benchmark really ran
                    # on (the bench resolves the machine as connected when it
                    # starts): whether THAT one left decides "ask again".
                    on_done=lambda state, ran_on=None, name=name, gen=row.id: (
                        self._autobench_settled(name, gen, state, ran_on=ran_on)
                    ),
                )
            except Exception as e:  # noqa: BLE001 - a refused bench never stops a scan
                self._log(f"Could not benchmark {row.name} on {label}: {e}")
                self._autobench_settled(name, row.id, "refused")

    def _autobench_settled(
        self, name: str, generation_id: str, state: str, *, ran_on: Any = None
    ) -> None:
        """A benchmark this worker asked for is over, whatever became of it.

        Done: the profile now has the pair's bench (and its best pools), so
        `autobench_needed` answers no. Failed on a machine that is still
        there: the pair runs untuned from now on rather than never. Failed
        because the machine LEFT: it is asked again when it comes back --
        its hardware never answered, which is not "cannot measure". Either
        way, a slot waiting for it is woken to claim again.

        "Left" is decided by the registration the benchmark RAN ON
        (``ran_on``), never by whether a processor of that name is connected
        at this moment: one that drops re-registers under the same name
        within seconds, usually before the library has even noticed the old
        benchmark's body is gone. Without ``ran_on`` (a caller that does not
        know it) the name is all there is to go on. This server
        (`LOCAL_PROFILE`) never leaves.
        """
        if name == LOCAL_PROFILE:
            left = False
        elif ran_on is not None:
            left = bool(getattr(ran_on, "dropped", False))
        else:
            left = not any(entry.name == name for entry in self._remote_entries())
        marker = (name, generation_id)
        with self._lock:
            self._autobench_inflight.discard(marker)
            if state != "done":
                if left:
                    # Asked again in THIS scan too: the machine's new slot
                    # must be able to record the request, not find this
                    # scan's "already asked" and walk past the row.
                    self._autobench_asked.discard(marker)
                else:
                    self._autobench_failed.add(marker)
            self._bump_queue_generation()
            self._lock.notify_all()
        if state == "done":
            return
        where = "this server" if name == LOCAL_PROFILE else name
        if left:
            self._log(
                f"Benchmark of {self._generation_name(generation_id)} on {where} ended "
                f"{state}: {where} disconnected; it is benchmarked when it is back"
            )
        else:
            self._log(
                f"Running {self._generation_name(generation_id)} untuned on {where}: "
                f"its benchmark ended {state}"
            )

    @staticmethod
    def _slot_entry(slot: _OcrSlot) -> ProcessorEntry | None:
        """The processor a slot runs on, or None for this machine."""
        entry = getattr(slot.processor, "entry", None)
        return entry if slot.processor_id != LOCAL_SLOT else None

    @classmethod
    def _slot_hardware(cls, slot: _OcrSlot | None) -> str:
        """Whose hardware a slot is: LOCAL_SLOT, or the processor's NAME.

        The name rather than the registration id, because a processor keeps
        its name across a reconnect: a hold for its benchmark, and the
        strikes its broken environment has earned, stay with the machine.
        """
        entry = cls._slot_entry(slot) if slot is not None else None
        return LOCAL_SLOT if entry is None else str(entry.name)

    def _slot_gone(self, slot: _OcrSlot) -> bool:
        """True for a processor's slot whose processor has left."""
        entry = self._slot_entry(slot)
        return entry is not None and bool(entry.dropped)

    @classmethod
    def _slot_processor_label(cls, slot: _OcrSlot) -> str | None:
        """``tower (RTX 4090)`` for a remote slot, None for this machine."""
        entry = cls._slot_entry(slot)
        return None if entry is None else str(entry.label())

    @classmethod
    def _slot_label(cls, slot: _OcrSlot | None) -> str:
        """Who a slot is, for a log line."""
        label = cls._slot_processor_label(slot) if slot is not None else None
        return label or "this server"

    # --- provenance: who wrote each sidecar (`ocr.provenance`) ----------

    @classmethod
    def _slot_account(cls, slot: _OcrSlot | None) -> str | None:
        """The processor ACCOUNT a slot's results arrive under; None here."""
        entry = cls._slot_entry(slot) if slot is not None else None
        username = getattr(entry, "username", None) if entry is not None else None
        return str(username) if username else None

    def _slot_build(
        self,
        slot: _OcrSlot | None,
        generation: GenerationSpec,
        facts: SidecarFacts | None,
        *,
        through_runner: bool,
    ) -> str | None:
        """What software wrote a result: this server's, or what a processor reported."""
        entry = self._slot_entry(slot) if slot is not None else None
        uses_mokuro = bool(generation.monolithic or generation.served)
        if entry is None:
            from mokuro_bunko import __version__
            from mokuro_bunko.ocr.staging import runner_digest

            try:
                digest: str | None = runner_digest() if through_runner else None
            except OSError:
                digest = None
            return runner_build(
                bunko_version=__version__, runner_digest=digest,
                uses_mokuro=uses_mokuro, facts=facts,
            )
        host = getattr(entry, "host", None) or {}
        version = host.get("version") if isinstance(host, Mapping) else None
        digest = host.get("runner_build") if isinstance(host, Mapping) else None
        return runner_build(
            bunko_version=version if isinstance(version, str) else None,
            runner_digest=digest if isinstance(digest, str) else None,
            uses_mokuro=uses_mokuro, facts=facts,
        )

    def _record_rejected(
        self,
        cbz: Path,
        generation: GenerationSpec,
        slot: _OcrSlot | None,
        reason: str,
        hardware: str | None = None,
    ) -> None:
        """Audit a delivered result the library refused, with the reason."""
        if self.provenance is None:
            return
        self.provenance.rejected(
            cbz=cbz,
            generation=generation,
            machine=hardware or self._slot_hardware(slot),
            account=self._slot_account(slot),
            reason=reason,
        )

    def _watch_remote_rejections(
        self, session: Any, slot: _OcrSlot, generation: GenerationSpec
    ) -> None:
        """Have a processor's session report the results it refuses on the wire."""
        if not hasattr(session, "on_rejected"):
            return

        def rejected(volume: SessionVolume, reason: str) -> None:
            if volume.archive is not None:
                self._record_rejected(Path(volume.archive), generation, slot, reason)

        session.on_rejected = rejected

    def _slots_in_use(self) -> list[_OcrSlot]:
        """This machine's slots plus the running scan's processor slots.

        Called under the lock. Between scans that is only this machine's:
        a processor's slot exists only while a scan is running it.
        """
        slots = list(self._slots)
        slots.extend(slot for slot in self._active_slots if slot.processor_id != LOCAL_SLOT)
        return slots

    def _lane_count(self) -> int:
        """How many volumes can run at once right now: the queue's lanes."""
        return max(1, len(self._lane_machines()))

    def _lane_machines(self) -> list[str]:
        """Whose hardware each of the queue's lanes is, in slot order.

        This machine's slots (LOCAL_SLOT), then each connected processor's
        ``max_sessions`` under its name -- the name its numbers are filed
        under (`_rate_key`) and the one a running card carries as
        ``machine``.
        """
        machines = [LOCAL_SLOT] * len(self._slots)
        for entry in self._remote_entries():
            machines.extend([str(entry.name)] * max(1, entry.max_sessions))
        return machines

    def connected_machines(self) -> list[dict[str, Any]]:
        """Every machine that can run OCR now, with its lane count, in lane order.

        The queue page gives each one a card of a FIXED size for as long as it
        is here -- idle included -- rather than one that comes and goes with
        its jobs.
        """
        counts: dict[str, int] = {}
        for machine in self._lane_machines():
            counts[machine] = counts.get(machine, 0) + 1
        rows = [{"machine": name, "slots": slots} for name, slots in counts.items()]
        with self._lock:
            # Machines one of whose slots the earliest-finish scan is leaving
            # queued work to faster machines: work it COULD run (the walk
            # only leaves a volume the slot may take), so "Standby", not
            # "Idle" -- the queue page's STATE_STANDBY.
            standby = {
                self._slot_hardware(slot)
                for slot in self._active_slots
                if slot.running and slot.waiting_for_faster and slot.job is None
                # A machine held (a benchmark's hold) claims nothing and keeps
                # its own message; its last scan's flag says nothing now.
                and not self._holds.get(self._slot_hardware(slot))
            }
        for row in rows:
            if row["machine"] in standby:
                row["standby"] = True
        now = time.time()
        entries = self._remote_entries()
        with self._lock:
            held = {
                entry.name: breaker
                for entry in entries
                if (breaker := self._breakers.get(entry.processor_id)) is not None
                and breaker.is_open(now)
            }
        for row in rows:
            breaker = held.get(str(row["machine"]))
            if breaker is not None:
                # Its archive downloads keep failing (design section 6.4).
                row["held"] = "downloads"
                row["held_until"] = breaker.open_until
                row["held_error"] = breaker.last_error
            backoffs = self.start_backoffs(str(row["machine"]))
            if backoffs:
                # Rows whose runner will not start here, and until when.
                row["cannot_start"] = backoffs
        bench = self._bench()
        configuring = getattr(bench, "configuring", None)
        if callable(configuring):
            try:
                lines = configuring()
            except Exception:  # noqa: BLE001 - a status read never fails a poll
                lines = {}
            for row in rows:
                # Being benchmarked (auto-configured) right now: the card says
                # so rather than "Idle", which read as a lost machine.
                line = lines.get(str(row["machine"]))
                if line:
                    row["configuring"] = dict(line)
        return rows

    def processor_disconnected(self, entry: ProcessorEntry, reason: str) -> None:
        """Return everything that processor's slots were holding, unrecorded.

        Spec section 3 rule 4. Keyed on the slot that CLAIMED each volume
        (`_claim_owner`, by `_OcrSlot.processor_id`), not on open sessions: a
        claim taken by `claim_next(slot)` before the session opened belongs
        to the slot already, and keying on sessions would leave it stuck in
        `_inflight_ocr` forever. A re-registration ("re-registered") reads
        the same: the old entry's claims come back before the new one is
        offered anything.

        The claims are released HERE, at once, and re-offered in this scan
        -- not when the slot's thread next looks up. That thread may still
        try to settle one (a `volume_done` queued before the drop, a session
        start refused after it); `_take_for_settling` turns each such late
        outcome into a log line, so it can neither blame the volume nor
        release the claim from under the slot that took it next. A claim
        whose sidecar is being installed at this instant is the one
        exception: it is left to finish, because returning it mid-install
        would let the next owner write the same file beside it.

        Its sessions are ended too, so the slot threads running them see
        their exit on the next poll instead of waiting out the wedge timer
        on a processor that is not coming back.
        """
        with self._lock:
            claims = {
                job
                for job, owner in self._claim_owner.items()
                if owner.processor_id == entry.processor_id and job not in self._settling
            }
            for job in claims:
                self._settle(job)
                self._attempted_ocr.discard(job)
            sessions = [
                session
                for session in self._open_sessions
                if getattr(session, "entry", None) is entry
            ]
            # Keyed by registration: a processor that comes back is a new
            # registration and starts clean.
            self._breakers.pop(entry.processor_id, None)
            self._bump_queue_generation()
            self._lock.notify_all()
        for session in sessions:
            end = getattr(session, "end", None)
            if callable(end):
                end(reason)
        for job in claims:
            self._clear_active_progress(job)
        self._log(
            f"{entry.label()} disconnected ({reason}); "
            f"{len(claims)} volume(s) back in the queue"
        )

    def processing_hold(self) -> dict[str, Any] | None:
        """Why the queue is doing nothing, when the answer is "no hardware".

        None whenever something could run: local processing on, or at least
        one processor connected. Otherwise the queue page and the admin
        panel say so by name, rather than showing a silent, empty queue.
        """
        if self.local_processing:
            return None
        if self._remote_entries():
            return None
        registry = self.remote
        last = registry.last_disconnect() if registry is not None else None
        # "Since" is when there last WAS hardware: the last disconnect, or
        # this worker's start when nothing has connected since.
        hold: dict[str, Any] = {
            "reason": "no-processor",
            "since": last[1] if last is not None else self._started_at,
        }
        if last is not None:
            hold["last"] = {"name": last[0], "disconnected_at": last[1]}
        return hold

    def release_ocr_job(
        self,
        job: tuple[Path, str],
        generation: GenerationSpec,
        *,
        reason: str,
        retry_this_scan: bool = False,
        slot: _OcrSlot | None = None,
    ) -> None:
        """Give a claimed job back to the queue, recording NOTHING.

        The outcome for a volume that was in flight when something other
        than the volume went wrong: a runner that crashed under it, a
        settings change that removed its row, a shutdown. It is not a
        failure of the archive, so it gets no failure record, no attempt
        count and no backoff.

        ``retry_this_scan`` also clears the job from the "already tried in
        this scan" set, which is right when the session that held it is
        being reopened and wrong when the reason it was released will still
        be true a moment later.

        ``slot`` is the slot releasing it. A claim that slot no longer holds
        -- its processor disconnected and the claim was returned already --
        is left alone: it may be another slot's by now.
        """
        if not self._take_for_settling(job, slot):
            self._ignore_late(job, generation, slot)
            return
        self._log(f"Returned {generation.name} for {job[0].name} to the queue: {reason}")
        with self._lock:
            self._settle(job)
            if retry_this_scan:
                self._attempted_ocr.discard(job)
            self._bump_queue_generation()
            self._lock.notify_all()
        self._clear_active_progress(job)

    def _scan_ocr_once(self) -> None:
        """Process (CBZ, generation) jobs with missing sidecars, in queue order.

        The queue is recomputed after every job, so a volume that arrives
        while a lower row's backlog is running gets the first row's sidecar
        next instead of waiting for the backlog. Each job is tried at most
        once per scan.
        """
        # Records of volumes that are gone, or of rows that are gone, must not
        # sit on the queue page forever; a scan is the natural moment to sweep.
        self._prune_failure_records()
        with self._lock:
            self._attempted_ocr = set()
            self._returned_by = {}
            self._left_logged = set()
            # A new scan is a fresh chance for a row whose sessions kept
            # dying: the usual fix (an install finishing, a card freeing up)
            # happens between scans and must not need a restart.
            self._stopped_generations = set()
            self._session_strikes = {}
            self._autobench_asked = set()
            self._bump_queue_generation()
        ocr_candidates = self._ocr_candidates(reset_stale_failures=True)
        if ocr_candidates:
            volumes = len({path for path, _ in ocr_candidates})
            slots = f"; {self.concurrency} at a time" if self.concurrency > 1 else ""
            self._log(
                f"Found {len(ocr_candidates)} missing OCR sidecar(s) across {volumes} CBZ file(s) "
                f"(generations, in run order: "
                f"{', '.join(row['name'] for row in self.generation_order())}; "
                f"within a generation series take turns{slots})"
            )

        try:
            self._drain_ocr_queue()
        finally:
            # Between scans nothing is "already tried": the page then lists
            # what the next scan will run, due retries included.
            with self._lock:
                self._attempted_ocr = set()
                self._returned_by = {}
                self._bump_queue_generation()

    def _drain_ocr_queue(self) -> None:
        """Run the queue to exhaustion on every slot.

        With the default one slot and no processors this is the plain loop
        it has always been, on the scan thread, with no thread created and
        nothing to join. With processors attached every slot runs on a
        thread of its own and the scan thread SUPERVISES them
        (`_supervise_slots`), because the set of slots can grow while the
        scan runs. With no slot at all -- local processing off, nobody
        logged in -- there is nothing to drain and the queue holds.
        """
        slots = self._all_slots()
        with self._lock:
            self._active_slots = list(slots)
        try:
            if not slots:
                return
            if self.remote is not None:
                self._supervise_slots(slots)
                return
            helpers = [self._slot_thread(slot) for slot in slots[1:]]
            for helper in helpers:
                helper.start()
            try:
                self._run_ocr_slot(slots[0])
            finally:
                # The scan is not over while a slot is still running a job.
                for helper in helpers:
                    helper.join()
        finally:
            with self._lock:
                self._active_slots = []

    def _slot_thread(self, slot: _OcrSlot) -> threading.Thread:
        return threading.Thread(
            target=self._run_helper_slot,
            args=(slot,),
            daemon=True,
            name=f"ocr-sidecar-worker-{slot.index}",
        )

    def _supervise_slots(self, slots: list[_OcrSlot]) -> None:
        """Run every slot on a thread, and give newcomers slots as they log in.

        Load balancing is however many processors are logged in (the spec's
        own words), so a processor that logs in while a long backlog is
        draining joins THIS scan: its slots start as soon as it is seen,
        rather than after the backlog, which could be hours. Each slot still
        stops on its own when the queue has nothing left it can claim; the
        scan ends when every slot has.
        """
        running = [(slot, self._slot_thread(slot)) for slot in slots]
        for _slot, thread in running:
            thread.start()
        threads = [thread for _slot, thread in running]
        seen = {slot.processor_id for slot in slots}
        next_index = len(slots)
        while True:
            for thread in threads:
                thread.join(timeout=SLOT_SUPERVISE_SECONDS / max(1, len(threads)))
            if not any(thread.is_alive() for thread in threads):
                return
            with self._lock:
                quiet = self._stop_requested
                held = {machine for machine, count in self._holds.items() if count > 0}
                generation = self._queue_generation
            if quiet:
                continue
            # A slot ends when, at the moment it looks, nothing is claimable
            # and nothing is running -- but the scan goes on while any other
            # slot does, and that machine then sat out the rest of it: idle
            # beside a queue, and (live) the lane the earliest-finish
            # walk kept leaving volumes to until their deadline handed them
            # to the slowest card. Started again once the queue has moved on
            # since it ended (a claim, a finish, a hold -- each bumps the
            # generation), which costs one claim when there is still nothing.
            for index, (slot, thread) in enumerate(running):
                if thread.is_alive() or slot.running or self._slot_gone(slot):
                    continue
                if self._slot_hardware(slot) in held:
                    continue
                if slot.exited_generation is None or slot.exited_generation == generation:
                    continue
                restarted = self._slot_thread(slot)
                restarted.start()
                running[index] = (slot, restarted)
                threads.append(restarted)
                if EFT_TRACE:
                    self._log(f"[eft] {self._slot_label(slot)} rejoined the running scan")
            for entry in self._remote_entries():
                if entry.processor_id in seen or entry.name in held:
                    continue
                seen.add(entry.processor_id)
                added = self._remote_slots(entry, first_index=next_index)
                next_index += len(added)
                with self._lock:
                    self._active_slots.extend(added)
                self._log(f"{entry.label()} joined the running scan with {len(added)} slot(s)")
                for slot in added:
                    thread = self._slot_thread(slot)
                    thread.start()
                    running.append((slot, thread))
                    threads.append(thread)

    def _run_helper_slot(self, slot: _OcrSlot) -> None:
        """Run a slot on its own thread, reporting what ends it.

        Slot 0 runs on the scan thread, where an unexpected exception ends
        the scan and `_run_ocr_loop` logs it. A helper has no such reader:
        without this it would die into the interpreter's default handler
        and the slot would vanish for the rest of the scan with nothing in
        the server log to say so.
        """
        try:
            self._run_ocr_slot(slot)
        except Exception as e:
            self._log(f"OCR slot {slot.index} stopped: {e}")

    def _run_ocr_slot(self, slot: _OcrSlot) -> None:
        """Run claimed jobs on one slot until the queue is drained.

        The claim decides what kind of run this is. Because `claim_next`
        hands back the head of the ordered queue, the row a slot ends up
        serving is always the FIRST row with claimable work -- a session is
        opened for that row and kept fed, and a monolithic row's volume runs
        the one-volume-one-invocation path it has always run. Both kinds
        share this loop, which is what lets them run side by side.
        """
        with self._lock:
            slot.running = True
            slot.exited_generation = None
        try:
            self._run_ocr_slot_loop(slot)
        finally:
            with self._lock:
                slot.running = False
                slot.waiting_for_faster = False
                slot.exited_generation = self._queue_generation

    def _run_ocr_slot_loop(self, slot: _OcrSlot) -> None:
        """The body of `_run_ocr_slot`: claim, run, wait, until drained."""
        while not self._stop_requested:
            if self._slot_gone(slot):
                # Its processor left. The claims it held are back in the
                # queue already (`processor_disconnected`); idling here until
                # the scan ends would only hold the scan open.
                return
            with self._lock:
                generation = self._queue_generation
            job = self.claim_next(slot)
            # Whatever the claim asked to have measured first, asked for now
            # that the lock is free (see `_want_autobench`).
            self._drain_autobench_requests()
            if job is not None:
                row = slot.generation
                if row is not None and self._session_row(row, slot):
                    self._run_session(slot, row, job)
                else:
                    self._run_ocr_job(job, slot)
                continue
            with self._lock:
                # A machine held for a benchmark waits it out in THIS scan.
                # Returning would take its slot out of the scan for good: a
                # scan only adds processors it has not seen, and one that
                # other machines keep busy can run for hours -- so a card
                # would sit idle long after its benchmark had finished.
                held = bool(self._holds.get(self._slot_hardware(slot)))
                if (
                    not held
                    and not self._inflight_ocr
                    and not self._autobench_pending()
                    and not slot.waiting_for_faster
                ):
                    # Nothing claimable and nothing running: the queue is
                    # drained for every slot, not just this one.
                    return
                # What is left is another slot's, or is blocked on a volume
                # another slot holds. Wait for one to finish instead of
                # walking the library again on a timer -- unless something
                # changed while we were choosing, in which case the wakeup
                # may already have been missed and we simply retry.
                if held or self._queue_generation == generation:
                    self._lock.wait(timeout=SLOT_IDLE_WAIT_SECONDS)
            if held:
                # Alive, just held: the health check reads this file.
                self._touch_heartbeat()

    def _session_row(self, generation: GenerationSpec, slot: _OcrSlot) -> bool:
        """True when this row's volumes stream through one open runner.

        What is left on the per-volume path is a row that reads a whole
        volume behind its own command line: a monolithic engine, or a served
        one whose installed package turns out to have no serve module
        (``OCRProcessor.runs_mokuro_cli``). Such a run is one invocation a
        volume, extracted up front and collected at exit. Every other row --
        the composed engines, and mokuro now that pages can be streamed into
        it -- keeps one runner open and is fed.
        """
        if slot.processor_id != LOCAL_SLOT:
            # A processor has no per-volume road on THIS server: that road
            # runs the OCR here. `ocr.sessions: false` is this machine's own
            # fallback and says nothing about another one's.
            return True
        return self.sessions_enabled and not slot.processor.runs_mokuro_cli(generation)

    # --- sessions -------------------------------------------------------

    def _next_session_job_id(self) -> str:
        with self._lock:
            self._session_job_seq += 1
            return f"v{self._session_job_seq}"

    def _session_log_path(self, slot: _OcrSlot, generation: GenerationSpec) -> Path:
        """Where one slot's session writes its own prose.

        Per slot as well as per generation: with `ocr.concurrency` above one
        two slots may hold a session of the SAME row, and one log truncated
        by the other would lose both.
        """
        log_dir = get_ocr_log_dir(self.storage_path)
        log_dir.mkdir(parents=True, exist_ok=True)
        return log_dir / f"session.{generation.name}.{slot.index}.log"

    def _run_session(
        self,
        slot: _OcrSlot,
        generation: GenerationSpec,
        first_job: tuple[Path, str],
    ) -> None:
        """Keep ONE runner open for a row and stream volumes through it.

        The loop: submit up to :data:`SESSION_LOOKAHEAD` volumes, then react
        to what the runner says. `volume_done` collects a finished sidecar
        into the library and tops the lookahead up, `volume_failed` records
        that one volume and carries on, `page` and `stats` are the Queue
        page's progress. The session closes when the generation has nothing
        claimable and nothing is in flight -- the top-up claim immediately
        before that decision IS the re-check, so a volume that arrived while
        the last one finished keeps the session open.

        Nothing here may leave a claimed job in the in-flight set or a
        runner behind: every exit path goes through the `finally`.
        """
        inflight: dict[str, _SessionJob] = {}
        order: list[str] = []
        completed = 0
        draining: str | None = None
        # When the next claim may walk the library, after one found nothing.
        # A claim is a walk of every volume (7 s at 12k volumes on a network
        # share) and this loop comes round once per EVENT -- a page -- so an
        # empty claim is not repeated before the scan interval, unless the
        # session is about to close for want of work.
        next_claim_at = 0.0
        session: OcrSession | None = None
        last_event = time.monotonic()
        fatal_error: str | None = None
        hardware = self._slot_hardware(slot)
        # Started before the process is spawned: the startup this clock
        # measures is everything from "we decided to open a session" to the
        # first page out, which is what a person waiting on the queue pays.
        clock = _SessionClock()
        try:
            try:
                session = slot.processor.open_session(
                    generation, self._session_log_path(slot, generation)
                )
            except (FileNotFoundError, OSError) as e:
                self._fail_session_start(
                    first_job, generation, f"{generation.name}: {e}", slot=slot
                )
                return
            self._watch_remote_rejections(session, slot, generation)
            with self._lock:
                slot.session = session
                slot.job = None
                self._open_sessions.add(session)
                cancelled_first = first_job in self._cancelled_ocr
            if cancelled_first:
                # Cancelled -- a settings change, a pre-empt -- after the
                # claim but before this runner existed, so there was nothing
                # to kill then. It never starts, and nothing is recorded.
                session.kill()
                self.finish_ocr_job(first_job, generation, ok=False, slot=slot)
                return
            if not session.start():
                with self._lock:
                    cancelled_first = first_job in self._cancelled_ocr
                if cancelled_first:
                    # Killed between being attached and starting: the same.
                    self.finish_ocr_job(first_job, generation, ok=False, slot=slot)
                    return
                event = session.poll_event(timeout=5.0)
                error = (event or {}).get("error") if event else None
                self._fail_session_start(
                    first_job, generation, str(error or "runner would not start"), slot=slot
                )
                return
            self._submit_session_volume(
                session, slot, generation, first_job, inflight, order, clock
            )

            while True:
                if draining is None:
                    draining = self._session_drain_reason(session, generation, hardware)
                    if draining is not None:
                        self._log(f"{generation.name} session: no more volumes ({draining})")
                while (
                    draining is None
                    and len(inflight) < SESSION_LOOKAHEAD
                    and (not inflight or time.monotonic() >= next_claim_at)
                ):
                    job, preempt = self.claim_for_session(slot, generation.id)
                    if job is None:
                        if preempt:
                            draining = "an earlier generation has work"
                        else:
                            next_claim_at = time.monotonic() + max(1.0, self.poll_interval)
                        break
                    next_claim_at = 0.0
                    if not self._submit_session_volume(
                        session, slot, generation, job, inflight, order, clock
                    ):
                        draining = "the runner stopped accepting volumes"
                        break
                    if preempt:
                        draining = "an earlier generation has work"
                        break
                if not inflight:
                    break
                event = session.poll_event(timeout=SESSION_POLL_SECONDS)
                if event is None:
                    wedged = time.monotonic() - last_event > SESSION_WEDGE_SECONDS
                    if self._remote_session_lost(session, wedged=wedged):
                        # The processor is gone, not the runner: dropping it
                        # returns every claim unrecorded, ends this session,
                        # and its exit arrives on the next poll.
                        continue
                    if wedged:
                        self._log(
                            f"{generation.name} session said nothing for "
                            f"{SESSION_WEDGE_SECONDS:.0f}s; killing it"
                        )
                        session.kill()
                        fatal_error = (
                            f"the {generation.name} runner stopped responding "
                            f"(no event for {SESSION_WEDGE_SECONDS:.0f}s)"
                        )
                        break
                    continue
                last_event = time.monotonic()
                kind = event.get("event")
                if kind == "exit":
                    fatal_error = self._session_exit_error(
                        session, generation, event, fatal_error, inflight
                    )
                    break
                if kind in ("fatal", "spawn_failed"):
                    # A spawn failure is the session's fatal error too: the
                    # exit that follows it would otherwise be recorded as a
                    # runner that "exited before its volumes were finished",
                    # which says nothing about why.
                    detail = str(event.get("error") or "").strip()
                    if kind == "fatal":
                        fatal_error = detail or "the runner reported a fatal error"
                    else:
                        fatal_error = f"the {generation.name} runner could not start" + (
                            f": {detail}" if detail else ""
                        )
                    self._log(f"{generation.name} session failed: {fatal_error}")
                    continue
                completed += self._handle_session_event(
                    event, generation, inflight, order, clock, hardware=hardware
                )
        finally:
            if session is not None:
                self._end_session(
                    session, inflight, order, generation, fatal_error, completed,
                    hardware=hardware, ready=clock.ready_at is not None,
                )
                with self._lock:
                    self._open_sessions.discard(session)
            with self._lock:
                slot.session = None
                slot.job = None
                slot.generation = None

    def _remote_session_lost(self, session: Any, *, wedged: bool = False) -> bool:
        """Drop a processor that is silently gone. True when it was dropped.

        Two signs, both only for a REMOTE session, and both the processor's
        absence rather than its runner's failure:

        * its events body was never opened. A processor opens it before it
          spawns anything, so one that has not after `EVENTS_OPEN_SECONDS`
          is not there -- suspended, unplugged, off the Wi-Fi -- while its
          idle assignment stream still looks open (the kernel notices a dead
          peer only when a heartbeat write finally fails, many minutes on);
        * the session sat silent for the whole wedge timeout (``wedged``)
          AND the processor has sent nothing at all -- not even the ping its
          open body carries every few seconds -- for
          `EVENTS_SILENCE_SECONDS`. A processor still pinging has a wedged
          RUNNER, which is killed and blamed exactly as a local one is.

        Spec section 3 rule 4 then applies exactly as it does to a stream
        that closed: the claims go back unrecorded and no row is struck.
        Blaming the oldest volume here would give an innocent volume a
        failure and a backoff for a machine that was switched off.
        """
        entry = getattr(session, "entry", None)
        registry = self.remote
        if entry is None or registry is None or getattr(entry, "dropped", True):
            return False
        reason: str | None = None
        overdue = getattr(session, "events_overdue", None)
        if callable(overdue) and overdue(EVENTS_OPEN_SECONDS):
            reason = (
                f"it never opened the events body of session "
                f"{getattr(session, 'sid', '?')} ({EVENTS_OPEN_SECONDS:.0f}s)"
            )
        elif wedged:
            silent = time.time() - float(getattr(entry, "last_seen", 0.0) or 0.0)
            if silent > EVENTS_SILENCE_SECONDS:
                reason = f"it has sent nothing for {silent:.0f}s"
        if reason is None:
            return False
        registry.drop(entry.processor_id, reason)
        return True

    def _failure(self, error: str, log_path: Path | None) -> OcrFailure:
        from mokuro_bunko.ocr.processor import OcrFailure as _OcrFailure

        return _OcrFailure(error=error, log_file=str(log_path) if log_path else None)

    def _fail_session_start(
        self,
        job: tuple[Path, str],
        generation: GenerationSpec,
        error: str,
        *,
        slot: _OcrSlot | None = None,
    ) -> None:
        """A session that never started is the claimed volume's failure.

        Only that one volume's: nothing else was claimed yet. It is recorded
        as an ordinary failure so the backoff applies -- a runner that will
        not start is usually a broken environment, and retrying it once a
        second for every volume in the library is how a scan is wasted.
        """
        self._log(f"{generation.name} session could not start: {error}")
        hardware = self._slot_hardware(slot)
        refused = _precision_refused(error)
        if slot is None or not self._slot_gone(slot):
            try:
                signature = self._slot_start_signature(slot, generation)
            except Exception:  # noqa: BLE001 - never lose the failure itself
                signature = ""
            self._note_start_failure(generation, hardware, signature, error)
        if hardware != LOCAL_SLOT or refused:
            # A processor whose runner will not start has a broken
            # ENVIRONMENT, and that is never the volume's fault: healthy
            # hardware would read it. It goes back unrecorded, and the strike
            # is that machine's alone -- two and it stops taking this row for
            # the scan, leaving it to machines that can.
            self.release_ocr_job(
                job, generation, reason=f"{hardware} could not start a runner: {error}",
                retry_this_scan=True, slot=slot,
            )
            if slot is not None and not self._slot_gone(slot) and not refused:
                self._strike_session(generation, error, hardware)
            return
        self.finish_ocr_job(
            job, generation, ok=False, failure=self._failure(error, None), slot=slot
        )

    def _submit_session_volume(
        self,
        session: OcrSession,
        slot: _OcrSlot,
        generation: GenerationSpec,
        job: tuple[Path, str],
        inflight: dict[str, _SessionJob],
        order: list[str],
        clock: _SessionClock | None = None,
    ) -> bool:
        """Send one claimed volume down the pipe and open its progress card.

        The row used is the LIVE one, looked up by id, so a rename applies
        to volumes submitted after it; the row that was used is remembered
        with the volume, because that is the name its file and its log were
        fixed under.
        """
        row = self._generation(generation.id) or generation
        job_id = self._next_session_job_id()
        try:
            volume = slot.processor.prepare_session_volume(job[0], row, job_id)
        except OSError as e:
            self.finish_ocr_job(
                job, row, ok=False, failure=self._failure(str(e), None), slot=slot
            )
            return True
        if slot.processor_id == LOCAL_SLOT:
            self._restamp(job)
        if not session.submit(volume):
            shutil.rmtree(volume.workspace, ignore_errors=True)
            self.release_ocr_job(
                job, row, reason="the runner closed before it took the volume",
                retry_this_scan=True, slot=slot,
            )
            return False
        inflight[job_id] = _SessionJob(
            job=job,
            generation=row,
            volume=volume,
            slot=slot.index,
            clock=clock if clock is not None else _SessionClock(),
            owner=slot,
            hardware=self._slot_hardware(slot),
            # A local runner has the volume the moment its pipe took the op.
            delivered=slot.processor_id == LOCAL_SLOT,
        )
        order.append(job_id)
        self.begin_ocr_job(
            job, row, slot=slot.index, total_pages=self._page_count(job[0]),
            processor=self._slot_processor_label(slot), owner=slot,
            machine=self._slot_hardware(slot),
            # Behind a session that is already warm, a volume is not loading
            # anything: it waits for its archive, or it is being read.
            session_ready=inflight[job_id].clock.ready_at is not None,
            delivered=inflight[job_id].delivered,
        )
        return True

    def _session_drain_reason(
        self,
        session: OcrSession,
        generation: GenerationSpec,
        hardware: str = LOCAL_SLOT,
    ) -> str | None:
        """Why this session must stop accepting volumes, or None to carry on."""
        if self._stop_requested:
            return "the worker is stopping"
        pid = getattr(getattr(session, "entry", None), "processor_id", None)
        with self._lock:
            if self._holds.get(hardware):
                return "the queue is held"
            if (generation.id, hardware) in self._stopped_generations:
                return "the generation was stopped for this scan"
            if isinstance(pid, str) and self._breaker_open(pid):
                return "its archive downloads keep failing"
        current = self._generation(generation.id)
        if current is None or not current.enabled:
            # `apply_settings` kills such a session outright; this covers the
            # window between a claim and the session being registered, where
            # the kill has nothing to reach yet.
            return "the generation was removed or disabled"
        if not session.is_alive():
            return "the runner exited"
        return None

    def _handle_session_event(
        self,
        event: dict[str, Any],
        generation: GenerationSpec,
        inflight: dict[str, _SessionJob],
        order: list[str],
        clock: _SessionClock | None = None,
        *,
        hardware: str = LOCAL_SLOT,
    ) -> int:
        """React to one runner event. Returns 1 for a volume completed, else 0.

        ``hardware`` is whose runner said it: every number an event carries
        is filed under THAT machine (`_rate_key`, the processor profiles),
        never blended into another's.
        """
        kind = event.get("event")
        if kind == "ready":
            self._log(
                f"{generation.name} session ready in "
                f"{float(event.get('startup_seconds') or 0.0):.1f}s"
                + (f" on {hardware}" if hardware != LOCAL_SLOT else "")
                + (f": {event['pipeline']}" if isinstance(event.get("pipeline"), str) else "")
            )
            # What that machine's session start really cost, straight from
            # the runner. It is charged once per session in the queue
            # prediction and shown while a volume has emitted nothing --
            # never added to a page rate.
            self.rates.record_startup(
                self._rate_key(generation.id, hardware), event.get("startup_seconds")
            )
            if clock is not None:
                clock.ready_at = time.time()
            # The cards' Loading ends HERE, on the runner's word -- not when
            # the estimate of the load runs out (`shape.job_state`).
            for waiting in inflight.values():
                self._set_owned_progress(waiting.job, {"session_ready": True}, waiting.owner)
            self._note_start_success(generation, hardware)
            return 0
        if kind == "stats":
            pipeline = summarize_event_stats(event.get("pipeline"))
            update: dict[str, Any] = {}
            if pipeline is not None:
                update["pipeline"] = pipeline
            if any(_as_float(event.get(key)) is not None for key in BUSY_SIGNALS):
                # The queue card's "host busy": a neighbour's load, said out
                # loud rather than read as this machine's speed.
                update["host_busy"] = self._busy_reason(event) is not None
            if update:
                for running in inflight.values():
                    self._set_owned_progress(running.job, dict(update), running.owner)
            return 0
        entry = inflight.get(str(event.get("id")))
        if entry is None:
            return 0
        if kind == "fetch":
            if event.get("state") == "ready":
                entry.delivered = True
                self._set_owned_progress(entry.job, {"delivered": True}, entry.owner)
                self._download_delivered(entry, event)
            # Anything else is progress: its arrival alone kept the session
            # clear of the wedge timer.
            return 0
        if kind == "volume_returned":
            inflight.pop(entry.volume.id, None)
            order.remove(entry.volume.id)
            shutil.rmtree(entry.volume.workspace, ignore_errors=True)
            self._judge_returned(entry, event)
            return 0
        if kind == "volume_started":
            entry.delivered = True
            entry.started_at = time.monotonic()
            entry.total_pages = _as_int(event.get("pages"))
            self._session_progress(entry)
            return 0
        if kind == "page":
            entry.done_pages = _as_int(event.get("done")) or 0
            total = _as_int(event.get("total"))
            if total:
                entry.total_pages = total
            if entry.first_page_at is None and entry.done_pages > 0:
                entry.first_page_at = time.time()
            self._session_progress(entry)
            return 0
        if kind == "volume_done":
            inflight.pop(entry.volume.id, None)
            order.remove(entry.volume.id)
            # The runner's own measure of the volume it just finished, folded
            # into this row's rate BEFORE the next volume's card is drawn, so
            # the second volume of a session is already predicted from the
            # first one's real speed rather than from a benchmark.
            first_of_session = entry.clock.completed == 0
            entry.clock.completed += 1
            busy = self._busy_reason(event)
            contended = busy is not None
            if busy is not None:
                # Read while a neighbour loaded the host: its seconds are the
                # neighbour's as much as this machine's (F5). The volume is
                # done all the same; only its speed is not learned.
                self._log(
                    f"{self._rel_library_path(entry.job[0])} ran on a busy host "
                    f"({busy}"
                    + (f" on {hardware}" if hardware != LOCAL_SLOT else "")
                    + "); its speed is not learned as this machine's"
                )
            else:
                self.rates.record_volume(
                    entry.rate_key,
                    event.get("pages"),
                    event.get("seconds"),
                    first_of_session=first_of_session,
                )
            processor_name = self._session_processor_name(entry)
            if not processor_name:
                # This server's own count of the row, kept like a processor's
                # (``@local.json``'s ``runs``); its congestion is already the
                # row's own history.
                self._record_local_run(
                    entry.generation,
                    _as_int(event.get("pages")) or 0,
                    _as_float(event.get("seconds")) or 0.0,
                    contended=contended,
                )
            if processor_name:
                # Per-MACHINE evidence (spec section 4, "numbers flow back"):
                # the processor's own profile, never this box's congestion
                # history. Nothing is recorded there for local hardware: the
                # row's own table and history already are its record.
                summary = summarize_event_stats(event.get("stats"))
                self.profiles.record_run(
                    processor_name,
                    entry.generation.id,
                    contended=contended,
                    pages=_as_int(event.get("pages")) or 0,
                    seconds=_as_float(event.get("seconds")) or 0.0,
                    # The SAME stored record this box's history keeps
                    # (`build_record`): `average_runs` reads the queues it
                    # hoists, so a raw summary averaged to no queue depths.
                    congestion=(
                        build_record(
                            summary,
                            volume=self._rel_library_path(entry.job[0]),
                            volume_pages=_as_int(event.get("pages")),
                            volume_seconds=_as_float(event.get("seconds")),
                            volume_first=first_of_session,
                        )
                        if summary is not None
                        else None
                    ),
                    recipe=entry.generation.output_affecting(),
                )
            return 1 if self._collect_session_volume(entry, event, first_of_session) else 0
        if kind == "volume_failed":
            inflight.pop(entry.volume.id, None)
            order.remove(entry.volume.id)
            error = str(event.get("error") or f"{generation.name} could not read this volume")
            if self._stop_requested:
                # A shutdown is nobody's failure. Closing a session aborts
                # the volume still streaming into it, and its runner reports
                # that as a failure -- which a slot still polling during
                # `stop()` would otherwise record, with a backoff, against a
                # volume that did nothing wrong.
                self.release_ocr_job(
                    entry.job, entry.generation,
                    reason="the worker is stopping", slot=entry.owner,
                )
                shutil.rmtree(entry.volume.workspace, ignore_errors=True)
                return 0
            self.finish_ocr_job(
                entry.job,
                entry.generation,
                ok=False,
                failure=self._failure(error, entry.volume.log),
                slot=entry.owner,
            )
            shutil.rmtree(entry.volume.workspace, ignore_errors=True)
            return 0
        return 0

    # --- archive downloads (protocol 2) ----------------------------------

    def _breaker_open(self, processor_id: str) -> bool:
        """Whether this processor's download breaker holds it now. Lock held
        or not: the worker's lock is re-entrant."""
        with self._lock:
            breaker = self._breakers.get(processor_id)
            return breaker is not None and breaker.is_open(time.time())

    def _download_delivered(self, entry: _SessionJob, event: Mapping[str, Any]) -> None:
        """A `fetch {state: ready}`: the claim is in the runner.

        Proves that processor's download path (and closes its breaker), and
        forgets the job's download returns: it downloads fine.
        """
        slot = entry.owner
        processor_id = slot.processor_id if slot is not None else LOCAL_SLOT
        label = self._slot_label(slot)
        reopened = False
        self._restamp(entry.job)
        with self._lock:
            self._download_returns.pop(entry.job, None)
            if processor_id != LOCAL_SLOT:
                breaker = self._breakers.setdefault(processor_id, _DownloadBreaker(label))
                reopened = breaker.open_until > 0
                breaker.proven = True
                breaker.consecutive = 0
                breaker.open_until = 0.0
                breaker.hold = DOWNLOAD_BREAKER_HOLD
                breaker.last_error = ""
                if reopened:
                    self._bump_queue_generation()
                    self._lock.notify_all()
        if reopened:
            self._log(f"{label} fetched an archive again; no longer held")
            processor_entry = self._slot_entry(slot) if slot is not None else None
            if processor_entry is not None:
                processor_entry.note_breaker(None)
        requests = _as_int(event.get("requests")) or 1
        anomalous = (
            requests > 1
            or (_as_int(event.get("restarts")) or 0) > 0
            or (_as_int(event.get("repairs")) or 0) > 0
            or bool(event.get("verdict"))
        )
        size = _as_float(event.get("bytes")) or 0.0
        rate = _as_float(event.get("mb_per_s"))
        line = (
            f"{label} fetched {self._rel_library_path(entry.job[0])} after {requests} "
            f"request{'s' if requests != 1 else ''}: {size / 1e6:.1f} MB"
            + (f" at {rate:.1f} MB/s" if rate is not None else "")
            + (f", {event.get('restarts')} restart(s)" if event.get("restarts") else "")
            + (f", {event.get('repairs')} repair(s)" if event.get("repairs") else "")
            + (f"; {event.get('verdict')}: {event.get('damaged')}" if event.get("verdict") else "")
        )
        if anomalous:
            self._log(line)
        else:
            logger.debug("%s", line)

    def _judge_returned(self, entry: _SessionJob, event: Mapping[str, Any]) -> None:
        """A claim its processor could not deliver: what, if anything, it means.

        The processor never records anything and never fails a volume; the
        library decides, from what only it can see -- its own file (design
        section 6.2):

        1. the file is gone: released, nothing counted;
        2. the file changed since the op was built: released for a fresh
           claim (which sends a fresh size), nothing counted;
        3. ``stalled``/``differs`` and the library cannot read its own copy
           either: RECORDED, with the OS error -- the one deliberate
           difference from the local road, where a bad sector would blank a
           page;
        4. otherwise unrecorded -- no attempt, no backoff, no strike -- but
           counted: per processor (the breaker) and, once that processor's
           path has proven itself, per job; the job's third counted return
           records it as "download failed".
        """
        slot = entry.owner
        klass = str(event.get("class") or "local")[:40]
        error = str(event.get("error") or "")[:300]
        machine = entry.hardware
        label = self._slot_label(slot)
        path = entry.job[0]
        if self._stop_requested:
            self.release_ocr_job(
                entry.job, entry.generation, reason="the worker is stopping", slot=slot
            )
            return
        try:
            st = path.stat()
        except FileNotFoundError:
            self.release_ocr_job(
                entry.job, entry.generation,
                reason=f"{label}: the archive is gone", slot=slot,
            )
            return
        except OSError:
            st = None
        size = entry.volume.archive_size
        if st is not None and size is not None and st.st_size != size:
            self.release_ocr_job(
                entry.job, entry.generation,
                reason=f"{label}: the archive changed after it was sent",
                retry_this_scan=True, slot=slot,
            )
            return
        if klass in ("stalled", "differs"):
            unreadable = read_own_copy(path)
            if unreadable is not None:
                self.finish_ocr_job(
                    entry.job, entry.generation, ok=False,
                    failure=self._failure(
                        f"the library cannot read its own copy of this archive: {unreadable}",
                        entry.volume.log,
                    ),
                    slot=slot,
                )
                return
        job_counted = self._note_download_return(slot, klass, error)
        counted = job_counted and klass != "no_room"
        returns = self._note_job_return(entry.job, st, klass, error, machine, counted=counted)
        if counted and returns.count >= DOWNLOAD_RETURN_LIMIT:
            summary = (
                f"download failed on {returns.count} tries ({', '.join(returns.machines)}): "
                f"{klass}: {error}"
            )
            with self._lock:
                self._download_returns.pop(entry.job, None)
            self.finish_ocr_job(
                entry.job, entry.generation, ok=False,
                failure=self._failure(summary, entry.volume.log), slot=slot,
            )
            return
        if slot is not None and slot.processor_id != LOCAL_SLOT:
            with self._lock:
                self._returned_by.setdefault(entry.job, set()).add(slot.processor_id)
        self.release_ocr_job(
            entry.job, entry.generation,
            reason=f"{label} could not fetch the archive ({klass}): {error}",
            retry_this_scan=True, slot=slot,
        )

    def _note_download_return(self, slot: _OcrSlot | None, klass: str, error: str) -> bool:
        """Count one return against its processor. Whether the JOB counts too.

        The job counts only for a processor that has delivered an archive in
        this registration (its path works) and had no return pending before
        this one: evidence the failure is about this job, not the path. A
        run of ``DOWNLOAD_BREAKER_RETURNS`` opens the breaker; ``changed``
        never counts against a processor -- it is the file's.
        """
        if slot is None or slot.processor_id == LOCAL_SLOT:
            return False
        label = self._slot_label(slot)
        now = time.time()
        opened: tuple[float, float, str, int] | None = None
        with self._lock:
            breaker = self._breakers.setdefault(slot.processor_id, _DownloadBreaker(label))
            job_counted = breaker.proven and breaker.consecutive == 0
            if klass == "changed":
                return job_counted
            breaker.consecutive += 1
            breaker.last_error = f"{klass}: {error}"[:300]
            if breaker.consecutive >= DOWNLOAD_BREAKER_RETURNS and not breaker.is_open(now):
                hold = breaker.hold
                breaker.open_until = now + hold
                breaker.hold = min(hold * 2, DOWNLOAD_BREAKER_MAX_HOLD)
                opened = (hold, breaker.open_until, breaker.last_error, breaker.consecutive)
                self._bump_queue_generation()
                self._lock.notify_all()
        if opened is not None:
            hold, until, last, run = opened
            self._log(
                f"Holding {label} for {hold / 60:.0f} min: {run} archive "
                f"downloads in a row failed (last: {last})"
            )
            processor_entry = self._slot_entry(slot)
            if processor_entry is not None:
                processor_entry.note_breaker(until, last)
        return job_counted

    def _note_job_return(
        self,
        job: tuple[Path, str],
        st: os.stat_result | None,
        klass: str,
        error: str,
        machine: str,
        *,
        counted: bool,
    ) -> _Returns:
        """Remember a job's return (for the queue page); count it if ``counted``.

        A different file under the same name (its size or mtime moved)
        starts the count again.
        """
        stamp = (st.st_size, st.st_mtime_ns) if st is not None else None
        with self._lock:
            returns = self._download_returns.get(job)
            if returns is None or (stamp is not None and returns.stamp != stamp):
                returns = _Returns(stamp=stamp)
                self._download_returns[job] = returns
            if counted:
                returns.count += 1
                returns.machines.append(machine)
            returns.klass, returns.error, returns.machine = klass, error, machine
            returns.at = time.time()
            self._bump_queue_generation()
            return returns

    @staticmethod
    def _busy_reason(event: Mapping[str, Any]) -> str | None:
        """Why a volume (or the live window) ran on a busy host, or None (F5).

        Either signal is enough: the runner's CPU pressure (tasks waiting for
        a CPU) or ``other_cpu`` (what processes that are not OCR used of the
        host's CPU). A runner too old to send one is simply not judged by it.
        """
        pressure = _as_float(event.get("cpu_pressure"))
        if pressure is not None and pressure >= CONTENDED_CPU_PRESSURE:
            return f"CPU pressure {pressure:.0%}"
        others = _as_float(event.get("other_cpu"))
        if others is not None and others >= CONTENDED_OTHER_CPU:
            return f"other processes used {others:.0%} of the CPU"
        return None

    @classmethod
    def _contended(cls, event: Mapping[str, Any]) -> bool:
        """Whether a finished volume ran on a busy host (F5)."""
        return cls._busy_reason(event) is not None

    def _session_progress(self, entry: _SessionJob) -> None:
        """One in-flight volume's own progress card, from its own page events.

        The window a rate is measured over starts at this volume's FIRST page
        event and nowhere earlier. A served engine can hold a hundred pages in
        flight and emit their results in a burst, so the gap between
        `volume_started` and the first `page` is pipeline fill -- measuring
        across it was what made a twelve-page volume that finishes in eight
        seconds open with an ETA over a minute.
        """
        from mokuro_bunko.ocr.processor import OCRProcessor

        total = entry.total_pages or 0
        now = time.time()
        since_first = now - entry.first_page_at if entry.first_page_at else 0.0
        # That machine's rate, falling back as the queue's lanes do
        # (`RateModel.rate_on`): a processor's first volume is priced from
        # whatever else has measured the row, not from nothing.
        estimate = self.rates.rate_on(
            entry.generation.id,
            entry.rate_key,
            observed_pages=entry.done_pages,
            observed_seconds=since_first,
        )
        percent, eta_seconds, status = OCRProcessor._progress_metrics(
            done=entry.done_pages,
            total_images=total,
            elapsed=since_first,
            rate=estimate.pages_per_second if estimate is not None else None,
        )
        self._set_owned_progress(
            entry.job,
            owner=entry.owner,
            data={
                "percent": percent,
                "eta_seconds": eta_seconds,
                "done_pages": entry.done_pages,
                "total_pages": total or None,
                "status": status,
                "generation_id": entry.generation.id,
                "slot": entry.slot,
                "first_page_at": entry.first_page_at,
                "session_started_at": entry.clock.started_at,
                "session_ready": entry.clock.ready_at is not None,
                "delivered": entry.delivered,
                "rate_pages_per_second": (
                    round(estimate.pages_per_second, 4) if estimate is not None else None
                ),
                "latency_seconds": (
                    round(estimate.latency_seconds, 2) if estimate is not None else None
                ),
                "rate_source": estimate.source if estimate is not None else None,
            },
        )

    def _collect_session_volume(
        self, entry: _SessionJob, event: dict[str, Any], first_of_session: bool = False
    ) -> bool:
        """Move one finished sidecar into the library and close its job.

        Nothing is installed for a claim this session's slot no longer holds:
        its processor disconnected, the claim went back to the queue, and a
        file written now would land beside the one the next owner writes (the
        library files a second copy under a new name rather than overwrite).
        """
        if not self._take_for_settling(entry.job, entry.owner):
            self._ignore_late(entry.job, entry.generation, entry.owner)
            shutil.rmtree(entry.volume.workspace, ignore_errors=True)
            return False
        if not self._archive_still_current(entry.job):
            # Deleted or replaced while it was read (here or on a processor):
            # the sidecar describes a file that is not there. Nothing is the
            # volume's fault; a replaced archive is queued again as new.
            self._record_rejected(
                entry.job[0], entry.generation, entry.owner, _DISCARDED, entry.hardware
            )
            shutil.rmtree(entry.volume.workspace, ignore_errors=True)
            self.release_ocr_job(
                entry.job, entry.generation, reason=_DISCARDED, retry_this_scan=True
            )
            return False
        processor = self.processor
        # Read before the install normalizes it (the runner's own stamp), and
        # where it will land (the install's own rule), for the record.
        facts = read_sidecar_facts(entry.volume.output) if self.provenance else None
        destination = processor.session_sidecar_destination(
            entry.job[0], entry.generation, entry.volume.output
        )
        try:
            error = processor.install_session_sidecar(
                entry.job[0], entry.generation, entry.volume.output
            )
        except Exception:
            # Settled as a failure below all the same; only the hold on the
            # claim has to be let go here if the install itself raised.
            with self._lock:
                self._settling.discard(entry.job)
            raise
        if error is not None:
            self._record_rejected(
                entry.job[0], entry.generation, entry.owner, error, entry.hardware
            )
        elif self.provenance is not None:
            self.provenance.written(
                sidecar=destination,
                cbz=entry.job[0],
                generation=entry.generation,
                machine=entry.hardware,
                account=self._slot_account(entry.owner),
                facts=facts,
                pages=_as_int(event.get("pages")),
                failed_pages=_as_int(event.get("failed_pages")),
                build=self._slot_build(entry.owner, entry.generation, facts, through_runner=True),
                archive_stamp=self._archive_stamp(entry.job[0]),
            )
        pipeline: dict[str, Any] | None = None
        if error is None:
            # The congestion history is THIS machine's (the admin table's
            # "average of the last few runs" for its own pools): a
            # processor's pipeline numbers went to its profile instead.
            summary = (
                summarize_event_stats(event.get("stats"))
                if entry.hardware == LOCAL_SLOT and not self._contended(event)
                else None
            )
            if summary is not None:
                pipeline = build_record(
                    summary,
                    volume=self._rel_library_path(entry.job[0]),
                    # The runner's own count of this volume's pages and its
                    # own timing of them, kept beside the pipeline's item
                    # counters: an item is a unit of stage work, not a page,
                    # and a rate fitted from items is not a page rate.
                    volume_pages=_as_int(event.get("pages")),
                    volume_seconds=_as_float(event.get("seconds")),
                    volume_first=first_of_session,
                )
            self._set_owned_progress(
                entry.job,
                {
                    "percent": 100,
                    "eta_seconds": 0,
                    "done_pages": _as_int(event.get("pages")) or entry.done_pages,
                    "status": "done",
                },
                entry.owner,
            )
        self.finish_ocr_job(
            entry.job,
            entry.generation,
            ok=error is None,
            failure=None if error is None else self._failure(error, entry.volume.log),
            pipeline=pipeline,
            slot=entry.owner,
        )
        shutil.rmtree(entry.volume.workspace, ignore_errors=True)
        return error is None

    def _session_exit_error(
        self,
        session: OcrSession,
        generation: GenerationSpec,
        event: dict[str, Any],
        fatal_error: str | None,
        inflight: dict[str, _SessionJob],
    ) -> str | None:
        """The reason a session's exit is a crash, or None when it was clean."""
        code = event.get("returncode")
        if session.closing and not inflight and (code in (0, None)):
            return None
        if session.killed:
            return fatal_error
        if not inflight and code in (0, None):
            return None
        detail = fatal_error or session.stderr_tail()
        return (
            f"the {generation.name} runner exited"
            + (f" with status {code}" if code not in (None, 0) else "")
            + (f": {detail}" if detail else " before its volumes were finished")
        )

    def _end_session(
        self,
        session: OcrSession,
        inflight: dict[str, _SessionJob],
        order: list[str],
        generation: GenerationSpec,
        fatal_error: str | None,
        completed: int,
        *,
        hardware: str = LOCAL_SLOT,
        ready: bool = True,
    ) -> None:
        """Close the runner and settle whatever was still in flight.

        A clean end has nothing in flight and nothing to settle. Anything
        else is a death, and the accounting is deliberate: the OLDEST
        unfinished volume is the one recorded as a failure, because it is the
        one the runner was working on when it died and the only one there is
        any evidence against; every other volume it had accepted goes back to
        the queue untouched. Blaming all of them would give a whole
        lookahead's worth of innocent volumes an attempt and a backoff for
        one crash.

        A session whose PROCESSOR left is nobody's failure either, and not
        the row's: its claims were returned when the processor dropped
        (`processor_disconnected`), so what is settled here is settled as
        nothing, and no strike counts against a row whose environment never
        broke. Nor does a session WE killed for a settings change or a
        benchmark: that end is ours, not the row's.

        A processor's runner that died BEFORE it was ready (``ready`` False:
        it never loaded its models -- a CUDA library that no longer matches
        the driver, a model that is not there) failed on its environment,
        not on a volume, and nothing is blamed: the volumes go back for
        healthy hardware to read. Every strike is the (row, machine) pair's
        (``hardware``), so one broken processor stops the row on itself and
        nowhere else.
        """
        if not session.closing and session.is_alive():
            session.close()
        if session.wait(timeout=30.0) is None:
            session.kill()
            session.wait(timeout=5.0)
        session.join_reader(timeout=5.0)
        entry_of = getattr(session, "entry", None)
        processor_left = entry_of is not None and bool(getattr(entry_of, "dropped", False))
        with self._lock:
            # Killed by a settings change or a benchmark's pre-emption, which
            # mark what they cancel BEFORE the kill: the row did not crash,
            # so the end counts no strike (and the volumes record nothing,
            # `finish_ocr_job`'s cancelled branch).
            cancelled = any(entry.job in self._cancelled_ocr for entry in inflight.values())
        if (
            not ready
            and fatal_error is not None
            and not processor_left
            and not cancelled
            and not self._stop_requested
        ):
            # The runner never became ready: an environment that will not
            # start this row here. Spaced out across scans (F9).
            if entry_of is not None:
                spec: Mapping[str, Any] = getattr(session, "row_spec", None) or {}
                signature = self._start_signature(spec, str(entry_of.processor_id))
            else:
                signature = self._start_signature(
                    self._local_run_row(generation).to_dict(), LOCAL_SLOT
                )
            self._note_start_failure(generation, hardware, signature, fatal_error)
        if not inflight:
            if fatal_error is not None and completed == 0 and not processor_left:
                self._strike_session(generation, fatal_error, hardware)
            return
        error = fatal_error or (
            f"the {generation.name} runner ended before it finished this volume"
        )
        # A shutdown is nobody's failure: everything in flight goes back
        # untouched, with no record and no backoff, and the next start
        # simply finds the volumes still missing their sidecars.
        blame_oldest = not self._stop_requested and not processor_left
        # A runner that refused the row's forced precision (its device cannot
        # run it) is not the volume's failure on ANY machine: eligibility
        # should never have sent it, and healthy hardware will read it.
        environment = (hardware != LOCAL_SLOT and not ready) or _precision_refused(fatal_error)
        # Only a volume the RUNNER had can be blamed for its death (design
        # section 6.3). A processor delivers in arrival order, so the
        # delivered claims are a prefix of `order`: if the oldest was never
        # delivered -- still downloading, or backing off -- nothing was, and
        # every claim goes back unrecorded.
        oldest = (
            order[0]
            if (
                order
                and blame_oldest
                and not environment
                and order[0] in inflight
                and inflight[order[0]].delivered
            )
            else None
        )
        for job_id in list(order):
            entry = inflight.get(job_id)
            if entry is None:
                continue
            if job_id == oldest:
                self.finish_ocr_job(
                    entry.job,
                    entry.generation,
                    ok=False,
                    failure=self._failure(error, entry.volume.log),
                    slot=entry.owner,
                )
            else:
                self.release_ocr_job(
                    entry.job,
                    entry.generation,
                    reason=error,
                    retry_this_scan=True,
                    slot=entry.owner,
                )
            shutil.rmtree(entry.volume.workspace, ignore_errors=True)
        inflight.clear()
        order.clear()
        if not blame_oldest or cancelled:
            return
        if completed == 0:
            self._strike_session(generation, error, hardware)
        else:
            with self._lock:
                self._session_strikes.pop((generation.id, hardware), None)

    def _strike_session(
        self, generation: GenerationSpec, error: str, hardware: str = LOCAL_SLOT
    ) -> None:
        """Count a session that died without finishing anything; stop at two.

        Two in a row is not bad luck, it is a broken environment, and every
        further attempt pays a model load to fail the same way. The row is
        given up on for the rest of THIS scan only, and on THIS machine only
        -- another processor, or this server, has an environment of its own
        -- because the usual fix (finishing an install, freeing a card)
        happens between scans and nothing should need a restart to be picked
        up.
        """
        key = (generation.id, hardware)
        with self._lock:
            strikes = self._session_strikes.get(key, 0) + 1
            self._session_strikes[key] = strikes
            stop = strikes >= SESSION_CRASH_LIMIT
            if stop:
                self._stopped_generations.add(key)
                self._bump_queue_generation()
                self._lock.notify_all()
        if stop:
            where = "" if hardware == LOCAL_SLOT else f" on {hardware}"
            self._log(
                f"Stopping {generation.name}{where} for this scan: {strikes} sessions in a "
                f"row ended without finishing a volume ({error})"
            )

    def _run_ocr_job(self, job: tuple[Path, str], slot: _OcrSlot) -> None:
        """Run one claimed (CBZ, generation) job and record the outcome.

        The job runs with the row `claim_next` froze onto the slot, so a
        settings change landing mid-run cannot move the file it writes, the
        log it writes to or the record its outcome goes under.
        """
        generation = slot.generation
        if generation is None:
            return
        self.begin_ocr_job(
            job, generation, slot=slot.index, total_pages=self._page_count(job[0]),
            processor=self._slot_processor_label(slot), owner=slot,
            machine=self._slot_hardware(slot),
        )
        ok = False
        slot.processor.last_discarded = False
        slot.processor.last_written = None
        slot.processor.publish_guard = lambda: self._archive_still_current(job)
        # A cancel that lands before this job's subprocess exists -- after
        # the claim, with nothing yet to kill -- is honoured when it starts:
        # the processor asks this set, which every canceller writes first.
        begin_job = getattr(slot.processor, "begin_job", None)
        if begin_job is not None:
            begin_job()
        slot.processor.cancel_check = lambda: self._job_cancelled(job)
        self._restamp(job)
        try:
            ok = slot.processor.process_library_ocr(job[0], generation)
        finally:
            pipeline = slot.processor.last_pipeline
            slot.processor.last_pipeline = None
            slot.processor.publish_guard = None
            slot.processor.cancel_check = None
            written = getattr(slot.processor, "last_written", None)
            slot.processor.last_written = None
            if getattr(slot.processor, "last_discarded", False):
                # A replaced archive is a new file: offered again this scan.
                self._record_rejected(job[0], generation, slot, _DISCARDED)
                self.release_ocr_job(
                    job, generation, reason=_DISCARDED, retry_this_scan=True, slot=slot
                )
            else:
                if ok and written is not None and self.provenance is not None:
                    self.provenance.written(
                        sidecar=written.path,
                        cbz=job[0],
                        generation=generation,
                        machine=self._slot_hardware(slot),
                        account=self._slot_account(slot),
                        facts=written.facts,
                        pages=None,
                        failed_pages=written.failed_pages,
                        build=self._slot_build(
                            slot, generation, written.facts,
                            through_runner=not slot.processor.runs_mokuro_cli(generation),
                        ),
                        archive_stamp=self._archive_stamp(job[0]),
                    )
                self.finish_ocr_job(
                    job,
                    generation,
                    ok=ok,
                    failure=None if ok else slot.processor.last_failure,
                    pipeline=pipeline,
                    slot=slot,
                )
            slot.job = None
            slot.generation = None

    def begin_ocr_job(
        self,
        job: tuple[Path, str],
        generation: GenerationSpec,
        *,
        slot: int | None = None,
        total_pages: int | None = None,
        processor: str | None = None,
        owner: _OcrSlot | None = None,
        machine: str | None = None,
        session_ready: bool = False,
        delivered: bool = True,
    ) -> None:
        """Open a claimed job's progress card.

        Takes the JOB rather than a slot, so that a caller holding several
        jobs in flight in one process (one runner kept open for a row, pages
        of several volumes streaming through it) opens and closes each one's
        card independently.

        ``session_ready`` is whether the runner reading it has already said
        it is ready (a one-shot run never has: its model loads until its
        first page); ``delivered``, whether that runner has the volume. They
        are the card's Loading / Waiting / Running (`shape.job_state`).
        """
        path = job[0]
        try:
            rel_cbz = str(path.relative_to(self.storage_path / "library"))
            rel_series = str(path.parent.relative_to(self.storage_path / "library"))
        except ValueError:
            rel_cbz, rel_series = str(path), ""
        card: dict[str, Any] = {
            "generation": generation.name,
            "generation_id": generation.id,
            # Whose hardware is running it: a processor's label, or None
            # when it is this machine. The queue page renders " · on tower
            # (RTX 4090)" from it.
            "processor": processor,
            "engine": generation.engine,
            "detector": generation.reported_detector,
            "series": rel_series,
            "volume": path.stem,
            "relative_cbz": rel_cbz,
            "percent": 0,
            "eta_seconds": None,
            # Nothing has come out of this volume yet. "starting" says so;
            # "running" with no ETA invited the old elapsed-time guess.
            "status": "starting",
            "session_ready": session_ready,
            "delivered": delivered,
        }
        if slot is not None:
            card["slot"] = slot
        if machine is not None:
            # Whose lane it holds in the queue prediction, and whose rate
            # prices it: LOCAL_SLOT, or the processor's NAME (`_rate_key`).
            card["machine"] = machine
        if total_pages:
            # How long this volume is, from the metadata cache, BEFORE the
            # runner has said anything. Without it the card is unpriceable
            # for the first seconds of every job and so is everything queued
            # behind it; the runner's own count replaces it the moment
            # `volume_started` arrives.
            card["total_pages"] = total_pages
        self._set_owned_progress(job, card, owner)

    def finish_ocr_job(
        self,
        job: tuple[Path, str],
        generation: GenerationSpec,
        *,
        ok: bool,
        failure: OcrFailure | None = None,
        pipeline: dict[str, Any] | None = None,
        slot: _OcrSlot | None = None,
    ) -> None:
        """Record one job's outcome and release it, whoever ran it.

        Every outcome is passed IN rather than read back off a processor, so
        that a caller with several jobs in flight in one process cannot
        attribute one volume's failure or congestion numbers to another. The
        completion signal is the caller's -- a subprocess exit today, a
        runner saying "this volume is done" later.

        ``slot`` is the slot whose outcome this is. Two things follow from
        it, both about a processor that left. A claim the slot no longer
        holds was returned by the disconnect and may be somebody else's now:
        its late outcome is dropped whole. And a failure on a slot whose
        processor is gone is the disconnect's, not the volume's -- the entry
        is marked gone a moment before the listener that returns its claims
        runs, and a session start refused in that moment must not become a
        failure record with a backoff.
        """
        path = job[0]
        if not self._take_for_settling(job, slot):
            self._ignore_late(job, generation, slot)
            return
        returned = not ok and slot is not None and self._slot_gone(slot)
        if not returned and job not in self._cancelled_ocr:
            # Done, or recorded: its download returns are history either way.
            with self._lock:
                self._download_returns.pop(job, None)
        try:
            if ok:
                self._forget_candidate(job)
                self._clear_ocr_failure(path, generation)
                self._record_congestion(generation, pipeline)
            elif returned:
                self._log(
                    f"Returned {generation.name} for {path.name} to the queue: "
                    f"{self._slot_label(slot)} disconnected"
                )
            elif job in self._cancelled_ocr:
                # Killed by a settings change or a benchmark's pre-emption:
                # the volume did nothing wrong, so no failure record and no
                # exponential backoff.
                self._log(
                    f"Skipped {generation.name} for {path.name}: "
                    "cancelled, not a failure of the volume"
                )
            elif self._generation(generation.id) is not None:
                self._record_ocr_failure(path, generation, failure)
            else:
                # The row went while the job ran: not a failure of the volume.
                self._log(
                    f"Skipped {generation.name} for {path.name}: "
                    "the generation is no longer configured"
                )
        finally:
            with self._lock:
                self._settle(job)
                if returned:
                    # Re-offered in THIS scan, like any returned claim.
                    self._attempted_ocr.discard(job)
                # Done or failed, the job left the pending list when it was
                # claimed and does not come back (a failure waits out its
                # backoff): the cached list stays right. Returned, it does.
                self._bump_queue_generation(keep_cache=not returned)
                # A volume just came free: wake any slot waiting for one.
                self._lock.notify_all()
            self._clear_active_progress(job)

    def _take_for_settling(self, job: tuple[Path, str], slot: _OcrSlot | None) -> bool:
        """Whether ``slot`` may record this job's outcome -- and if so, hold it.

        True with no slot (a caller that tracks no ownership) and for the
        slot that claimed it. From then until `_settle`, a disconnect leaves
        the claim to this slot rather than returning it mid-write. False
        once the claim was returned: it went back to the queue, and may
        already be running on another slot.
        """
        if slot is None:
            return True
        with self._lock:
            if self._claim_owner.get(job) is not slot:
                return False
            self._settling.add(job)
            return True

    def _settle(self, job: tuple[Path, str]) -> None:
        """Forget a claim everywhere it is tracked. Called under the lock."""
        self._inflight_ocr.discard(job)
        self._cancelled_ocr.discard(job)
        self._job_stamps.pop(job, None)
        self._settling.discard(job)
        self._claim_owner.pop(job, None)

    def _ignore_late(
        self, job: tuple[Path, str], generation: GenerationSpec, slot: _OcrSlot | None
    ) -> None:
        self._log(
            f"Ignored a late {generation.name} outcome for {job[0].name}: "
            f"{self._slot_label(slot)} disconnected and the volume went back to the queue"
        )

    def _record_congestion(
        self, generation: GenerationSpec, record: dict[str, Any] | None
    ) -> None:
        """Append a completed run's pool/queue numbers to its row's history.

        Only a COMPLETED run: a cancelled or failed one measured a pipeline
        that was stopped. Keyed by the row's immutable id, so a rename keeps
        the history; rows that no longer exist are pruned in the same write.
        """
        if record is None:
            return
        with self._lock:
            self._congestion.record(
                generation.id, record, known_ids=[row.id for row in self.generations]
            )

    def _scan_thumbnails_once(self) -> None:
        """Process library CBZ files with missing thumbnails."""
        thumb_candidates = self._thumbnail_candidates()
        if thumb_candidates:
            self._log(f"Found {len(thumb_candidates)} CBZ files missing thumbnails")

        for path in thumb_candidates:
            with self._lock:
                if path in self._inflight_thumbs:
                    continue
                self._inflight_thumbs.add(path)
            try:
                self.processor.process_library_thumbnail(path)
            finally:
                with self._lock:
                    self._inflight_thumbs.discard(path)

    def _wait_poll_interval(self) -> None:
        """Sleep for poll interval with stop checks."""
        for _ in range(max(1, int(self.poll_interval * 10))):
            if not self._running:
                break
            if self._wake.is_set():
                self._wake.clear()
                break
            time.sleep(0.1)

    def _run_ocr_loop(self) -> None:
        """Background OCR sidecar loop.

        A held queue skips the scan itself rather than starting one that can
        claim nothing: the scan's first act is to walk every sidecar in the
        library, and repeating that every poll interval for the length of a
        benchmark would be the loudest thing on the disk while something is
        trying to measure the machine.
        """
        while self._running:
            self._touch_heartbeat()
            if not self._every_machine_held() and not self._held_for_hardware():
                try:
                    self._scan_ocr_once()
                except Exception as e:
                    self._log(f"OCR scan error: {e}")
            self._wait_poll_interval()

    def _held_for_hardware(self) -> bool:
        """True while there is nothing to run OCR on, said once per hold.

        The same reasoning as a held queue: a scan that can claim nothing
        would still walk every sidecar in the library, and log how many are
        missing, every poll interval for as long as nobody is logged in.
        """
        hold = self.processing_hold()
        if hold is None:
            if self._hold_logged:
                self._log("OCR hardware available again; the queue resumes")
            self._hold_logged = False
            return False
        if not self._hold_logged:
            last = hold.get("last")
            self._log(
                "OCR queue held: local processing is off and no processor is connected"
                + (f" (last: {last['name']})" if isinstance(last, dict) else "")
            )
            self._hold_logged = True
        return True

    def _run_thumbnail_loop(self) -> None:
        """Background thumbnail loop."""
        while self._running:
            try:
                self._scan_thumbnails_once()
            except Exception as e:
                self._log(f"Thumbnail scan error: {e}")
            self._wait_poll_interval()

    def _remove_corrupt_sidecars(self) -> int:
        """Remove invalid mokuro sidecar files from library and return count."""
        library_path = self.storage_path / "library"
        removed = 0
        for path in sorted(library_path.rglob("*.mokuro*")):
            if not path.is_file():
                continue
            if not (path.name.endswith(".mokuro") or path.name.endswith(".mokuro.gz")):
                continue
            if self.processor.is_valid_mokuro_sidecar(path):
                continue
            try:
                path.unlink()
                removed += 1
                self._log(f"Removed corrupt mokuro sidecar: {path}")
                if self.provenance is not None:
                    self.provenance.forget(path)
            except OSError as e:
                self._log(f"Failed to remove corrupt sidecar {path}: {e}")
        return removed

    def start(self, background: bool = True) -> None:
        """Start the OCR worker.

        Args:
            background: If True, run in background thread.
        """
        library_path = self.storage_path / "library"
        library_path.mkdir(parents=True, exist_ok=True)

        if not self.thumbnails_only:
            removed = self._remove_corrupt_sidecars()
            if removed:
                self._log(f"Removed {removed} corrupt mokuro sidecar file(s) at startup")

        self._running = True
        self._log("Cover worker starting..." if self.thumbnails_only else "OCR worker starting...")

        self._thumb_thread = threading.Thread(
            target=self._run_thumbnail_loop,
            daemon=True,
            name="ocr-thumbnail-worker",
        )

        if self.thumbnails_only:
            self._thumb_thread.start()
            self._log("Cover worker started in background (thumbnail loop only)")
            return

        if background:
            self._ocr_thread = threading.Thread(
                target=self._run_ocr_loop,
                daemon=True,
                name="ocr-sidecar-worker",
            )
            self._ocr_thread.start()
            self._thumb_thread.start()
            self._log("OCR worker started in background (sidecar + thumbnail loops)")
        else:
            self._thumb_thread.start()
            self._run_ocr_loop()

    def stop(self) -> None:
        """Stop the OCR worker, leaving no runner behind.

        Open sessions are asked to end and then killed: waiting for the
        volumes they have accepted would make a shutdown as long as a volume,
        and those volumes are released rather than failed, so nothing is
        recorded against an archive that did nothing wrong. The kill is
        issued from HERE as well as from the slot threads, because a slot
        wedged on a runner that stopped reading is exactly the case that
        would otherwise orphan a subprocess.
        """
        self._running = False
        self._stop_requested = True
        with self._lock:
            self._bump_queue_generation()
            self._lock.notify_all()
            sessions = list(self._open_sessions)
        for session in sessions:
            session.close()
        for session in sessions:
            if session.wait(timeout=2.0) is None:
                session.kill()
        if self._ocr_thread:
            self._ocr_thread.join(timeout=5.0)
        if self._thumb_thread:
            self._thumb_thread.join(timeout=5.0)
        with self._lock:
            remaining = list(self._open_sessions)
        for session in remaining:
            session.kill()
            session.wait(timeout=2.0)
        self._log("OCR worker stopped")

    @property
    def is_running(self) -> bool:
        """Check if the worker is running."""
        return self._running
