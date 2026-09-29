"""Unit tests for the engines-environment installer."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from mokuro_bunko.ocr.generations import parse_generation_list, required_detectors
from mokuro_bunko.ocr.installer import (
    ENGINES_ENV_PACKAGES,
    EnginesInstaller,
    OCRBackend,
    OCRInstaller,
)


class TestEnvPath:
    def test_override_env_var(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv("MOKURO_BUNKO_OCR_ENGINES_ENV", "/tmp/engines-env")
        assert EnginesInstaller().env_path == Path("/tmp/engines-env")

    def test_project_default_is_distinct_from_mokuro_env(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.delenv("MOKURO_BUNKO_OCR_ENGINES_ENV", raising=False)
        monkeypatch.delenv("MOKURO_BUNKO_OCR_ENV", raising=False)
        mokuro_env = OCRInstaller.get_default_env_path()
        engines_env = EnginesInstaller.get_default_env_path()
        assert engines_env != mokuro_env
        assert engines_env.name == ".ocr-engines-env"

    def test_home_fallback(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.delenv("MOKURO_BUNKO_OCR_ENGINES_ENV", raising=False)
        monkeypatch.setattr(EnginesInstaller, "_discover_project_root", lambda: None)
        assert (
            EnginesInstaller.get_default_env_path()
            == Path.home() / ".mokuro-bunko" / "ocr-engines-env"
        )

    def test_mokuro_env_override_does_not_leak(
        self, monkeypatch: pytest.MonkeyPatch, tmp_path: Path
    ) -> None:
        monkeypatch.setenv("MOKURO_BUNKO_OCR_ENV", str(tmp_path / "mokuro"))
        monkeypatch.delenv("MOKURO_BUNKO_OCR_ENGINES_ENV", raising=False)
        assert EnginesInstaller.get_default_env_path() != tmp_path / "mokuro"


class TestInstall:
    def test_package_set(self, tmp_path: Path) -> None:
        installer = EnginesInstaller(env_path=tmp_path / "env", output_callback=lambda m: None)
        installer.create_environment()
        with patch.object(installer, "_run_pip", return_value=True) as mock_pip:
            assert installer.install_mokuro()
        cmd = mock_pip.call_args[0][0]
        assert "transformers==5.17.0" in cmd
        assert "peft" in cmd
        # `hayai-ocr` drove the retired v2 engine and nothing else; Nova goes
        # through transformers directly, so the package is not installed.
        assert not any("hayai" in part for part in cmd)
        # manga-ocr was here only to lend attention positions to the retired
        # character-map system. Nothing in this environment reads it now, and
        # a second recognizer's weights are not a download to keep for nobody.
        assert not any("manga" in part and "ocr" in part for part in cmd)
        # The GPL comic-text-detector (via mokuro) is never installed by default.
        assert "mokuro" not in cmd
        assert "transformers>=4.25,<5" not in cmd
        # ...and the default detector (ppocr-manga) brings onnxruntime.
        assert list(cmd[2:]) == [*ENGINES_ENV_PACKAGES, "onnxruntime"]

    def test_what_turns_a_page_into_a_crop_is_pinned(self) -> None:
        """Every read starts from a crop these three made.

        transformers' image processors turn a crop into patches, OpenCV warps
        and cuts the line out of the page, Pillow decodes the page: a silent
        upgrade of any of them could change a sidecar with nothing else
        changing. Pinned to what the workstation and tower both run (the two
        reproduce each other's line crops bit for bit, 403/403); bump a pin
        the way a model pin is bumped -- re-run a bench volume and compare.
        """
        pins = dict(spec.split("==", 1) for spec in ENGINES_ENV_PACKAGES if "==" in spec)
        assert pins == {
            "transformers": "5.17.0",
            "opencv-python-headless": "5.0.0.93",
            "Pillow": "12.3.0",
        }
        assert not any(">" in spec or "<" in spec for spec in ENGINES_ENV_PACKAGES)

    def test_ctd_detector_adds_mokuro_extra(self, tmp_path: Path) -> None:
        installer = EnginesInstaller(
            env_path=tmp_path / "env", output_callback=lambda m: None, detector="ctd"
        )
        installer.create_environment()
        with patch.object(installer, "_run_pip", return_value=True) as mock_pip:
            assert installer.install_mokuro()
        assert "mokuro" in mock_pip.call_args[0][0]
        with patch.object(installer, "_run_pip", return_value=True) as mock_pip:
            assert installer.install_detector("ctd")
        assert mock_pip.call_args[0][0][2:] == ["mokuro"]

    def test_the_default_detector_brings_onnxruntime(self, tmp_path: Path) -> None:
        installer = EnginesInstaller(env_path=tmp_path / "env", output_callback=lambda m: None)
        assert installer.detector == "ppocr-manga"
        installer.create_environment()
        with patch.object(installer, "_run_pip", return_value=True) as mock_pip:
            assert installer.install_detector("ppocr-manga")
        assert mock_pip.call_args[0][0][2:] == ["onnxruntime"]

    def test_has_detector_probes_extra_import(self, tmp_path: Path) -> None:
        installer = EnginesInstaller(
            env_path=tmp_path / "env", output_callback=lambda m: None, detector="ctd"
        )
        installer.create_environment()
        completed = MagicMock(returncode=1)
        with patch("mokuro_bunko.ocr.installer.subprocess.run", return_value=completed) as run:
            assert not installer.has_detector()
        assert "comic_text_detector" in run.call_args[0][0][2]

    def test_engine_with_its_own_detector_brings_that_detectors_extras(
        self, tmp_path: Path
    ) -> None:
        """The environment is built for the UNION over the generations.

        ppocr-manga reads pages with onnxruntime whatever its row's
        ``detector`` says, so a list holding both rows needs both detectors'
        extras installed. The union is computed from the rows
        (``generations.required_detectors``) and handed over: the installer
        knows nothing about generations.
        """
        rows = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "nova", "engine": "hayai-nova", "detector": "ctd"},
                {"name": "ppocr", "engine": "ppocr-manga"},
            ]
        )
        assert required_detectors(rows) == ("ctd", "ppocr-manga")
        installer = EnginesInstaller(
            env_path=tmp_path / "env",
            output_callback=lambda m: None,
            detector="ctd",
            detectors=required_detectors(rows),
        )
        assert installer.detectors == ("ctd", "ppocr-manga")
        installer.create_environment()
        with patch.object(installer, "_run_pip", return_value=True) as mock_pip:
            assert installer.install_mokuro()
        assert list(mock_pip.call_args[0][0][2:]) == [
            *ENGINES_ENV_PACKAGES,
            "mokuro",
            "onnxruntime",
        ]
        with patch.object(installer, "_run_pip", return_value=True) as mock_pip:
            assert installer.install_detector()
        assert [call[0][0][2:] for call in mock_pip.call_args_list] == [["mokuro"], ["onnxruntime"]]
        with patch.object(
            installer, "_probe", side_effect=lambda s: "onnxruntime" not in s
        ) as probe:
            assert not installer.has_detector()
            assert installer.has_detector("ctd")
        assert probe.call_count == 3

        only_ppocr = parse_generation_list(
            [
                {"name": "mokuro", "engine": "mokuro", "primary": True},
                {"name": "ppocr", "engine": "ppocr-manga"},
            ]
        )
        alone = EnginesInstaller(
            env_path=tmp_path / "env",
            output_callback=lambda m: None,
            detectors=required_detectors(only_ppocr),
        )
        # The row's own detector is never used, and the monolithic mokuro row
        # brings its detector with it, so neither reaches this environment.
        assert alone.detectors == ("ppocr-manga",)
        # `set_detectors` moves both fields together: a readiness check made
        # after a settings change must never test the detector before it.
        assert alone.detector == "ppocr-manga"

    def test_unknown_detector_rejected(self, tmp_path: Path) -> None:
        with pytest.raises(ValueError, match="Unknown OCR detector"):
            EnginesInstaller(env_path=tmp_path / "env", detector="yolo")

    def test_install_runs_torch_then_engines_then_verify(self, tmp_path: Path) -> None:
        installer = EnginesInstaller(env_path=tmp_path / "env", output_callback=lambda m: None)
        calls: list[list[str]] = []

        def fake_pip(cmd: list[str]) -> bool:
            calls.append(cmd)
            return True

        with (
            patch.object(installer, "_run_pip", side_effect=fake_pip),
            patch.object(installer, "verify_installation", return_value=(True, ["ok"])),
        ):
            assert installer.install(OCRBackend.CPU)
        # pip upgrade, torch, engines
        assert any("torch" in c for c in calls)
        assert any("transformers==5.17.0" in c for c in calls)

    def test_is_installed_false_without_env(self, tmp_path: Path) -> None:
        assert not EnginesInstaller(env_path=tmp_path / "nope").is_installed()

    def test_is_installed_checks_engine_imports(self, tmp_path: Path) -> None:
        installer = EnginesInstaller(env_path=tmp_path / "env", output_callback=lambda m: None)
        installer.create_environment()
        completed = MagicMock(returncode=0)
        with patch("mokuro_bunko.ocr.installer.subprocess.run", return_value=completed) as run:
            assert installer.is_installed()
        snippet = run.call_args[0][0][2]
        assert "transformers" in snippet and "cv2" in snippet
        # Probing for the retired v2 engine's package would report every
        # freshly built environment as missing.
        assert "hayai" not in snippet
        assert "comic_text_detector" not in snippet

    def test_verify_flags_old_transformers(self, tmp_path: Path) -> None:
        installer = EnginesInstaller(env_path=tmp_path / "env", output_callback=lambda m: None)
        installer.create_environment()
        completed = MagicMock(
            returncode=0,
            stdout="torch 2.x\ntransformers 4.57.0\nPROBLEM: transformers 4.57.0 is too old for PaddleOCR-VL-1.6 (needs >=5)\n",
            stderr="",
        )
        with patch("mokuro_bunko.ocr.installer.subprocess.run", return_value=completed):
            ok, lines = installer.verify_installation()
        assert not ok
        assert any("too old" in line for line in lines)
        assert "major < 5" in installer._VERIFY_SNIPPET
