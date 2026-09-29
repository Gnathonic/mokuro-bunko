"""The generations list never waits for its library-derived counts.

On a 12k-volume library the counts (a scan of every volume, the provenance
table) took long enough that the OCR settings page crawled. They are worked
out in the background, cached, and filled in by the page when ready.
"""

from __future__ import annotations

import threading
import time
from typing import Any

import pytest

from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.ocr.generations import parse_generation_list

ROWS = parse_generation_list([
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {"name": "hayai", "engine": "hayai-nova"},
])


def _api(monkeypatch: pytest.MonkeyPatch, delay: float) -> tuple[AdminAPI, list[int]]:
    api = AdminAPI.__new__(AdminAPI)
    api._gen_stats_lock = threading.Lock()
    api._gen_stats = None
    api._gen_stats_thread = None
    computed: list[int] = []

    def slow_counts(rows: Any) -> dict[str, tuple[int, int]]:
        computed.append(1)
        time.sleep(delay)
        return {row.id: (3, 10) for row in rows}

    monkeypatch.setattr(api, "_generation_volume_counts", slow_counts)
    monkeypatch.setattr(api, "_generation_skipped_counts", lambda rows: {})
    monkeypatch.setattr(api, "_generation_machine_counts", lambda rows: {})
    return api, computed


def test_a_slow_count_does_not_hold_the_list(monkeypatch: pytest.MonkeyPatch) -> None:
    api, _ = _api(monkeypatch, delay=1.5)
    started = time.monotonic()
    assert api._generation_stats(ROWS, wait=0.2) is None
    assert time.monotonic() - started < 1.0


def test_the_counts_arrive_when_ready_and_are_then_cached(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    api, computed = _api(monkeypatch, delay=0.3)
    assert api._generation_stats(ROWS) is None
    stats = api._generation_stats(ROWS, wait=5.0)
    assert stats is not None and stats["counts"][ROWS[1].id] == (3, 10)
    for _ in range(5):
        assert api._generation_stats(ROWS) is stats
    assert len(computed) == 1


def test_a_changed_list_of_rows_is_counted_again(monkeypatch: pytest.MonkeyPatch) -> None:
    api, computed = _api(monkeypatch, delay=0.0)
    assert api._generation_stats(ROWS, wait=5.0) is not None
    fewer = ROWS[:1]
    assert api._generation_stats(fewer, wait=5.0) is not None
    assert len(computed) == 2


def test_one_computation_at_a_time(monkeypatch: pytest.MonkeyPatch) -> None:
    api, computed = _api(monkeypatch, delay=0.5)
    for _ in range(10):
        api._generation_stats(ROWS)
    api._generation_stats(ROWS, wait=5.0)
    assert len(computed) == 1
