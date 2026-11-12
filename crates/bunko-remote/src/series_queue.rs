//! The store sink of the OCR queue file v2 (`docs/design/queue-file-v2.md` §5, §6): the
//! lease holder writes `<Series>/.mokuro-series-queue.json` into each series folder
//! that owes OCR, and keeps the library-wide state (hold, held rows, work clock) in the
//! lease marker's `series_queue` block ([`MarkerBlock`]).
//!
//! The format, the clock, the dirty test and the renderer are `bunko_sched::
//! series_queue`'s; this module decides **when** a file is written:
//!
//! - **What there is to write** comes from a [`SeriesQueueSource`] (the server's OCR
//!   scheduler), shifted onto the store's clock (§6.9, [`Fence::clock_offset_ms`]).
//! - **Batches** (§6.4): every window W ([`pacing`], or `queue_floor`) at most B
//!   series are staged in one burst, P0 (oldest first), P1, P2 (soonest finish first),
//!   then P3 tidies, with aging; a provider's throttle signal halves B for three
//!   windows ([`Budget`]).
//! - **Stage-time state** (§6.4): between staging and commit the dirty test compares
//!   against what was staged, so one change is staged once; a failed op marks its
//!   series again. What committed is persisted in `state.sqlite` (§6.7), so a restart
//!   rewrites nothing that did not change.
//! - **Settle rule** (§5.3): a finished job stays in its file, shown running, until no
//!   OCR or Compiler write of its series is left in the outbox and no metadata
//!   recompile is pending for it.
//! - **The clock** is refitted just before each heartbeat and switched to only once the
//!   marker carrying it is written (§6.2); drift is judged right after a switch. A
//!   change of the hold asks the lease for an early heartbeat (§6.8).
//! - **Foreign files** (§6.8, §10): the index says whose version each series file is
//!   (the ledger). A file this sink tracks that is no longer its own, or gone, is
//!   rewritten at P0; another Bunko's file where nothing is owed, a stray spelling and
//!   the root `.mokuro-queue.json` a 0.8 pre-release left are deleted (P3). The
//!   pre-release's writes of that file are dropped at startup (`outbox.rs` replay), so
//!   none holds it as pending.
//! - **No answer yet** (the server's queue, until the scheduler's first walk of the
//!   library is over): the source is asked again after [`SNAPSHOT_RETRY`].
//!   [`SeriesQueueSink::status_json`] says so, and why nothing else is written.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use bunko_sched::series_queue::{
    Backlog, Budget, Class, Clock, Priority, Published, RowDesc, SeriesLive, Snapshot, Tuple,
    classify, fit_points, marker_block, render,
};
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;

use crate::backend::{Backend, BackendCaps, HashKind};
use crate::cache::now_ms;
use crate::index::{ROOT_EID, RelPath};
use crate::names::{ROOT_QUEUE_FILE, SERIES_QUEUE_FILE, fold};
use crate::outbox::is_series_queue_op;
use crate::state::{SeriesQueueRow, StateDb};
use crate::status::{BlockFuture, Fence, HeartbeatRequester, MarkerBlock};
use crate::store::Core;

/// What a [`SeriesQueueSource`] answers with.
pub type SnapshotFuture<'a> = Pin<Box<dyn Future<Output = Option<Snapshot>> + Send + 'a>>;

/// Where the store sink learns what the queue holds (the server's OCR scheduler).
pub trait SeriesQueueSource: Send + Sync {
    /// The whole queue now, priced, on this machine's clock (None: no answer now).
    fn snapshot(&self) -> SnapshotFuture<'_>;
    /// Series `folder`'s compiled metadata may still change from work already asked
    /// for (a recompile armed, queued or running): the settle rule's second half.
    fn regeneration_pending(&self, folder: &str) -> bool;
    /// While [`snapshot`](Self::snapshot) has no answer: what it waits for, one word
    /// (`first_walk`), which the sink's status shows as `waiting_for_<word>`. `None`:
    /// nothing in particular (the status says `waiting_for_queue`).
    fn waiting_for(&self) -> Option<&'static str> {
        None
    }
}

/// How long the sink waits for a snapshot.
pub const SNAPSHOT_WAIT: Duration = Duration::from_secs(5);
/// A source with no answer is asked again this long after.
pub const SNAPSHOT_RETRY: Duration = Duration::from_secs(1);
/// The sink looks at the queue at least this often (holds, foreign files), and writes
/// once per window.
pub const TICK_MAX: Duration = Duration::from_secs(30);

/// The window W and the cap B of a provider (§6.4): local folder 15 s / 100, WebDAV
/// 30 s / 20, Google Drive 120 s / 30, OneDrive 120 s / 15, MEGA 300 s / 20.
pub fn pacing(caps: &BackendCaps) -> (Duration, usize) {
    if caps.path_ids && caps.push {
        // A local folder: path ids, and a watcher in place of a polled feed.
        (Duration::from_secs(15), 100)
    } else if caps.path_ids {
        (Duration::from_secs(30), 20)
    } else if caps.push && !caps.atomic_replace {
        (Duration::from_secs(300), 20)
    } else if caps.hash_kind == Some(HashKind::QuickXor) {
        (Duration::from_secs(120), 15)
    } else {
        (Duration::from_secs(120), 30)
    }
}

/// The early heartbeat's timing (§6.8) and the refit period of a store without a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesQueueTiming {
    /// A change of the hold or the held rows asks for a heartbeat this long after the
    /// last change …
    pub hold_debounce: Duration,
    /// … this long after an unhold (so the plan settles) …
    pub unhold_debounce: Duration,
    /// … and at most once per this.
    pub early_gap: Duration,
    /// Without a lease (no heartbeat to ride), the clock is refitted this often.
    pub refit_every: Duration,
}

