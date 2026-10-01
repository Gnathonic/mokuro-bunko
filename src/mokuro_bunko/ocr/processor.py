"""OCR processor for mokuro-bunko.

Handles running Mokuro on manga files and moving them to the library.
"""

from __future__ import annotations

import glob
import gzip
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import uuid
import zipfile
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from io import BytesIO
from pathlib import Path
from typing import Any, NamedTuple

from mokuro_bunko import __version__
from mokuro_bunko.logging_setup import get_ocr_log_dir
from mokuro_bunko.ocr.congestion import build_record, read_final_stats
from mokuro_bunko.ocr.engine_runner import (
    DEFAULT_PRECISION_MODE,
    PRECISION_FP16,
    STAGE_MOKURO,
    mokuro_placement,
)
from mokuro_bunko.ocr.engines import get_engine
from mokuro_bunko.ocr.eta import RateEstimate, RateModel, emission_rate
from mokuro_bunko.ocr.generations import (
    DEFAULT_GENERATION,
    GenerationSpec,
    default_generations,
    enabled_generations,
    parse_generation_list,
    primary_generation,
)
from mokuro_bunko.ocr.hf_cache import row_models_cached
from mokuro_bunko.ocr.installer import EnginesInstaller, OCRInstaller
from mokuro_bunko.ocr.pipeline_stats import pipeline_stats_path, read_pipeline_stats
from mokuro_bunko.ocr.provenance import (
    WrittenSidecar,
    failed_pages_from_log,
    read_sidecar_facts,
)
from mokuro_bunko.ocr.session import OcrSession, SessionVolume
from mokuro_bunko.ocr.staging import stage_runner

# Supported manga file extensions
SUPPORTED_EXTENSIONS = {".cbz", ".cbr", ".zip", ".rar"}

# How long the serve-module probe may take. It imports the package, which on
# the mokuro environment is torch, so it is not instant.
_SERVE_PROBE_TIMEOUT = 300.0
# (interpreter, module) -> can it be served? Answered ONCE per server, because
# the answer is a property of an installed environment and asking again per
# volume would be an import of torch per volume.
_SERVE_PROBE: dict[tuple[str, str], bool] = {}


def serve_module_available(
    python_path: str, module: str, log: Callable[[str], None] | None = None
) -> bool:
    """Can ``<python> -m <module>`` be imported? Probed once, then remembered.

    This is the ONLY thing the old one-volume CLI path gained: a row whose
    engine declares a serve module but whose installed package has not got one
    (a ``MOKURO_BUNKO_MOKURO_SPEC`` override) keeps that path, and the log
    says why rather than leaving a fallback to be discovered by its symptoms.
    """
    key = (python_path, module)
    known = _SERVE_PROBE.get(key)
    if known is not None:
        return known
    try:
        probe = subprocess.run(  # noqa: S603
            [python_path, "-c", f"import {module}"],
            capture_output=True,
            text=True,
            timeout=_SERVE_PROBE_TIMEOUT,
            check=False,
        )
        ok = probe.returncode == 0
        detail = (probe.stderr or "").strip().splitlines()[-1:] or [""]
    except (OSError, subprocess.SubprocessError) as e:
        ok, detail = False, [str(e)]
    _SERVE_PROBE[key] = ok
    if log is not None:
        log(
            f"{module} is available in {python_path}: pages stream into one process"
            if ok
            else (
                f"{module} is not in {python_path} ({detail[0]}); that engine keeps the "
                "one-volume command-line path"
            )
        )
    return ok


class MokuroRunResult(NamedTuple):
    """Outcome of one mokuro subprocess run."""

    ok: bool
    error: str | None = None
    log_path: Path | None = None

    def __bool__(self) -> bool:
        # A NamedTuple is always truthy; make `if not result:` mean failure.
        return self.ok


@dataclass
class OcrFailure:
    """Details of the most recent OCR failure, for callers to persist."""

    error: str
    log_file: str | None = None


# Patterns used to pull a human-readable reason out of mokuro's output.
# Final line of a Python traceback, e.g. "ValueError: Couldn't instantiate ..."
_TRACEBACK_FINAL_RE = re.compile(
    r"^(?:[A-Za-z_][\w.]*(?:Error|Exception)|KeyboardInterrupt|SystemExit|MemoryError)"
    r"(?::\s?.*)?$"
)
# Any "some.module.ExceptionClass: message" line — only trusted inside a
# "Traceback (most recent call last):" block, so arbitrarily named
# exceptions (e.g. mokuro's InvalidImage) are still caught. The exception
# is the last such line in the block.
_EXCEPTION_LINE_RE = re.compile(r"^[A-Za-z_][\w.]*:\s?.*$")
# loguru error lines, e.g. "2026-07-09 ... | ERROR | mokuro.run:run:142 - message"
_LOGURU_ERROR_RE = re.compile(r"\|\s*ERROR\s*\|.*?-\s*(?P<msg>.+)$")
# mokuro's per-run summary, e.g. "Processed successfully: 0/1"
_PROCESSED_RE = re.compile(r"Processed successfully:\s*(?P<done>\d+)/(?P<total>\d+)")


def _as_generations(value: Sequence[Any]) -> list[GenerationSpec]:
    """Rows as they are when they are already rows, parsed otherwise."""
    rows = list(value)
    if all(isinstance(row, GenerationSpec) for row in rows):
        return rows
    return parse_generation_list(rows)


def _stage_setting(values: Mapping[str, int]) -> str:
    """``{"detect": 4}`` as the runner's ``detect=4`` flag value; "" for none.

    The shape ``--stage-workers`` and ``--queue-capacity`` already take
    (``engine_runner.parse_stage_setting``). An empty map means the runner
    derives the sizes itself, which is what every untuned row does.
    """
    return ",".join(f"{key}={int(value)}" for key, value in sorted(values.items()))


def _device_setting(values: Mapping[str, str]) -> str:
    """``{"detect": "cpu"}`` as the runner's ``--stage-device`` flag value.

    Every key of a row that reaches the RUNNER, ``mokuro`` included: on the
    served road that stage is the serve process the runner spawns, and the
    runner makes it ``--force_cpu`` / ``CUDA_VISIBLE_DEVICES`` there
    (``engine_runner.mokuro_placement``) exactly as
    :meth:`OCRProcessor._mokuro_placement` does for the one-volume CLI
    fallback, which builds its own command line and never comes through here.
    An empty map means ``auto`` everywhere, which is what every untuned row
    does.
    """
    return ",".join(f"{key}={value}" for key, value in sorted(values.items()))


def _precision_args(generation: GenerationSpec, *, pick: bool = True) -> list[str]:
    """``--precision <mode>`` for a row whose mode is not the default, and the
    machine's benchmarked pick for a balanced/speed mode (``--precision-pick``).

    The default mode is the runner's own default, so it goes as nothing. A
    benchmark (``pick=False``) is sent the mode alone: finding the pick is
    what it is for.
    """
    if not generation.precision_applies:
        return []
    args: list[str] = []
    if generation.precision != DEFAULT_PRECISION_MODE:
        args += ["--precision", generation.precision]
    if pick and generation.precision_pick is not None:
        args += ["--precision-pick", generation.precision_pick]
        if generation.precision_why:
            args += ["--precision-why", generation.precision_why]
    return args


