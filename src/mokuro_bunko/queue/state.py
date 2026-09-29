"""The queue page's state version: one number that moves whenever the page would.

The queue page polls about once a second, and nearly every one of those polls
finds nothing new. So the status endpoint does not recompute anything to
answer it: every change the page shows -- a claim, a page coming out of a
running volume, a volume done or failed, the pending list changing, a queue
setting saved -- bumps this counter, the serialized payload is cached against
it, and a poll that already holds the current body's ETag is answered ``304``
with no body (see `QueueAPI`). Nothing that leaves the page as it was may bump
it: every bump costs a rebuild.

Monotonic for the life of the process. The ETag is a hash of the built body,
not this number, so a rebuild that changes nothing is still a 304.
"""

from __future__ import annotations

import secrets
import threading


class QueueStateVersion:
    """A thread-safe, monotonically increasing counter."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._value = 0
        self.epoch = secrets.token_hex(4)

    @property
    def value(self) -> int:
        return self._value

    def bump(self) -> int:
        with self._lock:
            self._value += 1
            return self._value
