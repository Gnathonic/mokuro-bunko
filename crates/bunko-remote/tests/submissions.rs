//! Submissions and first adoption (plan M2 task 8; spec §9.3, §9.4; D8, D18):
//! FakeBackend, real temp dirs, real time, a test server whose hooks merge
//! `series.json` facts newest-wins and recompile.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_remote::{CloudHooks, RemoteStore, RemoteStoreConfig, SubmissionResult};
use bunko_store::{Durability, LibraryStore, Mode, MountGuard, Origin, Version, WriteIntent, fs};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tempfile::TempDir;

const FOLDER: &str = "mokuro-reader";
const WAIT: Duration = Duration::from_secs(20);

type Store = RemoteStore<FakeBackend>;
type Before<'a> = &'a dyn Fn(&Arc<Store>, &Path, &Path);

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < WAIT, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One `series_submission` call: the series, the body, and how many uploads the
/// provider had seen by then.
#[derive(Debug, Clone)]
struct Call {
    series: String,
    body: Value,
    uploads: usize,
}

/// The server's side: a facts table, merged newest-wins (by `updated_at`), and a
/// compiler that writes it back as `series.json`.
struct Server {
    root: PathBuf,
    scratch: PathBuf,
    fake: Arc<FakeBackend>,
    facts: Mutex<BTreeMap<String, Map<String, Value>>>,
    calls: Mutex<Vec<Call>>,
    refuse: Mutex<HashSet<String>>,
    recompile: AtomicBool,
    seq: AtomicUsize,
}

impl Server {
    fn series_of(dir: &Path) -> String {
        dir.file_name().unwrap().to_string_lossy().into_owned()
    }

    /// The compile of one series: its facts as `series.json`, over the version
    /// the index shows (D8).
    fn compile(&self, series: &str) {
        let body = {
            let facts = self.facts.lock();
            serde_json::to_vec(&facts.get(series).cloned().unwrap_or_default()).unwrap()
        };
        let dest = self.root.join(series).join("series.json");
        let base = fs::metadata(&dest).ok().and_then(|m| m.version().cloned());
        let tmp = self.scratch.join(format!(
            "compile-{}",
            self.seq.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&tmp, &body).unwrap();
        let w = WriteIntent::new(
            Mode::Replace { base },
            Origin::Compiler,
            Durability::Deferred,
        );
        fs::publish(&tmp, &dest, &w).unwrap();
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().clone()
    }
}

impl CloudHooks for Server {
    fn moved(&self, _from: &Path, _to: &Path, _is_dir: bool) {}
    fn removed(&self, _path: &Path, _is_dir: bool) {}
    fn sidecar_overwritten(&self, _path: &Path) {}
    fn ocr_discarded(&self, _path: &Path) {}

    fn series_submission(&self, dir: &Path, bytes: &[u8]) -> SubmissionResult {
        let series = Self::series_of(dir);
        let parsed: Option<Map<String, Value>> = serde_json::from_slice(bytes).ok();
        self.calls.lock().push(Call {
            series: series.clone(),
            body: parsed.clone().map(Value::Object).unwrap_or(Value::Null),
            uploads: self.fake.calls(FakeOp::Upload),
        });
        if self.refuse.lock().contains(&series) {
            return SubmissionResult::Refused("refused by the test server".into());
        }
        let Some(incoming) = parsed else {
            return SubmissionResult::Refused("unparsable".into());
        };
        {
            let mut facts = self.facts.lock();
            let stored = facts.entry(series.clone()).or_default();
            let stamp = |m: &Map<String, Value>| {
                m.get("updated_at")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let newer = stamp(&incoming) >= stamp(stored);
            for (k, v) in incoming {
                if newer || !stored.contains_key(&k) {
                    stored.insert(k, v);
                }
            }
        }
        if self.recompile.load(Ordering::Acquire) {
            self.compile(&series);
        }
        SubmissionResult::Applied
    }

    fn has_series_facts(&self, dir: &Path) -> bool {
        self.facts.lock().contains_key(&Self::series_of(dir))
    }
}

struct T {
    tmp: TempDir,
    root: PathBuf,
    fake: Arc<FakeBackend>,
    server: Arc<Server>,
    store: Option<Arc<Store>>,
    mount: Option<MountGuard>,
}

impl T {
    fn new(fake: FakeBackend, files: &[(&str, &[u8])]) -> T {
        T::with(fake, files, None)
    }

