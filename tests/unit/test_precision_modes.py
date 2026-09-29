"""A generation's precision MODE: one per row, for every machine.

Values: ``auto-accuracy`` (the default), ``auto-balanced``, ``auto-speed`` and
the forced ``fp32``, ``bf16``, ``fp16``. Each machine resolves the mode from
its own card (`engine_runner.resolve_mode`); a machine that cannot run a
forced format is not eligible for the row at all -- never offered its
volumes, never benchmarked for it -- and a row nobody can run is held.
"""

from __future__ import annotations

import json
import logging
import sys
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.config import Config
from mokuro_bunko.ocr import engine_runner as runner
from mokuro_bunko.ocr.devices import (
    GPU_FACTS_KEY,
    DeviceCatalog,
    GpuDevice,
    catalog_from_processor,
    set_cached_catalog,
)
from mokuro_bunko.ocr.engine_runner import (
    DEFAULT_PRECISION_MODE,
    PRECISION_MODES,
    PRECISION_REFUSAL,
    PrecisionUnavailable,
    normalize_precision_mode,
    pick_precision,
    resolve_mode,
    resolve_precision,
)
from mokuro_bunko.ocr.generations import GenerationConfigError, parse_generation_list
from mokuro_bunko.ocr.remote import profiles as profiles_module
from mokuro_bunko.ocr.remote.profiles import (
    ProcessorProfiles,
    holds_pools,
    machine_pools,
    runner_pools,
)
from mokuro_bunko.ocr.remote.registry import ProcessorRegistry
from mokuro_bunko.ocr.remote.scheduler import catalog_can_run
from mokuro_bunko.ocr.watcher import OCRWorker

PRIMARY: dict[str, Any] = {"name": "mokuro", "engine": "mokuro", "primary": True}
HAYAI: dict[str, Any] = {"name": "hayai", "engine": "hayai-nova", "detector": "ctd"}
PADDLE: dict[str, Any] = {"name": "paddle", "engine": "paddle-manga", "detector": "ctd"}

# What a device supports, by the runtime probe.
BF16 = frozenset({"fp32", "fp16", "bf16"})   # a card torch says runs bf16
GPU = frozenset({"fp32", "fp16"})            # a card without bf16
CPU = frozenset({"fp32"})

# What a processor registers about its cards (the optional ``gpus`` field of a
# protocol-2 registration's catalog): the probe's formats, per card.
FAST = [{"index": 0, "formats": {"bf16": True, "fp16": True}}]
SLOW = [{"index": 0, "formats": {"bf16": False, "fp16": True}}]
DEVICES = [{"id": "auto", "label": "Auto"}, {"id": "cpu", "label": "CPU"},
           {"id": "gpu:0", "label": "GPU 0"}]


def _catalog(gpus: list[dict[str, Any]] | None, **extra: Any) -> dict[str, Any]:
    catalog: dict[str, Any] = {
        "engines": ["mokuro", "hayai-nova", "paddle-manga"], "detectors": ["ctd"],
        "devices": DEVICES, "serves_mokuro": True, **extra,
    }
    if gpus is not None:
        catalog[GPU_FACTS_KEY] = gpus
    return catalog


def _card(bf16: bool) -> DeviceCatalog:
    """This server's own probe: one card, with or without bf16."""
    formats = frozenset({"fp16", "bf16"}) if bf16 else frozenset({"fp16"})
    return DeviceCatalog(gpus=(GpuDevice(0, "card", formats=formats),), vendor="rocm", probed=True)


def _rows(*extra: dict[str, Any]) -> Any:
    return parse_generation_list([dict(PRIMARY), *[dict(r) for r in extra]],
                                 devices=DeviceCatalog())


@pytest.fixture(autouse=True)
def _no_published_catalog() -> Any:
    set_cached_catalog(None)
    yield
    set_cached_catalog(None)


# --- the resolution table ------------------------------------------------

ENGINES = ("hayai-nova", "paddle-manga", "mokuro")
# mode -> device -> (hayai-nova, paddle-manga, mokuro), before any benchmark;
# None = not eligible there.
TABLE: dict[str, dict[frozenset[str], tuple[str | None, ...]]] = {
    "auto-accuracy": {BF16: ("bf16", "fp32", "fp32"), GPU: ("fp32", "fp32", "fp32"),
                      CPU: ("fp32", "fp32", "fp32")},
    "auto-balanced": {BF16: ("bf16", "bf16", "fp32"), GPU: ("fp32", "fp32", "fp32"),
                      CPU: ("fp32", "fp32", "fp32")},
    "auto-speed": {BF16: ("bf16", "bf16", "fp16"), GPU: ("fp16", "fp16", "fp16"),
                   CPU: ("fp32", "fp32", "fp32")},
    "fp32": {BF16: ("fp32", "fp32", "fp32"), GPU: ("fp32", "fp32", "fp32"),
             CPU: ("fp32", "fp32", "fp32")},
    "bf16": {BF16: ("bf16", "bf16", None), GPU: (None, None, None), CPU: (None, None, None)},
    "fp16": {BF16: ("fp16", "fp16", "fp16"), GPU: ("fp16", "fp16", "fp16"),
             CPU: (None, None, None)},
}


