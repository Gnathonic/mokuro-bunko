//! Shared harness of the OCR scheduler tests: a scheduler on a manual clock with inline
//! helper work, fake processors speaking `Op`/`Event` over channels, and a library.

#![allow(dead_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bunko_core::StorageLayout;
use bunko_core::config::UpgradeConfig;
use bunko_core::generations::{Generation, default_generation};
use bunko_proto::{Event, Op};
use bunko_sched::rate::ManualClock;
use bunko_server::ocr::sched::{
    Exec, Msg, RegisterInput, RegisterOutcome, SchedDeps, Scheduler, Settings,
};
use bunko_server::ocr::types::{FileFacts, LibraryFacts};
use serde_json::json;
use tokio::sync::mpsc;

/// The manual clock starts a little after the real now (archives written by the test
/// must be older than every failure record, as they would be).
pub fn t0() -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    (now + 100.0).floor()
}

pub struct H {
    pub t0: f64,
    pub dir: tempfile::TempDir,
    pub s: Scheduler,
    pub clock: Arc<ManualClock>,
}

pub fn primary() -> Generation {
    default_generation("g-1")
}

pub fn layer(id: &str, name: &str) -> Generation {
    let mut g = default_generation(id);
    g.name = name.into();
    g.primary = false;
    g
}

pub fn settings(rows: Vec<Generation>) -> Settings {
    Settings {
        rows,
        poll_interval: 30.0,
        local_processing: false,
        concurrency: 1,
        autobench: false,
        upgrade: UpgradeConfig::default(),
    }
}

pub fn harness(rows: Vec<Generation>) -> H {
    harness_with(rows, Arc::new(FileFacts))
}

pub fn harness_with(rows: Vec<Generation>, facts: Arc<dyn LibraryFacts>) -> H {
    harness_full(rows, facts, None)
}

/// With generation upgrades configured (`ocr.upgrade`).
pub fn harness_full(
    rows: Vec<Generation>,
    facts: Arc<dyn LibraryFacts>,
    upgrade: Option<UpgradeConfig>,
) -> H {
    let dir = tempfile::tempdir().unwrap();
    let layout = StorageLayout::new(dir.path());
    layout.ensure_directories().unwrap();
    let up = upgrade.map(|cfg| {
        let u = Arc::new(bunko_server::ocr::upgrade::Upgrade::new(
            layout.library(),
            None,
            facts.clone(),
            Arc::new(bunko_library::service::NoPathLocks),
            "mokuro-bunko test".into(),
        ));
        u.configure(&cfg, &rows);
        u
    });
    let t0 = t0();
    let clock = Arc::new(ManualClock::new(t0));
    let s = Scheduler::new(
        SchedDeps {
            layout,
            db: None,
            facts,
            locks: Arc::new(bunko_library::service::NoPathLocks),
            clock: clock.clone(),
            exec: Exec::Inline,
            upgrade: up,
            generator: "mokuro-bunko test".into(),
            version: "0.7.0-test".into(),
        },
        settings(rows),
    );
    H { t0, dir, s, clock }
}

/// A `.cbz` with `pages` image members.
pub fn write_cbz(path: &Path, pages: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let f = std::fs::File::create(path).unwrap();
    let mut z = zip::ZipWriter::new(f);
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..pages {
        z.start_file(format!("{:03}.jpg", i + 1), opts).unwrap();
        z.write_all(b"not really a jpeg").unwrap();
    }
    z.finish().unwrap();
}

/// A fake remote processor's end of the link.
pub struct Proc {
    pub pid: String,
    pub name: String,
    pub ops: mpsc::UnboundedReceiver<Op>,
}

impl H {
    pub fn library(&self) -> PathBuf {
        self.dir.path().join("library")
    }

