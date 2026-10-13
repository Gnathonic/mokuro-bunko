//! Where the library lives (0.8 design §1, §5.1, §11): on this disk, or in a cloud
//! folder behind a `bunko_remote::RemoteStore`.
//!
//! [`open_library_store`] builds the store `storage.library` names and returns it
//! mounted at its synthetic root `<base>/cloud/<store-id>/library` (D2), which never
//! exists on disk: every library access in the server goes through the mount. The
//! store id is `sha256(provider ‖ pairing ‖ account_id ‖ folder)[..16]` (§5.1); its
//! local state (index, caches, spool) lives in `<base>/cloud/<store-id>/`.
//!
//! * **webdav**: the URL and user from config; the password from
//!   `MOKURO_WEBDAV_PASSWORD_FILE` or the credentials file. The account id is
//!   `user@url`.
//! * **gdrive / onedrive**: the sign-in from the credentials file
//!   (`<base>/cloud/credentials.json`). The entry whose own store id matches the
//!   configured provider, pairing and folder is used; its `account_id` is part of the id.
//! * **mega**: the session on file (`mega_session`, from `library connect mega`);
//!   the password is never stored, so a lapsed session means a new sign-in.
//!
//! A cloud library that cannot be opened (not signed in, no password, the pairing
//! changed) is not a startup failure: an [`UnconnectedStore`] is mounted instead, the
//! server runs with an empty library, and the admin panel reports why
//! ([`StoreHandle::status_json`]).
//!
//! With the `testing` feature, [`testing::register`] hands a `FakeBackend` to the next
//! server opened over a storage base path, whatever cloud backend its config names.

pub mod case_merge;
pub mod connect;
mod control;
pub mod disconnect;
pub mod doctor;
pub mod hooks;
mod unconnected;

#[cfg(feature = "testing")]
pub mod testing;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bunko_core::config::{GoogleProject, LibraryBackend, LibraryConfig};
use bunko_core::{Config, StorageLayout};
use bunko_remote::{Backend, CacheStats, CloudHooks, EngineState, RemoteStore, RemoteStoreConfig};
use bunko_store::{Health, LibraryStore, MountGuard};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub use case_merge::ServerCaseMergePolicy;
pub use control::{StorageControl, storage_problems};
pub use hooks::ServerCloudHooks;
pub use unconnected::UnconnectedStore;

/// `MOKURO_WEBDAV_PASSWORD_FILE`: a file holding the WebDAV password (§11).
pub const WEBDAV_PASSWORD_FILE_ENV: &str = "MOKURO_WEBDAV_PASSWORD_FILE";

/// The store id of §5.1: the first 16 hex digits of
/// `sha256(provider ‖ pairing ‖ account_id ‖ folder)`, fields NUL-separated.
pub fn store_id(provider: &str, pairing: &str, account_id: &str, folder: &str) -> String {
    let mut h = Sha256::new();
    for (i, part) in [provider, pairing, account_id, folder].iter().enumerate() {
        if i > 0 {
            h.update([0u8]);
        }
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())[..16].to_string()
}

/// The Google pairing part of the store id (D25): `release`, `beta`, or
/// `custom:<client_id>` (the Desktop override's id, else the device override's). Empty
/// for every other provider.
pub fn pairing_of(lib: &LibraryConfig) -> String {
    if lib.backend != LibraryBackend::GDrive {
        return String::new();
    }
    let o = &lib.oauth.google;
    if lib.google_project == GoogleProject::Custom || o.any() {
        let id = o
            .client_id
            .as_deref()
            .or(o.device_client_id.as_deref())
            .unwrap_or("");
        return format!("custom:{id}");
    }
    lib.google_project.as_str().to_string()
}

/// The provider defaults of the poll cadence (§5.3), overridden by `storage.library.poll`.
fn poll_intervals(lib: &LibraryConfig) -> (Duration, Duration) {
    let (active, idle) = match lib.backend {
        LibraryBackend::GDrive => (30, 300),
        _ => (60, 300),
    };
    (
        Duration::from_secs(u64::from(lib.poll.active_s.unwrap_or(active).max(1))),
        Duration::from_secs(u64::from(lib.poll.idle_s.unwrap_or(idle).max(1))),
    )
}

/// What the admin panel and `library status` show of a running cloud store.
pub trait CloudStore: Send + Sync {
    fn health(&self) -> Health;
    fn engine_state(&self) -> EngineState;
    fn cache_stats(&self) -> CacheStats;
    /// Block (a content-kind call: never from inside an async task) until the change
    /// feed is steady; false on timeout.
    fn wait_steady(&self, timeout: Duration) -> bool;
    fn set_hooks(&self, hooks: Arc<dyn CloudHooks>);
    /// Publish the cloud queue file from the server's queue document (§9.1; the store
    /// dedups and throttles). `true` when a write was queued or scheduled.
    fn publish_queue_doc(&self, doc: Value) -> io::Result<bool>;
    /// Every op in the outbox.
    fn outbox(&self) -> Vec<bunko_remote::outbox::OpSummary>;
    /// Unresolved conflict rows `(kind, path, detail)`.
    fn conflicts(&self) -> Vec<(String, String, String)>;
    /// Submission rows `(path, cver, state)`.
    fn submissions(&self) -> Vec<(String, String, String)>;
    /// Block until no op waits to commit; false on timeout.
    fn wait_outbox_idle(&self, timeout: Duration) -> bool;
    /// Copy the owed results' bytes to a folder (Disconnect → Export).
    fn export_outbox(&self, to: &Path) -> io::Result<Vec<PathBuf>>;
    /// Write `released` into the marker (Disconnect step 2).
    fn release_lease(&self, timeout: Duration) -> io::Result<bool>;
    /// The lease's latest event (`None`: no lease runs yet).
    fn lease_event(&self) -> Option<bunko_remote::status::LeaseEvent>;
    /// The admin's Take over (blocking).
    fn take_over(&self, timeout: Duration) -> io::Result<()>;
    /// The provider's storage quota (one call).
    fn quota(&self, timeout: Duration) -> io::Result<Option<bunko_store::Capacity>>;
    /// This install's id.
    fn instance_id(&self) -> String;
    /// Install the server's case-merge policy (§5.9): merges may run from now on.
    fn set_case_merge_policy(&self, p: Arc<dyn bunko_remote::CaseMergePolicy>);
    /// Unresolved conflicts with their ids (and a blocked merge's two sides).
    fn conflict_rows(&self) -> Vec<bunko_remote::Conflict>;
    /// "Keep this copy" / "Keep that copy" of a `case-merge-blocked` conflict.
    fn resolve_conflict(&self, id: i64, choice: bunko_remote::ResolveChoice) -> io::Result<String>;
    /// The latest case merges, newest first.
    fn recent_merges(&self) -> Vec<bunko_remote::MergeRecord>;
}

