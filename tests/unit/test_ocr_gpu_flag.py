"""Which OCR environments run on a GPU: decided by the backend each one
really runs on (after an install fell back to CPU), not by the backend that
was asked for.

It decides nothing about the queue any more -- row order does that -- but it
is still what the admin panel reports and what the startup warning is built
from, so it must keep telling the truth per environment: mokuro on a GPU
next to a CPU engines environment is a real host.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

from mokuro_bunko.config import Config, OcrConfig, StorageConfig
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.engines import GpuUse, backend_is_gpu
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.installer import HardwareInfo, OCRBackend

ALL = ["mokuro", "hayai-nova", "paddle-manga", "ppocr-manga"]


def _generations(engines: list[str]) -> list[GenerationSpec]:
    """One row per engine, in order, the first of them the primary one."""
    return parse_generation_list(
        [{"engine": engine, "primary": index == 0} for index, engine in enumerate(engines)]
    )


class TestGpuUse:
    def test_backend_names(self) -> None:
        assert backend_is_gpu("cuda") and backend_is_gpu("rocm") and backend_is_gpu("mps")
        assert backend_is_gpu("cpu") is False
        # Names that do not say what the engines run on.
        for unknown in ("auto", "skip", "unknown", "", None):
            assert backend_is_gpu(unknown) is None


class _FakeInstaller:
    """An OCR environment whose torch reports `backend` (None: no answer)."""

    def __init__(self, backend: OCRBackend | None, installed: bool = True) -> None:
        self.backend = backend
        self.installed = installed
        self.probes = 0
        self.env_path = Path("/nonexistent-env")
        self.detector = "ppocr-manga"

    def get_installed_backend(self) -> OCRBackend | None:
        self.probes += 1
        return self.backend

    def is_installed(self) -> bool:
        return self.installed

    def has_detector(self, detector: str | None = None) -> bool:
        return True


def _control(
    selected: str | None, mokuro: OCRBackend | None, engines: OCRBackend | None
) -> tuple[OcrControl, _FakeInstaller, _FakeInstaller]:
    control = OcrControl()
    mokuro_env, engines_env = _FakeInstaller(mokuro), _FakeInstaller(engines)
    control.mokuro_installer = mokuro_env  # type: ignore[assignment]
    control.engines_installer = engines_env  # type: ignore[assignment]
    control.selected_backend = selected
    return control, mokuro_env, engines_env


class TestResolveGpu:
    def test_the_installed_backend_wins_over_the_selected_one(self) -> None:
        # CUDA was selected, its wheels failed, the install fell back to CPU.
        control, _, _ = _control("cuda", OCRBackend.CPU, OCRBackend.CPU)
        assert control.resolve_gpu(ALL) == GpuUse(False, False)
        assert control.gpu == GpuUse(False, False)

    def test_each_environment_is_judged_on_its_own(self) -> None:
        control, _, _ = _control("cuda", OCRBackend.CPU, OCRBackend.CUDA)
        assert control.resolve_gpu(ALL) == GpuUse(mokuro_env=False, engines_env=True)
        control, _, _ = _control("rocm", OCRBackend.ROCM, OCRBackend.CPU)
        assert control.resolve_gpu(ALL) == GpuUse(mokuro_env=True, engines_env=False)

    def test_a_gpu_torch_under_a_cpu_selection_still_runs_on_the_gpu(self) -> None:
        # Nothing tells the engines to stay off the GPU: the torch build decides.
        control, _, _ = _control("cpu", OCRBackend.CUDA, OCRBackend.CPU)
        assert control.resolve_gpu(ALL) == GpuUse(mokuro_env=True, engines_env=False)

    def test_an_environment_that_does_not_answer_is_taken_at_the_selected_backend(self) -> None:
        control, _, _ = _control("cuda", None, None)
        assert control.resolve_gpu(ALL) == GpuUse(True, True)
        control, _, _ = _control("cpu", None, None)
        assert control.resolve_gpu(ALL) == GpuUse(False, False)

    def test_only_the_environments_in_use_are_probed_and_only_once(self) -> None:
        # The probe imports torch in a subprocess: seconds, not microseconds.
        control, mokuro_env, engines_env = _control("cuda", OCRBackend.CUDA, OCRBackend.CPU)
        assert control.resolve_gpu(["mokuro"]) == GpuUse(mokuro_env=True, engines_env=True)
        assert (mokuro_env.probes, engines_env.probes) == (1, 0)
        assert control.resolve_gpu(["mokuro", "hayai-nova"]) == GpuUse(True, False)
        assert control.resolve_gpu(["mokuro", "hayai-nova"]) == GpuUse(True, False)
        assert (mokuro_env.probes, engines_env.probes) == (1, 1)

    def test_no_decision_without_a_backend(self) -> None:
        control, mokuro_env, _ = _control(None, OCRBackend.CUDA, OCRBackend.CUDA)
        assert control.resolve_gpu(ALL) is None
        assert control.gpu is None
        assert mokuro_env.probes == 0


class TestRunServerWiring:
    """run_server reports the backend in use, not the one it selected."""

    def _run(
        self,
        tmp_path: Path,
        engines: list[str],
        mokuro_env: OCRBackend | None,
        engines_env: OCRBackend | None,
        preinstalled: bool = False,
        remote: Any = None,
        server: Any = None,
    ) -> tuple[MagicMock, OcrControl]:
        from mokuro_bunko import server as server_module

        def installer(backend: OCRBackend | None) -> MagicMock:
            fake = MagicMock()
            fake.env_path = tmp_path / "env"
            fake.detectors = ["ppocr-manga"]
            # Not installed yet: the install "succeeds" by falling back.
            fake.is_installed.side_effect = lambda: (
                preinstalled or fake.install_with_fallback.called
            )
            fake.needs_rebuild_for.return_value = False
            fake.install_with_fallback.return_value = True
            fake.has_detector.return_value = True
            fake.get_installed_backend.return_value = backend
            return fake

        mokuro_installer, engines_installer = installer(mokuro_env), installer(engines_env)
        # ROCm rather than CUDA: `auto` refuses CUDA on the newest Pythons.
        hardware = HardwareInfo(
            has_cuda=False, has_rocm=True, has_mps=False, cuda_version=None, rocm_version="6.2"
        )
        worker_cls = MagicMock()
        controls: list[OcrControl] = []

        def fake_server(*args: Any, **kwargs: Any) -> MagicMock:
            controls.append(kwargs["ocr_control"])
            if remote is not None:
                # What `create_app` does: the registry rides on the control.
                kwargs["ocr_control"].remote = remote
            return server if server is not None else MagicMock()

        config = Config(
            storage=StorageConfig(base_path=tmp_path),
            ocr=OcrConfig(backend="auto", generations=_generations(engines)),
        )
        with (
            patch.object(server_module, "_validate_startup_environment"),
            patch("mokuro_bunko.logging_setup.setup_logging", return_value=None),
            patch.object(server_module, "create_ssl_server", side_effect=fake_server),
            patch.object(server_module, "_start_server_resilient"),
            patch("mokuro_bunko.cheroot_watchdog.ThreadPoolWatchdog"),
            patch("mokuro_bunko.ocr.installer.detect_hardware", return_value=hardware),
            patch("mokuro_bunko.ocr.installer.OCRInstaller", return_value=mokuro_installer),
            patch("mokuro_bunko.ocr.installer.EnginesInstaller", return_value=engines_installer),
            patch("mokuro_bunko.ocr.watcher.OCRWorker", worker_cls),
        ):
            server_module.run_server(config)
        return worker_cls, controls[0]

    def test_shutdown_drops_every_processor_before_stopping_the_server(
        self, tmp_path: Path
    ) -> None:
        """Found on real hardware: Ctrl-C printed "Shutting down..." and the
        process never exited while a processor was connected -- its stream
        is a worker thread that yields a heartbeat every 15 s to a reader
        that never stops reading, so nothing ends it. Dropping every entry
        first puts the sentinel on each stream, which then ends."""
        order = MagicMock()
        self._run(
            tmp_path, ["mokuro"], OCRBackend.ROCM, None, preinstalled=True,
            remote=order.remote, server=order.server,
        )
        calls = [c[0] for c in order.mock_calls]
        assert "remote.drop_all" in calls, calls
        assert calls.index("remote.drop_all") < calls.index("server.stop")

    def test_a_gpu_selected_but_the_install_fell_back_to_cpu(self, tmp_path: Path) -> None:
        # The engines environment gives no answer, so it stays at the
        # selected backend; the mokuro one says CPU and is believed.
        worker_cls, control = self._run(tmp_path, ["mokuro", "ppocr-manga"], OCRBackend.CPU, None)
        kwargs = worker_cls.call_args.kwargs
        # The worker is handed the ROWS, in the order the operator listed
        # them: nothing reorders them by what their environment runs on.
        assert [row.engine for row in kwargs["generations"]] == ["mokuro", "ppocr-manga"]
        assert control.gpu == GpuUse(mokuro_env=False, engines_env=True)
        assert control.selected_backend == "rocm"

    def test_a_gpu_selected_and_installed(self, tmp_path: Path) -> None:
        _, control = self._run(
            tmp_path, ["mokuro", "hayai-nova"], OCRBackend.ROCM, OCRBackend.ROCM, preinstalled=True
        )
        assert control.gpu == GpuUse(True, True)

    def test_only_the_engines_environment_fell_back(self, tmp_path: Path) -> None:
        _, control = self._run(tmp_path, ["mokuro", "hayai-nova"], OCRBackend.ROCM, OCRBackend.CPU)
        assert control.gpu == GpuUse(mokuro_env=True, engines_env=False)
