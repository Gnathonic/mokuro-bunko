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
`.stop()` calls in `run_server`'s `finally` block instead.
"""

from __future__ import annotations

import inspect

from mokuro_bunko.server import run_server


def test_metadata_service_stops_before_propfind_cache() -> None:
    source = inspect.getsource(run_server)
    metadata_stop = source.index("_metadata_service.stop()")
    propfind_stop = source.index("_propfind_cache.stop()")
    assert metadata_stop < propfind_stop, (
        "metadata_service.stop() must run BEFORE propfind_cache.stop() in "
        "run_server's finally block -- swapping this order re-opens the "
        "on_published-after-stop race (Task 10 review F3)."
    )
