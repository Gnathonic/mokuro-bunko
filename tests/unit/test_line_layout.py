"""Unit tests for the line-layout geometry (``ocr/line_layout.py``).

Two kinds of input, neither needing a model, an image or the network:

* synthetic pages built from boxes, where each rule is exercised on its own;
* real raw page outputs of the PP-OCRv6 manga engine cached under
  ``tests/fixtures/ppocr`` (the way Manatan tests its merger against cached
  engine output). Those pin what the rules must do on real novel and manga
  pages: every ruby run gone, no base text lost, bubbles not fused, paragraphs
  found. The expectations were checked by eye against the page images when
  the fixtures were recorded (2026-09-19).
"""

from __future__ import annotations

import json
import math
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import line_layout as ll

FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "ppocr"
NOVEL_TEXT = (
    "novel-text-013",
    "novel-text-097",
    "novel-text-250",
    "novel-dialogue-019",
    "novel-dialogue-200",
    "novel-section-024",
)
MANGA = ("manga-page-066", "manga-page-069")
ILLUSTRATED = ("novel-cover-001", "novel-illustration-069")
ALL_FIXTURES = (*NOVEL_TEXT, *MANGA, "novel-map-006", "novel-frontmatter-005", *ILLUSTRATED)

EM = 60.0


# --------------------------------------------------------------------------
# builders
# --------------------------------------------------------------------------


def load(name: str) -> dict[str, Any]:
    return json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf-8"))


def box(x0: float, y0: float, x1: float, y1: float) -> list[list[float]]:
    return [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]


def raw(quad: list[list[float]], text: str, conf: float = 0.99) -> dict[str, Any]:
    return {"quad": quad, "text": text, "conf": conf, "score": 0.9}


def vline(x: float, y0: float, y1: float, text: str, t: float = EM, conf: float = 0.99) -> dict:
    """A vertical column centred on ``x``."""
    return raw(box(x - t / 2, y0, x + t / 2, y1), text, conf)


def hline(y: float, x0: float, x1: float, text: str, t: float = EM, conf: float = 0.99) -> dict:
    """A horizontal row centred on ``y``."""
    return raw(box(x0, y - t / 2, x1, y + t / 2), text, conf)


def column(x: float, y0: float, glyphs: int, text: str | None = None, t: float = EM) -> dict:
    """A vertical column of ``glyphs`` full-size glyphs starting at ``y0``."""
    body = text if text is not None else "漢" + "あ" * (glyphs - 1)
    return vline(x, y0, y0 + glyphs * t, body, t)


def rotated(line: dict[str, Any], degrees: float, about: tuple[float, float]) -> dict[str, Any]:
    c, s = math.cos(math.radians(degrees)), math.sin(math.radians(degrees))
    quad = [
        [
            about[0] + (x - about[0]) * c - (y - about[1]) * s,
            about[1] + (x - about[0]) * s + (y - about[1]) * c,
        ]
        for x, y in line["quad"]
    ]
    return {**line, "quad": quad}


def page(lines: list[dict[str, Any]], width: int = 2000, height: int = 2800) -> dict[str, Any]:
    return {"width": width, "height": height, "lines": lines}


def measured(raws: list[dict[str, Any]]) -> list[ll.Line]:
    lines, _ = ll.measure_lines(raws)
    ll.decide_orientations(lines)
    return lines


def texts(layout: ll.PageLayout) -> list[list[str]]:
    return [block["lines"] for block in layout.blocks]


def lines_bounds(block: dict[str, Any]) -> list[int]:
    xs = [p[0] for quad in block["lines_coords"] for p in quad]
    ys = [p[1] for quad in block["lines_coords"] for p in quad]
    return [min(xs), min(ys), max(xs), max(ys)]


def quad_samples(quad: list[list[int]], across: int = 5, along: int = 80) -> list[ll.Point]:
    """A grid of points over a quad (corner order: tl, tr, br, bl), edges included."""
    (ax, ay), (bx, by), (cx, cy), (dx, dy) = quad
    points = []
    for i in range(across + 1):
        for j in range(along + 1):
            u, v = i / across, j / along
            top = (ax + (bx - ax) * u, ay + (by - ay) * u)
            bottom = (dx + (cx - dx) * u, dy + (cy - dy) * u)
            points.append((top[0] + (bottom[0] - top[0]) * v, top[1] + (bottom[1] - top[1]) * v))
    return points


def grown_blocks(layout: ll.PageLayout) -> list[int]:
    """Blocks whose box reaches past its lines (by more than the rounding)."""
    found = []
    for k, block in enumerate(layout.blocks):
        (x0, y0, x1, y1), (lx0, ly0, lx1, ly1) = block["box"], lines_bounds(block)
        if max(lx0 - x0, ly0 - y0, x1 - lx1, y1 - ly1) > 1:
            found.append(k)
    return found


def holds_run(layout: ll.PageLayout, run: ll.Ruby) -> bool:
    """Is the run inside the box of the block its base line went to?"""
    (k,) = [k for k, group in enumerate(layout.groups) if run.base in group]
    x0, y0, x1, y1 = layout.blocks[k]["box"]
    return all(x0 - 1 <= x <= x1 + 1 and y0 - 1 <= y <= y1 + 1 for x, y in run.quad)


BODY_TOP, BODY_BOTTOM, PITCH = 200.0, 2600.0, 100.0


def novel_page(columns: list[tuple[float, float, str]], extra: list[dict] | None = None) -> dict:
    """A one-tier novel page: columns right to left, 40 em tall, 100 px pitch.

    Each column is ``(inset_em, short_em, text)``: how far below the body's
    top it starts and how far above the bottom it ends.
    """
    lines = []
    for k, (inset, short, text) in enumerate(columns):
        x = 1800 - k * PITCH
        lines.append(vline(x, BODY_TOP + inset * EM, BODY_BOTTOM - short * EM, text))
    return page(lines + (extra or []))


FULL = "漢字の続く長い文章がここに入る"


# --------------------------------------------------------------------------
# 1. metrics and orientation
# --------------------------------------------------------------------------


class TestCanonicalQuad:
    def test_upright_box_is_unchanged(self) -> None:
        quad = box(10, 20, 70, 500)
        assert ll.canonical_quad(quad) == tuple((float(x), float(y)) for x, y in quad)

    def test_is_idempotent_on_a_tilted_quad(self) -> None:
        quad = rotated(vline(500, 100, 900, "あ"), 20, (500, 500))["quad"]
        once = ll.canonical_quad(quad)
        assert once is not None
        assert ll.canonical_quad([list(p) for p in once]) == once

    def test_paddle_ordering_of_a_leaning_column_is_repaired(self) -> None:
        # PaddleOCR sorts corners by x then y; on a column leaning 30 degrees
        # that starts from the BOTTOM-left corner, i.e. "horizontal at -60".
        true_quad = rotated(vline(500, 100, 900, "あ", t=40), 30, (500, 500))["quad"]
        by_x = sorted(true_quad, key=lambda p: p[0])
        (tl, bl), (tr, br) = (
            sorted(by_x[:2], key=lambda p: p[1]),
            sorted(by_x[2:], key=lambda p: p[1]),
        )
        paddle = [tl, tr, br, bl]
        assert ll.quad_frame([tuple(p) for p in paddle])[2] == pytest.approx(-60, abs=0.5)
        fixed = ll.canonical_quad(paddle)
        assert fixed is not None
        width, height, angle = ll.quad_frame(fixed)
        assert (width, height) == (pytest.approx(40, abs=0.01), pytest.approx(800, abs=0.01))
        assert angle == pytest.approx(30, abs=0.01)

    def test_counter_clockwise_winding_is_flipped(self) -> None:
        quad = box(0, 0, 100, 30)
        fixed = ll.canonical_quad(list(reversed(quad)))
        assert fixed == tuple((float(x), float(y)) for x, y in quad)

    def test_degenerate_quads_are_rejected(self) -> None:
        assert ll.canonical_quad([[0, 0], [1, 1]]) is None
        assert ll.canonical_quad([[0, 0], [10, 0], [20, 0], [30, 0]]) is None


class TestLineMetrics:
    def test_extents_follow_the_quad_not_its_bounding_box(self) -> None:
        tilted = rotated(vline(500, 100, 900, "漢字かな交じり文"), 25, (500, 500))
        (line,) = measured([tilted])
        assert line.width == pytest.approx(EM, abs=0.01)
        assert line.height == pytest.approx(800, abs=0.01)
        assert line.angle == pytest.approx(25, abs=0.01)
        assert line.vertical and line.thickness == pytest.approx(EM, abs=0.01)
        assert line.length == pytest.approx(800, abs=0.01)
        x0, x1, _, _ = line.spans()
        assert x1 - x0 > 5 * EM  # the axis-aligned box is what we must NOT use
        tx0, tx1, _, _ = line.spans(25)
        assert tx1 - tx0 == pytest.approx(EM, abs=0.01)

    def test_trapezoid_uses_edge_midpoints(self) -> None:
        quad = [[0, 0], [100, 10], [100, 50], [0, 60]]
        width, height, _ = ll.quad_frame([tuple(p) for p in quad])
        assert width == pytest.approx(100)
        assert height == pytest.approx(50)  # midpoints (50,5) -> (50,55)

    def test_horizontal_line_swaps_thickness_and_length(self) -> None:
        (line,) = measured([hline(100, 0, 600, "よこがきの文")])
        assert not line.vertical
        assert (line.thickness, line.length) == (pytest.approx(EM), pytest.approx(600))

    def test_empty_text_and_flat_quads_are_dropped(self) -> None:
        lines, dropped = ll.measure_lines(
            [vline(100, 0, 300, "  "), raw(box(0, 0, 0, 50), "あ"), vline(300, 0, 300, "あい")]
        )
        assert [ln.index for ln in lines] == [2]
        assert dropped == [0, 1]


