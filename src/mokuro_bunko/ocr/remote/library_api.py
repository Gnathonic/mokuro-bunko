"""``/_processor/*``: the three channels a remote processor opens.

Mounted INSIDE :class:`~mokuro_bunko.middleware.auth.AuthMiddleware`, like
:class:`~mokuro_bunko.admin.api.AdminAPI`, so the actor is already decided
and this layer only has to check that the actor owns what it is reaching
for. Unlike AdminAPI it is mounted UNCONDITIONALLY: a server with its admin
panel switched off still has processors.
"""

from __future__ import annotations

import json
import logging
import queue
import time
from collections.abc import Callable, Iterable, Iterator
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko import __version__
from mokuro_bunko.ocr.remote.protocol import (
    HEARTBEAT_SECONDS,
    PROTOCOL_VERSION,
    ProtocolError,
    clean_archives_root,
    encode_line,
    read_frame,
)
from mokuro_bunko.ocr.remote.registry import (
    BENCH_ID_PREFIX,
    ProcessorEntry,
    ProcessorRegistry,
    clean_processor_name,
)

if TYPE_CHECKING:
    from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles

logger = logging.getLogger(__name__)

PROCESSOR_ROOT = "/_processor"
MAX_REGISTER_BODY_BYTES = 256 * 1024
# What `host` and `catalog` may take together, as JSON. They are written to
# the machine's profile on disk; a real one is well under a kilobyte.
MAX_IDENTITY_BYTES = 16 * 1024

# Every 4xx from the events sink carries one of these as `code`, because
# several of them share a status and even share their shape -- three are
# 409 -- while the client's next move is different for each. The prose is
# for a person reading a log; this is what Task 8 branches on.
EVENTS_REFUSALS: dict[str, str] = {
    "not_owner": "that processor belongs to another account; nothing to retry",
    "unknown": "no such processor or session here; register again",
    "dropped": "this processor was disconnected; register again, its claims are back",
    "body_open": "another body already holds this session; back off and retry",
    "session_ended": "the library ended THIS session (the reason says why); "
    "carry on with the others, no re-registration",
    "bad_frame": "the body could not be read; do not retry it",
}


SAMPLE_CHUNK = 256 * 1024


def bench_sample_filename(bid: str) -> str:
    """The one name a bench sample may have, so a bid cannot name a path."""
    safe = "".join(c for c in bid if c.isalnum() or c in "-_")[:80] or "sample"
    return f"{safe}.cbz"


