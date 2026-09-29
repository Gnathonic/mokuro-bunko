"""Tests for ``mokuro_bunko.ocr.ppocr`` and the ``ppocr_manga`` detector adapter.

Nothing here needs the ONNX models or the network: the network-facing parts
are thin, and everything that decides geometry or text (DB post-processing,
quad ordering, CTC decoding, window stitching, tile-seam merging, the
dense-page policy) is a pure function exercised on synthetic inputs. The JSON
files under ``tests/fixtures/ppocr`` are real raw page outputs, cached the way
Manatan caches engine output for its merger tests -- with every word replaced,
glyph for glyph, by a stand-in of the same script class
(``tests/fixtures/ppocr/anonymise.py``): the pages come from commercial books,
and nothing under test reads the words.

numpy + OpenCV live only in the OCR engines venv, so the whole module is
skipped where they are missing.
"""

from __future__ import annotations

import json
import math
import subprocess
import sys
from pathlib import Path

import pytest

np = pytest.importorskip("numpy")
cv2 = pytest.importorskip("cv2")

from mokuro_bunko.ocr import ppocr  # noqa: E402
from mokuro_bunko.ocr.detectors import ppocr_manga  # noqa: E402

FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "ppocr"


def rect(cx: float, cy: float, w: float, h: float, angle: float) -> np.ndarray:
    return ppocr.rect_to_quad((cx, cy), (w, h), angle)


def draw(prob: np.ndarray, quad: np.ndarray, value: float = 0.9) -> None:
    cv2.fillPoly(prob, [np.round(quad).astype(np.int32)], value)


# ---------------------------------------------------------------------------
# quad ordering
# ---------------------------------------------------------------------------


class TestOrderQuad:
    @pytest.mark.parametrize("shift", range(4))
    @pytest.mark.parametrize("reverse", [False, True])
    def test_upright_column_any_input_order(self, shift: int, reverse: bool) -> None:
        corners = np.array([[100, 50], [140, 50], [140, 450], [100, 450]], np.float32)
        pts = np.roll(corners[::-1] if reverse else corners, shift, axis=0)
        quad = ppocr.order_quad(pts)
        assert quad.tolist() == corners.tolist()
        assert ppocr.quad_is_vertical(quad)
        assert ppocr.quad_angle(quad) == pytest.approx(0.0, abs=1e-4)

    def test_horizontal_line(self) -> None:
        quad = ppocr.order_quad([[300, 80], [20, 80], [20, 40], [300, 40]])
        assert quad.tolist() == [[20, 40], [300, 40], [300, 80], [20, 80]]
        assert not ppocr.quad_is_vertical(quad)
        assert ppocr.quad_size(quad) == pytest.approx((280, 40))

    @pytest.mark.parametrize("tilt", [-30.0, -12.0, 8.0, 25.0, 40.0])
    def test_tilted_column_keeps_its_narrow_top_edge_first(self, tilt: float) -> None:
        # A 40x400 column leaning by `tilt`: the true angle survives, and the
        # first edge is still the narrow top one (PaddleOCR's sort-by-x
        # ordering starts a leaning column from the wrong corner).
        quad = ppocr.order_quad(rect(500, 500, 40, 400, tilt)[[2, 0, 3, 1]])
        assert ppocr.quad_size(quad) == pytest.approx((40, 400), abs=1e-2)
        assert ppocr.quad_is_vertical(quad)
        assert ppocr.quad_angle(quad) == pytest.approx(tilt, abs=1e-3)
        # Reading direction (top edge midpoint -> bottom edge midpoint) is downward.
        assert (quad[2] + quad[3])[1] > (quad[0] + quad[1])[1]

    @pytest.mark.parametrize("tilt", [-35.0, -10.0, 15.0, 44.0])
    def test_tilted_horizontal_line(self, tilt: float) -> None:
        quad = ppocr.order_quad(rect(500, 500, 400, 40, tilt)[[1, 3, 0, 2]])
        assert ppocr.quad_size(quad) == pytest.approx((400, 40), abs=1e-2)
        assert not ppocr.quad_is_vertical(quad)
        assert ppocr.quad_angle(quad) == pytest.approx(tilt, abs=1e-3)
        # Reading direction is rightward.
        assert (quad[1] + quad[2])[0] > (quad[0] + quad[3])[0]

    def test_positive_angle_is_clockwise_on_screen(self) -> None:
        # y grows downward: a clockwise turn lowers the right end of the top edge.
        quad = ppocr.order_quad(rect(0, 0, 200, 20, 10.0))
        assert quad[1][1] > quad[0][1]
        assert ppocr.quad_angle(quad) == pytest.approx(10.0, abs=1e-3)

    def test_idempotent(self) -> None:
        quad = ppocr.order_quad(rect(10, 20, 30, 300, -17.0))
        assert np.allclose(ppocr.order_quad(quad), quad)


# ---------------------------------------------------------------------------
# DB post-processing
# ---------------------------------------------------------------------------


class TestDbPostprocess:
    def test_two_rotated_rectangles(self) -> None:
        prob = np.zeros((640, 640), np.float32)
        column = rect(150, 320, 20, 400, 12.0)  # leaning vertical column
        shout = rect(430, 200, 260, 30, -25.0)  # slanted horizontal line
        draw(prob, column)
        draw(prob, shout)
        found = ppocr.db_postprocess(prob, unclip_ratio=1.5)
        assert len(found) == 2
        found.sort(key=lambda item: float(item[0][:, 0].mean()))
        (col_quad, col_score), (shout_quad, shout_score) = found

        assert ppocr.quad_is_vertical(col_quad)
        assert ppocr.quad_angle(col_quad) == pytest.approx(12.0, abs=1.0)
        assert not ppocr.quad_is_vertical(shout_quad)
        assert ppocr.quad_angle(shout_quad) == pytest.approx(-25.0, abs=1.0)
        assert col_score == pytest.approx(0.9, abs=0.02)
        assert shout_score == pytest.approx(0.9, abs=0.02)

        # Unclip grows every side by d = area * ratio / perimeter of the
        # region the network drew (ratio 0 gives that region's own rectangle,
        # which is the drawn one plus about a pixel of rasterisation).
        kernels = sorted(
            (q for q, _ in ppocr.db_postprocess(prob, unclip_ratio=0.0)),
            key=lambda q: float(q[:, 0].mean()),
        )
        drawn_sizes = ((20, 400), (260, 30))
        for quad, kernel, drawn in zip((col_quad, shout_quad), kernels, drawn_sizes, strict=True):
            w, h = ppocr.quad_size(kernel)
            assert (w, h) == pytest.approx(drawn, abs=3.0)
            d = ppocr.unclip_distance(w, h, 1.5)
            assert d > 5
            assert ppocr.quad_size(quad) == pytest.approx((w + 2 * d, h + 2 * d), abs=0.05)
        assert np.allclose(col_quad.mean(axis=0), [150, 320], atol=1.0)

    def test_unclip_ratio_scales_growth(self) -> None:
        prob = np.zeros((300, 300), np.float32)
        draw(prob, rect(150, 150, 30, 200, 0.0))
        w, h = ppocr.quad_size(ppocr.db_postprocess(prob, unclip_ratio=0.0)[0][0])
        small = ppocr.quad_size(ppocr.db_postprocess(prob, unclip_ratio=1.0)[0][0])
        large = ppocr.quad_size(ppocr.db_postprocess(prob, unclip_ratio=2.0)[0][0])
        d1, d2 = ppocr.unclip_distance(w, h, 1.0), ppocr.unclip_distance(w, h, 2.0)
        assert d2 == pytest.approx(2 * d1)
        assert large[0] - small[0] == pytest.approx(2 * (d2 - d1), abs=0.05)
        assert large[1] - small[1] == pytest.approx(2 * (d2 - d1), abs=0.05)

    def test_weak_and_tiny_regions_are_dropped(self) -> None:
        prob = np.zeros((200, 200), np.float32)
        draw(prob, rect(50, 50, 40, 40, 0.0), 0.2)  # above thresh, below box_thresh
        draw(prob, rect(150, 150, 2, 30, 0.0), 0.9)  # thinner than min_side
        assert ppocr.db_postprocess(prob) == []
        assert len(ppocr.db_postprocess(prob, box_thresh=0.1)) == 1

    def test_closed_form_unclip_matches_pyclipper(self) -> None:
        # PaddleOCR offsets the rectangle with pyclipper and takes the
        # min-area rectangle of the rounded result; ours is the closed form.
        pyclipper = pytest.importorskip("pyclipper")
        for w, h, angle in ((24.0, 410.0, 7.0), (300.0, 36.0, -20.0), (40.0, 44.0, 0.0)):
            box = rect(500, 500, w, h, angle)
            d = ppocr.unclip_distance(w, h, 1.5)
            offset = pyclipper.PyclipperOffset()
            offset.AddPath(
                [tuple(p) for p in np.round(box * 100).astype(int).tolist()],
                pyclipper.JT_ROUND,
                pyclipper.ET_CLOSEDPOLYGON,
            )
            grown = np.array(offset.Execute(d * 100)[0], np.float32) / 100
            (_, _), (rw, rh), _ = cv2.minAreaRect(grown.reshape(-1, 1, 2))
            assert sorted((rw, rh)) == pytest.approx(sorted((w + 2 * d, h + 2 * d)), abs=0.1)


