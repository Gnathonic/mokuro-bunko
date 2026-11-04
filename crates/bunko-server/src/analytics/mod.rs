//! Server analytics (docs/design/analytics.md): what is downloaded and read, by whom,
//! what each kind of traffic costs, how healthy the server is, and who crawls it.
//!
//! - [`layer`] (the second-outermost layer of the router) sees every request, WebDAV
//!   and web alike, and wraps both bodies to count their bytes;
//! - when a response body ends (or is dropped: a client abort) one record is folded
//!   into the in-memory [`collector`] window, under one short lock, with no I/O;
//! - [`flush`] writes the window to `analytics.sqlite` ([`bunko_db::analytics`]) every
//!   `analytics.flush_seconds`, and rolls up and prunes once an hour;
//! - the areas ([`reading`], [`library`], [`host`], [`crawl`], [`clients`], [`geo`])
//!   hang off the [`Analytics`] handle; the admin API is [`api`].
//!
//! `analytics.enabled: false` makes the layer pass requests through untouched; what is
//! stored stays viewable.

pub mod api;
mod body;
pub mod classify;
pub mod clients;
pub(crate) mod collector;
pub mod crawl;
mod flush;
pub mod gauges;
pub mod geo;
pub mod host;
mod layer;
pub mod library;
pub mod principal;
pub mod reading;

pub use gauges::Gauges;
pub use layer::layer;

use crate::core::{Core, RequestCtx};
use bunko_core::Role;
use bunko_core::config::AnalyticsConfig;
use bunko_db::Database;
use bunko_db::analytics::{AnalyticsDb, MaintainStats, Retention};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// The identity [`RequestCtx::from_request_parts`] resolved for a request, put where
/// the accounting layer (outside every layer that authenticates) can read it after the
/// response: a request extension the layer inserts and the extractor fills once.
#[derive(Clone, Default)]
pub struct AnalyticsSlot(pub(crate) Arc<OnceLock<SlotIdentity>>);

/// What [`AnalyticsSlot`] holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotIdentity {
    pub username: Option<String>,
    pub role: Role,
    pub client_ip: String,
    /// A credential was presented and refused.
    pub auth_error: Option<String>,
    pub attempted_username: Option<String>,
}

impl AnalyticsSlot {
    /// Record `ctx` (the first resolution wins; later ones are the same identity).
    pub fn fill(&self, ctx: &RequestCtx) {
        let _ = self.0.set(SlotIdentity {
            username: ctx.identity.username().map(str::to_string),
            role: ctx.identity.role(),
            client_ip: ctx.client_ip.clone(),
            auth_error: ctx.identity.error.clone(),
            attempted_username: ctx
                .identity
                .attempted_username
                .as_deref()
                .map(principal::attempted_username_text),
        });
    }

    pub fn get(&self) -> Option<&SlotIdentity> {
        self.0.get()
    }
}

/// A response extension a login endpoint puts on its 401/429: the account the refused
/// attempt named (from the JSON body, which the accounting layer never reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginAttempt(pub String);

/// What the analytics need from the rest of the server.
#[derive(Clone)]
pub struct AnalyticsDeps {
    pub core: Core,
    /// The main database (`volume_identities`, `volume_uploads`).
    pub db: Arc<Database>,
    /// The library runtime (its index: series, volumes, counts); `None` in tests that
    /// run without one.
    pub library: Option<Arc<crate::library::LibraryRuntime>>,
}

/// The analytics service: the collector, the database, the areas. Cheap to clone.
#[derive(Clone)]
pub struct Analytics(pub(crate) Arc<Inner>);

pub(crate) struct Inner {
    pub deps: AnalyticsDeps,
    pub db: Arc<AnalyticsDb>,
    enabled: AtomicBool,
    pub collector: collector::Collector,
    pub gauges: Arc<Gauges>,
    pub reading: reading::Reading,
    pub library: library::Scanner,
    pub host: host::Host,
    pub crawl: crawl::Detector,
    pub clients: clients::Clients,
    pub geo: geo::Geo,
    /// The flush task, its stop token, and whether `stop` ran.
    task: Mutex<Option<(CancellationToken, tokio::task::JoinHandle<()>)>>,
}

/// Epoch seconds now.
pub fn now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Epoch milliseconds now.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn retention_of(cfg: &AnalyticsConfig) -> Retention {
    Retention {
        ip_days: cfg.retention.ip_days,
        hourly_days: cfg.retention.hourly_days,
        host_minute_days: cfg.retention.host_minute_days,
        crawl_days: cfg.retention.crawl_days,
    }
}

