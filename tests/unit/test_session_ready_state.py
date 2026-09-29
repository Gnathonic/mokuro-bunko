"""The Loading / Waiting / Running pill comes from the session, not from the ETA.

It used to come from the ETA model's remaining startup estimate: a model load
that ran past its estimate had no `startup_seconds` left, and the card went
from Loading to Waiting while the model was still loading -- the shimmer
stopped, the bar went empty and the text read "waiting to start". A warm
session's head volume read "Waiting" for its first seconds, too, while it was
being read. The watcher knows the real signal (the runner's `ready` event),
so the card carries it: loading = the session is not ready; waiting = ready,
but the runner does not have the volume yet; running = the runner has it.
"""

from __future__ import annotations

import sys
import time
from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from mokuro_bunko.ocr import eta
from mokuro_bunko.ocr.eta import StartupEstimate
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.watcher import OCRWorker, _SessionClock, _SessionJob
from mokuro_bunko.queue.api import QueueAPI
from mokuro_bunko.queue.shape import job_state, shape_status

NOW = 1_000_000.0


def _priced(spent: float, **card: Any) -> dict[str, Any]:
    """A no-page-yet card after the ETA model priced it, ``spent`` s into its load."""
    entry: dict[str, Any] = {
        "generation_id": "g", "done_pages": 0, "total_pages": 100,
        "started_at": NOW - spent, "session_started_at": NOW - spent, **card,
    }
    eta._price_running(
        entry,
        rate_for=lambda *a, **k: None,
        startup_for=lambda *a, **k: StartupEstimate(seconds=20.0, source="prior", rough=True),
        now=NOW,
    )
    return entry


class TestTheState:
    @pytest.mark.parametrize("spent", [5.0, 30.0, 120.0])
    def test_a_load_that_overruns_its_estimate_is_still_loading(self, spent: float) -> None:
        entry = _priced(spent, session_ready=False)
        assert entry["status"] == "starting"
        assert job_state(entry) == "loading", (spent, entry.get("startup_seconds"))

    def test_a_warm_session_s_head_volume_is_being_read(self) -> None:
        entry = _priced(3.0, session_ready=True, delivered=True)
        assert job_state(entry) == "running"

    def test_a_ready_session_still_fetching_the_volume_is_waiting(self) -> None:
        entry = _priced(3.0, session_ready=True, delivered=False)
        assert job_state(entry) == "waiting"

    def test_a_card_from_before_the_flag_keeps_the_old_reading(self) -> None:
        assert job_state({"status": "starting", "startup_seconds": 12}) == "loading"
        assert job_state({"status": "starting", "startup_seconds": None}) == "waiting"

    def test_the_shaped_page_gets_it(self) -> None:
        card = _priced(120.0, session_ready=False, series="S", volume="V", generation="g")
        data = shape_status({"current_jobs": [card], "pending_ocr": []}, "normal", admin=False)
        machine = data["machines"][0]
        assert machine["state"] == "loading"
        assert machine["jobs"][0]["state"] == "loading"


class TestTheEta:
    def test_a_ready_session_charges_no_startup(self) -> None:
        estimate = eta.RateEstimate(pages_per_second=1.0, latency_seconds=0.0, source="measured")
        entry: dict[str, Any] = {
            "generation_id": "g", "done_pages": 0, "total_pages": 100,
            "started_at": NOW, "session_started_at": NOW - 300, "session_ready": True,
            "delivered": True,
        }
        eta._price_running(
            entry, rate_for=lambda *a, **k: estimate,
            startup_for=lambda *a, **k: StartupEstimate(seconds=600.0, source="prior"),
            now=NOW,
        )
        assert entry["startup_seconds"] is None
        assert entry["eta_seconds"] == round(estimate.volume_seconds(100))


def _worker(storage: Path) -> OCRWorker:
    (storage / "library").mkdir(exist_ok=True)
    return OCRWorker(
        storage_path=storage, poll_interval=30.0,
        generations=parse_generation_list([
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "hayai", "engine": "hayai-nova"},
        ]),
        engines_python_path=Path(sys.executable), concurrency=1, sessions=True,
    )


class TestTheCard:
    def test_the_card_follows_the_session(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        job = (tmp_path / "library" / "Alpha" / "Volume 1.cbz", row.id)
        clock = _SessionClock()
        # A processor's session: its runner has neither said it is ready nor
        # been handed the archive yet.
        worker.begin_ocr_job(job, row, slot=0, delivered=False)
        card = worker._active_progress[job]
        # Opened before the runner has said it is ready: the model is loading.
        assert card["session_ready"] is False
        entry = _SessionJob(job=job, generation=row, volume=SimpleNamespace(id="v1"),  # type: ignore[arg-type]
                            clock=clock)
        inflight = {"v1": entry}
        worker._handle_session_event(
            {"event": "ready", "startup_seconds": 12.0}, row, inflight, ["v1"], clock
        )
        assert worker._active_progress[job]["session_ready"] is True
        assert job_state(worker._active_progress[job]) == "waiting"  # not delivered yet
        worker._handle_session_event(
            {"event": "volume_started", "id": "v1", "pages": 40}, row, inflight, ["v1"], clock
        )
        card = worker._active_progress[job]
        assert card["session_ready"] is True and card["delivered"] is True
        assert card["status"] == "starting"
        assert job_state(card) == "running"

    def test_a_volume_submitted_to_a_warm_session_opens_ready(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        job = (tmp_path / "library" / "Alpha" / "Volume 2.cbz", row.id)
        worker.begin_ocr_job(job, row, slot=0, session_ready=True, delivered=True)
        assert job_state(worker._active_progress[job]) == "running"

    def test_the_status_api_passes_it_through(self) -> None:
        entry = QueueAPI._progress_entry({
            "series": "S", "volume": "V", "status": "starting",
            "session_ready": False, "delivered": True, "started_at": time.time(),
        })
        assert entry["session_ready"] is False
        assert entry["delivered"] is True
        assert QueueAPI._progress_entry({"status": "running"})["session_ready"] is None

    def test_a_fetched_archive_ends_the_wait(self, tmp_path: Path) -> None:
        worker = _worker(tmp_path)
        row = worker.generations[1]
        job = (tmp_path / "library" / "Alpha" / "Volume 3.cbz", row.id)
        clock = _SessionClock(ready_at=time.time())
        worker.begin_ocr_job(job, row, slot=0, session_ready=True, delivered=False)
        assert job_state(worker._active_progress[job]) == "waiting"
        entry = _SessionJob(job=job, generation=row, volume=SimpleNamespace(id="v3"),  # type: ignore[arg-type]
                            clock=clock)
        worker._handle_session_event(
            {"event": "fetch", "id": "v3", "state": "ready"}, row, {"v3": entry}, ["v3"], clock
        )
        assert worker._active_progress[job]["delivered"] is True
        assert job_state(worker._active_progress[job]) == "running"
