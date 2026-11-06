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
    env_tuned(backend, fake, tune).await
}

async fn env_tuned(
    backend: LibraryBackend,
    fake: Option<Arc<FakeBackend>>,
    tune: bunko_server::storage::testing::Tune,
) -> Env {
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

/// A correction layer, `text` on its one block.
fn correction(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "pages": [{"img_path": "001.png", "blocks": [{"lines": [text]}]}],
    }))
    .unwrap()
}

/// Personal corrections (D27) in a cloud library: the progress user's is the cloud
/// folder's own `Vol 1.edit.mokuro` (a cloud-direct reader shares it), every other
/// account's stays on this disk. Neither is library content: no upload record, no OCR
/// row of the volume forgotten, no provenance rewrite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrections_are_the_owners_cloud_file_and_everyone_elses_overlay() {
    let fake = library_fake();
    let primary = br#"{"pages": [], "volume": "Vol 1"}"#;
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.mokuro"), primary);
    let e = env(LibraryBackend::GDrive, Some(fake.clone())).await;
    let db = e.services.db.clone();
    db.record_ocr_sidecar(&bunko_db::OcrSidecar {
        sidecar_path: "Series/Vol 1.mokuro".into(),
        volume_key: "Series/Vol 1.cbz".into(),
        generation_id: "g-1".into(),
        generation_name: "Primary".into(),
        machine: "local".into(),
        ..Default::default()
    })
    .unwrap();
    let edit = "/mokuro-reader/Series/Vol%201.edit.mokuro";

    let (status, body) = e
        .req(Some("other"), "PUT", edit, &[], &correction("other"))
        .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let (status, body) = e
        .req(Some("owner"), "PUT", edit, &[], &correction("owner"))
        .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    // The owner's goes out like any sidecar; the other account's never reaches the
    // store (it was written first, so it would be there by now).
    eventually("the owner's correction reaches the cloud", || {
        cloud_bytes(&fake, "Series/Vol 1.edit.mokuro").is_some()
    })
    .await;
    assert_eq!(
        cloud_bytes(&fake, "Series/Vol 1.edit.mokuro").unwrap(),
        correction("owner")
    );
    assert_eq!(
        std::fs::read(e.users("other/edits/Series/Vol 1.edit.mokuro")).unwrap(),
        correction("other")
    );
    assert!(!e.users("owner").exists());
    for (user, body) in [("owner", "owner"), ("other", "other")] {
        assert_eq!(e.get(user, edit).await, (200, correction(body)), "{user}");
    }
    assert_eq!(e.get("boss", edit).await.0, 404);
    assert_eq!(e.req(None, "GET", edit, &[], b"").await.0, 404);
    // Each account lists its own, at every depth; the shared listing holds neither.
    for depth in ["1", "infinity"] {
        for user in ["owner", "other"] {
            let list = e.propfind(user, "/mokuro-reader/Series/", depth).await;
            assert_eq!(
                list.matches("Vol%201.edit.mokuro").count(),
                1,
                "{user}: {list}"
            );
        }
        let list = e.propfind("boss", "/mokuro-reader/Series/", depth).await;
        assert!(!list.contains(".edit."), "{list}");
    }
    let list = e.propfind("owner", "/mokuro-reader/", "infinity").await;
    assert_eq!(list.matches("Vol%201.edit.mokuro").count(), 1, "{list}");
    let list = e.propfind("boss", "/mokuro-reader/", "infinity").await;
    assert!(!list.contains(".edit."), "{list}");

    // The volume's records are untouched.
    let row = db.get_ocr_sidecar("Series/Vol 1.mokuro").unwrap();
    assert_eq!(row.map(|r| r.generation_id).as_deref(), Some("g-1"));
    assert_eq!(cloud_bytes(&fake, "Series/Vol 1.mokuro").unwrap(), primary);
    let audits = db.list_audit_events(100, None).unwrap();
    let mine: Vec<_> = audits
        .iter()
        .filter(|a| {
            a.target_path
                .as_deref()
                .is_some_and(|p| p.ends_with("Vol 1.edit.mokuro"))
        })
        .collect();
    assert_eq!(mine.len(), 2, "{audits:?}");
    assert!(
        mine.iter()
            .all(|a| a.action == "upload" && a.target_type.as_deref() == Some("personal")),
        "{mine:?}"
    );
}

