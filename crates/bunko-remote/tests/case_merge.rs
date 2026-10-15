//! The automatic case merge (spec §5.9 "The merge (owner decision)", §13 case-merge
//! cases): FakeBackend profiles Drive-like, MEGA-like and plain (case-sensitive)
//! WebDAV, real temp dirs, real time, a store driven from std threads.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::{
    BErr, Backend, CaseMergePolicy, CloudHooks, MergeChoice, MergeRecord, OpCtx, RemoteStore,
    RemoteStoreConfig, ResolveChoice, SubmissionResult, UploadSource, UploadTarget,
};
use bunko_store::{Health, LibraryStore, MountGuard, WatchEvent, fs};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const WAIT: Duration = Duration::from_secs(30);
const QUIET: Duration = Duration::from_millis(300);

type Store = RemoteStore<FakeBackend>;

// ---- the server's side, recorded --------------------------------------------------

#[derive(Default)]
struct Rec {
    merged: Mutex<Vec<(String, String, bool)>>,
    removed: Mutex<Vec<(String, bool)>>,
    moved: Mutex<Vec<(String, String)>>,
    submissions: Mutex<Vec<(String, Vec<u8>)>>,
    root: Mutex<PathBuf>,
}

impl Rec {
    fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&*self.root.lock())
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| p.display().to_string())
    }
}

impl CloudHooks for Rec {
    fn moved(&self, from: &Path, to: &Path, _is_dir: bool) {
        self.moved.lock().push((self.rel(from), self.rel(to)));
    }
    fn removed(&self, path: &Path, is_dir: bool) {
        self.removed.lock().push((self.rel(path), is_dir));
    }
    fn sidecar_overwritten(&self, _path: &Path) {}
    fn series_submission(&self, dir: &Path, bytes: &[u8]) -> SubmissionResult {
        self.submissions
            .lock()
            .push((self.rel(dir), bytes.to_vec()));
        SubmissionResult::Applied
    }
    fn ocr_discarded(&self, _path: &Path) {}
    fn merged(&self, from: &Path, to: &Path, is_dir: bool) {
        self.merged
            .lock()
            .push((self.rel(from), self.rel(to), is_dir));
    }
}

struct Policy {
    choice: Mutex<MergeChoice>,
    busy: Mutex<Vec<PathBuf>>,
    audits: Mutex<Vec<MergeRecord>>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            choice: Mutex::new(MergeChoice::Block),
            busy: Mutex::default(),
            audits: Mutex::default(),
        }
    }
}

impl CaseMergePolicy for Policy {
    fn prefer_main(&self, _a: &Path, _b: &Path) -> MergeChoice {
        *self.choice.lock()
    }
    fn busy(&self, path: &Path) -> bool {
        self.busy
            .lock()
            .iter()
            .any(|b| b == path || b.starts_with(path))
    }
    fn audit(&self, m: &MergeRecord) {
        self.audits.lock().push(m.clone());
    }
}

// ---- the harness ------------------------------------------------------------------

struct T {
    tmp: TempDir,
    root: PathBuf,
    fake: Arc<FakeBackend>,
    store: Option<Arc<Store>>,
    mount: Option<MountGuard>,
    hooks: Arc<Rec>,
    policy: Arc<Policy>,
    open: Arc<AtomicBool>,
    tune: fn(&mut RemoteStoreConfig),
}

fn no_tune(_: &mut RemoteStoreConfig) {}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

impl T {
    /// A store over `fake` whose reader folder holds `files` (created in order, so
    /// earlier entries are older: the first spelling of a folder wins).
    fn new(fake: FakeBackend, files: &[(&str, &[u8])]) -> T {
        T::with(fake, files, no_tune)
    }

    fn with(fake: FakeBackend, files: &[(&str, &[u8])], tune: fn(&mut RemoteStoreConfig)) -> T {
        fake.reader_mkdir(FOLDER);
        for (rel, bytes) in files {
            if rel.ends_with('/') {
                fake.reader_mkdir(&format!("{FOLDER}/{}", rel.trim_end_matches('/')));
            } else {
                fake.reader_put(&format!("{FOLDER}/{rel}"), bytes);
            }
        }
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cloud/s/library");
        let mut t = T {
            tmp,
            root,
            fake: Arc::new(fake),
            store: None,
            mount: None,
            hooks: Arc::default(),
            policy: Arc::default(),
            open: Arc::new(AtomicBool::new(true)),
            tune,
        };
        *t.hooks.root.lock() = t.root.clone();
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
        cfg.quiet = QUIET;
        cfg.merge_interval = Duration::from_millis(50);
        (self.tune)(&mut cfg);
        cfg
    }

    fn start(&mut self) {
        let store = RemoteStore::open_shared(self.cfg(), self.fake.clone()).unwrap();
        let open = self.open.clone();
        store.set_lease_gate(Arc::new(move || open.load(Ordering::Acquire)));
        store.set_hooks(self.hooks.clone());
        self.mount = Some(bunko_store::mount(store.clone() as Arc<dyn LibraryStore>));
        self.store = Some(store);
    }

    fn policy(&self) {
        self.s().set_case_merge_policy(self.policy.clone());
    }

    /// A crash: the store goes, nothing more is written.
    fn stop(&mut self) {
        self.mount = None;
        self.store = None;
    }

    fn s(&self) -> &Arc<Store> {
        self.store.as_ref().unwrap()
    }

    fn steady(&self) {
        assert!(self.s().wait_steady(WAIT), "{:?}", self.s().engine_state());
        wait_until("adoption", || self.s().adopted());
    }

    fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for s in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(s);
        }
        p
    }

    fn at(&self, rel: &str) -> Option<bunko_remote::RNode> {
        self.fake.node_at(&format!("{FOLDER}/{rel}"))
    }

    fn bytes(&self, rel: &str) -> Option<Vec<u8>> {
        self.at(rel).and_then(|n| self.fake.content(&n.pid))
    }

    /// Names of the live children of a cloud folder, sorted.
    fn names(&self, rel: &str) -> Vec<String> {
        let Some(dir) = self.at(rel) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        rt().block_on(self.fake.list_dir(&dir.pid, &mut |n| {
            out.push(String::from_utf8_lossy(&n.name_raw).into_owned())
        }))
        .unwrap();
        out.sort();
        out
    }

    fn done_merges(&self) -> usize {
        self.s()
            .recent_merges()
            .iter()
            .filter(|m| m.state == "done")
            .count()
    }

    fn wait_merges(&self, n: usize) {
        wait_until(&format!("{n} finished merge(s)"), || {
            self.done_merges() >= n
        });
    }

    fn blocked(&self) -> Vec<bunko_remote::Conflict> {
        self.s()
            .conflict_rows()
            .into_iter()
            .filter(|c| c.kind == "case-merge-blocked")
            .collect()
    }

    /// One more feed cycle, delivered.
    fn sync_feed(&self) {
        let n = self.fake.calls(FakeOp::Changes);
        self.s().feed().poll_now();
        wait_until("a feed poll", || self.fake.calls(FakeOp::Changes) > n);
        std::thread::sleep(Duration::from_millis(100));
    }
}

