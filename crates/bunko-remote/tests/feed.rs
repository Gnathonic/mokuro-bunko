//! The feed engine (plan M2 task 3, spec §5.3, §5.4, D19, D20): FakeBackend, paused
//! tokio time.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bunko_remote::backend::{BErr, Backend, OpCtx, UploadSource, UploadTarget};
use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::feed::{
    CloudHooks, EngineState, Feed, FeedConfig, FeedHandle, FeedState, SubmissionResult,
};
use bunko_remote::index::{Index, RelPath};
use bunko_store::WatchEvent;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio::time::Instant;

const ROOT: &str = "/cloud/test/library";
const LONG: Duration = Duration::from_secs(4 * 3600);
const GRACE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
enum H {
    Moved(PathBuf, PathBuf, bool),
    Removed(PathBuf, bool),
    Sidecar(PathBuf),
}

struct Rec {
    tx: mpsc::UnboundedSender<(Instant, H)>,
    no_facts: Mutex<HashSet<PathBuf>>,
}

impl CloudHooks for Rec {
    fn moved(&self, from: &Path, to: &Path, is_dir: bool) {
        let _ = self.tx.send((
            Instant::now(),
            H::Moved(from.to_path_buf(), to.to_path_buf(), is_dir),
        ));
    }
    fn removed(&self, path: &Path, is_dir: bool) {
        let _ = self
            .tx
            .send((Instant::now(), H::Removed(path.to_path_buf(), is_dir)));
    }
    fn sidecar_overwritten(&self, path: &Path) {
        let _ = self
            .tx
            .send((Instant::now(), H::Sidecar(path.to_path_buf())));
    }
    fn series_submission(&self, _series_dir: &Path, _bytes: &[u8]) -> SubmissionResult {
        SubmissionResult::Applied
    }
    fn ocr_discarded(&self, _path: &Path) {}
    fn has_series_facts(&self, series_dir: &Path) -> bool {
        !self.no_facts.lock().contains(series_dir)
    }
}

/// Names whose upserts are "ours" (a stand-in for the ledger).
#[derive(Default)]
struct Echoes {
    names: Mutex<HashSet<String>>,
}

impl FeedState for Echoes {
    fn is_own_echo(&self, _pid: &str, _parent: Option<&str>, name: &str, _cver: &str) -> bool {
        self.names.lock().contains(name)
    }
}

fn abs(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(ROOT);
    for s in rel.split('/').filter(|s| !s.is_empty()) {
        p.push(s);
    }
    p
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

fn added(r: &str, is_dir: bool) -> WatchEvent {
    WatchEvent::Added {
        path: abs(r),
        is_dir,
    }
}

fn removed(r: &str, is_dir: bool) -> WatchEvent {
    WatchEvent::Removed {
        path: abs(r),
        is_dir,
    }
}

fn config() -> FeedConfig {
    FeedConfig {
        poll_active: Duration::from_secs(30),
        poll_idle: Duration::from_secs(60),
        quiet: GRACE,
        // The D19 tests below use small libraries; the shipped minimum (200) is
        // covered by `deleting_a_series_from_a_small_library_does_not_trip_d19`.
        suspect_min_removals: 10,
        ..FeedConfig::new(ROOT, "mokuro-reader")
    }
}

struct Rig {
    dir: tempfile::TempDir,
    fake: Arc<FakeBackend>,
    index: Arc<Mutex<Index>>,
    events: mpsc::UnboundedReceiver<WatchEvent>,
    hooks: mpsc::UnboundedReceiver<(Instant, H)>,
    rec: Arc<Rec>,
    echoes: Arc<Echoes>,
    json: Arc<Mutex<Vec<(String, String, String)>>>,
    feed: FeedHandle,
}

fn rig(fake: FakeBackend) -> Rig {
    rig_with(Arc::new(fake), config(), None)
}

fn rig_with(fake: Arc<FakeBackend>, cfg: FeedConfig, dir: Option<tempfile::TempDir>) -> Rig {
    let dir = dir.unwrap_or_else(|| tempfile::tempdir().unwrap());
    let index = Arc::new(Mutex::new(
        Index::open(&dir.path().join("index.sqlite")).unwrap(),
    ));
    let (etx, events) = mpsc::unbounded_channel();
    let (htx, hooks) = mpsc::unbounded_channel();
    let rec = Arc::new(Rec {
        tx: htx,
        no_facts: Mutex::new(HashSet::new()),
    });
    let echoes = Arc::new(Echoes::default());
    let json = Arc::new(Mutex::new(Vec::new()));
    let j = json.clone();
    let feed = Feed::new(fake.clone(), index.clone(), cfg)
        .with_hooks(rec.clone())
        .with_state(echoes.clone())
        .on_foreign_json(Arc::new(move |r: &RelPath, pid: &str, cver: &str| {
            j.lock()
                .push((r.to_string(), pid.to_string(), cver.to_string()));
        }));
    feed.add_sink(Arc::new(move |e| {
        let _ = etx.send(e);
    }));
    let feed = feed.spawn(&tokio::runtime::Handle::current());
    Rig {
        dir,
        fake,
        index,
        events,
        hooks,
        rec,
        echoes,
        json,
        feed,
    }
}

impl Rig {
    async fn steady(&self) {
        self.feed
            .wait_steady(LONG)
            .await
            .unwrap_or_else(|s| panic!("never steady: {s:?}"));
    }

    async fn next(&mut self) -> WatchEvent {
        match tokio::time::timeout(LONG, self.events.recv()).await {
            Ok(Some(e)) => e,
            Ok(None) => panic!("the feed is gone"),
            Err(_) => panic!("no event; state {:?}", self.feed.state()),
        }
    }

    /// Events up to and including the first that `pred` accepts.
    async fn until(&mut self, mut pred: impl FnMut(&WatchEvent) -> bool) -> Vec<WatchEvent> {
        let mut out = Vec::new();
        loop {
            let e = self.next().await;
            let stop = pred(&e);
            out.push(e);
            if stop {
                return out;
            }
        }
    }

    fn drain(&mut self) -> Vec<WatchEvent> {
        let mut out = Vec::new();
        while let Ok(e) = self.events.try_recv() {
            out.push(e);
        }
        out
    }

    async fn hook(&mut self) -> (Instant, H) {
        tokio::time::timeout(LONG, self.hooks.recv())
            .await
            .expect("a hook")
            .expect("the feed is alive")
    }

    fn hooks_now(&mut self) -> Vec<H> {
        let mut out = Vec::new();
        while let Ok((_, h)) = self.hooks.try_recv() {
            out.push(h);
        }
        out
    }

    fn has(&self, r: &str) -> bool {
        self.index.lock().lookup(&rel(r)).is_some()
    }

    /// Start, finish the first sync and swallow its events.
    async fn ready(&mut self) {
        self.steady().await;
        self.until(|e| *e == WatchEvent::Rescan).await;
        self.drain();
    }
}

fn put(f: &FakeBackend, path: &str) {
    f.reader_put(&format!("mokuro-reader/{path}"), path.as_bytes());
}

fn seed(f: &FakeBackend, paths: &[&str]) {
    f.reader_mkdir("mokuro-reader");
    for p in paths {
        put(f, p);
    }
}

/// `series` folders of `vols` volumes each: `S{i}/v{j}.cbz`.
fn seed_series(f: &FakeBackend, series: usize, vols: usize) {
    f.reader_mkdir("mokuro-reader");
    for i in 0..series {
        for j in 0..vols {
            put(f, &format!("S{i}/v{j}.cbz"));
        }
    }
}

// ---- first sync and arrivals ----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn first_sync_emits_added_for_every_entry_then_rescan() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz", "A/v1.mokuro", "B/v2.cbz"]);
    let mut r = rig(f);
    r.steady().await;
    let evs = r.until(|e| *e == WatchEvent::Rescan).await;
    assert_eq!(evs.last(), Some(&WatchEvent::Rescan));
    let got: BTreeSet<String> = evs[..evs.len() - 1]
        .iter()
        .map(|e| match e {
            WatchEvent::Added { path, is_dir } => format!("{}:{is_dir}", path.display()),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    let want: BTreeSet<String> = [
        ("A", true),
        ("A/v1.cbz", false),
        ("A/v1.mokuro", false),
        ("B", true),
        ("B/v2.cbz", false),
    ]
    .iter()
    .map(|(p, d)| format!("{}:{d}", abs(p).display()))
    .collect();
    assert_eq!(got, want);
    assert_eq!(evs.len(), 6, "each entry once: {evs:?}");
    assert!(r.hooks_now().is_empty());
}

#[tokio::test(start_paused = true)]
async fn reader_upload_is_added() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    put(&r.fake, "A/v2.cbz");
    assert_eq!(r.next().await, added("A/v2.cbz", false));

    // A new series folder with a volume: each shows once (the folder's listing adds
    // nothing new).
    put(&r.fake, "C/v1.cbz");
    let evs = r.until(|e| *e == added("C/v1.cbz", false)).await;
    assert_eq!(evs, vec![added("C", true), added("C/v1.cbz", false)]);
    r.feed.poll_now();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(r.drain().is_empty());
    assert!(r.hooks_now().is_empty());
}

#[tokio::test(start_paused = true)]
async fn folder_moved_in_from_outside_is_listed() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz"]);
    f.reader_put("elsewhere/S/v1.cbz", b"x");
    f.reader_put("elsewhere/S/v2.cbz", b"y");
    let mut r = rig(f);
    r.ready().await;
    r.fake.reader_rename("elsewhere/S", "mokuro-reader/S");
    let mut evs = vec![r.next().await];
    tokio::time::sleep(Duration::from_secs(1)).await;
    evs.extend(r.drain());
    evs.sort_by_key(|e| format!("{e:?}"));
    let mut want = vec![
        added("S", true),
        added("S/v1.cbz", false),
        added("S/v2.cbz", false),
    ];
    want.sort_by_key(|e| format!("{e:?}"));
    assert_eq!(evs, want);
    assert!(r.has("S/v2.cbz"));
}

