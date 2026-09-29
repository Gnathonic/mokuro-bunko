"""A remote session, event by event, against the same handlers a local one uses."""

from __future__ import annotations

import http.client
import io
import json
import logging
import socket
import threading
from collections.abc import Callable, Iterator
from contextlib import contextmanager, suppress
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
from mokuro_bunko.ocr.remote.protocol import (
    ARCHIVES_ROOT,
    PROTOCOL_VERSION,
    clean_archives_root,
    encode_frame,
)
from mokuro_bunko.ocr.remote.registry import ProcessorEntry, ProcessorRegistry
from mokuro_bunko.ocr.remote.session import RemoteSession
from mokuro_bunko.ocr.session import SessionVolume

CATALOG: dict[str, Any] = {"engines": ["hayai-nova"], "detectors": ["ctd"], "devices": []}


def _row() -> GenerationSpec:
    return parse_generation_list(
        [
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "hayai-nova", "engine": "hayai-nova", "detector": "ctd"},
        ]
    )[1]


def _volume(tmp_path: Path, job_id: str = "v1") -> SessionVolume:
    workspace = tmp_path / "ws" / job_id
    workspace.mkdir(parents=True, exist_ok=True)
    return SessionVolume(
        id=job_id,
        workspace=workspace,
        output=workspace / "Volume 1.hayai-nova.mokuro",
        cache_dir=workspace / "_ocr",
        detect_dir=workspace / "_detect",
        log=tmp_path / "logs" / "Alpha_Volume 1.hayai-nova.log",
        title="Alpha",
        volume="Volume 1",
        archive=tmp_path / "library" / "Alpha" / "Volume 1.cbz",
        title_uuid="title-uuid",
        volume_uuid="volume-uuid",
    )


@pytest.fixture
def session(tmp_path: Path) -> RemoteSession:
    registry = ProcessorRegistry()
    entry = registry.register(username="tower", name="tower", host={"gpu": "RTX 4090"},
                              catalog=CATALOG, max_sessions=1)
    return RemoteSession(
        entry,
        _row(),
        sid="s1",
        row_spec={"id": "g-2", "engine": "hayai-nova", "detector": "ctd"},
        library_path=tmp_path / "library",
    )


