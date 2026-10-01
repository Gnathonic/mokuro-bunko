//! Ported from `tests/unit/test_upload_verdicts.py` and `tests/unit/test_atomic_writes_locks.py`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use bytes::Bytes;
use common::*;
use http::Request;

const TARGET: &str = "/mokuro-reader/S/V1.cbz";

fn env() -> Env {
    let env = Env::new();
    std::fs::create_dir_all(env.lib("S")).unwrap();
    env
}

async fn put(env: &Env, path: &str, body: &[u8], headers: &[(&str, &str)]) -> Resp {
    env.req(Some("uploader"), "PUT", path, headers, body).await
}

#[tokio::test]
async fn a_new_archive_is_verified_and_a_replace_is_204() {
    let env = env();
    let body = cbz_bytes(5);
    let r = put(&env, TARGET, &body, &[]).await;
    assert_eq!(r.code(), 201);
    assert_eq!(r.header("x-mokuro-upload"), Some("verified"));
    assert_eq!(
        r.header("x-mokuro-size"),
        Some(body.len().to_string().as_str())
    );
    assert_eq!(std::fs::read(env.lib("S/V1.cbz")).unwrap(), body);
    let body = cbz_bytes(7);
    let r = put(&env, TARGET, &body, &[]).await;
    assert_eq!(r.code(), 204);
    assert_eq!(r.header("x-mokuro-upload"), Some("verified"));
    assert_eq!(std::fs::read(env.lib("S/V1.cbz")).unwrap(), body);
    assert!(leftovers(&env.lib("S")).is_empty());
    assert!(env.hooks.has("record_volume_upload S/V1.cbz uploader true"));
    assert!(env.hooks.has("audit edit library /mokuro-reader/S/V1.cbz"));
}

#[tokio::test]
async fn a_truncated_body() {
    let env = env();
    let body = cbz_bytes(5);
    let len = body.len().to_string();
    let r = put(
        &env,
        TARGET,
        &body[..body.len() / 2],
        &[("content-length", &len)],
    )
    .await;
    assert_eq!(r.code(), 422);
    assert_eq!(r.header("content-type"), Some("application/json"));
    let v = r.json();
    assert_eq!(v["ok"], false);
    assert_eq!(v["reason"], "truncated");
    assert_eq!(v["retry"], true);
    assert!(v["detail"].as_str().unwrap().contains(&len));
    assert!(
        r.text()
            .starts_with("{\"ok\": false, \"reason\": \"truncated\", \"detail\": ")
    );
    assert!(!env.lib("S/V1.cbz").exists());
    assert!(leftovers(&env.lib("S")).is_empty());
}

#[tokio::test]
async fn a_damaged_zip_then_the_same_damage_again() {
    let env = env();
    let first = put(&env, TARGET, &damaged_cbz(), &[]).await;
    assert_eq!(first.code(), 422);
    let v = first.json();
    assert_eq!(
        (v["reason"].as_str().unwrap(), v["retry"].as_bool().unwrap()),
        ("archive-damaged", true)
    );
    assert!(
        v["detail"]
            .as_str()
            .unwrap()
            .contains("'001.jpg' fails its CRC-32 check")
    );
    assert!(!env.lib("S/V1.cbz").exists());
    let second = put(&env, TARGET, &damaged_cbz(), &[]).await.json();
    assert_eq!(
        (
            second["reason"].as_str().unwrap(),
            second["retry"].as_bool().unwrap()
        ),
        ("archive-damaged", false)
    );
    assert!(second["detail"].as_str().unwrap().contains("twice"));
    // Different damage is worth another try; the same damage elsewhere is a first.
    let other = put(&env, TARGET, &damaged_cbz_other(), &[]).await.json();
    assert_eq!(other["retry"], true);
    assert!(other["detail"].as_str().unwrap().contains("000.jpg"));
    let elsewhere = put(&env, "/mokuro-reader/S/V2.cbz", &damaged_cbz(), &[])
        .await
        .json();
    assert_eq!(elsewhere["retry"], true);
}

