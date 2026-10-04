//! Benchmarks: measure one generation row on THIS machine, on pages sampled from the
//! library (0.5.2 `engine_runner.py --bench` and the processor bridge's `_run_bench`;
//! spec ocr-generations-bench §6).
//!
//! The library packs the sample and sends a `bench` op; this module answers with
//! `bench_ready`, `bench_progress`…, `bench_trial`… and `bench_done` (or `fatal`), and
//! the hub ends every benchmark with `exit`. It runs the real pipeline — the same
//! [`PagePipeline::open`] / [`VolumeRunner::run_volume`] a session uses — so every
//! number is the engines' own on this hardware:
//!
//! 1. load the row once (its pools' widths and capacities stripped: a benchmark starts
//!    from the derived widths and measures the machine, not the hand-tuning; its device
//!    pins kept), warm up on the first pages (discarded; "first page after X s" is the
//!    only figure the model load appears in);
//! 2. for a balanced/speed row with more than one candidate format on the model's
//!    device, one trial per candidate on the same sample, the fastest winning (a 5% tie
//!    goes to the more accurate);
//! 3. the width search: widen the stage the pipeline's own verdict names while it pays
//!    (≥ 3%), then give back what is not used while throughput holds (within 1% of the
//!    peak); at most 8 trials (+ the precision phase) and 900 s;
//! 4. every trial re-feeds the sample as one continuous run until its measured window
//!    is ≥ 20 s (or 8 feeds), and the rate is `(M - 1) / (t_last - t_first)` over the
//!    page emissions after the pipeline fill — never a stopwatch around anything.
//!
//! What 0.7 does differently, because the engines are in-process: a width or a format
//! is a new [`VolumeRunner`] (the recognizer of an unchanged format is shared, so only
//! the stage threads are rebuilt), a format is a compiled package rather than a cast of
//! an fp32 master, and the detector placement search finds nothing to try (the PP-OCR
//! detector runs on the CPU only, as in 0.5.2). The busy percentages are sampled here,
//! where the work happens, and stamped on each trial (the bridge's behaviour in 0.5.2).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bunko_proto::{BenchOp, Catalog, Event, RowSpec};
use bunko_sched::bench::{
    BENCH_BUDGET_SECONDS, BENCH_GAIN, BENCH_HOLD, BENCH_MAX_PASSES, BENCH_MAX_TRIALS,
    BENCH_PROGRESS_INTERVAL, BENCH_WARMUP_PAGES, BenchWindow, WindowRules, bench_fill,
    bench_window, sized_repeat,
};
use bunko_sched::congestion::{summarize, widen_target};
use bunko_sched::precision as policy;
use bunko_sched::py::{float_value, round_int, round_to};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};

use crate::pipeline::{
    CancelToken, PagePipeline, PageProgress, PipelineReport, ReadyInfo, RunError, VolumeMeta,
    VolumeRunner,
};
use crate::utilization::{Sampler, device_index, peak_rss_mb};

/// The numbers a benchmark runs by (0.5.2's constants). Tests shrink them.
#[derive(Debug, Clone)]
pub struct BenchConfig {
    /// Width-search trials (the precision phase adds its own).
    pub max_trials: usize,
    /// The search stops starting trials after this.
    pub budget: Duration,
    /// Pages thrown away before the first trial.
    pub warmup_pages: usize,
    /// `bench_progress` events are at least this far apart.
    pub progress_interval: Duration,
    /// Feeds a trial may take to fill its window.
    pub max_passes: usize,
    pub rules: WindowRules,
    /// The utilisation sampler's tick.
    pub sample_interval: Duration,
    /// OCR jobs that share this host (the processor's sessions): the host's worker
    /// budget, the ceiling of every pooled stage, is split between them.
    pub jobs: usize,
    /// The host's worker budget as given (tests); None: from the cores and `jobs`.
    pub workers_budget: Option<u32>,
}

impl Default for BenchConfig {
    fn default() -> Self {
        BenchConfig {
            max_trials: BENCH_MAX_TRIALS,
            budget: Duration::from_secs_f64(BENCH_BUDGET_SECONDS),
            warmup_pages: BENCH_WARMUP_PAGES,
            progress_interval: Duration::from_secs_f64(BENCH_PROGRESS_INTERVAL),
            max_passes: BENCH_MAX_PASSES,
            rules: WindowRules::default(),
            sample_interval: Duration::from_secs(1),
            jobs: 1,
            workers_budget: None,
        }
    }
}

/// Why a benchmark stopped without a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BenchAbort {
    Cancelled,
    /// Said as `fatal{error}`.
    Fatal(String),
}

/// Image members of the sample, in reading order.
pub fn sample_pages(archive: &Path) -> Result<Vec<String>, String> {
    let file = std::fs::File::open(archive)
        .map_err(|e| format!("could not open the benchmark sample: {e}"))?;
    let zip = zip::ZipArchive::new(std::io::BufReader::new(file))
        .map_err(|e| format!("the benchmark sample is not a zip: {e}"))?;
    let mut names: Vec<String> = zip
        .file_names()
        .filter(|n| !n.ends_with('/'))
        .filter(|n| {
            let lower = n.to_ascii_lowercase();
            [
                ".jpg", ".jpeg", ".png", ".webp", ".bmp", ".gif", ".tif", ".tiff",
            ]
            .iter()
            .any(|s| lower.ends_with(s))
        })
        .map(str::to_string)
        .collect();
    names.sort_by(|a, b| bunko_sched::job_order::natural_cmp(a, b));
    Ok(names)
}

/// The first `count` pages of the sample as an archive of their own (the warm-up).
fn write_prefix(sample: &Path, pages: &[String], out: &Path) -> Result<(), String> {
    let file = std::fs::File::open(sample).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(|e| e.to_string())?;
    let target = std::fs::File::create(out).map_err(|e| e.to_string())?;
    let mut writer = zip::ZipWriter::new(std::io::BufWriter::new(target));
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for name in pages {
        let mut member = zip.by_name(name).map_err(|e| e.to_string())?;
        writer
            .start_file(name.as_str(), options)
            .map_err(|e| e.to_string())?;
        std::io::copy(&mut member, &mut writer).map_err(|e| e.to_string())?;
    }
    writer
        .finish()
        .map_err(|e| e.to_string())?
        .flush()
        .map_err(|e| e.to_string())
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic".to_string())
}