    /// `before_hooks(store, library root, scratch dir)` runs after the store opened
    /// and before the server installs its hooks.
    fn with(fake: FakeBackend, files: &[(&str, &[u8])], before_hooks: Option<Before<'_>>) -> T {
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
        let fake = Arc::new(fake);
        let server = Arc::new(Server {
            root: root.clone(),
            scratch: tmp.path().to_path_buf(),
            fake: fake.clone(),
            facts: Mutex::default(),
            calls: Mutex::default(),
            refuse: Mutex::default(),
            recompile: AtomicBool::new(true),
            seq: AtomicUsize::new(0),
        });
        let mut t = T {
            tmp,
            root,
            fake,
            server,
            store: None,
            mount: None,
        };
        t.start(before_hooks);
        t
    }

    fn start(&mut self, before_hooks: Option<Before<'_>>) {
        let mut cfg = RemoteStoreConfig::new(&self.root, self.tmp.path().join("cloud/s"), FOLDER);
        cfg.poll_active = Duration::from_secs(3600);
        cfg.poll_idle = Duration::from_secs(3600);
        cfg.lease = false;
        cfg.retry_base = Duration::from_millis(20);
        let store = RemoteStore::open_shared(cfg, self.fake.clone()).unwrap();
        self.mount = Some(bunko_store::mount(store.clone() as Arc<dyn LibraryStore>));
        if let Some(f) = before_hooks {
            f(&store, &self.root, self.tmp.path());
        }
        store.set_hooks(self.server.clone());
        self.store = Some(store);
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
        for s in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(s);
        }
        p
    }

    fn adopted(&self) {
        assert!(self.s().wait_steady(WAIT));
        wait_until("first adoption", || self.s().adopted());
        self.idle();
    }

    fn idle(&self) {
        assert!(self.s().wait_outbox_idle(WAIT), "{:?}", self.s().outbox());
    }

    fn sync_feed(&self) {
        let n = self.fake.calls(FakeOp::Changes);
        self.s().feed().poll_now();
        wait_until("a feed poll", || self.fake.calls(FakeOp::Changes) > n);
        std::thread::sleep(Duration::from_millis(100));
    }

    fn remote_json(&self, rel: &str) -> Value {
        let n = self.fake.node_at(&format!("{FOLDER}/{rel}")).unwrap();
        serde_json::from_slice(&self.fake.content(&n.pid).unwrap()).unwrap()
    }
}

fn series(updated: &str, extra: Value) -> Vec<u8> {
    let mut m = json!({"version": 2, "updated_at": updated});
    for (k, v) in extra.as_object().unwrap() {
        m[k] = v.clone();
    }
    serde_json::to_vec(&m).unwrap()
}

#[test]
fn reader_series_write_between_preflight_and_commit_is_merged() {
    let first = series(
        "2026-01-01T00:00:00.000Z",
        json!({"titles": {"native": "A"}}),
    );
    let mut t = T::new(
        FakeBackend::drive_like(),
        &[("S/v.cbz", b"zip"), ("S/series.json", &first)],
    );
    // Adoption hands the reader's file to the server, whose recompile lands after it.
    t.adopted();
    assert_eq!(t.server.calls().len(), 1);
    assert_eq!(t.remote_json("S/series.json")["titles"]["native"], "A");
    // The server learns a fact of its own and compiles.
    t.server.facts.lock().get_mut("S").unwrap().extend([
        ("tag".to_string(), json!("x")),
        ("updated_at".to_string(), json!("2026-02-01T00:00:00.000Z")),
    ]);
    let reader = series("2026-03-01T00:00:00.000Z", json!({"unit": "volumes"}));
    // The reader writes series.json while that compile is in PREFLIGHT.
    let armed = Arc::new(AtomicBool::new(true));
    {
        let (fake, armed, reader) = (t.fake.clone(), armed.clone(), reader.clone());
        t.fake.set_call_hook(Some(Arc::new(move |op| {
            if op == FakeOp::FindNamed && armed.swap(false, Ordering::AcqRel) {
                fake.reader_put(&format!("{FOLDER}/S/series.json"), &reader);
            }
        })));
    }
    t.server.compile("S");
    t.idle();
    assert!(
        !armed.load(Ordering::Acquire),
        "the compile reached PREFLIGHT"
    );
    // The reader's version was submitted, merged newest-wins, and recompiled on
    // top of it: nothing of either side is lost.
    let calls = t.server.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1].body,
        serde_json::from_slice::<Value>(&reader).unwrap()
    );
    let remote = t.remote_json("S/series.json");
    assert_eq!(remote["titles"]["native"], "A");
    assert_eq!(remote["tag"], "x");
    assert_eq!(remote["unit"], "volumes");
    assert_eq!(remote["updated_at"], "2026-03-01T00:00:00.000Z");
    let rows = t.s().submissions();
    assert!(rows.iter().all(|r| r.2 == "applied"), "{rows:?}");
    assert!(t.s().conflicts().is_empty(), "{:?}", t.s().conflicts());
    // The merged file is Bunko's own now; a restart changes nothing.
    t.stop();
    t.start(None);
    t.adopted();
    assert_eq!(t.server.calls().len(), 2);
}

