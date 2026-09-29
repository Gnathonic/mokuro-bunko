"""A processor account that is revoked is cut off while it is connected (I2).

Authentication happens once per request, and a processor's assignment stream
and events bodies are single requests that live for hours. So the library
asks about the account again -- at heartbeat pace on the stream, and on every
events body -- and the admin panel's own edits drop the account's processors
at once. On the processor, an archive it can no longer read for a reason that
is not the archive's is a TRANSPORT failure: no volume is failed for it, the
processor steps away (its claims go back unrecorded) and registers again,
which a revoked account cannot.
"""

from __future__ import annotations

import io
import json
import threading
import time
from collections.abc import Callable, Iterator
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.config import AdminConfig, Config
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.remote import library_api
from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
from mokuro_bunko.ocr.remote.protocol import PROTOCOL_VERSION
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.processor.archives import TransferFault
from mokuro_bunko.processor.client import (
    ACTION_REREGISTER,
    EventSink,
    LibraryClient,
    LibraryTransportError,
)
from mokuro_bunko.processor.config import (
    LibrarySettings,
    ProcessorConfig,
    ProcessorOcr,
    ProcessorSettings,
)
from tests.unit import test_processor_bridge as bridge_tests
from tests.unit.test_processor_bridge import FAKE_RUNNER, _cbz, _stack

PASSWORD = "a-long-enough-password"


# --- the account's fingerprint ----------------------------------------------


class TestTheAccountStamp:
    def test_it_changes_with_the_password_and_vanishes_with_the_role(
        self, tmp_path: Path
    ) -> None:
        db = Database(tmp_path / "mokuro.db")
        db.create_user("tower", PASSWORD, "processor")
        first = db.processor_account_stamp("tower")
        assert isinstance(first, str) and first
        assert db.processor_account_stamp("tower") == first, "stable while unchanged"
        db.update_user_password("tower", PASSWORD + "-new")
        second = db.processor_account_stamp("tower")
        assert second is not None and second != first
        db.update_user_role("tower", "registered")
        assert db.processor_account_stamp("tower") is None

    def test_a_disabled_deleted_or_unknown_account_has_none(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "mokuro.db")
        db.create_user("tower", PASSWORD, "processor")
        db.create_user("box", PASSWORD, "processor")
        db.disable_user("tower")
        db.delete_user("box")
        assert db.processor_account_stamp("tower") is None
        assert db.processor_account_stamp("box") is None
        assert db.processor_account_stamp("nobody") is None

    def test_it_never_carries_the_hash_itself(self, tmp_path: Path) -> None:
        db = Database(tmp_path / "mokuro.db")
        db.create_user("tower", PASSWORD, "processor")
        stamp = db.processor_account_stamp("tower")
        assert stamp is not None and "$2" not in stamp and len(stamp) == 32


# --- the library side ----------------------------------------------------------


class _Account:
    """A stand-in for the users table: what the stamp is right now."""

    def __init__(self) -> None:
        self.stamp: str | None = "v1"

    def __call__(self, username: str) -> str | None:
        return self.stamp if username == "tower" else None


def _api(account: _Account) -> tuple[ProcessorAPI, ProcessorRegistry]:
    registry = ProcessorRegistry()
    api = ProcessorAPI(lambda e, s: [], registry, account_check=account)
    return api, registry


def _register(api: ProcessorAPI) -> dict[str, Any]:
    body = json.dumps({"protocol": PROTOCOL_VERSION, "name": "tower", "catalog": {}}).encode()
    environ = {
        "REQUEST_METHOD": "POST", "PATH_INFO": "/_processor/register",
        "CONTENT_LENGTH": str(len(body)), "wsgi.input": io.BytesIO(body),
        "mokuro.role": "processor", "mokuro.username": "tower",
    }
    out: list[str] = []
    raw = b"".join(api(environ, lambda status, headers: out.append(status)))
    assert out[0].startswith("200"), raw
    return dict(json.loads(raw))


