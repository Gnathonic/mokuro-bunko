"""OCR generations: the configured list of named OCR recipes.

A *generation* is one row of ``ocr.generations``: a named recipe made of an
engine, the detector that feeds it, the patch budget it reads at, and the
per-stage pool sizes it runs with. The background queue's
unit of work is ``(volume, generation)``, not ``(volume, engine)`` -- which
is the whole point: two rows may share an engine and differ only in their
detector, and they must never share a file name, a log, a failure record or
a turn in the round-robin.

Three things are DERIVED from a row and never stored, so that renaming a row
moves all three together:

* its sidecar's file name -- ``<Volume>.mokuro`` for the row flagged
  ``primary``, ``<Volume>.<name>.mokuro`` for every other;
* its road through the runner (``engine_runner.page_road``) and therefore the
  set of stage keys its ``pools`` may name. Never hardcode a stage key: the
  roads are the runner's, read at runtime from ``STAGE_GRAPHS``;
* the detector actually used, which is the engine's own when it has one.

The ``id`` is minted once (``g-1``, ``g-2``, ...), is never shown, and is what
every piece of internal state is keyed by, so that a rename costs nothing
except the file name -- see ``docs/configuration.md``.

**The name is a file-name postfix, and the reader's grammar decides it.** A
postfix outside ``[a-z0-9-]{1,32}`` makes every reader classify the sidecar as
an orphan -- silently, forever, with no error on either side -- so the grammar
here (:data:`GENERATION_NAME_RE`) MATCHES the reader's exactly (32, not a
narrower 24: a 24 cap silently truncated `hayai-nova-animetext-attn` to
`...-att` on a live server, which is invisible corruption on every reader, not
a clean refusal) and is enforced at config load as well as in the admin API.
"""

from __future__ import annotations

import json
import re
from collections.abc import Collection, Iterable, Mapping, Sequence
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any

from mokuro_bunko.ocr.devices import (
    DEVICE_AUTO,
    DEVICE_CPU,
    DeviceCatalog,
    cached_catalog,
    stage_lock_reason,
)
from mokuro_bunko.ocr.engine_runner import (
    DEFAULT_PRECISION_MODE,
    FORCED_PRECISION_MODES,
    PRECISION_ENGINES,
    PRECISION_MODES,
    PRECISIONS,
    STAGE_GRAPHS,
    STAGE_MOKURO,
    engine_formats,
    model_stages,
    normalize_precision_mode,
    page_road,
    parse_device,
)
from mokuro_bunko.ocr.engines import (
    DEFAULT_DETECTOR,
    DEFAULT_PATCH_BUDGET,
    DISABLED_DETECTORS,
    ENGINE_IDS,
    MOKURO_ENGINE,
    OFFERED_DETECTOR_IDS,
    get_detector,
    get_engine,
    get_patch_budget,
)

# The postfix a generation's sidecar is named with, as the READER parses it
# (``LAYER_ID_RE`` in mokuro-reader's ``src/lib/util/sync/syncable-file.ts``).
# Files whose postfix fails this are not layers to any device; the delete
# cascade still sweeps them with their archive, because they were written
# beside it whatever produced them.
LAYER_ID_RE = re.compile(r"^[a-z0-9-]{1,32}\Z")

# What a generation may be NAMED, matching what the reader will read
# (`LAYER_ID_RE`, mokuro-reader `src/lib/util/sync/syncable-file.ts:108`).
# Lowercase ASCII and digits, hyphens inside, at most 32 characters:
#   * no dot -- the reader splits the postfix on the LAST dot;
#   * no uppercase -- the reader lowercases the parsed id, so two names that
#     differ only in case would collapse into one layer;
#   * ASCII only -- macOS writes file names in NFD, and a decomposed postfix
#     would never match its own config entry again;
#   * no leading hyphen -- so a name cannot read as a flag on a command line
#     (a trailing one is legal: the grammar published to the UI is exactly
#     this pattern, and the seed rule strips one anyway);
#   * 32, exactly the reader's own cap -- a narrower 24 silently truncated
#     `hayai-nova-animetext-attn` to `...-att` on a live server: invisible
#     corruption on every reader, not a clean refusal.
GENERATION_NAME_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,31}$")
MAX_GENERATION_NAME = 32

# Names the reader already owns. ``original`` is the editor's read-only
# pre-edit snapshot, ``gcv`` is its own Cloud Vision layer; a ``tr-`` prefix
# makes the reader file the layer as a TRANSLATION by id, whatever anything
# else says, which makes it permanently unpromotable.
RESERVED_NAMES: tuple[str, ...] = ("original", "gcv")
RESERVED_PREFIXES: tuple[str, ...] = ("tr-",)

# What an ``id`` may look like. Minted as ``g-<n>``; a hand-written one is
# accepted as long as it is unique and safe as a directory name, because it
# names this row's workspace cache and detector dumps.
GENERATION_ID_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,31}\Z")

# Bounds on a hand-set pool size. The runner clamps a width to the stage's own
# structural ceiling anyway; these only keep a typo from asking for a thousand
# threads or a queue that holds the whole volume in memory.
MAX_STAGE_WORKERS = 64
MAX_QUEUE_CAPACITY = 256

POOL_KEYS: tuple[str, ...] = ("stage_workers", "queue_capacity", "stage_device", "precision")


