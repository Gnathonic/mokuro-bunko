"""What the queue page is SENT, per display level and per viewer.

`QueueAPI.raw_status` assembles everything the server knows about the queue:
running cards, the planned pending list, failures with their raw errors and
log paths, the processors' hardware labels, pipeline readouts. Almost none of
that is for a visitor, and most of it is noise even for the host. This module
cuts it down to exactly what one page view shows, so a lower level never
carries a field it does not render:

* ``minimal`` -- one compact card per connected machine: a line per lane
  (its active volume and state, percent, finishing time) and the volume on
  deck; the first few pending volumes one compact line each, how many wait
  in all, and when the queue ends;
* ``normal`` -- one card per MACHINE: its active volume(s) with progress and
  state, the volume on deck as a single "Next" line, the pending list with
  finishing times, failures with a generic reason, run order, thumbnails,
  volumes skipped for missing pages;
* ``detailed`` -- normal plus the tuning readouts: each running volume's stage
  pipeline and congestion verdict, its machine's real throughput on that
  layer, its fixed per-volume latency and startup -- and ONE line per layer
  being read with what the machines reading it deliver together.

No level sends a per-machine speed breakdown: that is the admin panel's
Processors card. And every speed any level sends is REAL throughput -- pages
over the wall seconds of finished volumes (`ocr.throughput`) -- never the
ETA model's fitted slope.

The exposure rule holds at every level: raw error messages, log paths,
processor names and hardware labels, the benchmark key and the OCR backend
reach an authenticated ADMIN only. Everyone else sees an alias for each
machine ("machine 1", or the processor's explicit `public_name`) and a
short reason category for a failure. Nested objects are rebuilt field by
field, never passed through.
"""

from __future__ import annotations

import threading
from collections.abc import Callable, Mapping, Sequence
from typing import Any

LEVELS = ("minimal", "normal", "detailed")
DEFAULT_LEVEL = "normal"

# The machine name the worker files this server's own work under.
LOCAL_MACHINE = "local"
LOCAL_DISPLAY_NAME = "this server"

# Generic failure categories: what a visitor is told instead of the error.
# A download that failed on every try is checked FIRST: it is the path's (or a
# proxy's) doing far more often than the archive's, and must never again read
# as "archive incomplete" -- the wording that started the transfer redesign.
REASON_DOWNLOAD = "download failed — will retry"
REASON_ARCHIVE = "archive incomplete"
REASON_INTERRUPTED = "interrupted — will retry"
REASON_ENGINE = "engine error"
REASON_RETRY = "failed — will retry"

_ARCHIVE_WORDS = (
    "zip", "truncated", "missing page", "pages short", "incomplete", "corrupt",
    "no images", "invalid image", "cannot identify image", "archive",
)
_INTERRUPTED_WORDS = (
    "disconnect", "connection", "timed out", "timeout", "went away", "lost",
    "closed before", "stopping",
)
_ENGINE_WORDS = (
    "out of memory", "oom", "cuda", "hip", "rocm", "exited", "exit code",
    "traceback", "engine", "runner", "killed", "signal", "error", "exception",
)


def normalize_level(value: Any) -> str:
    """A configured level, or the default for anything that is not one."""
    return value if isinstance(value, str) and value in LEVELS else DEFAULT_LEVEL


def failure_reason(error: Any) -> str:
    """The short, generic category of a raw failure message."""
    text = str(error or "").lower()
    if text.startswith("download failed"):
        return REASON_DOWNLOAD
    if any(word in text for word in _ARCHIVE_WORDS):
        return REASON_ARCHIVE
    if any(word in text for word in _INTERRUPTED_WORDS):
        return REASON_INTERRUPTED
    if any(word in text for word in _ENGINE_WORDS):
        return REASON_ENGINE
    return REASON_RETRY


def display_name(machine: Any) -> str:
    """A machine's public name: the processor's name, or "this server"."""
    if not isinstance(machine, str) or not machine or machine == LOCAL_MACHINE:
        return LOCAL_DISPLAY_NAME
    return machine


