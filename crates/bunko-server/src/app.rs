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
    /// `full`, `full-cuda`, ... or `lite`.
    pub flavor: &'static str,
    /// Local OCR (full build): starts an in-process processor over the real engines.
    pub local: Option<Arc<dyn crate::ocr::LocalProcessorFactory>>,
    /// The machine this server runs on (the admin panel's "This server", the setup's
    /// OCR step); the binary supplies it.
    pub machine: Option<Arc<dyn crate::machine::Machine>>,
}

/// 0.5.2 `_validate_startup_environment`: directories exist and are writable; TLS
/// material is present and valid. The message is printed as
/// `Startup validation failed: <msg>` and the process exits 2.
// unmounted: the std calls here touch only the TLS certificate and key paths (config).
#[allow(clippy::disallowed_methods)]
pub fn validate_startup(config: &Config) -> Result<(), String> {
    let layout = config.storage.layout();
    layout.ensure_directories().map_err(|e| {
        format!(
            "Could not create storage directories under {}: {e}",
            layout.base.display()
        )
    })?;
    StorageLayout::assert_writable_dir(&layout.base, "storage.base_path")?;
    // A cloud library's root is a mount point, never a folder on this disk.
    if !layout.cloud_library() {
        StorageLayout::assert_writable_dir(&layout.library(), "storage.library_path")?;
    }
    StorageLayout::assert_writable_dir(&layout.inbox(), "storage.inbox_path")?;
    StorageLayout::assert_writable_dir(&layout.users(), "storage.users_path")?;
    if !config.ssl.enabled {
        return Ok(());
    }
    if config.ssl.auto_cert {
        let (cert, key) = crate::tls::default_cert_paths();
        for (p, label) in [
            (&cert, "ssl auto-cert directory"),
            (&key, "ssl auto-key directory"),
        ] {
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
        return Err(format!(
            "SSL certificate file not found: {}",
            cert.display()
        ));
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
    pub ocr: crate::ocr::OcrControl,
    /// Cover thumbnails for library volumes (started by `serve_router`).
    pub thumbs: Arc<crate::thumbs::Thumbnails>,
    pub stop: CancellationToken,
    /// Set by the admin "Update and restart" action: the binary re-execs after shutdown.
    pub restart_requested: Arc<std::sync::atomic::AtomicBool>,
    /// Requests that write (uploads, moves, deletes, processor results) being served
    /// now: an automatic update restarts only when there are none.
    pub writes: WritesInFlight,
    /// First-run state: the setup code and its sessions (shared with the accounts
    /// routes; [`announce_setup`] issues the code).
    pub setup: crate::accounts::SetupFlag,
    /// A cloud library's store (`storage.library.backend` is not `local`): status for
    /// the admin panel, the first-sync wait; `None` for a library on this disk.
    pub storage: Option<Arc<crate::storage::StoreHandle>>,
    /// Connect / Disconnect a cloud library (setup wizard, admin Storage panel, the
    /// local control API).
    pub storage_control: Arc<crate::storage::StorageControl>,
    /// A cloud library: the store's change events drop the PROPFIND listings they
    /// touch at once (spec §10).
    _listing_watch: Option<bunko_store::WatchGuard>,
    /// The library root mounted (a `LocalStore`, or the cloud store) for as long as the
    /// services live (last, so it is unmounted after everything else has been dropped).
    _library_mount: bunko_store::MountGuard,
}

/// Counts the writing requests in flight ([`Services::writes`]).
#[derive(Clone, Default, Debug)]
pub struct WritesInFlight(Arc<std::sync::atomic::AtomicUsize>);

impl WritesInFlight {
    pub fn count(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct WriteGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for WriteGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn count_writes(
    axum::extract::State(w): axum::extract::State<WritesInFlight>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    let writes = !matches!(
        req.method().as_str(),
        "GET" | "HEAD" | "OPTIONS" | "PROPFIND"
    );
    let _guard = writes.then(|| {
        w.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        WriteGuard(w.0.clone())
    });
    next.run(req).await
}

/// Library paths resolve as on NTFS (0.5.3 `PathCaseMiddleware`): a request spelled
/// `kingdom/` reaches the `Kingdom/` already on disk, so no layer above the filesystem
/// ever sees -- or creates -- a second spelling of one folder. Rewrites the path of a
/// `/mokuro-reader/<library path>` request and a MOVE/COPY `Destination`; segments naming
/// nothing on disk keep the client's spelling.
pub async fn path_case_middleware(
    State(path_case): State<bunko_dav::PathCase>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    let (parts, body) = req.into_parts();
    if !bunko_dav::PathCase::may_rewrite(&parts) {
        return next.run(Request::from_parts(parts, body)).await;
    }
    let rewritten = tokio::task::spawn_blocking(move || {
        let mut parts = parts;
        path_case.rewrite_request(&mut parts);
        parts
    })
    .await;
    match rewritten {
        Ok(parts) => next.run(Request::from_parts(parts, body)).await,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The automatic update's view of this server: OCR claims and writing requests.
struct ServerQuiet {
    ocr: crate::ocr::OcrControl,
    writes: WritesInFlight,
}

impl crate::admin::Quiet for ServerQuiet {
    fn drain(&self, on: bool) {
        let ocr = self.ocr.clone();
        tokio::spawn(async move { ocr.set_update_drain(on).await });
    }

    fn busy(&self) -> futures_util::future::BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let mut parts: Vec<String> = self.ocr.in_flight().await.into_iter().collect();
            let w = self.writes.count();
            if w > 0 {
                parts.push(format!("{w} upload(s) or other write(s)"));
            }
            (!parts.is_empty()).then(|| parts.join(", "))
        })
    }
}

impl Services {
    pub fn new(
        config: Config,
        config_path: Option<PathBuf>,
        opts: &ServeOptions,
    ) -> anyhow::Result<Self> {
        // Everything built here that keeps a runtime handle (the library's timers, the
        // PROPFIND cache's refreshes, the OCR control) belongs to the background runtime.
        let _background = crate::rt::enter();
        let flavor = opts.flavor;
        let db_options = DbOptions::from(&config.database);
        let layout = config.storage.layout();
        // Before anything below touches the library: the cloud store (mounted at its
        // synthetic root, which every `layout.library()` below names), or the disk.
        let (library_mount, storage, layout) =
            match crate::storage::open_library_store(&config, &layout)? {
                Some((store, guard, handle)) => {
                    let layout = layout.with_library_root(store.root());
                    (guard, Some(handle), layout)
                }
                None => (crate::library::mount_local(&layout.library()), None, layout),
            };
        let db = Arc::new(Database::open_with(layout.database(), &db_options)?);
        let dyndns = DynDnsService::new(config.dyndns.clone());
        let config = Arc::new(RwLock::new(config));
        let backend = Arc::new(DbAuthBackend::new(db.clone(), layout.clone()));
        let updates = crate::admin::UpdateService::from_config(config.clone(), flavor);
        // The PROPFIND cache takes half of the cache budget (server.cache_mb).
        let cache_mb = config.read().server.cache_mb.max(2) as usize;
        let dav_config = bunko_dav::DavConfig {
            propfind_cache: bunko_dav::CacheConfig {
                budget_bytes: cache_mb * 1024 * 1024 / 2,
                ..Default::default()
            },
        };
        let dav = bunko_dav::Dav::new(&layout, dav_config)?;
        let dav_hooks = Arc::new(crate::davhooks::ServerDavHooks::new(db.clone()));
        let core = Core::with_layout(config, config_path, backend, layout.clone());
        let storage_control = crate::storage::StorageControl::for_core(&core, storage.clone());
        let late_ocr: Arc<std::sync::OnceLock<crate::ocr::OcrControl>> =
            Arc::new(std::sync::OnceLock::new());
        let library = {
            let mut deps = crate::library::RuntimeDeps::new(core.clone(), db.clone());
            deps.hooks.archive_events = Some(Arc::new(LateArchiveEvents(late_ocr.clone())));
            deps.locks = Arc::new(DavPathLocks(dav.write_locks().clone()));
            let cache = dav.propfind_cache().clone();
            deps.hooks.propfind_refresh = Some(Arc::new(move || {
                cache.schedule_refresh(Duration::from_secs(5))
            }));
            crate::library::LibraryRuntime::new(deps)
        };
        dav_hooks.add_listener(library.clone());
        let thumbs = Arc::new(crate::thumbs::Thumbnails::default());
        let ocr = {
            let store: Arc<dyn bunko_library::MetadataStore> = library.store().clone();
            let lib = library.clone();
            let facts = crate::ocr::types::StoreFacts {
                store,
                library: layout.library(),
                installed: Some(Arc::new(move |cbz: &Path| lib.on_library_write(cbz))),
                thumbnails: Some({
                    let t = thumbs.clone();
                    Arc::new(move || t.pending())
                }),
            };
            crate::ocr::OcrControl::new(crate::ocr::OcrDeps {
                core: core.clone(),
                db: Some(db.clone()),
                facts: Arc::new(facts),
                locks: Arc::new(DavPathLocks(dav.write_locks().clone())),
                local: opts.local.clone(),
                clock: None,
            })
        };
        let _ = late_ocr.set(ocr.clone());
        dav_hooks.add_listener(Arc::new(ocr.clone()));
        let listing_watch = storage
            .as_ref()
            .and_then(|_| watch_listings(&dav, &layout.library()));
        if let Some(handle) = &storage {
            // Changes made directly in the cloud run the hooks a DAV write runs (D20).
            handle.set_hooks(Arc::new(crate::storage::ServerCloudHooks::new(
                layout.library(),
                dav_hooks.clone(),
                library.clone(),
                db.clone(),
            )));
            // Case variants merge into the oldest spelling (§5.9).
            handle.set_case_merge_policy(Arc::new(crate::storage::ServerCaseMergePolicy::new(
                layout.library(),
                core.clone(),
                db.clone(),
            )));
        }
        Ok(Services {
            storage,
            storage_control,
            _listing_watch: listing_watch,
            core,
            db,
            dyndns,
            tunnel: TunnelService::default(),
            updates,
            dav,
            dav_hooks,
            library,
            ocr,
            thumbs,
            stop: CancellationToken::new(),
            restart_requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            writes: WritesInFlight::default(),
            setup: crate::accounts::SetupFlag::default(),
            _library_mount: library_mount,
        })
    }
}

/// Drop the cached PROPFIND listings a store change touches (the path's own, its
/// ancestors' and its descendants'), as a DAV write does for its own changes; a
/// `Rescan` drops them all (spec §10).
fn watch_listings(dav: &bunko_dav::Dav, library: &Path) -> Option<bunko_store::WatchGuard> {
    let store = bunko_store::store_for(library)?;
    let (dav, root) = (dav.clone(), library.to_path_buf());
    let sink: bunko_store::WatchSink = Arc::new(move |ev: bunko_store::WatchEvent| {
        match ev.path().and_then(|p| crate::ocr::types::rel_of(&root, p)) {
            Some(rel) => dav.invalidate_listing(&[format!("/{}/{rel}", auth::paths::READER_ROOT)]),
            None => dav.propfind_cache().invalidate(),
        }
    });
    match store.watch(sink) {
        Ok(guard) => Some(guard),
        Err(e) => {
            tracing::warn!("[STORAGE] PROPFIND listings refresh on a timer only: {e}");
            None
        }
    }
}

/// Library watcher → OCR queue, bound once the OCR control exists.
struct LateArchiveEvents(Arc<std::sync::OnceLock<crate::ocr::OcrControl>>);

impl crate::library::ArchiveEvents for LateArchiveEvents {
    fn archive_added(&self, cbz: &Path) {
        if let Some(o) = self.0.get() {
            o.archive_arrived(cbz);
        }
    }
    fn archive_removed(&self, cbz: &Path) {
        if let Some(o) = self.0.get() {
            o.archives_removed(&[cbz.to_path_buf()]);
        }
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
pub type DavFallback = Arc<
    dyn Fn(Request, RequestCtx) -> futures_util::future::BoxFuture<'static, Response> + Send + Sync,
>;

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
    let path = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    let destination = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let anon = st.core.anonymous_access();
    let backend = st.core.backend.clone();
    if let Err(denied) = auth::authorize(
        req.method(),
        &path,
        destination.as_deref(),
        &ctx.identity,
        anon,
        backend.as_ref(),
    ) {
        let resp = denied.into_response();
        // A refused PUT still answers with the upload verdict JSON (spec http-webdav §9).
        return if req.method() == http::Method::PUT {
            bunko_dav::put_refusal_verdict(&path, resp)
        } else {
            resp
        };
    }
    // Without readers WebDAV only serves processors' archive fetches (also checked in
    // `readers_gate`); the rest (`/`, `/inbox`, ...) is not mounted.
    if !st.core.config.read().server.serve_readers {
        if !is_reader_path(&path) {
            return text_status(StatusCode::NOT_FOUND, "Not found");
        }
        if let Some(refused) = refuse_reader(req.method(), &path, &ctx.identity) {
            return refused;
        }
    }
    if let Some(lib) = &st.library
        && crate::library::is_series_put(req.method(), &path)
    {
        return crate::library::series_put(lib, req, ctx).await;
    }
    (st.dav)(req, ctx).await
}

/// The WebDAV handler over `bunko-dav`, with the server's hooks.
pub fn dav_handler(
    dav: bunko_dav::Dav,
    hooks: Arc<crate::davhooks::ServerDavHooks>,
) -> DavFallback {
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
async fn auth_gate(
    State(core): State<Core>,
    ctx: RequestCtx,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    let path = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    if auth::paths::is_admin_path(&path) || auth::paths::is_processor_path(&path) {
        let anon = core.anonymous_access();
        if let Err(denied) = auth::authorize(
            req.method(),
            &path,
            None,
            &ctx.identity,
            anon,
            core.backend.as_ref(),
        ) {
            if auth::paths::is_processor_path(&path)
                && matches!(denied.status.as_u16(), 401 | 429)
                && let (Some(user), Some(cb)) =
                    (&ctx.identity.attempted_username, PROCESSOR_REFUSALS.get())
            {
                cb(user, &format!("invalid credentials from {}", ctx.client_ip));
            }
            return denied.into_response();
        }
    }
    next.run(req).await
}

/// `/mokuro-reader` and everything under it (percent-decoded).
fn is_reader_path(path: &str) -> bool {
    path.strip_prefix('/')
        .and_then(|p| p.strip_prefix(auth::paths::READER_ROOT))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Without readers, a `/mokuro-reader/` request passes only as a processor's `GET` or
/// `HEAD` of a library file (the archive fetch, PROTOCOL.md step 5): `None`. Anything
/// else is refused, 401 anonymous (or the credential's own 401/429), 403 signed in.
fn refuse_reader(method: &http::Method, path: &str, identity: &auth::Identity) -> Option<Response> {
    let fetch = matches!(method.as_str(), "GET" | "HEAD")
        && auth::paths::is_library_path(path)
        && identity.role() == bunko_core::Role::Processor;
    if fetch {
        return None;
    }
    if !identity.authenticated() {
        let message = identity
            .error
            .clone()
            .unwrap_or_else(|| "Authentication required".into());
        let status = if message.contains("Too many failed attempts") {
            StatusCode::TOO_MANY_REQUESTS
        } else {
            StatusCode::UNAUTHORIZED
        };
        return Some(
            auth::Denied {
                status,
                message,
                bearer: identity.bearer_failed,
            }
            .into_response(),
        );
    }
    Some(text_status(
        StatusCode::FORBIDDEN,
        "This server does not serve readers (server.serve_readers is off)",
    ))
}

fn text_status(status: StatusCode, message: &str) -> Response {
    auth::Denied {
        status,
        message: message.to_string(),
        bearer: false,
    }
    .into_response()
}

/// `server.serve_readers: false` (read per request): the catalog (`/catalog*`, its
/// manifest included) answers 404, and `/mokuro-reader/` is limited to processors'
/// archive fetches ([`refuse_reader`]) before the queue file or WebDAV see it. The rest
/// of WebDAV is cut off in [`dav_fallback`].
async fn readers_gate(
    State(core): State<Core>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    if core.config.read().server.serve_readers || req.method() == http::Method::OPTIONS {
        return next.run(req).await;
    }
    let path = percent_encoding::percent_decode_str(req.uri().path())
        .decode_utf8_lossy()
        .into_owned();
    if path == "/catalog" || path.starts_with("/catalog/") {
        return text_status(StatusCode::NOT_FOUND, "Not found");
    }
    if !is_reader_path(&path) {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    let Ok(ctx) = <RequestCtx as axum::extract::FromRequestParts<Core>>::from_request_parts(
        &mut parts, &core,
    )
    .await;
    match refuse_reader(&parts.method, &path, &ctx.identity) {
        Some(refused) => refused,
        None => next.run(Request::from_parts(parts, body)).await,
    }
}

type RefusalHook = Arc<dyn Fn(&str, &str) + Send + Sync>;
/// Where refused processor logins are reported (the registry's admin-panel list).
static PROCESSOR_REFUSALS: std::sync::OnceLock<RefusalHook> = std::sync::OnceLock::new();

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
    let fallback_state = FallbackState {
        core: core.clone(),
        dav,
        library,
    };
    let dav_router: Router = Router::new()
        .fallback(dav_fallback)
        .with_state(fallback_state);
    // A method a module route does not take (PUT /login/x, HEAD /) goes to WebDAV, as 0.5.2.
    let mna = dav_router.clone();
    let app = app
        .method_not_allowed_fallback(move |req: Request| {
            let svc = mna.clone();
            async move {
                tower::ServiceExt::oneshot(svc, req)
                    .await
                    .unwrap_or_else(|e| match e {})
            }
        })
        .fallback_service(dav_router);
    inner(app.layer(axum::middleware::from_fn_with_state(
        core.clone(),
        auth_gate,
    )))
    .layer(axum::middleware::from_fn_with_state(
        core.clone(),
        readers_gate,
    ))
    .layer(axum::middleware::from_fn_with_state(
        core.clone(),
        cors::cors,
    ))
    .layer(axum::middleware::from_fn(headers::path_guard))
    .layer(axum::middleware::from_fn(headers::security_headers))
    .layer(axum::middleware::from_fn(
        |req: Request, next: axum::middleware::Next| async move {
            if headers::request_log_enabled() {
                headers::request_log(req, next).await
            } else {
                next.run(req).await
            }
        },
    ))
}

/// Bind, serve until SIGINT/SIGTERM, shut down in order.
pub async fn serve_router(services: &Services, app: Router) -> anyhow::Result<()> {
    serve_router_with(services, app, None).await
}

/// [`serve_router`] with the automatic update's installer (`update.auto`; the binary
/// supplies it).
pub async fn serve_router_with(
    services: &Services,
    app: Router,
    installer: Option<Arc<dyn bunko_update::auto::ReleaseInstaller>>,
) -> anyhow::Result<()> {
    let (host, port, ssl) = {
        let c = services.core.config.read();
        (c.server.host.clone(), c.server.port, c.ssl.clone())
    };
    let tls = crate::tls::server_config(&ssl)?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    let bound = listener.local_addr()?;
    info!(
        "Starting mokuro-bunko server on {scheme}://{host}:{}",
        bound.port()
    );
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
    // The background services start on the background runtime (crate::rt): nothing
    // they spawn competes with requests for the serving runtime's threads.
    let thumbs = {
        let _background = crate::rt::enter();
        if services.core.config.read().dyndns.enabled {
            services.dyndns.start();
        }
        if let Some(installer) = installer {
            let (flag, stop2) = (services.restart_requested.clone(), stop.clone());
            services.updates.set_auto(crate::admin::AutoDeps {
                installer,
                quiet: Arc::new(ServerQuiet {
                    ocr: services.ocr.clone(),
                    writes: services.writes.clone(),
                }),
                restart: Arc::new(move || {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    stop2.cancel();
                }),
                storage: services.core.layout.base.clone(),
            });
        }
        services.updates.start(stop.clone());
        start_library_services(services, stop.clone())
    };
    crate::serve::serve(listener, app, tls, stop, Duration::from_secs(5)).await?;
    // Ordered shutdown (0.5.2 shutdown_app order, plus the new services).
    let _ = thumbs.await;
    services.ocr.stop().await;
    services.library.stop().await;
    services.dyndns.stop();
    services.dav.shutdown();
    services.updates.stop().await;
    services.tunnel.stop().await;
    Ok(())
}

/// How long the library services wait for a cloud store's first sync before they start
/// on what the index holds so far.
pub const CLOUD_STEADY_WAIT: Duration = Duration::from_secs(15 * 60);
/// How often that wait logs its progress.
const CLOUD_STEADY_LOG_EVERY: Duration = Duration::from_secs(30);

/// Start the library runtime (watcher, metadata timers), the cover loop and the OCR
/// scheduler. The returned task ends with the cover loop (at `stop`).
///
/// On a library on this disk they start at once, after 0.5.2's one-shot corrupt-sidecar
/// sweep. On a cloud library they start once the store's change feed is STEADY (or after
/// [`CLOUD_STEADY_WAIT`], with a warning), so the first metadata compile and the
/// scheduler's first census see the whole library; admin, health and setup are served
/// meanwhile. The sweep is skipped (D13: corrupt sidecars are report-only on remote)
/// and covers are made as archives arrive, not by a periodic walk.
pub fn start_library_services(
    services: &Services,
    stop: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let config = services.core.config.clone();
    let library_dir = services.core.layout.library();
    let Some(storage) = services.storage.clone() else {
        services.library.start();
        {
            // 0.5.2 swept corrupt sidecars when the OCR worker started; one-shot, off the
            // request path.
            let (lib, db) = (library_dir.clone(), services.db.clone());
            tokio::task::spawn_blocking(move || {
                crate::thumbs::remove_corrupt_sidecars(&lib, Some(&db))
            });
        }
        let thumbs = services
            .thumbs
            .spawn(config, library_dir, stop.child_token());
        services.ocr.start(stop.child_token());
        return thumbs;
    };
    let (library, thumbs, ocr) = (
        services.library.clone(),
        services.thumbs.clone(),
        services.ocr.clone(),
    );
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        loop {
            let h = storage.clone();
            let wait = tokio::task::spawn_blocking(move || h.wait_steady(CLOUD_STEADY_LOG_EVERY));
            let steady = tokio::select! {
                _ = stop.cancelled() => return,
                r = wait => r.unwrap_or(false),
            };
            if steady {
                if storage.connected() {
                    info!(
                        "[STORAGE] The {} library is in sync: starting the library services",
                        storage.backend.as_str()
                    );
                } else {
                    info!(
                        "[STORAGE] The {} library is not connected: starting the library services on an empty library",
                        storage.backend.as_str()
                    );
                }
                break;
            }
            if started.elapsed() >= CLOUD_STEADY_WAIT {
                tracing::warn!(
                    "[STORAGE] The {} library is still not in sync after {} min ({:?}): starting the library services on what is indexed so far",
                    storage.backend.as_str(),
                    CLOUD_STEADY_WAIT.as_secs() / 60,
                    storage.engine_state()
                );
                break;
            }
            info!(
                "[STORAGE] Waiting for the first sync of the {} library: {:?}",
                storage.backend.as_str(),
                storage.engine_state()
            );
        }
        library.start();
        let covers = thumbs.spawn_remote(library_dir, stop.child_token());
        ocr.start(stop.child_token());
        publish_cloud_queue(ocr, storage, stop.child_token());
        let _ = covers.await;
    })
}

/// How often the cloud copy of the queue file is offered to the store (which writes it
/// only on change, at most once per provider floor).
pub const CLOUD_QUEUE_EVERY: Duration = Duration::from_secs(2);

/// Keep the cloud library's `.mokuro-queue.json` current (§9.1): the scheduler's queue
/// document, as the reader's WebDAV copy is built, until `stop`.
fn publish_cloud_queue(
    ocr: crate::ocr::OcrControl,
    storage: Arc<crate::storage::StoreHandle>,
    stop: CancellationToken,
) {
    if !storage.connected() {
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Some(((held, volumes, pending), now)) =
                ocr.ask(|s| (s.queue_document(), s.now())).await
            {
                let doc = serde_json::Value::Object(crate::ocr::queue_file::build_document(
                    held, volumes, pending, now,
                ));
                let st = storage.clone();
                match tokio::task::spawn_blocking(move || st.publish_queue_doc(doc)).await {
                    Ok(Err(e)) => tracing::debug!("[STORAGE] queue file not published: {e}"),
                    Err(e) => tracing::warn!("[STORAGE] queue file task: {e}"),
                    Ok(Ok(_)) => {}
                }
            }
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(CLOUD_QUEUE_EVERY) => {}
            }
        }
    });
}

/// Build every module router and the WebDAV fallback for these services. Modules are
/// added here as they land; the order is 0.5.2's precedence (spec http-webdav §2.1).
pub fn assemble(services: &Services, opts: &ServeOptions) -> Router {
    let ocr = services.ocr.clone();
    let refused: RefusalHook = {
        let ocr = ocr.clone();
        Arc::new(move |u: &str, why: &str| ocr.record_failed_login(u, why))
    };
    let _ = PROCESSOR_REFUSALS.set(refused.clone());
    let drop: crate::admin::DropProcessors = {
        let ocr = ocr.clone();
        Arc::new(move |u: &str, why: &str| ocr.drop_account(u, why))
    };
    let mut accounts =
        crate::accounts::AccountsDeps::new(services.core.clone(), services.db.clone());
    accounts.setup = services.setup.clone();
    accounts.library = Some(services.library.counts());
    accounts.health = Some(Arc::new(ocr.clone()));
    accounts.hooks.on_processor_login_refused = Some(refused);
    accounts.hooks.drop_processor_account = Some(drop.clone());
    let mut library = crate::library::LibraryDeps::new(
        services.core.clone(),
        services.db.clone(),
        services.library.clone(),
    );
    let glue = Arc::new(crate::glue::OcrGlue(ocr.clone()));
    library.ocr_status = Some(glue.clone());
    library.outlook = Some(glue);
    let core_for_layers = services.core.clone();
    library.layer_order = Some(Arc::new(move || {
        core_for_layers
            .config
            .read()
            .ocr
            .generations
            .iter()
            .filter(|g| g.runnable() && !g.primary)
            .map(|g| g.name.clone())
            .collect()
    }));
    let restart = {
        let flag = services.restart_requested.clone();
        let stop = services.stop.clone();
        Arc::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            stop.cancel();
        }) as Arc<dyn Fn() + Send + Sync>
    };
    if let Some(m) = &opts.machine {
        m.attach(crate::machine::MachineHooks {
            restart: restart.clone(),
            ocr: ocr.clone(),
        });
    }
    accounts.machine = opts.machine.clone();
    accounts.dyndns = Some(services.dyndns.clone());
    accounts.restart = Some(restart.clone());
    services.storage_control.set_restart(restart.clone());
    accounts.storage = Some(services.storage_control.clone());
    let admin = crate::admin::AdminDeps {
        core: services.core.clone(),
        db: services.db.clone(),
        ocr: Arc::new(ocr.clone()),
        tunnel: Some(services.tunnel.clone()),
        dyndns: Some(services.dyndns.clone()),
        updates: Some(services.updates.clone()),
        drop_processors: Some(drop),
        restart: Some(restart),
        machine: opts.machine.clone(),
        storage: services.storage.clone(),
        storage_control: Some(services.storage_control.clone()),
    };
    let mut modules: Vec<Router> = vec![
        crate::accounts::router(accounts.clone()),
        crate::ocr::queue_router(ocr.clone()),
        crate::library::router(library.clone()),
        crate::ocr::processor_router(ocr.clone()),
    ];
    // 0.5.2 mounted the admin panel and API only with `admin.enabled` (server.py);
    // off, `/_admin…` still passes the auth gate and then falls through to WebDAV.
    if services.core.config.read().admin.enabled {
        modules.push(crate::admin::router(admin));
    }
    let queue_file = crate::ocr::queue_file::QueueFileState {
        core: services.core.clone(),
        ocr: Some(ocr.clone()),
    };
    let dav = dav_handler(services.dav.clone(), services.dav_hooks.clone());
    let catalog = library.clone();
    let writes = services.writes.clone();
    let path_case = services.dav.path_case().clone();
    build_router_with(
        services.core.clone(),
        modules,
        dav,
        Some(library),
        move |r| {
            r.layer(axum::middleware::from_fn_with_state(
                queue_file,
                crate::ocr::queue_file::middleware,
            ))
            // 0.5.3 PathCaseMiddleware, in its place: inside the catalog, outside the
            // queue file, the auth gate, the series.json PUT and WebDAV.
            .layer(axum::middleware::from_fn_with_state(
                path_case,
                path_case_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(writes, count_writes))
            .layer(axum::middleware::from_fn_with_state(
                catalog,
                crate::library::catalog_middleware,
            ))
            .layer(axum::middleware::from_fn_with_state(
                accounts,
                crate::accounts::root_middleware,
            ))
        },
    )
}

