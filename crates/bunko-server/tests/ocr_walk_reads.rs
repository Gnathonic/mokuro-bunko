//! What the OCR walk reads of a library's sidecars: the upgrade census reads what it
//! needs of a sidecar (its recipe, its revision, its pages, its seal) once per version of
//! that file, ever. Neither a walk after the OCR settings are applied nor one after a
//! restart reads an unchanged sidecar again; a sidecar replaced (in place, or MEGA's
//! delete-then-upload), removed or renamed in case is judged on what is there now,
//! exactly as a census that never saw the library judges it.
//!
//! On a cloud library every such read is a provider download. The store's small-file
//! cache is smaller than the library's sidecars here, as it is on a large library
//! (`cache_mb` against GBs of sidecars), so the reads are counted from the
//! `FakeBackend`'s read log. On a local library they are counted by the census's cache.

// Fixtures are written and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::collections::{BTreeMap, HashMap};
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bunko_core::config::UpgradeConfig;
use bunko_core::generations::{Generation, default_generation};
use bunko_db::Database;
use bunko_remote::fake::FakeBackend;
use bunko_remote::{RemoteStore, RemoteStoreConfig};
use bunko_server::ocr::owed::{KnownPages, OwedVolume, UpgradeProbe, walk};
use bunko_server::ocr::types::LibraryFacts;
use bunko_server::ocr::upgrade::Upgrade;
use bunko_store::{LibraryStore, MountGuard};
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const WAIT: Duration = Duration::from_secs(20);

/// The metadata cache's answers, without the metadata pass (it has its own cache): no
/// volume misses a page, no page count is known. The census's own reads are what is
/// counted.
struct NoMetadata;

impl LibraryFacts for NoMetadata {
    fn missing_pages(&self, _cbz: &Path) -> i64 {
        0
    }
}

