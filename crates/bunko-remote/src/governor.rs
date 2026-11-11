//! The governor (spec §5.11, D23): one per backend, between the store and the
//! provider driver.
//!
//! [`Governed`] wraps a [`Backend`] and routes every call the store, the feed, the
//! outbox and the lease make through one [`Governor`]:
//!
//! - **Classes and priority.** Each call runs in an [`ApiClass`], taken from the task
//!   it runs in ([`scoped`]): `Interactive` (bridge requests, inside
//!   `bunko_store::interactive`) > `Lease` (marker and status files) > `Ocr` (spool
//!   downloads and OCR results) > `Background` (compile, thumbnails, the feed,
//!   first-sync reads). A call waits while a call of a higher class waits for the
//!   same limits.
//! - **Budget.** `Background` calls spend a per-install budget of provider cost units
//!   per minute ([`Limits::background_units_per_min`]; Drive quota units, D23). Other
//!   classes are metered but exempt. Costs are what the driver reports through
//!   [`charge`] while the call runs (Drive and OneDrive count every request), else the
//!   [`Costs`] estimate, which is also what admission checks against.
//! - **Rates and semaphores.** Requests per second, writes per second, requests in
//!   flight and transfers (downloads and uploads) in flight, per provider. An
//!   interactive call bypasses the two semaphores.
//! - **Pause.** A `Throttled` (Retry-After) or `DailyLimit` answer pauses every class
//!   until it passes; health is `Degraded{retry_at}` (or `Blocked(DailyLimit)`).
//!   Inside the pause an interactive call that would wait longer than
//!   [`Limits::interactive_wait`] (2 s; an upload [`Limits::upload_wait`], 60 s) fails
//!   at once with `Throttled(Some(left))`, which the store reports as
//!   `bunko_store::unavailable_for`.
//! - **Hard stops.** `AuthRevoked` → `NeedsReauth`: no provider call is made until
//!   [`Governor::reauthorized`]. `StorageFull` → `Blocked(StorageFull)`: uploads,
//!   folder creation and copies fail at once (the outbox keeps its ops) until a probe
//!   write, let through every [`Limits::storage_probe`] (doubling to
//!   [`Limits::storage_probe_max`]), succeeds. `TransferQuota` →
//!   `Blocked(TransferQuota)`: downloads fail at once until a probe download after the
//!   provider's delay or [`Limits::transfer_probe`] (30 min, doubling to 6 h).
//! - **The api log.** Every call is recorded (class, status, ms, units, bytes) in a
//!   24 h ring, flushed to `index.sqlite`'s `api_log` table; [`Governor::stats`] sums
//!   it per class over the last hour and day.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bunko_store::{Blocked, Health};
use bytes::Bytes;
use futures_util::Stream;
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::backend::{
    AccountInfo, BErr, Backend, BackendCaps, ChangePage, Cursor, OpCtx, Pid, RNode, RemoteBody,
    ServerTime, UploadSource, UploadTarget,
};

/// How long the api log keeps entries.
pub const LOG_SPAN: Duration = Duration::from_secs(24 * 3600);
/// The api log never holds more entries than this (a busy day at 5 req/s is ~430k).
const LOG_CAP: usize = 500_000;

// ---- classes and scopes --------------------------------------------------------------

/// Who a backend call is for, highest priority first (§5.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ApiClass {
    /// A bridge request a reader waits on.
    Interactive,
    /// The lease marker and the cloud status files.
    Lease,
    /// Spool downloads for local OCR, and OCR results going up.
    Ocr,
    /// Everything else: compile, thumbnails, the feed, reconcile, first-sync reads.
    Background,
}

impl ApiClass {
    /// Every class, highest priority first.
    pub const ALL: [ApiClass; 4] = [
        ApiClass::Interactive,
        ApiClass::Lease,
        ApiClass::Ocr,
        ApiClass::Background,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ApiClass::Interactive => "interactive",
            ApiClass::Lease => "lease",
            ApiClass::Ocr => "ocr",
            ApiClass::Background => "background",
        }
    }

    fn parse(s: &str) -> Option<ApiClass> {
        ApiClass::ALL.into_iter().find(|c| c.as_str() == s)
    }

    fn rank(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Copy)]
struct Scope {
    class: ApiClass,
    /// Interactive: the longest pause worth waiting out (else the limits' default).
    max_wait: Option<Duration>,
}

tokio::task_local! {
    static SCOPE: Scope;
    static UNITS: Cell<u64>;
}

/// Run `f` with its backend calls in `class`.
pub async fn scoped<F: Future>(class: ApiClass, f: F) -> F::Output {
    SCOPE
        .scope(
            Scope {
                class,
                max_wait: None,
            },
            f,
        )
        .await
}

/// Run `f` in [`ApiClass::Interactive`], waiting out pauses up to `max_wait`.
pub async fn scoped_interactive<F: Future>(max_wait: Duration, f: F) -> F::Output {
    SCOPE
        .scope(
            Scope {
                class: ApiClass::Interactive,
                max_wait: Some(max_wait),
            },
            f,
        )
        .await
}

