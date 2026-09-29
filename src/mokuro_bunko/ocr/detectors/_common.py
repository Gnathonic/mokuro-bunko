"""Shared helpers for detector adapters.

Copied next to the adapter into the workspace at run time, so adapters
import it as a sibling module (``from _common import ...``). Pure Python plus
Pillow; no torch.

Two ways to run an adapter, ONE code path behind both
(:func:`run_adapter`): an adapter loads its models once and hands back a
per-page callable, and the scaffold either walks a ``--pages`` list (batch)
or serves pages one at a time off stdin (``--serve``). Batch is the CLI and
what the tests drive; serve is what the runner uses, so detection is a
streaming stage of the page pipeline instead of a whole-volume pass that
finishes before the recognizer is even built.

THE SERVE PROTOCOL, in full:

* the adapter loads its models, then prints
  ``@@detect ready <device>\t<weights as JSON>`` and reads stdin;
* one request a line, ``<image path>\t<JSON path to write>``, both
  ABSOLUTE. For each, the adapter writes that page's JSON exactly as batch
  mode does and answers ONE line: ``@@detect ok <note>\t<image path>`` or
  ``@@detect fail <message>\t<image path>``. The path comes LAST, after a
  tab, because **page names contain spaces** ("Some Series 20 -
  101.webp") and a reply the runner cannot split back into note and page is
  a desync it has to kill the process over;
* EOF on stdin ends the loop and the process -- which is also how a parent
  that died takes its detector with it, without a signal or a pid file.

**A served process knows nothing about a volume.** It is given the image to
read and the file to write, one request at a time, and it reports what it
loaded on the ready line rather than into some run's directory -- so the same
process can serve pages of one volume, then the next, for as long as there is
work for its model. ``--input`` and ``--output-dir`` belong to BATCH mode,
where a volume is exactly what the adapter is given.

Everything else the process prints (its own progress, a library's warning)
is not a reply and is forwarded to the volume's log by the runner; replies
are found by :data:`SERVE_PREFIX` anywhere in the line, so a library writing
a partial line to stderr cannot swallow one.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import sys
from collections.abc import Callable, Mapping, Sequence
from pathlib import Path
from typing import Any

# Part of the adapter contract (see README.md): one file per run, beside the
# per-page JSON, naming the model versions the adapter actually loaded. The
# runner merges it into the sidecar's ``ocr_engine.weights``, so a file can
# say what boxed its text as well as what read it. Named with a leading
# underscore so it cannot collide with a page called ``weights``.
WEIGHTS_FILE = "_weights.json"

# What marks a line on stdout as a PROTOCOL reply rather than a log line.
# Must match ``engine_runner.DETECTOR_REPLY_PREFIX``; the two cannot share a
# constant, because the adapter is executed by path in a process that has
# never imported the package.
SERVE_PREFIX = "@@detect "
SERVE_READY = "ready"
SERVE_OK = "ok"
SERVE_FAIL = "fail"
# What separates a reply's free text from the page it is about. A page name
# has spaces in it far more often than not, so the page goes LAST, after this,
# and everything following the FIRST one is the path -- verbatim.
SERVE_SEP = "\t"

# One page in, one page out: what an adapter's per-page callable returns is
# the page JSON of the contract plus the note its progress line carries
# (``blocks=7``, ``lines=31 passes=2``). It is given the image to read, and
# nothing else: which volume that page belongs to is not its business.
DetectFn = Callable[[Path], "tuple[dict[str, Any], str]"]


class AdapterError(Exception):
    """A refusal to run at all, reported as the adapter's one ERROR line."""


