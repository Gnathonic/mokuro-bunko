"""What devices this server can place a model on, and what may go where.

A device id is ``auto``, ``cpu`` or ``gpu:<n>`` -- bunko's public spelling
whatever the vendor is (:mod:`engine_runner` owns the grammar and the
translation to torch's ``cuda:<n>``). This module is the SERVER's side of it:
the catalog of ids this host really has, and the rules that say which of them a
given row's stage may be set to.

The catalog is probed ONCE per server process, in the ENGINES environment --
the server's own environment has no torch, and the card the OCR runs on is the
one that environment can see (a ``HIP_VISIBLE_DEVICES`` set for the server
would otherwise advertise a card its OCR never touches). Without that
environment the honest answer is ``auto`` and ``cpu``: no claim about cards the
server cannot see.

Nothing here imports torch, and nothing here runs a subprocess: the probe's
OUTPUT is parsed here (:func:`parse_probe`) and the subprocess that produces it
lives beside the bench host probe in ``bench.py``, which already owns running
things in the engines environment.
"""

from __future__ import annotations

import json
import threading
from collections.abc import Iterable, Mapping, Sequence
from dataclasses import dataclass, replace
from typing import Any

from mokuro_bunko.ocr.engine_runner import (
    DEVICE_AUTO,
    DEVICE_CPU,
    GPU_DEVICE_PREFIX,
    MODEL_STAGES,
    ORT_GPU_PROVIDERS,
    PRECISION_BF16,
    PRECISION_FP16,
    PRECISION_FP32,
    STAGE_ENGINE,
    STAGE_MOKURO,
    model_stages,
    parse_device,
    stage_is_cpu_only,
    stage_needs_ort_gpu,
)
from mokuro_bunko.ocr.engines import get_detector, get_engine

__all__ = [
    "DEVICE_AUTO",
    "DEVICE_CPU",
    "MODEL_STAGES",
    "STAGE_MOKURO",
    "DeviceCatalog",
    "GPU_FACTS_KEY",
    "GpuDevice",
    "ORT_CATALOG_KEY",
    "PROBE_SOURCE",
    "cached_catalog",
    "catalog_from_entries",
    "catalog_from_processor",
    "merge_catalogs",
    "parse_probe",
    "set_cached_catalog",
    "stage_devices_allowed",
    "stage_lock_reason",
]

# The one-shot probe, run by ``bench.py`` with the engines environment's
# interpreter. One JSON object on stdout and nothing else, so a warning from
# some import cannot be mistaken for the answer. ``torch.version.hip`` is asked
# FIRST: on ROCm torch still calls the card ``cuda``, and a label that says
# CUDA on an AMD card is a lie the user has to un-learn.
#
# onnxruntime is asked too, and separately: a detector whose card is an
# onnxruntime execution provider (``engine_runner.ORT_GPU_DETECTORS``) cannot
# use the card torch sees unless the onnxruntime wheel beside it has a GPU
# provider -- and pip's default wheel has none. ``None`` (no onnxruntime in the
# environment) is "not asked", never "no".
#
# Each card's FORMATS too: what torch says that device can compute in, which
# is all a row's precision mode is ever judged by (no list of architectures
# anywhere). fp16 on any card; bf16 where ``torch.cuda.is_bf16_supported()``
# says yes with that device current -- an RX 6000 says yes and emulates it,
# which only a benchmark shows to be slow.
PROBE_SOURCE = (
    "import json,torch\n"
    "hip=getattr(torch.version,'hip',None)\n"
    "n=torch.cuda.device_count() if torch.cuda.is_available() else 0\n"
    "def bf16(i):\n"
    "    try:\n"
    "        with torch.cuda.device(i):\n"
    "            return bool(torch.cuda.is_bf16_supported())\n"
    "    except Exception:\n"
    "        return False\n"
    "try:\n"
    "    import onnxruntime\n"
    "    offered=list(onnxruntime.get_available_providers())\n"
    f"    ort=[p for p in {list(ORT_GPU_PROVIDERS)!r} if p in offered]\n"
    "except Exception:\n"
    "    ort=None\n"
    "print(json.dumps({'vendor':'rocm' if hip else ('cuda' if n else ''),"
    "'gpus':[{'index':i,'name':torch.cuda.get_device_name(i),"
    "'memory_bytes':torch.cuda.mem_get_info(i)[1],"
    "'formats':{'bf16':bf16(i),'fp16':True}} for i in range(n)],"
    "'onnxruntime_gpu_providers':ort}))"
)

