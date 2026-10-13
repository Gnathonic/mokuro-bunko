//! The change-feed engine (spec §5.3, §5.4; D19, D20, D21).
//!
//! [`Feed`] keeps the [`Index`] in step with the provider: INIT → LOCATE_ROOTS →
//! resume the persisted cursor or FULL_SYNC → `Rescan` → STEADY. It translates what
//! the index reports into [`WatchEvent`]s (absolute paths under the synthetic store
//! root) for the registered sinks, and replays cloud-side moves, deletes and sidecar
//! overwrites to the server through [`CloudHooks`].
//!
//! **FULL_SYNC** (mark and sweep): the cursor is taken before enumerating; every
//! listed node is upserted with a new generation, in chunks of
//! [`FeedConfig::chunk`] nodes that commit (and emit their events) as the listing
//! streams in, so a large first sync never holds the tree in memory; older-generation
//! nodes are swept only after a complete enumeration, skipping what
//! [`FeedState::keep_on_sweep`] keeps.
//!
//! **STEADY**: each change page and its new cursor apply in one transaction
//! (delivery is at least once, idempotent by pid). Within a page only the last item
//! per pid counts, and deletes run at the end of the page, so a delete and re-create
//! of one pid folds into a move. A folder that appears inside the tree with an
//! unknown pid (moved in from outside) is listed (`list_all` of that folder) and its
//! subtree joins the page; path-id providers (WebDAV) report the subtree themselves.
//!
//! **Moves and deletes (D20)**: a move (same pid at a new path) runs
//! [`CloudHooks::moved`] at once. A delete writes a tombstone; [`CloudHooks::removed`]
//! runs when it expires, after `max(2 × poll interval, quiet)`. The same pid
//! reappearing before that is a move. [`FeedHandle::recently_removed`] answers for
//! tombstones younger than `quiet`. On path-id providers a move arrives as removals
//! of the old pids plus upserts of new ones: when, within one page, a removed
//! subtree's content (relative names, kinds, sizes, cvers) matches exactly one subtree
//! that appeared, it is reported as one `moved` hook; otherwise (cvers changed by the
//! move, or the two halves landed in different pages) it stays a delete plus an add.
//!
//! **Safety valve (D19)**: removals above [`FeedConfig::suspect_ratio`] of the
//! indexed nodes, in a FULL_SYNC sweep or within [`FeedConfig::suspect_window`] in
//! STEADY, or a root removed, renamed or moved, put the engine in
//! [`EngineState::Suspect`]. The removals are stashed, not applied, until
//! [`FeedHandle::confirm_suspect`] or two consecutive full re-lists agree on them.
//!
//! **Hook delivery**: hooks and foreign-JSON notifications are queued in
//! `index.sqlite` in the same transaction as the change that causes them and are
//! delivered after it commits, outside the index lock (at least once: a crash before
//! delivery replays them at the next start). Events are delivered after commit too;
//! a lost one is covered by the `Rescan` every start emits.
//!
//! Lock order: [`FeedState`] methods run with the index locked and must not lock it
//! themselves; [`CloudHooks`], the foreign-JSON callback and the sinks run unlocked.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bunko_store::{WatchEvent, WatchSink};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tokio::runtime::Handle;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::backend::{BErr, Backend, BackendCaps, ChangeItem, ChangePage, Cursor, Pid, RNode};
use crate::index::{Index, IndexChange, ROOT_EID, RelPath};
use crate::names::nfc;

/// Consecutive failures before the engine reports [`EngineState::Offline`].
const OFFLINE_AFTER: u32 = 3;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// The pause for a `Throttled` without `Retry-After`.
const THROTTLE_DEFAULT: Duration = Duration::from_secs(30);
/// The shortest wait between steady cycles.
const MIN_WAIT: Duration = Duration::from_secs(1);
/// Subtrees larger than this are never compared for a path-id move.
const SIG_CAP: usize = 20_000;

const HOOK_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS feed_hook(id INTEGER PRIMARY KEY, \
    kind TEXT NOT NULL, a TEXT NOT NULL, b TEXT NOT NULL DEFAULT '', c TEXT NOT NULL DEFAULT '', \
    is_dir INTEGER NOT NULL DEFAULT 0);";

const TOMB_LIVE: i64 = 0;
const TOMB_APPLIED: i64 = 1;
/// Another node took the path inside the grace period: no `removed` hook.
const TOMB_REPLACED: i64 = 2;

// ---- the server's side ----------------------------------------------------------

/// The answer to a foreign `series.json` submission (task 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmissionResult {
    /// The facts were merged.
    Applied,
    /// Not applied, with the reason (kept as a conflict row).
    Refused(String),
}

/// The DB hooks the server runs for changes made directly in the cloud (D20). Paths
/// are absolute, under the synthetic store root.
pub trait CloudHooks: Send + Sync {
    /// Same pid, new parent or name: runs immediately.
    fn moved(&self, from: &Path, to: &Path, is_dir: bool);
    /// A delete, after its tombstone expired.
    fn removed(&self, path: &Path, is_dir: bool);
    /// A foreign new version of a `.mokuro`, `.mokuro.gz` or `.<layer>.mokuro`.
    fn sidecar_overwritten(&self, path: &Path);
    /// Apply a foreign `series.json` (`apply_series_update`, task 8).
    fn series_submission(&self, series_dir: &Path, bytes: &[u8]) -> SubmissionResult;
    /// A foreign sidecar found at install time (task 8).
    fn ocr_discarded(&self, path: &Path);
    /// The series at `series_dir` has a facts row (D18). A `series.json` that lands
    /// in a folder without one is reported as foreign even when its bytes are
    /// Bunko's own (a reader's folder rename carries its facts).
    fn has_series_facts(&self, _series_dir: &Path) -> bool {
        true
    }
}

/// What the durable state (task 5) tells the engine. The default answers "nothing is
/// ours, nothing is pending".
pub trait FeedState: Send + Sync {
    /// Keep `pid` in a FULL_SYNC sweep although the listing did not show it (it has a
    /// pending op, or a ledger entry newer than the sync start).
    fn keep_on_sweep(&self, _pid: &str) -> bool {
        false
    }
    /// The upsert matches a ledger row exactly on `(pid, parent_pid, name, cver)`:
    /// Bunko's own write, already announced at publish (D21). `name` is NFC.
    fn is_own_echo(&self, _pid: &str, _parent_pid: Option<&str>, _name: &str, _cver: &str) -> bool {
        false
    }
    /// The removal of `pid` (last indexed at `cver`) is Bunko's own trash.
    fn is_own_delete(&self, _pid: &str, _cver: &str) -> bool {
        false
    }
}

/// The [`FeedState`] that knows of no own writes.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoFeedState;

impl FeedState for NoFeedState {}

/// Called with `(rel, pid, cver)` for a foreign `series.json` or `catalog.json`
/// version, or a `series.json` in a series folder without facts (task 8 persists it
/// as a submission).
pub type ForeignJsonFn = Arc<dyn Fn(&RelPath, &str, &str) + Send + Sync>;

