//! The RemoteStore write path (plan M2 task 5; spec §3.4, §5.7, §5.8, §5.10; D6, D16,
//! D20, D21, D22): FakeBackend, real temp dirs, real time, driven through
//! `bunko_store::fs` from std threads.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::{Backend, CloudHooks, RemoteStore, RemoteStoreConfig, SubmissionResult};
use bunko_store::{
    Durability, Health, LibraryStore, Mode, MountGuard, Origin, WatchEvent, WriteIntent, fs,
};
use parking_lot::Mutex;
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const WAIT: Duration = Duration::from_secs(20);

type Store = RemoteStore<FakeBackend>;

struct T {
    tmp: TempDir,
    root: PathBuf,
    fake: Arc<FakeBackend>,
    store: Option<Arc<Store>>,
    mount: Option<MountGuard>,
    open: Arc<AtomicBool>,
    events: Arc<Mutex<Vec<WatchEvent>>>,
    hooks: Arc<Rec>,
    tune: fn(&mut RemoteStoreConfig),
}

#[derive(Default)]
struct Rec {
    discarded: Mutex<Vec<PathBuf>>,
    submissions: Mutex<Vec<(PathBuf, Vec<u8>)>>,
}

impl CloudHooks for Rec {
    fn moved(&self, _from: &Path, _to: &Path, _is_dir: bool) {}
    fn removed(&self, _path: &Path, _is_dir: bool) {}
    fn sidecar_overwritten(&self, _path: &Path) {}
    fn series_submission(&self, dir: &Path, bytes: &[u8]) -> SubmissionResult {
        self.submissions
            .lock()
            .push((dir.to_path_buf(), bytes.to_vec()));
        SubmissionResult::Applied
    }
    fn ocr_discarded(&self, path: &Path) {
        self.discarded.lock().push(path.to_path_buf());
    }
}

fn no_tune(_: &mut RemoteStoreConfig) {}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl T {
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
            open: Arc::new(AtomicBool::new(true)),
            events: Arc::default(),
            hooks: Arc::default(),
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
        (self.tune)(&mut cfg);
        cfg
    }

    /// Open (or reopen, after [`T::stop`]) the store on the same dir and provider.
    fn start(&mut self) {
        let store = RemoteStore::open_shared(self.cfg(), self.fake.clone()).unwrap();
        let open = self.open.clone();
        store.set_lease_gate(Arc::new(move || open.load(Ordering::Acquire)));
        store.set_hooks(self.hooks.clone());
        let ev = self.events.clone();
        std::mem::forget(
            store
                .watch(Arc::new(move |e: WatchEvent| ev.lock().push(e)))
                .unwrap(),
        );
        self.mount = Some(bunko_store::mount(store.clone() as Arc<dyn LibraryStore>));
        self.store = Some(store);
    }

    fn wait_steady(&self) {
        assert!(self.s().wait_steady(WAIT), "{:?}", self.s().engine_state());
    }

    /// Simulate a crash: the store goes away without any further writes.
    fn stop(&mut self) {
        self.mount = None;
        self.store = None;
    }

    fn s(&self) -> &Arc<Store> {
        self.store.as_ref().unwrap()
    }

    fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for s in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(s);
        }
        p
    }

    fn gate(&self, open: bool) {
        self.open.store(open, Ordering::Release);
    }

    fn idle(&self) {
        assert!(self.s().wait_outbox_idle(WAIT), "{:?}", self.s().outbox());
    }

    /// One more feed cycle, delivered.
    fn sync_feed(&self) {
        let n = self.fake.calls(FakeOp::Changes);
        self.s().feed().poll_now();
        wait_until("a feed poll", || self.fake.calls(FakeOp::Changes) > n);
        std::thread::sleep(Duration::from_millis(100));
    }

    /// A local file outside the library with `bytes`.
    fn local(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.tmp.path().join(format!("local-{name}"));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn publish(&self, rel: &str, bytes: &[u8], w: WriteIntent) -> bunko_store::Published {
        let src = self.local(&rel.replace('/', "_"), bytes);
        fs::publish(&src, self.abs(rel), &w).unwrap()
    }

    /// Every live node named `name` under the folder `dir` (relative to the root).
    fn remote_named(&self, dir: &str, name: &str) -> Vec<bunko_remote::RNode> {
        let parent = self.fake.node_at(&format!("{FOLDER}/{dir}")).unwrap().pid;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(self.fake.find_named(&parent, name)).unwrap().0
    }

    fn remote_bytes(&self, rel: &str) -> Option<Vec<u8>> {
        let n = self.fake.node_at(&format!("{FOLDER}/{rel}"))?;
        self.fake.content(&n.pid)
    }

    fn events(&self) -> Vec<WatchEvent> {
        // Events arrive on the store-events thread.
        std::thread::sleep(Duration::from_millis(50));
        std::mem::take(&mut *self.events.lock())
    }
}