/// The class of the calling task, if it set one.
pub fn current_class() -> Option<ApiClass> {
    SCOPE.try_with(|s| s.class).ok()
}

/// Drivers: the request being sent costs `units` provider units. Charged to the
/// governed call running in this task (no-op outside one).
pub fn charge(units: u64) {
    let _ = UNITS.try_with(|c| c.set(c.get() + units));
}

// ---- limits ----------------------------------------------------------------------

/// What a call does: its cost entry and which limits it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// Small metadata reads: stat, find, account, cursor, quota.
    Get,
    /// Listings.
    List,
    /// One change-feed page.
    Changes,
    /// A long poll on a push feed (no semaphore, no rate).
    Wait,
    /// A content read.
    Download,
    /// An upload.
    Upload,
    /// A new folder (or root), a server copy.
    Create,
    /// A move or rename.
    Update,
    /// A trash or permanent delete.
    Delete,
}

impl CallKind {
    fn is_transfer(self) -> bool {
        matches!(self, CallKind::Download | CallKind::Upload)
    }

    fn is_write(self) -> bool {
        matches!(
            self,
            CallKind::Upload | CallKind::Create | CallKind::Update | CallKind::Delete
        )
    }

    /// Writes that need storage space (stopped while the storage is full).
    fn needs_space(self) -> bool {
        matches!(self, CallKind::Upload | CallKind::Create)
    }
}

/// Estimated provider units per call (admission; see [`charge`] for the actual cost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Costs {
    pub get: u64,
    pub list: u64,
    pub changes: u64,
    pub download: u64,
    pub upload: u64,
    pub create: u64,
    pub update: u64,
    pub delete: u64,
}

impl Costs {
    /// Every call costs `n`.
    pub const fn flat(n: u64) -> Costs {
        Costs {
            get: n,
            list: n,
            changes: n,
            download: n,
            upload: n,
            create: n,
            update: n,
            delete: n,
        }
    }

    pub fn of(&self, k: CallKind) -> u64 {
        match k {
            CallKind::Get => self.get,
            CallKind::List => self.list,
            CallKind::Changes => self.changes,
            CallKind::Wait => 0,
            CallKind::Download => self.download,
            CallKind::Upload => self.upload,
            CallKind::Create => self.create,
            CallKind::Update => self.update,
            CallKind::Delete => self.delete,
        }
    }
}

/// One provider's limits (§5.11 Defaults).
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    /// The `Background` budget in provider units per minute (`None`: unlimited).
    pub background_units_per_min: Option<u64>,
    /// Requests per second, all classes.
    pub requests_per_sec: Option<u32>,
    /// Writes per second, all classes.
    pub writes_per_sec: Option<u32>,
    /// Requests in flight (interactive calls not counted).
    pub concurrency: usize,
    /// Downloads and uploads in flight (interactive calls not counted).
    pub transfers: usize,
    pub costs: Costs,
    /// A `Throttled` without `Retry-After` pauses this long.
    pub default_pause: Duration,
    /// A `DailyLimit` without `Retry-After` pauses this long.
    pub daily_pause: Duration,
    /// While the storage is full, a probe write is let through after this long…
    pub storage_probe: Duration,
    /// …doubling up to this.
    pub storage_probe_max: Duration,
    /// After a transfer quota, the first probe download (unless the provider said).
    pub transfer_probe: Duration,
    /// Repeated transfer quotas double the wait up to this.
    pub transfer_probe_max: Duration,
    /// The longest pause an interactive call waits out (spec §3.1).
    pub interactive_wait: Duration,
    /// The longest pause an interactive upload waits out (the bridge PUT, §5.11).
    pub upload_wait: Duration,
}

impl Limits {
    /// No budget, no rates, generous semaphores, every call 1 unit.
    pub fn unlimited() -> Limits {
        Limits {
            background_units_per_min: None,
            requests_per_sec: None,
            writes_per_sec: None,
            concurrency: 16,
            transfers: 8,
            costs: Costs::flat(1),
            default_pause: Duration::from_secs(30),
            daily_pause: Duration::from_secs(3600),
            storage_probe: Duration::from_secs(300),
            storage_probe_max: Duration::from_secs(3600),
            transfer_probe: Duration::from_secs(1800),
            transfer_probe_max: Duration::from_secs(6 * 3600),
            interactive_wait: Duration::from_secs(2),
            upload_wait: Duration::from_secs(60),
        }
    }

    /// Google Drive: a background budget of `units_per_min` quota units (default
    /// 20,000, `drive_background_units_per_min`), 5 req/s, 2 writes/s, 3 transfers.
    pub fn drive(units_per_min: u64) -> Limits {
        Limits {
            background_units_per_min: Some(units_per_min),
            requests_per_sec: Some(5),
            writes_per_sec: Some(2),
            concurrency: 8,
            transfers: 3,
            costs: Costs {
                get: 5,
                list: 100,
                changes: 100,
                download: 200,
                upload: 50,
                create: 50,
                update: 50,
                delete: 50,
            },
            ..Limits::unlimited()
        }
    }

