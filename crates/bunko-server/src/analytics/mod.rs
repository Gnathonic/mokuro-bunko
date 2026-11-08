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
//!   hang off the [`Analytics`] handle; the admin API is [`api`];
//! - [`popular`] keeps the public catalog's "Popular" order in memory.
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
pub mod popular;
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// A response extension a login endpoint puts on its 200: the account that just signed
/// in. Its credentials were in the body, so the request resolved no identity; the
/// accounting layer attributes it to this account, not to an anonymous visitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub username: String,
    pub role: bunko_core::Role,
}

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
    /// [`bunko_db::analytics::traffic::ANON_PER_DAY`] (tests lower it).
    pub(crate) anon_per_day: AtomicU64,
    /// The settings changed: the flush task works its wait out again.
    pub(crate) flush_wake: tokio::sync::Notify,
    pub collector: collector::Collector,
    pub gauges: Arc<Gauges>,
    pub reading: reading::Reading,
    pub library: library::Scanner,
    pub host: host::Host,
    pub crawl: crawl::Detector,
    pub clients: clients::Clients,
    pub geo: geo::Geo,
    /// The catalog's "Popular" order, refreshed off the request path.
    pub popular: popular::Popularity,
    /// The flush task, its stop token, and whether `stop` ran.
    task: Mutex<Option<(CancellationToken, tokio::task::JoinHandle<()>)>>,
}

/// Move a damaged `analytics.sqlite` and its `-wal`/`-shm` to
/// `analytics.sqlite.broken-<ts>[-<n>]` (never over an earlier one), so the new file
/// inherits none of them. Returns the new name of the main file.
fn move_aside(path: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ts = now_s();
    let mut n = 0u32;
    let aside = loop {
        let base = if n == 0 {
            format!("{name}.broken-{ts}")
        } else {
            format!("{name}.broken-{ts}-{n}")
        };
        let candidate = path.with_file_name(&base);
        let taken = ["", "-wal", "-shm"]
            .iter()
            .any(|s| bunko_store::fs::exists(path.with_file_name(format!("{base}{s}"))));
        if !taken {
            break candidate;
        }
        n += 1;
        if n > 1000 {
            return Err(std::io::Error::other("no free name to move it to"));
        }
    };
    let aside_name = aside
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    bunko_store::fs::rename(path, &aside)?;
    for side in ["-wal", "-shm"] {
        let from = path.with_file_name(format!("{name}{side}"));
        if bunko_store::fs::exists(&from) {
            bunko_store::fs::rename(&from, path.with_file_name(format!("{aside_name}{side}")))?;
        }
    }
    Ok(aside)
}