class TestTheLibraryCutsARevokedAccountOff:
    def test_the_stream_drops_it_at_the_next_heartbeat(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(library_api, "HEARTBEAT_SECONDS", 0.05)
        account = _Account()
        api, registry = _api(account)
        reply = _register(api)
        entry = registry.get(reply["processor_id"])
        assert entry is not None and entry.account_stamp == "v1"
        dropped: list[str] = []
        registry.on_drop = lambda gone, reason: dropped.append(reason)
        ops = api._ops(entry)
        assert b"heartbeat" in next(ops)
        assert b"heartbeat" in next(ops)
        account.stamp = None  # disabled, deleted or given another role
        with pytest.raises(StopIteration):
            for _ in range(10):
                next(ops)
        assert entry.dropped
        assert registry.get(entry.processor_id) is None
        assert dropped and "no longer an active processor" in dropped[0]

    def test_a_new_password_counts_as_a_revocation(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(library_api, "HEARTBEAT_SECONDS", 0.05)
        account = _Account()
        api, registry = _api(account)
        entry = registry.get(_register(api)["processor_id"])
        assert entry is not None
        ops = api._ops(entry)
        next(ops)
        account.stamp = "v2"
        with pytest.raises(StopIteration):
            for _ in range(10):
                next(ops)
        assert entry.dropped

    def test_a_busy_stream_is_checked_too(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """A stream that always has an op to send never goes idle, and the
        check must not wait for an idle moment that never comes."""
        monkeypatch.setattr(library_api, "HEARTBEAT_SECONDS", 0.05)
        account = _Account()
        api, registry = _api(account)
        entry = registry.get(_register(api)["processor_id"])
        assert entry is not None
        ops = api._ops(entry)
        entry.send({"op": "cancel", "sid": "s", "claim": "c"})
        next(ops)
        account.stamp = None
        stopped = False
        for _ in range(50):
            entry.send({"op": "cancel", "sid": "s", "claim": "c"})
            time.sleep(0.01)
            try:
                next(ops)
            except StopIteration:
                stopped = True
                break
        assert stopped and entry.dropped

    def test_an_events_body_for_a_revoked_account_is_refused_and_drops_it(self) -> None:
        account = _Account()
        api, registry = _api(account)
        entry = registry.get(_register(api)["processor_id"])
        assert entry is not None
        account.stamp = None
        environ = {
            "REQUEST_METHOD": "POST",
            "PATH_INFO": f"/_processor/{entry.processor_id}/sessions/s1/events",
            "wsgi.input": io.BytesIO(b""),
            "mokuro.role": "processor", "mokuro.username": "tower",
        }
        out: list[str] = []
        raw = b"".join(api(environ, lambda status, headers: out.append(status)))
        assert out[0].startswith("403")
        assert json.loads(raw)["code"] == "dropped"
        assert entry.dropped


def _nothing(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
    start_response("404 Not Found", [("Content-Type", "text/plain")])
    return [b""]


class _Control:
    def __init__(self, registry: ProcessorRegistry) -> None:
        self.remote = registry
        self.worker = None
        self.bench: Any = None
        self.bench_factory: Any = None


@pytest.fixture
def admin(tmp_path: Path) -> Iterator[tuple[AdminAPI, Database, ProcessorRegistry]]:
    db = Database(tmp_path / "mokuro.db")
    db.create_user("tower", PASSWORD, "processor")
    registry = ProcessorRegistry()
    config = Config()
    config.storage.base_path = tmp_path
    app = AdminAPI(_nothing, db, AdminConfig(enabled=True, path="/_admin"),
                   full_config=config, ocr_control=_Control(registry))  # type: ignore[arg-type]
    yield app, db, registry


def _admin(app: AdminAPI, method: str, path: str,
           body: dict[str, Any] | None = None) -> int:
    content = json.dumps(body).encode() if body is not None else b""
    environ = {
        "REQUEST_METHOD": method, "PATH_INFO": path, "QUERY_STRING": "",
        "CONTENT_LENGTH": str(len(content)), "CONTENT_TYPE": "application/json",
        "wsgi.input": io.BytesIO(content),
        "mokuro.role": "admin", "mokuro.username": "admin",
    }
    out: list[str] = []
    b"".join(app(environ, lambda status, headers: out.append(status)))
    return int(out[0].split()[0])


def _connected(registry: ProcessorRegistry) -> Any:
    entry = registry.register(username="tower", name="tower", host={}, catalog={},
                              max_sessions=1)
    entry.stream_open = True
    return entry


class TestTheAdminPanelsEditsCutItOffAtOnce:
    @pytest.mark.parametrize(
        ("method", "path", "body"),
        [
            ("POST", "/_admin/api/users/tower/disable", None),
            ("DELETE", "/_admin/api/users/tower", None),
            ("PUT", "/_admin/api/users/tower/role", {"role": "registered"}),
        ],
    )
    def test_an_edit_that_revokes_the_account_drops_its_processor(
        self, admin: tuple[AdminAPI, Database, ProcessorRegistry],
        method: str, path: str, body: dict[str, Any] | None,
    ) -> None:
        app, _db, registry = admin
        entry = _connected(registry)
        assert _admin(app, method, path, body) == 200
        assert entry.dropped

    def test_a_notes_edit_leaves_it_connected(
        self, admin: tuple[AdminAPI, Database, ProcessorRegistry]
    ) -> None:
        app, _db, registry = admin
        entry = _connected(registry)
        assert _admin(app, "PUT", "/_admin/api/users/tower/notes", {"notes": "4090"}) == 200
        assert not entry.dropped


# --- the processor side --------------------------------------------------------


def _serve(app: Callable[..., Any]) -> tuple[Any, int]:
    from cheroot.wsgi import Server as WSGIServer

    server = WSGIServer(("127.0.0.1", 0), app, numthreads=4)
    server.prepare()
    threading.Thread(target=server.serve, daemon=True).start()
    return server, int(server.bind_addr[1])


def _client(port: int, tmp_path: Path) -> LibraryClient:
    return LibraryClient(
        ProcessorConfig(
            library=LibrarySettings(url=f"http://127.0.0.1:{port}", username="tower",
                                    password=PASSWORD),
            processor=ProcessorSettings(name="tower", storage=tmp_path / "state"),
            ocr=ProcessorOcr(),
        )
    )


def _answering(status: str, headers: list[tuple[str, str]]) -> Callable[..., Any]:
    def app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
        start_response(status, headers)
        return [b""] if environ.get("REQUEST_METHOD") != "HEAD" else []

    return app


class TestWhatAnArchiveReadFailureMeans:
    """What the fetcher makes of each answer the library can give a download."""

    @staticmethod
    def _fetch(port: int, tmp_path: Path) -> Any:
        from mokuro_bunko.processor.archives import ArchiveFetcher, ArchiveSpool, FetchTiming

        shm = tmp_path / "shm"
        shm.mkdir(exist_ok=True)
        fetcher = ArchiveFetcher(
            _client(port, tmp_path),
            ArchiveSpool(tmp_path / "state", memory_dir=shm, headroom=lambda: None),
            timing=FetchTiming(read_timeout=1.0, retry_delays=(0.05,), stall_seconds=0.5,
                               progress_after=0.0),
        )
        return fetcher.fetch(
            "/mokuro-reader/Alpha/Volume 1.cbz", size=None, cancel=threading.Event()
        )

    @pytest.mark.parametrize(
        "status", ["401 Unauthorized", "403 Forbidden", "407 Proxy Authentication Required"]
    )
    def test_the_account_s_refusal_is_the_transport_s(
        self, tmp_path: Path, status: str
    ) -> None:
        server, port = _serve(_answering(status, [("Content-Length", "0")]))
        try:
            with pytest.raises(LibraryTransportError):
                self._fetch(port, tmp_path)
        finally:
            server.stop()

    def test_a_missing_archive_is_given_back_for_the_library_to_judge(
        self, tmp_path: Path
    ) -> None:
        server, port = _serve(_answering("404 Not Found", [("Content-Length", "0")]))
        try:
            with pytest.raises(TransferFault) as excinfo:
                self._fetch(port, tmp_path)
            assert excinfo.value.kind == "missing"
        finally:
            server.stop()

    def test_a_server_that_keeps_failing_is_given_back_as_stalled(
        self, tmp_path: Path
    ) -> None:
        server, port = _serve(_answering("503 Service Unavailable", [("Content-Length", "0")]))
        try:
            with pytest.raises(TransferFault) as excinfo:
                self._fetch(port, tmp_path)
            assert excinfo.value.kind == "stalled"
        finally:
            server.stop()

    def test_a_library_that_is_not_there_is_given_back_as_stalled(
        self, tmp_path: Path
    ) -> None:
        server, port = _serve(_answering("200 OK", []))
        server.stop()
        with pytest.raises(TransferFault) as excinfo:
            self._fetch(port, tmp_path)
        assert excinfo.value.kind == "stalled"


class TestAnAuthRefusalOfAnEventsBody:
    def test_a_401_with_no_code_asks_for_a_fresh_registration(self) -> None:
        """The AUTH layer answers before the events sink: no `code` at all.
        A fresh registration is what tells a revoked account from a blip."""

        class _Connection:
            sock = None

            def close(self) -> None:
                return None

        sink = EventSink(_Connection(), "/x", ping=False)  # type: ignore[arg-type]
        sink.status = 401
        assert sink.action == ACTION_REREGISTER
        sink.status = 403
        assert sink.action == ACTION_REREGISTER
        sink.status = 500
        assert sink.action == ""


class TestTheProcessorStepsAwayInsteadOfFailingVolumes:
    def test_a_revoked_archive_read_fails_no_volume_and_ends_the_stream(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        refuse = threading.Event()
        real = bridge_tests._archives_app
        monkeypatch.setattr(
            bridge_tests, "_archives_app",
            lambda root, faults=None: _refusing_archives_with(real, root, refuse),
        )
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"pages": 2, "page_delay": 0.05}) as stack:
            session = stack.open_session("s1")
            archive = stack.library_path / "Alpha" / "Volume 1.cbz"
            _cbz(archive, ["001.jpg", "002.jpg"])
            _events_until_ready(session)
            refuse.set()
            stack.volume(session, "c1", archive)
            assert _wait(lambda: stack.entry.dropped, timeout=30), (
                "the processor never stepped away"
            )
            seen = _drain(session)
            kinds = [event.get("event") for event in seen]
            assert "volume_failed" not in kinds, seen
            assert "volume_started" not in kinds, seen
            assert "fatal" not in kinds, seen


def _refusing_archives_with(
    real: Callable[[Path], Callable[..., Any]], root: Path, refuse: threading.Event
) -> Callable[..., Any]:
    """The library's archives -- until the account behind them is revoked."""
    serve = real(root)

    def app(environ: dict[str, Any], start_response: Callable[..., Any]) -> list[bytes]:
        if refuse.is_set():
            start_response("401 Unauthorized", [("Content-Length", "0")])
            return []
        return serve(environ, start_response)

    return app


def _events_until_ready(session: Any, timeout: float = 30.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        event = session.poll_event(timeout=0.5)
        if event is not None and event.get("event") == "ready":
            return
    raise AssertionError("the runner never became ready")


def _drain(session: Any) -> list[dict[str, Any]]:
    seen: list[dict[str, Any]] = []
    while True:
        event = session.poll_event(timeout=0.5)
        if event is None:
            return seen
        seen.append(event)


def _wait(predicate: Callable[[], bool], timeout: float = 10.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.05)
    return predicate()
