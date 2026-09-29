"""Stand-in recognizers an engine PROCESS can load (``EngineProcess(load=...)``).

Module-level functions, because ``spawn`` pickles the loader by reference and
the child imports it from here.
"""

from __future__ import annotations

import os
from typing import Any


class _Echo:
    repos = {"fake/recognizer": "b" * 40}

    def __init__(self, engine: str) -> None:
        self.engine = engine

    def __call__(self, crops: list[Any], max_tokens: Any = None) -> list[str]:
        if "boom" in crops:
            raise ValueError("a crop the model cannot read")
        if "die" in crops:
            os._exit(3)
        suffix = "" if max_tokens is None else f"/{max_tokens[0]}"
        return [f"{crop}@{os.getpid()}{suffix}" for crop in crops]


def load_echo(engine: str, **_kwargs: Any) -> _Echo:
    return _Echo(engine)


def load_boom(engine: str, **_kwargs: Any) -> Any:
    raise RuntimeError("no such model")


class _Precise(_Echo):
    """An echo that says which precision it was asked to load at."""

    def __init__(self, engine: str, precision: str | None) -> None:
        super().__init__(engine)
        self.precision = precision


def load_precise(engine: str, **kwargs: Any) -> _Precise:
    return _Precise(engine, kwargs.get("precision"))
