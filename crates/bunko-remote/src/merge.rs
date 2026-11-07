//! The automatic case merge (spec §5.9 "The merge (owner decision)", §13).
//!
//! A library is case-insensitive and case-preserving on every host (owner rule), but
//! Drive, MEGA and case-sensitive WebDAV hosts can hold `Kingdom/` beside `kingdom/`.
//! Bunko merges such variants by itself, into the oldest spelling:
//!
//! - **Winner:** the oldest `created_ms`, then the smallest pid
//!   ([`Index::variants`](crate::index::Index::variants)); an entry with several
//!   members (NFC-equal folders) has its oldest member's age, and the move target is
//!   that member node. PathCase resolves a folded request to the same winner through
//!   `case_winner`.
//! - **When:** a [`CaseMergePolicy`] is installed (the server's), the lease is held
//!   (the Deferred gate is open), health is `Online` (STEADY, not `Suspect`, not in
//!   standby), first adoption is done, neither variant changed (an event, a pending op)
//!   for `quiet_s`, and the policy reports no OCR claim, spool or upgrade under either.
//!   While a merge journal group is open, [`RemoteStore::merge_busy`] answers true for
//!   its paths: the scheduler claims nothing there and the census skips them.
//! - **Plan:** from a direct `list_dir` of every member node of both entries, never the
//!   index alone. Children merge as volume groups (`<stem>.cbz` with its sidecars,
//!   layers and cover), paired by folded stem; folders merge recursively. Identical
//!   archives keep the winner's; a loser-only sidecar, layer or cover moves in under
//!   the winner's stem; an unstamped layer whose id is taken moves in as `-2`, `-3`;
//!   differing main sidecars keep the one [`CaseMergePolicy::prefer_main`] picks and
//!   the other goes to the provider's trash, with no copy kept (owner decision,
//!   2026-11-01; the loser's picked main replaces the winner's in place, and where the
//!   provider's replace makes a new node the old one is trashed); differing
//!   archives, a stamped layer that differs, or a file against a folder leave the
//!   whole group in the loser as a `case-merge-blocked` conflict. A loser
//!   `series.json` is ingested as a submission into the winner's series key, as its
//!   facts only ([`crate::submissions`]), then trashed. A loser recreated too often
//!   stops being merged ([`RECREATE_LIMIT`]). These rules are the shared planner,
//!   [`bunko_core::case_merge`], which every store plans with; this store answers it
//!   through `RemoteHost` (direct listings, §5.9 Identical, the bytes a replace
//!   uploads) and keeps finding the sets, the journal and the executor.
//! - **Durability:** each merge is one journal group: a `merge_group` row in
//!   `state.sqlite` and its steps as outbox ops (origin `Admin`, `Deferred`,
//!   `group_id`), which the outbox worker never runs. A group resumes after a crash,
//!   only once the gate (the lease fence) is open again, and pauses while the store is
//!   not `Online` (a `Suspect` mid-merge included).
//! - **Commits:** a move or trash rewrites its ledger row and the index at COMMITTED
//!   (the outbox's own `commit_move`/`commit_trash`), so the feed's echo is exact: no
//!   hooks run twice and nothing counts toward D19. Each step holds the commit window
//!   from its provider call through COMMITTED, so a feed poll that lands in between
//!   (MEGA's push wakes the feed at once) waits for the ledger row. The DB hooks run once per file
//!   ([`CloudHooks::merged`](crate::CloudHooks::merged) for a move,
//!   [`CloudHooks::removed`](crate::CloudHooks::removed) for a trashed loser file).
//! - **Finishing:** an emptied losing member node is trashed only after a direct
//!   listing shows it empty; anything else is re-planned in a later pass.
//! - **Closing:** a group is audited ([`CaseMergePolicy::audit`]) before it closes;
//!   its row turns `done` and its paths stop being busy at once. Whoever sees a merge
//!   finished, by either, finds its audit record.
//!
//! ## Duplicates (D5 rev.)
//!
//! Drive and MEGA also hold two folders of one exact name in one parent (two
//! `mokuro-reader` roots, two copies of a series). Bunko runs Mokuro Reader's own merge
//! (#308) on the same journal, so whichever of the two gets there first, the cloud
//! ends the same ([`crate::index::Index::dup_member_groups`]):
//!
//! - **Groups:** folder nodes of one exact spelling (as the reader compares names, so
//!   NFC-equal spellings never merge) under one parent node; the canonical member
//!   is the oldest, then the smallest pid (the root's: [`Index::roots`] order, top-level
//!   first). Roots under different parents never merge. Groups merge pass by pass, the
//!   shallowest level first.
//! - **When:** the case merge's conditions (lease, `Online`, adoption, `quiet_s` on the
//!   folder's direct children, no OCR on what moves), with `storage.library.dedup` on
//!   and a provider that allows duplicate names. A folder merged
//!   [`DEDUP_RECREATE_LIMIT`] times in a day is left alone (`dup-recreated`). A group
//!   that moved and trashed nothing waits for a change in its scope (any origin), and
//!   a re-plan identical to its plan, in any listing order, journals nothing
//!   (`Plan::fingerprint`).
//! - **Plan:** per other member, fresh listings of it and the canonical one. An item
//!   with no counterpart moves in; a folder, or a keep-both file
//!   ([`crate::names::dedup_keeps_both`]), moves in beside its name; of two files the
//!   strictly newer write stays and the other goes to the trash (a tie or an unknown
//!   time: the canonical copy), a losing `series.json` submitted first (D18; its facts
//!   only, [`crate::submissions`]; trashed even when refused, as the reader does). A
//!   member is trashed only once a listing shows it empty.
//! - **Paths never change:** every node stays at its library path, so no step runs a DB
//!   hook or announces a removal, and D19 never counts one.
//!
//! [`Index::roots`]: crate::index::Index::roots
//!
//! One std thread (`bunko-store-merge`) runs the passes: planning reads file bytes
//! through the store's blocking content methods, which never run on the store runtime.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bunko_core::case_merge::{
    self as core_merge, Block, Defer, EVENT_RETAIN, Extra, Host, Memo, Node, Plan, Planned, Scope,
    SetSpec, StepKind, archive_stems, defer, file_key, folded, last_name, order_stem_members,
    pair_key, parent_of, split_name,
};
use bunko_store::{Durability, Health, LibraryStore, Mode, Origin, WatchEvent};
use parking_lot::{Condvar, Mutex};
use rusqlite::{OptionalExtension, params};
use tokio::sync::OwnedRwLockReadGuard;

use crate::backend::{BErr, Backend, OpCtx, Pid, RNode, UploadSource, UploadTarget};
use crate::cache::{SMALL_MAX, now_ms};
use crate::governor::{self, ApiClass};
use crate::index::{ROOT_EID, RelPath};
use crate::names::{dedup_keeps_both, nfc};
use crate::outbox::{Crash, Op, OpKind, OpState, Proof};
use crate::state::{ConflictRow, random_id};
use crate::store::{RemoteStore, Ver};

// The planner and the server's side of it are shared with every store
// ([`bunko_core::case_merge`]); they keep their names here.
pub use bunko_core::case_merge::{
    CaseMergePolicy, MergeChoice, MergeRecord, RECREATE_LIMIT, RECREATE_WINDOW, RECREATING,
    ResolveChoice, is_unstamped_layer_id,
};

#[cfg(feature = "testing")]
#[path = "merge_legacy.rs"]
mod legacy;
#[cfg(feature = "testing")]
pub use legacy::{DualPlanReport, dual_plan_report};

const EVENT_CAP: usize = 100_000;
/// After this many finished duplicate merges of one folder within [`RECREATE_WINDOW`],
/// its duplicates are left alone (a `dup-recreated` conflict says so).
pub const DEDUP_RECREATE_LIMIT: u64 = 10;
/// The detail of the duplicate merge's recreate-loop conflict.
pub const DUP_RECREATING: &str = "duplicate folders keep reappearing";

// ---- scopes, events ---------------------------------------------------------------

/// The merge's shared state: the policy, open scopes, recent events, the thread.
pub(crate) struct MergeState {
    policy: Mutex<Option<Arc<dyn CaseMergePolicy>>>,
    /// Scopes a merge works on (`Some(group)`), or is checking (`None`). A group's
    /// `merge_group` row changes state while this is held ([`MergeState::open_group`],
    /// [`MergeState::close_group`]): a caller that sees a group `open` finds its
    /// scope busy, one that sees it `done` (or its scope no longer busy) finds it
    /// closed. The order is this, then the state db, never the other way.
    busy: Mutex<Vec<(Option<String>, Scope)>>,
    events: Mutex<VecDeque<(Instant, Vec<String>)>>,
    /// Scopes planned with nothing to do (only blocks), or whose group made no
    /// progress, and when ([`Memo`]).
    memo: Mutex<HashMap<Scope, Memo>>,
    started: Instant,
    quiet: Duration,
    interval: Duration,
    woken: Mutex<bool>,
    cv: Condvar,
    stop: AtomicBool,
    root: PathBuf,
    /// Held while a group and its steps are journaled, and while a pass lists the
    /// open groups: a pass never runs (and closes) a group whose steps are still
    /// being written. An admin resolve journals on the caller's thread.
    journal: Mutex<()>,
}

