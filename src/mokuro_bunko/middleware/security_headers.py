"""Security headers middleware for mokuro-bunko.

Adds hardening response headers centrally instead of repeating them in every
API handler. The "safe" headers below are added to every response (harmless on
JSON, HTML, WebDAV, and file downloads). ``Cache-Control: no-store`` is added
only to JSON responses, so API payloads aren't cached while library file
downloads and static assets remain cacheable. Any header a downstream handler
has already set is left untouched.
"""

from __future__ import annotations

from typing import Any, Callable, Iterable

# Applied to every response.
_SAFE_HEADERS: list[tuple[str, str]] = [
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
    ("X-XSS-Protection", "1; mode=block"),
]


class SecurityHeadersMiddleware:
    """WSGI middleware that injects hardening headers into responses."""

    def __init__(self, app: Callable[..., Any]) -> None:
        self.app = app

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        def secure_start_response(
            status: str,
            headers: list[tuple[str, str]],
            exc_info: Any = None,
        ) -> Callable[[bytes], None]:
            present = {name.lower() for name, _ in headers}
            new_headers = list(headers)

            for name, value in _SAFE_HEADERS:
                if name.lower() not in present:
                    new_headers.append((name, value))

            content_type = ""
            for name, value in headers:
                if name.lower() == "content-type":
                    content_type = value
                    break
            is_json = content_type.split(";", 1)[0].strip().lower() == "application/json"
            if is_json and "cache-control" not in present:
                new_headers.append(("Cache-Control", "no-store"))

            return start_response(status, new_headers, exc_info)

        return self.app(environ, secure_start_response)