/// Another account's corrections follow changes made directly in the cloud folder (a
/// cloud-direct reader renaming or deleting): an archive rename, a folder rename, an
/// archive delete and a folder delete each carry or drop them (B3.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrections_follow_changes_made_in_the_cloud() {
    let fake = library_fake();
    fake.reader_put(&format!("{ROOT}/Series/Vol 3.cbz"), b"three");
    // A removal in the cloud is applied once its tombstone grace is over.
    fn quick(c: &mut bunko_remote::RemoteStoreConfig) {
        tune(c);
        c.quiet = Duration::from_millis(300);
        c.poll_active = Duration::from_millis(100);
        c.poll_idle = Duration::from_millis(100);
    }
    let e = env_tuned(LibraryBackend::GDrive, Some(fake.clone()), quick).await;
    let store = e
        .services
        .storage
        .as_ref()
        .unwrap()
        .fake_store()
        .unwrap()
        .clone();
    for vol in ["Vol%201", "Vol%203"] {
        let path = format!("/mokuro-reader/Series/{vol}.edit.mokuro");
        let (status, _) = e
            .req(Some("other"), "PUT", &path, &[], &correction(vol))
            .await;
        assert_eq!(status, 201, "{path}");
    }
    let overlay = |rel: &str| e.users(&format!("other/edits/{rel}"));

    fake.reader_rename(
        &format!("{ROOT}/Series/Vol 1.cbz"),
        &format!("{ROOT}/Series/Vol 2.cbz"),
    );
    store.feed().poll_now();
    eventually("the correction follows the archive", || {
        overlay("Series/Vol 2.edit.mokuro").exists()
    })
    .await;
    assert!(!overlay("Series/Vol 1.edit.mokuro").exists());
    assert_eq!(
        std::fs::read(overlay("Series/Vol 2.edit.mokuro")).unwrap(),
        correction("Vol%201")
    );

    fake.reader_rename(&format!("{ROOT}/Series"), &format!("{ROOT}/Renamed"));
    store.feed().poll_now();
    eventually("the corrections follow the folder", || {
        overlay("Renamed/Vol 2.edit.mokuro").exists()
            && overlay("Renamed/Vol 3.edit.mokuro").exists()
    })
    .await;
    assert!(!overlay("Series").exists());
    let (status, body) = e
        .get("other", "/mokuro-reader/Renamed/Vol%202.edit.mokuro")
        .await;
    assert_eq!((status, body), (200, correction("Vol%201")));

    fake.reader_delete(&format!("{ROOT}/Renamed/Vol 2.cbz"));
    store.feed().poll_now();
    eventually("the correction goes with its archive", || {
        !overlay("Renamed/Vol 2.edit.mokuro").exists()
    })
    .await;
    assert!(overlay("Renamed/Vol 3.edit.mokuro").exists());

    fake.reader_delete(&format!("{ROOT}/Renamed"));
    store.feed().poll_now();
    eventually("the corrections go with their folder", || {
        !overlay("Renamed").exists()
    })
    .await;
}

/// Bunko's own merge of two same-named folders (dedup) moves the younger one's
/// volumes into the canonical folder under the same path: every account's
/// corrections of them stay valid, and are served and listed beside them after.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrections_survive_a_duplicate_folder_merge() {
    let fake = library_fake();
    let root = fake.node_at(ROOT).unwrap().pid;
    let twin = fake.reader_mkdir_in(&root, "Series");
    fake.reader_put_in(&twin.pid, "Vol 3.cbz", b"three");
    fn quick(c: &mut bunko_remote::RemoteStoreConfig) {
        tune(c);
        c.quiet = Duration::from_millis(300);
        c.merge_interval = Duration::from_millis(50);
    }
    let e = env_tuned(LibraryBackend::GDrive, Some(fake.clone()), quick).await;
    let overlay = e.users("other/edits/Series");
    std::fs::create_dir_all(&overlay).unwrap();
    for vol in ["Vol 1", "Vol 3"] {
        std::fs::write(overlay.join(format!("{vol}.edit.mokuro")), correction(vol)).unwrap();
    }
    let f2 = fake.clone();
    eventually("the emptied twin goes to the trash", || {
        f2.trashed_log().contains(&twin.pid)
    })
    .await;
    let list = e.propfind("other", "/mokuro-reader/Series/", "1").await;
    for vol in ["Vol%201", "Vol%203"] {
        assert!(list.contains(&format!("{vol}.edit.mokuro")), "{list}");
        let (status, body) = e
            .get("other", &format!("/mokuro-reader/Series/{vol}.edit.mokuro"))
            .await;
        assert_eq!(status, 200);
        assert_eq!(body, correction(&vol.replace("%20", " ")));
    }
}

