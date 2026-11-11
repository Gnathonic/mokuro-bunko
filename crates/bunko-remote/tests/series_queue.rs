//! The store sink of the OCR queue file v2 (`docs/design/queue-file-v2.md` §5, §6,
//! §12.1): each series' `.mokuro-series-queue.json` written through the outbox, the
//! marker's `series_queue` block, the settle rule, restarts, foreign files, the budget,
//! the early heartbeat, the store clock, and the outbox's dependency and class rules.
//! FakeBackend, real temp dirs, real time.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::series_queue::{SeriesQueueSource, SeriesQueueTiming, SnapshotFuture};
use bunko_remote::status::{LeaseEvent, MARKER_NAME, Marker};
use bunko_remote::{ApiClass, BErr, Backend, RemoteStore, RemoteStoreConfig};
use bunko_sched::series_queue::{LiveJob, LiveState, RowDesc, SeriesLive, Snapshot};
use bunko_store::{Durability, LibraryStore, Mode, MountGuard, Origin, WriteIntent, fs};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const FILE: &str = ".mokuro-series-queue.json";
const WAIT: Duration = Duration::from_secs(30);

type Store = RemoteStore<FakeBackend>;

fn wall() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The scheduler, as the test sets it: the queue (its `now` follows the wall clock)
/// and the series with a metadata recompile pending.
#[derive(Default)]
struct Src {
    snap: Mutex<Snapshot>,
    regen: Mutex<BTreeSet<String>>,
}

impl SeriesQueueSource for Src {
    fn snapshot(&self) -> SnapshotFuture<'_> {
        let mut s = self.snap.lock().clone();
        s.now = wall();
        Box::pin(async move { Some(s) })
    }

    fn regeneration_pending(&self, folder: &str) -> bool {
        self.regen.lock().contains(folder)
    }
}

impl Src {
    fn set(&self, f: impl FnOnce(&mut Snapshot)) {
        f(&mut self.snap.lock());
    }
}

fn rows() -> Vec<RowDesc> {
    vec![RowDesc {
        kind: "ocr".into(),
        id: "main".into(),
        parts: json!([{"role": "recognizer", "name": "Main", "version": "1"}]),
    }]
}

fn queued(e: Option<f64>) -> Vec<LiveJob> {
    vec![LiveJob {
        row: "main".into(),
        state: LiveState::Queued { e },
    }]
}

fn running(t0: f64, e: f64) -> Vec<LiveJob> {
    vec![LiveJob {
        row: "main".into(),
        state: LiveState::Running { t0, e: Some(e) },
    }]
}

/// Set series `folder`'s volumes (none: the series owes nothing).
fn put_series(s: &mut Snapshot, folder: &str, vols: Vec<(&str, Vec<LiveJob>)>) {
    let key = folder.to_lowercase();
    if vols.is_empty() {
        s.series.remove(&key);
        return;
    }
    s.series.insert(
        key,
        SeriesLive {
            folder: folder.into(),
            vols: vols
                .into_iter()
                .map(|(stem, jobs)| (stem.to_string(), jobs))
                .collect::<BTreeMap<_, _>>(),
        },
    );
}

struct T {
    tmp: TempDir,
    root: PathBuf,
    fake: Arc<FakeBackend>,
    store: Option<Arc<Store>>,
    mount: Option<MountGuard>,
    src: Arc<Src>,
    tune: fn(&mut RemoteStoreConfig),
}

fn no_tune(_: &mut RemoteStoreConfig) {}

impl T {
    fn new(fake: FakeBackend, files: &[(&str, &[u8])], tune: fn(&mut RemoteStoreConfig)) -> T {
        fake.reader_mkdir(FOLDER);
        for (rel, bytes) in files {
            fake.reader_put(&format!("{FOLDER}/{rel}"), bytes);
        }
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cloud/s/library");
        let src = Arc::new(Src::default());
        src.set(|s| s.rows = rows());
        let mut t = T {
            tmp,
            root,
            fake: Arc::new(fake),
            store: None,
            mount: None,
            src,
            tune,
        };
        t.start();
        t
    }