type Profile = (&'static str, fn() -> FakeBackend);

fn profiles() -> Vec<Profile> {
    vec![
        ("drive_like", FakeBackend::drive_like as fn() -> FakeBackend),
        ("mega_like", FakeBackend::mega_like),
        ("webdav_plain", FakeBackend::webdav_plain),
    ]
}

// ---- the basic merge, every profile -----------------------------------------------

#[test]
fn case_variants_merge_into_the_oldest_spelling() {
    for (name, make) in profiles() {
        let t = T::new(
            make(),
            &[
                ("Kingdom/Vol 1.cbz", b"one"),
                ("kingdom/Vol 2.cbz", b"two"),
                ("kingdom/Vol 2.mokuro", b"{\"pages\":[]}"),
            ],
        );
        t.steady();
        assert!(t.at("kingdom").is_some(), "{name}: two variants before");
        t.policy();
        t.wait_merges(1);
        assert_eq!(
            t.names("Kingdom"),
            ["Vol 1.cbz", "Vol 2.cbz", "Vol 2.mokuro"],
            "{name}"
        );
        assert!(t.at("kingdom").is_none(), "{name}: the loser is trashed");
        assert_eq!(t.bytes("Kingdom/Vol 2.cbz").unwrap(), b"two", "{name}");
        // The index follows at COMMITTED; the hooks ran once per file.
        assert!(fs::is_file(t.abs("Kingdom/Vol 2.cbz")), "{name}");
        assert!(!fs::exists(t.abs("kingdom")), "{name}");
        let mut merged = t.hooks.merged.lock().clone();
        merged.sort();
        assert_eq!(
            merged,
            [
                (
                    "kingdom/Vol 2.cbz".to_string(),
                    "Kingdom/Vol 2.cbz".to_string(),
                    false
                ),
                (
                    "kingdom/Vol 2.mokuro".to_string(),
                    "Kingdom/Vol 2.mokuro".to_string(),
                    false
                ),
            ],
            "{name}"
        );
        // The feed's echo runs no hooks and counts toward nothing.
        t.sync_feed();
        assert!(
            t.hooks.moved.lock().is_empty(),
            "{name}: echoes run no hooks"
        );
        assert!(t.hooks.removed.lock().is_empty(), "{name}");
        assert_eq!(t.s().health(), Health::Online, "{name}");
        let rec = &t.s().recent_merges()[0];
        assert_eq!(
            (
                rec.winner.as_str(),
                rec.losers.clone(),
                rec.moved,
                rec.origin.as_str()
            ),
            ("Kingdom", vec!["kingdom".to_string()], 2, "auto"),
            "{name}"
        );
        assert_eq!(t.policy.audits.lock().len(), 1, "{name}: audited");
        assert!(
            t.s().conflicts().is_empty(),
            "{name}: {:?}",
            t.s().conflicts()
        );
    }
}

#[test]
fn a_feed_poll_between_a_merge_step_and_its_commit_still_sees_an_own_echo() {
    // Every merge move and trash answers slowly: the change is already at the
    // provider (MEGA's push wakes the feed at once) and the feed polls it before
    // the step's COMMITTED rewrite of the ledger and the index.
    for (name, make) in profiles() {
        let t = T::new(
            make(),
            &[
                ("Kingdom/Vol 1.cbz", b"one"),
                ("kingdom/Vol 2.cbz", b"two"),
                ("kingdom/Vol 2.mokuro", b"{\"pages\":[]}"),
            ],
        );
        t.steady();
        let fake = Arc::downgrade(&t.fake);
        let store = Arc::downgrade(t.s());
        let raced = Arc::new(AtomicUsize::new(0));
        let r2 = raced.clone();
        // The feed's poll count as each call starts: a poll after that (which the
        // push may start even before the after-hook runs) can show the change.
        let polls = Arc::new(AtomicUsize::new(0));
        let (p1, f1) = (polls.clone(), fake.clone());
        t.fake.set_call_hook(Some(Arc::new(move |op| {
            if matches!(op, FakeOp::Update | FakeOp::Trash)
                && let Some(fake) = f1.upgrade()
            {
                p1.store(fake.calls(FakeOp::Changes), Ordering::SeqCst);
            }
        })));
        t.fake.set_after_hook(Some(Arc::new(move |op| {
            if !matches!(op, FakeOp::Update | FakeOp::Trash) {
                return;
            }
            let (Some(fake), Some(store)) = (fake.upgrade(), store.upgrade()) else {
                return;
            };
            let n = polls.load(Ordering::SeqCst);
            store.feed().poll_now();
            // A poll that started before this call may be waiting for this very
            // commit: a fresh one gets a moment, not forever.
            let t0 = Instant::now();
            while fake.calls(FakeOp::Changes) <= n && t0.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if fake.calls(FakeOp::Changes) > n {
                r2.fetch_add(1, Ordering::SeqCst);
                // Room for the poll to apply its page (it must wait for the commit).
                std::thread::sleep(Duration::from_millis(300));
            }
        })));
        t.policy();
        t.wait_merges(1);
        t.fake.set_after_hook(None);
        t.fake.set_call_hook(None);
        assert!(
            raced.load(Ordering::SeqCst) >= 1,
            "{name}: a poll ran between a step's provider call and its commit"
        );
        t.sync_feed();
        assert_eq!(
            t.names("Kingdom"),
            ["Vol 1.cbz", "Vol 2.cbz", "Vol 2.mokuro"],
            "{name}"
        );
        assert!(t.at("kingdom").is_none(), "{name}");
        assert!(fs::is_file(t.abs("Kingdom/Vol 2.mokuro")), "{name}");
        assert!(!fs::exists(t.abs("kingdom")), "{name}");
        let mut merged = t.hooks.merged.lock().clone();
        merged.sort();
        assert_eq!(
            merged,
            [
                (
                    "kingdom/Vol 2.cbz".to_string(),
                    "Kingdom/Vol 2.cbz".to_string(),
                    false
                ),
                (
                    "kingdom/Vol 2.mokuro".to_string(),
                    "Kingdom/Vol 2.mokuro".to_string(),
                    false
                ),
            ],
            "{name}: once per file"
        );
        assert!(
            t.hooks.moved.lock().is_empty(),
            "{name}: echoes run no hooks: {:?}",
            t.hooks.moved.lock()
        );
        assert!(t.hooks.removed.lock().is_empty(), "{name}");
        assert_eq!(t.s().health(), Health::Online, "{name}");
    }
}

#[test]
fn folders_merge_recursively() {
    for (name, make) in profiles() {
        let t = T::new(
            make(),
            &[
                ("Kingdom/Extras/a.jpg", b"a"),
                ("kingdom/extras/b.jpg", b"b"),
                ("kingdom/Other/c.jpg", b"c"),
            ],
        );
        t.steady();
        t.policy();
        t.wait_merges(1);
        assert_eq!(t.names("Kingdom"), ["Extras", "Other"], "{name}");
        assert_eq!(t.names("Kingdom/Extras"), ["a.jpg", "b.jpg"], "{name}");
        assert_eq!(t.names("Kingdom/Other"), ["c.jpg"], "{name}");
        assert!(t.at("kingdom").is_none(), "{name}");
        // A folder only the loser had moves whole (one hook, for the folder).
        assert!(
            t.hooks
                .merged
                .lock()
                .contains(&("kingdom/Other".into(), "Kingdom/Other".into(), true)),
            "{name}: {:?}",
            t.hooks.merged.lock()
        );
    }
}

#[test]
fn an_identical_copy_in_the_loser_goes_to_the_trash() {
    for (name, make) in profiles() {
        let t = T::new(
            make(),
            &[
                ("Kingdom/Vol 1.cbz", b"same"),
                ("kingdom/Vol 1.cbz", b"same"),
            ],
        );
        t.steady();
        let loser = t.at("kingdom/Vol 1.cbz").unwrap().pid;
        t.policy();
        t.wait_merges(1);
        assert_eq!(t.names("Kingdom"), ["Vol 1.cbz"], "{name}");
        assert!(t.at("kingdom").is_none(), "{name}");
        assert!(t.fake.trashed_log().contains(&loser), "{name}");
        assert!(t.fake.deleted_log().is_empty(), "{name}: never deleted");
        assert_eq!(
            *t.hooks.removed.lock(),
            [("kingdom/Vol 1.cbz".to_string(), false)],
            "{name}"
        );
    }
}

#[test]
fn a_different_copy_blocks_its_group_and_keeps_the_loser() {
    for (name, make) in profiles() {
        let t = T::new(
            make(),
            &[
                ("Kingdom/Vol 1.cbz", b"winner"),
                ("kingdom/Vol 1.cbz", b"loser!"),
                ("kingdom/Vol 1.mokuro", b"{}"),
                ("kingdom/Vol 2.cbz", b"two"),
            ],
        );
        t.steady();
        t.policy();
        t.wait_merges(1);
        assert_eq!(t.names("Kingdom"), ["Vol 1.cbz", "Vol 2.cbz"], "{name}");
        // The whole group stays in the loser; the folder is not trashed.
        assert_eq!(t.names("kingdom"), ["Vol 1.cbz", "Vol 1.mokuro"], "{name}");
        let b = t.blocked();
        assert_eq!(b.len(), 1, "{name}: {b:?}");
        assert_eq!(b[0].path, "kingdom/Vol 1.cbz", "{name}");
        assert_eq!(
            b[0].this,
            ["kingdom/Vol 1.cbz", "kingdom/Vol 1.mokuro"],
            "{name}"
        );
        assert_eq!(b[0].that, ["Kingdom/Vol 1.cbz"], "{name}");
        // The next passes do not record it again.
        std::thread::sleep(QUIET * 2);
        assert_eq!(t.blocked().len(), 1, "{name}");
    }
}

// ---- when ---------------------------------------------------------------------------

#[test]
fn a_variant_still_changing_is_left_alone_until_quiet() {
    // The quiet period runs from when the store notes a change, which trails the
    // reader's upload by the feed's latency (a second or more under load): with the
    // usual QUIET two arrivals could be noted farther apart than it, and the merge
    // rightly started mid-loop. A longer one here, each arrival waited for until the
    // merge has noted it, and a check that no two came farther apart than it.
    const LONG: Duration = Duration::from_secs(5);
    fn tune(c: &mut RemoteStoreConfig) {
        c.quiet = LONG;
    }
    let t = T::with(
        FakeBackend::drive_like(),
        &[("Kingdom/Vol 1.cbz", b"one"), ("kingdom/Vol 2.cbz", b"two")],
        tune,
    );
    t.steady();
    // The merge's own sink was registered first, so it has noted what this one sees.
    let seen: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let s2 = seen.clone();
    let _watch = t
        .s()
        .watch(Arc::new(move |e: WatchEvent| {
            if let WatchEvent::Added { path, .. } = e {
                s2.lock().push(path);
            }
        }))
        .unwrap();
    // A reader keeps uploading into the loser: each arrival resets its quiet period.
    let arrive = |i: usize| -> Instant {
        let put = Instant::now();
        t.fake
            .reader_put(&format!("{FOLDER}/kingdom/extra {i}.jpg"), b"x");
        t.s().feed().poll_now();
        let p = t.abs(&format!("kingdom/extra {i}.jpg"));
        wait_until(&format!("extra {i} noted"), || seen.lock().contains(&p));
        put
    };
    let mut prev = arrive(0);
    // The policy goes in once a change is fresh (the adoption's may be old already).
    t.policy();
    for i in 1..8 {
        let put = arrive(i);
        let merges = t.s().recent_merges();
        // Every pass so far saw a change noted after `prev`'s upload began.
        let gap = prev.elapsed();
        assert!(
            gap < LONG,
            "the harness fell behind: {gap:?} between arrivals"
        );
        assert!(merges.is_empty(), "still uploading: {merges:?}");
        prev = put;
    }
    t.wait_merges(1);
    assert_eq!(t.names("Kingdom").len(), 10);
    assert!(t.at("kingdom").is_none());
}

#[test]
fn no_merge_while_suspect() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.suspect_min_removals = 3;
    }
    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("Kingdom/Vol 1.cbz".into(), b"one".to_vec()),
        ("kingdom/Vol 2.cbz".into(), b"two".to_vec()),
    ];
    for i in 0..10 {
        files.push((format!("Other/f{i}.jpg"), b"x".to_vec()));
    }
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_slice()))
        .collect();
    let t = T::with(FakeBackend::drive_like(), &refs, tune);
    t.steady();
    for i in 0..10 {
        t.fake.reader_delete(&format!("{FOLDER}/Other/f{i}.jpg"));
    }
    t.sync_feed();
    wait_until("suspect", || t.s().health() == Health::Suspect);
    t.policy();
    std::thread::sleep(QUIET * 4);
    assert_eq!(t.done_merges(), 0, "no merge while Suspect");
    assert!(t.at("kingdom").is_some());
    t.s().feed().confirm_suspect();
    t.sync_feed();
    t.wait_merges(1);
    assert!(t.at("kingdom").is_none());
}