fn no_poll(c: &mut bunko_remote::RemoteStoreConfig) {
    tune(c);
    c.poll_active = Duration::from_secs(3600);
    c.poll_idle = Duration::from_secs(3600);
}

/// A cloud-direct reader rewrites the owner's positions.json behind Bunko's index, on a
/// provider with no pinned reads (WebDAV): the owner's GET, which sent no precondition,
/// is never a 412. It answers 503 with a short Retry-After while Bunko reads the
/// library again, and the new bytes after that.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_changed_behind_the_index_is_a_retry_not_a_412() {
    let fake = Arc::new(FakeBackend::webdav_plain());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), b"volume");
    fake.reader_put(&format!("{ROOT}/positions.json"), b"{\"v\":1}");
    let e = env_tuned(LibraryBackend::WebDav, Some(fake.clone()), no_poll).await;
    fake.reader_put(&format!("{ROOT}/positions.json"), b"{\"v\":2}");
    let resp = e
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mokuro-reader/positions.json")
                .header("host", "localhost:8080")
                .header("authorization", basic("owner"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = e.get("owner", "/mokuro-reader/positions.json").await;
        if status == 200 {
            assert_eq!(body, b"{\"v\":2}");
            break;
        }
        assert_eq!(status, 503);
        assert!(Instant::now() < until, "the new version is read");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A COPY whose source changed behind the index is the same retry as a GET: the client
/// sent no precondition, so it answers 503 with a short Retry-After, never 412, and
/// copies the new bytes once Bunko has listed the library again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_copy_of_a_file_changed_behind_the_index_is_a_retry_not_a_412() {
    let fake = Arc::new(FakeBackend::webdav_plain());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), b"volume");
    fake.reader_put(&format!("{ROOT}/Series/notes.txt"), b"one");
    let e = env_tuned(LibraryBackend::WebDav, Some(fake.clone()), no_poll).await;
    fake.reader_put(&format!("{ROOT}/Series/notes.txt"), b"two");
    let copy = || {
        Request::builder()
            .method("COPY")
            .uri("/mokuro-reader/Series/notes.txt")
            .header("host", "localhost:8080")
            .header("authorization", basic("boss"))
            .header(
                "destination",
                "http://localhost:8080/mokuro-reader/Series/notes-copy.txt",
            )
            .body(Body::empty())
            .unwrap()
    };
    let resp = e.app.clone().oneshot(copy()).await.unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let resp = e.app.clone().oneshot(copy()).await.unwrap();
        let status = resp.status().as_u16();
        if matches!(status, 201 | 204) {
            break;
        }
        assert_eq!(status, 503);
        assert!(Instant::now() < until, "the new version is copied");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (status, body) = e.get("boss", "/mokuro-reader/Series/notes-copy.txt").await;
    assert_eq!((status, body), (200, b"two".to_vec()));
}

