//! The host sampler through the whole server (docs/design/analytics.md §8): samples
//! land in `host_minute` and `host_hourly`, `GET /_admin/api/analytics/host` answers
//! per-minute series for 24h and per-hour ones beyond, `now` and the overview's KPIs
//! carry this process's real memory, and the connection/in-flight peaks come from the
//! gauges (WebDAV and web requests alike).

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::analytics::host::{SUPPORTS_CPU, SUPPORTS_RSS};
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

const IP: &str = "192.0.2.30";

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

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library/series")).unwrap();
        std::fs::write(storage.join("library/series/vol1.cbz"), vec![7u8; 200_000]).unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        let opts = ServeOptions::default();
        let services = Services::new(config, Some(dir.path().join("config.yaml")), &opts).unwrap();
        services
            .db
            .create_user("boss", "pass1234", Role::Admin, UserStatus::Active, "")
            .unwrap();
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            storage,
            services,
            app,
        }
    }

    fn request(&self, user: Option<&str>, method: &str, path: &str) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080");
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        if method == "PROPFIND" {
            b = b.header("depth", "1");
        }
        let mut req = b.body(Body::empty()).unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::new(IP.parse().unwrap(), 40000)));
        req
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let resp = self
            .app
            .clone()
            .oneshot(self.request(Some("boss"), "GET", path))
            .await
            .unwrap();
        let status = resp.status();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn sample(&self) {
        let a = &self.services.analytics;
        a.host().sample_now(a).await.unwrap();
    }

    fn int(&self, sql: &str) -> i64 {
        rusqlite::Connection::open(self.storage.join("analytics.sqlite"))
            .unwrap()
            .query_row(sql, [], |r| r.get::<_, Option<i64>>(0))
            .unwrap()
            .unwrap_or(0)
    }
}

fn values<'a>(v: &'a Value, key: &str) -> &'a Vec<Value> {
    v["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == key)
        .unwrap_or_else(|| panic!("no series {key}: {v:#}"))["values"]
        .as_array()
        .unwrap()
}

