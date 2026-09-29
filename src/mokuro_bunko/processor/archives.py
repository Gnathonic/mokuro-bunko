"""The processor's archives: downloaded whole, held in RAM, verified.

The old road read each ``.cbz`` as ONE sequential ``GET`` and stopped reading
whenever the runner's page queue was full. cheroot's socket timeout (10 s)
bounds a server's WRITES too, so a pause longer than that made the library
close the connection mid-body -- and ``http.client`` reports that as a clean
end of stream, which read as "the archive ended earlier than its directory
said" and was recorded against an archive with nothing wrong with it.

So an archive now arrives whole, at full network speed, before the runner
sees a byte of it (design sections 4.1-4.5):

* :class:`ArchiveSpool` decides where its bytes live -- an UNNAMED file in
  ``/dev/shm`` (``O_TMPFILE``) within a RAM budget, else an unnamed (or, where
  the filesystem refuses, a private named) file under the processor's own
  storage -- and accounts for every byte by HANDLE, never by name.
* :class:`ArchiveFetcher` downloads it with one ``GET``, resumes a broken
  download from the byte it reached (``Range`` + ``If-Range``, so a file
  replaced mid-download starts over rather than splicing), and gives the
  claim back with a class when it cannot (never a failure of the volume).
* :func:`verify_archive` checks the zip's structure and every member's
  CRC-32 against its own central directory: the per-block check, at zero
  cost to the library. A copy that fails is diagnosed with a second full
  download: repaired in transit, or damaged at the library (then delivered
  as it is, so the runner decides exactly as it would locally).

The runner then opens the verified file as an ordinary ARCHIVE -- the road
the library itself runs locally -- through ``/proc/<pid>/fd/<n>``.
"""

from __future__ import annotations

import contextlib
import errno
import http.client
import logging
import os
import shutil
import socket
import sys
import threading
import time
import uuid
import zipfile
import zlib
from collections.abc import Callable, Sequence
from dataclasses import dataclass, field
from pathlib import Path, PurePosixPath
from typing import Any, Protocol
from urllib.parse import quote

# The RAM the queued archives may use, across every session of a processor:
# the volume in the runner and the one on deck (`processor.archive_memory_mb`).
from mokuro_bunko.processor.config import DEFAULT_ARCHIVE_MEMORY_MB

logger = logging.getLogger(__name__)

MiB = 1 << 20
GiB = 1 << 30


SHM_DIR = Path("/dev/shm")
MEMINFO = Path("/proc/meminfo")
PROC_SELF_CGROUP = Path("/proc/self/cgroup")
CGROUP_ROOT = Path("/sys/fs/cgroup")

# What must stay free beside an archive placed in RAM: on the tmpfs itself,
# and in the machine's (or its cgroup's) available memory -- a RAM-tight
# machine running a big model must never swap, or meet the OOM killer, for an
# archive. And what must stay free on storage for the disk fallback.
SHM_MARGIN = 64 * MiB
MEMORY_MARGIN = 1 * GiB
DISK_MARGIN = 256 * MiB

# Where the disk fallback lives, under the processor's storage. Swept at
# start (`sweep_named`), which only the storage's one owner may do: the
# storage lock (`processor.cli.lock_storage`) is what makes that safe.
ARCHIVES_SUBDIR = Path(".processing") / "archives"

READ_CHUNK = 1 * MiB

# A cgroup v1 "no limit" is a number near 2**63, not a word.
_UNLIMITED = 1 << 60


class Cancel(Protocol):
    def is_set(self) -> bool: ...


class FetchCancelled(Exception):
    """The session or the processor ended while an archive was on its way.

    Nothing is said to the library about it: the library settles its own
    claims when a session closes or a processor leaves.
    """


class TransferFault(Exception):
    """An archive this processor could not deliver, and why -- by class.

    Never a failure of the volume: the claim goes back to the library
    (``volume_returned``), which judges it from the one thing only it can
    see, its own file (design section 6). ``kind`` is one of
    :data:`RETURN_CLASSES`.
    """

    def __init__(
        self,
        kind: str,
        message: str,
        *,
        status: int | None = None,
        received: int = 0,
        total: int | None = None,
        requests: int = 0,
    ) -> None:
        super().__init__(message)
        self.kind = kind
        self.status = status
        self.received = received
        self.total = total
        self.requests = requests

    def head(self) -> dict[str, Any]:
        """The counters a ``volume_returned`` event carries."""
        out: dict[str, Any] = {
            "bytes": self.received,
            "total": self.total,
            "requests": self.requests,
        }
        if self.status is not None:
            out["status"] = self.status
        return out


# The classes a returned claim carries (design section 6.1).
RETURN_CLASSES = (
    "stalled", "differs", "changed", "no_range", "mismatch",
    "missing", "rejected", "no_room", "local",
)


class SpoolFull(Exception):
    """A write into a placement hit ENOSPC or EDQUOT."""

    def __init__(self, error: OSError) -> None:
        super().__init__(str(error))
        self.errno = error.errno


# --- memory headroom -----------------------------------------------------------


def _read_int(path: Path) -> int | None:
    try:
        text = path.read_text(encoding="ascii").strip()
    except (OSError, UnicodeDecodeError):
        return None
    if text == "max":
        return None
    try:
        return int(text)
    except ValueError:
        return None


def mem_available(meminfo: Path = MEMINFO) -> int | None:
    """``MemAvailable`` in bytes, or None where the kernel does not say."""
    try:
        lines = meminfo.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeDecodeError):
        return None
    for line in lines:
        if line.startswith("MemAvailable:"):
            parts = line.split()
            try:
                return int(parts[1]) * 1024
            except (IndexError, ValueError):
                return None
    return None


