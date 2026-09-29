"""Average congestion of the last few runs of each generation.

The runner publishes what its pools and queues measured while it works
(``pipeline.json``); ``ocr/pipeline_stats.py`` compacts that into one row a
stage and, when the numbers support one, a sentence naming what to widen.
That readout is LIVE -- it describes the job running now and is thrown away
with the workspace when the job ends.

This module is the durable half. Each COMPLETED job appends its final
readout to ``<storage>/.ocr-congestion.json``, keyed by the generation's
immutable ``id`` (never its name: the owner edits names, and a rename must
not erase a row's accumulated runs). The last
:data:`RUNS_PER_GENERATION` runs of each row are kept; rows that no longer
exist are pruned on the next write.

The admin table shows the AVERAGE of those runs, per stage, as percentages
of that stage's pool time -- not seconds, which are summed over the pool and
are not comparable between a two-wide stage and a four-wide one, nor between
a manga volume and a novel. The verdict sentence is
``pipeline_stats.pipeline_verdict`` applied to the averaged numbers, so the
table and the per-run log say the same thing in the same words.

Two honesty limits the numbers carry and the UI must state:

* a row that reads a whole volume behind its own command line never touches
  the staged pipeline, so it can never have data -- its cell is ``—``, not
  "no data yet". Since the mokuro engines became SERVE PROCESSES that is a
  fallback rather than a kind of engine: a mokuro row measures like any
  other, unless the installed package has no serve module and the row keeps
  the old one-volume path;
* on the adapter road the detector runs as its own whole-volume subprocess
  BEFORE the pipeline starts, so the numbers describe the recognizer half of
  the run only.

Cancelled and failed runs are not recorded: they measured a pipeline that
was stopped, not one that ran.
"""

from __future__ import annotations

import json
import logging
import os
import time
from collections.abc import Iterable, Mapping, Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.pipeline_stats import pipeline_verdict, summarize

logger = logging.getLogger(__name__)

CONGESTION_FILE = ".ocr-congestion.json"

# How many completed runs of one generation are averaged. Few enough that a
# host whose load changed shows it within a working session, many enough that
# one unusually dense volume does not decide the verdict.
RUNS_PER_GENERATION = 5


def congestion_path(storage_path: Path) -> Path:
    """Where the per-generation run history lives."""
    return storage_path / CONGESTION_FILE


