//! A server whose library is in a cloud folder (0.8 configurations b and c), mostly
//! the read side: the in-memory `FakeBackend` holds a series with one OCR'd and one
//! un-OCR'd volume. The bridge WebDAV lists and serves it (ranges included) and maps
//! provider failures to 503 + Retry-After, the catalog lists it, the admin status
//! reports the store, a rename made in the cloud carries the DB rows along, a store
//! that needs a sign-in holds the queue, and a local OCR claim reads a spooled copy of
//! the archive and installs into the cloud (or, refused, goes back to the queue
//! unrecorded). The scheduler keeps a cloud volume's failure records across scans,
//! and learns its page formats on a helper thread. The whole server's write side is
//! `cloud_e2e.rs`.

// Fixtures are checked with std::fs on purpose (the spool is a real local file).
#![allow(clippy::disallowed_methods)]

mod ocr_common;

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_proto::{Event, Op};
use bunko_remote::fake::FakeBackend;
use bunko_server::app::{ServeOptions, Services, assemble};
use bunko_server::ocr::sched::{Exec, Msg, SchedDeps, Scheduler};
use bunko_server::ocr::types::{FileFacts, Job, LibraryFacts};
use http::{Request, StatusCode};
use ocr_common::{exit, primary, ready, settings, started};
use tower::ServiceExt;

const ROOT: &str = "mokuro-reader";

/// A `.cbz` of `pages` stored members of `size` bytes each (stored: the archive is
/// exactly that large, so a 5 MiB page makes an archive past the small-file cache).
fn cbz(pages: usize, size: usize) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..pages {
        z.start_file(format!("{:03}.jpg", i + 1), opts).unwrap();
        let body: Vec<u8> = (0..size).map(|k| ((k * 31 + i * 7) % 251) as u8).collect();
        z.write_all(&body).unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn mokuro(uuid: &str, pages: usize) -> Vec<u8> {
    let pages: Vec<serde_json::Value> = (0..pages)
        .map(|i| {
            serde_json::json!({"img_path": format!("{:03}.jpg", i + 1), "img_width": 10, "img_height": 10, "blocks": []})
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "version": "0.2.5",
        "title": "Series",
        "title_uuid": "t-uuid",
        "volume": "Vol 1",
        "volume_uuid": uuid,
        "pages": pages,
    }))
    .unwrap()
}

struct Library {
    vol1: Vec<u8>,
    vol1_mokuro: Vec<u8>,
    vol2: Vec<u8>,
}

/// The reader's folder: `Series/Vol 1.cbz` (large) with its `.mokuro`, and
/// `Series/Vol 2.cbz` with none.
fn fake_library() -> (Arc<FakeBackend>, Library) {
    let fake = Arc::new(FakeBackend::drive_like());
    let lib = Library {
        vol1: cbz(1, 5 << 20),
        vol1_mokuro: mokuro("vol-1-uuid", 1),
        vol2: cbz(3, 1000),
    };
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), &lib.vol1);
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.mokuro"), &lib.vol1_mokuro);
    fake.reader_put(&format!("{ROOT}/Series/Vol 2.cbz"), &lib.vol2);
    (fake, lib)
}

fn cloud_config(base: PathBuf, serve_readers: bool) -> Config {
    let mut c = Config::default();
    c.storage.base_path = base;
    c.storage.library.backend = LibraryBackend::GDrive;
    c.server.serve_readers = serve_readers;
    c.catalog.enabled = true;
    c.catalog.enrich_community = false;
    c
}

struct Env {
    dir: tempfile::TempDir,
    services: Services,
    app: Router,
    lib: Library,
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

struct Resp {
    status: StatusCode,
    headers: http::HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

impl Env {
    async fn new(serve_readers: bool) -> Env {
        let (fake, lib) = fake_library();
        Env::with_fake(fake, lib, serve_readers).await
    }

    async fn with_fake(fake: Arc<FakeBackend>, lib: Library, serve_readers: bool) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        bunko_server::storage::testing::register(&base, fake);
        let config = cloud_config(base, serve_readers);
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        for (name, role) in [
            ("reader", Role::Registered),
            ("boss", Role::Admin),
            ("gpu", Role::Processor),
            ("alice", Role::Registered),
        ] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let storage = services.storage.clone().expect("a cloud library");
        assert!(storage.connected(), "{:?}", storage.problem());
        let steady =
            tokio::task::spawn_blocking(move || storage.wait_steady(Duration::from_secs(20)))
                .await
                .unwrap();
        assert!(steady, "the first sync finishes");
        let app = assemble(&services, &opts);
        Env {
            dir,
            services,
            app,
            lib,
        }
    }

    fn fake(&self) -> &Arc<bunko_remote::RemoteStore<FakeBackend>> {
        self.services
            .storage
            .as_ref()
            .unwrap()
            .fake_store()
            .expect("the fake store")
    }

    async fn send(&self, req: Request<Body>) -> Resp {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        Resp {
            status,
            headers,
            body,
        }
    }