/// Engine settings.
#[derive(Debug, Clone)]
pub struct FeedConfig {
    /// The synthetic store root that event and hook paths are joined under.
    pub root: PathBuf,
    /// The root folder name (`mokuro-reader`).
    pub folder: String,
    /// Create the root folder when none exists (otherwise wait for one).
    pub create_if_missing: bool,
    /// Poll interval while active (a bridge request, OCR running, reader activity).
    pub poll_active: Duration,
    /// Poll interval otherwise (push providers wait this long per long-poll).
    pub poll_idle: Duration,
    /// `quiet_s` (§5.4): the recently-removed window and the tombstone grace floor.
    pub quiet: Duration,
    /// D19: the share of indexed nodes whose removal makes the engine suspect.
    pub suspect_ratio: f64,
    /// D19: the STEADY window removals are summed over.
    pub suspect_window: Duration,
    /// Fewer removals than this never trip D19 (a small library losing one series).
    pub suspect_min_removals: u64,
    /// While suspect, how often the tree is listed again.
    pub suspect_relist: Duration,
    /// FULL_SYNC nodes per transaction.
    pub chunk: usize,
}

impl FeedConfig {
    /// The defaults: WebDAV cadence (60 s active, 300 s idle), `quiet` 120 s, D19 at
    /// 20 % over 10 min (at least 10 removals), re-list every 10 min while suspect.
    pub fn new(root: impl Into<PathBuf>, folder: impl Into<String>) -> FeedConfig {
        FeedConfig {
            root: root.into(),
            folder: folder.into(),
            create_if_missing: false,
            poll_active: Duration::from_secs(60),
            poll_idle: Duration::from_secs(300),
            quiet: Duration::from_secs(120),
            suspect_ratio: 0.20,
            suspect_window: Duration::from_secs(600),
            suspect_min_removals: 10,
            suspect_relist: Duration::from_secs(600),
            chunk: 512,
        }
    }
}

/// What the engine is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineState {
    /// Not started yet.
    Init,
    /// Looking for the root folder (or waiting for one to exist).
    LocateRoots,
    /// Enumerating the tree; `done` nodes so far.
    FullSync {
        done: u64,
        /// Unknown while listing (providers do not announce a count).
        total: Option<u64>,
    },
    /// Following the change feed.
    Steady,
    /// D19 tripped: `pending_removals` stashed removals, or a root changed.
    Suspect {
        pending_removals: u64,
        root_changed: bool,
    },
    /// Repeated transient failures; retrying with backoff.
    Offline,
    /// The provider refused the credentials; nothing is called until
    /// [`FeedHandle::resume`].
    NeedsReauth,
}

// ---- sinks and clock ------------------------------------------------------------

#[derive(Default)]
struct Sinks {
    next: AtomicU64,
    list: Mutex<Vec<(u64, WatchSink)>>,
}

impl Sinks {
    fn add(&self, s: WatchSink) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.list.lock().push((id, s));
        id
    }

    fn remove(&self, id: u64) {
        self.list.lock().retain(|(i, _)| *i != id);
    }

    fn emit(&self, evs: &[WatchEvent]) {
        if evs.is_empty() {
            return;
        }
        let list = self.list.lock().clone();
        for e in evs {
            for (_, s) in &list {
                s(e.clone());
            }
        }
    }
}

/// Unix ms that follows tokio's clock (so paused-time tests see tombstones age).
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

    fn now_ms(&self) -> i64 {
        self.sys0_ms + self.inst0.elapsed().as_millis() as i64
    }
}

fn ms(d: Duration) -> i64 {
    d.as_millis().min(i64::MAX as u128) as i64
}

// ---- name rules -----------------------------------------------------------------

fn last_seg(rel: &RelPath) -> &str {
    rel.segments().last().map_or("", String::as_str)
}

/// A main or layer sidecar: `.mokuro`, `.mokuro.gz`, `.<layer>.mokuro`.
fn is_sidecar(rel: &RelPath) -> bool {
    let n = last_seg(rel).to_ascii_lowercase();
    n.ends_with(".mokuro") || n.ends_with(".mokuro.gz")
}

fn is_series_json(rel: &RelPath) -> bool {
    last_seg(rel).eq_ignore_ascii_case("series.json")
}

fn is_metadata_json(rel: &RelPath) -> bool {
    is_series_json(rel) || last_seg(rel).eq_ignore_ascii_case("catalog.json")
}

fn parent_of(rel: &RelPath) -> RelPath {
    let s = rel.segments();
    RelPath::from_segments(s[..s.len().saturating_sub(1)].iter().cloned()).unwrap_or_default()
}

fn has_proper_ancestor(rel: &RelPath, set: &HashSet<String>) -> bool {
    let s = rel.segments();
    (1..s.len()).any(|k| set.contains(&s[..k].join("/")))
}

// ---- index helpers --------------------------------------------------------------

/// The library path a node sits at (not the merged root).
fn placed_path(idx: &Index, pid: &str) -> Option<RelPath> {
    let e = idx.entry_by_pid(pid)?;
    if e.eid == ROOT_EID {
        return None;
    }
    idx.path_of(e.eid)
}

/// A subtree's content, independent of its own name: (relative path, is_dir, size,
/// cver), sorted. `None` past [`SIG_CAP`].
type Sig = Vec<(String, bool, u64, Option<String>)>;

fn signature(idx: &Index, rel: &RelPath) -> Option<Sig> {
    let e = idx.lookup(rel)?;
    if !e.is_dir {
        return Some(vec![(String::new(), false, e.size, e.cver)]);
    }
    let mut out = Vec::new();
    let mut stack = vec![(e.eid, String::new())];
    while let Some((eid, prefix)) = stack.pop() {
        for c in idx.children(eid) {
            let p = if prefix.is_empty() {
                c.name.clone()
            } else {
                format!("{prefix}/{}", c.name)
            };
            if c.is_dir {
                stack.push((c.eid, p.clone()));
            }
            out.push((p, c.is_dir, c.size, c.cver));
            if out.len() > SIG_CAP {
                return None;
            }
        }
    }
    out.sort();
    Some(out)
}

fn enqueue(
    c: &Connection,
    kind: &str,
    a: &str,
    b: &str,
    cv: &str,
    is_dir: bool,
) -> rusqlite::Result<()> {
    c.prepare_cached("INSERT INTO feed_hook(kind, a, b, c, is_dir) VALUES(?1, ?2, ?3, ?4, ?5)")?
        .execute(params![kind, a, b, cv, is_dir as i64])?;
    Ok(())
}

fn tomb_put(
    c: &Connection,
    pid: &str,
    rel: &RelPath,
    is_dir: bool,
    at: i64,
) -> rusqlite::Result<()> {
    c.prepare_cached(
        "INSERT OR REPLACE INTO tombstone(pid, path, is_dir, removed_at, applied) VALUES(?1, ?2, ?3, ?4, ?5)",
    )?
    .execute(params![pid, rel.to_string(), is_dir as i64, at, TOMB_LIVE])?;
    Ok(())
}

/// Drop `pid`'s tombstone; its path if it had not expired.
fn tomb_take(c: &Connection, pid: &str) -> rusqlite::Result<Option<String>> {
    let path: Option<String> = c
        .prepare_cached("SELECT path FROM tombstone WHERE pid = ?1 AND applied = ?2")?
        .query_row(params![pid, TOMB_LIVE], |r| r.get(0))
        .optional()?;
    c.prepare_cached("DELETE FROM tombstone WHERE pid = ?1")?
        .execute([pid])?;
    Ok(path)
}

/// Another node (`pid`) took `rel` while a tombstone for it was live.
fn tomb_replaced(c: &Connection, pid: &str, rel: &RelPath) -> rusqlite::Result<bool> {
    let n = c
        .prepare_cached(
            "UPDATE tombstone SET applied = ?1 WHERE path = ?2 AND applied = ?3 AND pid != ?4",
        )?
        .execute(params![TOMB_REPLACED, rel.to_string(), TOMB_LIVE, pid])?;
    Ok(n > 0)
}

