//! Writes through the bridge WebDAV into a cloud library (0.8 §10), over real HTTP: a
//! whole server (its own OCR processor on `FakePipeline`) listens on a TCP port, and
//! `reqwest` PUTs, MOVEs and COPYs into the in-memory `FakeBackend`.
//!
//! - A new volume lands in the cloud with its exact bytes (digest checked, verdict
//!   headers, ETag), and is then OCR'd: its `.mokuro` appears in the cloud too.
//! - An overwrite replaces the cloud file (one copy), `If-Match` travels with it, and
//!   a stale `If-Match` is a 412 that changes nothing.
//! - A reader's sidecar `.mokuro` PUT lands; re-sending it unchanged edits nothing.
//! - A damaged body (bad `Content-Digest`) is refused and leaves nothing behind.
//! - While this Bunko is still in its own first-claim settle wait, a PUT (and a MOVE)
//!   is a clean 503 whose `Retry-After` is what is left of the wait, saying so, and
//!   the admin storage status says the same; while another Bunko holds the lease it
//!   is a 503 naming that.
//! - No PUT, failed or not, leaves a staging file or a `state.sqlite` staging row.

// Fixtures are checked with std::fs on purpose (the store's state folder is local).
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::{TokenKind, UserStatus};
use bunko_remote::fake::FakeBackend;
use bunko_server::app::{ServeOptions, Services, assemble, start_library_services};
use serde_json::Value;
use sha2::Digest as _;
use tokio_util::sync::CancellationToken;

const ROOT: &str = "mokuro-reader";

/// A `.cbz` of `pages` small PNG pages (real images: covers and OCR decode them).
fn cbz(pages: usize, seed: u8) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..pages {
        let img = image::RgbImage::from_fn(24, 32, |x, y| {
            image::Rgb([
                (x * 9) as u8,
                (y * 7) as u8,
                (i as u8 * 40).wrapping_add(seed),
            ])
        });
        let mut png = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        z.start_file(format!("{:03}.png", i + 1), opts).unwrap();
        z.write_all(&png).unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn mokuro(volume: &str, pages: usize, text: &str) -> Vec<u8> {
    let pages: Vec<Value> = (0..pages)
        .map(|i| {
            serde_json::json!({"img_path": format!("{:03}.png", i + 1), "img_width": 24,
                "img_height": 32, "blocks": [{"box": [1, 1, 5, 5], "lines": [text]}]})
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "version": "0.2.5", "title": "Series", "title_uuid": "t-uuid",
        "volume": volume, "volume_uuid": format!("{volume}-uuid"), "pages": pages,
    }))
    .unwrap()
}

fn fake_library(fake: FakeBackend) -> Arc<FakeBackend> {
    let fake = Arc::new(fake);
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), &cbz(2, 1));
    fake.reader_put(
        &format!("{ROOT}/Series/Vol 1.mokuro"),
        &mokuro("Vol 1", 2, "first"),
    );
    fake
}

/// No lease (writes may leave at once); quick retries and queue-file floor.
fn no_lease(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    c.queue_floor = Some(Duration::from_millis(200));
    c.retry_base = Duration::from_millis(50);
}

/// The lease runs, and its first claim waits an hour before it may hold the lease.
fn slow_claim(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_secs(3600));
    c.retry_base = Duration::from_millis(50);
}

/// The lease runs with a short settle wait (another Bunko's fresh marker wins anyway).
fn quick_claim(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_millis(100));
    c.retry_base = Duration::from_millis(50);
}

struct FakeLocal(bunko_processor::FakePipeline);

impl bunko_server::ocr::LocalProcessorFactory for FakeLocal {
    fn start(
        &self,
        results_dir: &std::path::Path,
    ) -> Result<bunko_server::ocr::LocalChannels, String> {
        use bunko_processor::PagePipeline;
        let info = self.0.describe();
        let mut link = bunko_processor::LocalProcessor::spawn(
            Arc::new(self.0.clone()),
            bunko_processor::LocalConfig {
                results_dir: results_dir.to_path_buf(),
            },
        );
        let finished = link.take_finished();
        Ok(bunko_server::ocr::LocalChannels {
            ops: link.ops,
            events: link.events,
            catalog: info.catalog,
            host: info.host,
            finished,
        })
    }
}

