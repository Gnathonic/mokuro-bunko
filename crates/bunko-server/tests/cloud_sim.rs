//! A whole server on each cloud provider, played by cloudsim (`bunko-cloudsim`) through
//! the test-only `MOKURO_TEST_CLOUD_*` settings: the stores are built exactly as a
//! connected server builds them (credentials file, refreshing tokens, the MEGA session
//! from a password login), only their addresses point at the simulator. Per provider:
//! the catalog lists a seeded two-volume series, the bridge serves an archive (whole
//! and ranged), and a sidecar PUT through WebDAV lands in the simulator's account.
//!
//! The variables are process-wide, so the tests take turns.

// Fixtures are checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use bunko_cloudsim::{CloudSim, Provider};
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_remote::auth::{CredentialStore, StoredCredential};
use bunko_server::app::{ServeOptions, Services, assemble, start_library_services};
use bunko_server::storage::{pairing_of, store_id, test_cloud};
use http::{Request, StatusCode};
use tower::ServiceExt;

const ROOT: &str = "mokuro-reader";
const ACCOUNT: &str = "bunko";

static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A `.cbz` of `pages` small PNG pages.
fn cbz(pages: usize, seed: u8) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 0..pages {
        let img = image::RgbImage::from_fn(24, 32, |x, y| {
            image::Rgb([(x * 9) as u8 ^ seed, (y * 7) as u8, (i * 40) as u8])
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

struct Series {
    vol1: Vec<u8>,
}

fn seed(sim: &CloudSim, p: Provider) -> Series {
    let vol1 = cbz(2, 1);
    sim.seed(
        p,
        ACCOUNT,
        &[
            (format!("{ROOT}/Series/Vol 1.cbz"), vol1.clone()),
            (format!("{ROOT}/Series/Vol 1.mokuro"), mokuro("Vol 1", 2)),
            (format!("{ROOT}/Series/Vol 2.cbz"), cbz(3, 2)),
        ],
    );
    Series { vol1 }
}

/// No lease (writes may leave at once) and quick retries.
fn quick(c: &mut bunko_remote::RemoteStoreConfig) {
    c.lease = false;
    c.queue_floor = Some(Duration::from_millis(200));
    c.retry_base = Duration::from_millis(50);
}

fn config(base: PathBuf, backend: LibraryBackend) -> Config {
    let mut c = Config::default();
    c.storage.base_path = base;
    c.storage.library.backend = backend;
    c.server.serve_readers = true;
    c.catalog.enabled = true;
    c.catalog.enrich_community = false;
    c.ocr.autobench = false;
    c.ocr.local_processing = false;
    c
}

/// The process environment for one test, put back on drop.
struct Vars(Vec<&'static str>);

impl Vars {
    fn set(sim: &CloudSim) -> Vars {
        let vals = [
            sim.drive_api_base(),
            sim.graph_api_base(),
            sim.mega_origin(),
            sim.token_url(Provider::Google),
            sim.token_url(Provider::Microsoft),
        ];
        for (k, v) in test_cloud::VARS.into_iter().zip(vals) {
            // SAFETY: the tests in this binary take turns (TURN) and nothing else in
            // it reads or writes the environment meanwhile.
            unsafe { std::env::set_var(k, v) };
        }
        Vars(test_cloud::VARS.to_vec())
    }
}

impl Drop for Vars {
    fn drop(&mut self) {
        for k in &self.0 {
            // SAFETY: as in `set`.
            unsafe { std::env::remove_var(k) };
        }
    }
}

/// Store the credential a connected server would have, under the id it looks for.
fn sign_in(base: &Path, config: &Config, cred: StoredCredential) {
    let lib = &config.storage.library;
    let account = cred.account_id.clone().unwrap();
    let id = store_id(
        lib.backend.as_str(),
        &pairing_of(lib),
        &account,
        &lib.folder,
    );
    let creds = CredentialStore::open(&base.join("cloud")).unwrap();
    creds.put(&id, cred).unwrap();
}

async fn oauth_credential(sim: &CloudSim, p: Provider) -> StoredCredential {
    let t = sim.token(p, ACCOUNT);
    StoredCredential {
        provider: if p == Provider::Google {
            "gdrive"
        } else {
            "onedrive"
        }
        .into(),
        refresh_token: Some(t.refresh_token),
        // Expired already: the server's first call refreshes it at the simulator.
        access_token: Some(t.access_token),
        expires_at: Some(time::OffsetDateTime::now_utc() - Duration::from_secs(60)),
        account_id: Some("sim-account".into()),
        account_label: Some("bunko@cloudsim.test".into()),
        client_id: Some("cloudsim-client".into()),
        ..Default::default()
    }
}

async fn mega_credential(sim: &CloudSim) -> StoredCredential {
    sim.mega_account(ACCOUNT, "bunko@cloudsim.test", "cloudsim");
    let mut mc = bunko_remote::mega::MegaConfig::new(PathBuf::new());
    mc.client.api_origin = sim.mega_origin();
    let blob = bunko_remote::mega::MegaSession::login_with(
        mc.client,
        "bunko@cloudsim.test",
        "cloudsim",
        None,
    )
    .await
    .unwrap();
    StoredCredential {
        provider: "mega".into(),
        account_id: Some("sim-account".into()),
        account_label: Some("bunko@cloudsim.test".into()),
        mega_session: Some(blob.expose().to_string()),
        ..Default::default()
    }
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

async fn send(
    app: &Router,
    method: &str,
    path: &str,
    user: Option<&str>,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
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
    let r = app
        .clone()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = r.status();
    let body = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, body)
}

async fn run(p: Provider) {
    let _turn = TURN.lock().await;
    let sim = bunko_cloudsim::start().await;
    let series = seed(&sim, p);
    let backend = match p {
        Provider::Google => LibraryBackend::GDrive,
        Provider::Microsoft => LibraryBackend::OneDrive,
        Provider::Mega => LibraryBackend::Mega,
    };
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    let config = config(base.clone(), backend);
    let cred = match p {
        Provider::Mega => mega_credential(&sim).await,
        _ => oauth_credential(&sim, p).await,
    };
    sign_in(&base, &config, cred);
    let _vars = Vars::set(&sim);
    test_cloud::register_tune(&base, quick);

    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    for (name, role) in [("reader", Role::Registered), ("boss", Role::Admin)] {
        services
            .db
            .create_user(name, "pass1234", role, UserStatus::Active, "")
            .unwrap();
    }
    let storage = services.storage.clone().expect("a cloud library");
    assert!(storage.connected(), "{p:?}: {:?}", storage.problem());
    let st = storage.clone();
    let steady = tokio::task::spawn_blocking(move || st.wait_steady(Duration::from_secs(30)))
        .await
        .unwrap();
    assert!(steady, "{p:?}: the first sync finishes");
    let stop = tokio_util::sync::CancellationToken::new();
    let running = start_library_services(&services, stop.clone());
    let app = assemble(&services, &opts);

    // The catalog lists the series.
    let (s, body) = send(&app, "GET", "/catalog/api/library", None, &[], vec![]).await;
    assert_eq!(s, StatusCode::OK, "{p:?}");
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let list = v["series"].as_array().unwrap();
    assert_eq!(list.len(), 1, "{p:?}: {v}");
    assert_eq!(list[0]["name"], "Series");
    assert_eq!(list[0]["volume_count"], 2, "{p:?}: {v}");

    // The bridge serves an archive, whole and ranged.
    let url = format!("/{ROOT}/Series/Vol%201.cbz");
    let (s, body) = send(&app, "GET", &url, Some("reader"), &[], vec![]).await;
    assert_eq!(s, StatusCode::OK, "{p:?}");
    assert!(body == series.vol1, "{p:?}: the archive's bytes");
    let (s, body) = send(
        &app,
        "GET",
        &url,
        Some("reader"),
        &[("range", "bytes=10-19")],
        vec![],
    )
    .await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT, "{p:?}");
    assert!(body == series.vol1[10..20], "{p:?}");

    // A sidecar PUT lands in the simulator's account.
    let sidecar = mokuro("Vol 2", 3);
    let (s, body) = send(
        &app,
        "PUT",
        &format!("/{ROOT}/Series/Vol%202.mokuro"),
        Some("boss"),
        &[("content-type", "application/json")],
        sidecar.clone(),
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::NO_CONTENT,
        "{p:?}: {s} {}",
        String::from_utf8_lossy(&body)
    );
    let want = format!("{ROOT}/Series/Vol 2.mokuro");
    let digest = {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(&sidecar))
    };
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let files = sim.files(p, ACCOUNT);
        if let Some(f) = files.iter().find(|f| f.path == want) {
            assert_eq!(f.sha256, digest, "{p:?}");
            break;
        }
        assert!(
            Instant::now() < until,
            "{p:?}: the sidecar never reached the simulator: {files:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Everything went to the simulator, the OAuth stores through its token endpoint.
    let log = sim.log();
    assert!(log.iter().all(|e| e.provider == p), "{p:?}");
    assert!(
        log.iter()
            .any(|e| e.class == "list" && e.account == ACCOUNT),
        "{p:?}"
    );
    if p != Provider::Mega {
        assert!(
            sim.refreshes() >= 1,
            "{p:?}: the expired token was refreshed"
        );
    }

    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), running).await;
    drop(app);
    drop(services);
    test_cloud::clear_tune(&base);
    sim.assert_clean();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_on_google_drive() {
    run(Provider::Google).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_on_onedrive() {
    run(Provider::Microsoft).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_on_mega() {
    run(Provider::Mega).await;
}