class GenerationConfigError(ValueError):
    """An ``ocr.generations`` row that cannot be used, and why.

    ``row`` is the row's INDEX in the list (0-based, so a caller can point at
    ``generations[row]``) or None for an error about the list as a whole;
    ``field`` names the stored field at fault, or None. The message repeats
    both in words, because it is also what a server prints on start.
    """

    def __init__(self, message: str, *, row: int | None = None, field: str | None = None) -> None:
        super().__init__(message)
        self.row = row
        self.field = field


@dataclass(frozen=True)
class GenerationPools:
    """Per-stage tuning of one row: where it runs, how wide, how deep between.

    All three map a stage key of this row's road to a setting; an empty map
    means "derive it" (``engine_runner.stage_widths`` / ``stage_capacities`` /
    ``road_specs``), which is what every row does until someone tunes it. None
    of them changes what is written -- the sidecar is identical at every width
    and on either device -- which is why a pools-only edit never cancels a
    running job.

    ``stage_device`` may only name a MODEL-BEARING stage (``detect``,
    ``engine``; the monolithic row's one stage ``mokuro``), because a device is
    a place to put a model and ``post``/``layout`` have none. Its values are
    catalog device ids: ``cpu`` or ``gpu:<n>``, with an absent key meaning
    ``auto``.

    There is no precision here any more: a row's precision is its MODE
    (``GenerationSpec.precision``), the same on every machine. A legacy
    ``pools.precision`` is read as that mode (see ``_parse_precision``).
    """

    stage_workers: Mapping[str, int] = field(default_factory=dict)
    queue_capacity: Mapping[str, int] = field(default_factory=dict)
    stage_device: Mapping[str, str] = field(default_factory=dict)

    def is_empty(self) -> bool:
        return not self.stage_workers and not self.queue_capacity and not self.stage_device

    def to_dict(self) -> dict[str, Any]:
        data: dict[str, Any] = {
            "stage_workers": {k: int(v) for k, v in sorted(self.stage_workers.items())},
            "queue_capacity": {k: int(v) for k, v in sorted(self.queue_capacity.items())},
            "stage_device": {k: str(v) for k, v in sorted(self.stage_device.items())},
        }
        return data


