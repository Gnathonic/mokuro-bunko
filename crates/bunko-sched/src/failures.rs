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
    /// A failure of the volume itself that nothing since could have changed: not
    /// retried by time ([`FailureKind::parks`]).
    Parked,
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

/// What a failure record's error means, as far as anyone browsing the library needs to
/// know: the catalog shows [`FailureKind::public_text`], never the error itself (it can
/// name a processor's paths, devices and settings).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// The archive holds no page the OCR could decode.
    NoReadablePages,
    /// The archive could not be opened as a zip.
    DamagedArchive,
    /// A machine's trouble (out of memory, a timeout, a lost connection or download).
    Machine,
    /// Anything else.
    Other,
}

impl FailureKind {
    /// The kind of a record's `error`, from the messages the OCR reports.
    pub fn of(error: &str) -> FailureKind {
        let e = error.trim();
        if e.starts_with("no page images found") || e == "every page failed" {
            FailureKind::NoReadablePages
        } else if e.starts_with("archive ") || e.contains("not a zip file") {
            FailureKind::DamagedArchive
        } else {
            let lower = e.to_ascii_lowercase();
            let machine = [
                "out of memory",
                "timed out",
                "timeout",
                "connection",
                "disconnect",
                "download",
            ];
            if machine.iter().any(|m| lower.contains(m)) {
                FailureKind::Machine
            } else {
                FailureKind::Other
            }
        }
    }

    /// A failure of the volume itself: waiting cannot help, so it is parked until the
    /// archive, the library's release or a release that has not tried it could change
    /// the outcome (see [`machine_may_retry`]).
    pub fn parks(self) -> bool {
        matches!(
            self,
            FailureKind::NoReadablePages | FailureKind::DamagedArchive
        )
    }

