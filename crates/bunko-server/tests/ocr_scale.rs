//! The OCR scheduler at a large library's scale. A library of ~2,000 volumes owed 5
//! generations (~10,000 jobs) runs OCR of its own, and a remote GPU processor with none
//! of the 5 rows benchmarked connects, drops and registers again. A busy scheduler
//! thread made such a processor's registration time out in a loop, and made the queue
//! page take from 1.5 s to over 30 s. These tests time what runs on the scheduler
//! thread, so a regression shows as a number, not as a stalled server.
//!
//! They time one message at a time on an idle scheduler. The waits over 30 s seen with
//! such a library came from these messages queueing behind each other (the queue page
//! and the queue file every 2 s, the local OCR's events, a processor's reconnects),
//! which they do not replay: the largest single cost measured before the fix was about
//! 6 s. They measure only the code they run on; the "before" figures quoted for the
//! fix were taken on its parent commit (473d3fca).
//!
//! `scheduler_costs_at_scale` is ignored (it prints a table, and its size is set by
//! `OCR_SCALE_VOLUMES`, default 2000, over `OCR_SCALE_STORE` = `local`, `cloud` or
//! `both`, default both):
//!
//! ```text
//! cargo test -p bunko-server --release --test ocr_scale -- --ignored --nocapture
//! ```

// Fixtures are written with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use bunko_core::StorageLayout;
use bunko_core::generations::Generation;
use bunko_proto::{Event, Op};
use bunko_remote::fake::FakeBackend;
use bunko_remote::{RemoteStore, RemoteStoreConfig};
use bunko_sched::rate::ManualClock;
use bunko_server::ocr::sched::{
    Exec, Msg, RegisterInput, RegisterOutcome, SchedDeps, Scheduler, Settings,
};
use bunko_server::ocr::types::FileFacts;
use serde_json::json;
use tokio::sync::mpsc;

mod ocr_common;
use ocr_common::{ENGINES, current_revisions, layer, primary};

/// What the remote processor's GPU computes in.
const GPU_FORMATS: &[&str] = &["fp32", "fp16", "bf16"];
/// Volumes per series folder.
const PER_SERIES: usize = 20;
const ROOT: &str = "mokuro-reader";

/// The 5 rows of the report: the primary and 4 layers.
fn rows() -> Vec<Generation> {
    let mut rows = vec![primary()];
    for i in 2..=5 {
        rows.push(layer(&format!("g-{i}"), &format!("layer{i}")));
    }
    rows
}

