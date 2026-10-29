//! The duplicate-folder merge (D5 rev., spec §5.9 "Duplicates"): Bunko runs Mokuro
//! Reader's merge (#308) on its merge journal, on providers that allow two folders of
//! one name (Drive, MEGA). FakeBackend profiles, real temp dirs, real time, a store
//! driven from std threads (as tests/case_merge.rs).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::merge::DEDUP_RECREATE_LIMIT;
use bunko_remote::{
    Backend, CaseMergePolicy, CloudHooks, EngineState, MergeChoice, Pid, RemoteStore,
    RemoteStoreConfig, SubmissionResult,
};
use bunko_store::{Health, LibraryStore, MountGuard, WatchEvent, fs};
use parking_lot::Mutex;
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const WAIT: Duration = Duration::from_secs(30);
const QUIET: Duration = Duration::from_millis(300);

type Store = RemoteStore<FakeBackend>;

// ---- the server's side, recorded --------------------------------------------------

#[derive(Default)]
struct Rec {
    merged: Mutex<Vec<(String, String)>>,
    removed: Mutex<Vec<String>>,
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
    fn moved(&self, _from: &Path, _to: &Path, _is_dir: bool) {}
    fn removed(&self, path: &Path, _is_dir: bool) {
        self.removed.lock().push(self.rel(path));
    }
    fn sidecar_overwritten(&self, _path: &Path) {}
    fn series_submission(&self, dir: &Path, bytes: &[u8]) -> SubmissionResult {
        self.submissions
            .lock()
            .push((self.rel(dir), bytes.to_vec()));
        SubmissionResult::Applied
    }
    fn ocr_discarded(&self, _path: &Path) {}
    fn merged(&self, from: &Path, to: &Path, _is_dir: bool) {
        self.merged.lock().push((self.rel(from), self.rel(to)));
    }
}

#[derive(Default)]
struct Policy {
    busy: Mutex<Vec<PathBuf>>,
}

