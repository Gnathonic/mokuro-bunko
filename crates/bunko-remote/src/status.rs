//! Cloud status files (spec §9): the marker and lease `mokuro-reader/.mokuro-bunko.json`
//! (§9.2, D9) and the pure transform behind the cloud copy of
//! `mokuro-reader/.mokuro-queue.json` (§9.1, D10).
//!
//! **Lease.** Every Bunko that manages a cloud library writes a [`Marker`]. The holder
//! is the top of the total order `(epoch desc, claimed_at asc, instance_id asc)` over
//! every *fresh* marker node: `state == "active"` and `server_now − node.modified <
//! ttl_s`, both read from the provider's clock (a [`Backend::find_named`] listing
//! returns its [`ServerTime`]; the node's `base_mtime_ms` is the provider's). Local
//! clocks only break ties, and every Bunko computes the same order from the same set.
//! [`Lease`] runs CLAIM_LEASE ([`Lease::claim`]), the heartbeat ([`Lease::heartbeat`]),
//! take over and release, and answers the outbox's fence ([`Lease::fence_ok`]).
//! Marker writes are direct backend calls, never outbox ops: CLAIM_LEASE must write
//! before any lease exists. [`Lease::spawn`] runs it all as a background task that
//! reports [`LeaseEvent`]s through a watch channel.
//!
//! The lease's own state (epoch, state, `lease_ok_at`, its marker node, the boot ids
//! its markers may carry, its data folder) persists through a [`LeaseStore`]:
//! [`MemoryLeaseStore`] for tests, [`SqliteLeaseStore`] over the `meta` table of a
//! caller's connection (`state.sqlite`, D7).
//!
//! **Queue document.** [`cloud_queue_doc`] turns the server's `build_document` output
//! into the cloud copy (instance id, `truncated`, `done[]`, progress quantised to 0.1,
//! 64 KiB cap); [`queue_doc_changed`] is the "write only on change" rule.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::backend::{
    BErr, Backend, OpCtx, Pid, RNode, ServerTime, UploadSource, UploadTarget, read_to_vec,
};

// ---- the marker ---------------------------------------------------------------

/// The marker's file name, in the merged `mokuro-reader` root.
pub const MARKER_NAME: &str = ".mokuro-bunko.json";
/// `kind` of every Bunko marker.
pub const MARKER_KIND: &str = "mokuro-bunko";
/// The marker format's `version`.
pub const MARKER_VERSION: u32 = 1;
/// The default lease TTL (`ttl_s` 900).
pub const DEFAULT_TTL: Duration = Duration::from_secs(900);
/// The TTL on MEGA (`ttl_s` 3600); the caller picks it for that provider.
pub const MEGA_TTL: Duration = Duration::from_secs(3600);
/// The shortest CLAIM_LEASE settle wait (`max(60 s, 2 × feed interval)`).
pub const CLAIM_SETTLE_MIN: Duration = Duration::from_secs(60);
/// A same-named node larger than this is not read: it is not a marker.
const MARKER_MAX_BYTES: u64 = 64 * 1024;
/// Evaluate-and-write rounds before CLAIM_LEASE gives up for now.
const CLAIM_ATTEMPTS: usize = 4;
/// Re-listings when a node changes between the listing and its read.
const LIST_ATTEMPTS: usize = 3;
/// How many of this install's recent `boot_id`s a [`LeaseRecord`] remembers.
pub const BOOT_HISTORY: usize = 8;

/// A marker's `state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkerState {
    /// The writer manages (or is claiming) the library.
    Active,
    /// "Disconnect library" ran: the writer gave the library up. Never fresh.
    Released,
    /// A state this version does not know (a newer Bunko). Never fresh.
    #[serde(other)]
    Unknown,
}

fn default_marker_version() -> u32 {
    MARKER_VERSION
}

fn default_ttl_s() -> u64 {
    DEFAULT_TTL.as_secs()
}

fn default_active() -> MarkerState {
    MarkerState::Active
}

/// The content of `.mokuro-bunko.json` (spec §9.2), field for field:
///
/// ```json
/// {"version":1,"kind":"mokuro-bunko","instance_id":"6f1c…","boot_id":"a93e…","instance_name":"nas",
///  "bunko_version":"0.8.0","epoch":3,"claimed_at":"…","heartbeat_at":"…","ttl_s":900,"state":"active",
///  "compiles":["series.json","catalog.json"],"ocr":true,"queue":".mokuro-queue.json","took_over_from":null}
/// ```
///
/// Reading is lenient (a newer Bunko's marker must still be respected): only `kind`,
/// `instance_id` and `epoch` are required, unknown fields are ignored, a missing
/// `state` reads as `active` and a missing `ttl_s` as 900.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    #[serde(default = "default_marker_version")]
    pub version: u32,
    /// Always [`MARKER_KIND`].
    pub kind: String,
    /// The Bunko install (its `state.sqlite`).
    pub instance_id: String,
    /// This run of that install; a restart gets a new one.
    #[serde(default)]
    pub boot_id: String,
    /// Shown in "Another Bunko (nas) manages this library".
    #[serde(default)]
    pub instance_name: String,
    #[serde(default)]
    pub bunko_version: String,
    /// Bumped by every claim (`max(seen) + 1`) and take over (`holder + 1`).
    pub epoch: u64,
    /// The writer's clock when it claimed (ISO-8601 UTC); a tie-breaker only.
    #[serde(default)]
    pub claimed_at: String,
    /// The writer's clock at its last heartbeat (informational; freshness uses the
    /// provider's node timestamp).
    #[serde(default)]
    pub heartbeat_at: String,
    #[serde(default = "default_ttl_s")]
    pub ttl_s: u64,
    #[serde(default = "default_active")]
    pub state: MarkerState,
    /// The compiled metadata files the holder writes (§9.3).
    #[serde(default)]
    pub compiles: Vec<String>,
    /// Whether the holder runs OCR.
    #[serde(default)]
    pub ocr: bool,
    /// The queue file the holder publishes (§9.1), if any.
    #[serde(default)]
    pub queue: Option<String>,
    /// The `instance_id` this marker took the lease over from.
    #[serde(default)]
    pub took_over_from: Option<String>,
}

impl Marker {
    /// Parse a marker file; `None` when it is not a Bunko marker.
    pub fn parse(bytes: &[u8]) -> Option<Marker> {
        serde_json::from_slice::<Marker>(bytes)
            .ok()
            .filter(|m| m.kind == MARKER_KIND)
    }

    /// `claimed_at` in unix ms; an unreadable value ranks last among equal epochs.
    pub fn claimed_at_ms(&self) -> Option<i64> {
        parse_iso_ms(&self.claimed_at)
    }

    /// The marker's TTL.
    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_s)
    }
}

/// The lease order: [`Ordering::Less`] when `a` ranks above `b`. Higher `epoch` first,
/// then earlier `claimed_at`, then smaller `instance_id` (then `boot_id`, so two runs
/// of one install still order totally).
pub fn rank(a: &Marker, b: &Marker) -> Ordering {
    let claimed = |m: &Marker| m.claimed_at_ms().unwrap_or(i64::MAX);
    b.epoch
        .cmp(&a.epoch)
        .then_with(|| claimed(a).cmp(&claimed(b)))
        .then_with(|| a.instance_id.cmp(&b.instance_id))
        .then_with(|| a.boot_id.cmp(&b.boot_id))
}

/// One provider node named [`MARKER_NAME`] and the marker it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerNode {
    pub node: RNode,
    pub marker: Marker,
}

impl MarkerNode {
    /// Fresh (Bunko side): `state == "active"` and `server_now − node.modified <
    /// ttl_s`, both on the provider's clock.
    pub fn is_fresh(&self, server_now: ServerTime) -> bool {
        let ttl_ms = i64::try_from(self.marker.ttl_s.saturating_mul(1000)).unwrap_or(i64::MAX);
        self.marker.state == MarkerState::Active
            && server_now.0.saturating_sub(self.node.base_mtime_ms) < ttl_ms
    }
}