class TestOrientation:
    def test_ambiguity_needs_aspect_and_text(self) -> None:
        tall_single, square_pair, clear = measured(
            [vline(100, 0, 200, "ー"), raw(box(300, 0, 370, 80), "!?"), vline(500, 0, 200, "ああ")]
        )
        assert ll.is_ambiguous(tall_single)  # one glyph never decides
        assert ll.is_ambiguous(square_pair)  # nor does a near-square box
        assert not ll.is_ambiguous(clear)

    def test_single_glyph_copies_its_neighbour(self) -> None:
        # A flat "一" inside a vertical bubble, a tall "ー" inside a horizontal caption.
        lines = measured(
            [
                vline(500, 100, 600, "縦書きの台詞"),
                raw(box(560, 100, 640, 130), "一"),
                hline(2000, 100, 700, "よこがきの説明"),
                raw(box(720, 1960, 750, 2040), "ー"),
            ]
        )
        assert [ln.vertical for ln in lines] == [True, True, False, False]

    def test_isolated_glyph_takes_the_page_orientation(self) -> None:
        lines = measured(
            [
                hline(100, 100, 900, "横書きのページです"),
                hline(200, 100, 900, "二行目もよこがき"),
                raw(box(1500, 2000, 1560, 2075), "字"),
            ]
        )
        assert lines[2].ambiguous and not lines[2].vertical

    def test_page_vote_is_by_area_and_defaults_to_vertical(self) -> None:
        big_vertical = measured(
            [vline(500, 0, 2000, "縦" * 30)]
            + [hline(50 * k, 0, 200, "よこ書き") for k in range(1, 4)]
        )
        assert ll.dominant_vertical(big_vertical)
        assert ll.dominant_vertical([])

    def test_real_map_page_single_glyph_labels_follow_the_title(self) -> None:
        layout = ll.layout_page(load("novel-map-006"))
        assert texts(layout)[0] == ["《钋鮋瞶画》"]
        assert not any(block["vertical"] for block in layout.blocks)

    def test_contents_grid_is_left_as_the_detector_boxed_it(self) -> None:
        # novel-frontmatter-005 (bench temp00005): four chapter headings stand
        # side by side as vertical columns of two glyphs, and the DETECTOR boxed
        # each ROW of the grid as one horizontal line (two 4-glyph quads, 504 x
        # 77 px) -- the merger and the orientation vote never see columns. The
        # grid is square (glyph pitch 142 px across, 136 px down), so no
        # geometry tells it from a letter-spaced two-row caption; only the
        # wording does, which the layout does not read. Left alone on purpose:
        # undoing it needs the recognizer again (crop the columns, read them
        # vertically), which is the engine's business, as with column_pieces.
        layout = ll.layout_page(load("novel-frontmatter-005"))
        rows = [b for b in layout.blocks if len(b["lines"]) == 2]
        assert [(b["vertical"], [len(t) for t in b["lines"]]) for b in rows] == [(False, [4, 4])]


# --------------------------------------------------------------------------
# 2. furigana
# --------------------------------------------------------------------------


def ruby_texts(raws: list[dict[str, Any]]) -> list[str]:
    _, ruby = ll.filter_furigana(measured(raws))
    return [r.text for r in ruby]


BASE = vline(500, 100, 580, "漢字を含む台詞だ")  # 8 glyphs, 8 em


class TestFuriganaRule:
    def test_script(self) -> None:
        assert ll.is_ruby_script("つまじ")
        assert ll.is_ruby_script("ぎ だぞしゃ")  # recognizer space inside a run
        assert ll.is_ruby_script("フェブトプ・ペンブ")
        assert ll.is_ruby_script("・・・・")  # bouten
        assert ll.is_ruby_script("つゑれ、")  # run with a comma read off its tail
        assert not ll.is_ruby_script("「ああ」")
        assert not ll.is_ruby_script("漢")
        assert not ll.is_ruby_script("OK")
        assert not ll.is_ruby_script("……")
        assert not ll.is_ruby_script(" ")

    def test_thin_kana_on_the_right_of_a_vertical_base_is_ruby(self) -> None:
        assert ruby_texts([BASE, vline(545, 120, 240, "かんじ", t=30)]) == ["かんじ"]

    def test_same_run_on_the_left_is_not(self) -> None:
        assert ruby_texts([BASE, vline(455, 120, 240, "かんじ", t=30)]) == []

    def test_thin_kana_above_a_horizontal_base_is_ruby_below_is_not(self) -> None:
        base = hline(500, 100, 700, "漢字を含む説明文")
        assert ruby_texts([base, hline(455, 120, 240, "かんじ", t=30)]) == ["かんじ"]
        assert ruby_texts([base, hline(545, 120, 240, "かんじ", t=30)]) == []

    def test_full_thickness_kana_line_survives(self) -> None:
        # The must-not-lose case: a short all-kana column right beside a kanji one.
        assert ruby_texts([BASE, vline(565, 100, 220, "ああ")]) == []
        assert ruby_texts([BASE, vline(565, 100, 340, "「ああ」")]) == []

    def test_brackets_protect_even_a_thin_box(self) -> None:
        assert ruby_texts([BASE, vline(545, 100, 340, "「ああ」", t=30)]) == []

    def test_gap_limit(self) -> None:
        near = vline(500 + 30 + 0.25 * EM + 15, 120, 240, "かんじ", t=30)
        far = vline(500 + 30 + 0.35 * EM + 15, 120, 240, "かんじ", t=30)
        assert ruby_texts([BASE, near]) == ["かんじ"]
        assert ruby_texts([BASE, far]) == []

    def test_thickness_limit(self) -> None:
        # 3 glyphs over 2.7 em: glyph pitch 0.9 of the base's, so only thickness can decide.
        ok = vline(500 + 30 + 0.70 * EM / 2, 120, 282, "かんじ", t=0.70 * EM)
        too_thick = vline(500 + 30 + 0.80 * EM / 2, 120, 282, "かんじ", t=0.80 * EM)
        assert ruby_texts([BASE, ok]) == ["かんじ"]
        assert ruby_texts([BASE, too_thick]) == []

    def test_must_run_alongside_the_base(self) -> None:
        beside_the_end = vline(545, 530, 680, "かんじ", t=30)  # a third of it overlaps
        assert ruby_texts([BASE, beside_the_end]) == []

    def test_base_must_hold_kanji_and_candidate_must_not(self) -> None:
        kana_base = vline(500, 100, 640, "かなだけのせりふだ")
        assert ruby_texts([kana_base, vline(545, 120, 240, "かんじ", t=30)]) == []
        assert ruby_texts([BASE, vline(545, 120, 240, "注釈", t=30)]) == []

    def test_generous_box_is_caught_by_its_small_glyphs(self) -> None:
        # Box 0.9 em thick (the detector filled the gutter) but 4 glyphs in 2.2 em.
        fat = vline(500 + 30 + 27 - 6, 120, 120 + 2.2 * EM, "かんじよ", t=0.9 * EM)
        assert ruby_texts([BASE, fat]) == ["かんじよ"]
        # Same box holding two full-size kana is a real column.
        real = vline(500 + 30 + 27 - 6, 120, 120 + 2.2 * EM, "ああ", t=0.9 * EM)
        assert ruby_texts([BASE, real]) == []

    def test_unreadable_glyph_low_confidence(self) -> None:
        misread = vline(545, 120, 160, "乳", t=30, conf=0.24)
        assert ruby_texts([BASE, misread]) == ["乳"]
        confident = vline(545, 120, 160, "乳", t=30, conf=0.9)
        assert ruby_texts([BASE, confident]) == []  # manga: small kanji asides are text

    def test_ruby_beside_a_tilted_base_is_judged_in_its_frame(self) -> None:
        pair = [rotated(ln, 25, (500, 400)) for ln in (BASE, vline(545, 120, 240, "かんじ", t=30))]
        assert ruby_texts(pair) == ["かんじ"]
        wrong_side = [
            rotated(ln, 25, (500, 400)) for ln in (BASE, vline(455, 120, 240, "かんじ", t=30))
        ]
        assert ruby_texts(wrong_side) == []

    def test_record_names_the_base_and_the_annotated_span(self) -> None:
        base = vline(500, 100, 900, "今日は漢字を勉強する。")
        n = len("今日は漢字を勉強する。")
        step = 800 / n
        run = vline(545, 100 + 3 * step, 100 + 5 * step, "かんじ", t=30)  # beside 漢字
        other = vline(400, 100, 900, "別の行にも漢字がある")
        _, (ruby,) = ll.filter_furigana(measured([other, base, run]))
        assert (ruby.line, ruby.base, ruby.text) == (2, 1, "かんじ")
        assert ruby.span == (pytest.approx(3 / n), pytest.approx(5 / n))
        assert "今日は漢字を勉強する。"[ruby.chars[0] : ruby.chars[1]] == "漢字"

    def test_closest_base_wins(self) -> None:
        # The run touches the column on its right too; its base is on its LEFT.
        left, right = vline(500, 100, 900, "左の漢字の列"), vline(600, 100, 900, "右の漢字の列")
        _, (ruby,) = ll.filter_furigana(
            measured([right, left, vline(550, 300, 420, "かんじ", t=36)])
        )
        assert ruby.base == 1


class TestFuriganaOnALattice:
    """Tiers that need a text body: the column lattice of a novel page."""

    COLUMNS = [(0, 0, FULL)] * 8

    def test_pitch_is_measured(self) -> None:
        lines = measured(novel_page(self.COLUMNS)["lines"])
        assert ll.column_pitch(lines) == pytest.approx(PITCH)
        assert ll.column_pitch(measured([BASE, vline(600, 100, 700, "漢字")])) is None

    def test_column_thick_single_glyph_ruby_in_the_gutter(self) -> None:
        # As thick as a column, one glyph: neither thickness nor glyph pitch can
        # tell. It sits 0.5 pitch from its base; the next real column is at 1.0.
        gutter = vline(1800 - 3 * PITCH + 50, 900, 960, "め", t=54)
        layout = ll.layout_page(novel_page(self.COLUMNS, [gutter]))
        assert [r.text for r in layout.ruby] == ["め"]
        assert layout.ruby[0].base == 3

    def test_short_kana_column_on_the_lattice_survives(self) -> None:
        columns = list(self.COLUMNS)
        columns[4] = (0, 36, "ああ")  # all-kana, bare, sitting on its own lattice slot
        layout = ll.layout_page(novel_page(columns))
        assert layout.ruby == []
        assert any("ああ" in block["lines"] for block in layout.blocks)

    def test_short_kanji_column_can_still_carry_a_reading(self) -> None:
        # "矉孱" is short enough to be doubted itself; once kept, it is a base.
        columns = list(self.COLUMNS)
        columns[4] = (1, 37, "矉孱")
        reading = vline(1800 - 4 * PITCH + 45, BODY_TOP + EM, BODY_TOP + 3 * EM, "よろぜ", t=30)
        layout = ll.layout_page(novel_page(columns, [reading]))
        assert [(r.text, r.base) for r in layout.ruby] == [("よろぜ", 4)]
        assert any(block["lines"] == ["矉孱"] for block in layout.blocks)

    def test_confident_misreading_needs_the_lattice_and_a_thin_box(self) -> None:
        thin = vline(1800 - 3 * PITCH + 45, 900, 960, "仇", t=30, conf=0.71)
        assert [r.text for r in ll.layout_page(novel_page(self.COLUMNS, [thin])).ruby] == ["仇"]
        thick = vline(1800 - 3 * PITCH + 50, 900, 960, "仇", t=40, conf=0.71)
        assert ll.layout_page(novel_page(self.COLUMNS, [thick])).ruby == []


