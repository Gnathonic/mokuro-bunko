//! The Depth:infinity PROPFIND cache (spec §10; `tests/unit/test_propfind_cache.py` for the
//! timer lifecycle).

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod common;

use std::time::Duration;

use bunko_core::StorageLayout;
use bunko_dav::{CacheConfig, Dav, DavConfig};
use common::*;

fn big_env(series: usize, per: usize) -> Env {
    let env = Env::new();
    for s in 0..series {
        let dir = env.lib(&format!("Series {s:03} Ω"));
        std::fs::create_dir_all(&dir).unwrap();
        for v in 0..per {
            std::fs::write(dir.join(format!("Vol {v:02}.cbz")), b"x").unwrap();
            std::fs::write(dir.join(format!("Vol {v:02}.mokuro")), b"{}").unwrap();
        }
    }
    env
}

#[tokio::test]
async fn a_large_listing_streams_identically_in_both_encodings() {
    let env = big_env(40, 30);
    // No Depth means infinity (RFC 4918 9.1): the same cached answer.
    let live = env
        .req(Some("reader"), "PROPFIND", "/mokuro-reader", &[], b"")
        .await;
    assert_eq!(live.code(), 207);
    let ident = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(ident.code(), 207);
    assert!(ident.body.len() > 500_000, "{}", ident.body.len());
    assert_eq!(
        ident.header("content-length").unwrap(),
        ident.body.len().to_string()
    );
    let gz = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity"), ("accept-encoding", "gzip")],
            b"",
        )
        .await;
    assert!(gz.body.len() < ident.body.len() / 5);
    assert_eq!(decode_gzip(&gz.body), ident.body);
    // Same set of responses as the live walk (the per-user ones move to the end).
    let mut a: Vec<&str> = live.text().leak().split("<D:response>").skip(1).collect();
    let mut b: Vec<&str> = ident.text().leak().split("<D:response>").skip(1).collect();
    a.iter_mut()
        .chain(b.iter_mut())
        .for_each(|s| *s = s.trim_end_matches("</D:multistatus>"));
    a.sort();
    b.sort();
    assert_eq!(a.len(), 7 + 40 * 61);
    assert_eq!(a, b);
}

/// Regression (review finding): a missing Depth or a differently spelled `infinity` used
/// to bypass the cache and build the whole listing in memory per request; 40 parallel
/// requests on a 30k-file library held ~1 GiB. They are now the cached answer.
#[tokio::test]
async fn implied_or_odd_case_infinity_is_served_from_the_cache() {
    let env = big_env(5, 4);
    let exact = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(exact.code(), 207);
    assert_eq!(env.dav.propfind_cache().usage().0, 1);
    for headers in [
        &[][..],
        &[("depth", "Infinity")][..],
        &[("depth", "INFINITY")][..],
    ] {
        let r = env
            .req(None, "PROPFIND", "/mokuro-reader/", headers, b"")
            .await;
        assert_eq!(r.code(), 207, "{headers:?}");
        assert_eq!(r.body, exact.body, "{headers:?}");
        // Still the one entry: the same key, not a new walk kept elsewhere.
        assert_eq!(env.dav.propfind_cache().usage().0, 1, "{headers:?}");
    }
    // A file is one response at any depth: answered live, never a cache entry that would
    // evict a listing.
    let file = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/Series%20000%20%CE%A9/Vol%2000.cbz",
            &[],
            b"",
        )
        .await;
    assert_eq!(file.code(), 207, "{}", file.text());
    assert_eq!(file.text().matches("<D:response>").count(), 1);
    assert_eq!(env.dav.propfind_cache().usage().0, 1);
    // A propname body cannot make the implied-infinity walk go live either.
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[],
            br#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:propname/></D:propfind>"#,
        )
        .await;
    assert_eq!(r.body, exact.body);
}