class PublicNames:
    """What a VISITOR calls each machine: "machine 1", "machine 2", ...

    A processor's name defaults to its hostname, which is nobody's business
    but the host's. So a visitor is given an alias instead, stable for the
    life of the process: numbered in the order the machines first registered
    (or were first seen), unless the processor was given an explicit
    `processor.public_name`, which is then used as is. This server is always
    "this server". Admins are sent the real names and never see these.
    """

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._aliases: dict[str, str] = {}
        self._explicit: set[str] = set()
        self._count = 0

    def assign(self, name: str, public_name: str | None = None) -> str:
        """Fix ``name``'s alias now (at registration), explicit or numbered."""
        with self._lock:
            if public_name:
                self._aliases[name] = public_name
                self._explicit.add(name)
            elif name not in self._aliases or name in self._explicit:
                self._explicit.discard(name)
                self._count += 1
                self._aliases[name] = f"machine {self._count}"
            return self._aliases[name]

    def __call__(self, machine: Any) -> str:
        if not isinstance(machine, str) or not machine or machine == LOCAL_MACHINE:
            return LOCAL_DISPLAY_NAME
        with self._lock:
            known = self._aliases.get(machine)
        return known if known is not None else self.assign(machine)


def _machine_of(job: Mapping[str, Any]) -> str:
    machine = job.get("machine")
    return machine if isinstance(machine, str) and machine else LOCAL_MACHINE


# What a machine (and each of its lanes) is doing, for the page's status pill:
# reading pages; opening a session (model loading); holding a volume that has
# not started; connected with nothing to do.
STATE_RUNNING = "running"
STATE_LOADING = "loading"
STATE_WAITING = "waiting"
STATE_IDLE = "idle"
# Connected, but its archive downloads keep failing: the library holds it
# for a while (design section 6.4). Shown only when it has nothing running.
STATE_HELD = "held"
# Connected, with nothing running because it is being benchmarked: an
# automatic benchmark measures a new row (or a new machine) before it runs
# there, and a card that read "Idle" meanwhile looked like a machine the
# library had lost. Shown only when it has nothing running.
STATE_CONFIGURING = "configuring"
# Connected and able to run what is queued, but the earliest-finish scheduler
# is leaving that work to machines that will finish it sooner. Not "Idle": an
# idle card beside a long queue read as a machine the library had forgotten.
# Shown only when it has nothing running, after "configuring" and "held".
STATE_STANDBY = "standby"
_STATE_RANK = {STATE_RUNNING: 0, STATE_LOADING: 1, STATE_WAITING: 2, STATE_IDLE: 3}

# At `minimal` the pending list is a short glance, not the queue.
MINIMAL_PENDING_LIMIT = 10


def job_state(job: Mapping[str, Any]) -> str:
    """One active volume's state: running, loading or waiting.

    From the SESSION, when the card says (``session_ready``, written by the
    watcher from the runner's own `ready` event): loading while it is not
    ready, however far past its estimate the load runs; waiting while it is
    ready but its runner does not have this volume yet (``delivered``: a
    processor still fetching the archive); running once it has, pages out or
    not. Never from the ETA model's remaining startup: that is a guess, and
    it runs out while a slow model is still loading.
    """
    if job.get("status") != "starting":
        return STATE_RUNNING
    ready = job.get("session_ready")
    if ready is False:
        return STATE_LOADING
    if ready is True:
        return STATE_WAITING if job.get("delivered") is False else STATE_RUNNING
    # A card from before the flag: the old reading, from the estimate.
    startup = job.get("startup_seconds")
    if isinstance(startup, (int, float)) and startup > 0:
        return STATE_LOADING
    return STATE_WAITING