// ---- moves and deletes (D20) ----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn reader_folder_rename_runs_moved_hook_immediately() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz", "B/v2.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    let t0 = Instant::now();
    r.fake.reader_rename("mokuro-reader/A", "mokuro-reader/A2");
    let evs = r.until(|e| matches!(e, WatchEvent::Added { .. })).await;
    assert_eq!(evs, vec![removed("A", true), added("A2", true)]);
    let (at, h) = r.hook().await;
    assert_eq!(h, H::Moved(abs("A"), abs("A2"), true));
    assert!(at - t0 <= Duration::from_secs(60), "within one poll");
    // Long past the grace: nothing else.
    tokio::time::sleep(Duration::from_secs(1200)).await;
    assert!(r.hooks_now().is_empty());
    assert!(r.has("A2/v1.cbz") && !r.has("A"));
}

#[tokio::test(start_paused = true)]
async fn reader_delete_runs_removed_hook_only_after_grace() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz", "A/v2.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    r.fake.reader_delete("mokuro-reader/A/v1.cbz");
    assert_eq!(r.next().await, removed("A/v1.cbz", false));
    let seen = Instant::now();
    assert!(!r.has("A/v1.cbz"));
    assert!(r.hooks_now().is_empty(), "no hook before the grace");
    assert!(r.feed.recently_removed(&abs("A/v1.cbz")).is_some());
    assert!(r.feed.recently_removed(&abs("A/v2.cbz")).is_none());

    let (at, h) = r.hook().await;
    assert_eq!(h, H::Removed(abs("A/v1.cbz"), false));
    let waited = at - seen;
    assert!(
        waited >= GRACE && waited <= GRACE + Duration::from_secs(5),
        "hook after {waited:?}"
    );
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert!(r.hooks_now().is_empty(), "once");
    assert!(r.feed.recently_removed(&abs("A/v1.cbz")).is_none());
}