#[test]
fn no_merge_in_standby() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.lease = true;
    }
    let fake = FakeBackend::drive_like();
    let marker = serde_json::json!({
        "version": 1, "kind": "mokuro-bunko", "instance_id": "other-install",
        "boot_id": "b", "instance_name": "nas", "epoch": 5,
        "claimed_at": "2026-10-12T10:00:00Z", "ttl_s": 900, "state": "active"
    });
    fake.reader_mkdir(FOLDER);
    fake.reader_put(
        &format!("{FOLDER}/.mokuro-bunko.json"),
        &serde_json::to_vec(&marker).unwrap(),
    );
    let t = T::with(
        fake,
        &[("Kingdom/Vol 1.cbz", b"one"), ("kingdom/Vol 2.cbz", b"two")],
        tune,
    );
    t.steady();
    wait_until("standby", || {
        t.s().health() == Health::LeaseHeld { by: "nas".into() }
    });
    t.policy();
    std::thread::sleep(QUIET * 4);
    assert_eq!(t.done_merges(), 0);
    assert!(t.at("kingdom").is_some());
    assert_eq!(t.fake.calls(FakeOp::Update), 0);
}

#[test]
fn no_merge_while_an_ocr_claim_or_an_upgrade_runs_under_a_variant() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Kingdom/Vol 1.cbz", b"one"), ("kingdom/Vol 2.cbz", b"two")],
    );
    t.steady();
    // The scheduler holds a claim on (or spools, or the census judges) a loser volume.
    t.policy.busy.lock().push(t.abs("kingdom/Vol 2.cbz"));
    t.policy();
    std::thread::sleep(QUIET * 4);
    assert_eq!(t.done_merges(), 0);
    assert!(!t.s().merge_busy(&t.abs("kingdom/Vol 2.cbz")));
    t.policy.busy.lock().clear();
    t.s().merge_now();
    t.wait_merges(1);
}

