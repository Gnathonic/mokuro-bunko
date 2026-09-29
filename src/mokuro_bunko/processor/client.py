"""Every request a processor makes, on ``http.client`` and nothing else.

The direction of every connection is OUT: the library server is the box
that is already reachable, so there is no port to open here, no token file
on the library side, and NAT and the library's own TLS are somebody else's
problem.
"""

from __future__ import annotations

import base64
import http.client
import io
import json
import logging
import select
import socket
import ssl
import threading
import time
from collections.abc import Callable, Iterator, Mapping
from typing import Any
from urllib.parse import urlsplit

from mokuro_bunko.ocr.remote.protocol import (
    EVENTS_PING_SECONDS,
    HEARTBEAT_SECONDS,
    MISSED_HEARTBEATS,
    PROTOCOL_VERSION,
    decode_line,
    encode_frame,
)
from mokuro_bunko.processor.config import ProcessorConfig

logger = logging.getLogger(__name__)

# The library writes a heartbeat every HEARTBEAT_SECONDS and this is the
# watchdog: a stream silent for MISSED_HEARTBEATS of them is gone, and the
# socket timeout IS how that is noticed -- there is no second timer.
#
# The grace matters. The response's headers only reach us with its FIRST
# chunk (cheroot), which on an idle queue is the first heartbeat, so
# `getresponse()` itself can sit for most of one interval before a single
# byte arrives. A timeout of exactly 2x HEARTBEAT_SECONDS would make that
# slow first byte eat into the budget the two missed beats are supposed to
# measure; this one is strictly longer than 2x, so a slow first byte is
# never mistaken for a dead library.
STREAM_GRACE_SECONDS = 5.0
STREAM_TIMEOUT = HEARTBEAT_SECONDS * MISSED_HEARTBEATS + STREAM_GRACE_SECONDS

# How long to wait for the library's answer to an events body once we have
# stopped writing to it (or been cut off). The body itself is long-lived;
# its REPLY is one small JSON object and arrives at once.
EVENTS_REPLY_TIMEOUT = 10.0
# A refusal is written the moment the app returns, before a single frame is
# read, so a body that is going to be refused says so within this. It is
# paid on every session open and only wasted when the body is FINE, which
# is why it is short: a session open costs a model load either way.
EVENTS_REFUSAL_PROBE = 0.5
EVENTS_OPEN_ATTEMPTS = 3

# `library_api.EVENTS_REFUSALS` is the prose; this is what a client DOES.
# Every 4xx from the events sink carries one of these as `code`, and the
# next move is different for each -- three of them even share a status.
ACTION_REREGISTER = "reregister"
ACTION_RETRY = "retry"
ACTION_MOVE_ON = "move_on"
ACTION_STOP = "stop"

EVENTS_ACTIONS: dict[str, str] = {
    # The library disconnected this processor and returned its claims; the
    # id we hold means nothing any more.
    "dropped": ACTION_REREGISTER,
    "not_owner": ACTION_REREGISTER,
    "unknown": ACTION_REREGISTER,
    # Another body already holds this session -- a ghost of ours the library
    # has not timed out yet. Back off and open it again.
    "body_open": ACTION_RETRY,
    # THIS session was ended by the library. The processor is still
    # connected and still has work: carry on with the others.
    "session_ended": ACTION_MOVE_ON,
    # The library could not read what we sent. Sending it again would fail
    # the same way.
    "bad_frame": ACTION_STOP,
}


class LibraryError(Exception):
    """The library server refused, or could not be reached."""


class LibraryLoginRefused(LibraryError):
    """The account itself was refused. Retrying will not help."""


class ReregisterNeeded(LibraryError):
    """The library wants a fresh registration before it will talk again."""


class LibraryTransportError(LibraryError):
    """The library refused THIS ACCOUNT an archive download: 401, 403, 407.

    Every volume would be refused the same way, so none of them may be
    reported, or even given back one by one. The processor steps away
    instead -- its claims go back unrecorded -- and registers again, which
    either works or is a refused login that ends it. Every other download
    trouble -- a server error, a broken connection, a missing file -- is
    the fetcher's to retry or give back with a class
    (`processor.archives`); none of it is this.
    """


