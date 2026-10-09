//! Benchmarks: the queue, hold and pre-empt contract and the stored results (spec
//! ocr-generations-bench §6; 0.5.2 `ocr/bench.py` `BenchService`). The measurement
//! itself runs on a processor — this server's own in-process one or a remote one —
//! answering a `bench` op (`bunko_processor::bench`); the library builds the sample,
//! holds and pre-empts that machine's OCR while its line runs, follows the `bench_*`
//! events, completes the result the way `_read_composed` did and stores it.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use bunko_core::generations::{Generation, parse_bench_spec};
use bunko_proto::{BenchOp, Event, Op, PoolsSpec, RowSpec};
use bunko_sched::precision as policy;
use bunko_sched::py::as_float;
use serde_json::{Map, Value, json};

use super::claim::catalog_can_run;
use super::{Msg, Scheduler};
use crate::ocr::profiles::{POOL_AUTO, profile_key};
use crate::ocr::types::{Job, LOCAL};

pub const BENCH_MAX_TRIALS: i64 = 8;
pub const BENCH_BUDGET_SECONDS: f64 = 900.0;
pub const BENCH_SLACK_SECONDS: f64 = 1800.0;
pub const DEFAULT_SAMPLE_PAGES: i64 = 32;
pub const MIN_SAMPLE_PAGES: i64 = 4;
pub const MAX_SAMPLE_PAGES: i64 = 512;
pub const MAX_DRAFT_RESULTS: usize = 32;
/// The volume the estimates are quoted for (`BENCH_VOLUME_PAGES`, mirrored in admin.js).
const BENCH_VOLUME_PAGES: f64 = 200.0;
const EMPTY_LIBRARY: &str = "there are no volumes in the library to benchmark with \u{2014} upload one first, then the numbers are measured on your own pages";

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
    /// What is measured: the parsed spec, or the saved row.
    pub row: Generation,
    pub draft: bool,
    pub autobench: bool,
    pub precision_only: bool,
    pub pages: i64,
    pub data: Map<String, Value>,
    pub started_mono: Option<f64>,
    /// The sample is being packed on a helper thread.
    pub building: bool,
    pub sent: bool,
    /// The processor's `fatal`, said before its `exit`.
    pub fatal: Option<String>,
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
    (
        status,
        json!({"error": message.into(), "row": null, "field": null}),
    )
}

/// `DRAFT_KEY_RE`: `^draft-[a-z0-9-]{1,24}\Z`.
pub fn is_draft(key: &str) -> bool {
    key.strip_prefix("draft-").is_some_and(|rest| {
        (1..=24).contains(&rest.len())
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

/// `bench_sample_filename`: `[A-Za-z0-9_-]` of the id, at most 80, + `.cbz`.
pub fn bench_sample_filename(bid: &str) -> String {
    let safe: String = bid
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(80)
        .collect();
    format!(
        "{}.cbz",
        if safe.is_empty() {
            "sample".to_string()
        } else {
            safe
        }
    )
}

/// `_spec_payload`: the recipe a benchmark measured, never the row's identity.
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

/// The measured row as the `bench` op carries it: its own pools (the processor strips
/// the widths and capacities unless the run is precision-only), no pick.
fn bench_spec(row: &Generation) -> RowSpec {
    RowSpec {
        id: row.id.clone(),
        name: row.name.clone(),
        engine: row.engine.clone(),
        detector: row.detector.clone(),
        patch_budget: row.patch_budget,
        precision: row.precision.clone(),
        pools: PoolsSpec {
            stage_workers: row.pools.stage_workers.clone(),
            queue_capacity: row.pools.queue_capacity.clone(),
            stage_device: row.pools.stage_device.clone(),
        },
        precision_pick: None,
        precision_why: String::new(),
        primary: row.primary,
    }
}

/// `cpu_label()`: the CPU as a person would name it, with its core count (the admin
/// sizes a machine's worker budget from that `(N cores)` suffix).
pub fn cpu_label(cpu: &str, cores: Option<u32>) -> String {
    match cores {
        Some(n) if n > 0 && !cpu.contains(" core") => {
            format!("{cpu} ({n} core{})", if n == 1 { "" } else { "s" })
        }
        _ => cpu.to_string(),
    }
}

/// `_physical_cores()`: distinct (physical id, core id) pairs of `/proc/cpuinfo`, else
/// the logical count — cores, not threads, what a pool width competes for.
pub fn physical_cores() -> Option<u32> {
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut pairs = std::collections::BTreeSet::new();
    let mut physical: Option<String> = None;
    for line in text.lines() {
        let value = || line.split_once(':').map(|(_, v)| v.trim().to_string());
        if line.starts_with("physical id") {
            physical = value();
        } else if line.starts_with("core id")
            && let (Some(p), Some(c)) = (physical.clone(), value())
        {
            pairs.insert((p, c));
        }
    }
    if !pairs.is_empty() {
        return Some(pairs.len() as u32);
    }
    std::thread::available_parallelism()
        .ok()
        .map(|n| n.get() as u32)
}

/// Is there any `.cbz` under the library? (Stops at the first.)
fn has_archive(library: &Path) -> bool {
    let mut stack = vec![library.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if bunko_library::sidecar::is_cbz_name(&entry.file_name().to_string_lossy()) {
                return true;
            }
        }
    }
    false
}

/// `build_sample`: up to `pages` story pages spread over the library's volumes,
/// round-robin across series, packed as a stored zip. `(pages, volumes)`.
pub fn build_sample(library: &Path, out: &Path, wanted: i64) -> Result<(i64, i64), String> {
    use std::io::Write;
    let mut by_series: std::collections::BTreeMap<String, Vec<PathBuf>> = Default::default();
    for cbz in crate::ocr::owed::list_archives(library) {
        let series = cbz
            .parent()
            .and_then(|p| crate::ocr::types::rel_of(library, p))
            .unwrap_or_default();
        by_series.entry(series).or_default().push(cbz);
    }
    if by_series.is_empty() {
        return Err(EMPTY_LIBRARY.into());
    }
    for list in by_series.values_mut() {
        list.sort_by(|a, b| {
            let name = |p: &PathBuf| {
                p.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            };
            bunko_sched::job_order::natural_cmp(&name(a), &name(b))
        });
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
    let wanted = wanted.clamp(MIN_SAMPLE_PAGES, MAX_SAMPLE_PAGES);
    let per_archive = 4.max(((wanted as f64) / (ordered.len() as f64)).ceil() as i64);
    let file =
        std::fs::File::create(out).map_err(|e| format!("could not write the sample: {e}"))?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let mut extracted = 0i64;
    let mut volumes = 0i64;
    let mut readable = false;
    for cbz in ordered {
        if extracted >= wanted {
            break;
        }
        let Ok(volume) = bunko_library::Volume::open(&cbz) else {
            continue;
        };
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
            let mut set: Vec<i64> = (0..count)
                .map(|i| edge + (span - 1).min(((i as f64 + 0.5) * step) as i64))
                .collect();
            set.sort();
            set.dedup();
            set
        };
        let stem: String = cbz
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        // `[^A-Za-z0-9._-]+` → `_`, first 40 characters.
        let mut safe = String::new();
        let mut in_run = false;
        for c in stem.chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                safe.push(c);
                in_run = false;
            } else if !in_run {
                safe.push('_');
                in_run = true;
            }
        }
        let safe: String = safe.chars().take(40).collect();
        let mut took = false;
        for index in picks {
            let page = &volume.pages()[index as usize];
            let suffix = page
                .path
                .rfind('.')
                .map(|i| page.path[i..].to_lowercase())
                .unwrap_or_default();
            let Ok(bytes) = volume.read_page(index as usize) else {
                continue;
            };
            let name = format!("{extracted:04}_{safe}{suffix}");
            if zip
                .start_file(name, options)
                .and_then(|_| zip.write_all(&bytes).map_err(Into::into))
                .is_err()
            {
                return Err("could not write the sample".into());
            }
            extracted += 1;
            took = true;
        }
        if took {
            volumes += 1;
        }
    }
    zip.finish()
        .and_then(|mut w| w.flush().map_err(Into::into))
        .map_err(|e| format!("could not write the sample: {e}"))?;
    if !readable {
        return Err("the library's archives have no readable pages to benchmark with".into());
    }
    if extracted == 0 {
        return Err(
            "no pages could be read out of the library's archives to benchmark with".into(),
        );
    }
    Ok((extracted, volumes))
}

fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `_workers_to_apply` / `_capacity_to_apply`: the runner's table, then each width the
/// spec pins and the search left alone — the pin when the winning trial ran at it,
/// `"auto"` otherwise (a benchmark never persists what it did not measure).
fn table_to_apply(
    best: Option<&Value>,
    pins: &std::collections::BTreeMap<String, u32>,
    ran: Option<&Value>,
) -> Map<String, Value> {
    let mut table: Map<String, Value> = best
        .and_then(Value::as_object)
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| int_of(v).map(|n| (k.clone(), json!(n))))
                .collect()
        })
        .unwrap_or_default();
    for (key, pin) in pins {
        if table.contains_key(key) {
            continue;
        }
        let same = ran
            .and_then(|r| r.get(key))
            .and_then(int_of)
            .is_some_and(|n| n == i64::from(*pin));
        table.insert(
            key.clone(),
            if same { json!(pin) } else { json!(POOL_AUTO) },
        );
    }
    table
}

/// `_placement_to_apply`: the spec's pins (a pin the run did not honour replaced by
/// where it ran), then what the search moved.
fn placement_to_apply(
    pins: &std::collections::BTreeMap<String, String>,
    placed: Option<&Value>,
    moved: Option<&Value>,
) -> Map<String, Value> {
    let mut table = Map::new();
    for (key, pin) in pins {
        let ran = placed.and_then(|p| p.get(key)).and_then(Value::as_str);
        let explicit = !pin.is_empty() && pin != "auto";
        let value = match ran {
            Some(r) if explicit && !r.is_empty() && r != pin => r.to_string(),
            _ => pin.clone(),
        };
        table.insert(key.clone(), json!(value));
    }
    if let Some(Value::Object(m)) = moved {
        for (k, v) in m {
            table.insert(k.clone(), json!(v.as_str().unwrap_or_default()));
        }
    }
    table
}