class TestFuriganaOnRealPages:
    EXPECTED_COUNTS = {
        "novel-text-013": 17,
        "novel-text-097": 18,
        "novel-text-250": 28,
        "novel-dialogue-019": 8,
        "novel-dialogue-200": 4,
        "novel-section-024": 25,
        "manga-page-066": 26,  # 25 kana runs + the tail of one read as "萮"
        "manga-page-069": 11,
    }

    @pytest.mark.parametrize("name", [*NOVEL_TEXT, *MANGA])
    def test_every_ruby_run_is_removed_and_nothing_else(self, name: str) -> None:
        raw_page = load(name)
        layout = ll.layout_page(raw_page)
        assert len(layout.ruby) == self.EXPECTED_COUNTS[name]
        for ruby in layout.ruby:
            readable = ll.is_ruby_script(ruby.text)
            assert readable or ll.glyph_count(ruby.text) <= 2, ruby.text
            base = raw_page["lines"][ruby.base]["text"]
            assert ll.has_kanji(base)
            if readable and ll.glyph_count(ruby.text) >= 2:
                # a reading always sits beside at least one kanji of its base
                lo, hi = max(ruby.chars[0] - 1, 0), ruby.chars[1] + 1
                assert ll.has_kanji(base[lo:hi]), (ruby.text, base[lo:hi])

    @pytest.mark.parametrize("name", NOVEL_TEXT)
    def test_no_ruby_is_left_in_a_novel_body(self, name: str) -> None:
        layout = ll.layout_page(load(name))
        leftovers = [
            text
            for block, kind in zip(layout.blocks, layout.kinds, strict=True)
            if kind == "body"
            for text in block["lines"]
            if all(ll.is_kana(ch) for ch in text if not ch.isspace()) and ll.glyph_count(text) <= 8
        ]
        assert leftovers == []

    def test_short_dialogue_lines_survive(self) -> None:
        kept = {
            text for lines in texts(ll.layout_page(load("novel-dialogue-019"))) for text in lines
        }
        assert {"「ろま」", "「うくつ」", "「なのーま。唄だて」"} <= kept
        kept = {
            text for lines in texts(ll.layout_page(load("novel-dialogue-200"))) for text in lines
        }
        assert {"「翆れ」", "「蘓ぞ！」"} <= kept

    def test_bases_are_the_annotated_words(self) -> None:
        raw_page = load("novel-dialogue-019")
        found = {
            r.text: raw_page["lines"][r.base]["text"][r.chars[0] : r.chars[1]]
            for r in ll.layout_page(raw_page).ruby
        }
        assert "豩蘒" in found["もつじう"]
        assert "碂瀴" in found["うわぎほ"]
        assert "丟昚" in found["あぞうぞ"]

    def test_manga_known_misses_are_documented(self) -> None:
        # "うえ" (懂楊り): two glyphs in a generous box -- thickness 0.88, glyph
        # pitch 0.94 of its base. Geometry cannot tell it from a real two-kana
        # column, and losing real text is the worse error, so it stays.
        blocks = texts(ll.layout_page(load("manga-page-069")))
        assert ["うえ", "懂楊りぶゐにお", "ちゃまづ阌噼ら", "崩ってぞゐ"] in blocks


# --------------------------------------------------------------------------
# 3. bodies, margins, noise
# --------------------------------------------------------------------------


class TestBodiesAndRoles:
    COLUMNS = [(0, 0, FULL)] * 4 + [(1, 0, FULL), (0, 30, "乒ぞ輢。"), (1, 0, FULL), (0, 0, FULL)]

    def test_body_edges_and_spacing(self) -> None:
        lines = measured(novel_page(self.COLUMNS)["lines"])
        (body,) = ll.find_bodies(lines)
        assert body.vertical and len(body.members) == 8
        assert (body.top, body.bottom) == (pytest.approx(BODY_TOP), pytest.approx(BODY_BOTTOM))
        assert body.em == pytest.approx(EM)
        assert body.gap == pytest.approx(PITCH - EM)

    def test_a_flush_bracket_does_not_lower_the_top_edge(self) -> None:
        # Every column opens with a bracket whose ink starts ~0.45 em low.
        columns = [(0.45, 0, "「" + FULL)] * 6
        (body,) = ll.find_bodies(measured(novel_page(columns)["lines"]))
        assert body.top == pytest.approx(BODY_TOP, abs=1)

    def test_one_long_column_is_enough_to_set_an_edge(self) -> None:
        columns = [(0.45, 20, "「みじかい台詞」")] * 6 + [
            (0, 0, FULL),
            (1, 12, FULL),
            (1, 14, FULL),
        ]
        (body,) = ll.find_bodies(measured(novel_page(columns)["lines"]))
        assert (body.top, body.bottom) == (
            pytest.approx(BODY_TOP, abs=1),
            pytest.approx(BODY_BOTTOM),
        )

    def test_manga_page_has_no_body(self) -> None:
        for name in MANGA:
            assert ll.layout_page(load(name)).bodies == []

    def test_skewed_scan_is_deskewed(self) -> None:
        skewed = [rotated(ln, 1.0, (1000, 1400)) for ln in novel_page(self.COLUMNS)["lines"]]
        (body,) = ll.find_bodies(measured(skewed))
        assert body.theta == pytest.approx(1.0, abs=0.01)
        flat = ll.layout_page(novel_page(self.COLUMNS))
        tilted = ll.layout_page(page(skewed))
        assert texts(tilted) == texts(flat)

    def test_page_number_and_running_title_are_fenced_off(self) -> None:
        nombre = hline(2740, 950, 1010, "123", t=36)
        title = hline(90, 1200, 1700, "軋め瘥 瘥め鰑", t=36)
        layout = ll.layout_page(novel_page(self.COLUMNS, [nombre, title]))
        assert layout.kinds[0] == "header" and layout.blocks[0]["lines"] == ["軋め瘥\u3000瘥め鰑"]
        assert layout.kinds[-1] == "footer" and layout.blocks[-1]["lines"] == ["123"]
        assert set(layout.kinds[1:-1]) == {"body"}

    def test_vertical_nombre_under_a_column_never_joins_it(self) -> None:
        # Same size, same x, just under the last column: only its role keeps it out.
        nombre = vline(1800 - 7 * PITCH, 2690, 2760, "九", t=EM)
        layout = ll.layout_page(novel_page(self.COLUMNS, [nombre]))
        assert layout.blocks[-1]["lines"] == ["九"] and layout.kinds[-1] == "footer"

    def test_no_margins_without_a_body(self) -> None:
        layout = ll.layout_page(page([hline(2740, 950, 1010, "123", t=36), BASE]))
        assert set(layout.kinds) == {"text"}

    def test_low_confidence_glyph_is_kept_but_alone_and_last(self) -> None:
        speck = raw(box(900, 1500, 930, 1535), "く", conf=0.2)
        layout = ll.layout_page(page([BASE, vline(570, 100, 700, "二行目の台詞です"), speck]))
        assert layout.kinds == ["text", "noise"]
        assert layout.blocks[-1]["lines"] == ["く"]

    def test_low_confidence_line_of_several_glyphs_is_noise_too(self) -> None:
        smudge = raw(box(900, 1500, 1000, 1560), "簈蒕", conf=0.21)
        layout = ll.layout_page(page([BASE, smudge]))
        assert layout.kinds == ["text", "noise"] and layout.blocks[-1]["lines"] == ["簈蒕"]

    def test_cover_logo_and_hatching_are_noise_on_real_pages(self) -> None:
        # Evaluation 2026-09-19: the publisher's grape logo read "簈蒕" (0.21)
        # and the hatching of an illustration "H:" (0.17) / "8" (0.39).
        cover = ll.layout_page(load("novel-cover-001"))
        kept = [
            ln
            for b, k in zip(cover.blocks, cover.kinds, strict=True)
            if k != "noise"
            for ln in b["lines"]
        ]
        assert "簈蒕" not in kept
        assert {"隻睅鷃輷", "軋め瘥瘥め鰑", "钋鮋瞶寃", "魾蟅菏酒筒祣", "隻睅嬁"} <= set(kept)
        drawing = ll.layout_page(load("novel-illustration-069"))
        kept = [
            ln
            for b, k in zip(drawing.blocks, drawing.kinds, strict=True)
            if k != "noise"
            for ln in b["lines"]
        ]
        # "ぐ" (0.77, leopard spots) is too confident for the noise rule, but it
        # is all the page has to say: artwork (see TestArtworkReadAsGlyphs).
        assert kept == []

    def test_doubtful_glyph_cut_off_its_column_is_stitched_back(self) -> None:
        head = vline(500, 100, 165, "マ", t=80, conf=0.3)
        rest = vline(500, 150, 600, "ワーら旬糳で")
        layout = ll.layout_page(page([rest, head, vline(430, 100, 600, "頇蟽しもえゑ")]))
        assert texts(layout) == [["マ", "ワーら旬糳で", "頇蟽しもえゑ"]]


# --------------------------------------------------------------------------
# 4. lines -> blocks
# --------------------------------------------------------------------------


def merged(a: dict, b: dict, body_gap: float | None = None) -> bool:
    la, lb = measured([a, b])
    assert ll.should_merge(la, lb, body_gap) == ll.should_merge(lb, la, body_gap)
    return ll.should_merge(la, lb, body_gap)


