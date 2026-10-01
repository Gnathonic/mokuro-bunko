"""One library walk serves every claim and the queue page.

On a 12,494-volume library on a network share one walk took 7 s. Eight
machines each walked for every claim, the queue page walked for its list,
and the queue file stat'ed every pending volume again: the settings page and
the queue page stopped answering. The walk is now shared, kept current in
place, and re-taken only when it has aged a few walks' worth.
"""

from __future__ import annotations

import json
import zipfile
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.watcher import OCRWorker


def _rows() -> list[GenerationSpec]:
    return parse_generation_list([
        {"name": "mokuro", "engine": "mokuro", "primary": True},
        {"name": "hayai", "engine": "hayai-nova"},
    ])


def _volume(storage: Path, series: str, volume: str) -> Path:
    path = storage / "library" / series / f"{volume}.cbz"
    path.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(path, "w") as zf:
        zf.writestr("001.jpg", b"x" * 16)
    path.with_suffix(".mokuro").write_text(
        json.dumps({"version": "0", "volume_uuid": f"u-{series}-{volume}", "pages": [], "chars": 0}),
        encoding="utf-8",
    )
    return path


@pytest.fixture
def storage(tmp_path: Path) -> Path:
    for name in ("library", "inbox", "users"):
        (tmp_path / name).mkdir()
    for s in range(3):
        for v in range(4):
            _volume(tmp_path, f"S{s}", f"V{v}")
    return tmp_path


@pytest.fixture
def worker(storage: Path, monkeypatch: pytest.MonkeyPatch) -> Any:
    w = OCRWorker(storage_path=storage, poll_interval=300.0, generations=_rows(), sessions=True)
    walks: list[int] = []
    real = w._walk_candidates

    def counted() -> list[tuple[Path, str]]:
        walks.append(1)
        result = real()
        return result

    monkeypatch.setattr(w, "_walk_candidates", counted)
    w.walks = walks  # type: ignore[attr-defined]
    return w


def _expensive(worker: Any) -> None:
    """Make the last walk look like the live library's: 7 s."""
    worker._candidate_walk_seconds = 7.0


def test_many_claims_share_one_walk(worker: Any) -> None:
    worker._ocr_candidates()
    _expensive(worker)
    for _ in range(20):
        worker._ocr_candidates()
        worker.pending_jobs(max_age=0)
    assert len(worker.walks) == 1


def test_a_cheap_walk_is_not_cached(worker: Any, storage: Path) -> None:
    worker._ocr_candidates()
    worker._candidate_walk_seconds = 0.0  # a walk that costs nothing is kept for nothing
    _volume(storage, "S9", "V0")  # copied straight onto the disk
    jobs = worker._ocr_candidates()
    assert any(job[0].parent.name == "S9" for job in jobs)


def test_an_arrival_joins_the_walk_without_a_new_one(worker: Any, storage: Path) -> None:
    worker._ocr_candidates()
    _expensive(worker)
    cbz = _volume(storage, "S9", "V0")
    worker.archive_arrived(cbz)
    assert (cbz, _rows()[1].id) in worker._ocr_candidates()
    assert len(worker.walks) == 1


def test_a_removal_leaves_the_walk(worker: Any, storage: Path) -> None:
    worker._ocr_candidates()
    _expensive(worker)
    worker.archive_removed(storage / "library" / "S1")
    assert not any(job[0].parent.name == "S1" for job in worker._ocr_candidates())
    assert len(worker.walks) == 1


def test_a_finished_job_leaves_the_walk(worker: Any) -> None:
    jobs = worker._ocr_candidates()
    _expensive(worker)
    done = jobs[0]
    worker._forget_candidate(done)
    assert done not in worker._ocr_candidates()


def test_a_settings_change_walks_again(worker: Any) -> None:
    worker._ocr_candidates()
    _expensive(worker)
    worker.apply_settings(_rows())
    worker._ocr_candidates()
    assert len(worker.walks) == 2


def test_a_claim_skips_a_job_whose_sidecar_appeared_since(worker: Any) -> None:
    jobs = worker._ocr_candidates()
    _expensive(worker)
    row = worker._generation(jobs[0][1])
    first = jobs[0][0]
    plain, _ = row.sidecar_paths(first)
    plain.write_text("{}", encoding="utf-8")  # a reader uploaded this layer
    job, _ = worker.claim_for_session(worker._slots[0], row.id)
    assert job is not None and job[0] != first
    assert (first, row.id) not in worker._ocr_candidates()


def test_the_queue_file_reads_every_volume_from_the_walk(worker: Any) -> None:
    worker._ocr_candidates()
    _expensive(worker)
    owed = worker.owed_by_volume()
    assert len(owed) == 12
    assert all([r.name for r in rows] == ["hayai"] for rows in owed.values())
    assert len(worker.walks) == 1