    /// OneDrive: 8 req/s, 2 transfers; costs in Graph resource units.
    pub fn onedrive() -> Limits {
        Limits {
            requests_per_sec: Some(8),
            concurrency: 8,
            transfers: 2,
            costs: Costs {
                get: 1,
                list: 2,
                changes: 1,
                download: 1,
                upload: 2,
                create: 2,
                update: 2,
                delete: 2,
            },
            ..Limits::unlimited()
        }
    }

    /// MEGA: the client already paces its `cs` batches (1/s), so only counted here;
    /// 2 transfers.
    pub fn mega() -> Limits {
        Limits {
            concurrency: 4,
            transfers: 2,
            ..Limits::unlimited()
        }
    }

    /// WebDAV: 3 concurrent requests.
    pub fn webdav() -> Limits {
        Limits {
            concurrency: 3,
            transfers: 3,
            ..Limits::unlimited()
        }
    }
}

// ---- stats ------------------------------------------------------------------------

/// One class's calls over a span.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassStats {
    pub calls: u64,
    /// Calls that failed (any error, throttling included).
    pub errors: u64,
    /// Calls the provider throttled (429 / Retry-After / daily limit).
    pub throttled: u64,
    pub units: u64,
    pub bytes: u64,
    /// Total time in the provider, ms.
    pub ms: u64,
}

impl ClassStats {
    fn add(&mut self, e: &ApiEntry) {
        self.calls += 1;
        if e.status >= 400 {
            self.errors += 1;
        }
        if e.status == 429 {
            self.throttled += 1;
        }
        self.units += e.units;
        self.bytes += e.bytes;
        self.ms += u64::from(e.ms);
    }
}

/// [`ClassStats`] per class for the last hour and the last day
/// (`RemoteStore::api_stats`; the admin panel's per-install units and egress).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiStats {
    hour: [ClassStats; 4],
    day: [ClassStats; 4],
}

impl ApiStats {
    pub fn last_hour(&self, c: ApiClass) -> ClassStats {
        self.hour[c.rank()]
    }

    pub fn last_day(&self, c: ApiClass) -> ClassStats {
        self.day[c.rank()]
    }

    /// Every class together.
    pub fn total_hour(&self) -> ClassStats {
        sum(&self.hour)
    }

    pub fn total_day(&self) -> ClassStats {
        sum(&self.day)
    }
}

fn sum(s: &[ClassStats; 4]) -> ClassStats {
    let mut t = ClassStats::default();
    for c in s {
        t.calls += c.calls;
        t.errors += c.errors;
        t.throttled += c.throttled;
        t.units += c.units;
        t.bytes += c.bytes;
        t.ms += c.ms;
    }
    t
}

/// One api-log row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ApiEntry {
    at_ms: i64,
    class: ApiClass,
    status: u16,
    ms: u32,
    units: u64,
    bytes: u64,
}

/// The HTTP-like status an api-log row records.
fn status_of(r: Option<&BErr>) -> u16 {
    match r {
        None => 200,
        Some(e) => match e {
            BErr::Throttled(_) | BErr::DailyLimit(_) => 429,
            BErr::AuthExpired => 401,
            BErr::AuthRevoked => 403,
            BErr::NotFound => 404,
            BErr::PreconditionFailed => 412,
            BErr::NameConflict => 409,
            BErr::StorageFull => 507,
            BErr::TransferQuota(_) => 509,
            BErr::Transient(_) => 503,
            BErr::CursorLost => 410,
            BErr::TooLarge => 413,
            BErr::InvalidName => 400,
            BErr::Unsupported => 501,
            BErr::Fatal(_) => 500,
        },
    }
}

// ---- the governor ---------------------------------------------------------------

/// A token bucket.
#[derive(Debug, Clone)]
struct Bucket {
    cap: f64,
    per_sec: f64,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(cap: f64, per_sec: f64, now: Instant) -> Bucket {
        Bucket {
            cap,
            per_sec,
            tokens: cap,
            at: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + dt * self.per_sec).min(self.cap);
        self.at = now;
    }

    /// How long until `need` tokens are there (zero: now).
    fn wait_for(&self, need: f64) -> Duration {
        let need = need.min(self.cap);
        if self.tokens >= need || self.per_sec <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((need - self.tokens) / self.per_sec)
        }
    }
}

/// A hard stop with a re-probe.
#[derive(Debug, Clone)]
struct Block {
    probe_at: Instant,
    backoff: Duration,
    /// A probe call is in flight.
    probing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Storage,
    Transfer,
}

