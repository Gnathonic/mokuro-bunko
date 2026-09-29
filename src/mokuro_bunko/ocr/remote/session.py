"""A runner session on another machine, shaped like a local one.

This module is the whole of the remote/local seam. Everything above the
session layer -- ``OCRWorker._run_session``, ``_handle_session_event``,
``_collect_session_volume`` -- is written against
:class:`~mokuro_bunko.ocr.session.OcrSession`, and :class:`RemoteSession`
answers the same calls without any of them learning that the runner is in
another room.

It lives beside the registry rather than inside it because the two have
different jobs: :mod:`mokuro_bunko.ocr.remote.registry` remembers WHO is
connected, this module drives ONE piece of work on one of them.
"""

from __future__ import annotations

import logging
import os
import queue
import threading
import time
from collections.abc import Callable, Mapping
from contextlib import suppress
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.ocr.remote.protocol import (
    ARCHIVES_ROOT,
    EVENTS_OPEN_SECONDS,
    clean_archives_root,
    is_event,
)

if TYPE_CHECKING:
    from mokuro_bunko.ocr.generations import GenerationSpec
    from mokuro_bunko.ocr.remote.registry import ProcessorEntry
    from mokuro_bunko.ocr.session import SessionVolume

logger = logging.getLogger(__name__)

# Spec section 3 rule 3: each session keeps at most two outstanding volumes.
# The worker's own SESSION_LOOKAHEAD says the same thing; this is the wire's
# copy of it, so a bug on the scheduling side cannot flood a processor.
MAX_OUTSTANDING_VOLUMES = 2


