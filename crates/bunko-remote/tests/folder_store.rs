//! `RemoteStore` over a real `FolderBackend` (plan §4.1): a library in a temporary
//! folder, driven through `bunko_store::fs` from std threads, with the watcher on and
//! the feed's poll intervals at an hour, so only the watcher brings outside changes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bunko_remote::folder::{FolderBackend, FolderConfig};
use bunko_remote::status::{LeaseEvent, MARKER_NAME, Marker};
use bunko_remote::{RemoteStore, RemoteStoreConfig};
use bunko_store::{Durability, LibraryStore, Mode, MountGuard, Origin, WriteIntent, fs};
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const WAIT: Duration = Duration::from_secs(30);

type Store = RemoteStore<FolderBackend>;

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Fields drop in order: the store goes before its folders (Windows cannot remove a
/// folder a store still holds files in).
struct T {
    mount: Option<MountGuard>,
    store: Option<Arc<Store>>,
    /// The library folder on disk (what Mokuro Reader's Local Folder would pick).
    lib: PathBuf,
    /// Where the store mounts it.
    root: PathBuf,
    tune: fn(&mut RemoteStoreConfig),
    tmp: TempDir,
}

fn no_tune(_: &mut RemoteStoreConfig) {}

/// The lease on, with a short settle wait.
fn lease_quick(c: &mut RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_millis(300));
}

impl T {
    fn new(files: &[(&str, &[u8])]) -> T {
        T::with(files, no_tune)
    }

    fn with(files: &[(&str, &[u8])], tune: fn(&mut RemoteStoreConfig)) -> T {
        let tmp = TempDir::new().unwrap();
        // Named otherwise than the root it is reported as.
        let lib = tmp.path().join("library-folder/Manga");
        std::fs::create_dir_all(&lib).unwrap();
        for (rel, bytes) in files {
            let p = lib.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        let root = tmp.path().join("cloud/s/library");
        let mut t = T {
            tmp,
            lib,
            root,
            store: None,
            mount: None,
            tune,
        };
        t.start();
        t
    }

    fn state_dir(&self) -> PathBuf {
        self.tmp.path().join("cloud/s")
    }

    fn cfg(&self) -> RemoteStoreConfig {
        let mut cfg = RemoteStoreConfig::new(&self.root, self.state_dir(), FOLDER);
        // Only the watcher wakes the feed.
        cfg.poll_active = Duration::from_secs(3600);
        cfg.poll_idle = Duration::from_secs(3600);
        cfg.quiet = Duration::from_millis(500);
        cfg.lease = false;
        cfg.retry_base = Duration::from_millis(20);
        cfg.mount_id = 7;
        (self.tune)(&mut cfg);
        cfg
    }

    /// Open (or reopen, after [`T::stop`]): a new backend, as after a restart.
    fn start(&mut self) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let backend = rt
            .block_on(FolderBackend::connect(FolderConfig {
                path: self.lib.clone(),
                root_name: FOLDER.into(),
                state_dir: self.state_dir(),
                watch: true,
            }))
            .unwrap();
        drop(rt);
        let store = RemoteStore::open_shared(self.cfg(), Arc::new(backend)).unwrap();
        self.mount = Some(bunko_store::mount(store.clone() as Arc<dyn LibraryStore>));
        self.store = Some(store);
        assert!(self.s().wait_steady(WAIT), "{:?}", self.s().engine_state());
    }

    /// A crash: the store goes away without any further writes.
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