def wanted_device(args: argparse.Namespace, *, cpu_only: bool = False, tag: str = "") -> str | None:
    """Where ``--device`` says this adapter's model goes; None = probe as before.

    ``cuda``/``cuda:<n>`` and ``gpu``/``gpu:<n>`` are the same card written two
    ways -- the runner speaks the second and torch the first -- and what comes
    back is torch's spelling, because that is what an adapter hands its
    library. ``cpu_only`` is an adapter whose model cannot leave the CPU: it
    REFUSES a card here rather than loading somewhere the caller did not ask
    for, because a placement silently not honoured is a measurement of the
    wrong thing.
    """
    raw = str(getattr(args, "device", None) or "").strip().lower()
    if not raw or raw == "auto":
        return None
    if raw == "cpu":
        return "cpu"
    for prefix in ("cuda", "gpu"):
        if raw == prefix:
            index = "0"
            break
        if raw.startswith(f"{prefix}:"):
            index = raw.split(":", 1)[1]
            break
    else:
        raise AdapterError(f"--device {raw!r} is not a device (cpu, cuda or cuda:<n>)")
    if not index.isdigit():
        raise AdapterError(f"--device {raw!r} is not a device (cpu, cuda or cuda:<n>)")
    if cpu_only:
        raise AdapterError(
            f"this detector runs on the CPU (onnxruntime) and cannot be placed "
            f"on {raw}: set its device to cpu"
        )
    return f"cuda:{int(index)}"


class AdapterOutput:
    """What an adapter reports about ITSELF, once, when its models are loaded.

    Only the weights, for now. It exists because where they go depends on how
    the adapter was started and the adapter should not have to care: in batch
    mode they are the ``_weights.json`` of the contract, beside the pages; in
    serve mode there is no volume directory to put them in and they ride the
    ready line back to the runner instead.
    """

    def __init__(self, output_dir: Path | None) -> None:
        self.dir = output_dir
        self.reported: dict[str, str] = {}

    def weights(self, weights: Mapping[str, str]) -> None:
        self.reported = {str(k): str(v) for k, v in weights.items()}
        if self.dir is not None:
            write_weights_json(self.dir, self.reported)


# What an adapter does once: import, resolve and load its models, report its
# weights through :class:`AdapterOutput`, and hand back the per-page callable.
# Raising :class:`AdapterError` is how it refuses (a checkpoint that is not
# the pinned one).
SetupFn = Callable[[argparse.Namespace, AdapterOutput], DetectFn]


def parse_adapter_args(description: str, argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=description)
    parser.add_argument("--input", default=None, help="volume directory (batch mode)")
    parser.add_argument(
        "--pages", default=None, help="file listing page paths relative to --input"
    )
    parser.add_argument(
        "--output-dir", default=None, help="where per-page JSON goes (batch mode)"
    )
    parser.add_argument(
        "--serve",
        action="store_true",
        help="stay loaded and take one request a line on stdin (see the module docstring)",
    )
    parser.add_argument(
        "--device",
        default=None,
        help=(
            "where to put this adapter's model: 'cpu', 'cuda', 'cuda:<n>' (or "
            "'gpu:<n>', the same card). Default: whatever the adapter's own "
            "probe picks, which is what it has always done. An adapter whose "
            "model cannot leave the CPU refuses a card rather than quietly "
            "running somewhere else"
        ),
    )
    args = parser.parse_args(argv)
    # A volume is a BATCH-mode idea: --serve is told each page and each
    # destination one request at a time, so it needs neither.
    if not args.serve and not (args.pages and args.input and args.output_dir):
        parser.error("--input, --pages and --output-dir are required unless --serve is given")
    return args


def run_adapter(
    tag: str, description: str, setup: SetupFn, argv: Sequence[str] | None = None
) -> int:
    """Load this adapter's models once, then run it in batch or serve mode.

    The two modes share everything that decides what is WRITTEN -- the same
    ``setup``, the same per-page callable, the same :func:`write_page_json` --
    so a page's JSON cannot depend on which way the adapter was started.
    """
    args = parse_adapter_args(description, argv)
    out = AdapterOutput(None if args.serve else Path(args.output_dir))
    try:
        detect = setup(args, out)
    except AdapterError as e:
        print(f"[detector:{tag}] ERROR {e}", flush=True)
        return 1
    if args.serve:
        return serve_pages(tag, detect, out)
    return batch_pages(
        tag, Path(args.input), Path(args.output_dir), read_pages(Path(args.pages)), detect
    )