class RemoteSession:
    """A runner session on another machine, shaped like a local one.

    ``OCRWorker._run_session`` and ``_handle_session_event`` do not know the
    difference: they get the same events out of ``poll_event``, hand the
    same ``SessionVolume`` to ``submit``, and find the finished sidecar at
    the same local path. What this class adds over
    :class:`~mokuro_bunko.ocr.session.OcrSession` is the two translations
    that cross the wire:

    * OUT: a ``SessionVolume`` full of LOCAL paths becomes a portable
      ``volume`` op naming the archive by its library URL path. The
      processor invents its own workspace; nothing about this server's
      filesystem leaves this server.
    * IN: a ``sidecar`` event's payload is written to ``volume.output``
      BEFORE the ``volume_done`` that announces it is queued, so
      ``_collect_session_volume`` finds the file exactly where the local
      path would have put it.

    Two locks are in play and their order is fixed: this session's own
    ``_lock`` (its claims, and the one-exit claim) is never held while
    ``entry.lock`` is taken, and ``entry.send`` takes ``entry.lock`` itself,
    so the writes of ``entry.sessions`` this class makes -- which the
    entry's contract requires to be under ``entry.lock`` -- stand on their
    own rather than wrapping a send.
    """

    def __init__(
        self,
        entry: ProcessorEntry,
        generation: GenerationSpec,
        *,
        sid: str,
        row_spec: Mapping[str, Any],
        library_path: Path,
        archives_root: str = ARCHIVES_ROOT,
    ) -> None:
        self.entry = entry
        self.generation = generation
        self.sid = sid
        self.row_spec = dict(row_spec)
        self.library_path = Path(library_path)
        # Through the same helper `ProcessorAPI` uses, because this is the
        # one place the string is CONCATENATED with a path: a root
        # configured without its trailing slash would otherwise glue the
        # series name straight onto it.
        self.archives_root = clean_archives_root(archives_root)
        self._events: queue.Queue[dict[str, Any]] = queue.Queue()
        self._volumes: dict[str, SessionVolume] = {}
        self._order: list[str] = []
        self._lock = threading.Lock()
        self._closing = False
        self._killed = False
        self._ending = False
        self._events_open = False
        # When `open_session` went out, and whether an events body has EVER
        # been claimed for it since -- the one sign a processor is really
        # there to run it (`events_overdue`).
        self._started_at: float | None = None
        self._events_seen = False
        self._ended = threading.Event()
        self._fatal: str | None = None
        # Told of every result this session refuses on the wire (the
        # watcher audits it against the processor's account): called with
        # the volume and the reason. None: nobody is listening.
        self.on_rejected: Callable[[SessionVolume, str], None] | None = None

    # -- lifecycle (the OcrSession duck-type) ----------------------------

    def start(self) -> bool:
        """Ask the processor to open a runner for this row.

        Not once it was killed: a settings change or a pre-empt that landed
        before the runner existed means it never opens (its exit is already
        queued by `kill`).
        """
        with self._lock:
            if self._killed or self._ending:
                return False
        with self.entry.lock:
            self.entry.sessions[self.sid] = self
        self._started_at = time.monotonic()
        sent = self.entry.send(
            {"op": "open_session", "sid": self.sid, "generation": self.row_spec}
        )
        if not sent:
            self._events.put(
                {"event": "spawn_failed", "error": f"{self.entry.name} disconnected"}
            )
            self._finish({"event": "exit", "returncode": None})
        return sent

    @property
    def closing(self) -> bool:
        return self._closing

    @property
    def killed(self) -> bool:
        return self._killed

    def is_alive(self) -> bool:
        return not self._ended.is_set() and not self.entry.dropped

    def close(self) -> None:
        with self._lock:
            if self._closing:
                return
            self._closing = True
        self.entry.send({"op": "close_session", "sid": self.sid})

    def kill(self) -> bool:
        """End it now: cancel every claim, close the runner, report the exit.

        The remote equivalent of SIGKILL is `cancel` + `close_session`:
        nothing is recorded for a cancelled claim, which is exactly what
        `preempt_for_bench` and `apply_settings` rely on.
        """
        with self._lock:
            if self._killed:
                return False
            self._killed = True
            claims = list(self._order)
        for claim in claims:
            self.entry.send({"op": "cancel", "sid": self.sid, "claim": claim})
        self.entry.send({"op": "close_session", "sid": self.sid})
        self.end("killed")
        return True

    def wait(self, timeout: float | None = None) -> int | None:
        return 0 if self._ended.wait(timeout=timeout) else None

    def join_reader(self, timeout: float = 5.0) -> None:
        """Nothing to join: the reader is the events request's own thread."""
        del timeout

    def stderr_tail(self) -> str | None:
        return self._fatal

    def poll_event(self, timeout: float | None = None) -> dict[str, Any] | None:
        try:
            return self._events.get(timeout=timeout)
        except queue.Empty:
            return None

    # -- the events body -------------------------------------------------

    def claim_events(self) -> bool:
        """Take the ONE events body this session may have. False if taken.

        The same rule the assignment stream has, for the same reason: two
        bodies feeding one session would interleave a live runner's events
        with a ghost connection's, and `received` would stop meaning
        anything.
        """
        with self._lock:
            if self._events_open:
                return False
            self._events_open = True
            self._events_seen = True
            return True

    def events_overdue(self, seconds: float) -> bool:
        """True when `open_session` went out ``seconds`` ago and no events
        body has been opened for it since, while the session is still live.

        A processor opens a session's body FIRST, before it spawns anything,
        so this is not a slow model load: it is a processor that is not
        there to answer (`OCRWorker._remote_session_lost`).
        """
        with self._lock:
            seen = self._events_seen
        started = self._started_at
        return (
            not seen
            and started is not None
            and self.is_alive()
            and time.monotonic() - started > seconds
        )

    def release_events(self) -> None:
        with self._lock:
            self._events_open = False

    # -- ops out ---------------------------------------------------------

    def submit(self, volume: SessionVolume) -> bool:
        """Send one volume as a portable op, and remember where its file goes.

        False for all three refusals -- no archive, an archive outside the
        library, and a session already holding its two -- each logged, so
        the worker's generic "the runner stopped accepting volumes" always
        has a cause beside it in the log.
        """
        archive = volume.archive
        if archive is None:
            logger.warning(
                "%s cannot go to %s: a remote session needs an archive, "
                "and this job has only an extracted directory",
                volume.id, self.entry.name,
            )
            return False
        try:
            relative = Path(archive).resolve().relative_to(self.library_path.resolve())
        except (OSError, ValueError):
            logger.warning(
                "%s cannot go to %s: %s is not inside the library at %s",
                volume.id, self.entry.name, archive, self.library_path,
            )
            return False
        with self._lock:
            full = len(self._order) >= MAX_OUTSTANDING_VOLUMES
            if not full:
                self._volumes[volume.id] = volume
                self._order.append(volume.id)
        if full:
            logger.warning(
                "%s cannot go to %s yet: session %s already holds its %d volumes",
                volume.id, self.entry.name, self.sid, MAX_OUTSTANDING_VOLUMES,
            )
            return False
        op: dict[str, Any] = {
            "op": "volume",
            "sid": self.sid,
            "claim": volume.id,
            "archive": self.archives_root + "/".join(relative.parts),
            "sidecar_name": volume.output.name,
            "title": volume.title,
            "volume_title": volume.volume,
            "title_uuid": volume.title_uuid,
            "volume_uuid": volume.volume_uuid,
        }
        if volume.archive_size is not None:
            # What the processor holds the download to: a response of any
            # other length is a proxy serving the wrong bytes, or a file that
            # changed after the claim (which the library then tells apart).
            op["size"] = int(volume.archive_size)
        sent = self.entry.send(op)
        if not sent:
            logger.warning(
                "%s cannot go to %s: it disconnected before the op was queued",
                volume.id, self.entry.name,
            )
            self._forget(volume.id)
            return False
        # Only once the volume really is on its way: a note saying a volume
        # was processed remotely, in the log of a volume that never went,
        # is worse than no note.
        self._note_log(volume)
        return True

    def claims(self) -> list[str]:
        with self._lock:
            return list(self._order)

    def _note_log(self, volume: SessionVolume) -> None:
        """One line in this volume's log, so the queue page's link means something."""
        try:
            volume.log.parent.mkdir(parents=True, exist_ok=True)
            with volume.log.open("a", encoding="utf-8") as handle:
                handle.write(
                    f"processed remotely on {self.entry.label()}; "
                    f"the runner's own log is on that host\n"
                )
        except OSError:  # pragma: no cover - a log is never worth a volume
            logger.debug("could not write the remote note into %s", volume.log)

    # -- events in -------------------------------------------------------

    def feed(self, head: Mapping[str, Any], payload: bytes) -> None:
        """One frame off the events body. The ONLY way events enter."""
        kind = head.get("event")
        if not is_event(kind):
            # The name is cut in the format string: it is whatever the far
            # end put in the frame, and `read_frame` allows a head of up to
            # a megabyte -- which must not become a megabyte of log line.
            logger.warning(
                "dropping an event %.40r this protocol does not define, from %s",
                kind, self.entry.name,
            )
            return
        if kind == "ping":
            return
        if kind == "sidecar":
            self._install(str(head.get("id") or ""), str(head.get("name") or ""), payload)
            return
        event = {key: value for key, value in head.items() if key != "payload"}
        if kind == "exit":
            # The far end's own exit goes through the same claim as `end()`,
            # so whichever arrives first is the only one the watcher sees.
            self._finish(event)
            return
        if kind in ("fatal", "spawn_failed"):
            self._fatal = str(head.get("error") or "")[:300] or None
        if kind in ("volume_done", "volume_failed", "volume_returned"):
            # Terminal for the claim: its outstanding slot is free at once,
            # so the lookahead can top up while the watcher judges it.
            self._forget(str(head.get("id") or ""))
        if kind == "volume_returned":
            event["error"] = str(event.get("error") or "")[:300]
            self.entry.note_returned(event)
        elif kind == "fetch" and head.get("state") == "ready":
            self.entry.note_transfer(event)
        self._events.put(event)

    def _install(self, claim: str, name: str, payload: bytes) -> None:
        """Write the finished sidecar where a local session would have left it.

        ``name`` is CHECKED, not trusted, and the check is unconditional:
        the library named the file when it submitted the volume, and a
        processor answering with a different name -- or with none at all --
        is a bug or a hostile client, never a rename to honour. It costs the
        session, because a processor that cannot name the file it has just
        produced cannot be trusted with the next volume either.
        """
        with self._lock:
            volume = self._volumes.get(claim)
        if volume is None:
            # NOT fatal, unlike a wrong name: a claim cancelled a moment ago
            # can still be answered by a processor that was already writing.
            logger.warning("sidecar for an unknown claim %.80r on %s", claim, self.entry.name)
            return
        output = volume.output
        if name != output.name:
            logger.error(
                "%s answered claim %.80r with a sidecar called %.120r, not %r",
                self.entry.name, claim, name, output.name,
            )
            if self.on_rejected is not None:
                try:
                    self.on_rejected(
                        volume, f"its sidecar arrived as {name[:80]!r}, not {output.name!r}"
                    )
                except Exception:  # noqa: BLE001 - an audit never changes the outcome
                    logger.exception("could not report the refused sidecar of %s", claim)
            self.end(f"a sidecar arrived as {name[:80]!r}, not {output.name!r}")
            return
        tmp = output.with_name(output.name + ".tmp")
        try:
            output.parent.mkdir(parents=True, exist_ok=True)
            tmp.write_bytes(payload)
            os.replace(tmp, output)
        except OSError as e:
            # The volume cannot be collected without this file, and staying
            # quiet here would surface much later as a mystery "the sidecar
            # is missing" against a processor that did nothing wrong.
            logger.error("could not write %s: %s", output, e)
            self.end(f"could not write {output.name}: {e}")
        finally:
            # A half-written temp file must not be left beside the real one
            # for the next writer -- or the next listing -- to find.
            with suppress(OSError):
                tmp.unlink(missing_ok=True)

    def end(self, reason: str) -> None:
        """The events body closed. One `exit`, once, whatever the reason."""
        if self._finish({"event": "exit", "returncode": None}):
            logger.debug("remote session %s ended: %s", self.sid, reason)

    def _finish(self, event: dict[str, Any]) -> bool:
        """Queue THE exit and take the session off its entry. Once.

        The claim is a critical section because the callers race: `kill()`
        runs on the worker thread while the sink's `end()` and a far-end
        `exit` run on the events thread. Only the caller that wins the claim
        queues anything, so the watcher can never see two exits for one
        session -- and `_ended` is set AFTER the put, so a reader that finds
        `is_alive()` False finds the exit already waiting for it.
        """
        with self._lock:
            if self._ending:
                return False
            self._ending = True
        self._events.put(event)
        self._ended.set()
        self._forget_session()
        return True

    def _forget_session(self) -> None:
        """Take this session off its entry -- under the entry's own lock.

        `entry.sessions` is read by the admin panel on the request thread
        while this runs on the events thread, and its contract puts every
        WRITER under `entry.lock`. Nothing of the registry is called from
        inside it, which is what keeps the lock order (registry `_lock` ->
        `entry.lock`) intact.
        """
        with self.entry.lock:
            if self.entry.sessions.get(self.sid) is self:
                del self.entry.sessions[self.sid]
                # Ended, not unknown, for a while (`ProcessorEntry.note_ended`).
                self.entry.note_ended(self.sid)

    def _forget(self, claim: str) -> None:
        with self._lock:
            self._volumes.pop(claim, None)
            if claim in self._order:
                self._order.remove(claim)