class TestDetectorInput:
    def test_multiple_of_32_and_longest_side(self) -> None:
        assert ppocr.detector_input_size(1925, 2800, 1280) == (896, 1280)
        assert ppocr.detector_input_size(2800, 1925, 1280) == (1280, 896)
        for w, h in ((1703, 2800), (800, 1131), (3850, 2800)):
            nw, nh = ppocr.detector_input_size(w, h, 1280)
            assert nw % 32 == 0 and nh % 32 == 0

    def test_never_upscales_beyond_cap(self) -> None:
        nw, nh = ppocr.detector_input_size(400, 600, 1280)
        assert nh == 896  # 600 * 1.5 = 900 -> nearest multiple of 32
        assert nw == 608

    def test_tensor_is_imagenet_normalised_bgr(self) -> None:
        img = np.full((64, 64, 3), 255, np.uint8)
        x = ppocr.detector_tensor(img, (64, 64))
        assert x.shape == (1, 3, 64, 64) and x.dtype == np.float32
        expected = [
            (1 - m) / s for m, s in zip(ppocr.IMAGENET_MEAN, ppocr.IMAGENET_STD, strict=True)
        ]
        assert x[0, :, 0, 0].tolist() == pytest.approx(expected, rel=1e-5)


# ---------------------------------------------------------------------------
# dense-page policy and tiling
# ---------------------------------------------------------------------------


class TestDensePolicy:
    def test_novel_page_at_good_scale_needs_nothing(self) -> None:
        # 19 columns ~30 px thick and ~1100 px long, plus ruby runs.
        thick = [30.0] * 19 + [13.0] * 12
        longs = [1100.0] * 19 + [60.0] * 12
        assert ppocr.needs_fine_pass(thick, longs) is None

    def test_thin_columns_trigger(self) -> None:
        thick = [18.0] * 40 + [8.0] * 10
        longs = [800.0] * 40 + [30.0] * 10
        why = ppocr.needs_fine_pass(thick, longs)
        assert why is not None and "18.0px" in why

    def test_manga_page_with_small_type_does_not_trigger(self) -> None:
        # Bubbles of 3-8 glyphs, 19 px thick at side 1280 (real OPM numbers):
        # too few LONG lines for the page to be judged dense.
        thick = [19.0] * 25 + [9.0] * 20 + [45.0] * 3
        longs = [110.0] * 25 + [40.0] * 20 + [150.0] * 3
        assert ppocr.needs_fine_pass(thick, longs) is None

    def test_fused_columns_trigger(self) -> None:
        thick = [30.0] * 12 + [62.0] * 3
        longs = [1000.0] * 15
        why = ppocr.needs_fine_pass(thick, longs)
        assert why is not None and "3 boxes" in why

    def test_fine_scale_reaches_target_but_never_enlarges(self) -> None:
        assert ppocr.fine_scale(16.0, 0.33) == pytest.approx(0.33 * 32 / 16)
        assert ppocr.fine_scale(10.0, 0.8) == 1.0
        assert ppocr.fine_scale(0.0, 0.5) == 1.0


class TestTiling:
    def test_grid_covers_page_with_overlap(self) -> None:
        grid = ppocr.tile_grid(3850, 2800, tile=1536, overlap=256)
        cover = np.zeros((2800, 3850), np.uint8)
        for x0, y0, x1, y1 in grid:
            assert 0 <= x0 < x1 <= 3850 and 0 <= y0 < y1 <= 2800
            assert x1 - x0 == 1536 and y1 - y0 == 1536
            cover[y0:y1, x0:x1] += 1
        assert cover.min() >= 1
        xs = sorted({x0 for x0, _, _, _ in grid})
        assert all(b - a <= 1536 - 256 for a, b in zip(xs, xs[1:], strict=False))

    def test_small_axis_is_not_split(self) -> None:
        assert ppocr.tile_grid(800, 1000, tile=1536, overlap=256) == [(0, 0, 800, 1000)]
        strip = ppocr.tile_grid(800, 6000, tile=1536, overlap=256)
        assert {(x0, x1) for x0, _, x1, _ in strip} == {(0, 800)}
        assert len(strip) == 5

    def test_boxes_cut_by_an_inner_tile_edge(self) -> None:
        page, overlap = (3850, 2800), 256
        tile = (1157, 0, 2693, 1536)  # inner edges: left, right and bottom

        def clipped(quad: np.ndarray) -> bool:
            return ppocr.clipped_by_tile(quad, tile, page, overlap)

        # A column sliced lengthwise by the right edge: only a sliver is seen.
        assert clipped(ppocr.order_quad(rect(2680, 700, 30, 1200, 0.0)))
        # (DBNet fades before the border: the sliver stops a few px short of it.)
        assert clipped(ppocr.order_quad(rect(2673, 700, 30, 1200, 0.0)))
        # A glyph cut in half by the bottom edge, a short line ending at it.
        assert clipped(ppocr.order_quad(rect(1800, 1520, 60, 40, 0.0)))
        assert clipped(ppocr.order_quad(rect(1800, 1476, 60, 120, 0.0)))
        # A long column crossing the bottom edge is kept for merging...
        assert not clipped(ppocr.order_quad(rect(1800, 1000, 60, 1080, 0.0)))
        # ...as is anything that touches no inner edge,
        assert not clipped(ppocr.order_quad(rect(1800, 700, 60, 1200, 0.0)))
        # and the page border is not a seam: nobody else will see this one.
        assert not clipped(ppocr.order_quad(rect(1800, 20, 200, 44, 0.0)))
        assert not ppocr.clipped_by_tile(
            ppocr.order_quad(rect(3830, 700, 40, 600, 0.0)), (2314, 0, 3850, 1536), page, overlap
        )

    def test_column_cut_by_a_seam_becomes_one_line(self) -> None:
        # Tile 0 covers y < 1536 and tile 1 covers y >= 1264: each sees only
        # its part of a column running from y=200 to y=2600.
        top = ppocr.order_quad(rect(500, (200 + 1536) / 2, 60, 1536 - 200, 0.0))
        bottom = ppocr.order_quad(rect(501, (1264 + 2600) / 2, 61, 2600 - 1264, 0.0))
        merged = ppocr.merge_tile_lines([top, bottom], [0.9, 0.8], [0, 1])
        assert len(merged) == 1
        quad, score = merged[0]
        assert quad[0][1] == pytest.approx(200, abs=1) and quad[2][1] == pytest.approx(2600, abs=1)
        width, height = ppocr.quad_size(quad)
        assert width == pytest.approx(60.5, abs=1.0) and height == pytest.approx(2400, abs=1)
        assert 0.8 < score < 0.9
        assert ppocr.quad_is_vertical(quad)

    def test_tilted_line_across_a_seam(self) -> None:
        whole = rect(1500, 700, 900, 50, 14.0)
        f = ppocr._frame(whole)
        left = ppocr.order_quad(
            rect(*(f.centre - f.axis * 200), 500, 50, 14.0)  # covers -450..+50 along the axis
        )
        right = ppocr.order_quad(rect(*(f.centre + f.axis * 150), 600, 50, 14.0))  # -150..+450
        merged = ppocr.merge_tile_lines([left, right], [0.9, 0.9], [0, 1])
        assert len(merged) == 1
        assert ppocr.quad_iou(merged[0][0], whole) > 0.97
        assert ppocr.quad_angle(merged[0][0]) == pytest.approx(14.0, abs=0.2)

    def test_same_box_seen_by_two_tiles_is_kept_once(self) -> None:
        a = ppocr.order_quad(rect(1400, 300, 40, 44, 0.0))
        b = ppocr.order_quad(rect(1401, 301, 41, 43, 0.0))
        assert len(ppocr.merge_tile_lines([a, b], [0.7, 0.9], [0, 1])) == 1

    def test_separate_lines_stay_separate(self) -> None:
        upper = ppocr.order_quad(rect(500, 600, 60, 800, 0.0))  # ends at y=1000
        lower = ppocr.order_quad(rect(500, 1500, 60, 900, 0.0))  # starts at y=1050
        neighbour = ppocr.order_quad(rect(570, 600, 60, 800, 0.0))  # the next column
        ruby = ppocr.order_quad(rect(540, 600, 22, 120, 0.0))  # thin run beside it
        merged = ppocr.merge_tile_lines([upper, lower, neighbour, ruby], [0.9] * 4, [0, 1, 1, 1])
        assert len(merged) == 4

    def test_fragment_inside_a_line_from_another_tile_is_dropped(self) -> None:
        column = ppocr.order_quad(rect(1540, 1000, 64, 1900, 0.0))
        sliver = ppocr.order_quad(rect(1522, 600, 20, 500, 0.0))  # too thin to be "the same line"
        ruby = ppocr.order_quad(rect(1590, 600, 30, 160, 0.0))  # beside the column, not inside it
        merged = ppocr.merge_tile_lines([column, sliver, ruby], [0.9, 0.6, 0.8], [1, 0, 0])
        assert len(merged) == 2
        assert not any(np.allclose(q, sliver) for q, _ in merged)
        # The same nesting reported by ONE tile is the detector's opinion: kept.
        assert len(ppocr.merge_tile_lines([column, sliver], [0.9, 0.6], [1, 1])) == 2

    def test_boxes_of_one_tile_are_never_merged(self) -> None:
        a = ppocr.order_quad(rect(500, 600, 60, 800, 0.0))
        b = ppocr.order_quad(rect(500, 900, 60, 800, 0.0))
        assert len(ppocr.merge_tile_lines([a, b], [0.9, 0.9], [2, 2])) == 2
        assert len(ppocr.merge_tile_lines([a, b], [0.9, 0.9], [2, 3])) == 1