/// Depth: 1 of a big folder is generated compressed and streamed: complete, identical in
/// both encodings, and the gzip form is the small one.
#[tokio::test]
async fn depth_one_of_a_big_folder_streams_compressed() {
    let env = Env::new();
    let dir = env.lib("Flat");
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..3000 {
        std::fs::write(dir.join(format!("Volume {i:05}.cbz")), b"x").unwrap();
    }
    let ident = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/Flat",
            &[("depth", "1")],
            b"",
        )
        .await;
    assert_eq!(ident.code(), 207);
    assert_eq!(
        ident.header("content-length").unwrap(),
        ident.body.len().to_string()
    );
    let text = ident.text();
    assert!(text.starts_with("<?xml"));
    assert!(text.ends_with("</D:multistatus>"));
    assert_eq!(text.matches("<D:response>").count(), 3001);
    assert!(text.contains("Volume%2002999.cbz") || text.contains("Volume 02999.cbz"));
    let gz = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/Flat",
            &[("depth", "1"), ("accept-encoding", "gzip")],
            b"",
        )
        .await;
    assert_eq!(gz.code(), 207);
    assert_eq!(gz.header("content-encoding").unwrap(), "gzip");
    assert!(gz.body.len() < ident.body.len() / 5);
    assert_eq!(decode_gzip(&gz.body), ident.body);
    // Not cached: a live answer (locks and dead properties are per request).
    assert_eq!(env.dav.propfind_cache().usage().0, 0);
}

#[tokio::test]
async fn budget_bounds_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    std::fs::create_dir_all(base.join("library")).unwrap();
    for i in 0..200 {
        std::fs::write(base.join(format!("library/f{i}.cbz")), b"x").unwrap();
    }
    std::fs::create_dir_all(base.join("library/a")).unwrap();
    std::fs::create_dir_all(base.join("library/b")).unwrap();
    let cfg = DavConfig {
        propfind_cache: CacheConfig {
            budget_bytes: 6000,
            max_entries: 2,
            ..CacheConfig::default()
        },
        ..DavConfig::default()
    };
    let dav = Dav::new(&StorageLayout::new(&base), cfg).unwrap();
    let env = Env {
        dir,
        base,
        dav,
        hooks: Default::default(),
    };
    for p in ["/mokuro-reader/a", "/mokuro-reader/b", "/"] {
        assert_eq!(
            env.req(None, "PROPFIND", p, &[("depth", "infinity")], b"")
                .await
                .code(),
            207
        );
    }
    let (n, bytes) = env.dav.propfind_cache().usage();
    assert!(n <= 2 && bytes <= 6000, "{n} {bytes}");
    // A listing bigger than the budget is served but not kept.
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 207);
    assert!(r.text().contains("f199.cbz"));
}

#[tokio::test]
async fn stale_entries_are_served_while_one_refresh_runs() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    std::fs::create_dir_all(base.join("library")).unwrap();
    let cfg = DavConfig {
        propfind_cache: CacheConfig {
            ttl: Duration::from_millis(50),
            ..CacheConfig::default()
        },
        ..DavConfig::default()
    };
    let dav = Dav::new(&StorageLayout::new(&base), cfg).unwrap();
    let env = Env {
        dir,
        base,
        dav,
        hooks: Default::default(),
    };
    let first = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert!(!first.text().contains("outside.cbz"));
    // A change the DAV layer never saw (another process, the watcher's job).
    std::fs::write(env.lib("outside.cbz"), b"x").unwrap();
    let fresh = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(fresh.body, first.body, "within ttl: cached");
    tokio::time::sleep(Duration::from_millis(80)).await;
    let stale = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(stale.body, first.body, "stale: served, refresh started");
    let mut seen = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let r = env
            .req(
                None,
                "PROPFIND",
                "/mokuro-reader",
                &[("depth", "infinity")],
                b"",
            )
            .await;
        if r.text().contains("outside.cbz") {
            seen = true;
            break;
        }
    }
    assert!(seen, "the background refresh never landed");
}