@dataclass(frozen=True)
class GenerationSpec:
    """One configured OCR recipe.

    Frozen on purpose: a claimed job carries the row it was claimed with, so
    that an edit landing mid-run cannot move the file the finished volume is
    collected under (see ``OCRProcessor.process_library_ocr``).
    """

    id: str
    name: str
    engine: str
    primary: bool = False
    enabled: bool = True
    # None when the engine brings its own detector, or is monolithic.
    detector: str | None = None
    patch_budget: int = DEFAULT_PATCH_BUDGET
    pools: GenerationPools = field(default_factory=GenerationPools)
    # The precision MODE (``engine_runner.PRECISION_MODES``), for every machine
    # that runs the row: auto-accuracy (the default), auto-balanced,
    # auto-speed, or a forced fp32/bf16/fp16. Always the default for an
    # engine that fixes its own precision (``precision_applies`` False).
    precision: str = DEFAULT_PRECISION_MODE
    # What ONE machine's benchmark picked for a balanced/speed mode, and why:
    # set only on the copy of the row that is turned into that machine's
    # command line (``OCRWorker._local_run_row``, ``_remote_row_spec``),
    # never stored in the config.
    precision_pick: str | None = None
    precision_why: str = ""

    # --- derived, never stored ------------------------------------------

    @property
    def monolithic(self) -> bool:
        """True when the engine reads a whole volume behind its own CLI.

        Such a row has no staged pipeline: no stages, no pools, and never any
        congestion numbers. It is a property of the ENGINE, not of the host --
        a served engine whose installed package turns out to have no serve
        module falls back to the same CLI, but that is a runtime fact the
        server discovers (``OCRProcessor.serves_pages``) and not what this
        row IS.
        """
        return self.road is None

    @property
    def served(self) -> bool:
        """True when this row's engine is a process pages are streamed into."""
        return get_engine(self.engine).serve_module is not None

    @property
    def mokuro_env(self) -> bool:
        """True when this row's engine lives in the mokuro environment.

        WHICH ENVIRONMENT, not which road: a served mokuro row runs through
        the runner like every other row, but the packages it needs are still
        the mokuro venv's and never the engines venv's. Anything deciding what
        to install, or which rows an install failure takes down with it, asks
        this and not ``monolithic``.
        """
        return get_engine(self.engine).uses_mokuro_env

    @property
    def detector_locked(self) -> bool:
        """True when this row's detector is not the operator's to choose."""
        spec = get_engine(self.engine)
        return spec.uses_mokuro_env or spec.detector is not None

    @property
    def effective_detector(self) -> str:
        """The detector this row really reads pages with.

        The engine's own when it has one (the two models were trained as a
        pair); else the configured one. A mokuro-environment engine detects
        behind its own command line, served or not, and this is only what its
        environment would need installed.
        """
        spec = get_engine(self.engine)
        if spec.detector is not None:
            return spec.detector
        return self.detector or DEFAULT_DETECTOR

    @property
    def reported_detector(self) -> str | None:
        """The detector to SHOW for this row, or None when it has none of ours.

        A row whose engine detects behind its own command line -- the mokuro
        CLI, or the serve process that replaced it -- reads pages with a
        detector this server never names, so naming one of ours anywhere it is
        reported would be a guess. ``effective_detector`` stays what its
        ENVIRONMENT would need installed.
        """
        if self.monolithic or self.served:
            return None
        return self.effective_detector

    @property
    def patch_budget_applies(self) -> bool:
        """True when ``patch_budget`` reaches this row's recognizer."""
        return get_engine(self.engine).patch_budget

    @property
    def precision_applies(self) -> bool:
        """True when the row's precision mode reaches its engine."""
        return self.engine in PRECISION_ENGINES

    @property
    def sidecar_suffix(self) -> str:
        """The file name postfix this row writes: ``.mokuro`` when primary."""
        if self.primary:
            return ".mokuro"
        return f".{self.name}.mokuro"

    @property
    def road(self) -> str | None:
        """This row's road through the runner, or None for a monolithic one.

        An engine that FIXES its road says so (the served ones do: they are a
        process of their own whatever detector a row names); for everything
        else the engine/detector pair decides. An engine in the mokuro
        environment with neither is the one-volume CLI and has no road at all.
        """
        spec = get_engine(self.engine)
        if spec.road is not None:
            return spec.road
        if spec.uses_mokuro_env:
            return None
        return page_road(self.engine, self.effective_detector)

    @property
    def stage_keys(self) -> tuple[str, ...]:
        """The stage keys of this row's road, in order; empty when monolithic.

        Read from the runner's own ``STAGE_GRAPHS`` every time. The keys are
        the runner's to name and have been renamed before.
        """
        road = self.road
        if road is None:
            return ()
        return tuple(spec.key for spec in STAGE_GRAPHS[road])

    @property
    def pool_stage_keys(self) -> tuple[str, ...]:
        """The stages this row's pools table has a line for.

        The road's stages, or -- for a monolithic row, which has no road --
        the single ``mokuro`` stage that IS the fork's page pipeline: one
        model on one device, with the fork's own worker pool
        (``--num_workers``) as its width.
        """
        road = self.road
        if road is None:
            return (STAGE_MOKURO,)
        return tuple(spec.key for spec in STAGE_GRAPHS[road])

    @property
    def device_stage_keys(self) -> tuple[str, ...]:
        """The stages of this row a DEVICE may be chosen for, in order.

        The model-bearing ones and no others: ``post``/``layout`` are assembly
        and JSON, with no model to place.
        """
        road = self.road
        if road is None:
            return (STAGE_MOKURO,)
        return model_stages(road)

    def sidecar_paths(self, cbz_path: Path) -> tuple[Path, Path]:
        """(plain, gzip) sidecar paths of this row for a CBZ.

        The gzip variant is only ever written by the mokuro CLI, but it counts
        for every row so a hand-compressed file still marks the work done.
        """
        plain = Path(f"{cbz_path.with_suffix('')}{self.sidecar_suffix}")
        return plain, Path(f"{plain}.gz")

    def output_affecting(self) -> tuple[Any, ...]:
        """What must not change under a running job of this row.

        The recipe, not the file name: a rename or a ``primary`` flip only
        moves where the finished sidecar lands, and the job carries the row it
        started with, so those are deferred rather than cancelled. Pool sizes
        are excluded deliberately -- they provably never change what is
        written.

        Each field is taken as it REACHES this row's runner, not as it is
        stored, so that editing a setting this row cannot use does not kill
        its job for nothing: the detector of an engine that brings its own,
        the patch budget of a recognizer without the knob.
        """
        spec = get_engine(self.engine)
        return (
            self.engine,
            self.effective_detector,
            self.patch_budget if spec.patch_budget else None,
        )

    def to_dict(self) -> dict[str, Any]:
        """The row as it is stored in ``config.yaml`` and sent over HTTP."""
        data: dict[str, Any] = {
            "id": self.id,
            "name": self.name,
            "primary": self.primary,
            "enabled": self.enabled,
            "engine": self.engine,
        }
        if self.detector is not None:
            data["detector"] = self.detector
        data["patch_budget"] = self.patch_budget
        if self.precision != DEFAULT_PRECISION_MODE:
            data["precision"] = self.precision
        data["pools"] = self.pools.to_dict()
        if self.precision_pick is not None:
            data["precision_pick"] = self.precision_pick
            data["precision_why"] = self.precision_why
        return data


DEFAULT_GENERATION = GenerationSpec(
    id="g-1",
    name=MOKURO_ENGINE,
    engine=MOKURO_ENGINE,
    primary=True,
    enabled=True,
)


def default_generations() -> list[GenerationSpec]:
    """The list an unset ``ocr.generations`` means: mokuro alone, primary."""
    return [DEFAULT_GENERATION]


# --- naming ------------------------------------------------------------


