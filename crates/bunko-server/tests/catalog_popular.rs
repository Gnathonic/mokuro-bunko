//! The public catalog's "Popular" order through the whole server (docs/design/analytics.md
//! §10, "Catalog: Popular"): `/catalog/api/library` gives each series it lists a
//! `popular_rank` (1 = the most whole downloads over the last 30 UTC days, accounts and
//! anonymous visitors alike, processors never), from a ranking kept in memory. Only the
//! order is handed out, never a count; with analytics off there is no rank at all.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const READER_IP: &str = "192.0.2.10";
const UPLOADER_IP: &str = "192.0.2.11";
const OCR_IP: &str = "192.0.2.12";
const ANON: [&str; 4] = [
    "203.0.113.10",
    "203.0.113.11",
    "198.51.100.23",
    "2001:db8:1::5",
];

const SERIES: [(&str, &[&str]); 5] = [
    ("Alpha Saga", &["Vol 1", "Vol 2"]),
    ("Beta Days", &["Vol 1"]),
    ("Gamma Run", &["Vol 1"]),
    ("Delta Line", &["Vol 1"]),
    ("Epsilon", &["Vol 1"]),
];
const VOLUME_BYTES: usize = 40_000;

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    services: Services,
    app: Router,
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

fn seed_library(storage: &Path) {
    for (series, volumes) in SERIES {
        let dir = storage.join("library").join(series);
        std::fs::create_dir_all(&dir).unwrap();
        for name in volumes {
            std::fs::write(dir.join(format!("{name}.cbz")), vec![b'z'; VOLUME_BYTES]).unwrap();
        }
    }
}

impl Env {
    async fn new(edit: impl FnOnce(&mut Config)) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        seed_library(&storage);
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.catalog.enabled = true;
        edit(&mut config);
        let config_path = dir.path().join("config.yaml");
        let opts = ServeOptions::default();
        let services = Services::new(config, Some(config_path), &opts).unwrap();
        for (name, role) in [
            ("reader", Role::Registered),
            ("uploader", Role::Uploader),
            ("ocr", Role::Processor),
        ] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        // The catalog's rows, as the startup pass fills them.
        services.library.recompile_all_now().await;
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            storage,
            services,
            app,
        }
    }

    async fn call(
        &self,
        user: Option<&str>,
        path: &str,
        ip: &str,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut b = Request::builder()
            .method("GET")
            .uri(path)
            .header("host", "localhost:8080");
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        let mut req = b.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(ip.parse().unwrap(), 40000)));
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let (parts, body) = resp.into_parts();
        let bytes = body.collect().await.unwrap().to_bytes().to_vec();
        (status, parts.headers, bytes)
    }

    async fn download(&self, user: Option<&str>, path: &str, ip: &str) {
        let (status, _, body) = self.call(user, path, ip).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body.len(), VOLUME_BYTES);
    }

    /// `/catalog/api/library`, as an anonymous visitor would read it.
    async fn library(&self) -> (HeaderMap, String, Value) {
        let (status, headers, body) = self.call(None, "/catalog/api/library", "192.0.2.99").await;
        assert_eq!(status, StatusCode::OK);
        let text = String::from_utf8(body).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        (headers, text, v)
    }

    async fn ranks(&self) -> Vec<(String, Option<u64>)> {
        let (_, _, v) = self.library().await;
        v["series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["name"].as_str().unwrap().to_string(),
                    s.get("popular_rank").map(|r| r.as_u64().unwrap()),
                )
            })
            .collect()
    }

    /// A completed download row written straight into `analytics.sqlite` (the past).
    fn stored_download(&self, days_ago: i64, principal: &str, volume: &str, series_key: &str) {
        let today = bunko_server::analytics::now_s().div_euclid(86400);
        let c = rusqlite::Connection::open(self.storage.join("analytics.sqlite")).unwrap();
        c.execute(
            "INSERT INTO volume_downloads (day, principal, volume_key, series_key, series_title, \
             bytes, size, completed) VALUES (?1, ?2, ?3, ?4, ?4, 100, 100, 1)",
            rusqlite::params![today - days_ago, principal, volume, series_key],
        )
        .unwrap();
    }
}

fn vol(series: &str, volume: &str) -> String {
    format!(
        "/mokuro-reader/{}/{}.cbz",
        series.replace(' ', "%20"),
        volume.replace(' ', "%20")
    )
}

