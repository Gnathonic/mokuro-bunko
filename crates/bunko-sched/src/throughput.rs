//! Real throughput: pages completed over the wall seconds they took
//! (`ocr/throughput.py`). Display only — never used to predict.
//!
//! Where each volume's end is known, the seconds are the wall clock the
//! volumes' windows (`end - seconds` to `end`) cover together, not their sum:
//! a pipelined session overlaps volumes, and summed seconds count it twice.

use serde_json::{Value, json};

use crate::py::{number_not_nan, round_to};

/// How many of a machine's most recent finished volumes are averaged.
pub const RECENT_VOLUMES: usize = 20;

/// Pages over seconds of `volumes` finished volumes, newest at `last_at`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Throughput {
    pub pages: f64,
    pub seconds: f64,
    pub volumes: i64,
    pub last_at: Option<f64>,
}

impl Throughput {
    pub fn pages_per_second(&self) -> f64 {
        self.pages / self.seconds
    }

    pub fn pages_per_minute(&self) -> f64 {
        self.pages_per_second() * 60.0
    }

    /// `{pages_per_minute (1dp), volumes, last_at}`.
    pub fn as_dict(&self) -> Value {
        json!({
            "pages_per_minute": round_to(self.pages_per_minute(), 1),
            "volumes": self.volumes,
            "last_at": self.last_at,
        })
    }
}

/// One finished volume: `(pages, seconds)` or `(pages, seconds, end)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub pages: Option<f64>,
    pub seconds: Option<f64>,
    pub end: Option<f64>,
}

impl Sample {
    pub fn new(pages: f64, seconds: f64) -> Self {
        Sample {
            pages: Some(pages),
            seconds: Some(seconds),
            end: None,
        }
    }

    /// From JSON values, with `_number`'s rules (bools, NaN, non-numbers → None).
    pub fn from_values(
        pages: Option<&Value>,
        seconds: Option<&Value>,
        end: Option<&Value>,
    ) -> Self {
        Sample {
            pages: number_not_nan(pages),
            seconds: number_not_nan(seconds),
            end: number_not_nan(end),
        }
    }
}

/// How much of the clock the `(start, end)` windows cover together.
pub fn covered_seconds(windows: &[(f64, f64)]) -> f64 {
    let mut sorted = windows.to_vec();
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let mut total = 0.0;
    let mut span: Option<(f64, f64)> = None;
    for (start, end) in sorted {
        if let Some((s0, s1)) = span {
            if start <= s1 {
                span = Some((s0, s1.max(end)));
                continue;
            }
            total += s1 - s0;
        }
        span = Some((start, end));
    }
    if let Some((s0, s1)) = span {
        total += s1 - s0;
    }
    total
}

/// `throughput_of(samples, last_at=…)`: `sum(pages)` over the wall seconds.
pub fn throughput_of(samples: &[Sample], last_at: Option<f64>) -> Option<Throughput> {
    let mut pages = 0.0;
    let mut loose = 0.0;
    let mut windows = Vec::new();
    let mut volumes = 0i64;
    for sample in samples {
        let (Some(p), Some(s)) = (sample.pages, sample.seconds) else {
            continue;
        };
        if p <= 0.0 || s <= 0.0 {
            continue;
        }
        pages += p;
        volumes += 1;
        match sample.end {
            None => loose += s,
            Some(end) => windows.push((end - s, end)),
        }
    }
    let seconds = loose + covered_seconds(&windows);
    if volumes == 0 || seconds <= 0.0 {
        return None;
    }
    Some(Throughput {
        pages,
        seconds,
        volumes,
        last_at,
    })
}

fn max_opt(values: impl Iterator<Item = Option<f64>>, default: Option<f64>) -> Option<f64> {
    let mut best: Option<f64> = None;
    for v in values.flatten() {
        // Python's max(): the first of equal values, a later one only if greater.
        if best.is_none_or(|b| v > b) {
            best = Some(v);
        }
    }
    best.or(default)
}

/// `records_throughput(records, limit)`: stored congestion records that carry
/// `volume_pages` / `volume_seconds`, newest `limit`, placed by their `at`.
pub fn records_throughput(records: &[Value], limit: usize) -> Option<Throughput> {
    let usable: Vec<&serde_json::Map<String, Value>> = records
        .iter()
        .filter_map(Value::as_object)
        .filter(|run| {
            number_not_nan(run.get("volume_pages")).is_some_and(|v| v != 0.0)
                && number_not_nan(run.get("volume_seconds")).is_some_and(|v| v != 0.0)
        })
        .collect();
    let usable = &usable[usable.len().saturating_sub(limit)..];
    let last = max_opt(usable.iter().map(|run| number_not_nan(run.get("at"))), None);
    let samples: Vec<Sample> = usable
        .iter()
        .map(|run| {
            Sample::from_values(
                run.get("volume_pages"),
                run.get("volume_seconds"),
                run.get("at"),
            )
        })
        .collect();
    throughput_of(&samples, last)
}

/// `profile_throughput(runs)`: a processor profile's `runs` entry — its
/// `recent` volumes, else its congestion records' pairs, else the cumulative
/// pages / seconds.
pub fn profile_throughput(runs: Option<&Value>) -> Option<Throughput> {
    let runs = runs?.as_object()?;
    let last_at = number_not_nan(runs.get("last_at"));
    if let Some(Value::Array(recent)) = runs.get("recent") {
        let rows: Vec<&serde_json::Map<String, Value>> =
            recent.iter().filter_map(Value::as_object).collect();
        let rows = &rows[rows.len().saturating_sub(RECENT_VOLUMES)..];
        let last = max_opt(rows.iter().map(|r| number_not_nan(r.get("at"))), last_at);
        let samples: Vec<Sample> = rows
            .iter()
            .map(|r| Sample::from_values(r.get("pages"), r.get("seconds"), r.get("at")))
            .collect();
        if let Some(found) = throughput_of(&samples, last) {
            return Some(found);
        }
    }
    let congestion = match runs.get("congestion") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    if let Some(found) = records_throughput(&congestion, RECENT_VOLUMES) {
        let last = match last_at {
            Some(v) if v != 0.0 => Some(v),
            _ => found.last_at,
        };
        return Some(Throughput {
            last_at: last,
            ..found
        });
    }
    let pages = number_not_nan(runs.get("pages"));
    let seconds = number_not_nan(runs.get("seconds"));
    let volumes = number_not_nan(runs.get("volumes"));
    match (pages, seconds) {
        (Some(p), Some(s)) if p > 0.0 && s > 0.0 => {
            let v = match volumes {
                Some(v) if v != 0.0 => v.trunc() as i64,
                _ => 1,
            };
            Some(Throughput {
                pages: p,
                seconds: s,
                volumes: v,
                last_at,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_windows_count_once() {
        let samples = [
            Sample {
                pages: Some(10.0),
                seconds: Some(10.0),
                end: Some(110.0),
            },
            Sample {
                pages: Some(10.0),
                seconds: Some(10.0),
                end: Some(105.0),
            },
            Sample::new(5.0, 5.0),
        ];
        let t = throughput_of(&samples, None).unwrap();
        assert_eq!(t.pages, 25.0);
        assert_eq!(t.seconds, 15.0 + 5.0);
        assert_eq!(t.volumes, 3);
    }
}
