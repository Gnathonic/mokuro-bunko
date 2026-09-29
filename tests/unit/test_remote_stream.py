"""The assignment stream: one JSON object a line, for as long as it lives."""

from __future__ import annotations

import http.client
import json
import threading
import time
from typing import Any

import pytest

from mokuro_bunko.ocr.remote import library_api
from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry

CATALOG: dict[str, Any] = {"engines": ["hayai-nova"], "detectors": ["ctd"], "devices": []}


@pytest.fixture(autouse=True)
def fast_heartbeat(monkeypatch: pytest.MonkeyPatch) -> None:
    """Every test here would otherwise wait a real 15 s on an idle queue."""
    monkeypatch.setattr(library_api, "HEARTBEAT_SECONDS", 0.05)


def _api() -> tuple[ProcessorAPI, ProcessorRegistry, Any]:
    registry = ProcessorRegistry()
    entry = registry.register(username="tower", name="tower", host={},
                              catalog=CATALOG, max_sessions=1)
    return ProcessorAPI(lambda e, s: [b""], registry), registry, entry


def _open(app: ProcessorAPI, pid: str, *, username: str = "tower") -> Any:
    environ = {
        "REQUEST_METHOD": "GET",
        "PATH_INFO": f"/_processor/{pid}/stream",
        "mokuro.role": "processor",
        "mokuro.username": username,
    }
    captured: list[tuple[str, list[tuple[str, str]]]] = []
    body = app(environ, lambda s, h: captured.append((s, h)))
    return captured, body


def test_the_stream_is_ndjson_and_unbuffered() -> None:
    app, registry, entry = _api()
    captured, body = _open(app, entry.processor_id)
    entry.send({"op": "open_session", "sid": "s1", "generation": {"id": "g-2"}})
    registry.drop(entry.processor_id, "test over")
    lines = [json.loads(chunk) for chunk in body if chunk.strip()]
    status, headers = captured[0]
    assert status.startswith("200")
    assert dict(headers)["Content-Type"] == "application/x-ndjson"
    assert dict(headers)["Cache-Control"] == "no-store"
    assert dict(headers)["X-Accel-Buffering"] == "no"
    assert lines[0]["op"] == "open_session"


def test_only_the_account_that_registered_it_may_read_it() -> None:
    app, _registry, entry = _api()
    captured, body = _open(app, entry.processor_id, username="someone-else")
    b"".join(body)
    assert captured[0][0].startswith("403")


def test_an_unknown_processor_id_is_a_404_not_a_hang() -> None:
    app, _registry, _entry = _api()
    captured, body = _open(app, "deadbeefdeadbeef")
    b"".join(body)
    assert captured[0][0].startswith("404")


def test_a_second_stream_drops_the_first_and_asks_it_to_register_again() -> None:
    """A second stream means the first is a ghost this end has not noticed."""
    app, registry, entry = _api()
    _captured, first = _open(app, entry.processor_id)
    iterator = iter(first)
    next(iterator)  # a heartbeat; the stream is now marked open
    assert entry.stream_open is True

    captured, second = _open(app, entry.processor_id)
    b"".join(second)
    assert captured[0][0].startswith("409")
    assert registry.get(entry.processor_id) is None, "the ghost entry was dropped"
    # The ghost's own response must END, not sit there holding a cheroot
    # worker thread until something else closes it: dropping the entry put
    # the sentinel on its queue.
    assert list(iterator) == [], "the first stream ran to its sentinel"
    assert entry.stream_open is False


def test_a_heartbeat_arrives_on_an_idle_stream() -> None:
    app, registry, entry = _api()
    _captured, body = _open(app, entry.processor_id)
    iterator = iter(body)
    beats = [json.loads(next(iterator)) for _ in range(3)]
    registry.drop(entry.processor_id, "test over")
    assert all(beat["op"] == "heartbeat" for beat in beats)


def test_our_own_heartbeats_are_not_news_from_the_processor() -> None:
    """`last_seen` is evidence from the FAR end, and a heartbeat is ours.

    Stamping it as we hand an op to the socket would leave a processor that
    lost power reading "seen just now" for as long as the kernel buffered
    those writes -- and `to_dict()` publishes the field to the admin panel.
    It buys the stale-registration reaper nothing either, since that only
    looks at entries with no stream at all.
    """
    app, registry, entry = _api()
    entry.last_seen -= 60.0
    registered_at = entry.last_seen

    _captured, body = _open(app, entry.processor_id)
    iterator = iter(body)
    beats = [json.loads(next(iterator)) for _ in range(2)]
    assert [beat["op"] for beat in beats] == ["heartbeat", "heartbeat"]
    assert entry.last_seen == registered_at, "a heartbeat is not far-end evidence"
    assert entry.to_dict()["last_seen"] == registered_at

    entry.send({"op": "cancel", "sid": "s1", "claim": "v0"})
    assert json.loads(next(iterator))["op"] == "cancel"
    assert entry.last_seen == registered_at, "nor is any other op we send"
    registry.drop(entry.processor_id, "test over")
    assert list(iterator) == []


