//! `PUT /mokuro-reader/<Series>/series.json`: the client fact update.

mod library_support;

use axum::body::Body;
use bunko_core::Role;
use library_support::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const UPDATE: &str = r#"{"version":2,"series_title":"ignored","updated_at":"2026-08-18T19:36:24.324Z","external_ids":{"anilist":98416,"mal":103897},"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},"synonyms":["ドクターストーン"],"tag":" HD Scan ","unit":"volumes","spine_offset":12.5}"#;

fn put(uri: &str, user: Option<&str>, body: &'static str) -> http::Request<Body> {
    let mut b = req("PUT", uri).header("content-length", body.len().to_string());
    if let Some(user) = user {
        b = b.header("authorization", basic(user));
    }
    b.body(Body::from(body)).unwrap()
}

async fn library_env() -> Env {
    let env = Env::new(|_| {});
    let dir = env.series_dir("Dr Stone");
    write_cbz(&dir.join("Volume 01.cbz"), 2);
    env.user("editor", Role::Editor);
    env.user("registered", Role::Registered);
    env.user("uploader", Role::Uploader);
    assert!(env.runtime.recompile_all_now().await > 0);
    env
}

fn audit_actions(env: &Env) -> Vec<(String, Option<String>, Option<String>)> {
    env.db
        .list_audit_events(50, None)
        .unwrap()
        .into_iter()
        .filter(|e| e.action.starts_with("metadata_"))
        .map(|e| (e.action, e.target_path, e.details))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accepted_update_republishes_the_series_and_catalog() {
    let env = library_env().await;
    let r = env
        .send(put(
            "/mokuro-reader/Dr%20Stone/series.json",
            Some("editor"),
            UPDATE,
        ))
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    assert!(r.body.is_empty());
    assert!(r.header("content-type").is_none());

    let series_file = env.library.join("Dr Stone/series.json");
    let text = std::fs::read_to_string(&series_file).unwrap();
    assert!(
        text.starts_with(r#"{"version":2,"series_title":"Dr Stone","external_ids":{"anilist":98416,"mal":103897},"titles":{"native":"Dr.STONE","romaji":"Dr. STONE"},"synonyms":["ドクターストーン"],"tag":"HD Scan","unit":"volumes","spine_offset":12.5,"updated_at":"2026-08-18T19:36:24.324Z","volumes":["#),
        "{text}"
    );
    let catalog = std::fs::read_to_string(env.library.join("catalog.json")).unwrap();
    assert!(
        catalog.contains(
            r#""updated_at":"2026-08-18T19:36:24.324Z","series":[{"series_title":"Dr Stone""#
        ),
        "{catalog}"
    );
    let facts = env.db.get_series_facts("dr stone").unwrap().expect("row");
    assert_eq!(facts.series_title, "Dr Stone");
    assert_eq!(facts.updated_by.as_deref(), Some("editor"));
    assert_eq!(
        audit_actions(&env),
        vec![(
            "metadata_update".into(),
            Some("/mokuro-reader/Dr Stone/series.json".into()),
            Some(r#"{"accepted":true}"#.into())
        )]
    );

    // The same PUT again: accepted, nothing rewritten.
    let before = std::fs::metadata(&series_file).unwrap().modified().unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let again = env
        .send(put(
            "/mokuro-reader/Dr%20Stone/series.json",
            Some("editor"),
            UPDATE,
        ))
        .await;
    assert_eq!(again.status, 204);
    assert_eq!(
        std::fs::metadata(&series_file).unwrap().modified().unwrap(),
        before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_and_malformed_requests() {
    let env = library_env().await;
    let uri = "/mokuro-reader/Dr%20Stone/series.json";

    let bad = env.send(put(uri, Some("editor"), "not json")).await;
    assert_eq!(
        (bad.status, bad.text().as_str()),
        (400, "Invalid metadata update")
    );
    assert_eq!(
        bad.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(audit_actions(&env)[0].0, "metadata_rejected");
    assert_eq!(
        audit_actions(&env)[0].2.as_deref(),
        Some(r#"{"accepted":false}"#)
    );

    let no_folder = env
        .send(put(
            "/mokuro-reader/Nope/series.json",
            Some("editor"),
            UPDATE,
        ))
        .await;
    assert_eq!(no_folder.status, 400);

    let no_length = env
        .send(
            req("PUT", uri)
                .header("authorization", basic("editor"))
                .body(Body::from(UPDATE))
                .unwrap(),
        )
        .await;
    assert_eq!(
        (no_length.status, no_length.text().as_str()),
        (411, "Content-Length required")
    );

    let garbage = env
        .send(
            req("PUT", uri)
                .header("authorization", basic("editor"))
                .header("content-length", "not a number")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        (garbage.status, garbage.text().as_str()),
        (400, "Invalid Content-Length")
    );

    let huge = env
        .send(
            req("PUT", uri)
                .header("authorization", basic("editor"))
                .header("content-length", (4 * 1024 * 1024 + 1).to_string())
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(
        (huge.status, huge.text().as_str()),
        (413, "Metadata update too large")
    );

    let anonymous = env.send(put(uri, None, UPDATE)).await;
    assert_eq!(anonymous.status, 401);
    let registered = env.send(put(uri, Some("registered"), UPDATE)).await;
    assert_eq!(
        (registered.status, registered.text().as_str()),
        (
            403,
            "Permission denied: cannot submit metadata updates for this series"
        )
    );
    // An uploader who uploaded every volume of the series may edit it.
    let uploader = env.send(put(uri, Some("uploader"), UPDATE)).await;
    assert_eq!(uploader.status, 403);
    env.db
        .record_volume_upload("Dr Stone/Volume 01.cbz", "uploader")
        .unwrap();
    let owner = env.send(put(uri, Some("uploader"), UPDATE)).await;
    assert_eq!(owner.status, 204, "{}", owner.text());

    // Called without an identity (a wiring mistake), the handler itself refuses.
    let ctx = bunko_server::RequestCtx {
        peer: "127.0.0.1".parse().unwrap(),
        client_ip: "127.0.0.1".into(),
        identity: Default::default(),
    };
    let resp = bunko_server::library::series_put(&env.deps, put(uri, None, UPDATE), ctx).await;
    assert_eq!(resp.status(), 401);
}

/// A DAV lock table whose acquisition stalls while `slow` is set: holds the compiler's
/// pass lock long enough for a PUT to time out.
#[derive(Default)]
struct SlowLocks {
    slow: AtomicBool,
    entered: AtomicBool,
}

impl bunko_library::PathWriteLocks for SlowLocks {
    fn try_lock(&self, _path: &Path) -> Option<Box<dyn Send>> {
        if self.slow.load(Ordering::SeqCst) {
            self.entered.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(1500));
        }
        Some(Box::new(()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_compiler_answers_503_with_retry_after() {
    let locks = Arc::new(SlowLocks::default());
    let mut policy = fast_policy();
    policy.update_lock_timeout = Duration::from_millis(200);
    let env = Env::with(
        |_| {},
        Options {
            locks: Some(locks.clone()),
            policy,
            ..Options::default()
        },
    );
    let dir = env.series_dir("Dr Stone");
    write_cbz(&dir.join("Volume 01.cbz"), 2);
    env.user("editor", Role::Editor);
    assert!(env.runtime.recompile_all_now().await > 0);

    // A new volume makes the next pass write, and the write stalls in the lock table.
    write_cbz(&dir.join("Volume 02.cbz"), 2);
    locks.slow.store(true, Ordering::SeqCst);
    let runtime = env.runtime.clone();
    let pass = tokio::spawn(async move { runtime.recompile_all_now().await });
    assert!(
        wait_for(Duration::from_secs(5), || locks
            .entered
            .load(Ordering::SeqCst))
        .await
    );

    let r = env
        .send(put(
            "/mokuro-reader/Dr%20Stone/series.json",
            Some("editor"),
            UPDATE,
        ))
        .await;
    assert_eq!(r.status, 503);
    assert_eq!(r.text(), "Server is busy compiling metadata; retry shortly");
    assert_eq!(r.header("retry-after"), Some("30"));
    // Busy answers are not audited (0.5.2 returned before the audit).
    assert!(audit_actions(&env).is_empty());

    locks.slow.store(false, Ordering::SeqCst);
    assert!(pass.await.unwrap() > 0);
    let retry = env
        .send(put(
            "/mokuro-reader/Dr%20Stone/series.json",
            Some("editor"),
            UPDATE,
        ))
        .await;
    assert_eq!(retry.status, 204);
}
