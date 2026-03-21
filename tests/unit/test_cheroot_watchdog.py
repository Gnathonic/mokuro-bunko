"""Tests for cheroot thread pool watchdog."""

from __future__ import annotations

import time

from mokuro_bunko.cheroot_watchdog import ThreadPoolWatchdog


class FakeThread:
    """Fake thread for testing."""

    def __init__(self, alive: bool = True) -> None:
        self._alive = alive

    def is_alive(self) -> bool:
        return self._alive


class FakePool:
    """Fake cheroot ThreadPool for testing."""

    def __init__(self, min: int = 10) -> None:
        self.min = min
        self._threads: list[FakeThread] = []
        self._grow_calls: list[int] = []

    def _clear_dead_threads(self) -> None:
        self._threads = [t for t in self._threads if t.is_alive()]

    def grow(self, amount: int) -> None:
        self._grow_calls.append(amount)
        for _ in range(amount):
            self._threads.append(FakeThread(alive=True))


class FakeServer:
    """Fake cheroot server for testing."""

    def __init__(self) -> None:
        self.ready = True
        self.interrupt = None
        self.requests = FakePool()


class TestThreadPoolWatchdog:
    """Tests for ThreadPoolWatchdog."""

    def test_detects_and_replaces_dead_threads(self) -> None:
        """Test watchdog detects dead threads and replaces them."""
        server = FakeServer()
        # Start with 10 alive threads
        server.requests._threads = [FakeThread(alive=True) for _ in range(10)]

        watchdog = ThreadPoolWatchdog(server)

        # Kill 3 threads
        for i in range(3):
            server.requests._threads[i]._alive = False

        # Run a check
        watchdog._check_and_recover()

        # Should have called grow(3) to replace the dead threads
        assert server.requests._grow_calls == [3]
        assert watchdog.deaths_recovered == 3

    def test_no_action_when_all_alive(self) -> None:
        """Test watchdog does nothing when all threads are alive."""
        server = FakeServer()
        server.requests._threads = [FakeThread(alive=True) for _ in range(10)]

        watchdog = ThreadPoolWatchdog(server)
        watchdog._check_and_recover()

        assert server.requests._grow_calls == []
        assert watchdog.deaths_recovered == 0

    def test_clears_server_interrupt(self) -> None:
        """Test watchdog clears server interrupt flag set by dying thread."""
        server = FakeServer()
        server.requests._threads = [FakeThread(alive=True) for _ in range(10)]
        server.interrupt = OSError("WinError 10022")

        watchdog = ThreadPoolWatchdog(server)
        watchdog._check_and_recover()

        assert server.interrupt is None

    def test_replaces_all_dead_threads(self) -> None:
        """Test watchdog handles all threads being dead."""
        server = FakeServer()
        server.requests._threads = [FakeThread(alive=False) for _ in range(10)]

        watchdog = ThreadPoolWatchdog(server)
        watchdog._check_and_recover()

        assert server.requests._grow_calls == [10]
        assert watchdog.deaths_recovered == 10

    def test_start_stop(self) -> None:
        """Test watchdog can be started and stopped cleanly."""
        server = FakeServer()
        server.requests._threads = [FakeThread(alive=True) for _ in range(10)]

        watchdog = ThreadPoolWatchdog(server)
        watchdog.start()
        time.sleep(0.1)  # Let it start
        watchdog.stop()
        # Should not hang or raise