fn r3(x: f64) -> Value {
    float_value(round_to(x, 3))
}

fn r4(x: f64) -> Value {
    float_value(round_to(x, 4))
}

/// This session's share of the host in pooled workers, as the engines plan it
/// (`bunko-engines` `plan::host_worker_budget`): the workers whose shared PP-OCR
/// session (4 threads, 2 more per extra worker) fits in the job's share of the logical
/// CPUs after 2 kept for the engine's host thread, the post stage and the feeder.
pub fn host_worker_budget(logical_cpus: usize, jobs: usize) -> u32 {
    let share = logical_cpus / jobs.max(1);
    let spare = share.saturating_sub(2 + 4);
    (spare / 2 + 1).max(1) as u32
}

/// The widest a stage may be widened to when the engine does not say
/// ([`VolumeRunner::width_ceilings`]), mirroring the engines' planner: a pooled CPU
/// stage up to the host budget (at most 8, 4 on the ppocr-manga road as 0.5.2); the
/// device-bound engine stage up to 8 threads over its one session on a GPU (and the
/// budget), 1 on the CPU. Never below what the engines derived themselves.
pub fn estimated_ceiling(
    stage: Option<&crate::pipeline::StageReport>,
    engine: &str,
    budget: u32,
    derived: u32,
) -> u32 {
    let pooled_cap = if engine == "ppocr-manga" { 4 } else { 8 };
    let ceiling = match stage {
        Some(s) if s.device_bound => {
            if s.device.starts_with("gpu") {
                budget.min(8)
            } else {
                1
            }
        }
        _ => budget.min(pooled_cap),
    };
    ceiling.max(derived).max(1)
}

/// Logical CPUs (SMT threads included).
pub fn logical_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Physical cores (0.5.2 sized pools on those, not on SMT threads).
pub fn physical_cpu_count() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    #[cfg(target_os = "linux")]
    {
        let mut cores = BTreeSet::new();
        if let Ok(dir) = std::fs::read_dir("/sys/devices/system/cpu") {
            for entry in dir.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(n) = name.strip_prefix("cpu") else {
                    continue;
                };
                if n.is_empty() || !n.chars().all(|c| c.is_ascii_digit()) {
                    continue;
                }
                let topo = entry.path().join("topology");
                let read = |f: &str| {
                    std::fs::read_to_string(topo.join(f))
                        .ok()
                        .map(|s| s.trim().to_string())
                };
                if let (Some(pkg), Some(core)) = (read("physical_package_id"), read("core_id")) {
                    cores.insert((pkg, core));
                }
            }
        }
        if !cores.is_empty() && cores.len() <= logical {
            return cores.len();
        }
    }
    logical
}

/// One trial: one set of widths (and one format) measured over the sample.
#[derive(Debug, Clone)]
struct Trial {
    n: usize,
    note: String,
    stage_workers: Map<String, Value>,
    queue_capacity: Map<String, Value>,
    stage_device: Map<String, Value>,
    seconds: f64,
    window: BenchWindow,
    accepted: bool,
    verdict: Option<String>,
    bottleneck: Option<String>,
    stages: Vec<Value>,
    queues: Vec<Value>,
    /// The full reading (`summarize`), for the search; never sent.
    reading: Option<Map<String, Value>>,
    precision: Option<String>,
    gpu_busy_pct: Option<f64>,
    cpu_busy_pct: Option<f64>,
}

impl Trial {
    fn pages_per_second(&self) -> f64 {
        self.window.pages_per_second
    }

    fn short_window(&self) -> bool {
        self.window.short_window
    }

    /// `BenchTrial.as_dict()` plus the busy percentages, in 0.5.2's key order.
    fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("n".into(), json!(self.n));
        m.insert("note".into(), json!(self.note));
        m.insert(
            "stage_workers".into(),
            Value::Object(self.stage_workers.clone()),
        );
        m.insert(
            "queue_capacity".into(),
            Value::Object(self.queue_capacity.clone()),
        );
        m.insert(
            "stage_device".into(),
            Value::Object(self.stage_device.clone()),
        );
        m.insert("seconds".into(), r3(self.seconds));
        m.insert("pages_per_second".into(), r4(self.pages_per_second()));
        m.extend(self.window.to_map());
        m.insert("accepted".into(), json!(self.accepted));
        m.insert("verdict".into(), json!(self.verdict));
        m.insert("bottleneck".into(), json!(self.bottleneck));
        m.insert("stages".into(), Value::Array(self.stages.clone()));
        m.insert("queues".into(), Value::Array(self.queues.clone()));
        if let Some(p) = &self.precision {
            m.insert("precision".into(), json!(p));
        }
        m.insert(
            "gpu_busy_pct".into(),
            self.gpu_busy_pct.map_or(Value::Null, float_value),
        );
        m.insert(
            "cpu_busy_pct".into(),
            self.cpu_busy_pct.map_or(Value::Null, float_value),
        );
        m
    }
}

