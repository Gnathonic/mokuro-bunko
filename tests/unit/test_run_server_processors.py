"""A library box that does no OCR of its own still hands work to processors (I4).

Spec section 0: "a weak box sets it off". With `ocr.local_processing: false`
-- or `ocr.backend: skip`, the natural setting for a box that must not OCR --
`run_server` must not install, probe or narrow anything for local OCR: every
configured row stays active (each processor's catalog decides what it is
offered), and the full worker is built so processors are offered the queue.
Tested at the `run_server` level, because that is where it went wrong: a test
that builds `OCRWorker` by hand cannot see it.
"""

from __future__ import annotations

import json
import sys
import zipfile
from pathlib import Path
from typing import Any
from unittest.mock import MagicMock, patch

from mokuro_bunko.config import Config, OcrConfig, StorageConfig
from mokuro_bunko.ocr.control import OcrControl
from mokuro_bunko.ocr.devices import (
    DeviceCatalog,
    GpuDevice,
    cached_catalog,
    set_cached_catalog,
)
from mokuro_bunko.ocr.generations import GenerationSpec, parse_generation_list
from mokuro_bunko.ocr.installer import HardwareInfo
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry

ROWS = [
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {"name": "hayai-ctd", "engine": "hayai-nova", "detector": "ctd"},
]


def _rows() -> list[GenerationSpec]:
    return parse_generation_list([dict(row) for row in ROWS])


def _untouchable(what: str) -> MagicMock:
    """An installer that fails the test the moment anything asks it anything."""
    return MagicMock(side_effect=AssertionError(f"{what} was built on a box that runs no OCR"))


def _run(
    tmp_path: Path, ocr: OcrConfig, *, installers: tuple[Any, Any] | None = None,
    probe: DeviceCatalog | None = None, seen_at_start: list[DeviceCatalog] | None = None,
) -> tuple[MagicMock, OcrControl]:
    from mokuro_bunko import server as server_module

    worker_cls = MagicMock()
    # What this server's cards were known to be when the worker started.
    worker_cls.return_value.start.side_effect = lambda *a, **k: (
        seen_at_start.append(cached_catalog()) if seen_at_start is not None else None
    )
    controls: list[OcrControl] = []

    def fake_server(*args: Any, **kwargs: Any) -> MagicMock:
        control = kwargs["ocr_control"]
        controls.append(control)
        control.remote = ProcessorRegistry()
        return MagicMock()

    config = Config(storage=StorageConfig(base_path=tmp_path), ocr=ocr)
    mokuro_installer, engines_installer = installers or (
        _untouchable("the mokuro installer"), _untouchable("the engines installer")
    )
    hardware = HardwareInfo(
        has_cuda=False, has_rocm=True, has_mps=False, cuda_version=None, rocm_version="6.2"
    )
    with (
        patch.object(server_module, "_validate_startup_environment"),
        patch("mokuro_bunko.logging_setup.setup_logging", return_value=None),
        patch.object(server_module, "create_ssl_server", side_effect=fake_server),
        patch.object(server_module, "_start_server_resilient"),
        patch("mokuro_bunko.cheroot_watchdog.ThreadPoolWatchdog"),
        patch("mokuro_bunko.ocr.installer.detect_hardware", return_value=hardware),
        patch("mokuro_bunko.ocr.installer.OCRInstaller", mokuro_installer),
        patch("mokuro_bunko.ocr.installer.EnginesInstaller", engines_installer),
        patch("mokuro_bunko.ocr.watcher.OCRWorker", worker_cls),
        patch("mokuro_bunko.ocr.bench.probe_devices",
              return_value=probe if probe is not None else DeviceCatalog()),
    ):
        try:
            server_module.run_server(config)
        finally:
            set_cached_catalog(None)
    return worker_cls, controls[0]


def _assert_processors_only(worker_cls: MagicMock, control: OcrControl) -> None:
    assert worker_cls.call_count == 1
    kwargs = worker_cls.call_args.kwargs
    assert kwargs.get("thumbnails_only") in (None, False), "the FULL worker, not covers-only"
    assert kwargs["local_processing"] is False
    assert [row.name for row in kwargs["generations"]] == ["mokuro", "hayai-ctd"], (
        "every configured row stays active: processor catalogs decide"
    )
    assert kwargs["remote"] is control.remote
    worker = worker_cls.return_value
    assert control.remote.on_drop == worker.processor_disconnected, (
        "a processor that drops gets its claims back"
    )
    assert control.worker is worker
    assert worker.start.called


