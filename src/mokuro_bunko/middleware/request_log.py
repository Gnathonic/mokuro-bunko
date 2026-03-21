"""Request logging middleware for mokuro-bunko.

Logs each HTTP request with method, path, status code, and timing.
Enabled by setting the MOKURO_DEBUG environment variable.
"""

from __future__ import annotations

import os
import sys
import threading
import time
from collections.abc import Callable, Iterable
from typing import Any


def _is_enabled() -> bool:
    return os.environ.get("MOKURO_DEBUG", "").strip() not in ("", "0", "false")


class RequestLogMiddleware:
    """WSGI middleware that logs every request with timing and status."""

    def __init__(self, app: Callable[..., Any]) -> None:
        self.app = app
        self._enabled = _is_enabled()

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        if not self._enabled:
            return self.app(environ, start_response)

        method = environ.get("REQUEST_METHOD", "?")
        path = environ.get("PATH_INFO", "/")
        thread_name = threading.current_thread().name
        t0 = time.monotonic()
        status_holder: list[str] = []

        def logging_start_response(
            status: str,
            headers: list[tuple[str, str]],
            exc_info: Any = None,
        ) -> Any:
            status_holder.append(status)
            return start_response(status, headers, exc_info)

        try:
            result = self.app(environ, logging_start_response)
            elapsed = time.monotonic() - t0
            status = status_holder[0] if status_holder else "???"
            code = status.split(" ", 1)[0]
            _log(f"[{thread_name}] {method} {path} -> {code} ({elapsed:.3f}s)")
            return result
        except Exception as exc:
            elapsed = time.monotonic() - t0
            _log(
                f"[{thread_name}] {method} {path} -> EXCEPTION "
                f"({elapsed:.3f}s): {type(exc).__name__}: {exc}"
            )
            raise


def _log(msg: str) -> None:
    print(f"[REQUEST] {msg}", file=sys.stderr, flush=True)
