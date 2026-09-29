"""A model whose card goes through onnxruntime goes on a card only where that
host's onnxruntime has a GPU execution provider.

The incident this file is built around: ``paddle-manga`` + ``animetext`` ran
fine on this server's own hardware ("detect (cpu x1)"), its per-machine
auto-benchmark "succeeded" on both processors, and then every session there
died at startup with ``--device cuda:0 was asked for, but this onnxruntime has
no GPU execution provider``. Two faults met:

* the auto-benchmark measured the row's own table (``detect: cpu``) but stored
  the runner's ``best`` -- a DELTA against that table, empty because nothing
  moved -- as the machine's whole pools, so the pin vanished from what the
  session was sent (see ``test_remote_bench``/``test_ocr_bench``);
* with the pin gone, ``auto`` resolved to card 0 because the host has a card,
  although the detector's runtime cannot reach it. That is what is tested here,
  at every layer that resolves or offers a device.

Both processors' engines environments had CPU-only onnxruntime wheels; the
stub below offers exactly what theirs offered.
"""

from __future__ import annotations

import json
import sys
import types
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner
from mokuro_bunko.ocr.devices import (
    DeviceCatalog,
    GpuDevice,
    catalog_from_processor,
    merge_catalogs,
    parse_probe,
    set_cached_catalog,
    stage_devices_allowed,
    stage_lock_reason,
)
from mokuro_bunko.ocr.generations import GenerationConfigError, parse_generation_list
from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.remote.profiles import ProcessorProfiles
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.watcher import OCRWorker
from mokuro_bunko.processor.bridge import RunnerBridge
from tests.unit.test_engine_runner import Image, _FakeDetectors, _one_crop_per_line

CPU_ONLY_ORT = ["AzureExecutionProvider", "CPUExecutionProvider"]
CUDA_ORT = ["CUDAExecutionProvider", "CPUExecutionProvider"]
ONE_CARD_DEVICES = [
    {"id": "auto", "label": "Auto — GPU 0 when available"},
    {"id": "cpu", "label": "AMD Ryzen 9 7950X (16 cores)"},
    {"id": "gpu:0", "label": "GPU 0 — AMD Radeon RX 9070 XT (17 GB)"},
]
# `animetext` is the one onnxruntime detector that may go on a card, and it
# is disabled for now (`engines.DISABLED_DETECTORS`): a row naming it no longer
# parses, so the tests that build its ROW -- the library's half of this file --
# are parked with it. The runner's half (no row, just the detector id) runs.
ANIMETEXT_ROW_DISABLED = pytest.mark.skip(
    reason="animetext is disabled for now: a row naming it is refused at parse "
    "(engines.DISABLED_DETECTORS); these come back with the detector"
)
ANIMETEXT_ROWS: list[dict[str, Any]] = [
    {"name": "mokuro", "engine": "mokuro", "primary": True},
    {
        "name": "paddle-manga-animetext",
        "engine": "paddle-manga",
        "detector": "animetext",
        "pools": {"queue_capacity": {"engine": 4, "post": 4}},
    },
]


def _processor_catalog(ort: list[str] | None) -> dict[str, Any]:
    catalog: dict[str, Any] = {
        "engines": ["mokuro", "paddle-manga"],
        "detectors": ["animetext"],
        "devices": ONE_CARD_DEVICES,
        "serves_mokuro": True,
    }
    if ort is not None:
        catalog["onnxruntime_gpu_providers"] = ort
    return catalog


def _stub_ort(monkeypatch: pytest.MonkeyPatch, providers: list[str]) -> None:
    stub = types.ModuleType("onnxruntime")
    stub.get_available_providers = lambda: list(providers)  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "onnxruntime", stub)
    runner.ort_gpu_providers.cache_clear()


@pytest.fixture(autouse=True)
def _clean_probes() -> Any:
    set_cached_catalog(None)
    runner.ort_gpu_providers.cache_clear()
    yield
    set_cached_catalog(None)
    runner.ort_gpu_providers.cache_clear()


@pytest.fixture(autouse=True)
def _restore_log() -> Any:
    yield
    runner.LOG.to_stdout()


# --- the runner: the last line ------------------------------------------------