fn deferred(origin: Origin, mode: Mode) -> WriteIntent {
    WriteIntent::new(mode, origin, Durability::Deferred)
}

fn ocr() -> Origin {
    Origin::Ocr { claim: "c1".into() }
}

fn added(p: PathBuf) -> WatchEvent {
    WatchEvent::Added {
        path: p,
        is_dir: false,
    }
}

fn version(m: &bunko_store::Meta) -> String {
    m.version()
        .map(|v| v.as_str().to_string())
        .unwrap_or_default()
}

// ---- Sync vs Deferred (D6, D21) -------------------------------------------------

#[test]
fn a_sync_publish_is_invisible_until_committed() {
    let t = T::new(FakeBackend::webdav_cas(), &[("S/a.mokuro", b"old")]);
    t.wait_steady();
    t.events();
    t.fake.set_latency(Duration::from_millis(300));
    let (src_new, src_rep) = (t.local("n", b"new file"), t.local("r", b"replaced"));
    let (dn, dr) = (t.abs("S/b.mokuro"), t.abs("S/a.mokuro"));
    let w = std::thread::spawn(move || {
        fs::rename(&src_new, &dn).unwrap();
        fs::rename(&src_rep, &dr).unwrap();
    });
    std::thread::sleep(Duration::from_millis(150));
    // In flight: other readers see the old state.
    assert!(!fs::exists(t.abs("S/b.mokuro")));
    assert_eq!(fs::read(t.abs("S/a.mokuro")).unwrap(), b"old");
    w.join().unwrap();
    t.fake.set_latency(Duration::ZERO);
    let m = fs::metadata(t.abs("S/b.mokuro")).unwrap();
    assert!(m.own() && m.len() == 8);
    let node = t.fake.node_at(&format!("{FOLDER}/S/b.mokuro")).unwrap();
    assert_eq!(version(&m), node.cver);
    assert_eq!(fs::read(t.abs("S/a.mokuro")).unwrap(), b"replaced");
    assert!(fs::metadata(t.abs("S/a.mokuro")).unwrap().own());
    assert_eq!(t.remote_bytes("S/a.mokuro").unwrap(), b"replaced");
    // One event each, at COMMITTED; the feed's echo adds none.
    assert_eq!(
        t.events(),
        vec![added(t.abs("S/b.mokuro")), added(t.abs("S/a.mokuro"))]
    );
    t.sync_feed();
    assert_eq!(t.events(), vec![]);
    assert!(t.s().outbox().is_empty() || t.s().outbox().iter().all(|o| o.state == "COMMITTED"));
}

