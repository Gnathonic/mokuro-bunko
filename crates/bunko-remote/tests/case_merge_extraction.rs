//! The case-merge extraction gate (Track A, step A1): the planner moved into
//! `bunko_core::case_merge` plans exactly what the store's own code planned before.
//!
//! In test builds every plan the store makes is planned twice, by the shared planner
//! and by the code as it read before the move (replayed from the same provider
//! answers), and any difference in fingerprint or in any step stops the process
//! (`bunko_remote::merge`'s `legacy` module). That runs inside every existing
//! fixture (`tests/case_merge.rs`, `tests/dedup.rs`, the server's
//! `cloud_case_merge.rs`), unedited. This test proves the comparison is live: one
//! scenario of each plan shape, in one sequence so the counts are exact, each
//! compared at least once and all equal (the process is still running).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::merge::{DualPlanReport, dual_plan_report};
use bunko_remote::{
    BErr, Backend, CaseMergePolicy, CloudHooks, MergeChoice, Pid, RemoteStore, RemoteStoreConfig,
    SubmissionResult,
};
use bunko_store::{LibraryStore, MountGuard};
use parking_lot::Mutex;
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const WAIT: Duration = Duration::from_secs(30);
const QUIET: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Hooks;

impl CloudHooks for Hooks {
    fn moved(&self, _from: &Path, _to: &Path, _is_dir: bool) {}
    fn removed(&self, _path: &Path, _is_dir: bool) {}
    fn sidecar_overwritten(&self, _path: &Path) {}
    fn series_submission(&self, _dir: &Path, _bytes: &[u8]) -> SubmissionResult {
        SubmissionResult::Applied
    }
    fn ocr_discarded(&self, _path: &Path) {}
}

struct Policy(Mutex<MergeChoice>);

impl CaseMergePolicy for Policy {
    fn prefer_main(&self, _a: &Path, _b: &Path) -> MergeChoice {
        *self.0.lock()
    }
}

