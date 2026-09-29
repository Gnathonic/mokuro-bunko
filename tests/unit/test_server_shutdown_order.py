"""F3 regression (Task 10 review): `run_server`'s shutdown order.

`MetadataService.stop()` does not wait for a just-finished pass's deferred
`on_published()` call: per `service.py`'s own re-entrancy design (Task 9,
unchanged here), `_published()` runs in the pass's own thread but AFTER
`_pass_lock` is released, so `stop()`'s "acquire-then-release `_pass_lock`"
wait can return before that thread reaches `on_published()`. If
`_propfind_cache.stop()` already ran by then, the freshly armed refresh
timer that `on_published` -> `on_metadata_published` -> `schedule_refresh`
schedules is never cancelled by anything.

Stopping `_metadata_service` FIRST — before `_propfind_cache` — shrinks that
window from "always missed" (cache already stopped, nothing left to catch a
late timer) to "requires a same-instant race" (cache stop still running
after the late timer arms). This is a live-thread race that isn't reliably
reproducible in a unit test, so this pins the SOURCE ORDER of the two
`.stop()` calls in `shutdown_app` -- which `run_server`'s `finally` block
calls -- instead.

`shutdown_app` itself is exercised on a real app below: nothing it armed
may outlive it.
"""

from __future__ import annotations

import inspect
import threading
import time
from pathlib import Path

from mokuro_bunko.config import Config, StorageConfig
from mokuro_bunko.server import create_app, run_server, shutdown_app


def test_run_server_shuts_the_app_down() -> None:
    assert "shutdown_app(server.wsgi_app)" in inspect.getsource(run_server)


def test_metadata_service_stops_before_propfind_cache() -> None:
    source = inspect.getsource(shutdown_app)
    metadata_stop = source.index("_metadata_service.stop()")
    propfind_stop = source.index("_propfind_cache.stop()")
    assert metadata_stop < propfind_stop, (
        "metadata_service.stop() must run BEFORE propfind_cache.stop() in "
        "shutdown_app -- swapping this order re-opens the "
        "on_published-after-stop race (Task 10 review F3)."
    )


def test_nothing_the_app_armed_outlives_shutdown(temp_dir: Path) -> None:
    before = set(threading.enumerate())
    app = create_app(Config(storage=StorageConfig(base_path=temp_dir)), temp_dir / "c.yaml")
    armed = [t for t in threading.enumerate() if t not in before]
    # The 20 s first metadata pass and the 6 h rescan are timers, at least.
    assert sum(isinstance(t, threading.Timer) for t in armed) >= 2
    shutdown_app(app)
    shutdown_app(app)  # a second call is harmless
    deadline = time.monotonic() + 10.0
    while time.monotonic() < deadline and any(t.is_alive() for t in armed):
        time.sleep(0.05)
    assert [t.name for t in armed if t.is_alive()] == []