#[test]
fn first_adoption_applies_all_series_json_before_any_compile_write() {
    let catalog = serde_json::to_vec(&json!({
        "version": 1, "updated_at": "2026-01-09T00:00:00.000Z",
        "series": [
            {"series_title": "A", "titles": {"native": "from the catalog"},
             "updated_at": "2026-01-01T00:00:00.000Z"},
            {"series_title": "c", "tag": "c-tag", "unit": "chapters",
             "updated_at": "2026-01-05T00:00:00.000Z"},
            {"series_title": "Gone", "tag": "nowhere", "updated_at": "2026-01-02T00:00:00.000Z"},
        ]
    }))
    .unwrap();
    let a = series(
        "2026-01-03T00:00:00.000Z",
        json!({"titles": {"native": "A"}}),
    );
    let b = series("2026-01-04T00:00:00.000Z", json!({"tag": "b"}));
    // A compile queued after the first sync but before the server's hooks (and so
    // the adoption) are in: it must wait for the adoption.
    let early = |s: &Arc<Store>, root: &Path, scratch: &Path| {
        assert!(s.wait_steady(WAIT));
        assert!(!s.adopted());
        let w = WriteIntent::new(
            Mode::Replace { base: None },
            Origin::Compiler,
            Durability::Deferred,
        );
        let tmp = scratch.join("early-catalog");
        std::fs::write(&tmp, b"{\"version\":1}").unwrap();
        fs::publish(&tmp, root.join("catalog.json"), &w).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(s.outbox()[0].state, "HELD", "{:?}", s.outbox());
    };
    let mut t = T::with(
        FakeBackend::drive_like(),
        &[
            ("A/series.json", &a),
            ("A/v.cbz", b"zip"),
            ("B/series.json", &b),
            ("C/v.cbz", b"zip"),
            ("catalog.json", &catalog),
        ],
        Some(&early),
    );
    assert_eq!(t.fake.calls(FakeOp::Upload), 0);
    t.adopted();
    let calls = t.server.calls();
    let order: Vec<&str> = calls.iter().map(|c| c.series.as_str()).collect();
    assert_eq!(order, vec!["A", "B", "C"], "{calls:?}");
    // Every fact went in before Bunko wrote anything.
    assert!(calls.iter().all(|c| c.uploads == 0), "{calls:?}");
    // The facts-only body synthesized from the catalog entry (module docs).
    assert_eq!(
        calls[2].body,
        json!({"version": 2, "updated_at": "2026-01-05T00:00:00.000Z",
               "tag": "c-tag", "unit": "chapters"})
    );
    assert_eq!(
        calls[0].body["titles"]["native"], "A",
        "series.json wins over the catalog"
    );
    let rows = t.s().submissions();
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows.iter().all(|r| r.2 == "applied"), "{rows:?}");
    // After the adoption the queued compiles went out.
    assert!(t.fake.upload_log().iter().any(|n| n == "catalog.json"));
    assert_eq!(t.remote_json("C/series.json")["tag"], "c-tag");
    // Adoption happens once per store.
    t.stop();
    t.start(None);
    t.adopted();
    assert_eq!(t.server.calls().len(), 3);
}

