"""Who is connected, what they can run, and what they are holding.

The registry is the library server's whole memory of remote hardware. It is
NOT persistent: a processor is present exactly while its assignment stream
is open, and a restart of either end starts from `register` again. What
survives a disconnect lives in the profile store instead
(:mod:`mokuro_bunko.ocr.remote.profiles`).
"""

from __future__ import annotations

import logging
import queue
import secrets
import threading
import time
from collections import OrderedDict, deque
from collections.abc import Callable, Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.remote.protocol import ARCHIVES_ROOT, clean_archives_root, is_op
from mokuro_bunko.ocr.remote.session import RemoteBench, RemoteSession
from mokuro_bunko.queue.shape import PublicNames

__all__ = [
    "FailedLogin",
    "ProcessorEntry",
    "ProcessorRegistry",
    "RemoteBench",
    "RemoteOCRProcessor",
    "RemoteSession",
]

logger = logging.getLogger(__name__)

# How many refused logins the admin panel can show. The roll is BOUNDED
# because the name in a refusal is attacker-chosen: `attempted_username` is
# the raw pre-`:` segment of a Basic header, so a caller can mint a fresh
# one per attempt and would otherwise grow this list without end.
FAILED_LOGIN_MEMORY = 20
# ...and for the same reason the name itself is truncated before it is kept.
MAX_FAILED_LOGIN_USERNAME = 64
# The id of the library server's own hardware, when it processes at all.
LOCAL_PROCESSOR_ID = "local"
# Benchmarks share `sessions` with sessions (the events sink looks both up
# there), and are told apart by this prefix.
BENCH_ID_PREFIX = "bench-"
# How long, and how many, session ids an entry remembers as ENDED after it
# forgot the session itself (`ProcessorEntry.note_ended`). A processor's events
# body or sample request for one can arrive after the library ended it -- a
# settings change, a benchmark's pre-empt, a cancel -- and must be told
# ``session_ended`` ("carry on with the others"), not ``unknown`` ("register
# again", which tore down its other sessions too).
ENDED_SESSION_TTL_SECONDS = 300.0
ENDED_SESSIONS_KEPT = 32

# A SUCCESSFUL registration is bounded too, and for a sharper reason than a
# refused one: an authenticated processor account can register as often as
# it likes under whatever name it chooses, and only an exactly matching name
# was ever replaced. Without these two rules `_entries` grows for as long as
# the account keeps posting, because `drop()` otherwise runs only when a
# stream CLOSES -- and a registration that never opened a stream never
# closes one either.
MAX_ENTRIES_PER_ACCOUNT = 4
STALE_REGISTRATION_SECONDS = 300.0
# The stored name is a KEY, not just a label: Task 12 files a processor's
# profile under it. `clean_processor_name` below is the one place it is
# derived, so the bound belongs there rather than at each caller.
MAX_PROCESSOR_NAME = 64
# The most concurrent volume sessions one processor may claim. A processor
# that asks for more is clamped, not refused: the number is its own opinion
# about its own hardware, and being wrong about it is not an error.
MAX_SESSIONS_PER_PROCESSOR = 16


# How many delivered archives the admin card's download line averages over.
TRANSFER_MEMORY = 20


