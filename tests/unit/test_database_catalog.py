"""The materialized catalog table: upsert, list, prune."""

from __future__ import annotations

from pathlib import Path

import pytest

from mokuro_bunko.database import CatalogSeriesRow, Database


@pytest.fixture
def db(tmp_path: Path) -> Database:
    return Database(tmp_path / "test.db")


def row(**overrides: object) -> CatalogSeriesRow:
    base: CatalogSeriesRow = {
        "series_key": "dr stone",
        "folder_name": "Dr Stone",
        "cover_path": "Dr Stone/v01.webp",
        "volume_count": 3,
        "latest_volume_modified": 1756400000.5,
        "total_pages": 570,
        "total_chars": 42000,
    }
    base.update(overrides)  # type: ignore[typeddict-item]
    return base


class TestCatalogSeries:
    def test_round_trips_every_field(self, db: Database) -> None:
        db.upsert_catalog_series(row())
        rows = db.list_catalog_series()
        assert len(rows) == 1
        stored = rows[0]
        assert stored["series_key"] == "dr stone"
        assert stored["folder_name"] == "Dr Stone"
        assert stored["cover_path"] == "Dr Stone/v01.webp"
        assert stored["volume_count"] == 3
        assert stored["latest_volume_modified"] == pytest.approx(1756400000.5)
        assert stored["total_pages"] == 570
        assert stored["total_chars"] == 42000

    def test_upsert_replaces_by_series_key(self, db: Database) -> None:
        db.upsert_catalog_series(row())
        db.upsert_catalog_series(row(volume_count=4, cover_path=None))
        rows = db.list_catalog_series()
        assert len(rows) == 1
        assert rows[0]["volume_count"] == 4
        assert rows[0]["cover_path"] is None

    def test_list_orders_by_folder_name(self, db: Database) -> None:
        db.upsert_catalog_series(row(series_key="b", folder_name="Beta"))
        db.upsert_catalog_series(row(series_key="a", folder_name="Alpha"))
        assert [r["folder_name"] for r in db.list_catalog_series()] == ["Alpha", "Beta"]

    def test_prune_drops_everything_not_kept(self, db: Database) -> None:
        db.upsert_catalog_series(row(series_key="a", folder_name="Alpha"))
        db.upsert_catalog_series(row(series_key="b", folder_name="Beta"))
        removed = db.prune_catalog_series({"a"})
        assert removed == 1
        assert [r["series_key"] for r in db.list_catalog_series()] == ["a"]