    fn cfg(&self) -> RemoteStoreConfig {
        let mut cfg = RemoteStoreConfig::new(&self.root, self.tmp.path().join("cloud/s"), FOLDER);
        cfg.poll_active = Duration::from_secs(3600);
        cfg.poll_idle = Duration::from_secs(3600);
        cfg.lease = false;
        cfg.retry_base = Duration::from_millis(20);
        cfg.mount_id = 3;
        cfg.queue_floor = Some(Duration::from_millis(200));
        cfg.series_queue_timing = SeriesQueueTiming {
            hold_debounce: Duration::from_millis(300),
            unhold_debounce: Duration::from_millis(600),
            early_gap: Duration::from_secs(2),
            refit_every: Duration::from_secs(3600),
        };
        (self.tune)(&mut cfg);
        cfg
    }

    fn start(&mut self) {
        let store = RemoteStore::open_shared(self.cfg(), self.fake.clone()).unwrap();
        self.mount = Some(bunko_store::mount(store.clone() as Arc<dyn LibraryStore>));
        self.store = Some(store);
        assert!(self.s().wait_steady(WAIT));
    }

    /// The queue's source goes in (as the server installs it).
    fn connect(&self) {
        self.s().set_series_queue_source(self.src.clone());
    }

    fn stop(&mut self) {
        self.mount = None;
        self.store = None;
    }

    fn s(&self) -> &Arc<Store> {
        self.store.as_ref().unwrap()
    }

    fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for s in rel.split('/') {
            p.push(s);
        }
        p
    }

    fn idle(&self) {
        assert!(self.s().wait_outbox_idle(WAIT), "{:?}", self.s().outbox());
    }

    /// Series `folder`'s file in the store.
    fn file(&self, folder: &str) -> Option<Value> {
        let n = self.fake.node_at(&format!("{FOLDER}/{folder}/{FILE}"))?;
        serde_json::from_slice(&self.fake.content(&n.pid)?).ok()
    }

    /// How many series files were uploaded so far.
    fn uploads(&self) -> usize {
        self.fake
            .upload_log()
            .iter()
            .filter(|n| n.as_str() == FILE)
            .count()
    }

    fn marker(&self) -> Marker {
        let root = self.fake.node_at(FOLDER).unwrap().pid;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let nodes = rt
            .block_on(self.fake.find_named(&root, MARKER_NAME))
            .unwrap()
            .0;
        let n = nodes.into_iter().find(|n| !n.trashed).expect("a marker");
        Marker::parse(&self.fake.content(&n.pid).unwrap()).unwrap()
    }

    fn holder(&self) {
        wait_until("the lease", || {
            self.s().lease().map(|l| l.event()) == Some(LeaseEvent::Acquired)
        });
    }
}

fn lease_quick(c: &mut RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_millis(300));
}

/// No file staged (each staging takes a new revision) and none uploaded for `d`.
fn quiet(t: &T, d: Duration) {
    t.idle();
    let rev = t.s().series_queue().unwrap().rev();
    let n = t.uploads();
    std::thread::sleep(d);
    t.idle();
    assert_eq!(
        t.s().series_queue().unwrap().rev(),
        rev,
        "a file was staged again"
    );
    assert_eq!(
        t.uploads(),
        n,
        "a file was written again: {:?}",
        t.s().outbox()
    );
}