struct T {
    _tmp: TempDir,
    fake: Arc<FakeBackend>,
    store: Arc<RemoteStore<FakeBackend>>,
    _mount: MountGuard,
    policy: Arc<Policy>,
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl T {
    /// A store over `fake`, its reader folder holding `files` (oldest first).
    fn new(fake: FakeBackend, files: &[(&str, &[u8])]) -> T {
        fake.reader_mkdir(FOLDER);
        for (rel, bytes) in files {
            if rel.ends_with('/') {
                fake.reader_mkdir(&format!("{FOLDER}/{}", rel.trim_end_matches('/')));
            } else {
                fake.reader_put(&format!("{FOLDER}/{rel}"), bytes);
            }
        }
        T::open(fake)
    }

    fn open(fake: FakeBackend) -> T {
        let tmp = TempDir::new().unwrap();
        let root: PathBuf = tmp.path().join("cloud/s/library");
        let mut cfg = RemoteStoreConfig::new(&root, tmp.path().join("cloud/s"), FOLDER);
        cfg.poll_active = Duration::from_secs(3600);
        cfg.poll_idle = Duration::from_secs(3600);
        cfg.lease = false;
        cfg.retry_base = Duration::from_millis(20);
        cfg.mount_id = 3;
        cfg.quiet = QUIET;
        cfg.merge_interval = Duration::from_millis(50);
        let fake = Arc::new(fake);
        let store = RemoteStore::open_shared(cfg, fake.clone()).unwrap();
        let open = Arc::new(AtomicBool::new(true));
        store.set_lease_gate(Arc::new(move || open.load(Ordering::Acquire)));
        store.set_hooks(Arc::new(Hooks));
        let mount = bunko_store::mount(store.clone() as Arc<dyn LibraryStore>);
        let t = T {
            _tmp: tmp,
            fake,
            store,
            _mount: mount,
            policy: Arc::new(Policy(Mutex::new(MergeChoice::Block))),
        };
        assert!(t.store.wait_steady(WAIT), "{:?}", t.store.engine_state());
        wait_until("adoption", || t.store.adopted());
        t
    }

    fn merge(&self) {
        self.store.set_case_merge_policy(self.policy.clone());
    }

    fn done(&self) -> usize {
        self.store
            .recent_merges()
            .iter()
            .filter(|m| m.state == "done")
            .count()
    }

    fn names(&self, rel: &str) -> Vec<String> {
        let Some(dir) = self.fake.node_at(&format!("{FOLDER}/{rel}")) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(self.fake.list_dir(&dir.pid, &mut |n| {
                out.push(String::from_utf8_lossy(&n.name_raw).into_owned())
            }))
            .unwrap();
        out.sort();
        out
    }
}

/// Wait for `more` beyond `since`, then return the new report.
fn compared(since: DualPlanReport, more: impl Fn(&DualPlanReport) -> bool) -> DualPlanReport {
    let mut now = dual_plan_report();
    wait_until("a plan compared both ways", || {
        now = dual_plan_report();
        more(&now)
    });
    assert!(now.sets >= since.sets && now.dups >= since.dups);
    now
}

#[test]
fn every_plan_shape_is_planned_both_ways_and_alike() {
    // A folder set with every volume-group rule: an identical archive, differing
    // mains (the loser's kept: an in-place replace), a colliding unstamped layer, a
    // differing cover, a loser-only volume and folder, a series.json submission.
    let r0 = dual_plan_report();
    let t = T::new(
        FakeBackend::drive_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"one"),
            ("Kingdom/Vol 1.mokuro", b"{\"w\":1}"),
            ("Kingdom/Vol 1.edit.mokuro", b"{\"e\":1}"),
            ("Kingdom/Vol 1.webp", b"c1"),
            ("Kingdom/series.json", b"{\"version\":2}"),
            ("kingdom/Vol 1.cbz", b"one"),
            ("kingdom/Vol 1.mokuro", b"{\"l\":1}"),
            ("kingdom/Vol 1.edit.mokuro", b"{\"e\":2}"),
            ("kingdom/Vol 1.webp", b"c2"),
            ("kingdom/Vol 2.cbz", b"two"),
            ("kingdom/Extras/x.cbz", b"x"),
            ("kingdom/series.json", b"{\"version\":2,\"titles\":[\"K\"]}"),
        ],
    );
    *t.policy.0.lock() = MergeChoice::B;
    t.merge();
    wait_until("the folder merge", || t.done() >= 1);
    assert_eq!(
        t.names("Kingdom"),
        [
            "Extras",
            "Vol 1.cbz",
            "Vol 1.edit-2.mokuro",
            "Vol 1.edit.mokuro",
            "Vol 1.mokuro",
            "Vol 1.webp",
            "Vol 2.cbz",
            "series.json"
        ]
    );
    let r1 = compared(r0, |r| r.sets > r0.sets);
    assert!(
        r1.orders > r0.orders && r1.specs > r0.specs,
        "{r0:?} {r1:?}"
    );
    drop(t);

    // A same-folder stem set on a case-sensitive host: renames into the archive's
    // stem, the identical copy trashed.
    let t = T::new(
        FakeBackend::webdav_plain(),
        &[
            ("K/Vol 1.cbz", b"a"),
            ("K/vol 1.cbz", b"a"),
            ("K/vol 1.mokuro", b"{}"),
        ],
    );
    t.merge();
    wait_until("the stem merge", || t.done() >= 1);
    assert_eq!(t.names("K"), ["Vol 1.cbz", "Vol 1.mokuro"]);
    let r2 = compared(r1, |r| r.sets > r1.sets);
    drop(t);

    // Three spellings, one volume group blocked (differing archives): blocks only.
    let t = T::new(
        FakeBackend::mega_like(),
        &[
            ("Kingdom/Vol 1.cbz", b"a"),
            ("kingdom/Vol 1.cbz", b"different"),
            ("KINGDOM/Vol 2.cbz", b"two"),
        ],
    );
    t.merge();
    wait_until("the three-way merge", || t.done() >= 1);
    assert!(t.names("Kingdom").contains(&"Vol 2.cbz".to_string()));
    assert_eq!(t.names("kingdom"), ["Vol 1.cbz"], "blocked, so kept");
    let r3 = compared(r2, |r| r.sets > r2.sets);
    drop(t);

    // A plan deferred mid-way (a transfer quota on a spooled read) compares too.
    let big = |b: u8| vec![b; (4 << 20) + 10];
    let t = T::new(
        FakeBackend::mega_like(),
        &[
            ("Kingdom/Vol 1.cbz", &big(1)),
            ("kingdom/Vol 1.cbz", &big(1)),
        ],
    );
    t.fake.fail_next(
        FakeOp::Read,
        BErr::TransferQuota(Some(Duration::from_millis(400))),
    );
    t.merge();
    let r4 = compared(r3, |r| r.sets > r3.sets);
    assert!(t.store.recent_merges().is_empty(), "deferred");
    drop(t);

    // A duplicate set (two top-level roots, one `Series` each): fingerprinted both
    // ways.
    let f = FakeBackend::drive_like();
    let roots: Vec<Pid> = (0..2)
        .map(|_| f.reader_mkdir_in(&f.account_root(), FOLDER).pid)
        .collect();
    for (i, r) in roots.iter().enumerate() {
        let s = f.reader_mkdir_in(r, "Series").pid;
        f.reader_put_in(&s, &format!("v{i}.cbz"), b"v");
    }
    let t = T::open(f);
    t.merge();
    let r5 = compared(r4, |r| r.dups > r4.dups);
    assert!(r5.dups > r0.dups);
}