impl Analytics {
    /// Open `analytics.sqlite` beside the main database. A file that cannot be opened
    /// is moved aside and made again; if even that fails the analytics live in memory
    /// for this run (logged): the server never fails to start over its analytics. Only
    /// a process that cannot run SQLite at all gets an error.
    pub fn open(deps: AnalyticsDeps) -> anyhow::Result<Analytics> {
        let path = deps
            .core
            .layout
            .database()
            .with_file_name(bunko_db::analytics::FILE_NAME);
        let db = match AnalyticsDb::open(&path) {
            Ok(db) => db,
            Err(first) => {
                let aside = path.with_extension(format!("sqlite.broken-{}", now_s()));
                tracing::warn!(
                    "[ANALYTICS] {} cannot be opened ({first}); moving it to {} and starting a new one",
                    path.display(),
                    aside.display()
                );
                let reopened = bunko_store::fs::rename(&path, &aside)
                    .map_err(|e| e.to_string())
                    .and_then(|()| AnalyticsDb::open(&path).map_err(|e| e.to_string()));
                match reopened {
                    Ok(db) => db,
                    Err(e) => {
                        tracing::warn!(
                            "[ANALYTICS] {} cannot be made either ({e}): analytics are kept in memory until the server stops",
                            path.display()
                        );
                        AnalyticsDb::open_in_memory()?
                    }
                }
            }
        };
        Ok(Self::with_db(deps, db))
    }

    /// Over an already open database (tests, the fallbacks).
    pub fn with_db(deps: AnalyticsDeps, db: AnalyticsDb) -> Analytics {
        let enabled = deps.core.config.read().analytics.enabled;
        let gauges = Arc::new(Gauges::default());
        let counts = deps.library.as_ref().map(|l| l.counts());
        Analytics(Arc::new(Inner {
            reading: reading::Reading::new(),
            library: library::Scanner::new(),
            host: host::Host::new(gauges.clone()),
            crawl: crawl::Detector::new(deps.core.clone(), counts),
            clients: clients::Clients::new(deps.core.clone()),
            geo: geo::Geo::new(deps.core.clone()),
            collector: collector::Collector::default(),
            db: Arc::new(db),
            enabled: AtomicBool::new(enabled),
            gauges,
            deps,
            task: Mutex::new(None),
        }))
    }

    /// Is the accounting layer counting (`analytics.enabled`, cached)?
    pub fn enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Relaxed)
    }

    /// Re-read the `analytics:` settings after an admin edit (the enabled flag is
    /// cached for the hot path; the areas re-read what they cache).
    pub fn settings_changed(&self) {
        let cfg = self.config();
        self.0.enabled.store(cfg.enabled, Ordering::Relaxed);
        self.0.geo.settings_changed(&cfg);
    }

    /// The live `analytics:` settings.
    pub fn config(&self) -> AnalyticsConfig {
        self.0.deps.core.config.read().analytics.clone()
    }

    pub fn db(&self) -> &Arc<AnalyticsDb> {
        &self.0.db
    }

    pub fn core(&self) -> &Core {
        &self.0.deps.core
    }

    /// The main database.
    pub fn main_db(&self) -> &Arc<Database> {
        &self.0.deps.db
    }

    pub fn library_runtime(&self) -> Option<&Arc<crate::library::LibraryRuntime>> {
        self.0.deps.library.as_ref()
    }

    pub fn gauges(&self) -> &Arc<Gauges> {
        &self.0.gauges
    }

    pub fn reading(&self) -> &reading::Reading {
        &self.0.reading
    }

    pub fn library_scanner(&self) -> &library::Scanner {
        &self.0.library
    }

    pub fn host(&self) -> &host::Host {
        &self.0.host
    }

    pub fn crawl(&self) -> &crawl::Detector {
        &self.0.crawl
    }

    pub fn clients(&self) -> &clients::Clients {
        &self.0.clients
    }

    pub fn geo(&self) -> &geo::Geo {
        &self.0.geo
    }

    /// Start the flush task and the areas' workers (beside `services.updates.start`,
    /// never inside the library services, which may wait for a cloud store).
    pub fn start(&self, stop: CancellationToken) {
        let token = stop.child_token();
        self.0.reading.start(self, token.clone());
        self.0.library.start(self, token.clone());
        self.0.host.start(self, token.clone());
        self.0.geo.start(self, token.clone());
        let handle = tokio::spawn(flush::run(self.clone(), token.clone()));
        if let Some((old, _)) = self.0.task.lock().replace((token, handle)) {
            old.cancel();
        }
    }

    /// Stop the workers and write what is left (shutdown: after `serve` returns, so
    /// every response has recorded, and before the library and the databases go).
    pub async fn stop(&self) {
        let task = self.0.task.lock().take();
        if let Some((token, handle)) = task {
            token.cancel();
            let _ = handle.await;
        }
        if let Err(e) = self.flush_now().await {
            tracing::warn!("[ANALYTICS] final flush failed: {e}");
        }
    }

    /// Write the current window now (the periodic flush; tests).
    pub async fn flush_now(&self) -> anyhow::Result<()> {
        let window = self.0.collector.take();
        let a = self.clone();
        tokio::task::spawn_blocking(move || flush::write_window(&a, window, now_s()))
            .await
            .map_err(|e| anyhow::anyhow!("flush task: {e}"))?
            .map_err(anyhow::Error::from)
    }

    /// Roll up and prune as of `now_s` (the hourly upkeep; tests pass a future time).
    pub async fn maintain_at(&self, now_s: i64) -> anyhow::Result<MaintainStats> {
        let db = self.0.db.clone();
        let retention = retention_of(&self.config());
        Ok(
            tokio::task::spawn_blocking(move || db.maintain(now_s, &retention))
                .await
                .map_err(|e| anyhow::anyhow!("maintenance task: {e}"))??,
        )
    }
}