#[tokio::test(start_paused = true)]
async fn delete_and_recreate_of_one_pid_within_grace_is_a_move() {
    // MEGA (a push feed): the delete and the create of one handle land in two batches.
    let f = FakeBackend::mega_like();
    seed(&f, &["A/v1.cbz", "B/x.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    let pid = r.fake.reader_detach("mokuro-reader/A/v1.cbz");
    assert_eq!(r.next().await, removed("A/v1.cbz", false));
    assert!(r.feed.recently_removed(&abs("A/v1.cbz")).is_some());
    tokio::time::sleep(Duration::from_secs(30)).await;
    r.fake.reader_attach(&pid, "mokuro-reader/B/v1.cbz");
    assert_eq!(r.next().await, added("B/v1.cbz", false));
    let (_, h) = r.hook().await;
    assert_eq!(h, H::Moved(abs("A/v1.cbz"), abs("B/v1.cbz"), false));
    tokio::time::sleep(Duration::from_secs(1200)).await;
    assert!(r.hooks_now().is_empty(), "no removed hook for a move");
}

#[tokio::test(start_paused = true)]
async fn cloud_delete_runs_hooks_after_grace() {
    let f = FakeBackend::drive_like();
    seed_series(&f, 6, 4);
    let mut r = rig(f);
    r.ready().await;
    r.fake.reader_delete("mokuro-reader/S1");
    let evs = r.until(|e| *e == removed("S1", true)).await;
    let seen = Instant::now();
    let mut want: Vec<WatchEvent> = (0..4)
        .map(|j| removed(&format!("S1/v{j}.cbz"), false))
        .collect();
    want.push(removed("S1", true));
    assert_eq!(evs, want, "children first");
    assert_eq!(r.feed.state(), EngineState::Steady);
    assert!(!r.has("S1") && r.has("S2/v0.cbz"));
    assert!(
        r.feed.recently_removed(&abs("S1/v3.cbz")).is_some(),
        "under a removed folder"
    );
    assert!(r.hooks_now().is_empty());

    let (at, h) = r.hook().await;
    assert_eq!(h, H::Removed(abs("S1"), true), "one hook for the folder");
    assert!(at - seen >= GRACE);
    tokio::time::sleep(Duration::from_secs(1200)).await;
    assert!(r.hooks_now().is_empty(), "no per-volume hooks");
}

#[tokio::test(start_paused = true)]
async fn webdav_plain_folder_move_is_one_moved_hook() {
    // Path ids: the move arrives as removals of the old pids and upserts of new ones.
    let f = FakeBackend::webdav_plain();
    seed(&f, &["A/v1.cbz", "A/v1.mokuro", "B/x.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    r.fake.reader_rename("mokuro-reader/A", "mokuro-reader/C");
    let mut evs = vec![r.next().await];
    tokio::time::sleep(Duration::from_secs(1)).await;
    evs.extend(r.drain());
    for e in [
        added("C", true),
        added("C/v1.cbz", false),
        added("C/v1.mokuro", false),
        removed("A", true),
        removed("A/v1.cbz", false),
        removed("A/v1.mokuro", false),
    ] {
        assert!(evs.contains(&e), "{e:?} missing from {evs:?}");
    }
    assert_eq!(r.hook().await.1, H::Moved(abs("A"), abs("C"), true));
    tokio::time::sleep(Duration::from_secs(1200)).await;
    assert!(
        r.hooks_now().is_empty(),
        "no removed hooks: {:?}",
        r.hooks_now()
    );
    assert!(r.has("C/v1.mokuro") && !r.has("A"));

    // A file rename too.
    r.fake
        .reader_rename("mokuro-reader/B/x.cbz", "mokuro-reader/B/y.cbz");
    assert_eq!(
        r.hook().await.1,
        H::Moved(abs("B/x.cbz"), abs("B/y.cbz"), false)
    );
}

#[tokio::test(start_paused = true)]
async fn sidecar_overwrite_and_foreign_json_are_reported() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz", "A/v1.mokuro", "A/v1.edit.mokuro"]);
    let mut r = rig(f);
    r.ready().await;
    r.fake
        .reader_put("mokuro-reader/A/v1.mokuro", b"reader edit");
    assert_eq!(r.next().await, added("A/v1.mokuro", false));
    assert_eq!(r.hook().await.1, H::Sidecar(abs("A/v1.mokuro")));
    r.fake
        .reader_put("mokuro-reader/A/v1.edit.mokuro", b"layer edit");
    assert_eq!(r.next().await, added("A/v1.edit.mokuro", false));
    assert_eq!(r.hook().await.1, H::Sidecar(abs("A/v1.edit.mokuro")));
    assert!(r.json.lock().is_empty());

    let n = r.fake.reader_put("mokuro-reader/A/series.json", b"{}");
    assert_eq!(r.next().await, added("A/series.json", false));
    assert_eq!(
        r.json.lock().clone(),
        vec![("A/series.json".to_string(), n.pid.clone(), n.cver.clone())]
    );
    assert!(r.hooks_now().is_empty(), "a json file is no sidecar");

    // Our own series.json moved with its folder into a series without facts.
    r.json.lock().clear();
    r.rec.no_facts.lock().insert(abs("A2"));
    r.fake.reader_rename("mokuro-reader/A", "mokuro-reader/A2");
    assert_eq!(r.hook().await.1, H::Moved(abs("A"), abs("A2"), true));
    assert_eq!(
        r.json.lock().clone(),
        vec![("A2/series.json".to_string(), n.pid, n.cver)]
    );
}

// ---- echoes, resync, restart ----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn own_echo_emits_no_event() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    r.echoes
        .names
        .lock()
        .extend(["v1.cbz".to_string(), "v9.cbz".to_string()]);

    // Bunko replaces v1.cbz: an exact ledger echo of a known node.
    let bytes = b"bunko output";
    let src = r.dir.path().join("src");
    std::fs::write(&src, bytes).unwrap();
    let pid = r.fake.node_at("mokuro-reader/A/v1.cbz").unwrap().pid;
    let mut op = OpCtx::default();
    let up = r
        .fake
        .upload(
            UploadTarget::Replace {
                pid: &pid,
                if_cver: None,
            },
            UploadSource::File {
                path: &src,
                size: bytes.len() as u64,
                sha256: Sha256::digest(bytes).into(),
            },
            &mut op,
        )
        .await
        .unwrap();
    // A ledger echo of a node the index never saw still shows.
    let a = r.fake.node_at("mokuro-reader/A").unwrap().pid;
    r.fake
        .upload(
            UploadTarget::New {
                parent: &a,
                name: "v9.cbz",
            },
            UploadSource::File {
                path: &src,
                size: bytes.len() as u64,
                sha256: Sha256::digest(bytes).into(),
            },
            &mut op,
        )
        .await
        .unwrap();
    put(&r.fake, "A/control.cbz");
    let evs = r.until(|e| *e == added("A/control.cbz", false)).await;
    assert_eq!(
        evs,
        vec![added("A/v9.cbz", false), added("A/control.cbz", false)]
    );
    let e = r.index.lock().lookup(&rel("A/v1.cbz")).unwrap();
    assert_eq!(
        e.cver.as_deref(),
        Some(up.cver.as_str()),
        "metadata still updated"
    );
    assert!(r.hooks_now().is_empty());
}

#[tokio::test(start_paused = true)]
async fn lose_cursor_resyncs_without_spurious_removed() {
    let f = FakeBackend::drive_like();
    seed_series(&f, 3, 3);
    let mut r = rig(f);
    r.ready().await;
    let syncs = r.fake.calls(FakeOp::StartCursor);
    r.fake.lose_cursor();
    put(&r.fake, "S0/new.cbz");
    r.feed.poll_now();
    let evs = r.until(|e| *e == WatchEvent::Rescan).await;
    assert_eq!(evs, vec![added("S0/new.cbz", false), WatchEvent::Rescan]);
    assert_eq!(
        r.fake.calls(FakeOp::StartCursor),
        syncs + 1,
        "one full resync"
    );
    r.steady().await;
    assert!(r.has("S2/v2.cbz"));
    tokio::time::sleep(Duration::from_secs(1200)).await;
    assert!(r.hooks_now().is_empty());
    assert!(r.drain().is_empty());
}

#[tokio::test(start_paused = true)]
async fn restart_resumes_from_the_persisted_cursor() {
    let f = Arc::new(FakeBackend::drive_like());
    seed(&f, &["A/v1.cbz"]);
    let r = rig_with(f.clone(), config(), None);
    r.steady().await;
    let lists = f.calls(FakeOp::ListAll);
    let Rig { dir, feed, .. } = r;
    feed.shutdown().await;
    put(&f, "A/v2.cbz");

    let mut r = rig_with(f.clone(), config(), Some(dir));
    r.steady().await;
    assert_eq!(r.next().await, WatchEvent::Rescan, "startup always rescans");
    assert_eq!(
        r.next().await,
        added("A/v2.cbz", false),
        "then the feed resumes"
    );
    assert_eq!(f.calls(FakeOp::ListAll), lists, "no full sync");
}

#[tokio::test(start_paused = true)]
async fn missing_root_waits_unless_create_if_missing() {
    let r = rig(FakeBackend::drive_like());
    tokio::time::sleep(Duration::from_secs(900)).await;
    assert_eq!(r.feed.state(), EngineState::LocateRoots);
    assert!(r.fake.node_at("mokuro-reader").is_none());
    assert_eq!(r.fake.calls(FakeOp::CreateRoot), 0);

    let cfg = FeedConfig {
        create_if_missing: true,
        ..config()
    };
    let r = rig_with(Arc::new(FakeBackend::drive_like()), cfg, None);
    r.steady().await;
    assert!(r.fake.node_at("mokuro-reader").is_some());
}

// ---- D19 ------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_quarter_vanishing_goes_suspect_until_confirmed() {
    let f = FakeBackend::drive_like();
    seed_series(&f, 4, 10); // 44 nodes
    let mut r = rig(f);
    r.ready().await;
    let gone: Vec<String> = (0..11)
        .map(|j| format!("S{}/v{}.cbz", j % 4, j / 4))
        .collect();
    for g in &gone {
        r.fake.reader_delete(&format!("mokuro-reader/{g}"));
    }
    let s = r
        .feed
        .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
        .await
        .unwrap();
    assert_eq!(
        s,
        EngineState::Suspect {
            pending_removals: 11,
            root_changed: false
        }
    );
    assert!(r.drain().is_empty(), "nothing removed");
    assert!(gone.iter().all(|g| r.has(g)));

    // Other changes still apply while suspect.
    put(&r.fake, "S0/new.cbz");
    assert_eq!(r.next().await, added("S0/new.cbz", false));
    assert!(gone.iter().all(|g| r.has(g)));

    r.feed.confirm_suspect();
    r.steady().await;
    let evs = r.drain();
    assert_eq!(evs.len(), 11, "{evs:?}");
    for g in &gone {
        assert!(evs.contains(&removed(g, false)));
        assert!(!r.has(g));
    }
    let mut hooks = Vec::new();
    for _ in 0..11 {
        hooks.push(r.hook().await.1);
    }
    for g in &gone {
        assert!(hooks.contains(&H::Removed(abs(g), false)));
    }
}

#[tokio::test(start_paused = true)]
async fn deleting_a_series_from_a_small_library_does_not_trip_d19() {
    let f = FakeBackend::drive_like();
    seed_series(&f, 4, 10); // 44 nodes
    let cfg = FeedConfig {
        suspect_min_removals: FeedConfig::new(ROOT, "mokuro-reader").suspect_min_removals,
        ..config()
    };
    let mut r = rig_with(Arc::new(f), cfg, None);
    r.ready().await;
    // A whole series (folder + 10 volumes, a quarter of the library) is deleted.
    for v in 0..10 {
        r.fake.reader_delete(&format!("mokuro-reader/S1/v{v}.cbz"));
    }
    r.fake.reader_delete("mokuro-reader/S1");
    for _ in 0..11 {
        let ev = r.next().await;
        assert!(matches!(ev, WatchEvent::Removed { .. }), "{ev:?}");
    }
    assert!(!matches!(r.feed.state(), EngineState::Suspect { .. }));
    assert!(!r.has("S1/v0.cbz"));
}

#[tokio::test(start_paused = true)]
async fn full_sync_suspect_clears_when_two_listings_agree() {
    let f = FakeBackend::drive_like();
    seed_series(&f, 4, 10);
    let mut r = rig(f);
    r.ready().await;
    r.fake.lose_cursor();
    let gone: Vec<String> = (0..12)
        .map(|j| format!("S{}/v{}.cbz", j % 4, j / 4))
        .collect();
    for g in &gone {
        r.fake.reader_delete(&format!("mokuro-reader/{g}"));
    }
    r.feed.poll_now();
    let s = r
        .feed
        .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
        .await
        .unwrap();
    assert_eq!(
        s,
        EngineState::Suspect {
            pending_removals: 12,
            root_changed: false
        }
    );
    assert!(gone.iter().all(|g| r.has(g)), "the sweep held");
    assert!(
        !r.drain()
            .iter()
            .any(|e| matches!(e, WatchEvent::Removed { .. }))
    );

    // The next re-list agrees with the one that tripped: the removals apply.
    let syncs = r.fake.calls(FakeOp::StartCursor);
    let t0 = Instant::now();
    r.steady().await;
    let took = Instant::now() - t0;
    assert!(
        took >= Duration::from_secs(600) && took < Duration::from_secs(1200),
        "{took:?}"
    );
    assert_eq!(r.fake.calls(FakeOp::StartCursor), syncs + 1, "one re-list");
    assert!(gone.iter().all(|g| !r.has(g)));
    let evs = r.drain();
    for g in &gone {
        assert!(evs.contains(&removed(g, false)), "{g}");
    }
}

#[tokio::test(start_paused = true)]
async fn steady_suspect_clears_after_two_agreeing_relists() {
    let f = FakeBackend::drive_like();
    seed_series(&f, 4, 10);
    let mut r = rig(f);
    r.ready().await;
    let gone: Vec<String> = (0..10)
        .map(|j| format!("S{}/v{}.cbz", j % 4, j / 4))
        .collect();
    for g in &gone {
        r.fake.reader_delete(&format!("mokuro-reader/{g}"));
    }
    r.feed
        .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
        .await
        .unwrap();
    let syncs = r.fake.calls(FakeOp::StartCursor);
    let t0 = Instant::now();
    r.steady().await;
    let took = Instant::now() - t0;
    assert!(took >= Duration::from_secs(1200), "two re-lists: {took:?}");
    assert_eq!(r.fake.calls(FakeOp::StartCursor), syncs + 2);
    assert!(gone.iter().all(|g| !r.has(g)));
    let evs = r.drain();
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, WatchEvent::Removed { .. }))
            .count(),
        10
    );
}

