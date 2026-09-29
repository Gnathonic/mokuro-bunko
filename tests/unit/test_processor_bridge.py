"""The processor against a REAL library: ops in, archives fetched, a sidecar back.

Nothing here is mocked on either side. The library half is the real
:class:`~mokuro_bunko.ocr.remote.library_api.ProcessorAPI` and the real
:class:`~mokuro_bunko.ocr.remote.session.RemoteSession` behind a real
cheroot server on a throwaway port; the processor half is the real
:class:`~mokuro_bunko.processor.client.LibraryClient` and
:class:`~mokuro_bunko.processor.bridge.RunnerBridge` -- with its real spool
and fetcher -- spawning a real subprocess. Only the OCR itself is a double:
``tests/fixtures/fake_runner`` for the events (reading the archive for real
where ``read_archive`` is on), and a plain op RECORDER where what is being
pinned down is the exact op the bridge writes to a runner's stdin.
"""

from __future__ import annotations

import errno
import hashlib
import http.client
import json
import os
import sys
import threading
import time
import zipfile
import zlib
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.generations import GenerationSpec
from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.remote import library_api
from mokuro_bunko.ocr.remote.library_api import ProcessorAPI
from mokuro_bunko.ocr.remote.registry import ProcessorEntry, ProcessorRegistry
from mokuro_bunko.ocr.remote.session import RemoteSession
from mokuro_bunko.ocr.session import SessionVolume
from mokuro_bunko.processor import client as client_module
from mokuro_bunko.processor.archives import ArchiveSpool, FetchTiming
from mokuro_bunko.processor.bridge import DAMAGED_AT_LIBRARY_NOTE, RunnerBridge
from mokuro_bunko.processor.client import (
    ACTION_MOVE_ON,
    ACTION_REREGISTER,
    ACTION_RETRY,
    LibraryClient,
    ReregisterNeeded,
)
from mokuro_bunko.processor.config import (
    LibrarySettings,
    ProcessorConfig,
    ProcessorOcr,
    ProcessorSettings,
)

FAKE_RUNNER = Path(__file__).resolve().parents[1] / "fixtures" / "fake_runner.py"

# The fetcher's patience, shrunk: a download that never moves is given back
# after a second and a half instead of two minutes.
FAST = FetchTiming(
    connect_timeout=5.0, read_timeout=1.0, retry_delays=(0.05, 0.1, 0.2),
    stall_seconds=1.5, max_restarts=3, progress_after=0.0,
)

# An op recorder, not a runner: it answers nothing at all. Where a test is
# about the op the bridge writes, the evidence is the op itself -- and the
# archive it names, which the recorder opens and fingerprints on the spot,
# because a RAM archive (`/proc/<pid>/fd/<n>`) is gone once its claim is.
RECORDER = r'''
import hashlib, json, os, sys
with open(os.environ["RECORDER_LOG"], "a", encoding="utf-8") as handle:
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        op = json.loads(line)
        if op.get("archive"):
            try:
                with open(op["archive"], "rb") as archive:
                    op["sha256"] = hashlib.sha256(archive.read()).hexdigest()
            except OSError as e:
                op["sha256"] = "unreadable: %s" % e
        handle.write(json.dumps(op) + "\n")
        handle.flush()
        if op.get("op") == "close":
            break
'''

# A runner that never reads its stdin at all -- the slowest consumer there
# is. On the old road it held the library's socket until the library's own
# write timeout cut the archive short.
NEVER_READS = "import time\ntime.sleep(3600)\n"


# --- archives ---------------------------------------------------------------


def _page_bytes(name: str) -> bytes:
    """Content that says which member it is, so a shuffle cannot hide."""
    return f"fake image data for {name}".encode() * 8


def _cbz(path: Path, names: list[str], *, stored: bool = False) -> None:
    """A .cbz whose members are stored in the order given."""
    path.parent.mkdir(parents=True, exist_ok=True)
    method = zipfile.ZIP_STORED if stored else zipfile.ZIP_DEFLATED
    with zipfile.ZipFile(path, "w", method) as zf:
        for name in names:
            zf.writestr(name, _page_bytes(name))


def _big_cbz(path: Path, pages: int, size: int = 64 * 1024) -> None:
    """Stored random pages: bigger than any socket buffer."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w", zipfile.ZIP_STORED) as zf:
        for n in range(pages):
            zf.writestr(f"{n:03d}.jpg", os.urandom(size))


def _flip_in_member(path: Path, member: str) -> None:
    """Damage one STORED member's bytes in place: its CRC no longer matches."""
    raw = bytearray(path.read_bytes())
    at = raw.find(_page_bytes(member))
    assert at > 0, member
    raw[at + 5] ^= 0xFF
    path.write_bytes(bytes(raw))


def _break_eocd(path: Path) -> None:
    raw = bytearray(path.read_bytes())
    at = raw.rfind(b"PK\x05\x06")
    assert at > 0
    raw[at : at + 4] = b"XXXX"
    path.write_bytes(bytes(raw))


def _sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Faults:
    """Per-file misbehaviour of the test library's archive route."""

    def __init__(self) -> None:
        # archive name -> {"cut_at": N} (every GET ends after N bytes),
        # {"status": 404}, or {"stall": seconds} (after its first bytes).
        self.by_name: dict[str, dict[str, Any]] = {}
        self.gets: list[tuple[str, str | None, str | None]] = []