    fn disk(&self, rel: &str) -> PathBuf {
        let mut p = self.lib.clone();
        for s in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(s);
        }
        p
    }

    fn idle(&self) {
        assert!(self.s().wait_outbox_idle(WAIT), "{:?}", self.s().outbox());
    }

    fn local(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let p = self.tmp.path().join(format!("local-{name}"));
        std::fs::write(&p, bytes).unwrap();
        p
    }

    fn publish(&self, rel: &str, bytes: &[u8]) -> std::io::Result<bunko_store::Published> {
        let src = self.local(&rel.replace('/', "_"), bytes);
        fs::publish(
            &src,
            self.abs(rel),
            &WriteIntent::new(
                Mode::CreateNew,
                Origin::Ocr { claim: "c1".into() },
                Durability::Deferred,
            ),
        )
    }

    /// Every file in the library folder whose name contains `needle`, outside the
    /// trash.
    fn on_disk_named(&self, dir: &str, needle: &str) -> Vec<PathBuf> {
        std::fs::read_dir(self.disk(dir))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().contains(needle))
            .collect()
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

#[test]
fn a_seeded_folder_syncs_and_reads_byte_exact() {
    let cbz = pattern(3 << 20);
    let t = T::new(&[
        ("S/Vol 1.cbz", &cbz),
        ("S/Vol 1.mokuro", b"{\"pages\":[]}"),
        ("S/series.json", b"{\"title\":\"S\"}"),
        ("T/Vol 2.cbz", b"zip"),
        ("volume-data.json", b"{}"),
    ]);
    for rel in [
        "S/Vol 1.cbz",
        "S/Vol 1.mokuro",
        "S/series.json",
        "T/Vol 2.cbz",
        "volume-data.json",
    ] {
        assert!(fs::is_file(t.abs(rel)), "{rel}");
    }
    assert!(fs::is_dir(t.abs("S")));
    assert_eq!(fs::read(t.abs("S/Vol 1.cbz")).unwrap(), cbz);
    assert_eq!(fs::read(t.abs("T/Vol 2.cbz")).unwrap(), b"zip");
    let mut names: Vec<String> = fs::read_dir(t.abs("S"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["Vol 1.cbz", "Vol 1.mokuro", "series.json"]);
}

#[test]
fn a_store_write_lands_in_the_folder() {
    let t = T::new(&[("S/Vol 1.cbz", b"zip")]);
    t.publish("S/Vol 1.mokuro", b"ocr result").unwrap();
    t.idle();
    assert_eq!(
        std::fs::read(t.disk("S/Vol 1.mokuro")).unwrap(),
        b"ocr result"
    );
    let m = fs::metadata(t.abs("S/Vol 1.mokuro")).unwrap();
    assert!(m.own());
    // Nothing of the write's own is left beside it.
    assert!(t.on_disk_named("S", ".bunko-").is_empty());
    // A new folder too.
    fs::create_dir_all(t.abs("New Series")).unwrap();
    assert!(t.disk("New Series").is_dir());
    t.publish("New Series/Vol 1.mokuro", b"x").unwrap();
    t.idle();
    assert_eq!(
        std::fs::read(t.disk("New Series/Vol 1.mokuro")).unwrap(),
        b"x"
    );
}

#[test]
fn a_chrome_style_write_shows_only_after_the_swap() {
    let t = T::new(&[("S/Vol 1.cbz", b"zip")]);
    // Chromium: an empty file, then the bytes into the swap file beside it.
    std::fs::write(t.disk("S/Vol 2.cbz"), b"").unwrap();
    let bytes = pattern(200_000);
    std::fs::write(t.disk("S/Vol 2.cbz.crswap"), &bytes).unwrap();
    // The watcher wakes the feed after the quiet period: nothing to show yet.
    std::thread::sleep(Duration::from_secs(4));
    assert!(!fs::exists(t.abs("S/Vol 2.cbz")));
    assert!(!fs::exists(t.abs("S/Vol 2.cbz.crswap")));
    // The swap lands.
    std::fs::rename(t.disk("S/Vol 2.cbz.crswap"), t.disk("S/Vol 2.cbz")).unwrap();
    let t0 = Instant::now();
    wait_until("the volume", || fs::exists(t.abs("S/Vol 2.cbz")));
    assert!(t0.elapsed() < Duration::from_secs(15), "{:?}", t0.elapsed());
    assert_eq!(fs::read(t.abs("S/Vol 2.cbz")).unwrap(), bytes);
}

#[test]
fn an_outside_delete_is_applied_after_the_grace() {
    let t = T::new(&[("S/Vol 1.cbz", b"one"), ("S/Vol 2.cbz", b"two")]);
    std::fs::remove_file(t.disk("S/Vol 2.cbz")).unwrap();
    wait_until("the removal", || !fs::exists(t.abs("S/Vol 2.cbz")));
    assert!(fs::exists(t.abs("S/Vol 1.cbz")));
    // An outside folder rename: the old path goes, the new one comes.
    std::fs::rename(t.disk("S"), t.disk("Renamed")).unwrap();
    wait_until("the rename", || {
        fs::exists(t.abs("Renamed/Vol 1.cbz")) && !fs::exists(t.abs("S"))
    });
}

#[test]
fn the_lease_marker_and_queue_file_are_written_at_the_root() {
    let t = T::with(&[("S/Vol 1.cbz", b"zip")], lease_quick);
    wait_until("the lease", || {
        t.s().lease().map(|l| l.event()) == Some(LeaseEvent::Acquired)
    });
    wait_until("the marker", || t.disk(MARKER_NAME).is_file());
    let m = Marker::parse(&std::fs::read(t.disk(MARKER_NAME)).unwrap()).unwrap();
    assert!(!m.instance_id.is_empty());
    let doc = serde_json::json!({"version": 1, "generated_at": "2026-10-12T10:00:01Z",
        "pending_volumes": 1, "volumes": [{"path": "S/Vol 1.cbz"}]});
    assert!(t.s().publish_queue_doc(doc).unwrap());
    t.idle();
    wait_until("the queue file", || t.disk(".mokuro-queue.json").is_file());
    let q: serde_json::Value =
        serde_json::from_slice(&std::fs::read(t.disk(".mokuro-queue.json")).unwrap()).unwrap();
    assert_eq!(q["pending_volumes"], 1, "{q}");
    // The control files are listed like any other file, and nothing else was left.
    assert!(t.on_disk_named("", ".bunko-").is_empty());
}

/// A crash after the rename put the file in place and before the commit: the reopened
/// store proves the file its own (the memo's sha256: no download above 4 MiB) and
/// adopts it, with no conflict and no second copy.
#[test]
fn a_crash_after_the_rename_adopts_the_file_at_reopen() {
    for (name, size) in [("small.mokuro", 1_000usize), ("big.mokuro", 6 << 20)] {
        let mut t = T::new(&[("S/Vol 1.cbz", b"zip")]);
        t.s()
            .set_failpoint(Some(Arc::new(|at: &str| at == "uploaded")));
        let bytes = pattern(size);
        let rel = format!("S/{name}");
        let _ = t.publish(&rel, &bytes);
        wait_until("the crash", || t.s().crashed());
        assert_eq!(
            std::fs::read(t.disk(&rel)).unwrap(),
            bytes,
            "{name}: renamed in place"
        );
        t.stop();
        t.start();
        t.idle();
        let copies = t.on_disk_named("S", name.trim_end_matches(".mokuro"));
        assert_eq!(copies, [t.disk(&rel)], "{name}");
        assert!(
            t.s().conflicts().is_empty(),
            "{name}: {:?}",
            t.s().conflicts()
        );
        // Committed; its echo came before the commit, so it may wait as COMMITTED
        // (dropped at the next start), but nothing is pending or in conflict.
        assert!(
            t.s().outbox().iter().all(|o| o.state == "COMMITTED"),
            "{name}: {:?}",
            t.s().outbox()
        );
        let m = fs::metadata(t.abs(&rel)).unwrap();
        assert!(m.own(), "{name}: adopted as Bunko's own");
        assert_eq!(fs::read(t.abs(&rel)).unwrap(), bytes, "{name}");
    }
}
