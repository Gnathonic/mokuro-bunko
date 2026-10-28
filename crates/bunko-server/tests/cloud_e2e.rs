//! A whole server on a cloud library (the in-memory `FakeBackend`), write side: an
//! un-OCR'd volume is OCR'd by this server's own processor (`FakePipeline`) and its
//! sidecar, the covers, `series.json`, `catalog.json` and the queue file (listing the
//! result in `done[]`) land in the cloud folder; the lease marker appears; a store that
//! stops mid-upload resumes after a restart with exactly one copy of each result.

// Fixtures are checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bunko_core::Config;
use bunko_core::config::LibraryBackend;
use bunko_remote::Backend as _;
use bunko_remote::fake::FakeBackend;
use bunko_server::app::{ServeOptions, Services, start_library_services};
use tokio_util::sync::CancellationToken;

const ROOT: &str = "mokuro-reader";

/// A `.cbz` of `pages` small PNG pages (real images: the covers decode them).
fn cbz(pages: usize) -> Vec<u8> {
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

fn mokuro(volume: &str, pages: usize) -> Vec<u8> {
    let pages: Vec<serde_json::Value> = (0..pages)
        .map(|i| {
            serde_json::json!({"img_path": format!("{:03}.png", i + 1), "img_width": 24, "img_height": 32, "blocks": []})
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({
        "version": "0.2.5", "title": "Series", "title_uuid": "t-uuid",
        "volume": volume, "volume_uuid": format!("{volume}-uuid"), "pages": pages,
    }))
    .unwrap()
}

fn fake_library() -> Arc<FakeBackend> {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), &cbz(2));
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.mokuro"), &mokuro("Vol 1", 2));
    fake.reader_put(&format!("{ROOT}/Series/Vol 2.cbz"), &cbz(3));
    fake
}

