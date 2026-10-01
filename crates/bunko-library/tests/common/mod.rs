//! Test support: a `MetadataStore` over SQLite with 0.5.2's schema — also a
//! reference for the server's implementation over `bunko-db`.

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use bunko_library::pyjson::{self, JsonObject, JsonValue};
use bunko_library::store::{
    CachedEntryRow, CachedEntryWrite, CatalogSeriesRow, MetadataStore, SeriesFactsRow, SqlNumber,
    StoreResult,
};
use parking_lot::Mutex;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension, params};

pub fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

pub fn load_json(name: &str) -> JsonValue {
    let text = std::fs::read_to_string(golden_dir().join(name)).expect("golden file");
    pyjson::parse(&text).expect("golden json")
}

pub fn obj(value: &JsonValue) -> &JsonObject {
    value.as_object().expect("object")
}

/// Copy the fixture library to a fresh temporary directory and apply the
/// recorded mtimes (git does not keep them).
pub fn fixture_library() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("library");
    copy_tree(&golden_dir().join("library"), &root);
    let mtimes = load_json("mtimes.json");
    for (rel, value) in obj(&mtimes).iter() {
        let pair = value.as_array().expect("pair");
        let seconds = pair[0].as_num().unwrap().as_f64() as u64;
        let nanos = pair[1].as_num().unwrap().as_f64() as u32;
        let file = std::fs::File::options()
            .write(true)
            .open(root.join(rel))
            .expect("fixture file");
        file.set_modified(UNIX_EPOCH + Duration::new(seconds, nanos))
            .expect("set mtime");
    }
    (temp, root)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// SQLite-backed store with 0.5.2's table definitions.
pub struct SqliteStore {
    pub conn: Mutex<Connection>,
    pub cache_writes: AtomicUsize,
}

impl SqliteStore {
    pub fn new(schema: &[String]) -> Self {
        let conn = Connection::open_in_memory().unwrap();
        for statement in schema {
            conn.execute_batch(statement).unwrap();
        }
        Self {
            conn: Mutex::new(conn),
            cache_writes: AtomicUsize::new(0),
        }
    }
}

fn number(value: ValueRef<'_>) -> Option<SqlNumber> {
    match value {
        ValueRef::Integer(v) => Some(SqlNumber::Integer(v)),
        ValueRef::Real(v) => Some(SqlNumber::Real(v)),
        _ => None,
    }
}

