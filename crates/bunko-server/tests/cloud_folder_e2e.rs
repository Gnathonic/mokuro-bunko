//! `cloud_e2e` on a local folder library (`storage.library.backend: folder`) over a
//! real directory instead of the in-memory `FakeBackend`: the folder is kept exactly
//! like a cloud folder. An un-OCR'd volume gets its sidecar, covers, `series.json` and
//! `catalog.json` written into the folder, and its series' `.mokuro-series-queue.json`
//! names it `done` (queue file v2); the lease marker is fresh and held; a store stopped
//! mid-upload resumes with exactly one copy of each result; a Chrome-like reader write of a new `.cbz` arrives
//! and is OCR'd; a reader-style rename (copy every file, then delete the old folder)
//! makes no OCR job for volumes whose sidecars moved with them.

// Fixtures are made and checked with std::fs on purpose: the folder is a plain folder.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bunko_core::Config;
use bunko_core::config::LibraryBackend;
use bunko_remote::RemoteStore;
use bunko_remote::folder::FolderBackend;
use bunko_server::app::{ServeOptions, Services, start_library_services};
use tokio_util::sync::CancellationToken;

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

/// The library folder, as Mokuro Reader's Local Folder leaves it: one series, a volume
/// with its OCR and one without.
fn seed(folder: &Path) {
    std::fs::create_dir_all(folder.join("Series")).unwrap();
    std::fs::write(folder.join("Series/Vol 1.cbz"), cbz(2)).unwrap();
    std::fs::write(folder.join("Series/Vol 1.mokuro"), mokuro("Vol 1", 2)).unwrap();
    std::fs::write(folder.join("Series/Vol 2.cbz"), cbz(3)).unwrap();
}

/// No lease (its claim waits before Deferred writes may leave) and a short queue-file
/// floor; everything else as configured.
fn quick(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    c.queue_floor = Some(Duration::from_millis(200));
    c.retry_base = Duration::from_millis(50);
}

/// The lease on, with a short settle wait.
fn leased(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = true;
    c.lease_settle = Some(Duration::from_millis(300));
    c.queue_floor = Some(Duration::from_millis(200));
    c.retry_base = Duration::from_millis(50);
}

struct FakeLocal(bunko_processor::FakePipeline);

