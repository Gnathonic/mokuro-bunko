//! What OCR one volume is still owed, and when to look again
//! (`ocr/volume_outlook.py`) — the manifest's `pending` / `recheck_after`
//! and the PUT's `X-Mokuro-Recheck-After`. Nothing is predicted here: every
//! time is the queue plan's own.

use serde_json::{Value, json};

use crate::py::{Object, parse_iso_timestamp};

/// Seconds added to the earliest finishing time.
pub const RECHECK_MARGIN_SECONDS: i64 = 10;
pub const RECHECK_MIN_SECONDS: i64 = 30;
pub const RECHECK_MAX_SECONDS: i64 = 3600;
/// Something is pending but nothing is priced: look again in 5 min.
pub const RECHECK_UNPRICED_SECONDS: i64 = 300;

/// The parts of a generation row an outlook needs.
#[derive(Clone, Debug, PartialEq)]
pub struct OwedRow {
    pub id: String,
    pub name: String,
    pub primary: bool,
}

/// `pending_entries(owed, series, volume, planned)`: `[{"kind", "id", "eta"}]`
/// for each owed row in order; `eta` = the first non-null `eta_at` the plan
/// gives that (series, volume, generation id), else null.
pub fn pending_entries(
    owed: &[OwedRow],
    series: &str,
    volume: &str,
    planned: &[Object],
) -> Vec<Value> {
    let mut etas: Vec<(String, Option<String>)> = Vec::new();
    for entry in planned {
        if entry.get("series").and_then(Value::as_str) != Some(series)
            || entry.get("volume").and_then(Value::as_str) != Some(volume)
        {
            continue;
        }
        let Some(generation_id) = entry.get("generation_id").and_then(Value::as_str) else {
            continue;
        };
        let eta = entry
            .get("eta_at")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match etas.iter_mut().find(|(g, _)| g == generation_id) {
            Some((_, existing)) if existing.is_none() => *existing = eta,
            Some(_) => {}
            None => etas.push((generation_id.to_owned(), eta)),
        }
    }
    owed.iter()
        .map(|row| {
            let eta = etas
                .iter()
                .find(|(g, _)| *g == row.id)
                .and_then(|(_, e)| e.clone());
            json!({"kind": if row.primary { "ocr" } else { "layer" }, "id": row.name, "eta": eta})
        })
        .collect()
}

/// `recheck_after(pending, now)`: whole seconds until the reader should ask
/// again — the earliest parsable `eta` + 10 s, clamped to [30, 3600]; 300
/// when nothing is priced; None when nothing is pending.
pub fn recheck_after(pending: &[Value], now: f64) -> Option<i64> {
    if pending.is_empty() {
        return None;
    }
    let due: Vec<f64> = pending
        .iter()
        .filter_map(|e| e.get("eta").and_then(Value::as_str))
        .filter_map(parse_iso_timestamp)
        .collect();
    if due.is_empty() {
        return Some(RECHECK_UNPRICED_SECONDS);
    }
    let mut earliest = due[0];
    for d in &due[1..] {
        if *d < earliest {
            earliest = *d;
        }
    }
    let seconds = (earliest - now).ceil() as i64 + RECHECK_MARGIN_SECONDS;
    Some(RECHECK_MIN_SECONDS.max(RECHECK_MAX_SECONDS.min(seconds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recheck_rules() {
        assert_eq!(recheck_after(&[], 0.0), None);
        assert_eq!(recheck_after(&[json!({"eta": null})], 0.0), Some(300));
        assert_eq!(
            recheck_after(&[json!({"eta": "1970-01-01T00:01:40Z"})], 0.0),
            Some(110)
        );
        assert_eq!(
            recheck_after(&[json!({"eta": "1970-01-01T00:00:01Z"})], 0.0),
            Some(30)
        );
        assert_eq!(
            recheck_after(&[json!({"eta": "1971-01-01T00:00:00Z"})], 0.0),
            Some(3600)
        );
    }
}
