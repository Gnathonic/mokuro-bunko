//! What the queue page is SENT, per display level and per viewer (0.5.2
//! `queue/shape.py`). Visitors never get raw errors, log paths, processor names, the
//! bench key of a draft, or the backend; nested objects are rebuilt field by field.

use serde_json::{Map, Value, json};

use super::sched::PublicNames;
use super::types::{LOCAL, LOCAL_DISPLAY};

pub const LEVELS: [&str; 3] = ["minimal", "normal", "detailed"];
pub const DEFAULT_LEVEL: &str = "normal";

pub const REASON_DOWNLOAD: &str = "download failed \u{2014} will retry";
pub const REASON_ARCHIVE: &str = "archive incomplete";
pub const REASON_INTERRUPTED: &str = "interrupted \u{2014} will retry";
pub const REASON_ENGINE: &str = "engine error";
pub const REASON_RETRY: &str = "failed \u{2014} will retry";

const ARCHIVE_WORDS: &[&str] = &["zip", "truncated", "missing page", "pages short", "incomplete", "corrupt", "no images", "invalid image", "cannot identify image", "archive"];
const INTERRUPTED_WORDS: &[&str] = &["disconnect", "connection", "timed out", "timeout", "went away", "lost", "closed before", "stopping"];
const ENGINE_WORDS: &[&str] = &["out of memory", "oom", "cuda", "hip", "rocm", "exited", "exit code", "traceback", "engine", "runner", "killed", "signal", "error", "exception"];

pub const MINIMAL_PENDING_LIMIT: usize = 10;
pub const QUEUE_REPORT_LIMIT: usize = 100;

const MINIMAL_JOB_KEYS: &[&str] = &["series", "volume", "generation", "percent", "eta_at"];
const NORMAL_JOB_EXTRA: &[&str] = &["status", "done_pages", "total_pages", "eta_seconds", "startup_seconds", "host_busy"];
const DETAILED_JOB_EXTRA: &[&str] = &["engine", "detector", "latency_seconds", "startup_rough", "pipeline"];
const NEXT_KEYS: &[&str] = &["series", "volume", "generation"];
const MINIMAL_PENDING_KEYS: &[&str] = &["series", "volume", "generation", "eta_at", "rough"];
const PENDING_KEYS: &[&str] = &["series", "volume", "generation", "eta_at", "rough", "attempts", "reason"];
const DETAILED_PENDING_EXTRA: &[&str] = &["engine", "detector", "pages", "rate_source", "latency_seconds"];
const FAILED_KEYS: &[&str] = &["series", "volume", "generation", "attempts", "reason"];
const ADMIN_FAILED_KEYS: &[&str] = &["error", "log_file", "last_attempt_at"];
const SKIPPED_KEYS: &[&str] = &["series", "volume", "missing_pages", "page_count", "generations"];

pub fn normalize_level(value: &str) -> &'static str {
    LEVELS.iter().find(|l| **l == value).copied().unwrap_or(DEFAULT_LEVEL)
}

/// `failure_reason(error)`.
pub fn failure_reason(error: &Value) -> &'static str {
    let text = match error {
        Value::Null => String::new(),
        Value::String(s) => s.to_lowercase(),
        other => other.to_string().to_lowercase(),
    };
    if text.starts_with("download failed") {
        REASON_DOWNLOAD
    } else if ARCHIVE_WORDS.iter().any(|w| text.contains(w)) {
        REASON_ARCHIVE
    } else if INTERRUPTED_WORDS.iter().any(|w| text.contains(w)) {
        REASON_INTERRUPTED
    } else if ENGINE_WORDS.iter().any(|w| text.contains(w)) {
        REASON_ENGINE
    } else {
        REASON_RETRY
    }
}

fn display_name(machine: &Value) -> String {
    match machine.as_str() {
        Some(m) if !m.is_empty() && m != LOCAL => m.to_string(),
        _ => LOCAL_DISPLAY.to_string(),
    }
}