# ---------------------------------------------------------------------------
# recognizer: crops, batching, CTC, windows
# ---------------------------------------------------------------------------


class TestCrop:
    def test_vertical_column_is_turned_counter_clockwise(self) -> None:
        page = np.full((600, 400, 3), 255, np.uint8)
        page[100:140, 200:240] = (0, 0, 255)  # red mark at the TOP of the column
        page[460:500, 200:240] = 0  # black mark at the bottom
        crop = ppocr.crop_line(page, ppocr.order_quad(rect(220, 300, 40, 400, 0.0)))
        assert crop.shape[:2] == (40, 400)
        # Top of the column -> left end of the crop.
        assert crop[20, 20].tolist() == [0, 0, 255]
        assert crop[20, 380].tolist() == [0, 0, 0]

    def test_tilted_line_is_deskewed(self) -> None:
        page = np.full((500, 500, 3), 255, np.uint8)
        quad = ppocr.order_quad(rect(250, 250, 300, 40, 20.0))
        cv2.fillPoly(page, [np.round(quad).astype(np.int32)], (0, 0, 0))
        crop = ppocr.crop_line(page, quad)
        assert crop.shape[:2] == (40, 300)
        assert crop[4:-4, 4:-4].max() == 0  # the whole deskewed crop is ink

    def test_recognizer_tensor_shape_and_range(self) -> None:
        crop = np.full((60, 1000, 3), 255, np.uint8)
        x = ppocr.recognizer_tensor(crop)
        assert x.shape == (3, 48, 800) and x.dtype == np.float32
        assert x.max() == pytest.approx(1.0)
        assert ppocr.recognizer_tensor(np.zeros((60, 60, 3), np.uint8)).min() == -1.0

    def test_width_is_a_multiple_of_the_ctc_stride(self) -> None:
        for w, h in ((61, 2437), (2437, 61), (33, 35), (5, 50)):
            width = ppocr.recognizer_width(w, h)
            assert width % ppocr.REC_STRIDE == 0 and width >= ppocr.REC_MIN_WIDTH
            assert width >= 48 * w / h


class TestPlanBatches:
    def test_default_batches_only_equal_widths(self) -> None:
        widths = [320, 96, 320, 960, 96, 104, 960, 320]
        batches = ppocr.plan_batches(widths)
        assert sorted(i for b in batches for i in b) == list(range(len(widths)))
        for batch in batches:
            assert len({widths[i] for i in batch}) == 1

    def test_waste_cap_and_batch_size(self) -> None:
        widths = [100, 98, 96, 60, 58, 20]
        batches = ppocr.plan_batches(widths, max_waste=0.1)
        assert [[widths[i] for i in b] for b in batches] == [[100, 98, 96], [60, 58], [20]]
        assert all(len(b) <= 4 for b in ppocr.plan_batches([64] * 10, max_batch=4))
        assert ppocr.plan_batches([]) == []


def ctc_probs(steps: list[int], classes: int = 8, conf: float = 0.9) -> np.ndarray:
    probs = np.full((len(steps), classes), (1 - conf) / (classes - 1), np.float32)
    for t, cls in enumerate(steps):
        probs[t, cls] = conf
    return probs


class TestCtcGreedy:
    VOCAB = ["", "あ", "い", "う", "。", "ー", "x", " "]

    def test_blank_repeat_and_space(self) -> None:
        #        あ あ _ あ い い _ ' ' _ 。
        steps = [1, 1, 0, 1, 2, 2, 0, 7, 0, 4]
        chars = ppocr.ctc_greedy(ctc_probs(steps), self.VOCAB)
        assert "".join(c.char for c in chars) == "ああい 。"
        assert [c.t for c in chars] == [0, 3, 4, 7, 9]

    def test_confidence_and_position_come_from_the_peak(self) -> None:
        probs = ctc_probs([0, 3, 3, 3, 0])
        probs[2, 3] = 0.99
        (char,) = ppocr.ctc_greedy(probs, self.VOCAB)
        assert (char.char, char.t) == ("う", 2)
        assert char.conf == pytest.approx(0.99)

    def test_adjacent_different_classes_need_no_blank(self) -> None:
        chars = ppocr.ctc_greedy(ctc_probs([1, 2, 3]), self.VOCAB)
        assert "".join(c.char for c in chars) == "あいう"

    def test_length_ignores_padding_and_empty_input(self) -> None:
        probs = ctc_probs([1, 0, 2, 0, 6, 6])
        assert "".join(c.char for c in ppocr.ctc_greedy(probs, self.VOCAB, length=4)) == "あい"
        assert ppocr.ctc_greedy(np.zeros((0, 8), np.float32), self.VOCAB) == []

    def test_class_outside_vocab_is_a_replacement_char(self) -> None:
        assert ppocr.ctc_greedy(ctc_probs([7]), ["", "a"])[0].char == "�"

    def test_load_vocab_adds_blank_and_space(self, tmp_path: Path) -> None:
        path = tmp_path / "dict.txt"
        path.write_text("あ\n　\nい\n", encoding="utf-8")
        assert ppocr.load_vocab(path) == ["", "あ", "　", "い", " "]


