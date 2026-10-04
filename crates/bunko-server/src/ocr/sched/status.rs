//! What the scheduler tells people: running cards, the pending list, the queue plan,
//! connected machines, holds, the raw queue-page status, the reader queue file's
//! content and one volume's outlook (spec ocr-scheduling §15.3, §19–§20, §23–§25).

use std::collections::HashSet;
use std::sync::Arc;

use bunko_core::generations::Generation;
use bunko_sched::outlook::{OwedRow, pending_entries};
use bunko_sched::plan::{PlanInputs, QueuePlan, job_identity, plan_queue};
use bunko_sched::speed::{RowRef, SPEED_WINDOW_SECONDS, speed_report};
use serde_json::{Map, Value, json};

use super::Scheduler;
use crate::ocr::types::{Job, LOCAL};

/// The raw (unshaped) queue status, as `QueueAPI.raw_status` assembles it.
pub type RawStatus = Map<String, Value>;

/// `QUEUE_REPORT_LIMIT`: waiting volumes the queue file lists.
pub const QUEUE_REPORT_LIMIT: usize = 100;

/// A row's precision hold: `"No connected machine can run <mode>"`.
pub fn hold_reason(mode: &str) -> String {
    format!("No connected machine can run {mode}")
}

impl Scheduler {
    // --- holds ----------------------------------------------------------------------------

    /// `processing_hold()`: nothing can run OCR (no local processing, no processor).
    pub fn processing_hold(&self) -> Option<Value> {
        if self.settings.local_processing && self.machines.get(LOCAL).is_some_and(|m| m.connected())
        {
            return None;
        }
        if !self.remote_connected().is_empty() {
            return None;
        }
        let mut hold = Map::new();
        hold.insert("reason".into(), json!("no-processor"));
        hold.insert(
            "since".into(),
            json!(
                self.last_disconnect
                    .as_ref()
                    .map_or(self.started_at, |l| l.1)
            ),
        );
        if let Some((name, at)) = &self.last_disconnect {
            hold.insert("last".into(), json!({"name": name, "disconnected_at": at}));
        }
        Some(Value::Object(hold))
    }

    /// `_every_machine_held()`: held = a hold, or (remote) an open breaker.
    pub fn every_machine_held(&self) -> bool {
        let mut seen = false;
        for m in self.machines.values() {
            if !m.connected() || (m.local && !self.settings.local_processing) {
                continue;
            }
            seen = true;
            let held = self.held(&m.name) || (!m.local && self.breaker_open(&m.pid));
            if !held {
                return false;
            }
        }
        seen
    }

