"""The queue page's short-of-pages list is not re-read faster than it costs.

It walks every archive: seconds on a large library on a network share. A
compile asks for a re-read (`invalidate_skipped`) after every sidecar, and
eight machines landing sidecars kept it walking back to back.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

from mokuro_bunko.queue.api import QueueAPI


class _Control:
    def __init__(self) -> None:
        self.reads = 0

    def refresh_pending(self) -> None:
        return None

    def skipped_missing_pages(self) -> list[dict[str, Any]]:
        self.reads += 1
        return []


def _api(tmp_path: Path) -> tuple[QueueAPI, _Control]:
    (tmp_path / "library").mkdir()
    control = _Control()
    api = QueueAPI(lambda e, s: [], storage_base_path=str(tmp_path))
    api._ocr_control = control  # type: ignore[assignment]
    return api, control


def test_a_slow_read_is_not_repeated_for_every_compile(tmp_path: Path) -> None:
    api, control = _api(tmp_path)
    api._refresh()
    api._skipped_cost = 7.0  # the live library's walk
    for _ in range(10):
        api.invalidate_skipped()
        api._refresh()
    assert control.reads == 1


def test_a_cheap_read_follows_every_compile(tmp_path: Path) -> None:
    api, control = _api(tmp_path)
    api._refresh()
    api._skipped_cost = 0.0
    api.invalidate_skipped()
    api._refresh()
    assert control.reads == 2