class TestTheResolutionTable:
    def test_the_modes_and_the_default(self) -> None:
        assert PRECISION_MODES == (
            "auto-accuracy", "auto-balanced", "auto-speed", "fp32", "bf16", "fp16",
        )
        assert DEFAULT_PRECISION_MODE == "auto-accuracy"

    def test_the_policy_is_one_table_of_candidate_lists(self) -> None:
        assert runner.PRECISION_POLICY == {
            "hayai-nova": {"auto-accuracy": ("bf16", "fp32"), "auto-balanced": ("bf16", "fp32"),
                           "auto-speed": ("bf16", "fp16", "fp32")},
            "paddle-manga": {"auto-accuracy": ("fp32",), "auto-balanced": ("bf16", "fp32"),
                             "auto-speed": ("bf16", "fp16", "fp32")},
            "mokuro": {"auto-accuracy": ("fp32",), "auto-balanced": ("fp32",),
                       "auto-speed": ("fp16", "fp32")},
        }

    @pytest.mark.parametrize("mode", list(TABLE))
    @pytest.mark.parametrize("device", [BF16, GPU, CPU], ids=["bf16-card", "gpu", "cpu"])
    def test_every_engine_and_mode_on_every_device(self, mode: str, device: frozenset[str]) -> None:
        for engine, expected in zip(ENGINES, TABLE[mode][device], strict=True):
            resolved = resolve_mode(engine, mode, device)
            assert resolved.precision == expected, (engine, mode, sorted(device), resolved)
            assert resolved.eligible is (expected is not None), (engine, mode, sorted(device))
            assert resolved.why

    @pytest.mark.parametrize("mode", PRECISION_MODES)
    @pytest.mark.parametrize("device", [BF16, GPU, CPU, None])
    def test_an_engine_that_fixes_its_own_precision_ignores_the_mode(
        self, mode: str, device: frozenset[str] | None
    ) -> None:
        resolved = resolve_mode("ppocr-manga", mode, device)
        assert resolved.eligible and resolved.precision is None

    def test_accuracy_never_uses_fp16(self) -> None:
        for engine in ENGINES:
            for device in (BF16, GPU, CPU):
                assert resolve_mode(engine, "auto-accuracy", device).precision != "fp16"

    def test_mokuro_runs_no_bf16(self) -> None:
        assert runner.engine_modes("mokuro") == (
            "auto-accuracy", "auto-balanced", "auto-speed", "fp32", "fp16",
        )
        assert not resolve_mode("mokuro", "bf16", BF16).eligible


