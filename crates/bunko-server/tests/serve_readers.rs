//! `server.serve_readers: false` (0.8 configuration matrix, spec §1): no catalog and no
//! reader WebDAV; admin, setup, login and `/_processor/*` stay, and processors still GET
//! archives under `/mokuro-reader/`. Also: a cloud library passes startup validation
//! without a `library/` folder on disk.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use axum::Router;
use axum::body::Body;
use bunko_core::config::LibraryBackend;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble, validate_startup};
use http::{Request, StatusCode};
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
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

impl Env {
    fn new(serve_readers: bool) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        let library = storage.join("library");
        std::fs::create_dir_all(library.join("Series")).unwrap();
        std::fs::write(library.join("Series/Vol 1.cbz"), b"archive bytes").unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage;
        config.server.serve_readers = serve_readers;
        config.catalog.enabled = true;
        config.catalog.use_as_homepage = true;
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        for (name, role) in [
            ("reader", Role::Registered),
            ("boss", Role::Admin),
            ("gpu", Role::Processor),
        ] {
            services
                .db
                .create_user(name, "pass1234", role, UserStatus::Active, "")
                .unwrap();
        }
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            _services: services,
            app,
        }
    }

    async fn status(&self, user: Option<&str>, method: &str, path: &str) -> StatusCode {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080");
        if let Some(u) = user {
            b = b.header("authorization", basic(u));
        }
        if method == "PROPFIND" {
            b = b.header("depth", "1");
        }
        if path == "/" {
            b = b.header("accept", "text/html");
        }
        let resp = self
            .app
            .clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        resp.status()
    }
}

const ARCHIVE: &str = "/mokuro-reader/Series/Vol%201.cbz";

#[tokio::test]
async fn readers_are_served_by_default() {
    let env = Env::new(true);
    assert_eq!(env.status(None, "GET", ARCHIVE).await, StatusCode::OK);
    assert_eq!(
        env.status(Some("reader"), "GET", ARCHIVE).await,
        StatusCode::OK
    );
    assert_eq!(
        env.status(Some("reader"), "PROPFIND", "/mokuro-reader/")
            .await,
        StatusCode::MULTI_STATUS
    );
    assert_eq!(env.status(None, "GET", "/catalog/").await, StatusCode::OK);
    assert_eq!(env.status(None, "GET", "/").await, StatusCode::FOUND);
}

#[tokio::test]
async fn processors_still_fetch_archives_without_readers() {
    let env = Env::new(false);
    assert_eq!(
        env.status(Some("gpu"), "GET", ARCHIVE).await,
        StatusCode::OK
    );
    assert_eq!(
        env.status(Some("gpu"), "HEAD", ARCHIVE).await,
        StatusCode::OK
    );
    // Only archive reads: no listing, no writes, not even for a processor.
    for (method, path) in [
        ("PROPFIND", "/mokuro-reader/"),
        ("PUT", "/mokuro-reader/Series/Vol%202.cbz"),
        ("DELETE", ARCHIVE),
        ("MKCOL", "/mokuro-reader/New"),
    ] {
        assert_eq!(
            env.status(Some("gpu"), method, path).await,
            StatusCode::FORBIDDEN,
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn reader_routes_are_gone_without_readers() {
    let env = Env::new(false);
    // The reader WebDAV: refused for every non-processor, admin included.
    assert_eq!(
        env.status(None, "GET", ARCHIVE).await,
        StatusCode::UNAUTHORIZED
    );
    for user in ["reader", "boss"] {
        assert_eq!(
            env.status(Some(user), "GET", ARCHIVE).await,
            StatusCode::FORBIDDEN,
            "{user}"
        );
        assert_eq!(
            env.status(Some(user), "PROPFIND", "/mokuro-reader/").await,
            StatusCode::FORBIDDEN,
            "{user}"
        );
        assert_eq!(
            env.status(Some(user), "GET", "/mokuro-reader/volume-data.json")
                .await,
            StatusCode::FORBIDDEN,
            "{user}"
        );
        assert_eq!(
            env.status(Some(user), "GET", "/mokuro-reader/.mokuro-queue.json")
                .await,
            StatusCode::FORBIDDEN,
            "{user}"
        );
    }
    // The rest of WebDAV is not mounted.
    assert_eq!(
        env.status(Some("reader"), "PROPFIND", "/").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        env.status(Some("reader"), "PROPFIND", "/inbox/").await,
        StatusCode::NOT_FOUND
    );
    // The catalog (page, API, manifest) is not mounted either.
    for path in [
        "/catalog",
        "/catalog/",
        "/catalog/api/library",
        "/catalog/api/manifest?series=Series&volume=Vol%201",
    ] {
        assert_eq!(
            env.status(Some("reader"), "GET", path).await,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    // The home page no longer sends browsers to the catalog.
    assert_eq!(env.status(None, "GET", "/").await, StatusCode::OK);
}

#[tokio::test]
async fn admin_and_login_stay_without_readers() {
    let env = Env::new(false);
    assert_eq!(
        env.status(Some("boss"), "GET", "/_admin/api/status").await,
        StatusCode::OK
    );
    assert_eq!(env.status(None, "GET", "/login").await, StatusCode::OK);
    assert_eq!(
        env.status(Some("reader"), "GET", "/login/api/me").await,
        StatusCode::OK
    );
}

/// A cloud library has no `library/` on disk, and startup validation does not ask for one.
#[test]
fn cloud_library_passes_startup_without_a_library_folder() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.base_path = dir.path().join("storage");
    config.storage.library.backend = LibraryBackend::WebDav;
    config.storage.library.webdav.url = Some("https://dav.example/files".into());
    validate_startup(&config).unwrap();
    let layout = config.storage.layout();
    assert!(layout.cloud_library());
    assert!(!layout.library().exists());
    assert!(layout.users().is_dir());
    assert!(layout.inbox().is_dir());

    // A local library still gets (and needs) its folder.
    let mut local = Config::default();
    local.storage.base_path = dir.path().join("local");
    validate_startup(&local).unwrap();
    assert!(local.storage.layout().library().join("thumbnails").is_dir());
}
