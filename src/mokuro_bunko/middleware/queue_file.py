"""``/mokuro-reader/.mokuro-queue.json``: the whole pending OCR queue, per volume.

One virtual file under the WebDAV root that a reader polls, instead of a
timer per volume. Built from the same priced plan as the volume manifest
(`OcrControl.queue_document`: every generation a volume is owed, a new
upload priced at once), so the two agree on every volume.

* Read with exactly a library file's rules -- the same `AuthMiddleware`
  decides it, as a GET of this path (`gate_read`) -- and CORS the same way
  (the outermost middleware). ``GET`` and ``HEAD``.
* Never a file: no PROPFIND lists it (it exists nowhere on disk), and every
  write -- PUT, DELETE, MOVE, COPY, and a MOVE or COPY *onto* it -- is 405.
* ``Cache-Control: no-cache`` and a strong ``ETag`` over the body without
  ``generated_at``, so an unchanged queue answers ``If-None-Match`` with a
  304 and no body. Rebuilt at most once a :data:`REBUILD_SECONDS`. A job's
  ETA is republished only when it moves by :data:`ETA_HYSTERESIS_SECONDS`
  or more, and a rebuild whose volumes, jobs and hold are what was last
  published serves that body again, ``next_check_after`` included -- so a
  quiet queue keeps its ETag.
* No machine names, no errors, no failure details: job ``state`` is
  ``running``, ``queued`` or ``held``, and ``held`` is a plain code.
"""

from __future__ import annotations

import gzip
import hashlib
import json
import threading
import time
from collections.abc import Callable, Iterable
from datetime import UTC, datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any

from mokuro_bunko.catalog.manifest import manifest_url, reader_file_url
from mokuro_bunko.middleware.auth import destination_path_from_environ
from mokuro_bunko.ocr.volume_outlook import recheck_after
from mokuro_bunko.queue.api import read_running_jobs
from mokuro_bunko.webdav.resources import PathMapper

if TYPE_CHECKING:
    from mokuro_bunko.middleware.auth import AuthMiddleware
    from mokuro_bunko.ocr.control import OcrControl

QUEUE_FILE_NAME = ".mokuro-queue.json"
QUEUE_FILE_PATH = f"/{PathMapper.READER_ROOT}/{QUEUE_FILE_NAME}"
QUEUE_FILE_VERSION = 1
#: A body younger than this is served again as is: a poll every few seconds
#: from many readers costs one build.
REBUILD_SECONDS = 1.0
#: How long a build may wait for the scheduler's pending list when none is
#: cached (the last snapshot stands in after that).
PENDING_WAIT_SECONDS = 1.0
#: A job's published ETA is only moved when the plan moves it by at least this
#: much. Every rebuild re-prices the queue against the clock, so an idle
#: queued job's ETA creeps forward by the second; republishing that would
#: change the ETag on every poll and no reader could ever get a 304.
ETA_HYSTERESIS_SECONDS = 60.0

_ALLOW = "GET, HEAD, OPTIONS"


