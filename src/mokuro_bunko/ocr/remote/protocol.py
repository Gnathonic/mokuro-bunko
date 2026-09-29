"""The wire between a library server and a processor.

TWO codecs, because the two directions have different floors:

* DOWN (library -> processor) is a chunked HTTP RESPONSE, and the processor
  reads it with ``http.client``'s own ``readline``, which is correct. So ops
  are NDJSON: one JSON object a line.
* UP (processor -> library) is a chunked HTTP REQUEST BODY, which cheroot
  hands the app as ``ChunkedRFile``. Its ``readline`` never returns once a
  newline sits in the buffer: the final ``else`` branch
  (``cheroot/server.py:599-601``) appends ``buffer[:newline_pos]`` and then
  keeps ``buffer[newline_pos:]``, so the newline stays at index 0, the next
  pass finds ``newline_pos == 0``, and the loop spins forever. ``read(n)``
  is the one method of it that behaves, so events are LENGTH-FRAMED -- and
  the framing is what lets a finished sidecar ride the same channel as the
  event that announces it.
"""

from __future__ import annotations

import json
from collections.abc import Callable, Mapping
from typing import Any

# 2: the processor downloads each archive whole and verified before its
# runner reads it, reports that as `fetch`, and gives back a claim it cannot
# deliver with `volume_returned` instead of failing the volume. Strict: a
# processor of any other version is refused at registration, because a v1
# processor is exactly the road that filed false "archive incomplete"
# records, and a v2 processor against a library that drops
# `volume_returned` would leave the claim hanging, then blamed.
PROTOCOL_VERSION = 2

# Where the library serves its archives. A processor is told this in its
# registration reply and every `volume` op names its archive underneath it,
# so the two MUST be the same string: it is defined once, here, and both the
# HTTP layer (`ProcessorAPI`) and the op builder (`RemoteSession.submit`)
# take their default from it rather than each spelling it out.
ARCHIVES_ROOT = "/mokuro-reader/"


def clean_archives_root(raw: str) -> str:
    """``/manga``, ``manga/``, ``//manga//`` -> ``/manga/``.

    Both ends of this string matter and neither is the caller's to get
    right: the leading slash makes the advertised root an absolute URL path,
    and the trailing one is what ``RemoteSession.submit`` concatenates a
    series name onto. Normalising in only one of the two places would put
    the advertised root and the op's archive path back out of step, which is
    the whole reason the constant exists -- so it happens HERE, and both
    call it.
    """
    stripped = raw.strip("/")
    return f"/{stripped}/" if stripped else "/"


# Library -> processor. Every outgoing op is checked against this set before
# it is queued, so a typo is a log line here rather than a silent no-op on
# a machine in another room.
OPS: frozenset[str] = frozenset(
    {"open_session", "volume", "cancel", "close_session", "bench", "heartbeat"}
)

# Processor -> library. The runner's own events pass through verbatim; the
# ones this protocol adds are `sidecar` (the finished file's bytes, sent
# immediately BEFORE the `volume_done` that announces it), `ping` (the
# keep-alive that stops cheroot's 10s socket timeout ending an idle events
# body), the session-level `exit`/`spawn_failed` the processor's own
# OcrSession produces, and two about the ARCHIVE (protocol 2):
#
# * `fetch` -- a claim's archive on its way: `downloading`/`retrying`/
#   `restarting` progress (liveness only), and exactly one `ready` once the
#   verified archive was handed to the runner's pipe, which is what makes
#   the claim DELIVERED (only a delivered claim can be blamed for a runner's
#   death);
# * `volume_returned` -- a claim that never reached the runner, given back
#   with a class (`stalled`, `differs`, `changed`, `no_range`, `mismatch`,
#   `missing`, `rejected`, `no_room`, `local`). Terminal for the claim, and
#   never a failure of the volume: the library judges it from its own file.
EVENTS: frozenset[str] = frozenset(
    {
        "ready",
        "volume_started",
        "page",
        "volume_done",
        "volume_failed",
        "stats",
        "fatal",
        "bench_ready",
        "bench_progress",
        "bench_trial",
        "bench_done",
        "sidecar",
        "ping",
        "exit",
        "spawn_failed",
        "fetch",
        "volume_returned",
    }
)

# The library writes a `heartbeat` op this often; the processor reconnects
# after this many silent intervals.
HEARTBEAT_SECONDS = 15.0
MISSED_HEARTBEATS = 2

# The processor writes a `ping` frame this often on an otherwise idle events
# body. It MUST stay below cheroot's `HTTPServer.timeout` (10s,
# `cheroot/server.py:1561`), which applies to reads on the connection socket
# and would otherwise raise TimeoutError inside the WSGI app.
EVENTS_PING_SECONDS = 3.0

# How long the library waits, after sending `open_session`, for that
# session's events body to be opened. A processor opens it FIRST, before it
# spawns anything, so a body that has not arrived by now never will: the
# processor is gone (suspended, unplugged, off the network) while its idle
# stream still looks open -- the kernel only notices a dead peer when a
# heartbeat write finally fails, which can take many minutes. The library
# then treats it as the disconnect it is (claims back, nothing recorded),
# never as a runner that crashed on a volume.
EVENTS_OPEN_SECONDS = 2 * HEARTBEAT_SECONDS

