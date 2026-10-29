//! Personal correction layers (D27): `<Vol>.edit[-n].mokuro` is one account's own. The
//! DAV tree resolves it per caller: any account's overlay `users/<u>/edits/<library
//! rel>`, or, for the cloud-root owner, the physical file beside the archive. Nobody
//! sees another account's copy, anonymous sees none, and none of it is library content.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod common;

use common::*;

const EDIT: &str = "/mokuro-reader/series/vol1.edit.mokuro";

/// A correction layer: a JSON object with `pages`, and `text` on its one block.
fn layer(text: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "version": "0.2.5",
        "pages": [{"img_path": "001.png", "blocks": [{"lines": [text]}]}],
    }))
    .unwrap()
}

impl Env {
    fn overlay(&self, user: &str, rel: &str) -> std::path::PathBuf {
        self.base.join("users").join(user).join("edits").join(rel)
    }

    async fn as_owner(&self, user: &str, method: &str, path: &str, body: &[u8]) -> Resp {
        let mut ctx = self.ctx(Some(user));
        ctx.owns_cloud_root = true;
        self.send_ctx(ctx, request(method, path, &[], body)).await
    }
}

#[tokio::test]
async fn each_account_reads_and_writes_its_own_correction() {
    let env = Env::new();
    let put = env
        .req(Some("alice"), "PUT", EDIT, &[], &layer("alice"))
        .await;
    assert_eq!(put.code(), 201, "{}", put.text());
    assert_eq!(
        std::fs::read(env.overlay("alice", "series/vol1.edit.mokuro")).unwrap(),
        layer("alice")
    );
    assert!(!env.lib("series/vol1.edit.mokuro").exists());

    assert_eq!(
        env.req(Some("bob"), "GET", EDIT, &[], b"").await.code(),
        404
    );
    assert_eq!(env.req(None, "GET", EDIT, &[], b"").await.code(), 404);
    let put = env.req(Some("bob"), "PUT", EDIT, &[], &layer("bob")).await;
    assert_eq!(put.code(), 201, "{}", put.text());
    let got = env.req(Some("alice"), "GET", EDIT, &[], b"").await;
    assert_eq!((got.code(), got.body), (200, layer("alice")));
    let got = env.req(Some("bob"), "GET", EDIT, &[], b"").await;
    assert_eq!((got.code(), got.body), (200, layer("bob")));

    // An overwrite answers 204 and replaces only the caller's copy.
    let put = env
        .req(Some("bob"), "PUT", EDIT, &[], &layer("bob 2"))
        .await;
    assert_eq!(put.code(), 204);
    assert_eq!(
        std::fs::read(env.overlay("alice", "series/vol1.edit.mokuro")).unwrap(),
        layer("alice")
    );

    // None of it is library content: no upload record, no OCR row forgotten, no
    // provenance hook; audited as the account's own file.
    let calls = env.hooks.calls();
    for call in &calls {
        assert!(
            !call.starts_with("record_volume_upload")
                && !call.starts_with("forget_")
                && call != "library_changed",
            "{calls:?}"
        );
    }
    assert!(env.hooks.staged.lock().is_empty());
    let audits = env.hooks.audits();
    assert!(
        audits
            .iter()
            .all(|a| a.target_type == "personal" && a.target_path == EDIT),
        "{audits:?}"
    );

    // DELETE removes the caller's copy only.
    let del = env.req(Some("alice"), "DELETE", EDIT, &[], b"").await;
    assert_eq!(del.code(), 204);
    assert!(!env.overlay("alice", "series/vol1.edit.mokuro").exists());
    assert!(env.overlay("bob", "series/vol1.edit.mokuro").exists());
    assert!(env.lib("series/vol1.cbz").exists());
}