#[test]
fn a_deferred_publish_is_visible_at_once_with_own() {
    let t = T::new(FakeBackend::drive_like(), &[("S/", b"")]);
    t.wait_steady();
    t.events();
    t.gate(false);
    let p = t.publish(
        "S/v.mokuro",
        b"{\"ocr\":1}",
        deferred(ocr(), Mode::CreateNew),
    );
    assert!(p.op.is_some());
    let m = fs::metadata(t.abs("S/v.mokuro")).unwrap();
    assert!(m.own() && m.is_file() && m.len() == 9);
    assert!(version(&m).starts_with("pending:"));
    assert_eq!(fs::read(t.abs("S/v.mokuro")).unwrap(), b"{\"ocr\":1}");
    assert_eq!(
        t.s().content_sha256(&t.abs("S/v.mokuro")).unwrap(),
        p.sha256
    );
    let names: Vec<String> = fs::read_dir(t.abs("S"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["v.mokuro"]);
    assert_eq!(t.fake.calls(FakeOp::Upload), 0);
    assert_eq!(t.events(), vec![added(t.abs("S/v.mokuro"))]);
    assert_eq!(t.s().outbox()[0].state, "HELD");
    t.gate(true);
    t.idle();
    assert_eq!(t.remote_bytes("S/v.mokuro").unwrap(), b"{\"ocr\":1}");
    let m = fs::metadata(t.abs("S/v.mokuro")).unwrap();
    let node = t.fake.node_at(&format!("{FOLDER}/S/v.mokuro")).unwrap();
    assert!(m.own());
    assert_eq!(version(&m), node.cver);
    // D21: the mtime changed at COMMITTED, so the event repeats.
    assert_eq!(t.events(), vec![added(t.abs("S/v.mokuro"))]);
    // The committed bytes are in the blob cache: reading costs nothing.
    t.fake.reset_calls();
    assert_eq!(fs::read(t.abs("S/v.mokuro")).unwrap(), b"{\"ocr\":1}");
    assert_eq!(t.fake.calls(FakeOp::Read), 0);
}

#[test]
fn deferred_publish_survives_outage_and_restart() {
    let mut t = T::new(FakeBackend::drive_like(), &[("S/", b"")]);
    t.wait_steady();
    t.fake.throttle_for(Duration::from_secs(3600));
    t.publish("S/v.mokuro", b"result", deferred(ocr(), Mode::CreateNew));
    wait_until("an attempt", || {
        t.s()
            .outbox()
            .first()
            .is_some_and(|o| o.last_error.is_some())
    });
    assert!(fs::exists(t.abs("S/v.mokuro")));
    t.stop();
    t.start();
    // Still pending (and visible) after the restart, while the outage lasts.
    assert!(fs::metadata(t.abs("S/v.mokuro")).unwrap().own());
    assert_eq!(t.s().outbox().len(), 1);
    t.fake.throttle_for(Duration::ZERO);
    t.idle();
    assert_eq!(t.remote_named("S", "v.mokuro").len(), 1);
    assert_eq!(t.remote_bytes("S/v.mokuro").unwrap(), b"result");
}

// ---- crash points (§5.10) ---------------------------------------------------------

#[test]
fn crash_at_each_state_yields_one_copy() {
    for point in [
        "STAGED",
        "HELD",
        "PREFLIGHT",
        "UPLOADING",
        "uploaded",
        "COMMITTED",
    ] {
        let mut t = T::new(FakeBackend::drive_like(), &[("S/", b"")]);
        t.wait_steady();
        if point == "HELD" {
            t.gate(false);
        }
        let p = point.to_string();
        t.s().set_failpoint(Some(Arc::new(move |at: &str| at == p)));
        let src = t.local("v", b"ocr result");
        let r = fs::publish(&src, t.abs("S/v.mokuro"), &deferred(ocr(), Mode::CreateNew));
        assert_eq!(r.is_err(), point == "STAGED", "{point}");
        wait_until(point, || t.s().crashed());
        t.stop();
        t.gate(true);
        t.start();
        t.wait_steady();
        t.idle();
        let copies = t.remote_named("S", "v.mokuro");
        assert_eq!(copies.len(), 1, "{point}: {copies:?}");
        assert_eq!(t.fake.content(&copies[0].pid).unwrap(), b"ocr result");
        let m = fs::metadata(t.abs("S/v.mokuro")).unwrap();
        assert!(m.own(), "{point}");
        assert_eq!(version(&m), copies[0].cver, "{point}");
    }
}

#[test]
fn an_interrupted_sync_write_is_reconciled_never_duplicated() {
    for point in ["STAGED", "UPLOADING", "uploaded"] {
        let mut t = T::new(FakeBackend::drive_like(), &[("S/", b"")]);
        t.wait_steady();
        let p = point.to_string();
        t.s().set_failpoint(Some(Arc::new(move |at: &str| at == p)));
        let src = t.local("v", b"reader upload");
        assert!(fs::rename(&src, t.abs("S/v.cbz")).is_err());
        t.stop();
        t.start();
        t.wait_steady();
        t.idle();
        let copies = t.remote_named("S", "v.cbz");
        // Before the upload: discarded (the client retries). After: indexed once.
        let want = usize::from(point == "uploaded");
        assert_eq!(copies.len(), want, "{point}");
        assert_eq!(fs::exists(t.abs("S/v.cbz")), want == 1, "{point}");
        assert!(t.s().outbox().is_empty(), "{point}: {:?}", t.s().outbox());
    }
}

// ---- preflight rules ---------------------------------------------------------------

#[test]
fn replay_never_recreates_a_missing_parent() {
    let t = T::new(FakeBackend::drive_like(), &[("S/a.cbz", b"zip")]);
    t.wait_steady();
    t.gate(false);
    t.publish("S/a.mokuro", b"sidecar", deferred(ocr(), Mode::CreateNew));
    t.fake.reader_delete(&format!("{FOLDER}/S"));
    t.sync_feed();
    assert!(!fs::exists(t.abs("S")));
    t.gate(true);
    t.idle();
    assert!(t.fake.node_at(&format!("{FOLDER}/S")).is_none());
    assert_eq!(t.fake.calls(FakeOp::Mkdir), 0);
    assert_eq!(t.fake.calls(FakeOp::Upload), 0);
    assert_eq!(t.s().outbox()[0].state, "CONFLICT");
    assert!(!fs::exists(t.abs("S/a.mokuro")));
}

#[test]
fn coalescing_keeps_only_the_newest_series_json() {
    let t = T::new(FakeBackend::drive_like(), &[("S/", b"")]);
    t.wait_steady();
    t.gate(false);
    for v in ["{\"v\":1}", "{\"v\":2}", "{\"v\":3}"] {
        t.publish(
            "S/series.json",
            v.as_bytes(),
            deferred(Origin::Compiler, Mode::Replace { base: None }),
        );
    }
    assert_eq!(t.s().outbox().len(), 1);
    assert_eq!(fs::read(t.abs("S/series.json")).unwrap(), b"{\"v\":3}");
    t.gate(true);
    t.idle();
    assert_eq!(t.fake.calls(FakeOp::Upload), 1);
    assert_eq!(t.remote_bytes("S/series.json").unwrap(), b"{\"v\":3}");
    // A later compile based on the pending version replaces Bunko's own version.
    let base = version(&fs::metadata(t.abs("S/series.json")).unwrap());
    t.publish(
        "S/series.json",
        b"{\"v\":4}",
        deferred(
            Origin::Compiler,
            Mode::Replace {
                base: Some(bunko_store::Version::new(base)),
            },
        ),
    );
    t.idle();
    assert_eq!(t.remote_named("S", "series.json").len(), 1);
    assert_eq!(t.remote_bytes("S/series.json").unwrap(), b"{\"v\":4}");
}

#[test]
fn d22_metadata_leaves_after_the_sidecars_it_describes() {
    let t = T::new(FakeBackend::drive_like(), &[("S/v.cbz", b"zip")]);
    t.wait_steady();
    t.gate(false);
    let replace = Mode::Replace { base: None };
    t.publish(
        "catalog.json",
        b"{}",
        deferred(Origin::Compiler, replace.clone()),
    );
    t.publish("S/series.json", b"{}", deferred(Origin::Compiler, replace));
    t.publish("S/v.mokuro", b"{}", deferred(ocr(), Mode::CreateNew));
    t.gate(true);
    t.idle();
    assert_eq!(
        t.fake.upload_log(),
        ["v.mokuro", "series.json", "catalog.json"]
    );
}

#[test]
fn ocr_create_new_over_a_foreign_sidecar_is_discarded() {
    let t = T::new(FakeBackend::drive_like(), &[("S/v.cbz", b"zip")]);
    t.wait_steady();
    t.gate(false);
    t.publish("S/v.mokuro", b"ours", deferred(ocr(), Mode::CreateNew));
    // The reader uploads its own sidecar before ours leaves.
    t.fake
        .reader_put(&format!("{FOLDER}/S/v.mokuro"), b"theirs");
    t.gate(true);
    t.idle();
    assert_eq!(*t.hooks.discarded.lock(), vec![t.abs("S/v.mokuro")]);
    assert_eq!(t.fake.calls(FakeOp::Upload), 0);
    let copies = t.remote_named("S", "v.mokuro");
    assert_eq!(copies.len(), 1);
    assert_eq!(t.fake.content(&copies[0].pid).unwrap(), b"theirs");
    // No `_n` orphan anywhere.
    t.sync_feed();
    let names: Vec<String> = fs::read_dir(t.abs("S"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["v.cbz", "v.mokuro"]);
    assert_eq!(fs::read(t.abs("S/v.mokuro")).unwrap(), b"theirs");
    // Known to the index already: discarded at once, nothing queued.
    t.hooks.discarded.lock().clear();
    let p = t.publish(
        "S/v.mokuro",
        b"ours again",
        deferred(ocr(), Mode::CreateNew),
    );
    assert_eq!(p.op, None);
    assert_eq!(*t.hooks.discarded.lock(), vec![t.abs("S/v.mokuro")]);
    assert!(t.s().outbox().iter().all(|o| o.state != "STAGED"));
    assert!(
        t.s()
            .conflicts()
            .iter()
            .all(|(k, p, _)| k == "discarded" && p == "S/v.mokuro")
    );
}

#[test]
fn a_compiler_conflict_on_series_json_becomes_a_submission() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("S/series.json", b"{\"r\":0}")],
    );
    t.wait_steady();
    // Bunko's own version first.
    t.publish(
        "S/series.json",
        b"{\"b\":1}",
        deferred(Origin::Compiler, Mode::Replace { base: None }),
    );
    t.idle();
    let base = version(&fs::metadata(t.abs("S/series.json")).unwrap());
    t.gate(false);
    t.publish(
        "S/series.json",
        b"{\"b\":2}",
        deferred(
            Origin::Compiler,
            Mode::Replace {
                base: Some(bunko_store::Version::new(base)),
            },
        ),
    );
    // The reader rewrites it between our base and our upload.
    t.fake
        .reader_put(&format!("{FOLDER}/S/series.json"), b"{\"reader\":true}");
    t.gate(true);
    t.idle();
    assert_eq!(
        t.remote_bytes("S/series.json").unwrap(),
        b"{\"reader\":true}"
    );
    let subs = t.hooks.submissions.lock().clone();
    assert_eq!(subs, vec![(t.abs("S"), b"{\"reader\":true}".to_vec())]);
    assert_eq!(t.s().submissions().len(), 1);
    assert_eq!(t.s().submissions()[0].2, "applied");
}

