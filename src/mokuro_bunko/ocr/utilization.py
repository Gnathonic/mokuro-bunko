"""How busy the machine was WHILE a benchmark's pages were coming out.

ADDENDUM 9. The owner's complaint about the first benchmarks was "neither the
GPU nor the CPU seemed tapped", and there was no number to answer it with.
This is that number: GPU busy and CPU busy, sampled once a second for the
life of a bench run, and averaged afterwards over exactly the window a trial
was timed over -- never over the model load in front of it, and never over
the gap between one trial and the next.

The probes are deliberately the cheapest ones the platform offers:

* ``/sys/class/drm/card*/device/gpu_busy_percent`` -- one integer, one read,
  no process. This host (AMD) has it;
* ``nvidia-smi --query-gpu=utilization.gpu`` when it does not and an NVIDIA
  driver is present;
* ``/proc/stat``'s aggregate ``cpu`` line, differenced between ticks, for
  the CPU across all cores.

Everything here takes its paths as arguments so the tests can point it at a
directory of files instead of at a kernel.

VRAM is deliberately NOT one of these numbers. A benchmark's
``peak_vram_mb`` is torch's peak allocation inside the RUNNER's own process,
and a road whose model lives in another process (a served engine, ADDENDUM 8)
reports 0 there while using the whole card -- so a zero is a fact about which
process allocated, never a signal about the device and never a failure. GPU
busy percent is what carries on every road, which is why it is the one
sampled here.
"""

from __future__ import annotations

import logging
import subprocess
import threading
import time
from collections.abc import Callable
from pathlib import Path
from typing import Any, NamedTuple

logger = logging.getLogger(__name__)

SYSFS_DRM = Path("/sys/class/drm")
PROC_STAT = Path("/proc/stat")

SAMPLE_INTERVAL_SECONDS = 1.0
# An hour of samples. A benchmark line is minutes, not hours, but a wedged
# runner must not cost memory as well as time.
MAX_SAMPLES = 4096


class Sample(NamedTuple):
    """One tick: when, and what each probe said (``None`` = cannot say)."""

    at: float
    gpu_pct: float | None
    cpu_pct: float | None


def gpu_busy_files(root: Path = SYSFS_DRM) -> list[Path]:
    """``cardN/device/gpu_busy_percent`` for every card, in card order.

    Card order is numeric (``card10`` after ``card2``) so that the index a
    device id names -- ``gpu:1`` -- picks the same card the runner's torch
    picked.
    """
    try:
        cards = [path for path in root.iterdir() if path.name.startswith("card")]
    except OSError:
        return []

    def _index(path: Path) -> tuple[int, str]:
        digits = path.name[4:]
        return (int(digits) if digits.isdigit() else 1 << 30, path.name)

    found = []
    for card in sorted(cards, key=_index):
        busy = card / "device" / "gpu_busy_percent"
        if busy.is_file():
            found.append(busy)
    return found


def read_percent_file(path: Path) -> float | None:
    """One integer percentage out of sysfs, or None when it will not read."""
    try:
        text = path.read_text(encoding="utf-8", errors="replace").strip()
    except OSError:
        return None
    try:
        return max(0.0, min(100.0, float(text)))
    except ValueError:
        return None


