//! The automatic case merge (0.8 §5.9) through the whole server: a cloud library on
//! the Drive-like `FakeBackend` holds `Kingdom/` and a younger `kingdom/`. The merge
//! moves the loser's volumes into the oldest spelling with the per-volume DB renames
//! (upload owner, OCR rows, volume id), holds the OCR queue for both spellings while
//! it is open, is audited and listed under "Recent merges"; a group it cannot join is
//! a `case-merge-blocked` conflict the admin resolves with "Keep that copy".

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::{TokenKind, UserStatus};
use bunko_remote::fake::{FakeBackend, FakeOp};
use bunko_server::app::{ServeOptions, Services, assemble};
use http::Request;
use parking_lot::Mutex;
use serde_json::Value;
use tower::ServiceExt;

const ROOT: &str = "mokuro-reader";

fn tune(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    // Long enough for the test to seed the DB rows before the first merge pass.
    c.quiet = Duration::from_millis(1500);
    c.merge_interval = Duration::from_millis(50);
}

struct Env {
    _dir: tempfile::TempDir,
    services: Services,
    app: Router,
    token: String,
}

async fn env(fake: Arc<FakeBackend>) -> Env {
    env_with(fake, |_| {}).await
}

async fn env_with(fake: Arc<FakeBackend>, configure: impl FnOnce(&mut Config)) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    bunko_server::storage::testing::register_tuned(&base, fake.clone(), Some(tune));
    let mut config = Config::default();
    config.storage.base_path = base;
    config.storage.library.backend = LibraryBackend::GDrive;
    config.catalog.enrich_community = false;
    configure(&mut config);
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    services
        .db
        .create_user("boss", "pass1234", Role::Admin, UserStatus::Active, "")
        .unwrap();
    let token = services
        .db
        .create_auth_token("boss", TokenKind::Web, "test", None)
        .unwrap()
        .0;
    let app = assemble(&services, &opts);
    Env {
        _dir: dir,
        services,
        app,
        token,
    }
}

