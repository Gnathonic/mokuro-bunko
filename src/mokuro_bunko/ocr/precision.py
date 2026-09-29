"""A generation's precision mode, as the library and the admin panel see it.

The policy itself lives in the runner (``engine_runner.PRECISION_POLICY`` and
``resolve_mode``): the runner resolves the mode on its own device at start,
and this server resolves it the same way from what each machine's probe
reported (``devices.GpuDevice.formats``) to decide which machines are
ELIGIBLE for a row and what each would run it at. Nothing here decides a
precision of its own.

A mode is one of ``auto-accuracy`` (the default), ``auto-balanced``,
``auto-speed``, or a forced ``fp32``/``bf16``/``fp16``. A machine whose
device does not support a forced format is not eligible for the row: it is
never offered the row's volumes and never benchmarked for it, and a row that
no connected machine can run is held (:func:`hold_reason`).
"""

from __future__ import annotations

from collections.abc import Callable, Mapping, Sequence
from typing import TYPE_CHECKING, Any

from mokuro_bunko.ocr.engine_runner import (
    BENCHED_PRECISION_MODES,
    DEFAULT_PRECISION_MODE,
    FORCED_PRECISION_MODES,
    PRECISION_ENGINES,
    PRECISION_MODES,
    PRECISIONS,
    STAGE_ENGINE,
    STAGE_MOKURO,
    ModeResolution,
    engine_modes,
    pick_precision,
    resolve_mode,
)

if TYPE_CHECKING:
    from mokuro_bunko.ocr.devices import DeviceCatalog
    from mokuro_bunko.ocr.generations import GenerationSpec

# What the admin panel's select says for each mode.
PRECISION_MODE_LABELS: dict[str, str] = {
    "auto-accuracy": "Auto: accuracy",
    "auto-balanced": "Auto: balanced",
    "auto-speed": "Auto: speed",
    "fp32": "fp32 only",
    "bf16": "bf16 only (cards that support it)",
    "fp16": "fp16 only (GPUs)",
}


def hold_reason(mode: str) -> str:
    """What a row nobody can run says, on every surface that shows it."""
    return f"No connected machine can run {mode}"


def is_forced(mode: str) -> bool:
    return mode in FORCED_PRECISION_MODES


def model_device(row: GenerationSpec, stage_device: Mapping[str, Any] | None) -> str:
    """Where the row's model sits under this placement (``auto`` when not pinned)."""
    key = STAGE_MOKURO if STAGE_MOKURO in row.device_stage_keys else STAGE_ENGINE
    pins = row.pools.stage_device if stage_device is None else stage_device
    return str(pins.get(key) or "auto")


def bench_pick(bench: Mapping[str, Any] | None, mode: str) -> tuple[str | None, str]:
    """A machine's benchmarked pick for a balanced/speed mode, and why.

    Only a benchmark taken FOR this mode with its precision trials counts;
    the pick is the precision it went on to run at.
    """
    if mode not in BENCHED_PRECISION_MODES or not isinstance(bench, Mapping):
        return None, ""
    if bench.get("precision_mode") != mode:
        return None, ""
    trials = bench_trials(bench)
    ran = bench.get("precision")
    if not trials or ran not in dict(trials):
        return None, ""
    picked = pick_precision(trials, [fmt for fmt, _rate in trials])
    why = str(bench.get("precision_why") or (picked[1] if picked else "benchmark"))
    return str(ran), why


def bench_trials(bench: Mapping[str, Any] | None) -> list[tuple[str, float]]:
    """``(format, pages/s)`` of a benchmark's precision trials, in the order run."""
    if not isinstance(bench, Mapping):
        return []
    out: list[tuple[str, float]] = []
    for trial in bench.get("precision_trials") or ():
        if not isinstance(trial, Mapping):
            continue
        fmt, rate = trial.get("precision"), trial.get("pages_per_second")
        if fmt in PRECISIONS and isinstance(rate, (int, float)) and not isinstance(rate, bool):
            out.append((str(fmt), float(rate)))
    return out