#[test]
fn a_refused_submission_is_a_conflict_and_the_compile_proceeds() {
    let t = T::new(FakeBackend::drive_like(), &[("S/v.cbz", b"zip")]);
    t.adopted();
    t.server.refuse.lock().insert("S".into());
    t.fake
        .reader_put(&format!("{FOLDER}/S/series.json"), b"not json at all");
    t.sync_feed();
    wait_until("the refusal", || {
        t.s().submissions().iter().any(|r| r.2 == "refused")
    });
    let conflicts = t.s().conflicts();
    assert!(
        conflicts
            .iter()
            .any(|c| c.0 == "lost-write" && c.1 == "S/series.json"),
        "{conflicts:?}"
    );
    // The compile overwrites the refused version (provider history keeps it).
    t.server.facts.lock().insert(
        "S".into(),
        Map::from_iter([("tag".to_string(), json!("ours"))]),
    );
    t.server.compile("S");
    t.idle();
    assert_eq!(t.remote_json("S/series.json")["tag"], "ours");
}

#[test]
fn a_restart_mid_fetch_resumes_the_submission() {
    let mut t = T::new(FakeBackend::drive_like(), &[("S/v.cbz", b"zip")]);
    t.adopted();
    t.s()
        .set_failpoint(Some(Arc::new(|at: &str| at == "SUBMISSION_FETCHING")));
    let body = series("2026-04-01T00:00:00.000Z", json!({"tag": "late"}));
    t.fake.reader_put(&format!("{FOLDER}/S/series.json"), &body);
    t.sync_feed();
    wait_until("the crash", || t.s().crashed());
    assert_eq!(t.s().submissions()[0].2, "fetching");
    assert!(t.server.calls().is_empty());
    t.stop();
    t.start(None);
    assert!(t.s().wait_steady(WAIT));
    wait_until("the resumed submission", || {
        t.s().submissions().iter().all(|r| r.2 == "applied")
    });
    let calls = t.server.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].body["tag"], "late");
    t.idle();
    assert_eq!(t.remote_json("S/series.json")["tag"], "late");
}

#[test]
fn compiles_of_a_series_wait_for_its_unanswered_submission() {
    let t = T::new(FakeBackend::drive_like(), &[("S/v.cbz", b"zip")]);
    t.adopted();
    t.server.recompile.store(false, Ordering::Release);
    // The reader's version arrives; its read is slow, the compile is already queued.
    t.fake.set_latency(Duration::from_millis(300));
    let reader = series("2026-05-01T00:00:00.000Z", json!({"tag": "reader"}));
    t.fake
        .reader_put(&format!("{FOLDER}/S/series.json"), &reader);
    t.s().feed().poll_now();
    wait_until("the submission", || !t.s().submissions().is_empty());
    // A base on the version still being fetched is refused (not merged yet, D8)…
    let base = fs::metadata(t.abs("S/series.json"))
        .unwrap()
        .version()
        .cloned()
        .map(|v| Version::new(v.as_str()));
    let tmp = t.tmp.path().join("c");
    std::fs::write(&tmp, b"{\"tag\":\"compiled\"}").unwrap();
    if t.s().submissions()[0].2 == "fetching" {
        let w = WriteIntent::new(
            Mode::Replace { base },
            Origin::Compiler,
            Durability::Deferred,
        );
        let e = fs::publish(&tmp, t.abs("S/series.json"), &w).unwrap_err();
        assert!(bunko_store::is_conflict(&e), "{e}");
    }
    // …and a compile without one waits for the answer.
    let w = WriteIntent::new(
        Mode::Replace { base: None },
        Origin::Compiler,
        Durability::Deferred,
    );
    fs::publish(&tmp, t.abs("S/series.json"), &w).unwrap();
    // While the submission is unanswered, the compile stays HELD.
    let held = t.s().outbox();
    if t.s().submissions()[0].2 != "applied" {
        assert!(matches!(held[0].state, "HELD" | "STAGED"), "{held:?}");
    }
    t.idle();
    assert_eq!(t.server.calls().len(), 1);
    let order = t.fake.upload_log();
    assert_eq!(order.last().map(String::as_str), Some("series.json"));
    assert_eq!(t.remote_json("S/series.json")["tag"], "compiled");
}
