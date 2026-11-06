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

impl Env {
    async fn mv(&self, user: &str, from: &str, to: &str) -> u16 {
        self.req(
            Some(user),
            "MOVE",
            from,
            &[("destination", &format!("http://localhost:8080{to}"))],
            b"",
        )
        .await
        .code()
    }

    async fn put_correction(&self, user: &str, path: &str, text: &str) {
        let r = self.req(Some(user), "PUT", path, &[], &layer(text)).await;
        assert!(matches!(r.code(), 201 | 204), "{user} {path}: {}", r.text());
    }
}

/// Corrections follow their archive (B3.4): a rename of the `.cbz` takes every
/// account's along, whatever order the reader moves its files in.
#[tokio::test]
async fn corrections_follow_an_archive_rename() {
    let env = Env::new();
    for user in ["alice", "bob"] {
        env.put_correction(user, EDIT, user).await;
    }
    env.put_correction("alice", "/mokuro-reader/series/vol1.edit-3.mokuro", "a3")
        .await;
    // Another volume's, and a dotted volume's own sidecar, stay where they are.
    std::fs::write(env.lib("series/vol1.5.cbz"), b"x").unwrap();
    env.put_correction("alice", "/mokuro-reader/series/vol1.5.edit.mokuro", "a15")
        .await;
    assert_eq!(
        env.mv(
            "editor",
            "/mokuro-reader/series/vol1.cbz",
            "/mokuro-reader/series/vol9.cbz"
        )
        .await,
        204
    );
    for (user, name, body) in [
        ("alice", "vol9.edit.mokuro", "alice"),
        ("alice", "vol9.edit-3.mokuro", "a3"),
        ("bob", "vol9.edit.mokuro", "bob"),
        ("alice", "vol1.5.edit.mokuro", "a15"),
    ] {
        let path = env.overlay(user, &format!("series/{name}"));
        assert_eq!(std::fs::read(&path).unwrap(), layer(body), "{user} {name}");
    }
    assert!(!env.overlay("alice", "series/vol1.edit.mokuro").exists());
    assert!(!env.overlay("bob", "series/vol1.edit.mokuro").exists());
    // The reader's own MOVE of the old name finds the source gone (404) and the
    // destination already there: it converges.
    assert_eq!(
        env.mv("alice", EDIT, "/mokuro-reader/series/vol9.edit.mokuro")
            .await,
        404
    );
    let got = env
        .req(
            Some("alice"),
            "GET",
            "/mokuro-reader/series/vol9.edit.mokuro",
            &[],
            b"",
        )
        .await;
    assert_eq!((got.code(), got.body), (200, layer("alice")));
}

/// The cloud-root owner's correction is the physical file beside the archive, which
/// nobody else sees: it follows an archive rename someone else makes too (sidecars
/// stay pinned, and the renamer's reader cannot move a file it is not shown).
#[tokio::test]
async fn the_owners_physical_correction_follows_an_archive_rename() {
    let env = Env::new();
    let r = env.as_owner("alice", "PUT", EDIT, &layer("owner")).await;
    assert_eq!(r.code(), 201, "{}", r.text());
    std::fs::write(env.lib("series/vol1.mokuro"), b"{}").unwrap();
    // An orphan already at the new name, and another volume's correction.
    std::fs::write(env.lib("series/vol9.edit.mokuro"), layer("kept")).unwrap();
    std::fs::write(env.lib("series/vol1.5.cbz"), b"x").unwrap();
    std::fs::write(env.lib("series/vol1.5.edit.mokuro"), layer("a15")).unwrap();
    assert_eq!(
        env.mv(
            "editor",
            "/mokuro-reader/series/vol1.cbz",
            "/mokuro-reader/series/vol9.cbz"
        )
        .await,
        204
    );
    for (name, body) in [
        ("vol9.edit.mokuro", "kept"),
        ("vol9.edit-2.mokuro", "owner"),
        ("vol1.5.edit.mokuro", "a15"),
    ] {
        assert_eq!(
            std::fs::read(env.lib(&format!("series/{name}"))).unwrap(),
            layer(body),
            "{name}"
        );
    }
    assert!(!env.lib("series/vol1.edit.mokuro").exists());
    // The shared sidecars stay pinned: the reader moves those itself.
    assert!(env.lib("series/vol1.mokuro").exists());
    let got = env
        .as_owner(
            "alice",
            "GET",
            "/mokuro-reader/series/vol9.edit-2.mokuro",
            b"",
        )
        .await;
    assert_eq!((got.code(), got.body), (200, layer("owner")));
}

