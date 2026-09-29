"""What a WebDAV write of a library archive answers, beyond the DAV status.

A ``.cbz`` PUT (or MOVE/COPY into place) that succeeds queues the volume for
OCR before the response goes out (`OcrControl.archive_arrived`) instead of
leaving it for the library's poll. A PUT that queued anything also says where
to follow it up:

* ``X-Mokuro-Manifest`` -- the absolute-path URL of the volume's manifest;
* ``X-Mokuro-Recheck-After`` -- whole seconds until its earliest pending OCR
  job should be done, priced from the queue's own plan (see
  `ocr.volume_outlook.recheck_after`).

Both are absent for any other file and when no OCR is owed or will run.

**Every PUT under the reader root or the inbox ends in a verdict.** The
writer stages the body beside its destination and checks it before moving it
into place (`webdav.resources._AtomicFileWriter`); its `UploadOutcome` is
turned into the answer here:

* success keeps the DAV status (201/204) and adds ``X-Mokuro-Upload``
  (``verified`` for an archive whose CRCs checked out, ``stored`` for any
  other file) and ``X-Mokuro-Size`` (the bytes now stored);
* a body sent with a `Content-Digest` (RFC 9530; ``sha-256`` or
  ``sha-512``) that matched adds ``X-Mokuro-Digest-Verified: <algorithm>``;
* failure replaces the body with JSON ``{"ok": false, "reason", "detail",
  "retry"}``: the writer's own reason (``truncated``,
  ``corrupted-in-transit``, ``archive-damaged``, ``not-an-archive`` -- 422;
  ``disk-full`` -- 507; ``server-error`` -- 500),
  else one read off the status (401/403 ``forbidden``, 507 ``disk-full``,
  anything else ``server-error``). ``retry`` is true only where sending the
  same file again could succeed.

A `series.json`/`catalog.json` PUT is `MetadataAPI`'s, with its own answers,
and is left alone.

The pricing is bounded: the queue's cached pending list is used when there
is one (joining the new volume to it costs an in-memory sort); when there is
none, it waits at most :data:`PUT_PRICE_WAIT` for one and otherwise answers
without the queued ETAs (``300``). It never waits on OCR itself.
"""

from __future__ import annotations

import json
import logging
import time
from collections.abc import Callable, Iterable
from http import HTTPStatus
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.catalog.manifest import manifest_url
from mokuro_bunko.metadata.paths import is_compiled_metadata_path
from mokuro_bunko.middleware.cors import PUT_CAPABILITY, is_dav_path
from mokuro_bunko.ocr.volume_outlook import recheck_after
from mokuro_bunko.queue.api import read_running_jobs
from mokuro_bunko.webdav.resources import (
    ARCHIVE_WRITTEN_KEY,
    ARCHIVES_REMOVED_KEY,
    UPLOAD_OUTCOME_KEY,
    PathMapper,
    UploadOutcome,
)

if TYPE_CHECKING:
    from mokuro_bunko.ocr.control import OcrControl

logger = logging.getLogger(__name__)

#: How long a PUT may wait for a pending list to price the new volume against,
#: when none is cached. A cached one (the usual case) costs nothing to wait for.
PUT_PRICE_WAIT = 0.005
#: How far down the queue a PUT prices its volume. The plan walks the queue in
#: order at roughly 0.05 ms an item; a volume queued further back than this is
#: answered unpriced (300 s) rather than walking the rest of the queue for it.
PUT_PRICE_MAX_ITEMS = 300

MANIFEST_HEADER = "X-Mokuro-Manifest"
RECHECK_HEADER = "X-Mokuro-Recheck-After"
UPLOAD_HEADER = "X-Mokuro-Upload"
SIZE_HEADER = "X-Mokuro-Size"
DIGEST_HEADER = "X-Mokuro-Digest-Verified"

_VERDICT_PREFIXES = (f"/{PathMapper.READER_ROOT}/", "/inbox/")

_WRITE_METHODS = frozenset({"PUT", "MOVE", "COPY", "DELETE"})