struct GovState {
    pause_until: Option<Instant>,
    daily_until: Option<Instant>,
    reauth: bool,
    storage: Option<Block>,
    transfer: Option<Block>,
    budget: Option<Bucket>,
    rps: Option<Bucket>,
    wps: Option<Bucket>,
    inflight: usize,
    transfers: usize,
    waiting: [usize; 4],
}

/// Wall-clock ms that follow tokio's clock (paused-time tests see the log age).
#[derive(Debug, Clone, Copy)]
struct Clock {
    sys0_ms: i64,
    inst0: Instant,
}

impl Clock {
    fn now() -> Clock {
        Clock {
            sys0_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64),
            inst0: Instant::now(),
        }
    }

    fn ms_at(&self, t: Instant) -> i64 {
        self.sys0_ms + t.saturating_duration_since(self.inst0).as_millis() as i64
    }

    fn system_at(&self, t: Instant) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(self.ms_at(t).max(0) as u64)
    }
}

struct Log {
    entries: VecDeque<ApiEntry>,
    /// Entries not yet in `api_log`.
    unflushed: usize,
    db: Option<Connection>,
}

/// The governor of one backend. Create it inside the runtime it will run on (its
/// clock follows tokio's).
pub struct Governor {
    limits: Limits,
    st: Mutex<GovState>,
    notify: Notify,
    log: Mutex<Log>,
    clock: Clock,
}