#[tokio::test(start_paused = true)]
async fn removed_folder_leftovers_do_not_trip_d19() {
    // Drive reports only the trashed folder; its children stay as unplaced rows
    // (for an untrash) until a full sync sweeps them, silently.
    let f = FakeBackend::drive_like();
    seed_series(&f, 8, 5); // 48 nodes
    let mut r = rig(f);
    r.ready().await;
    for s in ["S0", "S1", "S2"] {
        r.fake.reader_delete(&format!("mokuro-reader/{s}"));
        r.until(|e| *e == removed(s, true)).await;
        tokio::time::sleep(Duration::from_secs(700)).await; // past the D19 window
    }
    assert_eq!(r.feed.state(), EngineState::Steady);
    let syncs = r.fake.calls(FakeOp::StartCursor);
    let states = Arc::new(Mutex::new(Vec::new()));
    let mut rx = r.feed.subscribe();
    let st = states.clone();
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            st.lock().push(rx.borrow().clone());
        }
    });
    r.fake.lose_cursor();
    r.feed.poll_now();
    let evs = r.until(|e| *e == WatchEvent::Rescan).await;
    assert_eq!(evs, vec![WatchEvent::Rescan]);
    tokio::time::sleep(Duration::from_secs(1500)).await;
    assert_eq!(r.fake.calls(FakeOp::StartCursor), syncs + 1, "no re-lists");
    assert!(
        !states
            .lock()
            .iter()
            .any(|s| matches!(s, EngineState::Suspect { .. })),
        "{:?}",
        states.lock()
    );
    assert_eq!(r.index.lock().node_count(), 5 * 6);
}

