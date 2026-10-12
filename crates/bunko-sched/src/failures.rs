//! Failure records and retry backoff (`.ocr-failures.json`; the worker's
//! `failure_key`, `_record_ocr_failure`, `_retry_delay_seconds`,
//! `_replaced_since_failure`, `_eligible_ocr_jobs`, `_prune_failure_records`).
//!
//! The file maps `failure_key → record` and is written atomically with
//! `indent=2, ensure_ascii=False`, deleted when empty. Records are kept as
//! JSON objects so fields a newer or older server wrote survive a rewrite.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::SchedError;
use crate::py::{Object, float_value, py_float, py_int};
use crate::pyjson::{dumps_indent2, read_object, write_atomic};

pub const FAILURES_FILE: &str = ".ocr-failures.json";
/// The longest a failed volume waits before its next try.
pub const MAX_RETRY_DELAY_SECONDS: f64 = 3600.0;
/// Archive extensions a record's volume may still exist under.
pub const ARCHIVE_EXTENSIONS: [&str; 4] = [".cbz", ".cbr", ".zip", ".rar"];

/// `failure_key(rel_cbz, row)`: the primary row keeps the bare relative
/// path; every other row is suffixed `@<row name>` (name, not id: people
/// read this file, and a renamed row should stop matching).
pub fn failure_key(rel_cbz: &str, primary: bool, row_name: &str) -> String {
    if primary {
        rel_cbz.to_owned()
    } else {
        format!("{rel_cbz}@{row_name}")
    }
}

/// A failure record's `series`: `str(path.parent.relative_to(library))`,
/// which is `"."` (not `""`) for a volume directly in `library/` — a 0.5.2
/// quirk kept so records stay byte-identical (the prune rule reads it back
/// as `library/./<volume>.*`, which is the same folder).
pub fn record_series(rel_parent: &str) -> String {
    if rel_parent.is_empty() {
        ".".to_owned()
    } else {
        rel_parent.to_owned()
    }
}

/// `_retry_delay_seconds(attempts)`: `min(poll × 4^min(max(0, n−1), 16), 3600)`.
pub fn retry_delay_seconds(poll_interval: f64, attempts: i64) -> f64 {
    let exponent = attempts.saturating_sub(1).clamp(0, 16);
    (poll_interval * 4.0f64.powf(exponent as f64)).min(MAX_RETRY_DELAY_SECONDS)
}

/// `float(entry.get("last_attempt_at", 0.0))`.
pub fn last_attempt_at(record: &Object) -> f64 {
    match record.get("last_attempt_at") {
        None => 0.0,
        v => py_float(v).unwrap_or(0.0),
    }
}

/// `int(entry.get("attempts", default))`.
pub fn attempts(record: &Object, default: i64) -> i64 {
    match record.get("attempts") {
        None => default,
        v => py_int(v).unwrap_or(default),
    }
}

/// `_replaced_since_failure`: the archive's mtime (None: unreadable → 0)
/// is more than 1 s after the recorded attempt.
pub fn replaced_since_failure(archive_mtime: Option<f64>, record: &Object) -> bool {
    archive_mtime.unwrap_or(0.0) > last_attempt_at(record) + 1.0
}

/// Whether a candidate job may run now, given its failure record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eligibility {
    /// No record: runs.
    Fresh,
    /// The archive was replaced since the failure: runs, and the stale
    /// record is dropped by the worker's next claim.
    Replaced,
    /// The backoff has run out: runs (a retry).
    Retry,
    /// Still in backoff: skipped.
    BackedOff,
}

impl Eligibility {
    pub fn may_run(self) -> bool {
        !matches!(self, Eligibility::BackedOff)
    }
}

/// `_eligible_ocr_jobs`' rule for one candidate.
pub fn eligibility(
    record: Option<&Object>,
    archive_mtime: Option<f64>,
    now: f64,
    poll_interval: f64,
) -> Eligibility {
    let Some(record) = record else {
        return Eligibility::Fresh;
    };
    if replaced_since_failure(archive_mtime, record) {
        return Eligibility::Replaced;
    }
    if now >= last_attempt_at(record) + retry_delay_seconds(poll_interval, attempts(record, 1)) {
        Eligibility::Retry
    } else {
        Eligibility::BackedOff
    }
}

/// `_pending_entry`'s `attempts`: shown only for a record not made void by
/// a replaced archive.
pub fn pending_attempts(record: Option<&Object>, archive_mtime: Option<f64>) -> Option<i64> {
    let record = record?;
    if replaced_since_failure(archive_mtime, record) {
        None
    } else {
        Some(attempts(record, 1))
    }
}

