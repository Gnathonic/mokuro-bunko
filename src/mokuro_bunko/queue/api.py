"""OCR queue status page for mokuro-bunko."""

from __future__ import annotations

import hashlib
import hmac
import json
import logging
import secrets
import threading
import time
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.library_index import LibraryIndexCache, LibrarySnapshot
from mokuro_bunko.middleware.auth import AUTH_RATE_LIMITER, parse_basic_auth_checked
from mokuro_bunko.ocr.generations import (
    GenerationSpec,
    default_generations,
    enabled_generations,
    primary_generation,
)
from mokuro_bunko.ocr.job_order import order_jobs
from mokuro_bunko.queue.shape import DEFAULT_LEVEL, PublicNames, normalize_level, shape_status
from mokuro_bunko.queue.state import QueueStateVersion
from mokuro_bunko.security import get_client_ip, is_within_path

if TYPE_CHECKING:
    from mokuro_bunko.ocr.control import OcrControl

STATIC_DIR = Path(__file__).parent / "web"

logger = logging.getLogger(__name__)

# A Basic header's result is trusted this long without another bcrypt check
# (the page polls every second): a success, and a failure. Any account
# change drops them all at once (`Database.users_version`).
AUTH_CACHE_SECONDS = 30.0
AUTH_FAIL_CACHE_SECONDS = 60.0
AUTH_CACHE_SIZE = 256
# How often the background refresher re-reads the pending list and the
# library snapshot, and how often it re-walks the missing-pages list.
REFRESH_SECONDS = 2.0
SKIPPED_TTL_SECONDS = 10.0
# ...and never sooner than this many of its own reads: the list walks every
# archive, seconds on a large library on a network share, and a compile
# asking for a re-read after every sidecar kept it walking back to back.
SKIPPED_COST_FACTOR = 8.0


@dataclass(frozen=True)
class Viewer:
    """Who polled: an authenticated ``role``, or a visitor.

    ``failed``: credentials were sent and are wrong (or unreadable);
    ``limited``: the rate limiter refused to check them. Either way the
    viewer is served exactly as a visitor.
    """

    role: str | None = None
    failed: bool = False
    limited: bool = False

MIME_TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "application/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
}