def _archives_app(root: Path, faults: Faults | None = None) -> Callable[..., Any]:
    """The one thing the library serves a processor: its .cbz files.

    With what the real stack (wsgidav behind cheroot) gives a download: a
    strong ETag, Range answered 206, and an If-Range that no longer matches
    answered with the whole file.
    """
    faults = faults if faults is not None else Faults()

    def app(environ: dict[str, Any], start_response: Callable[..., Any]) -> Any:
        path = environ.get("PATH_INFO", "")
        prefix = "/mokuro-reader/"
        target = root.joinpath(*path[len(prefix) :].split("/")) if path.startswith(prefix) else None
        name = target.name if target is not None else path
        faults.gets.append((name, environ.get("HTTP_RANGE"), environ.get("HTTP_IF_RANGE")))
        rule = faults.by_name.get(name, {})
        if rule.get("status"):
            start_response(f"{rule['status']} Refused", [("Content-Length", "0")])
            return [b""]
        if target is None or not target.is_file():
            start_response("404 Not Found", [("Content-Length", "0")])
            return [b""]
        data = target.read_bytes()
        st = target.stat()
        etag = f'"{st.st_mtime_ns:x}-{st.st_size:x}"'
        start = 0
        raw_range = environ.get("HTTP_RANGE", "")
        if_range = environ.get("HTTP_IF_RANGE")
        if raw_range.startswith("bytes=") and (if_range is None or if_range == etag):
            start = int(raw_range[len("bytes=") :].partition("-")[0] or 0)
        body = data[start:]
        headers = [("Content-Length", str(len(body))), ("ETag", etag),
                   ("Accept-Ranges", "bytes")]
        if start:
            headers.append(("Content-Range", f"bytes {start}-{len(data) - 1}/{len(data)}"))
            start_response("206 Partial Content", headers)
        else:
            start_response("200 OK", headers)
        if environ.get("REQUEST_METHOD") == "HEAD":
            return []
        if "cut_at" in rule:
            def cut() -> Iterator[bytes]:
                yield body[: int(rule["cut_at"])]
                raise ConnectionAbortedError("the test library cuts this archive short")
            return cut()
        if "stall" in rule:
            def stall() -> Iterator[bytes]:
                yield body[:600]
                time.sleep(float(rule["stall"]))
                yield body[600:]
            return stall()
        return [body]

    return app


# --- the two halves, both real ----------------------------------------------


class _Stack:
    def __init__(
        self,
        *,
        registry: ProcessorRegistry,
        entry: ProcessorEntry,
        client: LibraryClient,
        bridge: RunnerBridge,
        library_path: Path,
        tmp_path: Path,
        faults: Faults,
    ) -> None:
        self.registry = registry
        self.entry = entry
        self.client = client
        self.bridge = bridge
        self.library_path = library_path
        self.tmp_path = tmp_path
        self.faults = faults
        self.sessions: list[RemoteSession] = []

    @property
    def spool(self) -> ArchiveSpool:
        return self.bridge.spool

    def _session(self, sid: str) -> RemoteSession:
        row = GenerationSpec(
            id="g-1", name="mokuro", engine="mokuro", primary=True, enabled=True
        )
        session = RemoteSession(
            self.entry, row, sid=sid, row_spec=row.to_dict(),
            library_path=self.library_path,
        )
        self.sessions.append(session)
        return session

    def open_session(self, sid: str = "s1") -> RemoteSession:
        """Ask the processor for a runner, the way the worker does."""
        session = self._session(sid)
        assert session.start(), "the open_session op was not queued"
        return session

    def staged_session(self, sid: str) -> RemoteSession:
        """A session the library knows about but has NOT asked for.

        The channel tests are about the events body, not about a runner:
        sending the op would have the bridge open the body first, and every
        assertion about a refusal would then be racing it.
        """
        session = self._session(sid)
        with self.entry.lock:
            self.entry.sessions[sid] = session
        return session

    def volume(
        self, session: RemoteSession, claim: str, archive: Path, *, title: str = "Alpha"
    ) -> SessionVolume:
        collected = self.tmp_path / "collected" / claim
        collected.mkdir(parents=True, exist_ok=True)
        volume = SessionVolume(
            id=claim,
            workspace=collected,
            output=collected / f"{archive.stem}.mokuro",
            cache_dir=collected / "cache",
            detect_dir=collected / "detect",
            log=collected / "volume.log",
            title=title,
            volume=archive.stem,
            archive=archive,
            volume_uuid=f"uuid-{claim}",
            archive_size=archive.stat().st_size if archive.exists() else None,
        )
        assert session.submit(volume), "the volume op was not queued"
        return volume


@contextmanager
def _stack(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    *,
    runner: Path,
    script: dict[str, Any] | None = None,
    max_sessions: int = 2,
    tls: bool = False,
    server_timeout: float | None = None,
    faults: Faults | None = None,
) -> Iterator[_Stack]:
    """Both halves, over plain HTTP or -- ``tls`` -- over the library's own
    TLS, exactly as `server.py` sets it up (cheroot's BuiltinSSLAdapter,
    which negotiates TLS 1.3 and so sends session tickets the moment the
    handshake is done)."""
    from cheroot.wsgi import Server as WSGIServer

    # Headers reach the client with the response's FIRST chunk, which on an
    # idle queue is the first heartbeat -- an unpatched 15 s would be 15 s
    # inside `getresponse()` before a single op could arrive.
    monkeypatch.setattr(library_api, "HEARTBEAT_SECONDS", 0.2)

    library_path = tmp_path / "library"
    library_path.mkdir(parents=True, exist_ok=True)
    registry = ProcessorRegistry()
    faults = faults if faults is not None else Faults()
    api = ProcessorAPI(_archives_app(library_path, faults), registry)

    def with_actor(environ: dict[str, Any], start_response: Any) -> Any:
        environ["mokuro.role"] = "processor"
        environ["mokuro.username"] = "tower"
        return api(environ, start_response)

    server = WSGIServer(("127.0.0.1", 0), with_actor, numthreads=12)
    if server_timeout is not None:
        server.timeout = server_timeout
    verify: bool | str = True
    if tls:
        from cheroot.ssl.builtin import BuiltinSSLAdapter

        from mokuro_bunko.ssl import generate_self_signed_cert

        cert, key = tmp_path / "tls" / "cert.pem", tmp_path / "tls" / "key.pem"
        cert.parent.mkdir(parents=True, exist_ok=True)
        generate_self_signed_cert(cert, key)
        server.ssl_adapter = BuiltinSSLAdapter(str(cert), str(key))
        # The library's certificate as the trust anchor: the middle option
        # of `tls_verify`, and the one a self-signed library is run with.
        verify = str(cert)
    server.prepare()
    serving = threading.Thread(target=server.serve, daemon=True)
    serving.start()
    port = int(server.bind_addr[1])

    script_path = tmp_path / "script.json"
    script_path.write_text(json.dumps(script or {}), encoding="utf-8")
    monkeypatch.setenv("FAKE_RUNNER_SCRIPT", str(script_path))
    monkeypatch.setenv("MOKURO_PROCESSOR_RUNNER", str(runner))

    storage = tmp_path / "processor"
    config = ProcessorConfig(
        library=LibrarySettings(
            url=f"{'https' if tls else 'http'}://127.0.0.1:{port}",
            username="tower",
            password="hunter2hunter2",
            tls_verify=verify,
        ),
        processor=ProcessorSettings(
            name="tower", max_sessions=max_sessions, storage=storage
        ),
        ocr=ProcessorOcr(),
    )
    client = LibraryClient(config)
    client.register({"engines": ["mokuro"], "detectors": [], "devices": []}, {})
    entry = registry.get(client.processor_id)
    assert entry is not None
    shm = tmp_path / "shm"
    shm.mkdir(exist_ok=True)
    bridge = RunnerBridge(
        client,
        storage=storage,
        engines_python=Path(sys.executable),
        concurrency=1,
        spool=ArchiveSpool(storage, memory_dir=shm, headroom=lambda: None),
        timing=FAST,
    )

    def pump() -> None:
        for op in client.ops():
            bridge.handle(op)

    ops_thread = threading.Thread(target=pump, name="ops", daemon=True)
    ops_thread.start()
    # The library marks the stream open from INSIDE its response generator,
    # so "registered" is not yet "connected": every test here starts from a
    # stream that is really open, or none of them can rely on one.
    deadline = time.monotonic() + 15.0
    while not entry.stream_open and time.monotonic() < deadline:
        time.sleep(0.02)
    assert entry.stream_open, "the assignment stream never opened"
    stack = _Stack(
        registry=registry, entry=entry, client=client, bridge=bridge,
        library_path=library_path, tmp_path=tmp_path, faults=faults,
    )
    try:
        yield stack
    finally:
        for session in stack.sessions:
            session.kill()
        bridge.shutdown()
        registry.drop(client.processor_id, "the test is over")
        client.close()
        ops_thread.join(timeout=10.0)
        server.stop()
        serving.join(timeout=10.0)
        bridge.spool.close()


