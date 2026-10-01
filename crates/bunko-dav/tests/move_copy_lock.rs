//! MOVE/COPY (incl. the fixed destructive quirks 14.1–14.3), LOCK/UNLOCK and PROPPATCH.

mod common;

use common::*;

fn dest(path: &str) -> String {
    format!("http://localhost:8080{path}")
}

#[tokio::test]
async fn move_library_file_preserves_sidecars_and_owner() {
    let env = Env::new();
    assert_eq!(
        env.req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/rename-me.cbz",
            &[],
            &cbz_bytes(1)
        )
        .await
        .code(),
        201
    );
    for s in [
        "rename-me.mokuro",
        "rename-me.mokuro.gz",
        "rename-me.webp",
        "rename-me.nocover",
    ] {
        std::fs::write(env.lib(s), b"x").unwrap();
    }
    let d = dest("/mokuro-reader/renamed.cbz");
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/rename-me.cbz",
            &[("destination", &d), ("overwrite", "T")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    assert!(!env.lib("rename-me.cbz").exists());
    assert!(env.lib("renamed.cbz").exists());
    for s in [
        "rename-me.mokuro",
        "rename-me.mokuro.gz",
        "rename-me.webp",
        "rename-me.nocover",
    ] {
        assert!(env.lib(s).exists(), "{s}");
    }
    assert!(!env.lib("renamed.mokuro").exists());
    for c in [
        "rename_volume_upload rename-me.cbz renamed.cbz",
        "forget_ocr_sidecars_of_volume rename-me.cbz",
        "archives_removed rename-me.cbz",
        "archive_arrived renamed.cbz",
        "audit move library /mokuro-reader/rename-me.cbz",
    ] {
        assert!(env.hooks.has(c), "{c}: {:?}", env.hooks.calls());
    }
    let ev = env
        .hooks
        .audits()
        .into_iter()
        .find(|a| a.action == "move")
        .unwrap();
    assert_eq!(
        ev.details,
        Some(serde_json::json!({ "destination": "/mokuro-reader/renamed.cbz" }))
    );
}

#[tokio::test]
async fn move_series_folder_moves_everything() {
    let env = Env::new();
    for s in ["vol1.mokuro", "vol1.mokuro.gz", "vol1.webp", "vol1.nocover"] {
        std::fs::write(env.lib(&format!("series/{s}")), b"x").unwrap();
    }
    let d = dest("/mokuro-reader/series-renamed");
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/series",
            &[("destination", &d), ("depth", "infinity")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201);
    assert!(!env.lib("series").exists());
    for s in [
        "vol1.cbz",
        "vol1.mokuro",
        "vol1.mokuro.gz",
        "vol1.webp",
        "vol1.nocover",
    ] {
        assert!(env.lib(&format!("series-renamed/{s}")).exists(), "{s}");
    }
    for c in [
        "rename_volume_upload series/vol1.cbz series-renamed/vol1.cbz",
        "rename_ocr_sidecars_under_prefix series series-renamed",
        "rename_volume_uuids_under_prefix series series-renamed",
        "archives_removed series",
        "audit move library_folder /mokuro-reader/series",
    ] {
        assert!(env.hooks.has(c), "{c}: {:?}", env.hooks.calls());
    }
    // A folder MOVE needs Depth: infinity.
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/series-renamed",
            &[("destination", &dest("/mokuro-reader/x")), ("depth", "0")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 400);
}

