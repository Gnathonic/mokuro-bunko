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
//!   differing main sidecars keep the one [`CaseMergePolicy::prefer_main`] picks (the
//!   other, stamped or not, first becomes the winner's `pre-upgrade` layer); differing
//!   archives, a stamped layer that differs, or a file against a folder leave the
//!   whole group in the loser as a `case-merge-blocked` conflict. A loser
//!   `series.json` is ingested as a submission into the winner's series key, then
//!   trashed. A loser recreated too often stops being merged ([`RECREATE_LIMIT`]).
//! - **Durability:** each merge is one journal group: a `merge_group` row in
//!   `state.sqlite` and its steps as outbox ops (origin `Admin`, `Deferred`,
//!   `group_id`), which the outbox worker never runs. A group resumes after a crash,
//!   only once the gate (the lease fence) is open again, and pauses while the store is
//!   not `Online` (a `Suspect` mid-merge included).
//! - **Commits:** a move or trash rewrites its ledger row and the index at COMMITTED
//!   (the outbox's own `commit_move`/`commit_trash`), so the feed's echo is exact: no
//!   hooks run twice and nothing counts toward D19. The DB hooks run once per file
//!   ([`CloudHooks::merged`](crate::CloudHooks::merged) for a move,
//!   [`CloudHooks::removed`](crate::CloudHooks::removed) for a trashed loser file).
//! - **Finishing:** an emptied losing member node is trashed only after a direct
//!   listing shows it empty; anything else is re-planned in a later pass.
//!
//! One std thread (`bunko-store-merge`) runs the passes: planning reads file bytes
//! through the store's blocking content methods, which never run on the store runtime.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use bunko_store::{Durability, Health, LibraryStore, Mode, Origin, WatchEvent};
use parking_lot::{Condvar, Mutex};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::backend::{BErr, Backend, OpCtx, Pid, RNode, UploadSource, UploadTarget};
use crate::cache::{SMALL_MAX, now_ms};
use crate::governor::{self, ApiClass};
use crate::index::{ROOT_EID, RelPath};
use crate::names::{fold, nfc};
use crate::outbox::{Crash, Op, OpKind, OpState, Proof};
use crate::state::{ConflictRow, random_id};
use crate::store::{RemoteStore, Ver};

/// After this many finished merges of one losing spelling within [`RECREATE_WINDOW`]
/// (the first and three recreations), a further recreation is not merged: the reader
/// keeps making the folder again, and a `case-merge-blocked` conflict says so.
pub const RECREATE_LIMIT: u64 = 4;
/// The recreate-loop guard's window (24 h).
pub const RECREATE_WINDOW: Duration = Duration::from_secs(24 * 3600);
/// How long store events are remembered for the quiet check and the re-plan memo.
const EVENT_RETAIN: Duration = Duration::from_secs(3600);
const EVENT_CAP: usize = 100_000;
/// The detail of the recreate-loop guard's conflict.
pub const RECREATING: &str = "Mokuro Reader keeps recreating this folder";

// ---- the server's side ------------------------------------------------------------

/// Which of two differing main sidecars a merge keeps ([`CaseMergePolicy::prefer_main`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeChoice {
    /// The first (the winner's).
    A,
    /// The second (the loser's).
    B,
    /// Neither, or both, match the primary recipe, or upgrades are off: the volume
    /// group stays in the losing spelling (`case-merge-blocked`).
    Block,
}

/// The admin's answer to a `case-merge-blocked` conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveChoice {
    /// Keep the losing spelling's copy (`this`): the winner's goes to the trash, and
    /// the next merge pass moves this one in.
    KeepThis,
    /// Keep the winner's copy (`that`): the losing spelling's goes to the trash.
    KeepThat,
}

impl ResolveChoice {
    /// `this` / `that` (the admin API's words).
    pub fn parse(s: &str) -> Option<ResolveChoice> {
        match s {
            "this" => Some(ResolveChoice::KeepThis),
            "that" => Some(ResolveChoice::KeepThat),
            _ => None,
        }
    }
}

/// What a case merge needs from the server (installed with
/// [`RemoteStore::set_case_merge_policy`]; no merge runs before).
pub trait CaseMergePolicy: Send + Sync {
    /// Two differing main sidecars beside identical archives (`a` the winner's, `b`
    /// the loser's, both store paths): the one whose recipe matches the primary
    /// row's output-affecting recipe (`Recipe::matches`, not `Verdict::Current`).
    /// `Block` when `ocr.upgrade.enabled` is off, or neither or both match.
    fn prefer_main(&self, a: &Path, b: &Path) -> MergeChoice;
    /// A layer id Bunko never stamps (`edit`, `pre-upgrade*`): on a collision with
    /// different content it moves in under the next free id (`-2`, `-3`).
    fn is_unstamped_layer(&self, id: &str) -> bool {
        is_unstamped_layer_id(id)
    }
    /// An OCR claim is running or spooling, or an upgrade is being judged or
    /// swapped, for the volume at `path` or any volume under it.
    fn busy(&self, _path: &Path) -> bool {
        false
    }
    /// A merge group finished (the audit record).
    fn audit(&self, _merge: &MergeRecord) {}
}

/// `edit`, `edit-<n>`, `pre-upgrade`, `pre-upgrade-<n>`: layers whose content shows a
/// human or a preserved file, never Bunko's model output (D4).
pub fn is_unstamped_layer_id(id: &str) -> bool {
    let id = id.to_lowercase();
    matches!(strip_counter(&id), "edit" | "pre-upgrade")
}

