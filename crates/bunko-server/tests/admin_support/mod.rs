//! Shared harness of the admin integration tests: a temp storage + config file, a real
//! `bunko_db::Database`, and the admin router driven with `oneshot`.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use bunko_core::{Config, Role};
use bunko_db::{Database, DbOptions, TokenKind, UserStatus};
use bunko_server::admin::{self, AdminDeps, NoOcr, OcrAdmin, UpdateService, UpdateSource};
use bunko_server::backend::DbAuthBackend;
use bunko_server::core::Core;
use bunko_server::ops::dyndns::DynDnsService;
use http::{HeaderMap, Method, Request, StatusCode, header};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

pub struct Harness {
    pub dir: tempfile::TempDir,
    pub db: Arc<Database>,
    pub core: Core,
    pub app: Router,
    pub config_path: PathBuf,
    pub dyndns: DynDnsService,
    pub dropped: Arc<Mutex<Vec<(String, String)>>>,
    pub restarts: Arc<AtomicUsize>,
}

pub type Configure = Box<dyn FnOnce(&mut Config)>;

#[derive(Default)]
pub struct Options {
    pub ocr: Option<Arc<dyn OcrAdmin>>,
    pub updates: Option<Arc<dyn UpdateSource>>,
    pub configure: Option<Configure>,
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub bytes: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(&self.bytes)))
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

impl Harness {
    pub fn new() -> Self {
        Self::with(Options::default())
    }

    pub fn with(opts: Options) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.storage.base_path = dir.path().join("storage");
        config.dyndns.token = "secret-token".into();
        if let Some(f) = opts.configure {
            f(&mut config);
        }
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
        let config_path = dir.path().join("config.yaml");
        let dyndns = DynDnsService::new(config.dyndns.clone());
        let config = Arc::new(RwLock::new(config));
        let backend = Arc::new(DbAuthBackend {
            db: db.clone(),
            layout: layout.clone(),
        });
        let core = Core::new(config.clone(), Some(config_path.clone()), backend);
        let dropped: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let restarts = Arc::new(AtomicUsize::new(0));
        let d2 = dropped.clone();
        let r2 = restarts.clone();
        let app = admin::router(AdminDeps {
            core: core.clone(),
            db: db.clone(),
            ocr: opts.ocr.unwrap_or_else(|| Arc::new(NoOcr)),
            tunnel: None,
            dyndns: Some(dyndns.clone()),
            updates: opts
                .updates
                .map(|src| UpdateService::new(src, config.clone())),
            drop_processors: Some(Arc::new(move |u: &str, why: &str| {
                d2.lock().push((u.to_string(), why.to_string()))
            })),
            restart: Some(Arc::new(move || {
                r2.fetch_add(1, Ordering::SeqCst);
            })),
        });
        Harness {
            dir,
            db,
            core,
            app,
            config_path,
            dyndns,
            dropped,
            restarts,
        }
    }

    /// Create an active account with `role` and return a bearer token for it.
    pub fn login(&self, username: &str, role: Role) -> String {
        self.db
            .create_user(username, "password123", role, UserStatus::Active, "")
            .expect("create user");
        self.db
            .create_auth_token(username, TokenKind::Web, "test", None)
            .expect("token")
            .0
    }

    pub fn admin(&self) -> String {
        self.login("boss", Role::Admin)
    }

    pub async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> Reply {
        let mut req = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        self.send(req.body(body).unwrap()).await
    }

    pub async fn raw(&self, method: &str, path: &str, token: Option<&str>, body: Vec<u8>) -> Reply {
        let mut req = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).unwrap())
            .uri(path);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = req
            .header(header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .unwrap();
        self.send(req).await
    }

    async fn send(&self, req: Request<Body>) -> Reply {
        let resp = self.app.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec();
        Reply {
            status,
            headers,
            bytes,
        }
    }

    /// The audit rows, newest first.
    pub fn audit(&self) -> Vec<bunko_db::AuditEvent> {
        self.db.list_audit_events(1000, None).expect("audit")
    }

    /// The config as saved on disk.
    pub fn saved_config(&self) -> Config {
        bunko_core::config::load_config(Some(&self.config_path)).expect("saved config")
    }
}
