"""Per-stage device choice: the catalog, and the graph that follows it.

The rules of Addendum 7: a device id is ``auto``/``cpu``/``gpu:<n>``, the
catalog is probed once in the engines environment, and ``road_specs`` puts
each model-bearing stage where the row says -- which is what makes a CPU
detector poolable and leaves the whole card to the engine.
"""

from __future__ import annotations

import json
from types import SimpleNamespace

import pytest

from mokuro_bunko.ocr import engine_runner as runner
from mokuro_bunko.ocr.devices import (
    PROBE_SOURCE,
    DeviceCatalog,
    GpuDevice,
    parse_probe,
    stage_devices_allowed,
    stage_lock_reason,
)
from mokuro_bunko.ocr.engines import DETECTORS, ENGINES


class TestDeviceIds:
    """One grammar, two spellings: bunko says gpu:<n>, torch says cuda:<n>."""

    @pytest.mark.parametrize(
        ("raw", "want"),
        [
            ("auto", "auto"),
            ("cpu", "cpu"),
            ("CPU", "cpu"),
            ("gpu", "gpu:0"),
            ("gpu:1", "gpu:1"),
            ("cuda", "gpu:0"),
            ("cuda:2", "gpu:2"),
            (" gpu:0 ", "gpu:0"),
        ],
    )
    def test_parse(self, raw: str, want: str) -> None:
        assert runner.parse_device(raw) == want

    @pytest.mark.parametrize("raw", ["", "tpu", "gpu:", "gpu:x", "gpu:99", "cuda:-1", "0"])
    def test_a_non_device_is_refused(self, raw: str) -> None:
        with pytest.raises(ValueError):
            runner.parse_device(raw)

    def test_torch_spelling(self) -> None:
        assert runner.torch_device("gpu:1") == "cuda:1"
        assert runner.torch_device("gpu") == "cuda:0"
        assert runner.torch_device("cpu") == "cpu"

    def test_auto_is_card_zero_when_there_is_one(self) -> None:
        assert runner.resolve_device("auto", gpu=True) == "gpu:0"
        assert runner.resolve_device("auto", gpu=False) == "cpu"
        assert runner.resolve_device("gpu:1", gpu=True) == "gpu:1"
        # A model that cannot leave the CPU resolves there whatever is asked:
        # the refusal belongs to whoever validated the edit.
        assert runner.resolve_device("gpu:0", gpu=True, cpu_only=True) == "cpu"


class TestTheProbe:
    """One JSON line from the engines environment, parsed here."""

    def test_a_rocm_host_is_not_called_cuda(self) -> None:
        catalog = parse_probe(
            json.dumps(
                {
                    "vendor": "rocm",
                    "gpus": [
                        {
                            "index": 0,
                            "name": "AMD Radeon RX 9070 XT",
                            "memory_bytes": 17_095_983_104,
                        }
                    ],
                }
            ),
            cpu_label="AMD Ryzen 9 7950X (16 cores)",
        )
        assert catalog.vendor == "rocm"
        assert catalog.ids() == ("auto", "cpu", "gpu:0")
        labels = {row["id"]: row["label"] for row in catalog.entries()}
        assert labels["gpu:0"] == "GPU 0 — AMD Radeon RX 9070 XT (17 GB)"
        assert labels["cpu"] == "AMD Ryzen 9 7950X (16 cores)"
        assert labels["auto"] == "Auto — GPU 0 when available"

    def test_two_cards_are_two_ids(self) -> None:
        catalog = parse_probe(
            json.dumps(
                {
                    "vendor": "cuda",
                    "gpus": [
                        {"index": 1, "name": "B", "memory_bytes": None},
                        {"index": 0, "name": "A", "memory_bytes": None},
                    ],
                }
            )
        )
        assert catalog.ids() == ("auto", "cpu", "gpu:0", "gpu:1")
        assert [row["label"] for row in catalog.entries()][2:] == ["GPU 0 — A", "GPU 1 — B"]

    @pytest.mark.parametrize("payload", [None, "", "not json", "[]", '{"gpus": "no"}'])
    def test_junk_is_the_fallback_catalog(self, payload: str | None) -> None:
        catalog = parse_probe(payload)
        assert catalog.ids() == ("auto", "cpu")
        assert catalog.has_gpu is False

    def test_a_probed_host_with_no_card_refuses_one(self) -> None:
        catalog = parse_probe(json.dumps({"vendor": "", "gpus": []}))
        assert catalog.probed is True
        assert catalog.knows("gpu:0") is False
        assert "reports 0 GPUs" in catalog.refusal("gpu:0")

    def test_an_unprobed_catalog_accepts_a_card_it_cannot_see(self) -> None:
        assert DeviceCatalog().knows("gpu:1") is True
        assert DeviceCatalog(gpus=(GpuDevice(0, "A"),), probed=True).knows("gpu:1") is False

    def test_the_probe_asks_hip_before_it_labels_anything(self) -> None:
        assert "torch.version" in PROBE_SOURCE
        assert PROBE_SOURCE.index("hip") < PROBE_SOURCE.index("cuda")


