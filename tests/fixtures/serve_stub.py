"""A fake serve-mode engine: the fork's protocol, none of the models.

``python -m serve_stub`` speaks exactly what ``mokuro/serve.py`` speaks --
``begin`` / ``page`` / ``end`` / ``quit`` in, ``ready`` / ``page`` /
``page_failed`` / ``volume_done`` / ``fatal`` out, one JSON object a line each
way -- so the client can be tested against the protocol rather than against a
re-implementation of the client's own idea of it.

It ENFORCES the contract rather than assuming the caller keeps it:

* a ``page`` outside a volume, or an index that is not exactly the number of
  pages already sent for this volume, is a ``fatal`` and exit 1 -- so a test
  that reorders wrongly fails loudly instead of quietly producing the right
  pages in the wrong places;
* more than ``window`` pages outstanding at once is a ``fatal`` too, which is
  the only way to prove the caller honours the window.

Every knob is an environment variable, because the thing under test spawns it:

``SERVE_STUB_WINDOW``     what the ready line promises (default 4)
``SERVE_STUB_DELAY``      seconds a page takes (default 0)
``SERVE_STUB_HOLD``       answer in batches of N, in REVERSE (default 1): the
                          engine may finish pages out of order, and the client
                          has to put them back
``SERVE_STUB_FAIL``       comma-separated page CONTENTS to answer
                          ``page_failed`` for
``SERVE_STUB_FATAL_AFTER`` die with a ``fatal`` after this many pages
``SERVE_STUB_EXIT_AFTER``  die WITHOUT a fatal after this many pages
``SERVE_STUB_NO_READY``    never print a ready line
``SERVE_STUB_RECORD``      a JSON file to write what it saw: every op, and the
                           high-water mark of outstanding pages
``SERVE_STUB_VERSION``     the version it stamps pages and reports (default
                           ``0.0.stub``)

A page's ``result`` carries the file's own text, so a test can tell which file
the engine was handed and prove the spool is what it read.
"""

from __future__ import annotations

import json
import os
import sys
import threading
import time
from pathlib import Path

VERSION = os.environ.get("SERVE_STUB_VERSION", "0.0.stub")
WINDOW = int(os.environ.get("SERVE_STUB_WINDOW", "4"))
DELAY = float(os.environ.get("SERVE_STUB_DELAY", "0"))
HOLD = max(1, int(os.environ.get("SERVE_STUB_HOLD", "1")))
FAIL = {p for p in os.environ.get("SERVE_STUB_FAIL", "").split(",") if p}
FATAL_AFTER = int(os.environ.get("SERVE_STUB_FATAL_AFTER", "0"))
EXIT_AFTER = int(os.environ.get("SERVE_STUB_EXIT_AFTER", "0"))
RECORD = os.environ.get("SERVE_STUB_RECORD", "")

_out = sys.stdout
_lock = threading.Lock()
_seen: list[dict] = []
_high_water = 0


def emit(event: dict) -> None:
    with _lock:
        _out.write(json.dumps(event) + "\n")
        _out.flush()


def note(entry: dict) -> None:
    if not RECORD:
        return
    with _lock:
        _seen.append(entry)


def save() -> None:
    if not RECORD:
        return
    with _lock:
        Path(RECORD).write_text(
            json.dumps({"ops": _seen, "max_outstanding": _high_water}, ensure_ascii=False),
            encoding="utf-8",
        )


def die(message: str) -> None:
    emit({"event": "fatal", "error": message})
    save()
    os._exit(1)


def answer(index: int, path: str) -> dict:
    try:
        text = Path(path).read_text(encoding="utf-8", errors="replace")
    except OSError as e:
        return {"event": "page_failed", "index": index, "stage": "load", "error": str(e)}
    if text in FAIL:
        return {"event": "page_failed", "index": index, "stage": "OCR", "error": f"refused {text}"}
    return {
        "event": "page",
        "index": index,
        "result": {
            "version": VERSION,
            "img_width": 100 + index,
            "img_height": 200,
            "blocks": [{"box": [0, 0, 1, 1], "vertical": True, "font_size": 8, "lines": [text]}],
        },
    }


def main() -> int:
    global _high_water
    if not os.environ.get("SERVE_STUB_NO_READY"):
        emit(
            {
                "event": "ready",
                "version": VERSION,
                "device": os.environ.get("SERVE_STUB_DEVICE", "cpu"),
                "num_workers": int(os.environ.get("SERVE_STUB_WORKERS", "0")),
                "window": WINDOW,
            }
        )
    volume: str | None = None
    sent = 0  # pages this volume has been given
    failed = 0
    answered = 0
    total = 0  # pages over the whole process, for the fatal/exit knobs
    held: list[dict] = []

    def flush(force: bool = False) -> None:
        nonlocal held, failed, answered
        if not held or (len(held) < HOLD and not force):
            return
        # Reverse: a real engine finishes out of order and the caller must
        # reorder by index.
        for event in reversed(held):
            if event["event"] == "page_failed":
                failed += 1
            answered += 1
            emit(event)
        held = []

    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            op = json.loads(raw)
        except ValueError as e:
            die(f"not JSON: {e}")
        note(op)
        kind = op.get("op")
        if kind == "begin":
            if volume is not None:
                die("begin while a volume is still open; send end first")
            volume, sent, failed, answered = str(op.get("volume")), 0, 0, 0
        elif kind == "page":
            if volume is None:
                die("page outside a volume; send begin first")
            index = op.get("index")
            if index != sent:
                die(f"page index {index!r}: expected {sent} (strictly increasing from 0)")
            # Sent minus answered, counting the page being accepted now.
            outstanding = sent + 1 - answered
            _high_water = max(_high_water, outstanding)
            if outstanding > WINDOW:
                die(f"{outstanding} pages outstanding, window is {WINDOW}")
            sent += 1
            total += 1
            if DELAY:
                time.sleep(DELAY)
            held.append(answer(index, str(op.get("path"))))
            flush()
            if FATAL_AFTER and total >= FATAL_AFTER:
                die("the stub was told to fail here")
            if EXIT_AFTER and total >= EXIT_AFTER:
                save()
                os._exit(3)
        elif kind == "end":
            if volume is None:
                die("end outside a volume")
            flush(force=True)
            emit(
                {
                    "event": "volume_done",
                    "volume": volume,
                    "pages": sent,
                    "failed": failed,
                    "seconds": 0.0,
                }
            )
            volume = None
        elif kind == "quit":
            break
        else:
            die(f"unknown op {kind!r}")
    flush(force=True)
    save()
    return 0


if __name__ == "__main__":
    sys.exit(main())