def cgroup_headroom(
    proc_cgroup: Path = PROC_SELF_CGROUP, cgroup_root: Path = CGROUP_ROOT
) -> int | None:
    """What this process's memory cgroup still allows, or None for no limit.

    cgroup v2: the smallest ``memory.max - memory.current`` over the
    process's cgroup and its ancestors (a ``max`` is no limit). cgroup v1:
    ``memory.limit_in_bytes - memory.usage_in_bytes`` of the memory
    controller's cgroup. None when nothing can be read. It matters because,
    without swap, a tmpfs page is pinned and charged to the WRITER's cgroup:
    a ``MemoryMax=`` or ``docker --memory`` below the machine's RAM is the
    real limit an archive in RAM competes with.
    """
    try:
        lines = proc_cgroup.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError):
        return None
    best: int | None = None

    def consider(value: int | None) -> None:
        nonlocal best
        if value is not None:
            best = value if best is None else min(best, value)

    for line in lines:
        hierarchy, _, rest = line.partition(":")
        controllers, _, path = rest.partition(":")
        relative = path.strip().lstrip("/")
        if hierarchy == "0" and controllers == "":
            node = cgroup_root / relative if relative else cgroup_root
            while True:
                limit = _read_int(node / "memory.max")
                current = _read_int(node / "memory.current")
                if limit is not None and current is not None:
                    consider(max(0, limit - current))
                if node == cgroup_root or cgroup_root not in node.parents:
                    break
                node = node.parent
        elif "memory" in controllers.split(","):
            node = cgroup_root / "memory" / relative
            limit = _read_int(node / "memory.limit_in_bytes")
            usage = _read_int(node / "memory.usage_in_bytes")
            if limit is not None and usage is not None and limit < _UNLIMITED:
                consider(max(0, limit - usage))
    return best


def memory_headroom(
    *,
    meminfo: Path = MEMINFO,
    proc_cgroup: Path = PROC_SELF_CGROUP,
    cgroup_root: Path = CGROUP_ROOT,
) -> int | None:
    """``min(MemAvailable, cgroup headroom)``, over whichever can be read."""
    values = [
        value
        for value in (mem_available(meminfo), cgroup_headroom(proc_cgroup, cgroup_root))
        if value is not None
    ]
    return min(values) if values else None


def _disk_free(path: Path) -> int:
    return shutil.disk_usage(path).free


def _open_unnamed(directory: Path) -> int | None:
    """An unnamed file in ``directory`` (``O_TMPFILE``), or None where refused.

    None where the platform has no ``O_TMPFILE`` or the filesystem will not
    make one (``EOPNOTSUPP``, ``EISDIR``, ``EINVAL`` -- an old kernel reads
    the flag as ``O_DIRECTORY``). Any other error is real and raised.
    """
    flag = getattr(os, "O_TMPFILE", None)
    if flag is None:
        return None
    try:
        return os.open(directory, flag | os.O_RDWR | getattr(os, "O_CLOEXEC", 0), 0o600)
    except OSError as e:
        if e.errno in (errno.EOPNOTSUPP, errno.EISDIR, errno.EINVAL, errno.ENOTSUP):
            return None
        raise


def _proc_fd_usable() -> bool:
    """Whether a child can open one of our fds by path: Linux's /proc."""
    return sys.platform.startswith("linux") and Path(f"/proc/{os.getpid()}/fd").is_dir()


# --- the spool -------------------------------------------------------------------


class Placement:
    """One archive's bytes: the file, its reservation, and nothing else.

    Owns its fd, its reservation and -- for a named file -- its path.
    :meth:`release` gives back exactly that reservation and closes or
    deletes exactly that file, once. No claim id, key or directory name is
    involved anywhere, so a library restart that reuses claim ids, or an old
    bridge's late cleanup, can never touch another claim's archive.
    """

    def __init__(
        self,
        spool: ArchiveSpool,
        *,
        kind: str,
        size: int,
        fd: int,
        path: Path | None,
    ) -> None:
        self._spool = spool
        self.kind = kind
        self.size = size
        self.fd = fd
        self.path = path
        self.written = 0
        self.released = False

    @property
    def runner_path(self) -> str:
        """The path the RUNNER opens: its own description of this file.

        ``/proc/<our pid>/fd/<n>`` for an unnamed file -- the runner's handle
        keeps working after ours is closed, and the kernel frees the file
        when the last one goes, crash included -- or the named path.
        """
        if self.path is not None:
            return str(self.path)
        return f"/proc/{os.getpid()}/fd/{self.fd}"

    @property
    def read_path(self) -> str:
        """A path THIS process reopens read-only, with its own offset."""
        if self.path is not None:
            return str(self.path)
        return f"/proc/self/fd/{self.fd}"

    def write(self, data: bytes | memoryview) -> None:
        """Append ``data``. :class:`SpoolFull` on ENOSPC/EDQUOT."""
        view = memoryview(data)
        while view:
            try:
                count = os.write(self.fd, view)
            except OSError as e:
                if e.errno in (errno.ENOSPC, errno.EDQUOT):
                    raise SpoolFull(e) from e
                raise
            view = view[count:]
            before = self.written
            self.written += count
            self._spool._moved(self, before, self.written)

    def reset(self) -> None:
        """Empty the file for a download that starts over from byte 0."""
        os.ftruncate(self.fd, 0)
        os.lseek(self.fd, 0, os.SEEK_SET)
        before = self.written
        self.written = 0
        self._spool._moved(self, before, 0)

    def release(self) -> None:
        """Give back this placement's reservation and file. Idempotent."""
        self._spool._release(self)