impl<B: Backend> CloudStore for RemoteStore<B> {
    fn health(&self) -> Health {
        LibraryStore::health(self)
    }
    fn engine_state(&self) -> EngineState {
        RemoteStore::engine_state(self)
    }
    fn cache_stats(&self) -> CacheStats {
        RemoteStore::cache_stats(self)
    }
    fn wait_steady(&self, timeout: Duration) -> bool {
        RemoteStore::wait_steady(self, timeout)
    }
    fn set_hooks(&self, hooks: Arc<dyn CloudHooks>) {
        RemoteStore::set_hooks(self, hooks)
    }
    fn publish_queue_doc(&self, doc: Value) -> io::Result<bool> {
        RemoteStore::publish_queue_doc(self, doc)
    }
    fn outbox(&self) -> Vec<bunko_remote::outbox::OpSummary> {
        RemoteStore::outbox(self)
    }
    fn conflicts(&self) -> Vec<(String, String, String)> {
        RemoteStore::conflicts(self)
    }
    fn submissions(&self) -> Vec<(String, String, String)> {
        RemoteStore::submissions(self)
    }
    fn wait_outbox_idle(&self, timeout: Duration) -> bool {
        RemoteStore::wait_outbox_idle(self, timeout)
    }
    fn export_outbox(&self, to: &Path) -> io::Result<Vec<PathBuf>> {
        RemoteStore::export_outbox(self, to)
    }
    fn release_lease(&self, timeout: Duration) -> io::Result<bool> {
        RemoteStore::release_lease(self, timeout)
    }
    fn lease_event(&self) -> Option<bunko_remote::status::LeaseEvent> {
        self.lease().map(|l| l.event())
    }
    fn take_over(&self, timeout: Duration) -> io::Result<()> {
        RemoteStore::take_over_lease(self, timeout)
    }
    fn quota(&self, timeout: Duration) -> io::Result<Option<bunko_store::Capacity>> {
        RemoteStore::quota(self, timeout)
    }
    fn instance_id(&self) -> String {
        RemoteStore::instance_id(self).to_string()
    }
    fn set_case_merge_policy(&self, p: Arc<dyn bunko_remote::CaseMergePolicy>) {
        RemoteStore::set_case_merge_policy(self, p)
    }
    fn conflict_rows(&self) -> Vec<bunko_remote::Conflict> {
        RemoteStore::conflict_rows(self)
    }
    fn resolve_conflict(&self, id: i64, choice: bunko_remote::ResolveChoice) -> io::Result<String> {
        RemoteStore::resolve_conflict(self, id, choice)
    }
    fn recent_merges(&self) -> Vec<bunko_remote::MergeRecord> {
        RemoteStore::recent_merges(self)
    }
}

/// Keeps the runtime a driver connected on alive (its pooled connections run there),
/// and shuts it down without waiting when the store goes.
struct ConnectRuntime(Option<tokio::runtime::Runtime>);

impl Drop for ConnectRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_background();
        }
    }
}

/// The opened cloud library: what it is, and the running store (or why there is none).
pub struct StoreHandle {
    pub backend: LibraryBackend,
    pub folder: String,
    /// `None` until a store could be identified (no sign-in yet).
    pub store_id: Option<String>,
    /// The synthetic library root the store is mounted at.
    pub root: PathBuf,
    /// The account's display label (never a secret).
    pub account: Option<String>,
    /// The Google pairing (D25); `None` for other providers.
    pub pairing: Option<String>,
    store: Option<Arc<dyn CloudStore>>,
    /// Why there is no store.
    problem: Option<String>,
    /// D25: the configured Google pairing differs from the connected store's
    /// (configured, connected); held with `storage-auth` until Disconnect.
    pub pairing_change: Option<(String, String)>,
    /// `<base>/cloud`.
    pub cloud_dir: PathBuf,
    /// When the feed was last seen in sync (sampled by every status read).
    synced_at: parking_lot::Mutex<Option<std::time::SystemTime>>,
    /// The provider quota, asked at most every [`QUOTA_EVERY`].
    quota: parking_lot::Mutex<Option<(std::time::Instant, Value)>>,
    #[cfg(feature = "testing")]
    fake: Option<Arc<RemoteStore<bunko_remote::fake::FakeBackend>>>,
    // Last: the store (and its runtime) goes first.
    _connect_rt: Option<ConnectRuntime>,
}

/// How often the admin status asks the provider for its quota.
pub const QUOTA_EVERY: Duration = Duration::from_secs(600);

impl StoreHandle {
    fn unconnected(lib: &LibraryConfig, root: PathBuf, problem: String) -> StoreHandle {
        tracing::warn!(
            "[STORAGE] The {} library is not connected: {problem}",
            lib.backend.as_str()
        );
        StoreHandle {
            backend: lib.backend,
            folder: lib.folder.clone(),
            store_id: None,
            root,
            account: None,
            pairing: (lib.backend == LibraryBackend::GDrive).then(|| pairing_of(lib)),
            store: None,
            problem: Some(problem),
            pairing_change: None,
            cloud_dir: PathBuf::new(),
            synced_at: parking_lot::Mutex::new(None),
            quota: parking_lot::Mutex::new(None),
            #[cfg(feature = "testing")]
            fake: None,
            _connect_rt: None,
        }
    }

    /// A store runs (it may still be syncing, offline or need a new sign-in).
    pub fn connected(&self) -> bool {
        self.store.is_some()
    }