/// How a machine is named to this viewer.
pub enum Namer<'a> {
    Admin,
    Visitor(&'a mut PublicNames),
}

impl Namer<'_> {
    fn name(&mut self, machine: &Value) -> String {
        match self {
            Namer::Admin => display_name(machine),
            Namer::Visitor(names) => match machine.as_str() {
                Some(m) => names.name(m),
                None => LOCAL_DISPLAY.to_string(),
            },
        }
    }
}

fn pick(source: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter().map(|k| (k.to_string(), source.get(*k).cloned().unwrap_or(Value::Null))).collect()
}

fn as_float(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// `job_state(card)`: running, loading or waiting.
pub fn job_state(job: &Map<String, Value>) -> &'static str {
    if job.get("status").and_then(Value::as_str) != Some("starting") {
        return "running";
    }
    match job.get("session_ready") {
        Some(Value::Bool(false)) => "loading",
        Some(Value::Bool(true)) => {
            if job.get("delivered") == Some(&Value::Bool(false)) {
                "waiting"
            } else {
                "running"
            }
        }
        _ => {
            if as_float(job.get("startup_seconds")).is_some_and(|s| s > 0.0) {
                "loading"
            } else {
                "waiting"
            }
        }
    }
}

fn state_rank(s: &str) -> u8 {
    match s {
        "running" => 0,
        "loading" => 1,
        "waiting" => 2,
        _ => 3,
    }
}

/// One machine of `group_by_machine`.
struct Grouped {
    machine: String,
    active: Vec<Map<String, Value>>,
    next: Vec<Map<String, Value>>,
    label: Option<String>,
    slots: i64,
    held: Value,
    held_error: Value,
    held_until: Value,
    configuring: Value,
    standby: bool,
    cannot_start: Vec<Value>,
    state: &'static str,
}

fn machine_of(job: &Map<String, Value>) -> String {
    match job.get("machine").and_then(Value::as_str) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => LOCAL.to_string(),
    }
}

fn group_by_machine(jobs: &[Map<String, Value>], connected: &[Map<String, Value>]) -> Vec<Grouped> {
    let mut machines: Vec<Grouped> = Vec::new();
    let new = |machine: String| Grouped {
        machine,
        active: vec![],
        next: vec![],
        label: None,
        slots: 1,
        held: Value::Null,
        held_error: Value::Null,
        held_until: Value::Null,
        configuring: Value::Null,
        standby: false,
        cannot_start: vec![],
        state: "idle",
    };
    for row in connected {
        let Some(machine) = row.get("machine").and_then(Value::as_str).filter(|m| !m.is_empty()) else { continue };
        if machines.iter().any(|m| m.machine == machine) {
            continue;
        }
        let mut g = new(machine.to_string());
        g.slots = row.get("slots").and_then(Value::as_i64).filter(|s| *s > 0).unwrap_or(1);
        g.held = row.get("held").cloned().unwrap_or(Value::Null);
        g.held_error = row.get("held_error").cloned().unwrap_or(Value::Null);
        g.held_until = row.get("held_until").cloned().unwrap_or(Value::Null);
        g.configuring = row.get("configuring").cloned().unwrap_or(Value::Null);
        g.standby = bunko_sched::py::truthy(row.get("standby"));
        g.cannot_start = row.get("cannot_start").and_then(Value::as_array).cloned().unwrap_or_default();
        machines.push(g);
    }
    let mut order: Vec<(String, String)> = Vec::new();
    let mut lanes: std::collections::HashMap<(String, String), Vec<Map<String, Value>>> = Default::default();
    for (index, job) in jobs.iter().enumerate() {
        let machine = machine_of(job);
        if !machines.iter().any(|m| m.machine == machine) {
            machines.push(new(machine.clone()));
        }
        if let Some(label) = job.get("processor").and_then(Value::as_str).filter(|l| !l.is_empty())
            && let Some(g) = machines.iter_mut().find(|m| m.machine == machine)
            && g.label.is_none()
        {
            g.label = Some(label.to_string());
        }
        let slot = match job.get("slot") {
            Some(Value::Number(n)) if n.is_i64() || n.is_u64() => n.to_string(),
            _ => format!("job-{index}"),
        };
        let key = (machine, slot);
        if !lanes.contains_key(&key) {
            order.push(key.clone());
        }
        lanes.entry(key).or_default().push(job.clone());
    }
    for key in order {
        let mut cards = lanes.remove(&key).unwrap_or_default();
        cards.sort_by(|a, b| {
            let pages_out = |j: &Map<String, Value>| if j.get("done_pages").and_then(Value::as_i64).unwrap_or(0) > 0 { 0 } else { 1 };
            let started = |j: &Map<String, Value>| as_float(j.get("started_at")).unwrap_or(0.0);
            pages_out(a).cmp(&pages_out(b)).then(started(a).total_cmp(&started(b)))
        });
        let Some(g) = machines.iter_mut().find(|m| m.machine == key.0) else { continue };
        let mut it = cards.into_iter();
        if let Some(first) = it.next() {
            g.active.push(first);
        }
        g.next.extend(it);
    }
    for g in &mut machines {
        g.slots = g.slots.max(g.active.len() as i64);
        let best = g.active.iter().map(job_state).min_by_key(|s| state_rank(s));
        g.state = match best {
            Some(s) => s,
            None if g.configuring.is_object() => "configuring",
            None if bunko_sched::py::truthy(Some(&g.held)) => "held",
            None if g.standby => "standby",
            None => "idle",
        };
    }
    machines
}