class ArchiveSpool:
    """Where each archive's bytes live, and how much RAM they may take.

    One per processor process, shared by every session: the budget is the
    processor's, not a session's. Placement and accounting sit under one
    lock, and a placement reserves its WHOLE size at once, so the budget can
    never be overshot by two downloads placing at the same moment.
    """

    def __init__(
        self,
        storage: Path,
        *,
        memory_mb: int = DEFAULT_ARCHIVE_MEMORY_MB,
        memory_dir: Path | None = SHM_DIR,
        headroom: Callable[[], int | None] | None = None,
        free_bytes: Callable[[Path], int] | None = None,
    ) -> None:
        self.storage = Path(storage)
        self.budget = max(0, int(memory_mb)) * MiB
        self.memory_dir = Path(memory_dir) if memory_dir is not None else None
        self._headroom = headroom if headroom is not None else memory_headroom
        self._free = free_bytes if free_bytes is not None else _disk_free
        self._lock = threading.Lock()
        self._live: set[Placement] = set()
        self._in_memory = 0
        self._unwritten = 0

    @property
    def disk_dir(self) -> Path:
        return self.storage / ARCHIVES_SUBDIR

    @property
    def in_memory_bytes(self) -> int:
        """The RAM reserved right now, by every live memory placement."""
        with self._lock:
            return self._in_memory

    def place(self, size: int, *, memory: bool = True) -> Placement:
        """A new, empty placement for an archive of ``size`` bytes.

        RAM when all of these hold: it fits the budget beside what is already
        there; the tmpfs has room for it plus a margin, after subtracting
        every live memory placement's still-unwritten reservation (which is
        not in the tmpfs's ``free`` yet); and the machine -- or its cgroup --
        keeps a gigabyte beside it. Otherwise disk, when storage has room;
        otherwise :class:`TransferFault` ``no_room``. ``memory=False`` asks
        for disk outright (a download re-placed after RAM filled up).
        """
        size = max(0, int(size))
        with self._lock:
            if memory and self._memory_fits(size):
                fd = _open_unnamed(self.memory_dir) if self.memory_dir is not None else None
                if fd is not None:
                    placement = Placement(self, kind="memory", size=size, fd=fd, path=None)
                    self._in_memory += size
                    self._unwritten += size
                    self._live.add(placement)
                    return placement
            placement = self._place_on_disk(size)
            self._live.add(placement)
            return placement

    def _memory_fits(self, size: int) -> bool:
        if self.memory_dir is None or self.budget <= 0 or not _proc_fd_usable():
            return False
        if self._in_memory + size > self.budget:
            return False
        try:
            free = self._free(self.memory_dir) - self._unwritten
        except OSError:
            return False
        if free < size + SHM_MARGIN:
            return False
        headroom = self._headroom()
        return headroom is None or headroom >= size + MEMORY_MARGIN

    def _place_on_disk(self, size: int) -> Placement:
        directory = self.disk_dir
        try:
            directory.mkdir(parents=True, exist_ok=True)
            free = self._free(directory)
        except OSError as e:
            raise TransferFault("no_room", f"the processor's storage is unusable: {e}") from e
        if free < size + DISK_MARGIN:
            raise TransferFault(
                "no_room",
                f"no room for a {size / MiB:.1f} MB archive: not in memory, and "
                f"{directory} has {free / MiB:.0f} MB free",
            )
        fd = _open_unnamed(directory) if _proc_fd_usable() else None
        if fd is not None:
            return Placement(self, kind="disk", size=size, fd=fd, path=None)
        path = directory / f"{uuid.uuid4().hex}.cbz"
        flags = os.O_RDWR | os.O_CREAT | os.O_EXCL | getattr(os, "O_BINARY", 0)
        fd = os.open(path, flags | getattr(os, "O_CLOEXEC", 0), 0o600)
        return Placement(self, kind="disk", size=size, fd=fd, path=path)

    def _moved(self, placement: Placement, before: int, after: int) -> None:
        """A placement's written count went from ``before`` to ``after``.

        Only the UNWRITTEN part of a memory reservation is subtracted from
        the tmpfs's free space when placing: what is written is in it.
        """
        if placement.kind != "memory":
            return
        with self._lock:
            if placement in self._live:
                self._unwritten += max(0, placement.size - after) - max(
                    0, placement.size - before
                )

    def _release(self, placement: Placement) -> None:
        with self._lock:
            if placement.released:
                return
            placement.released = True
            self._live.discard(placement)
            if placement.kind == "memory":
                self._in_memory -= placement.size
                self._unwritten -= max(0, placement.size - placement.written)
        with contextlib.suppress(OSError):
            os.close(placement.fd)
        if placement.path is not None:
            # On Windows a file still open in the runner cannot be deleted;
            # it is left for the next start's sweep.
            with contextlib.suppress(OSError):
                placement.path.unlink()

    def sweep_named(self) -> None:
        """Empty the disk fallback directory: leftovers of an earlier run.

        Only ever the named fallback -- RAM archives are unnamed and freed by
        the kernel -- and only at start, under the storage lock.
        """
        directory = self.disk_dir
        try:
            entries = list(directory.iterdir())
        except OSError:
            return
        for entry in entries:
            with contextlib.suppress(OSError):
                if entry.is_dir() and not entry.is_symlink():
                    shutil.rmtree(entry, ignore_errors=True)
                else:
                    entry.unlink()

    def close(self) -> None:
        """Release every placement still held: every fd closed, every named
        file deleted."""
        with self._lock:
            live = list(self._live)
        for placement in live:
            placement.release()


# --- verification -------------------------------------------------------------