def group_by_machine(
    jobs: Sequence[Mapping[str, Any]],
    connected: Sequence[Mapping[str, Any]] = (),
) -> list[dict[str, Any]]:
    """Running cards as machines, each with its ACTIVE volumes and its on-deck ones.

    A session keeps the next volume submitted behind the one it is reading
    (the lookahead). Both have a card, and both carry the same slot; the one
    that started first is the one being read, and the rest are on deck. They
    only ever showed 0% there, so they are not volumes in progress on the
    page: they are the "Next" line of the machine reading them.

    A card with no slot (an older progress file) is its own active volume.
    ``connected`` (``[{"machine", "slots"}]``) comes first, in lane order, so
    every machine that can run OCR has an entry -- an idle one included --
    and keeps its place; machines seen only on a card follow in the order
    their first card started.
    """
    machines: dict[str, dict[str, Any]] = {}
    for row in connected:
        machine = row.get("machine")
        if isinstance(machine, str) and machine and machine not in machines:
            slots = row.get("slots")
            machines[machine] = {
                "machine": machine, "active": [], "next": [], "label": None,
                "slots": slots if isinstance(slots, int) and slots > 0 else 1,
                "held": row.get("held"), "held_error": row.get("held_error"),
                "held_until": row.get("held_until"),
                "configuring": row.get("configuring"),
                "standby": bool(row.get("standby")),
                "cannot_start": row.get("cannot_start") or [],
            }
    lanes: dict[tuple[str, Any], list[Mapping[str, Any]]] = {}
    order: list[tuple[str, Any]] = []
    for index, job in enumerate(jobs):
        machine = _machine_of(job)
        if machine not in machines:
            machines[machine] = {
                "machine": machine, "active": [], "next": [], "label": None, "slots": 1,
            }
        label = job.get("processor")
        if isinstance(label, str) and label and machines[machine]["label"] is None:
            machines[machine]["label"] = label
        slot = job.get("slot")
        key = (machine, slot if isinstance(slot, int) else f"job-{index}")
        if key not in lanes:
            lanes[key] = []
            order.append(key)
        lanes[key].append(job)
    for key in order:
        cards = sorted(
            lanes[key],
            key=lambda job: (
                # A card that has put out pages is being read, whatever its
                # start time says; then the one that started first.
                0 if (job.get("done_pages") or 0) > 0 else 1,
                _as_float(job.get("started_at")) or 0.0,
            ),
        )
        machines[key[0]]["active"].append(cards[0])
        machines[key[0]]["next"].extend(cards[1:])
    for entry in machines.values():
        entry["slots"] = max(entry["slots"], len(entry["active"]))
        states = [job_state(job) for job in entry["active"]]
        if states:
            entry["state"] = min(states, key=_STATE_RANK.__getitem__)
        elif isinstance(entry.get("configuring"), Mapping):
            entry["state"] = STATE_CONFIGURING
        elif entry.get("held"):
            entry["state"] = STATE_HELD
        else:
            entry["state"] = STATE_STANDBY if entry.get("standby") else STATE_IDLE
    return list(machines.values())


def _as_float(value: Any) -> float | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    return None


def _running_jobs(raw: Mapping[str, Any]) -> list[Mapping[str, Any]]:
    jobs = raw.get("current_jobs")
    if isinstance(jobs, list):
        return [job for job in jobs if isinstance(job, Mapping)]
    current = raw.get("current")
    return [current] if isinstance(current, Mapping) else []


def _pick(source: Mapping[str, Any], keys: Sequence[str]) -> dict[str, Any]:
    return {key: source.get(key) for key in keys}


MINIMAL_JOB_KEYS = ("series", "volume", "generation", "percent", "eta_at")
NORMAL_JOB_KEYS = MINIMAL_JOB_KEYS + (
    "status", "done_pages", "total_pages", "eta_seconds", "startup_seconds",
    # The machine is loaded by something else right now (CPU pressure): its
    # progress is slower than the machine is, and says so.
    "host_busy",
)
# No `rate_pages_per_second`: that is the ETA model's rate (a fitted slope,
# the marginal cost of a page), which no display may call a speed. The
# detailed card shows `throughput_pages_per_minute` instead -- see `_speed`.
DETAILED_JOB_KEYS = NORMAL_JOB_KEYS + (
    "engine", "detector", "latency_seconds", "startup_rough", "pipeline",
)
NEXT_KEYS = ("series", "volume", "generation")
MINIMAL_PENDING_KEYS = ("series", "volume", "generation", "eta_at", "rough")
PENDING_KEYS = ("series", "volume", "generation", "eta_at", "rough", "attempts", "reason")
DETAILED_PENDING_KEYS = PENDING_KEYS + (
    "engine", "detector", "pages", "rate_source", "latency_seconds",
)
FAILED_KEYS = ("series", "volume", "generation", "attempts", "reason")
ADMIN_FAILED_KEYS = ("error", "log_file", "last_attempt_at")