#[tokio::test]
async fn a_correction_is_checked_before_it_is_stored() {
    let env = Env::new();
    // No archive for it (rule v1: `.cbz` stems only).
    std::fs::write(env.lib("series/vol2.zip"), b"x").unwrap();
    for path in [
        "/mokuro-reader/series/vol2.edit.mokuro",
        "/mokuro-reader/series/nope.edit-2.mokuro",
    ] {
        let r = env.req(Some("alice"), "PUT", path, &[], &layer("x")).await;
        assert_eq!(r.code(), 409, "{path}: {}", r.text());
        assert_eq!(r.json()["reason"], "no-volume");
    }
    let r = env
        .req(
            Some("alice"),
            "PUT",
            "/mokuro-reader/series/vol1.edit.mokuro.gz",
            &[],
            b"gz",
        )
        .await;
    assert_eq!(r.code(), 403);
    // The stored name is the plain lowercase suffix (critique C18).
    let r = env
        .req(
            Some("alice"),
            "PUT",
            "/mokuro-reader/series/vol1.edit.MOKURO",
            &[],
            &layer("x"),
        )
        .await;
    assert_eq!(r.code(), 409, "{}", r.text());
    for body in [
        b"not json".to_vec(),
        b"[]".to_vec(),
        br#"{"version": "x"}"#.to_vec(),
        br#"{"pages": {}}"#.to_vec(),
        br#"{"pages": [], "ocr_engine": {"id": "hayai"}}"#.to_vec(),
    ] {
        let r = env.req(Some("alice"), "PUT", EDIT, &[], &body).await;
        assert_eq!(r.code(), 422, "{}", String::from_utf8_lossy(&body));
        assert_eq!(r.json()["reason"], "not-a-layer");
    }
    // A BOM is tolerated (sidecar names rule 7).
    let mut bom = b"\xEF\xBB\xBF".to_vec();
    bom.extend(layer("bom"));
    let r = env.req(Some("alice"), "PUT", EDIT, &[], &bom).await;
    assert_eq!(r.code(), 201, "{}", r.text());
    // Over 32 MiB, said up front.
    let r = env
        .req(
            Some("alice"),
            "PUT",
            EDIT,
            &[("content-length", "33554433")],
            b"{}",
        )
        .await;
    assert_eq!(r.code(), 413);
    // Nothing refused was left behind.
    let dir = env.overlay("alice", "series");
    let names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["vol1.edit.mokuro"]);
    assert!(!env.overlay("alice", "series/vol2.edit.mokuro").exists());
    assert!(!env.lib("series/vol2.edit.mokuro").exists());
}

#[tokio::test]
async fn conditional_writes_compare_the_callers_copy() {
    let env = Env::new();
    assert_eq!(
        env.req(Some("alice"), "PUT", EDIT, &[], &layer("1"))
            .await
            .code(),
        201
    );
    let etag = env
        .req(Some("alice"), "HEAD", EDIT, &[], b"")
        .await
        .header("etag")
        .unwrap()
        .to_string();
    // Bob has no copy yet: If-Match fails for him.
    let r = env
        .req(
            Some("bob"),
            "PUT",
            EDIT,
            &[("if-match", &etag)],
            &layer("b"),
        )
        .await;
    assert_eq!(r.code(), 412);
    let r = env
        .req(
            Some("alice"),
            "PUT",
            EDIT,
            &[("if-match", &etag)],
            &layer("2"),
        )
        .await;
    assert_eq!(r.code(), 204, "{}", r.text());
    let r = env
        .req(
            Some("alice"),
            "PUT",
            EDIT,
            &[("if-match", &etag)],
            &layer("3"),
        )
        .await;
    assert_eq!(r.code(), 412);
    assert_eq!(
        std::fs::read(env.overlay("alice", "series/vol1.edit.mokuro")).unwrap(),
        layer("2")
    );
}

#[tokio::test]
async fn the_cloud_root_owner_writes_the_physical_file() {
    let env = Env::new();
    // With no owner, a physical correction file is nobody's to see.
    std::fs::write(env.lib("series/vol1.edit-2.mokuro"), layer("stray")).unwrap();
    let stray = "/mokuro-reader/series/vol1.edit-2.mokuro";
    assert_eq!(
        env.req(Some("bob"), "GET", stray, &[], b"").await.code(),
        404
    );
    assert_eq!(env.req(None, "GET", stray, &[], b"").await.code(), 404);

    let r = env.as_owner("alice", "PUT", EDIT, &layer("owner")).await;
    assert_eq!(r.code(), 201, "{}", r.text());
    assert_eq!(
        std::fs::read(env.lib("series/vol1.edit.mokuro")).unwrap(),
        layer("owner")
    );
    assert!(!env.overlay("alice", "series/vol1.edit.mokuro").exists());
    let got = env.as_owner("alice", "GET", EDIT, b"").await;
    assert_eq!((got.code(), got.body), (200, layer("owner")));
    let got = env.as_owner("alice", "GET", stray, b"").await;
    assert_eq!((got.code(), got.body), (200, layer("stray")));
    // Signed in without owning the root, the same account sees its overlay instead.
    assert_eq!(
        env.req(Some("alice"), "GET", EDIT, &[], b"").await.code(),
        404
    );
    assert_eq!(
        env.req(Some("bob"), "GET", EDIT, &[], b"").await.code(),
        404
    );
    // Still the account's own bytes: no upload record, no OCR row, no provenance.
    let calls = env.hooks.calls();
    for call in &calls {
        assert!(
            !call.starts_with("record_volume_upload") && !call.starts_with("forget_"),
            "{calls:?}"
        );
    }
    assert!(env.hooks.staged.lock().is_empty());
}

#[tokio::test]
async fn a_dotted_volumes_primary_is_the_library_file() {
    let env = Env::new();
    std::fs::write(env.lib("series/vol1.edit.cbz"), b"x").unwrap();
    let r = env
        .req(Some("editor"), "PUT", EDIT, &[], b"{\"pages\":[]}")
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    assert!(env.lib("series/vol1.edit.mokuro").exists());
    assert!(!env.overlay("editor", "series/vol1.edit.mokuro").exists());
    assert!(
        env.hooks
            .calls()
            .iter()
            .any(|c| c.starts_with("forget_ocr_sidecar"))
    );
    assert_eq!(env.req(None, "GET", EDIT, &[], b"").await.code(), 200);
}

