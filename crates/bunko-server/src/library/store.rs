//! [`DbMetadataStore`]: the metadata compiler's [`MetadataStore`] on `bunko_db::Database`.
//!
//! `bunko-library` speaks its own Python-faithful JSON model ([`pyjson`]); `bunko-db`
//! speaks `serde_json`. The conversions here are value-preserving for everything 0.5.2
//! can store (ints stay ints, floats stay floats, object order is kept), so a row
//! written through this store is byte-identical to the one 0.5.2 writes (pinned by the
//! `library_store` tests). One loss: an integer beyond `u64` (only reachable through
//! a client-sent `volume_offsets` value) becomes a float, as `serde_json` without
//! `arbitrary_precision` cannot hold it.

use std::collections::HashSet;
use std::sync::Arc;

use bunko_db::Database;
use bunko_library::pyjson::{JsonNum, JsonObject, JsonValue};
use bunko_library::store::{
    CachedEntryRow, CachedEntryWrite, CatalogSeriesRow, MetadataStore, SeriesFactsRow, SqlNumber,
    StoreResult,
};
use rusqlite::OptionalExtension;
use serde_json::{Map, Number, Value};

/// The metadata store over the server's database.
#[derive(Clone)]
pub struct DbMetadataStore {
    db: Arc<Database>,
}

impl DbMetadataStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }
}

/// `serde_json` → `pyjson`, value-preserving.
pub fn to_py(value: &Value) -> JsonValue {
    match value {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Number(n) => JsonValue::Num(num_to_py(n)),
        Value::String(s) => JsonValue::Str(s.clone()),
        Value::Array(items) => JsonValue::Array(items.iter().map(to_py).collect()),
        Value::Object(map) => JsonValue::Object(map_to_py(map)),
    }
}

fn num_to_py(n: &Number) -> JsonNum {
    if let Some(i) = n.as_i64() {
        JsonNum::Int(i)
    } else if let Some(u) = n.as_u64() {
        JsonNum::BigInt(u.to_string().into_boxed_str())
    } else {
        JsonNum::Float(n.as_f64().unwrap_or(f64::NAN))
    }
}

pub fn map_to_py(map: &Map<String, Value>) -> JsonObject {
    JsonObject(map.iter().map(|(k, v)| (k.clone(), to_py(v))).collect())
}

/// `pyjson` → `serde_json`. Non-finite floats become `null` (they never reach a
/// stored row: the validator refuses them).
pub fn from_py(value: &JsonValue) -> Value {
    match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(b) => Value::Bool(*b),
        JsonValue::Num(n) => py_num_to_serde(n).map_or(Value::Null, Value::Number),
        JsonValue::Str(s) => Value::String(s.clone()),
        JsonValue::Array(items) => Value::Array(items.iter().map(from_py).collect()),
        JsonValue::Object(obj) => Value::Object(object_from_py(obj)),
    }
}

fn py_num_to_serde(n: &JsonNum) -> Option<Number> {
    match n {
        JsonNum::Int(i) => Some(Number::from(*i)),
        JsonNum::BigInt(text) => text
            .parse::<u64>()
            .ok()
            .map(Number::from)
            .or_else(|| text.parse::<f64>().ok().and_then(Number::from_f64)),
        JsonNum::Float(f) => Number::from_f64(*f),
    }
}

pub fn object_from_py(obj: &JsonObject) -> Map<String, Value> {
    obj.0.iter().map(|(k, v)| (k.clone(), from_py(v))).collect()
}

fn sql_number(n: &Number) -> SqlNumber {
    match n.as_i64() {
        Some(i) => SqlNumber::Integer(i),
        None => SqlNumber::Real(n.as_f64().unwrap_or(f64::NAN)),
    }
}

fn json_text(value: Value) -> String {
    // Serialising a `Value` cannot fail (string keys only).
    serde_json::to_string(&value).unwrap_or_default()
}

impl MetadataStore for DbMetadataStore {
    fn series_facts(&self, series_key: &str) -> StoreResult<Option<SeriesFactsRow>> {
        let Some(row) = self.db.get_series_facts(series_key)? else {
            return Ok(None);
        };
        let external_ids = json_text(Value::Object(row.external_ids));
        let titles = json_text(Value::Object(row.titles));
        let synonyms = json_text(Value::Array(row.synonyms));
        let volume_offsets = json_text(Value::Object(row.volume_offsets));
        Ok(Some(SeriesFactsRow::from_columns(
            row.series_key,
            row.series_title,
            Some(&external_ids),
            Some(&titles),
            Some(&synonyms),
            row.tag,
            row.unit,
            row.facts_updated_at,
            row.spine_offset.as_ref().map(sql_number),
            Some(&volume_offsets),
            row.updated_by,
        )))
    }

