"""The settings page counts volumes uploaded short of pages without asking every volume.

Each ask stats an archive and its sidecar and queries the metadata cache;
asking all 12k volumes of a large library on every load made the OCR
settings page take seconds. The metadata pass already keeps per-series damage
totals, so only a damaged series -- or one it has not compiled yet -- is
asked volume by volume.
"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace
from typing import Any

import pytest

from mokuro_bunko.admin import api as admin_api
from mokuro_bunko.admin.api import AdminAPI
from mokuro_bunko.database import Database
from mokuro_bunko.library_index import LibrarySnapshot, SeriesSnapshot, VolumeSnapshot
from mokuro_bunko.ocr.generations import parse_generation_list


def _series(name: str, volumes: int) -> SeriesSnapshot:
    return SeriesSnapshot(
        name=name,
        cover=None,
        volumes=tuple(
            VolumeSnapshot(name=f"V{v}", has_cbz=True, has_mokuro=True, has_mokuro_gz=False, cover=None)
            for v in range(volumes)
        ),
    )


def _catalog_row(name: str, missing: int) -> Any:
    return {
        "series_key": name.lower(), "folder_name": name, "cover_path": None,
        "volume_count": 3, "latest_volume_modified": 0.0, "total_pages": 0,
        "total_chars": 0, "missing_pages": missing, "damaged_volumes": 1 if missing else 0,
    }


def test_only_damaged_or_uncompiled_series_are_asked(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    db = Database(tmp_path / "mokuro.db")
    db.upsert_catalog_series(_catalog_row("Clean", 0))
    db.upsert_catalog_series(_catalog_row("Damaged", 12))
    snapshot = LibrarySnapshot(
        series=(_series("Clean", 3), _series("Damaged", 3), _series("New", 2)),
        pending_ocr=(), pending_thumbnails=0,
    )
    asked: list[str] = []

    def fake_missing(_db: Any, _library: Path, cbz: Path) -> int:
        asked.append(f"{cbz.parent.name}/{cbz.stem}")
        return 5 if cbz.stem == "V0" else 0

    monkeypatch.setattr(admin_api, "cached_missing_pages", fake_missing)
    api = AdminAPI.__new__(AdminAPI)
    api.db = db
    api.library_index = SimpleNamespace(get_snapshot=lambda: snapshot)
    api.full_config = SimpleNamespace(storage=SimpleNamespace(base_path=tmp_path))
    rows = parse_generation_list([
        {"name": "mokuro", "engine": "mokuro", "primary": True},
        {"name": "hayai", "engine": "hayai-nova"},
    ])

    counts = api._generation_skipped_counts(rows)

    assert sorted(asked) == ["Damaged/V0", "Damaged/V1", "Damaged/V2", "New/V0", "New/V1"]
    assert counts == {rows[0].id: 0, rows[1].id: 2}
