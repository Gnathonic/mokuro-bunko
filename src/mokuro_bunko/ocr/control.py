"""Apply OCR settings changes to a running server.

The admin panel saves ``ocr.generations`` and ``ocr.poll_interval`` to the
config file; this object pushes the same values into the live OCR worker and
queue page so the change takes effect without a restart whenever the
environments the new settings need are already installed. When they are not,
it says what is missing and whether it is being installed in the background.
"""

from __future__ import annotations

import logging
import threading
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.ocr.engines import GpuUse, backend_is_gpu, get_detector, uses_mokuro_env
from mokuro_bunko.ocr.generations import (
    ENV_ENGINES,
    ENV_MOKURO,
    GenerationSpec,
    detector_env_key,
    enabled_generations,
    local_environment_problem,
    required_detectors,
    required_engines,
)
from mokuro_bunko.ocr.volume_outlook import pending_entries
from mokuro_bunko.queue.shape import QUEUE_REPORT_LIMIT
from mokuro_bunko.queue.state import QueueStateVersion

if TYPE_CHECKING:
    from mokuro_bunko.ocr.bench import BenchService
    from mokuro_bunko.ocr.installer import EnginesInstaller, OCRInstaller
    from mokuro_bunko.ocr.watcher import OCRWorker
    from mokuro_bunko.queue.api import QueueAPI

logger = logging.getLogger(__name__)