def _number(value: Any) -> float | None:
    """A protocol number, or None for anything that is not one."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


# Distinct return classes kept per processor (a handful are real).
MAX_RETURN_CLASSES = 16


class TransferStats:
    """How this processor's archive downloads have gone, for the admin card.

    In memory only, per connection. Every reader and writer holds the
    entry's ``lock`` (`ProcessorEntry.note_transfer` and friends).
    """

    def __init__(self) -> None:
        self.ready: deque[dict[str, float]] = deque(maxlen=TRANSFER_MEMORY)
        self.returned: dict[str, int] = {}
        self.last_returned: dict[str, Any] | None = None
        # The library's download breaker for this processor, copied in by
        # the watcher (design section 6.4).
        self.held_until: float | None = None
        self.held_error: str = ""

    def note_ready(self, head: Mapping[str, Any]) -> None:
        self.ready.append(
            {
                key: _number(head.get(key)) or 0.0
                for key in ("bytes", "seconds", "requests", "restarts", "repairs")
            }
            | {"damaged": 1.0 if head.get("verdict") else 0.0}
        )

    def note_returned(self, head: Mapping[str, Any]) -> None:
        klass = str(head.get("class") or "local")[:40]
        if klass not in self.returned and len(self.returned) >= MAX_RETURN_CLASSES - 1:
            # The processor names the class; one inventing a new name per
            # return must not grow this without end.
            klass = "other"
        self.returned[klass] = self.returned.get(klass, 0) + 1
        self.last_returned = {
            "class": klass,
            "error": str(head.get("error") or "")[:300],
            "at": time.time(),
        }

    def to_dict(self) -> dict[str, Any]:
        total_bytes = sum(row["bytes"] for row in self.ready)
        total_seconds = sum(row["seconds"] for row in self.ready)
        return {
            "volumes": len(self.ready),
            "mb_per_s": (
                round(total_bytes / 1e6 / total_seconds, 1) if total_seconds > 0 else None
            ),
            "resumed": sum(1 for row in self.ready if row["requests"] > 1),
            "restarted": sum(1 for row in self.ready if row["restarts"] > 0),
            "repaired": sum(1 for row in self.ready if row["repairs"] > 0),
            "damaged": sum(1 for row in self.ready if row["damaged"] > 0),
            "returned": sum(self.returned.values()),
            "returned_by_class": dict(self.returned),
            "last_returned": dict(self.last_returned) if self.last_returned else None,
            "held_until": self.held_until,
            "held_error": self.held_error or None,
        }


def clean_processor_name(raw: str, fallback: str) -> str:
    """The name as it will be STORED, from the name as it was sent.

    Truncation happens HERE and nowhere else, because what survives it is
    the key Task 12 files the processor's profile under: a name shortened in
    one place and not in another would file a profile that the next
    registration could not find again.
    """
    name = raw.strip()[:MAX_PROCESSOR_NAME].strip()
    return name or fallback.strip()[:MAX_PROCESSOR_NAME].strip()


@dataclass
class FailedLogin:
    """A login that was refused, for the admin panel to show."""

    username: str
    reason: str
    at: float


@dataclass
class ProcessorEntry:
    """One processor, for as long as its stream is open."""

    processor_id: str
    name: str
    username: str
    host: dict[str, Any]
    catalog: dict[str, Any]
    max_sessions: int
    connected_since: float
    last_seen: float
    ops: queue.Queue[dict[str, Any] | None] = field(default_factory=queue.Queue)
    # Both RemoteSessions (keyed by sid) and RemoteBenches (keyed by
    # `bench-<key>`), because the events sink resolves either by id.
    #
    # EVERY WRITER OF THIS DICT MUST HOLD `lock`. Tasks 5, 6 and 14 add and
    # remove sessions and benches from the events thread and the stream
    # thread while the admin panel is calling `to_dict()` on the request
    # thread; an unguarded write during that read is a "dictionary changed
    # size during iteration" on whichever thread happens to be reading.
    sessions: dict[str, Any] = field(default_factory=dict)
    stream_open: bool = False
    local: bool = False
    dropped: bool = False
    # What a queue-page VISITOR calls this machine, when its processor.yaml
    # says (`processor.public_name`); None gives it a numbered alias.
    public_name: str | None = None
    # What the account looked like when it registered (role, status and a
    # digest of its password hash, `Database.processor_account_stamp`). The
    # library checks it again at every heartbeat and every events body, so
    # an account that is disabled, deleted, demoted or given a new password
    # is cut off within a heartbeat instead of whenever it next reconnects.
    account_stamp: Any = field(default=None, repr=False, compare=False)
    # Guards `sessions`, `stream_open` and `dropped`.
    #
    # `stream_open` belongs here because it travels with `dropped`: `drop`
    # clears the flag and sets `dropped` in one critical section, and the
    # stream handler sets and clears the same flag as its response opens and
    # ends. Its WRITERS hold this lock; its READERS do not and must not.
    # Both places that read it -- `register`'s eviction below and the stream
    # handler's "already open?" gate -- read the bare bool (an atomic read),
    # the first because it already holds the registry's lock and the second
    # because it goes on to call back INTO the registry, which under
    # `entry.lock` would invert the order this class depends on. `sessions`
    # is the exception a dict forces: its readers (`to_dict`,
    # `open_sessions`) take the lock too, since iterating one while another
    # thread resizes it raises.
    #
    # Always taken AFTER the registry's own lock where both are held
    # (`ProcessorRegistry.drop`), never before it.
    lock: threading.Lock = field(
        default_factory=threading.Lock, repr=False, compare=False
    )
    # How its archive downloads have gone (the admin card). Under `lock`.
    transfer: TransferStats = field(default_factory=TransferStats, repr=False, compare=False)
    # Session (and bench) ids this entry ENDED, oldest first, with when:
    # tombstones, so a late request for one is told the session ended rather
    # than that it never existed. Bounded and short-lived
    # (`ENDED_SESSIONS_KEPT`, `ENDED_SESSION_TTL_SECONDS`). Under `lock`.
    ended_sessions: OrderedDict[str, float] = field(
        default_factory=OrderedDict, repr=False, compare=False
    )

    def note_ended(self, sid: str) -> None:
        """Remember that the session ``sid`` ended. The caller holds `lock`."""
        self.ended_sessions.pop(sid, None)
        self.ended_sessions[sid] = time.monotonic()
        while len(self.ended_sessions) > ENDED_SESSIONS_KEPT:
            self.ended_sessions.popitem(last=False)

    def was_ended(self, sid: str) -> bool:
        """Did this entry end ``sid`` within `ENDED_SESSION_TTL_SECONDS`?"""
        with self.lock:
            at = self.ended_sessions.get(sid)
            if at is None:
                return False
            if time.monotonic() - at > ENDED_SESSION_TTL_SECONDS:
                del self.ended_sessions[sid]
                return False
            return True

    def note_transfer(self, head: Mapping[str, Any]) -> None:
        """A `fetch {state: ready}`: one archive delivered to its runner."""
        with self.lock:
            self.transfer.note_ready(head)

    def note_returned(self, head: Mapping[str, Any]) -> None:
        """A `volume_returned`: one claim it could not deliver."""
        with self.lock:
            self.transfer.note_returned(head)

    def note_breaker(self, held_until: float | None, error: str = "") -> None:
        """The library's download breaker for this processor, open or not."""
        with self.lock:
            self.transfer.held_until = held_until
            self.transfer.held_error = error[:300] if held_until else ""

    def label(self) -> str:
        """``tower (RTX 4090)`` -- what the queue page and estimates say."""
        gpu = self.host.get("gpu")
        return f"{self.name} ({gpu})" if gpu else self.name

    @property
    def installing(self) -> bool:
        """A processor whose engines environment is not there yet."""
        if self.local:
            return False
        return not (self.catalog.get("engines") or [])

    @property
    def open_sessions(self) -> int:
        """Volume sessions only. A running benchmark is not a session."""
        with self.lock:
            return self._open_sessions()

    def _open_sessions(self) -> int:
        """`open_sessions`, for a caller that already holds `lock`."""
        return sum(1 for key in tuple(self.sessions) if not key.startswith(BENCH_ID_PREFIX))

    def send(self, op: Mapping[str, Any]) -> bool:
        """Queue one op for this processor's stream.

        False once it is gone -- and false for a name this protocol version
        does not define, which is a bug on this side and must never become
        a silent no-op on a machine in another room.

        The check and the put are ONE critical section with the flag that
        `drop` sets, so "gone" is the truth at the moment of queueing rather
        than a moment before it: otherwise an op could be queued after the
        sentinel that ends the stream and simply never be read.
        """
        if not is_op(op.get("op")):
            logger.error("refusing to send an unknown op %r to %s", op.get("op"), self.name)
            return False
        if self.local:
            return False
        with self.lock:
            if self.dropped:
                return False
            self.ops.put(dict(op))
        return True

    def to_dict(self) -> dict[str, Any]:
        with self.lock:
            sessions = self._open_sessions()
            transfer = self.transfer.to_dict()
        return {
            "processor_id": self.processor_id,
            "name": self.name,
            "label": self.label(),
            "username": self.username,
            "host": dict(self.host),
            "catalog": dict(self.catalog),
            "max_sessions": self.max_sessions,
            "sessions": sessions,
            "connected_since": self.connected_since,
            "last_seen": self.last_seen,
            "installing": self.installing,
            "local": self.local,
            "public_name": self.public_name,
            "transfer": transfer,
        }


