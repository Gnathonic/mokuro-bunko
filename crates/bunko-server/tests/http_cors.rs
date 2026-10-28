//! CORS through the whole server (`assemble`): the reader runs on another origin, so every
//! request it sends is either a preflight answered by the CORS layer or a simple request
//! whose answer must name the reader's origin and expose the headers it reads.

// Fixtures are set up with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_server::app::{ServeOptions, Services, assemble};
use http::{HeaderMap, Request, StatusCode};
use tower::ServiceExt;

struct Env {
    _dir: tempfile::TempDir,
    _services: Services,
    app: Router,
}

const READER: &str = "https://reader.mokuro.app";
const DEV: &str = "http://localhost:5173";

/// Every `X-Mokuro-*` header the reader reads from a response, and `ETag`.
const READER_READS: [&str; 7] = [
    "ETag",
    "X-Mokuro-Put",
    "X-Mokuro-Upload",
    "X-Mokuro-Size",
    "X-Mokuro-Manifest",
    "X-Mokuro-Recheck-After",
    "X-Mokuro-Digest-Verified",
];

fn basic(user: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:pass1234"))
    )
}

impl Env {
    fn new(cors: bool) -> Env {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("library/Series")).unwrap();
        std::fs::write(storage.join("library/Series/Vol.cbz"), b"archive bytes").unwrap();
        let mut config = Config::default();
        config.storage.base_path = storage;
        config.catalog.enabled = true;
        config.cors.enabled = cors;
        let opts = ServeOptions::default();
        let services = Services::new(config, None, &opts).unwrap();
        services
            .db
            .create_user("reader", "pass1234", Role::Uploader, UserStatus::Active, "")
            .unwrap();
        let app = assemble(&services, &opts);
        Env {
            _dir: dir,
            _services: services,
            app,
        }
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap) {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:8080");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let resp = self
            .app
            .clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        (resp.status(), resp.headers().clone())
    }

    /// A browser's preflight for a PUT with the headers the reader sends.
    async fn preflight(&self, path: &str, origin: &str) -> (StatusCode, HeaderMap) {
        self.send(
            "OPTIONS",
            path,
            &[
                ("origin", origin),
                ("access-control-request-method", "PUT"),
                (
                    "access-control-request-headers",
                    "authorization, content-type, if-match, depth",
                ),
            ],
        )
        .await
    }
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> &'a str {
    h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