class TestTheRunnerAsksItsOwnOnnxruntime:
    def test_a_cpu_only_wheel_offers_no_gpu_provider(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _stub_ort(monkeypatch, CPU_ONLY_ORT)
        assert runner.ort_gpu_providers() == ()

    def test_a_cuda_wheel_offers_its_provider(self, monkeypatch: pytest.MonkeyPatch) -> None:
        _stub_ort(monkeypatch, CUDA_ORT)
        assert runner.ort_gpu_providers() == ("CUDAExecutionProvider",)

    def test_no_onnxruntime_at_all_is_not_a_verdict(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setitem(sys.modules, "onnxruntime", None)
        runner.ort_gpu_providers.cache_clear()
        assert runner.ort_gpu_providers() is None

    def test_only_the_onnxruntime_detector_depends_on_it(self) -> None:
        assert runner.stage_needs_ort_gpu(
            runner.ROAD_ADAPTER, runner.STAGE_DETECT, detector="animetext"
        )
        assert not runner.stage_needs_ort_gpu(
            runner.ROAD_ADAPTER, runner.STAGE_DETECT, detector="ctd"
        )
        assert not runner.stage_needs_ort_gpu(
            runner.ROAD_ADAPTER, runner.STAGE_ENGINE, detector="animetext"
        )


class TestTheRunnerPlacesIt:
    def test_auto_is_the_cpu_when_onnxruntime_has_no_gpu(self) -> None:
        specs = runner.road_specs(
            runner.ROAD_ADAPTER, detector="animetext", engine="paddle-manga",
            gpu=True, ort_gpu=False,
        )
        placed = {s.key: s.device for s in specs if s.key in runner.MODEL_STAGES}
        assert placed == {"detect": "cpu", "engine": "gpu:0"}, "the engine keeps the card"

    def test_a_pin_to_a_card_it_cannot_reach_is_the_cpu(self) -> None:
        specs = runner.road_specs(
            runner.ROAD_ADAPTER, detector="animetext", engine="paddle-manga",
            gpu=True, devices={"detect": "gpu:0"}, ort_gpu=False,
        )
        assert next(s.device for s in specs if s.key == "detect") == "cpu"

    def test_an_unknown_runtime_and_a_torch_detector_are_untouched(self) -> None:
        unknown = runner.road_specs(
            runner.ROAD_ADAPTER, detector="animetext", engine="paddle-manga",
            gpu=True, ort_gpu=None,
        )
        assert next(s.device for s in unknown if s.key == "detect") == "gpu:0"
        torch = runner.road_specs(
            runner.ROAD_ADAPTER, detector="ctd", engine="paddle-manga",
            gpu=True, ort_gpu=False,
        )
        assert next(s.device for s in torch if s.key == "detect") == "gpu:0"

    def test_the_benchmark_never_tries_a_card_it_cannot_reach(self) -> None:
        stub = types.SimpleNamespace(
            road=runner.ROAD_ADAPTER, detectors=object(), detector="animetext",
            engine="paddle-manga", ort_gpu=False,
            _stage_device=lambda key: "cpu",
        )
        assert runner.OpenPipeline.detect_devices(stub) == []  # type: ignore[arg-type]


def _run_volume(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    stage_device: str | None,
) -> tuple[int, _FakeDetectors]:
    """One volume through the real runner on the adapter road, with the
    detector refusing a card exactly the way ``detectors/animetext.py`` does
    on a CPU-only onnxruntime."""

    class _AnimeText(_FakeDetectors):
        def open(self, detector: str, *, workers: int, device: str = "") -> Any:
            offered = sys.modules["onnxruntime"].get_available_providers()
            if runner.device_is_gpu(device) and not any(
                p in runner.ORT_GPU_PROVIDERS for p in offered
            ):
                raise runner.DetectorError(
                    f"detector {detector} exited before it was ready (exit code 1)"
                )
            return super().open(detector, workers=workers, device=device)

    class _Recognizer:
        def __call__(self, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
            return ["テスト"] * len(crops)

    input_dir = tmp_path / "Vol"
    input_dir.mkdir()
    (input_dir / "001.webp").write_bytes(b"x")
    detectors = _AnimeText()
    monkeypatch.setattr(runner, "open_detectors", detectors.open)
    monkeypatch.setattr(runner, "host_has_gpu", lambda: True)
    monkeypatch.setattr(runner, "_gpu_count", lambda: 1)
    monkeypatch.setattr(runner, "load_recognizer", lambda *a, **k: _Recognizer())
    monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(100, 200))
    monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _one_crop_per_line)
    monkeypatch.setattr(runner, "make_upright_crop_fn", lambda *a, **k: _one_crop_per_line)
    argv = [
        "--engine", "paddle-manga", "--detector", "animetext",
        "--input", str(input_dir), "--output", str(tmp_path / "out" / "Vol.x.mokuro"),
        "--cache-dir", str(tmp_path / "out" / "_ocr" / "x" / "Vol"),
    ]  # fmt: skip
    if stage_device:
        argv += ["--stage-device", stage_device]
    return runner.run(runner.parse_args(argv)), detectors


class TestTheIncident:
    @ANIMETEXT_ROW_DISABLED
    def test_the_session_a_processor_is_sent_puts_detect_on_the_cpu(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """desktop/tower as found: a card, a CPU-only onnxruntime, and the
        profile entry the auto-benchmark left (all three tables empty).
        local_processing is off, so the row only ever runs there."""
        _stub_ort(monkeypatch, CPU_ONLY_ORT)
        for name in ("library", "inbox"):
            (tmp_path / name).mkdir()
        registry = ProcessorRegistry()
        entry = registry.register(
            username="desktop", name="desktop",
            host={"cpu": "AMD Ryzen 9 7950X (16 cores)", "gpu": "AMD Radeon RX 9070 XT",
                  "backend": "rocm"},
            catalog=_processor_catalog([]), max_sessions=1,
        )
        entry.stream_open = True
        rows = parse_generation_list([dict(r) for r in ANIMETEXT_ROWS])
        worker = OCRWorker(
            storage_path=tmp_path, poll_interval=30.0, generations=rows,
            engines_python_path=Path(sys.executable), remote=registry,
            local_processing=False,
        )
        row = rows[1]
        ProcessorProfiles(tmp_path).set_pools(
            "desktop", row.id,
            {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
            recipe=row.output_affecting(),
        )

        spec = worker._remote_row_spec(entry, row)
        assert spec["pools"]["stage_device"].get("detect") == "cpu"

        # The processor turns it into its own runner command line...
        proc = OCRProcessor(
            storage_path=tmp_path / "processor",
            engines_python_path=Path(sys.executable),
            generations=[RunnerBridge._row(spec)],
        )
        command = proc.session_command(RunnerBridge._row(spec), tmp_path / "session.log")
        assert command[command.index("--stage-device") + 1] == "detect=cpu"

        # ...and a runner started with it gets past the detector's startup.
        code, detectors = _run_volume(
            tmp_path, monkeypatch, command[command.index("--stage-device") + 1]
        )
        assert code == 0
        assert detectors.device == "cpu"

    def test_a_runner_told_the_old_auto_resolves_it_to_the_cpu_itself(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """What the sessions were really given: no --stage-device at all."""
        _stub_ort(monkeypatch, CPU_ONLY_ORT)
        code, detectors = _run_volume(tmp_path, monkeypatch, None)
        assert code == 0
        assert detectors.device == "cpu"

    def test_a_runner_told_the_card_explicitly_warns_and_runs_on_the_cpu(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
    ) -> None:
        """A pin from a library that could not know (an older one, a hand
        edit): the same rule the ppocr pair already has -- a model that cannot
        reach the card resolves to the CPU, and ``ready`` says so."""
        _stub_ort(monkeypatch, CPU_ONLY_ORT)
        code, detectors = _run_volume(tmp_path, monkeypatch, "detect=gpu:0")
        assert code == 0
        assert detectors.device == "cpu"
        out = capsys.readouterr()
        said = [
            line for line in (out.out + out.err).splitlines()
            if "WARN" in line and "onnxruntime" in line
        ]
        assert len(said) == 1, said

    def test_a_host_whose_onnxruntime_has_a_card_keeps_it(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _stub_ort(monkeypatch, CUDA_ORT)
        code, detectors = _run_volume(tmp_path, monkeypatch, None)
        assert code == 0
        assert detectors.device == "gpu:0"


# --- the catalog: what a host reports ----------------------------------------


class TestTheCatalogSaysIt:
    def test_the_probe_asks_onnxruntime_for_its_gpu_providers(self) -> None:
        from mokuro_bunko.ocr.devices import PROBE_SOURCE

        assert "onnxruntime" in PROBE_SOURCE
        assert "get_available_providers" in PROBE_SOURCE

    def test_the_probe_output_carries_them(self) -> None:
        catalog = parse_probe(json.dumps({
            "vendor": "rocm",
            "gpus": [{"index": 0, "name": "RX 9070 XT", "memory_bytes": None}],
            "onnxruntime_gpu_providers": [],
        }))
        assert catalog.ort_gpu is False
        assert parse_probe(json.dumps({"vendor": "", "gpus": []})).ort_gpu is None
        cuda = parse_probe(json.dumps({
            "vendor": "cuda", "gpus": [],
            "onnxruntime_gpu_providers": ["CUDAExecutionProvider"],
        }))
        assert cuda.ort_gpu is True

    def test_a_processors_catalog_carries_them(self) -> None:
        assert catalog_from_processor(_processor_catalog([])).ort_gpu is False
        assert catalog_from_processor(_processor_catalog(["CUDAExecutionProvider"])).ort_gpu
        # A processor from before this key: unknown, and nothing is refused.
        assert catalog_from_processor(_processor_catalog(None)).ort_gpu is None
        assert catalog_from_processor(_processor_catalog([])).knows("gpu:0")

    def test_a_merge_is_as_permissive_as_its_most_capable_machine(self) -> None:
        none = DeviceCatalog(probed=True, ort_gpu_providers=())
        cuda = DeviceCatalog(probed=True, ort_gpu_providers=("CUDAExecutionProvider",))
        unknown = DeviceCatalog(probed=True)
        assert merge_catalogs([none, cuda]).ort_gpu is True
        assert merge_catalogs([none, none]).ort_gpu is False
        assert merge_catalogs([none, unknown]).ort_gpu is None

    def test_the_stage_is_locked_to_the_cpu_with_the_reason(self) -> None:
        catalog = DeviceCatalog(
            gpus=(GpuDevice(0, "RX 9070 XT"),), probed=True, ort_gpu_providers=()
        )
        allowed = stage_devices_allowed(
            runner.ROAD_ADAPTER, "detect", engine="paddle-manga", detector="animetext",
            catalog=catalog,
        )
        assert allowed == ["auto", "cpu"]
        reason = stage_lock_reason(
            runner.ROAD_ADAPTER, "detect", engine="paddle-manga", detector="animetext",
            catalog=catalog,
        )
        assert reason and "onnxruntime" in reason
        # The engine stage of the same row is free: it is torch.
        assert stage_devices_allowed(
            runner.ROAD_ADAPTER, "engine", engine="paddle-manga", detector="animetext",
            catalog=catalog,
        ) == ["auto", "cpu", "gpu:0"]

    def test_a_host_whose_runtime_can_reach_the_card_offers_it(self) -> None:
        catalog = DeviceCatalog(
            gpus=(GpuDevice(0, "RTX 4090"),), probed=True,
            ort_gpu_providers=("CUDAExecutionProvider",),
        )
        assert stage_devices_allowed(
            runner.ROAD_ADAPTER, "detect", engine="paddle-manga", detector="animetext",
            catalog=catalog,
        ) == ["auto", "cpu", "gpu:0"]
        assert stage_lock_reason(
            runner.ROAD_ADAPTER, "detect", engine="paddle-manga", detector="animetext",
            catalog=catalog,
        ) is None

    @ANIMETEXT_ROW_DISABLED
    def test_a_pin_to_a_card_there_is_refused_at_the_edit(self) -> None:
        catalog = DeviceCatalog(
            gpus=(GpuDevice(0, "RX 9070 XT"),), probed=True, ort_gpu_providers=()
        )
        pinned = {**ANIMETEXT_ROWS[1], "pools": {"stage_device": {"detect": "gpu:0"}}}
        with pytest.raises(GenerationConfigError, match="onnxruntime"):
            parse_generation_list([ANIMETEXT_ROWS[0], pinned], devices=catalog)
        # cpu is fine, and an unknown runtime refuses nothing.
        parse_generation_list(
            [ANIMETEXT_ROWS[0], {**pinned, "pools": {"stage_device": {"detect": "cpu"}}}],
            devices=catalog,
        )
        parse_generation_list(
            [ANIMETEXT_ROWS[0], pinned],
            devices=DeviceCatalog(gpus=(GpuDevice(0, "X"),), probed=True),
        )

    @ANIMETEXT_ROW_DISABLED
    def test_the_admin_stages_show_it(self) -> None:
        from mokuro_bunko.admin.api import AdminAPI, _stage_rows

        catalog = DeviceCatalog(
            gpus=(GpuDevice(0, "RX 9070 XT"),), probed=True, ort_gpu_providers=()
        )
        row = parse_generation_list([dict(r) for r in ANIMETEXT_ROWS])[1]
        stages = {s["key"]: s for s in _stage_rows(row, 8, True, catalog)}
        assert stages["detect"]["device"] == "cpu"
        assert stages["detect"]["devices_allowed"] == ["auto", "cpu"]
        assert "onnxruntime" in (stages["detect"]["device_locked_reason"] or "")
        assert stages["engine"]["device"] == "gpu:0"
        detectors = {
            d["id"]: d for d in AdminAPI._generations_catalog(catalog)["detectors"]
        }
        assert detectors["animetext"]["devices"] == ["cpu"]
        assert detectors["ctd"]["devices"] == "any"

    def test_a_processor_reports_its_onnxruntime(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.ocr import bench, installer
        from mokuro_bunko.processor.cli import _catalog
        from mokuro_bunko.processor.config import load_processor_config

        monkeypatch.delenv("MOKURO_PROCESSOR_RUNNER", raising=False)
        monkeypatch.setenv("MOKURO_PROCESSOR_ENGINES_PYTHON", str(tmp_path / "python"))
        monkeypatch.setattr(installer.EnginesInstaller, "is_installed", lambda self: True)
        monkeypatch.setattr(
            installer.EnginesInstaller, "has_detector", lambda self, detector=None: True
        )
        monkeypatch.setattr(installer.OCRInstaller, "is_installed", lambda self: False)
        monkeypatch.setattr(
            bench, "probe_devices",
            lambda python: DeviceCatalog(
                gpus=(GpuDevice(0, "RTX 4090"),), probed=True, ort_gpu_providers=()
            ),
        )
        monkeypatch.setattr(bench, "describe_host", lambda backend, python: {"gpu": None})
        path = tmp_path / "processor.yaml"
        path.write_text(
            "library:\n  url: https://library.example:8080\n  username: tower\n"
            "  password: hunter2hunter2\n",
            encoding="utf-8",
        )
        catalog, _host = _catalog(load_processor_config(path))
        assert catalog["onnxruntime_gpu_providers"] == []
        assert catalog_from_processor(catalog).ort_gpu is False


# --- the library: what it sends --------------------------------------------------


@ANIMETEXT_ROW_DISABLED
class TestTheLibrarySendsOnlyWhatTheHostCanRun:
    @staticmethod
    def _setup(
        tmp_path: Path, ort: list[str] | None, logs: list[str]
    ) -> tuple[OCRWorker, Any, Any]:
        for name in ("library", "inbox"):
            (tmp_path / name).mkdir(exist_ok=True)
        registry = ProcessorRegistry()
        entry = registry.register(
            username="tower", name="tower", host={"cpu": "x (48 cores)"},
            catalog=_processor_catalog(ort), max_sessions=1,
        )
        entry.stream_open = True
        worker = OCRWorker(
            storage_path=tmp_path, poll_interval=30.0,
            generations=parse_generation_list([dict(r) for r in ANIMETEXT_ROWS]),
            engines_python_path=Path(sys.executable), remote=registry,
            local_processing=False, status_callback=logs.append,
        )
        return worker, entry, worker.generations[1]

    def test_the_rows_own_table_goes_out_with_detect_on_the_cpu(
        self, tmp_path: Path
    ) -> None:
        worker, entry, row = self._setup(tmp_path, [], [])
        pools = worker._remote_row_spec(entry, row)["pools"]
        assert pools["stage_device"] == {"detect": "cpu"}
        assert pools["queue_capacity"] == {"engine": 4, "post": 4}, "the rest is the row's"

    def test_a_stored_pin_to_the_card_is_set_aside_and_said_once(
        self, tmp_path: Path
    ) -> None:
        logs: list[str] = []
        worker, entry, row = self._setup(tmp_path, [], logs)
        ProcessorProfiles(tmp_path).set_pools(
            "tower", row.id,
            {"stage_workers": {}, "queue_capacity": {}, "stage_device": {"detect": "gpu:0"}},
            recipe=row.output_affecting(),
        )
        assert worker._remote_row_spec(entry, row)["pools"]["stage_device"] == {"detect": "cpu"}
        worker._remote_row_spec(entry, row)
        said = [line for line in logs if "onnxruntime" in line]
        assert len(said) == 1, logs
        assert "tower" in said[0] and "gpu:0" in said[0]

    def test_a_host_that_can_reach_the_card_is_sent_the_row_unchanged(
        self, tmp_path: Path
    ) -> None:
        worker, entry, row = self._setup(tmp_path, ["CUDAExecutionProvider"], [])
        assert worker._remote_row_spec(entry, row)["pools"] == row.pools.to_dict()

    def test_a_processor_that_never_said_is_sent_the_row_unchanged(
        self, tmp_path: Path
    ) -> None:
        """Its own runner still decides (the tests above): the library does
        not guess about a runtime it was never told about."""
        worker, entry, row = self._setup(tmp_path, None, [])
        assert worker._remote_row_spec(entry, row)["pools"] == row.pools.to_dict()


# --- the profiles the incident left behind ---------------------------------------


@ANIMETEXT_ROW_DISABLED
class TestTheProfilesTheIncidentLeftBehind:
    """desktop.json and tower.json as deployed: the row's own table pins
    ``detect: cpu`` and ``queue_capacity: {engine: 4, post: 4}``, and each
    machine's entry for it holds ``pools`` of three EMPTY tables (the
    auto-benchmark's delta, stored as the whole table). Those tables say
    nothing about the machine, so the row's own table is what goes out --
    on a host whose onnxruntime can reach the card as much as on one whose
    cannot."""

    DEPLOYED_ROW: dict[str, Any] = {
        "name": "paddle-manga-animetext",
        "engine": "paddle-manga",
        "detector": "animetext",
        "patch_budget": 512,
        "pools": {
            "queue_capacity": {"engine": 4, "post": 4},
            "stage_device": {"detect": "cpu"},
            "stage_workers": {},
        },
    }

    def _setup(self, tmp_path: Path, ort: list[str] | None) -> tuple[OCRWorker, Any, Any]:
        for name in ("library", "inbox"):
            (tmp_path / name).mkdir(exist_ok=True)
        registry = ProcessorRegistry()
        entry = registry.register(
            username="desktop", name="desktop", host={"cpu": "x (16 cores)"},
            catalog=_processor_catalog(ort), max_sessions=1,
        )
        entry.stream_open = True
        rows = parse_generation_list([dict(ANIMETEXT_ROWS[0]), dict(self.DEPLOYED_ROW)])
        worker = OCRWorker(
            storage_path=tmp_path, poll_interval=30.0, generations=rows,
            engines_python_path=Path(sys.executable), remote=registry,
            local_processing=False,
        )
        row = worker.generations[1]
        store = ProcessorProfiles(tmp_path)
        store.set_bench("desktop", row.id, {"pages_per_second": 1.0663},
                        recipe=row.output_affecting())
        store.set_pools(
            "desktop", row.id,
            {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}},
            recipe=row.output_affecting(),
        )
        return worker, entry, row

    @pytest.mark.parametrize(
        "ort", [[], ["CUDAExecutionProvider"], None], ids=["cpu-only", "cuda", "never-said"]
    )
    def test_the_row_s_own_table_goes_out(self, tmp_path: Path, ort: list[str] | None) -> None:
        worker, entry, row = self._setup(tmp_path, ort)
        pools = worker._remote_row_spec(entry, row)["pools"]
        assert pools == {
            "stage_workers": {},
            "queue_capacity": {"engine": 4, "post": 4},
            "stage_device": {"detect": "cpu"},
        }

    def test_a_later_edit_of_the_row_reaches_the_machine(self, tmp_path: Path) -> None:
        worker, entry, row = self._setup(tmp_path, ["CUDAExecutionProvider"])
        edited = parse_generation_list([
            dict(ANIMETEXT_ROWS[0]),
            {**self.DEPLOYED_ROW, "pools": {"queue_capacity": {"engine": 8}}},
        ])[1]
        assert worker._remote_row_spec(entry, edited)["pools"]["queue_capacity"] == {
            "engine": 8
        }