/// Every node named [`MARKER_NAME`] in the root (duplicates included), as one direct
/// listing saw them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerSet {
    /// The nodes that hold a Bunko marker.
    pub nodes: Vec<MarkerNode>,
    /// Same-named files that are not markers (unreadable, foreign content).
    pub others: Vec<RNode>,
    /// The provider's clock at the listing.
    pub server_now: ServerTime,
}

impl MarkerSet {
    /// The fresh markers, highest-ranked first.
    pub fn fresh(&self) -> Vec<&MarkerNode> {
        let mut out: Vec<&MarkerNode> = self
            .nodes
            .iter()
            .filter(|n| n.is_fresh(self.server_now))
            .collect();
        out.sort_by(|a, b| rank(&a.marker, &b.marker).then_with(|| a.node.pid.cmp(&b.node.pid)));
        out
    }

    /// The holder: the top-ranked fresh marker.
    pub fn holder(&self) -> Option<&MarkerNode> {
        self.fresh().into_iter().next()
    }

    /// The highest epoch of any marker seen, fresh or not.
    pub fn max_epoch(&self) -> u64 {
        self.nodes.iter().map(|n| n.marker.epoch).max().unwrap_or(0)
    }
}

// ---- configuration, clocks, persisted state ----------------------------------

/// What this Bunko writes into its marker, and the lease timings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseConfig {
    pub instance_id: String,
    /// This run's id (new on every start).
    pub boot_id: String,
    /// A previous run's `boot_id`; a fresh marker with our `instance_id` and this
    /// boot id is our own restart, and resumes at once. Optional: the boot ids in the
    /// loaded [`LeaseRecord::boot_ids`] count the same way.
    pub last_boot_id: Option<String>,
    /// This install's data folder (the store's directory). A loaded record written in
    /// another folder (a copied `state.sqlite`) is not ours: its boot ids are dropped,
    /// so the marker it describes reads as another Bunko's. `None`: not checked.
    pub home: Option<String>,
    /// Overrides [`LeaseConfig::settle`] (tests).
    pub claim_settle: Option<Duration>,
    pub instance_name: String,
    pub bunko_version: String,
    /// `ttl_s`: [`DEFAULT_TTL`], [`MEGA_TTL`] on MEGA.
    pub ttl: Duration,
    /// The change feed's poll interval; CLAIM_LEASE waits `max(60 s, 2 × this)`.
    pub feed_interval: Duration,
    /// The marker's `compiles`.
    pub compiles: Vec<String>,
    /// The marker's `ocr`.
    pub ocr: bool,
    /// The marker's `queue`.
    pub queue: Option<String>,
    /// A heartbeat writes a new node and then permanently deletes the previous own
    /// node (MEGA, where an update is a new node), instead of replacing in place.
    pub heartbeat_new_node: bool,
}

impl LeaseConfig {
    /// The defaults: TTL 900 s, feed interval 300 s, compiles `series.json` and
    /// `catalog.json`, OCR on, queue `.mokuro-queue.json`, heartbeats replace in place.
    pub fn new(
        instance_id: impl Into<String>,
        boot_id: impl Into<String>,
        instance_name: impl Into<String>,
        bunko_version: impl Into<String>,
    ) -> LeaseConfig {
        LeaseConfig {
            instance_id: instance_id.into(),
            boot_id: boot_id.into(),
            last_boot_id: None,
            home: None,
            claim_settle: None,
            instance_name: instance_name.into(),
            bunko_version: bunko_version.into(),
            ttl: DEFAULT_TTL,
            feed_interval: Duration::from_secs(300),
            compiles: vec!["series.json".into(), "catalog.json".into()],
            ocr: true,
            queue: Some(".mokuro-queue.json".into()),
            heartbeat_new_node: false,
        }
    }

    /// How long CLAIM_LEASE waits before re-listing: `max(60 s, 2 × feed interval)`.
    pub fn settle(&self) -> Duration {
        self.claim_settle
            .unwrap_or_else(|| CLAIM_SETTLE_MIN.max(self.feed_interval.saturating_mul(2)))
    }

    /// The heartbeat period, `ttl / 3`.
    pub fn heartbeat_interval(&self) -> Duration {
        self.ttl / 3
    }

    /// How long a heartbeat keeps the outbox fence open, `ttl / 2`.
    pub fn fence_window(&self) -> Duration {
        self.ttl / 2
    }
}

/// The local wall clock (unix ms) written into `claimed_at` / `heartbeat_at` and used
/// for `lease_ok_at`. Never used for freshness.
pub trait WallClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// [`WallClock`] on [`SystemTime`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl WallClock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
    }
}

/// The lease's persisted state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LeaseRecordState {
    /// Never claimed.
    #[default]
    None,
    Holder,
    Standby,
    /// A higher-ranked marker appeared while we held the lease.
    Lost,
    /// We wrote `state: "released"`.
    Released,
}

impl LeaseRecordState {
    /// The stable name stored by [`SqliteLeaseStore`].
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseRecordState::None => "none",
            LeaseRecordState::Holder => "holder",
            LeaseRecordState::Standby => "standby",
            LeaseRecordState::Lost => "lost",
            LeaseRecordState::Released => "released",
        }
    }

    /// The inverse of [`LeaseRecordState::as_str`] (unknown names read as `None`).
    pub fn parse(s: &str) -> LeaseRecordState {
        match s {
            "holder" => LeaseRecordState::Holder,
            "standby" => LeaseRecordState::Standby,
            "lost" => LeaseRecordState::Lost,
            "released" => LeaseRecordState::Released,
            _ => LeaseRecordState::None,
        }
    }
}

/// What a [`LeaseStore`] persists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaseRecord {
    /// The epoch of our last marker.
    pub lease_epoch: u64,
    pub lease_state: LeaseRecordState,
    /// Local unix ms when the lease was last confirmed (claim or heartbeat).
    pub lease_ok_at: Option<i64>,
    /// Our marker node.
    pub marker_pid: Option<Pid>,
    /// The `boot_id`s this install's markers may carry, newest first (at most
    /// [`BOOT_HISTORY`]). A run records its own *before* its first marker write, so
    /// whenever a crash, kill or clean stop happens, the marker on the provider
    /// carries one of these and the next run resumes it at once (§9.2 (a)).
    pub boot_ids: Vec<String>,
    /// The data folder that wrote this record ([`LeaseConfig::home`]).
    pub home: Option<String>,
}

impl LeaseRecord {
    /// The newest recorded `boot_id`.
    pub fn boot_id(&self) -> Option<&str> {
        self.boot_ids.first().map(String::as_str)
    }

    /// Record `boot` as the newest boot id.
    fn remember_boot(&mut self, boot: &str) {
        if self.boot_id() == Some(boot) {
            return;
        }
        self.boot_ids.retain(|b| b != boot);
        self.boot_ids.insert(0, boot.to_string());
        self.boot_ids.truncate(BOOT_HISTORY);
    }
}

/// A wait as people say it: "40 s", "3 min", "1 h 5 min".
pub fn about(d: Duration) -> String {
    let s = d.as_secs().max(1);
    match s {
        0..60 => format!("{s} s"),
        60..3600 => format!("{} min", s.div_ceil(60)),
        _ => {
            let m = s.div_ceil(60);
            match m % 60 {
                0 => format!("{} h", m / 60),
                r => format!("{} h {r} min", m / 60),
            }
        }
    }
}

/// Do two [`LeaseConfig::home`] strings name the same folder? Case-insensitively:
/// library hosts may be case-insensitive (NTFS, APFS).
fn same_home(a: &str, b: &str) -> bool {
    a == b || a.to_lowercase() == b.to_lowercase()
}