    fn put_series_facts(&self, row: &SeriesFactsRow) -> StoreResult<()> {
        let spine_offset = match row.spine_offset.as_ref() {
            Some(JsonNum::Int(i)) => Some(Number::from(*i)),
            Some(JsonNum::Float(f)) => Number::from_f64(*f),
            // `_bindable_offset`: an int beyond i64 is stored as NULL.
            Some(JsonNum::BigInt(_)) | None => None,
        };
        let facts = bunko_db::SeriesFacts {
            series_key: row.series_key.clone(),
            series_title: row.series_title.clone(),
            external_ids: object_from_py(&row.external_ids),
            titles: object_from_py(&row.titles),
            synonyms: row.synonyms.iter().map(from_py).collect(),
            tag: row.tag.clone(),
            unit: row.unit.clone(),
            facts_updated_at: row.facts_updated_at.clone(),
            spine_offset,
            volume_offsets: object_from_py(&row.volume_offsets),
            updated_by: row.updated_by.clone(),
            updated_at: String::new(),
        };
        self.db.put_series_facts(&facts)?;
        Ok(())
    }

    fn cached_volume_entry(&self, volume_key: &str) -> StoreResult<Option<CachedEntryRow>> {
        // bunko-db only offers the stat-checked lookup (`get_cached_volume_entry`), but
        // this crate decides validity itself, so it needs the raw row. Read on the
        // writer connection until bunko-db grows a raw accessor (see the module report).
        let row = self.db.with_writer_connection(|conn| {
            conn.prepare_cached(
                "SELECT entry_json, cbz_size, cbz_mtime, sidecar_key FROM series_entry_cache \
                 WHERE volume_key = ?",
            )?
            .query_row([volume_key], |r| {
                Ok(CachedEntryRow {
                    entry_json: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    cbz_size: r.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                    cbz_mtime: r.get::<_, Option<f64>>(2)?.unwrap_or(f64::NAN),
                    sidecar_key: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                })
            })
            .optional()
        })?;
        Ok(row)
    }

    fn put_cached_volume_entry(&self, write: &CachedEntryWrite<'_>) -> StoreResult<()> {
        // bunko-db re-serialises the object in 0.5.2's `json.dumps(ensure_ascii=False)`
        // form and derives the identity with the same rule as `write.identity`.
        let entry: Map<String, Value> = match serde_json::from_str(write.entry_json)? {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        self.db.put_cached_volume_entry(
            write.volume_key,
            write.series_key,
            &entry,
            write.cbz_size,
            write.cbz_mtime,
            write.sidecar_key,
        )?;
        Ok(())
    }

    fn prune_series_entry_cache(&self, keep: &HashSet<String>) -> StoreResult<usize> {
        Ok(self.db.prune_series_entry_cache(keep)?)
    }

    fn upsert_catalog_series(&self, row: &CatalogSeriesRow) -> StoreResult<()> {
        self.db.upsert_catalog_series(&bunko_db::CatalogSeries {
            series_key: row.series_key.clone(),
            folder_name: row.folder_name.clone(),
            cover_path: row.cover_path.clone(),
            volume_count: row.volume_count,
            latest_volume_modified: row.latest_volume_modified,
            total_pages: row.total_pages,
            total_chars: row.total_chars,
            missing_pages: row.missing_pages,
            damaged_volumes: row.damaged_volumes,
        })?;
        Ok(())
    }

    fn prune_catalog_series(&self, keep: &HashSet<String>) -> StoreResult<usize> {
        Ok(self.db.prune_catalog_series(keep)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_round_trip() {
        let v: Value =
            serde_json::from_str(r#"{"a":1,"b":12.0,"c":-40,"d":18446744073709551615,"e":[1.5]}"#)
                .unwrap();
        let py = to_py(&v);
        let back = from_py(&py);
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&v).unwrap()
        );
        assert!(matches!(
            py.as_object().unwrap().get("b"),
            Some(JsonValue::Num(JsonNum::Float(_)))
        ));
    }
}
