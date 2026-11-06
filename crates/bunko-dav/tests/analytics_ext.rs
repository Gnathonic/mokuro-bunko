//! The response extensions the server's request accounting reads (docs/design/
//! analytics.md §4.2): `DavInfo` on every answer, `Served` on file bodies (and 304s),
//! `ListingInfo` on cached listings.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod common;

use axum::body::Body;
use bunko_dav::{DavDepth, DavInfo, DavTarget, ListingInfo, Served};
use common::*;
use http::Response;
use http_body_util::BodyExt;

async fn raw(
    env: &Env,
    user: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Response<Body> {
    env.dav
        .handle(request(method, path, headers, body), env.ctx(user))
        .await
}

fn info(r: &Response<Body>) -> DavInfo {
    r.extensions()
        .get::<DavInfo>()
        .cloned()
        .expect("DavInfo on every DAV response")
}

async fn body_len(r: Response<Body>) -> u64 {
    r.into_body().collect().await.unwrap().to_bytes().len() as u64
}

#[tokio::test]
async fn file_bodies_carry_served() {
    let env = Env::new();
    let r = raw(
        &env,
        None,
        "GET",
        "/mokuro-reader/series/vol1.cbz",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        info(&r),
        DavInfo {
            target: DavTarget::Library,
            rel: Some("series/vol1.cbz".into()),
            depth: None
        }
    );
    assert_eq!(
        r.extensions().get::<Served>(),
        Some(&Served {
            size: 8,
            len: 8,
            accel: false
        })
    );
    let etag = r.headers()["etag"].to_str().unwrap().to_string();
    assert_eq!(body_len(r).await, 8);

    let r = raw(
        &env,
        None,
        "GET",
        "/mokuro-reader/series/vol1.cbz",
        &[("range", "bytes=2-4")],
        b"",
    )
    .await;
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.extensions().get::<Served>(),
        Some(&Served {
            size: 8,
            len: 3,
            accel: false
        })
    );

    let r = raw(
        &env,
        None,
        "HEAD",
        "/mokuro-reader/series/vol1.cbz",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.extensions().get::<Served>(),
        Some(&Served {
            size: 8,
            len: 0,
            accel: false
        })
    );
    assert_eq!(body_len(r).await, 0);

    let r = raw(
        &env,
        None,
        "GET",
        "/mokuro-reader/series/vol1.cbz",
        &[("if-none-match", &etag)],
        b"",
    )
    .await;
    assert_eq!(r.status(), 304);
    assert_eq!(
        r.extensions().get::<Served>(),
        Some(&Served {
            size: 8,
            len: 0,
            accel: false
        })
    );

    let r = raw(
        &env,
        None,
        "GET",
        "/mokuro-reader/series/vol1.cbz",
        &[("range", "bytes=50-60")],
        b"",
    )
    .await;
    assert_eq!(r.status(), 416);
    assert!(r.extensions().get::<Served>().is_none());
    assert_eq!(info(&r).target, DavTarget::Library);

    // The caller's own file.
    let r = raw(
        &env,
        Some("reader"),
        "GET",
        "/mokuro-reader/volume-data.json",
        &[],
        b"",
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(
        info(&r),
        DavInfo {
            target: DavTarget::Progress,
            rel: Some("volume-data.json".into()),
            depth: None
        }
    );
    assert_eq!(r.extensions().get::<Served>().map(|s| s.size), Some(15));
}

#[tokio::test]
async fn nginx_offload_estimates_what_nginx_sends() {
    let env = Env::new();
    let mut ctx = env.ctx(None);
    ctx.nginx_accel = true;
    let r = env
        .dav
        .handle(
            request("GET", "/mokuro-reader/series/vol1.cbz", &[], b""),
            ctx.clone(),
        )
        .await;
    assert!(r.headers().contains_key("x-accel-redirect"));
    assert_eq!(
        r.extensions().get::<Served>(),
        Some(&Served {
            size: 8,
            len: 8,
            accel: true
        })
    );
    let r = env
        .dav
        .handle(
            request(
                "GET",
                "/mokuro-reader/series/vol1.cbz",
                &[("range", "bytes=0-1")],
                b"",
            ),
            ctx.clone(),
        )
        .await;
    assert_eq!(
        r.extensions().get::<Served>(),
        Some(&Served {
            size: 8,
            len: 2,
            accel: true
        })
    );
    assert_eq!(body_len(r).await, 0);
    // A range past the end (a download manager resuming a file it already has): a
    // 416 from here, as without offload, not an offloaded "whole file".
    let r = env
        .dav
        .handle(
            request(
                "GET",
                "/mokuro-reader/series/vol1.cbz",
                &[("range", "bytes=8-")],
                b"",
            ),
            ctx,
        )
        .await;
    assert_eq!(r.status(), 416);
    assert!(!r.headers().contains_key("x-accel-redirect"));
    assert_eq!(
        r.headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok()),
        Some("bytes */8")
    );
    assert_eq!(r.extensions().get::<Served>(), None);
}

