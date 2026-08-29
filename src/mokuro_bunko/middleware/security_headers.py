"""Security headers middleware for mokuro-bunko.

Adds hardening response headers centrally instead of repeating them in every
API handler. The "safe" headers below are added to every response (harmless on
JSON, HTML, WebDAV, and file downloads). ``Cache-Control: no-store`` is added
only to JSON responses, so API payloads aren't cached while library file
downloads and static assets remain cacheable. Any header a downstream handler
has already set is left untouched.

Image responses (page scans, `.webp`/`.jpg` cover sidecars) additionally get a
``Cache-Control: private, max-age=...`` header. wsgidav already emits
ETag/Last-Modified on these GET responses, so browsers revalidate correctly;
the missing piece was permission to actually cache between revalidations.
"private" because these are served behind per-user WebDAV auth and must never
land in a shared/proxy cache. This deliberately does not special-case `.webp`
covers versus other page images -- it keys off Content-Type, which covers
both. Compiled metadata JSON (`series.json`/`catalog.json`) must stay
revalidated-fresh, so it keeps the `no-store` behavior above instead.
"""

from __future__ import annotations

from collections.abc import Callable, Iterable
from typing import Any, cast

# Applied to every response.
_SAFE_HEADERS: list[tuple[str, str]] = [
    ("X-Content-Type-Options", "nosniff"),
    ("X-Frame-Options", "DENY"),
    ("Referrer-Policy", "no-referrer"),
    ("X-XSS-Protection", "1; mode=block"),
    # A bunko instance is a personal library, not a website to index: tell
    # search engines to keep every page and file out (robots.txt handles the
    # polite-crawler half; this covers content they already fetched).
    ("X-Robots-Tag", "noindex, nofollow"),
]

# One day: long enough for a real cache hit rate on manga page images and
# cover thumbnails (both are effectively immutable once written), short
# enough that a re-imported/re-processed volume's ETag change is noticed
# again soon after.
_IMAGE_CACHE_CONTROL = "private, max-age=86400"


class SecurityHeadersMiddleware:
    """WSGI middleware that injects hardening headers into responses."""

    def __init__(self, app: Callable[..., Iterable[bytes]]) -> None:
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
            bare_content_type = content_type.split(";", 1)[0].strip().lower()
            is_json = bare_content_type == "application/json"
            if is_json and "cache-control" not in present:
                new_headers.append(("Cache-Control", "no-store"))

            is_image = bare_content_type.startswith("image/")
            if is_image and "cache-control" not in present:
                new_headers.append(("Cache-Control", _IMAGE_CACHE_CONTROL))

            return cast(
                "Callable[[bytes], None]",
                start_response(status, new_headers, exc_info),
            )

        return self.app(environ, secure_start_response)