impl CaseMergePolicy for Policy {
    fn prefer_main(&self, _a: &Path, _b: &Path) -> MergeChoice {
        MergeChoice::Block
    }
    fn busy(&self, path: &Path) -> bool {
        self.busy
            .lock()
            .iter()
            .any(|b| b == path || b.starts_with(path))
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
    tune: fn(&mut RemoteStoreConfig),
    events: Arc<Mutex<Vec<WatchEvent>>>,
    /// Every engine state seen since the start (sampled).
    states: Arc<Mutex<Vec<EngineState>>>,
    sampling: Arc<AtomicBool>,
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
    /// A store over `fake` (already holding its roots).
    fn new(fake: FakeBackend) -> T {
        T::with(fake, no_tune)
    }

    fn with(fake: FakeBackend, tune: fn(&mut RemoteStoreConfig)) -> T {
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
            tune,
            events: Arc::default(),
            states: Arc::default(),
            sampling: Arc::new(AtomicBool::new(true)),
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
        store.set_hooks(self.hooks.clone());
        let ev = self.events.clone();
        std::mem::forget(
            store
                .watch(Arc::new(move |e: WatchEvent| ev.lock().push(e)))
                .unwrap(),
        );
        let (s2, states, on) = (
            Arc::downgrade(&store),
            self.states.clone(),
            self.sampling.clone(),
        );
        std::thread::spawn(move || {
            while on.load(Ordering::Acquire) {
                let Some(s) = s2.upgrade() else { return };
                let st = s.engine_state();
                drop(s);
                {
                    let mut v = states.lock();
                    if v.last() != Some(&st) {
                        v.push(st);
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
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

    /// `(name, pid, is_dir)` of the live children of a cloud folder, sorted.
    fn kids(&self, pid: &str) -> Vec<(String, Pid, bool)> {
        let mut out = Vec::new();
        rt().block_on(self.fake.list_dir(pid, &mut |n| {
            if !n.trashed {
                out.push((
                    String::from_utf8_lossy(&n.name_raw).into_owned(),
                    n.pid,
                    n.is_dir,
                ))
            }
        }))
        .unwrap();
        out.sort();
        out
    }

    fn names(&self, pid: &str) -> Vec<String> {
        self.kids(pid).into_iter().map(|(n, _, _)| n).collect()
    }

    /// The pid of the one live child folder `name` of `pid` (panics on none or two).
    fn dir(&self, pid: &str, name: &str) -> Pid {
        let v: Vec<Pid> = self
            .kids(pid)
            .into_iter()
            .filter(|(n, _, d)| n == name && *d)
            .map(|(_, p, _)| p)
            .collect();
        assert_eq!(v.len(), 1, "one folder {name} in {pid}: {v:?}");
        v[0].clone()
    }

    /// The live `mokuro-reader` folders directly in the account's top folder.
    fn top_roots(&self) -> Vec<Pid> {
        self.kids(&self.fake.account_root())
            .into_iter()
            .filter(|(n, _, d)| n == FOLDER && *d)
            .map(|(_, p, _)| p)
            .collect()
    }

    fn bytes_in(&self, pid: &str, name: &str) -> Vec<Vec<u8>> {
        self.kids(pid)
            .into_iter()
            .filter(|(n, _, d)| n == name && !*d)
            .map(|(_, p, _)| self.fake.content(&p).unwrap())
            .collect()
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

    /// One more feed cycle, delivered.
    fn sync_feed(&self) {
        let n = self.fake.calls(FakeOp::Changes);
        self.s().feed().poll_now();
        wait_until("a feed poll", || self.fake.calls(FakeOp::Changes) > n);
        std::thread::sleep(Duration::from_millis(100));
    }

    fn never_suspect(&self) {
        let s = self.states.lock();
        assert!(
            !s.iter().any(|s| matches!(s, EngineState::Suspect { .. })),
            "{s:?}"
        );
        assert_eq!(self.s().health(), Health::Online);
    }

    fn removed_events(&self) -> Vec<WatchEvent> {
        self.events
            .lock()
            .iter()
            .filter(|e| matches!(e, WatchEvent::Removed { .. }))
            .cloned()
            .collect()
    }
}

impl Drop for T {
    fn drop(&mut self) {
        self.sampling.store(false, Ordering::Release);
    }
}

/// A new top-level `mokuro-reader` folder (the next one is younger).
fn top_root(f: &FakeBackend) -> Pid {
    f.reader_mkdir_in(&f.account_root(), FOLDER).pid
}

fn mkdir(f: &FakeBackend, parent: &str, name: &str) -> Pid {
    f.reader_mkdir_in(parent, name).pid
}

fn put(f: &FakeBackend, parent: &str, name: &str, bytes: &[u8]) -> Pid {
    f.reader_put_in(parent, name, bytes).pid
}

type Profile = (&'static str, fn() -> FakeBackend);

fn dup_profiles() -> Vec<Profile> {
    vec![
        ("drive_like", FakeBackend::drive_like as fn() -> FakeBackend),
        ("mega_like", FakeBackend::mega_like),
    ]
}

// ---- convergence --------------------------------------------------------------------

#[test]
fn five_duplicate_roots_merge_into_the_canonical_top_level_root() {
    for (name, make) in dup_profiles() {
        let f = make();
        // An older nested root first: never merged (another parent), still read.
        let backup = mkdir(&f, &f.account_root(), "Backup");
        let nested = mkdir(&f, &backup, FOLDER);
        let n_dir = mkdir(&f, &nested, "Nested");
        put(&f, &n_dir, "n.cbz", b"n");
        let roots: Vec<Pid> = (0..5).map(|_| top_root(&f)).collect();
        for (i, r) in roots.iter().enumerate() {
            let s = mkdir(&f, r, "Series");
            put(&f, &s, &format!("v{i}.cbz"), format!("v{i}").as_bytes());
            let own = mkdir(&f, r, &format!("Only {i}"));
            put(&f, &own, "o.cbz", b"o");
        }
        let t = T::new(f);
        t.steady();
        assert!(!t.s().merge_busy(&t.abs("Series")));
        t.policy();
        wait_until(&format!("{name}: one root, one Series"), || {
            t.top_roots().len() == 1
                && t.kids(&roots[0])
                    .iter()
                    .filter(|(n, _, _)| n == "Series")
                    .count()
                    == 1
                && t.names(&t.dir(&roots[0], "Series")).len() == 5
        });
        assert_eq!(t.top_roots(), [roots[0].clone()], "{name}");
        let series = t.dir(&roots[0], "Series");
        assert_eq!(
            t.names(&series),
            ["v0.cbz", "v1.cbz", "v2.cbz", "v3.cbz", "v4.cbz"],
            "{name}"
        );
        for i in 0..5 {
            assert!(
                t.names(&roots[0]).contains(&format!("Only {i}")),
                "{name}: Only {i}"
            );
        }
        // Emptied roots and folders go to the trash, never a permanent delete.
        let trashed = t.fake.trashed_log();
        for r in &roots[1..] {
            assert!(trashed.contains(r), "{name}: {r} trashed");
        }
        assert!(t.fake.deleted_log().is_empty(), "{name}");
        // The nested root is untouched, and still read.
        assert_eq!(t.names(&nested), ["Nested"], "{name}");
        assert!(fs::is_file(t.abs("Nested/n.cbz")), "{name}");
        for i in 0..5 {
            assert!(fs::is_file(t.abs(&format!("Series/v{i}.cbz"))), "{name}");
        }
        t.sync_feed();
        t.never_suspect();
        let merges = t.s().recent_merges();
        assert!(!merges.is_empty());
        assert!(merges.iter().all(|m| m.origin == "dedup"), "{merges:?}");
    }
}

#[test]
fn series_folders_merge_level_by_level() {
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let b = top_root(&f);
    let sa = mkdir(&f, &a, "S");
    put(&f, &sa, "a.cbz", b"a");
    let suba = mkdir(&f, &sa, "extras");
    put(&f, &suba, "x.jpg", b"x");
    let sb = mkdir(&f, &b, "S");
    put(&f, &sb, "b.cbz", b"b");
    let subb = mkdir(&f, &sb, "extras");
    put(&f, &subb, "y.jpg", b"y");
    let t = T::new(f);
    t.steady();
    t.policy();
    wait_until("one S, one extras", || {
        t.top_roots().len() == 1
            && t.kids(&a).len() == 1
            && t.kids(&sa).len() == 3
            && t.names(&suba) == ["x.jpg", "y.jpg"]
    });
    assert_eq!(t.names(&sa), ["a.cbz", "b.cbz", "extras"]);
    // Root first, then S, then S/extras: one group per level.
    let mut merges = t.s().recent_merges();
    merges.reverse();
    let winners: Vec<&str> = merges.iter().map(|m| m.winner.as_str()).collect();
    assert_eq!(winners, ["", "S", "S/extras"], "{merges:?}");
    assert!(t.fake.trashed_log().contains(&sb));
    assert!(t.fake.trashed_log().contains(&subb));
    t.sync_feed();
    t.never_suspect();
}

#[test]
fn nfc_and_nfd_spellings_are_not_duplicates() {
    // The reader groups by the exact name, so it never merges these: neither does Bunko.
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let nfd = "\u{30AB}\u{3099}";
    let c = mkdir(&f, &a, "ガ");
    put(&f, &c, "a.cbz", b"a");
    let d = mkdir(&f, &a, nfd);
    put(&f, &d, "b.cbz", b"b");
    let t = T::new(f);
    t.steady();
    t.policy();
    std::thread::sleep(QUIET * 4);
    assert!(t.s().recent_merges().is_empty());
    assert_eq!(t.names(&c), ["a.cbz"]);
    assert_eq!(t.names(&d), ["b.cbz"]);
    // Still one library folder, the union.
    let mut names: Vec<_> = fs::read_dir(t.abs("ガ"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["a.cbz", "b.cbz"]);
}

#[test]
fn a_same_named_file_keeps_the_newer_write_and_a_tie_keeps_the_canonical_copy() {
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let b = top_root(&f);
    let sa = mkdir(&f, &a, "S");
    let sb = mkdir(&f, &b, "S");
    let old = put(&f, &sa, "v.cbz", b"old");
    let new = put(&f, &sb, "v.cbz", b"new!");
    // A tie: equal write times keep the canonical (older folder's) copy.
    let keep = put(&f, &sa, "t.cbz", b"canon");
    let lose = put(&f, &sb, "t.cbz", b"other");
    let at = f.now_ms();
    f.set_times(&keep, at - 10_000, at);
    f.set_times(&lose, at - 5_000, at);
    let t = T::new(f);
    t.steady();
    t.policy();
    wait_until("merged", || {
        t.top_roots().len() == 1 && t.kids(&a).len() == 1 && t.kids(&sa).len() == 2
    });
    assert_eq!(t.bytes_in(&sa, "v.cbz"), [b"new!".to_vec()]);
    assert_eq!(t.bytes_in(&sa, "t.cbz"), [b"canon".to_vec()]);
    let trashed = t.fake.trashed_log();
    assert!(trashed.contains(&old) && trashed.contains(&lose));
    assert!(!trashed.contains(&new) && !trashed.contains(&keep));
    assert_eq!(fs::read(t.abs("S/v.cbz")).unwrap(), b"new!");
    assert_eq!(fs::read(t.abs("S/t.cbz")).unwrap(), b"canon");
    t.sync_feed();
    t.never_suspect();
}

#[test]
fn an_unknown_write_time_keeps_the_canonical_copy_the_index_already_shows() {
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let b = top_root(&f);
    let sa = mkdir(&f, &a, "S");
    let sb = mkdir(&f, &b, "S");
    let keep = put(&f, &sa, "u.cbz", b"canon");
    let lose = put(&f, &sb, "u.cbz", b"known");
    let at = f.now_ms();
    f.set_times(&keep, at - 10_000, 0);
    f.set_times(&lose, at - 5_000, at);
    let t = T::new(f);
    t.steady();
    // Before any merge, the path serves the copy the merge will keep.
    assert_eq!(fs::read(t.abs("S/u.cbz")).unwrap(), b"canon");
    t.policy();
    wait_until("merged", || {
        t.top_roots().len() == 1 && t.kids(&a).len() == 1 && t.kids(&sa).len() == 1
    });
    assert_eq!(t.bytes_in(&sa, "u.cbz"), [b"canon".to_vec()]);
    let trashed = t.fake.trashed_log();
    assert!(trashed.contains(&lose) && !trashed.contains(&keep));
    assert_eq!(fs::read(t.abs("S/u.cbz")).unwrap(), b"canon");
    t.sync_feed();
    t.never_suspect();
}

/// Accepted limit (spec §5.9 "Duplicates"): an OCR file is a file here like any other.
/// An EDITED main or an `edit` layer that loses to a newer write goes to the provider
/// trash, where the case merge would keep it (`pre-upgrade`, `edit-2`): the reader's
/// merge (#308) does the same, and the cloud must end the same whoever merges.
#[test]
fn an_edited_main_and_an_edit_layer_keep_the_newer_write_as_the_reader_does() {
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let b = top_root(&f);
    let sa = mkdir(&f, &a, "S");
    let sb = mkdir(&f, &b, "S");
    put(&f, &sa, "v.cbz", b"v");
    let edited = put(
        &f,
        &sa,
        "v.mokuro",
        br#"{"volume_uuid":"u","edited":{"by":"reader"},"pages":[]}"#,
    );
    let old_layer = put(&f, &sa, "v.edit.mokuro", br#"{"pages":["mine"]}"#);
    let generated: &[u8] = br#"{"volume_uuid":"u","ocr_engine":{"id":"mokuro"},"pages":[]}"#;
    put(&f, &sb, "v.mokuro", generated);
    put(&f, &sb, "v.edit.mokuro", br#"{"pages":["theirs"]}"#);
    let t = T::new(f);
    t.steady();
    t.policy();
    wait_until("merged", || {
        t.top_roots().len() == 1 && t.kids(&a).len() == 1 && t.kids(&sa).len() == 3
    });
    assert_eq!(t.bytes_in(&sa, "v.mokuro"), [generated.to_vec()]);
    assert_eq!(
        t.bytes_in(&sa, "v.edit.mokuro"),
        [br#"{"pages":["theirs"]}"#.to_vec()]
    );
    let names: Vec<String> = t.kids(&sa).into_iter().map(|(n, _, _)| n).collect();
    assert!(
        !names
            .iter()
            .any(|n| n.contains("pre-upgrade") || n.contains("edit-2")),
        "no keepsake layer: {names:?}"
    );
    let trashed = t.fake.trashed_log();
    assert!(trashed.contains(&edited) && trashed.contains(&old_layer));
    t.sync_feed();
    t.never_suspect();
}

#[test]
fn keep_both_files_all_move_into_the_canonical_root() {
    const KEEP: [&str; 5] = [
        "volume-data.json",
        "goals.json",
        "positions.json",
        ".mokuro-bunko.json",
        ".mokuro-queue.json",
    ];
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let b = top_root(&f);
    for (i, r) in [&a, &b].into_iter().enumerate() {
        for k in KEEP {
            put(&f, r, k, format!("{{\"copy\":{i}}}").as_bytes());
        }
    }
    let t = T::new(f);
    t.steady();
    t.policy();
    wait_until("one root", || t.top_roots().len() == 1);
    for k in KEEP {
        assert_eq!(t.bytes_in(&a, k).len(), 2, "{k}: both copies kept");
    }
    // Only the emptied root went to the trash.
    assert_eq!(t.fake.trashed_log(), std::slice::from_ref(&b));
    let dup = t
        .s()
        .conflict_rows()
        .into_iter()
        .filter(|c| c.kind == "dup-file")
        .count();
    assert_eq!(dup, 0, "no dup-file conflict from a merge that expects one");
    t.sync_feed();
    t.never_suspect();
}

#[test]
fn the_older_series_json_is_submitted_then_trashed() {
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let b = top_root(&f);
    let sa = mkdir(&f, &a, "S");
    put(&f, &sa, "v.cbz", b"v");
    let old = put(
        &f,
        &sa,
        "series.json",
        br#"{"version":2,"series_title":"S","old":1}"#,
    );
    let sb = mkdir(&f, &b, "S");
    put(
        &f,
        &sb,
        "series.json",
        br#"{"version":2,"series_title":"S","new":1}"#,
    );
    let t = T::new(f);
    t.steady();
    t.hooks.submissions.lock().clear();
    t.policy();
    wait_until("merged", || {
        t.top_roots().len() == 1 && t.kids(&a).len() == 1 && t.kids(&sa).len() == 2
    });
    assert_eq!(
        t.bytes_in(&sa, "series.json"),
        [br#"{"version":2,"series_title":"S","new":1}"#.to_vec()]
    );
    let subs = t.hooks.submissions.lock().clone();
    assert!(
        subs.iter()
            .any(|(d, b)| d == "S" && b == br#"{"version":2,"series_title":"S","old":1}"#),
        "{subs:?}"
    );
    assert!(t.fake.trashed_log().contains(&old));
}

#[test]
fn a_merge_runs_no_hooks_and_announces_no_removal_of_a_surviving_path() {
    let f = FakeBackend::mega_like();
    let a = top_root(&f);
    let b = top_root(&f);
    let sa = mkdir(&f, &a, "S");
    put(&f, &sa, "v1.cbz", b"1");
    put(&f, &sa, "v2.cbz", b"old");
    let sb = mkdir(&f, &b, "S");
    put(&f, &sb, "v2.cbz", b"new");
    put(&f, &sb, "v3.cbz", b"3");
    let tb = mkdir(&f, &b, "T");
    put(&f, &tb, "t.cbz", b"t");
    let t = T::new(f);
    t.steady();
    t.events.lock().clear();
    t.policy();
    wait_until("merged", || {
        t.top_roots().len() == 1 && t.kids(&a).len() == 2 && t.kids(&sa).len() == 3
    });
    t.sync_feed();
    t.sync_feed();
    assert_eq!(t.removed_events(), [], "every path survives");
    assert!(t.hooks.merged.lock().is_empty());
    assert!(t.hooks.removed.lock().is_empty());
    for p in ["S/v1.cbz", "S/v2.cbz", "S/v3.cbz", "T/t.cbz"] {
        assert!(fs::is_file(t.abs(p)), "{p}");
    }
    assert_eq!(fs::read(t.abs("S/v2.cbz")).unwrap(), b"new");
    let merges = t.s().recent_merges();
    assert!(merges.iter().all(|m| m.origin == "dedup"), "{merges:?}");
    t.never_suspect();
}

// ---- when it does not run -----------------------------------------------------------

fn two_roots(f: &FakeBackend) -> (Pid, Pid) {
    let a = top_root(f);
    let b = top_root(f);
    let s = mkdir(f, &b, "S");
    put(f, &s, "v.cbz", b"v");
    (a, b)
}

#[test]
fn no_merge_with_dedup_off() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.dedup = false;
    }
    let f = FakeBackend::drive_like();
    let (a, _) = two_roots(&f);
    let t = T::with(f, tune);
    t.steady();
    t.policy();
    std::thread::sleep(QUIET * 4);
    assert_eq!(t.top_roots().len(), 2);
    assert!(t.names(&a).is_empty());
    assert!(t.s().recent_merges().is_empty());
}

#[test]
fn no_merge_before_quiet() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.quiet = Duration::from_secs(5);
    }
    let f = FakeBackend::drive_like();
    two_roots(&f);
    let t = T::with(f, tune);
    t.steady();
    t.policy();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(t.top_roots().len(), 2, "not yet quiet");
    t.wait_merges(1);
    assert_eq!(t.top_roots().len(), 1);
}

#[test]
fn no_merge_in_standby() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.lease = true;
    }
    let f = FakeBackend::drive_like();
    let (a, _) = two_roots(&f);
    let marker = serde_json::json!({
        "version": 1, "kind": "mokuro-bunko", "instance_id": "other-install",
        "boot_id": "b", "instance_name": "nas", "epoch": 5,
        "claimed_at": "2026-10-12T10:00:00Z", "ttl_s": 900, "state": "active"
    });
    put(
        &f,
        &a,
        ".mokuro-bunko.json",
        &serde_json::to_vec(&marker).unwrap(),
    );
    let t = T::with(f, tune);
    t.steady();
    wait_until("standby", || {
        t.s().health() == Health::LeaseHeld { by: "nas".into() }
    });
    t.policy();
    std::thread::sleep(QUIET * 4);
    assert_eq!(t.done_merges(), 0);
    assert_eq!(t.top_roots().len(), 2);
    assert_eq!(t.fake.calls(FakeOp::Update), 0);
}

#[test]
fn no_merge_while_suspect() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.suspect_min_removals = 3;
    }
    let f = FakeBackend::drive_like();
    let (a, _) = two_roots(&f);
    let other = mkdir(&f, &a, "Other");
    let gone: Vec<Pid> = (0..10)
        .map(|i| put(&f, &other, &format!("f{i}.jpg"), b"x"))
        .collect();
    let t = T::with(f, tune);
    t.steady();
    for p in &gone {
        t.fake.reader_trash_pid(p);
    }
    t.sync_feed();
    wait_until("suspect", || t.s().health() == Health::Suspect);
    t.policy();
    std::thread::sleep(QUIET * 4);
    assert_eq!(t.done_merges(), 0, "no merge while Suspect");
    assert_eq!(t.top_roots().len(), 2);
    t.s().feed().confirm_suspect();
    t.sync_feed();
    t.wait_merges(1);
    wait_until("one root", || t.top_roots().len() == 1);
}

#[test]
fn no_merge_while_an_ocr_claim_runs_in_a_moving_folder() {
    let f = FakeBackend::drive_like();
    two_roots(&f);
    let t = T::new(f);
    t.steady();
    t.policy.busy.lock().push(t.abs("S/v.cbz"));
    t.policy();
    std::thread::sleep(QUIET * 4);
    assert_eq!(t.done_merges(), 0);
    t.policy.busy.lock().clear();
    t.wait_merges(1);
    assert_eq!(t.top_roots().len(), 1);
}

#[test]
fn providers_without_duplicate_names_never_dedup() {
    // Real duplicates, on a provider whose caps say it has none: the caps gate alone
    // keeps the merge off.
    for (name, make) in dup_profiles() {
        let f = make().without_duplicate_names();
        let (a, _) = two_roots(&f);
        let t = T::new(f);
        t.steady();
        t.policy();
        std::thread::sleep(QUIET * 4);
        assert!(t.s().recent_merges().is_empty(), "{name}");
        assert_eq!(t.fake.calls(FakeOp::Update), 0, "{name}");
        assert_eq!(t.top_roots().len(), 2, "{name}");
        assert!(t.names(&a).is_empty(), "{name}");
        assert!(t.fake.trashed_log().is_empty(), "{name}");
    }
}

// ---- crashes, races, the reader ------------------------------------------------------

#[test]
fn a_crash_at_a_move_or_a_trash_resumes_and_converges() {
    for at in ["MERGE_MOVE", "MERGE_TRASH"] {
        let f = FakeBackend::drive_like();
        let a = top_root(&f);
        let b = top_root(&f);
        let sa = mkdir(&f, &a, "S");
        put(&f, &sa, "v.cbz", b"old");
        let sb = mkdir(&f, &b, "S");
        put(&f, &sb, "v.cbz", b"new");
        put(&f, &sb, "w.cbz", b"w");
        let mut t = T::new(f);
        t.steady();
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = n.clone();
        t.s().set_failpoint(Some(Arc::new(move |p: &str| {
            p == at && n2.fetch_add(1, Ordering::SeqCst) == 0
        })));
        t.policy();
        wait_until(&format!("the crash at {at}"), || t.s().crashed());
        assert!(t.s().recent_merges().iter().any(|m| m.state == "open"));
        t.stop();
        t.start();
        t.steady();
        t.policy();
        wait_until(&format!("{at}: converged"), || {
            t.top_roots().len() == 1 && t.kids(&a).len() == 1 && t.kids(&sa).len() == 2
        });
        assert_eq!(t.bytes_in(&sa, "v.cbz"), [b"new".to_vec()], "{at}");
        assert_eq!(t.bytes_in(&sa, "w.cbz"), [b"w".to_vec()], "{at}");
        wait_until("every group closed", || {
            t.s().recent_merges().iter().all(|m| m.state == "done")
        });
        t.sync_feed();
        t.never_suspect();
    }
}

#[test]
fn a_reader_upload_into_the_losing_member_is_merged_next_pass() {
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let b = top_root(&f);
    let sb = mkdir(&f, &b, "S");
    put(&f, &sb, "v.cbz", b"v");
    let t = T::new(f);
    t.steady();
    let fake = t.fake.clone();
    let once = Arc::new(AtomicBool::new(false));
    let (o2, b2) = (once.clone(), b.clone());
    t.fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Update && !o2.swap(true, Ordering::SeqCst) {
            fake.reader_put_in(&b2, "late.json", b"{}");
        }
    })));
    t.policy();
    t.wait_merges(1);
    // The finish saw late.json: that root stays for the next pass.
    assert_eq!(t.names(&b), ["late.json"]);
    t.fake.set_call_hook(None);
    t.sync_feed();
    t.wait_merges(2);
    wait_until("one root", || t.top_roots().len() == 1);
    assert_eq!(t.names(&a), ["S", "late.json"]);
    t.never_suspect();
}

#[test]
fn the_reader_merging_the_same_duplicates_at_once_converges() {
    for (name, make) in dup_profiles() {
        let f = make();
        let a = top_root(&f);
        let mut others = Vec::new();
        for i in 0..3 {
            let r = top_root(&f);
            let s = mkdir(&f, &r, "S");
            put(&f, &s, &format!("v{i}.cbz"), b"v");
            put(&f, &r, "volume-data.json", b"{}");
            others.push(r);
        }
        let sa = mkdir(&f, &a, "S");
        put(&f, &sa, "a.cbz", b"a");
        let t = T::new(f);
        t.steady();
        let fake = t.fake.clone();
        let once = Arc::new(AtomicBool::new(false));
        let o2 = once.clone();
        // The reader's whole merge runs in the middle of Bunko's first move.
        t.fake.set_call_hook(Some(Arc::new(move |op| {
            if op == FakeOp::Update && !o2.swap(true, Ordering::SeqCst) {
                fake.run_reader_dedup();
            }
        })));
        t.policy();
        wait_until(&format!("{name}: converged"), || {
            once.load(Ordering::SeqCst)
                && t.top_roots().len() == 1
                && t.kids(&a).iter().filter(|(n, _, _)| n == "S").count() == 1
        });
        t.fake.set_call_hook(None);
        let s = t.dir(&a, "S");
        assert_eq!(
            t.names(&s),
            ["a.cbz", "v0.cbz", "v1.cbz", "v2.cbz"],
            "{name}"
        );
        assert_eq!(t.bytes_in(&a, "volume-data.json").len(), 3, "{name}");
        t.sync_feed();
        t.sync_feed();
        t.never_suspect();
        assert!(
            t.s().conflict_rows().is_empty(),
            "{name}: {:?}",
            t.s().conflict_rows()
        );
        for p in ["S/a.cbz", "S/v0.cbz", "S/v1.cbz", "S/v2.cbz"] {
            assert!(fs::is_file(t.abs(p)), "{name}: {p}");
        }
    }
}

#[test]
fn duplicates_recreated_past_the_limit_stop_being_merged() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.quiet = Duration::from_millis(100);
    }
    let f = FakeBackend::drive_like();
    let a = top_root(&f);
    let sa = mkdir(&f, &a, "S");
    put(&f, &sa, "v.cbz", b"v");
    let t = T::with(f, tune);
    t.steady();
    t.policy();
    let limit = DEDUP_RECREATE_LIMIT as usize;
    for i in 1..=limit + 1 {
        // The reader keys folders by exact name and parent id: it makes `S` again.
        let s = mkdir(&t.fake, &a, "S");
        put(&t.fake, &s, &format!("r{i}.jpg"), b"r");
        t.sync_feed();
        if i <= limit {
            t.wait_merges(i);
        }
    }
    wait_until("the guard", || {
        t.s()
            .conflict_rows()
            .iter()
            .any(|c| c.kind == "dup-recreated")
    });
    let c = t
        .s()
        .conflict_rows()
        .into_iter()
        .find(|c| c.kind == "dup-recreated")
        .unwrap();
    assert_eq!(c.path, "S");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(t.done_merges(), limit);
    assert_eq!(
        t.kids(&a).iter().filter(|(n, _, _)| n == "S").count(),
        2,
        "the last copy stays"
    );
}

// ---- the marker ---------------------------------------------------------------------

fn quick_lease(c: &mut RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_millis(100));
}

