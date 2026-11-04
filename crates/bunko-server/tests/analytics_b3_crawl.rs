//! Crawl detection through the whole server (docs/design/analytics.md §8): real WebDAV
//! and catalog requests from documentation-range IPs, flushed with `flush_now`, read
//! back from `crawl_episodes` and `GET /_admin/api/analytics/crawlers`.
//!
//! The false positives matter most: Mokuro Reader's normal sync (one depth-infinity
//! listing of the whole library, covers for what it shows, a few downloads, its own
//! files) must never open an episode, while an rclone-style mirror, a metadata walk
//! over WebDAV and a scraper walking the catalog JSON must.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

/// Series in the demo library (the default thresholds: 20 series and a quarter).
const SERIES: usize = 40;
const VOLUMES_PER_SERIES: usize = 2;
const VOLUME_BYTES: usize = 4096;

const READER_IP: &str = "192.0.2.10";
const RCLONE_IP: &str = "203.0.113.9";
const ADMIN_IP: &str = "192.0.2.200";

const RCLONE_UA: &str = "rclone/v1.68.1";
const DAVFS_UA: &str = "davfs2/1.7.0 neon/0.33.0";

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

fn series_name(i: usize) -> String {
    format!("Series {i:02}")
}

fn seed_library(storage: &std::path::Path) {
    // A folder that is no series (the server makes `thumbnails/` itself).
    std::fs::create_dir_all(storage.join("library/Extras")).unwrap();
    std::fs::write(storage.join("library/Extras/readme.txt"), b"not a series").unwrap();
    for i in 0..SERIES {
        let dir = storage.join("library").join(series_name(i));
        std::fs::create_dir_all(&dir).unwrap();
        for v in 1..=VOLUMES_PER_SERIES {
            std::fs::write(dir.join(format!("Vol {v}.cbz")), vec![b'z'; VOLUME_BYTES]).unwrap();
            std::fs::write(dir.join(format!("Vol {v}.webp")), b"RIFF0000WEBP").unwrap();
        }
        std::fs::write(
            dir.join("series.json"),
            format!(r#"{{"title":"{}"}}"#, series_name(i)),
        )
        .unwrap();
    }
}

impl Env {
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        seed_library(&storage);
        Self::over(dir, storage).await
    }

    /// A server over `storage` (a second run reuses the first one's).
    async fn over(dir: tempfile::TempDir, storage: PathBuf) -> Env {
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        config.catalog.enabled = true;
        let config_path = dir.path().join("config.yaml");
        let opts = ServeOptions::default();
        let services = Services::new(config, Some(config_path), &opts).unwrap();
        for (name, role) in [("reader", Role::Registered), ("boss", Role::Admin)] {
            let _ = services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "");
        }
        // The library index (what `total_series` counts), as the startup pass builds it.
        services.library.recompile_all_now().await;
        // What the host sampler hands the detector every minute: the catalog's series.
        services
            .analytics
            .crawl()
            .refresh_catalog_from(services.library.index(), bunko_server::analytics::now_s());
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
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        ip: &str,
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
            .insert(ConnectInfo(SocketAddr::new(ip.parse().unwrap(), 40000)));
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

    async fn flush(&self) {
        self.services.analytics.flush_now().await.unwrap();
    }

    async fn crawlers(&self, range: &str) -> Value {
        let (status, body) = self
            .call(
                Some("boss"),
                "GET",
                &format!("/_admin/api/analytics/crawlers?range={range}"),
                &[],
                b"",
                ADMIN_IP,
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        serde_json::from_slice(&body).unwrap()
    }

    fn episodes(&self) -> i64 {
        rusqlite::Connection::open(self.storage.join("analytics.sqlite"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM crawl_episodes", [], |r| r.get(0))
            .unwrap()
    }

    /// What rclone (or davfs2) sends to mirror `series`: a `Depth: 1` listing of each
    /// series folder, then every volume (`get`).
    async fn walk(
        &self,
        user: Option<&str>,
        ip: &str,
        ua: &str,
        series: impl IntoIterator<Item = usize>,
        get: bool,
    ) -> u64 {
        let mut bytes = 0u64;
        let (status, _) = self
            .call(
                user,
                "PROPFIND",
                "/mokuro-reader/",
                &[("depth", "1"), ("user-agent", ua)],
                b"",
                ip,
            )
            .await;
        assert_eq!(status.as_u16(), 207);
        for i in series {
            let folder = format!("/mokuro-reader/{}/", enc(&series_name(i)));
            let (status, _) = self
                .call(
                    user,
                    "PROPFIND",
                    &folder,
                    &[("depth", "1"), ("user-agent", ua)],
                    b"",
                    ip,
                )
                .await;
            assert_eq!(status.as_u16(), 207, "{folder}");
            if get {
                for v in 1..=VOLUMES_PER_SERIES {
                    let (status, body) = self
                        .call(
                            user,
                            "GET",
                            &format!("{folder}Vol%20{v}.cbz"),
                            &[("user-agent", ua)],
                            b"",
                            ip,
                        )
                        .await;
                    assert_eq!(status, StatusCode::OK);
                    bytes += body.len() as u64;
                }
            }
        }
        bytes
    }
}

fn enc(s: &str) -> String {
    s.replace(' ', "%20")
}

const POSITIONS: &[u8] = br#"{"version":1,"positions":{"uuid-1":[1762000000000,12,3400,"dev-1"]}}"#;

/// Mokuro Reader's normal WebDAV sync, as a signed-in reader with a browser.
async fn reader_sync(env: &Env, ip: &str) {
    let ua = "Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0";
    let origin = "https://reader.example.com";
    let h = [("user-agent", ua), ("origin", origin)];
    let (s, _) = env
        .call(Some("reader"), "GET", "/login/api/me", &h, b"", ip)
        .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = env
        .call(Some("reader"), "OPTIONS", "/mokuro-reader/", &h, b"", ip)
        .await;
    assert!(s.is_success(), "{s}");
    // The whole library in one listing.
    let (s, _) = env
        .call(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/",
            &[
                ("depth", "infinity"),
                ("user-agent", ua),
                ("origin", origin),
            ],
            b"",
            ip,
        )
        .await;
    assert_eq!(s.as_u16(), 207);
    // Covers for every card the catalog scrolls past: the whole library.
    for i in 0..SERIES {
        let (s, _) = env
            .call(
                Some("reader"),
                "GET",
                &format!("/mokuro-reader/{}/Vol%201.webp", enc(&series_name(i))),
                &h,
                b"",
                ip,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
    }
    // A few volumes downloaded: HEAD, then GET.
    for i in 0..3 {
        let path = format!("/mokuro-reader/{}/Vol%201.cbz", enc(&series_name(i)));
        let (s, _) = env.call(Some("reader"), "HEAD", &path, &h, b"", ip).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = env.call(Some("reader"), "GET", &path, &h, b"", ip).await;
        assert_eq!(s, StatusCode::OK);
    }
    // Its own files.
    let (s, _) = env
        .call(
            Some("reader"),
            "PUT",
            "/mokuro-reader/positions.json",
            &h,
            POSITIONS,
            ip,
        )
        .await;
    assert!(s.is_success(), "{s}");
    let (s, _) = env
        .call(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/positions.json",
            &[("depth", "0"), ("user-agent", ua), ("origin", origin)],
            b"",
            ip,
        )
        .await;
    assert_eq!(s.as_u16(), 207);
}

#[tokio::test]
async fn the_readers_normal_sync_never_opens_an_episode() {
    let env = Env::new().await;
    // Several syncs (the reader re-lists on focus, refresh, the daily safety net).
    for _ in 0..3 {
        reader_sync(&env, READER_IP).await;
    }
    env.flush().await;
    assert_eq!(env.episodes(), 0);
    let v = env.crawlers("30d").await;
    assert_eq!(v["episodes"].as_array().unwrap().len(), 0);
    assert_eq!(v["catalog_series"], SERIES as u64);
    assert_eq!(v["thresholds"]["min_series"], 20);
    assert_eq!(v["thresholds"]["window_hours"], 6);
    assert_eq!(v["thresholds"]["min_coverage"], 0.25);
    let per_day = &v["episodes_per_day"];
    assert_eq!(per_day["t"].as_array().unwrap().len(), 30);
    assert!(
        per_day["series"][0]["values"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n == 0)
    );
    // The reader's IP is tracked (it downloaded) but well under the thresholds.
    assert_eq!(v["tracking"]["ips"], 1);
}

#[tokio::test]
async fn an_rclone_mirror_over_webdav_is_a_download_episode() {
    let env = Env::new().await;
    // rclone lists every folder, a folder that is no series too.
    let (status, _) = env
        .call(
            None,
            "PROPFIND",
            "/mokuro-reader/Extras/",
            &[("depth", "1"), ("user-agent", RCLONE_UA)],
            b"",
            RCLONE_IP,
        )
        .await;
    assert_eq!(status.as_u16(), 207);
    let bytes = env.walk(None, RCLONE_IP, RCLONE_UA, 0..SERIES, true).await;
    env.flush().await;
    let v = env.crawlers("24h").await;
    let eps = v["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 1, "{v:#}");
    let e = &eps[0];
    assert_eq!(e["ip"], RCLONE_IP);
    assert_eq!(e["kind"], "download");
    assert_eq!(e["series"], SERIES as u64);
    assert_eq!(e["volumes"], (SERIES * VOLUMES_PER_SERIES) as u64);
    assert_eq!(e["catalog_series"], SERIES as u64);
    assert_eq!(e["coverage"], 1.0);
    assert_eq!(e["status"], "open");
    assert_eq!(e["active"], true);
    assert_eq!(e["user_agent"], RCLONE_UA);
    // A listing and the volumes of every series (the root listing before the first
    // signal is not: only a signal makes an IP tracked).
    assert_eq!(e["requests"], (SERIES * (1 + VOLUMES_PER_SERIES)) as u64);
    assert!(e["bytes_out"].as_u64().unwrap() >= bytes, "{e}");
    let principals = e["principals"].as_array().unwrap();
    assert_eq!(principals.len(), 1);
    assert!(principals[0].as_str().unwrap().starts_with("a:"));
    let by_ip = v["by_ip"].as_array().unwrap();
    assert_eq!(by_ip.len(), 1);
    assert_eq!(by_ip[0]["episodes"], 1);
    // Daily points even on the hourly range: today's count is 1.
    let values = v["episodes_per_day"]["series"][0]["values"]
        .as_array()
        .unwrap();
    assert_eq!(values.last().unwrap(), 1);
    // The overview counts it too.
    let (s, body) = env
        .call(
            Some("boss"),
            "GET",
            "/_admin/api/analytics/overview?range=24h",
            &[],
            b"",
            ADMIN_IP,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let o: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(o["kpis"]["crawl_episodes"], 1);

    // It goes on: the same episode is updated in place, not a second one.
    env.walk(None, RCLONE_IP, RCLONE_UA, 0..2, true).await;
    env.flush().await;
    let v = env.crawlers("24h").await;
    let eps = v["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(
        eps[0]["requests"],
        (1 + (SERIES + 2) * (1 + VOLUMES_PER_SERIES)) as u64
    );
    assert_eq!(env.episodes(), 1);
    // The raw IP is only in crawl_episodes (and the other IP-retention tables).
    let ips: i64 = rusqlite::Connection::open(env.storage.join("analytics.sqlite"))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM principal_seen WHERE principal LIKE '%203.0.113%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ips, 0);
}

#[tokio::test]
async fn a_metadata_walk_over_webdav_is_a_metadata_episode() {
    let env = Env::new().await;
    // davfs2 listing every folder and reading every series.json, downloading nothing.
    env.walk(None, RCLONE_IP, DAVFS_UA, 0..30, false).await;
    for i in 0..30 {
        let (s, _) = env
            .call(
                None,
                "GET",
                &format!("/mokuro-reader/{}/series.json", enc(&series_name(i))),
                &[("user-agent", DAVFS_UA)],
                b"",
                RCLONE_IP,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
    }
    env.flush().await;
    let v = env.crawlers("7d").await;
    let eps = v["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 1, "{v:#}");
    assert_eq!(eps[0]["kind"], "metadata");
    assert_eq!(eps[0]["series"], 30);
    assert_eq!(eps[0]["volumes"], 0);
    assert_eq!(eps[0]["coverage"], 0.75);
    assert_eq!(eps[0]["user_agent"], DAVFS_UA);
}

#[tokio::test]
async fn a_scraper_walking_the_catalog_json_is_detected() {
    let env = Env::new().await;
    let ip = "198.51.100.77";
    let ua = "python-requests/2.32.3";
    let (s, _) = env
        .call(
            None,
            "GET",
            "/catalog/api/library",
            &[("user-agent", ua)],
            b"",
            ip,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    for i in 0..25 {
        let (s, body) = env
            .call(
                None,
                "GET",
                &format!("/catalog/api/series?name={}", enc(&series_name(i))),
                &[("user-agent", ua)],
                b"",
                ip,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        // And the cover of each (not a signal by itself).
        let (s, _) = env
            .call(
                None,
                "GET",
                &format!(
                    "/catalog/api/cover?path={}/Vol%201.webp",
                    enc(&series_name(i))
                ),
                &[("user-agent", ua)],
                b"",
                ip,
            )
            .await;
        assert!(s.is_success() || s == StatusCode::NOT_FOUND, "{s}");
    }
    env.flush().await;
    let v = env.crawlers("30d").await;
    let eps = v["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 1, "{v:#}");
    assert_eq!(eps[0]["kind"], "metadata");
    assert_eq!(eps[0]["series"], 25);
    assert_eq!(eps[0]["ip"], ip);
    assert_eq!(eps[0]["user_agent"], ua);
}

#[tokio::test]
async fn two_ips_splitting_the_catalog_are_two_episodes_but_small_slices_are_not() {
    let env = Env::new().await;
    // Halves: each IP alone touches half the catalog.
    env.walk(None, "203.0.113.21", RCLONE_UA, 0..20, true).await;
    env.walk(None, "203.0.113.22", RCLONE_UA, 20..40, true)
        .await;
    // Slices of 10 from four other IPs: under min_series each (detection is per IP).
    for (n, ip) in [
        "198.51.100.1",
        "198.51.100.2",
        "198.51.100.3",
        "198.51.100.4",
    ]
    .iter()
    .enumerate()
    {
        env.walk(None, ip, RCLONE_UA, n * 10..n * 10 + 10, true)
            .await;
    }
    env.flush().await;
    let v = env.crawlers("30d").await;
    let mut ips: Vec<&str> = v["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["ip"].as_str().unwrap())
        .collect();
    ips.sort();
    assert_eq!(ips, ["203.0.113.21", "203.0.113.22"]);
    for e in v["episodes"].as_array().unwrap() {
        assert_eq!(e["series"], 20);
        assert_eq!(e["coverage"], 0.5);
        assert_eq!(e["kind"], "download");
    }
    assert_eq!(v["by_ip"].as_array().unwrap().len(), 2);
    assert_eq!(v["tracking"]["ips"], 6);
}

#[tokio::test]
async fn a_signed_in_user_crawling_is_named_by_the_episode() {
    let env = Env::new().await;
    env.walk(Some("reader"), RCLONE_IP, RCLONE_UA, 0..SERIES, true)
        .await;
    // An admin is not exempt either.
    env.walk(Some("boss"), "192.0.2.201", RCLONE_UA, 0..SERIES, false)
        .await;
    env.flush().await;
    let v = env.crawlers("30d").await;
    let eps = v["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 2, "{v:#}");
    let of = |ip: &str| {
        eps.iter()
            .find(|e| e["ip"] == ip)
            .unwrap_or_else(|| panic!("{ip}: {v:#}"))
    };
    assert_eq!(of(RCLONE_IP)["principals"], serde_json::json!(["u:reader"]));
    assert_eq!(of(RCLONE_IP)["kind"], "download");
    assert_eq!(
        of("192.0.2.201")["principals"],
        serde_json::json!(["u:boss"])
    );
    assert_eq!(of("192.0.2.201")["kind"], "metadata");
}

#[tokio::test]
async fn ten_thousand_ips_keep_the_trackers_bounded() {
    let env = Env::new().await;
    // A crawler first, then 10 000 one-off downloaders (a botnet, or an IPv6 /64).
    env.walk(None, RCLONE_IP, RCLONE_UA, 0..SERIES, true).await;
    env.flush().await;
    for i in 0..10_000u32 {
        let ip = format!("2001:db8::{i:x}");
        let (s, _) = env
            .call(
                None,
                "GET",
                &format!(
                    "/mokuro-reader/{}/Vol%201.cbz",
                    enc(&series_name(i as usize % SERIES))
                ),
                &[],
                b"",
                &ip,
            )
            .await;
        assert_eq!(s, StatusCode::OK);
    }
    let (ips, entries) = env.services.analytics.crawl().tracked();
    assert!(ips <= bunko_server::analytics::crawl::MAX_IPS, "{ips}");
    assert!(entries <= bunko_server::analytics::crawl::MAX_ENTRIES);
    env.flush().await;
    // The crawler's open episode kept its tracker; nobody else crawled.
    let v = env.crawlers("24h").await;
    let eps = v["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0]["ip"], RCLONE_IP);
    assert_eq!(eps[0]["status"], "open");
    assert!(v["tracking"]["ips"].as_u64().unwrap() <= 4096);
}

#[tokio::test]
async fn a_restart_closes_the_episodes_the_previous_run_left_open() {
    let env = Env::new().await;
    env.walk(None, RCLONE_IP, RCLONE_UA, 0..SERIES, true).await;
    env.flush().await;
    let v = env.crawlers("24h").await;
    assert_eq!(v["episodes"][0]["status"], "open");
    let Env {
        _dir: dir,
        storage,
        services,
        app,
    } = env;
    services.analytics.stop().await;
    drop((services, app));
    let env = Env::over(dir, storage).await;
    env.flush().await;
    let v = env.crawlers("24h").await;
    let eps = v["episodes"].as_array().unwrap();
    assert_eq!(eps.len(), 1);
    assert_eq!(eps[0]["status"], "closed");
    assert_eq!(eps[0]["active"], false);
    // Crawling on opens a new one.
    env.walk(None, RCLONE_IP, RCLONE_UA, 0..SERIES, true).await;
    env.flush().await;
    let v = env.crawlers("24h").await;
    let open: Vec<&Value> = v["episodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["status"] == "open")
        .collect();
    assert_eq!(open.len(), 1);
    assert_eq!(v["by_ip"][0]["episodes"], 2);
}

#[tokio::test]
async fn analytics_off_detects_nothing() {
    let env = Env::new().await;
    {
        let mut c = env.services.core.config.write();
        c.analytics.enabled = false;
    }
    env.services.analytics.settings_changed();
    env.walk(None, RCLONE_IP, RCLONE_UA, 0..SERIES, true).await;
    env.flush().await;
    assert_eq!(env.episodes(), 0);
}