#[tokio::test]
async fn debounced_refresh_and_stop() {
    let env = Env::new();
    let cache = env.dav.propfind_cache().clone();
    let first = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    std::fs::write(env.lib("watched.cbz"), b"x").unwrap();
    cache.schedule_refresh(Duration::from_millis(30));
    cache.schedule_refresh(Duration::from_millis(30));
    let mut seen = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let r = env
            .req(
                None,
                "PROPFIND",
                "/mokuro-reader",
                &[("depth", "infinity")],
                b"",
            )
            .await;
        if r.text().contains("watched.cbz") {
            seen = true;
            break;
        }
    }
    assert!(seen);
    assert_ne!(first.body.len(), 0);
    // After stop(), scheduling is a no-op.
    cache.stop();
    std::fs::write(env.lib("late.cbz"), b"x").unwrap();
    cache.schedule_refresh(Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert!(!r.text().contains("late.cbz"));
}

#[tokio::test]
async fn per_user_paths_bypass_the_cache() {
    let env = Env::new();
    let r = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader/volume-data.json",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 207);
    assert!(r.text().contains("volume-data.json"));
    assert_eq!(env.dav.propfind_cache().usage().0, 0);
    // A missing path is a 404 and is not cached.
    assert_eq!(
        env.req(
            None,
            "PROPFIND",
            "/mokuro-reader/nope",
            &[("depth", "infinity")],
            b""
        )
        .await
        .code(),
        404
    );
    assert_eq!(env.dav.propfind_cache().usage().0, 0);
}

/// A correction layer body.
fn correction(text: &str) -> Vec<u8> {
    format!(r#"{{"pages": [{{"blocks": [{{"lines": ["{text}"]}}]}}]}}"#).into_bytes()
}

/// The hrefs of `<Vol>.edit*.mokuro` files in a multistatus.
fn edit_hrefs(xml: &str) -> Vec<String> {
    let mut v: Vec<String> = xml
        .split("<D:href>")
        .skip(1)
        .filter_map(|s| s.split("</D:href>").next())
        .filter(|h| h.contains(".edit"))
        .map(str::to_string)
        .collect();
    v.sort();
    v
}

async fn listing(env: &Env, ctx: bunko_dav::DavContext, path: &str, depth: &str) -> String {
    let r = env
        .send_ctx(ctx, request("PROPFIND", path, &[("depth", depth)], b""))
        .await;
    assert_eq!(r.code(), 207, "{path} depth {depth}");
    r.text()
}

/// D27: the shared `Depth: infinity` listing is the anonymous one, cached once; each
/// caller's own corrections are appended to it per request, at every collection.
#[tokio::test]
async fn each_caller_sees_its_own_corrections_in_the_cached_listing() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("series/deeper")).unwrap();
    std::fs::write(env.lib("series/deeper/v.cbz"), b"x").unwrap();
    for (path, user) in [
        ("/mokuro-reader/series/vol1.edit.mokuro", "alice"),
        ("/mokuro-reader/series/deeper/v.edit-2.mokuro", "alice"),
        ("/mokuro-reader/manga1.edit.mokuro", "alice"),
        ("/mokuro-reader/series/vol1.edit.mokuro", "bob"),
    ] {
        let r = env
            .req(Some(user), "PUT", path, &[], &correction(user))
            .await;
        assert_eq!(r.code(), 201, "{user} {path}: {}", r.text());
    }
    let alice_all = [
        "/mokuro-reader/manga1.edit.mokuro",
        "/mokuro-reader/series/deeper/v.edit-2.mokuro",
        "/mokuro-reader/series/vol1.edit.mokuro",
    ];
    for _ in 0..2 {
        let got =
            edit_hrefs(&listing(&env, env.ctx(Some("alice")), "/mokuro-reader", "infinity").await);
        assert_eq!(got, alice_all);
        let got =
            edit_hrefs(&listing(&env, env.ctx(Some("bob")), "/mokuro-reader/", "infinity").await);
        assert_eq!(got, ["/mokuro-reader/series/vol1.edit.mokuro"]);
        let got = edit_hrefs(&listing(&env, env.ctx(None), "/mokuro-reader", "infinity").await);
        assert!(got.is_empty(), "{got:?}");
        let got = edit_hrefs(&listing(&env, env.ctx(Some("alice")), "/", "infinity").await);
        assert_eq!(got, alice_all);
        let got = edit_hrefs(
            &listing(
                &env,
                env.ctx(Some("alice")),
                "/mokuro-reader/series",
                "infinity",
            )
            .await,
        );
        assert_eq!(got, &alice_all[1..]);
    }
    // One cached entry per key, shared by every caller.
    assert_eq!(env.dav.propfind_cache().usage().0, 3);

    // Depth 1 agrees with the infinity answer, folder by folder.
    let got =
        edit_hrefs(&listing(&env, env.ctx(Some("alice")), "/mokuro-reader/series", "1").await);
    assert_eq!(got, ["/mokuro-reader/series/vol1.edit.mokuro"]);
    let got = edit_hrefs(&listing(&env, env.ctx(Some("alice")), "/mokuro-reader", "1").await);
    assert_eq!(got, ["/mokuro-reader/manga1.edit.mokuro"]);

    // An overlay whose archive went (outside DAV) is listed nowhere.
    std::fs::remove_file(env.lib("series/deeper/v.cbz")).unwrap();
    env.dav.propfind_cache().invalidate();
    let got =
        edit_hrefs(&listing(&env, env.ctx(Some("alice")), "/mokuro-reader", "infinity").await);
    assert_eq!(
        got,
        [
            "/mokuro-reader/manga1.edit.mokuro",
            "/mokuro-reader/series/vol1.edit.mokuro"
        ]
    );
    let got = edit_hrefs(
        &listing(
            &env,
            env.ctx(Some("alice")),
            "/mokuro-reader/series/deeper",
            "1",
        )
        .await,
    );
    assert!(got.is_empty(), "{got:?}");
}

