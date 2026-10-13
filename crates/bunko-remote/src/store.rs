//! [`RemoteStore`]: a library on a cloud provider behind [`bunko_store::LibraryStore`]
//! (spec §3.1, §3.3, §5).
//!
//! **Threads.** The store owns a 2-worker tokio runtime (threads `bunko-store`) that
//! runs the [`Feed`] and every backend call. Metadata methods (`metadata`,
//! `read_dir`, `canonicalize`, `case_winner`, …) read `index.sqlite` only and never
//! touch the network, so they are safe from any thread. Content methods (`open`,
//! reads of an opened file, `local_path`, `content_sha256`, `wait_steady`) run their
//! network part with `Handle::block_on` on the store runtime: legal from std threads
//! and `spawn_blocking` threads, never from inside an async task (tokio panics there;
//! the store turns that into a clear message) and never from a store runtime thread
//! (a thread-local flag set at thread start; `debug_assert`). Watch sinks and the
//! [`CloudHooks`] run on a dedicated `bunko-store-events` std thread, so they may call
//! back into the shim.
//!
//! **Reads (D24).** Every byte that enters a cache comes from a read pinned to the
//! indexed version: the provider's pin token where it has one, else a re-stat after
//! the read whose `cver` must still match. A mismatch is an
//! `io::Error(Other, BErr::PreconditionFailed)`; nothing is cached and the feed is
//! asked to poll. Files up to [`SMALL_MAX`] come from the [`BlobCache`] (one pinned
//! read per version, provider hash verified); larger ones open as an
//! [`ArchiveFile`] (tail from the [`ZipDirCache`], other ranges from the spool or
//! pinned ranged reads through the [`BlockCache`]). `local_path` spools the whole
//! file once, however many callers ask at the same time.
//!
//! **Writes** go through the outbox (`outbox.rs`, spec §5.7): `open_with` hands out a
//! local staging file under `<dir>/staging/` (published only by an explicit
//! `publish` or `rename`, never on drop), `publish` stages the bytes and runs a Sync
//! op inline or queues a Deferred one, and `rename`, `remove_*` and `create_dir*`
//! are Sync ops. `state.sqlite` holds the outbox, the ledger and the lease (D7).
//! The lease ([`crate::status::Lease`]) starts after the first sync; its fence gates
//! Deferred ops and Sync writes alike.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak, mpsc};
use std::thread::ThreadId;
use std::time::{Duration, SystemTime};

use bunko_store::{
    Caps, DirEntry, File, Health, LibraryStore, LocalPath, Meta, OpenOptions, Published, ReadDir,
    RemoteEntry, RemoteMeta, Version, WatchEvent, WatchGuard, WatchSink, WriteIntent,
};
use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::runtime::{Handle, Runtime};

use crate::backend::{BErr, Backend, BackendCaps, HashKind, Pid, read_to_vec_max};
use crate::cache::{
    BlobCache, BlockCache, CacheDb, CacheStats, Hasher, SMALL_MAX, Spool, SpoolPinGuard,
    ZipDirCache,
};
use crate::feed::{
    CloudHooks, EngineState, Feed, FeedConfig, FeedHandle, FeedState, SubmissionResult,
};
use crate::file::{ArchiveFile, CachedFile};
use crate::index::{Entry, Index, RelPath};
use crate::outbox::{Failpoint, Gate, Op, OpSummary};
use crate::state::StateDb;
use crate::status::{
    DEFAULT_TTL, DoneEntry, Fence, Lease, LeaseConfig, LeaseEvent, LeaseHandle, LeaseRecord,
    LeaseStore, MEGA_TTL, SqliteLeaseStore,
};

thread_local! {
    /// Set on every thread of a store runtime (workers and its blocking pool).
    static ON_STORE_RUNTIME: Cell<bool> = const { Cell::new(false) };
}

/// Whether the current thread belongs to a RemoteStore runtime.
pub fn on_store_runtime() -> bool {
    ON_STORE_RUNTIME.with(Cell::get)
}

/// The message for a content method called from inside an async task.
const IN_ASYNC_TASK: &str = "called from inside an async task: RemoteStore content methods block on \
     the network; call them from a std thread or tokio::task::spawn_blocking";

/// Settings of one RemoteStore.
#[derive(Debug, Clone)]
pub struct RemoteStoreConfig {
    /// The synthetic library root (`<base>/cloud/<store-id>/library`); never on disk.
    pub root: PathBuf,
    /// The store's own directory (`<base>/cloud/<store-id>`): `index.sqlite`,
    /// `blobs/`, `spool/`.
    pub dir: PathBuf,
    /// The cloud root folder name (`mokuro-reader`).
    pub folder: String,
    /// Create the root folder when the provider has none.
    pub create_root: bool,
    /// The small-file cache budget (`cache_mb`, §5.5), bytes.
    pub cache_bytes: u64,
    /// The archive spool budget (`spool_gb`, §5.6), bytes.
    pub spool_bytes: u64,
    /// How long `local_path` waits for spool budget before `StorageFull`.
    pub spool_wait: Duration,
    /// The in-memory 256 KiB block cache budget, bytes.
    pub block_cache_bytes: u64,
    /// `quiet_s` (§5.4).
    pub quiet: Duration,
    pub poll_active: Duration,
    pub poll_idle: Duration,
    /// `Meta::dev` of every entry: two paths are the same file iff `(dev, ino)` match.
    pub mount_id: u64,
    /// D19: fewer removed files than this never make the feed suspect.
    pub suspect_min_removals: u64,
    /// Run the marker lease (§9.2) after the first sync. Off: Deferred ops run
    /// whenever the gate of [`RemoteStore::set_lease_gate`] (if any) allows.
    pub lease: bool,
    /// `instance_name` of the marker ("Another Bunko (nas) manages this library").
    pub instance_name: String,
    /// `bunko_version` of the marker.
    pub bunko_version: String,
    /// `ttl_s` of the marker; `None`: 900 s, 3600 s on MEGA-like providers.
    pub lease_ttl: Option<Duration>,
    /// The first retry delay of an outbox op (doubles per attempt, capped at 5 min).
    pub retry_base: Duration,
    /// The shortest interval between two cloud queue-file writes; `None`: per
    /// provider (WebDAV 30 s, Drive and OneDrive 60 s, MEGA 300 s).
    pub queue_floor: Option<Duration>,
}

