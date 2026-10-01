"""Shared helpers for the bunko-layout golden generators (Python 0.5.2 side).

Floats go into the fixtures as plain JSON numbers: ``json.dumps`` writes
``repr`` (shortest round-trip), and the Rust side parses with a correctly
rounded parser, so every float compares bit for bit.
"""

from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]


def reconciled_state(r: Any) -> dict[str, Any]:
    return {
        "text": r.text,
        "vlm": r.vlm,
        "ctc": r.ctc,
        "agreement": r.agreement,
        "source": r.source,
        "notes": list(r.notes),
        "second": r.second,
        "engine_only": bool(r.engine_only),
        "confirmed": bool(r.confirmed),
    }


def reconciled_from_state(lr: Any, s: dict[str, Any]) -> Any:
    return lr.Reconciled(
        s["text"], s["vlm"], s["ctc"], s["agreement"], s["source"], list(s["notes"]),
        s["second"], s["engine_only"], s["confirmed"],
    )  # fmt: skip


def layout_result(ll: Any, result: Any) -> dict[str, Any]:
    return {
        "blocks": result.blocks,
        "groups": result.groups,
        "kinds": result.kinds,
        "ruby": [
            {
                "line": r.line,
                "base": r.base,
                "text": r.text,
                "quad": [list(p) for p in r.quad],
                "span": list(r.span),
                "chars": list(r.chars),
            }
            for r in result.ruby
        ],
        "dropped": result.dropped,
        "bodies": [
            {
                "vertical": b.vertical,
                "theta": b.theta,
                "em": b.em,
                "top": b.top,
                "bottom": b.bottom,
                "cross0": b.cross0,
                "cross1": b.cross1,
                "gap": b.gap,
                "members": sorted(b.members),
            }
            for b in result.bodies
        ],
    }


def jsonable(x: Any) -> bool:
    try:
        json.dumps(x, allow_nan=True)
        return True
    except (TypeError, ValueError):
        return False


def finite(x: Any) -> bool:
    return not (isinstance(x, float) and not math.isfinite(x))


def write_json(path: Path, obj: Any) -> None:
    """Write ``<path>.gz`` (gzip, mtime 0 so re-runs are byte-identical)."""
    import gzip

    path = path.with_name(path.name + ".gz")
    path.parent.mkdir(parents=True, exist_ok=True)
    data = json.dumps(obj, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    with open(path, "wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=0, compresslevel=9) as f:
        f.write(data)
    print(f"wrote {path.relative_to(ROOT)} ({path.stat().st_size} bytes, {len(data)} raw)")
