//! [`LibraryRuntime`]: the metadata service with its timers, the library index, the
//! filesystem watcher and the community fetcher (0.5.2 `server.py` wiring plus the
//! timer half of `metadata/service.py`).
//!
//! Timers are tokio tasks armed by a [`Debouncer`] each (one for the full pass, one
//! per series key). A fired timer clears its slot under the timer lock BEFORE the pass
//! runs, so a later schedule arms a fresh timer instead of cancelling a running pass;
//! the pass itself runs on the blocking pool and the service's own locks serialise it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use bunko_core::Config;
use bunko_db::Database;
use bunko_library::compat::normalize_volume_title_key;
use bunko_library::paths::{LibraryChange, classify_change};
use bunko_library::{
    DebouncePolicy, Debouncer, LibraryIndexCache, MetadataHooks, MetadataService, PathWriteLocks,
    UpdateError,
};
use parking_lot::{Mutex, RwLock};
use tokio::runtime::Handle;
use tokio::task::AbortHandle;

use super::ArchiveEvents;
use super::community::{CommunityFetcher, CommunitySettings};
use super::store::DbMetadataStore;
use super::watcher::{LibraryWatcher, WatchEvent};
use crate::accounts::LibraryCounts;
use crate::core::Core;

/// Callbacks into services this module does not own. All optional; all called from
/// non-async threads (the watcher, the blocking pool), so they must not block.
#[derive(Clone, Default)]
pub struct LibraryHooks {
    /// The PROPFIND cache's `schedule_refresh(5.0)`: after every library change and
    /// every publish. Detached once [`LibraryRuntime::stop`] returns, so a late publish
    /// cannot arm a refresh after the cache stopped (0.5.2 shutdown note).
    pub propfind_refresh: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Extra work after compiled files changed (the queue page's `invalidate_skipped`).
    pub on_published: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Archive arrivals/removals for the OCR queue.
    pub archive_events: Option<Arc<dyn ArchiveEvents>>,
}

/// What [`LibraryRuntime::new`] needs.
pub struct RuntimeDeps {
    pub core: Core,
    pub db: Arc<Database>,
    /// The WebDAV per-path write lock table ([`bunko_library::service::NoPathLocks`]
    /// until bunko-dav is wired).
    pub locks: Arc<dyn PathWriteLocks>,
    pub hooks: LibraryHooks,
    pub policy: DebouncePolicy,
    pub community: CommunitySettings,
    /// Watch the library with `notify` (tests that drive changes by hand turn it off).
    pub watch: bool,
}

impl RuntimeDeps {
    /// 0.5.2's timings, no hooks, no DAV locks, watcher on.
    pub fn new(core: Core, db: Arc<Database>) -> Self {
        Self {
            core,
            db,
            locks: Arc::new(bunko_library::service::NoPathLocks),
            hooks: LibraryHooks::default(),
            policy: DebouncePolicy::default(),
            community: CommunitySettings::default(),
            watch: true,
        }
    }
}

#[derive(Default)]
struct Slot {
    debouncer: Debouncer,
    generation: u64,
    task: Option<AbortHandle>,
}

impl Slot {
    fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Default)]
struct Timers {
    stopped: bool,
    generation: u64,
    full: Slot,
    /// Per series key: the slot and the latest raw title scheduled for it.
    series: HashMap<String, (Slot, String)>,
    periodic: Option<AbortHandle>,
}

/// The library's background runtime. Create with [`LibraryRuntime::new`] inside the
/// tokio runtime, then [`start`](Self::start) it.
pub struct LibraryRuntime {
    me: Weak<LibraryRuntime>,
    config: Arc<RwLock<Config>>,
    library: PathBuf,
    library_resolved: OnceLock<PathBuf>,
    db: Arc<Database>,
    store: Arc<DbMetadataStore>,
    service: Arc<MetadataService>,
    index: Arc<LibraryIndexCache>,
    policy: DebouncePolicy,
    hooks: LibraryHooks,
    hooks_live: AtomicBool,
    handle: OnceLock<Handle>,
    timers: Mutex<Timers>,
    watcher: Mutex<Option<LibraryWatcher>>,
    watch: bool,
    community_settings: CommunitySettings,
    community: Mutex<Option<Arc<CommunityFetcher>>>,
    started: AtomicBool,
}

