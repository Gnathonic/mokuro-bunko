//! Ported from `tests/integration/test_webdav_ops.py` (the parts below the auth gate), plus
//! the method/status matrix of spec §8.3.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

const PROPFIND_ALLPROP: &[u8] = b"";

#[tokio::test]
async fn propfind_root_lists_only_the_reader_root() {
    let env = Env::new();
    let r = env
        .req(None, "PROPFIND", "/", &[("depth", "1")], PROPFIND_ALLPROP)
        .await;
    assert_eq!(r.code(), 207);
    assert_eq!(
        r.header("content-type"),
        Some("application/xml; charset=utf-8")
    );
    assert_eq!(
        r.header("content-length").unwrap(),
        r.body.len().to_string()
    );
    let t = r.text();
    assert!(t.starts_with(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\n<D:multistatus xmlns:D=\"DAV:\">"
    ));
    assert!(t.contains("<D:href>/</D:href>"));
    assert!(t.contains("<D:href>/mokuro-reader/</D:href>"));
    assert!(t.contains("<D:displayname>mokuro-bunko</D:displayname>"));
    assert!(!t.contains("inbox"));
    // Virtual folders have no ETag: a 404 propstat.
    assert!(t.contains("<D:propstat><D:prop><D:getetag /></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>"));
}

/// A body nested ~150k deep (just under the 1 MiB body limit) is a 400, and the server is
/// still there for the next request. It used to overflow the stack and abort the process.
#[tokio::test]
async fn deeply_nested_bodies_are_400_not_a_crash() {
    let env = Env::new();
    let nest = |depth: usize| format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth));
    let propfind = format!(
        "<D:propfind xmlns:D=\"DAV:\"><D:prop>{}</D:prop></D:propfind>",
        nest(149_000)
    );
    assert!(propfind.len() < 1024 * 1024);
    let r = env
        .req(
            None,
            "PROPFIND",
            "/",
            &[("depth", "0")],
            propfind.as_bytes(),
        )
        .await;
    assert_eq!(r.code(), 400);

    let proppatch = format!(
        "<D:propertyupdate xmlns:D=\"DAV:\" xmlns:Z=\"urn:z\"><D:set><D:prop><Z:a>{}</Z:a></D:prop></D:set></D:propertyupdate>",
        nest(149_000)
    );
    let path = "/mokuro-reader/manga1.cbz";
    let r = env
        .req(Some("admin"), "PROPPATCH", path, &[], proppatch.as_bytes())
        .await;
    assert_eq!(r.code(), 400);

    let lock = format!(
        "<D:lockinfo xmlns:D=\"DAV:\"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>{}</D:owner></D:lockinfo>",
        nest(149_000)
    );
    let r = env
        .req(Some("admin"), "LOCK", path, &[], lock.as_bytes())
        .await;
    assert_eq!(r.code(), 400);

    // A dead property nested deeply, but within the limit, is still kept.
    let ok = format!(
        "<D:propertyupdate xmlns:D=\"DAV:\" xmlns:Z=\"urn:z\"><D:set><D:prop><Z:a>{}</Z:a></D:prop></D:set></D:propertyupdate>",
        nest(200)
    );
    let r = env
        .req(Some("admin"), "PROPPATCH", path, &[], ok.as_bytes())
        .await;
    assert_eq!(r.code(), 207);
    assert!(r.text().contains("HTTP/1.1 200 OK"), "{}", r.text());
    let named =
        br#"<D:propfind xmlns:D="DAV:" xmlns:Z="urn:z"><D:prop><Z:a/></D:prop></D:propfind>"#;
    let r = env
        .req(Some("admin"), "PROPFIND", path, &[("depth", "0")], named)
        .await;
    assert_eq!(r.code(), 207);
    assert_eq!(
        r.text().matches("<a xmlns=\"\"").count(),
        200,
        "{}",
        r.text()
    );

    let r = env
        .req(None, "PROPFIND", "/", &[("depth", "0")], PROPFIND_ALLPROP)
        .await;
    assert_eq!(r.code(), 207);

    // The same LOCK body nested a few levels is granted: the 400 above is the depth.
    let lock = format!(
        "<D:lockinfo xmlns:D=\"DAV:\"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner>{}</D:owner></D:lockinfo>",
        nest(3)
    );
    let r = env
        .req(Some("admin"), "LOCK", path, &[], lock.as_bytes())
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
}

#[tokio::test]
async fn propfind_reader_root_shows_own_progress_first() {
    let env = Env::new();
    let r = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "1")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 207);
    let t = r.text();
    let vd = t.find("/mokuro-reader/volume-data.json").unwrap();
    let m1 = t.find("/mokuro-reader/manga1.cbz").unwrap();
    assert!(vd < m1);
    assert!(t.contains("<D:href>/mokuro-reader/series/</D:href>"));
    // Anonymous: no per-user files.
    let anon = env
        .req(None, "PROPFIND", "/mokuro-reader", &[("depth", "1")], b"")
        .await;
    assert!(!anon.text().contains("volume-data.json"));
    assert!(anon.text().contains("manga2.cbz"));
}

#[tokio::test]
async fn propfind_file_depth0_matches_the_python_shape() {
    let env = Env::new();
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/series/vol1.cbz",
            &[("depth", "0")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 207);
    let t = r.text();
    assert!(t.contains("<D:resourcetype />"));
    assert!(t.contains("<D:getcontentlength>8</D:getcontentlength>"));
    assert!(t.contains("<D:getcontenttype>application/vnd.comicbook+zip</D:getcontenttype>"));
    assert!(t.contains("<D:displayname>vol1.cbz</D:displayname>"));
    assert!(t.contains("-8</D:getetag>"));
    // A missing property would get a 404 propstat (the etag may contain "404" by chance).
    assert!(!t.contains("404 Not Found"));
}

#[tokio::test]
async fn stray_per_user_file_in_the_library_is_not_listed() {
    let env = Env::new();
    std::fs::write(env.lib("goals.json"), b"{}").unwrap();
    for user in [None, Some("reader")] {
        let r = env
            .req(user, "PROPFIND", "/mokuro-reader", &[("depth", "1")], b"")
            .await;
        assert_eq!(r.code(), 207);
        assert!(r.text().contains("manga1.cbz"));
        assert!(!r.text().contains("goals.json"));
    }
}

#[tokio::test]
async fn propfind_nested_inbox_and_bad_depth() {
    let env = Env::new();
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/series",
            &[("depth", "1")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 207);
    assert!(r.text().contains("vol1.cbz"));
    assert_eq!(
        env.req(None, "PROPFIND", "/inbox", &[("depth", "1")], b"")
            .await
            .code(),
        404
    );
    assert_eq!(
        env.req(None, "PROPFIND", "/nope", &[("depth", "0")], b"")
            .await
            .code(),
        404
    );
    assert_eq!(
        env.req(None, "PROPFIND", "/mokuro-reader", &[("depth", "2")], b"")
            .await
            .code(),
        400
    );
    assert_eq!(
        env.req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "1")],
            b"<not xml"
        )
        .await
        .code(),
        400
    );
    assert_eq!(
        env.req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "1")],
            b"<a xmlns='DAV:'/>"
        )
        .await
        .code(),
        400
    );
    // Per-user names: 404 for anonymous and for a user with no file yet.
    assert_eq!(
        env.req(
            None,
            "PROPFIND",
            "/mokuro-reader/volume-data.json",
            &[("depth", "0")],
            b""
        )
        .await
        .code(),
        404
    );
    assert_eq!(
        env.req(
            Some("uploader"),
            "PROPFIND",
            "/mokuro-reader/volume-data.json",
            &[("depth", "0")],
            b""
        )
        .await
        .code(),
        404
    );
}

