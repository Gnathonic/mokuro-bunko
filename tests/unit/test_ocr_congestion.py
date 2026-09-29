"""Per-generation congestion history: append, cap, prune, average.

The runner's live numbers are thrown away with the job's workspace; this is
what survives them. Keyed by the row's immutable id, so a rename keeps the
history a rename should not cost.
"""

from __future__ import annotations

import json

from mokuro_bunko.ocr.congestion import (
    RUNS_PER_GENERATION,
    CongestionHistory,
    average_runs,
    build_record,
    congestion_path,
    read_final_stats,
)
from mokuro_bunko.ocr.pipeline_stats import summarize


def raw_stats(
    *,
    elapsed: float = 120.0,
    detect_busy: float = 90.0,
    engine_starved: float = 40.0,
    items: int = 100,
) -> dict:
    """A runner stats file shaped like the real thing (adapter road)."""
    workers = {"detect": 2, "engine": 1, "post": 2}
    return {
        "elapsed_seconds": elapsed,
        "items": items,
        "bottleneck": "detect",
        "stages": [
            {
                "key": "detect",
                "name": "read page + detection",
                "device": "cpu",
                "workers": workers["detect"],
                "items": items,
                "busy_seconds": detect_busy / 100.0 * workers["detect"] * elapsed,
                "blocked_seconds": 0.0,
                "starved_seconds": 0.0,
            },
            {
                "key": "engine",
                "name": "engine read",
                "device": "gpu",
                "workers": workers["engine"],
                "items": items,
                "busy_seconds": 0.5 * workers["engine"] * elapsed,
                "blocked_seconds": 0.0,
                "starved_seconds": engine_starved / 100.0 * workers["engine"] * elapsed,
            },
            {
                "key": "post",
                "name": "assemble + place",
                "device": "cpu",
                "workers": workers["post"],
                "items": items,
                "busy_seconds": 0.2 * workers["post"] * elapsed,
                "blocked_seconds": 0.0,
                "starved_seconds": 0.0,
            },
        ],
        "queues": [
            {
                "name": "detect->engine",
                "capacity": 2,
                "depth": 0,
                "max_depth": 2,
                "mean_depth": 0.4,
            },
            {
                "name": "engine->post",
                "capacity": 1,
                "depth": 0,
                "max_depth": 1,
                "mean_depth": 0.1,
            },
        ],
    }


def record(volume: str = "Series/Vol 01", **kwargs) -> dict:
    summary = summarize(raw_stats(**kwargs))
    assert summary is not None
    return build_record(summary, volume=volume, at=1_790_000_000.0)


class TestRecording:
    def test_a_record_keeps_what_the_average_needs(self) -> None:
        entry = record()
        assert entry["volume"] == "Series/Vol 01"
        assert entry["pages"] == 100
        assert entry["elapsed"] == 120.0
        assert [stage["key"] for stage in entry["stages"]] == ["detect", "engine", "post"]
        assert [queue["name"] for queue in entry["queues"]] == [
            "detect->engine",
            "engine->post",
        ]
        # The per-stage numbers are shares of the stage's POOL time, so a
        # 2-worker stage and a 1-worker stage are comparable.
        detect = entry["stages"][0]
        assert 89 <= detect["busy_pct"] <= 91
        assert detect["workers"] == 2

    def test_reading_a_finished_run_has_no_staleness_rule(self, tmp_path) -> None:
        # `pipeline_stats.read_pipeline_stats` hides a file older than half a
        # minute, which is right for a LIVE readout and wrong for a finished
        # run whose last write is as old as the finishing took.
        stats = tmp_path / "pipeline.json"
        stats.write_text(json.dumps(raw_stats()), encoding="utf-8")
        import os
        import time

        old = time.time() - 600
        os.utime(stats, (old, old))
        assert read_final_stats(stats) is not None

    def test_an_unreadable_stats_file_is_simply_no_data(self, tmp_path) -> None:
        assert read_final_stats(tmp_path / "missing.json") is None
        broken = tmp_path / "broken.json"
        broken.write_text("{not json", encoding="utf-8")
        assert read_final_stats(broken) is None


