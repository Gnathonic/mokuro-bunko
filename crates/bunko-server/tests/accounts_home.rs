//! Home page, `/api/stats`, `/api/health` and `GET /`. Ports
//! tests/integration/test_home_page.py and test_home_stats.py.

mod accounts_support;

use accounts_support::*;
use bunko_core::Role;
use bunko_server::accounts::{HealthSource, LibraryCounts};
use serde_json::{Value, json};
use std::sync::Arc;

struct Volumes(Result<u64, String>);
impl LibraryCounts for Volumes {
    fn total_volumes(&self) -> Result<u64, String> {
        self.0.clone()
    }

    /// Volumes without a primary sidecar: one less than there are.
    fn pending_ocr(&self) -> Result<u64, String> {
        self.0.clone().map(|n| n.saturating_sub(1))
    }
}

/// The scheduler's block: 7 waiting `(volume, generation)` jobs.
struct Ocr;
impl HealthSource for Ocr {
    fn ocr_health(&self) -> Value {
        json!({"backend": "cpu", "worker_alive": true, "pending": null, "failed": 1, "queued_jobs": 7})
    }
}

/// Five live users and one deleted, plus an admin so `/` is not a setup redirect.
fn seeded() -> Env {
    let env = Env::new();
    env.user("admin", "password123", Role::Admin);
    for n in 0..4 {
        env.user(&format!("reader{n}"), "password123", Role::Registered);
    }
    env.user("ghost", "password123", Role::Registered);
    env.db.delete_user("ghost").unwrap();
    env
}

#[tokio::test]
async fn stats_returns_real_counts() {
    let mut env = seeded();
    env.deps.library = Some(Arc::new(Volumes(Ok(3))));
    let r = env.send(empty(req("GET", "/api/stats"))).await;
    assert_eq!(r.status, 200);
    let b = r.json();
    assert_eq!(b["total_users"], 5);
    assert_eq!(b["total_volumes"], 3);
    assert_eq!(b["total_pages_read"], 0);
    assert_eq!(b["total_characters_read"], 0);
    assert_eq!(b["total_reading_time_seconds"], 0);
    assert_eq!(b["total_reading_time_formatted"], "0s");
    assert!(b["last_updated"].as_u64().unwrap() > 1_700_000_000);
    assert!(
        r.text()
            .starts_with(r#"{"total_users":5,"total_volumes":3,"total_pages_read":0,"#)
    );
}

#[tokio::test]
async fn stats_degrade_to_zero() {
    let mut env = Env::new();
    env.deps.library = Some(Arc::new(Volumes(Err("index broken".into()))));
    let b = env.send(empty(req("GET", "/api/stats"))).await.json();
    assert_eq!(
        (b["total_users"].clone(), b["total_volumes"].clone()),
        (json!(0), json!(0))
    );
}

#[tokio::test]
async fn health_reports_ok_with_counts() {
    let mut env = seeded();
    env.deps.library = Some(Arc::new(Volumes(Ok(3))));
    env.deps.health = Some(Arc::new(Ocr));
    let r = env.send(empty(req("GET", "/api/health"))).await;
    assert_eq!(r.status, 200);
    let b = r.json();
    assert_eq!(b["status"], "ok");
    assert_eq!(b["db_status"], "ok");
    assert_eq!(b["library_status"], "ok");
    assert_eq!(b["total_users"], 5);
    assert_eq!(b["total_volumes"], 3);
    assert!(b["uptime_seconds"].is_u64());
    // Regression (upgrade test): `pending` is 0.5.3's count of volumes without a primary
    // sidecar (from the library index), not the scheduler's waiting jobs.
    assert_eq!(
        b["ocr"],
        json!({"backend": "cpu", "worker_alive": true, "pending": 2, "failed": 1, "queued_jobs": 7})
    );
    let ocr_keys: Vec<&str> = b["ocr"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        ocr_keys,
        [
            "backend",
            "worker_alive",
            "pending",
            "failed",
            "queued_jobs"
        ]
    );
    let keys: Vec<&str> = b.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "status",
            "uptime_seconds",
            "db_status",
            "library_status",
            "total_users",
            "total_volumes",
            "ocr"
        ]
    );
}