/// The service's hooks, pointing back at the runtime.
struct ServiceHooks(Weak<LibraryRuntime>);

impl MetadataHooks for ServiceHooks {
    fn schedule_regeneration(&self, delay: Option<Duration>) {
        if let Some(rt) = self.0.upgrade() {
            rt.schedule_regeneration(delay);
        }
    }

    fn on_published(&self) {
        if let Some(rt) = self.0.upgrade() {
            rt.on_metadata_published();
        }
    }

    fn on_external_ids_changed(&self, series_key: &str) {
        if let Some(rt) = self.0.upgrade() {
            let fetcher = rt.community.lock().clone();
            if let Some(fetcher) = fetcher {
                fetcher.request_fetch(series_key);
            }
        }
    }
}

/// The library index's byte budget: a quarter of `server.cache_mb` (the PROPFIND
/// cache and compiled metadata share the rest).
pub fn index_budget_bytes(cache_mb: u32) -> usize {
    (cache_mb as usize).saturating_mul(1024 * 1024) / 4
}

impl LibraryRuntime {
    pub fn new(deps: RuntimeDeps) -> Arc<Self> {
        let library = deps.core.layout.library();
        let cache_mb = deps.core.config.read().server.cache_mb;
        let store = Arc::new(DbMetadataStore::new(deps.db.clone()));
        let index = Arc::new(LibraryIndexCache::new(
            library.clone(),
            bunko_library::index::DEFAULT_TTL,
            index_budget_bytes(cache_mb),
        ));
        Arc::new_cyclic(|me: &Weak<LibraryRuntime>| {
            let service = Arc::new(MetadataService::new(
                library.clone(),
                store.clone(),
                Arc::new(ServiceHooks(me.clone())),
                deps.locks,
                deps.policy,
            ));
            let handle = OnceLock::new();
            if let Ok(current) = Handle::try_current() {
                let _ = handle.set(current);
            }
            LibraryRuntime {
                me: me.clone(),
                config: deps.core.config.clone(),
                library,
                library_resolved: OnceLock::new(),
                db: deps.db,
                store,
                service,
                index,
                policy: deps.policy,
                hooks: deps.hooks,
                hooks_live: AtomicBool::new(true),
                handle,
                timers: Mutex::new(Timers::default()),
                watcher: Mutex::new(None),
                watch: deps.watch,
                community_settings: deps.community,
                community: Mutex::new(None),
                started: AtomicBool::new(false),
            }
        })
    }

    pub fn library_path(&self) -> &Path {
        &self.library
    }

    /// The library root with symlinks resolved (for containment checks and for
    /// classifying resolved paths the DAV layer reports).
    pub fn library_resolved(&self) -> PathBuf {
        if let Some(path) = self.library_resolved.get() {
            return path.clone();
        }
        match std::fs::canonicalize(&self.library) {
            Ok(path) => self.library_resolved.get_or_init(|| path).clone(),
            Err(_) => self.library.clone(),
        }
    }

    pub fn service(&self) -> &Arc<MetadataService> {
        &self.service
    }

    pub fn index(&self) -> &Arc<LibraryIndexCache> {
        &self.index
    }

    pub fn store(&self) -> &Arc<DbMetadataStore> {
        &self.store
    }

    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    pub fn policy(&self) -> &DebouncePolicy {
        &self.policy
    }

    /// The running community fetcher, if `catalog.enabled && catalog.enrich_community`
    /// held at start.
    pub fn community(&self) -> Option<Arc<CommunityFetcher>> {
        self.community.lock().clone()
    }

    /// [`LibraryCounts`] for the accounts module (`/api/stats`, `/api/health`).
    pub fn counts(&self) -> Arc<dyn LibraryCounts> {
        Arc::new(IndexCounts(self.index.clone()))
    }

    fn handle(&self) -> Option<Handle> {
        if let Some(handle) = self.handle.get() {
            return Some(handle.clone());
        }
        let current = Handle::try_current().ok()?;
        Some(self.handle.get_or_init(|| current).clone())
    }

    fn arc(&self) -> Option<Arc<LibraryRuntime>> {
        self.me.upgrade()
    }

    // ------------------------------------------------------------------
    // Lifecycle
    // ------------------------------------------------------------------