    pub fn storage(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    pub fn add(&self, rel: &str, pages: usize) -> PathBuf {
        let p = self.library().join(rel);
        write_cbz(&p, pages);
        p
    }

    /// Advance the clock to `t0 + dt` and tick.
    pub fn at(&mut self, dt: f64) {
        self.clock.set(self.t0 + dt);
        self.s.handle(Msg::Tick);
    }

    /// Run `f` on the scheduler as a message (so everything it causes follows).
    pub fn run(&mut self, f: impl FnOnce(&mut Scheduler) + Send + 'static) {
        self.s.handle(Msg::Query(Box::new(f)));
    }

    /// Register + open the socket of a processor that runs every engine.
    pub fn connect(&mut self, name: &str, sessions: u32) -> Proc {
        self.connect_with(
            name,
            sessions,
            json!(["hayai-nova", "paddle-manga", "ppocr-manga"]),
        )
    }

    pub fn connect_with(&mut self, name: &str, sessions: u32, engines: serde_json::Value) -> Proc {
        let catalog = json!({"engines": engines, "detectors": ["ppocr-manga"], "devices": [{"id": "cpu", "label": "CPU"}]});
        self.connect_catalog(name, sessions, catalog)
    }

    /// A processor with a GPU computing in `formats`, running every engine.
    pub fn connect_gpu(&mut self, name: &str, formats: &[&str]) -> Proc {
        let catalog = json!({
            "engines": ["hayai-nova", "paddle-manga", "ppocr-manga"],
            "detectors": ["ppocr-manga"],
            "devices": [{"id": "cpu", "label": "CPU"},
                        {"id": "gpu:0", "label": "GPU 0", "formats": formats, "provider": "cuda", "arch": "sm_89"}],
        });
        self.connect_catalog(name, 1, catalog)
    }

    pub fn connect_catalog(
        &mut self,
        name: &str,
        sessions: u32,
        catalog: serde_json::Value,
    ) -> Proc {
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        let body = json!({
            "protocol": 3,
            "name": name,
            "host": {"cpu": "Test CPU (8 cores)", "gpu": "Test GPU", "backend": "cuda"},
            "catalog": catalog,
            "max_sessions": sessions,
        });
        self.s.handle(Msg::Register {
            input: RegisterInput {
                username: format!("acct-{name}"),
                body,
                account_stamp: None,
            },
            reply,
        });
        let pid = match rx.try_recv().unwrap() {
            RegisterOutcome::Ok(r) => r.processor_id,
            other => panic!("register refused: {other:?}"),
        };
        let (tx, ops) = mpsc::unbounded_channel();
        let (reply, mut rx) = tokio::sync::oneshot::channel();
        self.s.handle(Msg::SocketOpen {
            pid: pid.clone(),
            username: format!("acct-{name}"),
            ops: tx,
            reply,
        });
        rx.try_recv().unwrap().unwrap();
        Proc {
            pid,
            name: name.into(),
            ops,
        }
    }

    pub fn event(&mut self, p: &Proc, event: Event) {
        self.s.handle(Msg::Event {
            pid: p.pid.clone(),
            event,
        });
    }

    /// Answer a volume as done: the sidecar lands where an upload would put it.
    pub fn done(&mut self, p: &Proc, sid: &str, claim: &str, name: &str, pages: u32, seconds: f64) {
        self.done_with(
            p,
            sid,
            claim,
            name,
            pages,
            seconds,
            br#"{"version":"0.2.5","title":"x","volume":"y","pages":[]}"#,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn done_with(
        &mut self,
        p: &Proc,
        sid: &str,
        claim: &str,
        name: &str,
        pages: u32,
        seconds: f64,
        body: &[u8],
    ) {
        let dir = self.storage().join(".processing").join(sid).join(claim);
        std::fs::create_dir_all(&dir).unwrap();
        let stored = dir.join(bunko_proto::RESULT_FILE);
        std::fs::write(&stored, body).unwrap();
        let sha = bunko_server::ocr::collect::sha256_file(&stored).unwrap();
        self.s.handle(Msg::ResultStored {
            pid: p.pid.clone(),
            sid: sid.into(),
            claim: claim.into(),
            name: name.into(),
            sha256: sha.clone(),
        });
        self.event(
            p,
            Event::VolumeDone {
                sid: sid.into(),
                id: claim.into(),
                pages,
                failed_pages: 0,
                seconds,
                stats: serde_json::Value::Null,
                cpu_pressure: None,
                other_cpu: None,
                sidecar_sha256: Some(sha),
            },
        );
    }
}

impl Proc {
    pub fn drain(&mut self) -> Vec<Op> {
        let mut out = Vec::new();
        while let Ok(op) = self.ops.try_recv() {
            out.push(op);
        }
        out
    }
}

/// `(sid, claim, archive)` of every volume op.
pub fn volumes(ops: &[Op]) -> Vec<(String, String, String)> {
    ops.iter()
        .filter_map(|o| match o {
            Op::Volume(v) => Some((v.sid.clone(), v.claim.clone(), v.archive.clone())),
            _ => None,
        })
        .collect()
}

pub fn opened(ops: &[Op]) -> Vec<String> {
    ops.iter()
        .filter_map(|o| match o {
            Op::OpenSession { sid, .. } => Some(sid.clone()),
            _ => None,
        })
        .collect()
}

pub fn ready(sid: &str) -> Event {
    Event::Ready {
        sid: sid.into(),
        startup_seconds: 1.0,
        weights: Default::default(),
        stage_workers: Default::default(),
        queue_capacity: Default::default(),
        stage_device: Default::default(),
        pipeline: String::new(),
        precision: None,
    }
}

pub fn started(sid: &str, claim: &str, pages: u32) -> Event {
    Event::VolumeStarted {
        sid: sid.into(),
        id: claim.into(),
        pages,
    }
}

pub fn exit(sid: &str, code: Option<i32>) -> Event {
    Event::Exit {
        sid: sid.into(),
        returncode: code,
    }
}