/// When nobody can sign in yet: a new one-time setup code ([`crate::accounts::SetupFlag`]),
/// printed once in a banner with the address to open, so the wizard is reachable from
/// another computer (Docker, a NAS). Also removes the 0.7 betas' `.setup-token` file.
pub fn announce_setup(services: &Services) {
    crate::accounts::remove_legacy_token(&services.core.layout);
    if !services.setup.needs_setup(&services.db).unwrap_or(false) {
        return;
    }
    let (host, port, ssl) = {
        let c = services.core.config.read();
        (c.server.host.clone(), c.server.port, c.ssl.enabled)
    };
    let code = services.setup.issue_code();
    for line in setup_banner(&host, port, ssl, &code, in_container(), behind_nginx()) {
        info!("{line}");
    }
}

/// The first-run banner's lines (the code appears in exactly one of them).
pub fn setup_banner(
    host: &str,
    port: u16,
    ssl: bool,
    code: &str,
    container: bool,
    nginx: bool,
) -> Vec<String> {
    let scheme = if ssl { "https" } else { "http" };
    let any = matches!(host, "0.0.0.0" | "::" | "[::]" | "");
    let local_only = matches!(host, "127.0.0.1" | "::1" | "localhost");
    // Behind the image's bundled nginx the server listens on an internal port.
    let shown_host = if any || nginx {
        "<this server's address>".to_string()
    } else if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let shown_port = if nginx {
        "<web UI port>".to_string()
    } else {
        port.to_string()
    };
    let rule = "=".repeat(78);
    let mut lines = vec![
        rule.clone(),
        format!(
            "First run: create the admin account at {scheme}://{shown_host}:{shown_port}/setup (setup code: {code})"
        ),
    ];
    if container {
        lines.push(if nginx {
            "  In Docker: the host's address and the host port mapped to the container's web UI port."
                .to_string()
        } else {
            format!(
                "  In Docker: the host's address and the host port mapped to the container's port {port}."
            )
        });
    }
    if local_only {
        lines.push(
            "  The server listens on this computer only: open it here (no code needed).".into(),
        );
    } else {
        lines.push(format!(
            "  On the server itself, {scheme}://localhost:{shown_port}/setup needs no code."
        ));
    }
    lines.push("  The code works until an admin exists; every start makes a new one.".into());
    lines.push(rule);
    lines
}

/// Running in a container (Docker's `/.dockerenv`, Podman's `/run/.containerenv`, or
/// the images' `MOKURO_INSTALL_KIND=docker`).
// unmounted: container marker files at the filesystem root.
#[allow(clippy::disallowed_methods)]
fn in_container() -> bool {
    std::path::Path::new("/.dockerenv").exists()
        || std::path::Path::new("/run/.containerenv").exists()
        || std::env::var("MOKURO_INSTALL_KIND").is_ok_and(|k| k == "docker")
}

/// The image's entrypoint put nginx in front (`MOKURO_NGINX_ACCEL`).
fn behind_nginx() -> bool {
    std::env::var("BUNKO_NGINX_RUNNING").is_ok_and(|v| v == "1")
}

pub fn not_found_json() -> Response {
    let mut r = Response::new(Body::from(r#"{"error": "Not found"}"#));
    *r.status_mut() = StatusCode::NOT_FOUND;
    r.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    r
}
