//! Shared harness for the `library_*` integration tests: a temp storage with a library
//! of generated `.cbz` files, a real database, the library runtime with short timings,
//! and the catalog/manifest routes in front of a stand-in WebDAV fallback (418 `dav`)
//! that does what the orchestrator's does: authorise, then hand series.json PUTs over.
#![allow(dead_code)]

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use bunko_core::{Config, Role};
use bunko_db::{Database, DbOptions, UserStatus};
use bunko_library::DebouncePolicy;
use bunko_server::backend::DbAuthBackend;
use bunko_server::library::{self, LibraryDeps, LibraryRuntime, RuntimeDeps};
use bunko_server::{Core, RequestCtx};
use http::{HeaderMap, StatusCode};
use parking_lot::RwLock;
use serde_json::Value;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

pub struct Env {
    pub dir: tempfile::TempDir,
    pub db: Arc<Database>,
    pub core: Core,
    pub runtime: Arc<LibraryRuntime>,
    pub deps: LibraryDeps,
    pub library: PathBuf,
}

/// Timings short enough for tests (the shape of 0.5.2's policy, scaled down).
pub fn fast_policy() -> DebouncePolicy {
    DebouncePolicy {
        debounce: Duration::from_millis(100),
        max_debounce: Duration::from_millis(600),
        retry_delay: Duration::from_millis(100),
        startup_delay: Duration::from_millis(50),
        periodic_rescan: Duration::from_secs(3600),
        update_lock_timeout: Duration::from_secs(10),
    }
}

pub struct Options {
    pub watch: bool,
    pub policy: DebouncePolicy,
    pub locks: Option<Arc<dyn bunko_library::PathWriteLocks>>,
    pub hooks: library::LibraryHooks,
    pub community: Option<library::CommunitySettings>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            watch: false,
            policy: fast_policy(),
            locks: None,
            hooks: Default::default(),
            community: None,
        }
    }
}

impl Env {
    pub fn new(edit: impl FnOnce(&mut Config)) -> Env {
        Env::with(edit, Options::default())
    }

    pub fn with(edit: impl FnOnce(&mut Config), options: Options) -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = Config::default();
        config.storage.base_path = dir.path().join("storage");
        config.catalog.enabled = true;
        config.catalog.enrich_community = false;
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
            layout: layout.clone(),
        });
        let core = Core::new(
            Arc::new(RwLock::new(config)),
            Some(dir.path().join("config.yaml")),
            backend,
        );
        let mut rdeps = RuntimeDeps::new(core.clone(), db.clone());
        rdeps.watch = options.watch;
        rdeps.policy = options.policy;
        rdeps.hooks = options.hooks;
        if let Some(community) = options.community {
            rdeps.community = community;
        }
        if let Some(locks) = options.locks {
            rdeps.locks = locks;
        }
        let runtime = LibraryRuntime::new(rdeps);
        let deps = LibraryDeps::new(core.clone(), db.clone(), runtime.clone());
        Env {
            library: layout.library(),
            dir,
            db,
            core,
            runtime,
            deps,
        }
    }

    pub fn user(&self, name: &str, role: Role) {
        self.db
            .create_user(name, "password1", role, UserStatus::Active, "")
            .expect("create user");
    }

    pub fn app(&self) -> Router {
        app(self.deps.clone())
    }

    pub async fn send(&self, req: Request<Body>) -> Resp {
        send(self.app(), req).await
    }

    pub fn series_dir(&self, series: &str) -> PathBuf {
        let d = self.library.join(series);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

async fn dav(State(deps): State<LibraryDeps>, ctx: RequestCtx, req: Request) -> Response {
    let path = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    let anon = deps.core.anonymous_access();
    if let Err(denied) = bunko_server::auth::authorize(
        req.method(),
        &path,
        None,
        &ctx.identity,
        anon,
        deps.core.backend.as_ref(),
    ) {
        return denied.into_response();
    }
    if library::is_series_put(req.method(), &path) {
        return library::series_put(&deps, req, ctx).await;
    }
    (StatusCode::IM_A_TEAPOT, "dav").into_response()
}

/// The routes as the orchestrator mounts them.
pub fn app(deps: LibraryDeps) -> Router {
    let fallback: Router = Router::new().fallback(dav).with_state(deps.clone());
    let mna = fallback.clone();
    library::router(deps.clone())
        .method_not_allowed_fallback(move |req: Request| {
            let svc = mna.clone();
            async move { svc.oneshot(req).await.unwrap_or_else(|e| match e {}) }
        })
        .fallback_service(fallback)
        .layer(axum::middleware::from_fn_with_state(
            deps,
            library::catalog_middleware,
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

pub fn req(method: &str, uri: &str) -> http::request::Builder {
    let addr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    http::Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(addr))
}

pub fn basic(user: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:password1"))
    )
}

pub fn get(uri: &str) -> Request<Body> {
    req("GET", uri).body(Body::empty()).unwrap()
}

/// A `.cbz` holding `pages` tiny "images" (`001.jpg`...). Names only matter.
pub fn write_cbz(path: &Path, pages: usize) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for i in 1..=pages {
        zip.start_file(format!("{i:03}.jpg"), options).unwrap();
        zip.write_all(b"\xff\xd8\xff\xd9").unwrap();
    }
    zip.finish().unwrap();
}

/// A primary `.mokuro` naming `pages` pages (`img_path` 001.jpg...).
pub fn write_mokuro(path: &Path, uuid: &str, pages: usize) {
    let pages: Vec<Value> = (1..=pages)
        .map(|i| serde_json::json!({"img_path": format!("{i:03}.jpg"), "blocks": [{"lines": ["日本語です"]}]}))
        .collect();
    let doc =
        serde_json::json!({"version": "0.2.2", "volume_uuid": uuid, "title": "x", "pages": pages});
    std::fs::write(path, serde_json::to_vec(&doc).unwrap()).unwrap();
}

/// Poll `check` until it holds or `timeout` passes.
pub async fn wait_for(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    check()
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