    /// Why no store runs, if none does.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    pub fn health(&self) -> Health {
        match &self.store {
            Some(s) => s.health(),
            None => Health::NeedsReauth,
        }
    }

    pub fn engine_state(&self) -> Option<EngineState> {
        self.store.as_ref().map(|s| s.engine_state())
    }

    /// Block until the feed is steady (true at once without a store: there is nothing
    /// to wait for). Never call from inside an async task.
    pub fn wait_steady(&self, timeout: Duration) -> bool {
        match &self.store {
            Some(s) => s.wait_steady(timeout),
            None => true,
        }
    }

    /// Publish the cloud copy of the queue file (§9.1); nothing without a store.
    pub fn publish_queue_doc(&self, doc: Value) -> io::Result<bool> {
        match &self.store {
            Some(s) => s.publish_queue_doc(doc),
            None => Ok(false),
        }
    }

    /// Install the server's cloud hooks (D20).
    pub fn set_hooks(&self, hooks: Arc<dyn CloudHooks>) {
        if let Some(s) = &self.store {
            s.set_hooks(hooks);
        }
    }

    /// Install the server's case-merge policy (§5.9).
    pub fn set_case_merge_policy(&self, p: Arc<dyn bunko_remote::CaseMergePolicy>) {
        if let Some(s) = &self.store {
            s.set_case_merge_policy(p);
        }
    }

    /// "Keep this copy" / "Keep that copy" (§5.9) of conflict `id`: the other side goes
    /// to the provider trash through a merge journal group.
    pub fn resolve_conflict(
        &self,
        id: i64,
        choice: bunko_remote::ResolveChoice,
    ) -> io::Result<String> {
        match &self.store {
            Some(s) => s.resolve_conflict(id, choice),
            None => Err(io::Error::other("the library is not connected")),
        }
    }

    /// The test fake behind this store (feature `testing`).
    #[cfg(feature = "testing")]
    pub fn fake_store(&self) -> Option<&Arc<RemoteStore<bunko_remote::fake::FakeBackend>>> {
        self.fake.as_ref()
    }

    /// The admin status's `library.storage` block (§11): what the store is, its
    /// health and sync, caches, the outbox, conflicts, the lease, the provider quota and
    /// the `problems[]` of kinds `storage-*`. Never a secret.
    pub fn status_json(&self) -> Value {
        let (engine, progress) = match self.engine_state() {
            None => ("unconnected".to_string(), Value::Null),
            Some(e) => engine_json(&e),
        };
        let health = self.health();
        let now = std::time::SystemTime::now();
        if self.store.is_some() && health == Health::Online && engine == "steady" {
            *self.synced_at.lock() = Some(now);
        }
        let synced_at = *self.synced_at.lock();
        let cache = self.store.as_ref().map(|s| {
            let c = s.cache_stats();
            json!({
                "blobs": c.blobs,
                "blob_bytes": c.blob_bytes,
                "zipdirs": c.zipdirs,
                "blocks": c.blocks,
                "block_bytes": c.block_bytes,
                "spools": c.spools,
                "spool_bytes": c.spool_bytes,
            })
        });
        let outbox = self.store.as_ref().map(|s| outbox_json(&s.outbox()));
        let conflicts: Vec<Value> = self
            .store
            .as_ref()
            .map(|s| s.conflict_rows())
            .unwrap_or_default()
            .into_iter()
            .map(|c| {
                // A blocked case merge offers "Keep this copy" / "Keep that copy".
                let resolvable = c.kind == "case-merge-blocked" && !c.this.is_empty();
                json!({"id": c.id, "kind": c.kind, "path": c.path, "detail": c.detail,
                       "first_seen": c.first_seen, "this": c.this, "that": c.that,
                       "resolvable": resolvable})
            })
            .collect();
        let merges: Vec<Value> = self
            .store
            .as_ref()
            .map(|s| s.recent_merges())
            .unwrap_or_default()
            .into_iter()
            .take(20)
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .collect();
        let submissions = self.store.as_ref().map(|s| {
            let rows = s.submissions();
            let waiting = rows
                .iter()
                .filter(|(_, _, st)| !matches!(st.as_str(), "applied" | "refused"))
                .count();
            json!({"total": rows.len(), "waiting": waiting})
        });
        let lease = self
            .store
            .as_ref()
            .map(|s| lease_json(s.lease_event(), &s.instance_id()));
        let problems = storage_problems(self, &health, conflicts.len());
        json!({
            "backend": self.backend.as_str(),
            "provider": connect::provider_name(self.backend),
            "folder": self.folder,
            "store_id": self.store_id,
            "account": self.account,
            "pairing": self.pairing,
            "pairing_label": self.pairing.as_deref().map(connect::pairing_label),
            "pairing_change": self.pairing_change.as_ref().map(|(configured, connected)| json!({
                "configured": configured, "connected": connected,
            })),
            "connected": self.connected(),
            "problem": self.problem,
            "health": health_json(&health),
            "engine": engine,
            "sync": progress,
            "synced_at": synced_at.map(unix_s),
            "cache": cache,
            "outbox": outbox,
            "conflicts": conflicts,
            "recent_merges": merges,
            "submissions": submissions,
            "lease": lease,
            "quota": self.quota_json(),
            "problems": problems,
        })
    }

    /// The provider's quota, asked at most every [`QUOTA_EVERY`] (`null` until then,
    /// or when the provider reports none).
    fn quota_json(&self) -> Value {
        let Some(store) = &self.store else {
            return Value::Null;
        };
        if let Some((at, v)) = &*self.quota.lock()
            && at.elapsed() < QUOTA_EVERY
        {
            return v.clone();
        }
        let v = match store.quota(Duration::from_secs(10)) {
            Ok(Some(c)) => json!({"total": c.total, "available": c.available,
                "used": c.total.saturating_sub(c.available)}),
            Ok(None) => Value::Null,
            Err(e) => json!({"error": e.to_string()}),
        };
        *self.quota.lock() = Some((std::time::Instant::now(), v.clone()));
        v
    }

    /// The admin's Take over (§9.2).
    pub fn take_over(&self) -> io::Result<()> {
        match &self.store {
            Some(s) => s.take_over(Duration::from_secs(60)),
            None => Err(io::Error::other("the library is not connected")),
        }
    }
}