#[tokio::test]
async fn propfind_named_and_propname() {
    let env = Env::new();
    let body = br#"<?xml version="1.0"?><D:propfind xmlns:D="DAV:"><D:prop><D:getetag/><D:getcontentlength/><x:foo xmlns:x="urn:x"/></D:prop></D:propfind>"#;
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/series",
            &[("depth", "1")],
            body,
        )
        .await;
    let t = r.text();
    assert_eq!(r.code(), 207);
    // Folder: getcontentlength unknown -> 404; foreign property -> 404.
    assert!(t.contains("<ns0:foo xmlns:ns0=\"urn:x\" />"));
    assert!(t.contains("<D:getcontentlength />"));
    assert!(t.contains("<D:getcontentlength>8</D:getcontentlength>"));
    let names = br#"<D:propfind xmlns:D="DAV:"><D:propname/></D:propfind>"#;
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/series/vol1.cbz",
            &[("depth", "0")],
            names,
        )
        .await;
    assert!(r.text().contains("<D:getetag />"));
    assert!(r.text().contains("HTTP/1.1 200 OK"));
    assert!(!r.text().contains("404 Not Found"));
    // allprop + propname together: 400.
    let both = br#"<D:propfind xmlns:D="DAV:"><D:allprop/><D:propname/></D:propfind>"#;
    assert_eq!(
        env.req(None, "PROPFIND", "/", &[("depth", "0")], both)
            .await
            .code(),
        400
    );
}

#[tokio::test]
async fn depth_infinity_is_cached_and_injects_progress() {
    let env = Env::new();
    let anon = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(anon.code(), 207);
    assert_eq!(anon.header("vary"), Some("Accept-Encoding"));
    assert!(!anon.text().contains("volume-data.json"));
    assert!(anon.text().contains("/mokuro-reader/series/vol1.cbz"));
    let me = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(me.code(), 207);
    assert!(me.text().contains("volume-data.json"));
    assert!(me.text().ends_with("</D:response></D:multistatus>"));
    let root = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert!(root.text().contains("volume-data.json"));
    assert_eq!(env.dav.propfind_cache().usage().0, 2);
    // gzip and identity carry the same document.
    let gz = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity"), ("accept-encoding", "gzip, br")],
            b"",
        )
        .await;
    assert_eq!(gz.header("content-encoding"), Some("gzip"));
    assert_eq!(
        gz.header("content-length").unwrap(),
        gz.body.len().to_string()
    );
    assert_eq!(decode_gzip(&gz.body), me.body);
    // Always allprop, whatever the body asks.
    let named = br#"<D:propfind xmlns:D="DAV:"><D:prop><D:getetag/></D:prop></D:propfind>"#;
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            named,
        )
        .await;
    assert_eq!(r.body, anon.body);
}

#[tokio::test]
async fn cache_is_invalidated_after_a_write_commits() {
    let env = Env::new();
    let first = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert!(!first.text().contains("fresh.cbz"));
    let r = env
        .req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/series/fresh.cbz",
            &[],
            &cbz_bytes(2),
        )
        .await;
    assert_eq!(r.code(), 201);
    let after = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert!(after.text().contains("/mokuro-reader/series/fresh.cbz"));
    let r = env
        .req(Some("admin"), "DELETE", "/mokuro-reader/series", &[], b"")
        .await;
    assert_eq!(r.code(), 204);
    let after = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert!(!after.text().contains("/mokuro-reader/series/"));
}

#[tokio::test]
async fn get_files_and_errors() {
    let env = Env::new();
    let r = env
        .req(None, "GET", "/mokuro-reader/manga1.cbz", &[], b"")
        .await;
    assert_eq!(r.code(), 200);
    assert_eq!(r.body, b"fake cbz content 1");
    assert_eq!(r.header("content-length"), Some("18"));
    assert_eq!(
        r.header("content-type"),
        Some("application/vnd.comicbook+zip")
    );
    assert_eq!(r.header("accept-ranges"), Some("bytes"));
    assert!(r.header("etag").unwrap().starts_with('"'));
    assert!(r.header("last-modified").unwrap().ends_with(" GMT"));
    assert!(r.header("x-accel-redirect").is_none());
    assert_eq!(
        env.req(None, "GET", "/mokuro-reader/series/vol1.cbz", &[], b"")
            .await
            .body,
        b"volume 1"
    );
    assert_eq!(
        env.req(None, "GET", "/mokuro-reader/nonexistent.cbz", &[], b"")
            .await
            .code(),
        404
    );
    assert_eq!(
        env.req(None, "GET", "/mokuro-reader/", &[], b"")
            .await
            .code(),
        403
    );
    assert_eq!(env.req(None, "GET", "/", &[], b"").await.code(), 403);
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/manga1.cbz",
            &[("depth", "1")],
            b""
        )
        .await
        .code(),
        400
    );
    assert_eq!(
        env.req(None, "GET", "/mokuro-reader/manga1.cbz", &[], b"body")
            .await
            .code(),
        415
    );
    assert_eq!(
        env.req(None, "GET", "/mokuro-reader/%FF.cbz", &[], b"")
            .await
            .code(),
        400
    );
    let head = env
        .req(None, "HEAD", "/mokuro-reader/manga1.cbz", &[], b"")
        .await;
    assert_eq!(head.code(), 200);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length"), Some("18"));
}

#[tokio::test]
async fn progress_files_are_per_user() {
    let env = Env::new();
    std::fs::write(
        env.base.join("users/uploader/volume-data.json"),
        b"uploader progress",
    )
    .unwrap();
    assert_eq!(
        env.req(
            Some("reader"),
            "GET",
            "/mokuro-reader/volume-data.json",
            &[],
            b""
        )
        .await
        .body,
        b"reader progress"
    );
    assert_eq!(
        env.req(
            Some("uploader"),
            "GET",
            "/mokuro-reader/volume-data.json",
            &[],
            b""
        )
        .await
        .body,
        b"uploader progress"
    );
    assert_eq!(
        env.req(None, "GET", "/mokuro-reader/volume-data.json", &[], b"")
            .await
            .code(),
        404
    );
    // A first save creates users/<name>/.
    let r = env
        .req(
            Some("newbie"),
            "PUT",
            "/mokuro-reader/profiles.json",
            &[],
            b"profiles data",
        )
        .await;
    assert_eq!(r.code(), 201);
    assert_eq!(r.header("x-mokuro-upload"), Some("stored"));
    assert_eq!(
        std::fs::read(env.base.join("users/newbie/profiles.json")).unwrap(),
        b"profiles data"
    );
    let r = env
        .req(
            Some("reader"),
            "PUT",
            "/mokuro-reader/volume-data.json",
            &[],
            b"new progress data",
        )
        .await;
    assert_eq!(r.code(), 204);
    assert_eq!(
        std::fs::read(env.base.join("users/reader/volume-data.json")).unwrap(),
        b"new progress data"
    );
    assert!(
        env.hooks
            .has("audit edit progress /mokuro-reader/volume-data.json")
    );
    let r = env
        .req(
            Some("reader"),
            "DELETE",
            "/mokuro-reader/volume-data.json",
            &[],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    assert!(!env.base.join("users/reader/volume-data.json").exists());
}

#[tokio::test]
async fn put_library_file_records_ownership_and_queues() {
    let env = Env::new();
    *env.hooks.follow_up.lock() = Some(bunko_dav::PutFollowUp {
        manifest: "/catalog/api/manifest?series=.&volume=new_manga".into(),
        recheck_after: 42,
    });
    let body = cbz_bytes(3);
    let r = env
        .req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/new_manga.cbz",
            &[],
            &body,
        )
        .await;
    assert_eq!(r.code(), 201);
    assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(r.header("x-mokuro-upload"), Some("verified"));
    assert_eq!(
        r.header("x-mokuro-size"),
        Some(body.len().to_string().as_str())
    );
    assert_eq!(r.header("x-mokuro-put"), Some("verified"));
    assert_eq!(
        r.header("x-mokuro-manifest"),
        Some("/catalog/api/manifest?series=.&volume=new_manga")
    );
    assert_eq!(r.header("x-mokuro-recheck-after"), Some("42"));
    assert_eq!(std::fs::read(env.lib("new_manga.cbz")).unwrap(), body);
    let calls = env.hooks.calls();
    let rec = calls
        .iter()
        .position(|c| c == "record_volume_upload new_manga.cbz uploader false")
        .unwrap();
    let arrived = calls
        .iter()
        .position(|c| c == "archive_arrived new_manga.cbz")
        .unwrap();
    let follow = calls
        .iter()
        .position(|c| c == "put_follow_up . new_manga")
        .unwrap();
    assert!(rec < arrived && arrived < follow);
    assert!(
        env.hooks
            .has("audit upload library /mokuro-reader/new_manga.cbz")
    );
    let ev = env
        .hooks
        .audits()
        .into_iter()
        .find(|a| a.action == "upload")
        .unwrap();
    assert_eq!(
        ev.details,
        Some(serde_json::json!({ "existed_before": false }))
    );
    assert_eq!(ev.actor.as_deref(), Some("uploader"));
}

