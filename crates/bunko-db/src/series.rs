//! Series facts, the compiled-entry cache, the materialized catalog and community details
//! (spec §11) — the DB layer of the metadata/catalog subsystems.
//!
//! Reproduced: JSON columns are written exactly as 0.5.2's `json.dumps` writes them
//! (`ensure_ascii=False` with `", "`/`": "` separators for facts and entries; the default
//! ASCII form for community tags/genres), so rows are byte-identical whichever build
//! wrote them; corrupt JSON in facts/entries degrades to the empty value; `spine_offset`
//! (NUMERIC) keeps an integer an integer and refuses bools, out-of-range integers and
//! non-finite floats (stored NULL); cache hits need exact size, mtime (float equality)
//! and sidecar key; storing an entry compiled from a `.mokuro` remembers its
//! `volume_uuid` in the same transaction; prunes scan then delete in one transaction.
//!
//! Choice: `list_community_details` falls back to `[]` for corrupt `tags`/`genres` JSON
//! (0.5.2 raised).

use crate::database::Database;
use crate::error::Result;
use crate::identities::{identity_from_entry, upsert_volume_identity};
use crate::pyfmt::{self, JsonStyle};
use rusqlite::types::ValueRef;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::collections::HashSet;

/// One series' shareable facts plus its shelf alignment. `series_key` is the
/// caller-folded series identity (`fold_series_title_key`); nothing here re-folds it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SeriesFacts {
    pub series_key: String,
    pub series_title: String,
    pub external_ids: Map<String, Value>,
    pub titles: Map<String, Value>,
    pub synonyms: Vec<Value>,
    pub tag: Option<String>,
    pub unit: Option<String>,
    /// The facts clock that decides merges (stored as given).
    pub facts_updated_at: String,
    /// NUMERIC: an integer stays an integer.
    pub spine_offset: Option<Number>,
    pub volume_offsets: Map<String, Value>,
    pub updated_by: Option<String>,
    /// Row bookkeeping stamp, set by the database on write (ignored on input).
    pub updated_at: String,
}

/// One series' render-ready catalog entry (filesystem-derived; rebuilt every pass).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CatalogSeries {
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

/// Server-fetched community details (AniList/MAL) for one series.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CommunityDetails {
    pub series_key: String,
    pub score: Option<f64>,
    pub tags: Vec<Value>,
    pub genres: Vec<Value>,
    pub source: String,
    pub fetched_at: String,
}

/// Read a column leniently as text (numbers stringified, blobs lossily decoded).
pub(crate) fn opt_text(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<String>> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Null => None,
        ValueRef::Integer(i) => Some(i.to_string()),
        ValueRef::Real(f) => Some(pyfmt::float_repr(f)),
        ValueRef::Text(t) | ValueRef::Blob(t) => Some(String::from_utf8_lossy(t).into_owned()),
    })
}

fn text(row: &Row<'_>, idx: usize) -> rusqlite::Result<String> {
    Ok(opt_text(row, idx)?.unwrap_or_default())
}

/// `_load_json_object(raw, {})`: a JSON object, else (missing, corrupt, other type) `{}`.
pub(crate) fn load_json_object(raw: Option<&str>) -> Map<String, Value> {
    match raw.map(serde_json::from_str::<Value>) {
        Some(Ok(Value::Object(map))) => map,
        _ => Map::new(),
    }
}

/// `_load_json_object(raw, [])`.
pub(crate) fn load_json_array(raw: Option<&str>) -> Vec<Value> {
    match raw.map(serde_json::from_str::<Value>) {
        Some(Ok(Value::Array(items))) => items,
        _ => Vec::new(),
    }
}

/// `_bindable_offset`, applied to what JSON can carry.
fn bindable_offset(n: &Option<Number>) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match n {
        Some(n) if n.is_i64() => n.as_i64().map_or(Sql::Null, Sql::Integer),
        Some(n) if n.is_u64() => Sql::Null, // beyond i64: SQLite cannot hold it
        Some(n) => n
            .as_f64()
            .filter(|f| f.is_finite())
            .map_or(Sql::Null, Sql::Real),
        None => Sql::Null,
    }
}

