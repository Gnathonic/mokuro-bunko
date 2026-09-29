"""A volume's pending OCR and its recheck time (manifest `pending` / `recheck_after`)."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

import pytest

from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.volume_outlook import pending_entries, recheck_after

ROWS = parse_generation_list(
    [
        {"name": "mokuro-fp16", "engine": "mokuro", "primary": True, "precision": "auto-speed"},
        {"name": "hayai-nova-ppocr", "engine": "hayai-nova", "detector": "ppocr-manga"},
        {"name": "paddle", "engine": "paddle-manga"},
    ]
)
NOW = datetime(2026, 9, 27, 21, 0, 0, tzinfo=timezone.utc).timestamp()


def at(seconds: float) -> str:
    return datetime.fromtimestamp(NOW + seconds, tz=timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class TestPendingEntries:
    def test_kinds_ids_and_etas_from_the_plan(self) -> None:
        planned = [
            {"series": "S", "volume": "V", "generation_id": ROWS[0].id, "eta_at": at(840)},
            {"series": "S", "volume": "V", "generation_id": ROWS[1].id, "eta_at": at(1170)},
            {"series": "S", "volume": "Other", "generation_id": ROWS[2].id, "eta_at": at(5)},
        ]
        assert pending_entries(ROWS, "S", "V", planned) == [
            {"kind": "ocr", "id": "mokuro-fp16", "eta": at(840)},
            {"kind": "layer", "id": "hayai-nova-ppocr", "eta": at(1170)},
            {"kind": "layer", "id": "paddle", "eta": None},
        ]

    def test_an_unpriced_plan_entry_is_null(self) -> None:
        planned = [{"series": "S", "volume": "V", "generation_id": ROWS[0].id, "eta_at": None}]
        assert pending_entries(ROWS[:1], "S", "V", planned) == [
            {"kind": "ocr", "id": "mokuro-fp16", "eta": None}
        ]

    def test_a_priced_duplicate_wins(self) -> None:
        planned = [
            {"series": "S", "volume": "V", "generation_id": ROWS[0].id, "eta_at": None},
            {"series": "S", "volume": "V", "generation_id": ROWS[0].id, "eta_at": at(60)},
        ]
        assert pending_entries(ROWS[:1], "S", "V", planned)[0]["eta"] == at(60)

    def test_nothing_owed_is_empty(self) -> None:
        assert pending_entries([], "S", "V", []) == []

    def test_no_admin_details_leak(self) -> None:
        planned = [
            {
                "series": "S", "volume": "V", "generation_id": ROWS[0].id, "eta_at": at(60),
                "machine": "tower", "processor": "tower", "error": "boom", "reason": "x",
            }
        ]
        (entry,) = pending_entries(ROWS[:1], "S", "V", planned)
        assert set(entry) == {"kind", "id", "eta"}


class TestRecheckAfter:
    def test_nothing_pending_is_none(self) -> None:
        assert recheck_after([], NOW) is None

    def test_nothing_priced_is_300(self) -> None:
        assert recheck_after([{"eta": None}, {"eta": None}], NOW) == 300

    def test_earliest_eta_plus_ten(self) -> None:
        pending = [{"eta": at(1170)}, {"eta": None}, {"eta": at(85)}]
        assert recheck_after(pending, NOW) == 95

    @pytest.mark.parametrize(
        ("seconds", "expected"),
        [(0, 30), (-500, 30), (19, 30), (21, 31), (3590, 3600), (99999, 3600)],
    )
    def test_clamped_to_30_and_3600(self, seconds: int, expected: int) -> None:
        assert recheck_after([{"eta": at(seconds)}], NOW) == expected

    def test_whole_seconds_rounded_up(self) -> None:
        pending = [{"eta": at(100)}]
        assert recheck_after(pending, NOW + 0.4) == 110
        assert isinstance(recheck_after(pending, NOW + 0.4), int)


class TestPlanThrough:
    """`plan_queue(through=)`: one volume's ETAs without pricing the rest of the queue."""

    @staticmethod
    def plan(through: int | None) -> Any:
        from mokuro_bunko.ocr.eta import RateEstimate, StartupEstimate, plan_queue

        pending = [
            {"series": "S", "volume": f"V{index}", "generation_id": "g-1", "pages": 10}
            for index in range(5)
        ]
        return plan_queue(
            [],
            pending,
            lane_count=1,
            rate_for=lambda _g, **_k: RateEstimate(2.0, "session", 2),
            startup_for=lambda _g, **_k: StartupEstimate(5.0, "bench"),
            now=NOW,
            through=through,
        )

    def test_stops_after_the_item_and_prices_it_the_same(self) -> None:
        whole = self.plan(None)
        cut = self.plan(2)
        assert [e["volume"] for e in cut.pending] == ["V0", "V1", "V2"]
        assert [e["eta_at"] for e in cut.pending] == [e["eta_at"] for e in whole.pending[:3]]
        assert whole.done_at is not None
        assert cut.done_at is None  # the queue's end was not computed

    def test_through_the_last_item_is_the_whole_plan(self) -> None:
        assert self.plan(4).done_at == self.plan(None).done_at
