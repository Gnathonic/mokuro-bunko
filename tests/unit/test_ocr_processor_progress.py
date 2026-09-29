"""Unit tests for OCR progress metric behavior."""

from __future__ import annotations

import json
import sys
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.generations import parse_generation_list
from mokuro_bunko.ocr.pipeline_stats import pipeline_stats_path
from mokuro_bunko.ocr.processor import OCRProcessor


def test_progress_metrics_finalizing_at_total_pages(tmp_path: Path) -> None:
    """Progress reaches 100% and switches to finalizing when done == total."""
    processor = OCRProcessor(storage_path=tmp_path)
    percent, eta_seconds, status = processor._progress_metrics(
        done=195,
        total_images=195,
        elapsed=120.0,
    )
    assert percent == 100
    assert eta_seconds == 0
    assert status == "finalizing"


def test_progress_metrics_running_below_total_pages(tmp_path: Path) -> None:
    """Progress remains running below completion."""
    processor = OCRProcessor(storage_path=tmp_path)
    percent, eta_seconds, status = processor._progress_metrics(
        done=97,
        total_images=195,
        elapsed=60.0,
    )
    assert percent is not None and 0 < percent < 100
    assert isinstance(eta_seconds, int)
    assert eta_seconds > 0
    assert status == "running"


# -- the pipeline readout ride-along ---------------------------------------
#
# The runner publishes its pool and queue numbers to a JSON file while it
# works; the poll loop that already computes the percentage reads that file
# too, so the queue page can show where the time is going while it is going
# there. These pin the two ends of that wiring.


def _stats_file(path: Path) -> None:
    """A runner's published numbers: detect saturated, layout starved."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "elapsed_seconds": 120.0,
                "items": 40,
                "stages": [
                    {
                        "key": "detect",
                        "name": "detect + CTC read",
                        "device": "cpu",
                        "workers": 1,
                        "items": 40,
                        "busy_seconds": 119.0,
                        "blocked_seconds": 0.0,
                        "starved_seconds": 0.0,
                    },
                    {
                        "key": "layout",
                        "name": "layout + dump",
                        "device": "cpu",
                        "workers": 1,
                        "items": 40,
                        "busy_seconds": 1.0,
                        "blocked_seconds": 0.0,
                        "starved_seconds": 115.0,
                    },
                ],
                "queues": [
                    {"name": "in->detect", "capacity": 3, "mean_depth": 2.9, "max_depth": 3},
                    {"name": "detect->layout", "capacity": 1, "mean_depth": 0.0, "max_depth": 1},
                    {"name": "layout->out", "capacity": 1, "mean_depth": 0.0, "max_depth": 1},
                ],
                "bottleneck": "detect",
            }
        ),
        encoding="utf-8",
    )


def test_runner_command_names_the_stats_file_per_generation(tmp_path: Path) -> None:
    """The server pins where the readout lands rather than inheriting it.

    MOKURO_OCR_PIPELINE_STATS in the server's own environment is inherited
    by every job; without an explicit path, concurrent jobs would overwrite
    each other's readout and the page would show one job's numbers on
    another job's bar.

    Keyed by the generation's ID, not by its engine: two rows may run the
    SAME engine with different detectors, and sharing a directory would have
    them overwriting each other's readout and each other's page cache.
    """
    storage = tmp_path / "storage"
    (storage / "library").mkdir(parents=True)
    fake_python = tmp_path / "engines-python"
    fake_python.write_text("")
    rows = parse_generation_list(
        [
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "nova-ppocr", "engine": "hayai-nova", "detector": "ppocr-manga"},
            {"name": "nova-ctd", "engine": "hayai-nova", "detector": "ctd"},
        ]
    )
    proc = OCRProcessor(
        storage_path=storage, generations=rows, engines_python_path=fake_python
    )
    workspace = storage / ".processing" / "ws"
    extract = workspace / "Vol 01"
    extract.mkdir(parents=True)

    named: list[Path] = []
    caches: list[Path] = []
    for row in rows[1:]:
        cmd = proc._engine_runner_command(row, extract, workspace)
        assert "--stats-file" in cmd
        stats = Path(cmd[cmd.index("--stats-file") + 1])
        cache_dir = Path(cmd[cmd.index("--cache-dir") + 1])
        assert stats == pipeline_stats_path(workspace, row.id)
        assert cache_dir == workspace / "_ocr" / row.id / "Vol 01"
        # Never under --cache-dir: the progress percentage counts the JSON
        # files there, and this one would count as a finished page.
        assert cache_dir not in stats.parents
        named.append(stats)
        caches.append(cache_dir)

    # The two rows share an engine and must still share nothing else.
    assert named[0] != named[1]
    assert caches[0] != caches[1]


def test_progress_carries_the_stage_readout(tmp_path: Path) -> None:
    """A poll picks up what the runner published and hands it on."""
    storage = tmp_path / "storage"
    (storage / "library").mkdir(parents=True)
    seen: list[dict[str, Any]] = []
    proc = OCRProcessor(storage_path=storage, progress_callback=seen.append)
    workspace = tmp_path / "ws"
    workspace.mkdir()
    generation = parse_generation_list(
        [{"name": "ppocr", "engine": "ppocr-manga", "primary": True}]
    )[0]
    stats_path = pipeline_stats_path(workspace, generation.id)
    _stats_file(stats_path)

    result = proc._run_ocr_subprocess(
        [sys.executable, "-c", "pass"],
        tmp_path / "in",
        workspace,
        tmp_path / "run.log",
        total_images=40,
        generation=generation,
        stats_path=stats_path,
    )

    assert result.error is None
    emitted = [entry for entry in seen if "pipeline" in entry]
    assert emitted, "no progress update carried the pipeline readout"
    # Which row the numbers belong to, by name: the queue page has no other
    # way to tell two rows on one engine apart.
    assert emitted[0]["generation"] == "ppocr"
    assert emitted[0]["engine"] == "ppocr-manga"
    pipeline = emitted[0]["pipeline"]
    assert [stage["key"] for stage in pipeline["stages"]] == ["detect", "layout"]
    assert pipeline["bottleneck"] == "detect"
    assert pipeline["verdict"] == "layout starved 96% waiting on detect — widen detect"


def test_progress_omits_the_readout_when_there_is_none(tmp_path: Path) -> None:
    """The mokuro engine has no stages; its progress carries no key at all.

    Omitted, not None: a job with no pipeline must look exactly like one
    from a server that never had this.
    """
    storage = tmp_path / "storage"
    (storage / "library").mkdir(parents=True)
    seen: list[dict[str, Any]] = []
    proc = OCRProcessor(storage_path=storage, progress_callback=seen.append)

    proc._run_ocr_subprocess(
        [sys.executable, "-c", "pass"],
        tmp_path / "in",
        tmp_path,
        tmp_path / "run.log",
        total_images=40,
    )

    assert seen, "the poll loop emitted no progress at all"
    assert all("pipeline" not in entry for entry in seen)
