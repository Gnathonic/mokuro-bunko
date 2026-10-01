//! Client-submitted series facts: validating an untrusted `PUT
//! <Series>/series.json` body (`metadata/validate.py`) and merging it into
//! the stored state (`metadata/merge.py`).
//!
//! Two independent merges: FACTS on the facts stamp (newest wins, ties keep
//! the incoming copy, a factless payload needs a STRICTLY newer stamp), and
//! INDEX (shelf alignment) by presence, never touching the facts stamp.

use std::collections::HashSet;

use crate::compat::normalize_updated_at;
use crate::pyjson::{self, JsonNum, JsonObject, JsonValue, ParseOptions};
use crate::pyunicode;
use crate::schema::{ID_KEYS, SeriesFacts, SeriesIndexData, TITLE_KEYS, TRACKING_UNITS};

/// Largest accepted PUT body (`MAX_UPDATE_BODY_BYTES`, 4 MiB).
pub const MAX_UPDATE_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// A validated update REQUEST — not a file, and not authoritative.
#[derive(Debug, Clone)]
pub struct SeriesUpdate {
    pub facts: SeriesFacts,
    pub spine_offset: Option<JsonNum>,
    /// Absence is silence (inherit); presence replaces.
    pub spine_offset_present: bool,
    pub volume_offsets: JsonObject,
    /// Volumes the payload named at all, first-seen order. Listed WITHOUT an
    /// offset clears a stored one.
    pub listed_uuids: Vec<String>,
}

/// `_is_offset`: a real, finite, non-zero number (never a bool), kept verbatim.
fn as_offset(value: Option<&JsonValue>) -> Option<JsonNum> {
    let num = value?.as_num()?;
    (num.is_finite() == Some(true) && num.is_truthy()).then(|| num.clone())
}

fn facts_from(raw: &JsonObject, updated_at: String) -> SeriesFacts {
    let mut external_ids = JsonObject::new();
    if let Some(JsonValue::Object(ids)) = raw.get("external_ids") {
        for key in ID_KEYS {
            if let Some(JsonValue::Num(num)) = ids.get(key)
                && num.is_int()
                && num.is_positive()
            {
                external_ids.insert(*key, JsonValue::Num(num.clone()));
            }
        }
    }
    let mut titles = JsonObject::new();
    if let Some(JsonValue::Object(raw_titles)) = raw.get("titles") {
        for key in TITLE_KEYS {
            if let Some(JsonValue::Str(text)) = raw_titles.get(key)
                && !pyunicode::strip(text).is_empty()
            {
                titles.insert(*key, JsonValue::from(text.as_str()));
            }
        }
    }
    let synonyms = match raw.get("synonyms") {
        Some(JsonValue::Array(items)) => items
            .iter()
            .filter(|item| {
                item.as_str()
                    .is_some_and(|text| !pyunicode::strip(text).is_empty())
            })
            .cloned()
            .collect(),
        _ => Vec::new(),
    };
    let tag = raw
        .get("tag")
        .and_then(JsonValue::as_str)
        .map(pyunicode::strip)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned);
    let unit = raw
        .get("unit")
        .and_then(JsonValue::as_str)
        .filter(|unit| TRACKING_UNITS.contains(unit))
        .map(str::to_owned);
    SeriesFacts {
        external_ids,
        titles,
        synonyms,
        tag,
        unit,
        updated_at,
    }
}

/// `parse_series_update(payload, now=now)`: `None` means "reject" (400).
pub fn parse_series_update(payload: &[u8], now: f64) -> Option<SeriesUpdate> {
    let text = std::str::from_utf8(payload).ok()?;
    let JsonValue::Object(decoded) = pyjson::parse_with(
        text,
        ParseOptions {
            allow_constants: false,
        },
    )
    .ok()?
    else {
        return None;
    };
    // `version not in (1, 2)` with bools excluded; `1.0 == 1` in Python.
    let version_ok = matches!(decoded.get("version"), Some(JsonValue::Num(num))
        if num.py_eq(&JsonNum::Int(1)) || num.py_eq(&JsonNum::Int(2)));
    if !version_ok {
        return None;
    }
    let updated_at = normalize_updated_at(decoded.get("updated_at"), now)?;

    let spine_offset = as_offset(decoded.get("spine_offset"));
    let spine_offset_present = spine_offset.is_some();

    let mut volume_offsets = JsonObject::new();
    let mut listed: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if let Some(JsonValue::Array(volumes)) = decoded.get("volumes") {
        for raw_entry in volumes {
            let JsonValue::Object(entry) = raw_entry else {
                continue;
            };
            let Some(uuid) = entry.get("volume_uuid").and_then(JsonValue::as_str) else {
                continue;
            };
            if pyunicode::strip(uuid).is_empty() || !seen.insert(uuid.to_owned()) {
                continue;
            }
            listed.push(uuid.to_owned());
            if let Some(offset) = as_offset(entry.get("offset")) {
                volume_offsets.insert(uuid, JsonValue::Num(offset));
            }
        }
    }
    Some(SeriesUpdate {
        facts: facts_from(&decoded, updated_at),
        spine_offset,
        spine_offset_present,
        volume_offsets,
        listed_uuids: listed,
    })
}