def _returned(returned: Any, name: Callable[[Any], str], admin: bool) -> dict[str, Any] | None:
    """A pending job a processor gave back: who, and why -- by exposure rule.

    An admin gets the raw error, its class and the real machine name; a
    visitor, the machine's alias and the generic category.
    """
    if not isinstance(returned, Mapping):
        return None
    out: dict[str, Any] = {
        "machine": name(returned.get("machine")),
        "reason": REASON_DOWNLOAD,
        "at": returned.get("at"),
    }
    if admin:
        out["class"] = returned.get("class")
        out["error"] = returned.get("error")
        out["count"] = returned.get("count")
    return out


def _job(job: Mapping[str, Any], keys: Sequence[str]) -> dict[str, Any]:
    out = _pick(job, keys)
    out["state"] = job_state(job)
    if "percent" in out:
        out["percent"] = out["percent"] or 0
    if "done_pages" in out:
        out["done_pages"] = out["done_pages"] or 0
    if "pipeline" in out:
        out["pipeline"] = _pipeline(out["pipeline"])
    return out


def _hold(hold: Any, name: Callable[[Any], str]) -> dict[str, Any] | None:
    """`processing_hold`, field by field, with the last machine's name mapped."""
    if not isinstance(hold, Mapping):
        return None
    out: dict[str, Any] = {"reason": hold.get("reason"), "since": hold.get("since")}
    last = hold.get("last")
    if isinstance(last, Mapping):
        out["last"] = {
            "name": name(last.get("name")),
            "disconnected_at": last.get("disconnected_at"),
        }
    return out


def _configuring(machine: Mapping[str, Any], admin: bool) -> dict[str, Any] | None:
    """What a machine in STATE_CONFIGURING is measuring, by the exposure rule.

    The row's NAME for a saved row; for an unsaved spec the "generation" is
    its bench key, which only an admin is sent (a visitor gets None).
    """
    if machine.get("state") != STATE_CONFIGURING:
        return None
    line = machine.get("configuring")
    if not isinstance(line, Mapping):
        return None
    key = line.get("key")
    generation = line.get("generation")
    if not admin and (
        not isinstance(key, str) or key.startswith("draft-") or generation == key
    ):
        generation = None
    return {"generation": generation, "auto": bool(line.get("auto"))}


def _paused(paused: Any, name: Callable[[Any], str], admin: bool) -> dict[str, Any] | None:
    """`paused_for_benchmark`, field by field; the bench key is an admin's only."""
    if not isinstance(paused, Mapping):
        return None
    processor = paused.get("processor")
    out: dict[str, Any] = {
        "queued": paused.get("queued"),
        # "local" stays "local": the page words that case itself.
        "processor": processor if processor in (None, "", LOCAL_MACHINE) else name(processor),
    }
    key = paused.get("key")
    generation = paused.get("generation")
    if admin:
        out["key"] = key
        out["generation"] = generation
    elif isinstance(key, str) and not key.startswith("draft-") and generation != key:
        # A saved row's NAME. For a draft the "generation" is the draft's
        # bench key itself, which a visitor is never sent.
        out["generation"] = generation
    return out