class QueueAPI:
    """WSGI middleware for OCR queue status page."""

    def __init__(
        self,
        app: Callable[..., Iterable[bytes]],
        storage_base_path: str,
        ocr_backend: str = "unknown",
        database: Any | None = None,
        queue_config: Any | None = None,
        library_index: LibraryIndexCache | None = None,
        generations: Sequence[GenerationSpec] | None = None,
        ocr_control: OcrControl | None = None,
    ) -> None:
        self.app = app
        self.storage_base_path = Path(storage_base_path)
        self.ocr_backend = ocr_backend
        self.generations: list[GenerationSpec] = (
            list(generations) if generations else default_generations()
        )
        self.database = database
        self._queue_config = queue_config
        # Live handle on the OCR worker: the source of the pending list.
        self._ocr_control = ocr_control
        # See `_get_status`: the serialized payload per (version, level,
        # viewer-is-admin), and the passive fingerprint last seen.
        self._own_state = QueueStateVersion()
        self._own_names = PublicNames()
        self._status_cond = threading.Condition(threading.Lock())
        # (level, admin) -> (version, etag, body) of the newest body built.
        self._latest: dict[tuple[str, bool], tuple[int, str, bytes]] = {}
        self._building: set[tuple[str, bool]] = set()
        self._last_fingerprint: tuple[Any, ...] | None = None
        # What the background refresher last read (see `_kick_refresh`).
        self._refresh_lock = threading.Lock()
        self._refreshing = False
        self._refreshed_at = float("-inf")
        self._snapshot: LibrarySnapshot | None = None
        self._snapshot_signature = ""
        self._skipped: list[dict[str, Any]] = []
        self._skipped_signature = "[]"
        self._skipped_read_at = float("-inf")
        # How long the last `skipped_missing_pages` read took, and when it
        # was (`invalidate_skipped` resets `_skipped_read_at`, not this).
        self._skipped_cost = 0.0
        self._skipped_last_read = float("-inf")
        self._auth_lock = threading.Lock()
        self._auth_key = secrets.token_bytes(32)
        self._auth_cache: dict[bytes, tuple[float, int, str | None]] = {}
        # How many payloads were built and queues planned: what the 304 path
        # exists to keep flat (and what its tests count).
        self.builds = 0
        self.plans = 0
        if library_index is not None:
            self._library_index = library_index
        else:
            self._library_index = LibraryIndexCache(self.storage_base_path / "library", ttl=30.0)

    @property
    def show_in_nav(self) -> bool:
        if self._queue_config is not None:
            return bool(getattr(self._queue_config, "show_in_nav", False))
        return False

    @property
    def public_access(self) -> bool:
        if self._queue_config is not None:
            return bool(getattr(self._queue_config, "public_access", True))
        return True

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")

        if path in ("/queue", "/queue/"):
            return self._serve_static(start_response, "index.html")
        elif path == "/queue/api/config" and method == "GET":
            return self._json_response(start_response, 200, {
                "show_in_nav": self.show_in_nav,
                "public_access": self.public_access,
                "display": self.display_level,
            })
        elif path == "/queue/api/status" and method == "GET":
            viewer = self._viewer(environ)
            if not self.public_access and viewer.role is None:
                return self._json_response(start_response, 401, {"error": "Authentication required"})
            return self._get_status(environ, start_response, viewer)
        elif path.startswith("/queue/"):
            filename = path[len("/queue/"):]
            return self._serve_static(start_response, filename)

        return self.app(environ, start_response)

    # --- who is asking ---------------------------------------------------

    def _viewer(self, environ: dict[str, Any]) -> Viewer:
        """Who is polling: an authenticated role, or a visitor.

        This endpoint sits outside `AuthMiddleware`, so it applies the same
        rules itself: every real check goes through the middleware's
        `AUTH_RATE_LIMITER`, keyed exactly as the middleware keys it
        (``<client ip>:<username>``), and a failure is recorded there.

        The page polls about once a second with the same header, so a
        result is remembered -- a success for `AUTH_CACHE_SECONDS`, a
        failure for `AUTH_FAIL_CACHE_SECONDS` -- keyed by an HMAC of the
        header under a per-process random key (never the header itself, and
        never a plain hash of it), and dropped the moment any account
        changes (`Database.users_version`). A stale stored password polled
        every second therefore costs one bcrypt per window, and a guessed one
        is one attempt against the same limiter the login page uses.

        A failed or rate-limited check is answered EXACTLY as a visitor is
        -- same body, same ETag -- plus an ``X-Queue-Auth`` header the page
        uses to drop the stored login. That says no more than a 401 from the
        login endpoint says under the same limit.
        """
        if self.database is None:
            return Viewer()
        auth_header = environ.get("HTTP_AUTHORIZATION")
        if not auth_header:
            return Viewer()
        digest = hmac.new(
            self._auth_key, auth_header.encode("utf-8", "replace"), hashlib.sha256
        ).digest()
        now = time.monotonic()
        users_version = getattr(self.database, "users_version", 0)
        with self._auth_lock:
            cached = self._auth_cache.get(digest)
            if cached is not None and cached[0] > now and cached[1] == users_version:
                role = cached[2]
                return Viewer(role=role, failed=role is None)
        creds, parse_error = parse_basic_auth_checked(auth_header)
        if parse_error or creds is None:
            # Garbage, or not Basic at all: no username to key a limit on,
            # nothing to check. A visitor whose stored login is useless.
            self._remember(digest, now, users_version, None)
            return Viewer(failed=True)
        username, password = creds
        key = f"{get_client_ip(environ)}:{username}"
        allowed, _retry_after = AUTH_RATE_LIMITER.allow_attempt(key)
        if not allowed:
            return Viewer(limited=True)
        user = self.database.authenticate_user(username, password)
        if user is None:
            AUTH_RATE_LIMITER.record_failure(key)
            self._remember(digest, now, users_version, None)
            return Viewer(failed=True)
        AUTH_RATE_LIMITER.record_success(key)
        role = str(user["role"])
        self._remember(digest, now, users_version, role)
        return Viewer(role=role)

    def _remember(self, digest: bytes, now: float, users_version: int, role: str | None) -> None:
        ttl = AUTH_CACHE_SECONDS if role is not None else AUTH_FAIL_CACHE_SECONDS
        with self._auth_lock:
            if len(self._auth_cache) >= AUTH_CACHE_SIZE:
                self._auth_cache = {k: v for k, v in self._auth_cache.items() if v[0] > now}
                if len(self._auth_cache) >= AUTH_CACHE_SIZE:
                    self._auth_cache.clear()
            self._auth_cache[digest] = (now + ttl, users_version, role)

    def _is_authenticated(self, environ: dict[str, Any]) -> bool:
        """Return True when request includes valid Basic auth credentials."""
        return self._viewer(environ).role is not None

    @property
    def display_level(self) -> str:
        """``queue.display``, read live so a saved setting applies at once."""
        return normalize_level(getattr(self._queue_config, "display", DEFAULT_LEVEL))

    @property
    def state(self) -> QueueStateVersion:
        control = self._ocr_control
        return control.queue_state if control is not None else self._own_state

    @property
    def public_names(self) -> PublicNames:
        """Visitors' machine aliases: the processor registry's, or our own."""
        registry = getattr(self._ocr_control, "remote", None)
        names = getattr(registry, "public_names", None)
        return names if isinstance(names, PublicNames) else self._own_names

    # --- keeping the inputs fresh, off the request path ---------------------

    def invalidate_skipped(self) -> None:
        """Re-read the missing-pages list at the next refresh (a compile finished)."""
        self._skipped_read_at = float("-inf")
        self._refreshed_at = float("-inf")

    def _kick_refresh(self) -> None:
        """Start one background refresh when the last is older than REFRESH_SECONDS.

        Everything that walks the library -- the pending list, the library
        snapshot, the missing-pages list -- is re-read HERE, in a thread of
        its own, never in a request: a poll only ever reads what the last
        refresh left. Single-flight: one refresh at a time. The very first
        poll of the process runs it inline, since there is nothing to show
        without it.
        """
        now = time.monotonic()
        with self._refresh_lock:
            if self._refreshing or now - self._refreshed_at < REFRESH_SECONDS:
                return
            self._refreshing = True
        if self._snapshot is None:
            self._refresh()
            return
        threading.Thread(target=self._refresh, name="queue-refresh", daemon=True).start()

    def _refresh(self) -> None:
        try:
            control = self._ocr_control
            if control is not None:
                control.refresh_pending()
            snapshot, _scans = self._library_index.get_snapshot_counted()
            self._snapshot = snapshot
            # What the page shows of the snapshot, not how often it was taken:
            # a rescan of an unchanged library is not a new version.
            self._snapshot_signature = hashlib.sha256(
                repr((snapshot.pending_thumbnails, snapshot.pending_ocr)).encode()
            ).hexdigest()
            now = time.monotonic()
            due = now - self._skipped_read_at >= SKIPPED_TTL_SECONDS
            # However it was asked for, never sooner than a few of its own reads.
            if now - self._skipped_last_read < SKIPPED_COST_FACTOR * self._skipped_cost:
                due = False
            if control is not None and due:
                self._skipped_read_at = now
                self._skipped_last_read = now
                started = time.monotonic()
                skipped = control.skipped_missing_pages()
                self._skipped_cost = time.monotonic() - started
                signature = json.dumps(skipped, sort_keys=True, default=str)
                if signature != self._skipped_signature:
                    # The fingerprint carries the signature: the next poll
                    # bumps the version and rebuilds with the new list.
                    self._skipped = skipped
                    self._skipped_signature = signature
        except Exception:  # noqa: BLE001 - a refresh must never break a poll
            logger.exception("queue page refresh failed")
        finally:
            with self._refresh_lock:
                self._refreshing = False
                self._refreshed_at = time.monotonic()

    def _passive_fingerprint(self) -> tuple[Any, ...]:
        """What can change the page WITHOUT the worker saying so, cheaply read.

        The worker bumps the state version itself for everything it does
        (`OCRWorker.queue_state`). This is the rest, and none of it walks
        anything: a stat of each file the worker writes, a signature of what
        the page shows of the library snapshot, the missing-pages list's signature, the
        processors connected (a connect or disconnect moves lanes and ETAs),
        a processor hold, a benchmark holding the queue, the display level.
        """
        control = self._ocr_control
        stats: list[Any] = []
        for name in (".ocr-progress.json", ".ocr-failures.json"):
            try:
                st = (self.storage_base_path / name).stat()
                stats.append((st.st_mtime_ns, st.st_size))
            except OSError:
                stats.append(None)
        hold = control.processing_hold() if control is not None else None
        paused = control.paused_for_benchmark() if control is not None else None
        registry = getattr(control, "remote", None)
        members: tuple[str, ...] = ()
        if registry is not None:
            try:
                members = tuple(sorted(str(e.processor_id) for e in registry.entries()))
            except Exception:  # noqa: BLE001 - a fingerprint never fails a poll
                members = ()
        return (
            tuple(stats),
            self._snapshot_signature,
            self._skipped_signature,
            members,
            json.dumps(hold, sort_keys=True, default=str),
            json.dumps(paused, sort_keys=True, default=str),
            self.display_level,
        )

    # --- the status endpoint ------------------------------------------------

    def _get_status(
        self, environ: dict[str, Any], start_response: Callable[..., Any], viewer: Viewer
    ) -> list[bytes]:
        """The shaped status, from cache whenever nothing the page shows moved.

        Keyed by (state version, level, viewer-is-admin): any number of
        visitors polling an unchanged queue cost one build between them, and
        a poll that already holds the current ETag costs no build and no
        body. A build runs OUTSIDE the lock, single-flight per (level,
        admin): while one thread builds the new version, every other poll is
        answered at once from the last body built (a 304 if it holds that
        one), never made to wait.
        """
        admin = viewer.role == "admin"
        level = self.display_level
        self._kick_refresh()
        fingerprint = self._passive_fingerprint()
        state = self.state
        slot = (level, admin)
        build = False
        with self._status_cond:
            if fingerprint != self._last_fingerprint:
                self._last_fingerprint = fingerprint
                state.bump()
            version = state.value
            while True:
                latest = self._latest.get(slot)
                if latest is not None and latest[0] == version:
                    break
                if slot not in self._building:
                    self._building.add(slot)
                    build = True
                    break
                if latest is not None:
                    break  # someone is building the new one: serve the last
                self._status_cond.wait(timeout=30.0)
        if build:
            try:
                self.builds += 1
                payload = shape_status(
                    self.raw_status(), level, admin=admin, public_names=self.public_names
                )
                body = json.dumps(payload, sort_keys=True).encode("utf-8")
                # The ETag is the BODY's, not the version's: a rebuild that
                # comes out byte for byte the same keeps its ETag, and every
                # viewer keeps getting 304s. The version only says when to
                # rebuild.
                digest = hashlib.sha256(body).hexdigest()[:20]
                etag = f'"{level}-{"a" if admin else "v"}-{digest}"'
                latest = (version, etag, body)
                with self._status_cond:
                    current = self._latest.get(slot)
                    if current is None or current[0] <= version:
                        self._latest[slot] = latest
            finally:
                with self._status_cond:
                    self._building.discard(slot)
                    self._status_cond.notify_all()
        assert latest is not None
        _version, etag, body = latest
        headers = [
            ("ETag", etag),
            # Per viewer (an admin's body differs), never in a shared cache.
            ("Cache-Control", "private, no-cache"),
            ("Vary", "Authorization"),
        ]
        if viewer.failed:
            headers.append(("X-Queue-Auth", "failed"))
        elif viewer.limited:
            headers.append(("X-Queue-Auth", "limited"))
        if_none_match = environ.get("HTTP_IF_NONE_MATCH", "")
        if etag in [tag.strip() for tag in if_none_match.split(",")]:
            start_response("304 Not Modified", headers)
            return []
        start_response("200 OK", [
            ("Content-Type", "application/json; charset=utf-8"),
            ("Content-Length", str(len(body))),
            *headers,
        ])
        return [body]

    def raw_status(self) -> dict[str, Any]:
        """Everything known about the queue, unredacted and unshaped.

        The input to `shape.shape_status`; never sent to a browser as is.
        """
        running = self._read_ocr_progress()
        # `ocr.concurrency` may have several jobs running at once. `current`
        # is the first of them and stays what it always was, for readers
        # that know one job; `current_jobs` is all of them.
        current = running[0] if running else None
        failed = self._read_ocr_failures()
        # The refresher's snapshot: a build never scans the library itself.
        snapshot = self._snapshot or self._library_index.cached_snapshot()
        if snapshot is None:
            snapshot = self._library_index.get_snapshot()

        # `pending_ocr` is the queue in PROCESSING order: `current_jobs` run
        # now, then the entries top to bottom. The list comes from the
        # scheduler itself and is passed through untouched. Never sort or
        # filter it here (or in queue.js): a second opinion about the order
        # is how the page came to disagree with the worker.
        # The worker's last computed list: the refresher recomputes it off the
        # request path and bumps the version when it moved.
        scheduled = self._ocr_control.last_pending() if self._ocr_control is not None else None
        order = self._ocr_control.generation_order() if self._ocr_control is not None else None
        if scheduled is None:
            scheduled = self._unscheduled_jobs(snapshot, running, failed)
        if order is None:
            order = self._generation_order()

        # When each of these will be done, recomputed from live state on every
        # BUILD (a changed state version): the rate of each row, the pages
        # already out of the volumes in flight, the sessions already open. The
        # server sends UTC only -- which clock a person reads it on is the
        # browser's business.
        queue_done_at: str | None = None
        self.plans += 1
        plan = (
            self._ocr_control.queue_plan(running, scheduled)
            if self._ocr_control is not None
            else None
        )
        if plan is not None:
            running = plan.running
            scheduled = plan.pending
            queue_done_at = plan.done_at
            current = running[0] if running else None

        return {
            "current": current,
            "current_jobs": running,
            "pending_ocr": scheduled,
            # When the LAST volume in this queue finishes, the running ones
            # included. Null whenever any item in it could not be predicted:
            # a queue with an unknown volume in it has an unknown end.
            "queue_done_at": queue_done_at,
            "pending_thumbnails": snapshot.pending_thumbnails,
            "failed": failed,
            "backend": self.ocr_backend,
            # The enabled generations in the order the queue runs them --
            # which is their order in the list, nothing else.
            "generations": order,
            # Volumes uploaded short of pages get their primary OCR and no
            # additional layers until the file is replaced. Listed so the skip
            # is never silent. A library walk: the refresher re-reads it.
            "skipped_missing_pages": list(self._skipped),
            # Non-null while a benchmark's line holds the OCR queue (see
            # `OCRWorker.preempt_for_bench`); `pending_jobs` above still
            # lists the volumes it interrupted -- they are not lost, only
            # waiting for the line to drain.
            "paused_for_benchmark": (
                self._ocr_control.paused_for_benchmark() if self._ocr_control is not None else None
            ),
            # Non-null while nothing can run: local processing off and no
            # processor connected (see `OCRWorker.processing_hold`).
            "processing_hold": (
                self._ocr_control.processing_hold()
                if self._ocr_control is not None
                else None
            ),
            # Rows that no connected machine can run -- a forced precision no
            # card supports -- each with its plain reason (admins only).
            "held_rows": (
                self._ocr_control.held_rows() if self._ocr_control is not None else []
            ),
            # Every machine that can run OCR now, idle ones included, with its
            # lane count: each keeps a card of a fixed size while it is here.
            "connected_machines": (
                self._ocr_control.connected_machines() if self._ocr_control is not None else []
            ),
            # REAL pages per minute (finished volumes' pages over their wall
            # seconds) per generation, per machine and combined. Shaped down
            # to one combined line per layer, at `detailed` only.
            "speed": (
                self._ocr_control.speed_report(running) if self._ocr_control is not None else []
            ),
        }

    def _read_ocr_failures(self) -> list[dict[str, Any]]:
        """Read persisted OCR failure records written by the OCR worker."""
        failures_path = self.storage_base_path / ".ocr-failures.json"
        if not failures_path.exists():
            return []
        try:
            data = json.loads(failures_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return []
        if not isinstance(data, dict):
            return []
        failed = []
        for entry in data.values():
            if not isinstance(entry, dict):
                continue
            failed.append({
                "series": entry.get("series"),
                "volume": entry.get("volume"),
                "generation": entry.get("generation"),
                "engine": entry.get("engine"),
                "detector": entry.get("detector"),
                "error": entry.get("error"),
                "attempts": entry.get("attempts", 1),
                "last_attempt_at": entry.get("last_attempt_at"),
                "log_file": entry.get("log_file"),
            })
        failed.sort(key=lambda e: (e.get("series") or "", e.get("volume") or ""))
        return failed

    def _read_ocr_progress(self) -> list[dict[str, Any]]:
        """The jobs the OCR worker reports as running (see `read_running_jobs`)."""
        return read_running_jobs(self.storage_base_path)

    @staticmethod
    def _progress_entry(data: dict[str, Any]) -> dict[str, Any]:
        """One running job as the queue page reads it.

        ``pipeline`` is the running volume's stage readout, summarized by
        `ocr.pipeline_stats` from what the runner publishes: one row a
        stage, the bottleneck, and a verdict when the numbers support one.
        Present only while a staged engine is running and reporting -- the
        `mokuro` engine has no stages, an older worker sends none, and the
        page renders the same job without it either way.
        """
        pipeline = data.get("pipeline")
        entry = {
            "series": data.get("series"),
            "volume": data.get("volume"),
            "generation": data.get("generation"),
            "engine": data.get("engine"),
            "detector": data.get("detector"),
            "percent": data.get("percent", 0),
            "eta_seconds": data.get("eta_seconds"),
            "done_pages": data.get("done_pages", 0),
            "total_pages": data.get("total_pages"),
            "status": data.get("status", "running"),
            # The raw signals the ETA is re-derived from on every poll. They
            # are the job's own facts, not a computed answer: which row it
            # belongs to, which lane is feeding it, when its FIRST page came
            # out (never earlier -- see `ocr.eta`) and when its session began
            # paying its startup.
            "generation_id": data.get("generation_id"),
            "slot": data.get("slot"),
            "first_page_at": data.get("first_page_at"),
            # When the session began paying its startup, and -- for the
            # window before the runner has said anything at all -- when this
            # job's card was opened. Without the second one a volume that has
            # not reached its first event shows the FULL startup on every
            # poll instead of counting down.
            "session_started_at": data.get("session_started_at"),
            "started_at": data.get("started_at"),
            # Whether that session has said it is ready, and whether its
            # runner has this volume: the card's Loading / Waiting / Running
            # (`shape.job_state`). None on a card from before them.
            "session_ready": data.get("session_ready"),
            "delivered": data.get("delivered"),
            # Filled in by the prediction below; present and null on a server
            # with no OCR worker, so the page never has to guess whether a
            # field exists.
            "eta_at": None,
            "rate_pages_per_second": data.get("rate_pages_per_second"),
            # What this row costs per VOLUME whatever its length -- the
            # pipeline filling and draining around it. Charged once per
            # volume, and already spent once a page has come out.
            "latency_seconds": data.get("latency_seconds"),
            "rate_source": data.get("rate_source"),
            # Whose hardware: a processor's label, or None for this machine.
            "processor": data.get("processor"),
            # ...and the MACHINE, by the name its numbers are filed under
            # ("local", or the processor's name): the key the queue plan
            # puts the job on that machine's lane and prices it with.
            # Without it every running job took the first free lane --
            # this server's -- at this server's rate.
            "machine": data.get("machine"),
        }
        if isinstance(pipeline, dict) and pipeline.get("stages"):
            entry["pipeline"] = pipeline
        return entry

    def _generation_order(self) -> list[dict[str, Any]]:
        """The enabled rows in run order, for a server with no OCR worker."""
        return [
            {
                "id": row.id,
                "name": row.name,
                "engine": row.engine,
                "detector": row.reported_detector,
            }
            for row in enabled_generations(self.generations)
        ]

    def _unscheduled_jobs(
        self,
        snapshot: LibrarySnapshot,
        running: list[dict[str, Any]],
        failed: list[dict[str, Any]],
    ) -> list[dict[str, Any]]:
        """Missing sidecars per (volume, generation) when NO OCR worker runs.

        Only for a server whose OCR is disabled (backend ``skip``, or the OCR
        environment failed to install): nothing schedules these jobs, so the
        library index is the one source left. They go through the same
        `order_jobs` as the worker's queue, so the list reads the same way.
        Jobs with a failure record are in the failed list instead, and the
        jobs the progress file reports as running are not repeated.
        """
        rows = enabled_generations(self.generations)
        primary = primary_generation(self.generations)
        by_id = {row.id: row for row in rows}
        missing: list[tuple[str, str, str]] = []
        for series in snapshot.series:
            for volume in series.volumes:
                if not volume.has_cbz:
                    continue
                for row in rows:
                    if primary is not None and row.id == primary.id:
                        done = volume.has_mokuro or volume.has_mokuro_gz
                    else:
                        done = row.name in volume.sidecars
                    if not done:
                        # Every row a volume lacks, as the worker would list
                        # it: no layer waits for the primary.
                        missing.append((series.name, volume.name, row.id))

        hidden = {
            (entry.get("series"), entry.get("volume"), entry.get("generation"))
            for entry in failed
        }
        for entry in running:
            hidden.add((entry.get("series"), entry.get("volume"), entry.get("generation")))
        ranked = {row.id: rank for rank, row in enumerate(rows)}
        jobs = order_jobs(
            [
                job
                for job in missing
                if (job[0], job[1], by_id[job[2]].name) not in hidden
            ],
            ranked,
        )
        return [
            {
                "series": s,
                "volume": v,
                "generation": by_id[gen_id].name,
                "generation_id": gen_id,
                "engine": by_id[gen_id].engine,
                "detector": by_id[gen_id].reported_detector,
                # No worker means no measured rate and no page-count cache
                # handle, so these stay unknown rather than guessed.
                "pages": None,
                "eta_seconds": None,
                "eta_at": None,
                "rate_source": None,
                "latency_seconds": None,
                "rough": False,
                "reason": "this server is not running OCR",
            }
            for s, v, gen_id in jobs
        ]

    def _serve_static(
        self, start_response: Callable[..., Any], filename: str
    ) -> list[bytes]:
        if not filename or ".." in filename:
            return self._error_response(start_response, 404, "Not found")
        file_path = STATIC_DIR / filename
        if not file_path.is_file() or not is_within_path(file_path, STATIC_DIR):
            return self._error_response(start_response, 404, "Not found")
        suffix = file_path.suffix.lower()
        content_type = MIME_TYPES.get(suffix, "application/octet-stream")
        try:
            content = file_path.read_bytes()
            start_response("200 OK", [
                ("Content-Type", content_type),
                ("Content-Length", str(len(content))),
                ("Cache-Control", "no-cache"),
            ])
            return [content]
        except OSError:
            return self._error_response(start_response, 500, "Error")

    @staticmethod
    def _json_response(
        start_response: Callable[..., Any],
        status_code: int,
        data: Any,
    ) -> list[bytes]:
        body = json.dumps(data).encode("utf-8")
        status = f"{status_code} OK" if status_code == 200 else f"{status_code} Error"
        start_response(status, [
            ("Content-Type", "application/json; charset=utf-8"),
            ("Content-Length", str(len(body))),
            ("Cache-Control", "no-cache"),
        ])
        return [body]

    @staticmethod
    def _error_response(
        start_response: Callable[..., Any],
        status_code: int,
        message: str,
    ) -> list[bytes]:
        body = message.encode("utf-8")
        start_response(f"{status_code} {message}", [
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Content-Length", str(len(body))),
        ])
        return [body]


def read_running_jobs(storage_base_path: Path) -> list[dict[str, Any]]:
    """The jobs the OCR worker reports as running, in the order they started.

    The worker writes one flat job object with `active`, plus `jobs` when it
    has more than one slot. A file without `jobs` is one job: that is what
    every version before `ocr.concurrency` wrote. Each entry is shaped by
    `QueueAPI._progress_entry`, the form `OCRWorker.queue_plan` prices.
    """
    progress_path = Path(storage_base_path) / ".ocr-progress.json"
    try:
        data = json.loads(progress_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return []
    if not isinstance(data, dict) or not data.get("active"):
        return []
    entries = data.get("jobs")
    if not isinstance(entries, list) or not entries:
        entries = [data]
    return [QueueAPI._progress_entry(entry) for entry in entries if isinstance(entry, dict)]