fn cbz(pages: usize) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for i in 0..pages {
        z.start_file(
            format!("{:03}.jpg", i + 1),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        z.write_all(b"fake image bytes").unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn pages() -> String {
    (1..=3)
        .map(|i| format!(r#"{{"img_path":"{i:03}.jpg","blocks":[]}}"#))
        .collect::<Vec<_>>()
        .join(",")
}

/// Bunko's own hayai-nova output at `revision` (sealed, as an OCR install leaves it).
fn hayai(revision: u32, generation: &str, title: &str) -> Vec<u8> {
    let text = format!(
        r#"{{"version":"0.2.5","title":"{title}","volume":"V","ocr_engine":{{"id":"hayai-nova","detector":"ppocr-manga","patch_budget":512,"revision":{revision},"generation":"{generation}"}},"pages":[{}]}}"#,
        pages()
    );
    let mut v = bunko_layout::json::Value::parse(&text).unwrap();
    bunko_layout::seal::seal(&mut v)
        .expect("an ocr_engine block to seal")
        .into_bytes()
}

/// A sidecar mokuro made (no `ocr_engine` block): `mokuro-legacy`.
fn legacy() -> Vec<u8> {
    format!(
        r#"{{"version":"0.2.1","title":"S","volume":"V","pages":[{}]}}"#,
        pages()
    )
    .into_bytes()
}

/// A reader's own layer: no stamp, no seal.
fn readers_layer() -> Vec<u8> {
    format!(r#"{{"title":"S","volume":"V","pages":[{}]}}"#, pages()).into_bytes()
}

fn rows() -> Vec<Generation> {
    let mut fast = default_generation("g-2");
    fast.name = "fast".into();
    fast.primary = false;
    vec![default_generation("g-1"), fast]
}

fn upgrade_config() -> UpgradeConfig {
    UpgradeConfig {
        enabled: true,
        replace: vec!["mokuro-legacy".into(), "mokuro".into()],
    }
}

/// The revision the test's build gives hayai-nova: one above the real one, so a file at
/// the real revision is an older revision's output.
fn current() -> u32 {
    bunko_layout::sidecar::engine_revision("hayai-nova") + 1
}

/// A census over `library`, configured as the tests' server is.
fn census(library: &Path, db: Option<Arc<Database>>) -> Arc<Upgrade> {
    let up = Arc::new(Upgrade::new(
        library.to_path_buf(),
        db,
        Arc::new(NoMetadata),
        Arc::new(bunko_library::service::NoPathLocks),
        "mokuro-bunko test".into(),
    ));
    up.configure(&upgrade_config(), &rows());
    up.set_engine_revision("hayai-nova", current());
    up
}

/// Six volumes. `A/Vol n` are done (main and `fast` layer at the current revision, plus a
/// reader's `notes` layer no row writes). `B/Vol 1`'s main is mokuro's (outdated) beside a
/// current `fast` layer of the primary's recipe (ready for a direct replace); `B/Vol 2`'s
/// `fast` layer is an older revision's output (owed again); `B/Vol 3`'s main is mokuro's
/// with no layer at all (an upgrade job, and the layer).
fn library() -> Vec<(String, Vec<u8>)> {
    let now = hayai(current(), "hayai-nova", "S");
    let fast = hayai(current(), "fast", "S");
    let old_fast = hayai(current() - 1, "fast", "S");
    let mut files = Vec::new();
    for (vol, main, layer) in [
        ("A/Vol 1", now.clone(), Some(fast.clone())),
        ("A/Vol 2", now.clone(), Some(fast.clone())),
        ("A/Vol 3", now.clone(), Some(fast.clone())),
        ("B/Vol 1", legacy(), Some(fast.clone())),
        ("B/Vol 2", now.clone(), Some(old_fast.clone())),
        ("B/Vol 3", legacy(), None),
    ] {
        files.push((format!("{vol}.cbz"), cbz(3)));
        files.push((format!("{vol}.mokuro"), main));
        if let Some(layer) = layer {
            files.push((format!("{vol}.fast.mokuro"), layer));
        }
        if vol.starts_with("A/") {
            files.push((format!("{vol}.notes.mokuro"), readers_layer()));
        }
    }
    files
}

fn owed(rows: &[&str], outdated: &[&str], upgrade: bool) -> Owed {
    Owed {
        rows: rows.iter().map(|s| s.to_string()).collect(),
        outdated: outdated.iter().map(|s| s.to_string()).collect(),
        upgrade,
    }
}

fn expected() -> BTreeMap<String, Owed> {
    BTreeMap::from([
        ("B/Vol 2.cbz".to_string(), owed(&["g-2"], &["g-2"], false)),
        ("B/Vol 3.cbz".to_string(), owed(&["g-2"], &[], true)),
    ])
}

/// What a walk decided: the owed index, the census and the volumes ready for a direct
/// replace.
#[derive(Debug, PartialEq)]
struct Decisions {
    owed: BTreeMap<String, Owed>,
    census: serde_json::Value,
    ready: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct Owed {
    rows: Vec<String>,
    outdated: Vec<String>,
    upgrade: bool,
}

impl Owed {
    fn of(v: &OwedVolume) -> Owed {
        Owed {
            rows: v.rows.iter().map(|r| r.to_string()).collect(),
            outdated: v.outdated.iter().map(|r| r.to_string()).collect(),
            upgrade: v.upgrade,
        }
    }
}

/// One full walk of `library` with `up` as its probe.
fn decide(library: &Path, up: &Upgrade) -> Decisions {
    let probe: &dyn UpgradeProbe = up;
    let index = walk(
        library,
        &rows(),
        &NoMetadata,
        Some(probe),
        &KnownPages::new(),
    );
    Decisions {
        owed: index
            .volumes
            .into_iter()
            .map(|(rel, v)| (rel, Owed::of(&v)))
            .collect(),
        census: up.census(),
        ready: up.ready_volumes(),
    }
}

/// What a census that never saw the library decides of it now (no database, nothing
/// cached): what every walk must decide, whatever it remembers.
fn fresh(library: &Path) -> Decisions {
    decide(library, &census(library, None))
}

fn twice(reads: &BTreeMap<String, usize>) -> Vec<(&String, &usize)> {
    reads.iter().filter(|(_, n)| **n > 1).collect()
}

fn total(reads: &BTreeMap<String, usize>) -> usize {
    reads.values().sum()
}

// ---- a cloud library ---------------------------------------------------------------

struct Cloud {
    tmp: TempDir,
    fake: Arc<FakeBackend>,
    root: PathBuf,
    store: Option<Arc<RemoteStore<FakeBackend>>>,
    mount: Option<MountGuard>,
    db: Option<Arc<Database>>,
    up: Option<Arc<Upgrade>>,
}

impl Cloud {
    fn start(fake: FakeBackend) -> Cloud {
        let fake = Arc::new(fake);
        for (rel, bytes) in library() {
            fake.reader_put(&format!("{FOLDER}/{rel}"), &bytes);
        }
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("cloud/s/library");
        let mut t = Cloud {
            tmp,
            fake,
            root,
            store: None,
            mount: None,
            db: None,
            up: None,
        };
        t.open();
        t
    }

    /// Open the store, the database and the census over them (again, after
    /// [`Cloud::stop`]: as after a restart, from what each persisted).
    fn open(&mut self) {
        let mut cfg =
            RemoteStoreConfig::new(self.root.clone(), self.tmp.path().join("cloud/s"), FOLDER);
        cfg.poll_active = Duration::from_secs(3600);
        cfg.poll_idle = Duration::from_secs(3600);
        cfg.lease = false;
        // Smaller than any two sidecars: the small-file cache keeps none of them for
        // the next walk (a library with more sidecar bytes than `cache_mb`).
        cfg.cache_bytes = 1;
        let store = RemoteStore::open_shared(cfg, self.fake.clone()).unwrap();
        assert!(store.wait_steady(WAIT), "{:?}", store.engine_state());
        self.mount = Some(bunko_store::mount(store.clone() as Arc<dyn LibraryStore>));
        self.store = Some(store);
        let db = Arc::new(Database::open(self.tmp.path().join("mokuro.db")).unwrap());
        self.up = Some(census(&self.root, Some(db.clone())));
        self.db = Some(db);
    }

    fn stop(&mut self) {
        self.up = None;
        self.db = None;
        self.mount = None;
        self.store = None;
    }

    fn store(&self) -> &Arc<RemoteStore<FakeBackend>> {
        self.store.as_ref().unwrap()
    }

    fn up(&self) -> &Arc<Upgrade> {
        self.up.as_ref().unwrap()
    }

    fn db(&self) -> &Arc<Database> {
        self.db.as_ref().unwrap()
    }

    /// The reader's changes, picked up now: until the store lists every file the
    /// provider holds under the library, each at the provider's version.
    fn sync(&self) {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            self.store().feed().poll_now();
            assert!(self.store().wait_steady(WAIT));
            let provider: BTreeMap<String, String> = self
                .fake_files()
                .into_iter()
                .filter_map(|(rel, _)| {
                    let node = self.fake.node_at(&format!("{FOLDER}/{rel}"))?;
                    Some((rel, node.cver))
                })
                .collect();
            let listed: BTreeMap<String, String> = ["A", "B", "C"]
                .iter()
                .flat_map(|series| {
                    let names = bunko_library::sidecar::list_names(&self.root.join(series));
                    names
                        .unwrap_or_default()
                        .into_iter()
                        .map(move |n| format!("{series}/{n}"))
                })
                .filter_map(|rel| {
                    let meta = bunko_store::fs::metadata(self.root.join(&rel)).ok()?;
                    Some((rel, meta.version()?.as_str().to_string()))
                })
                .collect();
            if listed == provider {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the store catches up: {listed:?} != {provider:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// One full walk: what it decided, the provider reads it made of each sidecar (by
    /// library path), and of archives.
    fn walk(&self) -> (Decisions, BTreeMap<String, usize>, usize) {
        let pids: HashMap<String, String> = self
            .fake_files()
            .into_iter()
            .map(|(rel, pid)| (pid, rel))
            .collect();
        self.store().backend().reset_calls();
        let decisions = decide(&self.root, self.up());
        let mut sidecars: BTreeMap<String, usize> = BTreeMap::new();
        let mut archives = 0;
        for read in self.store().backend().read_log() {
            let rel = pids.get(&read.pid).cloned().unwrap_or(read.pid);
            if rel.to_ascii_lowercase().ends_with(".cbz") {
                archives += 1;
            } else {
                *sidecars.entry(rel).or_default() += 1;
            }
        }
        (decisions, sidecars, archives)
    }

    /// Every file the provider holds under the library: `(rel, pid)`.
    fn fake_files(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for series in ["A", "B", "C"] {
            for vol in ["Vol 1", "Vol 2", "Vol 3", "vol 2"] {
                for suffix in [".cbz", ".mokuro", ".fast.mokuro", ".notes.mokuro"] {
                    let rel = format!("{series}/{vol}{suffix}");
                    if let Some(n) = self.fake.node_at(&format!("{FOLDER}/{rel}")) {
                        out.push((rel, n.pid));
                    }
                }
            }
        }
        out
    }

    fn put(&self, rel: &str, bytes: &[u8]) {
        self.fake.reader_put(&format!("{FOLDER}/{rel}"), bytes);
    }

    fn delete(&self, rel: &str) {
        self.fake.reader_delete(&format!("{FOLDER}/{rel}"));
    }

    /// The library paths the census keeps facts of.
    fn kept(&self) -> Vec<String> {
        self.db()
            .sidecar_facts_keys()
            .unwrap()
            .into_iter()
            .map(|k| k.sidecar_path)
            .collect()
    }
}

/// The census reads a sidecar once per version, ever: the first walk reads each one it
/// needs once; a walk after the settings are applied, and one after a restart over the
/// same files, read none; a sidecar the reader replaced is read once more, at its new
/// version, and the walk decides on that version.
#[test]
fn a_cloud_walk_reads_each_sidecar_once_per_version() {
    let mut t = Cloud::start(FakeBackend::drive_like());
    let report = |phase: &str, reads: &BTreeMap<String, usize>, archives: usize| {
        eprintln!(
            "{phase}: {} provider reads of {} sidecars ({archives} of archives): {reads:?}",
            total(reads),
            reads.len()
        );
    };

    let (first, first_reads, archives) = t.walk();
    report("first walk", &first_reads, archives);

    // OCR settings applied (`apply_config` → `Upgrade::configure`, then a walk).
    t.up().configure(&upgrade_config(), &rows());
    let (second, applied_reads, archives) = t.walk();
    report("after the settings were applied", &applied_reads, archives);

    // A restart: the store, the database and the census open again.
    t.stop();
    t.open();
    let (third, restart_reads, archives) = t.walk();
    report("after a restart", &restart_reads, archives);

    // The reader replaces one layer in place (a new version at the same path): an older
    // revision's output now, so the volume owes the row again.
    t.put("A/Vol 1.fast.mokuro", &hayai(current() - 1, "fast", "S"));
    t.sync();
    let (fourth, replace_reads, archives) = t.walk();
    report("after one replace", &replace_reads, archives);

    assert_eq!(first.owed, expected(), "{first:?}");
    assert_eq!(first.ready, ["B/Vol 1.cbz"], "{first:?}");
    assert_eq!(second, first);
    assert_eq!(third, first);
    assert!(
        twice(&first_reads).is_empty(),
        "a sidecar read twice in one walk: {first_reads:?}"
    );
    assert!(
        !first_reads.keys().any(|r| r.ends_with(".notes.mokuro")),
        "a layer no row writes, beside a current main, is never read: {first_reads:?}"
    );
    assert!(
        applied_reads.is_empty(),
        "nothing read again: {applied_reads:?}"
    );
    assert!(
        restart_reads.is_empty(),
        "nothing read again: {restart_reads:?}"
    );
    assert_eq!(
        replace_reads,
        BTreeMap::from([("A/Vol 1.fast.mokuro".to_string(), 1)]),
        "only the new version is read"
    );
    let mut want = expected();
    want.insert("A/Vol 1.cbz".into(), owed(&["g-2"], &["g-2"], false));
    assert_eq!(fourth.owed, want);
    assert_eq!(fourth, fresh(&t.root));
}

/// Whatever happens to a sidecar, the walk decides on what is there now, as a census
/// that never saw the library does: MEGA's delete-then-upload (a new node) and Drive's
/// replace in place are read once at the new version; a removal reads nothing; a case
/// rename of the main sidecar is read once under its new name. The facts of the files
/// gone, and of the old spelling, are pruned; a restart then reads nothing.
#[test]
fn a_changed_cloud_sidecar_is_judged_on_what_is_there_now() {
    for (name, fake) in [
        ("drive", FakeBackend::drive_like()),
        ("mega", FakeBackend::mega_like()),
    ] {
        let mut t = Cloud::start(fake);
        let (first, _, _) = t.walk();
        assert_eq!(first, fresh(&t.root), "{name}");
        let mut want = expected();

        // The reader's upload of a new version: deleted first, then uploaded (MEGA's
        // way; on Drive the same calls give a new node too).
        t.delete("A/Vol 1.fast.mokuro");
        t.sync();
        let (now, reads, _) = t.walk();
        want.insert("A/Vol 1.cbz".into(), owed(&["g-2"], &[], false));
        assert_eq!(now.owed, want, "{name}: the layer is missing");
        assert!(reads.is_empty(), "{name}: {reads:?}");
        assert_eq!(now, fresh(&t.root), "{name}");
        t.put("A/Vol 1.fast.mokuro", &hayai(current() - 1, "fast", "S2"));
        t.sync();
        let (now, reads, _) = t.walk();
        want.insert("A/Vol 1.cbz".into(), owed(&["g-2"], &["g-2"], false));
        assert_eq!(now.owed, want, "{name}: the new version is outdated");
        assert_eq!(
            reads,
            BTreeMap::from([("A/Vol 1.fast.mokuro".to_string(), 1)]),
            "{name}"
        );
        assert_eq!(now, fresh(&t.root), "{name}");

        // A replace in place, back to the current revision.
        t.put("A/Vol 1.fast.mokuro", &hayai(current(), "fast", "S3"));
        t.sync();
        let (now, reads, _) = t.walk();
        want.remove("A/Vol 1.cbz");
        assert_eq!(now.owed, want, "{name}: current again");
        assert_eq!(
            reads,
            BTreeMap::from([("A/Vol 1.fast.mokuro".to_string(), 1)]),
            "{name}"
        );
        assert_eq!(now, fresh(&t.root), "{name}");

        // A removal: nothing to read; the row is owed as missing now.
        t.delete("B/Vol 2.fast.mokuro");
        t.sync();
        let (now, reads, _) = t.walk();
        want.insert("B/Vol 2.cbz".into(), owed(&["g-2"], &[], false));
        assert_eq!(now.owed, want, "{name}: the layer is gone");
        assert!(reads.is_empty(), "{name}: {reads:?}");
        assert_eq!(now, fresh(&t.root), "{name}");

        // A case rename of a main sidecar (rule v1 still gives it to its volume).
        t.fake.reader_rename(
            &format!("{FOLDER}/A/Vol 2.mokuro"),
            &format!("{FOLDER}/A/vol 2.mokuro"),
        );
        t.sync();
        let (now, reads, _) = t.walk();
        assert_eq!(now.owed, want, "{name}: nothing changes");
        assert!(
            reads.keys().all(|r| r == "A/vol 2.mokuro") && total(&reads) <= 1,
            "{name}: at most the renamed file, once: {reads:?}"
        );
        assert_eq!(now, fresh(&t.root), "{name}");

        // The facts of what is gone go; those of what is there stay.
        let before = t.kept();
        assert!(
            before.contains(&"B/Vol 2.fast.mokuro".to_string()),
            "{name}: {before:?}"
        );
        let pruned = t.up().sidecar_facts().prune();
        let after = t.kept();
        eprintln!("{name}: pruned {pruned}: {before:?} -> {after:?}");
        assert!(
            !after.contains(&"B/Vol 2.fast.mokuro".to_string()),
            "{name}: {after:?}"
        );
        for kept in [
            "A/Vol 1.fast.mokuro",
            "A/Vol 3.mokuro",
            "B/Vol 1.fast.mokuro",
        ] {
            assert!(
                after.contains(&kept.to_string()),
                "{name}: {kept} kept: {after:?}"
            );
        }
        assert_eq!(t.up().sidecar_facts().prune(), 0, "{name}: nothing more");

        // A restart judges every volume again: the renamed main is found by its version
        // (the provider keeps it through a rename), so nothing is read.
        t.stop();
        t.open();
        let (again, reads, _) = t.walk();
        assert!(reads.is_empty(), "{name}: after a restart: {reads:?}");
        assert_eq!(again.owed, want, "{name}");
        assert_eq!(again, fresh(&t.root), "{name}");
        assert!(t.kept().contains(&"A/vol 2.mokuro".to_string()), "{name}");

        // A whole series folder renamed: every file keeps its version.
        t.fake
            .reader_rename(&format!("{FOLDER}/A"), &format!("{FOLDER}/C"));
        t.sync();
        t.store().backend().reset_calls();
        let moved = decide(&t.root, t.up());
        let reads = t.store().backend().read_log();
        assert_eq!(moved.owed, want, "{name}: nothing owed in the moved series");
        // (The census still counts the volumes it judged under the old names, as it
        // always has until the next settings change: the decisions are compared.)
        let now = fresh(&t.root);
        assert_eq!((moved.owed, moved.ready), (now.owed, now.ready), "{name}");
        assert!(reads.is_empty(), "{name}: a moved folder: {reads:?}");
        let pruned = t.up().sidecar_facts().prune();
        let after = t.kept();
        eprintln!("{name}: pruned {pruned} after the folder moved: {after:?}");
        assert!(
            after.iter().all(|p| !p.starts_with("A/")),
            "{name}: {after:?}"
        );
        assert!(
            after.contains(&"C/vol 2.mokuro".to_string()),
            "{name}: {after:?}"
        );
        assert!(
            after.contains(&"C/Vol 1.fast.mokuro".to_string()),
            "{name}: {after:?}"
        );
    }
}

/// The upgrade's own swap: a ready volume's layer copied in as its main sidecar is a new
/// version of the main, read once by the next walk, which decides as a fresh census does;
/// a restart after it reads nothing.
#[test]
fn a_swapped_main_is_judged_at_its_new_version() {
    for (name, fake) in [
        ("drive", FakeBackend::drive_like()),
        ("mega", FakeBackend::mega_like()),
    ] {
        let mut t = Cloud::start(fake);
        let (first, _, _) = t.walk();
        assert_eq!(first.ready, ["B/Vol 1.cbz"], "{name}");
        let cbz = t.root.join("B/Vol 1.cbz");
        let layer = t.root.join("B/Vol 1.fast.mokuro");
        t.up().direct_replace(&cbz, &layer).unwrap();
        t.store().wait_steady(WAIT);
        t.sync();
        let (now, reads, _) = t.walk();
        eprintln!("{name}: after the swap: {reads:?}");
        assert!(now.ready.is_empty(), "{name}: {now:?}");
        let f = fresh(&t.root);
        assert_eq!((&now.owed, &now.ready), (&f.owed, &f.ready), "{name}");
        assert!(
            total(&reads) <= 1 && reads.keys().all(|r| r == "B/Vol 1.mokuro"),
            "{name}: at most the new main, once: {reads:?}"
        );
        t.stop();
        t.open();
        let (again, reads, _) = t.walk();
        assert!(reads.is_empty(), "{name}: after a restart: {reads:?}");
        let f = fresh(&t.root);
        assert_eq!((again.owed, again.ready), (f.owed, f.ready), "{name}");
    }
}

// ---- a local folder as the cloud store ---------------------------------------------

/// A local folder connected as the cloud store (`FolderBackend`), whose content version is
/// no file's identity: the file's mtime and size, as a WebDAV host's is without an ETag.
/// Every file here has one mtime (a library copied or unpacked with its timestamps), so a
/// current and an outdated layer of one size share a version: the store's effective mtime
/// (salted by the file's id) keeps one file's facts from ever being another's. Each walk
/// decides as a census that never saw the library; settings and a restart read nothing; a
/// file rewritten at its size is read once; a series folder renamed (new ids: the facts
/// are not found by version on a store of path ids) is judged on what is there.
#[test]
fn a_folder_store_with_weak_versions_judges_each_file_on_its_own_bytes() {
    use bunko_remote::folder::{FolderBackend, FolderConfig};
    type Store = Arc<RemoteStore<FolderBackend>>;
    let tmp = TempDir::new().unwrap();
    let disk = tmp.path().join("library-folder");
    let at = std::time::SystemTime::now() - Duration::from_secs(3600);
    let put = |rel: &str, bytes: &[u8], at: std::time::SystemTime| {
        let p = disk.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, bytes).unwrap();
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(at).unwrap();
    };
    let files = library();
    for (rel, bytes) in &files {
        put(rel, bytes, at);
    }
    let len = |rel: &str| files.iter().find(|(r, _)| r == rel).unwrap().1.len();
    assert_eq!(len("A/Vol 1.fast.mokuro"), len("B/Vol 2.fast.mokuro"));
    let root = tmp.path().join("cloud/s/library");
    let open = || -> (Store, MountGuard) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let backend = rt
            .block_on(FolderBackend::connect(FolderConfig {
                path: disk.clone(),
                root_name: FOLDER.into(),
                state_dir: tmp.path().join("cloud/s"),
                watch: false,
            }))
            .unwrap();
        drop(rt);
        let mut cfg = RemoteStoreConfig::new(root.clone(), tmp.path().join("cloud/s"), FOLDER);
        cfg.poll_active = Duration::from_secs(3600);
        cfg.poll_idle = Duration::from_secs(3600);
        cfg.lease = false;
        let store = RemoteStore::open_shared(cfg, Arc::new(backend)).unwrap();
        assert!(store.wait_steady(WAIT), "{:?}", store.engine_state());
        let mount = bunko_store::mount(store.clone() as Arc<dyn LibraryStore>);
        (store, mount)
    };
    // The store lists what the folder holds: `rel` there (or gone) at the size on disk.
    let sync = |store: &Store, rel: &str, there: bool| {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            store.feed().poll_now();
            assert!(store.wait_steady(WAIT));
            let listed = bunko_store::fs::metadata(root.join(rel))
                .ok()
                .map(|m| m.len());
            let disk = std::fs::metadata(disk.join(rel)).ok().map(|m| m.len());
            if listed == disk && listed.is_some() == there {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{rel}: {listed:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let version = |rel: &str| {
        bunko_store::fs::metadata(root.join(rel))
            .unwrap()
            .version()
            .cloned()
    };
    let (store, mount) = open();
    assert_eq!(
        version("A/Vol 1.fast.mokuro"),
        version("B/Vol 2.fast.mokuro"),
        "two files, one version"
    );
    let db = Arc::new(Database::open(tmp.path().join("mokuro.db")).unwrap());
    let up = census(&root, Some(db.clone()));
    let first = decide(&root, &up);
    assert_eq!(first.owed, expected(), "{first:?}");
    assert_eq!(first, fresh(&root));
    let read = up.sidecar_facts().reads();
    assert_eq!(read, 11, "each main and each `fast` layer once");
    up.configure(&upgrade_config(), &rows());
    assert_eq!(decide(&root, &up), first);
    assert_eq!(up.sidecar_facts().reads(), read, "nothing read again");

    // A restart.
    drop(up);
    drop(mount);
    drop(store);
    let (store, mount) = open();
    let up = census(&root, Some(db.clone()));
    assert_eq!(decide(&root, &up), first);
    assert_eq!(
        up.sidecar_facts().reads(),
        0,
        "nothing read after a restart"
    );

    // Rewritten at its size, a second later: an older revision's output now.
    let older = hayai(current() - 1, "fast", "S");
    assert_eq!(older.len(), len("A/Vol 1.fast.mokuro"));
    put("A/Vol 1.fast.mokuro", &older, at + Duration::from_secs(1));
    store.feed().poll_now();
    let deadline = std::time::Instant::now() + WAIT;
    while version("A/Vol 1.fast.mokuro") == version("A/Vol 2.fast.mokuro") {
        assert!(std::time::Instant::now() < deadline);
        store.feed().poll_now();
        std::thread::sleep(Duration::from_millis(20));
    }
    let now = decide(&root, &up);
    let mut want = expected();
    want.insert("A/Vol 1.cbz".into(), owed(&["g-2"], &["g-2"], false));
    assert_eq!(now.owed, want);
    assert_eq!(now, fresh(&root));
    assert_eq!(up.sidecar_facts().reads(), 1, "the new version only");

    // A series folder renamed.
    drop(up);
    drop(mount);
    drop(store);
    std::fs::rename(disk.join("A"), disk.join("C")).unwrap();
    let (store, _mount) = open();
    sync(&store, "C/Vol 1.fast.mokuro", true);
    sync(&store, "A/Vol 1.fast.mokuro", false);
    let up = census(&root, Some(db.clone()));
    let moved = decide(&root, &up);
    let now = fresh(&root);
    assert_eq!((&moved.owed, &moved.ready), (&now.owed, &now.ready));
    assert!(moved.owed.contains_key("C/Vol 1.cbz"), "{moved:?}");
    eprintln!(
        "folder store: first walk {read} reads; after the series folder moved {}",
        up.sidecar_facts().reads()
    );
    assert!(
        up.sidecar_facts().reads() <= 7,
        "at most the moved series' sidecars"
    );
    up.sidecar_facts().prune();
    let kept: Vec<String> = db
        .sidecar_facts_keys()
        .unwrap()
        .into_iter()
        .map(|k| k.sidecar_path)
        .collect();
    assert!(kept.iter().all(|p| !p.starts_with("A/")), "{kept:?}");
}

// ---- a local library ---------------------------------------------------------------

/// The same on a local library: the facts outlive a settings change and a restart (in
/// `mokuro.db`), a file rewritten is read again, a removed or renamed one is pruned.
#[test]
fn a_local_walk_reads_each_sidecar_once_per_version() {
    let tmp = TempDir::new().unwrap();
    let lib = tmp.path().join("library");
    // Written a while ago: a file written a moment ago is read each time until it is
    // older than its file system's clock tick (`sidecar_facts::RACY`).
    let write = |path: &Path, bytes: &[u8], ago: u64| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(ago))
            .unwrap();
    };
    for (rel, bytes) in library() {
        write(&lib.join(rel), &bytes, 3600);
    }
    let open = || Arc::new(Database::open(tmp.path().join("mokuro.db")).unwrap());
    let up = census(&lib, Some(open()));
    let first = decide(&lib, &up);
    assert_eq!(first.owed, expected());
    assert_eq!(first.ready, ["B/Vol 1.cbz"]);
    assert_eq!(first, fresh(&lib));
    let read = up.sidecar_facts().reads();
    eprintln!("local: the first walk read {read} sidecars");
    assert_eq!(read, 11, "each main and each `fast` layer once");

    up.configure(&upgrade_config(), &rows());
    assert_eq!(decide(&lib, &up), first);
    assert_eq!(up.sidecar_facts().reads(), read, "nothing read again");

    // A restart.
    drop(up);
    let up = census(&lib, Some(open()));
    assert_eq!(decide(&lib, &up), first);
    assert_eq!(
        up.sidecar_facts().reads(),
        0,
        "nothing read after a restart"
    );

    // Rewritten in place.
    write(
        &lib.join("A/Vol 1.fast.mokuro"),
        &hayai(current() - 1, "fast", "S, rewritten"),
        1800,
    );
    let now = decide(&lib, &up);
    let mut want = expected();
    want.insert("A/Vol 1.cbz".into(), owed(&["g-2"], &["g-2"], false));
    assert_eq!(now.owed, want);
    assert_eq!(now, fresh(&lib));
    assert_eq!(up.sidecar_facts().reads(), 1, "the new version only");

    // Removed, and renamed in case.
    std::fs::remove_file(lib.join("B/Vol 2.fast.mokuro")).unwrap();
    std::fs::rename(lib.join("A/Vol 2.mokuro"), lib.join("A/vol 2.mokuro")).unwrap();
    let now = decide(&lib, &up);
    want.insert("B/Vol 2.cbz".into(), owed(&["g-2"], &[], false));
    assert_eq!(now.owed, want);
    assert_eq!(now, fresh(&lib));
    assert!(up.sidecar_facts().reads() <= 2, "at most the renamed file");
    let db = open();
    let kept = || -> Vec<String> {
        db.sidecar_facts_keys()
            .unwrap()
            .into_iter()
            .map(|k| k.sidecar_path)
            .collect()
    };
    assert!(kept().contains(&"B/Vol 2.fast.mokuro".to_string()));
    assert!(up.sidecar_facts().prune() >= 1);
    let after = kept();
    assert!(
        !after.contains(&"B/Vol 2.fast.mokuro".to_string()),
        "{after:?}"
    );
    // On a host that folds names the old spelling still reaches the same file.
    if std::fs::metadata(lib.join("A/Vol 2.mokuro")).is_err() {
        assert!(!after.contains(&"A/Vol 2.mokuro".to_string()), "{after:?}");
    }
    assert!(
        after.contains(&"A/Vol 1.fast.mokuro".to_string()),
        "{after:?}"
    );
}
