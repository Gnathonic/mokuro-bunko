"""hayai-nova reads each line the same whatever else is in its batch.

The recognizer reads a page's line crops 16 at a time. The model repo's own
``generate()`` zero-pads every row of a batch to the longest one after the
DSCProjector and never masks that padding, so a line's text depended on its
page neighbours: 21 of 1,137 real crops (and 10 of 927 sidecar lines) read
differently in a batch of 16 than alone, identically on CPU fp32 and CUDA fp16.
``nova_generate`` is that loop with each row's projector padding masked out.

Three layers of test, each runnable where its dependencies are:

* the dev venv (no torch): the recognizer routes through ``nova_generate``,
  and the hayai-nova pin is still the one ``nova_generate`` was checked
  against (it re-implements the repo's loop, so a moved pin must re-check it);
* any environment with torch plus the pinned ``modeling_hayai.py`` in the
  Hugging Face cache: the mask itself, and the repo's REAL decoder code with
  small random weights -- where the repo's ``generate()`` is shown to depend
  on the batch and ``nova_generate`` is shown not to;
* where the real weights are cached too: the real model on synthetic crops.

The last two run under the engines environment's interpreter, e.g.
``<engines-env>/bin/python -m pytest --noconftest -p no:cacheprovider
tests/unit/test_hayai_nova_batching.py``. No page content is used: every
crop here is drawn by the test.
"""

from __future__ import annotations

import contextlib
import importlib.util
import shutil
import subprocess
import sys
import types
from pathlib import Path
from typing import Any

import pytest

from mokuro_bunko.ocr import engine_runner as runner

HAYAI_REPO = runner.RECOGNIZER_REPOS["hayai-nova"]


# -- the dev venv: routing and the pin tripwire --------------------------------