# Where a processor's registration catalog carries each card's formats:
# ``[{"index": 0, "formats": {"bf16": true, "fp16": true}}]``. Optional and
# backward compatible: a processor older than it reports none, and its cards
# count as fp32-only for a forced precision (their runner still decides an
# auto mode for itself at start).
GPU_FACTS_KEY = "gpus"

# Where a processor's catalog (and the probe's output) carries the onnxruntime
# answer: a list of GPU provider names, ``[]`` for a CPU-only wheel. Absent --
# a processor from before this key, or an environment without onnxruntime --
# is unknown, and unknown refuses nothing.
ORT_CATALOG_KEY = "onnxruntime_gpu_providers"


def _ort_providers(value: Any) -> tuple[str, ...] | None:
    """A reported provider list, or None when nothing (usable) was reported."""
    if not isinstance(value, (list, tuple)):
        return None
    return tuple(str(p) for p in value if isinstance(p, str) and p)


@dataclass(frozen=True)
class GpuDevice:
    """One card this host can place a model on."""

    index: int
    name: str
    memory_bytes: int | None = None
    # What the probe found this card can compute in, besides fp32 (``bf16``,
    # ``fp16``); None when nobody said (a processor older than the probe).
    formats: frozenset[str] | None = None

    def supported(self) -> frozenset[str] | None:
        """Every format this card computes in (fp32 included), or None: not reported."""
        if self.formats is None:
            return None
        return frozenset({PRECISION_FP32, *self.formats})

    @property
    def id(self) -> str:
        return f"{GPU_DEVICE_PREFIX}{self.index}"

    @property
    def label(self) -> str:
        name = self.name.strip()
        # A card nobody named (a processor's row without a label) is just
        # its index: never "GPU 0 — GPU", and never another machine's name.
        if not name:
            return f"GPU {self.index}"
        # A label a REMOTE processor already rendered arrives here as the
        # name; wrapping it again would read "GPU 0 — GPU 0 — RTX 4090".
        if name.startswith(f"GPU {self.index} "):
            return name
        if self.memory_bytes:
            return f"GPU {self.index} — {name} ({self.memory_bytes / 1_000_000_000:.0f} GB)"
        return f"GPU {self.index} — {name}"