def read_final_stats(stats_path: Path) -> dict[str, Any] | None:
    """The run's LAST published numbers, summarized, or None.

    Read once the subprocess has exited and before the workspace is removed.
    Deliberately NOT ``pipeline_stats.read_pipeline_stats``: that one hides a
    file older than half a minute, which is right for a live readout and
    wrong here -- a finished run's final write is exactly as old as the
    finishing took.

    Never raises: a diagnostic readout must not be why a job is recorded as
    failed.
    """
    try:
        raw = json.loads(stats_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return None
    return summarize(raw)


def summarize_event_stats(stats: Any) -> dict[str, Any] | None:
    """The summary behind a session's ``stats``/``volume_done`` numbers.

    A session reports a volume's pool and queue numbers IN the event rather
    than through a file that dies with a workspace, and the two readings a
    runner might send are both accepted:

    * raw counters (``busy_seconds``, ``blocked_seconds``, ``starved_seconds``
      per stage) -- the shape ``pipeline.json`` has always had, which
      :func:`summarize` turns into percentages;
    * an already-summarized object (``busy_pct`` and friends), which is used
      as it is, with the verdict re-derived if it carries none.

    The distinction is made on the STAGE rows, not on the top level, because
    only they differ. None for anything unreadable: a diagnostic readout must
    never be why a finished volume is treated as a failure.
    """
    if not isinstance(stats, Mapping):
        return None
    stages = stats.get("stages")
    if not isinstance(stages, list) or not stages:
        return None
    rows = [row for row in stages if isinstance(row, Mapping)]
    if any("busy_seconds" in row for row in rows) or not any("busy_pct" in row for row in rows):
        return summarize(dict(stats))
    summary = dict(stats)
    summary.setdefault("elapsed_seconds", 0.0)
    summary.setdefault("items", 0)
    if summary.get("verdict") is None:
        summary["verdict"] = pipeline_verdict(summary)
    return summary


def build_record(
    summary: Mapping[str, Any],
    *,
    volume: str,
    at: float | None = None,
    volume_pages: int | None = None,
    volume_seconds: float | None = None,
    volume_first: bool = False,
) -> dict[str, Any]:
    """One stored run: the summary reduced to what the average needs.

    The per-stage rows keep their name, device, width and ``fused`` flag as
    well as the three percentages, because the verdict is re-derived from the
    AVERAGE and needs the same shape the live readout has. The queue each
    stage fills is hoisted to its own list, which is also how the HTTP
    contract shows it.

    ``pages`` and ``elapsed`` are the PIPELINE's own counters -- items that
    went through it, and the window they went through in -- and they are what
    the congestion average has always been built from. They are NOT the
    volume's page count: measured on the served road, a four-page volume
    recorded sixteen items, because an item is a unit of pipeline work and
    not a page.

    So where the caller knows the volume's real length and duration (a
    session's ``volume_done`` says both), they ride ALONGSIDE as
    ``volume_pages`` / ``volume_seconds``, untouched by any stage counting.
    That pair is what `ocr.eta` fits a page rate and a per-volume fixed cost
    from. A record without them falls back to ``pages``/``elapsed``, which is
    all any record written before this carries.
    """
    stages: list[dict[str, Any]] = []
    queues: list[dict[str, Any]] = []
    for stage in summary.get("stages") or []:
        stages.append(
            {
                "key": stage.get("key"),
                "name": stage.get("name"),
                "device": stage.get("device"),
                "workers": int(stage.get("workers") or 0),
                "fused": bool(stage.get("fused")),
                "items": int(stage.get("items") or 0),
                "busy_pct": stage.get("busy_pct"),
                "starved_pct": stage.get("starved_pct"),
                "blocked_pct": stage.get("blocked_pct"),
            }
        )
        queue = stage.get("queue")
        if isinstance(queue, Mapping) and queue.get("name"):
            queues.append(
                {
                    "name": queue.get("name"),
                    "capacity": int(queue.get("capacity") or 0),
                    "mean_depth": float(queue.get("mean_depth") or 0.0),
                    "max_depth": int(queue.get("max_depth") or 0),
                }
            )
    record = {
        "at": float(at if at is not None else time.time()),
        "volume": volume,
        "pages": int(summary.get("items") or 0),
        "elapsed": float(summary.get("elapsed_seconds") or 0.0),
        "verdict": summary.get("verdict"),
        "bottleneck": summary.get("bottleneck"),
        "stages": stages,
        "queues": queues,
    }
    if volume_pages and volume_pages > 0 and volume_seconds and volume_seconds > 0:
        record["volume_pages"] = int(volume_pages)
        record["volume_seconds"] = float(volume_seconds)
        if volume_first:
            # The volume its session opened with: its time carries the
            # pipeline filling behind it, which is a per-SESSION cost and is
            # charged as one. Marked so a per-VOLUME fit can leave it out.
            record["volume_first"] = True
    return record


class CongestionHistory:
    """The ``.ocr-congestion.json`` file: read, append, prune, average.

    Written whole on every append, atomically (tmp + ``os.replace``), exactly
    as ``.ocr-failures.json`` is -- the file is small, it is rewritten once a
    volume rather than once a page, and a reader must never catch it
    half-written. The caller holds its own lock around
    :meth:`record`; nothing here locks.
    """

    def __init__(self, storage_path: Path, keep: int = RUNS_PER_GENERATION) -> None:
        self.path = congestion_path(storage_path)
        self.keep = max(1, int(keep))

    def load(self) -> dict[str, list[dict[str, Any]]]:
        """Every recorded run by generation id (empty when there are none)."""
        try:
            data = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, UnicodeDecodeError):
            return {}
        if not isinstance(data, dict):
            return {}
        out: dict[str, list[dict[str, Any]]] = {}
        for key, runs in data.items():
            if isinstance(runs, list):
                out[str(key)] = [run for run in runs if isinstance(run, dict)]
        return out

    def save(self, history: Mapping[str, Sequence[dict[str, Any]]]) -> None:
        """Persist the history atomically; delete the file when it is empty."""
        try:
            trimmed = {key: list(runs) for key, runs in history.items() if runs}
            if not trimmed:
                if self.path.exists():
                    self.path.unlink()
                return
            tmp = self.path.with_name(self.path.name + ".tmp")
            tmp.write_text(
                json.dumps(trimmed, ensure_ascii=False, indent=2), encoding="utf-8"
            )
            os.replace(tmp, self.path)
        except OSError as e:
            logger.warning("Could not persist OCR congestion history: %s", e)

    def record(
        self,
        generation_id: str,
        record: Mapping[str, Any],
        *,
        known_ids: Iterable[str] | None = None,
    ) -> None:
        """Append one completed run, capped and pruned, in one rewrite.

        ``known_ids`` are the generations that still exist: every other row's
        history is dropped in the same write, so a deleted row's numbers go
        when the row goes and no compaction pass is needed.
        """
        history = self.load()
        runs = list(history.get(generation_id, ()))
        runs.append(dict(record))
        history[generation_id] = runs[-self.keep :]
        if known_ids is not None:
            keep_ids = {str(value) for value in known_ids}
            keep_ids.add(generation_id)
            history = {key: runs for key, runs in history.items() if key in keep_ids}
        self.save(history)

    def prune(self, known_ids: Iterable[str]) -> None:
        """Drop the history of generations that no longer exist."""
        keep_ids = {str(value) for value in known_ids}
        history = self.load()
        pruned = {key: runs for key, runs in history.items() if key in keep_ids}
        if pruned != history:
            self.save(pruned)

    def summary(self, generation_id: str) -> dict[str, Any] | None:
        """The averaged congestion of one generation, or None with no runs."""
        return average_runs(self.load().get(generation_id, ()))