class TestABoxThatRunsNoOcr:
    def test_local_processing_off_installs_nothing_and_offers_every_row(
        self, tmp_path: Path
    ) -> None:
        worker_cls, control = _run(
            tmp_path,
            OcrConfig(backend="auto", generations=_rows(), local_processing=False),
        )
        _assert_processors_only(worker_cls, control)

    def test_backend_skip_is_the_same_box(self, tmp_path: Path) -> None:
        worker_cls, control = _run(
            tmp_path, OcrConfig(backend="skip", generations=_rows())
        )
        _assert_processors_only(worker_cls, control)

    def test_a_box_whose_every_local_install_failed_still_hands_out_work(
        self, tmp_path: Path
    ) -> None:
        def installer() -> MagicMock:
            fake = MagicMock()
            fake.env_path = tmp_path / "env"
            fake.detectors = ["ctd"]
            fake.is_installed.return_value = False
            fake.needs_rebuild_for.return_value = False
            fake.install_with_fallback.return_value = False
            fake.has_detector.return_value = False
            fake.get_installed_backend.return_value = None
            return fake

        mokuro, engines = installer(), installer()
        worker_cls, control = _run(
            tmp_path,
            OcrConfig(backend="auto", generations=_rows()),
            installers=(MagicMock(return_value=mokuro), MagicMock(return_value=engines)),
        )
        _assert_processors_only(worker_cls, control)

    def test_with_local_processing_on_this_box_still_installs_and_runs(
        self, tmp_path: Path
    ) -> None:
        def installer() -> MagicMock:
            fake = MagicMock()
            fake.env_path = tmp_path / "env"
            fake.detectors = ["ctd"]
            fake.is_installed.return_value = True
            fake.needs_rebuild_for.return_value = False
            fake.has_detector.return_value = True
            fake.get_installed_backend.return_value = None
            return fake

        worker_cls, _control = _run(
            tmp_path,
            OcrConfig(backend="auto", generations=_rows()),
            installers=(MagicMock(return_value=installer()),
                        MagicMock(return_value=installer())),
        )
        assert worker_cls.call_args.kwargs["local_processing"] is True


class TestSettingsReachProcessorsWithoutLocalEnvironments:
    def test_an_edit_applies_at_once_when_this_box_runs_no_ocr(self) -> None:
        control = OcrControl()
        worker = MagicMock(thumbnails_only=False, local_processing=False)
        control.worker = worker
        # No installers at all: nothing local is installed, nothing needs to be.
        outcome = control.apply(_rows())
        assert outcome["applied"] is True
        assert outcome["restart_required"] is False
        worker.apply_settings.assert_called_once()


def _installer(tmp_path: Path, *, installed: bool = True, detectors_ok: bool = True) -> MagicMock:
    fake = MagicMock()
    fake.env_path = tmp_path / "env"
    fake.detectors = ["ctd"]
    fake.is_installed.return_value = installed
    fake.needs_rebuild_for.return_value = False
    fake.has_detector.return_value = detectors_ok
    fake.install_detector.return_value = detectors_ok
    fake.get_installed_backend.return_value = None
    return fake