# --- waiting ----------------------------------------------------------------


def _events_until(
    session: RemoteSession, kind: str, *, timeout: float = 60.0,
    until: Callable[[dict[str, Any]], bool] | None = None,
) -> list[dict[str, Any]]:
    """Every event up to and including the first ``kind`` (that ``until`` likes)."""
    deadline = time.monotonic() + timeout
    seen: list[dict[str, Any]] = []
    while time.monotonic() < deadline:
        event = session.poll_event(timeout=1.0)
        if event is None:
            continue
        seen.append(event)
        if event.get("event") == kind and (until is None or until(event)):
            return seen
    raise AssertionError(f"no {kind!r} within {timeout:.0f}s; saw {seen}")


def _recorded(log: Path) -> list[dict[str, Any]]:
    try:
        text = log.read_text(encoding="utf-8")
    except OSError:
        return []
    lines = [line for line in text.splitlines() if line.strip()]
    if lines and not text.endswith("\n"):
        # A write the recorder is still in the middle of. Only the LAST line
        # can ever be partial, and the next poll will have all of it.
        lines.pop()
    return [json.loads(line) for line in lines]


def _recorded_until(
    log: Path, predicate: Callable[[list[dict[str, Any]]], bool], *, timeout: float = 60.0
) -> list[dict[str, Any]]:
    deadline = time.monotonic() + timeout
    ops: list[dict[str, Any]] = []
    while time.monotonic() < deadline:
        ops = _recorded(log)
        if predicate(ops):
            return ops
        time.sleep(0.05)
    raise AssertionError(f"the runner never received what was expected; got {ops}")


def _runner_log_lines(stack: _Stack, sid: str = "s1") -> list[str]:
    log = stack.bridge.storage / "logs" / f"session.{sid}.log"
    try:
        return log.read_text(encoding="utf-8").splitlines()
    except OSError:
        return []


def _archive_line(stack: _Stack, sid: str = "s1") -> list[dict[str, str]]:
    """The fake runner's ``archive ... sha256=... pages=...`` lines, parsed."""
    out = []
    for line in _runner_log_lines(stack, sid):
        if not line.startswith("archive ") or "sha256=" not in line:
            continue
        head, _, rest = line.partition(" stem=")
        fields = dict(part.split("=", 1) for part in rest.split(" ")[1:] if "=" in part)
        fields["path"] = head[len("archive ") :]
        fields["stem"] = rest.split(" sha256=")[0]
        out.append(fields)
    return out


