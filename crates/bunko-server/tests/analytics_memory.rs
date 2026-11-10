//! The analytics window's memory stays bounded (docs/design/analytics.md §2, §4.5)
//! under the worst flood a client can send: every request from a new address (any
//! IPv6 /64 allows it), so a new anonymous id, with a long, new `Origin` and
//! `User-Agent`. A counting allocator measures what the server keeps after the
//! responses are done, before the window is flushed.
//!
//! Alone in its binary: the allocator counts the whole process.

// Fixtures are set up with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicIsize, Ordering};

use axum::body::Body;
use axum::extract::ConnectInfo;
use bunko_core::Config;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every call is forwarded to `System` unchanged; only a counter is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        }
        p
    }

    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size() as isize, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size() as isize, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            LIVE.fetch_add(new as isize - l.size() as isize, Ordering::Relaxed);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> isize {
    LIVE.load(Ordering::Relaxed)
}

const MIB: isize = 1 << 20;

#[tokio::test]
async fn a_flood_of_new_clients_keeps_the_window_within_its_budget() {
    let n: usize = std::env::var("FLOOD_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60_000);
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("storage");
    std::fs::create_dir_all(storage.join("library")).unwrap();
    let mut config = Config::default();
    config.storage.base_path = storage.clone();
    let opts = ServeOptions::default();
    let services = Services::new(config, Some(dir.path().join("config.yaml")), &opts).unwrap();
    let app = assemble(&services, &opts);
    let send = |ip: String, origin: String, ua: String| {
        let app = app.clone();
        async move {
            let mut req = Request::builder()
                .method("GET")
                .uri("/robots.txt")
                .header("host", "localhost:8080")
                .header("origin", origin)
                .header("user-agent", ua)
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(SocketAddr::new(ip.parse().unwrap(), 40000)));
            let resp = app.oneshot(req).await.unwrap();
            drop(resp.into_body().collect().await);
        }
    };
    // Warm up every lazy cache of the request path, then start from an empty window.
    for i in 0..500u32 {
        send(
            format!("2001:db8:ffff::{i:x}"),
            "https://warm.example.com".into(),
            "warm".into(),
        )
        .await;
    }
    services.analytics.flush_now().await.unwrap();
    let before = live();
    let ua = "Mozilla/5.0 ".to_string() + &"x".repeat(240);
    for i in 0..n {
        send(
            format!("2001:db8:{:x}:{:x}::1", i >> 16, i & 0xffff),
            format!("https://{}{i}.example.com", "o".repeat(200)),
            format!("{ua}{i}"),
        )
        .await;
    }
    let held = live() - before;
    println!(
        "{n} requests from new clients: the server holds {:.2} MiB more before the flush",
        held as f64 / MIB as f64
    );
    // The window's client-keyed maps are budgeted at 10 MiB (with every estimate
    // erring high); the fixed-key maps add a little. The design's 32 MB covers two
    // windows (one being written, one filling) and the area caches.
    assert!(held < 12 * MIB, "held {held} bytes");
    services.analytics.flush_now().await.unwrap();
    let ips: i64 = rusqlite::Connection::open(storage.join("analytics.sqlite"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM ip_daily", [], |r| r.get(0))
        .unwrap();
    let origins: i64 = rusqlite::Connection::open(storage.join("analytics.sqlite"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM origins_daily", [], |r| r.get(0))
        .unwrap();
    let requests: i64 = rusqlite::Connection::open(storage.join("analytics.sqlite"))
        .unwrap()
        .query_row("SELECT SUM(requests) FROM traffic_hourly", [], |r| r.get(0))
        .unwrap();
    println!("after the flush: {ips} ip_daily rows, {origins} origins_daily rows");
    assert!(ips > 1_000 && (ips as usize) < n, "{ips}");
    assert!(origins <= 501, "{origins}");
    // Every request is still in the totals.
    assert_eq!(requests as usize, n + 500);
}