class TestThisServerIsJustAnotherProcessorEntry:
    """B10, spec section 0: a local install that fails takes the rows that
    need it off THIS server's slots -- never out of the queue a processor
    with that environment would serve."""

    def test_a_row_this_box_could_not_install_for_stays_in_the_queue(
        self, tmp_path: Path
    ) -> None:
        worker_cls, control = _run(
            tmp_path,
            OcrConfig(backend="auto", generations=_rows()),
            installers=(MagicMock(return_value=_installer(tmp_path)),
                        MagicMock(return_value=_installer(tmp_path, detectors_ok=False))),
        )
        kwargs = worker_cls.call_args.kwargs
        assert [row.name for row in kwargs["generations"]] == ["mokuro", "hayai-ctd"], (
            "the ctd row is still the queue's: a processor with ctd runs it"
        )
        assert kwargs["local_processing"] is True
        assert "ctd" in kwargs["local_unavailable"]["detector:ctd"]
        assert control.runtime is not None
        assert [row["name"] for row in control.runtime["active_generations"]] == ["mokuro"], (
            "what runs HERE is still reported as such"
        )

    def test_the_local_slot_leaves_that_row_to_a_processor(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.watcher import OCRWorker

        for name in ("library", "inbox"):
            (tmp_path / name).mkdir()
        cbz = tmp_path / "library" / "Alpha" / "Volume 1.cbz"
        cbz.parent.mkdir()
        with zipfile.ZipFile(cbz, "w") as zf:
            zf.writestr("page_000.jpg", b"fake image data")
        cbz.with_suffix(".mokuro").write_text(
            json.dumps({"version": "0.0", "volume_uuid": "u", "pages": [], "chars": 0}),
            encoding="utf-8",
        )
        registry = ProcessorRegistry(local_name="this server")
        tower = registry.register(
            username="tower", name="tower", host={},
            catalog={"engines": ["mokuro", "hayai-nova"], "detectors": ["ctd"],
                     "devices": [], "serves_mokuro": True},
            max_sessions=1,
        )
        tower.stream_open = True
        worker = OCRWorker(
            storage_path=tmp_path, poll_interval=30.0, generations=_rows(),
            engines_python_path=Path(sys.executable), remote=registry,
            local_processing=True, autobench=False,
            local_unavailable={"detector:ctd": "the ctd detector failed to install here"},
        )
        local = next(s for s in worker._all_slots() if s.processor_id == "local")
        remote = next(s for s in worker._all_slots() if s.processor_id != "local")
        assert worker.claim_next(local) is None, "this server cannot run the ctd row"
        job = worker.claim_next(remote)
        assert job is not None and job[1] == worker.generations[1].id

    def test_this_servers_cards_are_known_before_the_first_claim(
        self, tmp_path: Path
    ) -> None:
        """B4: the local slot declines a row pinned to a card only a
        processor has by the PROBED catalog; unprobed, it "knows" every
        gpu:<n>, and nothing probed until an admin opened the page."""
        one_card = DeviceCatalog(gpus=(GpuDevice(index=0, name="RX 9070 XT"),), probed=True)
        seen: list[DeviceCatalog] = []
        _run(
            tmp_path,
            OcrConfig(backend="auto", generations=_rows()),
            installers=(MagicMock(return_value=_installer(tmp_path)),
                        MagicMock(return_value=_installer(tmp_path))),
            probe=one_card,
            seen_at_start=seen,
        )
        assert seen == [one_card]
        assert seen[0].knows("gpu:1") is False


class TestALocalGapNeverHoldsARowFromProcessors:
    def _control(self, tmp_path: Path, rows: list[GenerationSpec]) -> tuple[OcrControl, Any]:
        from mokuro_bunko.ocr.watcher import OCRWorker

        for name in ("library", "inbox"):
            (tmp_path / name).mkdir(exist_ok=True)
        worker = OCRWorker(
            storage_path=tmp_path, poll_interval=30.0, generations=rows,
            engines_python_path=Path(sys.executable), remote=ProcessorRegistry(),
            local_processing=True, autobench=False,
        )
        control = OcrControl()
        control.worker = worker
        control.mokuro_installer = _installer(tmp_path)  # type: ignore[assignment]
        return control, worker

    def test_a_missing_environment_here_still_applies_the_row(self, tmp_path: Path) -> None:
        rows = _rows()
        control, worker = self._control(tmp_path, rows[:1])
        control.engines_installer = _installer(tmp_path, installed=False)  # type: ignore[assignment]
        out = control.apply(rows)
        assert out["applied"] is True, "a processor may run it now"
        assert out["restart_required"] is True, "this server needs a restart to"
        assert "hayai-ctd" in out["reason"]
        assert [row.name for row in worker.generations] == ["mokuro", "hayai-ctd"]
        assert "engines" in worker.local_unavailable

    def test_a_detector_that_will_not_install_here_still_reaches_processors(
        self, tmp_path: Path
    ) -> None:
        rows = _rows()
        control, worker = self._control(tmp_path, rows[:1])
        control.engines_installer = _installer(tmp_path, detectors_ok=False)  # type: ignore[assignment]
        out = control.apply(rows)
        assert out["installing"] is True
        assert control._install_thread is not None
        control._install_thread.join(timeout=5)
        assert [row.name for row in worker.generations] == ["mokuro", "hayai-ctd"]
        assert "detector:ctd" in worker.local_unavailable