def average_runs(runs: Sequence[Mapping[str, Any]]) -> dict[str, Any] | None:
    """Average recorded runs into the ``congestion`` object the API returns.

    None when there is nothing to average -- the state of every row until its
    first volume finishes, and the permanent state of a row that runs behind
    its own command line.

    Stages are matched by key across runs (a run that lacks a stage simply
    does not vote on it), percentages are means rounded to whole numbers, and
    the bottleneck is the busiest stage that really ran. The verdict comes
    from the same function the live readout uses, applied to the averaged
    rows, so the table cannot say something the log would not.
    """
    usable = [run for run in runs if isinstance(run, Mapping) and run.get("stages")]
    if not usable:
        return None

    order: list[str] = []
    busy: dict[str, list[float]] = {}
    starved: dict[str, list[float]] = {}
    blocked: dict[str, list[float]] = {}
    workers: dict[str, list[float]] = {}
    items: dict[str, list[float]] = {}
    meta: dict[str, dict[str, Any]] = {}
    for run in usable:
        for stage in run.get("stages") or []:
            key = stage.get("key")
            if not isinstance(key, str) or not key:
                continue
            if key not in busy:
                order.append(key)
                busy[key], starved[key], blocked[key] = [], [], []
                workers[key], items[key] = [], []
            meta[key] = {
                "name": stage.get("name") or key,
                "device": stage.get("device") or "cpu",
                "fused": bool(stage.get("fused")),
            }
            _collect(busy[key], stage.get("busy_pct"))
            _collect(starved[key], stage.get("starved_pct"))
            _collect(blocked[key], stage.get("blocked_pct"))
            _collect(workers[key], stage.get("workers"))
            _collect(items[key], stage.get("items"))

    queue_order: list[str] = []
    capacity: dict[str, list[float]] = {}
    mean_depth: dict[str, list[float]] = {}
    max_depth: dict[str, list[float]] = {}
    for run in usable:
        for queue in run.get("queues") or []:
            name = queue.get("name")
            if not isinstance(name, str) or not name:
                continue
            if name not in capacity:
                queue_order.append(name)
                capacity[name], mean_depth[name], max_depth[name] = [], [], []
            _collect(capacity[name], queue.get("capacity"))
            _collect(mean_depth[name], queue.get("mean_depth"))
            _collect(max_depth[name], queue.get("max_depth"))

    queues: list[dict[str, Any]] = [
        {
            "name": name,
            "capacity": _round(capacity[name]),
            "mean_depth": round(_mean(mean_depth[name]), 2),
            "max_depth": _round(max_depth[name]),
        }
        for name in queue_order
    ]
    by_name = {queue["name"]: queue for queue in queues}

    stages = []
    verdict_rows = []
    for key in order:
        width = _round(workers[key])
        row = {
            "key": key,
            "workers": width,
            "busy_pct": _round(busy[key]),
            "starved_pct": _round(starved[key]),
            "blocked_pct": _round(blocked[key]),
        }
        stages.append(row)
        verdict_rows.append(
            {
                **row,
                "name": meta[key]["name"],
                "device": meta[key]["device"],
                "fused": meta[key]["fused"],
                "items": _round(items[key]),
                # The verdict reads a stage's OUTBOUND queue, named
                # ``<stage>-><next>`` by the runner, exactly as the live
                # readout does.
                "queue": next(
                    (queue for name, queue in by_name.items() if name.startswith(f"{key}->")),
                    None,
                ),
            }
        )

    ran = [row for row in verdict_rows if not row["fused"] and row["items"]]
    bottleneck = max(ran, key=lambda row: row["busy_pct"], default=None)
    last = max(float(run.get("at") or 0.0) for run in usable)
    return {
        "runs": len(usable),
        "last_run_at": _iso(last),
        "verdict": pipeline_verdict(
            {
                "elapsed_seconds": _mean([float(run.get("elapsed") or 0.0) for run in usable]),
                "items": _round([float(run.get("pages") or 0) for run in usable]),
                "stages": verdict_rows,
                "bottleneck": bottleneck["key"] if bottleneck else None,
            }
        ),
        "bottleneck": bottleneck["key"] if bottleneck else None,
        "stages": stages,
        "queues": queues,
    }


def _collect(bucket: list[float], value: Any) -> None:
    """Add ``value`` to a bucket when it is a real number (None is a no-vote)."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return
    bucket.append(float(value))


def _mean(values: Sequence[float]) -> float:
    return sum(values) / len(values) if values else 0.0


def _round(values: Sequence[float]) -> int:
    return int(round(_mean(values)))


def _iso(stamp: float) -> str | None:
    """An epoch as the ISO-8601 Z string the API returns, or None at zero."""
    if stamp <= 0:
        return None
    return (
        datetime.fromtimestamp(stamp, tz=timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")
    )