/// A `.cbz` of 4 tiny stored pages.
fn cbz_bytes() -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..4 {
        z.start_file(format!("{:03}.jpg", i + 1), opts).unwrap();
        z.write_all(b"not really an image").unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn rel_of(i: usize) -> String {
    format!("S{:04}/V{:02}.cbz", i / PER_SERIES, i % PER_SERIES)
}

/// A scheduler over a library of `volumes` volumes, its helper work on threads as in
/// the server (the timings are the scheduler thread's own), local or in a fake Drive.
struct Scale {
    s: Scheduler,
    clock: Arc<ManualClock>,
    t0: f64,
    helpers: Receiver<Msg>,
    local: mpsc::UnboundedReceiver<Op>,
    /// The release the remote processor registers with.
    version: String,
    _mount: Option<bunko_store::MountGuard>,
    _store: Option<Arc<RemoteStore<FakeBackend>>>,
    _dir: tempfile::TempDir,
}

impl Scale {
    fn new(volumes: usize, cloud: bool) -> Scale {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        let mut layout = StorageLayout::new(&base);
        layout.ensure_directories().unwrap();
        let bytes = cbz_bytes();
        let (mount, store) = if cloud {
            let fake = FakeBackend::drive_like();
            fake.reader_mkdir(ROOT);
            for i in 0..volumes {
                fake.reader_put(&format!("{ROOT}/{}", rel_of(i)), &bytes);
            }
            let mut cfg =
                RemoteStoreConfig::new(base.join("cloud/s/library"), base.join("cloud/s"), ROOT);
            cfg.poll_active = Duration::from_secs(3600);
            cfg.poll_idle = Duration::from_secs(3600);
            cfg.lease = false;
            let store = RemoteStore::open(cfg, fake).unwrap();
            assert!(store.wait_steady(Duration::from_secs(120)));
            let mount = bunko_store::mount(store.clone() as Arc<dyn bunko_store::LibraryStore>);
            layout = layout
                .with_cloud_library(true)
                .with_library_root(bunko_store::LibraryStore::root(&*store));
            (Some(mount), Some(store))
        } else {
            for i in 0..volumes {
                let p = layout.library().join(rel_of(i));
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, &bytes).unwrap();
            }
            (None, None)
        };
        let t0 = ocr_common::t0();
        let clock = Arc::new(ManualClock::new(t0));
        let (tx, helpers) = std::sync::mpsc::channel();
        let s = Scheduler::new(
            SchedDeps {
                layout,
                db: None,
                facts: Arc::new(FileFacts),
                locks: Arc::new(bunko_library::service::NoPathLocks),
                clock: clock.clone(),
                exec: Exec::Threads(tx),
                upgrade: None,
                generator: "mokuro-bunko test".into(),
                version: "0.8.0-test".into(),
            },
            Settings {
                rows: rows(),
                poll_interval: 30.0,
                local_processing: true,
                concurrency: 2,
                autobench: true,
                upgrade: Default::default(),
            },
        );
        let (_, local) = mpsc::unbounded_channel();
        let mut scale = Scale {
            s,
            clock,
            t0,
            helpers,
            local,
            version: "0.8.0-test".into(),
            _mount: mount,
            _store: store,
            _dir: dir,
        };
        scale.s.handle(Msg::Tick);
        let started = Instant::now();
        while scale.s.walking {
            assert!(
                started.elapsed() < Duration::from_secs(600),
                "the walk ends"
            );
            scale.settle(Duration::from_millis(100));
        }
        scale
    }

    /// Hand the scheduler what its helpers answered, until they are quiet for `quiet`.
    fn settle(&mut self, quiet: Duration) {
        while let Ok(msg) = self.helpers.recv_timeout(quiet) {
            self.s.handle(msg);
        }
    }

    fn at(&mut self, dt: f64) {
        self.clock.set(self.t0 + dt);
        self.s.handle(Msg::Tick);
    }

    /// This server's own OCR, with each row measured (autobench answered), its two
    /// lanes then running sessions as a busy server's do.
    fn local_up(&mut self) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.local = rx;
        let catalog: bunko_proto::Catalog = serde_json::from_value(json!({
            "engines": ENGINES,
            "detectors": ["ppocr-manga"],
            "devices": [{"id": "cpu", "label": "CPU"}],
            "engine_revisions": current_revisions(ENGINES),
        }))
        .unwrap();
        self.s.handle(Msg::LocalUp {
            ops: tx,
            catalog,
            host: bunko_proto::HostInfo {
                cpu: "Test CPU".into(),
                backend: "cpu".into(),
                ..Default::default()
            },
        });
        let mut measured = 0;
        while measured < rows().len() {
            self.settle(Duration::from_millis(50));
            let mut bids = Vec::new();
            while let Ok(op) = self.local.try_recv() {
                if let Op::Bench(b) = op {
                    bids.push(b.bid);
                }
            }
            assert!(!bids.is_empty(), "this server's autobench runs");
            for bid in bids {
                self.s.handle(Msg::Event {
                    pid: "local".into(),
                    event: bench_done(&bid, &["fp32"]),
                });
                measured += 1;
            }
        }
        self.settle(Duration::from_millis(50));
    }

    /// Register "tower": a GPU processor running every engine, one session slot.
    fn register(&mut self) -> String {
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        self.s.handle(Msg::Register {
            input: RegisterInput {
                username: "acct-tower".into(),
                body: json!({
                    "protocol": 3,
                    "name": "tower",
                    "host": {"cpu": "Test CPU (16 cores)", "gpu": "Test GPU", "backend": "cuda", "version": self.version},
                    "catalog": {
                        "engines": ENGINES,
                        "detectors": ["ppocr-manga"],
                        "devices": [{"id": "cpu", "label": "CPU"},
                                    {"id": "gpu:0", "label": "GPU 0", "formats": GPU_FORMATS, "provider": "cuda", "arch": "sm_89"}],
                        "engine_revisions": current_revisions(ENGINES),
                    },
                    "max_sessions": 1,
                }),
                account_stamp: None,
            },
            reply,
        });
        match rx.try_recv().unwrap() {
            RegisterOutcome::Ok(r) => r.processor_id,
            other => panic!("register refused: {other:?}"),
        }
    }

    fn socket_open(&mut self, pid: &str) -> mpsc::UnboundedReceiver<Op> {
        let (tx, ops) = mpsc::unbounded_channel();
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        self.s.handle(Msg::SocketOpen {
            pid: pid.into(),
            username: "acct-tower".into(),
            ops: tx,
            reply,
        });
        rx.try_recv().unwrap().unwrap();
        ops
    }

    fn drop_processor(&mut self, pid: &str, reason: &str) {
        self.s.handle(Msg::Drop {
            pid: pid.into(),
            reason: reason.into(),
        });
    }

    /// Run `f` on the scheduler as one message and time it (what `ocr.ask` costs).
    fn timed<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut Scheduler) -> T + Send + 'static,
    ) -> (Duration, T) {
        let out = Arc::new(parking_lot::Mutex::new(None));
        let slot = out.clone();
        let started = Instant::now();
        self.s.handle(Msg::Query(Box::new(move |s| {
            *slot.lock() = Some(f(s));
        })));
        let took = started.elapsed();
        let value = out.lock().take().expect("the query ran");
        (took, value)
    }

    /// [`Self::timed`], the median of [`RUNS`] runs (the machine may be busy).
    fn median<T: Send + 'static>(
        &mut self,
        f: impl Fn(&mut Scheduler) -> T + Clone + Send + 'static,
    ) -> (Duration, T) {
        let mut took = Vec::new();
        let mut last = None;
        for _ in 0..RUNS {
            let (d, v) = self.timed(f.clone());
            took.push(d);
            last = Some(v);
        }
        took.sort();
        (took[RUNS / 2], last.expect("ran"))
    }
}

