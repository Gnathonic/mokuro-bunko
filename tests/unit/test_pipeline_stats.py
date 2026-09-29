"""The pipeline readout: summary, verdict, and what it refuses to say.

The numbers come from the runner's live stats file (`pipeline.json`, written
by `engine_runner.StagePipeline`); this is everything between that file and
the queue page. The shapes here are real ones -- the docstring examples in
`docs/configuration.md` and the `detect=1` run quoted there.
"""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.pipeline_stats import (
    MIN_PAGES,
    STALE_AFTER_SECONDS,
    pipeline_stats_path,
    pipeline_verdict,
    read_pipeline_stats,
    summarize,
)


def stage(
    key: str,
    *,
    device: str = "cpu",
    workers: int = 1,
    items: int = 12,
    busy: float = 0.0,
    blocked: float = 0.0,
    starved: float = 0.0,
    name: str | None = None,
    device_bound: bool = False,
) -> dict[str, Any]:
    return {
        "key": key,
        "name": name or f"{key} stage",
        "device": device,
        "workers": workers,
        "items": items,
        "busy_seconds": busy,
        "blocked_seconds": blocked,
        "starved_seconds": starved,
        "utilisation": 0.0,
        "device_bound": device_bound,
    }


def queue(
    name: str, *, capacity: int = 4, mean_depth: float = 0.0, max_depth: int = 0
) -> dict[str, Any]:
    return {
        "name": name,
        "capacity": capacity,
        "depth": 0,
        "max_depth": max_depth,
        "mean_depth": mean_depth,
        "puts": 12,
        "gets": 12,
        "blocked_seconds": 0.0,
        "blocked_events": 0,
        "starved_seconds": 0.0,
        "starved_events": 0,
        "fill": 0.0,
    }


def raw(stages: list[dict[str, Any]], queues: list[dict[str, Any]], **kw: Any) -> dict[str, Any]:
    return {
        "elapsed_seconds": kw.get("elapsed", 100.0),
        "items": kw.get("items", 12),
        "stages": stages,
        "queues": queues,
        "bottleneck": kw.get("bottleneck"),
    }


# -- the summary -----------------------------------------------------------


def test_summary_is_a_share_of_the_pool_not_the_clock() -> None:
    """Seconds are summed over the pool, so the denominator is pool time.

    Four workers over 100s is 400 worker-seconds: 200s busy is 50%, not 200%.
    That is what makes a two-wide stage and a four-wide one comparable.
    """
    summary = summarize(
        raw(
            [stage("detect", workers=4, busy=200.0, starved=100.0)],
            [queue("in->detect"), queue("detect->out")],
        )
    )
    assert summary is not None
    row = summary["stages"][0]
    assert row["busy_pct"] == 50.0
    assert row["starved_pct"] == 25.0


def test_stage_carries_the_queue_it_fills() -> None:
    """Each row names the queue the stage PUTS into, with depth over capacity."""
    summary = summarize(
        raw(
            [stage("detect", workers=2), stage("engine", device="gpu", workers=1)],
            [
                queue("in->detect", capacity=3),
                queue("detect->engine", capacity=4, mean_depth=1.0, max_depth=4),
                queue("engine->out", capacity=2),
            ],
        )
    )
    assert summary is not None
    detect, engine = summary["stages"]
    assert detect["queue"]["name"] == "detect->engine"
    assert detect["queue"]["capacity"] == 4
    assert detect["queue"]["mean_depth"] == 1.0
    assert detect["queue"]["max_depth"] == 4
    assert detect["queue"]["fill_pct"] == 25.0
    assert engine["queue"]["name"] == "engine->out"
    assert engine["device"] == "gpu"


def test_fused_stage_reports_no_waits_of_its_own() -> None:
    """A width-0 stage shares its leader's queues; its waits are the leader's.

    Reporting them on both rows would read as two stages waiting when one is.
    """
    summary = summarize(
        raw(
            [stage("detect", workers=2, busy=50.0), stage("layout", workers=0, busy=5.0)],
            [queue("in->detect"), queue("detect->out")],
        )
    )
    assert summary is not None
    layout = summary["stages"][1]
    assert layout["fused"] is True
    assert layout["blocked_pct"] is None
    assert layout["starved_pct"] is None
    assert layout["queue"] is None
    # Its own busy time IS its own: every stage meters itself.
    assert layout["busy_pct"] == 5.0


