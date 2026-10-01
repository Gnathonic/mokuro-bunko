//! Pipeline congestion: the per-stage busy / blocked / starved readout, the
//! one-line verdict naming what to widen, and the 5-run history per row
//! (`engine_runner.summarize` / `pipeline_verdict` / `widen_target`,
//! `ocr/congestion.py`).
//!
//! Summaries are JSON objects: they travel in runner events, progress cards
//! and `.ocr-congestion.json`, and the verdict reads whatever shape arrives
//! (raw counters, a summary, or averaged rows) as 0.5.2 did. The verdict
//! sentences (with their em dashes) are shown verbatim in the UI and logs.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::SchedError;
use crate::py::{
    Object, as_float, float_value, iso_utc, mean, number_or_zero, py_float, py_int, py_str,
    round_int, round_to, truthy,
};
use crate::pyjson::{dumps_indent2, read_object, write_atomic};

/// No verdict before this many pages have come out of the pipeline.
pub const MIN_PAGES: i64 = 8;
/// A stage waiting this share of its pool's time on a neighbour is a signal.
pub const WAIT_PCT: f64 = 15.0;
/// A stage this busy with nobody waiting on it is the pipeline's period.
pub const BUSY_PCT: f64 = 85.0;
/// Completed runs of one generation kept and averaged.
pub const RUNS_PER_GENERATION: usize = 5;
pub const CONGESTION_FILE: &str = ".ocr-congestion.json";

fn pct(seconds: Option<&Value>, pool_seconds: f64) -> f64 {
    if pool_seconds <= 0.0 {
        return 0.0;
    }
    round_to(
        100.0f64.min(0.0f64.max(100.0 * number_or_zero(seconds) / pool_seconds)),
        1,
    )
}

/// `summarize(raw)`: one row a stage plus the bottleneck and the verdict;
/// None unless `raw` is an object with a non-empty `stages` list (and at
/// least one usable stage).
pub fn summarize(raw: &Value) -> Option<Object> {
    let raw = raw.as_object()?;
    let stages_raw = raw.get("stages")?.as_array()?;
    if stages_raw.is_empty() {
        return None;
    }
    let elapsed = number_or_zero(raw.get("elapsed_seconds"));
    // A dict comprehension: a later duplicate name keeps the first position.
    let mut queues: Vec<(String, &Object)> = Vec::new();
    if let Some(Value::Array(items)) = raw.get("queues") {
        for q in items {
            let Some(q) = q.as_object() else { continue };
            let Some(name) = q.get("name").and_then(Value::as_str) else {
                continue;
            };
            match queues.iter_mut().find(|(n, _)| n == name) {
                Some(slot) => slot.1 = q,
                None => queues.push((name.to_owned(), q)),
            }
        }
    }
    let mut stages: Vec<Value> = Vec::new();
    for entry in stages_raw {
        if let Some(row) = stage_row(entry, &queues, elapsed) {
            stages.push(Value::Object(row));
        }
    }
    if stages.is_empty() {
        return None;
    }
    let bottleneck = match raw.get("bottleneck") {
        Some(Value::String(b))
            if stages
                .iter()
                .any(|s| s.get("key").and_then(Value::as_str) == Some(b)) =>
        {
            Value::String(b.clone())
        }
        _ => Value::Null,
    };
    let mut summary = Map::new();
    summary.insert("elapsed_seconds".into(), float_value(round_to(elapsed, 1)));
    summary.insert(
        "items".into(),
        Value::from(number_or_zero(raw.get("items")).trunc() as i64),
    );
    summary.insert("stages".into(), Value::Array(stages));
    summary.insert("bottleneck".into(), bottleneck);
    let verdict = pipeline_verdict(&summary);
    summary.insert("verdict".into(), verdict.map_or(Value::Null, Value::String));
    Some(summary)
}

