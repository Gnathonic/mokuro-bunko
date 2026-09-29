"""Queue reports carry a horizon of the queue, and the whole queue's totals.

A 12k-volume queue made the queue page's status 2 MB and the reader's queue
file 5.6 MB, rebuilt on every change. Nobody reads past the first hundred:
both now list the next `QUEUE_REPORT_LIMIT` and count the rest.
"""

from __future__ import annotations

from typing import Any

from mokuro_bunko.queue.shape import QUEUE_REPORT_LIMIT, shape_status


def _raw(pending: int) -> dict[str, Any]:
    return {
        "current_jobs": [],
        "pending_ocr": [
            {"series": f"S{n // 10}", "volume": f"V{n % 10}", "generation": "hayai",
             "generation_id": "g-2", "engine": "hayai-nova", "eta_at": None}
            for n in range(pending)
        ],
        "failed": [],
        "generations": [],
    }


def test_the_status_lists_a_hundred_and_counts_all() -> None:
    for level in ("minimal", "normal", "detailed"):
        payload = shape_status(_raw(250), level, admin=False)
        assert payload["pending_count"] == 250, level
        assert len(payload["pending"]) <= QUEUE_REPORT_LIMIT, level


def test_a_short_queue_is_listed_whole() -> None:
    payload = shape_status(_raw(40), "detailed", admin=True)
    assert payload["pending_count"] == 40
    assert len(payload["pending"]) == 40


def test_the_limit_is_a_hundred() -> None:
    assert QUEUE_REPORT_LIMIT == 100