class OCRProcessor:
    """Processes manga files with Mokuro OCR."""

    def __init__(
        self,
        storage_path: Path,
        python_path: Path | None = None,
        status_callback: Callable[[str], None] | None = None,
        progress_callback: Callable[[dict[str, Any]], None] | None = None,
        generations: Sequence[GenerationSpec] | None = None,
        engines_python_path: Path | None = None,
        concurrency: int = 1,
        staged_runner: Path | None = None,
    ) -> None:
        """Initialize the OCR processor.

        Args:
            storage_path: Base storage path containing inbox/ and library/.
            python_path: Path to Python executable with mokuro installed.
                        If None, auto-detects from OCRInstaller.
            status_callback: Optional callback for status messages.
            progress_callback: Optional callback for OCR progress updates.
            generations: The configured OCR recipes, IN RUN ORDER (default:
                one mokuro row). Every library volume needs a sidecar for
                each enabled row; the first enabled row also runs on inbox
                uploads and is the one that keeps normal OS priority.
            engines_python_path: Python executable of the engines
                environment (hayai-nova / paddle-manga / ppocr-manga). If
                None, auto-detects from EnginesInstaller.
            concurrency: How many OCR jobs the worker runs at once
                (``ocr.concurrency``), passed to the engine runner as
                ``MOKURO_OCR_JOBS`` so it sizes its own per-stage thread
                pools against this run's SHARE of the host rather than the
                whole of it. Must agree with the worker's, or N concurrent
                jobs each budget for all the cores and oversubscribe.
            staged_runner: A runner build already staged and held for the
                life of the caller's process. Every session and benchmark
                runs exactly this build and nothing is ever restaged. A
                PROCESSOR pins one at start (design section 5.1), because its
                bridge was imported once and must never drive a runner newer
                than itself; the library leaves it None and restages per
                session, as it always has.
        """
        self.storage_path = storage_path
        self.staged_runner = Path(staged_runner) if staged_runner is not None else None
        self.inbox_path = storage_path / "inbox"
        self.library_path = storage_path / "library"
        self.status_callback = status_callback or (lambda msg: None)
        self.progress_callback = progress_callback or (lambda data: None)
        # Details of the most recent failure (set by processing methods when
        # they return False), so callers can persist/display the reason.
        self.last_failure: OcrFailure | None = None
        # The final pool/queue numbers of the most recent SUCCESSFUL job,
        # harvested before its workspace is removed, for the caller to fold
        # into that generation's congestion history. None when the run had no
        # staged pipeline (it went through the one-volume command-line path)
        # or published nothing.
        self.last_pipeline: dict[str, Any] | None = None
        # True when the last `process_library_ocr` dropped its result because
        # the archive was deleted or replaced while it ran (not a failure).
        self.last_discarded = False
        # Asked just before `process_library_ocr` writes a sidecar: whether
        # the archive is still the one its job was claimed from. Set per job
        # by the worker (each slot owns its processor); None: never asked.
        self.publish_guard: Callable[[], bool] | None = None
        # What the last `process_library_ocr` installed, for the caller to
        # record who wrote it (`ocr.provenance`); None when it wrote nothing.
        # Single-valued like `last_pipeline`: each slot owns its processor.
        self.last_written: WrittenSidecar | None = None
        # Already-validated rows are taken as they are. They must be: the
        # server narrows the list as each environment fails to install, and
        # a host that could not install mokuro legitimately ends up with no
        # primary row -- which `parse_generation_list` refuses, and rightly,
        # for a CONFIG. Anything else (a dict, JSON text) is parsed.
        self.generations: list[GenerationSpec] = (
            _as_generations(generations) if generations else default_generations()
        )
        self.concurrency: int = max(1, int(concurrency))
        # "How many pages is this volume's archive short of what its .mokuro
        # names?" -- answered from the metadata pass's cache by whoever owns
        # the database (the server wires it; None = never skip). See
        # `missing_generations`.
        self.missing_pages_lookup: Callable[[Path], int] | None = None
        # "What id did this archive's primary `.mokuro` last carry?" -- asked
        # with the archive's library-relative path, answered from the app's
        # database (`Database.remembered_volume_uuid`) by whoever owns it.
        # None = nothing remembered, which a bare processor in a test is.
        self.volume_uuid_lookup: Callable[[str], str | None] | None = None
        # How fast each row reads a page, and what opening a session for it
        # costs (`ocr.eta.RateModel`). The worker owns one and hands it to
        # every slot's processor; None leaves the progress readout with no
        # rate of its own, which is what a bare processor in a test has.
        self.rates: RateModel | None = None
        # Where a finished one-volume run is counted as this machine's
        # (``OCRWorker`` points it at this server's own profile): ``(row,
        # pages, seconds)``. None: counted nowhere.
        self.run_recorder: Callable[[GenerationSpec, int, float], None] | None = None
        # The OCR subprocess currently running, so a settings change can end
        # a job whose engine was just removed (see cancel_active).
        self._active_process: subprocess.Popen[Any] | None = None
        self._cancel_requested = False
        # Guards `_active_process` and `_cancel_requested` together, so a
        # cancel and a start cannot pass each other (`_start_process`).
        self._process_lock = threading.Lock()
        # The worker's own record of whether THIS job was cancelled (set per
        # job: its `_cancelled_ocr`, which a settings change or a benchmark's
        # pre-empt writes BEFORE it asks this processor to cancel). Asked
        # before and right after the subprocess starts, so a cancel that came
        # while there was nothing to kill is still honoured.
        self.cancel_check: Callable[[], bool] | None = None
        # The row as THIS machine runs it, when that is not the row's own
        # table: the worker binds its `_local_run_row` (this server's
        # auto-benchmarked pools for a row nobody configured by hand). Read
        # only where a runner's command line is built -- a session, a
        # one-volume run -- never for a benchmark, which measures exactly
        # the spec it is given. None: the row's own table.
        self.run_row: Callable[[GenerationSpec], GenerationSpec] | None = None

        if python_path:
            self.python_path = python_path
        else:
            # Try to get Python from OCR installer
            installer = OCRInstaller()
            detected = installer.get_python_executable()
            self.python_path = detected or Path(sys.executable)

        if engines_python_path:
            self.engines_python_path: Path | None = engines_python_path
        else:
            self.engines_python_path = EnginesInstaller().get_python_executable()

    def runner_python(self, generation: GenerationSpec) -> Path | None:
        """The interpreter an engine runner for this row starts with, or None.

        The engines environment's when there is one. A row whose engine lives
        in the MOKURO environment (served mokuro) can do without it: the
        runner is stdlib-only until an engine loads, and mokuro's loads
        through ``--mokuro-python`` anyway -- and a mokuro-only server never
        installs an engines environment, since nothing else asks for one.
        """
        if self.engines_python_path is not None:
            return self.engines_python_path
        if generation.mokuro_env:
            return self.python_path
        return None

    def device_probe_python(self) -> Path:
        """An interpreter with torch in it, to ask what devices this host has."""
        return self.engines_python_path or self.python_path

    def _log(self, message: str) -> None:
        """Log a status message."""
        self.status_callback(message)

    def _as_run(self, generation: GenerationSpec) -> GenerationSpec:
        """``generation`` with the pools this machine runs it with (`run_row`)."""
        hook = self.run_row
        if hook is None:
            return generation
        try:
            return hook(generation)
        except Exception as e:  # noqa: BLE001 - tuning never stops a run
            self._log(f"{generation.name}: running the row's own pools ({e})")
            return generation

    # -- which road a row really takes on THIS host ------------------------

    def serves_pages(self, generation: GenerationSpec) -> bool:
        """True when this row streams pages into a serve process of its own.

        The row SAYS it is served (``GenerationSpec.served``); this asks
        whether the package that is installed can be. The two only ever differ
        under a ``MOKURO_BUNKO_MOKURO_SPEC`` override pointing at a mokuro
        without ``mokuro.serve`` -- an older release, or somebody else's fork
        -- and then the row keeps the one-volume CLI it has always had.
        Probed once per interpreter, and said in the log.
        """
        module = get_engine(generation.engine).serve_module
        if module is None:
            return False
        return serve_module_available(str(self.python_path), module, self._log)

    def runs_mokuro_cli(self, generation: GenerationSpec) -> bool:
        """True when this row is one volume, one invocation of its own CLI."""
        return generation.monolithic or (generation.served and not self.serves_pages(generation))

    def can_run(self, generation: GenerationSpec) -> str | None:
        """Why this processor cannot run this row, or None when it can.

        The server already dropped every row whose environment failed to
        install (`run_server`'s `active_generations`), so what is left to
        ask locally is the DEVICE: a saved row may be pinned to a card only a
        processor has (the admin API holds a row to every machine's cards,
        not just this one's), and this box must then leave it to that
        processor rather than fail a session start on a card it has not got.
        A REMOTE processor overrides this with its own catalog.
        """
        from mokuro_bunko.ocr.devices import cached_catalog
        from mokuro_bunko.ocr.precision import row_refusal

        catalog = cached_catalog()
        for stage, device in (generation.pools.stage_device or {}).items():
            if device in ("", "auto", "cpu"):
                continue
            if not catalog.knows(device):
                return f"{stage} is pinned to {device}, which this server does not have"
        # A forced precision this server's card (by its own probe) cannot run.
        return row_refusal(generation, catalog)

    def _served_engine_args(self, generation: GenerationSpec) -> list[str]:
        """The runner's ``--mokuro-python``, for a row whose engine is a process.

        The runner is executed by the ENGINES environment's interpreter and a
        served engine's packages are not in it, so the runner cannot find that
        interpreter for itself: the server, which installed both, hands it
        over. Empty for every other row.
        """
        if not generation.served:
            return []
        return ["--mokuro-python", str(self.python_path)]

    def configure(
        self,
        generations: Sequence[GenerationSpec],
        concurrency: int | None = None,
    ) -> None:
        """Replace the generations list and job count.

        This is called on EVERY slot, including one with a job running, so
        nothing a running job needs may be read from here afterwards: the job
        carries the row it was claimed with (see ``process_library_ocr``).
        """
        self.generations = list(generations) or default_generations()
        if concurrency is not None:
            self.concurrency = max(1, int(concurrency))

    def begin_job(self) -> None:
        """A new job starts on this processor: a cancel asked of the LAST one
        must not cancel it. Cleared here, never when a subprocess starts."""
        with self._process_lock:
            self._cancel_requested = False

    def _cancel_now(self) -> bool:
        check = self.cancel_check
        return self._cancel_requested or (check is not None and bool(check()))

    def _start_process(self, cmd: list[str], **popen_kwargs: Any) -> subprocess.Popen[Any] | None:
        """Start this job's subprocess -- unless it was already cancelled.

        None, and nothing started, when a cancel was asked for before it
        (`cancel_active` with no process to kill, or the worker's
        `cancel_check`). A cancel that lands while `Popen` itself is running
        is honoured the moment the process exists: it is killed before it is
        returned. Either way the caller sees a cancelled run, never a
        process that outlives the request.
        """
        with self._process_lock:
            if self._cancel_now():
                return None
        process = subprocess.Popen(cmd, **popen_kwargs)  # noqa: S603 - argv built here
        with self._process_lock:
            self._active_process = process
            cancelled = self._cancel_now()
        if cancelled:
            self._cancel_requested = True
            try:
                process.kill()
            except OSError:
                pass
        return process

    def cancel_active(self) -> bool:
        """Cancel this processor's job: kill its subprocess, or -- when none
        has started yet -- make sure none does (`_start_process`).

        The run reports a failure with a "cancelled" reason; the caller
        decides whether that counts as a failure of the volume. True when a
        running process was killed.
        """
        with self._process_lock:
            self._cancel_requested = True
            process = self._active_process
        if process is None or process.poll() is not None:
            return False
        try:
            process.kill()
        except OSError:
            return False
        return True

    def _emit_progress(self, data: dict[str, Any]) -> None:
        """Emit OCR progress update."""
        self.progress_callback(data)

    def _record_failure(self, error: str | None, log_path: Path | None) -> None:
        """Remember the most recent failure so callers can persist it."""
        self.last_failure = OcrFailure(
            error=error or "unknown error",
            log_file=str(log_path) if log_path is not None else None,
        )

    @staticmethod
    def get_mokuro_sidecar_paths(cbz_path: Path) -> tuple[Path, Path]:
        """Return the primary sidecar paths (`<Volume>.mokuro[.gz]`) for a CBZ.

        The bare file, whatever generation writes it: it is the volume's
        reader-facing OCR and the uuid every other layer inherits.
        """
        plain = Path(f"{cbz_path.with_suffix('')}.mokuro")
        return plain, Path(f"{plain}.gz")

    @staticmethod
    def get_cover_path(cbz_path: Path) -> Path:
        """Return expected cover thumbnail path for a CBZ file."""
        return cbz_path.with_suffix(".webp")

    def needs_sidecar(self, cbz_path: Path, generation: GenerationSpec) -> bool:
        """Check whether a CBZ file is missing this generation's sidecar."""
        if not cbz_path.is_file() or cbz_path.suffix.lower() != ".cbz":
            return False
        sidecar_plain, sidecar_gz = generation.sidecar_paths(cbz_path)
        return not sidecar_plain.exists() and not sidecar_gz.exists()

    def needs_mokuro_sidecar(self, cbz_path: Path) -> bool:
        """Check whether a CBZ file is missing the primary sidecar."""
        plain, gz = self.get_mokuro_sidecar_paths(cbz_path)
        if not cbz_path.is_file() or cbz_path.suffix.lower() != ".cbz":
            return False
        return not plain.exists() and not gz.exists()

    def missing_generations(self, cbz_path: Path) -> list[GenerationSpec]:
        """Enabled generations whose sidecar this volume lacks, in run order.

        **Every one of them may run now**, the primary's and the layers' alike,
        on as many machines at once: no row waits for another's file. What a
        layer used to wait for was the ``volume_uuid`` it inherited from the
        bare ``<Volume>.mokuro``; every sidecar is now stamped with the
        volume's own id whichever lands first (`volume_uuid_for`).

        The one gate left is the missing-pages rule: every layer of a volume
        whose ``.mokuro`` names pages its archive lacks has the same holes, so
        it gets no ADDITIONAL layers until the file is replaced with a whole
        one (which changes the archive's stamp and makes it an ordinary
        candidate again). Only a ``.mokuro`` that is already there can be
        short -- one uploaded or imported with the volume, judged at once
        (`pages_short`); the primary this server produces is read from the
        archive itself, so a volume without one has nothing to be short against.
        """
        rows = [
            generation
            for generation in enabled_generations(self.generations)
            if self.needs_sidecar(cbz_path, generation)
        ]
        if (
            any(not row.primary for row in rows)
            and not self.needs_mokuro_sidecar(cbz_path)
            and self.pages_short(cbz_path) > 0
        ):
            return [row for row in rows if row.primary]
        return rows

    def pages_short(self, cbz_path: Path) -> int:
        """Pages this volume is known to be missing (0 = whole, or not known).

        The `.mokuro`-against-archive cross-check the metadata pass makes and
        stores; the server's lookup makes it at once for a volume the pass has
        not reached yet (`metadata.compiler.missing_pages_now`), so a supplied
        `.mokuro` is judged before any layer can be claimed. Any trouble
        asking is "not known": a database hiccup must not stall the OCR queue.
        """
        lookup = self.missing_pages_lookup
        if lookup is None:
            return 0
        try:
            return max(0, int(lookup(cbz_path)))
        except Exception:
            return 0

    def skipped_generations(self, cbz_path: Path) -> list[GenerationSpec]:
        """Enabled NON-primary rows this volume lacks and will not get while it is short of pages."""
        if self.needs_mokuro_sidecar(cbz_path) or self.pages_short(cbz_path) <= 0:
            return []
        return [
            generation
            for generation in enabled_generations(self.generations)
            if not generation.primary and self.needs_sidecar(cbz_path, generation)
        ]

    @staticmethod
    def get_nocover_marker_path(cbz_path: Path) -> Path:
        """Return path for the marker that indicates thumbnail extraction was attempted but failed."""
        return cbz_path.with_suffix(".nocover")

    def needs_thumbnail(self, cbz_path: Path) -> bool:
        """Check whether a CBZ file is missing its cover thumbnail."""
        if not cbz_path.is_file() or cbz_path.suffix.lower() != ".cbz":
            return False
        if self.get_cover_path(cbz_path).exists():
            return False
        if self.get_nocover_marker_path(cbz_path).exists():
            return False
        return True

    def _extract_cover_image_data(self, cbz_path: Path) -> bytes | None:
        """Extract the first image (sorted by path) from a CBZ archive."""
        image_extensions = {
            ".jpg",
            ".jpeg",
            ".png",
            ".gif",
            ".bmp",
            ".webp",
            ".tiff",
            ".tif",
        }
        try:
            with zipfile.ZipFile(cbz_path, "r") as zip_file:
                image_files = sorted(
                    name
                    for name in zip_file.namelist()
                    if Path(name).suffix.lower() in image_extensions
                )
                if not image_files:
                    return None
                with zip_file.open(image_files[0]) as img_file:
                    return img_file.read()
        except (zipfile.BadZipFile, OSError, KeyError):
            return None

    def ensure_thumbnail(self, cbz_path: Path) -> bool:
        """Generate a WebP thumbnail constrained within 250x350 preserving aspect ratio."""
        if not self.needs_thumbnail(cbz_path):
            return True

        image_data = self._extract_cover_image_data(cbz_path)
        if image_data is None:
            self._log(f"No cover image found in: {cbz_path.name}")
            self.get_nocover_marker_path(cbz_path).touch()
            return False

        try:
            from PIL import Image, ImageOps
        except ImportError:
            self._log("Thumbnail generation unavailable: Pillow is not installed")
            return False

        output_path = self.get_cover_path(cbz_path)
        try:
            with Image.open(BytesIO(image_data)) as img:
                # Preserve source aspect ratio while constraining to max bounds.
                thumb = ImageOps.contain(
                    img.convert("RGB"), (250, 350), method=Image.Resampling.LANCZOS
                )
                thumb.save(output_path, format="WEBP", quality=85, method=6)
            self._log(f"Created thumbnail: {output_path.name}")
            return True
        except Exception as e:
            self._log(f"Failed to generate thumbnail for {cbz_path.name}: {e}")
            return False

    def _build_temp_workspace(self, name_hint: str) -> Path:
        """Create isolated temporary workspace for processing."""
        processing_root = self.storage_path / ".processing"
        processing_root.mkdir(parents=True, exist_ok=True)
        return Path(tempfile.mkdtemp(prefix=f"{name_hint}_", dir=str(processing_root)))

    def new_workspace(self, name_hint: str) -> Path:
        """A fresh scratch directory for one volume, under ``.processing``.

        The public name for :meth:`_build_temp_workspace`: a REMOTE processor
        builds a workspace for every volume it is sent, and that is an
        ordinary use of this object rather than a reach into it.
        """
        return self._build_temp_workspace(name_hint)

    def _extract_and_clean(self, cbz_path: Path, workspace: Path) -> Path:
        """Extract a CBZ into the workspace and remove embedded thumbnails.

        Some uploaders embed a .webp thumbnail named after the archive
        (e.g. ``Volume 01.webp`` inside ``Volume 01.cbz``).  These confuse
        mokuro into treating them as manga pages.  After extraction the
        matching .webp is deleted so mokuro never sees it.

        Returns the path to the extracted directory.
        """
        extract_dir = workspace / cbz_path.stem
        with zipfile.ZipFile(cbz_path, "r") as zf:
            zf.extractall(extract_dir)

        # Remove embedded thumbnail: top-level .webp matching the archive stem.
        thumb = extract_dir / f"{cbz_path.stem}.webp"
        if thumb.exists():
            self._log(f"Removing embedded thumbnail: {thumb.name}")
            thumb.unlink()

        return extract_dir

    def _collect_workspace_sidecar(self, temp_cbz_path: Path, workspace: Path) -> Path | None:
        """Find generated sidecar in temporary workspace."""
        stem = temp_cbz_path.stem
        candidates = sorted(
            p for p in workspace.rglob(f"{glob.escape(stem)}.mokuro*")
            if p.is_file()
        )
        if not candidates:
            return None
        # Prefer sidecars written at workspace root for cleaner import semantics.
        root_candidates = [p for p in candidates if p.parent == workspace]
        if root_candidates:
            candidates = root_candidates
        preferred = next((p for p in candidates if p.name.endswith(".mokuro.gz")), candidates[0])
        return preferred

    @staticmethod
    def is_valid_mokuro_sidecar(sidecar_path: Path) -> bool:
        """Check whether a mokuro sidecar is parseable JSON."""
        if not sidecar_path.exists() or not sidecar_path.is_file():
            return False
        try:
            if sidecar_path.name.endswith(".mokuro.gz"):
                with gzip.open(sidecar_path, "rt", encoding="utf-8") as f:
                    json.load(f)
            else:
                with sidecar_path.open("r", encoding="utf-8") as f:
                    json.load(f)
            return True
        except (OSError, UnicodeDecodeError, json.JSONDecodeError, gzip.BadGzipFile):
            return False

    def _collect_valid_workspace_sidecar(
        self, temp_cbz_path: Path, workspace: Path, generation: GenerationSpec
    ) -> Path | None:
        """Find generated sidecar in temporary workspace that is valid JSON.

        ``generation`` is the row the JOB was claimed with, not whatever the
        settings say now: the runner was told where to write at argv-build
        time, and looking for a different name here would throw a finished
        volume away and record a failure that never happened.

        The mokuro CLI may write next to the input or at the workspace root
        and may gzip, so it is searched; the engine runner writes exactly
        one known path. Which of the two ran is asked the same way the
        dispatch asked it (:meth:`runs_mokuro_cli`), so a served row that fell
        back to the CLI is still collected from where the CLI put it.
        """
        stem = temp_cbz_path.stem
        if not self.runs_mokuro_cli(generation):
            candidate = workspace / f"{stem}{generation.sidecar_suffix}"
            if self.is_valid_mokuro_sidecar(candidate):
                return candidate
            if candidate.exists():
                self._log(f"Ignoring corrupt {generation.name} sidecar: {candidate.name}")
            return None
        candidates = sorted(
            p for p in workspace.rglob(f"{glob.escape(stem)}.mokuro*")
            if p.is_file()
        )
        if not candidates:
            return None
        root_candidates = [p for p in candidates if p.parent == workspace]
        if root_candidates:
            candidates = root_candidates
        ordered = sorted(candidates, key=lambda p: (not p.name.endswith(".mokuro.gz"), str(p)))
        for candidate in ordered:
            if self.is_valid_mokuro_sidecar(candidate):
                return candidate
            self._log(f"Ignoring corrupt mokuro sidecar: {candidate.name}")
        return None

    def _count_archive_images(self, cbz_path: Path) -> int:
        """Count image files in a CBZ archive."""
        image_extensions = {".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif", ".tif", ".tiff"}
        try:
            with zipfile.ZipFile(cbz_path, "r") as zf:
                return sum(
                    1 for name in zf.namelist() if Path(name).suffix.lower() in image_extensions
                )
        except (zipfile.BadZipFile, OSError):
            return 0

    @staticmethod
    def _count_directory_images(directory: Path) -> int:
        """Count image files in an extracted directory."""
        image_extensions = {".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif", ".tif", ".tiff"}
        return sum(
            1 for p in directory.rglob("*") if p.is_file() and p.suffix.lower() in image_extensions
        )

    def _derive_series_name(self, source_cbz_path: Path) -> str:
        """Derive stable series name from the source CBZ parent folder."""
        parent = source_cbz_path.parent
        if parent in (self.library_path, self.inbox_path):
            return source_cbz_path.stem
        name = parent.name.strip()
        return name or source_cbz_path.stem

    @staticmethod
    def _sidecar_volume_uuid(path: Path) -> str | None:
        """``volume_uuid`` out of one sidecar file, or None."""
        if not path.is_file():
            return None
        try:
            if path.name.lower().endswith(".gz"):
                with gzip.open(path, "rt", encoding="utf-8") as f:
                    data = json.load(f)
            else:
                with path.open("r", encoding="utf-8") as f:
                    data = json.load(f)
        except (OSError, UnicodeDecodeError, json.JSONDecodeError, gzip.BadGzipFile, EOFError):
            return None
        raw = data.get("volume_uuid") if isinstance(data, dict) else None
        return raw if isinstance(raw, str) and raw.strip() else None

    def _primary_volume_uuid(self, cbz_path: Path) -> str | None:
        """Read ``volume_uuid`` from the volume's primary sidecar, if any."""
        for candidate in self.get_mokuro_sidecar_paths(cbz_path):
            found = self._sidecar_volume_uuid(candidate)
            if found is not None:
                return found
        return None

    def _layer_volume_uuid(
        self, cbz_path: Path, generation: GenerationSpec | None
    ) -> str | None:
        """The ``volume_uuid`` a layer of this volume already on disk carries.

        Any row's layer but ``generation``'s own; the one that landed first
        (oldest) when there are several.
        """
        found: list[tuple[float, str]] = []
        for row in self.generations:
            if row.primary or (generation is not None and row.id == generation.id):
                continue
            for candidate in row.sidecar_paths(cbz_path):
                uuid_ = self._sidecar_volume_uuid(candidate)
                if uuid_ is None:
                    continue
                try:
                    found.append((candidate.stat().st_mtime, uuid_))
                except OSError:
                    continue
        return min(found)[1] if found else None

    def _remembered_volume_uuid(self, cbz_path: Path) -> str | None:
        """The id this archive's primary carried before it went, if known."""
        lookup = self.volume_uuid_lookup
        if lookup is None:
            return None
        try:
            relative = cbz_path.resolve().relative_to(self.library_path.resolve()).as_posix()
        except (OSError, ValueError):
            return None
        try:
            found = lookup(relative)
        except Exception as e:
            self._log(f"Could not look up the remembered volume id of {relative}: {e}")
            return None
        return found if isinstance(found, str) and found.strip() else None

    def volume_uuid_for(
        self, cbz_path: Path, generation: GenerationSpec | None = None
    ) -> str:
        """The ``volume_uuid`` every OCR file of this volume carries.

        In order: the primary sidecar's (a `.mokuro` that came with the volume,
        or one already produced); else the id the primary carried before it
        went (`Database.remembered_volume_uuid`) -- the server only makes a
        primary that is MISSING, so a re-OCR (the `.mokuro` deleted over
        WebDAV, or removed on disk) would otherwise mint a new id while every
        device installed before it keeps the old one, splitting one reader's
        progress across their devices; else a layer's already on disk (a
        primary being made again keeps the id its layers, and every reader's
        progress, know the volume by); else the id the catalog index and the
        reader already give a volume with no `.mokuro` --
        ``deterministic_uuid("<Series>/<Volume>")``. ``generation`` is the row
        being written, whose own file is never its own evidence.

        This is what lets a volume's generations run at once, in any order:
        with nothing on disk, every one of them -- the primary included --
        works out the same id from the path alone, so a layer finished before
        its primary, or two landing together on different machines, still
        name one volume (a remembered id is one database row, the same answer
        to all of them). Before, a layer run first got a fresh random id and
        was detached from the volume for good, which is why no layer could be
        claimed until the primary's sidecar existed.
        """
        if generation is None or not generation.primary:
            found = self._primary_volume_uuid(cbz_path)
            if found is not None:
                return found
        found = self._remembered_volume_uuid(cbz_path)
        if found is not None:
            return found
        found = self._layer_volume_uuid(cbz_path, generation)
        if found is not None:
            return found
        # Imported here: `metadata` imports this module (a cycle at load time).
        from mokuro_bunko.metadata.reader_compat import deterministic_uuid

        return deterministic_uuid(f"{self._derive_series_name(cbz_path)}/{cbz_path.stem}")

    def _normalize_mokuro_metadata(
        self, sidecar_path: Path, source_cbz_path: Path, generation: GenerationSpec
    ) -> None:
        """Rewrite sidecar metadata to stable series/title UUID based on source folder.

        Every sidecar, the primary's included, is given the volume's own
        ``volume_uuid`` (`volume_uuid_for`), whatever id the run minted: all
        OCR files of one volume share the uuid the reader knows it by, in
        whatever order they land.

        A non-primary sidecar additionally:

        * is STAMPED with an ``ocr_engine`` block naming what produced it.
          That block is how a reader tells server OCR output from a layer a
          person edited and pushed: the engine runner writes one for every
          composed engine, but the mokuro CLI writes none, so a monolithic
          engine running as a secondary row would otherwise arrive on every
          device badged as somebody's edit. The generation's name is added to
          the block either way, since that is the recipe the file is named
          after.

        The PRIMARY ``<Volume>.mokuro`` is left in pure upstream mokuro shape:
        it is the volume's OCR, not a layer, and nothing reads a kind out of
        it. (A composed engine running as primary writes an ``ocr_engine``
        block of its own; that is the runner's and is left alone.)
        """
        try:
            if sidecar_path.suffix.lower() == ".gz":
                with gzip.open(sidecar_path, "rt", encoding="utf-8") as f:
                    data = json.load(f)
            else:
                with sidecar_path.open("r", encoding="utf-8") as f:
                    data = json.load(f)
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as e:
            self._log(f"Skipping metadata normalization for {sidecar_path.name}: {e}")
            return

        if not isinstance(data, dict):
            self._log(f"Skipping metadata normalization for {sidecar_path.name}: invalid JSON root")
            return

        series_name = self._derive_series_name(source_cbz_path)
        data["title"] = series_name
        data["volume"] = source_cbz_path.stem
        data["title_uuid"] = str(uuid.uuid5(uuid.NAMESPACE_DNS, series_name))
        data["volume_uuid"] = self.volume_uuid_for(source_cbz_path, generation)
        if not generation.primary:
            self._stamp_ocr_engine(data, generation)

        try:
            if sidecar_path.suffix.lower() == ".gz":
                with gzip.open(sidecar_path, "wt", encoding="utf-8") as f:
                    json.dump(data, f, ensure_ascii=False, separators=(",", ":"))
            else:
                with sidecar_path.open("w", encoding="utf-8") as f:
                    json.dump(data, f, ensure_ascii=False, separators=(",", ":"))
        except OSError as e:
            self._log(f"Failed to write normalized metadata for {sidecar_path.name}: {e}")
            return

        self._log(f"Normalized sidecar metadata: {sidecar_path.name}")

    @staticmethod
    def _stamp_ocr_engine(data: dict[str, Any], generation: GenerationSpec) -> None:
        """Mark a layer sidecar as server OCR output, in place.

        The reader decides a layer's kind from CONTENT: a top-level
        ``ocr_engine`` object with a string ``id`` is OCR a server produced,
        and a file without one is a layer a person edited. A closed list of
        engine ids cannot survive user-chosen generation names, so the block
        is the discriminator and it has to be there.

        What the runner already wrote is kept (it knows its recognizer, its
        detector and the weights it loaded); only what is missing is filled
        in. ``detector`` is NOT invented for a monolithic engine: it detects
        behind its own command line with a detector of its own, and naming
        one of ours would be a guess.
        """
        block = data.get("ocr_engine")
        stamped = dict(block) if isinstance(block, dict) else {}
        stamped.setdefault("id", generation.engine)
        stamped.setdefault("generator", f"mokuro-bunko {__version__}")
        # The layer id on every reader IS this name; saying so in the file
        # keeps the postfix-to-recipe mapping readable from the file alone.
        stamped["generation"] = generation.name
        data["ocr_engine"] = stamped

    @staticmethod
    def _count_ocr_json_files(workspace: Path) -> int:
        """Count generated per-page OCR JSON files in workspace cache."""
        ocr_root = workspace / "_ocr"
        if not ocr_root.exists():
            return 0
        return sum(1 for _ in ocr_root.rglob("*.json"))

    @staticmethod
    def _progress_metrics(
        done: int,
        total_images: int,
        elapsed: float,
        rate: float | None = None,
    ) -> tuple[int | None, int | None, str]:
        """Percent, seconds left and status for a volume being read.

        ``elapsed`` is the time since this volume's FIRST page emission -- not
        since the process started. That distinction is the whole fix: what
        comes before the first emission is interpreter start, imports, model
        load, detector spawn and pipeline fill, and dividing a page count by an
        elapsed time containing all of that reported an ETA several times too
        long on every fast engine, creeping towards the truth only as the load
        was amortised away (ADDENDUM 9: model load never enters a rate).

        Before the first page has landed there is therefore NO eta at all and
        the status is ``starting``: what the caller shows then is what a
        session start costs (`RateModel.startup`), which is a different number
        with a different meaning.

        ``rate`` is the row's pages per second from :class:`~ocr.eta.RateModel`
        -- this session's completed volumes, the congestion history, the saved
        benchmark, blended with this volume's own emissions. Without one the
        volume's own emission rate is used, by the same ``(M-1)/window`` rule.

        Returns:
            tuple of (percent, eta_seconds, status)
        """
        if total_images <= 0:
            return None, None, "starting" if done <= 0 else "running"

        if done >= total_images:
            return 100, 0, "finalizing"

        if done <= 0:
            return 0, None, "starting"

        percent = min(99, int((done / total_images) * 100))
        pages_per_second = rate if rate and rate > 0 else emission_rate(done, elapsed)
        eta_seconds: int | None = None
        if pages_per_second:
            eta_seconds = int((total_images - done) / pages_per_second)
        return percent, eta_seconds, "running"

    def _rate_for(
        self, generation: GenerationSpec, done: int, elapsed: float
    ) -> RateEstimate | None:
        """This row's pages per second, blended with the volume in flight."""
        if self.rates is None:
            return None
        return self.rates.rate(
            generation.id, observed_pages=done, observed_seconds=elapsed
        )

    def _record_run_rate(
        self, generation: GenerationSpec, output_dir: Path, first_page_at: float | None
    ) -> None:
        """Fold a finished one-volume run into this row's measured rate.

        The session road gets this from the runner's own ``volume_done``; a
        row still on the one-volume command line has only the page files it
        wrote, so the window is from the FIRST of them to now, and the count
        is the pages that landed inside it -- ``M - 1``, the same
        ``(M-1)/window`` rule the benchmark times by. What happened before
        that first file (interpreter, imports, model load) is this road's
        per-volume startup and is charged as startup, never as pages.
        """
        if first_page_at is None or (self.rates is None and self.run_recorder is None):
            return
        done = self._count_ocr_json_files(output_dir)
        window = time.time() - first_page_at
        if done < 2 or window <= 0:
            return
        if self.rates is not None:
            self.rates.record_volume(generation.id, done - 1, window)
        if self.run_recorder is not None:
            # This machine's own lifetime count of the row (its profile's
            # ``runs``), as a session's volume_done records it.
            self.run_recorder(generation, done - 1, window)

    def is_processable(self, path: Path) -> bool:
        """Check if a path is a processable manga file or folder.

        Args:
            path: Path to check.

        Returns:
            True if the path can be processed.
        """
        if not path.exists():
            return False

        # Check for supported archive extensions
        if path.is_file():
            return path.suffix.lower() in SUPPORTED_EXTENSIONS

        # Check for directory with images
        if path.is_dir():
            image_extensions = {".jpg", ".jpeg", ".png", ".webp", ".gif"}
            images = [
                f for f in path.iterdir() if f.is_file() and f.suffix.lower() in image_extensions
            ]
            return len(images) > 0

        return False

    def process(self, input_path: Path) -> bool:
        """Process a manga file or folder.

        Args:
            input_path: Path to the manga in the inbox.

        Returns:
            True if processing succeeded.
        """
        if not input_path.exists():
            self._log(f"Input path does not exist: {input_path}")
            return False

        # If the file is already in the library, process in place and
        # only generate missing sidecars.
        if input_path.is_file() and input_path.suffix.lower() == ".cbz":
            try:
                in_library = input_path.resolve().is_relative_to(self.library_path.resolve())
            except ValueError:
                in_library = False
            if in_library:
                return self.process_library_cbz(input_path)

        if not self.is_processable(input_path):
            self._log(f"Not a processable manga: {input_path}")
            return False

        self._log(f"Processing: {input_path.name}")

        # An inbox upload gets ONE generation before it lands in the library,
        # and it is the PRIMARY row wherever that sits in the list: the bare
        # `<Volume>.mokuro` is what readers count characters from, and its
        # uuid (named here, from the inbox path) is the one the volume's
        # layers then take in the library (`volume_uuid_for`). Only when no
        # enabled row is primary (a host whose mokuro environment failed to
        # install) does the head of the list run instead.
        rows = enabled_generations(self.generations)
        if not rows:
            self._log("No OCR generation is enabled; leaving the upload in the inbox")
            return False
        generation = primary_generation(self.generations) or rows[0]
        label = get_engine(generation.engine).label

        if input_path.is_file() and input_path.suffix.lower() == ".cbz":
            workspace = self._build_temp_workspace(input_path.stem)
            try:
                extract_dir = self._extract_and_clean(input_path, workspace)
                run = self._run_engine(generation, extract_dir, workspace)
                if not run.ok:
                    self._record_failure(run.error, run.log_path)
                    self._log(f"{label} failed for: {input_path.name}")
                    return False

                sidecar = self._collect_valid_workspace_sidecar(extract_dir, workspace, generation)
                if sidecar is None:
                    self._record_failure(
                        f"no valid {generation.name} sidecar generated", run.log_path
                    )
                    self._log(
                        f"No valid {generation.name} sidecar generated for: {input_path.name}"
                    )
                    return False
                self._normalize_mokuro_metadata(sidecar, input_path, generation)

                dest_path = self.library_path / input_path.name
                if dest_path.exists():
                    dest_path = self._get_unique_path(dest_path)
                shutil.move(str(input_path), str(dest_path))
                self._log(f"Moved to library: {dest_path.name}")

                sidecar_plain, sidecar_gz = generation.sidecar_paths(dest_path)
                sidecar_dest = sidecar_gz if sidecar.name.endswith(".gz") else sidecar_plain
                if sidecar_dest.exists():
                    sidecar_dest = self._get_unique_path(sidecar_dest)
                shutil.move(str(sidecar), str(sidecar_dest))
                self._log(f"Created: {sidecar_dest.name}")

                self.ensure_thumbnail(dest_path)
                return True
            except Exception as e:
                self._log(f"Error processing {input_path.name}: {e}")
                return False
            finally:
                if workspace.exists():
                    shutil.rmtree(workspace, ignore_errors=True)

        # Create temporary output directory for the engine
        temp_output = self.storage_path / ".processing" / input_path.stem
        temp_output.mkdir(parents=True, exist_ok=True)
        try:
            run = self._run_engine(generation, input_path, temp_output)
            if not run.ok:
                self._record_failure(run.error, run.log_path)
                self._log(f"{label} failed for: {input_path.name}")
                return False

            # Keep any sidecars generated adjacent to the source file.
            suffix_plain = generation.sidecar_suffix
            suffix_gz = f"{suffix_plain}.gz"
            generated_sidecars = [
                p for p in generation.sidecar_paths(input_path) if p.exists()
            ]
            temp_sidecars = [
                p
                for p in sorted(temp_output.glob(f"*{suffix_plain}*"))
                if p.is_file() and (p.name.endswith(suffix_plain) or p.name.endswith(suffix_gz))
            ]

            sidecars_to_move: list[Path] = []
            seen: set[str] = set()
            for sidecar in temp_sidecars + generated_sidecars:
                key = str(sidecar)
                if key in seen:
                    continue
                seen.add(key)
                if not sidecar.exists():
                    continue
                if not self.is_valid_mokuro_sidecar(sidecar):
                    self._log(f"Skipping corrupt mokuro sidecar: {sidecar.name}")
                    continue
                sidecars_to_move.append(sidecar)

            if not sidecars_to_move:
                self._log(f"No valid mokuro sidecar generated for: {input_path.name}")
                return False

            # Move original file to library
            dest_path = self.library_path / input_path.name
            if dest_path.exists():
                # Handle duplicate names
                dest_path = self._get_unique_path(dest_path)

            shutil.move(str(input_path), str(dest_path))
            self._log(f"Moved to library: {dest_path.name}")

            for sidecar in sidecars_to_move:
                if not sidecar.exists():
                    continue
                self._normalize_mokuro_metadata(sidecar, input_path, generation)
                dest_plain, dest_gz = generation.sidecar_paths(dest_path)
                sidecar_dest = dest_gz if sidecar.name.endswith(".gz") else dest_plain
                if sidecar_dest.exists():
                    sidecar_dest = self._get_unique_path(sidecar_dest)
                shutil.move(str(sidecar), str(sidecar_dest))
                self._log(f"Created: {sidecar_dest.name}")

            return True

        except Exception as e:
            self._log(f"Error processing {input_path.name}: {e}")
            return False

        finally:
            # Cleanup temp directory
            if temp_output.exists():
                shutil.rmtree(temp_output, ignore_errors=True)

    def process_library_cbz(self, cbz_path: Path) -> bool:
        """Process missing OCR assets (every enabled generation) for a library CBZ.

        The primary row runs FIRST whatever its position in the list: it is
        the volume's reader-facing OCR. This is the sequential path (an upload
        that landed in the library); the queue lets every row run at once, in
        any order, since each sidecar is stamped with the volume's own uuid
        (`volume_uuid_for`).
        """
        rows = enabled_generations(self.generations)
        primary = primary_generation(self.generations)
        if primary is not None:
            rows = [primary] + [row for row in rows if row.id != primary.id]
        ocr_ok = True
        for generation in rows:
            if not self.process_library_ocr(cbz_path, generation):
                ocr_ok = False
        thumb_ok = self.process_library_thumbnail(cbz_path)
        return ocr_ok and thumb_ok

    def process_library_ocr(self, cbz_path: Path, generation: GenerationSpec) -> bool:
        """Generate one generation's missing sidecar for a library CBZ.

        ``generation`` is a SNAPSHOT of the row, taken when the job was
        claimed. Every path this run writes or reads back -- the sidecar, the
        log, the workspace cache, the destination -- comes from it and not
        from the live settings, so an edit landing mid-run cannot make the
        collector look for a file that was never written.

        ``publish_guard`` (set by the caller for this job) is asked just
        before the sidecar is moved into the library: False (the archive was
        deleted or replaced meanwhile) drops the result, sets
        ``last_discarded`` and returns False WITHOUT a failure -- the caller
        gives the job back to the queue.
        """
        self.last_discarded = False
        self.last_written = None
        still_current = self.publish_guard
        label = get_engine(generation.engine).label
        if not cbz_path.exists():
            self._log(f"CBZ not found: {cbz_path}")
            return False
        if not self.needs_sidecar(cbz_path, generation):
            self._log(f"{generation.name} sidecar already exists, skipping OCR: {cbz_path.name}")
            return True

        self._log(f"Processing library CBZ with {generation.name} in temp workspace: {cbz_path}")
        self.last_pipeline = None
        workspace = self._build_temp_workspace(cbz_path.stem)
        try:
            extract_dir = self._extract_and_clean(cbz_path, workspace)
            total_images = self._count_directory_images(extract_dir)
            rel_cbz = str(cbz_path.relative_to(self.library_path))
            series_rel = str(cbz_path.parent.relative_to(self.library_path))
            base_progress = {
                "active": True,
                "generation": generation.name,
                "generation_id": generation.id,
                "engine": generation.engine,
                "detector": generation.reported_detector,
                "series": series_rel,
                "volume": cbz_path.stem,
                "relative_cbz": rel_cbz,
                "total_pages": total_images if total_images > 0 else None,
            }
            self._emit_progress(
                {
                    **base_progress,
                    "percent": 0,
                    "eta_seconds": None,
                    "done_pages": 0,
                    # Nothing has come out of the engine yet, and nothing here
                    # will pretend it can say when it will: the card reads
                    # "starting up (about N s)" from the row's startup cost.
                    "status": "starting",
                    "first_page_at": None,
                    "session_started_at": time.time(),
                }
            )
            sidecar: Path | None = None
            run = self._run_engine(
                generation, extract_dir, workspace, total_images=total_images, source_cbz=cbz_path
            )
            if not run.ok:
                sidecar = self._collect_valid_workspace_sidecar(
                    extract_dir, workspace, generation
                )
                if sidecar is None:
                    self._record_failure(run.error, run.log_path)
                    self._log(f"{label} failed for: {cbz_path.name}")
                    self._emit_progress(
                        {
                            **base_progress,
                            "percent": 0,
                            "eta_seconds": None,
                            "done_pages": 0,
                            "status": "error",
                            "error": run.error,
                        }
                    )
                    return False
                self._log(
                    f"{label} exited with error but sidecar was generated; importing for: {cbz_path.name}"
                )
            if sidecar is None:
                sidecar = self._collect_valid_workspace_sidecar(
                    extract_dir, workspace, generation
                )
            if sidecar is None:
                error = f"no valid {generation.name} sidecar generated"
                self._record_failure(error, run.log_path)
                self._log(f"No valid {generation.name} sidecar generated for: {cbz_path.name}")
                self._emit_progress(
                    {
                        **base_progress,
                        "percent": 0,
                        "eta_seconds": None,
                        "done_pages": 0,
                        "status": "error",
                        "error": error,
                    }
                )
                return False
            if still_current is not None and not still_current():
                self.last_discarded = True
                return False
            # Read BEFORE normalizing: the server's own stamp must not pass
            # for what the runner wrote.
            facts = read_sidecar_facts(sidecar)
            self._normalize_mokuro_metadata(sidecar, cbz_path, generation)
            sidecar_plain, sidecar_gz = generation.sidecar_paths(cbz_path)
            dest = sidecar_gz if sidecar.name.endswith(".gz") else sidecar_plain
            if dest.exists():
                dest = self._get_unique_path(dest)
            shutil.move(str(sidecar), str(dest))
            self._log(f"Created sidecar: {dest.name}")
            self.last_written = WrittenSidecar(
                path=dest, facts=facts, failed_pages=failed_pages_from_log(run.log_path)
            )
            self._emit_progress(
                {
                    **base_progress,
                    "percent": 100,
                    "eta_seconds": 0,
                    "done_pages": total_images if total_images > 0 else None,
                    "status": "done",
                }
            )
            # The runner's last publish happens before it assembles the
            # volume, so the numbers are complete by now -- and the workspace
            # holding them is removed in the `finally` below, which is why
            # this is the only chance to keep them.
            self._harvest_pipeline_stats(workspace, generation, rel_cbz)
            self.last_failure = None
            return True
        except Exception as e:
            self._record_failure(str(e), None)
            self._log(f"Error processing {cbz_path.name}: {e}")
            return False
        finally:
            if workspace.exists():
                shutil.rmtree(workspace, ignore_errors=True)

    def _harvest_pipeline_stats(
        self, workspace: Path, generation: GenerationSpec, volume: str
    ) -> None:
        """Keep this run's final pool/queue numbers before the workspace goes.

        Sets :attr:`last_pipeline` to the record the worker appends to the
        generation's congestion history, or leaves it None: a monolithic
        engine publishes nothing, and neither does a run that ended before
        the runner's first write. A readout must never be why a finished
        volume is treated as a failure, so nothing here raises.
        """
        if self.runs_mokuro_cli(generation):
            return
        summary = read_final_stats(pipeline_stats_path(workspace, generation.id))
        if summary is None:
            return
        self.last_pipeline = build_record(summary, volume=volume)

    def process_library_thumbnail(self, cbz_path: Path) -> bool:
        """Generate missing thumbnail for a library CBZ."""
        if not cbz_path.exists():
            self._log(f"CBZ not found: {cbz_path}")
            return False
        if not self.needs_thumbnail(cbz_path):
            return True
        return self.ensure_thumbnail(cbz_path)

    def _get_mokuro_log_path(
        self, input_path: Path, generation: GenerationSpec, source_cbz: Path | None = None
    ) -> Path:
        """Return the per-volume OCR log path (parent dirs created).

        Named after the GENERATION, not its engine: two rows may run one
        engine, and an engine-named log would have each truncating the
        other's while both subprocesses write to it -- and
        ``_mokuro_reported_failure`` then parses whichever survived, filing
        one row's failure against the other. The primary row keeps the
        historical bare ``<stem>.log``.

        A library volume is named by its SERIES as well
        (``<series>_<stem>.log``), because the stem alone is not unique in a
        library: almost every series has a "Volume 1.cbz", and the queue's
        round-robin deliberately runs one volume per series, so with more
        than one slot the concurrent jobs are usually volumes that share a
        stem.
        """
        log_dir = get_ocr_log_dir(self.storage_path)
        log_dir.mkdir(parents=True, exist_ok=True)
        parts = [input_path.stem]
        if source_cbz is not None:
            try:
                series = source_cbz.parent.relative_to(self.library_path).as_posix()
            except ValueError:
                series = ""
            if series and series != ".":
                parts.insert(0, series)
        # Sanitize so odd series/volume names can't escape the log dir. The
        # generation name needs none: its grammar is already file-name safe.
        safe_stem = re.sub(r'[<>:"/\\|?*]', "_", "_".join(parts)) or "volume"
        if generation.primary:
            return log_dir / f"{safe_stem}.log"
        return log_dir / f"{safe_stem}.{generation.name}.log"

    @staticmethod
    def _extract_mokuro_error(log_path: Path) -> str | None:
        """Pull a short human-readable failure reason from a mokuro log.

        Prefers the final line of the last Python traceback, then the last
        loguru ERROR line. Returns at most 300 characters.
        """
        try:
            text = log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return None

        lines = [line.strip() for line in text.splitlines() if line.strip()]
        traceback_final: str | None = None
        loguru_error: str | None = None
        module_error: str | None = None
        in_traceback = False
        for line in lines:
            if line.startswith("Traceback (most recent call last)"):
                in_traceback = True
                continue
            if in_traceback and _EXCEPTION_LINE_RE.match(line):
                # Keep overwriting: the raised exception is the last
                # "Exc.Class: message" line in the traceback block.
                traceback_final = line
                continue
            if _TRACEBACK_FINAL_RE.match(line):
                traceback_final = line
            match = _LOGURU_ERROR_RE.search(line)
            if match:
                loguru_error = match.group("msg").strip()
            # runpy's "python.exe: No module named mokuro" (broken OCR env)
            if "No module named" in line:
                module_error = line

        best = traceback_final or loguru_error or module_error
        if best is None:
            return None
        return best[:300]

    @staticmethod
    def _mokuro_reported_failure(log_path: Path) -> bool:
        """Check whether mokuro's own summary reports zero processed volumes.

        mokuro exits 0 even when every volume fails, so the exit code alone
        cannot be trusted; the "Processed successfully: N/M" summary can.
        """
        try:
            text = log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return False
        matches = _PROCESSED_RE.findall(text)
        if not matches:
            return False
        done, total = matches[-1]  # trust the last summary in the log
        return int(done) < int(total)

    def _run_engine(
        self,
        generation: GenerationSpec,
        input_path: Path,
        output_dir: Path,
        total_images: int = 0,
        source_cbz: Path | None = None,
    ) -> MokuroRunResult:
        """Run one generation over an extracted volume (dispatch by engine)."""
        if self.runs_mokuro_cli(generation):
            return self._run_mokuro(
                input_path,
                output_dir,
                total_images=total_images,
                generation=generation,
                source_cbz=source_cbz,
            )
        return self._run_engine_runner(
            generation, input_path, output_dir, total_images=total_images, source_cbz=source_cbz
        )

    def _engine_runner_command(
        self,
        generation: GenerationSpec,
        input_path: Path,
        output_dir: Path,
        source_cbz: Path | None = None,
    ) -> list[str]:
        """Build the engine-runner command (runner script copied into the workspace).

        The runner stays engine-and-detector level and never learns what a
        generation is: what this row contributes is WHERE the sidecar goes,
        how wide its stages run, and how deep the queues between them may
        get. Everything keyed by a name here uses the generation's immutable
        ``id``, so two rows on one engine never share a directory.
        """
        runner_python = self.runner_python(generation)
        if runner_python is None:
            needed = sorted(
                {
                    row.engine
                    for row in enabled_generations(self.generations)
                    if not row.mokuro_env
                }
                or {generation.engine}
            )
            raise FileNotFoundError(
                f"OCR engines environment not installed (needed for '{generation.engine}'); "
                "run: mokuro-bunko install-ocr --engines " + ",".join(needed)
            )
        generation = self._as_run(generation)
        runner_path = self._stage_runner(output_dir)

        stem = input_path.stem if input_path.is_file() else input_path.name
        output_file = output_dir / f"{stem}{generation.sidecar_suffix}"
        cache_dir = output_dir / "_ocr" / generation.id / stem
        cmd = [
            str(runner_python),
            str(runner_path),
            "--engine",
            generation.engine,
            "--detector",
            generation.effective_detector,
            "--input",
            str(input_path),
            "--output",
            str(output_file),
            "--cache-dir",
            str(cache_dir),
            # Where the runner publishes its pool/queue numbers while it
            # works. Named rather than left to the runner's default so the
            # poll loop below reads exactly the file this job writes: a
            # MOKURO_OCR_PIPELINE_STATS in the server's own environment is
            # inherited by every job, and would otherwise have concurrent
            # jobs overwriting each other's readout.
            "--stats-file",
            str(pipeline_stats_path(output_dir, generation.id)),
            "--generator",
            f"mokuro-bunko {__version__}",
        ]
        if generation.patch_budget_applies:
            cmd += ["--patches", str(generation.patch_budget)]
        cmd += self._served_engine_args(generation)
        stage_workers = _stage_setting(generation.pools.stage_workers)
        if stage_workers:
            cmd += ["--stage-workers", stage_workers]
        queue_capacity = _stage_setting(generation.pools.queue_capacity)
        if queue_capacity:
            cmd += ["--queue-capacity", queue_capacity]
        stage_device = _device_setting(generation.pools.stage_device)
        if stage_device:
            cmd += ["--stage-device", stage_device]
        cmd += _precision_args(generation)
        if source_cbz is not None:
            cmd += ["--volume-uuid", self.volume_uuid_for(source_cbz, generation)]
        return cmd

    def _stage_runner(self, output_dir: Path | None = None) -> Path:
        """The staged runner script, staged if this build has not been yet.

        The runner, the modules it imports and the detector adapters are all
        executed/imported BY PATH from the engines environment, which does
        not have ``mokuro_bunko`` installed; copying also works from a
        zipapp. ``ppocr.py``, ``line_layout.py`` and ``line_reconcile.py``
        land next to the runner, which imports them by
        adding its own directory to ``sys.path`` (the ``ppocr_manga``
        detector adapter finds ``ppocr.py`` there too, one directory above
        itself).

        Staged ONCE per content hash, in a stable directory, rather than
        copied into every volume's workspace: a session keeps one runner open
        across many volumes and there is no workspace that outlives them, and
        the per-volume path was paying eleven file copies a volume for a set
        of files that only change when the server is upgraded.

        ``output_dir`` is accepted and ignored; it is what the per-volume
        caller has to hand and keeps that call site unchanged.

        A processor built with ``staged_runner`` never stages at all: it
        runs the build its process pinned at start.
        """
        del output_dir
        if self.staged_runner is not None:
            return self.staged_runner
        return stage_runner(self.storage_path)

    def open_session(
        self,
        generation: GenerationSpec,
        session_log: Path,
    ) -> OcrSession:
        """A runner held open for one generation, not yet started.

        The caller starts it, feeds it volumes and closes it
        (:mod:`mokuro_bunko.ocr.session`). Everything about the environment
        the subprocess runs in is decided here, exactly as it is for a
        per-volume run: the same interpreter, the same ``MOKURO_OCR_JOBS``,
        the same OS priority rule (the first enabled row keeps normal
        priority, every backlog row is niced).
        """
        command = self.session_command(generation, session_log)
        return OcrSession(
            generation,
            command,
            session_log=session_log,
            env=self.ocr_env(generation),
            popen_kwargs=self._priority_popen_kwargs(self.is_backlog_generation(generation)),
        )

    def session_command(self, generation: GenerationSpec, session_log: Path) -> list[str]:
        """``engine_runner.py --serve`` for one generation.

        Only what the SESSION is: the engine, its detector, the character
        map, the patch budget and the pool widths. Everything per volume --
        where the pages come from, where the sidecar goes, which log line
        belongs to which volume -- arrives later as a ``volume`` op, because
        a session outlives every one of them.
        """
        runner_python = self.runner_python(generation)
        if runner_python is None:
            raise FileNotFoundError(
                f"OCR engines environment not installed (needed for '{generation.engine}'); "
                f"run: mokuro-bunko install-ocr --engines {generation.engine}"
            )
        generation = self._as_run(generation)
        cmd = [
            str(runner_python),
            str(self._stage_runner()),
            "--serve",
            "--engine",
            generation.engine,
            "--detector",
            generation.effective_detector,
            "--generator",
            f"mokuro-bunko {__version__}",
            "--session-log",
            str(session_log),
        ]
        if generation.patch_budget_applies:
            cmd += ["--patches", str(generation.patch_budget)]
        cmd += self._served_engine_args(generation)
        stage_workers = _stage_setting(generation.pools.stage_workers)
        if stage_workers:
            cmd += ["--stage-workers", stage_workers]
        queue_capacity = _stage_setting(generation.pools.queue_capacity)
        if queue_capacity:
            cmd += ["--queue-capacity", queue_capacity]
        stage_device = _device_setting(generation.pools.stage_device)
        if stage_device:
            cmd += ["--stage-device", stage_device]
        cmd += _precision_args(generation)
        return cmd

    def open_bench(
        self,
        generation: GenerationSpec,
        sample_dir: Path,
        session_log: Path,
        *,
        max_trials: int = 8,
        budget_seconds: float = 900.0,
        precision_only: bool = False,
    ) -> OcrSession:
        """``engine_runner.py --bench`` over a directory of sample pages.

        ``precision_only``: the precision trials of a balanced/speed mode and
        nothing else (``--bench-precision-only``), at the machine's pools
        EXACTLY as configured -- so its widths and capacities are sent too.

        The same process shape as a session -- load everything once, prose to
        the log file, protocol on stdout -- so the same reader owns the pipe.
        ``--stage-workers`` / ``--queue-capacity`` are deliberately NOT sent:
        a benchmark starts from the derived widths and explores from there,
        and handing it the row's saved widths would measure the tuning
        instead of the machine. ``--stage-device`` IS sent: where the models
        run is part of the spec being measured, not a width to rediscover,
        and the tuner explores placements from the one it was given.
        """
        runner_python = self.runner_python(generation)
        if runner_python is None:
            raise FileNotFoundError(
                f"OCR engines environment not installed (needed for '{generation.engine}'); "
                f"run: mokuro-bunko install-ocr --engines {generation.engine}"
            )
        session_log.parent.mkdir(parents=True, exist_ok=True)
        cmd = [
            str(runner_python),
            str(self._stage_runner()),
            "--bench",
            "--engine",
            generation.engine,
            "--detector",
            generation.effective_detector,
            "--input",
            str(sample_dir),
            "--session-log",
            str(session_log),
            "--bench-max-trials",
            str(int(max_trials)),
            "--bench-budget-seconds",
            str(int(budget_seconds)),
        ]
        if generation.patch_budget_applies:
            cmd += ["--patches", str(generation.patch_budget)]
        cmd += self._served_engine_args(generation)
        stage_device = _device_setting(generation.pools.stage_device)
        if stage_device:
            cmd += ["--stage-device", stage_device]
        if precision_only:
            cmd.append("--bench-precision-only")
            stage_workers = _stage_setting(generation.pools.stage_workers)
            if stage_workers:
                cmd += ["--stage-workers", stage_workers]
            queue_capacity = _stage_setting(generation.pools.queue_capacity)
            if queue_capacity:
                cmd += ["--queue-capacity", queue_capacity]
        cmd += _precision_args(generation, pick=False)
        return OcrSession(
            generation,
            cmd,
            session_log=session_log,
            env=self.ocr_env(generation),
            # A benchmark measures the machine as the queue would use it, so
            # it runs at the priority the FIRST row runs at -- never niced,
            # whatever position the row being measured holds. Niced numbers
            # would describe a machine nobody's OCR ever gets.
            popen_kwargs={},
        )

    def prepare_session_volume(
        self,
        cbz_path: Path,
        generation: GenerationSpec,
        job_id: str,
    ) -> SessionVolume:
        """The ``volume`` op for one library archive, with its own workspace.

        The ARCHIVE is what is sent: the runner decompresses one page at a
        time just ahead of its own pipeline, so nothing is extracted here and
        no lookahead of extracted volumes exists to fill a disk.

        ``generation`` is the row as it is NOW -- the output path and the log
        path are fixed from its name at this moment, so a rename applies to
        the volumes submitted after it and can never move the file a volume
        already in flight will be collected under.
        """
        stem = cbz_path.stem
        try:
            archive_size: int | None = cbz_path.stat().st_size
        except OSError:
            archive_size = None
        workspace = self._build_temp_workspace(stem)
        series_name = self._derive_series_name(cbz_path)
        return SessionVolume(
            id=job_id,
            archive=cbz_path,
            archive_size=archive_size,
            workspace=workspace,
            output=workspace / f"{stem}{generation.sidecar_suffix}",
            cache_dir=workspace / "_ocr" / generation.id / stem,
            detect_dir=workspace / "_detect" / generation.id,
            log=self._get_mokuro_log_path(cbz_path, generation, cbz_path),
            title=series_name,
            volume=stem,
            title_uuid=str(uuid.uuid5(uuid.NAMESPACE_DNS, series_name)),
            # The volume's own id, whatever has or has not run on it yet
            # (`volume_uuid_for`); the install stamps it again all the same.
            volume_uuid=self.volume_uuid_for(cbz_path, generation),
        )

    def session_sidecar_destination(
        self, cbz_path: Path, generation: GenerationSpec, sidecar: Path
    ) -> Path:
        """Where `install_session_sidecar` puts ``sidecar``: the row's own name
        beside the archive, or a numbered one when a file is already there."""
        sidecar_plain, sidecar_gz = generation.sidecar_paths(cbz_path)
        dest = sidecar_gz if sidecar.name.endswith(".gz") else sidecar_plain
        return self._get_unique_path(dest) if dest.exists() else dest

    def install_session_sidecar(
        self,
        cbz_path: Path,
        generation: GenerationSpec,
        sidecar: Path,
    ) -> str | None:
        """Validate, normalize and move a finished sidecar into the library.

        Returns None on success, or a one-sentence reason. It returns rather
        than setting ``last_failure``, because a session has several volumes
        in flight in one process and a single-valued failure slot would
        attribute one volume's trouble to another.
        """
        if not self.is_valid_mokuro_sidecar(sidecar):
            if sidecar.exists():
                self._log(f"Ignoring corrupt {generation.name} sidecar: {sidecar.name}")
                return f"the {generation.name} sidecar it wrote is not readable JSON"
            return f"no valid {generation.name} sidecar generated"
        self._normalize_mokuro_metadata(sidecar, cbz_path, generation)
        dest = self.session_sidecar_destination(cbz_path, generation, sidecar)
        try:
            shutil.move(str(sidecar), str(dest))
        except OSError as e:
            return f"could not move the {generation.name} sidecar into the library: {e}"
        self._log(f"Created sidecar: {dest.name}")
        return None

    def ocr_env(self, generation: GenerationSpec | None = None) -> dict[str, str]:
        """The environment every OCR subprocess of this processor runs in.

        One function so a session and a per-volume job cannot drift:
        ``MOKURO_OCR_JOBS`` is how the runner sizes its per-stage pools
        against this run's SHARE of the host, and without it every concurrent
        job budgets for the whole machine and they oversubscribe.

        With ``generation``, and every Hugging Face model that row loads
        already fully in the cache, the Hub is told to stay away
        (``HF_HUB_OFFLINE``/``TRANSFORMERS_OFFLINE``): a session start then
        makes no round trips for models it already has -- about 0.9 s a
        start, and a network dependency it did not need. Anything short of
        that stays online, and a value the operator set is never touched.
        """
        env = dict(os.environ)
        env["PYTHONIOENCODING"] = "utf-8"
        env["PYTHONUNBUFFERED"] = "1"
        env["MOKURO_OCR_JOBS"] = str(self.concurrency)
        if (
            generation is not None
            and "HF_HUB_OFFLINE" not in env
            and "TRANSFORMERS_OFFLINE" not in env
        ):
            try:
                cached = row_models_cached(generation.engine, generation.detector, env)
            except Exception as e:  # noqa: BLE001 - a cache check never stops a start
                self._log(f"Could not check the Hugging Face cache (staying online): {e}")
                cached = False
            if cached:
                env["HF_HUB_OFFLINE"] = "1"
                env["TRANSFORMERS_OFFLINE"] = "1"
        return env

    def _run_engine_runner(
        self,
        generation: GenerationSpec,
        input_path: Path,
        output_dir: Path,
        total_images: int = 0,
        source_cbz: Path | None = None,
    ) -> MokuroRunResult:
        """Run a composed generation through the standalone runner script."""
        label = get_engine(generation.engine).label
        try:
            cmd = self._engine_runner_command(
                generation, input_path, output_dir, source_cbz=source_cbz
            )
        except (FileNotFoundError, OSError) as e:
            return self._fail_run(f"{label}: {e}", None)
        log_path = self._get_mokuro_log_path(input_path, generation, source_cbz)
        return self._run_ocr_subprocess(
            cmd,
            input_path,
            output_dir,
            log_path,
            total_images=total_images,
            label=label,
            generation=generation,
            # Only the composed engines have a staged pipeline to report on;
            # `mokuro` detects and recognizes behind its own CLI and passes
            # None, so its progress carries no stage rows at all.
            stats_path=pipeline_stats_path(output_dir, generation.id),
        )

    def _run_mokuro(
        self,
        input_path: Path,
        output_dir: Path,
        total_images: int = 0,
        generation: GenerationSpec = DEFAULT_GENERATION,
        source_cbz: Path | None = None,
    ) -> MokuroRunResult:
        """Run mokuro on the input file.

        The subprocess's combined stdout/stderr is captured to a per-volume
        log file under ``<storage>/logs/ocr/`` so failures are diagnosable.

        Args:
            input_path: Path to manga file/folder.
            output_dir: Directory for mokuro output.
            total_images: Expected page count for progress reporting.
            generation: The row this run is for. Its precision mode adds
                the fork's ``--fp16`` where it resolves to fp16 on this
                server's card, and its name is the log file's.
            source_cbz: The library archive this run is for, when there is
                one; it names the log file's series (see
                ``_get_mokuro_log_path``).

        Returns:
            MokuroRunResult with success flag, short error summary, and the
            path of the captured log.
        """
        run = self._as_run(generation)
        flags, env_extra = self._mokuro_placement(run)
        cmd = [
            str(self.python_path),
            "-m",
            "mokuro",
            str(input_path),
            "--disable_confirmation",
            "--no_cache",
            *self._mokuro_precision_flags(run),
            *flags,
        ]
        try:
            log_path = self._get_mokuro_log_path(input_path, generation, source_cbz)
        except OSError as e:
            return self._fail_run(f"Mokuro exception: {e}", None)
        return self._run_ocr_subprocess(
            cmd,
            input_path,
            output_dir,
            log_path,
            total_images=total_images,
            label="Mokuro",
            generation=generation,
            env_extra=env_extra,
        )

    @staticmethod
    def _mokuro_precision_flags(generation: GenerationSpec) -> list[str]:
        """``--fp16`` where the row's mode resolves to fp16 on this server.

        The CLI decides nothing itself, so the mode is resolved here, from
        this server's own probe (the runner does the same on the served road).
        A card that was never probed counts as a card: the fork ignores the
        flag on the CPU anyway.
        """
        from mokuro_bunko.ocr.devices import cached_catalog
        from mokuro_bunko.ocr.engine_runner import PRECISION_FP32, resolve_mode
        from mokuro_bunko.ocr.precision import model_device

        if not generation.precision_applies:
            return []
        device = model_device(generation, None)
        supported = cached_catalog().supported_for(device)
        if supported is None:
            supported = frozenset({PRECISION_FP32, PRECISION_FP16}) if device != "cpu" else None
        resolved = resolve_mode(
            generation.engine, generation.precision, supported,
            pick=generation.precision_pick, pick_why=generation.precision_why,
        )
        return ["--fp16"] if resolved.precision == PRECISION_FP16 else []

    @staticmethod
    def _mokuro_placement(generation: GenerationSpec) -> tuple[list[str], dict[str, str]]:
        """The ``mokuro`` stage's two cells, as the mokuro CLI takes them.

        The one-volume CLI is what is left for a package with no serve module
        (``runs_mokuro_cli``); the stage it runs is the same stage the served
        road spawns as a process, so the DEVICE translation is the runner's
        own (``engine_runner.mokuro_placement``, Addendum 7's MONOLITHIC
        bullet) and not a second copy of it here. What is added on this path
        alone is the flag shape:

        * device -> ``--force_cpu`` / ``CUDA_VISIBLE_DEVICES`` +
          ``HIP_VISIBLE_DEVICES`` in that subprocess only;
        * workers ``N`` -> ``--num_workers N``; absent leaves the fork's own
          default, which is why this is only ever sent when it was set.
        """
        placement = mokuro_placement(generation.pools.stage_device.get(STAGE_MOKURO, ""))
        flags = placement.flags
        workers = generation.pools.stage_workers.get(STAGE_MOKURO)
        if workers is not None:
            flags += ["--num_workers", str(int(workers))]
        return flags, dict(placement.env)

    def _run_ocr_subprocess(
        self,
        cmd: list[str],
        input_path: Path,
        output_dir: Path,
        log_path: Path,
        total_images: int = 0,
        label: str = "Mokuro",
        generation: GenerationSpec = DEFAULT_GENERATION,
        stats_path: Path | None = None,
        env_extra: Mapping[str, str] | None = None,
    ) -> MokuroRunResult:
        """Run an OCR subprocess with progress polling, stall detection and log capture.

        ``stats_path`` is where this run publishes its pool and queue numbers
        (``pipeline.json``), when it has a staged pipeline at all. Each poll
        reads it and carries the summary along with the percentage, so the
        queue page can show where the time is going while it is going there.
        """
        try:
            hard_timeout_seconds = 3600
            no_progress_timeout_seconds = 600
            finalizing_timeout_seconds = 900

            self._log(f"Running: {' '.join(cmd)}")

            # The runner sizes its per-stage pools against (cores / jobs), so
            # it has to be told how many of us there are. Without this every
            # concurrent job budgets for the whole host and they oversubscribe.
            env = self.ocr_env(generation)
            # This row's device, for a subprocess that takes one as an
            # environment variable rather than a flag (the mokuro CLI).
            env.update(env_extra or {})

            with log_path.open("w", encoding="utf-8", errors="replace") as log_file:
                log_file.write(f"# {label} run for: {input_path}\n# command: {' '.join(cmd)}\n\n")
                log_file.flush()
                started = self._start_process(
                    cmd,
                    stdout=log_file,
                    stderr=subprocess.STDOUT,
                    env=env,
                    **self._priority_popen_kwargs(self.is_backlog_generation(generation)),
                )
                if started is None:
                    return self._fail_run(f"{label} cancelled before it started", log_path)
                process = started
                start = time.time()
                last_done = -1
                last_progress_time = start
                finalizing_since: float | None = None
                # When this volume's FIRST page landed. Everything before it
                # is startup (interpreter, imports, model load, pool spawn)
                # and must not divide into a rate -- see `_progress_metrics`.
                first_page_at: float | None = None

                while process.poll() is None:
                    now = time.time()
                    if self._cancel_now():
                        process.kill()
                        return self._fail_run(f"{label} cancelled by a settings change", log_path)
                    if now - start > hard_timeout_seconds:
                        process.kill()
                        return self._fail_run(f"{label} timed out", log_path)
                    done = self._count_ocr_json_files(output_dir)
                    if done != last_done:
                        last_done = done
                        last_progress_time = now
                    if done > 0 and first_page_at is None:
                        first_page_at = now

                    since_first = now - first_page_at if first_page_at is not None else 0.0
                    estimate = self._rate_for(generation, done, since_first)
                    percent, eta_seconds, progress_status = self._progress_metrics(
                        done=done,
                        total_images=total_images,
                        elapsed=since_first,
                        rate=estimate.pages_per_second if estimate is not None else None,
                    )
                    if progress_status == "finalizing":
                        if finalizing_since is None:
                            finalizing_since = now
                        if now - finalizing_since > finalizing_timeout_seconds:
                            if (
                                self._collect_valid_workspace_sidecar(
                                    input_path, output_dir, generation
                                )
                                is not None
                            ):
                                process.kill()
                                self._log(
                                    f"{label} finalizing exceeded timeout; valid sidecar found, continuing"
                                )
                                return MokuroRunResult(True, None, log_path)
                            process.kill()
                            return self._fail_run(f"{label} stalled in finalizing phase", log_path)
                    else:
                        finalizing_since = None

                    if now - last_progress_time > no_progress_timeout_seconds:
                        process.kill()
                        return self._fail_run(f"{label} stalled with no OCR progress", log_path)

                    progress: dict[str, Any] = {
                        "active": True,
                        "generation": generation.name,
                        # The row's immutable id rides along so a request
                        # thread can ask the rate model about it without
                        # having to map a (renameable) name back to a row.
                        "generation_id": generation.id,
                        "engine": generation.engine,
                        "detector": generation.reported_detector,
                        "percent": percent,
                        "eta_seconds": eta_seconds,
                        "done_pages": done,
                        "total_pages": total_images if total_images > 0 else None,
                        "status": progress_status,
                        # The raw signals the status endpoint re-derives an
                        # ETA from on every poll: WHEN the first page landed
                        # (epoch, so another thread can subtract it) and when
                        # this run started paying its startup.
                        "first_page_at": first_page_at,
                        "session_started_at": start,
                        "rate_pages_per_second": (
                            round(estimate.pages_per_second, 4) if estimate else None
                        ),
                        "latency_seconds": (
                            round(estimate.latency_seconds, 2) if estimate else None
                        ),
                        "rate_source": estimate.source if estimate else None,
                    }
                    # Absent until the runner has published something, and
                    # absent again once it stops: the key is omitted rather
                    # than set to None so a job with no pipeline looks
                    # exactly like one from a server that never had this.
                    pipeline = read_pipeline_stats(stats_path) if stats_path else None
                    if pipeline is not None:
                        progress["pipeline"] = pipeline
                    self._emit_progress(progress)
                    time.sleep(2.0)

                result_code = process.returncode

            if self._cancel_now():
                return self._fail_run(f"{label} cancelled by a settings change", log_path)
            if result_code != 0:
                detail = self._extract_mokuro_error(log_path)
                return self._fail_run(detail or "subprocess exited with non-zero status", log_path)

            # mokuro exits 0 even when a volume fails; trust its own summary.
            if self._mokuro_reported_failure(log_path):
                detail = self._extract_mokuro_error(log_path)
                return self._fail_run(
                    detail or f"{label.lower()} reported the volume was not processed", log_path
                )

            self._record_run_rate(generation, output_dir, first_page_at)
            return MokuroRunResult(True, None, log_path)

        except subprocess.TimeoutExpired:
            return self._fail_run(f"{label} timed out", log_path)
        except FileNotFoundError:
            return self._fail_run(f"Python not found: {cmd[0]}", log_path)
        except Exception as e:
            return self._fail_run(f"{label} exception: {e}", log_path)
        finally:
            with self._process_lock:
                self._active_process = None

    def is_backlog_generation(self, generation: GenerationSpec) -> bool:
        """True when this row is not the one the queue runs first.

        The first enabled row is the OCR layer readers are waiting for;
        every row below it is a backlog that should yield CPU to it and to
        the server itself.

        It asks ``enabled_generations`` -- the SAME function the scheduler
        orders the queue with -- so the job at normal priority is always the
        head of the list the queue page shows. Keying this on anything else
        is how a paddle-manga job was once observed running at ``ni=10``
        while it was the only thing in the queue. Keying it on the ENGINE
        would leave two rows sharing the first row's engine both at normal
        priority.
        """
        order = enabled_generations(self.generations)
        return bool(order) and generation.id != order[0].id

    @staticmethod
    def _priority_popen_kwargs(low_priority: bool) -> dict[str, Any]:
        """Popen options that lower the OS priority of a backlog run."""
        if not low_priority:
            return {}
        if sys.platform == "win32":
            return {"creationflags": getattr(subprocess, "BELOW_NORMAL_PRIORITY_CLASS", 0)}
        return {"preexec_fn": lambda: os.nice(10)}

    def _fail_run(self, error: str, log_path: Path | None) -> MokuroRunResult:
        """Log a mokuro failure and build its result object."""
        if log_path is not None:
            self._log(f"Mokuro failed: {error} (full log: {log_path})")
        else:
            self._log(f"Mokuro failed: {error}")
        return MokuroRunResult(False, error, log_path)

    def _get_unique_path(self, path: Path) -> Path:
        """Get a unique path by adding a counter suffix.

        Args:
            path: Original path that may exist.

        Returns:
            Unique path that doesn't exist.
        """
        if not path.exists():
            return path

        stem = path.stem
        suffix = path.suffix
        parent = path.parent
        counter = 1

        while True:
            new_path = parent / f"{stem}_{counter}{suffix}"
            if not new_path.exists():
                return new_path
            counter += 1


def create_processor_from_config(
    storage_path: Path,
    status_callback: Callable[[str], None] | None = None,
    generations: Sequence[GenerationSpec] | None = None,
) -> OCRProcessor:
    """Create an OCR processor from configuration.

    Args:
        storage_path: Base storage path.
        status_callback: Optional status callback.
        generations: Configured OCR recipes (default: one mokuro row).

    Returns:
        Configured OCRProcessor instance.
    """
    return OCRProcessor(
        storage_path=storage_path,
        status_callback=status_callback,
        generations=generations,
    )