fn unix_s(t: std::time::SystemTime) -> f64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// The outbox as the Storage panel shows it: counts by state, the oldest op still
/// owed, the last error, the held and failed ops.
fn outbox_json(ops: &[bunko_remote::outbox::OpSummary]) -> Value {
    use bunko_remote::outbox::OpSummary;
    let owed: Vec<&OpSummary> = ops
        .iter()
        .filter(|o| {
            matches!(
                o.state,
                "PREPARED" | "STAGED" | "HELD" | "PREFLIGHT" | "UPLOADING" | "FAILED"
            )
        })
        .collect();
    let count = |st: &str| ops.iter().filter(|o| o.state == st).count();
    let op = |o: &OpSummary| {
        json!({"id": o.id, "kind": o.kind, "path": o.dest, "state": o.state,
               "attempts": o.attempts, "last_error": o.last_error, "origin": o.origin})
    };
    let last_error = owed.iter().rev().find_map(|o| o.last_error.clone());
    json!({
        "owed": owed.len(),
        "uploading": count("UPLOADING") + count("PREFLIGHT"),
        "held": count("HELD"),
        "failed": count("FAILED"),
        "oldest": owed.iter().min_by_key(|o| o.id).map(|o| op(o)),
        "last_error": last_error,
        "owed_ops": owed.iter().take(50).map(|o| op(o)).collect::<Vec<_>>(),
        "held_ops": ops.iter().filter(|o| o.state == "HELD").take(20).map(op).collect::<Vec<_>>(),
        "failed_ops": ops.iter().filter(|o| o.state == "FAILED").take(20).map(op).collect::<Vec<_>>(),
    })
}

/// The lease as the Storage panel shows it (holder, epoch, last seen).
fn lease_json(ev: Option<bunko_remote::status::LeaseEvent>, ours: &str) -> Value {
    use bunko_remote::status::LeaseEvent;
    let holder = |m: &bunko_remote::status::Marker| {
        json!({
            "instance_id": m.instance_id,
            "name": m.instance_name,
            "epoch": m.epoch,
            "last_seen": m.heartbeat_at,
            "version": m.bunko_version,
            "ours": m.instance_id == ours,
        })
    };
    match ev {
        None => json!({"state": "not-started", "can_take_over": false}),
        Some(LeaseEvent::Claiming) => json!({"state": "claiming", "can_take_over": false}),
        Some(LeaseEvent::Acquired) => json!({"state": "held", "can_take_over": false,
            "holder": {"instance_id": ours, "ours": true}}),
        Some(LeaseEvent::Standby { holder: m }) => {
            json!({"state": "standby", "can_take_over": true, "holder": holder(&m)})
        }
        Some(LeaseEvent::Lost { holder: m }) => {
            json!({"state": "lost", "can_take_over": true, "holder": holder(&m)})
        }
        Some(LeaseEvent::HeartbeatFailed { error }) => {
            json!({"state": "heartbeat-failed", "can_take_over": false, "error": error})
        }
        Some(LeaseEvent::Released) => json!({"state": "released", "can_take_over": false}),
    }
}

/// D25: the configured Google pairing and the one the Drive sign-in on file was made
/// with, when they differ (configured, connected). Overrides count as `custom`.
pub fn pairing_change(lib: &LibraryConfig, cloud: &Path) -> Option<(String, String)> {
    if lib.backend != LibraryBackend::GDrive {
        return None;
    }
    let configured = pairing_of(lib);
    let creds = bunko_remote::auth::CredentialStore::open(cloud).ok()?;
    let mut connected = None;
    for id in creds.store_ids().ok()? {
        let Ok(Some(c)) = creds.get(&id) else {
            continue;
        };
        if c.provider != "gdrive" {
            continue;
        }
        let Some(account) = c.account_id.as_deref() else {
            continue;
        };
        if store_id("gdrive", &configured, account, &lib.folder) == id {
            return None;
        }
        let theirs = match c.client_id.as_deref().and_then(connect::pairing_of_client) {
            Some(p) => p.as_str().to_string(),
            None => format!("custom:{}", c.client_id.as_deref().unwrap_or("")),
        };
        if theirs != configured {
            connected = Some(theirs);
        }
    }
    connected.map(|c| (configured, c))
}

/// `{"state": "online" | ..., ...}` of a store's health.
pub fn health_json(h: &Health) -> Value {
    match h {
        Health::Online => json!({"state": "online"}),
        Health::Syncing { done, total } => {
            json!({"state": "syncing", "done": done, "total": total})
        }
        Health::Degraded { retry_at } => json!({
            "state": "degraded",
            "retry_at": retry_at
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |d| d.as_secs_f64()),
        }),
        Health::Offline => json!({"state": "offline"}),
        Health::NeedsReauth => json!({"state": "needs-reauth"}),
        Health::Blocked(b) => json!({"state": "blocked", "reason": format!("{b:?}")}),
        Health::Suspect => json!({"state": "suspect"}),
        Health::LeaseHeld { by } => json!({"state": "lease-held", "by": by}),
    }
}

fn engine_json(e: &EngineState) -> (String, Value) {
    match e {
        EngineState::Init => ("init".into(), Value::Null),
        EngineState::LocateRoots => ("locate-roots".into(), Value::Null),
        EngineState::FullSync { done, total } => {
            ("full-sync".into(), json!({"done": done, "total": total}))
        }
        EngineState::Steady => ("steady".into(), Value::Null),
        EngineState::Suspect {
            pending_removals,
            root_changed,
        } => (
            "suspect".into(),
            json!({"pending_removals": pending_removals, "root_changed": root_changed}),
        ),
        EngineState::Offline => ("offline".into(), Value::Null),
        EngineState::NeedsReauth => ("needs-reauth".into(), Value::Null),
    }
}

/// A mounted library store, its mount, and (for a cloud library) its handle.
pub type OpenedStore = (Arc<dyn LibraryStore>, MountGuard, Arc<StoreHandle>);

/// What identifies the store `storage.library` names, and how to open it.
#[allow(clippy::large_enum_variant)] // one short-lived value per start
enum Plan {
    WebDav {
        id: String,
        cfg: Box<bunko_remote::webdav::WebDavConfig>,
        account: String,
    },
    OAuth {
        id: String,
        account: Option<String>,
        backend: OAuthBackend,
    },
    Mega {
        id: String,
        account: Option<String>,
        session: bunko_remote::mega::SessionBlob,
    },
    #[cfg(feature = "testing")]
    Fake {
        id: String,
        fake: Arc<bunko_remote::fake::FakeBackend>,
        tune: Option<testing::Tune>,
    },
    Unconnected(String),
}

enum OAuthBackend {
    GDrive(bunko_remote::auth::OAuthClient),
    OneDrive(bunko_remote::auth::OAuthClient),
}