impl RemoteStoreConfig {
    /// The defaults: 1 GiB small-file cache, 4 GiB spool (10 min admission wait),
    /// 64 MiB block cache, the feed's WebDAV cadence and quiet period.
    pub fn new(
        root: impl Into<PathBuf>,
        dir: impl Into<PathBuf>,
        folder: impl Into<String>,
    ) -> RemoteStoreConfig {
        let root = root.into();
        let folder = folder.into();
        let feed = FeedConfig::new(root.clone(), folder.clone());
        RemoteStoreConfig {
            root,
            dir: dir.into(),
            folder,
            create_root: false,
            cache_bytes: 1024 << 20,
            spool_bytes: 4 << 30,
            spool_wait: Duration::from_secs(600),
            block_cache_bytes: 64 << 20,
            quiet: feed.quiet,
            poll_active: feed.poll_active,
            poll_idle: feed.poll_idle,
            mount_id: 0,
            suspect_min_removals: feed.suspect_min_removals,
            lease: true,
            instance_name: "bunko".into(),
            bunko_version: env!("CARGO_PKG_VERSION").into(),
            lease_ttl: None,
            retry_base: Duration::from_secs(1),
            queue_floor: None,
        }
    }

    /// The feed engine's settings.
    pub fn feed_config(&self) -> FeedConfig {
        let mut f = FeedConfig::new(self.root.clone(), self.folder.clone());
        f.create_if_missing = self.create_root;
        f.poll_active = self.poll_active;
        f.poll_idle = self.poll_idle;
        f.quiet = self.quiet;
        f.suspect_min_removals = self.suspect_min_removals;
        f
    }
}

/// A backend error as the `io::Error` the shim's callers see: `NotFound` keeps its
/// kind, `StorageFull` too; everything else is `Other`, wrapping the [`BErr`].
pub fn berr_to_io(e: BErr) -> io::Error {
    match e {
        BErr::NotFound => io::Error::new(io::ErrorKind::NotFound, e),
        BErr::StorageFull => io::Error::new(io::ErrorKind::StorageFull, e),
        e => io::Error::other(e),
    }
}

fn not_found(p: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{}: no such file or directory in the remote library",
            p.display()
        ),
    )
}

/// The indexed version of one file, everything a pinned read needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ver {
    pub pid: Pid,
    pub cver: String,
    pub pin: Option<String>,
    pub size: u64,
    pub hash: Option<(HashKind, String)>,
}

impl Ver {
    pub(crate) fn key(&self) -> (String, String) {
        (self.pid.clone(), self.cver.clone())
    }
}

// ---- the store-events thread --------------------------------------------------------

type Job = Box<dyn FnOnce() + Send>;

/// One std thread that runs watch sinks and cloud hooks in order.
struct Events {
    tx: Mutex<Option<mpsc::Sender<Job>>>,
    thread: ThreadId,
}

impl Events {
    fn start() -> io::Result<Arc<Events>> {
        let (tx, rx) = mpsc::channel::<Job>();
        let t = std::thread::Builder::new()
            .name("bunko-store-events".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    if catch_unwind(AssertUnwindSafe(job)).is_err() {
                        tracing::error!("a store event handler panicked");
                    }
                }
            })?;
        Ok(Arc::new(Events {
            tx: Mutex::new(Some(tx)),
            thread: t.thread().id(),
        }))
    }

    fn post(&self, job: Job) {
        if let Some(tx) = &*self.tx.lock() {
            let _ = tx.send(job);
        }
    }

    /// Run `f` on the events thread and wait for its answer (`None`: stopped).
    fn call<R: Send + 'static>(&self, f: impl FnOnce() -> R + Send + 'static) -> Option<R> {
        if std::thread::current().id() == self.thread {
            return Some(f());
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.post(Box::new(move || {
            let _ = tx.send(f());
        }));
        // On a store runtime thread, hand the worker back while waiting; anywhere
        // else (a std thread, possibly inside `block_on`) just wait.
        if on_store_runtime() {
            tokio::task::block_in_place(|| rx.recv().ok())
        } else {
            rx.recv().ok()
        }
    }

    fn close(&self) {
        self.tx.lock().take();
    }
}

/// The store's watch sinks.
#[derive(Default)]
struct SinkList {
    next: AtomicU64,
    list: Mutex<Vec<(u64, WatchSink)>>,
}

impl SinkList {
    fn emit(&self, e: &WatchEvent) {
        let list = self.list.lock().clone();
        for (_, s) in &list {
            s(e.clone());
        }
    }
}

struct SinkReg {
    sinks: Weak<SinkList>,
    id: u64,
}

impl Drop for SinkReg {
    fn drop(&mut self) {
        if let Some(s) = self.sinks.upgrade() {
            s.list.lock().retain(|(i, _)| *i != self.id);
        }
    }
}

// ---- cloud hooks, installed late ----------------------------------------------------

enum Pending {
    Moved(PathBuf, PathBuf, bool),
    Removed(PathBuf, bool),
    Sidecar(PathBuf),
    Discarded(PathBuf),
}

impl Pending {
    fn run(self, h: &dyn CloudHooks) {
        match self {
            Pending::Moved(a, b, d) => h.moved(&a, &b, d),
            Pending::Removed(a, d) => h.removed(&a, d),
            Pending::Sidecar(a) => h.sidecar_overwritten(&a),
            Pending::Discarded(a) => h.ocr_discarded(&a),
        }
    }
}

#[derive(Default)]
struct SlotState {
    hooks: Option<Arc<dyn CloudHooks>>,
    pending: Vec<Pending>,
}

/// The feed's [`CloudHooks`]: forwards to the server's hooks on the events thread.
/// Notifications that arrive before [`RemoteStore::set_hooks`] are kept and replayed
/// in order when the hooks are installed.
struct HookSlot {
    events: Arc<Events>,
    state: Mutex<SlotState>,
}

impl HookSlot {
    fn dispatch(&self, p: Pending) {
        let mut g = self.state.lock();
        match g.hooks.clone() {
            Some(h) => {
                drop(g);
                self.events.call(move || p.run(h.as_ref()));
            }
            None => g.pending.push(p),
        }
    }

    fn set(&self, h: Arc<dyn CloudHooks>) {
        let mut g = self.state.lock();
        g.hooks = Some(h.clone());
        let pending = std::mem::take(&mut g.pending);
        if !pending.is_empty() {
            // Under the lock, so a concurrent notification cannot overtake these.
            self.events.call(move || {
                for p in pending {
                    p.run(h.as_ref());
                }
            });
        }
    }