fn numeric(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<Number>> {
    Ok(match row.get_ref(idx)? {
        ValueRef::Integer(i) => Some(Number::from(i)),
        ValueRef::Real(f) => Number::from_f64(f),
        _ => None,
    })
}

fn facts_from_row(r: &Row<'_>) -> rusqlite::Result<SeriesFacts> {
    Ok(SeriesFacts {
        series_key: text(r, 0)?,
        series_title: text(r, 1)?,
        external_ids: load_json_object(opt_text(r, 2)?.as_deref()),
        titles: load_json_object(opt_text(r, 3)?.as_deref()),
        synonyms: load_json_array(opt_text(r, 4)?.as_deref()),
        tag: opt_text(r, 5)?,
        unit: opt_text(r, 6)?,
        facts_updated_at: text(r, 7)?,
        spine_offset: numeric(r, 8)?,
        volume_offsets: load_json_object(opt_text(r, 9)?.as_deref()),
        updated_by: opt_text(r, 10)?,
        updated_at: text(r, 11)?,
    })
}

const FACTS_COLUMNS: &str = "series_key, series_title, external_ids, titles, synonyms, tag, unit, \
    facts_updated_at, spine_offset, volume_offsets, updated_by, updated_at";

fn unicode_json(v: &Value) -> String {
    pyfmt::dumps(v, JsonStyle::DEFAULT_UNICODE)
}

fn unicode_json_map(m: &Map<String, Value>) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        pyfmt::write_str(&mut out, k, false);
        out.push_str(": ");
        pyfmt::write_value(&mut out, v, JsonStyle::DEFAULT_UNICODE);
    }
    out.push('}');
    out
}

fn ascii_json_list(items: &[Value]) -> String {
    let mut out = String::from("[");
    for (i, v) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        pyfmt::write_value(&mut out, v, JsonStyle::DEFAULT_ASCII);
    }
    out.push(']');
    out
}