/// `_same_as_spec`: applying `best` would change nothing.
fn same_as_spec(row: &Generation, best: &Map<String, Value>) -> bool {
    use std::collections::BTreeMap;
    let ints = |t: &BTreeMap<String, u32>| -> BTreeMap<String, Value> {
        t.iter().map(|(k, v)| (k.clone(), json!(v))).collect()
    };
    let norm = |v: Option<&Value>| -> BTreeMap<String, Value> {
        v.and_then(Value::as_object)
            .map(|t| {
                t.iter()
                    .map(|(k, v)| {
                        let v = if v.as_str() == Some(POOL_AUTO) {
                            v.clone()
                        } else {
                            int_of(v).map_or(v.clone(), |n| json!(n))
                        };
                        (k.clone(), v)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let devices: BTreeMap<String, Value> = row
        .pools
        .stage_device
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    let placed: BTreeMap<String, Value> = best
        .get("stage_device")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    norm(best.get("stage_workers")) == ints(&row.pools.stage_workers)
        && norm(best.get("queue_capacity")) == ints(&row.pools.queue_capacity)
        && placed == devices
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

    /// A saved row's NAME, or the key itself (a draft has no row).
    fn bench_display_name(&self, key: &str) -> String {
        self.settings
            .rows
            .iter()
            .find(|r| r.id == key)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| key.to_string())
    }

    /// `enqueue(key, spec, pages, processor, autobench, precision_only)`: validate
    /// everything that can be said at once, then queue on that machine's line.
    pub fn bench_enqueue(&mut self, req: BenchRequest) -> Result<Value, (u16, Value)> {
        let machine = if req.processor.is_empty() {
            LOCAL.to_string()
        } else {
            req.processor.clone()
        };
        let pid = if machine == LOCAL {
            LOCAL.to_string()
        } else {
            match self
                .machines
                .values()
                .find(|m| !m.local && m.name == machine && m.connected())
            {
                Some(m) => m.pid.clone(),
                None => {
                    return Err(bench_error(
                        400,
                        format!(
                            "no processor called {} is connected",
                            bunko_sched::py::py_repr(&json!(machine))
                        ),
                    ));
                }
            }
        };
        if self.machine_paused(&pid) {
            return Err(bench_error(
                409,
                format!(
                    "{} is paused by its owner; benchmarks run once it resumes",
                    if machine == LOCAL {
                        "this server's OCR"
                    } else {
                        machine.as_str()
                    }
                ),
            ));
        }
        let saved = self.settings.rows.iter().find(|r| r.id == req.key).cloned();
        let draft = is_draft(&req.key);
        if saved.is_none() && !draft {
            return Err(bench_error(
                400,
                format!(
                    "there is no generation {} to benchmark",
                    bunko_sched::py::py_repr(&json!(req.key))
                ),
            ));
        }
        let row = match &req.spec {
            Some(spec) => match parse_bench_spec(spec) {
                Ok(mut r) => {
                    r.id = req.key.clone();
                    r.name = saved
                        .as_ref()
                        .map(|s| s.name.clone())
                        .unwrap_or_else(|| req.key.clone());
                    r.primary = saved.as_ref().is_some_and(|s| s.primary);
                    r.enabled = true;
                    r
                }
                Err(e) => {
                    return Err((
                        400,
                        json!({"error": e.message, "row": null, "field": e.field}),
                    ));
                }
            },
            None => match saved {
                Some(r) => r,
                None => {
                    return Err(bench_error(
                        400,
                        format!(
                            "there is no generation {} to benchmark \u{2014} send a spec to measure one that is not saved yet",
                            bunko_sched::py::py_repr(&json!(req.key))
                        ),
                    ));
                }
            },
        };
        // Judged on the MEASURED row's own placement, against that machine's catalog.
        let stage_device: Map<String, Value> = row
            .pools
            .stage_device
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect();
        if machine == LOCAL {
            if !self.settings.local_processing || !self.machines.contains_key(LOCAL) {
                return Err(bench_error(
                    400,
                    "this server runs no OCR of its own (ocr.local_processing is off); choose a connected processor to benchmark on",
                ));
            }
            let refused = self
                .machines
                .get(LOCAL)
                .and_then(|m| catalog_can_run(&m.catalog, &row, &stage_device));
            if let Some(refusal) = refused {
                return Err(bench_error(
                    400,
                    format!("this server cannot run this row: {refusal}"),
                ));
            }
        } else {
            let refused = self.machines.get(&pid).and_then(|m| {
                if m.installing() {
                    Some("it is still installing".to_string())
                } else {
                    catalog_can_run(&m.catalog, &row, &stage_device)
                }
            });
            if let Some(refusal) = refused {
                return Err(bench_error(
                    400,
                    format!("{machine} cannot run this row: {refusal}"),
                ));
            }
        }
        let pages = match &req.pages {
            None | Some(Value::Null) => DEFAULT_SAMPLE_PAGES,
            Some(v)
                if v.is_i64()
                    && (MIN_SAMPLE_PAGES..=MAX_SAMPLE_PAGES).contains(&v.as_i64().unwrap_or(0)) =>
            {
                v.as_i64().unwrap_or(DEFAULT_SAMPLE_PAGES)
            }
            Some(_) => {
                return Err(bench_error(
                    400,
                    "pages must be a whole number between 4 and 512",
                ));
            }
        };
        if !has_archive(&self.library()) {
            return Err(bench_error(400, EMPTY_LIBRARY));
        }
        if self
            .bench
            .lines
            .get(&machine)
            .is_some_and(|l| l.iter().any(|r| r.key == req.key))
        {
            let on = if machine == LOCAL {
                String::new()
            } else {
                format!("on {machine} ")
            };
            return Err(bench_error(
                409,
                format!(
                    "a benchmark of {} is already queued or running {on}\u{2014} re-posting the same row is a no-op",
                    self.bench_display_name(&req.key)
                ),
            ));
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
        data.insert("host".into(), Value::Null);
        // No monolithic engine is left: every row has a pipeline (the runner says
        // otherwise on `bench_ready`).
        data.insert("tunable".into(), json!(true));
        data.insert("progress".into(), Value::Null);
        data.insert("startup_seconds".into(), Value::Null);
        data.insert("trials".into(), json!([]));
        data.insert("baseline".into(), Value::Null);
        data.insert("best".into(), Value::Null);
        for k in [
            "precision",
            "precision_mode",
            "precision_trials",
            "precision_why",
            "peak_rss_mb",
            "peak_vram_mb",
            "estimates",
        ] {
            data.insert(k.into(), Value::Null);
        }
        data.insert("preempted".into(), json!([]));
        data.insert("error".into(), Value::Null);
        let run = BenchRun {
            key: req.key.clone(),
            bid,
            machine: machine.clone(),
            row,
            draft,
            autobench: req.autobench,
            precision_only: req.precision_only,
            pages,
            data,
            started_mono: None,
            building: false,
            sent: false,
            fatal: None,
        };
        self.bench
            .lines
            .entry(machine.clone())
            .or_default()
            .push_back(run);
        self.bench.order.push((req.key.clone(), machine.clone()));
        self.bump_page();
        self.bench_advance(&machine);
        Ok(self.bench_get(&req.key, &machine))
    }

    /// `get(key, processor)`: live → recent → saved (local, saved rows) → idle.
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
        let mut found = self
            .bench
            .recent
            .get(&(key.to_string(), machine.to_string()))
            .cloned();
        if found.is_none() && machine == LOCAL && !is_draft(key) {
            found = bunko_sched::bench_file::BenchFile::new(&self.storage())
                .load()
                .get(key)
                .and_then(Value::as_object)
                .cloned();
        }
        let Some(mut data) = found else {
            return json!({"state": "idle", "generation": key, "key": key, "queue": self.queue_value()});
        };
        data.insert("position".into(), Value::Null);
        data.insert("queue".into(), self.queue_value());
        Value::Object(data)
    }

    /// `cancel(key, processor)`: a queued run leaves at once; the running one is told
    /// to stop and settles as cancelled (nothing it says afterwards is read).
    pub fn bench_cancel(&mut self, key: &str, machine: &str) -> Result<Value, (u16, Value)> {
        let machine = if machine.is_empty() {
            LOCAL.to_string()
        } else {
            machine.to_string()
        };
        let Some(pos) = self
            .bench
            .lines
            .get(&machine)
            .and_then(|l| l.iter().position(|r| r.key == key))
        else {
            return Err(bench_error(
                400,
                "there is no benchmark of this generation queued or running to cancel",
            ));
        };
        if pos == 0 && self.bench.lines[&machine][0].sent {
            let bid = self.bench.lines[&machine][0].bid.clone();
            if let Some(m) = self.machine_by_name(&machine) {
                m.send(Op::Cancel {
                    sid: None,
                    claim: None,
                    bid: Some(bid),
                });
            }
        }
        self.bench_finish(&machine, pos, "cancelled", None);
        Ok(self.bench_get(key, &machine))
    }

    /// `paused_for_benchmark()`: the head of the global line.
    pub fn paused_for_benchmark(&self) -> Option<Value> {
        let (key, machine) = self.bench.order.first()?;
        Some(json!({
            "key": key,
            "generation": self.bench_display_name(key),
            "queued": self.bench.order.len() - 1,
            "processor": machine,
        }))
    }

    /// `configuring()`: `{machine: {key, generation, auto}}` for each line's head.
    pub fn configuring(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for (machine, line) in &self.bench.lines {
            if let Some(run) = line.front() {
                out.insert(
                    machine.clone(),
                    json!({"key": run.key, "generation": self.bench_display_name(&run.key), "auto": run.autobench}),
                );
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
        let mut jobs: Vec<(Job, String)> = self
            .claims
            .iter()
            .filter(|(_, c)| c.machine == machine && !c.settling)
            .map(|(j, c)| (j.clone(), c.row.name.clone()))
            .collect();
        jobs.sort_by(|a, b| (&a.0.rel, &a.0.gid).cmp(&(&b.0.rel, &b.0.gid)));
        for (job, _) in &jobs {
            self.cancelled.insert(job.clone());
            self.attempted.remove(job);
        }
        let sids: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.machine == machine)
            .map(|(k, _)| k.clone())
            .collect();
        for sid in sids {
            self.kill_session(&sid, None);
        }
        jobs.into_iter()
            .map(|(j, name)| json!({"generation": name, "volume": j.volume()}))
            .collect()
    }

    // --- the line -------------------------------------------------------------------------

    fn sample_path(&self, bid: &str) -> PathBuf {
        self.storage()
            .join(".processing")
            .join(bench_sample_filename(bid))
    }

    /// The host a benchmark is about: this server's `{cpu, gpu, backend}`
    /// (`describe_host`), or the processor's registered host.
    fn bench_host(&self, machine: &str) -> Value {
        match self.machine_by_name(machine) {
            Some(m) if m.local => json!({
                // This server's own cores: physical, as 0.5.2 counted them.
                "cpu": cpu_label(&m.host.cpu, physical_cores()),
                "gpu": m.host.gpu.as_deref().filter(|g| !g.is_empty()),
                "backend": if m.host.backend.is_empty() { Value::Null } else { json!(m.host.backend) },
            }),
            Some(m) => m.host_value.clone(),
            None => json!({}),
        }
    }

    fn head_mut(&mut self, machine: &str) -> Option<&mut BenchRun> {
        self.bench
            .lines
            .get_mut(machine)
            .and_then(|l| l.front_mut())
    }

    /// Start the head of a machine's line: hold the machine (once per line), then pack
    /// the sample on a helper thread; [`Scheduler::bench_sample_built`] sends the op.
    fn bench_advance(&mut self, machine: &str) {
        let Some(run) = self
            .bench
            .lines
            .get(machine)
            .and_then(|l| l.front())
            .cloned()
        else {
            if self.bench.holding.remove(machine).is_some() {
                self.release_queue(machine);
            }
            return;
        };
        if run.sent || run.building {
            return;
        }
        if !self.bench.holding.contains_key(machine) {
            let preempted = self.preempt_for_bench(machine);
            self.bench
                .holding
                .insert(machine.to_string(), preempted.clone());
            if let Some(head) = self.head_mut(machine) {
                head.data
                    .insert("preempted".into(), Value::Array(preempted));
            }
        }
        let host = self.bench_host(machine);
        if let Some(head) = self.head_mut(machine) {
            head.building = true;
            head.data.insert("state".into(), json!("running"));
            head.data.insert("waiting_for_queue".into(), json!(false));
            head.data.insert("host".into(), host);
        }
        self.bump_page();
        let library = self.library();
        let out = self.sample_path(&run.bid);
        let (bid, pages, machine) = (run.bid.clone(), run.pages, machine.to_string());
        self.run_background(Box::new(move || {
            if let Some(dir) = out.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let result = build_sample(&library, &out, pages);
            if result.is_err() {
                let _ = std::fs::remove_file(&out);
            }
            Msg::BenchSample {
                machine,
                bid,
                result,
            }
        }));
    }

    /// The sample of a line's head is packed: hand the benchmark to its machine.
    pub fn bench_sample_built(
        &mut self,
        machine: &str,
        bid: &str,
        result: Result<(i64, i64), String>,
    ) {
        let is_head = self
            .bench
            .lines
            .get(machine)
            .and_then(|l| l.front())
            .is_some_and(|r| r.bid == bid && r.building);
        if !is_head {
            // Cancelled (or the machine left) while it was being packed.
            let _ = std::fs::remove_file(self.sample_path(bid));
            return;
        }
        if let Some(head) = self.head_mut(machine) {
            head.building = false;
        }
        let (pages, volumes) = match result {
            Ok(b) => b,
            Err(e) => {
                self.bench_finish(machine, 0, "failed", Some(e));
                return;
            }
        };
        let Some(target) = self
            .machine_by_name(machine)
            .map(|m| (m.pid.clone(), m.local))
        else {
            self.bench_finish(
                machine,
                0,
                "failed",
                Some(format!("{machine} is no longer connected")),
            );
            return;
        };
        let Some(row) = self
            .head_mut(machine)
            .map(|r| (r.row.clone(), r.precision_only))
        else {
            return;
        };
        let sample = if target.1 {
            self.sample_path(bid).to_string_lossy().into_owned()
        } else {
            format!(
                "{}/{}/bench/{}/sample",
                bunko_proto::PROCESSOR_ROOT,
                target.0,
                bid
            )
        };
        let op = Op::Bench(BenchOp {
            bid: bid.to_string(),
            spec: bench_spec(&row.0),
            sample,
            pages: pages as u32,
            precision_only: row.1,
        });
        let sent = self.machines.get(&target.0).is_some_and(|m| m.send(op));
        let mono = self.mono();
        if let Some(head) = self.head_mut(machine) {
            head.sent = sent;
            head.started_mono = Some(mono);
            head.data
                .insert("sample".into(), json!({"pages": pages, "volumes": volumes}));
        }
        self.bump_page();
        if !sent {
            self.bench_finish(
                machine,
                0,
                "failed",
                Some(format!("{machine} is no longer connected")),
            );
        }
    }

    /// `_finish`: settle the run at `pos` of a machine's line — persist it (if it is
    /// one to keep) BEFORE the state is visible, then the next run of the line.
    fn bench_finish(&mut self, machine: &str, pos: usize, state: &str, error: Option<String>) {
        let Some(mut run) = self
            .bench
            .lines
            .get_mut(machine)
            .and_then(|l| l.remove(pos))
        else {
            return;
        };
        if let Some(i) = self
            .bench
            .order
            .iter()
            .position(|(k, m)| *k == run.key && m == machine)
        {
            self.bench.order.remove(i);
        }
        if !run.building {
            let _ = std::fs::remove_file(self.sample_path(&run.bid));
        }
        let finished_at = self.now_iso();
        run.data.insert("state".into(), json!(state));
        run.data.insert("finished_at".into(), json!(finished_at));
        run.data.insert("progress".into(), Value::Null);
        run.data.insert("waiting_for_queue".into(), json!(false));
        if let Some(e) = error {
            run.data.insert("error".into(), json!(e));
        } else if state == "cancelled" {
            run.data.insert("error".into(), Value::Null);
        }
        if state == "done" && !run.draft {
            self.bench_store(&run);
        }
        self.bench
            .recent
            .insert((run.key.clone(), machine.to_string()), run.data.clone());
        let drafts: Vec<(String, String)> = self
            .bench
            .recent
            .keys()
            .filter(|(k, _)| is_draft(k))
            .cloned()
            .collect();
        for stale in drafts
            .iter()
            .take(drafts.len().saturating_sub(MAX_DRAFT_RESULTS))
        {
            self.bench.recent.shift_remove(stale);
        }
        let error = run
            .data
            .get("error")
            .and_then(Value::as_str)
            .map(|e| format!(" ({e})"))
            .unwrap_or_default();
        let on = if machine == LOCAL {
            String::new()
        } else {
            format!(" on {machine}")
        };
        self.log(format!(
            "Benchmark of {}{on}: {state}{error}",
            self.bench_display_name(&run.key)
        ));
        if run.autobench {
            self.autobench_settled(machine, &run.row.id, state);
        }
        self.bump_page();
        if pos == 0 {
            self.bench_advance(machine);
        }
    }

    /// Persist a finished result: `.ocr-bench.json` (this server, not precision-only)
    /// and the machine's profile (a processor's benchmark, and this server's autobench).
    fn bench_store(&mut self, run: &BenchRun) {
        let local = run.machine == LOCAL;
        if local && !run.precision_only {
            let ids: Vec<String> = self.settings.rows.iter().map(|r| r.id.clone()).collect();
            if let Err(e) = bunko_sched::bench_file::BenchFile::new(&self.storage()).save(
                &run.key,
                run.data.clone(),
                &ids,
            ) {
                tracing::warn!("Could not persist the OCR benchmark result: {e}");
            }
        }
        if !local || run.autobench {
            self.persist_profile(run);
        }
    }

    /// `_persist_remote`: the bench summary into that machine's profile, and — for an
    /// autobench that changed something — `best` as its pools (only where it has none).
    fn persist_profile(&mut self, run: &BenchRun) {
        let best = run
            .data
            .get("best")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let get = |m: &Map<String, Value>, k: &str| m.get(k).cloned().unwrap_or(Value::Null);
        let mut bench = Map::new();
        bench.insert("pages_per_second".into(), get(&best, "pages_per_second"));
        bench.insert("window_seconds".into(), get(&best, "window_seconds"));
        bench.insert("gpu_busy_pct".into(), get(&best, "gpu_busy_pct"));
        bench.insert("cpu_busy_pct".into(), get(&best, "cpu_busy_pct"));
        bench.insert("startup_seconds".into(), get(&run.data, "startup_seconds"));
        bench.insert(
            "host".into(),
            run.data
                .get("host")
                .filter(|h| h.is_object())
                .cloned()
                .unwrap_or_else(|| json!({})),
        );
        bench.insert(
            "at".into(),
            run.data
                .get("finished_at")
                .filter(|v| v.is_string())
                .cloned()
                .unwrap_or_else(|| json!(self.now_iso())),
        );
        let truthy = |k: &str| {
            run.data
                .get(k)
                .filter(|v| bunko_sched::py::truthy(Some(v)))
                .cloned()
        };
        if let Some(p) = truthy("precision") {
            bench.insert("precision".into(), p);
        }
        if let Some(m) = truthy("precision_mode") {
            bench.insert("precision_mode".into(), m);
        }
        if let Some(t) = truthy("precision_trials") {
            bench.insert("precision_trials".into(), t);
            bench.insert(
                "precision_why".into(),
                json!(
                    run.data
                        .get("precision_why")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                ),
            );
        }
        let recipe = run.row.output_affecting();
        self.profiles
            .set_bench(profile_key(&run.machine), &run.key, &bench, Some(&recipe));
        if run.autobench && !run.precision_only && best.get("same_as_spec") != Some(&json!(true)) {
            let mut pools = Map::new();
            for t in crate::ocr::profiles::POOL_TABLES {
                pools.insert(
                    t.into(),
                    best.get(t)
                        .filter(|v| v.is_object())
                        .cloned()
                        .unwrap_or(json!({})),
                );
            }
            self.profiles.set_pools(
                profile_key(&run.machine),
                &run.key,
                &pools,
                Some(&recipe),
                true,
                true,
            );
        }
    }

    /// `_estimates`: `200 / pages_per_second` and nothing else; the pages still owed.
    fn bench_estimates(&self, row: &Generation, pps: Option<f64>) -> Value {
        let remaining = self.remaining_pages(row);
        let (volume, left) = match pps.filter(|p| *p > 0.0) {
            Some(p) => (
                Some(bunko_sched::py::round_int(BENCH_VOLUME_PAGES / p)),
                remaining.map(|r| bunko_sched::py::round_int(r as f64 / p)),
            ),
            None => (None, None),
        };
        json!({"volume_200_pages_seconds": volume, "remaining_pages": remaining, "remaining_seconds": left})
    }

    /// `_remaining_pages_for`: pages still owing this row a sidecar, from what the
    /// queue already knows (no archive is opened); None when nothing is known.
    fn remaining_pages(&self, row: &Generation) -> Option<i64> {
        let mut total = 0i64;
        let mut known = false;
        for vol in self.owed.volumes.values() {
            if !vol.rows.iter().any(|g| **g == *row.id) {
                continue;
            }
            if let Some(p) = vol.pages {
                known = true;
                total += p;
            }
        }
        known.then_some(total)
    }

    /// The `bench_*` events (and `fatal`/`exit` of a `bench-…` id): `_read_composed`.
    pub fn bench_event(&mut self, pid: &str, event: Event) {
        let Some(machine) = self.machines.get(pid).map(|m| m.name.clone()) else {
            return;
        };
        let bid = match &event {
            Event::BenchReady { bid, .. }
            | Event::BenchProgress { bid, .. }
            | Event::BenchTrial { bid, .. }
            | Event::BenchDone { bid, .. } => bid.clone(),
            other => other.sid().unwrap_or_default().to_string(),
        };
        let Some(pos) = self
            .bench
            .lines
            .get(&machine)
            .and_then(|l| l.iter().position(|r| r.bid == bid && r.sent))
        else {
            return;
        };
        if let Event::BenchDone { detail, .. } = &event {
            let done = self.complete_done(&machine, pos, detail);
            if let Some(run) = self
                .bench
                .lines
                .get_mut(&machine)
                .and_then(|l| l.get_mut(pos))
            {
                run.data.extend(done);
            }
            self.bench_finish(&machine, pos, "done", None);
            return;
        }
        let mut finish: Option<(&str, Option<String>)> = None;
        {
            let Some(run) = self
                .bench
                .lines
                .get_mut(&machine)
                .and_then(|l| l.get_mut(pos))
            else {
                return;
            };
            match event {
                Event::BenchReady { detail, .. } => bench_ready(run, &detail),
                Event::BenchProgress { detail, .. } => bench_progress(run, &detail),
                Event::BenchTrial { detail, .. } => {
                    // A trial carries the busy percentages sampled on the machine that
                    // ran it (the processor samples them, local or remote).
                    if let Some(Value::Array(trials)) = run.data.get_mut("trials") {
                        trials.push(Value::Object(detail));
                    }
                }
                Event::Fatal { error, .. } | Event::SpawnFailed { error, .. } => {
                    run.fatal = Some(if error.is_empty() {
                        "the runner reported a fatal error".into()
                    } else {
                        error
                    });
                }
                Event::Exit { returncode, .. } => {
                    let code = returncode
                        .filter(|c| *c != 0)
                        .map(|c| format!(" with status {c}"))
                        .unwrap_or_default();
                    let detail = match &run.fatal {
                        Some(f) => format!(": {f}"),
                        None => " before it produced a result".into(),
                    };
                    finish = Some((
                        "failed",
                        Some(format!(
                            "the {} benchmark ended{code}{detail}",
                            run.row.name
                        )),
                    ));
                }
                _ => {}
            }
        }
        self.bump_page();
        if let Some((state, error)) = finish {
            self.bench_finish(&machine, pos, state, error);
        }
    }

    /// `bench_done`, completed: `best` as whole tables (applying it is `pools = best`),
    /// the precision kept beside it (never in it), `same_as_spec`, the winning trial's
    /// busy numbers, the estimates.
    fn complete_done(
        &self,
        machine: &str,
        pos: usize,
        event: &Map<String, Value>,
    ) -> Map<String, Value> {
        let Some(run) = self.bench.lines.get(machine).and_then(|l| l.get(pos)) else {
            return Map::new();
        };
        let trials: Vec<Map<String, Value>> = run
            .data
            .get("trials")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_object).cloned().collect())
            .unwrap_or_default();
        let mut best = event
            .get("best")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let winner = trials
            .iter()
            .find(|t| t.get("n").is_some() && t.get("n") == best.get("trial"));
        let pools = &run.row.pools;
        let workers = table_to_apply(
            best.get("stage_workers"),
            &pools.stage_workers,
            winner.and_then(|w| w.get("stage_workers")),
        );
        best.insert("stage_workers".into(), Value::Object(workers));
        let placement = placement_to_apply(
            &pools.stage_device,
            run.data.get("host").and_then(|h| h.get("devices")),
            best.get("stage_device"),
        );
        best.insert("stage_device".into(), Value::Object(placement));
        let capacity = table_to_apply(
            best.get("queue_capacity"),
            &pools.queue_capacity,
            winner.and_then(|w| w.get("queue_capacity")),
        );
        best.insert("queue_capacity".into(), Value::Object(capacity));
        let ran = event
            .get("precision")
            .or_else(|| best.get("precision"))
            .and_then(Value::as_str)
            .map(str::to_string);
        for stale in [
            "precision",
            "precision_auto",
            "precision_ran",
            "card_family",
        ] {
            best.remove(stale);
        }
        let mut out = Map::new();
        if run.row.precision_applies()
            && let Some(ran) = ran.filter(|r| policy::PRECISIONS.contains(&r.as_str()))
        {
            out.insert("precision".into(), json!(ran));
            if let Some(mode) = event
                .get("precision_mode")
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
            {
                out.insert("precision_mode".into(), json!(mode));
            }
            if let Some(Value::Array(tried)) = event.get("precision_trials")
                && !tried.is_empty()
            {
                out.insert(
                    "precision_trials".into(),
                    Value::Array(tried.iter().filter(|t| t.is_object()).cloned().collect()),
                );
                out.insert(
                    "precision_why".into(),
                    json!(
                        event
                            .get("precision_why")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                    ),
                );
            }
        }
        best.insert("same_as_spec".into(), json!(same_as_spec(&run.row, &best)));
        if let Some(w) = winner {
            for k in ["gpu_busy_pct", "cpu_busy_pct"] {
                best.entry(k)
                    .or_insert_with(|| w.get(k).cloned().unwrap_or(Value::Null));
            }
        }
        let pps = as_float(best.get("pages_per_second"));
        let baseline = event
            .get("baseline")
            .and_then(Value::as_object)
            .filter(|b| !b.is_empty())
            .cloned();
        out.insert(
            "baseline".into(),
            baseline.map_or(Value::Null, Value::Object),
        );
        out.insert("best".into(), Value::Object(best));
        out.insert(
            "peak_rss_mb".into(),
            event.get("peak_rss_mb").cloned().unwrap_or(Value::Null),
        );
        out.insert(
            "peak_vram_mb".into(),
            event.get("peak_vram_mb").cloned().unwrap_or(Value::Null),
        );
        out.insert("estimates".into(), self.bench_estimates(&run.row, pps));
        out.insert("progress".into(), Value::Null);
        out
    }

    /// The look at running benchmarks: give up on a silent one.
    pub fn bench_tick(&mut self) {
        let now = self.mono();
        let overdue: Vec<String> = self
            .bench
            .lines
            .iter()
            .filter_map(|(m, l)| {
                l.front()
                    .filter(|r| {
                        r.started_mono
                            .is_some_and(|t| now - t > BENCH_BUDGET_SECONDS + BENCH_SLACK_SECONDS)
                    })
                    .map(|_| m.clone())
            })
            .collect();
        for machine in overdue {
            if let Some(bid) = self
                .bench
                .lines
                .get(&machine)
                .and_then(|l| l.front())
                .map(|r| r.bid.clone())
                && let Some(m) = self.machine_by_name(&machine)
            {
                m.send(Op::Cancel {
                    sid: None,
                    claim: None,
                    bid: Some(bid),
                });
            }
            self.bench_finish(
                &machine,
                0,
                "failed",
                Some("the benchmark ran past its time budget and was stopped".into()),
            );
        }
    }

    /// A machine left: its line ends (autobench pairs may be asked again).
    pub fn bench_machine_left(&mut self, machine: &str) {
        let Some(line) = self.bench.lines.remove(machine) else {
            return;
        };
        self.bench.order.retain(|(_, m)| m != machine);
        let finished_at = self.now_iso();
        for mut run in line {
            let _ = std::fs::remove_file(self.sample_path(&run.bid));
            run.data.insert("state".into(), json!("failed"));
            run.data.insert("finished_at".into(), json!(finished_at));
            run.data.insert("progress".into(), Value::Null);
            run.data.insert("waiting_for_queue".into(), json!(false));
            run.data
                .insert("error".into(), json!(format!("{machine} disconnected")));
            self.bench
                .recent
                .insert((run.key.clone(), machine.to_string()), run.data.clone());
            if run.autobench {
                let key = (profile_key(machine).to_string(), run.row.id.clone());
                self.autobench.inflight.remove(&key);
                self.autobench.asked.remove(&key);
            }
        }
        if self.bench.holding.remove(machine).is_some() {
            self.release_queue(machine);
        }
        self.bump_page();
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
            let machine = if profile == crate::ocr::profiles::LOCAL_PROFILE {
                LOCAL.to_string()
            } else {
                profile.clone()
            };
            let pid = self.machine_by_name(&machine).map(|m| m.pid.clone());
            let row = self.row(&gid).cloned();
            let (Some(pid), Some(row)) = (pid, row) else {
                self.autobench.inflight.remove(&(profile, gid));
                continue;
            };
            let Some(kind) = self.autobench_kind(&pid, &row) else {
                // Answered meanwhile (a pick landed, the row changed).
                self.autobench_settled(&machine, &gid, "done");
                continue;
            };
            let precision_only = kind == "precision";
            let label = self
                .machines
                .get(&pid)
                .map(|m| m.label())
                .unwrap_or_default();
            self.log(if precision_only {
                format!(
                    "Benchmarking {}'s precision ({}) on {label} before it runs there; its pools stay as set",
                    row.name, row.precision
                )
            } else {
                format!("Benchmarking {} on {label} before it runs there", row.name)
            });
            // A precision-only benchmark measures the machine's pools exactly as it
            // runs them; a whole one starts from the row.
            let spec = precision_only.then(|| {
                let run = self.row_spec(&machine, &row);
                let mut v = row.to_value();
                if let Value::Object(m) = &mut v {
                    m.insert(
                        "pools".into(),
                        json!({
                            "stage_workers": run.pools.stage_workers,
                            "queue_capacity": run.pools.queue_capacity,
                            "stage_device": run.pools.stage_device,
                        }),
                    );
                }
                v
            });
            let req = BenchRequest {
                key: gid.clone(),
                spec,
                pages: None,
                processor: machine.clone(),
                autobench: true,
                precision_only,
            };
            if let Err((_, body)) = self.bench_enqueue(req) {
                self.log(format!(
                    "Could not benchmark {} on {label}: {}",
                    row.name,
                    body["error"].as_str().unwrap_or_default()
                ));
                self.autobench_settled(&machine, &gid, "refused");
            }
        }
    }

    /// The bench a sample URL serves, if `pid` owns it: its file.
    pub fn bench_sample(&self, pid: &str, bid: &str) -> Result<PathBuf, (u16, &'static str)> {
        let Some(machine) = self.machines.get(pid) else {
            return Err((404, "No such processor"));
        };
        let live = self
            .bench
            .lines
            .get(&machine.name)
            .is_some_and(|l| l.iter().any(|r| r.bid == bid && r.sent));
        if !bid.starts_with("bench-") || !live {
            return Err((404, "No such sample"));
        }
        Ok(self.sample_path(bid))
    }
}

/// `bench_ready`: startup, tunable, where each model came up, the sample's real size,
/// a fresh progress block.
fn bench_ready(run: &mut BenchRun, detail: &Map<String, Value>) {
    run.data.insert(
        "startup_seconds".into(),
        as_float(detail.get("startup_seconds")).map_or(Value::Null, bunko_sched::py::float_value),
    );
    let tunable = detail
        .get("tunable")
        .is_none_or(|v| bunko_sched::py::truthy(Some(v)));
    run.data.insert("tunable".into(), json!(tunable));
    if let Some(Value::Object(placed)) = detail.get("stage_device") {
        let mut host = run
            .data
            .get("host")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let devices: Map<String, Value> = placed
            .iter()
            .map(|(k, v)| (k.clone(), json!(v.as_str().unwrap_or_default())))
            .collect();
        host.insert("devices".into(), Value::Object(devices));
        run.data.insert("host".into(), Value::Object(host));
    }
    let pages = detail.get("pages").and_then(int_of).filter(|p| *p != 0);
    if let Some(p) = pages
        && let Some(Value::Object(sample)) = run.data.get_mut("sample")
    {
        sample.insert("pages".into(), json!(p));
    }
    let sample_pages = run
        .data
        .get("sample")
        .and_then(|s| s.get("pages"))
        .cloned()
        .unwrap_or(Value::Null);
    let max_trials = detail
        .get("max_trials")
        .and_then(int_of)
        .filter(|n| *n != 0)
        .unwrap_or(BENCH_MAX_TRIALS);
    run.data.insert(
        "progress".into(),
        json!({
            "trial": 0,
            "max_trials": max_trials,
            "pages_done": 0,
            "pages": pages.map_or(sample_pages, |p| json!(p)),
            "stage_workers": {},
            "pages_per_second": null,
        }),
    );
}

/// `bench_progress`: merged into the progress block (a trial now spans several passes).
fn bench_progress(run: &mut BenchRun, detail: &Map<String, Value>) {
    let mut progress = run
        .data
        .get("progress")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let field = |k: &str| detail.get(k).cloned().unwrap_or(Value::Null);
    let pages = detail
        .get("pages")
        .cloned()
        .or_else(|| progress.get("pages").cloned())
        .unwrap_or(Value::Null);
    progress.insert("trial".into(), field("trial"));
    progress.insert("pages_done".into(), field("pages_done"));
    progress.insert("pages".into(), pages);
    let workers = detail
        .get("stage_workers")
        .filter(|v| bunko_sched::py::truthy(Some(v)))
        .cloned()
        .unwrap_or_else(|| json!({}));
    progress.insert("stage_workers".into(), workers);
    progress.insert("pages_per_second".into(), field("pages_per_second"));
    progress.insert("pass_index".into(), field("pass_index"));
    progress.insert("window_seconds".into(), field("window_seconds"));
    progress.insert("pages_measured".into(), field("pages_measured"));
    progress
        .entry("max_trials")
        .or_insert(json!(BENCH_MAX_TRIALS));
    run.data.insert("progress".into(), Value::Object(progress));
}

impl Scheduler {
    /// For tests and the admin: queue a message to run after the current one.
    pub fn defer(&mut self, msg: Msg) {
        self.internal.push_back(msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn apply_tables_never_persist_an_unmeasured_pin() {
        let pins = BTreeMap::from([("detect".to_string(), 3u32), ("post".to_string(), 2)]);
        // The search moved nothing; detect ran at its pin, post did not.
        let t = table_to_apply(
            Some(&json!({})),
            &pins,
            Some(&json!({"detect": 3, "engine": 1, "post": 1})),
        );
        assert_eq!(Value::Object(t), json!({"detect": 3, "post": "auto"}));
        let t = table_to_apply(Some(&json!({"detect": 4})), &pins, None);
        assert_eq!(Value::Object(t), json!({"detect": 4, "post": "auto"}));
        let devices = BTreeMap::from([
            ("detect".to_string(), "cpu".to_string()),
            ("engine".to_string(), "gpu:1".to_string()),
        ]);
        let placed = json!({"detect": "cpu", "engine": "gpu:0"});
        let t = placement_to_apply(&devices, Some(&placed), Some(&json!({})));
        assert_eq!(
            Value::Object(t),
            json!({"detect": "cpu", "engine": "gpu:0"}),
            "a pin the runner did not honour is replaced by where it ran"
        );
    }

    #[test]
    fn drafts_and_sample_names() {
        assert!(is_draft("draft-ab-1"));
        assert!(!is_draft("draft-"));
        assert!(!is_draft("draft-UPPER"));
        assert!(!is_draft("g-1"));
        assert_eq!(bench_sample_filename("bench-a/b"), "bench-ab.cbz");
        assert_eq!(bench_sample_filename("///"), "sample.cbz");
    }
}