def name_rejection(name: str) -> str | None:
    """Why ``name`` cannot be a generation name, or None when it can.

    One sentence a person can act on, because it is shown verbatim by the
    admin API and printed by a server that refuses to start.
    """
    if not name:
        return "a name is required (it is this row's label and its file-name postfix)"
    # fullmatch: `$` alone would pass a trailing newline. The pattern keeps
    # `$` because the admin UI builds a JavaScript RegExp from it.
    if not GENERATION_NAME_RE.fullmatch(name):
        return (
            f"name {name!r} cannot be a file-name postfix: use lowercase letters, digits "
            f"and hyphens only, {MAX_GENERATION_NAME} characters at most, starting with a "
            "letter or a digit (a name outside this is invisible to every reader)"
        )
    if name in RESERVED_NAMES:
        return f"name {name!r} is reserved by the reader for its own layer of that name"
    for prefix in RESERVED_PREFIXES:
        if name.startswith(prefix):
            return (
                f"name {name!r} is reserved: the reader files any layer starting with "
                f"{prefix!r} as a translation, whatever produced it"
            )
    return None


def seed_generation_name(engine: str, detector: str | None, taken: Iterable[str]) -> str:
    """The name a NEW row gets: the engine, or ``<engine>-<detector>``.

    Evaluated once, when the row is created, and then stored. It is never a
    function of the current list: pending work is decided by which sidecar
    files exist, so a name that moved would un-claim every file under the old
    one and re-OCR the library.

    The engine id alone when the engine brings its own detector or is
    monolithic, because the detector then says nothing; otherwise engine and
    detector joined, truncated to the grammar's 32 characters, with ``-2``,
    ``-3``, ... appended (truncating the stem to fit) on a collision.
    """
    used = {str(name) for name in taken}
    spec = get_engine(engine)
    if spec.uses_mokuro_env or spec.detector is not None or not detector:
        stem = engine
    else:
        stem = f"{engine}-{detector}"
    stem = _trim_name(stem)
    candidate = stem
    counter = 1
    while candidate in used or name_rejection(candidate) is not None:
        counter += 1
        tail = f"-{counter}"
        candidate = _trim_name(stem[: MAX_GENERATION_NAME - len(tail)]) + tail
    return candidate


def _trim_name(stem: str) -> str:
    """``stem`` cut to the grammar's length and stripped of a trailing hyphen."""
    return stem[:MAX_GENERATION_NAME].rstrip("-")


def mint_generation_id(taken: Iterable[str]) -> str:
    """The next free ``g-<n>`` id."""
    used = {str(value) for value in taken}
    highest = 0
    for value in used:
        if value.startswith("g-") and value[2:].isdigit():
            highest = max(highest, int(value[2:]))
    candidate = f"g-{highest + 1}"
    while candidate in used:
        highest += 1
        candidate = f"g-{highest + 1}"
    return candidate


# --- parsing and validation --------------------------------------------


def parse_generation_list(
    value: object, *, devices: DeviceCatalog | None = None
) -> list[GenerationSpec]:
    """Validate a configured ``ocr.generations`` value into rows.

    Accepts the parsed YAML/JSON list of mappings, a JSON string (what
    ``MOKURO_OCR_GENERATIONS`` and ``config set`` take), or a list of already
    built :class:`GenerationSpec`. An empty or absent value is one mokuro row.

    Every rule is enforced here, not only in the admin API, because a
    hand-edited config file and an environment variable both bypass the API
    entirely. Errors carry the row index and the field (see
    :class:`GenerationConfigError`).

    ``devices`` is the catalog a ``pools.stage_device`` is checked against;
    without one the catalog probed for this server process is used, which
    before any probe knows only ``auto`` and ``cpu`` and therefore accepts any
    well-formed ``gpu:<n>``. A server that has not looked does not get to tell
    a user their second card does not exist.
    """
    raw = _coerce_rows(value)
    if not raw:
        return default_generations()

    rows: list[dict[str, Any]] = []
    for index, entry in enumerate(raw):
        if isinstance(entry, GenerationSpec):
            rows.append(entry.to_dict())
            continue
        if not isinstance(entry, Mapping):
            raise GenerationConfigError(
                f"ocr.generations[{index}]: each generation must be a mapping of "
                f"fields, got {type(entry).__name__}",
                row=index,
            )
        rows.append(dict(entry))

    ids = _assign_ids(rows)
    specs: list[GenerationSpec] = []
    names: dict[str, int] = {}
    for index, row in enumerate(rows):
        spec = _parse_row(index, row, ids[index], names, devices)
        names[spec.name] = index
        specs.append(spec)

    _validate_primary(specs)
    return specs


def parse_bench_spec(value: object, *, devices: DeviceCatalog | None = None) -> GenerationSpec:
    """Validate a benchmark's spec against the same rules a saved row gets.

    ``spec`` is the row AS CURRENTLY EDITED in the admin UI -- engine,
    detector, patch_budget and pools -- never read from config.
    ``name``/``primary``/``enabled``/``id`` are irrelevant to a benchmark and
    are ignored even when present. This reuses :func:`_parse_row`, so a spec
    is refused for exactly the reasons a ``PUT`` row would be (an unknown
    engine/detector/patch_budget, the removed ``char_map``, or a ``pools`` key
    outside the road's stage keys); the error always carries ``row=None`` (a
    spec is not a row in a list) and the offending ``field``.
    """
    if not isinstance(value, Mapping):
        raise GenerationConfigError(
            "spec must be a mapping of engine, detector, patch_budget and pools, "
            f"got {type(value).__name__ if value is not None else 'null'}"
        )
    row = {k: v for k, v in value.items() if k not in ("name", "primary", "enabled", "id")}
    try:
        spec = _parse_row(0, row, "bench-spec", {}, devices)
    except GenerationConfigError as e:
        message = str(e)
        prefix = "ocr.generations[0]: "
        if message.startswith(prefix):
            message = message[len(prefix) :]
        raise GenerationConfigError(f"spec: {message}", row=None, field=e.field) from None
    return spec


