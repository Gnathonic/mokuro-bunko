//! The accounting layer's cost per request (docs/design/analytics.md §2: < 5 µs
//! median), measured, never asserted: plain HTTP endpoints and WebDAV methods, each
//! sent through the whole router with analytics on and off in alternating blocks.
//!
//! `cargo test -p bunko-server --test analytics_overhead -- --ignored --nocapture`

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Scenario {
    name: &'static str,
    kind: &'static str,
    method: &'static str,
    path: &'static str,
    user: Option<&'static str>,
    headers: &'static [(&'static str, &'static str)],
    body: &'static [u8],
    /// Requests per mode.
    n: usize,
}

const POSITIONS: &[u8] = br#"{"version":1,"positions":{"uuid-1":[1762000000000,12,3400,"dev-1"]}}"#;

fn scenarios() -> Vec<Scenario> {
    let n = std::env::var("BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000usize);
    let s = |name, kind, method, path, user, headers, body, n| Scenario {
        name,
        kind,
        method,
        path,
        user,
        headers,
        body,
        n,
    };
    vec![
        s(
            "GET /robots.txt",
            "http",
            "GET",
            "/robots.txt",
            None,
            &[],
            b"",
            n,
        ),
        s(
            "GET /api/nav/config",
            "http",
            "GET",
            "/api/nav/config",
            None,
            &[],
            b"",
            n,
        ),
        s(
            "GET /login/api/me (Bearer)",
            "http",
            "GET",
            "/login/api/me",
            Some("bearer"),
            &[],
            b"",
            n,
        ),
        s(
            "GET small.cbz",
            "webdav",
            "GET",
            "/mokuro-reader/series/small.cbz",
            None,
            &[],
            b"",
            n,
        ),
        s(
            "GET small.cbz Range",
            "webdav",
            "GET",
            "/mokuro-reader/series/small.cbz",
            None,
            &[("range", "bytes=2-5")],
            b"",
            n,
        ),
        s(
            "HEAD small.cbz",
            "webdav",
            "HEAD",
            "/mokuro-reader/series/small.cbz",
            None,
            &[],
            b"",
            n,
        ),
        s(
            "OPTIONS /mokuro-reader/",
            "webdav",
            "OPTIONS",
            "/mokuro-reader/",
            None,
            &[],
            b"",
            n,
        ),
        s(
            "PROPFIND depth 0 (file)",
            "webdav",
            "PROPFIND",
            "/mokuro-reader/series/small.cbz",
            None,
            &[("depth", "0")],
            b"",
            n / 5,
        ),
        s(
            "PROPFIND depth 1 (series)",
            "webdav",
            "PROPFIND",
            "/mokuro-reader/series",
            None,
            &[("depth", "1")],
            b"",
            n / 5,
        ),
        s(
            "PROPFIND infinity gzip (cached)",
            "webdav",
            "PROPFIND",
            "/mokuro-reader/",
            Some("reader"),
            &[("accept-encoding", "gzip")],
            b"",
            n / 5,
        ),
        s(
            "PUT positions.json (Basic)",
            "webdav",
            "PUT",
            "/mokuro-reader/positions.json",
            Some("reader"),
            &[],
            POSITIONS,
            n / 25,
        ),
    ]
}

static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

fn request(s: &Scenario) -> Request<Body> {
    let mut b = Request::builder()
        .method(s.method)
        .uri(s.path)
        .header("host", "localhost:8080")
        .header("user-agent", "bench/1.0");
    match s.user {
        // A bearer token (the reader's steady state): a database lookup, no bcrypt.
        Some("bearer") => {
            b = b.header("authorization", format!("Bearer {}", TOKEN.get().unwrap()));
        }
        // Basic: WebDAV clients send it on every request (a cached login).
        Some(u) => b = b.header("authorization", basic(u)),
        None => {}
    }
    for (k, v) in s.headers {
        b = b.header(*k, *v);
    }
    if !s.body.is_empty() {
        b = b.header("content-length", s.body.len().to_string());
    }
    let mut req = b.body(Body::from(s.body.to_vec())).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([203, 0, 113, 50], 40000))));
    req
}

async fn block(app: &Router, s: &Scenario, n: usize) -> Duration {
    let started = Instant::now();
    for _ in 0..n {
        let resp = app.clone().oneshot(request(s)).await.unwrap();
        assert!(
            resp.status().as_u16() < 400,
            "{} -> {}",
            s.name,
            resp.status()
        );
        let _ = resp.into_body().collect().await.unwrap();
    }
    started.elapsed()
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

#[tokio::test]
#[ignore = "a measurement, run on demand with --ignored --nocapture"]
async fn analytics_layer_overhead_http_and_webdav() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    std::fs::create_dir_all(storage.join("library/series")).unwrap();
    std::fs::write(storage.join("library/series/small.cbz"), b"small volume").unwrap();
    for i in 0..20 {
        std::fs::write(storage.join(format!("library/series/v{i:02}.cbz")), b"v").unwrap();
    }
    let mut config = Config::default();
    config.storage.base_path = storage.clone();
    let opts = ServeOptions::default();
    let services = Services::new(config, Some(dir.path().join("config.yaml")), &opts).unwrap();
    services
        .db
        .create_user(
            "reader",
            "pass1234",
            Role::Registered,
            UserStatus::Active,
            "",
        )
        .unwrap();
    let (token, _) = services
        .db
        .create_auth_token("reader", bunko_db::TokenKind::Web, "bench", None)
        .unwrap();
    let _ = TOKEN.set(token);
    let app = assemble(&services, &opts);
    let a = services.analytics.clone();
    let set = |on: bool| {
        services.core.config.write().analytics.enabled = on;
        a.settings_changed();
    };
    const ROUNDS: usize = 10;
    let mut lines = Vec::new();
    for s in scenarios() {
        set(true);
        eprintln!("[bench] {} warming up", s.name);
        let _ = block(&app, &s, (s.n / 10).max(50)).await; // warm-up (caches, cached login)
        let per = (s.n / ROUNDS).max(1);
        let (mut on, mut off) = (Vec::new(), Vec::new());
        for round in 0..ROUNDS {
            // Alternate which mode goes first, so drift lands on both.
            for mode in [round % 2 == 0, round % 2 != 0] {
                set(mode);
                let t = block(&app, &s, per).await;
                let us = t.as_secs_f64() * 1e6 / per as f64;
                if mode {
                    on.push(us)
                } else {
                    off.push(us)
                }
                a.flush_now().await.unwrap();
            }
        }
        let (m_on, m_off) = (median(on), median(off));
        eprintln!("[bench] {} done", s.name);
        lines.push(format!(
            "{:<8} {:<34} n={:>6}/mode  off {:>9.2} us/req  on {:>9.2} us/req  overhead {:>+8.2} us/req",
            s.kind,
            s.name,
            per * ROUNDS,
            m_off,
            m_on,
            m_on - m_off
        ));
    }
    set(true);
    println!("\nanalytics layer overhead (median of {ROUNDS} alternating blocks per mode):");
    for l in &lines {
        println!("{l}");
    }
    // The counted requests did land.
    a.flush_now().await.unwrap();
    let total: i64 = rusqlite::Connection::open(storage.join("analytics.sqlite"))
        .unwrap()
        .query_row("SELECT SUM(requests) FROM traffic_hourly", [], |r| r.get(0))
        .unwrap();
    println!("requests recorded while on: {total}");
}
