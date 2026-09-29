"""A stored benchmark counts only while it describes the row's precision MODE.

Live: every profile's `bench` was measured by an older precision
phase, and its `pages_per_second` is the precision that phase CHOSE --
paddle-manga on this server at fp16, 0.91 pages/s, against 0.09 at the fp32
it runs now. The earliest-finish pricing used it as the machine's prior, 10x
off.

A bench now counts only while the precision it ran at -- recorded outside
`best`, else the legacy phase's `chosen` trial -- is what the row's mode
resolves to on that machine (`profiles.stale_bench_reason`), and a bench
taken for another mode is stale outright. A stale one reads as absent (the
row is unmeasured there, so autobench measures it again), is dropped from the
file on the next save, and is said once in the log. A bench that cannot be
judged -- nobody reported what the device runs, and the answer depends on it
-- is kept. A machine's own precision pin no longer counts at all.
"""

from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr.remote import profiles as profiles_module
from mokuro_bunko.ocr.remote.profiles import (
    LOCAL_PROFILE,
    ProcessorProfiles,
    profile_filename,
    profiles_dir,
)

PADDLE = ["paddle-manga", "ppocr-manga", None]
HAYAI = ["hayai-nova", "ppocr-manga", 512]
MOKURO = ["mokuro", "ppocr-manga", None]
ON_CARD = {"gpu": "AMD Radeon RX 9070 XT", "devices": {"detect": "cpu", "engine": "gpu:0"}}