class TestShouldMerge:
    A = vline(500, 100, 700, "漢字を含む台詞だ")

    def test_neighbouring_columns_merge(self) -> None:
        assert merged(self.A, vline(500 - EM - 0.5 * EM, 100, 650, "となりの行です"))

    def test_gap_tiers(self) -> None:
        def at(gap_em: float, y0: float = 100.0, t: float = EM) -> dict:
            return vline(500 - EM / 2 - gap_em * EM - t / 2, y0, y0 + 600, "となりの行です", t)

        assert not merged(self.A, at(0.8, y0=160))  # ragged start: tier 1 only (0.75)
        assert merged(self.A, at(0.8, y0=125))  # start within 0.5 em: tier 3 (1.0)
        assert merged(self.A, at(1.2, y0=110))  # same size, aligned start: tier 2 (1.25)
        assert not merged(self.A, at(1.2, y0=125))
        assert not merged(self.A, at(1.3, y0=100))

    def test_size_and_orientation(self) -> None:
        assert not merged(self.A, vline(400, 100, 700, "三倍の大きさ", t=3 * EM))
        assert not merged(self.A, hline(400, 300, 480, "よこがき"))

    def test_must_overlap_along_the_reading_axis(self) -> None:
        assert not merged(self.A, vline(420, 600, 1200, "下の吹き出しの行"))

    def test_staggered_bubbles_stay_apart(self) -> None:
        # Touching, overlapping by more than half of the shorter -- but both the
        # starts and the ends are over 2 em apart.
        assert not merged(self.A, vline(430, 250, 880, "ずれた吹き出し"))

    def test_split_column_is_stitched(self) -> None:
        assert merged(self.A, vline(503, 720, 1000, "つづきの部分"))
        assert not merged(self.A, vline(503, 800, 1000, "はなれた部分"))
        assert merged(self.A, vline(503, 800, 1000, "はなれた部分"), body_gap=40.0)

    def test_body_columns_use_the_body_spacing(self) -> None:
        wide = vline(500 - EM - 1.0 * EM, 160, 400, "字下げの短い行")
        assert not merged(self.A, wide)
        assert merged(self.A, wide, body_gap=0.7 * EM)  # 1.6 x 0.7 = 1.12 em allowed
        assert not merged(self.A, wide, body_gap=0.5 * EM)

    def test_angles_must_agree(self) -> None:
        a = rotated(self.A, 20, (500, 400))
        b = rotated(vline(410, 100, 700, "となりの行です"), 20, (500, 400))
        upright = vline(410, 100, 700, "となりの行です")
        assert merged(a, b)
        assert not merged(a, upright)

    def test_short_line_follows_its_partners_frame(self) -> None:
        # "!?" has no angle of its own to disagree with.
        a = rotated(self.A, 20, (500, 400))
        short = rotated(vline(410, 100, 200, "!?"), 20, (500, 400))
        assert merged(a, short)

    def test_tilted_pair_is_measured_in_its_own_frame(self) -> None:
        # Two 30-em columns 0.9 em apart, tilted 8 degrees: their axis-aligned
        # boxes overlap by several em, the true gap is what decides.
        a = rotated(vline(500, 100, 1900, "長" * 30), 8, (500, 1000))
        far = rotated(vline(500 - EM - 1.4 * EM, 160, 1900, "長" * 29), 8, (500, 1000))
        la, lb = measured([a, far])
        assert ll._overlap(*la.spans()[:2], *lb.spans()[:2]) > 0
        assert not ll.should_merge(la, lb)


class TestMergeLines:
    def test_two_bubbles_across_a_gutter(self) -> None:
        right = [vline(900 - k * 70, 100, 500, f"右の吹き出し{k}") for k in range(3)]
        left = [
            vline(900 - 3 * 70 - 1.5 * EM - k * 70, 100, 500, f"左の吹き出し{k}") for k in range(3)
        ]
        layout = ll.layout_page(page(right + left))
        assert texts(layout) == [[ln["text"] for ln in right], [ln["text"] for ln in left]]

    def test_rotated_bubble_is_one_block_with_axis_aligned_bounds(self) -> None:
        bubble = [vline(900 - k * 70, 300, 700, f"叫びの台詞{k}") for k in range(3)]
        upright = ll.layout_page(page(bubble))
        tilted_lines = [rotated(ln, 20, (830, 500)) for ln in bubble]
        tilted = ll.layout_page(page(tilted_lines + [vline(990, 300, 700, "まっすぐな行")]))
        assert texts(tilted) == [["まっすぐな行"], *texts(upright)]
        xs = [p[0] for ln in tilted_lines for p in ln["quad"]]
        ys = [p[1] for ln in tilted_lines for p in ln["quad"]]
        assert tilted.blocks[1]["box"] == [
            math.floor(min(xs)),
            math.floor(min(ys)),
            math.ceil(max(xs)),
            math.ceil(max(ys)),
        ]

    EXPECTED_BUBBLES = {
        "manga-page-066": [
            ["敎眄も嗦鷃で", "堶睖ら錪ゑだて", "しうっかえ"],
            ["隆萚めぽ", "つもり敎榸も", "マワーアッエじゃ"],
            ["ゾグブスめ劯", "諃傣力睏鑍颜"],
            ["ソンタヰ嬊嶘め", "アイゾンス皫り", "鸴にマワーら", "蕈ばておゑっか"],
            ["炕杌ら蜤ゑね熓觷で", "ギリギリうで", "斪力ら姲のか"],
            ["ワーら旬糳で", "頇蟽しもえゑ", "奡ろまじゃよ"],
            ["轫力棴实にしかゑ", "マークめ并瘖閗鼦ら", "繗をて賄むてしうろ"],
            ["あぼ", "うでお", "覗め鐤"],
        ],
        "manga-page-069": [
            ["もによ", "アレタれって", "しょっちゅろ", "ぞゐじゃもぞめ"],
            ["鷃遲漁ほむゐ", "墎注ぞもぞほ"],
            ["昶にぽ嬊嶘め", "賺孱れづぞろ", "抽藖睾えあゐ"],
            [
                "阌噼って…ランタヰぽ",
                "ぜめヰブショブめ",
                "阌廻じゃもぼて",
                "嵙鐤に驄までゐれこでしょ",
            ],
            ["阵くむてゐほよ", "アレタ"],
        ],
    }

    @pytest.mark.parametrize("name", MANGA)
    def test_real_bubbles_are_whole_and_separate(self, name: str) -> None:
        blocks = texts(ll.layout_page(load(name)))
        for bubble in self.EXPECTED_BUBBLES[name]:
            assert bubble in blocks

    def test_real_page_reads_tier_by_tier_right_to_left(self) -> None:
        first_lines = [lines[0] for lines in texts(ll.layout_page(load("manga-page-066")))]
        wanted = [
            "敎眄も嗦鷃で",
            "隆萚めぽ",
            "ゾグブスめ劯",
            "ソンタヰ嬊嶘め",
            "炕杌ら蜤ゑね熓觷で",
            "轫力棴实にしかゑ",
        ]
        positions = [first_lines.index(text) for text in wanted]
        assert positions == sorted(positions)


# --------------------------------------------------------------------------
# 5. paragraphs
# --------------------------------------------------------------------------


def paragraphs(columns: list[tuple[float, float, str]]) -> list[list[str]]:
    layout = ll.layout_page(novel_page(columns))
    assert set(layout.kinds) == {"body"}
    return texts(layout)


