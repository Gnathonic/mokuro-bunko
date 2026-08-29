"""PropfindCacheMiddleware's debounced-refresh timer lifecycle (F5, final review)."""

from __future__ import annotations

import time
from typing import Any

from mokuro_bunko.middleware.propfind_cache import PropfindCacheMiddleware


def _stub_app(environ: dict[str, Any], start_response: Any) -> list[bytes]:
    start_response("200 OK", [])
    return [b""]


class TestStoppedGate:
    """Before this fix, `stop()` could only cancel a timer that already
    existed AT THAT MOMENT. A `schedule_refresh` call arriving after
    `stop()` had already run -- a late `MetadataService.on_published` hook
    firing after `MetadataService.stop()` has returned, in particular, per
    `run_server`'s documented shutdown ordering -- armed a fresh timer
    nothing was left to ever cancel. Verified directly here without going
    through `__call__`, matching the reviewer's reproduction:
    `cache.stop(); cache.schedule_refresh(30)` used to arm a new timer.
    """

    def test_stop_cancels_a_pending_refresh_timer(self) -> None:
        cache = PropfindCacheMiddleware(_stub_app)
        cache.schedule_refresh(delay=30.0)
        assert cache._debounce_timer is not None

        cache.stop()

        assert cache._debounce_timer is None

    def test_schedule_refresh_after_stop_does_not_arm_a_new_timer(self) -> None:
        cache = PropfindCacheMiddleware(_stub_app)
        cache.stop()

        cache.schedule_refresh(delay=30.0)

        assert cache._debounce_timer is None

    def test_a_refresh_already_in_flight_when_stop_is_called_still_settles(self) -> None:
        """A timer whose wait already elapsed fires `_debounced_fire` on its
        own thread regardless of `stop()`; `stop()` itself must still
        return cleanly (no live timer left to join, nothing to cancel)."""
        cache = PropfindCacheMiddleware(_stub_app)
        cache.schedule_refresh(delay=0.02)
        time.sleep(0.2)

        cache.stop()  # must not raise

        assert cache._debounce_timer is None
        assert cache._stopped is True