fn stage_row(entry: &Value, queues: &[(String, &Object)], elapsed: f64) -> Option<Object> {
    let entry = entry.as_object()?;
    let key = entry
        .get("key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())?;
    let workers = number_or_zero(entry.get("workers")).trunc() as i64;
    let fused = workers <= 0;
    let pool_seconds = workers.max(1) as f64 * elapsed;
    let prefix = format!("{key}->");
    let outbound = queues
        .iter()
        .find(|(n, _)| n.starts_with(&prefix))
        .map(|(_, q)| *q);
    let mut row = Map::new();
    row.insert("key".into(), Value::from(key));
    row.insert(
        "name".into(),
        Value::from(entry.get("name").and_then(Value::as_str).unwrap_or(key)),
    );
    row.insert(
        "device".into(),
        Value::from(entry.get("device").and_then(Value::as_str).unwrap_or("cpu")),
    );
    row.insert("workers".into(), Value::from(workers));
    row.insert("fused".into(), Value::Bool(fused));
    row.insert(
        "device_bound".into(),
        Value::Bool(truthy(entry.get("device_bound"))),
    );
    row.insert(
        "items".into(),
        Value::from(number_or_zero(entry.get("items")).trunc() as i64),
    );
    row.insert(
        "busy_pct".into(),
        float_value(pct(entry.get("busy_seconds"), pool_seconds)),
    );
    row.insert(
        "blocked_pct".into(),
        if fused {
            Value::Null
        } else {
            float_value(pct(entry.get("blocked_seconds"), pool_seconds))
        },
    );
    row.insert(
        "starved_pct".into(),
        if fused {
            Value::Null
        } else {
            float_value(pct(entry.get("starved_seconds"), pool_seconds))
        },
    );
    row.insert("queue".into(), outbound.map_or(Value::Null, queue_row));
    Some(row)
}

fn queue_row(queue: &Object) -> Value {
    let capacity = number_or_zero(queue.get("capacity")).trunc() as i64;
    let mean_depth = number_or_zero(queue.get("mean_depth"));
    let mut row = Map::new();
    row.insert(
        "name".into(),
        Value::from(match queue.get("name") {
            None => String::new(),
            v => py_str(v),
        }),
    );
    row.insert("capacity".into(), Value::from(capacity));
    row.insert("mean_depth".into(), float_value(round_to(mean_depth, 2)));
    row.insert(
        "max_depth".into(),
        Value::from(number_or_zero(queue.get("max_depth")).trunc() as i64),
    );
    row.insert(
        "fill_pct".into(),
        float_value(if capacity > 0 {
            round_to(100.0 * mean_depth / capacity as f64, 1)
        } else {
            0.0
        }),
    );
    Value::Object(row)
}

/// `pipeline_verdict(summary)`: one sentence naming one thing to do, or None.
pub fn pipeline_verdict(summary: &Object) -> Option<String> {
    read_stages(summary).map(|(sentence, _)| sentence)
}

/// `widen_target(summary)`: the stage key the verdict says to widen (None
/// also when the verdict is a fact with no knob).
pub fn widen_target(summary: &Object) -> Option<String> {
    read_stages(summary).and_then(|(_, target)| target)
}

struct Segment {
    key: String,
    device: String,
    workers: i64,
    items: i64,
    busy_pct: f64,
    blocked_pct: f64,
    starved_pct: f64,
    device_bound: bool,
}

fn get_or<'a>(stage: &'a Object, key: &str) -> Option<&'a Value> {
    stage.get(key)
}

/// `str(stage.get(key, default))`.
fn str_or(stage: &Object, key: &str, default: &str) -> String {
    match stage.get(key) {
        None => default.to_owned(),
        v => py_str(v),
    }
}

fn segments(stages: &[Object]) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for stage in stages {
        if truthy(stage.get("fused"))
            && let Some(leader) = out.last_mut()
        {
            leader.key = format!("{}+{}", leader.key, str_or(stage, "key", ""));
            leader.busy_pct =
                100.0f64.min(leader.busy_pct + number_or_zero(get_or(stage, "busy_pct")));
            leader.device_bound = leader.device_bound || truthy(stage.get("device_bound"));
            leader.items = leader
                .items
                .max(number_or_zero(stage.get("items")).trunc() as i64);
            continue;
        }
        out.push(Segment {
            key: str_or(stage, "key", ""),
            device: str_or(stage, "device", "cpu"),
            workers: number_or_zero(stage.get("workers")).trunc() as i64,
            items: number_or_zero(stage.get("items")).trunc() as i64,
            busy_pct: number_or_zero(stage.get("busy_pct")),
            blocked_pct: number_or_zero(stage.get("blocked_pct")),
            starved_pct: number_or_zero(stage.get("starved_pct")),
            device_bound: truthy(stage.get("device_bound")),
        });
    }
    out
}