// ---- safety -------------------------------------------------------------------------

#[test]
fn a_crash_mid_merge_resumes_after_restart() {
    let mut t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"one"),
            ("kingdom/Vol 2.cbz", b"two"),
            ("kingdom/Vol 3.cbz", b"three"),
        ],
    );
    t.steady();
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    t.s().set_failpoint(Some(Arc::new(move |p: &str| {
        p == "MERGE_MOVED" && n2.fetch_add(1, Ordering::SeqCst) == 0
    })));
    t.policy();
    wait_until("the crash", || t.s().crashed());
    assert_eq!(t.s().recent_merges()[0].state, "open");
    t.stop();
    // The move reached the provider; nothing was recorded after it.
    t.start();
    t.steady();
    assert!(
        t.s().merge_busy(&t.abs("kingdom/Vol 3.cbz")),
        "the open group is busy again at once"
    );
    t.policy();
    t.wait_merges(1);
    assert_eq!(t.names("Kingdom"), ["Vol 1.cbz", "Vol 2.cbz", "Vol 3.cbz"]);
    assert!(t.at("kingdom").is_none());
    assert!(!t.s().merge_busy(&t.abs("Kingdom/Vol 3.cbz")));
}

#[test]
fn a_reader_upload_into_the_loser_during_the_merge_is_merged_next_pass() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Kingdom/Vol 1.cbz", b"one"), ("kingdom/Vol 2.cbz", b"two")],
    );
    t.steady();
    let fake = t.fake.clone();
    let once = Arc::new(AtomicBool::new(false));
    let o2 = once.clone();
    t.fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Update && !o2.swap(true, Ordering::SeqCst) {
            fake.reader_put(&format!("{FOLDER}/kingdom/Vol 3.cbz"), b"three");
        }
    })));
    t.policy();
    t.wait_merges(1);
    // The finish saw Vol 3 in the loser: not trashed.
    assert_eq!(t.names("kingdom"), ["Vol 3.cbz"]);
    t.fake.set_call_hook(None);
    t.sync_feed();
    t.wait_merges(2);
    assert_eq!(t.names("Kingdom"), ["Vol 1.cbz", "Vol 2.cbz", "Vol 3.cbz"]);
    assert!(t.at("kingdom").is_none());
}

#[test]
fn a_merge_past_the_d19_threshold_is_not_suspect_and_hooks_run_once_per_file() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.suspect_min_removals = 3;
    }
    let mut files: Vec<(String, Vec<u8>)> = vec![("Kingdom/Vol 0.cbz".into(), b"zero".to_vec())];
    for i in 1..=20 {
        files.push((format!("kingdom/Vol {i}.cbz"), format!("v{i}").into_bytes()));
    }
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_slice()))
        .collect();
    let t = T::with(FakeBackend::drive_like(), &refs, tune);
    t.steady();
    t.policy();
    t.wait_merges(1);
    t.sync_feed();
    t.sync_feed();
    assert_eq!(
        t.s().health(),
        Health::Online,
        "moves and trashes are own echoes"
    );
    assert_eq!(t.names("Kingdom").len(), 21);
    assert_eq!(t.hooks.merged.lock().len(), 20, "once per file");
    assert!(t.hooks.moved.lock().is_empty());
}

#[test]
fn suspect_mid_merge_pauses_and_resumes_from_the_journal() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.suspect_min_removals = 3;
    }
    let mut files: Vec<(String, Vec<u8>)> = vec![("Kingdom/Vol 0.cbz".into(), b"zero".to_vec())];
    for i in 1..=4 {
        files.push((format!("kingdom/Vol {i}.cbz"), format!("v{i}").into_bytes()));
    }
    for i in 0..10 {
        files.push((format!("Other/f{i}.jpg"), b"x".to_vec()));
    }
    let refs: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_slice()))
        .collect();
    let t = T::with(FakeBackend::drive_like(), &refs, tune);
    t.steady();
    // As the second move starts (its stat; no feed page applies during the provider
    // call itself, the commit window), the reader removes a lot elsewhere and the
    // feed sees it.
    let fake = t.fake.clone();
    let store = Arc::downgrade(t.s());
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    let fired = Arc::new(AtomicBool::new(false));
    t.fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Update {
            n2.fetch_add(1, Ordering::SeqCst);
        }
        if op == FakeOp::Stat
            && n2.load(Ordering::SeqCst) == 1
            && !fired.swap(true, Ordering::SeqCst)
        {
            for i in 0..10 {
                fake.reader_delete(&format!("{FOLDER}/Other/f{i}.jpg"));
            }
            if let Some(s) = store.upgrade() {
                s.feed().poll_now();
                let t0 = Instant::now();
                while s.health() != Health::Suspect && t0.elapsed() < WAIT {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    })));
    t.policy();
    wait_until("suspect", || t.s().health() == Health::Suspect);
    // The second move was past its gate check when the hook ran: it goes on to its
    // provider call and COMMITTED (several state-db writes, which can take seconds
    // under load) before the group can pause. Then give a merge that wrongly went on
    // the time to take a third step.
    wait_until("the second move", || {
        t.s().recent_merges().first().is_some_and(|m| m.moved >= 2)
    });
    std::thread::sleep(QUIET * 2);
    let rec = t.s().recent_merges()[0].clone();
    assert_eq!(rec.state, "open", "paused, not finished");
    assert_eq!(rec.moved, 2);
    assert!(t.s().merge_busy(&t.abs("kingdom/Vol 4.cbz")));
    assert_eq!(t.names("kingdom").len(), 2);
    t.fake.set_call_hook(None);
    t.s().feed().confirm_suspect();
    t.sync_feed();
    t.wait_merges(1);
    assert_eq!(t.names("Kingdom").len(), 5);
    assert!(t.at("kingdom").is_none());
}