def nvidia_busy(index: int | None = None, timeout: float = 2.0) -> float | None:
    """``nvidia-smi``'s utilization for one card, or None if it cannot say."""
    command = ["nvidia-smi", "--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"]
    if index is not None:
        command += ["-i", str(int(index))]
    try:
        result = subprocess.run(  # noqa: S603 - fixed argv
            command, capture_output=True, text=True, timeout=timeout, check=False
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if result.returncode != 0:
        return None
    line = (result.stdout or "").strip().splitlines()
    if not line:
        return None
    try:
        return max(0.0, min(100.0, float(line[0].strip())))
    except ValueError:
        return None


def gpu_reader(
    index: int | None = None,
    *,
    root: Path = SYSFS_DRM,
    nvidia: Callable[[int | None], float | None] | None = None,
) -> Callable[[], float | None]:
    """A no-argument probe for the card this row runs on, or a probe of None.

    Resolved ONCE, at the start of a run: which file to read (or whether to
    shell out to ``nvidia-smi``) is a fact about the host, and re-deciding it
    every second would be a directory scan a second for nothing.
    """
    files = gpu_busy_files(root)
    if files:
        chosen = files[index] if index is not None and 0 <= index < len(files) else files[0]
        return lambda: read_percent_file(chosen)
    ask = nvidia or nvidia_busy
    if ask(index) is not None:
        return lambda: ask(index)
    return lambda: None


def cpu_totals(stat: Path = PROC_STAT) -> tuple[float, float] | None:
    """``(busy, total)`` jiffies off ``/proc/stat``'s aggregate line.

    ``iowait`` counts as idle: a core waiting on a disk is a core a pool
    could have used, which is exactly what the reader of this number wants
    to know.
    """
    try:
        with stat.open(encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if not line.startswith("cpu "):
                    continue
                fields = [float(value) for value in line.split()[1:]]
                if len(fields) < 4:
                    return None
                total = sum(fields)
                idle = fields[3] + (fields[4] if len(fields) > 4 else 0.0)
                return (total - idle, total)
    except (OSError, ValueError):
        return None
    return None


def cpu_reader(stat: Path = PROC_STAT) -> Callable[[], float | None]:
    """A probe that returns the CPU busy PERCENT since its own last call."""
    previous: list[tuple[float, float] | None] = [cpu_totals(stat)]

    def read() -> float | None:
        now = cpu_totals(stat)
        was = previous[0]
        previous[0] = now
        if now is None or was is None:
            return None
        busy = now[0] - was[0]
        total = now[1] - was[1]
        if total <= 0:
            return None
        return max(0.0, min(100.0, 100.0 * busy / total))

    return read


class UtilizationSampler:
    """1 Hz GPU/CPU busy for the life of a bench run.

    It samples CONTINUOUSLY and averages afterwards, because the window a
    trial is timed over is not known until the trial ends (the runner reports
    it with the trial). :meth:`means` then takes the samples inside that
    window and nothing else -- a mean that includes the model load in front
    of the first emission would answer a different question from the one the
    rate answers.
    """

    def __init__(
        self,
        *,
        gpu: Callable[[], float | None] | None = None,
        cpu: Callable[[], float | None] | None = None,
        interval: float = SAMPLE_INTERVAL_SECONDS,
        clock: Callable[[], float] = time.monotonic,
    ) -> None:
        self._gpu = gpu or (lambda: None)
        self._cpu = cpu or (lambda: None)
        self._interval = max(0.01, float(interval))
        self._clock = clock
        self._samples: list[Sample] = []
        self._lock = threading.Lock()
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    # -- the thread --------------------------------------------------------

    def start(self) -> UtilizationSampler:
        if self._thread is not None:
            return self
        self._thread = threading.Thread(target=self._run, name="ocr-bench-util", daemon=True)
        self._thread.start()
        return self

    def stop(self, *, wait: bool = False) -> None:
        """Stop sampling. The join is OPTIONAL and off by default.

        A benchmark stops its sampler on the way to reporting a result, and
        the thread it is stopping may be inside a probe (``nvidia-smi`` gives
        itself two seconds). Waiting for it there would put those seconds
        between "this benchmark is done" and the next one starting, for a
        daemon thread that does one file read and exits on its own.
        """
        self._stop.set()
        thread, self._thread = self._thread, None
        if wait and thread is not None:
            thread.join(timeout=self._interval * 2 + 1.0)

    def __enter__(self) -> UtilizationSampler:
        return self.start()

    def __exit__(self, *_exc: object) -> None:
        self.stop(wait=True)

    def _run(self) -> None:
        while not self._stop.is_set():
            self.sample()
            self._stop.wait(self._interval)

    def sample(self) -> Sample:
        """Take one tick now. Public so a test can drive the clock itself."""
        try:
            gpu = self._gpu()
        except Exception:  # pragma: no cover - a probe must never end a bench
            logger.debug("the GPU utilization probe failed", exc_info=True)
            gpu = None
        try:
            cpu = self._cpu()
        except Exception:  # pragma: no cover - ditto
            logger.debug("the CPU utilization probe failed", exc_info=True)
            cpu = None
        entry = Sample(self._clock(), gpu, cpu)
        with self._lock:
            self._samples.append(entry)
            if len(self._samples) > MAX_SAMPLES:
                del self._samples[: len(self._samples) - MAX_SAMPLES]
        return entry

    # -- reading it back ---------------------------------------------------

    @property
    def samples(self) -> list[Sample]:
        with self._lock:
            return list(self._samples)

    def means(self, first: float | None, last: float | None) -> dict[str, float | None]:
        """``{"gpu_busy_pct", "cpu_busy_pct"}`` over ``[first, last]``.

        ``None`` for either when nothing was sampled inside the window or the
        platform never answered: a window too short to contain a tick is a
        thing this machine cannot say, not a zero.
        """
        empty: dict[str, float | None] = {"gpu_busy_pct": None, "cpu_busy_pct": None}
        if first is None or last is None or last < first:
            return empty
        inside = [s for s in self.samples if first <= s.at <= last]
        return {
            "gpu_busy_pct": _mean(s.gpu_pct for s in inside),
            "cpu_busy_pct": _mean(s.cpu_pct for s in inside),
        }


def _mean(values: Any) -> float | None:
    numbers = [float(value) for value in values if value is not None]
    if not numbers:
        return None
    return round(sum(numbers) / len(numbers), 1)


def device_index(device: str | None) -> int | None:
    """The card number a ``gpu:<n>`` device id names, else None."""
    if not device:
        return None
    text = str(device)
    if text.startswith("gpu:") and text[4:].isdigit():
        return int(text[4:])
    return None


def sampler_for(
    device: str | None,
    *,
    root: Path = SYSFS_DRM,
    stat: Path = PROC_STAT,
    interval: float = SAMPLE_INTERVAL_SECONDS,
) -> UtilizationSampler:
    """A sampler aimed at the card this row's models were placed on."""
    return UtilizationSampler(
        gpu=gpu_reader(device_index(device), root=root),
        cpu=cpu_reader(stat),
        interval=interval,
    )


def first_gpu_device(pools: Any) -> str | None:
    """Which device a row's benchmark should watch: its engine's, else its stage's.

    The engine is where the FLOPs are (the detector is a fraction of the run
    and may be on the CPU by choice), and a monolithic row has exactly one
    stage. ``auto``/absent means "whatever torch picked", which is card 0 --
    the same default :func:`gpu_reader` falls back to.
    """
    devices = dict(getattr(pools, "stage_device", None) or {})
    for key in ("engine", "mokuro", "detect"):
        value = devices.get(key)
        if value and str(value) != "auto":
            return str(value)
    return None
