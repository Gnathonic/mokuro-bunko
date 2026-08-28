"""Intercepting a `series.json` PUT (contract §6).

A client PUTting `<Series>/series.json` is not writing a file — bunko compiles
that file — it is REQUESTING a metadata update. The request is answered here,
before the DAV app can open a writer for the path.

Placement matters: inside `AuthMiddleware` (so the actor is known) and outside
`PropfindCacheMiddleware` (so the DAV layer never sees the PUT). The cache
invalidation that a normal PUT would trigger is done instead by the service's
`on_published` hook, which fires only when the compiled bytes actually changed.
"""

from __future__ import annotations

import sys
from collections.abc import Callable, Iterable
from typing import TYPE_CHECKING, Any

from mokuro_bunko.metadata.paths import is_series_file_path, series_title_from_series_file_path

if TYPE_CHECKING:
    from mokuro_bunko.metadata.service import MetadataService

#: A `series.json` for a 1000-volume series is well under 300 KB; anything
#: past this is not a metadata update.
MAX_UPDATE_BODY_BYTES = 4 * 1024 * 1024

_STATUS_TEXT = {
    400: "400 Bad Request",
    401: "401 Unauthorized",
    403: "403 Forbidden",
    411: "411 Length Required",
    413: "413 Payload Too Large",
}


def _re_encode_wsgi_path(path: str) -> str:
    """PEP 3333 delivers PATH_INFO as request bytes decoded latin-1.

    The DAV app below re-encodes it to UTF-8 for itself (wsgidav's
    `re_encode_path_info` hotfix runs INSIDE `WsgiDAVApp.__call__`), so this
    middleware — which sits above it — must apply the same transform to see
    the same path, or every non-ASCII series title fails folder resolution
    and the update is refused. Applied to a local copy only: environ is
    passed through untouched, the DAV app re-encodes for itself.

    A path the round-trip cannot handle is returned unchanged: a
    `UnicodeEncodeError` means it is already real unicode (a test harness,
    or a server that decoded for us — the transform would be a no-op
    anyway), and a `UnicodeDecodeError` means genuinely non-UTF-8 request
    bytes, which the DAV layer's own (fallback-less) re-encode rejects for
    every operation, so there is no folder such a spelling could name.
    """
    try:
        return path.encode("iso-8859-1").decode("utf-8")
    except UnicodeError:
        return path


class MetadataAPI:
    """Answers `PUT <Series>/series.json` instead of letting it write."""

    def __init__(
        self,
        app: Callable[..., Iterable[bytes]],
        service: MetadataService | None = None,
    ) -> None:
        self.app = app
        self.service = service

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        if environ.get("REQUEST_METHOD") != "PUT":
            return self.app(environ, start_response)
        path = _re_encode_wsgi_path(environ.get("PATH_INFO", "/"))
        if not is_series_file_path(path):
            return self.app(environ, start_response)
        return self._handle_update(environ, start_response, path)

    def _handle_update(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
        path: str,
    ) -> Iterable[bytes]:
        if self.service is None:
            # Fail closed: bunko owns this file even when compilation is off,
            # and a raw write would be silently replaced later.
            return self._text(start_response, 403, "Metadata files are compiled by the server")

        username = environ.get("mokuro.username")
        if not isinstance(username, str) or not username:
            return self._text(start_response, 401, "Authentication required")

        raw_content_length = environ.get("CONTENT_LENGTH")
        if raw_content_length is None:
            # A chunked-transfer PUT (HTTP/1.1 permits omitting Content-Length
            # when using Transfer-Encoding: chunked) has no declared length at
            # all. `int(None or 0)` used to coerce this to a phantom
            # zero-length body — silently applying an empty update instead of
            # rejecting the request. 411 is the RFC 9110 §15.5.12-precise
            # status for "refused without a declared length", kept distinct
            # from the 400 below (header PRESENT but garbage) so a client can
            # tell the two failure classes apart. Answered without reading
            # `wsgi.input` at all: cheroot's own post-response body drain
            # explicitly skips chunked requests, so a bounded/best-effort read
            # here would desync the connection rather than protect it — not
            # reading is the only response that doesn't make that worse.
            return self._text(start_response, 411, "Content-Length required")
        try:
            length = int(raw_content_length)
        except (TypeError, ValueError):
            return self._text(start_response, 400, "Invalid Content-Length")
        if length < 0:
            return self._text(start_response, 400, "Invalid Content-Length")
        if length > MAX_UPDATE_BODY_BYTES:
            return self._text(start_response, 413, "Metadata update too large")

        body = environ["wsgi.input"].read(length) if length else b""
        series_title = series_title_from_series_file_path(path)
        if series_title is None:  # pragma: no cover - guarded by is_series_file_path
            return self._text(start_response, 400, "Invalid metadata path")

        accepted = self.service.apply_series_update(series_title, body, username)
        self._audit(environ, path, username, accepted)
        if not accepted:
            return self._text(start_response, 400, "Invalid metadata update")

        start_response("204 No Content", [])
        return [b""]

    @staticmethod
    def _audit(
        environ: dict[str, Any], path: str, username: str, accepted: bool
    ) -> None:
        database = environ.get("mokuro.db")
        if database is None:
            return
        try:
            database.log_audit_event(
                action="metadata_update" if accepted else "metadata_rejected",
                actor_username=username,
                target_type="library",
                target_path=path,
                details={"accepted": accepted},
            )
        except Exception as error:  # noqa: BLE001 - auditing must never fail a request
            print(f"[METADATA] audit failed: {error}", file=sys.stderr, flush=True)

    @staticmethod
    def _text(
        start_response: Callable[..., Any], status_code: int, message: str
    ) -> list[bytes]:
        body = message.encode("utf-8")
        start_response(
            _STATUS_TEXT[status_code],
            [
                ("Content-Type", "text/plain; charset=utf-8"),
                ("Content-Length", str(len(body))),
            ],
        )
        return [body]