/// A correction is stored under one spelling, a plain `.mokuro` (plan §1, critique
/// C18): a MOVE or COPY cannot make the name a PUT would refuse.
#[tokio::test]
async fn a_correction_moves_only_to_a_plain_mokuro_name() {
    let env = Env::new();
    env.put_correction("alice", EDIT, "alice").await;
    for method in ["MOVE", "COPY"] {
        for (to, code) in [
            ("/mokuro-reader/series/vol1.edit-2.mokuro.gz", 403),
            ("/mokuro-reader/series/vol1.edit-2.MOKURO", 409),
        ] {
            let r = env
                .req(
                    Some("alice"),
                    method,
                    EDIT,
                    &[("destination", &format!("http://localhost:8080{to}"))],
                    b"",
                )
                .await;
            assert_eq!(r.code(), code, "{method} {to}: {}", r.text());
        }
    }
    let names: Vec<String> = std::fs::read_dir(env.overlay("alice", "series"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["vol1.edit.mokuro"]);
}

/// A correction already at the destination is kept; the moving one lands under the
/// next free `edit-<n>`, so nothing is lost.
#[tokio::test]
async fn a_colliding_correction_moves_in_as_the_next_free_edit() {
    let env = Env::new();
    env.put_correction("alice", EDIT, "moving").await;
    // An orphan the account left at the new name earlier (no archive there yet).
    std::fs::write(
        env.overlay("alice", "series/vol9.edit.mokuro"),
        layer("kept"),
    )
    .unwrap();
    std::fs::write(
        env.overlay("alice", "series/vol9.edit-2.mokuro"),
        layer("kept 2"),
    )
    .unwrap();
    assert_eq!(
        env.mv(
            "editor",
            "/mokuro-reader/series/vol1.cbz",
            "/mokuro-reader/series/vol9.cbz"
        )
        .await,
        204
    );
    for (name, body) in [
        ("vol9.edit.mokuro", "kept"),
        ("vol9.edit-2.mokuro", "kept 2"),
        ("vol9.edit-3.mokuro", "moving"),
    ] {
        let path = env.overlay("alice", &format!("series/{name}"));
        assert_eq!(std::fs::read(&path).unwrap(), layer(body), "{name}");
    }
}

/// A folder MOVE carries every overlay below it, merging into a folder the account
/// already has corrections in.
#[tokio::test]
async fn corrections_follow_a_folder_move() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("series/inner")).unwrap();
    std::fs::write(env.lib("series/inner/v.cbz"), b"x").unwrap();
    for user in ["alice", "bob"] {
        env.put_correction(user, EDIT, user).await;
    }
    env.put_correction("bob", "/mokuro-reader/series/inner/v.edit.mokuro", "inner")
        .await;
    // Bob already has a correction where the folder is going (an earlier volume).
    std::fs::create_dir_all(env.overlay("bob", "renamed")).unwrap();
    std::fs::write(env.overlay("bob", "renamed/vol1.edit.mokuro"), layer("old")).unwrap();
    assert_eq!(
        env.mv(
            "editor",
            "/mokuro-reader/series/",
            "/mokuro-reader/renamed/"
        )
        .await,
        201
    );
    for (user, rel, body) in [
        ("alice", "renamed/vol1.edit.mokuro", "alice"),
        ("bob", "renamed/vol1.edit.mokuro", "old"),
        ("bob", "renamed/vol1.edit-2.mokuro", "bob"),
        ("bob", "renamed/inner/v.edit.mokuro", "inner"),
    ] {
        assert_eq!(
            std::fs::read(env.overlay(user, rel)).unwrap(),
            layer(body),
            "{user} {rel}"
        );
    }
    assert!(!env.overlay("alice", "series").exists());
    assert!(!env.overlay("bob", "series").exists());
    let got = env
        .req(
            Some("bob"),
            "GET",
            "/mokuro-reader/renamed/inner/v.edit.mokuro",
            &[],
            b"",
        )
        .await;
    assert_eq!((got.code(), got.body), (200, layer("inner")));
}