/// Is `lease_ok_at` within `window` of `now_ms` (either side, so a small clock step
/// back does not shut the fence) while we hold the lease?
pub fn fence_open(rec: &LeaseRecord, window: Duration, now_ms: i64) -> bool {
    let window_ms = i64::try_from(window.as_millis()).unwrap_or(i64::MAX);
    rec.lease_state == LeaseRecordState::Holder
        && rec
            .lease_ok_at
            .is_some_and(|ok| now_ms.saturating_sub(ok).saturating_abs() < window_ms)
}

/// Where the lease keeps its state between runs (and where marker writes are
/// ledgered, so their echoes in the feed are recognised).
pub trait LeaseStore: Send + Sync {
    fn load(&self) -> Option<LeaseRecord>;
    fn save(&self, rec: &LeaseRecord) -> Result<(), LeaseError>;
    /// A marker node this lease wrote (claim, heartbeat, take over, release).
    fn marker_written(&self, _node: &RNode) {}
    /// A marker node this lease permanently deleted.
    fn marker_deleted(&self, _pid: &str) {}
}

/// A [`LeaseStore`] in memory (tests; share one `Arc` to model a restart).
#[derive(Debug, Default)]
pub struct MemoryLeaseStore {
    rec: Mutex<Option<LeaseRecord>>,
}

impl MemoryLeaseStore {
    pub fn new() -> MemoryLeaseStore {
        MemoryLeaseStore::default()
    }
}

impl LeaseStore for MemoryLeaseStore {
    fn load(&self) -> Option<LeaseRecord> {
        self.rec.lock().clone()
    }

    fn save(&self, rec: &LeaseRecord) -> Result<(), LeaseError> {
        *self.rec.lock() = Some(rec.clone());
        Ok(())
    }
}

/// A [`LeaseStore`] in the `meta(k, v)` table of a caller's SQLite connection
/// (`state.sqlite`, D7). Keys: `lease_epoch`, `lease_state`, `lease_ok_at`,
/// `lease_marker_pid`, `lease_boot_id` (the boot ids, comma-separated, newest first),
/// `lease_home`; a `None` (or empty) field has no row.
pub struct SqliteLeaseStore {
    conn: Arc<Mutex<Connection>>,
}

const META_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);";

impl SqliteLeaseStore {
    /// Use `conn`, creating the `meta` table when it is missing.
    pub fn new(conn: Arc<Mutex<Connection>>) -> rusqlite::Result<SqliteLeaseStore> {
        conn.lock().execute_batch(META_SCHEMA)?;
        Ok(SqliteLeaseStore { conn })
    }

    fn read(conn: &Connection) -> rusqlite::Result<Option<LeaseRecord>> {
        let get = |k: &str| -> rusqlite::Result<Option<String>> {
            conn.query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
                .optional()
        };
        let Some(state) = get("lease_state")? else {
            return Ok(None);
        };
        Ok(Some(LeaseRecord {
            lease_epoch: get("lease_epoch")?
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            lease_state: LeaseRecordState::parse(&state),
            lease_ok_at: get("lease_ok_at")?.and_then(|v| v.parse().ok()),
            marker_pid: get("lease_marker_pid")?,
            boot_ids: get("lease_boot_id")?
                .map(|v| {
                    v.split(',')
                        .filter(|b| !b.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            home: get("lease_home")?,
        }))
    }

    fn write(conn: &mut Connection, rec: &LeaseRecord) -> rusqlite::Result<()> {
        let sp = conn.savepoint()?;
        let boot_ids = (!rec.boot_ids.is_empty()).then(|| rec.boot_ids.join(","));
        let fields: [(&str, Option<String>); 6] = [
            ("lease_epoch", Some(rec.lease_epoch.to_string())),
            ("lease_state", Some(rec.lease_state.as_str().to_string())),
            ("lease_ok_at", rec.lease_ok_at.map(|v| v.to_string())),
            ("lease_marker_pid", rec.marker_pid.clone()),
            ("lease_boot_id", boot_ids),
            ("lease_home", rec.home.clone()),
        ];
        for (k, v) in fields {
            match v {
                Some(v) => sp.execute(
                    "INSERT INTO meta(k, v) VALUES(?1, ?2) \
                     ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                    params![k, v],
                )?,
                None => sp.execute("DELETE FROM meta WHERE k = ?1", [k])?,
            };
        }
        sp.commit()
    }
}

impl LeaseStore for SqliteLeaseStore {
    fn load(&self) -> Option<LeaseRecord> {
        match Self::read(&self.conn.lock()) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "lease: reading the lease state failed");
                None
            }
        }
    }

    fn save(&self, rec: &LeaseRecord) -> Result<(), LeaseError> {
        Self::write(&mut self.conn.lock(), rec).map_err(|e| LeaseError::Store(e.to_string()))
    }
}

// ---- outcomes, events, errors -------------------------------------------------

/// A lease failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    #[error("provider: {0}")]
    Backend(#[from] BErr),
    #[error("lease state store: {0}")]
    Store(String),
    #[error("marker staging file: {0}")]
    Staging(String),
    /// The marker set kept changing under every write; try again later.
    #[error("the marker set did not settle after {0} attempts")]
    Unsettled(usize),
    /// A heartbeat without the lease.
    #[error("not the lease holder")]
    NotHolder,
    /// The background lease task is gone.
    #[error("the lease task has stopped")]
    Stopped,
}

/// What [`Lease::claim`] and [`Lease::take_over`] end in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseState {
    /// We hold the lease; the fence is open.
    Holder,
    /// Another Bunko holds it (`LeaseHeld`): write nothing, keep Deferred ops HELD.
    Standby { holder: Box<Marker> },
}

/// What [`Lease::heartbeat`] ends in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heartbeat {
    /// Our marker is current and the fence is renewed.
    Held,
    /// A marker ranks above ours: stop scheduling, finish in-flight uploads, start
    /// nothing new, alert.
    Lost { holder: Box<Marker> },
}

/// What the background task reports (the latest value of a watch channel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseEvent {
    /// Started, nothing decided yet.
    Claiming,
    /// We hold the lease.
    Acquired,
    /// Another Bunko holds it.
    Standby { holder: Box<Marker> },
    /// We held it and a higher-ranked marker appeared.
    Lost { holder: Box<Marker> },
    /// A claim or heartbeat failed; it is retried. The fence lapses `ttl / 2` after
    /// the last good heartbeat.
    HeartbeatFailed { error: String },
    /// We wrote `state: "released"`; the task only waits for shutdown now.
    Released,
}

// ---- the lease ------------------------------------------------------------------

/// How a marker write may pick its node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    /// Own node in place, else a new node.
    Claim,
    /// As `Claim`, but with [`LeaseConfig::heartbeat_new_node`] honoured.
    Heartbeat,
    /// Only our own node; nothing to write when it is gone.
    Release,
}

enum Target {
    New,
    Replace { pid: Pid, if_cver: Option<String> },
}

/// The marker lease for one store (spec §9.2). Drive it with [`Lease::claim`] and
/// [`Lease::heartbeat`], or [`Lease::spawn`] it as a background task.
pub struct Lease<B: Backend> {
    backend: Arc<B>,
    root: Pid,
    cfg: LeaseConfig,
    cas: bool,
    store: Arc<dyn LeaseStore>,
    scratch: PathBuf,
    clock: Arc<dyn WallClock>,
    record: Arc<Mutex<LeaseRecord>>,
    /// Our marker as last written, while we hold the lease.
    mine: Option<Marker>,
    /// Parsed marker contents by `(pid, cver)`; `None` = not a marker.
    cache: HashMap<Pid, (String, Option<Marker>)>,
    /// When CLAIM_LEASE's settle wait ends, while it runs (shared with the [`Fence`],
    /// so a refused write can say how long this Bunko is still claiming).
    settle_until: Arc<Mutex<Option<Instant>>>,
}