impl MergeState {
    pub(crate) fn new(root: PathBuf, quiet: Duration, interval: Duration) -> MergeState {
        MergeState {
            policy: Mutex::new(None),
            busy: Mutex::new(Vec::new()),
            events: Mutex::new(VecDeque::new()),
            memo: Mutex::new(HashMap::new()),
            started: Instant::now(),
            quiet,
            interval,
            woken: Mutex::new(false),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            root,
            journal: Mutex::new(()),
        }
    }

    /// Record a store event (any change, the store's own included).
    pub(crate) fn note(&self, ev: &WatchEvent) {
        let Some(p) = ev.path() else { return };
        let Ok(rest) = p.strip_prefix(&self.root) else {
            return;
        };
        let segs: Vec<String> = rest
            .components()
            .filter_map(|c| c.as_os_str().to_str().map(pair_key))
            .collect();
        let now = Instant::now();
        let mut q = self.events.lock();
        q.push_back((now, segs));
        while q.len() > EVENT_CAP
            || q.front()
                .is_some_and(|(t, _)| now.saturating_duration_since(*t) > EVENT_RETAIN)
        {
            q.pop_front();
        }
    }

    fn last_event(&self, scope: &Scope) -> Option<Instant> {
        self.events
            .lock()
            .iter()
            .rev()
            .find(|(_, s)| scope.covers(s))
            .map(|(t, _)| *t)
    }

    fn quiet_now(&self, scope: &Scope) -> bool {
        self.started.elapsed() >= self.quiet
            && self
                .last_event(scope)
                .is_none_or(|t| t.elapsed() >= self.quiet)
    }

    pub(crate) fn busy_at(&self, segs: &[String]) -> bool {
        self.busy.lock().iter().any(|(_, s)| s.covers(segs))
    }

    fn mark(&self, group: Option<String>, scope: Scope) {
        self.busy.lock().push((group, scope));
    }

    fn unmark_probe(&self, scope: &Scope) {
        let mut b = self.busy.lock();
        if let Some(i) = b.iter().position(|(g, s)| g.is_none() && s == scope) {
            b.remove(i);
        }
    }

    /// Journal a group's row (`insert`) and swap the scope's check (if any) for the
    /// group's mark, all under the busy lock: the scope never looks free in between.
    fn open_group(
        &self,
        group: &str,
        scope: &Scope,
        insert: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let mut b = self.busy.lock();
        insert()?;
        if let Some(i) = b.iter().position(|(g, s)| g.is_none() && s == scope) {
            b.remove(i);
        }
        b.push((Some(group.to_string()), scope.clone()));
        Ok(())
    }

    /// Mark a group's row done (`finish`) and drop its marks under the busy lock:
    /// `done` is never visible while its scope is still busy, nor the other way.
    fn close_group(&self, group: &str, finish: impl FnOnce()) {
        let mut b = self.busy.lock();
        finish();
        b.retain(|(g, _)| g.as_deref() != Some(group));
    }

    pub(crate) fn wake(&self) {
        *self.woken.lock() = true;
        self.cv.notify_all();
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
    }

    fn wait(&self) {
        let mut w = self.woken.lock();
        if !*w {
            self.cv.wait_for(&mut w, self.interval);
        }
        *w = false;
    }

    fn policy(&self) -> Option<Arc<dyn CaseMergePolicy>> {
        self.policy.lock().clone()
    }

    pub(crate) fn set_policy(&self, p: Arc<dyn CaseMergePolicy>) {
        *self.policy.lock() = Some(p);
        self.wake();
    }
}

/// Run merge passes until the store goes.
pub(crate) fn spawn<B: Backend>(store: Weak<RemoteStore<B>>, state: Arc<MergeState>) {
    let r = std::thread::Builder::new()
        .name("bunko-store-merge".into())
        .spawn(move || {
            loop {
                state.wait();
                if state.stop.load(Ordering::Acquire) {
                    return;
                }
                let Some(s) = store.upgrade() else { return };
                s.merge_pass();
            }
        });
    if let Err(e) = r {
        tracing::error!("case merge: could not start its thread: {e}");
    }
}

fn extra_of(op: &Op) -> Option<Extra> {
    op.extra
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
}

/// A provider node as the planner sees it ([`Node`]): its pid, NFC name, content
/// version; the node itself rides along.
fn node_of(n: &RNode) -> Node<RNode> {
    Node {
        id: n.pid.clone(),
        name: nfc(&n.name_raw),
        is_dir: n.is_dir,
        created_ms: n.created_ms,
        ver: n.cver.clone(),
        size: n.size,
        x: n.clone(),
    }
}

/// The planner's [`Host`] over a remote store: direct provider listings, §5.9
/// Identical, store paths, and the bytes a replace uploads (read whole, at most
/// `SMALL_MAX`, deferred above).
pub(crate) struct RemoteHost<'a, B: Backend> {
    store: &'a RemoteStore<B>,
    /// Test builds: what the planner was answered, replayed to the planner as it was
    /// before the extraction (the fingerprint-equality check, `merge_legacy.rs`).
    #[cfg(feature = "testing")]
    tape: Option<&'a legacy::Tape>,
}

impl<'a, B: Backend> RemoteHost<'a, B> {
    fn new(store: &'a RemoteStore<B>) -> Self {
        RemoteHost {
            store,
            #[cfg(feature = "testing")]
            tape: None,
        }
    }
}

impl<B: Backend> Host for RemoteHost<'_, B> {
    type X = RNode;

    fn list(&self, id: &str) -> Result<Vec<Node<RNode>>, Defer> {
        let r = self.store.list(id).map_err(defer);
        #[cfg(feature = "testing")]
        if let Some(t) = self.tape {
            t.listed(id, &r);
        }
        r.map(|v| v.iter().map(node_of).collect())
    }

    fn identical(&self, a: &Node<RNode>, b: &Node<RNode>) -> Result<bool, Defer> {
        let r = self.store.identical(&a.x, &b.x);
        #[cfg(feature = "testing")]
        if let Some(t) = self.tape {
            t.compared(&a.x, &b.x, &r);
        }
        r
    }

    fn path_of(&self, _n: &Node<RNode>, rel: &RelPath) -> PathBuf {
        self.store.core.abs(rel)
    }

    fn replace_bytes(&self, n: &Node<RNode>) -> Result<Option<Vec<u8>>, Defer> {
        let r = self.store.bytes_of(&n.x);
        #[cfg(feature = "testing")]
        if let Some(t) = self.tape {
            t.read(&n.x, &r);
        }
        r.map(Some)
    }
}

// ---- the store's side -------------------------------------------------------------

/// How one step ended.
enum StepEnd {
    Done,
    /// Not now (the store left `Online`, a retryable error): the group pauses.
    Pause,
}

/// A DB hook a step owes, run once the step released the op lock.
enum HookCall {
    Merged(PathBuf, PathBuf, bool),
    Removed(PathBuf, bool),
}

/// How running a group ended.
enum GroupEnd {
    Done,
    Paused,
}

fn ver(n: &RNode) -> Ver {
    Ver {
        pid: n.pid.clone(),
        cver: n.cver.clone(),
        pin: n.pin.clone(),
        size: n.size,
        hash: n.hash.clone(),
    }
}

fn retryable(e: &BErr) -> bool {
    matches!(
        e,
        BErr::Throttled(_)
            | BErr::Transient(_)
            | BErr::AuthExpired
            | BErr::AuthRevoked
            | BErr::StorageFull
            | BErr::TransferQuota(_)
            | BErr::DailyLimit(_)
            | BErr::CursorLost
    )
}

/// A variant set the index shows.
struct VariantSet {
    parent: RelPath,
    parent_eid: i64,
    scope: Scope,
    /// Same-folder file variants: the [`Named::key`] of their files.
    group: String,
}

/// A duplicate set the index shows: same-named folders under one parent node.
struct DupSet {
    /// The library path they all are.
    path: RelPath,
    scope: Scope,
    /// Canonical first ([`crate::index::DupGroup::members`]).
    members: Vec<RNode>,
}

/// `a` was written strictly later than `b`; an unknown time is never newer. The index
/// picks a duplicate file's winner by this same walk (`Index::recompute`).
fn newer(a: &RNode, b: &RNode) -> bool {
    a.base_mtime_ms > 0 && b.base_mtime_ms > 0 && a.base_mtime_ms > b.base_mtime_ms
}

/// A folder's path for the log: the library root by name.
fn shown(path: &RelPath) -> String {
    if path.is_root() {
        "the library root".into()
    } else {
        path.to_string()
    }
}

/// The winner and losers of a set (library paths of the variant entries), from the
/// index: a stem set's files in [`order_stem_members`] order, one per exact stem (the
/// first is the winner's spelling); a folder set's folders, oldest first.
fn set_paths_in(idx: &crate::index::Index, set: &VariantSet) -> (RelPath, Vec<RelPath>) {
    let kids = idx.children(set.parent_eid);
    let stems = archive_stems(kids.iter().map(|k| k.name.as_str()));
    let vars: Vec<crate::index::Entry> = if set.scope.stem {
        let mut v: Vec<crate::index::Entry> = kids
            .into_iter()
            .filter(|k| !k.is_dir && file_key(&k.name, &stems).1 == set.group)
            .collect();
        order_stem_members(
            &mut v,
            &stems,
            |e| e.name.as_str(),
            |e| (e.created_ms, e.winner_pid.clone()),
        );
        v
    } else {
        // Files folding like the folders are blocked in the plan, never merged.
        idx.variants(set.parent_eid, &set.scope.key)
            .into_iter()
            .filter(|e| e.is_dir)
            .collect()
    };
    let paths: Vec<RelPath> = vars
        .iter()
        .filter_map(|e| set.parent.join(e.name.clone()))
        .collect();
    match paths.split_first() {
        Some((w, l)) => (w.clone(), l.to_vec()),
        None => (RelPath::root(), Vec::new()),
    }
}