#[test]
fn every_owing_series_gets_its_file_and_time_alone_writes_nothing() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Alpha/Alpha 1.cbz", b"zip"),
            ("Alpha/Alpha 2.cbz", b"zip"),
            ("Beta/Beta 1.cbz", b"zip"),
            ("Quiet/Quiet 1.cbz", b"zip"),
        ],
        no_tune,
    );
    let now = wall();
    t.src.set(|s| {
        put_series(
            s,
            "Alpha",
            vec![
                ("Alpha 1", running(now - 60.0, now + 600.0)),
                ("Alpha 2", queued(Some(now + 1200.0))),
            ],
        );
        put_series(s, "Beta", vec![("Beta 1", queued(None))]);
    });
    assert_eq!(t.uploads(), 0, "nothing before the queue's source is in");
    t.connect();
    wait_until("both files", || {
        t.file("Alpha").is_some() && t.file("Beta").is_some()
    });
    t.idle();
    let a = t.file("Alpha").unwrap();
    assert_eq!(a["v"], 2);
    assert_eq!(a["instance_id"], t.s().instance_id());
    assert_eq!(a["series"], "Alpha");
    assert_eq!(
        a["rows"],
        json!([{"kind": "ocr", "id": "main", "parts": rows()[0].parts}])
    );
    let round = |x: f64| ((x / 60.0).round() as i64) * 60;
    assert_eq!(
        a["vols"]["Alpha 1"],
        json!([[0, "r", (now - 60.0).floor() as i64, round(now + 600.0)]])
    );
    let w = &a["vols"]["Alpha 2"][0];
    assert_eq!(w[1], "q");
    assert_eq!(
        w[2].as_i64().unwrap() % 60,
        0,
        "a work position rounded to 60: {w}"
    );
    assert_eq!(a["done"], json!({}));
    assert!(a["rev"].as_u64().unwrap() > 0);
    let b = t.file("Beta").unwrap();
    assert_eq!(b["vols"], json!({"Beta 1": [[0, "q", null]]}));
    assert!(
        t.file("Quiet").is_none(),
        "a series that owes nothing has no file"
    );
    assert_eq!(t.uploads(), 2);
    // Time passing, with the plan as it was, is never a reason to write (finding 1).
    quiet(&t, Duration::from_millis(1500));
    // A job added is.
    t.src.set(|s| {
        put_series(
            s,
            "Beta",
            vec![("Beta 1", queued(None)), ("Beta 2", queued(None))],
        );
    });
    wait_until("Beta's new job", || {
        t.file("Beta")
            .is_some_and(|b| b["vols"].get("Beta 2").is_some())
    });
    assert_eq!(t.uploads(), 3, "only Beta's file was written again");
}

#[test]
fn a_finished_job_stays_running_until_its_series_settles() {
    let mut t = T::new(
        FakeBackend::drive_like(),
        &[("Alpha/Alpha 1.cbz", b"zip"), ("Alpha/Alpha 2.cbz", b"zip")],
        |c| c.retry_base = Duration::from_secs(3),
    );
    let now = wall();
    t.src.set(|s| {
        put_series(
            s,
            "Alpha",
            vec![
                ("Alpha 1", running(now - 60.0, now + 600.0)),
                ("Alpha 2", queued(None)),
            ],
        )
    });
    t.connect();
    wait_until("Alpha's file", || t.file("Alpha").is_some());
    t.idle();
    let first = t.file("Alpha").unwrap();
    // The sidecar's write is on its way (its first try failed: it waits seconds for the
    // next), and the series' metadata has a recompile pending.
    t.fake.fail_next(
        FakeOp::Upload,
        BErr::Transient("a dropped connection".into()),
    );
    let local = t.tmp.path().join("Alpha 1.mokuro");
    std::fs::write(&local, b"{}").unwrap();
    fs::publish(
        &local,
        t.abs("Alpha/Alpha 1.mokuro"),
        &WriteIntent::new(
            Mode::CreateNew,
            Origin::Ocr { claim: "c1".into() },
            Durability::Deferred,
        ),
    )
    .unwrap();
    wait_until("the sidecar's first try", || {
        t.s().outbox().iter().any(|o| o.last_error.is_some())
    });
    t.src.regen.lock().insert("Alpha".into());
    t.src
        .set(|s| put_series(s, "Alpha", vec![("Alpha 2", queued(None))]));
    // Not settled: the finished job still shows running, so nothing is written.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(t.file("Alpha").unwrap()["vols"], first["vols"]);
    assert!(
        t.s().outbox().iter().any(|o| o.dest.ends_with(".mokuro")),
        "the sidecar is still on its way: {:?}",
        t.s().outbox()
    );
    // The sidecar lands; the recompile is still pending.
    wait_until("the sidecar", || {
        t.fake
            .node_at(&format!("{FOLDER}/Alpha/Alpha 1.mokuro"))
            .is_some()
    });
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(t.file("Alpha").unwrap()["vols"], first["vols"]);
    // Settled: the job leaves, with a done entry.
    t.src.regen.lock().clear();
    wait_until("Alpha 1 done", || {
        t.file("Alpha")
            .is_some_and(|a| a["vols"].get("Alpha 1").is_none())
    });
    let a = t.file("Alpha").unwrap();
    assert!(a["done"]["Alpha 1"].as_i64().unwrap() >= now as i64 - 1);
    assert_eq!(a["vols"], json!({"Alpha 2": [[0, "q", null]]}));
    // Its last job leaving: the file keeps only the done tail.
    t.src.set(|s| put_series(s, "Alpha", vec![]));
    wait_until("Alpha owes nothing", || {
        t.file("Alpha")
            .is_some_and(|a| a["vols"] == json!({}) && a["done"].get("Alpha 2").is_some())
    });
    let n = t.uploads();
    // A restart writes nothing, and the revision keeps growing.
    t.idle();
    let rev = t.file("Alpha").unwrap()["rev"].as_u64().unwrap();
    t.stop();
    t.start();
    t.connect();
    quiet(&t, Duration::from_millis(1500));
    assert_eq!(t.uploads(), n);
    t.src
        .set(|s| put_series(s, "Alpha", vec![("Alpha 3", queued(None))]));
    wait_until("Alpha 3", || {
        t.file("Alpha")
            .is_some_and(|a| a["vols"].get("Alpha 3").is_some())
    });
    let a = t.file("Alpha").unwrap();
    assert!(a["rev"].as_u64().unwrap() > rev, "{a}");
    assert!(
        a["done"].get("Alpha 1").is_some() && a["done"].get("Alpha 2").is_some(),
        "the done tail survived the restart: {a}"
    );
}