/// A file over the blob cache's 4 MiB streams from ranged reads, so its first read only
/// happens once the body is asked for. It is made before the headers go out: a large
/// file changed behind the index answers 503 with a short Retry-After like a small
/// one, not a 200 whose body breaks off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_file_changed_behind_the_index_is_a_retry_before_any_header() {
    let fake = Arc::new(FakeBackend::webdav_plain());
    let big = |b: u8| vec![b; 5 << 20];
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), b"volume");
    fake.reader_put(&format!("{ROOT}/Series/big.bin"), &big(1));
    let e = env_tuned(LibraryBackend::WebDav, Some(fake.clone()), no_poll).await;
    fake.reader_put(&format!("{ROOT}/Series/big.bin"), &big(2));
    let resp = e
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mokuro-reader/Series/big.bin")
                .header("host", "localhost:8080")
                .header("authorization", basic("boss"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(
        resp.headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("1")
    );
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = e.get("boss", "/mokuro-reader/Series/big.bin").await;
        if status == 200 {
            assert!(body == big(2), "the new version, whole");
            break;
        }
        assert_eq!(status, 503);
        assert!(Instant::now() < until, "the new version is read");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// In a cloud library too: an archive only the filename fold makes `X.<layer>.cbz`
/// (`X.ed\u{200B}it.cbz`) is created by nobody, so once `X.cbz` is there every account's
/// correction of `X` (the cloud-root owner's in the cloud folder, the others' here) and
/// its controller's curated publish are written. Beside one made in the cloud, the
/// accounts whose corrections stay on this disk are never refused for it either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_the_fold_ties_to_a_layer_never_blocks_that_layer_in_the_cloud() {
    fn enc(path: &str) -> String {
        const SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b' ');
        percent_encoding::utf8_percent_encode(path, SET).to_string()
    }
    let cases = [
        ("X.ed\u{200B}it.cbz", "X.edit.mokuro"),
        ("X.ed\u{131}t.cbz", "X.edit.mokuro"),
        ("X.edit-\u{200B}2.cbz", "X.edit-2.mokuro"),
        ("X.cura\u{200B}ted.cbz", "X.curated.mokuro"),
    ];
    let fake = library_fake();
    // Shared folders, made in the cloud: anyone may add to them.
    for i in 0..cases.len() {
        fake.reader_put(&format!("{ROOT}/U{i}/notes.txt"), b"shared");
    }
    fake.reader_put(&format!("{ROOT}/H/X.ed\u{200B}it.cbz"), &cbz(1));
    let e = env(LibraryBackend::GDrive, Some(fake.clone())).await;
    for name in ["alice", "bob"] {
        e.services
            .db
            .create_user(name, "pass1234", Role::Uploader, UserStatus::Active, "")
            .unwrap();
    }
    let put = |user: &'static str, path: String, body: Vec<u8>| {
        let e = &e;
        async move {
            let (status, body) = e.req(Some(user), "PUT", &enc(&path), &[], &body).await;
            (status, String::from_utf8_lossy(&body).into_owned())
        }
    };
    for (i, (archive, layer)) in cases.iter().enumerate() {
        let folder = format!("/mokuro-reader/U{i}");
        let (status, body) = put("bob", format!("{folder}/{archive}"), cbz(1)).await;
        assert_eq!(status, 409, "bob PUT {archive:?}: {body}");
        assert!(body.contains("ambiguous-name"), "{body}");
        let (status, body) = put("alice", format!("{folder}/X.cbz"), cbz(1)).await;
        assert_eq!(status, 201, "alice PUT X.cbz: {body}");
        let path = format!("{folder}/{layer}");
        let users: &[&'static str] = if layer.contains("curated") {
            &["alice"]
        } else {
            &["owner", "other", "alice", "bob"]
        };
        for user in users {
            let mine = format!(r#"{{"pages":[],"by":"{user}"}}"#).into_bytes();
            let (status, body) = put(user, path.clone(), mine.clone()).await;
            assert!(
                matches!(status, 201 | 204),
                "{user} PUT {path:?} after {archive:?}: {status} {body}"
            );
            assert_eq!(
                e.get(user, &enc(&path)).await,
                (200, mine),
                "{user} {path:?}"
            );
        }
    }
    // Beside one made in the cloud: the corrections kept on this disk.
    let (status, body) = put("alice", "/mokuro-reader/H/X.cbz".into(), cbz(1)).await;
    assert_eq!(status, 201, "alice PUT H/X.cbz: {body}");
    for user in ["other", "alice", "bob"] {
        let mine = format!(r#"{{"pages":[],"by":"{user}"}}"#).into_bytes();
        let path = "/mokuro-reader/H/X.edit.mokuro".to_string();
        let (status, body) = put(user, path.clone(), mine.clone()).await;
        assert!(
            matches!(status, 201 | 204),
            "{user} PUT {path:?}: {status} {body}"
        );
        assert_eq!(e.get(user, &path).await, (200, mine), "{user}");
        assert!(e.users(&format!("{user}/edits/H/X.edit.mokuro")).exists());
    }
}

/// In a cloud library too: one volume name per filename fold, whichever came first
/// (`STRASSE.curated.cbz` and `Straße.cbz` are one volume name), and a pair made in
/// the cloud folder holds both volumes' files while it is read as ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_volume_name_per_fold_in_the_cloud() {
    fn enc(path: &str) -> String {
        const SET: &percent_encoding::AsciiSet = &percent_encoding::CONTROLS.add(b' ');
        percent_encoding::utf8_percent_encode(path, SET).to_string()
    }
    let (squat, volume) = ("STRASSE.curated.cbz", "Stra\u{df}e.cbz");
    let fake = library_fake();
    for folder in ["A", "B"] {
        fake.reader_put(&format!("{ROOT}/{folder}/notes.txt"), b"shared");
    }
    for name in [squat, volume] {
        fake.reader_put(&format!("{ROOT}/P/{name}"), &cbz(1));
    }
    let e = env(LibraryBackend::GDrive, Some(fake.clone())).await;
    for name in ["alice", "bob"] {
        e.services
            .db
            .create_user(name, "pass1234", Role::Uploader, UserStatus::Active, "")
            .unwrap();
    }
    let send = |user: &'static str, method: &'static str, path: String, body: Vec<u8>| {
        let e = &e;
        async move {
            let (status, body) = e.req(Some(user), method, &enc(&path), &[], &body).await;
            (status, String::from_utf8_lossy(&body).into_owned())
        }
    };
    let layer = br#"{"pages":[]}"#.to_vec();
    // The squatter first: the volume is refused, the squatter keeps its archive.
    let (status, body) = send("bob", "PUT", format!("/mokuro-reader/A/{squat}"), cbz(1)).await;
    assert_eq!(status, 201, "bob PUT {squat:?}: {body}");
    let (status, body) = send("alice", "PUT", format!("/mokuro-reader/A/{volume}"), cbz(1)).await;
    assert_eq!(status, 409, "alice PUT {volume:?}: {body}");
    assert!(body.contains("ambiguous-name"), "{body}");
    let (status, body) = send("bob", "DELETE", format!("/mokuro-reader/A/{squat}"), vec![]).await;
    assert_eq!(status, 204, "bob DELETE {squat:?}: {body}");
    // The volume first: the squatter is refused, the volume's layers are written.
    let (status, body) = send("alice", "PUT", format!("/mokuro-reader/B/{volume}"), cbz(1)).await;
    assert_eq!(status, 201, "alice PUT {volume:?}: {body}");
    let (status, body) = send("alice", "PUT", format!("/mokuro-reader/B/{squat}"), cbz(1)).await;
    assert_eq!(status, 409, "alice PUT {squat:?}: {body}");
    assert!(body.contains("ambiguous-name"), "{body}");
    let (status, body) = send(
        "alice",
        "PUT",
        "/mokuro-reader/B/Stra\u{df}e.curated.mokuro".into(),
        layer.clone(),
    )
    .await;
    assert_eq!(status, 201, "alice PUT the curated layer: {body}");
    // A pair made in the cloud: read, never written to.
    for name in [squat, volume] {
        let (status, _) = send("boss", "GET", format!("/mokuro-reader/P/{name}"), vec![]).await;
        assert_eq!(status, 200, "GET {name:?}");
    }
    let (status, body) = send(
        "boss",
        "PUT",
        "/mokuro-reader/P/Stra\u{df}e.curated.mokuro".into(),
        layer.clone(),
    )
    .await;
    assert_eq!(status, 409, "PUT beside the pair: {body}");
    assert!(body.contains("ambiguous-name"), "{body}");
    assert!(
        fake.node_at(&format!("{ROOT}/P/Stra\u{df}e.curated.mokuro"))
            .is_none()
    );
}