def row_resolution(
    row: GenerationSpec,
    catalog: DeviceCatalog,
    stage_device: Mapping[str, Any] | None = None,
    *,
    mode: str | None = None,
    bench: Mapping[str, Any] | None = None,
) -> ModeResolution:
    """What ``mode`` (the row's own by default) runs the row at on one machine."""
    wanted = mode or row.precision
    pick, why = bench_pick(bench, wanted)
    return resolve_mode(
        row.engine,
        wanted,
        catalog.supported_for(model_device(row, stage_device)),
        pick=pick,
        pick_why=why,
    )


def row_refusal(
    row: GenerationSpec,
    catalog: DeviceCatalog,
    stage_device: Mapping[str, Any] | None = None,
) -> str | None:
    """Why one machine cannot run the row's precision mode, or None when it can."""
    if not row.precision_applies:
        return None
    resolved = row_resolution(row, catalog, stage_device)
    if resolved.eligible:
        return None
    return f"it cannot run {row.precision} ({resolved.why})"


def resolution_entry(resolved: ModeResolution, bench: Mapping[str, Any] | None, mode: str) -> dict[str, Any]:
    """One machine x mode for the admin panel's resolution line."""
    entry: dict[str, Any] = {
        "precision": resolved.precision,
        "eligible": resolved.eligible,
        "why": resolved.why,
    }
    if mode in BENCHED_PRECISION_MODES and isinstance(bench, Mapping) and bench.get("precision_mode") == mode:
        trials = bench_trials(bench)
        if trials:
            entry["trials"] = [
                {"precision": fmt, "pages_per_second": rate} for fmt, rate in trials
            ]
    return entry


def precision_on(
    row: GenerationSpec,
    machines: Mapping[str, tuple[DeviceCatalog, Mapping[str, Any] | None, Mapping[str, Any] | None]],
    *,
    unpicked: Callable[[str], str] | None = None,
) -> dict[str, dict[str, dict[str, Any]]]:
    """``{machine: {mode: entry}}``: what EVERY mode comes to on every machine.

    ``machines`` maps a machine to ``(its catalog, its placement of the row,
    its stored benchmark of the row)``. Every mode, so the panel can say what
    a mode would do before it is saved.

    A balanced/speed mode with more than one candidate the machine supports
    also says where its pick stands (``bench``): ``"done"``, or -- without a
    current pick -- what ``unpicked(machine)`` says: ``"pending"`` (it is
    benchmarked before its next volume), ``"off"`` (``ocr.autobench:
    false``) or ``"failed"``.
    """
    out: dict[str, dict[str, dict[str, Any]]] = {}
    for name, (catalog, placement, bench) in machines.items():
        out[name] = {}
        for mode in engine_modes(row.engine):
            resolved = row_resolution(row, catalog, placement, mode=mode, bench=bench)
            entry = resolution_entry(resolved, bench, mode)
            if mode in BENCHED_PRECISION_MODES and len(resolved.usable) > 1:
                if bench_pick(bench, mode)[0] in resolved.usable:
                    entry["bench"] = "done"
                else:
                    entry["bench"] = unpicked(name) if unpicked is not None else "pending"
            out[name][mode] = entry
    return out


def precision_catalog() -> dict[str, Any]:
    """The modes, their labels and the default, for the admin panel's catalog."""
    return {
        "precision_modes": [
            {"id": mode, "label": PRECISION_MODE_LABELS[mode]} for mode in PRECISION_MODES
        ],
        "precision_default": DEFAULT_PRECISION_MODE,
    }


def engine_precision_modes(engine: str) -> list[str]:
    """The modes a row on ``engine`` may be set to ([] when the engine fixes its own)."""
    return list(engine_modes(engine)) if engine in PRECISION_ENGINES else []


__all__: Sequence[str] = (
    "PRECISION_MODE_LABELS",
    "bench_pick",
    "bench_trials",
    "engine_precision_modes",
    "hold_reason",
    "is_forced",
    "model_device",
    "precision_catalog",
    "precision_on",
    "resolution_entry",
    "row_refusal",
    "row_resolution",
)
