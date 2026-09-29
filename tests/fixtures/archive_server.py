"""A library's archive route, with every fault a network can produce.

One mutable resource at one path, served by a real HTTP/1.1 server on a
throwaway port -- over TLS when asked -- with the Range / If-Range semantics
the real library stack has (wsgidav 4.3.3 behind cheroot, and nginx's
X-Accel offload, both measured in the design's probes):

* a strong ``ETag`` (the content's sha1, quoted);
* ``Range: bytes=N-`` answers 206 with ``Content-Range: bytes N-M/T``;
* ``If-Range`` that does not match the current ETag answers 200 with the
  WHOLE file;
* a range past the end answers 416.

Each GET takes the next ACTION off a queue (or the default one) and does
what it says, which is how a test scripts a fault for one request:

``truncate_after``  write N body bytes, then close the connection (FIN)
``reset_after``     write N body bytes, then reset it (SO_LINGER 0: RST)
``stall_after`` + ``stall_seconds``  write N bytes, go silent, then the rest
``status`` (+ ``retry_after``)  answer that status with no body
``replace_with``    swap the resource BEFORE answering
``ignore_if_range`` honour ``Range`` even when ``If-Range`` does not match
``ignore_range``    answer 200 with the whole file whatever ``Range`` says
``drop_etag``       send no ``ETag``;  ``weak_etag``: a ``W/`` one
``no_length``       send the body chunked, with no ``Content-Length``
``x_accel``         200, ``Content-Length: 0`` and an ``X-Accel-Redirect``
``corrupt_at``      flip the byte at this ABSOLUTE offset in this response

``flip_at`` on the server flips a byte of the resource itself, for every
request (damage at the library). Every request is recorded -- method, path,
``Range``, ``If-Range``, the status answered and when.
"""

from __future__ import annotations

import hashlib
import socket
import ssl
import struct
import threading
import time
from collections.abc import Callable
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

ARCHIVE_PATH = "/mokuro-reader/Alpha/Volume 1.cbz"
QUOTED_PATH = "/mokuro-reader/Alpha/Volume%201.cbz"