/// A deleted archive or folder takes every account's corrections of it along; those
/// of another volume stay. A correction PUT in another spelling of the volume reaches
/// the one the account already has: one file per account and volume on every host.
#[tokio::test]
async fn corrections_go_with_their_volume() {
    let env = Env::new();
    std::fs::write(env.lib("series/vol2.cbz"), b"x").unwrap();
    for user in ["alice", "bob"] {
        env.put_correction(user, EDIT, user).await;
    }
    env.put_correction("alice", "/mokuro-reader/series/VOL1.edit.mokuro", "upper")
        .await;
    env.put_correction("alice", "/mokuro-reader/series/vol2.edit.mokuro", "two")
        .await;
    let listed = |user: &str| {
        let dir = env.base.join("users").join(user).join("edits/series");
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    };
    assert_eq!(listed("alice"), ["vol1.edit.mokuro", "vol2.edit.mokuro"]);
    assert_eq!(
        std::fs::read(env.overlay("alice", "series/vol1.edit.mokuro")).unwrap(),
        layer("upper")
    );
    let r = env
        .req(
            Some("editor"),
            "DELETE",
            "/mokuro-reader/series/vol1.cbz",
            &[],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204);
    assert_eq!(listed("alice"), ["vol2.edit.mokuro"]);
    assert!(listed("bob").is_empty());

    let r = env
        .req(Some("editor"), "DELETE", "/mokuro-reader/series/", &[], b"")
        .await;
    assert_eq!(r.code(), 204);
    assert!(!env.overlay("alice", "series").exists());
    assert!(env.base.join("users/alice/edits").exists());
}

/// Auth admits a progress writer to `<Vol>.edit.mokuro` only as that account's own
/// correction, from a folder listing of its own (`DavContext::personal_only`). An
/// archive `<Vol>.edit.cbz` landing before DAV lists the folder again makes the path
/// its shared primary: the write is refused rather than made there on that grant.
#[tokio::test]
async fn a_correction_grant_never_writes_the_shared_primary() {
    let env = Env::new();
    std::fs::write(env.lib("series/vol1.edit.cbz"), b"x").unwrap();
    std::fs::write(env.lib("series/vol1.edit.mokuro"), b"shared").unwrap();
    let granted = |method: &str, headers: &[(&str, &str)], body: &[u8]| {
        let mut ctx = env.ctx(Some("reader"));
        ctx.personal_only = true;
        env.send_ctx(ctx, request(method, EDIT, headers, body))
    };
    let dest = "http://localhost:8080/mokuro-reader/series/vol1.edit-2.mokuro";
    let lock = br#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockinfo>"#;
    let proppatch = br#"<?xml version="1.0"?><D:propertyupdate xmlns:D="DAV:" xmlns:Z="urn:z"><D:set><D:prop><Z:x>1</Z:x></D:prop></D:set></D:propertyupdate>"#;
    for (method, headers, body) in [
        ("PUT", vec![], layer("mine")),
        ("DELETE", vec![], vec![]),
        ("MOVE", vec![("destination", dest)], vec![]),
        ("COPY", vec![("destination", dest)], vec![]),
        ("LOCK", vec![], lock.to_vec()),
        ("PROPPATCH", vec![], proppatch.to_vec()),
    ] {
        let r = granted(method, &headers, &body).await;
        assert_eq!(r.code(), 409, "{method}: {}", r.text());
    }
    assert_eq!(
        std::fs::read(env.lib("series/vol1.edit.mokuro")).unwrap(),
        b"shared"
    );
    assert!(!env.lib("series/vol1.edit-2.mokuro").exists());
    assert!(!env.overlay("reader", "series/vol1.edit.mokuro").exists());
    let r = env.req(Some("editor"), "LOCK", EDIT, &[], lock).await;
    assert_eq!(r.code(), 200, "nobody holds a lock on it: {}", r.text());

    // The same grant on a path that is a correction goes ahead, into the overlay.
    let mut ctx = env.ctx(Some("reader"));
    ctx.personal_only = true;
    let path = "/mokuro-reader/series/vol1.edit-2.mokuro";
    let r = env
        .send_ctx(ctx, request("PUT", path, &[], &layer("mine")))
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    assert!(env.overlay("reader", "series/vol1.edit-2.mokuro").exists());
}