/// `pre-upgrade` of `pre-upgrade-3`; `id` itself when it has no `-<n>` counter.
fn strip_counter(id: &str) -> &str {
    match id.rsplit_once('-') {
        Some((b, n)) if !b.is_empty() && !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) => {
            b
        }
        _ => id,
    }
}

/// One merge group, as the admin panel's "Recent merges" lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergeRecord {
    pub id: String,
    /// The folder the variants are in (library path; empty: the root).
    pub parent: String,
    /// The winning spelling (library path).
    pub winner: String,
    pub losers: Vec<String>,
    /// `auto`, or `admin` (Keep this / that copy).
    pub origin: String,
    /// `open` (running or paused) or `done`.
    pub state: String,
    /// Unix ms.
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub moved: u64,
    pub trashed: u64,
    pub blocked: u64,
}

// ---- scopes, events ---------------------------------------------------------------

/// The paths one variant set covers, folded: a folder's subtree, or (same-folder file
/// variants) the files of one folded stem.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Scope {
    dir: Vec<String>,
    key: String,
    stem: bool,
}

impl Scope {
    fn covers(&self, segs: &[String]) -> bool {
        if segs.len() <= self.dir.len() || segs[..self.dir.len()] != self.dir[..] {
            return false;
        }
        let name = &segs[self.dir.len()];
        if self.stem {
            segs.len() == self.dir.len() + 1
                && (*name == self.key || name.starts_with(&format!("{}.", self.key)))
        } else {
            *name == self.key
        }
    }

    fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

fn folded(rel: &RelPath) -> Vec<String> {
    rel.segments().iter().map(|s| fold(s)).collect()
}

fn parent_of(rel: &RelPath) -> RelPath {
    let s = rel.segments();
    RelPath::from_segments(s[..s.len().saturating_sub(1)].iter().cloned()).unwrap_or_default()
}

fn last_name(rel: &RelPath) -> &str {
    rel.segments().last().map_or("", String::as_str)
}

/// The merge's shared state: the policy, open scopes, recent events, the thread.
pub(crate) struct MergeState {
    policy: Mutex<Option<Arc<dyn CaseMergePolicy>>>,
    /// Scopes a merge works on (`Some(group)`), or is checking (`None`).
    busy: Mutex<Vec<(Option<String>, Scope)>>,
    events: Mutex<VecDeque<(Instant, Vec<String>)>>,
    /// Scopes planned with nothing to do (only blocks), and when.
    memo: Mutex<HashMap<Scope, Instant>>,
    started: Instant,
    quiet: Duration,
    interval: Duration,
    woken: Mutex<bool>,
    cv: Condvar,
    stop: AtomicBool,
    root: PathBuf,
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
            .filter_map(|c| c.as_os_str().to_str().map(fold))
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

    fn unmark_group(&self, group: &str) {
        self.busy
            .lock()
            .retain(|(g, _)| g.as_deref() != Some(group));
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

// ---- journal steps ----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum StepKind {
    /// Move a loser node into the winner's target node.
    Move,
    /// Trash a file the merge superseded (identical, kept, ingested).
    Trash,
    /// Write a superseded main sidecar as the winner's `pre-upgrade` layer.
    Layer,
    /// Replace the winner's main sidecar in place with the loser's bytes (a new
    /// version based on the judged one); a provider whose replace makes a new node
    /// has the old one trashed after the commit.
    Replace,
    /// Ingest a loser `series.json` as a submission, then trash it once applied.
    Submit,
    /// Trash an emptied losing member folder (after a direct listing shows it empty).
    Finish,
}

/// A step's own fields, in the outbox row's `resume` column.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Extra {
    step: StepKind,
    /// The volume group (a block skips the rest of it).
    vg: String,
    #[serde(default)]
    is_dir: bool,
    /// The version the plan judged (a trash of another version is skipped).
    #[serde(default)]
    cver: Option<String>,
    /// Run the DB hooks for this step.
    #[serde(default)]
    hooks: bool,
    /// `Submit`: the winner's `series.json` path the submission is for.
    #[serde(default)]
    series: Option<String>,
}

fn extra_of(op: &Op) -> Option<Extra> {
    op.extra
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
}

/// One planned step.
#[derive(Debug, Clone)]
struct Planned {
    kind: StepKind,
    vg: String,
    pid: Pid,
    cver: Option<String>,
    src: Option<RelPath>,
    dest: RelPath,
    parent_pid: Option<Pid>,
    is_dir: bool,
    hooks: bool,
    bytes: Option<Vec<u8>>,
    series: Option<RelPath>,
}

impl Planned {
    fn mv(vg: &str, n: &RNode, src: RelPath, dest: RelPath, target: &str) -> Planned {
        Planned {
            kind: StepKind::Move,
            vg: vg.to_string(),
            pid: n.pid.clone(),
            cver: Some(n.cver.clone()),
            src: Some(src),
            dest,
            parent_pid: Some(target.to_string()),
            is_dir: n.is_dir,
            hooks: true,
            bytes: None,
            series: None,
        }
    }