fn job(card: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    let mut out = pick(card, keys);
    out.insert("state".into(), json!(job_state(card)));
    for k in ["percent", "done_pages"] {
        if let Some(v) = out.get_mut(k)
            && !bunko_sched::py::truthy(Some(v))
        {
            *v = json!(0);
        }
    }
    if out.contains_key("pipeline") {
        let p = pipeline(card.get("pipeline"));
        out.insert("pipeline".into(), p);
    }
    out
}

fn pipeline(p: Option<&Value>) -> Value {
    let Some(Value::Object(p)) = p else { return Value::Null };
    let stages: Vec<Value> = p
        .get("stages")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_object)
                .map(|s| {
                    let mut out = pick(s, &["key", "name", "device", "workers", "fused", "busy_pct", "blocked_pct", "starved_pct"]);
                    out.insert(
                        "queue".into(),
                        match s.get("queue") {
                            Some(Value::Object(q)) => Value::Object(pick(q, &["name", "capacity", "mean_depth", "max_depth"])),
                            _ => Value::Null,
                        },
                    );
                    Value::Object(out)
                })
                .collect()
        })
        .unwrap_or_default();
    if stages.is_empty() {
        return Value::Null;
    }
    json!({"verdict": p.get("verdict").cloned().unwrap_or(Value::Null), "bottleneck": p.get("bottleneck").cloned().unwrap_or(Value::Null), "stages": stages})
}

fn hold(h: Option<&Value>, namer: &mut Namer<'_>) -> Value {
    let Some(Value::Object(h)) = h else { return Value::Null };
    let mut out = Map::new();
    out.insert("reason".into(), h.get("reason").cloned().unwrap_or(Value::Null));
    out.insert("since".into(), h.get("since").cloned().unwrap_or(Value::Null));
    if let Some(Value::Object(last)) = h.get("last") {
        out.insert("last".into(), json!({"name": namer.name(last.get("name").unwrap_or(&Value::Null)), "disconnected_at": last.get("disconnected_at").cloned().unwrap_or(Value::Null)}));
    }
    Value::Object(out)
}