/// The cloud-root owner's correction is the physical file: hidden from the shared
/// listing, kept with the cached entry for the owner alone, and gone from it once
/// deleted (a physical change drops the entry).
#[tokio::test]
async fn the_owners_physical_correction_is_listed_to_the_owner_only() {
    let env = Env::new();
    let owner = || {
        let mut ctx = env.ctx(Some("alice"));
        ctx.owns_cloud_root = true;
        ctx
    };
    let path = "/mokuro-reader/series/vol1.edit.mokuro";
    let r = env
        .send_ctx(owner(), request("PUT", path, &[], &correction("owner")))
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    assert!(env.lib("series/vol1.edit.mokuro").exists());
    for _ in 0..2 {
        let got = edit_hrefs(&listing(&env, owner(), "/mokuro-reader", "infinity").await);
        assert_eq!(got, [path]);
        for ctx in [env.ctx(Some("alice")), env.ctx(Some("bob")), env.ctx(None)] {
            let got = edit_hrefs(&listing(&env, ctx, "/mokuro-reader", "infinity").await);
            assert!(got.is_empty(), "{got:?}");
        }
    }
    let got = edit_hrefs(&listing(&env, owner(), "/mokuro-reader/series", "1").await);
    assert_eq!(got, [path]);
    let got = edit_hrefs(&listing(&env, env.ctx(Some("bob")), "/mokuro-reader/series", "1").await);
    assert!(got.is_empty(), "{got:?}");

    let r = env
        .send_ctx(owner(), request("DELETE", path, &[], b""))
        .await;
    assert_eq!(r.code(), 204);
    assert!(!env.lib("series/vol1.edit.mokuro").exists());
    let got = edit_hrefs(&listing(&env, owner(), "/mokuro-reader", "infinity").await);
    assert!(got.is_empty(), "{got:?}");
}