class EventSink:
    """One long-lived chunked request body carrying one session's events.

    The body outlives every frame on it, so the library's ANSWER -- which is
    where a refusal's ``code`` lives -- only arrives when the body ends: at
    :meth:`close`, or the moment a refusal cuts it short. Either way it is
    read exactly once and turned into :attr:`action`, which is what the
    caller branches on.

    An end the library imposes can be discovered by ANY thread that touches
    the body -- an event's sender, or the ping thread while the runner sits
    idle. Whichever one finds it runs the ``on_end`` callback (see
    :meth:`on_end`), exactly once, so what the caller does about it never
    depends on which thread happened to be first.
    """

    def __init__(
        self,
        connection: http.client.HTTPConnection,
        path: str,
        *,
        ping: bool = True,
        reply_head: bytes = b"",
    ) -> None:
        self._connection = connection
        self._path = path
        self._lock = threading.Lock()
        self._closed = False
        self._collected = False
        self._stop = threading.Event()
        # Bytes of the library's answer already taken off the socket by a
        # probe; they are handed on to the response that parses the rest.
        self._reply_head = reply_head
        # Set once the far end has been found to have ENDED the body --
        # as opposed to this side closing it -- and the callback for it.
        self._ended_by_library = False
        self._on_end: Callable[[EventSink], None] | None = None
        self._on_end_fired = False
        self.status: int | None = None
        self.code = ""
        self.detail = ""
        # The latest progress each download has OFFERED, by key: a slot, not
        # a queue. The ping thread sends whatever is here in place of a ping
        # (each is a real frame, so it keeps the body alive too), and the
        # read loop that offers it never touches the send lock -- a sidecar
        # upload holding that lock can delay a progress frame, never a
        # download (design section 4.2).
        self._offers: dict[str, dict[str, Any]] = {}
        self._offer_lock = threading.Lock()
        self._pinger: threading.Thread | None = None
        if ping:
            self._pinger = threading.Thread(
                target=self._ping, name="events-ping", daemon=True
            )
            self._pinger.start()

    @property
    def ended(self) -> bool:
        """True once the library has ended this body (refused or cut it)."""
        return self._ended_by_library

    def on_end(self, callback: Callable[[EventSink], None]) -> None:
        """Run ``callback(self)`` once, when the library is found to have
        ended the body -- at once, if it already has.

        Never for a body THIS side closes: that end is the caller's own and
        needs no reaction.
        """
        with self._lock:
            self._on_end = callback
        self._fire_on_end()

    def _fire_on_end(self) -> None:
        with self._lock:
            if not self._ended_by_library or self._on_end is None or self._on_end_fired:
                return
            self._on_end_fired = True
            callback = self._on_end
        try:
            callback(self)
        except Exception:  # pragma: no cover - a reaction must not kill a thread
            logger.exception("reacting to the end of the events body for %s", self._path)

    def send(self, head: Mapping[str, Any], payload: bytes = b"") -> bool:
        """One event. False once the body is closed or the socket is gone."""
        frame = encode_frame(head, payload)
        with self._lock:
            if self._closed:
                return False
            # Any answer at all while the body is still open is the library
            # ending it: it only replies once it has stopped reading. Looking
            # BEFORE writing reads that answer intact, where a write into a
            # connection the far end has closed would draw a reset that can
            # throw the answer -- and its `code` -- away.
            early = _early_reply(self._connection, 0.0)
            if early is not None:
                logger.info("the library ended the events body for %s", self._path)
                self._reply_head = early
                failed = True
            else:
                try:
                    self._connection.send(b"%x\r\n" % len(frame) + frame + b"\r\n")
                except OSError as e:
                    logger.info("events stream for %s ended: %s", self._path, e)
                    failed = True
                else:
                    failed = False
            if failed:
                self._closed = True
                self._ended_by_library = True
        if failed:
            # OUTSIDE the lock: reading the reply can block for as long as
            # EVENTS_REPLY_TIMEOUT, and a ping thread parked on the lock for
            # that whole time would be a second thread wedged on a body that
            # is already over.
            self._collect()
            self._fire_on_end()
            return False
        return True

    def offer(self, key: str, head: Mapping[str, Any]) -> None:
        """Leave ``head`` to go out with the next ping tick. Never blocks.

        Replaces whatever ``key`` offered before: only the LATEST progress
        of a download is worth a frame. Takes only a tiny lock of its own.
        """
        with self._offer_lock:
            self._offers[key] = dict(head)

    def offered(self, key: str) -> dict[str, Any] | None:
        """What ``key`` has offered and the ping thread has not sent yet."""
        with self._offer_lock:
            head = self._offers.get(key)
            return dict(head) if head is not None else None

    def withdraw(self, key: str) -> None:
        """Drop a stale offer: its download is over, and says so itself."""
        with self._offer_lock:
            self._offers.pop(key, None)

    def _take_offers(self) -> list[dict[str, Any]]:
        with self._offer_lock:
            heads = list(self._offers.values())
            self._offers.clear()
        return heads

    def _ping(self) -> None:
        """Keep the body from idling past cheroot's socket timeout (10s).

        Also the one thread that touches the body while the runner is idle,
        so an end the library imposes then is found HERE -- and `send`
        hands it to the ``on_end`` callback like any other. A tick with a
        download's progress offered sends THAT instead of a ping.
        """
        while not self._stop.wait(EVENTS_PING_SECONDS):
            heads = self._take_offers() or [{"event": "ping"}]
            for head in heads:
                if not self.send(head):
                    return

    @property
    def action(self) -> str:
        """What to do about how this body ended: one of the ACTION_* verbs.

        Empty while the body is healthy, and empty for a body that simply
        ended -- only a refusal asks for anything. A 401 or 403 with no
        ``code`` is the AUTH layer refusing this account before the events
        sink ever saw the body (a revoked account, a changed password): only
        a fresh registration can say so for certain, and a refused one is
        what ends this processor (spec section 6).
        """
        if not self.code and self.status in (401, 403):
            return ACTION_REREGISTER
        return EVENTS_ACTIONS.get(self.code, "")

    def close(self) -> None:
        self._stop.set()
        with self._lock:
            already = self._closed
            self._closed = True
            if not already:
                try:
                    self._connection.send(b"0\r\n\r\n")
                except OSError:
                    pass
        self._collect()

    def _refused_at_open(self) -> None:
        """The library answered before a single frame: ended, not closed."""
        with self._lock:
            self._closed = True
            self._ended_by_library = True
        self._stop.set()
        self._collect()

    def _collect(self) -> None:
        """Read the library's one answer to this body, once, and close up."""
        with self._lock:
            if self._collected:
                return
            self._collected = True
        try:
            sock = self._connection.sock
            if sock is None:
                raise OSError("the connection is already closed")
            sock.settimeout(EVENTS_REPLY_TIMEOUT)
            # Parsed straight off the socket rather than through
            # `getresponse()`, because a probe may already have taken the
            # answer's first bytes: they are replayed ahead of the rest.
            response = http.client.HTTPResponse(
                _Replayed(sock, self._reply_head), method="POST"  # type: ignore[arg-type]
            )
            response.begin()
            raw = response.read()
            self.status = response.status
            if response.status >= 400:
                self.code, self.detail = _refusal_code(raw)
                logger.info(
                    "the library refused the events body for %s: %s (%s)",
                    self._path, self.detail or response.reason, self.code or "no code",
                )
        except (OSError, http.client.HTTPException) as e:
            logger.debug("no reply to the events body for %s: %s", self._path, e)
        finally:
            try:
                self._connection.close()
            except OSError:  # pragma: no cover - defensive
                pass


