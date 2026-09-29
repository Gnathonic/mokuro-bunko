"""Who is connected, what they may be sent, and what a refusal looks like."""

from __future__ import annotations

import io
import json
import queue
import sys
import threading
from typing import Any

from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
from mokuro_bunko.ocr.remote.protocol import PROTOCOL_VERSION
from mokuro_bunko.ocr.remote.registry import (
    FAILED_LOGIN_MEMORY,
    MAX_ENTRIES_PER_ACCOUNT,
    MAX_FAILED_LOGIN_USERNAME,
    MAX_PROCESSOR_NAME,
    MAX_SESSIONS_PER_PROCESSOR,
    STALE_REGISTRATION_SECONDS,
    ProcessorRegistry,
)

CATALOG: dict[str, Any] = {
    "engines": ["mokuro", "hayai-nova"],
    "detectors": ["ppocr-manga", "ctd"],
    "devices": [{"id": "gpu:0", "label": "GPU 0 — RTX 4090 (24 GB)"}],
    "serves_mokuro": True,
}
HOST: dict[str, Any] = {"cpu": "Ryzen 9 (16 cores)", "gpu": "RTX 4090", "backend": "cuda"}


def _call(
    app: Any,
    method: str,
    path: str,
    body: Any = None,
    *,
    role: str = "processor",
    username: str = "tower",
) -> tuple[int, dict[str, Any]]:
    raw = json.dumps(body).encode() if body is not None else b""
    environ = {
        "REQUEST_METHOD": method,
        "PATH_INFO": path,
        "CONTENT_LENGTH": str(len(raw)),
        "wsgi.input": io.BytesIO(raw),
        "mokuro.role": role,
        "mokuro.username": username,
    }
    captured: list[str] = []
    chunks = b"".join(app(environ, lambda status, headers: captured.append(status)))
    return int(captured[0].split()[0]), json.loads(chunks or b"{}")


def _api() -> tuple[ProcessorAPI, ProcessorRegistry]:
    registry = ProcessorRegistry()
    return ProcessorAPI(lambda e, s: [b"passthrough"], registry), registry