@dataclass
class Verified:
    """What the zip's own CRCs say about one copy of an archive."""

    # Members checked: every distinct name's resolved entry, directories out.
    members: int = 0
    # Checked members whose bytes do not match their CRC (or will not inflate).
    damaged: list[str] = field(default_factory=list)
    # Members of a method or encryption this `zipfile` cannot read: not an
    # integrity failure; the runner meets them in its own place, as locally.
    skipped: list[str] = field(default_factory=list)
    # Earlier entries shadowed by a later one of the same name: never read by
    # anyone, so never checked. Counted for the log line.
    shadowed: int = 0
    # Why the zip will not open at all, or None.
    structural: str | None = None
    # Why it was not read at all under an `InflateLimit`, or None.
    refused: str | None = None
    seconds: float = 0.0

    @property
    def ok(self) -> bool:
        return self.structural is None and self.refused is None and not self.damaged

    def describe(self) -> str:
        if self.refused is not None:
            return self.refused
        if self.structural is not None:
            return f"not a readable zip ({self.structural})"
        if self.damaged:
            return describe_damaged(self.damaged)
        return f"{self.members} members verified"


def describe_damaged(names: Sequence[str]) -> str:
    """Which members fail their CRC-32 check, by name (the first five)."""
    shown = ", ".join(repr(name) for name in names[:5])
    if len(names) == 1:
        return f"{shown} fails its CRC-32 check"
    more = f" and {len(names) - 5} more" if len(names) > 5 else ""
    return f"{shown}{more} fail their CRC-32 checks"


_INTEGRITY_ERRORS = (zipfile.BadZipFile, zlib.error, EOFError, OSError, ValueError)


_METHOD_NAMES = {
    zipfile.ZIP_BZIP2: "bzip2",
    zipfile.ZIP_LZMA: "LZMA",
}


@dataclass(frozen=True)
class InflateLimit:
    """How much reading an archive from a stranger may cost, decided unread.

    `zipfile` reads a member only to the size its directory entry DECLARES,
    so the declared total is the whole of verification's work: a 2 KB bzip2
    member declaring 2 GiB is five seconds of a worker's CPU, and a 1 MB
    upload of them is most of an hour. Pages are images that are already
    compressed, so a real volume declares little more than its own size;
    ``floor`` keeps small text-heavy archives clear of the ratio.

    Only deflate and stored members are accepted: they are what every tool
    writes a ``.cbz`` with, and all the reader's zip library opens -- so a
    bzip2 or LZMA archive is useless to a reader however it verifies.
    """

    ratio: float = 20.0
    floor: int = 256 * 1024 * 1024
    ceiling: int = 16 * 1024 * 1024 * 1024
    methods: frozenset[int] = frozenset({zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED})

    def refusal(self, infos: Sequence[zipfile.ZipInfo], archive_size: int) -> str | None:
        """Why these members are not to be read, or None."""
        odd = sorted(
            {_METHOD_NAMES.get(i.compress_type, f"method {i.compress_type}")
             for i in infos if i.compress_type not in self.methods}
        )
        if odd:
            return (
                f"its pages are compressed with {', '.join(odd)}, which a reader "
                "cannot open; re-pack it as an ordinary (deflate) zip"
            )
        declared = sum(i.file_size for i in infos)
        allowed = min(self.ceiling, max(self.floor, int(archive_size * self.ratio)))
        if declared > allowed:
            return (
                f"its pages declare {declared / 1024**3:.1f} GiB for a "
                f"{archive_size / 1024**2:.1f} MiB archive, more than any volume inflates to"
            )
        return None


def verify_archive(
    path: str | Path, *, cancel: Cancel | None = None, limit: InflateLimit | None = None
) -> Verified:
    """Check a downloaded archive against its own central directory.

    Every member a reader can reach is read to its end: for each DISTINCT
    name, the entry ``zf.getinfo(name)`` resolves to -- which is the one
    ``zf.read(name)``, and so the runner, reads. ``ZipExtFile`` checks the
    local header's name against the directory's, the inflate stream, and the
    CRC-32 at EOF. This only answers "do these bytes match their own CRCs?":
    the page list, the page count and every page verdict are the runner's
    (it may run a different Python, whose ``zipfile`` differs), so the worst
    a difference can cost is one unnecessary diagnostic download.

    ``cancel`` is looked at between members and after every 1 MiB read, so a
    multi-gigabyte archive never delays an abort: :class:`FetchCancelled`.

    ``limit`` bounds that work before any of it is done (:class:`InflateLimit`):
    an archive past it is ``refused`` unread. An upload passes one; a
    processor reading its own library's archives does not.
    """
    started = time.monotonic()
    result = Verified()

    def check() -> None:
        if cancel is not None and cancel.is_set():
            raise FetchCancelled("cancelled while verifying")

    try:
        zf = zipfile.ZipFile(path)
    except (zipfile.BadZipFile, OSError, ValueError, EOFError) as e:
        result.structural = f"{type(e).__name__}: {e}"
        result.seconds = time.monotonic() - started
        return result
    with zf:
        infos = zf.infolist()
        names = list(dict.fromkeys(info.filename for info in infos))
        result.shadowed = len(infos) - len(names)
        if limit is not None:
            reachable = [zf.getinfo(name) for name in names]
            result.refused = limit.refusal(
                [info for info in reachable if not info.is_dir()], os.path.getsize(path)
            )
            if result.refused is not None:
                result.seconds = time.monotonic() - started
                return result
        for name in names:
            check()
            info = zf.getinfo(name)
            if info.is_dir():
                continue
            result.members += 1
            try:
                with zf.open(info) as member:
                    while True:
                        chunk = member.read(READ_CHUNK)
                        check()
                        if not chunk:
                            break
            except FetchCancelled:
                raise
            except (NotImplementedError, RuntimeError):
                result.skipped.append(name)
            except _INTEGRITY_ERRORS:
                result.damaged.append(name)
    result.seconds = time.monotonic() - started
    return result


# --- the fetcher ------------------------------------------------------------------