class ProcessorAPI:
    """WSGI middleware for the remote-processor channels."""

    def __init__(
        self,
        app: Callable[..., Iterable[bytes]],
        registry: ProcessorRegistry,
        *,
        archives_root: str | None = None,
        profiles: ProcessorProfiles | None = None,
        samples_dir: Path | None = None,
        account_check: Callable[[str], Any] | None = None,
    ) -> None:
        self.app = app
        self.registry = registry
        # `processors/<name>.json`: a processor's identity is written there
        # when it registers, so it outlives the connection (spec section 4).
        self.profiles = profiles
        # Where a benchmark's packed sample waits for its processor: under
        # `<storage>/.processing/`, never in the served library tree.
        self.samples_dir = Path(samples_dir) if samples_dir is not None else None
        # `username -> stamp or None`: None when the account is no longer an
        # active processor. Asked at registration, at every heartbeat and at
        # every events body (see `_account_revoked`).
        self.account_check = account_check
        # Normalised through the SAME helper `RemoteSession` uses: this
        # value is advertised in the registration reply, and the op builder
        # concatenates onto its copy -- a trailing slash here and none there
        # is a 404 on every download. The registry's copy is the one the
        # worker's sessions read, so a root given HERE is written back to it:
        # the mount decides, and the ops follow.
        self.archives_root = clean_archives_root(
            registry.archives_root if archives_root is None else archives_root
        )
        registry.archives_root = self.archives_root

    def __call__(
        self, environ: dict[str, Any], start_response: Callable[..., Any]
    ) -> Iterable[bytes]:
        path = environ.get("PATH_INFO", "")
        if path != PROCESSOR_ROOT and not path.startswith(PROCESSOR_ROOT + "/"):
            return self.app(environ, start_response)
        method = environ.get("REQUEST_METHOD", "GET")
        role = environ.get("mokuro.role", "anonymous")
        username = environ.get("mokuro.username")
        if role != "processor" or not isinstance(username, str) or not username:
            # AuthMiddleware already refuses these (and reports a refused
            # LOGIN through its own hook); this is the second lock on the
            # same door, for a stack assembled without it.
            return self._json(start_response, 403, {"error": "Processor access required"})

        rest = path[len(PROCESSOR_ROOT) :]
        if rest == "/register" and method == "POST":
            return self._register(environ, start_response, username)
        parts = [part for part in rest.split("/") if part]
        if len(parts) == 2 and parts[1] == "stream" and method == "GET":
            return self._stream(environ, start_response, username, parts[0])
        if (
            len(parts) == 4
            and parts[1] == "sessions"
            and parts[3] == "events"
            and method == "POST"
        ):
            return self._events(environ, start_response, username, parts[0], parts[2])
        if (
            len(parts) == 4
            and parts[1] == "bench"
            and parts[3] == "sample"
            and method in ("GET", "HEAD")
        ):
            return self._bench_sample(environ, start_response, username, parts[0], parts[2])
        return self._json(start_response, 404, {"error": "No such processor endpoint"})

    # -- register --------------------------------------------------------

    def _register(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
    ) -> Iterable[bytes]:
        try:
            body = self._read_json(environ)
        except ValueError as e:
            return self._json(start_response, 400, {"error": str(e)})
        protocol = body.get("protocol")
        if protocol != PROTOCOL_VERSION:
            reason = f"this server speaks protocol {PROTOCOL_VERSION}, not {protocol!r}"
            # The release beside the protocol: `processor setup` probes with a
            # protocol no library speaks, and names the side to update from it.
            return self._json(
                start_response,
                400,
                {"error": reason, "protocols": [PROTOCOL_VERSION], "version": __version__},
            )
        raw_name = body.get("name")
        if raw_name is None:
            raw_name = username
        elif not isinstance(raw_name, str):
            # `str()` of whatever arrived would name a processor
            # "{'a': 1}" -- and Task 12 files its profile under that name.
            return self._json(start_response, 400, {"error": "name must be text"})
        name = clean_processor_name(raw_name, username)
        if self.registry.is_reserved_name(name):
            return self._json(start_response, 400, {"error": f"{name!r} is a reserved name"})
        # What a queue-page visitor calls this machine instead of its name
        # (a hostname by default). Checked exactly like the name.
        raw_public = body.get("public_name")
        public_name: str | None = None
        if raw_public is not None:
            if not isinstance(raw_public, str):
                return self._json(start_response, 400, {"error": "public_name must be text"})
            public_name = clean_processor_name(raw_public, "") or None
            if public_name is not None and self.registry.is_reserved_name(public_name):
                return self._json(
                    start_response, 400, {"error": f"{public_name!r} is a reserved name"}
                )
        catalog = body.get("catalog")
        if catalog is None:
            catalog = {}
        elif not isinstance(catalog, dict):
            return self._json(start_response, 400, {"error": "catalog must be an object"})
        else:
            for key in ("engines", "detectors", "devices"):
                if key in catalog and not isinstance(catalog[key], list):
                    # Absent or empty is fine -- that is a processor still
                    # installing. A string there is not: every reader of
                    # these treats them as lists.
                    return self._json(
                        start_response, 400, {"error": f"catalog.{key} must be a list"}
                    )
        host = body.get("host")
        try:
            max_sessions = int(body.get("max_sessions") or 1)
        except (TypeError, ValueError):
            # Same 400 an unreadable body gets. `int("lots")` raising out of
            # a WSGI middleware would be a 500 and a traceback for what is
            # just a malformed field.
            return self._json(
                start_response, 400, {"error": "max_sessions is not a whole number"}
            )
        host = host if isinstance(host, dict) else {}
        if len(json.dumps({"host": host, "catalog": catalog})) > MAX_IDENTITY_BYTES:
            return self._json(
                start_response,
                413,
                {"error": f"host and catalog take more than {MAX_IDENTITY_BYTES} bytes"},
            )
        if self.registry.held_by_another(name, username) or (
            self.profiles is not None and not self.profiles.claim(name, username)
        ):
            return self._json(
                start_response,
                409,
                {"error": f"the name {name!r} belongs to another processor account; "
                          "give this machine its own name"},
            )
        entry = self.registry.register(
            username=username,
            name=name,
            host=host,
            catalog=catalog,
            max_sessions=max_sessions,
            public_name=public_name,
        )
        entry.account_stamp = self._account_stamp(username)
        self.on_registered(entry)
        pid = entry.processor_id
        return self._json(
            start_response,
            200,
            {
                "protocol": PROTOCOL_VERSION,
                "processor_id": pid,
                "session_stream": f"{PROCESSOR_ROOT}/{pid}/stream",
                "events": f"{PROCESSOR_ROOT}/{pid}/sessions/{{sid}}/events",
                "archives": self.archives_root,
            },
        )

    def on_registered(self, entry: ProcessorEntry) -> None:
        """A processor just registered: remember what it is.

        The registry forgets a processor the moment it disconnects, but a
        benchmark taken on it has to keep reading "on tower (RTX 4090)"
        afterwards -- so the host and the catalog are written to its
        profile here, where they arrive.
        """
        if self.profiles is not None:
            try:
                self.profiles.set_identity(entry.name, host=entry.host, catalog=entry.catalog)
            except Exception:  # pragma: no cover - a profile is never worth a login
                logger.exception("could not record %s's profile", entry.name)

    # -- the account behind a processor ------------------------------------

    def _account_stamp(self, username: str) -> Any:
        check = self.account_check
        if check is None:
            return None
        try:
            return check(username)
        except Exception:  # pragma: no cover - a database hiccup revokes nobody
            logger.exception("could not look up the account %r", username)
            return None

    def _account_revoked(self, entry: ProcessorEntry) -> str | None:
        """Why this processor's account no longer holds, or None.

        Authentication happens once per REQUEST, and a processor's stream
        and its events bodies are single requests that live for hours. So
        the account is asked again here -- at every heartbeat and at every
        events body -- and a processor whose account was disabled, deleted,
        moved to another role or given a new password is dropped: its claims
        go back unrecorded, and the processor, finding its stream ended,
        registers again, is refused, and exits (spec sections 6 and 7).
        """
        check = self.account_check
        if check is None:
            return None
        try:
            stamp = check(entry.username)
        except Exception:  # pragma: no cover - a database hiccup revokes nobody
            logger.exception("could not look up the account %r", entry.username)
            return None
        if stamp is None:
            return f"the account {entry.username!r} is no longer an active processor"
        if stamp != entry.account_stamp:
            return f"the account {entry.username!r} changed since it registered"
        return None

    # -- the assignment stream --------------------------------------------

    def _stream(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
        processor_id: str,
    ) -> Iterable[bytes]:
        """The ops for one processor, as a long-lived chunked response.

        A generator, so cheroot writes each op as it is produced. The
        heartbeat is the only thing that keeps an idle stream honest in
        both directions: the processor reconnects after two missed ones,
        and this end notices a dead socket when the write fails.
        """
        entry = self._owned(processor_id, username)
        if entry is None:
            if self.registry.get(processor_id) is not None:
                return self._json(start_response, 403, {"error": "Not your processor"})
            return self._json(start_response, 404, {"error": "No such processor"})
        if entry.stream_open:
            # A second stream means the FIRST one is a ghost -- a dropped
            # TCP connection this end has not noticed. The old entry goes,
            # which returns its claims; the processor re-registers and gets
            # a fresh id. It is not resumed in place, because its ops queue
            # belongs to a response nobody is reading.
            self.registry.drop(processor_id, "a second stream was opened")
            return self._json(
                start_response, 409, {"error": "Stream already open; register again"}
            )
        start_response(
            "200 OK",
            [
                ("Content-Type", "application/x-ndjson"),
                ("Cache-Control", "no-store"),
                # nginx buffers a proxied response by default, which would
                # hold every op until the stream ended.
                ("X-Accel-Buffering", "no"),
            ],
        )
        return self._ops(entry)

    def _ops(self, entry: ProcessorEntry) -> Iterator[bytes]:
        try:
            # Both inside the `try`: a flag set outside it would be one the
            # `finally` below is not yet there to clear.
            with entry.lock:
                # A writer of `stream_open` holds `entry.lock`, because the
                # flag travels with `dropped`.
                entry.stream_open = True
            logger.debug("Processor %s stream open", entry.label())
            checked = time.monotonic()
            while True:
                try:
                    op = entry.ops.get(timeout=HEARTBEAT_SECONDS)
                except queue.Empty:
                    op = {"op": "heartbeat"}
                if op is None:
                    return
                # The account, asked again at heartbeat pace whether the
                # stream is idle or busy: a stream that is never idle would
                # otherwise never be checked at all.
                if time.monotonic() - checked >= HEARTBEAT_SECONDS:
                    checked = time.monotonic()
                    revoked = self._account_revoked(entry)
                    if revoked is not None:
                        # Dropped HERE, on the stream's own thread: the drop
                        # returns every claim (the op in hand included), and
                        # the `finally` below sees the entry already gone and
                        # says nothing more.
                        self.registry.drop(entry.processor_id, revoked)
                        return
                # `last_seen` is deliberately NOT stamped here. Writing an
                # op is evidence about THIS end, not the far one: a
                # processor that lost power would go on reading "seen just
                # now" for as long as the kernel buffered these writes, and
                # `to_dict()` publishes the field. Only frames the processor
                # really sent stamp it (the events channel, Task 6).
                yield encode_line(op)
        finally:
            with entry.lock:
                entry.stream_open = False
                gone = entry.dropped
            # Only a stream that ended on ITS OWN -- a closed socket -- is
            # news. When the entry was already dropped, that drop has been
            # announced with its own reason ("re-registered" for a
            # reconnect, which returns the claims rather than blaming the
            # hardware), and this end must not follow it with a second,
            # blunter one. `drop` runs outside `entry.lock`: the registry's
            # own lock is always taken first.
            if not gone:
                self.registry.drop(entry.processor_id, "stream closed")

    # -- the events sink ---------------------------------------------------

    def _events(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
        processor_id: str,
        sid: str,
    ) -> Iterable[bytes]:
        """The runner's events, as a long-lived length-framed request body.

        Length-framed rather than newline-delimited because cheroot's
        ``ChunkedRFile.readline`` never returns once a newline is in its
        buffer (the final ``else:`` branch of its read loop re-slices the
        same buffer forever); ``read(n)`` is the only method of it that
        behaves. The framing also lets a finished sidecar ride the same
        channel as the ``volume_done`` that announces it.

        EVERY frame is checked against the entry and the session, not just
        the first: a processor that was dropped mid-body -- because it
        re-registered, or because a second stream unmasked a ghost -- has
        already had its claims returned to the queue, and its old stream can
        still deliver a queued ``volume`` op before reaching its sentinel.
        Whatever that produces must not be recorded a second time.

        Every refusal carries a ``code`` as well as its prose, because the
        prose is for a person and the client has to ACT: see
        :data:`EVENTS_REFUSALS` for what each one means.
        """
        entry = self._owned(processor_id, username)
        if entry is None:
            if self.registry.get(processor_id) is not None:
                return self._refuse(start_response, 403, "not_owner", "Not your processor")
            return self._refuse(start_response, 404, "unknown", "No such processor")
        revoked = self._account_revoked(entry)
        if revoked is not None:
            self.registry.drop(entry.processor_id, revoked)
            return self._refuse(start_response, 403, "dropped", revoked)
        session = self._session(entry, sid)
        if session is None:
            if entry.was_ended(sid):
                # The library ended it -- a settings change, a pre-empt, a
                # cancel -- and this body is late. The processor is still
                # registered and may be running other sessions: "unknown"
                # would send it off to register again and lose them.
                return self._refuse(
                    start_response, 409, "session_ended",
                    f"session {sid} was ended by the library",
                )
            return self._refuse(start_response, 404, "unknown", "No such session")
        if not session.claim_events():
            # The stream's rule, for the same reason: a second body means
            # the first is a ghost this end has not noticed, and two of them
            # feeding one session would interleave a live runner's events
            # with a dead connection's. The ghost is left to its own socket
            # timeout rather than torn down from here -- the thread blocked
            # on reading it is not this request's to interrupt.
            return self._refuse(
                start_response, 409, "body_open", "Events already open for this session"
            )
        read = environ["wsgi.input"].read
        received = 0
        reason = "the events stream closed"
        try:
            while True:
                frame = read_frame(read)
                if frame is None:
                    break
                head, payload = frame
                # The name is quoted AND cut: it is whatever the far end put
                # in the frame, and `read_frame` allows a head of up to a
                # megabyte.
                refused = str(head.get("event"))[:40]
                if entry.dropped:
                    reason = (
                        f"{entry.name} was disconnected while its events were "
                        f"still arriving; {refused!r} was not recorded"
                    )
                    logger.info("%s", reason)
                    session.end(reason)
                    return self._refuse(
                        start_response, 409, "dropped", reason, received=received
                    )
                if self._session(entry, sid) is not session:
                    # A DIFFERENT outcome with the same shape, and the
                    # client's next move is not the same: the processor is
                    # still connected and still has work; it is this ONE
                    # session that the library ended -- because its sidecar
                    # was refused, or could not be written. Saying "you were
                    # disconnected" here would send it off to re-register
                    # for no reason.
                    reason = (
                        f"session {sid} was ended by the library; "
                        f"{refused!r} was not recorded"
                    )
                    logger.info("%s", reason)
                    session.end(reason)
                    return self._refuse(
                        start_response, 409, "session_ended", reason, received=received
                    )
                # The ONE place besides `register` that may stamp this: a
                # frame is the processor itself speaking. Any frame counts,
                # including a `ping` and including one `feed` goes on to
                # drop -- it arrived, which is the whole claim being made.
                entry.last_seen = time.time()
                session.feed(head, payload)
                received += 1
        except ProtocolError as e:
            # Cut for the same reason as `refused` above: a couple of
            # `read_frame`'s messages quote what the far end sent. Echoing
            # this one back is fair -- it is the client's own bytes.
            reason = f"unreadable event frame: {str(e)[:200]}"
            logger.warning("Processor %s: %s", entry.label(), reason)
            session.end(reason)
            return self._refuse(start_response, 400, "bad_frame", reason)
        except (TimeoutError, OSError) as e:
            # cheroot closes a connection idle for HTTPServer.timeout (10s);
            # the processor's `ping` every 3s is what normally prevents it,
            # so arriving here means the processor really went away.
            reason = f"the events stream from {entry.name} stopped ({e})"
            logger.info("%s", reason)
            self._ended_without_exit(entry, session, reason)
            return self._json(start_response, 200, {"received": received})
        except Exception as e:
            # Absorbing a broken body is this sink's whole job, so the net
            # is deliberately wide: cheroot's `ChunkedRFile` raises a BARE
            # `ValueError` on a malformed chunk-size line or terminator, and
            # anything that escaped here would leave the session sitting in
            # `entry.sessions` with nobody left to feed it -- the watcher
            # would then wait out SESSION_WEDGE_SECONDS on `poll_event` for
            # a runner that is already gone.
            #
            # WITH THE STACK, because this net also catches our own bugs in
            # `feed`/`_install`, and one warning line saying "TypeError"
            # about code on this side of the wire is not a bug report.
            logger.exception("Processor %s: the events body failed", entry.label())
            self._ended_without_exit(
                entry, session,
                f"the events body failed: {type(e).__name__}: {str(e)[:200]}",
            )
            # The CLASS only goes back: unlike the arms above, this message
            # is OURS, and an internal exception's text is not the client's
            # to read. The full one is in the log, with its traceback.
            return self._refuse(
                start_response, 400, "bad_frame",
                f"the events body failed: {type(e).__name__}",
            )
        finally:
            session.release_events()
        self._ended_without_exit(entry, session, reason)
        return self._json(start_response, 200, {"received": received})

    def _ended_without_exit(self, entry: ProcessorEntry, session: Any, reason: str) -> None:
        """A body that ended -- cleanly, silently or torn -- and its session.

        A session's body ends with its runner's `exit`, which ends the
        session as it is fed (`RemoteSession.feed`), and so does a session
        the library ended itself. A session still ALIVE when its body ends
        therefore never heard its runner stop: the PROCESSOR went away --
        its process was stopped (a leaving processor says nothing more on
        any body, `RunnerBridge.shutdown`), it died, or the network did.

        That is a disconnect, not a runner crash (spec sections 3 rule 4
        and 6), and the events body is the first channel to see it: the
        assignment stream only notices on its next write, up to a heartbeat
        later. So the processor is dropped HERE, before the session's exit
        is queued -- `on_drop` returns every claim it held unrecorded, and
        the watcher then settles this session with its processor already
        gone, which blames nothing and strikes no row. Were it the other way
        round, the oldest volume in flight would take a failure record and a
        backoff for a machine being switched off.

        A processor that is really still there finds its stream ended and
        registers again; nothing it held is lost but the work in flight.
        """
        if session.is_alive():
            self.registry.drop(
                entry.processor_id,
                f"the events body of session {session.sid} ended without its "
                f"runner's exit ({reason})",
            )
        session.end(reason)

    # -- a benchmark's sample ---------------------------------------------

    def _bench_sample(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        username: str,
        processor_id: str,
        bid: str,
    ) -> Iterable[bytes]:
        """The packed benchmark sample, for the processor to pull like a volume.

        It lives under ``<storage>/.processing/`` rather than in the library
        tree, so PROPFIND and the catalog never see it -- which means it
        needs a route of its own, and this is it. ``Range`` is supported
        because that is how the processor's fetcher resumes a broken
        download (with no ETag here, a bare Range; verification guards the
        result). Only the processor the benchmark is FOR may read it.
        """
        entry = self._owned(processor_id, username)
        if entry is None:
            return self._json(start_response, 404, {"error": "No such processor"})
        with entry.lock:
            mine = bid.startswith(BENCH_ID_PREFIX) and bid in entry.sessions
        if not mine and bid.startswith(BENCH_ID_PREFIX) and entry.was_ended(bid):
            return self._refuse(
                start_response, 409, "session_ended", f"benchmark {bid} was ended by the library"
            )
        if not mine or self.samples_dir is None:
            return self._json(start_response, 404, {"error": "No such sample"})
        name = bench_sample_filename(bid)
        path = self.samples_dir / name
        if path.name != name or not path.is_file():
            return self._json(start_response, 404, {"error": "No such sample"})
        size = path.stat().st_size
        start, end = 0, max(0, size - 1)
        partial = False
        raw_range = str(environ.get("HTTP_RANGE") or "")
        if raw_range.startswith("bytes=") and size:
            first, _, last = raw_range[len("bytes=") :].partition("-")
            try:
                if first:
                    start = int(first)
                    end = int(last) if last else size - 1
                else:
                    # A suffix range, ``bytes=-N``: the last N bytes.
                    start = max(0, size - int(last))
                    end = size - 1
            except ValueError:
                return self._json(start_response, 400, {"error": "bad Range header"})
            if start >= size or start > end:
                return self._json(start_response, 416, {"error": "range not satisfiable"})
            end = min(end, size - 1)
            partial = True
        length = (end - start + 1) if size else 0
        headers = [
            ("Content-Type", "application/vnd.comicbook+zip"),
            ("Content-Length", str(length)),
            ("Accept-Ranges", "bytes"),
            ("Cache-Control", "no-store"),
        ]
        if partial:
            headers.append(("Content-Range", f"bytes {start}-{end}/{size}"))
        start_response("206 Partial Content" if partial else "200 OK", headers)
        if environ.get("REQUEST_METHOD") == "HEAD" or not length:
            return []
        return self._read_range(path, start, length)

    @staticmethod
    def _read_range(path: Path, start: int, length: int) -> Iterator[bytes]:
        with path.open("rb") as handle:
            handle.seek(start)
            remaining = length
            while remaining > 0:
                chunk = handle.read(min(remaining, SAMPLE_CHUNK))
                if not chunk:
                    return
                remaining -= len(chunk)
                yield chunk

    # -- helpers ---------------------------------------------------------

    def _refuse(
        self,
        start_response: Callable[..., Any],
        status_code: int,
        code: str,
        error: str,
        **extra: Any,
    ) -> list[bytes]:
        """A 4xx from the events sink: prose for a person, `code` to act on."""
        return self._json(
            start_response, status_code, {"error": error, "code": code, **extra}
        )

    @staticmethod
    def _session(entry: ProcessorEntry, sid: str) -> Any:
        """The session this entry knows by that id, or None.

        Read under `entry.lock` because the dict's writers hold it (a
        session registers itself there on `start` and takes itself off when
        it ends), and nothing of the registry is called from inside.
        """
        with entry.lock:
            return entry.sessions.get(sid)

    def _owned(self, processor_id: str, username: str) -> ProcessorEntry | None:
        """This processor, but only for the account that registered it."""
        entry = self.registry.get(processor_id)
        if entry is None or entry.local or entry.username != username:
            return None
        return entry

    @staticmethod
    def _read_json(environ: dict[str, Any]) -> dict[str, Any]:
        raw_length = environ.get("CONTENT_LENGTH")
        if raw_length is None or not str(raw_length).strip():
            # A chunked registration is not supported and must not read as
            # an empty one: say which it is.
            raise ValueError("Content-Length is required")
        try:
            length = int(raw_length)
        except (TypeError, ValueError):
            # Never `str(e)` here: that answers with int()'s own wording
            # ("invalid literal for int() with base 10: ...") and quotes the
            # header's contents back to the caller.
            raise ValueError("invalid Content-Length") from None
        if length <= 0:
            raise ValueError("a registration needs a body")
        if length > MAX_REGISTER_BODY_BYTES:
            raise ValueError("registration body too large")
        raw = environ["wsgi.input"].read(length)
        try:
            value = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as e:
            raise ValueError("registration body is not readable JSON") from e
        if not isinstance(value, dict):
            raise ValueError("registration body is not an object")
        return value

    @staticmethod
    def _json(
        start_response: Callable[..., Any], status_code: int, data: dict[str, Any]
    ) -> list[bytes]:
        names = {
            200: "OK",
            400: "Bad Request",
            403: "Forbidden",
            404: "Not Found",
            409: "Conflict",
            416: "Range Not Satisfiable",
            500: "Internal Server Error",
        }
        body = json.dumps(data).encode("utf-8")
        start_response(
            f"{status_code} {names.get(status_code, 'Error')}",
            [
                ("Content-Type", "application/json"),
                ("Content-Length", str(len(body))),
            ],
        )
        return [body]
