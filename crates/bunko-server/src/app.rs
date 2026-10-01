//! Application assembly: startup validation, services, the router in 0.5.2's
//! precedence order (spec http-webdav §2.1), and ordered shutdown.

use crate::auth::{self};
use crate::backend::DbAuthBackend;
use crate::core::{Core, RequestCtx};
use crate::http::{cors, headers, static_files};
use crate::ops::dyndns::DynDnsService;
use crate::ops::tunnel::TunnelService;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bunko_core::{Config, StorageLayout};
use bunko_db::{Database, DbOptions};
use http::StatusCode;
use parking_lot::RwLock;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// What the binary tells the server about itself.
#[derive(Clone, Default)]
pub struct ServeOptions {
    pub verbose: bool,
    /// `full` or `lite`.
    pub flavor: &'static str,
}

/// 0.5.2 `_validate_startup_environment`: directories exist and are writable; TLS
/// material is present and valid. The message is printed as
/// `Startup validation failed: <msg>` and the process exits 2.
pub fn validate_startup(config: &Config) -> Result<(), String> {
    let layout = config.storage.layout();
    layout.ensure_directories().map_err(|e| format!("Could not create storage directories under {}: {e}", layout.base.display()))?;
    StorageLayout::assert_writable_dir(&layout.base, "storage.base_path")?;
    StorageLayout::assert_writable_dir(&layout.library(), "storage.library_path")?;
    StorageLayout::assert_writable_dir(&layout.inbox(), "storage.inbox_path")?;
    StorageLayout::assert_writable_dir(&layout.users(), "storage.users_path")?;
    if !config.ssl.enabled {
        return Ok(());
    }
    if config.ssl.auto_cert {
        let (cert, key) = crate::tls::default_cert_paths();
        for (p, label) in [(&cert, "ssl auto-cert directory"), (&key, "ssl auto-key directory")] {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
                StorageLayout::assert_writable_dir(parent, label)?;
            }
        }
        return Ok(());
    }
    let cert = bunko_core::storage::expand_user(Path::new(&config.ssl.cert_file));
    let key = bunko_core::storage::expand_user(Path::new(&config.ssl.key_file));
    if !cert.is_file() {
        return Err(format!("SSL certificate file not found: {}", cert.display()));
    }
    if !key.is_file() {
        return Err(format!("SSL private key file not found: {}", key.display()));
    }
    let (errors, _warnings) = crate::tls::validate_pair(&cert, &key);
    match errors.into_iter().next() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// The long-lived services, kept for shutdown and for wiring into modules.
pub struct Services {
    pub core: Core,
    pub db: Arc<Database>,
    pub dyndns: DynDnsService,
    pub tunnel: TunnelService,
    pub stop: CancellationToken,
}

impl Services {
    pub fn new(config: Config, config_path: Option<PathBuf>) -> anyhow::Result<Self> {
        let db_options = DbOptions::from(&config.database);
        let layout = config.storage.layout();
        let db = Arc::new(Database::open_with(layout.database(), &db_options)?);
        let dyndns = DynDnsService::new(config.dyndns.clone());
        let config = Arc::new(RwLock::new(config));
        let backend = Arc::new(DbAuthBackend { db: db.clone(), layout: layout.clone() });
        let core = Core::new(config, config_path, backend);
        Ok(Services { core, db, dyndns, tunnel: TunnelService::default(), stop: CancellationToken::new() })
    }
}

/// The handler WebDAV requests fall through to, after authentication and authorisation.
pub type DavFallback = Arc<dyn Fn(Request, RequestCtx) -> futures_util::future::BoxFuture<'static, Response> + Send + Sync>;

#[derive(Clone)]
struct FallbackState {
    core: Core,
    dav: DavFallback,
}

impl axum::extract::FromRef<FallbackState> for Core {
    fn from_ref(s: &FallbackState) -> Core {
        s.core.clone()
    }
}

/// Authenticate + authorise (spec §4.3) then hand the request to WebDAV.
async fn dav_fallback(State(st): State<FallbackState>, ctx: RequestCtx, req: Request) -> Response {
    let path = percent_encoding::percent_decode_str(req.uri().path()).decode_utf8_lossy().into_owned();
    let destination = req.headers().get("destination").and_then(|v| v.to_str().ok()).map(str::to_string);
    let anon = st.core.anonymous_access();
    let backend = st.core.backend.clone();
    if let Err(denied) = auth::authorize(req.method(), &path, destination.as_deref(), &ctx.identity, anon, backend.as_ref()) {
        return denied.into_response();
    }
    (st.dav)(req, ctx).await
}

/// A placeholder WebDAV handler until bunko-dav is wired.
pub fn dav_unavailable() -> DavFallback {
    Arc::new(|_req, _ctx| Box::pin(async { (StatusCode::NOT_IMPLEMENTED, "WebDAV not wired").into_response() }))
}

/// Assemble the router: module routers (each with its own state) in 0.5.2 precedence,
/// then the authenticated WebDAV fallback, wrapped by CORS and security headers.
pub fn build_router(core: Core, modules: Vec<Router>, dav: DavFallback) -> Router {
    let mut app: Router = Router::new()
        .route("/robots.txt", get(static_files::robots))
        .route("/_static/{*file}", get(static_files::shared_static));
    for m in modules {
        app = app.merge(m);
    }
    let fallback_state = FallbackState { core: core.clone(), dav };
    let dav_router: Router = Router::new().fallback(dav_fallback).with_state(fallback_state);
    app.fallback_service(dav_router)
        .layer(axum::middleware::from_fn_with_state(core.clone(), cors::cors))
        .layer(axum::middleware::from_fn(headers::security_headers))
}

/// Bind, serve until SIGINT/SIGTERM, shut down in order.
pub async fn serve_router(services: &Services, app: Router) -> anyhow::Result<()> {
    let (host, port, ssl) = {
        let c = services.core.config.read();
        (c.server.host.clone(), c.server.port, c.ssl.clone())
    };
    let tls = crate::tls::server_config(&ssl)?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    let bound = listener.local_addr()?;
    info!("Starting mokuro-bunko server on {scheme}://{host}:{}", bound.port());
    if ssl.enabled {
        info!("SSL: {}", crate::tls::describe(&ssl));
    }
    let stop = services.stop.clone();
    tokio::spawn({
        let stop = stop.clone();
        async move {
            crate::serve::shutdown_signal().await;
            stop.cancel();
        }
    });
    if services.core.config.read().dyndns.enabled {
        services.dyndns.start();
    }
    crate::serve::serve(listener, app, tls, stop, Duration::from_secs(5)).await?;
    // Ordered shutdown (0.5.2 shutdown_app order, plus the new services).
    services.dyndns.stop();
    services.tunnel.stop().await;
    Ok(())
}

pub fn not_found_json() -> Response {
    let mut r = Response::new(Body::from(r#"{"error": "Not found"}"#));
    *r.status_mut() = StatusCode::NOT_FOUND;
    r.headers_mut().insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
    r
}

#[allow(dead_code)]
fn warn_unused() {
    warn!("unused");
}