fn stash_save(idx: &Index, stash: &BTreeSet<Pid>) -> rusqlite::Result<()> {
    let v = serde_json::to_string(stash).expect("a string set serialises");
    idx.meta_set("suspect_stash", &v)
}

/// One removal planned for the end of a batch.
struct Del {
    pid: Pid,
    parent: Option<Pid>,
    cver: String,
    path: Option<RelPath>,
    is_dir: bool,
    echo: bool,
    /// The index (in the plan) of the removed subtree's top.
    top: usize,
    /// Nodes this top takes with it (tops only).
    size: u64,
    sig: Option<Sig>,
    /// A path-id move: where the subtree reappeared.
    moved_to: Option<RelPath>,
}

// ---- the engine core ------------------------------------------------------------

struct Core<B: Backend> {
    backend: Arc<B>,
    caps: BackendCaps,
    index: Arc<Mutex<Index>>,
    cfg: FeedConfig,
    sinks: Arc<Sinks>,
    hooks: Option<Arc<dyn CloudHooks>>,
    fstate: Arc<dyn FeedState>,
    on_json: Option<ForeignJsonFn>,
    clock: Clock,
    state_tx: watch::Sender<EngineState>,
    active: Arc<AtomicBool>,
}

impl<B: Backend> Core<B> {
    fn abs(&self, rel: &RelPath) -> PathBuf {
        let mut p = self.cfg.root.clone();
        for s in rel.segments() {
            p.push(s);
        }
        p
    }

    fn emit(&self, changes: &[IndexChange]) {
        let mut evs = Vec::with_capacity(changes.len());
        for c in changes {
            match c {
                IndexChange::Added { rel, is_dir } => evs.push(WatchEvent::Added {
                    path: self.abs(rel),
                    is_dir: *is_dir,
                }),
                IndexChange::Removed { rel, is_dir } => evs.push(WatchEvent::Removed {
                    path: self.abs(rel),
                    is_dir: *is_dir,
                }),
                IndexChange::Moved { from, to, is_dir } => {
                    evs.push(WatchEvent::Removed {
                        path: self.abs(from),
                        is_dir: *is_dir,
                    });
                    evs.push(WatchEvent::Added {
                        path: self.abs(to),
                        is_dir: *is_dir,
                    });
                }
                // What a local tmp+rename produces.
                IndexChange::ContentChanged { rel } => evs.push(WatchEvent::Added {
                    path: self.abs(rel),
                    is_dir: false,
                }),
            }
        }
        self.sinks.emit(&evs);
    }

    fn trips(&self, removals: u64, total: u64) -> bool {
        removals >= self.cfg.suspect_min_removals
            && removals as f64 > self.cfg.suspect_ratio * total as f64
    }

    /// A move hook, plus the facts check for any `series.json` it carried.
    #[allow(clippy::too_many_arguments)]
    fn queue_move(
        &self,
        idx: &Index,
        c: &Connection,
        from: &RelPath,
        to: &RelPath,
        is_dir: bool,
        pid: &str,
        cver: &str,
    ) -> rusqlite::Result<()> {
        enqueue(c, "moved", &from.to_string(), &to.to_string(), "", is_dir)?;
        if is_dir {
            self.series_checks_under(idx, c, to)
        } else if is_series_json(to) {
            enqueue(c, "series_check", &to.to_string(), pid, cver, false)
        } else {
            Ok(())
        }
    }

    fn series_checks_under(
        &self,
        idx: &Index,
        c: &Connection,
        dir: &RelPath,
    ) -> rusqlite::Result<()> {
        let Some(e) = idx.lookup(dir) else {
            return Ok(());
        };
        let mut stack = vec![(e.eid, dir.clone())];
        let mut seen = 0usize;
        while let Some((eid, rel)) = stack.pop() {
            for ch in idx.children(eid) {
                seen += 1;
                if seen > SIG_CAP {
                    return Ok(());
                }
                let Some(r) = rel.join(ch.name.clone()) else {
                    continue;
                };
                if ch.is_dir {
                    stack.push((ch.eid, r));
                } else if is_series_json(&r) {
                    let pid = ch.winner_pid.clone().unwrap_or_default();
                    let cver = ch.cver.clone().unwrap_or_default();
                    enqueue(c, "series_check", &r.to_string(), &pid, &cver, false)?;
                }
            }
        }
        Ok(())
    }

    /// Ingest one upserted node: index it, queue its hooks, collect its visible
    /// changes into `out` (own echoes of known nodes show nothing).
    fn apply_upsert(
        &self,
        idx: &Index,
        tx: &Transaction<'_>,
        n: &RNode,
        generation: u64,
        out: &mut Vec<IndexChange>,
    ) -> rusqlite::Result<()> {
        let pre_node = idx.node(&n.pid);
        let pre = placed_path(idx, &n.pid);
        let echo = self
            .fstate
            .is_own_echo(&n.pid, n.parent.as_deref(), &nfc(&n.name_raw), &n.cver);
        let changes = idx.upsert(tx, n, generation, self.caps.mtime_granularity_ns)?;
        let c: &Connection = tx;
        if echo && pre_node.is_some() {
            return Ok(());
        }
        let post = placed_path(idx, &n.pid);
        if echo {
            // A ledger echo the index did not know: it shows, but runs no hooks.
            if let Some(p) = &post
                && !n.is_dir
                && is_series_json(p)
            {
                enqueue(c, "series_check", &p.to_string(), &n.pid, &n.cver, false)?;
            }
            out.extend(changes);
            return Ok(());
        }
        match (&pre, &post) {
            (Some(a), Some(b)) if a != b && idx.lookup(a).is_none() => {
                self.queue_move(idx, c, a, b, n.is_dir, &n.pid, &n.cver)?;
            }
            (Some(a), None) if idx.lookup(a).is_none() => {
                // Moved out of the library: a delete as far as the library can tell.
                let was_dir = pre_node.as_ref().is_some_and(|p| p.is_dir);
                tomb_put(c, &n.pid, a, was_dir, self.clock.now_ms())?;
            }
            (None, Some(b)) => {
                if let Some(t) = tomb_take(c, &n.pid)? {
                    if t != b.to_string() {
                        if let Some(from) = RelPath::parse(&t) {
                            self.queue_move(idx, c, &from, b, n.is_dir, &n.pid, &n.cver)?;
                        }
                    } else if !n.is_dir && is_sidecar(b) {
                        enqueue(c, "sidecar", &b.to_string(), "", "", false)?;
                    }
                }
                if tomb_replaced(c, &n.pid, b)? && !n.is_dir && is_sidecar(b) {
                    enqueue(c, "sidecar", &b.to_string(), "", "", false)?;
                }
            }
            _ => {}
        }
        for ch in &changes {
            if let IndexChange::ContentChanged { rel } = ch
                && is_sidecar(rel)
            {
                enqueue(c, "sidecar", &rel.to_string(), "", "", false)?;
            }
        }
        if !n.is_dir
            && let Some(b) = &post
            && is_metadata_json(b)
            && changes.iter().any(|ch| match ch {
                IndexChange::Added { rel, .. } | IndexChange::ContentChanged { rel } => rel == b,
                IndexChange::Moved { to, .. } => to == b,
                IndexChange::Removed { .. } => false,
            })
        {
            enqueue(c, "json", &b.to_string(), &n.pid, &n.cver, false)?;
        }
        out.extend(changes);
        Ok(())
    }

