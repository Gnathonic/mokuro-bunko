//! Each account's reading history (`/mokuro-reader/history/<device>/<file>`, 0.8)
//! through the whole server: the auth gate lets any account that saves progress write
//! its own tree, DAV keeps it in `users/<u>/history`, and nobody else sees it.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use std::path::PathBuf;

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    storage: PathBuf,
    _services: Services,
    app: Router,
}

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

const EVENTS: &str = "/mokuro-reader/history/dev-1/2026-10.events";

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library/series")).unwrap();
        std::fs::write(storage.join("library/series/vol1.cbz"), b"volume 1").unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage.clone();
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        for (name, role) in [
            ("reader", Role::Registered),
            ("uploader", Role::Uploader),
            ("processor", Role::Processor),
        ] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            storage,
            _services: services,
            app,
        }
    }

    async fn req(
        &self,
        user: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (StatusCode, Vec<u8>) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080");
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        if !body.is_empty() {
            b = b.header("content-length", body.len().to_string());
        }
        let resp = self
            .app
            .clone()
            .oneshot(b.body(Body::from(body.to_vec())).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn write_history(&self, user: &str, body: &[u8]) {
        for path in ["/mokuro-reader/history", "/mokuro-reader/history/dev-1"] {
            let (status, _) = self.req(Some(user), "MKCOL", path, &[], b"").await;
            assert_eq!(status, StatusCode::CREATED, "{user} MKCOL {path}");
        }
        let (status, _) = self.req(Some(user), "PUT", EVENTS, &[], body).await;
        assert_eq!(status, StatusCode::CREATED, "{user} PUT");
    }
}

#[tokio::test]
async fn a_registered_account_keeps_its_own_history() {
    let env = Env::new();
    env.write_history("reader", b"reader events").await;
    env.write_history("uploader", b"uploader events").await;
    for (user, body) in [("reader", "reader events"), ("uploader", "uploader events")] {
        let (status, got) = env.req(Some(user), "GET", EVENTS, &[], b"").await;
        assert_eq!((status, got.as_slice()), (StatusCode::OK, body.as_bytes()));
        let on_disk = env
            .storage
            .join(format!("users/{user}/history/dev-1/2026-10.events"));
        assert_eq!(std::fs::read(on_disk).unwrap(), body.as_bytes());
    }
    assert!(!env.storage.join("library/history").exists());
    let (status, _) = env.req(None, "GET", EVENTS, &[], b"").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env.req(Some("processor"), "GET", EVENTS, &[], b"").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env
        .req(
            Some("reader"),
            "DELETE",
            "/mokuro-reader/history/dev-1",
            &[],
            b"",
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!env.storage.join("users/reader/history/dev-1").exists());
    assert!(
        env.storage
            .join("users/uploader/history/dev-1/2026-10.events")
            .is_file()
    );
}

#[tokio::test]
async fn others_are_refused() {
    let env = Env::new();
    // Anonymous: 401 (a PUT with the upload verdict).
    let (status, body) = env.req(None, "PUT", EVENTS, &[], b"x").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["ok"],
        false
    );
    let (status, _) = env
        .req(None, "MKCOL", "/mokuro-reader/history", &[], b"")
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // A processor saves no progress.
    for (method, path) in [("MKCOL", "/mokuro-reader/history"), ("PUT", EVENTS)] {
        let (status, _) = env.req(Some("processor"), method, path, &[], b"x").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
    }
    assert!(!env.storage.join("users/processor/history").exists());
    // The history is never rearranged.
    env.write_history("uploader", b"events").await;
    let (status, _) = env
        .req(
            Some("uploader"),
            "MOVE",
            EVENTS,
            &[(
                "destination",
                "http://localhost:8080/mokuro-reader/history/dev-1/b.events",
            )],
            b"",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Shapes the reader never writes.
    let (status, body) = env
        .req(
            Some("uploader"),
            "PUT",
            "/mokuro-reader/history/x.events",
            &[],
            b"x",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap()["ok"],
        false
    );
}

#[tokio::test]
async fn listings_show_only_the_callers_history() {
    let env = Env::new();
    env.write_history("reader", b"events").await;
    let (_, mine) = env
        .req(
            Some("reader"),
            "PROPFIND",
            "/mokuro-reader",
            &[("depth", "infinity")],
            b"",
        )
        .await;
    let mine = String::from_utf8(mine).unwrap();
    assert!(mine.contains("<D:href>/mokuro-reader/history/dev-1/2026-10.events</D:href>"));
    assert!(mine.contains("/mokuro-reader/series/vol1.cbz"));
    for user in [None, Some("uploader")] {
        let (_, theirs) = env
            .req(
                user,
                "PROPFIND",
                "/mokuro-reader",
                &[("depth", "infinity")],
                b"",
            )
            .await;
        let theirs = String::from_utf8(theirs).unwrap();
        assert!(
            theirs.contains("/mokuro-reader/series/vol1.cbz"),
            "{user:?}"
        );
        assert!(!theirs.contains("history"), "{user:?}");
    }
}