    /// Start the watcher, arm the startup pass (20 s) and the periodic rescan (6 h),
    /// and start the community fetcher when `catalog.enabled && enrich_community`.
    /// Call inside the tokio runtime. Idempotent.
    pub fn start(&self) {
        if self.started.swap(true, Ordering::SeqCst) || self.timers.lock().stopped {
            return;
        }
        let Some(handle) = self.handle() else {
            tracing::error!("[METADATA] LibraryRuntime::start called outside a tokio runtime");
            return;
        };
        if self.watch {
            let me = self.me.clone();
            match LibraryWatcher::start(&self.library, move |event| {
                if let Some(rt) = me.upgrade() {
                    rt.on_watch_event(event);
                }
            }) {
                Ok(watcher) => *self.watcher.lock() = Some(watcher),
                Err(error) => {
                    tracing::warn!(%error, "[FS-WATCHER] filesystem watching disabled");
                }
            }
        }
        self.schedule_regeneration(Some(self.policy.startup_delay));
        {
            let me = self.me.clone();
            let period = self.policy.periodic_rescan;
            let task = handle.spawn(async move {
                loop {
                    tokio::time::sleep(period).await;
                    let Some(rt) = me.upgrade() else { return };
                    // Through the debounced path, so a tick and a burst coalesce.
                    rt.schedule_regeneration(None);
                }
            });
            let mut timers = self.timers.lock();
            if timers.stopped {
                task.abort();
            } else {
                timers.periodic = Some(task.abort_handle());
            }
        }
        let enrich = {
            let c = self.config.read();
            c.catalog.enabled && c.catalog.enrich_community
        };
        if enrich {
            let fetcher = CommunityFetcher::new(self.db.clone(), self.community_settings.clone());
            let _guard = handle.enter();
            fetcher.start();
            *self.community.lock() = Some(fetcher);
        }
    }

    /// Stop in 0.5.2's order: the watcher, the community fetcher, the metadata service
    /// (pending timers cancelled, then waits for a running pass), and finally detach
    /// the PROPFIND refresh hook. Safe to call more than once.
    pub async fn stop(&self) {
        let watcher = self.watcher.lock().take();
        if let Some(watcher) = watcher {
            // Dropping joins the backend thread.
            let _ = tokio::task::spawn_blocking(move || drop(watcher)).await;
        }
        let fetcher = self.community.lock().take();
        if let Some(fetcher) = fetcher {
            fetcher.stop().await;
        }
        {
            let mut timers = self.timers.lock();
            timers.stopped = true;
            timers.full.cancel();
            for (slot, _) in timers.series.values_mut() {
                slot.cancel();
            }
            timers.series.clear();
            if let Some(task) = timers.periodic.take() {
                task.abort();
            }
        }
        let service = self.service.clone();
        let _ = tokio::task::spawn_blocking(move || service.stop()).await;
        self.hooks_live.store(false, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.timers.lock().stopped
    }

    // ------------------------------------------------------------------
    // Scheduling
    // ------------------------------------------------------------------

    /// Debounced full pass (`schedule_regeneration`): every call restarts the quiet
    /// period (`delay` or 10 s), never past 60 s after the first call since the last
    /// fire.
    pub fn schedule_regeneration(&self, delay: Option<Duration>) {
        let (Some(handle), Some(rt)) = (self.handle(), self.arc()) else {
            return;
        };
        let mut timers = self.timers.lock();
        if timers.stopped {
            return;
        }
        let wait = timers
            .full
            .debouncer
            .schedule(&self.policy, Instant::now(), delay);
        timers.full.cancel();
        timers.generation += 1;
        let generation = timers.generation;
        timers.full.generation = generation;
        let me = Arc::downgrade(&rt);
        let task = handle.spawn(async move {
            tokio::time::sleep(wait).await;
            if let Some(rt) = me.upgrade() {
                rt.fire_full(generation).await;
            }
        });
        timers.full.task = Some(task.abort_handle());
    }

    /// Debounced single-series recompile (`schedule_series_regeneration`), keyed by
    /// `normalize_volume_title_key(title)`; the latest raw title is what compiles.
    pub fn schedule_series_regeneration(&self, series_title: &str, delay: Option<Duration>) {
        let (Some(handle), Some(rt)) = (self.handle(), self.arc()) else {
            return;
        };
        let key = normalize_volume_title_key(series_title);
        let mut timers = self.timers.lock();
        if timers.stopped {
            return;
        }
        timers.generation += 1;
        let generation = timers.generation;
        let (slot, title) = timers.series.entry(key.clone()).or_default();
        let wait = slot.debouncer.schedule(&self.policy, Instant::now(), delay);
        slot.cancel();
        slot.generation = generation;
        *title = series_title.to_owned();
        let me = Arc::downgrade(&rt);
        let task = handle.spawn(async move {
            tokio::time::sleep(wait).await;
            if let Some(rt) = me.upgrade() {
                rt.fire_series(key, generation).await;
            }
        });
        slot.task = Some(task.abort_handle());
    }

    async fn fire_full(&self, generation: u64) {
        {
            let mut timers = self.timers.lock();
            if timers.stopped || timers.full.generation != generation {
                return;
            }
            timers.full.debouncer.fired();
            timers.full.task = None;
        }
        tracing::info!("[METADATA] full pass fired");
        let service = self.service.clone();
        match tokio::task::spawn_blocking(move || service.recompile_all()).await {
            Ok(Ok(changed)) => tracing::info!("[METADATA] full pass done (changed={changed})"),
            Ok(Err(error)) => tracing::warn!(%error, "[METADATA] regeneration failed"),
            Err(error) => tracing::warn!(%error, "[METADATA] regeneration failed"),
        }
    }

    async fn fire_series(&self, key: String, generation: u64) {
        let title = {
            let mut timers = self.timers.lock();
            if timers.stopped
                || timers
                    .series
                    .get(&key)
                    .is_none_or(|(slot, _)| slot.generation != generation)
            {
                return;
            }
            match timers.series.remove(&key) {
                Some((_, title)) => title,
                None => return,
            }
        };
        tracing::info!("[METADATA] series regen fired: {title}");
        let service = self.service.clone();
        let name = title.clone();
        match tokio::task::spawn_blocking(move || service.recompile_series(&name)).await {
            Ok(Ok(changed)) => {
                tracing::info!("[METADATA] series regen done: {title} (changed={changed})")
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "[METADATA] series regeneration failed: {title}")
            }
            Err(error) => tracing::warn!(%error, "[METADATA] series regeneration failed: {title}"),
        }
    }