    /// Plan the removal of `pids` (known ones only): group them by removed subtree,
    /// size each group, and on path-id providers match groups against subtrees that
    /// appeared in `batch` (moves).
    fn plan_deletes(
        &self,
        idx: &Index,
        pids: &[Pid],
        batch: &[IndexChange],
        match_moves: bool,
    ) -> Vec<Del> {
        let mut dels: Vec<Del> = Vec::new();
        let mut at: HashMap<Pid, usize> = HashMap::new();
        for pid in pids {
            if at.contains_key(pid) {
                continue;
            }
            let Some(node) = idx.node(pid) else { continue };
            at.insert(pid.clone(), dels.len());
            dels.push(Del {
                pid: pid.clone(),
                parent: node.parent.clone(),
                echo: self.fstate.is_own_delete(pid, &node.cver),
                cver: node.cver,
                path: placed_path(idx, pid),
                is_dir: node.is_dir,
                top: 0,
                size: 0,
                sig: None,
                moved_to: None,
            });
        }
        for i in 0..dels.len() {
            let mut j = i;
            for _ in 0..dels.len() {
                match dels[j].parent.as_ref().and_then(|p| at.get(p)) {
                    Some(&k) if k != j => j = k,
                    _ => break,
                }
            }
            dels[i].top = j;
        }
        for (i, d) in dels.iter_mut().enumerate() {
            if d.top != i {
                continue;
            }
            d.size = idx.subtree_size(&d.pid);
            if match_moves
                && !d.echo
                && let Some(p) = &d.path
            {
                d.sig = signature(idx, p);
            }
        }
        if match_moves {
            self.match_moves(idx, &mut dels, batch);
        }
        dels
    }

    fn match_moves(&self, idx: &Index, dels: &mut [Del], batch: &[IndexChange]) {
        let added_dirs: HashSet<String> = batch
            .iter()
            .filter_map(|c| match c {
                IndexChange::Added { rel, is_dir: true } => Some(rel.to_string()),
                _ => None,
            })
            .collect();
        let mut tops: Vec<(RelPath, bool, Option<Option<Sig>>, bool)> = batch
            .iter()
            .filter_map(|c| match c {
                IndexChange::Added { rel, is_dir } if !has_proper_ancestor(rel, &added_dirs) => {
                    Some((rel.clone(), *is_dir, None, false))
                }
                _ => None,
            })
            .collect();
        if tops.is_empty() {
            return;
        }
        for (i, d) in dels.iter_mut().enumerate() {
            if d.top != i {
                continue;
            }
            let Some(sig) = d.sig.as_ref() else { continue };
            let mut hit = None;
            let mut hits = 0;
            for (k, (rel, is_dir, cached, used)) in tops.iter_mut().enumerate() {
                if *used || *is_dir != d.is_dir {
                    continue;
                }
                let s = cached.get_or_insert_with(|| signature(idx, rel));
                if s.as_ref() == Some(sig) {
                    hits += 1;
                    hit = Some(k);
                }
            }
            if hits == 1
                && let Some(k) = hit
            {
                tops[k].3 = true;
                d.moved_to = Some(tops[k].0.clone());
            }
        }
    }

    /// Removals that count toward D19: the subtrees of foreign, unmatched groups.
    fn removal_count(dels: &[Del]) -> u64 {
        dels.iter()
            .enumerate()
            .filter(|(i, d)| d.top == *i && !d.echo && d.moved_to.is_none())
            .map(|(_, d)| d.size)
            .sum()
    }

    /// Apply a plan. With `stash`, foreign unmatched groups are left in the index and
    /// returned instead. Moves queue their hook; true deletes write tombstones (one
    /// per removed subtree).
    fn apply_deletes(
        &self,
        idx: &Index,
        tx: &Transaction<'_>,
        dels: &[Del],
        stash: bool,
        out: &mut Vec<IndexChange>,
    ) -> rusqlite::Result<Vec<Pid>> {
        let c: &Connection = tx;
        let mut stashed = Vec::new();
        let mut removed = vec![false; dels.len()];
        for (i, d) in dels.iter().enumerate() {
            let top = &dels[d.top];
            let echo = d.echo || top.echo;
            if stash && !echo && top.moved_to.is_none() {
                stashed.push(d.pid.clone());
                continue;
            }
            let ch = idx.remove(tx, &d.pid)?;
            removed[i] = true;
            if !echo {
                out.extend(ch);
            }
        }
        for (i, d) in dels.iter().enumerate() {
            if d.top == i
                && removed[i]
                && let (Some(from), Some(to)) = (&d.path, &d.moved_to)
            {
                self.queue_move(idx, c, from, to, d.is_dir, &d.pid, &d.cver)?;
            }
        }
        let mut cands: Vec<&Del> = dels
            .iter()
            .enumerate()
            .filter(|(i, d)| {
                let top = &dels[d.top];
                removed[*i]
                    && !(d.echo || top.echo)
                    && top.moved_to.is_none()
                    && d.path.as_ref().is_some_and(|p| idx.lookup(p).is_none())
            })
            .map(|(_, d)| d)
            .collect();
        cands.sort_by(|a, b| a.path.cmp(&b.path));
        let dirs: HashSet<String> = cands
            .iter()
            .filter(|d| d.is_dir)
            .filter_map(|d| d.path.as_ref().map(ToString::to_string))
            .collect();
        let now = self.clock.now_ms();
        for d in cands {
            let Some(p) = &d.path else { continue };
            if !has_proper_ancestor(p, &dirs) {
                tomb_put(c, &d.pid, p, d.is_dir, now)?;
            }
        }
        Ok(stashed)
    }