// ---- volume groups ------------------------------------------------------------------

fn mokuro(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"version": "0.2.1", "pages": [], "note": text})).unwrap()
}

#[test]
fn differing_main_sidecars_keep_the_one_matching_the_primary_recipe() {
    // The winner's matches: the loser's unstamped one becomes a pre-upgrade layer.
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"same"),
            ("Kingdom/Vol 1.mokuro", &mokuro("winner")),
            ("kingdom/Vol 1.cbz", b"same"),
            ("kingdom/Vol 1.mokuro", &mokuro("loser")),
        ],
    );
    t.steady();
    *t.policy.choice.lock() = MergeChoice::A;
    t.policy();
    t.wait_merges(1);
    assert_eq!(
        t.names("Kingdom"),
        ["Vol 1.cbz", "Vol 1.mokuro", "Vol 1.pre-upgrade.mokuro"]
    );
    assert_eq!(t.bytes("Kingdom/Vol 1.mokuro").unwrap(), mokuro("winner"));
    let kept: serde_json::Value =
        serde_json::from_slice(&t.bytes("Kingdom/Vol 1.pre-upgrade.mokuro").unwrap()).unwrap();
    assert_eq!(kept["note"], "loser");
    assert_eq!(
        kept["preserved"]["sha256"],
        hex::encode(Sha256::digest(mokuro("loser")))
    );
    assert!(kept.get("ocr_engine").is_none());
    assert!(t.at("kingdom").is_none());

    // The loser's matches: the winner's is kept as the layer, the loser's moves in.
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"same"),
            ("Kingdom/Vol 1.mokuro", &mokuro("winner")),
            ("kingdom/Vol 1.cbz", b"same"),
            ("kingdom/Vol 1.mokuro", &mokuro("loser")),
        ],
    );
    t.steady();
    *t.policy.choice.lock() = MergeChoice::B;
    t.policy();
    t.wait_merges(1);
    assert_eq!(t.bytes("Kingdom/Vol 1.mokuro").unwrap(), mokuro("loser"));
    let kept: serde_json::Value =
        serde_json::from_slice(&t.bytes("Kingdom/Vol 1.pre-upgrade.mokuro").unwrap()).unwrap();
    assert_eq!(kept["note"], "winner");

    // Upgrades off, or neither (or both) matching: the group is blocked.
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"same"),
            ("Kingdom/Vol 1.mokuro", &mokuro("winner")),
            ("kingdom/Vol 1.cbz", b"same"),
            ("kingdom/Vol 1.mokuro", &mokuro("loser")),
        ],
    );
    t.steady();
    t.policy();
    wait_until("the block", || !t.blocked().is_empty());
    assert_eq!(t.names("kingdom"), ["Vol 1.cbz", "Vol 1.mokuro"]);
    assert_eq!(t.names("Kingdom"), ["Vol 1.cbz", "Vol 1.mokuro"]);
}

fn stamped_mokuro(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "pages": [], "note": text, "ocr_engine": {"id": "hayai-nova", "detector": "rtdetr"}
    }))
    .unwrap()
}

/// The loser's main wins: the winner's is kept as a layer first (stamped or not), then
/// replaced in place; the winner's is never trashed before the replacement is in.
#[test]
fn a_winning_loser_main_replaces_the_winners_in_place_after_keeping_it() {
    for (name, make) in [
        ("drive_like", FakeBackend::drive_like as fn() -> FakeBackend),
        ("mega_like", FakeBackend::mega_like),
        ("webdav_cas", FakeBackend::webdav_cas),
    ] {
        let t = T::new(
            make(),
            &[
                ("Kingdom/Vol 1.cbz", b"same"),
                ("Kingdom/Vol 1.mokuro", &stamped_mokuro("winner")),
                ("kingdom/Vol 1.cbz", b"same"),
                ("kingdom/Vol 1.mokuro", &mokuro("loser")),
            ],
        );
        t.steady();
        let old_main = t.at("Kingdom/Vol 1.mokuro").unwrap().pid;
        let loser_main = t.at("kingdom/Vol 1.mokuro").unwrap().pid;
        *t.policy.choice.lock() = MergeChoice::B;
        t.policy();
        t.wait_merges(1);
        assert_eq!(
            t.names("Kingdom"),
            ["Vol 1.cbz", "Vol 1.mokuro", "Vol 1.pre-upgrade.mokuro"],
            "{name}"
        );
        assert_eq!(
            t.bytes("Kingdom/Vol 1.mokuro").unwrap(),
            mokuro("loser"),
            "{name}"
        );
        // Bunko's own stamp is no shortcut here: the winner's is kept.
        let kept: serde_json::Value =
            serde_json::from_slice(&t.bytes("Kingdom/Vol 1.pre-upgrade.mokuro").unwrap()).unwrap();
        assert_eq!(kept["note"], "winner", "{name}");
        assert!(kept.get("ocr_engine").is_none(), "{name}");
        assert!(t.at("kingdom").is_none(), "{name}");
        let trashed = t.fake.trashed_log();
        assert!(
            trashed.contains(&loser_main),
            "{name}: the loser's copy is trashed"
        );
        if name == "mega_like" {
            // A replace there is a new node: the old one leaves after the commit.
            assert_ne!(
                t.at("Kingdom/Vol 1.mokuro").unwrap().pid,
                old_main,
                "{name}"
            );
        } else {
            // Replaced in place: same node, new version.
            assert_eq!(
                t.at("Kingdom/Vol 1.mokuro").unwrap().pid,
                old_main,
                "{name}"
            );
        }
        assert!(t.fake.deleted_log().is_empty(), "{name}: nothing deleted");
        assert!(
            t.s().conflicts().is_empty(),
            "{name}: {:?}",
            t.s().conflicts()
        );
        // The DB hooks: the loser's OCR row now names the winner's main.
        assert!(
            t.hooks.merged.lock().contains(&(
                "kingdom/Vol 1.mokuro".into(),
                "Kingdom/Vol 1.mokuro".into(),
                false
            )),
            "{name}: {:?}",
            t.hooks.merged.lock()
        );
    }
}

