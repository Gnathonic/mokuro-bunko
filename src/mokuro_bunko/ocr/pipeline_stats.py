"""Read the runner's live pipeline numbers, and say what they point at.

``engine_runner.py`` runs one volume through one pipeline: a pool a stage,
a bounded queue between every pair of them. While it runs it republishes
what those pools and queues measured to a JSON file every two seconds
(``pipeline.json``; see ``docs/configuration.md``, "The staged page
pipeline"). That file is the whole readout -- this module is what turns it
into something a person can act on, and it is the only thing between the
runner and the queue page:

    engine_runner.py  ->  pipeline.json  ->  [this]  ->  OCR progress
        ->  .ocr-progress.json  ->  GET /queue/api/status  ->  queue.js

Two jobs, both pure:

* :func:`summarize` compacts a run's numbers to one row a stage -- name,
  device, pool width, the depth of the queue that stage FILLS, and the
  share of its pool's time spent busy, blocked on a full output queue, or
  starved on an empty input queue. Everything the page needs, nothing it
  does not; the full file stays on disk for anyone debugging the runner.
* :func:`pipeline_verdict` reads those rows and names ONE thing to widen,
  or says nothing at all. Saying nothing is the common case early in a run
  and the right answer whenever the numbers do not carry a signal: a
  verdict off ten seconds of a cold pipeline would be worse than silence.

The percentages are shares of a stage's POOL time, not of the wall clock:
the runner sums its seconds over every worker in the pool, so a stage of
two workers each waiting a second reads as two seconds against a wall
clock of one. Dividing by ``workers * elapsed`` is what makes a two-wide
stage and a four-wide one comparable.

Everything here is cumulative from the start of the volume, because that is
what the runner publishes. A stage that was starved for the first minute
and has been saturated since reads as something in between; the queue depth
next to it is the live one.
"""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any

# THE READING ITSELF LIVES IN THE RUNNER. ``summarize`` and ``pipeline_verdict``
# are re-exported from here, unchanged, because the runner needs them too:
# ``engine_runner.py --bench`` follows the same verdict to decide which stage
# to widen, and a second copy of the rule would drift from this one the first
# time either side was tuned. This module keeps what only the SERVER does --
# finding the file the runner published and deciding whether it is still live.
#
# The import is one-way. ``engine_runner`` imports nothing from the package
# (it is executed by path, from the engines environment, with no
# ``mokuro_bunko`` around it), so importing it here cannot cycle and costs
# nothing: its own module-level imports are standard library only.
from mokuro_bunko.ocr.engine_runner import (
    BUSY_PCT,
    MIN_PAGES,
    WAIT_PCT,
    pipeline_verdict,
    summarize,
)

__all__ = [
    "BUSY_PCT",
    "MIN_PAGES",
    "PIPELINE_STATS_FILE",
    "STALE_AFTER_SECONDS",
    "WAIT_PCT",
    "pipeline_stats_path",
    "pipeline_verdict",
    "read_pipeline_stats",
    "summarize",
]

# Must match ``engine_runner.PIPELINE_STATS_FILE``. The two cannot share a
# constant: the runner is executed by path from the engines environment and
# never imports the server package. The server pins the location by passing
# ``--stats-file`` anyway (see ``OCRProcessor._engine_runner_command``), so a
# drift here shows up as a missing readout, never as a wrong one.
PIPELINE_STATS_FILE = "pipeline.json"

# The runner rewrites the file every 2s while it works. Anything older than
# this is from a run that has stopped -- a finished engine, or one that died --
# and showing its numbers against a live progress bar would be a lie.
STALE_AFTER_SECONDS = 30.0

def pipeline_stats_path(output_dir: Path, engine: str) -> Path:
    """Where this engine's runner publishes its pool and queue numbers.

    The runner's own default (``<output>/_detect/<engine>/pipeline.json``),
    named explicitly so the server reads exactly what it told the runner to
    write -- and so a ``MOKURO_OCR_PIPELINE_STATS`` set in the server's
    environment cannot make two concurrent jobs share one file.

    Deliberately NOT under ``--cache-dir``: the progress percentage counts
    the JSON files there, and this one would count as a page.
    """
    return output_dir / "_detect" / engine / PIPELINE_STATS_FILE


def read_pipeline_stats(path: Path, now: float | None = None) -> dict[str, Any] | None:
    """The live summary from ``path``, or None when there is nothing to show.

    None for every ordinary absence -- no file yet, a stale file from a run
    that has stopped, a half-written or malformed one, a run with no stages.
    A readout must never be why a job fails, so nothing here raises.
    """
    try:
        stamp = path.stat().st_mtime
    except OSError:
        return None
    if (now if now is not None else time.time()) - stamp > STALE_AFTER_SECONDS:
        return None
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, UnicodeDecodeError):
        return None
    return summarize(raw)