/// What a failure record says (the 0.5.2 record shape, in its key order).
#[derive(Clone, Debug, PartialEq)]
pub struct FailureRecord {
    /// Library-relative parent folder (`"."` for a volume directly in
    /// `library/`, see [`record_series`]).
    pub series: String,
    /// The archive's stem.
    pub volume: String,
    /// The row's name.
    pub generation: String,
    pub engine: String,
    /// `reported_detector` (None for rows whose engine brings its own).
    pub detector: Option<String>,
    pub error: String,
    pub attempts: i64,
    pub last_attempt_at: f64,
    pub log_file: Option<String>,
}

impl FailureRecord {
    pub fn to_object(&self) -> Object {
        let opt = |v: &Option<String>| v.as_ref().map_or(Value::Null, |s| Value::String(s.clone()));
        let mut m = Map::new();
        m.insert("series".into(), Value::from(self.series.as_str()));
        m.insert("volume".into(), Value::from(self.volume.as_str()));
        m.insert("generation".into(), Value::from(self.generation.as_str()));
        m.insert("engine".into(), Value::from(self.engine.as_str()));
        m.insert("detector".into(), opt(&self.detector));
        m.insert("error".into(), Value::from(self.error.as_str()));
        m.insert("attempts".into(), Value::from(self.attempts));
        m.insert("last_attempt_at".into(), float_value(self.last_attempt_at));
        m.insert("log_file".into(), opt(&self.log_file));
        m
    }

    /// Read a record leniently (missing fields default).
    pub fn from_object(record: &Object) -> Self {
        let s = |k: &str| {
            record
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let o = |k: &str| record.get(k).and_then(Value::as_str).map(str::to_owned);
        FailureRecord {
            series: s("series"),
            volume: s("volume"),
            generation: s("generation"),
            engine: s("engine"),
            detector: o("detector"),
            error: s("error"),
            attempts: attempts(record, 1),
            last_attempt_at: last_attempt_at(record),
            log_file: o("log_file"),
        }
    }
}

/// The parts of a new failure the caller knows (`attempts` and the time
/// come from the store).
#[derive(Clone, Debug, PartialEq)]
pub struct NewFailure<'a> {
    pub series: &'a str,
    pub volume: &'a str,
    pub generation: &'a str,
    pub engine: &'a str,
    pub detector: Option<&'a str>,
    /// None: "unknown error".
    pub error: Option<&'a str>,
    pub log_file: Option<&'a str>,
}

/// `_prune_failure_records`' rule for one record: kept iff its `generation`
/// is not a non-empty string or is a current row name, AND its
/// `series`/`volume` are not both strings (volume non-empty) or the archive
/// `library/<series>/<volume>.{cbz,cbr,zip,rar}` still exists. Never parses
/// the key.
pub fn keep_record(
    record: &Object,
    row_names: &[String],
    archive_exists: impl Fn(&str, &str) -> bool,
) -> bool {
    let configured = match record.get("generation") {
        Some(Value::String(name)) if !name.is_empty() => row_names.iter().any(|n| n == name),
        _ => true,
    };
    if !configured {
        return false;
    }
    match (record.get("series"), record.get("volume")) {
        (Some(Value::String(series)), Some(Value::String(volume))) if !volume.is_empty() => {
            archive_exists(series, volume)
        }
        _ => true,
    }
}

/// `archive_still_there` against a real library directory.
pub fn archive_exists_in(library: &Path, series: &str, volume: &str) -> bool {
    let folder = if series.is_empty() {
        library.to_path_buf()
    } else {
        library.join(series)
    };
    ARCHIVE_EXTENSIONS
        .iter()
        .any(|ext| folder.join(format!("{volume}{ext}")).is_file())
}

/// `<storage>/.ocr-failures.json`.
#[derive(Clone, Debug)]
pub struct FailureStore {
    pub path: PathBuf,
}

impl FailureStore {
    pub fn new(storage: &Path) -> Self {
        FailureStore {
            path: storage.join(FAILURES_FILE),
        }
    }

    /// The persisted records (object values only), file order.
    pub fn load(&self) -> Map<String, Value> {
        read_object(&self.path)
            .map(|m| m.into_iter().filter(|(_, v)| v.is_object()).collect())
            .unwrap_or_default()
    }