def _stage(stage: Mapping[str, Any]) -> dict[str, Any]:
    queue = stage.get("queue")
    return {
        **_pick(stage, (
            "key", "name", "device", "workers", "fused",
            "busy_pct", "blocked_pct", "starved_pct",
        )),
        "queue": (
            _pick(queue, ("name", "capacity", "mean_depth", "max_depth"))
            if isinstance(queue, Mapping)
            else None
        ),
    }


def _pipeline(pipeline: Any) -> dict[str, Any] | None:
    """A running volume's stage readout, rebuilt field by field.

    It is written by whichever machine runs the volume -- a processor
    included -- so nothing it adds beyond the readout's own fields is sent on.
    """
    if not isinstance(pipeline, Mapping):
        return None
    stages = [_stage(s) for s in pipeline.get("stages") or [] if isinstance(s, Mapping)]
    if not stages:
        return None
    return {
        "verdict": pipeline.get("verdict"),
        "bottleneck": pipeline.get("bottleneck"),
        "stages": stages,
    }


SKIPPED_KEYS = ("series", "volume", "missing_pages", "page_count", "generations")


def _machine_rates(raw: Mapping[str, Any]) -> dict[tuple[Any, Any], float]:
    """``{(generation_id, machine): real pages/min}`` out of the raw report."""
    rates: dict[tuple[Any, Any], float] = {}
    for entry in raw.get("speed") or []:
        if not isinstance(entry, Mapping):
            continue
        for m in entry.get("machines") or []:
            value = m.get("pages_per_minute") if isinstance(m, Mapping) else None
            if isinstance(value, (int, float)) and value > 0:
                rates[(entry.get("generation_id"), m.get("machine"))] = float(value)
    return rates


def _speed(raw: Mapping[str, Any]) -> list[dict[str, Any]]:
    """``detailed`` only: ONE line per layer being read right now.

    What the lanes reading it deliver together (real throughput, see the
    module doc) and how many machines those are -- never which machines, and
    never a machine's own number: the per-machine table is the admin panel's.
    """
    out: list[dict[str, Any]] = []
    for entry in raw.get("speed") or []:
        if not isinstance(entry, Mapping):
            continue
        combined = entry.get("combined_pages_per_minute")
        if not isinstance(combined, (int, float)) or combined <= 0:
            continue
        machines = [
            m for m in entry.get("machines") or [] if isinstance(m, Mapping) and m.get("working")
        ]
        out.append(
            {
                "generation": entry.get("generation"),
                "pages_per_minute": combined,
                "machines": len(machines),
            }
        )
    return out


def _held_rows(value: Any) -> list[dict[str, Any]]:
    """``[{"generation", "reason"}]`` for the rows the queue is holding."""
    out: list[dict[str, Any]] = []
    for row in value or []:
        if not isinstance(row, Mapping):
            continue
        generation, reason = row.get("generation"), row.get("reason")
        if isinstance(generation, str) and isinstance(reason, str):
            out.append({"generation": generation, "reason": reason})
    return out