#[tokio::test]
async fn put_preconditions() {
    let env = Env::new();
    // New series folder without MKCOL first: 409.
    assert_eq!(
        env.req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/NewSeries/x.cbz",
            &[],
            &cbz_bytes(1)
        )
        .await
        .code(),
        409
    );
    // Onto a collection: 405.
    assert_eq!(
        env.req(Some("admin"), "PUT", "/mokuro-reader/series", &[], b"x")
            .await
            .code(),
        405
    );
    // Traversal never escapes.
    let outside = env.base.parent().unwrap().join("escape-put.txt");
    let r = env
        .req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/../../escape-put.txt",
            &[],
            b"escape",
        )
        .await;
    assert!([400, 403, 404, 409].contains(&r.code()), "{}", r.code());
    assert!(!outside.exists());
    assert_eq!(
        env.req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/x.txt",
            &[("content-encoding", "gzip")],
            b"x"
        )
        .await
        .code(),
        501
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/x.txt",
            &[("content-range", "bytes 0-0/1")],
            b"x"
        )
        .await
        .code(),
        400
    );
    // If-None-Match: * on an existing file.
    let r = env
        .req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/manga1.cbz",
            &[("if-none-match", "*")],
            &cbz_bytes(1),
        )
        .await;
    assert_eq!(r.code(), 412);
    assert_eq!(r.json()["reason"], "server-error");
    // If-Match on a missing file: 412 (RFC 9110).
    assert_eq!(
        env.req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/new.txt",
            &[("if-match", "\"x\"")],
            b"x"
        )
        .await
        .code(),
        412
    );
}

#[tokio::test]
async fn put_overwrite_returns_the_new_etag() {
    let env = Env::new();
    std::fs::write(env.lib("notes.txt"), b"12345").unwrap();
    let r = env
        .req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/notes.txt",
            &[],
            b"hello world",
        )
        .await;
    assert_eq!(r.code(), 204);
    let etag = r.header("etag").unwrap().to_string();
    assert!(etag.ends_with("-11\""), "{etag}");
    let get = env
        .req(None, "GET", "/mokuro-reader/notes.txt", &[], b"")
        .await;
    assert_eq!(get.header("etag").unwrap(), etag);
}

#[tokio::test]
async fn conditional_get() {
    let env = Env::new();
    let r = env
        .req(None, "GET", "/mokuro-reader/manga1.cbz", &[], b"")
        .await;
    let etag = r.header("etag").unwrap().to_string();
    let lm = r.header("last-modified").unwrap().to_string();
    let r304 = env
        .req(
            None,
            "GET",
            "/mokuro-reader/manga1.cbz",
            &[("if-none-match", &etag)],
            b"",
        )
        .await;
    assert_eq!(r304.code(), 304);
    assert!(r304.body.is_empty());
    assert_eq!(r304.header("etag"), Some(etag.as_str()));
    // The equal date is "not modified" (0.5.2 answered 200).
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/manga1.cbz",
            &[("if-modified-since", &lm)],
            b""
        )
        .await
        .code(),
        304
    );
    // Both validators, as browsers send them: 304.
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/manga1.cbz",
            &[("if-none-match", &etag), ("if-modified-since", &lm)],
            b""
        )
        .await
        .code(),
        304
    );
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/manga1.cbz",
            &[("if-none-match", "\"other\""), ("if-modified-since", &lm)],
            b""
        )
        .await
        .code(),
        200
    );
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/manga1.cbz",
            &[("if-match", "\"other\"")],
            b""
        )
        .await
        .code(),
        412
    );
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/manga1.cbz",
            &[("if-modified-since", "Thu, 01 Jan 1998 00:00:00 GMT")],
            b""
        )
        .await
        .code(),
        200
    );
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/manga1.cbz",
            &[("if-unmodified-since", "Thu, 01 Jan 1998 00:00:00 GMT")],
            b""
        )
        .await
        .code(),
        412
    );
}

#[tokio::test]
async fn ranges() {
    let env = Env::new();
    std::fs::write(env.lib("ten.bin"), b"0123456789").unwrap();
    let r = env
        .req(
            None,
            "GET",
            "/mokuro-reader/ten.bin",
            &[("range", "bytes=2-4")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 206);
    assert_eq!(r.body, b"234");
    assert_eq!(r.header("content-range"), Some("bytes 2-4/10"));
    assert_eq!(r.header("content-length"), Some("3"));
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/ten.bin",
            &[("range", "bytes=-3")],
            b""
        )
        .await
        .body,
        b"789"
    );
    assert_eq!(
        env.req(
            None,
            "GET",
            "/mokuro-reader/ten.bin",
            &[("range", "bytes=7-")],
            b""
        )
        .await
        .body,
        b"789"
    );
    // Several ranges: one spanning range (a superset).
    let multi = env
        .req(
            None,
            "GET",
            "/mokuro-reader/ten.bin",
            &[("range", "bytes=0-0,5-9")],
            b"",
        )
        .await;
    assert_eq!(multi.header("content-range"), Some("bytes 0-9/10"));
    let bad = env
        .req(
            None,
            "GET",
            "/mokuro-reader/ten.bin",
            &[("range", "bytes=10-")],
            b"",
        )
        .await;
    assert_eq!(bad.code(), 416);
    assert_eq!(bad.header("content-range"), Some("bytes */10"));
    // If-Range mismatch: the whole file.
    let r = env
        .req(
            None,
            "GET",
            "/mokuro-reader/ten.bin",
            &[("range", "bytes=2-4"), ("if-range", "\"nope\"")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 200);
    assert_eq!(r.body.len(), 10);
    let etag = r.header("etag").unwrap().to_string();
    let r = env
        .req(
            None,
            "GET",
            "/mokuro-reader/ten.bin",
            &[("range", "bytes=2-4"), ("if-range", &etag)],
            b"",
        )
        .await;
    assert_eq!(r.code(), 206);
}