    /// One sentence for the catalog's hover text.
    pub fn public_text(self) -> &'static str {
        match self {
            FailureKind::NoReadablePages => "This volume has no pages that could be decoded.",
            FailureKind::DamagedArchive => "The archive is damaged or is not a valid zip file.",
            FailureKind::Machine | FailureKind::Other => {
                "OCR failed; it will be tried again later."
            }
        }
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

/// Record field: the library's release when the record was last written.
pub const LIBRARY_VERSION: &str = "library_version";
/// Record field: the releases of the machines that failed it, once each, in order.
pub const TRIED_BY: &str = "tried_by";

/// The retry delay for a failure of `kind` after `attempts`: a machine's trouble keeps
/// [`retry_delay_seconds`] (capped at an hour); an error nobody recognizes backs off
/// further once it reached the cap: 6 h at the 7th attempt, a day at the 8th, a week
/// from the 9th on. (A parked failure is not retried by time at all.)
pub fn retry_delay_for(kind: FailureKind, poll_interval: f64, attempts: i64) -> f64 {
    let base = retry_delay_seconds(poll_interval, attempts);
    match (kind, attempts) {
        (FailureKind::Machine, _) | (_, ..=6) => base,
        (_, 7) => 6.0 * 3600.0,
        (_, 8) => 24.0 * 3600.0,
        _ => 7.0 * 86400.0,
    }
}

/// Stamp a record the server just wrote with the library's release and add the failing
/// machine's to `tried_by` (once).
pub fn stamp_versions(record: &mut Object, library_version: &str, machine_version: &str) {
    record.insert(LIBRARY_VERSION.into(), Value::from(library_version));
    let mut tried = tried_by(record);
    if !tried.iter().any(|v| v == machine_version) {
        tried.push(machine_version.to_string());
    }
    record.insert(
        TRIED_BY.into(),
        Value::Array(tried.into_iter().map(Value::from).collect()),
    );
}

fn tried_by(record: &Object) -> Vec<String> {
    record
        .get(TRIED_BY)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn error_kind(record: &Object) -> FailureKind {
    FailureKind::of(record.get("error").and_then(Value::as_str).unwrap_or(""))
}

/// Whether a machine of release `machine_version` may try a volume again, given its
/// failure record (none: yes). A machine's trouble: once its backoff is over. Otherwise
/// a machine may when the library's release changed since the record was written, or
/// its own release has not tried it; an unrecognized error also once its (growing)
/// backoff is over. A parked failure never comes back by time.
pub fn machine_may_retry(
    record: Option<&Object>,
    machine_version: &str,
    now: f64,
    poll_interval: f64,
    library_version: &str,
) -> bool {
    let Some(record) = record else {
        return true;
    };
    let kind = error_kind(record);
    let delay = retry_delay_for(kind, poll_interval, attempts(record, 1));
    let timed = now >= last_attempt_at(record) + delay;
    if kind == FailureKind::Machine {
        return timed;
    }
    let library_changed =
        record.get(LIBRARY_VERSION).and_then(Value::as_str) != Some(library_version);
    let untried = !tried_by(record).iter().any(|v| v == machine_version);
    library_changed || untried || (!kind.parks() && timed)
}

/// The releases a failed volume could be retried by: the library's and those of the
/// machines connected now.
#[derive(Clone, Debug, Default)]
pub struct Fleet<'a> {
    pub library_version: &'a str,
    /// The connected machines' releases (none connected: judged as the library's).
    pub machine_versions: Vec<String>,
}

/// [`eligibility`] for a fleet: a record is retried when some connected machine may
/// retry it ([`machine_may_retry`]); else a failure of the volume itself is
/// [`Eligibility::Parked`] and any other [`Eligibility::BackedOff`].
pub fn eligibility_in(
    record: Option<&Object>,
    archive_mtime: Option<f64>,
    now: f64,
    poll_interval: f64,
    fleet: &Fleet<'_>,
) -> Eligibility {
    let Some(record) = record else {
        return Eligibility::Fresh;
    };
    if replaced_since_failure(archive_mtime, record) {
        return Eligibility::Replaced;
    }
    let lone = [fleet.library_version.to_string()];
    let versions: &[String] = if fleet.machine_versions.is_empty() {
        &lone
    } else {
        &fleet.machine_versions
    };
    if versions
        .iter()
        .any(|v| machine_may_retry(Some(record), v, now, poll_interval, fleet.library_version))
    {
        Eligibility::Retry
    } else if error_kind(record).parks() {
        Eligibility::Parked
    } else {
        Eligibility::BackedOff
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
    let kind = FailureKind::of(error);
    if kind.parks() {
        return format!(
            "OCR ({row_name}) failed for {rel_cbz} (attempt {attempts}, not retried until the archive is replaced or a release changes): {error}"
        );
    }
    let delay = retry_delay_for(kind, poll_interval, attempts).trunc() as i64;
    format!(
        "OCR ({row_name}) failed for {rel_cbz} (attempt {attempts}, next retry in ~{delay}s): {error}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the catalog may show anyone: the kind of failure only, never the error text
    /// (which names a processor's paths).
    #[test]
    fn failure_kinds_and_their_public_text() {
        use FailureKind::*;
        for (error, kind) in [
            ("no page images found in Vol 1.cbz", NoReadablePages),
            ("every page failed", NoReadablePages),
            (
                "archive /proc/self/fd/7: no end of central directory",
                DamagedArchive,
            ),
            ("File is not a zip file", DamagedArchive),
            ("CUDA out of memory", Machine),
            ("download of the archive timed out", Machine),
            ("the models would not load", Other),
            ("", Other),
        ] {
            assert_eq!(FailureKind::of(error), kind, "{error:?}");
        }
        assert_eq!(
            NoReadablePages.public_text(),
            "This volume has no pages that could be decoded."
        );
        assert_eq!(
            DamagedArchive.public_text(),
            "The archive is damaged or is not a valid zip file."
        );
        assert_eq!(
            Other.public_text(),
            "OCR failed; it will be tried again later."
        );
        assert_eq!(Machine.public_text(), Other.public_text());
    }

    /// A record as the server writes it: failed at `at` with `error`, by a machine of
    /// release `machine` while the library ran `library`.
    fn failed(error: &str, at: f64, attempts: i64, library: &str, machine: &str) -> Object {
        let mut failures = Map::new();
        let new = NewFailure {
            series: "S",
            volume: "V",
            generation: "g",
            engine: "e",
            detector: None,
            error: Some(error),
            log_file: None,
        };
        for i in 0..attempts {
            record_failure(
                &mut failures,
                "S/V.cbz",
                &new,
                at - (attempts - 1 - i) as f64,
            );
        }
        let mut record = failures["S/V.cbz"].as_object().unwrap().clone();
        stamp_versions(&mut record, library, machine);
        record
    }

    fn fleet<'a>(library: &'a str, machines: &[&str]) -> Fleet<'a> {
        Fleet {
            library_version: library,
            machine_versions: machines.iter().map(|m| m.to_string()).collect(),
        }
    }

    /// A failure of the volume itself is parked: no amount of waiting brings it back,
    /// only a replaced archive, another library release, or a machine of a release that
    /// has not tried it (and then only that machine).
    #[test]
    fn volume_failures_park_until_something_could_change_the_outcome() {
        let r = failed("no page images found in V.cbz", 1000.0, 3, "0.7.7", "0.7.6");
        assert_eq!(r["tried_by"], serde_json::json!(["0.7.6"]));
        let later = 1000.0 + 365.0 * 86400.0;
        let el = |f: &Fleet| eligibility_in(Some(&r), Some(900.0), later, 30.0, f);
        assert_eq!(el(&fleet("0.7.7", &["0.7.6"])), Eligibility::Parked);
        assert_eq!(el(&fleet("0.7.7", &["0.7.6", "0.7.8"])), Eligibility::Retry);
        assert!(machine_may_retry(Some(&r), "0.7.8", later, 30.0, "0.7.7"));
        assert!(!machine_may_retry(Some(&r), "0.7.6", later, 30.0, "0.7.7"));
        // The library's release changed: once more, by any machine.
        assert_eq!(el(&fleet("0.7.8", &["0.7.6"])), Eligibility::Retry);
        assert!(machine_may_retry(Some(&r), "0.7.6", later, 30.0, "0.7.8"));
        // A replaced archive, as before.
        let f = fleet("0.7.7", &["0.7.6"]);
        assert_eq!(
            eligibility_in(Some(&r), Some(5000.0), later, 30.0, &f),
            Eligibility::Replaced
        );
        // A record an older server wrote (no versions): tried once more.
        let mut old = r.clone();
        old.remove("library_version");
        old.remove("tried_by");
        assert_eq!(
            eligibility_in(Some(&old), Some(900.0), later, 30.0, &f),
            Eligibility::Retry
        );
        // A second failure by another release adds it, once.
        let mut both = r.clone();
        stamp_versions(&mut both, "0.7.7", "0.7.8");
        stamp_versions(&mut both, "0.7.7", "0.7.8");
        assert_eq!(both["tried_by"], serde_json::json!(["0.7.6", "0.7.8"]));
        let f2 = fleet("0.7.7", &["0.7.6", "0.7.8"]);
        assert_eq!(
            eligibility_in(Some(&both), Some(900.0), later, 30.0, &f2),
            Eligibility::Parked
        );
    }

    /// A machine's trouble keeps the hourly cap; an error nobody recognizes backs off
    /// further: an hour, then 6 h, a day, and a week from then on.
    #[test]
    fn machine_failures_keep_the_hourly_cap_and_unknown_ones_back_off_further() {
        assert_eq!(retry_delay_for(FailureKind::Machine, 30.0, 1), 30.0);
        assert_eq!(retry_delay_for(FailureKind::Machine, 30.0, 50), 3600.0);
        let other = |n| retry_delay_for(FailureKind::Other, 30.0, n);
        assert_eq!(other(1), 30.0);
        assert_eq!(other(6), 3600.0);
        assert_eq!(other(7), 6.0 * 3600.0);
        assert_eq!(other(8), 24.0 * 3600.0);
        assert_eq!(other(9), 7.0 * 86400.0);
        assert_eq!(other(500), 7.0 * 86400.0);
        // An unknown error 2 h after its 7th attempt still waits (6 h)…
        let r = failed("the models would not load", 1000.0, 7, "0.7.7", "0.7.6");
        let f = fleet("0.7.7", &["0.7.6"]);
        let el = |now: f64, f: &Fleet| eligibility_in(Some(&r), Some(900.0), now, 30.0, f);
        assert_eq!(el(1000.0 + 7200.0, &f), Eligibility::BackedOff);
        assert_eq!(el(1000.0 + 6.0 * 3600.0 + 1.0, &f), Eligibility::Retry);
        // …unless a release that has not tried it is here.
        assert_eq!(
            el(1000.0 + 7200.0, &fleet("0.7.7", &["0.7.6", "0.7.8"])),
            Eligibility::Retry
        );
        // A machine's trouble: the hourly cap, whoever is here.
        let m = failed("CUDA out of memory", 1000.0, 9, "0.7.7", "0.7.6");
        assert_eq!(
            eligibility_in(Some(&m), Some(900.0), 1000.0 + 3601.0, 30.0, &f),
            Eligibility::Retry
        );
    }

    #[test]
    fn the_log_line_says_when_it_comes_back() {
        assert!(
            failure_log_line("g", "S/V.cbz", 3, 30.0, "every page failed")
                .contains("not retried until the archive is replaced or a release changes")
        );
        assert!(
            failure_log_line("g", "S/V.cbz", 7, 30.0, "the models would not load")
                .contains("next retry in ~21600s")
        );
        assert!(
            failure_log_line("g", "S/V.cbz", 7, 30.0, "CUDA out of memory")
                .contains("next retry in ~3600s")
        );
    }

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