class TestRegistration:
    def test_a_registration_names_the_channels_and_mints_an_id(self) -> None:
        app, registry = _api()
        status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {
                "protocol": PROTOCOL_VERSION,
                "name": "tower",
                "host": HOST,
                "catalog": CATALOG,
                "max_sessions": 1,
            },
        )
        assert status == 200
        assert body["protocol"] == PROTOCOL_VERSION
        pid = body["processor_id"]
        assert body["session_stream"] == f"/_processor/{pid}/stream"
        assert body["events"] == f"/_processor/{pid}/sessions/{{sid}}/events"
        assert body["archives"] == "/mokuro-reader/"
        entry = registry.get(pid)
        assert entry is not None
        assert entry.name == "tower"
        assert entry.username == "tower"
        assert entry.catalog["engines"] == ["mokuro", "hayai-nova"]

    def test_an_unknown_protocol_is_refused_with_what_we_speak(self) -> None:
        app, registry = _api()
        status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {"protocol": 99, "name": "tower", "host": HOST, "catalog": CATALOG},
        )
        assert status == 400
        assert body["protocols"] == [PROTOCOL_VERSION]
        assert registry.entries() == []

    def test_a_protocol_refusal_names_this_library_s_release(self) -> None:
        """`processor setup` probes with a protocol no library speaks: the
        refusal registers nothing and says which release answered, so a
        mismatch can name the side to update."""
        from mokuro_bunko import __version__

        app, registry = _api()
        status, body = _call(app, "POST", "/_processor/register", {"protocol": 0})
        assert status == 400
        assert body["version"] == __version__
        assert registry.entries() == []

    def test_a_protocol_1_processor_is_refused_by_name(self) -> None:
        """The road that filed false "archive incomplete" records is refused
        outright: one loud line in its log, not a failure per volume."""
        app, registry = _api()
        status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {"protocol": 1, "name": "tower", "host": HOST, "catalog": CATALOG},
        )
        assert status == 400
        assert body["protocols"] == [2]
        assert "protocol 2" in body["error"]
        assert registry.entries() == []

    def test_a_non_processor_role_never_reaches_the_registry(self) -> None:
        app, registry = _api()
        status, _body = _call(
            app,
            "POST",
            "/_processor/register",
            {"protocol": PROTOCOL_VERSION, "name": "tower", "host": HOST, "catalog": CATALOG},
            role="editor",
        )
        assert status == 403
        assert registry.entries() == []

    def test_a_second_registration_from_the_same_account_replaces_the_first(self) -> None:
        app, registry = _api()
        _s1, first = _call(
            app,
            "POST",
            "/_processor/register",
            {"protocol": PROTOCOL_VERSION, "name": "tower", "host": HOST, "catalog": CATALOG},
        )
        _s2, second = _call(
            app,
            "POST",
            "/_processor/register",
            {"protocol": PROTOCOL_VERSION, "name": "tower", "host": HOST, "catalog": CATALOG},
        )
        assert first["processor_id"] != second["processor_id"]
        assert [e.processor_id for e in registry.entries()] == [second["processor_id"]]

    def test_an_empty_catalog_registers_and_reads_as_installing(self) -> None:
        app, registry = _api()
        _status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {
                "protocol": PROTOCOL_VERSION,
                "name": "tower",
                "host": HOST,
                "catalog": {"engines": [], "detectors": [], "devices": []},
            },
        )
        entry = registry.get(body["processor_id"])
        assert entry is not None and entry.installing is True

    def test_a_max_sessions_that_is_not_a_number_is_a_refusal_not_a_crash(self) -> None:
        app, registry = _api()
        status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {
                "protocol": PROTOCOL_VERSION,
                "name": "tower",
                "host": HOST,
                "catalog": CATALOG,
                "max_sessions": "lots",
            },
        )
        assert status == 400
        assert "max_sessions" in body["error"]
        assert registry.entries() == []

    def test_a_name_that_is_not_text_is_refused(self) -> None:
        """Task 12 keys a processor's profile by the stored name."""
        app, registry = _api()
        status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {"protocol": PROTOCOL_VERSION, "name": {"a": 1}, "host": HOST, "catalog": CATALOG},
        )
        assert status == 400
        assert "name" in body["error"]
        assert registry.entries() == []

    def test_a_very_long_name_is_truncated_not_refused(self) -> None:
        app, registry = _api()
        _status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {"protocol": PROTOCOL_VERSION, "name": "b" * 500, "host": HOST, "catalog": CATALOG},
        )
        entry = registry.get(body["processor_id"])
        assert entry is not None
        assert entry.name == "b" * MAX_PROCESSOR_NAME

    def test_a_catalog_of_the_wrong_shape_is_refused(self) -> None:
        app, registry = _api()
        status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {"protocol": PROTOCOL_VERSION, "name": "tower", "host": HOST, "catalog": "everything"},
        )
        assert status == 400
        assert "catalog" in body["error"]
        status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {
                "protocol": PROTOCOL_VERSION,
                "name": "tower",
                "host": HOST,
                "catalog": {"engines": "mokuro"},
            },
        )
        assert status == 400
        assert "catalog.engines" in body["error"]
        assert registry.entries() == []

    def test_a_processor_cannot_claim_more_sessions_than_the_cap(self) -> None:
        app, registry = _api()
        _status, body = _call(
            app,
            "POST",
            "/_processor/register",
            {
                "protocol": PROTOCOL_VERSION,
                "name": "tower",
                "host": HOST,
                "catalog": CATALOG,
                "max_sessions": 9999,
            },
        )
        entry = registry.get(body["processor_id"])
        assert entry is not None
        assert entry.max_sessions == MAX_SESSIONS_PER_PROCESSOR

    def test_the_local_hardwares_names_are_reserved(self) -> None:
        registry = ProcessorRegistry(local_name="this server")
        app = ProcessorAPI(lambda e, s: [b"passthrough"], registry)
        for taken in ("local", "This Server", "  this server  "):
            status, body = _call(
                app,
                "POST",
                "/_processor/register",
                {"protocol": PROTOCOL_VERSION, "name": taken, "host": HOST, "catalog": CATALOG},
            )
            assert status == 400, taken
            assert "reserved" in body["error"], taken
        assert [e.processor_id for e in registry.entries()] == ["local"]

    def test_a_body_with_no_length_says_so(self) -> None:
        app, _registry = _api()
        for length, expected in (
            (None, "Content-Length is required"),
            ("", "Content-Length is required"),
            ("banana", "invalid Content-Length"),
            ("0", "a registration needs a body"),
        ):
            environ: dict[str, Any] = {
                "REQUEST_METHOD": "POST",
                "PATH_INFO": "/_processor/register",
                "wsgi.input": io.BytesIO(b""),
                "mokuro.role": "processor",
                "mokuro.username": "tower",
            }
            if length is not None:
                environ["CONTENT_LENGTH"] = length
            captured: list[str] = []
            chunks = b"".join(
                app(environ, lambda s, h, seen=captured: seen.append(s))
            )
            assert captured[0].startswith("400"), length
            assert json.loads(chunks)["error"] == expected, length

    def test_a_processor_belongs_to_the_account_that_registered_it(self) -> None:
        app, registry = _api()
        entry = registry.register(
            username="tower", name="tower", host=HOST, catalog=CATALOG, max_sessions=1
        )
        assert app._owned(entry.processor_id, "tower") is entry
        assert app._owned(entry.processor_id, "someone-else") is None
        assert app._owned("no-such-processor", "tower") is None

    def test_the_local_entry_is_nobodys(self) -> None:
        registry = ProcessorRegistry(local_name="this server")
        app = ProcessorAPI(lambda e, s: [b"passthrough"], registry)
        assert app._owned("local", "") is None
        assert app._owned("local", "tower") is None

    def test_a_path_that_is_not_ours_falls_through(self) -> None:
        app, _registry = _api()
        environ = {
            "REQUEST_METHOD": "GET",
            "PATH_INFO": "/mokuro-reader/",
            "mokuro.role": "processor",
            "mokuro.username": "tower",
        }
        assert b"".join(app(environ, lambda s, h: None)) == b"passthrough"