impl Governor {
    pub fn new(limits: Limits) -> Arc<Governor> {
        let now = Instant::now();
        let st = GovState {
            pause_until: None,
            daily_until: None,
            reauth: false,
            storage: None,
            transfer: None,
            budget: limits
                .background_units_per_min
                .map(|u| Bucket::new(u as f64, u as f64 / 60.0, now)),
            rps: limits
                .requests_per_sec
                .map(|r| Bucket::new(f64::from(r), f64::from(r), now)),
            wps: limits
                .writes_per_sec
                .map(|r| Bucket::new(f64::from(r), f64::from(r), now)),
            inflight: 0,
            transfers: 0,
            waiting: [0; 4],
        };
        Arc::new(Governor {
            limits,
            st: Mutex::new(st),
            notify: Notify::new(),
            log: Mutex::new(Log {
                entries: VecDeque::new(),
                unflushed: 0,
                db: None,
            }),
            clock: Clock::now(),
        })
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Keep the api log in `index.sqlite` (its `api_log` table): load the last 24 h
    /// now, append on [`Governor::flush_log`].
    pub fn attach_log(&self, index_path: &Path) -> rusqlite::Result<()> {
        let conn = Connection::open(index_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let since = self.clock.ms_at(Instant::now()) - LOG_SPAN.as_millis() as i64;
        conn.execute("DELETE FROM api_log WHERE at < ?1", [since])?;
        let rows: Vec<ApiEntry> = conn
            .prepare("SELECT at, class, status, ms, units, bytes FROM api_log ORDER BY at")?
            .query_map([], |r| {
                let class: String = r.get(1)?;
                Ok(ApiEntry {
                    at_ms: r.get(0)?,
                    class: ApiClass::parse(&class).unwrap_or(ApiClass::Background),
                    status: r.get(2)?,
                    ms: r.get(3)?,
                    units: r.get::<_, i64>(4)? as u64,
                    bytes: r.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut log = self.log.lock();
        let pending: Vec<ApiEntry> = log.entries.drain(..).collect();
        log.entries.extend(rows);
        log.entries.extend(pending);
        log.db = Some(conn);
        Ok(())
    }

    /// Write the unflushed api-log entries to `index.sqlite` and drop rows older
    /// than 24 h.
    pub fn flush_log(&self) {
        let mut log = self.log.lock();
        let n = log.unflushed;
        if n == 0 || log.db.is_none() {
            return;
        }
        let start = log.entries.len().saturating_sub(n);
        let rows: Vec<ApiEntry> = log.entries.range(start..).copied().collect();
        let since = self.clock.ms_at(Instant::now()) - LOG_SPAN.as_millis() as i64;
        let Some(db) = log.db.as_mut() else { return };
        let r = (|| -> rusqlite::Result<()> {
            let tx = db.transaction()?;
            {
                let mut s = tx.prepare_cached(
                    "INSERT INTO api_log(at, class, status, ms, units, bytes) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                )?;
                for e in &rows {
                    s.execute(params![
                        e.at_ms,
                        e.class.as_str(),
                        e.status,
                        e.ms,
                        e.units as i64,
                        e.bytes as i64
                    ])?;
                }
            }
            tx.execute("DELETE FROM api_log WHERE at < ?1", [since])?;
            tx.commit()
        })();
        match r {
            Ok(()) => log.unflushed = 0,
            Err(e) => tracing::warn!("api log flush: {e}"),
        }
    }

    /// The api log summed per class over the last hour and day.
    pub fn stats(&self) -> ApiStats {
        let now = self.clock.ms_at(Instant::now());
        let hour = now - 3_600_000;
        let day = now - LOG_SPAN.as_millis() as i64;
        let mut s = ApiStats::default();
        for e in self.log.lock().entries.iter().filter(|e| e.at_ms >= day) {
            s.day[e.class.rank()].add(e);
            if e.at_ms >= hour {
                s.hour[e.class.rank()].add(e);
            }
        }
        s
    }

    /// What the governor makes of the provider's state, if anything: `NeedsReauth`,
    /// `Blocked(..)`, or `Degraded{retry_at}` while paused.
    pub fn health(&self) -> Option<Health> {
        let st = self.st.lock();
        let now = Instant::now();
        if st.reauth {
            return Some(Health::NeedsReauth);
        }
        if st.storage.is_some() {
            return Some(Health::Blocked(Blocked::StorageFull));
        }
        if st.daily_until.is_some_and(|t| t > now) {
            return Some(Health::Blocked(Blocked::DailyLimit));
        }
        if st.transfer.is_some() {
            return Some(Health::Blocked(Blocked::TransferQuota));
        }
        st.pause_until
            .filter(|t| *t > now)
            .map(|t| Health::Degraded {
                retry_at: self.clock.system_at(t),
            })
    }

    /// How much longer every call waits (a Retry-After or daily-limit pause).
    pub fn paused_for(&self) -> Option<Duration> {
        let st = self.st.lock();
        let now = Instant::now();
        [st.pause_until, st.daily_until]
            .into_iter()
            .flatten()
            .max()
            .filter(|t| *t > now)
            .map(|t| t - now)
    }

    /// Wait (without taking anything) until a call of `class` costing `units` would
    /// not wait for a pause or the budget. Hard stops do not wait: their calls fail
    /// at once.
    pub async fn ready(&self, class: ApiClass, units: u64) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let mut guard = self.st.lock();
                let st = &mut *guard;
                let now = Instant::now();
                let paused = [st.pause_until, st.daily_until]
                    .into_iter()
                    .flatten()
                    .max()
                    .filter(|t| *t > now)
                    .map(|t| t - now);
                let budget = match (&mut st.budget, class) {
                    (Some(b), ApiClass::Background) => {
                        b.refill(now);
                        Some(b.wait_for(units as f64)).filter(|d| !d.is_zero())
                    }
                    _ => None,
                };
                match paused.or(budget) {
                    None => return,
                    Some(d) => d,
                }
            };
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = &mut notified => {}
            }
        }
    }

    /// The credentials were replaced: provider calls may run again.
    pub fn reauthorized(&self) {
        self.st.lock().reauth = false;
        self.notify.notify_waiters();
    }

    /// Wait for the limits, then take a permit for one call of `kind` in `class`.
    async fn admit(
        self: &Arc<Self>,
        class: ApiClass,
        max_wait: Option<Duration>,
        kind: CallKind,
    ) -> Result<Permit, BErr> {
        let est = self.limits.costs.of(kind) as f64;
        let interactive = class == ApiClass::Interactive;
        let max_wait = interactive.then(|| {
            max_wait.unwrap_or(if kind == CallKind::Upload {
                self.limits.upload_wait
            } else {
                self.limits.interactive_wait
            })
        });
        let rank = class.rank();
        let mut queued: Option<Queued> = None;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let wait = {
                let mut guard = self.st.lock();
                let st = &mut *guard;
                let now = Instant::now();
                if st.reauth {
                    return Err(BErr::AuthRevoked);
                }
                let mut probe = None;
                if kind.needs_space()
                    && let Some(b) = &st.storage
                {
                    if now < b.probe_at || b.probing {
                        return Err(BErr::StorageFull);
                    }
                    probe = Some(Probe::Storage);
                }
                if kind == CallKind::Download
                    && let Some(b) = &st.transfer
                {
                    if now < b.probe_at || b.probing {
                        return Err(BErr::TransferQuota(Some(
                            b.probe_at.saturating_duration_since(now),
                        )));
                    }
                    probe = Some(Probe::Transfer);
                }
                let paused = [st.pause_until, st.daily_until]
                    .into_iter()
                    .flatten()
                    .max()
                    .filter(|t| *t > now);
                let wait = if let Some(until) = paused {
                    let left = until - now;
                    if max_wait.is_some_and(|m| left > m) {
                        return Err(if st.daily_until == Some(until) {
                            BErr::DailyLimit(Some(left))
                        } else {
                            BErr::Throttled(Some(left))
                        });
                    }
                    Some(left)
                } else {
                    for b in [&mut st.budget, &mut st.rps, &mut st.wps]
                        .into_iter()
                        .flatten()
                    {
                        b.refill(now);
                    }
                    let ahead = st.waiting[..rank].iter().any(|n| *n > 0);
                    let slots = interactive
                        || kind == CallKind::Wait
                        || (st.inflight < self.limits.concurrency
                            && (!kind.is_transfer() || st.transfers < self.limits.transfers));
                    let mut timed = Duration::ZERO;
                    if kind != CallKind::Wait {
                        if let Some(b) = &st.rps {
                            timed = timed.max(b.wait_for(1.0));
                        }
                        if kind.is_write()
                            && let Some(b) = &st.wps
                        {
                            timed = timed.max(b.wait_for(1.0));
                        }
                    }
                    if class == ApiClass::Background
                        && let Some(b) = &st.budget
                    {
                        timed = timed.max(b.wait_for(est));
                    }
                    if !ahead && slots && timed.is_zero() {
                        let counted = !interactive && kind != CallKind::Wait;
                        if kind != CallKind::Wait {
                            if let Some(b) = &mut st.rps {
                                b.tokens -= 1.0;
                            }
                            if kind.is_write()
                                && let Some(b) = &mut st.wps
                            {
                                b.tokens -= 1.0;
                            }
                        }
                        let charged = class == ApiClass::Background && st.budget.is_some();
                        if charged && let Some(b) = &mut st.budget {
                            b.tokens -= est;
                        }
                        if counted {
                            st.inflight += 1;
                            if kind.is_transfer() {
                                st.transfers += 1;
                            }
                        }
                        match probe {
                            Some(Probe::Storage) => {
                                if let Some(b) = &mut st.storage {
                                    b.probing = true;
                                }
                            }
                            Some(Probe::Transfer) => {
                                if let Some(b) = &mut st.transfer {
                                    b.probing = true;
                                }
                            }
                            None => {}
                        }
                        if let Some(q) = queued.take() {
                            q.disarm();
                            st.waiting[rank] -= 1;
                            // Lower classes held back only by this waiter may go.
                            self.notify.notify_waiters();
                        }
                        return Ok(Permit {
                            gov: self.clone(),
                            class,
                            kind,
                            counted,
                            charged_est: if charged { est } else { 0.0 },
                            probe,
                            pending: None,
                        });
                    }
                    (!timed.is_zero()).then_some(timed)
                };
                if queued.is_none() {
                    st.waiting[rank] += 1;
                    queued = Some(Queued {
                        gov: self.clone(),
                        rank,
                        armed: true,
                    });
                }
                wait
            };
            match wait {
                Some(d) => {
                    tokio::select! {
                        _ = tokio::time::sleep(d) => {}
                        _ = &mut notified => {}
                    }
                }
                None => notified.await,
            }
        }
    }

    /// Account for a finished call: state transitions, the budget, the log.
    fn finish(&self, p: &mut Permit, err: Option<&BErr>, took: Duration, units: u64, bytes: u64) {
        p.pending = None;
        let now = Instant::now();
        {
            let mut guard = self.st.lock();
            let st = &mut *guard;
            let probe = p.probe.take();
            match err {
                None => match probe {
                    Some(Probe::Storage) => {
                        tracing::info!("storage: writes accepted again");
                        st.storage = None;
                    }
                    Some(Probe::Transfer) => {
                        tracing::info!("transfer quota: downloads work again");
                        st.transfer = None;
                    }
                    None => {}
                },
                Some(e) => {
                    match e {
                        BErr::Throttled(d) => {
                            let until = now + d.unwrap_or(self.limits.default_pause);
                            st.pause_until = st.pause_until.max(Some(until));
                        }
                        BErr::DailyLimit(d) => {
                            let until = now + d.unwrap_or(self.limits.daily_pause);
                            st.daily_until = st.daily_until.max(Some(until));
                        }
                        BErr::AuthRevoked => {
                            if !st.reauth {
                                tracing::warn!(
                                    "the provider revoked Bunko's authorization; no calls until reauthorized"
                                );
                            }
                            st.reauth = true;
                        }
                        BErr::StorageFull => {
                            let backoff = match &st.storage {
                                Some(b) => (b.backoff * 2).min(self.limits.storage_probe_max),
                                None => self.limits.storage_probe,
                            };
                            st.storage = Some(Block {
                                probe_at: now + backoff,
                                backoff,
                                probing: false,
                            });
                        }
                        BErr::TransferQuota(d) if p.kind == CallKind::Download => {
                            let first = d.unwrap_or(self.limits.transfer_probe);
                            let backoff = match &st.transfer {
                                Some(b) => first.max(b.backoff * 2),
                                None => first,
                            }
                            .min(self.limits.transfer_probe_max);
                            st.transfer = Some(Block {
                                probe_at: now + backoff,
                                backoff,
                                probing: false,
                            });
                        }
                        _ => {}
                    }
                    // An inconclusive probe: the next call probes again.
                    match probe {
                        Some(Probe::Storage) if *e != BErr::StorageFull => {
                            if let Some(b) = &mut st.storage {
                                b.probing = false;
                            }
                        }
                        Some(Probe::Transfer) if !matches!(e, BErr::TransferQuota(_)) => {
                            if let Some(b) = &mut st.transfer {
                                b.probing = false;
                            }
                        }
                        _ => {}
                    }
                }
            }
            if p.charged_est > 0.0
                && let Some(b) = &mut st.budget
            {
                b.tokens -= units as f64 - p.charged_est;
            }
        }
        let entry = ApiEntry {
            at_ms: self.clock.ms_at(now),
            class: p.class,
            status: status_of(err),
            ms: took.as_millis().min(u128::from(u32::MAX)) as u32,
            units,
            bytes,
        };
        let mut log = self.log.lock();
        log.entries.push_back(entry);
        log.unflushed += 1;
        let cut = entry.at_ms - LOG_SPAN.as_millis() as i64;
        while log.entries.len() > LOG_CAP || log.entries.front().is_some_and(|e| e.at_ms < cut) {
            log.entries.pop_front();
            log.unflushed = log.unflushed.min(log.entries.len());
        }
        drop(log);
        self.notify.notify_waiters();
    }

    /// Run one call of `kind` through the limits.
    async fn run<T, F>(self: &Arc<Self>, kind: CallKind, f: F) -> Result<T, BErr>
    where
        F: Future<Output = Result<T, BErr>>,
    {
        self.run_bytes(kind, 0, f).await.map(|(t, _)| t)
    }

    async fn run_bytes<T, F>(
        self: &Arc<Self>,
        kind: CallKind,
        bytes: u64,
        f: F,
    ) -> Result<(T, Permit), BErr>
    where
        F: Future<Output = Result<T, BErr>>,
    {
        let scope = SCOPE.try_with(|s| *s).ok();
        let class = scope.map_or(ApiClass::Background, |s| s.class);
        let mut permit = self
            .admit(class, scope.and_then(|s| s.max_wait), kind)
            .await?;
        let t0 = Instant::now();
        let (r, charged) = UNITS
            .scope(Cell::new(0), async {
                let r = f.await;
                (r, UNITS.with(Cell::get))
            })
            .await;
        let units = if charged > 0 {
            charged
        } else {
            self.limits.costs.of(kind)
        };
        match r {
            Ok(t) => {
                // A download is logged when its body ends (with its bytes).
                if kind != CallKind::Download {
                    self.finish(&mut permit, None, t0.elapsed(), units, bytes);
                } else {
                    permit.pending = Some((t0, units));
                }
                Ok((t, permit))
            }
            Err(e) => {
                self.finish(&mut permit, Some(&e), t0.elapsed(), units, bytes);
                Err(e)
            }
        }
    }
}

/// A call's admission: releases its slots when dropped.
struct Permit {
    gov: Arc<Governor>,
    class: ApiClass,
    kind: CallKind,
    counted: bool,
    charged_est: f64,
    probe: Option<Probe>,
    /// A download whose body is still streaming: when it started, its units.
    pending: Option<(Instant, u64)>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        {
            let mut st = self.gov.st.lock();
            if self.counted {
                st.inflight -= 1;
                if self.kind.is_transfer() {
                    st.transfers -= 1;
                }
            }
            // Cancelled mid-call: let another call probe.
            match self.probe.take() {
                Some(Probe::Storage) => {
                    if let Some(b) = &mut st.storage {
                        b.probing = false;
                    }
                }
                Some(Probe::Transfer) => {
                    if let Some(b) = &mut st.transfer {
                        b.probing = false;
                    }
                }
                None => {}
            }
        }
        self.gov.notify.notify_waiters();
    }
}

/// A waiter's place in its class's queue.
struct Queued {
    gov: Arc<Governor>,
    rank: usize,
    armed: bool,
}

impl Queued {
    /// The waiter got its permit (the count was taken down under the lock).
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for Queued {
    fn drop(&mut self) {
        if self.armed {
            self.gov.st.lock().waiting[self.rank] -= 1;
            self.gov.notify.notify_waiters();
        }
    }
}

// ---- the governed backend -------------------------------------------------------

/// A [`Backend`] whose every call goes through a [`Governor`].
pub struct Governed<B: Backend> {
    inner: Arc<B>,
    gov: Arc<Governor>,
}

impl<B: Backend> Governed<B> {
    pub fn new(inner: Arc<B>, gov: Arc<Governor>) -> Governed<B> {
        Governed { inner, gov }
    }