fn no_dedup_lease(c: &mut RemoteStoreConfig) {
    quick_lease(c);
    c.dedup = false;
}

fn marker_dedup(t: &T) -> Option<bool> {
    let n = t.fake.node_at(&format!("{FOLDER}/.mokuro-bunko.json"))?;
    let v: serde_json::Value = serde_json::from_slice(&t.fake.content(&n.pid)?).ok()?;
    v.get("dedup").and_then(serde_json::Value::as_bool)
}

#[test]
fn the_marker_says_whether_bunko_merges_duplicates() {
    type Case = (
        &'static str,
        fn() -> FakeBackend,
        fn(&mut RemoteStoreConfig),
        bool,
    );
    let cases: [Case; 5] = [
        ("drive_like", FakeBackend::drive_like, quick_lease, true),
        ("mega_like", FakeBackend::mega_like, quick_lease, true),
        (
            "webdav_plain",
            FakeBackend::webdav_plain,
            quick_lease,
            false,
        ),
        (
            "onedrive_like",
            FakeBackend::onedrive_like,
            quick_lease,
            false,
        ),
        (
            "drive_like off",
            FakeBackend::drive_like,
            no_dedup_lease,
            false,
        ),
    ];
    for (name, make, tune, want) in cases {
        let f = make();
        f.reader_mkdir(FOLDER);
        let t = T::with(f, tune);
        t.steady();
        wait_until(&format!("{name}: our marker"), || {
            marker_dedup(&t).is_some()
        });
        assert_eq!(marker_dedup(&t), Some(want), "{name}");
    }
}

