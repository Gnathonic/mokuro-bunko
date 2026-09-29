"""Is every Hugging Face model a row loads already in the local cache?

A session start used to make Hub round trips for models it already had --
about 0.8-0.9 s per start on both processors, and a network dependency for a
start that needs none (perf diagnosis F4). With every repo a row loads fully
in the cache, the runner's environment says ``HF_HUB_OFFLINE=1`` and
``TRANSFORMERS_OFFLINE=1`` (`OCRProcessor.ocr_env`); anything short of that
stays online, so a model never downloaded still downloads.

Read straight off the Hub cache's own layout -- ``models--<org>--<name>/``
with ``snapshots/<commit>/`` and ``refs/main`` -- because this server's
environment does not have ``huggingface_hub`` (the engine environments do).
Pure stdlib; safe to import from the server.
"""

from __future__ import annotations

import os
from collections.abc import Mapping
from pathlib import Path

from mokuro_bunko.ocr.engines import DEFAULT_DETECTOR, ENGINES


def hub_cache_dir(env: Mapping[str, str] | None = None) -> Path:
    """Where ``huggingface_hub`` keeps models under this environment."""
    env = os.environ if env is None else env
    explicit = env.get("HF_HUB_CACHE") or env.get("HUGGINGFACE_HUB_CACHE")
    if explicit:
        return Path(explicit).expanduser()
    home = env.get("HF_HOME")
    if home:
        return Path(home).expanduser() / "hub"
    cache = Path(env.get("XDG_CACHE_HOME") or Path.home() / ".cache").expanduser()
    return cache / "huggingface" / "hub"


def repo_cached(repo: str, revision: str | None, cache: Path) -> bool:
    """Whether one repo is fully downloaded at ``revision`` (``main`` if None).

    Fully: its snapshot exists and is not empty, and no blob of the repo is
    still ``.incomplete`` -- a download that was cut off. A model that has
    loaded once has every file it loads in its snapshot.
    """
    folder = cache / f"models--{repo.replace('/', '--')}"
    if revision is None:
        try:
            revision = (folder / "refs" / "main").read_text(encoding="utf-8").strip()
        except OSError:
            return False
        if not revision:
            return False
    snapshot = folder / "snapshots" / revision
    try:
        if not any(snapshot.iterdir()):
            return False
        return not any((folder / "blobs").glob("*.incomplete"))
    except OSError:
        return False


def _detector_repo(detector: str) -> tuple[str, str | None] | None:
    """The Hub repo a detector adapter loads, with its pin; None for none."""
    if detector == "animetext":
        from mokuro_bunko.ocr.detectors import animetext

        return animetext.REPO, animetext.REVISION
    if detector == "ppocr-manga":
        from mokuro_bunko.ocr import ppocr

        return ppocr.REPO_ID, ppocr.REPO_REVISION
    return None  # ctd: weights from mokuro's own release, not a Hub repo


def row_repos(engine: str, detector: str | None) -> list[tuple[str, str | None]] | None:
    """Every ``(repo, pinned revision or None for main)`` a row loads.

    None for an engine this table does not know: such a row stays online.
    """
    from mokuro_bunko.ocr.engine_runner import (
        HAYAI_VISION_REPO,
        PADDLE_BASE_REPO,
        REPO_REVISIONS,
    )

    spec = ENGINES.get(engine)
    if spec is None:
        return None
    if spec.serve_module is not None or spec.uses_mokuro_env:
        # mokuro's own manga-ocr, at `main` as mokuro loads it; its text
        # detector's weights come from a release download, not the Hub.
        return [(spec.recognizer, None)]
    repos: list[tuple[str, str | None]] = []
    if engine == "hayai-nova":
        repos += [(spec.recognizer, REPO_REVISIONS[spec.recognizer]),
                  (HAYAI_VISION_REPO, REPO_REVISIONS[HAYAI_VISION_REPO])]
    elif engine == "paddle-manga":
        repos += [(spec.recognizer, REPO_REVISIONS[spec.recognizer]),
                  (PADDLE_BASE_REPO, REPO_REVISIONS[PADDLE_BASE_REPO])]
    elif spec.detector is None:
        return None
    used = spec.detector or detector or DEFAULT_DETECTOR
    found = _detector_repo(used)
    if found is not None and found not in repos:
        repos.append(found)
    return repos


def row_models_cached(
    engine: str, detector: str | None, env: Mapping[str, str] | None = None
) -> bool:
    """Whether every model this row loads is fully in the local Hub cache."""
    repos = row_repos(engine, detector)
    if not repos:
        return False
    cache = hub_cache_dir(env)
    return all(repo_cached(repo, revision, cache) for repo, revision in repos)
