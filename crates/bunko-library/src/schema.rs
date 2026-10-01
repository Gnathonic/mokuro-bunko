//! The two compiled documents (`<Series>/series.json` v2, root `catalog.json`
//! v1) and the one place that turns them into bytes (`metadata/schema.py`).
//!
//! Clients version their caches on the file's size/mtime, so a rebuild that
//! changed nothing must produce exactly the same bytes: fixed key order, fixed
//! volume order, no wall-clock stamps.

use std::cmp::Ordering;
use std::collections::HashSet;

use crate::compat::{natural_sort_key, normalize_series_key};
use crate::pyjson::{self, DumpOptions, JsonNum, JsonObject, JsonValue, NonFiniteError};
use crate::pyunicode;

/// The stamp of a document whose facts come from nowhere (loses every merge).
pub const FACTLESS_UPDATED_AT: &str = "1970-01-01T00:00:00.000Z";
pub const ID_KEYS: &[&str] = &["anilist", "mal"];
pub const TITLE_KEYS: &[&str] = &["native", "romaji", "english"];
pub const TRACKING_UNITS: &[&str] = &["volumes", "chapters"];

/// The shareable half of a series: what `catalog.json` carries verbatim.
///
/// Values are kept as JSON values (not typed) because a stored row is
/// republished verbatim, whatever it holds.
#[derive(Debug, Clone)]
pub struct SeriesFacts {
    pub external_ids: JsonObject,
    pub titles: JsonObject,
    pub synonyms: Vec<JsonValue>,
    pub tag: Option<String>,
    pub unit: Option<String>,
    pub updated_at: String,
}

impl Default for SeriesFacts {
    fn default() -> Self {
        Self {
            external_ids: JsonObject::new(),
            titles: JsonObject::new(),
            synonyms: Vec::new(),
            tag: None,
            unit: None,
            updated_at: FACTLESS_UPDATED_AT.to_owned(),
        }
    }
}

impl SeriesFacts {
    /// Does this say anything shareable? (client `hasSeriesFacts`; RAW fields,
    /// so it can disagree with what [`facts_payload`] writes — deliberately.)
    pub fn has_facts(&self) -> bool {
        !self.external_ids.is_empty()
            || !self.titles.is_empty()
            || self
                .synonyms
                .iter()
                .any(|s| s.as_str().is_some_and(|s| !pyunicode::strip(s).is_empty()))
            || self
                .tag
                .as_deref()
                .is_some_and(|tag| !pyunicode::strip(tag).is_empty())
            || self.unit.as_deref().is_some_and(|unit| !unit.is_empty())
    }

    /// Dataclass `==` with Python value semantics.
    pub fn py_eq(&self, other: &SeriesFacts) -> bool {
        self.external_ids.py_eq(&other.external_ids)
            && self.titles.py_eq(&other.titles)
            && self.synonyms.len() == other.synonyms.len()
            && self
                .synonyms
                .iter()
                .zip(&other.synonyms)
                .all(|(a, b)| a.py_eq(b))
            && self.tag == other.tag
            && self.unit == other.unit
            && self.updated_at == other.updated_at
    }
}

/// Shelf alignment: INDEX data, never facts, never moves the facts stamp.
#[derive(Debug, Clone, Default)]
pub struct SeriesIndexData {
    /// Stored verbatim (`-40` stays an int, `12.5` a float).
    pub spine_offset: Option<JsonNum>,
    /// `volume_uuid -> offset`, verbatim.
    pub volume_offsets: JsonObject,
}

impl SeriesIndexData {
    pub fn py_eq(&self, other: &SeriesIndexData) -> bool {
        let spine = match (&self.spine_offset, &other.spine_offset) {
            (None, None) => true,
            (Some(a), Some(b)) => a.py_eq(b),
            _ => false,
        };
        spine && self.volume_offsets.py_eq(&other.volume_offsets)
    }
}

/// One compiled volume (offsets are applied at dump time, by uuid).
#[derive(Debug, Clone)]
pub struct VolumeEntry {
    pub volume_uuid: String,
    pub volume_title: String,
    pub page_count: i64,
    /// How many of `page_count` pages have an image in the archive; `None` =
    /// not determined (key omitted, never 0).
    pub matched_page_count: Option<i64>,
    /// A Python int (a sidecar's own `chars` can be any size).
    pub character_count: JsonNum,
    pub mokuro_version: String,
    /// Positive, passed through uncoerced (an int stays an int).
    pub spine_width: Option<JsonNum>,
    pub archive_size: Option<JsonNum>,
    /// Sidecar `st_size` / `int(st_mtime)`; verbatim when read from the cache.
    pub mokuro_size: Option<JsonValue>,
    pub mokuro_modified: Option<JsonValue>,
    pub mokuro_sha256: Option<String>,
    pub cover_size: Option<i64>,
    pub cover_modified: Option<i64>,
}