    /// `queue_hold()`: why the WHOLE queue is not moving, or None.
    pub fn queue_hold(&self) -> Option<&'static str> {
        if self.processing_hold().is_some() {
            return Some("no-processor");
        }
        if self.every_machine_held() {
            return Some(if self.paused_for_benchmark().is_some() {
                "benchmarking"
            } else {
                "paused"
            });
        }
        None
    }

    /// `precision_holds()`: forced-precision rows every current machine refuses.
    pub fn precision_holds(&self) -> Vec<(String, String)> {
        let machines: Vec<&super::Machine> = self
            .machines
            .values()
            .filter(|m| m.connected() && (!m.local || self.settings.local_processing))
            .collect();
        if machines.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for row in self.enabled_rows() {
            if !row.precision_applies() || !bunko_core::engines::is_forced_precision(&row.precision)
            {
                continue;
            }
            let all_refuse = machines.iter().all(|m| {
                let pools = self.machine_row_pools(&m.name, &row);
                let sd = pools
                    .get("stage_device")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                super::claim::precision_refusal(&row, &m.catalog, &sd).is_some()
            });
            if all_refuse {
                out.push((row.id.clone(), hold_reason(&row.precision)));
            }
        }
        out
    }

    pub fn held_rows(&self) -> Vec<Value> {
        self.precision_holds()
            .into_iter()
            .map(|(id, reason)| {
                let name = self
                    .settings
                    .rows
                    .iter()
                    .find(|r| r.id == id)
                    .map(|r| r.name.clone())
                    .unwrap_or(id);
                json!({"generation": name, "reason": reason})
            })
            .collect()
    }

    // --- cards and lists -----------------------------------------------------------------------

    /// The running jobs in `_progress_entry` shape, with page totals filled in.
    pub fn running_jobs(&self) -> Vec<Map<String, Value>> {
        let mut out = Vec::new();
        for (job, data) in &self.cards {
            let g = |k: &str| data.get(k).cloned().unwrap_or(Value::Null);
            let mut e = Map::new();
            for k in ["series", "volume", "generation", "engine", "detector"] {
                e.insert(k.into(), g(k));
            }
            e.insert(
                "percent".into(),
                data.get("percent").cloned().unwrap_or(json!(0)),
            );
            e.insert("eta_seconds".into(), g("eta_seconds"));
            e.insert(
                "done_pages".into(),
                data.get("done_pages").cloned().unwrap_or(json!(0)),
            );
            let total = data
                .get("total_pages")
                .filter(|v| v.as_i64().is_some())
                .cloned();
            e.insert(
                "total_pages".into(),
                total.unwrap_or_else(|| json!(self.known_pages(&job.rel))),
            );
            e.insert(
                "status".into(),
                data.get("status").cloned().unwrap_or(json!("running")),
            );
            for k in [
                "generation_id",
                "slot",
                "first_page_at",
                "session_started_at",
                "started_at",
                "session_ready",
                "delivered",
            ] {
                e.insert(k.into(), g(k));
            }
            e.insert("eta_at".into(), Value::Null);
            for k in [
                "rate_pages_per_second",
                "latency_seconds",
                "rate_source",
                "processor",
                "machine",
            ] {
                e.insert(k.into(), g(k));
            }
            if data
                .get("pipeline")
                .and_then(|p| p.get("stages"))
                .is_some_and(|s| s.as_array().is_some_and(|a| !a.is_empty()))
            {
                e.insert("pipeline".into(), g("pipeline"));
            }
            if let Some(b) = data.get("host_busy") {
                e.insert("host_busy".into(), b.clone());
            }
            out.push(e);
        }
        out
    }

    /// `_pending_entry`.
    fn pending_entry(&self, job: &Job) -> Option<Map<String, Value>> {
        let row = self.row(&job.gid)?;
        let mut e = Map::new();
        e.insert("series".into(), json!(job.series()));
        e.insert("volume".into(), json!(job.volume()));
        e.insert("generation".into(), json!(row.name));
        e.insert("generation_id".into(), json!(row.id));
        e.insert("engine".into(), json!(row.engine));
        e.insert("detector".into(), json!(row.effective_detector()));
        e.insert("pages".into(), json!(self.known_pages(&job.rel)));
        if job.upgrade {
            e.insert("kind".into(), json!("upgrade"));
        }
        let key = self.failure_key_of(job, row);
        let mtime = self.owed.volumes.get(&*job.rel).map(|v| v.mtime);
        if let Some(attempts) = bunko_sched::failures::pending_attempts(
            self.failures.get(&key).and_then(Value::as_object),
            mtime,
        ) {
            e.insert("attempts".into(), json!(attempts));
        }
        if let Some(r) = self.download_returns.get(job) {
            e.insert("returned".into(), r.as_entry());
        }
        Some(e)
    }

    /// `pending_jobs()`: the queue page's list, in processing order (cached per queue
    /// generation; never resets a failure record).
    pub fn pending_jobs(&mut self) -> Arc<Vec<Map<String, Value>>> {
        if let Some((g, list)) = &self.pending_cache
            && *g == self.queue_generation
        {
            return list.clone();
        }
        let exclude: HashSet<Job> = self
            .claims
            .keys()
            .cloned()
            .chain(self.attempted.iter().cloned())
            .collect();
        let jobs = self.candidates(&exclude, false);
        let list: Vec<Map<String, Value>> =
            jobs.iter().filter_map(|j| self.pending_entry(j)).collect();
        let list = Arc::new(list);
        let changed = self
            .pending_cache
            .as_ref()
            .is_none_or(|(_, old)| **old != *list);
        self.pending_cache = Some((self.queue_generation, list.clone()));
        if changed {
            self.bump_page();
        }
        list
    }

    /// `plan_items`: the pending list without what runs.
    pub fn plan_items(
        pending: &[Map<String, Value>],
        running: &[Map<String, Value>],
    ) -> Vec<Map<String, Value>> {
        let running_keys: HashSet<String> = running
            .iter()
            .map(|j| format!("{:?}", job_identity(j)))
            .collect();
        pending
            .iter()
            .filter(|e| !running_keys.contains(&format!("{:?}", job_identity(e))))
            .cloned()
            .collect()
    }

    /// `_lane_machines()`: one entry per lane, by hardware name.
    pub fn lane_machines(&self) -> Vec<String> {
        self.lanes
            .iter()
            .filter_map(|l| self.machines.get(&l.pid).map(|m| m.name.clone()))
            .collect()
    }

    /// `queue_plan(running, pending, through)`.
    pub fn queue_plan(
        &self,
        running: &[Map<String, Value>],
        pending: &[Map<String, Value>],
        through: Option<i64>,
    ) -> QueuePlan {
        let pricing = self.pricing();
        let lane_machines = self.lane_machines();
        let refusal = |g: &str, machine: &str| -> Option<String> {
            let row = self.settings.rows.iter().find(|r| r.id == g)?;
            let m = self.machine_by_name(machine)?;
            self.refusal(&m.pid, row)
        };
        let hold = |g: &str| -> Option<String> {
            let row = self.settings.rows.iter().find(|r| r.id == g)?;
            (row.precision_applies() && bunko_core::engines::is_forced_precision(&row.precision))
                .then(|| hold_reason(&row.precision))
        };
        let inputs = PlanInputs {
            lane_count: lane_machines.len().max(1),
            lane_machines: Some(&lane_machines),
            pricing: &pricing,
            now: self.now(),
            through,
            refusal_for: Some(&refusal),
            hold_for: Some(&hold),
        };
        plan_queue(running, pending, &inputs)
    }

    /// `connected_machines()` (§25.2), in lane order.
    pub fn connected_machines(&self) -> Vec<Value> {
        let mut order: Vec<(String, String, usize)> = Vec::new();
        for lane in &self.lanes {
            let Some(m) = self.machines.get(&lane.pid) else {
                continue;
            };
            match order.iter_mut().find(|(n, _, _)| *n == m.name) {
                Some(e) => e.2 += 1,
                None => order.push((m.name.clone(), m.pid.clone(), 1)),
            }
        }
        let configuring = self.configuring();
        let now = self.now();
        order
            .into_iter()
            .map(|(name, pid, slots)| {
                let mut row = Map::new();
                row.insert("machine".into(), json!(name));
                row.insert("slots".into(), json!(slots));
                let standby = !self.held(&name)
                    && self
                        .lanes
                        .iter()
                        .any(|l| l.pid == pid && l.waiting_for_faster && l.session.is_none());
                if standby {
                    row.insert("standby".into(), json!(true));
                }
                if let Some(b) = self.breakers.get(&pid).filter(|b| b.is_open(now)) {
                    row.insert("held".into(), json!("downloads"));
                    row.insert("held_until".into(), json!(b.open_until));
                    row.insert("held_error".into(), json!(b.last_error));
                }
                let backoffs = self.start_backoffs(&name);
                if !backoffs.is_empty() {
                    row.insert("cannot_start".into(), Value::Array(backoffs));
                }
                if let Some(line) = configuring.get(&name) {
                    row.insert("configuring".into(), line.clone());
                }
                Value::Object(row)
            })
            .collect()
    }

    pub fn speed(&self, running: &[Map<String, Value>]) -> Vec<Value> {
        let rows: Vec<RowRef> = self
            .enabled_rows()
            .iter()
            .map(|r| RowRef {
                id: r.id.clone(),
                name: r.name.clone(),
            })
            .collect();
        speed_report(&rows, running, &self.rates, SPEED_WINDOW_SECONDS)
    }

    /// `skipped_missing_pages()`.
    pub fn skipped_missing_pages(&self) -> Vec<Value> {
        let library = self.library();
        self.owed
            .volumes
            .iter()
            .filter(|(_, v)| !v.skipped.is_empty())
            .map(|(rel, v)| {
                let job = Job::new(rel, "");
                let names: Vec<String> = v
                    .skipped
                    .iter()
                    .filter_map(|g| {
                        self.settings
                            .rows
                            .iter()
                            .find(|r| *r.id == **g)
                            .map(|r| r.name.clone())
                    })
                    .collect();
                json!({
                    "series": job.series(),
                    "volume": job.volume(),
                    "missing_pages": v.missing_pages,
                    "page_count": self.deps.facts.page_count(&library.join(rel)),
                    "generations": names,
                })
            })
            .collect()
    }

    /// The failure records as the queue page lists them, sorted by (series, volume).
    pub fn failed_list(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .failures
            .values()
            .filter_map(Value::as_object)
            .map(|e| {
                let g = |k: &str| e.get(k).cloned().unwrap_or(Value::Null);
                json!({
                    "series": g("series"), "volume": g("volume"), "generation": g("generation"),
                    "engine": g("engine"), "detector": g("detector"), "error": g("error"),
                    "attempts": e.get("attempts").cloned().unwrap_or(json!(1)),
                    "last_attempt_at": g("last_attempt_at"), "log_file": g("log_file"),
                })
            })
            .collect();
        out.sort_by(|a, b| {
            let k = |v: &Value| {
                (
                    v["series"].as_str().unwrap_or("").to_string(),
                    v["volume"].as_str().unwrap_or("").to_string(),
                )
            };
            k(a).cmp(&k(b))
        });
        out
    }

    /// The enabled rows in run order: `[{id, name, engine, detector}]`.
    pub fn generation_order(&self) -> Vec<Value> {
        self.enabled_rows().iter().map(|r| json!({"id": r.id, "name": r.name, "engine": r.engine, "detector": r.effective_detector()})).collect()
    }

    /// `QueueAPI.raw_status` (the input of the queue page's shaping).
    pub fn raw_status(&mut self) -> RawStatus {
        let running = self.running_jobs();
        let pending = self.pending_jobs();
        let items = Self::plan_items(&pending, &running);
        let plan = self.queue_plan(&running, &items, None);
        let mut raw = Map::new();
        raw.insert(
            "current".into(),
            plan.running
                .first()
                .cloned()
                .map_or(Value::Null, Value::Object),
        );
        raw.insert(
            "current_jobs".into(),
            Value::Array(plan.running.iter().cloned().map(Value::Object).collect()),
        );
        raw.insert(
            "pending_ocr".into(),
            Value::Array(plan.pending.iter().cloned().map(Value::Object).collect()),
        );
        raw.insert("queue_done_at".into(), json!(plan.done_at));
        raw.insert(
            "pending_thumbnails".into(),
            json!(self.deps.facts.pending_thumbnails()),
        );
        raw.insert("failed".into(), Value::Array(self.failed_list()));
        raw.insert("backend".into(), json!(self.backend_label()));
        raw.insert("generations".into(), Value::Array(self.generation_order()));
        raw.insert(
            "skipped_missing_pages".into(),
            Value::Array(self.skipped_missing_pages()),
        );
        raw.insert(
            "paused_for_benchmark".into(),
            self.paused_for_benchmark().unwrap_or(Value::Null),
        );
        raw.insert(
            "processing_hold".into(),
            self.processing_hold().unwrap_or(Value::Null),
        );
        raw.insert("held_rows".into(), Value::Array(self.held_rows()));
        raw.insert(
            "connected_machines".into(),
            Value::Array(self.connected_machines()),
        );
        raw.insert("speed".into(), Value::Array(self.speed(&plan.running)));
        raw
    }

    fn backend_label(&self) -> String {
        if self.settings.local_processing {
            self.machines
                .get(LOCAL)
                .map(|m| m.host.backend.clone())
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| "onnxruntime".into())
        } else {
            "remote processors".into()
        }
    }

    /// The rows one volume is still owed: primary first, then list order.
    pub fn owed_rows(&self, rel: &str) -> Vec<OwedRow> {
        let Some(v) = self.owed.volumes.get(rel) else {
            return Vec::new();
        };
        let mut rows: Vec<OwedRow> = self
            .enabled_rows()
            .into_iter()
            .filter(|r| v.rows.iter().any(|g| **g == *r.id) || (v.upgrade && r.primary))
            .map(|r| OwedRow {
                id: r.id,
                name: r.name,
                primary: r.primary,
            })
            .collect();
        rows.sort_by_key(|r| !r.primary);
        rows
    }

    /// `OcrControl.volume_pending`: `[{kind, id, eta}]`, None when this process schedules
    /// nothing, `[]` when nothing is owed.
    pub fn volume_pending(&mut self, rel: &str, max_items: Option<usize>) -> Vec<Value> {
        let job = Job::new(rel, "");
        let (series, volume) = (job.series().to_string(), job.volume().to_string());
        let owed = self.owed_rows(rel);
        if owed.is_empty() {
            return Vec::new();
        }
        let mut planned: Vec<Map<String, Value>> = Vec::new();
        if self.queue_hold().is_none() {
            let running = self.running_jobs();
            let pending = self.pending_jobs();
            let items = Self::plan_items(&pending, &running);
            let ours: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.get("series").and_then(Value::as_str) == Some(series.as_str())
                        && e.get("volume").and_then(Value::as_str) == Some(volume.as_str())
                })
                .map(|(i, _)| i)
                .collect();
            let last = ours
                .last()
                .copied()
                .filter(|l| max_items.is_none_or(|m| *l < m));
            let plan = match last {
                Some(l) => self.queue_plan(&running, &items, Some(l as i64)),
                None => self.queue_plan(&running, &[], None),
            };
            planned.extend(plan.running);
            planned.extend(plan.pending);
        }
        pending_entries(&owed, &series, &volume, &planned)
    }

    /// `OcrControl.queue_document`: `(held, volumes, pending_volumes)` for the reader's
    /// queue file. Volumes: every running one, then the next 100 waiting, in order.
    pub fn queue_document(&mut self) -> (Option<&'static str>, Vec<Value>, usize) {
        let held = self.queue_hold();
        let running = self.running_jobs();
        let pending = self.pending_jobs();
        let items = Self::plan_items(&pending, &running);
        let plan = self.queue_plan(&running, &items, None);
        let mut order: Vec<(String, String)> = Vec::new();
        let mut jobs: std::collections::HashMap<
            (String, String),
            indexmap::IndexMap<String, Map<String, Value>>,
        > = Default::default();
        let mut running_now: HashSet<(String, String, String)> = HashSet::new();
        for (entry, is_running) in plan
            .running
            .iter()
            .map(|e| (e, true))
            .chain(plan.pending.iter().map(|e| (e, false)))
        {
            let (Some(s), Some(v), Some(g)) = (
                entry.get("series").and_then(Value::as_str),
                entry.get("volume").and_then(Value::as_str),
                entry.get("generation_id").and_then(Value::as_str),
            ) else {
                continue;
            };
            let key = (s.to_string(), v.to_string());
            if !jobs.contains_key(&key) {
                order.push(key.clone());
                jobs.insert(key.clone(), Default::default());
            }
            if is_running {
                running_now.insert((s.to_string(), v.to_string(), g.to_string()));
            }
            jobs.get_mut(&key)
                .expect("inserted above")
                .entry(g.to_string())
                .or_insert_with(|| entry.clone());
        }
        let rank = self.rank();
        let running_volumes: HashSet<(String, String)> = running_now
            .iter()
            .map(|(s, v, _)| (s.clone(), v.clone()))
            .collect();
        let pending_volumes = order
            .iter()
            .filter(|k| !running_volumes.contains(*k))
            .count();
        let mut waiting_listed = 0;
        let mut volumes = Vec::new();
        for (series, volume) in order {
            let key = (series.clone(), volume.clone());
            if !running_volumes.contains(&key) {
                if waiting_listed >= QUEUE_REPORT_LIMIT {
                    continue;
                }
                waiting_listed += 1;
            }
            let listed = &jobs[&key];
            let rel = if series == "." {
                format!("{volume}.cbz")
            } else {
                format!("{series}/{volume}.cbz")
            };
            let mut rows: Vec<bunko_core::generations::Generation> = Vec::new();
            for o in self.owed_rows(&rel) {
                if let Some(r) = self.settings.rows.iter().find(|r| r.id == o.id) {
                    rows.push(r.clone());
                }
            }
            for g in listed.keys() {
                if !rows.iter().any(|r| r.id == *g)
                    && let Some(r) = self.settings.rows.iter().find(|r| r.id == *g)
                {
                    rows.push(r.clone());
                }
            }
            rows.sort_by_key(|r| (!r.primary, rank.get(&r.id).copied().unwrap_or(rank.len())));
            let mut out = Vec::new();
            for row in rows {
                let priced = listed.get(&row.id);
                let is_running =
                    running_now.contains(&(series.clone(), volume.clone(), row.id.clone()));
                let state = if is_running {
                    "running"
                } else if priced.is_none()
                    || held.is_some()
                    || priced.is_some_and(|p| bunko_sched::py::truthy(p.get("held")))
                {
                    "held"
                } else {
                    "queued"
                };
                let eta = if state != "held" {
                    priced
                        .and_then(|p| p.get("eta_at"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    None
                };
                let mut progress = Value::Null;
                if is_running && let Some(p) = priced {
                    let done = p.get("done_pages").and_then(Value::as_f64);
                    let total = p.get("total_pages").and_then(Value::as_f64);
                    if let (Some(d), Some(t)) = (done, total)
                        && t > 0.0
                    {
                        progress = json!(bunko_sched::py::round_to((d / t).clamp(0.0, 1.0), 3));
                    }
                }
                out.push(json!({"kind": if row.primary { "ocr" } else { "layer" }, "id": row.name, "state": state, "eta": eta, "progress": progress}));
            }
            if !out.is_empty() {
                volumes.push(json!({"series": series, "volume": volume, "jobs": out}));
            }
        }
        (held, volumes, pending_volumes)
    }
}

/// One (machine, layer) line of the admin speed list (`_speed_layer`), or None when
/// there is nothing to say. `bench_flat`: a profile's bench (its own
/// `pages_per_second`), else a `.ocr-bench.json` result (`best`, then `baseline`).
fn speed_layer(
    row: &Generation,
    found: Option<bunko_sched::throughput::Throughput>,
    bench: Option<&Map<String, Value>>,
    bench_flat: bool,
) -> Option<Value> {
    let blocks: Vec<Option<&Value>> = match bench {
        None => Vec::new(),
        Some(b) if bench_flat => vec![b.get("pages_per_second")],
        Some(b) => vec![
            b.get("best").and_then(|v| v.get("pages_per_second")),
            b.get("baseline").and_then(|v| v.get("pages_per_second")),
        ],
    };
    let bench_pps = blocks
        .into_iter()
        .flatten()
        .filter(|v| v.is_number())
        .filter_map(Value::as_f64)
        .find(|v| *v > 0.0);
    if found.is_none() && bench_pps.is_none() {
        return None;
    }
    Some(json!({
        "generation_id": row.id,
        "generation": row.name,
        "pages_per_minute": found.map(|f| bunko_sched::py::round_to(f.pages_per_minute(), 1)),
        "volumes": found.map_or(0, |f| f.volumes),
        "last_at": found.and_then(|f| f.last_at),
        "bench_pages_per_minute": bench_pps.map(|p| bunko_sched::py::round_to(p * 60.0, 1)),
    }))
}

/// `_hardware(host)`: `{cpu, gpu}` as text, or None for nothing to show.
fn hardware(host: Option<&Value>) -> Value {
    let Some(Value::Object(h)) = host else {
        return Value::Null;
    };
    let pick = |k: &str| match h.get(k) {
        Some(v) if bunko_sched::py::truthy(Some(v)) => match v {
            Value::String(s) => json!(s),
            other => json!(other.to_string()),
        },
        _ => Value::Null,
    };
    let (cpu, gpu) = (pick("cpu"), pick("gpu"));
    if cpu.is_null() && gpu.is_null() {
        Value::Null
    } else {
        json!({"cpu": cpu, "gpu": gpu})
    }
}

impl Scheduler {
    /// `_processor_speed`: per machine, per row it has run, what it really delivers
    /// (real throughput) beside its own benchmark — this server first (while it does
    /// OCR), then every connected processor, then every remembered one.
    pub fn admin_speed(&self, local_host: Option<&Value>) -> Vec<Value> {
        use bunko_sched::throughput::{RECENT_VOLUMES, profile_throughput, records_throughput};
        let rows: Vec<Generation> = self
            .settings
            .rows
            .iter()
            .filter(|r| r.enabled)
            .cloned()
            .collect();
        if rows.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.settings.local_processing {
            let history = self.congestion.load();
            let saved = bunko_sched::bench_file::BenchFile::new(&self.storage()).load();
            let mut layers = Vec::new();
            for row in &rows {
                let records: Vec<Value> = history
                    .iter()
                    .find(|(id, _)| *id == row.id)
                    .map(|(_, runs)| runs.iter().cloned().map(Value::Object).collect())
                    .unwrap_or_default();
                let found = records_throughput(&records, RECENT_VOLUMES);
                let mine = saved.get(&row.id).and_then(Value::as_object);
                let measured_here = mine.is_some_and(|m| {
                    matches!(m.get("processor"), None | Some(Value::Null))
                        || m.get("processor")
                            .and_then(Value::as_str)
                            .is_some_and(|p| p.is_empty() || p == LOCAL)
                });
                let layer = if measured_here {
                    speed_layer(row, found, mine, false)
                } else {
                    let profile = self.machine_profile(LOCAL, row);
                    speed_layer(row, found, profile.and_then(|p| p.bench).as_ref(), true)
                };
                layers.extend(layer);
            }
            out.push(json!({"name": LOCAL, "local": true, "connected": true,
                "host": local_host.cloned().unwrap_or(Value::Null), "layers": layers}));
        }
        let connected: Vec<&super::Machine> = self
            .machines
            .values()
            .filter(|m| !m.local && m.connected())
            .collect();
        let mut names: Vec<String> = connected.iter().map(|m| m.name.clone()).collect();
        let mut remembered: Vec<String> = self
            .profiles
            .names()
            .into_iter()
            .filter(|n| !names.contains(n))
            .collect();
        remembered.sort();
        names.extend(remembered);
        for name in names {
            let live = connected.iter().find(|m| m.name == name);
            let mut layers = Vec::new();
            for row in &rows {
                let profile = match live {
                    Some(_) => self.machine_profile(&name, row),
                    None => self.profiles.row_for(
                        &name,
                        &row.id,
                        Some(&row.output_affecting()),
                        &row.precision,
                        crate::ocr::profiles::Formats::Recorded,
                    ),
                };
                let Some(profile) = profile else { continue };
                let runs = Value::Object(profile.runs.clone());
                layers.extend(speed_layer(
                    row,
                    profile_throughput(Some(&runs)),
                    profile.bench.as_ref(),
                    true,
                ));
            }
            let host = match live {
                Some(m) => Some(m.host_value.clone()),
                None => self.profiles.load(&name).get("host").cloned(),
            };
            out.push(
                json!({"name": name, "local": false, "connected": live.is_some(),
                "host": hardware(host.as_ref()), "layers": layers}),
            );
        }
        out
    }
}