class UploadMiddleware:
    """Queue archives written over WebDAV at once; tell a PUT when to look again."""

    def __init__(
        self,
        app: Callable[..., Iterable[bytes]],
        *,
        storage_base_path: Path,
        ocr_control: OcrControl | None,
    ) -> None:
        self.app = app
        self.storage_base_path = Path(storage_base_path)
        self.library_path = self.storage_base_path / "library"
        self._ocr_control = ocr_control

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        method = environ.get("REQUEST_METHOD", "GET")
        if method == "OPTIONS" and is_dav_path(environ.get("PATH_INFO", "")):
            return self.app(environ, _with_put_capability(start_response))
        if method not in _WRITE_METHODS:
            return self.app(environ, start_response)

        captured: dict[str, Any] = {}
        written: list[bytes] = []

        def capture(status: str, headers: list[tuple[str, str]], exc_info: Any = None) -> Callable[[bytes], None]:
            captured["status"] = status
            captured["headers"] = list(headers)
            captured["exc_info"] = exc_info
            return written.append

        result = self.app(environ, capture)
        try:
            body = b"".join([*written, *result])
        finally:
            close = getattr(result, "close", None)
            if close is not None:
                close()

        status: str = captured.get("status", "500 Internal Server Error")
        headers: list[tuple[str, str]] = captured.get("headers", [])
        succeeded = status[:1] == "2"
        if method == "PUT" and self._gives_verdict(environ.get("PATH_INFO", "")):
            outcome = environ.get(UPLOAD_OUTCOME_KEY)
            if not isinstance(outcome, UploadOutcome):
                outcome = None
            if succeeded:
                if outcome is not None and outcome.verdict is not None:
                    headers = headers + [
                        (UPLOAD_HEADER, outcome.verdict),
                        (SIZE_HEADER, str(outcome.size)),
                    ]
                    if outcome.digest_verified is not None:
                        headers.append((DIGEST_HEADER, outcome.digest_verified))
            else:
                status, headers, body = self._failure(status, headers, outcome)
        if method == "PUT" and environ.get("PATH_INFO", "").lower().endswith(".cbz"):
            headers = headers + [PUT_CAPABILITY]
        removed = environ.get(ARCHIVES_REMOVED_KEY)
        if isinstance(removed, list) and succeeded:
            self._archives_removed(removed)
        arrived = environ.get(ARCHIVE_WRITTEN_KEY)
        if isinstance(arrived, Path) and succeeded:
            headers = headers + self._archive_arrived(arrived, announce=method == "PUT")
        start_response(status, headers, captured.get("exc_info"))
        return [body]

    @staticmethod
    def _gives_verdict(path: str) -> bool:
        return path.startswith(_VERDICT_PREFIXES) and not is_compiled_metadata_path(path)

    @staticmethod
    def _failure(
        status: str, headers: list[tuple[str, str]], outcome: UploadOutcome | None
    ) -> tuple[str, list[tuple[str, str]], bytes]:
        """The JSON verdict for a failed PUT: the writer's reason, else the status's."""
        try:
            code = int(status.split(" ", 1)[0])
        except ValueError:
            code = 500
        if outcome is not None and outcome.reason is not None and outcome.status is not None:
            code = outcome.status
            reason, detail, retry = outcome.reason, outcome.detail or "", outcome.retry
        elif code in (401, 403, 429):
            reason, retry = "forbidden", False
            detail = (
                "Sign in to upload."
                if code == 401
                else "This account may not upload this file."
                if code == 403
                else "Too many failed sign-ins; wait and try again."
            )
        elif code == 507:
            reason, detail, retry = "disk-full", "The server's disk is full.", False
        else:
            reason, retry = "server-error", code >= 500 or code in (408, 423)
            detail = f"The server could not store the upload ({code})."
        try:
            phrase = HTTPStatus(code).phrase
        except ValueError:
            phrase = "Error"
        body = json.dumps(
            {"ok": False, "reason": reason, "detail": detail, "retry": retry}
        ).encode("utf-8")
        kept = [
            (name, value)
            for name, value in headers
            if name.lower() not in ("content-type", "content-length")
        ]
        return (
            f"{code} {phrase}",
            kept + [("Content-Type", "application/json"), ("Content-Length", str(len(body)))],
            body,
        )

    def _archives_removed(self, paths: list[Path]) -> None:
        """Drop the queued OCR of archives (or folders) that are gone; cancel the running."""
        control = self._ocr_control
        if control is None:
            return
        for path in paths:
            try:
                control.archive_removed(path)
            except Exception:  # noqa: BLE001 - the delete stands; collection discards the rest
                logger.exception("cancelling the OCR of removed %s failed", path)

    def _archive_arrived(self, cbz: Path, *, announce: bool) -> list[tuple[str, str]]:
        """Queue ``cbz``; for a PUT, the headers that say when to look again."""
        control = self._ocr_control
        if control is None:
            return []
        try:
            control.archive_arrived(cbz)
            if not announce:
                return []
            relative = cbz.resolve().relative_to(self.library_path.resolve())
            series, volume = relative.parent.as_posix(), cbz.stem
            pending = control.volume_pending(
                cbz,
                series,
                volume,
                read_running_jobs(self.storage_base_path),
                wait=PUT_PRICE_WAIT,
                max_items=PUT_PRICE_MAX_ITEMS,
            )
        except Exception:  # noqa: BLE001 - the file is stored; queueing it is the poll's job too
            logger.exception("queueing uploaded archive %s failed", cbz)
            return []
        if not pending:
            return []
        seconds = recheck_after(pending, time.time())
        return [
            (MANIFEST_HEADER, manifest_url(series, volume)),
            (RECHECK_HEADER, str(seconds)),
        ]


def _with_put_capability(start_response: Callable[..., Any]) -> Callable[..., Any]:
    """``start_response`` that adds ``X-Mokuro-Put: verified`` to the answer."""

    def wrapped(status: str, headers: list[tuple[str, str]], exc_info: Any = None) -> Any:
        return start_response(status, [*headers, PUT_CAPABILITY], exc_info)

    return wrapped