class TestParagraphs:
    def test_one_running_paragraph_stays_one_block(self) -> None:
        assert len(paragraphs([(0, 0, FULL)] * 12)) == 1

    def test_indent_starts_a_paragraph(self) -> None:
        columns = [
            (0, 0, "前の頁から"),
            (0, 0, FULL),
            (1, 0, "字下げ"),
            (0, 0, FULL),
            (0, 0, FULL),
            (0, 0, FULL),
        ]
        assert [p[0] for p in paragraphs(columns)] == ["前の頁から", "字下げ"]

    def test_two_glyph_last_column_after_an_indented_one_stays_with_it(self) -> None:
        """Bench temp00070: "…突進して交わっ" | "た。" came out as two blocks.

        The paragraph is two columns: its first is indented, its last holds
        two glyphs at the top of the body. The two overlap along the column by
        a third of the short one -- the indent eats the rest -- which is under
        the merger's half, so "た。" became a block of its own and the verb was
        cut where no dictionary can put it together again (21 times in one
        novel).
        """
        columns = [
            (0, 0, FULL),
            (0, 12, "前の段落はここで終わる。"),
            (1.2, 0, "字下げして始まり底まで続いて交わっ"),
            (0, 38.4, "た。"),
            (1.2, 0, "次の段落も字下げ"),
            (0, 0, FULL),
        ]
        assert paragraphs(columns) == [
            [FULL, "前の段落はここで終わる。"],
            ["字下げして始まり底まで続いて交わっ", "た。"],
            ["次の段落も字下げ", FULL],
        ]

    def test_short_previous_column_starts_a_paragraph(self) -> None:
        # The indent itself was lost (detector box started flush) but the
        # previous column stopped 10 em short.
        columns = [
            (0, 0, FULL),
            (0, 10, "短く終わる。"),
            (0, 0, "次の段落"),
            (0, 0, FULL),
            (0, 0, FULL),
            (0, 0, FULL),
        ]
        assert [p[0] for p in paragraphs(columns)] == [FULL, "次の段落"]

    def test_period_at_the_bottom_is_not_a_short_end(self) -> None:
        columns = [
            (0, 0, FULL),
            (0, 0.6, "最後が句点。"),
            (0, 0, "続きの列"),
            (0, 0, FULL),
            (0, 0, FULL),
            (0, 0, FULL),
        ]
        assert len(paragraphs(columns)) == 1

    def test_flush_bracket_after_a_finished_sentence(self) -> None:
        columns = [
            (0, 0, FULL),
            (0, 0.6, "最後が句点。"),
            (0.45, 0, "「台詞が始まる"),
            (0, 0, FULL),
            (0, 0, FULL),
            (0, 0, FULL),
        ]
        assert [p[0] for p in paragraphs(columns)] == [FULL, "「台詞が始まる"]

    def test_flush_bracket_mid_sentence_is_a_quotation_not_a_paragraph(self) -> None:
        columns = [
            (0, 0, FULL),
            (0, 0, "彼はこう言った、"),
            (0.45, 0, "「引用」と。"),
            (0, 0, FULL),
            (0, 0, FULL),
            (0, 0, FULL),
        ]
        assert len(paragraphs(columns)) == 1

    def test_dialogue_exchange(self) -> None:
        columns = [
            (1, 0, FULL),
            (0, 25, "恺ほり。"),
            (0.45, 30, "「ろま」"),
            (0.45, 28, "「ぷろつも」"),
            (1, 0, FULL),
            (0, 0, FULL),
            (0, 12, "簘数め恺ほり。"),
        ]
        assert [p[0] for p in paragraphs(columns)] == [FULL, "「ろま」", "「ぷろつも」", FULL]

    def test_inset_passage_stays_together(self) -> None:
        letter = [(3, 0, FULL), (3, 0, FULL), (3, 5, "手紙の終わり。")]
        columns = [(0, 0, FULL), (0, 0, FULL), *letter, (1, 0, FULL), (0, 0, FULL)]
        result = paragraphs(columns)
        assert [len(p) for p in result] == [2, 3, 2]

    EXPECTED_STARTS = {
        "novel-text-013": [
            "りにぽ浞ぞもぞ。",
            "伪鈵め竢かち",
            "ぷめ趌媀に",
            "囵熔ぽ誕こて",
            "―あむえ褗かゑ",
            "ぷろ眄鹳できてお",
            "蜱姶め豩で",
            "唄どのゐ懂に",
            "矉孱ぽ數び缡きか",
        ],
        "novel-dialogue-200": [
            "まもゑ、",
            "搧ぶめえ鳲呒",
            "「れっかゑ",
            "「くてもェ",
            "矉孱ぽぷめ峙ぞ",
            "宵むぱをよ」",
            "唞え歙ぞつこて",
            "「ん烓、宵むぱをよ",
            "「翆れ」",
            "翆えゐぜづぽ",
            "「蘓ぞ！」",
            "矉孱め憕びぽ",
        ],
        "novel-dialogue-019": [
            "「豩蘒くま、",
            "「ろま」",
            "「なのーま",
            "矉孱ぽ崅ぼ",
            "「豩蘒くまって",
            "漁ほむて矉孱ぽ",
            "「ムブト、",
            "「ぷろ、ぷろ",
            "「ぜまもめ",
            "矉孱ぽ膡てて",
            "ぷ、ぷまもぜづ",
            "「じゃ、虚胯",
            "「うくつ」",
            "矉孱ぽ峙って",
            "ろち、壱涥え",
        ],
    }

    @pytest.mark.parametrize("name", sorted(EXPECTED_STARTS))
    def test_real_paragraphs(self, name: str) -> None:
        layout = ll.layout_page(load(name))
        starts = [block["lines"][0] for block in layout.blocks]
        expected = self.EXPECTED_STARTS[name]
        assert len(starts) == len(expected)
        for got, want in zip(starts, expected, strict=True):
            assert got.startswith(want), (got, want)

    def test_real_split_column_is_read_as_one(self) -> None:
        # "...れがろ？" / "―" / "熙みもゑ..." are one column cut at a dash.
        blocks = texts(ll.layout_page(load("novel-dialogue-200")))
        (column_block,) = [b for b in blocks if b[0].startswith("翆えゐぜづぽ")]
        assert len(column_block) == 3 and column_block[2].startswith("熙みもゑ")

    @pytest.mark.parametrize("name", NOVEL_TEXT)
    def test_real_blocks_are_paragraph_sized(self, name: str) -> None:
        layout = ll.layout_page(load(name))
        assert max(len(block["lines"]) for block in layout.blocks) <= 6


# --------------------------------------------------------------------------
# 6. reading order
# --------------------------------------------------------------------------


class TestReadingOrder:
    def test_lines_inside_a_vertical_block_run_right_to_left(self) -> None:
        lines = [vline(500 + k * 70, 100, 500, f"第{k}列の台詞") for k in range(4)]
        assert texts(ll.layout_page(page(lines))) == [[f"第{k}列の台詞" for k in (3, 2, 1, 0)]]

    def test_lines_inside_a_horizontal_block_run_top_down(self) -> None:
        lines = [hline(900 - k * 70, 100, 700, f"よこがきの第{k}行") for k in range(3)]
        assert texts(ll.layout_page(page(lines))) == [[f"よこがきの第{k}行" for k in (2, 1, 0)]]

    def test_vertical_page_rows_then_right_to_left(self) -> None:
        def bubble(x: float, y: float, tag: str) -> list[dict]:
            return [vline(x - k * 70, y, y + 400, f"{tag}の台詞{k}") for k in range(2)]

        lines = (
            bubble(400, 1500, "左下")
            + bubble(1600, 1450, "右下")
            + bubble(500, 150, "左上")
            + bubble(1500, 100, "右上")
        )
        order = [b[0][:2] for b in texts(ll.layout_page(page(lines)))]
        assert order == ["右上", "左上", "右下", "左下"]

    def test_horizontal_page_reads_left_to_right(self) -> None:
        def para(x: float, y: float, tag: str) -> list[dict]:
            return [hline(y + k * 80, x, x + 500, f"{tag} paragraph {k}") for k in range(2)]

        lines = para(1200, 100, "NE") + para(100, 120, "NW") + para(100, 1500, "SW")
        assert [b[0][:2] for b in texts(ll.layout_page(page(lines)))] == ["NW", "NE", "SW"]

    def test_mixed_page_each_row_its_own_way(self) -> None:
        picture_text = [vline(1500 - k * 70, 300, 800, f"絵の中の台詞{k}") for k in range(2)]
        other = [vline(500 - k * 70, 350, 800, f"左の吹き出し{k}") for k in range(2)]
        caption = [hline(2400, 300, 900, "A caption, left"), hline(2400, 1100, 1700, "and right")]
        order = [b[0] for b in texts(ll.layout_page(page(caption + other + picture_text)))]
        assert order == ["絵の中の台詞0", "左の吹き出し0", "A caption, left", "and right"]

    def test_two_tier_page_reads_the_upper_tier_first(self) -> None:
        lines = []
        for tier, y0 in enumerate((200, 1500)):
            for k in range(8):
                inset = EM if k in (0, 4) else 0
                lines.append(
                    vline(
                        1800 - k * PITCH,
                        y0 + inset,
                        y0 + 1080,
                        f"{'上下'[tier]}段{k}" + "の漢字文" * 4,
                    )
                )
        # a tall heading in the margin straddles both tiers: it must not fuse
        # them, and it is read with the tier its centre falls in (the lower)
        lines.append(vline(150, 700, 2100, "柱の見出しが縦に長く続く", t=40))
        layout = ll.layout_page(page(list(reversed(lines))))
        assert len(layout.bodies) == 2
        firsts = [b[0][:3] for b in texts(layout)]
        assert firsts == ["上段0", "上段4", "下段0", "下段4", "柱の見"]

    def test_section_number_between_text_regions_keeps_its_place(self) -> None:
        layout = ll.layout_page(load("novel-section-024"))
        order = [b[0] for b in texts(layout)]
        index = order.index("3")
        assert layout.kinds[index] == "text"
        assert order[index - 1].startswith("矉孱ぽぷろ旬糳に")
        assert order[index + 1].startswith("鎋おもぼ冻おもぞ")

    def test_small_first_block_does_not_break_the_row(self) -> None:
        # A sound effect tops the tier but overlaps the far bubble only barely.
        sfx = vline(100, 900, 1130, "バン", t=110)
        mid = [vline(700 - k * 70, 960, 1380, f"中の台詞{k}") for k in range(2)]
        far_right = [vline(1500 - k * 70, 1090, 1440, f"右の台詞{k}") for k in range(2)]
        order = [b[0] for b in texts(ll.layout_page(page([sfx, *mid, *far_right])))]
        assert order == ["右の台詞0", "中の台詞0", "バン"]


# --------------------------------------------------------------------------
# 7. the block
# --------------------------------------------------------------------------


class TestBuildBlock:
    def test_font_size_box_and_coords(self) -> None:
        raws = [
            raw([[10.4, -3.2], [70.6, -3.2], [70.6, 500.5], [10.4, 500.5]], "一行目"),
            vline(110, 0, 400, "二行目", t=50),
            vline(180, 0, 400, "三行目", t=90),
        ]
        lines = measured(raws)
        block = ll.build_block(lines, page_width=200, page_height=480)
        assert block["box"] == [10, 0, 200, 480]  # clamped to the page
        assert block["font_size"] == 60  # the median, not the mean (66.7)
        assert block["vertical"] is True
        assert block["lines"] == ["一行目", "二行目", "三行目"]
        assert block["lines_coords"][0] == [[10, -3], [71, -3], [71, 500], [10, 500]]
        assert all(isinstance(v, int) for quad in block["lines_coords"] for p in quad for v in p)
        assert all(isinstance(v, int) for v in block["box"])

    def test_rotated_quad_keeps_its_corner_order(self) -> None:
        tilted = rotated(vline(500, 100, 900, "斜めの叫び"), 30, (500, 500))
        layout = ll.layout_page(page([tilted]))
        (quad,) = layout.blocks[0]["lines_coords"]
        assert quad == [[int(round(x)), int(round(y))] for x, y in tilted["quad"]]
        width, height, angle = ll.quad_frame([tuple(p) for p in quad])
        assert angle == pytest.approx(30, abs=0.2) and height > width

    def test_meaningless_tilt_of_a_short_line_is_taken_out(self) -> None:
        # A flush three-glyph line whose min-area rectangle came back at +4 degrees.
        short = rotated(vline(500, 100, 280, "「翆れ」"), 4.1, (500, 190))
        (quad,) = ll.layout_page(page([short])).blocks[0]["lines_coords"]
        assert quad == [[470, 100], [530, 100], [530, 280], [470, 280]]

    def test_short_line_follows_the_tilt_of_its_block(self) -> None:
        long_a = rotated(vline(500, 100, 700, "斜めに組まれた長い台詞"), 8, (500, 400))
        short = rotated(vline(430, 100, 250, "です"), 3, (430, 175))
        layout = ll.layout_page(page([long_a, short]))
        assert texts(layout) == [["斜めに組まれた長い台詞", "です"]]
        _, _, angle = ll.quad_frame([tuple(p) for p in layout.blocks[0]["lines_coords"][1]])
        assert angle == pytest.approx(8, abs=0.6)

    def test_really_rotated_short_line_keeps_its_angle(self) -> None:
        shout = rotated(vline(500, 100, 300, "バン", t=110), -37, (500, 200))
        (quad,) = ll.layout_page(page([shout])).blocks[0]["lines_coords"]
        _, _, angle = ll.quad_frame([tuple(p) for p in quad])
        assert angle == pytest.approx(-37, abs=0.5)

    def test_one_glyph_box_has_no_angle_of_its_own(self) -> None:
        glyph = rotated(raw(box(470, 470, 530, 536), "き"), 14, (500, 503))
        (quad,) = ll.layout_page(page([glyph])).blocks[0]["lines_coords"]
        assert quad == [[470, 470], [530, 470], [530, 536], [470, 536]]

    def test_map_labels_come_out_upright(self) -> None:
        # novel-map-006: one-glyph labels, raw angles up to +9.7 degrees ("蒩").
        raw_page = load("novel-map-006")
        assert max(abs(ln["angle"]) for ln in raw_page["lines"] if ln["text"] == "蒩") > 9
        for block in ll.layout_page(raw_page).blocks:
            for quad, text in zip(block["lines_coords"], block["lines"], strict=True):
                if len(text) <= 2:
                    _, _, angle = ll.quad_frame([tuple(p) for p in quad])
                    assert abs(angle) < 1.0, (text, angle)

    def test_output_is_json_serialisable_mokuro(self) -> None:
        layout = ll.layout_page(load("manga-page-066"))
        for block in json.loads(json.dumps(ll.page_blocks(load("manga-page-066")))):
            assert set(block) == {"box", "vertical", "font_size", "lines", "lines_coords"}
            assert len(block["lines"]) == len(block["lines_coords"]) >= 1
            assert all(len(quad) == 4 for quad in block["lines_coords"])
        assert len(layout.blocks) == len(layout.groups) == len(layout.kinds)


