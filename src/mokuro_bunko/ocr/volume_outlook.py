"""What OCR one volume is still owed, and when to look again.

The reader asks this through a volume's manifest (``pending`` and
``recheck_after``) and hears it on the ``.cbz`` PUT that queued the volume
(``X-Mokuro-Recheck-After``). Nothing is predicted here: every finishing time
is the queue plan's own (``ocr.eta.plan_queue``, the numbers the queue page
shows), looked up by (series, volume, generation). A job
the plan does not price -- in a failure backoff, held for want of a machine,
of a row nothing has measured -- is still pending, with ``eta: null``.

Only the generation's NAME goes out, never a machine name or an error: the
manifest is read with a volume's download rules, not the admin's.
"""

from __future__ import annotations

import math
from collections.abc import Iterable, Mapping, Sequence
from datetime import datetime
from typing import Any

from mokuro_bunko.ocr.generations import GenerationSpec

#: Seconds added to the earliest finishing time, so a recheck lands after the
#: sidecar has been written rather than on the second it is due.
RECHECK_MARGIN_SECONDS = 10
RECHECK_MIN_SECONDS = 30
RECHECK_MAX_SECONDS = 3600
#: Something is pending but the queue cannot say when: look again in 5 min.
RECHECK_UNPRICED_SECONDS = 300


def pending_entries(
    owed: Sequence[GenerationSpec],
    series: str,
    volume: str,
    planned: Iterable[Mapping[str, Any]] = (),
) -> list[dict[str, Any]]:
    """``[{"kind", "id", "eta"}]`` for each row in ``owed``, in that order.

    ``planned`` are the plan's priced entries -- its running jobs and its
    pending ones alike -- each with ``series``, ``volume``, ``generation_id``
    and ``eta_at``. ``kind`` is ``ocr`` for the primary row, else ``layer``;
    ``id`` is the row's name, which is the layer id its sidecar carries.
    """
    etas: dict[str, str | None] = {}
    for entry in planned:
        if entry.get("series") != series or entry.get("volume") != volume:
            continue
        generation_id = entry.get("generation_id")
        if not isinstance(generation_id, str):
            continue
        eta = entry.get("eta_at")
        # A job listed twice (running and still queued in a stale list) keeps
        # the priced answer.
        if etas.get(generation_id) is None:
            etas[generation_id] = eta if isinstance(eta, str) else None
    return [
        {
            "kind": "ocr" if row.primary else "layer",
            "id": row.name,
            "eta": etas.get(row.id),
        }
        for row in owed
    ]


def recheck_after(pending: Sequence[Mapping[str, Any]], now: float) -> int | None:
    """Whole seconds until the reader should ask again, or None when nothing is pending.

    The earliest non-null ``eta`` plus :data:`RECHECK_MARGIN_SECONDS`,
    clamped to [:data:`RECHECK_MIN_SECONDS`, :data:`RECHECK_MAX_SECONDS`];
    :data:`RECHECK_UNPRICED_SECONDS` when nothing pending is priced.
    """
    if not pending:
        return None
    due: list[float] = []
    for entry in pending:
        eta = entry.get("eta")
        if not isinstance(eta, str):
            continue
        try:
            due.append(datetime.fromisoformat(eta.replace("Z", "+00:00")).timestamp())
        except ValueError:
            continue
    if not due:
        return RECHECK_UNPRICED_SECONDS
    seconds = math.ceil(min(due) - now) + RECHECK_MARGIN_SECONDS
    return max(RECHECK_MIN_SECONDS, min(RECHECK_MAX_SECONDS, seconds))
