"""The processor's archive spool, its verification and (Task 2) its fetcher.

The processor downloads each archive WHOLE, at full speed, before the runner
sees a byte of it (design sections 4.1-4.5): into an unnamed file in RAM
(`/dev/shm`, `O_TMPFILE`) within a budget, else onto its own storage; then
checks every member's CRC-32 against the zip's own central directory. These
tests pin the spool's placement and accounting rules and what verification
does and does not decide. Archive content is synthetic throughout.
"""

from __future__ import annotations

import errno
import io
import os
import stat
import subprocess
import sys
import threading
import time
import warnings
import zipfile
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.processor import archives
from mokuro_bunko.processor.archives import (
    ArchiveSpool,
    FetchCancelled,
    TransferFault,
    verify_archive,
)

MiB = 1 << 20


# --- archives ----------------------------------------------------------------


def _zip_bytes(
    members: list[tuple[str, bytes]], *, compression: int = zipfile.ZIP_STORED
) -> bytes:
    blob = io.BytesIO()
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")  # duplicate names are the point of one test
        with zipfile.ZipFile(blob, "w", compression) as zf:
            for name, data in members:
                zf.writestr(name, data)
    return blob.getvalue()


def _pages(count: int, size: int = 4096) -> list[tuple[str, bytes]]:
    return [(f"{n:03d}.jpg", os.urandom(size)) for n in range(count)]


def _flip(raw: bytes, at: int) -> bytes:
    out = bytearray(raw)
    out[at] ^= 0xFF
    return bytes(out)


def _spool(tmp_path: Path, **kwargs: Any) -> ArchiveSpool:
    """A spool whose RAM is a directory under `tmp_path`, and whose host
    has plenty of memory unless a test says otherwise."""
    shm = tmp_path / "shm"
    shm.mkdir(exist_ok=True)
    kwargs.setdefault("memory_dir", shm)
    kwargs.setdefault("memory_mb", 64)
    kwargs.setdefault("headroom", lambda: None)
    return ArchiveSpool(tmp_path / "storage", **kwargs)


def _fill(placement: Any, data: bytes) -> None:
    placement.write(data)


# --- the spool -----------------------------------------------------------------