/// Runs of a repeatable measurement; the median is reported.
const RUNS: usize = 5;

/// A finished benchmark, at the precision the row's mode resolves to on a device
/// computing in `formats` (a result at any other reads as stale there).
fn bench_done(bid: &str, formats: &[&str]) -> Event {
    let row = primary();
    let supported = formats.iter().map(|f| f.to_string()).collect();
    let precision = bunko_sched::precision::resolve_mode(
        &row.engine,
        &row.precision,
        Some(&supported),
        None,
        "",
    )
    .precision
    .unwrap_or_else(|| "fp32".into());
    Event::BenchDone {
        bid: bid.into(),
        detail: json!({
            "baseline": {"pages_per_second": 4.0},
            "best": {"trial": 1, "stage_workers": {}, "queue_capacity": {}, "stage_device": {}, "pages_per_second": 4.0},
            "precision": precision, "precision_mode": row.precision,
        })
        .as_object()
        .unwrap()
        .clone(),
    }
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

/// What the queue page's rebuild asks of the scheduler: its version, then the raw
/// status shaped (`queue_api::build`; the body is serialized off the scheduler thread).
fn status_build(s: &mut Scheduler) -> serde_json::Value {
    let _ = s.page_version();
    let raw = s.raw_status();
    bunko_server::ocr::shape::shape_status(&raw, "full", true, &mut s.public_names)
}

/// Volumes and jobs owed.
fn owed(s: &Scheduler) -> (usize, usize) {
    let jobs = s.owed.volumes.values().map(|v| v.rows.len()).sum();
    (s.owed.volumes.len(), jobs)
}

/// The bench runs queued or running on tower: their ids.
fn tower_bids(s: &Scheduler) -> Vec<String> {
    s.bench
        .lines
        .get("tower")
        .map(|l| l.iter().map(|r| r.bid.clone()).collect())
        .unwrap_or_default()
}

/// Tower's lane, if it has one.
fn tower_lane(s: &Scheduler, pid: &str) -> u64 {
    s.lanes
        .iter()
        .find(|l| l.pid == pid)
        .map(|l| l.id)
        .expect("tower has a lane")
}

/// The cost of one remote processor's arrival, a status poll, a queue file build and a
/// reconnect, before and after its rows are measured; then what those are made of.
fn measure(volumes: usize, cloud: bool, version: &str) {
    let what = if cloud {
        "cloud (fake Drive)"
    } else {
        "local disk"
    };
    let started = Instant::now();
    let mut x = Scale::new(volumes, cloud);
    x.version = version.to_string();
    let (v, j) = owed(&x.s);
    println!(
        "\n== {what}: {v} volumes, {j} jobs owed, processor release {version:?} (set up in {:.1} s; repeatable rows: median of {RUNS})",
        started.elapsed().as_secs_f64()
    );
    x.local_up();
    x.at(1.0);
    x.settle(Duration::from_millis(50));
    let note = |what: &str, d: Duration| println!("  {what:<62} {}", ms(d));

    // A processor with no benchmark of any row arrives.
    let t = Instant::now();
    let pid = x.register();
    note("register (unmeasured processor)", t.elapsed());
    let t = Instant::now();
    let mut ops = x.socket_open(&pid);
    note(
        "socket open (its first claim, 5 autobench asks)",
        t.elapsed(),
    );
    x.settle(Duration::from_millis(50));
    let mut seen: std::collections::HashSet<String> = tower_bids(&x.s).into_iter().collect();
    println!("  benchmarks announced on arrival: {}", seen.len());
    let lane = tower_lane(&x.s, &pid);
    // In a running server the claim that asks for the benchmarks starts one, and the
    // benchmark then holds the machine, so its later claims return at once: this cost
    // is paid about once per socket open, not after every change to the queue.
    let (d, _) = x.median(move |s| {
        // Its benchmark holds it; the claim of the moment it is not held (between two).
        let held = s.holds.remove("tower");
        let got = s.claim(lane, None);
        if let Some(n) = held {
            s.holds.insert("tower".into(), n);
        }
        got
    });
    note("one claim by its lane, rows unmeasured and asked", d);
    let (d, body) = x.median(|s| {
        s.bump();
        status_build(s)
    });
    let n = body.to_string().len();
    note(&format!("/queue/api/status build ({n} bytes)"), d);
    let (d, _) = x.median(|s| {
        s.bump();
        s.queue_document()
    });
    note("queue_document (the cloud queue file, every 2 s)", d);
    let rel = rel_of(volumes - 1);
    let (d, _) = x.median(move |s| s.volume_pending(&rel, None));
    note("volume_pending (a reader asks about one volume)", d);
    let t = Instant::now();
    x.s.handle(Msg::Tick);
    note("tick", t.elapsed());

    // The reconnect loop: the socket closes, it registers again, its socket opens.
    let mut pid = pid;
    for round in 1..=3 {
        let t = Instant::now();
        x.drop_processor(&pid, "the socket closed");
        pid = x.register();
        ops = x.socket_open(&pid);
        let took = t.elapsed();
        x.settle(Duration::from_millis(50));
        let bids = tower_bids(&x.s);
        let fresh = bids.iter().filter(|b| seen.insert((*b).clone())).count();
        note(
            &format!("reconnect round {round} (drop, register, open; {fresh} benchmark(s) anew)"),
            took,
        );
    }

    // What those are made of.
    let (d, n) = x.median(|s| s.candidates(&Default::default(), false).len());
    note(&format!("  candidates(): the queue in order ({n} jobs)"), d);
    let (d, n) = x.timed(|s| {
        let library = s.library();
        let jobs = s.candidates(&Default::default(), false);
        let rows = s.settings.rows.clone();
        let mut n = 0;
        for job in jobs.iter().take(1000) {
            let row = rows.iter().find(|r| r.id == *job.gid).unwrap();
            bunko_server::ocr::owed::still_owed_with(&library, &job.rel, row, job.upgrade, None);
            n += 1;
        }
        n
    });
    note(
        &format!(
            "  still_owed_with x {n} (per job: {:.0} us)",
            d.as_secs_f64() * 1e6 / n as f64
        ),
        d,
    );
    let (d, n) = x.timed(|s| {
        let row = s.settings.rows[0].clone();
        for _ in 0..1000 {
            let _ = s
                .profiles
                .row("tower", &row.id, Some(&row.output_affecting()));
        }
        1000
    });
    note(
        &format!(
            "  Profiles::row x {n} (per read: {:.0} us)",
            d.as_secs_f64() * 1e6 / n as f64
        ),
        d,
    );
    let (d, n) = x.median(|s| {
        s.bump();
        s.pending_jobs().len()
    });
    note(&format!("  pending_jobs, cold ({n} entries)"), d);
    let (d, _) = x.timed(|s| s.pending_jobs().len());
    note("  pending_jobs, warm", d);
    let (d, n) = x.median(|s| {
        let running = s.running_jobs();
        let pending = s.pending_jobs();
        let items = Scheduler::plan_items(&pending, &running);
        s.queue_plan(&running, &items, None).pending.len()
    });
    note(
        &format!("  queue_plan over the pending list ({n} items)"),
        d,
    );
    let (d, n) = x.median(|s| s.raw_status().len());
    note(&format!("  raw_status ({n} fields)"), d);
    let (_, (took, fields)) = x.timed(|s| {
        let raw = s.raw_status();
        let started = Instant::now();
        let shaped =
            bunko_server::ocr::shape::shape_status(&raw, "full", true, &mut s.public_names);
        (started.elapsed(), shaped.as_object().map_or(0, |o| o.len()))
    });
    note(&format!("  shape_status alone ({fields} fields)"), took);

    // Measure its 5 rows (the benchmarks answer), then the same costs again.
    let mut measured = 0;
    let deadline = Instant::now() + Duration::from_secs(300);
    while measured < rows().len() && Instant::now() < deadline {
        x.settle(Duration::from_millis(50));
        let mut bids = Vec::new();
        while let Ok(op) = ops.try_recv() {
            if let Op::Bench(b) = op {
                bids.push(b.bid);
            }
        }
        for bid in bids {
            x.s.handle(Msg::Event {
                pid: pid.clone(),
                event: bench_done(&bid, GPU_FORMATS),
            });
            measured += 1;
        }
    }
    assert_eq!(measured, rows().len(), "tower's 5 rows are measured");
    x.settle(Duration::from_millis(50));
    let lane = tower_lane(&x.s, &pid);
    let (d, _) = x.timed(move |s| s.claim(lane, None));
    note("one claim by its lane, rows measured", d);
    let (d, body) = x.median(|s| {
        s.bump();
        status_build(s)
    });
    let n = body.to_string().len();
    note(&format!("/queue/api/status build, measured ({n} bytes)"), d);
    let (d, _) = x.median(|s| {
        s.bump();
        s.queue_document()
    });
    note("queue_document, measured", d);
}

fn scale_volumes() -> usize {
    std::env::var("OCR_SCALE_VOLUMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

#[test]
#[ignore = "a measurement: run it with --ignored --nocapture"]
fn scheduler_costs_at_scale() {
    let volumes = scale_volumes();
    let store = std::env::var("OCR_SCALE_STORE").unwrap_or_else(|_| "both".into());
    // The reported processor ran the library's own release; an older one (any release
    // before every page format, e.g. `OCR_SCALE_PROCESSOR_VERSION=0.7.4`) has each
    // volume's pages looked at before it is offered one.
    let version =
        std::env::var("OCR_SCALE_PROCESSOR_VERSION").unwrap_or_else(|_| "0.8.0-test".into());
    if store != "cloud" {
        measure(volumes, false, &version);
    }
    if store != "local" {
        measure(volumes, true, &version);
    }
}

/// A processor that drops off while its benchmark's sample is still being packed gets
/// that sample on its return, sent at once: it is not packed again. Packing reads
/// volumes out of the library (provider reads on a cloud library) on a helper thread of
/// its own, and every reconnect of a looping processor started another.
#[test]
fn a_sample_packed_while_its_processor_was_away_is_sent_on_its_return() {
    let mut x = Scale::new(40, false);
    let pid = x.register();
    let _ = x.socket_open(&pid);
    let head = tower_bids(&x.s)[0].clone();
    // It drops off before the helper hands the sample over.
    x.drop_processor(&pid, "the socket closed");
    x.settle(Duration::from_millis(200));
    let parked = &x.s.bench.parked["tower"];
    assert_eq!(parked.runs[0].bid, head);
    assert!(parked.runs[0].built.is_some(), "the sample is kept for it");
    let pid = x.register();
    let mut ops = x.socket_open(&pid);
    let sent: Vec<String> = std::iter::from_fn(|| ops.try_recv().ok())
        .filter_map(|op| match op {
            Op::Bench(b) => Some(b.bid),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [head], "sent at once, from the packed sample");
    assert!(
        x.helpers.recv_timeout(Duration::from_millis(300)).is_err(),
        "no sample is packed again"
    );
    assert_eq!(tower_bids(&x.s).len(), rows().len(), "none asked for again");
}
