//! The database access the metadata compiler needs, as a trait the server
//! implements on top of `bunko-db` (this crate must not depend on it).
//!
//! Semantics follow 0.5.3's `database.py` (spec `db-auth-admin.md` §10.4, §11)
//! for the tables `series_facts`, `series_entry_cache`, `catalog_folders` and
//! `volume_identities`. Everything that decides *meaning* (cache validity,
//! JSON shapes, identity rule) is done in this crate; implementations only
//! move rows. Helpers here produce/consume the exact column texts Python
//! wrote, so a database shared with 0.5.2 round-trips.

use std::collections::HashSet;

use crate::pyjson::{self, DumpOptions, JsonNum, JsonObject, JsonValue};

/// Any storage failure (the implementation's own error, boxed).
pub type StoreError = Box<dyn std::error::Error + Send + Sync + 'static>;
pub type StoreResult<T> = Result<T, StoreError>;

/// A value of a `NUMERIC` column (`series_facts.spine_offset`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SqlNumber {
    Integer(i64),
    Real(f64),
}

impl SqlNumber {
    pub fn to_json(self) -> JsonNum {
        match self {
            SqlNumber::Integer(value) => JsonNum::Int(value),
            SqlNumber::Real(value) => JsonNum::Float(value),
        }
    }
}

/// `_bindable_offset`: what may be bound to `spine_offset` — ints within i64,
/// finite floats; anything else is stored as NULL.
pub fn bindable_offset(value: Option<&JsonNum>) -> Option<SqlNumber> {
    match value? {
        JsonNum::Int(value) => Some(SqlNumber::Integer(*value)),
        JsonNum::BigInt(_) => None,
        JsonNum::Float(value) => value.is_finite().then_some(SqlNumber::Real(*value)),
    }
}

/// One `series_facts` row (minus the bookkeeping `updated_at`, which the
/// database stamps with `datetime('now')` on every write).
#[derive(Debug, Clone)]
pub struct SeriesFactsRow {
    /// `normalize_volume_title_key(folder title)`.
    pub series_key: String,
    /// The FOLDER's spelling.
    pub series_title: String,
    pub external_ids: JsonObject,
    pub titles: JsonObject,
    pub synonyms: Vec<JsonValue>,
    pub tag: Option<String>,
    pub unit: Option<String>,
    pub facts_updated_at: String,
    pub spine_offset: Option<JsonNum>,
    pub volume_offsets: JsonObject,
    pub updated_by: Option<String>,
}

/// The JSON/text columns of a [`SeriesFactsRow`] exactly as 0.5.2 writes them
/// (`json.dumps(x, ensure_ascii=False)`).
#[derive(Debug, Clone)]
pub struct SeriesFactsColumns {
    pub external_ids: String,
    pub titles: String,
    pub synonyms: String,
    pub volume_offsets: String,
    pub spine_offset: Option<SqlNumber>,
}

fn dumps_column(value: &JsonValue) -> String {
    // Column values are built from validated data; a non-finite float cannot
    // reach here (`allow_nan` is on for this format anyway, as in Python).
    pyjson::dumps(value, DumpOptions::DEFAULT_UTF8).unwrap_or_else(|_| "null".to_owned())
}

impl SeriesFactsRow {
    /// Build a row from the raw columns (`_series_facts_from_row`; JSON
    /// columns that are corrupt fall back to empty, never fail).
    #[allow(clippy::too_many_arguments)]
    pub fn from_columns(
        series_key: String,
        series_title: String,
        external_ids: Option<&str>,
        titles: Option<&str>,
        synonyms: Option<&str>,
        tag: Option<String>,
        unit: Option<String>,
        facts_updated_at: String,
        spine_offset: Option<SqlNumber>,
        volume_offsets: Option<&str>,
        updated_by: Option<String>,
    ) -> Self {
        Self {
            series_key,
            series_title,
            external_ids: pyjson::load_json_object(external_ids),
            titles: pyjson::load_json_object(titles),
            synonyms: pyjson::load_json_array(synonyms),
            tag,
            unit,
            facts_updated_at,
            spine_offset: spine_offset.map(SqlNumber::to_json),
            volume_offsets: pyjson::load_json_object(volume_offsets),
            updated_by,
        }
    }

    /// The column values to bind on `put_series_facts`.
    pub fn columns(&self) -> SeriesFactsColumns {
        SeriesFactsColumns {
            external_ids: dumps_column(&JsonValue::Object(self.external_ids.clone())),
            titles: dumps_column(&JsonValue::Object(self.titles.clone())),
            synonyms: dumps_column(&JsonValue::Array(self.synonyms.clone())),
            volume_offsets: dumps_column(&JsonValue::Object(self.volume_offsets.clone())),
            spine_offset: bindable_offset(self.spine_offset.as_ref()),
        }
    }
}

/// A `series_entry_cache` row as read (`SELECT entry_json, cbz_size,
/// cbz_mtime, sidecar_key ... WHERE volume_key = ?`). Validity is decided by
/// this crate.
#[derive(Debug, Clone)]
pub struct CachedEntryRow {
    pub entry_json: String,
    pub cbz_size: i64,
    pub cbz_mtime: f64,
    pub sidecar_key: String,
}

