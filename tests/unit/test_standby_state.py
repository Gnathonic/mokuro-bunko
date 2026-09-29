"""A machine the earliest-finish scheduler is leaving work to others: "Standby".

It COULD run what is queued, but faster machines will finish it sooner
(`OCRWorker._claim` leaves it the volumes and sets the slot's
`waiting_for_faster`). Its card says so rather than "Idle", at every display
level. A machine that cannot run the queued work, a held machine, and an
empty queue keep their own states.
"""

from __future__ import annotations

from pathlib import Path

from mokuro_bunko.queue.shape import STATE_STANDBY, shape_status
from tests.unit.test_eft_claim import _rig
from tests.unit.test_queue_display import _raw


def test_the_slow_machine_left_the_queue_is_on_standby(tmp_path: Path) -> None:
    worker, _row, tower, desktop = _rig(tmp_path, 1)
    assert worker.claim_next(desktop) is None
    assert desktop.waiting_for_faster is True
    rows = {row["machine"]: row for row in worker.connected_machines()}
    assert rows["desktop"].get("standby") is True
    assert "standby" not in rows["tower"]
    # Once it takes a volume -- or the queue empties -- it is not.
    job = worker.claim_next(tower)
    assert job is not None
    assert worker.claim_next(desktop) is None
    assert desktop.waiting_for_faster is False, "nothing queued left for it"
    rows = {row["machine"]: row for row in worker.connected_machines()}
    assert "standby" not in rows["desktop"]


def test_an_empty_queue_is_never_standby(tmp_path: Path) -> None:
    worker, _row, _tower, desktop = _rig(tmp_path, 0)
    assert worker.claim_next(desktop) is None
    assert all("standby" not in row for row in worker.connected_machines())


def test_a_machine_that_cannot_run_the_work_is_never_standby(tmp_path: Path) -> None:
    worker, row, _tower, desktop = _rig(tmp_path, 1)
    # desktop's catalog cannot run the row: the walk never leaves it anything.
    desktop.processor.entry.catalog = {"engines": [], "detectors": [], "devices": []}
    assert worker.claim_next(desktop) is None
    assert desktop.waiting_for_faster is False
    assert all("standby" not in r for r in worker.connected_machines())


def test_a_held_machine_keeps_its_own_state(tmp_path: Path) -> None:
    worker, _row, _tower, desktop = _rig(tmp_path, 1)
    assert worker.claim_next(desktop) is None
    worker._holds["desktop"] = 1
    rows = {row["machine"]: row for row in worker.connected_machines()}
    assert "standby" not in rows["desktop"]


class TestTheShape:
    def _raw(self, **tower: object) -> dict[str, object]:
        raw = _raw()
        raw["current_jobs"] = [j for j in raw["current_jobs"] if j["machine"] != "tower"]
        raw["connected_machines"] = [
            {"machine": "local", "slots": 1}, {"machine": "tower", "slots": 1, **tower},
        ]
        return raw

    def test_every_level_says_standby(self) -> None:
        raw = self._raw(standby=True)
        for level in ("minimal", "normal", "detailed"):
            for admin in (False, True):
                tower = shape_status(raw, level, admin=admin)["machines"][1]
                assert tower["state"] == STATE_STANDBY == "standby", (level, admin)
                assert "standby" not in tower, "the state says it; no extra field"

    def test_work_a_benchmark_and_a_hold_outrank_it(self) -> None:
        raw = self._raw(standby=True, configuring={"key": "g-2", "generation": "nova",
                                                   "auto": True})
        assert shape_status(raw, "normal", admin=True)["machines"][1]["state"] == "configuring"
        raw = self._raw(standby=True, held="downloads", held_until=1.0)
        assert shape_status(raw, "normal", admin=True)["machines"][1]["state"] == "held"
        raw = _raw()
        raw["connected_machines"] = [{"machine": "local", "slots": 1},
                                     {"machine": "tower", "slots": 1, "standby": True}]
        assert shape_status(raw, "normal", admin=True)["machines"][1]["state"] != "standby"

    def test_without_it_an_empty_machine_is_idle(self) -> None:
        assert shape_status(self._raw(), "normal", admin=False)["machines"][1]["state"] == "idle"