fn named(ranks: &[(String, Option<u64>)], name: &str) -> Option<u64> {
    ranks.iter().find(|(n, _)| n == name).and_then(|(_, r)| *r)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn popular_ranks_whole_downloads_of_the_last_30_days() {
    let env = Env::new(|_| {}).await;
    // Nothing downloaded yet: no series carries a rank.
    let (headers, before, _) = env.library().await;
    assert!(!before.contains("popular_rank"), "{before}");
    // No validator that could keep a stale ranking cached.
    assert!(headers.get("etag").is_none() && headers.get("last-modified").is_none());

    // Alpha Saga: an anonymous visitor's two volumes, the reader's through a
    // case-variant path (the folder is one series whatever the spelling), the
    // uploader's.
    env.download(None, &vol("Alpha Saga", "Vol 1"), ANON[3])
        .await;
    env.download(None, &vol("Alpha Saga", "Vol 2"), ANON[3])
        .await;
    env.download(Some("reader"), &vol("alpha saga", "VOL 1"), READER_IP)
        .await;
    env.download(Some("uploader"), &vol("Alpha Saga", "Vol 2"), UPLOADER_IP)
        .await;
    // Gamma Run: three anonymous visitors.
    for ip in &ANON[..3] {
        env.download(None, &vol("Gamma Run", "Vol 1"), ip).await;
    }
    // Beta Days: two accounts.
    env.download(Some("reader"), &vol("Beta Days", "Vol 1"), READER_IP)
        .await;
    env.download(Some("uploader"), &vol("Beta Days", "Vol 1"), UPLOADER_IP)
        .await;
    // Delta Line: a processor only, however often: no rank.
    for _ in 0..5 {
        env.download(Some("ocr"), &vol("Delta Line", "Vol 1"), OCR_IP)
            .await;
    }
    // A flush that wrote downloads refreshes the order.
    env.services.analytics.flush_now().await.unwrap();
    let ranks = env.ranks().await;
    assert_eq!(named(&ranks, "Alpha Saga"), Some(1), "{ranks:?}");
    assert_eq!(named(&ranks, "Gamma Run"), Some(2), "{ranks:?}");
    assert_eq!(named(&ranks, "Beta Days"), Some(3), "{ranks:?}");
    assert_eq!(named(&ranks, "Delta Line"), None, "{ranks:?}");
    assert_eq!(named(&ranks, "Epsilon"), None, "{ranks:?}");

    // Older rows: 29 days ago is inside the window, 30 is not. Beta's ten old
    // downloads would put it first; a series no longer in the library (Zeta) is
    // never listed and leaves no gap.
    for i in 0..10 {
        env.stored_download(30, &format!("u:old{i}"), "Beta Days/Vol 1.cbz", "beta days");
    }
    for i in 0..3 {
        env.stored_download(
            29,
            &format!("u:edge{i}"),
            "Gamma Run/Vol 1.cbz",
            "gamma run",
        );
    }
    for i in 0..50 {
        env.stored_download(2, &format!("u:z{i}"), "Zeta/Vol 1.cbz", "zeta");
    }
    // A row kept under another spelling of the folder still counts for it.
    env.stored_download(5, "u:renamed", "ALPHA  SAGA/Vol 1.cbz", "ALPHA  SAGA");
    let (_, unchanged, _) = env.library().await;
    assert_eq!(
        named(&env.ranks().await, "Gamma Run"),
        Some(2),
        "the order is the one in memory until it is refreshed: {unchanged}"
    );
    env.services.analytics.refresh_popular().await;
    let ranks = env.ranks().await;
    // Gamma 3 + 3 = 6, Alpha 4 + 1 = 5, Beta 2.
    assert_eq!(
        ranks
            .iter()
            .filter_map(|(n, r)| r.map(|r| (n.as_str(), r)))
            .collect::<Vec<_>>(),
        [("Alpha Saga", 2), ("Beta Days", 3), ("Gamma Run", 1)]
    );
    let (headers, after, v) = env.library().await;
    assert_ne!(after, unchanged, "a new ranking is served at once");
    assert!(headers.get("etag").is_none());

    // Only the order leaves the server: the rank is the one field added, and an
    // integer from 1 to the number of ranked series.
    let off = Env::new(|c| c.analytics.enabled = false).await;
    let (_, _, plain) = off.library().await;
    let keys = |s: &Value| -> Vec<String> {
        s.as_object()
            .unwrap()
            .keys()
            .filter(|k| *k != "popular_rank" && *k != "latest_volume_modified")
            .cloned()
            .collect()
    };
    for (with, without) in v["series"]
        .as_array()
        .unwrap()
        .iter()
        .zip(plain["series"].as_array().unwrap())
    {
        assert_eq!(keys(with), keys(without));
        if let Some(r) = with.get("popular_rank") {
            assert!((1..=3).contains(&r.as_u64().unwrap()), "{with}");
        }
    }
    for count in ["\"downloads\"", "\"count\""] {
        assert!(!after.contains(count), "{after}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_rank_while_analytics_is_off() {
    let env = Env::new(|_| {}).await;
    env.download(None, &vol("Gamma Run", "Vol 1"), ANON[0])
        .await;
    env.services.analytics.flush_now().await.unwrap();
    assert_eq!(named(&env.ranks().await, "Gamma Run"), Some(1));
    // Switched off: the ranking it had stays in memory, but is not handed out.
    env.services.core.config.write().analytics.enabled = false;
    env.services.analytics.settings_changed();
    let (_, text, _) = env.library().await;
    assert!(!text.contains("popular_rank"), "{text}");
    env.services.core.config.write().analytics.enabled = true;
    env.services.analytics.settings_changed();
    assert_eq!(named(&env.ranks().await, "Gamma Run"), Some(1));

    // Off from the start: never a rank, even over stored downloads.
    let off = Env::new(|c| c.analytics.enabled = false).await;
    off.stored_download(1, "u:someone", "Gamma Run/Vol 1.cbz", "gamma run");
    off.services.analytics.refresh_popular().await;
    let (_, text, _) = off.library().await;
    assert!(!text.contains("popular_rank"), "{text}");
}