    /// Whether a timer (full or any series) is armed. For tests and diagnostics.
    pub fn has_pending_pass(&self) -> bool {
        let timers = self.timers.lock();
        timers.full.task.is_some() || timers.series.values().any(|(slot, _)| slot.task.is_some())
    }

    // ------------------------------------------------------------------
    // Change notifications
    // ------------------------------------------------------------------

    fn propfind_refresh(&self) {
        if self.hooks_live.load(Ordering::SeqCst)
            && let Some(refresh) = &self.hooks.propfind_refresh
        {
            refresh();
        }
    }

    /// `on_metadata_published`: refresh the listings that show compiled files. Never
    /// schedules another regeneration (that would feed itself).
    fn on_metadata_published(&self) {
        self.index.invalidate();
        self.propfind_refresh();
        if self.hooks_live.load(Ordering::SeqCst)
            && let Some(hook) = &self.hooks.on_published
        {
            hook();
        }
    }

    /// `on_library_change` (the watcher): invalidate the index, refresh PROPFIND, and
    /// route the metadata work by what changed.
    pub fn on_library_change(&self, path: &Path) {
        self.index.invalidate();
        self.propfind_refresh();
        self.route_change(path);
    }

    /// A file inside a series recompiles that series; a top-level entry takes the full
    /// pass, which also prunes deleted series; `thumbnails/` is ignored.
    fn route_change(&self, path: &Path) {
        let root = if path.starts_with(&self.library) {
            self.library.clone()
        } else {
            self.library_resolved()
        };
        match classify_change(&root, path) {
            LibraryChange::Series(title) => self.schedule_series_regeneration(&title, None),
            LibraryChange::Library => self.schedule_regeneration(None),
            LibraryChange::Ignore => {}
        }
    }