impl VolumeEntry {
    /// Pages this volume is short (see [`missing_page_count`]).
    pub fn missing_pages(&self) -> i64 {
        missing_page_count(self.page_count, self.matched_page_count)
    }
}

/// Pages a `.mokuro` references that its archive does not contain: 0 when the
/// match was never determined, clamped at 0 (duplicate `img_path`s can make
/// `matched` exceed `page_count`).
pub fn missing_page_count(page_count: i64, matched_page_count: Option<i64>) -> i64 {
    match matched_page_count {
        None => 0,
        Some(matched) => page_count.saturating_sub(matched).max(0),
    }
}

fn is_spine_width(value: Option<&JsonNum>) -> bool {
    value.is_some_and(|num| num.is_finite() == Some(true) && num.is_positive())
}

fn is_sha256(value: Option<&str>) -> bool {
    value.is_some_and(|text| {
        text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// `_facts_payload`: facts in canonical key order, unknown providers/languages dropped.
pub fn facts_payload(facts: &SeriesFacts) -> JsonObject {
    let mut payload = JsonObject::new();
    let mut ids = JsonObject::new();
    for key in ID_KEYS {
        if let Some(value) = facts.external_ids.get(key) {
            ids.insert(*key, value.clone());
        }
    }
    let mut titles = JsonObject::new();
    for key in TITLE_KEYS {
        if let Some(value) = facts.titles.get(key).filter(|value| value.is_truthy()) {
            titles.insert(*key, value.clone());
        }
    }
    payload.insert("external_ids", JsonValue::Object(ids));
    payload.insert("titles", JsonValue::Object(titles));
    payload.insert("synonyms", JsonValue::Array(facts.synonyms.clone()));
    let tag = pyunicode::strip(facts.tag.as_deref().unwrap_or(""));
    if !tag.is_empty() {
        payload.insert("tag", JsonValue::from(tag));
    }
    if let Some(unit) = facts
        .unit
        .as_deref()
        .filter(|unit| TRACKING_UNITS.contains(unit))
    {
        payload.insert("unit", JsonValue::from(unit));
    }
    payload
}

/// `_dumps`: compact, UTF-8, unescaped, non-finite numbers refused.
pub fn dumps_compact(value: &JsonValue) -> Result<Vec<u8>, NonFiniteError> {
    pyjson::dumps(value, DumpOptions::COMPACT_UTF8).map(String::into_bytes)
}

/// Volume order: `(natural_sort_key(title), title)`.
pub fn volume_order(a: &VolumeEntry, b: &VolumeEntry) -> Ordering {
    natural_sort_key(&a.volume_title)
        .cmp(&natural_sort_key(&b.volume_title))
        .then_with(|| a.volume_title.cmp(&b.volume_title))
}

/// Serialize `<Series>/series.json` (contract §2).
pub fn dump_series_file(
    series_title: &str,
    facts: &SeriesFacts,
    index: &SeriesIndexData,
    volumes: &[VolumeEntry],
) -> Result<Vec<u8>, NonFiniteError> {
    let mut payload = JsonObject::new();
    payload.insert("version", JsonValue::from(2));
    payload.insert("series_title", JsonValue::from(series_title));
    for (key, value) in facts_payload(facts).0 {
        payload.insert(key, value);
    }
    if let Some(spine) = index
        .spine_offset
        .as_ref()
        .filter(|spine| spine.is_truthy())
    {
        payload.insert("spine_offset", JsonValue::Num(spine.clone()));
    }
    payload.insert("updated_at", JsonValue::from(facts.updated_at.as_str()));

    // Dedup by uuid (first wins), then a stable sort with the raw-title tiebreak.
    let mut seen = HashSet::new();
    let mut ordered: Vec<&VolumeEntry> = volumes
        .iter()
        .filter(|volume| seen.insert(volume.volume_uuid.as_str()))
        .collect();
    let mut keyed: Vec<(crate::compat::NaturalKey, &VolumeEntry)> = ordered
        .drain(..)
        .map(|volume| (natural_sort_key(&volume.volume_title), volume))
        .collect();
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.volume_title.cmp(&b.1.volume_title))
    });

    let mut entries = Vec::with_capacity(keyed.len());
    for (_, volume) in keyed {
        let mut entry = JsonObject::new();
        entry.insert("volume_uuid", JsonValue::from(volume.volume_uuid.as_str()));
        entry.insert(
            "volume_title",
            JsonValue::from(volume.volume_title.as_str()),
        );
        entry.insert("page_count", JsonValue::from(volume.page_count));
        if let Some(matched) = volume.matched_page_count {
            entry.insert("matched_page_count", JsonValue::from(matched));
        }
        entry.insert(
            "character_count",
            JsonValue::Num(volume.character_count.clone()),
        );
        entry.insert(
            "mokuro_version",
            JsonValue::from(volume.mokuro_version.as_str()),
        );
        if is_spine_width(volume.spine_width.as_ref()) {
            entry.insert(
                "spine_width",
                JsonValue::Num(volume.spine_width.clone().unwrap_or(JsonNum::Int(0))),
            );
        }
        if let Some(size) = volume
            .archive_size
            .as_ref()
            .filter(|size| size.is_positive())
        {
            entry.insert("archive_size", JsonValue::Num(size.clone()));
        }
        if let Some(size) = &volume.mokuro_size {
            entry.insert("mokuro_size", size.clone());
        }
        if let Some(modified) = &volume.mokuro_modified {
            entry.insert("mokuro_modified", modified.clone());
        }
        if is_sha256(volume.mokuro_sha256.as_deref()) {
            entry.insert(
                "mokuro_sha256",
                JsonValue::from(volume.mokuro_sha256.clone()),
            );
        }
        if let Some(size) = volume.cover_size {
            entry.insert("cover_size", JsonValue::from(size));
        }
        if let Some(modified) = volume.cover_modified {
            entry.insert("cover_modified", JsonValue::from(modified));
        }
        if let Some(offset) = index
            .volume_offsets
            .get(&volume.volume_uuid)
            .filter(|offset| offset.is_truthy())
        {
            entry.insert("offset", offset.clone());
        }
        entries.push(JsonValue::Object(entry));
    }
    payload.insert("volumes", JsonValue::Array(entries));
    dumps_compact(&JsonValue::Object(payload))
}

