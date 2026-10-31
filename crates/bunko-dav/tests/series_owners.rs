//! What DAV does for the server's series owner rules (0.8): it reports the folder
//! changes the owner records follow, asks the auth decision again where an owner rule
//! admitted a write, and never lets a folder stand in for an archive.

#![allow(clippy::disallowed_methods)] // fixtures are written with std::fs

mod common;

use std::sync::Arc;

use bunko_dav::{OwnershipRecheck, Refusal};
use common::{Env, cbz_bytes, request};

fn series_hooks(env: &Env) -> Vec<String> {
    env.hooks
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("series_folder_"))
        .collect()
}

fn refusing() -> OwnershipRecheck {
    Arc::new(|| {
        Err(Refusal {
            message: "Permission denied: this series belongs to another account".into(),
            detail: Some("This series belongs to another account."),
        })
    })
}

#[tokio::test]
async fn only_the_mkcol_that_creates_a_series_folder_reports_it() {
    let env = Env::new();
    let r = env
        .req(Some("uploader"), "MKCOL", "/mokuro-reader/New", &[], b"")
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    assert_eq!(series_hooks(&env), ["series_folder_created New uploader"]);
    // There already: 405, nothing reported.
    let r = env
        .req(Some("editor"), "MKCOL", "/mokuro-reader/New", &[], b"")
        .await;
    assert_eq!(r.code(), 405);
    // A subfolder is no series folder.
    let r = env
        .req(
            Some("uploader"),
            "MKCOL",
            "/mokuro-reader/New/Sub",
            &[],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201);
    assert_eq!(series_hooks(&env), ["series_folder_created New uploader"]);
    // The caller's own history collections are no library folders.
    let r = env
        .req(Some("reader"), "MKCOL", "/mokuro-reader/history", &[], b"")
        .await;
    assert_eq!(r.code(), 201);
    assert_eq!(series_hooks(&env).len(), 1);

    // Two at once: one of them created it.
    let (a, b) = tokio::join!(
        env.req(Some("uploader"), "MKCOL", "/mokuro-reader/Race", &[], b""),
        env.req(Some("editor"), "MKCOL", "/mokuro-reader/Race", &[], b""),
    );
    assert!(
        [a.code(), b.code()].contains(&201),
        "{} {}",
        a.code(),
        b.code()
    );
    let created: Vec<String> = series_hooks(&env)
        .into_iter()
        .filter(|c| c.starts_with("series_folder_created Race "))
        .collect();
    assert_eq!(created.len(), 1, "{created:?}");
}

#[tokio::test]
async fn a_folder_delete_or_move_reports_it() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("other")).unwrap();
    let r = env
        .req(Some("editor"), "DELETE", "/mokuro-reader/series", &[], b"")
        .await;
    assert_eq!(r.code(), 204);
    assert_eq!(series_hooks(&env), ["series_folder_removed series"]);

    std::fs::create_dir_all(env.lib("series")).unwrap();
    let dest = |to: &str| format!("http://localhost:8080/mokuro-reader/{to}");
    let d = dest("renamed");
    let r = env
        .req(
            Some("editor"),
            "MOVE",
            "/mokuro-reader/series",
            &[("destination", d.as_str())],
            b"",
        )
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    // Over an existing folder: it goes first.
    let d = dest("other");
    let r = env
        .req(
            Some("editor"),
            "MOVE",
            "/mokuro-reader/renamed",
            &[("destination", d.as_str())],
            b"",
        )
        .await;
    assert_eq!(r.code(), 204, "{}", r.text());
    assert_eq!(
        series_hooks(&env),
        [
            "series_folder_removed series",
            "series_folder_moved series renamed",
            "series_folder_removed other",
            "series_folder_moved renamed other",
        ]
    );
}

/// A write admitted by an owner rule is decided again where it lands: a refusal then
/// is the PUT's 403 verdict (with its detail) and stores nothing, staging included.
#[tokio::test]
async fn a_refused_recheck_stores_nothing() {
    let env = Env::new();
    let mut ctx = env.ctx(Some("uploader"));
    ctx.ownership_recheck = Some(refusing());
    let path = "/mokuro-reader/series/vol1.curated.cbz";
    let r = env
        .send_ctx(ctx.clone(), request("PUT", path, &[], &cbz_bytes(2)))
        .await;
    assert_eq!(r.code(), 403, "{}", r.text());
    assert_eq!(
        r.json(),
        serde_json::json!({
            "ok": false,
            "reason": "forbidden",
            "detail": "This series belongs to another account.",
            "retry": false
        })
    );
    let left: Vec<String> = std::fs::read_dir(env.lib("series"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["vol1.cbz"], "nothing stored, no staging left");
    assert!(
        !env.hooks
            .calls()
            .iter()
            .any(|c| c.starts_with("record_volume_upload"))
    );

    // A DELETE asks before anything goes.
    let r = env
        .send_ctx(
            ctx.clone(),
            request("DELETE", "/mokuro-reader/series/vol1.cbz", &[], b""),
        )
        .await;
    assert_eq!(r.code(), 403, "{}", r.text());
    assert!(env.lib("series/vol1.cbz").exists());
    let r = env
        .send_ctx(ctx, request("DELETE", "/mokuro-reader/series", &[], b""))
        .await;
    assert_eq!(r.code(), 403);
    assert!(env.lib("series").exists());

    // A recheck that still agrees lets it through.
    let mut ctx = env.ctx(Some("uploader"));
    ctx.ownership_recheck = Some(Arc::new(|| Ok(())));
    let r = env
        .send_ctx(ctx, request("PUT", path, &[], &cbz_bytes(2)))
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
}

/// A folder is no archive (rule 2): one named `vol1.edit.cbz` makes no
/// `vol1.edit.mokuro` its primary, so that name stays each account's own correction.
#[tokio::test]
async fn a_folder_named_like_an_archive_claims_no_sidecar() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("series/vol1.edit.cbz")).unwrap();
    let body = br#"{"pages":[]}"#;
    let r = env
        .req(
            Some("reader"),
            "PUT",
            "/mokuro-reader/series/vol1.edit.mokuro",
            &[],
            body,
        )
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    assert!(
        env.base
            .join("users/reader/edits/series/vol1.edit.mokuro")
            .exists()
    );
    assert!(!env.lib("series/vol1.edit.mokuro").exists());
}

/// The listing agrees (F2): beside a folder named `vol1.edit.cbz`, the cloud-root
/// owner's physical `vol1.edit.mokuro` is still a correction, listed to nobody else,
/// and each account still sees its own.
#[tokio::test]
async fn a_folder_named_like_an_archive_hides_no_correction_in_a_listing() {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("series/vol1.edit.cbz")).unwrap();
    std::fs::write(env.lib("series/vol1.edit.mokuro"), br#"{"pages":[]}"#).unwrap();
    let body = br#"{"pages":[{"img_path":"1.png","blocks":[]}]}"#;
    let r = env
        .req(
            Some("reader"),
            "PUT",
            "/mokuro-reader/series/vol1.edit.mokuro",
            &[],
            body,
        )
        .await;
    assert_eq!(r.code(), 201, "{}", r.text());
    let list = |user: &'static str| {
        let env = &env;
        async move {
            let r = env
                .req(
                    Some(user),
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
    let other = list("uploader").await;
    assert!(!other.contains("vol1.edit.mokuro"), "{other}");
    let own = list("reader").await;
    assert!(own.contains("vol1.edit.mokuro"), "{own}");
    assert!(own.contains(&format!(">{}<", body.len())), "{own}");
}