def _coerce_rows(value: object) -> list[Any]:
    if value is None:
        return []
    if isinstance(value, GenerationSpec):
        return [value]
    if isinstance(value, str):
        text = value.strip()
        if not text:
            return []
        try:
            decoded = json.loads(text)
        except json.JSONDecodeError as e:
            raise GenerationConfigError(
                f"ocr.generations must be a list of generations, or the JSON text of one; "
                f"could not read it as JSON ({e.msg})"
            ) from None
        return _coerce_rows(decoded)
    if isinstance(value, Mapping):
        return [value]
    if isinstance(value, Sequence):
        return list(value)
    raise GenerationConfigError(
        f"ocr.generations must be a list of generations, got {type(value).__name__}"
    )


def _assign_ids(rows: list[dict[str, Any]]) -> list[str]:
    """Each row's id: the one it carries, or a freshly minted ``g-<n>``."""
    taken: set[str] = set()
    for index, row in enumerate(rows):
        raw = row.get("id")
        if raw is None or str(raw).strip() == "":
            continue
        value = str(raw).strip()
        if not GENERATION_ID_RE.match(value):
            raise GenerationConfigError(
                f"ocr.generations[{index}]: id {value!r} is not usable — ids are letters, "
                "digits, '-' and '_' only (they name this row's working directories); "
                "leave it out and the server mints one",
                row=index,
                field="id",
            )
        if value in taken:
            raise GenerationConfigError(
                f"ocr.generations[{index}]: id {value!r} is already used by an earlier "
                "generation; ids identify a row for its whole life and must be unique",
                row=index,
                field="id",
            )
        taken.add(value)

    assigned: list[str] = []
    for row in rows:
        raw = row.get("id")
        if raw is None or str(raw).strip() == "":
            minted = mint_generation_id(taken)
            taken.add(minted)
            assigned.append(minted)
        else:
            assigned.append(str(raw).strip())
    return assigned


def _parse_row(
    index: int,
    row: Mapping[str, Any],
    row_id: str,
    names: Mapping[str, int],
    devices: DeviceCatalog | None = None,
) -> GenerationSpec:
    engine = str(row.get("engine", "") or "").strip()
    if not engine:
        raise GenerationConfigError(
            f"ocr.generations[{index}]: engine is required (one of {', '.join(ENGINE_IDS)})",
            row=index,
            field="engine",
        )
    try:
        spec = get_engine(engine)
    except ValueError as e:
        raise GenerationConfigError(
            f"ocr.generations[{index}]: {e}", row=index, field="engine"
        ) from None
    engine = spec.id

    detector: str | None = None
    if not (spec.uses_mokuro_env or spec.detector is not None):
        raw_detector = row.get("detector")
        wanted = str(raw_detector).strip() if raw_detector is not None else ""
        if not wanted:
            wanted = DEFAULT_DETECTOR
        try:
            detector = get_detector(wanted).id
        except ValueError as e:
            raise GenerationConfigError(
                f"ocr.generations[{index}]: {e}", row=index, field="detector"
            ) from None
        # Out of service for now (``DISABLED_DETECTORS``). Refused rather than
        # swapped for another detector or dropped: a row read with something
        # it does not name writes a sidecar that misstates what made it, and
        # a silently dropped row is a layer that quietly stops filling in --
        # the rule the removed ``char_map`` key gets. No released config can
        # carry such a row (``ocr.generations`` has not shipped, and the
        # released ``ocr.detector`` key is refused by name already), so this
        # bricks nothing on upgrade.
        if detector in DISABLED_DETECTORS:
            raise GenerationConfigError(
                f"ocr.generations[{index}]: {DISABLED_DETECTORS[detector]} -- set this "
                f"row's detector to one of {', '.join(OFFERED_DETECTOR_IDS)}, or delete "
                "the row",
                row=index,
                field="detector",
            )

    raw_name = row.get("name")
    name = str(raw_name).strip() if raw_name is not None else ""
    if not name:
        name = seed_generation_name(engine, detector, names)
    rejection = name_rejection(name)
    if rejection is not None:
        raise GenerationConfigError(
            f"ocr.generations[{index}]: {rejection}", row=index, field="name"
        )
    if name in names:
        raise GenerationConfigError(
            f"ocr.generations[{index}]: name {name!r} is already generation "
            f"{names[name]}'s; every generation writes a file named after it, so names "
            "must be unique",
            row=index,
            field="name",
        )

    # The character-map system was removed, so a row still carrying its key is
    # a recipe nothing can honour. Refused rather than ignored: the same rule
    # the retired ``ocr.*`` keys get, and for the same reason -- a silently
    # dropped setting is a run that does not do what the file says.
    if "char_map" in row:
        raise GenerationConfigError(
            f"ocr.generations[{index}]: char_map was removed with the character-map "
            "system (no per-character placement mode produced output worth using; "
            "readers lay characters on a uniform grid) -- delete the key",
            row=index,
            field="char_map",
        )

    # A JSON `null` is "not set", the same as leaving the key out: a client
    # editing a row whose engine has no use for a field (no patch budget on
    # paddle-manga) has nothing to put there, and refusing the whole list over
    # it helps nobody.
    raw_patch_budget = row.get("patch_budget")
    try:
        patch_budget = get_patch_budget(
            DEFAULT_PATCH_BUDGET if raw_patch_budget is None else raw_patch_budget
        )
    except ValueError as e:
        raise GenerationConfigError(
            f"ocr.generations[{index}]: {e}", row=index, field="patch_budget"
        ) from None

    raw_pools = row.get("pools")
    legacy = raw_pools.get("precision") if isinstance(raw_pools, Mapping) else None
    pick = row.get("precision_pick")
    candidate = GenerationSpec(
        id=row_id,
        name=name,
        engine=engine,
        primary=bool(row.get("primary", False)),
        enabled=bool(row.get("enabled", True)),
        detector=detector,
        patch_budget=patch_budget,
        precision=_parse_precision(index, engine, row.get("precision"), legacy),
        precision_pick=str(pick) if pick in PRECISIONS else None,
        precision_why=str(row.get("precision_why") or "") if pick in PRECISIONS else "",
    )
    return replace(candidate, pools=_parse_pools(index, row.get("pools"), candidate, devices))


