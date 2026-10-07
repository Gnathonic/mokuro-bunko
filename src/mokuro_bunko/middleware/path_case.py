"""Library paths behave as on NTFS: case-insensitive, case-preserving.

The library has to stay valid on the most restrictive filesystem bunko runs
on. On NTFS (and APFS) `Kingdom/` and `kingdom/` are one folder, so a library
holding both cannot be copied to, synced with, or served from Windows. On a
case-sensitive host they are two folders, and an upload spelled `kingdom/`
used to create the second one beside the first -- the catalog then showed
one of them and hid the other's volumes.

`PathCaseMiddleware` closes that at the edge, before anything else reads the
request path: every segment of a library request path that names something
already on disk is rewritten to the on-disk spelling, so a PUT, MKCOL,
PROPFIND or GET for `kingdom/Vol 80.cbz` lands in (or reads) `Kingdom/`, and
every layer below -- the auth gate's ownership checks, the upload and
metadata middlewares, the DAV provider, the database rows keyed by library
path -- sees one spelling for one file. A segment that names nothing on disk
is left exactly as the client sent it: that is the case-preserving half.

Renaming to fix the case stays possible, as it is on NTFS: a MOVE whose
destination is a case-variant of its own source keeps the destination's
spelling (rewriting it to the source's would make it a move onto itself).
"""

from __future__ import annotations

from collections.abc import Callable, Iterable
from pathlib import Path
from typing import Any
from urllib.parse import quote, unquote, urlparse, urlunparse

from mokuro_bunko.webdav.path_case import LibraryPathCanonicalizer
from mokuro_bunko.webdav.resources import PathMapper

_LIBRARY_PREFIX = f"/{PathMapper.READER_ROOT}/"


class PathCaseMiddleware:
    """Rewrites library request paths (and MOVE/COPY destinations) to on-disk spelling."""

    def __init__(
        self,
        app: Callable[..., Iterable[bytes]],
        library_path: Path,
        *,
        case_sensitive: bool | None = None,
    ) -> None:
        self.app = app
        self.canonicalizer = LibraryPathCanonicalizer(
            library_path, case_sensitive=case_sensitive
        )

    @staticmethod
    def _library_relative(path: str) -> str | None:
        if not path.startswith(_LIBRARY_PREFIX):
            return None
        relative = path[len(_LIBRARY_PREFIX) :]
        if not relative.strip("/") or relative in PathMapper.PER_USER_FILES:
            return None
        return relative

    def _canonical_path(self, path: str, *, keep_last_if_variant_of: str | None = None) -> str:
        relative = self._library_relative(path)
        if relative is None:
            return path
        return _LIBRARY_PREFIX + self.canonicalizer.canonicalize(
            relative, keep_last_if_variant_of=keep_last_if_variant_of
        )

    def _rewrite_destination(self, environ: dict[str, Any], source: str) -> None:
        """Canonicalize the `Destination` header, parsed the way wsgidav parses it."""
        header = environ.get("HTTP_DESTINATION")
        if not header:
            return
        try:
            parsed = urlparse(unquote(header), allow_fragments=False)
        except ValueError:
            return
        keep = None
        if environ.get("REQUEST_METHOD") == "MOVE":
            keep = self._library_relative(source)
        canonical = self._canonical_path(parsed.path, keep_last_if_variant_of=keep)
        if canonical != parsed.path:
            environ["HTTP_DESTINATION"] = urlunparse(
                parsed._replace(path=quote(canonical, safe="/"))
            )

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        raw = environ.get("PATH_INFO", "")
        # PATH_INFO travels as latin-1-decoded request bytes (PEP 3333), and
        # every layer below re-encodes it the same way (`re_encode_wsgi_path`),
        # so a rewrite goes back in that form.
        try:
            path = raw.encode("iso-8859-1").decode("utf-8")
            as_wsgi_bytes = True
        except UnicodeEncodeError:
            path = raw  # already real unicode (a test harness)
            as_wsgi_bytes = False
        except UnicodeDecodeError:
            # Not UTF-8: no name on disk is spelled that way, and the DAV
            # layer refuses the request itself.
            return self.app(environ, start_response)
        canonical = self._canonical_path(path)
        if canonical != path:
            environ["PATH_INFO"] = (
                canonical.encode("utf-8").decode("iso-8859-1") if as_wsgi_bytes else canonical
            )
        if environ.get("REQUEST_METHOD") in ("MOVE", "COPY"):
            self._rewrite_destination(environ, canonical)
        return self.app(environ, start_response)