@pytest.fixture(autouse=True)
def _fresh_log_memory(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(profiles_module, "_STALE_LOGGED", set())
    monkeypatch.setattr(profiles_module, "_STALE_PENDING", {})


def legacy_trials(chosen: str) -> list[dict[str, Any]]:
    return [
        {"precision": p, "pages_per_second": 1.0, "passed": True, "chosen": p == chosen,
         **({"reference": True} if p == "fp32" else {})}
        for p in ("fp32", "bf16", "fp16")
    ]


def write(storage: Path, name: str, rows: dict[str, Any], host: dict[str, Any] | None = None) -> Path:
    directory = profiles_dir(storage)
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / profile_filename(name)
    body: dict[str, Any] = {"name": name, "rows": rows}
    if host is not None:
        body["host"] = host
    path.write_text(json.dumps(body), encoding="utf-8")
    return path


def bench(**fields: Any) -> dict[str, Any]:
    return {"pages_per_second": 0.9066, "at": "2026-09-27T18:30:55Z", "host": dict(ON_CARD),
            **fields}


class TestLegacyBenches:
    def test_a_chosen_trial_the_policy_disagrees_with_is_dropped(
        self, tmp_path: Path, caplog: pytest.LogCaptureFixture
    ) -> None:
        """paddle-manga's policy is fp32 on every card; the old phase chose bf16."""
        path = write(tmp_path, LOCAL_PROFILE, {"g-3": {
            "recipe": PADDLE, "bench": bench(precision_trials=legacy_trials("bf16")),
        }})
        store = ProcessorProfiles(tmp_path)

        with caplog.at_level(logging.INFO, logger=profiles_module.__name__):
            row = store.row(LOCAL_PROFILE, "g-3", recipe=PADDLE)
            store.row(LOCAL_PROFILE, "g-3", recipe=PADDLE)

        assert row is not None
        assert row.bench is None
        assert row.stale_bench is True
        said = [r for r in caplog.records if "stale" in r.getMessage()]
        assert len(said) == 1, [r.getMessage() for r in caplog.records]
        assert "bf16" in said[0].getMessage() and "fp32" in said[0].getMessage()

        # Dropped from the file on the next save of that profile.
        store.record_run(LOCAL_PROFILE, "g-1", pages=10, seconds=1.0, congestion=None)
        on_disk = json.loads(path.read_text(encoding="utf-8"))
        assert "bench" not in on_disk["rows"]["g-3"]

    def test_a_chosen_trial_the_policy_agrees_with_is_kept(self, tmp_path: Path) -> None:
        write(tmp_path, "server", {"g-3": {
            "recipe": PADDLE, "bench": bench(precision_trials=legacy_trials("fp32")),
        }})
        row = ProcessorProfiles(tmp_path).row("server", "g-3", recipe=PADDLE)
        assert row is not None and row.bench is not None
        assert row.stale_bench is False

    def test_no_precision_at_all_is_stale_for_a_policy_engine(self, tmp_path: Path) -> None:
        write(tmp_path, "tower", {"g-3": {"recipe": PADDLE, "bench": bench()}})
        row = ProcessorProfiles(tmp_path).row("tower", "g-3", recipe=PADDLE)
        assert row is not None and row.bench is None

    def test_mokuro_is_unaffected(self, tmp_path: Path) -> None:
        write(tmp_path, "tower", {"g-1": {"recipe": MOKURO, "bench": bench()}})
        row = ProcessorProfiles(tmp_path).row("tower", "g-1", recipe=MOKURO)
        assert row is not None and row.bench is not None


class TestRecordedPrecision:
    def test_a_new_bench_at_the_policy_is_kept(self, tmp_path: Path) -> None:
        write(tmp_path, "tower", {"g-3": {"recipe": PADDLE, "bench": bench(precision="fp32")}})
        row = ProcessorProfiles(tmp_path).row("tower", "g-3", recipe=PADDLE)
        assert row is not None and row.bench is not None

    def test_a_new_bench_off_the_policy_is_stale(self, tmp_path: Path) -> None:
        write(tmp_path, "tower", {"g-3": {"recipe": PADDLE, "bench": bench(precision="fp16")}})
        row = ProcessorProfiles(tmp_path).row("tower", "g-3", recipe=PADDLE)
        assert row is not None and row.bench is None

    def test_on_the_cpu_it_is_fp32(self, tmp_path: Path) -> None:
        cpu = {"devices": {"detect": "cpu", "engine": "cpu"}}
        write(tmp_path, "box", {
            "g-2": {"recipe": HAYAI, "bench": bench(precision="fp32", host=cpu)},
            "g-3": {"recipe": PADDLE, "bench": bench(precision="bf16", host=cpu)},
        })
        store = ProcessorProfiles(tmp_path)
        assert store.row("box", "g-2", recipe=HAYAI).bench is not None  # type: ignore[union-attr]
        assert store.row("box", "g-3", recipe=PADDLE).bench is None  # type: ignore[union-attr]


class TestTheRowsMode:
    def test_a_machine_s_old_pin_no_longer_counts(self, tmp_path: Path) -> None:
        write(tmp_path, "tower", {"g-2": {
            "recipe": HAYAI, "pools": {"precision": "fp32"}, "bench": bench(precision="fp16"),
        }})
        row = ProcessorProfiles(tmp_path).row("tower", "g-2", recipe=HAYAI, mode="fp16")
        assert row is not None and row.bench is not None
        assert "precision" not in row.pools

    def test_the_row_s_mode_decides(self, tmp_path: Path) -> None:
        write(tmp_path, "tower", {"g-3": {"recipe": PADDLE, "bench": bench(precision="bf16")}})
        store = ProcessorProfiles(tmp_path)
        forced = store.row("tower", "g-3", recipe=PADDLE, mode="bf16")
        assert forced is not None and forced.bench is not None
        accuracy = store.row("tower", "g-3", recipe=PADDLE, mode="auto-accuracy")
        assert accuracy is not None and accuracy.bench is None

    def test_a_bench_for_another_mode_is_stale(self, tmp_path: Path) -> None:
        write(tmp_path, "tower", {"g-3": {"recipe": PADDLE, "bench": bench(
            precision="fp32", precision_mode="auto-accuracy")}})
        row = ProcessorProfiles(tmp_path).row("tower", "g-3", recipe=PADDLE, mode="fp32")
        assert row is not None and row.bench is None and row.stale_bench


class TestWhatTheDeviceRuns:
    def test_unknown_support_keeps_a_device_dependent_bench(self, tmp_path: Path) -> None:
        """hayai-nova's accuracy is bf16 only where the card runs bf16:
        without the machine's probe the bench cannot be judged, so it stays."""
        write(tmp_path, "tower", {"g-2": {"recipe": HAYAI, "bench": bench(precision="bf16")}},
              host={"gpu": "NVIDIA GeForce RTX 4090", "backend": "cuda"})
        row = ProcessorProfiles(tmp_path).row("tower", "g-2", recipe=HAYAI)
        assert row is not None and row.bench is not None

    def test_unknown_support_keeps_a_legacy_device_dependent_bench(self, tmp_path: Path) -> None:
        write(tmp_path, "server", {"g-2": {
            "recipe": HAYAI, "bench": bench(precision_trials=legacy_trials("fp32")),
        }})
        row = ProcessorProfiles(tmp_path).row("server", "g-2", recipe=HAYAI)
        assert row is not None and row.bench is not None

    @pytest.mark.parametrize(
        ("bf16", "ran", "kept"),
        [(True, "bf16", True), (False, "bf16", False), (False, "fp32", True), (True, "fp32", False)],
    )
    def test_the_registered_probe_decides(
        self, tmp_path: Path, bf16: bool, ran: str, kept: bool
    ) -> None:
        path = write(tmp_path, "box", {"g-2": {"recipe": HAYAI, "bench": bench(precision=ran)}})
        body = json.loads(path.read_text(encoding="utf-8"))
        body["catalog"] = {
            "devices": [{"id": "gpu:0", "label": "GPU 0"}],
            "gpus": [{"index": 0, "formats": {"bf16": bf16, "fp16": True}}],
        }
        path.write_text(json.dumps(body), encoding="utf-8")
        row = ProcessorProfiles(tmp_path).row("box", "g-2", recipe=HAYAI)
        assert row is not None
        assert (row.bench is not None) is kept


class TestWhatReadsIt:
    def test_the_worker_prices_without_a_stale_bench_and_autobenches_again(
        self, tmp_path: Path
    ) -> None:
        from mokuro_bunko.ocr.generations import parse_generation_list
        from mokuro_bunko.ocr.watcher import LOCAL_SLOT, OCRWorker

        (tmp_path / "library").mkdir()
        rows = parse_generation_list([
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "paddle", "engine": "paddle-manga", "detector": "ppocr-manga"},
        ])
        worker = OCRWorker(storage_path=tmp_path, generations=rows,
                           engines_python_path=Path("/nonexistent"))
        paddle = rows[1]
        write(tmp_path, LOCAL_PROFILE, {paddle.id: {
            "recipe": list(paddle.output_affecting()),
            "pools": {"stage_workers": {"detect": 2}},
            "pools_autobench": {},
            "bench": bench(precision_trials=legacy_trials("fp16")),
        }})

        # EFT pricing: no prior from the stale bench.
        assert worker._machine_bench(paddle.id, LOCAL_SLOT, {}) is None
        # Unmeasured on this machine: its autobench measures it again, even
        # though the entry keeps the pools the old benchmark wrote.
        worker.bench_service = object()
        worker.processor.runs_mokuro_cli = lambda _row: False  # type: ignore[method-assign]
        assert worker.autobench_needed(None, paddle) is True
        write(tmp_path, LOCAL_PROFILE, {paddle.id: {
            "recipe": list(paddle.output_affecting()),
            "pools": {"stage_workers": {"detect": 2}},
            "bench": bench(precision="fp32"),
        }})
        assert worker.autobench_needed(None, paddle) is False