static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

impl<B: Backend> Lease<B> {
    /// A lease on the marker in `root` (the merged root's first member). Marker bytes
    /// are staged under `scratch` before upload. The state is loaded from `store`.
    pub fn new(
        backend: Arc<B>,
        root: impl Into<Pid>,
        cfg: LeaseConfig,
        store: Arc<dyn LeaseStore>,
        scratch: impl Into<PathBuf>,
    ) -> Lease<B> {
        let cas = backend.caps().cas_overwrite;
        let mut rec = store.load().unwrap_or_default();
        if let (Some(home), Some(was)) = (&cfg.home, &rec.home)
            && !same_home(home, was)
        {
            tracing::warn!(
                was = %was,
                now = %home,
                "lease: this lease state was written in another data folder (a copied \
                 data folder?); a fresh marker it wrote counts as another Bunko's (if the \
                 folder was moved, the old marker expires after its ttl)"
            );
            rec.boot_ids.clear();
            rec.lease_state = LeaseRecordState::None;
            rec.lease_ok_at = None;
            rec.marker_pid = None;
        }
        if cfg.home.is_some() {
            rec.home = cfg.home.clone();
        }
        Lease {
            backend,
            root: root.into(),
            cfg,
            cas,
            store,
            scratch: scratch.into(),
            clock: Arc::new(SystemClock),
            record: Arc::new(Mutex::new(rec)),
            mine: None,
            cache: HashMap::new(),
            settle_until: Arc::new(Mutex::new(None)),
        }
    }

    /// Use `clock` for `claimed_at`, `heartbeat_at` and `lease_ok_at`.
    pub fn with_clock(mut self, clock: Arc<dyn WallClock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn config(&self) -> &LeaseConfig {
        &self.cfg
    }

    /// The persisted state as it stands.
    pub fn record(&self) -> LeaseRecord {
        self.record.lock().clone()
    }

    /// Our marker, while we hold the lease.
    pub fn marker(&self) -> Option<&Marker> {
        self.mine.as_ref()
    }

    /// The outbox fence: an upload may start only while we hold the lease and the
    /// last confirmation is within `ttl / 2` of `now_ms` (local unix ms).
    pub fn fence_ok(&self, now_ms: i64) -> bool {
        fence_open(&self.record.lock(), self.cfg.fence_window(), now_ms)
    }

    /// A cloneable view of the fence for the outbox worker.
    pub fn fence(&self) -> Fence {
        Fence {
            record: self.record.clone(),
            window: self.cfg.fence_window(),
            clock: self.clock.clone(),
            settle_until: self.settle_until.clone(),
        }
    }

    /// The marker set by a direct `find_named` listing.
    pub async fn marker_set(&mut self) -> Result<MarkerSet, LeaseError> {
        'list: for _ in 0..LIST_ATTEMPTS {
            let (nodes, server_now) = self.backend.find_named(&self.root, MARKER_NAME).await?;
            let mut set = MarkerSet {
                nodes: Vec::new(),
                others: Vec::new(),
                server_now,
            };
            let mut seen: HashMap<Pid, (String, Option<Marker>)> = HashMap::new();
            for n in nodes {
                if n.is_dir || n.trashed {
                    continue;
                }
                let parsed = match self.cache.get(&n.pid) {
                    Some((cver, m)) if *cver == n.cver => m.clone(),
                    _ => match self.fetch(&n).await? {
                        Some(m) => m,
                        // Changed or gone since the listing: list again.
                        None => continue 'list,
                    },
                };
                seen.insert(n.pid.clone(), (n.cver.clone(), parsed.clone()));
                match parsed {
                    Some(marker) => set.nodes.push(MarkerNode { node: n, marker }),
                    None => set.others.push(n),
                }
            }
            self.cache = seen;
            return Ok(set);
        }
        Err(LeaseError::Unsettled(LIST_ATTEMPTS))
    }