    fn trash(vg: &str, n: &RNode, path: RelPath) -> Planned {
        Planned {
            kind: StepKind::Trash,
            vg: vg.to_string(),
            pid: n.pid.clone(),
            cver: Some(n.cver.clone()),
            src: None,
            dest: path,
            parent_pid: None,
            is_dir: n.is_dir,
            hooks: true,
            bytes: None,
            series: None,
        }
    }
}

/// A group or folder the merge leaves in the losing spelling.
#[derive(Debug, Clone)]
struct Block {
    this: Vec<RelPath>,
    that: Vec<RelPath>,
    detail: String,
}

/// A merge's steps, in the order they run: every layer is committed before
/// anything is trashed; moves and in-place replaces next; loser files are trashed
/// only after every move (a duplicate a move meets stops all trashing); emptied
/// folders last, deepest first.
#[derive(Default)]
struct Plan {
    layers: Vec<Planned>,
    steps: Vec<Planned>,
    submits: Vec<Planned>,
    l_trash: Vec<Planned>,
    finishes: Vec<Planned>,
    blocks: Vec<Block>,
    /// Folded names planned into each target node (moves and layers).
    taken: HashMap<Pid, HashSet<String>>,
    /// A losing member listed empty: its finish is work of its own.
    empty_loser: bool,
}

impl Plan {
    fn work(&self) -> bool {
        !(self.layers.is_empty()
            && self.steps.is_empty()
            && self.submits.is_empty()
            && self.l_trash.is_empty()
            && !self.empty_loser)
    }

    fn ordered(&self) -> impl Iterator<Item = &Planned> {
        self.layers
            .iter()
            .chain(&self.steps)
            .chain(&self.submits)
            .chain(&self.l_trash)
            .chain(&self.finishes)
    }
}

/// A plan cannot be made now (a read failed, a transfer quota): try again later.
type Defer = String;

fn defer(e: impl std::fmt::Display) -> Defer {
    e.to_string()
}

// ---- names ------------------------------------------------------------------------

/// What one file of a volume group is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Archive,
    Main,
    Layer(String),
    Cover,
    Other,
}

/// A listed file: its NFC name split into stem and suffix.
#[derive(Debug, Clone)]
struct Named {
    node: RNode,
    name: String,
    stem: String,
    suffix: String,
    part: Part,
    key: String,
}

fn strip_ci<'a>(name: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = name.len().checked_sub(suffix.len())?;
    (name.is_char_boundary(cut) && name[cut..].eq_ignore_ascii_case(suffix)).then(|| &name[..cut])
}

fn is_layer_id(id: &str) -> bool {
    (1..=32).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && id.bytes().any(|b| b.is_ascii_lowercase())
}

/// The folded stems of the archives among `names`.
fn archive_stems<'a>(names: impl Iterator<Item = &'a str>) -> HashSet<String> {
    names
        .filter_map(|n| strip_ci(n, ".cbz").or_else(|| strip_ci(n, ".zip")))
        .map(fold)
        .collect()
}

/// `(stem, suffix, part)` of a file name, given the folder's archive stems.
fn split_name(name: &str, stems: &HashSet<String>) -> (String, String, Part) {
    if let Some(s) = strip_ci(name, ".cbz").or_else(|| strip_ci(name, ".zip")) {
        return (s.to_string(), name[s.len()..].to_string(), Part::Archive);
    }
    let base = strip_ci(name, ".mokuro.gz").or_else(|| strip_ci(name, ".mokuro"));
    if let Some(base) = base {
        if !stems.contains(&fold(base))
            && let Some((b, id)) = base.rsplit_once('.')
            && stems.contains(&fold(b))
            && is_layer_id(id)
        {
            return (
                b.to_string(),
                name[b.len()..].to_string(),
                Part::Layer(id.to_string()),
            );
        }
        return (base.to_string(), name[base.len()..].to_string(), Part::Main);
    }
    if let Some(s) = strip_ci(name, ".webp") {
        return (s.to_string(), name[s.len()..].to_string(), Part::Cover);
    }
    (name.to_string(), String::new(), Part::Other)
}

fn named(n: RNode, stems: &HashSet<String>) -> Named {
    let name = nfc(&n.name_raw);
    let (stem, suffix, part) = split_name(&name, stems);
    let key = match part {
        Part::Other => format!("\u{0}{}", fold(&name)),
        _ => fold(&stem),
    };
    Named {
        node: n,
        name,
        stem,
        suffix,
        part,
        key,
    }
}

/// `(scope key, group key)` of a file variant set: the folded stem (a file that is
/// no volume part: its folded name), and the [`Named::key`] its files share.
fn file_key(name: &str, stems: &HashSet<String>) -> (String, String) {
    let (stem, _, part) = split_name(name, stems);
    match part {
        Part::Other => (fold(name), format!("\u{0}{}", fold(name))),
        _ => (fold(&stem), fold(&stem)),
    }
}

/// The `pre-upgrade` layer bytes of a superseded main sidecar (D4's shape): its JSON
/// with no `ocr_engine` and a `preserved` block. `None`: not a JSON object.
pub fn preserved_layer(old: &[u8], replaced_by: &str) -> Option<Vec<u8>> {
    let mut doc: serde_json::Value = serde_json::from_slice(old).ok()?;
    let obj = doc.as_object_mut()?;
    obj.remove("ocr_engine");
    obj.insert(
        "preserved".into(),
        serde_json::json!({
            "by": "mokuro-bunko",
            "reason": "case-merge",
            "replaced_at": crate::status::format_iso_s(now_ms()),
            "sha256": hex::encode(Sha256::digest(old)),
            "replaced_by": replaced_by,
        }),
    );
    serde_json::to_vec(&doc).ok()
}

