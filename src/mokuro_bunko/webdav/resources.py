"""WebDAV resources for mokuro-bunko.

Compatible with mokuro-reader's expected WebDAV structure.
The reader creates a /mokuro-reader/ folder on the server and stores:
  - volume-data.json, profiles.json and goals.json (per-user progress/settings)
  - {SeriesTitle}/{Volume}.cbz (manga files, shared across users)

This module maps those virtual paths to a physical layout where manga
files are shared and per-user data is isolated.
"""

from __future__ import annotations

import base64
import binascii
import errno
import hashlib
import io
import os
import re
import shutil
import tempfile
import threading
import time
from collections import OrderedDict
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import TYPE_CHECKING, Any, BinaryIO, cast
from urllib.parse import quote

from wsgidav.dav_provider import DAVCollection, DAVError, DAVNonCollection
from wsgidav.util import join_uri

from mokuro_bunko.ocr.generations import sidecar_siblings
from mokuro_bunko.processor.archives import InflateLimit, verify_archive
from mokuro_bunko.security import safe_resolve_under

# Internal nginx location used for X-Accel-Redirect download offload. The nginx
# config aliases this prefix to the library root, so the redirect path is the
# requested file's path relative to the library, URL-encoded. See
# deploy/nginx-internal.conf.template.
_NGINX_INTERNAL_PREFIX = "/internal-library/"

if TYPE_CHECKING:

    from mokuro_bunko.database import Database


class _PathWriteLocks:
    """Process-local per-path mutex registry for conflicting write operations.

    A path conflicts with itself and with any ancestor/descendant (so a
    folder move blocks writes to files inside it, and vice versa). Keys are
    casefolded resolved parts, matching case-insensitive filesystems.
    """

    def __init__(self) -> None:
        self._locks: set[tuple[str, ...]] = set()
        self._guard = threading.Lock()

    @staticmethod
    def _parts(path: Path) -> tuple[str, ...]:
        return tuple(part.casefold() for part in path.resolve().parts)

    @staticmethod
    def _is_prefix(prefix: tuple[str, ...], full: tuple[str, ...]) -> bool:
        if len(prefix) > len(full):
            return False
        return full[: len(prefix)] == prefix

    @classmethod
    def _conflicts(cls, current: tuple[str, ...], candidate: tuple[str, ...]) -> bool:
        return cls._is_prefix(current, candidate) or cls._is_prefix(candidate, current)

    def acquire(self, path: Path, blocking: bool = False) -> bool:
        key = self._parts(path)
        if blocking:
            raise ValueError("blocking path lock acquisition is not supported")
        with self._guard:
            for locked in self._locks:
                if self._conflicts(locked, key):
                    return False
            self._locks.add(key)
            return True

    def release(self, path: Path) -> None:
        key = self._parts(path)
        with self._guard:
            self._locks.discard(key)


_PATH_WRITE_LOCKS = _PathWriteLocks()

_LOCKED_MESSAGE = "Resource is locked by another write operation"
_HTTP_LOCKED = 423

#: Environ key a successful PUT/MOVE/COPY of a library ``.cbz`` sets to the
#: archive's path (`MokuroFileResource._note_archive_written`); read by
#: `middleware.upload.UploadMiddleware` once the request has succeeded.
ARCHIVE_WRITTEN_KEY = "mokuro.archive_written"

#: Environ key listing the library paths a successful DELETE or MOVE took
#: away -- a ``.cbz``, or a whole folder -- for `UploadMiddleware` to cancel
#: their OCR (`OcrControl.archive_removed`) once the request has succeeded.
ARCHIVES_REMOVED_KEY = "mokuro.archives_removed"

#: Environ key holding the `UploadOutcome` of a PUT's write, filled in by the
#: writer; `middleware.upload.UploadMiddleware` turns it into the response's
#: verdict headers or its JSON failure body.
UPLOAD_OUTCOME_KEY = "mokuro.upload"

# Errors that mean the disk (or the user's quota) is full: a 507, not a 500.
_DISK_FULL_ERRNOS = frozenset({errno.ENOSPC, errno.EDQUOT})


@dataclass
class UploadOutcome:
    """What became of one PUT body.

    On success ``verdict`` is ``verified`` (an archive whose zip structure and
    CRC-32s check out) or ``stored`` (any other file), ``size`` the bytes
    now on disk, and ``digest_verified`` the `Content-Digest` algorithm the
    body matched (None without one). On failure ``reason`` is one of
    ``truncated``, ``corrupted-in-transit``, ``archive-damaged``,
    ``not-an-archive``, ``disk-full`` or ``server-error``, with the HTTP
    ``status``, a sentence for a person and whether sending the same file
    again could succeed.
    """

    verdict: str | None = None
    size: int | None = None
    digest_verified: str | None = None
    status: int | None = None
    reason: str | None = None
    detail: str | None = None
    retry: bool = False


# `Content-Digest` (RFC 9530) algorithms this server checks, in preference
# order, with the digest length each must have.
_DIGEST_ALGORITHMS: dict[str, tuple[str, int]] = {
    "sha-256": ("sha256", 32),
    "sha-512": ("sha512", 64),
}
# One dictionary member: `<key>=:<base64>:` (an RFC 8941 byte sequence). The
# field defines no parameters, so a member carrying any is malformed.
_DIGEST_MEMBER = re.compile(r"^([a-z*][a-z0-9_.*-]*)=:([A-Za-z0-9+/]*={0,2}):$")


def parse_content_digest(header: str | None) -> tuple[str, bytes] | None:
    """``(algorithm, digest)`` from a `Content-Digest` header, or None.

    Strict: a header any member of which is not ``<key>=:<base64>:`` -- bad
    base64, parameters, a bare token, a digest of the wrong length for a
    known algorithm -- is ignored whole, as if absent. Members naming an
    algorithm this server does not check are skipped. Of the rest, the first
    in :data:`_DIGEST_ALGORITHMS` order is used; a repeated key keeps its
    last value, as a structured-field dictionary does.
    """
    if header is None or not header.strip():
        return None
    members: dict[str, bytes] = {}
    for raw in header.split(","):
        match = _DIGEST_MEMBER.match(raw.strip())
        if match is None:
            return None
        try:
            value = base64.b64decode(match.group(2), validate=True)
        except (binascii.Error, ValueError):
            return None
        algorithm = match.group(1)
        known = _DIGEST_ALGORITHMS.get(algorithm)
        if known is not None and len(value) != known[1]:
            return None
        members[algorithm] = value
    for algorithm in _DIGEST_ALGORITHMS:
        if algorithm in members:
            return algorithm, members[algorithm]
    return None


