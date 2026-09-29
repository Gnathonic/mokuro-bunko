"""Unit tests for the detector registry/config and the pure parts of the
detector adapters (container dropping, YOLO decoding, font-size estimate),
plus the revision pins each adapter carries."""

from __future__ import annotations

import json
import re
import sys
import types
from pathlib import Path
from typing import Any

import pytest
from PIL import Image, ImageDraw

from mokuro_bunko.config import Config, OcrConfig, set_by_dotted_key
from mokuro_bunko.ocr.detectors import _common
from mokuro_bunko.ocr.engines import DEFAULT_DETECTOR, DETECTOR_IDS, get_detector


class TestRegistry:
    def test_ids_and_licenses(self) -> None:
        assert DETECTOR_IDS == ("ppocr-manga", "ctd", "animetext")
        assert DEFAULT_DETECTOR == "ppocr-manga"
        assert get_detector("ctd").license == "GPL-3.0"
        assert get_detector("ctd").extra_packages == ("mokuro",)
        assert get_detector("ctd").probe_import == "comic_text_detector"
        assert get_detector("animetext").license == "GPL-3.0"
        assert get_detector("animetext").extra_packages == ("onnxruntime",)
        assert get_detector("animetext").probe_import == "onnxruntime"
        assert not get_detector("animetext").line_level
        ppocr = get_detector("ppocr-manga")
        assert (ppocr.license, ppocr.line_level) == ("Apache-2.0", True)
        assert (ppocr.extra_packages, ppocr.probe_import) == (("onnxruntime",), "onnxruntime")
        assert all(get_detector(d).isolated for d in DETECTOR_IDS)

    def test_unknown(self) -> None:
        with pytest.raises(ValueError, match="Unknown OCR detector 'x'") as refused:
            get_detector("x")
        # A typo is told only the detectors it could have meant.
        assert "animetext" not in str(refused.value)
        assert "ppocr-manga" in str(refused.value)

    def test_adapter_scripts_exist_with_license_note_on_gpl(self) -> None:
        base = Path(_common.__file__).parent
        for d in DETECTOR_IDS:
            assert (base / get_detector(d).script).is_file()
        assert "GPL-3.0" in (base / "ctd.py").read_text(encoding="utf-8")
        assert "GPL-3.0" in (base / "animetext.py").read_text(encoding="utf-8")


class TestAnimeTextAdapter:
    def test_drop_containers_keeps_parts(self) -> None:
        # One box around two bubbles (also emitted on their own) plus a lone box.
        boxes = [[0, 0, 100, 200], [5, 5, 95, 95], [5, 105, 95, 195], [300, 300, 340, 400]]
        assert _common.drop_containers(boxes) == [1, 2, 3]
        # A single contained box is not enough to call it a container.
        assert _common.drop_containers([[0, 0, 100, 200], [5, 5, 95, 95]]) == [0, 1]
        # Partial overlaps don't count as parts.
        assert _common.drop_containers(
            [[0, 0, 100, 100], [50, 50, 150, 150], [50, 0, 150, 60]]
        ) == [0, 1, 2]

    def test_decode_yolo_maps_back_to_page_pixels(self) -> None:
        from mokuro_bunko.ocr.detectors import animetext

        scale = animetext.letterbox_scale(2000, 1000, 640)
        assert scale == pytest.approx(0.32)
        rows = [
            [320.0, 160.0, 64.0, 32.0, 0.9],  # confident: centre (1000,500), 200x100 on the page
            [10.0, 10.0, 10.0, 10.0, 0.1],  # below confidence
            [1.0, 1.0, 0.5, 0.5, 0.95],  # degenerate (<4 px)
        ]
        boxes, scores = animetext.decode_yolo(rows, scale, 2000, 1000, 0.3)
        assert boxes == [[900, 450, 1100, 550]]
        assert scores == [0.9]

    def test_block_level_in_runner(self) -> None:
        from mokuro_bunko.ocr import engine_runner as runner

        assert "animetext" in runner.BLOCK_LEVEL_DETECTORS
        assert runner.select_crop("hayai-nova", "animetext") == ("upright", 0.0)

    def test_runner_knows_every_registered_detector(self) -> None:
        from mokuro_bunko.ocr import engine_runner as runner

        assert {d: get_detector(d).script for d in DETECTOR_IDS} == runner.DETECTOR_SCRIPTS
        # ppocr-manga emits rotated LINE quads: deskewed crops for hayai-nova, and
        # its one-line blocks are regrouped by line_layout after recognition.
        assert "ppocr-manga" not in runner.BLOCK_LEVEL_DETECTORS
        assert runner.LAYOUT_DETECTORS == {"ppocr-manga"}
        assert runner.select_crop("hayai-nova", "ppocr-manga") == ("line", 0.0)