    /// Persist atomically; delete the file when there are no records.
    pub fn save(&self, failures: &Map<String, Value>) -> Result<(), SchedError> {
        if failures.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(SchedError::Io {
                    path: self.path.clone(),
                    source: e,
                }),
            };
        }
        write_atomic(&self.path, &dumps_indent2(&Value::Object(failures.clone()))).map_err(|e| {
            SchedError::Io {
                path: self.path.clone(),
                source: e,
            }
        })
    }

    /// `_record_ocr_failure`: write the record (attempts = previous + 1) and
    /// return it.
    pub fn record(
        &self,
        key: &str,
        failure: &NewFailure<'_>,
        now: f64,
    ) -> Result<FailureRecord, SchedError> {
        let mut failures = self.load();
        let record = record_failure(&mut failures, key, failure, now);
        self.save(&failures)?;
        Ok(record)
    }

    /// `_clear_ocr_failure`: drop a key; writes only when it existed.
    pub fn clear(&self, key: &str) -> Result<bool, SchedError> {
        let mut failures = self.load();
        if failures.shift_remove(key).is_none() {
            return Ok(false);
        }
        self.save(&failures)?;
        Ok(true)
    }

    /// Drop several keys (the stale records a claim found); writes only when
    /// something was dropped.
    pub fn drop_keys(&self, keys: &[String]) -> Result<bool, SchedError> {
        let mut failures = self.load();
        let before = failures.len();
        for key in keys {
            failures.shift_remove(key);
        }
        if failures.len() == before {
            return Ok(false);
        }
        self.save(&failures)?;
        Ok(true)
    }

    /// `_prune_failure_records`: keep what [`keep_record`] keeps; writes only
    /// when something was dropped. Returns whether it wrote.
    pub fn prune(
        &self,
        row_names: &[String],
        archive_exists: impl Fn(&str, &str) -> bool,
    ) -> Result<bool, SchedError> {
        let failures = self.load();
        let before = failures.len();
        let kept: Map<String, Value> = failures
            .into_iter()
            .filter(|(_, v)| {
                v.as_object()
                    .is_some_and(|r| keep_record(r, row_names, &archive_exists))
            })
            .collect();
        if kept.len() == before {
            return Ok(false);
        }
        self.save(&kept)?;
        Ok(true)
    }
}

/// `_record_ocr_failure`'s map update: replaces the record under `key`
/// (keeping its position), `attempts = int(previous attempts or 0) + 1`.
pub fn record_failure(
    failures: &mut Map<String, Value>,
    key: &str,
    failure: &NewFailure<'_>,
    now: f64,
) -> FailureRecord {
    let previous = failures
        .get(key)
        .and_then(Value::as_object)
        .map_or(0, |p| attempts(p, 0));
    let record = FailureRecord {
        series: failure.series.to_owned(),
        volume: failure.volume.to_owned(),
        generation: failure.generation.to_owned(),
        engine: failure.engine.to_owned(),
        detector: failure.detector.map(str::to_owned),
        error: failure.error.unwrap_or("unknown error").to_owned(),
        attempts: previous + 1,
        last_attempt_at: now,
        log_file: failure.log_file.map(str::to_owned),
    };
    failures.insert(key.to_owned(), Value::Object(record.to_object()));
    record
}

/// The log line of a recorded failure.
pub fn failure_log_line(
    row_name: &str,
    rel_cbz: &str,
    attempts: i64,
    poll_interval: f64,
    error: &str,
) -> String {
    let delay = retry_delay_seconds(poll_interval, attempts).trunc() as i64;
    format!(
        "OCR ({row_name}) failed for {rel_cbz} (attempt {attempts}, next retry in ~{delay}s): {error}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule() {
        let d: Vec<f64> = (0..8).map(|n| retry_delay_seconds(30.0, n)).collect();
        assert_eq!(
            d,
            vec![30.0, 30.0, 120.0, 480.0, 1920.0, 3600.0, 3600.0, 3600.0]
        );
        assert_eq!(retry_delay_seconds(30.0, 10_000), 3600.0);
    }

    #[test]
    fn keys() {
        assert_eq!(failure_key("S/V.cbz", true, "x"), "S/V.cbz");
        assert_eq!(failure_key("S/V.cbz", false, "fast"), "S/V.cbz@fast");
    }

    #[test]
    fn store_round_trip_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = FailureStore::new(dir.path());
        let f = NewFailure {
            series: "S",
            volume: "V",
            generation: "main",
            engine: "hayai",
            detector: None,
            error: Some("boom"),
            log_file: None,
        };
        assert_eq!(store.record("S/V.cbz", &f, 100.5).unwrap().attempts, 1);
        assert_eq!(store.record("S/V.cbz", &f, 101.5).unwrap().attempts, 2);
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert!(text.starts_with("{\n  \"S/V.cbz\": {\n    \"series\": \"S\","));
        assert!(text.contains("\"last_attempt_at\": 101.5"));
        assert!(store.clear("S/V.cbz").unwrap());
        assert!(!store.path.exists());
    }
}
