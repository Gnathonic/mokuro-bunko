//! Shared harness for the `accounts_*` integration tests: a temp storage, a real
//! database (cheap bcrypt), and the accounts router in front of a stand-in WebDAV
//! fallback that answers 418 `dav`.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use base64::Engine as _;
use bunko_core::{Config, Role};
use bunko_db::{Database, DbOptions, UserStatus};
use bunko_server::Core;
use bunko_server::accounts::{self, AccountsDeps};
use bunko_server::backend::DbAuthBackend;
use http::{HeaderMap, Request, StatusCode};
use parking_lot::RwLock;
use serde_json::Value;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

pub struct Env {
    pub dir: tempfile::TempDir,
    pub db: Arc<Database>,
    pub deps: AccountsDeps,
}

impl Env {
    pub fn new() -> Env {
        Env::with_config(|_| {})
    }

    pub fn with_config(edit: impl FnOnce(&mut Config)) -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.storage.base_path = dir.path().join("storage");
        edit(&mut config);
        let layout = config.storage.layout();
        layout.ensure_directories().expect("dirs");
        let db = Arc::new(
            Database::open_with(
                layout.database(),
                &DbOptions {
                    bcrypt_cost: 4,
                    ..DbOptions::default()
                },
            )
            .expect("db"),
        );
        let backend = Arc::new(DbAuthBackend {
            db: db.clone(),
            layout,
        });
        // A config path inside the temp dir: setup saves the config, never to ~/.config.
        let core = Core::new(
            Arc::new(RwLock::new(config)),
            Some(dir.path().join("config.yaml")),
            backend,
        );
        let mut deps = AccountsDeps::new(core, db.clone());
        deps.setup.env_token = None;
        Env { dir, db, deps }
    }

    pub fn user(&self, name: &str, password: &str, role: Role) {
        self.db
            .create_user(name, password, role, UserStatus::Active, "")
            .expect("create user");
    }

    /// The router as the orchestrator mounts it: module routes, the `/` gate, WebDAV.
    pub fn app(&self) -> Router {
        app(self.deps.clone())
    }

    pub fn config(&self) -> parking_lot::RwLockWriteGuard<'_, Config> {
        self.deps.core.config.write()
    }

    pub async fn send(&self, req: Request<Body>) -> Resp {
        send(self.app(), req).await
    }
}

pub fn app(deps: AccountsDeps) -> Router {
    accounts::router(deps.clone())
        .fallback(|| async { (StatusCode::IM_A_TEAPOT, "dav") })
        .layer(axum::middleware::from_fn_with_state(
            deps,
            accounts::root_middleware,
        ))
}

pub struct Resp {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: bytes::Bytes,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {:?}", self.text()))
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

pub async fn send(app: Router, req: Request<Body>) -> Resp {
    let resp = app.oneshot(req).await.expect("infallible");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    Resp {
        status,
        headers,
        body,
    }
}

/// A request builder from a loopback peer (what a test without ConnectInfo would get
/// anyway, made explicit).
pub fn req(method: &str, uri: &str) -> http::request::Builder {
    from_peer(method, uri, "127.0.0.1")
}

pub fn from_peer(method: &str, uri: &str, peer: &str) -> http::request::Builder {
    let addr: SocketAddr = format!("{peer}:50000")
        .parse()
        .unwrap_or_else(|_| format!("[{peer}]:50000").parse().expect("peer"));
    Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(addr))
}

pub fn json_body(b: http::request::Builder, value: Value) -> Request<Body> {
    b.header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .expect("request")
}

pub fn empty(b: http::request::Builder) -> Request<Body> {
    b.body(Body::empty()).expect("request")
}

pub fn raw(b: http::request::Builder, body: &'static [u8]) -> Request<Body> {
    b.header("content-length", body.len().to_string())
        .body(Body::from(body))
        .expect("request")
}

pub fn basic(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

pub fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// Sign in through the API; returns the token.
pub async fn token(env: &Env, user: &str, password: &str) -> String {
    let r = env
        .send(json_body(
            req("POST", "/login/api/token"),
            serde_json::json!({"username": user, "password": password}),
        ))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["token"].as_str().expect("token").to_string()
}
