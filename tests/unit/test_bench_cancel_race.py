"""A cancelled benchmark ends ``cancelled``, whichever path notices first.

Cancelling a running benchmark kills its runner. The read loop looks at the
cancel flag only between events, so when it is blocked waiting for the next
one, the runner's own ``exit`` -- no result yet -- is what it reads first,
and that path settled the run as ``failed`` ("the benchmark ended before it
produced a result"). Seen on CI, where the loop is usually mid-wait: a person
who pressed Cancel was told the benchmark failed.
"""

from __future__ import annotations

import queue
import threading
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock

from mokuro_bunko.ocr.bench import BenchService, _BenchRun
from mokuro_bunko.ocr.generations import parse_generation_list


class _Session:
    """A runner whose death is an event on its own stream, like the real one."""

    def __init__(self) -> None:
        self.events: queue.Queue[dict[str, Any]] = queue.Queue()
        self.waiting = threading.Event()

    def start(self) -> bool:
        return True

    def poll_event(self, timeout: float = 1.0) -> dict[str, Any] | None:
        self.waiting.set()
        try:
            return self.events.get(timeout=timeout)
        except queue.Empty:
            return None

    def kill(self) -> None:
        self.events.put({"event": "exit", "returncode": -9})

    def wait(self, timeout: float = 0.0) -> None:
        return None

    def stderr_tail(self) -> str:
        return ""


def test_a_cancel_that_lands_mid_wait_is_a_cancel(tmp_path: Path) -> None:
    (tmp_path / "library").mkdir()
    row = parse_generation_list(
        [{"name": "mokuro", "engine": "mokuro", "primary": True},
         {"name": "hayai-nova", "engine": "hayai-nova"}]
    )[1]
    service = BenchService(tmp_path, worker=lambda: None, generations=lambda: [row])
    run = _BenchRun(row.id, row, 8, draft=False, spec={})
    session = _Session()
    run.session = session

    reader = threading.Thread(
        target=service._read_composed,
        args=(run, session, MagicMock(), 0.0, MagicMock()),
    )
    reader.start()
    assert session.waiting.wait(timeout=5.0), "the loop never waited for an event"
    run.cancel()
    reader.join(timeout=10.0)

    assert not reader.is_alive()
    assert run.data["state"] == "cancelled", run.data.get("error")


def test_a_run_nobody_cancelled_still_fails_when_its_runner_dies(tmp_path: Path) -> None:
    (tmp_path / "library").mkdir()
    row = parse_generation_list(
        [{"name": "mokuro", "engine": "mokuro", "primary": True},
         {"name": "hayai-nova", "engine": "hayai-nova"}]
    )[1]
    service = BenchService(tmp_path, worker=lambda: None, generations=lambda: [row])
    run = _BenchRun(row.id, row, 8, draft=False, spec={})
    session = _Session()
    session.kill()  # it died on its own

    service._read_composed(run, session, MagicMock(), 0.0, MagicMock())

    assert run.data["state"] == "failed"
