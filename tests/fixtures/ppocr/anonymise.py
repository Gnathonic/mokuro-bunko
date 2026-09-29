# This Source Code Form is subject to the terms of the Mozilla Public License,
# v. 2.0. If a copy of the MPL was not distributed with this file, You can
# obtain one at https://mozilla.org/MPL/2.0/.
"""Anonymise raw ``ppocr-lines/1`` pages before they become test fixtures.

Why: the fixtures are recognizer output for whole pages of commercial books,
and this repository is public. The layout rules under test read geometry,
script class, brackets and punctuation, glyph counts and confidences, and a
handful of glyphs they name (the lookalike and small-kana tables) -- never the
wording. So no word of a page survives: every kanji, hiragana and katakana is
replaced, glyph for glyph, by a stand-in of its own class (small kana by small
kana), through a table drawn at random for the run and never stored. The same
glyph always gets the same stand-in, so two different words stay different and
a probe's misread stays a misread of its line; nothing maps back. Kept as they
are: the glyphs the layout code names in its own string constants, the older
placeholders 漢 / あ / ア / っ / ッ, and everything that is not a letter
(punctuation, brackets, digits, Latin, ー, ―, spaces).

A test asserts on the fixture as written -- stand-ins and all -- never on the
book's text. Probe fixtures (``ppocr-probes/1``) go through the same table.

usage: anonymise.py <raw page dir> <fixture dir>
"""

from __future__ import annotations

import ast
import json
import os
import random
import sys
from pathlib import Path

CODE = Path(__file__).resolve().parents[3] / "src" / "mokuro_bunko" / "ocr"
NAMING = ("line_layout.py", "ppocr.py", "line_reconcile.py")
PLACEHOLDERS = "漢あアっッ々〆〇"
SMALL_HIRAGANA = "ぁぃぅぇぉっゃゅょゎゕゖ"
SMALL_KATAKANA = "ァィゥェォッャュョヮヵヶ"
CLASSES = ("kanji", "hiragana", "small hiragana", "katakana", "small katakana")


def glyph_class(ch: str) -> str | None:
    """``kanji``, ``hiragana``, ``small hiragana``, ... or None for a non-letter."""
    o = ord(ch)
    if 0x4E00 <= o <= 0x9FFF or 0x3400 <= o <= 0x4DBF or 0xF900 <= o <= 0xFAFF:
        return "kanji"
    if ch in SMALL_HIRAGANA:
        return "small hiragana"
    if 0x3041 <= o <= 0x3096:
        return "hiragana"
    if ch in SMALL_KATAKANA:
        return "small katakana"
    if 0x30A1 <= o <= 0x30FA:
        return "katakana"
    return None


def named_glyphs() -> set[str]:
    """Letters the layout code spells out in its own string constants."""
    named = set(PLACEHOLDERS)
    for name in NAMING:
        tree = ast.parse((CODE / name).read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                named |= {ch for ch in node.value if glyph_class(ch)}
    return named


def stand_in_table(texts: list[str], keep: set[str], rng: random.Random) -> dict[str, str]:
    """A one-to-one stand-in for every letter of ``texts`` not in ``keep``."""
    table: dict[str, str] = {}
    for cls in CLASSES:
        glyphs = sorted({ch for text in texts for ch in text if glyph_class(ch) == cls} - keep)
        if cls == "kanji":
            span = range(0x4E00, 0x9FA6)
        elif "hiragana" in cls:
            span = range(0x3041, 0x3097)
        else:
            span = range(0x30A1, 0x30FB)
        pool = [chr(o) for o in span if glyph_class(chr(o)) == cls and chr(o) not in keep]
        table.update(zip(glyphs, rng.sample(pool, len(glyphs)), strict=True))
    return table


def page_texts(page: dict) -> list[str]:
    if page.get("format") == "ppocr-lines/1":
        return [line["text"] for line in page["lines"]]
    if page.get("format") == "ppocr-probes/1":
        return [probe[0] for entry in page["reads"] for probe in entry["probes"]]
    return []


def main(argv: list[str]) -> int:
    source, target = Path(argv[0]), Path(argv[1])
    pages = {p: json.loads(p.read_text(encoding="utf-8")) for p in sorted(source.glob("*.json"))}
    texts = [text for page in pages.values() for text in page_texts(page)]
    table = stand_in_table(texts, named_glyphs(), random.Random(os.urandom(32)))

    def scrub(text: str) -> str:
        return "".join(table.get(ch, ch) for ch in text)

    for path, page in pages.items():
        if page.get("format") == "ppocr-lines/1":
            for line in page["lines"]:
                line["text"] = scrub(line["text"])
        elif page.get("format") == "ppocr-probes/1":
            page.pop("source", None)
            for entry in page["reads"]:
                for probe in entry["probes"]:
                    probe[0] = scrub(probe[0])
        else:
            continue
        (target / path.name).write_text(
            json.dumps(page, ensure_ascii=False, indent=1) + "\n", encoding="utf-8"
        )
    print(f"{len(table)} glyphs replaced; the table is not kept")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