/// Another account's lock on its own correction (even a lock-null one, Timeout:
/// Infinite) is private to it: a series DELETE or MOVE by an editor is never refused
/// for it, nor is its href reported. The lock goes with the folder, as the overlay does.
#[tokio::test]
async fn a_private_correction_lock_never_blocks_a_folder_operation() {
    let env = Env::new();
    let lock = br#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockinfo>"#;
    let orphan = "/mokuro-reader/series/nothing.edit.mokuro";
    for path in [EDIT, orphan] {
        let r = env
            .req(Some("bob"), "LOCK", path, &[("timeout", "Infinite")], lock)
            .await;
        assert!(matches!(r.code(), 200 | 201), "{path}: {}", r.text());
    }
    let dest = "http://localhost:8080/mokuro-reader/renamed";
    let r = env
        .req(
            Some("editor"),
            "MOVE",
            "/mokuro-reader/series",
            &[("destination", dest)],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    assert!(!r.text().contains("edit.mokuro"), "{}", r.text());
    let r = env
        .req(Some("editor"), "DELETE", "/mokuro-reader/renamed", &[], b"")
        .await;
    assert_eq!(r.code(), 204, "{}", r.text());
}

/// The other direction: an editor's depth-infinity folder lock never reaches into
/// another account's own correction. bob keeps his exclusive lock (and with its token,
/// his write) after the editor locks the series, and bob may LOCK and write his own
/// copy while the editor's folder lock is held.
#[tokio::test]
async fn a_folder_lock_never_blocks_an_account_s_own_correction() {
    let env = Env::new();
    let lock = br#"<?xml version="1.0"?><D:lockinfo xmlns:D="DAV:"><D:lockscope><D:exclusive/></D:lockscope><D:locktype><D:write/></D:locktype></D:lockinfo>"#;
    let token = |r: &Resp| {
        r.header("lock-token")
            .unwrap()
            .trim_matches(|c| c == '<' || c == '>')
            .to_string()
    };
    let put = env.req(Some("bob"), "PUT", EDIT, &[], &layer("bob")).await;
    assert_eq!(put.code(), 201, "{}", put.text());
    let r = env.req(Some("bob"), "LOCK", EDIT, &[], lock).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let bob_token = token(&r);
    let bob = format!("(<{bob_token}>)");
    let r = env
        .req(
            Some("editor"),
            "LOCK",
            "/mokuro-reader/series",
            &[("depth", "infinity")],
            lock,
        )
        .await;
    assert_eq!(r.code(), 200, "{}", r.text());
    let editor = token(&r);
    let put = env
        .req(Some("bob"), "PUT", EDIT, &[("if", &bob)], &layer("bob 2"))
        .await;
    assert_eq!(
        put.code(),
        204,
        "bob's own lock still holds: {}",
        put.text()
    );
    let r = env
        .req(
            Some("bob"),
            "UNLOCK",
            EDIT,
            &[("lock-token", &format!("<{bob_token}>"))],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204, "{}", r.text());

    // With the editor's folder lock held, bob writes and LOCKs his own copy.
    let put = env
        .req(Some("bob"), "PUT", EDIT, &[], &layer("bob 3"))
        .await;
    assert_eq!(put.code(), 204, "{}", put.text());
    let r = env.req(Some("bob"), "LOCK", EDIT, &[], lock).await;
    assert_eq!(r.code(), 200, "{}", r.text());
    // The editor's own correction is still under the editor's lock.
    let put = env
        .req(Some("editor"), "PUT", EDIT, &[], &layer("editor"))
        .await;
    assert_eq!(put.code(), 423, "{}", put.text());
    let ifh = format!("(<{editor}>)");
    let put = env
        .req(
            Some("editor"),
            "PUT",
            EDIT,
            &[("if", &ifh)],
            &layer("editor"),
        )
        .await;
    assert_eq!(put.code(), 201, "{}", put.text());
}

/// A library folder respelled outside Bunko (or in the cloud): a `Depth: infinity`
/// listing shows the caller's correction under the folder's listed spelling, exactly as
/// `Depth: 1` of that folder does, on every host.
#[tokio::test]
async fn a_respelled_folders_corrections_list_under_its_spelling() {
    let env = Env::new();
    let r = env.req(Some("alice"), "PUT", EDIT, &[], &layer("a")).await;
    assert_eq!(r.code(), 201, "{}", r.text());
    bunko_store::fs::rename(env.lib("series"), env.lib("Series")).unwrap();
    let hrefs = |depth: &'static str, path: &'static str| {
        let env = &env;
        async move {
            let r = env
                .req(Some("alice"), "PROPFIND", path, &[("depth", depth)], b"")
                .await;
            assert_eq!(r.code(), 207);
            let text = r.text();
            let mut out: Vec<String> = text
                .split("<D:href>")
                .skip(1)
                .filter_map(|s| s.split("</D:href>").next())
                .filter(|h| h.contains("edit.mokuro"))
                .map(str::to_string)
                .collect();
            out.sort();
            out
        }
    };
    let want = ["/mokuro-reader/Series/vol1.edit.mokuro"];
    assert_eq!(hrefs("infinity", "/mokuro-reader/").await, want);
    assert_eq!(hrefs("1", "/mokuro-reader/Series/").await, want);
}

/// A shadowed series' corrections are its own (a case-sensitive disk holding `Kingdom/`
/// and `kingdom/`): nothing reads, overwrites, moves or deletes `edits/kingdom/`.
/// The two folders are a frozen case-variant group: every write into either, the
/// winner included, answers 409 until they are merged.
#[tokio::test]
async fn a_shadowed_series_keeps_its_corrections() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("kingdom")).unwrap();
    std::fs::write(env.lib("kingdom/v.cbz"), b"hidden").unwrap();
    std::fs::create_dir_all(env.overlay("alice", "kingdom")).unwrap();
    let hidden = env.overlay("alice", "kingdom/v.edit.mokuro");
    std::fs::write(&hidden, layer("hidden")).unwrap();
    let _ = std::fs::create_dir_all(env.lib("Kingdom"));
    if !holds_two_spellings(&env.lib(""), "Kingdom", "kingdom") {
        return; // one folder: the correction is that folder's (the other tests)
    }
    std::fs::write(env.lib("Kingdom/v.cbz"), b"winner").unwrap();
    std::fs::write(env.lib("Kingdom/w.cbz"), b"winner").unwrap();
    let got = env
        .req(
            Some("alice"),
            "GET",
            "/mokuro-reader/Kingdom/v.edit.mokuro",
            &[],
            b"",
        )
        .await;
    assert_eq!(got.code(), 404, "{}", got.text());
    let put = env
        .req(
            Some("alice"),
            "PUT",
            "/mokuro-reader/Kingdom/v.edit.mokuro",
            &[],
            &layer("winner"),
        )
        .await;
    assert_eq!(put.code(), 409, "{}", put.text());
    assert_eq!(std::fs::read(&hidden).unwrap(), layer("hidden"));
    let overlays: Vec<String> = std::fs::read_dir(env.overlay("alice", ""))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(overlays, ["kingdom"]);
    let r = env
        .req(
            Some("admin"),
            "DELETE",
            "/mokuro-reader/Kingdom/v.cbz",
            &[],
            b"",
        )
        .await;
    assert_eq!(r.code(), 409, "{}", r.text());
    assert_eq!(
        env.mv("admin", "/mokuro-reader/Kingdom/", "/mokuro-reader/Other/")
            .await,
        409
    );
    assert_eq!(std::fs::read(env.lib("Kingdom/v.cbz")).unwrap(), b"winner");
    assert_eq!(std::fs::read(&hidden).unwrap(), layer("hidden"));
}

