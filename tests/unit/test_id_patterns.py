"""Name and id patterns refuse a trailing newline.

``$`` also matches just before a final ``\\n``, so ``"g-1\\n"`` read as a
valid id and went on to name a workspace directory and a sidecar postfix.
"""

from __future__ import annotations

import pytest

from mokuro_bunko.ocr.bench import is_draft_key
from mokuro_bunko.ocr.generations import (
    GENERATION_ID_RE,
    GENERATION_NAME_RE,
    LAYER_ID_RE,
    name_rejection,
)


@pytest.mark.parametrize("pattern", [GENERATION_ID_RE, LAYER_ID_RE])
def test_a_trailing_newline_is_refused(pattern) -> None:  # type: ignore[no-untyped-def]
    assert pattern.match("g-1")
    assert not pattern.match("g-1\n")


def test_a_draft_key_with_a_trailing_newline_is_not_a_draft() -> None:
    assert is_draft_key("draft-abc")
    assert not is_draft_key("draft-abc\n")


def test_a_generation_name_with_a_trailing_newline_is_refused() -> None:
    assert name_rejection("hayai") is None
    assert name_rejection("hayai\n") is not None


def test_the_published_name_pattern_is_one_javascript_can_compile() -> None:
    # The admin UI builds `new RegExp(name_pattern)`; `\\Z` is not JavaScript.
    assert "\\Z" not in GENERATION_NAME_RE.pattern