impl<B: Backend> RemoteStore<B> {
    /// Install the server's [`CaseMergePolicy`]: merges may run from now on.
    pub fn set_case_merge_policy(&self, p: Arc<dyn CaseMergePolicy>) {
        self.merge.set_policy(p);
    }

    /// Run a merge pass soon (instead of at the next interval).
    pub fn merge_now(&self) {
        self.merge.wake();
    }

    /// A merge journal group is open on `p` (or a folder above it), or a merge is
    /// checking it: the scheduler claims nothing there, the census skips it.
    pub fn merge_busy(&self, p: &Path) -> bool {
        match self.rel(p) {
            Ok(rel) => self.merge.busy_at(&folded(&rel)),
            Err(_) => false,
        }
    }

    /// The latest merge groups, newest first.
    pub fn recent_merges(&self) -> Vec<MergeRecord> {
        self.core
            .state
            .with("recent_merges", |c| {
                c.prepare(
                    "SELECT id, parent, winner, losers, origin, state, created_at, finished_at, \
                     moved, trashed, blocked FROM merge_group ORDER BY created_at DESC, rowid DESC \
                     LIMIT 50",
                )?
                .query_map([], |r| {
                    let losers: String = r.get(3)?;
                    Ok(MergeRecord {
                        id: r.get(0)?,
                        parent: r.get(1)?,
                        winner: r.get(2)?,
                        losers: serde_json::from_str(&losers).unwrap_or_default(),
                        origin: r.get(4)?,
                        state: r.get(5)?,
                        created_at: r.get(6)?,
                        finished_at: r.get(7)?,
                        moved: r.get::<_, i64>(8)? as u64,
                        trashed: r.get::<_, i64>(9)? as u64,
                        blocked: r.get::<_, i64>(10)? as u64,
                    })
                })?
                .collect()
            })
            .unwrap_or_default()
    }

    /// Every unresolved conflict, with its id and (for `case-merge-blocked`) both
    /// sides.
    pub fn conflict_rows(&self) -> Vec<crate::state::Conflict> {
        self.core.state.conflict_rows()
    }

