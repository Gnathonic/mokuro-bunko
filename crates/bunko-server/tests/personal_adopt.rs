//! Physical correction files nobody owns (D27, C3-B8). A library from before 0.8 can
//! hold `<Vol>.edit.mokuro` files written into the shared folder (a hand-made layer
//! slugged `edit`, an early reader push). 0.8 resolves that name per account, so such a
//! file is invisible to everyone. At start a library on this disk gives each one to the
//! account that last wrote it (the audit log); one with no writer on record stays hidden
//! and is counted in the storage status. In a cloud library they are the progress
//! user's; with none chosen they are only counted.

// Fixtures are written and checked with std::fs on purpose (local library, users/).
#![allow(clippy::disallowed_methods)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::{NewAuditEvent, UserStatus};
use bunko_remote::fake::FakeBackend;
use bunko_server::app::{ServeOptions, Services, start_library_services};
use bunko_server::library::adopt_unowned_corrections;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const ROOT: &str = "mokuro-reader";

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    stop: CancellationToken,
}

impl Drop for Env {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Env {
    fn library(&self) -> PathBuf {
        self.storage.join("library")
    }

    fn overlay(&self, user: &str, rel: &str) -> PathBuf {
        self.storage
            .join("users")
            .join(user)
            .join("edits")
            .join(rel)
    }