    /// The driver itself (calls on it bypass the governor).
    pub fn inner(&self) -> &Arc<B> {
        &self.inner
    }

    pub fn governor(&self) -> &Arc<Governor> {
        &self.gov
    }
}

/// A download body that holds its permit (a transfer slot) until it ends, then logs
/// the call with its bytes.
struct CountedBody {
    inner: RemoteBody,
    permit: Option<Permit>,
    bytes: u64,
    failed: bool,
}

impl CountedBody {
    fn end(&mut self) {
        if let Some(mut p) = self.permit.take() {
            let (t0, units) = p.pending.take().unwrap_or((Instant::now(), 0));
            let gov = p.gov.clone();
            let err = self
                .failed
                .then(|| BErr::Transient("the download body failed".into()));
            gov.finish(&mut p, err.as_ref(), t0.elapsed(), units, self.bytes);
        }
    }
}

impl Stream for CountedBody {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let r = self.inner.as_mut().poll_next(cx);
        match &r {
            Poll::Ready(Some(Ok(b))) => self.bytes += b.len() as u64,
            Poll::Ready(Some(Err(_))) => {
                self.failed = true;
                self.end();
            }
            Poll::Ready(None) => self.end(),
            Poll::Pending => {}
        }
        r
    }
}

impl Drop for CountedBody {
    fn drop(&mut self) {
        self.end();
    }
}

fn upload_size(src: &UploadSource<'_>) -> u64 {
    let UploadSource::File { size, .. } = src;
    *size
}

impl<B: Backend> Backend for Governed<B> {
    fn caps(&self) -> BackendCaps {
        self.inner.caps()
    }

