"""A recognizer on a card keeps torch to a small CPU pool (four threads).

torch's OpenMP pool defaults to one thread per core. With the recognizer on a
GPU, the CPU side of each call (the image processor, the loop's bookkeeping)
is small and its parallel regions are mostly barrier waits -- harmless on an
idle host, ruinous beside a neighbour: measured on tower (RTX 4090, 48 CPU
threads, hayai-nova + ppocr-manga, 669 pages), 24 busy-spinning processes
took the runner from 9.0 to 2.33 pages/s with the default pool and to 7.6
with one thread, which idle read the same 9.0 on 30% less CPU; four threads
held the same beside that neighbour. On the workstation (RX 9070 XT, 32 CPU
threads) one thread cost ~5% idle (4.56 vs 4.80 pages/s) where four cost
nothing, and beside 20 spinners four read 3.45 against 3.60 for one and 1.49
for the default pool. So a torch recognizer on a card sets torch's pool to
four threads once it has loaded, and
in each thread that calls it -- the load itself (weights, the LoRA merge)
keeps every core -- and a value the operator set in
``OMP_NUM_THREADS``/``MKL_NUM_THREADS`` is theirs and left alone. On the CPU the pool is the recognizer's compute and
is never touched.

The recognizers are built here against stand-in torch/transformers modules:
nothing is loaded, only what ``__init__`` asks torch to do is recorded.
"""

from __future__ import annotations

import sys
import threading
import types
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner


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


class _Loader:
    @staticmethod
    def from_pretrained(*_args: Any, **_kwargs: Any) -> Any:
        model = _Model()
        # What the paddle recognizer asks of its processor.
        model.tokenizer = types.SimpleNamespace(padding_side="right")  # type: ignore[attr-defined]
        model.apply_chat_template = lambda *a, **k: "prompt"  # type: ignore[attr-defined]
        return model


@pytest.fixture
def torch_calls(monkeypatch: pytest.MonkeyPatch) -> list[int]:
    """Stand-in torch & co; returns every ``torch.set_num_threads`` argument."""
    calls: list[int] = []
    torch = types.ModuleType("torch")
    torch.set_num_threads = calls.append  # type: ignore[attr-defined]
    torch.bfloat16, torch.float16, torch.float32 = "bfloat16", "float16", "float32"  # type: ignore[attr-defined]
    torch.cuda = types.SimpleNamespace(is_available=lambda: True)  # type: ignore[attr-defined]
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
    monkeypatch.delenv("OMP_NUM_THREADS", raising=False)
    monkeypatch.delenv("MKL_NUM_THREADS", raising=False)
    return calls


def _build(engine: str, device: str | None) -> Any:
    return runner.load_recognizer(engine, device=device)


@pytest.mark.parametrize("engine", ["hayai-nova", "paddle-manga"])
class TestTheCap:
    def test_on_a_card_torch_gets_four_threads(
        self, engine: str, torch_calls: list[int], capsys: pytest.CaptureFixture[str]
    ) -> None:
        _build(engine, "gpu:0")
        assert torch_calls == [runner.GPU_ENGINE_TORCH_THREADS] == [4]
        said = capsys.readouterr()
        assert "[runner] torch CPU threads: 4 (engine on cuda:0)" in said.out + said.err

    def test_auto_that_finds_a_card_is_a_card(self, engine: str, torch_calls: list[int]) -> None:
        _build(engine, None)
        assert torch_calls == [4]

    def test_on_the_cpu_torch_keeps_its_pool(self, engine: str, torch_calls: list[int]) -> None:
        _build(engine, "cpu")
        assert torch_calls == []

    @pytest.mark.parametrize("variable", ["OMP_NUM_THREADS", "MKL_NUM_THREADS"])
    def test_a_value_the_operator_set_is_theirs(
        self,
        engine: str,
        variable: str,
        torch_calls: list[int],
        monkeypatch: pytest.MonkeyPatch,
    ) -> None:
        monkeypatch.setenv(variable, "6")
        _build(engine, "gpu:0")
        assert torch_calls == []


def test_the_cap_comes_after_the_load(torch_calls: list[int]) -> None:
    """The weights load (and paddle's LoRA merge) on every core; the cap is last."""
    order: list[str] = []

    class Timed(_Loader):
        @staticmethod
        def from_pretrained(*args: Any, **kwargs: Any) -> Any:
            order.append("load")
            return _Loader.from_pretrained(*args, **kwargs)

    sys.modules["transformers"].AutoModel = Timed  # type: ignore[attr-defined]
    sys.modules["torch"].set_num_threads = lambda n: order.append(f"threads={n}")  # type: ignore[attr-defined]
    _build("hayai-nova", "gpu:0")
    assert order[-1] == "threads=4" and "load" in order[:-1]


# --- the cap reaches the thread that calls the recognizer --------------------
#
# ``torch.set_num_threads`` sets the CALLING thread's OpenMP/MKL count and a
# global that another thread only picks up lazily, at its first ATen
# parallel_for. Probed on the workstation's engines env (torch 2.13.0+rocm7.1,
# MKL, OpenMP backend) with the cap set from another thread: a thread that
# had already computed kept 16 threads (cpu/wall 28 on a matmul), and so did
# a FRESH thread whose first op was a matmul (26.8 -- BLAS does not go
# through parallel_for), while one that called set_num_threads itself ran at
# 0.97-1.1. The load runs on the ``ocr-load`` thread and the engine stage's
# workers call the recognizer, so the cap is applied again in each calling
# thread, once.


def _calls_by_thread(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, int]]:
    # By thread NAME: a joined thread's ident is handed to the next one.
    calls: list[tuple[str, int]] = []
    monkeypatch.setattr(
        sys.modules["torch"], "set_num_threads",
        lambda n: calls.append((threading.current_thread().name, n)), raising=False,
    )
    return calls


def _call_on_a_thread(recognizer: Any, name: str, times: int = 2) -> str:
    def work() -> None:
        for _ in range(times):
            assert recognizer([]) == []

    thread = threading.Thread(target=work, name=name)
    thread.start()
    thread.join()
    return name


@pytest.mark.parametrize("engine", ["hayai-nova", "paddle-manga"])
def test_each_calling_thread_is_capped_once(
    engine: str, torch_calls: list[int], monkeypatch: pytest.MonkeyPatch
) -> None:
    calls = _calls_by_thread(monkeypatch)
    recognizer = _build(engine, "gpu:0")
    worker = _call_on_a_thread(recognizer, "engine-0")
    assert [n for name, n in calls if name == worker] == [4]  # once, not per call
    other = _call_on_a_thread(recognizer, "engine-1")
    assert [n for name, n in calls if name == other] == [4]


@pytest.mark.parametrize("engine", ["hayai-nova", "paddle-manga"])
def test_no_calling_thread_is_capped_on_the_cpu_or_by_the_operator(
    engine: str, torch_calls: list[int], monkeypatch: pytest.MonkeyPatch
) -> None:
    calls = _calls_by_thread(monkeypatch)
    _call_on_a_thread(_build(engine, "cpu"), "engine-cpu")
    monkeypatch.setenv("OMP_NUM_THREADS", "6")
    _call_on_a_thread(_build(engine, "gpu:0"), "engine-operator")
    assert calls == []