    async fn request(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Resp {
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
        self.send(b.body(Body::empty()).unwrap()).await
    }
}

/// Wait (polling) until `f` holds.
async fn eventually(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bridge_lists_and_streams_a_cloud_library() {
    let env = Env::new(true).await;
    let layout_root = env.services.core.layout.library();
    let store = env.services.storage.as_ref().unwrap();
    assert_eq!(layout_root, store.root);
    assert!(
        layout_root.starts_with(env.dir.path().join("storage/cloud")),
        "{}",
        layout_root.display()
    );
    assert!(
        !layout_root.exists(),
        "the synthetic root is never created on disk"
    );
    assert!(!env.dir.path().join("storage/library").exists());

    // PROPFIND from the index.
    let listing = env
        .request(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/Series/",
            &[("depth", "1")],
        )
        .await;
    assert_eq!(
        listing.status,
        StatusCode::MULTI_STATUS,
        "{}",
        listing.text()
    );
    let xml = listing.text();
    for name in ["Vol%201.cbz", "Vol%201.mokuro", "Vol%202.cbz"] {
        assert!(xml.contains(name), "{name} in {xml}");
    }
    let root = env
        .request(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/",
            &[("depth", "1")],
        )
        .await;
    assert_eq!(root.status, StatusCode::MULTI_STATUS);
    assert!(
        root.text().contains("/mokuro-reader/Series/"),
        "{}",
        root.text()
    );

    // GET streams the large archive whole, with validators.
    let url = "/mokuro-reader/Series/Vol%201.cbz";
    let full = env.request(Some("reader"), "GET", url, &[]).await;
    assert_eq!(full.status, StatusCode::OK);
    assert_eq!(full.body.len(), env.lib.vol1.len());
    assert!(full.body == env.lib.vol1, "the archive's bytes");
    assert_eq!(full.header("accept-ranges"), Some("bytes"));
    assert!(full.header("x-accel-redirect").is_none());
    let etag = full.header("etag").expect("an ETag").to_string();
    let size = env.lib.vol1.len();

    // Range, and If-Range against the ETag.
    let part = env
        .request(
            Some("reader"),
            "GET",
            url,
            &[("range", "bytes=4000000-4000099")],
        )
        .await;
    assert_eq!(part.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        part.header("content-range"),
        Some(format!("bytes 4000000-4000099/{size}").as_str())
    );
    assert!(part.body == env.lib.vol1[4_000_000..4_000_100]);
    let held = env
        .request(
            Some("reader"),
            "GET",
            url,
            &[("range", "bytes=10-19"), ("if-range", &etag)],
        )
        .await;
    assert_eq!(held.status, StatusCode::PARTIAL_CONTENT);
    assert!(held.body == env.lib.vol1[10..20]);
    let stale = env
        .request(
            Some("reader"),
            "GET",
            url,
            &[("range", "bytes=10-19"), ("if-range", "\"1.000000-1\"")],
        )
        .await;
    assert_eq!(
        stale.status,
        StatusCode::OK,
        "a stale If-Range gets the whole file"
    );
    assert_eq!(stale.body.len(), size);
    let tail = env
        .request(Some("reader"), "GET", url, &[("range", "bytes=-5")])
        .await;
    assert_eq!(tail.status, StatusCode::PARTIAL_CONTENT);
    assert!(tail.body == env.lib.vol1[size - 5..]);
    let not_modified = env
        .request(Some("reader"), "GET", url, &[("if-none-match", &etag)])
        .await;
    assert_eq!(not_modified.status, StatusCode::NOT_MODIFIED);

    // A small file (the sidecar) comes from the blob cache.
    let sidecar = env
        .request(
            Some("reader"),
            "GET",
            "/mokuro-reader/Series/Vol%201.mokuro",
            &[],
        )
        .await;
    assert_eq!(sidecar.status, StatusCode::OK);
    assert!(sidecar.body == env.lib.vol1_mokuro);
    let small = env
        .request(
            Some("reader"),
            "GET",
            "/mokuro-reader/Series/Vol%202.cbz",
            &[("range", "bytes=0-3")],
        )
        .await;
    assert_eq!(small.status, StatusCode::PARTIAL_CONTENT);
    assert!(small.body == env.lib.vol2[..4]);

    // The catalog lists the series (from the index).
    let catalog = env.request(None, "GET", "/catalog/api/library", &[]).await;
    assert_eq!(catalog.status, StatusCode::OK, "{}", catalog.text());
    let v: serde_json::Value = serde_json::from_slice(&catalog.body).unwrap();
    let series = v["series"].as_array().unwrap();
    assert_eq!(series.len(), 1, "{v}");
    assert_eq!(series[0]["name"], "Series");
    assert_eq!(series[0]["volume_count"], 2);
    let page = env
        .request(None, "GET", "/catalog/api/series?name=Series", &[])
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.text());
    assert!(page.text().contains("Vol 2"), "{}", page.text());

    // The admin status reports the store.
    let status = env
        .request(Some("boss"), "GET", "/_admin/api/status", &[])
        .await;
    assert_eq!(status.status, StatusCode::OK, "{}", status.text());
    let v: serde_json::Value = serde_json::from_slice(&status.body).unwrap();
    let st = &v["library"]["storage"];
    assert_eq!(st["backend"], "gdrive");
    assert_eq!(st["folder"], "mokuro-reader");
    assert_eq!(st["connected"], true);
    assert_eq!(st["health"]["state"], "online");
    assert_eq!(st["engine"], "steady");
    assert_eq!(st["pairing"], "release");
    assert!(st["cache"]["blobs"].as_u64().unwrap() >= 1, "{st}");
    assert_eq!(v["volume_count"], 1);
}

/// Spec §3.5: a lapsed sign-in answers the bridge client 503 (never 401), and the
/// governor then holds every provider call until the admin signs in again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lapsed_sign_in_answers_503_and_holds_provider_calls() {
    use bunko_remote::BErr;
    use bunko_remote::fake::FakeOp;
    let env = Env::new(true).await;
    let fake = env.fake().backend().clone();
    let url = "/mokuro-reader/Series/Vol%202.cbz";
    fake.fail_next(FakeOp::Read, BErr::AuthRevoked);
    let r = env.request(Some("reader"), "GET", url, &[]).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());
    assert_eq!(r.header("retry-after"), Some("300"));
    assert!(r.text().contains("sign-in"), "{}", r.text());
    // Until a new sign-in, no request reaches the provider.
    fake.reset_calls();
    let r = env.request(Some("reader"), "GET", url, &[]).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());
    assert_eq!(fake.calls(FakeOp::Read), 0);
}