fn read_stages(summary: &Object) -> Option<(String, Option<String>)> {
    let empty = Map::new();
    let stages: Vec<&Object> = match summary.get("stages") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|s| s.as_object().unwrap_or(&empty))
            .collect(),
        _ => Vec::new(),
    };
    if !stages.iter().any(|s| truthy(s.get("queue"))) {
        return None;
    }
    let owned: Vec<Object> = stages.into_iter().cloned().collect();
    let segs = segments(&owned);
    if segs.is_empty() || segs.iter().map(|s| s.items).min().unwrap_or(0) < MIN_PAGES {
        return None;
    }
    let candidates: Vec<(f64, String, String)> =
        [starved_candidate(&segs), blocked_candidate(&segs)]
            .into_iter()
            .flatten()
            .collect();
    if let Some(first) = candidates.first() {
        let mut best = first;
        for c in &candidates[1..] {
            if c.0 > best.0 {
                best = c;
            }
        }
        return Some((best.1.clone(), Some(best.2.clone())));
    }
    let mut busiest = &segs[0];
    for s in &segs[1..] {
        if s.busy_pct > busiest.busy_pct {
            busiest = s;
        }
    }
    if busiest.busy_pct < BUSY_PCT {
        return None;
    }
    let width = if busiest.workers != 0 {
        busiest.workers
    } else {
        1
    };
    let busy = crate::py::fmt_fixed(busiest.busy_pct, 0);
    if busiest.device_bound {
        return Some((
            format!(
                "{} busy {}% on the {} — it sets the pace and cannot be widened",
                busiest.key, busy, busiest.device
            ),
            None,
        ));
    }
    Some((
        format!(
            "{} busy {}% of {} worker{} — widen {}",
            busiest.key,
            busy,
            width,
            if width != 1 { "s" } else { "" },
            busiest.key
        ),
        Some(busiest.key.clone()),
    ))
}

fn starved_candidate(segs: &[Segment]) -> Option<(f64, String, String)> {
    for index in 1..segs.len() {
        let (stage, feeder) = (&segs[index], &segs[index - 1]);
        let starved = stage.starved_pct;
        if starved < WAIT_PCT || feeder.starved_pct >= WAIT_PCT || feeder.device_bound {
            continue;
        }
        return Some((
            starved,
            format!(
                "{} starved {}% waiting on {} — widen {}",
                stage.key,
                crate::py::fmt_fixed(starved, 0),
                feeder.key,
                feeder.key
            ),
            feeder.key.clone(),
        ));
    }
    None
}

fn blocked_candidate(segs: &[Segment]) -> Option<(f64, String, String)> {
    if segs.len() < 2 {
        return None;
    }
    for index in (0..segs.len() - 1).rev() {
        let (stage, drain) = (&segs[index], &segs[index + 1]);
        let blocked = stage.blocked_pct;
        if blocked < WAIT_PCT || drain.blocked_pct >= WAIT_PCT || drain.device_bound {
            continue;
        }
        return Some((
            blocked,
            format!(
                "{} blocked {}% waiting on {} — widen {}",
                stage.key,
                crate::py::fmt_fixed(blocked, 0),
                drain.key,
                drain.key
            ),
            drain.key.clone(),
        ));
    }
    None
}

/// `summarize_event_stats(stats)`: a session's `stats` / `volume_done`
/// numbers as a summary — raw counters are summarized, an already
/// summarized object is used as is (verdict re-derived when missing).
pub fn summarize_event_stats(stats: &Value) -> Option<Object> {
    let obj = stats.as_object()?;
    let stages = obj.get("stages")?.as_array()?;
    if stages.is_empty() {
        return None;
    }
    let rows: Vec<&Object> = stages.iter().filter_map(Value::as_object).collect();
    if rows.iter().any(|r| r.contains_key("busy_seconds"))
        || !rows.iter().any(|r| r.contains_key("busy_pct"))
    {
        return summarize(stats);
    }
    let mut summary = obj.clone();
    summary
        .entry("elapsed_seconds")
        .or_insert_with(|| float_value(0.0));
    summary.entry("items").or_insert_with(|| Value::from(0));
    if summary.get("verdict").is_none_or(Value::is_null) {
        let verdict = pipeline_verdict(&summary);
        summary.insert("verdict".into(), verdict.map_or(Value::Null, Value::String));
    }
    Some(summary)
}

