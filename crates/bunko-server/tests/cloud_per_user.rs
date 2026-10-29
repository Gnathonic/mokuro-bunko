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

/// Each account's events file, under a device folder named for it: a listing or a read
/// served from the wrong account's tree shows the other name.
fn events(user: &str) -> String {
    format!("/mokuro-reader/history/dev-{user}/2026-10.events")
}

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

    /// MKCOL history and history/dev-<user> (either may exist), PUT the events file
    /// and positions.json.
    async fn write_tree(&self, user: &str, body: &[u8], positions: &[u8]) {
        let device = format!("/mokuro-reader/history/dev-{user}");
        for path in ["/mokuro-reader/history", device.as_str()] {
            let (status, _) = self.req(Some(user), "MKCOL", path, &[], b"").await;
            assert!(matches!(status, 201 | 405), "{user} MKCOL {path}: {status}");
        }
        let (status, body) = self.req(Some(user), "PUT", &events(user), &[], body).await;
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

/// A `.cbz` of `pages` small PNG pages (real images: the cover loop decodes them).
fn cbz(pages: usize) -> Vec<u8> {
    use std::io::Write as _;
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..pages {
        let img = image::RgbImage::from_fn(24, 32, |x, y| {
            image::Rgb([(x * 9) as u8, (y * 7) as u8, (i * 40) as u8])
        });
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        z.start_file(format!("{:03}.png", i + 1), opts).unwrap();
        z.write_all(&png).unwrap();
    }
    z.finish().unwrap().into_inner()
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
        cloud_bytes(&fake, "history/dev-owner/2026-10.events").as_deref()
            == Some(b"owner events".as_slice())
            && cloud_bytes(&fake, "positions.json").as_deref()
                == Some(b"owner positions".as_slice())
    })
    .await;
    assert!(!e.users("owner").exists());
    assert_eq!(
        std::fs::read(e.users("other/history/dev-other/2026-10.events")).unwrap(),
        b"other events"
    );
    assert!(fake.node_at(&format!("{ROOT}/history/dev-other")).is_none());
    assert_eq!(
        std::fs::read(e.users("other/positions.json")).unwrap(),
        b"other positions"
    );
    for (user, other, body, positions) in [
        ("owner", "other", "owner events", "owner positions"),
        ("other", "owner", "other events", "other positions"),
    ] {
        let got = e.get(user, &events(user)).await;
        assert_eq!(got, (200, body.as_bytes().to_vec()));
        assert_eq!(e.get(user, &events(other)).await.0, 404, "{user}");
        let got = e.get(user, "/mokuro-reader/positions.json").await;
        assert_eq!(got, (200, positions.as_bytes().to_vec()));
    }

    // Listings: each account sees its own tree only, at every depth.
    for depth in ["1", "infinity"] {
        for (user, other) in [("owner", "other"), ("other", "owner")] {
            let own = format!("/mokuro-reader/history/dev-{user}/");
            let foreign = format!("dev-{other}");
            let root = e.propfind(user, "/mokuro-reader/", depth).await;
            assert!(root.contains("/mokuro-reader/history/"), "{root}");
            assert!(root.contains("/mokuro-reader/Series/"), "{root}");
            assert!(!root.contains(&foreign), "{user} sees {foreign}: {root}");
            if depth == "infinity" {
                assert!(root.contains(&own), "{root}");
            }
            let history = e.propfind(user, "/mokuro-reader/history/", depth).await;
            assert!(history.contains(&own), "{history}");
            assert!(
                !history.contains(&foreign),
                "{user} sees {foreign}: {history}"
            );
            let device = e.propfind(user, &own, depth).await;
            assert!(device.contains("2026-10.events"), "{device}");
        }
    }
    let boss = e.propfind("boss", "/mokuro-reader/", "infinity").await;
    assert!(!boss.contains("history"), "{boss}");
    assert!(!boss.contains("positions.json"), "{boss}");
    for user in ["owner", "other"] {
        let (status, _) = e.get("boss", &events(user)).await;
        assert_eq!(status, 404);
    }

    // Not library content: no upload record, no series, no metadata pass.
    let db = e.services.db.clone();
    assert_eq!(
        db.get_volume_owner("history/dev-owner/2026-10.events")
            .unwrap(),
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
        std::fs::read(e.users("owner/history/dev-owner/2026-10.events")).unwrap(),
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

/// The history rule allows any name, `.cbz` included, and the owner's history sits
/// beside the series: an archive there is still that account's history. The watcher's
/// event for it owes no OCR, writes no cover into it, and drops no shared listing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_in_the_owners_history_is_no_volume() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), &cbz(2));
    let e = env(LibraryBackend::GDrive, Some(fake.clone())).await;
    let node = |rel: &str| fake.node_at(&format!("{ROOT}/{rel}"));
    eventually("the cover loop covers Vol 1", || {
        node("Series/Vol 1.webp").is_some()
    })
    .await;

    // The shared whole-library listing, cached.
    e.propfind("boss", "/mokuro-reader/", "infinity").await;
    let cache = e.services.dav.propfind_cache().clone();
    let cached = cache.usage().0;
    assert!(cached >= 1);

    let archive = cbz(2);
    for path in ["/mokuro-reader/history", "/mokuro-reader/history/dev-owner"] {
        let (status, _) = e.req(Some("owner"), "MKCOL", path, &[], b"").await;
        assert_eq!(status, 201, "MKCOL {path}");
    }
    let (status, body) = e
        .req(
            Some("owner"),
            "PUT",
            "/mokuro-reader/history/dev-owner/x.cbz",
            &[],
            &archive,
        )
        .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    eventually("the owner's archive reaches the cloud", || {
        cloud_bytes(&fake, "history/dev-owner/x.cbz").as_deref() == Some(archive.as_slice())
    })
    .await;
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        assert_eq!(
            cache.usage().0,
            cached,
            "a history write dropped the shared listing"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // A library volume after it: once its cover is written and its OCR is owed, the
    // archive announced before it has been looked at too.
    let (status, _) = e
        .req(
            Some("boss"),
            "PUT",
            "/mokuro-reader/Series/Vol%202.cbz",
            &[],
            &cbz(2),
        )
        .await;
    assert_eq!(status, 201);
    let library = e.services.core.layout.library();
    let ocr = e.services.ocr.clone();
    let vol2 = library.join("Series/Vol 2.cbz");
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        let owed = ocr
            .volume_outlook(&vol2)
            .await
            .is_some_and(|(pending, _)| !pending.is_empty());
        if owed && node("Series/Vol 2.webp").is_some() {
            break;
        }
        assert!(Instant::now() < until, "Vol 2 is covered and owes OCR");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let history = library.join("history/dev-owner/x.cbz");
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        let (pending, _) = ocr.volume_outlook(&history).await.unwrap();
        assert!(pending.is_empty(), "OCR owed for the history: {pending:?}");
        assert!(
            node("history/dev-owner/x.webp").is_none(),
            "a cover in the history"
        );
        assert!(node("history/dev-owner/x.nocover").is_none());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let (status, stats) = e
        .admin("GET", "/_admin/api/ocr/generations/stats", Value::Null)
        .await;
    assert_eq!(status, 200, "{stats}");
    for (id, row) in stats["generations"].as_object().unwrap() {
        assert!(
            row["volumes_total"].as_u64().is_none_or(|n| n <= 2),
            "{id}: {row}"
        );
    }
}
