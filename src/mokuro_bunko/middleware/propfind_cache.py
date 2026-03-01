"""PROPFIND response cache with gzip compression and stale-while-revalidate."""

from __future__ import annotations

import gzip
import sys
import threading
import time
import traceback
from io import BytesIO
from typing import Any, Callable, Iterable, Optional


class PropfindCacheMiddleware:
    """Cache PROPFIND Depth:infinity responses and serve them gzip-compressed.

    Uses stale-while-revalidate so users never block on regeneration after
    the first cold load.  Write operations invalidate the cache immediately.

    Lifecycle for a cached entry:
        age < ttl          → fresh, serve immediately
        ttl ≤ age < stale  → stale, serve immediately + background refresh
        age ≥ stale        → expired, block and regenerate
    """

    def __init__(
        self,
        app: Callable[..., Any],
        ttl: float = 120.0,
        stale_ttl: float = 86400.0,
        max_entries: int = 10,
    ) -> None:
        self.app = app
        self.ttl = ttl
        self.stale_ttl = stale_ttl
        self.max_entries = max_entries
        self._cache: dict[str, dict[str, Any]] = {}
        self._lock = threading.Lock()
        self._refreshing: set[str] = set()
        self._debounce_timer: Optional[threading.Timer] = None

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        method = environ.get("REQUEST_METHOD", "")

        # Invalidate cache on write operations (serve stale, refresh in background)
        if method in ("PUT", "DELETE", "MKCOL", "MOVE", "COPY"):
            self.refresh_all()
            return self.app(environ, start_response)

        # Only cache PROPFIND with Depth: infinity
        if method != "PROPFIND":
            return self.app(environ, start_response)

        depth = environ.get("HTTP_DEPTH", "0")
        if depth != "infinity":
            return self.app(environ, start_response)

        path = environ.get("PATH_INFO", "")
        cache_key = path
        accepts_gzip = "gzip" in environ.get("HTTP_ACCEPT_ENCODING", "")

        now = time.monotonic()
        with self._lock:
            entry = self._cache.get(cache_key)

        if entry:
            age = now - entry["time"]
            if age < self.ttl:
                # Fresh — serve immediately
                return self._serve_cached(entry, accepts_gzip, start_response)
            if age < self.stale_ttl:
                # Stale — serve immediately, refresh in background
                self._trigger_refresh(cache_key, environ)
                return self._serve_cached(entry, accepts_gzip, start_response)

        # No cache or too old — block and generate
        return self._generate_and_serve(
            cache_key, environ, accepts_gzip, start_response,
        )

    def _generate_and_serve(
        self,
        cache_key: str,
        environ: dict[str, Any],
        accepts_gzip: bool,
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Generate response from WsgiDAV, cache it, and serve."""
        new_entry = self._generate(environ)
        if new_entry is None:
            # Non-207 response — don't cache, pass through
            return self._passthrough(environ, start_response)

        with self._lock:
            if len(self._cache) >= self.max_entries and cache_key not in self._cache:
                oldest = min(self._cache, key=lambda k: self._cache[k]["time"])
                del self._cache[oldest]
            self._cache[cache_key] = new_entry

        return self._serve_cached(new_entry, accepts_gzip, start_response)

    def _generate(self, environ: dict[str, Any]) -> Optional[dict[str, Any]]:
        """Call WsgiDAV and capture the full response."""
        captured: dict[str, Any] = {}

        def buffer_start_response(
            status: str,
            headers: list[tuple[str, str]],
            exc_info: Any = None,
        ) -> Callable[[bytes], None]:
            captured["status"] = status
            captured["headers"] = headers
            return lambda s: None

        body_iter = self.app(environ, buffer_start_response)
        raw_body = b"".join(body_iter)
        if hasattr(body_iter, "close"):
            body_iter.close()

        status: str = captured.get("status", "")
        if not status.startswith("207"):
            return None

        gzip_body = gzip.compress(raw_body, compresslevel=6)

        return {
            "status": captured["status"],
            "headers": captured["headers"],
            "raw_body": raw_body,
            "gzip_body": gzip_body,
            "time": time.monotonic(),
        }

    def _passthrough(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        """Forward request to WsgiDAV without caching."""
        return self.app(environ, start_response)

    def _trigger_refresh(self, cache_key: str, environ: dict[str, Any]) -> None:
        """Spawn a background thread to refresh this cache entry."""
        with self._lock:
            if cache_key in self._refreshing:
                return
            self._refreshing.add(cache_key)

        # Build a minimal environ copy for the background thread
        bg_environ = self._copy_environ(environ)

        def refresh() -> None:
            try:
                new_entry = self._generate(bg_environ)
                if new_entry:
                    with self._lock:
                        self._cache[cache_key] = new_entry
            finally:
                with self._lock:
                    self._refreshing.discard(cache_key)

        t = threading.Thread(target=refresh, daemon=True, name="propfind-refresh")
        t.start()

    def warm(self, path: str = "/mokuro-reader/") -> None:
        """Pre-populate the cache (call from server startup)."""
        environ = self._make_warm_environ(path)

        def do_warm() -> None:
            try:
                new_entry = self._generate(environ)
                if new_entry:
                    with self._lock:
                        self._cache[path] = new_entry
                    size_raw = len(new_entry["raw_body"])
                    size_gz = len(new_entry["gzip_body"])
                    elapsed = time.monotonic() - new_entry["time"]
                    print(
                        f"[PROPFIND-CACHE] Warmed {path}: "
                        f"{size_raw / 1024 / 1024:.1f}MB raw, "
                        f"{size_gz / 1024:.0f}KB gzip, "
                        f"{elapsed:.1f}s",
                        file=sys.stderr, flush=True,
                    )
                else:
                    print(
                        f"[PROPFIND-CACHE] Warm failed: _generate returned None",
                        file=sys.stderr, flush=True,
                    )
            except Exception:
                traceback.print_exc(file=sys.stderr)
                sys.stderr.flush()

        t = threading.Thread(target=do_warm, daemon=True, name="propfind-warm")
        t.start()

    @staticmethod
    def _copy_environ(environ: dict[str, Any]) -> dict[str, Any]:
        """Copy the WSGI environ for use in a background thread."""
        bg = {}
        for key, value in environ.items():
            if isinstance(value, (str, int, bool, float, bytes)):
                bg[key] = value
        # Provide a dummy wsgi.input
        bg["wsgi.input"] = BytesIO(b"")
        bg["wsgi.errors"] = environ.get("wsgi.errors")
        return bg

    @staticmethod
    def _make_warm_environ(path: str) -> dict[str, Any]:
        """Create a synthetic WSGI environ for cache warming."""
        return {
            "REQUEST_METHOD": "PROPFIND",
            "PATH_INFO": path,
            "SCRIPT_NAME": "",
            "QUERY_STRING": "",
            "CONTENT_TYPE": "application/xml",
            "CONTENT_LENGTH": "0",
            "HTTP_DEPTH": "infinity",
            "HTTP_ACCEPT_ENCODING": "gzip",
            "HTTP_HOST": "localhost:8080",
            "SERVER_NAME": "localhost",
            "SERVER_PORT": "8080",
            "SERVER_PROTOCOL": "HTTP/1.1",
            "wsgi.version": (1, 0),
            "wsgi.input": BytesIO(b""),
            "wsgi.errors": BytesIO(),
            "wsgi.url_scheme": "http",
            "wsgi.multithread": True,
            "wsgi.multiprocess": False,
            "wsgi.run_once": False,
        }

    @staticmethod
    def _serve_cached(
        entry: dict[str, Any],
        accepts_gzip: bool,
        start_response: Callable[..., Any],
    ) -> list[bytes]:
        """Serve a cached response, with gzip if the client supports it."""
        status = entry["status"]
        headers = [
            (k, v)
            for k, v in entry["headers"]
            if k.lower() not in (
                "content-length", "content-encoding", "transfer-encoding",
            )
        ]
        headers.append(("Vary", "Accept-Encoding"))

        if accepts_gzip:
            body = entry["gzip_body"]
            headers.append(("Content-Encoding", "gzip"))
        else:
            body = entry["raw_body"]

        headers.append(("Content-Length", str(len(body))))
        start_response(status, headers)
        return [body]

    def refresh_all(self) -> None:
        """Background-refresh all cached entries (stale entries remain available)."""
        with self._lock:
            keys = list(self._cache.keys())
        for key in keys:
            environ = self._make_warm_environ(key)
            self._trigger_refresh(key, environ)

    def schedule_refresh(self, delay: float = 5.0) -> None:
        """Debounced refresh: resets the timer on each call, fires after *delay* seconds of quiet."""
        with self._lock:
            if self._debounce_timer is not None:
                self._debounce_timer.cancel()
            self._debounce_timer = threading.Timer(delay, self._debounced_fire)
            self._debounce_timer.daemon = True
            self._debounce_timer.start()

    def _debounced_fire(self) -> None:
        """Called when the debounce timer expires."""
        print("[PROPFIND-CACHE] Debounced refresh triggered", file=sys.stderr, flush=True)
        self.refresh_all()

    def stop(self) -> None:
        """Cancel any pending debounce timer (for shutdown)."""
        with self._lock:
            if self._debounce_timer is not None:
                self._debounce_timer.cancel()
                self._debounce_timer = None

    def invalidate(self) -> None:
        """Clear all cached entries."""
        with self._lock:
            self._cache.clear()