class TestWhatMayGoWhere:
    """The catalog's CPU-only flags and the runner's placement rules agree."""

    def test_the_registry_and_the_runner_say_the_same_thing(self) -> None:
        for spec in DETECTORS.values():
            adapter_is_cpu = runner.stage_is_cpu_only(
                runner.ROAD_ADAPTER, runner.STAGE_DETECT, engine="hayai-nova", detector=spec.id
            )
            assert spec.cpu_only == adapter_is_cpu, spec.id
        assert ENGINES["ppocr-manga"].cpu_only is True
        assert ENGINES["hayai-nova"].cpu_only is False

    def test_only_a_model_bearing_stage_takes_a_device(self) -> None:
        assert runner.model_stages(runner.ROAD_ADAPTER) == ("detect", "engine")
        assert runner.model_stages(runner.ROAD_RECONCILED) == ("detect", "engine")
        assert runner.model_stages(runner.ROAD_LINE) == ("detect",)
        assert stage_devices_allowed(runner.ROAD_ADAPTER, "post", engine="hayai-nova") == []

    def test_a_cpu_only_stage_offers_only_the_cpu_and_says_why(self) -> None:
        allowed = stage_devices_allowed(
            runner.ROAD_LINE,
            runner.STAGE_DETECT,
            engine="ppocr-manga",
            detector="ppocr-manga",
            catalog=DeviceCatalog(gpus=(GpuDevice(0, "A"),), probed=True),
        )
        assert allowed == ["auto", "cpu"]
        reason = stage_lock_reason(
            runner.ROAD_LINE, runner.STAGE_DETECT, engine="ppocr-manga", detector="ppocr-manga"
        )
        assert reason and "onnxruntime" in reason

    def test_a_free_stage_offers_the_whole_catalog(self) -> None:
        catalog = DeviceCatalog(gpus=(GpuDevice(0, "A"), GpuDevice(1, "B")), probed=True)
        allowed = stage_devices_allowed(
            runner.ROAD_ADAPTER,
            runner.STAGE_ENGINE,
            engine="hayai-nova",
            detector="ctd",
            catalog=catalog,
        )
        assert allowed == ["auto", "cpu", "gpu:0", "gpu:1"]
        assert (
            stage_lock_reason(
                runner.ROAD_ADAPTER, runner.STAGE_ENGINE, engine="hayai-nova", detector="ctd"
            )
            is None
        )

    def test_a_monolithic_row_has_one_stage_and_it_is_free(self) -> None:
        assert stage_devices_allowed(None, "mokuro", engine="mokuro") != []
        assert stage_devices_allowed(None, "detect", engine="mokuro") == []
        assert stage_lock_reason(None, "mokuro", engine="mokuro") is None


class TestTheGraphFollowsTheChoice:
    """``road_specs`` places the models; the widths follow the placement."""

    def test_the_detector_moves_to_the_cpu_and_becomes_poolable(self) -> None:
        specs = runner.road_specs(
            runner.ROAD_ADAPTER,
            detector="ctd",
            engine="hayai-nova",
            gpu=True,
            devices={"detect": "cpu"},
        )
        by_key = {spec.key: spec for spec in specs}
        assert by_key["detect"].device == "cpu"
        assert by_key["engine"].device == "gpu:0"
        widths = runner.stage_widths("hayai-nova", runner.ROAD_ADAPTER, budget=8, specs=specs)
        detect_width = widths[[spec.key for spec in specs].index("detect")]
        assert detect_width > 1, "a CPU detector is a pool, not one process"

    def test_a_detector_left_on_the_card_stays_at_one(self) -> None:
        specs = runner.road_specs(
            runner.ROAD_ADAPTER, detector="ctd", engine="hayai-nova", gpu=True
        )
        widths = runner.stage_widths("hayai-nova", runner.ROAD_ADAPTER, budget=8, specs=specs)
        assert dict(zip([s.key for s in specs], widths, strict=True))["detect"] == 1

    def test_a_second_card_is_named_in_the_graph(self) -> None:
        specs = runner.road_specs(
            runner.ROAD_ADAPTER,
            detector="ctd",
            engine="hayai-nova",
            gpu=True,
            devices={"detect": "gpu:1", "engine": "gpu:0"},
        )
        assert [spec.device for spec in specs] == ["gpu:1", "gpu:0", "cpu"]

    def test_an_engine_on_the_cpu_is_still_one_model(self) -> None:
        specs = runner.road_specs(
            runner.ROAD_ADAPTER,
            detector="ctd",
            engine="hayai-nova",
            gpu=True,
            devices={"engine": "cpu"},
        )
        by_key = {spec.key: spec for spec in specs}
        assert by_key["engine"].device == "cpu"
        assert by_key["engine"].max_workers == runner.DEVICE_BOUND
        widths = runner.stage_widths("hayai-nova", runner.ROAD_ADAPTER, budget=8, specs=specs)
        assert widths[[spec.key for spec in specs].index("engine")] == 1

    def test_a_cpu_only_model_is_never_placed_on_a_card(self) -> None:
        specs = runner.road_specs(
            runner.ROAD_RECONCILED,
            detector="ppocr-manga",
            engine="hayai-nova",
            gpu=True,
            devices={"detect": "gpu:0"},
        )
        assert {spec.key: spec.device for spec in specs}["detect"] == "cpu"

    def test_without_a_choice_it_is_what_it_has_always_been(self) -> None:
        on_card = runner.road_specs(runner.ROAD_ADAPTER, detector="ctd", gpu=True)
        off_card = runner.road_specs(runner.ROAD_ADAPTER, detector="ctd", gpu=False)
        assert [spec.device for spec in on_card] == ["gpu:0", "gpu:0", "cpu"]
        assert [spec.device for spec in off_card] == ["cpu", "cpu", "cpu"]