/// The per-volume extras of a stored run.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VolumeTiming {
    pub volume_pages: Option<i64>,
    pub volume_seconds: Option<f64>,
    /// The volume its session opened with (its time carries the fill).
    pub volume_first: bool,
}

/// `build_record(summary, volume=, at=, volume_pages=, volume_seconds=,
/// volume_first=)`: one stored run.
pub fn build_record(summary: &Object, volume: &str, at: f64, timing: VolumeTiming) -> Object {
    let mut stages = Vec::new();
    let mut queues = Vec::new();
    if let Some(Value::Array(items)) = summary.get("stages") {
        for stage in items {
            let Some(stage) = stage.as_object() else {
                continue;
            };
            let int_or_zero = |k: &str| {
                if truthy(stage.get(k)) {
                    py_int(stage.get(k)).unwrap_or(0)
                } else {
                    0
                }
            };
            let mut row = Map::new();
            row.insert(
                "key".into(),
                stage.get("key").cloned().unwrap_or(Value::Null),
            );
            row.insert(
                "name".into(),
                stage.get("name").cloned().unwrap_or(Value::Null),
            );
            row.insert(
                "device".into(),
                stage.get("device").cloned().unwrap_or(Value::Null),
            );
            row.insert("workers".into(), Value::from(int_or_zero("workers")));
            row.insert("fused".into(), Value::Bool(truthy(stage.get("fused"))));
            row.insert("items".into(), Value::from(int_or_zero("items")));
            row.insert(
                "busy_pct".into(),
                stage.get("busy_pct").cloned().unwrap_or(Value::Null),
            );
            row.insert(
                "starved_pct".into(),
                stage.get("starved_pct").cloned().unwrap_or(Value::Null),
            );
            row.insert(
                "blocked_pct".into(),
                stage.get("blocked_pct").cloned().unwrap_or(Value::Null),
            );
            stages.push(Value::Object(row));
            if let Some(Value::Object(queue)) = stage.get("queue")
                && truthy(queue.get("name"))
            {
                let qi = |k: &str| {
                    if truthy(queue.get(k)) {
                        py_int(queue.get(k)).unwrap_or(0)
                    } else {
                        0
                    }
                };
                let mut q = Map::new();
                q.insert(
                    "name".into(),
                    queue.get("name").cloned().unwrap_or(Value::Null),
                );
                q.insert("capacity".into(), Value::from(qi("capacity")));
                q.insert(
                    "mean_depth".into(),
                    float_value(if truthy(queue.get("mean_depth")) {
                        py_float(queue.get("mean_depth")).unwrap_or(0.0)
                    } else {
                        0.0
                    }),
                );
                q.insert("max_depth".into(), Value::from(qi("max_depth")));
                queues.push(Value::Object(q));
            }
        }
    }
    let mut record = Map::new();
    record.insert("at".into(), float_value(at));
    record.insert("volume".into(), Value::from(volume));
    record.insert(
        "pages".into(),
        Value::from(if truthy(summary.get("items")) {
            py_int(summary.get("items")).unwrap_or(0)
        } else {
            0
        }),
    );
    record.insert(
        "elapsed".into(),
        float_value(if truthy(summary.get("elapsed_seconds")) {
            py_float(summary.get("elapsed_seconds")).unwrap_or(0.0)
        } else {
            0.0
        }),
    );
    record.insert(
        "verdict".into(),
        summary.get("verdict").cloned().unwrap_or(Value::Null),
    );
    record.insert(
        "bottleneck".into(),
        summary.get("bottleneck").cloned().unwrap_or(Value::Null),
    );
    record.insert("stages".into(), Value::Array(stages));
    record.insert("queues".into(), Value::Array(queues));
    if let (Some(pages), Some(seconds)) = (timing.volume_pages, timing.volume_seconds)
        && pages > 0
        && seconds != 0.0
        && seconds > 0.0
    {
        record.insert("volume_pages".into(), Value::from(pages));
        record.insert("volume_seconds".into(), float_value(seconds));
        if timing.volume_first {
            record.insert("volume_first".into(), Value::Bool(true));
        }
    }
    record
}

