"""A processor trades its password for a token and sends that from then on."""

from __future__ import annotations

import base64
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.processor.client import LibraryClient, LibraryLoginRefused
from mokuro_bunko.processor.config import (
    LibrarySettings,
    ProcessorConfig,
    ProcessorOcr,
    ProcessorSettings,
)

BASIC = "Basic " + base64.b64encode(b"tower:hunter2hunter2").decode()


class _Library:
    """A library that issues tokens (or not), and remembers what it was sent."""

    def __init__(self, *, tokens: bool = True, password_ok: bool = True) -> None:
        self.tokens = tokens
        self.password_ok = password_ok
        self.valid: set[str] = set()
        self.issued = 0
        self.register_auth: list[str] = []

    def handler(self) -> type[BaseHTTPRequestHandler]:
        library = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args: Any) -> None:
                return None

            def _reply(self, status: int, body: dict[str, Any]) -> None:
                raw = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length") or 0)
                self.rfile.read(length)
                auth = self.headers.get("Authorization", "")
                if self.path == "/login/api/token":
                    if not library.tokens:
                        return self._reply(404, {"error": "not found"})
                    if auth != BASIC or not library.password_ok:
                        return self._reply(401, {"error": "Invalid credentials"})
                    library.issued += 1
                    token = f"tok-{library.issued}"
                    library.valid.add(token)
                    return self._reply(200, {"token": token, "token_type": "Bearer"})
                if self.path == "/_processor/register":
                    library.register_auth.append(auth)
                    ok = auth == BASIC if not library.tokens else auth.removeprefix("Bearer ") in library.valid
                    if not ok:
                        return self._reply(401, {"error": "Invalid or expired token"})
                    return self._reply(200, {
                        "processor_id": "p1", "session_stream": "/_processor/p1/stream",
                        "events": "/_processor/p1/sessions/{sid}/events",
                    })
                return self._reply(404, {"error": "not found"})

        return Handler


@pytest.fixture
def serve() -> Any:
    servers: list[ThreadingHTTPServer] = []

    def start(library: _Library) -> int:
        server = ThreadingHTTPServer(("127.0.0.1", 0), library.handler())
        threading.Thread(target=server.serve_forever, daemon=True).start()
        servers.append(server)
        return int(server.server_address[1])

    yield start
    for server in servers:
        server.shutdown()


def _client(port: int, tmp_path: Path) -> LibraryClient:
    return LibraryClient(ProcessorConfig(
        library=LibrarySettings(url=f"http://127.0.0.1:{port}", username="tower",
                                password="hunter2hunter2"),
        processor=ProcessorSettings(name="tower", storage=tmp_path / "state"),
        ocr=ProcessorOcr(),
    ))


def test_it_registers_with_a_token_not_its_password(serve: Any, tmp_path: Path) -> None:
    library = _Library()
    client = _client(serve(library), tmp_path)
    client.register({}, {})
    assert library.register_auth == ["Bearer tok-1"]
    assert client.request_headers()["Authorization"] == "Bearer tok-1"


def test_a_library_older_than_tokens_is_sent_the_password(serve: Any, tmp_path: Path) -> None:
    library = _Library(tokens=False)
    client = _client(serve(library), tmp_path)
    client.register({}, {})
    assert library.register_auth == [BASIC]


def test_a_refused_token_is_replaced_once(serve: Any, tmp_path: Path) -> None:
    library = _Library()
    client = _client(serve(library), tmp_path)
    client.register({}, {})
    library.valid.clear()  # a password change revoked it
    client.register({}, {})
    assert library.register_auth[-2:] == ["Bearer tok-1", "Bearer tok-2"]
    assert library.issued == 2


def test_a_wrong_password_is_still_a_refused_login(serve: Any, tmp_path: Path) -> None:
    library = _Library(password_ok=False)
    client = _client(serve(library), tmp_path)
    with pytest.raises(LibraryLoginRefused):
        client.register({}, {})