    fn hooks(&self) -> Option<Arc<dyn CloudHooks>> {
        self.state.lock().hooks.clone()
    }
}

impl CloudHooks for HookSlot {
    fn moved(&self, from: &Path, to: &Path, is_dir: bool) {
        self.dispatch(Pending::Moved(from.into(), to.into(), is_dir));
    }

    fn removed(&self, path: &Path, is_dir: bool) {
        self.dispatch(Pending::Removed(path.into(), is_dir));
    }

    fn sidecar_overwritten(&self, path: &Path) {
        self.dispatch(Pending::Sidecar(path.into()));
    }

    fn series_submission(&self, series_dir: &Path, bytes: &[u8]) -> SubmissionResult {
        let Some(h) = self.hooks() else {
            return SubmissionResult::Refused("the cloud hooks are not installed yet".into());
        };
        let (dir, bytes) = (series_dir.to_path_buf(), bytes.to_vec());
        self.events
            .call(move || h.series_submission(&dir, &bytes))
            .unwrap_or_else(|| SubmissionResult::Refused("the store is shutting down".into()))
    }

    fn ocr_discarded(&self, path: &Path) {
        self.dispatch(Pending::Discarded(path.into()));
    }

    fn has_series_facts(&self, series_dir: &Path) -> bool {
        let Some(h) = self.hooks() else {
            return true;
        };
        let dir = series_dir.to_path_buf();
        self.events
            .call(move || h.has_series_facts(&dir))
            .unwrap_or(true)
    }
}

// ---- the shared core ----------------------------------------------------------------

/// What the store, its open files and its download tasks share. Holds no runtime, so a
/// task that owns the last reference never drops the runtime it runs on.
pub(crate) struct Core<B: Backend> {
    pub backend: Arc<B>,
    pub caps: BackendCaps,
    pub index: Arc<Mutex<Index>>,
    pub blobs: BlobCache,
    pub zipdir: ZipDirCache,
    pub blocks: BlockCache,
    pub spool: Arc<Spool>,
    pub feed: FeedHandle,
    /// Striped locks so one small file version is fetched once at a time.
    fills: Vec<Mutex<()>>,
    // ---- the write side (outbox.rs) ----
    pub state: Arc<StateDb>,
    /// The store's own directory.
    pub dir: PathBuf,
    /// The synthetic library root.
    pub root: PathBuf,
    pub ops: Arc<Mutex<BTreeMap<u64, Op>>>,
    pub seq: AtomicU64,
    /// One op runs at a time: the worker's, or a Sync caller's (the barrier).
    pub exec: tokio::sync::Mutex<()>,
    pub wake: tokio::sync::Notify,
    /// `(pid, cver)` echoes the feed delivered (COMMITTED → CONFIRMED).
    pub echoed: Arc<Mutex<HashSet<(String, String)>>>,
    pub gate: Mutex<Option<Gate>>,
    pub fence: OnceLock<Fence>,
    pub lease_on: bool,
    pub failpoint: Mutex<Option<Failpoint>>,
    pub crashed: AtomicBool,
    pub hooks: Arc<dyn CloudHooks>,
    pub emit: Arc<dyn Fn(WatchEvent) + Send + Sync>,
    pub retry_base: Duration,
    /// `done[]` of the cloud queue file.
    pub done: Mutex<Vec<DoneEntry>>,
    pub queue: Mutex<QueueState>,
    pub instance_id: String,
    pub queue_floor: Duration,
    /// Staging files by library path (never published on drop).
    pub staging: Mutex<HashMap<RelPath, PathBuf>>,
}

/// The cloud queue file writer's state.
#[derive(Default)]
pub(crate) struct QueueState {
    pub last: Option<serde_json::Value>,
    pub last_write: Option<std::time::Instant>,
    pub pending: Option<serde_json::Value>,
    pub timer: bool,
}

/// The ledger's answers to the feed (D20).
struct LedgerFeed {
    state: Arc<StateDb>,
    echoed: Arc<Mutex<HashSet<(String, String)>>>,
    ops: Arc<Mutex<BTreeMap<u64, Op>>>,
}

impl FeedState for LedgerFeed {
    fn keep_on_sweep(&self, pid: &str) -> bool {
        let live = self.ops.lock().values().any(|o| {
            o.state.live()
                && (o.target_pid.as_deref() == Some(pid) || o.result_pid.as_deref() == Some(pid))
        });
        live || self.state.ledger_recent(pid, Duration::from_secs(3600))
    }

    fn is_own_echo(&self, pid: &str, parent_pid: Option<&str>, name: &str, cver: &str) -> bool {
        let echo = self.state.is_echo(pid, parent_pid, name, cver);
        if echo {
            self.echoed
                .lock()
                .insert((pid.to_string(), cver.to_string()));
        }
        echo
    }

    fn is_own_delete(&self, pid: &str, cver: &str) -> bool {
        self.state.is_own_delete(pid, cver)
    }
}

/// The lease's state in `state.sqlite`, with marker writes ledgered so their echoes
/// are own echoes.
struct StateLease {
    inner: SqliteLeaseStore,
    state: Arc<StateDb>,
}

impl LeaseStore for StateLease {
    fn load(&self) -> Option<LeaseRecord> {
        self.inner.load()
    }

    fn save(&self, rec: &LeaseRecord) -> Result<(), crate::status::LeaseError> {
        self.inner.save(rec)
    }

    fn marker_written(&self, node: &crate::backend::RNode) {
        let _ = self.state.ledger_put(&crate::state::LedgerRow {
            pid: node.pid.clone(),
            cver: node.cver.clone(),
            parent_pid: node.parent.clone(),
            name: crate::names::nfc(&node.name_raw),
            sha256: None,
            op_id: Some("lease".into()),
            trashed: false,
            echo_only: false,
        });
    }

    fn marker_deleted(&self, pid: &str) {
        let _ = self.state.ledger_trash(pid);
    }
}