fn collect(bucket: &mut Vec<f64>, value: Option<&Value>) {
    if let Some(v) = as_float(value) {
        bucket.push(v);
    }
}

/// `average_runs(runs)`: the `congestion` object the admin API returns
/// (`runs`, `last_run_at`, `verdict`, `bottleneck`, `stages`, `queues`), or
/// None when no run has stages.
pub fn average_runs(runs: &[Value]) -> Option<Object> {
    let usable: Vec<&Object> = runs
        .iter()
        .filter_map(Value::as_object)
        .filter(|r| truthy(r.get("stages")))
        .collect();
    if usable.is_empty() {
        return None;
    }
    struct Acc {
        busy: Vec<f64>,
        starved: Vec<f64>,
        blocked: Vec<f64>,
        workers: Vec<f64>,
        items: Vec<f64>,
        name: Value,
        device: Value,
        fused: bool,
    }
    let mut order: Vec<(String, Acc)> = Vec::new();
    for run in &usable {
        let Some(Value::Array(stages)) = run.get("stages") else {
            continue;
        };
        for stage in stages {
            let Some(stage) = stage.as_object() else {
                continue;
            };
            let Some(key) = stage
                .get("key")
                .and_then(Value::as_str)
                .filter(|k| !k.is_empty())
            else {
                continue;
            };
            let idx = match order.iter().position(|(k, _)| k == key) {
                Some(i) => i,
                None => {
                    order.push((
                        key.to_owned(),
                        Acc {
                            busy: vec![],
                            starved: vec![],
                            blocked: vec![],
                            workers: vec![],
                            items: vec![],
                            name: Value::Null,
                            device: Value::Null,
                            fused: false,
                        },
                    ));
                    order.len() - 1
                }
            };
            let acc = &mut order[idx].1;
            acc.name = if truthy(stage.get("name")) {
                stage["name"].clone()
            } else {
                Value::from(key)
            };
            acc.device = if truthy(stage.get("device")) {
                stage["device"].clone()
            } else {
                Value::from("cpu")
            };
            acc.fused = truthy(stage.get("fused"));
            collect(&mut acc.busy, stage.get("busy_pct"));
            collect(&mut acc.starved, stage.get("starved_pct"));
            collect(&mut acc.blocked, stage.get("blocked_pct"));
            collect(&mut acc.workers, stage.get("workers"));
            collect(&mut acc.items, stage.get("items"));
        }
    }

    // (name, capacities, mean depths, max depths), in first-seen order.
    type QueueAcc = (String, Vec<f64>, Vec<f64>, Vec<f64>);
    let mut queue_order: Vec<QueueAcc> = Vec::new();
    for run in &usable {
        let Some(Value::Array(qs)) = run.get("queues") else {
            continue;
        };
        for queue in qs {
            let Some(queue) = queue.as_object() else {
                continue;
            };
            let Some(name) = queue
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
            else {
                continue;
            };
            let idx = match queue_order.iter().position(|q| q.0 == name) {
                Some(i) => i,
                None => {
                    queue_order.push((name.to_owned(), vec![], vec![], vec![]));
                    queue_order.len() - 1
                }
            };
            let q = &mut queue_order[idx];
            collect(&mut q.1, queue.get("capacity"));
            collect(&mut q.2, queue.get("mean_depth"));
            collect(&mut q.3, queue.get("max_depth"));
        }
    }
    let avg_round = |v: &[f64]| round_int(mean(v));
    let queues: Vec<Object> = queue_order
        .iter()
        .map(|(name, cap, mean_depth, max_depth)| {
            let mut q = Map::new();
            q.insert("name".into(), Value::from(name.as_str()));
            q.insert("capacity".into(), Value::from(avg_round(cap)));
            q.insert(
                "mean_depth".into(),
                float_value(round_to(mean(mean_depth), 2)),
            );
            q.insert("max_depth".into(), Value::from(avg_round(max_depth)));
            q
        })
        .collect();

    let mut stages = Vec::new();
    let mut verdict_rows: Vec<Object> = Vec::new();
    for (key, acc) in &order {
        let mut row = Map::new();
        row.insert("key".into(), Value::from(key.as_str()));
        row.insert("workers".into(), Value::from(avg_round(&acc.workers)));
        row.insert("busy_pct".into(), Value::from(avg_round(&acc.busy)));
        row.insert("starved_pct".into(), Value::from(avg_round(&acc.starved)));
        row.insert("blocked_pct".into(), Value::from(avg_round(&acc.blocked)));
        stages.push(Value::Object(row.clone()));
        let mut vrow = row;
        vrow.insert("name".into(), acc.name.clone());
        vrow.insert("device".into(), acc.device.clone());
        vrow.insert("fused".into(), Value::Bool(acc.fused));
        vrow.insert("items".into(), Value::from(avg_round(&acc.items)));
        let prefix = format!("{key}->");
        vrow.insert(
            "queue".into(),
            queues
                .iter()
                .find(|q| {
                    q.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|n| n.starts_with(&prefix))
                })
                .map_or(Value::Null, |q| Value::Object(q.clone())),
        );
        verdict_rows.push(vrow);
    }

    let mut bottleneck: Option<&Object> = None;
    for row in &verdict_rows {
        if truthy(row.get("fused")) || !truthy(row.get("items")) {
            continue;
        }
        let busy = |r: &Object| r.get("busy_pct").and_then(Value::as_i64).unwrap_or(0);
        if bottleneck.is_none_or(|b| busy(row) > busy(b)) {
            bottleneck = Some(row);
        }
    }
    let bottleneck_key = bottleneck
        .and_then(|b| b.get("key").cloned())
        .unwrap_or(Value::Null);
    let mut last = f64::NEG_INFINITY;
    for run in &usable {
        let at = if truthy(run.get("at")) {
            py_float(run.get("at")).unwrap_or(0.0)
        } else {
            0.0
        };
        if at > last {
            last = at;
        }
    }
    let elapsed: Vec<f64> = usable
        .iter()
        .map(|r| {
            if truthy(r.get("elapsed")) {
                py_float(r.get("elapsed")).unwrap_or(0.0)
            } else {
                0.0
            }
        })
        .collect();
    let pages: Vec<f64> = usable
        .iter()
        .map(|r| {
            if truthy(r.get("pages")) {
                py_float(r.get("pages")).unwrap_or(0.0)
            } else {
                0.0
            }
        })
        .collect();
    let mut verdict_summary = Map::new();
    verdict_summary.insert("elapsed_seconds".into(), float_value(mean(&elapsed)));
    verdict_summary.insert("items".into(), Value::from(avg_round(&pages)));
    verdict_summary.insert(
        "stages".into(),
        Value::Array(verdict_rows.iter().cloned().map(Value::Object).collect()),
    );
    verdict_summary.insert("bottleneck".into(), bottleneck_key.clone());

    let mut out = Map::new();
    out.insert("runs".into(), Value::from(usable.len()));
    out.insert(
        "last_run_at".into(),
        if last > 0.0 {
            Value::String(iso_utc(last))
        } else {
            Value::Null
        },
    );
    out.insert(
        "verdict".into(),
        pipeline_verdict(&verdict_summary).map_or(Value::Null, Value::String),
    );
    out.insert("bottleneck".into(), bottleneck_key);
    out.insert("stages".into(), Value::Array(stages));
    out.insert(
        "queues".into(),
        Value::Array(queues.into_iter().map(Value::Object).collect()),
    );
    Some(out)
}