class _Replayed:
    """A socket whose reader first gives back bytes a probe already took.

    ``HTTPResponse`` only ever asks its socket for ``makefile("rb")``.
    """

    def __init__(self, sock: socket.socket, head: bytes) -> None:
        self._sock = sock
        self._head = head

    def makefile(self, mode: str, *args: Any, **kwargs: Any) -> io.BufferedReader:
        del mode, args, kwargs
        return io.BufferedReader(_ReplayedRaw(self._sock, self._head))


class _ReplayedRaw(io.RawIOBase):
    def __init__(self, sock: socket.socket, head: bytes) -> None:
        super().__init__()
        self._sock = sock
        self._head = head

    def readable(self) -> bool:
        return True

    def readinto(self, buffer: Any) -> int:
        if self._head:
            count = min(len(buffer), len(self._head))
            buffer[:count] = self._head[:count]
            self._head = self._head[count:]
            return count
        return self._sock.recv_into(buffer)


def _refusal_code(raw: bytes) -> tuple[str, str]:
    """``(code, prose)`` out of a refusal body; empty when it carried none."""
    try:
        body = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        return "", raw.decode("utf-8", "replace")[:200]
    if not isinstance(body, dict):
        return "", raw.decode("utf-8", "replace")[:200]
    return str(body.get("code") or ""), str(body.get("error") or "")[:200]