fn configuring(g: &Grouped, admin: bool) -> Option<Value> {
    if g.state != "configuring" {
        return None;
    }
    let Value::Object(line) = &g.configuring else { return None };
    let key = line.get("key");
    let mut generation = line.get("generation").cloned().unwrap_or(Value::Null);
    let keystr = key.and_then(Value::as_str);
    if !admin && (keystr.is_none() || keystr.is_some_and(|k| k.starts_with("draft-")) || Some(&generation) == key) {
        generation = Value::Null;
    }
    Some(json!({"generation": generation, "auto": bunko_sched::py::truthy(line.get("auto"))}))
}

fn paused(p: Option<&Value>, namer: &mut Namer<'_>, admin: bool) -> Value {
    let Some(Value::Object(p)) = p else { return Value::Null };
    let processor = p.get("processor").cloned().unwrap_or(Value::Null);
    let processor = match processor.as_str() {
        None => processor,
        Some("") | Some(LOCAL) => processor,
        Some(_) => json!(namer.name(&processor)),
    };
    let mut out = Map::new();
    out.insert("queued".into(), p.get("queued").cloned().unwrap_or(Value::Null));
    out.insert("processor".into(), processor);
    let key = p.get("key").cloned().unwrap_or(Value::Null);
    let generation = p.get("generation").cloned().unwrap_or(Value::Null);
    if admin {
        out.insert("key".into(), key);
        out.insert("generation".into(), generation);
    } else if key.as_str().is_some_and(|k| !k.starts_with("draft-")) && generation != key {
        out.insert("generation".into(), generation);
    }
    Value::Object(out)
}

fn returned(r: Option<&Value>, namer: &mut Namer<'_>, admin: bool) -> Value {
    let Some(Value::Object(r)) = r else { return Value::Null };
    let mut out = Map::new();
    out.insert("machine".into(), json!(namer.name(r.get("machine").unwrap_or(&Value::Null))));
    out.insert("reason".into(), json!(REASON_DOWNLOAD));
    out.insert("at".into(), r.get("at").cloned().unwrap_or(Value::Null));
    if admin {
        for k in ["class", "error", "count"] {
            out.insert(k.into(), r.get(k).cloned().unwrap_or(Value::Null));
        }
    }
    Value::Object(out)
}

fn objects(v: Option<&Value>) -> Vec<Map<String, Value>> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_object).cloned().collect()).unwrap_or_default()
}

