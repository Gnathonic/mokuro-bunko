//! The library area of the analytics (docs/design/analytics.md §7) through the whole
//! server: file types, page formats from the zip directories (incremental across
//! passes), growth, uploads over WebDAV, dormant series, the rescan endpoint, and a
//! cloud library scanned from its store's index without one byte downloaded.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_remote::fake::FakeBackend;
use bunko_server::analytics::library::Outcome;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

const IP: &str = "192.0.2.41";
const JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0, 0x10, b'J', b'F', b'I', b'F', 0, 1,
];
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
const WEBP: &[u8] = b"RIFF\x10\0\0\0WEBPVP8 ";
const AVIF: &[u8] = b"\0\0\0\x1cftypavif\0\0\0\0";

/// A `.cbz` with `(name, bytes)` pages, deflated.
fn cbz(pages: &[(&str, &[u8])]) -> Vec<u8> {
    let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let o = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, data) in pages {
        z.start_file(*name, o).unwrap();
        z.write_all(data).unwrap();
    }
    z.finish().unwrap().into_inner()
}

fn jpeg_volume(n: usize) -> Vec<u8> {
    let names: Vec<String> = (0..n).map(|i| format!("{i:03}.jpg")).collect();
    let pages: Vec<(&str, &[u8])> = names.iter().map(|s| (s.as_str(), JPEG)).collect();
    cbz(&pages)
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

struct Env {
    _dir: tempfile::TempDir,
    library: PathBuf,
    services: Services,
    app: Router,
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

impl Env {
    fn new() -> Env {
        Self::with(|_| {})
    }

    /// A local library: `Alpha` (two jpeg volumes, a sidecar, a cover, series.json),
    /// `Beta` (one png+webp volume, one avif page with no extension, and a `.cbr` that
    /// is really RAR), `Gamma` (one zip volume, no sidecar) and a stray text file.
    fn with(edit: impl FnOnce(&mut Config)) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let library = storage.join("library");
        write(&library.join("Alpha/Alpha 01.cbz"), &jpeg_volume(3));
        write(&library.join("Alpha/Alpha 02.cbz"), &jpeg_volume(2));
        write(&library.join("Alpha/Alpha 01.mokuro"), b"{}");
        write(&library.join("Alpha/Alpha 01.webp"), WEBP);
        write(&library.join("Alpha/series.json"), b"{}");
        write(
            &library.join("Beta/Beta 01.cbz"),
            &cbz(&[
                ("a.png", PNG),
                ("b.webp", WEBP),
                ("c", AVIF),
                ("notes.txt", b"x"),
            ]),
        );
        write(
            &library.join("Beta/Beta 02.cbr"),
            b"Rar!\x1a\x07\x01\0 not a zip",
        );
        write(&library.join("Gamma/Gamma 01.zip"), &jpeg_volume(1));
        write(&library.join("Gamma/readme.txt"), b"hello");
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        edit(&mut config);
        let opts = ServeOptions::default();
        let services = Services::new(config, Some(dir.path().join("config.yaml")), &opts).unwrap();
        for (name, role) in [
            ("reader", Role::Registered),
            ("uploader", Role::Uploader),
            ("boss", Role::Admin),
        ] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            library,
            services,
            app,
        }
    }

    async fn call(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
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
        if !body.is_empty() {
            b = b.header("content-length", body.len().to_string());
        }
        let mut req = b.body(Body::from(body.to_vec())).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(IP.parse().unwrap(), 40000)));
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        (status, bytes)
    }

    async fn library(&self, range: &str) -> Value {
        let (status, body) = self
            .call(
                Some("boss"),
                "GET",
                &format!("/_admin/api/analytics/library?range={range}"),
                &[],
                b"",
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    async fn scan(&self) -> bunko_server::analytics::library::PassStats {
        let (outcome, stats) = self
            .services
            .analytics
            .library_scanner()
            .scan_now(&self.services.analytics)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Done);
        stats
    }

    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.library.parent().unwrap().join("analytics.sqlite")).unwrap()
    }
}