def test_bottleneck_only_survives_when_it_names_a_stage_shown() -> None:
    kept = summarize(raw([stage("detect")], [queue("detect->out")], bottleneck="detect"))
    assert kept is not None and kept["bottleneck"] == "detect"
    dropped = summarize(raw([stage("detect")], [queue("detect->out")], bottleneck="ghost"))
    assert dropped is not None and dropped["bottleneck"] is None
    absent = summarize(raw([stage("detect")], [queue("detect->out")]))
    assert absent is not None and absent["bottleneck"] is None


def test_unreadable_input_summarizes_to_nothing() -> None:
    """Defensive because the file may be from a runner of another version."""
    assert summarize(None) is None
    assert summarize([]) is None
    assert summarize({}) is None
    assert summarize({"stages": []}) is None
    assert summarize({"stages": "detect"}) is None
    assert summarize({"stages": [{"name": "no key"}]}) is None
    # Junk fields become zeroes rather than exceptions.
    odd = summarize({"stages": [{"key": "detect", "workers": "four", "busy_seconds": None}]})
    assert odd is not None
    assert odd["stages"][0]["workers"] == 0
    assert odd["stages"][0]["busy_pct"] == 0.0


def test_percentages_are_clamped() -> None:
    """A stage cannot be busy 300% of its pool, whatever the arithmetic says."""
    summary = summarize(
        raw([stage("detect", workers=1, busy=300.0)], [queue("detect->out")], elapsed=100.0)
    )
    assert summary is not None
    assert summary["stages"][0]["busy_pct"] == 100.0


# -- the verdict -----------------------------------------------------------


def verdict_for(stages: list[dict[str, Any]], queues: list[dict[str, Any]], **kw: Any) -> str | None:
    summary = summarize(raw(stages, queues, **kw))
    assert summary is not None
    return summary["verdict"]