class LibraryClient:
    """Register, take ops, post events; the connections an archive download uses."""

    def __init__(self, config: ProcessorConfig) -> None:
        self.config = config
        parts = urlsplit(config.library.url)
        self.host = parts.hostname or "127.0.0.1"
        self.port = parts.port or (443 if parts.scheme == "https" else 80)
        self.secure = parts.scheme == "https"
        self.root = parts.path.rstrip("/")
        token = f"{config.library.username}:{config.library.password}".encode()
        self._auth = "Basic " + base64.b64encode(token).decode("ascii")
        self.channels: dict[str, str] = {}
        self.processor_id: str = ""
        self._stream: http.client.HTTPConnection | None = None

    # -- connections -----------------------------------------------------

    def _context(self) -> ssl.SSLContext | None:
        if not self.secure:
            return None
        verify = self.config.library.tls_verify
        if verify is False:
            context = ssl.create_default_context()
            context.check_hostname = False
            context.verify_mode = ssl.CERT_NONE
            return context
        if isinstance(verify, str):
            return ssl.create_default_context(cafile=verify)
        return ssl.create_default_context()

    def connect(self, timeout: float) -> http.client.HTTPConnection:
        """A new connection to the library, over its TLS when it has one.

        Not yet connected: ``http.client`` connects on the first request (or
        an explicit ``connect()``), with ``timeout`` for both.
        """
        if self.secure:
            return http.client.HTTPSConnection(
                self.host, self.port, timeout=timeout, context=self._context()
            )
        return http.client.HTTPConnection(self.host, self.port, timeout=timeout)

    def request_headers(self, **extra: str) -> dict[str, str]:
        """This processor's credentials, plus ``extra``."""
        headers = {"Authorization": self._auth, "Accept": "application/json"}
        headers.update(extra)
        return headers

    # The names the rest of this module has always used.
    _connect = connect
    _headers = request_headers

    # -- the handshake ---------------------------------------------------

    def register(
        self, catalog: Mapping[str, Any], host: Mapping[str, Any]
    ) -> dict[str, Any]:
        """Log in and learn the channel paths. Raises `LibraryError` on refusal."""
        payload: dict[str, Any] = {
            "protocol": PROTOCOL_VERSION,
            "name": self.config.processor.name,
            "host": dict(host),
            "catalog": dict(catalog),
            "max_sessions": self.config.processor.max_sessions,
        }
        if self.config.processor.public_name:
            payload["public_name"] = self.config.processor.public_name
        body = json.dumps(payload).encode()
        connection = self._connect(timeout=30.0)
        try:
            connection.request(
                "POST",
                f"{self.root}/_processor/register",
                body=body,
                headers=self._headers(**{"Content-Type": "application/json"}),
            )
            response = connection.getresponse()
            raw = response.read()
            status = response.status
            if status != 200:
                raise self._refusal(status, raw)
            reply: dict[str, Any] = json.loads(raw.decode("utf-8"))
            if not isinstance(reply, dict):
                raise LibraryError("the library's registration reply is not an object")
        except (OSError, ValueError) as e:
            raise LibraryError(f"could not reach {self.config.library.url}: {e}") from e
        finally:
            connection.close()
        self.processor_id = str(reply.get("processor_id") or "")
        self.channels = {
            "stream": str(reply.get("session_stream") or ""),
            "events": str(reply.get("events") or ""),
            "archives": str(reply.get("archives") or "/mokuro-reader/"),
        }
        if not self.processor_id or not self.channels["stream"]:
            raise LibraryError("the library's registration reply named no channels")
        return reply

    @staticmethod
    def _refusal(status: int, raw: bytes) -> LibraryError:
        try:
            body = json.loads(raw.decode("utf-8"))
            detail = body.get("error") or raw.decode("utf-8", "replace")
            versions = body.get("protocols")
        except ValueError:
            detail, versions = raw.decode("utf-8", "replace")[:200], None
        if status in (401, 403):
            return LibraryLoginRefused(
                f"the library refused this account ({status}): {detail}"
            )
        if versions:
            return LibraryError(f"{detail} (this library speaks protocol {versions})")
        return LibraryError(f"the library answered {status}: {detail}")

    # -- the assignment stream -------------------------------------------

    def ops(self) -> Iterator[dict[str, Any]]:
        """Every op the library sends, until the stream ends.

        A 409 is the library saying a stream is already open for this id --
        the ghost of a connection it has not noticed die. It drops the entry
        when it says so, so the only way back is a fresh registration:
        :class:`ReregisterNeeded`, not a retry of this request.

        Silence is the other ending. The socket's own timeout is the
        heartbeat watchdog (see :data:`STREAM_TIMEOUT`); it firing means the
        library stopped beating, and this generator simply ends so the caller
        reconnects.
        """
        connection = self._connect(timeout=STREAM_TIMEOUT)
        self._stream = connection
        try:
            connection.request(
                "GET", self.root + self.channels["stream"], headers=self._headers()
            )
            response = connection.getresponse()
            if response.status != 200:
                raw = response.read()
                if response.status == 409:
                    raise ReregisterNeeded(
                        f"the library wants a fresh registration: {_refusal_code(raw)[1]}"
                    )
                raise self._refusal(response.status, raw)
            while True:
                line = response.readline()
                if not line:
                    return
                op = decode_line(line)
                if op is None:
                    continue
                yield op
        except TimeoutError:
            logger.info(
                "the assignment stream went silent for %.0fs (%d missed heartbeats)",
                STREAM_TIMEOUT, MISSED_HEARTBEATS,
            )
        except (OSError, http.client.HTTPException, AttributeError) as e:
            # AttributeError is the shape a connection closed from ANOTHER
            # thread takes: `http.client` drops the response's `fp`, and a
            # `readline` already in flight trips over it. `close()` shuts
            # the socket down rather than closing it for exactly that
            # reason; this is the net under the cases it cannot reach.
            logger.info("assignment stream ended: %s", e)
        finally:
            self._stream = None
            connection.close()

    # -- the events channel ----------------------------------------------

    def open_events(self, sid: str) -> EventSink:
        """Open one session's events body and leave it open.

        ``body_open`` is the one refusal worth waiting out: it means a ghost
        of OUR own body still holds this session, and the library lets that
        one go on its socket timeout. Every other refusal is answered by the
        caller, from :attr:`EventSink.action`.
        """
        delay = 1.0
        sink = self._open_events(sid)
        for _ in range(EVENTS_OPEN_ATTEMPTS - 1):
            if sink.action != ACTION_RETRY:
                return sink
            logger.info("the events body for %s is still held; retrying in %.0fs", sid, delay)
            time.sleep(delay)
            delay *= 2
            sink = self._open_events(sid)
        return sink

    def _open_events(self, sid: str) -> EventSink:
        path = self.root + self.channels["events"].replace("{sid}", sid)
        connection = self._connect(timeout=60.0)
        connection.putrequest("POST", path, skip_accept_encoding=True)
        connection.putheader("Authorization", self._auth)
        connection.putheader("Transfer-Encoding", "chunked")
        connection.putheader("Content-Type", "application/x-mokuro-events")
        connection.endheaders()
        # A refusal is decided before a single frame is read, so it is
        # already on its way back. Catching it HERE -- rather than at the
        # first send that fails -- is what lets `open_events` retry a
        # `body_open` before any event has been produced for it.
        early = _early_reply(connection, EVENTS_REFUSAL_PROBE)
        if early is not None:
            sink = EventSink(connection, path, ping=False, reply_head=early)
            sink._refused_at_open()
            return sink
        return EventSink(connection, path)

    def close(self) -> None:
        """End the assignment stream, from whichever thread noticed.

        It is SHUT DOWN rather than closed: closing pulls ``http.client``'s
        buffer out from under a ``readline`` already in flight on the ops
        thread, while a shutdown makes that read return a clean end of
        stream -- which is what :meth:`ops` is written to end on. This is
        also the path a re-registration is asked for down (see
        ``RunnerBridge._report``), so it has to be safe to call at any
        moment from any thread.
        """
        stream = self._stream
        if stream is None:
            return
        sock = stream.sock
        try:
            if sock is not None:
                sock.shutdown(socket.SHUT_RDWR)
            else:
                stream.close()
        except OSError:
            pass