impl<B: Backend> Core<B> {
    /// One read of `v` pinned to the indexed version (D24): `range` is inclusive.
    pub(crate) async fn read_pinned(
        &self,
        v: &Ver,
        range: Option<(u64, u64)>,
    ) -> Result<Vec<u8>, BErr> {
        let pinned = self.caps.pinned_read && v.pin.is_some();
        let pin = if pinned { v.pin.as_deref() } else { None };
        // The bound comes first: never ask for, or buffer, more than the indexed
        // version holds.
        let want = match range {
            None => v.size,
            Some((a, b)) if a <= b && b < v.size => b - a + 1,
            Some((a, b)) => {
                return Err(BErr::Fatal(format!(
                    "range {a}-{b} is outside the indexed {} bytes",
                    v.size
                )));
            }
        };
        let r = async {
            let body = self
                .backend
                .read(&v.pid, pin, range.map(|(a, b)| (a, Some(b))))
                .await?;
            let data = read_to_vec_max(body, want)
                .await
                .map_err(|e| BErr::Transient(format!("read body: {e}")));
            // Unpinned: the re-stat decides first (another version is a likelier
            // cause of a wrong length than a broken transfer).
            if !pinned {
                self.check_version(v).await?;
            }
            let data = data?;
            if data.len() as u64 != want {
                return Err(BErr::Transient(format!(
                    "read returned {} of {want} bytes",
                    data.len()
                )));
            }
            Ok(data)
        }
        .await;
        if r.as_ref().is_err_and(|e| *e == BErr::PreconditionFailed) {
            self.feed.poll_now();
        }
        r
    }

    /// The pin of a read the provider could not pin: `v` must still be current.
    pub(crate) async fn check_version(&self, v: &Ver) -> Result<(), BErr> {
        match self.backend.stat(&v.pid).await? {
            Some(n) if !n.trashed && n.cver == v.cver => Ok(()),
            _ => Err(BErr::PreconditionFailed),
        }
    }

    fn fill_lock(&self, v: &Ver) -> parking_lot::MutexGuard<'_, ()> {
        let h = crate::names::version_salt(&v.pid, &v.cver) as usize;
        self.fills[h % self.fills.len()].lock()
    }

    /// Download `entry` (a new spool file for `v`) once, for every reader.
    async fn download(self: Arc<Self>, entry: SpoolPinGuard, v: Ver) {
        let e = entry.entry.clone();
        let mut r = self.download_inner(&e, &v).await;
        let pinned = self.caps.pinned_read && v.pin.is_some();
        if !pinned && matches!(r, Err(BErr::Transient(_))) && self.check_version(&v).await.is_err()
        {
            r = Err(BErr::PreconditionFailed);
        }
        match r {
            Ok(sha) => {
                let _ = e.sha256.set(sha);
                self.spool.complete(&e);
            }
            Err(err) => {
                tracing::warn!(pid = %v.pid, "spool download failed: {err}");
                if err == BErr::PreconditionFailed {
                    self.feed.poll_now();
                }
                self.spool.fail(&e, err);
            }
        }
        drop(entry);
    }

    async fn download_inner(
        &self,
        e: &crate::cache::SpoolEntry,
        v: &Ver,
    ) -> Result<[u8; 32], BErr> {
        use std::io::Write;
        let local =
            |err: io::Error| BErr::Transient(format!("spool file {}: {err}", e.path.display()));
        let mut f = std::fs::File::create(&e.path).map_err(local)?;
        let pinned = self.caps.pinned_read && v.pin.is_some();
        let pin = if pinned { v.pin.as_deref() } else { None };
        let mut body = self.backend.read(&v.pid, pin, None).await?;
        let mut h = Hasher::new(v.hash.as_ref().map(|x| x.0));
        let mut n: u64 = 0;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|err| BErr::Transient(format!("read body: {err}")))?;
            n += chunk.len() as u64;
            if n > v.size {
                return Err(BErr::Transient(format!(
                    "the download is longer than the indexed {} bytes",
                    v.size
                )));
            }
            f.write_all(&chunk).map_err(local)?;
            h.update(&chunk);
            // An unpinned read's bytes are not the indexed version until the re-stat
            // below: readers see them only once it passes.
            if pinned {
                e.set_filled(n);
            }
        }
        if n != v.size {
            return Err(BErr::Transient(format!(
                "the download ended at {n} of {} bytes",
                v.size
            )));
        }
        let sha = h.finish(v.hash.as_ref())?;
        if !pinned {
            self.check_version(v).await?;
        }
        Ok(sha)
    }
}

// ---- the store ----------------------------------------------------------------------

/// A library on a cloud provider (spec §5). Open it with [`RemoteStore::open`], mount
/// it with `bunko_store::mount`.
pub struct RemoteStore<B: Backend> {
    me: Weak<RemoteStore<B>>,
    pub(crate) cfg: RemoteStoreConfig,
    pub(crate) core: Arc<Core<B>>,
    rt: Option<Runtime>,
    handle: Handle,
    events: Arc<Events>,
    sinks: Arc<SinkList>,
    hooks: Arc<HookSlot>,
    lease: Arc<OnceLock<LeaseHandle>>,
}

impl<B: Backend> RemoteStore<B> {
    /// Create `cfg.dir`, open its `index.sqlite` and `state.sqlite`, replay the
    /// outbox (§5.10), start the store runtime, the feed, the outbox worker and (after
    /// the first sync) the lease.
    pub fn open(cfg: RemoteStoreConfig, backend: B) -> io::Result<Arc<RemoteStore<B>>> {
        Self::open_shared(cfg, Arc::new(backend))
    }

