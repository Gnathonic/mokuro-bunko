"""``<storage>/processors/<name>.json``: a row's numbers on ONE machine.

``detect x3`` tuned on a 16-core box means nothing on a 48-core one, so the
pools, the devices, the benchmark and the run history are all per
(row, processor) -- not per row. The row's own table stays the default for
a processor that has no entry yet.

This store MODELS nothing. Rates are :class:`~mokuro_bunko.ocr.eta.RateModel`'s
job and stay there; what is kept here is the evidence a particular machine
produced -- volumes finished, pages, seconds, the last few runs' congestion
and the last benchmark -- with a plain cumulative mean beside it for the
admin panel to show. Nothing schedules on that mean.

Every row entry remembers the RECIPE it was measured with (the row's
``output_affecting()`` fields: engine, detector, patch budget). A row whose
recipe changed is, for this store, a different row: its pools were tuned
for another pipeline -- possibly one with other stage names -- and its
benchmark measured another engine, so an entry whose recipe no longer
matches reads as absent and is replaced on the next write. That is the
spec's "a row whose output-affecting fields changed has no profile entry".

Persistent on purpose, unlike the registry: a processor that has been
measured once should not have to be measured again after a reconnect.

The profile is keyed by the processor's STORED name (``clean_processor_name``,
truncated to 64 characters) and by nothing coarser: two names the registry,
the strikes and the holds keep apart never share a file. Two names that only
differ past the 64th character, or two accounts registering the same name,
DO share one -- the registry already folds those into one machine. The file
name is the name itself when that is a plain lowercase file name, and
otherwise a readable part plus a digest of the whole name (``ビースト`` and
``ボックス``, or ``tower 1`` and ``tower/1``, are different machines and get
different files; see :func:`profile_filename`).
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import threading
import time
from collections.abc import Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TypeGuard, cast

from mokuro_bunko.ocr.engine_runner import (
    BENCHED_PRECISION_MODES,
    DEFAULT_PRECISION_MODE,
    PRECISION_ENGINES,
    PRECISION_FP32,
    PRECISIONS,
    normalize_precision_mode,
    resolve_mode,
)
from mokuro_bunko.ocr.throughput import RECENT_VOLUMES

logger = logging.getLogger(__name__)

PROFILES_DIRNAME = "processors"
RUNS_KEPT = 5
_UNSAFE = re.compile(r"[^a-zA-Z0-9._-]+")
# A name that may be its own file name: lowercase, no separator, no dot first.
_PLAIN = re.compile(r"[a-z0-9][a-z0-9._-]*")
_PLAIN_MAX = 64
_READABLE_MAX = 40
_DIGEST_HEX = 16

# The three tables a ``pools`` holds, as `GenerationPools` names them.
POOL_TABLES: tuple[str, ...] = ("stage_workers", "queue_capacity", "stage_device")
# A precision a machine's pools still carry, from before a row's precision
# became ONE mode for every machine (``GenerationSpec.precision``). It is
# ignored wherever it is read -- the row's mode is the only precision setting
# -- and said once per (machine, row) in the log (`ProcessorProfiles.row`).
POOL_PRECISION = "precision"
# A row entry whose pools an AUTOMATIC benchmark wrote says so here (``{}``);
# a person's save of the pools removes it.
POOLS_AUTOBENCH = "pools_autobench"
# (processor, row, precision) already said to be ignored: once each per process.
_IGNORED_LOGGED: set[tuple[str, str, str]] = set()
# (processor, row, bench stamp) already said to be stale: once each per process.
_STALE_LOGGED: set[tuple[str, str, str]] = set()
# processor -> {row: bench stamp} of stale benches to drop from that file the
# next time it is saved (`ProcessorProfiles._update`).
_STALE_PENDING: dict[str, dict[str, str]] = {}
# "Derived here": how a machine's stored width or capacity says so for a stage
# the row's own table PINS. Only ever in a profile -- the runner is told
# "derived" by an absent key (`runner_pools`), and a stored ``0`` is a serial
# width, not a derived one. ``stage_device`` has always had its ``auto``.
POOL_AUTO = "auto"
# This server's OWN hardware ("this server", the worker's ``LOCAL_SLOT``)
# keeps a profile too: a row nobody configured by hand is auto-benchmarked on
# it exactly as on a processor, and what that finds is kept here rather than
# in the user's config. Its key is one no processor can ever be stored under:
# `registry.clean_processor_name` strips every stored name (and its fallback)
# of surrounding whitespace, so no stored name starts with a space -- not
# even ``local``, which a processor may not register as anyway. Its file,
# ``@local.json``, is one no stored name maps to either: a plain name's file
# starts with ``[a-z0-9]`` and a digested one's carries a ``~``. Never listed
# by `ProcessorProfiles.names` (the processors'), always pruned with them.
LOCAL_PROFILE = " local"
LOCAL_PROFILE_FILENAME = "@local.json"
# ONE lock for every store in the process. The worker, the admin API and the
# register handler each hold a `ProcessorProfiles` of their own over the same
# files, and a read-modify-write under a per-instance lock would let a
# finished volume's run and an admin's pools edit race and lose one of them.
_WRITE_LOCK = threading.Lock()


def profiles_dir(storage_path: Path) -> Path:
    return Path(storage_path) / PROFILES_DIRNAME


def profile_filename(name: str) -> str:
    """The file a processor's profile lives in: one per stored name, exactly.

    A plain lowercase name (``tower``, ``box-2``) is its own file name, so
    the directory stays readable. Anything else -- a separator, a space, a
    capital, a name in another script -- would fold into some other
    machine's file if it were only sanitised (``tower 1``, ``tower/1`` and
    ``tower_1`` all read ``tower_1``; every all-kana name read
    ``processor``), so it gets a readable part plus a digest of the WHOLE
    name. The ``~`` between them never appears in a plain name, so the two
    forms cannot meet either; and a plain name has no capitals, so a
    case-insensitive filesystem cannot fold two of them together. Whatever
    it is called, the result cannot leave the directory.

    This server's own profile (`LOCAL_PROFILE`) has a file of its own that no
    stored name can reach.
    """
    if name == LOCAL_PROFILE:
        return LOCAL_PROFILE_FILENAME
    if _PLAIN.fullmatch(name) and len(name) <= _PLAIN_MAX:
        return f"{name}.json"
    readable = _UNSAFE.sub("_", name).strip("._")[:_READABLE_MAX] or "processor"
    digest = hashlib.sha256(name.encode("utf-8", "surrogatepass")).hexdigest()[:_DIGEST_HEX]
    return f"{readable}~{digest}.json"


def recipe_key(recipe: Sequence[Any] | None) -> list[Any] | None:
    """A recipe as it is stored: JSON has lists, not tuples."""
    return None if recipe is None else list(recipe)


def holds_pools(pools: Any) -> TypeGuard[Mapping[str, Any]]:
    """Does a stored ``pools`` say anything about the machine?

    Only when one of its tables names a stage. Three EMPTY tables are no
    opinion -- every stage derived, every model ``auto`` -- and an absent
    ``pools`` already says exactly that while leaving the row's own table in
    charge; stored as a machine's pools, they replaced that table there and
    froze it out of the reach of every later edit of the row (what the
    paddle-manga-animetext auto-benchmarks left on two machines). A machine
    that should run a stage ``auto`` against a row that pins it says so
    explicitly (``stage_device: {detect: auto}``), which is a table that
    names a stage.
    """
    if not isinstance(pools, Mapping):
        return False
    return any(
        isinstance(table, Mapping) and table
        for key, table in pools.items()
        if key != POOL_PRECISION
    )


def machine_pools(
    stored: Mapping[str, Any] | None, own: Mapping[str, Any]
) -> dict[str, Any]:
    """What a machine runs a row with: its stored pools over the row's own
    table, TABLE BY TABLE.

    A table the machine names is the machine's, whole (a key it leaves out is
    derived there, not the row's). A table it leaves EMPTY says nothing, so
    the row's own runs for it -- the rule `holds_pools` applies to all three
    at once, one table at a time. Without it an entry that named only widths
    dropped the row's capacities on that machine for good: what the pre-fix
    auto-benchmark stored for every row it widened, since its capacities were
    never a choice (``best.queue_capacity`` is always ``{}``). A machine that
    should run a table derived where the row pins it says so with `POOL_AUTO`.
    """
    source = stored if isinstance(stored, Mapping) else {}
    out: dict[str, Any] = {}
    for key in POOL_TABLES:
        table = source.get(key)
        if not (isinstance(table, Mapping) and table):
            table = own.get(key)
        out[key] = dict(table) if isinstance(table, Mapping) else {}
    return out


def runner_pools(pools: Mapping[str, Any]) -> dict[str, Any]:
    """``pools`` as the runner reads them: a width or capacity `POOL_AUTO`
    is left out, which is how the runner is told to derive it."""
    out: dict[str, Any] = {key: dict(pools.get(key) or {}) for key in POOL_TABLES}
    for key in ("stage_workers", "queue_capacity"):
        out[key] = {k: v for k, v in out[key].items() if v != POOL_AUTO}
    return out


def bench_precision(bench: Mapping[str, Any]) -> str | None:
    """The precision a stored benchmark's recognizer ran at, or None if it does not say.

    ``bench.precision`` since the runner records it outside ``best``; a
    summary written before that ran at the trial its precision phase marked
    ``chosen``.
    """
    recorded = bench.get("precision")
    if isinstance(recorded, str) and recorded in PRECISIONS:
        return recorded
    trials = bench.get("precision_trials")
    if isinstance(trials, list):
        for trial in trials:
            if isinstance(trial, Mapping) and trial.get("chosen"):
                chosen = trial.get("precision")
                if isinstance(chosen, str) and chosen in PRECISIONS:
                    return chosen
    return None


def recorded_formats(
    bench: Mapping[str, Any] | None, profile: Mapping[str, Any]
) -> frozenset[str] | None:
    """What a machine's device supports, by what it REPORTED, or None: not known.

    The machine's registered catalog (its probe's ``gpus`` formats, where the
    row's model would sit by default); a benchmark whose recognizer ran on
    the CPU says fp32.
    """
    host = bench.get("host") if isinstance(bench, Mapping) else None
    devices = host.get("devices") if isinstance(host, Mapping) else None
    engine = devices.get("engine") if isinstance(devices, Mapping) else None
    if engine == "cpu":
        return frozenset({PRECISION_FP32})
    from mokuro_bunko.ocr.devices import catalog_from_processor

    catalog = profile.get("catalog")
    if not isinstance(catalog, Mapping):
        return None
    return catalog_from_processor(catalog).supported_for("auto")


# Every kind of device a machine that reported nothing could be.
_ANY_DEVICE: tuple[frozenset[str], ...] = (
    frozenset({"fp32"}),
    frozenset({"fp32", "fp16"}),
    frozenset({"fp32", "fp16", "bf16"}),
)


def stale_bench_reason(
    engine: str | None,
    bench: Mapping[str, Any],
    mode: str,
    supported: frozenset[str] | None,
) -> str | None:
    """Why a stored benchmark no longer describes this machine, or None if it does.

    A benchmark's rate is the rate AT the precision it ran at, so it counts
    only while it is what the row's MODE resolves to on this machine now
    (`engine_runner.resolve_mode`), for an engine the mode reaches:

    * a benchmark taken for ANOTHER mode is stale -- changing a row's mode
      re-measures it;
    * a balanced/speed benchmark is stale unless it tried exactly the
      candidates this device supports now (a new card, a new runner);
    * any other mode's is stale when it ran at another format than the
      mode resolves to here.

    One that records no precision at all is stale for such an engine. What
    cannot be judged (a machine that reported nothing) is kept.
    """
    if engine not in PRECISION_ENGINES:
        return None
    ran = bench_precision(bench)
    if ran is None and engine == "mokuro":
        # The served fork recorded no precision before the modes, and ran fp32
        # (half precision was an engine of its own then, now gone).
        ran = PRECISION_FP32
    if ran is None:
        return "it records no precision (measured before the precision modes)"
    measured_for = bench.get("precision_mode")
    if isinstance(measured_for, str) and measured_for:
        try:
            measured_for = normalize_precision_mode(measured_for)
        except ValueError:
            measured_for = None
        if measured_for is not None and measured_for != mode:
            return f"it was measured for {measured_for}; the row asks {mode} now"
    if supported is None:
        # Nobody reported what the device runs: judged only when every device
        # it could be gives the same answer (paddle-manga's accuracy is fp32
        # on all of them; hayai-nova's depends on bf16, so it is kept).
        answers = {
            resolve_mode(engine, mode, device).precision
            for device in _ANY_DEVICE
            if resolve_mode(engine, mode, device).eligible
        }
        if mode in BENCHED_PRECISION_MODES or len(answers) != 1:
            return None
        now = answers.pop()
        if now is not None and ran != now:
            return f"it ran at {ran}; this machine runs {now} now"
        return None
    resolved = resolve_mode(engine, mode, supported)
    if not resolved.eligible:
        return None
    if mode in BENCHED_PRECISION_MODES and len(resolved.usable) > 1:
        from mokuro_bunko.ocr.precision import bench_trials

        tried = {fmt for fmt, _rate in bench_trials(bench)}
        if measured_for != mode or tried != set(resolved.usable):
            return (
                f"its precision trials were {', '.join(sorted(tried)) or 'none'}; "
                f"this machine's candidates for {mode} are {', '.join(resolved.usable)}"
            )
        return None
    if resolved.precision is not None and ran != resolved.precision:
        return f"it ran at {ran}; this machine runs {resolved.precision} now"
    return None


@dataclass
class RowProfile:
    """One (row, processor) pair's measured life.

    ``bench`` is None, and ``stale_bench`` True, when the stored benchmark
    no longer describes the row's precision mode on this machine
    (`stale_bench_reason`): the pair is then unmeasured there.
    """

    pools: dict[str, Any]
    bench: dict[str, Any] | None
    runs: dict[str, Any]
    recipe: list[Any] | None = None
    stale_bench: bool = False


class ProcessorProfiles:
    """Read and write the per-processor profiles, one file each."""

    def __init__(self, storage_path: Path, *, keep_runs: int = RUNS_KEPT) -> None:
        self.storage_path = Path(storage_path)
        self.keep_runs = max(1, keep_runs)
        self._lock = _WRITE_LOCK

    # -- reads -----------------------------------------------------------

    def load(self, name: str) -> dict[str, Any]:
        return self._read(profiles_dir(self.storage_path) / profile_filename(name))

    @staticmethod
    def _read(path: Path) -> dict[str, Any]:
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {}
        return value if isinstance(value, dict) else {}

    def row(
        self,
        name: str,
        generation_id: str,
        *,
        recipe: Sequence[Any] | None = None,
        mode: str | None = None,
        supported: frozenset[str] | None | object = ...,
    ) -> RowProfile | None:
        """This pair's entry, or None -- also None when ``recipe`` is given
        and the entry was measured with a different one.

        ``mode`` is the row's precision mode (the default when not given) and
        ``supported`` what this machine's device computes in (None: not
        known; left out: read from what the machine registered). A benchmark
        that no longer describes the mode there reads as absent
        (`stale_bench_reason`). A precision the machine's pools still carry
        is ignored, and said once."""
        profile = self.load(name)
        rows = profile.get("rows")
        entry = rows.get(generation_id) if isinstance(rows, dict) else None
        if not isinstance(entry, dict):
            return None
        stored = entry.get("recipe")
        if recipe is not None and stored is not None and stored != recipe_key(recipe):
            return None
        pools = entry.get("pools")
        if isinstance(pools, Mapping):
            pools = self._without_precision(name, generation_id, pools)
        bench = dict(entry["bench"]) if isinstance(entry.get("bench"), dict) else None
        stale = False
        if bench is not None:
            engine_source = stored if isinstance(stored, list) else recipe_key(recipe)
            engine = engine_source[0] if engine_source else None
            try:
                wanted = normalize_precision_mode(mode)
            except ValueError:
                wanted = DEFAULT_PRECISION_MODE
            formats = (
                recorded_formats(bench, profile)
                if supported is ...
                else cast("frozenset[str] | None", supported)
            )
            reason = stale_bench_reason(
                engine if isinstance(engine, str) else None, bench, wanted, formats
            )
            if reason is not None:
                self._note_stale(name, generation_id, bench, reason)
                bench, stale = None, True
        return RowProfile(
            pools=dict(pools) if holds_pools(pools) else {},
            bench=bench,
            runs=dict(entry.get("runs") or {}),
            recipe=list(stored) if isinstance(stored, list) else None,
            stale_bench=stale,
        )

    @staticmethod
    def _note_stale(name: str, generation_id: str, bench: Mapping[str, Any], reason: str) -> None:
        """Say a stale benchmark once, and have it dropped at the next save."""
        stamp = str(bench.get("at") or "")
        with _WRITE_LOCK:
            _STALE_PENDING.setdefault(name, {})[generation_id] = stamp
        said = (name, generation_id, stamp)
        if said in _STALE_LOGGED:
            return
        _STALE_LOGGED.add(said)
        logger.info(
            "Ignoring the stale benchmark of %s on %s (%s): %s; it is re-measured "
            "where autobench is on, and dropped from the profile at its next save",
            generation_id,
            "this server" if name == LOCAL_PROFILE else name,
            stamp or "undated",
            reason,
        )

    @staticmethod
    def _without_precision(
        name: str, generation_id: str, pools: Mapping[str, Any]
    ) -> Mapping[str, Any]:
        """``pools`` without a precision pin: the row's mode is the only setting.

        A machine's pools could once carry their own precision (a person's
        pin, or an earlier automatic benchmark's). Old files keep reading
        without error; the pin is ignored and said once per (machine, row).
        """
        precision = pools.get(POOL_PRECISION)
        if POOL_PRECISION not in pools:
            return pools
        said = (name, generation_id, str(precision))
        if isinstance(precision, str) and precision and said not in _IGNORED_LOGGED:
            _IGNORED_LOGGED.add(said)
            logger.info(
                "Ignoring the precision %s saved in %s's pools on %s: a generation's "
                "precision mode applies to every machine now",
                precision,
                generation_id,
                "this server" if name == LOCAL_PROFILE else name,
            )
        return {key: value for key, value in pools.items() if key != POOL_PRECISION}

    def names(self) -> list[str]:
        """Every processor with a profile, by the name it was filed under.

        Read out of each file, never off its file name: a digested file name
        cannot be turned back into the name. A file that does not say whose
        it is -- or is not where that name would file it -- is nobody's.
        This server's own profile (`LOCAL_PROFILE`) is not a processor's and
        is never listed.
        """
        directory = profiles_dir(self.storage_path)
        if not directory.is_dir():
            return []
        found: list[str] = []
        for path in sorted(directory.glob("*.json")):
            name = self._read(path).get("name")
            if (
                isinstance(name, str)
                and name
                and name != LOCAL_PROFILE
                and profile_filename(name) == path.name
            ):
                found.append(name)
        return found

    # -- writes ----------------------------------------------------------

    def claim(self, name: str, account: str) -> bool:
        """Record ``account`` as the owner of ``name``, unless another owns it.

        A machine's name is its identity here -- its pools, benches and the
        hardware its estimates are read against -- so it belongs to the
        processor account that first registered under it. A profile written
        before names had owners is claimed by its next registration. One
        locked read-and-write, so two accounts racing cannot both win.
        """
        taken: list[str] = []

        def edit(profile: dict[str, Any]) -> None:
            owner = profile.get("account")
            if isinstance(owner, str) and owner and owner != account:
                taken.append(owner)
                return
            profile["account"] = account

        self._update(name, edit)
        return not taken

    def set_identity(
        self, name: str, *, host: Mapping[str, Any], catalog: Mapping[str, Any]
    ) -> None:
        """What this machine IS, written when it registers.

        The spec's ``processors/<name>.json`` carries ``host`` and
        ``catalog`` so a benchmark's estimate can read "on tower (RTX 4090)"
        after that machine has gone offline.
        """

        def edit(profile: dict[str, Any]) -> None:
            profile["host"] = dict(host)
            profile["catalog"] = dict(catalog)

        self._update(name, edit)

    def set_pools(
        self,
        name: str,
        generation_id: str,
        pools: Mapping[str, Any],
        *,
        recipe: Sequence[Any] | None = None,
        keep_existing: bool = False,
        autobench: bool = False,
    ) -> None:
        """This pair's pools. ``keep_existing``: only if it has none yet --
        decided under the write lock, so a save that lands in between wins.
        An entry whose tables are all empty has none (`holds_pools`).

        ``autobench``: an automatic benchmark wrote them -- recorded
        (`POOLS_AUTOBENCH`). Any other save is a person's and clears that
        record. A precision is never stored in pools.
        """

        def edit(profile: dict[str, Any]) -> None:
            row = self._row(profile, generation_id, recipe)
            if keep_existing and holds_pools(row.get("pools")):
                return
            row["pools"] = {key: value for key, value in pools.items() if key != POOL_PRECISION}
            if autobench:
                row[POOLS_AUTOBENCH] = {}
            else:
                row.pop(POOLS_AUTOBENCH, None)

        self._update(name, edit)

    def set_bench(
        self,
        name: str,
        generation_id: str,
        bench: Mapping[str, Any],
        *,
        recipe: Sequence[Any] | None = None,
    ) -> None:
        def edit(profile: dict[str, Any]) -> None:
            self._row(profile, generation_id, recipe)["bench"] = dict(bench)

        self._update(name, edit)

    def record_run(
        self,
        name: str,
        generation_id: str,
        *,
        pages: int,
        seconds: float,
        congestion: Mapping[str, Any] | None,
        recipe: Sequence[Any] | None = None,
        contended: bool = False,
    ) -> None:
        """One finished volume's contribution to this pair's evidence.

        A pair that cannot make a rate is simply not one: a readout must
        never be why a finished volume raises.

        A ``contended`` volume -- read while a neighbour loaded the host (F5)
        -- is COUNTED, and nothing more: its pages and seconds describe the
        neighbour as much as this machine, so they never reach
        ``pages_per_second``, the recent throughput or the congestion history.
        """
        if contended:
            def count(profile: dict[str, Any]) -> None:
                runs = self._row(profile, generation_id, recipe).setdefault("runs", {})
                runs["contended"] = int(runs.get("contended") or 0) + 1
                runs["contended_last_at"] = time.time()

            self._update(name, count)
            return
        if pages <= 0 or seconds <= 0:
            return

        def edit(profile: dict[str, Any]) -> None:
            row = self._row(profile, generation_id, recipe)
            runs = row.setdefault("runs", {})
            runs["volumes"] = int(runs.get("volumes") or 0) + 1
            runs["pages"] = int(runs.get("pages") or 0) + int(pages)
            runs["seconds"] = float(runs.get("seconds") or 0.0) + float(seconds)
            # The cumulative mean of the evidence above, for display beside
            # this machine's name. NOT a rate model: that is RateModel's.
            runs["pages_per_second"] = runs["pages"] / runs["seconds"]
            # The newest few volumes on their own, and when the last one
            # finished: the admin panel's "recent throughput" and "last ran".
            at = time.time()
            recent = [r for r in runs.get("recent") or [] if isinstance(r, dict)]
            recent.append({"pages": int(pages), "seconds": float(seconds), "at": at})
            runs["recent"] = recent[-RECENT_VOLUMES:]
            runs["last_at"] = at
            if congestion is not None:
                history = list(runs.get("congestion") or [])
                history.append(dict(congestion))
                runs["congestion"] = history[-self.keep_runs :]

        self._update(name, edit)

    def prune(self, generation_ids: Iterable[str]) -> None:
        """Drop rows that no longer exist, from every processor's profile
        and from this server's own."""
        known = set(generation_ids)

        def edit(profile: dict[str, Any]) -> None:
            rows = profile.get("rows")
            if not isinstance(rows, dict):
                return
            for gone in [key for key in rows if key not in known]:
                del rows[gone]

        for name in [*self.names(), LOCAL_PROFILE]:
            self._update(name, edit, create=False)

    # -- internals -------------------------------------------------------

    @staticmethod
    def _row(
        profile: dict[str, Any], generation_id: str, recipe: Sequence[Any] | None
    ) -> dict[str, Any]:
        rows = profile.setdefault("rows", {})
        entry = rows.get(generation_id)
        wanted = recipe_key(recipe)
        if not isinstance(entry, dict) or (
            wanted is not None and entry.get("recipe") not in (None, wanted)
        ):
            # A new pair, or one whose recipe changed: everything measured
            # for the old recipe is about another pipeline.
            entry = {}
            rows[generation_id] = entry
        if wanted is not None:
            entry["recipe"] = wanted
        return entry

    @staticmethod
    def _drop_stale_benches(name: str, profile: dict[str, Any]) -> None:
        """Remove the benchmarks `row` found stale, if they are still the ones stored.

        Called under the write lock. Matched by the benchmark's ``at``, so a
        benchmark written since (the re-measurement) is never removed.
        """
        pending = _STALE_PENDING.pop(name, None)
        rows = profile.get("rows")
        if not pending or not isinstance(rows, dict):
            return
        for generation_id, stamp in pending.items():
            entry = rows.get(generation_id)
            bench = entry.get("bench") if isinstance(entry, dict) else None
            if isinstance(bench, dict) and str(bench.get("at") or "") == stamp:
                entry.pop("bench", None)  # type: ignore[union-attr]

    def _update(
        self, name: str, edit: Callable[[dict[str, Any]], None], *, create: bool = True
    ) -> None:
        with self._lock:
            directory = profiles_dir(self.storage_path)
            path = directory / profile_filename(name)
            if not create and not path.is_file():
                return
            profile = self._read(path)
            # Always the name this file is FOR: `names()` reads it back.
            profile["name"] = name
            profile.setdefault("rows", {})
            self._drop_stale_benches(name, profile)
            edit(profile)
            try:
                directory.mkdir(parents=True, exist_ok=True)
                tmp = path.with_name(path.name + ".tmp")
                tmp.write_text(json.dumps(profile, indent=2), encoding="utf-8")
                os.replace(tmp, path)
            except OSError as e:
                logger.error("could not write %s: %s", path, e)