class TestHistoryFile:
    def test_it_lives_beside_the_other_worker_dotfiles(self, tmp_path) -> None:
        assert congestion_path(tmp_path).name == ".ocr-congestion.json"

    def test_runs_append_under_the_generation_id(self, tmp_path) -> None:
        history = CongestionHistory(tmp_path)
        history.record("g-2", record("Series/Vol 01"))
        history.record("g-2", record("Series/Vol 02"))
        history.record("g-3", record("Series/Vol 01"))
        stored = history.load()
        assert [run["volume"] for run in stored["g-2"]] == ["Series/Vol 01", "Series/Vol 02"]
        assert len(stored["g-3"]) == 1

    def test_only_the_last_few_runs_are_kept(self, tmp_path) -> None:
        history = CongestionHistory(tmp_path)
        for index in range(RUNS_PER_GENERATION + 3):
            history.record("g-1", record(f"Series/Vol {index:02d}"))
        stored = history.load()["g-1"]
        assert len(stored) == RUNS_PER_GENERATION
        # The OLDEST go, not the newest.
        assert stored[-1]["volume"] == f"Series/Vol {RUNS_PER_GENERATION + 2:02d}"

    def test_a_row_that_no_longer_exists_is_pruned_on_write(self, tmp_path) -> None:
        history = CongestionHistory(tmp_path)
        history.record("g-1", record())
        history.record("g-9", record())
        history.record("g-1", record(), known_ids=["g-1", "g-2"])
        assert set(history.load()) == {"g-1"}

    def test_prune_drops_them_without_a_new_run(self, tmp_path) -> None:
        history = CongestionHistory(tmp_path)
        history.record("g-1", record())
        history.record("g-9", record())
        history.prune(["g-1"])
        assert set(history.load()) == {"g-1"}

    def test_the_write_is_atomic_and_leaves_no_temporary_behind(self, tmp_path) -> None:
        history = CongestionHistory(tmp_path)
        history.record("g-1", record())
        assert congestion_path(tmp_path).exists()
        assert not list(tmp_path.glob("*.tmp"))
        # And the file really is JSON a later process can read.
        json.loads(congestion_path(tmp_path).read_text(encoding="utf-8"))

    def test_an_empty_history_removes_the_file(self, tmp_path) -> None:
        history = CongestionHistory(tmp_path)
        history.record("g-1", record())
        history.prune([])
        assert not congestion_path(tmp_path).exists()
        assert history.load() == {}

    def test_a_corrupt_file_reads_as_no_history(self, tmp_path) -> None:
        congestion_path(tmp_path).write_text("{not json", encoding="utf-8")
        assert CongestionHistory(tmp_path).load() == {}


class TestAveraging:
    def test_no_runs_means_no_congestion_object(self) -> None:
        assert average_runs([]) is None
        assert average_runs([{"stages": []}]) is None

    def test_percentages_are_means_over_the_runs(self) -> None:
        summary = average_runs([record(detect_busy=80.0), record(detect_busy=90.0)])
        assert summary is not None
        assert summary["runs"] == 2
        detect = next(row for row in summary["stages"] if row["key"] == "detect")
        assert detect["busy_pct"] == 85

    def test_the_shape_is_the_one_the_http_contract_promises(self) -> None:
        summary = average_runs([record()])
        assert summary is not None
        assert set(summary) == {
            "runs",
            "last_run_at",
            "verdict",
            "bottleneck",
            "stages",
            "queues",
        }
        assert summary["last_run_at"].endswith("Z")
        for stage in summary["stages"]:
            assert set(stage) == {
                "key",
                "workers",
                "busy_pct",
                "starved_pct",
                "blocked_pct",
            }
            for key in ("busy_pct", "starved_pct", "blocked_pct"):
                assert isinstance(stage[key], int)
                assert 0 <= stage[key] <= 100
        for queue in summary["queues"]:
            assert set(queue) == {"name", "capacity", "mean_depth", "max_depth"}

    def test_the_bottleneck_is_the_busiest_stage_that_ran(self) -> None:
        summary = average_runs([record()])
        assert summary is not None
        assert summary["bottleneck"] == "detect"

    def test_the_verdict_is_the_runners_own_reading_of_the_average(self) -> None:
        # A GPU stage starved on a slow detector is the case the docs single
        # out, and the wording must match what the per-run log says.
        summary = average_runs([record(engine_starved=40.0), record(engine_starved=38.0)])
        assert summary is not None
        assert summary["verdict"] is not None
        assert "engine" in summary["verdict"]
        assert "detect" in summary["verdict"]

    def test_a_balanced_pipeline_gets_no_verdict_rather_than_an_invented_one(self) -> None:
        summary = average_runs([record(detect_busy=30.0, engine_starved=2.0)])
        assert summary is not None
        assert summary["verdict"] is None

    def test_a_run_too_short_to_read_supports_no_verdict(self) -> None:
        # "Too short" is too few PAGES through the pipeline, not too few
        # seconds: a 5-second run that read 100 pages is perfectly readable
        # (`pipeline_stats.MIN_PAGES`), and a filling pipeline is not.
        summary = average_runs([record(items=5)])
        assert summary is not None
        assert summary["verdict"] is None

    def test_a_short_run_with_enough_pages_still_gets_its_verdict(self) -> None:
        summary = average_runs([record(elapsed=5.0)])
        assert summary is not None
        assert summary["verdict"] is not None