class TestKanaLookalikes:
    @pytest.mark.parametrize(
        ("read", "fixed"),
        [
            ("裬にぽ暟めよろも夕ンサら頒き", "裬にぽ暟めよろもタンサら頒き"),
            ("ザニ夕ーら唄ゐ", "ザニターら唄ゐ"),
            ("力ーテブら卜ブトブづ", "カーテブらトブトブづ"),
            ("棴力マブチ", "棴力マブチ"),  # a real 力: it follows a kanji
            ("耺力データ", "耺力データ"),
            ("マブチ力えあゐ", "マブチ力えあゐ"),  # nothing katakana after it
            ("夕埚めニュース", "夕埚めニュース"),
            ("鮋ゼートサ嬊め颱ツミ", "鮋ゼートサ嬊め颱ツミ"),  # never touched
        ],
    )
    def test_rule(self, read: str, fixed: str) -> None:
        assert ll.fix_kana_lookalikes(read) == fixed

    def test_written_lines_are_fixed_on_a_real_page(self) -> None:
        raw_page = load("novel-text-097")
        assert any("夕ンサ" in ln["text"] for ln in raw_page["lines"])
        written = [line for block in ll.page_blocks(raw_page) for line in block["lines"]]
        assert any("タンサら頒き縅のて" in line for line in written)
        assert not any("夕ンサ" in line for line in written)


class TestKanaInKatakanaWords:
    """Bench, one novel: ゲンキ written "ゲンき" 29 times of 100, ジョハユハ "ジョハュハ" 14 of 38."""

    @pytest.mark.parametrize(
        ("read", "fixed"),
        [
            ("ゲンきら傻くもぼてぽ", "ゲンキら傻くもぼてぽ"),
            ("ヒョハき、づ漗まれ", "ヒョハキ、づ漗まれ"),
            ("ジョハュハめ力", "ジョハユハめ力"),
            ("べサトら盨のゐ", "ベサトら盨のゐ"),
            ("轜めべサトれ", "轜めベサトれ"),
            ("「へサゼット」", "「ヘサゼット」"),
            ("アへブめ鬠", "アヘブめ鬠"),
            ("ロッカりしか", "ロッカりしか"),  # り after katakana is a verb ending (ソヸり)
            ("ボスりづ峙ろ", "ボスりづ峙ろ"),
            ("スりッマ", "スリッマ"),
            # untouched: particles, verb endings, real small kana
            ("糵糜へトグッボで輢ぼ", "糵糜へトグッボで輢ぼ"),
            ("ぜぜへタボシーえ褗か", "ぜぜへタボシーえ褗か"),
            ("璜べタン", "璜べタン"),
            ("ングどきら剂をか", "ングどきら剂をか"),
            ("キきかぞ", "キきかぞ"),  # one katakana before it proves nothing
            ("ジュース、シャク、チョツ、デュガット", "ジュース、シャク、チョツ、デュガット"),
            ("ョハツ", "ョハツ"),  # a line may open with the small kana of a wrapped word
        ],
    )
    def test_rule(self, read: str, fixed: str) -> None:
        assert ll.normalize_text(read) == fixed


class TestDashesAndSpaces:
    @pytest.mark.parametrize("read", ["すげー", "すげーー", "あー", "ねえー"])
    def test_a_long_vowel_that_ends_the_line_is_left_alone(self, read: str) -> None:
        # Nothing follows the run, so there is no next glyph to classify. This
        # used to raise (ord of an empty string) and cost the page its sidecar
        # entry: 2-13 pages of every manga volume on the first library run.
        assert ll.normalize_text(read) == read

    @pytest.mark.parametrize(
        ("read", "fixed"),
        [
            ("窅つにーーぷむえ窅つぽ", "窅つに――ぷむえ窅つぽ"),
            ("ーあむえ褗かゑ", "―あむえ褗かゑ"),
            ("ーーあむえ褗かゑ", "――あむえ褗かゑ"),
            ("誎ぞ辂貁み。ーーうゐで粝に", "誎ぞ辂貁み。――うゐで粝に"),
            ("矉孱ーーづ漗げ唞", "矉孱――づ漗げ唞"),
            ("ゐめぽーー　うして", "ゐめぽ――　うして"),
            ("―—瓇り輌。", "――瓇り輌。"),
            ("侸瑍れってーー」", "侸瑍れって――」"),
            ("A—B", "A—B"),
            # long vowels stay long vowels
            ("ツーヒーら綦ぬ", "ツーヒーら綦ぬ"),
            ("ぶばーー！", "ぶばーー！"),
            ("あーー、詈むか", "あーー、詈むか"),
            ("ぱー、愈ぞてゐ", "ぱー、愈ぞてゐ"),
            ("ギャーーッ", "ギャーーッ"),
            # the recognizer's ASCII space inside Japanese text is a full-width one
            ("「ぞや！ 遐って！」", "「ぞや！　遐って！」"),
            ("TEL 03 1234", "TEL 03 1234"),
        ],
    )
    def test_rule(self, read: str, fixed: str) -> None:
        assert ll.normalize_text(read) == fixed

    def test_lookalike_kanji_are_still_fixed(self) -> None:
        assert ll.normalize_text("夕イルのべルト") == "タイルのベルト"


class TestHangingPunctuation:
    def test_one_hanging_stop_does_not_set_the_bottom(self) -> None:
        full = 40 * EM
        columns = [vline(1800 - k * PITCH, 100, 100 + full, FULL) for k in range(6)]
        # One full column whose closing stop hangs an em below the last cell...
        columns[1] = vline(1800 - PITCH, 100, 100 + full + EM, FULL + "。")
        # ...and one whose final comma the detector clipped (half an em short).
        columns[3] = vline(1800 - 3 * PITCH, 100, 100 + full - 0.6 * EM, FULL)
        (body,) = ll.find_bodies(measured(columns))
        assert body.bottom == pytest.approx(100 + full)
        assert len(ll.layout_page(page(columns)).blocks) == 1

    def test_sentence_running_over_a_clipped_comma_stays_one_paragraph(self) -> None:
        # novel-hanging-250: the engine's lines AFTER end recovery, where one
        # column's recovered hanging "。" reaches an em below every other.
        layout = ll.layout_page(load("novel-hanging-250"))
        (block,) = [b for b in layout.blocks if b["lines"][0].startswith("ぷめ粝え梴ぼ")]
        assert [line[:6] for line in block["lines"]] == ["ぷめ粝え梴ぼ", "摮っ嬊ら蕈ば"]
        assert len(layout.blocks) == 9


class TestColumnPieces:
    def test_column_cut_at_a_dash(self) -> None:
        head = vline(500, 100, 760, "翆えゐぜづぽぱをれがろ？")
        dash = vline(500, 860, 980, "―", conf=0.4)
        tail = vline(500, 1080, 1800, "熙みもゑ一靮で魊ぬまれさ」")
        other = vline(400, 100, 900, "耵め輢ぽ坁纲もぞ")
        novel = novel_page(TestFuriganaOnALattice.COLUMNS, [])
        offset = len(novel["lines"])
        novel["lines"] += [
            vline(1800 - 9 * PITCH, 200, 860, "翆えゐぜづぽぱをれがろ？"),
            vline(1800 - 9 * PITCH, 900, 1020, "―", conf=0.4),
            vline(1800 - 9 * PITCH, 1080, 1800, "熙みもゑ一靮で魊ぬまれさ」"),
        ]
        assert ll.column_pieces(novel) == [[offset, offset + 1, offset + 2]]
        # Outside a text body the hole a dash leaves is too wide to bridge.
        assert ll.column_pieces(page([head, dash, tail, other])) == []

    def test_unread_piece_joins_the_column_it_was_cut_from(self) -> None:
        blank = vline(500, 95, 160, "", t=70, conf=0.0)
        rest = vline(500, 150, 600, "ワーら旬糳で")
        stray = vline(1500, 2000, 2080, "", conf=0.0)
        assert ll.column_pieces(
            page([rest, blank, stray, vline(430, 100, 600, "頇蟽しもえゑ")])
        ) == [[0, 1]]

    def test_ruby_and_neighbouring_columns_never_join(self) -> None:
        ruby = vline(545, 100, 220, "かんじ", t=28)
        lines = [BASE, ruby, vline(570, 100, 700, "二行目の台詞です")]
        assert [r.text for r in ll.layout_page(page(lines)).ruby] == ["かんじ"]
        assert ll.column_pieces(page(lines)) == []

    def test_real_pages(self) -> None:
        # novel-dialogue-200: 「翆えゐぜづぽぱをれがろ？ / ― / 熙みもゑ一靮で魊ぬまれさ」
        page_200 = load("novel-dialogue-200")
        groups = [{page_200["lines"][i]["text"] for i in g} for g in ll.column_pieces(page_200)]
        assert groups == [{"翆えゐぜづぽぱをれがろ？", "―", "熙みもゑ一靮で魊ぬまれさ」"}]
        # manga-page-066: the "マ" of "マワーら旬糳で" came back tilted and unread.
        page_066 = load("manga-page-066")
        groups = [{page_066["lines"][i]["text"] for i in g} for g in ll.column_pieces(page_066)]
        assert groups == [{"", "ワーら旬糳で"}]
        # novel-text-097: one column boxed as two overlapping pieces.
        page_097 = load("novel-text-097")
        groups = [[page_097["lines"][i]["text"] for i in g] for g in ll.column_pieces(page_097)]
        assert groups == [
            ["、もめにわ", "ぜめ羀錨ぽ豩豸鱋め羀錨ら涜ほだゐ。誎ぼ錿ゑむか趑、跩やつも禡め炕癲"]
        ]
        for name in ("novel-text-250", "novel-text-013", "manga-page-069", "novel-map-006"):
            assert ll.column_pieces(load(name)) == [], name