impl Default for SeriesQueueTiming {
    fn default() -> Self {
        SeriesQueueTiming {
            hold_debounce: Duration::from_secs(30),
            unhold_debounce: Duration::from_secs(60),
            early_gap: Duration::from_secs(120),
            refit_every: Duration::from_secs(300),
        }
    }
}

/// What the hold looks like: `(held reason, held rows)`.
type HoldSig = (Option<String>, BTreeSet<String>);

fn hold_sig(s: &Snapshot) -> HoldSig {
    (s.held.clone(), s.held_rows.clone())
}

/// One write the sink owes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Task {
    /// Write series (fold key)'s file.
    Write(String),
    /// Delete series (fold key)'s file: it owes nothing and its `done` tail expired.
    Expire(String),
    /// Delete the file at this path: a stray spelling, another Bunko's copy where
    /// nothing is owed, the root leftover.
    Stray(String),
}

/// What committed for a series.
#[derive(Clone, Debug)]
struct Landed {
    file: Published,
}

/// What is on its way for a series.
#[derive(Clone, Debug)]
struct Staged {
    op: u64,
    file: Published,
    priority: Priority,
    forced: bool,
}

/// What the sink's last look found (its status, [`SeriesQueueSink::status_json`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No look yet.
    Starting,
    /// The store's first adoption runs: nothing is written before it ends.
    Adopting,
    /// The fence is shut: this Bunko does not hold the lease (yet), or a caller's
    /// gate holds the writes.
    Fenced,
    /// The source had no answer (what it waits for, if it says).
    Waiting(Option<&'static str>),
    /// Writing what the queue needs.
    Ready,
}

struct State {
    /// The clock in use (confirmed by a marker write); None before the first.
    clock: Option<Clock>,
    /// The clock (and hold) the marker being written carries.
    prepared: Option<(Clock, Option<HoldSig>)>,
    /// Judge drift at the next look (a clock switch happened).
    judge_drift: bool,
    rev: u64,
    landed: BTreeMap<String, Landed>,
    staged: BTreeMap<String, Staged>,
    backlog: Backlog<Task>,
    /// Series whose mark came from drift (kept until written or judged again).
    drifted: BTreeSet<String>,
    /// Series rewritten at P0 whatever the dirty test says (a foreign or missing file,
    /// an op left from an earlier run).
    forced: BTreeSet<String>,
    budget: Budget,
    /// The last snapshot, on the store's clock.
    snap: Option<Arc<Snapshot>>,
    last_tick: Option<Instant>,
    last_flush: Option<Instant>,
    /// When the last window's burst ran, ms since the epoch (this machine's clock).
    last_flush_ms: Option<i64>,
    last_refit: Option<Instant>,
    /// The source had no answer: asked again at this time.
    retry_at: Option<Instant>,
    phase: Phase,
    /// The hold the last confirmed marker carries.
    marker_hold: Option<HoldSig>,
    /// The hold an early heartbeat is waiting to carry, and when it is due.
    early: Option<(HoldSig, Instant)>,
    last_early: Option<Instant>,
    /// Ops a previous run left were looked for.
    replay_checked: bool,
    /// Series whose folder holds only another spelling of the file: written once it
    /// is deleted.
    blocked: BTreeSet<String>,
}

/// The store sink: one per store with `status_files` on.
pub struct SeriesQueueSink {
    window: Duration,
    cap: usize,
    timing: SeriesQueueTiming,
    instance_id: String,
    /// The store runs a lease (its marker carries the block, its heartbeat refits).
    lease: bool,
    db: Arc<StateDb>,
    source: Mutex<Option<Arc<dyn SeriesQueueSource>>>,
    outcomes: Mutex<Vec<(u64, bool)>>,
    throttles: AtomicU32,
    wake: Notify,
    st: Mutex<State>,
    fence: OnceLock<Fence>,
    heartbeat: OnceLock<HeartbeatRequester>,
}

const META_CLOCK: &str = "series_queue_clock";
const META_REV: &str = "series_queue_rev";

fn tuple_json(row: &str, t: &Tuple) -> Value {
    match t {
        Tuple::Queued(w) => json!([row, "q", w]),
        Tuple::Claimed => json!([row, "c"]),
        Tuple::Running { t0, e } => json!([row, "r", t0, e]),
    }
}

fn tuple_of(v: &Value) -> Option<(String, Tuple)> {
    let a = v.as_array()?;
    let row = a.first()?.as_str()?.to_string();
    let t = match a.get(1)?.as_str()? {
        "q" => Tuple::Queued(a.get(2).and_then(Value::as_i64)),
        "c" => Tuple::Claimed,
        "r" => Tuple::Running {
            t0: a.get(2)?.as_i64()?,
            e: a.get(3).and_then(Value::as_i64),
        },
        _ => return None,
    };
    Some((row, t))
}

/// `jobs` and `done` of a `series_queue` row.
fn row_of(fold_key: &str, f: &Published, content_sha: String) -> SeriesQueueRow {
    let vols: Map<String, Value> = f
        .vols
        .iter()
        .map(|(stem, jobs)| {
            (
                stem.clone(),
                Value::Array(jobs.iter().map(|(r, t)| tuple_json(r, t)).collect()),
            )
        })
        .collect();
    let rows: Vec<Value> = f
        .rows
        .iter()
        .map(|r| json!({"kind": r.kind, "id": r.id, "parts": r.parts}))
        .collect();
    SeriesQueueRow {
        series_fold: fold_key.to_string(),
        folder: f.folder.clone(),
        rev: f.rev,
        content_sha,
        jobs: json!({"at": f.at, "clock_at": f.clock_at, "rows": rows, "vols": vols}).to_string(),
        done: Value::Object(f.done.iter().map(|(k, v)| (k.clone(), json!(v))).collect())
            .to_string(),
        last_write_ms: now_ms(),
    }
}