@pytest.fixture
def recorder(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> tuple[Path, Path]:
    """The op recorder script, and the file it appends every op to."""
    script = tmp_path / "op_recorder.py"
    script.write_text(RECORDER, encoding="utf-8")
    log = tmp_path / "ops.jsonl"
    monkeypatch.setenv("RECORDER_LOG", str(log))
    return script, log


def _no_terminal_for(events: list[dict[str, Any]], claim: str) -> bool:
    return not any(
        e.get("id") == claim
        and e.get("event") in ("volume_done", "volume_failed", "volume_returned")
        for e in events
    )


def _drain(session: RemoteSession, seconds: float = 1.0) -> list[dict[str, Any]]:
    seen: list[dict[str, Any]] = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        event = session.poll_event(timeout=0.1)
        if event is not None:
            seen.append(event)
    return seen


# --- the round trip ---------------------------------------------------------


class TestTheRoundTrip:
    def test_a_volume_goes_out_whole_and_its_sidecar_comes_back(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The whole seam: op, archive fetched and verified, runner, sidecar.

        The runner reads the archive for real, through the unnamed RAM file
        the processor holds it in, and the thumbnail rule is keyed on the
        stem the OP carries -- the path it reads is /proc/<pid>/fd/<n>.
        """
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, ["000.jpg", "001.jpg", "002.jpg", "003.jpg", "Volume 1.webp"])
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            session = stack.open_session()
            volume = stack.volume(session, "v1", archive)
            events = _events_until(session, "volume_done")
            (line,) = _archive_line(stack)

        kinds = [e["event"] for e in events]
        ready = next(e for e in events if e["event"] == "fetch" and e.get("state") == "ready")
        assert ready["requests"] == 1 and ready["placement"] == "memory"
        assert ready["crc32"] == f"{zlib.crc32(archive.read_bytes()) & 0xFFFFFFFF:08x}"
        assert kinds.index("fetch") < kinds.index("volume_started")
        started = next(e for e in events if e["event"] == "volume_started")
        assert started["pages"] == 4, "the embedded thumbnail is not a page"
        assert events[-1]["pages"] == 4
        assert line["path"].startswith(f"/proc/{os.getpid()}/fd/")
        assert line["stem"] == "Volume 1"
        assert line["sha256"] == _sha256(archive), "the runner read the library's bytes"
        assert volume.output.is_file(), "the library wrote the sidecar it was sent"
        sidecar = json.loads(volume.output.read_text(encoding="utf-8"))
        assert [p["img_path"] for p in sidecar["pages"]] == [
            "000.jpg", "001.jpg", "002.jpg", "003.jpg"
        ]
        assert sidecar["volume_uuid"] == "uuid-v1"

    def test_the_next_volume_is_fetched_while_the_first_is_read(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The on-deck volume: v2 is fetched, verified and handed over while
        the runner is still on v1's pages -- in claim order."""
        first = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        second = tmp_path / "library" / "Alpha" / "Volume 2.cbz"
        _cbz(first, [f"{n:03d}.jpg" for n in range(4)])
        _cbz(second, [f"{n:03d}.jpg" for n in range(7)])
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True, "page_delay": 0.3}) as stack:
            session = stack.open_session()
            one = stack.volume(session, "v1", first)
            two = stack.volume(session, "v2", second)
            events = _events_until(session, "volume_done",
                                   until=lambda e: e["id"] == "v2", timeout=90)

        order = [
            (e["event"], e.get("id"))
            for e in events
            if (e["event"] == "fetch" and e.get("state") == "ready")
            or e["event"] == "volume_done"
        ]
        assert order.index(("fetch", "v1")) < order.index(("fetch", "v2"))
        assert order.index(("fetch", "v2")) < order.index(("volume_done", "v1")), order
        done = {e["id"]: e for e in events if e["event"] == "volume_done"}
        assert done["v1"]["pages"] == 4 and done["v2"]["pages"] == 7
        assert json.loads(one.output.read_text(encoding="utf-8"))["pages"]
        assert json.loads(two.output.read_text(encoding="utf-8"))["pages"]


class TestTheOpsTheBridgeWrites:
    def test_one_archive_op_per_claim_naming_the_verified_copy(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
        recorder: tuple[Path, Path],
    ) -> None:
        """Exactly one `volume` op, with `archive` the processor's RAM copy
        and `stem` the library archive's -- no `page` or `end` ops, ever."""
        script, log = recorder
        first = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        second = tmp_path / "library" / "Alpha" / "Volume 2.cbz"
        _cbz(first, ["003.jpg", "001.jpg", "notes.txt", "000.jpg", "Volume 1.webp", "002.jpg"])
        _cbz(second, ["000.jpg"])
        with _stack(tmp_path, monkeypatch, runner=script) as stack:
            session = stack.open_session()
            stack.volume(session, "v1", first)
            stack.volume(session, "v2", second)
            ready = _events_until(session, "fetch", until=lambda e: e.get("state") == "ready"
                                  and e["id"] == "v2")
            ops = _recorded_until(log, lambda o: len(o) >= 2, timeout=5)

        kinds = [op["op"] for op in ops]
        assert kinds == ["volume", "volume"], kinds
        assert [op["id"] for op in ops] == ["v1", "v2"], "in claim order"
        for op, archive in zip(ops, (first, second), strict=True):
            assert op["archive"].startswith(f"/proc/{os.getpid()}/fd/")
            assert op["stem"] == archive.stem
            assert "pages" not in op and "page_count" not in op
            assert op["sha256"] == _sha256(archive), "the bytes are the library's"
        assert [e["id"] for e in ready if e["event"] == "fetch"] == ["v1", "v2"]

    def test_a_runner_that_never_reads_cannot_hold_the_library_s_socket(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        """THE regression for the root cause, at unit scale.

        The library's write timeout is 1 s here, and its runner never reads
        a byte of stdin. Two 20 MiB archives -- more than loopback socket
        buffers and a 64 KiB pipe can absorb -- still arrive whole and
        verified, because nothing about the runner can pause the download.
        """
        script = tmp_path / "never_reads.py"
        script.write_text(NEVER_READS, encoding="utf-8")
        first = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        second = tmp_path / "library" / "Alpha" / "Volume 2.cbz"
        _big_cbz(first, 320)
        _big_cbz(second, 320)
        with _stack(tmp_path, monkeypatch, runner=script, server_timeout=1.0) as stack:
            session = stack.open_session()
            stack.volume(session, "v1", first)
            stack.volume(session, "v2", second)
            events = _events_until(
                session, "fetch", timeout=60,
                until=lambda e: e.get("state") == "ready" and e["id"] == "v2",
            )
        ready = {e["id"]: e for e in events if e["event"] == "fetch" and e.get("state") == "ready"}
        for claim, archive in (("v1", first), ("v2", second)):
            assert ready[claim]["bytes"] == archive.stat().st_size
            assert ready[claim]["crc32"] == f"{zlib.crc32(archive.read_bytes()) & 0xFFFFFFFF:08x}"
            assert ready[claim]["requests"] == 1, "not even a resume was needed"
        assert "volume_returned" not in [e["event"] for e in events]

    def test_an_archive_is_released_before_its_done_reaches_the_library(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, [f"{n:03d}.jpg" for n in range(3)])
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True, "page_delay": 0.1}) as stack:
            session = stack.open_session()
            stack.volume(session, "v1", archive)
            _events_until(session, "fetch", until=lambda e: e.get("state") == "ready")
            assert stack.spool.in_memory_bytes == archive.stat().st_size
            _events_until(session, "volume_done")
            assert stack.spool.in_memory_bytes == 0, "held past its claim's end"
            session.close()
            _events_until(session, "exit")
            assert stack.spool.in_memory_bytes == 0

    def test_a_done_whose_sidecar_cannot_be_sent_becomes_a_failure(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The sidecar goes FIRST, and a `volume_done` it did not precede is
        not a done."""
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, [f"{n:03d}.jpg" for n in range(3)])
        with _stack(
            tmp_path, monkeypatch, runner=FAKE_RUNNER,
            script={"volumes": {"v1": {"no_sidecar": True}}},
        ) as stack:
            session = stack.open_session()
            volume = stack.volume(session, "v1", archive)
            events = _events_until(session, "volume_failed")

        assert "volume_done" not in [e["event"] for e in events]
        assert "sidecar" in events[-1]["error"]
        assert not volume.output.exists()

    def test_an_archive_with_no_pages_is_the_runner_s_to_fail(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The processor no longer decides what a page is: it delivers, and
        the runner fails the volume in its own words, naming the LIBRARY's
        archive rather than the path it read."""
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, ["readme.txt", "Volume 1.webp"])
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            session = stack.open_session()
            stack.volume(session, "v1", archive)
            events = _events_until(session, "volume_failed")

        assert events[-1]["error"] == "no page images found in Volume 1.cbz"