/// `shape_status(raw, level, admin, public_names)`.
pub fn shape_status(raw: &Map<String, Value>, level: &str, admin: bool, names: &mut PublicNames) -> Value {
    let level = normalize_level(level);
    let mut namer = if admin { Namer::Admin } else { Namer::Visitor(names) };
    let jobs = match raw.get("current_jobs") {
        Some(Value::Array(_)) => objects(raw.get("current_jobs")),
        _ => objects(Some(&Value::Array(raw.get("current").into_iter().cloned().collect()))),
    };
    let connected = objects(raw.get("connected_machines"));
    let grouped = group_by_machine(&jobs, &connected);
    let pending = objects(raw.get("pending_ocr"));
    let mut common = Map::new();
    common.insert("level".into(), json!(level));
    common.insert("queue_done_at".into(), raw.get("queue_done_at").cloned().unwrap_or(Value::Null));
    common.insert("pending_count".into(), json!(pending.len()));
    let h = hold(raw.get("processing_hold"), &mut namer);
    common.insert("processing_hold".into(), h);
    let p = paused(raw.get("paused_for_benchmark"), &mut namer, admin);
    common.insert("paused_for_benchmark".into(), p);
    if admin {
        let held: Vec<Value> = objects(raw.get("held_rows"))
            .into_iter()
            .filter(|r| r.get("generation").is_some_and(Value::is_string) && r.get("reason").is_some_and(Value::is_string))
            .map(|r| json!({"generation": r["generation"], "reason": r["reason"]}))
            .collect();
        common.insert("held_rows".into(), Value::Array(held));
    }
    if level == "minimal" {
        let mut machines = Vec::new();
        for g in &grouped {
            let mut m = Map::new();
            m.insert("name".into(), json!(namer.name(&json!(g.machine))));
            m.insert("state".into(), json!(g.state));
            m.insert("slots".into(), json!(g.slots));
            m.insert("jobs".into(), Value::Array(g.active.iter().map(|j| Value::Object(job(j, MINIMAL_JOB_KEYS))).collect()));
            m.insert("next".into(), Value::Array(g.next.iter().map(|j| Value::Object(pick(j, NEXT_KEYS))).collect()));
            if let Some(c) = configuring(g, admin) {
                m.insert("configuring".into(), c);
            }
            machines.push(Value::Object(m));
        }
        common.insert("machines".into(), Value::Array(machines));
        common.insert("pending".into(), Value::Array(pending.iter().take(MINIMAL_PENDING_LIMIT).map(|i| Value::Object(pick(i, MINIMAL_PENDING_KEYS))).collect()));
        return Value::Object(common);
    }
    let detailed = level == "detailed";
    let mut job_keys: Vec<&str> = MINIMAL_JOB_KEYS.to_vec();
    job_keys.extend(NORMAL_JOB_EXTRA);
    let mut pending_keys: Vec<&str> = PENDING_KEYS.to_vec();
    if detailed {
        job_keys.extend(DETAILED_JOB_EXTRA);
        pending_keys.extend(DETAILED_PENDING_EXTRA);
    }
    let mut rates: std::collections::HashMap<(String, String), f64> = Default::default();
    if detailed {
        for entry in objects(raw.get("speed")) {
            let gid = entry.get("generation_id").map(|v| v.to_string()).unwrap_or_default();
            for m in objects(entry.get("machines")) {
                if let Some(v) = as_float(m.get("pages_per_minute")).filter(|v| *v > 0.0) {
                    rates.insert((gid.clone(), m.get("machine").and_then(Value::as_str).unwrap_or("").to_string()), v);
                }
            }
        }
    }
    let mut machines = Vec::new();
    for g in &grouped {
        let mut jobs_out = Vec::new();
        for j in &g.active {
            let mut out = job(j, &job_keys);
            if detailed {
                let gid = j.get("generation_id").map(|v| v.to_string()).unwrap_or_default();
                out.insert("throughput_pages_per_minute".into(), json!(rates.get(&(gid, g.machine.clone()))));
            }
            jobs_out.push(Value::Object(out));
        }
        let mut entry = Map::new();
        entry.insert("name".into(), json!(namer.name(&json!(g.machine))));
        entry.insert("state".into(), json!(g.state));
        entry.insert("slots".into(), json!(g.slots));
        entry.insert("jobs".into(), Value::Array(jobs_out));
        entry.insert("next".into(), Value::Array(g.next.iter().map(|j| Value::Object(pick(j, NEXT_KEYS))).collect()));
        if let Some(c) = configuring(g, admin) {
            entry.insert("configuring".into(), c);
        }
        if bunko_sched::py::truthy(Some(&g.held)) {
            let mut held = Map::new();
            held.insert("reason".into(), json!("downloads failing"));
            held.insert("until".into(), g.held_until.clone());
            if admin {
                held.insert("error".into(), g.held_error.clone());
            }
            entry.insert("held".into(), Value::Object(held));
        }
        let cannot: Vec<Value> = g
            .cannot_start
            .iter()
            .filter_map(Value::as_object)
            .map(|row| {
                let mut m = Map::new();
                m.insert("generation".into(), row.get("generation").cloned().unwrap_or(Value::Null));
                m.insert("until".into(), row.get("until").cloned().unwrap_or(Value::Null));
                if admin {
                    m.insert("error".into(), row.get("error").cloned().unwrap_or(Value::Null));
                }
                Value::Object(m)
            })
            .collect();
        if !cannot.is_empty() {
            entry.insert("cannot_start".into(), Value::Array(cannot));
        }
        if admin {
            entry.insert("label".into(), json!(g.label));
        }
        machines.push(Value::Object(entry));
    }
    let mut failed = Vec::new();
    for item in objects(raw.get("failed")) {
        let mut out = pick(&item, FAILED_KEYS);
        if !bunko_sched::py::truthy(out.get("attempts")) {
            out.insert("attempts".into(), json!(1));
        }
        out.insert("reason".into(), json!(failure_reason(item.get("error").unwrap_or(&Value::Null))));
        if admin {
            out.extend(pick(&item, ADMIN_FAILED_KEYS));
        }
        failed.push(Value::Object(out));
    }
    let mut payload = common;
    payload.insert("machines".into(), Value::Array(machines));
    let mut pend = Vec::new();
    for item in pending.iter().take(QUEUE_REPORT_LIMIT) {
        let mut out = pick(item, &pending_keys);
        let attempts = item.get("attempts").filter(|v| bunko_sched::py::truthy(Some(v))).cloned().unwrap_or(json!(0));
        out.insert("attempts".into(), attempts);
        out.insert("returned".into(), returned(item.get("returned"), &mut namer, admin));
        pend.push(Value::Object(out));
    }
    payload.insert("pending".into(), Value::Array(pend));
    let thumbs = raw.get("pending_thumbnails").filter(|v| bunko_sched::py::truthy(Some(v))).cloned().unwrap_or(json!(0));
    payload.insert("pending_thumbnails".into(), thumbs);
    payload.insert("failed_count".into(), json!(failed.len()));
    payload.insert("failed".into(), Value::Array(failed));
    payload.insert("skipped_missing_pages".into(), Value::Array(objects(raw.get("skipped_missing_pages")).iter().map(|i| Value::Object(pick(i, SKIPPED_KEYS))).collect()));
    payload.insert(
        "generations".into(),
        Value::Array(objects(raw.get("generations")).iter().map(|r| json!({"id": r.get("id").cloned().unwrap_or(Value::Null), "name": r.get("name").cloned().unwrap_or(Value::Null)})).collect()),
    );
    if detailed {
        let mut speed = Vec::new();
        for entry in objects(raw.get("speed")) {
            let Some(combined) = as_float(entry.get("combined_pages_per_minute")).filter(|c| *c > 0.0) else { continue };
            let working = objects(entry.get("machines")).iter().filter(|m| bunko_sched::py::truthy(m.get("working"))).count();
            speed.push(json!({"generation": entry.get("generation").cloned().unwrap_or(Value::Null), "pages_per_minute": combined, "machines": working}));
        }
        payload.insert("speed".into(), Value::Array(speed));
    }
    if admin {
        payload.insert("backend".into(), raw.get("backend").cloned().unwrap_or(Value::Null));
    }
    Value::Object(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons() {
        assert_eq!(failure_reason(&json!("download failed on 3 tries")), REASON_DOWNLOAD);
        assert_eq!(failure_reason(&json!("Bad zip file")), REASON_ARCHIVE);
        assert_eq!(failure_reason(&json!("the runner exited")), REASON_ENGINE);
        assert_eq!(failure_reason(&json!("connection reset")), REASON_INTERRUPTED);
        assert_eq!(failure_reason(&json!("hmm")), REASON_RETRY);
    }

    #[test]
    fn states() {
        let card = |v: Value| v.as_object().cloned().unwrap();
        assert_eq!(job_state(&card(json!({"status": "running"}))), "running");
        assert_eq!(job_state(&card(json!({"status": "starting", "session_ready": false}))), "loading");
        assert_eq!(job_state(&card(json!({"status": "starting", "session_ready": true, "delivered": false}))), "waiting");
    }
}