// ---- deletes, moves, folders ---------------------------------------------------------

#[test]
fn delete_trashes_foreign_files_and_deletes_own_ones() {
    let t = T::new(
        FakeBackend::drive_like(),
        &[("S/foreign.mokuro", b"f"), ("T/x.webp", b"x")],
    );
    t.wait_steady();
    fs::write(t.abs("S/own.json"), b"mine").unwrap();
    let foreign = t
        .fake
        .node_at(&format!("{FOLDER}/S/foreign.mokuro"))
        .unwrap()
        .pid;
    let own = t.fake.node_at(&format!("{FOLDER}/S/own.json")).unwrap().pid;
    t.events();
    fs::remove_file(t.abs("S/foreign.mokuro")).unwrap();
    fs::remove_file(t.abs("S/own.json")).unwrap();
    assert_eq!(t.fake.trashed_log(), vec![foreign]);
    assert_eq!(t.fake.deleted_log(), vec![own]);
    assert!(!fs::exists(t.abs("S/foreign.mokuro")) && !fs::exists(t.abs("S/own.json")));
    let tdir = t.fake.node_at(&format!("{FOLDER}/T")).unwrap().pid;
    assert_eq!(
        fs::remove_dir(t.abs("T")).unwrap_err().kind(),
        std::io::ErrorKind::DirectoryNotEmpty
    );
    fs::remove_dir_all(t.abs("T")).unwrap();
    assert_eq!(t.fake.trashed_log().last(), Some(&tdir));
    assert!(!fs::exists(t.abs("T/x.webp")));
    let ev = t.events();
    assert!(ev.contains(&WatchEvent::Removed {
        path: t.abs("S/foreign.mokuro"),
        is_dir: false
    }));
    assert!(ev.contains(&WatchEvent::Removed {
        path: t.abs("T"),
        is_dir: true
    }));
    // The echoes are Bunko's own: nothing more, no hooks.
    t.sync_feed();
    assert_eq!(t.events(), vec![]);
}