#[tokio::test]
async fn the_curated_layer_is_checked_like_a_correction() {
    let env = Env::new();
    let path = "/mokuro-reader/series/vol1.curated.mokuro";
    let r = env
        .req(
            Some("editor"),
            "PUT",
            "/mokuro-reader/series/vol9.curated.mokuro",
            &[],
            &layer("c"),
        )
        .await;
    assert_eq!(r.code(), 409);
    let r = env
        .req(Some("editor"), "PUT", &format!("{path}.gz"), &[], b"gz")
        .await;
    assert_eq!(r.code(), 403);
    let r = env
        .req(
            Some("editor"),
            "PUT",
            path,
            &[],
            br#"{"pages":[],"ocr_engine":{}}"#,
        )
        .await;
    assert_eq!(r.code(), 422);
    let r = env.req(Some("editor"), "PUT", path, &[], &layer("c")).await;
    assert_eq!(r.code(), 201, "{}", r.text());
    // Shared: a library file everyone reads.
    assert!(env.lib("series/vol1.curated.mokuro").exists());
    let got = env.req(None, "GET", path, &[], b"").await;
    assert_eq!((got.code(), got.body), (200, layer("c")));
}

#[tokio::test]
async fn corrections_move_only_between_correction_names() {
    let env = Env::new();
    for user in ["alice", "bob"] {
        let r = env.req(Some(user), "PUT", EDIT, &[], &layer(user)).await;
        assert_eq!(r.code(), 201);
    }
    let mv = |user: &'static str, from: &'static str, to: &'static str| {
        let env = &env;
        async move {
            env.req(
                Some(user),
                "MOVE",
                from,
                &[("destination", &format!("http://localhost:8080{to}"))],
                b"",
            )
            .await
            .code()
        }
    };
    // No archive check on a MOVE (the reader moves layers before the archive).
    assert_eq!(
        mv("alice", EDIT, "/mokuro-reader/series/later.edit.mokuro").await,
        204
    );
    assert!(env.overlay("alice", "series/later.edit.mokuro").exists());
    assert!(!env.overlay("alice", "series/vol1.edit.mokuro").exists());
    assert!(env.overlay("bob", "series/vol1.edit.mokuro").exists());
    // Never into or out of a library name.
    assert_eq!(
        mv("bob", EDIT, "/mokuro-reader/series/vol1.hand.mokuro").await,
        403
    );
    std::fs::write(env.lib("series/vol1.hand.mokuro"), b"{}").unwrap();
    assert_eq!(
        mv("bob", "/mokuro-reader/series/vol1.hand.mokuro", EDIT).await,
        403
    );
    assert!(env.lib("series/vol1.hand.mokuro").exists());
    assert_eq!(
        std::fs::read(env.overlay("bob", "series/vol1.edit.mokuro")).unwrap(),
        layer("bob")
    );
    let calls = env.hooks.calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("rename_volume_upload") || c.starts_with("forget_ocr")),
        "{calls:?}"
    );
}

#[tokio::test]
async fn a_folder_lists_the_callers_corrections_beside_their_archive() {
    let env = Env::new();
    for user in ["alice", "bob"] {
        let r = env.req(Some(user), "PUT", EDIT, &[], &layer(user)).await;
        assert_eq!(r.code(), 201);
    }
    // An overlay whose archive is gone is not listed.
    std::fs::create_dir_all(env.overlay("alice", "series")).unwrap();
    std::fs::write(env.overlay("alice", "series/gone.edit.mokuro"), layer("g")).unwrap();
    let list = |user: Option<&'static str>| {
        let env = &env;
        async move {
            let r = env
                .req(
                    user,
                    "PROPFIND",
                    "/mokuro-reader/series/",
                    &[("depth", "1")],
                    b"",
                )
                .await;
            assert_eq!(r.code(), 207);
            r.text()
        }
    };
    let alice = list(Some("alice")).await;
    assert_eq!(alice.matches("vol1.edit.mokuro").count(), 2, "{alice}"); // href + name
    assert!(!alice.contains("gone.edit.mokuro"), "{alice}");
    assert!(list(Some("bob")).await.contains("vol1.edit.mokuro"));
    assert!(!list(Some("carol")).await.contains("edit.mokuro"));
    assert!(!list(None).await.contains("edit.mokuro"));
    let depth0 = env
        .req(Some("alice"), "PROPFIND", EDIT, &[("depth", "0")], b"")
        .await;
    assert_eq!(depth0.code(), 207);
    assert!(depth0.text().contains(&layer("alice").len().to_string()));
}