def test_cheroot_really_streams_it_chunk_by_chunk() -> None:
    """The proof, against a real server on a throwaway port.

    Nothing else in this codebase returns a generator from a WSGI app, so
    "cheroot streams responses" is asserted here rather than assumed. What
    is checked is not only that the lines arrive but that they arrive
    SPREAD OUT -- a buffering server would deliver all three at the end.
    """
    from cheroot.wsgi import Server as WSGIServer

    app, registry, entry = _api()

    def with_actor(environ: dict[str, Any], start_response: Any) -> Any:
        environ["mokuro.role"] = "processor"
        environ["mokuro.username"] = "tower"
        return app(environ, start_response)

    server = WSGIServer(("127.0.0.1", 0), with_actor, numthreads=4)
    server.prepare()
    port = server.bind_addr[1]
    threading.Thread(target=server.serve, daemon=True).start()
    try:
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
        connection.request("GET", f"/_processor/{entry.processor_id}/stream")
        response = connection.getresponse()
        assert response.getheader("Transfer-Encoding") == "chunked"

        def push() -> None:
            for index in range(3):
                time.sleep(0.3)
                entry.send({"op": "cancel", "sid": "s1", "claim": f"v{index}"})
            time.sleep(0.3)
            registry.drop(entry.processor_id, "test over")

        threading.Thread(target=push, daemon=True).start()
        started = time.monotonic()
        arrivals = []
        while len(arrivals) < 3:
            line = response.readline()
            op = json.loads(line)
            if op.get("op") == "heartbeat":
                continue
            arrivals.append((time.monotonic() - started, op))
        connection.close()
    finally:
        server.stop()

    assert [a[1]["claim"] for a in arrivals] == ["v0", "v1", "v2"]
    assert arrivals[-1][0] > 0.5, "a buffering server would deliver them together"


class _WatchedLock:
    """`entry.lock`, but it remembers whether it is held right now."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.held = False

    def __enter__(self) -> _WatchedLock:
        self._lock.acquire()
        self.held = True
        return self

    def __exit__(self, *exc: object) -> None:
        self.held = False
        self._lock.release()


def _watch_stream_flag(entry: Any) -> list[bool]:
    """Record every write of `entry.stream_open` made without `entry.lock`.

    A plain bool cannot report who wrote it, so the flag becomes a property
    for the length of one test and the entry's lock becomes one that knows
    whether it is held.
    """
    lock = _WatchedLock()
    unguarded: list[bool] = []

    class Watched(type(entry)):  # type: ignore[misc]
        @property
        def stream_open(self) -> bool:
            return bool(self._flag)

        @stream_open.setter
        def stream_open(self, value: bool) -> None:
            if not lock.held:
                unguarded.append(value)
            self._flag = value

    entry._flag = entry.__dict__.pop("stream_open", False)
    entry.lock = lock
    entry.__class__ = Watched
    return unguarded


def test_the_stream_flag_is_written_under_the_entrys_lock() -> None:
    """`stream_open` travels with `dropped`, so it shares `dropped`'s lock.

    The registry clears both together when a processor is dropped; if the
    stream set its own flag outside that lock, a reader holding the lock
    could see an entry that is gone and still claiming a live stream -- and
    the registry's eviction, which now spares a streaming entry, reads it.
    """
    app, registry, entry = _api()
    unguarded = _watch_stream_flag(entry)

    _captured, body = _open(app, entry.processor_id)
    iterator = iter(body)
    next(iterator)  # the generator's first pass: the stream is open
    assert entry.stream_open is True

    registry.drop(entry.processor_id, "test over")
    assert list(iterator) == [], "the sentinel ends it"
    assert entry.stream_open is False
    assert unguarded == [], "stream_open was written without entry.lock"


def _watched(on_drop: Any) -> tuple[ProcessorAPI, ProcessorRegistry, Any]:
    """`_api()`, but with a listener on the registry's drops."""
    registry = ProcessorRegistry(on_drop=on_drop)
    entry = registry.register(
        username="tower", name="tower", host={}, catalog=CATALOG, max_sessions=1
    )
    return ProcessorAPI(lambda e, s: [b""], registry), registry, entry


def test_a_stream_that_ends_drops_its_processor() -> None:
    """The response closing is the only news that a processor has gone."""
    seen: list[tuple[str, str]] = []
    app, registry, entry = _watched(lambda e, r: seen.append((e.processor_id, r)))

    _captured, body = _open(app, entry.processor_id)
    iterator = iter(body)
    assert json.loads(next(iterator))["op"] == "heartbeat"
    iterator.close()  # the far end hung up

    assert entry.stream_open is False
    assert registry.get(entry.processor_id) is None
    assert seen == [(entry.processor_id, "stream closed")]


def test_a_re_registration_is_not_re_announced_as_the_stream_closing() -> None:
    """Whoever dropped the entry owns the reason for it.

    A processor that reconnects registers first, and `register` drops the
    old entry as "re-registered" -- which Task 9 reads as "its claims come
    straight back", not as hardware going away. The replaced response then
    ends, and must announce nothing of its own: one drop, one reason, and
    the entry that replaced it untouched.

    The stream is watched for the CALL and not only for its effect: a drop
    of an id the registry has already forgotten happens to be a no-op
    today, so a stream that asked for one anyway would look innocent here
    until Task 9 gave that call a meaning.
    """
    seen: list[tuple[str, str]] = []
    app, registry, entry = _watched(lambda e, r: seen.append((e.processor_id, r)))
    asked: list[tuple[str, str]] = []
    real_drop = registry.drop

    def watched_drop(processor_id: str, reason: str) -> Any:
        asked.append((processor_id, reason))
        return real_drop(processor_id, reason)

    _captured, body = _open(app, entry.processor_id)
    iterator = iter(body)
    assert json.loads(next(iterator))["op"] == "heartbeat"
    assert entry.stream_open is True

    again = registry.register(
        username="tower", name="tower", host={}, catalog=CATALOG, max_sessions=1
    )
    registry.drop = watched_drop  # type: ignore[method-assign]
    assert list(iterator) == [], "the replaced stream ends on its sentinel"
    assert asked == [], "the stream asked the registry to drop something"
    assert seen == [(entry.processor_id, "re-registered")], "not a second drop"
    assert registry.get(again.processor_id) is again, "the new entry stands"