#[tokio::test]
async fn propfind_depths_and_cached_listings() {
    let env = Env::new();
    let r = raw(
        &env,
        None,
        "PROPFIND",
        "/mokuro-reader/series",
        &[("depth", "1")],
        b"",
    )
    .await;
    assert_eq!(r.status(), 207);
    assert_eq!(
        info(&r),
        DavInfo {
            target: DavTarget::Library,
            rel: Some("series".into()),
            depth: Some(DavDepth::One)
        }
    );
    let listing = *r
        .extensions()
        .get::<ListingInfo>()
        .expect("a generated listing");
    assert!(!listing.gzip);
    assert_eq!(listing.raw_len, body_len(r).await);

    let r = raw(
        &env,
        None,
        "PROPFIND",
        "/mokuro-reader/",
        &[("accept-encoding", "gzip")],
        b"",
    )
    .await;
    assert_eq!(r.status(), 207);
    assert_eq!(info(&r).target, DavTarget::ReaderRoot);
    assert_eq!(info(&r).depth, Some(DavDepth::Infinity));
    let listing = *r.extensions().get::<ListingInfo>().unwrap();
    assert!(listing.gzip);
    let sent = body_len(r).await;
    assert!(
        sent > 0 && sent < listing.raw_len,
        "{sent} vs {}",
        listing.raw_len
    );

    let r = raw(
        &env,
        None,
        "PROPFIND",
        "/mokuro-reader/series/vol1.cbz",
        &[("depth", "0")],
        b"",
    )
    .await;
    assert_eq!(info(&r).depth, Some(DavDepth::Zero));
    assert!(r.extensions().get::<ListingInfo>().is_none());
}

#[tokio::test]
async fn every_method_and_status_carries_dav_info() {
    let env = Env::new();
    let r = raw(
        &env,
        Some("reader"),
        "PUT",
        "/mokuro-reader/positions.json",
        &[],
        b"{}",
    )
    .await;
    assert!(r.status().is_success(), "{}", r.status());
    assert_eq!(
        info(&r),
        DavInfo {
            target: DavTarget::Progress,
            rel: Some("positions.json".into()),
            depth: None
        }
    );
    for (method, path) in [
        ("MKCOL", "/mokuro-reader/newseries"),
        ("DELETE", "/mokuro-reader/missing.cbz"),
        ("OPTIONS", "/mokuro-reader/"),
        ("PROPPATCH", "/mokuro-reader/series/vol1.cbz"),
        ("LOCK", "/mokuro-reader/series/vol1.cbz"),
        ("BREW", "/mokuro-reader/series"),
        ("GET", "/mokuro-reader/series"),
        ("GET", "/inbox/x"),
    ] {
        let r = raw(&env, Some("uploader"), method, path, &[], b"").await;
        let i = info(&r);
        let want = bunko_dav::dav_target_of(path).0;
        assert_eq!(i.target, want, "{method} {path} -> {}", r.status());
    }
    let r = raw(&env, None, "GET", "/%FF", &[], b"").await;
    assert_eq!(r.status(), 400);
    assert_eq!(info(&r).target, DavTarget::Other);
}
