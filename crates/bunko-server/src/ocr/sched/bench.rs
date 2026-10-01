//! Benchmarks: the queue, hold and pre-empt contract and the stored results (spec
//! ocr-generations-bench §6). The measurement itself runs on a processor (`bench` op);
//! the library builds the sample, holds and pre-empts that machine's OCR while its line
//! runs, follows the `bench_*` events and stores what comes back.

use std::collections::{HashMap, VecDeque};

use bunko_core::generations::{Generation, parse_bench_spec};
use bunko_proto::{BenchOp, Event, Op};
use serde_json::{Map, Value, json};

use super::{Msg, Scheduler};
use crate::ocr::profiles::profile_key;
use crate::ocr::types::{Job, LOCAL};

pub const BENCH_BUDGET_SECONDS: f64 = 900.0;
pub const BENCH_SLACK_SECONDS: f64 = 1800.0;
pub const DEFAULT_SAMPLE_PAGES: i64 = 32;
pub const MIN_SAMPLE_PAGES: i64 = 4;
pub const MAX_SAMPLE_PAGES: i64 = 512;
pub const MAX_DRAFT_RESULTS: usize = 32;
const BENCH_VOLUME_PAGES: f64 = 200.0;

/// `POST /api/ocr/generations/<key>/bench`'s arguments.
#[derive(Clone, Debug, Default)]
pub struct BenchRequest {
    pub key: String,
    pub spec: Option<Value>,
    pub pages: Option<Value>,
    /// `"local"` or a connected processor's name.
    pub processor: String,
    pub autobench: bool,
    pub precision_only: bool,
}

/// One queued or running benchmark.
#[derive(Clone, Debug)]
pub struct BenchRun {
    pub key: String,
    pub bid: String,
    pub machine: String,
    pub row: Generation,
    pub autobench: bool,
    pub precision_only: bool,
    pub pages: i64,
    pub data: Map<String, Value>,
    pub started_mono: Option<f64>,
    pub sent: bool,
}

#[derive(Default, Debug)]
pub struct BenchState {
    /// One FIFO line per machine.
    pub lines: HashMap<String, VecDeque<BenchRun>>,
    /// Global FIFO of keys for `queue.running` / `queue.queued`.
    pub order: Vec<(String, String)>,
    /// The last result of each `(key, machine)`.
    pub recent: indexmap::IndexMap<(String, String), Map<String, Value>>,
    /// Machines whose line holds (and pre-empted) their OCR.
    pub holding: HashMap<String, Vec<Value>>,
}

fn bench_error(status: u16, message: impl Into<String>) -> (u16, Value) {
    (status, json!({"error": message.into(), "row": null, "field": null}))
}

