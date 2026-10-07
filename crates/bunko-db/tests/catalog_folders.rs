//! The materialized catalog table (0.5.3 `tests/unit/test_database_catalog.py`,
//! `TestCatalogSeries`): one row per series FOLDER in `catalog_folders`; the 0.5.2
//! series-keyed `catalog_series` is left as it was, for a rollback.

use bunko_db::{CatalogSeries, Database, DbOptions};
use std::collections::HashSet;
use std::path::Path;

fn open(path: &Path) -> Database {
    Database::open_with(
        path,
        &DbOptions {
            bcrypt_cost: 4,
            ..DbOptions::default()
        },
    )
    .unwrap()
}

fn db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("test.db"));
    (dir, db)
}

/// The Python test's `row(**overrides)`.
fn row() -> CatalogSeries {
    CatalogSeries {
        series_key: "dr stone".into(),
        folder_name: "Dr Stone".into(),
        cover_path: Some("Dr Stone/v01.webp".into()),
        volume_count: 3,
        latest_volume_modified: 1_756_400_000.5,
        total_pages: 570,
        total_chars: 42000,
        missing_pages: 7,
        damaged_volumes: 1,
    }
}

fn keyed(series_key: &str, folder_name: &str) -> CatalogSeries {
    CatalogSeries {
        series_key: series_key.into(),
        folder_name: folder_name.into(),
        ..row()
    }
}

fn keep(names: &[&str]) -> HashSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn round_trips_every_field() {
    let (_dir, db) = db();
    db.upsert_catalog_series(&row()).unwrap();
    assert_eq!(db.list_catalog_series().unwrap(), vec![row()]);
}

/// 0.5.2 materialized into `catalog_series`, keyed by the folded series key. This build
/// neither reads nor writes it: the startup pass rebuilds every row anyway (the listing
/// falls back to the scanning index until then), and leaving the table exactly as it was
/// means a rollback to 0.5.2 finds what its own `ON CONFLICT(series_key)` upsert needs.
#[test]
fn the_0_5_2_series_keyed_table_is_left_for_a_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE catalog_series (
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
            );
            INSERT INTO catalog_series (series_key, folder_name, volume_count)
            VALUES ('dr stone', 'Dr Stone', 3);",
        )
        .unwrap();
    }

    let upgraded = open(&path);
    assert_eq!(upgraded.list_catalog_series().unwrap(), vec![]);
    upgraded
        .upsert_catalog_series(&CatalogSeries {
            volume_count: 5,
            ..row()
        })
        .unwrap();

    let legacy_rows: Vec<(String, i64)> = {
        let conn = rusqlite::Connection::open(&path).unwrap();
        // 0.5.2's own upsert, verbatim in shape, still works on its table.
        conn.execute(
            "INSERT INTO catalog_series (series_key, folder_name, volume_count) \
             VALUES ('dr stone', 'Dr Stone', 4) \
             ON CONFLICT(series_key) DO UPDATE SET volume_count = excluded.volume_count",
            [],
        )
        .unwrap();
        conn.prepare("SELECT folder_name, volume_count FROM catalog_series")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(legacy_rows, vec![("Dr Stone".to_string(), 4)]);
    let counts: Vec<i64> = upgraded
        .list_catalog_series()
        .unwrap()
        .iter()
        .map(|r| r.volume_count)
        .collect();
    assert_eq!(counts, [5]);
}

/// A fresh 0.5.3 database has no `catalog_series` at all (0.5.2 creates it on a rollback).
#[test]
fn a_fresh_database_has_only_the_folder_keyed_table() {
    let (dir, db) = db();
    drop(db);
    let conn = rusqlite::Connection::open(dir.path().join("test.db")).unwrap();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'catalog_%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(tables, ["catalog_folders"]);
}

#[test]
fn upsert_replaces_by_folder_name() {
    let (_dir, db) = db();
    db.upsert_catalog_series(&row()).unwrap();
    db.upsert_catalog_series(&CatalogSeries {
        volume_count: 4,
        cover_path: None,
        ..row()
    })
    .unwrap();
    let rows = db.list_catalog_series().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].volume_count, 4);
    assert_eq!(rows[0].cover_path, None);
}

/// Prod 2026-10-06: `Kingdom/` (79 volumes) and a stray `kingdom/` (volume 80) shared the
/// key `kingdom`; one row meant the catalog showed only volume 80.
#[test]
fn case_variant_folders_sharing_a_series_key_keep_separate_rows() {
    let (_dir, db) = db();
    db.upsert_catalog_series(&CatalogSeries {
        volume_count: 79,
        ..keyed("kingdom", "Kingdom")
    })
    .unwrap();
    db.upsert_catalog_series(&CatalogSeries {
        volume_count: 1,
        ..keyed("kingdom", "kingdom")
    })
    .unwrap();
    let rows: Vec<(String, i64)> = db
        .list_catalog_series()
        .unwrap()
        .into_iter()
        .map(|r| (r.folder_name, r.volume_count))
        .collect();
    assert_eq!(
        rows,
        [("Kingdom".to_string(), 79), ("kingdom".to_string(), 1)]
    );
}

#[test]
fn prune_keeps_by_folder_name_not_series_key() {
    let (_dir, db) = db();
    db.upsert_catalog_series(&keyed("kingdom", "Kingdom"))
        .unwrap();
    db.upsert_catalog_series(&keyed("kingdom", "kingdom"))
        .unwrap();
    assert_eq!(db.prune_catalog_series(&keep(&["Kingdom"])).unwrap(), 1);
    let names: Vec<String> = db
        .list_catalog_series()
        .unwrap()
        .into_iter()
        .map(|r| r.folder_name)
        .collect();
    assert_eq!(names, ["Kingdom"]);
}

#[test]
fn list_orders_by_folder_name() {
    let (_dir, db) = db();
    db.upsert_catalog_series(&keyed("b", "Beta")).unwrap();
    db.upsert_catalog_series(&keyed("a", "Alpha")).unwrap();
    let names: Vec<String> = db
        .list_catalog_series()
        .unwrap()
        .into_iter()
        .map(|r| r.folder_name)
        .collect();
    assert_eq!(names, ["Alpha", "Beta"]);
}

#[test]
fn prune_drops_everything_not_kept() {
    let (_dir, db) = db();
    db.upsert_catalog_series(&keyed("a", "Alpha")).unwrap();
    db.upsert_catalog_series(&keyed("b", "Beta")).unwrap();
    assert_eq!(db.prune_catalog_series(&keep(&["Alpha"])).unwrap(), 1);
    let keys: Vec<String> = db
        .list_catalog_series()
        .unwrap()
        .into_iter()
        .map(|r| r.series_key)
        .collect();
    assert_eq!(keys, ["a"]);
}
