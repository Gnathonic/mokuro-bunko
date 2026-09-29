"""The claim gives a volume to the machine that finishes it first (live lanes).

`earliest_finish_claim` is tested on its own in test_eft_assign.py; this is
the worker feeding it the running scan's real slots: who is idle, who is
busy and for how long, who may take the row at all.
"""

from __future__ import annotations

import sys
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import watcher as watcher_module
from mokuro_bunko.ocr.devices import DeviceCatalog
from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.watcher import EFT_CLAIM_GRACE_SECONDS, OCRWorker
from tests.unit.test_warm_session_first import FULL, _library


def _rig(tmp_path: Path, volumes: int, *, autobench: bool = False) -> tuple[OCRWorker, Any, Any, Any]:
    """tower reads ~10 pages/s, desktop ~4 and takes 16 s to start; 100-page volumes."""
    (tmp_path / "inbox").mkdir(exist_ok=True)
    _library(tmp_path, *[f"Volume {n}" for n in range(1, volumes + 1)])
    registry = ProcessorRegistry()
    worker = OCRWorker(
        storage_path=tmp_path, poll_interval=30.0,
        generations=parse_generation_list(
            [{"name": "mokuro", "engine": "mokuro", "primary": True},
             {"name": "hayai", "engine": "hayai-nova", "detector": "ctd"}],
            devices=DeviceCatalog(),
        ),
        engines_python_path=Path(sys.executable), concurrency=1, sessions=True,
        remote=registry, local_processing=False, autobench=autobench,
        page_count_lookup=lambda path: 100,
    )
    row = worker.generations[1]
    slots = []
    for index, (name, seconds) in enumerate((("tower", 10.0), ("desktop", 25.0))):
        entry = registry.register(username=name, name=name, host={}, catalog=FULL,
                                  max_sessions=1)
        entry.stream_open = True
        key = worker._rate_key(row.id, name)
        for _ in range(3):
            worker.rates.record_volume(key, 100, seconds)
        worker.rates.record_startup(key, 16.0 if name == "desktop" else 3.0)
        slots.append(worker._make_remote_slot(index, entry))
    for slot in slots:
        slot.running = True  # both slots' loops are running in the scan
    worker._active_slots = list(slots)
    return worker, row, slots[0], slots[1]