#[tokio::test]
async fn nginx_accel_offload() {
    let env = Env::new();
    let mut ctx = env.ctx(None);
    ctx.nginx_accel = true;
    let r = env
        .send_ctx(
            ctx.clone(),
            request("GET", "/mokuro-reader/series/vol1.cbz", &[], b""),
        )
        .await;
    assert_eq!(r.code(), 200);
    assert!(r.body.is_empty());
    assert_eq!(r.header("content-length"), Some("0"));
    assert_eq!(
        r.header("x-accel-redirect"),
        Some("/internal-library/series/vol1.cbz")
    );
    assert!(r.header("accept-ranges").is_none());
    // Ranges are nginx's.
    let r = env
        .send_ctx(
            ctx.clone(),
            request(
                "GET",
                "/mokuro-reader/series/vol1.cbz",
                &[("range", "bytes=0-1")],
                b"",
            ),
        )
        .await;
    assert_eq!(r.code(), 200);
    std::fs::create_dir_all(env.lib("Series Ω")).unwrap();
    std::fs::write(env.lib("Series Ω/Vol #1.cbz"), b"x").unwrap();
    let r = env
        .send_ctx(
            ctx.clone(),
            request(
                "GET",
                "/mokuro-reader/Series%20%CE%A9/Vol%20%231.cbz",
                &[],
                b"",
            ),
        )
        .await;
    assert_eq!(
        r.header("x-accel-redirect"),
        Some("/internal-library/Series%20%CE%A9/Vol%20%231.cbz")
    );
    // Per-user files stream normally.
    let mut me = env.ctx(Some("reader"));
    me.nginx_accel = true;
    let r = env
        .send_ctx(
            me,
            request("GET", "/mokuro-reader/volume-data.json", &[], b""),
        )
        .await;
    assert_eq!(r.body, b"reader progress");
    assert!(r.header("x-accel-redirect").is_none());
}

#[tokio::test]
async fn options_answers() {
    let env = Env::new();
    for (path, allow) in [
        (
            "/",
            "OPTIONS, HEAD, GET, PROPFIND, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK",
        ),
        (
            "/mokuro-reader/",
            "OPTIONS, HEAD, GET, PROPFIND, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK",
        ),
        (
            "/mokuro-reader/manga1.cbz",
            "OPTIONS, HEAD, GET, PROPFIND, PUT, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK",
        ),
        ("/mokuro-reader/series/nope.cbz", "OPTIONS, PUT, MKCOL"),
        ("/inbox", "OPTIONS, PUT, MKCOL"),
    ] {
        let r = env.req(None, "OPTIONS", path, &[], b"").await;
        assert_eq!(r.code(), 200, "{path}");
        assert_eq!(r.header("allow"), Some(allow), "{path}");
        assert_eq!(r.header("dav"), Some("1,2"));
        assert_eq!(r.header("ms-author-via"), Some("DAV"));
        assert_eq!(r.header("x-mokuro-put"), Some("verified"));
        assert_eq!(r.header("content-length"), Some("0"));
    }
    assert_eq!(
        env.req(None, "OPTIONS", "/mokuro-reader/manga1.cbz", &[], b"")
            .await
            .header("accept-ranges"),
        Some("bytes")
    );
    assert_eq!(
        env.req(None, "OPTIONS", "/foo/bar", &[], b"").await.code(),
        404
    );
    assert_eq!(env.req(None, "OPTIONS", "*", &[], b"").await.code(), 200);
}

/// Collections advertise what the account may write there (plan X9), so a client that
/// reads Allow does not take a writable account for read-only.
#[tokio::test]
async fn options_allow_follows_the_account() {
    const BASE: &str = "OPTIONS, HEAD, GET, PROPFIND, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
    const WRITE: &str =
        "OPTIONS, HEAD, GET, PROPFIND, PUT, MKCOL, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
    const PUT: &str =
        "OPTIONS, HEAD, GET, PROPFIND, PUT, DELETE, COPY, MOVE, PROPPATCH, LOCK, UNLOCK";
    const FILE: &str = PUT;
    const UNMAPPED: &str = "OPTIONS, PUT, MKCOL";

    let env = Env::new();
    for user in ["reader", "uploader"] {
        for path in ["/mokuro-reader/history", "/mokuro-reader/history/dev"] {
            let r = env.req(Some(user), "MKCOL", path, &[], b"").await;
            assert_eq!(r.code(), 201, "{user} MKCOL {path}");
        }
    }
    let hooks: std::sync::Arc<dyn bunko_dav::DavHooks> = env.hooks.clone();
    let processor =
        || bunko_dav::DavContext::user("proc", bunko_core::Role::Processor, hooks.clone());

    let rows: &[(Option<&str>, &str, &str)] = &[
        // AddFiles: every collection at or under the reader root.
        (Some("uploader"), "/mokuro-reader/", WRITE),
        (Some("uploader"), "/mokuro-reader/series/", WRITE),
        (Some("uploader"), "/mokuro-reader/history/dev", WRITE),
        (Some("editor"), "/mokuro-reader/series", WRITE),
        (Some("admin"), "/mokuro-reader/series/", WRITE),
        // WriteProgress alone: its own tree, and PUT (personal corrections) in the library.
        (Some("reader"), "/mokuro-reader/", WRITE),
        (Some("reader"), "/mokuro-reader/history", WRITE),
        (Some("reader"), "/mokuro-reader/history/dev/", WRITE),
        (Some("reader"), "/mokuro-reader/series/", PUT),
        // Outside the reader root nothing changes, whoever asks.
        (Some("uploader"), "/", BASE),
        (Some("reader"), "/", BASE),
        // Files and unmapped paths are unchanged.
        (Some("reader"), "/mokuro-reader/manga1.cbz", FILE),
        (Some("uploader"), "/mokuro-reader/series/vol1.cbz", FILE),
        (Some("reader"), "/mokuro-reader/series/nope.cbz", UNMAPPED),
        (Some("uploader"), "/mokuro-reader/series/nope.cbz", UNMAPPED),
        // Anonymous: the base everywhere.
        (None, "/mokuro-reader/", BASE),
        (None, "/mokuro-reader/series/", BASE),
    ];
    for (user, path, allow) in rows {
        let r = env.req(*user, "OPTIONS", path, &[], b"").await;
        assert_eq!(r.code(), 200, "{user:?} {path}");
        assert_eq!(r.header("allow"), Some(*allow), "{user:?} {path}");
    }
    for (path, allow) in [
        ("/mokuro-reader/", BASE),
        ("/mokuro-reader/series/", BASE),
        ("/mokuro-reader/manga1.cbz", FILE),
    ] {
        let r = env
            .send_ctx(processor(), request("OPTIONS", path, &[], b""))
            .await;
        assert_eq!(r.header("allow"), Some(allow), "processor {path}");
    }
}

#[tokio::test]
async fn unknown_methods_are_405_with_allow() {
    let env = Env::new();
    let r = env.req(None, "POST", "/mokuro-reader/", &[], b"").await;
    assert_eq!(r.code(), 405);
    assert!(r.header("allow").is_some());
}

