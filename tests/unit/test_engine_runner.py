"""Unit tests for the standalone engine runner's pure page/volume assembly.

These run in the dev venv without torch, cv2 or numpy: detector output is
plain JSON-shaped dicts, images are small shape-compatible stand-ins and the
recognizer is faked. The engine-backed loaders are covered by the real-run
evidence in the spec.
"""

from __future__ import annotations

import contextlib
import io
import json
import math
import sys
import threading
import time
import traceback
import types
from collections.abc import Sequence
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner


class Image:
    """HxWx3 image stand-in supporting numpy-style 2-D slicing."""

    def __init__(self, width: int, height: int, name: str = "") -> None:
        self.shape = (height, width, 3)
        # Which page this is, for fakes that replay a different one per image.
        self.name = name
        self.rows = [[(y, x) for x in range(width)] for y in range(height)]

    def __getitem__(self, key: Any) -> Any:
        ys, xs = key
        sub = [row[xs] for row in self.rows[ys]]
        img = Image.__new__(Image)
        img.rows = sub
        img.shape = (len(sub), len(sub[0]) if sub else 0, 3)
        return img


def _blocks() -> list[dict[str, Any]]:
    """Detector JSON blocks: one two-line vertical block, one horizontal."""
    return [
        {
            "box": [10, 10, 40, 90],
            "vertical": True,
            "font_size": 20,
            "lines": [
                [[30, 10], [40, 10], [40, 90], [30, 90]],
                [[10, 10], [20, 10], [20, 90], [10, 90]],
            ],
        },
        {
            "box": [50, 100, 90, 120],
            "vertical": False,
            "font_size": 16,
            "lines": [[[50, 100], [90, 100], [90, 120], [50, 120]]],
        },
    ]


def _one_crop_per_line(img: Any, blk: dict[str, Any], line_idx: int) -> list[Any]:
    return [f"crop:{blk['box'][0]}:{line_idx}"]


class TestOcrPage:
    def test_schema_and_types(self) -> None:
        page = runner.ocr_page(
            Image(100, 200),
            _blocks(),
            _one_crop_per_line,
            lambda crops: [f"t{i}" for i in range(len(crops))],
            version="0.2.5",
        )
        assert set(page) == {"version", "img_width", "img_height", "blocks"}
        assert page["version"] == "0.2.5"
        assert (page["img_width"], page["img_height"]) == (100, 200)
        assert len(page["blocks"]) == 2
        first = page["blocks"][0]
        assert set(first) == {"box", "vertical", "font_size", "lines_coords", "lines"}
        assert first["box"] == [10, 10, 40, 90]
        assert first["vertical"] is True
        assert isinstance(first["font_size"], int)
        assert first["lines_coords"] == [
            [[30.0, 10.0], [40.0, 10.0], [40.0, 90.0], [30.0, 90.0]],
            [[10.0, 10.0], [20.0, 10.0], [20.0, 90.0], [10.0, 90.0]],
        ]
        assert first["lines"] == ["t0", "t1"]
        assert page["blocks"][1]["lines"] == ["t2"]
        json.dumps(page)

    def test_batches_all_crops_once_and_concatenates_chunks(self) -> None:
        calls: list[list[Any]] = []

        def two_chunks_for_vertical(img: Any, blk: dict[str, Any], line_idx: int) -> list[Any]:
            return ["a", "b"] if blk["vertical"] else ["c"]

        def recognize(crops: list[Any]) -> list[str]:
            calls.append(list(crops))
            return [f"<{c}>" for c in crops]

        page = runner.ocr_page(
            Image(100, 200), _blocks(), two_chunks_for_vertical, recognize, version="x"
        )
        assert len(calls) == 1 and len(calls[0]) == 5
        assert page["blocks"][0]["lines"] == ["<a><b>", "<a><b>"]
        assert page["blocks"][1]["lines"] == ["<c>"]

    def test_empty_page_skips_recognizer(self) -> None:
        def recognize(crops: list[Any]) -> list[str]:
            raise AssertionError("must not be called")

        page = runner.ocr_page(Image(100, 200), [], _one_crop_per_line, recognize, version="x")
        assert page["blocks"] == []

    def test_recognizer_length_mismatch_is_an_error(self) -> None:
        with pytest.raises(RuntimeError, match="returned 1 strings for 3 crops"):
            runner.ocr_page(
                Image(100, 200),
                _blocks(),
                _one_crop_per_line,
                lambda crops: ["only one"],
                version="x",
            )


class TestLoadDetection:
    def test_reads_contract_and_drops_lineless_blocks(self, tmp_path: Path) -> None:
        payload = {
            "img_width": 100,
            "img_height": 200,
            "blocks": [
                {
                    "box": [1, 2, 30, 40],
                    "vertical": True,
                    "font_size": 12,
                    "lines": [[[1, 2], [30, 2], [30, 40], [1, 40]]],
                },
                {"box": [5, 5, 6, 6], "vertical": False, "font_size": 9, "lines": []},
            ],
        }
        path = tmp_path / "p.json"
        path.write_text(json.dumps(payload), encoding="utf-8")
        det = runner.load_detection(path)
        assert det["img_width"] == 100 and det["img_height"] == 200
        assert len(det["blocks"]) == 1
        assert det["blocks"][0]["lines"][0][2] == [30.0, 40.0]

    def test_missing_font_size_falls_back_to_line_width(self, tmp_path: Path) -> None:
        payload = {
            "blocks": [{"box": [0, 0, 50, 90], "lines": [[[10, 0], [42, 0], [42, 90], [10, 90]]]}]
        }
        path = tmp_path / "p.json"
        path.write_text(json.dumps(payload), encoding="utf-8")
        det = runner.load_detection(path)
        assert det["blocks"][0]["font_size"] == 32
        assert det["blocks"][0]["vertical"] is True

    def test_malformed_rejected(self, tmp_path: Path) -> None:
        path = tmp_path / "p.json"
        path.write_text("[]", encoding="utf-8")
        with pytest.raises(ValueError, match="malformed"):
            runner.load_detection(path)


class TestChunkCutPoints:
    def test_cuts_at_ink_minima_near_anchors(self) -> None:
        # 100 columns, ink everywhere except a gap at 45..48 and 70..72.
        density = [10.0] * 100
        for i in (45, 46, 47):
            density[i] = 0.0
        for i in (70, 71):
            density[i] = 0.0
        # 2 chunks -> one anchor at 50, window 20 -> picks 45..47.
        assert runner.chunk_cut_points(density, 100, 2, 20) in ([45], [46], [47])
        # 3 chunks -> anchors at 33 and 67 -> 33 has no gap (any column), 67 finds 70/71.
        cuts = runner.chunk_cut_points(density, 100, 3, 10)
        assert len(cuts) == 2 and cuts[1] in (70, 71)

    def test_single_chunk_has_no_cuts(self) -> None:
        assert runner.chunk_cut_points([1.0] * 10, 10, 1, 4) == []


class TestUprightCrop:
    def test_margin_and_clipping(self) -> None:
        img = Image(100, 200)
        crop = runner.upright_line_crop(img, [[10, 20], [20, 20], [20, 70], [10, 70]])
        assert crop.shape[1] == 22 - 8  # x 8..21 inclusive
        assert crop.shape[0] == 77 - 14  # y 14..76 inclusive
        assert crop.rows[0][0] == (14, 8)
        crop = runner.upright_line_crop(img, [[0, 0], [10, 0], [10, 10], [0, 10]])
        assert crop.rows[0][0] == (0, 0)

    def test_degenerate_line_still_yields_pixels(self) -> None:
        crop = runner.upright_line_crop(Image(100, 200), [[5, 5], [5, 5], [5, 5], [5, 5]])
        assert crop.shape[0] >= 1 and crop.shape[1] >= 1


class TestBuildVolume:
    def test_volume_schema(self) -> None:
        page = {"version": "0.2.5", "img_width": 1, "img_height": 1, "blocks": []}
        vol = runner.build_volume(
            [("sub\\p001.jpg", page), ("p002.png", page)],
            version="0.2.5",
            title="Series",
            volume="Vol 01",
            title_uuid="t-uuid",
            volume_uuid="v-uuid",
            engine_meta={"id": "hayai-nova", "recognizer": "r", "detector": "ctd", "generator": "g"},
        )
        assert list(vol) == [
            "version",
            "title",
            "title_uuid",
            "volume",
            "volume_uuid",
            "ocr_engine",
            "pages",
        ]
        assert vol["ocr_engine"]["detector"] == "ctd"
        assert [p["img_path"] for p in vol["pages"]] == ["sub/p001.jpg", "p002.png"]
        assert "img_path" not in page


class TestListPages:
    def test_natural_order_and_extensions(self, tmp_path: Path) -> None:
        for name in ("p10.jpg", "p2.JPG", "p1.webp", "notes.txt", "sub/p3.avif", "cover.gif"):
            p = tmp_path / name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(b"x")
        pages = runner.list_pages(tmp_path)
        assert [p.as_posix() for p in pages] == ["p1.webp", "p2.JPG", "p10.jpg", "sub/p3.avif"]


class TestCropSelection:
    def test_line_level_detectors_keep_engine_mode(self) -> None:
        assert runner.select_crop("hayai-nova", "ctd") == ("line", 0.0)
        assert runner.select_crop("paddle-manga", "ctd") == ("upright", runner.UPRIGHT_MARGIN)

    def test_block_level_detector_forces_upright_crops(self) -> None:
        # A whole bubble deskewed as one column is unreadable; both engines
        # read upright bubbles well (measured), hayai-nova tight, paddle with margin.
        assert runner.select_crop("hayai-nova", "animetext") == ("upright", 0.0)
        assert runner.select_crop("paddle-manga", "animetext") == ("upright", runner.UPRIGHT_MARGIN)

    def test_layout_detector_gives_paddle_the_lines_own_quads(self) -> None:
        # deskewed, with a margin in ems -- never 12% of a 40-glyph column
        assert runner.select_crop("paddle-manga", "ppocr-manga") == ("quad", 0.0)
        assert runner.select_crop("hayai-nova", "ppocr-manga") == ("line", 0.0)

    def test_every_detector_is_classified(self) -> None:
        for detector in runner.DETECTOR_SCRIPTS:
            mode, margin = runner.select_crop("hayai-nova", detector)
            assert mode in ("line", "upright") and 0.0 <= margin < 1.0


class TestLineCropGeometry:
    def test_margin_is_capped_in_ems_not_a_share_of_the_column(self) -> None:
        """The duplicated run of bench page 97: 12% of a 2500 px column is five glyphs."""
        column = [[100.0, 100.0], [180.0, 100.0], [180.0, 2600.0], [100.0, 2600.0]]
        assert runner.line_margin_px(column) == pytest.approx(runner.LINE_MARGIN_EM * 80)
        assert runner.line_margin_px(column) < 0.5 * 80  # under half a glyph
        assert runner.line_margin_px(column, 0.5) == pytest.approx(40.0)
        # a few glyphs of SFX keep the rule the LoRA was trained with
        sfx = [[0.0, 0.0], [90.0, 0.0], [90.0, 100.0], [0.0, 100.0]]
        assert runner.line_margin_px(sfx, 0.5) == pytest.approx(runner.UPRIGHT_MARGIN * 100)

    def test_padded_quad_grows_along_the_lines_own_axes(self) -> None:
        upright = [[10.0, 20.0], [50.0, 20.0], [50.0, 220.0], [10.0, 220.0]]
        assert runner.padded_quad(upright, 5.0) == [
            [5.0, 15.0], [55.0, 15.0], [55.0, 225.0], [5.0, 225.0],
        ]  # fmt: skip
        # a line tilted 90 degrees: "right" is down the page, "down" is to the left
        turned = [[100.0, 0.0], [100.0, 40.0], [0.0, 40.0], [0.0, 0.0]]
        grown = runner.padded_quad(turned, 10.0)
        assert [[round(x), round(y)] for x, y in grown] == [
            [110, -10], [110, 50], [-10, 50], [-10, -10],
        ]  # fmt: skip

    def test_generation_batches_group_like_with_like(self) -> None:
        caps = [68, 12, 68, 20, 12, 68, 68]
        areas = [9.0, 1.0, 8.0, 2.0, 1.5, 7.0, 9.5]
        batches = runner.plan_generation_batches(areas, caps, 3)
        assert batches == [[1, 4, 3], [5, 2, 0], [6]]
        assert sorted(i for b in batches for i in b) == list(range(7))
        assert runner.plan_generation_batches([], [], 4) == []


class TestMisc:
    def test_normalize_text(self) -> None:
        assert runner.normalize_text(" ｱｲｳ！？ ") == "アイウ!?"

    def test_engine_and_detector_choices(self) -> None:
        args = runner.parse_args(
            ["--engine", "hayai-nova", "--input", "i", "--output", "o", "--cache-dir", "c"]
        )
        assert args.engine == "hayai-nova" and args.detector == "ppocr-manga" and args.volume_uuid is None
        args = runner.parse_args(
            [
                "--engine",
                "paddle-manga",
                "--detector",
                "ctd",
                "--input",
                "i",
                "--output",
                "o",
                "--cache-dir",
                "c",
            ]
        )
        assert args.detector == "ctd"
        with pytest.raises(SystemExit):
            runner.parse_args(
                ["--engine", "mokuro", "--input", "i", "--output", "o", "--cache-dir", "c"]
            )
        with pytest.raises(SystemExit):
            runner.parse_args(
                [
                    "--engine",
                    "hayai-nova",
                    "--detector",
                    "yolo",
                    "--input",
                    "i",
                    "--output",
                    "o",
                    "--cache-dir",
                    "c",
                ]
            )

    def test_every_registry_detector_has_a_runner_script(self) -> None:
        from mokuro_bunko.ocr.engines import DETECTORS

        detectors_dir = Path(runner.__file__).parent / "detectors"
        for spec in DETECTORS.values():
            assert runner.DETECTOR_SCRIPTS[spec.id] == spec.script
            assert (detectors_dir / spec.script).is_file()


def _axis_quad(x0: float, y0: float, x1: float, y1: float) -> list[list[float]]:
    """An axis-aligned box as a line quad, clockwise from the top left."""
    return [[x0, y0], [x1, y0], [x1, y1], [x0, y1]]


class TestLineGeometryHelpers:
    def test_quad_extents_uses_midpoint_vectors(self) -> None:
        quad = [[10, 20], [40, 20], [40, 120], [10, 120]]
        assert runner.quad_extents(quad, vertical=True) == (100.0, 30.0)
        assert runner.quad_extents(quad, vertical=False) == (30.0, 100.0)

    def test_quad_extents_of_a_rotated_quad(self) -> None:
        # A 3-4-5 slanted column: main extent is the midpoint vector length.
        quad = [[0, 0], [10, 0], [16, 8], [6, 8]]
        main, cross = runner.quad_extents(quad, vertical=True)
        assert round(main, 3) == round((3**2 + 4**2) ** 0.5 * 2, 3)
        assert round(cross, 3) == 10.0

    def test_body_pitch_is_the_median_thickness_of_the_lines_read(self) -> None:
        class Line:
            def __init__(self, thickness: float, text: str, conf: float) -> None:
                self.quad = _axis_quad(0.0, 0.0, thickness, 400.0)
                self.vertical, self.text, self.conf = True, text, conf

        body = [Line(t, "あいうえお", 0.99) for t in (25.0, 26.0, 27.0, 28.0)]
        # display lettering is outvoted by the running text, however big
        assert runner.body_pitch(body) == 26.5
        assert runner.body_pitch([*body, Line(120.0, "ザッ", 0.9)]) == 27.0
        # a blob and a stroke are read as nothing, and a doubted read is no measure
        blank, doubted = Line(159.0, "", 0.0), Line(9.0, "ミ", 0.2)
        assert runner.body_pitch([*body, blank, doubted]) == 26.5
        # too few lines to measure: no pitch, and the rules that need one stay off
        assert runner.body_pitch([*body[:2], blank, doubted]) == 0.0
        assert runner.body_pitch([]) == 0.0

    def test_parallel_neighbours_are_the_read_lines_a_quad_runs_beside(self) -> None:
        """The company a quad keeps, which is what the ``body`` rule asks for.

        A body of printed columns 27 px to the cell; ruby beside one of them is
        in their company, an identical stroke out in the artwork is not.
        """

        class Line:
            def __init__(self, x: float, y: float, w: float, h: float, text: str) -> None:
                self.quad = _axis_quad(x, y, x + w, y + h)
                self.vertical, self.text = True, text
                self.angle = 0.0

        body = [Line(700.0 - 60 * k, 200.0, 27.0, 120.0, "本文です") for k in range(3)]
        ruby = Line(672.0, 210.0, 13.0, 40.0, "")
        far = Line(120.0, 900.0, 13.0, 40.0, "")
        near, alone = runner.parallel_neighbours([*body, ruby, far])[3:]
        assert (near, alone) == (2, 0)  # the third column is a column too far
        # ... a line the CTC recognizer read nothing in vouches for nobody
        blind = [Line(700.0 - 60 * k, 200.0, 27.0, 120.0, "") for k in range(3)]
        assert runner.parallel_neighbours([*blind, ruby, far])[3:] == [0, 0]
        # ... and neither does one reading across the page
        across = Line(660.0, 240.0, 120.0, 20.0, "よこがき")
        across.vertical = False
        assert runner.parallel_neighbours([across, ruby]) == [0, 0]

    def test_upright_crop_bounds_matches_the_crop(self) -> None:
        img = Image(100, 200)
        pts = [[10, 20], [20, 20], [20, 70], [10, 70]]
        left, top, right, bottom = runner.upright_crop_bounds(100, 200, pts)
        crop = runner.upright_line_crop(img, pts)
        assert (crop.shape[1], crop.shape[0]) == (right - left, bottom - top)
        assert crop.rows[0][0] == (top, left)


class TestPageJson:
    def test_the_page_json_is_exactly_the_mokuro_shape(self) -> None:
        """Every key a page carries, in order, and not one more."""
        page = runner.ocr_page(
            Image(100, 200),
            _blocks(),
            _one_crop_per_line,
            lambda crops: [f"t{i}" for i in range(len(crops))],
            version="0.2.5",
        )
        volume = runner.build_volume(
            [("p001.jpg", page)],
            version="0.2.5",
            title="S",
            volume="V",
            title_uuid="t",
            volume_uuid="v",
            engine_meta={"id": "hayai-nova", "recognizer": "r", "detector": "ctd", "generator": "g"},
        )
        assert json.dumps(volume, ensure_ascii=False) == (
            '{"version": "0.2.5", "title": "S", "title_uuid": "t", "volume": "V", '
            '"volume_uuid": "v", "ocr_engine": {"id": "hayai-nova", "recognizer": "r", '
            '"detector": "ctd", "generator": "g"}, "pages": [{"version": "0.2.5", '
            '"img_width": 100, "img_height": 200, "blocks": ['
            '{"box": [10, 10, 40, 90], "vertical": true, "font_size": 20, '
            '"lines_coords": [[[30.0, 10.0], [40.0, 10.0], [40.0, 90.0], [30.0, 90.0]], '
            '[[10.0, 10.0], [20.0, 10.0], [20.0, 90.0], [10.0, 90.0]]], '
            '"lines": ["t0", "t1"]}, '
            '{"box": [50, 100, 90, 120], "vertical": false, "font_size": 16, '
            '"lines_coords": [[[50.0, 100.0], [90.0, 100.0], [90.0, 120.0], [50.0, 120.0]]], '
            '"lines": ["t2"]}]'
            ', "img_path": "p001.jpg"}]}'
        )



class TestPatchBudgetCli:
    """``--patches``: the NaFlex resolution the hayai-nova recognizer reads at."""

    def test_default_and_choices(self) -> None:
        base = ["--engine", "hayai-nova", "--input", "i", "--output", "o", "--cache-dir", "c"]
        assert runner.parse_args(base).patches == 512
        assert runner.parse_args(base).patches == runner.DEFAULT_PATCH_BUDGET
        for budget in runner.PATCH_BUDGETS:
            assert runner.parse_args([*base, "--patches", str(budget)]).patches == budget
        for bad in ("128", "1024", "400"):
            with pytest.raises(SystemExit):
                runner.parse_args([*base, "--patches", bad])

    def test_runner_knows_which_engines_it_reaches(self) -> None:
        """Mirrors ``engines.EngineSpec.patch_budget``; the runner cannot
        import it, so the two lists are checked against each other here."""
        from mokuro_bunko.ocr.engines import ENGINE_IDS, uses_patch_budget

        assert runner.PATCH_BUDGET_ENGINES == frozenset(
            e for e in ENGINE_IDS if uses_patch_budget(e)
        )
        from mokuro_bunko.ocr.engines import DEFAULT_PATCH_BUDGET, PATCH_BUDGETS

        assert runner.PATCH_BUDGETS == PATCH_BUDGETS
        assert runner.DEFAULT_PATCH_BUDGET == DEFAULT_PATCH_BUDGET

    def test_nova_is_registered_as_a_line_crop_engine(self) -> None:
        assert runner.CROP_MODES["hayai-nova"] == "line"
        assert (
            runner.RECOGNIZER_REPOS["hayai-nova"]
            == "JustANormalTinkerer/hayai-ocr-v2.5-nova"
        )
        assert "hayai-nova" in runner.ENGINE_IDS