fn preserved_sha(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .get("preserved")?
        .get("sha256")?
        .as_str()
        .map(str::to_lowercase)
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
                    steps.push(Planned::trash("admin", &n, rel.clone()));
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
            (fold(&name), false)
        } else {
            (file_key(&name, &stems).0, true)
        };
        let scope = Scope {
            dir: folded(&parent),
            key,
            stem,
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

    /// One pass: resume open groups, then merge the variant sets that are ready.
    pub(crate) fn merge_pass(&self) {
        let Some(policy) = self.merge.policy() else {
            return;
        };
        if !self.can_run() {
            return;
        }
        for gid in self.open_group_ids() {
            match self.run_group(&gid, policy.as_ref()) {
                Ok(GroupEnd::Done) => {}
                Ok(GroupEnd::Paused) | Err(Crash) => return,
            }
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
        if self.merge.busy.lock().iter().any(|(_, s)| s == scope) {
            return Ok(());
        }
        // Quiet: no change, no pending op, for quiet_s on either variant.
        if !self.merge.quiet_now(scope) || self.pending_in(scope) {
            return Ok(());
        }
        // Planned before with nothing to do, and nothing changed since.
        if let Some(at) = self.merge.memo.lock().get(scope).copied()
            && self.merge.last_event(scope).is_none_or(|t| t <= at)
            && at.elapsed() < EVENT_RETAIN
        {
            return Ok(());
        }
        let (winner, losers) = self.set_paths(set);
        if losers.is_empty() {
            return Ok(());
        }
        if !scope.stem && self.recreations(scope) >= RECREATE_LIMIT {
            self.block_recreated(&winner, &losers);
            self.merge.memo.lock().insert(scope.clone(), Instant::now());
            return Ok(());
        }
        // Busy first, then the policy's check: a claim cannot slip in between.
        self.merge.mark(None, scope.clone());
        let busy_paths = self.busy_paths(set, &winner, &losers);
        if busy_paths.iter().any(|p| policy.busy(p)) {
            self.merge.unmark_probe(scope);
            return Ok(());
        }
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
            self.merge.memo.lock().insert(scope.clone(), Instant::now());
            self.merge.unmark_probe(scope);
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
        self.run_group(&gid, policy).map(|_| ())
    }

    /// The winner and losers of a set (library paths of the variant entries).
    fn set_paths(&self, set: &VariantSet) -> (RelPath, Vec<RelPath>) {
        let idx = self.core.index.lock();
        let kids = idx.children(set.parent_eid);
        let stems = archive_stems(kids.iter().map(|k| k.name.as_str()));
        let vars: Vec<crate::index::Entry> = if set.scope.stem {
            // The entries of the stem's files, the archives' first, oldest first; one
            // per exact stem (the first is the winner's spelling).
            let mut v: Vec<crate::index::Entry> = kids
                .into_iter()
                .filter(|k| !k.is_dir && file_key(&k.name, &stems).1 == set.group)
                .collect();
            let archive = |e: &crate::index::Entry| split_name(&e.name, &stems).2 == Part::Archive;
            v.sort_by(|a, b| {
                (!archive(a), a.created_ms, &a.winner_pid).cmp(&(
                    !archive(b),
                    b.created_ms,
                    &b.winner_pid,
                ))
            });
            let mut stems_seen = HashSet::new();
            v.retain(|e| stems_seen.insert(split_name(&e.name, &stems).0));
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

    /// Finished automatic merges of this scope in the last 24 h.
    fn recreations(&self, scope: &Scope) -> u64 {
        let since = now_ms() - RECREATE_WINDOW.as_millis() as i64;
        self.core
            .state
            .with("recreations", |c| {
                c.query_row(
                    "SELECT COUNT(*) FROM merge_group WHERE scope = ?1 AND origin = 'auto' \
                     AND state = 'done' AND created_at >= ?2 AND moved + trashed > 0",
                    params![scope.json(), since],
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

    fn plan_set(&self, set: &VariantSet, policy: &dyn CaseMergePolicy) -> Result<Plan, Defer> {
        let mut plan = Plan::default();
        let (winner, losers) = self.set_paths(set);
        if set.scope.stem {
            // Same-folder file variants: one folder, renames.
            let (members, target) = {
                let idx = self.core.index.lock();
                let members: Vec<Pid> = idx
                    .members(set.parent_eid)
                    .into_iter()
                    .map(|n| n.pid)
                    .collect();
                let target = if set.parent_eid == ROOT_EID {
                    idx.roots().first().cloned()
                } else {
                    idx.entry(set.parent_eid).and_then(|e| e.winner_pid)
                };
                (members, target)
            };
            let target = target.ok_or_else(|| defer("the folder has no node"))?;
            let mut files = Vec::new();
            for m in &members {
                files.extend(
                    self.list(m)
                        .map_err(defer)?
                        .into_iter()
                        .filter(|n| !n.is_dir),
                );
            }
            let names: Vec<String> = files.iter().map(|n| nfc(&n.name_raw)).collect();
            let stems = archive_stems(names.iter().map(String::as_str));
            let all: Vec<Named> = files.into_iter().map(|n| named(n, &stems)).collect();
            let wstem = split_name(last_name(&winner), &stems).0;
            let group: Vec<Named> = all.iter().filter(|n| n.key == set.group).cloned().collect();
            let (wg, lg): (Vec<Named>, Vec<Named>) =
                group.into_iter().partition(|n| n.stem == wstem);
            let taken: HashSet<String> = all.iter().map(|n| fold(&n.name)).collect();
            plan.taken.insert(target.clone(), taken);
            if !lg.is_empty() {
                self.pair(
                    policy,
                    &set.group,
                    &wg,
                    &lg,
                    &target,
                    &set.parent,
                    &set.parent,
                    &mut plan,
                )?;
            }
            return Ok(plan);
        }
        // Folders. A file that folds like them stays (file vs folder).
        let (w_members, files_vs) = {
            let idx = self.core.index.lock();
            let w_members: Vec<Pid> = idx
                .lookup(&winner)
                .map(|e| idx.members(e.eid).into_iter().map(|n| n.pid).collect())
                .unwrap_or_default();
            let files: Vec<RelPath> = idx
                .variants(set.parent_eid, &set.scope.key)
                .into_iter()
                .filter(|e| !e.is_dir)
                .filter_map(|e| set.parent.join(e.name))
                .collect();
            (w_members, files)
        };
        for f in files_vs {
            plan.blocks.push(Block {
                this: vec![f],
                that: vec![winner.clone()],
                detail: "a file and a folder whose names differ only in case".into(),
            });
        }
        if w_members.is_empty() {
            return Err(defer("the winning folder has no node"));
        }
        for l in &losers {
            let l_members: Vec<Pid> = {
                let idx = self.core.index.lock();
                idx.lookup(l)
                    .map(|e| idx.members(e.eid).into_iter().map(|n| n.pid).collect())
                    .unwrap_or_default()
            };
            if l_members.is_empty() {
                continue;
            }
            self.plan_dirs(policy, &winner, &w_members, l, &l_members, &mut plan)?;
        }
        Ok(plan)
    }

    /// Merge the folder `l` (members `l_members`) into `w` (members `w_members`,
    /// the first is the target), recursively.
    fn plan_dirs(
        &self,
        policy: &dyn CaseMergePolicy,
        w: &RelPath,
        w_members: &[Pid],
        l: &RelPath,
        l_members: &[Pid],
        plan: &mut Plan,
    ) -> Result<(), Defer> {
        let target = w_members[0].clone();
        let mut w_kids = Vec::new();
        for m in w_members {
            w_kids.extend(self.list(m).map_err(defer)?);
        }
        let mut l_kids = Vec::new();
        for m in l_members {
            let kids = self.list(m).map_err(defer)?;
            plan.empty_loser |= kids.is_empty();
            l_kids.extend(kids);
        }
        let taken: HashSet<String> = w_kids.iter().map(|n| fold(&nfc(&n.name_raw))).collect();
        plan.taken.entry(target.clone()).or_default().extend(taken);
        // Folders: same fold → recursive; else move in whole.
        let mut w_dirs: HashMap<String, Vec<RNode>> = HashMap::new();
        for n in w_kids.iter().filter(|n| n.is_dir) {
            w_dirs
                .entry(fold(&nfc(&n.name_raw)))
                .or_default()
                .push(n.clone());
        }
        for v in w_dirs.values_mut() {
            v.sort_by(|a, b| (a.created_ms, &a.pid).cmp(&(b.created_ms, &b.pid)));
        }
        let w_file_folds: HashMap<String, String> = w_kids
            .iter()
            .filter(|n| !n.is_dir)
            .map(|n| (fold(&nfc(&n.name_raw)), nfc(&n.name_raw)))
            .collect();
        let mut l_dirs: Vec<(String, Vec<RNode>)> = Vec::new();
        for n in l_kids.iter().filter(|n| n.is_dir) {
            let name = nfc(&n.name_raw);
            match l_dirs.iter_mut().find(|(k, _)| *k == name) {
                Some((_, v)) => v.push(n.clone()),
                None => l_dirs.push((name, vec![n.clone()])),
            }
        }
        for (name, nodes) in l_dirs {
            let f = fold(&name);
            let Some(lsub) = l.join(name.clone()) else {
                continue;
            };
            if let Some(wds) = w_dirs.get(&f) {
                let wname = wds
                    .iter()
                    .find(|n| nfc(&n.name_raw) == name)
                    .map_or_else(|| nfc(&wds[0].name_raw), |n| nfc(&n.name_raw));
                let mut sub: Vec<&RNode> =
                    wds.iter().filter(|n| nfc(&n.name_raw) == wname).collect();
                sub.sort_by(|a, b| (a.created_ms, &a.pid).cmp(&(b.created_ms, &b.pid)));
                let sub_members: Vec<Pid> = sub.iter().map(|n| n.pid.clone()).collect();
                let Some(wsub) = w.join(wname) else { continue };
                let mut lsorted = nodes.clone();
                lsorted.sort_by(|a, b| (a.created_ms, &a.pid).cmp(&(b.created_ms, &b.pid)));
                let l_sub: Vec<Pid> = lsorted.iter().map(|n| n.pid.clone()).collect();
                self.plan_dirs(policy, &wsub, &sub_members, &lsub, &l_sub, plan)?;
            } else if let Some(wf) = w_file_folds.get(&f) {
                plan.blocks.push(Block {
                    this: vec![lsub],
                    that: w.join(wf.clone()).into_iter().collect(),
                    detail: "a file and a folder whose names differ only in case".into(),
                });
            } else {
                let Some(dest) = w.join(name.clone()) else {
                    continue;
                };
                for n in &nodes {
                    plan.steps.push(Planned::mv(
                        &format!("{l}/{f}"),
                        n,
                        lsub.clone(),
                        dest.clone(),
                        &target,
                    ));
                }
                plan.taken.entry(target.clone()).or_default().insert(f);
            }
        }
        // Files, as volume groups by folded stem.
        let w_files: Vec<RNode> = w_kids.iter().filter(|n| !n.is_dir).cloned().collect();
        let l_files: Vec<RNode> = l_kids.iter().filter(|n| !n.is_dir).cloned().collect();
        let names: Vec<String> = w_files
            .iter()
            .chain(&l_files)
            .map(|n| nfc(&n.name_raw))
            .collect();
        let stems = archive_stems(names.iter().map(String::as_str));
        let wn: Vec<Named> = w_files.into_iter().map(|n| named(n, &stems)).collect();
        let ln: Vec<Named> = l_files.into_iter().map(|n| named(n, &stems)).collect();
        let w_dir_folds: HashSet<&String> = w_dirs.keys().collect();
        let mut keys: Vec<String> = Vec::new();
        for n in &ln {
            if !keys.contains(&n.key) {
                keys.push(n.key.clone());
            }
        }
        for key in keys {
            let lg: Vec<Named> = ln.iter().filter(|n| n.key == key).cloned().collect();
            if let Some(f) = lg.iter().find(|n| w_dir_folds.contains(&fold(&n.name))) {
                plan.blocks.push(Block {
                    this: lg.iter().filter_map(|n| l.join(n.name.clone())).collect(),
                    that: w.join(f.name.clone()).into_iter().collect(),
                    detail: "a file and a folder whose names differ only in case".into(),
                });
                continue;
            }
            let wg: Vec<Named> = wn.iter().filter(|n| n.key == key).cloned().collect();
            self.pair(policy, &key, &wg, &lg, &target, w, l, plan)?;
        }
        for m in l_members {
            plan.finishes.push(Planned {
                kind: StepKind::Finish,
                vg: format!("{l}/"),
                pid: m.clone(),
                cver: None,
                // Where Bunko's own writes still queued for the loser go instead.
                src: Some(w.clone()),
                dest: l.clone(),
                parent_pid: None,
                is_dir: true,
                hooks: false,
                bytes: None,
                series: None,
            });
        }
        Ok(())
    }

    /// The next free `<stem>.<id>.mokuro` in `target` starting from `base` (`base`
    /// itself first, then `-2`, `-3`, …).
    fn free_layer(plan: &mut Plan, target: &str, stem: &str, base: &str, ext: &str) -> String {
        let taken = plan.taken.entry(target.to_string()).or_default();
        let base = strip_counter(base).to_string();
        let mut n = 1u32;
        loop {
            let id = if n == 1 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            let name = format!("{stem}.{id}{ext}");
            if !taken.contains(&fold(&name)) {
                taken.insert(fold(&name));
                return name;
            }
            n += 1;
        }
    }

    /// Pair one loser volume group with the winner's of the same folded stem
    /// (§5.9 Volume groups).
    #[allow(clippy::too_many_arguments)]
    fn pair(
        &self,
        policy: &dyn CaseMergePolicy,
        key: &str,
        wg: &[Named],
        lg: &[Named],
        target: &str,
        w: &RelPath,
        l: &RelPath,
        plan: &mut Plan,
    ) -> Result<(), Defer> {
        let vg = format!("{l}/{key}");
        let lpath = |n: &Named| l.join(n.name.clone());
        let wpath = |n: &Named| w.join(n.name.clone());
        let block = |plan: &mut Plan, detail: String| {
            plan.blocks.push(Block {
                this: lg.iter().filter_map(lpath).collect(),
                that: wg.iter().filter_map(wpath).collect(),
                detail,
            });
        };
        let series_dir = w.segments().len() == 1;
        let is_series = |n: &Named| {
            series_dir && n.part == Part::Other && n.name.eq_ignore_ascii_case("series.json")
        };
        let wstem = wg
            .iter()
            .find(|n| n.part == Part::Archive)
            .or_else(|| wg.first())
            .map(|n| n.stem.clone());
        let mut lsorted: Vec<&Named> = lg.iter().collect();
        lsorted.sort_by_key(|n| n.part != Part::Archive);
        let (mut layers, mut moves, mut l_trash, mut submits) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut archives_identical = false;
        for lf in lsorted {
            let Some(lrel) = lpath(lf) else { continue };
            if is_series(lf) {
                let Some(series) = w.join("series.json") else {
                    continue;
                };
                submits.push(Planned {
                    kind: StepKind::Submit,
                    vg: vg.clone(),
                    pid: lf.node.pid.clone(),
                    cver: Some(lf.node.cver.clone()),
                    src: None,
                    dest: lrel,
                    parent_pid: None,
                    is_dir: false,
                    hooks: true,
                    bytes: None,
                    series: Some(series),
                });
                continue;
            }
            let counterpart = wg
                .iter()
                .find(|wf| fold(&wf.suffix) == fold(&lf.suffix) && wf.part == lf.part);
            let Some(wf) = counterpart else {
                // Only the loser has it: in it goes, under the winner's stem.
                let name = match &wstem {
                    Some(s) => format!("{s}{}", lf.suffix),
                    None => lf.name.clone(),
                };
                let Some(dest) = w.join(name.clone()) else {
                    continue;
                };
                plan.taken
                    .entry(target.to_string())
                    .or_default()
                    .insert(fold(&name));
                moves.push(Planned::mv(&vg, &lf.node, lrel, dest, target));
                continue;
            };
            let Some(wrel) = wpath(wf) else { continue };
            if self.identical(&wf.node, &lf.node)? {
                if lf.part == Part::Archive {
                    archives_identical = true;
                }
                l_trash.push(Planned::trash(&vg, &lf.node, lrel));
                continue;
            }
            match &lf.part {
                Part::Archive => {
                    block(plan, format!("{lrel} and {wrel} are different archives"));
                    return Ok(());
                }
                Part::Cover => l_trash.push(Planned::trash(&vg, &lf.node, lrel)),
                Part::Main => {
                    if !archives_identical || fold(&lf.suffix).ends_with(".gz") {
                        block(
                            plan,
                            format!("{lrel} and {wrel} hold different OCR for this volume"),
                        );
                        return Ok(());
                    }
                    let choice = policy.prefer_main(&self.core.abs(&wrel), &self.core.abs(&lrel));
                    let (old, old_rel, keep_rel) = match choice {
                        MergeChoice::A => (&lf.node, &lrel, &wrel),
                        MergeChoice::B => (&wf.node, &wrel, &lrel),
                        MergeChoice::Block => {
                            block(
                                plan,
                                format!(
                                    "{lrel} and {wrel} hold different OCR, and neither (or both) \
                                     matches the primary OCR setting"
                                ),
                            );
                            return Ok(());
                        }
                    };
                    // Never lost (owner rule): the superseded main is first kept as
                    // the winner's `pre-upgrade` layer, stamped or not (a merge is not
                    // an upgrade: no "replace without a copy").
                    let old_bytes = self.bytes_of(old)?;
                    let sha = hex::encode(Sha256::digest(&old_bytes));
                    let kept = wg.iter().any(|n| {
                        matches!(&n.part, Part::Layer(id) if strip_counter(&id.to_lowercase()) == "pre-upgrade")
                            && self
                                .bytes_of(&n.node)
                                .ok()
                                .and_then(|b| preserved_sha(&b))
                                .is_some_and(|s| s == sha)
                    });
                    if !kept {
                        let Some(bytes) =
                            preserved_layer(&old_bytes, &format!("case-merge:{keep_rel}"))
                        else {
                            block(plan, format!("{old_rel} is not readable OCR"));
                            return Ok(());
                        };
                        let stem = wstem.clone().unwrap_or_else(|| wf.stem.clone());
                        let name = Self::free_layer(plan, target, &stem, "pre-upgrade", ".mokuro");
                        let Some(dest) = w.join(name) else { continue };
                        layers.push(Planned {
                            kind: StepKind::Layer,
                            vg: vg.clone(),
                            pid: old.pid.clone(),
                            cver: Some(old.cver.clone()),
                            src: Some(old_rel.clone()),
                            dest,
                            parent_pid: Some(target.to_string()),
                            is_dir: false,
                            hooks: false,
                            bytes: Some(bytes),
                            series: None,
                        });
                    }
                    if choice == MergeChoice::B {
                        // The loser's main replaces the winner's in place: a new version
                        // of the winner's node, based on the version judged here. The
                        // winner's stays untouched until that replace has committed; a
                        // 412 or any failure blocks the group instead.
                        let new_bytes = self.bytes_of(&lf.node)?;
                        moves.push(Planned {
                            kind: StepKind::Replace,
                            vg: vg.clone(),
                            pid: wf.node.pid.clone(),
                            cver: Some(wf.node.cver.clone()),
                            src: Some(lrel.clone()),
                            dest: wrel.clone(),
                            parent_pid: Some(target.to_string()),
                            is_dir: false,
                            hooks: true,
                            bytes: Some(new_bytes),
                            series: None,
                        });
                    }
                    l_trash.push(Planned::trash(&vg, &lf.node, lrel));
                }
                Part::Layer(id) if policy.is_unstamped_layer(id) => {
                    let stem = wstem.clone().unwrap_or_else(|| wf.stem.clone());
                    let ext = &lf.suffix[1 + id.len()..];
                    let name = Self::free_layer(plan, target, &stem, id, ext);
                    let Some(dest) = w.join(name) else { continue };
                    moves.push(Planned::mv(&vg, &lf.node, lrel, dest, target));
                }
                Part::Layer(_) | Part::Other => {
                    block(plan, format!("{lrel} and {wrel} differ"));
                    return Ok(());
                }
            }
        }
        plan.layers.extend(layers);
        plan.steps.extend(moves);
        plan.submits.extend(submits);
        plan.l_trash.extend(l_trash);
        Ok(())
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
        })?;
        // Busy from now on, until the group closes.
        self.merge.unmark_probe(scope);
        self.merge.mark(Some(gid.clone()), scope.clone());
        for p in plan.ordered() {
            let kind = match p.kind {
                StepKind::Move => OpKind::Move,
                StepKind::Layer | StepKind::Replace => OpKind::Put,
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
            op.target_pid = Some(p.pid.clone());
            op.parent_pid = p.parent_pid.clone();
            op.group = Some(gid.clone());
            op.extra = serde_json::to_string(&Extra {
                step: p.kind,
                vg: p.vg.clone(),
                is_dir: p.is_dir,
                cver: p.cver.clone(),
                hooks: p.hooks,
                series: p.series.as_ref().map(ToString::to_string),
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
                        base: p.cver.clone().map(bunko_store::Version::new),
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

    fn close_group(&self, gid: &str, policy: &dyn CaseMergePolicy) {
        let _ = self.core.state.with("merge_group_close", |c| {
            c.execute(
                "UPDATE merge_group SET state = 'done', finished_at = ?2 WHERE id = ?1",
                params![gid, now_ms()],
            )
        });
        self.merge.unmark_group(gid);
        if let Some(rec) = self.recent_merges().into_iter().find(|r| r.id == gid) {
            tracing::info!(
                "[STORAGE] Case merge into {} done: {} moved, {} trashed, {} blocked",
                rec.winner,
                rec.moved,
                rec.trashed,
                rec.blocked
            );
            policy.audit(&rec);
        }
    }

    /// A step met a block: record it, and drop the rest of its volume group.
    fn block_vg(&self, gid: &str, op: &Op, detail: String) {
        let vg = extra_of(op).map(|e| e.vg).unwrap_or_default();
        let this: Vec<RelPath> = op.src.clone().into_iter().collect();
        let that = vec![op.dest.clone()];
        self.record_blocks(&[Block {
            this: if this.is_empty() { that.clone() } else { this },
            that,
            detail,
        }]);
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
        // The submission is answered by the server's hooks: before the op lock.
        if ex.step == StepKind::Submit && !self.submitted(&op, &ex) {
            self.core.drop_op(&op, false);
            return Ok(StepEnd::Done);
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
                StepKind::Layer => self.step_layer(gid, &mut op),
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
            return self.moved(gid, op, &src, &pid, n, ex, hooks);
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
        let r = self.block_on_as("case merge", Some(ApiClass::Background), async move {
            backend
                .update(&p2, new_parent.as_deref(), new_name.as_deref(), None)
                .await
        });
        match r {
            Ok(n) => {
                self.core.fail_at("MERGE_MOVED")?;
                self.moved(gid, op, &src, &pid, n, ex, hooks)
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
    /// providers that allow duplicate names) a direct re-list of the winner.
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
    ) -> Result<StepEnd, Crash> {
        let is_dir = n.is_dir;
        let parent = op.parent_pid.clone().unwrap_or_default();
        let name = nfc(&n.name_raw);
        let dest = op.dest.clone();
        self.core.commit_move(op, src, pid, n)?;
        self.group_bump(gid, "moved");
        if ex.hooks {
            hooks.push(HookCall::Merged(
                self.core.abs(src),
                self.core.abs(&dest),
                is_dir,
            ));
        }
        if !self.core.caps.path_ids {
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

    fn step_layer(&self, gid: &str, op: &mut Op) -> Result<StepEnd, Crash> {
        let (Some(parent), Some(staged)) = (op.parent_pid.clone(), self.core.staged_path(op))
        else {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        };
        let name = last_name(&op.dest).to_string();
        let backend = self.core.backend.clone();
        let (p2, n2) = (parent.clone(), name.clone());
        let found = self.block_on_as("case merge", Some(ApiClass::Background), async move {
            backend.find_named(&p2, &n2).await
        });
        let existing = match found {
            Ok((nodes, _)) => nodes,
            Err(e) if retryable(&e) => return Ok(StepEnd::Pause),
            Err(e) => {
                self.block_vg(gid, op, format!("could not keep {}: {e}", op.dest));
                return Ok(StepEnd::Done);
            }
        };
        if !existing.is_empty() {
            if op.state == OpState::Uploading {
                let core = self.core.clone();
                let o = op.clone();
                let ex2 = existing.clone();
                let proven = self.block_on("case merge", async move {
                    for n in &ex2 {
                        if core.content_proof(n, &o).await == Proof::Proven {
                            return Some(n.clone());
                        }
                    }
                    None
                });
                if let Some(n) = proven {
                    self.core.commit_put(op, n, &parent)?;
                    return Ok(StepEnd::Done);
                }
            }
            self.block_vg(gid, op, format!("{} appeared meanwhile", op.dest));
            return Ok(StepEnd::Done);
        }
        op.state = OpState::Uploading;
        let _ = self.core.save_op(op);
        self.core.fail_at("MERGE_LAYER")?;
        let (Some(sha), size) = (op.sha256, op.size) else {
            self.core.drop_op(op, false);
            return Ok(StepEnd::Done);
        };
        let backend = self.core.backend.clone();
        let (p2, n2, s2, oid) = (parent.clone(), name.clone(), staged.clone(), op.op_id());
        let r = self.block_on_as("case merge", Some(ApiClass::Background), async move {
            let mut ctx = OpCtx {
                op_id: oid,
                resume: None,
            };
            backend
                .upload(
                    UploadTarget::New {
                        parent: &p2,
                        name: &n2,
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
                self.core.commit_put(op, n, &parent)?;
                Ok(StepEnd::Done)
            }
            Err(e) if retryable(&e) => {
                op.last_error = Some(e.to_string());
                let _ = self.core.save_op(op);
                Ok(StepEnd::Pause)
            }
            Err(e) => {
                self.block_vg(gid, op, format!("could not keep {}: {e}", op.dest));
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
    /// the winner's series key (§9.3). `true`: applied, so it may be trashed.
    fn submitted(&self, op: &Op, ex: &Extra) -> bool {
        let (Some(pid), Some(cver), Some(series)) =
            (op.target_pid.clone(), ex.cver.clone(), ex.series.clone())
        else {
            return false;
        };
        let Some((id, state, _)) = self.core.state.submission_open(&series, &pid, &cver) else {
            return false;
        };
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
            tracing::info!(%series, state, "case merge: the losing series.json stays (not applied)");
        }
        state == "applied"
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