#[tokio::test]
async fn delete_archive_takes_its_sidecars() {
    let env = Env::new();
    assert_eq!(
        env.req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/owned.cbz",
            &[],
            &cbz_bytes(1)
        )
        .await
        .code(),
        201
    );
    for s in [
        "owned.mokuro",
        "owned.mokuro.gz",
        "owned.webp",
        "owned.nocover",
        "owned.hayai-nova.mokuro",
        "owned.5.mokuro",
    ] {
        std::fs::write(env.lib(s), b"x").unwrap();
    }
    std::fs::write(env.lib("owned.5.cbz"), b"x").unwrap();
    let r = env
        .req(
            Some("uploader"),
            "DELETE",
            "/mokuro-reader/owned.cbz",
            &[],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    for s in [
        "owned.cbz",
        "owned.mokuro",
        "owned.mokuro.gz",
        "owned.webp",
        "owned.nocover",
        "owned.hayai-nova.mokuro",
    ] {
        assert!(!env.lib(s).exists(), "{s}");
    }
    // Volume `owned.5` keeps its OCR.
    assert!(env.lib("owned.5.mokuro").exists());
    for c in [
        "forget_volume_upload owned.cbz",
        "forget_volume_uuid owned.cbz",
        "forget_ocr_sidecars_of_volume owned.cbz",
        "archives_removed owned.cbz",
        "audit delete library /mokuro-reader/owned.cbz",
    ] {
        assert!(env.hooks.has(c), "{c}: {:?}", env.hooks.calls());
    }
    // Deleting a primary sidecar (a re-OCR request) reports it before it goes.
    std::fs::write(env.lib("manga1.mokuro"), b"{}").unwrap();
    assert_eq!(
        env.req(
            Some("admin"),
            "DELETE",
            "/mokuro-reader/manga1.mokuro",
            &[],
            b""
        )
        .await
        .code(),
        204
    );
    assert!(env.hooks.has("primary_sidecar_leaving manga1.mokuro"));
    assert!(env.hooks.has("forget_ocr_sidecar manga1.mokuro"));
    assert!(env.lib("manga1.cbz").exists());
}

#[tokio::test]
async fn delete_folder_and_errors() {
    let env = Env::new();
    std::fs::write(env.lib("series/vol1.mokuro"), b"{}").unwrap();
    let r = env
        .req(Some("admin"), "DELETE", "/mokuro-reader/series/", &[], b"")
        .await;
    assert_eq!(r.code(), 204);
    assert!(!env.lib("series").exists());
    for c in [
        "forget_volume_uploads_under_prefix series",
        "forget_ocr_sidecars_under_prefix series",
        "forget_volume_uuids_under_prefix series",
        "archives_removed series",
        "audit delete library_folder /mokuro-reader/series",
    ] {
        assert!(env.hooks.has(c), "{c}: {:?}", env.hooks.calls());
    }
    assert_eq!(
        env.req(Some("admin"), "DELETE", "/mokuro-reader/nope", &[], b"")
            .await
            .code(),
        404
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "DELETE",
            "/mokuro-reader/manga1.cbz",
            &[("depth", "1")],
            b""
        )
        .await
        .code(),
        400
    );
    // The roots are never deleted (14.3).
    for p in ["/", "/mokuro-reader", "/mokuro-reader/"] {
        assert_eq!(
            env.req(Some("admin"), "DELETE", p, &[], b"").await.code(),
            403,
            "{p}"
        );
    }
    assert!(env.lib("manga1.cbz").exists());
    // Traversal.
    let outside = env.base.parent().unwrap().join("escape-del.txt");
    std::fs::write(&outside, b"keep").unwrap();
    let r = env
        .req(
            Some("admin"),
            "DELETE",
            "/mokuro-reader/../../escape-del.txt",
            &[],
            b"",
        )
        .await;
    assert!([400, 403, 404, 409].contains(&r.code()));
    assert!(outside.exists());
}

#[tokio::test]
async fn mkcol() {
    let env = Env::new();
    let r = env
        .req(
            Some("uploader"),
            "MKCOL",
            "/mokuro-reader/new_series",
            &[],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201);
    assert!(env.lib("new_series").is_dir());
    let ev = env
        .hooks
        .audits()
        .into_iter()
        .find(|a| a.action == "mkdir")
        .unwrap();
    assert_eq!(ev.target_type, "webdav_folder");
    assert_eq!(ev.target_path, "/mokuro-reader");
    assert_eq!(
        ev.details,
        Some(serde_json::json!({ "path": "/mokuro-reader/new_series" }))
    );
    assert_eq!(
        env.req(
            Some("uploader"),
            "MKCOL",
            "/mokuro-reader/new_series/Vol",
            &[],
            b""
        )
        .await
        .code(),
        201
    );
    let ev = env
        .hooks
        .audits()
        .into_iter()
        .rfind(|a| a.action == "mkdir")
        .unwrap();
    assert_eq!(
        (ev.target_type, ev.target_path.as_str()),
        ("library_folder", "/mokuro-reader/new_series")
    );
    assert_eq!(
        env.req(
            Some("uploader"),
            "MKCOL",
            "/mokuro-reader/new_series",
            &[],
            b""
        )
        .await
        .code(),
        405
    );
    assert_eq!(
        env.req(Some("uploader"), "MKCOL", "/mokuro-reader/a/b", &[], b"")
            .await
            .code(),
        409
    );
    assert_eq!(
        env.req(Some("uploader"), "MKCOL", "/mokuro-reader/x", &[], b"body")
            .await
            .code(),
        415
    );
    assert_eq!(
        env.req(
            Some("uploader"),
            "MKCOL",
            "/mokuro-reader/x",
            &[("depth", "1")],
            b""
        )
        .await
        .code(),
        400
    );
}

#[tokio::test]
async fn physical_path_mapping() {
    let env = Env::new();
    let lib = std::fs::canonicalize(env.base.join("library")).unwrap();
    let users = std::fs::canonicalize(env.base.join("users")).unwrap();
    let dav = &env.dav;
    let alice = Some(bunko_dav::Caller {
        username: "alice",
        owns_cloud_root: false,
    });
    let owner = Some(bunko_dav::Caller {
        username: "alice",
        owns_cloud_root: true,
    });
    assert_eq!(dav.physical_path("/", None), None);
    assert_eq!(dav.physical_path("/mokuro-reader", None), None);
    assert_eq!(
        dav.physical_path("/mokuro-reader/manga1.cbz", None),
        Some(lib.join("manga1.cbz"))
    );
    assert_eq!(
        dav.physical_path("/mokuro-reader/series/new.cbz", None),
        Some(lib.join("series/new.cbz"))
    );
    assert_eq!(
        dav.physical_path("/mokuro-reader/volume-data.json", alice),
        Some(users.join("alice/volume-data.json"))
    );
    assert_eq!(
        dav.physical_path("/mokuro-reader/history/d/x.events", alice),
        Some(users.join("alice/history/d/x.events"))
    );
    assert_eq!(
        dav.physical_path("/mokuro-reader/history/d/x.events", owner),
        Some(lib.join("history/d/x.events"))
    );
    assert_eq!(
        dav.physical_path("/mokuro-reader/goals.json", owner),
        Some(lib.join("goals.json"))
    );
    assert_eq!(
        dav.physical_path("/mokuro-reader/history/../../x", alice),
        None
    );
    assert_eq!(
        dav.physical_path("/mokuro-reader/volume-data.json", None),
        None
    );
    assert_eq!(dav.physical_path("/mokuro-reader/../escape", None), None);
    assert_eq!(
        dav.physical_path("/mokuro-reader/series/../goals.json", owner),
        None
    );
    assert_eq!(dav.physical_path("/inbox/x.cbz", alice), None);
    assert_eq!(dav.physical_path("/foo", alice), None);
}