class DamageMemory:
    """Damage seen in archive PUTs that carried no digest, per destination.

    Without a `Content-Digest` the server cannot tell damage in transit from
    a damaged copy on the client. The same damage twice -- same size, same
    damaged members -- to the same path says it is the source. Bounded
    (least recently used out) and forgetful (``ttl`` seconds), in memory
    only: a restart simply forgets, which costs at most one more retry.
    """

    def __init__(
        self,
        capacity: int = 256,
        ttl: float = 3600.0,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._capacity = capacity
        self._ttl = ttl
        self._clock = clock
        self._lock = threading.Lock()
        self._seen: OrderedDict[str, tuple[tuple[Any, ...], float]] = OrderedDict()

    def seen_before(self, path: str, signature: tuple[Any, ...]) -> bool:
        """Record this damage at ``path``; True when it is what was seen there last."""
        now = self._clock()
        with self._lock:
            previous = self._seen.pop(path, None)
            repeat = (
                previous is not None
                and previous[0] == signature
                and now - previous[1] <= self._ttl
            )
            self._seen[path] = (signature, now)
            while len(self._seen) > self._capacity:
                self._seen.popitem(last=False)
            return repeat

    def forget(self, path: str) -> None:
        with self._lock:
            self._seen.pop(path, None)


_DAMAGE_MEMORY = DamageMemory()


def expected_upload_size(environ: dict[str, Any]) -> int | None:
    """The body length a PUT announced, or None (chunked, or unparseable)."""
    raw = environ.get("CONTENT_LENGTH")
    if raw is None or str(raw).strip() == "":
        return None
    try:
        value = int(str(raw).strip())
    except ValueError:
        return None
    return value if value >= 0 else None


def _forget_ocr_records(db: Any, rel: str | None, *, archive_too: bool = True) -> None:
    """A library file at ``rel`` left (or was overwritten): drop who wrote its OCR.

    An archive takes every sidecar record of its volume (``archive_too``;
    a PUT replacing an archive leaves its sidecars where they are, so it
    passes False). A sidecar takes its own. The records are
    `Database.record_ocr_sidecar`'s -- whoever writes a sidecar over WebDAV
    is not the machine on record, so its row goes too.
    """
    if db is None or rel is None:
        return
    lower = rel.lower()
    if lower.endswith(".cbz"):
        if archive_too:
            db.forget_ocr_sidecars_of_volume(rel)
    elif lower.endswith(".mokuro") or lower.endswith(".mokuro.gz"):
        db.forget_ocr_sidecar(rel)


def _remember_primary_uuid(db: Any, path: Path, rel: str | None) -> None:
    """``path`` is leaving its place: if it is an archive's primary `.mokuro`, keep its id.

    Deleting the bare `<Volume>.mokuro` is how a volume is re-OCR'd: the
    server makes a missing primary again, and it must name the volume by the
    id every reader's progress already knows (`Database.remember_volume_uuid`,
    read by `OCRProcessor.volume_uuid_for`). A layer file, or a sidecar with
    no archive beside it, is not a volume's primary and is left alone.
    """
    if db is None or rel is None:
        return
    name = path.name
    lower = name.lower()
    if lower.endswith(".mokuro.gz"):
        stem = name[: -len(".mokuro.gz")]
    elif lower.endswith(".mokuro"):
        stem = name[: -len(".mokuro")]
    else:
        return
    if not (path.parent / f"{stem}.cbz").is_file():
        return
    # Imported here: the OCR processor is heavy, and only this path needs it.
    from mokuro_bunko.ocr.processor import OCRProcessor

    volume_uuid = OCRProcessor._sidecar_volume_uuid(path)
    if volume_uuid is not None:
        db.remember_volume_uuid(rel, volume_uuid)


def _try_acquire_all(paths: list[Path]) -> list[Path] | None:
    """Acquire write locks on all paths or none.

    Returns the acquired paths (release in reverse order when done), or None
    if any acquisition failed (already-acquired ones are rolled back).
    Deterministic ordering keeps lock acquisition patterns predictable.
    """
    ordered = sorted(paths, key=lambda p: str(p.resolve()).casefold())
    acquired: list[Path] = []
    for path in ordered:
        if not _PATH_WRITE_LOCKS.acquire(path):
            for held in reversed(acquired):
                _PATH_WRITE_LOCKS.release(held)
            return None
        acquired.append(path)
    return acquired


@contextmanager
def path_write_lock(path: Path) -> Iterator[None]:
    """Hold the per-path write lock for a non-DAV writer.

    The compiled metadata files are written by the server itself, outside the
    DAV request path, but they live in the same tree: taking the same lock is
    what stops a regeneration from interleaving with an upload or a folder
    MOVE. Raises `DAVError(423)` when the path (or an ancestor) is busy.
    """
    if not _PATH_WRITE_LOCKS.acquire(path):
        raise DAVError(_HTTP_LOCKED, _LOCKED_MESSAGE)
    try:
        yield
    finally:
        _PATH_WRITE_LOCKS.release(path)


class PathMapper:
    """Maps virtual WebDAV paths to physical filesystem paths.

    Virtual structure (compatible with mokuro-reader):
        /                                - Root (virtual)
        /mokuro-reader/                  - Reader root (virtual, merged view)
        /mokuro-reader/volume-data.json  - Per-user progress data
        /mokuro-reader/profiles.json     - Per-user profile settings
        /mokuro-reader/goals.json        - Per-user reading goals
        /mokuro-reader/{series}/         - Shared series folder
        /mokuro-reader/{series}/{file}   - Shared manga files (CBZ etc.)

    Physical structure:
        {storage_base}/library/          - Shared manga library
        {storage_base}/inbox/            - OCR upload queue
        {storage_base}/users/{username}/ - Per-user data
    """

    READER_ROOT = "mokuro-reader"
    # Root .json files that belong to ONE user and map into their private
    # directory. Everything else under /mokuro-reader/ is the shared library,
    # so a per-user file left off this set would be a single file shared by
    # every account — each one overwriting the others — and would be rejected
    # outright for any account without library write permission.
    PER_USER_FILES = frozenset({"volume-data.json", "profiles.json", "goals.json"})

    def __init__(self, storage_base: Path) -> None:
        """Initialize path mapper.

        Args:
            storage_base: Base path for storage directory.
        """
        self.storage_base = Path(storage_base)
        self.library_path = self.storage_base / "library"
        self.inbox_path = self.storage_base / "inbox"
        self.users_path = self.storage_base / "users"

    def ensure_directories(self) -> None:
        """Create storage directories if they don't exist."""
        self.library_path.mkdir(parents=True, exist_ok=True)
        self.inbox_path.mkdir(parents=True, exist_ok=True)
        self.users_path.mkdir(parents=True, exist_ok=True)

    def ensure_user_directory(self, username: str) -> Path:
        """Ensure user directory exists and return its path."""
        user_dir = (self.users_path / username).resolve()
        if not user_dir.is_relative_to(self.users_path.resolve()):
            raise ValueError("Invalid username path")
        user_dir.mkdir(parents=True, exist_ok=True)
        return user_dir

    def get_user_file_path(self, username: str, filename: str) -> Path | None:
        """Safely resolve a per-user file path under users/{username}/."""
        user_dir = (self.users_path / username).resolve()
        users_root = self.users_path.resolve()
        if not user_dir.is_relative_to(users_root):
            return None
        return user_dir / filename

    def is_per_user_file(self, virtual_path: str) -> bool:
        """Check if path is a per-user file (see PER_USER_FILES).

        These files live directly under /mokuro-reader/ and are mapped
        to each user's private directory.
        """
        virtual_path = "/" + virtual_path.strip("/")
        prefix = f"/{self.READER_ROOT}/"
        if virtual_path.startswith(prefix):
            relative = virtual_path[len(prefix):]
            return relative in self.PER_USER_FILES
        return False

    def is_reader_path(self, virtual_path: str) -> bool:
        """Check if path is under /mokuro-reader/."""
        virtual_path = "/" + virtual_path.strip("/")
        return (
            virtual_path == f"/{self.READER_ROOT}"
            or virtual_path.startswith(f"/{self.READER_ROOT}/")
        )

    def is_inbox_path(self, virtual_path: str) -> bool:
        """Check if path is under /inbox/."""
        virtual_path = "/" + virtual_path.strip("/")
        return virtual_path == "/inbox" or virtual_path.startswith("/inbox/")

    def virtual_to_physical(
        self,
        virtual_path: str,
        username: str | None = None,
    ) -> Path | None:
        """Convert virtual WebDAV path to physical filesystem path.

        Args:
            virtual_path: Virtual path from WebDAV request.
            username: Current user's username (for per-user file mapping).

        Returns:
            Physical filesystem path, or None if path is virtual-only.
        """
        virtual_path = "/" + virtual_path.strip("/")

        # Root and reader root are virtual
        if virtual_path == "/":
            return None
        if virtual_path == f"/{self.READER_ROOT}":
            return None

        # /mokuro-reader/* paths
        if virtual_path.startswith(f"/{self.READER_ROOT}/"):
            relative = virtual_path[len(f"/{self.READER_ROOT}/"):]

            # Per-user files map to user's private directory
            if relative in self.PER_USER_FILES:
                if username:
                    return self.get_user_file_path(username, relative)
                return None

            # Everything else maps to shared library
            return safe_resolve_under(self.library_path, relative)

        # /inbox paths
        if virtual_path == "/inbox" or virtual_path.startswith("/inbox/"):
            relative = virtual_path[6:].lstrip("/")  # Remove "/inbox"
            if relative:
                return safe_resolve_under(self.inbox_path, relative)
            return self.inbox_path.resolve()

        return None

    def physical_to_virtual(
        self,
        physical_path: Path,
        username: str | None = None,
    ) -> str | None:
        """Convert physical filesystem path to virtual WebDAV path.

        Args:
            physical_path: Physical filesystem path.
            username: Current user's username.

        Returns:
            Virtual WebDAV path, or None if not mappable.
        """
        physical_path = Path(physical_path).resolve()
        storage_base = self.storage_base.resolve()

        try:
            relative = physical_path.relative_to(storage_base)
        except ValueError:
            return None

        parts = relative.parts
        if not parts:
            return "/"

        # library/* -> /mokuro-reader/*
        if parts[0] == "library":
            if len(parts) > 1:
                return f"/{self.READER_ROOT}/" + "/".join(parts[1:])
            return f"/{self.READER_ROOT}"

        # inbox/* -> /inbox/*
        if parts[0] == "inbox":
            return "/" + "/".join(parts)

        # users/{username}/{per-user-file} -> /mokuro-reader/{per-user-file}
        if parts[0] == "users" and len(parts) >= 3:
            filename = parts[2]
            if filename in self.PER_USER_FILES:
                return f"/{self.READER_ROOT}/{filename}"

        return None

    def get_path_type(self, virtual_path: str) -> str:
        """Determine the type of a virtual path.

        Returns:
            One of: "root", "reader_root", "progress", "library", "inbox", "unknown"
        """
        virtual_path = "/" + virtual_path.strip("/")

        if virtual_path == "/":
            return "root"

        if virtual_path == f"/{self.READER_ROOT}":
            return "reader_root"

        if virtual_path.startswith(f"/{self.READER_ROOT}/"):
            relative = virtual_path[len(f"/{self.READER_ROOT}/"):]
            if relative in self.PER_USER_FILES:
                return "progress"
            return "library"

        if virtual_path == "/inbox" or virtual_path.startswith("/inbox/"):
            return "inbox"

        return "unknown"


class MokuroFileResource(DAVNonCollection):  # type: ignore[misc]
    """WebDAV resource for files."""

    # Static property list — avoids calling getters to probe existence (8 calls
    # × 33k resources = 264k method calls saved on a Depth:infinity PROPFIND).
    _PROP_NAMES = [
        "{DAV:}resourcetype",
        "{DAV:}creationdate",
        "{DAV:}getcontentlength",
        "{DAV:}getcontenttype",
        "{DAV:}getlastmodified",
        "{DAV:}displayname",
        "{DAV:}getetag",
    ]

    def __init__(
        self,
        path: str,
        environ: dict[str, Any],
        file_path: Path,
    ) -> None:
        super().__init__(path, environ)
        self.file_path = file_path
        self._stat: os.stat_result | None = None
        self._accel_redirect: str | None = None
        self._accel_redirect_computed = False
        self._active_writer: _LockedWriter | None = None

    def _get_database(self) -> Database | None:
        db = self.environ.get("mokuro.db")
        if db is None:
            return None
        return cast("Database", db)

    def _get_actor_username(self) -> str | None:
        user_data = self.environ.get("mokuro.user")
        if isinstance(user_data, dict):
            username = user_data.get("username")
            if isinstance(username, str):
                return username
        username = self.environ.get("mokuro.username")
        if isinstance(username, str):
            return username
        return None

    def _relative_under_library(self) -> str | None:
        provider = self.provider
        if not hasattr(provider, "path_mapper"):
            return None
        mapper: PathMapper = provider.path_mapper
        try:
            return str(self.file_path.resolve().relative_to(mapper.library_path.resolve()))
        except ValueError:
            return None

    def _get_mapper(self) -> PathMapper | None:
        provider = self.provider
        if not hasattr(provider, "path_mapper"):
            return None
        return cast("PathMapper", provider.path_mapper)

    def _resolve_destination_path(self, dest_path: str) -> Path | None:
        mapper = self._get_mapper()
        if mapper is None:
            return None
        source_type = mapper.get_path_type(self.path)
        dest_type = mapper.get_path_type(dest_path)
        if source_type not in {"library", "progress"} or dest_type != source_type:
            return None
        return mapper.virtual_to_physical(dest_path, self._get_actor_username())

    def _audit(self, action: str, *, details: dict[str, Any] | None = None) -> None:
        db = self._get_database()
        if db is None:
            return
        rel = self._relative_under_library()
        if rel is not None:
            target_path = f"/{PathMapper.READER_ROOT}/{rel}"
            target_type = "library"
        else:
            target_path = self.path
            target_type = "progress" if self.path.split("/")[-1] in PathMapper.PER_USER_FILES else "webdav"
        db.log_audit_event(
            action=action,
            actor_username=self._get_actor_username(),
            target_type=target_type,
            target_path=target_path,
            details=details,
        )

    def _audit_lock_conflict(
        self, operation: str, *, details: dict[str, Any] | None = None
    ) -> None:
        payload: dict[str, Any] = {"operation": operation}
        if details:
            payload.update(details)
        self._audit("lock_conflict", details=payload)

    def _note_archive_written(self, path: Path) -> None:
        """Tell the upload middleware a library ``.cbz`` is now in place.

        It queues the volume for OCR at once (`OcrControl.archive_arrived`)
        once the request has succeeded, instead of leaving it for the poll.
        """
        mapper = self._get_mapper()
        if mapper is None or path.suffix != ".cbz":
            return
        try:
            path.resolve().relative_to(mapper.library_path.resolve())
        except (OSError, ValueError):
            return
        self.environ[ARCHIVE_WRITTEN_KEY] = path

    def _note_archive_removed(self, path: Path) -> None:
        """Note that a library ``.cbz`` left its place (deleted, or moved away)."""
        if path.suffix.lower() != ".cbz":
            return
        _note_removed(self.environ, self._get_mapper(), path)

    def _on_write_committed(self, existed_before: bool) -> None:
        self._note_archive_written(self.file_path)
        db = self._get_database()
        actor = self._get_actor_username()
        rel = self._relative_under_library()
        if db is not None and rel is not None and actor:
            db.record_volume_upload(rel, actor, existed_before=existed_before)
        _forget_ocr_records(db, rel, archive_too=False)
        self._audit(
            "edit" if existed_before else "upload",
            details={"existed_before": existed_before},
        )

    def get_property_names(self, *, is_allprop: bool) -> list[str]:
        """Return static property list (no getter probing needed)."""
        return list(self._PROP_NAMES)

    def _get_stat(self) -> os.stat_result | None:
        """Get cached stat result."""
        if self._stat is None:
            try:
                self._stat = os.stat(self.file_path)
            except OSError:
                pass
        return self._stat

    def get_content_length(self) -> int | None:
        """Return file size."""
        stat_result = self._get_stat()
        if stat_result:
            return stat_result.st_size
        return None

    def get_content_type(self) -> str | None:
        """Return content type based on extension."""
        suffix = self.file_path.suffix.lower()
        content_types = {
            ".cbz": "application/vnd.comicbook+zip",
            ".cbr": "application/vnd.comicbook-rar",
            ".zip": "application/zip",
            ".gz": "application/gzip",
            ".json": "application/json",
            ".jpg": "image/jpeg",
            ".jpeg": "image/jpeg",
            ".png": "image/png",
            ".gif": "image/gif",
            ".webp": "image/webp",
        }
        # Handle compound extensions
        name_lower = self.file_path.name.lower()
        if name_lower.endswith(".json.gz"):
            return "application/gzip"
        if name_lower.endswith(".mokuro.gz"):
            return "application/gzip"
        return content_types.get(suffix, "application/octet-stream")

    def get_creation_date(self) -> float | None:
        """Return creation time."""
        stat_result = self._get_stat()
        if stat_result:
            return stat_result.st_ctime
        return None

    def get_display_name(self) -> str:
        """Return display name."""
        return self.file_path.name

    def get_etag(self) -> str | None:
        """Return ETag based on mtime and size."""
        stat_result = self._get_stat()
        if stat_result:
            return f"{stat_result.st_mtime:.6f}-{stat_result.st_size}"
        return None

    def get_last_modified(self) -> float | None:
        """Return last modified time."""
        stat_result = self._get_stat()
        if stat_result:
            return stat_result.st_mtime
        return None

    def support_etag(self) -> bool:
        return True

    def _compute_accel_redirect(self) -> str | None:
        """Internal nginx X-Accel-Redirect path, or None to stream normally.

        Returns a path only when (a) the server runs behind nginx
        (``mokuro.nginx_accel`` is set) and (b) ``file_path`` resolves *inside*
        the library root. Per-user files and any path that escapes the library
        are streamed through Python instead, so nginx is never asked to serve a
        file outside the directory its internal location is aliased to.

        The returned path is the file's location relative to the library root,
        URL-encoded (preserving "/"), so spaces, unicode and reserved
        characters cannot break out of the header or mis-resolve.
        """
        if not self.environ.get("mokuro.nginx_accel"):
            return None
        mapper = self._get_mapper()
        if mapper is None:
            return None
        try:
            rel = self.file_path.resolve().relative_to(mapper.library_path.resolve())
        except ValueError:
            return None  # Not under the library root -> do not offload.
        encoded = quote(rel.as_posix(), safe="/")
        return f"{_NGINX_INTERNAL_PREFIX}{encoded}"

    def _accel_redirect_path(self) -> str | None:
        """Memoized accessor for the X-Accel-Redirect path (stable per request)."""
        if not self._accel_redirect_computed:
            self._accel_redirect = self._compute_accel_redirect()
            self._accel_redirect_computed = True
        return self._accel_redirect

    def support_ranges(self) -> bool:
        # When offloading to nginx, let nginx satisfy Range requests against the
        # real file; Python returns an empty body via X-Accel-Redirect, so it
        # must not advertise/compute ranges itself.
        if self._accel_redirect_path() is not None:
            return False
        return True

    def finalize_headers(
        self, environ: dict[str, Any], response_headers: list[tuple[str, str]]
    ) -> None:
        """Inject X-Accel-Redirect for nginx-offloaded library downloads."""
        accel = self._accel_redirect_path()
        if accel is None:
            return
        # Set Content-Length: 0 (not drop it). The body we return IS empty
        # (get_content -> b""), so 0 is accurate; nginx serves the real bytes via
        # the X-Accel-Redirect and overrides Content-Length (and Content-Range
        # for ranges) with the real file size. Keeping a Content-Length is
        # REQUIRED: WsgiDAV force-closes any keep-alive response that has a
        # body-bearing status but no Content-Length (see wsgidav_app.py
        # _start_response_wrapper). On the nginx-offload path that fired on every
        # single library download, tearing down the upstream connection each time
        # and churning nginx's `keepalive` pool — which surfaces as sporadic 502
        # ("upstream prematurely closed connection") on unrelated requests such
        # as renames. Verified: nginx serves the full file whether upstream sends
        # Content-Length 0, the real size, or none.
        response_headers[:] = [
            (k, v) for (k, v) in response_headers if k.lower() != "content-length"
        ]
        response_headers.append(("Content-Length", "0"))
        response_headers.append(("X-Accel-Redirect", accel))

    def get_content(self) -> BinaryIO:
        """Return file content as file object."""
        if self._accel_redirect_path() is not None:
            # nginx serves the bytes via X-Accel-Redirect; Python sends no body.
            return io.BytesIO(b"")
        try:
            return open(self.file_path, "rb")
        except OSError as e:
            raise DAVError(500, f"Cannot read file: {e}") from e

    def begin_write(self, content_type: str | None = None) -> BinaryIO:
        """Begin writing to file, return file object."""
        if not _PATH_WRITE_LOCKS.acquire(self.file_path):
            self._audit_lock_conflict("write")
            raise DAVError(_HTTP_LOCKED, _LOCKED_MESSAGE)

        try:
            existed_before = self.file_path.exists()
            self.file_path.parent.mkdir(parents=True, exist_ok=True)
            outcome = UploadOutcome()
            self.environ[UPLOAD_OUTCOME_KEY] = outcome
            writer_class = (
                _ValidatedCbzWriter
                if self.file_path.suffix.lower() == ".cbz"
                else _AtomicFileWriter
            )
            writer = cast(
                "BinaryIO",
                writer_class(
                    self.file_path,
                    expected_size=expected_upload_size(self.environ),
                    expected_digest=parse_content_digest(
                        self.environ.get("HTTP_CONTENT_DIGEST")
                    ),
                    outcome=outcome,
                ),
            )
            audited = _AuditedWriter(
                writer,
                on_commit=lambda: self._on_write_committed(existed_before),
            )
            locked = _LockedWriter(
                cast("BinaryIO", audited),
                on_release=lambda: _PATH_WRITE_LOCKS.release(self.file_path),
            )
        except BaseException:
            _PATH_WRITE_LOCKS.release(self.file_path)
            raise

        self._active_writer = locked
        return cast("BinaryIO", locked)

    def end_write(self, *, with_errors: bool) -> None:
        """Finish a PUT. On error wsgidav never closes the file object, so
        discard the temp file and release the path lock here."""
        writer = self._active_writer
        self._active_writer = None
        if with_errors and writer is not None:
            writer.abort()

    def delete(self) -> None:
        """Delete the file."""
        if not self.file_path.exists():
            return

        if not _PATH_WRITE_LOCKS.acquire(self.file_path):
            self._audit_lock_conflict("delete")
            raise DAVError(_HTTP_LOCKED, _LOCKED_MESSAGE)

        try:
            rel = self._relative_under_library()
            lower = self.file_path.name.lower()
            _remember_primary_uuid(self._get_database(), self.file_path, rel)
            if lower.endswith(".cbz"):
                # Every sidecar this archive has, LISTED from the directory
                # rather than looked up in a registry of configured names: a
                # generation that was renamed, disabled or deleted still has
                # its files here, and so may a layer another server wrote or
                # a reader pushed. All of them belong to this archive and
                # all of them go with it (see `generations.sidecar_siblings`).
                for sidecar in sidecar_siblings(self.file_path):
                    try:
                        sidecar.unlink(missing_ok=True)
                    except OSError:
                        pass

            os.remove(self.file_path)
            self._note_archive_removed(self.file_path)

            db = self._get_database()
            if db is not None and rel is not None and lower.endswith(".cbz"):
                db.forget_volume_upload(rel)
                # Its sidecars went with it: a new upload under this name is
                # a new volume and gets an id of its own.
                db.forget_volume_uuid(rel)
            _forget_ocr_records(db, rel)
            self._audit("delete")
        finally:
            _PATH_WRITE_LOCKS.release(self.file_path)

    def handle_move(self, dest_path: str) -> bool:
        """Handle direct file moves natively without touching sibling sidecars."""
        dest_physical = self._resolve_destination_path(dest_path)
        if dest_physical is None:
            return False

        acquired = _try_acquire_all([self.file_path, dest_physical])
        if acquired is None:
            self._audit_lock_conflict("move", details={"destination": dest_path})
            raise DAVError(_HTTP_LOCKED, _LOCKED_MESSAGE)

        try:
            mapper = self._get_mapper()
            old_rel = self._relative_under_library()
            new_rel = None
            if mapper is not None:
                try:
                    new_rel = str(
                        dest_physical.resolve().relative_to(mapper.library_path.resolve())
                    )
                except ValueError:
                    new_rel = None

            db = self._get_database()
            _remember_primary_uuid(db, self.file_path, old_rel)
            dest_physical.parent.mkdir(parents=True, exist_ok=True)
            os.replace(self.file_path, dest_physical)
            self._note_archive_removed(self.file_path)
            self._note_archive_written(dest_physical)

            if db is not None and old_rel is not None and new_rel is not None:
                db.rename_volume_upload(old_rel, new_rel)
            # The archive's sidecars stay where they were: their records are
            # of a volume that is not there any more.
            _forget_ocr_records(db, old_rel)
            _forget_ocr_records(db, new_rel, archive_too=False)

            self._audit("move", details={"destination": dest_path})
            return True
        finally:
            for path in reversed(acquired):
                _PATH_WRITE_LOCKS.release(path)

    def support_recursive_move(self, dest_path: str) -> bool:
        return False

    def copy_move_single(
        self,
        dest_path: str,
        is_move: bool,
    ) -> bool:
        """Copy or move this resource."""
        mapper = self._get_mapper()
        dest_physical = self._resolve_destination_path(dest_path)
        if mapper is not None and dest_physical is not None:
            # A copy only writes the destination; a move also mutates source.
            lock_paths = [self.file_path, dest_physical] if is_move else [dest_physical]
            acquired = _try_acquire_all(lock_paths)
            if acquired is None:
                self._audit_lock_conflict(
                    "move" if is_move else "copy",
                    details={"destination": dest_path},
                )
                raise DAVError(_HTTP_LOCKED, _LOCKED_MESSAGE)

            try:
                if is_move:
                    _remember_primary_uuid(
                        self._get_database(), self.file_path, self._relative_under_library()
                    )
                dest_physical.parent.mkdir(parents=True, exist_ok=True)
                if is_move:
                    os.replace(self.file_path, dest_physical)
                    self._note_archive_removed(self.file_path)
                else:
                    shutil.copy2(self.file_path, dest_physical)
                self._note_archive_written(dest_physical)
                db = self._get_database()
                if db is not None:
                    old_rel = self._relative_under_library()
                    try:
                        new_rel = str(
                            dest_physical.resolve().relative_to(mapper.library_path.resolve())
                        )
                    except ValueError:
                        new_rel = None
                    if is_move and old_rel is not None and new_rel is not None:
                        db.rename_volume_upload(old_rel, new_rel)
                    if is_move:
                        _forget_ocr_records(db, old_rel)
                    _forget_ocr_records(db, new_rel, archive_too=False)
                self._audit(
                    "move" if is_move else "copy",
                    details={"destination": dest_path},
                )
                return True
            finally:
                for path in reversed(acquired):
                    _PATH_WRITE_LOCKS.release(path)
        return False


class MokuroFolderResource(DAVCollection):  # type: ignore[misc]
    """WebDAV resource for folders (both virtual and physical)."""

    # Static property list for folders (no getcontentlength).
    _PROP_NAMES = [
        "{DAV:}resourcetype",
        "{DAV:}creationdate",
        "{DAV:}getlastmodified",
        "{DAV:}displayname",
        "{DAV:}getetag",
    ]

    def __init__(
        self,
        path: str,
        environ: dict[str, Any],
        folder_path: Path | None,
        path_mapper: PathMapper,
        is_virtual: bool = False,
    ) -> None:
        super().__init__(path, environ)
        self.folder_path = folder_path
        self.path_mapper = path_mapper
        self.is_virtual = is_virtual
        self._stat: os.stat_result | None = None
        self._scandir_cache: dict[str, os.DirEntry[str]] | None = None

    def get_property_names(self, *, is_allprop: bool) -> list[str]:
        """Return static property list (no getter probing needed)."""
        return list(self._PROP_NAMES)

    def _get_stat(self) -> os.stat_result | None:
        """Get cached stat result."""
        if self._stat is None and self.folder_path:
            try:
                self._stat = os.stat(self.folder_path)
            except OSError:
                pass
        return self._stat

    def _get_username(self) -> str | None:
        """Get current username from environ."""
        user_data = self.environ.get("mokuro.user")
        if user_data:
            return cast("str | None", user_data.get("username"))
        return None

    def _get_database(self) -> Database | None:
        db = self.environ.get("mokuro.db")
        if db is None:
            return None
        return cast("Database", db)

    def _get_actor_username(self) -> str | None:
        user_data = self.environ.get("mokuro.user")
        if isinstance(user_data, dict):
            username = user_data.get("username")
            if isinstance(username, str):
                return username
        username = self.environ.get("mokuro.username")
        if isinstance(username, str):
            return username
        return None

    def _relative_under_library(self) -> str | None:
        if self.folder_path is None:
            return None
        try:
            return str(self.folder_path.resolve().relative_to(self.path_mapper.library_path.resolve()))
        except ValueError:
            return None

    def _audit(self, action: str, *, details: dict[str, Any] | None = None) -> None:
        db = self._get_database()
        if db is None:
            return
        rel = self._relative_under_library()
        target_path = f"/{PathMapper.READER_ROOT}/{rel}" if rel else self.path
        db.log_audit_event(
            action=action,
            actor_username=self._get_actor_username(),
            target_type="library_folder" if rel else "webdav_folder",
            target_path=target_path,
            details=details,
        )

    def _audit_lock_conflict(
        self, operation: str, *, details: dict[str, Any] | None = None
    ) -> None:
        payload: dict[str, Any] = {"operation": operation}
        if details:
            payload.update(details)
        self._audit("lock_conflict", details=payload)

    def _resolve_member_path(self, name: str) -> Path | None:
        """Resolve a child resource safely under this physical folder."""
        if self.folder_path is None:
            return None
        return safe_resolve_under(self.folder_path, name)

    def _resolve_destination_path(self, dest_path: str) -> Path | None:
        if self.path_mapper.get_path_type(dest_path) != "library":
            return None
        return self.path_mapper.virtual_to_physical(dest_path, self._get_actor_username())

    def _get_library_volume_paths(self) -> list[str]:
        """Collect CBZ paths relative to the library root before a folder move."""
        if self.folder_path is None:
            return []

        library_root = self.path_mapper.library_path.resolve()
        volume_paths: list[str] = []
        for candidate in self.folder_path.rglob("*"):
            if candidate.suffix.lower() != ".cbz":
                continue
            try:
                volume_paths.append(str(candidate.resolve().relative_to(library_root)))
            except ValueError:
                continue
        return volume_paths

    def get_creation_date(self) -> float | None:
        stat_result = self._get_stat()
        if stat_result:
            return stat_result.st_ctime
        return datetime.now().timestamp()

    def get_display_name(self) -> str:
        if self.path == "/":
            return "mokuro-bunko"
        if self.folder_path:
            return self.folder_path.name
        return self.path.rstrip("/").split("/")[-1] or "root"

    def get_directory_info(self) -> dict[str, Any] | None:
        return None

    def get_etag(self) -> str | None:
        stat_result = self._get_stat()
        if stat_result:
            return f"{stat_result.st_mtime:.6f}"
        return None

    def get_last_modified(self) -> float | None:
        stat_result = self._get_stat()
        if stat_result:
            return stat_result.st_mtime
        return datetime.now().timestamp()

    def get_member_names(self) -> list[str]:
        """Return list of member names."""
        normalized = self.path.rstrip("/") or "/"
        username = self._get_username()

        # Root: show mokuro-reader
        if normalized == "/":
            return [PathMapper.READER_ROOT]

        # /mokuro-reader: merge per-user files + shared library contents
        if normalized == f"/{PathMapper.READER_ROOT}":
            members: list[str] = []

            # Per-user JSON files (only if they exist for this user)
            if username:
                for name in sorted(PathMapper.PER_USER_FILES):
                    file_path = self.path_mapper.get_user_file_path(username, name)
                    if file_path and file_path.exists():
                        members.append(name)

            # Shared library contents — use scandir to cache entry metadata.
            # A per-user file name found physically in the shared folder (a
            # goals.json from before it became per-user, a stray upload) is
            # NOT a member: every name listed here must resolve in
            # `get_member`, which maps those names to the user's own copy and
            # returns None when there is none -- and wsgidav asserts on a
            # listed name that resolves to nothing, turning one stale file
            # into a 500 on the root for every user.
            try:
                cache: dict[str, os.DirEntry[str]] = {}
                with os.scandir(self.path_mapper.library_path) as it:
                    for entry in it:
                        if entry.name in PathMapper.PER_USER_FILES:
                            continue
                        cache[entry.name] = entry
                self._scandir_cache = cache
                members.extend(sorted(cache.keys()))
            except OSError:
                pass

            return members

        # Physical folder: list filesystem contents
        if self.folder_path:
            try:
                cache = {}
                with os.scandir(self.folder_path) as it:
                    for entry in it:
                        cache[entry.name] = entry
                self._scandir_cache = cache
                return list(cache.keys())
            except OSError:
                pass

        return []

    def _resource_from_entry(
        self,
        member_path: str,
        entry: os.DirEntry[str],
    ) -> DAVCollection | DAVNonCollection:
        """Create a resource from a cached DirEntry, pre-populating stat."""
        physical = Path(entry.path)
        if entry.is_dir(follow_symlinks=True):
            res = MokuroFolderResource(
                member_path, self.environ, physical, self.path_mapper,
            )
        else:
            res = MokuroFileResource(member_path, self.environ, physical)
        try:
            res._stat = entry.stat(follow_symlinks=True)
        except OSError:
            pass
        return res

    def get_member(self, name: str) -> DAVCollection | DAVNonCollection | None:
        """Get a member resource by name."""
        member_path = join_uri(self.path, name)
        normalized = self.path.rstrip("/") or "/"
        username = self._get_username()

        # Root members
        if normalized == "/":
            if name == PathMapper.READER_ROOT:
                return MokuroFolderResource(
                    f"/{PathMapper.READER_ROOT}",
                    self.environ,
                    None,
                    self.path_mapper,
                    is_virtual=True,
                )
            return None

        # /mokuro-reader members
        if normalized == f"/{PathMapper.READER_ROOT}":
            # Per-user files
            if name in PathMapper.PER_USER_FILES:
                if username:
                    file_path = self.path_mapper.get_user_file_path(username, name)
                    if not file_path:
                        return None
                    return MokuroFileResource(
                        f"/{PathMapper.READER_ROOT}/{name}",
                        self.environ,
                        file_path,
                    )
                return None

            # Fast path: use cached scandir entry (no stat/resolve needed)
            if self._scandir_cache and name in self._scandir_cache:
                return self._resource_from_entry(
                    member_path, self._scandir_cache[name],
                )

            # Fallback for uncached lookups
            physical = safe_resolve_under(self.path_mapper.library_path, name)
            if physical is None:
                return None
            if physical.is_dir():
                return MokuroFolderResource(
                    member_path,
                    self.environ,
                    physical,
                    self.path_mapper,
                )
            elif physical.exists():
                return MokuroFileResource(member_path, self.environ, physical)
            # Return resource for non-existent file (supports PUT)
            return MokuroFileResource(member_path, self.environ, physical)

        # Physical folder members
        if self.folder_path:
            # Fast path: use cached scandir entry
            if self._scandir_cache and name in self._scandir_cache:
                return self._resource_from_entry(
                    member_path, self._scandir_cache[name],
                )

            # Fallback for uncached lookups
            member_physical = self._resolve_member_path(name)
            if member_physical is None:
                return None
            if member_physical.exists():
                if member_physical.is_dir():
                    return MokuroFolderResource(
                        member_path,
                        self.environ,
                        member_physical,
                        self.path_mapper,
                    )
                else:
                    return MokuroFileResource(
                        member_path,
                        self.environ,
                        member_physical,
                    )
            # Return resource for non-existent file (supports PUT)
            return MokuroFileResource(member_path, self.environ, member_physical)

        return None

    def create_empty_resource(self, name: str) -> DAVNonCollection:
        """Create an empty file resource for PUT."""
        member_path = join_uri(self.path, name)
        normalized = self.path.rstrip("/") or "/"
        username = self._get_username()

        # /mokuro-reader: per-user files or library files
        if normalized == f"/{PathMapper.READER_ROOT}":
            if name in PathMapper.PER_USER_FILES:
                if username:
                    file_path = self.path_mapper.get_user_file_path(username, name)
                    if not file_path:
                        raise ValueError("Invalid username path")
                    file_path.parent.mkdir(parents=True, exist_ok=True)
                    return MokuroFileResource(
                        f"/{PathMapper.READER_ROOT}/{name}",
                        self.environ,
                        file_path,
                    )
                raise ValueError("Authentication required to create per-user files")
            # Library file
            file_path = safe_resolve_under(self.path_mapper.library_path, name)
            if file_path is None:
                raise DAVError(403, "Forbidden")
            file_path.parent.mkdir(parents=True, exist_ok=True)
            return MokuroFileResource(member_path, self.environ, file_path)

        # Physical folder
        if self.folder_path:
            file_path = self._resolve_member_path(name)
            if file_path is None:
                raise DAVError(403, "Forbidden")
            return MokuroFileResource(member_path, self.environ, file_path)

        raise ValueError(f"Cannot create resource at {member_path}")

    def create_collection(self, name: str) -> MokuroFolderResource:
        """Create a subdirectory (MKCOL)."""
        member_path = join_uri(self.path, name)
        normalized = self.path.rstrip("/") or "/"

        # /mokuro-reader: create series folder in shared library
        if normalized == f"/{PathMapper.READER_ROOT}":
            new_dir = safe_resolve_under(self.path_mapper.library_path, name)
            if new_dir is None:
                raise DAVError(403, "Forbidden")
            new_dir.mkdir(parents=True, exist_ok=True)
            self._audit("mkdir", details={"path": member_path})
            return MokuroFolderResource(
                member_path,
                self.environ,
                new_dir,
                self.path_mapper,
            )

        # Physical folder
        if self.folder_path:
            new_dir = self._resolve_member_path(name)
            if new_dir is None:
                raise DAVError(403, "Forbidden")
            new_dir.mkdir(parents=True, exist_ok=True)
            self._audit("mkdir", details={"path": member_path})
            return MokuroFolderResource(
                member_path,
                self.environ,
                new_dir,
                self.path_mapper,
            )

        raise ValueError(f"Cannot create collection at {member_path}")

    def copy_move_single(
        self,
        dest_path: str,
        is_move: bool,
    ) -> bool:
        """Create the destination collection for generic COPY/MOVE handling."""
        dest_physical = self._resolve_destination_path(dest_path)
        if dest_physical is None:
            return False
        dest_physical.mkdir(parents=True, exist_ok=True)
        return True

    def support_recursive_move(self, dest_path: str) -> bool:
        return (
            self.folder_path is not None
            and self._resolve_destination_path(dest_path) is not None
        )

    def move_recursive(self, dest_path: str) -> list[tuple[str, DAVError]]:
        """Move a folder tree atomically, preserving OCR sidecars and ownership."""
        if self.folder_path is None:
            raise DAVError(403, "Forbidden")

        dest_physical = self._resolve_destination_path(dest_path)
        if dest_physical is None:
            raise DAVError(403, "Forbidden")

        acquired = _try_acquire_all([self.folder_path, dest_physical])
        if acquired is None:
            self._audit_lock_conflict("move", details={"destination": dest_path})
            raise DAVError(_HTTP_LOCKED, _LOCKED_MESSAGE)

        try:
            old_rel_prefix = self._relative_under_library()
            new_rel_prefix = None
            try:
                new_rel_prefix = str(
                    dest_physical.resolve().relative_to(self.path_mapper.library_path.resolve())
                )
            except ValueError:
                new_rel_prefix = None

            volume_paths = self._get_library_volume_paths()
            dest_physical.parent.mkdir(parents=True, exist_ok=True)
            os.replace(self.folder_path, dest_physical)
            _note_removed(self.environ, self.path_mapper, self.folder_path)

            db = self._get_database()
            if db is not None and old_rel_prefix is not None and new_rel_prefix is not None:
                for old_rel in volume_paths:
                    suffix = old_rel[len(old_rel_prefix):].lstrip("/")
                    new_rel = f"{new_rel_prefix}/{suffix}" if suffix else new_rel_prefix
                    db.rename_volume_upload(old_rel, new_rel)
                # The sidecars moved with the folder: so does who wrote them,
                # and the ids their volumes are known by.
                db.rename_ocr_sidecars_under_prefix(old_rel_prefix, new_rel_prefix)
                db.rename_volume_uuids_under_prefix(old_rel_prefix, new_rel_prefix)
            elif db is not None and old_rel_prefix is not None:
                db.forget_ocr_sidecars_under_prefix(old_rel_prefix)
                db.forget_volume_uuids_under_prefix(old_rel_prefix)

            self._audit("move", details={"destination": dest_path})
            return []
        finally:
            for path in reversed(acquired):
                _PATH_WRITE_LOCKS.release(path)

    def delete(self) -> None:
        """Delete this folder."""
        if self.folder_path and self.folder_path.exists():
            if not _PATH_WRITE_LOCKS.acquire(self.folder_path):
                self._audit_lock_conflict("delete")
                raise DAVError(_HTTP_LOCKED, _LOCKED_MESSAGE)
            try:
                db = self._get_database()
                rel = self._relative_under_library()
                if db is not None and rel:
                    db.forget_volume_uploads_under_prefix(rel)
                    db.forget_ocr_sidecars_under_prefix(rel)
                    db.forget_volume_uuids_under_prefix(rel)
                shutil.rmtree(self.folder_path)
                _note_removed(self.environ, self.path_mapper, self.folder_path)
                self._audit("delete")
            finally:
                _PATH_WRITE_LOCKS.release(self.folder_path)

    def support_recursive_delete(self) -> bool:
        return True


def _note_removed(environ: dict[str, Any], mapper: PathMapper | None, path: Path) -> None:
    """Add ``path`` to the request's removed library paths (`ARCHIVES_REMOVED_KEY`)."""
    if mapper is None:
        return
    try:
        path.resolve().relative_to(mapper.library_path.resolve())
    except (OSError, ValueError):
        return
    environ.setdefault(ARCHIVES_REMOVED_KEY, []).append(path)


class _AtomicFileWriter:
    """Temporary file writer that atomically replaces the destination on close.

    An interrupted, short or rejected upload never touches the destination:
    bytes go to a temp file in the destination directory, and only a close()
    that finds them whole -- as many as ``expected_size`` announced, and
    passing the subclass's own check (`_verify`) -- publishes them via
    os.replace(). Every way it ends is recorded in ``outcome``.
    """

    #: What a successful write is called in ``UploadOutcome.verdict``.
    VERDICT = "stored"

    def __init__(
        self,
        destination: Path,
        *,
        expected_size: int | None = None,
        expected_digest: tuple[str, bytes] | None = None,
        outcome: UploadOutcome | None = None,
    ) -> None:
        self.destination = destination
        self.expected_size = expected_size
        # The body is hashed as it streams to the temp file: no second read.
        self.expected_digest = expected_digest
        self._hasher = (
            hashlib.new(_DIGEST_ALGORITHMS[expected_digest[0]][0])
            if expected_digest is not None
            else None
        )
        self.outcome = outcome if outcome is not None else UploadOutcome()
        self.destination.parent.mkdir(parents=True, exist_ok=True)
        fd, temp_name = tempfile.mkstemp(
            prefix=f".{destination.name}.upload-",
            suffix=".tmp",
            dir=str(destination.parent),
        )
        os.close(fd)
        self.temp_path = Path(temp_name)
        self._file = open(self.temp_path, "wb")
        self._closed = False

    def write(self, data: bytes) -> int:
        try:
            return self._write(data)
        except OSError as e:
            self._fail_on_os_error(e, "writing the upload")
            raise  # not reached: _fail_on_os_error raises

    def _write(self, data: bytes) -> int:
        written = self._file.write(data)
        if self._hasher is not None:
            self._hasher.update(data)
        return written

    def flush(self) -> None:
        self._file.flush()

    def fileno(self) -> int:
        return self._file.fileno()

    def tell(self) -> int:
        return self._file.tell()

    def seek(self, offset: int, whence: int = 0) -> int:
        return self._file.seek(offset, whence)

    def truncate(self, size: int | None = None) -> int:
        if size is None:
            return self._file.truncate()
        return self._file.truncate(size)

    @property
    def closed(self) -> bool:
        return self._closed

    def _discard_temp(self) -> None:
        try:
            self._file.close()
        except OSError:
            pass
        try:
            self.temp_path.unlink(missing_ok=True)
        except OSError:
            pass

    def _reject(self, status: int, reason: str, detail: str, *, retry: bool) -> None:
        """Discard the staged bytes, record why, and fail the request."""
        self._closed = True
        self._discard_temp()
        self.outcome.status = status
        self.outcome.reason = reason
        self.outcome.detail = detail
        self.outcome.retry = retry
        raise DAVError(status, detail)

    def _fail_on_os_error(self, error: OSError, doing: str) -> None:
        if error.errno in _DISK_FULL_ERRNOS:
            self._reject(
                507,
                "disk-full",
                "The server's disk is full; the upload was not stored.",
                retry=False,
            )
        self._reject(
            500,
            "server-error",
            f"The server failed {doing}: {error.strerror or error}.",
            retry=True,
        )

    def _finalize_temp(self) -> None:
        """Flush, sync and close the temp file handle.

        A flush that fails (a full disk often surfaces only here, when the
        filesystem allocates) fails the upload; an fsync the filesystem does
        not support does not.
        """
        try:
            self._file.flush()
            try:
                os.fsync(self._file.fileno())
            except OSError as e:
                if e.errno not in (errno.EINVAL, errno.ENOTSUP, errno.EROFS):
                    raise
        except OSError as e:
            self._fail_on_os_error(e, "saving the upload")
        finally:
            self._file.close()

    def _check_size(self) -> int:
        """The staged file's size; a short (or long) body is `truncated`."""
        try:
            size = self.temp_path.stat().st_size
        except OSError as e:
            self._fail_on_os_error(e, "checking the upload")
            raise  # not reached
        if self.expected_size is not None and size != self.expected_size:
            self._reject(
                422,
                "truncated",
                f"Received {size} of {self.expected_size} bytes; the upload was cut short.",
                retry=True,
            )
        return size

    def _check_digest(self) -> None:
        """A body that does not match its `Content-Digest` was damaged on the way."""
        if self.expected_digest is None or self._hasher is None:
            return
        algorithm, expected = self.expected_digest
        if self._hasher.digest() != expected:
            self._reject(
                422,
                "corrupted-in-transit",
                f"The upload does not match its {algorithm} Content-Digest: it was "
                "damaged on the way here. Sending it again should work.",
                retry=True,
            )
        self.outcome.digest_verified = algorithm

    def _verify(self) -> None:
        """A subclass's check of the staged bytes; `_reject` to refuse them."""

    def _publish(self) -> None:
        os.replace(self.temp_path, self.destination)

    def _commit(self) -> None:
        """Atomically publish the temp file to the destination."""
        try:
            self._publish()
        except OSError as e:
            self._fail_on_os_error(e, "moving the upload into place")
        # mkstemp creates with 0o600; apply umask-derived permissions instead.
        # On Windows, umask/chmod have no effect on NTFS permissions.
        if os.name != "nt":
            umask = os.umask(0)
            os.umask(umask)
            os.chmod(self.destination, 0o666 & ~umask)

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._finalize_temp()
        size = self._check_size()
        self._check_digest()
        self._verify()
        self._commit()
        _DAMAGE_MEMORY.forget(str(self.destination))
        self.outcome.verdict = self.VERDICT
        self.outcome.size = size

    def abort(self) -> None:
        """Discard the temp file without touching the destination."""
        if self._closed:
            return
        self._closed = True
        self._discard_temp()

    def writable(self) -> bool:
        return True

    def __enter__(self) -> _AtomicFileWriter:
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        if exc_type is not None:
            self.abort()
            return
        self.close()


# The first bytes of every zip: a local file header, or the end-of-central-
# directory record of an empty archive.
_ZIP_MAGIC = (b"PK\x03\x04", b"PK\x05\x06")


# An upload is a stranger's archive: its inflation is bounded before a byte of
# it is read, because verification holds a worker thread and the path's lock.
_UPLOAD_INFLATE_LIMIT = InflateLimit()


class _ValidatedCbzWriter(_AtomicFileWriter):
    """Atomic CBZ writer: publishes only an archive whose every CRC checks out.

    The check is the processor's own (`processor.archives.verify_archive`):
    the zip's structure, and every member a reader can reach read to its end
    against its CRC-32.
    """

    VERDICT = "verified"

    def _verify(self) -> None:
        try:
            with open(self.temp_path, "rb") as staged:
                head = staged.read(4)
        except OSError as e:
            self._fail_on_os_error(e, "reading the upload back")
        result = verify_archive(self.temp_path, limit=_UPLOAD_INFLATE_LIMIT)
        if result.ok:
            return
        if result.refused is not None:
            self._reject(
                422,
                "archive-refused",
                f"The archive was not accepted: {result.refused}.",
                retry=False,
            )
        if result.structural is not None and head not in _ZIP_MAGIC:
            self._reject(
                422,
                "not-an-archive",
                "The upload is not a zip archive, so it cannot be a .cbz.",
                retry=False,
            )
        damage = result.describe()
        if self.outcome.digest_verified is not None:
            # Received exactly as sent: the damage is in the client's copy.
            self._reject(
                422,
                "archive-damaged",
                f"The archive arrived intact ({self.outcome.digest_verified} matched) "
                f"but is damaged: {damage}. Your copy of it is damaged; "
                "re-import this volume.",
                retry=False,
            )
        try:
            size: int | None = self.temp_path.stat().st_size
        except OSError:
            size = self.expected_size
        signature = (size, result.structural, tuple(sorted(result.damaged)))
        if _DAMAGE_MEMORY.seen_before(str(self.destination), signature):
            self._reject(
                422,
                "archive-damaged",
                f"The archive is damaged: {damage}. The same damage arrived twice, "
                "so your copy of it is damaged; re-import this volume.",
                retry=False,
            )
        self._reject(
            422,
            "archive-damaged",
            f"The archive is damaged: {damage}. It may have been damaged on the "
            "way here; sending it again may work.",
            retry=True,
        )


class _AuditedWriter:
    """File wrapper that triggers callback only after successful close."""

    def __init__(self, inner: BinaryIO, on_commit: Callable[[], None]) -> None:
        self._inner = inner
        self._on_commit = on_commit
        self._committed = False

    def __getattr__(self, item: str) -> Any:
        return getattr(self._inner, item)

    def close(self) -> None:
        if self._committed:
            return
        self._inner.close()
        self._committed = True
        self._on_commit()

    def abort(self) -> None:
        """Discard the underlying write without committing or auditing."""
        self._committed = True
        abort = getattr(self._inner, "abort", None)
        if abort is not None:
            abort()
        else:
            self._inner.close()

    def __enter__(self) -> _AuditedWriter:
        self._inner.__enter__()
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        if exc_type is not None:
            self._inner.__exit__(exc_type, exc, tb)
            return
        self.close()


class _LockedWriter:
    """File wrapper that releases a path lock once the write finishes.

    The lock is released on close() and on abort() — including the abort
    driven by end_write(with_errors=True), where wsgidav never calls close.
    """

    def __init__(self, inner: BinaryIO, on_release: Callable[[], None]) -> None:
        self._inner = inner
        self._on_release = on_release
        self._released = False

    def __getattr__(self, item: str) -> Any:
        return getattr(self._inner, item)

    def _release(self) -> None:
        if self._released:
            return
        self._released = True
        self._on_release()

    def close(self) -> None:
        try:
            self._inner.close()
        finally:
            self._release()

    def abort(self) -> None:
        try:
            abort = getattr(self._inner, "abort", None)
            if abort is not None:
                abort()
            else:
                self._inner.close()
        finally:
            self._release()

    def __enter__(self) -> _LockedWriter:
        if hasattr(self._inner, "__enter__"):
            self._inner.__enter__()
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        try:
            if hasattr(self._inner, "__exit__"):
                self._inner.__exit__(exc_type, exc, tb)
            elif exc_type is None:
                self._inner.close()
        finally:
            self._release()