    /// "Keep this copy" / "Keep that copy" on a `case-merge-blocked` conflict: the
    /// other side goes to the provider trash through a merge journal group (the next
    /// pass then merges what is left). Returns the group id.
    pub fn resolve_conflict(&self, id: i64, choice: ResolveChoice) -> io::Result<String> {
        let row = self
            .core
            .state
            .conflict_rows()
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such conflict"))?;
        if row.kind != ConflictRow::CaseMergeBlocked.as_str() || row.this.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "only a case-merge-blocked conflict has copies to keep",
            ));
        }
        let gone = match choice {
            ResolveChoice::KeepThis => &row.that,
            ResolveChoice::KeepThat => &row.this,
        };
        let mut steps = Vec::new();
        {
            let idx = self.core.index.lock();
            for p in gone {
                let Some(rel) = RelPath::parse(p) else {
                    continue;
                };
                let Some(e) = idx.lookup(&rel) else { continue };
                let nodes = if e.is_dir {
                    idx.members(e.eid)
                } else {
                    e.winner_pid
                        .as_deref()
                        .and_then(|w| idx.node(w))
                        .into_iter()
                        .collect()
                };
                for n in nodes {
                    steps.push(Planned::trash("admin", &node_of(&n), rel.clone()));
                }
            }
        }
        if steps.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "the copy to remove is no longer in the library",
            ));
        }
        let first = RelPath::parse(&row.this[0]).unwrap_or_default();
        let parent = parent_of(&first);
        let name = last_name(&first).to_string();
        let stems = archive_stems(std::iter::once(name.as_str()));
        let is_dir = self
            .core
            .index
            .lock()
            .lookup(&first)
            .is_some_and(|e| e.is_dir);
        let (key, stem) = if is_dir {
            (pair_key(&name), false)
        } else {
            (file_key(&name, &stems).0, true)
        };
        let scope = Scope {
            dir: folded(&parent),
            key,
            stem,
            shallow: false,
        };
        let winner = row.that.first().cloned().unwrap_or_default();
        let gid = self.persist_group(
            &scope,
            &parent,
            &winner,
            &row.this,
            "admin",
            &Plan {
                l_trash: steps,
                ..Plan::default()
            },
        )?;
        self.core.state.conflict_resolve(id);
        self.merge.memo.lock().clear();
        self.merge.wake();
        tracing::info!(%gid, "case merge: the admin keeps one copy ({choice:?})");
        Ok(gid)
    }

    /// Open groups left by an earlier run: their scopes are busy again at once.
    pub(crate) fn load_open_groups(&self) {
        let rows: Vec<(String, String)> = self
            .core
            .state
            .with("open_groups", |c| {
                c.prepare("SELECT id, scope FROM merge_group WHERE state = 'open'")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect()
            })
            .unwrap_or_default();
        for (id, scope) in rows {
            if let Ok(s) = serde_json::from_str::<Scope>(&scope) {
                self.merge.mark(Some(id), s);
            }
        }
    }

    /// Whether a merge step may reach the provider now.
    fn can_run(&self) -> bool {
        !self.core.crashed.load(Ordering::Acquire)
            && self.core.gate_open()
            && self.core.adopted()
            && LibraryStore::health(self) == Health::Online
    }

    /// Duplicate merges run here: the store's setting, on a provider that allows two
    /// nodes of one name.
    pub(crate) fn dedup_on(&self) -> bool {
        self.cfg.dedup && self.core.caps.duplicate_names
    }

    /// One pass: resume open groups, then merge the duplicate sets and the variant
    /// sets that are ready.
    pub(crate) fn merge_pass(&self) {
        let Some(policy) = self.merge.policy() else {
            return;
        };
        if !self.can_run() {
            return;
        }
        let open = {
            let _journal = self.merge.journal.lock();
            self.open_group_ids()
        };
        for gid in open {
            match self.run_group(&gid, policy.as_ref()) {
                Ok(GroupEnd::Done) => {}
                Ok(GroupEnd::Paused) | Err(Crash) => return,
            }
        }
        if self.dedup_on() {
            for set in self.dup_sets() {
                if !self.can_run() {
                    return;
                }
                if let Err(Crash) = self.try_dup(&set, policy.as_ref()) {
                    return;
                }
            }
        }
        if !self.cfg.case_merge {
            return;
        }
        for set in self.variant_sets() {
            if !self.can_run() {
                return;
            }
            if let Err(Crash) = self.try_set(&set, policy.as_ref()) {
                return;
            }
        }
    }

    fn open_group_ids(&self) -> Vec<String> {
        self.core
            .state
            .with("open_group_ids", |c| {
                c.prepare(
                    "SELECT id FROM merge_group WHERE state = 'open' ORDER BY created_at, rowid",
                )?
                .query_map([], |r| r.get(0))?
                .collect()
            })
            .unwrap_or_default()
    }

    /// The variant sets the index shows: folders (with any file that folds the
    /// same), and same-folder file variants by folded stem.
    fn variant_sets(&self) -> Vec<VariantSet> {
        let idx = self.core.index.lock();
        let mut out: Vec<VariantSet> = Vec::new();
        let mut seen: HashSet<(i64, String)> = HashSet::new();
        for (pe, f) in idx.fold_collisions() {
            let vars = idx.variants(pe, &f);
            if vars.len() < 2 {
                continue;
            }
            let Some(parent) = idx.path_of(pe) else {
                continue;
            };
            let (key, group, stem) = if vars.iter().any(|e| e.is_dir) {
                (f.clone(), f.clone(), false)
            } else {
                let kids = idx.children(pe);
                let stems = archive_stems(kids.iter().map(|k| k.name.as_str()));
                let (key, group) = file_key(&vars[0].name, &stems);
                (key, group, true)
            };
            if !seen.insert((pe, group.clone())) {
                continue;
            }
            out.push(VariantSet {
                scope: Scope {
                    dir: folded(&parent),
                    key,
                    stem,
                    shallow: false,
                },
                group,
                parent,
                parent_eid: pe,
            });
        }
        out
    }

    /// Check one variant set's conditions; plan and run it when they hold.
    fn try_set(&self, set: &VariantSet, policy: &dyn CaseMergePolicy) -> Result<(), Crash> {
        let scope = &set.scope;
        if policy.excluded(set.parent.segments(), &scope.key) {
            return Ok(());
        }
        if self.merge.busy.lock().iter().any(|(_, s)| s == scope) {
            return Ok(());
        }
        // Quiet: no change, no pending op, for quiet_s on either variant.
        if !self.merge.quiet_now(scope) || self.pending_in(scope) {
            return Ok(());
        }
        if self.memoized(scope) {
            return Ok(());
        }
        let (winner, losers) = self.set_paths(set);
        if losers.is_empty() {
            return Ok(());
        }
        if !scope.stem && self.recreations(scope, "auto") >= RECREATE_LIMIT {
            self.block_recreated(&winner, &losers);
            self.merge
                .memo
                .lock()
                .insert(scope.clone(), Memo::new(Instant::now(), None));
            return Ok(());
        }
        // Busy first, then the policy's check: a claim cannot slip in between.
        self.merge.mark(None, scope.clone());
        let busy_paths = self.busy_paths(set, &winner, &losers);
        if busy_paths.iter().any(|p| policy.busy(p)) {
            self.merge.unmark_probe(scope);
            return Ok(());
        }
        let planned = Instant::now();
        let plan = match self.plan_set(set, policy) {
            Ok(p) => p,
            Err(why) => {
                tracing::info!(scope = ?scope, "case merge deferred: {why}");
                self.merge.unmark_probe(scope);
                return Ok(());
            }
        };
        self.record_blocks(&plan.blocks);
        if !plan.work() {
            self.merge
                .memo
                .lock()
                .insert(scope.clone(), Memo::new(Instant::now(), None));
            self.merge.unmark_probe(scope);
            return Ok(());
        }
        let print = plan.fingerprint();
        if self.unchanged_idle(scope, planned, &print) {
            return Ok(());
        }
        let losers_s: Vec<String> = losers.iter().map(ToString::to_string).collect();
        let gid = match self.persist_group(
            scope,
            &set.parent,
            &winner.to_string(),
            &losers_s,
            "auto",
            &plan,
        ) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("case merge: could not journal the merge: {e}");
                self.merge.unmark_probe(scope);
                return Ok(());
            }
        };
        tracing::info!(
            "[STORAGE] Merging {} into {} (case variants)",
            losers_s.join(", "),
            winner
        );
        if let GroupEnd::Done = self.run_group(&gid, policy)? {
            self.memo_if_idle(&gid, scope, planned, print);
        }
        Ok(())
    }

    /// A finished group that moved and trashed nothing (every step refused, blocked or
    /// skipped) would be planned the same way next pass, journal row, audit and
    /// listings included, every merge interval: it is memoized as of its planning, so
    /// only a change since (or the memo's age) plans it again, and with its plan
    /// (`print`), so that planning the same again journals nothing
    /// ([`Self::unchanged_idle`]).
    fn memo_if_idle(&self, gid: &str, scope: &Scope, planned: Instant, print: String) {
        let idle = self
            .core
            .state
            .with("merge_group_idle", |c| {
                c.query_row(
                    "SELECT moved + trashed = 0 FROM merge_group WHERE id = ?1 AND state = 'done'",
                    [gid],
                    |r| r.get::<_, bool>(0),
                )
            })
            .unwrap_or(false);
        if idle {
            self.merge
                .memo
                .lock()
                .insert(scope.clone(), Memo::new(planned, Some(print)));
        }
    }

    /// The scope's last group made no progress and this plan (`print`) is that one
    /// again: it would make none again, so nothing is journaled (no `merge_group` row,
    /// no audit record pushing real history out of "Recent merges") and the scope is
    /// memoized anew as of this planning. Its steps run again once something in the
    /// scope changes the plan, or after a restart (the memo is not kept).
    fn unchanged_idle(&self, scope: &Scope, planned: Instant, print: &str) -> bool {
        let mut memo = self.merge.memo.lock();
        if memo.get(scope).and_then(|m| m.idle.as_deref()) != Some(print) {
            return false;
        }
        memo.insert(scope.clone(), Memo::new(planned, Some(print.to_string())));
        drop(memo);
        self.merge.unmark_probe(scope);
        true
    }

    /// Test-only: expire every memo, as [`EVENT_RETAIN`] without a change would, so the
    /// next pass plans every memoized set again.
    #[cfg(feature = "testing")]
    pub fn expire_merge_memos(&self) {
        let now = Instant::now();
        for m in self.merge.memo.lock().values_mut() {
            m.until = now;
        }
        self.merge.wake();
    }

    /// Planned before with nothing to do (or guarded), and nothing changed since.
    fn memoized(&self, scope: &Scope) -> bool {
        self.merge.memo.lock().get(scope).cloned().is_some_and(|m| {
            self.merge.last_event(scope).is_none_or(|t| t <= m.at) && Instant::now() < m.until
        })
    }

    // ---- duplicates (D5 rev.) -------------------------------------------------------

    /// The duplicate sets ready for this pass: same-named folders under one parent
    /// node (Mokuro Reader's `parentId:name` groups), the shallowest level first and
    /// alone, so a merge moves whole folders before the level below merges what met
    /// (the reader converges the same way, pass by pass). A memoized set does not
    /// hold the levels below it.
    fn dup_sets(&self) -> Vec<DupSet> {
        let mut sets: Vec<(usize, DupSet)> = {
            let idx = self.core.index.lock();
            idx.dup_member_groups()
                .into_iter()
                .filter(|g| g.members.len() > 1)
                .filter_map(|g| {
                    let path = idx.path_of(g.eid)?;
                    Some((
                        g.depth,
                        DupSet {
                            scope: Scope::dup(&path),
                            path,
                            members: g.members,
                        },
                    ))
                })
                .collect()
        };
        sets.retain(|(_, s)| !self.memoized(&s.scope));
        let Some(top) = sets.iter().map(|(d, _)| *d).min() else {
            return Vec::new();
        };
        sets.into_iter()
            .filter(|(d, _)| *d == top)
            .map(|(_, s)| s)
            .collect()
    }

    /// Check one duplicate set's conditions (those of [`Self::try_set`]); plan and
    /// run it when they hold.
    fn try_dup(&self, set: &DupSet, policy: &dyn CaseMergePolicy) -> Result<(), Crash> {
        let scope = &set.scope;
        if self.merge.busy.lock().iter().any(|(_, s)| s == scope) {
            return Ok(());
        }
        if !self.merge.quiet_now(scope) || self.pending_in(scope) {
            return Ok(());
        }
        if self.recreations(scope, "dedup") >= DEDUP_RECREATE_LIMIT {
            tracing::warn!(path = %set.path, "duplicate merge: {DUP_RECREATING}; left alone");
            self.core.state.conflict_once(
                ConflictRow::DupRecreated,
                &set.path.to_string(),
                Some(&set.members[0].pid),
                DUP_RECREATING,
            );
            self.merge
                .memo
                .lock()
                .insert(scope.clone(), Memo::new(Instant::now(), None));
            return Ok(());
        }
        // Busy first, then the policy's check: a claim cannot slip in between.
        self.merge.mark(None, scope.clone());
        if self.dup_busy_paths(set).iter().any(|p| policy.busy(p)) {
            self.merge.unmark_probe(scope);
            return Ok(());
        }
        let planned = Instant::now();
        let plan = match self.plan_dup(set) {
            Ok(p) => p,
            Err(why) => {
                tracing::info!(path = %set.path, "duplicate merge deferred: {why}");
                self.merge.unmark_probe(scope);
                return Ok(());
            }
        };
        if !plan.work() {
            self.merge
                .memo
                .lock()
                .insert(scope.clone(), Memo::new(Instant::now(), None));
            self.merge.unmark_probe(scope);
            return Ok(());
        }
        let print = plan.fingerprint();
        if self.unchanged_idle(scope, planned, &print) {
            return Ok(());
        }
        let path = set.path.to_string();
        let gid = match self.persist_group(
            scope,
            &parent_of(&set.path),
            &path,
            std::slice::from_ref(&path),
            "dedup",
            &plan,
        ) {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("duplicate merge: could not journal the merge: {e}");
                self.merge.unmark_probe(scope);
                return Ok(());
            }
        };
        tracing::info!(
            "[STORAGE] Merging {} duplicate folders of {}",
            set.members.len(),
            shown(&set.path)
        );
        if let GroupEnd::Done = self.run_group(&gid, policy)? {
            self.memo_if_idle(&gid, scope, planned, print);
        }
        Ok(())
    }

    /// What a duplicate merge moves: every node directly in a non-canonical member,
    /// as the library path it shows at (the same path afterwards).
    fn dup_busy_paths(&self, set: &DupSet) -> Vec<PathBuf> {
        let idx = self.core.index.lock();
        set.members[1..]
            .iter()
            .flat_map(|m| idx.nodes_under(&m.pid))
            .filter_map(|n| set.path.join(nfc(&n.name_raw)))
            .map(|r| self.core.abs(&r))
            .collect()
    }

    /// Mokuro Reader's merge of one duplicate set (#308 `mergeGroup`), as journal
    /// steps: every other member's items move into the canonical one (`members[0]`),
    /// each against a fresh listing of both. A folder, or a keep-both file
    /// ([`dedup_keeps_both`]), moves in beside the same name; of two files the
    /// strictly newer write stays and the other goes to the trash (a tie or an unknown
    /// time keeps the canonical copy; a `series.json` is submitted first). Every path
    /// survives, so no step runs a DB hook. Each emptied member is trashed only once a
    /// direct listing shows it empty.
    fn plan_dup(&self, set: &DupSet) -> Result<Plan, Defer> {
        let mut plan = Plan::default();
        let canon = &set.members[0];
        let series_dir = set.path.segments().len() == 1;
        let trash = |vg: &str, n: &RNode, rel: RelPath| {
            let mut p = Planned::trash(vg, &node_of(n), rel.clone());
            p.hooks = false;
            if series_dir
                && !n.is_dir
                && bunko_core::names::is_fixed_name(&nfc(&n.name_raw), "series.json")
            {
                p.kind = StepKind::Submit;
                p.series = Some(rel);
            }
            p
        };
        // What the canonical folder holds once the planned moves ran, by NFC name.
        let mut planned: HashMap<String, RNode> = HashMap::new();
        for d in &set.members[1..] {
            let mut have: HashMap<String, RNode> = HashMap::new();
            for n in self.list(&canon.pid).map_err(defer)? {
                have.entry(nfc(&n.name_raw)).or_insert(n);
            }
            have.extend(planned.iter().map(|(k, v)| (k.clone(), v.clone())));
            let items = self.list(&d.pid).map_err(defer)?;
            plan.empty_loser |= items.is_empty();
            for item in items {
                let name = nfc(&item.name_raw);
                // A name that is no library path stays (and so does its folder).
                let Some(rel) = set.path.join(name.clone()) else {
                    continue;
                };
                let vg = rel.to_string();
                let mv = |expect_dup: bool| {
                    let mut p =
                        Planned::mv(&vg, &node_of(&item), rel.clone(), rel.clone(), &canon.pid);
                    p.hooks = false;
                    p.expect_dup = expect_dup;
                    p
                };
                // A folder or a keep-both file may meet its name in the canonical
                // folder by the time it moves (the reader merging too): never a race.
                let twice = item.is_dir || dedup_keeps_both(&name);
                match have.get(&name) {
                    None => {
                        plan.steps.push(mv(twice));
                        planned.insert(name, item.clone());
                    }
                    Some(ex) if ex.is_dir || twice => {
                        plan.steps.push(mv(true));
                    }
                    Some(ex) if newer(&item, ex) => {
                        // The older copy may itself be a move planned from an earlier
                        // member: it is trashed where it is instead.
                        let ex = ex.clone();
                        plan.steps.retain(|p| p.id != ex.pid);
                        plan.pre_trash.push(trash(&vg, &ex, rel.clone()));
                        plan.steps.push(mv(false));
                        planned.insert(name.clone(), item.clone());
                        have.insert(name, item.clone());
                    }
                    Some(_) => plan.l_trash.push(trash(&vg, &item, rel.clone())),
                }
            }
            plan.finishes.push(Planned {
                kind: StepKind::Finish,
                vg: format!("{}/", set.path),
                id: d.pid.clone(),
                ver: None,
                // Nothing queued follows: the path stays, and writes go to the
                // entry's winner, never to a member that is not canonical.
                src: None,
                dest: set.path.clone(),
                parent_id: None,
                is_dir: true,
                hooks: false,
                bytes: None,
                series: None,
                expect_dup: false,
            });
        }
        #[cfg(feature = "testing")]
        legacy::check_dup(&plan);
        Ok(plan)
    }

    /// The winner and losers of a set (library paths of the variant entries).
    fn set_paths(&self, set: &VariantSet) -> (RelPath, Vec<RelPath>) {
        let idx = self.core.index.lock();
        let paths = set_paths_in(&idx, set);
        #[cfg(feature = "testing")]
        legacy::check_set_paths(&idx, set, &paths);
        paths
    }

    /// The paths the policy's busy check covers: each variant, and each stem's
    /// archive.
    fn busy_paths(&self, set: &VariantSet, winner: &RelPath, losers: &[RelPath]) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for r in std::iter::once(winner).chain(losers) {
            out.push(self.core.abs(r));
            if set.scope.stem {
                let (stem, _, _) = split_name(last_name(r), &HashSet::new());
                if let Some(cbz) = set.parent.join(format!("{stem}.cbz")) {
                    out.push(self.core.abs(&cbz));
                }
            }
        }
        out
    }

    /// A live op (not a merge step) writes under the scope.
    fn pending_in(&self, scope: &Scope) -> bool {
        self.core
            .ops
            .lock()
            .values()
            .any(|o| o.group.is_none() && o.state.live() && scope.covers(&folded(&o.dest)))
    }

    /// Finished merges of this scope by `origin` (`auto`, `dedup`) in the last 24 h.
    fn recreations(&self, scope: &Scope, origin: &str) -> u64 {
        let since = now_ms() - RECREATE_WINDOW.as_millis() as i64;
        self.core
            .state
            .with("recreations", |c| {
                c.query_row(
                    "SELECT COUNT(*) FROM merge_group WHERE scope = ?1 AND origin = ?3 \
                     AND state = 'done' AND created_at >= ?2 AND moved + trashed > 0",
                    params![scope.json(), since, origin],
                    |r| r.get::<_, i64>(0),
                )
            })
            .unwrap_or(0) as u64
    }

    fn block_recreated(&self, winner: &RelPath, losers: &[RelPath]) {
        for l in losers {
            self.record_blocks(&[Block {
                this: vec![l.clone()],
                that: vec![winner.clone()],
                detail: RECREATING.to_string(),
            }]);
        }
    }

    fn record_blocks(&self, blocks: &[Block]) {
        for b in blocks {
            let Some(first) = b.this.first() else {
                continue;
            };
            let pid = self
                .core
                .index
                .lock()
                .lookup(first)
                .and_then(|e| e.winner_pid);
            let s = |v: &[RelPath]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
            self.core.state.merge_block_add(
                &first.to_string(),
                pid.as_deref(),
                &b.detail,
                &s(&b.this),
                &s(&b.that),
            );
        }
    }

    // ---- planning ---------------------------------------------------------------

    fn list(&self, pid: &str) -> Result<Vec<RNode>, BErr> {
        let backend = self.core.backend.clone();
        let pid = pid.to_string();
        self.block_on_as("case merge", Some(ApiClass::Background), async move {
            let mut out = Vec::new();
            let r = {
                let mut sink = |n: RNode| {
                    if !n.trashed {
                        out.push(n);
                    }
                };
                backend.list_dir(&pid, &mut sink).await
            };
            r.map(|()| out)
        })
    }

    fn stat(&self, pid: &str) -> Result<Option<RNode>, BErr> {
        let backend = self.core.backend.clone();
        let pid = pid.to_string();
        self.block_on_as("case merge", Some(ApiClass::Background), async move {
            backend.stat(&pid).await
        })
    }

    /// The bytes of a small file version (pinned, through the blob cache).
    fn bytes_of(&self, n: &RNode) -> Result<Vec<u8>, Defer> {
        if n.size > SMALL_MAX {
            return Err(defer(format!("{} is too large to read whole", n.pid)));
        }
        self.small_bytes(&ver(n)).map(|(d, _)| d).map_err(defer)
    }

    /// The sha256 of a large version from a pinned spooled copy, downloaded in the
    /// background class.
    fn spooled_sha(&self, n: &RNode) -> Result<[u8; 32], BErr> {
        let core = self.core.clone();
        let v = ver(n);
        self.block_on_as("case merge", Some(ApiClass::Background), async move {
            let admitted = core
                .spool
                .admit(&v.key(), v.size, true)
                .await
                .map_err(|e| BErr::Transient(format!("spool: {e}")))?;
            let Some((pin, new)) = admitted else {
                return Err(BErr::Transient("no spool room".into()));
            };
            if new {
                let task_pin = pin.clone_pin();
                tokio::spawn(governor::scoped(
                    ApiClass::Background,
                    core.clone().download(task_pin, v.clone()),
                ));
            }
            let fill = pin.entry.wait(u64::MAX).await;
            match fill.done {
                Some(Ok(())) => pin
                    .entry
                    .sha256
                    .get()
                    .copied()
                    .ok_or_else(|| BErr::Transient("the spooled copy has no hash".into())),
                Some(Err(e)) => Err(e),
                None => Err(BErr::Transient("the spool download stopped".into())),
            }
        })
    }

    /// §5.9 Identical: an equal provider hash of the same kind, else the blob sha256
    /// (files up to 4 MiB), else size and then the sha256 of pinned spooled copies.
    fn identical(&self, a: &RNode, b: &RNode) -> Result<bool, Defer> {
        if a.is_dir || b.is_dir || a.size != b.size {
            return Ok(false);
        }
        if let (Some((ka, ha)), Some((kb, hb))) = (&a.hash, &b.hash)
            && ka == kb
        {
            return Ok(ha.eq_ignore_ascii_case(hb));
        }
        if a.size <= SMALL_MAX {
            let sa = self.small_bytes(&ver(a)).map_err(defer)?.1;
            let sb = self.small_bytes(&ver(b)).map_err(defer)?.1;
            return Ok(sa == sb);
        }
        let sa = self.spooled_sha(a).map_err(defer)?;
        let sb = self.spooled_sha(b).map_err(defer)?;
        Ok(sa == sb)
    }

    /// The set as the planner takes it ([`SetSpec`]): its spellings, and the nodes
    /// behind them, from the index.
    fn set_spec(&self, set: &VariantSet) -> SetSpec {
        let (winner, losers) = self.set_paths(set);
        let idx = self.core.index.lock();
        let members_of = |rel: &RelPath| -> Vec<Pid> {
            idx.lookup(rel)
                .map(|e| idx.members(e.eid).into_iter().map(|n| n.pid).collect())
                .unwrap_or_default()
        };
        if set.scope.stem {
            let parent_ids: Vec<Pid> = idx
                .members(set.parent_eid)
                .into_iter()
                .map(|n| n.pid)
                .collect();
            let target = if set.parent_eid == ROOT_EID {
                idx.roots().first().cloned()
            } else {
                idx.entry(set.parent_eid).and_then(|e| e.winner_pid)
            };
            let spec = SetSpec {
                parent: set.parent.clone(),
                stem: true,
                group: set.group.clone(),
                winner: (winner, Vec::new()),
                losers: losers.into_iter().map(|l| (l, Vec::new())).collect(),
                parent_ids,
                target,
                files_vs: Vec::new(),
            };
            #[cfg(feature = "testing")]
            legacy::check_spec(&idx, set, &spec);
            return spec;
        }
        let w_members = members_of(&winner);
        let files_vs: Vec<RelPath> = idx
            .variants(set.parent_eid, &set.scope.key)
            .into_iter()
            .filter(|e| !e.is_dir)
            .filter_map(|e| set.parent.join(e.name))
            .collect();
        let losers = losers
            .into_iter()
            .map(|l| {
                let m = members_of(&l);
                (l, m)
            })
            .collect();
        let spec = SetSpec {
            parent: set.parent.clone(),
            stem: false,
            group: set.group.clone(),
            winner: (winner, w_members),
            losers,
            parent_ids: Vec::new(),
            target: None,
            files_vs,
        };
        #[cfg(feature = "testing")]
        legacy::check_spec(&idx, set, &spec);
        spec
    }

    /// Plan one variant set with the shared planner ([`core_merge::plan_set`]) over
    /// this store's listings. Test builds also plan it as the code before the
    /// extraction did, from the same answers, and stop the process on any difference
    /// (`merge_legacy.rs`).
    fn plan_set(&self, set: &VariantSet, policy: &dyn CaseMergePolicy) -> Result<Plan, Defer> {
        let spec = self.set_spec(set);
        #[cfg(feature = "testing")]
        {
            let tape = legacy::Tape::default();
            let host = RemoteHost {
                tape: Some(&tape),
                ..RemoteHost::new(self)
            };
            let recording = legacy::Recording {
                inner: policy,
                tape: &tape,
            };
            let plan = core_merge::plan_set(&host, &recording, &spec);
            legacy::check_set(self, set, &spec, &tape, policy, &plan);
            plan
        }
        #[cfg(not(feature = "testing"))]
        core_merge::plan_set(&RemoteHost::new(self), policy, &spec)
    }

    // ---- the journal ------------------------------------------------------------

    fn persist_group(
        &self,
        scope: &Scope,
        parent: &RelPath,
        winner: &str,
        losers: &[String],
        origin: &str,
        plan: &Plan,
    ) -> io::Result<String> {
        let gid = random_id();
        let losers_j = serde_json::to_string(losers).unwrap_or_else(|_| "[]".into());
        // A pass sees the group with all its steps, or not yet (`journal`).
        let _journal = self.merge.journal.lock();
        // Busy from now on, until the group closes.
        self.merge.open_group(&gid, scope, || {
            self.core.state.with("merge_group", |c| {
                c.execute(
                    "INSERT INTO merge_group(id, scope, stem, parent, winner, losers, origin, state, created_at) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', ?8)",
                    params![
                        gid,
                        scope.json(),
                        scope.stem,
                        parent.to_string(),
                        winner,
                        losers_j,
                        origin,
                        now_ms()
                    ],
                )
                .map(|_| ())
            })
        })?;
        self.core
            .fail_at("MERGE_GROUP")
            .map_err(|_| io::Error::other("the store stopped (failpoint)"))?;
        for p in plan.ordered() {
            let kind = match p.kind {
                StepKind::Move => OpKind::Move,
                StepKind::Replace => OpKind::Put,
                StepKind::Trash | StepKind::Submit | StepKind::Finish => OpKind::Trash,
            };
            let id = self.core.next_seq();
            let mut op = Op::new(
                id,
                kind,
                p.dest.clone(),
                Origin::Admin,
                Durability::Deferred,
            );
            op.src = p.src.clone();
            op.target_pid = Some(p.id.clone());
            op.parent_pid = p.parent_id.clone();
            op.group = Some(gid.clone());
            op.extra = serde_json::to_string(&Extra {
                step: p.kind,
                vg: p.vg.clone(),
                is_dir: p.is_dir,
                cver: p.ver.clone(),
                hooks: p.hooks,
                series: p.series.as_ref().map(ToString::to_string),
                expect_dup: p.expect_dup,
            })
            .ok();
            if let Some(bytes) = &p.bytes {
                let tmp = self
                    .core
                    .dir
                    .join("staging")
                    .join(format!("merge-{}", random_id()));
                std::fs::write(&tmp, bytes)?;
                let (name, size, sha) = self.core.take_bytes(&tmp, id)?;
                op.staged = Some(name);
                op.size = size;
                op.sha256 = Some(sha);
                op.mode = match p.kind {
                    StepKind::Replace => Mode::Replace {
                        base: p.ver.clone().map(bunko_store::Version::new),
                    },
                    _ => Mode::CreateNew,
                };
            }
            op.state = OpState::Held;
            self.core.save_op(&op)?;
        }
        Ok(gid)
    }

    fn group_flag(&self, gid: &str, col: &str) -> bool {
        self.core
            .state
            .with("merge_group_flag", |c| {
                c.query_row(
                    &format!("SELECT {col} FROM merge_group WHERE id = ?1"),
                    [gid],
                    |r| r.get::<_, i64>(0),
                )
                .optional()
            })
            .ok()
            .flatten()
            .unwrap_or(0)
            != 0
    }

    fn group_bump(&self, gid: &str, col: &str) {
        let _ = self.core.state.with("merge_group_bump", |c| {
            c.execute(
                &format!("UPDATE merge_group SET {col} = {col} + 1 WHERE id = ?1"),
                [gid],
            )
        });
    }

    fn group_set_no_trash(&self, gid: &str) {
        let _ = self.core.state.with("merge_group_no_trash", |c| {
            c.execute("UPDATE merge_group SET no_trash = 1 WHERE id = ?1", [gid])
        });
    }

    /// Run a group's remaining steps in order; close it when none is left.
    fn run_group(&self, gid: &str, policy: &dyn CaseMergePolicy) -> Result<GroupEnd, Crash> {
        loop {
            let next: Option<Op> = self
                .core
                .ops
                .lock()
                .values()
                .find(|o| o.group.as_deref() == Some(gid) && o.state.live())
                .cloned();
            let Some(op) = next else { break };
            if !self.can_run() {
                return Ok(GroupEnd::Paused);
            }
            match self.run_step(gid, op)? {
                StepEnd::Done => {}
                StepEnd::Pause => return Ok(GroupEnd::Paused),
            }
        }
        self.close_group(gid, policy);
        Ok(GroupEnd::Done)
    }

    /// Audit the group, then mark it done and its scope free at once: whoever sees
    /// the merge finished (`done` under "Recent merges", or the scope no longer
    /// busy) finds its audit record. A crash between the audit and `done` audits the
    /// group again when it resumes (at least once, never lost).
    fn close_group(&self, gid: &str, policy: &dyn CaseMergePolicy) {
        let at = now_ms();
        // The record as it reads once closed.
        let rec = self
            .recent_merges()
            .into_iter()
            .find(|r| r.id == gid)
            .map(|r| MergeRecord {
                state: "done".into(),
                finished_at: Some(at),
                ..r
            });
        if let Some(rec) = &rec {
            policy.audit(rec);
        }
        self.merge.close_group(gid, || {
            let _ = self.core.state.with("merge_group_close", |c| {
                c.execute(
                    "UPDATE merge_group SET state = 'done', finished_at = ?2 WHERE id = ?1",
                    params![gid, at],
                )
            });
        });
        if let Some(rec) = rec {
            if rec.origin == "dedup" {
                tracing::info!(
                    "[STORAGE] Duplicate merge of {} done: {} moved, {} trashed, {} blocked",
                    RelPath::parse(&rec.winner).map_or_else(|| rec.winner.clone(), |p| shown(&p)),
                    rec.moved,
                    rec.trashed,
                    rec.blocked
                );
            } else {
                tracing::info!(
                    "[STORAGE] Case merge into {} done: {} moved, {} trashed, {} blocked",
                    rec.winner,
                    rec.moved,
                    rec.trashed,
                    rec.blocked
                );
            }
        }
    }

    /// A step met a block: record it, and drop the rest of its volume group.
    fn block_vg(&self, gid: &str, op: &Op, detail: String) {
        let vg = extra_of(op).map(|e| e.vg).unwrap_or_default();
        let this: Vec<RelPath> = op.src.clone().into_iter().collect();
        let that = vec![op.dest.clone()];
        if this == that {
            // A duplicate merge's step: both copies are at one path, so there is no
            // copy to keep (a `case-merge-blocked` row would offer to trash the path).
            self.core.state.conflict_once(
                ConflictRow::FailedOp,
                &op.dest.to_string(),
                op.target_pid.as_deref(),
                &detail,
            );
        } else {
            self.record_blocks(&[Block {
                this: if this.is_empty() { that.clone() } else { this },
                that,
                detail,
            }]);
        }
        self.group_bump(gid, "blocked");
        let rest: Vec<Op> = self
            .core
            .ops
            .lock()
            .values()
            .filter(|o| {
                o.group.as_deref() == Some(gid)
                    && o.state.live()
                    && extra_of(o).is_some_and(|e| e.vg == vg && e.step != StepKind::Finish)
            })
            .cloned()
            .collect();
        for o in rest {
            self.core.drop_op(&o, false);
        }
        self.core.drop_op(op, false);
    }

    fn run_step(&self, gid: &str, mut op: Op) -> Result<StepEnd, Crash> {
        let Some(ex) = extra_of(&op) else {
            self.core.drop_op(&op, false);
            return Ok(StepEnd::Done);
        };
        // The submission is answered by the server's hooks: before the op lock. A
        // duplicate merge trashes a refused loser all the same, as the reader's merge
        // does (the refusal is a lost-write conflict); kept, it would hold its member
        // open and the merge would never finish.
        if ex.step == StepKind::Submit {
            let go = match self.submitted(&op, &ex).as_deref() {
                Some("applied") => true,
                Some("refused") => self.group_origin(gid).as_deref() == Some("dedup"),
                _ => false,
            };
            if !go {
                self.core.drop_op(&op, false);
                return Ok(StepEnd::Done);
            }
        }
        let mut hooks = Vec::new();
        let end = {
            // One op at a time across the store (the §5.7 barrier).
            let _g = self.block_on("case merge", self.core.exec.lock());
            if !self.core.gate_open() {
                return Ok(StepEnd::Pause);
            }
            match ex.step {
                StepKind::Move => self.step_move(gid, &mut op, &ex, &mut hooks),
                StepKind::Trash | StepKind::Submit => {
                    self.step_trash(gid, &mut op, &ex, &mut hooks)
                }
                StepKind::Replace => self.step_replace(gid, &mut op, &mut hooks),
                StepKind::Finish => self.step_finish(gid, &mut op, &mut hooks),
            }
        };
        // The DB hooks run once per file, outside the op lock (a hook may write).
        for h in hooks {
            match h {
                HookCall::Merged(a, b, d) => self.core.hooks.merged(&a, &b, d),
                HookCall::Removed(a, d) => self.core.hooks.removed(&a, d),
            }
        }
        end
    }

    fn step_move(
        &self,
        gid: &str,
        op: &mut Op,
        ex: &Extra,
        hooks: &mut Vec<HookCall>,
    ) -> Result<StepEnd, Crash> {
        let (Some(pid), Some(src), Some(parent)) =
            (op.target_pid.clone(), op.src.clone(), op.parent_pid.clone())
        else {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        };
        let name = last_name(&op.dest).to_string();
        if op.state == OpState::Uploading
            && let Ok(Some(n)) = self.stat(&pid)
            && !n.trashed
            && n.parent.as_deref() == Some(parent.as_str())
            && nfc(&n.name_raw) == name
        {
            return self.moved(gid, op, &src, &pid, n, ex, hooks, None);
        }
        let current = match self.stat(&pid) {
            Ok(Some(n)) if !n.trashed => n,
            Ok(_) | Err(BErr::NotFound) => {
                // Gone meanwhile (the reader moved or deleted it): re-planned.
                self.core.drop_op(op, false);
                return Ok(StepEnd::Done);
            }
            Err(e) => {
                op.last_error = Some(e.to_string());
                let _ = self.core.save_op(op);
                return Ok(StepEnd::Pause);
            }
        };
        op.state = OpState::Uploading;
        let _ = self.core.save_op(op);
        self.core.fail_at("MERGE_MOVE")?;
        let new_parent =
            (current.parent.as_deref() != Some(parent.as_str())).then_some(parent.clone());
        let new_name = (nfc(&current.name_raw) != name).then_some(name.clone());
        let backend = self.core.backend.clone();
        let p2 = pid.clone();
        let window = self.block_on("case merge", self.core.commit_window());
        let r = self.block_on_as("case merge", Some(ApiClass::Background), async move {
            backend
                .update(&p2, new_parent.as_deref(), new_name.as_deref(), None)
                .await
        });
        match r {
            Ok(n) => {
                self.core.fail_at("MERGE_MOVED")?;
                self.moved(gid, op, &src, &pid, n, ex, hooks, Some(window))
            }
            Err(BErr::NameConflict | BErr::PreconditionFailed) => {
                // WebDAV `Overwrite: F` answered 412: the name is taken.
                self.block_vg(
                    gid,
                    op,
                    format!("{} already exists (the move was refused)", op.dest),
                );
                Ok(StepEnd::Done)
            }
            Err(BErr::NotFound) => {
                self.core.drop_op(op, false);
                Ok(StepEnd::Done)
            }
            Err(e) if retryable(&e) => {
                op.last_error = Some(e.to_string());
                let _ = self.core.save_op(op);
                Ok(StepEnd::Pause)
            }
            Err(e) => {
                self.block_vg(gid, op, format!("the move of {src} failed: {e}"));
                Ok(StepEnd::Done)
            }
        }
    }

    /// COMMITTED for a merge move: ledger, index, events, the DB hooks once, then (on
    /// providers that allow duplicate names) a direct re-list of the winner. `window`
    /// (the commit window, held since the provider call) is released once committed.
    #[allow(clippy::too_many_arguments)]
    fn moved(
        &self,
        gid: &str,
        op: &mut Op,
        src: &RelPath,
        pid: &str,
        n: RNode,
        ex: &Extra,
        hooks: &mut Vec<HookCall>,
        window: Option<OwnedRwLockReadGuard<()>>,
    ) -> Result<StepEnd, Crash> {
        let is_dir = n.is_dir;
        let parent = op.parent_pid.clone().unwrap_or_default();
        let name = nfc(&n.name_raw);
        let dest = op.dest.clone();
        self.core.commit_move(op, src, pid, n)?;
        drop(window);
        self.group_bump(gid, "moved");
        if ex.hooks {
            hooks.push(HookCall::Merged(
                self.core.abs(src),
                self.core.abs(&dest),
                is_dir,
            ));
        }
        if !self.core.caps.path_ids && !ex.expect_dup {
            match self.list(&parent) {
                Ok(kids) => {
                    let same = kids.iter().filter(|k| nfc(&k.name_raw) == name).count();
                    if same > 1 {
                        tracing::warn!(%dest, "case merge: the name now has duplicates; nothing is trashed");
                        self.core.state.conflict_once(
                            ConflictRow::DupFile,
                            &dest.to_string(),
                            Some(pid),
                            "two files have this name after a case merge (a reader upload raced it)",
                        );
                        self.group_set_no_trash(gid);
                    }
                }
                Err(e) => tracing::info!("case merge: re-listing the winner: {e}"),
            }
        }
        Ok(StepEnd::Done)
    }

    fn step_trash(
        &self,
        gid: &str,
        op: &mut Op,
        ex: &Extra,
        hooks: &mut Vec<HookCall>,
    ) -> Result<StepEnd, Crash> {
        let Some(pid) = op.target_pid.clone() else {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        };
        if self.group_flag(gid, "no_trash") {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        }
        let run_hooks = ex.hooks;
        self.trash_node(
            gid,
            op,
            &pid,
            ex.cver.as_deref(),
            run_hooks,
            ex.is_dir,
            hooks,
        )
    }

    /// Trash `pid` if it is still the judged version; COMMITTED as a ledger trash.
    #[allow(clippy::too_many_arguments)]
    fn trash_node(
        &self,
        gid: &str,
        op: &mut Op,
        pid: &str,
        cver: Option<&str>,
        run_hooks: bool,
        is_dir: bool,
        hooks: &mut Vec<HookCall>,
    ) -> Result<StepEnd, Crash> {
        let indexed = self.core.index.lock().node(pid);
        let current = match self.stat(pid) {
            Ok(Some(n)) if !n.trashed => n,
            Ok(_) | Err(BErr::NotFound) => {
                let node = indexed;
                if op.state == OpState::Uploading {
                    let dest = op.dest.clone();
                    self.core.commit_trash(op, pid, node)?;
                    self.group_bump(gid, "trashed");
                    if run_hooks {
                        hooks.push(HookCall::Removed(self.core.abs(&dest), is_dir));
                    }
                } else {
                    self.core.drop_op(op, false);
                }
                return Ok(StepEnd::Done);
            }
            Err(e) => {
                op.last_error = Some(e.to_string());
                let _ = self.core.save_op(op);
                return Ok(StepEnd::Pause);
            }
        };
        if !current.is_dir && cver.is_some_and(|c| c != current.cver) {
            // Another version since the plan: left for the next pass.
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        }
        op.state = OpState::Uploading;
        let _ = self.core.save_op(op);
        self.core.fail_at("MERGE_TRASH")?;
        let if_cver =
            (self.core.caps.cas_overwrite && !current.is_dir).then(|| current.cver.clone());
        let core = self.core.clone();
        let p2 = pid.to_string();
        let _window = self.block_on("case merge", self.core.commit_window());
        let r = self.block_on_as("case merge", Some(ApiClass::Background), async move {
            core.discard_node(&p2, false, if_cver.as_deref()).await
        });
        match r {
            Ok(_) | Err(BErr::NotFound) => {
                self.core.fail_at("MERGE_TRASHED")?;
                let dest = op.dest.clone();
                let node = indexed.or(Some(current));
                self.core.commit_trash(op, pid, node)?;
                self.group_bump(gid, "trashed");
                if run_hooks {
                    hooks.push(HookCall::Removed(self.core.abs(&dest), is_dir));
                }
                Ok(StepEnd::Done)
            }
            Err(e) if retryable(&e) => {
                op.last_error = Some(e.to_string());
                let _ = self.core.save_op(op);
                Ok(StepEnd::Pause)
            }
            Err(e) => {
                tracing::info!(%pid, "case merge: not trashed ({e}); re-planned later");
                self.core.drop_op(op, false);
                Ok(StepEnd::Done)
            }
        }
    }

    fn step_replace(
        &self,
        gid: &str,
        op: &mut Op,
        hooks: &mut Vec<HookCall>,
    ) -> Result<StepEnd, Crash> {
        let (Some(pid), Some(src), Some(staged)) = (
            op.target_pid.clone(),
            op.src.clone(),
            self.core.staged_path(op),
        ) else {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        };
        let Mode::Replace { base: Some(base) } = op.mode.clone() else {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        };
        let base = base.as_str().to_string();
        let current = match self.stat(&pid) {
            Ok(Some(n)) if !n.trashed => n,
            Ok(_) | Err(BErr::NotFound) => {
                self.block_vg(gid, op, format!("{} is gone", op.dest));
                return Ok(StepEnd::Done);
            }
            Err(e) => {
                op.last_error = Some(e.to_string());
                let _ = self.core.save_op(op);
                return Ok(StepEnd::Pause);
            }
        };
        if current.cver != base {
            // An interrupted replace that did land is committed; anything else is
            // another writer: the winner's file stays as it is.
            if op.state == OpState::Uploading {
                let core = self.core.clone();
                let (o, n) = (op.clone(), current.clone());
                let proven = self.block_on("case merge", async move {
                    core.content_proof(&n, &o).await == Proof::Proven
                });
                if proven {
                    return self.replaced(gid, op, &src, &pid, current, hooks);
                }
            }
            self.block_vg(
                gid,
                op,
                format!("{} changed since the merge judged it", op.dest),
            );
            return Ok(StepEnd::Done);
        }
        op.state = OpState::Uploading;
        let _ = self.core.save_op(op);
        self.core.fail_at("MERGE_REPLACE")?;
        let (Some(sha), size) = (op.sha256, op.size) else {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        };
        let if_cver = self.core.caps.cas_overwrite.then(|| base.clone());
        let backend = self.core.backend.clone();
        let (p2, s2, oid) = (pid.clone(), staged.clone(), op.op_id());
        // Held through `replaced` (its trash of a superseded node included).
        let _window = self.block_on("case merge", self.core.commit_window());
        let r = self.block_on_as("case merge", Some(ApiClass::Background), async move {
            let mut ctx = OpCtx {
                op_id: oid,
                resume: None,
            };
            backend
                .upload(
                    UploadTarget::Replace {
                        pid: &p2,
                        if_cver: if_cver.as_deref(),
                    },
                    UploadSource::File {
                        path: &s2,
                        size,
                        sha256: sha,
                    },
                    &mut ctx,
                )
                .await
        });
        match r {
            Ok(n) => {
                self.core.fail_at("uploaded")?;
                if self.core.hash_proof(&n, op, None) == Proof::Mismatch {
                    self.block_vg(gid, op, format!("{} was stored with other bytes", op.dest));
                    return Ok(StepEnd::Done);
                }
                self.replaced(gid, op, &src, &pid, n, hooks)
            }
            Err(BErr::PreconditionFailed | BErr::NameConflict | BErr::NotFound) => {
                // The winner's file changed (a 412) or went: it is left as it is.
                self.block_vg(
                    gid,
                    op,
                    format!("{} changed while the merge replaced it", op.dest),
                );
                Ok(StepEnd::Done)
            }
            Err(e) if retryable(&e) => {
                op.last_error = Some(e.to_string());
                let _ = self.core.save_op(op);
                Ok(StepEnd::Pause)
            }
            Err(e) => {
                self.block_vg(gid, op, format!("could not replace {}: {e}", op.dest));
                Ok(StepEnd::Done)
            }
        }
    }

    /// COMMITTED for an in-place replace of the winner's main sidecar; then, where
    /// the provider's replace made a new node, the old node goes to the trash.
    fn replaced(
        &self,
        gid: &str,
        op: &mut Op,
        src: &RelPath,
        old_pid: &str,
        n: RNode,
        hooks: &mut Vec<HookCall>,
    ) -> Result<StepEnd, Crash> {
        let parent = op.parent_pid.clone().unwrap_or_default();
        let dest = op.dest.clone();
        let new_pid = n.pid.clone();
        let old = self.core.index.lock().node(old_pid);
        self.core.commit_put(op, n, &parent)?;
        self.group_bump(gid, "moved");
        hooks.push(HookCall::Merged(
            self.core.abs(src),
            self.core.abs(&dest),
            false,
        ));
        if new_pid != old_pid {
            let core = self.core.clone();
            let o2 = old_pid.to_string();
            let r = self.block_on_as("case merge", Some(ApiClass::Background), async move {
                core.discard_node(&o2, false, None).await
            });
            match r {
                Ok(_) | Err(BErr::NotFound) => {
                    if self.core.state.ledger_rows(old_pid).is_empty() {
                        if let Some(o) = &old {
                            let _ = self.core.state.ledger_put(&crate::state::LedgerRow {
                                pid: old_pid.to_string(),
                                cver: o.cver.clone(),
                                parent_pid: o.parent.clone(),
                                name: nfc(&o.name_raw),
                                sha256: None,
                                op_id: Some(op.op_id()),
                                trashed: true,
                                echo_only: true,
                            });
                        }
                    } else {
                        let _ = self.core.state.ledger_trash(old_pid);
                    }
                    let idx = self.core.index.lock();
                    let r = idx.begin().and_then(|tx| {
                        idx.remove(&tx, old_pid)?;
                        tx.commit()
                    });
                    if let Err(e) = r {
                        tracing::warn!("case merge: index: {e}");
                    }
                    self.group_bump(gid, "trashed");
                }
                Err(e) => tracing::warn!(
                    pid = old_pid,
                    "case merge: the replaced main sidecar stays beside the new one: {e}"
                ),
            }
        }
        Ok(StepEnd::Done)
    }

    /// A `Submit` step's first half: the losing `series.json` as a submission into
    /// the winner's series key (§9.3): its answer (`applied`, so it may be trashed,
    /// or `refused`), or `None` when there is none to give.
    fn submitted(&self, op: &Op, ex: &Extra) -> Option<String> {
        let (Some(pid), Some(cver), Some(series)) =
            (op.target_pid.clone(), ex.cver.clone(), ex.series.clone())
        else {
            return None;
        };
        let (id, state, _) = self
            .core
            .state
            .submission_open(&series, &pid, &cver, true)?;
        if matches!(state.as_str(), "fetching" | "queued") {
            let core = self.core.clone();
            self.block_on("case merge", core.process_submission(id));
        }
        let state = self
            .core
            .state
            .submission_get(id)
            .map(|r| r.state)
            .unwrap_or_default();
        if state != "applied" {
            tracing::info!(%series, state, "case merge: the losing series.json was not applied");
        }
        Some(state)
    }

    /// The group's origin (`auto`, `admin`, `dedup`).
    fn group_origin(&self, gid: &str) -> Option<String> {
        self.core
            .state
            .with("merge_group_origin", |c| {
                c.query_row("SELECT origin FROM merge_group WHERE id = ?1", [gid], |r| {
                    r.get(0)
                })
            })
            .ok()
    }

    /// Queued Deferred writes (not yet running) under `from` move to `to`: the path,
    /// the pending marker, and the parent folder they upload into.
    fn rebase_pending(&self, from: &RelPath, to: &RelPath) {
        let ops: Vec<Op> = self
            .core
            .ops
            .lock()
            .values()
            .filter(|o| {
                o.group.is_none()
                    && o.durability == Durability::Deferred
                    && matches!(o.state, OpState::Staged | OpState::Held)
                    && o.dest.segments().starts_with(from.segments())
            })
            .cloned()
            .collect();
        for mut o in ops {
            let Some(dest) = RelPath::from_segments(
                to.segments()
                    .iter()
                    .chain(&o.dest.segments()[from.segments().len()..])
                    .cloned(),
            ) else {
                continue;
            };
            tracing::info!(from = %o.dest, to = %dest, "case merge: a queued write follows the merge");
            if let Ok(ch) = self.core.index.lock().clear_pending(&o.op_id()) {
                self.core.emit_changes(&ch);
            }
            o.dest = dest;
            o.parent_pid = self.core.parent_pid_of(&o.dest);
            if self.core.save_op(&o).is_ok()
                && let Err(e) = self.core.stage_deferred(o)
            {
                tracing::warn!("case merge: re-queueing a write: {e}");
            }
        }
    }

    fn step_finish(
        &self,
        gid: &str,
        op: &mut Op,
        hooks: &mut Vec<HookCall>,
    ) -> Result<StepEnd, Crash> {
        let Some(pid) = op.target_pid.clone() else {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        };
        if self.group_flag(gid, "no_trash") {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        }
        // Bunko's own writes still queued under the losing path land in the winner
        // (the folder they were queued for is about to go).
        if let Some(w) = op.src.clone() {
            self.rebase_pending(&op.dest, &w);
        }
        // §5.9 Finishing: only a member a direct listing shows empty.
        match self.list(&pid) {
            Ok(kids) if kids.is_empty() => {}
            Ok(_) | Err(BErr::NotFound) => {
                self.core.drop_op(op, false);
                return Ok(StepEnd::Done);
            }
            Err(e) if retryable(&e) => return Ok(StepEnd::Pause),
            Err(_) => {
                self.core.drop_op(op, false);
                return Ok(StepEnd::Done);
            }
        }
        self.trash_node(gid, op, &pid, None, false, true, hooks)
    }
}