#[tokio::test]
async fn samples_become_minutes_hours_and_the_host_view() {
    let env = Env::new();
    // Requests held open while sampling: two WebDAV downloads whose bodies are not read
    // yet, so they are still in flight (and their connections would be, behind a
    // listener; the oneshot router has none, so connections stay 0 here).
    let a = env
        .app
        .clone()
        .oneshot(env.request(None, "GET", "/mokuro-reader/series/vol1.cbz"))
        .await
        .unwrap();
    let b = env
        .app
        .clone()
        .oneshot(env.request(None, "PROPFIND", "/mokuro-reader/series/"))
        .await
        .unwrap();
    env.sample().await;
    drop((a, b));
    // A second sample, after some CPU work, has an interval to measure.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let mut x = 0u64;
    for i in 0..3_000_000u64 {
        x = x.wrapping_mul(31).wrapping_add(i);
    }
    std::hint::black_box(x);
    env.sample().await;

    assert!(env.int("SELECT COUNT(*) FROM host_minute") >= 1);
    assert!(env.int("SELECT COUNT(*) FROM host_hourly") >= 1);
    assert_eq!(env.int("SELECT MAX(inflight_max) FROM host_minute"), 2);
    if SUPPORTS_RSS {
        assert!(env.int("SELECT MAX(rss_max) FROM host_minute") > 1 << 20);
        assert!(env.int("SELECT MAX(rss_max) FROM host_hourly") > 1 << 20);
    }

    let (status, v) = env.get("/_admin/api/analytics/host?range=24h").await;
    assert_eq!(status, StatusCode::OK, "{v:#}");
    assert_eq!(v["resolution"], "minute");
    assert_eq!(v["step"], 60);
    assert_eq!(v["range"]["granularity"], "hour");
    let t = v["t"].as_array().unwrap();
    assert!(t.len() > 23 * 60 && t.len() <= 24 * 60, "{}", t.len());
    assert_eq!(t[1].as_i64().unwrap() - t[0].as_i64().unwrap(), 60);
    let rss = values(&v, "rss");
    assert_eq!(rss.len(), t.len());
    // The minutes before the server ran have no reading (null), never a made-up 0.
    assert!(rss[0].is_null());
    let inflight = values(&v, "inflight_max");
    assert_eq!(inflight.iter().filter_map(Value::as_u64).max(), Some(2));
    if SUPPORTS_RSS {
        assert!(rss.iter().filter_map(Value::as_u64).any(|b| b > 1 << 20));
        assert!(v["now"]["rss"].as_u64().unwrap() > 1 << 20);
        assert!(v["now"]["peak_rss"].as_u64().unwrap() >= v["now"]["rss"].as_u64().unwrap() / 2);
        assert!(v["now"]["host_total"].as_u64().unwrap() > v["now"]["rss"].as_u64().unwrap());
        assert!(v["now"]["host_available"].as_u64().is_some());
    }
    if SUPPORTS_CPU {
        assert!(v["now"]["cpu_pct"].as_f64().is_some(), "{:#}", v["now"]);
        assert!(values(&v, "cpu_pct").iter().any(Value::is_f64));
    }
    assert_eq!(v["platform_supported"]["rss"], SUPPORTS_RSS);
    assert_eq!(v["platform_supported"]["cpu"], SUPPORTS_CPU);
    // In flight now: only this admin request.
    assert_eq!(v["now"]["inflight"], 1);
    assert_eq!(v["now"]["reading_queue"], 0);
    for key in ["rss_max", "cpu_max", "connections_max"] {
        assert_eq!(values(&v, key).len(), t.len());
    }

    let (status, v) = env.get("/_admin/api/analytics/host?range=7d").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["resolution"], "hour");
    let t = v["t"].as_array().unwrap();
    assert_eq!(t.len(), 7 * 24);
    assert_eq!(values(&v, "inflight_max").last().unwrap(), 2);
    let (status, v) = env.get("/_admin/api/analytics/host?range=30d").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["range"]["granularity"], "hour");
    let t = v["t"].as_array().unwrap();
    assert!(t.len() > 29 * 24 && t.len() <= 30 * 24, "{}", t.len());
    assert_eq!(v["range"]["from"], t[0]);
    if SUPPORTS_RSS {
        assert!(values(&v, "rss").last().unwrap().as_u64().unwrap() > 1 << 20);
    }
    // Server readings are kept 7 days per minute and `hourly_days` per hour.
    let (status, _) = env.get("/_admin/api/analytics/host?range=90d").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The overview's KPIs read the same sampler.
    let (status, o) = env.get("/_admin/api/analytics/overview?range=24h").await;
    assert_eq!(status, StatusCode::OK);
    if SUPPORTS_RSS {
        assert!(o["kpis"]["rss_bytes"].as_u64().unwrap() > 1 << 20);
        assert!(o["kpis"]["rss_peak_bytes"].as_u64().is_some());
    }
    if SUPPORTS_CPU {
        assert!(o["kpis"]["cpu_pct"].as_f64().is_some());
    }
}

#[tokio::test]
async fn analytics_off_samples_but_writes_nothing() {
    let env = Env::new();
    {
        let mut c = env.services.core.config.write();
        c.analytics.enabled = false;
    }
    env.services.analytics.settings_changed();
    env.sample().await;
    assert_eq!(env.int("SELECT COUNT(*) FROM host_minute"), 0);
    let (status, v) = env.get("/_admin/api/analytics/host?range=24h").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["enabled"], false);
    if SUPPORTS_RSS {
        assert!(v["now"]["rss"].as_u64().is_some());
    }
}

#[tokio::test]
async fn old_minutes_are_pruned_and_their_hours_kept() {
    let env = Env::new();
    env.sample().await;
    let now = bunko_server::analytics::now_s();
    // Nine days on: the minutes are past `host_minute_days` (7), the hour is not past
    // `hourly_days` (90).
    env.services
        .analytics
        .maintain_at(now + 9 * 86400)
        .await
        .unwrap();
    assert_eq!(env.int("SELECT COUNT(*) FROM host_minute"), 0);
    assert_eq!(env.int("SELECT COUNT(*) FROM host_hourly"), 1);
}