    /// [`RemoteStore::open`] over a backend the caller keeps a handle to.
    pub fn open_shared(cfg: RemoteStoreConfig, backend: Arc<B>) -> io::Result<Arc<RemoteStore<B>>> {
        std::fs::create_dir_all(&cfg.dir)?;
        for d in ["outbox", "staging", "submissions", "lease"] {
            std::fs::create_dir_all(cfg.dir.join(d))?;
        }
        let state = Arc::new(StateDb::open(&cfg.dir.join("state.sqlite"))?);
        let instance_id = state.instance_id()?;
        let ops: Arc<Mutex<BTreeMap<u64, Op>>> = Arc::default();
        let echoed: Arc<Mutex<HashSet<(String, String)>>> = Arc::default();
        let index_path = cfg.dir.join("index.sqlite");
        let index = Index::open(&index_path).map_err(io::Error::other)?;
        let index = Arc::new(Mutex::new(index));
        let db = Arc::new(CacheDb::open(&index_path)?);
        let blobs = BlobCache::open(cfg.dir.join("blobs"), db.clone(), cfg.cache_bytes)?;
        let spool = Arc::new(Spool::open(
            cfg.dir.join("spool"),
            db.clone(),
            cfg.spool_bytes,
            cfg.spool_wait,
        )?);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("bunko-store")
            .on_thread_start(|| ON_STORE_RUNTIME.with(|f| f.set(true)))
            .enable_all()
            .build()?;
        let events = Events::start()?;
        let sinks = Arc::new(SinkList::default());
        let hooks = Arc::new(HookSlot {
            events: events.clone(),
            state: Mutex::new(SlotState::default()),
        });
        let caps = backend.caps();
        let feed = Feed::new(backend.clone(), index.clone(), cfg.feed_config())
            .with_hooks(hooks.clone() as Arc<dyn CloudHooks>)
            .with_state(Arc::new(LedgerFeed {
                state: state.clone(),
                echoed: echoed.clone(),
                ops: ops.clone(),
            }));
        {
            let (events, sinks) = (Arc::downgrade(&events), Arc::downgrade(&sinks));
            feed.add_sink(Arc::new(move |ev: WatchEvent| {
                if let (Some(events), Some(sinks)) = (events.upgrade(), sinks.upgrade()) {
                    events.post(Box::new(move || sinks.emit(&ev)));
                }
            }));
        }
        let emit: Arc<dyn Fn(WatchEvent) + Send + Sync> = {
            let (events, sinks) = (Arc::downgrade(&events), Arc::downgrade(&sinks));
            Arc::new(move |ev: WatchEvent| {
                if let (Some(events), Some(sinks)) = (events.upgrade(), sinks.upgrade()) {
                    events.post(Box::new(move || sinks.emit(&ev)));
                }
            })
        };
        let queue_floor = cfg.queue_floor.unwrap_or(if caps.path_ids {
            Duration::from_secs(30)
        } else if caps.push && !caps.atomic_replace {
            Duration::from_secs(300)
        } else {
            Duration::from_secs(60)
        });
        let handle = rt.handle().clone();
        let feed = feed.spawn(&handle);
        let core = Arc::new(Core {
            backend,
            caps,
            index,
            blobs,
            zipdir: ZipDirCache::new(db.clone()),
            blocks: BlockCache::new(cfg.block_cache_bytes),
            spool,
            feed,
            fills: (0..64).map(|_| Mutex::new(())).collect(),
            state,
            dir: cfg.dir.clone(),
            root: cfg.root.clone(),
            ops,
            seq: AtomicU64::new(0),
            exec: tokio::sync::Mutex::new(()),
            wake: tokio::sync::Notify::new(),
            echoed,
            gate: Mutex::new(None),
            fence: OnceLock::new(),
            lease_on: cfg.lease,
            failpoint: Mutex::new(None),
            crashed: AtomicBool::new(false),
            hooks: hooks.clone() as Arc<dyn CloudHooks>,
            emit,
            retry_base: cfg.retry_base,
            done: Mutex::new(Vec::new()),
            queue: Mutex::new(QueueState::default()),
            instance_id,
            queue_floor,
            staging: Mutex::new(HashMap::new()),
        });
        core.seq.store(core.max_seq(), Ordering::Release);
        core.replay();
        crate::write::load_staging(&core);
        handle.spawn(core.clone().worker());
        let lease: Arc<OnceLock<LeaseHandle>> = Arc::default();
        if cfg.lease {
            handle.spawn(start_lease(core.clone(), cfg.clone(), lease.clone()));
        }
        Ok(Arc::new_cyclic(|me| RemoteStore {
            me: me.clone(),
            cfg,
            core,
            rt: Some(rt),
            handle,
            events,
            sinks,
            hooks,
            lease,
        }))
    }

    /// The marker lease, once it started (after the first sync).
    pub fn lease(&self) -> Option<&LeaseHandle> {
        self.lease.get()
    }

    /// Gate Deferred ops on `gate` in addition to the lease fence: while it answers
    /// `false`, Deferred ops stay HELD (startup replay included). Sync writes follow
    /// the lease fence only, and fail with `storage-lease` while it is shut.
    pub fn set_lease_gate(&self, gate: Gate) {
        *self.core.gate.lock() = Some(gate);
        self.core.wake.notify_one();
    }

    /// Test-only crash injection (see [`Failpoint`]).
    #[cfg(feature = "testing")]
    pub fn set_failpoint(&self, f: Option<Failpoint>) {
        *self.core.failpoint.lock() = f;
    }

    /// The store stopped at a failpoint.
    pub fn crashed(&self) -> bool {
        self.core.crashed.load(Ordering::Acquire)
    }

    /// Every op in the outbox, by `seq`.
    pub fn outbox(&self) -> Vec<OpSummary> {
        self.core.ops.lock().values().map(OpSummary::from).collect()
    }

    /// Wait (blocking) until no op is waiting to commit; `false` on timeout.
    pub fn wait_outbox_idle(&self, timeout: Duration) -> bool {
        let core = self.core.clone();
        self.block_on("wait_outbox_idle", async move {
            let end = tokio::time::Instant::now() + timeout;
            loop {
                let busy = core.ops.lock().values().any(|o| {
                    o.state.live()
                        || o.reconcile_only
                        || o.state == crate::outbox::OpState::SupersedeOld
                });
                if !busy {
                    return true;
                }
                if tokio::time::Instant::now() >= end {
                    return false;
                }
                core.wake.notify_one();
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    }

    /// Unresolved conflict rows `(kind, path, detail)`.
    pub fn conflicts(&self) -> Vec<(String, String, String)> {
        self.core.state.conflicts()
    }

    /// Submission rows `(path, cver, state)`.
    pub fn submissions(&self) -> Vec<(String, String, String)> {
        self.core.state.submissions()
    }

    /// Disconnect step 1, "Export to a folder" (§7): copy the bytes of every op that
    /// still owes its write (HELD and FAILED ones included) to `<to>/<library path>`.
    /// A name already taken in `to` gets ` (<op id>)` before its extension. Moves and
    /// trashes carry no bytes and are skipped. Returns the files written.
    pub fn export_outbox(&self, to: &Path) -> io::Result<Vec<PathBuf>> {
        let ops: Vec<Op> = self
            .core
            .ops
            .lock()
            .values()
            .filter(|o| {
                o.state.live()
                    || matches!(
                        o.state,
                        crate::outbox::OpState::Failed | crate::outbox::OpState::Prepared
                    )
            })
            .cloned()
            .collect();
        let mut written = Vec::new();
        for op in ops {
            let Some(src) = self.core.staged_path(&op) else {
                continue;
            };
            if !src.is_file() {
                continue;
            }
            let mut dest = to.join(op.dest.to_string());
            if dest.exists() {
                let stem = dest
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let ext = dest
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                dest.set_file_name(format!("{stem} ({}){ext}", op.id));
            }
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&src, &dest)?;
            written.push(dest);
        }
        Ok(written)
    }

    /// Disconnect step 2 (§7): write `state: "released"` into this install's marker
    /// and stop heartbeating. `Ok(false)` when no lease runs (off, or the first sync
    /// never finished).
    pub fn release_lease(&self, timeout: Duration) -> io::Result<bool> {
        let Some(lease) = self.lease.get() else {
            return Ok(false);
        };
        self.block_on("release_lease", async {
            tokio::time::timeout(timeout, lease.release()).await
        })
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "releasing the lease timed out"))?
        .map_err(|e| io::Error::other(e.to_string()))?;
        Ok(true)
    }