class TestTheRecognizerUsesTheMaskedLoop:
    def test_generate_goes_through_nova_generate_not_the_repos(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        class Tensor:
            def __init__(self, name: str) -> None:
                self.name = name
                self.device: str | None = None

            def to(self, device: str) -> Tensor:
                self.device = device
                return self

        class Model:
            def generate(self, **_kwargs: Any) -> list[str]:
                raise AssertionError("the repo's generate() does not mask a batch's padding")

        seen: dict[str, Any] = {}

        def fake_nova_generate(
            model: Any,
            tokenizer: Any,
            pixel_values: Any,
            pixel_attention_mask: Any,
            spatial_shapes: Any,
            *,
            max_new_tokens: int,
            precision: str,
        ) -> list[str]:
            seen.update(
                precision=precision,
                model=model,
                tokenizer=tokenizer,
                tensors=(pixel_values.name, pixel_attention_mask.name, spatial_shapes.name),
                devices={pixel_values.device, pixel_attention_mask.device, spatial_shapes.device},
                max_new_tokens=max_new_tokens,
            )
            return [" ﾄﾞｷﾄﾞｷ ", "ありがとう！"]

        monkeypatch.setattr(runner, "nova_generate", fake_nova_generate, raising=False)
        recognizer = runner.HayaiNovaRecognizer.__new__(runner.HayaiNovaRecognizer)
        recognizer.model = Model()
        recognizer.tokenizer = object()
        recognizer.device = "cuda:0"
        recognizer.patches = 512
        recognizer.fold = True
        recognizer.batch_size = 16
        recognizer.torch = types.SimpleNamespace(inference_mode=contextlib.nullcontext)
        recognizer.processor = lambda **kwargs: {
            key: Tensor(key) for key in ("pixel_values", "pixel_attention_mask", "spatial_shapes")
        }

        texts = recognizer(["crop-a", "crop-b"])

        assert seen["model"] is recognizer.model and seen["tokenizer"] is recognizer.tokenizer
        assert seen["tensors"] == ("pixel_values", "pixel_attention_mask", "spatial_shapes")
        assert seen["devices"] == {"cuda:0"}
        assert seen["max_new_tokens"] == runner.HAYAI_NOVA_MAX_NEW_TOKENS
        # What ``__init__`` resolved (``runner.resolve_precision``); a
        # recognizer built without it is fp32, autocast off.
        assert seen["precision"] == runner.PRECISION_FP32
        # Folded exactly as before: NFKC plus the recognizer's normalization.
        assert texts == [runner.normalize_text(" ﾄﾞｷﾄﾞｷ "), runner.normalize_text("ありがとう！")]

    def test_the_loop_was_checked_against_the_pinned_revision(self) -> None:
        """``nova_generate`` copies the repo's greedy loop; a new pin may change it.

        When this fails, the hayai-nova pin in ``REPO_REVISIONS`` moved: diff
        the new ``modeling_hayai.py``'s ``generate()`` against ``nova_generate``
        (and re-run the torch tests below against the new file) before moving
        ``NOVA_GENERATE_REVISION`` to match.
        """
        assert runner.pinned(HAYAI_REPO) == runner.NOVA_GENERATE_REVISION


# -- torch: the mask, and the repo's own decoder with small random weights -----


def _torch() -> Any:
    return pytest.importorskip("torch")


def _hayai_modeling() -> types.ModuleType:
    """The pinned ``modeling_hayai.py``, imported from the Hugging Face cache."""
    _torch()
    pytest.importorskip("transformers")
    hub = pytest.importorskip("huggingface_hub")
    try:
        paths = [
            Path(
                hub.hf_hub_download(
                    HAYAI_REPO, name, revision=runner.pinned(HAYAI_REPO), local_files_only=True
                )
            )
            for name in ("configuration_hayai.py", "modeling_hayai.py")
        ]
    except Exception as e:  # noqa: BLE001 - not cached is a skip, whatever the hub raises
        pytest.skip(f"the pinned hayai-nova code is not in the Hugging Face cache: {e}")
    name = "hayai_modeling_under_test"
    if name in sys.modules:
        return sys.modules[name]
    folder = str(paths[1].parent)
    # The file imports ``configuration_hayai`` relatively, falling back to a
    # plain import: outside a package, the plain one needs its folder.
    sys.path.insert(0, folder)
    try:
        spec = importlib.util.spec_from_file_location(name, paths[1])
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        sys.modules[name] = module
        spec.loader.exec_module(module)
    finally:
        sys.path.remove(folder)
    return module


class TestTheKeyBias:
    def test_each_rows_own_projector_padding_and_nothing_else(self) -> None:
        torch = _torch()
        bias = runner.nova_key_bias(torch.tensor([3, 5, 1]), 5, 8)
        assert tuple(bias.shape) == (3, 1, 1, 8)
        assert bias.dtype == torch.float32
        masked = (bias[:, 0, 0, :] < -1e8).tolist()
        # Row 0 has 3 of the 5 vision slots, row 1 all 5, row 2 one; the three
        # text slots after them are left to the causal mask for every row.
        assert masked == [
            [False, False, False, True, True, False, False, False],
            [False] * 8,
            [False, True, True, True, True, False, False, False],
        ]
        assert set(bias[bias != 0].tolist()) == {-1e9}

    def test_a_row_never_masks_every_key(self) -> None:
        # Softmax over nothing is NaN: every row keeps its own vision tokens
        # and every text slot.
        torch = _torch()
        bias = runner.nova_key_bias(torch.tensor([1, 1]), 6, 9)
        assert bool(((bias[:, 0, 0, :] == 0).sum(dim=-1) >= 1).all())


class _Tokenizer:
    # End and pad sit outside the tiny vocabulary, so argmax never picks
    # them: every row reads all its tokens and a difference anywhere shows.
    bos_token_id, eos_token_id, pad_token_id = 1, 100, 101

    def decode(self, ids: list[int], skip_special_tokens: bool = True) -> str:
        assert skip_special_tokens
        return " ".join(str(i) for i in ids)


def _tiny_model(modeling: types.ModuleType, *, d_in: int = 12, d_vision: int = 32) -> Any:
    """The repo's own decoder at a small size, random weights, fp32.

    The vision tower is a per-patch linear map: NaFlex masks its own padding,
    so a stand-in that mixes nothing across patches is exactly as batch-blind
    as the real tower -- which puts the whole of any batch dependence in the
    decoder, where the bug is.
    """
    torch = _torch()
    torch.manual_seed(1234)

    class Tower(torch.nn.Module):  # type: ignore[misc]
        def __init__(self) -> None:
            super().__init__()
            self.proj = torch.nn.Linear(d_in, d_vision)

        def forward(self, pixel_values: Any, pixel_attention_mask: Any, spatial_shapes: Any) -> Any:
            del pixel_attention_mask, spatial_shapes
            return types.SimpleNamespace(last_hidden_state=self.proj(pixel_values))

    class TinyNova(torch.nn.Module):  # type: ignore[misc]
        def __init__(self) -> None:
            super().__init__()
            self.vision_encoder = Tower()
            self.decoder = modeling.VisualCausalOCRDecoder(
                vocab_size=48, d_model=64, d_vision=d_vision, d_ffn=128, n_layers=3
            )

    # ``nova_generate`` finds the repo's rope helper through the model's
    # module, as it does for the real (trust_remote_code) class.
    TinyNova.__module__ = modeling.__name__
    return TinyNova().eval()


def _crops(torch: Any, shapes: list[tuple[int, int]], d_in: int = 12) -> list[tuple[Any, Any]]:
    """One (patches, shape) per crop: ``h * w`` random patches of ``d_in``."""
    generator = torch.Generator().manual_seed(99)
    return [
        (torch.randn(h * w, d_in, generator=generator), torch.tensor([h, w]))
        for h, w in shapes
    ]


def _batch(torch: Any, crops: list[tuple[Any, Any]]) -> tuple[Any, Any, Any]:
    """NaFlex's layout: patches zero-padded to the longest row, a mask, shapes."""
    longest = max(int(p.shape[0]) for p, _ in crops)
    pixels = torch.zeros(len(crops), longest, crops[0][0].shape[1])
    mask = torch.zeros(len(crops), longest, dtype=torch.long)
    for i, (patches, _shape) in enumerate(crops):
        pixels[i, : patches.shape[0]] = patches
        mask[i, : patches.shape[0]] = 1
    return pixels, mask, torch.stack([shape for _, shape in crops])


class TestTheReposDecoderWithRandomWeights:
    # A short line, a long one, one with an odd patch grid (the projector
    # pads it to even by replication) and one exactly as long as the longest.
    SHAPES = [(2, 4), (4, 12), (3, 5), (4, 12), (2, 2)]
    TOKENS = 12

    def _reads(self, generate: Any, model: Any, crops: list[Any]) -> list[str]:
        torch = _torch()
        with torch.inference_mode():
            return list(generate(model, _Tokenizer(), *_batch(torch, crops)))

    def test_the_repos_generate_depends_on_the_batch(self) -> None:
        """The bug, reproduced on the repo's own code -- this test's control.

        If this ever passes by reading the same batched as alone, the repo's
        loop changed and the next test proves nothing: re-check the pin.
        """
        torch = _torch()
        modeling = _hayai_modeling()
        model = _tiny_model(modeling)
        crops = _crops(torch, self.SHAPES)

        def repo(model: Any, tokenizer: Any, *tensors: Any) -> list[str]:
            return modeling.HayaiModel.generate(
                model, *tensors, tokenizer, max_new_tokens=self.TOKENS
            )

        batched = self._reads(repo, model, crops)
        alone = [self._reads(repo, model, [crop])[0] for crop in crops]
        # The longest rows have no padding and read the same; every shorter
        # one is read beside zero tokens it attends to.
        assert batched[1] == alone[1] and batched[3] == alone[3]
        assert [i for i in (0, 2, 4) if batched[i] != alone[i]], (batched, alone)

    def test_nova_generate_reads_each_row_as_it_reads_alone(self) -> None:
        torch = _torch()
        modeling = _hayai_modeling()
        model = _tiny_model(modeling)
        crops = _crops(torch, self.SHAPES)

        def nova(model: Any, tokenizer: Any, *tensors: Any) -> list[str]:
            return runner.nova_generate(model, tokenizer, *tensors, max_new_tokens=self.TOKENS)

        def repo(model: Any, tokenizer: Any, *tensors: Any) -> list[str]:
            return modeling.HayaiModel.generate(
                model, *tensors, tokenizer, max_new_tokens=self.TOKENS
            )

        batched = self._reads(nova, model, crops)
        alone = [self._reads(nova, model, [crop])[0] for crop in crops]
        assert batched == alone
        # ... and a batch of one IS the repo's loop, token for token.
        assert alone == [self._reads(repo, model, [crop])[0] for crop in crops]
        assert all(len(text.split()) == self.TOKENS for text in batched), batched

    def test_every_order_of_the_batch_reads_the_same(self) -> None:
        torch = _torch()
        modeling = _hayai_modeling()
        model = _tiny_model(modeling)
        crops = _crops(torch, self.SHAPES)

        def nova(model: Any, tokenizer: Any, *tensors: Any) -> list[str]:
            return runner.nova_generate(model, tokenizer, *tensors, max_new_tokens=self.TOKENS)

        forward = self._reads(nova, model, crops)
        backward = self._reads(nova, model, crops[::-1])[::-1]
        assert forward == backward


# -- the real model, on crops drawn here ---------------------------------------


def _cjk_font() -> str | None:
    """A font with Japanese glyphs, if this machine has one."""
    fc_match = shutil.which("fc-match")
    if fc_match is None:
        return None
    try:
        out = subprocess.run(
            [fc_match, "-f", "%{file}", ":lang=ja"], capture_output=True, text=True, timeout=10
        )
    except (OSError, subprocess.SubprocessError):
        return None
    path = out.stdout.strip()
    return path if path and Path(path).is_file() else None


# Written for this test; nothing here comes from a page.
LINES = [
    "今日はいい天気ですね", "ドキドキ", "ありがとう！", "そうか…", "え？",
    "明日の朝、駅の前で待ってる", "ふふっ", "本当に大丈夫なの？", "ガタン", "行こう",
    "静かにしてください", "よし！", "わかった、わかった", "それで？", "はあ…", "二つ目の角を右",
]


def _draw_crops() -> list[Any]:
    image_module = pytest.importorskip("PIL.Image")
    draw_module = pytest.importorskip("PIL.ImageDraw")
    font_module = pytest.importorskip("PIL.ImageFont")
    font_path = _cjk_font()
    if font_path is None:
        pytest.skip("no font with Japanese glyphs on this machine")
    font = font_module.truetype(font_path, 28)
    crops = []
    for index, text in enumerate(LINES):
        vertical = index % 3 != 2
        cell = 32
        if vertical:
            image = image_module.new("RGB", (cell + 8, cell * len(text) + 8), "white")
            draw = draw_module.Draw(image)
            for i, char in enumerate(text):
                draw.text((4, 4 + i * cell), char, fill="black", font=font)
        else:
            image = image_module.new("RGB", (cell * len(text) + 8, cell + 8), "white")
            draw_module.Draw(image).text((4, 4), text, fill="black", font=font)
        crops.append(image)
    return crops


class TestTheRealModel:
    def test_a_batch_reads_every_crop_as_it_reads_alone(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _torch()
        _hayai_modeling()  # skips unless the pinned code is cached
        crops = _draw_crops()
        # Cached or skipped: never a download from a test.
        monkeypatch.setenv("HF_HUB_OFFLINE", "1")
        monkeypatch.setenv("TRANSFORMERS_OFFLINE", "1")
        try:
            recognizer = runner.HayaiNovaRecognizer(512, device="cpu")
        except OSError as e:
            pytest.skip(f"the hayai-nova weights are not cached: {e}")
        assert recognizer.batch_size == runner.HAYAI_NOVA_BATCH == len(crops)

        batched = recognizer(crops)
        alone = [recognizer([crop])[0] for crop in crops]

        assert batched == alone
        assert sum(1 for text in batched if text) >= len(crops) // 2, batched