class TestBodyQuadMargin:
    def test_body_columns_are_widened_but_never_into_the_gutter(self) -> None:
        layout = ll.layout_page(novel_page([(0, 0, FULL)] * 8))
        (block,) = layout.blocks
        quad = block["lines_coords"][0]
        # 60 px of ink, 5% a side; the 40 px gutter allows up to 20
        assert (quad[0][0], quad[1][0]) == (1800 - 33, 1800 + 33)
        assert (quad[0][1], quad[2][1]) == (int(BODY_TOP), int(BODY_BOTTOM))  # length untouched
        assert block["font_size"] == 66
        tight = [vline(1800 - k * 62, BODY_TOP, BODY_BOTTOM, FULL) for k in range(8)]
        quad = ll.layout_page(page(tight)).blocks[0]["lines_coords"][0]
        assert (quad[0][0], quad[1][0]) == (1800 - 31, 1800 + 31)  # 2 px gutter: 1 px a side

    def test_a_tilted_row_is_widened_along_its_own_normal(self) -> None:
        (line,) = measured([rotated(hline(500, 100, 900, "横書きの文章です"), 30, (500, 500))])
        wide = ll._widened(line.quad, vertical=False, margin=6.0)
        width, height, angle = ll.quad_frame(wide)
        assert (width, height, angle) == pytest.approx((800, 72, 30), abs=0.01)

    def test_manga_bubbles_keep_the_detector_quads(self) -> None:
        block = ll.layout_page(page([BASE])).blocks[0]
        xs = sorted({p[0] for p in block["lines_coords"][0]})
        assert xs[1] - xs[0] == EM


class TestBoxHoldsTheRuby:
    """The reader whites out a block's ``box`` and draws the text on it.

    Furigana stands in the gutter to the right of its column: inside the
    paragraph where the column has a neighbour on that side, OUTSIDE the
    lines' bounds where it is the paragraph's first column. A box that stops
    at the lines cuts those glyphs in half (bench temp00097, temp00200).
    """

    RUN = vline(548, 110, 200, "かんじ", t=30)  # 3 px off BASE's right edge

    def test_box_takes_in_the_ruby_of_its_lines(self) -> None:
        bare = ll.layout_page(page([BASE])).blocks[0]
        assert bare["box"] == [470, 100, 530, 580]
        layout = ll.layout_page(page([BASE, self.RUN]))
        (block,) = layout.blocks
        assert [r.text for r in layout.ruby] == ["かんじ"]
        assert block["box"] == [470, 100, 563, 580]
        # only the box: the text is still laid out on the lines alone
        assert {k: v for k, v in block.items() if k != "box"} == {
            k: v for k, v in bare.items() if k != "box"
        }

    def test_a_run_overhanging_the_column_and_the_page_is_clamped(self) -> None:
        high = vline(548, 60, 160, "かんじ", t=30)
        (block,) = ll.layout_page(page([BASE, high], width=560, height=600)).blocks
        assert block["box"] == [470, 60, 560, 580]

    def test_ruby_above_a_horizontal_line(self) -> None:
        row = hline(500, 100, 580, "漢字を含む台詞だ")
        run = hline(452, 110, 200, "かんじ", t=30)
        (block,) = ll.layout_page(page([row, run])).blocks
        assert block["box"] == [100, 437, 580, 530]

    # two paragraphs: columns 0-3, then an indented column 4 opens the second
    COLUMNS = [(0, 0, FULL)] * 3 + [(0, 20, "終わり。"), (1, 0, FULL)] + [(0, 0, FULL)] * 3
    SECOND_X = 1800 - 4 * PITCH  # the second paragraph's first column

    def second_box(self, run: dict[str, Any]) -> list[int]:
        layout = ll.layout_page(novel_page(self.COLUMNS, [run]))
        assert [r.text for r in layout.ruby] == ["かな"]
        assert len(layout.blocks) == 2
        return layout.blocks[1]["box"]

    def test_first_column_of_a_paragraph_grows_into_the_gutter(self) -> None:
        bare = ll.layout_page(novel_page(self.COLUMNS)).blocks[1]["box"]
        assert bare[2] == self.SECOND_X + 33  # the widened column, no further
        run = vline(self.SECOND_X + 48, 900, 1020, "かな", t=30)
        assert self.second_box(run) == [bare[0], bare[1], self.SECOND_X + 63, bare[3]]

    def test_never_into_the_neighbouring_paragraph(self) -> None:
        bare = ll.layout_page(novel_page(self.COLUMNS)).blocks[1]["box"]
        # The detector pads ruby quads: a fat run reaching 3 px into the previous
        # paragraph's last column. The box stops AT that column (widened: +67).
        fat = vline(self.SECOND_X + 50, 900, 1020, "かな", t=40)
        assert self.second_box(fat) == [bare[0], bare[1], self.SECOND_X + 67, bare[3]]
        first = ll.layout_page(novel_page(self.COLUMNS, [fat])).blocks[0]
        assert min(p[0] for p in first["lines_coords"][-1]) == self.SECOND_X + 67

    def test_the_whole_strip_has_to_be_clear_not_just_the_run(self) -> None:
        # The same fat run lower down, where the neighbouring column (a short
        # paragraph end) has stopped: the run is clear of it, but a box is a
        # rectangle, and growing it all the way would white out the edge of
        # that column higher up.
        bare = ll.layout_page(novel_page(self.COLUMNS)).blocks[1]["box"]
        low = vline(self.SECOND_X + 50, 2000, 2120, "かな", t=40)
        assert self.second_box(low) == [bare[0], bare[1], self.SECOND_X + 67, bare[3]]

    def test_a_line_of_another_orientation_is_a_neighbour_too(self) -> None:
        caption = hline(150, 540, 900, "横書きの見出し")
        blocks = ll.layout_page(page([BASE, self.RUN, caption])).blocks
        assert [b["box"] for b in blocks if b["vertical"]] == [[470, 100, 540, 580]]

    def test_a_tilted_neighbour_is_met_as_a_quad_not_as_its_bounds(self) -> None:
        # A shout leaning 2 degrees: its axis-aligned bounds reach over the
        # strip the run adds, the shout itself stays 4 px clear of it.
        shout = rotated(vline(684, 100, 1300, "斜めの叫び声", t=200), 2, (684, 100))
        layout = ll.layout_page(page([BASE, self.RUN, shout]))
        assert len(layout.blocks) == 2 and len(layout.ruby) == 1
        assert min(p[0] for p in shout["quad"]) < 563
        assert [470, 100, 563, 580] in [b["box"] for b in layout.blocks]

    def test_noise_is_not_a_neighbour(self) -> None:
        # never drawn (the runner leaves noise out of the page), so nothing to protect
        smudge = raw(box(540, 300, 640, 420), "刂忍", conf=0.19)
        layout = ll.layout_page(page([BASE, self.RUN, smudge]))
        assert layout.kinds.count("noise") == 1 and len(layout.ruby) == 1
        assert [470, 100, 563, 580] in [b["box"] for b in layout.blocks]

    def test_input_order_of_the_runs_does_not_matter(self) -> None:
        runs = [self.RUN, vline(552, 300, 420, "よみ", t=36), vline(546, 60, 160, "かな", t=28)]
        boxes = {
            tuple(ll.layout_page(page([BASE, *order])).blocks[0]["box"])
            for order in (runs, runs[::-1], [runs[1], runs[2], runs[0]])
        }
        assert boxes == {(470, 60, 570, 580)}

    @pytest.mark.parametrize(
        ("name", "runs", "grown", "short"),
        [("novel-text-097", 18, 6, 7), ("novel-dialogue-200", 4, 4, 2)],
    )
    def test_real_pages(self, name: str, runs: int, grown: int, short: int) -> None:
        # Runs between two columns of one paragraph were inside the box all
        # along; the ones beside a paragraph's FIRST column are what the box
        # grows for (6 of the 8 paragraphs of temp00097). ``short``: runs
        # whose padded quad overlaps the previous paragraph's last column --
        # the box stops at that column, a few px of padding short of the quad
        # (bench, 292 pages: the ink of 3081 of 3397 runs is inside the box,
        # against 1010 before; the rest sit beside a column that a leaning
        # neighbour crosses further up or down).
        layout = ll.layout_page(load(name))
        assert len(layout.ruby) == runs
        assert len(grown_blocks(layout)) == grown
        outside = [run for run in layout.ruby if not holds_run(layout, run)]
        assert len(outside) == short
        gutter = max(body.gap for body in layout.bodies)
        for run in outside:
            (k,) = [k for k, group in enumerate(layout.groups) if run.base in group]
            assert max(p[0] for p in run.quad) - layout.blocks[k]["box"][2] < gutter / 2 + 1