#[test]
fn rename_moves_by_id_including_case_only_renames() {
    for mk in [FakeBackend::drive_like, FakeBackend::onedrive_like] {
        let t = T::new(
            mk(),
            &[("S/a.mokuro", b"a"), ("S/v.cbz", b"zip"), ("U/", b"")],
        );
        t.wait_steady();
        let pid = t.fake.node_at(&format!("{FOLDER}/S/a.mokuro")).unwrap().pid;
        t.fake.reset_calls();
        fs::rename(t.abs("S/a.mokuro"), t.abs("S/b.mokuro")).unwrap();
        fs::rename(t.abs("S/b.mokuro"), t.abs("S/B.mokuro")).unwrap();
        assert_eq!(t.fake.calls(FakeOp::Update), 2);
        assert_eq!(t.fake.calls(FakeOp::Upload), 0);
        assert_eq!(
            t.fake.node_at(&format!("{FOLDER}/S/B.mokuro")).unwrap().pid,
            pid
        );
        assert!(!fs::exists(t.abs("S/a.mokuro")) && !fs::exists(t.abs("S/b.mokuro")));
        assert_eq!(fs::read(t.abs("S/B.mokuro")).unwrap(), b"a");
        // A folder moves with its contents.
        fs::rename(t.abs("S"), t.abs("U/S2")).unwrap();
        assert_eq!(fs::read(t.abs("U/S2/v.cbz")).unwrap(), b"zip");
        assert!(!fs::exists(t.abs("S")));
        // The echo of Bunko's own move runs nothing and changes nothing.
        t.events();
        t.sync_feed();
        assert_eq!(t.events(), vec![]);
        assert_eq!(fs::read(t.abs("U/S2/B.mokuro")).unwrap(), b"a");
    }
}

