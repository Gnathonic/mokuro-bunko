"""The processor channels, reached the way a processor really reaches them.

Every other test of `ProcessorAPI` hands it a synthetic environ. This one
goes through the whole stack `create_app` builds -- real `AuthMiddleware`,
real database, real Basic auth -- because what is most likely to break is
what only the assembled stack decides: that the mount survives the admin
panel being switched OFF, that a refused login reaches the registry, and
that the registry is the one the rest of the server holds.
"""

from __future__ import annotations

import http.client
import json
import threading
import time
from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any, NamedTuple

import pytest

from mokuro_bunko.config import AdminConfig, Config, StorageConfig
from mokuro_bunko.database import Database
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.remote import library_api
from mokuro_bunko.ocr.remote.protocol import PROTOCOL_VERSION
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.server import create_app
from tests.integration.test_webdav_ops import WSGITestClient, make_auth_header

PASSWORD = "pass1234"


class Stack(NamedTuple):
    """A running app and the registry `create_app` gave the OCR control."""

    client: WSGITestClient
    registry: ProcessorRegistry

    @property
    def app(self) -> Callable[..., Iterable[bytes]]:
        """The composed WSGI app itself, for serving over a real socket."""
        return self.client.app  # type: ignore[no-any-return]


@pytest.fixture
def stack(temp_dir: Path) -> Stack:
    """A live app with its admin panel OFF.

    `admin.enabled = False` is the point: `AdminAPI` is mounted inside that
    `if` and `ProcessorAPI` must not be.
    """
    storage = temp_dir / "storage"
    (storage / "library").mkdir(parents=True)
    (storage / "inbox").mkdir()
    (storage / "users").mkdir()
    database = Database(storage / "mokuro.db")
    database.create_user("tower", PASSWORD, "processor")
    database.create_user("reader", PASSWORD, "registered")
    config = Config(
        storage=StorageConfig(base_path=storage), admin=AdminConfig(enabled=False)
    )
    control = OcrControl()
    app = create_app(config, config_path=storage / "config.yaml", ocr_control=control)
    assert isinstance(control.remote, ProcessorRegistry), "create_app sets ocr_control.remote"
    return Stack(WSGITestClient(app), control.remote)


def _register(
    stack: Stack, username: str, password: str, body: dict[str, Any] | None = None
) -> tuple[int, dict[str, Any]]:
    payload = json.dumps(
        body
        if body is not None
        else {
            "protocol": PROTOCOL_VERSION,
            "name": "tower",
            "host": {"gpu": "RTX 4090", "backend": "cuda"},
            "catalog": {"engines": ["mokuro"], "detectors": ["ctd"], "devices": []},
            "max_sessions": 2,
        }
    ).encode()
    response = stack.client.request(
        "POST",
        "/_processor/register",
        headers={
            "Authorization": make_auth_header(username, password),
            "Content-Type": "application/json",
        },
        content=payload,
    )
    try:
        parsed = json.loads(response.content or b"{}")
    except ValueError:
        parsed = {"body": response.text}
    return response.status_code, parsed


class TestTheMount:
    def test_a_processor_registers_through_the_whole_stack(self, stack: Stack) -> None:
        status, body = _register(stack, "tower", PASSWORD)
        assert status == 200, body
        pid = body["processor_id"]
        assert body["protocol"] == PROTOCOL_VERSION
        assert body["session_stream"] == f"/_processor/{pid}/stream"
        assert body["events"] == f"/_processor/{pid}/sessions/{{sid}}/events"
        assert body["archives"] == "/mokuro-reader/"

        entry = stack.registry.get(pid)
        assert entry is not None, "the registry the rest of the server holds"
        assert (entry.username, entry.name) == ("tower", "tower")

    def test_a_refused_login_reaches_the_registry(self, stack: Stack) -> None:
        status, _body = _register(stack, "tower", "wrong-password")
        assert status == 401
        assert [f.username for f in stack.registry.failures()] == ["tower"]

    def test_an_ordinary_account_is_not_a_processor(self, stack: Stack) -> None:
        status, _body = _register(stack, "reader", PASSWORD)
        assert status == 403
        assert [e.processor_id for e in stack.registry.entries() if not e.local] == []
        assert stack.registry.failures() == [], "a wrong ROLE is not a failed login"


class TestTheStreamThroughTheStack:
    """The assignment stream is a LONG-LIVED response, which is unlike
    anything else this server returns.

    `tests/unit/test_remote_stream.py` proves cheroot streams the generator;
    this proves that the seventeen middlewares `create_app` wraps around it
    do not undo that. Any one of them that read the response into memory --
    to gzip it, to measure it, to log it -- would turn every op into
    something the processor only sees when the stream ends, and the unit
    test could not tell.
    """

    def test_ops_arrive_one_at_a_time_through_the_whole_stack(
        self, stack: Stack, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from cheroot.wsgi import Server as WSGIServer

        # Headers reach the client with the response's FIRST chunk, and on
        # an idle queue that is the first heartbeat -- so an unpatched 15 s
        # interval would be 15 s spent inside `getresponse()` before the
        # measurement even starts.
        monkeypatch.setattr(library_api, "HEARTBEAT_SECONDS", 0.05)

        status, body = _register(stack, "tower", PASSWORD)
        assert status == 200, body
        entry = stack.registry.get(body["processor_id"])
        assert entry is not None

        server = WSGIServer(("127.0.0.1", 0), stack.app, numthreads=4)
        server.prepare()
        port = server.bind_addr[1]
        threading.Thread(target=server.serve, daemon=True).start()
        try:
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
            connection.request(
                "GET",
                body["session_stream"],
                headers={"Authorization": make_auth_header("tower", PASSWORD)},
            )
            response = connection.getresponse()
            assert response.status == 200
            assert response.getheader("Content-Type") == "application/x-ndjson"
            assert response.getheader("X-Accel-Buffering") == "no"
            assert response.getheader("Transfer-Encoding") == "chunked"
            assert response.getheader("Content-Length") is None, "not a measured body"

            def push() -> None:
                for index in range(3):
                    time.sleep(0.3)
                    entry.send({"op": "cancel", "sid": "s1", "claim": f"v{index}"})
                time.sleep(0.3)
                stack.registry.drop(entry.processor_id, "test over")

            threading.Thread(target=push, daemon=True).start()
            started = time.monotonic()
            arrivals: list[tuple[float, dict[str, Any]]] = []
            beats = 0
            while len(arrivals) < 3:
                op = json.loads(response.readline())
                if op.get("op") == "heartbeat":
                    beats += 1
                    continue
                arrivals.append((time.monotonic() - started, op))
            assert beats > 0, "the idle stream kept beating through the stack"
            # The drop lands 0.3 s after the last op, and the stream keeps
            # beating until it does; what matters is that the response then
            # ENDS rather than hanging on a held worker thread.
            while (line := response.readline()) != b"":
                assert json.loads(line)["op"] == "heartbeat", line
            assert response.isclosed(), "the drop ended the response"
            connection.close()
        finally:
            server.stop()

        assert [a[1]["claim"] for a in arrivals] == ["v0", "v1", "v2"]
        assert arrivals[-1][0] > 0.5, "a middleware had buffered the response"
        assert stack.registry.get(entry.processor_id) is None