    /// The `problems[]` of the admin storage status (which blocks on a cloud store).
    fn problems(&self) -> Vec<Value> {
        tokio::task::block_in_place(|| {
            self.services.storage_control.status_json()["problems"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
    }

    fn unowned_problem(&self) -> Option<Value> {
        self.problems()
            .into_iter()
            .find(|p| p["kind"] == "unowned-corrections")
    }

    /// A WebDAV write as 0.7.x logged it (a library file).
    fn audit(&self, action: &str, actor: &str, target_type: &str, rel: &str) {
        let path = format!("/{ROOT}/{rel}");
        self.services
            .db
            .log_audit_event(
                &NewAuditEvent::new(action)
                    .actor(Some(actor))
                    .target_type(target_type)
                    .target_path(&path),
            )
            .unwrap();
    }
}

/// A sidecar marked with `who` (valid: the corrupt-sidecar sweep runs first).
fn layer(who: &str) -> Vec<u8> {
    format!(r#"{{"pages":[],"who":"{who}"}}"#).into_bytes()
}

fn write(path: &Path, body: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// Services on a fresh storage (`fake`: a cloud library on it), with accounts, but the
/// library services not started yet.
async fn services(backend: LibraryBackend, fake: Option<Arc<FakeBackend>>) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    if let Some(fake) = &fake {
        bunko_server::storage::testing::register_tuned(
            &storage,
            fake.clone(),
            Some(|c: &mut bunko_remote::RemoteStoreConfig| c.lease = false),
        );
    } else {
        std::fs::create_dir_all(storage.join("library")).unwrap();
    }
    let mut config = Config::default();
    config.storage.base_path = storage.clone();
    config.storage.library.backend = backend;
    config.server.serve_readers = true;
    config.ocr.autobench = false;
    config.catalog.enrich_community = false;
    let services = Services::new(config, None, &ServeOptions::default()).unwrap();
    for (name, status) in [
        ("alice", UserStatus::Active),
        ("carol", UserStatus::Active),
        ("bob", UserStatus::Disabled),
    ] {
        services
            .db
            .create_user(name, "pass1234", Role::Registered, status, "")
            .unwrap();
    }
    if let Some(storage) = services.storage.clone() {
        let steady =
            tokio::task::spawn_blocking(move || storage.wait_steady(Duration::from_secs(20)))
                .await
                .unwrap();
        assert!(steady, "the first sync finishes");
    }
    Env {
        _dir: dir,
        storage,
        services,
        stop: CancellationToken::new(),
    }
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_library_gives_each_correction_to_its_last_writer() {
    let e = services(LibraryBackend::Local, None).await;
    let lib = e.library();
    for vol in ["V", "W", "X", "Y"] {
        write(&lib.join(format!("S/{vol}.cbz")), b"volume");
    }
    // Written by carol, then by alice: the newest library write decides. A later
    // overlay write of carol's own is no claim on the shared file.
    write(&lib.join("S/V.edit.mokuro"), &layer("alice"));
    e.audit("upload", "carol", "library", "S/V.edit.mokuro");
    e.audit("edit", "alice", "library", "S/V.edit.mokuro");
    e.audit("edit", "carol", "personal", "S/V.edit.mokuro");
    // No writer on record.
    write(&lib.join("S/W.edit.mokuro"), &layer("nobody"));
    // alice already has her own correction there: this one moves in beside it.
    write(&lib.join("S/X.edit.mokuro"), &layer("alice-older"));
    write(&e.overlay("alice", "S/X.edit.mokuro"), &layer("alice-own"));
    e.audit("upload", "alice", "library", "S/X.edit.mokuro");
    // Its writer's account is disabled: nobody to give it to.
    write(&lib.join("S/Y.edit-2.mokuro"), &layer("bob"));
    e.audit("upload", "bob", "library", "S/Y.edit-2.mokuro");
    // A dotted volume's own primary is no correction.
    write(&lib.join("S/D.edit.cbz"), b"volume");
    write(&lib.join("S/D.edit.mokuro"), &layer("primary"));

    let _running = start_library_services(&e.services, e.stop.clone());
    eventually("the startup sweep", || {
        !lib.join("S/V.edit.mokuro").exists() && !lib.join("S/X.edit.mokuro").exists()
    })
    .await;
    assert_eq!(
        std::fs::read(e.overlay("alice", "S/V.edit.mokuro")).unwrap(),
        layer("alice")
    );
    assert!(!e.overlay("carol", "S/V.edit.mokuro").exists());
    assert_eq!(
        std::fs::read(e.overlay("alice", "S/X.edit.mokuro")).unwrap(),
        layer("alice-own")
    );
    assert_eq!(
        std::fs::read(e.overlay("alice", "S/X.edit-2.mokuro")).unwrap(),
        layer("alice-older")
    );
    for kept in ["S/W.edit.mokuro", "S/Y.edit-2.mokuro", "S/D.edit.mokuro"] {
        assert!(lib.join(kept).exists(), "{kept}");
    }
    assert!(!e.overlay("bob", "S/Y.edit-2.mokuro").exists());

    eventually("the report", || e.unowned_problem().is_some()).await;
    let p = e.unowned_problem().unwrap();
    assert_eq!(p["severity"], "warn");
    assert_eq!(p["text"], "2 correction files have no owner");

    // A second sweep finds nothing more to give.
    let overlays = e.services.dav.overlays();
    let again = tokio::task::spawn_blocking({
        let db = e.services.db.clone();
        move || adopt_unowned_corrections(&db, &overlays, true)
    })
    .await
    .unwrap();
    assert_eq!((again.adopted, again.unowned), (0, 2));
    assert_eq!(
        std::fs::read(e.overlay("alice", "S/V.edit.mokuro")).unwrap(),
        layer("alice")
    );
    assert!(!e.overlay("alice", "S/X.edit-3.mokuro").exists());
    for kept in ["S/W.edit.mokuro", "S/Y.edit-2.mokuro"] {
        assert!(lib.join(kept).exists(), "{kept}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_local_library_with_nothing_unowned_reports_nothing() {
    let e = services(LibraryBackend::Local, None).await;
    write(&e.library().join("S/V.cbz"), b"volume");
    write(&e.library().join("S/V.edit.mokuro"), &layer("alice"));
    e.audit("upload", "alice", "library", "S/V.edit.mokuro");
    let _running = start_library_services(&e.services, e.stop.clone());
    eventually("the startup sweep", || {
        e.overlay("alice", "S/V.edit.mokuro").exists()
    })
    .await;
    // Give a report the time it would take.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(e.unowned_problem().is_none(), "{:?}", e.problems());
}

/// A cloud library with no progress user: the cloud folder's corrections are nobody's
/// yet. Nothing is moved out of the cloud; the admin is told to pick their owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cloud_library_without_an_owner_only_reports() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put(&format!("{ROOT}/S/V.cbz"), b"volume");
    fake.reader_put(&format!("{ROOT}/S/V.edit.mokuro"), br#"{"pages":[]}"#);
    let e = services(LibraryBackend::GDrive, Some(fake.clone())).await;
    e.audit("upload", "alice", "library", "S/V.edit.mokuro");
    let _running = start_library_services(&e.services, e.stop.clone());
    eventually("the report", || e.unowned_problem().is_some()).await;
    let p = e.unowned_problem().unwrap();
    assert_eq!(p["text"], "1 correction file has no owner");
    assert!(
        p["hint"]
            .as_str()
            .is_some_and(|h| h.contains("Reading progress linked to the cloud")),
        "{p}"
    );
    assert!(fake.node_at(&format!("{ROOT}/S/V.edit.mokuro")).is_some());
    assert!(!e.overlay("alice", "S/V.edit.mokuro").exists());

    // Once an owner is chosen they are that account's: nothing left to report.
    e.services.core.config.write().storage.library.progress_user = Some("alice".into());
    assert!(e.unowned_problem().is_none(), "{:?}", e.problems());
}