#[test]
fn webdav_path_id_moves_rebase_pending_ops_and_the_ledger() {
    let t = T::new(FakeBackend::webdav_cas(), &[("S/v.cbz", b"zip")]);
    t.wait_steady();
    fs::write(t.abs("S/own.json"), b"mine").unwrap();
    t.gate(false);
    t.publish("S/v.mokuro", b"sidecar", deferred(ocr(), Mode::CreateNew));
    fs::rename(t.abs("S"), t.abs("T")).unwrap();
    // The pending write moved with its folder; Bunko's own file is still its own.
    assert_eq!(fs::read(t.abs("T/v.mokuro")).unwrap(), b"sidecar");
    assert!(fs::metadata(t.abs("T/own.json")).unwrap().own());
    assert!(!fs::exists(t.abs("S/v.mokuro")));
    t.gate(true);
    t.idle();
    assert_eq!(t.remote_bytes("T/v.mokuro").unwrap(), b"sidecar");
    assert!(t.fake.node_at(&format!("{FOLDER}/S")).is_none());
    t.events();
    t.sync_feed();
    assert_eq!(t.events(), vec![]);
    assert!(fs::metadata(t.abs("T/own.json")).unwrap().own());
    // Own after the move: removing it is a permanent delete.
    fs::remove_file(t.abs("T/own.json")).unwrap();
    assert_eq!(t.fake.deleted_log().len(), 1);
}