def _parse_pools(
    index: int,
    raw: object,
    spec: GenerationSpec,
    devices: DeviceCatalog | None = None,
) -> GenerationPools:
    """Validate a row's ``pools`` against the stage keys of ITS road."""
    if raw is None:
        return GenerationPools()
    if not isinstance(raw, Mapping):
        raise GenerationConfigError(
            f"ocr.generations[{index}]: pools must be a mapping with "
            f"{', '.join(repr(key) for key in POOL_KEYS)}",
            row=index,
            field="pools",
        )
    unknown = [key for key in raw if key not in POOL_KEYS]
    if unknown:
        raise GenerationConfigError(
            f"ocr.generations[{index}]: pools has no {unknown[0]!r} setting "
            f"(the settings are {', '.join(POOL_KEYS)})",
            row=index,
            field="pools",
        )

    parsed: dict[str, dict[str, int]] = {"stage_workers": {}, "queue_capacity": {}}
    for pool_key in ("stage_workers", "queue_capacity"):
        values = raw.get(pool_key)
        if values is None:
            continue
        if not isinstance(values, Mapping):
            raise GenerationConfigError(
                f"ocr.generations[{index}]: pools.{pool_key} must map a stage name to a "
                "number",
                row=index,
                field="pools",
            )
        # A monolithic row has ONE stage and no queues: its Workers cell is the
        # fork's own ``--num_workers``, and there is nothing between stages to
        # give a depth to.
        stage_keys = spec.pool_stage_keys if pool_key == "stage_workers" else spec.stage_keys
        limit = MAX_STAGE_WORKERS if pool_key == "stage_workers" else MAX_QUEUE_CAPACITY
        floor = 0 if pool_key == "stage_workers" else 1
        for stage, number in values.items():
            key = str(stage).strip()
            if not stage_keys:
                raise GenerationConfigError(
                    f"ocr.generations[{index}]: engine {spec.engine!r} runs behind its own "
                    f"command line and has no queues between stages; pools.{pool_key} "
                    f"must be empty (its one stage {STAGE_MOKURO!r} takes a device and a "
                    "worker count)",
                    row=index,
                    field="pools",
                )
            if key not in stage_keys:
                runs = (
                    f"{spec.engine} runs behind its own command line, whose one stage is"
                    if spec.road is None
                    else f"{spec.engine} with {spec.effective_detector} runs"
                )
                raise GenerationConfigError(
                    f"ocr.generations[{index}]: pools.{pool_key} names stage {key!r}, but "
                    f"{runs} {', '.join(stage_keys)}",
                    row=index,
                    field="pools",
                )
            try:
                width = int(number)
            except (TypeError, ValueError):
                raise GenerationConfigError(
                    f"ocr.generations[{index}]: pools.{pool_key}.{key} must be a whole "
                    f"number, got {number!r}",
                    row=index,
                    field="pools",
                ) from None
            if width < floor or width > limit:
                raise GenerationConfigError(
                    f"ocr.generations[{index}]: pools.{pool_key}.{key} is {width}; it must "
                    f"be between {floor} and {limit}",
                    row=index,
                    field="pools",
                )
            parsed[pool_key][key] = width
    return GenerationPools(
        stage_workers=parsed["stage_workers"],
        queue_capacity=parsed["queue_capacity"],
        stage_device=_parse_stage_device(index, raw.get("stage_device"), spec, devices),
    )