    /// Read one listed node: `Some(None)` = not a marker, `None` = changed or gone.
    async fn fetch(&self, n: &RNode) -> Result<Option<Option<Marker>>, LeaseError> {
        if n.size > MARKER_MAX_BYTES {
            return Ok(Some(None));
        }
        let body = match self.backend.read(&n.pid, n.pin.as_deref(), None).await {
            Ok(b) => b,
            Err(BErr::NotFound | BErr::PreconditionFailed) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let bytes = read_to_vec(body)
            .await
            .map_err(|e| LeaseError::Backend(BErr::Transient(e.to_string())))?;
        Ok(Some(Marker::parse(&bytes)))
    }

    /// Our current run's marker.
    fn is_mine(&self, m: &Marker) -> bool {
        m.instance_id == self.cfg.instance_id && m.boot_id == self.cfg.boot_id
    }

    /// Ours from this run or an earlier one of this install (a recorded boot id).
    fn is_ours(&self, m: &Marker) -> bool {
        m.instance_id == self.cfg.instance_id
            && (m.boot_id == self.cfg.boot_id
                || self.cfg.last_boot_id.as_deref() == Some(m.boot_id.as_str())
                || self.record.lock().boot_ids.contains(&m.boot_id))
    }

    /// Another Bunko with our `instance_id` (a cloned data folder): a boot id this
    /// install never recorded.
    fn is_twin(&self, m: &Marker) -> bool {
        m.instance_id == self.cfg.instance_id && !self.is_ours(m)
    }

    fn now_iso(&self) -> String {
        format_iso_ms(self.clock.now_ms())
    }

    fn new_marker(&self, epoch: u64, took_over_from: Option<String>) -> Marker {
        let now = self.now_iso();
        Marker {
            version: MARKER_VERSION,
            kind: MARKER_KIND.into(),
            instance_id: self.cfg.instance_id.clone(),
            boot_id: self.cfg.boot_id.clone(),
            instance_name: self.cfg.instance_name.clone(),
            bunko_version: self.cfg.bunko_version.clone(),
            epoch,
            claimed_at: now.clone(),
            heartbeat_at: now,
            ttl_s: self.cfg.ttl.as_secs(),
            state: MarkerState::Active,
            compiles: self.cfg.compiles.clone(),
            ocr: self.cfg.ocr,
            queue: self.cfg.queue.clone(),
            took_over_from,
        }
    }

    /// CLAIM_LEASE (§9.2 step 1).
    ///
    /// - The top fresh marker is ours (our `instance_id`, and our `last_boot_id` or
    ///   this run's `boot_id`): resume at once, rewriting it with the new `boot_id`.
    /// - It is anyone else's — including our `instance_id` with an unknown `boot_id`,
    ///   a cloned data folder, which is logged: standby.
    /// - None fresh: write ours with `epoch = max(seen) + 1` (our own node replaced in
    ///   place, else a new node), wait [`LeaseConfig::settle`], re-list, and hold the
    ///   lease only if the top fresh marker is ours with our `boot_id`. A loser
    ///   permanently deletes its own node.
    pub async fn claim(&mut self) -> Result<LeaseState, LeaseError> {
        for _ in 0..CLAIM_ATTEMPTS {
            let set = self.marker_set().await?;
            if let Some(top) = set.holder() {
                if self.is_ours(&top.marker) {
                    let mut m = top.marker.clone();
                    m.boot_id = self.cfg.boot_id.clone();
                    m.instance_name = self.cfg.instance_name.clone();
                    m.bunko_version = self.cfg.bunko_version.clone();
                    m.heartbeat_at = self.now_iso();
                    m.ttl_s = self.cfg.ttl.as_secs();
                    m.compiles = self.cfg.compiles.clone();
                    m.ocr = self.cfg.ocr;
                    m.queue = self.cfg.queue.clone();
                    let ok_at = self.clock.now_ms();
                    let Some(node) = self.write_marker(&m, &set, WriteMode::Claim).await? else {
                        continue;
                    };
                    tracing::info!(epoch = m.epoch, "lease: resumed our own fresh marker");
                    self.become_holder(m, &node, ok_at)?;
                    return Ok(LeaseState::Holder);
                }
                let holder = top.marker.clone();
                if self.is_twin(&holder) {
                    tracing::warn!(
                        boot_id = %holder.boot_id,
                        "lease: a fresh marker carries this install's instance id but a \
                         boot id this install never recorded; treating it as another Bunko \
                         (a cloned data folder?)"
                    );
                }
                self.become_standby(&set, false).await?;
                return Ok(LeaseState::Standby {
                    holder: Box::new(holder),
                });
            }
            let epoch = set.max_epoch().max(self.record.lock().lease_epoch) + 1;
            let m = self.new_marker(epoch, None);
            if self
                .write_marker(&m, &set, WriteMode::Claim)
                .await?
                .is_none()
            {
                continue;
            }
            let settle = self.cfg.settle();
            *self.settle_until.lock() = Some(Instant::now() + settle);
            tracing::info!(
                epoch,
                settle_s = settle.as_secs(),
                "lease: marker written; waiting for the marker set to settle before \
                 holding the lease"
            );
            tokio::time::sleep(settle).await;
            *self.settle_until.lock() = None;
            let ok_at = self.clock.now_ms();
            let after = self.marker_set().await?;
            match after.holder() {
                Some(top) if self.is_mine(&top.marker) => {
                    let (marker, node) = (top.marker.clone(), top.node.clone());
                    tracing::info!(epoch, "lease: claimed");
                    self.become_holder(marker, &node, ok_at)?;
                    return Ok(LeaseState::Holder);
                }
                Some(top) => {
                    let holder = top.marker.clone();
                    tracing::info!(holder = %holder.instance_name, "lease: lost the claim");
                    self.become_standby(&after, true).await?;
                    return Ok(LeaseState::Standby {
                        holder: Box::new(holder),
                    });
                }
                // Our marker is not visible as fresh: drop what is ours and go again.
                None => self.delete_own(&after).await,
            }
        }
        Err(LeaseError::Unsettled(CLAIM_ATTEMPTS))
    }

    /// The heartbeat (§9.2 step 3): evaluate the marker set first; anything ranking
    /// above our marker → [`Heartbeat::Lost`]. Otherwise rewrite our node (on MEGA a
    /// new node, then the previous own node is deleted permanently) and renew
    /// `lease_ok_at`.
    pub async fn heartbeat(&mut self) -> Result<Heartbeat, LeaseError> {
        let Some(mine) = self.mine.clone() else {
            return Err(LeaseError::NotHolder);
        };
        let ok_at = self.clock.now_ms();
        for _ in 0..2 {
            let set = self.marker_set().await?;
            // A twin (our instance id, a boot id we never recorded) holding a fresh
            // marker is a cloned data folder that resumed our marker: it ranks level
            // with ours, so step down rather than have two holders.
            let above = set
                .fresh()
                .into_iter()
                .find(|n| {
                    !self.is_mine(&n.marker)
                        && (rank(&n.marker, &mine) == Ordering::Less || self.is_twin(&n.marker))
                })
                .map(|n| n.marker.clone());
            if let Some(holder) = above {
                tracing::warn!(
                    holder = %holder.instance_name,
                    epoch = holder.epoch,
                    "lease: another Bunko's marker ranks above ours; stepping down"
                );
                self.mine = None;
                self.update_record(|r| r.lease_state = LeaseRecordState::Lost)?;
                return Ok(Heartbeat::Lost {
                    holder: Box::new(holder),
                });
            }
            let mut next = mine.clone();
            next.heartbeat_at = self.now_iso();
            if let Some(node) = self.write_marker(&next, &set, WriteMode::Heartbeat).await? {
                self.become_holder(next, &node, ok_at)?;
                return Ok(Heartbeat::Held);
            }
        }
        Err(LeaseError::Unsettled(2))
    }

    /// Take over (§9.2 step 2): write `epoch = holder.epoch + 1`, `took_over_from =
    /// holder.instance_id`, which wins regardless of clocks. With no fresh holder (or
    /// our own) this is [`Lease::claim`].
    pub async fn take_over(&mut self) -> Result<LeaseState, LeaseError> {
        for _ in 0..CLAIM_ATTEMPTS {
            let set = self.marker_set().await?;
            let holder = match set.holder() {
                Some(top) if !self.is_ours(&top.marker) => top.marker.clone(),
                _ => return self.claim().await,
            };
            let m = self.new_marker(holder.epoch + 1, Some(holder.instance_id.clone()));
            let ok_at = self.clock.now_ms();
            if let Some(node) = self.write_marker(&m, &set, WriteMode::Claim).await? {
                tracing::info!(from = %holder.instance_name, epoch = m.epoch, "lease: took over");
                self.become_holder(m, &node, ok_at)?;
                return Ok(LeaseState::Holder);
            }
        }
        Err(LeaseError::Unsettled(CLAIM_ATTEMPTS))
    }

    /// "Disconnect library" (§7 step 2): write `state: "released"` into our marker.
    /// Without the lease there is nothing to release; the state still becomes
    /// `Released`.
    pub async fn release(&mut self) -> Result<(), LeaseError> {
        if let Some(mine) = self.mine.clone() {
            let set = self.marker_set().await?;
            let mut m = mine;
            m.state = MarkerState::Released;
            m.heartbeat_at = self.now_iso();
            let node = self.write_marker(&m, &set, WriteMode::Release).await?;
            if let Some(n) = &node {
                self.update_record(|r| r.marker_pid = Some(n.pid.clone()))?;
            }
        }
        self.mine = None;
        self.update_record(|r| r.lease_state = LeaseRecordState::Released)
    }

    fn update_record(&self, f: impl FnOnce(&mut LeaseRecord)) -> Result<(), LeaseError> {
        let rec = {
            let mut r = self.record.lock();
            f(&mut r);
            r.remember_boot(&self.cfg.boot_id);
            r.clone()
        };
        self.store.save(&rec)
    }

    /// Before writing a marker with `epoch` and this run's boot id: persist both, so
    /// the next run recognises the marker as its own however this run stops (during
    /// a claim's settle wait included).
    fn note_write(&self, epoch: u64) -> Result<(), LeaseError> {
        let recorded = {
            let r = self.record.lock();
            r.boot_id() == Some(self.cfg.boot_id.as_str()) && r.lease_epoch >= epoch
        };
        if recorded {
            return Ok(());
        }
        self.update_record(|r| r.lease_epoch = r.lease_epoch.max(epoch))
    }

    fn become_holder(&mut self, m: Marker, node: &RNode, ok_at: i64) -> Result<(), LeaseError> {
        let epoch = m.epoch;
        self.mine = Some(m);
        self.update_record(|r| {
            r.lease_epoch = epoch;
            r.lease_state = LeaseRecordState::Holder;
            r.lease_ok_at = Some(ok_at);
            r.marker_pid = Some(node.pid.clone());
        })
    }

    async fn become_standby(&mut self, set: &MarkerSet, delete: bool) -> Result<(), LeaseError> {
        if delete {
            self.delete_own(set).await;
        }
        self.mine = None;
        self.update_record(|r| r.lease_state = LeaseRecordState::Standby)
    }

    /// Permanently delete every node holding this run's marker (a lost claim).
    async fn delete_own(&mut self, set: &MarkerSet) {
        let own: Vec<Pid> = set
            .nodes
            .iter()
            .filter(|n| self.is_mine(&n.marker))
            .map(|n| n.node.pid.clone())
            .collect();
        for pid in own {
            self.delete_node(&pid).await;
        }
    }

    async fn delete_node(&mut self, pid: &str) {
        match self.backend.delete_permanent(pid).await {
            Ok(()) | Err(BErr::NotFound) => {
                self.cache.remove(pid);
                self.store.marker_deleted(pid);
                let mut r = self.record.lock();
                if r.marker_pid.as_deref() == Some(pid) {
                    r.marker_pid = None;
                }
            }
            Err(e) => tracing::warn!(pid, error = %e, "lease: deleting our marker node failed"),
        }
    }

    /// The node our marker lives in: one holding a marker of ours (this run or the
    /// previous one), the recorded node first, then this run's.
    fn own_node<'a>(&self, set: &'a MarkerSet) -> Option<&'a MarkerNode> {
        let recorded = self.record.lock().marker_pid.clone();
        let mut own: Vec<&MarkerNode> = set
            .nodes
            .iter()
            .filter(|n| self.is_ours(&n.marker))
            .collect();
        own.sort_by_key(|n| {
            (
                Some(&n.node.pid) != recorded.as_ref(),
                !self.is_mine(&n.marker),
                std::cmp::Reverse(n.node.base_mtime_ms),
            )
        });
        own.into_iter().next()
    }

    fn if_cver(&self, n: &RNode) -> Option<String> {
        self.cas.then(|| n.cver.clone())
    }

    /// Write `m`. `Ok(None)`: the node changed under us (CAS) or a higher-ranked
    /// marker took the name; the caller re-evaluates.
    async fn write_marker(
        &mut self,
        m: &Marker,
        set: &MarkerSet,
        mode: WriteMode,
    ) -> Result<Option<RNode>, LeaseError> {
        let own = self.own_node(set).map(|n| n.node.clone());
        let written = match (&own, mode) {
            (Some(o), WriteMode::Heartbeat) if self.cfg.heartbeat_new_node => {
                match self.put(m, Target::New).await {
                    Ok(n) => Some(n),
                    Err(LeaseError::Backend(BErr::NameConflict | BErr::PreconditionFailed)) => {
                        self.put_replace(m, o).await?
                    }
                    Err(e) => return Err(e),
                }
            }
            (Some(o), _) => self.put_replace(m, o).await?,
            (None, WriteMode::Release) => return Ok(None),
            (None, _) => match self.put(m, Target::New).await {
                Ok(n) => Some(n),
                // A unique-name provider already has a node by this name.
                Err(LeaseError::Backend(BErr::NameConflict | BErr::PreconditionFailed)) => {
                    self.replace_existing(m).await?
                }
                Err(e) => return Err(e),
            },
        };
        let Some(node) = written else {
            return Ok(None);
        };
        // Our other nodes (a MEGA update's previous node, a replace that moved to a
        // new pid, leftovers of the previous run) go permanently.
        let mut extra: Vec<Pid> = set
            .nodes
            .iter()
            .filter(|n| self.is_ours(&n.marker) && n.node.pid != node.pid)
            .map(|n| n.node.pid.clone())
            .collect();
        if let Some(o) = &own
            && o.pid != node.pid
            && !extra.contains(&o.pid)
        {
            extra.push(o.pid.clone());
        }
        for pid in extra {
            self.delete_node(&pid).await;
        }
        Ok(Some(node))
    }

    async fn put_replace(&mut self, m: &Marker, o: &RNode) -> Result<Option<RNode>, LeaseError> {
        let target = Target::Replace {
            pid: o.pid.clone(),
            if_cver: self.if_cver(o),
        };
        match self.put(m, target).await {
            Ok(n) => Ok(Some(n)),
            Err(LeaseError::Backend(BErr::PreconditionFailed | BErr::NotFound)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// On a provider with unique names, overwrite the existing node — unless it holds
    /// a fresh marker that ranks above `m`.
    async fn replace_existing(&mut self, m: &Marker) -> Result<Option<RNode>, LeaseError> {
        let set = self.marker_set().await?;
        let fresh_above = set
            .fresh()
            .iter()
            .any(|n| !self.is_mine(&n.marker) && rank(&n.marker, m) == Ordering::Less);
        if fresh_above {
            return Ok(None);
        }
        let target = set
            .others
            .first()
            .cloned()
            .or_else(|| set.nodes.first().map(|n| n.node.clone()));
        match target {
            Some(t) => self.put_replace(m, &t).await,
            None => Ok(None),
        }
    }

    /// Stage `m` in a scratch file and upload it.
    async fn put(&mut self, m: &Marker, target: Target) -> Result<RNode, LeaseError> {
        self.note_write(m.epoch)?;
        let bytes = serde_json::to_vec(m).map_err(|e| LeaseError::Staging(e.to_string()))?;
        let seq = STAGING_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
        let tag: String = self
            .cfg
            .boot_id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(16)
            .collect();
        let path = self.scratch.join(format!("marker-{tag}-{seq}.json.tmp"));
        std::fs::create_dir_all(&self.scratch)
            .and_then(|()| std::fs::write(&path, &bytes))
            .map_err(|e| LeaseError::Staging(format!("{}: {e}", path.display())))?;
        let src = UploadSource::File {
            path: &path,
            size: bytes.len() as u64,
            sha256: Sha256::digest(&bytes).into(),
        };
        let mut op = OpCtx {
            op_id: format!("lease-{tag}-{seq}"),
            resume: None,
        };
        let to = match &target {
            Target::New => UploadTarget::New {
                parent: &self.root,
                name: MARKER_NAME,
            },
            Target::Replace { pid, if_cver } => UploadTarget::Replace {
                pid,
                if_cver: if_cver.as_deref(),
            },
        };
        let res = self.backend.upload(to, src, &mut op).await;
        let _ = std::fs::remove_file(&path);
        let node = res?;
        self.cache
            .insert(node.pid.clone(), (node.cver.clone(), Some(m.clone())));
        self.store.marker_written(&node);
        Ok(node)
    }

    /// Run the lease as a background task on the current runtime.
    pub fn spawn(self) -> LeaseHandle {
        let (tx, rx) = watch::channel(LeaseEvent::Claiming);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let fence = self.fence();
        // Marker writes and heartbeats outrank OCR and background calls (§5.11).
        let join = tokio::spawn(crate::governor::scoped(
            crate::governor::ApiClass::Lease,
            self.run(cmd_rx, tx),
        ));
        LeaseHandle {
            events: rx,
            cmds: cmd_tx,
            fence,
            join: Some(join),
        }
    }

    /// The background loop: claim; while holding, heartbeat every `ttl / 3`; in
    /// standby (or after a loss) run CLAIM_LEASE again every `ttl / 3`, so a holder
    /// that goes away is replaced. Failures are retried after `ttl / 12` (or the
    /// provider's `Retry-After`).
    pub async fn run(
        mut self,
        mut cmds: mpsc::UnboundedReceiver<LeaseCommand>,
        events: watch::Sender<LeaseEvent>,
    ) {
        let emit = |e: LeaseEvent| {
            events.send_if_modified(|cur| {
                let changed = *cur != e;
                *cur = e;
                changed
            });
        };
        let every = self.cfg.heartbeat_interval();
        let ttl = self.cfg.ttl;
        let retry = |e: &LeaseError| match e {
            LeaseError::Backend(BErr::Throttled(Some(d)) | BErr::TransferQuota(Some(d))) => {
                (*d).max(Duration::from_secs(5))
            }
            _ => (ttl / 12).max(Duration::from_secs(1)),
        };
        let mut released = false;
        let mut deadline = Instant::now();
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline), if !released => {
                    let holding = self.mine.is_some();
                    deadline = Instant::now() + if holding {
                        match self.heartbeat().await {
                            Ok(Heartbeat::Held) => { emit(LeaseEvent::Acquired); every }
                            Ok(Heartbeat::Lost { holder }) => { emit(LeaseEvent::Lost { holder }); every }
                            Err(e) => {
                                let d = retry(&e);
                                emit(LeaseEvent::HeartbeatFailed { error: e.to_string() });
                                d
                            }
                        }
                    } else {
                        match self.claim().await {
                            Ok(LeaseState::Holder) => { emit(LeaseEvent::Acquired); every }
                            Ok(LeaseState::Standby { holder }) => { emit(LeaseEvent::Standby { holder }); every }
                            Err(e) => {
                                let d = retry(&e);
                                emit(LeaseEvent::HeartbeatFailed { error: e.to_string() });
                                d
                            }
                        }
                    };
                }
                cmd = cmds.recv() => match cmd {
                    Some(LeaseCommand::TakeOver(reply)) => {
                        let r = self.take_over().await;
                        match &r {
                            Ok(LeaseState::Holder) => { emit(LeaseEvent::Acquired); deadline = Instant::now() + every; }
                            Ok(LeaseState::Standby { holder }) => emit(LeaseEvent::Standby { holder: holder.clone() }),
                            Err(e) => emit(LeaseEvent::HeartbeatFailed { error: e.to_string() }),
                        }
                        let _ = reply.send(r);
                    }
                    Some(LeaseCommand::Release(reply)) => {
                        let r = self.release().await;
                        if r.is_ok() {
                            released = true;
                            emit(LeaseEvent::Released);
                        }
                        let _ = reply.send(r);
                    }
                    Some(LeaseCommand::Stop) | None => return,
                },
            }
        }
    }
}