    /// Commit one FULL_SYNC chunk and deliver what it produced.
    fn ingest(&self, buf: &mut Vec<RNode>, generation: u64) -> rusqlite::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let mut out = Vec::new();
        {
            let idx = self.index.lock();
            let tx = idx.begin()?;
            for n in buf.drain(..) {
                if !n.trashed {
                    self.apply_upsert(&idx, &tx, &n, generation, &mut out)?;
                }
            }
            tx.commit()?;
        }
        self.emit(&out);
        self.drain_hooks();
        Ok(())
    }

    /// Run expired tombstones' `removed` hooks; purge old rows.
    fn expire_tombstones(&self, grace: Duration) -> rusqlite::Result<()> {
        let now = self.clock.now_ms();
        let cutoff = now - ms(grace);
        let quiet_cut = now - ms(self.cfg.quiet);
        {
            let idx = self.index.lock();
            let tx = idx.begin()?;
            let c: &Connection = &tx;
            let rows: Vec<(String, String, bool, i64)> = c
                .prepare_cached(
                    "SELECT pid, path, is_dir, removed_at FROM tombstone WHERE applied = ?1 ORDER BY path",
                )?
                .query_map([TOMB_LIVE], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?))
                })?
                .collect::<rusqlite::Result<_>>()?;
            let mut gone: Vec<String> = Vec::new();
            for (pid, path, is_dir, at) in &rows {
                let under = gone.iter().any(|d| path.starts_with(&format!("{d}/")));
                if !under && *at > cutoff {
                    continue;
                }
                c.prepare_cached("UPDATE tombstone SET applied = ?1 WHERE pid = ?2")?
                    .execute(params![TOMB_APPLIED, pid])?;
                if under {
                    continue;
                }
                let back = RelPath::parse(path)
                    .and_then(|r| idx.lookup(&r))
                    .is_some_and(|e| e.is_dir == *is_dir);
                if !back {
                    enqueue(c, "removed", path, "", "", *is_dir)?;
                    if *is_dir {
                        gone.push(path.clone());
                    }
                }
            }
            c.prepare_cached("DELETE FROM tombstone WHERE applied != ?1 AND removed_at < ?2")?
                .execute(params![TOMB_LIVE, quiet_cut])?;
            tx.commit()?;
        }
        self.drain_hooks();
        Ok(())
    }

    /// The oldest live tombstone's removal time (unix ms).
    fn oldest_tombstone(&self) -> Option<i64> {
        let idx = self.index.lock();
        idx.conn()
            .query_row(
                "SELECT MIN(removed_at) FROM tombstone WHERE applied = ?1",
                [TOMB_LIVE],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
    }

    /// Deliver queued hooks (outside the index lock), then drop them.
    fn drain_hooks(&self) {
        type Row = (i64, String, String, String, String, bool);
        let rows: Vec<Row> = {
            let idx = self.index.lock();
            let r = idx
                .conn()
                .prepare_cached("SELECT id, kind, a, b, c, is_dir FROM feed_hook ORDER BY id")
                .and_then(|mut s| {
                    s.query_map([], |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get::<_, i64>(5)? != 0,
                        ))
                    })?
                    .collect()
                });
            match r {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("feed hook queue: {e}");
                    return;
                }
            }
        };
        let Some(last) = rows.last().map(|r| r.0) else {
            return;
        };
        for (_, kind, a, b, cv, is_dir) in &rows {
            let Some(rel) = RelPath::parse(a) else {
                continue;
            };
            match kind.as_str() {
                "moved" => {
                    if let (Some(h), Some(to)) = (&self.hooks, RelPath::parse(b)) {
                        h.moved(&self.abs(&rel), &self.abs(&to), *is_dir);
                    }
                }
                "removed" => {
                    if let Some(h) = &self.hooks {
                        h.removed(&self.abs(&rel), *is_dir);
                    }
                }
                "sidecar" => {
                    if let Some(h) = &self.hooks {
                        h.sidecar_overwritten(&self.abs(&rel));
                    }
                }
                "json" => {
                    if let Some(f) = &self.on_json {
                        f(&rel, b, cv);
                    }
                }
                "series_check" => {
                    let facts = self
                        .hooks
                        .as_ref()
                        .is_none_or(|h| h.has_series_facts(&self.abs(&parent_of(&rel))));
                    if !facts && let Some(f) = &self.on_json {
                        f(&rel, b, cv);
                    }
                }
                other => tracing::warn!("feed hook queue: unknown kind {other}"),
            }
        }
        let idx = self.index.lock();
        if let Err(e) = idx
            .conn()
            .execute("DELETE FROM feed_hook WHERE id <= ?1", [last])
        {
            tracing::warn!("feed hook queue: {e}");
        }
    }
}

// ---- the public face ------------------------------------------------------------

/// The change-feed engine for one store; configure it, then [`Feed::spawn`] it.
pub struct Feed<B: Backend> {
    core: Core<B>,
}

impl<B: Backend> Feed<B> {
    /// An engine over `backend` that keeps `index` current. The index is shared with
    /// the store (`Index` is `Send`, not `Sync`).
    pub fn new(backend: Arc<B>, index: Arc<Mutex<Index>>, cfg: FeedConfig) -> Feed<B> {
        let caps = backend.caps();
        let (state_tx, _) = watch::channel(EngineState::Init);
        Feed {
            core: Core {
                backend,
                caps,
                index,
                cfg,
                sinks: Arc::new(Sinks::default()),
                hooks: None,
                fstate: Arc::new(NoFeedState),
                on_json: None,
                clock: Clock::now(),
                state_tx,
                active: Arc::new(AtomicBool::new(false)),
            },
        }
    }

    /// The server's cloud hooks (D20).
    pub fn with_hooks(mut self, hooks: Arc<dyn CloudHooks>) -> Self {
        self.core.hooks = Some(hooks);
        self
    }

    /// The durable state's answers (echoes, sweep keeps).
    pub fn with_state(mut self, state: Arc<dyn FeedState>) -> Self {
        self.core.fstate = state;
        self
    }

    /// Where foreign `series.json` / `catalog.json` versions are reported.
    pub fn on_foreign_json(mut self, f: ForeignJsonFn) -> Self {
        self.core.on_json = Some(f);
        self
    }

    /// Register an event sink; the id removes it again.
    pub fn add_sink(&self, sink: WatchSink) -> u64 {
        self.core.sinks.add(sink)
    }

    /// Start the engine on `handle`.
    pub fn spawn(mut self, handle: &Handle) -> FeedHandle {
        self.core.clock = {
            let _g = handle.enter();
            Clock::now()
        };
        let (ctl, rx) = mpsc::unbounded_channel();
        let h = FeedHandle {
            state_rx: self.core.state_tx.subscribe(),
            ctl,
            index: self.core.index.clone(),
            sinks: self.core.sinks.clone(),
            root: self.core.cfg.root.clone(),
            quiet: self.core.cfg.quiet,
            clock: self.core.clock,
            active: self.core.active.clone(),
            join: None,
        };
        let engine = Engine {
            core: self.core,
            rx,
            phase: Phase::Locate,
            cursor: None,
            stash: BTreeSet::new(),
            suspect: false,
            root_suspect: false,
            prev_relist: None,
            next_relist: None,
            window: VecDeque::new(),
            failures: 0,
            confirmed: false,
            bypass: false,
            rescan_due: false,
            reauth: false,
            stop: false,
            sync_done: 0,
        };
        FeedHandle {
            join: Some(handle.spawn(engine.run())),
            ..h
        }
    }
}

enum Ctl {
    Wake,
    Confirm,
    Resume,
    Shutdown,
}

