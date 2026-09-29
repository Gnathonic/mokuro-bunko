"""A session the library closes says nothing about the volume it cut short (I6).

The library closes a session when it shuts down (`OCRWorker.stop`) or ends
one itself. A volume whose archive is still DOWNLOADING at that moment -- a
slow or distant link, the feeder waiting on the network -- never reached the
runner: the bridge aborts the download and says nothing about it, and the
library settles its own claim. (It used to be the runner's volume, fed page by page,
cut and reported failed, and recorded against a volume that did nothing
wrong.)
"""

from __future__ import annotations

import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pytest

from tests.unit import test_processor_bridge as bridge_tests
from tests.unit.test_processor_bridge import FAKE_RUNNER, _cbz, _stack


def _stalling_archives(real: Callable[[Path], Callable[..., Any]], root: Path,
                       started: threading.Event, release: threading.Event
                       ) -> Callable[..., Any]:
    """The archives, with a full read of the body stalling after its first
    bytes until ``release`` -- the feeder then waits on the network with
    pages already in the runner, which is the moment the close must find."""
    serve = real(root)

    def app(environ: dict[str, Any], start_response: Callable[..., Any]) -> Any:
        body = serve(environ, start_response)
        raw_range = str(environ.get("HTTP_RANGE") or "")
        whole = environ.get("REQUEST_METHOD") == "GET" and (
            not raw_range or raw_range.endswith("-")
        )
        if not whole:
            return body

        def stall() -> Any:
            data = b"".join(body)
            first = data[:600]
            yield first
            started.set()
            release.wait(timeout=60)
            yield data[len(first):]

        return stall()

    return app


def test_a_close_while_a_volume_streams_reports_no_failure_for_it(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    started = threading.Event()
    release = threading.Event()
    real = bridge_tests._archives_app
    monkeypatch.setattr(
        bridge_tests, "_archives_app",
        lambda root, faults=None: _stalling_archives(real, root, started, release),
    )
    with _stack(tmp_path, monkeypatch, runner=FAKE_RUNNER, script={"pages": 6}) as stack:
        try:
            seen = _close_mid_stream(stack, started)
        finally:
            release.set()
    kinds = [event.get("event") for event in seen]
    assert "exit" in kinds, f"the session never ended: {kinds}"
    assert not any(
        event.get("event") == "volume_failed" and event.get("id") == "c1" for event in seen
    ), seen
    assert "fatal" not in kinds, seen


def _close_mid_stream(stack: Any, started: threading.Event) -> list[dict[str, Any]]:
    session = stack.open_session("s1")
    archive = stack.library_path / "Alpha" / "Volume 1.cbz"
    _cbz(archive, [f"{n:03d}.jpg" for n in range(20)])
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        event = session.poll_event(timeout=0.5)
        if event is not None and event.get("event") == "ready":
            break
    stack.volume(session, "c1", archive)
    assert started.wait(timeout=30), "the archive never started arriving"
    time.sleep(0.5)  # the feeder is now waiting on the network mid-archive
    session.close()
    seen: list[dict[str, Any]] = []
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        event = session.poll_event(timeout=0.5)
        if event is None:
            continue
        seen.append(event)
        if event.get("event") == "exit":
            break
    return seen
