"""Cloudflare tunnel subprocess management."""

from __future__ import annotations

import atexit
import re
import shutil
import subprocess
import threading

from mokuro_bunko.config import Config


class TunnelService:
    """Manages cloudflared subprocess lifecycle."""

    def __init__(self, config: Config, config_path: object | None = None) -> None:
        self._config = config
        self._config_path = config_path
        self._process: subprocess.Popen[str] | None = None
        self._url: str | None = None
        self._lock = threading.Lock()
        self._reader_thread: threading.Thread | None = None
        atexit.register(self.stop)

    def start(self, port: int | None = None) -> None:
        """Start a cloudflare quick tunnel."""
        with self._lock:
            if self._process and self._process.poll() is None:
                return  # Already running

            if not self.available:
                raise RuntimeError("cloudflared is not installed")

            if port is None:
                port = self._config.server.port

            protocol = "https" if self._config.ssl.enabled else "http"
            local_url = f"{protocol}://localhost:{port}"

            cmd = ["cloudflared", "tunnel", "--url", local_url]
            self._url = None
            self._process = subprocess.Popen(
                cmd,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
            )

            # Background thread reads stderr for the tunnel URL
            self._reader_thread = threading.Thread(
                target=self._read_stderr, daemon=True
            )
            self._reader_thread.start()

    def stop(self) -> None:
        """Terminate the cloudflared subprocess."""
        with self._lock:
            if self._process and self._process.poll() is None:
                self._process.terminate()
                try:
                    self._process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self._process.kill()
            self._process = None
            self._url = None

    @property
    def available(self) -> bool:
        """Check if cloudflared binary exists."""
        return shutil.which("cloudflared") is not None

    @property
    def status(self) -> dict:
        """Return tunnel status."""
        running = (
            self._process is not None and self._process.poll() is None
        )
        return {
            "running": running,
            "url": self._url if running else None,
            "available": self.available,
        }

    def _read_stderr(self) -> None:
        """Read stderr from cloudflared looking for the tunnel URL."""
        proc = self._process
        if not proc or not proc.stderr:
            return
        for line in iter(proc.stderr.readline, ""):
            if not self._url:
                match = re.search(
                    r"(https://[a-z0-9-]+\.trycloudflare\.com)", line
                )
                if match:
                    self._url = match.group(1)
