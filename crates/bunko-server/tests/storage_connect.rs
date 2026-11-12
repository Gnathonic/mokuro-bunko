//! Where the library lives, through the pages (0.8 §7, §11; M6): the setup wizard's
//! storage steps and its finish, the admin Storage panel's API, and Disconnect on a
//! cloud store (the in-memory `FakeBackend`): drain, export, discard, the store
//! directory removed at the next start, nothing removed in the cloud.

// Fixtures are checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod accounts_support;
mod admin_support;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bunko_core::Config;
use bunko_core::config::LibraryBackend;
use bunko_remote::fake::FakeBackend;
use bunko_remote::mega::{LoginError, SessionBlob};
use bunko_remote::{AccountInfo, BErr};
use bunko_server::storage::StorageControl;
use bunko_server::storage::connect::{
    Auth, CheckReport, Connector, ProviderOps, Tuning, check_backend,
};
use bunko_server::storage::disconnect::{DisconnectError, DisconnectMode};
use bunko_store::{Durability, Mode, Origin, WriteIntent};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

const ROOT: &str = "mokuro-reader";

/// The Check on a FakeBackend, as account `acct-1`.
struct FakeOps(Arc<FakeBackend>);

impl ProviderOps for FakeOps {
    fn inspect<'a>(
        &'a self,
        _backend: LibraryBackend,
        _auth: &'a Auth,
        folder: &'a str,
        create: bool,
        _scratch: &'a Path,
        ours: &'a HashSet<String>,
    ) -> BoxFuture<'a, Result<(AccountInfo, CheckReport), BErr>> {
        Box::pin(async move {
            let (_, check) = check_backend(&*self.0, folder, create, ours).await?;
            Ok((
                AccountInfo {
                    id: "acct-1".into(),
                    label: "me@example.com".into(),
                },
                check,
            ))
        })
    }

    fn mega_login<'a>(
        &'a self,
        _email: &'a str,
        _password: &'a str,
        _totp: Option<&'a str>,
    ) -> BoxFuture<'a, Result<SessionBlob, LoginError>> {
        Box::pin(async { Ok(SessionBlob::new("mega-session")) })
    }
}

fn fake_library() -> Arc<FakeBackend> {
    let fake = Arc::new(FakeBackend::webdav_cas());
    fake.reader_put(&format!("{ROOT}/Series/Vol 1.cbz"), b"zip");
    fake.reader_put(&format!("{ROOT}/Series/Vol 2.cbz"), b"zip");
    fake
}

/// A StorageControl over `core` whose Connect runs on `fake`.
fn control_over(core: &bunko_server::Core, fake: Arc<FakeBackend>) -> Arc<StorageControl> {
    let (rc, wc) = (core.clone(), core.clone());
    let read = Arc::new(move || rc.config.read().clone());
    let write = Arc::new(move |f: &mut dyn FnMut(&mut Config)| {
        {
            let mut c = wc.config.write();
            f(&mut c);
        }
        wc.save_config().map_err(|e| e.to_string())
    });
    let (r2, w2) = (read.clone(), write.clone());
    let connector = Connector::with_ops(
        core.layout.cloud_dir(),
        Arc::new(move || r2().storage.library),
        Arc::new(move |choice| w2(&mut |c: &mut Config| choice.apply_to(&mut c.storage.library))),
        Arc::new(FakeOps(fake)),
        Tuning::default(),
    );
    StorageControl::new(core.layout.cloud_dir(), None, read, write, Some(connector))
}

fn webdav_form(password: &str) -> Value {
    json!({
        "provider": "webdav",
        "webdav": {"url": "https://nas.example/dav", "username": "me", "password": password},
    })
}