/// Open the library store `config` names and mount it. `None` for a local library (the
/// server mounts its `LocalStore` itself). A cloud library is always mounted: its
/// `RemoteStore`, or an [`UnconnectedStore`] (with the reason in the handle) when it
/// cannot be opened yet. The only error is a store directory that cannot be created.
pub fn open_library_store(
    config: &Config,
    layout: &StorageLayout,
) -> io::Result<Option<OpenedStore>> {
    let cloud = layout.cloud_dir();
    // What a Disconnect left for this start to remove (nothing has it open now).
    disconnect::sweep_disconnected(&cloud);
    if !config.is_cloud_library() {
        return Ok(None);
    }
    let lib = &config.storage.library;
    // Provider errors answer clients by class (412, 503 + Retry-After, 507; §3.5).
    bunko_store::register_classifier(classify_berr);
    let plan = plan(lib, layout)?;
    let open_remote = |id: &str| -> (PathBuf, RemoteStoreConfig) {
        let dir = cloud.join(id);
        let root = dir.join("library");
        let (active, idle) = poll_intervals(lib);
        let mut rc = RemoteStoreConfig::new(root.clone(), dir, lib.folder.clone());
        rc.cache_bytes = u64::from(lib.cache_mb) << 20;
        rc.spool_bytes = u64::from(lib.spool_gb) << 30;
        rc.quiet = Duration::from_secs(u64::from(lib.quiet_s));
        rc.poll_active = active;
        rc.poll_idle = idle;
        rc.mount_id = u64::from_str_radix(&id[..id.len().min(15)], 16).unwrap_or(1) | 1;
        rc.instance_name = instance_name();
        rc.bunko_version = bunko_core::VERSION.to_string();
        (root, rc)
    };
    let unconnected_root = layout
        .clone()
        .with_library_root(
            cloud
                .join(bunko_core::storage::UNCONNECTED_STORE)
                .join("library"),
        )
        .library();
    let pairing = (lib.backend == LibraryBackend::GDrive).then(|| pairing_of(lib));
    let handle_of = |id: String, root: PathBuf, account: Option<String>| StoreHandle {
        backend: lib.backend,
        folder: lib.folder.clone(),
        store_id: Some(id),
        root,
        account,
        pairing: pairing.clone(),
        store: None,
        problem: None,
        pairing_change: None,
        cloud_dir: PathBuf::new(),
        synced_at: parking_lot::Mutex::new(None),
        quota: parking_lot::Mutex::new(None),
        #[cfg(feature = "testing")]
        fake: None,
        _connect_rt: None,
    };
    let opened: (Arc<dyn LibraryStore>, StoreHandle) = match plan {
        Plan::Unconnected(problem) => {
            let store = Arc::new(UnconnectedStore::new(unconnected_root.clone(), &problem));
            (
                store,
                StoreHandle::unconnected(lib, unconnected_root, problem),
            )
        }
        Plan::WebDav { id, cfg, account } => {
            let (root, rc) = open_remote(&id);
            let mut cfg = *cfg;
            cfg.state_dir = rc.dir.clone();
            match connect_on_own_runtime(bunko_remote::webdav::WebDavBackend::connect(cfg)) {
                Ok((backend, rt)) => {
                    let store = RemoteStore::open(rc, backend)?;
                    let mut h = handle_of(id, root, Some(account));
                    h.store = Some(store.clone());
                    h._connect_rt = Some(rt);
                    (store, h)
                }
                Err(e) => {
                    let problem = format!("could not reach the WebDAV server: {e}");
                    let store = Arc::new(UnconnectedStore::new(unconnected_root.clone(), &problem));
                    (
                        store,
                        StoreHandle::unconnected(lib, unconnected_root, problem),
                    )
                }
            }
        }
        Plan::OAuth {
            id,
            account,
            backend,
        } => {
            let (root, rc) = open_remote(&id);
            let creds = Arc::new(
                bunko_remote::auth::CredentialStore::open(&cloud).map_err(io::Error::other)?,
            );
            let mut h = handle_of(id.clone(), root, account);
            let store: Arc<dyn LibraryStore> = match backend {
                OAuthBackend::GDrive(client) => {
                    let token = token_source(client, &id, creds);
                    let b = bunko_remote::gdrive::GDriveBackend::new(
                        token,
                        bunko_remote::gdrive::GDriveConfig::new(rc.dir.clone()),
                    )
                    .map_err(bunko_remote::berr_to_io)?;
                    let s = RemoteStore::open(rc, b)?;
                    h.store = Some(s.clone());
                    s
                }
                OAuthBackend::OneDrive(client) => {
                    let token = token_source(client, &id, creds);
                    let b = bunko_remote::onedrive::OneDriveBackend::new(
                        token,
                        bunko_remote::onedrive::OneDriveConfig::default(),
                    )
                    .map_err(bunko_remote::berr_to_io)?;
                    let s = RemoteStore::open(rc, b)?;
                    h.store = Some(s.clone());
                    s
                }
            };
            (store, h)
        }
        Plan::Mega {
            id,
            account,
            session,
        } => {
            let (root, rc) = open_remote(&id);
            let mc = bunko_remote::mega::MegaConfig::new(rc.dir.clone());
            match connect_on_own_runtime(async move {
                bunko_remote::mega::MegaBackend::resume(&session, mc).await
            }) {
                Ok((backend, rt)) => {
                    let store = RemoteStore::open(rc, backend)?;
                    let mut h = handle_of(id, root, account);
                    h.store = Some(store.clone());
                    h._connect_rt = Some(rt);
                    (store, h)
                }
                Err(e) => {
                    let problem = format!(
                        "could not resume the MEGA session ({e}): sign in again with `mokuro-bunko library connect mega`"
                    );
                    let store = Arc::new(UnconnectedStore::new(unconnected_root.clone(), &problem));
                    (
                        store,
                        StoreHandle::unconnected(lib, unconnected_root, problem),
                    )
                }
            }
        }
        #[cfg(feature = "testing")]
        Plan::Fake { id, fake, tune } => {
            let (root, mut rc) = open_remote(&id);
            if let Some(tune) = tune {
                tune(&mut rc);
            }
            let s = RemoteStore::open_shared(rc, fake)?;
            let mut h = handle_of(id, root, Some("fake".into()));
            h.store = Some(s.clone());
            h.fake = Some(s.clone());
            (s, h)
        }
    };
    let (store, mut handle) = opened;
    handle.cloud_dir = cloud.clone();
    handle.pairing_change = pairing_change(lib, &cloud);
    if let Some(id) = &handle.store_id {
        tracing::info!(
            "[STORAGE] Library: {} folder {:?} (store {id}) mounted at {}",
            handle.backend.as_str(),
            handle.folder,
            handle.root.display()
        );
    }
    let guard = bunko_store::mount(store.clone());
    Ok(Some((store, guard, Arc::new(handle))))
}