#[test]
fn own_echoes_emit_nothing_and_do_not_trip_d19() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.suspect_min_removals = 1;
    }
    let t = T::with(FakeBackend::drive_like(), &[("S/keep.cbz", b"zip")], tune);
    t.wait_steady();
    t.events();
    for i in 0..4 {
        fs::write(t.abs(&format!("S/f{i}.json")), b"x").unwrap();
    }
    assert_eq!(t.events().len(), 4);
    t.sync_feed();
    assert_eq!(t.events(), vec![]);
    for i in 0..4 {
        fs::remove_file(t.abs(&format!("S/f{i}.json"))).unwrap();
    }
    assert_eq!(t.events().len(), 4);
    t.sync_feed();
    assert_eq!(t.events(), vec![]);
    assert_eq!(t.s().engine_state(), bunko_remote::EngineState::Steady);
    assert_eq!(t.s().health(), Health::Online);
    // The control: the same removals by the reader do trip D19.
    for i in 0..4 {
        t.fake.reader_put(&format!("{FOLDER}/S/r{i}.json"), b"x");
    }
    t.sync_feed();
    for i in 0..4 {
        t.fake.reader_delete(&format!("{FOLDER}/S/r{i}.json"));
    }
    t.sync_feed();
    assert!(matches!(
        t.s().engine_state(),
        bunko_remote::EngineState::Suspect { .. }
    ));
}