impl Database {
    /// Stored facts for one series key.
    pub fn get_series_facts(&self, series_key: &str) -> Result<Option<SeriesFacts>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {FACTS_COLUMNS} FROM series_facts WHERE series_key = ?"
                ))?
                .query_row([series_key], facts_from_row)
                .optional()?)
        })
    }

    /// Every stored series, including ones whose folder is gone.
    pub fn list_series_facts(&self) -> Result<Vec<SeriesFacts>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!("SELECT {FACTS_COLUMNS} FROM series_facts"))?
                .query_map([], facts_from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Insert or replace one series' facts and shelf alignment (`updated_at` = now).
    pub fn put_series_facts(&self, row: &SeriesFacts) -> Result<()> {
        let synonyms = unicode_json(&Value::Array(row.synonyms.clone()));
        self.write(|conn| {
            conn.execute(
                "INSERT INTO series_facts (series_key, series_title, external_ids, titles, \
                 synonyms, tag, unit, facts_updated_at, spine_offset, volume_offsets, \
                 updated_by, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now')) \
                 ON CONFLICT(series_key) DO UPDATE SET \
                 series_title = excluded.series_title, \
                 external_ids = excluded.external_ids, \
                 titles = excluded.titles, \
                 synonyms = excluded.synonyms, \
                 tag = excluded.tag, \
                 unit = excluded.unit, \
                 facts_updated_at = excluded.facts_updated_at, \
                 spine_offset = excluded.spine_offset, \
                 volume_offsets = excluded.volume_offsets, \
                 updated_by = excluded.updated_by, \
                 updated_at = datetime('now')",
                params![
                    row.series_key,
                    row.series_title,
                    unicode_json_map(&row.external_ids),
                    unicode_json_map(&row.titles),
                    synonyms,
                    row.tag,
                    row.unit,
                    row.facts_updated_at,
                    bindable_offset(&row.spine_offset),
                    unicode_json_map(&row.volume_offsets),
                    row.updated_by,
                ],
            )?;
            Ok(())
        })
    }

    /// A previously compiled volume entry, if its sources' stat is unchanged (exact size,
    /// exact float mtime, same sidecar key) and it is a non-empty object.
    pub fn get_cached_volume_entry(
        &self,
        volume_key: &str,
        cbz_size: i64,
        cbz_mtime: f64,
        sidecar_key: &str,
    ) -> Result<Option<Map<String, Value>>> {
        let row: Option<(i64, f64, String, Option<String>)> = self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT cbz_size, cbz_mtime, sidecar_key, entry_json FROM series_entry_cache \
                     WHERE volume_key = ?",
                )?
                .query_row([volume_key], |r| {
                    Ok((r.get(0)?, r.get(1)?, text(r, 2)?, opt_text(r, 3)?))
                })
                .optional()?)
        })?;
        let Some((size, mtime, key, entry)) = row else {
            return Ok(None);
        };
        if size != cbz_size || mtime != cbz_mtime || key != sidecar_key {
            return Ok(None);
        }
        let entry = load_json_object(entry.as_deref());
        Ok((!entry.is_empty()).then_some(entry))
    }

    /// The raw cache row `(entry_json, cbz_size, cbz_mtime, sidecar_key)` for callers that
    /// decide validity themselves (bunko-library compares stats with its own rules).
    /// NULL columns come back as `""`, `-1`, `NaN` and `""`, which never validate.
    pub fn cached_volume_entry_row(&self, volume_key: &str) -> Result<Option<(String, i64, f64, String)>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT entry_json, cbz_size, cbz_mtime, sidecar_key FROM series_entry_cache \
                     WHERE volume_key = ?",
                )?
                .query_row([volume_key], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                        r.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                        r.get::<_, Option<f64>>(2)?.unwrap_or(f64::NAN),
                        r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    ))
                })
                .optional()?)
        })
    }

    /// Remember a compiled volume entry against its sources' stat; an entry compiled from
    /// a `.mokuro` also keeps its `volume_uuid` (same transaction).
    pub fn put_cached_volume_entry(
        &self,
        volume_key: &str,
        series_key: &str,
        entry: &Map<String, Value>,
        cbz_size: i64,
        cbz_mtime: f64,
        sidecar_key: &str,
    ) -> Result<()> {
        let identity = identity_from_entry(entry);
        let entry_json = unicode_json_map(entry);
        self.write(|conn| {
            if let Some(uuid) = &identity {
                upsert_volume_identity(conn, volume_key, uuid)?;
            }
            conn.prepare_cached(
                "INSERT INTO series_entry_cache (volume_key, series_key, entry_json, cbz_size, \
                 cbz_mtime, sidecar_key, computed_at) VALUES (?, ?, ?, ?, ?, ?, datetime('now')) \
                 ON CONFLICT(volume_key) DO UPDATE SET \
                 series_key = excluded.series_key, \
                 entry_json = excluded.entry_json, \
                 cbz_size = excluded.cbz_size, \
                 cbz_mtime = excluded.cbz_mtime, \
                 sidecar_key = excluded.sidecar_key, \
                 computed_at = datetime('now')",
            )?
            .execute(params![
                volume_key,
                series_key,
                entry_json,
                cbz_size,
                cbz_mtime,
                sidecar_key
            ])?;
            Ok(())
        })
    }

    /// Drop cache rows for volumes not in `keep`; how many.
    pub fn prune_series_entry_cache(&self, keep: &HashSet<String>) -> Result<usize> {
        self.prune_keyed("series_entry_cache", "volume_key", keep)
    }

    /// Insert or replace one series' catalog entry (`scanned_at` = now).
    pub fn upsert_catalog_series(&self, row: &CatalogSeries) -> Result<()> {
        self.write(|conn| {
            conn.prepare_cached(
                "INSERT INTO catalog_series (series_key, folder_name, cover_path, volume_count, \
                 latest_volume_modified, total_pages, total_chars, missing_pages, \
                 damaged_volumes, scanned_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, datetime('now')) \
                 ON CONFLICT(series_key) DO UPDATE SET \
                 folder_name = excluded.folder_name, \
                 cover_path = excluded.cover_path, \
                 volume_count = excluded.volume_count, \
                 latest_volume_modified = excluded.latest_volume_modified, \
                 total_pages = excluded.total_pages, \
                 total_chars = excluded.total_chars, \
                 missing_pages = excluded.missing_pages, \
                 damaged_volumes = excluded.damaged_volumes, \
                 scanned_at = excluded.scanned_at",
            )?
            .execute(params![
                row.series_key,
                row.folder_name,
                row.cover_path,
                row.volume_count,
                row.latest_volume_modified,
                row.total_pages,
                row.total_chars,
                row.missing_pages,
                row.damaged_volumes
            ])?;
            Ok(())
        })
    }

    /// Every catalog row, ordered by folder name.
    pub fn list_catalog_series(&self) -> Result<Vec<CatalogSeries>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT series_key, folder_name, cover_path, volume_count, \
                     latest_volume_modified, total_pages, total_chars, missing_pages, \
                     damaged_volumes FROM catalog_series ORDER BY folder_name",
                )?
                .query_map([], |r| {
                    Ok(CatalogSeries {
                        series_key: text(r, 0)?,
                        folder_name: text(r, 1)?,
                        cover_path: opt_text(r, 2)?,
                        volume_count: r.get(3)?,
                        latest_volume_modified: r.get(4)?,
                        total_pages: r.get(5)?,
                        total_chars: r.get(6)?,
                        missing_pages: r.get(7)?,
                        damaged_volumes: r.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Drop catalog rows for series not in `keep`; how many.
    pub fn prune_catalog_series(&self, keep: &HashSet<String>) -> Result<usize> {
        self.prune_keyed("catalog_series", "series_key", keep)
    }

    fn prune_keyed(&self, table: &str, key: &str, keep: &HashSet<String>) -> Result<usize> {
        self.write(|conn| {
            let stale: Vec<String> = conn
                .prepare(&format!("SELECT {key} FROM {table}"))?
                .query_map([], |r| text(r, 0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|k| !keep.contains(k))
                .collect();
            let mut delete = conn.prepare(&format!("DELETE FROM {table} WHERE {key} = ?"))?;
            for k in &stale {
                delete.execute([k])?;
            }
            Ok(stale.len())
        })
    }

    /// Insert or replace one series' community details.
    pub fn upsert_community_details(&self, row: &CommunityDetails) -> Result<()> {
        self.write(|conn| {
            conn.execute(
                "INSERT INTO community_details (series_key, score, tags, genres, source, fetched_at) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(series_key) DO UPDATE SET \
                 score = excluded.score, \
                 tags = excluded.tags, \
                 genres = excluded.genres, \
                 source = excluded.source, \
                 fetched_at = excluded.fetched_at",
                params![
                    row.series_key,
                    row.score,
                    ascii_json_list(&row.tags),
                    ascii_json_list(&row.genres),
                    row.source,
                    row.fetched_at
                ],
            )?;
            Ok(())
        })
    }

    /// Every community row.
    pub fn list_community_details(&self) -> Result<Vec<CommunityDetails>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT series_key, score, tags, genres, source, fetched_at FROM community_details",
                )?
                .query_map([], |r| {
                    Ok(CommunityDetails {
                        series_key: text(r, 0)?,
                        score: r.get(1)?,
                        tags: load_json_array(opt_text(r, 2)?.as_deref()),
                        genres: load_json_array(opt_text(r, 3)?.as_deref()),
                        source: text(r, 4)?,
                        fetched_at: text(r, 5)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_db;
    use serde_json::json;

    fn facts(key: &str, offset: Option<Number>) -> SeriesFacts {
        SeriesFacts {
            series_key: key.into(),
            series_title: "Dr. ストーン".into(),
            external_ids: json!({"anilist": 123}).as_object().unwrap().clone(),
            titles: json!({"ja": "ドクターストーン", "en": "Dr. \"Stone\""})
                .as_object()
                .unwrap()
                .clone(),
            synonyms: vec![json!("DS"), json!("\u{1F600}")],
            tag: Some("shounen".into()),
            unit: None,
            facts_updated_at: "1970-01-01T00:00:00.000Z".into(),
            spine_offset: offset,
            volume_offsets: json!({"V1.cbz": 2.5, "V2.cbz": -40})
                .as_object()
                .unwrap()
                .clone(),
            updated_by: Some("alice".into()),
            updated_at: String::new(),
        }
    }

    #[test]
    fn series_facts_round_trip_python_spelling() {
        let (_dir, db) = temp_db();
        db.put_series_facts(&facts("dr. ストーン", Some(Number::from(-40))))
            .unwrap();
        let got = db.get_series_facts("dr. ストーン").unwrap().unwrap();
        assert_eq!(got.spine_offset, Some(Number::from(-40)));
        assert!(
            got.spine_offset.as_ref().unwrap().is_i64(),
            "NUMERIC keeps -40 an integer"
        );
        assert_eq!(got.titles["ja"], "ドクターストーン");
        assert_eq!(got.updated_at.len(), 19);
        let (titles, synonyms, offsets): (String, String, String) = db
            .with_writer_connection(|c| {
                c.query_row(
                    "SELECT titles, synonyms, volume_offsets FROM series_facts",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
            })
            .unwrap();
        // Insertion order (serde_json preserve_order), as Python's dict keeps it.
        assert_eq!(
            titles,
            "{\"ja\": \"ドクターストーン\", \"en\": \"Dr. \\\"Stone\\\"\"}"
        );
        assert_eq!(synonyms, "[\"DS\", \"\u{1F600}\"]");
        assert_eq!(offsets, "{\"V1.cbz\": 2.5, \"V2.cbz\": -40}");
        db.put_series_facts(&facts("k2", Number::from_f64(8.0)))
            .unwrap();
        assert_eq!(
            db.get_series_facts("k2").unwrap().unwrap().spine_offset,
            Some(Number::from(8))
        );
        db.put_series_facts(&facts("k3", Some(Number::from(u64::MAX))))
            .unwrap();
        assert_eq!(
            db.get_series_facts("k3").unwrap().unwrap().spine_offset,
            None
        );
        assert_eq!(db.list_series_facts().unwrap().len(), 3);
        db.with_writer_connection(|c| {
            c.execute(
                "UPDATE series_facts SET titles = 'nope', synonyms = '{}'",
                [],
            )
        })
        .unwrap();
        let got = db.get_series_facts("k2").unwrap().unwrap();
        assert!(
            got.titles.is_empty() && got.synonyms.is_empty(),
            "corrupt JSON degrades"
        );
    }

    #[test]
    fn entry_cache() {
        let (_dir, db) = temp_db();
        let entry = json!({"volume_uuid": "u1", "mokuro_sha256": "abc", "title": "V1 é"});
        let entry = entry.as_object().unwrap();
        db.put_cached_volume_entry("S/V1.cbz", "s", entry, 100, 1.5, "k")
            .unwrap();
        assert_eq!(
            db.get_cached_volume_entry("S/V1.cbz", 100, 1.5, "k")
                .unwrap()
                .as_ref(),
            Some(entry)
        );
        assert!(
            db.get_cached_volume_entry("S/V1.cbz", 101, 1.5, "k")
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_cached_volume_entry("S/V1.cbz", 100, 1.5000001, "k")
                .unwrap()
                .is_none()
        );
        assert!(
            db.get_cached_volume_entry("S/V1.cbz", 100, 1.5, "j")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.remembered_volume_uuid("S/V1.cbz").unwrap().as_deref(),
            Some("u1")
        );
        let image_only = json!({"volume_uuid": "u2"});
        db.put_cached_volume_entry("S/V2.cbz", "s", image_only.as_object().unwrap(), 1, 2.0, "")
            .unwrap();
        assert_eq!(db.remembered_volume_uuid("S/V2.cbz").unwrap(), None);
        db.put_cached_volume_entry("S/V3.cbz", "s", &Map::new(), 1, 2.0, "")
            .unwrap();
        assert!(
            db.get_cached_volume_entry("S/V3.cbz", 1, 2.0, "")
                .unwrap()
                .is_none(),
            "{{}} is a miss"
        );
        let keep: HashSet<String> = ["S/V1.cbz".to_string()].into();
        assert_eq!(db.prune_series_entry_cache(&keep).unwrap(), 2);
        assert!(
            db.get_cached_volume_entry("S/V1.cbz", 100, 1.5, "k")
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn catalog_and_community() {
        let (_dir, db) = temp_db();
        for (key, folder) in [("b", "Beta"), ("a", "Alpha")] {
            db.upsert_catalog_series(&CatalogSeries {
                series_key: key.into(),
                folder_name: folder.into(),
                cover_path: None,
                volume_count: 3,
                latest_volume_modified: 1700000000.25,
                total_pages: 600,
                total_chars: 12345,
                missing_pages: 1,
                damaged_volumes: 1,
            })
            .unwrap();
        }
        let rows = db.list_catalog_series().unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r.folder_name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "Beta"]
        );
        assert_eq!(rows[0].latest_volume_modified, 1700000000.25);
        assert_eq!(
            db.prune_catalog_series(&HashSet::from(["a".to_string()]))
                .unwrap(),
            1
        );
        db.upsert_community_details(&CommunityDetails {
            series_key: "a".into(),
            score: Some(8.5),
            tags: vec![json!("Time Skip"), json!("日本")],
            genres: vec![],
            source: "anilist".into(),
            fetched_at: "2026-10-01T00:00:00Z".into(),
        })
        .unwrap();
        let tags: String = db
            .with_writer_connection(|c| {
                c.query_row("SELECT tags FROM community_details", [], |r| r.get(0))
            })
            .unwrap();
        assert_eq!(tags, "[\"Time Skip\", \"\\u65e5\\u672c\"]");
        let got = db.list_community_details().unwrap();
        assert_eq!(got[0].tags[1], "日本");
        assert_eq!(got[0].score, Some(8.5));
    }
}