fn published_of(row: &SeriesQueueRow) -> Option<Published> {
    let jobs: Value = serde_json::from_str(&row.jobs).ok()?;
    let done: Value = serde_json::from_str(&row.done).ok()?;
    let rows = jobs["rows"]
        .as_array()?
        .iter()
        .map(|r| {
            Some(RowDesc {
                kind: r["kind"].as_str()?.to_string(),
                id: r["id"].as_str()?.to_string(),
                parts: r["parts"].clone(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let vols = jobs["vols"]
        .as_object()?
        .iter()
        .map(|(stem, list)| {
            let jobs = list
                .as_array()?
                .iter()
                .map(tuple_of)
                .collect::<Option<Vec<_>>>()?;
            Some((stem.clone(), jobs))
        })
        .collect::<Option<BTreeMap<_, _>>>()?;
    let done = done
        .as_object()?
        .iter()
        .map(|(k, v)| Some((k.clone(), v.as_i64()?)))
        .collect::<Option<BTreeMap<_, _>>>()?;
    Some(Published {
        folder: row.folder.clone(),
        rev: row.rev,
        at: jobs["at"].as_i64()?,
        clock_at: jobs["clock_at"].as_i64()?,
        rows,
        vols,
        done,
    })
}

/// The body's bytes and the sha256 of the body without `at`, `rev` and `clock_at`.
fn encode(f: &Published, instance_id: &str) -> (Vec<u8>, String) {
    let body = f.body(instance_id);
    let mut stable = body.clone();
    for k in ["at", "rev", "clock_at"] {
        stable.remove(k);
    }
    let sha = hex::encode(Sha256::digest(
        serde_json::to_vec(&Value::Object(stable)).unwrap_or_default(),
    ));
    (
        serde_json::to_vec(&Value::Object(body)).unwrap_or_default(),
        sha,
    )
}

/// A published job is no longer owed (the settle rule is asked only then).
fn gone(base: Option<&Published>, live: Option<&SeriesLive>) -> bool {
    base.is_some_and(|p| {
        p.vols.iter().any(|(stem, jobs)| {
            jobs.iter().any(|(row, _)| {
                !live
                    .and_then(|l| l.vols.get(stem))
                    .is_some_and(|l| l.iter().any(|j| j.row == *row))
            })
        })
    })
}

impl State {
    /// What the store has, or will have once what is staged lands, per series.
    fn view(&self) -> Vec<(&String, &Published)> {
        self.landed
            .iter()
            .filter(|(k, _)| !self.staged.contains_key(*k))
            .map(|(k, l)| (k, &l.file))
            .chain(self.staged.iter().map(|(k, s)| (k, &s.file)))
            .collect()
    }

    fn base(&self, key: &str) -> Option<&Published> {
        self.staged
            .get(key)
            .map(|s| &s.file)
            .or_else(|| self.landed.get(key).map(|l| &l.file))
    }
}

impl SeriesQueueSink {
    /// A sink writing every `window` at most `cap` files, its persisted state loaded
    /// from `db`.
    pub(crate) fn new(
        window: Duration,
        cap: usize,
        timing: SeriesQueueTiming,
        instance_id: String,
        lease: bool,
        db: Arc<StateDb>,
    ) -> SeriesQueueSink {
        let mut landed = BTreeMap::new();
        for row in db.series_queue_rows() {
            match published_of(&row) {
                Some(file) => {
                    landed.insert(row.series_fold.clone(), Landed { file });
                }
                None => {
                    tracing::warn!(series = %row.folder, "series queue: an unreadable row is dropped");
                    let _ = db.series_queue_del(&row.series_fold);
                }
            }
        }
        let clock = db.meta_get(META_CLOCK).and_then(|j| {
            let v: Value = serde_json::from_str(&j).ok()?;
            Clock::from_json(&v["knots"], v["at"].as_i64()?)
        });
        let rev = db
            .meta_get(META_REV)
            .and_then(|r| r.parse().ok())
            .unwrap_or(0);
        SeriesQueueSink {
            window,
            cap,
            timing,
            instance_id,
            lease,
            db,
            source: Mutex::new(None),
            outcomes: Mutex::new(Vec::new()),
            throttles: AtomicU32::new(0),
            wake: Notify::new(),
            st: Mutex::new(State {
                clock,
                prepared: None,
                judge_drift: false,
                rev,
                landed,
                staged: BTreeMap::new(),
                backlog: Backlog::default(),
                drifted: BTreeSet::new(),
                forced: BTreeSet::new(),
                budget: Budget::new(cap),
                snap: None,
                last_tick: None,
                last_flush: None,
                last_flush_ms: None,
                last_refit: None,
                retry_at: None,
                phase: Phase::Starting,
                marker_hold: None,
                early: None,
                last_early: None,
                replay_checked: false,
                blocked: BTreeSet::new(),
            }),
            fence: OnceLock::new(),
            heartbeat: OnceLock::new(),
        }
    }

    /// The window W.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// The cap B.
    pub fn cap(&self) -> usize {
        self.cap
    }

    pub(crate) fn set_source(&self, src: Arc<dyn SeriesQueueSource>) {
        *self.source.lock() = Some(src);
        // A new source is asked at once.
        self.st.lock().retry_at = None;
        self.wake.notify_one();
        // A marker written before there was a queue carries no block yet.
        if let Some(hb) = self.heartbeat.get() {
            hb.request();
        }
    }

    pub(crate) fn set_lease(&self, fence: Fence, heartbeat: HeartbeatRequester) {
        let _ = self.fence.set(fence);
        let _ = self.heartbeat.set(heartbeat);
    }

    /// A series op ended: committed (`ok`), or not.
    pub(crate) fn outcome(&self, op: u64, ok: bool) {
        self.outcomes.lock().push((op, ok));
        self.wake.notify_one();
    }

    /// The provider pushed back on a series op.
    pub(crate) fn throttled(&self) {
        self.throttles.fetch_add(1, Ordering::Relaxed);
    }

    /// What committed for series `fold_key`, if anything.
    pub fn landed(&self, fold_key: &str) -> Option<Published> {
        self.st.lock().landed.get(fold_key).map(|l| l.file.clone())
    }

    /// How many writes and deletes wait for a window.
    pub fn backlog_len(&self) -> usize {
        self.st.lock().backlog.len()
    }

    /// The last revision handed out.
    pub fn rev(&self) -> u64 {
        self.st.lock().rev
    }

    /// The clock in use.
    pub fn clock(&self) -> Option<Clock> {
        self.st.lock().clock.clone()
    }

    /// What the admin's storage status shows of the sink: `state` (why it writes
    /// nothing yet, or whether writes are on their way), `files` (the series files it
    /// wrote that are in the store), `staged` (writes on their way), `backlog` (writes
    /// and deletes waiting for a window) and `last_flush_at` (when the last window's
    /// burst ran, Unix seconds; `null` before the first).
    ///
    /// `state`: `no_source` (no OCR queue to write from yet), `starting`, `adopting`
    /// (the store's first adoption runs), `fenced` (this Bunko does not hold the
    /// lease, or is still claiming it), `waiting_for_<what>` (the queue has no answer
    /// yet: `waiting_for_first_walk` until the scheduler's first walk of the library is
    /// over), `writing` or `idle`.
    pub fn status_json(&self) -> Value {
        let source = self.source.lock().is_some();
        let st = self.st.lock();
        let state = match st.phase {
            _ if !source => "no_source".to_string(),
            Phase::Starting => "starting".into(),
            Phase::Adopting => "adopting".into(),
            Phase::Fenced => "fenced".into(),
            Phase::Waiting(what) => format!("waiting_for_{}", what.unwrap_or("queue")),
            Phase::Ready if !st.backlog.is_empty() || !st.staged.is_empty() => "writing".into(),
            Phase::Ready => "idle".into(),
        };
        json!({
            "state": state,
            "files": st.landed.len(),
            "staged": st.staged.len(),
            "backlog": st.backlog.len(),
            "last_flush_at": st.last_flush_ms.map(|ms| ms as f64 / 1000.0),
        })
    }

    /// The store's clock against this machine's, seconds (0 without a lease).
    fn offset_s(&self) -> f64 {
        self.fence
            .get()
            .and_then(Fence::clock_offset_ms)
            .unwrap_or(0) as f64
            / 1000.0
    }

    /// A fresh snapshot on the store's clock.
    async fn fresh_snapshot(&self) -> Option<Snapshot> {
        let src = self.source.lock().clone()?;
        let snap = tokio::time::timeout(SNAPSHOT_WAIT, src.snapshot())
            .await
            .ok()??;
        Some(snap.shifted(self.offset_s()))
    }

    fn persist_clock(&self, c: &Clock) {
        let v = json!({"knots": c.knots_json(), "at": c.at});
        if let Err(e) = self.db.meta_set(META_CLOCK, &v.to_string()) {
            tracing::warn!("series queue: the clock was not saved: {e}");
        }
    }

    /// Switch to `clock` (confirmed), and judge drift at the next look.
    fn switch_clock(&self, st: &mut State, clock: Clock) {
        if st.clock.as_ref() != Some(&clock) {
            self.persist_clock(&clock);
            st.clock = Some(clock);
            st.judge_drift = true;
        }
    }

    /// The clock in use, the identity from `now` before the first.
    fn clock_in(&self, st: &mut State, now: f64) -> Clock {
        if let Some(c) = &st.clock {
            return c.clone();
        }
        let c = Clock::identity(now.floor() as i64);
        self.persist_clock(&c);
        st.clock = Some(c.clone());
        c
    }

    /// A refit (§6.2) of what the store has: None while the queue is held, or with
    /// fewer than two points.
    fn fit(st: &State, snap: &Snapshot) -> Option<Clock> {
        if snap.held.is_some() {
            return None;
        }
        let (points, span) = fit_points(st.view(), snap);
        Clock::fit(&points, span?, snap.now)
    }

    /// Bring outcomes in: what committed is persisted, what failed is marked again.
    fn apply_outcomes(&self) {
        let outs = std::mem::take(&mut *self.outcomes.lock());
        if outs.is_empty() {
            return;
        }
        let mut st = self.st.lock();
        let now = st.snap.as_ref().map_or(0.0, |s| s.now);
        for (op, ok) in outs {
            let Some(key) = st
                .staged
                .iter()
                .find(|(_, s)| s.op == op)
                .map(|(k, _)| k.clone())
            else {
                continue;
            };
            let Some(s) = st.staged.remove(&key) else {
                continue;
            };
            if ok {
                let (_, sha) = encode(&s.file, &self.instance_id);
                if let Err(e) = self.db.series_queue_put(&row_of(&key, &s.file, sha)) {
                    tracing::warn!(series = %s.file.folder, "series queue: not saved: {e}");
                }
                st.landed.insert(key, Landed { file: s.file });
            } else {
                st.backlog
                    .mark(Task::Write(key.clone()), s.priority, now, None);
                if s.forced {
                    st.forced.insert(key);
                }
            }
        }
    }

    /// How long the loop may sleep.
    fn next_wait(&self) -> Duration {
        let tick = self.window.min(TICK_MAX);
        let st = self.st.lock();
        if let Some(at) = st.retry_at {
            // Nothing to do before the source answers.
            return at
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(10));
        }
        let mut wait = st
            .last_tick
            .map_or(Duration::ZERO, |t| tick.saturating_sub(t.elapsed()));
        if let Some((_, at)) = &st.early {
            wait = wait.min(at.saturating_duration_since(Instant::now()));
        }
        if let Some(t) = st.last_flush
            && !st.backlog.is_empty()
        {
            wait = wait.min(self.window.saturating_sub(t.elapsed()));
        }
        wait.max(Duration::from_millis(10))
    }

    /// Ask the lease for an early heartbeat when the hold changed and stayed (§6.8).
    fn early_heartbeat(&self, st: &mut State, snap: &Snapshot) {
        let Some(hb) = self.heartbeat.get() else {
            return;
        };
        let Some(marker) = st.marker_hold.clone() else {
            return;
        };
        let sig = hold_sig(snap);
        if sig == marker {
            st.early = None;
            return;
        }
        let now = Instant::now();
        let due = match &st.early {
            Some((s, at)) if *s == sig => *at,
            _ => {
                let unhold = marker.0.is_some() && sig.0.is_none();
                let d = if unhold {
                    self.timing.unhold_debounce
                } else {
                    self.timing.hold_debounce
                };
                now + d
            }
        };
        let due = match st.last_early {
            Some(t) => due.max(t + self.timing.early_gap),
            None => due,
        };
        if now >= due {
            tracing::debug!("series queue: the hold changed; asking for an early heartbeat");
            hb.request();
            st.last_early = Some(now);
            st.early = None;
        } else {
            st.early = Some((sig, due));
        }
    }

    /// One look: outcomes, the index, the dirty test, the early heartbeat, and a burst
    /// of writes when a window has passed.
    ///
    /// `false` when the sink may not write now (no source yet, not adopted, the fence
    /// shut: another Bunko holds the lease): the loop looks again soon.
    pub(crate) async fn tick<B: Backend>(&self, core: &Arc<Core<B>>) -> bool {
        self.apply_outcomes();
        let shut = if !core.adopted() {
            Some(Phase::Adopting)
        } else if !core.gate_open() {
            Some(Phase::Fenced)
        } else {
            None
        };
        if let Some(phase) = shut {
            self.st.lock().phase = phase;
            return false;
        }
        let Some(src) = self.source.lock().clone() else {
            return false;
        };
        let tick = self.window.min(TICK_MAX);
        let due = {
            let st = self.st.lock();
            let now = Instant::now();
            st.retry_at.is_none_or(|t| now >= t)
                && (st.judge_drift
                    // The first look since the fence opened (or ever).
                    || matches!(st.phase, Phase::Starting | Phase::Adopting | Phase::Fenced)
                    || st.last_tick.is_none_or(|t| t.elapsed() >= tick)
                    || st.early.as_ref().is_some_and(|(_, at)| now >= *at)
                    || (!st.backlog.is_empty()
                        && st.last_flush.is_none_or(|t| t.elapsed() >= self.window)))
        };
        if !due {
            return true;
        }
        let Some(snap) = self.fresh_snapshot().await else {
            // No answer (the server's queue, until its first walk of the library is
            // over): asked again in a second, not at once.
            let mut st = self.st.lock();
            st.retry_at = Some(Instant::now() + SNAPSHOT_RETRY);
            st.phase = Phase::Waiting(src.waiting_for());
            return true;
        };
        {
            let mut st = self.st.lock();
            st.retry_at = None;
            st.phase = Phase::Ready;
        }
        let snap = Arc::new(snap);
        // What the outbox and the index say, read before the sink's lock.
        let (busy, replayed) = {
            let ops = core.ops.lock();
            let mut busy = BTreeSet::new();
            let mut replayed = BTreeSet::new();
            for o in ops.values().filter(|o| o.state.live()) {
                let Some(first) = o.dest.segments().first() else {
                    continue;
                };
                if is_series_queue_op(o) {
                    replayed.insert((fold(first), o.id));
                } else if o.durability == bunko_store::Durability::Deferred
                    && matches!(
                        o.origin,
                        bunko_store::Origin::Ocr { .. } | bunko_store::Origin::Compiler
                    )
                    && o.dest.segments().len() >= 2
                {
                    busy.insert(fold(first));
                }
            }
            (busy, replayed)
        };
        let tracked: Vec<String> = {
            let st = self.st.lock();
            st.landed.keys().chain(st.staged.keys()).cloned().collect()
        };
        let (files, leftovers, bare) = {
            let idx = core.index.lock();
            let files = idx.series_level_files(&fold(SERIES_QUEUE_FILE));
            let leftovers: Vec<String> = idx
                .variants(ROOT_EID, &fold(ROOT_QUEUE_FILE))
                .into_iter()
                .filter(|e| !e.is_dir && e.pending_op.is_none())
                .map(|e| e.name)
                .collect();
            // Series whose folder holds no archive any more (every volume moved out
            // or deleted): what a file there names, done or queued, no reader has a
            // volume of.
            let bare: BTreeSet<String> = files
                .iter()
                .filter_map(|(rel, _)| rel.segments().first().map(|f| fold(f)))
                .chain(snap.finished.keys().cloned())
                .chain(tracked)
                .collect::<BTreeSet<String>>()
                .into_iter()
                .filter(|key| !idx.folder_holds_archive(key))
                .collect();
            (files, leftovers, bare)
        };
        let mut settled: BTreeMap<String, bool> = BTreeMap::new();
        {
            let mut st = self.st.lock();
            st.last_tick = Some(Instant::now());
            st.snap = Some(snap.clone());
            let clock = self.clock_in(&mut st, snap.now);
            if !st.replay_checked {
                // A write a previous run staged still lands; what it holds is not
                // known here, so its series is written again (it supersedes the old
                // op while that waits).
                for (key, op) in &replayed {
                    if st.staged.get(key).is_none_or(|s| s.op != *op) {
                        st.forced.insert(key.clone());
                    }
                }
                st.replay_checked = true;
            }
            self.scan(
                &mut st,
                &snap,
                &files,
                &leftovers,
                &|pid: &str, cver: &str| core.state.is_own(pid, cver),
            );
            // Without a lease, the clock is refitted on its own period.
            if !self.lease
                && st
                    .last_refit
                    .is_none_or(|t| t.elapsed() >= self.timing.refit_every)
            {
                st.last_refit = Some(Instant::now());
                if let Some(c) = Self::fit(&st, &snap) {
                    self.switch_clock(&mut st, c);
                }
            }
            let clock = st.clock.clone().unwrap_or(clock);
            let drift = std::mem::take(&mut st.judge_drift);
            let keys: BTreeSet<String> = snap
                .series
                .keys()
                .chain(snap.finished.keys())
                .chain(st.landed.keys())
                .chain(st.staged.keys())
                .cloned()
                .collect();
            for key in keys {
                let live = snap.series.get(&key);
                let mut base = st.base(&key).cloned();
                let folder = live
                    .map(|l| l.folder.clone())
                    .or_else(|| base.as_ref().map(|b| b.folder.clone()))
                    .or_else(|| snap.finished.get(&key).map(|(f, _)| f.clone()))
                    .unwrap_or_default();
                let calm = !busy.contains(&key) && !src.regeneration_pending(&folder);
                settled.insert(key.clone(), calm);
                // Owing nothing, with no archive left in its folder: its file is never
                // written again, only deleted, its `done` tail with it.
                let vacated = live.is_none() && bare.contains(&key);
                if calm
                    && !vacated
                    && let Some(b) = snap.unshown_finishes(&key, base.as_ref(), &clock)
                {
                    // Finished between two looks, never shown: its `done` entry.
                    base = Some(b);
                }
                let is_settled = !gone(base.as_ref(), live) || calm;
                let mut class = if vacated {
                    None
                } else {
                    classify(base.as_ref(), live, &snap, is_settled, &clock, drift)
                };
                if st.forced.contains(&key) {
                    if !vacated
                        && (live.is_some() || base.as_ref().is_some_and(|b| !b.expired(snap.now)))
                    {
                        class = Some(Class::Hard);
                    } else {
                        st.forced.remove(&key);
                    }
                }
                let task = Task::Write(key.clone());
                match class {
                    Some(c) => {
                        st.backlog.unmark(&Task::Expire(key.clone()));
                        if c == Class::Drift {
                            st.drifted.insert(key.clone());
                        }
                        st.backlog.mark(
                            task,
                            c.into(),
                            snap.now,
                            live.and_then(SeriesLive::soonest),
                        );
                    }
                    None => {
                        let keep = !vacated && !drift && st.drifted.contains(&key);
                        if !keep {
                            st.backlog.unmark(&task);
                            st.drifted.remove(&key);
                        }
                        let expired = live.is_none()
                            && !st.staged.contains_key(&key)
                            && st
                                .landed
                                .get(&key)
                                .is_some_and(|l| vacated || l.file.expired(snap.now));
                        if expired {
                            st.backlog.mark(
                                Task::Expire(key.clone()),
                                Priority::P3,
                                snap.now,
                                None,
                            );
                        }
                    }
                }
            }
            self.early_heartbeat(&mut st, &snap);
        }
        let flush = {
            let st = self.st.lock();
            !st.backlog.is_empty() && st.last_flush.is_none_or(|t| t.elapsed() >= self.window)
        };
        if flush {
            self.flush(core, &snap, &settled, &bare).await;
        }
        true
    }

    /// What the index shows of series files (§6.8, §10): a tracked file that is not
    /// this sink's version any more (another holder wrote it, a user replaced it) or is
    /// gone is rewritten at P0; a stray spelling, a file nobody tracks where nothing
    /// is owed, and the root leftover are deleted.
    fn scan(
        &self,
        st: &mut State,
        snap: &Snapshot,
        files: &[(RelPath, crate::index::Entry)],
        leftovers: &[String],
        own: &dyn Fn(&str, &str) -> bool,
    ) {
        let now = snap.now;
        let mut seen: BTreeSet<String> = BTreeSet::new();
        st.blocked.clear();
        for (rel, e) in files {
            let Some(folder) = rel.segments().first() else {
                continue;
            };
            let key = fold(folder);
            if e.pending_op.is_some() {
                // A write of ours on its way.
                seen.insert(key);
                continue;
            }
            if e.name != SERIES_QUEUE_FILE {
                // Another spelling: deleted (§3). Where it is the folder's only copy it
                // stands in the way of the file's write, so it goes first.
                let alone = !files.iter().any(|(r, x)| {
                    r.segments().first() == Some(folder) && x.name == SERIES_QUEUE_FILE
                });
                let p = if alone && snap.series.contains_key(&key) {
                    Priority::P0
                } else {
                    Priority::P3
                };
                st.backlog.mark(Task::Stray(rel.to_string()), p, now, None);
                if alone {
                    st.blocked.insert(key);
                }
                continue;
            }
            seen.insert(key.clone());
            let mine = match (&e.winner_pid, &e.cver) {
                (Some(p), Some(c)) => own(p, c),
                _ => false,
            };
            let tracked = st.landed.contains_key(&key) || st.staged.contains_key(&key);
            if tracked {
                if !mine && !st.staged.contains_key(&key) {
                    st.forced.insert(key);
                }
            } else if !snap.series.contains_key(&key) {
                // Nothing owed, nothing of ours: another holder's leftover (or ours,
                // its state lost).
                st.backlog
                    .mark(Task::Stray(rel.to_string()), Priority::P3, now, None);
            }
        }
        // A file this sink landed is gone (a user or a sync tool deleted it).
        let missing: Vec<String> = st
            .landed
            .keys()
            .filter(|k| !seen.contains(*k) && !st.staged.contains_key(*k))
            .cloned()
            .collect();
        for key in missing {
            let keep = st
                .landed
                .get(&key)
                .is_some_and(|l| !l.file.vols.is_empty() || !l.file.expired(now));
            if keep {
                st.forced.insert(key);
            } else {
                st.landed.remove(&key);
                let _ = self.db.series_queue_del(&key);
            }
        }
        for name in leftovers {
            st.backlog
                .mark(Task::Stray(name.clone()), Priority::P3, now, None);
        }
    }

    /// The folder a series' file goes in, as the store spells it (the index's case
    /// winner among the folders folding to it).
    fn folder_in_store<B: Backend>(core: &Core<B>, folder: &str) -> Option<String> {
        core.index
            .lock()
            .variants(ROOT_EID, &fold(folder))
            .into_iter()
            .find(|e| e.is_dir)
            .map(|e| e.name)
    }

    /// One window's burst: at most B tasks, in backlog order (§6.4). `calm`: per
    /// series, no OCR or Compiler write of it is left in the outbox and no recompile
    /// is pending for it (the settle rule, §5.3).
    async fn flush<B: Backend>(
        &self,
        core: &Arc<Core<B>>,
        snap: &Arc<Snapshot>,
        settled: &BTreeMap<String, bool>,
        bare: &BTreeSet<String>,
    ) {
        let throttles = self.throttles.swap(0, Ordering::Relaxed);
        let (order, burst) = {
            let mut st = self.st.lock();
            for _ in 0..throttles.min(4) {
                st.budget.throttled();
            }
            let burst = st.budget.window();
            st.backlog.age(snap.now, self.window.as_secs_f64());
            st.last_flush = Some(Instant::now());
            st.last_flush_ms = Some(now_ms());
            (st.backlog.order(), burst)
        };
        let mut used = 0;
        for task in order {
            if used >= burst {
                break;
            }
            match &task {
                Task::Write(key) => {
                    if self.st.lock().blocked.contains(key) {
                        continue;
                    }
                    let staged = {
                        let mut st = self.st.lock();
                        let priority = st.backlog.get(&task).map_or(Priority::P0, |m| m.priority);
                        let live = snap.series.get(key);
                        let calm = settled.get(key).copied().unwrap_or(true);
                        let clock = self.clock_in(&mut st, snap.now);
                        let mut base = st.base(key).cloned();
                        if calm && let Some(b) = snap.unshown_finishes(key, base.as_ref(), &clock) {
                            base = Some(b);
                        }
                        let is_settled = !gone(base.as_ref(), live) || calm;
                        let next_rev = (st.rev + 1).max(u64::try_from(now_ms()).unwrap_or(0));
                        let file = render(base.as_ref(), live, snap, is_settled, &clock, next_rev);
                        (file, priority, st.forced.contains(key), next_rev)
                    };
                    let (file, priority, forced, rev) = staged;
                    let Some(dir) = Self::folder_in_store(core, &file.folder) else {
                        let mut st = self.st.lock();
                        if !snap.series.contains_key(key) && !st.staged.contains_key(key) {
                            // The folder is gone and nothing is owed there: forgotten.
                            st.backlog.unmark(&task);
                            st.forced.remove(key);
                            st.drifted.remove(key);
                            if st.landed.remove(key).is_some() {
                                let _ = self.db.series_queue_del(key);
                            }
                        }
                        // Otherwise the folder is not in the store yet: the mark stays.
                        continue;
                    };
                    let (bytes, _) = encode(&file, &self.instance_id);
                    let c = core.clone();
                    let r = tokio::task::block_in_place(|| c.stage_series_queue(&dir, &bytes));
                    let mut st = self.st.lock();
                    st.rev = st.rev.max(rev);
                    let _ = self.db.meta_set(META_REV, &st.rev.to_string());
                    match r {
                        Ok(op) => {
                            st.backlog.unmark(&task);
                            st.forced.remove(key);
                            st.drifted.remove(key);
                            st.staged.insert(
                                key.clone(),
                                Staged {
                                    op,
                                    file,
                                    priority,
                                    forced,
                                },
                            );
                            used += 1;
                        }
                        Err(e) => {
                            tracing::warn!(series = %dir, "series queue: not staged: {e}");
                        }
                    }
                }
                Task::Expire(key) => {
                    let folder = {
                        let mut st = self.st.lock();
                        // Still owing nothing, nothing on its way, its tail expired or
                        // its folder left with no archive?
                        let retire = |l: &Landed| bare.contains(key) || l.file.expired(snap.now);
                        let still = !snap.series.contains_key(key)
                            && !st.staged.contains_key(key)
                            && st.landed.get(key).is_some_and(retire);
                        if !still {
                            st.backlog.unmark(&task);
                            continue;
                        }
                        st.landed.get(key).map(|l| l.file.folder.clone())
                    };
                    let rel = folder
                        .and_then(|f| Self::folder_in_store(core, &f))
                        .and_then(|d| RelPath::from_segments([d, SERIES_QUEUE_FILE.to_string()]));
                    let r = match &rel {
                        Some(rel) => core.delete_status_file(rel).await,
                        None => Ok(()),
                    };
                    used += 1;
                    let mut st = self.st.lock();
                    match r {
                        Ok(()) => {
                            st.backlog.unmark(&task);
                            if st
                                .landed
                                .get(key)
                                .is_some_and(|l| bare.contains(key) || l.file.expired(snap.now))
                            {
                                st.landed.remove(key);
                                let _ = self.db.series_queue_del(key);
                            }
                        }
                        Err(e) => {
                            tracing::info!("series queue: an expired file stays for now: {e}")
                        }
                    }
                }
                Task::Stray(path) => {
                    let r = match RelPath::parse(path) {
                        Some(rel) => core.delete_status_file(&rel).await,
                        None => Ok(()),
                    };
                    used += 1;
                    match r {
                        Ok(()) => {
                            tracing::info!(%path, "series queue: deleted a stray queue file");
                            self.st.lock().backlog.unmark(&task);
                        }
                        Err(e) => {
                            tracing::info!(%path, "series queue: a stray queue file stays for now: {e}")
                        }
                    }
                }
            }
        }
    }
}

impl MarkerBlock for SeriesQueueSink {
    fn prepare(&self, heartbeat: bool) -> BlockFuture<'_> {
        Box::pin(async move {
            if self.source.lock().is_none() {
                return None;
            }
            let fresh = self.fresh_snapshot().await;
            let mut st = self.st.lock();
            if let Some(s) = fresh {
                st.snap = Some(Arc::new(s));
            }
            let snap = st.snap.clone();
            let now = snap
                .as_ref()
                .map_or_else(|| now_ms() as f64 / 1000.0 + self.offset_s(), |s| s.now);
            let mut clock = self.clock_in(&mut st, now);
            if heartbeat
                && let Some(s) = &snap
                && let Some(c) = Self::fit(&st, s)
            {
                clock = c;
            }
            st.prepared = Some((clock.clone(), snap.as_deref().map(hold_sig)));
            Some(marker_block(
                snap.as_deref(),
                &clock,
                self.window.as_secs().max(1),
            ))
        })
    }

    fn held(&self) {
        {
            let mut st = self.st.lock();
            if let Some((clock, sig)) = st.prepared.take() {
                self.switch_clock(&mut st, clock);
                st.marker_hold = Some(sig.unwrap_or_default());
            }
        }
        self.wake.notify_one();
    }
}

/// Run the sink until the store goes: a look every `min(W, 30 s)` (sooner for an early
/// heartbeat due, or an op that ended).
pub(crate) async fn run<B: Backend>(core: Weak<Core<B>>, sink: Arc<SeriesQueueSink>) {
    /// While the sink may not write, it looks again this often.
    const IDLE: Duration = Duration::from_secs(1);
    let mut ready = true;
    loop {
        let wait = if ready {
            sink.next_wait()
        } else {
            IDLE.min(sink.next_wait().max(Duration::from_millis(100)))
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = sink.wake.notified() => {}
        }
        let Some(core) = core.upgrade() else { return };
        if core.crashed.load(Ordering::Acquire) {
            return;
        }
        ready = sink.tick(&core).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_round_trips() {
        let mut f = Published {
            folder: "Example Saga".into(),
            rev: 7,
            at: 1_794_398_520,
            clock_at: 1_794_398_400,
            rows: vec![RowDesc {
                kind: "ocr".into(),
                id: "main".into(),
                parts: json!([{"role": "recognizer", "name": "Main", "version": "1"}]),
            }],
            vols: BTreeMap::new(),
            done: BTreeMap::from([("Example Saga 01".to_string(), 1_794_395_000)]),
        };
        f.vols.insert(
            "Example Saga 02".into(),
            vec![("main".into(), Tuple::Running { t0: 10, e: None })],
        );
        f.vols.insert(
            "Example Saga 03".into(),
            vec![("main".into(), Tuple::Queued(Some(1_794_400_200)))],
        );
        f.vols.insert(
            "Example Saga 04".into(),
            vec![("main".into(), Tuple::Claimed)],
        );
        f.vols.insert(
            "Example Saga 05".into(),
            vec![("main".into(), Tuple::Queued(None))],
        );
        let row = row_of("example saga", &f, "sha".into());
        assert_eq!(published_of(&row), Some(f.clone()));
        // The content hash leaves out what never decides a write.
        let (_, a) = encode(&f, "id");
        let mut g = f.clone();
        g.at += 60;
        g.rev += 1;
        g.clock_at += 300;
        assert_eq!(encode(&g, "id").1, a);
        g.done.clear();
        assert_ne!(encode(&g, "id").1, a);
    }

    #[test]
    fn pacing_per_provider() {
        let base = BackendCaps {
            cas_create: true,
            cas_overwrite: true,
            atomic_replace: true,
            server_copy: true,
            range: true,
            pinned_read: true,
            push: false,
            hash_kind: Some(HashKind::Sha256),
            mtime_granularity_ns: 1,
            simple_upload_max: 1,
            chunk_multiple: 1,
            path_ids: false,
            duplicate_names: true,
        };
        assert_eq!(pacing(&base), (Duration::from_secs(120), 30));
        let onedrive = BackendCaps {
            hash_kind: Some(HashKind::QuickXor),
            ..base.clone()
        };
        assert_eq!(pacing(&onedrive), (Duration::from_secs(120), 15));
        let mega = BackendCaps {
            push: true,
            atomic_replace: false,
            ..base.clone()
        };
        assert_eq!(pacing(&mega), (Duration::from_secs(300), 20));
        let webdav = BackendCaps {
            path_ids: true,
            hash_kind: None,
            ..base.clone()
        };
        assert_eq!(pacing(&webdav), (Duration::from_secs(30), 20));
        let folder = BackendCaps {
            path_ids: true,
            push: true,
            hash_kind: None,
            ..base
        };
        assert_eq!(pacing(&folder), (Duration::from_secs(15), 100));
    }
}
