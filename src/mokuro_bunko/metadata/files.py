"""Publishing compiled bytes to the library tree.

Two rules, both from contract §4: a file is rewritten ONLY when its bytes
changed (clients version their caches on size/mtime, so a no-op rewrite makes
every device re-download), and a rewrite is atomic (a reader mid-GET never
sees a half-written document).
"""

from __future__ import annotations

import os
import tempfile
from pathlib import Path

from wsgidav.dav_provider import DAVError

from mokuro_bunko.webdav.resources import path_write_lock


class MetadataWriteBusy(RuntimeError):
    """The path is locked by a DAV write; retry on the next regeneration."""


def atomic_write_bytes(path: Path, data: bytes) -> None:
    """Write *data* to *path* via a temp file in the same directory."""
    path.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temp_name = tempfile.mkstemp(
        prefix=f".{path.name}.compile-", suffix=".tmp", dir=str(path.parent)
    )
    temp_path = Path(temp_name)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temp_path, path)
    except BaseException:
        temp_path.unlink(missing_ok=True)
        raise
    if os.name != "nt":
        # mkstemp creates 0o600; match the umask like _AtomicFileWriter does,
        # or the file becomes unreadable to the download path under nginx.
        umask = os.umask(0)
        os.umask(umask)
        os.chmod(path, 0o666 & ~umask)


def write_if_changed(path: Path, data: bytes) -> bool:
    """Publish *data* unless the file already says exactly that.

    Returns True when the file was written.
    """
    try:
        if path.read_bytes() == data:
            return False
    except OSError:
        pass  # missing or unreadable: write it
    try:
        with path_write_lock(path):
            atomic_write_bytes(path, data)
    except DAVError as error:
        raise MetadataWriteBusy(str(path)) from error
    return True
