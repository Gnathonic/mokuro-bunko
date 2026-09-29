"""A slow start is not paid for one volume a warm machine finishes sooner (F7).

Desktop mokuro-fp16 sessions carried 1.2 volumes each: startup was 38.7 % of
their time, 23 s a volume. When a row has ONE volume left that this machine
could take, and another machine already has a session of that row open, the
volume is left to the warm session if it will finish it -- its own work in
flight included -- before this machine could even start a runner and read
it.
"""

from __future__ import annotations

import json
import sys
import zipfile
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.devices import DeviceCatalog
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.watcher import OCRWorker

FULL: dict[str, Any] = {"engines": ["mokuro", "hayai-nova"], "detectors": ["ctd"],
                        "devices": [], "serves_mokuro": True}


def _library(storage: Path, *volumes: str) -> list[Path]:
    out = []
    for volume in volumes:
        cbz = storage / "library" / "Alpha" / f"{volume}.cbz"
        cbz.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(cbz, "w") as zf:
            zf.writestr("page_000.jpg", b"fake image data")
        cbz.with_suffix(".mokuro").write_text(
            json.dumps({"version": "0.0", "volume_uuid": f"u-{volume}", "pages": [],
                        "chars": 0}), encoding="utf-8")
        out.append(cbz)
    return out


def _rig(tmp_path: Path, volumes: int) -> tuple[OCRWorker, Any, Any, Any]:
    (tmp_path / "inbox").mkdir(exist_ok=True)
    _library(tmp_path, *[f"Volume {n}" for n in range(1, volumes + 1)])
    registry = ProcessorRegistry()
    entries = []
    for name in ("tower", "desktop"):
        entry = registry.register(username=name, name=name, host={}, catalog=FULL,
                                  max_sessions=1)
        entry.stream_open = True
        entries.append(entry)
    worker = OCRWorker(
        storage_path=tmp_path, poll_interval=30.0,
        generations=parse_generation_list(
            [{"name": "mokuro", "engine": "mokuro", "primary": True},
             {"name": "hayai", "engine": "hayai-nova", "detector": "ctd"}],
            devices=DeviceCatalog(),
        ),
        engines_python_path=Path(sys.executable), concurrency=1, sessions=True,
        remote=registry, local_processing=False, autobench=False,
        page_count_lookup=lambda path: 100,
    )
    row = worker.generations[1]
    tower_key = worker._rate_key(row.id, "tower")
    desk_key = worker._rate_key(row.id, "desktop")
    for _ in range(3):
        worker.rates.record_volume(tower_key, 100, 10.0)  # ~10 pages/s
        worker.rates.record_volume(desk_key, 100, 25.0)   # ~4 pages/s
    worker.rates.record_startup(desk_key, 16.0)
    return worker, row, entries[0], entries[1]


def _warm(worker: OCRWorker, row: Any, tower: Any, *, eta: float) -> None:
    """tower has a session of the row open, one volume of it in flight."""
    session = type("Warm", (), {
        "generation": row, "entry": tower, "closing": False, "killed": False,
        "is_alive": lambda self: True,
    })()
    worker._open_sessions.add(session)
    slot = worker._make_remote_slot(0, tower)
    job = worker.claim_next(slot)
    assert job is not None
    worker._active_progress[job] = {"eta_seconds": eta, "done_pages": 50, "total_pages": 100}


class TestAWarmSessionFirst:
    def test_one_volume_left_goes_to_the_warm_machine(self, tmp_path: Path) -> None:
        worker, row, tower, desktop = _rig(tmp_path, 2)
        _warm(worker, row, tower, eta=5.0)
        # desktop: 16 s to start + 25 s to read; tower: 5 s left + 10 s.
        assert worker.claim_next(worker._make_remote_slot(1, desktop)) is None
        pending = {j["volume"] for j in worker.pending_jobs(max_age=0)}
        assert "Volume 2" in pending, "left pending for the warm session's top-up"

    def test_with_nobody_warm_it_starts_as_before(self, tmp_path: Path) -> None:
        worker, row, _tower, desktop = _rig(tmp_path, 1)
        assert worker.claim_next(worker._make_remote_slot(1, desktop)) is not None

    def test_a_warm_machine_that_is_slower_does_not_keep_it(self, tmp_path: Path) -> None:
        worker, row, tower, desktop = _rig(tmp_path, 2)
        _warm(worker, row, tower, eta=120.0)
        assert worker.claim_next(worker._make_remote_slot(1, desktop)) is not None

    def test_more_than_one_volume_left_is_worth_a_session(self, tmp_path: Path) -> None:
        worker, row, tower, desktop = _rig(tmp_path, 3)
        _warm(worker, row, tower, eta=5.0)
        assert worker.claim_next(worker._make_remote_slot(1, desktop)) is not None

    def test_a_session_s_own_top_up_is_never_held_back(self, tmp_path: Path) -> None:
        worker, row, tower, desktop = _rig(tmp_path, 2)
        _warm(worker, row, tower, eta=5.0)
        slot = worker._make_remote_slot(1, desktop)
        assert worker.claim_for_session(slot, row.id)[0] is not None

    def test_an_unknown_length_is_no_evidence(self, tmp_path: Path) -> None:
        worker, row, tower, desktop = _rig(tmp_path, 2)
        _warm(worker, row, tower, eta=5.0)
        worker.page_count_lookup = lambda path: None  # type: ignore[assignment]
        assert worker.claim_next(worker._make_remote_slot(1, desktop)) is not None