class RemoteBench:
    """A benchmark running on a processor, shaped like the runner's own.

    ``BenchService`` reads ``bench_ready``/``bench_progress``/``bench_trial``/
    ``bench_done``/``fatal``/``exit`` off a local process's stdout; this hands
    it the same events off the wire, so Addendum 9's timing rules, the trial
    search and the bench object are untouched -- only the machine underneath
    changed. It lives in ``entry.sessions`` under a ``bench-`` prefixed id,
    because the events sink resolves either kind there; `ProcessorEntry`'s
    session count excludes it.

    It answers the events sink's half of `RemoteSession`'s duck-type too
    (``claim_events``/``release_events``/``feed``/``end``/``is_alive``), and
    one rule differs: a benchmark is OVER once its terminal event
    (``bench_done`` or ``fatal``) has arrived, so a body that closes after
    one is not a processor leaving -- ``is_alive`` is already False then,
    and the sink drops nobody for it.
    """

    def __init__(
        self,
        entry: ProcessorEntry,
        *,
        bid: str,
        spec: Mapping[str, Any],
        sample_url: str,
        pages: int,
        precision_only: bool = False,
    ) -> None:
        self.entry = entry
        self.bid = bid
        # Only the precision trials, at the pools as sent (a machine whose
        # pools a person set). A processor older than the flag ignores it
        # and runs a whole benchmark, of which the library keeps only the
        # precision fields anyway.
        self.precision_only = bool(precision_only)
        # The events sink names a body by its session id.
        self.sid = bid
        self.spec = dict(spec)
        self.sample_url = sample_url
        self.pages = int(pages)
        self._events: queue.Queue[dict[str, Any]] = queue.Queue()
        self._lock = threading.Lock()
        self._ended = threading.Event()
        self._ending = False
        self._terminal = False
        self._events_open = False
        self._events_seen = False
        self._started_at: float | None = None
        self._fatal: str | None = None
        self.closing = False
        self.killed = False

    # -- what BenchService drives ---------------------------------------

    def start(self) -> bool:
        with self.entry.lock:
            self.entry.sessions[self.bid] = self
        self._started_at = time.monotonic()
        sent = self.entry.send(
            {
                "op": "bench",
                "bid": self.bid,
                "spec": self.spec,
                "sample": self.sample_url,
                "pages": self.pages,
                **({"precision_only": True} if self.precision_only else {}),
            }
        )
        if not sent:
            self._events.put({"event": "fatal", "error": f"{self.entry.name} disconnected"})
            self._finish()
        return sent

    def poll_event(self, timeout: float | None = None) -> dict[str, Any] | None:
        """The next event -- or, for a processor that never answered, the end.

        A processor opens a benchmark's events body before it touches the
        sample; one that has not within `EVENTS_OPEN_SECONDS` is not there
        (see `RemoteSession.events_overdue`), and waiting out the
        benchmark's whole time budget for it would hold that machine's
        queue for nothing.
        """
        try:
            return self._events.get(timeout=timeout)
        except queue.Empty:
            pass
        with self._lock:
            seen = self._events_seen
        started = self._started_at
        if (
            not seen
            and started is not None
            and not self._ended.is_set()
            and time.monotonic() - started > EVENTS_OPEN_SECONDS
        ):
            self._fatal = f"{self.entry.name} never opened the benchmark's events body"
            self._events.put({"event": "fatal", "error": self._fatal})
            self.end("the processor never answered")
        return None

    def kill(self) -> bool:
        """The ONE cancel op, in its ``{bid}`` shape (Global Constraints)."""
        with self._lock:
            if self.killed:
                return False
            self.killed = True
        self.entry.send({"op": "cancel", "bid": self.bid})
        self.end("cancelled")
        return True

    cancel = kill

    def close(self) -> None:
        self.closing = True

    def wait(self, timeout: float | None = None) -> int | None:
        return 0 if self._ended.wait(timeout=timeout) else None

    def stderr_tail(self) -> str | None:
        return self._fatal

    # -- what the events sink drives ------------------------------------

    def is_alive(self) -> bool:
        return not self._ended.is_set() and not self._terminal and not self.entry.dropped

    def claim_events(self) -> bool:
        with self._lock:
            if self._events_open:
                return False
            self._events_open = True
            self._events_seen = True
            return True

    def release_events(self) -> None:
        with self._lock:
            self._events_open = False

    def feed(self, head: Mapping[str, Any], payload: bytes) -> None:
        del payload
        kind = head.get("event")
        if not is_event(kind) or kind in ("ping", "sidecar"):
            return
        event = {key: value for key, value in head.items() if key != "payload"}
        if kind in ("bench_done", "fatal"):
            if kind == "fatal":
                self._fatal = str(head.get("error") or "")[:300] or None
            self._terminal = True
        if kind == "exit":
            self.end("the processor's benchmark exited")
            return
        self._events.put(event)

    def end(self, reason: str) -> None:
        """The one exit, once, whatever ended it."""
        with self._lock:
            if self._ending:
                return
            self._ending = True
        logger.debug("remote bench %s ended: %s", self.bid, reason)
        self._events.put({"event": "exit", "returncode": None})
        self._finish()

    def _finish(self) -> None:
        self._ended.set()
        with self.entry.lock:
            if self.entry.sessions.get(self.bid) is self:
                del self.entry.sessions[self.bid]
                self.entry.note_ended(self.bid)