/// No lease (its claim waits a minute before Deferred writes may leave) and a short
/// queue-file floor; everything else as configured.
fn quick(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    c.queue_floor = Some(Duration::from_millis(200));
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
    c.server.serve_readers = false;
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

/// One running server over the cloud library at `base`.
struct Server {
    services: Services,
    stop: CancellationToken,
    running: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(
        base: &std::path::Path,
        fake: &Arc<FakeBackend>,
        tune: Option<bunko_server::storage::testing::Tune>,
        before: impl FnOnce(&Services),
    ) -> Server {
        bunko_server::storage::testing::register_tuned(base, fake.clone(), tune);
        let services = Services::new(config(base.to_path_buf()), None, &options()).unwrap();
        before(&services);
        let stop = CancellationToken::new();
        let running = start_library_services(&services, stop.clone());
        Server {
            services,
            stop,
            running,
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

    async fn stop(self) {
        self.stop.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(10), self.running).await;
        self.services.ocr.stop().await;
        self.services.library.stop().await;
        drop(self.services);
    }
}

async fn eventually(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn node(fake: &FakeBackend, rel: &str) -> Option<bunko_remote::RNode> {
    fake.node_at(&format!("{ROOT}/{rel}"))
}

fn json_at(fake: &FakeBackend, rel: &str) -> Option<serde_json::Value> {
    let n = node(fake, rel)?;
    serde_json::from_slice(&fake.content(&n.pid)?).ok()
}

async fn copies(fake: &FakeBackend, dir: &str, name: &str) -> usize {
    let parent = node(fake, dir).unwrap().pid;
    fake.find_named(&parent, name).await.unwrap().0.len()
}

async fn idle(store: &Arc<bunko_remote::RemoteStore<FakeBackend>>) {
    let s = store.clone();
    let idle = tokio::task::spawn_blocking(move || s.wait_outbox_idle(Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(idle, "the outbox drains: {:?}", store.outbox());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unocrd_cloud_volume_is_ocrd_and_everything_lands_in_the_cloud() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let fake = fake_library();
    let server = Server::start(&base, &fake, Some(quick), |_| {}).await;
    let store = server.store();

    eventually(
        "the OCR result in the cloud",
        Duration::from_secs(60),
        || node(&fake, "Series/Vol 2.mokuro").is_some(),
    )
    .await;
    let sidecar = json_at(&fake, "Series/Vol 2.mokuro").unwrap();
    assert_eq!(sidecar["volume"], "Vol 2");
    assert_eq!(sidecar["pages"].as_array().map(Vec::len), Some(3));
    eventually("both covers in the cloud", Duration::from_secs(30), || {
        node(&fake, "Series/Vol 1.webp").is_some() && node(&fake, "Series/Vol 2.webp").is_some()
    })
    .await;
    assert!(
        node(&fake, "Series/Vol 1.nocover").is_none()
            && node(&fake, "Series/Vol 2.nocover").is_none()
    );

    // The metadata compile (its timers are minutes long; run a pass now).
    assert!(server.services.library.recompile_all_now().await > 0);
    idle(&store).await;
    let series = json_at(&fake, "Series/series.json").expect("series.json in the cloud");
    assert!(series.to_string().contains("Vol 2"), "{series}");
    let catalog = json_at(&fake, "catalog.json").expect("catalog.json in the cloud");
    assert!(catalog.to_string().contains("Series"), "{catalog}");

    // The cloud queue file: nothing pending, the result in `done[]`.
    eventually(
        "the queue file with its done entry",
        Duration::from_secs(30),
        || {
            json_at(&fake, ".mokuro-queue.json").is_some_and(|q| {
                q["done"]
                    .as_array()
                    .is_some_and(|d| d.iter().any(|e| e["volume"] == "Vol 2"))
            })
        },
    )
    .await;
    let queue = json_at(&fake, ".mokuro-queue.json").unwrap();
    assert_eq!(queue["instance_id"], store.instance_id());
    assert_eq!(queue["pending_volumes"], 0, "{queue}");
    // Exactly one copy of everything Bunko wrote.
    for name in ["Vol 2.mokuro", "Vol 1.webp", "Vol 2.webp", "series.json"] {
        assert_eq!(copies(&fake, "Series", name).await, 1, "{name}");
    }
    // Nothing on disk under the synthetic root.
    assert!(!server.services.core.layout.library().exists());
    server.stop().await;
}

/// The marker lease: claimed after the first sync, in the cloud root (§9.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lease_marker_appears_in_the_cloud() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let fake = fake_library();
    let server = Server::start(&base, &fake, None, |_| {}).await;
    let store = server.store();
    eventually("the lease marker", Duration::from_secs(30), || {
        json_at(&fake, ".mokuro-bunko.json").is_some()
    })
    .await;
    let marker = json_at(&fake, ".mokuro-bunko.json").unwrap();
    assert_eq!(marker["instance_id"], store.instance_id());
    assert_eq!(marker["kind"], "mokuro-bunko");
    server.stop().await;
}

/// The store stops right after the provider took the OCR result's bytes, before it
/// recorded the commit (a crash between UPLOADING and COMMITTED); after a restart the
/// outbox reconciles it: every result is in the cloud exactly once, and the volume is
/// not OCR'd again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_mid_upload_resumes_with_one_copy_of_each_result() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let fake = fake_library();
    let watched = fake.clone();
    let server = Server::start(&base, &fake, Some(quick), move |services| {
        let store = services.storage.as_ref().unwrap().fake_store().unwrap();
        store.set_failpoint(Some(Arc::new(move |at: &str| {
            at == "uploaded"
                && watched
                    .upload_log()
                    .last()
                    .is_some_and(|n| n == "Vol 2.mokuro")
        })));
    })
    .await;
    let store = server.store();
    eventually(
        "the store to stop mid-upload",
        Duration::from_secs(60),
        || store.crashed(),
    )
    .await;
    assert_eq!(
        copies(&fake, "Series", "Vol 2.mokuro").await,
        1,
        "the bytes were taken"
    );
    let failures = server.services.ocr.ask(|s| s.failures.len()).await.unwrap();
    assert_eq!(failures, 0);
    drop(store);
    server.stop().await;

    let server = Server::start(&base, &fake, Some(quick), |_| {}).await;
    eventually(
        "the covers after the restart",
        Duration::from_secs(60),
        || node(&fake, "Series/Vol 1.webp").is_some() && node(&fake, "Series/Vol 2.webp").is_some(),
    )
    .await;
    idle(&server.store()).await;
    for name in ["Vol 2.mokuro", "Vol 1.webp", "Vol 2.webp"] {
        assert_eq!(copies(&fake, "Series", name).await, 1, "{name}");
    }
    // The queue file is republished every few seconds: a fresh write of it may be
    // in flight whenever this looks.
    assert!(
        server
            .store()
            .outbox()
            .iter()
            .filter(|o| o.origin != "status")
            .all(|o| o.state == "COMMITTED" || o.state == "CONFIRMED"),
        "{:?}",
        server.store().outbox()
    );
    let lib = server.services.core.layout.library();
    let meta = bunko_store::fs::metadata(lib.join("Series/Vol 2.mokuro")).unwrap();
    assert!(meta.own(), "the reconciled upload is Bunko's own version");
    // Not OCR'd again: the volume owes nothing.
    let owed = server
        .services
        .ocr
        .ask(|s| s.owed.volumes.contains_key("Series/Vol 2.cbz"))
        .await
        .unwrap();
    assert!(!owed);
    server.stop().await;
}

/// No metadata pass runs before first adoption (§9.4) ends: while the series.json
/// submissions cannot be read the library services wait (a pass would compile the
/// series on top of a version not read yet), then they start, and the compiled
/// series.json keeps the facts it was adopted with.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_metadata_passes_wait_for_first_adoption() {
    use std::sync::atomic::{AtomicBool, Ordering};

    use bunko_remote::BErr;
    use bunko_remote::fake::FakeOp;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let fake = fake_library();
    fake.reader_put(
        &format!("{ROOT}/Series/series.json"),
        br#"{"version":2,"series_title":"Series","updated_at":"2026-01-01T00:00:00.000Z","external_ids":{"anilist":4242},"titles":{"english":"The Series"},"volumes":[]}"#,
    );
    // Every content read fails while the gate is shut.
    let gate = Arc::new(AtomicBool::new(true));
    {
        let (gate, weak) = (gate.clone(), Arc::downgrade(&fake));
        fake.set_call_hook(Some(Arc::new(move |op| {
            if op == FakeOp::Read
                && gate.load(Ordering::SeqCst)
                && let Some(fake) = weak.upgrade()
            {
                fake.fail_next(FakeOp::Read, BErr::Transient("gated".into()));
            }
        })));
    }
    let server = Server::start(&base, &fake, Some(quick), |_| {}).await;
    let store = server.store();
    let s = store.clone();
    let steady = tokio::task::spawn_blocking(move || s.wait_steady(Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(steady);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!store.adopted());
    assert!(
        !server.services.library.is_started(),
        "the library services wait for first adoption"
    );

    gate.store(false, Ordering::SeqCst);
    eventually(
        "first adoption and the library services",
        Duration::from_secs(60),
        || store.adopted() && server.services.library.is_started(),
    )
    .await;
    server.services.library.recompile_all_now().await;
    idle(&store).await;
    let series = json_at(&fake, "Series/series.json").expect("series.json in the cloud");
    assert_eq!(series["external_ids"]["anilist"], 4242, "{series}");
    assert!(series.to_string().contains("Vol 1"), "{series}");
    assert!(store.conflicts().is_empty(), "{:?}", store.conflicts());
    fake.set_call_hook(None);
    server.stop().await;
}