class TestTheDuckType:
    def test_it_satisfies_every_member_the_watcher_touches(
        self, session: RemoteSession
    ) -> None:
        from mokuro_bunko.ocr.session import OcrSession

        for name in ("start", "submit", "poll_event", "close", "kill", "wait",
                     "is_alive", "join_reader", "stderr_tail"):
            assert callable(getattr(session, name)), name
            assert callable(getattr(OcrSession, name)), name
        for name in ("generation", "closing", "killed"):
            assert hasattr(session, name), name
        assert {session} == {session}, "hashable, for OCRWorker._open_sessions"

    def test_start_sends_open_session_and_registers_the_session(
        self, session: RemoteSession
    ) -> None:
        assert session.start() is True
        assert session.entry.sessions["s1"] is session
        assert session.entry.ops.get_nowait() == {
            "op": "open_session",
            "sid": "s1",
            "generation": {"id": "g-2", "engine": "hayai-nova", "detector": "ctd"},
        }

    def test_submit_sends_a_portable_volume_op_not_local_paths(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        session.start()
        session.entry.ops.get_nowait()
        assert session.submit(_volume(tmp_path)) is True
        assert session.entry.ops.get_nowait() == {
            "op": "volume",
            "sid": "s1",
            "claim": "v1",
            # The ONE archives root, not a second copy of the string: what
            # the op names has to be what the register reply advertised.
            "archive": ARCHIVES_ROOT + "Alpha/Volume 1.cbz",
            "sidecar_name": "Volume 1.hayai-nova.mokuro",
            "title": "Alpha",
            "volume_title": "Volume 1",
            "title_uuid": "title-uuid",
            "volume_uuid": "volume-uuid",
        }
        assert session.claims() == ["v1"]

    def test_submit_leaves_a_local_log_naming_the_processor(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        session.start()
        volume = _volume(tmp_path)
        session.submit(volume)
        assert "tower" in volume.log.read_text(encoding="utf-8")

    def test_a_volume_that_could_not_be_sent_leaves_no_note_behind(
        self, tmp_path: Path
    ) -> None:
        """The note says this volume is being processed remotely, so it is
        written only once the op really is on its way -- a note in the log
        of a volume that never left is worse than no note."""
        _app, registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        volume = _volume(tmp_path)
        registry.drop(entry.processor_id, "it went away")

        assert session.submit(volume) is False
        assert not volume.log.exists(), "nothing claims it was processed remotely"
        assert session.claims() == [], "and the claim was given straight back"

    def test_a_session_never_holds_more_than_two_outstanding_volumes(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        """Spec section 3 rule 3, pinned on the object that holds them."""
        session.start()
        assert session.submit(_volume(tmp_path, "v1")) is True
        assert session.submit(_volume(tmp_path, "v2")) is True
        assert session.submit(_volume(tmp_path, "v3")) is False, "the third is refused"
        assert session.claims() == ["v1", "v2"]
        session.feed({"event": "volume_done", "id": "v1", "pages": 3}, b"")
        assert session.submit(_volume(tmp_path, "v3")) is True
        assert session.claims() == ["v2", "v3"]

    def test_close_and_kill_say_so_and_end_the_session(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        session.start()
        session.submit(_volume(tmp_path))
        while not session.entry.ops.empty():
            session.entry.ops.get_nowait()
        session.close()
        assert session.closing is True
        assert session.entry.ops.get_nowait() == {"op": "close_session", "sid": "s1"}

        session.kill()
        assert session.killed is True
        sent = [session.entry.ops.get_nowait() for _ in range(2)]
        assert {"op": "cancel", "sid": "s1", "claim": "v1"} in sent
        assert session.poll_event(timeout=1.0)["event"] == "exit"
        assert session.is_alive() is False


class TestProtocolTwo:
    """The archive's own events: `fetch` and `volume_returned`."""

    def test_the_op_carries_the_archive_s_size(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        from dataclasses import replace

        session.start()
        session.entry.ops.get_nowait()
        assert session.submit(replace(_volume(tmp_path), archive_size=65_665_250))
        op = session.entry.ops.get_nowait()
        assert op["size"] == 65_665_250
        assert session.submit(_volume(tmp_path, "v2"))
        assert "size" not in session.entry.ops.get_nowait(), "no stat, no size"

    def test_a_returned_claim_frees_its_outstanding_slot(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        session.start()
        assert session.submit(_volume(tmp_path, "v1"))
        assert session.submit(_volume(tmp_path, "v2"))
        assert session.submit(_volume(tmp_path, "v3")) is False, "two outstanding"
        session.feed({"event": "volume_returned", "id": "v1", "class": "stalled",
                      "error": "x" * 1000}, b"")
        assert session.claims() == ["v2"]
        assert session.submit(_volume(tmp_path, "v3")) is True
        event = session.poll_event(timeout=1.0)
        assert event is not None and event["event"] == "volume_returned"
        assert len(event["error"]) == 300
        transfer = session.entry.to_dict()["transfer"]
        assert transfer["returned"] == 1
        assert transfer["last_returned"]["class"] == "stalled"

    def test_fetch_progress_reaches_the_watcher(self, session: RemoteSession) -> None:
        session.start()
        session.feed({"event": "fetch", "id": "v1", "state": "downloading",
                      "bytes": 1, "total": 2, "requests": 1}, b"")
        event = session.poll_event(timeout=1.0)
        assert event == {"event": "fetch", "id": "v1", "state": "downloading",
                         "bytes": 1, "total": 2, "requests": 1}
        assert session.entry.to_dict()["transfer"]["volumes"] == 0, "progress is not a delivery"

    def test_a_ready_is_folded_into_the_processor_s_transfer_numbers(
        self, session: RemoteSession
    ) -> None:
        session.start()
        session.feed({"event": "fetch", "id": "v1", "state": "ready", "bytes": 50_000_000,
                      "seconds": 0.5, "requests": 2, "restarts": 0, "repairs": 1}, b"")
        assert session.poll_event(timeout=1.0)["state"] == "ready"  # type: ignore[index]
        transfer = session.entry.to_dict()["transfer"]
        assert transfer["volumes"] == 1
        assert transfer["mb_per_s"] == 100.0
        assert transfer["resumed"] == 1 and transfer["repaired"] == 1

    def test_garbage_numbers_in_a_fetch_head_are_tolerated(
        self, session: RemoteSession
    ) -> None:
        session.start()
        session.feed({"event": "fetch", "id": "v1", "state": "ready", "bytes": "lots",
                      "seconds": None, "requests": True, "restarts": [1]}, b"")
        assert session.poll_event(timeout=1.0) is not None
        transfer = session.entry.to_dict()["transfer"]
        assert transfer["volumes"] == 1 and transfer["mb_per_s"] is None


class TestFeeding:
    def test_runner_events_are_queued_verbatim(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        session.start()
        session.submit(_volume(tmp_path))
        for event in (
            {"event": "ready", "startup_seconds": 9.5, "pipeline": "detect -> engine"},
            {"event": "volume_started", "id": "v1", "pages": 12},
            {"event": "page", "id": "v1", "done": 1, "total": 12},
            {"event": "stats", "pipeline": {"items": 1}},
        ):
            session.feed(event, b"")
        assert [session.poll_event(timeout=1.0) for _ in range(4)] == [
            {"event": "ready", "startup_seconds": 9.5, "pipeline": "detect -> engine"},
            {"event": "volume_started", "id": "v1", "pages": 12},
            {"event": "page", "id": "v1", "done": 1, "total": 12},
            {"event": "stats", "pipeline": {"items": 1}},
        ]

    def test_an_event_this_protocol_does_not_define_is_dropped(
        self, session: RemoteSession
    ) -> None:
        session.start()
        session.feed({"event": "rm -rf", "id": "v1"}, b"")
        assert session.poll_event(timeout=0.1) is None

    def test_a_sidecar_lands_on_local_disk_and_is_never_an_event(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        session.start()
        volume = _volume(tmp_path)
        session.submit(volume)
        blob = json.dumps({"version": "0.0", "pages": [], "chars": 0}).encode()
        session.feed(
            {"event": "sidecar", "id": "v1",
             "name": "Volume 1.hayai-nova.mokuro", "payload": len(blob)},
            blob,
        )
        assert volume.output.read_bytes() == blob
        assert not list(volume.output.parent.glob("*.tmp")), "written tmp + replace"
        session.feed({"event": "volume_done", "id": "v1", "pages": 12}, b"")
        assert session.poll_event(timeout=1.0)["event"] == "volume_done"

    def test_a_sidecar_under_a_different_name_is_refused_not_written(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        """`name` is checked, not decorative: the library names the file."""
        session.start()
        volume = _volume(tmp_path)
        session.submit(volume)
        session.feed(
            {"event": "sidecar", "id": "v1", "name": "somewhere-else.mokuro",
             "payload": 2},
            b"{}",
        )
        assert not volume.output.exists()
        assert session.poll_event(timeout=1.0)["event"] == "exit", "and it costs the session"

    def test_a_sidecar_with_no_name_at_all_is_refused_the_same_way(
        self, session: RemoteSession, tmp_path: Path
    ) -> None:
        """The check is UNCONDITIONAL. A processor that cannot name the file
        it has just produced cannot be trusted with the next volume, and a
        missing name is not a licence to guess."""
        session.start()
        volume = _volume(tmp_path)
        session.submit(volume)
        session.feed({"event": "sidecar", "id": "v1", "payload": 2}, b"{}")
        assert not volume.output.exists()
        assert session.poll_event(timeout=1.0)["event"] == "exit"

    def test_a_ping_is_not_an_event(self, session: RemoteSession) -> None:
        session.start()
        session.feed({"event": "ping"}, b"")
        assert session.poll_event(timeout=0.1) is None

    def test_a_fatal_becomes_the_stderr_tail_the_watcher_reads(
        self, session: RemoteSession
    ) -> None:
        session.start()
        session.feed({"event": "fatal", "error": "detector pool would not start"}, b"")
        assert session.poll_event(timeout=1.0)["event"] == "fatal"
        assert session.stderr_tail() == "detector pool would not start"

    def test_ending_pushes_exactly_one_exit(self, session: RemoteSession) -> None:
        session.start()
        session.end("the events stream closed")
        session.end("again")
        assert session.poll_event(timeout=1.0) == {"event": "exit", "returncode": None}
        assert session.poll_event(timeout=0.1) is None
        assert session.entry.sessions == {}

    def test_two_threads_ending_it_at_once_still_push_exactly_one_exit(
        self, tmp_path: Path
    ) -> None:
        """They really do arrive together: `kill()` runs on the worker
        thread while the sink's `end()` runs on the events thread, and a
        second `exit` would end the NEXT session the watcher polls.

        The interleaving is FORCED, not raced. The gap between reading the
        claim and taking it is two bytecodes, and a barrier never lands in
        it: measured against a deliberately non-atomic `_finish`, 0 bad
        rounds in 1000, at 1e-6 and 1e-9 switch intervals and at 2 and 8
        threads -- a timing test here would pass on the broken code and
        carry no signal at all. So instead every thread that reads the flag
        as False waits for another to read it too, which is exactly what
        one critical section makes impossible: the second thread is still
        waiting for `_lock` and never reaches the read.
        """
        _app, _registry, entry = _connected()
        session = RemoteSession(entry, _row(), sid="s1", row_spec={},
                                library_path=tmp_path / "library")
        read_together = threading.Barrier(2)

        class Interleaved(type(session)):  # type: ignore[misc, valid-type]
            @property
            def _ending(self) -> bool:
                # What this reader SAW, not what the flag says once the
                # other thread has been let through -- returning the later
                # value would hand the unsynchronised version the answer the
                # lock is supposed to be what gives it.
                seen = bool(self._flag)
                if not seen:
                    # Broken by the timeout when the other thread is locked
                    # out, which is the passing case.
                    with suppress(threading.BrokenBarrierError):
                        read_together.wait(timeout=0.25)
                return seen

            @_ending.setter
            def _ending(self, value: bool) -> None:
                self._flag = value

        session._flag = session.__dict__.pop("_ending")
        session.__class__ = Interleaved

        threads = [
            threading.Thread(target=session.end, args=("at the same moment",))
            for _ in range(2)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=10.0)

        drained = []
        while (event := session.poll_event(timeout=0.0)) is not None:
            drained.append(event)
        assert drained == [{"event": "exit", "returncode": None}]


def _connected() -> tuple[ProcessorAPI, ProcessorRegistry, ProcessorEntry]:
    """A registered processor and the API its events would reach."""
    registry = ProcessorRegistry()
    entry = registry.register(username="tower", name="tower", host={},
                              catalog=CATALOG, max_sessions=2)
    return ProcessorAPI(lambda e, s: [b""], registry), registry, entry


def _status_and_code(answer: tuple[int, dict[str, Any]]) -> tuple[int, str]:
    """A refusal as the client reads it: the status, and what to do about it."""
    status, reply = answer
    return status, str(reply.get("code"))


def _opened(entry: ProcessorEntry, sid: str, tmp_path: Path) -> RemoteSession:
    session = RemoteSession(entry, _row(), sid=sid, row_spec={"id": "g-2"},
                            library_path=tmp_path / "library")
    session.start()
    return session


@contextmanager
def _serving(app: ProcessorAPI, seen: list[tuple[str, str]] | None = None) -> Iterator[int]:
    """The app behind a real cheroot server on a throwaway port.

    Yields the port; stops the server and joins its thread in a `finally`,
    so no listener and no thread outlives the test.
    """
    from cheroot.wsgi import Server as WSGIServer

    def with_actor(environ: dict[str, Any], start_response: Any) -> Any:
        environ["mokuro.role"] = "processor"
        environ["mokuro.username"] = "tower"
        if seen is not None:
            seen.append(
                (
                    environ.get("HTTP_TRANSFER_ENCODING", ""),
                    type(environ["wsgi.input"]).__name__,
                )
            )
        return app(environ, start_response)

    server = WSGIServer(("127.0.0.1", 0), with_actor, numthreads=4)
    server.prepare()
    thread = threading.Thread(target=server.serve, daemon=True)
    thread.start()
    try:
        yield int(server.bind_addr[1])
    finally:
        server.stop()
        thread.join(timeout=5.0)


def test_the_archives_root_is_one_string_not_two(tmp_path: Path) -> None:
    """What the register reply advertises and what a `volume` op names have
    to be the same string.

    They were two literals -- `ProcessorAPI`'s argument default and a
    hard-coded one in `submit` -- which is a 404 on every download the day
    one of them moves. One constant now, and both take their default from
    it.
    """
    app, _registry, entry = _connected()
    session = _opened(entry, "s1", tmp_path)
    assert app.archives_root == session.archives_root == ARCHIVES_ROOT
    assert json.loads(  # and it is what the processor is told at registration
        b"".join(
            app(
                {
                    "REQUEST_METHOD": "POST",
                    "PATH_INFO": "/_processor/register",
                    "CONTENT_LENGTH": str(len(body := json.dumps(
                        {"protocol": PROTOCOL_VERSION, "name": "tower-2"}).encode())),
                    "wsgi.input": io.BytesIO(body),
                    "mokuro.role": "processor",
                    "mokuro.username": "tower",
                },
                lambda s, h: None,
            )
        )
    )["archives"] == ARCHIVES_ROOT

    # And they still agree once it is CONFIGURED, which is the only way
    # they can drift now: both normalise through the same helper, so a root
    # written any of the plausible ways lands on the same string.
    for configured in ("/manga", "manga/", "//manga//", "manga"):
        elsewhere = ProcessorAPI(lambda e, s: [b""], _registry,
                                 archives_root=configured)
        moved = RemoteSession(entry, _row(), sid="s2", row_spec={},
                              library_path=tmp_path, archives_root=configured)
        assert elsewhere.archives_root == moved.archives_root == "/manga/", configured
    assert clean_archives_root("") == "/", "the library root itself is still a root"


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


class _WatchedSessions(dict):  # type: ignore[type-arg]
    """`entry.sessions`, but it records writes made without the lock."""

    def __init__(self, lock: _WatchedLock, unguarded: list[tuple[str, str]]) -> None:
        super().__init__()
        self._lock = lock
        self._unguarded = unguarded

    def __setitem__(self, key: Any, value: Any) -> None:
        if not self._lock.held:
            self._unguarded.append(("set", key))
        super().__setitem__(key, value)

    def __delitem__(self, key: Any) -> None:
        if not self._lock.held:
            self._unguarded.append(("del", key))
        super().__delitem__(key)


def test_the_sessions_dict_is_only_ever_written_under_the_entrys_lock(
    tmp_path: Path,
) -> None:
    """Sessions come and go on the scheduler and the events threads while
    the admin panel reads `entry.sessions` on the request thread, and
    `ProcessorEntry`'s contract puts every WRITER of that dict under
    `entry.lock`. This is the brief's snippet's one real divergence (it
    wrote the dict bare), so it is pinned rather than trusted.

    Pinned STRUCTURALLY, because a load test cannot see this: today's only
    reader, `to_dict()` -> `_open_sessions()`, snapshots with `tuple(self
    .sessions)`, and `tuple(dict)` is one C call that never releases the
    GIL -- so no interleaving exists for it to catch, at any switch
    interval (measured: 0 failures with both writes unguarded). The lock
    still has to hold for the reader that iterates at Python level, which
    is what this asserts directly.
    """
    _app, _registry, entry = _connected()
    lock = _WatchedLock()
    unguarded: list[tuple[str, str]] = []
    entry.lock = lock  # type: ignore[assignment]
    entry.sessions = _WatchedSessions(lock, unguarded)

    session = _opened(entry, "s1", tmp_path)  # start(): the insert
    assert dict(entry.sessions) == {"s1": session}
    session.end("the events stream closed")  # _finish(): the delete
    assert dict(entry.sessions) == {}
    assert unguarded == [], "entry.sessions was written without entry.lock"


class _HookedBody:
    """A request body that runs a hook between one frame and the next.

    The hook is hung on the READ that starts the second frame, so the first
    frame is whole and has been fed while the second is still on the wire.
    That is the only moment worth observing in this channel -- both what the
    sink has already done with frame one, and what happens to frame two when
    the world changes underneath it.
    """

    def __init__(self, data: bytes, boundary: int, on_boundary: Callable[[], None]) -> None:
        self._data = data
        self._boundary = boundary
        self._on_boundary = on_boundary
        self._fired = False
        self._pos = 0

    def read(self, size: int) -> bytes:
        if not self._fired and self._pos >= self._boundary:
            self._fired = True
            self._on_boundary()
        chunk = self._data[self._pos : self._pos + size]
        self._pos += len(chunk)
        return chunk


class _GoesSilent:
    """A body that delivers its frames and then never speaks again.

    What cheroot's socket does to an events body idle past
    `HTTPServer.timeout`, without spending the ten seconds on it.
    """

    def __init__(self, data: bytes) -> None:
        self._data = data
        self._pos = 0

    def read(self, size: int) -> bytes:
        if self._pos >= len(self._data):
            raise TimeoutError("timed out")
        chunk = self._data[self._pos : self._pos + size]
        self._pos += len(chunk)
        return chunk


class TestTheSink:
    @staticmethod
    def _post(app: ProcessorAPI, pid: str, sid: str, body: bytes | Any,
              *, username: str = "tower") -> tuple[int, dict[str, Any]]:
        environ = {
            "REQUEST_METHOD": "POST",
            "PATH_INFO": f"/_processor/{pid}/sessions/{sid}/events",
            "HTTP_TRANSFER_ENCODING": "chunked",
            "wsgi.input": io.BytesIO(body) if isinstance(body, bytes) else body,
            "mokuro.role": "processor",
            "mokuro.username": username,
        }
        captured: list[str] = []
        chunks = b"".join(app(environ, lambda s, h: captured.append(s)))
        return int(captured[0].split()[0]), json.loads(chunks or b"{}")

    def test_frames_reach_the_session_and_the_count_comes_back(
        self, tmp_path: Path
    ) -> None:
        registry = ProcessorRegistry()
        entry = registry.register(username="tower", name="tower", host={},
                                  catalog=CATALOG, max_sessions=1)
        session = RemoteSession(entry, _row(), sid="s1", row_spec={"id": "g-2"},
                                library_path=tmp_path / "library")
        session.start()
        app = ProcessorAPI(lambda e, s: [b""], registry)
        body = (
            encode_frame({"event": "volume_started", "id": "v1", "pages": 3})
            + encode_frame({"event": "page", "id": "v1", "done": 1, "total": 3})
        )
        status, reply = self._post(app, entry.processor_id, "s1", body)
        assert status == 200
        assert reply["received"] == 2
        assert session.poll_event(timeout=1.0)["event"] == "volume_started"
        assert session.poll_event(timeout=1.0)["event"] == "page"
        assert session.poll_event(timeout=1.0)["event"] == "exit", "the body ended"

    def test_someone_elses_session_is_refused(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        entry = registry.register(username="tower", name="tower", host={},
                                  catalog=CATALOG, max_sessions=1)
        RemoteSession(entry, _row(), sid="s1", row_spec={"id": "g-2"},
                      library_path=tmp_path / "library").start()
        app = ProcessorAPI(lambda e, s: [b""], registry)
        status, reply = self._post(app, entry.processor_id, "s1", b"", username="someone-else")
        assert (status, reply["code"]) == (403, "not_owner")

    def test_an_unknown_session_id_is_refused(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        entry = registry.register(username="tower", name="tower", host={},
                                  catalog=CATALOG, max_sessions=1)
        app = ProcessorAPI(lambda e, s: [b""], registry)
        status, reply = self._post(app, entry.processor_id, "nope", b"")
        assert (status, reply["code"]) == (404, "unknown")

    def test_a_torn_frame_is_a_400_and_ends_the_session(self, tmp_path: Path) -> None:
        """A cut connection is not a clean close, and the watcher has to
        hear about it: `read_frame` raises, and the session gets its exit."""
        app, _registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        whole = encode_frame({"event": "volume_started", "id": "v1", "pages": 3})
        torn = encode_frame({"event": "volume_done", "id": "v1", "pages": 3})[:6]

        status, reply = self._post(app, entry.processor_id, "s1", whole + torn)
        assert (status, reply["code"]) == (400, "bad_frame"), "do not retry this body"
        assert session.poll_event(timeout=1.0)["event"] == "volume_started"
        assert session.poll_event(timeout=1.0)["event"] == "exit"
        assert entry.sessions == {}, "and it is off its entry"

    def test_a_malformed_chunked_body_is_absorbed_not_raised(
        self, tmp_path: Path
    ) -> None:
        """Absorbing a broken body is this sink's whole job.

        cheroot's `ChunkedRFile` raises a BARE `ValueError` on a chunk-size
        line that is not hexadecimal, and an exception escaping `_events`
        would leave the session sitting in `entry.sessions` with nobody left
        to feed it -- the watcher would then wait out SESSION_WEDGE_SECONDS
        on `poll_event` for a runner that is already gone. Driven over a raw
        socket, because no HTTP client will send a body this broken.
        """
        app, _registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        request = (
            f"POST /_processor/{entry.processor_id}/sessions/s1/events HTTP/1.1\r\n"
            "Host: 127.0.0.1\r\n"
            "Transfer-Encoding: chunked\r\n"
            "Connection: close\r\n"
            "\r\n"
            "zzz\r\n"  # a chunk-size line that is not a number
        ).encode()

        with _serving(app) as port:
            with socket.create_connection(("127.0.0.1", port), timeout=20) as client:
                client.sendall(request)
                answer = b""
                while chunk := client.recv(4096):
                    answer += chunk

        head, _, raw = answer.partition(b"\r\n\r\n")
        assert head.startswith(b"HTTP/1.1 400"), head[:120]
        assert json.loads(raw)["code"] == "bad_frame", "do not retry this body"
        # The exception is OURS to read, not the client's: cheroot's message
        # is `Bad chunked transfer size: b'zzz'`, and only the log gets it.
        assert b"chunked transfer size" not in raw, raw
        assert session.poll_event(timeout=1.0)["event"] == "exit", "the session was told"
        assert entry.sessions == {}, "and it is off its entry, not left to wedge"

    def test_an_unexpected_failure_is_logged_with_its_stack(
        self, tmp_path: Path, caplog: pytest.LogCaptureFixture
    ) -> None:
        """The wide net also catches OUR bugs in `feed`/`_install`, and one
        warning line reading "TypeError" about code on this side of the wire
        is not a bug report. The client still gets the class and no more."""
        app, _registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)

        class _Explodes:
            def read(self, size: int) -> bytes:
                raise ValueError("a bug on our side of the wire")

        with caplog.at_level(logging.ERROR, logger="mokuro_bunko.ocr.remote.library_api"):
            status, reply = self._post(app, entry.processor_id, "s1", _Explodes())

        assert (status, reply["code"]) == (400, "bad_frame")
        with_stack = [record for record in caplog.records if record.exc_info is not None]
        assert with_stack, "no traceback was logged"
        assert with_stack[-1].exc_info[0] is ValueError  # type: ignore[index]
        assert "a bug on our side" not in reply["error"], "and it is not the client's"
        assert session.poll_event(timeout=1.0)["event"] == "exit"

    def test_a_second_events_body_for_one_session_is_refused(
        self, tmp_path: Path
    ) -> None:
        """One body per session, the way the stream has one per processor.

        The second POST is made from inside the first body's read, so the
        two really do overlap rather than merely follow one another.
        """
        app, _registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        first = encode_frame({"event": "page", "id": "v1", "done": 1, "total": 2})
        second: list[tuple[int, str]] = []
        body = _HookedBody(
            first + encode_frame({"event": "page", "id": "v1", "done": 2, "total": 2}),
            len(first),
            lambda: second.append(
                _status_and_code(
                    self._post(app, entry.processor_id, "s1",
                               encode_frame({"event": "ping"}))
                )
            ),
        )

        status, reply = self._post(app, entry.processor_id, "s1", body)
        assert second == [(409, "body_open")], "back off and retry, not re-register"
        assert (status, reply["received"]) == (200, 2), "and the first was not disturbed"
        assert session.poll_event(timeout=1.0)["event"] == "page"

    def test_a_sidecar_that_cannot_be_written_fails_the_session(
        self, tmp_path: Path
    ) -> None:
        """No file, no volume. Staying quiet would surface much later as a
        mystery "the sidecar is missing" against a blameless processor.

        The failure is observed WHILE the body is still arriving, because
        the body ending would end the session anyway and an assertion made
        afterwards could not tell the two apart. A directory sitting where
        the file goes makes `os.replace` fail AFTER the temp file has been
        written, which is also the only way to leave one behind.
        """
        app, _registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        volume = _volume(tmp_path)
        volume.output.mkdir(parents=True, exist_ok=True)
        session.submit(volume)
        sidecar = encode_frame(
            {"event": "sidecar", "id": "v1", "name": volume.output.name}, b"{}"
        )
        alive_after_the_write: list[bool] = []
        body = _HookedBody(
            sidecar + encode_frame({"event": "volume_done", "id": "v1", "pages": 1}),
            len(sidecar),
            lambda: alive_after_the_write.append(session.is_alive()),
        )

        status, reply = self._post(app, entry.processor_id, "s1", body)
        assert alive_after_the_write == [False], "the write failed the session there and then"
        assert (status, reply["received"]) == (409, 1), "the rest of the body is refused"
        # The SAME status as a drop, and a different instruction: this
        # processor is still connected and still has its other sessions.
        assert reply["code"] == "session_ended", "not `dropped` -- nothing to re-register"
        assert list(volume.workspace.glob("**/*.tmp")) == [], "no half-written temp left"
        assert list(volume.output.iterdir()) == [], "and nothing was written into it"
        assert session.poll_event(timeout=1.0)["event"] == "exit"
        assert session.poll_event(timeout=0.1) is None, "exactly one"

    def test_a_body_that_goes_silent_keeps_what_already_arrived(
        self, tmp_path: Path
    ) -> None:
        """cheroot times a socket out after `HTTPServer.timeout` (10 s), and
        the processor's 3 s `ping` is what normally prevents it -- so getting
        here means the processor really went away. What it managed to send
        still counts, and the session gets its exit rather than hanging."""
        app, _registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        body = _GoesSilent(
            encode_frame({"event": "page", "id": "v1", "done": 1, "total": 3})
        )

        status, reply = self._post(app, entry.processor_id, "s1", body)
        assert (status, reply["received"]) == (200, 1)
        assert session.poll_event(timeout=1.0)["event"] == "page"
        assert session.poll_event(timeout=1.0)["event"] == "exit"
        assert session.is_alive() is False

    # -- carried from Task 5's review -------------------------------------

    def test_a_dropped_processor_records_nothing_more(self, tmp_path: Path) -> None:
        """Its claims went back to the queue; its late events are not news."""
        app, registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        registry.drop(entry.processor_id, "re-registered")

        status, reply = self._post(
            app, entry.processor_id, "s1",
            encode_frame({"event": "volume_done", "id": "v1", "pages": 3}),
        )
        assert (status, reply["code"]) == (404, "unknown")
        assert session.poll_event(timeout=0.1) is None, "nothing was recorded"

    def test_a_drop_partway_through_stops_the_rest_being_recorded(
        self, tmp_path: Path
    ) -> None:
        """The entry can go while the body is still arriving, so every frame
        is checked -- not only the first one."""
        app, registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        first = encode_frame({"event": "volume_started", "id": "v1", "pages": 3})
        second = encode_frame({"event": "volume_done", "id": "v1", "pages": 3})
        body = _HookedBody(
            first + second,
            len(first),
            lambda: registry.drop(entry.processor_id, "re-registered"),
        )

        status, reply = self._post(app, entry.processor_id, "s1", body)
        assert (status, reply["code"]) == (409, "dropped"), "register again"
        assert reply["received"] == 1
        assert session.poll_event(timeout=1.0)["event"] == "volume_started"
        assert session.poll_event(timeout=1.0)["event"] == "exit"
        assert session.poll_event(timeout=0.1) is None, "the volume_done was refused"

    def test_a_session_that_is_no_longer_the_entrys_is_refused(
        self, tmp_path: Path
    ) -> None:
        """An ended sid, and a live sid posted under another processor's id."""
        app, registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        other = registry.register(username="tower", name="other", host={},
                                  catalog=CATALOG, max_sessions=1)
        frame = encode_frame({"event": "volume_done", "id": "v1", "pages": 3})

        status, reply = self._post(app, other.processor_id, "s1", frame)
        assert (status, reply["code"]) == (404, "unknown"), "s1 is not that one's session"

        session.end("the events stream closed")
        assert session.poll_event(timeout=1.0)["event"] == "exit"
        status, reply = self._post(app, entry.processor_id, "s1", frame)
        # Ended, not unknown: the processor moves on with its other sessions
        # instead of registering again (`TestEndedSessions`).
        assert (status, reply["code"]) == (409, "session_ended"), "nor this one's any more"
        assert session.poll_event(timeout=0.1) is None

    def test_only_a_frame_from_the_processor_stamps_last_seen(
        self, tmp_path: Path
    ) -> None:
        """Carried from Task 5: `last_seen` is far-end evidence, and this
        channel is now its only writer besides `register`."""
        app, registry, entry = _connected()
        quiet = _opened(entry, "s1", tmp_path)
        entry.last_seen -= 60.0
        registered_at = entry.last_seen

        status, reply = self._post(app, entry.processor_id, "s1", b"")
        assert (status, reply["received"]) == (200, 0)
        assert entry.last_seen == registered_at, "an empty body is not evidence"
        assert quiet.poll_event(timeout=1.0)["event"] == "exit"

        # A body that ends without its runner's exit drops its processor
        # (`TestABodyWithoutItsExit`), so the ping half is heard from a
        # second one.
        other = registry.register(username="tower", name="other", host={},
                                  catalog=CATALOG, max_sessions=1)
        other.last_seen -= 60.0
        other_registered_at = other.last_seen
        heard = _opened(other, "s2", tmp_path)
        status, reply = self._post(
            app, other.processor_id, "s2", encode_frame({"event": "ping"})
        )
        assert (status, reply["received"]) == (200, 1)
        assert other.last_seen > other_registered_at, "a ping says the processor is there"
        assert heard.poll_event(timeout=1.0)["event"] == "exit", "and is not an event"

    # -- the ordering the whole seam rests on ------------------------------

    def test_the_sidecar_is_on_disk_before_volume_done_can_be_polled(
        self, tmp_path: Path
    ) -> None:
        """`_collect_session_volume` reads the file the instant it sees
        `volume_done`, so the file has to be there FIRST.

        Observed at the one moment that can tell the two orders apart: the
        sidecar frame has been read and fed, and `volume_done` is still on
        the wire.
        """
        app, _registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        volume = _volume(tmp_path)
        session.submit(volume)
        blob = json.dumps({"version": "0.0", "pages": [], "chars": 0}).encode()
        sidecar = encode_frame({"event": "sidecar", "id": "v1",
                                "name": volume.output.name}, blob)
        done = encode_frame({"event": "volume_done", "id": "v1", "pages": 1})
        midway: list[tuple[bool, dict[str, Any] | None]] = []
        body = _HookedBody(
            sidecar + done,
            len(sidecar),
            lambda: midway.append((volume.output.exists(), session.poll_event(timeout=0.0))),
        )

        status, reply = self._post(app, entry.processor_id, "s1", body)
        assert (status, reply["received"]) == (200, 2)
        assert midway == [(True, None)], "written, and not queued as an event"
        assert volume.output.read_bytes() == blob
        assert session.poll_event(timeout=1.0)["event"] == "volume_done"

    def test_a_real_chunked_body_through_cheroot_feeds_the_session(
        self, tmp_path: Path
    ) -> None:
        """The proof against a real server on a throwaway port.

        The whole channel is length-framed because cheroot hands a chunked
        request body over as `ChunkedRFile`, whose `readline` never returns;
        only `read(n)` behaves. That claim is worth nothing asserted against
        a BytesIO, so this one posts real chunks over a real socket and
        checks the class the app was actually given.
        """
        app, _registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        volume = _volume(tmp_path)
        session.submit(volume)
        blob = json.dumps({"version": "0.0", "pages": [], "chars": 0}).encode()
        frames = [
            encode_frame({"event": "volume_started", "id": "v1", "pages": 1}),
            encode_frame({"event": "sidecar", "id": "v1", "name": volume.output.name}, blob),
            encode_frame({"event": "volume_done", "id": "v1", "pages": 1}),
        ]
        seen: list[tuple[str, str]] = []

        with _serving(app, seen) as port:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
            # An iterable body with no Content-Length: http.client chunks it
            # itself, which is the only way to get a ChunkedRFile in here.
            connection.request(
                "POST",
                f"/_processor/{entry.processor_id}/sessions/s1/events",
                body=iter(frames),
            )
            response = connection.getresponse()
            reply = json.loads(response.read())
            status = response.status
            connection.close()

        assert seen == [("chunked", "ChunkedRFile")], "a real chunked body"
        assert (status, reply["received"]) == (200, 3)
        assert volume.output.read_bytes() == blob, "the sidecar landed on local disk"
        assert [
            (session.poll_event(timeout=1.0) or {}).get("event") for _ in range(3)
        ] == ["volume_started", "volume_done", "exit"], "three frames, two events"


class _TornChunks:
    """A chunked body cut off mid-stream, as cheroot reports it.

    A processor process that dies with its events body open never sends the
    terminating chunk; cheroot's `ChunkedRFile` then finds an empty
    chunk-size line and raises a bare `ValueError` -- the exact message
    below is the one the end-to-end test saw.
    """

    def __init__(self, data: bytes) -> None:
        self._data = data
        self._pos = 0

    def read(self, size: int) -> bytes:
        if self._pos >= len(self._data):
            raise ValueError("Bad chunked transfer size: b''")
        chunk = self._data[self._pos : self._pos + size]
        self._pos += len(chunk)
        return chunk


class TestABodyWithoutItsExit:
    """A session's body ends with its runner's `exit`; one that ends without
    it means the PROCESSOR went away (spec sections 3 rule 4 and 6).

    Found by Task 10's end-to-end test: a `processor serve` stopped
    mid-volume tore its events body, the sink ended the session as though
    the runner had died, and the watcher blamed the volume -- a failure
    record and a backoff for a machine being switched off. The events body
    is the FIRST channel to notice a processor leaving (the assignment
    stream only notices on its next write, up to a heartbeat later), so the
    sink drops the processor there and then: `on_drop` returns every claim
    unrecorded before the session's exit reaches the watcher.
    """

    @staticmethod
    def _post(app: ProcessorAPI, pid: str, sid: str, body: Any) -> tuple[int, dict[str, Any]]:
        return TestTheSink._post(app, pid, sid, body)

    @pytest.mark.parametrize(
        "make_body",
        [
            pytest.param(lambda frames: frames, id="clean-eof"),
            pytest.param(_GoesSilent, id="went-silent"),
            pytest.param(_TornChunks, id="torn-chunks"),
        ],
    )
    def test_the_processor_is_dropped_before_the_session_ends(
        self, tmp_path: Path, make_body: Callable[[bytes], Any]
    ) -> None:
        registry = ProcessorRegistry()
        entry = registry.register(username="tower", name="tower", host={},
                                  catalog=CATALOG, max_sessions=1)
        app = ProcessorAPI(lambda e, s: [b""], registry)
        session = _opened(entry, "s1", tmp_path)
        dropped_while_alive: list[tuple[str, bool]] = []
        registry.on_drop = lambda gone, reason: dropped_while_alive.append(
            # `wait` answers None while the session has not ended yet.
            (gone.name, session.wait(timeout=0.0) is None)
        )

        self._post(app, entry.processor_id, "s1", make_body(
            encode_frame({"event": "volume_started", "id": "v1", "pages": 3})
        ))

        assert registry.get(entry.processor_id) is None, "the processor is gone"
        assert entry.dropped is True
        assert dropped_while_alive == [("tower", True)], (
            "dropped BEFORE the session's exit was queued, so the watcher "
            "finds the processor gone when it settles the claims"
        )
        assert session.poll_event(timeout=1.0)["event"] == "volume_started"
        assert session.poll_event(timeout=1.0) == {"event": "exit", "returncode": None}

    def test_a_body_that_ends_after_the_runners_exit_is_an_ordinary_close(
        self, tmp_path: Path
    ) -> None:
        app, registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        body = (
            encode_frame({"event": "volume_started", "id": "v1", "pages": 3})
            + encode_frame({"event": "exit", "returncode": 0})
        )

        status, reply = self._post(app, entry.processor_id, "s1", body)
        assert (status, reply["received"]) == (200, 2)
        assert session.poll_event(timeout=1.0)["event"] == "volume_started"
        assert session.poll_event(timeout=1.0) == {"event": "exit", "returncode": 0}, (
            "the runner's own exit, not one the drop would have made"
        )
        assert registry.get(entry.processor_id) is entry, "still connected"
        assert entry.dropped is False

    def test_a_session_the_library_already_ended_costs_nothing_more(
        self, tmp_path: Path
    ) -> None:
        """A kill (`cancel` + `close_session`) ends the session HERE first;
        the body closing afterwards is that session's tail, not news."""
        app, registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        other = _opened(entry, "s2", tmp_path)
        session.kill()

        self._post(app, entry.processor_id, "s1", b"")  # 404: already gone
        assert registry.get(entry.processor_id) is entry
        assert other.is_alive()


class TestEndedSessions:
    """A session the library ended is ENDED, not unknown, for a while.

    Live (and the settings-change integration test): the library killed
    a processor's session, forgot it at once, and the processor's events body
    for it -- which can arrive after the kill -- got 404 ``unknown``. That
    means "register again", so the processor tore down its registration and
    its OTHER session with it, and was gone for five seconds. An ended id is
    now remembered (a bounded, short-lived tombstone on its entry) and
    answered 409 ``session_ended``: "carry on with the others".
    """

    @staticmethod
    def _post(app: ProcessorAPI, pid: str, sid: str) -> tuple[int, str]:
        status, reply = TestTheSink._post(
            app, pid, sid, encode_frame({"event": "volume_done", "id": "v1", "pages": 3})
        )
        return status, str(reply.get("code"))

    @pytest.mark.parametrize("how", ["kill", "events-body-ended", "runner-exit"])
    def test_every_way_a_session_ends_leaves_it_ended(self, tmp_path: Path, how: str) -> None:
        app, registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        other = _opened(entry, "s2", tmp_path)
        if how == "kill":  # a settings change, a benchmark's pre-empt, a cancel
            assert session.kill()
        elif how == "events-body-ended":
            session.end("the events stream closed")
        else:
            session.feed({"event": "exit", "returncode": 0}, b"")

        assert self._post(app, entry.processor_id, "s1") == (409, "session_ended")
        # Still registered, and its other session is still its own.
        assert registry.get(entry.processor_id) is entry
        assert not entry.dropped
        with entry.lock:
            assert entry.sessions.get("s2") is other
        assert other.is_alive()

    def test_an_events_body_opened_after_a_kill_is_told_it_ended(self, tmp_path: Path) -> None:
        """Over a real socket, the body arriving only AFTER the library's kill."""
        app, registry, entry = _connected()
        session = _opened(entry, "s1", tmp_path)
        other = _opened(entry, "s2", tmp_path)
        session.kill()
        with _serving(app) as port:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
            conn.request(
                "POST", f"/_processor/{entry.processor_id}/sessions/s1/events",
                body=encode_frame({"event": "ping"}),
            )
            response = conn.getresponse()
            reply = json.loads(response.read() or b"{}")
            conn.close()
        assert (response.status, reply.get("code")) == (409, "session_ended")
        assert registry.get(entry.processor_id) is entry and other.is_alive()

    def test_a_sid_it_never_had_is_still_unknown(self, tmp_path: Path) -> None:
        app, _registry, entry = _connected()
        _opened(entry, "s1", tmp_path).kill()
        assert self._post(app, entry.processor_id, "never") == (404, "unknown")

    def test_an_unknown_processor_is_still_unknown(self, tmp_path: Path) -> None:
        app, _registry, entry = _connected()
        _opened(entry, "s1", tmp_path).kill()
        assert self._post(app, "no-such-processor", "s1") == (404, "unknown")

    def test_another_processors_ended_sid_is_unknown_to_this_one(self, tmp_path: Path) -> None:
        app, registry, entry = _connected()
        _opened(entry, "s1", tmp_path).kill()
        other = registry.register(username="tower", name="other", host={},
                                  catalog=CATALOG, max_sessions=1)
        assert self._post(app, other.processor_id, "s1") == (404, "unknown")

    def test_the_tombstone_expires(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
        from mokuro_bunko.ocr.remote import registry as registry_module

        clock = [1000.0]
        monkeypatch.setattr(registry_module.time, "monotonic", lambda: clock[0])
        app, _registry, entry = _connected()
        _opened(entry, "s1", tmp_path).kill()
        clock[0] += registry_module.ENDED_SESSION_TTL_SECONDS - 1
        assert self._post(app, entry.processor_id, "s1") == (409, "session_ended")
        clock[0] += 2
        assert self._post(app, entry.processor_id, "s1") == (404, "unknown")

    def test_the_tombstones_are_bounded(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.remote.registry import ENDED_SESSIONS_KEPT

        app, _registry, entry = _connected()
        for index in range(ENDED_SESSIONS_KEPT + 5):
            _opened(entry, f"s{index}", tmp_path).kill()
        assert len(entry.ended_sessions) == ENDED_SESSIONS_KEPT
        assert self._post(app, entry.processor_id, "s0") == (404, "unknown"), "the oldest went"
        last = f"s{ENDED_SESSIONS_KEPT + 4}"
        assert self._post(app, entry.processor_id, last) == (409, "session_ended")

    def test_an_ended_benchmark_s_sample_is_ended_too(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.remote.session import RemoteBench

        registry = ProcessorRegistry()
        entry = registry.register(username="tower", name="tower", host={},
                                  catalog=CATALOG, max_sessions=1)
        app = ProcessorAPI(lambda e, s: [b""], registry, samples_dir=tmp_path)
        bench = RemoteBench(entry, bid="bench-g-2-1", spec={}, sample_url="x", pages=4)
        bench.start()
        bench.kill()
        environ = {
            "REQUEST_METHOD": "GET",
            "PATH_INFO": f"/_processor/{entry.processor_id}/bench/bench-g-2-1/sample",
            "wsgi.input": io.BytesIO(b""),
            "mokuro.role": "processor",
            "mokuro.username": "tower",
        }
        captured: list[str] = []
        reply = json.loads(b"".join(app(environ, lambda s, h: captured.append(s))) or b"{}")
        assert (int(captured[0].split()[0]), reply.get("code")) == (409, "session_ended")
        assert self._post(app, entry.processor_id, "bench-g-2-1") == (409, "session_ended")