@dataclass(frozen=True)
class FetchTiming:
    """How patient a download is. Tests shrink these; nothing else should.

    ``read_timeout`` is longer than the library's own 10 s socket timeout, so
    a stalled library shows up as ITS close rather than our timeout -- and
    longer than a spun-down disk waking up. ``stall_seconds`` is silence, not
    slowness: a slow link, or one that drops every few megabytes but keeps
    delivering, is never given up on. Any outage longer than ~10 s also
    silences the events body, and then the library drops the processor
    anyway; this budget only has to ride out download-only trouble.
    """

    connect_timeout: float = 15.0
    read_timeout: float = 30.0
    # 1, 2, 4, 8, 15, 30 s, then 30 s repeating; back to the first after any
    # attempt that received new bytes.
    retry_delays: tuple[float, ...] = (1.0, 2.0, 4.0, 8.0, 15.0, 30.0)
    stall_seconds: float = 120.0
    max_restarts: int = 3
    # The longest `Retry-After` honoured.
    retry_after_cap: float = 30.0
    # No progress is offered for a download shorter than one ping tick...
    progress_after: float = 3.0
    # ...and at most this often while one runs.
    progress_every: float = 0.5


@dataclass
class _Copy:
    """One full copy of the file, as far as it has arrived."""

    placement: Placement | None = None
    received: int = 0
    total: int | None = None
    etag: str | None = None
    crc: int = 0

    def reset(self) -> None:
        if self.placement is not None:
            self.placement.reset()
        self.received = 0
        self.crc = 0

    def release(self) -> None:
        if self.placement is not None:
            self.placement.release()


@dataclass
class FetchedArchive:
    """A verified archive (or one proven damaged at the library), held.

    ``release`` is idempotent and returns the placement's reservation; the
    bridge calls it BEFORE forwarding the claim's terminal event, so a
    processor never holds more than its lookahead's archives at once.
    """

    placement: Placement
    size: int
    crc32: int
    requests: int
    restarts: int
    repairs: int
    seconds: float
    verify_seconds: float
    members: int
    damaged: list[str] = field(default_factory=list)
    structural: str | None = None
    verdict: str | None = None
    shadowed: int = 0

    @property
    def runner_path(self) -> str:
        return self.placement.runner_path

    def summary(self) -> dict[str, Any]:
        """The numbers a ``fetch {state: ready}`` event carries."""
        out: dict[str, Any] = {
            "bytes": self.size,
            "seconds": round(self.seconds, 3),
            "mb_per_s": round(self.size / 1e6 / self.seconds, 1) if self.seconds > 0 else None,
            "requests": self.requests,
            "restarts": self.restarts,
            "repairs": self.repairs,
            "verify_seconds": round(self.verify_seconds, 3),
            "placement": self.placement.kind,
            "crc32": f"{self.crc32:08x}",
            "members": self.members,
        }
        if self.verdict:
            out["verdict"] = self.verdict
            out["damaged"] = list(self.damaged)
            if self.structural:
                out["structural"] = self.structural[:200]
        return out

    def release(self) -> None:
        self.placement.release()


class _Download:
    """One fetch's counters, its silence clock and its live connection."""

    def __init__(
        self,
        label: str,
        cancel: Cancel,
        progress: Callable[[dict[str, Any]], None] | None,
        timing: FetchTiming,
    ) -> None:
        self.label = label
        # What messages call the archive: its library path, unquoted.
        self.shown = label
        self.cancel = cancel
        self.progress = progress
        self.timing = timing
        self.started = time.monotonic()
        # When a new byte last arrived, across every attempt of every copy:
        # the stall budget is SILENCE, measured from here.
        self.last_progress = self.started
        self.requests = 0
        self.restarts = 0
        self.no_range_restarts = 0
        self.repairs = 0
        self.last_error = ""
        self.connection: Any = None
        self.lock = threading.Lock()
        self._offered_at = 0.0

    def check(self) -> None:
        if self.cancel.is_set():
            raise FetchCancelled(f"{self.label}: cancelled")

    def offer(self, head: dict[str, Any], *, force: bool = False) -> None:
        """Hand the latest progress to whoever reports it. Never blocks."""
        if self.progress is None:
            return
        now = time.monotonic()
        if now - self.started < self.timing.progress_after:
            return
        if not force and now - self._offered_at < self.timing.progress_every:
            return
        self._offered_at = now
        try:
            self.progress(head)
        except Exception:  # pragma: no cover - a progress sink never breaks a download
            logger.debug("offering fetch progress failed", exc_info=True)

    def wait(self, seconds: float) -> None:
        waiter = getattr(self.cancel, "wait", None)
        if callable(waiter):
            waiter(max(0.0, seconds))
        else:  # pragma: no cover - a bare flag
            time.sleep(max(0.0, seconds))
        self.check()


class _Transport(Exception):
    """A request that failed for a reason worth retrying on the backoff."""

    def __init__(
        self, message: str, *, retry_after: float | None = None, server_error: bool = False
    ) -> None:
        super().__init__(message)
        self.retry_after = retry_after
        self.server_error = server_error


class _Restart(Exception):
    """This copy must start over with a plain GET."""

    def __init__(self, reason: str) -> None:
        super().__init__(reason)


class _Replaced(Exception):
    """The copy moved from RAM to disk and starts again at once."""


def _strong_etag(value: str | None) -> str | None:
    """The ETag exactly as sent, when it is a strong one; None otherwise."""
    if not value:
        return None
    value = value.strip()
    if value.startswith("W/") or not (value.startswith('"') and value.endswith('"')):
        return None
    return value


def _content_range(value: str | None) -> tuple[int, int, int | None] | None:
    """``bytes START-END/TOTAL`` -> (start, end, total or None)."""
    if not value or not value.startswith("bytes "):
        return None
    span, _, total = value[len("bytes ") :].partition("/")
    first, _, last = span.partition("-")
    try:
        return int(first), int(last), (None if total.strip() in ("", "*") else int(total))
    except ValueError:
        return None


def _int_header(value: str | None) -> int | None:
    try:
        return int(value) if value is not None else None
    except ValueError:
        return None