    /// The admin's [Take over] (§9.2): bump the epoch and hold the lease. Fails when no
    /// lease runs yet (it starts after the first sync).
    pub fn take_over_lease(&self, timeout: Duration) -> io::Result<()> {
        let Some(lease) = self.lease.get() else {
            return Err(io::Error::other(
                "the lease has not started yet (it starts after the first sync)",
            ));
        };
        self.block_on("take_over_lease", async {
            tokio::time::timeout(timeout, lease.take_over()).await
        })
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "taking over timed out"))?
        .map(|_| ())
        .map_err(|e| io::Error::other(e.to_string()))
    }

    /// The provider's storage quota, asked once (`None`: the provider reports none).
    pub fn quota(&self, timeout: Duration) -> io::Result<Option<bunko_store::Capacity>> {
        let backend = self.core.backend.clone();
        self.block_on("quota", async move {
            tokio::time::timeout(timeout, backend.quota()).await
        })
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the quota request timed out"))?
        .map_err(berr_to_io)
    }

    /// This install's id (`state.sqlite`).
    pub fn instance_id(&self) -> &str {
        &self.core.instance_id
    }

    /// The server's cloud hooks (D20); notifications from before are replayed now.
    pub fn set_hooks(&self, hooks: Arc<dyn CloudHooks>) {
        self.hooks.set(hooks);
    }

    /// The change feed.
    pub fn feed(&self) -> &FeedHandle {
        &self.core.feed
    }

    pub fn engine_state(&self) -> EngineState {
        self.core.feed.state()
    }

    /// Wait (blocking: a content-kind call) until the feed is steady; `false` on
    /// timeout.
    pub fn wait_steady(&self, timeout: Duration) -> bool {
        self.block_on("wait_steady", self.core.feed.wait_steady(timeout))
            .is_ok()
    }

    pub fn config(&self) -> &RemoteStoreConfig {
        &self.cfg
    }

    /// The provider driver.
    pub fn backend(&self) -> &Arc<B> {
        &self.core.backend
    }

    /// Debug builds: fail a content method called from inside an async task at once,
    /// even when its answer is cached and it would not block this time.
    fn guard(&self, what: &'static str) {
        if cfg!(debug_assertions) {
            self.block_on(what, async {});
        }
    }

    /// What the caches hold.
    pub fn cache_stats(&self) -> CacheStats {
        let (blobs, blob_bytes) = self.core.blobs.stats();
        let (blocks, block_bytes) = self.core.blocks.stats();
        let (spools, spool_bytes) = self.core.spool.stats();
        CacheStats {
            blobs,
            blob_bytes,
            zipdirs: self.core.zipdir.count(),
            blocks,
            block_bytes,
            spools,
            spool_bytes,
        }
    }

    /// Run the network part of a content method on the store runtime.
    pub(crate) fn block_on<F: Future>(&self, what: &'static str, f: F) -> F::Output {
        debug_assert!(
            !on_store_runtime(),
            "RemoteStore::{what} called on a store runtime thread (re-entry)"
        );
        let h = &self.handle;
        match catch_unwind(AssertUnwindSafe(move || h.block_on(f))) {
            Ok(v) => v,
            Err(p) => {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                if msg.contains("runtime from within a runtime") {
                    panic!("RemoteStore::{what} {IN_ASYNC_TASK}");
                }
                resume_unwind(p)
            }
        }
    }

    pub(crate) fn handle(&self) -> &Handle {
        &self.handle
    }

    pub(crate) fn arc(&self) -> Arc<RemoteStore<B>> {
        self.me
            .upgrade()
            .expect("a RemoteStore method runs while the store lives")
    }

    /// `p` as a library path: under the root, normal UTF-8 segments only.
    pub(crate) fn rel(&self, p: &Path) -> io::Result<RelPath> {
        let rest = p.strip_prefix(&self.cfg.root).map_err(|_| not_found(p))?;
        let mut rel = RelPath::root();
        for c in rest.components() {
            match c {
                Component::Normal(s) => {
                    let s = s.to_str().ok_or_else(|| not_found(p))?;
                    rel.push_checked(s).map_err(|_| not_found(p))?;
                }
                Component::CurDir => {}
                _ => return Err(not_found(p)),
            }
        }
        Ok(rel)
    }

    pub(crate) fn lookup(&self, p: &Path) -> io::Result<Entry> {
        let rel = self.rel(p)?;
        self.core
            .index
            .lock()
            .lookup(&rel)
            .ok_or_else(|| not_found(p))
    }

    pub(crate) fn meta(&self, e: &Entry) -> Meta {
        // A pending Deferred write shows its staged bytes (§5.2 special cases).
        if let Some(op) = e.pending_op.as_deref().and_then(|id| self.pending(id)) {
            let sha = op.sha256.map(hex::encode).unwrap_or_default();
            return Meta::Remote(RemoteMeta {
                size: op.size,
                mtime_ns: crate::names::effective_mtime_ns(
                    op.created_at,
                    self.core.caps.mtime_granularity_ns,
                    &op.op_id(),
                    &sha,
                ),
                ctime_s: op.created_at.div_euclid(1000),
                is_dir: false,
                dev: self.cfg.mount_id,
                ino: e.eid as u64,
                cver: Some(Version::new(format!("pending:{}", op.id))),
                own: true,
            });
        }
        let own = match (&e.winner_pid, &e.cver) {
            (Some(p), Some(c)) => self.core.state.is_own(p, c),
            _ => false,
        };
        Meta::Remote(RemoteMeta {
            size: e.size,
            mtime_ns: e.mtime_ns,
            ctime_s: e.created_ms.div_euclid(1000),
            is_dir: e.is_dir,
            dev: self.cfg.mount_id,
            ino: e.eid as u64,
            cver: e.cver.clone().map(Version::new),
            own,
        })
    }

    /// The live Deferred op behind a pending marker.
    pub(crate) fn pending(&self, op_id: &str) -> Option<Op> {
        let id: u64 = op_id.parse().ok()?;
        self.core
            .ops
            .lock()
            .get(&id)
            .filter(|o| o.state.live())
            .cloned()
    }

    /// The file at `p` and its indexed version.
    fn file_ver(&self, p: &Path) -> io::Result<(Entry, Ver)> {
        let rel = self.rel(p)?;
        let idx = self.core.index.lock();
        let e = idx.lookup(&rel).ok_or_else(|| not_found(p))?;
        if e.is_dir {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{} is a directory", p.display()),
            ));
        }
        let Some(pid) = e.winner_pid.clone() else {
            return Err(not_found(p));
        };
        let n = idx.node(&pid).ok_or_else(|| not_found(p))?;
        drop(idx);
        let v = Ver {
            pid,
            cver: n.cver,
            pin: n.pin,
            size: n.size,
            hash: n.hash,
        };
        Ok((e, v))
    }

    /// The bytes of a small file version and their sha256: the blob cache, else one
    /// pinned read (provider hash verified) that fills it.
    pub(crate) fn small_bytes(&self, v: &Ver) -> io::Result<(Vec<u8>, [u8; 32])> {
        let cached = |s: &Self| {
            let sha = s.core.blobs.lookup(&v.pid, &v.cver)?;
            s.core.blobs.read(&sha).map(|d| (d, sha))
        };
        if let Some(hit) = cached(self) {
            return Ok(hit);
        }
        let _fill = self.core.fill_lock(v);
        if let Some(hit) = cached(self) {
            return Ok(hit);
        }
        let data = self
            .block_on("open", self.core.read_pinned(v, None))
            .map_err(berr_to_io)?;
        let mut h = Hasher::new(v.hash.as_ref().map(|x| x.0));
        h.update(&data);
        let sha = h.finish(v.hash.as_ref()).map_err(berr_to_io)?;
        if let Err(e) = self.core.blobs.insert(&v.pid, &v.cver, &data, sha) {
            tracing::warn!("blob cache insert: {e}");
        }
        Ok((data, sha))
    }

    /// The spool entry of `v`, pinned, starting its download when new. `wait`: block
    /// for budget (else `None` when there is no room).
    pub(crate) fn spool_for(&self, v: &Ver, wait: bool) -> io::Result<Option<SpoolPinGuard>> {
        let key = v.key();
        let core = self.core.clone();
        self.block_on("spool", async move {
            let Some((pin, new)) = core.spool.admit(&key, v.size, wait).await? else {
                return Ok(None);
            };
            if new {
                let task_pin = SpoolPinGuard::clone_pin(&pin);
                tokio::spawn(core.clone().download(task_pin, v.clone()));
            }
            Ok(Some(pin))
        })
    }
}