fn list(h: &HeaderMap, name: &str) -> Vec<String> {
    header(h, name)
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn vary_has_origin(h: &HeaderMap) -> bool {
    h.get_all(http::header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|v| v.trim().eq_ignore_ascii_case("origin"))
}

/// The rules every allowed preflight follows, whatever the path.
fn assert_allowed_preflight(status: StatusCode, h: &HeaderMap, origin: &str) {
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(header(h, "access-control-allow-origin"), origin);
    assert_eq!(header(h, "access-control-allow-credentials"), "true");
    let methods = list(h, "access-control-allow-methods");
    for m in ["propfind", "put", "delete", "mkcol", "move"] {
        assert!(methods.contains(&m.to_string()), "{m} not in {methods:?}");
    }
    let allowed = list(h, "access-control-allow-headers");
    for name in ["authorization", "content-type", "if-match", "depth"] {
        assert!(
            allowed.contains(&name.to_string()),
            "{name} not in {allowed:?}"
        );
    }
    assert_eq!(header(h, "access-control-max-age"), "3600");
    assert!(vary_has_origin(h), "{h:?}");
}

#[tokio::test]
async fn dav_preflight_from_the_reader() {
    let env = Env::new(true);
    let (status, h) = env.preflight("/mokuro-reader/Series/Vol.cbz", READER).await;
    assert_allowed_preflight(status, &h, READER);
    assert_eq!(header(&h, "x-mokuro-put"), "verified");
    assert!(h.get("access-control-allow-private-network").is_none());
}

#[tokio::test]
async fn private_network_access_is_granted_when_asked() {
    let env = Env::new(true);
    let (status, h) = env
        .send(
            "OPTIONS",
            "/mokuro-reader/Series/Vol.cbz",
            &[
                ("origin", READER),
                ("access-control-request-method", "PROPFIND"),
                ("access-control-request-private-network", "true"),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(header(&h, "access-control-allow-private-network"), "true");
}

#[tokio::test]
async fn api_preflights_from_the_dev_server() {
    let env = Env::new(true);
    for path in [
        "/login/api/token",
        "/catalog/api/manifest?series=Series&volume=Vol",
    ] {
        let (status, h) = env.preflight(path, DEV).await;
        assert_allowed_preflight(status, &h, DEV);
        // Not a WebDAV path: no verified-PUT advertisement.
        assert!(h.get("x-mokuro-put").is_none(), "{path}");
    }
}

#[tokio::test]
async fn any_numeric_port_on_loopback_is_allowed() {
    let env = Env::new(true);
    for origin in ["http://localhost:41234", "http://127.0.0.1:8080"] {
        let (status, h) = env.preflight("/login/api/token", origin).await;
        assert_allowed_preflight(status, &h, origin);
    }
}

#[tokio::test]
async fn look_alike_origins_are_refused() {
    let env = Env::new(true);
    for origin in [
        "http://localhost:abc",
        "http://localhost.evil.example:5173",
        "https://evil.example",
    ] {
        let (status, h) = env.preflight("/login/api/token", origin).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{origin}");
        for name in [
            "access-control-allow-origin",
            "access-control-allow-credentials",
            "access-control-allow-methods",
            "access-control-allow-headers",
        ] {
            assert!(h.get(name).is_none(), "{origin}: {name}");
        }
    }
}

/// A refused preflight is still answered by the CORS layer: the app would have asked
/// for credentials (WebDAV `OPTIONS` on the library) or listed the methods it takes.
#[tokio::test]
async fn a_refused_preflight_never_reaches_the_app() {
    let env = Env::new(true);
    let (plain, plain_h) = env
        .send("OPTIONS", "/mokuro-reader/Series/Vol.cbz", &[])
        .await;
    assert_ne!(plain, StatusCode::NO_CONTENT, "{plain_h:?}");
    let (status, h) = env
        .preflight("/mokuro-reader/Series/Vol.cbz", "https://evil.example")
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(h.get("access-control-allow-origin").is_none());
    assert!(h.get("www-authenticate").is_none());
    assert!(h.get("dav").is_none());
    assert!(h.get("allow").is_none());
}

#[tokio::test]
async fn a_simple_request_names_the_origin_and_exposes_what_the_reader_reads() {
    let env = Env::new(true);
    let auth = basic("reader");
    let (status, h) = env
        .send(
            "GET",
            "/login/api/me",
            &[("origin", READER), ("authorization", &auth)],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(header(&h, "access-control-allow-origin"), READER);
    assert_eq!(header(&h, "access-control-allow-credentials"), "true");
    assert!(vary_has_origin(&h));
    let exposed = list(&h, "access-control-expose-headers");
    for name in READER_READS {
        assert!(
            exposed.contains(&name.to_ascii_lowercase()),
            "{name} not in {exposed:?}"
        );
    }
}

#[tokio::test]
async fn a_simple_request_from_another_origin_gets_no_cors_headers() {
    let env = Env::new(true);
    let (status, h) = env
        .send(
            "GET",
            "/login/api/me",
            &[("origin", "https://evil.example")],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(h.get("access-control-allow-origin").is_none());
    assert!(h.get("access-control-expose-headers").is_none());
}

#[tokio::test]
async fn cors_off_sends_no_cors_headers() {
    let env = Env::new(false);
    let (pre_status, pre) = env.preflight("/mokuro-reader/Series/Vol.cbz", READER).await;
    let auth = basic("reader");
    let (status, simple) = env
        .send(
            "GET",
            "/login/api/me",
            &[("origin", READER), ("authorization", &auth)],
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    for h in [&pre, &simple] {
        for name in [
            "access-control-allow-origin",
            "access-control-allow-credentials",
            "access-control-allow-methods",
            "access-control-expose-headers",
            "access-control-max-age",
        ] {
            assert!(h.get(name).is_none(), "{name}: {h:?}");
        }
    }
    // With CORS off the preflight is an ordinary OPTIONS: the app answers it.
    assert_ne!(pre_status, StatusCode::NO_CONTENT, "{pre:?}");
    assert!(pre.get("dav").is_some(), "{pre:?}");
}
