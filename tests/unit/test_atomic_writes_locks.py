"""Tests for atomic uploads and per-path write locks on WebDAV resources.

Ports the atomic-write + path-lock ideas from PR #2 (by MokuroEnjoyer):
  - _PathWriteLocks: process-local registry; conflicting concurrent write
    operations (same path, or ancestor/descendant) get DAVError 423.
  - _AtomicFileWriter: non-CBZ uploads write to a temp file and atomically
    os.replace() on close, so an interrupted upload never leaves a
    truncated file (CBZ uploads already had this via _ValidatedCbzWriter).
  - end_write(with_errors=True): wsgidav's abort path discards the temp
    file and releases the lock (wsgidav does NOT close the file object on
    error, so cleanup must happen here).
"""

from __future__ import annotations

import zipfile
from pathlib import Path

import pytest
from wsgidav.dav_provider import DAVError

from mokuro_bunko.webdav.provider import MokuroDAVProvider
from mokuro_bunko.webdav.resources import (
    _PATH_WRITE_LOCKS,
    MokuroFileResource,
    _AtomicFileWriter,
    _PathWriteLocks,
)


@pytest.fixture(autouse=True)
def _clean_global_locks() -> None:
    """The registry is module-global; keep tests independent."""
    _PATH_WRITE_LOCKS._locks.clear()


def _make_resource(storage_base: Path, physical_path: Path) -> MokuroFileResource:
    provider = MokuroDAVProvider(storage_base)
    environ: dict[str, object] = {"wsgidav.provider": provider}
    virtual = f"/mokuro-reader/{physical_path.parent.name}/{physical_path.name}"
    return MokuroFileResource(virtual, environ, physical_path)


def _library_path(storage_base: Path, rel: str) -> Path:
    p = storage_base / "library" / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    return p


def _valid_cbz_bytes() -> bytes:
    import io

    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as zf:
        zf.writestr("page1.jpg", b"fake image data")
    return buf.getvalue()


class TestPathWriteLocks:
    """Registry semantics: same-path and ancestor/descendant conflicts."""

    def test_acquire_and_release(self, tmp_path: Path) -> None:
        locks = _PathWriteLocks()
        target = tmp_path / "a.txt"
        assert locks.acquire(target)
        assert not locks.acquire(target)
        locks.release(target)
        assert locks.acquire(target)

    def test_parent_folder_conflicts_with_child_file(self, tmp_path: Path) -> None:
        locks = _PathWriteLocks()
        folder = tmp_path / "series"
        child = folder / "vol.cbz"
        assert locks.acquire(folder)
        assert not locks.acquire(child)
        locks.release(folder)
        assert locks.acquire(child)
        assert not locks.acquire(folder)

    def test_case_insensitive_conflict(self, tmp_path: Path) -> None:
        locks = _PathWriteLocks()
        assert locks.acquire(tmp_path / "Series" / "Vol.cbz")
        assert not locks.acquire(tmp_path / "series" / "vol.CBZ")

    def test_unrelated_paths_do_not_conflict(self, tmp_path: Path) -> None:
        locks = _PathWriteLocks()
        assert locks.acquire(tmp_path / "a" / "x.txt")
        assert locks.acquire(tmp_path / "b" / "x.txt")

    def test_blocking_acquire_rejected(self, tmp_path: Path) -> None:
        locks = _PathWriteLocks()
        with pytest.raises(ValueError):
            locks.acquire(tmp_path / "a.txt", blocking=True)


class TestAtomicFileWriter:
    """Temp-file writes with atomic replace on close."""

    def test_close_commits_atomically(self, tmp_path: Path) -> None:
        dest = tmp_path / "sidecar.mokuro"
        writer = _AtomicFileWriter(dest)
        writer.write(b'{"pages": []}')
        assert not dest.exists()  # nothing visible until commit
        writer.close()
        assert dest.read_bytes() == b'{"pages": []}'
        assert not list(tmp_path.glob(".*.tmp"))

    def test_overwrite_keeps_old_content_until_commit(self, tmp_path: Path) -> None:
        dest = tmp_path / "sidecar.mokuro"
        dest.write_bytes(b"old")
        writer = _AtomicFileWriter(dest)
        writer.write(b"new-content")
        assert dest.read_bytes() == b"old"
        writer.close()
        assert dest.read_bytes() == b"new-content"

    def test_abort_discards_temp_and_preserves_destination(self, tmp_path: Path) -> None:
        dest = tmp_path / "sidecar.mokuro"
        dest.write_bytes(b"old")
        writer = _AtomicFileWriter(dest)
        writer.write(b"partial garbage")
        writer.abort()
        assert dest.read_bytes() == b"old"
        assert not list(tmp_path.glob(".*upload*"))

    def test_exception_in_context_discards_temp(self, tmp_path: Path) -> None:
        dest = tmp_path / "sidecar.mokuro"
        with pytest.raises(RuntimeError):
            with _AtomicFileWriter(dest) as writer:
                writer.write(b"partial")
                raise RuntimeError("upload interrupted")
        assert not dest.exists()
        assert not list(tmp_path.glob(".*upload*"))


