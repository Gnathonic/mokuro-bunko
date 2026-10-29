//! A cloud library's progress user (0.8 §10) owns the cloud root's per-user files: its
//! `positions.json` and `history/**` are the cloud folder's own, shared with a reader
//! that reads the cloud directly. Every other account keeps its tree in `users/<u>/`
//! and never sees the owner's. Who owns the root is read from the live config on each
//! request, so the admin's change applies at once. None of it is library content: no
//! upload record, no metadata pass, no series.

// Fixtures are checked with std::fs on purpose (users/ is on this disk).
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::{TokenKind, UserStatus};
use bunko_remote::fake::FakeBackend;
use bunko_server::app::{ServeOptions, Services, assemble, start_library_services};
use http::Request;
use http_body_util::BodyExt;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const ROOT: &str = "mokuro-reader";
const EVENTS: &str = "/mokuro-reader/history/dev-1/2026-10.events";

fn tune(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    c.retry_base = Duration::from_millis(50);
}

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    app: Router,
    token: String,
    stop: CancellationToken,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

async fn env(backend: LibraryBackend, fake: Option<Arc<FakeBackend>>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    if let Some(fake) = &fake {
        bunko_server::storage::testing::register_tuned(&storage, fake.clone(), Some(tune));
    } else {
        std::fs::create_dir_all(storage.join("library/Series")).unwrap();
        std::fs::write(storage.join("library/Series/Vol 1.cbz"), b"volume").unwrap();
    }
    let mut config = Config::default();
    config.storage.base_path = storage.clone();
    config.storage.library.backend = backend;
    config.storage.library.progress_user = Some("owner".into());
    config.server.serve_readers = true;
    config.ocr.autobench = false;
    config.catalog.enrich_community = false;
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    for (name, role) in [
        ("boss", Role::Admin),
        ("owner", Role::Registered),
        ("other", Role::Registered),
    ] {
        services
            .db
            .create_user(name, "pass1234", role, UserStatus::Active, "")
            .unwrap();
    }
    let token = services
        .db
        .create_auth_token("boss", TokenKind::Web, "test", None)
        .unwrap()
        .0;
    if let Some(storage) = services.storage.clone() {
        let steady =
            tokio::task::spawn_blocking(move || storage.wait_steady(Duration::from_secs(20)))
                .await
                .unwrap();
        assert!(steady, "the first sync finishes");
    }
    let stop = CancellationToken::new();
    let _running = start_library_services(&services, stop.clone());
    let app = assemble(&services, &opts);
    Env {
        _dir: dir,
        storage,
        services,
        app,
        token,
        stop,
    }
}