#[test]
fn a_volume_finished_before_any_file_showed_it_is_named_done_once_settled() {
    // Regression (rig S4/S6): a volume OCR'd within one write window never reached a
    // series file, so no reader ever learnt it finished.
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Beta/Beta 1.cbz", b"zip")],
        |c| c.retry_base = Duration::from_secs(3),
    );
    t.connect();
    quiet(&t, Duration::from_millis(600));
    assert!(t.file("Beta").is_none());
    // Its sidecar is on its way (the first try failed), and the scheduler already
    // counts the volume finished: the series is not settled.
    t.fake.fail_next(
        FakeOp::Upload,
        BErr::Transient("a dropped connection".into()),
    );
    let local = t.tmp.path().join("Beta 1.mokuro");
    std::fs::write(&local, b"{}").unwrap();
    fs::publish(
        &local,
        t.abs("Beta/Beta 1.mokuro"),
        &WriteIntent::new(
            Mode::CreateNew,
            Origin::Ocr { claim: "c1".into() },
            Durability::Deferred,
        ),
    )
    .unwrap();
    wait_until("the sidecar's first try", || {
        t.s().outbox().iter().any(|o| o.last_error.is_some())
    });
    let at = wall() - 1.0;
    t.src.set(|s| {
        s.finished.insert(
            "beta".into(),
            ("Beta".into(), BTreeMap::from([("Beta 1".to_string(), at)])),
        );
    });
    std::thread::sleep(Duration::from_millis(800));
    assert!(t.file("Beta").is_none(), "nothing before the sidecar lands");
    wait_until("the sidecar", || {
        t.fake
            .node_at(&format!("{FOLDER}/Beta/Beta 1.mokuro"))
            .is_some()
    });
    wait_until("Beta's file", || t.file("Beta").is_some());
    let b = t.file("Beta").unwrap();
    assert_eq!(b["vols"], json!({}), "{b}");
    assert_eq!(b["rows"], json!([]), "{b}");
    assert!(b["done"]["Beta 1"].as_i64().unwrap() >= at as i64, "{b}");
    // Written once.
    let n = t.uploads();
    assert_eq!(n, 1);
    quiet(&t, Duration::from_millis(1500));
}

#[test]
fn foreign_files_are_rewritten_and_leftovers_deleted() {
    let foreign = br#"{"v":2,"instance_id":"another-bunko","rev":1,"vols":{}}"#;
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            (".mokuro-queue.json", br#"{"version":1}"#),
            ("Alpha/Alpha 1.cbz", b"zip"),
            ("Alpha/.mokuro-series-queue.json", foreign),
            ("Beta/Beta 1.cbz", b"zip"),
            ("Beta/.MOKURO-Series-Queue.json", foreign),
            ("Gamma/Gamma 1.cbz", b"zip"),
            ("Gamma/.mokuro-series-queue.json", foreign),
        ],
        lease_quick,
    );
    t.holder();
    t.src.set(|s| {
        put_series(s, "Alpha", vec![("Alpha 1", queued(None))]);
        put_series(s, "Beta", vec![("Beta 1", queued(None))]);
    });
    t.connect();
    let ours = |f: Option<Value>| f.is_some_and(|f| f["instance_id"] == t.s().instance_id());
    wait_until("every leftover gone and every file ours", || {
        t.fake
            .node_at(&format!("{FOLDER}/.mokuro-queue.json"))
            .is_none()
            && t.fake.node_at(&format!("{FOLDER}/Gamma/{FILE}")).is_none()
            && t.fake
                .node_at(&format!("{FOLDER}/Beta/.MOKURO-Series-Queue.json"))
                .is_none()
            && ours(t.file("Alpha"))
            && ours(t.file("Beta"))
    });
    // Other Bunkos' files went to the trash, not away for good.
    assert!(
        t.fake.trashed_log().len() >= 3,
        "{:?}",
        t.fake.trashed_log()
    );
    t.idle();
    // A user deleting a file: it is written again.
    let n = t.uploads();
    t.fake.reader_delete(&format!("{FOLDER}/Alpha/{FILE}"));
    t.s().feed().poll_now();
    wait_until("Alpha's file again", || ours(t.file("Alpha")));
    assert_eq!(t.uploads(), n + 1);
}