#[test]
fn a_refused_replace_leaves_the_winners_main_and_blocks_the_group() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"same"),
            ("Kingdom/Vol 1.mokuro", &mokuro("winner")),
            ("kingdom/Vol 1.cbz", b"same"),
            ("kingdom/Vol 1.mokuro", &mokuro("loser")),
        ],
    );
    t.steady();
    // The first upload is the pre-upgrade layer; the replace (the second) gets a 412.
    let fake = t.fake.clone();
    let n = Arc::new(AtomicUsize::new(0));
    let n2 = n.clone();
    t.fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Upload && n2.fetch_add(1, Ordering::SeqCst) == 1 {
            fake.fail_next(FakeOp::Upload, BErr::PreconditionFailed);
        }
    })));
    *t.policy.choice.lock() = MergeChoice::B;
    t.policy();
    t.wait_merges(1);
    assert_eq!(
        n.load(Ordering::SeqCst),
        2,
        "layer, then the refused replace"
    );
    assert_eq!(t.bytes("Kingdom/Vol 1.mokuro").unwrap(), mokuro("winner"));
    assert_eq!(t.names("kingdom"), ["Vol 1.cbz", "Vol 1.mokuro"]);
    assert!(
        t.fake.trashed_log().is_empty(),
        "nothing of the group is trashed"
    );
    let b = t.blocked();
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(b[0].this, ["kingdom/Vol 1.mokuro"]);
    assert_eq!(b[0].that, ["Kingdom/Vol 1.mokuro"]);
}

#[test]
fn identical_archives_move_a_loser_only_layer_in_under_the_winners_stem() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"same"),
            ("kingdom/vol 1.cbz", b"same"),
            ("kingdom/vol 1.edit.mokuro", &mokuro("edit")),
            ("kingdom/vol 1.webp", b"cover"),
        ],
    );
    t.steady();
    t.policy();
    t.wait_merges(1);
    assert_eq!(
        t.names("Kingdom"),
        ["Vol 1.cbz", "Vol 1.edit.mokuro", "Vol 1.webp"]
    );
    assert!(t.at("kingdom").is_none());
    assert!(t.hooks.merged.lock().contains(&(
        "kingdom/vol 1.edit.mokuro".into(),
        "Kingdom/Vol 1.edit.mokuro".into(),
        false
    )));
}

#[test]
fn an_unstamped_layer_whose_id_the_winner_uses_moves_in_as_the_next_free_id() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"same"),
            ("Kingdom/Vol 1.edit.mokuro", &mokuro("mine")),
            ("Kingdom/Vol 1.webp", b"cover-w"),
            ("kingdom/Vol 1.cbz", b"same"),
            ("kingdom/Vol 1.edit.mokuro", &mokuro("theirs")),
            ("kingdom/Vol 1.webp", b"cover-l"),
        ],
    );
    t.steady();
    t.policy();
    t.wait_merges(1);
    assert_eq!(
        t.names("Kingdom"),
        [
            "Vol 1.cbz",
            "Vol 1.edit-2.mokuro",
            "Vol 1.edit.mokuro",
            "Vol 1.webp"
        ]
    );
    assert_eq!(
        t.bytes("Kingdom/Vol 1.edit-2.mokuro").unwrap(),
        mokuro("theirs")
    );
    // The winner has a cover: the loser's is trashed.
    assert_eq!(t.bytes("Kingdom/Vol 1.webp").unwrap(), b"cover-w");
    assert!(t.at("kingdom").is_none());
}

#[test]
fn the_losers_series_json_is_folded_in_as_a_submission_then_trashed() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"one"),
            ("Kingdom/series.json", b"{\"title\":\"Kingdom\"}"),
            ("kingdom/series.json", b"{\"title\":\"kingdom\",\"x\":1}"),
        ],
    );
    t.steady();
    let loser = t.at("kingdom/series.json").unwrap();
    t.policy();
    t.wait_merges(1);
    assert!(t.at("kingdom").is_none());
    assert!(t.fake.trashed_log().contains(&loser.pid));
    assert!(t.hooks.submissions.lock().contains(&(
        "Kingdom".into(),
        b"{\"title\":\"kingdom\",\"x\":1}".to_vec()
    )));
    assert!(t.s().submissions().contains(&(
        "Kingdom/series.json".into(),
        loser.cver,
        "applied".into()
    )));
    assert_eq!(
        t.bytes("Kingdom/series.json").unwrap(),
        b"{\"title\":\"Kingdom\"}"
    );
}

#[test]
fn same_folder_file_variants_merge_by_rename() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("S/Vol 1.cbz", b"same"),
            ("S/vol 1.cbz", b"same"),
            ("S/vol 1.mokuro", &mokuro("ocr")),
        ],
    );
    t.steady();
    t.policy();
    t.wait_merges(1);
    assert_eq!(t.names("S"), ["Vol 1.cbz", "Vol 1.mokuro"]);
    assert_eq!(t.bytes("S/Vol 1.mokuro").unwrap(), mokuro("ocr"));
}

#[test]
fn same_folder_variants_of_a_plain_file_merge_or_block() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("S/Cover.jpg", b"same"),
            ("S/cover.jpg", b"same"),
            ("S/Notes.txt", b"one"),
            ("S/notes.txt", b"two"),
        ],
    );
    t.steady();
    t.policy();
    wait_until("the block", || !t.blocked().is_empty());
    t.wait_merges(1);
    assert_eq!(t.names("S"), ["Cover.jpg", "Notes.txt", "notes.txt"]);
    let b = t.blocked();
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(
        (b[0].this.clone(), b[0].that.clone()),
        (
            vec!["S/notes.txt".to_string()],
            vec!["S/Notes.txt".to_string()]
        )
    );
}

// ---- moves --------------------------------------------------------------------------

#[test]
fn a_webdav_overwrite_f_412_blocks_the_group() {
    let t = T::new(
        FakeBackend::webdav_plain(),
        &[
            ("Kingdom/Vol 1.cbz", b"one"),
            ("kingdom/Vol 2.cbz", b"two"),
            ("kingdom/Vol 3.cbz", b"three"),
        ],
    );
    t.steady();
    let fake = t.fake.clone();
    let once = Arc::new(AtomicBool::new(false));
    let o2 = once.clone();
    t.fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Update && !o2.swap(true, Ordering::SeqCst) {
            // Another client takes the destination name first.
            fake.reader_put(&format!("{FOLDER}/Kingdom/Vol 2.cbz"), b"racer");
        }
    })));
    t.policy();
    t.wait_merges(1);
    assert_eq!(
        t.bytes("Kingdom/Vol 2.cbz").unwrap(),
        b"racer",
        "never overwritten"
    );
    assert_eq!(t.names("kingdom"), ["Vol 2.cbz"]);
    assert_eq!(t.names("Kingdom"), ["Vol 1.cbz", "Vol 2.cbz", "Vol 3.cbz"]);
    let b = t.blocked();
    assert_eq!(b.len(), 1, "{b:?}");
    assert_eq!(b[0].this, ["kingdom/Vol 2.cbz"]);
}