fn is_draft(key: &str) -> bool {
    key.strip_prefix("draft-").is_some_and(|rest| (1..=24).contains(&rest.len()) && rest.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
}

/// `bench_sample_filename`: `[A-Za-z0-9_-]` of the id, at most 80, + `.cbz`.
pub fn bench_sample_filename(bid: &str) -> String {
    let safe: String = bid.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').take(80).collect();
    format!("{}.cbz", if safe.is_empty() { "sample".to_string() } else { safe })
}

fn spec_payload(row: &Generation) -> Value {
    let mut m = Map::new();
    m.insert("engine".into(), json!(row.engine));
    if let Some(d) = &row.detector {
        m.insert("detector".into(), json!(d));
    }
    m.insert("patch_budget".into(), json!(row.patch_budget));
    if row.precision_applies() && row.precision != bunko_core::engines::DEFAULT_PRECISION_MODE {
        m.insert("precision".into(), json!(row.precision));
    }
    m.insert("pools".into(), row.pools.to_value());
    Value::Object(m)
}

/// `build_sample`: up to `pages` story pages spread over the library's volumes,
/// round-robin across series, packed as a stored zip. `(pages, volumes)`.
pub fn build_sample(library: &std::path::Path, out: &std::path::Path, wanted: i64) -> Result<(i64, i64), String> {
    use std::io::Write;
    let mut by_series: std::collections::BTreeMap<String, Vec<std::path::PathBuf>> = Default::default();
    for cbz in crate::ocr::owed::list_archives(library) {
        let series = cbz.parent().and_then(|p| crate::ocr::types::rel_of(library, p)).unwrap_or_default();
        by_series.entry(series).or_default().push(cbz);
    }
    if by_series.is_empty() {
        return Err("there are no volumes in the library to benchmark with \u{2014} upload one first, then the numbers are measured on your own pages".into());
    }
    for list in by_series.values_mut() {
        list.sort_by(|a, b| bunko_sched::job_order::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    }
    let mut ordered = Vec::new();
    let longest = by_series.values().map(Vec::len).max().unwrap_or(0);
    for i in 0..longest {
        for list in by_series.values() {
            if let Some(p) = list.get(i) {
                ordered.push(p.clone());
            }
        }
    }
    let per_archive = 4.max(((wanted as f64) / (ordered.len() as f64)).ceil() as i64);
    let file = std::fs::File::create(out).map_err(|e| format!("could not write the sample: {e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut extracted = 0i64;
    let mut volumes = 0i64;
    let mut readable = false;
    for cbz in ordered {
        if extracted >= wanted {
            break;
        }
        let Ok(volume) = bunko_library::Volume::open(&cbz) else { continue };
        let total = volume.pages().len() as i64;
        if total == 0 {
            continue;
        }
        readable = true;
        let count = per_archive.min(wanted - extracted);
        let picks: Vec<i64> = if count >= total {
            (0..total).collect()
        } else {
            let edge = 2.min((total - count) / 2);
            let span = total - 2 * edge;
            let step = span as f64 / count as f64;
            let mut set: Vec<i64> = (0..count).map(|i| edge + (span - 1).min(((i as f64 + 0.5) * step) as i64)).collect();
            set.sort();
            set.dedup();
            set
        };
        let stem: String = cbz.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let safe: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).take(40).collect();
        let mut took = false;
        for index in picks {
            let page = &volume.pages()[index as usize];
            let suffix = page.path.rfind('.').map(|i| page.path[i..].to_lowercase()).unwrap_or_default();
            let Ok(bytes) = volume.read_page(index as usize) else { continue };
            let name = format!("{extracted:04}_{safe}{suffix}");
            if zip.start_file(name, options).and_then(|_| zip.write_all(&bytes).map_err(Into::into)).is_err() {
                return Err("could not write the sample".into());
            }
            extracted += 1;
            took = true;
        }
        if took {
            volumes += 1;
        }
    }
    zip.finish().map_err(|e| format!("could not write the sample: {e}"))?;
    if !readable {
        return Err("the library's archives have no readable pages to benchmark with".into());
    }
    if extracted == 0 {
        return Err("no pages could be read out of the library's archives to benchmark with".into());
    }
    Ok((extracted, volumes))
}

impl Scheduler {
    fn now_iso(&self) -> String {
        bunko_sched::py::iso_utc(self.now())
    }

    fn queue_value(&self) -> Value {
        let mut keys = self.bench.order.iter().map(|(k, _)| k.clone());
        let running = keys.next();
        json!({"running": running, "queued": keys.collect::<Vec<_>>()})
    }

    /// `enqueue(key, spec, pages, processor, autobench, precision_only)`.
    pub fn bench_enqueue(&mut self, req: BenchRequest) -> Result<Value, (u16, Value)> {
        let machine = if req.processor.is_empty() { LOCAL.to_string() } else { req.processor.clone() };
        let pid = if machine == LOCAL {
            LOCAL.to_string()
        } else {
            match self.machines.values().find(|m| !m.local && m.name == machine && m.connected()) {
                Some(m) => m.pid.clone(),
                None => return Err(bench_error(400, format!("no processor called {} is connected", bunko_sched::py::py_repr(&json!(machine))))),
            }
        };
        let saved = self.settings.rows.iter().find(|r| r.id == req.key).cloned();
        if saved.is_none() && !is_draft(&req.key) {
            return Err(bench_error(400, format!("there is no generation {} to benchmark", bunko_sched::py::py_repr(&json!(req.key)))));
        }
        let row = match &req.spec {
            Some(spec) => match parse_bench_spec(spec) {
                Ok(mut r) => {
                    r.id = req.key.clone();
                    r.name = saved.as_ref().map(|s| s.name.clone()).unwrap_or_else(|| req.key.clone());
                    r.primary = true;
                    r.enabled = true;
                    r
                }
                Err(e) => return Err((400, json!({"error": e.message, "row": null, "field": e.field}))),
            },
            None => match saved {
                Some(r) => r,
                None => {
                    return Err(bench_error(
                        400,
                        format!("there is no generation {} to benchmark \u{2014} send a spec to measure one that is not saved yet", bunko_sched::py::py_repr(&json!(req.key))),
                    ));
                }
            },
        };
        if machine == LOCAL {
            if !self.settings.local_processing || !self.machines.contains_key(LOCAL) {
                return Err(bench_error(400, "this server runs no OCR of its own (ocr.local_processing is off); choose a connected processor to benchmark on"));
            }
            if let Some(refusal) = self.refusal(LOCAL, &row) {
                return Err(bench_error(400, format!("this server cannot run this row: {refusal}")));
            }
        } else if let Some(refusal) = self.refusal(&pid, &row) {
            return Err(bench_error(400, format!("{machine} cannot run this row: {refusal}")));
        }
        let pages = match &req.pages {
            None | Some(Value::Null) => DEFAULT_SAMPLE_PAGES,
            Some(v) if v.is_i64() && (MIN_SAMPLE_PAGES..=MAX_SAMPLE_PAGES).contains(&v.as_i64().unwrap_or(0)) => v.as_i64().unwrap_or(DEFAULT_SAMPLE_PAGES),
            Some(_) => return Err(bench_error(400, "pages must be a whole number between 4 and 512")),
        };
        if crate::ocr::owed::list_archives(&self.library()).is_empty() {
            return Err(bench_error(
                400,
                "there are no volumes in the library to benchmark with \u{2014} upload one first, then the numbers are measured on your own pages",
            ));
        }
        if self.bench.lines.get(&machine).is_some_and(|l| l.iter().any(|r| r.key == req.key)) {
            let on = if machine == LOCAL { String::new() } else { format!("on {machine} ") };
            return Err(bench_error(409, format!("a benchmark of {} is already queued or running {on}\u{2014} re-posting the same row is a no-op", row.name)));
        }
        let bid = format!("bench-{}", crate::ocr::types::token_hex(6));
        let mut data = Map::new();
        data.insert("state".into(), json!("queued"));
        data.insert("key".into(), json!(req.key));
        data.insert("generation".into(), json!(req.key));
        data.insert("processor".into(), json!(machine));
        data.insert("autobench".into(), json!(req.autobench));
        data.insert("precision_only".into(), json!(req.precision_only));
        data.insert("spec".into(), spec_payload(&row));
        data.insert("started_at".into(), json!(self.now_iso()));
        data.insert("finished_at".into(), Value::Null);
        data.insert("waiting_for_queue".into(), json!(true));
        data.insert("sample".into(), Value::Null);
        data.insert("host".into(), self.machines.get(&pid).map_or(Value::Null, |m| m.host_value.clone()));
        data.insert("tunable".into(), json!(!req.precision_only));
        data.insert("progress".into(), Value::Null);
        data.insert("startup_seconds".into(), Value::Null);
        data.insert("trials".into(), json!([]));
        data.insert("baseline".into(), Value::Null);
        data.insert("best".into(), Value::Null);
        data.insert("estimates".into(), Value::Null);
        data.insert("preempted".into(), json!([]));
        data.insert("error".into(), Value::Null);
        let run = BenchRun { key: req.key.clone(), bid, machine: machine.clone(), row, autobench: req.autobench, precision_only: req.precision_only, pages, data, started_mono: None, sent: false };
        self.bench.lines.entry(machine.clone()).or_default().push_back(run);
        self.bench.order.push((req.key.clone(), machine.clone()));
        self.bench_advance(&machine);
        Ok(self.bench_get(&req.key, &machine))
    }

    /// `get(key, processor)`: live → recent → saved (local) → idle.
    pub fn bench_get(&self, key: &str, machine: &str) -> Value {
        let machine = if machine.is_empty() { LOCAL } else { machine };
        if let Some(line) = self.bench.lines.get(machine)
            && let Some(pos) = line.iter().position(|r| r.key == key)
        {
            let mut data = line[pos].data.clone();
            if data.get("state") != Some(&json!("running")) {
                data.insert("progress".into(), Value::Null);
            }
            data.insert("position".into(), json!(pos));
            data.insert("queue".into(), self.queue_value());
            return Value::Object(data);
        }
        let mut found = self.bench.recent.get(&(key.to_string(), machine.to_string())).cloned();
        if found.is_none() && machine == LOCAL && !is_draft(key) {
            found = bunko_sched::bench_file::BenchFile::new(&self.storage()).load().get(key).and_then(Value::as_object).cloned();
        }
        let mut data = found.unwrap_or_else(|| {
            let mut m = Map::new();
            m.insert("state".into(), json!("idle"));
            m.insert("generation".into(), json!(key));
            m.insert("key".into(), json!(key));
            m
        });
        data.insert("position".into(), Value::Null);
        data.insert("queue".into(), self.queue_value());
        Value::Object(data)
    }

    /// `cancel(key, processor)`.
    pub fn bench_cancel(&mut self, key: &str, machine: &str) -> Result<Value, (u16, Value)> {
        let machine = if machine.is_empty() { LOCAL.to_string() } else { machine.to_string() };
        let Some(pos) = self.bench.lines.get(&machine).and_then(|l| l.iter().position(|r| r.key == key)) else {
            return Err(bench_error(400, "there is no benchmark of this generation queued or running to cancel"));
        };
        if pos == 0 && self.bench.lines[&machine][0].sent {
            let bid = self.bench.lines[&machine][0].bid.clone();
            if let Some(m) = self.machine_by_name(&machine) {
                m.send(Op::Cancel { sid: None, claim: None, bid: Some(bid) });
            }
        }
        self.bench_finish(&machine, pos, "cancelled", None);
        Ok(self.bench_get(key, &machine))
    }

    /// `paused_for_benchmark()`: the head of the global line.
    pub fn paused_for_benchmark(&self) -> Option<Value> {
        let (key, machine) = self.bench.order.first()?;
        let generation = self.settings.rows.iter().find(|r| &r.id == key).map(|r| r.name.clone()).unwrap_or_else(|| key.clone());
        Some(json!({"key": key, "generation": generation, "queued": self.bench.order.len() - 1, "processor": machine}))
    }

    /// `configuring()`: `{machine: {key, generation, auto}}` for each line's head.
    pub fn configuring(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for (machine, line) in &self.bench.lines {
            if let Some(run) = line.front() {
                let generation = self.settings.rows.iter().find(|r| r.id == run.key).map(|r| r.name.clone()).unwrap_or_else(|| run.key.clone());
                out.insert(machine.clone(), json!({"key": run.key, "generation": generation, "auto": run.autobench}));
            }
        }
        out
    }

    // --- holds ----------------------------------------------------------------------------

    /// `hold_queue(machine)`: counted; nothing is killed, sessions drain.
    pub fn hold_queue(&mut self, machine: &str) {
        *self.holds.entry(machine.to_string()).or_insert(0) += 1;
        self.bump();
        self.bump_page();
    }

    /// `release_queue(machine)`.
    pub fn release_queue(&mut self, machine: &str) {
        if let Some(n) = self.holds.get_mut(machine) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.holds.remove(machine);
            }
        }
        self.bump();
        self.bump_page();
        for lane in &mut self.lanes {
            lane.idle_at = None;
        }
        self.maybe_start_scan();
    }

    /// `preempt_for_bench(machine)`: hold, and cancel everything running there now
    /// (visible to other machines at once). `[{generation, volume}]` pre-empted.
    pub fn preempt_for_bench(&mut self, machine: &str) -> Vec<Value> {
        self.hold_queue(machine);
        let mut jobs: Vec<(Job, String)> = self.claims.iter().filter(|(_, c)| c.machine == machine && !c.settling).map(|(j, c)| (j.clone(), c.row.name.clone())).collect();
        jobs.sort_by(|a, b| (&a.0.rel, &a.0.gid).cmp(&(&b.0.rel, &b.0.gid)));
        for (job, _) in &jobs {
            self.cancelled.insert(job.clone());
            self.attempted.remove(job);
        }
        let sids: Vec<String> = self.sessions.iter().filter(|(_, s)| s.machine == machine).map(|(k, _)| k.clone()).collect();
        for sid in sids {
            self.kill_session(&sid, None);
        }
        jobs.into_iter().map(|(j, name)| json!({"generation": name, "volume": j.volume()})).collect()
    }

    // --- the line -------------------------------------------------------------------------

    /// Start the head of a machine's line if it is not running yet.
    fn bench_advance(&mut self, machine: &str) {
        let Some(run) = self.bench.lines.get(machine).and_then(|l| l.front()).cloned() else {
            if self.bench.holding.remove(machine).is_some() {
                self.release_queue(machine);
            }
            return;
        };
        if run.sent {
            return;
        }
        if !self.bench.holding.contains_key(machine) {
            let preempted = self.preempt_for_bench(machine);
            self.bench.holding.insert(machine.to_string(), preempted.clone());
            if let Some(head) = self.bench.lines.get_mut(machine).and_then(|l| l.front_mut()) {
                head.data.insert("preempted".into(), Value::Array(preempted));
            }
        }
        let Some(target) = self.machine_by_name(machine).map(|m| (m.pid.clone(), m.local)) else {
            self.bench_finish(machine, 0, "failed", Some(format!("{machine} is not connected")));
            return;
        };
        let sample_path = self.storage().join(".processing").join(bench_sample_filename(&run.bid));
        if let Some(dir) = sample_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let built = build_sample(&self.library(), &sample_path, run.pages);
        let (pages, volumes) = match built {
            Ok(b) => b,
            Err(e) => {
                self.bench_finish(machine, 0, "failed", Some(e));
                return;
            }
        };
        let spec = self.row_spec(machine, &run.row);
        let sample = if target.1 { sample_path.to_string_lossy().into_owned() } else { format!("{}/{}/bench/{}/sample", bunko_proto::PROCESSOR_ROOT, target.0, run.bid) };
        let op = Op::Bench(BenchOp { bid: run.bid.clone(), spec, sample, pages: pages as u32, precision_only: run.precision_only });
        let sent = self.machines.get(&target.0).is_some_and(|m| m.send(op));
        let mono = self.mono();
        if let Some(head) = self.bench.lines.get_mut(machine).and_then(|l| l.front_mut()) {
            head.sent = sent;
            head.started_mono = Some(mono);
            head.data.insert("state".into(), json!("running"));
            head.data.insert("waiting_for_queue".into(), json!(false));
            head.data.insert("sample".into(), json!({"pages": pages, "volumes": volumes}));
        }
        if !sent {
            self.bench_finish(machine, 0, "failed", Some(format!("{machine} disconnected")));
        }
    }

    /// End the run at `pos` of a machine's line.
    fn bench_finish(&mut self, machine: &str, pos: usize, state: &str, error: Option<String>) {
        let Some(mut run) = self.bench.lines.get_mut(machine).and_then(|l| l.remove(pos)) else { return };
        if let Some(i) = self.bench.order.iter().position(|(k, m)| *k == run.key && m == machine) {
            self.bench.order.remove(i);
        }
        let _ = std::fs::remove_file(self.storage().join(".processing").join(bench_sample_filename(&run.bid)));
        run.data.insert("state".into(), json!(state));
        run.data.insert("finished_at".into(), json!(self.now_iso()));
        run.data.insert("progress".into(), Value::Null);
        run.data.insert("waiting_for_queue".into(), json!(false));
        run.data.insert("error".into(), error.map_or(Value::Null, Value::String));
        if state == "done" {
            self.bench_store(&run);
        }
        self.bench.recent.insert((run.key.clone(), machine.to_string()), run.data.clone());
        let drafts: Vec<(String, String)> = self.bench.recent.keys().filter(|(k, _)| is_draft(k)).cloned().collect();
        if drafts.len() > MAX_DRAFT_RESULTS
            && let Some(oldest) = drafts.first()
        {
            self.bench.recent.shift_remove(oldest);
        }
        if run.autobench {
            self.autobench_settled(machine, &run.row.id, state);
        }
        self.bump_page();
        if pos == 0 {
            self.bench_advance(machine);
        }
    }

    /// Persist a finished result: `.ocr-bench.json` (local, saved row, not precision-only)
    /// and the machine's profile (remote benches and every autobench).
    fn bench_store(&mut self, run: &BenchRun) {
        let pps = run.data.get("best").and_then(|b| b.get("pages_per_second")).and_then(Value::as_f64).or_else(|| run.data.get("baseline").and_then(|b| b.get("pages_per_second")).and_then(Value::as_f64));
        let saved = self.settings.rows.iter().any(|r| r.id == run.key);
        if run.machine == LOCAL && saved && !run.precision_only {
            let ids: Vec<String> = self.settings.rows.iter().map(|r| r.id.clone()).collect();
            let mut result = run.data.clone();
            result.insert("state".into(), json!("done"));
            if let Err(e) = bunko_sched::bench_file::BenchFile::new(&self.storage()).save(&run.key, result, &ids) {
                tracing::warn!("could not save the benchmark: {e}");
            }
        }
        if saved && (run.machine != LOCAL || run.autobench) {
            let mut bench = Map::new();
            bench.insert("pages_per_second".into(), json!(pps));
            for k in ["startup_seconds", "host", "precision", "precision_mode", "precision_trials", "precision_why"] {
                if let Some(v) = run.data.get(k).filter(|v| !v.is_null()) {
                    bench.insert(k.into(), v.clone());
                }
            }
            bench.insert("at".into(), json!(self.now_iso()));
            self.profiles.set_bench(profile_key(&run.machine), &run.key, &bench, Some(&run.row.output_affecting()));
            if run.autobench
                && !run.precision_only
                && let Some(best) = run.data.get("best").and_then(Value::as_object)
                && best.get("same_as_spec") != Some(&json!(true))
            {
                let mut pools = Map::new();
                for t in crate::ocr::profiles::POOL_TABLES {
                    pools.insert(t.into(), best.get(t).cloned().unwrap_or(json!({})));
                }
                self.profiles.set_pools(profile_key(&run.machine), &run.key, &pools, Some(&run.row.output_affecting()), true, true);
            }
        }
    }

    /// The `bench_*` events (and `fatal`/`exit` of a `bench-…` id).
    pub fn bench_event(&mut self, pid: &str, event: Event) {
        let Some(machine) = self.machines.get(pid).map(|m| m.name.clone()) else { return };
        let bid = match &event {
            Event::BenchReady { bid, .. } | Event::BenchProgress { bid, .. } | Event::BenchTrial { bid, .. } | Event::BenchDone { bid, .. } => bid.clone(),
            other => other.sid().unwrap_or_default().to_string(),
        };
        let Some(pos) = self.bench.lines.get(&machine).and_then(|l| l.iter().position(|r| r.bid == bid)) else { return };
        let mut finish: Option<(&str, Option<String>)> = None;
        {
            let Some(run) = self.bench.lines.get_mut(&machine).and_then(|l| l.get_mut(pos)) else { return };
            match event {
                Event::BenchReady { detail, .. } => {
                    for k in ["startup_seconds", "tunable"] {
                        if let Some(v) = detail.get(k) {
                            run.data.insert(k.into(), v.clone());
                        }
                    }
                    if let (Some(Value::Object(host)), Some(sd)) = (run.data.get_mut("host"), detail.get("stage_device")) {
                        host.insert("devices".into(), sd.clone());
                    }
                }
                Event::BenchProgress { detail, .. } => {
                    run.data.insert("progress".into(), json!(detail));
                }
                Event::BenchTrial { detail, .. } => {
                    if let Some(Value::Array(trials)) = run.data.get_mut("trials") {
                        trials.push(json!(detail));
                    }
                }
                Event::BenchDone { detail, .. } => {
                    for (k, v) in detail {
                        if k != "bid" {
                            run.data.insert(k, v);
                        }
                    }
                    let pps = run.data.get("best").and_then(|b| b.get("pages_per_second")).and_then(Value::as_f64);
                    if let Some(pps) = pps.filter(|p| *p > 0.0) {
                        run.data.insert("estimates".into(), json!({"volume_200_pages_seconds": (BENCH_VOLUME_PAGES / pps).round() as i64, "remaining_pages": null, "remaining_seconds": null}));
                    }
                    let error = run.data.get("error").and_then(Value::as_str).map(str::to_string);
                    finish = Some(if error.is_some() { ("failed", error) } else { ("done", None) });
                }
                Event::Fatal { error, .. } | Event::SpawnFailed { error, .. } => finish = Some(("failed", Some(error))),
                Event::Exit { returncode, .. } => {
                    let name = run.row.name.clone();
                    let code = returncode.filter(|c| *c != 0).map(|c| format!(" with status {c}")).unwrap_or_default();
                    finish = Some(("failed", Some(format!("the {name} benchmark ended{code} before it produced a result"))));
                }
                _ => {}
            }
        }
        self.bump_page();
        if let Some((state, error)) = finish {
            self.bench_finish(&machine, pos, state, error);
        }
    }

    /// The 30 s look at running benchmarks: give up on a silent one.
    pub fn bench_tick(&mut self) {
        let now = self.mono();
        let overdue: Vec<String> = self
            .bench
            .lines
            .iter()
            .filter_map(|(m, l)| l.front().filter(|r| r.started_mono.is_some_and(|t| now - t > BENCH_BUDGET_SECONDS + BENCH_SLACK_SECONDS)).map(|_| m.clone()))
            .collect();
        for machine in overdue {
            if let Some(bid) = self.bench.lines.get(&machine).and_then(|l| l.front()).map(|r| r.bid.clone())
                && let Some(m) = self.machine_by_name(&machine)
            {
                m.send(Op::Cancel { sid: None, claim: None, bid: Some(bid) });
            }
            self.bench_finish(&machine, 0, "failed", Some("the benchmark ran past its time budget and was stopped".into()));
        }
    }

    /// A machine left: its line ends (autobench pairs may be asked again).
    pub fn bench_machine_left(&mut self, machine: &str) {
        let Some(line) = self.bench.lines.remove(machine) else { return };
        self.bench.order.retain(|(_, m)| m != machine);
        for mut run in line {
            run.data.insert("state".into(), json!("failed"));
            run.data.insert("error".into(), json!(format!("{machine} disconnected")));
            self.bench.recent.insert((run.key.clone(), machine.to_string()), run.data.clone());
            if run.autobench {
                let key = (profile_key(machine).to_string(), run.row.id.clone());
                self.autobench.inflight.remove(&key);
                self.autobench.asked.remove(&key);
            }
        }
        if self.bench.holding.remove(machine).is_some() {
            self.release_queue(machine);
        }
    }

    /// `_autobench_settled`.
    fn autobench_settled(&mut self, machine: &str, gid: &str, state: &str) {
        let key = (profile_key(machine).to_string(), gid.to_string());
        self.autobench.inflight.remove(&key);
        if state != "done" {
            let left = machine != LOCAL && self.machine_by_name(machine).is_none();
            if left {
                self.autobench.asked.remove(&key);
            } else {
                self.autobench.failed.insert(key);
            }
        }
        self.bump();
        for lane in &mut self.lanes {
            lane.idle_at = None;
        }
    }

    /// `_drain_autobench_requests`: fire what claims asked for.
    pub fn drain_autobench_requests(&mut self) {
        let wanted = std::mem::take(&mut self.autobench.wanted);
        for (profile, gid) in wanted {
            let machine = if profile == crate::ocr::profiles::LOCAL_PROFILE { LOCAL.to_string() } else { profile.clone() };
            let pid = self.machine_by_name(&machine).map(|m| m.pid.clone());
            let row = self.row(&gid).cloned();
            let (Some(pid), Some(row)) = (pid, row) else {
                self.autobench.inflight.remove(&(profile, gid));
                continue;
            };
            if self.autobench_kind(&pid, &row).is_none() {
                self.autobench.inflight.remove(&(profile, gid));
                continue;
            }
            let label = self.machines.get(&pid).map(|m| m.label()).unwrap_or_default();
            self.log(format!("Benchmarking {} on {label} before it runs there", row.name));
            let req = BenchRequest { key: gid.clone(), spec: None, pages: None, processor: machine.clone(), autobench: true, precision_only: false };
            if let Err((_, body)) = self.bench_enqueue(req) {
                tracing::info!("benchmark of {} on {label} refused: {}", row.name, body["error"]);
                self.autobench_settled(&machine, &gid, "refused");
            }
        }
    }

    /// The bench a sample URL serves, if `pid` owns it: its file.
    pub fn bench_sample(&self, pid: &str, bid: &str) -> Result<std::path::PathBuf, (u16, &'static str)> {
        let Some(machine) = self.machines.get(pid) else { return Err((404, "No such processor")) };
        let live = self.bench.lines.get(&machine.name).is_some_and(|l| l.iter().any(|r| r.bid == bid && r.sent));
        if !bid.starts_with("bench-") || !live {
            return Err((404, "No such sample"));
        }
        Ok(self.storage().join(".processing").join(bench_sample_filename(bid)))
    }
}

impl Scheduler {
    /// For tests and the admin: queue a message to run after the current one.
    pub fn defer(&mut self, msg: Msg) {
        self.internal.push_back(msg);
    }
}