#[test]
fn the_marker_carries_the_queue_block_and_no_root_queue_file() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Alpha/Alpha 1.cbz", b"zip")],
        lease_quick,
    );
    t.src.set(|s| {
        put_series(s, "Alpha", vec![("Alpha 1", queued(None))]);
        s.held_rows.insert("lines".into());
    });
    t.connect();
    t.holder();
    wait_until("the block in the marker", || {
        t.marker().series_queue.is_some()
    });
    let m = t.marker();
    assert_eq!(m.queue, None);
    let b = m.series_queue.unwrap();
    assert_eq!(b["v"], 2);
    assert_eq!(b["file"], FILE);
    assert_eq!(b["held"], Value::Null);
    assert_eq!(b["held_rows"], json!(["lines"]));
    assert_eq!(b["clock"].as_array().unwrap().len(), 2);
    assert!(b["clock_at"].as_i64().is_some());
    let raw: Value = {
        let root = t.fake.node_at(FOLDER).unwrap().pid;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let n = rt
            .block_on(t.fake.find_named(&root, MARKER_NAME))
            .unwrap()
            .0;
        serde_json::from_slice(&t.fake.content(&n[0].pid).unwrap()).unwrap()
    };
    assert_eq!(raw["queue"], Value::Null, "{raw}");
    wait_until("Alpha's file", || t.file("Alpha").is_some());
    assert!(
        t.fake
            .node_at(&format!("{FOLDER}/.mokuro-queue.json"))
            .is_none()
    );
}

#[test]
fn status_files_off_writes_no_series_file_and_no_block() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Alpha/Alpha 1.cbz", b"zip")],
        |c| {
            lease_quick(c);
            c.status_files = false;
        },
    );
    t.src
        .set(|s| put_series(s, "Alpha", vec![("Alpha 1", queued(None))]));
    t.connect();
    t.holder();
    assert!(t.s().series_queue().is_none());
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(t.uploads(), 0);
    assert_eq!(t.marker().series_queue, None);
}

#[test]
fn files_are_written_on_the_store_clock() {
    // The fake provider's clock is far behind this machine's.
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Alpha/Alpha 1.cbz", b"zip"), ("Alpha/Alpha 2.cbz", b"zip")],
        lease_quick,
    );
    t.holder();
    let now = wall();
    let server = t.fake.now_ms() as f64 / 1000.0;
    assert!(now - server > 3600.0, "the fake's clock is not skewed");
    t.src.set(|s| {
        put_series(
            s,
            "Alpha",
            vec![
                ("Alpha 1", running(now - 120.0, now + 600.0)),
                ("Alpha 2", queued(None)),
            ],
        )
    });
    t.connect();
    wait_until("Alpha's file", || t.file("Alpha").is_some());
    let a = t.file("Alpha").unwrap();
    let off = now - server;
    let at = a["at"].as_f64().unwrap();
    assert!(
        (at - (now - off)).abs() < 60.0,
        "at {at} is on the store's clock"
    );
    let job = &a["vols"]["Alpha 1"][0];
    assert!(
        (job[2].as_f64().unwrap() - (now - 120.0 - off)).abs() < 60.0,
        "{job}"
    );
    assert!(
        (job[3].as_f64().unwrap() - (now + 600.0 - off)).abs() < 120.0,
        "{job}"
    );
    let clock_at = t.marker().series_queue.unwrap()["clock_at"]
        .as_f64()
        .unwrap();
    assert!(
        (clock_at - (now - off)).abs() < 120.0,
        "the clock too: {clock_at}"
    );
}