@dataclass(frozen=True)
class DeviceCatalog:
    """Every device id a row may name, in the order the UI shows them.

    ``probed`` is False for the fallback catalog (no engines environment, or a
    probe that failed): the ids are then ``auto`` and ``cpu``, and a ``gpu:<n>``
    is NOT refused -- a server that cannot look is not entitled to tell a user
    their card does not exist. Once a probe has answered, an index it does not
    list is refused, because then the refusal is knowledge.
    """

    gpus: tuple[GpuDevice, ...] = ()
    cpu_label: str = "CPU"
    vendor: str = ""
    probed: bool = False
    # The GPU execution providers the host's onnxruntime offers; None when the
    # host never said (see ``ORT_CATALOG_KEY``).
    ort_gpu_providers: tuple[str, ...] | None = None
    # Whose devices these are, as a refusal names it: this server's own by
    # default, a processor's name once :meth:`for_machine` has said so.
    where: str = "this server"

    @property
    def has_gpu(self) -> bool:
        return bool(self.gpus)

    @property
    def ort_gpu(self) -> bool | None:
        """Can an onnxruntime model reach a card here? None: not known."""
        if self.ort_gpu_providers is None:
            return None
        return bool(self.ort_gpu_providers)

    def ids(self) -> tuple[str, ...]:
        return (DEVICE_AUTO, DEVICE_CPU, *(gpu.id for gpu in self.gpus))

    def supported_for(self, device_id: str | None) -> frozenset[str] | None:
        """The formats a model placed at ``device_id`` could compute in here.

        The CPU: fp32. A card: what its probe said, or None when it said
        nothing (not reported, or a catalog that never looked). ``auto`` is
        card 0 where there is one, else the CPU -- or unknown when this
        catalog could not look at all.
        """
        wanted = str(device_id or DEVICE_AUTO)
        if wanted == DEVICE_CPU:
            return frozenset({PRECISION_FP32})
        if wanted in (DEVICE_AUTO, ""):
            if self.gpus:
                return self.gpus[0].supported()
            return frozenset({PRECISION_FP32}) if self.probed else None
        for gpu in self.gpus:
            if gpu.id == wanted:
                return gpu.supported()
        return None

    def gpu_facts(self) -> list[dict[str, Any]]:
        """``GPU_FACTS_KEY``: each card's probed formats, as a processor registers them."""
        return [
            {
                "index": gpu.index,
                "formats": {
                    PRECISION_BF16: PRECISION_BF16 in gpu.formats,
                    PRECISION_FP16: PRECISION_FP16 in gpu.formats,
                },
            }
            for gpu in self.gpus
            if gpu.formats is not None
        ]

    def knows(self, device_id: str) -> bool:
        """Whether this catalog can place a model on this id."""
        try:
            wanted = parse_device(device_id)
        except ValueError:
            return False
        if wanted in (DEVICE_AUTO, DEVICE_CPU):
            return True
        if not self.probed:
            return True
        return wanted in {gpu.id for gpu in self.gpus}

    def refusal(self, device_id: str) -> str:
        """Why this id cannot be used here, in the server's own words."""
        count = len(self.gpus)
        cards = "1 GPU" if count == 1 else f"{count} GPUs"
        return f"device {device_id!r} is not on {self.where}, which reports {cards}"

    def label_for(self, device_id: str) -> str:
        """What the Device select calls this id ON THIS MACHINE.

        The catalog's own words where it has them; plain ``CPU`` / ``GPU <n>``
        where it has none, so a machine that did not say is never labelled
        with another machine's hardware.
        """
        text = str(device_id or DEVICE_AUTO)
        if text == DEVICE_AUTO:
            return "Auto"
        if text == DEVICE_CPU:
            return self.cpu_label.strip() or "CPU"
        for gpu in self.gpus:
            if gpu.id == text:
                return gpu.label
        if text.startswith(GPU_DEVICE_PREFIX):
            return f"GPU {text[len(GPU_DEVICE_PREFIX):]}"
        return text

    def for_machine(self, name: str, host: Mapping[str, Any] | None) -> DeviceCatalog:
        """This catalog as ONE named machine's, with its host line filling gaps.

        ``host`` is that machine's own ``describe_host`` answer (its CPU and
        first card), so it may name a CPU the catalog left as plain ``CPU``
        and card 0 where the catalog's row for it had no label. Nothing else
        is invented: no card is added, and a refusal names the machine.
        """
        facts = host if isinstance(host, Mapping) else {}
        cpu = str(facts.get("cpu") or "").strip()
        first = str(facts.get("gpu") or "").strip()
        cpu_label = self.cpu_label
        if cpu and (not cpu_label.strip() or cpu_label.strip() == "CPU"):
            cpu_label = cpu
        gpus = tuple(
            GpuDevice(gpu.index, first, gpu.memory_bytes, gpu.formats)
            if first and gpu.index == 0 and not gpu.name.strip()
            else gpu
            for gpu in self.gpus
        )
        return replace(self, gpus=gpus, cpu_label=cpu_label, where=name)

    def entries(self) -> list[dict[str, Any]]:
        """``catalog.devices``: what the Device select offers, in order."""
        auto = f"Auto — GPU {self.gpus[0].index} when available" if self.gpus else "Auto — CPU"
        rows: list[dict[str, Any]] = [
            {"id": DEVICE_AUTO, "label": auto},
            {"id": DEVICE_CPU, "label": self.cpu_label},
        ]
        rows.extend({"id": gpu.id, "label": gpu.label} for gpu in self.gpus)
        return rows


def parse_probe(payload: str | None, *, cpu_label: str = "CPU") -> DeviceCatalog:
    """The catalog a :data:`PROBE_SOURCE` run describes; the fallback on junk."""
    if not payload:
        return DeviceCatalog(cpu_label=cpu_label)
    try:
        data = json.loads(payload)
    except (TypeError, ValueError):
        return DeviceCatalog(cpu_label=cpu_label)
    if not isinstance(data, dict):
        return DeviceCatalog(cpu_label=cpu_label)
    gpus: list[GpuDevice] = []
    for entry in data.get("gpus") or ():
        if not isinstance(entry, dict):
            continue
        try:
            index = int(entry["index"])
        except (KeyError, TypeError, ValueError):
            continue
        memory = entry.get("memory_bytes")
        gpus.append(
            GpuDevice(
                index=index,
                name=str(entry.get("name") or "").strip(),
                memory_bytes=int(memory) if isinstance(memory, (int, float)) and memory else None,
                formats=_formats(entry.get("formats")),
            )
        )
    vendor = str(data.get("vendor") or "")
    return DeviceCatalog(
        gpus=tuple(sorted(gpus, key=lambda gpu: gpu.index)),
        cpu_label=cpu_label,
        vendor=vendor,
        probed=True,
        ort_gpu_providers=_ort_providers(data.get(ORT_CATALOG_KEY)),
    )