class TestTheRegistry:
    @staticmethod
    def _entry(registry: ProcessorRegistry, name: str = "tower") -> Any:
        return registry.register(
            username=name, name=name, host=HOST, catalog=CATALOG, max_sessions=2
        )

    def test_an_entry_labels_itself_by_host(self) -> None:
        registry = ProcessorRegistry()
        assert self._entry(registry).label() == "tower (RTX 4090)"
        plain = registry.register(
            username="box", name="box", host={"cpu": "x"}, catalog=CATALOG, max_sessions=1
        )
        assert plain.label() == "box"

    def test_an_op_queues_for_the_stream_to_pick_up(self) -> None:
        registry = ProcessorRegistry()
        entry = self._entry(registry)
        assert entry.send({"op": "heartbeat"}) is True
        assert entry.ops.get_nowait() == {"op": "heartbeat"}

    def test_an_op_this_protocol_does_not_define_is_refused(self) -> None:
        registry = ProcessorRegistry()
        entry = self._entry(registry)
        assert entry.send({"op": "reboot"}) is False
        assert entry.ops.empty()

    def test_dropping_tells_the_listener_and_ends_the_stream(self) -> None:
        seen: list[tuple[str, str]] = []
        registry = ProcessorRegistry(on_drop=lambda e, r: seen.append((e.name, r)))
        entry = self._entry(registry)
        assert registry.drop(entry.processor_id, "stream closed") is entry
        assert seen == [("tower", "stream closed")]
        assert entry.ops.get_nowait() is None, "the stream's sentinel"
        assert registry.get(entry.processor_id) is None
        assert entry.send({"op": "heartbeat"}) is False

    def test_drop_all_ends_every_stream_and_keeps_the_local_entry(self) -> None:
        """What the library server does on its way down. Found on real
        hardware: a connected processor reads its heartbeats forever, so
        its stream's worker thread never ends and Ctrl-C never exits."""
        seen: list[tuple[str, str]] = []
        registry = ProcessorRegistry(
            local_name="this server", on_drop=lambda e, r: seen.append((e.name, r))
        )
        first, second = self._entry(registry, "tower"), self._entry(registry, "attic")

        assert registry.drop_all("the library server is shutting down") == 2

        assert sorted(seen) == [
            ("attic", "the library server is shutting down"),
            ("tower", "the library server is shutting down"),
        ]
        assert first.ops.get_nowait() is None and second.ops.get_nowait() is None
        assert [e.name for e in registry.entries()] == ["this server"]
        assert registry.drop_all("again") == 0

    def test_a_dropped_processor_is_remembered_for_the_hold_message(self) -> None:
        registry = ProcessorRegistry()
        entry = self._entry(registry)
        registry.drop(entry.processor_id, "stream closed")
        last = registry.last_disconnect()
        assert last is not None and last[0] == "tower"

    def test_a_refused_login_is_kept_with_its_username(self) -> None:
        registry = ProcessorRegistry()
        registry.record_failed_login("tower", "Invalid credentials")
        assert [f.username for f in registry.failures()] == ["tower"]

    def test_a_fabricated_username_cannot_grow_the_memory(self) -> None:
        """`attempted_username` is whatever was typed before the `:`.

        It is untrusted text of unbounded length and a caller can invent a
        new one per attempt, so neither the record nor the roll of records
        may grow with it.
        """
        registry = ProcessorRegistry()
        registry.record_failed_login("x" * 5000, "Invalid credentials")
        assert len(registry.failures()[0].username) <= MAX_FAILED_LOGIN_USERNAME
        for i in range(FAILED_LOGIN_MEMORY * 3):
            registry.record_failed_login(f"made-up-{i}", "Invalid credentials")
        assert len(registry.failures()) == FAILED_LOGIN_MEMORY

    def test_the_newest_refusal_is_first(self) -> None:
        registry = ProcessorRegistry()
        registry.record_failed_login("first", "Invalid credentials")
        registry.record_failed_login("second", "Invalid credentials")
        assert [f.username for f in registry.failures()] == ["second", "first"]

    def test_the_local_hardware_is_an_entry_when_it_is_named(self) -> None:
        registry = ProcessorRegistry(local_name="this server")
        local = registry.get("local")
        assert local is not None and local.local is True
        assert [e.processor_id for e in registry.entries()] == ["local"]
        assert local.send({"op": "heartbeat"}) is False

    def test_one_account_cannot_fill_the_registry(self) -> None:
        """Only the exact name was ever replaced, so distinct names piled up.

        An authenticated processor account can register as often as it
        likes; `drop` runs when a stream CLOSES, and a registration that
        never opened one never closes one.
        """
        registry = ProcessorRegistry()
        ids = [
            registry.register(
                username="tower",
                name=f"name-{i}",
                host=HOST,
                catalog=CATALOG,
                max_sessions=1,
            ).processor_id
            for i in range(MAX_ENTRIES_PER_ACCOUNT + 3)
        ]
        kept = {e.processor_id for e in registry.entries()}
        assert len(kept) == MAX_ENTRIES_PER_ACCOUNT
        assert kept == set(ids[-MAX_ENTRIES_PER_ACCOUNT:]), "the newest always win"
        assert registry.get(ids[0]) is None, "the oldest made room"

    def test_eviction_takes_a_registration_with_no_stream_first(self) -> None:
        """The cap must not cut off the processor that is actually working.

        Victims were the oldest entries outright -- and the oldest is
        typically the one that has been streaming longest, so a burst of
        registrations from the same account would disconnect a running
        processor (returning its claims) in favour of registrations that
        never opened a stream at all.
        """
        registry = ProcessorRegistry()
        entries = [
            registry.register(
                username="tower",
                name=f"name-{i}",
                host=HOST,
                catalog=CATALOG,
                max_sessions=1,
            )
            for i in range(MAX_ENTRIES_PER_ACCOUNT)
        ]
        working = entries[0]  # the oldest of them, and the only one streaming
        with working.lock:
            working.stream_open = True
        registry.register(
            username="tower", name="newcomer", host=HOST, catalog=CATALOG, max_sessions=1
        )
        kept = {e.processor_id for e in registry.entries()}
        assert working.processor_id in kept, "a live stream is the last to go"
        assert entries[1].processor_id not in kept, "the oldest idle one made room"

    def test_another_account_is_not_crowded_out(self) -> None:
        registry = ProcessorRegistry()
        mine = registry.register(
            username="quiet", name="quiet", host=HOST, catalog=CATALOG, max_sessions=1
        )
        for i in range(MAX_ENTRIES_PER_ACCOUNT + 3):
            registry.register(
                username="loud", name=f"loud-{i}", host=HOST, catalog=CATALOG,
                max_sessions=1,
            )
        assert registry.get(mine.processor_id) is mine

    def test_a_registration_that_never_opened_a_stream_goes_stale(self) -> None:
        registry = ProcessorRegistry()
        forgotten = self._entry(registry, "tower")
        forgotten.name = "an-old-name"
        forgotten.last_seen -= STALE_REGISTRATION_SECONDS + 1
        live = registry.register(
            username="tower", name="still-here", host=HOST, catalog=CATALOG, max_sessions=1
        )
        live.stream_open = True
        live.last_seen -= STALE_REGISTRATION_SECONDS + 1  # old, but streaming
        registry.register(
            username="tower", name="newcomer", host=HOST, catalog=CATALOG, max_sessions=1
        )
        kept = [e.name for e in registry.entries()]
        assert "an-old-name" not in kept, "never opened a stream and long silent"
        assert "still-here" in kept, "a live stream is not stale, however quiet"
        assert forgotten.send({"op": "heartbeat"}) is False
        assert forgotten.ops.get_nowait() is None, "the reap ends its stream too"

    def test_an_op_never_lands_behind_the_sentinel(self) -> None:
        """`send` promises False once the entry is gone -- under a race too.

        The window is the few bytecodes between reading `dropped` and
        putting the op, so this runs many trials with thread switching made
        as likely as the interpreter allows; a check and a put that are not
        one critical section lose an op within a few hundred trials.
        """
        was = sys.getswitchinterval()
        sys.setswitchinterval(1e-6)
        try:
            for _ in range(200):
                registry = ProcessorRegistry()
                entry = self._entry(registry)
                stop = threading.Event()
                started = threading.Event()

                def sender(entry: Any = entry, stop: Any = stop, started: Any = started) -> None:
                    started.set()
                    while not stop.is_set():
                        entry.send({"op": "heartbeat"})

                thread = threading.Thread(target=sender)
                thread.start()
                started.wait()
                try:
                    registry.drop(entry.processor_id, "stream closed")
                finally:
                    stop.set()
                    thread.join()
                drained: list[Any] = []
                while True:
                    try:
                        drained.append(entry.ops.get_nowait())
                    except queue.Empty:
                        break
                assert drained.count(None) == 1, "one sentinel"
                assert drained[-1] is None, "and nothing queued behind it"
                assert entry.send({"op": "heartbeat"}) is False
        finally:
            sys.setswitchinterval(was)

    def test_reading_an_entry_while_its_sessions_change_is_safe(self) -> None:
        """Tasks 5/6 mutate `sessions` from the events thread."""
        registry = ProcessorRegistry()
        entry = self._entry(registry)
        errors: list[BaseException] = []
        stop = threading.Event()

        def churn() -> None:
            # Grow and then clear: it is the dict's SIZE changing that a
            # live iteration trips over. Unguarded, this raises
            # "dictionary changed size during iteration" on the reader
            # within a few hundred reads.
            i = 0
            try:
                while not stop.is_set():
                    with entry.lock:
                        for k in range(8):
                            entry.sessions[f"s{i}-{k}"] = object()
                        entry.sessions.clear()
                    i += 1
            except BaseException as e:  # pragma: no cover - the point of the test
                errors.append(e)

        was = sys.getswitchinterval()
        sys.setswitchinterval(1e-6)
        thread = threading.Thread(target=churn)
        thread.start()
        try:
            for _ in range(5_000):
                entry.to_dict()
                assert entry.open_sessions >= 0
        except BaseException as e:  # pragma: no cover - the point of the test
            errors.append(e)
        finally:
            stop.set()
            thread.join()
            sys.setswitchinterval(was)
        assert errors == []

    def test_the_sessions_count_is_sessions_not_benchmarks(self) -> None:
        """The Processors card shows this as "Sessions"; a bench is not one."""
        registry = ProcessorRegistry()
        entry = self._entry(registry)
        # Under `entry.lock` even here, where nothing else is running: this
        # is the example Tasks 5/6 copy for writing `sessions` from the
        # stream and events threads.
        with entry.lock:
            entry.sessions["s1"] = object()
            entry.sessions["bench-g-2"] = object()
        assert entry.open_sessions == 1
        assert entry.to_dict()["sessions"] == 1