class TestTheBenchmarkedPick:
    def test_emulated_bf16_slower_than_fp32_picks_fp32(self) -> None:
        chosen, why = pick_precision([("bf16", 0.32), ("fp32", 0.57)], ("bf16", "fp32")) or ("", "")
        assert chosen == "fp32"
        assert why == "benchmark: fp32 0.57 p/s beat bf16 0.32 p/s"

    def test_a_tie_within_five_percent_keeps_the_earlier_candidate(self) -> None:
        assert pick_precision([("bf16", 1.00), ("fp16", 1.04), ("fp32", 0.6)],
                              ("bf16", "fp16", "fp32"))[0] == "bf16"  # type: ignore[index]
        assert pick_precision([("bf16", 1.00), ("fp16", 1.06), ("fp32", 0.6)],
                              ("bf16", "fp16", "fp32"))[0] == "fp16"  # type: ignore[index]

    def test_before_any_benchmark_the_first_supported_candidate_runs(self) -> None:
        resolved = resolve_mode("paddle-manga", "auto-speed", GPU)
        assert (resolved.precision, resolved.usable) == ("fp16", ("fp16", "fp32"))
        assert "not benchmarked yet" in resolved.why

    def test_a_pick_is_honoured_while_it_is_still_a_usable_candidate(self) -> None:
        picked = resolve_mode("paddle-manga", "auto-balanced", BF16, pick="fp32",
                              pick_why="benchmark: fp32 0.57 p/s beat bf16 0.32 p/s")
        assert picked.precision == "fp32" and picked.why.startswith("benchmark:")
        # ... never once the device no longer supports it
        assert resolve_mode("paddle-manga", "auto-speed", GPU, pick="bf16").precision == "fp16"

    def test_the_bench_tries_every_supported_candidate_and_keeps_the_fastest(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """The runner's precision trials, with the pipeline stood in for."""
        import argparse
        import types

        rates = {"bf16": 0.32, "fp32": 0.57}
        switched: list[str] = []

        class _Target:
            precision = "fp32"

            def supported(self) -> frozenset[str]:
                return BF16

            def set_precision(self, name: str) -> None:
                switched.append(name)
                self.precision = name

            def release_master(self) -> None:
                switched.append("released")

        target = _Target()
        args = argparse.Namespace(engine="paddle-manga", precision="auto-balanced")
        bench = runner.BenchRun(args, protocol=types.SimpleNamespace(emit=lambda *a, **k: None))
        bench.pipe = types.SimpleNamespace(  # type: ignore[assignment]
            precision_target=lambda: target, loader=None, specs=[],
        )
        monkeypatch.setattr(bench, "_map", lambda widths: {})

        def trial(widths: Any, *, note: str) -> Any:
            n = len(bench.trials) + 1
            made = types.SimpleNamespace(
                n=n, precision=bench.precision, pages_per_second=rates[bench.precision or ""],
                _replace=lambda **kw: made,
            )
            bench.trials.append(made)
            return made

        monkeypatch.setattr(bench, "_trial", trial)
        monkeypatch.setattr(bench, "_emit", lambda made, accepted: made)
        winner = bench.precision_phase([1])
        assert winner is not None and winner.precision == "fp32"
        assert bench.precision == "fp32"
        assert switched == ["bf16", "fp32", "fp32", "released"]
        assert bench.precision_why == "benchmark: fp32 0.57 p/s beat bf16 0.32 p/s"


class TestTheRunnerResolves:
    def test_it_says_the_mode_and_the_pick(self) -> None:
        assert resolve_precision("hayai-nova", "auto-accuracy", supported=BF16) == (
            "bf16", "auto-accuracy",
        )
        assert resolve_precision(
            "paddle-manga", "auto-balanced", supported=BF16, pick="fp32",
            pick_why="benchmark: fp32 0.57 p/s beat bf16 0.32 p/s",
        ) == ("fp32", "auto-balanced; benchmark: fp32 0.57 p/s beat bf16 0.32 p/s")

    def test_legacy_auto_is_the_default(self) -> None:
        assert normalize_precision_mode("auto") == "auto-accuracy"
        assert resolve_precision("hayai-nova", "auto", supported=BF16)[0] == "bf16"

    @pytest.mark.parametrize(
        ("mode", "device"), [("bf16", GPU), ("bf16", CPU), ("fp16", CPU)],
    )
    def test_a_forced_format_the_device_does_not_support_is_refused(
        self, mode: str, device: frozenset[str]
    ) -> None:
        with pytest.raises(PrecisionUnavailable) as caught:
            resolve_precision("paddle-manga", mode, supported=device)
        assert PRECISION_REFUSAL in str(caught.value)
        assert mode in str(caught.value)

    def test_forced_bf16_on_a_card_that_supports_it_slowly_still_runs(self) -> None:
        # An RX 6000 reports bf16 (emulated): forced bf16 is the user's call.
        assert resolve_precision("paddle-manga", "bf16", supported=BF16)[0] == "bf16"

    def test_the_probe_asks_torch_not_a_list_of_cards(self) -> None:
        import contextlib
        import types

        def fake(bf16: bool) -> Any:
            return types.SimpleNamespace(cuda=types.SimpleNamespace(
                device=lambda index: contextlib.nullcontext(),
                is_bf16_supported=lambda: bf16,
            ))

        assert runner.supported_formats(fake(True), "cuda:0") == BF16
        assert runner.supported_formats(fake(False), "cuda:1") == GPU
        assert runner.supported_formats(fake(True), "cpu") == CPU

    def test_the_flags(self) -> None:
        base = ["--engine", "hayai-nova", "--input", "i", "--output", "o", "--cache-dir", "c"]
        assert runner.parse_args(base).precision == "auto-accuracy"
        for mode in (*PRECISION_MODES, "auto"):
            assert runner.parse_args([*base, "--precision", mode]).precision == mode
        args = runner.parse_args([*base, "--precision", "auto-speed", "--precision-pick", "fp16",
                                  "--precision-why", "benchmark: fp16 2.00 p/s"])
        config = runner.SessionConfig.from_args(args)
        assert (config.precision, config.precision_pick, config.precision_why) == (
            "auto-speed", "fp16", "benchmark: fp16 2.00 p/s",
        )


# --- where the setting lives ----------------------------------------------


class TestTheRowsSetting:
    def test_absent_is_accuracy(self) -> None:
        row = _rows(PADDLE)[1]
        assert row.precision == "auto-accuracy"
        assert "precision" not in row.to_dict()

    @pytest.mark.parametrize("mode", PRECISION_MODES)
    def test_a_top_level_mode_round_trips(self, mode: str) -> None:
        row = _rows({**PADDLE, "precision": mode})[1]
        assert row.precision == mode
        assert _rows(row.to_dict())[1].precision == mode

    def test_legacy_auto_reads_as_the_default(self) -> None:
        assert normalize_precision_mode("auto") == "auto-accuracy"
        assert _rows({**PADDLE, "precision": "auto"})[1].precision == "auto-accuracy"
        assert _rows({**PADDLE, "pools": {"precision": "auto"}})[1].precision == "auto-accuracy"

    def test_a_legacy_row_level_pin_becomes_the_forced_mode(self) -> None:
        row = _rows({**PADDLE, "pools": {"precision": "bf16"}})[1]
        assert row.precision == "bf16"
        assert "precision" not in row.pools.to_dict()
        assert row.to_dict()["precision"] == "bf16"

    def test_the_top_level_key_wins_over_a_legacy_pin(self) -> None:
        row = _rows({**PADDLE, "precision": "fp32", "pools": {"precision": "bf16"}})[1]
        assert row.precision == "fp32"

    def test_an_unknown_mode_is_refused_naming_the_choices(self) -> None:
        with pytest.raises(GenerationConfigError) as caught:
            _rows({**PADDLE, "precision": "int8"})
        assert caught.value.field == "precision"
        assert "auto-balanced" in str(caught.value) and "fp16" in str(caught.value)

    def test_an_engine_with_a_fixed_precision_ignores_it(self) -> None:
        row = _rows({"name": "pp", "engine": "ppocr-manga", "precision": "bf16"})[1]
        assert row.precision == "auto-accuracy"
        assert not row.precision_applies

    def test_mokuro_takes_a_mode_but_never_bf16(self) -> None:
        mokuro = {"name": "m2", "engine": "mokuro"}
        row = _rows({**mokuro, "precision": "auto-speed"})[1]
        assert row.precision_applies and row.precision == "auto-speed"
        with pytest.raises(GenerationConfigError) as caught:
            _rows({**mokuro, "precision": "bf16"})
        assert caught.value.field == "precision" and "mokuro" in str(caught.value)

    def test_the_mokuro_fp16_engine_is_gone(self) -> None:
        with pytest.raises(GenerationConfigError) as caught:
            parse_generation_list([{"name": "m", "engine": "mokuro-fp16", "primary": True}])
        assert caught.value.field == "engine"

    def test_a_mode_is_not_a_hand_configured_pools_table(self) -> None:
        row = _rows({**HAYAI, "precision": "fp32"})[1]
        assert row.pools.is_empty()

    def test_the_config_file_reads_it(self) -> None:
        config = Config.from_dict({"ocr": {"generations": [PRIMARY, {**HAYAI, "precision": "auto"}]}})
        assert config.ocr.generations[1].precision == "auto-accuracy"
        config = Config.from_dict({"ocr": {"generations": [PRIMARY, {**HAYAI, "precision": "fp16"}]}})
        assert config.to_dict()["ocr"]["generations"][1]["precision"] == "fp16"


class TestTheCommandLines:
    def test_the_mode_goes_to_every_runner(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.processor import OCRProcessor

        rows = _rows({**PADDLE, "precision": "auto-speed"})
        proc = OCRProcessor(storage_path=tmp_path, generations=rows,
                            engines_python_path=Path(sys.executable))
        session = proc.session_command(rows[1], tmp_path / "s.log")
        assert session[session.index("--precision") + 1] == "auto-speed"
        bench = proc.open_bench(rows[1], tmp_path / "sample", tmp_path / "b.log").command
        assert bench[bench.index("--precision") + 1] == "auto-speed"

    def test_the_default_sends_nothing(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.processor import OCRProcessor

        rows = _rows(PADDLE)
        proc = OCRProcessor(storage_path=tmp_path, generations=rows,
                            engines_python_path=Path(sys.executable))
        assert "--precision" not in proc.session_command(rows[1], tmp_path / "s.log")


class TestPerMachinePinsAreIgnored:
    def test_the_pools_helpers_drop_them(self) -> None:
        own = {"stage_workers": {}, "queue_capacity": {}, "stage_device": {}}
        assert "precision" not in machine_pools({"precision": "fp16"}, own)
        assert "precision" not in runner_pools({"precision": "fp32"})
        assert not holds_pools({"precision": "fp16"})

    def test_a_stored_pin_is_ignored_with_one_log_line(
        self, tmp_path: Path, caplog: pytest.LogCaptureFixture
    ) -> None:
        profiles_module._IGNORED_LOGGED.clear()
        row = _rows(PADDLE)[1]
        store = ProcessorProfiles(tmp_path)
        store.set_pools("tower", row.id, {"stage_workers": {"detect": 3}},
                        recipe=row.output_affecting())
        # An older file: the pools still carry the machine's own precision.
        path = tmp_path / "processors" / "tower.json"
        raw = json.loads(path.read_text(encoding="utf-8"))
        raw["rows"][row.id]["pools"]["precision"] = "fp16"
        path.write_text(json.dumps(raw), encoding="utf-8")
        with caplog.at_level(logging.INFO, logger=profiles_module.__name__):
            first = store.row("tower", row.id, recipe=row.output_affecting())
            second = store.row("tower", row.id, recipe=row.output_affecting())
        assert first is not None and second is not None
        assert "precision" not in first.pools
        assert first.pools["stage_workers"] == {"detect": 3}
        said = [r for r in caplog.records if "precision" in r.getMessage()]
        assert len(said) == 1

    def test_the_wire_carries_the_row_s_mode_not_a_machine_s(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        entry = registry.register(username="tower", name="tower", host={},
                                  catalog=_catalog(FAST), max_sessions=1)
        worker = _worker(tmp_path, registry, _rows({**HAYAI, "precision": "auto-speed"}))
        row = worker.generations[1]
        ProcessorProfiles(tmp_path).set_pools("tower", row.id, {"precision": "fp16"},
                                              recipe=row.output_affecting())
        spec = worker._remote_row_spec(entry, row)
        assert spec["precision"] == "auto-speed"
        assert "precision" not in spec["pools"]
        # the processor parses what it is sent
        parsed = parse_generation_list([dict(PRIMARY), spec], devices=DeviceCatalog())
        assert parsed[1].precision == "auto-speed"


# --- eligibility -------------------------------------------------------------


class TestEligibility:
    def test_the_registration_field_is_parsed(self) -> None:
        cards = catalog_from_processor(_catalog(FAST))
        assert cards.supported_for("gpu:0") == BF16
        assert cards.supported_for("auto") == BF16
        assert cards.supported_for("cpu") == CPU
        assert catalog_from_processor(_catalog(SLOW)).supported_for("auto") == GPU
        # An older processor: cards listed, nothing said about what they run.
        assert catalog_from_processor(_catalog(None)).supported_for("auto") is None

    @pytest.mark.parametrize("mode", ["auto-accuracy", "auto-balanced", "auto-speed", "fp32"])
    def test_an_old_processor_runs_the_autos_and_fp32(self, mode: str) -> None:
        row = _rows({**PADDLE, "precision": mode})[1]
        assert catalog_can_run(_catalog(None), row) is None

    @pytest.mark.parametrize("mode", ["bf16", "fp16"])
    def test_an_old_processor_is_not_eligible_for_forced_bf16_or_fp16(self, mode: str) -> None:
        row = _rows({**PADDLE, "precision": mode})[1]
        reason = catalog_can_run(_catalog(None), row)
        assert reason is not None and mode in reason

    def test_forced_bf16_needs_a_card_that_supports_it_and_fp16_a_gpu(self) -> None:
        bf16 = _rows({**PADDLE, "precision": "bf16"})[1]
        fp16 = _rows({**PADDLE, "precision": "fp16"})[1]
        assert catalog_can_run(_catalog(FAST), bf16) is None
        assert "bf16 not supported" in (catalog_can_run(_catalog(SLOW), bf16) or "")
        assert catalog_can_run(_catalog(SLOW), fp16) is None
        # The row's recognizer placed on the CPU runs no forced GPU format.
        on_cpu = _rows({**PADDLE, "precision": "fp16",
                        "pools": {"stage_device": {"engine": "cpu"}}})[1]
        assert "fp16 not supported" in (catalog_can_run(_catalog(FAST), on_cpu) or "")

    def test_a_fixed_precision_engine_is_never_refused_for_it(self) -> None:
        row = _rows({"name": "pp", "engine": "ppocr-manga", "precision": "bf16"})[1]
        catalog = _catalog(None, engines=["ppocr-manga"], detectors=["ppocr-manga"])
        assert catalog_can_run(catalog, row) is None

    def test_this_server_answers_from_its_own_probe(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.processor import OCRProcessor

        rows = _rows({**PADDLE, "precision": "bf16"})
        proc = OCRProcessor(storage_path=tmp_path, generations=rows,
                            engines_python_path=Path(sys.executable))
        set_cached_catalog(_card(bf16=False))
        assert "bf16 not supported" in (proc.can_run(rows[1]) or "")
        set_cached_catalog(_card(bf16=True))
        assert proc.can_run(rows[1]) is None

    def test_the_probe_reports_the_facts(self) -> None:
        from mokuro_bunko.ocr.devices import parse_probe

        payload = json.dumps({
            "vendor": "rocm",
            "gpus": [{"index": 0, "name": "RX 6900 XT", "memory_bytes": 16 * 10**9,
                      "formats": {"bf16": True, "fp16": True}}],
        })
        catalog = parse_probe(payload)
        assert catalog.supported_for("gpu:0") == BF16
        assert catalog.gpu_facts() == [{"index": 0, "formats": {"bf16": True, "fp16": True}}]
        # ... and the probe's own source asks torch for it, per device.
        from mokuro_bunko.ocr.devices import PROBE_SOURCE

        assert "is_bf16_supported" in PROBE_SOURCE and "torch.cuda.device(i)" in PROBE_SOURCE

    def test_a_processor_registers_what_its_probe_found(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from mokuro_bunko.ocr import bench, installer
        from mokuro_bunko.processor.cli import _catalog as processor_catalog
        from mokuro_bunko.processor.config import load_processor_config

        monkeypatch.delenv("MOKURO_PROCESSOR_RUNNER", raising=False)
        monkeypatch.setenv("MOKURO_PROCESSOR_ENGINES_PYTHON", str(tmp_path / "python"))
        monkeypatch.setattr(installer.EnginesInstaller, "is_installed", lambda self: True)
        monkeypatch.setattr(installer.EnginesInstaller, "has_detector",
                            lambda self, detector=None: True)
        monkeypatch.setattr(installer.OCRInstaller, "is_installed", lambda self: False)
        monkeypatch.setattr(bench, "probe_devices", lambda python: _card(bf16=False))
        monkeypatch.setattr(bench, "describe_host", lambda backend, python: {"gpu": None})
        path = tmp_path / "processor.yaml"
        path.write_text("library:\n  url: https://library.example:8080\n  username: tower\n"
                        "  password: hunter2hunter2\n", encoding="utf-8")
        catalog, _host = processor_catalog(load_processor_config(path))
        assert catalog[GPU_FACTS_KEY] == [{"index": 0, "formats": {"bf16": False, "fp16": True}}]


def _library(storage: Path, *volumes: str) -> None:
    import zipfile

    for volume in volumes:
        cbz = storage / "library" / "Alpha" / f"{volume}.cbz"
        cbz.parent.mkdir(parents=True, exist_ok=True)
        with zipfile.ZipFile(cbz, "w") as zf:
            zf.writestr("page_000.jpg", b"fake image data")
        cbz.with_suffix(".mokuro").write_text(
            json.dumps({"version": "0.0", "volume_uuid": f"u-{volume}", "pages": [], "chars": 0}),
            encoding="utf-8",
        )


def _worker(storage: Path, registry: ProcessorRegistry, rows: Any, *,
            local: bool = False, autobench: bool = False) -> OCRWorker:
    (storage / "library").mkdir(exist_ok=True)
    return OCRWorker(
        storage_path=storage,
        poll_interval=30.0,
        generations=rows,
        engines_python_path=Path(sys.executable),
        concurrency=1,
        sessions=True,
        remote=registry,
        local_processing=local,
        autobench=autobench,
    )


def _connect(registry: ProcessorRegistry, name: str, gpus: list[dict[str, Any]] | None) -> Any:
    entry = registry.register(username=name, name=name, host={"gpu": name},
                              catalog=_catalog(gpus), max_sessions=1)
    entry.stream_open = True
    return entry


class TestTheScheduler:
    def test_an_ineligible_machine_is_never_offered_the_row(self, tmp_path: Path) -> None:
        _library(tmp_path, "Volume 1", "Volume 2")
        registry = ProcessorRegistry()
        _connect(registry, "slow", SLOW)
        _connect(registry, "fast", FAST)
        worker = _worker(tmp_path, registry, _rows({**PADDLE, "precision": "bf16"}))
        slots = {slot.processor.entry.name: slot for slot in worker._all_slots()}
        worker._active_slots = list(slots.values())
        assert worker.claim_next(slots["slow"]) is None
        job = worker.claim_next(slots["fast"])
        assert job is not None and job[1] == worker.generations[1].id

    def test_the_earliest_finish_lanes_leave_it_out(self, tmp_path: Path) -> None:
        _library(tmp_path, "Volume 1")
        registry = ProcessorRegistry()
        _connect(registry, "slow", SLOW)
        _connect(registry, "fast", FAST)
        worker = _worker(tmp_path, registry, _rows({**PADDLE, "precision": "bf16"}))
        slots = worker._all_slots()
        for slot in slots:
            slot.running = True
        worker._active_slots = slots
        rate_for, _ = worker._lane_pricing()
        lanes = worker._eft_lanes(slots[0], {worker.generations[1].id}, rate_for)
        assert lanes is not None
        by_machine = {lane.machine: lane.rows for lane in lanes}
        assert worker.generations[1].id not in by_machine["slow"]
        assert worker.generations[1].id in by_machine["fast"]

    def test_autobench_skips_it(self, tmp_path: Path) -> None:
        _library(tmp_path, "Volume 1")
        registry = ProcessorRegistry()
        _connect(registry, "slow", SLOW)
        worker = _worker(tmp_path, registry, _rows({**PADDLE, "precision": "bf16"}),
                         autobench=True)
        worker.bench_service = object()  # anything: autobench is asked only with one
        slot = worker._all_slots()[0]
        worker._active_slots = [slot]
        assert worker.claim_next(slot) is None
        assert worker._autobench_wanted == []

    def test_a_row_nobody_can_run_is_held_with_a_plain_reason(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        _connect(registry, "slow", SLOW)
        _connect(registry, "old", None)
        worker = _worker(tmp_path, registry, _rows({**PADDLE, "precision": "bf16"}, HAYAI))
        paddle, hayai = worker.generations[1], worker.generations[2]
        assert worker.precision_holds() == {paddle.id: "No connected machine can run bf16"}
        assert hayai.id not in worker.precision_holds()
        _connect(registry, "fast", FAST)
        assert worker.precision_holds() == {}

    def test_this_server_counts_as_a_machine(self, tmp_path: Path) -> None:
        set_cached_catalog(_card(bf16=True))
        worker = _worker(tmp_path, ProcessorRegistry(), _rows({**PADDLE, "precision": "bf16"}),
                         local=True)
        assert worker.precision_holds() == {}
        set_cached_catalog(DeviceCatalog(probed=True))  # a CPU-only host
        assert worker.precision_holds() == {
            worker.generations[1].id: "No connected machine can run bf16"
        }

    def test_the_plan_prices_it_only_on_eligible_lanes_and_holds_it_otherwise(
        self, tmp_path: Path
    ) -> None:
        registry = ProcessorRegistry()
        _connect(registry, "slow", SLOW)
        worker = _worker(tmp_path, registry, _rows({**PADDLE, "precision": "bf16"}, HAYAI))
        paddle, hayai = worker.generations[1], worker.generations[2]
        for row in (paddle, hayai):
            worker.rates.record_volume(row.id, 20, 10.0)
            worker.rates.record_volume(row.id, 20, 10.0)
            worker.rates.record_startup(row.id, 5.0)
        pending = [
            {"series": "Alpha", "volume": "Volume 1", "generation": paddle.name,
             "generation_id": paddle.id, "pages": 20},
            {"series": "Alpha", "volume": "Volume 1", "generation": hayai.name,
             "generation_id": hayai.id, "pages": 20},
        ]
        plan = worker.queue_plan([], pending)
        held, priced = plan.pending
        assert held["eta_at"] is None and held["held"] is True
        assert held["reason"] == "No connected machine can run bf16"
        # ... and it holds up nothing behind it.
        assert priced["eta_at"] is not None


class TestTheQueueFile:
    def test_a_held_row_s_jobs_say_held(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.control import OcrControl

        rows = _rows({**PADDLE, "precision": "bf16"})
        _library(tmp_path, "Volume 1")
        set_cached_catalog(DeviceCatalog(probed=True))  # a CPU-only host
        worker = OCRWorker(
            storage_path=tmp_path, poll_interval=3600.0, generations=rows,
            engines_python_path=Path("/nonexistent"), sessions=False,
            page_count_lookup=lambda _path: 20,
        )
        for row in rows:
            worker.rates.record_volume(row.id, 20, 10.0)
            worker.rates.record_volume(row.id, 20, 10.0)
        control = OcrControl()
        control.worker = worker
        held, volumes = control.queue_document([], wait=10.0)
        assert held is None
        (volume,) = volumes
        (job,) = volume["jobs"]
        assert (job["id"], job["state"], job["eta"]) == ("paddle", "held", None)


# --- stale benchmarks --------------------------------------------------------


class TestAModeChangeMakesBenchesStale:
    @staticmethod
    def _store(tmp_path: Path, bench: dict[str, Any]) -> tuple[ProcessorProfiles, Any]:
        row = _rows(PADDLE)[1]
        store = ProcessorProfiles(tmp_path)
        store.set_identity("tower", host={"gpu": "RX 9070 XT"}, catalog=_catalog(FAST))
        store.set_bench("tower", row.id, {"pages_per_second": 2.0, **bench},
                        recipe=row.output_affecting())
        return store, row

    TRIALS = [{"precision": "bf16", "pages_per_second": 0.32, "chosen": False},
              {"precision": "fp32", "pages_per_second": 0.57, "chosen": True}]

    def test_a_bench_for_the_row_s_mode_is_kept(self, tmp_path: Path) -> None:
        store, row = self._store(tmp_path, {"precision": "fp32", "precision_mode": "auto-accuracy"})
        found = store.row("tower", row.id, recipe=row.output_affecting(), mode="auto-accuracy")
        assert found is not None and found.bench is not None and not found.stale_bench

    @pytest.mark.parametrize("mode", ["auto-balanced", "auto-speed", "fp32", "bf16", "fp16"])
    def test_a_mode_change_makes_it_stale(self, tmp_path: Path, mode: str) -> None:
        store, row = self._store(tmp_path, {"precision": "fp32", "precision_mode": "auto-accuracy"})
        found = store.row("tower", row.id, recipe=row.output_affecting(), mode=mode)
        assert found is not None and found.stale_bench and found.bench is None

    def test_a_balanced_bench_with_every_candidate_tried_is_the_pick(self, tmp_path: Path) -> None:
        from mokuro_bunko.ocr.precision import bench_pick

        store, row = self._store(tmp_path, {
            "precision": "fp32", "precision_mode": "auto-balanced",
            "precision_trials": self.TRIALS,
            "precision_why": "benchmark: fp32 0.57 p/s beat bf16 0.32 p/s",
        })
        found = store.row("tower", row.id, recipe=row.output_affecting(), mode="auto-balanced")
        assert found is not None and found.bench is not None
        assert bench_pick(found.bench, "auto-balanced") == (
            "fp32", "benchmark: fp32 0.57 p/s beat bf16 0.32 p/s",
        )

    def test_a_candidate_change_re_measures(self, tmp_path: Path) -> None:
        store, row = self._store(tmp_path, {
            "precision": "fp32", "precision_mode": "auto-balanced",
            "precision_trials": [self.TRIALS[1]],  # only fp32 was tried
        })
        found = store.row("tower", row.id, recipe=row.output_affecting(), mode="auto-balanced")
        assert found is not None and found.stale_bench

    def test_the_worker_asks_for_a_new_benchmark_after_a_mode_change(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry, "tower", FAST)
        worker = _worker(tmp_path, registry, _rows(PADDLE), autobench=True)
        worker.bench_service = object()
        row = worker.generations[1]
        ProcessorProfiles(tmp_path).set_bench(
            "tower", row.id,
            {"precision": "fp32", "precision_mode": "auto-accuracy", "pages_per_second": 2.0},
            recipe=row.output_affecting(),
        )
        assert not worker.autobench_needed(entry, row)
        from dataclasses import replace

        worker.generations[1] = replace(row, precision="auto-speed")
        assert worker.autobench_needed(entry, worker.generations[1])

    def test_the_pick_reaches_that_machine_s_runner(self, tmp_path: Path) -> None:
        registry = ProcessorRegistry()
        entry = _connect(registry, "tower", FAST)
        worker = _worker(tmp_path, registry, _rows({**PADDLE, "precision": "auto-balanced"}))
        row = worker.generations[1]
        spec = worker._remote_row_spec(entry, row)
        assert "precision_pick" not in spec  # not benchmarked yet: the runner takes bf16
        ProcessorProfiles(tmp_path).set_bench(
            "tower", row.id,
            {"precision": "fp32", "precision_mode": "auto-balanced", "pages_per_second": 0.57,
             "precision_trials": self.TRIALS,
             "precision_why": "benchmark: fp32 0.57 p/s beat bf16 0.32 p/s"},
            recipe=row.output_affecting(),
        )
        spec = worker._remote_row_spec(entry, row)
        assert (spec["precision"], spec["precision_pick"]) == ("auto-balanced", "fp32")
        parsed = parse_generation_list([dict(PRIMARY), spec], devices=DeviceCatalog())[1]
        from mokuro_bunko.ocr.processor import OCRProcessor

        command = OCRProcessor(storage_path=tmp_path, generations=[parsed],
                               engines_python_path=Path(sys.executable)
                               ).session_command(parsed, tmp_path / "s.log")
        assert command[command.index("--precision-pick") + 1] == "fp32"


# --- the runner that refuses ------------------------------------------------


def test_a_runner_that_refuses_its_forced_format_gives_the_volume_back(tmp_path: Path) -> None:
    """Defence in depth: eligibility should never send it, but if it does,
    the runner refuses at start and the volume goes back unrecorded."""
    from tests.unit.test_ocr_sessions import _script
    from tests.unit.test_ocr_sessions import _worker as session_worker

    (tmp_path / "library").mkdir()
    (tmp_path / "inbox").mkdir()
    _library(tmp_path, "Volume 1")
    set_cached_catalog(_card(bf16=True))
    rows = parse_generation_list(
        [dict(PRIMARY), {"name": "hayai-nova", "engine": "hayai-nova", "precision": "bf16"}]
    )
    script = _script(
        tmp_path,
        spawn_fatal=f"hayai-nova failed to load: {PRECISION_REFUSAL}: bf16 needs a card "
                    "with fast bf16; gfx1030 has none",
    )
    worker = session_worker(tmp_path, rows, script=script)
    opened: list[Any] = []
    real_open = worker.processor.open_session

    def spy(generation: Any, log: Path) -> Any:
        session = real_open(generation, log)
        opened.append(session)
        return session

    worker.processor.open_session = spy  # type: ignore[method-assign]
    worker._scan_ocr_once()
    assert opened, "the runner was started (eligibility let it through)"
    # Not tried again at once: the refusal backs the row off on this machine.
    assert [b["generation"] for b in worker.start_backoffs("local")] == ["hayai-nova"]
    assert not (tmp_path / ".ocr-failures.json").exists() or json.loads(
        (tmp_path / ".ocr-failures.json").read_text(encoding="utf-8")
    ) == {}
    assert worker._inflight_ocr == set()
    assert not list((tmp_path / "library").rglob("*.hayai-nova.mokuro"))