/// The `.ocr-congestion.json` file: `{gen_id: [record, … ≤ keep]}`.
#[derive(Clone, Debug)]
pub struct CongestionHistory {
    pub path: PathBuf,
    pub keep: usize,
}

/// Every recorded run by generation id, file order.
pub type History = Vec<(String, Vec<Object>)>;

impl CongestionHistory {
    pub fn new(storage: &Path) -> Self {
        CongestionHistory {
            path: storage.join(CONGESTION_FILE),
            keep: RUNS_PER_GENERATION,
        }
    }

    pub fn with_keep(storage: &Path, keep: usize) -> Self {
        CongestionHistory {
            path: storage.join(CONGESTION_FILE),
            keep: keep.max(1),
        }
    }

    /// Every recorded run by generation id (empty when there are none).
    pub fn load(&self) -> History {
        let Some(data) = read_object(&self.path) else {
            return Vec::new();
        };
        data.into_iter()
            .filter_map(|(k, v)| match v {
                Value::Array(runs) => Some((
                    k,
                    runs.into_iter()
                        .filter_map(|r| match r {
                            Value::Object(o) => Some(o),
                            _ => None,
                        })
                        .collect(),
                )),
                _ => None,
            })
            .collect()
    }

    /// Persist atomically (indent 2, as 0.5.2); delete the file when empty.
    pub fn save(&self, history: &History) -> Result<(), SchedError> {
        let trimmed: Map<String, Value> = history
            .iter()
            .filter(|(_, runs)| !runs.is_empty())
            .map(|(k, runs)| {
                (
                    k.clone(),
                    Value::Array(runs.iter().cloned().map(Value::Object).collect()),
                )
            })
            .collect();
        if trimmed.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(SchedError::Io {
                    path: self.path.clone(),
                    source: e,
                }),
            }
        } else {
            write_atomic(&self.path, &dumps_indent2(&Value::Object(trimmed))).map_err(|e| {
                SchedError::Io {
                    path: self.path.clone(),
                    source: e,
                }
            })
        }
    }

    /// Append one completed run (capped to `keep`) and, with `known_ids`,
    /// drop every other row's history in the same write.
    pub fn record(
        &self,
        generation_id: &str,
        record: Object,
        known_ids: Option<&[String]>,
    ) -> Result<(), SchedError> {
        let mut history = self.load();
        match history.iter_mut().find(|(k, _)| k == generation_id) {
            Some((_, runs)) => {
                runs.push(record);
                let drop = runs.len().saturating_sub(self.keep);
                runs.drain(..drop);
            }
            None => history.push((generation_id.to_owned(), vec![record])),
        }
        if let Some(known) = known_ids {
            history.retain(|(k, _)| k == generation_id || known.iter().any(|id| id == k));
        }
        self.save(&history)
    }

    /// Drop the history of generations that no longer exist.
    pub fn prune(&self, known_ids: &[String]) -> Result<(), SchedError> {
        let history = self.load();
        let before = history.len();
        let pruned: History = history
            .into_iter()
            .filter(|(k, _)| known_ids.iter().any(|id| id == k))
            .collect();
        if pruned.len() != before {
            self.save(&pruned)
        } else {
            Ok(())
        }
    }

    /// The averaged congestion of one generation, or None with no runs.
    pub fn summary(&self, generation_id: &str) -> Option<Object> {
        let history = self.load();
        let runs = history
            .into_iter()
            .find(|(k, _)| k == generation_id)
            .map(|(_, r)| r)
            .unwrap_or_default();
        average_runs(&runs.into_iter().map(Value::Object).collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn starved_verdict() {
        let raw = json!({
            "elapsed_seconds": 10.0, "items": 40,
            "stages": [
                {"key": "detect", "workers": 1, "items": 40, "busy_seconds": 9.5, "starved_seconds": 0.0, "blocked_seconds": 0.0},
                {"key": "engine", "workers": 1, "items": 40, "busy_seconds": 5.0, "starved_seconds": 4.0, "blocked_seconds": 0.0}
            ],
            "queues": [{"name": "detect->engine", "capacity": 4, "mean_depth": 0.2, "max_depth": 2}]
        });
        let s = summarize(&raw).unwrap();
        assert_eq!(
            s["verdict"],
            json!("engine starved 40% waiting on detect — widen detect")
        );
        assert_eq!(widen_target(&s).as_deref(), Some("detect"));
    }
}
