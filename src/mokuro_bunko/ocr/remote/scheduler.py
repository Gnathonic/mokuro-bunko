"""Which processor is offered which row.

One rule on top of the queue's own, which is unchanged: a row a processor's
catalog cannot run is never offered to it. Everything else -- row order,
round-robin within a row, claim exclusivity, the missing-pages skip -- is ``OCRWorker``'s and stays there.

The warm-session rule (spec section 3 rule 1) needs no code here: an open
session tops itself up with its own row (``OCRWorker.claim_for_session``).
A slot starting a new session is offered rows in plain queue order, never a
row another slot has open ahead of an earlier one. Each session runs its own
runner, so that row would cost this slot a model load all the same, and
jumping order for it let pre-emption and re-opening chase each other so the
earlier row never started.
"""

from __future__ import annotations

from collections.abc import Mapping
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from mokuro_bunko.ocr.generations import GenerationSpec

# A device id a row pins a stage to must be one the processor reports.
# `auto` and `cpu` are universal.
UNIVERSAL_DEVICES = frozenset({"auto", "cpu", ""})


def reported_devices(catalog: Mapping[str, Any]) -> set[str]:
    """The device ids a processor's catalog reports (malformed rows skipped)."""
    return {
        str(entry.get("id"))
        for entry in (catalog.get("devices") or ())
        if isinstance(entry, Mapping)
    }


def unreported_pins(
    catalog: Mapping[str, Any], stage_device: Mapping[str, Any]
) -> list[tuple[str, str]]:
    """``(stage, device)`` for every pin to a card this catalog does not report."""
    known = reported_devices(catalog)
    return [
        (str(stage), str(device))
        for stage, device in sorted((stage_device or {}).items())
        if str(device) not in UNIVERSAL_DEVICES and str(device) not in known
    ]


def catalog_can_run(
    catalog: Mapping[str, Any],
    row: GenerationSpec,
    *,
    stage_device: Mapping[str, Any] | None = None,
) -> str | None:
    """None when this processor can run this row, else the reason it cannot.

    ``stage_device`` is the placement the processor would REALLY run the row
    with -- its own pools when its profile has some (see
    ``OCRWorker._remote_pools``) -- and defaults to the row's own table. The
    gate must check what the session will carry, or the two disagree: a
    valid per-machine override never makes the row runnable, and a pin the
    row does not have is sent to a machine without the card.
    """
    engines = set(catalog.get("engines") or ())
    if row.engine not in engines:
        return f"{row.engine} is not installed on this processor"
    if row.mokuro_env:
        # A mokuro-environment engine detects behind its own serve process,
        # with a model that lives in THAT environment: the processor's
        # `detectors` list is the engines environment's and says nothing
        # about it. What it needs is a mokuro that can be served.
        if not catalog.get("serves_mokuro"):
            return f"{row.engine} needs a mokuro environment this processor has not got"
    else:
        detector = row.effective_detector
        # Checked even when the row's detector is LOCKED: a locked detector
        # is one the engine brings (ppocr-manga), so the row never names it
        # -- and a processor without it installed would fail minutes into a
        # volume instead of simply not being offered the row.
        if detector and detector not in set(catalog.get("detectors") or ()):
            return f"the {detector} detector is not installed on this processor"
    pins = row.pools.stage_device if stage_device is None else stage_device
    for stage, device in unreported_pins(catalog, pins):
        return f"{stage} is pinned to {device}, which this processor does not report"
    # The row's precision mode, judged by what this processor's probe says
    # its card computes in: a forced format it cannot run makes it
    # ineligible (an older processor that reports nothing counts as fp32).
    from mokuro_bunko.ocr.devices import catalog_from_processor
    from mokuro_bunko.ocr.precision import row_refusal

    return row_refusal(row, catalog_from_processor(catalog), pins)