/// Control and observation of a running [`Feed`].
pub struct FeedHandle {
    state_rx: watch::Receiver<EngineState>,
    ctl: mpsc::UnboundedSender<Ctl>,
    index: Arc<Mutex<Index>>,
    sinks: Arc<Sinks>,
    root: PathBuf,
    quiet: Duration,
    clock: Clock,
    active: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl FeedHandle {
    /// The engine's current state.
    pub fn state(&self) -> EngineState {
        self.state_rx.borrow().clone()
    }

    /// A receiver that sees every state change.
    pub fn subscribe(&self) -> watch::Receiver<EngineState> {
        self.state_rx.clone()
    }

    /// Wait until the state satisfies `pred`; on timeout, the state then.
    pub async fn wait_for(
        &self,
        pred: impl FnMut(&EngineState) -> bool,
        timeout: Duration,
    ) -> Result<EngineState, EngineState> {
        let mut rx = self.state_rx.clone();
        match tokio::time::timeout(timeout, rx.wait_for(pred)).await {
            Ok(Ok(s)) => Ok(s.clone()),
            _ => Err(self.state()),
        }
    }

    /// Wait for [`EngineState::Steady`].
    pub async fn wait_steady(&self, timeout: Duration) -> Result<(), EngineState> {
        self.wait_for(|s| *s == EngineState::Steady, timeout)
            .await
            .map(|_| ())
    }

    /// The active/idle cadence hint; turning active polls at once.
    pub fn set_active(&self, on: bool) {
        if self.active.swap(on, Ordering::Relaxed) != on && on {
            let _ = self.ctl.send(Ctl::Wake);
        }
    }

    /// Poll now instead of at the next interval.
    pub fn poll_now(&self) {
        let _ = self.ctl.send(Ctl::Wake);
    }

    /// Apply the stashed removals (or follow a root change) after D19 tripped.
    pub fn confirm_suspect(&self) {
        let _ = self.ctl.send(Ctl::Confirm);
    }

    /// Call the provider again after [`EngineState::NeedsReauth`].
    pub fn resume(&self) {
        let _ = self.ctl.send(Ctl::Resume);
    }

    /// When `p` (or a folder above it) was removed in the cloud, if within `quiet`.
    pub fn recently_removed(&self, p: &Path) -> Option<SystemTime> {
        let rel = p.strip_prefix(&self.root).ok()?;
        let segs: Vec<&str> = rel
            .components()
            .map(|c| match c {
                Component::Normal(s) => s.to_str(),
                _ => None,
            })
            .collect::<Option<_>>()?;
        let since = self.clock.now_ms() - ms(self.quiet);
        let idx = self.index.lock();
        let mut best: Option<i64> = None;
        for k in 1..=segs.len() {
            let path = segs[..k].join("/");
            let at: Option<i64> = idx
                .conn()
                .query_row(
                    "SELECT MAX(removed_at) FROM tombstone WHERE path = ?1 AND removed_at >= ?2",
                    params![path, since],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            best = best.max(at);
        }
        best.map(|t| UNIX_EPOCH + Duration::from_millis(t.max(0) as u64))
    }

    /// Register an event sink; the id removes it again.
    pub fn add_sink(&self, sink: WatchSink) -> u64 {
        self.sinks.add(sink)
    }

    /// Unregister a sink.
    pub fn remove_sink(&self, id: u64) {
        self.sinks.remove(id);
    }

    /// Stop the engine and wait for it.
    pub async fn shutdown(mut self) {
        let _ = self.ctl.send(Ctl::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.await;
        }
    }
}

// ---- the loop -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Locate,
    FullSync,
    Steady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncMode {
    /// Sweep unless D19 trips.
    Normal,
    /// Suspect: sweep only if this listing agrees with the previous one.
    Relist,
    /// Confirmed: sweep regardless.
    Apply,
}

enum Fail {
    B(BErr),
    Db(rusqlite::Error),
}

impl From<BErr> for Fail {
    fn from(e: BErr) -> Fail {
        Fail::B(e)
    }
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Fail {
        Fail::Db(e)
    }
}

struct Engine<B: Backend> {
    core: Core<B>,
    rx: mpsc::UnboundedReceiver<Ctl>,
    phase: Phase,
    cursor: Option<Cursor>,
    /// D19: removals held back (pids still in the index).
    stash: BTreeSet<Pid>,
    suspect: bool,
    root_suspect: bool,
    /// The stale set of the last full listing while suspect.
    prev_relist: Option<BTreeSet<Pid>>,
    next_relist: Option<Instant>,
    /// STEADY removals applied within the D19 window.
    window: VecDeque<(Instant, u64)>,
    failures: u32,
    confirmed: bool,
    /// The next FULL_SYNC sweeps without the D19 check.
    bypass: bool,
    rescan_due: bool,
    reauth: bool,
    stop: bool,
    sync_done: u64,
}

impl<B: Backend> Engine<B> {
    async fn run(mut self) {
        if let Err(e) = self.init() {
            tracing::error!("feed init: {e}");
        }
        self.publish_state();
        while !self.stop {
            let r = match self.phase {
                Phase::Locate => self.locate().await,
                Phase::FullSync => self.full_sync().await,
                Phase::Steady => self.steady().await,
            };
            if let Err(f) = r {
                self.on_error(f).await;
            }
            loop {
                match self.rx.try_recv() {
                    Ok(c) => self.on_ctl(Some(c)),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        self.stop = true;
                        break;
                    }
                }
            }
        }
    }

    fn init(&mut self) -> rusqlite::Result<()> {
        {
            let idx = self.core.index.lock();
            idx.conn().execute_batch(HOOK_SCHEMA)?;
            self.cursor = idx.meta_get("cursor").map(Cursor);
            self.stash = idx
                .meta_get("suspect_stash")
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_default();
        }
        if !self.stash.is_empty() {
            self.suspect = true;
            self.next_relist = Some(Instant::now() + self.core.cfg.suspect_relist);
        }
        // Hooks queued before a crash.
        self.core.drain_hooks();
        Ok(())
    }

    fn publish_state(&self) {
        let s = if self.reauth {
            EngineState::NeedsReauth
        } else if self.failures >= OFFLINE_AFTER {
            EngineState::Offline
        } else if self.suspect {
            EngineState::Suspect {
                pending_removals: self.stash.len() as u64,
                root_changed: self.root_suspect,
            }
        } else {
            match self.phase {
                Phase::Locate => EngineState::LocateRoots,
                Phase::FullSync => EngineState::FullSync {
                    done: self.sync_done,
                    total: None,
                },
                Phase::Steady => EngineState::Steady,
            }
        };
        self.core.state_tx.send_if_modified(|cur| {
            if *cur == s {
                false
            } else {
                *cur = s;
                true
            }
        });
    }

    fn ok(&mut self) {
        if self.failures != 0 {
            self.failures = 0;
            self.publish_state();
        }
    }

    fn on_ctl(&mut self, c: Option<Ctl>) {
        match c {
            None | Some(Ctl::Shutdown) => self.stop = true,
            Some(Ctl::Confirm) => {
                if self.suspect {
                    self.confirmed = true;
                }
            }
            Some(Ctl::Wake) | Some(Ctl::Resume) => {}
        }
    }

    /// Sleep `d`, or less if a control message arrives.
    async fn idle(&mut self, d: Duration) {
        if self.stop {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(d) => {}
            c = self.rx.recv() => self.on_ctl(c),
        }
    }

    fn poll_interval(&self) -> Duration {
        if self.core.active.load(Ordering::Relaxed) {
            self.core.cfg.poll_active
        } else {
            self.core.cfg.poll_idle
        }
    }

    /// The tombstone grace: `max(2 × feed interval, quiet)`.
    fn grace(&self) -> Duration {
        let interval = if self.core.caps.push {
            Duration::ZERO
        } else {
            self.poll_interval()
        };
        (interval * 2).max(self.core.cfg.quiet)
    }

    fn next_wait(&self) -> Duration {
        let mut wait = self.poll_interval();
        if let Some(t) = self.core.oldest_tombstone() {
            let due = t + ms(self.grace()) - self.core.clock.now_ms();
            wait = wait.min(Duration::from_millis(due.max(0) as u64));
        }
        if self.suspect
            && let Some(t) = self.next_relist
        {
            wait = wait.min(t.saturating_duration_since(Instant::now()));
        }
        wait.max(MIN_WAIT)
    }

    async fn on_error(&mut self, f: Fail) {
        match f {
            Fail::Db(e) => {
                tracing::error!("feed: index error: {e}");
                self.backoff().await;
            }
            Fail::B(BErr::Throttled(d)) | Fail::B(BErr::TransferQuota(d)) => {
                let d = d.unwrap_or(THROTTLE_DEFAULT);
                tracing::info!("feed: throttled, pausing {d:?}");
                self.idle(d).await;
            }
            Fail::B(BErr::AuthExpired) | Fail::B(BErr::AuthRevoked) => {
                tracing::warn!("feed: authorization lost; waiting for reauthorization");
                self.reauth = true;
                self.publish_state();
                while !self.stop {
                    match self.rx.recv().await {
                        Some(Ctl::Resume) => break,
                        c @ (None | Some(Ctl::Shutdown) | Some(Ctl::Confirm)) => self.on_ctl(c),
                        Some(Ctl::Wake) => {}
                    }
                }
                self.reauth = false;
                self.failures = 0;
                self.publish_state();
            }
            Fail::B(BErr::CursorLost) => self.phase = Phase::FullSync,
            Fail::B(e) => {
                tracing::warn!("feed: {e}");
                self.backoff().await;
            }
        }
    }

    async fn backoff(&mut self) {
        self.failures = self.failures.saturating_add(1);
        let d = BACKOFF_MIN
            .saturating_mul(1u32 << (self.failures - 1).min(6))
            .min(BACKOFF_MAX);
        self.publish_state();
        self.idle(d).await;
    }

    // ---- LOCATE_ROOTS ---------------------------------------------------------

    async fn locate(&mut self) -> Result<(), Fail> {
        self.publish_state();
        let folder = self.core.cfg.folder.clone();
        let mut found = self.core.backend.locate_roots(&folder).await?;
        self.ok();
        let (known, nested, changed) = {
            let idx = self.core.index.lock();
            let known = idx.roots();
            let changed = known.iter().any(|k| {
                let was = idx.node(k).and_then(|n| n.parent);
                found
                    .iter()
                    .find(|f| &f.pid == k)
                    .is_none_or(|f| f.parent != was)
            });
            (known, idx.nested_roots(), changed)
        };
        if changed && !self.confirmed {
            // A root removed, renamed or moved (D19): wait for the admin, or for
            // the root to be back where it was.
            if !self.root_suspect {
                tracing::warn!("feed: the library root changed; holding until confirmed");
            }
            self.suspect = true;
            self.root_suspect = true;
            self.publish_state();
            let d = self.core.cfg.suspect_relist;
            self.idle(d).await;
            return Ok(());
        }
        let confirmed = std::mem::take(&mut self.confirmed);
        if self.root_suspect {
            self.root_suspect = false;
            self.suspect = !self.stash.is_empty() && !confirmed;
        }
        if confirmed {
            self.bypass = true;
            self.suspect = false;
        }
        if found.is_empty() && self.core.cfg.create_if_missing {
            found.push(self.core.backend.create_root(&folder).await?);
        }
        if found.is_empty() {
            if !known.is_empty() {
                let changes = self.core.index.lock().set_roots(&[])?;
                self.core.emit(&changes);
            }
            self.publish_state();
            let d = self.core.cfg.poll_idle;
            self.idle(d).await;
            return Ok(());
        }
        // A candidate never seen before (nested ones are already known).
        let extra = found
            .iter()
            .any(|f| !known.contains(&f.pid) && !nested.contains(&f.pid));
        if changed || extra {
            let changes = self.core.index.lock().set_roots(&found)?;
            self.core.emit(&changes);
        }
        let pending = self
            .core
            .index
            .lock()
            .meta_get("full_sync_pending")
            .as_deref()
            == Some("1");
        if self.cursor.is_some() && !pending && !extra && !changed && !self.bypass {
            self.phase = Phase::Steady;
            self.rescan_due = true;
        } else {
            self.phase = Phase::FullSync;
        }
        Ok(())
    }

    // ---- FULL_SYNC --------------------------------------------------------------

    async fn full_sync(&mut self) -> Result<(), Fail> {
        let mode = if self.bypass {
            SyncMode::Apply
        } else if self.suspect {
            SyncMode::Relist
        } else {
            SyncMode::Normal
        };
        self.sync_done = 0;
        self.publish_state();
        let start = self.core.backend.start_cursor().await?;
        let (generation, roots) = {
            let idx = self.core.index.lock();
            let g = idx.bump_generation()?;
            idx.meta_set("full_sync_pending", "1")?;
            (g, idx.roots())
        };
        if roots.is_empty() {
            self.phase = Phase::Locate;
            return Ok(());
        }
        let show = mode != SyncMode::Relist && self.failures < OFFLINE_AFTER;
        let chunk = self.core.cfg.chunk.max(1);
        let mut buf: Vec<RNode> = Vec::with_capacity(chunk);
        let mut err: Option<rusqlite::Error> = None;
        let mut done = 0u64;
        let listed = {
            let core = &self.core;
            let mut sink = |n: RNode| {
                if err.is_some() {
                    return;
                }
                buf.push(n);
                done += 1;
                if buf.len() >= chunk {
                    if let Err(e) = core.ingest(&mut buf, generation) {
                        err = Some(e);
                    }
                    if show {
                        core.state_tx
                            .send_replace(EngineState::FullSync { done, total: None });
                    }
                }
            };
            core.backend.list_all(&roots, &mut sink).await
        };
        match listed {
            Ok(()) => {}
            Err(BErr::NotFound) => {
                // A root vanished under the listing.
                self.phase = Phase::Locate;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        if let Some(e) = err {
            return Err(e.into());
        }
        self.core.ingest(&mut buf, generation)?;
        self.sync_done = done;
        self.ok();

        let fstate = self.core.fstate.clone();
        let (stale, visible, total) = {
            let idx = self.core.index.lock();
            let stale: Vec<Pid> = idx
                .stale_pids(generation)
                .into_iter()
                .filter(|p| !fstate.keep_on_sweep(p))
                .collect();
            // Unplaced leftovers (children of a removed folder) leave silently.
            let visible = stale
                .iter()
                .filter(|p| idx.entry_by_pid(p).is_some())
                .count() as u64;
            (stale, visible, idx.node_count())
        };
        let set: BTreeSet<Pid> = stale.iter().cloned().collect();
        let apply = match mode {
            SyncMode::Apply => true,
            SyncMode::Normal => !self.core.trips(visible, total),
            SyncMode::Relist => self.prev_relist.as_ref() == Some(&set),
        };
        let mut out = Vec::new();
        {
            let idx = self.core.index.lock();
            let tx = idx.begin()?;
            if apply && !stale.is_empty() {
                let dels = self.core.plan_deletes(&idx, &stale, &[], false);
                self.core.apply_deletes(&idx, &tx, &dels, false, &mut out)?;
            }
            // Ends the generation; with nothing applied, everything stale stays.
            idx.sweep_older_than(&tx, generation, &|p| !apply || fstate.keep_on_sweep(p))?;
            let stash = if apply { BTreeSet::new() } else { set.clone() };
            stash_save(&idx, &stash)?;
            idx.meta_set("cursor", &start.0)?;
            idx.meta_set("full_sync_pending", "0")?;
            tx.commit()?;
        }
        self.core.emit(&out);
        self.core.drain_hooks();
        if apply {
            self.stash.clear();
            self.suspect = false;
            self.prev_relist = None;
            self.next_relist = None;
            self.window.clear();
        } else {
            if !self.suspect {
                tracing::warn!(
                    removals = set.len(),
                    total,
                    "feed: a full listing would remove too much; holding (D19)"
                );
            }
            self.stash = set.clone();
            self.suspect = true;
            self.prev_relist = Some(set);
            self.next_relist = Some(Instant::now() + self.core.cfg.suspect_relist);
        }
        self.bypass = false;
        self.cursor = Some(start);
        self.phase = Phase::Steady;
        self.rescan_due = true;
        Ok(())
    }

    // ---- STEADY -----------------------------------------------------------------

    async fn steady(&mut self) -> Result<(), Fail> {
        if self.rescan_due {
            self.rescan_due = false;
            self.core.sinks.emit(&[WatchEvent::Rescan]);
        }
        if std::mem::take(&mut self.confirmed) && self.suspect && !self.root_suspect {
            self.apply_confirm()?;
        }
        self.publish_state();
        loop {
            let Some(cur) = self.cursor.clone() else {
                self.phase = Phase::FullSync;
                return Ok(());
            };
            let page = self.core.backend.changes(&cur).await?;
            match page {
                ChangePage::CursorLost => {
                    tracing::info!("feed: change cursor lost; full resync");
                    self.phase = Phase::FullSync;
                    return Ok(());
                }
                ChangePage::Page { items, next, more } => {
                    let items = self.expand_new_dirs(items).await?;
                    self.apply_batch(items, next)?;
                    self.ok();
                    self.publish_state();
                    if self.phase != Phase::Steady {
                        return Ok(());
                    }
                    if !more {
                        break;
                    }
                }
            }
        }
        self.core.expire_tombstones(self.grace())?;
        if self.suspect && self.next_relist.is_some_and(|t| Instant::now() >= t) {
            self.phase = Phase::FullSync;
            return Ok(());
        }
        let wait = self.next_wait();
        if self.core.caps.push {
            let Some(cur) = self.cursor.clone() else {
                return Ok(());
            };
            let r = {
                let core = &self.core;
                tokio::select! {
                    r = core.backend.wait_change(&cur, wait) => Ok(r),
                    c = self.rx.recv() => Err(c),
                }
            };
            match r {
                Ok(r) => r?,
                Err(c) => self.on_ctl(c),
            }
        } else {
            self.idle(wait).await;
        }
        Ok(())
    }

    /// Folders that appeared inside the tree with an unknown pid may have been moved
    /// in with their contents (which the feed does not repeat): list them and add
    /// their subtrees to the page.
    async fn expand_new_dirs(
        &mut self,
        mut items: Vec<ChangeItem>,
    ) -> Result<Vec<ChangeItem>, Fail> {
        if self.core.caps.path_ids {
            return Ok(items);
        }
        let new_dirs: Vec<Pid> = {
            let idx = self.core.index.lock();
            let fresh: HashSet<&str> = items
                .iter()
                .filter_map(|i| match i {
                    ChangeItem::Upsert(n)
                        if n.is_dir && !n.trashed && idx.node(&n.pid).is_none() =>
                    {
                        Some(n.pid.as_str())
                    }
                    _ => None,
                })
                .collect();
            let mut out: Vec<Pid> = Vec::new();
            for i in &items {
                if let ChangeItem::Upsert(n) = i
                    && fresh.contains(n.pid.as_str())
                    && let Some(p) = n.parent.as_deref()
                    && !fresh.contains(p)
                    && idx.node(p).is_some()
                    && !out.contains(&n.pid)
                {
                    out.push(n.pid.clone());
                }
            }
            out
        };
        for d in new_dirs {
            let mut found = Vec::new();
            match self
                .core
                .backend
                .list_all(std::slice::from_ref(&d), &mut |n| found.push(n))
                .await
            {
                Ok(()) => items.extend(found.into_iter().map(ChangeItem::Upsert)),
                Err(BErr::NotFound) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(items)
    }

    /// Apply one change page and its cursor in one transaction.
    fn apply_batch(&mut self, items: Vec<ChangeItem>, next: Cursor) -> Result<(), Fail> {
        let pid_of = |i: &ChangeItem| -> Pid {
            match i {
                ChangeItem::Upsert(n) => n.pid.clone(),
                ChangeItem::Removed { pid } => pid.clone(),
            }
        };
        let mut last: HashMap<Pid, usize> = HashMap::new();
        for (i, it) in items.iter().enumerate() {
            last.insert(pid_of(it), i);
        }
        let now = Instant::now();
        let window = self.core.cfg.suspect_window;
        while self
            .window
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > window)
        {
            self.window.pop_front();
        }
        let in_window: u64 = self.window.iter().map(|(_, n)| n).sum();
        let mut out = Vec::new();
        let mut root_changed = false;
        let (stashed, counted) = {
            let core = &self.core;
            let idx = core.index.lock();
            let roots: HashSet<Pid> = idx.roots().into_iter().collect();
            let generation = idx.generation();
            let tx = idx.begin()?;
            let mut deletes: Vec<Pid> = Vec::new();
            for (i, it) in items.iter().enumerate() {
                let pid = pid_of(it);
                if last.get(&pid) != Some(&i) {
                    continue; // a later item for this pid wins
                }
                if roots.contains(&pid) {
                    match it {
                        ChangeItem::Upsert(n)
                            if !n.trashed
                                && nfc(&n.name_raw) == core.cfg.folder
                                && idx.node(&pid).and_then(|o| o.parent) == n.parent =>
                        {
                            idx.upsert(&tx, n, generation, core.caps.mtime_granularity_ns)?;
                        }
                        _ => root_changed = true,
                    }
                    continue;
                }
                match it {
                    ChangeItem::Removed { pid } => deletes.push(pid.clone()),
                    ChangeItem::Upsert(n) if n.trashed => deletes.push(n.pid.clone()),
                    ChangeItem::Upsert(n) => {
                        self.stash.remove(&n.pid);
                        core.apply_upsert(&idx, &tx, n, generation, &mut out)?;
                    }
                }
            }
            let dels = core.plan_deletes(&idx, &deletes, &out, core.caps.path_ids);
            let count = Core::<B>::removal_count(&dels);
            let total = idx.node_count();
            // A root change holds everything that came with it (on path-id providers
            // a root rename also removes every old descendant pid).
            let hold = self.suspect
                || root_changed
                || (!self.bypass && core.trips(in_window + count, total));
            let stashed = core.apply_deletes(&idx, &tx, &dels, hold, &mut out)?;
            if !stashed.is_empty() {
                let mut all = self.stash.clone();
                all.extend(stashed.iter().cloned());
                stash_save(&idx, &all)?;
            } else if self.suspect {
                stash_save(&idx, &self.stash)?;
            }
            idx.meta_set("cursor", &next.0)?;
            tx.commit()?;
            (stashed, if hold { 0 } else { count })
        };
        self.cursor = Some(next);
        if counted > 0 {
            self.window.push_back((now, counted));
        }
        if !stashed.is_empty() {
            if !self.suspect {
                tracing::warn!(
                    removals = stashed.len(),
                    "feed: too many cloud removals at once; holding (D19)"
                );
                self.prev_relist = None;
                self.next_relist = Some(now + self.core.cfg.suspect_relist);
            }
            self.suspect = true;
            self.stash.extend(stashed);
        }
        self.core.emit(&out);
        self.core.drain_hooks();
        if root_changed {
            self.phase = Phase::Locate;
        }
        Ok(())
    }

    /// The admin confirmed: apply the stashed removals.
    fn apply_confirm(&mut self) -> Result<(), Fail> {
        let pids: Vec<Pid> = self.stash.iter().cloned().collect();
        let mut out = Vec::new();
        {
            let idx = self.core.index.lock();
            let tx = idx.begin()?;
            let dels = self.core.plan_deletes(&idx, &pids, &[], false);
            self.core.apply_deletes(&idx, &tx, &dels, false, &mut out)?;
            stash_save(&idx, &BTreeSet::new())?;
            tx.commit()?;
        }
        self.stash.clear();
        self.suspect = false;
        self.prev_relist = None;
        self.next_relist = None;
        self.window.clear();
        self.core.emit(&out);
        self.core.drain_hooks();
        Ok(())
    }
}