fn config(base: PathBuf) -> Config {
    let mut c = Config::default();
    c.storage.base_path = base;
    c.storage.library.backend = LibraryBackend::GDrive;
    c.server.serve_readers = true;
    c.ocr.autobench = false;
    c.ocr.local_processing = true;
    c.catalog.enrich_community = false;
    c
}

fn options() -> ServeOptions {
    let pipeline = bunko_processor::FakePipeline::new(bunko_processor::FakeConfig {
        engines: vec![bunko_core::engines::DEFAULT_ENGINE.into()],
        page_delay: Duration::from_millis(5),
        overlap: 1,
        ..Default::default()
    });
    ServeOptions {
        local: Some(Arc::new(FakeLocal(pipeline))),
        ..ServeOptions::default()
    }
}

/// One server over the cloud library, listening on a loopback port.
struct Server {
    _dir: tempfile::TempDir,
    services: Services,
    stop: CancellationToken,
    running: tokio::task::JoinHandle<()>,
    url: String,
    http: reqwest::Client,
    token: String,
}

struct Resp {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

impl Resp {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl Server {
    async fn start(fake: &Arc<FakeBackend>, tune: bunko_server::storage::testing::Tune) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        bunko_server::storage::testing::register_tuned(&base, fake.clone(), Some(tune));
        let opts = options();
        let services = Services::new(config(base), None, &opts).unwrap();
        services
            .db
            .create_user("boss", "pass1234", Role::Admin, UserStatus::Active, "")
            .unwrap();
        let token = services
            .db
            .create_auth_token("boss", TokenKind::Web, "test", None)
            .unwrap()
            .0;
        let storage = services.storage.clone().expect("a cloud library");
        let steady =
            tokio::task::spawn_blocking(move || storage.wait_steady(Duration::from_secs(20)))
                .await
                .unwrap();
        assert!(steady, "the first sync finishes");
        let stop = CancellationToken::new();
        let running = start_library_services(&services, stop.clone());
        let app = assemble(&services, &opts);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server {
            _dir: dir,
            services,
            stop,
            running,
            url: format!("http://{addr}"),
            http: reqwest::Client::new(),
            token,
        }
    }

    fn store(&self) -> Arc<bunko_remote::RemoteStore<FakeBackend>> {
        self.services
            .storage
            .as_ref()
            .unwrap()
            .fake_store()
            .unwrap()
            .clone()
    }

    /// `<base>/cloud/<store>/staging/`.
    fn staging_dir(&self) -> PathBuf {
        let h = self.services.storage.as_ref().unwrap();
        h.cloud_dir
            .join(h.store_id.as_deref().unwrap())
            .join("staging")
    }