class TestWhatAnArchiveCanBe:
    def test_a_page_damaged_at_the_library_is_blanked_in_its_place(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, [f"{n:03d}.jpg" for n in range(4)], stored=True)
        _flip_in_member(archive, "002.jpg")
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            session = stack.open_session()
            volume = stack.volume(session, "v1", archive)
            events = _events_until(session, "volume_done")
            (line,) = _archive_line(stack)

        ready = next(e for e in events if e["event"] == "fetch" and e.get("state") == "ready")
        assert ready["verdict"] == "damaged at the library"
        assert ready["damaged"] == ["002.jpg"]
        assert len([g for g in stack.faults.gets if g[0] == "Volume 1.cbz"]) == 2
        kinds = [e["event"] for e in events]
        assert "volume_failed" not in kinds and "fatal" not in kinds
        assert line["blank"] == "1"
        assert events[-1]["pages"] == 4
        assert volume.output.is_file()

    def test_a_zip_damaged_at_the_library_fails_by_the_runner_s_word_and_says_so(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        broken = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        fine = tmp_path / "library" / "Alpha" / "Volume 2.cbz"
        _cbz(broken, [f"{n:03d}.jpg" for n in range(3)])
        _break_eocd(broken)
        _cbz(fine, [f"{n:03d}.jpg" for n in range(2)])
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            session = stack.open_session()
            stack.volume(session, "v1", broken)
            stack.volume(session, "v2", fine)
            events = _events_until(session, "volume_done", until=lambda e: e["id"] == "v2")

        failed = next(e for e in events if e["event"] == "volume_failed")
        assert failed["id"] == "v1"
        assert "BadZipFile" in failed["error"]
        assert failed["error"].endswith(DAMAGED_AT_LIBRARY_NOTE)
        assert [e["event"] for e in events].count("ready") == 1, "the same runner went on"

    def test_a_failure_over_pages_damaged_at_the_library_names_them(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The runner blanks each page and fails the volume ("every page
        failed"), as locally; what the library is told names the members the
        processor proved damaged, not only that the copy was the library's."""
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, ["000.jpg", "001.jpg"], stored=True)
        _flip_in_member(archive, "000.jpg")
        _flip_in_member(archive, "001.jpg")
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            session = stack.open_session()
            stack.volume(session, "v1", archive)
            events = _events_until(session, "volume_failed")

        assert events[-1]["error"] == (
            "every page failed (the library's copy: the same bytes on two downloads; "
            "'000.jpg', '001.jpg' fail their CRC-32 checks)"
        )

    def test_an_archive_that_will_not_download_goes_back_and_the_next_one_runs(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        cut = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        fine = tmp_path / "library" / "Alpha" / "Volume 2.cbz"
        _cbz(cut, [f"{n:03d}.jpg" for n in range(3)])
        _cbz(fine, [f"{n:03d}.jpg" for n in range(2)])
        faults = Faults()
        faults.by_name["Volume 1.cbz"] = {"cut_at": 0}
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER, faults=faults,
                    script={"read_archive": True}) as stack:
            session = stack.open_session()
            stack.volume(session, "v1", cut)
            stack.volume(session, "v2", fine)
            events = _events_until(session, "volume_done", until=lambda e: e["id"] == "v2")
            assert stack.spool.in_memory_bytes == 0

        returned = [e for e in events if e["event"] == "volume_returned"]
        assert [(e["id"], e["class"]) for e in returned] == [("v1", "stalled")]
        assert not [e for e in events if e["event"] == "volume_failed"]
        assert "fatal" not in [e["event"] for e in events]


class TestTheFeederNeverDies:
    def test_a_local_fault_gives_the_claim_back_and_the_next_is_still_fed(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archives = [tmp_path / "library" / "Alpha" / f"Volume {n}.cbz" for n in (1, 2, 3)]
        for archive in archives:
            _cbz(archive, ["000.jpg", "001.jpg"])
        real_workspace = OCRProcessor.new_workspace
        full = {"left": 1}

        def new_workspace(self: Any, hint: str) -> Path:
            if full["left"]:
                full["left"] -= 1
                raise OSError(errno.ENOSPC, "No space left on device")
            return real_workspace(self, hint)

        real_begin = http.client.HTTPResponse.begin
        garbled = {"left": 0}

        def begin(self: Any) -> None:
            if garbled["left"]:
                garbled["left"] -= 1
                raise http.client.LineTooLong("header line")
            real_begin(self)

        monkeypatch.setattr(OCRProcessor, "new_workspace", new_workspace)
        monkeypatch.setattr(http.client.HTTPResponse, "begin", begin)
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            session = stack.open_session()
            _events_until(session, "ready", timeout=30)
            stack.volume(session, "v1", archives[0])
            first = _events_until(session, "volume_returned")
            assert stack.spool.in_memory_bytes == 0, "the placement went back with it"
            garbled["left"] = 1
            stack.volume(session, "v2", archives[1])
            second = _events_until(session, "volume_returned")
            stack.volume(session, "v3", archives[2])
            third = _events_until(session, "volume_done")

        assert first[-1]["id"] == "v1" and first[-1]["class"] == "local"
        assert "No space left" in first[-1]["error"]
        assert second[-1]["id"] == "v2" and second[-1]["class"] == "local"
        assert "LineTooLong" in second[-1]["error"]
        assert third[-1]["id"] == "v3"

    def test_a_cancel_during_a_stalled_download_says_nothing(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, [f"{n:03d}.jpg" for n in range(20)])
        faults = Faults()
        faults.by_name["Volume 1.cbz"] = {"stall": 3.0}
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER, faults=faults) as stack:
            session = stack.open_session()
            stack.volume(session, "c1", archive)
            deadline = time.monotonic() + 10
            while not stack.faults.gets and time.monotonic() < deadline:
                time.sleep(0.02)
            time.sleep(0.3)
            started = time.monotonic()
            session.kill()
            while stack.bridge.fetcher._active and time.monotonic() - started < 5:
                time.sleep(0.02)
            assert time.monotonic() - started < 2.0, "the download was not aborted"
            seen = _drain(session, 1.0)
        assert _no_terminal_for(seen, "c1"), seen

    def test_a_close_during_a_download_says_nothing_about_it(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, [f"{n:03d}.jpg" for n in range(20)])
        faults = Faults()
        faults.by_name["Volume 1.cbz"] = {"stall": 3.0}
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER, faults=faults) as stack:
            session = stack.open_session()
            _events_until(session, "ready", timeout=30)
            stack.volume(session, "c1", archive)
            deadline = time.monotonic() + 10
            while not stack.faults.gets and time.monotonic() < deadline:
                time.sleep(0.02)
            time.sleep(0.3)
            session.close()
            seen = _events_until(session, "exit", timeout=15)
        assert _no_terminal_for(seen, "c1"), seen
        assert "fatal" not in [e["event"] for e in seen]


# --- the channels ------------------------------------------------------------


class TestTheChannels:
    def test_the_read_timeout_outlasts_two_missed_heartbeats(self) -> None:
        """A slow first byte is not a failure.

        The stream's headers only flush with its FIRST chunk, which on an
        idle queue is the first heartbeat -- so `getresponse()` alone can
        sit for most of one interval. A read timeout of exactly two
        intervals would spend that on the budget the two missed beats are
        supposed to measure.
        """
        assert client_module.MISSED_HEARTBEATS == 2
        assert client_module.STREAM_TIMEOUT > 2 * client_module.HEARTBEAT_SECONDS

    def test_a_409_on_the_stream_asks_for_a_fresh_registration(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A second stream means the first is a ghost, and the library drops
        the entry when it says so: the only way back is to register again."""
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER) as stack:
            second = LibraryClient(stack.client.config)
            second.processor_id = stack.client.processor_id
            second.channels = dict(stack.client.channels)
            with pytest.raises(ReregisterNeeded):
                next(second.ops())
            second.close()

    def test_a_dropped_processor_costs_the_events_body_and_asks_to_re_register(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """`dropped`: the claims are already back on the queue and the id we
        hold means nothing. Only a fresh registration fixes it."""
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER) as stack:
            session = stack.staged_session("s-drop")
            sink = stack.client.open_events("s-drop")
            stack.registry.drop(stack.client.processor_id, "pulled the plug")
            for _ in range(20):
                if not sink.send({"event": "ready"}):
                    break
                time.sleep(0.1)
            sink.close()
            assert sink.status == 409, sink.detail
            assert sink.code == "dropped"
            assert sink.action == ACTION_REREGISTER
            session.end("the test is over")

    def test_a_session_the_library_ended_is_moved_on_from_not_re_registered(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """`session_ended` has the same 409 as `dropped` and a different
        answer: this processor is still connected and still has work."""
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER) as stack:
            session = stack.staged_session("s-ended")
            sink = stack.client.open_events("s-ended")
            session.end("the library ended it")
            for _ in range(20):
                if not sink.send({"event": "ready"}):
                    break
                time.sleep(0.1)
            sink.close()
            assert sink.status == 409, sink.detail
            assert sink.code == "session_ended"
            assert sink.action == ACTION_MOVE_ON

    def test_a_body_already_open_is_waited_out_not_abandoned(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """`body_open`: a ghost of OUR own body still holds this session.

        The library lets that one go on its own socket timeout, so the
        answer is to back off and try again -- and above all not to disturb
        the body that is already there.
        """
        monkeypatch.setattr(client_module, "EVENTS_OPEN_ATTEMPTS", 2)
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER) as stack:
            session = stack.staged_session("s-busy")
            first = stack.client.open_events("s-busy")
            assert first.code == "", "the first body was accepted"
            second = stack.client.open_events("s-busy")
            assert second.status == 409, second.detail
            assert second.code == "body_open"
            assert second.action == ACTION_RETRY
            assert first.send({"event": "ready"}), "the first body is untouched"
            first.close()
            session.end("the test is over")

    def test_a_session_the_library_ends_takes_its_idle_runner_with_it(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The end can be noticed by the PING thread, with no event due.

        The library ends a session on its own -- a sidecar it could not
        write, a torn body -- and sends no op about it: the watcher only
        closes a session that is still alive. A runner sitting idle
        produces no event, so nothing but the ping ever touches that body
        again; if the ping's discovery is not acted on, the runner keeps
        its models loaded on a card the library already counts as free.
        """
        monkeypatch.setattr(client_module, "EVENTS_PING_SECONDS", 0.3)
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER) as stack:
            session = stack.open_session("s-idle")
            _events_until(session, "ready", timeout=30.0)
            with stack.bridge._lock:
                runner = stack.bridge._sessions["s-idle"].session
            assert runner.is_alive()
            session.end("the library ended it")
            deadline = time.monotonic() + 10.0
            while runner.is_alive() and time.monotonic() < deadline:
                time.sleep(0.05)
            assert not runner.is_alive(), "an idle runner outlived its session"

    def test_a_processor_that_leaves_blames_no_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Stopping `processor serve` mid-volume is the processor LEAVING.

        Spec sections 3 rule 4 and 6: its claims go back unrecorded. So once
        `shutdown` begins nothing more is said on any events body -- not the
        killed runner's `exit` (whose signal status the library would read
        as a runner crash and blame the oldest volume for), and not a
        `fatal` for a feed the shutdown cut -- and the body ends without an
        exit, which the library reads as the processor having gone: it drops
        the entry, and the session's exit is the plain `returncode: None`
        that drop produces. Found by Task 10's end-to-end test.
        """
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, [f"{n:03d}.jpg" for n in range(3)])
        with _stack(
            tmp_path, monkeypatch, runner=FAKE_RUNNER,
            script={"pages": 3, "page_delay": 1.0, "volume_delay": 1.0},
        ) as stack:
            session = stack.open_session()
            stack.volume(session, "v1", archive)
            before = _events_until(session, "volume_started", timeout=30.0)
            stack.bridge.shutdown()
            after = _events_until(session, "exit", timeout=30.0)
            gone = stack.registry.get(stack.entry.processor_id) is None

        said = [e["event"] for e in before + after]
        assert "fatal" not in said and "volume_failed" not in said, said
        assert after[-1] == {"event": "exit", "returncode": None}, (
            "the runner's own exit status must not reach the library"
        )
        assert gone, "a body that ended without its exit means the processor left"


class TestOverTheLibrarysTls:
    """The same seam over HTTPS, the way a library reachable from outside runs.

    TLS 1.3 sends session tickets right after the handshake, so the raw
    socket under an events body is readable before the library has said a
    word. Nothing here may take TLS bytes for an HTTP answer.
    """

    def test_an_events_body_is_not_mistaken_for_a_refusal(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER, tls=True) as stack:
            probe = stack.client._connect(timeout=10.0)
            probe.connect()
            try:
                # The premise: the version that sends tickets unasked.
                assert probe.sock.version() == "TLSv1.3"  # type: ignore[union-attr]
            finally:
                probe.close()
            session = stack.staged_session("s-tls")
            sink = stack.client.open_events("s-tls")
            assert sink.status is None, (sink.status, sink.code, sink.detail)
            assert sink.send({"event": "ready"}), "the body was ended at open"
            assert session.poll_event(timeout=10.0) == {"event": "ready"}
            sink.close()
            assert sink.status == 200

    def test_a_refusal_is_still_read_through_the_tls_layer(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A REAL early answer must still be caught, and its code read."""
        monkeypatch.setattr(client_module, "EVENTS_OPEN_ATTEMPTS", 1)
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER, tls=True) as stack:
            session = stack.staged_session("s-tls-busy")
            first = stack.client.open_events("s-tls-busy")
            second = stack.client.open_events("s-tls-busy")
            assert second.status == 409, (second.status, second.detail)
            assert second.code == "body_open"
            assert first.send({"event": "ready"}), "the first body is untouched"
            first.close()
            session.end("the test is over")

    def test_a_volume_goes_out_whole_and_its_sidecar_comes_back(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        archive = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        _cbz(archive, ["000.jpg", "001.jpg", "002.jpg", "Volume 1.webp"])
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER, tls=True,
                    script={"read_archive": True}) as stack:
            session = stack.open_session()
            volume = stack.volume(session, "v1", archive)
            events = _events_until(session, "volume_done")
            (line,) = _archive_line(stack)

        assert events[-1]["pages"] == 3
        assert line["sha256"] == _sha256(archive)
        sidecar = json.loads(volume.output.read_text(encoding="utf-8"))
        assert len(sidecar["pages"]) == 3
        assert sidecar["volume_uuid"] == "uuid-v1"