/// A `series_entry_cache` write.
#[derive(Debug, Clone)]
pub struct CachedEntryWrite<'a> {
    pub volume_key: &'a str,
    pub series_key: &'a str,
    /// `json.dumps(entry, ensure_ascii=False)` of the compiled entry.
    pub entry_json: &'a str,
    pub cbz_size: i64,
    pub cbz_mtime: f64,
    pub sidecar_key: &'a str,
    /// `_identity_from_entry(entry)`: when `Some`, the implementation upserts
    /// `volume_identities(volume_key, volume_uuid)` IN THE SAME TRANSACTION
    /// (`ON CONFLICT(volume_key) DO UPDATE ... WHERE volume_uuid !=
    /// excluded.volume_uuid`).
    pub identity: Option<&'a str>,
}

/// A `catalog_folders` row (`scanned_at` is stamped by the database): one per series
/// folder, so case-variant folders sharing a `series_key` keep separate rows (0.5.3; 0.5.2
/// keyed `catalog_series` by `series_key`).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogSeriesRow {
    pub series_key: String,
    pub folder_name: String,
    pub cover_path: Option<String>,
    pub volume_count: i64,
    pub latest_volume_modified: f64,
    pub total_pages: i64,
    pub total_chars: i64,
    pub missing_pages: i64,
    pub damaged_volumes: i64,
}

/// Database operations of the metadata subsystem. Implemented by the server
/// over `bunko-db`; every method is blocking (call from `spawn_blocking`).
pub trait MetadataStore: Send + Sync {
    /// `SELECT * FROM series_facts WHERE series_key = ?`.
    fn series_facts(&self, series_key: &str) -> StoreResult<Option<SeriesFactsRow>>;

    /// Upsert a `series_facts` row: `INSERT ... ON CONFLICT(series_key) DO
    /// UPDATE SET <every column> = excluded.*, updated_at = datetime('now')`.
    /// Bind the texts from [`SeriesFactsRow::columns`].
    fn put_series_facts(&self, row: &SeriesFactsRow) -> StoreResult<()>;

    /// The `series_entry_cache` row for `volume_key`, if any.
    fn cached_volume_entry(&self, volume_key: &str) -> StoreResult<Option<CachedEntryRow>>;

    /// Upsert a `series_entry_cache` row (`computed_at = datetime('now')`),
    /// plus the `volume_identities` upsert when `write.identity` is set, in
    /// one transaction.
    fn put_cached_volume_entry(&self, write: &CachedEntryWrite<'_>) -> StoreResult<()>;

    /// Delete every `series_entry_cache` row whose key is not in `keep`;
    /// returns the number deleted.
    fn prune_series_entry_cache(&self, keep: &HashSet<String>) -> StoreResult<usize>;

    /// Upsert a `catalog_folders` row by `folder_name` (`scanned_at = datetime('now')`).
    fn upsert_catalog_series(&self, row: &CatalogSeriesRow) -> StoreResult<()>;

    /// Delete every `catalog_folders` row whose `folder_name` is not in `keep`.
    fn prune_catalog_series(&self, keep: &HashSet<String>) -> StoreResult<usize>;
}

impl<T: MetadataStore + ?Sized> MetadataStore for std::sync::Arc<T> {
    fn series_facts(&self, series_key: &str) -> StoreResult<Option<SeriesFactsRow>> {
        (**self).series_facts(series_key)
    }
    fn put_series_facts(&self, row: &SeriesFactsRow) -> StoreResult<()> {
        (**self).put_series_facts(row)
    }
    fn cached_volume_entry(&self, volume_key: &str) -> StoreResult<Option<CachedEntryRow>> {
        (**self).cached_volume_entry(volume_key)
    }
    fn put_cached_volume_entry(&self, write: &CachedEntryWrite<'_>) -> StoreResult<()> {
        (**self).put_cached_volume_entry(write)
    }
    fn prune_series_entry_cache(&self, keep: &HashSet<String>) -> StoreResult<usize> {
        (**self).prune_series_entry_cache(keep)
    }
    fn upsert_catalog_series(&self, row: &CatalogSeriesRow) -> StoreResult<()> {
        (**self).upsert_catalog_series(row)
    }
    fn prune_catalog_series(&self, keep: &HashSet<String>) -> StoreResult<usize> {
        (**self).prune_catalog_series(keep)
    }
}

/// `_identity_from_entry`: the id a compiled entry published FROM ITS
/// `.mokuro` (a parsed sidecar), else `None`. Image-only ids are derivable and
/// never remembered. Exposed for implementations that backfill
/// `volume_identities` from existing cache rows.
pub fn identity_from_entry(entry: &JsonObject) -> Option<&str> {
    let uuid = entry.get("volume_uuid")?.as_str()?;
    if crate::pyunicode::strip(uuid).is_empty() {
        return None;
    }
    let hashed = entry.get("mokuro_sha256").is_some_and(JsonValue::is_truthy);
    let legacy = entry
        .get("mokuro_size")
        .is_some_and(|size| !matches!(size, JsonValue::Null))
        && entry
            .get("mokuro_version")
            .is_some_and(JsonValue::is_truthy);
    (hashed || legacy).then_some(uuid)
}
