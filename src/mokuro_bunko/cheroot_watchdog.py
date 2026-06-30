"""Cheroot thread pool watchdog for mokuro-bunko.

Monitors cheroot's worker thread pool and replaces dead threads.

Cheroot has a known issue where worker threads can die from unhandled
exceptions in socket cleanup code (especially on Windows, where errors like
WinError 10022 occur during socket.shutdown). Once a thread dies, cheroot
does not replace it. If enough threads die, the server stops processing
requests and returns 503 Service Unavailable.

This watchdog periodically checks the thread pool and replaces dead threads,
keeping the server alive despite the cheroot bug.

See: https://github.com/cherrypy/cheroot/issues/375
     https://github.com/cherrypy/cheroot/issues/710
"""

from __future__ import annotations

import logging
import sys
import threading
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from cheroot.wsgi import Server as WSGIServer

logger = logging.getLogger(__name__)

_CHECK_INTERVAL = 5.0  # seconds between health checks


class ThreadPoolWatchdog:
    """Monitors cheroot's thread pool and replaces dead threads."""

    def __init__(self, server: WSGIServer) -> None:
        self._server = server
        self._stop_event = threading.Event()
        self._thread: threading.Thread | None = None
        self._deaths_recovered = 0

    @property
    def deaths_recovered(self) -> int:
        """Total number of dead threads that have been replaced."""
        return self._deaths_recovered

    def start(self) -> None:
        """Start the watchdog background thread."""
        self._thread = threading.Thread(
            target=self._run,
            name="ThreadPoolWatchdog",
            daemon=True,
        )
        self._thread.start()

    def stop(self) -> None:
        """Stop the watchdog."""
        self._stop_event.set()
        if self._thread is not None:
            self._thread.join(timeout=5)

    def _run(self) -> None:
        """Main watchdog loop."""
        # Wait for the server to fully start
        while not self._stop_event.is_set() and not getattr(self._server, "ready", False):
            self._stop_event.wait(0.5)

        while not self._stop_event.is_set():
            self._stop_event.wait(_CHECK_INTERVAL)
            if self._stop_event.is_set():
                break
            try:
                self._check_and_recover()
            except Exception:
                logger.exception("Watchdog check failed")

    def _check_and_recover(self) -> None:
        """Check thread pool health and replace dead threads."""
        pool = self._server.requests

        # Clear cheroot's internal interrupt flag if a worker thread set it.
        # A dying worker sets server.interrupt which causes the serve() loop
        # to exit. We clear it so the server keeps running.
        interrupt = getattr(self._server, "interrupt", None)
        if interrupt is not None:
            _warn(
                f"Server interrupt flag was set by a dying thread: {interrupt!r}. "
                "Clearing it to keep the server running."
            )
            self._server.interrupt = None

        # Count alive vs dead threads
        threads = list(pool._threads)
        alive = [t for t in threads if t.is_alive()]
        dead = [t for t in threads if not t.is_alive()]

        if not dead:
            return

        n_dead = len(dead)
        n_alive = len(alive)
        _warn(
            f"Thread pool: {n_dead} dead, {n_alive} alive "
            f"(min={pool.min}). Replacing dead threads."
        )

        # Remove dead threads from cheroot's list
        pool._clear_dead_threads()

        # Grow back to minimum
        current = len(pool._threads)
        needed = max(pool.min - current, 0)
        if needed > 0:
            pool.grow(needed)
            self._deaths_recovered += needed
            _warn(f"Spawned {needed} replacement worker thread(s).")


def _warn(msg: str) -> None:
    print(f"[WATCHDOG] {msg}", file=sys.stderr, flush=True)