def batch_pages(
    tag: str, input_dir: Path, output_dir: Path, pages: Sequence[str], detect: DetectFn
) -> int:
    """Every page of the volume, in order, in this process. The CLI's mode."""
    failed = 0
    for idx, rel in enumerate(pages, 1):
        try:
            payload, note = detect(input_dir / rel)
            write_page_json(output_dir, rel, payload)
            print(f"[detector:{tag}] page {idx}/{len(pages)} {rel} {note}", flush=True)
        except Exception as e:  # keep going; the runner treats missing JSON as a failed page
            failed += 1
            print(f"[detector:{tag}] ERROR page {rel}: {e}", flush=True)
    print(f"[detector:{tag}] done pages={len(pages)} failed={failed}", flush=True)
    return 0 if failed < len(pages) else 1


def serve_pages(tag: str, detect: DetectFn, out: AdapterOutput) -> int:
    """One request a line on stdin, one reply a line on stdout, until EOF.

    A request is ``<image>\t<JSON to write>``, both absolute: this process
    outlives any one volume, so every request stands alone.

    A page that fails is ANSWERED rather than raised: the runner surfaces it
    at that page's own place in the volume, which is where a bad page has
    always been handled, and the process stays up for the next one.
    """
    ready = f"{SERVE_PREFIX}{SERVE_READY} {getattr(detect, 'device', '') or 'cpu'}"
    print(f"{ready}{SERVE_SEP}{json.dumps(out.reported, ensure_ascii=False, sort_keys=True)}",
          flush=True)  # fmt: skip
    for raw in sys.stdin:
        request = raw.rstrip("\n")
        if not request.strip():
            continue
        image, _, destination = request.partition(SERVE_SEP)
        try:
            if not destination:
                raise ValueError(f"malformed request {request!r}")
            payload, note = detect(Path(image))
            target = Path(destination)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
        except Exception as e:
            reply = f"{SERVE_FAIL} {one_line(str(e)) or type(e).__name__}"
        else:
            reply = f"{SERVE_OK} {one_line(note)}"
        print(f"{SERVE_PREFIX}{reply}{SERVE_SEP}{image}", flush=True)
    return 0


def one_line(text: str) -> str:
    """A reply is one line and one field, so newlines and tabs are folded out."""
    return " | ".join(
        part.strip() for part in str(text).replace(SERVE_SEP, " ").splitlines() if part.strip()
    )


def read_pages(list_file: Path) -> list[str]:
    return [
        line.rstrip("\n")
        for line in list_file.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]


def write_page_json(output_dir: Path, rel_page: str, payload: dict[str, Any]) -> Path:
    out = (output_dir / rel_page).with_suffix(".json")
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
    return out


def write_weights_json(output_dir: Path, weights: Mapping[str, str]) -> Path:
    """Report the model versions this adapter loaded, for the sidecar.

    ``weights`` maps a source -- a Hugging Face repo id, or a release asset
    for weights that are not on the Hub -- to the revision or digest it was
    pinned to. Written once, after the models are resolved and before the
    pages are read, so a crashed run still says what it had loaded.
    """
    out = Path(output_dir)
    out.mkdir(parents=True, exist_ok=True)
    path = out / WEIGHTS_FILE
    path.write_text(
        json.dumps(
            {str(k): str(v) for k, v in weights.items()}, ensure_ascii=False, sort_keys=True
        ),
        encoding="utf-8",
    )
    return path


def file_sha256(path: Path, chunk: int = 1 << 20) -> str:
    """Digest of a model file, for weights that are not a pinned Hub repo."""
    digest = hashlib.sha256()
    with Path(path).open("rb") as fh:
        while block := fh.read(chunk):
            digest.update(block)
    return digest.hexdigest()


def box_iou(a: Sequence[float], b: Sequence[float]) -> float:
    ix1, iy1 = max(a[0], b[0]), max(a[1], b[1])
    ix2, iy2 = min(a[2], b[2]), min(a[3], b[3])
    inter = max(0.0, ix2 - ix1) * max(0.0, iy2 - iy1)
    union = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1]) - inter
    return inter / union if union > 0 else 0.0