impl Env {
    async fn req(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (u16, Vec<u8>) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080");
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        if !body.is_empty() {
            b = b.header("content-length", body.len().to_string());
        }
        let resp = self
            .app
            .clone()
            .oneshot(b.body(Body::from(body.to_vec())).unwrap())
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn admin(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", "localhost:8080")
                    .header("authorization", format!("Bearer {}", self.token))
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// MKCOL history and history/dev-1 (either may exist), PUT the events file and
    /// positions.json.
    async fn write_tree(&self, user: &str, events: &[u8], positions: &[u8]) {
        for path in ["/mokuro-reader/history", "/mokuro-reader/history/dev-1"] {
            let (status, _) = self.req(Some(user), "MKCOL", path, &[], b"").await;
            assert!(matches!(status, 201 | 405), "{user} MKCOL {path}: {status}");
        }
        let (status, body) = self.req(Some(user), "PUT", EVENTS, &[], events).await;
        assert!(
            matches!(status, 201 | 204),
            "{user} PUT history: {status} {}",
            String::from_utf8_lossy(&body)
        );
        let (status, _) = self
            .req(
                Some(user),
                "PUT",
                "/mokuro-reader/positions.json",
                &[],
                positions,
            )
            .await;
        assert!(
            matches!(status, 201 | 204),
            "{user} PUT positions: {status}"
        );
    }

    async fn get(&self, user: &str, path: &str) -> (u16, Vec<u8>) {
        self.req(Some(user), "GET", path, &[], b"").await
    }

    async fn propfind(&self, user: &str, path: &str, depth: &str) -> String {
        let (status, body) = self
            .req(Some(user), "PROPFIND", path, &[("depth", depth)], b"")
            .await;
        assert_eq!(status, 207, "{user} PROPFIND {path} depth {depth}");
        String::from_utf8(body).unwrap()
    }

    fn users(&self, rel: &str) -> PathBuf {
        self.storage.join("users").join(rel)
    }
}

fn cloud_bytes(fake: &FakeBackend, rel: &str) -> Option<Vec<u8>> {
    let node = fake.node_at(&format!("{ROOT}/{rel}"))?;
    fake.content(&node.pid)
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(60);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn library_fake() -> Arc<FakeBackend> {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), b"volume");
    fake
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_progress_user_owns_the_cloud_roots_files() {
    let fake = library_fake();
    let e = env(LibraryBackend::GDrive, Some(fake.clone())).await;
    let library = e.services.library.clone();
    // The startup pass and the one after it each wait out the 10 s debounce.
    eventually("the startup passes publish catalog.json", || {
        cloud_bytes(&fake, "catalog.json").is_some()
            && !library.has_pending_pass()
            && library.wait_idle(Duration::ZERO)
    })
    .await;
    let catalog = cloud_bytes(&fake, "catalog.json").unwrap();
    let series = cloud_bytes(&fake, "Series/series.json");
    let passes = library.passes_run();

    e.write_tree("owner", b"owner events", b"owner positions")
        .await;
    e.write_tree("other", b"other events", b"other positions")
        .await;

    // The owner's files are the cloud root's own; the other account's stay here.
    eventually("the owner's files reach the cloud", || {
        cloud_bytes(&fake, "history/dev-1/2026-10.events").as_deref()
            == Some(b"owner events".as_slice())
            && cloud_bytes(&fake, "positions.json").as_deref()
                == Some(b"owner positions".as_slice())
    })
    .await;
    assert!(!e.users("owner").exists());
    assert_eq!(
        std::fs::read(e.users("other/history/dev-1/2026-10.events")).unwrap(),
        b"other events"
    );
    assert_eq!(
        std::fs::read(e.users("other/positions.json")).unwrap(),
        b"other positions"
    );
    for (user, events, positions) in [
        ("owner", "owner events", "owner positions"),
        ("other", "other events", "other positions"),
    ] {
        assert_eq!(e.get(user, EVENTS).await, (200, events.as_bytes().to_vec()));
        let got = e.get(user, "/mokuro-reader/positions.json").await;
        assert_eq!(got, (200, positions.as_bytes().to_vec()));
    }

    // Listings: each account sees its own tree only, at every depth.
    for depth in ["1", "infinity"] {
        let other = e.propfind("other", "/mokuro-reader/", depth).await;
        assert!(other.contains("/mokuro-reader/history/"), "{other}");
        let other_events = e
            .propfind("other", "/mokuro-reader/history/dev-1/", depth)
            .await;
        assert!(other_events.contains("2026-10.events"), "{other_events}");
        let owner = e.propfind("owner", "/mokuro-reader/", depth).await;
        assert!(owner.contains("/mokuro-reader/history/"), "{owner}");
        assert!(owner.contains("/mokuro-reader/Series/"), "{owner}");
    }
    let boss = e.propfind("boss", "/mokuro-reader/", "infinity").await;
    assert!(!boss.contains("history"), "{boss}");
    assert!(!boss.contains("positions.json"), "{boss}");
    let (status, _) = e.get("boss", EVENTS).await;
    assert_eq!(status, 404);

    // Not library content: no upload record, no series, no metadata pass.
    let db = e.services.db.clone();
    assert_eq!(
        db.get_volume_owner("history/dev-1/2026-10.events").unwrap(),
        None
    );
    assert_eq!(db.get_volume_owner("positions.json").unwrap(), None);
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        assert!(
            !library.has_pending_pass(),
            "a history write scheduled a pass"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(library.passes_run(), passes);
    assert_eq!(cloud_bytes(&fake, "catalog.json").unwrap(), catalog);
    assert_eq!(cloud_bytes(&fake, "Series/series.json"), series);
    let (status, st) = e.admin("GET", "/_admin/api/status", Value::Null).await;
    assert_eq!(status, 200, "{st}");
    assert_eq!(st["volume_count"], 1, "{st}");
}

/// The admin's `PUT /api/storage/readers` moves the cloud root's files to another
/// account on the very next request, with no restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_progress_user_takes_effect_on_the_next_request() {
    let fake = library_fake();
    let e = env(LibraryBackend::GDrive, Some(fake.clone())).await;
    let put = |user: &'static str, body: &'static [u8]| {
        let e = &e;
        async move {
            let (status, _) = e
                .req(
                    Some(user),
                    "PUT",
                    "/mokuro-reader/positions.json",
                    &[],
                    body,
                )
                .await;
            assert!(matches!(status, 201 | 204), "{user}: {status}");
        }
    };
    put("owner", b"owner 1").await;
    let (status, body) = e
        .admin(
            "PUT",
            "/_admin/api/storage/readers",
            serde_json::json!({"serve_readers": true, "progress_user": "other"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    put("other", b"other 1").await;
    put("owner", b"owner 2").await;
    eventually("the new owner's file reaches the cloud", || {
        cloud_bytes(&fake, "positions.json").as_deref() == Some(b"other 1".as_slice())
    })
    .await;
    assert_eq!(
        std::fs::read(e.users("owner/positions.json")).unwrap(),
        b"owner 2"
    );
    assert!(!e.users("other").exists());
    let got = e.get("owner", "/mokuro-reader/positions.json").await;
    assert_eq!(got, (200, b"owner 2".to_vec()));
    let got = e.get("other", "/mokuro-reader/positions.json").await;
    assert_eq!(got, (200, b"other 1".to_vec()));
}

/// A library on this disk has no owner: progress_user is ignored, and the account it
/// names keeps its files in `users/<u>/` like everyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_library_ignores_the_progress_user() {
    let e = env(LibraryBackend::Local, None).await;
    e.write_tree("owner", b"owner events", b"owner positions")
        .await;
    assert_eq!(
        std::fs::read(e.users("owner/history/dev-1/2026-10.events")).unwrap(),
        b"owner events"
    );
    assert_eq!(
        std::fs::read(e.users("owner/positions.json")).unwrap(),
        b"owner positions"
    );
    let library = e.storage.join("library");
    assert!(!library.join("history").exists());
    assert!(!library.join("positions.json").exists());
}