    /// Nothing is left staged: no staging file on disk, no `state.sqlite` row. The
    /// server's own writers (a cover being made) stage for a moment too, so this waits
    /// for them; an upload's leftover never goes away and fails it.
    async fn assert_no_staging_leak(&self) {
        let list = || -> (Vec<String>, Vec<String>) {
            let files = std::fs::read_dir(self.staging_dir())
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            (files, self.store().staging_rows())
        };
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let (files, rows) = list();
            if files.is_empty() && rows.is_empty() {
                return;
            }
            assert!(
                Instant::now() < until,
                "staging left: files {files:?}, rows {rows:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn dav(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> Resp {
        let auth = base64::engine::general_purpose::STANDARD.encode("boss:pass1234");
        let mut b = self
            .http
            .request(
                reqwest::Method::from_bytes(method.as_bytes()).unwrap(),
                format!("{}{path}", self.url),
            )
            .header("authorization", format!("Basic {auth}"));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        if let Some(body) = body {
            b = b.body(body);
        }
        let r = b.send().await.unwrap();
        Resp {
            status: r.status().as_u16(),
            headers: r.headers().clone(),
            body: r.bytes().await.unwrap().to_vec(),
        }
    }

    async fn put(&self, path: &str, bytes: &[u8], headers: &[(&str, &str)]) -> Resp {
        self.dav("PUT", path, headers, Some(bytes.to_vec())).await
    }

    async fn admin_storage(&self) -> Value {
        let r = self
            .http
            .get(format!("{}/_admin/api/storage", self.url))
            .header("authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        r.json().await.unwrap()
    }

    async fn idle(&self) {
        let s = self.store();
        let idle = tokio::task::spawn_blocking(move || s.wait_outbox_idle(Duration::from_secs(30)))
            .await
            .unwrap();
        assert!(idle, "the outbox drains: {:?}", self.store().outbox());
    }

    async fn stop(self) {
        self.stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), self.running).await;
        self.services.ocr.stop().await;
        self.services.library.stop().await;
    }
}

async fn eventually(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn cloud(fake: &FakeBackend, rel: &str) -> Option<Vec<u8>> {
    let n = fake.node_at(&format!("{ROOT}/{rel}"))?;
    fake.content(&n.pid)
}

async fn copies(fake: &FakeBackend, dir: &str, name: &str) -> usize {
    use bunko_remote::Backend as _;
    let parent = fake.node_at(&format!("{ROOT}/{dir}")).unwrap().pid;
    fake.find_named(&parent, name).await.unwrap().0.len()
}

fn digest(bytes: &[u8]) -> String {
    let d = sha2::Sha256::digest(bytes);
    format!(
        "sha-256=:{}:",
        base64::engine::general_purpose::STANDARD.encode(d)
    )
}

/// A new volume PUT through the bridge lands in the cloud with its bytes, then this
/// server OCRs it into the cloud; a damaged body before it leaves nothing. On a
/// Drive-like provider and on a WebDAV one (path ids, If-Match CAS).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_volume_put_lands_in_the_cloud_and_is_ocrd() {
    for (label, fake) in [
        ("drive", FakeBackend::drive_like()),
        ("webdav", FakeBackend::webdav_cas()),
    ] {
        let fake = fake_library(fake);
        let server = Server::start(&fake, no_lease).await;
        let vol = cbz(3, 7);
        let url = "/mokuro-reader/Series/Vol%202.cbz";

        // A body damaged on the way: refused, nothing in the cloud, nothing staged.
        let mut bad = vol.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xff;
        let r = server
            .put(url, &bad, &[("content-digest", &digest(&vol))])
            .await;
        assert_eq!(r.status, 422, "{label}: {}", r.text());
        assert_eq!(r.json()["reason"], "corrupted-in-transit", "{label}");
        assert!(cloud(&fake, "Series/Vol 2.cbz").is_none(), "{label}");
        server.assert_no_staging_leak().await;

        let r = server
            .put(url, &vol, &[("content-digest", &digest(&vol))])
            .await;
        assert_eq!(r.status, 201, "{label}: {}", r.text());
        assert_eq!(r.header("x-mokuro-upload"), Some("verified"), "{label}");
        assert_eq!(r.header("x-mokuro-digest-verified"), Some("sha-256"));
        assert_eq!(
            r.header("x-mokuro-size"),
            Some(vol.len().to_string().as_str())
        );
        let etag = r.header("etag").expect("an ETag").to_string();
        // COMMITTED before the answer (Sync, D6): the provider already has the bytes.
        assert!(
            cloud(&fake, "Series/Vol 2.cbz").as_deref() == Some(vol.as_slice()),
            "{label}: the cloud file holds the uploaded bytes"
        );
        server.assert_no_staging_leak().await;
        // The bridge serves it back with the same validator.
        let got = server.dav("GET", url, &[], None).await;
        assert_eq!(got.status, 200, "{label}");
        assert!(got.body == vol, "{label}");
        assert_eq!(got.header("etag"), Some(etag.as_str()), "{label}");
        let lib = server.services.core.layout.library();
        let meta = bunko_store::fs::metadata(lib.join("Series/Vol 2.cbz")).unwrap();
        assert!(meta.own(), "{label}: Bunko's own version (ledgered)");

        // The arrival is OCR'd, and the result reaches the cloud.
        eventually(
            &format!("{label}: the OCR result in the cloud"),
            Duration::from_secs(60),
            || cloud(&fake, "Series/Vol 2.mokuro").is_some(),
        )
        .await;
        let sidecar: Value =
            serde_json::from_slice(&cloud(&fake, "Series/Vol 2.mokuro").unwrap()).unwrap();
        assert_eq!(sidecar["volume"], "Vol 2", "{label}");
        assert_eq!(sidecar["pages"].as_array().map(Vec::len), Some(3));
        server.idle().await;
        assert_eq!(copies(&fake, "Series", "Vol 2.cbz").await, 1, "{label}");
        assert_eq!(copies(&fake, "Series", "Vol 2.mokuro").await, 1, "{label}");
        server.assert_no_staging_leak().await;
        // The upload is on record as the admin's.
        let owner = server
            .services
            .db
            .get_volume_owner("Series/Vol 2.cbz")
            .unwrap();
        assert_eq!(owner.as_deref(), Some("boss"), "{label}");
        server.stop().await;
    }
}

/// An overwrite replaces the cloud file in place (one copy); the client's If-Match
/// travels with the publish, so a stale one is a 412 that changes nothing. A reader's
/// sidecar PUT lands too, and re-sending it unchanged edits nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_overwrite_and_a_sidecar_put_land_in_the_cloud() {
    let fake = fake_library(FakeBackend::webdav_cas());
    let server = Server::start(&fake, no_lease).await;
    let url = "/mokuro-reader/Series/Vol%201.cbz";
    let head = server.dav("HEAD", url, &[], None).await;
    let etag = head.header("etag").unwrap().to_string();