def shape_status(
    raw: Mapping[str, Any],
    level: str,
    *,
    admin: bool,
    public_names: Callable[[Any], str] | None = None,
) -> dict[str, Any]:
    """The payload one viewer is sent at one display level. See the module doc.

    ``public_names`` maps a machine to what a visitor calls it (a
    `PublicNames`); an admin is always sent the real name.
    """
    level = normalize_level(level)
    name: Callable[[Any], str] = display_name if admin else (public_names or PublicNames())
    jobs = _running_jobs(raw)
    connected = [row for row in raw.get("connected_machines") or [] if isinstance(row, Mapping)]
    grouped = group_by_machine(jobs, connected)
    pending = [item for item in raw.get("pending_ocr") or [] if isinstance(item, Mapping)]

    common = {
        "level": level,
        "queue_done_at": raw.get("queue_done_at"),
        "pending_count": len(pending),
        "processing_hold": _hold(raw.get("processing_hold"), name),
        "paused_for_benchmark": _paused(raw.get("paused_for_benchmark"), name, admin),
    }
    if admin:
        # Rows no connected machine can run (a forced precision nobody's card
        # supports), each with its plain reason. An admin's to act on: the
        # row's mode is theirs to change.
        common["held_rows"] = _held_rows(raw.get("held_rows"))

    if level == "minimal":
        return {
            **common,
            "machines": [
                {
                    "name": name(machine["machine"]),
                    "state": machine["state"],
                    "slots": machine["slots"],
                    "jobs": [_job(job, MINIMAL_JOB_KEYS) for job in machine["active"]],
                    # The volume on deck: every card has an on-deck field,
                    # at this level too ("Next: None" when there is none).
                    "next": [_pick(job, NEXT_KEYS) for job in machine["next"]],
                    **(
                        {"configuring": configuring}
                        if (configuring := _configuring(machine, admin)) is not None
                        else {}
                    ),
                }
                for machine in grouped
            ],
            # A glance: the first few, one line each, and the count for the rest.
            "pending": [_pick(item, MINIMAL_PENDING_KEYS) for item in pending[:MINIMAL_PENDING_LIMIT]],
        }

    detailed = level == "detailed"
    job_keys = DETAILED_JOB_KEYS if detailed else NORMAL_JOB_KEYS
    pending_keys = DETAILED_PENDING_KEYS if detailed else PENDING_KEYS
    rates = _machine_rates(raw) if detailed else {}
    machines: list[dict[str, Any]] = []
    for machine in grouped:
        jobs = []
        for job in machine["active"]:
            out = _job(job, job_keys)
            if detailed:
                # What this machine has really delivered on this layer.
                out["throughput_pages_per_minute"] = rates.get(
                    (job.get("generation_id"), machine["machine"])
                )
            jobs.append(out)
        entry: dict[str, Any] = {
            "name": name(machine["machine"]),
            "state": machine["state"],
            "slots": machine["slots"],
            "jobs": jobs,
            "next": [_pick(job, NEXT_KEYS) for job in machine["next"]],
        }
        configuring = _configuring(machine, admin)
        if configuring is not None:
            entry["configuring"] = configuring
        if machine.get("held"):
            entry["held"] = {"reason": "downloads failing", "until": machine.get("held_until")}
            if admin:
                entry["held"]["error"] = machine.get("held_error")
        cannot = [row for row in machine.get("cannot_start") or [] if isinstance(row, Mapping)]
        if cannot:
            # Rows whose runner will not start on this machine, and the next
            # try; the error is an admin's only.
            entry["cannot_start"] = [
                {"generation": row.get("generation"), "until": row.get("until"),
                 **({"error": row.get("error")} if admin else {})}
                for row in cannot
            ]
        if admin:
            # Whose hardware, as the admin panel names it ("tower (RTX 4090)").
            entry["label"] = machine["label"]
        machines.append(entry)

    failed: list[dict[str, Any]] = []
    for item in raw.get("failed") or []:
        if not isinstance(item, Mapping):
            continue
        out = _pick(item, FAILED_KEYS)
        out["attempts"] = out["attempts"] or 1
        out["reason"] = failure_reason(item.get("error"))
        if admin:
            out.update(_pick(item, ADMIN_FAILED_KEYS))
        failed.append(out)

    payload: dict[str, Any] = {
        **common,
        "machines": machines,
        "pending": [
            {
                **_pick(item, pending_keys),
                "attempts": item.get("attempts") or 0,
                "returned": _returned(item.get("returned"), name, admin),
            }
            for item in pending
        ],
        "pending_thumbnails": raw.get("pending_thumbnails") or 0,
        "failed": failed,
        "failed_count": len(failed),
        "skipped_missing_pages": [
            _pick(item, SKIPPED_KEYS)
            for item in raw.get("skipped_missing_pages") or []
            if isinstance(item, Mapping)
        ],
        "generations": [
            {"id": row.get("id"), "name": row.get("name")}
            for row in raw.get("generations") or []
            if isinstance(row, Mapping)
        ],
    }
    if detailed:
        payload["speed"] = _speed(raw)
    if admin:
        payload["backend"] = raw.get("backend")
    return payload