#[tokio::test(start_paused = true)]
async fn duplicated_and_reordered_deliveries_are_idempotent() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz", "B/v2.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    r.fake.duplicate_deliveries(true);
    r.fake.reorder_changes(true);
    put(&r.fake, "C/v3.cbz");
    r.fake.reader_rename("mokuro-reader/A", "mokuro-reader/A2");
    r.fake.reader_delete("mokuro-reader/B/v2.cbz");
    r.feed.poll_now();
    let mut evs = vec![r.next().await];
    tokio::time::sleep(Duration::from_secs(1)).await;
    evs.extend(r.drain());
    evs.sort_by_key(|e| format!("{e:?}"));
    let mut want = vec![
        added("C", true),
        added("C/v3.cbz", false),
        removed("A", true),
        added("A2", true),
        removed("B/v2.cbz", false),
    ];
    want.sort_by_key(|e| format!("{e:?}"));
    assert_eq!(evs, want);
    assert!(r.has("A2/v1.cbz") && r.has("C/v3.cbz") && !r.has("B/v2.cbz"));
    assert_eq!(r.hook().await.1, H::Moved(abs("A"), abs("A2"), true));
    assert_eq!(r.hook().await.1, H::Removed(abs("B/v2.cbz"), false));
    tokio::time::sleep(Duration::from_secs(1200)).await;
    assert!(r.hooks_now().is_empty());
}

#[tokio::test(start_paused = true)]
async fn root_renamed_goes_suspect() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    r.fake.reader_rename("mokuro-reader", "old-library");
    let s = r
        .feed
        .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
        .await
        .unwrap();
    assert_eq!(
        s,
        EngineState::Suspect {
            pending_removals: 0,
            root_changed: true
        }
    );
    assert!(r.has("A/v1.cbz"), "nothing applied");
    assert!(r.drain().is_empty());

    // Confirmed: re-locate by name; there is none, so the library is empty.
    r.feed.confirm_suspect();
    r.feed
        .wait_for(|s| *s == EngineState::LocateRoots, LONG)
        .await
        .unwrap();
    assert!(!r.has("A"));
}

// ---- errors ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn webdav_root_rename_holds_every_removal() {
    // Path ids: the rename also removes every descendant pid; none of it may apply.
    let f = FakeBackend::webdav_plain();
    seed(&f, &["A/v1.cbz", "A/v2.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    r.fake.reader_rename("mokuro-reader", "old-library");
    let s = r
        .feed
        .wait_for(
            |s| {
                matches!(
                    s,
                    EngineState::Suspect {
                        root_changed: true,
                        ..
                    }
                )
            },
            LONG,
        )
        .await
        .unwrap();
    assert!(matches!(s, EngineState::Suspect { .. }));
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert!(r.drain().is_empty());
    assert!(r.hooks_now().is_empty());
    assert!(r.has("A/v2.cbz"));
}

#[tokio::test(start_paused = true)]
async fn throttle_honours_retry_after() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    let polls = r.fake.calls(FakeOp::Changes);
    let t0 = Instant::now();
    r.fake.throttle_for(Duration::from_secs(90));
    put(&r.fake, "A/v2.cbz");
    assert_eq!(r.next().await, added("A/v2.cbz", false));
    let took = Instant::now() - t0;
    assert!(took >= Duration::from_secs(90), "{took:?}");
    assert!(took <= Duration::from_secs(91), "{took:?}");
    assert_eq!(
        r.fake.calls(FakeOp::Changes) - polls,
        2,
        "one throttled poll, then one after Retry-After"
    );
    assert_eq!(r.feed.state(), EngineState::Steady);
}