def test_starved_stage_points_at_its_feeder() -> None:
    """The owner's case: the card is idle because nothing keeps it fed."""
    line = verdict_for(
        [
            stage("detect", workers=2, busy=95.0),
            stage("engine", device="gpu", workers=1, busy=60.0, starved=38.0),
            stage("post", workers=2, busy=20.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
    )
    assert line == "engine starved 38% waiting on detect — widen detect"


def test_blocked_stage_points_at_its_drain() -> None:
    """A stage that cannot hand work on is waiting for what comes after it."""
    line = verdict_for(
        [
            stage("detect", workers=2, busy=40.0),
            stage("engine", device="gpu", workers=1, busy=70.0, blocked=22.0),
            stage("post", workers=1, busy=99.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
    )
    assert line == "engine blocked 22% waiting on post — widen post"


def test_the_larger_wait_wins() -> None:
    line = verdict_for(
        [
            stage("detect", workers=1, busy=50.0),
            stage("engine", workers=1, busy=30.0, starved=20.0, blocked=45.0),
            stage("post", workers=1, busy=90.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
    )
    assert line == "engine blocked 45% waiting on post — widen post"


def test_saturated_stage_with_nobody_waiting_names_itself() -> None:
    # 291 worker-seconds of a three-worker pool over 100s of wall clock.
    line = verdict_for(
        [stage("detect", workers=3, busy=291.0), stage("layout", workers=1, busy=2.0)],
        [queue("in->detect"), queue("detect->layout"), queue("layout->out")],
    )
    assert line == "detect busy 97% of 3 workers — widen detect"


def test_first_stage_starving_is_not_a_stage_to_widen() -> None:
    """Nothing feeds the first stage but the volume; no pool widens that."""
    line = verdict_for(
        [stage("detect", workers=2, busy=30.0, starved=60.0), stage("layout", workers=1, busy=5.0)],
        [queue("in->detect"), queue("detect->layout"), queue("layout->out")],
    )
    assert line is None


def test_last_stage_blocked_is_not_a_stage_to_widen() -> None:
    """The last stage's queue drains into the driver, which is not a stage."""
    line = verdict_for(
        [stage("detect", workers=2, busy=30.0), stage("layout", workers=1, busy=5.0, blocked=70.0)],
        [queue("in->detect"), queue("detect->layout"), queue("layout->out")],
    )
    assert line is None


def test_balanced_pipeline_says_nothing() -> None:
    """No stage waiting, none saturated: there is nothing to widen, so silence."""
    line = verdict_for(
        [
            stage("detect", workers=2, busy=60.0, starved=5.0),
            stage("engine", device="gpu", workers=1, busy=70.0, starved=8.0, blocked=4.0),
            stage("post", workers=2, busy=55.0, starved=9.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
    )
    assert line is None


def test_a_filling_pipeline_says_nothing() -> None:
    """Every stage but the first is starved at page one, by construction.

    Gated on PAGES, not on seconds: what makes the counters readable is that
    the fill is over, and a fast run reaches that in a second.
    """
    line = verdict_for(
        [
            stage("detect", workers=2, busy=4.0, items=MIN_PAGES - 1),
            stage("engine", workers=1, busy=0.0, starved=4.0, items=MIN_PAGES - 2),
            stage("post", workers=1, busy=0.0, starved=4.0, items=MIN_PAGES - 3),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
        elapsed=2.0,
    )
    assert line is None


def test_run_with_no_pages_yet_says_nothing() -> None:
    line = verdict_for(
        [stage("detect", workers=2, items=0), stage("layout", workers=1, items=0, starved=90.0)],
        [queue("in->detect"), queue("detect->layout"), queue("layout->out")],
        items=0,
    )
    assert line is None


def test_serial_fallback_says_nothing() -> None:
    """`--cpu-workers 0`: no queues at all, so nothing can be backed up."""
    line = verdict_for([stage("detect", workers=0, busy=99.0)], [])
    assert line is None


def test_verdict_is_callable_on_a_summary_alone() -> None:
    summary = summarize(
        raw(
            [stage("detect", workers=1, busy=99.0), stage("layout", workers=1, busy=2.0)],
            [queue("in->detect"), queue("detect->layout"), queue("layout->out")],
        )
    )
    assert summary is not None
    assert pipeline_verdict(summary) == summary["verdict"]
    assert pipeline_verdict(summary) == "detect busy 99% of 1 worker — widen detect"


# -- reading the file ------------------------------------------------------


def test_path_is_the_runner_default_beside_the_detector_dumps(tmp_path: Path) -> None:
    """Matches `engine_runner`'s own default, and stays out of --cache-dir.

    The progress percentage counts the JSON files under `_ocr`; a stats file
    there would be counted as a finished page.
    """
    path = pipeline_stats_path(tmp_path, "paddle-manga")
    assert path == tmp_path / "_detect" / "paddle-manga" / "pipeline.json"
    assert "_ocr" not in path.parts


def test_reads_a_fresh_file(tmp_path: Path) -> None:
    path = tmp_path / "pipeline.json"
    path.write_text(
        json.dumps(raw([stage("detect", workers=2, busy=100.0)], [queue("detect->out")])),
        encoding="utf-8",
    )
    summary = read_pipeline_stats(path)
    assert summary is not None
    assert summary["stages"][0]["key"] == "detect"
    assert summary["items"] == 12


def test_missing_file_reads_as_nothing(tmp_path: Path) -> None:
    assert read_pipeline_stats(tmp_path / "nope.json") is None


def test_stale_file_reads_as_nothing(tmp_path: Path) -> None:
    """A file from a run that stopped must not be shown against a live bar."""
    path = tmp_path / "pipeline.json"
    path.write_text(json.dumps(raw([stage("detect")], [queue("detect->out")])), encoding="utf-8")
    old = time.time() - STALE_AFTER_SECONDS - 10
    import os

    os.utime(path, (old, old))
    assert read_pipeline_stats(path) is None
    # ...and fresh again when the clock says it is.
    assert read_pipeline_stats(path, now=old + 1.0) is not None


def test_half_written_file_reads_as_nothing(tmp_path: Path) -> None:
    """The runner renames over, but a reader must survive seeing junk anyway."""
    path = tmp_path / "pipeline.json"
    path.write_text('{"stages": [{"key": "det', encoding="utf-8")
    assert read_pipeline_stats(path) is None


def test_directory_in_place_of_a_file_reads_as_nothing(tmp_path: Path) -> None:
    """A readout is never why a job fails: every OSError is just 'no data'."""
    path = tmp_path / "pipeline.json"
    path.mkdir()
    assert read_pipeline_stats(path) is None


# -- waiting propagates: the biggest number is not the culprit -------------
#
# These are the cases that broke the first rule ("largest wait wins"): a
# ripple's far end always waits harder than its source, so the largest number
# names the wrong stage every time it is a chain rather than one stage.


def test_starvation_is_read_from_the_source_end() -> None:
    """A slow detector starves the engine, and the engine starves post harder.

    detect is saturated, engine starved 34%, post starved 73%. The answer is
    `widen detect` -- widening `engine` would leave it just as unfed.
    """
    line = verdict_for(
        [
            stage("detect", workers=2, busy=232.0, blocked=4.0, starved=2.0),
            stage("engine", device="gpu", workers=1, busy=78.0, starved=41.0),
            stage("post", workers=2, busy=60.0, starved=176.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
        elapsed=120.0,
    )
    assert line == "engine starved 34% waiting on detect — widen detect"


def test_blocking_is_read_from_the_sink_end() -> None:
    """A narrow post blocks the engine, and the engine blocks detect harder.

    post is saturated, engine blocked 37%, detect blocked 62%. The answer is
    `widen post` -- widening `engine` would only fill the same full queue.
    """
    line = verdict_for(
        [
            stage("detect", workers=4, busy=120.0, blocked=300.0, starved=20.0),
            stage("engine", device="gpu", workers=1, busy=70.0, blocked=45.0, starved=2.0),
            stage("post", workers=1, busy=118.0, starved=1.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
        elapsed=120.0,
    )
    assert line == "engine blocked 38% waiting on post — widen post"


def test_a_gpu_bound_run_is_reported_as_gpu_bound_not_as_a_fault() -> None:
    """THE commonest healthy shape, and what the old rule got wrong.

    The engine holds the card at 99%: detect is blocked behind it and post is
    starved in front of it, both BY CONSTRUCTION -- post costs 11 ms against
    the engine's 915. Nothing is wrong, and "widen engine" names a knob that
    is clamped to one model on one device. So the line says what is true:
    the card sets the pace.
    """
    line = verdict_for(
        [
            stage("detect", workers=1, busy=40.0, blocked=55.0),
            stage("engine", device="gpu", workers=1, busy=99.0, device_bound=True),
            stage("post", workers=1, busy=30.0, starved=65.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
    )
    assert line == "engine busy 99% on the gpu — it sets the pace and cannot be widened"


def test_a_saturated_pooled_feeder_is_still_named() -> None:
    """Being busy is not what disqualifies a feeder; being unwidenable is.

    Same shape as the GPU-bound run, with a CPU pool in the middle instead of
    a model on a card: here widening it IS the fix, and the verdict says so.
    """
    line = verdict_for(
        [
            stage("detect", workers=1, busy=40.0, blocked=55.0),
            stage("engine", workers=2, busy=198.0),
            stage("post", workers=1, busy=30.0, starved=65.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
    )
    assert line == "post starved 65% waiting on engine — widen engine"


def test_a_fused_stage_is_not_mistaken_for_the_queue_neighbour() -> None:
    """`--stage-workers engine=0` fuses engine onto detect's worker.

    post's feeder is then the detect+engine POOL, not the row above it in the
    list -- which has no queues of its own, so reading the neighbour by index
    found nothing and the readout went silent on a real starvation.
    """
    line = verdict_for(
        [
            stage("detect", workers=2, busy=100.0, blocked=1.0, starved=1.0),
            stage("engine", workers=0, busy=80.0),
            stage("post", workers=2, busy=20.0, starved=150.0),
        ],
        [queue("in->detect"), queue("detect->post"), queue("post->out")],
        elapsed=120.0,
    )
    assert line == "post starved 62% waiting on detect+engine — widen detect+engine"


def test_a_pipeline_starved_end_to_end_names_nothing() -> None:
    """Every stage waiting on the one before it: the volume is the limit.

    No pool widens a disk, so there is no stage to name and the verdict is
    silence rather than a confident lie about the first stage.
    """
    line = verdict_for(
        [
            stage("detect", workers=2, busy=20.0, starved=160.0),
            stage("engine", workers=1, busy=10.0, starved=85.0),
            stage("post", workers=1, busy=5.0, starved=90.0),
        ],
        [queue("in->detect"), queue("detect->engine"), queue("engine->post"), queue("post->out")],
    )
    assert line is None
