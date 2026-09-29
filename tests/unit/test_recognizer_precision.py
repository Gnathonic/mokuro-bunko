"""Recognizer PRECISION (bf16 / fp16 / fp32) on one device: the row's MODE.

The policy (``runner.PRECISION_POLICY``) and its resolution are pinned in
``test_precision_modes.py``. Here: what the recognizers load and autocast to
on a stand-in torch that says whether its card runs bf16, the log line that
says why, the sidecar's record, the copies in processes of their own, and
the benchmark -- which runs one trial per supported candidate of a
balanced/speed mode and keeps the fastest.

Everything here runs against stand-ins: no torch, no model, no page content.
"""

from __future__ import annotations

import contextlib
import json
import sys
import time
import types
from collections.abc import Sequence
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner
from tests.fixtures import fake_engine_recognizer as fakes
from tests.unit.test_engine_runner import Image, _FakeDetectors, _one_crop_per_line
from tests.unit.test_engine_sessions import fake_ppocr  # noqa: F401 - fixture

# --- the recognizers, against stand-in torch ------------------------------------


class _Model:
    def to(self, device: str) -> _Model:
        self.device = device
        return self

    def eval(self) -> _Model:
        return self

    def load_state_dict(self, state: Any, strict: bool = True) -> Any:
        return types.SimpleNamespace(unexpected_keys=[])

    def named_modules(self) -> list[Any]:
        return []

    def merge_and_unload(self) -> _Model:
        return self


LOADS: list[dict[str, Any]] = []


class _Loader:
    @staticmethod
    def from_pretrained(*_args: Any, **kwargs: Any) -> Any:
        LOADS.append(dict(kwargs))
        model = _Model()
        model.tokenizer = types.SimpleNamespace(padding_side="right")  # type: ignore[attr-defined]
        model.apply_chat_template = lambda *a, **k: "prompt"  # type: ignore[attr-defined]
        return model


def _stand_in_torch(monkeypatch: pytest.MonkeyPatch, bf16: bool) -> None:
    """A torch whose card says ``bf16`` about bfloat16 support."""
    torch = types.ModuleType("torch")
    torch.set_num_threads = lambda n: None  # type: ignore[attr-defined]
    torch.bfloat16, torch.float16, torch.float32 = "bfloat16", "float16", "float32"  # type: ignore[attr-defined]
    torch.cuda = types.SimpleNamespace(  # type: ignore[attr-defined]
        is_available=lambda: True,
        device=lambda index: contextlib.nullcontext(),
        is_bf16_supported=lambda: bf16,
    )
    torch.nn = types.SimpleNamespace(Module=object, Conv2d=type("Conv2d", (), {}))  # type: ignore[attr-defined]
    transformers = types.ModuleType("transformers")
    for name in (
        "AutoModel", "PreTrainedTokenizerFast", "AutoProcessor", "AutoModelForImageTextToText"
    ):
        setattr(transformers, name, _Loader)
    peft = types.ModuleType("peft")
    peft.PeftModel = types.SimpleNamespace(  # type: ignore[attr-defined]
        from_pretrained=lambda model, *a, **k: model
    )
    safetensors = types.ModuleType("safetensors")
    safetensors_torch = types.ModuleType("safetensors.torch")
    safetensors_torch.load_file = lambda path: {}  # type: ignore[attr-defined]
    safetensors.torch = safetensors_torch  # type: ignore[attr-defined]
    hub = types.ModuleType("huggingface_hub")
    hub.hf_hub_download = lambda *a, **k: "tower.safetensors"  # type: ignore[attr-defined]
    for name, module in {
        "torch": torch,
        "transformers": transformers,
        "peft": peft,
        "safetensors": safetensors,
        "safetensors.torch": safetensors_torch,
        "huggingface_hub": hub,
    }.items():
        monkeypatch.setitem(sys.modules, name, module)
    monkeypatch.setenv("OMP_NUM_THREADS", "1")  # leave the thread cap out of it
    LOADS.clear()


BF16_CARD = True  # torch says the card runs bf16 (natively or not)
NO_BF16 = False


def _said(capsys: pytest.CaptureFixture[str]) -> str:
    out = capsys.readouterr()
    return out.out + out.err


