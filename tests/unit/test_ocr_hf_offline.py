"""A session start makes no Hugging Face round trips for models it already has.

Every session start used to ask the Hub about models already in the cache --
about 0.8-0.9 s on both processors, and a network dependency for a start
that needs none (perf diagnosis F4). The runner's environment now carries
``HF_HUB_OFFLINE=1`` and ``TRANSFORMERS_OFFLINE=1`` when -- and only when --
every repo the row loads is fully in the cache: a model that has never been
downloaded still is, online.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest

from mokuro_bunko.ocr import hf_cache
from mokuro_bunko.ocr.generations import GenerationSpec
from mokuro_bunko.ocr.processor import OCRProcessor


def _cache(root: Path, repo: str, revision: str, *, main: bool = False,
           incomplete: bool = False) -> None:
    folder = root / f"models--{repo.replace('/', '--')}"
    snapshot = folder / "snapshots" / revision
    snapshot.mkdir(parents=True)
    (snapshot / "config.json").write_text("{}", encoding="utf-8")
    (folder / "blobs").mkdir(exist_ok=True)
    if main:
        (folder / "refs").mkdir(exist_ok=True)
        (folder / "refs" / "main").write_text(revision, encoding="utf-8")
    if incomplete:
        (folder / "blobs" / "abc123.incomplete").write_bytes(b"half")


def _row(engine: str, detector: str | None = None) -> GenerationSpec:
    return GenerationSpec(id="g-1", name="row", engine=engine, detector=detector,
                          primary=True, enabled=True)


def _env(tmp_path: Path, monkeypatch: pytest.MonkeyPatch, row: GenerationSpec) -> dict[str, str]:
    monkeypatch.setenv("HF_HUB_CACHE", str(tmp_path / "hub"))
    monkeypatch.delenv("HF_HUB_OFFLINE", raising=False)
    monkeypatch.delenv("TRANSFORMERS_OFFLINE", raising=False)
    processor = OCRProcessor(
        storage_path=tmp_path, generations=[row],
        engines_python_path=Path(sys.executable), python_path=Path(sys.executable),
    )
    return processor.ocr_env(row)


def _cache_all(root: Path, engine: str, detector: str | None) -> None:
    repos = hf_cache.row_repos(engine, detector)
    assert repos is not None
    for repo, revision in repos:
        _cache(root, repo, revision or "0" * 40, main=revision is None)


class TestTheRowsRepos:
    def test_each_engine_names_the_repos_it_loads(self) -> None:
        mokuro = hf_cache.row_repos("mokuro", None)
        assert mokuro == [("kha-white/manga-ocr-base", None)]
        hayai = dict(hf_cache.row_repos("hayai-nova", "ctd") or [])
        assert set(hayai) == {
            "JustANormalTinkerer/hayai-ocr-v2.5-nova", "google/siglip2-base-patch16-naflex"
        }, "ctd's weights are not a Hub repo"
        assert all(hayai.values()), "pinned revisions"
        paddle = dict(hf_cache.row_repos("paddle-manga", "animetext") or [])
        assert "deepghs/AnimeText_yolo" in paddle and "PaddlePaddle/PaddleOCR-VL-1.6" in paddle
        assert dict(hf_cache.row_repos("ppocr-manga", "ctd") or []) == {
            "Kellenok/PP-OCRv6_manga": hf_cache.row_repos("ppocr-manga", None)[0][1]  # type: ignore[index]
        }, "an engine with its own detector ignores the row's"

    def test_an_engine_it_does_not_know_is_none(self) -> None:
        assert hf_cache.row_repos("some-future-engine", None) is None


class TestTheEnvironment:
    @pytest.mark.parametrize(
        ("engine", "detector"),
        [("mokuro", None), ("hayai-nova", "ctd"), ("paddle-manga", "animetext"),
         ("hayai-nova", "ppocr-manga"), ("ppocr-manga", None)],
    )
    def test_every_model_cached_means_offline(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch, engine: str,
        detector: str | None,
    ) -> None:
        _cache_all(tmp_path / "hub", engine, detector)
        env = _env(tmp_path, monkeypatch, _row(engine, detector))
        assert env["HF_HUB_OFFLINE"] == "1"
        assert env["TRANSFORMERS_OFFLINE"] == "1"

    def test_one_model_missing_stays_online(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        repos = hf_cache.row_repos("hayai-nova", "ppocr-manga")
        assert repos is not None
        repo, revision = repos[0]
        _cache(tmp_path / "hub", repo, revision or "0" * 40)
        env = _env(tmp_path, monkeypatch, _row("hayai-nova", "ppocr-manga"))
        assert "HF_HUB_OFFLINE" not in env and "TRANSFORMERS_OFFLINE" not in env

    def test_a_half_downloaded_model_stays_online(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _cache(tmp_path / "hub", "kha-white/manga-ocr-base", "a" * 40, main=True,
               incomplete=True)
        env = _env(tmp_path, monkeypatch, _row("mokuro"))
        assert "HF_HUB_OFFLINE" not in env

    def test_a_pinned_revision_the_cache_does_not_have_stays_online(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        for repo, _revision in hf_cache.row_repos("hayai-nova", "ctd") or []:
            _cache(tmp_path / "hub", repo, "f" * 40)  # an older commit
        env = _env(tmp_path, monkeypatch, _row("hayai-nova", "ctd"))
        assert "HF_HUB_OFFLINE" not in env

    def test_a_choice_the_operator_made_is_left_alone(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _cache_all(tmp_path / "hub", "mokuro", None)
        monkeypatch.setenv("HF_HUB_OFFLINE", "0")
        monkeypatch.setenv("HF_HUB_CACHE", str(tmp_path / "hub"))
        processor = OCRProcessor(
            storage_path=tmp_path, generations=[_row("mokuro")],
            engines_python_path=Path(sys.executable), python_path=Path(sys.executable),
        )
        env = processor.ocr_env(_row("mokuro"))
        assert env["HF_HUB_OFFLINE"] == "0"

    def test_the_session_command_s_environment_carries_it(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _cache_all(tmp_path / "hub", "hayai-nova", "ctd")
        monkeypatch.setenv("HF_HUB_CACHE", str(tmp_path / "hub"))
        monkeypatch.delenv("HF_HUB_OFFLINE", raising=False)
        row = _row("hayai-nova", "ctd")
        processor = OCRProcessor(
            storage_path=tmp_path, generations=[row],
            engines_python_path=Path(sys.executable), python_path=Path(sys.executable),
        )
        session = processor.open_session(row, tmp_path / "s.log")
        bench = processor.open_bench(row, tmp_path / "sample", tmp_path / "b.log")
        assert session._env is not None and session._env["HF_HUB_OFFLINE"] == "1"
        assert bench._env is not None and bench._env["HF_HUB_OFFLINE"] == "1"

    def test_the_cache_follows_hf_home(self, tmp_path: Path) -> None:
        assert hf_cache.hub_cache_dir({"HF_HOME": str(tmp_path / "h")}) == tmp_path / "h" / "hub"
        assert hf_cache.hub_cache_dir({"HF_HUB_CACHE": str(tmp_path / "c")}) == tmp_path / "c"
        assert hf_cache.hub_cache_dir(
            {"XDG_CACHE_HOME": str(tmp_path / "x")}
        ) == tmp_path / "x" / "huggingface" / "hub"