fn kind<'a>(v: &'a Value, kind: &str) -> &'a Value {
    v["by_kind"]
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} in {}", v["by_kind"]))
}

fn formats(v: &Value) -> Vec<(String, u64, u64)> {
    v["page_formats"]["formats"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["ext"].as_str().unwrap().to_string(),
                f["pages"].as_u64().unwrap(),
                f["volumes"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn a_scan_reports_file_types_page_formats_and_growth() {
    let env = Env::new();
    let before = env.library("30d").await;
    assert_eq!(before["scanned_at"], Value::Null);
    assert_eq!(before["dormant"], Value::Null);
    assert!(
        before.get("available").is_none(),
        "the view is never 'not collected'"
    );

    let stats = env.scan().await;
    assert_eq!((stats.series, stats.volumes), (3, 5));
    assert_eq!(stats.archives_read, 5);
    let v = env.library("30d").await;
    // The common fields every analytics answer carries (design §10).
    assert_eq!(v["range"]["key"], "30d");
    assert_eq!(v["range"]["granularity"], "day");
    assert_eq!(v["enabled"], true);
    assert!(v.get("tracking_since").is_some());
    assert!(v["scanned_at"].as_i64().unwrap() > 0);
    assert_eq!(v["cloud"], false);
    assert_eq!(v["scanning"], false);
    assert_eq!(v["totals"]["series"], 3);
    assert_eq!(v["totals"]["volumes"], 5);
    assert_eq!(kind(&v, "archive")["files"], 5);
    assert_eq!(kind(&v, "primary_sidecar")["files"], 1);
    assert_eq!(kind(&v, "cover")["files"], 1);
    assert_eq!(kind(&v, "series_json")["files"], 1);
    assert_eq!(kind(&v, "other")["files"], 1);
    assert_eq!(kind(&v, "other")["label"], "Other files");
    let exts: Vec<&str> = v["by_ext"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["ext"].as_str().unwrap())
        .collect();
    for e in ["cbz", "cbr", "zip", "mokuro", "webp", "json", "txt"] {
        assert!(exts.contains(&e), "{e} missing from {exts:?}");
    }
    assert_eq!(
        v["archive_mix"],
        json!([{"mix": "cbz", "series": 1}, {"mix": "cbz+cbr", "series": 1}, {"mix": "zip", "series": 1}])
    );
    assert_eq!(v["no_sidecar"]["count"], 2);
    // Page formats: jpeg 3+2+1, png, webp, the extension-less avif sniffed; the RAR
    // `.cbr` is unreadable.
    let pf = &v["page_formats"];
    assert_eq!(pf["available"], true);
    assert_eq!(
        (pf["scanned"].as_u64(), pf["unreadable"].as_u64()),
        (Some(4), Some(1))
    );
    assert_eq!(pf["pending"], 0);
    assert_eq!(
        formats(&v),
        [
            ("jpeg".to_string(), 6, 3),
            ("png".to_string(), 1, 1),
            ("webp".to_string(), 1, 1),
            ("avif".to_string(), 1, 1),
        ]
    );
    // Growth: one point a day, today's is this scan.
    let g = &v["growth"];
    // Only today has a snapshot: the axis starts there (the size before is unknown).
    assert_eq!(g["t"].as_array().unwrap().len(), 1);
    let last = |key: &str| {
        g["series"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == key)
            .unwrap()["values"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .as_u64()
            .unwrap()
    };
    assert_eq!((last("series"), last("volumes")), (3, 5));
    assert_eq!(last("bytes"), v["totals"]["bytes"].as_u64().unwrap());
    // An older snapshot is carried forward over the days without one.
    let today = bunko_server::analytics::now_s() / 86400;
    env.sql()
        .execute(
            "INSERT INTO library_daily (day, series, volumes, bytes) VALUES (?1, 2, 2, 10)",
            [today - 5],
        )
        .unwrap();
    let v = env.library("30d").await;
    let volumes = v["growth"]["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "volumes")
        .unwrap()["values"]
        .clone();
    assert_eq!(volumes, json!([2, 2, 2, 2, 2, 5]));
    assert_eq!(v["growth"]["t"][0], (today - 5) * 86400);
    // Hourly ranges still get daily growth points.
    let day = env.library("24h").await;
    assert_eq!(day["range"]["granularity"], "hour");
    assert!(day["growth"]["t"].as_array().unwrap().len() <= 2);
}

#[tokio::test]
async fn page_formats_are_incremental_across_passes() {
    let env = Env::new();
    assert_eq!(env.scan().await.archives_read, 5);
    // Nothing changed: nothing is read again (a restart reads nothing either: the
    // rows are in analytics.sqlite).
    let again = env.scan().await;
    assert_eq!((again.archives_read, again.pages_pending), (0, 0));
    // One archive rewritten, one removed, one added.
    write(&env.library.join("Alpha/Alpha 02.cbz"), &jpeg_volume(7));
    std::fs::remove_file(env.library.join("Gamma/Gamma 01.zip")).unwrap();
    write(
        &env.library.join("Gamma/Gamma 02.cbz"),
        &cbz(&[("x.gif", b"GIF89a")]),
    );
    let third = env.scan().await;
    assert_eq!((third.archives_read, third.archives_dropped), (2, 1));
    let v = env.library("7d").await;
    assert_eq!(
        formats(&v),
        [
            ("jpeg".to_string(), 10, 2),
            ("png".to_string(), 1, 1),
            ("webp".to_string(), 1, 1),
            ("avif".to_string(), 1, 1),
            ("gif".to_string(), 1, 1),
        ]
    );
    let keys: i64 = env
        .sql()
        .query_row("SELECT COUNT(*) FROM archive_pages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(keys, 5);
}

#[tokio::test]
async fn webdav_uploads_count_in_types_uploads_and_series() {
    let env = Env::new();
    let vol = jpeg_volume(4);
    for (method, path, body) in [
        ("MKCOL", "/mokuro-reader/Delta", &b""[..]),
        ("PUT", "/mokuro-reader/Delta/Delta%2001.cbz", &vol[..]),
        ("PUT", "/mokuro-reader/Delta/Delta%2001.webp", WEBP),
    ] {
        let (status, body) = env.call(Some("uploader"), method, path, &[], body).await;
        assert!(
            status.is_success(),
            "{method} {path}: {status} {}",
            String::from_utf8_lossy(&body)
        );
    }
    // A MOVE (by an admin: the uploader owns no Gamma) carries an archive into another
    // series folder.
    let (status, _) = env
        .call(
            Some("boss"),
            "MOVE",
            "/mokuro-reader/Gamma/Gamma%2001.zip",
            &[("destination", "/mokuro-reader/Delta/Delta%2002.zip")],
            b"",
        )
        .await;
    assert!(status.is_success(), "MOVE: {status}");
    env.services.analytics.flush_now().await.unwrap();
    env.scan().await;
    let v = env.library("30d").await;
    assert_eq!(v["totals"]["series"], 3, "Gamma has no archive left");
    assert_eq!(v["totals"]["volumes"], 6);
    assert_eq!(
        kind(&v, "cover")["files"],
        2,
        "a cover PUT over WebDAV is a cover"
    );
    assert_eq!(
        v["archive_mix"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["mix"] == "cbz+zip")
            .map(|m| m["series"].clone()),
        Some(json!(1))
    );
    let up = &v["uploads"];
    assert_eq!(
        up["by_uploader"],
        json!([{"username": "uploader", "volumes": 1, "bytes": vol.len()}])
    );
    let s = &up["series"][0];
    assert_eq!(
        (s["key"].as_str(), s["label"].as_str()),
        (Some("uploader-0"), Some("uploader"))
    );
    assert_eq!(s["values"].as_array().unwrap().last(), Some(&json!(1)));
    assert_eq!(up["t"].as_array().unwrap().len(), 30);
    let recent = &up["recent"][0];
    assert_eq!(recent["volume_key"], "Delta/Delta 01.cbz");
    assert_eq!(recent["series_title"], "Delta");
    assert_eq!(recent["username"], "uploader");
    assert!(recent["uploaded_at"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn uploads_before_analytics_come_from_the_main_database() {
    let env = Env::new();
    env.services
        .db
        .record_volume_upload("Alpha/Alpha 01.cbz", "uploader")
        .unwrap();
    // Nothing collected yet (no tracking_since): the main database answers.
    let v = env.library("7d").await;
    assert_eq!(v["tracking_since"], Value::Null);
    assert_eq!(
        v["uploads"]["by_uploader"],
        json!([{"username": "uploader", "volumes": 1, "bytes": 0}])
    );
}

#[tokio::test]
async fn dormant_series_were_neither_downloaded_nor_read() {
    let env = Env::new();
    env.scan().await;
    let v = env.library("30d").await;
    assert_eq!(v["dormant"]["days"], 90);
    assert_eq!(v["dormant"]["count"], 3);
    // A WebDAV download of Alpha, and a read of Beta (reading_daily is the reading
    // area's table; written here directly).
    let (status, _) = env
        .call(
            Some("reader"),
            "GET",
            "/mokuro-reader/Alpha/Alpha%2001.cbz",
            &[],
            b"",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    env.services.analytics.flush_now().await.unwrap();
    let today = bunko_server::analytics::now_s() / 86400;
    env.sql()
        .execute(
            "INSERT INTO reading_daily (day, username, volume_uuid, series_key, series_title) \
             VALUES (?1, 'reader', 'u-1', 'beta', 'Beta')",
            [today],
        )
        .unwrap();
    let v = env.library("30d").await;
    assert_eq!(
        v["dormant"]["series"],
        json!([{"series_key": "gamma", "title": "Gamma", "volumes": 1}])
    );
    assert_eq!(v["dormant"]["count"], 1);
    assert!(v["dormant"]["tracking_since"].as_i64().is_some());
}

#[tokio::test]
async fn case_variants_of_a_folder_are_one_series() {
    let env = Env::new();
    write(&env.library.join("alpha/Alpha 03.cbz"), &jpeg_volume(1));
    let stats = env.scan().await;
    // On a case-sensitive disk `alpha/` is a second folder; on any disk it is the
    // same series as `Alpha/`.
    assert_eq!((stats.series, stats.volumes), (3, 6));
    let n: i64 = env
        .sql()
        .query_row(
            "SELECT volumes FROM library_series WHERE series_key = 'alpha'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 3);
}

#[tokio::test]
async fn rescan_runs_a_pass_in_the_background() {
    let env = Env::new();
    let stop = CancellationToken::new();
    env.services.analytics.start(stop.clone());
    let (status, body) = env
        .call(
            Some("boss"),
            "POST",
            "/_admin/api/analytics/library/rescan",
            &[],
            b"",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let reply: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(reply["ok"], true);
    let mut v = Value::Null;
    for _ in 0..200 {
        v = env.library("30d").await;
        if v["scanned_at"].is_i64() && v["scanning"] == false {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(v["scanned_at"].is_i64(), "no scan: {v}");
    assert_eq!(v["scanning"], false);
    assert_eq!(v["page_formats"]["scanned"], 4);
    assert!(v["scan"]["duration_ms"].is_u64());
    // Only an admin may ask.
    let (status, _) = env
        .call(
            Some("reader"),
            "POST",
            "/_admin/api/analytics/library/rescan",
            &[],
            b"",
        )
        .await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
        "{status}"
    );
    stop.cancel();
}

#[tokio::test]
async fn nothing_is_scanned_with_analytics_off_or_page_formats_off() {
    let off = Env::with(|c| c.analytics.enabled = false);
    let (status, _) = off
        .call(
            Some("boss"),
            "POST",
            "/_admin/api/analytics/library/rescan",
            &[],
            b"",
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (outcome, _) = off
        .services
        .analytics
        .library_scanner()
        .scan_now(&off.services.analytics)
        .await
        .unwrap();
    assert_eq!(outcome, Outcome::Disabled);
    let rows: i64 = off
        .sql()
        .query_row("SELECT COUNT(*) FROM library_scan", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);

    let no_pages = Env::with(|c| c.analytics.library_scan.page_formats = false);
    let stats = no_pages.scan().await;
    assert_eq!(stats.archives_read, 0);
    let v = no_pages.library("30d").await;
    assert_eq!(
        v["page_formats"],
        json!({"available": false, "reason": "disabled"})
    );
    assert_eq!(v["totals"]["volumes"], 5);
}

/// A cloud library (the in-memory FakeBackend as Google Drive): the walk lists it from
/// the store's index; not one byte of any file is read from the provider, and page
/// formats say why they are not available.
#[tokio::test]
async fn a_cloud_library_is_scanned_from_the_index_without_a_download() {
    let fake = Arc::new(FakeBackend::drive_like());
    fake.reader_put("mokuro-reader/Series/Vol 1.cbz", &jpeg_volume(3));
    fake.reader_put("mokuro-reader/Series/Vol 1.mokuro", b"{}");
    fake.reader_put("mokuro-reader/Series/Vol 2.cbz", &jpeg_volume(2));
    fake.reader_put("mokuro-reader/Other/Book.cbr", b"Rar!");
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    bunko_server::storage::testing::register(&base, fake);
    let mut config = Config::default();
    config.storage.base_path = base;
    config.storage.library.backend = LibraryBackend::GDrive;
    config.catalog.enrich_community = false;
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    services
        .db
        .create_user("boss", "pass1234", Role::Admin, UserStatus::Active, "")
        .unwrap();
    let storage = services.storage.clone().expect("a cloud library");
    let steady = tokio::task::spawn_blocking(move || storage.wait_steady(Duration::from_secs(20)))
        .await
        .unwrap();
    assert!(steady);
    let store = services
        .storage
        .as_ref()
        .unwrap()
        .fake_store()
        .expect("the fake store")
        .clone();
    store.backend().reset_calls();
    let (outcome, stats) = services
        .analytics
        .library_scanner()
        .scan_now(&services.analytics)
        .await
        .unwrap();
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(
        (stats.series, stats.volumes, stats.archives_read),
        (2, 3, 0)
    );
    assert!(
        store.backend().read_log().is_empty(),
        "the scan downloaded: {:?}",
        store.backend().read_log()
    );
    let app = assemble(&services, &opts);
    let mut req = Request::builder()
        .uri("/_admin/api/analytics/library")
        .header("host", "localhost:8080")
        .header("authorization", basic("boss"))
        .body(Body::empty())
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(IP.parse().unwrap(), 40000)));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(v["cloud"], true);
    assert_eq!(
        v["page_formats"],
        json!({"available": false, "reason": "cloud"})
    );
    assert_eq!(v["totals"]["volumes"], 3);
    assert_eq!(kind(&v, "primary_sidecar")["files"], 1);
}

/// Turning analytics off stops a pass already reading archives at the next one:
/// "disabled: nothing is written" holds for a scan that was running too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_analytics_off_stops_a_running_pass() {
    let env = Env::new();
    for i in 0..40 {
        write(
            &env.library.join(format!("Delta/Delta {i:02}.cbz")),
            &jpeg_volume(1),
        );
    }
    let a = env.services.analytics.clone();
    let pass = tokio::spawn(async move { a.library_scanner().scan_now(&a).await.unwrap() });
    // Pages are read at 20 a second: let a few go, then turn analytics off.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    env.services.core.config.write().analytics.enabled = false;
    env.services.analytics.settings_changed();
    let (outcome, _) = pass.await.unwrap();
    assert_eq!(outcome, Outcome::Disabled);
    let count = || -> i64 {
        env.sql()
            .query_row("SELECT COUNT(*) FROM archive_pages", [], |r| r.get(0))
            .unwrap()
    };
    let read = count();
    assert!(read > 0 && read < 45, "{read} archives read");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(count(), read, "nothing more is opened");
}