#[test]
fn a_hold_asks_for_an_early_heartbeat_at_most_once_per_gap() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Alpha/Alpha 1.cbz", b"zip")],
        |c| {
            lease_quick(c);
            // Regular heartbeats every 40 s: none during the test.
            c.lease_ttl = Some(Duration::from_secs(120));
        },
    );
    t.src
        .set(|s| put_series(s, "Alpha", vec![("Alpha 1", queued(None))]));
    t.connect();
    t.holder();
    wait_until("the block", || t.marker().series_queue.is_some());
    let held = |t: &T| t.marker().series_queue.unwrap()["held"].clone();
    assert_eq!(held(&t), Value::Null);
    t.src.set(|s| s.held = Some("no-processor".into()));
    wait_until("the hold in the marker", || {
        held(&t) == json!({"reason": "no-processor"})
    });
    let first = Instant::now();
    // An unhold right after: the next early heartbeat waits for the gap (2 s).
    t.src.set(|s| s.held = None);
    wait_until("the unhold in the marker", || held(&t) == Value::Null);
    assert!(
        first.elapsed() >= Duration::from_millis(1500),
        "a second early heartbeat {:?} after the first",
        first.elapsed()
    );
}

#[test]
fn a_window_stages_at_most_its_cap() {
    let files: Vec<(String, &[u8])> = (0..20)
        .map(|i| (format!("S{i:02}/S{i:02} 1.cbz"), &b"zip"[..]))
        .collect();
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), *b)).collect();
    // OneDrive: 15 per window; a window of 5 s.
    let t = T::new(FakeBackend::onedrive_like(), &refs, |c| {
        c.queue_floor = Some(Duration::from_secs(5))
    });
    let sink = t.s().series_queue().unwrap().clone();
    assert_eq!(sink.cap(), 15);
    t.src.set(|s| {
        for i in 0..20 {
            let folder = format!("S{i:02}");
            let stem = format!("S{i:02} 1");
            put_series(s, &folder, vec![(stem.as_str(), queued(None))]);
        }
    });
    // Series with a file in the store (an upload the outbox retried counts once).
    let written = |t: &T| {
        (0..20)
            .filter(|i| t.file(&format!("S{i:02}")).is_some())
            .count()
    };
    let start = Instant::now();
    t.connect();
    wait_until("the first burst", || written(&t) >= 15);
    t.idle();
    if start.elapsed() < Duration::from_secs(4) {
        assert_eq!(written(&t), 15, "one window, one burst of B");
        assert_eq!(sink.backlog_len(), 5, "the rest waits");
    }
    wait_until("the second burst", || written(&t) == 20);
    assert!(
        start.elapsed() >= Duration::from_secs(5),
        "the rest waited for the next window"
    );
}

/// A Status op of a series' queue file, published directly (as the sink stages it).
fn status_put(t: &T, rel: &str, bytes: &[u8]) {
    let local = t.tmp.path().join(rel.replace('/', "_"));
    std::fs::write(&local, bytes).unwrap();
    fs::publish(
        &local,
        t.abs(rel),
        &WriteIntent::new(
            Mode::Replace { base: None },
            Origin::Status,
            Durability::Deferred,
        ),
    )
    .unwrap();
}

fn put(t: &T, rel: &str, bytes: &[u8], origin: Origin, mode: Mode) {
    let local = t.tmp.path().join(rel.replace('/', "_"));
    std::fs::write(&local, bytes).unwrap();
    fs::publish(
        &local,
        t.abs(rel),
        &WriteIntent::new(mode, origin, Durability::Deferred),
    )
    .unwrap();
}