def _formats(value: Any) -> frozenset[str] | None:
    """``{"bf16": true, "fp16": true}`` as the formats said yes; None when absent."""
    if not isinstance(value, Mapping):
        return None
    return frozenset(
        fmt for fmt in (PRECISION_BF16, PRECISION_FP16) if value.get(fmt) is True
    )


def catalog_from_entries(
    entries: Sequence[Mapping[str, Any]],
    *,
    cpu_label: str = "CPU",
    ort_gpu_providers: tuple[str, ...] | None = None,
) -> DeviceCatalog:
    """A catalog out of what a REMOTE processor reported.

    The other direction of :meth:`DeviceCatalog.entries`: a processor probes
    its own hardware and sends the same ``{"id", "label"}`` rows, and the
    admin API needs them back as a catalog so a row's Device select and its
    derived widths can be computed for THAT machine. Rows that are not
    mappings are skipped rather than trusted.
    """
    gpus: list[GpuDevice] = []
    cpu = cpu_label
    seen = False
    for entry in entries or ():
        if not isinstance(entry, Mapping):
            continue
        seen = True
        device_id = str(entry.get("id") or "")
        label = str(entry.get("label") or "")
        if device_id == DEVICE_CPU:
            cpu = label or cpu
            continue
        if not device_id.startswith(GPU_DEVICE_PREFIX):
            continue
        try:
            index = int(device_id[len(GPU_DEVICE_PREFIX) :])
        except ValueError:
            continue
        gpus.append(GpuDevice(index=index, name=label))
    if not seen:
        return DeviceCatalog(cpu_label=cpu, ort_gpu_providers=ort_gpu_providers)
    return DeviceCatalog(
        gpus=tuple(sorted(gpus, key=lambda gpu: gpu.index)),
        cpu_label=cpu,
        probed=True,
        ort_gpu_providers=ort_gpu_providers,
    )


def catalog_from_processor(catalog: Mapping[str, Any] | None) -> DeviceCatalog:
    """The device catalog of a processor's WHOLE reported catalog.

    Its ``devices`` rows (:func:`catalog_from_entries`) and what its
    onnxruntime can reach (``ORT_CATALOG_KEY``). Anything that is not a
    mapping is a processor that reported nothing.
    """
    if not isinstance(catalog, Mapping):
        return DeviceCatalog()
    devices = catalog.get("devices")
    parsed = catalog_from_entries(
        devices if isinstance(devices, list) else [],
        ort_gpu_providers=_ort_providers(catalog.get(ORT_CATALOG_KEY)),
    )
    # Each card's probed formats (``GPU_FACTS_KEY``), where it reported them.
    facts: dict[int, frozenset[str]] = {}
    for entry in catalog.get(GPU_FACTS_KEY) or ():
        if not isinstance(entry, Mapping):
            continue
        index = entry.get("index")
        formats = _formats(entry.get("formats"))
        if isinstance(index, int) and not isinstance(index, bool) and formats is not None:
            facts[index] = formats
    if not facts:
        return parsed
    return DeviceCatalog(
        gpus=tuple(
            GpuDevice(gpu.index, gpu.name, gpu.memory_bytes, formats=facts.get(gpu.index))
            for gpu in parsed.gpus
        ),
        cpu_label=parsed.cpu_label,
        vendor=parsed.vendor,
        probed=parsed.probed,
        ort_gpu_providers=parsed.ort_gpu_providers,
    )