#[test]
fn status_file_writes_leave_the_roots_quiet() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.quiet = Duration::from_millis(800);
    }
    let f = FakeBackend::drive_like();
    let (a, _) = two_roots(&f);
    put(&f, &a, ".mokuro-queue.json", b"{}");
    let t = T::with(f, tune);
    t.steady();
    t.policy();
    // The queue file changes more often than quiet_s (Bunko's floor is 60 s on
    // Drive; quiet_s 120 s): the roots still merge.
    let stop = Arc::new(AtomicBool::new(false));
    let (fake, s2, st) = (t.fake.clone(), Arc::downgrade(t.s()), stop.clone());
    let writer = std::thread::spawn(move || {
        let mut i = 0;
        while !st.load(Ordering::Acquire) {
            i += 1;
            fake.reader_put(
                &format!("{FOLDER}/.mokuro-queue.json"),
                format!("{{\"n\":{i}}}").as_bytes(),
            );
            if let Some(s) = s2.upgrade() {
                s.feed().poll_now();
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    });
    let merged = {
        let t0 = Instant::now();
        loop {
            if t.top_roots().len() == 1 {
                break true;
            }
            if t0.elapsed() > Duration::from_secs(8) {
                break false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
    assert!(
        merged,
        "the roots merged while the queue file kept changing"
    );
}