def build_document(control: OcrControl | None, storage_base_path: Path) -> dict[str, Any]:
    """The queue file's content, ``generated_at`` included."""
    held: str | None = None
    volumes: list[dict[str, Any]] = []
    if control is not None:
        held, volumes = control.queue_document(
            read_running_jobs(storage_base_path), wait=PENDING_WAIT_SECONDS
        )
    now = time.time()
    all_jobs = [job for volume in volumes for job in volume["jobs"]]
    return {
        "version": QUEUE_FILE_VERSION,
        "generated_at": datetime.fromtimestamp(now, tz=UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "held": {"reason": held} if held is not None else None,
        "next_check_after": recheck_after(all_jobs, now),
        "volumes": [
            {
                "series": volume["series"],
                "volume": volume["volume"],
                "path": reader_file_url(volume["series"], f"{volume['volume']}.cbz"),
                "manifest": manifest_url(volume["series"], volume["volume"]),
                "jobs": volume["jobs"],
            }
            for volume in volumes
        ],
    }


def build_body(control: OcrControl | None, storage_base_path: Path) -> bytes:
    return json.dumps(build_document(control, storage_base_path), separators=(",", ":")).encode()


def _parse_eta(value: Any) -> float | None:
    if not isinstance(value, str):
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
    except ValueError:
        return None


def _keep_close_etas(document: dict[str, Any], previous: dict[str, Any]) -> None:
    """Keep each job's previously published ETA where the new one is within
    `ETA_HYSTERESIS_SECONDS` of it (in place)."""
    published: dict[tuple[str, str, str], str] = {}
    for volume in previous.get("volumes", []):
        for job in volume["jobs"]:
            if isinstance(job.get("eta"), str):
                published[(volume["series"], volume["volume"], job["id"])] = job["eta"]
    for volume in document.get("volumes", []):
        for job in volume["jobs"]:
            old = published.get((volume["series"], volume["volume"], job["id"]))
            new, was = _parse_eta(job.get("eta")), _parse_eta(old)
            if new is not None and was is not None and abs(new - was) < ETA_HYSTERESIS_SECONDS:
                job["eta"] = old


def _same_queue(document: dict[str, Any], previous: dict[str, Any]) -> bool:
    """Do the two say the same about the queue (all but the two clock fields)?"""
    return all(
        document.get(key) == previous.get(key) for key in ("version", "held", "volumes")
    )


def _etag(document: dict[str, Any]) -> str:
    stable = {key: value for key, value in document.items() if key != "generated_at"}
    digest = hashlib.sha256(
        json.dumps(stable, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()[:32]
    return f'"{digest}"'


class QueueFileMiddleware:
    """Serves the queue file; refuses every write to it."""

    def __init__(
        self,
        app: Callable[..., Iterable[bytes]],
        *,
        storage_base_path: Path,
        ocr_control: OcrControl | None,
        read_gate: AuthMiddleware,
    ) -> None:
        self.app = app
        self.storage_base_path = Path(storage_base_path)
        self._ocr_control = ocr_control
        self._read_gate = read_gate
        self._lock = threading.Lock()
        self._built: tuple[float, bytes, str, bytes] | None = None
        self._document: dict[str, Any] | None = None

    def __call__(
        self,
        environ: dict[str, Any],
        start_response: Callable[..., Any],
    ) -> Iterable[bytes]:
        path = environ.get("PATH_INFO", "")
        method = environ.get("REQUEST_METHOD", "GET")
        if method in ("MOVE", "COPY") and destination_path_from_environ(environ) == QUEUE_FILE_PATH:
            return self._not_allowed(start_response)
        if path != QUEUE_FILE_PATH:
            return self.app(environ, start_response)
        if method == "OPTIONS":
            return self.app(environ, start_response)
        if method not in ("GET", "HEAD"):
            return self._not_allowed(start_response)
        refused = self._read_gate.gate_read(environ, start_response, QUEUE_FILE_PATH)
        if refused is not None:
            return refused
        body, etag, packed = self._current()
        headers = [
            ("Content-Type", "application/json"),
            ("Cache-Control", "no-cache"),
            ("Vary", "Accept-Encoding"),
        ]
        if "gzip" in environ.get("HTTP_ACCEPT_ENCODING", "").lower():
            # A few thousand jobs is a few hundred kB of JSON and a tenth of
            # that compressed. A strong ETag names one representation.
            body, etag = packed, f'{etag[:-1]}-gz"'
            headers.append(("Content-Encoding", "gzip"))
        headers.append(("ETag", etag))
        if etag in [tag.strip() for tag in environ.get("HTTP_IF_NONE_MATCH", "").split(",")]:
            start_response("304 Not Modified", headers)
            return []
        start_response("200 OK", [*headers, ("Content-Length", str(len(body)))])
        return [] if method == "HEAD" else [body]

    def _current(self) -> tuple[bytes, str, bytes]:
        """The body and its ETag, rebuilt when older than `REBUILD_SECONDS`.

        An unchanged queue keeps its ETag across rebuilds (it is taken over
        the body without ``generated_at``), and keeps the body it had too.
        """
        now = time.monotonic()
        with self._lock:
            built = self._built
            if built is not None and now - built[0] < REBUILD_SECONDS:
                return built[1], built[2], built[3]
            document = build_document(self._ocr_control, self.storage_base_path)
            previous = self._document
            if previous is not None:
                _keep_close_etas(document, previous)
                if _same_queue(document, previous):
                    assert built is not None
                    self._built = (now, built[1], built[2], built[3])
                    return built[1], built[2], built[3]
            etag = _etag(document)
            body = json.dumps(document, separators=(",", ":")).encode()
            packed = gzip.compress(body, compresslevel=6)
            self._built = (now, body, etag, packed)
            self._document = document
            return body, etag, packed

    @staticmethod
    def _not_allowed(start_response: Callable[..., Any]) -> list[bytes]:
        body = b"The OCR queue file is generated by the server and cannot be written."
        start_response("405 Method Not Allowed", [
            ("Allow", _ALLOW),
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Content-Length", str(len(body))),
        ])
        return [body]