    let v2 = cbz(2, 99);
    let r = server.put(url, &v2, &[("if-match", &etag)]).await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(cloud(&fake, "Series/Vol 1.cbz").as_deref() == Some(v2.as_slice()));
    assert_eq!(copies(&fake, "Series", "Vol 1.cbz").await, 1);
    let new_etag = r.header("etag").unwrap().to_string();
    assert_ne!(new_etag, etag);
    server.assert_no_staging_leak().await;

    // The old ETag is stale now: 412, the cloud keeps v2.
    let v3 = cbz(2, 123);
    let r = server.put(url, &v3, &[("if-match", &etag)]).await;
    assert_eq!(r.status, 412, "{}", r.text());
    assert!(cloud(&fake, "Series/Vol 1.cbz").as_deref() == Some(v2.as_slice()));
    // A create-only PUT over an existing file: 412 as well.
    let r = server.put(url, &v3, &[("if-none-match", "*")]).await;
    assert_eq!(r.status, 412, "{}", r.text());
    server.assert_no_staging_leak().await;

    // The reader uploads a sidecar (backfill, an edit).
    let side_url = "/mokuro-reader/Series/Vol%201.mokuro";
    let edited = mokuro("Vol 1", 2, "edited by the reader");
    let r = server.put(side_url, &edited, &[]).await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert_eq!(r.header("x-mokuro-upload"), Some("stored"));
    assert!(cloud(&fake, "Series/Vol 1.mokuro").as_deref() == Some(edited.as_slice()));
    let uploads = fake.upload_log().len();
    // Sent again unchanged: nothing is rewritten.
    let r = server.put(side_url, &edited, &[]).await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert_eq!(fake.upload_log().len(), uploads, "{:?}", fake.upload_log());
    // A new sidecar next to a new volume (a reader that OCR'd it itself).
    let r = server
        .put(
            "/mokuro-reader/Series/Vol%209.mokuro",
            &mokuro("Vol 9", 1, "x"),
            &[],
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(cloud(&fake, "Series/Vol 9.mokuro").is_some());
    server.idle().await;
    server.assert_no_staging_leak().await;

    // A full cloud account: 507 (disk-full, no retry), nothing staged.
    fake.set_quota(Some(1));
    let r = server
        .put("/mokuro-reader/Series/Vol%204.cbz", &cbz(2, 4), &[])
        .await;
    assert_eq!(r.status, 507, "{}", r.text());
    assert_eq!(r.json()["reason"], "disk-full");
    assert_eq!(r.json()["retry"], false);
    assert!(
        r.json()["detail"]
            .as_str()
            .unwrap()
            .contains("library storage is full"),
        "{}",
        r.text()
    );
    assert!(cloud(&fake, "Series/Vol 4.cbz").is_none());
    server.assert_no_staging_leak().await;
    server.stop().await;
}

/// The other bridge writes on a cloud library: MKCOL, COPY (a staged copy published
/// into the cloud), MOVE (by id) and DELETE (to the provider trash).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mkcol_copy_move_and_delete_work_on_a_cloud_library() {
    let fake = fake_library(FakeBackend::drive_like());
    let server = Server::start(&fake, no_lease).await;
    let original = cloud(&fake, "Series/Vol 1.cbz").unwrap();
    let r = server
        .dav("MKCOL", "/mokuro-reader/Other/", &[], None)
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(fake.node_at(&format!("{ROOT}/Other")).is_some());
    let dest = format!("{}/mokuro-reader/Other/Copy.cbz", server.url);
    let r = server
        .dav(
            "COPY",
            "/mokuro-reader/Series/Vol%201.cbz",
            &[("destination", &dest)],
            None,
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.text());
    assert!(cloud(&fake, "Other/Copy.cbz").as_deref() == Some(original.as_slice()));
    server.assert_no_staging_leak().await;
    let dest = format!("{}/mokuro-reader/Other/Moved.cbz", server.url);
    let r = server
        .dav(
            "MOVE",
            "/mokuro-reader/Other/Copy.cbz",
            &[("destination", &dest)],
            None,
        )
        .await;
    // A file MOVE answers 204 (as WsgiDAV's native move did, local libraries too).
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(cloud(&fake, "Other/Moved.cbz").is_some());
    assert!(cloud(&fake, "Other/Copy.cbz").is_none());
    let r = server
        .dav("DELETE", "/mokuro-reader/Other/Moved.cbz", &[], None)
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(cloud(&fake, "Other/Moved.cbz").is_none());
    server.assert_no_staging_leak().await;
    server.stop().await;
}