class TestAnimeTextDisabled:
    """``animetext`` is out of service for this release, not deleted.

    Its spec, adapter and runner support stay (the registry tests above still
    hold), so it can come back by deleting one entry. What is gone is every
    way to ASK for it: no catalog offers it, no ``--detector`` flag takes it,
    and a row naming it is refused wherever a row is parsed, with one
    sentence saying it is disabled -- the removed ``char_map`` key's style.
    """

    @staticmethod
    def _rows(detector: str = "animetext") -> list[dict[str, Any]]:
        return [
            {"name": "mokuro", "engine": "mokuro", "primary": True},
            {"name": "paddle-at", "engine": "paddle-manga", "detector": detector},
        ]

    def test_kept_in_the_registry_but_not_offered(self) -> None:
        from mokuro_bunko.ocr.engines import DISABLED_DETECTORS, OFFERED_DETECTOR_IDS

        assert "animetext" in DETECTOR_IDS
        assert get_detector("animetext").script == "animetext.py"
        assert "animetext" in DISABLED_DETECTORS
        assert OFFERED_DETECTOR_IDS == ("ppocr-manga", "ctd")

    def test_a_row_naming_it_is_refused_at_load(self) -> None:
        from mokuro_bunko.ocr.generations import GenerationConfigError, parse_generation_list

        with pytest.raises(GenerationConfigError) as caught:
            parse_generation_list(self._rows())
        assert (caught.value.row, caught.value.field) == (1, "detector")
        message = str(caught.value)
        assert message.startswith("ocr.generations[1]: detector 'animetext' is disabled for now")
        assert "one of ppocr-manga, ctd," in message
        # Whitespace from YAML does not get it past the check.
        with pytest.raises(ValueError, match="'animetext' is disabled for now"):
            Config.from_dict({"ocr": {"generations": self._rows(" animetext ")}})

    def test_a_config_file_or_environment_naming_it_is_refused(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        from mokuro_bunko.config import load_config

        monkeypatch.setenv("MOKURO_OCR_GENERATIONS", json.dumps(self._rows()))
        with pytest.raises(ValueError, match="'animetext' is disabled for now"):
            load_config(tmp_path / "missing.yaml")

    def test_a_benchmark_spec_naming_it_is_refused(self) -> None:
        from mokuro_bunko.ocr.generations import GenerationConfigError, parse_bench_spec

        with pytest.raises(GenerationConfigError) as caught:
            parse_bench_spec({"engine": "hayai-nova", "detector": "animetext"})
        assert (caught.value.row, caught.value.field) == (None, "detector")
        assert str(caught.value).startswith("spec: detector 'animetext' is disabled for now")

    def test_a_processor_refuses_a_row_a_library_sends_with_it(self) -> None:
        # An older library can still hand one out; the processor reports it
        # as a spawn failure (ValueError) instead of loading it.
        from mokuro_bunko.processor.bridge import RunnerBridge

        with pytest.raises(ValueError, match="'animetext' is disabled for now"):
            RunnerBridge._row({"engine": "paddle-manga", "detector": "animetext"})

    def test_an_engine_with_its_own_detector_is_unaffected(self) -> None:
        # ppocr-manga never reads the row's detector, so a stale value there
        # is ignored exactly as any other detector name is.
        rows = OcrConfig(
            generations=[
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"engine": "ppocr-manga", "detector": "animetext"},
            ]
        ).generations
        assert rows[1].detector is None

    def test_the_admin_catalog_does_not_offer_it(self) -> None:
        from mokuro_bunko.admin.api import AdminAPI
        from mokuro_bunko.ocr.devices import DeviceCatalog

        catalog = AdminAPI._generations_catalog(DeviceCatalog(probed=True))
        assert [d["id"] for d in catalog["detectors"]] == ["ppocr-manga", "ctd"]

    def test_a_processor_catalog_does_not_report_it(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        from mokuro_bunko.ocr import bench, installer
        from mokuro_bunko.processor.cli import _catalog
        from mokuro_bunko.processor.config import load_processor_config

        # onnxruntime is installed (ppocr-manga needs it), which is all the
        # animetext probe asks for: without the filter it would be reported.
        monkeypatch.delenv("MOKURO_PROCESSOR_RUNNER", raising=False)
        monkeypatch.setenv("MOKURO_PROCESSOR_ENGINES_PYTHON", str(tmp_path / "python"))
        monkeypatch.setattr(installer.EnginesInstaller, "is_installed", lambda self: True)
        monkeypatch.setattr(
            installer.EnginesInstaller, "has_detector", lambda self, detector=None: True
        )
        monkeypatch.setattr(installer.OCRInstaller, "is_installed", lambda self: False)

        class _Devices:
            ort_gpu_providers = None

            def entries(self) -> list[dict[str, Any]]:
                return [{"id": "auto", "label": "Auto"}]

        monkeypatch.setattr(bench, "probe_devices", lambda python: _Devices())
        monkeypatch.setattr(bench, "describe_host", lambda backend, python: {"gpu": None})
        config_path = tmp_path / "processor.yaml"
        config_path.write_text(
            "library:\n"
            "  url: https://library.example:8080\n"
            "  username: tower\n"
            "  password: hunter2hunter2\n",
            encoding="utf-8",
        )
        catalog, _host = _catalog(load_processor_config(config_path))
        assert catalog["detectors"] == ["ctd", "ppocr-manga"]
        # The runner-override branch lists everything it knows -- minus this.
        monkeypatch.setenv("MOKURO_PROCESSOR_RUNNER", str(tmp_path / "runner.py"))
        catalog, _host = _catalog(load_processor_config(config_path))
        assert catalog["detectors"] == ["ctd", "ppocr-manga"]

    @pytest.mark.parametrize("command", ["install-ocr", "processor install"])
    def test_no_install_flag_takes_it(
        self, command: str, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from click.testing import CliRunner

        from mokuro_bunko.__main__ import install_ocr
        from mokuro_bunko.ocr import installer
        from mokuro_bunko.processor.cli import processor_group

        # Were the flag accepted, the command would go on to build real
        # environments (the default path is the project root's .ocr-env):
        # nothing here may get that far, whatever the outcome.
        def refuse(*_args: Any, **_kwargs: Any) -> bool:
            raise AssertionError("the command reached an installer")

        for cls in (installer.OCRInstaller, installer.EnginesInstaller):
            monkeypatch.setattr(cls, "install_with_fallback", refuse)
            monkeypatch.setattr(cls, "install", refuse, raising=False)
        monkeypatch.setenv("MOKURO_BUNKO_OCR_ENV", str(tmp_path / "ocr-env"))
        monkeypatch.setenv("MOKURO_BUNKO_OCR_ENGINES_ENV", str(tmp_path / "engines-env"))
        if command == "install-ocr":
            target, args = install_ocr, ["--detector", "animetext"]
        else:
            target = processor_group
            args = ["install", "--config", str(tmp_path / "p.yaml"), "--detector", "animetext"]
        result = CliRunner().invoke(target, args)
        assert result.exit_code == 2, result.output
        assert "animetext" in result.output and "--detector" in result.output


class TestConfig:
    """The detector is a field of a GENERATION row, not a server setting.

    There is no ``ocr.detector`` any more (a config or environment still
    carrying one is refused by name); two rows on one engine reading with
    different detectors is the whole point of the list. What the config
    layer owed the setting is unchanged and is pinned here: a default, a
    validated override, a round trip through ``to_dict``/``config set``, and
    the environment.
    """

    @staticmethod
    def _rows(*extra: dict[str, Any]) -> list[dict[str, Any]]:
        return [{"name": "mokuro", "engine": "mokuro", "primary": True}, *extra]

    def test_default_and_override(self) -> None:
        # A row that names no detector reads pages with the default one.
        rows = OcrConfig(generations=self._rows({"engine": "hayai-nova"})).generations
        assert rows[1].effective_detector == DEFAULT_DETECTOR == "ppocr-manga"
        ocr = Config.from_dict(
            {"ocr": {"generations": self._rows({"engine": "hayai-nova", "detector": "ctd"})}}
        ).ocr
        assert ocr.generations[1].detector == "ctd"
        # YAML hands it over with whatever whitespace it was written with.
        ocr = Config.from_dict(
            {
                "ocr": {
                    "generations": self._rows({"engine": "hayai-nova", "detector": " ppocr-manga "})
                }
            }
        ).ocr
        assert ocr.generations[1].detector == "ppocr-manga"

    def test_invalid_rejected(self) -> None:
        # A detector the tree no longer has (a removed one included) is told
        # only the detectors on offer.
        with pytest.raises(ValueError) as caught:
            Config.from_dict(
                {"ocr": {"generations": self._rows({"engine": "hayai-nova", "detector": "yolo"})}}
            )
        assert "Unknown OCR detector 'yolo' (known: ppocr-manga, ctd)" in str(caught.value)

    def test_round_trip_and_dotted_key(self) -> None:
        config = Config.from_dict(
            {"ocr": {"generations": self._rows({"engine": "hayai-nova", "detector": "ctd"})}}
        )
        assert config.to_dict()["ocr"]["generations"][1]["detector"] == "ctd"
        set_by_dotted_key(
            config,
            "ocr.generations",
            json.dumps(self._rows({"engine": "hayai-nova", "detector": "ppocr-manga"})),
        )
        assert config.ocr.generations[1].detector == "ppocr-manga"
        with pytest.raises(ValueError):
            set_by_dotted_key(
                config,
                "ocr.generations",
                json.dumps(self._rows({"engine": "hayai-nova", "detector": "nope"})),
            )

    def test_env_override(self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> None:
        from mokuro_bunko.config import load_config

        monkeypatch.setenv(
            "MOKURO_OCR_GENERATIONS",
            json.dumps(self._rows({"engine": "hayai-nova", "detector": "ctd"})),
        )
        rows = load_config(tmp_path / "missing.yaml").ocr.generations
        assert rows[1].detector == "ctd"

    def test_an_engine_with_its_own_detector_ignores_the_rows_choice(self) -> None:
        """Its two models were trained as a pair; the row cannot split them."""
        rows = OcrConfig(
            generations=self._rows({"engine": "ppocr-manga", "detector": "ctd"})
        ).generations
        assert rows[1].detector is None
        assert rows[1].effective_detector == "ppocr-manga"


def _call_arguments(source: str, opener: str) -> list[str]:
    """The argument text of every ``opener(...)`` call in ``source``."""
    calls = []
    for match in re.finditer(re.escape(opener) + r"\(", source):
        depth, start = 0, match.end()
        for i in range(match.end() - 1, len(source)):
            if source[i] == "(":
                depth += 1
            elif source[i] == ")":
                depth -= 1
                if depth == 0:
                    calls.append(source[start:i])
                    break
    return calls


class TestPinnedDetectorWeights:
    """Every detector adapter pins the weights it loads, and reports them.

    The runner pins what IT resolves (``engine_runner.REPO_REVISIONS``), but an
    adapter is a separate script in a separate process -- that boundary is the
    licence containment -- so it cannot reach that table and carries its own
    pin. Without one the DEFAULT detector resolves a moving ``main``: what
    boxes the text can change under a self-hoster between two runs of the same
    volume, and the sidecar's ``weights`` cannot name it.
    """

    def test_every_adapter_pins_what_it_loads(self) -> None:
        from mokuro_bunko.ocr import ppocr
        from mokuro_bunko.ocr.detectors import animetext, ctd

        assert len(animetext.REVISION) == 40
        assert all(c in "0123456789abcdef" for c in animetext.REVISION)
        # ctd's weights are not on the Hub: the mokuro package fetches them
        # from an immutable GitHub release asset, so the URL is the pin and
        # the digest is what enforces it.
        assert "/releases/download/" in ctd.WEIGHTS_URL
        assert len(ctd.WEIGHTS_SHA256) == 64
        assert all(c in "0123456789abcdef" for c in ctd.WEIGHTS_SHA256)
        # ppocr_manga loads through ppocr.py, which pins its own repo.
        assert len(ppocr.REPO_REVISION) == 40

    def test_no_adapter_resolves_a_repo_without_a_revision(self) -> None:
        """The pin has to reach the loader, not merely sit in a constant.

        This is the regression: ``REPO_REVISIONS`` covered the runner while
        the default detector's adapter still called ``from_pretrained(REPO)``
        bare.
        """
        base = Path(_common.__file__).parent
        for script in sorted(base.glob("*.py")):
            source = script.read_text(encoding="utf-8")
            for opener in (".from_pretrained", "hf_hub_download", "snapshot_download"):
                for arguments in _call_arguments(source, opener):
                    assert "revision=" in arguments, f"{script.name}: {opener}({arguments})"

    def test_animetext_passes_the_pin_to_the_download(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.ocr.detectors import animetext

        downloads: list[tuple[tuple[Any, ...], dict[str, Any]]] = []

        def fake_download(*args: Any, **kwargs: Any) -> str:
            downloads.append((args, kwargs))
            return str(tmp_path / "model.onnx")

        class _Session:
            def __init__(self, *a: Any, **kw: Any) -> None:
                pass

            def get_inputs(self) -> list[Any]:
                return [types.SimpleNamespace(name="images", shape=[1, 3, 640, 640])]

            def get_providers(self) -> list[str]:
                return ["CPUExecutionProvider"]

        monkeypatch.setitem(
            sys.modules,
            "huggingface_hub",
            types.SimpleNamespace(hf_hub_download=fake_download),
        )
        monkeypatch.setitem(
            sys.modules,
            "onnxruntime",
            types.SimpleNamespace(
                InferenceSession=_Session,
                get_available_providers=lambda: ["CPUExecutionProvider"],
            ),
        )
        # numpy lives only in the engines venv; setup merely binds it, and
        # with no pages nothing reaches it.
        monkeypatch.setitem(sys.modules, "numpy", types.SimpleNamespace())

        input_dir, out_dir = tmp_path / "Vol", tmp_path / "detect"
        input_dir.mkdir()
        pages = tmp_path / "pages.txt"
        pages.write_text("", encoding="utf-8")
        monkeypatch.setattr(
            sys, "argv", ["animetext.py", "--input", str(input_dir), "--pages", str(pages),
                          "--output-dir", str(out_dir)],
        )  # fmt: skip

        animetext.main()
        ((_args, kwargs),) = downloads
        assert kwargs["revision"] == animetext.REVISION
        reported = json.loads((out_dir / _common.WEIGHTS_FILE).read_text(encoding="utf-8"))
        assert reported == {animetext.REPO: animetext.REVISION}

    def test_ctd_refuses_a_checkpoint_that_is_not_the_pinned_one(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
    ) -> None:
        """A .pt is a pickle, so the digest is the only pin available and it
        has to refuse, not warn: loading the wrong file executes it."""
        from mokuro_bunko.ocr.detectors import ctd

        impostor = tmp_path / "comictextdetector.pt"
        impostor.write_bytes(b"not the checkpoint")

        def _no_load(*a: Any, **kw: Any) -> Any:  # pragma: no cover - must not run
            raise AssertionError("TextDetector must not see an unpinned checkpoint")

        monkeypatch.setitem(
            sys.modules,
            "torch",
            types.SimpleNamespace(cuda=types.SimpleNamespace(is_available=lambda: False)),
        )
        monkeypatch.setitem(sys.modules, "cv2", types.SimpleNamespace(cvtColor=_no_load))
        # numpy lives only in the engines venv; the refusal comes before any use.
        monkeypatch.setitem(sys.modules, "numpy", types.SimpleNamespace())
        monkeypatch.setitem(
            sys.modules,
            "comic_text_detector",
            types.SimpleNamespace(inference=types.SimpleNamespace(TextDetector=_no_load)),
        )
        monkeypatch.setitem(
            sys.modules,
            "comic_text_detector.inference",
            types.SimpleNamespace(TextDetector=_no_load),
        )
        monkeypatch.setitem(
            sys.modules,
            "mokuro",
            types.SimpleNamespace(cache=types.SimpleNamespace(cache=None)),
        )
        monkeypatch.setitem(
            sys.modules,
            "mokuro.cache",
            types.SimpleNamespace(cache=types.SimpleNamespace(comic_text_detector=impostor)),
        )

        input_dir, out_dir = tmp_path / "Vol", tmp_path / "detect"
        input_dir.mkdir()
        pages = tmp_path / "pages.txt"
        pages.write_text("", encoding="utf-8")
        monkeypatch.setattr(
            sys, "argv", ["ctd.py", "--input", str(input_dir), "--pages", str(pages),
                          "--output-dir", str(out_dir)],
        )  # fmt: skip

        assert ctd.main() == 1
        assert "not the pinned checkpoint" in capsys.readouterr().out
        assert not (out_dir / _common.WEIGHTS_FILE).exists()

    def test_ppocr_manga_claims_the_pin_only_for_the_pinned_download(self) -> None:
        """Files found in a ``$MOKURO_PPOCR_MODELS`` directory are whatever was
        copied there, so no commit can be claimed for them."""
        from mokuro_bunko.ocr import ppocr
        from mokuro_bunko.ocr.detectors import ppocr_manga

        pinned = types.SimpleNamespace(models=types.SimpleNamespace(pinned=True))
        local = types.SimpleNamespace(models=types.SimpleNamespace(pinned=False))
        assert ppocr_manga.model_weights(ppocr, pinned) == {ppocr.REPO_ID: ppocr.REPO_REVISION}
        assert ppocr_manga.model_weights(ppocr, local) == {}


class TestFontSizeEstimate:
    def _columns(self, width: int, height: int, col_w: int, gap: int, n: int) -> Image.Image:
        img = Image.new("L", (width, height), 255)
        draw = ImageDraw.Draw(img)
        x = 5
        for _ in range(n):
            draw.rectangle([x, 5, x + col_w - 1, height - 6], fill=0)
            x += col_w + gap
        return img

    def test_vertical_columns(self) -> None:
        img = self._columns(120, 200, col_w=20, gap=8, n=4)
        assert 18 <= _common.estimate_font_size(img, vertical=True) <= 22

    def test_ignores_thin_runs(self) -> None:
        img = self._columns(120, 200, col_w=20, gap=8, n=3)
        ImageDraw.Draw(img).rectangle([100, 5, 104, 194], fill=0)  # furigana-like sliver
        assert 18 <= _common.estimate_font_size(img, vertical=True) <= 22

    def test_blank_and_tiny_crops_fall_back(self) -> None:
        assert _common.estimate_font_size(Image.new("L", (50, 100), 255), vertical=True) == 30
        assert _common.estimate_font_size(Image.new("L", (2, 2), 0), vertical=True) == 8

    def test_dedupe_boxes_is_class_agnostic_and_keeps_best(self) -> None:
        boxes = [[0, 0, 100, 100], [2, 2, 98, 98], [200, 200, 300, 300], [0, 0, 50, 50]]
        scores = [0.8, 0.9, 0.7, 0.6]
        # Box 1 beats its near-duplicate box 0; box 3 overlaps box 1 only partially (IoU 0.27).
        assert _common.dedupe_boxes(boxes, scores) == [1, 2, 3]
        assert _common.dedupe_boxes([], []) == []
        assert _common.box_iou([0, 0, 10, 10], [5, 5, 15, 15]) == pytest.approx(25 / 175)

    def test_block_quad(self) -> None:
        assert _common.block_quad([1, 2, 3, 4]) == [[1.0, 2.0], [3.0, 2.0], [3.0, 4.0], [1.0, 4.0]]
