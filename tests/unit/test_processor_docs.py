"""The shipped examples are files the loaders accept, and the docs say what matters."""

from __future__ import annotations

import re
from pathlib import Path

import yaml

from mokuro_bunko.config import Config
from mokuro_bunko.processor.config import load_processor_config

ROOT = Path(__file__).parents[2]


def test_the_processor_example_is_a_file_the_loader_accepts() -> None:
    config = load_processor_config(ROOT / "docs" / "processor.example.yaml")
    assert config.processor.name == "gpu-box"
    assert config.library.url == "https://library.example:8080"
    assert config.processor.max_sessions == 1
    assert config.processor.archive_memory_mb == 2048


def test_the_processor_example_leaves_storage_to_the_platform_default() -> None:
    """An explicit ~/.local/share storage would be a Linux path on Windows."""
    from mokuro_bunko.processor.config import default_storage_path

    example = ROOT / "docs" / "processor.example.yaml"
    assert load_processor_config(example).processor.storage == default_storage_path()
    text = example.read_text(encoding="utf-8")
    assert "%LOCALAPPDATA%" in text
    assert "uv run mokuro-bunko processor setup" in text
    assert "uv run mokuro-bunko processor serve   --config processor.yaml" in text


def test_the_config_example_carries_both_new_keys() -> None:
    data = yaml.safe_load((ROOT / "config.example.yaml").read_text(encoding="utf-8"))
    config = Config.from_dict(data)
    assert config.ocr.local_processing is True
    assert config.ocr.autobench is True
    assert "local_processing" in data["ocr"] and "autobench" in data["ocr"]


def test_autobench_round_trips_through_the_config() -> None:
    config = Config.from_dict({"ocr": {"autobench": False, "local_processing": False}})
    assert config.ocr.autobench is False
    assert config.to_dict()["ocr"]["autobench"] is False


def test_the_configuration_guide_documents_the_processor() -> None:
    guide = (ROOT / "docs" / "configuration.md").read_text(encoding="utf-8")
    assert "#### Remote OCR processors" in guide
    assert re.search(r"\| `processor` \|", guide), "the role matrix learns the role"
    assert re.search(r"\| `local_processing` \| boolean \|", guide)
    assert re.search(r"\| `autobench` \| boolean \|", guide)
    assert "--detector" in guide


def test_the_guides_document_the_archive_transfer() -> None:
    guide = (ROOT / "docs" / "configuration.md").read_text(encoding="utf-8")
    internals = (ROOT / "docs" / "ocr-internals.md").read_text(encoding="utf-8")
    deployment = (ROOT / "docs" / "deployment.md").read_text(encoding="utf-8")
    assert "archive_memory_mb" in guide
    assert "**Protocol 2: whole archives.**" in internals
    updating = deployment.split("### Updating a library and its processors", 1)[1]
    steps = [updating.index(word) for word in ("**Stop**", "**Update**", "**Restart**",
                                               "**Start**")]
    assert steps == sorted(steps), "stop, update, restart the library, start"
    assert "If-Range" in deployment


def test_skip_is_documented_as_no_ocr_on_this_machine_not_none_at_all() -> None:
    """B11: since `skip` became "local processing off", a skip server holds
    its queue for a processor. Docs that still promise "Disable OCR, WebDAV
    server only" describe a server that no longer exists."""
    guide = (ROOT / "docs" / "configuration.md").read_text(encoding="utf-8")
    example = (ROOT / "config.example.yaml").read_text(encoding="utf-8")
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    assert "Disable OCR, WebDAV server only" not in guide
    assert "No OCR, WebDAV server only" not in example
    row = next(line for line in guide.splitlines() if line.startswith("| `skip` |"))
    assert "local_processing: false" in row
    unreleased = changelog.split("## [", 2)[1]
    changed = unreleased.split("### Changed", 1)[1].split("###", 1)[0]
    assert "`ocr.backend: skip`" in changed, "a Changed entry, not only an Added aside"