#[tokio::test]
async fn a_zip_cut_short_with_no_length_and_not_a_zip() {
    let env = env();
    let body = cbz_bytes(5);
    let r = put(
        &env,
        TARGET,
        &body[..body.len() - 30],
        &[("content-length", "")],
    )
    .await;
    assert_eq!(r.code(), 422);
    assert_eq!(r.json()["reason"], "archive-damaged");
    let r = put(
        &env,
        TARGET,
        "<html>this is a login page</html>".repeat(20).as_bytes(),
        &[],
    )
    .await;
    assert_eq!(r.code(), 422);
    let v = r.json();
    assert_eq!(
        (v["reason"].as_str().unwrap(), v["retry"].as_bool().unwrap()),
        ("not-an-archive", false)
    );
    assert_eq!(
        v["detail"],
        "The upload is not a zip archive, so it cannot be a .cbz."
    );
    assert_eq!(r.header("x-mokuro-put"), Some("verified"));
    // An empty body is not an archive either.
    assert_eq!(
        put(&env, TARGET, b"", &[]).await.json()["reason"],
        "not-an-archive"
    );
}

#[tokio::test]
async fn a_failed_replace_keeps_the_old_file() {
    for kind in ["truncated", "damaged", "not-zip"] {
        let env = env();
        let old = cbz_bytes(3);
        std::fs::write(env.lib("S/V1.cbz"), &old).unwrap();
        let body = cbz_bytes(9);
        let r = match kind {
            "truncated" => {
                put(
                    &env,
                    TARGET,
                    &body[..100],
                    &[("content-length", &body.len().to_string())],
                )
                .await
            }
            "damaged" => put(&env, TARGET, &damaged_cbz(), &[]).await,
            _ => put(&env, TARGET, &b"garbage".repeat(100), &[]).await,
        };
        assert_eq!(r.code(), 422, "{kind}");
        assert_eq!(std::fs::read(env.lib("S/V1.cbz")).unwrap(), old, "{kind}");
        assert!(leftovers(&env.lib("S")).is_empty(), "{kind}");
        assert!(
            !env.hooks
                .calls()
                .iter()
                .any(|c| c.starts_with("record_volume_upload")),
            "{kind}"
        );
    }
}

#[tokio::test]
async fn other_files_are_stored_and_size_checked() {
    let env = env();
    let body = br#"{"pages": []}"#;
    let r = put(&env, "/mokuro-reader/S/V1.mokuro", body, &[]).await;
    assert_eq!(r.code(), 201);
    assert_eq!(r.header("x-mokuro-upload"), Some("stored"));
    assert_eq!(
        r.header("x-mokuro-size"),
        Some(body.len().to_string().as_str())
    );
    assert!(r.header("x-mokuro-put").is_none());
    assert!(env.hooks.has("forget_ocr_sidecar S/V1.mokuro"));
    let r = put(
        &env,
        "/mokuro-reader/S/V1.mokuro",
        br#"{"pages": ["#,
        &[("content-length", "500")],
    )
    .await;
    assert_eq!(r.code(), 422);
    assert_eq!(r.json()["reason"], "truncated");
    assert_eq!(std::fs::read(env.lib("S/V1.mokuro")).unwrap(), body);
}