#[tokio::test]
async fn cross_class_moves_are_refused_without_effects() {
    let env = Env::new();
    // 14.1: progress -> library would have deleted alice's progress file.
    let r = env
        .req(
            Some("reader"),
            "MOVE",
            "/mokuro-reader/volume-data.json",
            &[("destination", &dest("/mokuro-reader/series/x.cbz"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 403);
    assert!(env.base.join("users/reader/volume-data.json").exists());
    // library .cbz -> a per-user name.
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/manga1.cbz",
            &[("destination", &dest("/mokuro-reader/goals.json"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 403);
    assert!(env.lib("manga1.cbz").exists());
    // Anything -> outside the reader root.
    for target in ["/foo", "/inbox/x.cbz", "/", "/mokuro-reader"] {
        let r = env
            .req(
                Some("admin"),
                "MOVE",
                "/mokuro-reader/manga1.cbz",
                &[("destination", &dest(target))],
                b"",
            )
            .await;
        assert_eq!(r.code(), 403, "{target}");
        let r = env
            .req(
                Some("admin"),
                "COPY",
                "/mokuro-reader/manga1.cbz",
                &[("destination", &dest(target))],
                b"",
            )
            .await;
        assert_eq!(r.code(), 403, "{target}");
    }
    assert!(env.lib("manga1.cbz").exists());
    // 14.3: the virtual roots are never moved or copied.
    for src in ["/", "/mokuro-reader", "/mokuro-reader/"] {
        for m in ["MOVE", "COPY"] {
            let r = env
                .req(
                    Some("admin"),
                    m,
                    src,
                    &[("destination", &dest("/mokuro-reader/elsewhere"))],
                    b"",
                )
                .await;
            assert_eq!(r.code(), 403, "{m} {src}");
        }
    }
    assert!(env.lib("series/vol1.cbz").exists());
    assert!(
        !env.hooks
            .calls()
            .iter()
            .any(|c| c.starts_with("audit move") || c.starts_with("audit delete"))
    );
}

#[tokio::test]
async fn file_and_collection_never_replace_each_other() {
    let env = Env::new();
    std::fs::write(env.lib("series/big.bin"), b"data").unwrap();
    // 14.2: this removed the whole series folder in 0.5.2.
    let r = env
        .req(
            Some("admin"),
            "COPY",
            "/mokuro-reader/series/big.bin",
            &[("destination", &dest("/mokuro-reader/series"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 409);
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/series/big.bin",
            &[("destination", &dest("/mokuro-reader/thumbnails"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 409);
    std::fs::create_dir_all(env.lib("dir")).unwrap();
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/dir",
            &[("destination", &dest("/mokuro-reader/manga1.cbz"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 409);
    assert!(env.lib("series/vol1.cbz").exists());
    assert!(env.lib("manga1.cbz").exists());
    assert!(env.lib("dir").is_dir());
}

#[tokio::test]
async fn copy_move_preconditions() {
    let env = Env::new();
    let m1 = "/mokuro-reader/manga1.cbz";
    assert_eq!(
        env.req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/nope.cbz",
            &[("destination", &dest("/mokuro-reader/x.cbz"))],
            b""
        )
        .await
        .code(),
        404
    );
    assert_eq!(
        env.req(Some("admin"), "MOVE", m1, &[], b"").await.code(),
        400
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "MOVE",
            m1,
            &[
                ("destination", &dest("/mokuro-reader/x.cbz")),
                ("overwrite", "X")
            ],
            b""
        )
        .await
        .code(),
        400
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "MOVE",
            m1,
            &[("destination", "http://elsewhere:9/mokuro-reader/x.cbz")],
            b""
        )
        .await
        .code(),
        502
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "MOVE",
            m1,
            &[("destination", &dest("/mokuro-reader/nope/x.cbz"))],
            b""
        )
        .await
        .code(),
        409
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "MOVE",
            m1,
            &[("destination", &dest(m1))],
            b""
        )
        .await
        .code(),
        403
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/series",
            &[("destination", &dest("/mokuro-reader/series/inner"))],
            b""
        )
        .await
        .code(),
        403
    );
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            m1,
            &[
                ("destination", &dest("/mokuro-reader/manga2.cbz")),
                ("overwrite", "F"),
            ],
            b"",
        )
        .await;
    assert_eq!(r.code(), 412);
    // A path-only Destination works too, and percent-encoding is decoded.
    let r = env
        .req(
            Some("admin"),
            "MOVE",
            m1,
            &[("destination", "/mokuro-reader/series/Vol%20%CE%A9.cbz")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    assert!(env.lib("series/Vol Ω.cbz").exists());
}

#[tokio::test]
async fn copy_file_and_folder() {
    let env = Env::new();
    std::fs::write(env.lib("manga2.mokuro"), b"{}").unwrap();
    let r = env
        .req(
            Some("admin"),
            "COPY",
            "/mokuro-reader/manga1.cbz",
            &[("destination", &dest("/mokuro-reader/series/copy.cbz"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201);
    assert_eq!(
        std::fs::read(env.lib("series/copy.cbz")).unwrap(),
        b"fake cbz content 1"
    );
    assert_eq!(
        std::fs::metadata(env.lib("series/copy.cbz"))
            .unwrap()
            .modified()
            .unwrap(),
        std::fs::metadata(env.lib("manga1.cbz"))
            .unwrap()
            .modified()
            .unwrap()
    );
    assert!(env.hooks.has("archive_arrived copy.cbz"));
    assert!(
        !env.hooks
            .calls()
            .iter()
            .any(|c| c.starts_with("record_volume_upload"))
    );
    // COPY over an existing archive deletes it (and its sidecars) first.
    let r = env
        .req(
            Some("admin"),
            "COPY",
            "/mokuro-reader/manga1.cbz",
            &[("destination", &dest("/mokuro-reader/manga2.cbz"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    assert!(!env.lib("manga2.mokuro").exists());
    assert_eq!(
        std::fs::read(env.lib("manga2.cbz")).unwrap(),
        b"fake cbz content 1"
    );
    // Folder copy, infinity and 0.
    let r = env
        .req(
            Some("admin"),
            "COPY",
            "/mokuro-reader/series",
            &[("destination", &dest("/mokuro-reader/series2"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201);
    assert!(env.lib("series2/vol1.cbz").exists());
    assert!(env.lib("series2/copy.cbz").exists());
    let r = env
        .req(
            Some("admin"),
            "COPY",
            "/mokuro-reader/series",
            &[
                ("destination", &dest("/mokuro-reader/series3")),
                ("depth", "0"),
            ],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201);
    assert!(env.lib("series3").is_dir());
    assert!(!env.lib("series3/vol1.cbz").exists());
    // Collection over collection: not merged.
    std::fs::write(env.lib("series2/extra.txt"), b"x").unwrap();
    let r = env
        .req(
            Some("admin"),
            "COPY",
            "/mokuro-reader/series",
            &[("destination", &dest("/mokuro-reader/series2"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    assert!(!env.lib("series2/extra.txt").exists());
    assert!(env.lib("series2/vol1.cbz").exists());
    // Progress -> progress.
    let r = env
        .req(
            Some("reader"),
            "COPY",
            "/mokuro-reader/volume-data.json",
            &[("destination", &dest("/mokuro-reader/goals.json"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201);
    assert_eq!(
        std::fs::read(env.base.join("users/reader/goals.json")).unwrap(),
        b"reader progress"
    );
}

const LOCKINFO: &[u8] = br#"<?xml version="1.0" encoding="utf-8"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype><D:owner><D:href>me</D:href></D:owner></D:lockinfo>"#;

#[tokio::test]
async fn lock_unlock_and_writes_under_a_lock() {
    let env = Env::new();
    let path = "/mokuro-reader/series/vol1.cbz";
    let r = env
        .req(
            Some("admin"),
            "LOCK",
            path,
            &[("timeout", "Second-60")],
            LOCKINFO,
        )
        .await;
    assert_eq!(r.code(), 200);
    assert_eq!(
        r.header("content-type"),
        Some("application/xml; charset=utf-8")
    );
    let token = r
        .header("lock-token")
        .unwrap()
        .trim_matches(|c| c == '<' || c == '>')
        .to_string();
    assert!(token.starts_with("opaquelocktoken:"));
    let t = r.text();
    assert!(t.contains("<D:lockscope><D:exclusive /></D:lockscope>"));
    assert!(t.contains("<D:owner><href xmlns=\"DAV:\">me</href></D:owner>"));
    assert!(t.contains("<D:timeout>Second-"));
    assert!(t.contains("<D:lockroot><D:href>/mokuro-reader/series/vol1.cbz</D:href></D:lockroot>"));
    // A second exclusive lock conflicts.
    let r = env.req(Some("editor"), "LOCK", path, &[], LOCKINFO).await;
    assert_eq!(r.code(), 423);
    assert!(r.text().contains("no-conflicting-lock"));
    // Writes without the token: 423; with it: allowed.
    assert_eq!(
        env.req(Some("admin"), "PUT", path, &[], &cbz_bytes(1))
            .await
            .code(),
        423
    );
    assert_eq!(
        env.req(Some("admin"), "DELETE", "/mokuro-reader/series", &[], b"")
            .await
            .code(),
        423
    );
    let ifh = format!("(<{token}>)");
    assert_eq!(
        env.req(Some("admin"), "PUT", path, &[("if", &ifh)], &cbz_bytes(1))
            .await
            .code(),
        204
    );
    // The lock shows in a named PROPFIND.
    let body = br#"<D:propfind xmlns:D="DAV:"><D:prop><D:lockdiscovery/><D:supportedlock/></D:prop></D:propfind>"#;
    let r = env
        .req(Some("admin"), "PROPFIND", path, &[("depth", "0")], body)
        .await;
    assert!(r.text().contains(&token));
    assert!(r.text().contains("<D:lockentry>"));
    // Refresh.
    let r = env
        .req(
            Some("admin"),
            "LOCK",
            path,
            &[("if", &ifh), ("timeout", "Second-120")],
            b"",
        )
        .await;
    assert_eq!(r.code(), 200);
    assert!(r.header("lock-token").is_none());
    // Only the owner unlocks (0.5.2: anyone).
    let lt = format!("<{token}>");
    assert_eq!(
        env.req(Some("editor"), "UNLOCK", path, &[("lock-token", &lt)], b"")
            .await
            .code(),
        403
    );
    assert_eq!(
        env.req(Some("admin"), "UNLOCK", path, &[], b"")
            .await
            .code(),
        400
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "UNLOCK",
            path,
            &[("lock-token", "<opaquelocktoken:nope>")],
            b""
        )
        .await
        .code(),
        409
    );
    assert_eq!(
        env.req(Some("admin"), "UNLOCK", path, &[("lock-token", &lt)], b"")
            .await
            .code(),
        204
    );
    assert_eq!(
        env.req(Some("editor"), "LOCK", path, &[], LOCKINFO)
            .await
            .code(),
        200
    );
}

#[tokio::test]
async fn lock_null_and_per_user_locks() {
    let env = Env::new();
    // LOCK of an unmapped URL: 201, nothing created (0.5.2: 500 + dangling lock).
    let r = env
        .req(
            Some("admin"),
            "LOCK",
            "/mokuro-reader/series/new.txt",
            &[],
            LOCKINFO,
        )
        .await;
    assert_eq!(r.code(), 201);
    assert!(!env.lib("series/new.txt").exists());
    let token = r.header("lock-token").unwrap().to_string();
    assert_eq!(
        env.req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/series/new.txt",
            &[],
            b"x"
        )
        .await
        .code(),
        423
    );
    let ifh = format!("({token})");
    assert_eq!(
        env.req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/series/new.txt",
            &[("if", &ifh)],
            b"x"
        )
        .await
        .code(),
        201
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "LOCK",
            "/mokuro-reader/nope/new.txt",
            &[],
            LOCKINFO
        )
        .await
        .code(),
        409
    );
    // Locks on progress files are per user (0.5.2 keyed them by URL).
    assert_eq!(
        env.req(
            Some("reader"),
            "LOCK",
            "/mokuro-reader/volume-data.json",
            &[],
            LOCKINFO
        )
        .await
        .code(),
        200
    );
    assert_eq!(
        env.req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/volume-data.json",
            &[],
            b"{}"
        )
        .await
        .code(),
        201
    );
    assert_eq!(
        env.req(
            Some("reader"),
            "PUT",
            "/mokuro-reader/volume-data.json",
            &[],
            b"{}"
        )
        .await
        .code(),
        423
    );
    // The roots are never locked.
    assert_eq!(
        env.req(Some("admin"), "LOCK", "/mokuro-reader", &[], LOCKINFO)
            .await
            .code(),
        403
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "LOCK",
            "/mokuro-reader/manga1.cbz",
            &[("depth", "1")],
            LOCKINFO
        )
        .await
        .code(),
        400
    );
    assert_eq!(
        env.req(
            Some("admin"),
            "LOCK",
            "/mokuro-reader/manga1.cbz",
            &[],
            b"<D:nope xmlns:D='DAV:'/>"
        )
        .await
        .code(),
        400
    );
}

#[tokio::test]
async fn proppatch_dead_properties() {
    let env = Env::new();
    let path = "/mokuro-reader/manga1.cbz";
    let set = br#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:set><D:prop><Z:Win32FileAttributes>00000020</Z:Win32FileAttributes></D:prop></D:set></D:propertyupdate>"#;
    let r = env.req(Some("admin"), "PROPPATCH", path, &[], set).await;
    assert_eq!(r.code(), 207);
    assert!(r.text().contains("HTTP/1.1 200 OK"));
    let body = br#"<D:propfind xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:prop><Z:Win32FileAttributes/></D:prop></D:propfind>"#;
    let r = env
        .req(Some("admin"), "PROPFIND", path, &[("depth", "0")], body)
        .await;
    assert!(r.text().contains("<ns0:Win32FileAttributes xmlns:ns0=\"urn:schemas-microsoft-com:\">00000020</ns0:Win32FileAttributes>"), "{}", r.text());
    // A protected DAV: property fails, and takes the rest with it.
    let mixed = br#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:z"><D:set><D:prop><D:getetag>x</D:getetag><Z:a>1</Z:a></D:prop></D:set></D:propertyupdate>"#;
    let r = env.req(Some("admin"), "PROPPATCH", path, &[], mixed).await;
    assert!(r.text().contains("HTTP/1.1 403 Forbidden"));
    assert!(r.text().contains("HTTP/1.1 424 Failed Dependency"));
    let remove = br#"<D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:schemas-microsoft-com:"><D:remove><D:prop><Z:Win32FileAttributes/></D:prop></D:remove></D:propertyupdate>"#;
    assert_eq!(
        env.req(Some("admin"), "PROPPATCH", path, &[], remove)
            .await
            .code(),
        207
    );
    let r = env
        .req(Some("admin"), "PROPFIND", path, &[("depth", "0")], body)
        .await;
    assert!(r.text().contains("404 Not Found"));
    assert_eq!(
        env.req(Some("admin"), "PROPPATCH", "/mokuro-reader/", &[], set)
            .await
            .code(),
        403
    );
    assert_eq!(
        env.req(Some("admin"), "PROPPATCH", "/mokuro-reader/nope", &[], set)
            .await
            .code(),
        404
    );
    assert_eq!(
        env.req(Some("admin"), "PROPPATCH", path, &[], b"<x/>")
            .await
            .code(),
        400
    );
}