#[tokio::test]
async fn listings_hide_staging_files_and_symlinks_stay_contained() {
    let env = Env::new();
    std::fs::write(env.lib("series/.vol2.cbz.upload-abc123.tmp"), b"partial").unwrap();
    std::fs::write(env.lib("series/.hidden"), b"x").unwrap();
    let r = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader/series",
            &[("depth", "1")],
            b"",
        )
        .await;
    assert!(!r.text().contains("upload-abc123"));
    assert!(r.text().contains("/mokuro-reader/series/.hidden"));
    #[cfg(unix)]
    {
        let outside = env.base.parent().unwrap().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, env.lib("escape")).unwrap();
        std::os::unix::fs::symlink(env.lib("series"), env.lib("alias")).unwrap();
        // Listed, but never resolved or walked out of the library.
        let r = env
            .req(None, "PROPFIND", "/mokuro-reader", &[("depth", "1")], b"")
            .await;
        assert!(r.text().contains("/mokuro-reader/escape/"));
        assert_eq!(
            env.req(None, "GET", "/mokuro-reader/escape/secret.txt", &[], b"")
                .await
                .code(),
            404
        );
        assert_eq!(
            env.req(
                Some("admin"),
                "DELETE",
                "/mokuro-reader/escape/secret.txt",
                &[],
                b""
            )
            .await
            .code(),
            404
        );
        assert_eq!(
            env.req(
                Some("admin"),
                "PUT",
                "/mokuro-reader/escape/new.txt",
                &[],
                b"x"
            )
            .await
            .code(),
            409
        );
        let all = env.req(None, "PROPFIND", "/mokuro-reader", &[], b"").await;
        assert!(!all.text().contains("secret.txt"));
        // A symlink inside the library works.
        assert_eq!(
            env.req(None, "GET", "/mokuro-reader/alias/vol1.cbz", &[], b"")
                .await
                .body,
            b"volume 1"
        );
        assert!(outside.join("secret.txt").exists());
    }
}

/// Adding an OCR file is an `upload`; overwriting it an `edit`; re-sending its exact bytes
/// replaces nothing and is an `edit` marked `unchanged` (generation-upgrade.md §3).
#[tokio::test]
async fn put_ocr_sidecar_create_overwrite_and_unchanged_resend() {
    let env = Env::new();
    let path = "/mokuro-reader/series/Vol%209.mokuro";
    let file = env.lib("series/Vol 9.mokuro");
    let first = br#"{"version":"0.2.1","pages":[]}"#;
    let r = env.req(Some("admin"), "PUT", path, &[], first).await;
    assert_eq!(r.code(), 201);
    let details = |n: usize| env.hooks.audits()[n].details.clone();
    assert_eq!(env.hooks.audits()[0].action, "upload");
    assert_eq!(
        details(0),
        Some(serde_json::json!({ "existed_before": false }))
    );

    // The same bytes again: the file is left alone (mtime, provenance) and no temp remains.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let before = std::fs::metadata(&file).unwrap().modified().unwrap();
    let forgets = |env: &Env| {
        env.hooks
            .calls()
            .iter()
            .filter(|c| c.as_str() == "forget_ocr_sidecar series/Vol 9.mokuro")
            .count()
    };
    let forgot = forgets(&env);
    let r = env.req(Some("admin"), "PUT", path, &[], first).await;
    assert_eq!(r.code(), 204);
    assert!(r.header("etag").is_some());
    assert_eq!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );
    assert_eq!(std::fs::read(&file).unwrap(), first);
    assert_eq!(forgets(&env), forgot, "its provenance row is kept");
    assert_eq!(env.hooks.audits()[1].action, "edit");
    assert_eq!(
        details(1),
        Some(serde_json::json!({ "existed_before": true, "unchanged": true }))
    );
    let leftovers: Vec<_> = std::fs::read_dir(env.lib("series"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".upload-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Different bytes of the same length: a real edit.
    let second = br#"{"version":"0.2.1","pages":[1]}"#;
    let r = env
        .req(Some("admin"), "PUT", path, &[], &second[..first.len()])
        .await;
    assert_eq!(r.code(), 204);
    assert_eq!(std::fs::read(&file).unwrap(), &second[..first.len()]);
    assert_ne!(
        std::fs::metadata(&file).unwrap().modified().unwrap(),
        before
    );
    assert_eq!(forgets(&env), forgot + 1);
    assert_eq!(env.hooks.audits()[2].action, "edit");
    assert_eq!(
        details(2),
        Some(serde_json::json!({ "existed_before": true }))
    );
}

#[tokio::test]
async fn positions_json_is_per_user() {
    let env = Env::new();
    for (user, body) in [
        ("reader", &b"reader positions"[..]),
        ("uploader", &b"uploader positions"[..]),
    ] {
        let r = env
            .req(
                Some(user),
                "PUT",
                "/mokuro-reader/positions.json",
                &[],
                body,
            )
            .await;
        assert_eq!(r.code(), 201, "{user}");
    }
    for (user, body) in [
        ("reader", &b"reader positions"[..]),
        ("uploader", &b"uploader positions"[..]),
    ] {
        assert_eq!(
            env.req(Some(user), "GET", "/mokuro-reader/positions.json", &[], b"")
                .await
                .body,
            body,
            "{user}"
        );
        assert_eq!(
            std::fs::read(env.base.join(format!("users/{user}/positions.json"))).unwrap(),
            body
        );
    }
    // Never a library file, never shown to anyone else.
    assert!(!env.lib("positions.json").exists());
    assert_eq!(
        env.req(None, "GET", "/mokuro-reader/positions.json", &[], b"")
            .await
            .code(),
        404
    );
    let me = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "1")],
            b"",
        )
        .await;
    assert!(me.text().contains("positions.json"));
    let anon = env
        .req(None, "PROPFIND", "/mokuro-reader", &[("depth", "1")], b"")
        .await;
    assert!(!anon.text().contains("positions.json"));
}

// --- The per-account reading-history tree (0.8) ---------------------------------------

const EVENTS: &str = "/mokuro-reader/history/dev-1/2026-10.events";

/// MKCOL `history` and `history/dev-1` as `user`, then PUT one events file.
async fn put_history(env: &Env, user: &str, body: &[u8]) {
    for path in ["/mokuro-reader/history", "/mokuro-reader/history/dev-1"] {
        let r = env.req(Some(user), "MKCOL", path, &[], b"").await;
        assert!(
            matches!(r.code(), 201 | 405),
            "{user} MKCOL {path}: {}",
            r.code()
        );
    }
    let r = env.req(Some(user), "PUT", EVENTS, &[], body).await;
    assert!(matches!(r.code(), 201 | 204), "{user} PUT: {}", r.code());
}

/// A history file is the account's own bytes, stored as sent: one named `.mokuro` never
/// reaches the hook that may rewrite a library OCR file before it lands (critique C5),
/// while a library `.mokuro` still does.
#[tokio::test]
async fn a_history_file_never_reaches_the_ocr_sidecar_hook() {
    let env = Env::new();
    put_history(&env, "reader", b"events").await;
    let put = |user, path| env.req(Some(user), "PUT", path, &[], b"{}");
    let r = put("reader", "/mokuro-reader/history/dev-1/x.mokuro").await;
    assert_eq!(r.code(), 201);
    let r = put("uploader", "/mokuro-reader/series/vol1.mokuro").await;
    assert_eq!(r.code(), 201);
    assert_eq!(*env.hooks.staged.lock(), ["series/vol1.mokuro"]);
}