/// The cloud-root owner's per-user entries keep their canonical names in the cloud
/// folder, and Bunko never writes one beside another spelling of it (a 0.7 `History/`,
/// a `Goals.json`): the write is refused until that entry is renamed, the same on
/// every host. Without such an entry the owner writes as usual.
#[tokio::test]
async fn the_cloud_owner_never_writes_a_second_spelling_of_a_per_user_name() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("History/dev")).unwrap();
    std::fs::write(env.lib("Goals.json"), b"{}").unwrap();
    for (method, path) in [
        ("MKCOL", "/mokuro-reader/history"),
        ("MKCOL", "/mokuro-reader/history/dev2"),
        ("PUT", "/mokuro-reader/history/dev/x.events"),
        ("PUT", "/mokuro-reader/goals.json"),
        ("PUT", "/mokuro-reader/Goals.json"),
    ] {
        let r = env.as_owner("owner", method, path, b"{}").await;
        assert_eq!(r.code(), 409, "{method} {path}: {}", r.text());
    }
    let mut root: Vec<String> = std::fs::read_dir(env.lib(""))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.eq_ignore_ascii_case("history") || n.eq_ignore_ascii_case("goals.json"))
        .collect();
    root.sort();
    assert_eq!(root, ["Goals.json", "History"]);
    assert_eq!(std::fs::read(env.lib("Goals.json")).unwrap(), b"{}");
    let r = env
        .as_owner("owner", "PUT", "/mokuro-reader/profiles.json", b"{}")
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
}