class TestArtworkReadAsGlyphs:
    def test_a_page_whose_only_text_is_one_doubted_glyph_is_artwork(self) -> None:
        # bench temp00069 / temp00035: leopard spots "ぐ" 0.77, hatching "AL" 0.84
        spots = raw(box(800, 1200, 870, 1290), "ぐ", conf=0.77)
        assert ll.layout_page(page([spots])).kinds == ["noise"]
        hatching = rotated(hline(600, 300, 400, "AL", t=40, conf=0.84), -45, (350, 600))
        assert ll.layout_page(page([hatching])).kinds == ["noise"]

    def test_a_splash_page_of_sound_effects_keeps_them(self) -> None:
        # bench OPM 065: nothing on the page but "ザッ" (0.81) and "ガッ" (0.90)
        effects = [
            raw(box(300, 400, 420, 640), "ザッ", conf=0.81),
            raw(box(1100, 1500, 1250, 1790), "ガッ", conf=0.9),
        ]
        assert ll.layout_page(page(effects)).kinds == ["text", "text"]

    def test_the_same_glyph_beside_real_text_is_a_sound_effect(self) -> None:
        shout = raw(box(800, 1200, 1080, 1700), "ず", conf=0.63)
        layout = ll.layout_page(page([shout, BASE]))
        assert layout.kinds.count("text") == 2

    def test_a_confident_or_kanji_glyph_alone_stays(self) -> None:
        assert ll.layout_page(page([raw(box(800, 1200, 870, 1290), "ぐ", conf=0.95)])).kinds == [
            "text"
        ]
        assert ll.layout_page(page([raw(box(800, 1200, 870, 1290), "完", conf=0.7)])).kinds == [
            "text"
        ]

    def test_a_doubted_numeral_alone_on_a_text_page_is_a_section_number(self) -> None:
        # Why there is no "short, doubted, not Japanese, no neighbours = artwork"
        # rule for the map's compass ("W―" 0.72, its box over half the rose):
        # bench, 292 novel pages, the same rule takes out the section numbers
        # "7" (0.73), "3" (0.84) and "8" (0.70), set alone between two chapters'
        # columns -- and nothing else but one stray "s".
        number = raw(box(1077, 383, 1119, 437), "7", conf=0.73)
        columns = [vline(1800 - k * PITCH, BODY_TOP, BODY_BOTTOM, FULL) for k in (0, 1, 2, 3)]
        columns += [vline(1800 - k * PITCH, BODY_TOP, BODY_BOTTOM, FULL) for k in (10, 11, 12, 13)]
        layout = ll.layout_page(page([*columns, number]))
        assert ["7"] in texts(layout) and "noise" not in layout.kinds
        compass = ll.layout_page(load("novel-map-006"))
        assert compass.kinds[texts(compass).index(["W―"])] == "text"


class TestMisreadRubyOnALattice:
    """Ruby runs the recognizer read as something else, confidently.

    Bench (292 novel pages): "きよ灯" 0.71, "!わた" 0.85, "…おく" 0.80 -- three
    glyphs, so not "unreadable" by count, no kana-only text, each left as a
    stray line that split the paragraph it sat in. On a page with a column
    lattice size and place are enough: nothing but ruby is set at under two
    thirds of the body size in the gutter beside a column.
    """

    COLUMNS = [(0, 0, FULL)] * 8

    def test_half_size_run_in_the_gutter_is_ruby_whatever_it_reads(self) -> None:
        run = vline(1800 - 2 * PITCH + 48, 900, 1020, "きよ灯", t=34, conf=0.71)
        layout = ll.layout_page(novel_page(self.COLUMNS, [run]))
        assert [r.text for r in layout.ruby] == ["きよ灯"]
        assert len(layout.blocks) == 1

    def test_stop_caught_in_front_of_a_ruby_run(self) -> None:
        assert ll.is_ruby_script("。よこだお") and not ll.is_ruby_script("た。")
        assert not ll.is_ruby_script("。")

    def test_full_size_short_line_in_a_body_is_text(self) -> None:
        columns = [*self.COLUMNS[:4], (0, 37, "!わた"), *self.COLUMNS[4:]]
        layout = ll.layout_page(novel_page(columns))
        assert layout.ruby == []

    def test_without_a_lattice_small_print_is_not_judged_by_size(self) -> None:
        aside = vline(548, 300, 420, "注意!", t=38, conf=0.85)
        assert ll.layout_page(page([BASE, aside])).ruby == []


class TestTinyKanjiBesideAColumn:
    def test_tail_of_a_ruby_run_read_as_a_kanji(self) -> None:
        # manga-page-066: "萮" (0.73), 0.34 of the column it hugs. No lattice here.
        layout = ll.layout_page(load("manga-page-066"))
        assert "萮" in [r.text for r in layout.ruby]
        assert all("萮" not in block["lines"] for block in layout.blocks)

    def test_small_kanji_aside_is_not_ruby(self) -> None:
        aside = vline(545, 100, 190, "注", t=36)  # 0.6 em: small print, not ruby-small
        layout = ll.layout_page(page([BASE, aside]))
        assert layout.ruby == []


# --------------------------------------------------------------------------
# properties
# --------------------------------------------------------------------------


def synthetic_pages() -> list[dict[str, Any]]:
    cols = [
        (0, 0, FULL),
        (1, 0, FULL),
        (0, 20, "恺ほり。"),
        (0.45, 30, "「ろま」"),
        (1, 0, FULL),
        (0, 0, FULL),
    ]
    extras = [vline(1800 - PITCH + 50, 900, 1020, "つも", t=30), hline(2740, 950, 1010, "12", t=36)]
    bubble = [vline(900 - k * 70, 300, 700, f"憕びめ婫徿{k}") for k in range(3)]
    return [
        novel_page(cols, extras),
        page(
            [rotated(ln, -15, (830, 500)) for ln in bubble]
            + [hline(2000, 100, 900, "caption text")]
        ),
        page([vline(100, 0, 100, ""), raw(box(5, 5, 5, 5), "x"), BASE]),
        page([]),
    ]


def all_pages() -> list[tuple[str, dict[str, Any]]]:
    pages = [(name, load(name)) for name in ALL_FIXTURES]
    return pages + [(f"synthetic-{k}", p) for k, p in enumerate(synthetic_pages())]


@pytest.mark.parametrize(("name", "raw_page"), all_pages(), ids=[n for n, _ in all_pages()])
class TestProperties:
    def test_every_line_lands_exactly_once(self, name: str, raw_page: dict[str, Any]) -> None:
        layout = ll.layout_page(raw_page)
        placed = [i for group in layout.groups for i in group]
        placed += [r.line for r in layout.ruby] + layout.dropped
        assert sorted(placed) == list(range(len(raw_page["lines"])))
        for index in layout.dropped:
            entry = raw_page["lines"][index]
            assert (
                not entry["text"].strip()
                or ll.canonical_quad(entry["quad"]) is None
                or (min(ll.quad_frame(ll.canonical_quad(entry["quad"]))[:2]) <= 0)
            )

    def test_blocks_carry_their_lines_in_order(self, name: str, raw_page: dict[str, Any]) -> None:
        layout = ll.layout_page(raw_page)
        for block, group in zip(layout.blocks, layout.groups, strict=True):
            # the input text, but for the systematic slips normalize_text undoes
            assert block["lines"] == [
                ll.normalize_text(raw_page["lines"][i]["text"].strip()) for i in group
            ]

    def test_no_block_mixes_orientations(self, name: str, raw_page: dict[str, Any]) -> None:
        lines = {ln.index: ln for ln in measured(raw_page["lines"])}
        layout = ll.layout_page(raw_page)
        for block, group in zip(layout.blocks, layout.groups, strict=True):
            assert {lines[i].vertical for i in group} == {block["vertical"]}

    def test_boxes_are_inside_the_page_and_hold_their_lines(
        self, name: str, raw_page: dict[str, Any]
    ) -> None:
        for block in ll.layout_page(raw_page).blocks:
            x0, y0, x1, y1 = block["box"]
            assert 0 <= x0 <= x1 <= raw_page["width"] and 0 <= y0 <= y1 <= raw_page["height"]
            assert block["font_size"] > 0

    def test_a_box_holds_its_lines_and_stays_off_its_neighbours(
        self, name: str, raw_page: dict[str, Any]
    ) -> None:
        layout = ll.layout_page(raw_page)
        drawn = [k for k, kind in enumerate(layout.kinds) if kind != "noise"]
        for k, block in enumerate(layout.blocks):
            x0, y0, x1, y1 = block["box"]
            lx0, ly0, lx1, ly1 = lines_bounds(block)
            w, h = raw_page["width"], raw_page["height"]
            assert x0 <= max(lx0, 0) + 1 and y0 <= max(ly0, 0) + 1
            assert x1 >= min(lx1, w) - 1 and y1 >= min(ly1, h) - 1
            # where the box reaches past its lines (for ruby), no other block's
            # line may lie under it: sample every neighbouring quad densely
            for other in drawn:
                if other == k:
                    continue
                for quad in layout.blocks[other]["lines_coords"]:
                    for px, py in quad_samples(quad):
                        inside_box = x0 < px < x1 and y0 < py < y1
                        inside_lines = lx0 - 1 <= px <= lx1 + 1 and ly0 - 1 <= py <= ly1 + 1
                        assert not inside_box or inside_lines, (block["lines"][0][:6], px, py)

    def test_ruby_stays_with_the_block_of_its_base(
        self, name: str, raw_page: dict[str, Any]
    ) -> None:
        layout = ll.layout_page(raw_page)
        placed = {i for group in layout.groups for i in group}
        assert all(run.base in placed for run in layout.ruby)

    def test_deterministic(self, name: str, raw_page: dict[str, Any]) -> None:
        first = ll.layout_page(json.loads(json.dumps(raw_page)))
        second = ll.layout_page(json.loads(json.dumps(raw_page)))
        assert first.blocks == second.blocks and first.ruby == second.ruby

    def test_input_order_does_not_matter(self, name: str, raw_page: dict[str, Any]) -> None:
        flipped = {**raw_page, "lines": list(reversed(raw_page["lines"]))}
        assert ll.layout_page(flipped).blocks == ll.layout_page(raw_page).blocks
