"""Which machine a volume goes to: the one that would FINISH it first.

Measured live (four machines, 80 volumes): whichever idle machine
asked first took the volume, so a lone novel volume went to the slowest box
-- 331 s where the fastest takes ~35 s -- and at the end of a queue the
slowest boxes held on-deck volumes while the fastest one sat idle, ~190 s of
a 535 s run. `earliest_finish_claim` walks the queue in its own order and
gives each volume to the lane that finishes it first; a machine claims the
first volume that walk gives IT.
"""

from __future__ import annotations

from typing import Any

import pytest

from mokuro_bunko.ocr.eta import (
    SOURCE_BENCH,
    EftLane,
    RateEstimate,
    earliest_finish_claim,
)

# pages per second, per (row, machine)
SPEED = {
    ("nova", "tower"): 10.0,
    ("nova", "rig-c"): 1.0,
    ("nova", "local"): 5.0,
    ("fp16", "tower"): 30.0,
    ("fp16", "rig-c"): 5.0,
}
STARTUP = 5.0


def rate_for(generation_id: str, machine: str) -> RateEstimate | None:
    speed = SPEED.get((generation_id, machine))
    if speed is None:
        return None
    # No fixed cost: a volume is pages / speed, which keeps the arithmetic
    # below readable.
    return RateEstimate(speed, SOURCE_BENCH, 0, 0.0)


def startup_for(generation_id: str, machine: str) -> float:
    del generation_id, machine
    return STARTUP


def lane(machine: str, free_in: float = 0.0, warm: str | None = None,
         rows: tuple[str, ...] = ("nova", "fp16")) -> EftLane:
    return EftLane(key=machine, machine=machine, free_in=free_in, warm=warm,
                   rows=frozenset(rows))


def claim(jobs: list[tuple[Any, str, int | None]], lanes: list[EftLane], asking: str,
          **kwargs: Any) -> Any:
    return earliest_finish_claim(jobs, lanes, asking, rate_for=rate_for,
                                 startup_for=startup_for, **kwargs)


class TestALoneVolume:
    def test_it_goes_to_the_machine_that_finishes_it_first(self) -> None:
        jobs = [("novel", "nova", 292)]
        lanes = [lane("rig-c"), lane("tower")]
        slow = claim(jobs, lanes, "rig-c")
        assert slow is not None and slow.mine is None, "the slow box leaves it"
        assert [(left.job, left.to) for left in slow.left] == [("novel", "tower")]
        assert slow.left[0].there == pytest.approx(5 + 29.2)
        assert slow.left[0].here == pytest.approx(5 + 292)
        fast = claim(jobs, lanes, "tower")
        assert fast is not None and fast.mine == "novel"

    def test_a_busy_fast_machine_still_wins_when_it_frees_soon_enough(self) -> None:
        jobs = [("novel", "nova", 292)]
        lanes = [lane("rig-c"), lane("tower", free_in=40.0, warm="nova")]
        slow = claim(jobs, lanes, "rig-c")
        assert slow is not None and slow.mine is None
        assert slow.left[0].there == pytest.approx(40 + 29.2), "warm: no startup"

    def test_but_not_when_it_is_busy_for_longer_than_the_slow_one_takes(self) -> None:
        jobs = [("small", "nova", 20)]
        lanes = [lane("rig-c"), lane("tower", free_in=300.0)]
        slow = claim(jobs, lanes, "rig-c")
        assert slow is not None and slow.mine == "small"


class TestALongQueue:
    def test_the_slow_machine_still_gets_work_further_down(self) -> None:
        """Greedy per-volume "someone else is faster" would starve it forever."""
        jobs = [(f"v{n}", "nova", 200) for n in range(12)]
        lanes = [lane("tower"), lane("rig-c")]
        fast = claim(jobs, lanes, "tower")
        assert fast is not None and fast.mine == "v0"
        slow = claim(jobs, lanes, "rig-c")
        assert slow is not None and slow.mine is not None
        index = int(slow.mine[1:])
        # tower finishes volume j at 5 + 20 (j + 1); rig-c its first at 205.
        assert 7 <= index <= 9, index
        assert all(left.to == "tower" for left in slow.left)
        assert len(slow.left) == index

    def test_the_queue_order_is_never_changed_for_the_asking_machine(self) -> None:
        """It takes the FIRST volume the walk gives it, not the best one."""
        jobs = [("a", "nova", 20), ("b", "nova", 20)]
        lanes = [lane("tower"), lane("rig-c")]
        fast = claim(jobs, lanes, "tower")
        assert fast is not None and fast.mine == "a"