impl Env {
    fn library(&self) -> PathBuf {
        self.services.core.layout.library()
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080")
            .header("authorization", format!("Bearer {}", self.token));
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let resp = self
            .app
            .clone()
            .oneshot(b.body(body).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_case_merge_carries_the_db_rows_per_volume_and_holds_the_queue() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Kingdom/Vol 1.cbz"), b"one");
    fake.reader_put(&format!("{ROOT}/kingdom/Vol 2.cbz"), b"two");
    fake.reader_put(&format!("{ROOT}/kingdom/Vol 2.mokuro"), b"{\"pages\":[]}");
    let e = env(fake.clone()).await;
    let db = e.services.db.clone();
    db.record_volume_upload("kingdom/Vol 2.cbz", "alice")
        .unwrap();
    db.remember_volume_uuid("kingdom/Vol 2.cbz", "uuid-2")
        .unwrap();
    db.record_ocr_sidecar(&bunko_db::OcrSidecar {
        sidecar_path: "kingdom/Vol 2.mokuro".into(),
        volume_key: "kingdom/Vol 2.cbz".into(),
        generation_id: "g-1".into(),
        generation_name: "Primary".into(),
        machine: "local".into(),
        ..Default::default()
    })
    .unwrap();
    // While the merge moves things, the scheduler's claim check says "not now" for
    // the volume under either spelling.
    let held: Arc<Mutex<Vec<bool>>> = Arc::default();
    let (h2, library) = (held.clone(), e.library());
    let row = bunko_core::generations::default_generation("g-1");
    fake.set_call_hook(Some(Arc::new(move |op| {
        if op == FakeOp::Update {
            for rel in ["kingdom/Vol 2.cbz", "Kingdom/Vol 1.cbz"] {
                h2.lock().push(bunko_server::ocr::owed::still_owed(
                    &library, rel, &row, false,
                ));
            }
        }
    })));
    let f2 = fake.clone();
    eventually("the merge", || {
        f2.node_at(&format!("{ROOT}/kingdom")).is_none()
    })
    .await;
    fake.set_call_hook(None);
    assert!(!held.lock().is_empty());
    assert!(held.lock().iter().all(|owed| !owed), "{:?}", held.lock());

    let names = |dir: &str| {
        let mut v: Vec<String> = bunko_store::fs::read_dir(e.library().join(dir))
            .unwrap()
            .flatten()
            .map(|d| d.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    let kingdom = names("Kingdom");
    for n in ["Vol 1.cbz", "Vol 2.cbz", "Vol 2.mokuro"] {
        assert!(kingdom.contains(&n.to_string()), "{kingdom:?}");
    }
    // Per volume: the owner, the OCR row and the volume id follow the archive.
    assert_eq!(
        db.get_volume_owner("Kingdom/Vol 2.cbz").unwrap().as_deref(),
        Some("alice")
    );
    assert_eq!(db.get_volume_owner("kingdom/Vol 2.cbz").unwrap(), None);
    let row = db.get_ocr_sidecar("Kingdom/Vol 2.mokuro").unwrap().unwrap();
    assert_eq!(row.volume_key, "Kingdom/Vol 2.cbz");
    assert!(
        db.get_ocr_sidecar("kingdom/Vol 2.mokuro")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        db.remembered_volume_uuid("Kingdom/Vol 2.cbz")
            .unwrap()
            .as_deref(),
        Some("uuid-2")
    );
    // Audited, and listed in the Storage tab.
    let audits = db.list_audit_events(100, None).unwrap();
    let a = audits
        .iter()
        .find(|a| a.action == "library_case_merged")
        .expect("audited");
    assert_eq!(a.target_path.as_deref(), Some("Kingdom"));
    let (status, st) = e.call("GET", "/_admin/api/storage", None).await;
    assert_eq!(status, 200, "{st}");
    let m = &st["recent_merges"][0];
    assert_eq!(m["winner"], "Kingdom", "{st}");
    assert_eq!(m["losers"][0], "kingdom");
    assert_eq!(m["state"], "done");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_that_copy_resolves_a_blocked_merge_through_the_admin_api() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Kingdom/Vol 1.cbz"), b"winner");
    fake.reader_put(&format!("{ROOT}/kingdom/Vol 1.cbz"), b"loser!");
    let e = env(fake.clone()).await;
    let mut conflict = Value::Null;
    let until = Instant::now() + Duration::from_secs(30);
    while conflict.is_null() {
        assert!(Instant::now() < until, "no case-merge-blocked conflict");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (_, st) = e.call("GET", "/_admin/api/storage", None).await;
        if let Some(c) = st["conflicts"]
            .as_array()
            .and_then(|a| a.iter().find(|c| c["kind"] == "case-merge-blocked"))
        {
            conflict = c.clone();
        }
    }
    assert_eq!(conflict["resolvable"], true, "{conflict}");
    assert_eq!(conflict["this"][0], "kingdom/Vol 1.cbz");
    assert_eq!(conflict["that"][0], "Kingdom/Vol 1.cbz");
    let id = conflict["id"].as_i64().unwrap();
    let (status, body) = e
        .call(
            "POST",
            &format!("/_admin/api/storage/conflicts/{id}/resolve"),
            Some(serde_json::json!({"choice": "sideways"})),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    let (status, body) = e
        .call(
            "POST",
            &format!("/_admin/api/storage/conflicts/{id}/resolve"),
            Some(serde_json::json!({"choice": "that"})),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let f2 = fake.clone();
    eventually("the loser folder goes", || {
        f2.node_at(&format!("{ROOT}/kingdom")).is_none()
    })
    .await;
    let w = fake.node_at(&format!("{ROOT}/Kingdom/Vol 1.cbz")).unwrap();
    assert_eq!(fake.content(&w.pid).unwrap(), b"winner");
    let (_, st) = e.call("GET", "/_admin/api/storage", None).await;
    assert!(
        st["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["kind"] != "case-merge-blocked"),
        "{st}"
    );
}

/// Differing main sidecars beside identical archives: the server's policy keeps the
/// one whose recipe matches the primary row (here the winner's, stamped by Bunko), and
/// the loser's stock mokuro file is kept as the winner's `pre-upgrade` layer first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_main_sidecar_matching_the_primary_recipe_is_kept() {
    let primary = bunko_core::generations::default_generation("g-1");
    let (engine, detector, budget) = primary.output_affecting();
    let stamped = serde_json::to_vec(&serde_json::json!({
        "version": "0.2.1", "pages": [],
        "ocr_engine": {"id": engine, "detector": detector, "patch_budget": budget},
    }))
    .unwrap();
    let stock =
        serde_json::to_vec(&serde_json::json!({"version": "0.2.1", "pages": [], "x": 1})).unwrap();
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Kingdom/Vol 1.cbz"), b"same");
    fake.reader_put(&format!("{ROOT}/Kingdom/Vol 1.mokuro"), &stamped);
    fake.reader_put(&format!("{ROOT}/kingdom/Vol 1.cbz"), b"same");
    fake.reader_put(&format!("{ROOT}/kingdom/Vol 1.mokuro"), &stock);
    let _e = env_with(fake.clone(), move |c| {
        c.ocr.upgrade.enabled = true;
        c.ocr.generations = vec![primary];
    })
    .await;
    let f2 = fake.clone();
    eventually("the merge", || {
        f2.node_at(&format!("{ROOT}/kingdom")).is_none()
    })
    .await;
    let bytes = |rel: &str| {
        let n = fake.node_at(&format!("{ROOT}/{rel}")).unwrap();
        fake.content(&n.pid).unwrap()
    };
    assert_eq!(bytes("Kingdom/Vol 1.mokuro"), stamped);
    let kept: Value = serde_json::from_slice(&bytes("Kingdom/Vol 1.pre-upgrade.mokuro")).unwrap();
    assert_eq!(kept["x"], 1);
    assert_eq!(kept["preserved"]["by"], "mokuro-bunko");
}