class ArchiveServer:
    def __init__(
        self,
        content: bytes,
        *,
        path: str = QUOTED_PATH,
        tls: tuple[Path, Path] | None = None,
    ) -> None:
        self._lock = threading.Lock()
        self._content = content
        self.path = path
        self.actions: list[dict[str, Any]] = []
        self.default: dict[str, Any] | Callable[[int], dict[str, Any]] = {}
        self.flip_at: int | None = None
        self.requests: list[dict[str, Any]] = []
        self._stop = threading.Event()
        server = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args: Any) -> None:
                return

            def do_HEAD(self) -> None:  # noqa: N802
                self.send_response(405)
                self.send_header("Content-Length", "0")
                self.end_headers()

            def do_GET(self) -> None:  # noqa: N802
                server._answer(self)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.httpd.daemon_threads = True
        if tls is not None:
            context = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
            context.load_cert_chain(str(tls[0]), str(tls[1]))
            self.httpd.socket = context.wrap_socket(self.httpd.socket, server_side=True)
        self.port = int(self.httpd.server_address[1])
        self.scheme = "https" if tls is not None else "http"
        self._thread = threading.Thread(
            target=self.httpd.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True
        )
        self._thread.start()

    # -- the resource ------------------------------------------------------

    @property
    def url(self) -> str:
        return f"{self.scheme}://127.0.0.1:{self.port}"

    @property
    def content(self) -> bytes:
        with self._lock:
            return self._content

    @content.setter
    def content(self, value: bytes) -> None:
        with self._lock:
            self._content = value

    def served(self) -> bytes:
        """The resource as a request sees it, with any persistent flip."""
        data = self.content
        if self.flip_at is not None:
            raw = bytearray(data)
            raw[self.flip_at] ^= 0xFF
            data = bytes(raw)
        return data

    @staticmethod
    def etag_of(data: bytes) -> str:
        return '"' + hashlib.sha1(data).hexdigest()[:16] + '"'

    def push(self, **action: Any) -> None:
        self.actions.append(action)

    def gets(self) -> list[dict[str, Any]]:
        return [r for r in self.requests if r["method"] == "GET"]

    def close(self) -> None:
        self._stop.set()
        self.httpd.shutdown()
        self.httpd.server_close()

    # -- one request ----------------------------------------------------------

    def _next_action(self) -> dict[str, Any]:
        with self._lock:
            if self.actions:
                return self.actions.pop(0)
            default = self.default
            count = len(self.requests)
        return dict(default(count) if callable(default) else default)

    def _answer(self, handler: BaseHTTPRequestHandler) -> None:
        action = self._next_action()
        record = {
            "method": "GET",
            "path": handler.path,
            "range": handler.headers.get("Range"),
            "if_range": handler.headers.get("If-Range"),
            "at": time.monotonic(),
        }
        with self._lock:
            self.requests.append(record)
        if "replace_with" in action:
            self.content = action["replace_with"]
        if handler.path != self.path:
            record["status"] = 404
            self._empty(handler, 404)
            return
        if "status" in action:
            record["status"] = int(action["status"])
            extra = []
            if action.get("retry_after") is not None:
                extra.append(("Retry-After", str(action["retry_after"])))
            self._empty(handler, int(action["status"]), extra)
            return
        data = self.served()
        etag = self.etag_of(self.content)
        if action.get("x_accel"):
            record["status"] = 200
            self._empty(handler, 200, [("X-Accel-Redirect", "/_accel/Alpha/Volume 1.cbz")])
            return
        start = 0
        partial = False
        raw_range = handler.headers.get("Range") or ""
        if_range = handler.headers.get("If-Range")
        if raw_range.startswith("bytes=") and not action.get("ignore_range"):
            honour = if_range is None or if_range == etag or action.get("ignore_if_range")
            if honour:
                first = raw_range[len("bytes=") :].partition("-")[0]
                start = int(first or 0)
                if start >= len(data):
                    record["status"] = 416
                    self._empty(handler, 416, [("Content-Range", f"bytes */{len(data)}")])
                    return
                partial = True
        body = data[start:]
        corrupt = action.get("corrupt_at")
        if corrupt is not None and start <= int(corrupt) < len(data):
            raw = bytearray(body)
            raw[int(corrupt) - start] ^= 0xFF
            body = bytes(raw)
        record["status"] = 206 if partial else 200
        handler.send_response(206 if partial else 200)
        handler.send_header("Content-Type", "application/vnd.comicbook+zip")
        handler.send_header("Accept-Ranges", "bytes")
        if action.get("weak_etag"):
            handler.send_header("ETag", "W/" + etag)
        elif not action.get("drop_etag"):
            handler.send_header("ETag", etag)
        if partial:
            handler.send_header("Content-Range", f"bytes {start}-{len(data) - 1}/{len(data)}")
        chunked = bool(action.get("no_length"))
        if chunked:
            handler.send_header("Transfer-Encoding", "chunked")
        else:
            handler.send_header("Content-Length", str(len(body)))
        handler.end_headers()
        self._write_body(handler, body, action, chunked)

    def _write_body(
        self, handler: BaseHTTPRequestHandler, body: bytes, action: dict[str, Any], chunked: bool
    ) -> None:
        cut = None
        for key in ("truncate_after", "reset_after", "stall_after"):
            if action.get(key) is not None:
                cut = min(int(action[key]), len(body))
        head, tail = (body, b"") if cut is None else (body[:cut], body[cut:])
        try:
            if chunked and action.get("truncate_after") is not None:
                # Whole chunks up to the cut, then one that promises more
                # than it delivers: the reader's chunked decoder must see the
                # cut (IncompleteRead), with the whole chunks in its hands.
                self._send(handler, head, chunked)
                piece = tail[: 64 * 1024]
                handler.wfile.write(b"%x\r\n" % len(piece) + piece[: len(piece) // 2])
                handler.close_connection = True
                handler.wfile.flush()
                handler.connection.shutdown(socket.SHUT_WR)
                return
            self._send(handler, head, chunked)
            if action.get("truncate_after") is not None:
                handler.close_connection = True
                handler.wfile.flush()
                handler.connection.shutdown(socket.SHUT_WR)
                return
            if action.get("reset_after") is not None:
                handler.wfile.flush()
                handler.close_connection = True
                handler.connection.setsockopt(
                    socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0)
                )
                handler.connection.close()
                return
            if action.get("stall_after") is not None:
                handler.wfile.flush()
                self._stop.wait(float(action.get("stall_seconds") or 0))
            self._send(handler, tail, chunked)
            if chunked:
                handler.wfile.write(b"0\r\n\r\n")
            handler.wfile.flush()
        except OSError:
            handler.close_connection = True

    @staticmethod
    def _send(handler: BaseHTTPRequestHandler, data: bytes, chunked: bool) -> None:
        if not data:
            return
        if not chunked:
            handler.wfile.write(data)
            return
        step = 64 * 1024
        for at in range(0, len(data), step):
            piece = data[at : at + step]
            handler.wfile.write(b"%x\r\n" % len(piece) + piece + b"\r\n")

    @staticmethod
    def _empty(
        handler: BaseHTTPRequestHandler, status: int, extra: list[tuple[str, str]] | None = None
    ) -> None:
        handler.send_response(status)
        for key, value in extra or []:
            handler.send_header(key, value)
        handler.send_header("Content-Length", "0")
        handler.end_headers()