#[test]
fn a_drive_duplicate_after_a_move_is_a_dup_file_and_nothing_is_trashed() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"same"),
            ("kingdom/Vol 1.cbz", b"same"),
            ("kingdom/Vol 2.cbz", b"two"),
        ],
    );
    t.steady();
    let fake = t.fake.clone();
    let once = Arc::new(AtomicBool::new(false));
    let o2 = once.clone();
    t.fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Update && !o2.swap(true, Ordering::SeqCst) {
            fake.reader_put(&format!("{FOLDER}/Kingdom/Vol 2.cbz"), b"racer");
        }
    })));
    t.policy();
    t.wait_merges(1);
    let dups: Vec<_> = t
        .s()
        .conflict_rows()
        .into_iter()
        .filter(|c| c.kind == "dup-file")
        .collect();
    assert_eq!(dups.len(), 1, "{:?}", t.s().conflict_rows());
    assert_eq!(dups[0].path, "Kingdom/Vol 2.cbz");
    // Nothing trashed: the identical Vol 1 and the folder stay.
    assert!(t.fake.trashed_log().is_empty());
    assert_eq!(t.names("kingdom"), ["Vol 1.cbz"]);
}

#[test]
fn every_member_of_a_multi_member_losing_entry_is_emptied_and_trashed() {
    let fake = FakeBackend::drive_like();
    fake.reader_mkdir(FOLDER);
    fake.reader_put(&format!("{FOLDER}/Kingdom/Vol 1.cbz"), b"one");
    let root = fake.node_at(FOLDER).unwrap().pid;
    let rt = rt();
    let tmp = TempDir::new().unwrap();
    let mut losers = Vec::new();
    for (i, body) in [(2, b"two"), (3, b"thr")] {
        let dir = rt
            .block_on(fake.mkdir(&root, "kingdom", &OpCtx::default()))
            .unwrap();
        let f = tmp.path().join(format!("v{i}"));
        std::fs::write(&f, body).unwrap();
        rt.block_on(fake.upload(
            UploadTarget::New {
                parent: &dir.pid,
                name: &format!("Vol {i}.cbz"),
            },
            UploadSource::File {
                path: &f,
                size: 3,
                sha256: Sha256::digest(body).into(),
            },
            &mut OpCtx::default(),
        ))
        .unwrap();
        losers.push(dir.pid);
    }
    let t = T::new(fake, &[]);
    t.steady();
    t.policy();
    t.wait_merges(1);
    assert_eq!(t.names("Kingdom"), ["Vol 1.cbz", "Vol 2.cbz", "Vol 3.cbz"]);
    let trashed = t.fake.trashed_log();
    for l in &losers {
        assert!(trashed.contains(l), "{l} trashed: {trashed:?}");
    }
    assert!(t.at("kingdom").is_none());
}

#[test]
fn mega_compares_by_size_then_spooled_sha256_and_defers_under_transfer_quota() {
    let big = |b: u8| vec![b; (4 << 20) + 10];
    let mut odd = big(1);
    odd[100] = 9;
    let t = T::new(
        FakeBackend::mega_like(),
        &[
            ("Kingdom/Vol 1.cbz", &big(1)),
            ("Kingdom/Vol 2.cbz", &big(2)),
            ("Kingdom/Vol 3.cbz", &big(3)),
            ("kingdom/Vol 1.cbz", &big(1)),
            ("kingdom/Vol 2.cbz", &odd),
            ("kingdom/Vol 3.cbz", b"short"),
        ],
    );
    t.steady();
    // A transfer quota on the first spool read: the merge is deferred.
    t.fake.fail_next(
        FakeOp::Read,
        BErr::TransferQuota(Some(Duration::from_millis(400))),
    );
    t.fake.reset_calls();
    t.policy();
    wait_until("a read", || t.fake.calls(FakeOp::Read) >= 1);
    std::thread::sleep(QUIET * 2);
    assert!(t.s().recent_merges().is_empty(), "deferred");
    assert_eq!(t.fake.calls(FakeOp::Update), 0);
    assert!(
        matches!(t.s().health(), Health::Blocked(_)),
        "{:?}",
        t.s().health()
    );
    // A download after the quota's delay probes it; the merge runs then.
    std::thread::sleep(Duration::from_millis(500));
    let _ = fs::read(t.abs("Kingdom/Vol 1.cbz"));
    wait_until("online", || t.s().health() == Health::Online);
    t.fake.reset_calls();
    t.wait_merges(1);
    // Vol 1 identical (spooled sha256), Vol 2 same size but other bytes, Vol 3 another
    // size (never read).
    assert_eq!(t.names("kingdom"), ["Vol 2.cbz", "Vol 3.cbz"]);
    let short = t.at("kingdom/Vol 3.cbz").unwrap().pid;
    assert!(
        !t.fake.read_log().iter().any(|r| r.pid == short),
        "sizes differ: no read"
    );
    assert_eq!(t.blocked().len(), 2);
}

// ---- the recreate loop, the admin --------------------------------------------------

#[test]
fn a_loser_recreated_more_than_three_times_in_a_day_stops_being_merged() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Kingdom/Vol 1.cbz", b"one"), ("kingdom/a.jpg", b"a")],
    );
    t.steady();
    t.policy();
    t.wait_merges(1);
    for i in 2..=5 {
        // The reader keys folders by exact name: it makes `kingdom/` again.
        t.fake
            .reader_put(&format!("{FOLDER}/kingdom/r{i}.jpg"), b"r");
        t.sync_feed();
        if i <= 4 {
            t.wait_merges(i);
        }
    }
    wait_until("the guard", || !t.blocked().is_empty());
    let b = t.blocked();
    assert_eq!(b[0].detail, "Mokuro Reader keeps recreating this folder");
    assert_eq!(b[0].path, "kingdom");
    std::thread::sleep(QUIET * 2);
    assert_eq!(t.done_merges(), 4);
    assert_eq!(t.names("kingdom"), ["r5.jpg"]);
}

#[test]
fn a_pass_never_closes_a_resolve_whose_steps_are_still_being_journaled() {
    // The admin's resolve journals its group on the caller's thread while merge
    // passes keep running: here it pauses between the group row and its steps.
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"winner"),
            ("kingdom/Vol 1.cbz", b"loser!"),
        ],
    );
    t.steady();
    t.policy();
    wait_until("the block", || !t.blocked().is_empty());
    let id = t.blocked()[0].id;
    let loser = t.at("kingdom/Vol 1.cbz").unwrap().pid;
    t.s().set_failpoint(Some(Arc::new(|at: &str| {
        if at == "MERGE_GROUP" {
            std::thread::sleep(QUIET);
        }
        false
    })));
    t.s().resolve_conflict(id, ResolveChoice::KeepThat).unwrap();
    t.s().set_failpoint(None);
    // Not closed with nothing done (its trash queued forever): the step runs.
    wait_until("trashed", || t.fake.trashed_log().contains(&loser));
    t.sync_feed();
    wait_until("the loser folder goes", || t.at("kingdom").is_none());
    wait_until("no step left queued", || t.s().outbox().is_empty());
    let admin = t
        .s()
        .recent_merges()
        .into_iter()
        .find(|m| m.origin == "admin")
        .unwrap();
    assert_eq!((admin.state.as_str(), admin.trashed), ("done", 1));
}