def _parse_precision(index: int, engine: str, raw: object, legacy: object = None) -> str:
    """The row's precision mode, from ``precision`` or a legacy ``pools.precision``.

    ``auto`` (the old spelling) is the default mode, and a legacy row-level
    pin becomes the forced mode of that format. An engine that fixes its own
    precision takes the default whatever the row says: the mode never reaches
    it. A forced format the engine cannot run at all (bf16 on mokuro) is
    refused.
    """
    value = raw if raw not in (None, "") else legacy
    try:
        mode = normalize_precision_mode(value)
    except ValueError:
        raise GenerationConfigError(
            f"ocr.generations[{index}]: precision is {value!r}; it must be one of "
            f"{', '.join(PRECISION_MODES)}",
            row=index,
            field="precision",
        ) from None
    if engine not in PRECISION_ENGINES:
        return DEFAULT_PRECISION_MODE
    if mode in FORCED_PRECISION_MODES and mode not in engine_formats(engine):
        raise GenerationConfigError(
            f"ocr.generations[{index}]: precision is {mode!r}, but {engine} does not run "
            f"{mode} (it runs {', '.join(engine_formats(engine))})",
            row=index,
            field="precision",
        )
    return mode


def _parse_stage_device(
    index: int,
    raw: object,
    spec: GenerationSpec,
    devices: DeviceCatalog | None = None,
) -> dict[str, str]:
    """Validate ``pools.stage_device``: model-bearing stages, catalog ids.

    Three refusals, each naming what to do instead: a stage with no model
    (``post`` is CPU work with nothing to place), a device this server does not
    have, and a GPU for a model that cannot leave the CPU.
    """
    if raw is None:
        return {}
    if not isinstance(raw, Mapping):
        raise GenerationConfigError(
            f"ocr.generations[{index}]: pools.stage_device must map a stage name to a "
            "device (cpu or gpu:<n>)",
            row=index,
            field="pools",
        )
    catalog = devices if devices is not None else cached_catalog()
    allowed = spec.device_stage_keys
    out: dict[str, str] = {}
    for stage, value in raw.items():
        key = str(stage).strip()
        if key not in allowed:
            runs = ", ".join(allowed)
            raise GenerationConfigError(
                f"ocr.generations[{index}]: pools.stage_device names stage {key!r}, but "
                f"only a stage holding a model takes a device; this row's are {runs}",
                row=index,
                field="pools",
            )
        if value is None:
            continue
        try:
            device = parse_device(str(value))
        except ValueError as e:
            raise GenerationConfigError(
                f"ocr.generations[{index}]: pools.stage_device.{key}: {e}",
                row=index,
                field="pools",
            ) from None
        if not catalog.knows(device):
            raise GenerationConfigError(
                f"ocr.generations[{index}]: pools.stage_device.{key}: {catalog.refusal(device)}",
                row=index,
                field="pools",
            )
        locked = stage_lock_reason(
            spec.road, key, engine=spec.engine, detector=spec.effective_detector,
            catalog=catalog,
        )
        if locked is not None and device not in (DEVICE_AUTO, DEVICE_CPU):
            raise GenerationConfigError(
                f"ocr.generations[{index}]: pools.stage_device.{key} is {device!r}, but "
                f"{locked}; leave it on cpu",
                row=index,
                field="pools",
            )
        out[key] = device
    return out


def _validate_primary(specs: Sequence[GenerationSpec]) -> None:
    """Exactly one enabled row writes the bare ``<Volume>.mokuro``.

    The primary sidecar is the only source of a downloaded volume's
    ``mokuro_version``, character counts and page character counts. Without
    one, every volume reads as image-only.
    """
    enabled = [spec for spec in specs if spec.enabled]
    if not enabled:
        return
    primaries = [spec for spec in enabled if spec.primary]
    if not primaries:
        raise GenerationConfigError(
            "ocr.generations: no enabled generation is the primary one — exactly one must "
            "be, because it writes the bare <Volume>.mokuro every reader counts characters "
            "and inherits volume ids from",
            field="primary",
        )
    if len(primaries) > 1:
        names = ", ".join(repr(spec.name) for spec in primaries)
        raise GenerationConfigError(
            f"ocr.generations: {names} are all marked primary — exactly one enabled "
            "generation may write the bare <Volume>.mokuro",
            row=specs.index(primaries[1]),
            field="primary",
        )


# --- collection helpers -------------------------------------------------


def enabled_generations(specs: Sequence[GenerationSpec]) -> list[GenerationSpec]:
    """The rows the queue runs, in the order it runs them: LIST ORDER.

    The one ordering function. The queue takes its order from it and the OS
    priority rule keys on it, so the job running at normal priority is always
    the head of the list the queue page shows.
    """
    return [spec for spec in specs if spec.enabled]


def primary_generation(specs: Sequence[GenerationSpec]) -> GenerationSpec | None:
    """The enabled row that writes the bare ``<Volume>.mokuro``, if any."""
    for spec in enabled_generations(specs):
        if spec.primary:
            return spec
    return None


def generation_by_id(specs: Sequence[GenerationSpec], gen_id: str) -> GenerationSpec | None:
    """The row with this id, enabled or not."""
    for spec in specs:
        if spec.id == gen_id:
            return spec
    return None


def required_detectors(specs: Sequence[GenerationSpec]) -> tuple[str, ...]:
    """Detectors whose extra packages the engines environment needs.

    The UNION over the rows that run in it: a row on an engine with its own
    detector needs that one, every other row needs the one it configures.
    Monolithic rows bring their own and need none.
    """
    needed: list[str] = []
    for spec in enabled_generations(specs):
        if spec.mokuro_env:
            continue
        wanted = spec.effective_detector
        if wanted not in needed:
            needed.append(wanted)
    return tuple(needed)