/// While this Bunko is still in its own first-claim settle wait (§9.2 step 1.2), a
/// bridge PUT is a clean 503 — not a 500 — whose detail says this Bunko is still
/// claiming the library and whose Retry-After is what is left of the wait; nothing
/// reaches the cloud and nothing is left staged. A MOVE gets the same; reads work.
/// The admin storage status shows the claim and the time left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_put_during_the_first_claim_is_a_503_with_the_time_left() {
    let fake = fake_library(FakeBackend::drive_like());
    let server = Server::start(&fake, slow_claim).await;
    let store = server.store();
    eventually("the settle wait", Duration::from_secs(20), || {
        store.lease().and_then(|l| l.claiming_left()).is_some()
    })
    .await;

    let vol = cbz(2, 5);
    let r = server
        .put("/mokuro-reader/Series/Vol%203.cbz", &vol, &[])
        .await;
    assert_eq!(r.status, 503, "{}", r.text());
    let retry: u64 = r.header("retry-after").unwrap().parse().unwrap();
    assert!((3000..=3600).contains(&retry), "Retry-After {retry}");
    let body = r.json();
    assert_eq!(body["ok"], false);
    assert_eq!(body["retry"], true);
    let detail = body["detail"].as_str().unwrap();
    assert!(
        detail.contains("this Bunko is still claiming the library") && detail.contains("min left"),
        "{detail}"
    );
    assert!(!detail.contains("another Bunko"), "{detail}");
    assert!(fake.node_at(&format!("{ROOT}/Series/Vol 3.cbz")).is_none());
    server.assert_no_staging_leak().await;
    // The refused write left no op (the HELD ones are this server's own covers and
    // queue file, waiting for the lease as Deferred ops do).
    let outbox = server.store().outbox();
    assert!(
        outbox
            .iter()
            .all(|o| o.deferred && o.dest != "Series/Vol 3.cbz"),
        "{outbox:?}"
    );

    // An overwrite and a MOVE: the same answer; the cloud file is untouched.
    let before = cloud(&fake, "Series/Vol 1.cbz").unwrap();
    let r = server
        .put("/mokuro-reader/Series/Vol%201.cbz", &vol, &[])
        .await;
    assert_eq!(r.status, 503, "{}", r.text());
    assert!(r.header("retry-after").is_some());
    assert!(cloud(&fake, "Series/Vol 1.cbz").as_deref() == Some(before.as_slice()));
    let dest = format!("{}/mokuro-reader/Series/Renamed.cbz", server.url);
    let r = server
        .dav(
            "MOVE",
            "/mokuro-reader/Series/Vol%201.cbz",
            &[("destination", &dest)],
            None,
        )
        .await;
    assert_eq!(r.status, 503, "{}", r.text());
    let retry: u64 = r.header("retry-after").unwrap().parse().unwrap();
    assert!(retry >= 3000, "Retry-After {retry}");
    assert!(r.text().contains("still claiming"), "{}", r.text());
    // Reads keep working.
    let got = server
        .dav("GET", "/mokuro-reader/Series/Vol%201.cbz", &[], None)
        .await;
    assert_eq!(got.status, 200);
    server.assert_no_staging_leak().await;

    // The admin storage status says the same.
    let st = server.admin_storage().await;
    assert_eq!(st["lease"]["state"], "claiming", "{st}");
    let left = st["lease"]["claiming_left_s"].as_u64().unwrap();
    assert!((3000..=3600).contains(&left), "{left}");
    assert!(
        st["lease"]["detail"]
            .as_str()
            .unwrap()
            .contains("still claiming the library"),
        "{st}"
    );
    let problems = st["problems"].as_array().unwrap();
    assert!(
        problems.iter().any(|p| p["kind"] == "storage-lease"
            && p["text"]
                .as_str()
                .unwrap()
                .starts_with("This Bunko is still claiming the library: about")),
        "{st}"
    );
    server.stop().await;
}