class TestPaddleDtype:
    def _dtype(self) -> Any:
        (base,) = [load for load in LOADS if "dtype" in load]
        return base["dtype"]

    @pytest.mark.parametrize("card", [BF16_CARD, NO_BF16])
    def test_accuracy_loads_fp32_on_every_card_and_says_why(
        self, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str], card: bool
    ) -> None:
        _stand_in_torch(monkeypatch, card)
        recognizer = runner.load_recognizer("paddle-manga", device="gpu:0")
        assert self._dtype() == "float32" and recognizer.precision == "fp32"
        assert "[runner] paddle-manga precision: fp32 (auto-accuracy)" in _said(capsys)

    @pytest.mark.parametrize(("mode", "dtype"), [("bf16", "bfloat16"), ("fp16", "float16")])
    def test_a_forced_format_the_card_supports_is_loaded(
        self, monkeypatch: pytest.MonkeyPatch, mode: str, dtype: str
    ) -> None:
        _stand_in_torch(monkeypatch, BF16_CARD)
        recognizer = runner.load_recognizer("paddle-manga", device="gpu:0", precision=mode)
        assert self._dtype() == dtype and recognizer.precision == mode

    def test_balanced_takes_the_benchmark_s_pick(
        self, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
    ) -> None:
        _stand_in_torch(monkeypatch, BF16_CARD)
        recognizer = runner.load_recognizer(
            "paddle-manga", device="gpu:0", precision="auto-balanced", pick="fp32",
            pick_why="benchmark: fp32 0.57 p/s beat bf16 0.32 p/s",
        )
        assert self._dtype() == "float32" and recognizer.precision == "fp32"
        assert (
            "[runner] paddle-manga precision: fp32 (auto-balanced; benchmark: fp32 0.57 p/s "
            "beat bf16 0.32 p/s)" in _said(capsys)
        )

    def test_a_forced_format_the_device_cannot_run_refuses_to_load(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _stand_in_torch(monkeypatch, NO_BF16)
        with pytest.raises(runner.PrecisionUnavailable, match=runner.PRECISION_REFUSAL):
            runner.load_recognizer("paddle-manga", device="gpu:0", precision="bf16")
        assert LOADS == [], "refused before any weights were loaded"
        with pytest.raises(runner.PrecisionUnavailable):
            runner.load_recognizer("paddle-manga", device="cpu", precision="fp16")

    def test_the_cpu_reads_in_fp32(self, monkeypatch: pytest.MonkeyPatch) -> None:
        _stand_in_torch(monkeypatch, BF16_CARD)
        recognizer = runner.load_recognizer("paddle-manga", device="cpu", precision="auto-speed")
        assert self._dtype() == "float32" and recognizer.precision == "fp32"

    def test_no_master_copy_until_a_benchmark_re_casts(self, monkeypatch: pytest.MonkeyPatch) -> None:
        _stand_in_torch(monkeypatch, BF16_CARD)
        recognizer = runner.load_recognizer("paddle-manga", device="gpu:0")
        assert recognizer._master is None  # type: ignore[attr-defined]


class TestHayaiAutocast:
    @pytest.mark.parametrize(
        ("mode", "card", "precision", "amp"),
        [
            ("auto-accuracy", BF16_CARD, "bf16", "bfloat16"),
            ("auto-accuracy", NO_BF16, "fp32", None),
            ("auto-speed", NO_BF16, "fp16", "float16"),
            ("bf16", BF16_CARD, "bf16", "bfloat16"),
            ("fp16", NO_BF16, "fp16", "float16"),
            ("fp32", BF16_CARD, "fp32", None),
        ],
    )
    def test_the_autocast_dtype_follows_the_mode_on_the_card(
        self, monkeypatch: pytest.MonkeyPatch, mode: str, card: bool, precision: str, amp: Any
    ) -> None:
        _stand_in_torch(monkeypatch, card)
        recognizer = runner.load_recognizer("hayai-nova", device="gpu:0", precision=mode)
        assert recognizer.precision == precision
        assert recognizer.amp_dtype == amp

    def test_the_default_says_why(
        self, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
    ) -> None:
        _stand_in_torch(monkeypatch, BF16_CARD)
        runner.load_recognizer("hayai-nova", device="gpu:0")
        assert "[runner] hayai-nova precision: bf16 (auto-accuracy)" in _said(capsys)

    def test_on_the_cpu_there_is_no_autocast(self, monkeypatch: pytest.MonkeyPatch) -> None:
        _stand_in_torch(monkeypatch, BF16_CARD)
        recognizer = runner.load_recognizer("hayai-nova", device="cpu")
        assert recognizer.precision == "fp32" and recognizer.amp_dtype is None

    def test_a_benchmark_switches_the_autocast_without_a_reload(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _stand_in_torch(monkeypatch, BF16_CARD)
        recognizer = runner.load_recognizer("hayai-nova", device="gpu:0", precision="fp32")
        loads = len(LOADS)
        recognizer.set_precision("fp16")  # type: ignore[attr-defined]
        assert recognizer.amp_dtype == "float16" and len(LOADS) == loads


# --- the sidecar ------------------------------------------------------------------


class TestTheSidecar:
    def test_the_precision_a_volume_was_read_at_is_in_ocr_engine(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        input_dir = tmp_path / "Vol"
        input_dir.mkdir()
        (input_dir / "001.webp").write_bytes(b"x")

        class _Recognizer:
            repos = {"some/recognizer": "a" * 40}
            precision = "fp16"

            def __call__(self, crops: list[Any], max_tokens: list[int] | None = None) -> list[str]:
                return ["テスト"] * len(crops)

        asked: list[dict[str, Any]] = []

        def load(*_a: Any, **kwargs: Any) -> Any:
            asked.append(kwargs)
            return _Recognizer()

        monkeypatch.setattr(runner, "open_detectors", _FakeDetectors().open)
        monkeypatch.setattr(runner, "load_recognizer", load)
        monkeypatch.setattr(runner, "imread_bgr", lambda path: Image(100, 200))
        monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _one_crop_per_line)
        monkeypatch.setattr(runner, "make_upright_crop_fn", lambda *a, **k: _one_crop_per_line)
        output = tmp_path / "out" / "Vol.paddle-manga.mokuro"
        args = runner.parse_args(
            [
                "--engine", "paddle-manga", "--detector", "ctd",
                "--input", str(input_dir), "--output", str(output),
                "--cache-dir", str(tmp_path / "out" / "_ocr" / "paddle-manga" / "Vol"),
                "--precision", "fp16",
            ]
        )  # fmt: skip
        assert runner.run(args) == 0
        assert asked and asked[0]["precision"] == "fp16"
        volume = json.loads(output.read_text(encoding="utf-8"))
        assert volume["ocr_engine"]["precision"] == "fp16"


# --- the precision reaches every copy of the recognizer ------------------------------


class _Copy:
    repos: dict[str, str] = {}
    precision = "bf16"

    def __call__(self, crops: list[Any], max_tokens: Any = None) -> list[str]:
        return list(crops)


class _FakeProcess(_Copy):
    started: list[_FakeProcess] = []

    def __init__(self, engine: str, load_kwargs: dict[str, Any], index: int = 0) -> None:
        self.load_kwargs = load_kwargs
        _FakeProcess.started.append(self)

    def close(self) -> None:
        pass


@pytest.mark.usefixtures("fake_ppocr")
def test_the_precision_reaches_recognizer_copies_in_processes_of_their_own(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(runner, "load_recognizer", lambda *a, **k: _Copy())
    monkeypatch.setattr(runner, "EngineProcess", _FakeProcess)
    # the crop makers need cv2, which the crops here never reach
    monkeypatch.setattr(runner, "make_quad_crop_fn", lambda *a, **k: _one_crop_per_line)
    monkeypatch.setattr(runner, "make_line_crop_fn", lambda *a, **k: _one_crop_per_line)
    _FakeProcess.started = []
    session = runner.OpenPipeline(
        runner.SessionConfig(
            engine="paddle-manga",
            detector="ppocr-manga",
            stage_workers="engine=2",
            stage_device="engine=gpu:0",
            precision="fp16",
        )
    )
    try:
        session.loader._done.wait(5.0)  # type: ignore[union-attr]
        assert [p.load_kwargs["precision"] for p in _FakeProcess.started] == ["fp16", "fp16"]
        assert session.precision() == "bf16"  # what the copies REPORT they loaded
    finally:
        session.pipeline.close()
        session.close()


def test_an_engine_process_reports_its_precision_on_ready() -> None:
    proc = runner.EngineProcess("paddle-manga", {"precision": "fp16"}, load=fakes.load_precise)
    try:
        assert proc.precision == "fp16"
    finally:
        proc.close()


# --- the benchmark: widths only, at the precision the policy (or a pin) loaded ------


class _Stream:
    def __init__(self, items: list[tuple[Any, Any]], offsets: Sequence[float]) -> None:
        self.items, self.offsets = items, list(offsets)

    def __iter__(self) -> Any:
        start = time.monotonic()
        for index, item in enumerate(self.items):
            due = start + self.offsets[index]
            while time.monotonic() < due:
                time.sleep(0.001)
            yield item

    def close(self) -> None:
        pass


class _Pipeline:
    def __init__(self, pipe: _Pipe) -> None:
        self.pipe = pipe
        self.count = 0

    def run(self, jobs: Sequence[Any]) -> _Stream:
        self.count = len(jobs)
        items = [
            (
                job,
                runner.Outcome(
                    value=runner.PageResult(page={"blocks": [{"lines": [job.rel.stem]}]}),
                    error=None,
                ),
            )
            for job in jobs
        ]
        return _Stream(items, [self.pipe.per_page * (n + 1) for n in range(len(jobs))])

    def report(self) -> runner.PipelineReport:
        return runner.PipelineReport(elapsed=1.0, items=self.count, stages=(), queues=())


class _Pipe:
    """One engine stage at width 1 on a card: nothing to widen, one trial."""

    host_budget = 1

    def __init__(self, loaded: str | None, per_page: float = 0.02) -> None:
        self.loaded = loaded
        self.per_page = per_page
        self.widths = [1]
        self.specs = (runner.StageSpec("engine", "engine", "gpu:0", runner.DEVICE_BOUND, 1),)
        self.caps = [1]

    def rebuild(self, widths: Sequence[int]) -> _Pipeline:
        return _Pipeline(self)

    def precision(self) -> str | None:
        return self.loaded

    def stage_device(self) -> dict[str, str]:
        return {"engine": "gpu:0"}

    def detect_devices(self) -> list[str]:
        return []

    def _stage_device(self, key: str) -> str:
        return "gpu:0"

    def wait_ready(self) -> None:
        pass

    def close(self) -> None:
        pass


def _bench_run(
    monkeypatch: pytest.MonkeyPatch, pipe: _Pipe, *, precision: str = "auto",
    engine: str = "hayai-nova",
) -> tuple[list[dict[str, Any]], list[Any]]:
    """A whole ``--bench`` run against ``pipe``: (events, configs it opened)."""
    monkeypatch.setattr(runner, "BENCH_MIN_WINDOW_SECONDS", 0.3)
    monkeypatch.setattr(runner, "BENCH_SHORT_WINDOW_SECONDS", 0.1)
    monkeypatch.setattr(runner, "BENCH_MAX_PASSES", 4)
    events: list[dict[str, Any]] = []

    class _Protocol:
        def emit(self, event: str, **fields: Any) -> None:
            events.append({"event": event, **fields})

    args = types.SimpleNamespace(
        input=".",
        bench_max_trials=8,
        bench_budget_seconds=900.0,
        precision=precision,
        engine=engine,
        detector="ppocr-manga",
        patches=runner.DEFAULT_PATCH_BUDGET,
        generator=None,
    )
    bench = runner.BenchRun(args, _Protocol())  # type: ignore[arg-type]
    bench.scratch = Path(".")
    bench.sample = [Path(f"{n:03d}.jpg") for n in range(8)]
    configs: list[Any] = []

    def open_pipeline(config: Any) -> _Pipe:
        configs.append(config)
        return pipe

    monkeypatch.setattr(runner, "OpenPipeline", open_pipeline)
    assert bench._run() == 0
    return events, configs


class _Target:
    """The loaded recognizer a benchmark re-casts: its speed follows its format."""

    def __init__(self, pipe: _Pipe, supported: frozenset[str], seconds: dict[str, float]) -> None:
        self.pipe, self._supported, self.seconds = pipe, supported, seconds
        self.precision = pipe.loaded
        self.switched: list[str] = []

    def supported(self) -> frozenset[str]:
        return self._supported

    def set_precision(self, name: str) -> None:
        self.switched.append(name)
        self.precision = name
        self.pipe.loaded = name
        self.pipe.per_page = self.seconds[name]

    def release_master(self) -> None:
        self.switched.append("released")


class TestTheBenchmark:
    def test_a_fixed_mode_runs_at_what_it_loaded_and_tunes_widths_only(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """hayai-nova on a bf16 card, auto-accuracy: bf16, no other format tried."""
        events, configs = _bench_run(monkeypatch, _Pipe("bf16"))
        assert configs[0].precision == "auto-accuracy", "loaded as asked: no fp32 override"
        ready = next(e for e in events if e["event"] == "bench_ready")
        assert ready["max_trials"] == 1, "no precision trials on top"
        trials = [e for e in events if e["event"] == "bench_trial"]
        assert len(trials) == 1 and trials[0]["precision"] == "bf16"
        done = next(e for e in events if e["event"] == "bench_done")
        assert (done["precision"], done["precision_mode"]) == ("bf16", "auto-accuracy")
        assert "precision_trials" not in done
        for key in ("precision", "precision_auto", "card_family"):
            assert key not in done["best"], f"a precision is never a pool ({key})"

    def test_balanced_where_emulated_bf16_is_slower_than_fp32_picks_fp32(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        pipe = _Pipe("fp32", per_page=0.02)
        pipe.engine = "paddle-manga"  # type: ignore[attr-defined]
        target = _Target(pipe, frozenset({"fp32", "fp16", "bf16"}), {"bf16": 0.04, "fp32": 0.02})
        pipe.precision_target = lambda: target  # type: ignore[attr-defined]
        events, configs = _bench_run(
            monkeypatch, pipe, precision="auto-balanced", engine="paddle-manga"
        )
        assert configs[0].precision == "fp32", "loaded in fp32: every cast starts from it"
        ready = next(e for e in events if e["event"] == "bench_ready")
        assert ready["max_trials"] == 3, "one widening trial plus one per candidate"
        tried = [e["precision"] for e in events if e["event"] == "bench_trial"]
        assert tried == ["bf16", "fp32"], "one trial per supported candidate, in list order"
        assert target.switched == ["bf16", "fp32", "fp32", "released"]
        done = next(e for e in events if e["event"] == "bench_done")
        assert (done["precision"], done["precision_mode"]) == ("fp32", "auto-balanced")
        assert [(t["precision"], t["chosen"]) for t in done["precision_trials"]] == [
            ("bf16", False), ("fp32", True),
        ]
        assert done["precision_why"].startswith("benchmark: fp32 ")

    def test_speed_with_one_supported_candidate_tries_nothing(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        pipe = _Pipe("fp32", per_page=0.02)
        pipe.engine = "paddle-manga"  # type: ignore[attr-defined]
        target = _Target(pipe, frozenset({"fp32"}), {"fp32": 0.02})
        pipe.precision_target = lambda: target  # type: ignore[attr-defined]
        events, _configs = _bench_run(
            monkeypatch, pipe, precision="auto-speed", engine="paddle-manga"
        )
        assert [e["precision"] for e in events if e["event"] == "bench_trial"] == ["fp32"]
        done = next(e for e in events if e["event"] == "bench_done")
        assert done["precision"] == "fp32" and "precision_trials" not in done

    def test_a_forced_format_is_what_it_loads_and_runs(self, monkeypatch: pytest.MonkeyPatch) -> None:
        events, configs = _bench_run(monkeypatch, _Pipe("fp16"), precision="fp16")
        assert configs[0].precision == "fp16"
        done = next(e for e in events if e["event"] == "bench_done")
        assert done["precision"] == "fp16" and "precision" not in done["best"]

    def test_an_engine_that_fixes_its_own_reports_no_precision(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        events, _configs = _bench_run(monkeypatch, _Pipe(None))
        trials = [e for e in events if e["event"] == "bench_trial"]
        assert trials and all("precision" not in t for t in trials)
        done = next(e for e in events if e["event"] == "bench_done")
        assert "precision" not in done and "precision" not in done["best"]
