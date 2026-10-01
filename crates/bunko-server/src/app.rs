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
use tracing::info;

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
    pub updates: crate::admin::UpdateService,
    pub dav: bunko_dav::Dav,
    pub dav_hooks: Arc<crate::davhooks::ServerDavHooks>,
    pub library: Arc<crate::library::LibraryRuntime>,
    pub stop: CancellationToken,
    /// Set by the admin "Update and restart" action: the binary re-execs after shutdown.
    pub restart_requested: Arc<std::sync::atomic::AtomicBool>,
}

impl Services {
    pub fn new(config: Config, config_path: Option<PathBuf>, flavor: &str) -> anyhow::Result<Self> {
        let db_options = DbOptions::from(&config.database);
        let layout = config.storage.layout();
        let db = Arc::new(Database::open_with(layout.database(), &db_options)?);
        let dyndns = DynDnsService::new(config.dyndns.clone());
        let config = Arc::new(RwLock::new(config));
        let backend = Arc::new(DbAuthBackend { db: db.clone(), layout: layout.clone() });
        let updates = crate::admin::UpdateService::from_config(config.clone(), flavor);
        // The PROPFIND cache takes half of the cache budget (server.cache_mb).
        let cache_mb = config.read().server.cache_mb.max(2) as usize;
        let dav_config = bunko_dav::DavConfig {
            propfind_cache: bunko_dav::CacheConfig { budget_bytes: cache_mb * 1024 * 1024 / 2, ..Default::default() },
        };
        let dav = bunko_dav::Dav::new(&layout, dav_config)?;
        let dav_hooks = Arc::new(crate::davhooks::ServerDavHooks::new(db.clone()));
        let core = Core::new(config, config_path, backend);
        let library = {
            let mut deps = crate::library::RuntimeDeps::new(core.clone(), db.clone());
            deps.locks = Arc::new(DavPathLocks(dav.write_locks().clone()));
            let cache = dav.propfind_cache().clone();
            deps.hooks.propfind_refresh = Some(Arc::new(move || cache.schedule_refresh(Duration::from_secs(5))));
            crate::library::LibraryRuntime::new(deps)
        };
        dav_hooks.add_listener(library.clone());
        Ok(Services {
            core,
            db,
            dyndns,
            tunnel: TunnelService::default(),
            updates,
            dav,
            dav_hooks,
            library,
            stop: CancellationToken::new(),
            restart_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
}

/// The metadata compiler takes the same per-path write locks as WebDAV writes.
struct DavPathLocks(bunko_dav::PathWriteLocks);

impl bunko_library::service::PathWriteLocks for DavPathLocks {
    fn try_lock(&self, path: &Path) -> Option<Box<dyn Send>> {
        self.0.try_lock(path).map(|g| Box::new(g) as Box<dyn Send>)
    }
}

/// The handler WebDAV requests fall through to, after authentication and authorisation.
pub type DavFallback = Arc<dyn Fn(Request, RequestCtx) -> futures_util::future::BoxFuture<'static, Response> + Send + Sync>;

#[derive(Clone)]
struct FallbackState {
    core: Core,
    dav: DavFallback,
    library: Option<crate::library::LibraryDeps>,
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
        let resp = denied.into_response();
        // A refused PUT still answers with the upload verdict JSON (spec http-webdav §9).
        return if req.method() == http::Method::PUT { bunko_dav::put_refusal_verdict(&path, resp) } else { resp };
    }
    if let Some(lib) = &st.library
        && crate::library::is_series_put(req.method(), &path)
    {
        return crate::library::series_put(lib, req, ctx).await;
    }
    (st.dav)(req, ctx).await
}

/// The WebDAV handler over `bunko-dav`, with the server's hooks.
pub fn dav_handler(dav: bunko_dav::Dav, hooks: Arc<crate::davhooks::ServerDavHooks>) -> DavFallback {
    let nginx_accel = std::env::var("MOKURO_NGINX_ACCEL").is_ok_and(|v| v.trim() == "1");
    Arc::new(move |req, ctx| {
        let dav = dav.clone();
        let hooks: Arc<dyn bunko_dav::DavHooks> = hooks.clone();
        Box::pin(async move {
            let dctx = bunko_dav::DavContext {
                username: ctx.identity.username().map(str::to_string),
                role: ctx.identity.role(),
                nginx_accel,
                client_ip: Some(ctx.client_ip.clone()),
                hooks,
            };
            dav.handle(req, dctx).await
        })
    })
}

/// 0.5.2 put `AuthMiddleware` in front of the admin and processor APIs: anonymous or
/// wrongly-roled requests get its 401/403 text answers before the module runs.
async fn auth_gate(State(core): State<Core>, ctx: RequestCtx, req: Request, next: axum::middleware::Next) -> Response {
    let path = percent_encoding::percent_decode_str(req.uri().path()).decode_utf8_lossy().into_owned();
    if auth::paths::is_admin_path(&path) || auth::paths::is_processor_path(&path) {
        let anon = core.anonymous_access();
        if let Err(denied) = auth::authorize(req.method(), &path, None, &ctx.identity, anon, core.backend.as_ref()) {
            return denied.into_response();
        }
    }
    next.run(req).await
}

/// A placeholder WebDAV handler until bunko-dav is wired.
pub fn dav_unavailable() -> DavFallback {
    Arc::new(|_req, _ctx| Box::pin(async { (StatusCode::NOT_IMPLEMENTED, "WebDAV not wired").into_response() }))
}

/// Assemble the router: module routers (each with its own state) in 0.5.2 precedence,
/// then the authenticated WebDAV fallback, wrapped by CORS and security headers.
pub fn build_router(core: Core, modules: Vec<Router>, dav: DavFallback) -> Router {
    build_router_with(core, modules, dav, None, |r| r)
}

/// [`build_router`] with `inner` applied inside CORS and the security headers (for
/// gates that must see every request, like the `/` home/setup redirect).
pub fn build_router_with(
    core: Core,
    modules: Vec<Router>,
    dav: DavFallback,
    library: Option<crate::library::LibraryDeps>,
    inner: impl FnOnce(Router) -> Router,
) -> Router {
    let mut app: Router = Router::new()
        .route("/robots.txt", get(static_files::robots))
        .route("/_static/{*file}", get(static_files::shared_static));
    for m in modules {
        app = app.merge(m);
    }
    let fallback_state = FallbackState { core: core.clone(), dav, library };
    let dav_router: Router = Router::new().fallback(dav_fallback).with_state(fallback_state);
    // A method a module route does not take (PUT /login/x, HEAD /) goes to WebDAV, as 0.5.2.
    let mna = dav_router.clone();
    let app = app
        .method_not_allowed_fallback(move |req: Request| {
            let svc = mna.clone();
            async move { tower::ServiceExt::oneshot(svc, req).await.unwrap_or_else(|e| match e {}) }
        })
        .fallback_service(dav_router);
    inner(app.layer(axum::middleware::from_fn_with_state(core.clone(), auth_gate)))
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
    services.updates.start(stop.clone());
    services.library.start();
    crate::serve::serve(listener, app, tls, stop, Duration::from_secs(5)).await?;
    // Ordered shutdown (0.5.2 shutdown_app order, plus the new services).
    services.library.stop().await;
    services.dyndns.stop();
    services.dav.shutdown();
    services.updates.stop().await;
    services.tunnel.stop().await;
    Ok(())
}

/// Build every module router and the WebDAV fallback for these services. Modules are
/// added here as they land; the order is 0.5.2's precedence (spec http-webdav §2.1).
pub fn assemble(services: &Services, _opts: &ServeOptions) -> Router {
    let mut accounts = crate::accounts::AccountsDeps::new(services.core.clone(), services.db.clone());
    accounts.library = Some(services.library.counts());
    let library = crate::library::LibraryDeps::new(services.core.clone(), services.db.clone(), services.library.clone());
    let restart = {
        let flag = services.restart_requested.clone();
        let stop = services.stop.clone();
        Arc::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            stop.cancel();
        }) as Arc<dyn Fn() + Send + Sync>
    };
    let admin = crate::admin::AdminDeps {
        core: services.core.clone(),
        db: services.db.clone(),
        ocr: Arc::new(crate::admin::NoOcr),
        tunnel: Some(services.tunnel.clone()),
        dyndns: Some(services.dyndns.clone()),
        updates: Some(services.updates.clone()),
        drop_processors: None,
        restart: Some(restart),
    };
    let modules: Vec<Router> =
        vec![crate::accounts::router(accounts.clone()), crate::admin::router(admin), crate::library::router(library.clone())];
    let dav = dav_handler(services.dav.clone(), services.dav_hooks.clone());
    let catalog = library.clone();
    build_router_with(services.core.clone(), modules, dav, Some(library), move |r| {
        r.layer(axum::middleware::from_fn_with_state(catalog, crate::library::catalog_middleware))
            .layer(axum::middleware::from_fn_with_state(accounts, crate::accounts::root_middleware))
    })
}

