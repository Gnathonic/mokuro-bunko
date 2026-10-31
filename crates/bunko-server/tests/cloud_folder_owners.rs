//! Series folder owners (0.8) on a cloud library (the Drive-like `FakeBackend`): a
//! record is the series', so a case merge keeps it; it follows a folder moved in the
//! cloud and goes when the folder is deleted there; two MKCOLs at once make one owner.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_remote::fake::FakeBackend;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::Request;
use tower::ServiceExt;

const ROOT: &str = "mokuro-reader";

fn tune(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    // Long enough for the test to seed the records before the first merge pass.
    c.quiet = Duration::from_millis(1500);
    c.merge_interval = Duration::from_millis(50);
    // A removal in the cloud is applied once its tombstone grace (two polls) is over.
    c.poll_active = Duration::from_millis(100);
    c.poll_idle = Duration::from_millis(100);
}

struct Env {
    _dir: tempfile::TempDir,
    services: Services,
    app: Router,
}

async fn env(fake: Arc<FakeBackend>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    bunko_server::storage::testing::register_tuned(&base, fake.clone(), Some(tune));
    let mut config = Config::default();
    config.storage.base_path = base;
    config.storage.library.backend = LibraryBackend::GDrive;
    config.catalog.enrich_community = false;
    config.ocr.autobench = false;
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    for name in ["alice", "bob"] {
        services
            .db
            .create_user(name, "pass1234", Role::Uploader, UserStatus::Active, "")
            .unwrap();
    }
    let app = assemble(&services, &opts);
    Env {
        _dir: dir,
        services,
        app,
    }
}

impl Env {
    fn owner(&self, folder: &str) -> Option<String> {
        self.services.db.series_folder_owner(folder).unwrap()
    }

    fn library(&self) -> PathBuf {
        self.services.core.layout.library()
    }

    /// Have the store read the cloud's changes now.
    fn poll(&self) {
        let storage = self.services.storage.as_ref().expect("a cloud library");
        storage
            .fake_store()
            .expect("the fake store")
            .feed()
            .poll_now();
    }

    async fn mkcol(&self, user: &str, path: &str) -> u16 {
        use base64::Engine;
        let auth = base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"));
        let req = Request::builder()
            .method("MKCOL")
            .uri(path.replace(' ', "%20"))
            .header("host", "localhost:8080")
            .header("authorization", format!("Basic {auth}"))
            .body(Body::empty())
            .unwrap();
        self.app
            .clone()
            .oneshot(req)
            .await
            .unwrap()
            .status()
            .as_u16()
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
async fn a_record_follows_its_series_in_the_cloud() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Kingdom/Vol 1.cbz"), b"one");
    fake.reader_put(&format!("{ROOT}/kingdom/Vol 2.cbz"), b"two");
    let e = env(fake.clone()).await;
    e.services
        .db
        .record_series_folder_owner("Kingdom", "alice")
        .unwrap();
    // The case merge joins `kingdom/` into `Kingdom/`: one series, one record.
    let f = fake.clone();
    eventually("the case merge", || {
        f.node_at(&format!("{ROOT}/kingdom")).is_none()
    })
    .await;
    assert_eq!(e.owner("Kingdom").as_deref(), Some("alice"));

    // Renamed in the cloud: the record moves with it.
    fake.reader_rename(
        &format!("{ROOT}/Kingdom"),
        &format!("{ROOT}/Kingdom (2006)"),
    );
    e.poll();
    eventually("the rename's record", || {
        e.owner("Kingdom (2006)").as_deref() == Some("alice")
    })
    .await;
    assert_eq!(e.owner("Kingdom"), None);

    // Deleted in the cloud: forgotten.
    // A removal is applied once its tombstone grace is over.
    fake.reader_delete(&format!("{ROOT}/Kingdom (2006)"));
    eventually("the delete's record", || {
        e.poll();
        e.owner("Kingdom (2006)").is_none()
    })
    .await;
}

/// The cloud index's create is a lookup and then a make: two MKCOLs of one folder at
/// once still create it once, for one owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_mkcols_at_once_make_one_owner() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/Other/Vol 1.cbz"), b"one");
    let e = env(fake.clone()).await;
    let storage = e.services.storage.clone().expect("a cloud library");
    let steady = tokio::task::spawn_blocking(move || storage.wait_steady(Duration::from_secs(20)))
        .await
        .unwrap();
    assert!(steady);
    let (a, b) = tokio::join!(
        e.mkcol("alice", "/mokuro-reader/Race"),
        e.mkcol("bob", "/mokuro-reader/Race")
    );
    assert!([a, b].contains(&201), "{a} {b}");
    let owner = e.owner("Race").expect("an owner");
    let winner = if a == 201 && b != 201 {
        "alice"
    } else if b == 201 && a != 201 {
        "bob"
    } else {
        owner.as_str()
    };
    assert_eq!(owner, winner);
    let folders: Vec<String> = bunko_store::fs::read_dir(e.library())
        .unwrap()
        .flatten()
        .map(|d| d.file_name().to_string_lossy().into_owned())
        .filter(|n| n == "Race")
        .collect();
    assert_eq!(folders.len(), 1, "{folders:?}");
}

/// Startup adoption reads a cloud library through its store (F5): once the store is in
/// sync, a folder of one uploader becomes its, a mixed one stays shared, and adoption is
/// recorded as done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cloud_library_is_adopted_once_in_sync() {
    let fake = Arc::new(FakeBackend::drive_like());
    for rel in [
        "Solo/Vol 1.cbz",
        "Solo/Vol 2.cbz",
        "Solo/Vol 1.mokuro",
        "Mixed/Vol 1.cbz",
        "Mixed/Vol 2.cbz",
        "Untracked/Vol 1.cbz",
        "Untracked/Vol 2.cbz",
    ] {
        fake.reader_put(&format!("{ROOT}/{rel}"), b"x");
    }
    let e = env(fake.clone()).await;
    let db = e.services.db.clone();
    // A database from before 0.8: tracked uploads, adoption pending.
    db.with_writer_connection(|c| c.execute("DELETE FROM series_folder_adoption", []))
        .unwrap();
    for (rel, user) in [
        ("Solo/Vol 1.cbz", "alice"),
        ("Solo/Vol 2.cbz", "alice"),
        ("Mixed/Vol 1.cbz", "alice"),
        ("Mixed/Vol 2.cbz", "bob"),
        ("Untracked/Vol 1.cbz", "alice"),
    ] {
        db.record_volume_upload(rel, user).unwrap();
    }
    let storage = e.services.storage.clone().expect("a cloud library");
    let steady = tokio::task::spawn_blocking(move || storage.wait_steady(Duration::from_secs(20)))
        .await
        .unwrap();
    assert!(steady);
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::time::timeout(
        Duration::from_secs(20),
        bunko_server::library::adopt_series_folder_owners_until_done(
            db.clone(),
            e.library(),
            Duration::from_millis(50),
            stop.clone(),
        ),
    )
    .await
    .expect("adopted");
    assert!(!db.series_folder_adoption_pending().unwrap());
    assert_eq!(e.owner("Solo").as_deref(), Some("alice"));
    assert_eq!(e.owner("Mixed"), None);
    assert_eq!(e.owner("Untracked"), None);
}