#[tokio::test]
async fn history_is_per_user() {
    let env = Env::new();
    put_history(&env, "reader", b"reader events").await;
    put_history(&env, "uploader", b"uploader events").await;
    for (user, body) in [("reader", "reader events"), ("uploader", "uploader events")] {
        let r = env.req(Some(user), "GET", EVENTS, &[], b"").await;
        assert_eq!((r.code(), r.text().as_str()), (200, body), "{user}");
        assert_eq!(
            std::fs::read_to_string(
                env.base
                    .join(format!("users/{user}/history/dev-1/2026-10.events"))
            )
            .unwrap(),
            body
        );
    }
    assert!(!env.lib("history").exists());
    assert_eq!(env.req(None, "GET", EVENTS, &[], b"").await.code(), 404);
    assert_eq!(
        env.req(Some("admin"), "GET", EVENTS, &[], b"").await.code(),
        404
    );
}

#[tokio::test]
async fn history_listing() {
    let env = Env::new();
    put_history(&env, "reader", b"reader events").await;
    let depth1 = |user| env.req(user, "PROPFIND", "/mokuro-reader", &[("depth", "1")], b"");
    assert!(
        depth1(Some("reader"))
            .await
            .text()
            .contains("<D:href>/mokuro-reader/history/</D:href>")
    );
    for user in [None, Some("uploader")] {
        assert!(!depth1(user).await.text().contains("history"), "{user:?}");
    }
    for path in ["/mokuro-reader", "/mokuro-reader/history", "/"] {
        let r = env
            .req(
                Some("reader"),
                "PROPFIND",
                path,
                &[("depth", "infinity")],
                b"",
            )
            .await;
        assert_eq!(r.code(), 207, "{path}");
        let t = r.text();
        let at = t
            .find("<D:href>/mokuro-reader/history/dev-1/2026-10.events</D:href>")
            .unwrap_or_else(|| panic!("{path}: {t}"));
        let entry = &t[at..at + t[at..].find("</D:response>").unwrap()];
        assert!(
            entry.contains("<D:getcontentlength>13</D:getcontentlength>"),
            "{path}"
        );
        assert!(entry.contains("<D:getlastmodified>"), "{path}");
        assert!(entry.contains("<D:getetag>"), "{path}");
        assert!(
            t.contains("<D:href>/mokuro-reader/history/dev-1/</D:href>"),
            "{path}"
        );
        assert!(t.ends_with("</D:response></D:multistatus>"), "{path}");
    }
    // Another account sees none of it, whatever the depth.
    for user in [None, Some("uploader")] {
        let r = env
            .req(
                user,
                "PROPFIND",
                "/mokuro-reader",
                &[("depth", "infinity")],
                b"",
            )
            .await;
        assert!(!r.text().contains("history"), "{user:?}");
    }
    assert_eq!(
        env.req(
            Some("uploader"),
            "PROPFIND",
            "/mokuro-reader/history",
            &[("depth", "infinity")],
            b""
        )
        .await
        .code(),
        404
    );
}

#[tokio::test]
async fn stray_history_in_library_is_not_listed() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("history/dev")).unwrap();
    std::fs::write(env.lib("history/dev/x.events"), b"stray").unwrap();
    for user in [None, Some("reader")] {
        for depth in ["1", "infinity"] {
            let r = env
                .req(user, "PROPFIND", "/mokuro-reader", &[("depth", depth)], b"")
                .await;
            assert_eq!(r.code(), 207);
            assert!(r.text().contains("manga1.cbz"));
            assert!(!r.text().contains("history"), "{user:?} {depth}");
        }
        assert_eq!(
            env.req(user, "GET", "/mokuro-reader/history/dev/x.events", &[], b"")
                .await
                .code(),
            404
        );
    }
}

#[tokio::test]
async fn depth_infinity_cache_survives_history_put() {
    let env = Env::new();
    let warm = env
        .req(
            None,
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(warm.code(), 207);
    assert_eq!(env.dav.propfind_cache().usage().0, 1);
    put_history(&env, "reader", b"events").await;
    let r = env
        .req(
            Some("reader"),
            "PUT",
            "/mokuro-reader/goals.json",
            &[],
            b"{}",
        )
        .await;
    assert_eq!(r.code(), 201);
    // The shared listing holds no per-user resource: nothing to evict.
    assert_eq!(env.dav.propfind_cache().usage().0, 1);
}

#[tokio::test]
async fn history_mkcol() {
    let env = Env::new();
    let mk = |path: &'static str| env.req(Some("reader"), "MKCOL", path, &[], b"");
    // history/a/b with history missing: the parent is not there yet.
    assert_eq!(mk("/mokuro-reader/history/a/b").await.code(), 409);
    assert_eq!(mk("/mokuro-reader/history").await.code(), 201);
    assert!(env.base.join("users/reader/history").is_dir());
    assert_eq!(mk("/mokuro-reader/history").await.code(), 405);
    assert_eq!(mk("/mokuro-reader/history/dev").await.code(), 201);
    assert_eq!(mk("/mokuro-reader/history/dev/sub").await.code(), 403);
    assert_eq!(mk("/mokuro-reader/history/.dev").await.code(), 403);
    assert_eq!(mk("/mokuro-reader/goals.json").await.code(), 403);
    assert!(!env.lib("history").exists());
    // A first MKCOL creates the account's directory too.
    std::fs::remove_dir_all(env.base.join("users/uploader")).unwrap();
    assert_eq!(
        env.req(
            Some("uploader"),
            "MKCOL",
            "/mokuro-reader/history",
            &[],
            b""
        )
        .await
        .code(),
        201
    );
    assert!(env.base.join("users/uploader/history").is_dir());
    // Anonymous has no tree to create it in.
    assert_eq!(
        env.req(None, "MKCOL", "/mokuro-reader/history", &[], b"")
            .await
            .code(),
        403
    );
}

#[tokio::test]
async fn history_put_shape_and_limits() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("storage");
    std::fs::create_dir_all(base.join("users/reader/history/dev")).unwrap();
    std::fs::create_dir_all(base.join("library")).unwrap();
    let env = Env::with_config(
        dir,
        base,
        bunko_dav::DavConfig {
            history: bunko_dav::HistoryLimits {
                max_file_bytes: 100,
                max_total_bytes: 250,
            },
            ..Default::default()
        },
    );
    let env = &env;
    let put = |path: &'static str, body: Vec<u8>| async move {
        env.req(Some("reader"), "PUT", path, &[], &body).await
    };
    for path in [
        "/mokuro-reader/history/x.events",
        "/mokuro-reader/history/dev/a/b",
        "/mokuro-reader/history/.dev/x",
        "/mokuro-reader/history/dev/.x",
    ] {
        let r = put(path, b"x".to_vec()).await;
        assert_eq!(r.code(), 403, "{path}");
        assert_eq!(r.json()["ok"], false, "{path}");
    }
    let r = put("/mokuro-reader/history/dev/big", vec![b'x'; 101]).await;
    assert_eq!(r.code(), 413);
    assert_eq!(r.json()["ok"], false);
    assert!(!env.base.join("users/reader/history/dev/big").exists());
    // A body with no Content-Length is cut at the cap.
    let r = env
        .req(
            Some("reader"),
            "PUT",
            "/mokuro-reader/history/dev/chunked",
            &[("content-length", "")],
            &[b'x'; 101],
        )
        .await;
    assert_eq!(r.code(), 413);
    assert!(!env.base.join("users/reader/history/dev/chunked").exists());
    // Nothing is left behind, staging files included.
    let left = std::fs::read_dir(env.base.join("users/reader/history/dev")).unwrap();
    assert_eq!(left.count(), 0);
    for name in ["a", "b"] {
        let path = format!("/mokuro-reader/history/dev/{name}");
        let r = env
            .req(Some("reader"), "PUT", &path, &[], &[b'x'; 100])
            .await;
        assert_eq!(r.code(), 201, "{name}");
    }
    // 200 stored: 60 more is over 250.
    let r = put("/mokuro-reader/history/dev/c", vec![b'x'; 60]).await;
    assert_eq!(r.code(), 507);
    assert_eq!(r.json()["ok"], false);
    // Replacing a file counts its new size, not both.
    let r = put("/mokuro-reader/history/dev/a", vec![b'y'; 100]).await;
    assert_eq!(r.code(), 204);
    let r = put("/mokuro-reader/history/dev/c", vec![b'x'; 50]).await;
    assert_eq!(r.code(), 201);
}