def _early_reply(connection: http.client.HTTPConnection, timeout: float) -> bytes | None:
    """The first byte of an answer already sent back, or None if none has been.

    ``b""`` when the far end has closed the connection without a word.

    Readable is NOT the same as answered. Over TLS 1.3 the server sends its
    session tickets right after the handshake, so the raw socket under a
    perfectly healthy body is readable before the library has said
    anything. So a readable socket is READ -- through the TLS layer,
    without blocking -- and only application data counts: TLS-layer records
    are consumed by that read and yield nothing. The byte a real answer
    gives up is the caller's to hand on to whatever parses the rest.
    """
    sock = connection.sock
    if sock is None:  # pragma: no cover - defensive
        return None
    deadline = time.monotonic() + timeout
    while True:
        try:
            ready, _, _ = select.select([sock], [], [], max(0.0, deadline - time.monotonic()))
        except (OSError, ValueError):  # pragma: no cover - a torn-down socket
            return b""
        if not ready:
            return None
        previous = sock.gettimeout()
        try:
            sock.setblocking(False)
            return sock.recv(1)
        except (BlockingIOError, ssl.SSLWantReadError, ssl.SSLWantWriteError):
            # TLS-layer bytes only: nothing has been said yet.
            pass
        except OSError:
            return b""
        finally:
            sock.settimeout(previous)
        if time.monotonic() >= deadline:
            return None