#[tokio::test(start_paused = true)]
async fn auth_revoked_needs_reauth_until_resumed() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    r.fake.fail_next(FakeOp::Changes, BErr::AuthRevoked);
    r.feed
        .wait_for(|s| *s == EngineState::NeedsReauth, LONG)
        .await
        .unwrap();
    put(&r.fake, "A/v2.cbz");
    let calls: usize = all_calls(&r.fake);
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert_eq!(
        all_calls(&r.fake),
        calls,
        "no provider calls while unauthorized"
    );
    assert!(r.drain().is_empty());

    r.feed.resume();
    assert_eq!(r.next().await, added("A/v2.cbz", false));
    assert_eq!(r.feed.state(), EngineState::Steady);
}

fn all_calls(f: &FakeBackend) -> usize {
    [
        FakeOp::LocateRoots,
        FakeOp::StartCursor,
        FakeOp::ListAll,
        FakeOp::ListDir,
        FakeOp::Changes,
        FakeOp::WaitChange,
        FakeOp::Stat,
    ]
    .iter()
    .map(|op| f.calls(*op))
    .sum()
}

#[tokio::test(start_paused = true)]
async fn transient_failures_go_offline_then_recover() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz"]);
    let mut r = rig(f);
    r.ready().await;
    for _ in 0..4 {
        r.fake
            .fail_next(FakeOp::Changes, BErr::Transient("503".into()));
    }
    r.feed
        .wait_for(|s| *s == EngineState::Offline, LONG)
        .await
        .unwrap();
    put(&r.fake, "A/v2.cbz");
    assert_eq!(r.next().await, added("A/v2.cbz", false));
    assert_eq!(r.feed.state(), EngineState::Steady);
}

// ---- scale ----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn full_sync_10k_nodes_streams() {
    let f = Arc::new(FakeBackend::drive_like());
    seed_series(&f, 100, 100);
    let dir = tempfile::tempdir().unwrap();
    let index = Arc::new(Mutex::new(
        Index::open(&dir.path().join("index.sqlite")).unwrap(),
    ));
    let seen_events = Arc::new(AtomicUsize::new(0));
    // What the engine has committed and emitted while the provider is still listing.
    let probes: Arc<Mutex<Vec<(usize, u64, usize)>>> = Arc::default();
    {
        let (index, ev, probes) = (index.clone(), seen_events.clone(), probes.clone());
        f.set_list_hook(Some(Arc::new(move |k| {
            if k % 1000 == 0 {
                let n = index.lock().node_count();
                probes.lock().push((k, n, ev.load(Ordering::Relaxed)));
            }
        })));
    }
    let feed = Feed::new(f.clone(), index.clone(), config());
    let ev = seen_events.clone();
    feed.add_sink(Arc::new(move |e| {
        if matches!(e, WatchEvent::Added { .. }) {
            ev.fetch_add(1, Ordering::Relaxed);
        }
    }));
    let feed = feed.spawn(&tokio::runtime::Handle::current());
    feed.wait_steady(LONG).await.unwrap();
    assert_eq!(index.lock().node_count(), 100 * 100 + 100);
    assert_eq!(seen_events.load(Ordering::Relaxed), 100 * 100 + 100);
    let probes = probes.lock().clone();
    assert_eq!(probes.len(), 10, "{probes:?}");
    for (k, nodes, events) in probes {
        // Committed in chunks as the listing streams in, never gathered first.
        assert!(nodes as usize + 512 >= k, "at {k}: {nodes} nodes indexed");
        assert!(nodes as usize <= k, "at {k}: {nodes} nodes indexed");
        assert!(events + 512 >= k, "at {k}: {events} events");
    }
}

// ---- duplicate merges are not catastrophes (D19) ------------------------------------

/// Two top-level roots: `A` (canonical) holds `S/V1.cbz`, `catalog.json` and
/// `volume-data.json`; `B` holds `T/V2.cbz`, `S/V3.cbz`, an older `catalog.json` and
/// its own `volume-data.json`.
struct Twins {
    a: String,
    b: String,
    t: String,
    s_a: String,
    s_b: String,
    v3: String,
    cat_b: String,
    vd_b: String,
}

fn twins(f: &FakeBackend) -> Twins {
    let top = f.account_root();
    let a = f.reader_mkdir_in(&top, "mokuro-reader").pid;
    let s_a = f.reader_mkdir_in(&a, "S").pid;
    f.reader_put_in(&s_a, "V1.cbz", b"v1");
    let cat_a = f.reader_put_in(&a, "catalog.json", b"{}");
    f.reader_put_in(&a, "volume-data.json", b"{\"a\":1}");
    let b = f.reader_mkdir_in(&top, "mokuro-reader").pid;
    let t = f.reader_mkdir_in(&b, "T").pid;
    f.reader_put_in(&t, "V2.cbz", b"v2");
    let s_b = f.reader_mkdir_in(&b, "S").pid;
    let v3 = f.reader_put_in(&s_b, "V3.cbz", b"v3").pid;
    let cat_b = f.reader_put_in(&b, "catalog.json", b"{}");
    f.set_times(&cat_b.pid, cat_b.created_ms, cat_a.base_mtime_ms - 1000);
    let vd_b = f.reader_put_in(&b, "volume-data.json", b"{\"b\":1}").pid;
    Twins {
        a,
        b,
        t,
        s_a,
        s_b,
        v3,
        cat_b: cat_b.pid,
        vd_b,
    }
}

const SURVIVORS: [&str; 7] = [
    "T",
    "T/V2.cbz",
    "S",
    "S/V1.cbz",
    "S/V3.cbz",
    "catalog.json",
    "volume-data.json",
];