/// When `name` is one of the files [`move_aside`] makes of `file` (the main file, its
/// `-wal` or its `-shm`): the epoch seconds it was moved aside.
fn moved_aside_at(file: &str, name: &str) -> Option<i64> {
    let rest = name.strip_prefix(file)?.strip_prefix(".broken-")?;
    let rest = rest
        .strip_suffix("-wal")
        .or_else(|| rest.strip_suffix("-shm"))
        .unwrap_or(rest);
    let (ts, n) = rest.split_once('-').unwrap_or((rest, "0"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (digits(ts) && digits(n)).then(|| ts.parse().ok()).flatten()
}

/// Delete what [`move_aside`] kept beside `path` once it was moved aside `ip_days` or
/// more ago: a damaged file holds raw IPs and the salt like the live one, so it is kept
/// no longer than the live file keeps its raw IPs. Returns how many files went.
fn remove_old_moved_aside(path: &std::path::Path, now_s: i64, ip_days: u32) -> usize {
    let (Some(dir), Some(file)) = (path.parent(), path.file_name()) else {
        return 0;
    };
    let file = file.to_string_lossy();
    let Ok(entries) = bunko_store::fs::read_dir(dir) else {
        return 0;
    };
    let cut = now_s - i64::from(ip_days) * 86_400;
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !moved_aside_at(&file, &name).is_some_and(|at| at <= cut) {
            continue;
        }
        match bunko_store::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(
                "[ANALYTICS] cannot delete {} (a damaged analytics file older than the IP retention): {e}",
                entry.path().display()
            ),
        }
    }
    if removed > 0 {
        tracing::info!(
            "[ANALYTICS] deleted {removed} damaged analytics file(s) moved aside more than {ip_days} days ago (they held raw IPs)"
        );
    }
    removed
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
    /// Open `analytics.sqlite` beside the main database. A damaged file (SQLite says
    /// corrupt or not a database, or `quick_check` fails) is moved aside with its
    /// `-wal` and `-shm` and made again; a healthy file that cannot be opened now
    /// (locked by another process, read-only, unreachable) stays where it is, and the
    /// analytics live in memory for this run (logged), as they do when even a new file
    /// cannot be made: the server never fails to start over its analytics. Only a
    /// process that cannot run SQLite at all gets an error.
    pub fn open(deps: AnalyticsDeps) -> anyhow::Result<Analytics> {
        let path = deps
            .core
            .layout
            .database()
            .with_file_name(bunko_db::analytics::FILE_NAME);
        let in_memory = |why: &str| -> anyhow::Result<AnalyticsDb> {
            tracing::warn!(
                "[ANALYTICS] {} {why}: analytics are kept in memory until the server stops",
                path.display()
            );
            Ok(AnalyticsDb::open_in_memory()?)
        };
        let db = match AnalyticsDb::open(&path) {
            Ok(db) => db,
            Err(first) if first.is_corrupt() => {
                let moved = move_aside(&path);
                match &moved {
                    Ok(aside) => tracing::warn!(
                        "[ANALYTICS] {} is damaged ({first}); moved it to {} and starting a new one",
                        path.display(),
                        aside.display()
                    ),
                    Err(e) => tracing::warn!(
                        "[ANALYTICS] {} is damaged ({first}) and cannot be moved aside ({e})",
                        path.display()
                    ),
                }
                match moved.map(|_| AnalyticsDb::open(&path)) {
                    Ok(Ok(db)) => db,
                    Ok(Err(e)) => in_memory(&format!("cannot be made either ({e})"))?,
                    Err(_) => in_memory("stays as it is")?,
                }
            }
            Err(e) => in_memory(&format!(
                "cannot be opened ({e}); it is left as it is, its history untouched"
            ))?,
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
            popular: popular::Popularity::default(),
            collector: collector::Collector::default(),
            db: Arc::new(db),
            enabled: AtomicBool::new(enabled),
            anon_per_day: AtomicU64::new(bunko_db::analytics::traffic::ANON_PER_DAY),
            flush_wake: tokio::sync::Notify::new(),
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
    /// cached for the hot path; the areas re-read what they cache; a new flush or scan
    /// interval applies to the wait already running).
    pub fn settings_changed(&self) {
        let cfg = self.config();
        self.0.enabled.store(cfg.enabled, Ordering::Relaxed);
        self.0.geo.settings_changed(&cfg);
        self.0.flush_wake.notify_one();
        self.0.library.settings_changed();
    }

    /// Lower the day's anonymous-visitor cap (tests: the real one is 10,000).
    #[doc(hidden)]
    pub fn set_anon_per_day(&self, n: u64) {
        self.0.anon_per_day.store(n, Ordering::Relaxed);
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

    /// The catalog's "Popular" order (what `/catalog/api/library` ranks by).
    pub fn popularity(&self) -> &popular::Popularity {
        &self.0.popular
    }

    /// Compute the "Popular" order again now (start, the hourly upkeep, a new UTC
    /// day; tests). A failure keeps the order there was.
    pub async fn refresh_popular(&self) {
        let a = self.clone();
        let done = tokio::task::spawn_blocking(move || a.0.popular.refresh(&a.0.db, now_s())).await;
        match done {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("[ANALYTICS] popular series not refreshed: {e}"),
            Err(e) => tracing::warn!("[ANALYTICS] popular series task failed: {e}"),
        }
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
            // The reading worker reads what is still waiting its debounce first.
            self.0.reading.stopped().await;
        }
        if let Err(e) = self.flush_now().await {
            tracing::warn!("[ANALYTICS] final flush failed: {e}");
        }
    }

    /// Write the current window now (the periodic flush; tests).
    pub async fn flush_now(&self) -> anyhow::Result<()> {
        let window = self.0.collector.take();
        // New downloads can change the catalog's "Popular" order.
        let downloads = !window.downloads.is_empty();
        let a = self.clone();
        tokio::task::spawn_blocking(move || {
            let now = now_s();
            flush::write_window(&a, window, now)?;
            if downloads && let Err(e) = a.0.popular.refresh(&a.0.db, now) {
                tracing::warn!("[ANALYTICS] popular series not refreshed: {e}");
            }
            Ok::<_, bunko_db::DbError>(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("flush task: {e}"))?
        .map_err(anyhow::Error::from)
    }

    /// Roll up and prune as of `now_s` (the hourly upkeep; tests pass a future time),
    /// and delete the damaged files moved aside `retention.ip_days` ago.
    pub async fn maintain_at(&self, now_s: i64) -> anyhow::Result<MaintainStats> {
        let db = self.0.db.clone();
        let retention = retention_of(&self.config());
        let path = self
            .0
            .deps
            .core
            .layout
            .database()
            .with_file_name(bunko_db::analytics::FILE_NAME);
        Ok(tokio::task::spawn_blocking(move || {
            remove_old_moved_aside(&path, now_s, retention.ip_days);
            let stats = db.maintain(now_s, &retention)?;
            // The prune may have been the biggest write of the day: cut the WAL.
            db.checkpoint()?;
            Ok::<_, bunko_db::DbError>(stats)
        })
        .await
        .map_err(|e| anyhow::anyhow!("maintenance task: {e}"))??)
    }
}