    fn archive_event(&self, event: &WatchEvent) {
        let Some(events) = &self.hooks.archive_events else {
            return;
        };
        let is_cbz = |path: &Path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| bunko_library::pyunicode::lower(n).ends_with(".cbz"))
        };
        match event {
            WatchEvent::Added {
                path,
                is_dir: false,
            } if is_cbz(path) => events.archive_added(path),
            WatchEvent::Removed {
                path,
                is_dir: false,
            } if is_cbz(path) => events.archive_removed(path),
            WatchEvent::Added { is_dir: true, .. }
            | WatchEvent::Removed { is_dir: true, .. }
            | WatchEvent::Rescan => events.rescan(),
            _ => {}
        }
    }

    /// One watcher event (also what [`on_library_write`](Self::on_library_write) feeds).
    pub fn on_watch_event(&self, event: WatchEvent) {
        if self.is_stopped() {
            return;
        }
        self.archive_event(&event);
        match event.path() {
            Some(path) => self.on_library_change(path),
            None => {
                self.index.invalidate();
                self.propfind_refresh();
                self.schedule_regeneration(None);
            }
        }
    }

    /// The DAV layer committed a change at physical `path` (an upload, delete, either
    /// side of a move, MKCOL). The watcher normally sees the same change; reacting here
    /// too makes the metadata independent of the watcher (duplicates coalesce in the
    /// debounce). The PROPFIND cache and the OCR queue hear about DAV writes from the
    /// DAV layer itself, so neither is notified here.
    pub fn on_library_write(&self, path: &Path) {
        if self.is_stopped() {
            return;
        }
        self.index.invalidate();
        let is_dir = std::fs::metadata(path)
            .map(|m| m.is_dir())
            .unwrap_or_else(|_| super::watcher::vanished_is_dir(path));
        if bunko_library::paths::is_relevant_change(path, is_dir) {
            self.route_change(path);
        }
    }

    // ------------------------------------------------------------------
    // Requests
    // ------------------------------------------------------------------

    /// A client `series.json` PUT (`apply_series_update`), on the blocking pool.
    /// `Ok(true)` accepted (204), `Ok(false)` refused (400), `Err(Busy)` 503.
    pub async fn on_series_put(
        &self,
        series_title: &str,
        body: Vec<u8>,
        actor: Option<&str>,
    ) -> Result<bool, UpdateError> {
        let service = self.service.clone();
        let title = series_title.to_owned();
        let actor = actor.map(str::to_owned);
        match tokio::task::spawn_blocking(move || {
            service.apply_series_update(&title, &body, actor.as_deref())
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(UpdateError::Store(Box::new(error))),
        }
    }

    /// Run a full pass now (bypassing the timers), on the blocking pool.
    pub async fn recompile_all_now(&self) -> usize {
        let service = self.service.clone();
        match tokio::task::spawn_blocking(move || service.recompile_all()).await {
            Ok(Ok(changed)) => changed,
            Ok(Err(error)) => {
                tracing::warn!(%error, "[METADATA] regeneration failed");
                0
            }
            Err(_) => 0,
        }
    }

    /// `missing_pages_now` for the OCR scheduler (blocking).
    pub fn missing_pages_now(&self, cbz: &Path) -> i64 {
        bunko_library::compiler::missing_pages_now(self.store.as_ref(), &self.library, cbz)
            .unwrap_or(0)
    }

    /// `cached_page_count` for the OCR scheduler (blocking).
    pub fn cached_page_count(&self, cbz: &Path) -> Option<i64> {
        bunko_library::compiler::cached_page_count(self.store.as_ref(), &self.library, cbz)
            .ok()
            .flatten()
    }

    /// `cached_mokuro_sha256` (blocking): the primary sidecar's hash when the cache
    /// entry is current. `library` must be the root `cbz` is expressed under.
    pub fn cached_mokuro_sha256(&self, library: &Path, cbz: &Path) -> Option<String> {
        bunko_library::compiler::cached_mokuro_sha256(self.store.as_ref(), library, cbz)
            .ok()
            .flatten()
    }
}

/// `total_volumes` from the library index.
struct IndexCounts(Arc<LibraryIndexCache>);

impl LibraryCounts for IndexCounts {
    fn total_volumes(&self) -> Result<u64, String> {
        let snapshot = self.0.get_snapshot();
        Ok(snapshot.series.iter().map(|s| s.volumes.len() as u64).sum())
    }
}

/// The library runtime as a WebDAV listener (`ServerDavHooks::add_listener`): DAV
/// commits reach the index and the metadata scheduler without waiting for the watcher.
impl bunko_dav::DavHooks for LibraryRuntime {
    fn library_changed(&self, paths: &[PathBuf]) {
        for path in paths {
            self.on_library_write(path);
        }
    }
}