/// Spec §3.5 / §5.11: throttling answers 503 with the provider's Retry-After, and the
/// pause holds later interactive calls instead of hammering the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn throttling_answers_503_with_the_providers_retry_after() {
    use bunko_remote::BErr;
    use bunko_remote::fake::FakeOp;
    let env = Env::new(true).await;
    let fake = env.fake().backend().clone();
    let url = "/mokuro-reader/Series/Vol%202.cbz";
    fake.fail_next(FakeOp::Read, BErr::Throttled(Some(Duration::from_secs(42))));
    let r = env.request(Some("reader"), "GET", url, &[]).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());
    assert_eq!(r.header("retry-after"), Some("42"));
    fake.reset_calls();
    let r = env.request(Some("reader"), "GET", url, &[]).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());
    let after: u64 = r.header("retry-after").unwrap().parse().unwrap();
    assert!(after <= 42 && after > 0, "{after}");
    assert_eq!(fake.calls(FakeOp::Read), 0, "paused: no provider call");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_readers_only_processors_fetch_cloud_archives() {
    let env = Env::new(false).await;
    assert_eq!(
        env.request(None, "GET", "/catalog/api/library", &[])
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        env.request(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/",
            &[("depth", "1")]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        env.request(
            Some("reader"),
            "GET",
            "/mokuro-reader/Series/Vol%202.cbz",
            &[]
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    // A processor's archive fetch (PROTOCOL.md step 5), resumed with a range.
    let got = env
        .request(Some("gpu"), "GET", "/mokuro-reader/Series/Vol%201.cbz", &[])
        .await;
    assert_eq!(got.status, StatusCode::OK);
    assert!(got.body == env.lib.vol1);
    let etag = got.header("etag").unwrap().to_string();
    let resumed = env
        .request(
            Some("gpu"),
            "GET",
            "/mokuro-reader/Series/Vol%201.cbz",
            &[("range", "bytes=5000000-"), ("if-range", &etag)],
        )
        .await;
    assert_eq!(resumed.status, StatusCode::PARTIAL_CONTENT);
    assert!(resumed.body == env.lib.vol1[5_000_000..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rename_made_in_the_cloud_moves_the_db_rows() {
    let env = Env::new(true).await;
    let db = env.services.db.clone();
    db.record_volume_upload("Series/Vol 2.cbz", "alice")
        .unwrap();
    db.record_volume_upload("Series/Vol 1.cbz", "alice")
        .unwrap();
    db.record_ocr_sidecar(&bunko_db::OcrSidecar {
        sidecar_path: "Series/Vol 1.mokuro".into(),
        volume_key: "Series/Vol 1.cbz".into(),
        generation_id: "g-1".into(),
        generation_name: "default".into(),
        machine: "local".into(),
        ..Default::default()
    })
    .unwrap();
    let fake = env.fake().clone();
    // A whole-tree listing, cached (fresh for minutes).
    let tree = || {
        env.request(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/",
            &[("depth", "infinity")],
        )
    };
    let before = tree().await;
    assert_eq!(before.status, StatusCode::MULTI_STATUS);
    assert!(before.text().contains("Vol%202.cbz"));

    // A file rename in the reader.
    fake.backend().reader_rename(
        &format!("{ROOT}/Series/Vol 2.cbz"),
        &format!("{ROOT}/Series/Vol 2b.cbz"),
    );
    fake.feed().poll_now();
    eventually(
        "the volume's owner to follow the rename",
        Duration::from_secs(10),
        || {
            db.get_volume_owner("Series/Vol 2b.cbz")
                .ok()
                .flatten()
                .as_deref()
                == Some("alice")
        },
    )
    .await;
    assert_eq!(db.get_volume_owner("Series/Vol 2.cbz").unwrap(), None);
    // The store's event dropped the cached listing.
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let after = tree().await.text();
        if after.contains("Vol%202b.cbz") && !after.contains("Vol%202.cbz") {
            break;
        }
        assert!(
            Instant::now() < until,
            "the cached listing is stale: {after}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A folder rename carries every volume and sidecar row.
    fake.backend()
        .reader_rename(&format!("{ROOT}/Series"), &format!("{ROOT}/Renamed"));
    fake.feed().poll_now();
    eventually(
        "the folder's rows to follow the rename",
        Duration::from_secs(10),
        || {
            db.get_volume_owner("Renamed/Vol 1.cbz")
                .ok()
                .flatten()
                .as_deref()
                == Some("alice")
                && db
                    .get_volume_owner("Renamed/Vol 2b.cbz")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("alice")
                // The sidecar rows follow in their own step.
                && db
                    .get_ocr_sidecar("Renamed/Vol 1.mokuro")
                    .ok()
                    .flatten()
                    .is_some()
                && db
                    .get_ocr_sidecar("Series/Vol 1.mokuro")
                    .ok()
                    .flatten()
                    .is_none()
        },
    )
    .await;
    // The bridge sees the new spelling.
    let listing = env
        .request(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/Renamed/",
            &[("depth", "1")],
        )
        .await;
    assert_eq!(listing.status, StatusCode::MULTI_STATUS);
    assert!(
        listing.text().contains("Vol%202b.cbz"),
        "{}",
        listing.text()
    );
}

/// The library services start once the store is in sync; the cover loop works from the
/// store's events (no periodic walk), records a cover-less archive locally (D14: no
/// `.nocover` file in the cloud), and nothing appears on disk under the synthetic root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library_services_start_on_the_synced_store_and_record_nocover_locally() {
    let (fake, lib) = fake_library();
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    z.start_file("readme.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    z.write_all(b"no pictures here").unwrap();
    fake.reader_put(
        &format!("{ROOT}/Solo/NoPics.cbz"),
        &z.finish().unwrap().into_inner(),
    );
    let env = Env::with_fake(fake, lib, true).await;
    let stop = tokio_util::sync::CancellationToken::new();
    let running = bunko_server::app::start_library_services(&env.services, stop.clone());
    let root = env.services.core.layout.library();
    let record = root.parent().unwrap().join("nocover.json");
    eventually(
        "the missing cover recorded",
        Duration::from_secs(20),
        || std::fs::read_to_string(&record).is_ok_and(|t| t.contains("Solo/NoPics.cbz")),
    )
    .await;
    assert!(
        env.fake()
            .backend()
            .node_at(&format!("{ROOT}/Solo/NoPics.nocover"))
            .is_none(),
        "no marker file in the cloud"
    );
    assert!(!root.exists(), "nothing on disk under the synthetic root");
    assert!(!env.services.library.is_stopped());
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the services stop with the token")
        .unwrap();
}

/// No sign-in yet: the server still starts, with an empty library, and the admin
/// status says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconnected_cloud_library_serves_an_empty_library() {
    let dir = tempfile::tempdir().unwrap();
    let config = cloud_config(dir.path().join("storage"), true);
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    services
        .db
        .create_user("boss", "pass1234", Role::Admin, UserStatus::Active, "")
        .unwrap();
    let storage = services.storage.clone().unwrap();
    assert!(!storage.connected());
    let app = assemble(&services, &opts);
    let get = |method: &str, path: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080")
            .header("authorization", basic("boss"))
            .header("depth", "1")
            .body(Body::empty())
            .unwrap()
    };
    let r = app
        .clone()
        .oneshot(get("PROPFIND", "/mokuro-reader/"))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::MULTI_STATUS);
    let r = app
        .clone()
        .oneshot(get("GET", "/_admin/api/status"))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let st = &v["library"]["storage"];
    assert_eq!(st["connected"], false, "{st}");
    assert_eq!(st["health"]["state"], "needs-reauth");
    assert!(
        st["problem"]
            .as_str()
            .is_some_and(|p| p.contains("library connect gdrive")),
        "{st}"
    );
    assert!(!dir.path().join("storage/library").exists());
}

/// A cloud library opened and mounted outside a server, with the scheduler's deps.
struct CloudSched {
    _dir: tempfile::TempDir,
    layout: bunko_core::StorageLayout,
    remote: Arc<bunko_remote::RemoteStore<FakeBackend>>,
    _mount: bunko_store::MountGuard,
    _handle: Arc<bunko_server::storage::StoreHandle>,
}

fn cloud_sched() -> CloudSched {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let (fake, _) = fake_library();
    bunko_server::storage::testing::register(&base, fake);
    let config = cloud_config(base, true);
    let layout = config.storage.layout();
    layout.ensure_directories().unwrap();
    let (store, mount, handle) = bunko_server::storage::open_library_store(&config, &layout)
        .unwrap()
        .unwrap();
    assert!(handle.wait_steady(Duration::from_secs(20)));
    let layout = layout.with_library_root(store.root());
    let remote = handle.fake_store().unwrap().clone();
    CloudSched {
        _dir: dir,
        layout,
        remote,
        _mount: mount,
        _handle: handle,
    }
}

fn sched_over(layout: &bunko_core::StorageLayout, exec: Exec) -> Scheduler {
    sched_with(layout, exec, Arc::new(FileFacts)).0
}

/// [`sched_over`] with the library facts given, and its clock.
fn sched_with(
    layout: &bunko_core::StorageLayout,
    exec: Exec,
    facts: Arc<dyn LibraryFacts>,
) -> (Scheduler, Arc<bunko_sched::rate::ManualClock>) {
    let clock = Arc::new(bunko_sched::rate::ManualClock::new(ocr_common::t0()));
    let sched = Scheduler::new(
        SchedDeps {
            layout: layout.clone(),
            db: None,
            facts,
            locks: Arc::new(bunko_library::service::NoPathLocks),
            clock: clock.clone(),
            exec,
            upgrade: None,
            generator: "mokuro-bunko test".into(),
            version: "0.8.0-test".into(),
        },
        settings(vec![primary()]),
    );
    (sched, clock)
}

/// Wait (blocking) until `f` holds.
fn eventually_sync(what: &str, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spec §8: on a remote store an arrival is judged on a helper thread (it reads the
/// zip directory over the network); the actor answers at once and takes the verdict
/// when it comes back.
#[test]
fn remote_arrivals_are_judged_off_the_actor() {
    let c = cloud_sched();
    let fake = c.remote.backend();
    fake.reader_put(&format!("{ROOT}/Series/Vol 3.cbz"), &cbz(2, 100));
    c.remote.feed().poll_now();
    let vol3 = c.layout.library().join("Series/Vol 3.cbz");
    eventually_sync("the new archive in the index", || {
        bunko_store::fs::is_file(&vol3)
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let mut sched = sched_over(&c.layout, Exec::Threads(tx));
    fake.set_latency(Duration::from_millis(300));
    let t = Instant::now();
    sched.handle(Msg::ArchiveArrived(vol3.clone()));
    assert!(
        t.elapsed() < Duration::from_millis(250),
        "the actor did not wait for the network ({:?})",
        t.elapsed()
    );
    assert!(!sched.owed.volumes.contains_key("Series/Vol 3.cbz"));
    let msg = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the verdict");
    assert!(matches!(msg, Msg::ArrivalJudged { .. }), "{msg:?}");
    sched.handle(msg);
    let v = &sched.owed.volumes["Series/Vol 3.cbz"];
    assert_eq!(v.pages, Some(2), "the page count came with it");
    assert_eq!(v.rows.len(), 1);
}

/// Spec §3.5 / §9.1: while the store needs a new sign-in the queue holds with
/// `storage-auth` (no new claims); it takes work again once the store is back.
#[test]
fn a_store_that_needs_a_sign_in_holds_the_queue() {
    use bunko_remote::BErr;
    use bunko_remote::fake::FakeOp;
    let c = cloud_sched();
    let mut sched = sched_over(&c.layout, Exec::Inline);
    sched.handle(Msg::Tick);
    assert_eq!(sched.storage_hold(), None);
    c.remote
        .backend()
        .fail_next(FakeOp::Changes, BErr::AuthRevoked);
    c.remote.feed().poll_now();
    eventually_sync("the store to need a sign-in", || {
        sched.storage_hold() == Some("storage-auth")
    });
    assert_eq!(sched.queue_hold(), Some("storage-auth"));
    assert!(sched.held("local"), "no new claims");
    sched.handle(Msg::Tick);
    assert_eq!(sched.storage_held, Some("storage-auth"));
    // Signing in again clears the governor's hold and resumes the feed.
    c.remote.reauthorized();
    c.remote.feed().resume();
    eventually_sync("the store back online", || sched.storage_hold().is_none());
    sched.handle(Msg::Tick);
    assert_eq!(sched.storage_held, None);
    assert_ne!(sched.queue_hold(), Some("storage-auth"));
}

/// Spec §5.4: a sidecar the reader just deleted (its archive stayed) is being
/// replaced: the volume is not owed until the quiet period is over.
#[test]
fn a_sidecar_removed_in_the_cloud_holds_its_volume_for_the_quiet_period() {
    let c = cloud_sched();
    let library = c.layout.library();
    let sidecar = library.join("Series/Vol 1.mokuro");
    assert!(!bunko_server::ocr::owed::still_owed(
        &library,
        "Series/Vol 1.cbz",
        &primary(),
        false
    ));
    c.remote
        .backend()
        .reader_delete(&format!("{ROOT}/Series/Vol 1.mokuro"));
    c.remote.feed().poll_now();
    eventually_sync("the delete in the index", || {
        !bunko_store::fs::exists(&sidecar)
    });
    assert!(bunko_server::ocr::owed::sidecar_recently_removed(
        &library.join("Series/Vol 1.cbz"),
        &primary()
    ));
    assert!(
        !bunko_server::ocr::owed::still_owed(&library, "Series/Vol 1.cbz", &primary(), false),
        "held while the reader replaces it"
    );
    // The other volume is unaffected.
    assert!(bunko_server::ocr::owed::still_owed(
        &library,
        "Series/Vol 2.cbz",
        &primary(),
        false
    ));
}

fn no_lease(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
}

/// The scheduler over a cloud library: the census sees the one volume owed, a local
/// claim spools the archive before it is submitted, and the result lands in the cloud.
#[test]
fn local_claims_spool_the_cloud_archive_and_install_into_the_cloud() {
    run_local_claim(false);
}

/// The same, with the store refusing the install (it stopped): the claim goes back to
/// the queue, the volume is neither lost nor recorded as failed.
#[test]
fn an_install_the_cloud_store_refuses_is_held() {
    run_local_claim(true);
}

fn run_local_claim(refuse: bool) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let (fake, lib) = fake_library();
    bunko_server::storage::testing::register_tuned(&base, fake, Some(no_lease));
    let config = cloud_config(base.clone(), true);
    let layout = config.storage.layout();
    layout.ensure_directories().unwrap();
    let (store, _mount, handle) = bunko_server::storage::open_library_store(&config, &layout)
        .unwrap()
        .unwrap();
    assert!(handle.wait_steady(Duration::from_secs(20)));
    let layout = layout.with_library_root(store.root());
    let remote = handle.fake_store().unwrap().clone();
    remote.backend().reset_calls();
    let vol2 = remote
        .backend()
        .node_at(&format!("{ROOT}/Series/Vol 2.cbz"))
        .unwrap();

    let t0 = ocr_common::t0();
    let clock = Arc::new(bunko_sched::rate::ManualClock::new(t0));
    let mut s = settings(vec![primary()]);
    s.local_processing = true;
    let mut sched = Scheduler::new(
        SchedDeps {
            layout: layout.clone(),
            db: None,
            facts: Arc::new(FileFacts),
            locks: Arc::new(bunko_library::service::NoPathLocks),
            clock: clock.clone(),
            exec: Exec::Inline,
            upgrade: None,
            generator: "mokuro-bunko test".into(),
            version: "0.8.0-test".into(),
        },
        s,
    );
    sched.handle(Msg::Tick);
    // The census: only the un-OCR'd volume is owed.
    let owed: Vec<String> = sched.owed.volumes.keys().cloned().collect();
    assert_eq!(owed, ["Series/Vol 2.cbz"]);

    let (tx, mut ops) = tokio::sync::mpsc::unbounded_channel();
    let catalog: bunko_proto::Catalog = serde_json::from_value(serde_json::json!({
        "engines": ocr_common::ENGINES,
        "detectors": ["ppocr-manga"],
        "devices": [{"id": "cpu", "label": "CPU"}],
        "engine_revisions": ocr_common::current_revisions(ocr_common::ENGINES),
    }))
    .unwrap();
    sched.handle(Msg::LocalUp {
        ops: tx,
        catalog,
        host: bunko_proto::HostInfo {
            cpu: "Test CPU".into(),
            backend: "cpu".into(),
            ..Default::default()
        },
    });
    clock.set(t0 + 1.0);
    sched.handle(Msg::Tick);
    let mut sent = Vec::new();
    while let Ok(op) = ops.try_recv() {
        sent.push(op);
    }
    let sid = sent
        .iter()
        .find_map(|o| match o {
            Op::OpenSession { sid, .. } => Some(sid.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a local session opens: {sent:?}"));
    let vol = sent
        .iter()
        .find_map(|o| match o {
            Op::Volume(v) => Some(v.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the volume is submitted once spooled: {sent:?}"));
    // The runner reads a local spool file, not the synthetic library path.
    let archive = PathBuf::from(&vol.archive);
    assert!(
        !archive.starts_with(layout.library()),
        "{}",
        archive.display()
    );
    assert!(
        archive.starts_with(base.join("cloud")),
        "{}",
        archive.display()
    );
    assert_eq!(std::fs::read(&archive).unwrap(), lib.vol2);
    assert_eq!(vol.size, Some(lib.vol2.len() as u64));
    // One whole download of that archive (the spool).
    let reads = remote.backend().read_log();
    assert!(
        reads.iter().any(|r| r.pid == vol2.pid && r.range.is_none()),
        "{reads:?}"
    );
    let job = sched
        .claims
        .keys()
        .next()
        .cloned()
        .expect("the claim is held");
    assert!(sched.claims[&job].spool.is_some(), "pinned while claimed");

    if refuse {
        // The store stops at STAGED, where the op is already in the outbox: the gate
        // is shut first, so the worker cannot send it before the store has stopped.
        remote.set_lease_gate(Arc::new(|| false));
        remote.set_failpoint(Some(Arc::new(|at: &str| at == "STAGED")));
    }
    // The runner finishes.
    let local = sched
        .machines
        .values()
        .find(|m| m.local)
        .unwrap()
        .pid
        .clone();
    let event = |sched: &mut Scheduler, event: Event| {
        sched.handle(Msg::Event {
            pid: local.clone(),
            event,
        })
    };
    event(&mut sched, ready(&sid));
    event(&mut sched, started(&sid, &vol.claim, 3));
    let result_dir = base.join(".processing").join(&sid).join(&vol.claim);
    std::fs::create_dir_all(&result_dir).unwrap();
    let result = result_dir.join(bunko_proto::RESULT_FILE);
    std::fs::write(&result, mokuro("vol-2-uuid", 3)).unwrap();
    let sha = bunko_server::ocr::collect::sha256_file(&result).unwrap();
    event(
        &mut sched,
        Event::VolumeDone {
            sid: sid.clone(),
            id: vol.claim.clone(),
            pages: 3,
            failed_pages: 0,
            seconds: 2.0,
            stats: serde_json::Value::Null,
            cpu_pressure: None,
            other_cpu: None,
            sidecar_sha256: Some(sha),
        },
    );
    assert!(sched.claims.is_empty(), "the claim ended");
    assert!(sched.failures.is_empty(), "no failure recorded");
    if !refuse {
        assert!(!sched.owed.volumes.contains_key("Series/Vol 2.cbz"));
        assert!(remote.wait_outbox_idle(Duration::from_secs(20)));
        let node = remote
            .backend()
            .node_at(&format!("{ROOT}/Series/Vol 2.mokuro"))
            .expect("the sidecar is in the cloud");
        let bytes = remote.backend().content(&node.pid).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["volume"], "Vol 2", "normalised before the upload");
        assert!(
            bunko_store::fs::metadata(layout.library().join("Series/Vol 2.mokuro"))
                .unwrap()
                .own()
        );
        return;
    }
    // Returned to the queue, not lost and not a failure of the volume.
    assert!(
        sched.owed.volumes.contains_key("Series/Vol 2.cbz"),
        "the volume is still owed"
    );
    assert!(sched.failures.is_empty(), "no failure recorded");
    assert!(
        remote
            .backend()
            .node_at(&format!("{ROOT}/Series/Vol 2.mokuro"))
            .is_none(),
        "nothing was written to the cloud"
    );
}

/// Register a remote processor of mokuro-bunko `version` and open its socket.
fn connect(
    sched: &mut Scheduler,
    name: &str,
    sessions: u32,
    version: &str,
) -> (String, tokio::sync::mpsc::UnboundedReceiver<Op>) {
    let (reply, mut rx) = tokio::sync::oneshot::channel();
    let body = serde_json::json!({
        "protocol": 3,
        "name": name,
        "host": {"cpu": "Test CPU", "gpu": "Test GPU", "backend": "cuda", "version": version},
        "catalog": {"engines": ocr_common::ENGINES, "detectors": ["ppocr-manga"], "devices": [{"id": "cpu", "label": "CPU"}], "engine_revisions": ocr_common::current_revisions(ocr_common::ENGINES)},
        "max_sessions": sessions,
    });
    sched.handle(Msg::Register {
        input: bunko_server::ocr::sched::RegisterInput {
            username: format!("acct-{name}"),
            body,
            account_stamp: None,
        },
        reply,
    });
    let pid = match rx.try_recv().unwrap() {
        bunko_server::ocr::sched::RegisterOutcome::Ok(r) => r.processor_id,
        other => panic!("register refused: {other:?}"),
    };
    let (tx, ops) = tokio::sync::mpsc::unbounded_channel();
    let (reply, mut rx) = tokio::sync::oneshot::channel();
    sched.handle(Msg::SocketOpen {
        pid: pid.clone(),
        username: format!("acct-{name}"),
        ops: tx,
        reply,
    });
    rx.try_recv().unwrap().unwrap();
    (pid, ops)
}

fn drain(ops: &mut tokio::sync::mpsc::UnboundedReceiver<Op>) -> Vec<Op> {
    let mut out = Vec::new();
    while let Ok(op) = ops.try_recv() {
        out.push(op);
    }
    out
}

/// `(sid, claim, library-relative archive)` of every volume op.
fn volume_ops(ops: &[Op]) -> Vec<(String, String, String)> {
    ops.iter()
        .filter_map(|o| match o {
            Op::Volume(v) => Some((
                v.sid.clone(),
                v.claim.clone(),
                v.archive.trim_start_matches("/mokuro-reader/").to_string(),
            )),
            _ => None,
        })
        .collect()
}

/// A remote processor fails `Series/Vol 2.cbz` with "no page images found"; returns
/// its pid and op channel (its session is over).
fn fail_vol2(sched: &mut Scheduler) -> (String, tokio::sync::mpsc::UnboundedReceiver<Op>) {
    sched.handle(Msg::Tick);
    assert!(sched.owed.volumes.contains_key("Series/Vol 2.cbz"));
    let (pid, mut ops) = connect(sched, "box", 1, "0.7.6");
    let sent = drain(&mut ops);
    let (sid, claim, rel) = volume_ops(&sent).pop().expect("Vol 2 is offered");
    assert_eq!(rel, "Series/Vol 2.cbz");
    let event = |sched: &mut Scheduler, event: Event| {
        sched.handle(Msg::Event {
            pid: pid.clone(),
            event,
        })
    };
    event(sched, ready(&sid));
    event(
        sched,
        Event::VolumeFailed {
            sid: sid.clone(),
            id: claim,
            error: "no page images found in Vol 2.cbz".into(),
        },
    );
    event(sched, exit(&sid, Some(0)));
    let rec = &sched.failures["Series/Vol 2.cbz"];
    assert_eq!(rec["tried_by"], serde_json::json!(["0.7.6"]));
    (pid, ops)
}

/// Regression: a failure record was kept only while `<library>/<series>/<volume>.cbz`
/// was a file on disk, and a cloud library's root never is, so every scan dropped every
/// record: a volume nothing in which decodes was never parked (each scan offered it to
/// the release that had just failed it), and the catalog never showed it as failing.
#[test]
fn a_cloud_volumes_failure_record_survives_scans_and_parks_it() {
    let c = cloud_sched();
    let (mut sched, clock) = sched_with(&c.layout, Exec::Inline, Arc::new(FileFacts));
    let (_pid, mut ops) = fail_vol2(&mut sched);
    // A day of scans.
    for hours in 1..=24 {
        clock.set(ocr_common::t0() + f64::from(hours) * 3600.0);
        sched.handle(Msg::Tick);
    }
    sched.prune_failures();
    assert!(
        sched.failures.contains_key("Series/Vol 2.cbz"),
        "the record survives the scans"
    );
    assert!(
        volume_ops(&drain(&mut ops)).is_empty(),
        "parked: never retried by the release that failed it"
    );
    assert_eq!(
        sched
            .failing_volumes()
            .get("series/vol 2.cbz")
            .map(String::as_str),
        Some("This volume has no pages that could be decoded."),
        "the catalog's badge"
    );
    // A release that has not tried it gets it at once.
    let (_, mut newer) = connect(&mut sched, "newer", 1, "0.7.7");
    let got: Vec<String> = volume_ops(&drain(&mut newer))
        .into_iter()
        .map(|(_, _, rel)| rel)
        .collect();
    assert_eq!(got, ["Series/Vol 2.cbz"]);
}

/// The other half of the prune rule on a cloud library: a record whose archive left
/// the cloud folder goes at the next scan.
#[test]
fn a_cloud_volumes_failure_record_goes_with_its_archive() {
    let c = cloud_sched();
    let (mut sched, clock) = sched_with(&c.layout, Exec::Inline, Arc::new(FileFacts));
    fail_vol2(&mut sched);
    c.remote
        .backend()
        .reader_delete(&format!("{ROOT}/Series/Vol 2.cbz"));
    c.remote.feed().poll_now();
    let vol2 = c.layout.library().join("Series/Vol 2.cbz");
    eventually_sync("the delete in the index", || {
        !bunko_store::fs::exists(&vol2)
    });
    clock.set(ocr_common::t0() + 3600.0);
    sched.handle(Msg::Tick);
    assert!(
        sched.failures.is_empty(),
        "the record of a removed archive is pruned: {:?}",
        sched.failures
    );
}

/// A `.cbz` of `pages` stored members named `NNN.<suffix>`.
fn cbz_of(pages: usize, suffix: &str) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..pages {
        z.start_file(format!("{:03}.{suffix}", i + 1), opts)
            .unwrap();
        z.write_all(b"not really an image").unwrap();
    }
    z.finish().unwrap().into_inner()
}

/// Page counts without opening an archive (what the metadata cache gives a server), so
/// nothing but the page-format check reads one.
struct CachedCounts;
impl LibraryFacts for CachedCounts {
    fn missing_pages(&self, cbz: &std::path::Path) -> i64 {
        LibraryFacts::missing_pages(&FileFacts, cbz)
    }
    fn page_count(&self, _cbz: &std::path::Path) -> Option<i64> {
        Some(3)
    }
}

/// Spec §8 with the page-format rule (0.7.6): whether an older processor's release
/// reads a cloud volume's pages is learnt from its zip directory on a helper thread,
/// never on the actor; until then the volume waits. A volume of JPEG XL pages is then
/// not offered to a release older than 0.7.5, and is offered to one that reads them.
#[test]
fn a_cloud_volumes_page_formats_are_read_off_the_actor() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let c = cloud_sched();
    let fake = c.remote.backend().clone();
    fake.reader_put(&format!("{ROOT}/Series/Vol 3.cbz"), &cbz_of(3, "jxl"));
    c.remote.feed().poll_now();
    let vol3 = c.layout.library().join("Series/Vol 3.cbz");
    eventually_sync("the new archive in the index", || {
        bunko_store::fs::is_file(&vol3)
    });
    // A read the actor makes while it handles a message would hold the actor here
    // (the helper's reads wait until it is done): it is recorded, never waited out.
    let on_actor = Arc::new(AtomicBool::new(false));
    let read_on_actor = Arc::new(AtomicBool::new(false));
    let hook = {
        let (on_actor, read_on_actor) = (on_actor.clone(), read_on_actor.clone());
        Arc::new(move |op: bunko_remote::fake::FakeOp| {
            if op != bunko_remote::fake::FakeOp::Read {
                return;
            }
            let until = Instant::now() + Duration::from_secs(2);
            while on_actor.load(Ordering::SeqCst) {
                if Instant::now() > until {
                    read_on_actor.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    fake.set_call_hook(Some(hook));
    let (tx, rx) = std::sync::mpsc::channel();
    let (mut sched, _clock) = sched_with(&c.layout, Exec::Threads(tx), Arc::new(CachedCounts));
    let mut handle = |sched: &mut Scheduler, msg: Msg| {
        on_actor.store(true, Ordering::SeqCst);
        sched.handle(msg);
        on_actor.store(false, Ordering::SeqCst);
    };
    // Take helper messages until `done` holds.
    let helpers = |sched: &mut Scheduler,
                   handle: &mut dyn FnMut(&mut Scheduler, Msg),
                   done: &dyn Fn(&Scheduler) -> bool| {
        while !done(sched) {
            let msg = rx
                .recv_timeout(Duration::from_secs(20))
                .expect("a helper's answer");
            handle(sched, msg);
        }
    };
    handle(&mut sched, Msg::Tick);
    helpers(&mut sched, &mut handle, &|s| s.walks > 0);
    let owed: Vec<&String> = sched.owed.volumes.keys().collect();
    assert_eq!(owed, ["Series/Vol 2.cbz", "Series/Vol 3.cbz"]);
    fake.reset_calls();

    // 0.7.4 reads no JPEG XL: offered nothing until the formats are known, then only
    // the volume of JPEG pages.
    on_actor.store(true, Ordering::SeqCst);
    let (_, mut old) = connect(&mut sched, "old", 2, "0.7.4");
    on_actor.store(false, Ordering::SeqCst);
    assert!(
        volume_ops(&drain(&mut old)).is_empty(),
        "offered before its pages were known"
    );
    helpers(&mut sched, &mut handle, &|s| {
        s.owed.volumes["Series/Vol 3.cbz"].page_suffixes.is_some()
    });
    let got: Vec<String> = volume_ops(&drain(&mut old))
        .into_iter()
        .map(|(_, _, rel)| rel)
        .collect();
    assert_eq!(got, ["Series/Vol 2.cbz"], "0.7.4 cannot read JPEG XL pages");
    let (_, read) = sched.owed.volumes["Series/Vol 3.cbz"]
        .page_suffixes
        .clone()
        .unwrap();
    assert_eq!(
        read,
        bunko_server::ocr::owed::PageSuffixes::Read(Arc::from(vec![".jxl".to_string()]))
    );
    let vol3_pid = fake
        .node_at(&format!("{ROOT}/Series/Vol 3.cbz"))
        .unwrap()
        .pid;
    assert!(
        fake.read_log().iter().any(|r| r.pid == vol3_pid),
        "a helper read the archive: {:?}",
        fake.read_log()
    );
    assert!(sched.failures.is_empty());

    // A release that reads them is offered it at once (nothing more to read).
    on_actor.store(true, Ordering::SeqCst);
    let (_, mut new) = connect(&mut sched, "new", 1, "0.7.5");
    on_actor.store(false, Ordering::SeqCst);
    let got: Vec<String> = volume_ops(&drain(&mut new))
        .into_iter()
        .map(|(_, _, rel)| rel)
        .collect();
    assert_eq!(got, ["Series/Vol 3.cbz"]);
    fake.set_call_hook(None);
    assert!(
        !read_on_actor.load(Ordering::SeqCst),
        "the actor read an archive"
    );
}

/// An older processor facing a long backlog of cloud volumes whose page formats are not
/// known yet is offered the first it reads after one helper batch
/// ([`bunko_server::ocr::sched::SUFFIX_BATCH`] reads), not after the whole backlog has
/// been read, and the batch reports on its own.
#[test]
fn an_older_processor_is_offered_work_after_one_batch_of_page_format_reads() {
    use bunko_server::ocr::sched::SUFFIX_BATCH;
    const BACKLOG: usize = 40;
    let c = cloud_sched();
    let fake = c.remote.backend().clone();
    for i in 0..BACKLOG {
        fake.reader_put(
            &format!("{ROOT}/Backlog/Vol {:02}.cbz", i + 1),
            &cbz_of(3, "jpg"),
        );
    }
    c.remote.feed().poll_now();
    let last = c
        .layout
        .library()
        .join(format!("Backlog/Vol {BACKLOG:02}.cbz"));
    eventually_sync("the backlog in the index", || {
        bunko_store::fs::is_file(&last)
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let (mut sched, _clock) = sched_with(&c.layout, Exec::Threads(tx), Arc::new(CachedCounts));
    sched.handle(Msg::Tick);
    while sched.walks == 0 {
        let msg = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("a helper's answer");
        sched.handle(msg);
    }
    assert!(
        sched.owed.volumes.len() > BACKLOG,
        "{:?}",
        sched.owed.volumes.keys()
    );
    fake.reset_calls();

    let (_, mut old) = connect(&mut sched, "old", 2, "0.7.4");
    assert!(volume_ops(&drain(&mut old)).is_empty());
    assert!(sched.suffixes_reading, "a batch is being read");
    assert!(sched.suffixes_wanted.len() <= SUFFIX_BATCH);
    // The first batch's answer arrives once its reads are done, and the next batch
    // cannot start before the actor has taken it: the read log, taken as the answer
    // arrives, holds that batch's archives only. (Taking the answer starts the next
    // batch on a helper at once, so the log is read before it is handled.)
    let (batch, read) = loop {
        let msg = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("a helper's answer");
        if let Msg::PageSuffixesRead(answer) = &msg {
            let read: std::collections::HashSet<_> =
                fake.read_log().iter().map(|r| r.pid.clone()).collect();
            let batch: Vec<String> = answer.iter().map(|(rel, _, _)| rel.clone()).collect();
            sched.handle(msg);
            break (batch, read);
        }
        sched.handle(msg);
    };
    assert!(
        (1..=SUFFIX_BATCH).contains(&batch.len()),
        "batch of {}",
        batch.len()
    );
    let batch_pids: std::collections::HashSet<_> = batch
        .iter()
        .map(|rel| fake.node_at(&format!("{ROOT}/{rel}")).unwrap().pid)
        .collect();
    assert!(
        read.len() <= SUFFIX_BATCH && read.is_subset(&batch_pids),
        "{} archives read before the first offer, not all of the batch {batch:?}",
        read.len()
    );
    let got = volume_ops(&drain(&mut old));
    assert!(!got.is_empty(), "offered nothing after the first batch");
    assert!(sched.failures.is_empty());
}

/// The lease runs, with a first-claim settle wait of a few seconds.
fn claim_in_seconds(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_secs(4));
    c.retry_base = Duration::from_millis(50);
}

/// An old main sidecar (no `ocr_engine`): the generation upgrade replaces it.
const LEGACY: &str = r#"{"version": "0.2.1", "title": "Series", "volume": "Vol 1", "pages": [{"img_path": "001.jpg", "blocks": []}, {"img_path": "002.jpg", "blocks": []}, {"img_path": "003.jpg", "blocks": []}]}"#;

/// While this Bunko is still claiming a cloud library (its settle wait, spec §9.2), an
/// upgrade job is not handed out: its swap replaces the cloud's current version with a
/// Sync write, which the store refuses until this Bunko holds the library, so the OCR
/// run would be thrown away and the volume recorded as failed. The other jobs run (a
/// new OCR file waits in the outbox), the queue page says why the upgrade waits and
/// the queue file shows it held, and once the lease is held the upgrade is handed out.
#[test]
fn upgrade_jobs_wait_while_this_bunko_claims_the_cloud_library() {
    use bunko_remote::status::LeaseEvent;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), &cbz(3, 100));
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.mokuro"), LEGACY.as_bytes());
    fake.reader_put(&format!("{ROOT}/Series/Vol 2.cbz"), &cbz(3, 100));
    bunko_server::storage::testing::register_tuned(&base, fake, Some(claim_in_seconds));
    let config = cloud_config(base.clone(), true);
    let layout = config.storage.layout();
    layout.ensure_directories().unwrap();
    let (store, _mount, handle) = bunko_server::storage::open_library_store(&config, &layout)
        .unwrap()
        .unwrap();
    assert!(handle.wait_steady(Duration::from_secs(20)));
    let layout = layout.with_library_root(store.root());
    let remote = handle.fake_store().unwrap().clone();
    let claiming = || remote.lease().and_then(|l| l.claiming_left()).is_some();
    eventually_sync("the claim's settle wait", claiming);

    let upgrade = Arc::new(bunko_server::ocr::upgrade::Upgrade::new(
        layout.library(),
        None,
        Arc::new(FileFacts),
        Arc::new(bunko_library::service::NoPathLocks),
        "mokuro-bunko test".into(),
    ));
    upgrade.configure(
        &bunko_core::config::UpgradeConfig {
            enabled: true,
            replace: vec!["mokuro-legacy".into(), "mokuro".into()],
        },
        &[primary()],
    );
    let t0 = ocr_common::t0();
    let clock = Arc::new(bunko_sched::rate::ManualClock::new(t0));
    let mut s = settings(vec![primary()]);
    s.local_processing = true;
    let mut sched = Scheduler::new(
        SchedDeps {
            layout: layout.clone(),
            db: None,
            facts: Arc::new(FileFacts),
            locks: Arc::new(bunko_library::service::NoPathLocks),
            clock: clock.clone(),
            exec: Exec::Inline,
            upgrade: Some(upgrade),
            generator: "mokuro-bunko test".into(),
            version: "0.8.0-test".into(),
        },
        s,
    );
    sched.handle(Msg::Tick);
    let upgrade_job = Job::upgrade("Series/Vol 1.cbz", "g-1");
    let plain_job = Job::new("Series/Vol 2.cbz", "g-1");
    assert!(
        sched.owed.volumes["Series/Vol 1.cbz"].upgrade,
        "an upgrade is owed"
    );
    assert!(sched.owed.volumes.contains_key("Series/Vol 2.cbz"));
    let (tx, mut ops) = tokio::sync::mpsc::unbounded_channel();
    let catalog: bunko_proto::Catalog = serde_json::from_value(serde_json::json!({
        "engines": ocr_common::ENGINES,
        "detectors": ["ppocr-manga"],
        "devices": [{"id": "cpu", "label": "CPU"}],
        "engine_revisions": ocr_common::current_revisions(ocr_common::ENGINES),
    }))
    .unwrap();
    sched.handle(Msg::LocalUp {
        ops: tx,
        catalog,
        host: bunko_proto::HostInfo {
            cpu: "Test CPU".into(),
            backend: "cpu".into(),
            ..Default::default()
        },
    });
    clock.set(t0 + 1.0);
    sched.handle(Msg::Tick);
    let sent = drain(&mut ops);
    let sid = sent
        .iter()
        .find_map(|o| match o {
            Op::OpenSession { sid, .. } => Some(sid.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("a local session opens: {sent:?}"));
    let vol = sent
        .iter()
        .find_map(|o| match o {
            Op::Volume(v) => Some(v.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the plain job is handed out: {sent:?}"));
    assert!(
        sched.claims.contains_key(&plain_job),
        "{:?}",
        sched.claims.keys()
    );
    assert!(
        !sched.claims.contains_key(&upgrade_job),
        "no upgrade job while claiming"
    );
    // The library is not held as a whole: the claim holds only the upgrades.
    assert_eq!(sched.storage_hold(), None);
    assert_eq!(sched.queue_hold(), None);

    // The plain job finishes and is installed (a Deferred write, held in the outbox
    // until the lease); the free lane still gets no upgrade job.
    let local = sched
        .machines
        .values()
        .find(|m| m.local)
        .unwrap()
        .pid
        .clone();
    let event = |sched: &mut Scheduler, event: Event| {
        sched.handle(Msg::Event {
            pid: local.clone(),
            event,
        })
    };
    event(&mut sched, ready(&sid));
    event(&mut sched, started(&sid, &vol.claim, 3));
    let result_dir = base.join(".processing").join(&sid).join(&vol.claim);
    std::fs::create_dir_all(&result_dir).unwrap();
    let result = result_dir.join(bunko_proto::RESULT_FILE);
    std::fs::write(&result, mokuro("vol-2-uuid", 3)).unwrap();
    let sha = bunko_server::ocr::collect::sha256_file(&result).unwrap();
    event(
        &mut sched,
        Event::VolumeDone {
            sid: sid.clone(),
            id: vol.claim.clone(),
            pages: 3,
            failed_pages: 0,
            seconds: 2.0,
            stats: serde_json::Value::Null,
            cpu_pressure: None,
            other_cpu: None,
            sidecar_sha256: Some(sha),
        },
    );
    clock.set(t0 + 2.0);
    sched.handle(Msg::Tick);
    assert!(
        !sched.owed.volumes.contains_key("Series/Vol 2.cbz"),
        "installed"
    );
    let sent = drain(&mut ops);
    assert!(
        !sched.claims.contains_key(&upgrade_job) && volume_ops(&sent).is_empty(),
        "no upgrade job while claiming: {sent:?}"
    );
    assert!(sched.failures.is_empty(), "no failure recorded");
    // With nothing it may run, the session closes.
    if sent
        .iter()
        .any(|o| matches!(o, Op::CloseSession { sid: s } if *s == sid))
    {
        event(&mut sched, exit(&sid, Some(0)));
    }

    // The queue page says why it waits; the queue file shows it held.
    let pending = sched.pending_jobs();
    let entry = pending
        .iter()
        .find(|e| e.get("kind").and_then(|k| k.as_str()) == Some("upgrade"))
        .unwrap_or_else(|| panic!("the upgrade is listed: {pending:?}"));
    assert_eq!(entry["held"], true, "{entry:?}");
    let reason = entry["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("Upgrades wait until this Bunko holds the library")
            && reason.contains("left)"),
        "{reason}"
    );
    let (held, volumes, _) = sched.queue_document();
    assert_eq!(held, None, "the queue as a whole is not held");
    let vol1 = volumes
        .iter()
        .find(|v| v["volume"] == "Vol 1")
        .unwrap_or_else(|| panic!("{volumes:?}"));
    assert_eq!(vol1["jobs"][0]["state"], "held", "{vol1}");
    assert!(claiming(), "all of the above ran while claiming");

    // Once this Bunko holds the library, the upgrade is handed out.
    eventually_sync("the lease", || {
        remote.lease().map(|l| l.event()) == Some(LeaseEvent::Acquired)
    });
    clock.set(t0 + 3.0);
    sched.handle(Msg::Tick);
    let sent = drain(&mut ops);
    assert!(
        sched.claims.contains_key(&upgrade_job),
        "the upgrade runs once the lease is held: {:?} {sent:?}",
        sched.claims.keys()
    );
    let pending = sched.pending_jobs();
    assert!(
        pending.iter().all(|e| e.get("held").is_none()),
        "{pending:?}"
    );
}