/// What bunko currently holds for one series.
#[derive(Debug, Clone)]
pub struct StoredSeries {
    pub facts: SeriesFacts,
    pub index: SeriesIndexData,
}

/// The merged state and what moved.
#[derive(Debug, Clone)]
pub struct MergeResult {
    pub facts: SeriesFacts,
    pub index: SeriesIndexData,
    pub facts_changed: bool,
    pub index_changed: bool,
}

impl MergeResult {
    /// Did anything move? Drives "rewrite the files or not".
    pub fn changed(&self) -> bool {
        self.facts_changed || self.index_changed
    }
}

fn merge_facts(stored: Option<&SeriesFacts>, incoming: &SeriesFacts) -> SeriesFacts {
    let Some(stored) = stored else {
        return incoming.clone();
    };
    let incoming_wins = if incoming.has_facts() {
        incoming.updated_at >= stored.updated_at
    } else {
        incoming.updated_at > stored.updated_at
    };
    if incoming_wins {
        incoming.clone()
    } else {
        stored.clone()
    }
}

fn merge_index(stored: Option<&SeriesIndexData>, update: &SeriesUpdate) -> SeriesIndexData {
    let base = stored.cloned().unwrap_or_default();
    let spine_offset = if update.spine_offset_present {
        update.spine_offset.clone()
    } else {
        base.spine_offset
    };
    let mut offsets = base.volume_offsets;
    for uuid in &update.listed_uuids {
        match update.volume_offsets.get(uuid) {
            Some(offset) => offsets.insert(uuid.as_str(), offset.clone()),
            None => {
                offsets.remove(uuid);
            }
        }
    }
    SeriesIndexData {
        spine_offset,
        volume_offsets: offsets,
    }
}

/// `merge_series_update`: fold a validated update into the stored state.
pub fn merge_series_update(stored: Option<&StoredSeries>, update: &SeriesUpdate) -> MergeResult {
    let facts = merge_facts(stored.map(|s| &s.facts), &update.facts);
    let index = merge_index(stored.map(|s| &s.index), update);
    MergeResult {
        facts_changed: stored.is_none_or(|s| !facts.py_eq(&s.facts)),
        index_changed: stored.is_none_or(|s| !index.py_eq(&s.index)),
        facts,
        index,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_800_000_000.0;

    #[test]
    fn validation_rules() {
        assert!(parse_series_update(b"{\"version\":2}", NOW).is_none());
        assert!(
            parse_series_update(
                b"{\"version\":true,\"updated_at\":\"2026-01-01T00:00:00Z\"}",
                NOW
            )
            .is_none()
        );
        assert!(
            parse_series_update(
                b"{\"version\":1.0,\"updated_at\":\"2026-01-01T00:00:00Z\"}",
                NOW
            )
            .is_some()
        );
        assert!(
            parse_series_update(
                b"{\"version\":2,\"updated_at\":\"2026-01-01T00:00:00Z\",\"x\":NaN}",
                NOW
            )
            .is_none()
        );
        let update = parse_series_update(
            br#"{"version":2,"updated_at":"2026-01-01T00:00:00Z","external_ids":{"anilist":98416.0,"mal":5},"tag":"  HD  ","spine_offset":-40,"volumes":[{"volume_uuid":"u","offset":0},{"volume_uuid":"u","offset":3}]}"#,
            NOW,
        )
        .unwrap();
        assert_eq!(update.facts.external_ids.len(), 1);
        assert_eq!(update.facts.tag.as_deref(), Some("HD"));
        assert!(update.spine_offset_present);
        assert_eq!(update.listed_uuids, vec!["u".to_owned()]);
        assert!(update.volume_offsets.is_empty());
    }

    #[test]
    fn factless_needs_strictly_newer() {
        let stored = StoredSeries {
            facts: SeriesFacts {
                tag: Some("x".into()),
                updated_at: "2026-01-01T00:00:00.000Z".into(),
                ..Default::default()
            },
            index: SeriesIndexData::default(),
        };
        let tie = parse_series_update(br#"{"version":2,"updated_at":"2026-01-01T00:00:00Z"}"#, NOW)
            .unwrap();
        let result = merge_series_update(Some(&stored), &tie);
        assert!(!result.changed());
        let newer =
            parse_series_update(br#"{"version":2,"updated_at":"2026-01-01T00:00:01Z"}"#, NOW)
                .unwrap();
        assert!(merge_series_update(Some(&stored), &newer).facts_changed);
    }
}