    fn limits(&self) -> Limits {
        self.gov.limits.clone()
    }

    async fn account(&self) -> Result<AccountInfo, BErr> {
        self.gov.run(CallKind::Get, self.inner.account()).await
    }

    async fn locate_roots(&self, name: &str) -> Result<Vec<RNode>, BErr> {
        self.gov
            .run(CallKind::List, self.inner.locate_roots(name))
            .await
    }

    async fn create_root(&self, name: &str) -> Result<RNode, BErr> {
        self.gov
            .run(CallKind::Create, self.inner.create_root(name))
            .await
    }

    async fn start_cursor(&self) -> Result<Cursor, BErr> {
        self.gov.run(CallKind::Get, self.inner.start_cursor()).await
    }

    async fn list_all(
        &self,
        roots: &[Pid],
        sink: &mut (dyn FnMut(RNode) + Send),
    ) -> Result<(), BErr> {
        self.gov
            .run(CallKind::List, self.inner.list_all(roots, sink))
            .await
    }

    async fn list_dir(&self, dir: &str, sink: &mut (dyn FnMut(RNode) + Send)) -> Result<(), BErr> {
        self.gov
            .run(CallKind::List, self.inner.list_dir(dir, sink))
            .await
    }

    async fn find_named(&self, parent: &str, name: &str) -> Result<(Vec<RNode>, ServerTime), BErr> {
        self.gov
            .run(CallKind::Get, self.inner.find_named(parent, name))
            .await
    }

