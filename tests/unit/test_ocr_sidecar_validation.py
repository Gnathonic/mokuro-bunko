"""Unit tests for mokuro sidecar validation and startup cleanup."""

from __future__ import annotations

import gzip
import json
from pathlib import Path

from mokuro_bunko.ocr.processor import OCRProcessor
from mokuro_bunko.ocr.watcher import OCRWorker


def test_sidecar_validation_plain_and_gz(tmp_path: Path) -> None:
    """Processor validates both plain and gzip sidecar payloads."""
    processor = OCRProcessor(storage_path=tmp_path)

    valid_plain = tmp_path / "ok.mokuro"
    valid_plain.write_text(json.dumps({"ok": True}), encoding="utf-8")
    invalid_plain = tmp_path / "bad.mokuro"
    invalid_plain.write_text("{bad", encoding="utf-8")

    valid_gz = tmp_path / "ok.mokuro.gz"
    with gzip.open(valid_gz, "wt", encoding="utf-8") as f:
        json.dump({"ok": True}, f)
    invalid_gz = tmp_path / "bad.mokuro.gz"
    invalid_gz.write_bytes(b"not-gzip")

    assert processor.is_valid_mokuro_sidecar(valid_plain)
    assert not processor.is_valid_mokuro_sidecar(invalid_plain)
    assert processor.is_valid_mokuro_sidecar(valid_gz)
    assert not processor.is_valid_mokuro_sidecar(invalid_gz)


def test_worker_startup_removes_corrupt_sidecars(tmp_path: Path) -> None:
    """Startup scrub removes corrupt sidecars while preserving valid ones."""
    library = tmp_path / "library" / "Series"
    library.mkdir(parents=True)

    valid = library / "v01.mokuro"
    valid.write_text(json.dumps({"ok": True}), encoding="utf-8")
    invalid = library / "v02.mokuro"
    invalid.write_text("{oops", encoding="utf-8")

    worker = OCRWorker(storage_path=tmp_path, poll_interval=0.1)
    removed = worker._remove_corrupt_sidecars()

    assert removed == 1
    assert valid.exists()
    assert not invalid.exists()


def test_workspace_sidecar_found_for_glob_metacharacter_names(tmp_path: Path) -> None:
    """Volume names with [brackets] etc. are matched literally, not as globs.

    ``[荒木飛呂彦] ...`` used to be read as a one-character class, so the
    sidecar mokuro had just written was never found.
    """
    processor = OCRProcessor(storage_path=tmp_path)
    for stem in (
        "[荒木飛呂彦] ジョジョの奇妙な冒険 ファントムブラッド カラー版 02",
        "Vol*1?",
    ):
        workspace = tmp_path / f"ws-{len(stem)}"
        extract_dir = workspace / stem
        extract_dir.mkdir(parents=True)
        sidecar = workspace / f"{stem}.mokuro"
        sidecar.write_text(json.dumps({"ok": True}), encoding="utf-8")

        assert processor._collect_valid_workspace_sidecar(extract_dir, workspace) == sidecar
        assert processor._collect_workspace_sidecar(extract_dir, workspace) == sidecar