class ProcessorRegistry:
    """The connected processors, plus the local hardware when it counts."""

    def __init__(
        self,
        *,
        local_name: str | None = None,
        on_drop: Callable[[ProcessorEntry, str], None] | None = None,
        archives_root: str = ARCHIVES_ROOT,
    ) -> None:
        self._lock = threading.Lock()
        # Where every `volume` op names its archive. Carried HERE, where both
        # of its readers can reach it: `ProcessorAPI` advertises it in the
        # registration reply (and, when it is mounted with a root of its own,
        # writes that back), and each `RemoteOCRProcessor` hands it to the
        # sessions it opens. One string, so the op and the reply can never
        # disagree about where the archives are.
        self.archives_root = clean_archives_root(archives_root)
        self._entries: dict[str, ProcessorEntry] = {}
        self._failures: deque[FailedLogin] = deque(maxlen=FAILED_LOGIN_MEMORY)
        self._last_disconnect: tuple[str, float] | None = None
        self.on_drop = on_drop
        # Visitors' aliases for the processors ("machine 1", ...), numbered
        # in the order they first registered this process (`queue.shape`).
        self.public_names = PublicNames()
        self._local: ProcessorEntry | None = None
        if local_name:
            now = time.time()
            self._local = ProcessorEntry(
                processor_id=LOCAL_PROCESSOR_ID,
                name=local_name,
                username="",
                host={},
                catalog={},
                max_sessions=0,
                connected_since=now,
                last_seen=now,
                stream_open=True,
                local=True,
            )

    # -- membership ------------------------------------------------------

    def held_by_another(self, name: str, username: str) -> bool:
        """True while a processor of ANOTHER account is registered as ``name``."""
        with self._lock:
            return any(
                row.name == name and row.username != username
                for row in self._entries.values()
            )

    def register(
        self,
        *,
        username: str,
        name: str,
        host: Mapping[str, Any],
        catalog: Mapping[str, Any],
        max_sessions: int,
        public_name: str | None = None,
    ) -> ProcessorEntry:
        """A processor just logged in. Any earlier entry of its is dropped.

        Replacing rather than rejecting is what makes a reconnect work: a
        processor whose network dropped re-registers while the library may
        still believe the old stream is alive, and the old entry's claims
        have to come back before the new one is offered anything.

        Three things make room, all of them for THIS account only:

        * the same name re-registering, which is the reconnect above;
        * an entry that never opened a stream and has not been heard from
          for `STALE_REGISTRATION_SECONDS` -- nothing else would ever reap
          one, since `drop` runs on a stream CLOSING;
        * the oldest entry, once the account holds `MAX_ENTRIES_PER_ACCOUNT`
          -- the oldest one WITHOUT a stream first. The newest registration
          always wins, so a processor can never lock itself out by having
          registered too often.

        The whole decision is one critical section -- the scan, the removals
        and the insert together -- so two registrations racing cannot both
        read the same "room left". Only the sentinel and the listeners, which
        must not run under the lock, happen afterwards.
        """
        name = clean_processor_name(name, username)
        now = time.time()
        entry = ProcessorEntry(
            processor_id=secrets.token_hex(8),
            name=name,
            username=username,
            host=dict(host),
            catalog=dict(catalog),
            max_sessions=max(1, min(MAX_SESSIONS_PER_PROCESSOR, int(max_sessions or 1))),
            connected_since=now,
            last_seen=now,
            public_name=public_name or None,
        )
        self.public_names.assign(name, entry.public_name)
        doomed: list[tuple[ProcessorEntry, str]] = []
        with self._lock:
            mine = [row for row in self._entries.values() if row.username == username]
            keep: list[ProcessorEntry] = []
            for row in mine:
                if row.name == name:
                    doomed.append((row, "re-registered"))
                elif not row.stream_open and row.last_seen < now - STALE_REGISTRATION_SECONDS:
                    doomed.append((row, "registered but never opened a stream"))
                else:
                    keep.append(row)
            # A live stream goes LAST. By age alone the victim is usually
            # the processor that has been working longest, so a burst of
            # registrations from one account would cut off the machine
            # actually running volumes -- returning its claims -- to keep
            # registrations that never opened a stream at all.
            keep.sort(key=lambda row: (row.stream_open, row.connected_since))
            while len(keep) >= MAX_ENTRIES_PER_ACCOUNT:
                doomed.append((keep.pop(0), "too many registrations from this account"))
            for row, _reason in doomed:
                self._entries.pop(row.processor_id, None)
                with row.lock:
                    row.dropped = True
                    row.stream_open = False
                self._last_disconnect = (row.name, now)
            self._entries[entry.processor_id] = entry
        for row, reason in doomed:
            self._announce_drop(row, reason)
        logger.info("Processor %s registered (%s)", entry.label(), entry.processor_id)
        return entry

    def is_reserved_name(self, name: str) -> bool:
        """Names that belong to this server rather than to any processor.

        The local hardware's row is not addressable by a remote, and two
        rows reading as the same machine in the Processors card would make
        the one that is really here unidentifiable.
        """
        candidate = name.strip().casefold()
        if candidate == LOCAL_PROCESSOR_ID:
            return True
        return self._local is not None and candidate == self._local.name.strip().casefold()

    def get(self, processor_id: str) -> ProcessorEntry | None:
        if self._local is not None and processor_id == LOCAL_PROCESSOR_ID:
            return self._local
        with self._lock:
            return self._entries.get(processor_id)

    def drop(self, processor_id: str, reason: str) -> ProcessorEntry | None:
        """Forget a processor and release whatever it was holding.

        The sentinel `None` on the ops queue is what ends its streaming
        response; `on_drop` is what returns its claims to the queue
        unrecorded, exactly as a settings change does for a local session.
        Both happen OUTSIDE the registry's lock, so a listener that blocks
        cannot stop another processor registering meanwhile.
        """
        with self._lock:
            entry = self._entries.pop(processor_id, None)
            if entry is None:
                return None
            # Marked gone while the roll is still locked: a `send` racing
            # this drop either queued its op before the flag or returns
            # False, never lands one behind the sentinel.
            with entry.lock:
                entry.dropped = True
                entry.stream_open = False
            self._last_disconnect = (entry.name, time.time())
        self._announce_drop(entry, reason)
        return entry

    def drop_account(self, username: str, reason: str) -> int:
        """Drop every processor this ACCOUNT registered. How many.

        What the admin API calls when it disables, deletes or re-roles a
        user: authentication happens once per request, and a processor's
        stream and events bodies are single requests that live for hours, so
        without this a revoked account would keep receiving volumes -- and
        posting sidecars the library writes -- until it happened to
        reconnect (spec section 7).
        """
        return sum(
            1
            for entry in self._snapshot()
            if entry.username == username
            and self.drop(entry.processor_id, reason) is not None
        )

    def drop_all(self, reason: str) -> int:
        """Drop every connected processor; the local entry stays. How many.

        The library server's way down. A processor's op stream is a worker
        thread that yields a heartbeat to a reader that never stops reading,
        so nothing but the sentinel ever ends it -- and a server with a
        thread still inside a response never exits.
        """
        return sum(
            1
            for entry in self._snapshot()
            if self.drop(entry.processor_id, reason) is not None
        )

    def _announce_drop(self, entry: ProcessorEntry, reason: str) -> None:
        """End the stream and tell the listener. Never under `self._lock`."""
        entry.ops.put(None)
        logger.info("Processor %s disconnected: %s", entry.label(), reason)
        if self.on_drop is not None:
            try:
                self.on_drop(entry, reason)
            except Exception:  # pragma: no cover - a listener never breaks a drop
                logger.exception("processor drop listener failed")

    def entries(self) -> list[ProcessorEntry]:
        """The local entry first, then every connected processor by name."""
        rows = sorted(self._snapshot(), key=lambda e: e.name.lower())
        return ([self._local] if self._local is not None else []) + rows

    def connected(self) -> list[ProcessorEntry]:
        return [entry for entry in self.entries() if entry.stream_open]

    def _snapshot(self) -> list[ProcessorEntry]:
        with self._lock:
            return list(self._entries.values())

    # -- refusals --------------------------------------------------------

    def record_failed_login(self, username: str, reason: str) -> None:
        """A login that never got as far as a registration. Wired to
        `AuthMiddleware(on_processor_login_refused=...)` in `create_app`.

        The log line is this side's, deliberately: `AuthMiddleware` swallows
        whatever a refusal listener raises so a broken one can never turn a
        401 into a 500, which also means a listener that only *stored* the
        refusal would leave a failure here looking exactly like "nobody
        tried". The name is untrusted text -- stored as it arrived, only
        truncated -- and whoever renders it escapes it.
        """
        kept = FailedLogin(username[:MAX_FAILED_LOGIN_USERNAME], reason, time.time())
        logger.warning("Processor login refused for %r: %s", kept.username, reason)
        with self._lock:
            self._failures.appendleft(kept)

    def failures(self) -> list[FailedLogin]:
        """The refusals still remembered, newest first."""
        with self._lock:
            return list(self._failures)

    def last_disconnect(self) -> tuple[str, float] | None:
        with self._lock:
            return self._last_disconnect