impl bunko_server::ocr::LocalProcessorFactory for FakeLocal {
    fn start(&self, results_dir: &Path) -> Result<bunko_server::ocr::LocalChannels, String> {
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

fn config(base: PathBuf, folder: &Path) -> Config {
    let mut c = Config::default();
    c.storage.base_path = base;
    c.storage.library.backend = LibraryBackend::Folder;
    c.storage.library.path = Some(folder.to_path_buf());
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

/// One running server over the folder library at `folder`, its state at `base`.
struct Server {
    services: Services,
    stop: CancellationToken,
    running: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(
        base: &Path,
        folder: &Path,
        tune: bunko_server::storage::testing::Tune,
        before: impl FnOnce(&Services),
    ) -> Server {
        bunko_server::storage::test_cloud::register_tune(base, tune);
        let services = Services::new(config(base.to_path_buf(), folder), None, &options()).unwrap();
        let handle = services.storage.as_ref().expect("a folder library store");
        assert!(handle.connected(), "{:?}", handle.problem());
        before(&services);
        let stop = CancellationToken::new();
        let running = start_library_services(&services, stop.clone());
        Server {
            services,
            stop,
            running,
        }
    }

    fn store(&self) -> Arc<RemoteStore<FolderBackend>> {
        self.services
            .storage
            .as_ref()
            .unwrap()
            .folder_store()
            .expect("a folder store")
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

fn json_at(folder: &Path, rel: &str) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(folder.join(rel)).ok()?).ok()
}

/// The names in `dir` (files and folders), Bunko's hidden ones included.
fn names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

/// No temporary file of an upload is left anywhere in the folder.
fn assert_no_temp_files(dir: &Path) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        assert!(
            !(name.starts_with(".bunko-") && name.ends_with(".tmp")),
            "a temporary file was left: {}",
            e.path().display()
        );
        if e.file_type().unwrap().is_dir() {
            assert_no_temp_files(&e.path());
        }
    }
}

async fn idle(store: &Arc<RemoteStore<FolderBackend>>) {
    let s = store.clone();
    let idle = tokio::task::spawn_blocking(move || s.wait_outbox_idle(Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(idle, "the outbox drains: {:?}", store.outbox());
}

/// The library and the state directory, apart (the library is never inside Bunko's
/// own folder).
fn dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let folder = dir.path().join("Manga");
    seed(&folder);
    (dir, base, folder)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unocrd_volume_is_ocrd_and_everything_lands_in_the_folder() {
    let (_dir, base, folder) = dirs();
    let server = Server::start(&base, &folder, quick, |_| {}).await;
    let store = server.store();

    eventually(
        "the OCR result in the folder",
        Duration::from_secs(60),
        || folder.join("Series/Vol 2.mokuro").is_file(),
    )
    .await;
    let sidecar = json_at(&folder, "Series/Vol 2.mokuro").unwrap();
    assert_eq!(sidecar["volume"], "Vol 2");
    assert_eq!(sidecar["pages"].as_array().map(Vec::len), Some(3));
    eventually("both covers in the folder", Duration::from_secs(30), || {
        folder.join("Series/Vol 1.webp").is_file() && folder.join("Series/Vol 2.webp").is_file()
    })
    .await;

    assert!(server.services.library.recompile_all_now().await > 0);
    idle(&store).await;
    let series = json_at(&folder, "Series/series.json").expect("series.json in the folder");
    assert!(series.to_string().contains("Vol 2"), "{series}");
    let catalog = json_at(&folder, "catalog.json").expect("catalog.json in the folder");
    assert!(catalog.to_string().contains("Series"), "{catalog}");

    // The series' queue file (queue file v2) in the series folder: nothing pending,
    // the volume in `done`. No root queue file is written.
    eventually(
        "the series' queue file with its done entry",
        Duration::from_secs(30),
        || {
            json_at(&folder, "Series/.mokuro-series-queue.json")
                .is_some_and(|q| q["done"].get("Vol 2").is_some())
        },
    )
    .await;
    let queue = json_at(&folder, "Series/.mokuro-series-queue.json").unwrap();
    assert_eq!(queue["instance_id"], store.instance_id());
    assert_eq!(queue["vols"], serde_json::json!({}), "{queue}");
    assert!(!folder.join(".mokuro-queue.json").exists());

    // One copy of everything Bunko wrote, under its own name, and nothing half-written.
    idle(&store).await;
    let series_names = names(&folder.join("Series"));
    for name in [
        "Vol 1.cbz",
        "Vol 1.mokuro",
        "Vol 1.webp",
        "Vol 2.cbz",
        "Vol 2.mokuro",
        "Vol 2.webp",
        "series.json",
        ".mokuro-series-queue.json",
    ] {
        assert!(series_names.contains(name), "{name} in {series_names:?}");
    }
    assert!(
        series_names
            .iter()
            .all(|n| !n.contains(" (1)") && !n.contains("conflict")),
        "{series_names:?}"
    );
    assert_no_temp_files(&folder);
    // Nothing on disk under the synthetic root.
    assert!(!server.services.core.layout.library().exists());
    server.stop().await;
}

/// The marker lease: claimed after the first sync, fresh, in the folder itself (§9.2).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lease_marker_is_fresh_and_held_in_the_folder() {
    let (_dir, base, folder) = dirs();
    let server = Server::start(&base, &folder, leased, |_| {}).await;
    let store = server.store();
    let handle = server.services.storage.clone().unwrap();
    // The status asks the store's quota: a blocking call, never from an async task.
    let held = tokio::task::spawn_blocking(move || {
        let until = Instant::now() + Duration::from_secs(60);
        while handle.status_json()["lease"]["state"] != "held" {
            if Instant::now() > until {
                return Err(handle.status_json().to_string());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    })
    .await
    .unwrap();
    assert!(held.is_ok(), "the lease held: {held:?}");
    let marker = json_at(&folder, ".mokuro-bunko.json").expect("the marker in the folder");
    assert_eq!(marker["instance_id"], store.instance_id());
    assert_eq!(marker["kind"], "mokuro-bunko");
    assert_eq!(marker["state"], "active", "{marker}");
    let ttl = marker["ttl_s"].as_u64().unwrap();
    let age = std::fs::metadata(folder.join(".mokuro-bunko.json"))
        .unwrap()
        .modified()
        .unwrap()
        .elapsed()
        .unwrap_or_default();
    assert!(
        age < Duration::from_secs(ttl),
        "a fresh marker: {age:?} old"
    );
    server.stop().await;
}

/// The store stops right after the folder took the OCR result's bytes, before it
/// recorded the commit; after a restart the outbox reconciles it: every result is in
/// the folder exactly once, and the volume is not OCR'd again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_mid_upload_resumes_with_one_copy_of_each_result() {
    let (_dir, base, folder) = dirs();
    let watched = folder.join("Series/Vol 2.mokuro");
    let server = Server::start(&base, &folder, quick, move |services| {
        let store = services.storage.as_ref().unwrap().folder_store().unwrap();
        store.set_failpoint(Some(Arc::new(move |at: &str| {
            at == "uploaded" && watched.is_file()
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
    assert!(
        folder.join("Series/Vol 2.mokuro").is_file(),
        "the bytes were taken"
    );
    let failures = server.services.ocr.ask(|s| s.failures.len()).await.unwrap();
    assert_eq!(failures, 0);
    drop(store);
    server.stop().await;

    let server = Server::start(&base, &folder, quick, |_| {}).await;
    eventually(
        "the covers after the restart",
        Duration::from_secs(60),
        || folder.join("Series/Vol 1.webp").is_file() && folder.join("Series/Vol 2.webp").is_file(),
    )
    .await;
    idle(&server.store()).await;
    let series_names = names(&folder.join("Series"));
    let mokuros: Vec<&String> = series_names
        .iter()
        .filter(|n| n.starts_with("Vol 2") && n.ends_with(".mokuro"))
        .collect();
    assert_eq!(mokuros, ["Vol 2.mokuro"], "{series_names:?}");
    assert_eq!(
        json_at(&folder, "Series/Vol 2.mokuro").unwrap()["volume"],
        "Vol 2"
    );
    assert_no_temp_files(&folder);
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
    assert!(
        server.store().conflicts().is_empty(),
        "{:?}",
        server.store().conflicts()
    );
    let lib = server.services.core.layout.library();
    let meta = bunko_store::fs::metadata(lib.join("Series/Vol 2.mokuro")).unwrap();
    assert!(meta.own(), "the reconciled upload is Bunko's own version");
    let owed = server
        .services
        .ocr
        .ask(|s| s.owed.volumes.contains_key("Series/Vol 2.cbz"))
        .await
        .unwrap();
    assert!(!owed);
    server.stop().await;
}

/// Chrome's File System Access write, as Mokuro Reader's Local Folder makes it: an
/// empty `X` first, the bytes into `X.crswap`, then the swap file renamed over `X`.
/// The volume arrives through the watcher (no poll is asked for) and is OCR'd.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_chrome_like_reader_write_arrives_and_is_ocrd() {
    let (_dir, base, folder) = dirs();
    let server = Server::start(&base, &folder, quick, |_| {}).await;
    let store = server.store();
    eventually("the first OCR result", Duration::from_secs(60), || {
        folder.join("Series/Vol 2.mokuro").is_file()
    })
    .await;
    idle(&store).await;

    let target = folder.join("Series/Vol 3.cbz");
    let swap = folder.join("Series/Vol 3.cbz.crswap");
    std::fs::write(&target, b"").unwrap();
    std::fs::write(&swap, cbz(2)).unwrap();
    std::fs::rename(&swap, &target).unwrap();

    eventually(
        "the new volume OCR'd in the folder",
        Duration::from_secs(90),
        || folder.join("Series/Vol 3.mokuro").is_file(),
    )
    .await;
    assert_eq!(
        json_at(&folder, "Series/Vol 3.mokuro").unwrap()["pages"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );
    assert!(!swap.exists());
    idle(&store).await;
    assert_no_temp_files(&folder);
    server.stop().await;
}

/// Mokuro Reader's Local Folder renames a series by copying every file, then deleting
/// the old folder. The sidecars travel with their volumes, so no OCR job is made for
/// them under the new name, and nothing is written back under the old one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_style_rename_makes_no_ocr_job() {
    let (_dir, base, folder) = dirs();
    let server = Server::start(&base, &folder, quick, |_| {}).await;
    let store = server.store();
    eventually("every volume OCR'd", Duration::from_secs(60), || {
        folder.join("Series/Vol 2.mokuro").is_file()
    })
    .await;
    eventually("both covers", Duration::from_secs(30), || {
        folder.join("Series/Vol 1.webp").is_file() && folder.join("Series/Vol 2.webp").is_file()
    })
    .await;
    idle(&store).await;

    let old = folder.join("Series");
    let new = folder.join("Series Renamed");
    std::fs::create_dir(&new).unwrap();
    for e in std::fs::read_dir(&old).unwrap().flatten() {
        if e.file_type().unwrap().is_file() {
            std::fs::copy(e.path(), new.join(e.file_name())).unwrap();
        }
    }
    let sidecar_before = std::fs::read(new.join("Vol 2.mokuro")).unwrap();
    std::fs::remove_dir_all(&old).unwrap();

    let lib = server.services.core.layout.library();
    eventually(
        "the renamed series in the index",
        Duration::from_secs(60),
        || {
            bunko_store::fs::is_file(lib.join("Series Renamed/Vol 2.cbz"))
                && !bunko_store::fs::exists(lib.join("Series/Vol 2.cbz"))
        },
    )
    .await;
    // Give the scheduler's walk time to see the volumes under their new names.
    tokio::time::sleep(Duration::from_secs(5)).await;
    for v in ["Series Renamed/Vol 1.cbz", "Series Renamed/Vol 2.cbz"] {
        let owed = server
            .services
            .ocr
            .ask(move |s| s.owed.volumes.contains_key(v))
            .await
            .unwrap();
        assert!(!owed, "{v} is owed OCR");
    }
    idle(&store).await;
    assert_eq!(
        std::fs::read(new.join("Vol 2.mokuro")).unwrap(),
        sidecar_before,
        "the sidecar that moved is kept, not OCR'd again"
    );
    assert!(!old.exists(), "nothing is written back under the old name");
    server.stop().await;
}