/// Serialize the root `catalog.json` (contract §3). The file's own
/// `updated_at` is the NEWEST entry stamp, never the clock.
pub fn dump_catalog_file(entries: &[(String, SeriesFacts)]) -> Result<Vec<u8>, NonFiniteError> {
    let mut seen = HashSet::new();
    let mut ordered: Vec<(String, &String, &SeriesFacts)> = Vec::new();
    for (title, facts) in entries {
        let key = normalize_series_key(title);
        if seen.insert(key.clone()) {
            ordered.push((key, title, facts));
        }
    }
    ordered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let mut newest = FACTLESS_UPDATED_AT.to_owned();
    let mut series = Vec::with_capacity(ordered.len());
    for (_, title, facts) in ordered {
        let mut entry = JsonObject::new();
        entry.insert("series_title", JsonValue::from(title.as_str()));
        for (key, value) in facts_payload(facts).0 {
            entry.insert(key, value);
        }
        entry.insert("updated_at", JsonValue::from(facts.updated_at.as_str()));
        if facts.updated_at > newest {
            newest = facts.updated_at.clone();
        }
        series.push(JsonValue::Object(entry));
    }
    let mut payload = JsonObject::new();
    payload.insert("version", JsonValue::from(1));
    payload.insert("updated_at", JsonValue::from(newest));
    payload.insert("series", JsonValue::Array(series));
    dumps_compact(&JsonValue::Object(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(uuid: &str, title: &str) -> VolumeEntry {
        VolumeEntry {
            volume_uuid: uuid.into(),
            volume_title: title.into(),
            page_count: 187,
            matched_page_count: None,
            character_count: JsonNum::Int(13247),
            mokuro_version: "0.2.2".into(),
            spine_width: None,
            archive_size: Some(JsonNum::Int(1234)),
            mokuro_size: None,
            mokuro_modified: None,
            mokuro_sha256: None,
            cover_size: None,
            cover_modified: None,
        }
    }

    #[test]
    fn spec_example_series_file() {
        let bytes = dump_series_file(
            "Bakemonogatari",
            &SeriesFacts::default(),
            &SeriesIndexData::default(),
            &[entry("cfb5220c-57db-4008-9f44-e659d794e381", "v01")],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            r#"{"version":2,"series_title":"Bakemonogatari","external_ids":{},"titles":{},"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z","volumes":[{"volume_uuid":"cfb5220c-57db-4008-9f44-e659d794e381","volume_title":"v01","page_count":187,"character_count":13247,"mokuro_version":"0.2.2","archive_size":1234}]}"#
        );
    }

    #[test]
    fn empty_catalog() {
        assert_eq!(
            String::from_utf8(dump_catalog_file(&[]).unwrap()).unwrap(),
            r#"{"version":1,"updated_at":"1970-01-01T00:00:00.000Z","series":[]}"#
        );
    }
}
