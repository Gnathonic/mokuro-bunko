"""The status a processor writes, and what ``processor status`` reads back."""

from __future__ import annotations

import json
import time
from pathlib import Path
from typing import Any


def status_path(storage: Path) -> Path:
    return Path(storage) / "processor-status.json"


def write_status(storage: Path, **fields: Any) -> None:
    """Last-known state, atomically, for `processor status` and for humans."""
    payload = {"updated_at": time.time(), **fields}
    path = status_path(storage)
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        tmp = path.with_name(path.name + ".tmp")
        tmp.write_text(json.dumps(payload, indent=2), encoding="utf-8")
        tmp.replace(path)
    except OSError:
        pass


def read_status(storage: Path) -> dict[str, Any]:
    try:
        return dict(json.loads(status_path(storage).read_text(encoding="utf-8")))
    except (OSError, ValueError):
        return {}


def describe(status: dict[str, Any]) -> str:
    if not status:
        return "never connected"
    state = status.get("state") or "unknown"
    library = status.get("library") or "?"
    sessions = status.get("sessions")
    detail = f" — {sessions} session(s)" if sessions else ""
    return f"{state} to {library}{detail}"