#[test]
fn a_queue_file_waits_only_for_earlier_ocr_and_compiler_writes_of_its_series() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("S/S 1.cbz", b"zip"), ("T/T 1.cbz", b"zip")],
        |c| {
            c.status_files = false;
            c.retry_base = Duration::from_secs(2);
        },
    );
    // The sidecar's first try fails: it waits seconds for the next.
    t.fake.fail_next(
        FakeOp::Upload,
        BErr::Transient("a dropped connection".into()),
    );
    put(
        &t,
        "S/S 1.mokuro",
        b"{}",
        Origin::Ocr { claim: "c".into() },
        Mode::CreateNew,
    );
    wait_until("the sidecar's first try", || {
        t.s().outbox().iter().any(|o| o.last_error.is_some())
    });
    status_put(&t, &format!("S/{FILE}"), b"{\"v\":2}");
    status_put(&t, &format!("T/{FILE}"), b"{\"v\":2}");
    t.idle();
    let log = t.fake.upload_log();
    let at = |name: &str, nth: usize| {
        log.iter()
            .enumerate()
            .filter(|(_, n)| n.as_str() == name)
            .nth(nth)
            .map(|(i, _)| i)
            .unwrap_or_else(|| panic!("no {name} #{nth}: {log:?}"))
    };
    // T's file did not wait for S's sidecar; S's did.
    assert!(at(FILE, 0) < at("S 1.mokuro", 0), "{log:?}");
    assert!(at(FILE, 1) > at("S 1.mokuro", 0), "{log:?}");
    assert!(t.fake.node_at(&format!("{FOLDER}/T/{FILE}")).is_some());
    // Every call of theirs ran in the background class; with no lease, none in the
    // lease's.
    let stats = t.s().api_stats();
    assert_eq!(stats.last_hour(ApiClass::Lease).calls, 0);
    assert!(stats.last_hour(ApiClass::Background).calls > 0);
}

#[test]
fn series_json_and_the_queue_file_never_wait_for_each_other() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("S/S 1.cbz", b"zip"), ("U/U 1.cbz", b"zip")],
        |c| c.status_files = false,
    );
    let gate = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let g = gate.clone();
        t.s().set_lease_gate(Arc::new(move || {
            g.load(std::sync::atomic::Ordering::Acquire)
        }));
    }
    // series.json, then the queue file; then (in another series) the other way round.
    put(
        &t,
        "S/series.json",
        b"{}",
        Origin::Compiler,
        Mode::Replace { base: None },
    );
    status_put(&t, &format!("S/{FILE}"), b"{}");
    gate.store(true, std::sync::atomic::Ordering::Release);
    t.idle();
    gate.store(false, std::sync::atomic::Ordering::Release);
    status_put(&t, &format!("U/{FILE}"), b"{}");
    put(
        &t,
        "U/series.json",
        b"{}",
        Origin::Compiler,
        Mode::Replace { base: None },
    );
    gate.store(true, std::sync::atomic::Ordering::Release);
    t.idle();
    for p in [
        "S/series.json",
        "S/.mokuro-series-queue.json",
        "U/series.json",
        "U/.mokuro-series-queue.json",
    ] {
        assert!(t.fake.node_at(&format!("{FOLDER}/{p}")).is_some(), "{p}");
    }
}

/// On every provider shape: a file rewritten in place (the Status fast path, or MEGA's
/// new node per write) is still this sink's own once the feed has listed it again, so
/// a quiet queue never writes again (a version the feed reports differently from the
/// write's answer would read as foreign and be rewritten forever).
#[test]
fn a_rewritten_file_stays_own_through_feed_polls_on_every_provider() {
    for (name, fake) in [
        ("webdav_cas", FakeBackend::webdav_cas()),
        ("webdav_plain", FakeBackend::webdav_plain()),
        ("onedrive_like", FakeBackend::onedrive_like()),
        ("mega_like", FakeBackend::mega_like()),
        ("drive_like", FakeBackend::drive_like()),
    ] {
        let t = T::new(fake, &[("Gamma/Gamma 1.cbz", b"zip")], no_tune);
        t.src
            .set(|s| put_series(s, "Gamma", vec![("Gamma 1", queued(None))]));
        t.connect();
        wait_until("Gamma's file", || t.file("Gamma").is_some());
        t.idle();
        t.src.set(|s| {
            put_series(
                s,
                "Gamma",
                vec![("Gamma 1", queued(None)), ("Gamma 2", queued(None))],
            )
        });
        wait_until("Gamma 2", || {
            t.file("Gamma")
                .is_some_and(|g| g["vols"].get("Gamma 2").is_some())
        });
        t.idle();
        let n = t.uploads();
        for _ in 0..3 {
            t.s().feed().poll_now();
            std::thread::sleep(Duration::from_millis(400));
        }
        quiet(&t, Duration::from_millis(1200));
        assert_eq!(t.uploads(), n, "{name}: written again");
        assert_eq!(n, 2, "{name}: one write per change");
    }
}
