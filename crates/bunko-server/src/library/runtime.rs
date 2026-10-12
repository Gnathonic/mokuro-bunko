//! [`LibraryRuntime`]: the metadata service with its timers, the library index, the
//! filesystem watcher and the community fetcher (0.5.2 `server.py` wiring plus the
//! timer half of `metadata/service.py`).
//!
//! Timers are tokio tasks armed by a [`Debouncer`] each (one for the full pass, one
//! per series key). A fired timer clears its slot under the timer lock BEFORE the pass
//! runs, so a later schedule arms a fresh timer instead of cancelling a running pass.
//!
//! Passes never run on the tokio blocking pool: a fired timer hands its work to ONE
//! metadata thread ([`MetadataWorker`]), which coalesces everything queued since its
//! last pass (a full pass, or one batch of changed series compiled over a single library
//! scan). A burst of changes therefore costs a few passes instead of one per series,
//! and however much changes, the request path's blocking pool is never involved.

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
use parking_lot::{Condvar, Mutex, RwLock};
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
    worker: Arc<MetadataWorker>,
}

/// What the metadata thread has been asked to do since its last pass.
#[derive(Default)]
struct WorkQueue {
    stopped: bool,
    /// A pass (or a batch) is running now.
    running: bool,
    full: bool,
    /// Series to recompile, by key: the latest raw title for each.
    series: HashMap<String, String>,
    /// When the last series batch started (batches are spaced by [`batch_gap`]).
    last_batch: Option<Instant>,
    /// The least time between two series batches.
    batch_gap: Duration,
}

/// The least time between two series batches: a fifth of the debounce (2 s by default).
/// Each batch walks the library once, so a steady trickle of changes (the generation
/// upgrade's paced swaps) costs a walk every two seconds at most, however fast it comes.
fn batch_gap(policy: &DebouncePolicy) -> Duration {
    policy.debounce / 5
}

impl WorkQueue {
    fn is_idle(&self) -> bool {
        !self.running && !self.full && self.series.is_empty()
    }
}

/// The one thread that runs metadata passes (see the module docs), started on first use.
#[derive(Default)]
struct MetadataWorker {
    queue: Mutex<WorkQueue>,
    wake: Condvar,
    /// Signalled whenever the queue goes idle.
    idle: Condvar,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Passes and batches run so far.
    passes: std::sync::atomic::AtomicU64,
}

enum Work {
    Full,
    Series(Vec<String>),
}

impl MetadataWorker {
    /// The next piece of work, waiting for it; `None` once stopped. A queued full pass
    /// covers every queued series.
    fn next_work(&self) -> Option<Work> {
        let mut queue = self.queue.lock();
        queue.running = false;
        if queue.is_idle() {
            self.idle.notify_all();
        }
        loop {
            if queue.stopped {
                return None;
            }
            if queue.full {
                queue.full = false;
                queue.series.clear();
                queue.running = true;
                return Some(Work::Full);
            }
            if !queue.series.is_empty() {
                if let Some(next) = queue.last_batch.map(|at| at + queue.batch_gap)
                    && Instant::now() < next
                {
                    self.wake.wait_until(&mut queue, next);
                    continue;
                }
                queue.last_batch = Some(Instant::now());
                let mut titles: Vec<String> = queue.series.drain().map(|(_, t)| t).collect();
                titles.sort();
                queue.running = true;
                return Some(Work::Series(titles));
            }
            self.wake.wait(&mut queue);
        }
    }