class TestPinnedRevisions:
    """Every repo the runner resolves is pinned to a commit.

    The point is not reproducibility: hayai-nova and PaddleOCR-VL are loaded
    with ``trust_remote_code=True``, so an unpinned repo means someone else's
    push changes the Python this process executes.
    """

    def test_every_repo_the_runner_names_is_pinned(self) -> None:
        from mokuro_bunko.ocr import ppocr

        runner_loaded = {
            *(repo for engine, repo in runner.RECOGNIZER_REPOS.items() if engine != "ppocr-manga"),
            runner.HAYAI_VISION_REPO,
            runner.PADDLE_BASE_REPO,
        }
        assert runner_loaded <= set(runner.REPO_REVISIONS)
        for sha in runner.REPO_REVISIONS.values():
            assert len(sha) == 40 and all(c in "0123456789abcdef" for c in sha)
        # ppocr-manga pins in its own module, which the runner reports from.
        assert len(ppocr.REPO_REVISION) == 40

    def test_an_unpinned_repo_is_a_hard_error_not_a_moving_branch(self) -> None:
        with pytest.raises(RuntimeError, match="no pinned revision"):
            runner.pinned("someone/brand-new-repo")

    def test_the_detectors_pin_in_their_own_process_and_report_back(
        self, tmp_path: Path
    ) -> None:
        """A detector runs as its own script, so its pin cannot live in
        ``REPO_REVISIONS``; it reports what it loaded through the adapter
        contract instead, and the runner merges that into the sidecar.
        """
        from mokuro_bunko.ocr.detectors import _common

        assert runner.DETECTOR_WEIGHTS_FILE == _common.WEIGHTS_FILE
        detect_dir = tmp_path / "_detect"
        detect_dir.mkdir()
        # Nothing reported yet: the sidecar must claim nothing, not guess.
        assert runner.load_detector_weights(detect_dir) == {}
        _common.write_weights_json(detect_dir, {"some/detector": "d" * 40})
        assert runner.load_detector_weights(detect_dir) == {"some/detector": "d" * 40}
        # A truncated or hand-edited file is "reported nothing", never a crash
        # halfway through a volume.
        (detect_dir / runner.DETECTOR_WEIGHTS_FILE).write_text("{oops", encoding="utf-8")
        assert runner.load_detector_weights(detect_dir) == {}
        (detect_dir / runner.DETECTOR_WEIGHTS_FILE).write_text("[]", encoding="utf-8")
        assert runner.load_detector_weights(detect_dir) == {}

    def test_nova_passes_the_pin_to_every_load(self, monkeypatch: pytest.MonkeyPatch) -> None:
        """The revision must reach transformers, not merely sit in a table."""
        calls: list[tuple[str, str, dict[str, Any]]] = []

        class _Loader:
            def __init__(self, label: str) -> None:
                self.label = label

            def from_pretrained(self, repo: str, **kwargs: Any) -> Any:
                calls.append((self.label, repo, kwargs))
                return _Model()

        class _Model:
            def to(self, device: str) -> _Model:
                return self

            def eval(self) -> _Model:
                return self

        fake_transformers = types.SimpleNamespace(
            AutoModel=_Loader("model"),
            AutoProcessor=_Loader("processor"),
            PreTrainedTokenizerFast=_Loader("tokenizer"),
        )
        monkeypatch.setitem(sys.modules, "transformers", fake_transformers)
        monkeypatch.setitem(sys.modules, "torch", types.SimpleNamespace())
        monkeypatch.setattr(runner, "_pick_device", lambda: "cpu")

        rec = runner.HayaiNovaRecognizer(patches=384)

        nova = runner.RECOGNIZER_REPOS["hayai-nova"]
        vision = runner.HAYAI_VISION_REPO
        assert {(label, repo, kwargs["revision"]) for label, repo, kwargs in calls} == {
            ("model", nova, runner.REPO_REVISIONS[nova]),
            ("tokenizer", nova, runner.REPO_REVISIONS[nova]),
            ("processor", vision, runner.REPO_REVISIONS[vision]),
        }
        # trust_remote_code is what makes the pin load-bearing; keep them together.
        model_call = next(c for c in calls if c[0] == "model")
        assert model_call[2]["trust_remote_code"] is True
        # And the recognizer reports what it loaded, for the sidecar.
        assert rec.repos == {
            nova: runner.REPO_REVISIONS[nova],
            vision: runner.REPO_REVISIONS[vision],
        }
        assert rec.patches == 384


# --------------------------------------------------------------------------
# ppocr-manga: the line engine (detector + recognizer + line_layout)
# --------------------------------------------------------------------------

from mokuro_bunko.ocr import line_layout  # noqa: E402

PPOCR_FIXTURES = Path(__file__).resolve().parents[1] / "fixtures" / "ppocr"


def _raw_page(name: str) -> dict[str, Any]:
    return json.loads((PPOCR_FIXTURES / f"{name}.json").read_text(encoding="utf-8"))


class FakeLine:
    def __init__(self, entry: dict[str, Any]) -> None:
        self.entry = dict(entry)
        self.text = entry["text"]

    @property
    def quad(self) -> list[list[float]]:
        return self.entry["quad"]

    @property
    def vertical(self) -> bool:
        return bool(self.entry.get("vertical", True))

    @property
    def conf(self) -> float:
        return float(self.entry["conf"])

    @conf.setter
    def conf(self, value: float) -> None:
        self.entry["conf"] = value

    @property
    def char_confs(self) -> list[float]:
        return list(self.entry.get("char_confs", []))

    @property
    def score(self) -> float:
        """What the DETECTOR made of the quad; it decides engine-only lines."""
        return float(self.entry.get("score", 0.0))

    @property
    def angle(self) -> float:
        """The quad's tilt, the way ``ppocr.Line`` computes it (without numpy)."""
        (x0, y0), (x1, y1), (x2, y2), (x3, y3) = (tuple(p) for p in self.quad[:4])
        return math.degrees(math.atan2((y1 - y0) + (y2 - y3), (x1 - x0) + (x2 - x3)))


class FakePPOcrEngine:
    """Stands in for ``ppocr.PPOcr``: replays one cached raw page.

    ``by_name`` instead replays a different cached page per image, which is
    what makes page ORDER visible when the detect stage is pooled.
    """

    def __init__(
        self,
        raw_page: dict[str, Any],
        *,
        joins: bool = True,
        by_name: dict[str, dict[str, Any]] | None = None,
    ) -> None:
        self.raw_page = raw_page
        self.by_name = by_name
        self.last_detect_info = {"side": 1280}
        self.models = type("M", (), {"precision": "fp32"})()
        self.threads = 4
        # What ``clone_engine`` copies onto a pooled session.
        self.side = 1280
        self.tile = "auto"
        self.calls: list[str] = []
        self.probed: list[str] = []
        # False: the joined read "did not hold up", the pieces stay apart
        self.joins = joins

    def read_page(self, img: Any) -> list[FakeLine]:
        self.calls.append("read_page")
        raw = self.by_name[img.name] if self.by_name is not None else self.raw_page
        self.current = [FakeLine(entry) for entry in raw["lines"]]
        return self.current

    def join_lines(self, img: Any, lines: list[FakeLine], groups: Any) -> list[FakeLine]:
        self.calls.append("join_lines")
        if not self.joins:
            return list(lines)
        gone = {i for group in groups for i in group[1:]}
        for group in groups:
            pieces = sorted((lines[i] for i in group), key=lambda ln: ln.entry["quad"][0][1])
            lines[group[0]].entry = {
                **pieces[0].entry,
                "quad": [*pieces[0].entry["quad"][:2], *pieces[-1].entry["quad"][2:]],
                "text": "".join(ln.text for ln in pieces),
                "conf": 0.99,
            }
            lines[group[0]].text = lines[group[0]].entry["text"]
        self.current = [ln for i, ln in enumerate(lines) if i not in gone]
        return self.current

    def recover_clipped_ends(self, img: Any, lines: list[FakeLine], thin: list[bool]) -> int:
        self.calls.append("recover_clipped_ends")
        self.probed = [ln.text for ln in lines]
        self.thin = [ln.text for ln, flag in zip(lines, thin, strict=True) if flag]
        return 0

    def second_opinions(self, img: Any, lines: list[FakeLine]) -> int:
        self.calls.append("second_opinions")
        self.voted = [ln.text for ln in lines]
        return 0


class FakePPOcrModule:
    def __init__(
        self,
        raw_page: dict[str, Any],
        *,
        joins: bool = True,
        by_name: dict[str, dict[str, Any]] | None = None,
    ) -> None:
        self.raw_page = raw_page
        self.joins = joins
        self.by_name = by_name
        self.engine = FakePPOcrEngine(raw_page, joins=joins, by_name=by_name)
        self.clones: list[FakePPOcrEngine] = []

    def PPOcr(self, models: Any = None, **kwargs: Any) -> FakePPOcrEngine:  # noqa: N802
        """The reader's own engine, or a fresh one for each pooled worker.

        The real class is built bare by the reader and with resolved models by
        ``clone_engine``, which is exactly how the pool is told apart here.
        """
        if models is None:
            return self.engine
        clone = FakePPOcrEngine(self.raw_page, joins=self.joins, by_name=self.by_name)
        self.clones.append(clone)
        return clone

    @staticmethod
    def page_to_json(
        lines: list[FakeLine], width: int, height: int, *, detector: dict[str, Any]
    ) -> dict[str, Any]:
        return {
            "format": "ppocr-lines/1",
            "width": width,
            "height": height,
            "detector": detector,
            "lines": [{**ln.entry, "text": ln.text} for ln in lines],
        }


class TestLineEngineRegistry:
    def test_runner_tables_match_the_server_registry(self) -> None:
        from mokuro_bunko.ocr import engines

        # EVERY engine reaches this runner now: the mokuro ones on the served
        # road, behind a process of their own.
        assert set(runner.ENGINE_IDS) == set(engines.ENGINE_IDS)
        # ... but only the ones this process loads name a recognizer here. A
        # served engine resolves its own weights in its own environment.
        in_process = {
            e for e in engines.ENGINE_IDS if not engines.get_engine(e).serve_module
        }
        assert set(runner.RECOGNIZER_REPOS) == in_process
        for engine_id, detector in runner.LINE_ENGINES.items():
            assert engines.get_engine(engine_id).detector == detector
            assert runner.RECOGNIZER_REPOS[engine_id] == engines.get_engine(engine_id).recognizer
        for engine_id, (module, args) in runner.SERVED_ENGINES.items():
            assert engines.get_engine(engine_id).serve_module == module
            # The serve process's precision is the row's mode (``--fp16``),
            # never an engine of its own.
            assert args == ()

    # Engine ids that once ran and no longer do. The registry stopped
    # keeping them: with a generation naming its own sidecar no engine owns
    # a file name any more, so a retired entry had nothing left to say and
    # an unknown engine is simply unknown. This list is the test's own
    # memory instead, and an engine removed from here on is added to it.
    RETIRED_ENGINE_IDS = ("hayai", "mokuro-fp16")

    def test_no_retired_engine_is_left_anywhere_in_the_runner(self) -> None:
        """A removed engine must leave no id behind that something can reach.

        The runner cannot import the registry (it runs outside the package),
        so its tables are checked from here.
        """
        from mokuro_bunko.ocr import engines

        for retired in self.RETIRED_ENGINE_IDS:
            # Gone from the registry too, so a config still naming it fails
            # to load rather than resolving to something that runs.
            with pytest.raises(ValueError, match="Unknown OCR engine"):
                engines.get_engine(retired)
            assert retired not in runner.ENGINE_IDS
            assert retired not in runner.CROP_MODES
            assert retired not in runner.RECOGNIZER_REPOS
            assert retired not in runner.LINE_ENGINES
            assert retired not in runner.PATCH_BUDGET_ENGINES
            with pytest.raises(ValueError, match="unsupported engine"):
                runner.load_recognizer(retired)
            with pytest.raises(SystemExit):
                runner.parse_args(
                    ["--engine", retired, "--input", "i", "--output", "o", "--cache-dir", "c"]
                )

    def test_cli_accepts_the_engine(self) -> None:
        base = ["--input", "i", "--output", "o", "--cache-dir", "c"]
        assert runner.parse_args(["--engine", "ppocr-manga", *base]).engine == "ppocr-manga"
        args = runner.parse_args(["--engine", "hayai-nova", "--detector", "ppocr-manga", *base])
        assert args.detector == "ppocr-manga"


class TestLayoutPageDict:
    def test_novel_page_becomes_mokuro_blocks(self) -> None:
        raw_page = _raw_page("novel-text-250")
        page, result = runner.layout_page_dict(raw_page, line_layout, "0.2.5")
        assert set(page) == {"version", "img_width", "img_height", "blocks"}
        assert (page["img_width"], page["img_height"]) == (1925, 2800)
        assert len(page["blocks"]) >= 5 and len(result.ruby) == 28
        for block in page["blocks"]:
            assert set(block) == {"box", "vertical", "font_size", "lines", "lines_coords"}
        json.dumps(page)

    def test_noise_is_left_out_of_the_page_but_stays_in_the_result(self) -> None:
        page, result = runner.layout_page_dict(_raw_page("novel-cover-001"), line_layout, "0.2.5")
        written = [line for block in page["blocks"] for line in block["lines"]]
        assert "簈蒕" not in written and "隻睅嬁" in written
        assert "noise" in result.kinds
        assert len(page["blocks"]) == sum(1 for kind in result.kinds if kind != "noise")


class TestPPOcrPageReader:
    def test_join_then_probe_everything_but_ruby_then_layout(self) -> None:
        raw_page = _raw_page("novel-dialogue-200")
        fake = FakePPOcrModule(raw_page)
        reader = runner.PPOcrPageReader(ppocr=fake, layout=line_layout)
        page = reader(Image(1925, 2800), "0.2.5")
        assert fake.engine.calls == [
            "read_page",
            "join_lines",
            "recover_clipped_ends",
            "second_opinions",
        ]
        # The three pieces of one column reach the page as ONE line.
        lines = [line for block in page["blocks"] for line in block["lines"]]
        assert "翆えゐぜづぽぱをれがろ？―熙みもゑ一靮で魊ぬまれさ」" in lines
        # Ruby is never probed; every body line is.
        assert "しゆろばき" not in fake.engine.probed and "ぞどしゆま" not in fake.engine.probed
        assert "矉孱め憕びぽ貨め豩に倿ぞ痢うむてぞっか。" in fake.engine.probed
        # Thin first glyphs are probed for on the body's columns (all of this
        # page's text), and ruby is kept out of the vote as well.
        assert "矉孱め憕びぽ貨め豩に倿ぞ痢うむてぞっか。" in fake.engine.thin
        assert set(fake.engine.thin) <= set(fake.engine.probed)
        assert fake.engine.voted == fake.engine.probed
        assert reader.ruby_count == 4
        assert reader.raw["detector"] == {
            "side": 1280,
            "joined": 2,
            "recovered_ends": 0,
            "second_opinions": 0,
        }
        # The removed readings stay in the raw dump, tied to their base glyphs.
        assert len(reader.raw["ruby"]) == 4
        run = reader.raw["ruby"][0]
        base = reader.raw["lines"][run["base"]]["text"]
        assert reader.raw["lines"][run["line"]]["text"] == run["text"]
        assert 0 <= run["chars"][0] < run["chars"][1] <= len(base)

    def test_manga_lines_are_never_probed_for_thin_glyphs(self) -> None:
        """No text body, no thin probes: there they read bubble outlines as "一"."""
        fake = FakePPOcrModule(_raw_page("manga-page-069"))
        runner.PPOcrPageReader(ppocr=fake, layout=line_layout)(Image(1300, 1900), "0.2.5")
        assert fake.engine.probed and fake.engine.thin == []


def _corner_crops(img: Any, blk: dict[str, Any], line_idx: int) -> list[Any]:
    """Stand-in crop: the quad's first corner, which names the line."""
    x, y = blk["lines"][line_idx][0]
    return [(round(x), round(y))]