/// What `config` says about the library's storage without opening anything (no
/// network): `mokuro-bunko library status` when no server answers. The same keys as
/// [`StoreHandle::status_json`] where they can be known offline, plus `store_dir` and
/// `indexed` (whether a first sync left an index there).
// unmounted: the store's own local directory.
#[allow(clippy::disallowed_methods)]
pub fn describe_offline(config: &Config) -> Value {
    let lib = &config.storage.library;
    if !config.is_cloud_library() {
        return json!({
            "backend": "local",
            "folder": config.storage.layout().library(),
        });
    }
    let layout = config.storage.layout();
    let pairing = (lib.backend == LibraryBackend::GDrive).then(|| pairing_of(lib));
    let (id, account, problem) = match plan_inner(lib, &layout) {
        Ok(Plan::WebDav { id, account, .. }) => (Some(id), Some(account), None),
        Ok(Plan::OAuth { id, account, .. }) => (Some(id), account, None),
        Ok(Plan::Mega { id, account, .. }) => (Some(id), account, None),
        #[cfg(feature = "testing")]
        Ok(Plan::Fake { id, .. }) => (Some(id), None, None),
        Ok(Plan::Unconnected(why)) => (None, None, Some(why)),
        Err(e) => (None, None, Some(e.to_string())),
    };
    let dir = id.as_ref().map(|id| layout.cloud_dir().join(id));
    json!({
        "backend": lib.backend.as_str(),
        "folder": lib.folder,
        "store_id": id,
        "account": account,
        "pairing": pairing,
        "problem": problem,
        "store_dir": dir,
        "indexed": dir.as_ref().is_some_and(|d| d.join("index.sqlite").is_file()),
    })
}

/// The id and the way to open the configured cloud store.
fn plan(lib: &LibraryConfig, layout: &StorageLayout) -> io::Result<Plan> {
    #[cfg(feature = "testing")]
    if let Some((fake, tune)) = testing::take(&layout.base) {
        return Ok(Plan::Fake {
            id: store_id("fake", "", "fake", &lib.folder),
            fake,
            tune,
        });
    }
    plan_inner(lib, layout)
}

fn plan_inner(lib: &LibraryConfig, layout: &StorageLayout) -> io::Result<Plan> {
    let cloud = layout.cloud_dir();
    match lib.backend {
        LibraryBackend::Local => Err(io::Error::other("not a cloud library")),
        LibraryBackend::Mega => signed_in_plan(lib, &cloud),
        LibraryBackend::WebDav => {
            let (Some(url), Some(user)) = (
                lib.webdav.url.clone().filter(|u| !u.trim().is_empty()),
                lib.webdav.username.clone().filter(|u| !u.is_empty()),
            ) else {
                return Ok(Plan::Unconnected(
                    "storage.library.webdav.url and .username must be set".into(),
                ));
            };
            let account = format!("{user}@{url}");
            let id = store_id("webdav", "", &account, &lib.folder);
            let Some(password) = webdav_password(&cloud, &id)? else {
                return Ok(Plan::Unconnected(format!(
                    "no WebDAV password: set {WEBDAV_PASSWORD_FILE_ENV} or run `mokuro-bunko library connect webdav`"
                )));
            };
            Ok(Plan::WebDav {
                id,
                cfg: Box::new(bunko_remote::webdav::WebDavConfig {
                    url,
                    username: user,
                    password,
                    folder: lib.folder.clone(),
                    allow_http: lib.webdav.allow_http,
                    allow_bunko_backend_for_tests: false,
                    state_dir: PathBuf::new(),
                    disable_cas: false,
                }),
                account,
            })
        }
        LibraryBackend::GDrive | LibraryBackend::OneDrive => signed_in_plan(lib, &cloud),
    }
}

/// Find the credentials entry of the configured OAuth store (§5.1, §7).
fn signed_in_plan(lib: &LibraryConfig, cloud: &Path) -> io::Result<Plan> {
    use bunko_remote::auth::{CredentialStore, google_clients, microsoft_client};
    let provider = lib.backend.as_str();
    let pairing = pairing_of(lib);
    let creds = match CredentialStore::open(cloud) {
        Ok(c) => c,
        Err(e) => return Ok(Plan::Unconnected(format!("the credentials file: {e}"))),
    };
    let ids = match creds.store_ids() {
        Ok(ids) => ids,
        Err(e) => return Ok(Plan::Unconnected(format!("the credentials file: {e}"))),
    };
    let mut other_pairing = false;
    for id in ids {
        let Ok(Some(c)) = creds.get(&id) else {
            continue;
        };
        if c.provider != provider {
            continue;
        }
        let Some(account) = c.account_id.as_deref() else {
            continue;
        };
        if store_id(provider, &pairing, account, &lib.folder) != id {
            other_pairing = true;
            continue;
        }
        let label = c.account_label.clone().or(c.account_id.clone());
        if lib.backend == LibraryBackend::Mega {
            let Some(session) = c.mega_session.clone().filter(|s| !s.is_empty()) else {
                continue;
            };
            return Ok(Plan::Mega {
                id,
                account: label,
                session: bunko_remote::mega::SessionBlob::new(session),
            });
        }
        let backend = if lib.backend == LibraryBackend::GDrive {
            let clients = google_clients(lib.google_project, &lib.oauth.google);
            let issued = c.client_id.as_deref();
            let client = [clients.loopback.clone(), clients.device.clone()]
                .into_iter()
                .flatten()
                .find(|cl| Some(cl.client_id.as_str()) == issued)
                .or(clients.loopback)
                .or(clients.device);
            let Some(client) = client else {
                return Ok(Plan::Unconnected(
                    "no Google sign-in client for the configured pairing (storage.library.oauth.google)"
                        .into(),
                ));
            };
            OAuthBackend::GDrive(client)
        } else {
            OAuthBackend::OneDrive(microsoft_client(&lib.oauth.microsoft))
        };
        return Ok(Plan::OAuth {
            id,
            account: label,
            backend,
        });
    }
    let name = match lib.backend {
        LibraryBackend::GDrive => "Google Drive",
        LibraryBackend::Mega => "MEGA",
        _ => "OneDrive",
    };
    Ok(Plan::Unconnected(
        if other_pairing && lib.backend == LibraryBackend::GDrive {
            // D25: never applied silently; held with storage-auth until Disconnect.
            format!(
                "The Google pairing changed: the Google Drive sign-in on file is for another pairing or folder than {}. Disconnect the old library first (admin → Storage → Disconnect, or `mokuro-bunko library disconnect`), then connect again",
                connect::pairing_label(&pairing)
            )
        } else if other_pairing {
            format!(
                "the {name} sign-in on file is for another pairing or folder; disconnect the old library first, then run `mokuro-bunko library connect {provider}`"
            )
        } else {
            format!("not signed in to {name}: run `mokuro-bunko library connect {provider}`")
        },
    ))
}