#[tokio::test]
async fn health_without_library_or_ocr() {
    let env = seeded();
    let b = env.send(empty(req("GET", "/api/health"))).await.json();
    assert_eq!(b["status"], "ok");
    assert_eq!(b["library_status"], "unavailable");
    assert_eq!(b["total_volumes"], Value::Null);
    assert_eq!(
        b["ocr"],
        json!({"backend": "skip", "worker_alive": null, "pending": null, "failed": 0})
    );
}

#[tokio::test]
async fn health_degraded_when_library_errors() {
    let mut env = seeded();
    env.deps.library = Some(Arc::new(Volumes(Err("boom".into()))));
    let r = env.send(empty(req("GET", "/api/health"))).await;
    assert_eq!(r.status, 200);
    let b = r.json();
    assert_eq!(
        (
            b["status"].as_str(),
            b["library_status"].as_str(),
            b["db_status"].as_str()
        ),
        (Some("degraded"), Some("error"), Some("ok"))
    );
}

#[tokio::test]
async fn methods() {
    let env = seeded();
    for path in ["/api/health", "/api/stats"] {
        let r = env.send(empty(req("OPTIONS", path))).await;
        assert_eq!((r.status, r.header("allow")), (204, Some("GET, OPTIONS")));
        let r = env.send(empty(req("POST", path))).await;
        assert_eq!(
            (r.status, r.json()),
            (405, json!({"error": "Method not allowed"}))
        );
    }
}

#[tokio::test]
async fn home_static_files() {
    let env = seeded();
    let r = env.send(empty(req("GET", "/_home/styles.css"))).await;
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").unwrap().contains("text/css"));
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    let r = env.send(empty(req("GET", "/_home/home.js"))).await;
    assert!(r.header("content-type").unwrap().contains("javascript"));
    assert!(r.text().contains("loadStats"));
    let r = env.send(empty(req("GET", "/_home/nonexistent.css"))).await;
    assert_eq!(
        (r.status, r.json()),
        (404, json!({"error": "File not found"}))
    );
    let r = env
        .send(empty(req("GET", "/_home/..%2F..%2Fetc%2Fpasswd")))
        .await;
    assert_eq!(r.status, 404);
}

#[tokio::test]
async fn root_for_browsers_and_webdav_clients() {
    let env = seeded();
    let r = env
        .send(empty(
            req("GET", "/")
                .header("accept", "text/html,application/xhtml+xml")
                .header("user-agent", "Mozilla/5.0"),
        ))
        .await;
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").unwrap().contains("text/html"));
    assert!(r.text().contains("<!DOCTYPE html>") && r.text().contains("Mokuro Bunko"));
    // WebDAV clients and non-GET methods fall through to WebDAV.
    let r = env
        .send(empty(req("GET", "/").header("user-agent", "davfs2/1.5.6")))
        .await;
    assert_eq!((r.status, r.text().as_str()), (418, "dav"));
    let r = env
        .send(empty(req("PROPFIND", "/").header("depth", "1")))
        .await;
    assert_eq!(r.status, 418);
    let r = env.send(empty(req("HEAD", "/"))).await;
    assert_eq!(r.status, 418);
    // No Accept and no DAV User-Agent counts as a browser (0.5.2 default).
    assert_eq!(env.send(empty(req("GET", "/"))).await.status, 200);
}

#[tokio::test]
async fn root_redirects_to_the_catalog_when_it_is_the_homepage() {
    let env = seeded();
    {
        let mut c = env.config();
        c.catalog.enabled = true;
        c.catalog.use_as_homepage = true;
    }
    let r = env
        .send(empty(req("GET", "/").header("accept", "text/html")))
        .await;
    assert_eq!((r.status, r.header("location")), (302, Some("/catalog/")));
    assert!(r.body.is_empty());
    // use_as_homepage without catalog.enabled is ignored.
    env.config().catalog.enabled = false;
    assert_eq!(
        env.send(empty(req("GET", "/").header("accept", "text/html")))
            .await
            .status,
        200
    );
}