    fn stop(&self) {
        {
            let mut queue = self.queue.lock();
            queue.stopped = true;
            queue.full = false;
            queue.series.clear();
        }
        self.wake.notify_all();
    }
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
                worker: Arc::new(MetadataWorker {
                    queue: Mutex::new(WorkQueue {
                        batch_gap: batch_gap(&deps.policy),
                        ..WorkQueue::default()
                    }),
                    ..MetadataWorker::default()
                }),
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
        self.worker.stop();
        let thread = self.worker.thread.lock().take();
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
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
        self.enqueue(|queue| queue.full = true);
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
        self.enqueue(move |queue| {
            queue.series.insert(key, title);
        });
    }

    /// Queue work for the metadata thread (starting it on first use) and wake it.
    fn enqueue(&self, add: impl FnOnce(&mut WorkQueue)) {
        {
            let mut queue = self.worker.queue.lock();
            if queue.stopped {
                return;
            }
            add(&mut queue);
        }
        self.ensure_worker();
        self.worker.wake.notify_one();
    }

    fn ensure_worker(&self) {
        let mut thread = self.worker.thread.lock();
        if thread.is_some() {
            return;
        }
        let (me, worker) = (self.me.clone(), self.worker.clone());
        match std::thread::Builder::new()
            .name("metadata".into())
            .spawn(move || metadata_loop(me, worker))
        {
            Ok(handle) => *thread = Some(handle),
            Err(error) => tracing::error!(%error, "[METADATA] could not start the metadata thread"),
        }
    }

    fn run_work(&self, work: Work) {
        self.worker.passes.fetch_add(1, Ordering::SeqCst);
        match work {
            Work::Full => {
                tracing::info!("[METADATA] full pass fired");
                match self.service.recompile_all() {
                    Ok(changed) => tracing::info!("[METADATA] full pass done (changed={changed})"),
                    Err(error) => tracing::warn!(%error, "[METADATA] regeneration failed"),
                }
            }
            Work::Series(titles) => {
                let named = series_list(&titles);
                tracing::info!("[METADATA] series regen fired: {named}");
                match self.service.recompile_series_batch(&titles) {
                    Ok(changed) => {
                        tracing::info!("[METADATA] series regen done: {named} (changed={changed})")
                    }
                    Err(error) => {
                        tracing::warn!(%error, "[METADATA] series regeneration failed: {named}")
                    }
                }
            }
        }
    }

    /// How many passes (full passes and series batches) the metadata thread has run.
    pub fn passes_run(&self) -> u64 {
        self.worker.passes.load(Ordering::SeqCst)
    }

    /// Wait (blocking) until the metadata thread has nothing queued or running. For
    /// tests and shutdown paths.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut queue = self.worker.queue.lock();
        while !queue.is_idle() && !queue.stopped {
            if self
                .worker
                .idle
                .wait_until(&mut queue, deadline)
                .timed_out()
            {
                return queue.is_idle();
            }
        }
        true
    }

    /// Whether a timer (full or any series) is armed. For tests and diagnostics.
    pub fn has_pending_pass(&self) -> bool {
        let armed = {
            let timers = self.timers.lock();
            timers.full.task.is_some()
                || timers.series.values().any(|(slot, _)| slot.task.is_some())
        };
        armed || !self.worker.queue.lock().is_idle()
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

    /// Run a full pass now (bypassing the timers and the metadata thread's queue) on a
    /// thread of its own, never the blocking pool requests use.
    pub async fn recompile_all_now(&self) -> usize {
        let service = self.service.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("metadata-now".into())
            .spawn(move || {
                let _ = tx.send(service.recompile_all());
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "[METADATA] could not start a pass");
            return 0;
        }
        match rx.await {
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

/// The metadata thread: run whatever is queued until the runtime stops or goes away.
/// It waits holding only the queue, so a dropped runtime is not kept alive by it.
fn metadata_loop(me: Weak<LibraryRuntime>, worker: Arc<MetadataWorker>) {
    while let Some(work) = worker.next_work() {
        let Some(rt) = me.upgrade() else { return };
        rt.run_work(work);
    }
}

impl Drop for LibraryRuntime {
    fn drop(&mut self) {
        // Without a `stop`, the metadata thread still has to end.
        self.worker.stop();
    }
}

/// `A, B, C (+12 more)` for the log.
fn series_list(titles: &[String]) -> String {
    const SHOWN: usize = 5;
    let mut text = titles
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if titles.len() > SHOWN {
        text.push_str(&format!(" (+{} more)", titles.len() - SHOWN));
    }
    text
}

/// `total_volumes` and `pending_ocr` from the library index.
struct IndexCounts(Arc<LibraryIndexCache>);

impl LibraryCounts for IndexCounts {
    fn total_volumes(&self) -> Result<u64, String> {
        let snapshot = self.0.get_snapshot();
        Ok(snapshot.series.iter().map(|s| s.volumes.len() as u64).sum())
    }

    fn pending_ocr(&self) -> Result<u64, String> {
        Ok(self.0.get_snapshot().pending_ocr.len() as u64)
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