/// Start the marker lease once the first sync is done (§9.2), and wake the outbox
/// worker on every lease event.
async fn start_lease<B: Backend>(
    core: Arc<Core<B>>,
    cfg: RemoteStoreConfig,
    slot: Arc<OnceLock<LeaseHandle>>,
) {
    let mut rx = core.feed.subscribe();
    if rx.wait_for(|s| *s == EngineState::Steady).await.is_err() {
        return;
    }
    let root = loop {
        let r = core.index.lock().roots().first().cloned();
        match r {
            Some(r) => break r,
            None => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    };
    let inner = match SqliteLeaseStore::new(core.state.conn().clone()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("lease: state store: {e}");
            return;
        }
    };
    let store = Arc::new(StateLease {
        inner,
        state: core.state.clone(),
    });
    let mut lc = LeaseConfig::new(
        core.instance_id.clone(),
        crate::state::random_id(),
        cfg.instance_name.clone(),
        cfg.bunko_version.clone(),
    );
    lc.last_boot_id = store.load().and_then(|r| r.boot_id);
    let mega = core.caps.push && !core.caps.atomic_replace;
    lc.ttl = cfg
        .lease_ttl
        .unwrap_or(if mega { MEGA_TTL } else { DEFAULT_TTL });
    lc.feed_interval = cfg.poll_idle;
    lc.heartbeat_new_node = mega;
    let lease = Lease::new(core.backend.clone(), root, lc, store, cfg.dir.join("lease"));
    let _ = core.fence.set(lease.fence());
    let handle = lease.spawn();
    let mut events = handle.subscribe();
    let _ = slot.set(handle);
    core.wake.notify_one();
    while events.changed().await.is_ok() {
        let lost = matches!(
            *events.borrow(),
            LeaseEvent::Standby { .. } | LeaseEvent::Lost { .. }
        );
        if lost {
            // Queued status-file ops are dropped on lease loss (§5.10).
            let status: Vec<Op> = core
                .ops
                .lock()
                .values()
                .filter(|o| o.origin == bunko_store::Origin::Status && o.state.live())
                .cloned()
                .collect();
            for mut o in status {
                core.finish(&mut o, crate::outbox::OpState::Superseded);
            }
        }
        core.wake.notify_one();
    }
}

impl<B: Backend> Drop for RemoteStore<B> {
    fn drop(&mut self) {
        // Safe in any context, async included; the feed is crash-safe.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
        self.events.close();
    }
}

impl<B: Backend> LibraryStore for RemoteStore<B> {
    fn root(&self) -> &Path {
        &self.cfg.root
    }

    fn caps(&self) -> Caps {
        Caps {
            remote: true,
            case_sensitive: true,
            symlinks: false,
            local_paths: false,
            atomic_replace: self.core.caps.atomic_replace,
            server_copy: self.core.caps.server_copy,
            trash_on_delete: true,
        }
    }

    fn health(&self) -> Health {
        if let Some(l) = self.lease.get()
            && let LeaseEvent::Standby { holder } | LeaseEvent::Lost { holder } = l.event()
            && self.core.feed.state() == EngineState::Steady
        {
            return Health::LeaseHeld {
                by: if holder.instance_name.is_empty() {
                    holder.instance_id.clone()
                } else {
                    holder.instance_name.clone()
                },
            };
        }
        match self.core.feed.state() {
            EngineState::Steady => Health::Online,
            EngineState::FullSync { done, total } => Health::Syncing {
                done,
                total: total.unwrap_or(0),
            },
            EngineState::Init | EngineState::LocateRoots => Health::Syncing { done: 0, total: 0 },
            EngineState::Suspect { .. } => Health::Suspect,
            EngineState::Offline => Health::Offline,
            EngineState::NeedsReauth => Health::NeedsReauth,
        }
    }