/// A request to a running lease task.
#[derive(Debug)]
pub enum LeaseCommand {
    /// The admin's [Take over].
    TakeOver(oneshot::Sender<Result<LeaseState, LeaseError>>),
    /// "Disconnect library": write `released` and stop heartbeating.
    Release(oneshot::Sender<Result<(), LeaseError>>),
    /// Stop the task (a clean shutdown writes nothing).
    Stop,
}

/// The outbox fence of a [`Lease`] (cloneable; reads the lease's live state).
#[derive(Clone)]
pub struct Fence {
    record: Arc<Mutex<LeaseRecord>>,
    window: Duration,
    clock: Arc<dyn WallClock>,
    settle_until: Arc<Mutex<Option<Instant>>>,
}

impl Fence {
    /// The lease's persisted state as it stands (epoch, `lease_ok_at`, ...).
    pub fn record(&self) -> LeaseRecord {
        self.record.lock().clone()
    }

    /// [`Lease::fence_ok`] at `now_ms` (local unix ms).
    pub fn ok_at(&self, now_ms: i64) -> bool {
        fence_open(&self.record.lock(), self.window, now_ms)
    }

    /// [`Fence::ok_at`] on the lease's clock.
    pub fn ok(&self) -> bool {
        self.ok_at(self.clock.now_ms())
    }

