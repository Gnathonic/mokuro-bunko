//! What people are shown about speed: the busy-host judgement of a volume
//! (`_busy_reason`) and the raw status `speed` list (`speed_report`).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::py::{Object, as_float, fmt_percent0, round_to};
use crate::rate::{LOCAL_MACHINE, RateModel, rate_key};

/// PSI "some" CPU pressure at or above which a volume ran on a busy host.
pub const CONTENDED_CPU_PRESSURE: f64 = 0.6;
/// Share of the host CPU other processes used at or above which it did too.
pub const CONTENDED_OTHER_CPU: f64 = 0.5;
/// The window of evidence `speed_report` looks at by default.
pub const SPEED_WINDOW_SECONDS: f64 = 6.0 * 3600.0;

/// `_busy_reason(event)`: why a volume (or the live window) ran on a busy
/// host — its speed is then not learned — or None.
pub fn busy_reason(event: &Object) -> Option<String> {
    if let Some(pressure) = as_float(event.get("cpu_pressure"))
        && pressure >= CONTENDED_CPU_PRESSURE
    {
        return Some(format!("CPU pressure {}", fmt_percent0(pressure)));
    }
    if let Some(others) = as_float(event.get("other_cpu"))
        && others >= CONTENDED_OTHER_CPU
    {
        return Some(format!(
            "other processes used {} of the CPU",
            fmt_percent0(others)
        ));
    }
    None
}

/// An enabled row, in queue order.
#[derive(Clone, Debug, PartialEq)]
pub struct RowRef {
    pub id: String,
    pub name: String,
}

/// `speed_report(running, within)`: real pages per minute per enabled row,
/// per machine and combined over the lanes reading it now.
pub fn speed_report(
    rows: &[RowRef],
    running: &[Object],
    rates: &RateModel,
    within: f64,
) -> Vec<Value> {
    let mut lane_order: Vec<(String, String)> = Vec::new();
    let mut lanes: HashMap<(String, String), i64> = HashMap::new();
    for job in running {
        let Some(gen_id) = job.get("generation_id").and_then(Value::as_str) else {
            continue;
        };
        if job.get("status").and_then(Value::as_str) == Some("starting") {
            continue;
        }
        let machine = match job.get("machine").and_then(Value::as_str) {
            Some(m) if !m.is_empty() => m.to_owned(),
            _ => LOCAL_MACHINE.to_owned(),
        };
        let key = (gen_id.to_owned(), machine);
        if !lanes.contains_key(&key) {
            lane_order.push(key.clone());
        }
        *lanes.entry(key).or_insert(0) += 1;
    }
    let mut report = Vec::new();
    for row in rows {
        let mut names = rates.machines_with_evidence(&row.id, within);
        let extra: Vec<String> = lane_order
            .iter()
            .filter(|(g, m)| *g == row.id && !names.contains(m))
            .map(|(_, m)| m.clone())
            .collect();
        names.extend(extra);
        let mut machines = Vec::new();
        let mut combined = 0.0;
        for name in &names {
            let Some(found) = rates.throughput(&rate_key(&row.id, name)) else {
                continue;
            };
            let working = lanes
                .get(&(row.id.clone(), name.clone()))
                .copied()
                .unwrap_or(0);
            machines.push(json!({
                "machine": name,
                "pages_per_minute": round_to(found.pages_per_minute(), 1),
                "volumes": found.volumes,
                "lanes": working,
                "working": working > 0,
            }));
            combined += found.pages_per_minute() * working as f64;
        }
        if machines.is_empty() {
            continue;
        }
        report.push(json!({
            "generation": row.name,
            "generation_id": row.id,
            "machines": machines,
            "combined_pages_per_minute": if combined != 0.0 { Some(round_to(combined, 1)) } else { None },
        }));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_reasons() {
        let ev = |v: Value| v.as_object().cloned().unwrap_or_default();
        assert_eq!(
            busy_reason(&ev(json!({"cpu_pressure": 0.84}))).as_deref(),
            Some("CPU pressure 84%")
        );
        assert_eq!(
            busy_reason(&ev(json!({"cpu_pressure": 0.2, "other_cpu": 0.5}))).as_deref(),
            Some("other processes used 50% of the CPU")
        );
        assert_eq!(busy_reason(&ev(json!({"cpu_pressure": true}))), None);
    }
}