#[tokio::test]
async fn content_digest() {
    let env = env();
    std::fs::write(env.lib("S/V1.cbz"), b"old").unwrap();
    // Mismatch: corrupted in transit, CRCs never consulted.
    let r = put(
        &env,
        TARGET,
        &damaged_cbz(),
        &[("content-digest", &digest_header(&cbz_bytes(5), "sha-256"))],
    )
    .await;
    assert_eq!(r.code(), 422);
    let v = r.json();
    assert_eq!(
        (v["reason"].as_str().unwrap(), v["retry"].as_bool().unwrap()),
        ("corrupted-in-transit", true)
    );
    assert!(!v["detail"].as_str().unwrap().contains("001.jpg"));
    assert_eq!(std::fs::read(env.lib("S/V1.cbz")).unwrap(), b"old");
    // A match with bad CRCs: the client's copy.
    let bad = damaged_cbz();
    let v = put(
        &env,
        TARGET,
        &bad,
        &[("content-digest", &digest_header(&bad, "sha-256"))],
    )
    .await
    .json();
    assert_eq!(
        (v["reason"].as_str().unwrap(), v["retry"].as_bool().unwrap()),
        ("archive-damaged", false)
    );
    assert!(
        v["detail"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("re-import this volume")
    );
    for algo in ["sha-256", "sha-512"] {
        let body = cbz_bytes(4);
        let r = put(
            &env,
            TARGET,
            &body,
            &[("content-digest", &digest_header(&body, algo))],
        )
        .await;
        assert!(r.code() == 201 || r.code() == 204);
        assert_eq!(r.header("x-mokuro-digest-verified"), Some(algo));
    }
    let body = cbz_bytes(4);
    let mixed = format!(
        "md5=:AAAAAAAAAAAAAAAAAAAAAA==:, {}",
        digest_header(&body, "sha-256")
    );
    assert_eq!(
        put(&env, TARGET, &body, &[("content-digest", &mixed)])
            .await
            .header("x-mokuro-digest-verified"),
        Some("sha-256")
    );
    let r = put(
        &env,
        TARGET,
        &body,
        &[("content-digest", "md5=:AAAAAAAAAAAAAAAAAAAAAA==:")],
    )
    .await;
    assert!(r.header("x-mokuro-digest-verified").is_none());
    for bad in [
        "sha-256=abc",
        "sha-256=:not base64!:",
        "sha-256=:AAAA:",
        "sha-256",
    ] {
        let r = put(&env, TARGET, &body, &[("content-digest", bad)]).await;
        assert_eq!(r.header("x-mokuro-upload"), Some("verified"), "{bad}");
        assert!(r.header("x-mokuro-digest-verified").is_none());
    }
    // Non-archives are digest-checked too.
    let r = put(
        &env,
        "/mokuro-reader/S/V1.mokuro",
        br#"{"pages": [1]}"#,
        &[(
            "content-digest",
            &digest_header(br#"{"pages": [2]}"#, "sha-256"),
        )],
    )
    .await;
    assert_eq!(r.json()["reason"], "corrupted-in-transit");
}

#[tokio::test]
async fn inflation_bound_and_methods() {
    let env = env();
    let bomb = declaring(cbz_bytes(3), 3 * 1024 * 1024 * 1024);
    let v = put(&env, TARGET, &bomb, &[]).await.json();
    assert_eq!(
        (v["reason"].as_str().unwrap(), v["retry"].as_bool().unwrap()),
        ("archive-refused", false)
    );
    assert!(v["detail"].as_str().unwrap().contains("GiB"), "{v}");
    assert!(!env.lib("S/V1.cbz").exists());
    let v = put(&env, TARGET, &bzip2_cbz(), &[]).await.json();
    assert_eq!(v["reason"], "archive-refused");
    assert!(v["detail"].as_str().unwrap().contains("bzip2"), "{v}");
    assert_eq!(
        put(&env, TARGET, &cbz_bytes(20), &[])
            .await
            .header("x-mokuro-upload"),
        Some("verified")
    );
}

#[tokio::test]
async fn refusals_from_the_server_become_verdicts() {
    let mut resp = http::Response::new(Body::from("Authentication required"));
    *resp.status_mut() = http::StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        "www-authenticate",
        "Basic realm=\"mokuro-bunko\", charset=\"UTF-8\""
            .parse()
            .unwrap(),
    );
    let r = Resp::read(bunko_dav::put_refusal_verdict(TARGET, resp)).await;
    assert_eq!(r.code(), 401);
    assert!(r.header("www-authenticate").is_some());
    assert_eq!(r.header("x-mokuro-put"), Some("verified"));
    assert_eq!(
        r.text(),
        r#"{"ok": false, "reason": "forbidden", "detail": "Sign in to upload.", "retry": false}"#
    );
    let mut resp = http::Response::new(Body::from("no"));
    *resp.status_mut() = http::StatusCode::FORBIDDEN;
    let r = Resp::read(bunko_dav::put_refusal_verdict(
        "/mokuro-reader/S/series.json",
        resp,
    ))
    .await;
    assert_eq!(r.text(), "no");
}

/// A body the test feeds by hand.
fn channel_body() -> (
    tokio::sync::mpsc::Sender<Result<Bytes, std::io::Error>>,
    Body,
) {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    (tx, Body::from_stream(stream))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_get_during_an_upload_sees_the_old_file_and_writes_conflict() {
    let env = Arc::new(env());
    let old = cbz_bytes(2);
    std::fs::write(env.lib("S/V1.cbz"), &old).unwrap();
    let new = cbz_bytes(8);
    let half = new.len() / 2;
    let (tx, body) = channel_body();
    let req = Request::builder()
        .method("PUT")
        .uri(TARGET)
        .header("content-length", new.len().to_string())
        .body(body)
        .unwrap();
    let env2 = env.clone();
    let upload = tokio::spawn(async move { env2.send(Some("uploader"), req).await });
    tx.send(Ok(Bytes::copy_from_slice(&new[..half])))
        .await
        .unwrap();
    // Wait until the staging file exists (the upload holds the path lock).
    for _ in 0..200 {
        if !leftovers(&env.lib("S")).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        env.req(Some("uploader"), "GET", TARGET, &[], b"")
            .await
            .body,
        old
    );
    // Concurrent writers of the same path (or its folder) get 423.
    let r = env
        .req(Some("uploader"), "PUT", TARGET, &[], &cbz_bytes(1))
        .await;
    assert_eq!(r.code(), 423);
    assert_eq!(r.json()["retry"], true);
    assert_eq!(
        env.req(Some("admin"), "DELETE", TARGET, &[], b"")
            .await
            .code(),
        423
    );
    assert_eq!(
        env.req(Some("admin"), "DELETE", "/mokuro-reader/S", &[], b"")
            .await
            .code(),
        423
    );
    let mv = env
        .req(
            Some("admin"),
            "MOVE",
            "/mokuro-reader/manga1.cbz",
            &[(
                "destination",
                "http://localhost:8080/mokuro-reader/S/V1.cbz",
            )],
            b"",
        )
        .await;
    assert_eq!(mv.code(), 423);
    assert!(env.lib("manga1.cbz").exists());
    assert!(
        env.hooks
            .calls()
            .iter()
            .any(|c| c == "audit lock_conflict library /mokuro-reader/S/V1.cbz")
    );
    let conflict = env
        .hooks
        .audits()
        .into_iter()
        .find(|a| a.action == "lock_conflict")
        .unwrap();
    assert_eq!(
        conflict.details,
        Some(serde_json::json!({ "operation": "write" }))
    );
    tx.send(Ok(Bytes::copy_from_slice(&new[half..])))
        .await
        .unwrap();
    drop(tx);
    let r = upload.await.unwrap();
    assert_eq!(r.code(), 204);
    assert_eq!(std::fs::read(env.lib("S/V1.cbz")).unwrap(), new);
    assert_eq!(env.dav.write_locks().held_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_upload_leaves_nothing_and_releases_the_lock() {
    let env = Arc::new(env());
    std::fs::write(env.lib("S/notes.txt"), b"original").unwrap();
    let (tx, body) = channel_body();
    let req = Request::builder()
        .method("PUT")
        .uri("/mokuro-reader/S/notes.txt")
        .header("content-length", "100")
        .body(body)
        .unwrap();
    let env2 = env.clone();
    let upload = tokio::spawn(async move { env2.send(Some("admin"), req).await });
    tx.send(Ok(Bytes::from_static(b"partial garbage")))
        .await
        .unwrap();
    tx.send(Err(std::io::Error::other("client went away")))
        .await
        .unwrap();
    let r = upload.await.unwrap();
    assert_eq!(r.code(), 422);
    assert_eq!(r.json()["reason"], "truncated");
    assert_eq!(std::fs::read(env.lib("S/notes.txt")).unwrap(), b"original");
    assert!(leftovers(&env.lib("S")).is_empty());
    assert_eq!(env.dav.write_locks().held_count(), 0);
    assert_eq!(
        env.req(
            Some("admin"),
            "PUT",
            "/mokuro-reader/S/notes.txt",
            &[],
            b"clean"
        )
        .await
        .code(),
        204
    );
}

#[test]
fn path_write_locks_are_shared_with_other_writers() {
    let env = env();
    let dav = env.dav.clone();
    let guard = dav.write_locks().try_lock(&env.lib("S")).unwrap();
    assert!(dav.write_locks().try_lock(&env.lib("s/v1.CBZ")).is_none());
    drop(guard);
    assert!(dav.write_locks().try_lock(&env.lib("S/V1.cbz")).is_some());
}