class OcrControl:
    """Live handle on the OCR worker, queue page and installers.

    Created before the WSGI app so the admin API can hold it; the server
    binds the worker and installers once they exist.
    """

    def __init__(self) -> None:
        # The queue page's state version, shared by everything that changes
        # what that page shows: the worker (bound below), the queue page's own
        # passive checks and the admin API's queue settings.
        self.queue_state = QueueStateVersion()
        self._worker: OCRWorker | None = None
        self.queue_api: QueueAPI | None = None
        # Set by the admin API the first time it builds its BenchService
        # (`_bench_service()`), the same way `queue_api` above is set the
        # other direction: `QueueAPI` has no bench state of its own and
        # reads `paused_for_benchmark` off this SAME instance.
        self.bench: BenchService | None = None
        # How to BUILD that service when nothing has asked for one yet: the
        # admin API's own `_bench_service`, registered when it is created.
        # The worker's auto-bench (spec section 4) reaches it through
        # `bench_service()` rather than waiting for someone to open the
        # admin panel first. None: no admin panel, so no benchmarks.
        self.bench_factory: Callable[[], BenchService | None] | None = None
        # The ProcessorRegistry, set by `create_app` when it mounts the
        # processor channels, so the worker and the admin API can reach the
        # connected hardware through the handle they already hold. Typed
        # loosely on purpose: `control` must not import the remote package,
        # which imports back into the OCR stack. Task 9 is its first reader.
        self.remote: Any = None
        self.mokuro_installer: OCRInstaller | None = None
        self.engines_installer: EnginesInstaller | None = None
        # The server's ocr_runtime status dict (shared with the admin API).
        self.runtime: dict[str, Any] | None = None
        # Backend the server selected at start (an ``OCRBackend`` value, after
        # ``auto`` was resolved). Only what an environment is ASSUMED to run
        # on until it says otherwise: see `resolve_gpu`. None: not decided.
        self.selected_backend: str | None = None
        # The last `resolve_gpu` answer: which environments run on a GPU.
        # Reported by the admin panel and warned about at startup when it
        # disagrees with the backend that was selected. It decides nothing
        # about the queue, whose order is the generations list. None: never
        # resolved.
        self.gpu: GpuUse | None = None
        # Environment ("mokuro" / "engines") -> backend its torch reported.
        self._env_backends: dict[str, str] = {}
        self._lock = threading.Lock()
        self._install_thread: threading.Thread | None = None

    @property
    def worker(self) -> OCRWorker | None:
        return self._worker

    @worker.setter
    def worker(self, worker: OCRWorker | None) -> None:
        # The worker bumps the SAME counter the queue page reads.
        if worker is not None:
            worker.queue_state = self.queue_state
        self._worker = worker
        self.queue_state.bump()

    def refresh_pending(self) -> None:
        """Let the worker notice a moved pending list (cheap while its cache is good)."""
        worker = self._ocr_worker()
        if worker is not None:
            worker.refresh_pending()

    def start_backoffs(self, machine: str) -> list[dict[str, Any]]:
        """Rows whose runner will not start on ``machine`` right now (F9)."""
        worker = self._ocr_worker()
        return worker.start_backoffs(machine) if worker is not None else []

    def connected_machines(self) -> list[dict[str, Any]]:
        """``[{"machine", "slots"}]`` for every machine that can run OCR ([] without a worker)."""
        worker = self._ocr_worker()
        return worker.connected_machines() if worker is not None else []

    def speed_report(self, running: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
        """Real pages per minute per generation and machine ([] without an OCR worker)."""
        worker = self._ocr_worker()
        return worker.speed_report(running) if worker is not None else []

    @property
    def installing(self) -> bool:
        thread = self._install_thread
        return thread is not None and thread.is_alive()

    def _ocr_worker(self) -> OCRWorker | None:
        """The worker, when it really schedules OCR (not the cover-only one)."""
        worker = self.worker
        if worker is None or worker.thumbnails_only:
            return None
        return worker

    def skipped_missing_pages(self) -> list[dict[str, Any]]:
        """Volumes whose extra layers are skipped for missing pages ([] without a worker)."""
        worker = self._ocr_worker()
        return worker.skipped_missing_pages() if worker is not None else []

    def paused_for_benchmark(self) -> dict[str, Any] | None:
        """``{"key", "generation", "queued"}`` while a benchmark holds the OCR queue.

        None when no `BenchService` has been built yet (no benchmark has
        ever been requested this process) or its bench queue is empty.
        """
        bench = self.bench
        return bench.paused_for_benchmark() if bench is not None else None

    def bench_service(self) -> BenchService | None:
        """The benchmark line, built on first use; None without an admin API."""
        if self.bench is not None:
            return self.bench
        factory = self.bench_factory
        return factory() if factory is not None else None

    def last_pending(self) -> list[dict[str, Any]] | None:
        """The worker's last computed queue without recomputing (see its doc)."""
        worker = self._ocr_worker()
        return worker.last_pending() if worker is not None else None

    def pending_jobs(self) -> list[dict[str, Any]] | None:
        """The worker's queue in processing order, or None without an OCR worker.

        This is the scheduler's own list (``OCRWorker.pending_jobs``): the
        queue page shows it as is, so it cannot drift from what runs next.
        """
        worker = self._ocr_worker()
        return worker.pending_jobs() if worker is not None else None

    def processors(self) -> list[dict[str, Any]]:
        """Every processor entry, the local one first, as the admin panel shows it."""
        registry = self.remote
        return [] if registry is None else [e.to_dict() for e in registry.entries()]

    def processing_hold(self) -> dict[str, Any] | None:
        """Why the queue holds for want of hardware, or None (see the worker's)."""
        worker = self._ocr_worker()
        return None if worker is None else worker.processing_hold()

    def queue_plan(
        self,
        running: Sequence[dict[str, Any]],
        pending: Sequence[dict[str, Any]],
    ) -> Any | None:
        """When each running and queued volume will be done, or None with no worker.

        The prediction needs what only the worker has: the measured page rate
        of every row, what a session start costs on this machine, and how many
        lanes there are. A server with OCR disabled has none of that and says
        so by leaving every ETA null rather than inventing one.
        """
        worker = self._ocr_worker()
        return worker.queue_plan(running, pending) if worker is not None else None

    def archive_arrived(self, cbz_path: Path) -> None:
        """Queue an archive just written over WebDAV now (see `OCRWorker.archive_arrived`).

        Nothing at all without an OCR worker (OCR off, or cover-only).
        """
        worker = self._ocr_worker()
        if worker is not None:
            worker.archive_arrived(cbz_path)

    def archive_removed(self, path: Path) -> None:
        """An archive (or a folder of them) left the library over WebDAV: stop its OCR.

        See `OCRWorker.archive_removed`. Nothing without an OCR worker.
        """
        worker = self._ocr_worker()
        if worker is not None:
            worker.archive_removed(path)

    def volume_pending(
        self,
        cbz_path: Path,
        series: str,
        volume: str,
        running: Sequence[dict[str, Any]],
        *,
        wait: float,
        max_items: int | None = None,
    ) -> list[dict[str, Any]] | None:
        """``[{"kind", "id", "eta"}]``: the OCR this volume is still owed, and when.

        None without an OCR worker -- nothing will run, so nothing is
        pending. Each eta is the queue plan's own (the queue page's numbers),
        from the running jobs and the pending list; the pending list is used
        only if it can be had within ``wait`` seconds (`pending_within`), and
        without it only the running jobs are priced. ``max_items`` bounds the
        plan's walk: a volume queued further back than that is left unpriced
        rather than walking the whole queue for it. While no machine can run
        anything (`processing_hold`) nothing is priced.
        """
        worker = self._ocr_worker()
        if worker is None:
            return None
        owed = worker.owed_generations(cbz_path)
        if not owed:
            return []
        planned: list[dict[str, Any]] = []
        if self.queue_hold() is None:
            try:
                planned = worker.volume_plan(
                    cbz_path,
                    series,
                    volume,
                    owed,
                    running,
                    worker.pending_within(wait),
                    max_items=max_items,
                )
            except Exception:  # noqa: BLE001 - an estimate never fails an upload or a manifest
                logger.exception("pricing %s/%s failed", series, volume)
                planned = []
        return pending_entries(owed, series, volume, planned)

    def held_rows(self) -> list[dict[str, str]]:
        """Rows no connected machine can run (their precision mode), with why."""
        worker = self._ocr_worker()
        if worker is None:
            return []
        try:
            return worker.held_rows()
        except Exception:  # noqa: BLE001 - a status read never fails a poll
            logger.exception("working out the held rows failed")
            return []

    def autobench_failed(self) -> set[tuple[str, str]]:
        """``(machine, row id)`` whose automatic benchmark could not be had.

        ``machine`` is ``"local"`` for this server, else a processor's name.
        """
        worker = self._ocr_worker()
        if worker is None:
            return set()
        from mokuro_bunko.ocr.remote.profiles import LOCAL_PROFILE

        return {
            ("local" if name == LOCAL_PROFILE else name, gen_id)
            for name, gen_id in set(getattr(worker, "_autobench_failed", set()))
        }

    def precision_holds(self) -> dict[str, str]:
        """Row id -> why no connected machine can run it; {} without a worker."""
        worker = self._ocr_worker()
        if worker is None:
            return {}
        try:
            return worker.precision_holds()
        except Exception:  # noqa: BLE001 - a status read never fails a poll
            logger.exception("working out the precision holds failed")
            return {}

    def queue_hold(self) -> str | None:
        """Why the WHOLE queue is not moving, as a plain code, or None.

        ``no-processor``: nothing can run OCR (local processing off, no
        processor connected). ``benchmarking``: every machine is held and a
        benchmark holds one. ``paused``: every machine is held otherwise (a
        hold, or downloads refused on every processor).
        """
        worker = self._ocr_worker()
        if worker is None:
            return None
        if worker.processing_hold() is not None:
            return "no-processor"
        if worker._every_machine_held():
            return "benchmarking" if self.paused_for_benchmark() is not None else "paused"
        return None

    def queue_document(
        self, running: Sequence[dict[str, Any]], *, wait: float
    ) -> tuple[str | None, list[dict[str, Any]], int]:
        """``(held, volumes, pending_volumes)`` for the queue file (`middleware.queue_file`).

        Every volume with OCR running, then the next `QUEUE_REPORT_LIMIT`
        volumes waiting, in queue order; ``pending_volumes`` counts ALL the
        waiting ones (the plan still orders and prices the whole queue).
        Each volume is ``(series, volume, jobs)``: one job per row the
        volume is owed (primary first, then list order) and per row running
        on it, as ``{"kind", "id", "state", "eta", "progress"}``. Priced from
        the same plan as the manifest (`OCRWorker.plan_items`). A job the plan
        does not list (a failure
        backoff) is ``held``, and every queued job is while the whole queue
        is. Nothing about machines or errors.
        """
        worker = self._ocr_worker()
        if worker is None:
            return None, [], 0
        held = self.queue_hold()
        running = worker._with_known_totals(running)
        pending = worker.pending_within(wait) or []
        items = worker.plan_items(pending, running)
        try:
            plan = worker.queue_plan(running, items)
            priced_running, priced_pending = plan.running, plan.pending
        except Exception:  # noqa: BLE001 - an estimate never fails the file
            logger.exception("pricing the queue file failed")
            priced_running, priced_pending = list(running), list(items)

        order: list[tuple[str, str]] = []
        jobs: dict[tuple[str, str], dict[str, dict[str, Any]]] = {}
        running_now: set[tuple[str, str, str]] = set()
        for entry, is_running in [
            *((entry, True) for entry in priced_running),
            *((entry, False) for entry in priced_pending),
        ]:
            series, volume, gen_id = (
                entry.get("series"), entry.get("volume"), entry.get("generation_id")
            )
            if not (isinstance(series, str) and isinstance(volume, str) and isinstance(gen_id, str)):
                continue
            key = (series, volume)
            if key not in jobs:
                order.append(key)
                jobs[key] = {}
            if is_running:
                running_now.add((series, volume, gen_id))
            jobs[key].setdefault(gen_id, entry)

        library = worker.storage_path / "library"
        rank = worker._generation_rank()
        volumes: list[dict[str, Any]] = []
        # Every pending volume's rows at once, from the worker's shared walk:
        # asking each of 12k volumes in turn stat'ed every sidecar again.
        owed_all = worker.owed_by_volume()
        running_volumes = {(series, volume) for series, volume, _ in running_now}
        pending_volumes = sum(1 for key in order if key not in running_volumes)
        waiting_listed = 0
        for series, volume in order:
            if (series, volume) not in running_volumes:
                if waiting_listed >= QUEUE_REPORT_LIMIT:
                    continue
                waiting_listed += 1
            listed = jobs[(series, volume)]
            owed = {row.id: row for row in owed_all.get(library / series / f"{volume}.cbz", [])}
            rows = {**owed}
            for gen_id in listed:
                row = worker._generation(gen_id)
                if row is not None:
                    rows.setdefault(gen_id, row)
            out: list[dict[str, Any]] = []
            for row in sorted(rows.values(), key=lambda r: (not r.primary, rank.get(r.id, len(rank)))):
                priced = listed.get(row.id)
                is_running = (series, volume, row.id) in running_now
                if is_running:
                    state = "running"
                elif priced is None or held is not None or priced.get("held"):
                    # Not priced (a backoff), the whole queue held, or a row
                    # no connected machine can run (its precision mode).
                    state = "held"
                else:
                    state = "queued"
                eta = priced.get("eta_at") if priced is not None and state != "held" else None
                progress: float | None = None
                if is_running and priced is not None:
                    done = priced.get("done_pages")
                    total = priced.get("total_pages")
                    if isinstance(done, (int, float)) and isinstance(total, (int, float)) and total > 0:
                        progress = round(max(0.0, min(1.0, float(done) / float(total))), 3)
                out.append({
                    "kind": "ocr" if row.primary else "layer",
                    "id": row.name,
                    "state": state,
                    "eta": eta if isinstance(eta, str) else None,
                    "progress": progress,
                })
            if out:
                volumes.append({"series": series, "volume": volume, "jobs": out})
        return held, volumes, pending_volumes

    def generation_order(self) -> list[dict[str, Any]] | None:
        """The enabled generations in the order the worker runs them."""
        worker = self._ocr_worker()
        return worker.generation_order() if worker is not None else None

    def resolve_gpu(self, engines: Sequence[str]) -> GpuUse | None:
        """Which OCR environments run on a GPU, judged by the backend IN USE.

        The selected backend is only an intention: when the GPU wheels fail
        to install, `install_with_fallback` leaves a CPU torch behind, and an
        environment built earlier keeps whatever it was built with. So each
        environment `engines` needs is asked what its torch runs on
        (`get_installed_backend`); `selected_backend` stands in only for an
        environment that gives no answer or that no engine uses yet.

        The probe imports torch in a subprocess (seconds), so an answer is
        kept for the life of the process, and this is called at start and on
        a live settings change, never for a page request (those read `gpu`).
        Returns None, and decides nothing, before the server selected a
        backend.
        """
        selected = backend_is_gpu(self.selected_backend)
        if selected is None:
            return None
        needs_mokuro_env = any(uses_mokuro_env(e) for e in engines)
        needs_engines_env = any(not uses_mokuro_env(e) for e in engines)
        self.gpu = GpuUse(
            mokuro_env=self._env_on_gpu(
                "mokuro", self.mokuro_installer, needs_mokuro_env, selected
            ),
            engines_env=self._env_on_gpu(
                "engines", self.engines_installer, needs_engines_env, selected
            ),
        )
        return self.gpu

    def record_env_backend(self, env: str, backend: str) -> None:
        """Note the backend an environment ("mokuro" / "engines") reported.

        For a caller that has just probed it anyway: `resolve_gpu` then does
        not start the same subprocess a second time.
        """
        self._env_backends[env] = backend

    def _env_on_gpu(
        self, env: str, installer: OCRInstaller | None, probe: bool, assumed: bool
    ) -> bool:
        if env not in self._env_backends and probe and installer is not None:
            backend = installer.get_installed_backend()
            if backend is not None:
                self._env_backends[env] = backend.value
        in_use = backend_is_gpu(self._env_backends.get(env))
        return assumed if in_use is None else in_use

    def apply(
        self,
        generations: Sequence[GenerationSpec],
        poll_interval: float | None = None,
    ) -> dict[str, Any]:
        """Push a new generations list into the running worker.

        Returns ``{"applied", "installing", "restart_required", "reason"}``:
        applied when the worker took the settings; installing when the extras
        of a detector some row now needs are being added in the background
        and the settings apply once that finishes; restart_required when a
        whole OCR environment is missing HERE (the server installs
        environments at start).

        A missing environment is this server's gap, not the queue's: the
        library's own hardware is just another processor entry (spec
        section 0), so the settings still apply at once -- every row stays
        in the queue for the processors whose catalogs can run it, and only
        this server's slots leave the rows that need the missing
        environment alone until a restart installs it. That answer is
        ``applied`` AND ``restart_required``, with the reason.

        The detectors checked are the UNION over the enabled rows -- a row's
        own detector when its engine brings one, the configured one
        otherwise -- because with a detector per generation the set really
        does move on an edit.
        """
        rows = list(generations)
        with self._lock:
            worker = self.worker
            if worker is None or worker.thumbnails_only:
                return self._result(
                    restart_required=True,
                    reason="OCR is disabled in this server process",
                )
            if not getattr(worker, "local_processing", True):
                # This box runs no OCR of its own, so no environment of its
                # has to be ready: every processor checks the rows against
                # its own catalog, from its next claim.
                self._apply_now(worker, rows, poll_interval, {})
                return self._result(applied=True)
            problems: dict[str, str] = {}
            engines = required_engines(rows)
            if any(uses_mokuro_env(e) for e in engines) and not self._mokuro_env_ready():
                problems[ENV_MOKURO] = (
                    "the mokuro environment is not installed on this server; "
                    "it installs on restart"
                )
            extra = [e for e in engines if not uses_mokuro_env(e)]
            if extra:
                installer = self.engines_installer
                if installer is None or not installer.is_installed():
                    problems[ENV_ENGINES] = (
                        f"the engines environment for {', '.join(extra)} is not "
                        "installed on this server; it installs on restart"
                    )
                else:
                    # Asked per detector rather than through `has_detector()`
                    # with no argument, so the installer is not reconfigured
                    # before we know these settings are the ones that will
                    # apply (see `_apply_now`).
                    missing = [
                        d for d in required_detectors(rows) if not installer.has_detector(d)
                    ]
                    if missing:
                        spec = get_detector(missing[0])
                        if self.installing:
                            return self._result(
                                installing=True,
                                reason=(
                                    f"still installing {spec.id}; settings apply when it "
                                    "is ready"
                                ),
                            )
                        self._install_thread = threading.Thread(
                            target=self._install_then_apply,
                            args=(installer, rows, poll_interval, missing, problems),
                            name="ocr-detector-install",
                            daemon=True,
                        )
                        self._install_thread.start()
                        return self._result(
                            installing=True,
                            reason=(
                                f"installing {spec.id} ({', '.join(spec.extra_packages)}); "
                                "settings apply when it is ready"
                            ),
                        )
            self._apply_now(worker, rows, poll_interval, problems)
            if problems:
                return self._result(
                    applied=True,
                    restart_required=True,
                    reason=self._local_gap_reason(rows, problems),
                )
            return self._result(applied=True)

    @staticmethod
    def _local_gap_reason(rows: Sequence[GenerationSpec], problems: dict[str, str]) -> str:
        """What this server cannot run, why, and who runs it meanwhile."""
        names = [
            row.name
            for row in enabled_generations(rows)
            if local_environment_problem(problems, row) is not None
        ]
        why = "; ".join(dict.fromkeys(problems.values()))
        if not names:
            return why
        return f"{why} — until then only a connected processor runs {', '.join(names)}"

    def _mokuro_env_ready(self) -> bool:
        installer = self.mokuro_installer
        return installer is not None and installer.is_installed()

    def _install_then_apply(
        self,
        installer: EnginesInstaller,
        generations: list[GenerationSpec],
        poll_interval: float | None,
        missing: Sequence[str],
        problems: dict[str, str] | None = None,
    ) -> None:
        # Every detector is tried: one that fails takes only ITS rows off
        # this server, and the settings apply regardless -- a processor
        # with that detector must not wait for this box's install.
        gaps = dict(problems or {})
        failed = [d for d in missing if not installer.install_detector(d)]
        for detector in failed:
            gaps[detector_env_key(detector)] = (
                f"the {detector} detector failed to install on this server"
            )
        with self._lock:
            worker = self.worker
            if worker is None:
                return
            if failed:
                logger.error(
                    "Detector install (%s) failed; the generations that need it run "
                    "only on connected processors until it installs",
                    ", ".join(failed),
                )
            self._apply_now(worker, generations, poll_interval, gaps)

    def _apply_now(
        self,
        worker: OCRWorker,
        generations: list[GenerationSpec],
        poll_interval: float | None,
        local_unavailable: dict[str, str],
    ) -> None:
        worker.apply_settings(
            generations, poll_interval=poll_interval, local_unavailable=local_unavailable
        )
        if self.queue_api is not None:
            self.queue_api.generations = list(generations)
        if self.engines_installer is not None:
            # Both halves, together, and only now that the settings are the
            # live ones: `has_detector()` and `install_detector()` iterate
            # `detectors`, and setting only the scalar left the readiness
            # check testing the detector from before the change.
            self.engines_installer.set_detectors(required_detectors(generations))
        if self.runtime is not None:
            rows = [row.to_dict() for row in generations]
            self.runtime["generations"] = rows
            # "Active" is what runs HERE; the rest waits for a processor.
            self.runtime["active_generations"] = [
                row.to_dict()
                for row in generations
                if local_environment_problem(local_unavailable, row) is None
            ]
            self.runtime["detectors"] = list(required_detectors(generations))
            self.runtime["detector_ready"] = True
        # A row added live may be the first to use its environment.
        self.resolve_gpu(required_engines(generations))
        logger.info(
            "OCR settings applied live: generations=%s",
            ",".join(row.name for row in enabled_generations(generations)),
        )

    @staticmethod
    def _result(
        applied: bool = False,
        installing: bool = False,
        restart_required: bool = False,
        reason: str = "",
    ) -> dict[str, Any]:
        return {
            "applied": applied,
            "installing": installing,
            "restart_required": restart_required,
            "reason": reason,
        }