    fn metadata(&self, p: &Path) -> io::Result<Meta> {
        match self.lookup(p) {
            Ok(e) => Ok(self.meta(&e)),
            Err(e) => match self.staging_file(p) {
                Some(local) => std::fs::metadata(local).map(Meta::Std),
                None => Err(e),
            },
        }
    }

    fn symlink_metadata(&self, p: &Path) -> io::Result<Meta> {
        self.metadata(p)
    }

    fn read_dir(&self, p: &Path) -> io::Result<ReadDir> {
        let rel = self.rel(p)?;
        let children = {
            let idx = self.core.index.lock();
            let e = idx.lookup(&rel).ok_or_else(|| not_found(p))?;
            if !e.is_dir {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("{} is not a directory", p.display()),
                ));
            }
            idx.children(e.eid)
        };
        let mut out: Vec<io::Result<DirEntry>> = children
            .iter()
            .map(|c| {
                Ok(DirEntry::Remote(RemoteEntry {
                    path: p.join(&c.name),
                    name: OsString::from(&c.name),
                    meta: self.meta(c),
                }))
            })
            .collect();
        // Staging files in this folder that are not (yet) published.
        let names: HashSet<&str> = children.iter().map(|c| c.name.as_str()).collect();
        let staged: Vec<(String, PathBuf)> = self
            .core
            .staging
            .lock()
            .iter()
            .filter(|(r, _)| {
                r.segments().len() == rel.segments().len() + 1
                    && r.segments().starts_with(rel.segments())
            })
            .map(|(r, f)| (r.segments().last().cloned().unwrap_or_default(), f.clone()))
            .collect();
        for (name, local) in staged {
            if names.contains(name.as_str()) {
                continue;
            }
            if let Ok(m) = std::fs::metadata(&local) {
                out.push(Ok(DirEntry::Remote(RemoteEntry {
                    path: p.join(&name),
                    name: OsString::from(&name),
                    meta: Meta::Std(m),
                })));
            }
        }
        Ok(ReadDir::Remote(out.into_iter()))
    }

    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        let e = self.lookup(p)?;
        let rel = self
            .core
            .index
            .lock()
            .path_of(e.eid)
            .ok_or_else(|| not_found(p))?;
        let mut out = self.cfg.root.clone();
        for s in rel.segments() {
            out.push(s);
        }
        Ok(out)
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        self.lookup(p)?;
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a remote library has no symlinks",
        ))
    }

    fn open(&self, p: &Path) -> io::Result<File> {
        self.guard("open");
        if let Some(f) = self.open_local(p)? {
            return Ok(f);
        }
        let (e, v) = self.file_ver(p)?;
        let meta = self.meta(&e);
        if v.size <= SMALL_MAX {
            let (data, _) = self.small_bytes(&v)?;
            Ok(File::Remote(Box::new(CachedFile::new(data, meta))))
        } else {
            Ok(File::Remote(Box::new(ArchiveFile::new(
                self.arc(),
                v,
                meta,
            ))))
        }
    }

    fn local_path(&self, p: &Path, expect: Option<&Version>) -> io::Result<LocalPath> {
        self.guard("local_path");
        if let Some(local) = self.local_bytes(p)? {
            return Ok(LocalPath::InPlace(local));
        }
        let (_, v) = self.file_ver(p)?;
        if let Some(want) = expect
            && want.as_str() != v.cver
        {
            return Err(io::Error::other(BErr::PreconditionFailed));
        }
        let pin = self.spool_for(&v, true)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::StorageFull, "the spool budget is full")
        })?;
        let entry = pin.entry.clone();
        // Until the download ended and was verified.
        let fill = self.block_on("local_path", entry.wait(u64::MAX));
        match fill.done {
            Some(Ok(())) => Ok(LocalPath::Spooled(Arc::new(pin))),
            Some(Err(e)) => Err(berr_to_io(e)),
            None => Err(io::Error::other("the spool download stopped")),
        }
    }

    fn content_sha256(&self, p: &Path) -> io::Result<Option<[u8; 32]>> {
        self.guard("content_sha256");
        if let Ok(e) = self.lookup(p)
            && let Some(op) = e.pending_op.as_deref().and_then(|id| self.pending(id))
        {
            return Ok(op.sha256);
        }
        let (_, v) = self.file_ver(p)?;
        if v.size <= SMALL_MAX {
            return self.small_bytes(&v).map(|(_, sha)| Some(sha));
        }
        // A large file: known only once a verified spool computed it.
        Ok(self
            .core
            .spool
            .attach(&v.key())
            .and_then(|pin| pin.entry.sha256.get().copied()))
    }

    fn open_with(&self, p: &Path, o: &OpenOptions) -> io::Result<File> {
        self.open_with_impl(p, o)
    }

    fn publish(&self, staged: &Path, dest: &Path, w: &WriteIntent) -> io::Result<Published> {
        self.publish_impl(staged, dest, w)
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename_impl(from, to)
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.copy_impl(from, to)
    }

    fn remove_file(&self, p: &Path) -> io::Result<()> {
        self.remove_impl(p, crate::write::Remove::File)
    }

    fn remove_dir(&self, p: &Path) -> io::Result<()> {
        self.remove_impl(p, crate::write::Remove::EmptyDir)
    }

    fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
        self.remove_impl(p, crate::write::Remove::Tree)
    }

    fn create_dir(&self, p: &Path) -> io::Result<()> {
        self.create_dir_impl(p, false)
    }

    fn create_dir_all(&self, p: &Path) -> io::Result<()> {
        self.create_dir_impl(p, true)
    }

    fn watch(&self, sink: WatchSink) -> io::Result<WatchGuard> {
        let id = self.sinks.next.fetch_add(1, Ordering::Relaxed);
        self.sinks.list.lock().push((id, sink));
        Ok(WatchGuard(Box::new(SinkReg {
            sinks: Arc::downgrade(&self.sinks),
            id,
        })))
    }

    fn recently_removed(&self, p: &Path) -> Option<SystemTime> {
        self.core.feed.recently_removed(p)
    }

    fn case_winner(&self, parent: &Path, fold: &str) -> Option<String> {
        let rel = self.rel(parent).ok()?;
        let idx = self.core.index.lock();
        let e = idx.lookup(&rel).filter(|e| e.is_dir)?;
        idx.variants(e.eid, &crate::names::fold(fold))
            .into_iter()
            .next()
            .map(|v| v.name)
    }
}