class TestWhoMayTakeIt:
    def test_a_lane_that_cannot_run_the_row_does_not_compete(self) -> None:
        jobs = [("novel", "nova", 292)]
        lanes = [lane("rig-c"), lane("tower", rows=("fp16",))]
        slow = claim(jobs, lanes, "rig-c")
        assert slow is not None and slow.mine == "novel"

    def test_a_volume_the_asker_cannot_run_is_not_counted_as_left(self) -> None:
        jobs = [("x", "fp16", 100), ("y", "nova", 10)]
        lanes = [lane("local", rows=("nova",)), lane("tower")]
        mine = claim(jobs, lanes, "local")
        assert mine is not None
        assert [left.job for left in mine.left] == [], "not ours to leave"

    def test_a_warm_session_pays_no_startup(self) -> None:
        jobs = [("v", "nova", 100)]
        # local is warm on the row (10 s for 100 pages at 5 p/s... + 0 startup = 20 s);
        # tower cold: 5 + 10 = 15 s -- tower still wins, by less than the margin?
        lanes = [lane("local", warm="nova"), lane("tower")]
        mine = claim(jobs, lanes, "local")
        assert mine is not None
        # local 20 s vs tower 15 s: 20 > 15 * 1.1 + 2 = 18.5, so tower takes it.
        assert mine.mine is None


class TestTheAskingMachinesMargin:
    def test_a_near_tie_stays_with_the_machine_that_is_asking(self) -> None:
        """The other machine has to come and claim it; a hair's advantage is
        not worth the wait or the churn."""
        jobs = [("v", "nova", 100)]
        # local 5 + 20 = 25 s, tower busy 12 s then 5 + 10 = 27 s -- local wins
        # outright; make tower a hair faster instead:
        lanes = [lane("local"), lane("tower", free_in=8.0)]
        mine = claim(jobs, lanes, "local")
        assert mine is not None and mine.mine == "v", (
            "tower 8 + 5 + 10 = 23 s against local's 25 s is inside the margin"
        )


class TestWhenItCannotBePriced:
    def test_an_unmeasured_machine_means_the_old_rule(self) -> None:
        jobs = [("v", "nova", 100)]
        lanes = [lane("rig-c"), lane("mystery")]
        assert claim(jobs, lanes, "rig-c") is None

    def test_no_page_counts_at_all_means_the_old_rule(self) -> None:
        jobs = [("v", "nova", None)]
        lanes = [lane("rig-c"), lane("tower")]
        assert claim(jobs, lanes, "rig-c") is None

    def test_an_unknown_length_is_priced_at_the_median(self) -> None:
        jobs = [("a", "nova", 200), ("b", "nova", None), ("c", "nova", 200)]
        lanes = [lane("tower"), lane("rig-c")]
        slow = claim(jobs, lanes, "rig-c")
        assert slow is not None
        assert [left.job for left in slow.left] == ["a", "b", "c"], (
            "tower finishes all three (25, 45, 65 s) before rig-c's 205 s"
        )
        assert slow.mine is None

    def test_the_walk_is_bounded(self) -> None:
        jobs = [(f"v{n}", "nova", 200) for n in range(1000)]
        lanes = [lane("tower"), lane("rig-c", rows=("fp16",))]
        slow = claim(jobs, lanes, "rig-c", limit=50)
        assert slow is not None and slow.mine is None and slow.left == []


class TestWhetherTheFasterLaneIsBusy:
    def test_an_idle_lane_is_not_busy_first(self) -> None:
        slow = claim([("v", "nova", 292)], [lane("rig-c"), lane("tower")], "rig-c")
        assert slow is not None and slow.left[0].busy_first is False

    def test_a_lane_with_work_in_flight_is(self) -> None:
        slow = claim([("v", "nova", 292)],
                     [lane("rig-c"), lane("tower", free_in=40.0)], "rig-c")
        assert slow is not None and slow.left[0].busy_first is True

    def test_a_lane_the_walk_already_gave_a_volume_is(self) -> None:
        jobs = [("a", "nova", 100), ("b", "nova", 100)]
        slow = claim(jobs, [lane("rig-c"), lane("tower")], "rig-c")
        assert slow is not None
        assert [left.busy_first for left in slow.left] == [False, True]


class TestTheMarginIsCapped:
    def test_a_far_booked_fast_lane_still_beats_a_slow_asker(self) -> None:
        """Live: tower ~700 s, rig-c ~770 s on a 27-page volume at the
        end of a booked queue; a 10% margin (~72 s) gave it to rig-c."""
        jobs = [("small", "nova", 27)]
        # tower busy 697.3 s then 2.7 s: 700 s; rig-c cold 5 + 27 s... make
        # rig-c's finish 770 by giving it work in flight.
        lanes = [lane("rig-c", free_in=738.0, warm="nova"), lane("tower", free_in=697.3,
                                                                     warm="nova")]
        slow = claim(jobs, lanes, "rig-c")
        assert slow is not None
        assert slow.left and slow.left[0].there == pytest.approx(700.0)
        assert slow.left[0].here == pytest.approx(765.0)
        assert slow.mine is None, "65 s later is not a near-tie"

    def test_a_real_near_tie_still_stays_with_the_asker(self) -> None:
        jobs = [("v", "nova", 100)]
        lanes = [lane("local", free_in=600.0, warm="nova"),
                 lane("tower", free_in=607.0, warm="nova")]
        # local 600 + 20 = 620 s; tower 607 + 10 = 617 s: 3 s is inside 5 + 2.
        mine = claim(jobs, lanes, "local")
        assert mine is not None and mine.mine == "v"