async fn eventually(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let until = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------- the setup wizard

mod wizard {
    use super::accounts_support::*;
    use super::*;

    fn env() -> Env {
        let mut env = Env::new();
        env.deps.storage = Some(control_over(&env.deps.core, fake_library()));
        env
    }

    async fn poll_connected(env: &Env) -> Value {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let r = env
                .send(empty(req("GET", "/setup/api/storage/connect")))
                .await;
            assert_eq!(r.status, 200, "{}", r.text());
            let v = r.json();
            if !matches!(
                v["state"].as_str(),
                Some("starting" | "checking" | "code" | "browser")
            ) {
                return v;
            }
            assert!(Instant::now() < until, "{v}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Steps 2-4 for WebDAV: the form, the live probe, the Check; config.yaml names
    /// the server and user, the password goes to credentials.json only.
    #[tokio::test]
    async fn the_storage_steps_write_the_config_and_the_credentials() {
        let env = env();
        let r = env.send(empty(req("GET", "/setup/api/storage"))).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["backend"], "local");
        assert_eq!(v["connect"]["state"], "idle");
        assert_eq!(v["configured"]["backend"], "local");
        assert_eq!(v["serve_readers"], true);

        let r = env
            .send(json_body(
                req("POST", "/setup/api/storage/connect"),
                webdav_form("app-pass-secret"),
            ))
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(!r.text().contains("app-pass-secret"));
        let v = poll_connected(&env).await;
        assert_eq!(v["state"], "connected", "{v}");
        assert_eq!(
            v["check"]["summary"],
            "Found mokuro-reader/ — 2 volumes in 1 series"
        );
        assert!(!v.to_string().contains("app-pass-secret"));

        let saved = std::fs::read_to_string(env.dir.path().join("config.yaml")).unwrap();
        assert!(saved.contains("backend: webdav"), "{saved}");
        assert!(saved.contains("https://nas.example/dav"), "{saved}");
        assert!(!saved.contains("app-pass-secret"), "{saved}");
        let creds =
            std::fs::read_to_string(env.deps.core.layout.cloud_dir().join("credentials.json"))
                .unwrap();
        assert!(creds.contains("app-pass-secret"));
        let c = env.deps.core.config.read().clone();
        assert_eq!(c.storage.library.backend, LibraryBackend::WebDav);
        assert_eq!(c.storage.library.webdav.username.as_deref(), Some("me"));
        // The status says a restart opens it; the store is not mounted yet.
        let v = env
            .send(empty(req("GET", "/setup/api/storage")))
            .await
            .json();
        assert_eq!(v["restart_pending"], true);
        assert_eq!(v["configured"]["backend"], "webdav");
    }

    /// Step 5 and the finish: readers off (the cloud default) is saved; the connected
    /// library makes the server restart; "link my reading progress" names the admin.
    #[tokio::test]
    async fn the_finish_saves_the_readers_answer_and_restarts_onto_the_library() {
        let mut env = env();
        let restarts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r2 = restarts.clone();
        env.deps.restart = Some(Arc::new(move || {
            r2.fetch_add(1, Ordering::SeqCst);
        }));
        env.deps
            .storage
            .as_ref()
            .unwrap()
            .set_restart(env.deps.restart.clone().unwrap());
        env.send(json_body(
            req("POST", "/setup/api/storage/connect"),
            webdav_form("pw"),
        ))
        .await;
        assert_eq!(poll_connected(&env).await["state"], "connected");
        let r = env
            .send(json_body(
                req("POST", "/setup/api/complete"),
                json!({
                    "admin": {"username": "admin", "password": "password123"},
                    "registration": {"mode": "disabled"},
                    "library": {"serve_readers": false},
                }),
            ))
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
        let v = r.json();
        assert_eq!(v["restarting"], true, "{v}");
        assert_eq!(v["storage_restart"], true, "{v}");
        assert!(!env.deps.core.config.read().server.serve_readers);
        eventually("the restart", Duration::from_secs(5), || {
            restarts.load(Ordering::SeqCst) == 1
        })
        .await;
        // Done: the storage steps are the admin panel's now.
        let r = env.send(empty(req("GET", "/setup/api/storage"))).await;
        assert_eq!(r.status, 410);
    }

    #[tokio::test]
    async fn readers_yes_can_link_the_admins_progress_to_the_cloud() {
        let env = env();
        env.config().storage.library.backend = LibraryBackend::GDrive;
        let r = env
            .send(json_body(
                req("POST", "/setup/api/complete"),
                json!({
                    "admin": {"username": "owner", "password": "password123"},
                    "library": {"serve_readers": true, "link_progress": true},
                }),
            ))
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
        let c = env.deps.core.config.read().clone();
        assert!(c.server.serve_readers);
        assert_eq!(c.storage.library.progress_user.as_deref(), Some("owner"));
        let r = env
            .send(json_body(
                req("POST", "/setup/api/complete"),
                json!({"admin": {"username": "x", "password": "password123"}, "library": {"serve_readers": "no"}}),
            ))
            .await;
        assert_ne!(r.status, 201);
    }

    /// The lite build's "Connect a processor": an account for the first processor,
    /// its password shown once.
    #[tokio::test]
    async fn the_lite_finish_makes_a_processor_account() {
        let env = env();
        let r = env
            .send(json_body(
                req("POST", "/setup/api/complete"),
                json!({
                    "admin": {"username": "admin", "password": "password123"},
                    "processor": {"create": true},
                }),
            ))
            .await;
        assert_eq!(r.status, 201, "{}", r.text());
        let p = &r.json()["processor"];
        assert_eq!(p["username"], "processor");
        let pw = p["password"].as_str().unwrap();
        assert!(pw.len() >= 20);
        let u = env.db.get_user("processor").unwrap().unwrap();
        assert_eq!(u.role, bunko_core::Role::Processor);
        assert!(env.db.authenticate_user("processor", pw).unwrap().is_some());
    }

    /// The storage steps are setup routes: another computer needs the setup code, and
    /// a POST must be JSON.
    #[tokio::test]
    async fn the_storage_steps_are_gated_like_the_rest_of_setup() {
        let env = env();
        for uri in ["/setup/api/storage", "/setup/api/storage/connect"] {
            let r = env.send(empty(from_peer("GET", uri, "192.168.1.20"))).await;
            assert_eq!(r.status, 403, "{uri}");
        }
        let r = env
            .send(json_body(
                from_peer("POST", "/setup/api/storage/connect", "192.168.1.20"),
                webdav_form("pw"),
            ))
            .await;
        assert_eq!(r.status, 403);
        let r = env
            .send(empty(req("POST", "/setup/api/storage/local")))
            .await;
        assert_eq!(r.status, 415, "{}", r.text());
        let r = env
            .send(json_body(
                req("POST", "/setup/api/storage/local"),
                json!({}),
            ))
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.json()["backend"], "local");
        let r = env
            .send(json_body(
                req("POST", "/setup/api/storage/connect"),
                json!({"provider": "dropbox"}),
            ))
            .await;
        assert_eq!(r.status, 400);
    }
}

// ---------------------------------------------------------------- the admin panel

mod admin_panel {
    use super::admin_support::*;
    use super::*;

    fn harness() -> Harness {
        Harness::with(Options {
            storage: Some(Box::new(|core| control_over(core, fake_library()))),
            ..Options::default()
        })
    }

    #[tokio::test]
    async fn the_storage_block_and_its_actions() {
        let h = harness();
        let t = h.admin();
        // Admins only.
        let editor = h.login("editor", bunko_core::Role::Editor);
        assert_eq!(
            h.call("GET", "/_admin/api/storage", Some(&editor), None)
                .await
                .status,
            403
        );
        let r = h.call("GET", "/_admin/api/storage", Some(&t), None).await;
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        for k in [
            "backend",
            "connected",
            "health",
            "problems",
            "connect",
            "configured",
            "serve_readers",
        ] {
            assert!(v.get(k).is_some(), "{k} missing from {v}");
        }
        assert_eq!(v["backend"], "local");
        // /api/status carries the same block.
        let st = h
            .call("GET", "/_admin/api/status", Some(&t), None)
            .await
            .json();
        assert_eq!(st["library"]["storage"]["backend"], "local");
        assert_eq!(st["library"]["storage"]["connect"]["state"], "idle");

        // Connect: the server restarts onto the library once connected.
        let r = h
            .call(
                "POST",
                "/_admin/api/storage/connect",
                Some(&t),
                Some(webdav_form("pw-secret")),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        let until = Instant::now() + Duration::from_secs(10);
        let v = loop {
            let v = h
                .call("GET", "/_admin/api/storage/connect", Some(&t), None)
                .await
                .json();
            if v["state"] == "connected" || Instant::now() > until {
                break v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(v["state"], "connected", "{v}");
        eventually("the restart", Duration::from_secs(5), || {
            h.restarts.load(Ordering::SeqCst) == 1
        })
        .await;
        let saved = h.saved_config();
        assert_eq!(saved.storage.library.backend, LibraryBackend::WebDav);
        let all = h
            .call("GET", "/_admin/api/storage", Some(&t), None)
            .await
            .text();
        assert!(!all.contains("pw-secret"), "{all}");

        // Readers: saved, read per request.
        let r = h
            .call(
                "PUT",
                "/_admin/api/storage/readers",
                Some(&t),
                Some(json!({"serve_readers": false})),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert!(!h.saved_config().server.serve_readers);
        let r = h
            .call(
                "PUT",
                "/_admin/api/storage/readers",
                Some(&t),
                Some(json!({"serve_readers": true, "progress_user": "nobody"})),
            )
            .await;
        assert_eq!(r.status, 400);
        let r = h
            .call(
                "PUT",
                "/_admin/api/storage/readers",
                Some(&t),
                Some(json!({"serve_readers": true, "progress_user": "boss"})),
            )
            .await;
        assert_eq!(r.status, 200);
        assert_eq!(
            h.saved_config().storage.library.progress_user.as_deref(),
            Some("boss")
        );

        // Take over needs a cloud store with a lease.
        let r = h
            .call(
                "POST",
                "/_admin/api/storage/lease/take-over",
                Some(&t),
                None,
            )
            .await;
        assert_eq!(r.status, 409);
        // Disconnect the sign-in on file (the store never ran: nothing owed).
        let r = h
            .call(
                "POST",
                "/_admin/api/storage/disconnect",
                Some(&t),
                Some(json!({"mode": "wait", "wait_s": 1})),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.json()["report"]["credentials_removed"], true);
        let r = h
            .call(
                "POST",
                "/_admin/api/storage/disconnect",
                Some(&t),
                Some(json!({"mode": "export", "folder": "/tmp/anywhere"})),
            )
            .await;
        assert_eq!(r.status, 400, "a request never chooses the export folder");
        assert!(
            r.text().contains("cannot choose the folder"),
            "{}",
            r.text()
        );
        let r = h
            .call(
                "POST",
                "/_admin/api/storage/disconnect",
                Some(&t),
                Some(json!({"mode": "export"})),
            )
            .await;
        assert_eq!(r.status, 200, "Bunko's own folder: {}", r.text());
        // Back to this disk.
        let r = h
            .call("POST", "/_admin/api/storage/local", Some(&t), None)
            .await;
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(
            h.saved_config().storage.library.backend,
            LibraryBackend::Local
        );
    }
}

// ---------------------------------------------------------------- Disconnect

/// A cloud library opened on `fake` at `base`, with a StorageControl over it.
struct Cloud {
    base: PathBuf,
    cloud: PathBuf,
    fake: Arc<FakeBackend>,
    control: Arc<StorageControl>,
    handle: Arc<bunko_server::storage::StoreHandle>,
    guard: bunko_store::MountGuard,
    // Last: the store goes before its directory.
    dir: tempfile::TempDir,
}

fn quick(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    c.retry_base = Duration::from_millis(20);
}

fn open_cloud() -> Cloud {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let fake = fake_library();
    bunko_server::storage::testing::register_tuned(&base, fake.clone(), Some(quick));
    let mut config = Config::default();
    config.storage.base_path = base.clone();
    config.storage.library.backend = LibraryBackend::WebDav;
    let layout = config.storage.layout();
    let (_store, guard, handle) = bunko_server::storage::open_library_store(&config, &layout)
        .unwrap()
        .unwrap();
    let cloud = layout.cloud_dir();
    let id = handle.store_id.clone().unwrap();
    bunko_remote::auth::CredentialStore::open_with_key(&cloud, None)
        .unwrap()
        .put(
            &id,
            bunko_remote::auth::StoredCredential {
                provider: "webdav".into(),
                webdav_password: Some("pw".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(handle.wait_steady(Duration::from_secs(20)));
    // STEADY comes before first adoption (INGEST_FACTS) ends; health is `syncing`
    // while it runs. It is a task of its own that starts once STEADY is seen, so health
    // can read `online` for a moment before it begins: wait for it to be done too.
    let t0 = std::time::Instant::now();
    while !handle.adopted() || handle.health() != bunko_store::Health::Online {
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "{:?}",
            handle.health()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let c2 = config.clone();
    let control = StorageControl::new(
        cloud.clone(),
        Some(handle.clone()),
        Arc::new(move || c2.clone()),
        Arc::new(|_f: &mut dyn FnMut(&mut Config)| Ok(())),
        None,
    );
    Cloud {
        base,
        cloud,
        fake,
        control,
        handle,
        guard,
        dir,
    }
}

impl Cloud {
    /// A Deferred OCR result for `rel`, through the store.
    fn publish(&self, rel: &str, bytes: &[u8]) {
        let staged = self.base.join(format!("staged-{}", rel.replace('/', "_")));
        std::fs::write(&staged, bytes).unwrap();
        let w = WriteIntent::new(
            Mode::CreateNew,
            Origin::Ocr { claim: "c1".into() },
            Durability::Deferred,
        );
        bunko_store::fs::publish(&staged, self.handle.root.join(rel), &w).unwrap();
    }

    fn store_dir(&self) -> PathBuf {
        self.cloud.join(self.handle.store_id.as_ref().unwrap())
    }

    /// Close the store (as a restart does), keeping the directory.
    fn close(self) -> tempfile::TempDir {
        let Cloud {
            control,
            handle,
            guard,
            dir,
            ..
        } = self;
        drop(control);
        drop(handle);
        drop(guard);
        dir
    }

    fn creds_ids(&self) -> Vec<String> {
        bunko_remote::auth::CredentialStore::open_with_key(&self.cloud, None)
            .unwrap()
            .store_ids()
            .unwrap()
    }
}

/// The panel's view of a running cloud store: health, sync, caches, outbox, conflicts,
/// lease, quota, problems.
#[test]
fn the_storage_block_of_a_cloud_store() {
    let c = open_cloud();
    let v = c.control.status_json();
    for k in [
        "provider",
        "account",
        "folder",
        "store_id",
        "health",
        "engine",
        "sync",
        "synced_at",
        "cache",
        "outbox",
        "conflicts",
        "submissions",
        "lease",
        "quota",
        "series_queue",
        "problems",
        "connect",
        "configured",
        "restart_pending",
    ] {
        assert!(v.get(k).is_some(), "{k} missing from {v}");
    }
    // The series queue files' sink: no queue to write from here (no OCR scheduler).
    assert_eq!(v["series_queue"]["state"], "no_source", "{v}");
    assert_eq!(v["series_queue"]["files"], 0);
    assert_eq!(v["series_queue"]["backlog"], 0);
    assert_eq!(v["series_queue"]["last_flush_at"], Value::Null);
    assert_eq!(v["health"]["state"], "online");
    assert_eq!(v["engine"], "steady");
    assert!(v["synced_at"].as_f64().unwrap() > 0.0);
    assert_eq!(v["outbox"]["owed"], 0);
    assert_eq!(v["lease"]["state"], "not-started");
    assert_eq!(v["problems"], json!([]));
    assert_eq!(v["provider"], "the WebDAV server");
}

/// Wait: results still owed are counted and nothing happens; Export copies them into
/// a new folder Bunko owns (`<base>/exports/<store-id>-<time>/`, flat safe names and
/// an `index.json`) and goes on; the credentials go, the directory is removed at the
/// next start, and nothing is deleted in the cloud.
#[test]
fn disconnect_waits_then_exports() {
    let c = open_cloud();
    c.fake.throttle_for(Duration::from_secs(120));
    c.publish("Series/Vol 1.mokuro", b"{\"ocr\": 1}");
    c.publish("Series/Vol 2.mokuro", b"{\"ocr\": 2}");
    let owed = c.handle.owed();
    assert_eq!(owed.pending, 2, "{:?}", c.control.status_json()["outbox"]);
    let st = c.control.status_json();
    assert_eq!(st["outbox"]["owed"], 2);
    assert!(
        st["outbox"]["oldest"]["path"]
            .as_str()
            .unwrap()
            .contains("Series/")
    );
    match c
        .control
        .disconnect(&DisconnectMode::Drain(Duration::from_millis(300)))
    {
        Err(DisconnectError::Owed(o)) => {
            assert_eq!(o.total(), 2);
            assert_eq!(o.describe(), "2 results not yet uploaded");
            let j = DisconnectError::Owed(o).as_json();
            assert_eq!(j["choices"], json!(["wait", "export", "discard"]));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        c.creds_ids().len(),
        1,
        "a refused Disconnect changes nothing"
    );
    let report = c.control.disconnect(&DisconnectMode::Export).unwrap();
    assert_eq!(report.exported.len(), 2, "{report:?}");
    let out = report
        .export_dir
        .clone()
        .expect("the folder Bunko wrote to");
    let id = c.handle.store_id.clone().unwrap();
    assert_eq!(out.parent(), Some(c.base.join("exports").as_path()));
    let leaf = out.file_name().unwrap().to_string_lossy().into_owned();
    assert!(leaf.starts_with(&format!("{id}-")), "{leaf}");
    let mut names: Vec<String> = std::fs::read_dir(&out)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), 3, "{names:?}");
    assert_eq!(names[2], "index.json");
    for (name, body) in names[..2]
        .iter()
        .zip([&b"{\"ocr\": 1}"[..], b"{\"ocr\": 2}"])
    {
        assert!(!name.contains('/') && !name.contains(".."), "{name}");
        assert!(name.ends_with(".mokuro"), "{name}");
        assert_eq!(std::fs::read(out.join(name)).unwrap(), body, "{name}");
    }
    let index: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("index.json")).unwrap()).unwrap();
    let dests: Vec<&str> = index
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["dest"].as_str().unwrap())
        .collect();
    assert!(dests.contains(&"Series/Vol 1.mokuro"), "{index}");
    for e in index.as_array().unwrap() {
        let op = e["op"].as_u64().expect("each file names its op");
        let file = e["file"].as_str().unwrap();
        assert!(file.starts_with(&format!("{op}-Series__Vol ")), "{file}");
    }
    assert!(
        !std::fs::read_dir(c.base.join("exports"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(".staging")),
        "the staging folder is gone"
    );
    assert!(report.credentials_removed);
    assert!(c.creds_ids().is_empty());
    assert!(c.control.restart_pending());
    assert!(c.store_dir().join(".disconnected").is_file());
    // Nothing in the cloud was deleted or trashed.
    assert!(
        c.fake
            .node_at(&format!("{ROOT}/Series/Vol 1.cbz"))
            .is_some()
    );
    assert!(c.fake.trashed_log().is_empty() && c.fake.deleted_log().is_empty());
    // The next start removes the store directory, keeping .upgrade-originals.
    let dir = c.store_dir();
    let cloud = c.cloud.clone();
    let _keep = c.close();
    std::fs::create_dir_all(dir.join(".upgrade-originals")).unwrap();
    std::fs::write(dir.join(".upgrade-originals/keep.mokuro"), b"x").unwrap();
    bunko_server::storage::disconnect::sweep_disconnected(&cloud);
    assert!(dir.join(".upgrade-originals/keep.mokuro").is_file());
    // The sweep runs at the next start, in a new process (`bunko_update::restart`). Here
    // it runs in the same one, after dropping the store: on Windows a file still open in
    // a winding-down thread cannot be removed (unlinked elsewhere), so the directory is
    // only checked off Windows; a later sweep removes it.
    #[cfg(not(windows))]
    {
        assert!(!dir.join("state.sqlite").exists());
        assert!(!dir.join(".disconnected").exists());
    }
}

/// The CLI's own folder (`library disconnect --export DIR`, in-process): a non-empty
/// folder or a symlink is refused before anything is disconnected; a new one gets the
/// same flat files, created new.
#[test]
fn export_to_a_chosen_folder_refuses_non_empty_and_symlinks() {
    let c = open_cloud();
    c.fake.throttle_for(Duration::from_secs(120));
    c.publish("Series/Vol 1.mokuro", b"{\"ocr\": 1}");
    let full = c.base.join("full");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::write(full.join("mine.txt"), b"keep").unwrap();
    let err = c
        .control
        .disconnect(&DisconnectMode::ExportTo(full.clone()))
        .unwrap_err();
    assert!(err.to_string().contains("not empty"), "{err}");
    assert_eq!(std::fs::read(full.join("mine.txt")).unwrap(), b"keep");
    let target = c.base.join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    #[cfg(unix)]
    {
        let link = c.base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = c
            .control
            .disconnect(&DisconnectMode::ExportTo(link))
            .unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
    }
    assert_eq!(c.creds_ids().len(), 1, "a refused Export changes nothing");

    let out = c.base.join("new/export");
    let report = c
        .control
        .disconnect(&DisconnectMode::ExportTo(out.clone()))
        .unwrap();
    assert_eq!(report.export_dir.as_deref(), Some(out.as_path()));
    assert_eq!(report.exported.len(), 1, "{report:?}");
    let file = &report.exported[0];
    assert_eq!(file.parent(), Some(out.as_path()));
    assert!(
        file.file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-Series__Vol 1.mokuro"),
        "{file:?}"
    );
    assert_eq!(std::fs::read(file).unwrap(), b"{\"ocr\": 1}");
    assert!(std::fs::read_dir(&target).unwrap().next().is_none());
}

/// Discard drops what is owed; Wait succeeds once the outbox drained.
#[test]
fn disconnect_discards_or_drains() {
    let c = open_cloud();
    c.fake.throttle_for(Duration::from_secs(120));
    c.publish("Series/Vol 1.mokuro", b"x");
    let report = c.control.disconnect(&DisconnectMode::Discard).unwrap();
    assert_eq!(report.discarded, 1);
    assert!(report.credentials_removed);
    drop(c);

    let c = open_cloud();
    c.publish("Series/Vol 2.mokuro", b"y");
    let report = c
        .control
        .disconnect(&DisconnectMode::Drain(Duration::from_secs(20)))
        .unwrap();
    assert!(report.exported.is_empty() && report.discarded == 0);
    let n = c
        .fake
        .node_at(&format!("{ROOT}/Series/Vol 2.mokuro"))
        .expect("uploaded before the disconnect");
    assert_eq!(c.fake.content(&n.pid).unwrap(), b"y");
    assert!(c.creds_ids().is_empty());
}

#[test]
fn disconnect_modes_parse_from_the_pages() {
    assert_eq!(
        DisconnectMode::from_json(&json!({})).unwrap(),
        DisconnectMode::Drain(bunko_server::storage::disconnect::DEFAULT_DRAIN)
    );
    assert_eq!(
        DisconnectMode::from_json(&json!({"mode": "wait", "wait_s": 90})).unwrap(),
        DisconnectMode::Drain(Duration::from_secs(90))
    );
    assert_eq!(
        DisconnectMode::from_json(&json!({"mode": "discard"})).unwrap(),
        DisconnectMode::Discard
    );
    // An HTTP request never chooses the export folder: Bunko's own, or refused.
    assert_eq!(
        DisconnectMode::from_json(&json!({"mode": "export"})).unwrap(),
        DisconnectMode::Export
    );
    for folder in ["rel/dir", "/etc", "/tmp/x", "~/.ssh", "../../x"] {
        assert!(
            DisconnectMode::from_json(&json!({"mode": "export", "folder": folder})).is_err(),
            "{folder}"
        );
    }
    assert!(DisconnectMode::from_json(&json!({"mode": "nuke"})).is_err());
}

// ---------------------------------------------------------------- a local folder

/// A library in a folder (`backend: folder`), through the live driver: the Check over a
/// real directory, "Create it?", the refusals of Bunko's own folder, Disconnect (the
/// folder is left as it was but for the marker) and a switch to another folder (an
/// account change, as on the clouds).
mod folder_library {
    use super::*;
    use bunko_core::config::LibraryConfig;
    use bunko_server::storage::connect::{ConnectRequest, ConnectStatus, RequestOrigin};
    use parking_lot::Mutex;
    use std::collections::BTreeMap;

    /// Two series, three volumes, one OCR file.
    fn seed(folder: &Path) {
        std::fs::create_dir_all(folder.join("Series A")).unwrap();
        std::fs::create_dir_all(folder.join("Series B")).unwrap();
        std::fs::write(folder.join("Series A/Vol 1.cbz"), b"zip-1").unwrap();
        std::fs::write(folder.join("Series A/Vol 1.mokuro"), b"{}").unwrap();
        std::fs::write(folder.join("Series A/Vol 2.cbz"), b"zip-2").unwrap();
        std::fs::write(folder.join("Series B/Vol 1.cbz"), b"zip-3").unwrap();
    }

    /// Every file and folder under `dir` (folders end in `/`) with its bytes.
    fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut todo = vec![(dir.to_path_buf(), String::new())];
        while let Some((d, rel)) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let name = format!("{rel}{}", e.file_name().to_string_lossy());
                if e.file_type().unwrap().is_dir() {
                    out.insert(format!("{name}/"), Vec::new());
                    todo.push((e.path(), format!("{name}/")));
                } else {
                    out.insert(name, std::fs::read(e.path()).unwrap());
                }
            }
        }
        out
    }

    fn folder_req(path: &Path) -> ConnectRequest {
        ConnectRequest {
            provider: "folder".into(),
            path: Some(path.to_string_lossy().into_owned()),
            ..ConnectRequest::default()
        }
    }

    /// A Connector on the live drivers over the storage base `base`.
    fn live_connector(base: &Path) -> (Connector, Arc<Mutex<LibraryConfig>>) {
        let lib = Arc::new(Mutex::new(LibraryConfig::default()));
        let (r, w) = (lib.clone(), lib.clone());
        let c = Connector::new(
            base.join("cloud"),
            Arc::new(move || r.lock().clone()),
            Arc::new(move |choice| {
                choice.apply_to(&mut w.lock());
                Ok(())
            }),
        );
        (c, lib)
    }

    async fn settle(c: &Connector) -> ConnectStatus {
        c.wait_for(|s| !s.in_progress(), Duration::from_secs(30))
            .await
    }

    fn shown(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    /// Refused before any folder is opened: Bunko's own folder, anything in it but its
    /// `library/`, a folder that holds it, a relative path, a file, a path whose parent
    /// is missing too. Nothing is written.
    #[tokio::test]
    async fn folders_bunko_cannot_use_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        std::fs::create_dir_all(base.join("cloud")).unwrap();
        std::fs::write(dir.path().join("a-file"), b"x").unwrap();
        let (c, lib) = live_connector(&base);
        for (path, says) in [
            (base.clone(), "part of Bunko's own folder"),
            (base.join("cloud"), "part of Bunko's own folder"),
            (base.join("users/me"), "part of Bunko's own folder"),
            (dir.path().to_path_buf(), "holds Bunko's own folder"),
            (PathBuf::from("manga"), "starts with / or a drive letter"),
            (dir.path().join("a-file"), "is a file, not a folder"),
            (dir.path().join("gone/manga"), "is missing too"),
        ] {
            c.start(folder_req(&path), RequestOrigin::cli()).unwrap();
            let s = settle(&c).await;
            assert_eq!(s.state, "failed", "{path:?}: {s:?}");
            let e = s.error.unwrap();
            assert!(e.contains(says), "{path:?}: {e}");
        }
        assert!(!dir.path().join("gone").exists());
        let empty = ConnectRequest {
            provider: "folder".into(),
            ..ConnectRequest::default()
        };
        assert_eq!(
            c.start(empty, RequestOrigin::cli()).unwrap_err(),
            "Enter the folder's full path."
        );
        assert_eq!(lib.lock().backend, LibraryBackend::Local);
        assert!(!base.join("cloud/credentials.json").exists());
    }

    /// Refused for everyone, the CLI included, before the folder is opened: the top of
    /// the system disk, system folders, and Bunko's own settings folder (and a folder
    /// holding it).
    #[tokio::test]
    async fn system_folders_and_bunkos_settings_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        std::fs::create_dir_all(base.join("cloud")).unwrap();
        let (c, lib) = live_connector(&base);
        let mut refused: Vec<(PathBuf, &str)> = Vec::new();
        #[cfg(unix)]
        for p in ["/", "/etc", "/usr/lib", "/proc/self", "/boot/../etc/x"] {
            refused.push((PathBuf::from(p), "is a system folder"));
        }
        #[cfg(windows)]
        {
            let windir = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
            let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
            refused.push((PathBuf::from(format!(r"{drive}\")), "is a system folder"));
            refused.push((PathBuf::from(&windir), "is a system folder"));
            refused.push((
                PathBuf::from(&windir).join("System32"),
                "is a system folder",
            ));
            refused.push((
                PathBuf::from(format!(r"{drive}\Program Files")),
                "is a system folder",
            ));
            refused.push((
                PathBuf::from(format!(r"{drive}\ProgramData\x")),
                "is a system folder",
            ));
        }
        let settings = bunko_core::storage::default_config_path()
            .parent()
            .unwrap()
            .to_path_buf();
        refused.push((settings.clone(), "Bunko's settings"));
        refused.push((settings.join("x"), "Bunko's settings"));
        // Its parent may hold Bunko's own folder too (on Windows both are in
        // %LOCALAPPDATA%): either refusal.
        refused.push((settings.parent().unwrap().to_path_buf(), "holds Bunko's"));
        for (path, says) in refused {
            c.start(folder_req(&path), RequestOrigin::cli()).unwrap();
            let s = settle(&c).await;
            assert_eq!(s.state, "failed", "{path:?}: {s:?}");
            assert!(s.check.is_none(), "{path:?}: never opened: {s:?}");
            let e = s.error.unwrap();
            assert!(e.contains(says), "{path:?}: {e}");
        }
        assert_eq!(lib.lock().backend, LibraryBackend::Local);
        assert!(!base.join("cloud/credentials.json").exists());
    }

    /// A caller elsewhere with `storage.library.folder_roots` set: a folder at or under
    /// a root connects, and "Create it?" works there; one outside is refused; and the
    /// Check's refusals read the same for a file, a missing parent and anything else, so
    /// it tells them nothing of the disk. The CLI gets the detailed words.
    #[tokio::test]
    async fn a_caller_elsewhere_connects_inside_a_root_and_learns_nothing_more() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        let root = dir.path().join("library");
        seed(&root.join("Manga"));
        std::fs::write(root.join("a-file"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("outside")).unwrap();
        let (c, lib) = live_connector(&base);
        lib.lock().folder_roots = vec![root.clone()];
        let far = RequestOrigin::of(false, Some("Mozilla/5.0"));

        c.start(folder_req(&root.join("Manga")), far).unwrap();
        let s = settle(&c).await;
        assert_eq!(s.state, "connected", "{s:?}");
        assert_eq!(s.check.unwrap().volumes, 3);

        let fresh = root.join("New");
        c.start(folder_req(&fresh), far).unwrap();
        let s = settle(&c).await;
        assert_eq!(s.state, "connected", "{s:?}");
        assert!(c.create_folder().await.unwrap().found);
        assert!(fresh.is_dir());

        let e = {
            c.start(folder_req(&dir.path().join("outside")), far)
                .unwrap();
            let s = settle(&c).await;
            assert_eq!(s.state, "failed", "{s:?}");
            s.error.unwrap()
        };
        assert!(e.contains("is not inside the folders"), "{e}");

        let mut said = Vec::new();
        for p in [root.join("a-file"), root.join("gone").join("manga")] {
            c.start(folder_req(&p), far).unwrap();
            let s = settle(&c).await;
            assert_eq!(s.state, "failed", "{p:?}: {s:?}");
            said.push(s.error.unwrap().replace(&shown(&p), "<path>"));
        }
        assert_eq!(said[0], said[1]);
        assert_eq!(
            said[0],
            "There is no folder Bunko can use at <path>: check the path."
        );
        // The CLI is told which it is.
        c.start(folder_req(&root.join("a-file")), RequestOrigin::cli())
            .unwrap();
        let s = settle(&c).await;
        assert!(s.error.unwrap().contains("is a file, not a folder"));
    }

    /// The Check counts the folder's volumes and series; a missing folder is offered
    /// ("Create it?") and made, one level only; `<base>/library` may be chosen.
    #[tokio::test]
    async fn the_check_counts_the_folder_and_creates_one_level() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        let manga = dir.path().join("Manga");
        seed(&manga);
        let (c, lib) = live_connector(&base);
        c.start(folder_req(&manga), RequestOrigin::cli()).unwrap();
        let s = settle(&c).await;
        assert_eq!(s.state, "connected", "{s:?}");
        assert_eq!(s.account.as_deref(), Some(shown(&manga).as_str()));
        let check = s.check.unwrap();
        assert!(check.found);
        assert_eq!((check.volumes, check.series), (3, 2));
        assert_eq!(
            check.summary,
            format!("Found {}: 3 volumes in 2 series.", shown(&manga))
        );
        assert_eq!(check.warning, None, "a writable folder needs no warning");
        {
            let l = lib.lock();
            assert_eq!(l.backend, LibraryBackend::Folder);
            assert_eq!(l.path.as_deref(), Some(manga.as_path()));
            assert_eq!(l.folder, ROOT);
        }
        let id = bunko_server::storage::store_id("folder", "", &shown(&manga), ROOT);
        let cred = bunko_remote::auth::CredentialStore::open(&base.join("cloud"))
            .unwrap()
            .get(&id)
            .unwrap()
            .expect("the stub");
        assert_eq!(cred.provider, "folder");
        assert_eq!(cred.account_id.as_deref(), Some(shown(&manga).as_str()));

        // A folder that is not there yet: offered, then made, one level only.
        let fresh = dir.path().join("New Manga");
        c.start(folder_req(&fresh), RequestOrigin::cli()).unwrap();
        let s = settle(&c).await;
        assert_eq!(s.state, "connected", "{s:?}");
        let check = s.check.unwrap();
        assert!(!check.found);
        assert_eq!(
            check.summary,
            format!("There is no folder at {} yet. Create it?", shown(&fresh))
        );
        assert!(!fresh.exists(), "the Check alone makes nothing");
        let made = c.create_folder().await.unwrap();
        assert!(made.created && made.found, "{made:?}");
        assert_eq!((made.volumes, made.series), (0, 0));
        assert!(fresh.is_dir());
        assert!(
            std::fs::read_dir(&fresh).unwrap().next().is_none(),
            "nothing but the folder itself: {:?}",
            tree(&fresh)
        );
        // The parent missing too: refused, nothing made.
        let deep = dir.path().join("a/b");
        c.start(folder_req(&deep), RequestOrigin::cli()).unwrap();
        let s = settle(&c).await;
        assert_eq!(s.state, "failed");
        assert!(s.error.unwrap().contains("is missing too"));
        assert!(!dir.path().join("a").exists());

        // <base>/library: allowed (a library on this computer kept in place).
        std::fs::create_dir_all(base.join("library/Series")).unwrap();
        std::fs::write(base.join("library/Series/Vol 1.cbz"), b"zip").unwrap();
        c.start(folder_req(&base.join("library")), RequestOrigin::cli())
            .unwrap();
        let s = settle(&c).await;
        assert_eq!(s.state, "connected", "{s:?}");
        assert_eq!(s.check.unwrap().volumes, 1);
    }

    /// The lease on, with a short settle wait.
    fn leased(c: &mut bunko_remote::RemoteStoreConfig) {
        c.lease = true;
        c.lease_settle = Some(Duration::from_millis(300));
        c.retry_base = Duration::from_millis(20);
    }

    /// A folder library that ran here: `use_local` is refused until its Disconnect;
    /// choosing another folder is an account change ("Use this folder instead"), which
    /// disconnects the first; the first folder is then as it was, but for the marker,
    /// which says `released`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn disconnect_and_switch_leave_the_folder_alone() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("storage");
        let first = dir.path().join("Manga");
        let second = dir.path().join("Other Manga");
        seed(&first);
        seed(&second);
        let before = tree(&first);

        let mut config = Config::default();
        config.storage.base_path = base.clone();
        config.storage.library.backend = LibraryBackend::Folder;
        config.storage.library.path = Some(first.clone());
        let layout = config.storage.layout();
        bunko_server::storage::test_cloud::register_tune(&base, leased);
        let (_store, _guard, handle) = bunko_server::storage::open_library_store(&config, &layout)
            .unwrap()
            .unwrap();
        assert!(handle.connected(), "{:?}", handle.problem());
        let h = handle.clone();
        let held = tokio::task::spawn_blocking(move || {
            let until = Instant::now() + Duration::from_secs(60);
            while h.status_json()["lease"]["state"] != "held" || !h.adopted() {
                if Instant::now() > until {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            true
        })
        .await
        .unwrap();
        assert!(held, "{}", handle.status_json());
        let st = handle.status_json();
        assert_eq!(st["path"], shown(&first).as_str());
        assert_eq!(st["provider"], "the library folder");

        let shared = Arc::new(Mutex::new(config.clone()));
        let (r, w) = (shared.clone(), shared.clone());
        let control = StorageControl::new(
            layout.cloud_dir(),
            Some(handle.clone()),
            Arc::new(move || r.lock().clone()),
            Arc::new(move |f: &mut dyn FnMut(&mut Config)| {
                f(&mut w.lock());
                Ok(())
            }),
            None,
        );
        let e = control.use_local().unwrap_err();
        assert_eq!(
            e,
            format!(
                "this Bunko manages the library in {}: disconnect it first (it is a different library)",
                shown(&first)
            )
        );
        assert_eq!(
            control.status_json()["configured"]["path"],
            shown(&first).as_str()
        );

        // Another folder: the account decision.
        let s = control
            .connect(folder_req(&second), RequestOrigin::cli(), false)
            .unwrap();
        let s = control
            .connector()
            .wait_for(
                |x| x.session != s.session || !x.in_progress(),
                Duration::from_secs(30),
            )
            .await;
        assert_eq!(s.state, "account-mismatch", "{s:?}");
        assert_eq!(
            s.message.as_deref(),
            Some(
                format!(
                    "This Bunko's library is in {}, not {}. Choose that folder, or choose \"Use this folder instead\".",
                    shown(&first),
                    shown(&second)
                )
                .as_str()
            )
        );
        // "Use this folder instead": the first library is disconnected (drained).
        let s = control.decide_account(true, false).await.unwrap();
        assert_eq!(s.state, "connected", "{s:?}");
        let ids = bunko_remote::auth::CredentialStore::open(&layout.cloud_dir())
            .unwrap()
            .store_ids()
            .unwrap();
        let second_id = bunko_server::storage::store_id("folder", "", &shown(&second), ROOT);
        assert_eq!(ids, vec![second_id]);
        {
            let c = shared.lock();
            assert_eq!(c.storage.library.backend, LibraryBackend::Folder);
            assert_eq!(c.storage.library.path.as_deref(), Some(second.as_path()));
        }

        // The first folder: as it was, but for the marker, which says released.
        let mut after = tree(&first);
        let marker = after
            .remove(".mokuro-bunko.json")
            .expect("the marker stays, released");
        let marker: Value = serde_json::from_slice(&marker).unwrap();
        assert_eq!(marker["state"], "released", "{marker}");
        assert_eq!(after, before, "Disconnect leaves the folder alone");
        // Nothing in the second folder was touched by the Check.
        assert!(!second.join(".mokuro-bunko.json").exists());
        // And the library on this computer may be chosen again.
        control.use_local().unwrap();
    }
}
