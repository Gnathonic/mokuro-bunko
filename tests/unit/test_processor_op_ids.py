"""Ids off the wire never become a path outside the processor's storage.

A session id and a claim id name log files on the processor
(``logs/session.<sid>.log``, ``logs/<title>.<claim>.log``). The library
mints them itself -- ``secrets.token_hex`` and ``v<n>`` -- so a well-behaved
one never sends anything but letters, digits, ``_`` and ``-``. An op whose
id is anything else is a library to distrust, and is dropped whole.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.processor.bridge import RunnerBridge


class _Client:
    """Records what the bridge asks of the library; answers nothing."""

    def __init__(self) -> None:
        self.calls: list[tuple[str, Any]] = []

    def open_events(self, sid: str) -> Any:
        self.calls.append(("open_events", sid))
        raise OSError("not reachable in this test")

    def open_bench_events(self, bid: str) -> Any:
        self.calls.append(("open_bench_events", bid))
        raise OSError("not reachable in this test")

    def close(self) -> None:
        self.calls.append(("close", None))


def _bridge(tmp_path: Path) -> tuple[RunnerBridge, _Client]:
    client = _Client()
    bridge = RunnerBridge(
        client,  # type: ignore[arg-type]
        storage=tmp_path / "storage",
        engines_python=None,
        runner=tmp_path / "runner.py",
    )
    return bridge, client


HOSTILE = ["x/../../../../pwn", "../pwn", "a\\b", "a b", "", "x" * 65, "ok\n"]


@pytest.mark.parametrize("sid", HOSTILE)
def test_open_session_with_a_hostile_sid_is_dropped(tmp_path: Path, sid: str) -> None:
    bridge, client = _bridge(tmp_path)
    bridge.handle({"op": "open_session", "sid": sid, "generation": {"id": "g"}})
    assert client.calls == []
    assert not list(tmp_path.rglob("*.log"))


@pytest.mark.parametrize("claim", HOSTILE[:-3])
def test_volume_with_a_hostile_claim_is_dropped(tmp_path: Path, claim: str) -> None:
    bridge, client = _bridge(tmp_path)
    bridge.handle({"op": "volume", "sid": "a1b2c3", "claim": claim, "archive": "S/V.cbz"})
    assert client.calls == []


def test_the_ids_a_library_mints_pass(tmp_path: Path) -> None:
    bridge, client = _bridge(tmp_path)
    bridge.handle({"op": "open_session", "sid": "0f3a9c11be42", "generation": {"id": "g"}})
    assert client.calls[0] == ("open_events", "0f3a9c11be42")