impl Rig {
    /// Let the feed take one more page (or several) and apply it.
    async fn settle(&self) {
        self.feed.poll_now();
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    /// Every state the engine publishes from now on.
    fn record_states(&self) -> Arc<Mutex<Vec<EngineState>>> {
        let states = Arc::new(Mutex::new(Vec::new()));
        let mut rx = self.feed.subscribe();
        let st = states.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                st.lock().push(rx.borrow().clone());
            }
        });
        states
    }
}

fn never_suspect(states: &Mutex<Vec<EngineState>>) {
    let s = states.lock();
    assert!(
        !s.iter().any(|s| matches!(s, EngineState::Suspect { .. })),
        "{s:?}"
    );
}

fn removed_any(evs: &[WatchEvent], paths: &[&str]) -> Vec<WatchEvent> {
    evs.iter()
        .filter(|e| {
            paths
                .iter()
                .any(|p| **e == removed(p, false) || **e == removed(p, true))
        })
        .cloned()
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_reader_merge_of_duplicate_roots_never_holds() {
    for make in [FakeBackend::drive_like, FakeBackend::mega_like] {
        for paged in [false, true] {
            let f = make();
            let w = twins(&f);
            let mut r = rig(f);
            r.ready().await;
            assert_eq!(r.index.lock().roots(), [w.a.clone(), w.b.clone()]);
            let states = r.record_states();
            // As the reader merges: every child of B moves into A (a folder or a
            // keep-both file beside its same-named twin), B's S is emptied into A's,
            // the older copy of a file is trashed, then B goes (MEGA: deleted).
            r.fake.reader_move(&w.t, &w.a);
            r.fake.reader_move(&w.s_b, &w.a);
            r.fake.reader_move(&w.vd_b, &w.a);
            if paged {
                r.settle().await;
            }
            r.fake.reader_move(&w.v3, &w.s_a);
            r.fake.reader_trash_pid(&w.cat_b);
            if paged {
                r.settle().await;
            }
            r.fake.reader_trash_pid(&w.s_b);
            r.fake.reader_trash_pid(&w.b);
            r.settle().await;
            let label = format!("{} paged={paged}", r.fake.account().await.unwrap().label);
            assert_eq!(r.feed.state(), EngineState::Steady, "{label}");
            never_suspect(&states);
            assert_eq!(
                r.index.lock().roots(),
                std::slice::from_ref(&w.a),
                "{label}"
            );
            for p in SURVIVORS {
                assert!(r.has(p), "{label}: {p}");
            }
            let evs = r.drain();
            assert_eq!(removed_any(&evs, &SURVIVORS), [], "{label}: {evs:?}");
            tokio::time::sleep(Duration::from_secs(1200)).await;
            assert_eq!(r.hooks_now(), [], "{label}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_root_removed_with_a_unique_child_still_holds() {
    for make in [FakeBackend::drive_like, FakeBackend::mega_like] {
        let f = make();
        let w = twins(&f);
        let mut r = rig(f);
        r.ready().await;
        // T only exists in B.
        r.fake.reader_trash_pid(&w.b);
        let s = r
            .feed
            .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
            .await
            .unwrap();
        assert!(
            matches!(
                s,
                EngineState::Suspect {
                    root_changed: true,
                    ..
                }
            ),
            "{s:?}"
        );
        assert!(r.has("T/V2.cbz"), "nothing applied");
        assert!(r.drain().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn the_last_root_removed_still_holds() {
    let f = FakeBackend::drive_like();
    seed(&f, &["A/v1.cbz"]);
    let root = f.node_at("mokuro-reader").unwrap().pid;
    let mut r = rig(f);
    r.ready().await;
    r.fake.reader_trash_pid(&root);
    let s = r
        .feed
        .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
        .await
        .unwrap();
    assert_eq!(
        s,
        EngineState::Suspect {
            pending_removals: 0,
            root_changed: true
        }
    );
    assert!(r.has("A/v1.cbz"));
}

#[tokio::test(start_paused = true)]
async fn a_merge_while_stopped_drops_the_vacated_root_at_restart() {
    for make in [FakeBackend::drive_like, FakeBackend::mega_like] {
        let f = Arc::new(make());
        let w = twins(&f);
        let r = rig_with(f.clone(), config(), None);
        r.steady().await;
        let Rig { dir, feed, .. } = r;
        feed.shutdown().await;
        assert_eq!(f.run_reader_dedup(), 2);

        let mut r = rig_with(f.clone(), config(), Some(dir));
        let states = r.record_states();
        r.steady().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        never_suspect(&states);
        assert_eq!(r.index.lock().roots(), std::slice::from_ref(&w.a));
        for p in SURVIVORS {
            assert!(r.has(p), "{p}");
        }
        // The moved pids are indexed under A.
        assert_eq!(r.index.lock().node(&w.t).unwrap().parent, Some(w.a.clone()));
        assert_eq!(
            r.index.lock().node(&w.v3).unwrap().parent,
            Some(w.s_a.clone())
        );
        let evs = r.drain();
        assert_eq!(removed_any(&evs, &SURVIVORS), [], "{evs:?}");
        tokio::time::sleep(Duration::from_secs(1200)).await;
        assert_eq!(r.hooks_now(), []);
    }
}

#[tokio::test(start_paused = true)]
async fn a_root_trashed_while_stopped_with_a_unique_child_holds_at_restart() {
    let f = Arc::new(FakeBackend::drive_like());
    let w = twins(&f);
    let r = rig_with(f.clone(), config(), None);
    r.steady().await;
    let Rig { dir, feed, .. } = r;
    feed.shutdown().await;
    // T is neither in A by pid nor by name.
    f.reader_trash_pid(&w.b);
    let r = rig_with(f.clone(), config(), Some(dir));
    let s = r
        .feed
        .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
        .await
        .unwrap();
    assert!(
        matches!(
            s,
            EngineState::Suspect {
                root_changed: true,
                ..
            }
        ),
        "{s:?}"
    );
    assert!(r.has("T/V2.cbz"));
}

#[tokio::test(start_paused = true)]
async fn merging_two_full_library_copies_never_trips_d19() {
    let f = FakeBackend::drive_like();
    let top = f.account_root();
    let mut copies = Vec::new();
    for _ in 0..2 {
        let root = f.reader_mkdir_in(&top, "mokuro-reader").pid;
        for i in 0..50 {
            let s = f.reader_mkdir_in(&root, &format!("S{i}")).pid;
            for j in 0..10 {
                f.reader_put_in(&s, &format!("v{j}.cbz"), b"zip");
            }
        }
        copies.push(root);
    }
    let cfg = FeedConfig {
        suspect_min_removals: FeedConfig::new(ROOT, "mokuro-reader").suspect_min_removals,
        ..config()
    };
    let mut r = rig_with(Arc::new(f), cfg, None);
    r.ready().await;
    let states = r.record_states();
    // 500 superseded copies trashed, 50 emptied folders and a root removed.
    assert_eq!(r.fake.run_reader_dedup(), 51);
    for _ in 0..4 {
        r.settle().await;
    }
    assert_eq!(r.feed.state(), EngineState::Steady);
    never_suspect(&states);
    assert_eq!(r.index.lock().roots(), [copies[0].clone()]);
    assert_eq!(r.index.lock().node_count(), 550);
    let evs = r.drain();
    assert!(
        !evs.iter().any(|e| matches!(e, WatchEvent::Removed { .. })),
        "{evs:?}"
    );

    // Control: 300 unique volumes trashed is still a catastrophe.
    for k in 0..300 {
        let n = r
            .fake
            .node_at(&format!("mokuro-reader/S{}/v{}.cbz", k / 10, k % 10))
            .unwrap();
        r.fake.reader_trash_pid(&n.pid);
    }
    let s = r
        .feed
        .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
        .await
        .unwrap();
    assert_eq!(
        s,
        EngineState::Suspect {
            pending_removals: 300,
            root_changed: false
        }
    );
}

#[tokio::test(start_paused = true)]
async fn a_hard_deleted_root_member_and_its_marker_do_not_hold() {
    use bunko_remote::status::{Lease, LeaseConfig, LeaseState, MARKER_NAME, MemoryLeaseStore};
    // Mokuro Reader 1.10 on Drive deletes for good: B, its copies and the marker in it.
    let f = FakeBackend::drive_like();
    let top = f.account_root();
    let a = f.reader_mkdir_in(&top, "mokuro-reader").pid;
    let s_a = f.reader_mkdir_in(&a, "S").pid;
    f.reader_put_in(&s_a, "V1.cbz", b"v1");
    let b = f.reader_mkdir_in(&top, "mokuro-reader").pid;
    let s_b = f.reader_mkdir_in(&b, "S").pid;
    f.reader_put_in(&s_b, "V1.cbz", b"v1");
    let t = f.reader_mkdir_in(&b, "T").pid;
    f.reader_put_in(&t, "V2.cbz", b"v2");
    f.reader_put_in(&b, MARKER_NAME, b"{}");
    let mut r = rig(f);
    r.ready().await;
    let states = r.record_states();
    r.fake.reader_move(&t, &a);
    r.fake.reader_delete_pid(&b);
    r.settle().await;
    assert_eq!(r.feed.state(), EngineState::Steady);
    never_suspect(&states);
    assert_eq!(r.index.lock().roots(), std::slice::from_ref(&a));
    assert!(r.has("S/V1.cbz") && r.has("T/V2.cbz"));
    assert!(!r.has(MARKER_NAME));
    let evs = r.drain();
    assert_eq!(
        removed_any(&evs, &["S", "S/V1.cbz", "T", "T/V2.cbz"]),
        [],
        "{evs:?}"
    );

    // Bunko's lease writes its marker into roots()[0], as the store starts it.
    let root = r.index.lock().roots()[0].clone();
    let mut lc = LeaseConfig::new("inst", "boot", "host", "0.8.0");
    lc.claim_settle = Some(Duration::ZERO);
    let lease_dir = r.dir.path().join("lease");
    let mut lease = Lease::new(
        r.fake.clone(),
        root,
        lc,
        Arc::new(MemoryLeaseStore::new()),
        lease_dir,
    );
    assert_eq!(lease.claim().await.unwrap(), LeaseState::Holder);
    r.settle().await;
    let marker = r.index.lock().lookup(&rel(MARKER_NAME)).unwrap();
    let node = r.index.lock().winner_node(marker.eid).unwrap();
    assert_eq!(node.parent, Some(a));
}

#[tokio::test(start_paused = true)]
async fn a_same_named_folder_does_not_hide_what_only_the_vanished_root_held() {
    let f = Arc::new(FakeBackend::drive_like());
    let w = twins(&f);
    // B's S holds V9 that A's S does not have; T moves over, then B is trashed whole.
    f.reader_put_in(&w.s_b, "V9.cbz", b"v9");
    let r = rig_with(f.clone(), config(), None);
    r.steady().await;
    let Rig { dir, feed, .. } = r;
    feed.shutdown().await;
    f.reader_move(&w.t, &w.a);
    f.reader_move(&w.vd_b, &w.a);
    f.reader_trash_pid(&w.cat_b);
    f.reader_trash_pid(&w.b);
    let r = rig_with(f.clone(), config(), Some(dir));
    let s = r
        .feed
        .wait_for(|s| matches!(s, EngineState::Suspect { .. }), LONG)
        .await
        .unwrap();
    assert!(
        matches!(
            s,
            EngineState::Suspect {
                root_changed: true,
                ..
            }
        ),
        "{s:?}"
    );
    assert!(r.has("S/V9.cbz"));

    // Without V9, B's S holds only what A's has: dropped.
    let Rig { dir, feed, .. } = r;
    feed.shutdown().await;
    let f = Arc::new(FakeBackend::drive_like());
    let w = twins(&f);
    f.reader_put_in(&w.s_a, "V3.cbz", b"v3");
    let r = rig_with(f.clone(), config(), None);
    r.steady().await;
    let Rig { dir: d2, feed, .. } = r;
    feed.shutdown().await;
    drop(dir);
    f.reader_move(&w.t, &w.a);
    f.reader_move(&w.vd_b, &w.a);
    f.reader_trash_pid(&w.cat_b);
    f.reader_trash_pid(&w.b);
    let r = rig_with(f.clone(), config(), Some(d2));
    let states = r.record_states();
    r.steady().await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    never_suspect(&states);
    assert_eq!(r.index.lock().roots(), std::slice::from_ref(&w.a));
    assert!(r.has("S/V3.cbz") && r.has("T/V2.cbz"));
}
