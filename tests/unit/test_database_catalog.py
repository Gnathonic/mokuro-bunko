"""The materialized catalog table: upsert, list, prune."""

from __future__ import annotations

import sqlite3
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
        "missing_pages": 7,
        "damaged_volumes": 1,
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
        assert stored["missing_pages"] == 7
        assert stored["damaged_volumes"] == 1

    def test_the_0_5_2_series_keyed_table_is_left_for_a_rollback(
        self, tmp_path: Path
    ) -> None:
        """0.5.2 materialized into `catalog_series`, keyed by the folded
        series key. This build neither reads nor writes it: the startup pass
        rebuilds every row anyway (the listing falls back to the scanning
        index until then), and leaving the table exactly as it was means a
        rollback to 0.5.2 finds what its own `ON CONFLICT(series_key)` upsert
        needs."""
        path = tmp_path / "legacy.db"
        with sqlite3.connect(path) as conn:
            conn.execute(
                """
                CREATE TABLE catalog_series (
                    series_key TEXT PRIMARY KEY,
                    folder_name TEXT NOT NULL,
                    cover_path TEXT,
                    volume_count INTEGER NOT NULL,
                    latest_volume_modified REAL NOT NULL DEFAULT 0,
                    total_pages INTEGER NOT NULL DEFAULT 0,
                    total_chars INTEGER NOT NULL DEFAULT 0,
                    missing_pages INTEGER NOT NULL DEFAULT 0,
                    damaged_volumes INTEGER NOT NULL DEFAULT 0,
                    scanned_at TEXT NOT NULL DEFAULT (datetime('now'))
                )
                """
            )
            conn.execute(
                "INSERT INTO catalog_series (series_key, folder_name, volume_count) "
                "VALUES ('dr stone', 'Dr Stone', 3)"
            )

        upgraded = Database(path)
        assert upgraded.list_catalog_series() == []
        upgraded.upsert_catalog_series(row(volume_count=5))

        with sqlite3.connect(path) as conn:
            # 0.5.2's own upsert, verbatim in shape, still works on its table.
            conn.execute(
                "INSERT INTO catalog_series (series_key, folder_name, volume_count) "
                "VALUES ('dr stone', 'Dr Stone', 4) "
                "ON CONFLICT(series_key) DO UPDATE SET volume_count = excluded.volume_count"
            )
            legacy_rows = conn.execute(
                "SELECT folder_name, volume_count FROM catalog_series"
            ).fetchall()
        assert legacy_rows == [("Dr Stone", 4)]
        assert [r["volume_count"] for r in upgraded.list_catalog_series()] == [5]

    def test_upsert_replaces_by_folder_name(self, db: Database) -> None:
        db.upsert_catalog_series(row())
        db.upsert_catalog_series(row(volume_count=4, cover_path=None))
        rows = db.list_catalog_series()
        assert len(rows) == 1
        assert rows[0]["volume_count"] == 4
        assert rows[0]["cover_path"] is None

    def test_case_variant_folders_sharing_a_series_key_keep_separate_rows(
        self, db: Database
    ) -> None:
        """Prod 2026-10-06: `Kingdom/` (79 volumes) and a stray `kingdom/`
        (volume 80) shared the key `kingdom`; one row meant the catalog
        showed only volume 80."""
        db.upsert_catalog_series(row(series_key="kingdom", folder_name="Kingdom", volume_count=79))
        db.upsert_catalog_series(row(series_key="kingdom", folder_name="kingdom", volume_count=1))
        assert [(r["folder_name"], r["volume_count"]) for r in db.list_catalog_series()] == [
            ("Kingdom", 79),
            ("kingdom", 1),
        ]

    def test_prune_keeps_by_folder_name_not_series_key(self, db: Database) -> None:
        db.upsert_catalog_series(row(series_key="kingdom", folder_name="Kingdom"))
        db.upsert_catalog_series(row(series_key="kingdom", folder_name="kingdom"))
        assert db.prune_catalog_series({"Kingdom"}) == 1
        assert [r["folder_name"] for r in db.list_catalog_series()] == ["Kingdom"]

    def test_list_orders_by_folder_name(self, db: Database) -> None:
        db.upsert_catalog_series(row(series_key="b", folder_name="Beta"))
        db.upsert_catalog_series(row(series_key="a", folder_name="Alpha"))
        assert [r["folder_name"] for r in db.list_catalog_series()] == ["Alpha", "Beta"]

    def test_prune_drops_everything_not_kept(self, db: Database) -> None:
        db.upsert_catalog_series(row(series_key="a", folder_name="Alpha"))
        db.upsert_catalog_series(row(series_key="b", folder_name="Beta"))
        removed = db.prune_catalog_series({"Alpha"})
        assert removed == 1
        assert [r["series_key"] for r in db.list_catalog_series()] == ["a"]


class TestCommunityDetails:
    def test_round_trips_and_lists(self, db: Database) -> None:
        db.upsert_community_details(
            {
                "series_key": "dr stone",
                "score": 82.0,
                "tags": ["Survival", "Science"],
                "genres": ["Adventure", "Sci-Fi"],
                "source": "anilist",
                "fetched_at": "2026-08-28T00:00:00Z",
            }
        )
        rows = db.list_community_details()
        assert len(rows) == 1
        stored = rows[0]
        assert stored["series_key"] == "dr stone"
        assert stored["score"] == 82.0
        assert stored["tags"] == ["Survival", "Science"]
        assert stored["genres"] == ["Adventure", "Sci-Fi"]
        assert stored["source"] == "anilist"
        assert stored["fetched_at"] == "2026-08-28T00:00:00Z"

    def test_upsert_replaces_by_series_key(self, db: Database) -> None:
        base = {
            "series_key": "dr stone",
            "score": 82.0,
            "tags": [],
            "genres": [],
            "source": "anilist",
            "fetched_at": "2026-08-28T00:00:00Z",
        }
        db.upsert_community_details(base)  # type: ignore[arg-type]
        db.upsert_community_details({**base, "score": None, "source": "mal"})  # type: ignore[arg-type]
        rows = db.list_community_details()
        assert len(rows) == 1
        assert rows[0]["score"] is None
        assert rows[0]["source"] == "mal"