class RemoteOCRProcessor(OCRProcessor):
    """An ``OCRProcessor`` whose sessions run on somebody else's hardware.

    Everything about a FINISHED volume stays local and stays inherited:
    ``prepare_session_volume`` builds the workspace and the output path on
    this server, and the worker's own processor validates, normalises and
    moves the file into the library -- byte for byte the same code a local
    slot runs. The only things that differ are where the session lives,
    that it is never the mokuro CLI, which rows it may be offered, and what
    a cancel means.
    """

    def __init__(
        self,
        *args: Any,
        entry: ProcessorEntry,
        registry: ProcessorRegistry,
        library_path: Path,
        row_spec_for: Callable[[ProcessorEntry, Any], dict[str, Any]],
        **kwargs: Any,
    ) -> None:
        super().__init__(*args, **kwargs)
        self.entry = entry
        self.registry = registry
        self.library_path = Path(library_path)
        self._row_spec_for = row_spec_for
        self._session: RemoteSession | None = None

    def open_session(self, generation: Any, session_log: Path) -> Any:
        """A session on the remote processor, shaped like a local one."""
        del session_log  # the runner's log lives on the processor
        session = RemoteSession(
            self.entry,
            generation,
            sid=secrets.token_hex(6),
            row_spec=self._row_spec_for(self.entry, generation),
            library_path=self.library_path,
            archives_root=self.registry.archives_root,
        )
        self._session = session
        return session

    def runs_mokuro_cli(self, generation: Any) -> bool:
        """Never. A remote row is always a session; a processor whose mokuro
        has no serve module simply is not offered a mokuro row
        (`scheduler.catalog_can_run`)."""
        del generation
        return False

    def can_run(self, generation: Any) -> str | None:
        """Checked against the row AS THIS MACHINE WOULD RUN IT.

        The placement comes from the same spec `open_session` sends
        (`row_spec_for`): this processor's profile pools when it has them,
        else the row's own table. Checking the row's own pins instead left a
        valid per-machine override unable to make the row runnable here.
        """
        from mokuro_bunko.ocr.remote.scheduler import catalog_can_run

        pools = self._row_spec_for(self.entry, generation).get("pools") or {}
        return catalog_can_run(
            self.entry.catalog, generation, stage_device=pools.get("stage_device") or {}
        )

    def cancel_active(self) -> bool:
        """Kill the session this slot has open, if it still has one.

        Only a LIVE one: a slot between two sessions still remembers the
        last, and killing that would send the processor a `close_session`
        for a runner it closed long ago.
        """
        session = self._session
        if session is None or not session.is_alive():
            return False
        return session.kill()