    /// How much of CLAIM_LEASE's settle wait is left, while it runs (§9.2 step 1.2):
    /// this Bunko wrote its marker and waits before it may hold the lease.
    pub fn claiming_left(&self) -> Option<Duration> {
        let until = (*self.settle_until.lock())?;
        Some(until.saturating_duration_since(Instant::now()))
    }

    /// The lease's persisted state as it stands.
    pub fn state(&self) -> LeaseRecordState {
        self.record.lock().lease_state
    }
}

/// Control and observation of a running lease task.
pub struct LeaseHandle {
    events: watch::Receiver<LeaseEvent>,
    cmds: mpsc::UnboundedSender<LeaseCommand>,
    fence: Fence,
    join: Option<JoinHandle<()>>,
}

impl LeaseHandle {
    /// The latest event.
    pub fn event(&self) -> LeaseEvent {
        self.events.borrow().clone()
    }

    /// A receiver for every later event.
    pub fn subscribe(&self) -> watch::Receiver<LeaseEvent> {
        self.events.clone()
    }

    pub fn fence(&self) -> &Fence {
        &self.fence
    }

    /// What is left of this Bunko's first-claim settle wait, while it runs.
    pub fn claiming_left(&self) -> Option<Duration> {
        self.fence.claiming_left()
    }

    /// Run [`Lease::take_over`] in the task.
    pub async fn take_over(&self) -> Result<LeaseState, LeaseError> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(LeaseCommand::TakeOver(tx))
            .map_err(|_| LeaseError::Stopped)?;
        rx.await.map_err(|_| LeaseError::Stopped)?
    }

    /// Run [`Lease::release`] in the task.
    pub async fn release(&self) -> Result<(), LeaseError> {
        let (tx, rx) = oneshot::channel();
        self.cmds
            .send(LeaseCommand::Release(tx))
            .map_err(|_| LeaseError::Stopped)?;
        rx.await.map_err(|_| LeaseError::Stopped)?
    }

    /// Stop the task and wait for it. Writes nothing (§9.2 step 5).
    pub async fn shutdown(mut self) {
        let _ = self.cmds.send(LeaseCommand::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

impl Drop for LeaseHandle {
    fn drop(&mut self) {
        if let Some(j) = self.join.take() {
            j.abort();
        }
    }
}

// ---- the cloud queue document ---------------------------------------------------

/// The cloud queue file's size cap (§9.1).
pub const QUEUE_DOC_MAX_BYTES: usize = 64 * 1024;
/// Waiting volumes listed at most (`QUEUE_REPORT_LIMIT`).
pub const QUEUE_WAITING_LIMIT: usize = 100;
/// `done[]` holds at most this many completions …
pub const DONE_LIMIT: usize = 100;
/// … from at most this long ago.
pub const DONE_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// One layer file of a [`DoneEntry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneLayer {
    pub id: String,
    /// Relative to the root, e.g. `Kingdom/Kingdom 12.hayai.mokuro`.
    pub path: String,
}

/// A completed volume whose sidecar op COMMITTED (D10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoneEntry {
    pub series: String,
    pub volume: String,
    /// When it committed, unix ms.
    pub at_ms: i64,
    /// The main sidecar, relative to the root, e.g. `Kingdom/Kingdom 12.mokuro`.
    pub ocr: Option<String>,
    pub layers: Vec<DoneLayer>,
}

impl DoneEntry {
    fn to_json(&self) -> Value {
        json!({
            "series": self.series,
            "volume": self.volume,
            "at": format_iso_s(self.at_ms),
            "ocr": self.ocr,
            "layers": self.layers.iter().map(|l| json!({"id": l.id, "path": l.path})).collect::<Vec<_>>(),
        })
    }
}

fn is_running(volume: &Value) -> bool {
    volume["jobs"]
        .as_array()
        .is_some_and(|jobs| jobs.iter().any(|j| j["state"] == "running"))
}

fn quantise_progress(volume: &mut Value) {
    let Some(Value::Array(jobs)) = volume.get_mut("jobs") else {
        return;
    };
    for j in jobs {
        if let Some(p) = j.get("progress").and_then(Value::as_f64) {
            j["progress"] = json!((p * 10.0).round() / 10.0);
        }
    }
}

fn json_len(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(0, |b| b.len())
}