#[test]
fn folders_are_created_once_and_idempotently() {
    let t = T::new(FakeBackend::webdav_cas(), &[]);
    t.wait_steady();
    t.fake.reset_calls();
    fs::create_dir_all(t.abs("A/B/C")).unwrap();
    assert_eq!(t.fake.calls(FakeOp::Mkdir), 3);
    fs::create_dir_all(t.abs("A/B/C")).unwrap();
    assert_eq!(t.fake.calls(FakeOp::Mkdir), 3);
    assert_eq!(
        fs::create_dir(t.abs("A/B")).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        fs::create_dir(t.abs("X/Y")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(fs::metadata(t.abs("A/B/C")).unwrap().is_dir());
    assert!(t.fake.node_at(&format!("{FOLDER}/A/B/C")).is_some());
    // Publishing never creates a missing parent.
    let src = t.local("x", b"x");
    assert_eq!(
        fs::rename(&src, t.abs("Q/x.json")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

// ---- staging (§3.4) --------------------------------------------------------------

#[test]
fn staging_files_publish_only_explicitly_and_survive_a_restart() {
    let mut t = T::new(FakeBackend::drive_like(), &[("S/v.cbz", b"zip")]);
    t.wait_steady();
    {
        use std::io::Write;
        let mut f = fs::File::create(t.abs("S/.nocover")).unwrap();
        f.write_all(b"1").unwrap();
    }
    // Dropped, never published: local only, but there.
    assert!(fs::exists(t.abs("S/.nocover")));
    assert!(
        fs::metadata(t.abs("S/.nocover"))
            .unwrap()
            .as_std()
            .is_some()
    );
    assert!(t.fake.node_at(&format!("{FOLDER}/S/.nocover")).is_none());
    assert_eq!(t.fake.calls(FakeOp::Upload), 0);
    t.stop();
    t.start();
    assert!(fs::exists(t.abs("S/.nocover")));
    // copy + rename: the rename publishes.
    let src = t.local("cover", b"webp bytes");
    fs::copy(&src, t.abs("S/.cover.tmp")).unwrap();
    assert!(t.fake.node_at(&format!("{FOLDER}/S/.cover.tmp")).is_none());
    fs::rename(t.abs("S/.cover.tmp"), t.abs("S/cover.webp")).unwrap();
    assert_eq!(t.remote_bytes("S/cover.webp").unwrap(), b"webp bytes");
    assert!(!fs::exists(t.abs("S/.cover.tmp")));
    fs::remove_file(t.abs("S/.nocover")).unwrap();
    assert!(!fs::exists(t.abs("S/.nocover")));
}

#[test]
fn conflicts_reach_sync_callers_as_conflict_errors() {
    let t = T::new(FakeBackend::webdav_cas(), &[("S/a.mokuro", b"v1")]);
    t.wait_steady();
    let base = version(&fs::metadata(t.abs("S/a.mokuro")).unwrap());
    t.fake.reader_put(&format!("{FOLDER}/S/a.mokuro"), b"v2");
    let src = t.local("a", b"upgrade");
    let w = WriteIntent::new(
        Mode::Replace {
            base: Some(bunko_store::Version::new(base)),
        },
        Origin::Upgrade,
        Durability::Sync,
    );
    let e = fs::publish(&src, t.abs("S/a.mokuro"), &w).unwrap_err();
    assert!(bunko_store::is_conflict(&e), "{e}");
    assert_eq!(t.remote_bytes("S/a.mokuro").unwrap(), b"v2");
    // A Dav create over an existing name.
    let src = t.local("b", b"x");
    let w = WriteIntent::new(
        Mode::CreateNew,
        Origin::Dav { user: None },
        Durability::Sync,
    );
    let e = fs::publish(&src, t.abs("S/a.mokuro"), &w).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
}

// ---- the lease and the queue file ------------------------------------------------

#[test]
fn standby_holds_deferred_ops_and_refuses_sync_writes() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.lease = true;
    }
    let fake = FakeBackend::drive_like();
    let marker = serde_json::json!({
        "version": 1, "kind": "mokuro-bunko", "instance_id": "other-install",
        "boot_id": "b", "instance_name": "nas", "epoch": 5,
        "claimed_at": "2026-10-12T10:00:00Z", "ttl_s": 900, "state": "active"
    });
    fake.reader_mkdir(&format!("{FOLDER}/S"));
    fake.reader_put(
        &format!("{FOLDER}/.mokuro-bunko.json"),
        &serde_json::to_vec(&marker).unwrap(),
    );
    let t = T::with(fake, &[], tune);
    t.wait_steady();
    wait_until("standby", || {
        t.s().health() == Health::LeaseHeld { by: "nas".into() }
    });
    t.publish("S/v.mokuro", b"x", deferred(ocr(), Mode::CreateNew));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(t.s().outbox()[0].state, "HELD");
    assert_eq!(t.fake.calls(FakeOp::Upload), 0);
    let e = fs::write(t.abs("S/a.json"), b"x").unwrap_err();
    assert!(bunko_store::is_unavailable(&e), "{e}");
}

#[test]
fn the_queue_file_is_written_on_change_within_its_floor_with_done_entries() {
    fn tune(c: &mut RemoteStoreConfig) {
        c.queue_floor = Some(Duration::from_millis(400));
    }
    let t = T::with(
        FakeBackend::drive_like(),
        &[("Kingdom/Kingdom 12.cbz", b"zip")],
        tune,
    );
    t.wait_steady();
    t.publish(
        "Kingdom/Kingdom 12.mokuro",
        b"{}",
        deferred(ocr(), Mode::CreateNew),
    );
    t.idle();
    let doc = |n: u64| {
        serde_json::json!({"version": 1, "generated_at": format!("2026-10-12T10:00:0{n}Z"),
            "pending_volumes": n, "volumes": []})
    };
    assert!(t.s().publish_queue_doc(doc(1)).unwrap());
    t.idle();
    let read = |t: &T| -> serde_json::Value {
        serde_json::from_slice(&t.remote_bytes(".mokuro-queue.json").unwrap()).unwrap()
    };
    let v = read(&t);
    assert_eq!(v["instance_id"], t.s().instance_id());
    assert_eq!(v["done"][0]["volume"], "Kingdom 12");
    assert_eq!(v["done"][0]["ocr"], "Kingdom/Kingdom 12.mokuro");
    let uploads = t.fake.calls(FakeOp::Upload);
    // Only `generated_at` differs: nothing is written.
    let mut same = doc(1);
    same["generated_at"] = "2026-10-12T11:00:00Z".into();
    assert!(!t.s().publish_queue_doc(same).unwrap());
    // A change within the floor waits for it; only the newest is written.
    assert!(t.s().publish_queue_doc(doc(2)).unwrap());
    assert!(t.s().publish_queue_doc(doc(3)).unwrap());
    std::thread::sleep(Duration::from_millis(100));
    t.idle();
    assert_eq!(t.fake.calls(FakeOp::Upload), uploads);
    wait_until("the floor", || t.fake.calls(FakeOp::Upload) > uploads);
    t.idle();
    assert_eq!(t.fake.calls(FakeOp::Upload), uploads + 1);
    assert_eq!(read(&t)["pending_volumes"], 3);
}