    async fn changes(&self, c: &Cursor) -> Result<ChangePage, BErr> {
        self.gov.run(CallKind::Changes, self.inner.changes(c)).await
    }

    async fn wait_change(&self, c: &Cursor, max: Duration) -> Result<(), BErr> {
        self.gov
            .run(CallKind::Wait, self.inner.wait_change(c, max))
            .await
    }

    async fn stat(&self, pid: &str) -> Result<Option<RNode>, BErr> {
        self.gov.run(CallKind::Get, self.inner.stat(pid)).await
    }

    async fn read(
        &self,
        pid: &str,
        pin: Option<&str>,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<RemoteBody, BErr> {
        let (body, permit) = self
            .gov
            .run_bytes(CallKind::Download, 0, self.inner.read(pid, pin, range))
            .await?;
        Ok(Box::pin(CountedBody {
            inner: body,
            permit: Some(permit),
            bytes: 0,
            failed: false,
        }))
    }

    async fn upload(
        &self,
        to: UploadTarget<'_>,
        src: UploadSource<'_>,
        op: &mut OpCtx,
    ) -> Result<RNode, BErr> {
        let size = upload_size(&src);
        self.gov
            .run_bytes(CallKind::Upload, size, self.inner.upload(to, src, op))
            .await
            .map(|(n, _)| n)
    }

    async fn mkdir(&self, parent: &str, name: &str, op: &OpCtx) -> Result<RNode, BErr> {
        self.gov
            .run(CallKind::Create, self.inner.mkdir(parent, name, op))
            .await
    }

    async fn update(
        &self,
        pid: &str,
        parent: Option<&str>,
        name: Option<&str>,
        if_cver: Option<&str>,
    ) -> Result<RNode, BErr> {
        self.gov
            .run(
                CallKind::Update,
                self.inner.update(pid, parent, name, if_cver),
            )
            .await
    }

    async fn copy(&self, pid: &str, parent: &str, name: &str) -> Result<RNode, BErr> {
        self.gov
            .run(CallKind::Create, self.inner.copy(pid, parent, name))
            .await
    }

    async fn trash(&self, pid: &str, if_cver: Option<&str>) -> Result<(), BErr> {
        self.gov
            .run(CallKind::Delete, self.inner.trash(pid, if_cver))
            .await
    }

    async fn delete_permanent(&self, pid: &str) -> Result<(), BErr> {
        self.gov
            .run(CallKind::Delete, self.inner.delete_permanent(pid))
            .await
    }

    async fn quota(&self) -> Result<Option<bunko_store::Capacity>, BErr> {
        self.gov.run(CallKind::Get, self.inner.quota()).await
    }
}