class TestBeginWriteLocking:
    """begin_write takes the path lock; conflicts get 423."""

    def test_concurrent_write_same_file_is_423(self, temp_dir: Path) -> None:
        target = _library_path(temp_dir, "Series/notes.txt")
        res_a = _make_resource(temp_dir, target)
        res_b = _make_resource(temp_dir, target)

        writer = res_a.begin_write()
        try:
            with pytest.raises(DAVError) as exc_info:
                res_b.begin_write()
            assert exc_info.value.value == 423
        finally:
            writer.write(b"data")
            writer.close()

    def test_lock_released_after_close(self, temp_dir: Path) -> None:
        target = _library_path(temp_dir, "Series/notes.txt")
        writer = _make_resource(temp_dir, target).begin_write()
        writer.write(b"first")
        writer.close()

        second = _make_resource(temp_dir, target).begin_write()
        second.write(b"second")
        second.close()
        assert target.read_bytes() == b"second"

    def test_delete_conflicts_with_active_write(self, temp_dir: Path) -> None:
        target = _library_path(temp_dir, "Series/notes.txt")
        target.write_bytes(b"existing")
        writer = _make_resource(temp_dir, target).begin_write()
        try:
            with pytest.raises(DAVError) as exc_info:
                _make_resource(temp_dir, target).delete()
            assert exc_info.value.value == 423
        finally:
            writer.write(b"data")
            writer.close()

    def test_lock_released_when_writer_construction_fails(
        self, temp_dir: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        target = _library_path(temp_dir, "Series/notes.txt")
        res = _make_resource(temp_dir, target)

        import mokuro_bunko.webdav.resources as resources_mod

        def boom(*args: object, **kwargs: object) -> None:
            raise OSError("disk full")

        monkeypatch.setattr(resources_mod, "_AtomicFileWriter", boom)
        with pytest.raises(Exception):
            res.begin_write()
        monkeypatch.undo()

        # Lock must not have leaked.
        writer = _make_resource(temp_dir, target).begin_write()
        writer.write(b"ok")
        writer.close()


class TestEndWriteAbort:
    """wsgidav's error path (end_write with_errors=True) cleans up fully."""

    def test_aborted_noncbz_upload_leaves_no_partial_file(self, temp_dir: Path) -> None:
        target = _library_path(temp_dir, "Series/notes.txt")
        res = _make_resource(temp_dir, target)
        writer = res.begin_write()
        writer.write(b"partial data that must not survive")
        res.end_write(with_errors=True)

        assert not target.exists()
        assert not list(target.parent.glob(".*upload*"))
        # And the lock is released.
        follow_up = _make_resource(temp_dir, target).begin_write()
        follow_up.write(b"clean")
        follow_up.close()
        assert target.read_bytes() == b"clean"

    def test_aborted_overwrite_preserves_original(self, temp_dir: Path) -> None:
        target = _library_path(temp_dir, "Series/notes.txt")
        target.write_bytes(b"original")
        res = _make_resource(temp_dir, target)
        writer = res.begin_write()
        writer.write(b"garbage")
        res.end_write(with_errors=True)
        assert target.read_bytes() == b"original"

    def test_aborted_cbz_upload_discards_temp_and_releases_lock(
        self, temp_dir: Path
    ) -> None:
        target = _library_path(temp_dir, "Series/Vol 1.cbz")
        res = _make_resource(temp_dir, target)
        writer = res.begin_write()
        writer.write(b"not a real zip, interrupted anyway")
        res.end_write(with_errors=True)

        assert not target.exists()
        assert not list(target.parent.glob(".*upload*"))
        follow_up = _make_resource(temp_dir, target).begin_write()
        follow_up.write(_valid_cbz_bytes())
        follow_up.close()
        assert zipfile.is_zipfile(target)

    def test_end_write_without_errors_is_noop(self, temp_dir: Path) -> None:
        target = _library_path(temp_dir, "Series/notes.txt")
        res = _make_resource(temp_dir, target)
        writer = res.begin_write()
        writer.write(b"complete")
        writer.close()
        res.end_write(with_errors=False)
        assert target.read_bytes() == b"complete"


class TestMoveLocking:
    """Moves lock source and destination; conflicts get 423."""

    def test_move_blocked_while_destination_being_written(self, temp_dir: Path) -> None:
        src = _library_path(temp_dir, "Series/a.txt")
        src.write_bytes(b"src")
        dest = _library_path(temp_dir, "Series/b.txt")

        dest_writer = _make_resource(temp_dir, dest).begin_write()
        try:
            res = _make_resource(temp_dir, src)
            with pytest.raises(DAVError) as exc_info:
                res.handle_move(f"/mokuro-reader/{dest.parent.name}/{dest.name}")
            assert exc_info.value.value == 423
            assert src.read_bytes() == b"src"
        finally:
            dest_writer.write(b"dest")
            dest_writer.close()

    def test_move_succeeds_and_releases_locks(self, temp_dir: Path) -> None:
        src = _library_path(temp_dir, "Series/a.txt")
        src.write_bytes(b"payload")
        dest = _library_path(temp_dir, "Series/b.txt")

        res = _make_resource(temp_dir, src)
        assert res.handle_move(f"/mokuro-reader/{dest.parent.name}/{dest.name}")
        assert dest.read_bytes() == b"payload"
        assert not src.exists()

        # Both paths must be writable again afterwards.
        for path in (src, dest):
            writer = _make_resource(temp_dir, path).begin_write()
            writer.write(b"x")
            writer.close()