class TestWindows:
    def test_short_lines_are_one_window(self) -> None:
        assert ppocr.window_spans(40, 120, 18) == [(0, 40)]
        assert ppocr.window_spans(120 + 18, 120, 18) == [(0, 138)]

    def test_a_full_novel_column_is_read_whole(self) -> None:
        # ~40 glyphs of 61 px at height 48 -> ~1950 px -> 244 timesteps.
        assert ppocr.window_spans(ppocr.recognizer_width(2480, 61) // ppocr.REC_STRIDE) == [
            (0, 244)
        ]
        assert len(ppocr.window_spans(5850 // ppocr.REC_STRIDE)) == 4

    def test_spans_cover_with_overlap_and_equal_width(self) -> None:
        for total in (139, 240, 247, 500, 1000):
            spans = ppocr.window_spans(total, 120, 18)
            assert spans[0][0] == 0 and spans[-1][1] == total
            assert {e - s for s, e in spans} == {120}
            for (_, e0), (s1, _) in zip(spans, spans[1:], strict=False):
                assert e0 - s1 >= 18

    @staticmethod
    def line(total: int, pitch: int = 6, classes: int = 40) -> tuple[np.ndarray, str]:
        """A fixed-pitch line: glyph k peaks for 3 timesteps, then 3 blanks."""
        steps, text = [], []
        for k in range(total // pitch):
            cls = 1 + k % (classes - 1)
            steps += [cls, cls, cls, 0, 0, 0]
            text.append(chr(0x3041 + cls))
        steps += [0] * (total - len(steps))
        return ctc_probs(steps, classes), "".join(text)

    VOCAB = [""] + [chr(0x3041 + i) for i in range(1, 40)]

    def decode(self, probs: np.ndarray) -> str:
        return "".join(c.char for c in ppocr.ctc_greedy(probs, self.VOCAB))

    def test_stitched_windows_decode_like_the_whole_line(self) -> None:
        full, text = self.line(246)
        spans = ppocr.window_spans(246, 120, 18)
        assert len(spans) == 3
        stitched = ppocr.stitch_windows([full[s:e] for s, e in spans], spans)
        assert stitched.shape == full.shape
        assert self.decode(stitched) == text == self.decode(full)

    def test_cut_falls_in_a_gap_both_windows_agree_on(self) -> None:
        # The two windows place one glyph of their overlap a few timesteps
        # apart, on either side of the overlap's midpoint. Cutting at the
        # midpoint reads it twice; cutting where both windows see a blank
        # reads it once.
        full, _ = self.line(246)
        spans = ppocr.window_spans(246, 120, 18)
        (s0, e0), (s1, _) = spans[0], spans[1]
        mid = (s1 + e0) // 2
        views = [full.copy(), full.copy()]
        blank, glyph = ctc_probs([0], 40)[0], ctc_probs([38], 40)[0]
        for view, first in zip(views, (mid - 4, mid + 1), strict=True):
            view[mid - 6 : mid + 6] = blank
            view[first : first + 3] = glyph
        expected = self.decode(views[0])
        assert expected == self.decode(views[1]) and expected.count(self.VOCAB[38]) == 2
        windows = [views[0][s0:e0], views[1][s1 : spans[1][1]], views[1][spans[2][0] :]]

        naive = np.concatenate([windows[0][: mid - s0], views[1][mid:]])
        assert self.decode(naive).count(self.VOCAB[38]) == 3  # the doubled glyph
        assert self.decode(ppocr.stitch_windows(windows, spans)) == expected

    def test_glyphs_sliced_by_a_window_border_are_not_used(self) -> None:
        # A window misreads whatever its own border cuts through; those
        # timesteps lie in the overlap and must come from the other window.
        full, text = self.line(246)
        spans = ppocr.window_spans(246, 120, 18)
        windows = [full[s:e].copy() for s, e in spans]
        junk = ctc_probs([39] * ppocr.REC_WINDOW_GUARD, 40)
        windows[0][-3:], windows[1][:3], windows[1][-3:], windows[2][:3] = junk, junk, junk, junk
        assert self.decode(ppocr.stitch_windows(windows, spans)) == text

    def test_single_window_is_returned_as_is(self) -> None:
        full, _ = self.line(60)
        assert np.array_equal(ppocr.stitch_windows([full], [(0, 60)]), full)


# ---------------------------------------------------------------------------
# model resolution (no network)
# ---------------------------------------------------------------------------


class TestResolveModels:
    @staticmethod
    def populate(base: Path, precision: str = "fp32", flat: bool = False) -> None:
        for rel in (*ppocr.MODEL_FILES[precision], ppocr.DICT_FILE):
            target = base / (Path(rel).name if flat else rel)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(b"x")

    def test_repo_layout(self, tmp_path: Path) -> None:
        self.populate(tmp_path)
        models = ppocr.resolve_models(tmp_path, download=False)
        assert models.detector == tmp_path / "det" / "manga_det_v0.1.onnx"
        assert models.recognizer == tmp_path / "rec" / "manga_rec_v0.1.onnx"
        assert models.dictionary == tmp_path / "ppocrv6_dict.txt"
        assert models.precision == "fp32"

    def test_flat_layout_env_dir_and_precision(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        self.populate(tmp_path, "fp16", flat=True)
        monkeypatch.setenv(ppocr.MODELS_ENV, str(tmp_path))
        monkeypatch.setenv(ppocr.PRECISION_ENV, "fp16")
        monkeypatch.setenv(ppocr.DOWNLOAD_ENV, "0")
        models = ppocr.resolve_models()
        assert models.detector == tmp_path / "manga_det_v0.1_fp16.onnx"
        assert models.precision == "fp16"

    def test_missing_without_download_names_the_file(self, tmp_path: Path) -> None:
        with pytest.raises(FileNotFoundError, match=r"det/manga_det_v0\.1\.onnx"):
            ppocr.resolve_models(tmp_path, download=False)

    def test_missing_files_are_downloaded_into_the_dir(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        fetched: list[tuple[str, Path | None]] = []

        def fake(rel: str, base: Path | None) -> Path:
            fetched.append((rel, base))
            return tmp_path / rel

        monkeypatch.setattr(ppocr, "_download", fake)
        (tmp_path / ppocr.DICT_FILE).write_bytes(b"x")
        models = ppocr.resolve_models(tmp_path, download=True)
        assert [rel for rel, _ in fetched] == list(ppocr.MODEL_FILES["fp32"])
        assert all(base == tmp_path for _, base in fetched)
        assert models.dictionary == tmp_path / ppocr.DICT_FILE
        # The dictionary came out of the directory, so the set as a whole is
        # not the pinned download.
        assert models.pinned is False

    def test_only_the_pinned_download_is_reported_as_pinned(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """``_download`` passes ``REPO_REVISION``; a models directory holds
        whatever was copied into it, so a sidecar must not claim a commit for
        it (see ``engine_runner.PPOcrPageReader``)."""
        monkeypatch.setattr(ppocr, "_download", lambda rel, base: tmp_path / "dl" / rel)
        assert ppocr.resolve_models(tmp_path, download=True).pinned is True
        self.populate(tmp_path)
        assert ppocr.resolve_models(tmp_path, download=False).pinned is False

    def test_unknown_precision(self) -> None:
        with pytest.raises(ValueError, match="unknown precision 'int8'"):
            ppocr.resolve_models(precision="int8", download=False)

    def test_fp32_is_the_cpu_default(self) -> None:
        assert ppocr.DEFAULT_PRECISION == "fp32"


def test_module_imports_without_cv2_or_onnxruntime() -> None:
    code = (
        "import sys; from mokuro_bunko.ocr import ppocr; "
        "bad = [m for m in ('cv2', 'onnxruntime', 'huggingface_hub') if m in sys.modules]; "
        "assert not bad, bad"
    )
    subprocess.run([sys.executable, "-c", code], check=True)


# ---------------------------------------------------------------------------
# raw page JSON + fixtures
# ---------------------------------------------------------------------------

FIXTURE_NAMES = (
    "novel-text-097",
    "novel-text-250",
    "novel-frontmatter-005",
    "novel-map-006",
    "manga-page-066",
    "manga-page-069",
)


class TestPageJson:
    def test_round_trip(self) -> None:
        line = ppocr.Line(
            ppocr.order_quad(rect(100, 300, 40, 500, 5.0)),
            0.91234,
            "ぜまにちぽ",
            0.98765,
            [1.0] * 5,
        )
        page = ppocr.page_to_json([line], 800, 1200, detector={"side": 1280})
        assert page["format"] == ppocr.FORMAT_ID
        assert (page["width"], page["height"]) == (800, 1200)
        entry = page["lines"][0]
        assert set(entry) == {"quad", "score", "text", "conf", "vertical", "angle", "char_confs"}
        assert entry["vertical"] is True and entry["angle"] == pytest.approx(5.0, abs=0.01)
        (back,) = ppocr.lines_from_json(json.loads(json.dumps(page)))
        assert np.allclose(back.quad, line.quad, atol=0.01)
        assert (back.text, back.vertical) == ("ぜまにちぽ", True)

    def test_compact_flavour_drops_char_confs(self) -> None:
        line = ppocr.Line(ppocr.order_quad(rect(10, 10, 9.87654, 40, 0.0)), 0.5, "a", 0.5, [0.5])
        entry = ppocr.page_to_json([line], 100, 100, compact=True)["lines"][0]
        assert "char_confs" not in entry
        assert all(round(v, 1) == v for point in entry["quad"] for v in point)

    def test_sort_is_right_to_left_then_top_down(self) -> None:
        lines = [
            ppocr.Line(ppocr.order_quad(rect(x, y, 40, 100, 0.0)), 0.9)
            for x, y in ((100, 50), (300, 400), (300, 100))
        ]
        ordered = ppocr.sort_lines(lines)
        assert [tuple(line.quad.mean(axis=0)) for line in ordered] == [
            (300, 100),
            (300, 400),
            (100, 50),
        ]

    @pytest.mark.parametrize("name", FIXTURE_NAMES)
    def test_fixture_is_well_formed(self, name: str) -> None:
        page = json.loads((FIXTURES / f"{name}.json").read_text(encoding="utf-8"))
        assert page["format"] == ppocr.FORMAT_ID
        lines = ppocr.lines_from_json(page)
        assert lines and len(lines) == len(page["lines"])
        for line, entry in zip(lines, page["lines"], strict=True):
            assert "char_confs" not in entry
            # Stored quads are already in reading-frame order, inside the page.
            assert np.allclose(ppocr.order_quad(line.quad), line.quad, atol=0.11)
            assert -45.0 < entry["angle"] <= 45.0
            assert entry["vertical"] == line.vertical
            assert line.quad[:, 0].min() > -40 and line.quad[:, 0].max() < page["width"] + 40
            assert line.quad[:, 1].min() > -40 and line.quad[:, 1].max() < page["height"] + 40

    def test_novel_fixture_has_whole_columns_and_ruby(self) -> None:
        page = json.loads((FIXTURES / "novel-text-097.json").read_text(encoding="utf-8"))
        lines = ppocr.lines_from_json(page)
        body = [
            ln
            for ln in lines
            if max(ppocr.quad_size(ln.quad)) >= 10 * min(ppocr.quad_size(ln.quad))
        ]
        ruby = [ln for ln in lines if len(ln.text) <= 6 and ppocr.quad_thickness(ln.quad) < 50]
        assert len(body) >= 14 and len(ruby) >= 12
        assert all(ln.vertical for ln in body)
        # Columns are whole: no fragment boxes, ~40 glyphs read as ONE line,
        # and the final "。" of a full column survives the windowed decoding.
        assert max(len(ln.text) for ln in body) >= 38
        assert sum(1 for ln in body if ln.text.endswith("。") and len(ln.text) > 30) >= 3
        # Ruby comes back as its own thin line beside the column it glosses.
        assert {"ぷろしよぼ", "がろなぼ", "かかみ"} <= {ln.text for ln in ruby}
        # The scan is skewed by a third of a degree, and the quads say so.
        angles = [ppocr.quad_angle(ln.quad) for ln in body if len(ln.text) > 30]
        assert all(-0.6 < a < -0.05 for a in angles)

    def test_manga_fixture_keeps_tilted_lines_tilted(self) -> None:
        page = json.loads((FIXTURES / "manga-page-069.json").read_text(encoding="utf-8"))
        tilted = [e for e in page["lines"] if abs(e["angle"]) > 5 and e["conf"] > 0.9]
        assert tilted
        for entry in tilted:
            xs = {round(x) for x, _ in entry["quad"]}
            assert len(xs) == 4  # a rotated rectangle, not its bounding box


# ---------------------------------------------------------------------------
# detector adapter
# ---------------------------------------------------------------------------


class TestAdapter:
    def test_line_block_follows_the_contract(self) -> None:
        quad = ppocr.order_quad(rect(1690, 300, 44.4, 420, 12.0))
        block = ppocr_manga.line_block(
            quad.tolist(),
            vertical=True,
            thickness=44.4,
            angle=12.0,
            score=0.87654,
            width=1703,
            height=2800,
        )
        assert set(block) >= {"box", "vertical", "font_size", "lines"}
        assert block["vertical"] is True and block["font_size"] == 44
        assert len(block["lines"]) == 1 and len(block["lines"][0]) == 4
        assert np.allclose(block["lines"][0], quad, atol=0.01)  # rotated quad, not the bbox
        x0, y0, x1, y1 = block["box"]
        assert all(isinstance(v, int) for v in block["box"])
        assert x0 <= quad[:, 0].min() and y0 <= quad[:, 1].min()
        assert x1 == 1703  # clipped to the page
        assert y1 >= math.floor(quad[:, 1].max())
        assert (block["angle"], block["score"]) == (12.0, 0.8765)

    def test_runner_loader_accepts_the_block(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr import engine_runner

        quad = ppocr.order_quad(rect(200, 300, 40, 400, -8.0))
        block = ppocr_manga.line_block(
            quad.tolist(),
            vertical=True,
            thickness=40,
            angle=-8.0,
            score=0.9,
            width=800,
            height=1200,
        )
        path = tmp_path / "page.json"
        path.write_text(
            json.dumps({"img_width": 800, "img_height": 1200, "blocks": [block]}), encoding="utf-8"
        )
        loaded = engine_runner.load_detection(path)
        assert loaded["blocks"][0]["font_size"] == 40
        assert np.allclose(loaded["blocks"][0]["lines"][0], quad, atol=0.01)

    def test_adapter_loads_the_package_module(self) -> None:
        assert ppocr_manga.load_ppocr() is ppocr


# ---------------------------------------------------------------------------
# clipped line ends, joined columns (evaluation fixes, 2026-09-19)
# ---------------------------------------------------------------------------


def fake_engine(reads: list[tuple[str, float]]) -> tuple[ppocr.PPOcr, list[np.ndarray]]:
    """A ``PPOcr`` without models: ``recognize_crops`` replays ``reads`` in order."""
    engine = object.__new__(ppocr.PPOcr)
    seen: list[np.ndarray] = []
    pending = list(reads)

    def recognize_crops(crops: list[np.ndarray]) -> list[tuple[str, float, list[float]]]:
        seen.extend(crops)
        return [(*pending.pop(0), []) for _ in crops]

    engine.recognize_crops = recognize_crops  # type: ignore[method-assign]
    return engine, seen


PAGE = np.full((2800, 1925, 3), 255, np.uint8)


class TestProbeGeometry:
    def test_slice_of_a_column_and_of_a_row(self) -> None:
        column = ppocr.order_quad(rect(500, 1000, 60, 1200, 0.0))
        piece = ppocr.slice_quad(column, -60, 150)
        assert np.allclose(piece, [[470, 340], [530, 340], [530, 550], [470, 550]], atol=0.01)
        row = ppocr.order_quad(rect(1000, 500, 1200, 60, 0.0))
        piece = ppocr.slice_quad(row, 1050, 1260)
        assert np.allclose(piece, [[1450, 470], [1660, 470], [1660, 530], [1450, 530]], atol=0.01)

    def test_slice_follows_a_tilted_line(self) -> None:
        tilted = ppocr.order_quad(rect(500, 1000, 60, 1200, 20.0))
        grown = ppocr.order_quad(ppocr.slice_quad(tilted, -30, 1230))
        assert ppocr.quad_angle(grown) == pytest.approx(20.0, abs=0.01)
        assert ppocr.quad_size(grown) == pytest.approx((60, 1260), abs=0.01)
        assert np.allclose(grown.mean(axis=0), tilted.mean(axis=0), atol=0.01)

    def test_long_line_gets_two_equal_probes_short_line_one(self) -> None:
        column = ppocr.order_quad(rect(500, 1000, 60, 1200, 0.0))
        head, tail = ppocr.probe_quads(column)
        assert ppocr.quad_size(head) == pytest.approx((60, 210), abs=0.01)
        assert ppocr.quad_size(tail) == pytest.approx((60, 210), abs=0.01)
        assert head[:, 1].min() == pytest.approx(400 - 60) and tail[:, 1].max() == pytest.approx(
            1600 + 60
        )
        (whole,) = ppocr.probe_quads(ppocr.order_quad(rect(500, 1000, 60, 240, 0.0)))
        assert ppocr.quad_size(whole) == pytest.approx((60, 360), abs=0.01)


class TestClippedMarks:
    def test_opener_needs_the_probe_to_repeat_the_line(self) -> None:
        assert ppocr.clipped_opener("搧ぶめえ鳲呒じゃぱをめつおレェ」", "「搧ぶめ") == "「"
        # the probe disagrees about the glyphs it shares: not trusted
        assert ppocr.clipped_opener("蘓ぞ！」", "「騵ぞ") == ""
        # a bubble outline read as a dash is not a bracket
        assert ppocr.clipped_opener("炕杌ら蜤ゑね熓觷で", "一炕杌") == ""
        # the line already opens with one
        assert ppocr.clipped_opener("「れっかゑ、窅え鳲呒？」", "「れっ") == ""
        assert ppocr.clipped_opener("矉孱", "「") == ""

    def test_probe_end_cut_through_a_glyph_may_disagree(self) -> None:
        # bench temp00070: the probe's inner end sliced "ぞ" and read it as "】"
        assert ppocr.clipped_opener("ん騵ぞ、やのて!!」", "「ん騵】") == "「"
        assert ppocr.clipped_closer("豩蘒矉孱でぶ", "』孱でぶ」") == "」"
        # one whole glyph must still agree
        assert ppocr.clipped_opener("ん騵ぞ、やのて!!」", "「あ騵") == ""
        assert ppocr.clipped_opener("ん騵ぞ", "「ん") == "「"
        assert ppocr.clipped_opener("ん騵ぞ", "「あ") == ""

    def test_thin_strokes_only_where_the_caller_allows(self) -> None:
        assert ppocr.clipped_opener("靮で腥曜えどぞてしうろ。", "一靮でよ") == ""
        assert ppocr.clipped_opener("靮で腥曜えどぞてしうろ。", "一靮でよ", thin=True) == "一"
        assert ppocr.clipped_opener("―あむえ褗かゑ、", "――あむ", thin=True) == "―"
        assert ppocr.clipped_opener("――あむえ褗かゑ、", "―――あ", thin=True) == ""

    def test_closer(self) -> None:
        assert ppocr.clipped_closer("うれ一萫え錼ってぞか", "ぞか。") == "。"
        assert ppocr.clipped_closer("跩やつも禡め炕癲", "炕癲、") == "、"
        assert ppocr.clipped_closer("ゾグブスめ劯", "め劯ぞ") == ""  # a glyph, not a mark
        assert ppocr.clipped_closer("侸彀肘毁め韻騢ら", "騢ら―") == ""  # outline read as a dash
        assert ppocr.clipped_closer("きか。", "きか。") == ""

    def test_single_probe_over_a_short_line(self) -> None:
        assert ppocr.clipped_marks("翆れ", "「翆れ」") == ("「", "」")
        assert ppocr.clipped_marks("翆れ」", "「翆れ」") == ("「", "")
        assert ppocr.clipped_marks("「翆れ", "「翆れ」") == ("", "」")
        assert ppocr.clipped_marks("「翆れ」", "「翆れ」") == ("", "")
        assert ppocr.clipped_marks("蘓ぞ！", "「騵ぞ！」") == ("", "")


class TestRecoverClippedEnds:
    def test_bracket_and_stop_grow_the_quad_by_half_a_pitch(self) -> None:
        quad = ppocr.order_quad(rect(500, 1000, 60, 1200, 0.0))
        line = ppocr.Line(quad, 0.9, "搧ぶめえ鳲呒じゃぱをめつおレェ", 0.99, [0.99] * 15)
        engine, seen = fake_engine([("「搧ぶめ", 0.97), ("レェ」", 0.96)])
        assert engine.recover_clipped_ends(PAGE, [line]) == 2
        assert line.text == "「搧ぶめえ鳲呒じゃぱをめつおレェ」"
        assert line.char_confs == [0.97, *[0.99] * 15, 0.96]
        pitch = 1200 / 15
        assert line.quad[:, 1].min() == pytest.approx(400 - pitch / 2, abs=0.01)
        assert line.quad[:, 1].max() == pytest.approx(1600 + pitch / 2, abs=0.01)
        assert line.vertical and len(seen) == 2

    def test_thin_first_glyph_of_a_body_column_takes_a_whole_cell(self) -> None:
        quad = ppocr.order_quad(rect(500, 1000, 60, 1200, 0.0))
        line = ppocr.Line(quad.copy(), 0.9, "靮で腥曜えどぞてしうろ。", 0.99, [0.99] * 12)
        engine, _ = fake_engine([("一靮でよ", 0.8), ("うろ。", 0.9)])
        assert engine.recover_clipped_ends(PAGE, [line], thin=[True]) == 1
        assert line.text == "一靮で腥曜えどぞてしうろ。"
        assert line.quad[:, 1].min() == pytest.approx(400 - 1200 / 12, abs=0.01)
        assert line.quad[:, 1].max() == pytest.approx(1600, abs=0.01)
        # without the caller's word (manga) the same probe changes nothing
        line = ppocr.Line(quad.copy(), 0.9, "靮で腥曜えどぞてしうろ。", 0.99, [0.99] * 12)
        engine, _ = fake_engine([("一靮でよ", 0.8), ("うろ。", 0.9)])
        assert engine.recover_clipped_ends(PAGE, [line]) == 0

    def test_nothing_changes_without_an_accepted_mark(self) -> None:
        quad = ppocr.order_quad(rect(500, 1000, 60, 1200, 0.0))
        line = ppocr.Line(quad.copy(), 0.9, "炕杌ら蜤ゑね熓觷で", 0.99, [0.99] * 9)
        engine, _ = fake_engine([("一炕杌", 0.9), ("觷で", 0.9)])
        assert engine.recover_clipped_ends(PAGE, [line]) == 0
        assert line.text == "炕杌ら蜤ゑね熓觷で" and np.array_equal(line.quad, quad)

    def test_one_glyph_lines_are_not_probed(self) -> None:
        line = ppocr.Line(ppocr.order_quad(rect(500, 500, 60, 66, 0.0)), 0.9, "蒩", 0.99, [0.99])
        engine, seen = fake_engine([])
        assert engine.recover_clipped_ends(PAGE, [line]) == 0 and seen == []

    def test_recorded_probe_reads_of_a_dialogue_page(self) -> None:
        """novel-dialogue-200 with the probe reads the real recognizer gave.

        Three columns of dialogue had lost their 「 to the detector's box; the
        probes that misread (「騵ぞ！」 for 「蘓ぞ！」, よ☆」) change nothing.
        """
        page = json.loads((FIXTURES / "novel-dialogue-200.json").read_text(encoding="utf-8"))
        probes = json.loads(
            (FIXTURES / "probes-novel-dialogue-200.json").read_text(encoding="utf-8")
        )
        lines = ppocr.lines_from_json(page)
        before = [ln.text for ln in lines]
        owners = [lines[entry["line"]] for entry in probes["reads"]]
        reads = [(text, conf) for entry in probes["reads"] for text, conf in entry["probes"]]
        engine, _ = fake_engine(reads)
        assert engine.recover_clipped_ends(PAGE, owners) == 3
        changed = {b: ln.text for b, ln in zip(before, lines, strict=True) if b != ln.text}
        assert changed == {
            "搧ぶめえ鳲呒じゃぱをめつおレェ」": "「搧ぶめえ鳲呒じゃぱをめつおレェ」",
            "宵むぱをよ」": "「宵むぱをよ」",
            "翆えゐぜづぽぱをれがろ？": "「翆えゐぜづぽぱをれがろ？",
        }


class TestJoinLines:
    def lines(self) -> list[ppocr.Line]:
        def column(y0: float, y1: float, text: str, conf: float = 0.99) -> ppocr.Line:
            quad = ppocr.order_quad(rect(500, (y0 + y1) / 2, 60, y1 - y0, 0.0))
            return ppocr.Line(quad, 0.9, text, conf)

        return [
            column(100, 760, "翆えゐぜづぽぱをれがろ？"),
            column(800, 920, "―", 0.4),
            column(950, 1700, "熙みもゑ一靮で魊ぬまれさ」"),
            ppocr.Line(ppocr.order_quad(rect(300, 900, 60, 1600, 0.0)), 0.9, "耵め輢", 0.99),
        ]

    def test_joined_read_replaces_its_pieces(self) -> None:
        engine, seen = fake_engine([("翆えゐぜづぽぱをれがろ？――熙みもゑ一靮で魊ぬまれさ」", 0.98)])
        out = engine.join_lines(PAGE, self.lines(), [[0, 1, 2]])
        assert [ln.text for ln in out] == [
            "翆えゐぜづぽぱをれがろ？――熙みもゑ一靮で魊ぬまれさ」",
            "耵め輢",
        ]
        assert (out[0].quad[:, 1].min(), out[0].quad[:, 1].max()) == pytest.approx((100, 1700))
        assert ppocr.quad_size(out[0].quad)[0] == pytest.approx(60, abs=0.01)
        assert seen[0].shape[:2] == (60, 1600)  # read as ONE crop, turned to read left to right

    def test_shorter_or_doubtful_read_keeps_the_pieces(self) -> None:
        for read in (("翆えゐぜづぽぱをれがろ？熙みもゑ", 0.99), ("翆" * 40, 0.41)):
            engine, _ = fake_engine([read])
            out = engine.join_lines(PAGE, self.lines(), [[0, 1, 2]])
            assert [ln.text for ln in out] == [ln.text for ln in self.lines()]

    def test_unread_piece_must_add_a_glyph(self) -> None:
        head = ppocr.Line(ppocr.order_quad(rect(500, 120, 70, 66, 0.0)), 0.6, "", 0.0)
        rest = ppocr.Line(ppocr.order_quad(rect(500, 400, 60, 500, 0.0)), 0.9, "ワーら旬糳で", 0.99)
        engine, _ = fake_engine([("マワーら旬糳で", 0.97)])
        assert [ln.text for ln in engine.join_lines(PAGE, [head, rest], [[0, 1]])] == [
            "マワーら旬糳で"
        ]
        engine, _ = fake_engine([("ワーら旬糳で", 0.99)])  # the blank box held nothing
        assert [ln.text for ln in engine.join_lines(PAGE, [head, rest], [[0, 1]])] == [
            "",
            "ワーら旬糳で",
        ]

    def test_a_mark_both_overlapping_pieces_read_is_counted_once(self) -> None:
        """The detector split a column at a stop and unclip made the boxes overlap.

        Both pieces then read the shared mark (bench temp00077: "...辂貁み。" over
        y 129-1001 and "。―うゐで..." over y 981-2490), so the correct joined
        read is one glyph SHORTER than the pieces together. It used to be
        rejected, leaving "。。" in the block and two overlapping touch zones.
        """

        def column(y0: float, y1: float, text: str) -> ppocr.Line:
            quad = ppocr.order_quad(rect(500, (y0 + y1) / 2, 60, y1 - y0, 0.0))
            return ppocr.Line(quad, 0.9, text, 0.99)

        pieces = [column(981, 2490, "。―うゐで粝に颍鑍して"), column(129, 1001, "誎ぞ辂貁み。")]
        engine, _ = fake_engine([("誎ぞ辂貁み。―うゐで粝に颍鑍して", 0.98)])
        out = engine.join_lines(PAGE, pieces, [[0, 1]])
        assert [ln.text for ln in out] == ["誎ぞ辂貁み。―うゐで粝に颍鑍して"]
        # ...but a glyph lost beyond the shared one still keeps the pieces
        engine, _ = fake_engine([("誎ぞ辂貁み。―うゐで粝に颍鑍し", 0.98)])
        assert len(engine.join_lines(PAGE, pieces, [[0, 1]])) == 2
        # ...and pieces that do not overlap share nothing, whatever they read
        apart = [column(129, 960, "誎ぞ辂貁み。"), column(981, 2490, "。―うゐで粝に颍鑍して")]
        engine, _ = fake_engine([("誎ぞ辂貁み。―うゐで粝に颍鑍して", 0.98)])
        assert len(engine.join_lines(PAGE, apart, [[0, 1]])) == 2


# ---------------------------------------------------------------------------
# cells the decoder skipped (repair round, 2026-09-19)
# ---------------------------------------------------------------------------


def decoded(text: str, skip: dict[int, int] | None = None, pitch: int = 6) -> list[ppocr.CtcChar]:
    """``text`` as CTC peaks one ``pitch`` apart; ``skip[i]`` extra cells before char ``i``."""
    chars, t = [], 3
    for i, ch in enumerate(text):
        t += pitch * (skip or {}).get(i, 0)
        chars.append(ppocr.CtcChar(ch, 0.99, t))
        t += pitch
    return chars


def paper(chars: list[ppocr.CtcChar], ink: dict[int, float] | None = None) -> np.ndarray:
    """A white recognizer tensor; ``ink[t] = share`` darkens that share of timestep ``t``'s rows."""
    width = (chars[-1].t + 4) * ppocr.REC_STRIDE
    tensor = np.ones((3, ppocr.REC_HEIGHT, width), np.float32)
    for t, share in (ink or {}).items():
        rows = int(round(share * ppocr.REC_HEIGHT))
        tensor[:, :rows, t * ppocr.REC_STRIDE : (t + 1) * ppocr.REC_STRIDE] = -1.0
    return tensor


class TestFillGaps:
    def test_blank_cell_is_a_full_width_space(self) -> None:
        chars = decoded("摮むず？ぷろしかゑ", skip={4: 1})
        out = ppocr.fill_gaps(chars, paper(chars))
        assert "".join(c.char for c in out) == "摮むず？　ぷろしかゑ"
        assert out[4].conf == 1.0 and chars[3].t < out[4].t < chars[4].t

    def test_inked_cell_is_a_missing_glyph(self) -> None:
        chars = decoded("暐ぞ膴ぽ聹にして、", skip={6: 1})
        hole = dict.fromkeys(range(chars[5].t + 3, chars[6].t - 2), 0.4)
        out = ppocr.fill_gaps(chars, paper(chars, hole))
        assert "".join(c.char for c in out) == "暐ぞ膴ぽ聹に〓して、"
        assert out[6].conf == 0.0

    def test_thin_ink_beside_a_dash_is_the_rest_of_the_dash(self) -> None:
        chars = decoded("ぞか。―あむえ褗かゑ", skip={4: 1})
        hole = dict.fromkeys(range(chars[3].t + 3, chars[4].t - 2), 0.04)
        out = ppocr.fill_gaps(chars, paper(chars, hole))
        assert "".join(c.char for c in out) == "ぞか。――あむえ褗かゑ"
        # the same thin ink with no dash beside it says nothing
        chars = decoded("ぞか。「あむえ褗かゑ", skip={4: 1})
        assert ppocr.fill_gaps(chars, paper(chars, hole)) == chars

    def test_short_line_gets_its_dash_back_but_nothing_else(self) -> None:
        chars = decoded("―翆れ。", skip={1: 1})
        hole = dict.fromkeys(range(chars[0].t + 3, chars[1].t - 2), 0.04)
        out = ppocr.fill_gaps(chars, paper(chars, hole))
        assert "".join(c.char for c in out) == "――翆れ。"
        chars = decoded("を？ぷろ", skip={2: 1})
        assert ppocr.fill_gaps(chars, paper(chars)) == chars

    def test_short_or_irregular_lines_are_left_alone(self) -> None:
        ruby = decoded("もみれ", skip={2: 1})
        assert ppocr.fill_gaps(ruby, paper(ruby)) == ruby
        latin = decoded("http://www.example", skip={9: 1}, pitch=3)
        assert ppocr.fill_gaps(latin, paper(latin)) == latin

    def test_wide_hole_fills_every_cell_up_to_the_cap(self) -> None:
        chars = decoded("轛搂毁嬁炇堖疫菮騢", skip={4: 2})
        out = ppocr.fill_gaps(chars, paper(chars))
        assert "".join(c.char for c in out) == "轛搂毁嬁　　炇堖疫菮騢"


class TestDoubtForeignGlyphs:
    def test_doubted_letter_between_japanese_glyphs(self) -> None:
        chars = decoded("鮾らｗぬ。")
        chars[2] = ppocr.CtcChar("ｗ", 0.14, chars[2].t)
        assert "".join(c.char for c in ppocr.doubt_foreign_glyphs(chars)) == "鮾ら〓ぬ。"

    def test_confident_or_grouped_letters_stay(self) -> None:
        chars = decoded("囵ぽＡ級れ")
        assert ppocr.doubt_foreign_glyphs(chars) == chars
        chars = [ppocr.CtcChar(c.char, 0.3, c.t) for c in decoded("蕟ｗｗｗ")]
        assert ppocr.doubt_foreign_glyphs(chars) == chars


class TestVoteCharacters:
    def test_both_wider_reads_must_name_the_same_character(self) -> None:
        text, confs = "てぞゐ鐍缣ぽもぞ", [0.99, 0.99, 0.99, 0.53, 0.99, 0.99, 0.99, 0.99]
        agree = [("てぞゐ罋缣ぽもぞ", [0.9] * 8), ("てぞゐ罋缣ぽもぞ", [0.94] * 8)]
        assert ppocr.vote_characters(text, confs, agree) == (
            "てぞゐ罋缣ぽもぞ",
            [0.99, 0.99, 0.99, pytest.approx(0.92), 0.99, 0.99, 0.99, 0.99],
            1,
        )
        split = [("てぞゐ罋缣ぽもぞ", [0.9] * 8), ("てぞゐ鐍缣ぽもぞ", [0.9] * 8)]
        assert ppocr.vote_characters(text, confs, split)[2] == 0

    def test_wider_reads_must_be_as_sure_as_the_first_read(self) -> None:
        # bench: 扩, read right at 0.79, against 梹 at 0.58 and 0.51 -- five times in one book
        text, confs = "矉孱め疗扩え", [0.99, 0.99, 0.99, 0.99, 0.79, 0.99]
        others = [("矉孱め疗梹え", [0.58] * 6), ("矉孱め疗梹え", [0.51] * 6)]
        assert ppocr.vote_characters(text, confs, others) == (text, confs, 0)

    def test_only_kanji_are_voted_on(self) -> None:
        # a wider crop likes to swap widths and kana sizes: "？" -> "?", キ -> き
        text, confs = "虑朱ぽ？ゲンキ", [0.99, 0.99, 0.99, 0.63, 0.99, 0.99, 0.57]
        others = [("虑朱ぽ?ゲンき", [0.95] * 7)] * 2
        assert ppocr.vote_characters(text, confs, others) == (text, confs, 0)

    def test_confident_characters_are_never_touched(self) -> None:
        # bench temp00045: a wider crop turned 抜 (0.78.. here 0.9) into the old form 嫊
        text, confs = "辴ら抜ぞて", [0.99, 0.99, 0.9, 0.99, 0.99]
        others = [("辴ら嫊ぞて", [0.99] * 5)] * 2
        assert ppocr.vote_characters(text, confs, others) == (text, confs, 0)

    def test_reads_of_another_length_vote_where_they_line_up(self) -> None:
        text, confs = "鑍錨に陣み注ぞ、", [0.99, 0.99, 0.99, 0.56, 0.99, 0.99, 0.99, 0.99]
        others = [("鑍錨に莐み注ぞ", [0.9] * 7), ("「鑍錨に莐み注ぞ、", [0.9] * 9)]
        assert ppocr.vote_characters(text, confs, others)[0] == "鑍錨に莐み注ぞ、"

    def test_a_vote_never_adds_a_placeholder(self) -> None:
        text, confs = "鮾ら痱ぬ。矉孱", [0.99, 0.99, 0.4, 0.99, 0.99, 0.99, 0.99]
        others = [("鮾ら〓ぬ。矉孱", [0.5] * 7)] * 2
        assert ppocr.vote_characters(text, confs, others)[2] == 0

    def test_widen_quad_grows_across_the_line_only(self) -> None:
        column = ppocr.order_quad(rect(500, 1000, 60, 1200, 0.0))
        assert ppocr.quad_size(ppocr.widen_quad(column, 0.1)) == pytest.approx((72, 1200), abs=0.01)
        row = ppocr.order_quad(rect(1000, 500, 1200, 60, 10.0))
        wide = ppocr.order_quad(ppocr.widen_quad(row, 0.1))
        assert ppocr.quad_size(wide) == pytest.approx((1200, 72), abs=0.01)
        assert ppocr.quad_angle(wide) == pytest.approx(10.0, abs=0.01)

    def test_engine_rereads_only_lines_with_a_doubted_character(self) -> None:
        quad = ppocr.order_quad(rect(500, 1000, 60, 1200, 0.0))
        sure = ppocr.Line(quad, 0.9, "かれ崅ぞて", 0.99, [0.99] * 5)
        weak = ppocr.Line(quad, 0.9, "かれ槕ぞて", 0.9, [0.99, 0.99, 0.44, 0.99, 0.99])
        engine, seen = fake_engine([("かれ崅ぞて", 0.9), ("かれ崅ぞて", 0.9)])
        engine.recognize_crops = lambda crops: [  # type: ignore[method-assign]
            (seen.append(c) or "かれ崅ぞて", 0.9, [0.9] * 5) for c in crops
        ]
        assert engine.second_opinions(PAGE, [sure, weak]) == 1
        assert weak.text == "かれ崅ぞて" and sure.text == "かれ崅ぞて" and len(seen) == 2
