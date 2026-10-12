"""pytest plugin: record every line_layout / line_reconcile call the 0.5.2
unit tests make, with its result, for replay against the Rust port.

    GOLDEN_CAPTURE=out.json python -m pytest -p capture_plugin tests/unit/...

(``gen_golden.py`` runs this; the cases are the tests' own hand-made edge
cases, which is why they are worth harvesting.)
"""

from __future__ import annotations

import copy
import functools
import inspect
import json
import os
from typing import Any

from golden_common import jsonable, layout_result, reconciled_state

CASES: dict[str, dict[str, Any]] = {}
_DEPTH = [0]

RECONCILE_FNS = [
    "reconcile_line", "settle_disputes", "engine_only_verdict", "corroborates", "needs_second_read",
    "widen_punctuation", "repeated_tail", "engine_looped", "is_runaway", "line_cells", "axis_cells",
    "region_cells", "is_region", "token_cap", "overlap_repeat", "fold", "page_summary",
]  # fmt: skip
LAYOUT_FNS = ["layout_page", "column_pieces", "normalize_text", "is_ruby_script"]


def _encode(name: str, value: Any, lr: Any) -> Any:
    if isinstance(value, lr.Reconciled):
        return {"__reconciled__": reconciled_state(value)}
    if isinstance(value, (list, tuple)) and value and isinstance(value[0], lr.Reconciled):
        return [{"__reconciled__": reconciled_state(v)} for v in value]
    return value


def _record(kind: str, fn_name: str, args: dict[str, Any], result: Any) -> None:
    case = {"fn": fn_name, "args": args, "result": result}
    if not jsonable(case):
        return
    key = json.dumps(case, ensure_ascii=False, sort_keys=True)
    CASES.setdefault(key, case)


def _wrap(module: Any, name: str, kind: str, lr: Any, ll: Any) -> None:
    fn = getattr(module, name)
    sig = inspect.signature(fn)

    @functools.wraps(fn)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        try:
            bound = sig.bind(*args, **kwargs)
            bound.apply_defaults()
            before = {k: _encode(k, copy.deepcopy(v), lr) for k, v in bound.arguments.items()}
        except Exception:
            return fn(*args, **kwargs)
        result = fn(*args, **kwargs)
        try:
            if name == "layout_page":
                out = layout_result(ll, result)
            elif name == "column_pieces":
                out = result
            else:
                out = _encode(name, result, lr)
            _record(kind, name, before, out)
        except Exception:
            pass
        return result

    setattr(module, name, wrapper)


def pytest_configure(config: Any) -> None:
    from mokuro_bunko.ocr import line_layout as ll
    from mokuro_bunko.ocr import line_reconcile as lr

    for name in RECONCILE_FNS:
        _wrap(lr, name, "reconcile", lr, ll)
    for name in LAYOUT_FNS:
        _wrap(ll, name, "layout", lr, ll)


def pytest_unconfigure(config: Any) -> None:
    out = os.environ.get("GOLDEN_CAPTURE")
    if out:
        with open(out, "w", encoding="utf-8") as f:
            json.dump(list(CASES.values()), f, ensure_ascii=False)