impl MetadataStore for SqliteStore {
    fn series_facts(&self, series_key: &str) -> StoreResult<Option<SeriesFactsRow>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT series_key, series_title, external_ids, titles, synonyms, tag, unit, facts_updated_at, \
                 spine_offset, volume_offsets, updated_by FROM series_facts WHERE series_key = ?",
                [series_key],
                |row| {
                    Ok(SeriesFactsRow::from_columns(
                        row.get(0)?,
                        row.get(1)?,
                        row.get::<_, Option<String>>(2)?.as_deref(),
                        row.get::<_, Option<String>>(3)?.as_deref(),
                        row.get::<_, Option<String>>(4)?.as_deref(),
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        number(row.get_ref(8)?),
                        row.get::<_, Option<String>>(9)?.as_deref(),
                        row.get(10)?,
                    ))
                },
            )
            .optional()?;
        Ok(row)
    }

    fn put_series_facts(&self, row: &SeriesFactsRow) -> StoreResult<()> {
        let columns = row.columns();
        let spine: rusqlite::types::Value = match columns.spine_offset {
            Some(SqlNumber::Integer(v)) => v.into(),
            Some(SqlNumber::Real(v)) => v.into(),
            None => rusqlite::types::Value::Null,
        };
        self.conn.lock().execute(
            "INSERT INTO series_facts (series_key, series_title, external_ids, titles, synonyms, tag, unit, \
             facts_updated_at, spine_offset, volume_offsets, updated_by, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now')) \
             ON CONFLICT(series_key) DO UPDATE SET series_title = excluded.series_title, \
             external_ids = excluded.external_ids, titles = excluded.titles, synonyms = excluded.synonyms, \
             tag = excluded.tag, unit = excluded.unit, facts_updated_at = excluded.facts_updated_at, \
             spine_offset = excluded.spine_offset, volume_offsets = excluded.volume_offsets, \
             updated_by = excluded.updated_by, updated_at = datetime('now')",
            params![
                row.series_key,
                row.series_title,
                columns.external_ids,
                columns.titles,
                columns.synonyms,
                row.tag,
                row.unit,
                row.facts_updated_at,
                spine,
                columns.volume_offsets,
                row.updated_by
            ],
        )?;
        Ok(())
    }

    fn cached_volume_entry(&self, volume_key: &str) -> StoreResult<Option<CachedEntryRow>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT entry_json, cbz_size, cbz_mtime, sidecar_key FROM series_entry_cache WHERE volume_key = ?",
                [volume_key],
                |row| {
                    Ok(CachedEntryRow {
                        entry_json: row.get(0)?,
                        cbz_size: row.get(1)?,
                        cbz_mtime: row.get(2)?,
                        sidecar_key: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    fn put_cached_volume_entry(&self, write: &CachedEntryWrite<'_>) -> StoreResult<()> {
        self.cache_writes.fetch_add(1, Ordering::SeqCst);
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        if let Some(uuid) = write.identity {
            tx.execute(
                "INSERT INTO volume_identities (volume_key, volume_uuid, recorded_at) VALUES (?, ?, datetime('now')) \
                 ON CONFLICT(volume_key) DO UPDATE SET volume_uuid = excluded.volume_uuid, \
                 recorded_at = excluded.recorded_at WHERE volume_uuid != excluded.volume_uuid",
                params![write.volume_key, uuid],
            )?;
        }
        tx.execute(
            "INSERT INTO series_entry_cache (volume_key, series_key, entry_json, cbz_size, cbz_mtime, sidecar_key, \
             computed_at) VALUES (?, ?, ?, ?, ?, ?, datetime('now')) ON CONFLICT(volume_key) DO UPDATE SET \
             series_key = excluded.series_key, entry_json = excluded.entry_json, cbz_size = excluded.cbz_size, \
             cbz_mtime = excluded.cbz_mtime, sidecar_key = excluded.sidecar_key, computed_at = datetime('now')",
            params![write.volume_key, write.series_key, write.entry_json, write.cbz_size, write.cbz_mtime, write.sidecar_key],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn prune_series_entry_cache(&self, keep: &HashSet<String>) -> StoreResult<usize> {
        prune(&self.conn.lock(), "series_entry_cache", "volume_key", keep)
    }

    fn upsert_catalog_series(&self, row: &CatalogSeriesRow) -> StoreResult<()> {
        self.conn.lock().execute(
            "INSERT INTO catalog_series (series_key, folder_name, cover_path, volume_count, latest_volume_modified, \
             total_pages, total_chars, missing_pages, damaged_volumes, scanned_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now')) ON CONFLICT(series_key) DO UPDATE SET \
             folder_name = excluded.folder_name, cover_path = excluded.cover_path, volume_count = excluded.volume_count, \
             latest_volume_modified = excluded.latest_volume_modified, total_pages = excluded.total_pages, \
             total_chars = excluded.total_chars, missing_pages = excluded.missing_pages, \
             damaged_volumes = excluded.damaged_volumes, scanned_at = excluded.scanned_at",
            params![
                row.series_key,
                row.folder_name,
                row.cover_path,
                row.volume_count,
                row.latest_volume_modified,
                row.total_pages,
                row.total_chars,
                row.missing_pages,
                row.damaged_volumes
            ],
        )?;
        Ok(())
    }

    fn prune_catalog_series(&self, keep: &HashSet<String>) -> StoreResult<usize> {
        prune(&self.conn.lock(), "catalog_series", "series_key", keep)
    }
}

fn prune(conn: &Connection, table: &str, key: &str, keep: &HashSet<String>) -> StoreResult<usize> {
    let mut statement = conn.prepare(&format!("SELECT {key} FROM {table}"))?;
    let keys: Vec<String> = statement
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut removed = 0;
    for value in keys.iter().filter(|value| !keep.contains(*value)) {
        removed += conn.execute(&format!("DELETE FROM {table} WHERE {key} = ?"), [value])?;
    }
    Ok(removed)
}

/// One table as `[[{"type", "value"}...]...]`, the shape `gen_library.py` dumps.
pub fn dump_table(
    store: &SqliteStore,
    table: &str,
    key: &str,
    columns: &[&str],
) -> Vec<Vec<(String, String)>> {
    let conn = store.conn.lock();
    let mut statement = conn
        .prepare(&format!(
            "SELECT {} FROM {table} ORDER BY {key}",
            columns.join(", ")
        ))
        .unwrap();
    statement
        .query_map([], |row| {
            Ok((0..columns.len())
                .map(|i| match row.get_ref(i).unwrap() {
                    ValueRef::Null => ("NoneType".to_owned(), "null".to_owned()),
                    ValueRef::Integer(v) => ("int".to_owned(), v.to_string()),
                    ValueRef::Real(v) => ("float".to_owned(), pyjson::float_repr(v)),
                    ValueRef::Text(t) => {
                        ("str".to_owned(), String::from_utf8_lossy(t).into_owned())
                    }
                    ValueRef::Blob(_) => ("bytes".to_owned(), String::new()),
                })
                .collect())
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// The `[{"type","value"}]` rows of `db.json` in the same shape.
pub fn expected_table(db: &JsonValue, table: &str) -> Vec<Vec<(String, String)>> {
    obj(db)
        .get(table)
        .and_then(JsonValue::as_array)
        .unwrap()
        .iter()
        .map(|row| {
            row.as_array()
                .unwrap()
                .iter()
                .map(|cell| {
                    let cell = obj(cell);
                    let kind = cell
                        .get("type")
                        .and_then(JsonValue::as_str)
                        .unwrap()
                        .to_owned();
                    let value = match cell.get("value").unwrap() {
                        JsonValue::Str(text) => text.clone(),
                        JsonValue::Null => "null".to_owned(),
                        other => pyjson::dumps(other, pyjson::DumpOptions::COMPACT_UTF8).unwrap(),
                    };
                    (kind, value)
                })
                .collect()
        })
        .collect()
}