def dedupe_boxes(
    boxes: Sequence[Sequence[float]], scores: Sequence[float], iou_threshold: float = 0.6
) -> list[int]:
    """Indices to keep after class-agnostic NMS (highest score wins).

    Detectors with several text classes can fire twice on one region
    (e.g. ``text_bubble`` and ``text_free``); the schema wants one block.
    """
    order = sorted(range(len(boxes)), key=lambda i: -scores[i])
    kept: list[int] = []
    for i in order:
        if all(box_iou(boxes[i], boxes[j]) < iou_threshold for j in kept):
            kept.append(i)
    return sorted(kept)


def drop_containers(
    boxes: Sequence[Sequence[float]], min_parts: int = 2, inside: float = 0.8
) -> list[int]:
    """Indices to keep after removing boxes that merely group other boxes.

    A detector can emit one box around two bubbles and also each bubble on
    its own. When a box holds at least ``min_parts`` other, smaller boxes
    (each with ``inside`` of its area within it), the parts are the better
    layout and the container is dropped.
    """

    def area(b: Sequence[float]) -> float:
        return max(0.0, b[2] - b[0]) * max(0.0, b[3] - b[1])

    def inside_frac(part: Sequence[float], box: Sequence[float]) -> float:
        ix1, iy1 = max(part[0], box[0]), max(part[1], box[1])
        ix2, iy2 = min(part[2], box[2]), min(part[3], box[3])
        inter = max(0.0, ix2 - ix1) * max(0.0, iy2 - iy1)
        return inter / area(part) if area(part) > 0 else 0.0

    kept = []
    for i, box in enumerate(boxes):
        parts = sum(
            1
            for j, other in enumerate(boxes)
            if j != i and area(other) < area(box) * 0.9 and inside_frac(other, box) >= inside
        )
        if parts < min_parts:
            kept.append(i)
    return kept


def block_quad(box: Sequence[float]) -> list[list[float]]:
    """Axis-aligned quad for a box, in the corner order the schema uses."""
    x1, y1, x2, y2 = (float(v) for v in box)
    return [[x1, y1], [x2, y1], [x2, y2], [x1, y2]]


def estimate_font_size(gray_crop: Any, vertical: bool) -> int:
    """Estimate glyph size from ink runs of a grayscale PIL crop.

    Projects ink onto columns (vertical text) or rows (horizontal text),
    splits into runs, merges small gaps, and returns the median width of the
    dominant runs. Falls back to a fraction of the crop size when no runs are
    found (blank or very small crops).
    """
    width, height = gray_crop.size
    if width < 4 or height < 4:
        return int(max(8, min(width, height)))
    pixels = gray_crop.load()
    # Otsu-free threshold: ink is darker than the crop's mid-tone.
    values = list(gray_crop.getdata())
    lo, hi = min(values), max(values)
    thresh = lo + (hi - lo) * 0.5 if hi > lo else 0
    if vertical:
        profile = [
            sum(1 for y in range(height) if pixels[x, y] < thresh) / height for x in range(width)
        ]
    else:
        profile = [
            sum(1 for x in range(width) if pixels[x, y] < thresh) / width for y in range(height)
        ]
    runs: list[list[int]] = []
    for i, v in enumerate(profile):
        if v > 0.02:
            if runs and i - runs[-1][1] <= 1:
                runs[-1][1] = i + 1
            else:
                runs.append([i, i + 1])
    if not runs:
        return max(8, int((width if vertical else height) * 0.6))
    # Merge gaps narrower than 20% of the widest run (strokes inside a glyph column).
    widest = max(e - s for s, e in runs)
    merged: list[list[int]] = []
    for s, e in runs:
        if merged and s - merged[-1][1] < widest * 0.2:
            merged[-1][1] = e
        else:
            merged.append([s, e])
    widths = sorted(e - s for s, e in merged)
    widest = widths[-1]
    dominant = [w for w in widths if w >= widest * 0.5] or widths
    return max(8, int(dominant[len(dominant) // 2]))
