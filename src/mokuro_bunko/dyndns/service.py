"""Background DynDNS updater service."""

from __future__ import annotations

import threading
import time
import urllib.request
from typing import Any, Optional

from mokuro_bunko.config import DynDNSConfig


class DynDNSService:
    """Background daemon thread for periodic DNS updates."""

    def __init__(self, config: DynDNSConfig) -> None:
        self._config = config
        self._thread: Optional[threading.Thread] = None
        self._stop_event = threading.Event()
        self._last_update: Optional[str] = None
        self._last_ip: Optional[str] = None
        self._last_error: Optional[str] = None
        self._running = False

    def start(self) -> None:
        """Start the background update thread."""
        if self._running:
            return
        self._stop_event.clear()
        self._running = True
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        """Stop the background update thread."""
        self._running = False
        self._stop_event.set()
        if self._thread:
            self._thread.join(timeout=5)
            self._thread = None

    def update_now(self) -> dict[str, Any]:
        """Force an immediate DNS update and return the result."""
        return self._do_update()

    def status(self) -> dict[str, Any]:
        """Return service status."""
        return {
            "enabled": self._config.enabled,
            "running": self._running,
            "provider": self._config.provider,
            "domain": self._config.domain,
            "last_update": self._last_update,
            "last_ip": self._last_ip,
            "last_error": self._last_error,
        }

    def configure(self, config: DynDNSConfig) -> None:
        """Update configuration at runtime."""
        was_running = self._running
        if was_running:
            self.stop()
        self._config = config
        if was_running and config.enabled:
            self.start()

    def _run(self) -> None:
        """Background loop: update DNS at configured interval."""
        while not self._stop_event.is_set():
            self._do_update()
            self._stop_event.wait(self._config.interval)
        self._running = False

    def _do_update(self) -> dict[str, Any]:
        """Perform a single DNS update."""
        try:
            ip = self._get_public_ip()
            self._last_ip = ip

            if self._config.provider == "duckdns":
                result = self._update_duckdns(ip)
            else:
                result = self._update_generic(ip)

            self._last_update = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
            self._last_error = None
            return {"success": True, "ip": ip, **result}
        except Exception as e:
            self._last_error = str(e)
            return {"success": False, "error": str(e)}

    def _get_public_ip(self) -> str:
        """Get public IP via ipify."""
        req = urllib.request.Request("https://api.ipify.org", method="GET")
        with urllib.request.urlopen(req, timeout=10) as resp:
            return resp.read().decode("utf-8").strip()

    def _update_duckdns(self, ip: str) -> dict[str, Any]:
        """Update DuckDNS record."""
        domain = self._config.domain
        # Strip .duckdns.org suffix if present
        if domain.endswith(".duckdns.org"):
            domain = domain[: -len(".duckdns.org")]

        url = (
            f"https://www.duckdns.org/update"
            f"?domains={domain}&token={self._config.token}&ip={ip}"
        )
        req = urllib.request.Request(url, method="GET")
        with urllib.request.urlopen(req, timeout=10) as resp:
            body = resp.read().decode("utf-8").strip()
        if body != "OK":
            raise RuntimeError(f"DuckDNS update failed: {body}")
        return {"response": body}

    def _update_generic(self, ip: str) -> dict[str, Any]:
        """Update generic DynDNS provider."""
        url = self._config.update_url
        if not url:
            raise RuntimeError("No update_url configured for generic provider")

        url = url.replace("{ip}", ip)
        url = url.replace("{domain}", self._config.domain)
        url = url.replace("{token}", self._config.token)

        req = urllib.request.Request(url, method="GET")
        with urllib.request.urlopen(req, timeout=10) as resp:
            body = resp.read().decode("utf-8").strip()
        return {"response": body}