class TestWhatTheLibraryRecords:
    """The processor's events through the library's REAL session accounting."""

    @staticmethod
    def _library(tmp_path: Path, *stems: str) -> Path:
        library = tmp_path / "library"
        (tmp_path / "inbox").mkdir(parents=True, exist_ok=True)
        for stem in stems:
            _cbz(library / "Alpha" / f"{stem}.cbz", [f"{n:03d}.jpg" for n in range(3)],
                 stored=True)
            # The primary layer is already there, so the SECONDARY row --
            # the one that runs as a session -- is what each volume owes.
            (library / "Alpha" / f"{stem}.mokuro").write_text(
                json.dumps({"version": "0.0", "volume_uuid": f"uuid-{stem}",
                            "pages": [], "chars": 0}),
                encoding="utf-8",
            )
        return library

    @staticmethod
    def _worker(stack: _Stack, tmp_path: Path, *,
                archives_root: str = "/mokuro-reader/") -> Any:
        """The real worker, with this processor as its only hardware."""
        from mokuro_bunko.ocr.generations import parse_generation_list
        from mokuro_bunko.ocr.watcher import OCRWorker

        stack.entry.catalog = {
            "engines": ["mokuro", "hayai-nova"], "detectors": ["ppocr-manga", "ctd"],
            "devices": [], "serves_mokuro": True,
        }
        stack.registry.archives_root = archives_root
        rows = parse_generation_list([
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "hayai-nova", "engine": "hayai-nova"},
        ])
        worker = OCRWorker(
            storage_path=tmp_path, poll_interval=30.0, generations=rows,
            engines_python_path=Path(sys.executable), concurrency=1, sessions=True,
            remote=stack.registry, local_processing=False, autobench=False,
        )
        stack.registry.on_drop = worker.processor_disconnected
        return worker

    @staticmethod
    def _scan(worker: Any) -> None:
        scan = threading.Thread(target=worker._scan_ocr_once, daemon=True)
        scan.start()
        scan.join(timeout=120.0)
        assert not scan.is_alive(), "the scan never finished"

    @staticmethod
    def _failures(tmp_path: Path) -> dict[str, Any]:
        path = tmp_path / ".ocr-failures.json"
        return json.loads(path.read_text(encoding="utf-8")) if path.exists() else {}

    @staticmethod
    def _layers(library: Path, stem: str) -> list[str]:
        return sorted(
            p.name for p in library.rglob("*.mokuro")
            if p.name.startswith(stem) and p.name != f"{stem}.mokuro"
        )

    def test_a_page_damaged_at_the_library_records_nothing(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        library = self._library(tmp_path, "Volume 1", "Volume 2")
        _flip_in_member(library / "Alpha" / "Volume 1.cbz", "001.jpg")
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            worker = self._worker(stack, tmp_path)
            self._scan(worker)
        assert self._failures(tmp_path) == {}
        assert self._layers(library, "Volume 1"), "Volume 1 was processed, blank page and all"
        assert self._layers(library, "Volume 2")

    def test_a_volume_failed_by_damage_at_the_library_is_recorded_naming_the_member(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        library = self._library(tmp_path, "Volume 1", "Volume 2")
        for member in ("000.jpg", "001.jpg", "002.jpg"):
            _flip_in_member(library / "Alpha" / "Volume 1.cbz", member)
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            worker = self._worker(stack, tmp_path)
            self._scan(worker)
        failures = self._failures(tmp_path)
        assert sorted(failures) == ["Alpha/Volume 1.cbz@hayai-nova"], failures
        assert failures["Alpha/Volume 1.cbz@hayai-nova"]["error"] == (
            "every page failed (the library's copy: the same bytes on two downloads; "
            "'000.jpg', '001.jpg', '002.jpg' fail their CRC-32 checks)"
        )
        assert self._layers(library, "Volume 2"), "Volume 2 was processed"

    def test_a_zip_damaged_at_the_library_is_recorded_and_its_neighbour_runs(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        library = self._library(tmp_path, "Volume 1", "Volume 2")
        _break_eocd(library / "Alpha" / "Volume 1.cbz")
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER,
                    script={"read_archive": True}) as stack:
            worker = self._worker(stack, tmp_path)
            self._scan(worker)
        failures = self._failures(tmp_path)
        assert sorted(failures) == ["Alpha/Volume 1.cbz@hayai-nova"], failures
        error = failures["Alpha/Volume 1.cbz@hayai-nova"]["error"]
        assert "BadZipFile" in error and error.endswith(DAMAGED_AT_LIBRARY_NOTE), error
        assert self._layers(library, "Volume 2"), "Volume 2 was processed"
        assert worker._stopped_generations == set() or not any(
            gen for gen, _ in worker._stopped_generations
        ), "the row was not stopped"

    def test_a_download_that_fails_one_scan_is_unrecorded_and_runs_the_next(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        library = self._library(tmp_path, "Volume 1", "Volume 2")
        faults = Faults()
        faults.by_name["Volume 1.cbz"] = {"cut_at": 0}
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER, faults=faults,
                    script={"read_archive": True}) as stack:
            sent: list[dict[str, Any]] = []
            real_send = stack.entry.send

            def send(op: dict[str, Any]) -> bool:
                sent.append(dict(op))
                return real_send(op)

            monkeypatch.setattr(stack.entry, "send", send)
            worker = self._worker(stack, tmp_path)
            self._scan(worker)
            first_scan = [op for op in sent if op["op"] == "volume"]
            assert self._failures(tmp_path) == {}
            assert self._layers(library, "Volume 2"), "Volume 2 ran in the first scan"
            assert not self._layers(library, "Volume 1")
            assert [op["archive"].endswith("Volume 1.cbz") for op in first_scan].count(True) == 1, (
                "one claim, one budget: never a second claim in the same scan"
            )
            faults.by_name.clear()
            self._scan(worker)
        assert self._failures(tmp_path) == {}
        assert self._layers(library, "Volume 1"), "Volume 1 ran in the second scan"

    def test_a_processor_that_404s_everything_is_held_and_nothing_recorded(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A misrouted processor (a wrong archives root): its returns count
        against IT, never against the jobs -- it never proved its path -- and
        three in a row hold it. No record, no strike."""
        self._library(tmp_path, "Volume 1", "Volume 2", "Volume 3", "Volume 4")
        with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER) as stack:
            worker = self._worker(stack, tmp_path, archives_root="/wrong-root/")
            self._scan(worker)
            breaker = worker._breakers.get(stack.entry.processor_id)
            held = worker.connected_machines()
        assert self._failures(tmp_path) == {}
        assert breaker is not None and breaker.open_until > time.time(), "held"
        assert breaker.consecutive >= 3 and not breaker.proven
        assert all(returns.count == 0 for returns in worker._download_returns.values())
        assert worker._session_strikes == {}
        assert held[0]["held"] == "downloads"


class TestTheCommand:
    def test_serve_gives_up_on_a_refused_login(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A wrong password is not a network blip: retrying it forever helps
        nobody, and an operator staring at a service that says nothing is
        worse than one that exits."""
        from cheroot.wsgi import Server as WSGIServer
        from click.testing import CliRunner

        from mokuro_bunko.processor.cli import processor_group
        from mokuro_bunko.processor.status import read_status

        def refuse(environ: dict[str, Any], start_response: Any) -> list[bytes]:
            body = json.dumps({"error": "Invalid credentials"}).encode()
            start_response(
                "401 Unauthorized",
                [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
            )
            return [body]

        server = WSGIServer(("127.0.0.1", 0), refuse, numthreads=2)
        server.prepare()
        serving = threading.Thread(target=server.serve, daemon=True)
        serving.start()
        try:
            storage = tmp_path / "state"
            config_path = tmp_path / "processor.yaml"
            config_path.write_text(
                f"library:\n"
                f"  url: http://127.0.0.1:{server.bind_addr[1]}\n"
                f"  username: tower\n"
                f"  password: hunter2hunter2\n"
                f"processor:\n"
                f"  storage: {storage}\n",
                encoding="utf-8",
            )
            # Short-circuits the device probe, which would otherwise spawn a
            # subprocess to ask an environment this test has not installed.
            monkeypatch.setenv("MOKURO_PROCESSOR_RUNNER", str(FAKE_RUNNER))
            monkeypatch.setenv("MOKURO_PROCESSOR_ENGINES_PYTHON", sys.executable)
            result = CliRunner().invoke(
                processor_group, ["serve", "--config", str(config_path)]
            )
        finally:
            server.stop()
            serving.join(timeout=10.0)

        assert result.exit_code == 1, result.output
        assert "Login refused" in result.output
        assert read_status(storage)["state"] == "refused"