def _describe(error: BaseException) -> str:
    text = str(error)
    if isinstance(error, TimeoutError) or "timed out" in text:
        return "timed out"
    return f"{type(error).__name__}: {text}" if text else type(error).__name__


def _mb(value: int | None) -> str:
    return "?" if value is None else f"{value / 1e6:.1f}"


class ArchiveFetcher:
    """Whole archives off the library, resumed, verified, held in the spool.

    Uses the :class:`~mokuro_bunko.processor.client.LibraryClient`'s own
    connections -- its TLS context and ``tls_verify`` -- and its auth header.
    One fetcher per processor; each session's feeder runs its own downloads
    through it, one at a time, and :meth:`abort` cuts the ones a stopping
    session (or a leaving processor) is running.
    """

    def __init__(
        self, client: Any, spool: ArchiveSpool, timing: FetchTiming | None = None
    ) -> None:
        self.client = client
        self.spool = spool
        self.timing = timing or FetchTiming()
        self._lock = threading.Lock()
        self._active: set[_Download] = set()

    # -- control -------------------------------------------------------------

    def abort(self, cancel: Cancel | None = None) -> None:
        """Cut the downloads running under ``cancel`` (every one, for None).

        Shuts the socket down rather than closing it, the same trick as
        `LibraryClient.close`: a read blocked on it returns at once. The
        caller has set ``cancel`` first, so the download then ends with
        :class:`FetchCancelled` and says nothing to the library.
        """
        with self._lock:
            downloads = [d for d in self._active if cancel is None or d.cancel is cancel]
        for download in downloads:
            with download.lock:
                connection = download.connection
            sock = getattr(connection, "sock", None) if connection is not None else None
            if sock is not None:
                with contextlib.suppress(OSError):
                    sock.shutdown(socket.SHUT_RDWR)

    # -- one archive ------------------------------------------------------------

    def fetch(
        self,
        url_path: str,
        *,
        size: int | None,
        cancel: Cancel,
        progress: Callable[[dict[str, Any]], None] | None = None,
        label: str | None = None,
    ) -> FetchedArchive:
        """Download, verify and hold one archive -- or raise why not.

        Raises :class:`TransferFault` (give the claim back, with its class),
        :class:`FetchCancelled` (say nothing), ``LibraryTransportError`` (the
        account was refused: step away), or anything else for a bug or a
        local fault (the caller's catch-all gives the claim back as
        ``local``). Whatever it raises, nothing it placed is left held.

        The path is quoted from its BYTES (``surrogateescape``), so a library
        filename that is not valid UTF-8 cannot raise in ``quote()``: the
        library answers whatever it answers, and a 404 is judged there.
        """
        path = self.client.root + quote(url_path.encode("utf-8", "surrogateescape"), safe="/")
        shown = "/".join(PurePosixPath(url_path).parts[-2:]) or url_path
        download = _Download(label or shown, cancel, progress, self.timing)
        download.shown = shown
        with self._lock:
            self._active.add(download)
        # Both copies are released on ANY way out but the one handed over:
        # a cancel, a bug or a refused account while the second (diagnostic)
        # copy is verified must not keep its reservation and fd until exit.
        # `release` is idempotent, so a copy both names point at is fine.
        copy: _Copy | None = None
        second: _Copy | None = None
        try:
            copy = self._download(path, size, download)
            seconds = time.monotonic() - download.started
            verified = self._verify(copy, download)
            while not verified.ok:
                logger.warning(
                    "%s: %s; downloading a second copy to tell damage from a bad transfer",
                    download.label, verified.describe(),
                )
                second = self._download(path, size, download)
                if second.etag != copy.etag or second.total != copy.total:
                    # The file changed between the copies: the second IS the
                    # file now, judged on its own.
                    reason = f"the library's copy changed ({copy.etag} -> {second.etag})"
                    copy.release()
                    copy = second
                    self._restart(download, reason, no_range=False, copy=copy)
                    verified = self._verify(copy, download)
                    continue
                again = self._verify(second, download)
                if again.ok:
                    logger.warning(
                        "%s: the second copy is clean: the first was corrupted in transit",
                        download.label,
                    )
                    copy.release()
                    copy, verified = second, again
                    download.repairs += 1
                    break
                if second.received == copy.received and second.crc == copy.crc:
                    second.release()
                    logger.warning(
                        "%s: damaged at the library (the same bytes on two downloads): %s; "
                        "the runner gets it as it is",
                        download.label, verified.describe(),
                    )
                    return self._held(
                        copy, download, seconds, verified, verdict="damaged at the library"
                    )
                copy.release()
                second.release()
                raise TransferFault(
                    "differs",
                    "two downloads failed their CRC checks with different bytes "
                    f"({verified.describe()}; then {again.describe()})",
                    received=second.received, total=second.total, requests=download.requests,
                )
            return self._held(copy, download, seconds, verified)
        except BaseException:
            for held in (copy, second):
                if held is not None:
                    held.release()
            raise
        finally:
            with self._lock:
                self._active.discard(download)

    def _held(
        self,
        copy: _Copy,
        download: _Download,
        seconds: float,
        verified: Verified,
        *,
        verdict: str | None = None,
    ) -> FetchedArchive:
        assert copy.placement is not None
        fetched = FetchedArchive(
            placement=copy.placement,
            size=copy.received,
            crc32=copy.crc & 0xFFFFFFFF,
            requests=download.requests,
            restarts=download.restarts,
            repairs=download.repairs,
            seconds=seconds,
            verify_seconds=verified.seconds,
            members=verified.members,
            damaged=list(verified.damaged) if verdict else [],
            structural=verified.structural if verdict else None,
            verdict=verdict,
            shadowed=verified.shadowed,
        )
        logger.info(
            "fetched %s: %s MB in %.2f s (%s MB/s) to %s, %d request%s%s%s; "
            "%d members verified in %.2f s (crc32 %08x)%s",
            download.label, _mb(copy.received), seconds,
            fetched.summary()["mb_per_s"], copy.placement.kind,
            download.requests, "" if download.requests == 1 else "s",
            f", {download.restarts} restart(s)" if download.restarts else "",
            f", {download.repairs} repair(s)" if download.repairs else "",
            verified.members, verified.seconds, fetched.crc32,
            f", {verified.shadowed} shadowed" if verified.shadowed else "",
        )
        return fetched

    def _verify(self, copy: _Copy, download: _Download) -> Verified:
        assert copy.placement is not None
        return verify_archive(copy.placement.read_path, cancel=download.cancel)

    def _restart(
        self, download: _Download, reason: str, *, no_range: bool, copy: _Copy
    ) -> None:
        """Count one restart; give the claim back past ``max_restarts``.

        More than ``max_restarts`` returns it as ``changed`` -- the library's
        copy kept changing -- or, when every restart was a resume answered
        with the whole file under the same (or no) ETag, as ``no_range``: a
        proxy that ignores Range, so no broken download can ever resume.
        """
        download.restarts += 1
        if no_range:
            download.no_range_restarts += 1
        limit = self.timing.max_restarts
        if download.restarts > limit:
            if download.no_range_restarts == download.restarts:
                raise TransferFault(
                    "no_range",
                    "the library (or a proxy in front of it) ignores Range, so a broken "
                    "download cannot resume",
                    received=copy.received, total=copy.total, requests=download.requests,
                )
            raise TransferFault(
                "changed",
                f"the library's copy kept changing while it was read ({reason})",
                received=copy.received, total=copy.total, requests=download.requests,
            )
        logger.warning(
            "%s: %s; starting over (%d of %d)", download.label, reason, download.restarts, limit
        )
        download.offer(
            {"state": "restarting", "error": reason[:300], "restarts": download.restarts},
            force=True,
        )

    def _download(self, path: str, size: int | None, download: _Download) -> _Copy:
        """One full copy of the file, across as many requests as it takes."""
        timing = self.timing
        copy = _Copy()
        backoff = 0
        server_errors = 0
        try:
            while True:
                download.check()
                heard_from = download.last_progress
                try:
                    self._attempt(path, size, copy, download)
                    return copy
                except _Replaced:
                    continue
                except _Restart as restart:
                    self._restart(download, str(restart), no_range=False, copy=copy)
                    copy.reset()
                    copy.etag = None
                    copy.total = None
                    backoff = 0
                    continue
                except _Transport as e:
                    download.last_error = str(e)
                    server_errors = server_errors + 1 if e.server_error else 0
                    retry_after = e.retry_after
                if server_errors >= 2:
                    raise TransferFault(
                        "stalled",
                        f"the library answered {download.last_error.rsplit(' ', 1)[-1]} "
                        f"twice in a row for {download.shown}",
                        status=500, received=copy.received, total=copy.total,
                        requests=download.requests,
                    )
                if download.last_progress > heard_from:
                    backoff = 0  # new bytes arrived: the next wait is the first again
                delays = timing.retry_delays or (1.0,)
                wait = (
                    min(retry_after, timing.retry_after_cap)
                    if retry_after is not None
                    else delays[min(backoff, len(delays) - 1)]
                )
                backoff += 1
                silent = time.monotonic() - download.last_progress
                if silent >= timing.stall_seconds:
                    logger.error(
                        "%s: no new byte for %.0f s after %d requests (last: %s); "
                        "giving it back (stalled)",
                        download.label, silent, download.requests, download.last_error,
                    )
                    raise TransferFault(
                        "stalled",
                        f"no new byte for {silent:.0f} s at byte {copy.received:,} of "
                        f"{copy.total if copy.total is not None else '?'} "
                        f"(last: {download.last_error})",
                        received=copy.received, total=copy.total, requests=download.requests,
                    )
                wait = min(wait, timing.stall_seconds - silent)
                logger.warning(
                    "%s: request %d ended after %s of %s MB (%s); resuming in %.1f s",
                    download.label, download.requests, _mb(copy.received), _mb(copy.total),
                    download.last_error, wait,
                )
                download.offer(
                    {
                        "state": "retrying", "bytes": copy.received, "total": copy.total,
                        "requests": download.requests, "error": download.last_error[:300],
                        "retry_in": round(wait, 1),
                    },
                    force=True,
                )
                download.wait(wait)
        except BaseException:
            copy.release()
            raise

    def _attempt(self, path: str, size: int | None, copy: _Copy, download: _Download) -> None:
        """One request, read to its end. Returns once the copy is complete.

        Raises `_Transport` (retry on the backoff), `_Restart` (start over
        with a plain GET), `_Replaced` (moved to disk: go again at once), a
        :class:`TransferFault`, or ``LibraryTransportError``.
        """
        from mokuro_bunko.processor.client import LibraryTransportError

        timing = self.timing
        resuming = copy.received > 0
        extra: dict[str, str] = {}
        if resuming:
            extra["Range"] = f"bytes={copy.received}-"
            if copy.etag:
                extra["If-Range"] = copy.etag
        download.requests += 1
        connection = self.client.connect(timeout=timing.connect_timeout)
        with download.lock:
            download.connection = connection
        try:
            try:
                connection.connect()
                if connection.sock is not None:
                    connection.sock.settimeout(timing.read_timeout)
                download.check()
                connection.request("GET", path, headers=self.client.request_headers(**extra))
                response = connection.getresponse()
            except OSError as e:
                # Refused, unreachable, DNS, TLS, a timeout, a reset, a
                # connection dropped before a status line (RemoteDisconnected).
                # A garbled answer (LineTooLong, BadStatusLine) is NOT retried:
                # it propagates, and the feeder gives the claim back as local.
                download.check()
                raise _Transport(_describe(e)) from e
            status = response.status
            if response.getheader("X-Accel-Redirect"):
                raise TransferFault(
                    "mismatch",
                    f"the library sent an X-Accel-Redirect for {download.shown}: an offload with no "
                    "nginx in front of it (MOKURO_NGINX_ACCEL without the proxy)",
                    status=status, requests=download.requests,
                )
            if status in (401, 403, 407):
                raise LibraryTransportError(f"the library answered {status} for {download.shown}")
            if status in (404, 410):
                raise TransferFault(
                    "missing", f"the library has no {download.shown} ({status})",
                    status=status, requests=download.requests,
                )
            if status in (412, 416):
                raise _Restart(f"the library answered {status} to a resume")
            if status in (408, 429) or status >= 500:
                with contextlib.suppress(OSError, http.client.HTTPException):
                    response.read()
                retry_after = _int_header(response.getheader("Retry-After"))
                raise _Transport(
                    f"the library answered {status}",
                    retry_after=float(max(0, retry_after)) if retry_after is not None else None,
                    # 500 twice in a row is usually THIS file -- one the
                    # library cannot open -- and the library's own read then
                    # decides (design 6.2). 502/503/504 are the path's.
                    server_error=status >= 500 and status not in (502, 503, 504),
                )
            if status not in (200, 206):
                raise TransferFault(
                    "rejected", f"the library answered {status} for {download.shown}",
                    status=status, requests=download.requests,
                )
            etag = _strong_etag(response.getheader("ETag"))
            length = _int_header(response.getheader("Content-Length"))
            if status == 206:
                span = _content_range(response.getheader("Content-Range"))
                if not resuming or span is None:
                    raise _Restart("the library sent a partial answer to a plain GET")
                start, _end, total = span
                if (
                    start != copy.received
                    or (total is not None and copy.total is not None and total != copy.total)
                    or (etag is not None and copy.etag is not None and etag != copy.etag)
                ):
                    raise _Restart(
                        f"the library's copy changed while it was read ({copy.etag} -> {etag})"
                    )
                expected = total if total is not None else copy.total
            else:
                total = length if length is not None else size
                if resuming:
                    # The file changed (If-Range did not match), or something
                    # ignored Range: either way THIS body is the whole file,
                    # and it is taken as the new copy from byte 0.
                    changed = etag is not None and copy.etag is not None and etag != copy.etag
                    self._restart(
                        download,
                        f"the library's copy changed while it was read ({copy.etag} -> {etag})"
                        if changed
                        else "the library answered a resume with the whole file",
                        no_range=not changed,
                        copy=copy,
                    )
                    copy.reset()
                if (
                    copy.placement is not None
                    and total is not None
                    and total != copy.placement.size
                ):
                    # A different file than the one placed for: placed again,
                    # so the reservation is always the real size.
                    copy.placement.release()
                    copy.placement = None
                copy.total = total
                copy.etag = etag
                expected = total
            if size is not None and expected is not None and expected != size:
                raise TransferFault(
                    "mismatch",
                    f"the library sent {expected:,} bytes for {download.shown}, which it said is "
                    f"{size:,} bytes",
                    status=status, total=expected, requests=download.requests,
                )
            if copy.placement is None:
                known = expected if expected is not None else size
                copy.placement = self.spool.place(known or 0, memory=known is not None)
            self._read_body(response, copy, download)
            if expected is not None and copy.received != expected:
                if copy.received > expected:
                    raise _Restart(
                        f"the library sent {copy.received:,} bytes of a {expected:,}-byte file"
                    )
                raise _Transport(
                    f"the connection closed at byte {copy.received:,} of {expected:,}"
                )
            copy.total = copy.received
        finally:
            with download.lock:
                download.connection = None
            with contextlib.suppress(Exception):
                connection.close()

    def _read_body(self, response: Any, copy: _Copy, download: _Download) -> None:
        """Read the body into the copy until ``read`` says it is over.

        ``read`` returning ``b""`` is NOT completion: ``http.client`` returns
        it for a connection closed before ``Content-Length`` without raising
        (Defect B). The caller checks the count it was promised.
        """
        while True:
            download.check()
            cut: BaseException | None = None
            try:
                chunk = response.read(READ_CHUNK)
            except http.client.IncompleteRead as e:
                # A chunked body cut mid-chunk. The whole chunks it read
                # before the cut are good bytes: kept, and resumed after.
                chunk, cut = bytes(e.partial or b""), e
            except OSError as e:
                download.check()
                raise _Transport(_describe(e)) from e
            if not chunk:
                if cut is not None:
                    download.check()
                    raise _Transport(_describe(cut)) from cut
                return
            assert copy.placement is not None
            try:
                copy.placement.write(chunk)
            except SpoolFull as e:
                if copy.placement.kind != "memory":
                    raise TransferFault(
                        "no_room", f"the processor's storage filled up: {e}",
                        received=copy.received, total=copy.total, requests=download.requests,
                    ) from e
                logger.warning(
                    "%s: /dev/shm is full (%s) at %s of %s MB; downloading again to disk",
                    download.label, errno.errorcode.get(e.errno or 0, "full"),
                    _mb(copy.received), _mb(copy.total),
                )
                known = copy.placement.size
                copy.placement.release()
                copy.placement = self.spool.place(known, memory=False)
                copy.received = 0
                copy.crc = 0
                raise _Replaced() from e
            copy.crc = zlib.crc32(chunk, copy.crc)
            copy.received += len(chunk)
            download.last_progress = time.monotonic()
            download.offer(
                {
                    "state": "downloading", "bytes": copy.received, "total": copy.total,
                    "requests": download.requests,
                }
            )
            if cut is not None:
                download.check()
                raise _Transport(_describe(cut)) from cut