class TestTheRunnerCli:
    """``--stage-device``: the same k=v shape as ``--stage-workers``."""

    def test_it_parses_both_spellings(self) -> None:
        assert runner.parse_stage_devices("detect=cpu,engine=cuda:1") == {
            "detect": "cpu",
            "engine": "gpu:1",
        }
        assert runner.parse_stage_devices(" engine = gpu:0 ") == {"engine": "gpu:0"}
        assert runner.parse_stage_devices(None) == {}
        assert runner.parse_stage_devices("") == {}

    @pytest.mark.parametrize(
        "raw",
        [
            "cpu",  # a bare device: there is no "everything" to put on a card
            "post=cpu",  # no model on that stage
            "layout=cpu",
            "detect=tpu",
            "detect=gpu:x",
        ],
    )
    def test_a_typo_is_refused_rather_than_ignored(self, raw: str) -> None:
        with pytest.raises(ValueError):
            runner.parse_stage_devices(raw)

    def test_the_environment_is_the_fallback(self, monkeypatch: pytest.MonkeyPatch) -> None:
        monkeypatch.setenv(runner.STAGE_DEVICE_ENV, "detect=cpu")
        assert runner.resolve_stage_devices(None, runner.STAGE_DEVICE_ENV) == {"detect": "cpu"}
        # What the caller said wins, and is not merged with the environment.
        assert runner.resolve_stage_devices("engine=gpu:0", runner.STAGE_DEVICE_ENV) == {
            "engine": "gpu:0"
        }

    def test_the_flag_reaches_the_session_config(self) -> None:
        args = runner.parse_args(
            [
                "--engine",
                "hayai-nova",
                "--detector",
                "ctd",
                "--input",
                "/tmp/in",
                "--output",
                "/tmp/out.mokuro",
                "--cache-dir",
                "/tmp/cache",
                "--stage-device",
                "detect=cpu,engine=gpu:0",
            ]
        )
        assert runner.SessionConfig.from_args(args).stage_device == "detect=cpu,engine=gpu:0"

    def test_a_benchmark_may_be_told_where_to_put_the_models(self, tmp_path: object) -> None:
        """Unlike the widths: the placement IS part of the spec being measured."""
        args = runner.parse_args(
            [
                "--bench",
                "--engine",
                "hayai-nova",
                "--detector",
                "ctd",
                "--input",
                "/tmp/in",
                "--session-log",
                "/tmp/bench.log",
                "--stage-device",
                "detect=cpu",
            ]
        )
        assert args.stage_device == "detect=cpu"

    def test_the_adapter_is_told_the_device_on_its_command_line(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        seen: list[list[str]] = []

        class FakePopen:
            def __init__(self, cmd: list[str], **_kwargs: object) -> None:
                seen.append(list(cmd))
                self.stdin = None
                self.stdout = []

        monkeypatch.setattr(runner.subprocess, "Popen", FakePopen)
        monkeypatch.setattr(runner.threading, "Thread", lambda **kw: _NoThread())

        runner.DetectorProcess("ctd", "/tmp/ctd.py", index=0, device="gpu:1").start()
        runner.DetectorProcess("ctd", "/tmp/ctd.py", index=0).start()

        assert seen[0][-2:] == ["--device", "cuda:1"], "torch's spelling reaches the adapter"
        assert "--device" not in seen[1], "no choice made: the adapter probes as it always did"

    def test_the_pipeline_reports_where_the_models_really_are(self) -> None:
        """``ready``/``bench_ready`` say the RESOLVED placement, not the ask."""
        specs = runner.road_specs(
            runner.ROAD_ADAPTER,
            detector="ctd",
            engine="hayai-nova",
            gpu=True,
            devices={"detect": "cpu"},
        )
        stub = SimpleNamespace(specs=specs)
        assert runner.OpenPipeline.stage_device(stub) == {"detect": "cpu", "engine": "gpu:0"}


class _NoThread:
    def start(self) -> None:
        pass