#[test]
fn keep_this_copy_and_keep_that_copy_resolve_a_block() {
    // Keep that copy (the winner's): the loser's goes, the folder then finishes.
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"winner"),
            ("kingdom/Vol 1.cbz", b"loser!"),
        ],
    );
    t.steady();
    t.policy();
    wait_until("the block", || !t.blocked().is_empty());
    let id = t.blocked()[0].id;
    let loser = t.at("kingdom/Vol 1.cbz").unwrap().pid;
    t.s().resolve_conflict(id, ResolveChoice::KeepThat).unwrap();
    wait_until("trashed", || t.fake.trashed_log().contains(&loser));
    t.sync_feed();
    wait_until("the loser folder goes", || t.at("kingdom").is_none());
    assert_eq!(t.bytes("Kingdom/Vol 1.cbz").unwrap(), b"winner");
    assert!(t.blocked().is_empty());
    assert!(t.s().recent_merges().iter().any(|m| m.origin == "admin"));

    // Keep this copy (the loser's): the winner's goes, the loser's moves in.
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"winner"),
            ("kingdom/Vol 1.cbz", b"loser!"),
        ],
    );
    t.steady();
    t.policy();
    wait_until("the block", || !t.blocked().is_empty());
    let id = t.blocked()[0].id;
    t.s().resolve_conflict(id, ResolveChoice::KeepThis).unwrap();
    t.sync_feed();
    wait_until("the loser folder goes", || t.at("kingdom").is_none());
    assert_eq!(t.bytes("Kingdom/Vol 1.cbz").unwrap(), b"loser!");
    // Only a case-merge-blocked conflict has copies.
    assert!(
        t.s()
            .resolve_conflict(9999, ResolveChoice::KeepThis)
            .is_err()
    );
}

#[test]
fn a_claim_asked_for_during_an_open_merge_is_held_until_it_ends() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"one"),
            ("kingdom/Vol 2.cbz", b"two"),
            ("kingdom/Vol 3.cbz", b"three"),
        ],
    );
    t.steady();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (s2, store, paths) = (
        seen.clone(),
        Arc::downgrade(t.s()),
        [
            t.abs("kingdom/Vol 3.cbz"),
            t.abs("Kingdom/Vol 1.cbz"),
            t.abs("KINGDOM/x/y.cbz"),
            t.abs("Other/Vol 1.cbz"),
        ],
    );
    t.fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Update
            && let Some(s) = store.upgrade()
        {
            s2.lock()
                .push(paths.iter().map(|p| s.merge_busy(p)).collect::<Vec<_>>());
        }
    })));
    assert!(!t.s().merge_busy(&t.abs("kingdom/Vol 3.cbz")));
    t.policy();
    t.wait_merges(1);
    // During the merge both spellings (any case) were busy, other series were not.
    for b in seen.lock().iter() {
        assert_eq!(*b, [true, true, true, false]);
    }
    assert!(!seen.lock().is_empty());
    assert!(!t.s().merge_busy(&t.abs("Kingdom/Vol 3.cbz")));
}

// ---- PathCase -----------------------------------------------------------------------

#[test]
fn path_case_resolves_a_folded_request_to_the_oldest_variant() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.case_merge = false;
    }
    // The oldest spelling sorts last: `min()` would pick the other one.
    let t = T::with(
        FakeBackend::drive_like(),
        &[("kingdom/Vol 1.cbz", b"old"), ("Kingdom/Vol 2.cbz", b"new")],
        tune,
    );
    t.steady();
    assert_eq!(
        t.s().case_winner(&t.root, "KINGDOM").as_deref(),
        Some("kingdom")
    );
    let pc = bunko_dav::LibraryPathCanonicalizer::new(&t.root, Some(true));
    assert_eq!(
        pc.canonicalize("KINGDOM/Vol 3.cbz", None),
        "kingdom/Vol 3.cbz"
    );
    // An exact spelling still wins.
    assert_eq!(
        pc.canonicalize("Kingdom/Vol 3.cbz", None),
        "Kingdom/Vol 3.cbz"
    );

    // A local library keeps the first in sorted order. On a filesystem that ignores
    // case (APFS, NTFS) the second mkdir is the first folder, so there is one variant:
    // its on-disk spelling. The canonicalizer probes the filesystem, as the server's does.
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("kingdom")).unwrap();
    std::fs::create_dir_all(dir.path().join("Kingdom")).unwrap();
    let both = std::fs::read_dir(dir.path()).unwrap().count() == 2;
    let local = Arc::new(bunko_store::LocalStore::new(dir.path()));
    let _m = bunko_store::mount(local as Arc<dyn LibraryStore>);
    let pc = bunko_dav::LibraryPathCanonicalizer::new(dir.path(), None);
    let want = if both { "Kingdom/x" } else { "kingdom/x" };
    assert_eq!(pc.canonicalize("KINGDOM/x", None), want);
}

#[test]
fn a_write_bunko_queued_for_the_loser_lands_in_the_winner() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("Kingdom/Vol 1.cbz", b"one"), ("kingdom/Vol 2.cbz", b"two")],
    );
    t.steady();
    let src = t.tmp.path().join("local-ocr");
    std::fs::write(&src, br#"{"pages":[]}"#).unwrap();
    let dest = t.abs("kingdom/Vol 2.mokuro");
    let once = Arc::new(AtomicBool::new(false));
    let o2 = once.clone();
    t.fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Update && !o2.swap(true, Ordering::SeqCst) {
            // An OCR result for the loser's volume, queued while the merge runs.
            let w = bunko_store::WriteIntent::new(
                bunko_store::Mode::CreateNew,
                bunko_store::Origin::Ocr { claim: "c".into() },
                bunko_store::Durability::Deferred,
            );
            fs::publish(&src, &dest, &w).unwrap();
        }
    })));
    t.policy();
    t.wait_merges(1);
    t.fake.set_call_hook(None);
    t.sync_feed();
    wait_until("everything in the winner", || {
        t.at("kingdom").is_none() && t.at("Kingdom/Vol 2.mokuro").is_some()
    });
    assert_eq!(t.bytes("Kingdom/Vol 2.mokuro").unwrap(), br#"{"pages":[]}"#);
    assert!(
        t.s().conflicts().iter().all(|c| c.0 != "lost-write"),
        "{:?}",
        t.s().conflicts()
    );
}