class TestTheSpool:
    def test_a_memory_placement_is_unnamed_and_opens_in_a_child_interpreter(
        self, tmp_path: Path
    ) -> None:
        spool = _spool(tmp_path)
        data = _zip_bytes(_pages(5))
        placement = spool.place(len(data))
        try:
            _fill(placement, data)
            assert placement.kind == "memory"
            assert list((tmp_path / "shm").iterdir()) == [], "a RAM archive has no name"
            assert placement.runner_path.startswith(f"/proc/{os.getpid()}/fd/")
            probe = subprocess.run(
                [sys.executable, "-c",
                 "import sys, zipfile; zf = zipfile.ZipFile(sys.argv[1]); "
                 "print(zf.testzip(), len(zf.namelist()))",
                 placement.runner_path],
                capture_output=True, text=True, timeout=60, check=False,
            )
            assert probe.returncode == 0, probe.stderr
            assert probe.stdout.split() == ["None", "5"]
            assert spool.in_memory_bytes == len(data)
        finally:
            placement.release()
        assert spool.in_memory_bytes == 0

    def test_a_second_archive_over_the_budget_goes_to_disk(self, tmp_path: Path) -> None:
        spool = _spool(tmp_path, memory_mb=3)
        first = spool.place(2 * MiB)
        second = spool.place(2 * MiB)
        try:
            assert first.kind == "memory"
            assert second.kind == "disk"
            assert spool.in_memory_bytes == 2 * MiB
        finally:
            first.release()
            second.release()

    def test_an_archive_larger_than_the_whole_budget_goes_to_disk(
        self, tmp_path: Path
    ) -> None:
        spool = _spool(tmp_path, memory_mb=1)
        placement = spool.place(3 * MiB)
        try:
            assert placement.kind == "disk"
            assert spool.in_memory_bytes == 0
        finally:
            placement.release()

    def test_a_budget_of_zero_means_disk(self, tmp_path: Path) -> None:
        spool = _spool(tmp_path, memory_mb=0)
        placement = spool.place(1024)
        try:
            assert placement.kind == "disk"
        finally:
            placement.release()

    def test_a_memory_dir_without_unnamed_files_falls_back_to_disk(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        real = archives._open_unnamed
        shm = tmp_path / "shm"

        def refusing(directory: Path) -> int | None:
            return None if Path(directory) == shm else real(directory)

        monkeypatch.setattr(archives, "_open_unnamed", refusing)
        spool = _spool(tmp_path)
        placement = spool.place(1024)
        try:
            assert placement.kind == "disk"
        finally:
            placement.release()

    def test_no_unnamed_files_on_disk_either_gives_a_named_private_file(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(archives, "_open_unnamed", lambda directory: None)
        spool = _spool(tmp_path)
        data = _zip_bytes(_pages(2))
        placement = spool.place(len(data))
        path = Path(placement.runner_path)
        try:
            _fill(placement, data)
            assert placement.kind == "disk"
            assert path.parent == tmp_path / "storage" / ".processing" / "archives"
            assert path.suffix == ".cbz" and len(path.stem) == 32
            assert stat.S_IMODE(path.stat().st_mode) == 0o600
            assert path.read_bytes() == data
        finally:
            placement.release()
        assert not path.exists(), "a released named file is deleted"

    def test_tmpfs_space_too_small_falls_back_to_disk(self, tmp_path: Path) -> None:
        shm = tmp_path / "shm"
        spool = _spool(
            tmp_path,
            free_bytes=lambda d: 10 * MiB if Path(d) == shm else 1 << 40,
        )
        placement = spool.place(8 * MiB)  # 8 + 64 MiB margin > 10
        try:
            assert placement.kind == "disk"
        finally:
            placement.release()

    def test_unwritten_reservations_are_subtracted_from_tmpfs_free_space(
        self, tmp_path: Path
    ) -> None:
        """Two sessions placing at once cannot both pass on the same space:
        a reservation not yet written is not in `free` yet."""
        shm = tmp_path / "shm"
        spool = _spool(
            tmp_path,
            memory_mb=1024,
            free_bytes=lambda d: 100 * MiB if Path(d) == shm else 1 << 40,
        )
        first = spool.place(30 * MiB)  # 30 + 64 <= 100
        second = spool.place(30 * MiB)  # 100 - 30 unwritten = 70 < 94
        try:
            assert first.kind == "memory"
            assert second.kind == "disk"
        finally:
            first.release()
            second.release()

    def test_too_little_available_memory_falls_back_to_disk(
        self, tmp_path: Path
    ) -> None:
        meminfo = tmp_path / "meminfo"
        meminfo.write_text(
            "MemTotal:       65536000 kB\nMemAvailable:     1048576 kB\n", encoding="utf-8"
        )
        spool = _spool(
            tmp_path,
            headroom=lambda: archives.memory_headroom(
                meminfo=meminfo, proc_cgroup=tmp_path / "none", cgroup_root=tmp_path / "none"
            ),
        )
        placement = spool.place(1 * MiB)  # 1 MiB + 1 GiB margin > 1 GiB available
        try:
            assert placement.kind == "disk"
        finally:
            placement.release()

    def test_a_cgroup_limit_is_the_real_limit(self, tmp_path: Path) -> None:
        """Without swap, tmpfs pages are charged to the writer's memcg: a
        `MemoryMax=` below the machine's RAM is what an archive competes
        with, whatever `MemAvailable` says."""
        meminfo = tmp_path / "meminfo"
        meminfo.write_text("MemAvailable:   65536000 kB\n", encoding="utf-8")
        proc_cgroup = tmp_path / "cgroup"
        proc_cgroup.write_text("0::/system.slice/processor.service\n", encoding="utf-8")
        root = tmp_path / "sys-fs-cgroup"
        leaf = root / "system.slice" / "processor.service"
        leaf.mkdir(parents=True)
        (root / "system.slice" / "memory.max").write_text("max\n", encoding="utf-8")
        (root / "system.slice" / "memory.current").write_text("1\n", encoding="utf-8")
        (leaf / "memory.max").write_text(str(2 << 30) + "\n", encoding="utf-8")
        (leaf / "memory.current").write_text(str((2 << 30) - (512 << 20)) + "\n",
                                             encoding="utf-8")

        def headroom() -> int | None:
            return archives.memory_headroom(
                meminfo=meminfo, proc_cgroup=proc_cgroup, cgroup_root=root
            )

        assert headroom() == 512 << 20
        spool = _spool(tmp_path, headroom=headroom)
        placement = spool.place(1 * MiB)
        try:
            assert placement.kind == "disk"
        finally:
            placement.release()

    def test_a_cgroup_without_a_limit_counts_as_unlimited(self, tmp_path: Path) -> None:
        meminfo = tmp_path / "meminfo"
        meminfo.write_text("MemAvailable:   65536000 kB\n", encoding="utf-8")
        proc_cgroup = tmp_path / "cgroup"
        proc_cgroup.write_text("0::/user.slice\n", encoding="utf-8")
        root = tmp_path / "sys-fs-cgroup"
        (root / "user.slice").mkdir(parents=True)
        (root / "user.slice" / "memory.max").write_text("max\n", encoding="utf-8")
        (root / "user.slice" / "memory.current").write_text("123\n", encoding="utf-8")
        assert archives.memory_headroom(
            meminfo=meminfo, proc_cgroup=proc_cgroup, cgroup_root=root
        ) == 65536000 * 1024

    def test_storage_too_full_for_the_fallback_is_no_room(self, tmp_path: Path) -> None:
        spool = _spool(tmp_path, memory_mb=0, free_bytes=lambda d: 100 * MiB)
        with pytest.raises(TransferFault) as excinfo:
            spool.place(1 * MiB)  # 1 + 256 MiB margin > 100
        assert excinfo.value.kind == "no_room"

    def test_release_returns_exactly_its_own_reservation_in_either_order(
        self, tmp_path: Path
    ) -> None:
        spool = _spool(tmp_path, memory_mb=64)
        a = spool.place(3 * MiB)
        b = spool.place(5 * MiB)
        assert spool.in_memory_bytes == 8 * MiB
        b.release()
        assert spool.in_memory_bytes == 3 * MiB
        b.release()  # idempotent
        assert spool.in_memory_bytes == 3 * MiB
        a.release()
        assert spool.in_memory_bytes == 0
        c = spool.place(2 * MiB)
        d = spool.place(1 * MiB)
        c.release()
        assert spool.in_memory_bytes == 1 * MiB
        d.release()
        assert spool.in_memory_bytes == 0

    def test_release_closes_its_own_file_only(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(archives, "_open_unnamed", lambda directory: None)
        spool = _spool(tmp_path, memory_mb=0)
        a = spool.place(10)
        b = spool.place(10)
        _fill(a, b"a" * 10)
        _fill(b, b"b" * 10)
        a.release()
        assert not Path(a.runner_path).exists()
        assert Path(b.runner_path).read_bytes() == b"b" * 10
        b.release()

    def test_close_frees_everything(self, tmp_path: Path) -> None:
        spool = _spool(tmp_path, memory_mb=4)
        held = [spool.place(1 * MiB) for _ in range(3)]
        assert any(p.kind == "memory" for p in held)
        spool.close()
        assert spool.in_memory_bytes == 0
        assert all(p.released for p in held)

    def test_sweep_empties_only_the_named_fallback_directory(self, tmp_path: Path) -> None:
        storage = tmp_path / "storage"
        named = storage / ".processing" / "archives"
        named.mkdir(parents=True)
        (named / "left-over.cbz").write_bytes(b"x")
        workspace = storage / ".processing" / "Volume 1_abc"
        workspace.mkdir()
        (workspace / "keep.txt").write_text("mine", encoding="utf-8")
        runner = storage / ".processing" / "runner-0123456789abcdef"
        runner.mkdir()
        spool = _spool(tmp_path)
        spool.sweep_named()
        assert list(named.iterdir()) == []
        assert (workspace / "keep.txt").exists()
        assert runner.exists()

    def test_a_full_memory_write_says_so(self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
                                         ) -> None:
        spool = _spool(tmp_path)
        placement = spool.place(2 * MiB)
        real_write = os.write

        def full(fd: int, data: Any) -> int:
            if fd == placement.fd:
                raise OSError(errno.ENOSPC, "No space left on device")
            return real_write(fd, data)

        monkeypatch.setattr(archives.os, "write", full)
        try:
            with pytest.raises(archives.SpoolFull):
                placement.write(b"x" * 100)
        finally:
            monkeypatch.undo()
            placement.release()


# --- verification --------------------------------------------------------------


def _write(tmp_path: Path, raw: bytes, name: str = "v.cbz") -> Path:
    path = tmp_path / name
    path.write_bytes(raw)
    return path


class _CancelAfter:
    """An Event whose `is_set` turns true after ``n`` looks."""

    def __init__(self, n: int) -> None:
        self.n = n
        self.calls = 0

    def is_set(self) -> bool:
        self.calls += 1
        return self.calls > self.n


class TestVerification:
    def test_a_clean_archive_verifies_stored_or_deflated(self, tmp_path: Path) -> None:
        for compression in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
            path = _write(tmp_path, _zip_bytes(_pages(6), compression=compression))
            verified = verify_archive(path)
            assert verified.ok, verified
            assert verified.members == 6
            assert verified.damaged == []

    def test_a_crc_flip_in_a_member_names_it(self, tmp_path: Path) -> None:
        pages = _pages(4)
        raw = _zip_bytes(pages)
        at = raw.find(pages[2][1]) + 100
        verified = verify_archive(_write(tmp_path, _flip(raw, at)))
        assert not verified.ok
        assert verified.structural is None
        assert verified.damaged == ["002.jpg"]

    def test_a_truncated_file_is_a_structural_failure(self, tmp_path: Path) -> None:
        raw = _zip_bytes(_pages(4))
        verified = verify_archive(_write(tmp_path, raw[: len(raw) // 2]))
        assert not verified.ok
        assert verified.structural

    def test_an_unsupported_method_is_not_an_integrity_failure(
        self, tmp_path: Path
    ) -> None:
        """The runner meets it in its own place, exactly as locally."""
        raw = bytearray(_zip_bytes([("000.jpg", b"fake image data" * 10)]))
        # Method 98 (PPMd) in the local header and the central directory.
        for signature, offset in ((b"PK\x03\x04", 8), (b"PK\x01\x02", 10)):
            at = raw.find(signature) + offset
            raw[at : at + 2] = (98).to_bytes(2, "little")
        verified = verify_archive(_write(tmp_path, bytes(raw)))
        assert verified.ok, verified
        assert verified.skipped == ["000.jpg"]

    def test_directory_entries_are_ignored(self, tmp_path: Path) -> None:
        blob = io.BytesIO()
        with zipfile.ZipFile(blob, "w") as zf:
            # A directory entry; `ZipFile.mkdir` is 3.11+, and 3.10 is supported.
            zf.writestr(zipfile.ZipInfo("sub/"), b"")
            zf.writestr("sub/000.jpg", b"fake image data")
        verified = verify_archive(_write(tmp_path, blob.getvalue()))
        assert verified.ok
        assert verified.members == 1

    def test_only_the_entry_a_reader_resolves_to_is_checked(self, tmp_path: Path) -> None:
        """For a duplicated name, `zf.read(name)` -- and so the runner --
        reads the LAST entry. The shadowed one is never read by anyone."""
        early, late = os.urandom(3000), os.urandom(3000)
        raw = _zip_bytes([("000.jpg", early), ("001.jpg", b"x" * 100), ("000.jpg", late)])
        damaged_early = _flip(raw, raw.find(early) + 10)
        verified = verify_archive(_write(tmp_path, damaged_early, "early.cbz"))
        assert verified.ok, verified
        assert verified.shadowed == 1
        damaged_late = _flip(raw, raw.find(late) + 10)
        verified = verify_archive(_write(tmp_path, damaged_late, "late.cbz"))
        assert verified.damaged == ["000.jpg"]

    def test_cancel_mid_member_stops_within_one_read(self, tmp_path: Path) -> None:
        path = _write(tmp_path, _zip_bytes([("000.jpg", os.urandom(8 * MiB))]))
        cancel = _CancelAfter(1)
        with pytest.raises(FetchCancelled):
            verify_archive(path, cancel=cancel)  # type: ignore[arg-type]
        assert cancel.calls == 2, "checked before the member and after its first read"

    def test_a_real_event_is_accepted(self, tmp_path: Path) -> None:
        path = _write(tmp_path, _zip_bytes(_pages(2)))
        stop = threading.Event()
        assert verify_archive(path, cancel=stop).ok
        stop.set()
        with pytest.raises(FetchCancelled):
            verify_archive(path, cancel=stop)


# --- the fetcher -----------------------------------------------------------------

from mokuro_bunko.processor.archives import ArchiveFetcher, FetchTiming  # noqa: E402
from mokuro_bunko.processor.client import (  # noqa: E402
    EventSink,
    LibraryClient,
    LibraryTransportError,
)
from mokuro_bunko.processor.config import (  # noqa: E402
    LibrarySettings,
    ProcessorConfig,
    ProcessorOcr,
    ProcessorSettings,
)
from tests.fixtures.archive_server import ARCHIVE_PATH, ArchiveServer  # noqa: E402

FAST = FetchTiming(
    connect_timeout=5.0,
    read_timeout=0.4,
    retry_delays=(0.01, 0.02, 0.04),
    stall_seconds=1.5,
    max_restarts=3,
    progress_after=0.0,
)


def _library_client(url: str, tmp_path: Path, tls_verify: bool | str = True) -> LibraryClient:
    return LibraryClient(
        ProcessorConfig(
            library=LibrarySettings(
                url=url, username="tower", password="hunter2hunter2", tls_verify=tls_verify
            ),
            processor=ProcessorSettings(name="tower", storage=tmp_path / "state"),
            ocr=ProcessorOcr(),
        )
    )


class _Rig:
    def __init__(self, tmp_path: Path, content: bytes, *, timing: FetchTiming = FAST,
                 tls: tuple[Path, Path] | None = None, **spool: Any) -> None:
        self.content = content
        self.server = ArchiveServer(content, tls=tls)
        verify: bool | str = str(tls[0]) if tls is not None else True
        self.client = _library_client(self.server.url, tmp_path, verify)
        self.spool = _spool(tmp_path, **spool)
        self.fetcher = ArchiveFetcher(self.client, self.spool, timing=timing)
        self.cancel = threading.Event()
        self.offers: list[dict[str, Any]] = []

    def fetch(self, *, size: int | None | str = "same", path: str = ARCHIVE_PATH) -> Any:
        return self.fetcher.fetch(
            path,
            size=len(self.content) if size == "same" else size,  # type: ignore[arg-type]
            cancel=self.cancel,
            progress=self.offers.append,
        )

    def close(self) -> None:
        self.server.close()
        self.spool.close()


@pytest.fixture
def rig(tmp_path: Path) -> Any:
    made: list[_Rig] = []

    def make(content: bytes | None = None, **kwargs: Any) -> _Rig:
        made.append(_Rig(tmp_path, content if content is not None else _archive(), **kwargs))
        return made[-1]

    yield make
    for each in made:
        each.close()


def _archive(pages: int = 12, size: int = 256 * 1024) -> bytes:
    """A stored zip of random 'pages': larger than one read, easy to cut."""
    return _zip_bytes(_pages(pages, size))


def _held(fetched: Any) -> bytes:
    return Path(fetched.placement.read_path).read_bytes()


class TestTheFetcher:
    def test_a_clean_download_is_one_request_into_memory(self, rig: Any) -> None:
        r = rig()
        fetched = r.fetch()
        try:
            assert _held(fetched) == r.content
            assert fetched.crc32 == zlib_crc(r.content)
            assert fetched.placement.kind == "memory"
            assert fetched.requests == 1
            assert fetched.restarts == 0 and fetched.repairs == 0
            assert fetched.verdict is None and fetched.damaged == []
            summary = fetched.summary()
            assert summary["placement"] == "memory" and summary["requests"] == 1
            assert summary["crc32"] == f"{zlib_crc(r.content):08x}"
        finally:
            fetched.release()
        assert r.spool.in_memory_bytes == 0

    def test_a_connection_closed_mid_body_resumes_where_it_stopped(self, rig: Any) -> None:
        """Defect B's regression: `read()` returning b"" early is a cut
        connection, never an archive that ended early."""
        r = rig()
        r.server.push(truncate_after=700_000)
        fetched = r.fetch()
        try:
            assert _held(fetched) == r.content
            assert fetched.requests == 2
            assert fetched.verdict is None and fetched.damaged == [] and fetched.repairs == 0
            second = r.server.gets()[1]
            assert second["range"] == "bytes=700000-"
            assert second["if_range"] == ArchiveServer.etag_of(r.content)
        finally:
            fetched.release()

    def test_a_reset_mid_body_resumes(self, rig: Any) -> None:
        r = rig()
        r.server.push(reset_after=1_000_000)
        fetched = r.fetch()
        try:
            # An RST throws away whatever was still in flight, so how much
            # arrived before it is the kernel's business; the copy is whole.
            assert _held(fetched) == r.content
            assert fetched.requests >= 2
            assert fetched.verdict is None and fetched.repairs == 0
        finally:
            fetched.release()

    def test_a_stall_past_the_read_timeout_resumes(self, rig: Any) -> None:
        r = rig()
        r.server.push(stall_after=500_000, stall_seconds=3.0)
        started = time.monotonic()
        fetched = r.fetch()
        try:
            assert _held(fetched) == r.content
            assert fetched.requests == 2
            assert time.monotonic() - started < 2.5
        finally:
            fetched.release()

    def test_a_file_replaced_between_attempts_is_downloaded_again_whole(
        self, rig: Any
    ) -> None:
        r = rig()
        new = _archive()
        assert len(new) == len(r.content)
        r.server.push(truncate_after=600_000)
        r.server.push(replace_with=new)
        fetched = r.fetch()
        try:
            assert _held(fetched) == new, "no byte of the old file survives"
            assert fetched.restarts == 1
            assert r.server.gets()[1]["if_range"] == ArchiveServer.etag_of(r.content)
        finally:
            fetched.release()

    def test_a_412_to_a_resume_restarts_with_a_plain_get(self, rig: Any) -> None:
        r = rig()
        r.server.push(truncate_after=600_000)
        r.server.push(status=412)
        fetched = r.fetch()
        try:
            assert _held(fetched) == r.content
            assert r.server.gets()[2]["range"] is None
            assert fetched.restarts == 1
        finally:
            fetched.release()

    def test_a_206_with_a_different_etag_is_caught_and_restarted(self, rig: Any) -> None:
        """A server that ignores If-Range but names the new file's ETag."""
        r = rig()
        new = _archive()
        r.server.push(truncate_after=600_000)
        r.server.push(replace_with=new, ignore_if_range=True)
        fetched = r.fetch()
        try:
            assert _held(fetched) == new
            assert fetched.restarts == 1
            assert r.server.gets()[2]["range"] is None
        finally:
            fetched.release()

    def test_a_real_splice_is_caught_by_verification_and_repaired(self, rig: Any) -> None:
        """No ETag at all, and a file replaced between attempts: the resume
        splices two files. Verification catches it; a second full copy is
        clean."""
        r = rig()
        new = _archive()
        r.server.default = {"drop_etag": True}
        r.server.push(truncate_after=600_000, drop_etag=True)
        r.server.push(replace_with=new, drop_etag=True)
        fetched = r.fetch()
        try:
            assert _held(fetched) == new
            assert fetched.repairs == 1
            assert fetched.verdict is None
        finally:
            fetched.release()
        assert r.spool.in_memory_bytes == 0

    def test_damage_at_the_library_is_delivered_as_it_is_with_a_verdict(
        self, rig: Any
    ) -> None:
        pages = _pages(6, 100_000)
        r = rig(_zip_bytes(pages))
        r.server.flip_at = r.content.find(pages[3][1]) + 50
        fetched = r.fetch()
        try:
            assert fetched.damaged == ["003.jpg"]
            assert fetched.verdict == "damaged at the library"
            assert len(r.server.gets()) == 2, "exactly two full GETs"
            assert all(g["range"] is None for g in r.server.gets())
            assert _held(fetched) == r.server.served(), "the first copy, as it is"
            assert fetched.summary()["verdict"] == "damaged at the library"
        finally:
            fetched.release()
        assert r.spool.in_memory_bytes == 0

    def test_a_flip_in_transit_is_repaired(self, rig: Any) -> None:
        r = rig()
        r.server.push(corrupt_at=len(r.content) // 2)
        fetched = r.fetch()
        try:
            assert _held(fetched) == r.content
            assert fetched.repairs == 1 and fetched.verdict is None
        finally:
            fetched.release()

    def test_structural_damage_at_the_source_is_delivered_with_the_verdict(
        self, rig: Any
    ) -> None:
        raw = bytearray(_archive(4))
        eocd = raw.rfind(b"PK\x05\x06")
        raw[eocd : eocd + 4] = b"XXXX"
        r = rig(bytes(raw))
        fetched = r.fetch()
        try:
            assert fetched.verdict == "damaged at the library"
            assert fetched.structural
            assert len(r.server.gets()) == 2
        finally:
            fetched.release()

    @pytest.mark.parametrize("status", [404, 410])
    def test_not_there_is_missing_after_one_request(self, rig: Any, status: int) -> None:
        r = rig()
        r.server.push(status=status)
        with pytest.raises(TransferFault) as excinfo:
            r.fetch()
        assert excinfo.value.kind == "missing"
        assert len(r.server.gets()) == 1
        assert r.spool.in_memory_bytes == 0

    @pytest.mark.parametrize("status", [401, 403, 407])
    def test_the_account_s_refusal_is_the_transport_s(self, rig: Any, status: int) -> None:
        r = rig()
        r.server.push(status=status)
        with pytest.raises(LibraryTransportError):
            r.fetch()
        assert len(r.server.gets()) == 1

    def test_a_503_honours_retry_after(self, rig: Any) -> None:
        r = rig()
        r.server.push(status=503, retry_after=1)
        started = time.monotonic()
        fetched = r.fetch()
        try:
            elapsed = time.monotonic() - started
            assert _held(fetched) == r.content
            assert 0.9 <= elapsed < 3.0, elapsed
            assert fetched.requests == 2
        finally:
            fetched.release()

    def test_two_500s_in_a_row_are_given_back_as_stalled(self, rig: Any) -> None:
        r = rig()
        r.server.default = {"status": 500}
        with pytest.raises(TransferFault) as excinfo:
            r.fetch()
        assert excinfo.value.kind == "stalled"
        assert excinfo.value.status == 500
        assert len(r.server.gets()) == 2

    def test_a_download_that_never_moves_is_given_back_after_the_stall_budget(
        self, rig: Any
    ) -> None:
        timing = FetchTiming(
            connect_timeout=5.0, read_timeout=0.4, retry_delays=(0.05, 0.1, 0.2),
            stall_seconds=1.0, max_restarts=3, progress_after=0.0,
        )
        r = rig(timing=timing)
        r.server.default = {"reset_after": 0}
        started = time.monotonic()
        with pytest.raises(TransferFault) as excinfo:
            r.fetch()
        elapsed = time.monotonic() - started
        assert excinfo.value.kind == "stalled"
        assert 1.0 <= elapsed < 2.5, elapsed
        stamps = [g["at"] for g in r.server.gets()]
        gaps = [b - a for a, b in zip(stamps, stamps[1:], strict=False)]
        assert len(gaps) >= 3
        assert gaps[0] < gaps[2], f"the retries back off: {gaps}"
        assert any(o.get("state") == "retrying" for o in r.offers)

    def test_a_file_that_keeps_changing_is_given_back_as_changed(self, rig: Any) -> None:
        r = rig()
        versions = [_archive(12, 256 * 1024) for _ in range(8)]
        r.server.default = lambda n: {
            "replace_with": versions[n % len(versions)], "truncate_after": 400_000,
        }
        with pytest.raises(TransferFault) as excinfo:
            r.fetch()
        assert excinfo.value.kind == "changed"

    def test_a_proxy_that_ignores_range_is_given_back_as_no_range(self, rig: Any) -> None:
        r = rig()
        r.server.default = {"ignore_range": True, "truncate_after": 400_000}
        with pytest.raises(TransferFault) as excinfo:
            r.fetch()
        assert excinfo.value.kind == "no_range"

    def test_a_chunked_body_is_read_to_its_end(self, rig: Any) -> None:
        r = rig()
        r.server.push(no_length=True)
        fetched = r.fetch()
        try:
            assert _held(fetched) == r.content
            assert fetched.requests == 1
        finally:
            fetched.release()

    def test_a_cut_chunked_body_resumes_with_a_bare_range(self, rig: Any) -> None:
        r = rig()
        r.server.push(no_length=True, drop_etag=True, truncate_after=500_000)
        r.server.push(drop_etag=True)
        fetched = r.fetch()
        try:
            assert _held(fetched) == r.content
            second = r.server.gets()[1]
            assert second["range"] is not None and second["range"].startswith("bytes=")
            assert second["if_range"] is None
        finally:
            fetched.release()

    def test_an_offload_misconfiguration_is_a_mismatch(self, rig: Any) -> None:
        r = rig()
        r.server.push(x_accel=True)
        with pytest.raises(TransferFault) as excinfo:
            r.fetch()
        assert excinfo.value.kind == "mismatch"
        assert len(r.server.gets()) == 1

    def test_a_length_other_than_the_op_s_size_is_a_mismatch(self, rig: Any) -> None:
        r = rig()
        with pytest.raises(TransferFault) as excinfo:
            r.fetch(size=len(r.content) + 1)
        assert excinfo.value.kind == "mismatch"
        assert len(r.server.gets()) == 1

    def test_an_archive_over_the_budget_is_held_on_disk(
        self, rig: Any, tmp_path: Path
    ) -> None:
        r = rig(_zip_bytes(_pages(3, 1024 * 1024)), memory_mb=1)
        fetched = r.fetch()
        try:
            assert fetched.placement.kind == "disk"
            assert r.spool.in_memory_bytes == 0
            assert _held(fetched) == r.content
        finally:
            fetched.release()
        assert fetched.placement.released

    def test_a_third_archive_goes_to_disk_until_one_is_released(self, rig: Any) -> None:
        content = _zip_bytes(_pages(3, 1024 * 1024))
        r = rig(content, memory_mb=7)
        first, second = r.fetch(), r.fetch()
        third = r.fetch()
        try:
            assert [first.placement.kind, second.placement.kind] == ["memory", "memory"]
            assert third.placement.kind == "disk"
            first.release()
            fourth = r.fetch()
            assert fourth.placement.kind == "memory"
            fourth.release()
        finally:
            for f in (first, second, third):
                f.release()

    def test_a_full_tmpfs_restarts_the_download_on_disk(
        self, rig: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        r = rig()
        real = archives.Placement.write

        def write(self: Any, data: Any) -> None:
            if self.kind == "memory" and self.written + len(data) > MiB:
                raise archives.SpoolFull(OSError(errno.ENOSPC, "No space left on device"))
            real(self, data)

        monkeypatch.setattr(archives.Placement, "write", write)
        fetched = r.fetch()
        try:
            assert fetched.placement.kind == "disk"
            assert _held(fetched) == r.content
            assert fetched.restarts == 0
        finally:
            fetched.release()
        assert r.spool.in_memory_bytes == 0

    def test_cancel_during_a_stall_ends_it_at_once(self, rig: Any) -> None:
        r = rig(timing=FetchTiming(read_timeout=30.0, stall_seconds=120.0, progress_after=0.0))
        r.server.push(stall_after=300_000, stall_seconds=10.0)
        outcome: dict[str, Any] = {}

        def run() -> None:
            try:
                outcome["result"] = r.fetch()
            except BaseException as e:  # noqa: BLE001 - the outcome is the test
                outcome["error"] = e

        worker = threading.Thread(target=run)
        worker.start()
        deadline = time.monotonic() + 5
        while not r.server.gets() and time.monotonic() < deadline:
            time.sleep(0.01)
        time.sleep(0.3)
        started = time.monotonic()
        r.cancel.set()
        r.fetcher.abort(r.cancel)
        worker.join(timeout=5)
        assert not worker.is_alive()
        assert time.monotonic() - started < 1.0
        assert isinstance(outcome.get("error"), FetchCancelled), outcome
        assert r.spool.in_memory_bytes == 0

    def test_cancel_during_verification_ends_it(
        self, rig: Any, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        r = rig()
        real = archives.verify_archive

        def verify(path: Any, *, cancel: Any = None) -> Any:
            r.cancel.set()
            return real(path, cancel=cancel)

        monkeypatch.setattr(archives, "verify_archive", verify)
        with pytest.raises(FetchCancelled):
            r.fetch()
        assert r.spool.in_memory_bytes == 0

    @pytest.mark.parametrize("memory_mb", [64, 0], ids=["memory", "disk"])
    def test_cancel_while_the_second_copy_is_verified_releases_both(
        self, rig: Any, monkeypatch: pytest.MonkeyPatch, memory_mb: int
    ) -> None:
        """The diagnostic copy is held too: a session that ends while it is
        verified must not keep its reservation, or its fd, until exit."""
        pages = _pages(6, 100_000)
        r = rig(_zip_bytes(pages), memory_mb=memory_mb)
        r.server.flip_at = r.content.find(pages[3][1]) + 50  # damaged at the library
        placed: list[Any] = []
        place = r.spool.place

        def record(*args: Any, **kwargs: Any) -> Any:
            placed.append(place(*args, **kwargs))
            return placed[-1]

        monkeypatch.setattr(r.spool, "place", record)
        real = archives.verify_archive
        calls: list[Any] = []

        def verify(path: Any, *, cancel: Any = None) -> Any:
            calls.append(path)
            if len(calls) == 2:
                r.cancel.set()  # the session ends while copy 2 is verified
            return real(path, cancel=cancel)

        monkeypatch.setattr(archives, "verify_archive", verify)
        with pytest.raises(FetchCancelled):
            r.fetch()
        assert len(calls) == 2 and len(placed) == 2, "copy 2 was the one cancelled"
        assert r.spool.in_memory_bytes == 0
        assert r.spool._live == set(), "every placement was released"
        # Released = its fd closed (and a named file unlinked), exactly once.
        assert [p.released for p in placed] == [True, True]

    @pytest.mark.parametrize("etag", ["weak_etag", "drop_etag"])
    def test_a_weak_or_absent_etag_sends_no_if_range(self, rig: Any, etag: str) -> None:
        r = rig()
        r.server.push(truncate_after=600_000, **{etag: True})
        r.server.push(**{etag: True})
        fetched = r.fetch()
        try:
            assert _held(fetched) == r.content
            assert r.server.gets()[1]["range"] == "bytes=600000-"
            assert r.server.gets()[1]["if_range"] is None
        finally:
            fetched.release()

    def test_storage_full_is_no_room(self, rig: Any) -> None:
        r = rig(memory_mb=0, free_bytes=lambda d: 10 * MiB)
        with pytest.raises(TransferFault) as excinfo:
            r.fetch()
        assert excinfo.value.kind == "no_room"

    def test_a_path_that_is_not_utf8_is_quoted_from_its_bytes(self, rig: Any) -> None:
        r = rig()
        with pytest.raises(TransferFault) as excinfo:
            r.fetch(path="/mokuro-reader/Alpha/Vol\udcff.cbz")
        assert excinfo.value.kind == "missing"
        assert r.server.gets()[0]["path"] == "/mokuro-reader/Alpha/Vol%FF.cbz"

    def test_progress_never_waits_on_the_events_body(self, rig: Any) -> None:
        """A sidecar upload holding the sink's send lock can delay a progress
        FRAME, never the download."""

        class _Connection:
            sock = None

            def send(self, data: bytes) -> None:
                return None

            def close(self) -> None:
                return None

        sink = EventSink(_Connection(), "/x", ping=False)  # type: ignore[arg-type]
        r = rig(_zip_bytes(_pages(20, 1024 * 1024)))
        held = threading.Event()

        def hold() -> None:
            with sink._lock:
                held.set()
                time.sleep(2.0)

        threading.Thread(target=hold, daemon=True).start()
        assert held.wait(5)
        started = time.monotonic()
        fetched = r.fetcher.fetch(
            ARCHIVE_PATH, size=len(r.content), cancel=r.cancel,
            progress=lambda head: sink.offer("fetch:v1", {**head, "id": "v1"}),
        )
        elapsed = time.monotonic() - started
        try:
            assert elapsed < 1.5, elapsed
            assert sink.offered("fetch:v1") is not None
        finally:
            fetched.release()

    def test_over_tls_a_clean_fetch_and_a_resume(self, rig: Any, tmp_path: Path) -> None:
        from mokuro_bunko.ssl import generate_self_signed_cert

        cert, key = tmp_path / "tls" / "cert.pem", tmp_path / "tls" / "key.pem"
        cert.parent.mkdir(parents=True, exist_ok=True)
        generate_self_signed_cert(cert, key)
        r = rig(tls=(cert, key))
        clean = r.fetch()
        try:
            assert _held(clean) == r.content and clean.requests == 1
        finally:
            clean.release()
        r.server.push(truncate_after=700_000)
        resumed = r.fetch()
        try:
            assert _held(resumed) == r.content and resumed.requests == 2
        finally:
            resumed.release()


def zlib_crc(data: bytes) -> int:
    import zlib

    return zlib.crc32(data) & 0xFFFFFFFF


class TestTheEventSinkOffers:
    def test_the_ping_tick_sends_the_latest_offer_in_place_of_a_ping(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.ocr.remote.protocol import read_frame
        from mokuro_bunko.processor import client as client_module

        sent: list[bytes] = []

        class _Connection:
            sock = None

            def send(self, data: bytes) -> None:
                sent.append(data)

            def close(self) -> None:
                return None

        monkeypatch.setattr(client_module, "EVENTS_PING_SECONDS", 0.05)
        sink = EventSink(_Connection(), "/x")  # type: ignore[arg-type]
        try:
            sink.offer("fetch:v1", {"event": "fetch", "id": "v1", "bytes": 1})
            sink.offer("fetch:v1", {"event": "fetch", "id": "v1", "bytes": 2})
            deadline = time.monotonic() + 5
            while len(sent) < 3 and time.monotonic() < deadline:
                time.sleep(0.01)
        finally:
            sink._stop.set()
        heads = []
        for data in sent:
            body = data.split(b"\r\n", 1)[1].rsplit(b"\r\n", 1)[0]
            frame = read_frame(io.BytesIO(body).read)
            assert frame is not None
            heads.append(frame[0])
        assert heads[0] == {"event": "fetch", "id": "v1", "bytes": 2}, "only the latest"
        assert all(head == {"event": "ping"} for head in heads[1:]), heads
        assert sink.offered("fetch:v1") is None

    def test_a_withdrawn_offer_is_never_sent(self) -> None:
        class _Connection:
            sock = None

            def close(self) -> None:
                return None

        sink = EventSink(_Connection(), "/x", ping=False)  # type: ignore[arg-type]
        sink.offer("fetch:v1", {"event": "fetch", "id": "v1"})
        sink.withdraw("fetch:v1")
        assert sink.offered("fetch:v1") is None
        assert sink._take_offers() == []