class TestALoneVolume:
    def test_the_slow_machine_leaves_it_and_waits(self, tmp_path: Path) -> None:
        worker, _row, tower, desktop = _rig(tmp_path, 1)
        assert worker.claim_next(desktop) is None
        assert desktop.waiting_for_faster is True, "waits for tower, does not leave the scan"
        job = worker.claim_next(tower)
        assert job is not None and job[0].stem == "Volume 1"

    def test_it_says_why_once(self, tmp_path: Path) -> None:
        worker, _row, _tower, desktop = _rig(tmp_path, 1)
        said: list[str] = []
        worker._log = said.append  # type: ignore[method-assign]
        worker.claim_next(desktop)
        worker.claim_next(desktop)
        lines = [line for line in said if line.startswith("Leaving Volume 1.cbz")]
        assert len(lines) == 1, said
        assert "tower" in lines[0] and "desktop" in lines[0]

    def test_a_held_fast_machine_is_not_waited_for(self, tmp_path: Path) -> None:
        worker, _row, _tower, desktop = _rig(tmp_path, 1)
        worker._holds["tower"] = 1
        assert worker.claim_next(desktop) is not None

    def test_an_unmeasured_fast_machine_is_not_waited_for(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """tower is benchmarked before it may take the row; desktop is not
        told to wait for a machine that cannot claim it yet."""
        worker, _row, _tower, desktop = _rig(tmp_path, 1, autobench=True)
        monkeypatch.setattr(
            worker, "autobench_needed", lambda entry, row: entry.name == "tower"
        )
        worker._want_autobench = lambda entry, row: None  # type: ignore[method-assign]
        assert worker.claim_next(desktop) is not None
        assert desktop.waiting_for_faster is False

    def test_a_machine_that_is_not_coming_is_not_waited_for_forever(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker, _row, _tower, desktop = _rig(tmp_path, 1)
        assert worker.claim_next(desktop) is None
        now = watcher_module.time.time()
        # tower (3 s start) never claimed it: past its start and the grace.
        monkeypatch.setattr(
            watcher_module.time, "time", lambda: now + 3.0 + EFT_CLAIM_GRACE_SECONDS + 1.0
        )
        job = worker.claim_next(desktop)
        assert job is not None and job[0].stem == "Volume 1"
        assert desktop.waiting_for_faster is False


class TestAQueue:
    def test_the_slow_machine_still_gets_work(self, tmp_path: Path) -> None:
        worker, _row, _tower, desktop = _rig(tmp_path, 6)
        job = worker.claim_next(desktop)
        assert job is not None, "tower cannot reach every volume before desktop would"
        assert job[0].stem != "Volume 1", "the first ones are tower's"

    def test_a_busy_fast_machine_counts_its_work_in_flight(self, tmp_path: Path) -> None:
        worker, _row, tower, desktop = _rig(tmp_path, 2)
        first = worker.claim_next(tower)
        assert first is not None
        worker._active_progress[first] = {"eta_seconds": 600.0}
        job = worker.claim_next(desktop)
        assert job is not None, "tower is busy for ten minutes; desktop reads it now"


class TestTheOldRuleStands:
    def test_without_a_running_scan_nothing_changes(self, tmp_path: Path) -> None:
        worker, _row, _tower, desktop = _rig(tmp_path, 1)
        worker._active_slots = []
        assert worker.claim_next(desktop) is not None

    def test_a_machine_with_no_rate_means_first_come(self, tmp_path: Path) -> None:
        worker, row, tower, desktop = _rig(tmp_path, 1)
        worker.rates = type(worker.rates)()  # forget every rate
        assert worker.claim_next(desktop) is not None


class TestTheCardsSayWhatIsBeingConfigured:
    def test_a_machine_with_a_benchmark_line_carries_it(self, tmp_path: Path) -> None:
        worker, _row, _tower, _desktop = _rig(tmp_path, 1)
        worker.bench_service = type("Bench", (), {
            "configuring": lambda self: {
                "tower": {"key": "g-2", "generation": "hayai", "auto": True}
            },
        })()
        rows = {row["machine"]: row for row in worker.connected_machines()}
        assert rows["tower"]["configuring"] == {"key": "g-2", "generation": "hayai", "auto": True}
        assert "configuring" not in rows["desktop"]


class TestTheDeadlineFollowsABusyMachine:
    """Live: a paddle volume was left to this server at 01:35:51,
    predicted to start once its current volume was done; the deadline was
    fixed then, ran out at ~01:39:14 while this server was still busy (it had
    taken another volume first), and at 01:39:42 the slowest box took it --
    ~3150 s there against ~470 s here."""

    def test_a_busy_fast_machine_is_still_waited_for_past_its_first_prediction(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker, _row, tower, desktop = _rig(tmp_path, 2)
        first = worker.claim_next(tower)
        assert first is not None
        worker._active_progress[first] = {"eta_seconds": 20.0}
        assert worker.claim_next(desktop) is None, "tower finishes it sooner even after 20 s"
        now = watcher_module.time.time()
        # Well past the FIRST prediction (20 s + startup + grace) -- but tower
        # is still busy: its volume turned out slower than predicted.
        later = now + 20.0 + 3.0 + EFT_CLAIM_GRACE_SECONDS + 30.0
        monkeypatch.setattr(watcher_module.time, "time", lambda: later)
        worker._active_progress[first] = {"eta_seconds": 10.0}
        assert worker.claim_next(desktop) is None, "still left to tower, which is still busy"
        assert desktop.waiting_for_faster is True

    def test_an_idle_machine_that_never_comes_is_not_waited_for_forever(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker, _row, _tower, desktop = _rig(tmp_path, 1)
        assert worker.claim_next(desktop) is None
        now = watcher_module.time.time()
        # Asked again and again while tower stays idle: the clock is not reset.
        for step in range(1, 4):
            monkeypatch.setattr(watcher_module.time, "time", lambda s=step: now + 5.0 * s)
            assert worker.claim_next(desktop) is None
        monkeypatch.setattr(
            watcher_module.time, "time", lambda: now + 3.0 + EFT_CLAIM_GRACE_SECONDS + 1.0
        )
        job = worker.claim_next(desktop)
        assert job is not None and job[0].stem == "Volume 1"


class TestAQueueWithNoSidecarsYet:
    """Live (MOKURO_EFT_TRACE): 240 of 282 claims fell back to
    first-come. The page-count cache is compiled from the sidecars, so the
    volumes just queued -- the ones whose sidecars are missing -- had no
    length, and the walk could price neither them nor the lanes reading
    them."""

    def test_the_length_is_read_from_the_archive(self, tmp_path: Path) -> None:
        worker, _row, tower, desktop = _rig(tmp_path, 1)
        worker.page_count_lookup = lambda path: None  # type: ignore[assignment]
        assert worker.claim_next(desktop) is None, "left to tower, not first-come"
        assert desktop.waiting_for_faster is True
        job = worker.claim_next(tower)
        assert job is not None

    def test_a_volume_in_flight_is_priced_from_its_archive_too(self, tmp_path: Path) -> None:
        worker, _row, tower, desktop = _rig(tmp_path, 2)
        worker.page_count_lookup = lambda path: None  # type: ignore[assignment]
        first = worker.claim_next(tower)
        assert first is not None
        # No progress card yet (the session has not reported): priced from
        # its archive, not a reason to give up on the whole walk.
        worker._active_progress.pop(first, None)
        assert worker._eft_lanes(desktop, {first[1]}, worker._lane_pricing()[0]) is not None

    def test_a_broken_archive_is_just_unknown(self, tmp_path: Path) -> None:
        worker, _row, _tower, _desktop = _rig(tmp_path, 1)
        broken = tmp_path / "library" / "Alpha" / "Broken.cbz"
        broken.write_bytes(b"not a zip")
        assert worker._archive_pages(broken) is None


class TestASlotWhoseLoopEnded:
    """Live: tower's loop ended in a lull while the scan went on;
    the walk kept leaving volumes to its lane, their deadline ran out, and
    the slowest card took them."""

    def test_is_no_lane_to_leave_a_volume_to(self, tmp_path: Path) -> None:
        worker, _row, tower, desktop = _rig(tmp_path, 1)
        tower.running = False
        job = worker.claim_next(desktop)
        assert job is not None, "nobody to wait for: desktop reads it"
        assert desktop.waiting_for_faster is False

    def test_is_started_again_once_the_queue_moves(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        import threading
        import time as real_time

        worker, _row, tower, desktop = _rig(tmp_path, 1)
        for slot in (tower, desktop):
            slot.running = False
        calls: dict[str, int] = {"tower": 0, "desktop": 0}
        release = threading.Event()

        def fake_loop(slot: Any) -> None:
            name = "tower" if slot is tower else "desktop"
            calls[name] += 1
            if name == "desktop" and calls[name] == 1:
                # desktop keeps the scan alive until tower has been restarted
                release.wait(timeout=10)

        monkeypatch.setattr(worker, "_run_ocr_slot_loop", fake_loop)
        monkeypatch.setattr(watcher_module, "SLOT_SUPERVISE_SECONDS", 0.05)
        supervisor = threading.Thread(target=worker._supervise_slots, args=([tower, desktop],))
        supervisor.start()
        deadline = real_time.monotonic() + 5
        while calls["tower"] < 1 and real_time.monotonic() < deadline:
            real_time.sleep(0.01)
        real_time.sleep(0.2)
        assert calls["tower"] == 1, "not restarted while the queue has not moved"
        with worker._lock:
            worker._bump_queue_generation()
        while calls["tower"] < 2 and real_time.monotonic() < deadline:
            real_time.sleep(0.01)
        assert calls["tower"] == 2, "started again once the queue moved"
        release.set()
        supervisor.join(timeout=5)
        assert not supervisor.is_alive()


class TestTheClaimTakesTheWalksVolume:
    """Traced live, 02:53:03: the walk gave rig-c a small hayai volume,
    but the claim loop goes through the queue in its own order (volumes for
    an idle device first) and took the first volume the walk had not left to
    anyone -- a 238-page paddle volume, ~1850 s there against ~260 s on tower."""

    def test_the_walks_volume_is_taken_whatever_order_the_loop_uses(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker, _row, _tower, desktop = _rig(tmp_path, 6)
        expected = worker._eft_left(
            desktop, desktop.processor_id,
            worker._ocr_candidates(exclude=set()),
        )
        assert expected[0], "the walk leaves tower the first volumes"
        assert expected[2] is not None, "and gives desktop one further down"
        monkeypatch.setattr(
            worker, "_in_device_order",
            lambda proposed, *args, **kwargs: list(reversed(list(proposed))),
        )
        job = worker.claim_next(desktop)
        assert job is not None
        # The loop's order now starts at the LAST volume -- one the walk
        # gives tower, not desktop -- but the claim takes desktop's own.
        assert job[0].stem not in {"Volume 6"}, job
        assert job == expected[2]


class TestWakingTheOthers:
    """Live: 23,920 claim decisions in a 34-minute run, ~12 a second. Every
    claim that left a volume woke every waiting slot, each of which walked
    the queue again, left it again and woke the rest."""

    def test_a_volume_is_announced_once_not_on_every_claim(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        worker, _row, _tower, desktop = _rig(tmp_path, 1)
        woken: list[int] = []
        real = worker._lock.notify_all
        monkeypatch.setattr(worker._lock, "notify_all", lambda: (woken.append(1), real())[1])
        assert worker.claim_next(desktop) is None
        assert len(woken) == 1, "newly left: the machine it is left to is woken"
        assert worker.claim_next(desktop) is None
        assert worker.claim_next(desktop) is None
        assert len(woken) == 1, "left again, nothing new: nobody is woken"