def merge_catalogs(catalogs: Iterable[DeviceCatalog]) -> DeviceCatalog:
    """Every device ANY of these machines can place a model on.

    What a saved row is held to once processors exist: a row pinned to
    ``gpu:1`` is a valid setting when one machine that may run it has two
    cards, even if the library's own box has none (spec section 3 rule 2
    is then what keeps it off the machines that cannot). An UNPROBED
    catalog among them keeps the merge unprobed -- a machine that cannot
    look must not make another machine's cards unsettable.
    """
    parts = list(catalogs)
    if not parts:
        return DeviceCatalog()
    by_index: dict[int, GpuDevice] = {}
    for catalog in parts:
        for gpu in catalog.gpus:
            by_index.setdefault(gpu.index, gpu)
    # The same rule for onnxruntime: one machine whose runtime reaches a card
    # makes a card a valid setting; only machines that ALL said "none" make it
    # a refusal, and one that never said keeps the answer unknown.
    reached = [c.ort_gpu_providers for c in parts if c.ort_gpu_providers]
    if reached:
        ort: tuple[str, ...] | None = tuple(dict.fromkeys(p for ps in reached for p in ps))
    elif any(c.ort_gpu_providers is None for c in parts):
        ort = None
    else:
        ort = ()
    return DeviceCatalog(
        gpus=tuple(by_index[index] for index in sorted(by_index)),
        cpu_label=parts[0].cpu_label,
        vendor=parts[0].vendor,
        probed=all(catalog.probed for catalog in parts),
        ort_gpu_providers=ort,
    )


_cached: DeviceCatalog | None = None
_cache_lock = threading.Lock()


def cached_catalog() -> DeviceCatalog:
    """The catalog probed for this server process, or the fallback."""
    with _cache_lock:
        return _cached if _cached is not None else DeviceCatalog()


def set_cached_catalog(catalog: DeviceCatalog | None) -> None:
    """Publish a freshly probed catalog (or clear it, so the next ask probes)."""
    global _cached
    with _cache_lock:
        _cached = catalog


def stage_devices_allowed(
    road: str | None,
    key: str,
    *,
    engine: str,
    detector: str = "",
    catalog: DeviceCatalog | None = None,
) -> list[str]:
    """Which device ids this stage may be set to, in catalog order.

    ``["cpu"]`` for a model that cannot leave the CPU (plus ``auto``, which
    resolves there); the whole catalog for anything else; ``[]`` for a stage
    with no model, whose Device cell is a label and not a control.
    """
    known = catalog if catalog is not None else cached_catalog()
    if not stage_takes_a_device(road, key):
        return []
    if stage_lock_reason(road, key, engine=engine, detector=detector, catalog=known):
        return [DEVICE_AUTO, DEVICE_CPU]
    return list(known.ids())


def stage_takes_a_device(road: str | None, key: str) -> bool:
    """Whether a device may be chosen for this stage at all."""
    if road is None:
        return key == STAGE_MOKURO
    return key in model_stages(road)


def stage_lock_reason(
    road: str | None,
    key: str,
    *,
    engine: str,
    detector: str = "",
    catalog: DeviceCatalog | None = None,
) -> str | None:
    """Why this stage is locked to the CPU, or None when it is free to move.

    The sentence comes from the engine/detector registry, so the UI shows the
    same reason the catalog publishes, and the two cannot drift.

    ``catalog`` is the HOST's (this server's, a processor's, or every
    machine's merged): a detector whose card is an onnxruntime execution
    provider is locked on a host whose onnxruntime reported none. Without a
    catalog, or on one that never said, only the registry decides.
    """
    if road is None:
        if key != STAGE_MOKURO:
            return None
        return get_engine(engine).cpu_only_reason or None
    if key not in model_stages(road):
        return None
    if not stage_is_cpu_only(road, key, engine=engine, detector=detector):
        if (
            catalog is not None
            and catalog.ort_gpu is False
            and stage_needs_ort_gpu(road, key, detector=detector, engine=engine)
        ):
            return (
                f"the {detector} detector reaches a GPU through onnxruntime, and this "
                "machine's onnxruntime has no GPU execution provider"
            )
        return None
    if key == STAGE_ENGINE:
        return get_engine(engine).cpu_only_reason or "this recognizer runs on the CPU"
    spec = get_engine(engine)
    if spec.detector:
        # The engine brings its own detector: its reason is the engine's.
        return (
            get_detector(spec.detector).cpu_only_reason
            or spec.cpu_only_reason
            or "this detector runs on the CPU"
        )
    return get_detector(detector).cpu_only_reason or "this detector runs on the CPU"