# A processor whose open events body has carried no frame at all for this
# long is gone rather than busy: the body pings every EVENTS_PING_SECONDS
# whatever its runner is doing. Used to tell a wedged RUNNER (the processor
# still pinging: the runner is blamed, as a local one would be) from a
# vanished PROCESSOR (dropped: nothing is blamed).
EVENTS_SILENCE_SECONDS = 2 * HEARTBEAT_SECONDS

# A frame's JSON head is 8 hex digits of length; the head itself is small,
# and a payload is a sidecar (a few MB at most for a 200-page volume).
FRAME_HEAD_BYTES = 8
MAX_HEAD_BYTES = 1 << 20
MAX_PAYLOAD_BYTES = 128 << 20


class ProtocolError(Exception):
    """A frame that cannot be trusted enough to allocate for."""


def is_op(name: object) -> bool:
    """True for an op name this protocol version defines."""
    return isinstance(name, str) and name in OPS


def is_event(name: object) -> bool:
    """True for an event name this protocol version defines."""
    return isinstance(name, str) and name in EVENTS


def encode_line(obj: Mapping[str, Any]) -> bytes:
    """One op, as one line, with no embedded newline."""
    return (json.dumps(dict(obj), ensure_ascii=True) + "\n").encode("utf-8")


def decode_line(raw: bytes | str) -> dict[str, Any] | None:
    """One op back, or None for a blank or unreadable line.

    Never raises: a garbage line on a long-lived stream must cost that line
    and nothing else.
    """
    text = raw.decode("utf-8", errors="replace") if isinstance(raw, bytes) else raw
    text = text.strip()
    if not text:
        return None
    try:
        value = json.loads(text)
    except ValueError:
        return None
    return value if isinstance(value, dict) else None


def encode_frame(obj: Mapping[str, Any], payload: bytes = b"") -> bytes:
    """One event, as one frame: 8 hex digits, the JSON head, then the payload.

    ``payload`` is a reserved head key: whatever the caller's ``obj`` carries
    under it is dropped before the real tail's length (if any) is written in
    its place. Otherwise a caller-supplied ``{"payload": 99}`` with no tail
    would declare 99 bytes that never arrive and desync every frame after it.
    """
    head = dict(obj)
    head.pop("payload", None)
    if payload:
        head["payload"] = len(payload)
    body = json.dumps(head, ensure_ascii=True).encode("utf-8")
    return b"%08x" % len(body) + body + payload


def _read_maybe_short(read: Callable[[int], bytes], size: int) -> bytes:
    """As many as ``size`` bytes, stopping early at a clean EOF.

    Unlike :func:`read_exactly`, a short read is returned as-is instead of
    being collapsed to ``b""`` -- :func:`read_frame` needs to tell "nothing
    of the next frame has arrived yet" (a clean end of stream) from "some
    bytes arrived, then the stream ended" (a torn frame), and only the
    length of what actually came back can make that distinction.
    """
    if size <= 0:
        return b""
    chunks: list[bytes] = []
    have = 0
    while have < size:
        chunk = read(size - have)
        if not chunk:
            break
        chunks.append(chunk)
        have += len(chunk)
    return b"".join(chunks)


def read_exactly(read: Callable[[int], bytes], size: int) -> bytes:
    """Exactly ``size`` bytes, or ``b""`` when the stream ended first.

    ``ChunkedRFile.read(n)`` already blocks until it has ``n`` bytes, but a
    socket file or a BytesIO may short-read, so the loop is here rather than
    assumed.
    """
    data = _read_maybe_short(read, size)
    return data if len(data) == size else b""


def read_frame(
    read: Callable[[int], bytes],
) -> tuple[dict[str, Any], bytes] | None:
    """The next frame, or None at a clean end of stream.

    A short read is a clean close only when it happens before any byte of
    the next frame has arrived. Once the header -- or a complete header's
    head, or a complete head's payload -- has started arriving, a further
    short read means the connection was cut mid-frame: that raises
    ProtocolError rather than returning None, so a torn frame can never be
    mistaken for "no more frames".
    """
    header = _read_maybe_short(read, FRAME_HEAD_BYTES)
    if not header:
        return None
    if len(header) < FRAME_HEAD_BYTES:
        raise ProtocolError("stream ended mid-frame")
    try:
        head_len = int(header.decode("ascii"), 16)
    except (UnicodeDecodeError, ValueError):
        raise ProtocolError(f"bad frame header {header!r}") from None
    if head_len <= 0 or head_len > MAX_HEAD_BYTES:
        raise ProtocolError(f"frame head of {head_len} bytes is out of range")
    body = _read_maybe_short(read, head_len)
    if len(body) < head_len:
        raise ProtocolError("stream ended mid-frame")
    try:
        head = json.loads(body.decode("utf-8"))
    except (UnicodeDecodeError, ValueError):
        raise ProtocolError("frame head is not readable JSON") from None
    if not isinstance(head, dict):
        raise ProtocolError("frame head is not an object")
    size = head.get("payload", 0)
    if isinstance(size, bool) or not isinstance(size, int) or size < 0:
        raise ProtocolError(f"frame payload size {size!r} is not a whole number")
    if size > MAX_PAYLOAD_BYTES:
        raise ProtocolError(f"frame payload of {size} bytes is too large")
    payload = _read_maybe_short(read, size) if size else b""
    if size and len(payload) < size:
        raise ProtocolError("stream ended mid-frame")
    return head, payload