/// When nobody can sign in yet, write (or reuse) a one-time setup token and log the URL,
/// so the wizard is reachable from another machine (Docker bridge networking).
pub fn announce_setup(services: &Services) {
    let flag = crate::accounts::SetupFlag::default();
    if !flag.needs_setup(&services.db).unwrap_or(false) {
        return;
    }
    let (host, port, ssl) = {
        let c = services.core.config.read();
        (c.server.host.clone(), c.server.port, c.ssl.enabled)
    };
    let scheme = if ssl { "https" } else { "http" };
    let host = if host == "0.0.0.0" || host == "::" { "localhost".to_string() } else { host };
    match crate::accounts::ensure_setup_token(&services.core.layout) {
        Ok(Some(token)) => info!("First run: finish setup at {scheme}://{host}:{port}/setup?token={token}"),
        Ok(None) => info!("First run: finish setup at {scheme}://{host}:{port}/setup (token from MOKURO_SETUP_TOKEN)"),
        Err(e) => info!("First run: finish setup at {scheme}://{host}:{port}/setup from this machine ({e})"),
    }
}

pub fn not_found_json() -> Response {
    let mut r = Response::new(Body::from(r#"{"error": "Not found"}"#));
    *r.status_mut() = StatusCode::NOT_FOUND;
    r.headers_mut().insert(http::header::CONTENT_TYPE, http::HeaderValue::from_static("application/json"));
    r
}