# Keys of a "what this server could not install" map (see
# `local_environment_problem`): the mokuro environment, the engines
# environment, or one detector's extras in it.
ENV_MOKURO = "mokuro"
ENV_ENGINES = "engines"


def detector_env_key(detector: str) -> str:
    return f"detector:{detector}"


def local_environment_problem(
    problems: Mapping[str, str], row: GenerationSpec
) -> str | None:
    """Why THIS server cannot run ``row``, from what it could not install.

    ``problems`` maps `ENV_MOKURO`, `ENV_ENGINES` or `detector_env_key(id)`
    to a sentence. The library's own hardware is just another processor
    entry (spec section 0): an environment that failed to install HERE
    takes the rows that need it off this server's slots only -- the queue
    keeps them, and a processor whose catalog can run them is offered them.
    """
    if not problems:
        return None
    if row.mokuro_env:
        return problems.get(ENV_MOKURO)
    return problems.get(ENV_ENGINES) or problems.get(detector_env_key(row.effective_detector))


def required_engines(specs: Sequence[GenerationSpec]) -> list[str]:
    """Engine ids the enabled rows need, de-duplicated, in row order."""
    engines: list[str] = []
    for spec in enabled_generations(specs):
        if spec.engine not in engines:
            engines.append(spec.engine)
    return engines


def sidecar_siblings(cbz_path: Path) -> list[Path]:
    """Every file that belongs to this archive and goes when it goes.

    Listed from the DIRECTORY rather than from a registry of configured
    names: a row that was renamed, disabled or deleted, a layer some other
    server wrote, and a layer a reader pushed all sat beside the archive and
    must all go with it. The postfix is matched against the READER's layer
    grammar, so ``Vol 01.hayai-nova.mokuro`` is swept and an unrelated
    ``Vol 01.backup.2024.mokuro`` is not.
    """
    stem = cbz_path.stem
    directory = cbz_path.parent
    siblings: list[Path] = []
    for suffix in (".mokuro", ".mokuro.gz", ".webp", ".nocover"):
        siblings.append(directory / f"{stem}{suffix}")
    try:
        entries = sorted(directory.iterdir())
    except OSError:
        return siblings
    # The OTHER volumes here. ``Volume 01.5.mokuro`` reads as layer ``5`` of
    # ``Volume 01`` -- and is the primary OCR of ``Volume 01.5.cbz``. Deleting
    # volume 1 must not take volume 1.5's text with it.
    others = volume_stems(entry.name for entry in entries) - {stem}
    for entry in entries:
        layer = layer_id_of_sidecar(entry.name, stem, other_volumes=others)
        if layer is not None:
            siblings.append(entry)
    return siblings


# Archive extensions a volume can have (``processor.SUPPORTED_EXTENSIONS``; a
# test pins the two together -- this module cannot import the processor).
VOLUME_ARCHIVE_EXTENSIONS: tuple[str, ...] = (".cbz", ".cbr", ".zip", ".rar")


def volume_stems(file_names: Iterable[str]) -> set[str]:
    """The stems of the volume archives among these file names."""
    stems: set[str] = set()
    for name in file_names:
        lower = name.lower()
        for ext in VOLUME_ARCHIVE_EXTENSIONS:
            if lower.endswith(ext):
                stems.add(name[: -len(ext)])
                break
    return stems


def split_layer_sidecar(file_name: str) -> tuple[str, str] | None:
    """``("Vol 1", "hayai-nova")`` for ``Vol 1.hayai-nova.mokuro[.gz]``.

    None for the bare ``<stem>.mokuro`` (that is the volume's primary OCR,
    not a layer), for anything that is not a sidecar, and for a postfix the
    reader would not read as a layer id.

    Split on the LAST dot, which is what the reader does: a volume stem may
    itself contain one, so ``Vol 1.backup.v1.mokuro`` is layer ``v1`` of a
    volume called ``Vol 1.backup`` -- and a layer of ``Vol 1`` called
    ``backup.v1`` is not a thing, because a layer id may not contain a dot.
    """
    name = file_name
    if name.endswith(".gz"):
        name = name[: -len(".gz")]
    if not name.endswith(".mokuro"):
        return None
    middle = name[: -len(".mokuro")]
    cut = middle.rfind(".")
    if cut <= 0:
        return None
    layer = middle[cut + 1 :]
    if not LAYER_ID_RE.match(layer):
        return None
    return middle[:cut], layer


def layer_id_of_sidecar(
    file_name: str, stem: str, *, other_volumes: Collection[str] = ()
) -> str | None:
    """The layer id of ``<stem>.<id>.mokuro[.gz]``, or None for another file.

    ``other_volumes`` are the stems of the other archives beside it. The name
    alone is ambiguous: ``Volume 01.5.mokuro`` is layer ``5`` of ``Volume 01``
    AND the primary sidecar of a volume called ``Volume 01.5`` -- a decimal
    volume number is an ordinary thing for a series to have. An archive of that
    name settles it, exactly as the reader does (``classifyMokuroSidecar``):
    the file is that volume's own OCR and no layer of this one.
    """
    split = split_layer_sidecar(file_name)
    if split is None or split[0] != stem:
        return None
    if f"{stem}.{split[1]}" in other_volumes:
        return None
    return split[1]