fn assemble(
    src: &Map<String, Value>,
    instance_id: &str,
    running: &[Value],
    waiting: &[Value],
    pending: usize,
    done: &[Value],
) -> Value {
    let truncated = waiting.len() < pending;
    let volumes = || Value::Array(running.iter().chain(waiting).cloned().collect());
    let mut out = Map::new();
    let (mut has_id, mut has_trunc, mut has_vols) = (false, false, false);
    for (k, v) in src {
        match k.as_str() {
            "instance_id" | "truncated" | "done" => continue,
            "volumes" => {
                out.insert(k.clone(), volumes());
                has_vols = true;
            }
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
        if k == "generated_at" {
            out.insert("instance_id".into(), json!(instance_id));
            has_id = true;
        }
        if k == "pending_volumes" {
            out.insert("truncated".into(), json!(truncated));
            has_trunc = true;
        }
    }
    if !has_id {
        out.insert("instance_id".into(), json!(instance_id));
    }
    if !has_trunc {
        out.insert("truncated".into(), json!(truncated));
    }
    if !has_vols {
        out.insert("volumes".into(), volumes());
    }
    out.insert("done".into(), Value::Array(done.to_vec()));
    Value::Object(out)
}

/// The cloud copy of the queue file (§9.1, D10) from the server's `build_document`
/// output: adds `instance_id`, `truncated` and `done[]`, quantises every job's
/// `progress` to 0.1, and keeps the compact JSON within `max_bytes`
/// ([`QUEUE_DOC_MAX_BYTES`]).
///
/// - `volumes`: every running volume plus the first [`QUEUE_WAITING_LIMIT`] waiting
///   ones, in the server's order. `truncated` is true when fewer waiting volumes are
///   listed than `pending_volumes`.
/// - `done`: entries from the [`DONE_WINDOW`] before the document's `generated_at`
///   (every entry when it has none), newest first, at most [`DONE_LIMIT`].
/// - Over the cap, waiting volumes go first (from the end), then the oldest `done`
///   entries, then running volumes (from the end).
pub fn cloud_queue_doc(
    server_doc: &Value,
    instance_id: &str,
    done: &[DoneEntry],
    max_bytes: usize,
) -> Value {
    let src = server_doc.as_object().cloned().unwrap_or_default();
    let now = src
        .get("generated_at")
        .and_then(Value::as_str)
        .and_then(parse_iso_ms);
    let (mut running, mut waiting): (Vec<Value>, Vec<Value>) = (Vec::new(), Vec::new());
    for v in src
        .get("volumes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut v = v.clone();
        quantise_progress(&mut v);
        if is_running(&v) {
            running.push(v);
        } else {
            waiting.push(v);
        }
    }
    let pending = src
        .get("pending_volumes")
        .and_then(Value::as_u64)
        .map_or(waiting.len(), |n| usize::try_from(n).unwrap_or(usize::MAX));
    waiting.truncate(QUEUE_WAITING_LIMIT);
    let window_ms = i64::try_from(DONE_WINDOW.as_millis()).unwrap_or(i64::MAX);
    let mut recent: Vec<&DoneEntry> = done
        .iter()
        .filter(|e| now.is_none_or(|n| n.saturating_sub(e.at_ms) < window_ms))
        .collect();
    recent.sort_by_key(|a| std::cmp::Reverse(a.at_ms));
    recent.truncate(DONE_LIMIT);
    let mut done: Vec<Value> = recent.into_iter().map(DoneEntry::to_json).collect();
    loop {
        let doc = assemble(&src, instance_id, &running, &waiting, pending, &done);
        let size = json_len(&doc);
        if size <= max_bytes || (running.is_empty() && waiting.is_empty() && done.is_empty()) {
            return doc;
        }
        // Drop entries until their bytes (plus a comma each) cover the overflow;
        // the next round measures again.
        let mut over = size - max_bytes;
        while over > 0 {
            let dropped = if let Some(v) = waiting.pop() {
                v
            } else if let Some(d) = done.pop() {
                d
            } else if let Some(v) = running.pop() {
                v
            } else {
                break;
            };
            over = over.saturating_sub(json_len(&dropped) + 1);
        }
    }
}

/// Whether `next` differs from `prev` in anything but `generated_at` (the queue file
/// is written only on change).
pub fn queue_doc_changed(prev: &Value, next: &Value) -> bool {
    fn stable(v: &Value) -> Value {
        match v {
            Value::Object(m) => Value::Object(
                m.iter()
                    .filter(|(k, _)| *k != "generated_at")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    stable(prev) != stable(next)
}

// ---- ISO-8601 -------------------------------------------------------------------

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

fn civil_parts(ms: i64) -> (i64, i64, i64, i64, i64, i64, i64) {
    let secs = ms.div_euclid(1000);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    (
        y,
        mo,
        d,
        sod / 3600,
        sod % 3600 / 60,
        sod % 60,
        ms.rem_euclid(1000),
    )
}

/// `2026-10-12T10:00:00.123Z` (UTC, milliseconds).
pub fn format_iso_ms(ms: i64) -> String {
    let (y, mo, d, h, mi, s, f) = civil_parts(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{f:03}Z")
}

/// `2026-10-12T10:00:00Z` (UTC, whole seconds).
pub fn format_iso_s(ms: i64) -> String {
    let (y, mo, d, h, mi, s, _) = civil_parts(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Parse `YYYY-MM-DDTHH:MM:SS[.fraction][Z|±HH:MM]` (a missing offset is UTC) to
/// unix ms.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.trim().as_bytes();
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(r)?;
        if part.iter().all(u8::is_ascii_digit) {
            std::str::from_utf8(part).ok()?.parse().ok()
        } else {
            None
        }
    };
    if b.len() < 19
        || b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac_ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return None;
        }
        let digits = &b[start..i.min(start + 3)];
        let mut v = 0i64;
        for &c in digits {
            v = v * 10 + i64::from(c - b'0');
        }
        for _ in digits.len()..3 {
            v *= 10;
        }
        frac_ms = v;
    }
    let offset_s = match b.get(i) {
        None => 0,
        Some(b'Z' | b'z') if i + 1 == b.len() => 0,
        Some(&sign @ (b'+' | b'-')) if b.len() == i + 6 && b[i + 3] == b':' => {
            let off = num(i + 1..i + 3)? * 3600 + num(i + 4..i + 6)? * 60;
            if sign == b'+' { off } else { -off }
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec - offset_s;
    Some(secs * 1000 + frac_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        for ms in [0, 1_760_000_000_123, 951_782_400_000, -86_400_000] {
            assert_eq!(parse_iso_ms(&format_iso_ms(ms)), Some(ms));
            assert_eq!(
                parse_iso_ms(&format_iso_s(ms)),
                Some(ms - ms.rem_euclid(1000))
            );
        }
        assert_eq!(format_iso_s(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            parse_iso_ms("2026-10-12T10:00:00+02:00"),
            parse_iso_ms("2026-10-12T08:00:00Z")
        );
        assert_eq!(
            parse_iso_ms("2026-10-12T10:00:00.5Z"),
            parse_iso_ms("2026-10-12T10:00:00Z").map(|v| v + 500)
        );
        assert_eq!(parse_iso_ms("2026-10-12"), None);
        assert_eq!(parse_iso_ms("not a date at all!!"), None);
    }

    #[test]
    fn rank_orders_epoch_then_claim_then_id() {
        let m = |epoch, at: &str, id: &str| Marker {
            epoch,
            claimed_at: at.into(),
            instance_id: id.into(),
            ..Marker::parse(br#"{"kind":"mokuro-bunko","instance_id":"x","epoch":0}"#).unwrap()
        };
        let a = m(2, "2026-10-12T10:00:00Z", "b");
        let b = m(1, "2026-10-12T09:00:00Z", "a");
        assert_eq!(rank(&a, &b), Ordering::Less);
        let c = m(2, "2026-10-12T09:59:59Z", "z");
        assert_eq!(rank(&c, &a), Ordering::Less);
        let d = m(2, "2026-10-12T10:00:00Z", "a");
        assert_eq!(rank(&d, &a), Ordering::Less);
        let junk = m(2, "garbage", "0");
        assert_eq!(rank(&a, &junk), Ordering::Less);
    }

    #[test]
    fn lenient_parse() {
        let m = Marker::parse(br#"{"kind":"mokuro-bunko","instance_id":"x","epoch":4,"future":1}"#)
            .unwrap();
        assert_eq!((m.state, m.ttl_s, m.version), (MarkerState::Active, 900, 1));
        let m = Marker::parse(
            br#"{"kind":"mokuro-bunko","instance_id":"x","epoch":4,"state":"draining"}"#,
        )
        .unwrap();
        assert_eq!(m.state, MarkerState::Unknown);
        assert!(Marker::parse(br#"{"kind":"other","instance_id":"x","epoch":4}"#).is_none());
        assert!(Marker::parse(b"{}").is_none());
    }
}