#[tokio::test]
async fn history_move_copy_lock_refused() {
    let env = Env::new();
    put_history(&env, "reader", b"events").await;
    let dest = "http://localhost:8080/mokuro-reader/history/dev-1/moved.events";
    for (method, path, headers) in [
        ("MOVE", EVENTS, vec![("destination", dest)]),
        ("COPY", EVENTS, vec![("destination", dest)]),
        (
            "MOVE",
            "/mokuro-reader/history/dev-1",
            vec![(
                "destination",
                "http://localhost:8080/mokuro-reader/history/dev-2",
            )],
        ),
        (
            "COPY",
            "/mokuro-reader/volume-data.json",
            vec![("destination", "http://localhost:8080/mokuro-reader/history")],
        ),
    ] {
        let r = env.req(Some("reader"), method, path, &headers, b"").await;
        assert_eq!(r.code(), 403, "{method} {path}");
    }
    let lockinfo = br#"<D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockinfo>"#;
    let r = env.req(Some("reader"), "LOCK", EVENTS, &[], lockinfo).await;
    assert_eq!(r.code(), 403);
    let r = env
        .req(
            Some("reader"),
            "UNLOCK",
            EVENTS,
            &[("lock-token", "<opaquelocktoken:x>")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 403);
    let patch = br#"<D:propertyupdate xmlns:D="DAV:"><D:set><D:prop><x:a xmlns:x="urn:x">1</x:a></D:prop></D:set></D:propertyupdate>"#;
    let r = env
        .req(Some("reader"), "PROPPATCH", EVENTS, &[], patch)
        .await;
    assert_eq!(r.code(), 403);
    assert!(
        env.base
            .join("users/reader/history/dev-1/2026-10.events")
            .is_file()
    );
}

#[tokio::test]
async fn history_delete() {
    let env = Env::new();
    put_history(&env, "reader", b"reader events").await;
    put_history(&env, "uploader", b"uploader events").await;
    let r = env
        .req(
            Some("reader"),
            "DELETE",
            "/mokuro-reader/history/dev-1",
            &[],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    assert!(!env.base.join("users/reader/history/dev-1").exists());
    assert!(env.base.join("users/reader/history").is_dir());
    assert!(
        env.base
            .join("users/uploader/history/dev-1/2026-10.events")
            .is_file()
    );
    let r = env.req(Some("uploader"), "DELETE", EVENTS, &[], b"").await;
    assert_eq!(r.code(), 204);
    assert!(
        !env.base
            .join("users/uploader/history/dev-1/2026-10.events")
            .exists()
    );
}

#[tokio::test]
async fn history_writes_fire_no_library_hooks() {
    let env = Env::new();
    put_history(&env, "uploader", b"events").await;
    let r = env
        .req(
            Some("uploader"),
            "DELETE",
            "/mokuro-reader/history",
            &[],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    let calls = env.hooks.calls();
    assert!(
        calls.iter().all(|c| c.starts_with("audit ")),
        "library hooks fired: {calls:?}"
    );
    let audits = env.hooks.audits();
    assert_eq!(audits.len(), 4, "{audits:?}");
    for a in &audits {
        assert_eq!(a.target_type, "progress", "{a:?}");
    }
    assert_eq!(
        audits.iter().map(|a| a.action).collect::<Vec<_>>(),
        ["mkdir", "mkdir", "upload", "delete"]
    );
    assert_eq!(audits[2].target_path, EVENTS);
}

/// The account that owns a cloud library's root keeps its per-user files there.
#[tokio::test]
async fn cloud_root_owner_uses_the_library_root() {
    let env = Env::new();
    let owner = || {
        let mut ctx = env.ctx(Some("uploader"));
        ctx.owns_cloud_root = true;
        ctx
    };
    for path in ["/mokuro-reader/history", "/mokuro-reader/history/dev-1"] {
        let r = env
            .send_ctx(owner(), request("MKCOL", path, &[], b""))
            .await;
        assert_eq!(r.code(), 201, "{path}");
    }
    let r = env
        .send_ctx(owner(), request("PUT", EVENTS, &[], b"owner events"))
        .await;
    assert_eq!(r.code(), 201);
    let r = env
        .send_ctx(
            owner(),
            request("PUT", "/mokuro-reader/goals.json", &[], b"{}"),
        )
        .await;
    assert_eq!(r.code(), 201);
    assert!(env.lib("history/dev-1/2026-10.events").is_file());
    assert!(env.lib("goals.json").is_file());
    assert!(!env.base.join("users/uploader/history").exists());
    // No library hook for them, and no one else sees them.
    assert!(
        env.hooks.calls().iter().all(|c| c.starts_with("audit ")),
        "{:?}",
        env.hooks.calls()
    );
    let r = env
        .send_ctx(
            owner(),
            request("PROPFIND", "/mokuro-reader", &[("depth", "infinity")], b""),
        )
        .await;
    assert!(
        r.text()
            .contains("/mokuro-reader/history/dev-1/2026-10.events")
    );
    assert!(r.text().contains("/mokuro-reader/goals.json"));
    for user in [None, Some("uploader"), Some("reader")] {
        let r = env
            .req(
                user,
                "PROPFIND",
                "/mokuro-reader",
                &[("depth", "infinity")],
                b"",
            )
            .await;
        assert!(!r.text().contains("history"), "{user:?}");
        assert!(!r.text().contains("goals.json"), "{user:?}");
        assert_eq!(env.req(user, "GET", EVENTS, &[], b"").await.code(), 404);
        // Not through a library path that walks back up into them either.
        for path in [
            "/mokuro-reader/series/../history/dev-1/2026-10.events",
            "/mokuro-reader/series/../goals.json",
        ] {
            assert_eq!(
                env.req(user, "GET", path, &[], b"").await.code(),
                404,
                "{path}"
            );
        }
    }
    // A library symlink into them is listed (it is a library entry) but never walked.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(env.lib("history"), env.lib("link")).unwrap();
        env.dav.propfind_cache().invalidate();
        let r = env
            .req(
                None,
                "PROPFIND",
                "/mokuro-reader",
                &[("depth", "infinity")],
                b"",
            )
            .await;
        assert!(r.text().contains("/mokuro-reader/link/"));
        assert!(!r.text().contains("2026-10.events"));
        let r = env
            .req(
                None,
                "GET",
                "/mokuro-reader/link/dev-1/2026-10.events",
                &[],
                b"",
            )
            .await;
        assert_eq!(r.code(), 404);
    }
    let r = env
        .req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/series/../goals.json",
            &[],
            b"overwritten",
        )
        .await;
    // Held before any handler resolves it: a `..` segment is never "not frozen"
    // (the HTTP edge answers 400 for it first).
    assert_eq!(r.code(), 409);
    assert_eq!(std::fs::read(env.lib("goals.json")).unwrap(), b"{}");
}
