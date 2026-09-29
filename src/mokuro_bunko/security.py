"""Security helpers for path containment and authentication throttling."""

from __future__ import annotations

import ipaddress
import threading
import time
from collections import deque
from pathlib import Path


def is_within_path(path: Path, base: Path) -> bool:
    """Return True when path resolves inside base (or equals base)."""
    try:
        return path.resolve().is_relative_to(base.resolve())
    except (OSError, ValueError):
        return False


def safe_resolve_under(base: Path, relative: str) -> Path | None:
    """Resolve a relative path under base, returning None on traversal escape."""
    try:
        candidate = (base / relative).resolve()
    except (OSError, ValueError):
        return None
    if candidate.is_relative_to(base.resolve()):
        return candidate
    return None


_TRUSTED_PROXIES: tuple[ipaddress.IPv4Network | ipaddress.IPv6Network, ...] = ()


def set_trusted_proxies(networks: list[str]) -> None:
    """Networks besides this machine whose proxy headers are believed.

    Installed by `create_app` from ``server.trusted_proxies``. This machine
    (loopback) is always trusted: it is where the container's own nginx
    connects from.
    """
    global _TRUSTED_PROXIES
    _TRUSTED_PROXIES = tuple(ipaddress.ip_network(n, strict=False) for n in networks)


def _is_proxy_peer(value: str) -> bool:
    """A peer whose proxy headers are believed: loopback, or a configured network.

    A private address is NOT enough: a server exposed straight on a LAN sees
    its clients' own private addresses, and they would pick their own
    rate-limit key.
    """
    try:
        address = ipaddress.ip_address(value)
    except ValueError:
        return False
    return address.is_loopback or any(address in net for net in _TRUSTED_PROXIES)


def get_client_ip(environ: dict[str, object]) -> str:
    """The address a request is counted against (rate limits, local-only checks).

    Proxy headers are believed only from a trusted proxy (`_is_proxy_peer`):
    from anyone else they are the client's own words. From a proxy,
    ``X-Real-IP`` (set outright by the shipped nginx and Caddy configs) wins;
    failing that, the RIGHTMOST ``X-Forwarded-For`` entry, which is the one
    the proxy appended -- everything left of it arrived from the client, so
    rotating it must not change the answer.
    """
    remote = str(environ.get("REMOTE_ADDR", "") or "").strip()
    if not _is_proxy_peer(remote):
        return remote
    xreal = str(environ.get("HTTP_X_REAL_IP", "") or "").strip()
    if xreal:
        return xreal
    xff = str(environ.get("HTTP_X_FORWARDED_FOR", "") or "").strip()
    if xff:
        last = xff.split(",")[-1].strip()
        if last:
            return last
    return remote


def is_loopback_ip(value: str) -> bool:
    """Return True for a valid loopback IP address."""
    try:
        return ipaddress.ip_address(value).is_loopback
    except ValueError:
        return False


class AuthAttemptLimiter:
    """Simple in-memory auth attempt limiter per key."""

    def __init__(
        self,
        max_failures: int = 10,
        window_seconds: int = 300,
        block_seconds: int = 900,
    ) -> None:
        self.max_failures = max_failures
        self.window_seconds = window_seconds
        self.block_seconds = block_seconds
        self._failures: dict[str, deque[float]] = {}
        self._blocked_until: dict[str, float] = {}
        self._lock = threading.Lock()

    def allow_attempt(self, key: str) -> tuple[bool, int]:
        """Return whether an attempt is allowed and retry-after seconds."""
        now = time.monotonic()
        with self._lock:
            blocked_until = self._blocked_until.get(key, 0.0)
            if blocked_until > now:
                return False, int(blocked_until - now) + 1

            failures = self._failures.setdefault(key, deque())
            cutoff = now - self.window_seconds
            while failures and failures[0] < cutoff:
                failures.popleft()

            if len(failures) >= self.max_failures:
                block_until = now + self.block_seconds
                self._blocked_until[key] = block_until
                failures.clear()
                return False, self.block_seconds

            return True, 0

    def record_failure(self, key: str) -> None:
        """Record failed auth attempt for key."""
        now = time.monotonic()
        with self._lock:
            failures = self._failures.setdefault(key, deque())
            failures.append(now)

    def record_success(self, key: str) -> None:
        """Reset failure state on successful auth."""
        with self._lock:
            self._failures.pop(key, None)
            self._blocked_until.pop(key, None)