/// Another Bunko's fresh marker: this one is a standby, and a PUT is a 503 that says
/// another Bunko manages the library (with Retry-After), leaving nothing behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_put_while_another_bunko_holds_the_lease_is_a_503() {
    use bunko_remote::status::{LeaseEvent, MARKER_KIND, MARKER_NAME, Marker, MarkerState};
    let fake = fake_library(FakeBackend::drive_like());
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let marker = Marker {
        version: 1,
        kind: MARKER_KIND.into(),
        instance_id: "other-install".into(),
        boot_id: "b".into(),
        instance_name: "nas".into(),
        bunko_version: "0.8.0".into(),
        epoch: 5,
        claimed_at: now.clone(),
        heartbeat_at: now,
        ttl_s: 900,
        state: MarkerState::Active,
        compiles: vec![],
        ocr: true,
        queue: None,
        took_over_from: None,
    };
    fake.reader_put(
        &format!("{ROOT}/{MARKER_NAME}"),
        &serde_json::to_vec(&marker).unwrap(),
    );
    let server = Server::start(&fake, quick_claim).await;
    let store = server.store();
    eventually("standby", Duration::from_secs(20), || {
        matches!(
            store.lease().map(|l| l.event()),
            Some(LeaseEvent::Standby { .. })
        )
    })
    .await;
    let r = server
        .put("/mokuro-reader/Series/Vol%203.cbz", &cbz(2, 3), &[])
        .await;
    assert_eq!(r.status, 503, "{}", r.text());
    assert!(r.header("retry-after").is_some());
    let detail = r.json()["detail"].as_str().unwrap().to_string();
    assert!(
        detail.contains("another Bunko manages this library"),
        "{detail}"
    );
    assert!(fake.node_at(&format!("{ROOT}/Series/Vol 3.cbz")).is_none());
    server.assert_no_staging_leak().await;
    server.stop().await;
}