class FakeEngineRecognizer:
    """Stands in for the VLM: answers a crop with the text filed under its corner.

    Lines it was not told about are read like the CTC read, minus any closing
    bracket (the engine's habit). Every call is recorded.
    """

    token_caps = True

    def __init__(self, fake: FakePPOcrModule, reads: dict[str, str] | None = None) -> None:
        self.engine = fake.engine
        self.reads = reads or {}
        self.calls: list[list[str]] = []
        self.caps: list[list[int]] = []

    def __call__(self, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
        # the lines as the fake engine last returned them (joined columns included)
        by_corner = {
            (round(ln.quad[0][0]), round(ln.quad[0][1])): ln.entry["text"]
            for ln in self.engine.current
        }
        ctc = [by_corner.get(crop, "") for crop in crops]
        self.calls.append(ctc)
        self.caps.append(list(max_tokens or []))
        return [self.reads.get(text, text.rstrip("」")) for text in ctc]


def _reconciled_reader(
    fake: FakePPOcrModule, recognizer: Any, **kwargs: Any
) -> runner.ReconciledPageReader:
    from mokuro_bunko.ocr import line_reconcile

    return runner.ReconciledPageReader(
        recognizer, _corner_crops, ppocr=fake, layout=line_layout, reconcile=line_reconcile,
        **kwargs,
    )  # fmt: skip


class TestReconciledPageReader:
    """Another engine on the ppocr-manga detector's lines (paddle-manga in production)."""

    def test_engine_reads_text_lines_once_and_the_reads_are_merged(self) -> None:
        raw_page = _raw_page("novel-dialogue-200")
        fake = FakePPOcrModule(raw_page)
        recognizer = FakeEngineRecognizer(fake)
        reader = _reconciled_reader(fake, recognizer)
        page = reader(Image(1925, 2800), "0.2.5")
        # The same road as the line engine, then the recognizer: one batch a page.
        assert fake.engine.calls == [
            "read_page", "join_lines", "recover_clipped_ends", "second_opinions",
        ]  # fmt: skip
        assert len(recognizer.calls) == 1
        read = recognizer.calls[0]
        # ruby is never handed to the engine, a joined column is handed over ONCE
        assert "しゆろばき" not in read and "ぞどしゆま" not in read
        assert len(read) == len(fake.engine.raw_page["lines"]) - 2 - 4  # 2 joined away, 4 ruby
        # every closing bracket the stand-in engine dropped is back
        lines = [line for block in page["blocks"] for line in block["lines"]]
        assert "「翆れ」" in lines and "「蘓ぞ！」" in lines
        assert not any(line.startswith("「") and not line.endswith("」") for line in lines)
        # token budgets follow the quads: a full column gets more than a short line
        caps = dict(zip(read, recognizer.caps[0], strict=True))
        assert caps["「翆れ」"] < caps["矉孱め憕びぽ貨め豩に倿ぞ痢うむてぞっか。"] <= 160

    def test_both_reads_and_their_agreement_stay_in_the_raw_dump(self) -> None:
        fake = FakePPOcrModule(_raw_page("novel-dialogue-200"))
        recognizer = FakeEngineRecognizer(fake, reads={"宵むぱをよ」": "鴩むぱをよ"})
        reader = _reconciled_reader(fake, recognizer)
        page = reader(Image(1925, 2800), "0.2.5")
        entry = next(ln for ln in reader.raw["lines"] if ln.get("ctc") == "宵むぱをよ」")
        assert (entry["vlm"], entry["merged"], entry["text"]) == (
            "鴩むぱをよ", "鴩むぱをよ」", "鴩むぱをよ」",
        )  # fmt: skip
        assert entry["agreement"] == 0.8333 and entry["notes"] == ["closer"]
        ruby = next(ln for ln in reader.raw["lines"] if ln["text"] == "しゆろばき")
        assert "vlm" not in ruby  # ruby keeps its CTC read and is not compared
        tally = reader.raw["reconcile"]
        assert tally["lines"] == tally["compared"] == len(recognizer.calls[0])
        assert tally["full_agreement"] < tally["lines"] and tally["from_ctc"] == 0
        # nothing of this reaches the mokuro page
        for block in page["blocks"]:
            assert set(block) == {"box", "vertical", "font_size", "lines", "lines_coords"}

    def test_runaway_and_empty_reads_fall_back_to_the_ctc_line(self) -> None:
        fake = FakePPOcrModule(_raw_page("novel-dialogue-200"))
        recognizer = FakeEngineRecognizer(
            fake, reads={"宵むぱをよ」": "宵むぱをよよよよよよよよよよよよよよよ", "「蘓ぞ！」": ""}
        )
        reader = _reconciled_reader(fake, recognizer)
        page = reader(Image(1925, 2800), "0.2.5")
        lines = [line for block in page["blocks"] for line in block["lines"]]
        assert "宵むぱをよ」" in lines and "「蘓ぞ！」" in lines
        notes = {ln["ctc"]: ln["notes"] for ln in reader.raw["lines"] if "ctc" in ln}
        assert "runaway" in notes["宵むぱをよ」"] and notes["「蘓ぞ！」"] == ["empty"]
        assert reader.raw["reconcile"]["from_ctc"] == 2

    def test_the_ctc_reads_confidence_per_glyph_reaches_the_merge(self) -> None:
        """A glyph the engine skipped comes back only on the CTC read's per-glyph say-so."""
        raw_page = _raw_page("novel-dialogue-200")
        skipped = "矉孱め憕びぽ貨め豩に倿ぞ痢うむてぞっか。"
        for entry in raw_page["lines"]:
            if entry["text"] == skipped:
                entry["char_confs"] = [0.99] * len(skipped)
        fake = FakePPOcrModule(raw_page)
        recognizer = FakeEngineRecognizer(fake, reads={skipped: skipped.replace("貨め豩に", "")})
        page = _reconciled_reader(fake, recognizer)(Image(1925, 2800), "0.2.5")
        assert skipped in [line for block in page["blocks"] for line in block["lines"]]

    def test_doubted_lines_get_a_second_read_from_a_wider_crop(self) -> None:
        fake = FakePPOcrModule(_raw_page("novel-dialogue-200"))
        doubted = "矉孱め憕びぽ貨め豩に倿ぞ痢うむてぞっか。"
        first = FakeEngineRecognizer(fake, reads={doubted: doubted.replace("倿", "粄")})
        second = FakeEngineRecognizer(fake)

        def wider(img: Any, blk: dict[str, Any], line_idx: int) -> list[Any]:
            return [("wide", *_corner_crops(img, blk, line_idx)[0])]

        def recognize(crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
            if crops and crops[0][0] == "wide":
                return second([crop[1:] for crop in crops], max_tokens)
            return first(crops, max_tokens)

        recognize.token_caps = True  # type: ignore[attr-defined]
        reader = _reconciled_reader(fake, recognize, second_crop_fn=wider)
        page = reader(Image(1925, 2800), "0.2.5")
        # only lines the two reads differ on are read again, in one more batch
        assert len(second.calls) == 1 and doubted in second.calls[0]
        assert len(second.calls[0]) < len(first.calls[0]) / 2
        # two reads out of three say 倿
        assert doubted in [line for block in page["blocks"] for line in block["lines"]]
        entry = next(ln for ln in reader.raw["lines"] if ln.get("ctc") == doubted)
        assert "vote" in entry["notes"] and entry["vlm_second"] == doubted

    def test_a_line_only_the_engine_read_needs_the_detector_behind_its_quad(self) -> None:
        from mokuro_bunko.ocr import line_reconcile

        def line(
            x: float, width: float, height: float, text: str, conf: float, score: float
        ) -> dict[str, Any]:
            quad = [[x, 300.0], [x + width, 300.0], [x + width, 300.0 + height], [x, 300.0 + height]]
            return {"quad": quad, "text": text, "score": score, "conf": conf, "vertical": True}

        raw_page = {
            "format": "ppocr-lines/1", "width": 1000, "height": 1400,
            "lines": [
                # hand-lettered SFX the CTC read as nothing, on a quad the detector is sure of
                line(100.0, 120.0, 260.0, "", 0.0, 0.9),
                # the same size of quad, read the same way twice, but faint: art
                line(700.0, 120.0, 260.0, "", 0.0, 0.55),
                line(400.0, 40.0, 400.0, "隆碸ぽぞぞ鎋璂でぶぱ", 0.99, 0.9),
            ],
        }  # fmt: skip
        by_corner = {(100, 300): "ラッ", (700, 300): "ふふ", (400, 300): "隆碸ぽぞぞ鎋璂でぶぱ"}
        reader = runner.ReconciledPageReader(
            lambda crops: [by_corner[crop] for crop in crops], _corner_crops,
            ppocr=FakePPOcrModule(raw_page), layout=line_layout, reconcile=line_reconcile,
            second_crop_fn=_corner_crops,
        )  # fmt: skip
        page = reader(Image(1000, 1400), "0.2.5")
        # both were read the same twice, and the size of the lettering is the
        # same; only the quad the detector backs is believed to be text
        assert sorted(block["lines"] for block in page["blocks"]) == [["ラッ"], ["隆碸ぽぞぞ鎋璂でぶぱ"]]
        backed, faint, _text = reader.raw["lines"]
        assert "backed" in backed["notes"] and backed["confirmed"] and backed["conf"] == 0.75
        assert faint["confirmed"] and faint["notes"][-2:] == ["unbacked", "dropped"]
        assert faint["conf"] == 0.0 and faint["text"] == faint["merged"] == ""

    def test_a_small_glyph_among_read_columns_is_kept_where_the_same_quad_alone_is_not(
        self,
    ) -> None:
        """The ``body`` rule, end to end: half a body cell thick, at 0.76, and
        the only difference between the two quads is the company they keep.

        Bench: printed ruby and printed single glyphs the CTC recognizer read
        nothing in ("くぞな", "よぜ", "を", "き") sat at 0.74-0.79, and the
        0.80 bar dropped every one of them.
        """
        from mokuro_bunko.ocr import line_reconcile

        def quad(x: float, y: float, w: float, h: float) -> list[list[float]]:
            return [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]

        body = ["隆碸ぽぞぞ鎋璂でぶぱ", "ぷろでぶぱづ峭をか", "漪ぽ嘺ゑもぞでしょろ"]
        raw_page = {
            "format": "ppocr-lines/1", "width": 833, "height": 1186,
            "lines": [
                # the page's running text: 27 px to a glyph cell, read by the CTC
                *(
                    {"quad": quad(700.0 - 60 * k, 200.0, 27.0, 100.0), "text": text,
                     "score": 0.9, "conf": 0.99, "vertical": True}
                    for k, text in enumerate(body)
                ),
                # a glyph half a cell thick at the foot of the first column,
                # which only the engine read
                {"quad": quad(700.0, 305.0, 13.0, 40.0), "text": "", "score": 0.76,
                 "conf": 0.0, "vertical": True},
                # the same quad, the same read, out in the artwork
                {"quad": quad(120.0, 900.0, 13.0, 40.0), "text": "", "score": 0.76,
                 "conf": 0.0, "vertical": True},
            ],
        }  # fmt: skip
        by_corner = {(700, 305): "よぜ", (120, 900): "よぜ"}
        by_corner.update({(700 - 60 * k, 200): text for k, text in enumerate(body)})
        reader = runner.ReconciledPageReader(
            lambda crops: [by_corner[crop] for crop in crops], _corner_crops,
            ppocr=FakePPOcrModule(raw_page, joins=False), layout=line_layout,
            reconcile=line_reconcile, second_crop_fn=_corner_crops,
        )  # fmt: skip
        page = reader(Image(833, 1186), "0.2.5")
        written = [line for block in page["blocks"] for line in block["lines"]]
        assert written.count("よぜ") == 1
        kept, dropped = reader.raw["lines"][3:]
        assert kept["notes"][-1] == "body" and dropped["notes"][-1] == "dropped"
        assert kept["merged"] == "よぜ" and kept["conf"] == 0.75
        assert dropped["notes"][-2:] == ["unbacked", "dropped"] and dropped["text"] == ""

    def test_a_hand_lettered_panel_the_detector_boxed_as_one_blob_is_kept(self) -> None:
        """Bench: Saki 02 page 129, an afterword whose middle panel is ten slanted
        hand-lettered columns. The detector returns them as ONE near-square quad,
        the CTC recognizer reads nothing in it, and the engine reads it fluently.
        An emphasis stroke in the same panel is read as "イキ" and must not stay.

        A panel is ink the line detector is right about and unsure of at once,
        so it is kept under ``DETECTOR_SURE`` -- on being a region with a
        phrase read out of it. The stroke is one glyph, and faint.
        """
        from mokuro_bunko.ocr import line_reconcile

        def quad(x: float, y: float, w: float, h: float) -> list[list[float]]:
            return [[x, y], [x + w, y], [x + w, y + h], [x, y + h]]

        panel = "あぞろをんつきぼこぜくしぶだぷかちどてづもにねぱめぽひふへと"
        body = ["隆碸ぽぞぞ鎋璂でぶぱ", "ぷろでぶぱづ峭をか", "漪ぽ嘺ゑもぞでしょろ"]
        raw_page = {
            "format": "ppocr-lines/1", "width": 833, "height": 1186,
            "lines": [
                # the page's own running text: 27 px to a glyph cell
                *(
                    {"quad": quad(700.0 - 60 * k, 200.0, 27.0, 270.0), "text": text,
                     "score": 0.9, "conf": 0.99, "vertical": True}
                    for k, text in enumerate(body)
                ),
                # the panel: 159 x 196, six pitches thick, and no line, so the
                # detector is less sure of it than of the printed columns
                {"quad": quad(340.0, 480.0, 159.0, 196.0), "text": "", "score": 0.66,
                 "conf": 0.0, "vertical": True},
                # an emphasis stroke inside that same panel, read as "イキ"
                {"quad": quad(520.0, 660.0, 9.0, 14.0), "text": "", "score": 0.5,
                 "conf": 0.0, "vertical": True},
            ],
        }  # fmt: skip
        by_corner = {(340, 480): panel, (520, 660): "イキ"}
        by_corner.update({(700 - 60 * k, 200): text for k, text in enumerate(body)})
        reader = runner.ReconciledPageReader(
            lambda crops: [by_corner[crop] for crop in crops], _corner_crops,
            ppocr=FakePPOcrModule(raw_page, joins=False), layout=line_layout,
            reconcile=line_reconcile, second_crop_fn=_corner_crops,
        )  # fmt: skip
        page = reader(Image(833, 1186), "0.2.5")
        written = [line for block in page["blocks"] for line in block["lines"]]
        assert panel in written and "イキ" not in written
        assert reader.raw["reconcile"]["body_pitch"] == 27.0
        kept, art = reader.raw["lines"][3], reader.raw["lines"][4]
        assert kept["merged"] == panel and "runaway" not in kept["notes"]
        assert kept["confirmed"] and "region" in kept["notes"]
        assert art["merged"] == "" and art["notes"][-2:] == ["unbacked", "dropped"]

    def test_a_gap_in_a_column_the_engine_read_as_text_stays_off_the_page(self) -> None:
        """Bench page 283: "ふふ" read twice from the blank cell between two pieces of a column.

        Its confidence of 0 does not keep it out -- the layout takes a doubted
        quad collinear with a column for a piece of that column.
        """
        from mokuro_bunko.ocr import line_reconcile

        def column(
            x: float,
            y0: float,
            y1: float,
            text: str,
            conf: float,
            width: float = 50.0,
            score: float = 0.9,
        ) -> dict[str, Any]:
            quad = [[x, y0], [x + width, y0], [x + width, y1], [x, y1]]
            return {"quad": quad, "text": text, "score": score, "conf": conf, "vertical": True}

        full = "あぞろをんつきぼこぜくしぶだぷかちどてづ"
        raw_page = {
            "format": "ppocr-lines/1", "width": 1925, "height": 2800,
            "lines": [
                *(column(900.0 - 70 * k, 400.0, 1400.0, full, 0.99) for k in range(3)),
                column(690.0, 400.0, 1000.0, "あぞろをんつ", 0.97),
                column(698.0, 992.0, 1052.0, "", 0.0, width=36.0, score=0.62),  # the gap
                column(690.0, 1050.0, 1400.0, "きぼこぜくしぶ", 0.95),
                *(column(620.0 - 70 * k, 400.0, 1400.0, full, 0.99) for k in range(2)),
            ],
        }  # fmt: skip
        by_corner = {
            (round(ln["quad"][0][0]), round(ln["quad"][0][1])): ln["text"] or "ふふ"
            for ln in raw_page["lines"]
        }
        reader = runner.ReconciledPageReader(
            lambda crops: [by_corner[crop] for crop in crops], _corner_crops,
            ppocr=FakePPOcrModule(raw_page, joins=False), layout=line_layout,
            reconcile=line_reconcile, second_crop_fn=_corner_crops,
        )  # fmt: skip
        page = reader(Image(1925, 2800), "0.2.5")
        written = [line for block in page["blocks"] for line in block["lines"]]
        assert "ふふ" not in written and len(written) == 7
        assert "あぞろをんつ" in written and "きぼこぜくしぶ" in written
        gap = reader.raw["lines"][4]
        # what the engine said is still there for whoever reads the dump
        assert (gap["vlm"], gap["vlm_second"]) == ("ふふ", "ふふ")
        assert gap["merged"] == gap["text"] == ""
        assert gap["confirmed"] and "dropped" in gap["notes"]


class TestDuplicatedRun:
    """Bench page 97: one column boxed as two overlapping quads, read twice by the engine.

    ``novel-text-097`` is that page (wording replaced by placeholders). On the
    old road -- bare quads from the adapter, crops with 12% of the column as
    margin -- the engine read "...X、もめに" and then "X、もめにわ".
    """

    PIECES = ("ぜめ羀錨ぽ豩豸鱋め羀錨ら涜ほだゐ。誎ぼ錿ゑむか趑、跩やつも禡め炕癲", "、もめにわ")

    def test_the_column_is_joined_and_read_once(self) -> None:
        fake = FakePPOcrModule(_raw_page("novel-text-097"))
        recognizer = FakeEngineRecognizer(fake)
        reader = _reconciled_reader(fake, recognizer)
        page = reader(Image(1925, 2800), "0.2.5")
        joined = "".join(self.PIECES)
        assert joined in recognizer.calls[0]
        assert not set(self.PIECES) & set(recognizer.calls[0])
        lines = [line for block in page["blocks"] for line in block["lines"]]
        assert lines.count(joined) == 1 and self.PIECES[1] not in lines

    def test_pieces_that_stay_apart_do_not_both_carry_the_seam(self) -> None:
        fake = FakePPOcrModule(_raw_page("novel-text-097"), joins=False)
        head, tail = self.PIECES
        # what the engine makes of two crops sharing ink: the tail piece
        # starts with the glyph the head piece ended on
        recognizer = FakeEngineRecognizer(fake, reads={tail: head[-1] + tail})
        reader = _reconciled_reader(fake, recognizer)
        page = reader(Image(1925, 2800), "0.2.5")
        lines = [line for block in page["blocks"] for line in block["lines"]]
        assert head in lines and tail in lines and head[-1] + tail not in lines
        entry = next(ln for ln in reader.raw["lines"] if ln.get("ctc") == tail)
        assert entry["vlm"] == head[-1] + tail and "seam" in entry["notes"]

    def test_a_repeat_longer_than_the_shared_ink_is_left_alone(self) -> None:
        fake = FakePPOcrModule(_raw_page("novel-text-097"), joins=False)
        head, tail = self.PIECES
        long_repeat = head[-12:] + tail  # twelve glyphs cannot fit a 26 px overlap
        recognizer = FakeEngineRecognizer(fake, reads={tail: long_repeat})
        reader = _reconciled_reader(fake, recognizer)
        reader(Image(1925, 2800), "0.2.5")
        entry = next(ln for ln in reader.raw["lines"] if ln.get("ctc") == tail)
        assert "seam" not in entry["notes"]


class TestRunLineEngine:
    def test_volume_is_written_without_a_detector_process(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        input_dir = tmp_path / "Vol"
        input_dir.mkdir()
        (input_dir / "001.webp").write_bytes(b"x")
        fake = FakePPOcrModule(_raw_page("manga-page-069"))
        monkeypatch.setattr(
            runner, "load_sibling", lambda name: fake if name == "ppocr" else line_layout
        )
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(1115, 1600))

        def no_detector(*args: Any, **kwargs: Any) -> None:
            raise AssertionError("the line engine must not start a detector process")

        monkeypatch.setattr(runner, "open_detectors", no_detector)
        output = tmp_path / "out" / "Vol.ppocr-manga.mokuro"
        args = runner.parse_args(
            [
                "--engine", "ppocr-manga", "--detector", "ctd",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(tmp_path / "out" / "_ocr" / "ppocr-manga" / "Vol"),
                "--volume-uuid", "vol-uuid",
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        volume = json.loads(output.read_text(encoding="utf-8"))
        assert volume["ocr_engine"] == {
            "id": "ppocr-manga",
            "recognizer": "Kellenok/PP-OCRv6_manga",
            "detector": "ppocr-manga",  # not the configured ctd
            "generator": "mokuro-bunko",
        }
        assert volume["volume_uuid"] == "vol-uuid"
        (page,) = volume["pages"]
        assert page["img_path"] == "001.webp" and len(page["blocks"]) >= 8
        # progress JSON in the cache, the raw lines beside the other detectors' dumps
        assert (tmp_path / "out" / "_ocr" / "ppocr-manga" / "Vol" / "001.json").is_file()
        raw_dump = tmp_path / "out" / "_detect" / "ppocr-manga" / "001.json"
        assert json.loads(raw_dump.read_text(encoding="utf-8"))["format"] == "ppocr-lines/1"


class _FakeDetectors:
    """The detector subprocess pool, without the subprocess.

    Same contract the real :class:`DetectorPool` has with the detect stage:
    ``detect(rel)`` boxes exactly that page and writes its JSON where the
    adapter would have, and nothing exists for a page until it is asked for --
    which is the whole point of the change, so a stand-in that wrote the whole
    volume up front would not be testing this road.
    """

    def __init__(
        self,
        *,
        blocks: Any = None,
        weights: dict[str, str] | None = None,
        fail: Sequence[str] = (),
    ) -> None:
        self.blocks = blocks
        # What a served detector reports on its ready line, not a file it
        # writes into some volume's directory.
        self.weights = dict(weights or {})
        self.fail = set(fail)
        self.asked: list[str] = []
        self.closed = False
        self.workers = 0
        self.device = ""

    def open(self, _detector: str, *, workers: int, device: str = "") -> Any:
        self.workers = workers
        self.device = device
        return self

    def detect(self, image: Path, destination: Path) -> str:
        name = Path(image).name
        self.asked.append(name)
        if name in self.fail:
            raise runner.DetectorPageError(f"no detection for {name}")
        blocks = self.blocks(name) if callable(self.blocks) else _blocks()
        destination = Path(destination)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(
            json.dumps({"img_width": 100, "img_height": 200, "blocks": blocks}), encoding="utf-8"
        )
        return f"blocks={len(blocks)}"

    def close(self) -> None:
        self.closed = True


class TestRunThroughADetectorAdapter:
    """``--engine hayai-nova --detector ctd``: the adapter road.

    The detector is a subprocess here, so what it loaded can only reach the
    sidecar through the adapter contract's weights file.
    """

    def test_the_sidecar_names_the_detector_weights_beside_the_recognizer(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:

        input_dir = tmp_path / "Vol"
        input_dir.mkdir()
        (input_dir / "001.webp").write_bytes(b"x")

        class _Recognizer:
            repos = {"some/recognizer": "a" * 40}

            def __call__(self, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
                return ["テスト"] * len(crops)

        detectors = _FakeDetectors(weights={"some/detector": "d" * 40})
        monkeypatch.setattr(runner, "open_detectors", detectors.open)
        monkeypatch.setattr(runner, "load_recognizer", lambda *a, **k: _Recognizer())
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(100, 200))
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _one_crop_per_line)
        monkeypatch.setattr(runner, "make_upright_crop_fn", lambda *a, **k: _one_crop_per_line)

        output = tmp_path / "out" / "Vol.hayai-nova.mokuro"
        args = runner.parse_args(
            [
                "--engine", "hayai-nova", "--detector", "ctd",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(tmp_path / "out" / "_ocr" / "hayai-nova" / "Vol"),
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        volume = json.loads(output.read_text(encoding="utf-8"))
        assert volume["ocr_engine"]["weights"] == {
            "some/detector": "d" * 40,
            "some/recognizer": "a" * 40,
        }

    def test_a_silent_adapter_makes_the_sidecar_silent_too(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """An older adapter writing no weights file is not a failure, and not
        an excuse to claim a pin nothing reported."""

        input_dir = tmp_path / "Vol"
        input_dir.mkdir()
        (input_dir / "001.webp").write_bytes(b"x")

        class _Recognizer:
            def __call__(self, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
                return ["テスト"] * len(crops)

        detectors = _FakeDetectors()
        monkeypatch.setattr(runner, "open_detectors", detectors.open)
        monkeypatch.setattr(runner, "load_recognizer", lambda *a, **k: _Recognizer())
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(100, 200))
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _one_crop_per_line)
        monkeypatch.setattr(runner, "make_upright_crop_fn", lambda *a, **k: _one_crop_per_line)

        output = tmp_path / "out" / "Vol.hayai-nova.mokuro"
        args = runner.parse_args(
            [
                "--engine", "hayai-nova", "--detector", "ctd",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(tmp_path / "out" / "_ocr" / "hayai-nova" / "Vol"),
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        volume = json.loads(output.read_text(encoding="utf-8"))
        assert "weights" not in volume["ocr_engine"]


class TestRunEngineOnLineDetector:
    """``--engine paddle-manga --detector ppocr-manga`` end to end, with stand-ins."""

    def _run(
        self,
        tmp_path: Path,
        monkeypatch: pytest.MonkeyPatch,
        *extra: str,
        engine: str = "paddle-manga",
        repos: dict[str, str] | None = None,
        ppocr_repo: tuple[str, str] | None = None,
    ) -> tuple[dict[str, Any], dict[str, Any], list[str]]:
        from mokuro_bunko.ocr import line_reconcile

        input_dir = tmp_path / "Vol"
        input_dir.mkdir()
        (input_dir / "001.webp").write_bytes(b"x")
        fake = FakePPOcrModule(_raw_page("novel-dialogue-200"))
        if ppocr_repo is not None:
            fake.REPO_ID, fake.REPO_REVISION = ppocr_repo
        siblings = {"ppocr": fake, "line_layout": line_layout, "line_reconcile": line_reconcile}
        loaded: list[str] = []

        def load_recognizer(
            engine: str,
            *,
            fold: bool = True,
            patches: int = runner.DEFAULT_PATCH_BUDGET,
            device: str | None = None,
            precision: str = runner.PRECISION_AUTO,
        ) -> Any:
            loaded.append(f"{engine} fold={fold} patches={patches}")
            recognizer = FakeEngineRecognizer(fake)
            if repos is not None:
                recognizer.repos = dict(repos)
            return recognizer

        def forbidden(*args: Any, **kwargs: Any) -> None:
            raise AssertionError("not on this road: no detector process")

        monkeypatch.setattr(runner, "load_sibling", lambda name: siblings[name])
        monkeypatch.setattr(runner, "load_recognizer", load_recognizer)
        monkeypatch.setattr(runner, "make_quad_crop_fn", lambda *margin: _corner_crops)
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _corner_crops)
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(1925, 2800))
        monkeypatch.setattr(runner, "open_detectors", forbidden)
        output = tmp_path / "out" / f"Vol.{engine}.mokuro"
        args = runner.parse_args(
            [
                "--engine", engine, "--detector", "ppocr-manga",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(tmp_path / "out" / "_ocr" / engine / "Vol"),
                *extra,
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        volume = json.loads(output.read_text(encoding="utf-8"))
        raw = json.loads(
            (tmp_path / "out" / "_detect" / engine / "001.json").read_text("utf-8")
        )
        return volume, raw, loaded

    def test_the_sidecar_is_the_merged_read_and_nothing_else(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        volume, raw, loaded = self._run(tmp_path, monkeypatch)
        assert volume["ocr_engine"] == {
            "id": "paddle-manga",
            "recognizer": "sorryhyun/paddleocr-vl-1.6-manga-lora",
            "detector": "ppocr-manga",
            "generator": "mokuro-bunko",
        }
        # A block is a box, an orientation, a font size, the line quads and the
        # lines. Nothing places characters inside a line any more.
        (page,) = volume["pages"]
        assert all(
            set(block) == {"box", "vertical", "font_size", "lines_coords", "lines"}
            for block in page["blocks"]
        )
        assert "「翆れ」" in [line for block in page["blocks"] for line in block["lines"]]
        # the engine's text is wanted as generated: reconciling undoes no folding
        assert loaded == ["paddle-manga fold=False patches=512"]
        # both reads per line and the page tally, in the raw dump only
        assert raw["format"] == "ppocr-lines/1" and raw["reconcile"]["lines"] == 12
        assert {"vlm", "ctc", "merged", "agreement"} <= set(raw["lines"][0])

    def test_sidecar_records_the_budget_and_the_resolved_weights(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A file says what read it: the resolution AND the exact commits.

        Two model pairs read a reconciled page, so both are named, and the
        budget reaches ``ocr_engine`` only for the engine it applies to.
        """
        volume, _raw, loaded = self._run(
            tmp_path,
            monkeypatch,
            "--patches",
            "384",
            engine="hayai-nova",
            repos={"some/recognizer": "a" * 40, "some/processor": "b" * 40},
            ppocr_repo=("some/ctc", "c" * 40),
        )
        assert loaded == ["hayai-nova fold=False patches=384"]
        assert volume["ocr_engine"] == {
            "id": "hayai-nova",
            "recognizer": "JustANormalTinkerer/hayai-ocr-v2.5-nova",
            "detector": "ppocr-manga",
            "generator": "mokuro-bunko",
            "patch_budget": 384,
            "weights": {
                "some/ctc": "c" * 40,
                "some/recognizer": "a" * 40,
                "some/processor": "b" * 40,
            },
        }

    def test_an_engine_without_a_budget_claims_none(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """``--patches`` on paddle-manga's command line would be a claim
        about a resolution its recognizer never read at."""
        volume, _raw, _loaded = self._run(
            tmp_path, monkeypatch, "--patches", "256", engine="paddle-manga"
        )
        assert "patch_budget" not in volume["ocr_engine"]

    def test_weights_are_omitted_when_nothing_reported_them(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Collected from the loaders, never from a static table: a run that
        cannot say what it resolved says nothing rather than guessing."""
        volume, _raw, _loaded = self._run(tmp_path, monkeypatch)
        assert "weights" not in volume["ocr_engine"]

    def test_lines_the_two_reads_differ_on_are_listed_for_a_proofreader(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        real = FakeEngineRecognizer.__call__

        def misread(self: Any, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
            return [text.replace("宵", "鴩") for text in real(self, crops, max_tokens)]

        monkeypatch.setattr(FakeEngineRecognizer, "__call__", misread)
        self._run(tmp_path, monkeypatch)
        listed = json.loads(
            (tmp_path / "out" / "_detect" / "paddle-manga" / "review.json").read_text("utf-8")
        )
        assert (listed["format"], listed["engine"]) == ("ocr-review/1", "paddle-manga")
        (page,) = listed["pages"]
        assert page["page"] == "001.webp"
        # only the lines with the misread glyph, out of the page's twelve
        assert all("宵" in entry["ctc"] for entry in page["lines"]) and len(page["lines"]) < 4
        entry = next(entry for entry in page["lines"] if entry["ctc"] == "宵むぱをよ」")
        assert (entry["text"], entry["vlm"]) == ("鴩むぱをよ」", "鴩むぱをよ")
        assert entry["agreement"] < 1 and len(entry["quad"]) == 4


class TestBlankPage:
    """A page the engine failed on stays in the sidecar, empty."""

    def test_carries_the_image_size_and_no_blocks(self, tmp_path: Path) -> None:
        from PIL import Image

        path = tmp_path / "p.png"
        Image.new("L", (120, 80), 255).save(path)
        page = runner.blank_page(path)
        assert page == {
            "version": runner.MOKURO_FORMAT_VERSION,
            "img_width": 120,
            "img_height": 80,
            "blocks": [],
        }

    def test_is_none_for_a_file_that_is_not_an_image(self, tmp_path: Path) -> None:
        path = tmp_path / "p.png"
        path.write_bytes(b"not an image")
        assert runner.blank_page(path) is None


# --------------------------------------------------------------------------
# The scheduler: one pool a stage, a bounded queue between every pair
# --------------------------------------------------------------------------


class TestStageQueue:
    """The bounded hand-off. Capacity, real blocking, and what it counts."""

    def test_capacity_is_the_most_that_can_wait(self) -> None:
        q = runner.StageQueue("a->b", 2)
        assert q.put(1) and q.put(2)
        assert q.depth == 2
        landed = threading.Event()
        thread = threading.Thread(target=lambda: (q.put(3), landed.set()), daemon=True)
        thread.start()
        assert not landed.wait(0.2), "a third page went into a queue of two"
        assert q.depth == 2
        assert q.get() == 1
        assert landed.wait(5), "a slot opened and the producer never took it"
        thread.join(5)
        assert q.depth == 2

    def test_a_blocked_producer_is_asleep_not_spinning(self) -> None:
        """Backpressure has to GIVE THE RESOURCE BACK, not burn it waiting.

        Half a second parked on a full queue must cost no measurable CPU --
        this is what lets a GPU-side producer that has run ahead hand the
        FLOPs to the stage behind it instead of spinning on them.
        """
        q = runner.StageQueue("gpu->post", 1)
        q.put("held")
        burned: list[float] = []

        def produce() -> None:
            start = time.thread_time()
            q.put("waits")
            burned.append(time.thread_time() - start)

        thread = threading.Thread(target=produce, daemon=True)
        thread.start()
        time.sleep(0.5)
        assert q.get() == "held"
        thread.join(5)
        assert burned, "the producer never came back"
        assert burned[0] < 0.05, f"a parked producer burned {burned[0]:.3f}s of CPU"
        assert q.report(1.0).blocked_seconds >= 0.4

    def test_a_blocked_consumer_is_asleep_too(self) -> None:
        q = runner.StageQueue("a->b", 4)
        burned: list[float] = []
        got: list[Any] = []

        def consume() -> None:
            start = time.thread_time()
            got.append(q.get())
            burned.append(time.thread_time() - start)

        thread = threading.Thread(target=consume, daemon=True)
        thread.start()
        time.sleep(0.4)
        q.put("late")
        thread.join(5)
        assert got == ["late"]
        assert burned[0] < 0.05, f"a starved consumer burned {burned[0]:.3f}s of CPU"
        assert q.report(1.0).starved_seconds >= 0.3

    def test_waiting_is_counted_on_the_side_that_waited(self) -> None:
        """Blocked belongs to the producer, starved to the consumer -- never mixed."""
        q = runner.StageQueue("a->b", 1)
        q.put(1)
        blocked = threading.Thread(target=lambda: q.put(2), daemon=True)
        blocked.start()
        time.sleep(0.2)
        assert q.get() == 1
        blocked.join(5)
        report = q.report(1.0)
        assert report.blocked_seconds >= 0.15 and report.blocked_events == 1
        assert report.starved_seconds == 0.0 and report.starved_events == 0

    def test_a_finished_queue_hands_out_what_is_left_then_a_sentinel(self) -> None:
        q = runner.StageQueue("a->b", 4)
        q.put(1)
        q.finish()
        assert q.get() == 1
        assert q.get() is runner._SENTINEL
        assert q.get() is runner._SENTINEL  # and stays that way

    def test_closing_wakes_both_sides_and_refuses_more(self) -> None:
        q = runner.StageQueue("a->b", 1)
        q.put(1)
        refused: list[bool] = []
        thread = threading.Thread(target=lambda: refused.append(q.put(2)), daemon=True)
        thread.start()
        time.sleep(0.1)
        q.close()
        thread.join(5)
        assert refused == [False]
        assert q.get() is runner._SENTINEL
        assert q.put(3) is False

    def test_mean_depth_is_weighted_by_time_not_by_put(self) -> None:
        """One page sitting for a second is a full queue; a hundred passing is not."""
        q = runner.StageQueue("a->b", 4)
        for n in range(100):
            q.put(n)
            q.get()
        brief = q.report(1.0)
        assert brief.mean_depth < 0.1 and brief.puts == 100
        held = runner.StageQueue("c->d", 4)
        held.put("stays")
        time.sleep(0.3)
        assert held.report(0.3).mean_depth > 0.9

    def test_a_capacity_below_one_is_refused(self) -> None:
        for capacity in (0, -1):
            with pytest.raises(ValueError, match="capacity of at least 1"):
                runner.StageQueue("a->b", capacity)


class TestStagePipeline:
    """Pools, queues, order, backpressure and what the run reports."""

    @staticmethod
    def _stage(
        key: str,
        run: Any,
        *,
        workers: int = 2,
        capacity: int = 2,
        device: str = runner.DEVICE_CPU,
    ) -> runner.Stage:
        spec = runner.StageSpec(key, key, device, runner.POOLED, 0.1)
        return runner.Stage(spec, run, workers, capacity)

    def test_a_baseexception_in_a_stage_is_carried_like_any_other_failure(self) -> None:
        """Outcome.error is typed BaseException, so _apply must catch that wide.

        Catching only Exception let a BaseException kill the worker; its finally
        decremented the countdown, the last worker out finished the output queue,
        and the drain ended CLEANLY on a truncated volume with no error anywhere.
        """

        class Rude(BaseException):
            pass

        def boom(item: int, payload: Any) -> int:
            if item == 7:
                raise Rude("not an Exception subclass")
            return payload * 10

        pipe = runner.StagePipeline([self._stage("a", boom, workers=1, capacity=2)])
        out = list(pipe.run(range(20)))
        pipe.close()

        assert len(out) == 20, "every page must still arrive"
        failed = [(item, oc) for item, oc in out if oc.error is not None]
        assert [item for item, _ in failed] == [7]
        assert isinstance(failed[0][1].error, Rude)

    def test_a_worker_lost_abnormally_raises_instead_of_truncating(self) -> None:
        """A dying worker must not pass for a finished one.

        The pool's finally closes the output queue when the last worker leaves,
        so a worker lost outside _apply ends the drain exactly like completion.
        Without the guard the caller got a short page list and no error at all.
        """
        pipe = runner.StagePipeline([self._stage("a", lambda i, p: p, workers=1, capacity=2)])

        started = pipe.run(range(50))
        first = next(started)
        # What _work records when a worker is lost outside _apply.
        pipe._worker_error = RuntimeError("worker died outside _apply")

        with pytest.raises(RuntimeError, match="worker died outside _apply"):
            list(started)
        pipe.close()
        assert first is not None

    def test_order_is_the_input_order_however_the_work_finishes(self) -> None:
        """The last page is done first and still comes out last."""
        total = 20

        def slow_first(item: int, payload: Any) -> int:
            if item == 0:
                time.sleep(0.3)
            return payload * 10

        stages = [
            self._stage("a", slow_first, workers=4, capacity=4),
            self._stage("b", lambda i, p: p, workers=2, capacity=2),
        ]
        out = [o.unwrap() for _i, o in runner.staged_pipeline(range(total), stages)]
        assert out == [n * 10 for n in range(total)]

    def test_each_item_walks_its_stages_in_order(self) -> None:
        trail: list[str] = []

        def one(item: int, payload: Any) -> str:
            assert payload == item  # the first stage is handed the item itself
            return f"a{item}"

        def two(item: int, payload: str) -> str:
            trail.append(f"{payload}->b{item}")
            return f"b{item}"

        stages = [self._stage("a", one, workers=1), self._stage("b", two, workers=1)]
        out = [o.unwrap() for _i, o in runner.staged_pipeline(range(3), stages)]
        assert out == ["b0", "b1", "b2"]
        assert sorted(trail) == ["a0->b0", "a1->b1", "a2->b2"]

    def test_every_stage_gets_a_pool_of_its_own_size(self) -> None:
        """Three stages, three widths, and each one really runs that wide."""
        peak: dict[str, int] = {"a": 0, "b": 0, "c": 0}
        live: dict[str, int] = {"a": 0, "b": 0, "c": 0}
        lock = threading.Lock()

        def busy(key: str, delay: float) -> Any:
            def run(_item: int, payload: Any) -> Any:
                with lock:
                    live[key] += 1
                    peak[key] = max(peak[key], live[key])
                time.sleep(delay)
                with lock:
                    live[key] -= 1
                return payload

            return run

        # ``b`` is the cheap one in the middle, so it is not what keeps ``c``
        # from filling: a stage's width is its own, and only its feeder's rate
        # can stop it being used.
        stages = [
            self._stage("a", busy("a", 0.02), workers=4, capacity=4),
            self._stage("b", busy("b", 0.001), workers=1, capacity=3),
            self._stage("c", busy("c", 0.02), workers=3, capacity=3),
        ]
        list(runner.staged_pipeline(range(120), stages))
        assert peak["a"] == 4, peak
        assert peak["b"] == 1, "a stage of one must never run two pages at once"
        assert peak["c"] == 3, peak

    def test_a_pool_shares_its_work_out_over_every_member(self) -> None:
        """Whichever member is free takes the next page; none of them idles."""
        threads: set[int] = set()
        lock = threading.Lock()

        def run(_item: int, payload: Any) -> Any:
            with lock:
                threads.add(threading.get_ident())
            time.sleep(0.005)
            return payload

        stages = [self._stage("a", run, workers=3, capacity=3)]
        list(runner.staged_pipeline(range(90), stages))
        assert len(threads) == 3, f"{len(threads)} of 3 pool members ever ran"

    def test_a_full_queue_parks_the_producer_at_a_bounded_depth(self) -> None:
        """A fast stage in front of a slow one settles; it does not run away.

        This is the whole mechanism: the producer is asleep on the queue, the
        queue never grows past its capacity, and the summary attributes the
        wait to the producer as blocked time and names the consumer as the
        bottleneck.
        """
        produced = 0
        lock = threading.Lock()

        def fast(_item: int, payload: Any) -> Any:
            nonlocal produced
            with lock:
                produced += 1
            return payload

        def slow(_item: int, payload: Any) -> Any:
            time.sleep(0.02)
            return payload

        stages = [
            self._stage("fast", fast, workers=2, capacity=2),
            self._stage("slow", slow, workers=1, capacity=2),
        ]
        pipeline = runner.StagePipeline(stages)
        stream = pipeline.run(range(60))
        next(stream)  # let the pipeline fill and settle
        time.sleep(0.2)
        in_flight = produced
        # what can be in hand: the two queues, the two pools and the sink
        assert in_flight <= 2 + 2 + 2 + 1 + 2 + 4, in_flight
        for _ in stream:
            pass
        report = pipeline.report()
        blocked = {s.key: s.blocked_seconds for s in report.stages}
        assert blocked["fast"] > 0.2, blocked
        assert report.bottleneck() is not None and report.bottleneck().key == "slow"
        fast_to_slow = next(q for q in report.queues if q.name == "fast->slow")
        assert fast_to_slow.max_depth <= 2
        assert fast_to_slow.fill > 0.7, "a backed-up queue must read as backed up"

    def test_a_starved_stage_reads_as_starved_and_names_its_feeder(self) -> None:
        """The other way round: the consumer waits, and the numbers say so."""

        def slow(_item: int, payload: Any) -> Any:
            time.sleep(0.02)
            return payload

        stages = [
            self._stage("slow", slow, workers=1, capacity=2),
            self._stage("quick", lambda i, p: p, workers=2, capacity=2),
        ]
        pipeline = runner.StagePipeline(stages)
        list(pipeline.run(range(40)))
        report = pipeline.report()
        starved = {s.key: s.starved_seconds for s in report.stages}
        assert starved["quick"] > 0.3, starved
        assert report.bottleneck().key == "slow"
        slow_to_quick = next(q for q in report.queues if q.name == "slow->quick")
        assert slow_to_quick.fill < 0.5, "a queue nobody can fill must read as empty"
        assert slow_to_quick.starved_seconds > 0.3

    def test_the_summary_says_something_a_person_can_act_on(self) -> None:
        stages = [
            self._stage("a", lambda i, p: p, workers=2, capacity=2),
            self._stage("b", lambda i, p: time.sleep(0.01) or p, workers=1, capacity=2),
        ]
        pipeline = runner.StagePipeline(stages)
        list(pipeline.run(range(20)))
        text = "\n".join(pipeline.report().lines())
        assert "stage a" in text and "stage b" in text
        assert "queue in->a" in text and "queue a->b" in text and "queue b->out" in text
        assert "bottleneck: b" in text
        live = pipeline.snapshot()
        assert live["bottleneck"] == "b"
        assert {q["name"] for q in live["queues"]} == {"in->a", "a->b", "b->out"}
        assert all("mean_depth" in q and "max_depth" in q for q in live["queues"])
        assert all("starved_seconds" in s and "blocked_seconds" in s for s in live["stages"])
        assert json.loads(json.dumps(live))  # what the stats file has to carry

    def test_capacity_one_and_two_hundred_items_never_deadlocks(self) -> None:
        stages = [
            self._stage("a", lambda i, p: p, workers=1, capacity=1),
            self._stage("b", lambda i, p: p, workers=3, capacity=1),
            self._stage("c", lambda i, p: p, workers=2, capacity=1),
        ]
        pipeline = runner.StagePipeline(stages, source_capacity=1)
        out = [o.unwrap() for _i, o in pipeline.run(range(200))]
        assert out == list(range(200))

    def test_memory_stays_flat_over_a_long_volume(self) -> None:
        """A 292-page volume is never more than a queueful of pages in hand."""
        live = 0
        peak = 0
        lock = threading.Lock()

        class Page:
            def __init__(self) -> None:
                nonlocal live, peak
                with lock:
                    live += 1
                    peak = max(peak, live)

            def drop(self) -> None:
                nonlocal live
                with lock:
                    live -= 1

        def decode(_item: int, _payload: Any) -> Page:
            return Page()

        def read(_item: int, page: Page) -> Page:
            time.sleep(0.001)
            return page

        def finish(_item: int, page: Page) -> int:
            page.drop()
            return 0

        widths = runner.stage_widths("paddle-manga", runner.ROAD_RECONCILED, budget=7)
        caps = runner.stage_capacities(runner.ROAD_RECONCILED, widths)
        stages = runner.page_stages(
            runner.ROAD_RECONCILED, (decode, read, finish), engine="paddle-manga", budget=7
        )
        assert [s.workers for s in stages] == widths
        pipeline = runner.StagePipeline(stages)
        list(pipeline.run(range(292)))
        # every slot of every queue plus every worker of every pool
        ceiling = sum(caps) + sum(widths) + runner.StagePipeline.SOURCE_EXTRA + widths[0]
        assert peak <= ceiling, f"{peak} pages in hand at once, ceiling {ceiling}"
        assert peak < 30, peak

    @pytest.mark.parametrize("limit", [None, 10**6])
    def test_one_slow_page_does_not_pile_the_volume_up_behind_it(self, limit: int | None) -> None:
        """The reorder buffer is bounded too, or a stuck page costs the volume.

        Page 0 sits in the last stage while the rest of a 300-page volume tries
        to stream past it. The queues alone do not stop that: a finished page
        the driver has taken off the sink is out of every queue but still in
        memory, waiting for the head. With the in-flight ceiling the feeder is
        parked and the pipeline holds its own width; without it (the ``10**6``
        case, which is what a pipeline with no ceiling would do) it holds all
        300 decoded pages.
        """
        started = threading.Event()
        release = threading.Event()
        live = 0
        peak = 0
        lock = threading.Lock()

        class Page:
            """Stands in for the decoded image a real payload carries."""

            def __init__(self) -> None:
                nonlocal live, peak
                with lock:
                    live += 1
                    peak = max(peak, live)

            def drop(self) -> None:
                nonlocal live
                with lock:
                    live -= 1

        def decode(_item: int, _payload: Any) -> Page:
            return Page()

        def hold(item: int, page: Page) -> Page:
            if item == 0:
                started.set()
                assert release.wait(20), "the head never got to finish"
            return page

        stages = [
            self._stage("decode", decode, workers=2, capacity=2),
            self._stage("hold", hold, workers=4, capacity=2),
        ]
        pipeline = runner.StagePipeline(stages, in_flight_limit=limit)
        stream = pipeline.run(range(300))
        # The driver has to be another thread: the head is what it would block
        # on, and the head is what this test is holding. Only the driver drops
        # a page, so a page the driver is holding is still counted.
        out: list[Any] = []

        def drive() -> None:
            for item, outcome in stream:
                outcome.unwrap().drop()
                out.append(item)

        driver = threading.Thread(target=drive, daemon=True)
        driver.start()
        assert started.wait(20), "the head never reached the last stage"
        time.sleep(0.5)  # let everything that can run, run
        stalled = peak
        release.set()
        driver.join(60)
        assert out == list(range(300))
        # source runway + every queue slot + every worker, and nothing like 300
        ceiling = (2 + runner.StagePipeline.SOURCE_EXTRA) + (2 + 2) + (2 + 4)
        if limit is None:
            assert stalled <= ceiling, f"{stalled} pages in flight behind one stuck page"
        else:
            assert stalled > ceiling, (
                "this is the hole the ceiling closes; if it no longer opens, "
                "the bound is coming from somewhere else and the test proves nothing"
            )

    @pytest.mark.parametrize("failing", [0, 1, 2])
    def test_a_failure_costs_that_page_and_no_other(self, failing: int) -> None:
        seen: list[Any] = []
        lock = threading.Lock()

        def stage(index: int) -> Any:
            def run(item: int, payload: Any) -> str:
                if index == failing and item == 2:
                    raise ValueError(f"page {item} died in stage {index}")
                with lock:
                    seen.append((index, item))
                return f"s{index}:{item}"

            return run

        stages = [
            self._stage("a", stage(0), workers=2),
            self._stage("b", stage(1), workers=1),
            self._stage("c", stage(2), workers=2),
        ]
        out: list[Any] = []
        for item, outcome in runner.staged_pipeline(range(5), stages):
            try:
                out.append(outcome.unwrap())
            except ValueError as e:
                assert item == 2 and f"stage {failing}" in str(e)
                out.append("blank")
        assert out == ["s2:0", "s2:1", "blank", "s2:3", "s2:4"]
        # a page that died never reached any stage after the one it died in
        assert not [entry for entry in seen if entry[1] == 2 and entry[0] >= failing]

    def test_a_carried_failure_still_has_its_traceback(self) -> None:
        def boom(item: int, _payload: Any) -> None:
            raise RuntimeError("from the worker")

        stages = [self._stage("a", boom), self._stage("b", lambda i, p: p)]
        (_item, outcome), *_ = runner.staged_pipeline([1], stages)
        try:
            outcome.unwrap()
        except RuntimeError:
            assert "from the worker" in traceback.format_exc()
            assert "in boom" in traceback.format_exc()
        else:  # pragma: no cover - the raise above is unconditional
            raise AssertionError("unwrap must re-raise")

    def test_a_source_that_dies_surfaces_after_what_it_did_produce(self) -> None:
        def pages() -> Any:
            yield 1
            yield 2
            raise OSError("the volume went away")

        stages = [self._stage("a", lambda i, p: p, workers=2)]
        stream = runner.staged_pipeline(pages(), stages)
        seen = []
        with pytest.raises(OSError, match="went away"):
            for _item, outcome in stream:
                seen.append(outcome.unwrap())
        assert seen == [1, 2]

    def test_every_stage_serial_is_the_serial_path(self) -> None:
        """No threads, and each page walked through every stage before the next."""
        order: list[str] = []
        stages = [
            self._stage("a", lambda i, p: order.append(f"a{i}"), workers=runner.SERIAL),
            self._stage("b", lambda i, p: order.append(f"b{i}"), workers=runner.SERIAL),
        ]
        pipeline = runner.StagePipeline(stages)
        assert pipeline.serial
        before = threading.active_count()
        for _item, outcome in pipeline.run(range(3)):
            outcome.unwrap()
        assert order == ["a0", "b0", "a1", "b1", "a2", "b2"]
        assert threading.active_count() == before
        report = pipeline.report()
        assert report.items == 3
        # ...and it says so, rather than reporting "a pool of 0"
        text = "\n".join(report.lines())
        assert not report.queues and "fused" in text and "serial fallback" in text
        assert "x0" not in text

    def test_a_serial_stage_in_the_middle_fuses_into_the_one_before_it(self) -> None:
        """No pool of its own means no queue either: one worker runs both."""
        threads: dict[str, set[int]] = {"a": set(), "b": set(), "c": set()}
        lock = threading.Lock()

        def mark(key: str) -> Any:
            def run(_item: int, payload: Any) -> Any:
                with lock:
                    threads[key].add(threading.get_ident())
                return payload

            return run

        stages = [
            self._stage("a", mark("a"), workers=2),
            self._stage("b", mark("b"), workers=runner.SERIAL),
            self._stage("c", mark("c"), workers=2),
        ]
        pipeline = runner.StagePipeline(stages)
        list(pipeline.run(range(20)))
        assert threads["a"] == threads["b"], "a fused stage runs on its host's threads"
        assert not threads["a"] & threads["c"]
        assert [q.name for q in pipeline.report().queues] == ["in->a", "a->c", "c->out"]

    def test_a_serial_first_stage_still_gets_a_pool_of_one(self) -> None:
        """It cannot fuse backwards, and the feeder must not become a worker."""
        stages = [
            self._stage("a", lambda i, p: p, workers=runner.SERIAL),
            self._stage("b", lambda i, p: p, workers=2),
        ]
        pipeline = runner.StagePipeline(stages)
        assert not pipeline.serial
        assert pipeline.stages[0].workers == 1
        assert [o.unwrap() for _i, o in pipeline.run(range(5))] == list(range(5))

    def test_a_width_of_one_produces_what_the_serial_path_produces(self) -> None:
        def a(item: int, _payload: Any) -> str:
            return f"a{item}"

        def b(item: int, payload: str) -> str:
            return f"{payload}b{item}"

        def graph(width: int) -> list[runner.Stage]:
            return [self._stage("a", a, workers=width), self._stage("b", b, workers=width)]

        serial = [o.unwrap() for _i, o in runner.staged_pipeline(range(30), graph(runner.SERIAL))]
        single = [o.unwrap() for _i, o in runner.staged_pipeline(range(30), graph(1))]
        pooled = [o.unwrap() for _i, o in runner.staged_pipeline(range(30), graph(4))]
        assert single == serial and pooled == serial

    def test_closing_the_stream_leaves_no_thread_behind(self) -> None:
        before = threading.active_count()
        stages = [
            self._stage("a", lambda i, p: p, workers=3, capacity=2),
            self._stage("b", lambda i, p: p, workers=2, capacity=2),
        ]
        stream = runner.staged_pipeline(range(5000), stages)
        with contextlib.closing(stream):
            next(stream)
        deadline = time.time() + 10
        while threading.active_count() > before and time.time() < deadline:
            time.sleep(0.01)
        assert threading.active_count() == before

    def test_a_consumer_that_gives_up_leaves_nothing_running(self) -> None:
        before = threading.active_count()
        stages = [self._stage("a", lambda i, p: p, workers=3, capacity=2)]
        stream = runner.staged_pipeline(range(5000), stages)
        with contextlib.closing(stream), pytest.raises(RuntimeError, match="gave up"):
            for _item, outcome in stream:
                outcome.unwrap()
                raise RuntimeError("the consumer gave up")
        deadline = time.time() + 10
        while threading.active_count() > before and time.time() < deadline:
            time.sleep(0.01)
        assert threading.active_count() == before

    def test_an_empty_volume_runs_nothing_and_leaves_nothing(self) -> None:
        before = threading.active_count()

        def never(item: Any, payload: Any) -> Any:
            raise AssertionError("no pages, no stages")

        stages = [self._stage("a", never), self._stage("b", never)]
        assert list(runner.staged_pipeline([], stages)) == []
        deadline = time.time() + 10
        while threading.active_count() > before and time.time() < deadline:
            time.sleep(0.01)
        assert threading.active_count() == before

    def test_a_pipeline_runs_once(self) -> None:
        """Its queues, pools and counters are one run's; a second would leak threads."""
        pipeline = runner.StagePipeline([self._stage("a", lambda i, p: p, workers=2)])
        assert [o.unwrap() for _i, o in pipeline.run(range(4))] == list(range(4))
        with pytest.raises(RuntimeError, match="already run"):
            pipeline.run(range(4))

    def test_more_stages_than_two_still_hold_order_and_content(self) -> None:
        stages = [
            self._stage("a", lambda i, p: p + 1, workers=3, capacity=2),
            self._stage("b", lambda i, p: p * 2, workers=1, capacity=2),
            self._stage("c", lambda i, p: p - 1, workers=4, capacity=2),
        ]
        out = [o.unwrap() for _i, o in runner.staged_pipeline(range(50), stages)]
        assert out == [(n + 1) * 2 - 1 for n in range(50)]


class TestPPOcrPool:
    """One onnxruntime session per detect worker, and none of them shared."""

    def test_a_pool_of_one_is_the_readers_own_engine(self) -> None:
        own = object()

        def make() -> Any:
            raise AssertionError("a single worker must not build a second session")

        pool = runner.PPOcrPool(own, make, 1)
        assert pool.size == 1
        with pool.lease() as engine:
            assert engine is own
        with pool.lease() as engine:  # and it came back
            assert engine is own

    def test_every_worker_gets_a_session_of_its_own(self) -> None:
        built: list[str] = []

        def make() -> str:
            built.append(f"clone{len(built)}")
            return built[-1]

        pool = runner.PPOcrPool("reader", make, 3)
        assert built == ["clone0", "clone1"]  # the reader's own is the third
        with pool.lease() as a, pool.lease() as b, pool.lease() as c:
            assert {a, b, c} == {"reader", "clone0", "clone1"}

    def test_a_lease_comes_back_even_when_the_page_fails(self) -> None:
        pool = runner.PPOcrPool("reader", lambda: "clone", 2)
        with pytest.raises(ValueError, match="bad page"), pool.lease():
            raise ValueError("bad page")
        with pool.lease() as a, pool.lease() as b:
            assert {a, b} == {"reader", "clone"}

    def test_a_pool_is_never_empty(self) -> None:
        pool = runner.PPOcrPool("reader", lambda: "clone", 0)
        assert pool.size == 1
        with pool.lease() as engine:
            assert engine == "reader"


class TestCpuWorkerOverride:
    """An explicit width, or nothing and let each stage be sized."""

    def test_the_flag_beats_the_environment_beats_the_derivation(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv(runner.CPU_WORKERS_ENV, "6")
        assert runner.resolve_cpu_workers(2) == 2
        assert runner.resolve_cpu_workers(None) == 6
        monkeypatch.delenv(runner.CPU_WORKERS_ENV)
        assert runner.resolve_cpu_workers(None) is None  # nothing forced: derive per stage

    def test_zero_is_a_real_answer_not_a_missing_one(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The serial fallback has to be askable for, from either side."""
        monkeypatch.delenv(runner.CPU_WORKERS_ENV, raising=False)
        assert runner.resolve_cpu_workers(0) == 0
        monkeypatch.setenv(runner.CPU_WORKERS_ENV, "0")
        assert runner.resolve_cpu_workers(None) == 0

    def test_junk_in_the_environment_is_ignored(self, monkeypatch: pytest.MonkeyPatch) -> None:
        for junk in ("", "  ", "lots", "-2", "2.5"):
            monkeypatch.setenv(runner.CPU_WORKERS_ENV, junk)
            assert runner.resolve_cpu_workers(None) is None

    def test_the_cli_default_is_auto(self) -> None:
        args = runner.parse_args(
            ["--engine", "ppocr-manga", "--input", "i", "--output", "o", "--cache-dir", "c"]
        )
        assert args.cpu_workers is None
        args = runner.parse_args(
            [
                "--engine", "ppocr-manga", "--input", "i", "--output", "o",
                "--cache-dir", "c", "--cpu-workers", "5",
            ]
        )  # fmt: skip
        assert args.cpu_workers == 5


class TestPipelinedRun:
    """``run()`` over several pages: pooled detection writes the serial volume."""

    PAGES = ("001.webp", "002.webp", "003.webp", "004.webp", "005.webp", "006.webp")
    FIXTURES = ("manga-page-066", "manga-page-069", "novel-text-013")

    def _run(
        self,
        root: Path,
        monkeypatch: pytest.MonkeyPatch,
        workers: int,
        *,
        fail_on: str = "",
        extra: Sequence[str] = (),
    ) -> tuple[bytes, FakePPOcrModule]:
        input_dir = root / "Vol"
        input_dir.mkdir(parents=True)
        for name in self.PAGES:
            (input_dir / name).write_bytes(b"x")
        # a different cached page per image, so a page out of order shows up
        by_name = {
            name: _raw_page(self.FIXTURES[i % len(self.FIXTURES)])
            for i, name in enumerate(self.PAGES)
        }
        fake = FakePPOcrModule(by_name[self.PAGES[0]], by_name=by_name)

        def imread(path: Path) -> Any:
            if path.name == fail_on:
                raise OSError(f"cannot decode {path.name}")
            return Image(1115, 1600, path.name)

        monkeypatch.setattr(
            runner, "load_sibling", lambda name: fake if name == "ppocr" else line_layout
        )
        monkeypatch.setattr(runner, "imread_bgr", imread)
        monkeypatch.setattr(runner, "blank_page", lambda path: {
            "version": runner.MOKURO_FORMAT_VERSION,
            "img_width": 1115,
            "img_height": 1600,
            "blocks": [],
        })  # fmt: skip
        output = root / "out" / "Vol.ppocr-manga.mokuro"
        args = runner.parse_args(
            [
                "--engine", "ppocr-manga",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(root / "out" / "_ocr" / "ppocr-manga" / "Vol"),
                "--title-uuid", "title-uuid", "--volume-uuid", "vol-uuid",
                "--title", "Vol", "--volume", "Vol",
                "--cpu-workers", str(workers), *extra,
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        return output.read_bytes(), fake

    def test_every_width_writes_the_same_bytes_as_the_serial_path(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        serial, none = self._run(tmp_path / "serial", monkeypatch, workers=0)
        single, one = self._run(tmp_path / "single", monkeypatch, workers=1)
        pooled, three = self._run(tmp_path / "pooled", monkeypatch, workers=3)
        assert not none.clones and not one.clones, "one session until a second is needed"
        assert len(three.clones) == 2, "three workers, three sessions"
        assert single == serial and pooled == serial
        # and it is a real volume, not two identical empties
        volume = json.loads(serial)
        assert [page["img_path"] for page in volume["pages"]] == list(self.PAGES)
        assert sum(len(page["blocks"]) for page in volume["pages"]) > 0

    def test_pages_keep_their_own_content_when_detection_is_pooled(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        pooled, _fake = self._run(tmp_path / "pooled", monkeypatch, workers=3)
        pages = json.loads(pooled)["pages"]
        # three fixtures round-robin over six pages: 1 and 4 match, 1 and 2 do not
        shapes = [[block["box"] for block in page["blocks"]] for page in pages]
        assert shapes[0] == shapes[3] and shapes[1] == shapes[4]
        assert shapes[0] != shapes[1]

    def test_a_page_that_fails_in_the_detect_stage_is_blank_and_in_place(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        serial, _ = self._run(tmp_path / "serial", monkeypatch, workers=0, fail_on="003.webp")
        single, _ = self._run(tmp_path / "single", monkeypatch, workers=1, fail_on="003.webp")
        pooled, _ = self._run(tmp_path / "pooled", monkeypatch, workers=3, fail_on="003.webp")
        assert single == serial and pooled == serial
        pages = json.loads(pooled)["pages"]
        assert [page["img_path"] for page in pages] == list(self.PAGES)
        assert pages[2]["blocks"] == []
        assert pages[3]["blocks"], "the pages after a bad one still read"

    def test_the_pool_never_outnumbers_the_pages(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A four-page volume must not pay for eight onnxruntime sessions."""
        _bytes, fake = self._run(tmp_path / "wide", monkeypatch, workers=99)
        assert len(fake.clones) == len(self.PAGES) - 1

    def test_only_the_detect_stages_width_buys_ppocr_sessions(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Widening the layout must not buy sessions nothing ever leases."""
        serial, _ = self._run(tmp_path / "serial", monkeypatch, workers=0)
        wide_tail, fake = self._run(
            tmp_path / "tail",
            monkeypatch,
            workers=1,
            extra=("--stage-workers", "layout=5"),
        )
        assert not fake.clones, "a detect stage of one needs exactly one session"
        assert wide_tail == serial


class TestPipelinedReconciledRun:
    """The road every GPU engine takes: pooled detection, one recognizer.

    ``--engine paddle-manga --detector ppocr-manga`` over several pages. The
    recognizer is a stand-in, so this runs without a GPU, but it is the real
    ``ReconciledPageReader`` split across the two stages.
    """

    PAGES = ("001.webp", "002.webp", "003.webp", "004.webp")
    FIXTURES = ("novel-dialogue-200", "novel-text-013", "novel-text-097")

    def _run(
        self,
        root: Path,
        monkeypatch: pytest.MonkeyPatch,
        workers: int,
        *,
        disagree: bool = False,
        extra: Sequence[str] = (),
    ) -> bytes:
        from mokuro_bunko.ocr import line_reconcile

        input_dir = root / "Vol"
        input_dir.mkdir(parents=True)
        for name in self.PAGES:
            (input_dir / name).write_bytes(b"x")
        by_name = {
            name: _raw_page(self.FIXTURES[i % len(self.FIXTURES)])
            for i, name in enumerate(self.PAGES)
        }
        fake = FakePPOcrModule(by_name[self.PAGES[0]], by_name=by_name)
        # The engine agrees with the CTC read of whatever line the crop names,
        # which is page state the fake recognizer must NOT take off a pooled
        # session: the corner map is built once, from every fixture.
        by_corner = {
            (round(line["quad"][0][0]), round(line["quad"][0][1])): line["text"]
            for raw in by_name.values()
            for line in raw["lines"]
        }

        def read(crop: Any) -> str:
            text = by_corner.get(crop, "")
            # ...unless this run is asked for an engine that MISREADS: half the
            # lines come back with a different last glyph, so the two reads
            # disagree and ``review.json`` has something in it to compare.
            if disagree and len(text) > 2 and crop[1] % 2 == 0:
                return text[:-1] + "々"
            return text

        class _Recognizer:
            token_caps = True
            repos = {"fake/recognizer": "0" * 40}

            def __call__(self, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
                return [read(crop) for crop in crops]

        siblings = {"ppocr": fake, "line_layout": line_layout, "line_reconcile": line_reconcile}
        monkeypatch.setattr(runner, "load_sibling", lambda name: siblings[name])
        monkeypatch.setattr(runner, "load_recognizer", lambda *a, **k: _Recognizer())
        monkeypatch.setattr(runner, "make_quad_crop_fn", lambda *margin: _corner_crops)
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _corner_crops)
        monkeypatch.setattr(runner, "imread_bgr", lambda p: Image(1925, 2800, p.name))
        output = root / "out" / "Vol.paddle-manga.mokuro"
        args = runner.parse_args(
            [
                "--engine", "paddle-manga", "--detector", "ppocr-manga",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(root / "out" / "_ocr" / "paddle-manga" / "Vol"),
                "--title-uuid", "tu", "--volume-uuid", "vu",
                "--title", "T", "--volume", "V",
                "--cpu-workers", str(workers), *extra,
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        return output.read_bytes()

    @staticmethod
    def _detect_dir(root: Path) -> Path:
        return root / "out" / "_detect" / "paddle-manga"

    def test_pooled_detection_writes_the_serial_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        serial = self._run(tmp_path / "serial", monkeypatch, workers=0)
        single = self._run(tmp_path / "single", monkeypatch, workers=1)
        pooled = self._run(tmp_path / "pooled", monkeypatch, workers=3)
        assert single == serial and pooled == serial
        volume = json.loads(serial)
        assert [page["img_path"] for page in volume["pages"]] == list(self.PAGES)
        assert sum(len(page["blocks"]) for page in volume["pages"]) > 0
        # the reads that landed are real text, not a page of empties
        assert any(
            line for page in volume["pages"] for block in page["blocks"] for line in block["lines"]
        )

    def test_every_stage_pooled_separately_writes_the_serial_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Three stages at three different widths, against the one-thread path."""
        serial = self._run(tmp_path / "serial", monkeypatch, workers=0)
        tuned = self._run(
            tmp_path / "tuned",
            monkeypatch,
            workers=2,
            extra=("--stage-workers", "detect=3,post=2", "--queue-capacity", "detect=1,engine=4"),
        )
        assert tuned == serial

    def test_the_review_file_is_the_serial_one_byte_for_byte(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """``review.json`` is an ORDERED list, and the post stage is a pool.

        The entries are appended by the driver from what the stage handed back,
        so a page that finished early cannot jump the queue in the file.
        """
        serial_root, pooled_root = tmp_path / "serial", tmp_path / "pooled"
        self._run(serial_root, monkeypatch, workers=0, disagree=True)
        self._run(pooled_root, monkeypatch, workers=3, disagree=True)
        serial_review = self._detect_dir(serial_root) / runner.REVIEW_FILE
        pooled_review = self._detect_dir(pooled_root) / runner.REVIEW_FILE
        listing = json.loads(serial_review.read_text(encoding="utf-8"))
        assert listing["pages"], "the disagreeing engine must produce review entries"
        assert [entry["page"] for entry in listing["pages"]] == sorted(
            entry["page"] for entry in listing["pages"]
        )
        assert pooled_review.read_bytes() == serial_review.read_bytes()

    def test_the_raw_dumps_are_the_serial_ones_too(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Written from the pooled post stage; one page's dump per page."""
        serial_root, pooled_root = tmp_path / "serial", tmp_path / "pooled"
        self._run(serial_root, monkeypatch, workers=0, disagree=True)
        self._run(pooled_root, monkeypatch, workers=3, disagree=True)
        for name in self.PAGES:
            dump = Path(name).with_suffix(".json").name
            serial_dump = (self._detect_dir(serial_root) / dump).read_bytes()
            assert (self._detect_dir(pooled_root) / dump).read_bytes() == serial_dump

    def test_the_run_publishes_what_its_queues_did(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The live file another process reads, and never under --cache-dir."""
        root = tmp_path / "stats"
        self._run(root, monkeypatch, workers=2)
        stats = json.loads(
            (self._detect_dir(root) / runner.PIPELINE_STATS_FILE).read_text(encoding="utf-8")
        )
        assert stats["items"] == len(self.PAGES)
        assert [s["key"] for s in stats["stages"]] == ["detect", "engine", "post"]
        assert [q["name"] for q in stats["queues"]] == [
            "in->detect",
            "detect->engine",
            "engine->post",
            "post->out",
        ]
        assert stats["bottleneck"] in {"detect", "engine", "post"}
        cache = root / "out" / "_ocr" / "paddle-manga" / "Vol"
        assert len(list(cache.rglob("*.json"))) == len(self.PAGES), (
            "the server counts the JSON under --cache-dir as finished pages"
        )


class TestPipelinedAdapterRun:
    """The adapter road pooled: decode, recognizer, assemble -- and the serial bytes.

    ``--engine hayai-nova --detector ctd``, the detector having already run
    in its own process, so the stages are reading the page off disk, the
    recognizer, and building the blocks.
    """

    PAGES = ("001.webp", "002.webp", "003.webp", "004.webp", "005.webp", "006.webp")

    def _run(
        self, root: Path, monkeypatch: pytest.MonkeyPatch, workers: int, *, extra: Sequence[str] = ()
    ) -> bytes:

        input_dir = root / "Vol"
        input_dir.mkdir(parents=True)
        for name in self.PAGES:
            (input_dir / name).write_bytes(b"x")

        def page_blocks(rel: str) -> list[dict[str, Any]]:
            # a different box per page, so a page out of order shows up
            blocks = _blocks()
            blocks[0]["box"] = [10 + self.PAGES.index(rel), 10, 40, 90]
            return blocks

        class _Recognizer:
            repos = {"some/recognizer": "a" * 40}

            def __call__(self, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
                # the read follows the crop, which names its own block
                return [f"読{crop}" for crop in crops]

        detectors = _FakeDetectors(blocks=page_blocks)
        monkeypatch.setattr(runner, "open_detectors", detectors.open)
        monkeypatch.setattr(runner, "load_recognizer", lambda *a, **k: _Recognizer())
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(100, 200, path.name))
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _one_crop_per_line)
        monkeypatch.setattr(runner, "make_upright_crop_fn", lambda *a, **k: _one_crop_per_line)
        output = root / "out" / "Vol.hayai-nova.mokuro"
        args = runner.parse_args(
            [
                "--engine", "hayai-nova", "--detector", "ctd",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(root / "out" / "_ocr" / "hayai-nova" / "Vol"),
                "--title-uuid", "tu", "--volume-uuid", "vu",
                "--title", "T", "--volume", "V",
                "--cpu-workers", str(workers), *extra,
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        return output.read_bytes()

    def test_every_width_writes_the_same_bytes_as_the_serial_path(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        serial = self._run(tmp_path / "serial", monkeypatch, workers=0)
        single = self._run(tmp_path / "single", monkeypatch, workers=1)
        pooled = self._run(tmp_path / "pooled", monkeypatch, workers=3)
        tuned = self._run(
            tmp_path / "tuned",
            monkeypatch,
            workers=1,
            extra=("--stage-workers", "post=4", "--queue-capacity", "1"),
        )
        assert single == serial and pooled == serial and tuned == serial
        volume = json.loads(serial)
        assert [page["img_path"] for page in volume["pages"]] == list(self.PAGES)
        # every page kept its own detection, not the one another thread held
        boxes = [page["blocks"][0]["box"][0] for page in volume["pages"]]
        assert boxes == [10 + i for i in range(len(self.PAGES))]
        assert all(
            line for page in volume["pages"] for block in page["blocks"] for line in block["lines"]
        )


class TestStageGraphs:
    """Every composed engine declares a graph; the scheduler reads only that."""

    def test_every_engine_reaches_this_runner_on_a_road_of_its_own(self) -> None:
        """Nothing is scheduled outside this file any more.

        ``mokuro`` used to be the exception -- detection and recognition
        behind its own CLI, one invocation a volume, ``processor._run_mokuro``
        instead of a pipeline. It is a SERVE PROCESS now, so it has a road,
        stages and pools like every other engine; what is left on the old
        path is a package with no serve module (the probe decides, per host).
        """
        from mokuro_bunko.ocr import engines

        for engine_id in runner.ENGINE_IDS:
            assert runner.page_road(engine_id, "ctd") in runner.STAGE_GRAPHS
        served = {e for e in engines.ENGINE_IDS if engines.get_engine(e).uses_mokuro_env}
        assert served, "the mokuro engines are still the ones in their own environment"
        for engine_id in served:
            assert runner.page_road(engine_id, "ctd") == runner.ROAD_SERVED

    def test_every_engine_and_detector_pair_lands_on_a_declared_graph(self) -> None:
        for engine in runner.ENGINE_IDS:
            for detector in runner.DETECTOR_SCRIPTS:
                road = runner.page_road(engine, detector)
                assert road in runner.STAGE_GRAPHS

    def test_the_road_follows_the_pair_not_the_engine(self) -> None:
        assert runner.page_road("ppocr-manga", "ctd") == runner.ROAD_LINE  # detector ignored
        assert runner.page_road("paddle-manga", "ppocr-manga") == runner.ROAD_RECONCILED
        assert runner.page_road("paddle-manga", "ctd") == runner.ROAD_ADAPTER
        assert runner.page_road("hayai-nova", "ppocr-manga") == runner.ROAD_RECONCILED
        assert runner.page_road("hayai-nova", "ctd") == runner.ROAD_ADAPTER

    def test_every_stage_of_every_graph_can_be_pooled(self) -> None:
        """No stage is stuck at the end on the driver's thread any more."""
        for road, specs in runner.STAGE_GRAPHS.items():
            assert len(specs) >= 2, road
            assert all(spec.max_workers != runner.SERIAL for spec in specs), road
            assert all(s.device in (runner.DEVICE_CPU, runner.DEVICE_GPU) for s in specs), road

    def test_a_device_bound_stage_has_a_queue_on_both_sides(self) -> None:
        """The whole point of the shape: the GPU stage is never first or last.

        A stage at the end has no output queue, so it can neither be blocked --
        which is the signal that what comes after it is too narrow -- nor give
        its device back when it runs ahead.
        """
        for road, specs in runner.STAGE_GRAPHS.items():
            bound = [i for i, spec in enumerate(specs) if spec.max_workers == runner.DEVICE_BOUND]
            for index in bound:
                assert 0 < index < len(specs) - 1, road
            if bound:
                assert [specs[i].device for i in bound] == [runner.DEVICE_GPU] * len(bound), road

    def test_stage_keys_are_unique_within_a_road(self) -> None:
        """They name a queue, a meter and a ``--stage-workers`` entry."""
        for road, specs in runner.STAGE_GRAPHS.items():
            keys = [spec.key for spec in specs]
            assert len(set(keys)) == len(keys), road
            assert all(key and key.isidentifier() for key in keys), road

    def test_a_graph_with_no_gpu_stage_at_all_is_legal(self) -> None:
        """``ppocr-manga`` is 0% GPU and still has something to overlap."""
        devices = [s.device for s in runner.STAGE_GRAPHS[runner.ROAD_LINE]]
        assert runner.DEVICE_GPU not in devices

    def test_a_graph_needs_exactly_one_callable_a_stage(self) -> None:
        with pytest.raises(RuntimeError, match="declares 2 stages but was given 1"):
            runner.page_stages(
                runner.ROAD_LINE, (lambda i, p: p,), engine="ppocr-manga", budget=2
            )

    def test_every_stage_declares_what_it_costs(self) -> None:
        for road, specs in runner.STAGE_GRAPHS.items():
            assert all(spec.seconds > 0 for spec in specs), road

    def test_the_per_engine_cost_table_names_real_stages(self) -> None:
        declared = {spec.key for specs in runner.STAGE_GRAPHS.values() for spec in specs}
        for engine, costs in runner.ENGINE_STAGE_SECONDS.items():
            assert engine in runner.ENGINE_IDS
            assert set(costs) <= declared, engine

    def test_a_stages_cost_is_the_engines_where_the_engine_has_one(self) -> None:
        spec = runner.STAGE_GRAPHS[runner.ROAD_RECONCILED][1]
        assert spec.key == runner.STAGE_ENGINE
        assert runner.stage_seconds("paddle-manga", spec) == 0.915
        assert runner.stage_seconds("hayai-nova", spec) == 0.177
        # an engine with nothing to say falls back to the road's own figure
        assert runner.stage_seconds("ppocr-manga", spec) == spec.seconds


class TestStageSizing:
    """Stage widths are derived from what a stage costs, relative to each other."""

    def test_a_stage_is_sized_against_the_one_that_sets_the_pace(self) -> None:
        """paddle-manga: 0.225 s of CPU against 0.915 s on the card asks for ONE.

        And one is right, measured: the engine stage is idle 0.02% of the time
        at width 1 on this pair, and widening it made the wall clock WORSE
        (0.7%). The headroom multiplier does not lift it, because half a
        pace's worth of headroom is still under one pace.
        """
        wide = runner.stage_widths("paddle-manga", runner.ROAD_RECONCILED, budget=7)
        assert wide == [1, runner.DEVICE_BOUND, 1]
        # ...and a bigger host does not change that, because the GPU still rules
        assert runner.stage_widths("paddle-manga", runner.ROAD_RECONCILED, budget=64) == wide

    def test_a_cpu_bound_engine_grows_until_it_reaches_the_pace(self) -> None:
        """hayai-nova is 0.225 s of CPU against 0.177 s on the card: it needs THREE.

        Measured engine-stage idle at widths 1/2/3: 25.7%, 5.2%, 0.8%. The
        mean ratio alone asks for 2 (still idle a twentieth of the time); the
        headroom for page-to-page variance is what makes it 3.
        """
        widths = runner.stage_widths("hayai-nova", runner.ROAD_RECONCILED, budget=7)
        assert widths == [3, runner.DEVICE_BOUND, 1]

    def test_the_device_bound_stage_is_never_widened_by_the_derivation(self) -> None:
        for engine in ("paddle-manga", "hayai-nova"):
            for road in (runner.ROAD_RECONCILED, runner.ROAD_ADAPTER):
                specs = runner.STAGE_GRAPHS[road]
                widths = runner.stage_widths(engine, road, budget=64)
                for spec, width in zip(specs, widths, strict=True):
                    if spec.max_workers == runner.DEVICE_BOUND:
                        assert width == 1, (engine, road, spec.key)

    def test_a_graph_with_nothing_to_hide_behind_takes_the_whole_budget(self) -> None:
        """ppocr-manga has no GPU stage, so only the host and the plateau stop it."""
        assert runner.stage_widths("ppocr-manga", runner.ROAD_LINE, budget=7) == [
            runner.CPU_WORKERS_MAX,
            1,
        ]
        assert runner.stage_widths("ppocr-manga", runner.ROAD_LINE, budget=2) == [2, 1]

    def test_the_ratio_between_stages_is_read_off_their_cost(self) -> None:
        specs = (
            runner.StageSpec("slow", "slow", runner.DEVICE_CPU, runner.POOLED, 1.0),
            runner.StageSpec("bound", "bound", runner.DEVICE_GPU, runner.DEVICE_BOUND, 0.25),
            runner.StageSpec("tail", "tail", runner.DEVICE_CPU, runner.POOLED, 0.5),
        )
        # 1.0 / 0.25 = 4 and 0.5 / 0.25 = 2: the pace is the bound stage's, and
        # the two pooled stages come out in the ratio of their own costs --
        # times the headroom, which scales both and so leaves the ratio alone.
        h = runner.STAGE_WIDTH_HEADROOM
        assert runner.plan_stage_workers("x", specs, budget=16, cap=None) == [4 * h, 1, 2 * h]
        faster = (specs[0]._replace(seconds=0.5), specs[1], specs[2])
        assert runner.plan_stage_workers("x", faster, budget=16, cap=None) == [2 * h, 1, 2 * h]

    def test_a_stage_is_not_carved_out_of_one_shared_number(self) -> None:
        """The widths may add up to more than the budget: it is a per-stage ceiling.

        Sizing by share of a global budget is what stops two stages being set
        RELATIVE to one another, which is the thing this derivation exists to
        avoid: an expensive stage and a cheap one next to it both get what they
        need, and the ceiling only says how wide any ONE of them may go.
        """
        specs = (
            runner.StageSpec("a", "a", runner.DEVICE_CPU, runner.POOLED, 1.0),
            runner.StageSpec("b", "b", runner.DEVICE_GPU, runner.DEVICE_BOUND, 0.25),
            runner.StageSpec("c", "c", runner.DEVICE_CPU, runner.POOLED, 1.0),
        )
        plan = runner.plan_stage_workers("x", specs, budget=4, cap=None)
        assert plan == [4, 1, 4]
        assert sum(plan) > 4

    def test_a_budget_of_zero_is_the_serial_fallback(self) -> None:
        for engine, road in (
            ("ppocr-manga", runner.ROAD_LINE),
            ("paddle-manga", runner.ROAD_RECONCILED),
            ("hayai-nova", runner.ROAD_ADAPTER),
        ):
            widths = runner.stage_widths(engine, road, budget=0)
            assert widths == [runner.SERIAL] * len(runner.STAGE_GRAPHS[road])

    def test_every_composed_engine_gets_a_plan_on_every_road(self) -> None:
        for engine in runner.ENGINE_IDS:
            for detector in runner.DETECTOR_SCRIPTS:
                road = runner.page_road(engine, detector)
                widths = runner.stage_widths(engine, road, budget=7)
                assert len(widths) == len(runner.STAGE_GRAPHS[road])
                assert all(width >= 1 for width in widths)

    def test_an_explicit_width_beats_the_derivation_and_the_plateau(self) -> None:
        forced = runner.stage_widths("paddle-manga", runner.ROAD_RECONCILED, budget=1, forced=9)
        # past the budget AND past the cap, but never past the GPU stage's one
        assert forced == [9, runner.DEVICE_BOUND, 9]
        # ...and a bare number does not reach a stage holding a model on the
        # card at all: --cpu-workers is a number of CPU workers.
        on_gpu = runner.road_specs(runner.ROAD_ADAPTER, detector="ctd", gpu=True)
        assert runner.stage_widths(
            "hayai-nova", runner.ROAD_ADAPTER, budget=8, forced=9, specs=on_gpu
        ) == [1, runner.DEVICE_BOUND, 9]
        # naming it is unambiguous, and does widen it
        assert runner.stage_widths(
            "hayai-nova", runner.ROAD_ADAPTER, budget=8, workers={"detect": 2}, specs=on_gpu
        )[0] == 2
        # on a host with no card the same stage is an ordinary CPU pool
        on_cpu = runner.road_specs(runner.ROAD_ADAPTER, detector="ctd", gpu=False)
        assert runner.stage_widths(
            "hayai-nova", runner.ROAD_ADAPTER, budget=8, forced=9, specs=on_cpu
        )[0] == 9
        assert runner.stage_widths("paddle-manga", runner.ROAD_RECONCILED, budget=7, forced=0) == [
            0,
            0,
            0,
        ]

    def test_one_stage_can_be_set_without_touching_the_others(self) -> None:
        """The point of per-stage pools: widen the starved one, leave the rest."""
        derived = runner.stage_widths("paddle-manga", runner.ROAD_RECONCILED, budget=7)
        tuned = runner.stage_widths(
            "paddle-manga", runner.ROAD_RECONCILED, budget=7, workers={"detect": 6}
        )
        assert tuned[0] == 6
        assert tuned[1:] == derived[1:]

    def test_a_named_stage_beats_a_bare_number_which_beats_the_derivation(self) -> None:
        road, engine = runner.ROAD_RECONCILED, "paddle-manga"
        assert runner.stage_widths(engine, road, budget=7, workers={"*": 3}) == [3, 1, 3]
        assert runner.stage_widths(
            engine, road, budget=7, workers={"*": 3, "post": 5}
        ) == [3, 1, 5]
        # and the bare number in the map beats an older --cpu-workers too
        assert runner.stage_widths(engine, road, budget=7, forced=2, workers={"*": 4}) == [
            4,
            1,
            4,
        ]

    def test_queue_capacity_defaults_to_the_width_of_who_fills_it(self) -> None:
        widths = [4, 1, 2]
        assert runner.stage_capacities(runner.ROAD_RECONCILED, widths) == [4, 1, 2]

    def test_queue_capacity_can_be_set_per_queue(self) -> None:
        widths = [4, 1, 2]
        assert runner.stage_capacities(
            runner.ROAD_RECONCILED, widths, capacities={"engine": 8}
        ) == [4, 8, 2]
        assert runner.stage_capacities(runner.ROAD_RECONCILED, widths, capacities={"*": 3}) == [
            3,
            3,
            3,
        ]
        # a named queue beats the bare number
        assert runner.stage_capacities(
            runner.ROAD_RECONCILED, widths, capacities={"*": 3, "detect": 9}
        ) == [9, 3, 3]

    def test_a_queue_always_has_room_for_at_least_one_page(self) -> None:
        """A capacity of zero is a deadlock, not a tuning choice."""
        # ...and the queue feeding the device-bound stage keeps its load window
        assert runner.stage_capacities(runner.ROAD_RECONCILED, [0, 0, 0]) == [
            runner.LOAD_WINDOW_SLOTS,
            1,
            1,
        ]
        assert runner.stage_capacities(
            runner.ROAD_RECONCILED, [2, 1, 2], capacities={"*": 0}
        ) == [1, 1, 1]

    def test_page_stages_carries_the_widths_and_the_capacities(self) -> None:
        runs = (lambda i, p: p, lambda i, p: p, lambda i, p: p)
        stages = runner.page_stages(
            runner.ROAD_RECONCILED,
            runs,
            engine="paddle-manga",
            budget=7,
            workers={"detect": 5},
            capacities={"engine": 6},
        )
        assert [s.spec.key for s in stages] == ["detect", "engine", "post"]
        assert [s.workers for s in stages] == [5, 1, 1]
        assert [s.capacity for s in stages] == [5, 6, 1]


class TestStageSettingParsing:
    """``--stage-workers`` / ``--queue-capacity``: what they take and what they refuse."""

    def test_a_bare_number_is_every_stage(self) -> None:
        assert runner.parse_stage_setting("3") == {runner.ALL_STAGES: 3}
        assert runner.parse_stage_setting("  3 ") == {runner.ALL_STAGES: 3}

    def test_named_stages_parse_in_any_spacing(self) -> None:
        assert runner.parse_stage_setting("detect=4,post=2") == {"detect": 4, "post": 2}
        assert runner.parse_stage_setting(" detect = 4 , engine = 1 ") == {
            "detect": 4,
            "engine": 1,
        }

    def test_nothing_is_an_empty_setting_not_a_zero(self) -> None:
        for empty in (None, "", "   "):
            assert runner.parse_stage_setting(empty) == {}

    def test_a_typo_is_refused_rather_than_ignored(self) -> None:
        with pytest.raises(ValueError, match="unknown stage 'detct'"):
            runner.parse_stage_setting("detct=4")
        with pytest.raises(ValueError, match="not a stage setting"):
            runner.parse_stage_setting("detect=lots")
        with pytest.raises(ValueError, match="cannot be negative"):
            runner.parse_stage_setting("detect=-2")

    def test_the_flag_beats_the_environment(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv(runner.STAGE_WORKERS_ENV, "detect=8")
        assert runner.resolve_stage_setting("post=2", runner.STAGE_WORKERS_ENV) == {"post": 2}
        assert runner.resolve_stage_setting(None, runner.STAGE_WORKERS_ENV) == {"detect": 8}
        monkeypatch.delenv(runner.STAGE_WORKERS_ENV)
        assert runner.resolve_stage_setting(None, runner.STAGE_WORKERS_ENV) == {}

    def test_the_cli_takes_both_and_defaults_to_nothing(self) -> None:
        base = ["--engine", "ppocr-manga", "--input", "i", "--output", "o", "--cache-dir", "c"]
        args = runner.parse_args(base)
        assert args.stage_workers is None and args.queue_capacity is None
        assert args.stats_file is None
        args = runner.parse_args(
            [*base, "--stage-workers", "detect=4", "--queue-capacity", "2", "--stats-file", "s"]
        )
        assert args.stage_workers == "detect=4"
        assert args.queue_capacity == "2"
        assert args.stats_file == "s"


class TestHostBudget:
    """How much of the host one run may claim, before any stage asks."""

    def test_cores_less_the_driver_split_between_jobs_over_session_threads(self) -> None:
        assert runner.host_worker_budget(32, jobs=1) == 7
        assert runner.host_worker_budget(32, jobs=2) == 3
        assert runner.host_worker_budget(32, jobs=4) == 1
        assert runner.host_worker_budget(32, jobs=8) == 1

    def test_raising_job_concurrency_shrinks_every_runs_claim(self) -> None:
        budgets = [runner.host_worker_budget(64, jobs=j) for j in (1, 2, 4, 8)]
        assert budgets == sorted(budgets, reverse=True)
        assert all(b >= 1 for b in budgets)

    def test_a_small_host_still_gets_one_worker(self) -> None:
        for cpus in (1, 2, 4, 8):
            assert runner.host_worker_budget(cpus, jobs=4) == 1

    def test_the_jobs_environment_is_the_contract_with_the_scheduler(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # PHYSICAL cores, not os.cpu_count(): SMT siblings buy nothing here.
        monkeypatch.setattr(runner, "physical_cpu_count", lambda: 32)
        monkeypatch.setenv(runner.CPU_JOBS_ENV, "4")
        assert runner.host_worker_budget() == 1
        monkeypatch.delenv(runner.CPU_JOBS_ENV)
        assert runner.host_worker_budget() == 7  # the shipped ocr.concurrency default of 1

    def test_the_budget_counts_physical_cores_not_smt_siblings(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A 16c/32t host must budget for 16, or every pool doubles for nothing."""
        monkeypatch.delenv(runner.CPU_JOBS_ENV, raising=False)
        monkeypatch.setattr(runner, "physical_cpu_count", lambda: 16)
        sixteen = runner.host_worker_budget()
        monkeypatch.setattr(runner, "physical_cpu_count", lambda: 32)
        assert runner.host_worker_budget() > sixteen

    def test_physical_cpu_count_falls_back_to_the_logical_count(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Unreadable sysfs (containers, non-Linux) must not be fatal."""

        def boom(*_args: Any, **_kwargs: Any) -> Any:
            raise OSError("no sysfs here")

        monkeypatch.setattr(runner.Path, "glob", boom)
        monkeypatch.setattr(runner.os, "cpu_count", lambda: 12)
        assert runner.physical_cpu_count() == 12

    def test_physical_cpu_count_never_exceeds_the_logical_count(self) -> None:
        """Whatever sysfs says on the real host, it cannot exceed the logical count."""
        assert 1 <= runner.physical_cpu_count() <= (runner.os.cpu_count() or 4)


# ---------------------------------------------------------------------------
# The detector, as a streaming stage
# ---------------------------------------------------------------------------

#: A detector adapter in serve mode, built on the REAL scaffold, so these
#: tests pin the protocol from both ends. ``MOKURO_FAKE_DETECTOR`` chooses
#: what it does with a page: answer, fail it, hang, or die.
FAKE_ADAPTER = '''
import os, sys, time
from pathlib import Path
sys.path.insert(0, {src!r})
from mokuro_bunko.ocr.detectors._common import AdapterError, run_adapter

MODE = os.environ.get("MOKURO_FAKE_DETECTOR", "ok")
SEEN = Path(os.environ["MOKURO_FAKE_SEEN"])


def setup(args, out):
    if MODE == "refuse":
        raise AdapterError("this detector refuses to load")
    out.weights({{"fake/detector": "f" * 40}})
    print("[detector:fake] loaded", flush=True)

    def detect(page):
        with SEEN.open("a", encoding="utf-8") as fh:
            fh.write(page.name + "\\n")
        if MODE == "fail-002" and page.name.startswith("002"):
            raise ValueError("cannot read this page")
        if MODE == "die-002" and page.name.startswith("002"):
            os._exit(9)
        if MODE == "hang-002" and page.name.startswith("002"):
            time.sleep(600)
        return {{"img_width": 100, "img_height": 200, "blocks": []}}, "blocks=0"

    detect.device = "cpu"
    return detect


sys.exit(run_adapter("fake", "fake detector", setup))
'''


def _fake_adapter(tmp_path: Path) -> Path:
    src = str(Path(runner.__file__).resolve().parents[3])
    script = tmp_path / "fake_adapter.py"
    script.write_text(FAKE_ADAPTER.format(src=src), encoding="utf-8")
    return script


class TestDetectorProcess:
    """One adapter subprocess, spoken to one page at a time."""

    def _pool(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, mode: str, **kwargs: Any
    ) -> Any:
        monkeypatch.setenv("MOKURO_FAKE_DETECTOR", mode)
        monkeypatch.setenv("MOKURO_FAKE_SEEN", str(tmp_path / "seen.txt"))
        (tmp_path / "Vol").mkdir(exist_ok=True)
        return runner.DetectorPool("fake", 1, script=_fake_adapter(tmp_path), **kwargs)

    @staticmethod
    def _ask(pool: Any, tmp_path: Path, name: str) -> str:
        """One page, the way the detect stage asks: two absolute paths."""
        image = tmp_path / "Vol" / name
        image.parent.mkdir(parents=True, exist_ok=True)
        return pool.detect(image, (tmp_path / "detect" / name).with_suffix(".json"))

    def test_a_page_at_a_time_and_the_json_lands_where_the_contract_says(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Nothing is detected until it is asked for: that is the whole point."""
        pool = self._pool(tmp_path, monkeypatch, "ok")
        try:
            assert self._ask(pool, tmp_path, "001.webp") == "blocks=0"
            assert (tmp_path / "detect" / "001.json").is_file()
            # ...and page 2 was not touched while page 1 was being asked for
            assert not (tmp_path / "detect" / "002.json").exists()
            self._ask(pool, tmp_path, "sub/002.webp")
            assert (tmp_path / "detect" / "sub" / "002.json").is_file()
            assert pool.devices == ["cpu"]
            # what it loaded came back on the ready line, not from a file...
            assert pool.weights == {"fake/detector": "f" * 40}
        finally:
            pool.close()
        # ...and it survives the teardown, because the sidecar naming those
        # weights is written after the detectors have been shut down.
        assert pool.weights == {"fake/detector": "f" * 40}

    def test_a_page_name_with_spaces_is_not_a_protocol_desync(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """THE regression: real page names have spaces, and the reply is split.

        ``ok <page> <note>`` split on spaces turned "Some Series 20 -
        101.webp" into "Some", read as an answer for the wrong page, and
        killed a perfectly healthy detector -- once a page, until the respawns
        ran out and the rest of the volume went blank.
        """
        pool = self._pool(tmp_path, monkeypatch, "ok")
        try:
            for name in ("Some Series 20 - 101.webp", "a b/c d  e.webp", "ordinary.webp"):
                assert self._ask(pool, tmp_path, name) == "blocks=0"
                assert (tmp_path / "detect" / name).with_suffix(".json").is_file()
            # one process, never retired
            assert len(pool.devices) == 1
        finally:
            pool.close()

    def test_a_failed_page_is_a_failed_page_and_the_process_lives(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        pool = self._pool(tmp_path, monkeypatch, "fail-002")
        try:
            self._ask(pool, tmp_path, "001.webp")
            with pytest.raises(runner.DetectorPageError, match="cannot read this page"):
                self._ask(pool, tmp_path, "002.webp")
            # the same process answers the next page: one bad page, one cost
            assert self._ask(pool, tmp_path, "003.webp") == "blocks=0"
            assert len(pool.devices) == 1
        finally:
            pool.close()

    def test_a_process_that_dies_costs_its_page_and_is_replaced(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """EOF on its stdout fails THAT page; the next one gets a fresh process."""
        pool = self._pool(tmp_path, monkeypatch, "die-002")
        try:
            self._ask(pool, tmp_path, "001.webp")
            with pytest.raises(runner.DetectorError, match="died"):
                self._ask(pool, tmp_path, "002.webp")
            assert self._ask(pool, tmp_path, "003.webp") == "blocks=0"
        finally:
            pool.close()

    def test_respawns_are_bounded_and_then_every_page_fails_fast(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A crash loop must not become an hour of reloading a broken model."""
        pool = self._pool(tmp_path, monkeypatch, "die-002", respawns=1)
        try:
            for _ in range(2):
                with pytest.raises(runner.DetectorError):
                    self._ask(pool, tmp_path, "002.webp")
            started = time.monotonic()
            with pytest.raises(runner.DetectorError, match="no fake detector process left"):
                self._ask(pool, tmp_path, "003.webp")
            assert time.monotonic() - started < 5.0  # fails fast, never hangs
        finally:
            pool.close()

    def test_a_wedged_process_times_out_rather_than_hanging_the_volume(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        pool = self._pool(tmp_path, monkeypatch, "hang-002", page_timeout=1.0)
        try:
            with pytest.raises(runner.DetectorError, match="did not answer"):
                self._ask(pool, tmp_path, "002.webp")
            # and the wedged process was killed, not left holding the card
            assert self._ask(pool, tmp_path, "003.webp") == "blocks=0"
        finally:
            pool.close()

    def test_an_adapter_that_refuses_to_load_is_an_error_not_a_hang(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        pool = self._pool(tmp_path, monkeypatch, "refuse")
        try:
            with pytest.raises(runner.DetectorError, match="exited before it was ready"):
                self._ask(pool, tmp_path, "001.webp")
        finally:
            pool.close()

    def test_close_leaves_no_child_behind(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        pool = self._pool(tmp_path, monkeypatch, "ok")
        self._ask(pool, tmp_path, "001.webp")
        children = [m.proc for m in pool._live]  # noqa: SLF001 - the point of the test
        assert children and all(c is not None and c.poll() is None for c in children)
        pool.close()
        assert all(c.poll() is not None for c in children if c is not None)
        # idempotent, and a second close is not a second teardown
        pool.close()

    def test_a_pool_of_two_is_two_processes(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setenv("MOKURO_FAKE_DETECTOR", "ok")
        monkeypatch.setenv("MOKURO_FAKE_SEEN", str(tmp_path / "seen.txt"))
        (tmp_path / "Vol").mkdir()
        pool = runner.DetectorPool("fake", 2, script=_fake_adapter(tmp_path))
        try:
            for name in ("001.webp", "002.webp", "003.webp"):
                self._ask(pool, tmp_path, name)
            assert len(pool.devices) == 2
            pids = {m.proc.pid for m in pool._live if m.proc}  # noqa: SLF001
            assert len(pids) == 2
        finally:
            pool.close()


class TestAdapterServeMode:
    """Batch and serve are the same code path, so they write the same bytes."""

    def test_serve_writes_what_batch_writes(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
    ) -> None:
        from mokuro_bunko.ocr.detectors import _common

        calls: list[str] = []

        def setup(_args: Any, out: Any) -> Any:
            out.weights({"some/detector": "d" * 40})

            def detect(page: Path) -> tuple[dict[str, Any], str]:
                calls.append(page.name)
                return {"img_width": 7, "img_height": 9, "blocks": []}, "blocks=0"

            return detect

        pages = tmp_path / "pages.txt"
        pages.write_text("001.webp\n002.webp\n", encoding="utf-8")
        batch_out = tmp_path / "batch"
        assert (
            _common.run_adapter(
                "fake",
                "fake",
                setup,
                ["--input", str(tmp_path), "--pages", str(pages), "--output-dir", str(batch_out)],
            )
            == 0
        )
        # Serve mode is told each page and each destination: no volume.
        serve_out = tmp_path / "serve"
        sep = _common.SERVE_SEP
        requests = "".join(
            f"{tmp_path / name}{sep}{(serve_out / name).with_suffix('.json')}\n"
            for name in ("001.webp", "002.webp")
        )
        monkeypatch.setattr(sys, "stdin", io.StringIO(requests))
        assert _common.run_adapter("fake", "fake", setup, ["--serve"]) == 0
        for name in ("001.json", "002.json"):
            assert (serve_out / name).read_bytes() == (batch_out / name).read_bytes()
        assert calls == ["001.webp", "002.webp", "001.webp", "002.webp"]
        # ...and the weights come back on the ready line instead of a file
        ready = next(
            line for line in capsys.readouterr().out.splitlines()
            if line.startswith(f"{_common.SERVE_PREFIX}{_common.SERVE_READY}")
        )  # fmt: skip
        assert json.loads(ready.split(sep, 1)[1]) == {"some/detector": "d" * 40}
        assert not (serve_out / _common.WEIGHTS_FILE).exists()

    def test_a_run_that_is_neither_batch_nor_serve_is_refused(self) -> None:
        from mokuro_bunko.ocr.detectors import _common

        with pytest.raises(SystemExit):
            _common.parse_adapter_args("fake", ["--input", "/x", "--output-dir", "/y"])
        # ...and serve mode needs neither of them
        assert _common.parse_adapter_args("fake", ["--serve"]).serve is True


class TestDeferredRecognizerLoad:
    """The recognizer loads on a thread; the pipeline does not wait for it."""

    def test_the_load_runs_while_the_caller_gets_on_with_it(self) -> None:
        gate = threading.Event()

        class _Recognizer:
            repos = {"some/recognizer": "a" * 40}

            def __call__(self, crops: list[Any], max_tokens: Any = None) -> list[str]:
                return ["読"] * len(crops)

        def load() -> Any:
            gate.wait(5.0)
            return _Recognizer()

        loader = runner.DeferredRecognizer("paddle-manga", load)
        # ...it is usable as a recognizer before the model exists
        assert loader.token_caps is True
        assert loader.repos == {} and not loader.loaded
        gate.set()
        assert loader(["a", "b"]) == ["読", "読"]
        assert loader.weights() == {"some/recognizer": "a" * 40}
        assert loader.loaded

    def test_token_caps_is_known_before_the_model_is(self) -> None:
        held = threading.Event()
        try:
            hayai = runner.DeferredRecognizer("hayai-nova", held.wait)
            assert hayai.token_caps is False
            assert runner.DeferredRecognizer("paddle-manga", held.wait).token_caps is True
        finally:
            held.set()

    def test_a_failed_load_is_raised_in_the_caller_not_swallowed(self) -> None:
        def boom() -> Any:
            raise RuntimeError("no such model")

        loader = runner.DeferredRecognizer("paddle-manga", boom)
        with pytest.raises(RuntimeError, match="no such model"):
            loader(["a"])
        assert isinstance(loader.error, RuntimeError)

    def test_a_failed_load_ends_the_run_with_the_real_error(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
    ) -> None:
        """Not a volume of blank pages: the first page says why and stops."""
        input_dir = tmp_path / "Vol"
        input_dir.mkdir()
        names = [f"{i:03d}.webp" for i in range(1, 31)]
        for name in names:
            (input_dir / name).write_bytes(b"x")

        def boom(*_a: Any, **_k: Any) -> Any:
            raise RuntimeError("CUDA out of memory while loading the LoRA")

        detectors = _FakeDetectors()
        monkeypatch.setattr(runner, "open_detectors", detectors.open)
        monkeypatch.setattr(runner, "load_recognizer", boom)
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(100, 200))
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _one_crop_per_line)
        monkeypatch.setattr(runner, "make_upright_crop_fn", lambda *a, **k: _one_crop_per_line)
        output = tmp_path / "out" / "Vol.hayai-nova.mokuro"
        args = runner.parse_args(
            [
                "--engine", "hayai-nova", "--detector", "ctd",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(tmp_path / "out" / "_ocr" / "hayai-nova" / "Vol"),
            ]
        )  # fmt: skip
        assert runner.run(args) == 1
        out = capsys.readouterr().out
        assert "CUDA out of memory while loading the LoRA" in out
        assert not output.exists()
        # it stopped at the first page to reach the engine rather than
        # detecting the whole volume and blanking every page of it
        assert len(detectors.asked) < len(names)
        assert detectors.closed


class TestRoadSpecsResolveWhatTheRunHolds:
    """A graph is what a road IS; what sits on each stage is per run."""

    def test_a_torch_detector_on_a_card_is_a_gpu_stage_derived_to_one(self) -> None:
        specs = runner.road_specs(runner.ROAD_ADAPTER, detector="ctd", gpu=True)
        assert specs[0].key == runner.STAGE_DETECT
        assert runner.device_is_gpu(specs[0].device) and specs[0].device == "gpu:0"
        widths = runner.stage_widths(
            "paddle-manga", runner.ROAD_ADAPTER, budget=16, specs=specs
        )
        assert widths[0] == 1, "a second detector process is a second model in VRAM"

    def test_a_cpu_detector_is_a_cpu_stage_and_may_be_pooled(self) -> None:
        specs = runner.road_specs(runner.ROAD_ADAPTER, detector="ppocr-manga", gpu=True)
        assert specs[0].device == runner.DEVICE_CPU
        assert runner.stage_widths(
            "hayai-nova", runner.ROAD_ADAPTER, budget=16, specs=specs
        )[0] > 1
        # ...and so is any detector on a host with no card at all
        on_cpu = runner.road_specs(runner.ROAD_ADAPTER, detector="ctd", gpu=False)
        assert on_cpu[0].device == runner.DEVICE_CPU

    def test_no_stage_but_the_engine_ever_holds_a_model(self) -> None:
        """``post`` is layout, assembly and JSON on every road, and CPU on each.

        It is not a knob that can put it on the card: nothing the runner does
        after the recognizer loads a model of its own. The detect stage is the
        only other stage that can be a GPU one, and only because the detector
        it holds is.
        """
        for road in runner.STAGE_GRAPHS:
            for detector in ("", "ctd", "animetext", "ppocr-manga"):
                for gpu in (True, False, None):
                    specs = runner.road_specs(road, detector=detector, gpu=gpu)
                    for spec in specs:
                        if spec.key in (runner.STAGE_POST, runner.STAGE_LAYOUT):
                            assert spec.device == runner.DEVICE_CPU, (road, detector, gpu)
        # ...and the adapter road's post is the cost of assembling dicts, which
        # is what is left of it: measured at 0.04 ms a page over 24 real pages.
        post = runner.road_specs(runner.ROAD_ADAPTER, detector="ctd", gpu=True)[-1]
        assert post.key == runner.STAGE_POST and post.seconds < 0.001


class TestStructuralCeilingsSurviveFusing:
    """`--stage-workers engine=0` must not put four threads in one model."""

    def _stage(self, key: str, *, workers: int, ceiling: int | None) -> Any:
        spec = runner.StageSpec(key, key, runner.DEVICE_CPU, ceiling, 0.1)
        return runner.Stage(spec, lambda item, payload: payload, workers, 1)

    def test_a_device_bound_stage_never_fuses_into_a_wider_pool(self) -> None:
        segments = runner.fuse_stages(
            [
                self._stage("detect", workers=4, ceiling=None),
                self._stage("engine", workers=runner.SERIAL, ceiling=runner.DEVICE_BOUND),
                self._stage("post", workers=2, ceiling=None),
            ]
        )
        assert [seg.key for seg in segments] == ["detect", "engine", "post"]
        assert [seg.workers for seg in segments] == [4, 1, 2]

    def test_it_still_fuses_where_the_ceiling_allows_it(self) -> None:
        """Fusing into a pool of ONE keeps exactly one caller in the model."""
        segments = runner.fuse_stages(
            [
                self._stage("detect", workers=1, ceiling=None),
                self._stage("engine", workers=runner.SERIAL, ceiling=runner.DEVICE_BOUND),
                self._stage("post", workers=2, ceiling=None),
            ]
        )
        assert [seg.key for seg in segments] == ["detect", "post"]
        assert [[s.spec.key for s in seg.stages] for seg in segments] == [
            ["detect", "engine"],
            ["post"],
        ]

    def test_no_width_can_put_two_pages_in_one_device_bound_stage(self) -> None:
        """The property, end to end: however it is asked for, the count is 1."""
        inside = []
        lock = threading.Lock()
        peak = 0

        def count(_item: Any, payload: Any) -> Any:
            nonlocal peak
            with lock:
                inside.append(1)
                peak = max(peak, len(inside))
            time.sleep(0.005)
            with lock:
                inside.pop()
            return payload

        for engine_width in (0, 1, 4):
            bound = runner.StageSpec("engine", "engine", runner.DEVICE_GPU, runner.DEVICE_BOUND, 1)
            stages = [
                runner.Stage(
                    runner.StageSpec("detect", "detect", runner.DEVICE_CPU, runner.POOLED, 1),
                    lambda item, payload: payload,
                    4,
                    4,
                ),
                runner.Stage(bound, count, engine_width, 4),
            ]
            pipeline = runner.StagePipeline(stages)
            assert len(list(pipeline.run(range(40)))) == 40
            pipeline.close()
        assert peak == 1, f"{peak} concurrent callers in a stage declared max_workers=1"


class TestReaderStateIsNotSharedAcrossPooledPages:
    def test_the_pooled_stages_leave_the_reader_alone(self) -> None:
        """``finish``/``finish_read`` run on a POOL: they must write nothing."""
        fake = FakePPOcrModule(_raw_page("manga-page-069"))
        reader = runner.PPOcrPageReader(ppocr=fake, layout=line_layout)
        detected = reader.detect_page(Image(1115, 1600))
        result = reader.layout_detected(detected, "0.2.5")
        assert result.raw and reader.raw == {} and reader.ruby_count == 0
        # the single-page caller still gets its debug state
        reader(Image(1115, 1600), "0.2.5")
        assert reader.raw and reader.ruby_count == len(reader.raw["ruby"])


class TestThePipelineIsNotTiedToOneVolume:
    """What a pipeline that outlives a volume needs, pinned while it is cheap.

    The next step is one runner process per generation, with ONE pipeline that
    stays open while the pages of successive volumes stream through it. These
    are the properties that would have to be true then and are cheap to hold
    now, so a later change is a new entry point rather than a rewrite.
    """

    def test_a_page_carries_its_own_volume(self, tmp_path: Path) -> None:
        a = runner.VolumePaths(tmp_path / "A", tmp_path / "dA", tmp_path / "cA")
        b = runner.VolumePaths(tmp_path / "B", tmp_path / "dB", tmp_path / "cB")
        first = runner.PageJob(a, Path("sub/001.webp"))
        second = runner.PageJob(b, Path("sub/001.webp"))
        assert first.image == tmp_path / "A" / "sub/001.webp"
        assert second.image == tmp_path / "B" / "sub/001.webp"
        assert first.detection == tmp_path / "dA" / "sub/001.json"
        assert second.cache == tmp_path / "cB" / "sub/001.json"
        # two volumes' page 1 are different items, so one pipeline can hold both
        assert first != second and str(first) == str(second) == "sub/001.webp"

    def test_a_detector_process_is_told_the_page_and_the_destination(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Not a volume directory and a relative path: a process outlives a volume."""
        monkeypatch.setenv("MOKURO_FAKE_DETECTOR", "ok")
        monkeypatch.setenv("MOKURO_FAKE_SEEN", str(tmp_path / "seen.txt"))
        pool = runner.DetectorPool("fake", 1, script=_fake_adapter(tmp_path))
        try:
            for volume in ("A", "B"):
                image = tmp_path / volume / "001.webp"
                image.parent.mkdir(parents=True, exist_ok=True)
                out = tmp_path / f"detect-{volume}" / "001.json"
                assert pool.detect(image, out) == "blocks=0"
                assert out.is_file()
            # one process served both volumes, and never saw a volume root
            assert len(pool.devices) == 1
            seen = (tmp_path / "seen.txt").read_text(encoding="utf-8").split()
            assert seen == ["001.webp", "001.webp"]
        finally:
            pool.close()

    def test_the_counters_subtract(self) -> None:
        """A per-volume figure is the delta between two snapshots of a live run."""
        stage = runner.Stage(
            runner.StageSpec("detect", "d", runner.DEVICE_CPU, runner.POOLED, 1),
            lambda item, payload: time.sleep(0.002) or payload,
            1,
            2,
        )
        pipeline = runner.StagePipeline([stage])
        stream = pipeline.run(range(30))
        for _ in range(10):
            next(stream)
        first = pipeline.report()
        for _ in range(10):
            next(stream)
        second = pipeline.report()
        # every counter is cumulative from the start, so it differences
        assert second.stages[0].items - first.stages[0].items == 10
        assert second.stages[0].busy_seconds > first.stages[0].busy_seconds
        queue = {q.name: q for q in second.queues}["in->detect"]
        before = {q.name: q for q in first.queues}["in->detect"]
        assert queue.puts >= before.puts and queue.gets - before.gets == 10
        # ...including the depth integral the mean is derived from
        assert queue.depth_seconds >= before.depth_seconds
        stream.close()
        pipeline.close()


# ---------------------------------------------------------------------------
# ADDENDUM 9: a benchmark is timed by the instants its per-page results LEAVE
# the pipeline, and by nothing else.
#
# The window these tests pin down replaces one that collapsed. On unmodified
# HEAD (98ed1d0) `BenchRun._pass` divided the post-fill page COUNT by
# `last - filled`, where `filled` was the instant of the fill-th emission. A
# pool wide enough to hold the whole sample in flight emits it in one burst,
# `filled` then lands INSIDE that burst, and the divisor is microseconds: the
# device agent measured 217052 pages/s on an 8-page widen trial and ~4x
# inflation on 24 pages -- and every accept/reject the tuner had made was made
# on that number.
# ---------------------------------------------------------------------------


class _ScriptedStream:
    """Emits its items at scripted offsets (seconds) from the feed's start."""

    def __init__(self, jobs: Sequence[Any], offsets: Sequence[float]) -> None:
        self.jobs = list(jobs)
        self.offsets = list(offsets)
        self.closed = False

    def __iter__(self) -> Any:
        start = time.monotonic()
        for index, job in enumerate(self.jobs):
            due = start + self.offsets[min(index, len(self.offsets) - 1)]
            while True:
                left = due - time.monotonic()
                if left <= 0:
                    break
                time.sleep(min(left, 0.002))
            yield job, runner.Outcome(value=job, error=None)

    def close(self) -> None:
        self.closed = True


class _ScriptedPipeline:
    """One feed. ``script(widths, count)`` says when each of its pages lands."""

    def __init__(self, script: Any, widths: Sequence[int], seen: list[int]) -> None:
        self.script = script
        self.widths = tuple(int(w) for w in widths)
        self.seen = seen
        self.offsets: list[float] = [0.0]
        self.pages = 0

    def run(self, jobs: Sequence[Any]) -> _ScriptedStream:
        self.pages = len(jobs)
        self.seen.append(len(jobs))
        self.offsets = list(self.script(self.widths, len(jobs)))
        return _ScriptedStream(jobs, self.offsets)

    def report(self) -> runner.PipelineReport:
        elapsed = max(self.offsets) if self.offsets else 0.0
        return runner.PipelineReport(
            elapsed=elapsed,
            items=self.pages,
            stages=(
                runner.StageReport(
                    key="detect",
                    name="detect",
                    device="cpu",
                    workers=1,
                    items=self.pages,
                    busy_seconds=elapsed * 0.9,
                    blocked_seconds=0.0,
                    starved_seconds=0.0,
                    utilisation=0.9,
                ),
            ),
            queues=(
                runner.QueueReport(
                    name="in->detect",
                    capacity=1,
                    depth=0,
                    max_depth=1,
                    mean_depth=0.2,
                    depth_seconds=elapsed * 0.2,
                    puts=self.pages,
                    gets=self.pages,
                    blocked_seconds=0.0,
                    blocked_events=0,
                    starved_seconds=0.0,
                    starved_events=0,
                ),
            ),
        )


class _ScriptedPipe:
    """Everything ``BenchRun`` touches on an ``OpenPipeline``, and no more.

    ``script(widths, count)`` returns the emission offsets a feed of
    ``count`` pages produces at those widths, so a test can say "at detect
    x2 the whole feed lands in one burst" or "this road only emits when the
    feed ends". ``feeds`` records the SIZE of every feed the pipeline was
    given, which is how a test tells "one long feed" from "several short
    ones".
    """

    host_budget = 4

    def __init__(self, script: Any, widths: Sequence[int] = (1,)) -> None:
        self.script = script
        self.widths = list(widths)
        self.specs = (runner.StageSpec("detect", "detect", runner.DEVICE_CPU, runner.POOLED, 4),)
        self.caps = [1]
        self.at_widths: list[tuple[int, ...]] = []
        self.feeds: list[int] = []

    def rebuild(self, widths: Sequence[int]) -> _ScriptedPipeline:
        self.widths = list(widths)
        self.at_widths.append(tuple(int(w) for w in widths))
        return _ScriptedPipeline(self.script, self.widths, self.feeds)

    def stage_device(self) -> dict[str, str]:
        return {"detect": "cpu"}

    def detect_devices(self) -> list[str]:
        return []

    def _stage_device(self, key: str) -> str:
        return "cpu"


def _continuous(per_page: float) -> Any:
    """A road that emits as it works: page n lands after n x per_page."""

    def script(_widths: tuple[int, ...], count: int) -> list[float]:
        return [per_page * (n + 1) for n in range(count)]

    return script


def _served_burst(per_page: float, burst: float = 0.1) -> Any:
    """A road that emits NOTHING until the feed ends, then all of it at once.

    The shape a served engine has on a short feed (ADDENDUM 8): its last OCR
    batch waits for the end of the volume, so a 24-page sample did 2.7 s of
    work and then emitted 24 results inside 0.1 s. The old bench read that as
    72.99 pages/s.
    """

    def script(_widths: tuple[int, ...], count: int) -> list[float]:
        work = per_page * count
        step = burst / max(1, count)
        return [work + n * step for n in range(count)]

    return script


def _served_tail(per_page: float, batch: int = 8, burst: float = 0.1) -> Any:
    """A served road fed a LONG volume: batches fire, only the last bursts.

    What the same engine does once the feed is long enough to fill batches:
    every full batch lands as it completes and only the final partial one
    waits for the end. This is the shape the re-feed rule is trying to buy.
    """

    def script(_widths: tuple[int, ...], count: int) -> list[float]:
        offsets = []
        whole = (count // batch) * batch
        for n in range(whole):
            offsets.append(per_page * batch * (n // batch + 1))
        tail = count - whole
        end = per_page * count
        for n in range(tail):
            offsets.append(end + n * burst / max(1, tail))
        return offsets

    return script


def _bench(
    monkeypatch: pytest.MonkeyPatch,
    pages: int,
    script: Any,
    *,
    min_window: float = 0.5,
    short_window: float = 0.25,
    max_passes: int = 8,
    max_repeat: int = 64,
) -> tuple[Any, list[dict[str, Any]]]:
    """A ``BenchRun`` over a scripted pipeline, with the windows scaled down.

    The RULES are what is under test, not the constants: 20 s and 10 s in a
    unit test would be twenty seconds of sleeping per trial.
    """
    monkeypatch.setattr(runner, "BENCH_MIN_WINDOW_SECONDS", min_window)
    monkeypatch.setattr(runner, "BENCH_SHORT_WINDOW_SECONDS", short_window)
    monkeypatch.setattr(runner, "BENCH_MAX_PASSES", max_passes)
    monkeypatch.setattr(runner, "BENCH_MAX_REPEAT", max_repeat)
    events: list[dict[str, Any]] = []

    class _Protocol:
        def emit(self, event: str, **fields: Any) -> None:
            events.append({"event": event, **fields})

    args = types.SimpleNamespace(
        input=".", bench_max_trials=8, bench_budget_seconds=900.0
    )
    bench = runner.BenchRun(args, _Protocol())  # type: ignore[arg-type]
    bench.pipe = _ScriptedPipe(script)  # type: ignore[assignment]
    bench.scratch = Path(".")
    bench.sample = [Path(f"{n:03d}.jpg") for n in range(pages)]
    return bench, events


class TestBenchWindowArithmetic:
    def test_the_rate_is_intervals_over_the_span_with_the_fill_skipped(self) -> None:
        # Ten emissions a second apart; the first two are the fill.
        window = runner.bench_window([float(n) for n in range(10)], fill=2)
        assert window.pages_measured == 8
        assert window.window_seconds == pytest.approx(7.0)
        # 8 emissions delimit 7 intervals: 7/7, not 8/7.
        assert window.pages_per_second == pytest.approx(1.0)
        assert window.first_emission_at == 2.0 and window.last_emission_at == 9.0

    def test_the_fill_is_the_first_eight_or_a_quarter_whichever_is_smaller(self) -> None:
        assert runner.bench_fill(8) == 2
        assert runner.bench_fill(32) == 8
        assert runner.bench_fill(512) == 8
        assert runner.bench_fill(3) == 0

    def test_nothing_before_the_first_emission_can_enter_the_rate(self) -> None:
        """A model load in front of the emissions is simply not in the span."""
        loaded_late = [30.0, 30.5, 31.0, 31.5, 32.0, 32.5]
        loaded_fast = [t - 29.0 for t in loaded_late]
        assert runner.bench_window(loaded_late, fill=1).pages_per_second == pytest.approx(
            runner.bench_window(loaded_fast, fill=1).pages_per_second
        )

    def test_a_window_under_ten_seconds_is_flagged(self) -> None:
        assert runner.bench_window([0.0, 9.0], fill=0).short_window is True
        assert runner.bench_window([0.0, 11.0], fill=0).short_window is False

    def test_a_burst_yields_no_rate_rather_than_a_six_figure_one(self) -> None:
        """The defect's own shape, fed to the new arithmetic directly."""
        burst = [2.4 + n * 5.5e-6 for n in range(8)]
        window = runner.bench_window(burst, fill=2)
        assert window.pages_measured == 6
        assert window.short_window is True
        # The old formula on this very list:
        old = (len(burst) - 2) / (burst[-1] - burst[1])
        assert old > 100000, "the burst is the one that produced the 217052 number"
        assert window.pages_per_second < 1e6 and window.window_seconds < 1e-3

    def test_a_single_emission_is_not_a_rate(self) -> None:
        window = runner.bench_window([1.0], fill=0)
        assert window.pages_per_second == 0.0 and window.short_window is True


class TestTheTrialWindowCannotCollapse:
    def test_a_burst_feed_is_re_fed_until_the_window_is_real(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """THE REGRESSION: the 8-page widen trial that reported 217052 pages/s.

        Eight pages, all emitted in one instant 0.24 s into the feed -- the
        shape a widened detect pool produces when the whole sample fits in
        flight. The old window was the span of the last six of those, which
        is microseconds. The new one keeps feeding until the span of the
        EMISSIONS is long enough to mean something.
        """
        bench, _events = _bench(monkeypatch, 8, _served_burst(0.03), min_window=0.5)
        seconds, window, _report = bench._measure(bench.sample, [2], trial=1)

        assert window.window_seconds >= 0.5
        assert window.short_window is False
        # ~33 pages/s is what 8 pages in 0.24 s is, and that is what comes
        # out -- not the six figures the collapsed window produced.
        assert 15.0 < window.pages_per_second < 60.0
        assert seconds >= window.window_seconds
        # Every feed ran at the widths it was asked for.
        assert set(bench.pipe.at_widths) == {(2,)}

    def test_a_short_feed_is_re_fed_as_ONE_LONGER_FEED_not_as_more_feeds(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The served road's requirement (ADDENDUM 8, window 138).

        A served engine holds its last batch until the feed ends, so N
        separate begin/end cycles emit N bursts and a rate read across them
        is reading the gaps BETWEEN volumes. The sample must therefore be
        repeated INSIDE one continuous feed, which is what makes the batches
        fire continuously and leaves only the final partial one bursting.
        """
        bench, _events = _bench(monkeypatch, 8, _served_tail(0.03, batch=8), min_window=0.6)
        _seconds, window, _report = bench._measure(bench.sample, [1], trial=1)

        assert len(bench.pipe.feeds) == 2, "one probe feed, then one sized feed"
        assert bench.pipe.feeds[0] == 8, "the probe is the sample once"
        assert bench.pipe.feeds[1] % 8 == 0 and bench.pipe.feeds[1] >= 24, (
            "the re-feed is the sample repeated inside ONE run, not more runs"
        )
        # The window is that one long feed's own emissions.
        assert window.passes == bench.pipe.feeds[1] // 8
        assert window.window_seconds >= 0.6
        assert window.short_window is False
        assert window.pages_per_second == pytest.approx(1 / 0.03, rel=0.3)

    def test_the_re_feed_is_sized_from_the_steady_rate_not_the_ramp(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """One extra feed per trial is what getting this wrong costs.

        A short feed spends a real share of its wall time filling the
        pipeline, so pages-over-wall-time understates what a longer feed
        runs at. Size from that and the sized feed lands short and has to be
        grown again. Here the first feed idles 0.5 s before its pages come
        out at 0.05 s each: wall rate ~12 pages/s, steady rate 20.
        """

        def ramped(_widths: tuple[int, ...], count: int) -> list[float]:
            return [0.5 + 0.05 * (n + 1) for n in range(count)]

        bench, _events = _bench(monkeypatch, 8, ramped, min_window=1.0)
        _seconds, window, _report = bench._measure(bench.sample, [1], trial=1)
        # Two feeds, not three: the second was sized to clear the window.
        assert len(bench.pipe.feeds) == 2
        assert window.window_seconds >= 1.0
        assert window.short_window is False

    def test_a_burst_feed_is_never_sized_from_its_own_burst_rate(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Its window covers none of its work, so the wall clock sizes it.

        Sizing from a burst's own rate (thousands of pages a second) would
        ask for a feed that never ends.
        """
        bench, _events = _bench(
            monkeypatch, 8, _served_burst(0.02, burst=0.001), min_window=1.0, max_passes=2
        )
        bench._measure(bench.sample, [1], trial=1)
        # 8 pages in 0.16 s is 50 pages/s; a 1 s window needs ~60 pages, so
        # ~8 repeats -- not the thousands the burst rate would have asked for.
        assert bench.pipe.feeds[1] <= 8 * 16, bench.pipe.feeds

    def test_a_road_that_bursts_every_feed_is_read_over_the_whole_span(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """All emissions of a feed inside 100 ms, however long the feed.

        Nothing can make such a road emit continuously, so the rate is
        (M-1) over the span of every feed together -- never the burst's own
        rate, which here would be four orders of magnitude out.
        """
        bench, _events = _bench(
            monkeypatch, 8, _served_burst(0.02, burst=0.1), min_window=1.0, max_passes=4
        )
        _seconds, window, _report = bench._measure(bench.sample, [1], trial=1)

        assert len(bench.pipe.feeds) == 4, "it kept trying to buy a real window"
        assert window.passes == sum(bench.pipe.feeds) // 8
        assert window.window_seconds >= 1.0, "the span of the feeds, not of a burst"
        assert window.short_window is False
        # The true rate is 1/0.02 = 50 pages/s. The BURST rate is
        # count/0.1 s -- hundreds. Only the first is allowed out.
        assert 25.0 < window.pages_per_second < 90.0

    def test_the_loop_stops_at_the_feed_cap_and_says_the_window_is_short(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        bench, _events = _bench(
            monkeypatch,
            8,
            lambda _widths, count: [0.0] * count,
            min_window=60.0,
            max_passes=3,
        )
        _seconds, window, _report = bench._measure(bench.sample, [1], trial=1)
        assert len(bench.pipe.feeds) == 3
        assert window.short_window is True

    def test_a_feed_that_already_fills_the_window_runs_once(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # fill=2, so the measured span is the last six: 5 x 0.06 = 0.30 s.
        bench, _events = _bench(monkeypatch, 8, _continuous(0.06), min_window=0.2)
        _seconds, window, _report = bench._measure(bench.sample, [1], trial=1)
        assert bench.pipe.feeds == [8]
        assert window.passes == 1
        assert window.pages_measured == 6
        assert window.pages_per_second == pytest.approx(1 / 0.06, rel=0.25)

    def test_the_feeds_are_merged_into_one_reading(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The verdict is about the TRIAL, not about whichever feed was last."""
        bench, _events = _bench(
            monkeypatch, 8, _served_burst(0.01), min_window=0.3, max_passes=3
        )
        _seconds, _window, report = bench._measure(bench.sample, [1], trial=1)
        assert report.items == sum(bench.pipe.feeds)
        assert report.stages[0].items == sum(bench.pipe.feeds)

    def test_progress_reports_the_running_window(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(runner, "BENCH_PROGRESS_INTERVAL", 0.0)
        bench, events = _bench(monkeypatch, 8, _continuous(0.02), min_window=0.3)
        bench._measure(bench.sample, [1], trial=2)
        progress = [e for e in events if e["event"] == "bench_progress"]
        assert progress, "a trial says what it is seeing while it sees it"
        assert {"window_seconds", "pages_measured", "pass_index"} <= set(progress[-1])
        assert progress[-1]["trial"] == 2
        # The bar counts the WHOLE feed, which is the sample repeated.
        assert progress[-1]["pages"] == bench.pipe.feeds[-1]


class TestTheTunerRefusesShortWindows:
    def _search(
        self, monkeypatch: pytest.MonkeyPatch, script: Any, **kw: Any
    ) -> tuple[Any, list[dict[str, Any]]]:
        # The verdict machinery has its own tests; what is under test here is
        # what the search does with a window it cannot read.
        monkeypatch.setattr(runner, "widen_target", lambda reading: "detect")
        bench, events = _bench(monkeypatch, 8, script, **kw)
        baseline, best, widths = bench.search([1])
        return (baseline, best, widths), events

    def test_a_widening_measured_over_a_short_window_is_never_kept(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A burst at detect x2 would have looked infinitely fast before."""

        def script(widths: tuple[int, ...], count: int) -> list[float]:
            if widths == (1,):
                return [0.04 * (n + 1) for n in range(count)]
            return [0.02] * count  # the whole feed in one instant, always

        (baseline, best, widths), events = self._search(
            monkeypatch, script, min_window=0.3, short_window=0.3, max_passes=2
        )
        trials = [e for e in events if e["event"] == "bench_trial"]
        assert len(trials) == 2
        assert trials[1]["short_window"] is True
        assert trials[1]["accepted"] is False
        assert best.n == baseline.n == 1 and widths == [1]

    def test_a_short_baseline_freezes_the_search_instead_of_guessing(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        (baseline, best, widths), events = self._search(
            monkeypatch,
            lambda _widths, count: [0.0] * count,
            min_window=10.0,
            short_window=10.0,
            max_passes=2,
        )
        assert baseline.short_window is True
        assert best is baseline and widths == [1]
        assert [e["accepted"] for e in events if e["event"] == "bench_trial"] == [True, False]

    def test_a_long_window_still_decides_normally(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The refusal is about the WINDOW, not a new reluctance to tune."""

        def script(widths: tuple[int, ...], count: int) -> list[float]:
            per_page = 0.04 if widths == (1,) else 0.02
            return [per_page * (n + 1) for n in range(count)]

        (baseline, best, widths), events = self._search(
            monkeypatch, script, min_window=0.4, short_window=0.05, max_passes=8
        )
        assert widths == [2] and best.n > baseline.n
        assert [e["accepted"] for e in events if e["event"] == "bench_trial"][:2] == [True, True]