/// `bench_rows(report)`: a trial's stage and queue rows, read the way the queue page
/// reads a run (`summarize`).
fn bench_rows(report: &PipelineReport) -> (Vec<Value>, Vec<Value>, Option<Map<String, Value>>) {
    let summary = summarize(&report.to_value());
    let pct = |row: &Map<String, Value>, key: &str| {
        round_int(row.get(key).and_then(Value::as_f64).unwrap_or(0.0))
    };
    let stages = summary
        .as_ref()
        .and_then(|s| s.get("stages"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(Value::as_object)
                .map(|row| {
                    json!({
                        "key": row.get("key").cloned().unwrap_or(Value::Null),
                        "workers": row.get("workers").cloned().unwrap_or(Value::Null),
                        "busy_pct": pct(row, "busy_pct"),
                        "starved_pct": pct(row, "starved_pct"),
                        "blocked_pct": pct(row, "blocked_pct"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let queues = report
        .queues
        .iter()
        .map(|q| {
            json!({
                "name": q.name,
                "capacity": q.capacity,
                "mean_depth": float_value(round_to(q.mean_depth, 2)),
                "max_depth": q.max_depth,
            })
        })
        .collect();
    (stages, queues, summary)
}

/// A loaded session at one set of widths.
struct Loaded {
    runner: Box<dyn VolumeRunner>,
    ready: ReadyInfo,
    /// The widths this runner was asked for (the derivation's for the first one).
    widths: Vec<u32>,
    /// The format it was forced to; None: the row's mode resolved it.
    forced: Option<String>,
}

/// Everything one benchmark run needs.
pub struct BenchRun<'a> {
    pipeline: &'a dyn PagePipeline,
    op: &'a BenchOp,
    config: &'a BenchConfig,
    cancel: &'a CancelToken,
    say: &'a (dyn Fn(Event) + Sync),
    sample: PathBuf,
    workspace: PathBuf,
    pages: usize,
    origin: Instant,
    deadline: Instant,
    catalog: Catalog,
    sampler: Option<Sampler>,
    current: Option<Loaded>,
    keys: Vec<String>,
    ceilings: Vec<u32>,
    trials: Vec<Trial>,
    max_trials: usize,
    mode: &'static str,
    /// The format every later runner is forced to once the precision phase settled.
    settled: Option<String>,
    /// What the recognizer runs at (None: the engine fixes its own).
    precision: Option<String>,
    precision_trials: Vec<usize>,
    /// Candidates of the precision phase that would not load here (no compiled package
    /// for this card): reported at 0 pages/s, so the library sees every candidate it
    /// expects tried and does not ask for the benchmark again and again.
    unrunnable: Vec<String>,
    /// The phase's candidates, in order.
    candidates: Vec<String>,
    precision_why: String,
    volumes: AtomicUsize,
}

impl<'a> BenchRun<'a> {
    pub fn new(
        pipeline: &'a dyn PagePipeline,
        op: &'a BenchOp,
        sample: PathBuf,
        workspace: PathBuf,
        config: &'a BenchConfig,
        cancel: &'a CancelToken,
        say: &'a (dyn Fn(Event) + Sync),
    ) -> BenchRun<'a> {
        let origin = Instant::now();
        BenchRun {
            pipeline,
            op,
            config,
            cancel,
            say,
            sample,
            workspace,
            pages: 0,
            origin,
            deadline: origin + config.budget,
            catalog: Catalog::default(),
            sampler: None,
            current: None,
            keys: Vec::new(),
            ceilings: Vec::new(),
            trials: Vec::new(),
            max_trials: config.max_trials.max(1),
            mode: policy::normalize_mode(Some(&op.spec.precision)).unwrap_or(policy::DEFAULT_MODE),
            settled: None,
            precision: None,
            precision_trials: Vec::new(),
            unrunnable: Vec::new(),
            candidates: Vec::new(),
            precision_why: String::new(),
            volumes: AtomicUsize::new(0),
        }
    }

    fn emit(&self, event: Event) {
        (self.say)(event);
    }

    fn bid(&self) -> String {
        self.op.bid.clone()
    }

    fn since_origin(&self) -> f64 {
        self.origin.elapsed().as_secs_f64()
    }

    fn check(&self) -> Result<(), BenchAbort> {
        if self.cancel.is_cancelled() {
            Err(BenchAbort::Cancelled)
        } else {
            Ok(())
        }
    }

    fn map(&self, widths: &[u32]) -> Map<String, Value> {
        self.keys
            .iter()
            .zip(widths)
            .map(|(k, w)| (k.clone(), json!(w)))
            .collect()
    }

    /// The row as one trial runs it.
    fn spec_for(&self, widths: Option<&[u32]>, precision: Option<&str>) -> RowSpec {
        let mut spec = self.op.spec.clone();
        spec.precision_pick = None;
        spec.precision_why.clear();
        if !self.op.precision_only {
            // `open_bench`: never the row's widths or capacities, always its devices.
            spec.pools.stage_workers.clear();
            spec.pools.queue_capacity.clear();
        }
        if let Some(widths) = widths {
            spec.pools.stage_workers = self
                .keys
                .iter()
                .cloned()
                .zip(widths.iter().copied())
                .collect();
        }
        if let Some(p) = precision {
            spec.precision = p.to_string();
        }
        spec
    }

    fn open(&self, widths: Option<&[u32]>, precision: Option<&str>) -> Result<Loaded, String> {
        let spec = self.spec_for(widths, precision);
        let opened = std::panic::catch_unwind(AssertUnwindSafe(|| self.pipeline.open(&spec)))
            .unwrap_or_else(|p| Err(format!("loading the models panicked: {}", panic_text(&*p))))?;
        let ready = opened.ready();
        let widths = match widths {
            Some(w) => w.to_vec(),
            None => self.widths_of(&ready),
        };
        Ok(Loaded {
            runner: opened,
            ready,
            widths,
            forced: precision.map(str::to_string),
        })
    }

    fn widths_of(&self, ready: &ReadyInfo) -> Vec<u32> {
        self.keys
            .iter()
            .map(|k| ready.stage_workers.get(k).copied().unwrap_or(1))
            .collect()
    }

    /// Make the current runner the one for `widths` at the settled format (or the one
    /// `forced` names), opening a new one only when they differ. For a new width the old
    /// runner stays alive until the new one is ready, so the recognizer (and the PP-OCR
    /// pair) is shared rather than loaded again; for a new format it goes first, so two
    /// recognizers never share the card (a format is its own compiled package; 0.5.2
    /// cast one fp32 master instead).
    fn ensure(&mut self, widths: &[u32], forced: Option<&str>) -> Result<(), String> {
        let want = forced.map(str::to_string).or_else(|| self.settled.clone());
        let same_format =
            |l: &Loaded| want.is_none() || l.forced == want || l.ready.precision == want;
        if let Some(cur) = &self.current
            && cur.widths == widths
            && same_format(cur)
        {
            return Ok(());
        }
        if self.current.as_ref().is_some_and(|cur| !same_format(cur)) {
            self.current = None;
        }
        let loaded = self.open(Some(widths), want.as_deref())?;
        self.current = Some(loaded);
        Ok(())
    }

    // --- one feed ---------------------------------------------------------------------

    /// One continuous feed of the sample, `repeat` times, through the current runner:
    /// `(wall seconds, emissions)`. Emissions are the instants each page left the
    /// pipeline, in seconds since the benchmark began.
    #[allow(clippy::too_many_arguments)]
    fn feed(
        &self,
        archive: &Path,
        pages: usize,
        repeat: usize,
        trial: usize,
        so_far: &[f64],
        fill: usize,
        pass_index: usize,
        widths: &[u32],
    ) -> Result<(f64, Vec<f64>), BenchAbort> {
        let Some(loaded) = self.current.as_ref() else {
            return Err(BenchAbort::Fatal("no pipeline is loaded".into()));
        };
        let runner = loaded.runner.as_ref();
        let jobs = pages * repeat;
        let mine: Mutex<Vec<f64>> = Mutex::new(Vec::with_capacity(jobs));
        let announced: Mutex<Option<Instant>> = Mutex::new(None);
        let failure: Mutex<Option<BenchAbort>> = Mutex::new(None);
        let failed_pages = AtomicUsize::new(0);
        let next = AtomicUsize::new(0);
        let run_cancel = self.cancel.child_token();
        let threads = runner.overlap().clamp(1, 2).min(repeat.max(1));
        let started = Instant::now();
        let stage_workers = self.map(widths);
        let record = |_: ()| {
            let mut list = mine.lock();
            let now = Instant::now();
            list.push(now.duration_since(self.origin).as_secs_f64());
            if trial == 0 {
                return;
            }
            let mut last = announced.lock();
            if last.is_some_and(|t| now.duration_since(t) < self.config.progress_interval) {
                return;
            }
            *last = Some(now);
            let mut all = so_far.to_vec();
            all.extend_from_slice(&list);
            let running = bench_window(
                &all,
                fill,
                pass_index,
                self.config.rules.short_window_seconds,
            );
            let mut detail = Map::new();
            detail.insert("trial".into(), json!(trial));
            detail.insert("pass_index".into(), json!(pass_index));
            detail.insert("pages_done".into(), json!(list.len()));
            detail.insert("pages".into(), json!(jobs));
            detail.insert("stage_workers".into(), Value::Object(stage_workers.clone()));
            detail.insert("pages_per_second".into(), r4(running.pages_per_second));
            detail.insert("window_seconds".into(), r3(running.window_seconds));
            detail.insert("pages_measured".into(), json!(running.pages_measured));
            drop(list);
            self.emit(Event::BenchProgress {
                bid: self.bid(),
                detail,
            });
        };
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::SeqCst);
                        if index >= repeat || run_cancel.is_cancelled() {
                            return;
                        }
                        let seq = self.volumes.fetch_add(1, Ordering::SeqCst);
                        let name = format!("bench-{seq}.mokuro");
                        let out = self.workspace.join(&name);
                        let meta = VolumeMeta {
                            claim: format!("bench-{seq}"),
                            title: "benchmark".into(),
                            volume: format!("benchmark {seq}"),
                            title_uuid: None,
                            volume_uuid: None,
                            stem: "sample".into(),
                            sidecar_name: name,
                        };
                        let progress = |p: PageProgress| {
                            if let PageProgress::Page { .. } = p {
                                record(());
                            }
                        };
                        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                            runner.run_volume(archive, &meta, &out, &progress, &run_cancel)
                        }))
                        .unwrap_or_else(|p| {
                            Err(RunError::Fatal(format!(
                                "the pipeline panicked: {}",
                                panic_text(&*p)
                            )))
                        });
                        let _ = std::fs::remove_file(&out);
                        let _ = std::fs::remove_file(out.with_extension("mokuro.tmp"));
                        match result {
                            Ok(outcome) => {
                                failed_pages
                                    .fetch_add(outcome.failed_pages as usize, Ordering::SeqCst);
                            }
                            Err(RunError::Volume(e)) => {
                                failed_pages.fetch_add(pages, Ordering::SeqCst);
                                tracing::warn!(
                                    "benchmark {}: a sample pass failed: {e}",
                                    self.op.bid
                                );
                                if trial == 0 {
                                    failure.lock().get_or_insert(BenchAbort::Fatal(e));
                                    run_cancel.cancel();
                                }
                            }
                            Err(RunError::Fatal(e)) => {
                                failure.lock().get_or_insert(BenchAbort::Fatal(e));
                                run_cancel.cancel();
                            }
                            Err(RunError::Cancelled) => {
                                if self.cancel.is_cancelled() {
                                    failure.lock().get_or_insert(BenchAbort::Cancelled);
                                }
                            }
                        }
                    }
                });
            }
        });
        if let Some(abort) = failure.into_inner() {
            return Err(abort);
        }
        self.check()?;
        let mine = mine.into_inner();
        let last = mine
            .last()
            .map(|t| self.origin + Duration::from_secs_f64(*t))
            .unwrap_or(started);
        let seconds = last
            .saturating_duration_since(started)
            .as_secs_f64()
            .max(1e-9);
        let failed = failed_pages.into_inner();
        if failed > 0 {
            tracing::warn!(
                "benchmark {}: {failed} of {jobs} sample pages failed at these widths",
                self.op.bid
            );
        }
        Ok((seconds, mine))
    }

    /// `_measure`: feed the sample until the measured window is worth deciding on.
    fn measure(
        &self,
        widths: &[u32],
        trial: usize,
    ) -> Result<(f64, BenchWindow, PipelineReport), BenchAbort> {
        let rules = &self.config.rules;
        let fill = bench_fill(self.pages);
        let mark = self.current.as_ref().and_then(|l| l.runner.stats());
        let mut per_feed: Vec<Vec<f64>> = Vec::new();
        let mut repeats: Vec<usize> = Vec::new();
        let mut seconds = 0.0;
        let mut repeat = 1usize;
        let mut feeds = 0usize;
        while feeds < self.config.max_passes.max(1) {
            feeds += 1;
            let so_far: Vec<f64> = per_feed.iter().flatten().copied().collect();
            let (elapsed, mine) = self.feed(
                &self.sample,
                self.pages,
                repeat,
                trial,
                &so_far,
                fill,
                feeds,
                widths,
            )?;
            seconds += elapsed;
            let window = bench_window(&mine, fill, repeat, rules.short_window_seconds);
            let grown = sized_repeat(self.pages, &mine, elapsed, repeat, rules);
            per_feed.push(mine);
            repeats.push(repeat);
            if window.window_seconds >= rules.min_window_seconds {
                break;
            }
            if Instant::now() >= self.deadline {
                tracing::info!(
                    "benchmark {}: out of time after {feeds} feed(s); the window is {:.1}s",
                    self.op.bid,
                    window.window_seconds
                );
                break;
            }
            if grown <= repeat {
                break;
            }
            tracing::info!(
                "benchmark {}: a {:.1}s window over {} page(s) is too short -- feeding the sample x{grown} as one run",
                self.op.bid,
                window.window_seconds,
                per_feed.last().map_or(0, Vec::len)
            );
            repeat = grown;
        }
        let last = per_feed.last().cloned().unwrap_or_default();
        let mut window = bench_window(
            &last,
            fill,
            repeats.last().copied().unwrap_or(1),
            rules.short_window_seconds,
        );
        if window.window_seconds < rules.min_window_seconds && per_feed.len() > 1 {
            let all: Vec<f64> = per_feed.iter().flatten().copied().collect();
            window = bench_window(&all, fill, repeats.iter().sum(), rules.short_window_seconds);
        }
        if window.short_window {
            tracing::warn!(
                "benchmark {}: the measured window is only {:.1}s over {} pages after {feeds} feed(s) -- too short to decide anything on",
                self.op.bid,
                window.window_seconds,
                window.pages_measured
            );
        }
        let report = self
            .current
            .as_ref()
            .and_then(|l| l.runner.stats())
            .map(|now| now.since(mark.as_ref()))
            .unwrap_or_default();
        Ok((seconds, window, report))
    }

    /// `_trial`: one measured trial at `widths` (the current format). Its index.
    fn trial(&mut self, widths: &[u32], note: &str) -> Result<usize, BenchAbort> {
        self.check()?;
        self.ensure(widths, None).map_err(BenchAbort::Fatal)?;
        let n = self.trials.len() + 1;
        tracing::info!(
            "benchmark {} trial {n} ({note}): {:?}",
            self.op.bid,
            self.map(widths)
        );
        let (seconds, window, report) = self.measure(widths, n)?;
        let (stages, queues, reading) = bench_rows(&report);
        let (gpu, cpu) = self.sampler.as_ref().map_or((None, None), |s| {
            s.means(window.first_emission_at, window.last_emission_at)
        });
        let loaded = self.current.as_ref();
        let mut queue_capacity = Map::new();
        if let Some(l) = loaded {
            for k in &self.keys {
                if let Some(c) = l.ready.queue_capacity.get(k) {
                    queue_capacity.insert(k.clone(), json!(c));
                }
            }
        }
        let stage_device: Map<String, Value> = loaded
            .map(|l| {
                l.ready
                    .stage_device
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect()
            })
            .unwrap_or_default();
        let verdict = reading
            .as_ref()
            .and_then(|r| r.get("verdict"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let bottleneck = reading
            .as_ref()
            .and_then(|r| r.get("bottleneck"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let precision = loaded
            .and_then(|l| l.ready.precision.clone())
            .or_else(|| self.precision.clone());
        let trial = Trial {
            n,
            note: note.to_string(),
            stage_workers: self.map(widths),
            queue_capacity,
            stage_device,
            seconds,
            window,
            accepted: true,
            verdict,
            bottleneck,
            stages,
            queues,
            reading,
            precision,
            gpu_busy_pct: gpu,
            cpu_busy_pct: cpu,
        };
        tracing::info!(
            "benchmark {} trial {n}: {:.3} pages/s over {:.1}s of emissions ({} pages, {} pass(es), {:.1}s wall){}; verdict: {}",
            self.op.bid,
            trial.pages_per_second(),
            trial.window.window_seconds,
            trial.window.pages_measured,
            trial.window.passes,
            seconds,
            if trial.short_window() {
                " SHORT WINDOW"
            } else {
                ""
            },
            trial.verdict.as_deref().unwrap_or("balanced")
        );
        self.trials.push(trial);
        Ok(n - 1)
    }

    /// `_emit`: settle a trial's verdict and say it.
    fn emit_trial(&mut self, index: usize, accepted: bool) -> usize {
        self.trials[index].accepted = accepted;
        let detail = self.trials[index].to_map();
        self.emit(Event::BenchTrial {
            bid: self.bid(),
            detail,
        });
        index
    }

    /// `_decidable`: may these two trials be compared at all?
    fn decidable(&self, candidate: usize, against: usize) -> bool {
        let (c, a) = (&self.trials[candidate], &self.trials[against]);
        if !c.short_window() && !a.short_window() {
            return true;
        }
        let which = if c.short_window() {
            "the candidate"
        } else {
            "the trial it is measured against"
        };
        tracing::info!(
            "benchmark {}: trial {} cannot decide anything -- {which} has a {:.1}s window; add pages to the sample",
            self.op.bid,
            c.n,
            c.window.window_seconds.min(a.window.window_seconds)
        );
        false
    }

    fn out_of_time(&self) -> bool {
        Instant::now() >= self.deadline || self.trials.len() >= self.max_trials
    }

    fn tunable(&self, widths: &[u32]) -> bool {
        widths.iter().zip(&self.ceilings).any(|(w, c)| w < c)
    }

    /// `search(derived, start)`: `(baseline, best, widths)` as trial indexes.
    fn search(
        &mut self,
        derived: &[u32],
        start: Option<usize>,
    ) -> Result<(usize, usize, Vec<u32>), BenchAbort> {
        let mut widths = derived.to_vec();
        let baseline = match start {
            Some(s) => s,
            None => {
                let t = self.trial(&widths, "auto")?;
                self.emit_trial(t, true)
            }
        };
        let mut best = baseline;
        let mut seen: BTreeSet<Vec<u32>> = BTreeSet::new();
        seen.insert(widths.clone());

        // 1. WIDEN what the pipeline says is in the way, while it pays.
        while !self.out_of_time() {
            let target = self.trials[best].reading.as_ref().and_then(widen_target);
            let Some(index) = target
                .as_ref()
                .and_then(|t| self.keys.iter().position(|k| k == t))
            else {
                tracing::info!(
                    "benchmark {}: nothing left to widen ({})",
                    self.op.bid,
                    target.as_deref().unwrap_or("balanced")
                );
                break;
            };
            let name = self.keys[index].clone();
            if widths[index] >= self.ceilings[index] {
                tracing::info!(
                    "benchmark {}: {name} is already at its ceiling {}",
                    self.op.bid,
                    self.ceilings[index]
                );
                break;
            }
            let mut candidate = widths.clone();
            candidate[index] += 1;
            if !seen.insert(candidate.clone()) {
                break;
            }
            let t = self.trial(&candidate, &format!("widen {name} to {}", candidate[index]))?;
            if !self.decidable(t, best) {
                self.emit_trial(t, false);
                break;
            }
            if self.trials[t].pages_per_second()
                >= self.trials[best].pages_per_second() * (1.0 + BENCH_GAIN)
            {
                self.emit_trial(t, true);
                widths = candidate;
                best = t;
                continue;
            }
            self.emit_trial(t, false);
            tracing::info!(
                "benchmark {}: widening {name} bought {:+.1}%; keeping {:?}",
                self.op.bid,
                (self.trials[t].pages_per_second() / self.trials[best].pages_per_second() - 1.0)
                    * 100.0,
                self.trials[best].stage_workers
            );
            break;
        }

        // 2. NARROW what is not being used, while throughput HOLDS (against the peak).
        let peak = self.trials[best].pages_per_second();
        let peak_trial = best;
        let mut order: Vec<usize> = (0..widths.len()).collect();
        // `sorted(range(n), key=-widths[i])`: stable, widest first.
        order.sort_by_key(|i| std::cmp::Reverse(widths[*i]));
        for index in order {
            while widths[index] > 1 && !self.out_of_time() {
                let mut candidate = widths.clone();
                candidate[index] -= 1;
                if !seen.insert(candidate.clone()) {
                    break;
                }
                let name = self.keys[index].clone();
                let t = self.trial(
                    &candidate,
                    &format!("narrow {name} to {}", candidate[index]),
                )?;
                if !self.decidable(t, peak_trial) {
                    self.emit_trial(t, false);
                    break;
                }
                if self.trials[t].pages_per_second() < peak * (1.0 - BENCH_HOLD) {
                    self.emit_trial(t, false);
                    break;
                }
                self.emit_trial(t, true);
                widths = candidate;
                best = t;
            }
        }
        Ok((baseline, best, widths))
    }

    /// The formats the model's device runs the row's mode at, as this machine reports
    /// it (`policy::device_formats`: bf16 only where native for the auto modes — the
    /// rule the engines resolve with and the library judges with).
    fn device_formats(&self, device: Option<&str>) -> BTreeSet<String> {
        let device = device.unwrap_or("cpu");
        match self.catalog.devices.iter().find(|d| d.id == device) {
            Some(d) => policy::device_formats(
                self.mode,
                &d.formats,
                d.provider.as_deref().or(Some("cpu")),
                d.arch.as_deref(),
            ),
            None => BTreeSet::from([policy::FP32.to_string()]),
        }
    }

    /// The candidates a balanced/speed row tries here (empty: nothing to try).
    fn precision_candidates(&self) -> Vec<String> {
        let engine = self.op.spec.engine.as_str();
        if !policy::is_benched(self.mode) || !policy::is_precision_engine(engine) {
            return Vec::new();
        }
        let device = self
            .current
            .as_ref()
            .and_then(|l| l.ready.stage_device.get("engine"))
            .map(String::as_str);
        let formats = self.device_formats(device);
        let usable = policy::resolve_mode(engine, self.mode, Some(&formats), None, "").usable;
        if usable.len() > 1 { usable } else { Vec::new() }
    }

    /// `precision_phase(widths)`: one trial per candidate; the winning trial's index.
    fn precision_phase(
        &mut self,
        widths: &[u32],
        usable: &[String],
    ) -> Result<Option<usize>, BenchAbort> {
        tracing::info!(
            "benchmark {}: precision trials -- {} ({}) at {:?}",
            self.op.bid,
            usable.join(", "),
            self.mode,
            self.map(widths)
        );
        self.candidates = usable.to_vec();
        let mut measured: Vec<(String, f64)> = Vec::new();
        let mut by_format: Vec<(String, usize)> = Vec::new();
        for candidate in usable {
            if !measured.is_empty() && Instant::now() >= self.deadline {
                tracing::info!(
                    "benchmark {}: out of time before trying {candidate}",
                    self.op.bid
                );
                break;
            }
            self.check()?;
            if let Err(e) = self.ensure(widths, Some(candidate)) {
                tracing::warn!(
                    "benchmark {}: could not switch to {candidate}: {e}",
                    self.op.bid
                );
                self.unrunnable.push(candidate.clone());
                continue;
            }
            self.precision = Some(candidate.clone());
            let t = self.trial(widths, &format!("precision {candidate}"))?;
            self.trials[t].precision = Some(candidate.clone());
            self.precision_trials.push(t + 1);
            by_format.push((candidate.clone(), t));
            measured.push((candidate.clone(), self.trials[t].pages_per_second()));
            tracing::info!(
                "benchmark {}: {candidate}: {:.3} pages/s",
                self.op.bid,
                self.trials[t].pages_per_second()
            );
        }
        let Some((chosen, why)) = policy::pick_precision(&measured, usable) else {
            self.settle(&usable[0], widths)?;
            return Ok(None);
        };
        for (fmt, t) in &by_format {
            self.emit_trial(*t, *fmt == chosen);
        }
        self.settle(&chosen, widths)?;
        tracing::info!(
            "benchmark {}: {} precision: {chosen} ({}; {why})",
            self.op.bid,
            self.op.spec.engine,
            self.mode
        );
        self.precision_why = why;
        Ok(by_format
            .iter()
            .find(|(f, _)| *f == chosen)
            .map(|(_, t)| *t))
    }

    /// `_settle_precision`: every later runner runs `chosen` (loaded again when the
    /// phase ended on another candidate).
    fn settle(&mut self, chosen: &str, widths: &[u32]) -> Result<(), BenchAbort> {
        self.settled = Some(chosen.to_string());
        self.precision = Some(chosen.to_string());
        self.ensure(widths, Some(chosen)).map_err(BenchAbort::Fatal)
    }

    /// `peak_rss_mb` / the card's VRAM rise.
    fn peaks(&self) -> (Value, Value) {
        (
            peak_rss_mb().map_or(Value::Null, |m| json!(m)),
            self.sampler
                .as_ref()
                .and_then(Sampler::vram_rise_mb)
                .map_or(Value::Null, |m| json!(m)),
        )
    }

    /// The whole benchmark. `Ok` once `bench_done` has been said.
    pub fn run(mut self) -> Result<(), BenchAbort> {
        let result = self.run_inner();
        if let Some(mut s) = self.sampler.take() {
            s.stop();
        }
        self.current = None;
        result
    }

    fn run_inner(&mut self) -> Result<(), BenchAbort> {
        self.origin = Instant::now();
        self.deadline = self.origin + self.config.budget;
        let names = sample_pages(&self.sample).map_err(BenchAbort::Fatal)?;
        if names.is_empty() {
            return Err(BenchAbort::Fatal(format!(
                "no page images found under {}",
                self.sample.display()
            )));
        }
        self.pages = names.len();
        let warm_pages = &names[..names.len().min(self.config.warmup_pages.max(1))];
        let warm = self.workspace.join("warm-up.cbz");
        write_prefix(&self.sample, warm_pages, &warm)
            .map_err(|e| BenchAbort::Fatal(format!("could not prepare the warm-up pages: {e}")))?;
        self.catalog = self.pipeline.describe().catalog;
        self.sampler = Some(Sampler::start(
            device_index(
                self.op
                    .spec
                    .pools
                    .stage_device
                    .get("engine")
                    .map(String::as_str),
            ),
            self.origin,
            self.config.sample_interval,
        ));
        self.check()?;

        // Load once, as a session would (the row's mode decides the first format).
        let first = self.open(None, None).map_err(BenchAbort::Fatal)?;
        let loaded = self.since_origin();
        let report = first.runner.stats();
        self.keys = match &report {
            Some(r) if !r.stages.is_empty() => r.stages.iter().map(|s| s.key.clone()).collect(),
            _ => first.ready.stage_workers.keys().cloned().collect(),
        };
        let derived = self.widths_of(&first.ready);
        let asked_devices = first.ready.stage_device.clone();
        let budget = self
            .config
            .workers_budget
            .unwrap_or_else(|| host_worker_budget(logical_cpu_count(), self.config.jobs))
            .max(1);
        let told = first.runner.width_ceilings();
        self.ceilings = self
            .keys
            .iter()
            .zip(&derived)
            .map(|(k, d)| match told.get(k) {
                Some(c) => (*c).max(*d).max(1),
                None => estimated_ceiling(
                    report
                        .as_ref()
                        .and_then(|r| r.stages.iter().find(|s| &s.key == k)),
                    &self.op.spec.engine,
                    budget,
                    *d,
                ),
            })
            .collect();
        self.precision = first.ready.precision.clone();
        let mut first = first;
        first.widths = derived.clone();
        self.current = Some(first);
        self.check()?;

        // A short discarded pass: the first trial does not pay for the first page.
        let (_, warm_emissions) =
            match self.feed(&warm, warm_pages.len(), 1, 0, &[], 0, 1, &derived) {
                Ok(done) => done,
                Err(BenchAbort::Fatal(e)) => {
                    return Err(BenchAbort::Fatal(format!("the warm-up pass failed: {e}")));
                }
                Err(e) => return Err(e),
            };
        let _ = std::fs::remove_file(&warm);
        let startup = warm_emissions
            .first()
            .copied()
            .unwrap_or_else(|| self.since_origin());
        tracing::info!(
            "benchmark {}: first page after {startup:.1}s (of which {loaded:.1}s was loading the models)",
            self.op.bid
        );
        let tunable = self.tunable(&derived) && !self.op.precision_only;
        let usable = self.precision_candidates();
        let phase = usable.len();
        self.max_trials += phase;
        let mut ready = Map::new();
        ready.insert("startup_seconds".into(), r3(startup));
        ready.insert("model_load_seconds".into(), r3(loaded));
        ready.insert(
            "min_window_seconds".into(),
            float_value(self.config.rules.min_window_seconds),
        );
        ready.insert("pages".into(), json!(self.pages));
        ready.insert("tunable".into(), json!(tunable));
        ready.insert(
            "max_trials".into(),
            json!(if tunable { self.max_trials - phase } else { 1 } + phase),
        );
        ready.insert("stage_keys".into(), json!(self.keys));
        ready.insert("stage_device".into(), json!(asked_devices));
        self.emit(Event::BenchReady {
            bid: self.bid(),
            detail: ready,
        });

        let winner = if phase > 0 {
            self.precision_phase(&derived, &usable)?
        } else {
            None
        };
        if let Some(p) = &self.precision {
            tracing::info!(
                "benchmark {}: the recognizer runs at {p} from here on",
                self.op.bid
            );
        }
        let (baseline, best, widths) = if !tunable {
            tracing::info!(
                "benchmark {}: {}",
                self.op.bid,
                if self.op.precision_only {
                    "precision only; the pools stay as given"
                } else {
                    "nothing on this road can be widened; one trial only"
                }
            );
            let baseline = match winner {
                Some(w) => w,
                None => {
                    let t = self.trial(&derived, "auto")?;
                    self.emit_trial(t, true)
                }
            };
            (baseline, baseline, derived.clone())
        } else {
            // `place()` finds nothing to move: the detector runs on the CPU only.
            self.search(&derived, winner)?
        };
        let placed: BTreeMap<String, String> = self
            .current
            .as_ref()
            .map(|l| l.ready.stage_device.clone())
            .unwrap_or_default();
        let (b, w) = (&self.trials[baseline], &self.trials[best]);
        let per_page = |pps: f64| {
            if pps > 0.0 {
                r4(1.0 / pps)
            } else {
                Value::Null
            }
        };
        let mut base = Map::new();
        base.insert("pages_per_second".into(), r4(b.pages_per_second()));
        base.insert("seconds_per_page".into(), per_page(b.pages_per_second()));
        base.extend(b.window.to_map());
        let mut top = Map::new();
        top.insert("trial".into(), json!(w.n));
        top.insert(
            "stage_workers".into(),
            Value::Object(
                self.keys
                    .iter()
                    .zip(widths.iter().zip(&derived))
                    .filter(|(_, (w, d))| w != d)
                    .map(|(k, (w, _))| (k.clone(), json!(w)))
                    .collect(),
            ),
        );
        top.insert("queue_capacity".into(), json!({}));
        top.insert(
            "stage_device".into(),
            Value::Object(
                placed
                    .iter()
                    .filter(|(k, v)| asked_devices.get(*k).is_some_and(|a| a != *v))
                    .map(|(k, v)| (k.clone(), json!(v)))
                    .collect(),
            ),
        );
        top.insert("pages_per_second".into(), r4(w.pages_per_second()));
        top.insert("seconds_per_page".into(), per_page(w.pages_per_second()));
        top.insert(
            "speedup".into(),
            if b.pages_per_second() > 0.0 {
                r4(w.pages_per_second() / b.pages_per_second())
            } else {
                float_value(1.0)
            },
        );
        top.extend(w.window.to_map());
        let mut done = Map::new();
        done.insert("baseline".into(), Value::Object(base));
        done.insert("best".into(), Value::Object(top));
        if let Some(p) = &self.precision {
            done.insert("precision".into(), json!(p));
            done.insert("precision_mode".into(), json!(self.mode));
        }
        if !self.precision_trials.is_empty() {
            let mut trials: Vec<Value> = Vec::new();
            for candidate in &self.candidates {
                if self.unrunnable.contains(candidate) {
                    trials.push(json!({
                        "precision": candidate,
                        "pages_per_second": float_value(0.0),
                        "chosen": false,
                    }));
                    continue;
                }
                for n in &self.precision_trials {
                    let t = &self.trials[n - 1];
                    if t.precision.as_deref() == Some(candidate.as_str()) {
                        trials.push(json!({
                            "precision": t.precision,
                            "pages_per_second": r4(t.pages_per_second()),
                            "chosen": t.precision == self.precision,
                        }));
                    }
                }
            }
            done.insert("precision_trials".into(), Value::Array(trials));
            done.insert("precision_why".into(), json!(self.precision_why));
        }
        let (rss, vram) = self.peaks();
        done.insert("peak_rss_mb".into(), rss);
        done.insert("peak_vram_mb".into(), vram);
        self.emit(Event::BenchDone {
            bid: self.bid(),
            detail: done,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_and_ceilings_match_the_engines() {
        assert_eq!(host_worker_budget(32, 1), 14);
        assert_eq!(host_worker_budget(16, 1), 6);
        assert_eq!(host_worker_budget(16, 2), 2);
        assert_eq!(host_worker_budget(4, 1), 1);
        assert!(physical_cpu_count() >= 1 && logical_cpu_count() >= 1);
        let gpu = crate::pipeline::StageReport {
            key: "engine".into(),
            device: "gpu:0".into(),
            device_bound: true,
            ..Default::default()
        };
        let cpu = crate::pipeline::StageReport {
            device: "cpu".into(),
            ..gpu.clone()
        };
        assert_eq!(estimated_ceiling(Some(&gpu), "hayai-nova", 14, 2), 8);
        assert_eq!(estimated_ceiling(Some(&gpu), "hayai-nova", 3, 1), 3);
        assert_eq!(estimated_ceiling(Some(&cpu), "hayai-nova", 14, 1), 1);
        assert_eq!(estimated_ceiling(None, "hayai-nova", 14, 6), 8);
        assert_eq!(estimated_ceiling(None, "ppocr-manga", 14, 2), 4);
        assert_eq!(
            estimated_ceiling(None, "hayai-nova", 2, 6),
            6,
            "never below the derivation"
        );
    }

    #[test]
    fn sample_pages_and_warm_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let sample = dir.path().join("s.cbz");
        {
            let f = std::fs::File::create(&sample).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let o = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for name in ["0010_b.jpg", "0002_a.png", "notes.txt", "0001_a.jpg"] {
                w.start_file(name, o).unwrap();
                w.write_all(name.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        let pages = sample_pages(&sample).unwrap();
        assert_eq!(pages, ["0001_a.jpg", "0002_a.png", "0010_b.jpg"]);
        let warm = dir.path().join("w.cbz");
        write_prefix(&sample, &pages[..2], &warm).unwrap();
        assert_eq!(sample_pages(&warm).unwrap(), ["0001_a.jpg", "0002_a.png"]);
    }
}