/// The WebDAV password: `MOKURO_WEBDAV_PASSWORD_FILE`, else the credentials entry.
// unmounted: the password file is a local secret named by the environment.
#[allow(clippy::disallowed_methods)]
fn webdav_password(cloud: &Path, id: &str) -> io::Result<Option<String>> {
    if let Some(file) = std::env::var_os(WEBDAV_PASSWORD_FILE_ENV).filter(|v| !v.is_empty()) {
        let text = std::fs::read_to_string(&file).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "{WEBDAV_PASSWORD_FILE_ENV} ({}): {e}",
                    Path::new(&file).display()
                ),
            )
        })?;
        let pw = text.trim_end_matches(['\r', '\n']).to_string();
        return Ok((!pw.is_empty()).then_some(pw));
    }
    let Ok(creds) = bunko_remote::auth::CredentialStore::open(cloud) else {
        return Ok(None);
    };
    Ok(creds
        .get(id)
        .ok()
        .flatten()
        .and_then(|c| c.webdav_password)
        .filter(|p| !p.is_empty()))
}

/// This machine's name in the lease marker ("Another Bunko (nas) manages this library").
fn instance_name() -> String {
    hostname::get()
        .ok()
        .map(|h| h.to_string_lossy().into_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "bunko".into())
}

/// A provider error by what it means for a client (spec §3.5), for
/// `bunko_store::classify`.
fn classify_berr(
    e: &(dyn std::error::Error + Send + Sync + 'static),
) -> Option<bunko_store::ErrorClass> {
    use bunko_remote::BErr;
    use bunko_store::ErrorClass;
    Some(match e.downcast_ref::<BErr>()? {
        BErr::Throttled(after) => ErrorClass::Unavailable {
            retry_after: *after,
        },
        BErr::Transient(_) => ErrorClass::Unavailable { retry_after: None },
        BErr::AuthExpired | BErr::AuthRevoked => ErrorClass::NeedsAuth,
        BErr::PreconditionFailed | BErr::NameConflict => ErrorClass::Conflict,
        BErr::StorageFull => ErrorClass::StorageFull,
        BErr::TransferQuota(after) => ErrorClass::TransferQuota {
            retry_after: *after,
        },
        BErr::InvalidName => ErrorClass::InvalidName,
        BErr::NotFound => ErrorClass::NotFound,
        _ => return None,
    })
}

/// Run a driver's connect step (the WebDAV probe, a MEGA session resume) on a runtime
/// that stays alive with the backend (its pooled connections live there), from a
/// thread of its own so this works inside or outside an async context.
fn connect_on_own_runtime<T: Send + 'static>(
    connect: impl Future<Output = Result<T, bunko_remote::BErr>> + Send + 'static,
) -> Result<(T, ConnectRuntime), String> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("bunko-store-connect")
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let handle = rt.handle().clone();
    let joined = std::thread::Builder::new()
        .name("bunko-store-connect".into())
        .spawn(move || handle.block_on(connect))
        .map_err(|e| e.to_string())?
        .join();
    match joined {
        Ok(Ok(b)) => Ok((b, ConnectRuntime(Some(rt)))),
        Ok(Err(e)) => {
            rt.shutdown_background();
            Err(e.to_string())
        }
        Err(_) => {
            rt.shutdown_background();
            Err("the connect probe panicked".into())
        }
    }
}

/// A refreshing OAuth token as the drivers' object-safe token seam.
fn token_source(
    client: bunko_remote::auth::OAuthClient,
    store_id: &str,
    creds: Arc<bunko_remote::auth::CredentialStore>,
) -> Arc<dyn bunko_remote::token_dyn::TokenSourceDyn> {
    Arc::new(DynToken(bunko_remote::auth::RefreshingToken::new(
        client, store_id, creds,
    )))
}

struct DynToken(bunko_remote::auth::RefreshingToken);

impl bunko_remote::token_dyn::TokenSourceDyn for DynToken {
    fn access_token(
        &self,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String, bunko_remote::BErr>> + Send + '_>>
    {
        use bunko_remote::auth::TokenSource;
        Box::pin(self.0.access_token())
    }
    fn invalidate(&self) {
        use bunko_remote::auth::TokenSource;
        self.0.invalidate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_ids_separate_every_field() {
        let a = store_id("gdrive", "release", "acct", "mokuro-reader");
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, store_id("gdrive", "release", "acct", "mokuro-reader"));
        assert_ne!(a, store_id("gdrive", "beta", "acct", "mokuro-reader"));
        assert_ne!(a, store_id("gdrive", "release", "acct", "manga"));
        assert_ne!(a, store_id("onedrive", "release", "acct", "mokuro-reader"));
        // The separator keeps field boundaries apart.
        assert_ne!(store_id("a", "bc", "", ""), store_id("ab", "c", "", ""));
    }

    #[test]
    fn pairing_names_the_reader_project() {
        let mut lib = LibraryConfig {
            backend: LibraryBackend::GDrive,
            ..LibraryConfig::default()
        };
        assert_eq!(pairing_of(&lib), "release");
        lib.google_project = GoogleProject::Beta;
        assert_eq!(pairing_of(&lib), "beta");
        lib.oauth.google.device_client_id = Some("dev-id".into());
        assert_eq!(pairing_of(&lib), "custom:dev-id");
        lib.oauth.google.client_id = Some("desk-id".into());
        assert_eq!(pairing_of(&lib), "custom:desk-id");
        lib.backend = LibraryBackend::OneDrive;
        assert_eq!(pairing_of(&lib), "");
    }

    fn cloud_config(dir: &Path, backend: LibraryBackend) -> Config {
        let mut c = Config::default();
        c.storage.base_path = dir.to_path_buf();
        c.storage.library.backend = backend;
        c
    }

    /// No sign-in yet: an empty unconnected library is mounted, nothing is created
    /// on disk for it, and the handle says why.
    #[test]
    fn missing_credentials_mount_an_unconnected_library() {
        let dir = tempfile::tempdir().unwrap();
        for backend in [
            LibraryBackend::GDrive,
            LibraryBackend::OneDrive,
            LibraryBackend::WebDav,
        ] {
            let c = cloud_config(dir.path(), backend);
            let layout = c.storage.layout();
            let (store, _guard, handle) = open_library_store(&c, &layout).unwrap().unwrap();
            assert!(!handle.connected(), "{backend:?}");
            assert_eq!(handle.health(), Health::NeedsReauth);
            assert!(handle.problem().is_some());
            assert_eq!(store.root(), layout.library());
            let root = layout.library();
            assert!(bunko_store::fs::is_dir(&root));
            assert_eq!(bunko_store::fs::read_dir(&root).unwrap().count(), 0);
            assert!(bunko_store::fs::File::create(root.join("x")).is_err());
            // unmounted check of the real disk.
            #[allow(clippy::disallowed_methods)]
            let on_disk = root.exists();
            assert!(!on_disk, "the synthetic root must not exist on disk");
            let st = handle.status_json();
            assert_eq!(st["connected"], false);
            assert_eq!(st["health"]["state"], "needs-reauth");
            assert_eq!(st["backend"], backend.as_str());
        }
    }

    /// A WebDAV server that cannot be reached at start: an unconnected library with the
    /// reason, from inside an async context too (the probe runs on its own runtime).
    #[tokio::test]
    async fn an_unreachable_webdav_server_mounts_an_unconnected_library() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cloud_config(dir.path(), LibraryBackend::WebDav);
        c.storage.library.webdav.url = Some("https://127.0.0.1:9/dav".into());
        c.storage.library.webdav.username = Some("me".into());
        let layout = c.storage.layout();
        let id = store_id("webdav", "", "me@https://127.0.0.1:9/dav", "mokuro-reader");
        let creds = bunko_remote::auth::CredentialStore::open(&layout.cloud_dir()).unwrap();
        creds
            .put(
                &id,
                bunko_remote::auth::StoredCredential {
                    provider: "webdav".into(),
                    webdav_password: Some("secret".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let offline = describe_offline(&c);
        assert_eq!(offline["store_id"], id.as_str());
        assert_eq!(offline["problem"], Value::Null);
        let (_store, _guard, handle) = open_library_store(&c, &layout).unwrap().unwrap();
        assert!(!handle.connected());
        assert!(
            handle
                .problem()
                .is_some_and(|p| p.contains("could not reach the WebDAV server")),
            "{:?}",
            handle.problem()
        );
        assert!(!handle.status_json().to_string().contains("secret"));
    }

    /// A Drive sign-in on file opens a RemoteStore with that account's store id; a grant
    /// without tokens reads as revoked (no network): the store reports NeedsReauth.
    #[test]
    fn a_drive_sign_in_on_file_opens_its_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = cloud_config(dir.path(), LibraryBackend::GDrive);
        let layout = c.storage.layout();
        let id = store_id("gdrive", "release", "acct-1", "mokuro-reader");
        let creds = bunko_remote::auth::CredentialStore::open(&layout.cloud_dir()).unwrap();
        let cred = bunko_remote::auth::StoredCredential {
            provider: "gdrive".into(),
            account_id: Some("acct-1".into()),
            account_label: Some("me@example.com".into()),
            client_id: Some(bunko_remote::auth::clients::RELEASE_DESKTOP_CLIENT_ID.into()),
            ..Default::default()
        };
        creds.put(&id, cred).unwrap();
        let (store, _guard, handle) = open_library_store(&c, &layout).unwrap().unwrap();
        assert!(handle.connected(), "{:?}", handle.problem());
        assert_eq!(handle.store_id.as_deref(), Some(id.as_str()));
        assert_eq!(handle.account.as_deref(), Some("me@example.com"));
        assert_eq!(
            store.root(),
            layout.cloud_dir().join(&id).join("library").as_path()
        );
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while handle.health() != Health::NeedsReauth {
            assert!(
                std::time::Instant::now() < until,
                "{:?}",
                handle.engine_state()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(handle.status_json()["health"]["state"], "needs-reauth");
        drop((store, _guard, handle));
        // Another pairing does not pick the release sign-in up.
        c.storage.library.google_project = GoogleProject::Beta;
        let (_s, _g, beta) = open_library_store(&c, &c.storage.layout())
            .unwrap()
            .unwrap();
        assert!(!beta.connected());
        assert!(
            beta.problem()
                .is_some_and(|p| p.contains("another pairing")),
            "{:?}",
            beta.problem()
        );
    }

    #[test]
    fn mega_without_a_session_is_unconnected() {
        let dir = tempfile::tempdir().unwrap();
        let c = cloud_config(dir.path(), LibraryBackend::Mega);
        let (_s, _g, h) = open_library_store(&c, &c.storage.layout())
            .unwrap()
            .unwrap();
        assert!(!h.connected());
        assert!(
            h.problem()
                .is_some_and(|p| p.contains("library connect mega")),
            "{:?}",
            h.problem()
        );
    }

    #[test]
    fn provider_errors_classify_for_clients() {
        use bunko_remote::{BErr, berr_to_io};
        use bunko_store::{ErrorClass, classify};
        bunko_store::register_classifier(classify_berr);
        assert_eq!(
            classify(&berr_to_io(BErr::AuthRevoked)),
            ErrorClass::NeedsAuth
        );
        assert_eq!(
            classify(&berr_to_io(BErr::Throttled(Some(Duration::from_secs(9))))),
            ErrorClass::Unavailable {
                retry_after: Some(Duration::from_secs(9))
            }
        );
        assert_eq!(
            classify(&berr_to_io(BErr::PreconditionFailed)),
            ErrorClass::Conflict
        );
        assert_eq!(
            classify(&berr_to_io(BErr::StorageFull)),
            ErrorClass::StorageFull
        );
    }

    #[test]
    fn a_local_library_opens_no_store() {
        let dir = tempfile::tempdir().unwrap();
        let c = cloud_config(dir.path(), LibraryBackend::Local);
        assert!(
            open_library_store(&c, &c.storage.layout())
                .unwrap()
                .is_none()
        );
    }
}
