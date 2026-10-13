//! The write path (spec §5.7, §5.10; D6, D8, D16, D20, D21, D22).
//!
//! Every write is an outbox op in `state.sqlite`: PREPARED → STAGED → HELD →
//! PREFLIGHT → UPLOADING → COMMITTED → (SUPERSEDE_OLD) → CONFIRMED, or one of the
//! terminal CONFLICT, FAILED, DISCARDED, SUPERSEDED.
//!
//! - **Sync** ops (reader-originated writes, the upgrade swap, moves, deletes,
//!   folders) run inline: the caller blocks through COMMITTED and gets any error.
//!   Nothing is visible to other readers before COMMITTED.
//! - **Deferred** ops (Bunko's own outputs) return after STAGED: the bytes are in
//!   `outbox/<op>` (fsynced), the index shows them at once with `own` (a
//!   `pending_op` marker), and the worker task drains them in `seq` order. A
//!   Deferred op waits (HELD) while the lease gate is shut or its D22 dependencies
//!   are uncommitted. Compiler and status ops coalesce by destination.
//!
//! PREFLIGHT lists the parent for the destination name (never recreating a missing
//! parent) and decides the upload target from the op's mode and base; a conflict
//! goes to the origin's handler (Ocr: discard and `ocr_discarded`; Thumbs: drop
//! ours; Compiler `series.json`: capture a submission and `series_submission`;
//! Compiler/Status elsewhere: overwrite the foreign version; Upgrade, Dav, Admin:
//! CONFLICT, which a Sync caller sees as `bunko_store::is_conflict`). COMMITTED
//! writes the ledger row, then the node into the index (so the feed's echo changes
//! nothing and runs no hooks), fills the blob cache, and emits the event. An upload
//! with an uncertain outcome is reconciled at the next PREFLIGHT: a node at the
//! destination with the staged size (and hash, where the provider reports one) is
//! the op's own commit.
//!
//! Retries back off exponentially with jitter (`Retry-After` honoured); attempts
//! count only while the feed is steady. FAILED is for permanent errors only
//! (invalid name, too large, unsupported, fatal): the op keeps its bytes and records
//! a `failed-op` conflict.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bunko_store::{
    Durability, Mode, Origin, Published, Version, WatchEvent, conflict, unavailable,
};
use rusqlite::params;
use sha2::{Digest, Sha256};

use crate::backend::{BErr, Backend, HashKind, OpCtx, Pid, RNode, UploadSource, UploadTarget};
use crate::cache::{SMALL_MAX, now_ms};
use crate::feed::{EngineState, SubmissionResult};
use crate::index::{IndexChange, ROOT_EID, RelPath};
use crate::names::nfc;
use crate::state::{ConflictRow, LedgerRow};
use crate::status::DoneEntry;
use crate::store::Core;

/// Decides whether Deferred ops may run (the lease fence, or a caller's gate).
pub type Gate = Arc<dyn Fn() -> bool + Send + Sync>;
/// Test-only crash injection: called with a state name after that state was
/// persisted (and with `uploaded` after an upload returned, before COMMITTED is
/// written); `true` stops the store there, as a crash would.
pub type Failpoint = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// How long a Sync move or delete waits for in-flight ops before 503 (§5.7 Barrier).
pub const BARRIER: Duration = Duration::from_secs(30);
/// Overwrite attempts of a Compiler or Status op against foreign versions.
const MAX_OVERWRITES: u32 = 3;
/// Inline attempts of a Sync op.
const SYNC_ATTEMPTS: u32 = 3;

/// What an op does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// Upload staged bytes to `dest`.
    Put,
    /// Create the folder `dest`.
    Mkdir,
    /// Move or rename `src` (pid `target_pid`) to `dest`.
    Move,
    /// Move `dest` (pid `target_pid`) to the provider trash.
    Trash,
    /// Delete `dest` (pid `target_pid`) permanently: Bunko's own versions only (D16).
    Delete,
}

impl OpKind {
    fn as_str(self) -> &'static str {
        match self {
            OpKind::Put => "put",
            OpKind::Mkdir => "mkdir",
            OpKind::Move => "move",
            OpKind::Trash => "trash",
            OpKind::Delete => "delete",
        }
    }

    fn parse(s: &str) -> Option<OpKind> {
        Some(match s {
            "put" => OpKind::Put,
            "mkdir" => OpKind::Mkdir,
            "move" => OpKind::Move,
            "trash" => OpKind::Trash,
            "delete" => OpKind::Delete,
            _ => return None,
        })
    }
}

/// An op's state (§5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpState {
    Prepared,
    Staged,
    Held,
    Preflight,
    Uploading,
    Committed,
    SupersedeOld,
    Confirmed,
    Conflict,
    Failed,
    Discarded,
    Superseded,
}

impl OpState {
    pub fn as_str(self) -> &'static str {
        match self {
            OpState::Prepared => "PREPARED",
            OpState::Staged => "STAGED",
            OpState::Held => "HELD",
            OpState::Preflight => "PREFLIGHT",
            OpState::Uploading => "UPLOADING",
            OpState::Committed => "COMMITTED",
            OpState::SupersedeOld => "SUPERSEDE_OLD",
            OpState::Confirmed => "CONFIRMED",
            OpState::Conflict => "CONFLICT",
            OpState::Failed => "FAILED",
            OpState::Discarded => "DISCARDED",
            OpState::Superseded => "SUPERSEDED",
        }
    }

    fn parse(s: &str) -> Option<OpState> {
        Some(match s {
            "PREPARED" => OpState::Prepared,
            "STAGED" => OpState::Staged,
            "HELD" => OpState::Held,
            "PREFLIGHT" => OpState::Preflight,
            "UPLOADING" => OpState::Uploading,
            "COMMITTED" => OpState::Committed,
            "SUPERSEDE_OLD" => OpState::SupersedeOld,
            "CONFIRMED" => OpState::Confirmed,
            "CONFLICT" => OpState::Conflict,
            "FAILED" => OpState::Failed,
            "DISCARDED" => OpState::Discarded,
            "SUPERSEDED" => OpState::Superseded,
            _ => return None,
        })
    }

    /// Not yet committed and not given up: the op still owes its write.
    pub fn live(self) -> bool {
        matches!(
            self,
            OpState::Staged | OpState::Held | OpState::Preflight | OpState::Uploading
        )
    }
}

fn origin_str(o: &Origin) -> String {
    match o {
        Origin::Compiler => "compiler".into(),
        Origin::Ocr { claim } => format!("ocr:{claim}"),
        Origin::Upgrade => "upgrade".into(),
        Origin::Thumbs => "thumbs".into(),
        Origin::Status => "status".into(),
        Origin::Dav { user: Some(u) } => format!("dav:{u}"),
        Origin::Dav { user: None } => "dav".into(),
        Origin::Admin => "admin".into(),
    }
}

fn origin_parse(s: &str) -> Origin {
    match s {
        "compiler" => Origin::Compiler,
        "upgrade" => Origin::Upgrade,
        "thumbs" => Origin::Thumbs,
        "status" => Origin::Status,
        "dav" => Origin::Dav { user: None },
        "admin" => Origin::Admin,
        s if s.starts_with("ocr:") => Origin::Ocr {
            claim: s[4..].to_string(),
        },
        s if s.starts_with("dav:") => Origin::Dav {
            user: Some(s[4..].to_string()),
        },
        _ => Origin::Admin,
    }
}

/// One outbox op.
#[derive(Debug, Clone)]
pub(crate) struct Op {
    pub id: u64,
    pub kind: OpKind,
    pub dest: RelPath,
    pub src: Option<RelPath>,
    pub mode: Mode,
    pub origin: Origin,
    pub durability: Durability,
    pub target_pid: Option<Pid>,
    pub parent_pid: Option<Pid>,
    /// The file name under `outbox/`.
    pub staged: Option<String>,
    pub sha256: Option<[u8; 32]>,
    pub size: u64,
    pub state: OpState,
    pub attempts: u32,
    pub next_at: i64,
    pub last_error: Option<String>,
    pub result_pid: Option<Pid>,
    pub result_cver: Option<String>,
    pub superseded_pid: Option<Pid>,
    pub created_at: i64,
    /// Not persisted: a Sync op found UPLOADING at startup is only looked up.
    pub reconcile_only: bool,
    /// Not persisted: overwrites of foreign versions so far.
    pub overwrites: u32,
}

impl Op {
    pub(crate) fn new(
        id: u64,
        kind: OpKind,
        dest: RelPath,
        origin: Origin,
        durability: Durability,
    ) -> Op {
        Op {
            id,
            kind,
            dest,
            src: None,
            mode: Mode::Replace { base: None },
            origin,
            durability,
            target_pid: None,
            parent_pid: None,
            staged: None,
            sha256: None,
            size: 0,
            state: OpState::Prepared,
            attempts: 0,
            next_at: 0,
            last_error: None,
            result_pid: None,
            result_cver: None,
            superseded_pid: None,
            created_at: now_ms(),
            reconcile_only: false,
            overwrites: 0,
        }
    }

    pub(crate) fn op_id(&self) -> String {
        self.id.to_string()
    }

    fn base(&self) -> Option<&str> {
        match &self.mode {
            Mode::Replace { base: Some(b) } => Some(b.as_str()),
            _ => None,
        }
    }

    fn coalesces(&self) -> bool {
        self.kind == OpKind::Put
            && self.durability == Durability::Deferred
            && matches!(self.origin, Origin::Compiler | Origin::Status)
    }
}

/// What an admin (or a test) sees of one op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpSummary {
    pub id: u64,
    pub kind: &'static str,
    pub dest: String,
    pub origin: String,
    pub deferred: bool,
    pub state: &'static str,
    pub attempts: u32,
    pub last_error: Option<String>,
}

impl From<&Op> for OpSummary {
    fn from(op: &Op) -> OpSummary {
        OpSummary {
            id: op.id,
            kind: op.kind.as_str(),
            dest: op.dest.to_string(),
            origin: origin_str(&op.origin),
            deferred: op.durability == Durability::Deferred,
            state: op.state.as_str(),
            attempts: op.attempts,
            last_error: op.last_error.clone(),
        }
    }
}

/// The store stopped at a failpoint (a simulated crash).
#[derive(Debug)]
pub(crate) struct Crash;

impl From<Crash> for io::Error {
    fn from(_: Crash) -> io::Error {
        io::Error::other("the store stopped at a failpoint")
    }
}

/// How a run of an op ended.
pub(crate) enum Step {
    /// Committed (or finished another way); the published result.
    Done(Published),
    /// Waiting for the gate or a dependency.
    Blocked,
    /// A transient failure; try again (after the provider's delay, if it gave one).
    Retry(String, Option<Duration>),
    /// Given up: the error a Sync caller sees.
    Gave(io::Error),
}

enum Class {
    Conflict,
    Gone,
    Retry(Option<Duration>),
    Permanent,
}

fn classify(e: &BErr) -> Class {
    match e {
        BErr::PreconditionFailed | BErr::NameConflict => Class::Conflict,
        BErr::NotFound => Class::Gone,
        BErr::Throttled(d) | BErr::TransferQuota(d) => Class::Retry(*d),
        BErr::Transient(_)
        | BErr::AuthExpired
        | BErr::AuthRevoked
        | BErr::StorageFull
        | BErr::CursorLost => Class::Retry(None),
        BErr::InvalidName | BErr::TooLarge | BErr::Unsupported | BErr::Fatal(_) => Class::Permanent,
    }
}

fn parent_of(rel: &RelPath) -> RelPath {
    let s = rel.segments();
    RelPath::from_segments(s[..s.len().saturating_sub(1)].iter().cloned()).unwrap_or_default()
}

fn last_name(rel: &RelPath) -> &str {
    rel.segments().last().map_or("", String::as_str)
}

fn under(rel: &RelPath, dir: &RelPath) -> bool {
    rel.segments().starts_with(dir.segments())
}

fn rebase_rel(rel: &RelPath, from: &RelPath, to: &RelPath) -> Option<RelPath> {
    if !under(rel, from) {
        return None;
    }
    RelPath::from_segments(
        to.segments()
            .iter()
            .chain(&rel.segments()[from.segments().len()..])
            .cloned(),
    )
}

fn rebase_pid(pid: &str, old: &str, new: &str) -> Option<String> {
    if pid == old {
        Some(new.to_string())
    } else {
        pid.strip_prefix(old)
            .filter(|rest| rest.starts_with('/'))
            .map(|rest| format!("{new}{rest}"))
    }
}

// ---- persistence ------------------------------------------------------------------

impl<B: Backend> Core<B> {
    pub(crate) fn outbox_dir(&self) -> PathBuf {
        self.dir.join("outbox")
    }

    pub(crate) fn staged_path(&self, op: &Op) -> Option<PathBuf> {
        op.staged.as_ref().map(|s| self.outbox_dir().join(s))
    }

    /// Persist `op` (and mirror it in memory).
    pub(crate) fn save_op(&self, op: &Op) -> io::Result<()> {
        let (mode, base) = match &op.mode {
            Mode::CreateNew => ("create", None),
            Mode::Replace { base } => ("replace", base.as_ref().map(|b| b.as_str().to_string())),
        };
        self.state.with("save_op", |c| {
            c.execute(
                "INSERT OR REPLACE INTO outbox(op_id, seq, kind, dest, src, mode, origin, durability, base_cver, \
                 target_pid, parent_pid, name, staged, sha256, size, state, attempts, next_at, last_error, \
                 result_pid, result_cver, superseded_pid, created_at) \
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
                params![
                    op.op_id(),
                    op.id as i64,
                    op.kind.as_str(),
                    op.dest.to_string(),
                    op.src.as_ref().map(ToString::to_string),
                    mode,
                    origin_str(&op.origin),
                    if op.durability == Durability::Sync { "sync" } else { "deferred" },
                    base,
                    op.target_pid,
                    op.parent_pid,
                    last_name(&op.dest),
                    op.staged,
                    op.sha256.map(hex::encode),
                    op.size as i64,
                    op.state.as_str(),
                    op.attempts,
                    op.next_at,
                    op.last_error,
                    op.result_pid,
                    op.result_cver,
                    op.superseded_pid,
                    op.created_at,
                ],
            )
            .map(|_| ())
        })?;
        self.ops.lock().insert(op.id, op.clone());
        Ok(())
    }

    /// Forget a finished op: its row, its bytes (unless `keep_bytes`).
    pub(crate) fn drop_op(&self, op: &Op, keep_bytes: bool) {
        if !keep_bytes && let Some(p) = self.staged_path(op) {
            let _ = std::fs::remove_file(p);
        }
        let _ = self.state.with("drop_op", |c| {
            c.execute("DELETE FROM outbox WHERE op_id = ?1", [op.op_id()])
        });
        self.ops.lock().remove(&op.id);
    }

    /// Every persisted op, by `seq`.
    pub(crate) fn load_ops(&self) -> Vec<Op> {
        self.state
            .with("load_ops", |c| {
                let mut s = c.prepare(
                    "SELECT seq, kind, dest, src, mode, origin, durability, base_cver, target_pid, parent_pid, \
                     staged, sha256, size, state, attempts, next_at, last_error, result_pid, result_cver, \
                     superseded_pid, created_at FROM outbox ORDER BY seq",
                )?;
                let rows = s.query_map([], |r| {
                    let kind: String = r.get(1)?;
                    let dest: String = r.get(2)?;
                    let src: Option<String> = r.get(3)?;
                    let mode: String = r.get(4)?;
                    let origin: String = r.get(5)?;
                    let dur: String = r.get(6)?;
                    let base: Option<String> = r.get(7)?;
                    let sha: Option<String> = r.get(11)?;
                    let state: String = r.get(13)?;
                    Ok((
                        r.get::<_, i64>(0)?,
                        kind,
                        dest,
                        src,
                        mode,
                        origin,
                        dur,
                        base,
                        (r.get::<_, Option<String>>(8)?, r.get::<_, Option<String>>(9)?),
                        r.get::<_, Option<String>>(10)?,
                        sha,
                        r.get::<_, i64>(12)?,
                        state,
                        (r.get::<_, u32>(14)?, r.get::<_, i64>(15)?, r.get::<_, Option<String>>(16)?),
                        (
                            r.get::<_, Option<String>>(17)?,
                            r.get::<_, Option<String>>(18)?,
                            r.get::<_, Option<String>>(19)?,
                        ),
                        r.get::<_, i64>(20)?,
                    ))
                })?;
                let mut out = Vec::new();
                for row in rows {
                    let (seq, kind, dest, src, mode, origin, dur, base, (target, parent), staged, sha, size, state, (attempts, next_at, last_error), (rp, rc, sp), created) = row?;
                    let (Some(kind), Some(dest), Some(state)) =
                        (OpKind::parse(&kind), RelPath::parse(&dest), OpState::parse(&state))
                    else {
                        tracing::warn!(seq, "outbox: unreadable op row skipped");
                        continue;
                    };
                    out.push(Op {
                        id: seq as u64,
                        kind,
                        dest,
                        src: src.as_deref().and_then(RelPath::parse),
                        mode: if mode == "create" {
                            Mode::CreateNew
                        } else {
                            Mode::Replace {
                                base: base.map(Version::new),
                            }
                        },
                        origin: origin_parse(&origin),
                        durability: if dur == "sync" {
                            Durability::Sync
                        } else {
                            Durability::Deferred
                        },
                        target_pid: target,
                        parent_pid: parent,
                        staged,
                        sha256: sha
                            .and_then(|h| hex::decode(h).ok())
                            .and_then(|b| <[u8; 32]>::try_from(b).ok()),
                        size: size as u64,
                        state,
                        attempts,
                        next_at,
                        last_error,
                        result_pid: rp,
                        result_cver: rc,
                        superseded_pid: sp,
                        created_at: created,
                        reconcile_only: false,
                        overwrites: 0,
                    });
                }
                Ok(out)
            })
            .unwrap_or_default()
    }

    /// The highest `seq` ever used.
    pub(crate) fn max_seq(&self) -> u64 {
        self.state
            .with("max_seq", |c| {
                c.query_row("SELECT COALESCE(MAX(seq), 0) FROM outbox", [], |r| {
                    r.get::<_, i64>(0)
                })
            })
            .unwrap_or(0)
            .max(
                self.state
                    .meta_get("outbox_seq")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0),
            ) as u64
    }

    pub(crate) fn next_seq(&self) -> u64 {
        let id = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = self.state.meta_set("outbox_seq", &id.to_string());
        id
    }

    // ---- gate, failpoints, events -----------------------------------------------------

    /// Writes may reach the provider: the lease fence is open (when the lease runs).
    pub(crate) fn fence_open(&self) -> bool {
        !self.crashed.load(Ordering::Acquire)
            && (!self.lease_on || self.fence.get().is_some_and(|f| f.ok()))
    }

    /// Deferred ops may run: the fence is open and the caller's gate agrees.
    pub(crate) fn gate_open(&self) -> bool {
        let custom = self.gate.lock().clone();
        self.fence_open() && custom.is_none_or(|g| g())
    }

    pub(crate) fn fail_at(&self, point: &str) -> Result<(), Crash> {
        if self.crashed.load(Ordering::Acquire) {
            return Err(Crash);
        }
        let fp = self.failpoint.lock().clone();
        if fp.is_some_and(|f| f(point)) {
            tracing::warn!(point, "failpoint: the store stops here");
            self.crashed.store(true, Ordering::Release);
            return Err(Crash);
        }
        Ok(())
    }

    pub(crate) fn abs(&self, rel: &RelPath) -> PathBuf {
        let mut p = self.root.clone();
        for s in rel.segments() {
            p.push(s);
        }
        p
    }

    pub(crate) fn emit_changes(&self, changes: &[IndexChange]) {
        for c in changes {
            match c {
                IndexChange::Added { rel, is_dir } => (self.emit)(WatchEvent::Added {
                    path: self.abs(rel),
                    is_dir: *is_dir,
                }),
                IndexChange::Removed { rel, is_dir } => (self.emit)(WatchEvent::Removed {
                    path: self.abs(rel),
                    is_dir: *is_dir,
                }),
                IndexChange::Moved { from, to, is_dir } => {
                    (self.emit)(WatchEvent::Removed {
                        path: self.abs(from),
                        is_dir: *is_dir,
                    });
                    (self.emit)(WatchEvent::Added {
                        path: self.abs(to),
                        is_dir: *is_dir,
                    });
                }
                IndexChange::ContentChanged { rel } => (self.emit)(WatchEvent::Added {
                    path: self.abs(rel),
                    is_dir: false,
                }),
            }
        }
    }

    /// The feed is steady (retries count only then, §5.7 Retries).
    fn online(&self) -> bool {
        self.feed.state() == EngineState::Steady
    }

    /// The parent folder's pid of `dest`, from the index.
    fn parent_pid_of(&self, dest: &RelPath) -> Option<Pid> {
        let idx = self.index.lock();
        let p = idx.lookup(&parent_of(dest)).filter(|e| e.is_dir)?;
        if p.eid == ROOT_EID {
            return idx.winner_node(ROOT_EID).map(|n| n.pid);
        }
        p.winner_pid
    }

    /// Where a Deferred op's destination is now (its marker moves with its folder).
    fn current_dest(&self, op: &Op) -> RelPath {
        if op.durability == Durability::Deferred
            && let Some(p) = self.index.lock().pending_path(&op.op_id())
        {
            return p;
        }
        op.dest.clone()
    }

    // ---- D22 ------------------------------------------------------------------------

    /// Every op this one must wait for has committed (D22): a Compiler `series.json`
    /// waits for the non-Compiler Deferred ops in its series folder, `catalog.json`
    /// for every `series.json`, the queue file for earlier OCR results.
    fn deps_done(&self, op: &Op, ops: &BTreeMap<u64, Op>) -> bool {
        let others = || {
            ops.values().filter(move |o| {
                o.id != op.id && o.durability == Durability::Deferred && o.state.live()
            })
        };
        let name = last_name(&op.dest);
        match (&op.origin, name) {
            (Origin::Compiler, "series.json") => {
                let series = parent_of(&op.dest);
                !others().any(|o| o.origin != Origin::Compiler && under(&o.dest, &series))
            }
            (Origin::Compiler, "catalog.json") if op.dest.segments().len() == 1 => {
                !others().any(|o| last_name(&o.dest) == "series.json")
            }
            (Origin::Status, ".mokuro-queue.json") => {
                !others().any(|o| o.id < op.id && matches!(o.origin, Origin::Ocr { .. }))
            }
            _ => true,
        }
    }

    // ---- staging a Deferred op ---------------------------------------------------------

    /// After STAGED: coalesce older Compiler/Status ops for the same destination,
    /// mark the index (read-after-write) and emit the event (D21).
    pub(crate) fn stage_deferred(&self, mut op: Op) -> io::Result<Op> {
        if op.coalesces() {
            let older: Vec<Op> = self
                .ops
                .lock()
                .values()
                .filter(|o| {
                    o.id != op.id
                        && o.coalesces()
                        && o.dest == op.dest
                        && matches!(o.state, OpState::Staged | OpState::Held)
                })
                .cloned()
                .collect();
            for mut old in older {
                // A base on the replaced op means the base that op had.
                if op.base() == Some(&format!("pending:{}", old.id)) {
                    op.mode = old.mode.clone();
                }
                old.state = OpState::Superseded;
                self.drop_op(&old, false);
            }
            self.save_op(&op)?;
        }
        let change = self
            .index
            .lock()
            .set_pending(&op.dest, &op.op_id())
            .map_err(io::Error::other)?;
        if let Some(c) = change {
            self.emit_changes(&[c]);
        }
        self.wake.notify_one();
        Ok(op)
    }

    // ---- running ops ------------------------------------------------------------------

    /// Run a Sync op through COMMITTED (or its error), inline.
    pub(crate) async fn run_sync(self: &Arc<Self>, mut op: Op) -> io::Result<Published> {
        let barrier = matches!(op.kind, OpKind::Move | OpKind::Trash | OpKind::Delete);
        let _g = if barrier {
            match tokio::time::timeout(BARRIER, self.exec.lock()).await {
                Ok(g) => g,
                Err(_) => {
                    self.finish(&mut op, OpState::Discarded);
                    return Err(unavailable(
                        "pending writes under the path did not finish in time",
                    ));
                }
            }
        } else {
            self.exec.lock().await
        };
        if !self.fence_open() {
            self.finish(&mut op, OpState::Discarded);
            return Err(unavailable(
                "this Bunko does not hold the library lease (storage-lease)",
            ));
        }
        let mut last = String::new();
        for attempt in 0..SYNC_ATTEMPTS {
            match self.drive(&mut op).await? {
                Step::Done(p) => return Ok(p),
                Step::Gave(e) => return Err(e),
                Step::Blocked => break,
                Step::Retry(msg, after) => {
                    last = msg;
                    let d = after
                        .unwrap_or(self.retry_base * 2u32.pow(attempt))
                        .min(Duration::from_secs(5));
                    tokio::time::sleep(d).await;
                }
            }
        }
        self.finish(&mut op, OpState::Discarded);
        Err(unavailable(format!(
            "the provider did not take the write: {last}"
        )))
    }

    /// One run of `op` from wherever it stands.
    pub(crate) async fn drive(self: &Arc<Self>, op: &mut Op) -> Result<Step, Crash> {
        loop {
            let step = match op.kind {
                OpKind::Put => self.drive_put(op).await?,
                OpKind::Mkdir => self.drive_mkdir(op).await?,
                OpKind::Move => self.drive_move(op).await?,
                OpKind::Trash | OpKind::Delete => self.drive_trash(op).await?,
            };
            // A handler may ask for one more round (an overwrite of a foreign version).
            if let Some(s) = step {
                return Ok(s);
            }
        }
    }

    /// End an op that will not commit.
    pub(crate) fn finish(&self, op: &mut Op, state: OpState) {
        op.state = state;
        if op.durability == Durability::Deferred
            && let Ok(ch) = self.index.lock().clear_pending(&op.op_id())
        {
            self.emit_changes(&ch);
        }
        match state {
            OpState::Failed | OpState::Conflict => {
                let _ = self.save_op(op);
                if state == OpState::Conflict {
                    // The bytes of a conflicting op are of no use any more.
                    if let Some(p) = self.staged_path(op) {
                        let _ = std::fs::remove_file(p);
                    }
                }
            }
            _ => self.drop_op(op, false),
        }
    }

    fn retry(&self, op: &mut Op, msg: String, after: Option<Duration>) -> Step {
        if self.online() {
            op.attempts += 1;
        }
        let exp = self
            .retry_base
            .saturating_mul(2u32.saturating_pow(op.attempts.min(10)))
            .min(Duration::from_secs(300));
        let jitter = Duration::from_millis(
            crate::names::version_salt(&op.op_id(), &op.attempts.to_string()) % 250,
        );
        let d = after.unwrap_or(exp) + jitter;
        op.next_at = now_ms() + d.as_millis() as i64;
        op.last_error = Some(msg.clone());
        let _ = self.save_op(op);
        Step::Retry(msg, after)
    }

    fn give_up(&self, op: &mut Op, msg: String) -> Step {
        tracing::warn!(op = op.id, dest = %op.dest, "outbox op failed: {msg}");
        op.last_error = Some(msg.clone());
        self.state.conflict_add(
            ConflictRow::FailedOp,
            &op.dest.to_string(),
            op.target_pid.as_deref(),
            &msg,
        );
        self.finish(op, OpState::Failed);
        Step::Gave(io::Error::other(msg))
    }

    /// Handle a backend error of an upload or call (`None`: try another round).
    async fn on_error(
        self: &Arc<Self>,
        op: &mut Op,
        e: BErr,
        current: Option<RNode>,
    ) -> Option<Step> {
        match classify(&e) {
            Class::Retry(after) => Some(self.retry(op, e.to_string(), after)),
            Class::Permanent => Some(self.give_up(op, e.to_string())),
            Class::Conflict => self.on_conflict(op, e.to_string(), current).await,
            Class::Gone => self.on_conflict(op, format!("gone: {e}"), None).await,
        }
    }

    /// The per-origin conflict handlers (§5.7). `None`: run the op again.
    async fn on_conflict(
        self: &Arc<Self>,
        op: &mut Op,
        why: String,
        current: Option<RNode>,
    ) -> Option<Step> {
        let path = self.abs(&op.dest);
        let dest = op.dest.to_string();
        tracing::info!(op = op.id, %dest, "outbox conflict: {why}");
        match (&op.origin, &current) {
            (Origin::Ocr { .. }, Some(_)) if op.kind == OpKind::Put => {
                self.state.conflict_add(
                    ConflictRow::Discarded,
                    &dest,
                    current.as_ref().map(|n| n.pid.as_str()),
                    "discarded: a sidecar appeared in the cloud",
                );
                self.hooks.ocr_discarded(&path);
                self.finish(op, OpState::Conflict);
                Some(Step::Done(Published::default()))
            }
            (Origin::Thumbs, _) if op.kind == OpKind::Put => {
                // Keep the reader's cover.
                self.finish(op, OpState::Conflict);
                Some(Step::Done(Published::default()))
            }
            (Origin::Compiler, Some(n))
                if op.kind == OpKind::Put && last_name(&op.dest) == "series.json" =>
            {
                self.capture_submission(&op.dest, n).await;
                self.finish(op, OpState::Conflict);
                Some(Step::Gave(conflict(format!(
                    "{dest}: a foreign version was submitted ({why})"
                ))))
            }
            (Origin::Compiler | Origin::Status, Some(n))
                if op.kind == OpKind::Put && op.overwrites < MAX_OVERWRITES =>
            {
                // Derived or Bunko-owned: overwrite the version that is there now.
                op.overwrites += 1;
                op.mode = Mode::Replace {
                    base: Some(Version::new(n.cver.clone())),
                };
                None
            }
            _ => {
                if op.durability == Durability::Deferred {
                    self.state.conflict_add(
                        ConflictRow::LostWrite,
                        &dest,
                        current.as_ref().map(|n| n.pid.as_str()),
                        &why,
                    );
                }
                self.finish(op, OpState::Conflict);
                Some(Step::Gave(conflict(format!("{dest}: {why}"))))
            }
        }
    }

    /// A foreign `series.json` version: keep its bytes under `submissions/`, record
    /// it, and hand it to the server's merge (D8).
    async fn capture_submission(&self, dest: &RelPath, n: &RNode) {
        let pinned = self.caps.pinned_read && n.pin.is_some();
        let bytes = match self
            .backend
            .read(&n.pid, if pinned { n.pin.as_deref() } else { None }, None)
            .await
        {
            Ok(body) => crate::backend::read_to_vec_max(body, SMALL_MAX).await.ok(),
            Err(_) => None,
        };
        let Some(bytes) = bytes else {
            tracing::warn!(%dest, "series.json submission: the foreign version could not be read");
            return;
        };
        let sha = hex::encode(Sha256::digest(&bytes));
        let Some(id) = self
            .state
            .submission_add(&dest.to_string(), &n.pid, &n.cver, &sha)
        else {
            return;
        };
        let dir = self.dir.join("submissions");
        if std::fs::create_dir_all(&dir)
            .and_then(|_| std::fs::write(dir.join(format!("{id}.json")), &bytes))
            .is_err()
        {
            tracing::warn!(%dest, "series.json submission: could not keep its bytes");
        }
        let series = self.abs(&parent_of(dest));
        match self.hooks.series_submission(&series, &bytes) {
            SubmissionResult::Applied => self.state.submission_set(id, "applied"),
            SubmissionResult::Refused(r) => {
                self.state.submission_set(id, "refused");
                self.state.conflict_add(
                    ConflictRow::LostWrite,
                    &dest.to_string(),
                    Some(&n.pid),
                    &r,
                );
            }
        }
    }

    // ---- Put ----------------------------------------------------------------------

    fn same_bytes(&self, n: &RNode, op: &Op) -> bool {
        if n.is_dir || n.size != op.size {
            return false;
        }
        match (&n.hash, op.sha256) {
            (Some((HashKind::Sha256, h)), Some(sha)) => h.eq_ignore_ascii_case(&hex::encode(sha)),
            (Some((HashKind::Md5, h)), _) => self
                .staged_path(op)
                .and_then(|p| std::fs::read(p).ok())
                .is_some_and(|d| {
                    hex::encode(<md5::Md5 as md5::Digest>::digest(&d)).eq_ignore_ascii_case(h)
                }),
            // No hash to compare: the size, and that it is not the version replaced.
            _ => op.base() != Some(n.cver.as_str()),
        }
    }

    /// `Ok(None)`: ask again (an overwrite); otherwise how the run ended.
    async fn drive_put(self: &Arc<Self>, op: &mut Op) -> Result<Option<Step>, Crash> {
        let deferred = op.durability == Durability::Deferred;
        if deferred && !op.reconcile_only {
            let ready = self.gate_open() && self.deps_done(op, &self.ops.lock());
            if !ready {
                return Ok(Some(Step::Blocked));
            }
        }
        let uncertain = op.state == OpState::Uploading;
        if !uncertain {
            op.state = OpState::Preflight;
            let _ = self.save_op(op);
            self.fail_at("PREFLIGHT")?;
        }
        let dest = self.current_dest(op);
        let name = last_name(&dest).to_string();
        let Some(parent) = self.parent_pid_of(&dest) else {
            return Ok(self
                .on_conflict(op, "the parent folder is gone".into(), None)
                .await);
        };
        let existing = match self.backend.find_named(&parent, &name).await {
            Ok((nodes, _)) => nodes,
            Err(BErr::NotFound) => {
                return Ok(self
                    .on_conflict(op, "the parent folder is gone".into(), None)
                    .await);
            }
            Err(e) => return Ok(self.on_error(op, e, None).await),
        };
        if existing.iter().any(|n| n.is_dir) {
            return Ok(self
                .on_conflict(op, "a folder holds the name".into(), None)
                .await);
        }
        let indexed = self.index.lock().lookup(&dest).and_then(|e| e.winner_pid);
        let current = existing
            .iter()
            .find(|n| Some(&n.pid) == indexed.as_ref())
            .or_else(|| {
                existing
                    .iter()
                    .max_by_key(|n| (n.created_ms, n.pid.clone()))
            })
            .cloned();
        if uncertain && let Some(n) = existing.iter().find(|n| self.same_bytes(n, op)) {
            let n = n.clone();
            return self.commit_put(op, n, &parent).map(Some);
        }
        if op.reconcile_only {
            self.finish(op, OpState::Discarded);
            return Ok(Some(Step::Gave(io::Error::other(
                "the interrupted write did not land",
            ))));
        }
        let target = match (&current, &op.mode) {
            (None, Mode::CreateNew | Mode::Replace { base: None }) => None,
            (None, Mode::Replace { base: Some(_) }) => {
                if matches!(op.origin, Origin::Compiler | Origin::Status) {
                    None
                } else {
                    return Ok(self.on_conflict(op, "the file is gone".into(), None).await);
                }
            }
            (Some(n), Mode::CreateNew) => {
                let n = n.clone();
                return Ok(self
                    .on_conflict(op, "the name exists".into(), Some(n))
                    .await);
            }
            (Some(n), Mode::Replace { base }) => {
                let ok = match base.as_ref().map(Version::as_str) {
                    None => true,
                    Some(b) if b == n.cver => true,
                    Some(b) => {
                        let own_base = b.starts_with("pending:") || self.state.is_own(&n.pid, b);
                        own_base && self.state.is_own(&n.pid, &n.cver)
                    }
                };
                if !ok {
                    let n = n.clone();
                    return Ok(self
                        .on_conflict(op, "the file changed since its base".into(), Some(n))
                        .await);
                }
                Some(n.clone())
            }
        };
        op.state = OpState::Uploading;
        op.parent_pid = Some(parent.clone());
        op.target_pid = target.as_ref().map(|n| n.pid.clone());
        let _ = self.save_op(op);
        self.fail_at("UPLOADING")?;
        let Some(staged) = self.staged_path(op) else {
            return Ok(Some(self.give_up(op, "the op has no staged bytes".into())));
        };
        let sha = op.sha256.unwrap_or_default();
        let if_cver = target
            .as_ref()
            .filter(|_| self.caps.cas_overwrite)
            .map(|n| n.cver.clone());
        let to = match &target {
            None => UploadTarget::New {
                parent: &parent,
                name: &name,
            },
            Some(n) => UploadTarget::Replace {
                pid: &n.pid,
                if_cver: if_cver.as_deref(),
            },
        };
        let mut ctx = OpCtx {
            op_id: op.op_id(),
            resume: None,
        };
        let r = self
            .backend
            .upload(
                to,
                UploadSource::File {
                    path: &staged,
                    size: op.size,
                    sha256: sha,
                },
                &mut ctx,
            )
            .await;
        match r {
            Ok(node) => {
                self.fail_at("uploaded")?;
                let step = self.commit_put(op, node.clone(), &parent)?;
                if let Some(old) = &target
                    && old.pid != node.pid
                    && !self.caps.atomic_replace
                {
                    self.supersede(op, old).await;
                }
                Ok(Some(step))
            }
            Err(e) => Ok(self.on_error(op, e, current).await),
        }
    }

    /// COMMITTED (§5.7 step 6): ledger, index, caches, event.
    fn commit_put(&self, op: &mut Op, node: RNode, parent: &str) -> Result<Step, Crash> {
        let name = nfc(&node.name_raw);
        let sha = op.sha256.map(hex::encode);
        let _ = self.state.ledger_put(&LedgerRow {
            pid: node.pid.clone(),
            cver: node.cver.clone(),
            parent_pid: node.parent.clone().or_else(|| Some(parent.to_string())),
            name,
            sha256: sha,
            op_id: Some(op.op_id()),
            trashed: false,
            echo_only: false,
        });
        op.state = OpState::Committed;
        op.result_pid = Some(node.pid.clone());
        op.result_cver = Some(node.cver.clone());
        let _ = self.save_op(op);
        let rel = {
            let idx = self.index.lock();
            let r = idx.begin().and_then(|tx| {
                let ch =
                    idx.upsert(&tx, &node, idx.generation(), self.caps.mtime_granularity_ns)?;
                tx.commit()?;
                Ok(ch)
            });
            if let Err(e) = &r {
                tracing::warn!("outbox commit: index: {e}");
            }
            let _ = idx.clear_pending(&op.op_id());
            idx.entry_by_pid(&node.pid)
                .and_then(|e| idx.path_of(e.eid))
                .unwrap_or_else(|| op.dest.clone())
        };
        if node.size <= SMALL_MAX
            && let (Some(p), Some(sha)) = (self.staged_path(op), op.sha256)
            && let Ok(data) = std::fs::read(&p)
            && let Err(e) = self.blobs.insert(&node.pid, &node.cver, &data, sha)
        {
            tracing::warn!("outbox commit: blob cache: {e}");
        }
        (self.emit)(WatchEvent::Added {
            path: self.abs(&rel),
            is_dir: false,
        });
        if matches!(op.origin, Origin::Ocr { .. }) {
            self.record_done(&rel);
        }
        self.fail_at("COMMITTED")?;
        Ok(Step::Done(Published {
            version: Some(Version::new(node.cver)),
            sha256: op.sha256,
            op: (op.durability == Durability::Deferred).then_some(bunko_store::OpId(op.id)),
        }))
    }

    /// SUPERSEDE_OLD (MEGA-style replaces): the previous node goes away — trashed
    /// for an upgrade swap or a version Bunko did not write, else deleted.
    async fn supersede(&self, op: &mut Op, old: &RNode) {
        op.state = OpState::SupersedeOld;
        op.superseded_pid = Some(old.pid.clone());
        let _ = self.save_op(op);
        if let Ok(Some(n)) = self.backend.stat(&old.pid).await
            && !n.trashed
        {
            let own = self.state.is_own(&old.pid, &old.cver);
            let r = if own && op.origin != Origin::Upgrade {
                self.backend.delete_permanent(&old.pid).await
            } else {
                self.backend.trash(&old.pid, None).await
            };
            if r.is_ok() {
                let _ = self.state.ledger_trash(&old.pid);
            }
        }
        op.state = OpState::Committed;
        let _ = self.save_op(op);
    }

    /// `done[]` of the cloud queue file: a committed OCR sidecar.
    fn record_done(&self, rel: &RelPath) {
        let segs = rel.segments();
        let Some(file) = segs.last() else { return };
        let stem = file
            .strip_suffix(".mokuro.gz")
            .or_else(|| file.strip_suffix(".mokuro"));
        let Some(stem) = stem else { return };
        let series = segs[..segs.len() - 1].join("/");
        // `<Vol>.<layer>.mokuro` when `<Vol>` is a sibling archive's name.
        let (volume, layer) = match stem.rsplit_once('.') {
            Some((vol, layer)) if self.has_archive(rel, vol) => {
                (vol.to_string(), Some(layer.to_string()))
            }
            _ => (stem.to_string(), None),
        };
        let path = rel.to_string();
        let mut done = self.done.lock();
        let at_ms = now_ms();
        let i = match done
            .iter()
            .position(|d| d.series == series && d.volume == volume)
        {
            Some(i) => i,
            None => {
                done.push(DoneEntry {
                    series: series.clone(),
                    volume: volume.clone(),
                    at_ms,
                    ocr: None,
                    layers: Vec::new(),
                });
                done.len() - 1
            }
        };
        let e = &mut done[i];
        e.at_ms = at_ms;
        match layer {
            Some(id) => {
                e.layers.retain(|l| l.id != id);
                e.layers.push(crate::status::DoneLayer { id, path });
            }
            None => e.ocr = Some(path),
        }
        done.sort_by(|a, b| b.at_ms.cmp(&a.at_ms));
        done.truncate(crate::status::DONE_LIMIT);
        if let Ok(j) = serde_json::to_string(&*done) {
            let _ = self.state.meta_set("queue_done", &j);
        }
    }

    fn has_archive(&self, rel: &RelPath, vol: &str) -> bool {
        let dir = parent_of(rel);
        let idx = self.index.lock();
        [".cbz", ".zip"].iter().any(|ext| {
            dir.join(format!("{vol}{ext}"))
                .is_some_and(|p| idx.lookup(&p).is_some())
        })
    }

    // ---- Mkdir --------------------------------------------------------------------

    async fn drive_mkdir(self: &Arc<Self>, op: &mut Op) -> Result<Option<Step>, Crash> {
        let uncertain = op.state == OpState::Uploading;
        let dest = op.dest.clone();
        let name = last_name(&dest).to_string();
        let Some(parent) = self.parent_pid_of(&dest) else {
            return Ok(self
                .on_conflict(op, "the parent folder is gone".into(), None)
                .await);
        };
        if uncertain && let Ok((nodes, _)) = self.backend.find_named(&parent, &name).await {
            if let Some(n) = nodes.into_iter().find(|n| n.is_dir) {
                return self.commit_mkdir(op, n, false).map(Some);
            }
            if op.reconcile_only {
                self.finish(op, OpState::Discarded);
                return Ok(Some(Step::Done(Published::default())));
            }
        }
        op.state = OpState::Uploading;
        op.parent_pid = Some(parent.clone());
        let _ = self.save_op(op);
        self.fail_at("UPLOADING")?;
        let ctx = OpCtx {
            op_id: op.op_id(),
            resume: None,
        };
        match self.backend.mkdir(&parent, &name, &ctx).await {
            Ok(n) => {
                self.fail_at("uploaded")?;
                self.commit_mkdir(op, n, true).map(Some)
            }
            Err(BErr::NameConflict) => match self.backend.find_named(&parent, &name).await {
                // Somebody made it first: it exists, which is all create_dir_all wants.
                Ok((nodes, _)) if nodes.iter().any(|n| n.is_dir) => {
                    let n = nodes.into_iter().find(|n| n.is_dir).expect("checked");
                    self.commit_mkdir(op, n, false).map(Some)
                }
                _ => Ok(self
                    .on_conflict(op, "a file holds the name".into(), None)
                    .await),
            },
            Err(e) => Ok(self.on_error(op, e, None).await),
        }
    }

    fn commit_mkdir(&self, op: &mut Op, node: RNode, own: bool) -> Result<Step, Crash> {
        let _ = self.state.ledger_put(&LedgerRow {
            pid: node.pid.clone(),
            cver: node.cver.clone(),
            parent_pid: node.parent.clone(),
            name: nfc(&node.name_raw),
            sha256: None,
            op_id: Some(op.op_id()),
            trashed: false,
            echo_only: !own,
        });
        op.state = OpState::Committed;
        op.result_pid = Some(node.pid.clone());
        op.result_cver = Some(node.cver.clone());
        let changes = {
            let idx = self.index.lock();
            idx.begin()
                .and_then(|tx| {
                    let ch =
                        idx.upsert(&tx, &node, idx.generation(), self.caps.mtime_granularity_ns)?;
                    tx.commit()?;
                    Ok(ch)
                })
                .unwrap_or_default()
        };
        self.emit_changes(&changes);
        self.drop_op(op, true);
        self.fail_at("COMMITTED")?;
        Ok(Step::Done(Published::default()))
    }

    // ---- Move ---------------------------------------------------------------------

    async fn drive_move(self: &Arc<Self>, op: &mut Op) -> Result<Option<Step>, Crash> {
        let uncertain = op.state == OpState::Uploading;
        let (Some(pid), Some(src)) = (op.target_pid.clone(), op.src.clone()) else {
            return Ok(Some(self.give_up(op, "a move without its source".into())));
        };
        let dest = op.dest.clone();
        let name = last_name(&dest).to_string();
        let Some(parent) = self.parent_pid_of(&dest) else {
            return Ok(self
                .on_conflict(op, "the destination folder is gone".into(), None)
                .await);
        };
        if uncertain && let Ok(Some(n)) = self.backend.stat(&pid).await {
            if n.parent.as_deref() == Some(parent.as_str()) && nfc(&n.name_raw) == name {
                return self.commit_move(op, &src, &pid, n).map(Some);
            }
            if op.reconcile_only {
                self.finish(op, OpState::Discarded);
                return Ok(Some(Step::Done(Published::default())));
            }
        }
        let node = self.index.lock().node(&pid);
        let Some(node) = node else {
            return Ok(self
                .on_conflict(op, "the source is gone".into(), None)
                .await);
        };
        op.state = OpState::Uploading;
        op.parent_pid = Some(parent.clone());
        let _ = self.save_op(op);
        self.fail_at("UPLOADING")?;
        let new_parent =
            (node.parent.as_deref() != Some(parent.as_str())).then_some(parent.as_str());
        let new_name = (nfc(&node.name_raw) != name).then_some(name.as_str());
        match self.backend.update(&pid, new_parent, new_name, None).await {
            Ok(n) => {
                self.fail_at("uploaded")?;
                self.commit_move(op, &src, &pid, n).map(Some)
            }
            Err(e) => Ok(self.on_error(op, e, None).await),
        }
    }

    /// COMMITTED for a move: the ledger row follows the node (or an `echo_only` row
    /// is written), the index relocates it (path-id subtrees are rebased first), and
    /// pending ops under the moved path follow.
    fn commit_move(
        &self,
        op: &mut Op,
        src: &RelPath,
        old_pid: &str,
        node: RNode,
    ) -> Result<Step, Crash> {
        let name = nfc(&node.name_raw);
        let rebased = self.caps.path_ids && node.pid != old_pid;
        if rebased {
            let _ = self.state.ledger_rebase(old_pid, &node.pid);
        }
        if self.state.ledger_rows(old_pid).is_empty() {
            let _ = self.state.ledger_put(&LedgerRow {
                pid: node.pid.clone(),
                cver: node.cver.clone(),
                parent_pid: node.parent.clone(),
                name,
                sha256: None,
                op_id: Some(op.op_id()),
                trashed: false,
                echo_only: true,
            });
        } else {
            let cver = (!node.is_dir).then_some(node.cver.as_str());
            let _ = self
                .state
                .ledger_move(old_pid, &node.pid, cver, node.parent.as_deref(), &name);
        }
        {
            let idx = self.index.lock();
            let r = idx.begin().and_then(|tx| {
                if rebased {
                    idx.rebase_pids(&tx, old_pid, &node.pid)?;
                }
                idx.upsert(&tx, &node, idx.generation(), self.caps.mtime_granularity_ns)?;
                tx.commit()
            });
            if let Err(e) = r {
                tracing::warn!("outbox move commit: index: {e}");
            }
        }
        // Pending ops follow the moved path (and, on path-id providers, the pids).
        let dest = op.dest.clone();
        let follow: Vec<Op> = self
            .ops
            .lock()
            .values()
            .filter(|o| o.id != op.id && o.state.live())
            .cloned()
            .collect();
        for mut o in follow {
            let mut changed = false;
            if let Some(d) = rebase_rel(&o.dest, src, &dest) {
                o.dest = d;
                changed = true;
            }
            if rebased {
                for p in [&mut o.target_pid, &mut o.parent_pid].into_iter().flatten() {
                    if let Some(np) = rebase_pid(p, old_pid, &node.pid) {
                        *p = np;
                        changed = true;
                    }
                }
            }
            if changed {
                let _ = self.save_op(&o);
            }
        }
        (self.emit)(WatchEvent::Removed {
            path: self.abs(src),
            is_dir: node.is_dir,
        });
        (self.emit)(WatchEvent::Added {
            path: self.abs(&dest),
            is_dir: node.is_dir,
        });
        op.state = OpState::Committed;
        self.drop_op(op, true);
        self.fail_at("COMMITTED")?;
        Ok(Step::Done(Published::default()))
    }

    // ---- Trash / Delete -------------------------------------------------------------

    async fn drive_trash(self: &Arc<Self>, op: &mut Op) -> Result<Option<Step>, Crash> {
        let Some(pid) = op.target_pid.clone() else {
            return Ok(Some(self.give_up(op, "a delete without its node".into())));
        };
        let node = self.index.lock().node(&pid);
        if op.state == OpState::Uploading {
            match self.backend.stat(&pid).await {
                Ok(None) => return self.commit_trash(op, &pid, node).map(Some),
                Ok(Some(n)) if n.trashed => return self.commit_trash(op, &pid, node).map(Some),
                _ if op.reconcile_only => {
                    self.finish(op, OpState::Discarded);
                    return Ok(Some(Step::Done(Published::default())));
                }
                _ => {}
            }
        }
        op.state = OpState::Uploading;
        let _ = self.save_op(op);
        self.fail_at("UPLOADING")?;
        let r = if op.kind == OpKind::Delete {
            self.backend.delete_permanent(&pid).await
        } else {
            let if_cver = node
                .as_ref()
                .filter(|n| self.caps.cas_overwrite && !n.is_dir)
                .map(|n| n.cver.clone());
            self.backend.trash(&pid, if_cver.as_deref()).await
        };
        match r {
            Ok(()) | Err(BErr::NotFound) => {
                self.fail_at("uploaded")?;
                self.commit_trash(op, &pid, node).map(Some)
            }
            Err(e) => Ok(self.on_error(op, e, None).await),
        }
    }

    fn commit_trash(&self, op: &mut Op, pid: &str, node: Option<RNode>) -> Result<Step, Crash> {
        let is_dir = node.as_ref().is_some_and(|n| n.is_dir);
        if self.state.ledger_rows(pid).is_empty() {
            if let Some(n) = &node {
                let _ = self.state.ledger_put(&LedgerRow {
                    pid: pid.to_string(),
                    cver: n.cver.clone(),
                    parent_pid: n.parent.clone(),
                    name: nfc(&n.name_raw),
                    sha256: None,
                    op_id: Some(op.op_id()),
                    trashed: true,
                    echo_only: true,
                });
            }
        } else {
            let _ = self.state.ledger_trash(pid);
        }
        {
            let idx = self.index.lock();
            let r = idx.begin().and_then(|tx| {
                idx.remove(&tx, pid)?;
                tx.commit()
            });
            if let Err(e) = r {
                tracing::warn!("outbox trash commit: index: {e}");
            }
        }
        (self.emit)(WatchEvent::Removed {
            path: self.abs(&op.dest),
            is_dir,
        });
        op.state = OpState::Committed;
        self.drop_op(op, true);
        self.fail_at("COMMITTED")?;
        Ok(Step::Done(Published::default()))
    }

    // ---- the worker -----------------------------------------------------------------

    /// COMMITTED ops whose echo the feed delivered: CONFIRMED, bytes deleted.
    fn confirm_echoes(&self) {
        let committed: Vec<Op> = self
            .ops
            .lock()
            .values()
            .filter(|o| o.state == OpState::Committed)
            .cloned()
            .collect();
        if committed.is_empty() {
            return;
        }
        let mut echoed = self.echoed.lock();
        for mut op in committed {
            if let (Some(p), Some(c)) = (&op.result_pid, &op.result_cver)
                && echoed.remove(&(p.clone(), c.clone()))
            {
                op.state = OpState::Confirmed;
                self.drop_op(&op, false);
            }
        }
    }

    /// The next op the worker may run, by `seq`; live Deferred ops that must wait
    /// are marked HELD on the way.
    fn next_ready(&self) -> Result<Option<u64>, Crash> {
        let now = now_ms();
        let gate = self.gate_open();
        let mut hold = Vec::new();
        let pick = {
            let ops = self.ops.lock();
            let mut pick = None;
            for o in ops.values() {
                if o.reconcile_only || o.state == OpState::SupersedeOld {
                    pick = pick.or(Some(o.id));
                    continue;
                }
                if o.durability != Durability::Deferred || !o.state.live() {
                    continue;
                }
                if gate && self.deps_done(o, &ops) {
                    if o.next_at <= now {
                        pick = pick.or(Some(o.id));
                    }
                } else if o.state == OpState::Staged {
                    hold.push(o.clone());
                }
            }
            pick
        };
        for mut o in hold {
            o.state = OpState::Held;
            let _ = self.save_op(&o);
            self.fail_at("HELD")?;
        }
        Ok(pick)
    }

    /// Drain Deferred ops (and reconcile interrupted Sync ones) until the store stops.
    pub(crate) async fn worker(self: Arc<Self>) {
        loop {
            if self.crashed.load(Ordering::Acquire) {
                return;
            }
            self.confirm_echoes();
            let Ok(next) = self.next_ready() else {
                return;
            };
            let Some(id) = next else {
                let notified = self.wake.notified();
                let _ = tokio::time::timeout(Duration::from_millis(200), notified).await;
                continue;
            };
            let _g = self.exec.lock().await;
            let Some(mut op) = self.ops.lock().get(&id).cloned() else {
                continue;
            };
            if op.state == OpState::SupersedeOld {
                // Interrupted at SUPERSEDE_OLD: finish it.
                if let (Some(old), Some(cver)) = (op.superseded_pid.clone(), op.result_cver.clone())
                {
                    let n = RNode {
                        pid: old,
                        parent: None,
                        name_raw: Vec::new(),
                        is_dir: false,
                        size: 0,
                        base_mtime_ms: 0,
                        created_ms: 0,
                        version: String::new(),
                        cver,
                        pin: None,
                        hash: None,
                        trashed: false,
                    };
                    self.supersede(&mut op, &n).await;
                } else {
                    op.state = OpState::Committed;
                    let _ = self.save_op(&op);
                }
                continue;
            }
            match self.drive(&mut op).await {
                Err(Crash) => return,
                Ok(Step::Blocked) => {
                    drop(_g);
                    let notified = self.wake.notified();
                    let _ = tokio::time::timeout(Duration::from_millis(100), notified).await;
                }
                Ok(_) => {}
            }
        }
    }

    // ---- startup (§5.10) ----------------------------------------------------------

    /// Apply the startup table to the persisted ops and load the survivors.
    pub(crate) fn replay(&self) {
        let ops = self.load_ops();
        let mut keep = std::collections::HashSet::new();
        for mut op in ops {
            let sync = op.durability == Durability::Sync;
            match op.state {
                OpState::Prepared => self.drop_op(&op, false),
                OpState::Staged | OpState::Held | OpState::Preflight if sync => {
                    self.drop_op(&op, false)
                }
                OpState::Uploading if sync => {
                    op.reconcile_only = true;
                    keep.extend(op.staged.clone());
                    self.ops.lock().insert(op.id, op);
                }
                OpState::Staged | OpState::Held | OpState::Preflight | OpState::Uploading => {
                    keep.extend(op.staged.clone());
                    // A restart retries at once.
                    op.next_at = 0;
                    let _ = self.index.lock().set_pending(&op.dest, &op.op_id());
                    self.ops.lock().insert(op.id, op);
                }
                // The node and its ledger row are durable; the echo is not needed.
                OpState::Committed
                | OpState::Confirmed
                | OpState::Discarded
                | OpState::Superseded => self.drop_op(&op, false),
                OpState::SupersedeOld | OpState::Conflict | OpState::Failed => {
                    keep.extend(op.staged.clone());
                    self.ops.lock().insert(op.id, op);
                }
            }
        }
        // Unreferenced outbox files.
        if let Ok(rd) = std::fs::read_dir(self.outbox_dir()) {
            for f in rd.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if !keep.contains(&name) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
        }
        if let Some(d) = self
            .state
            .meta_get("queue_done")
            .and_then(|j| serde_json::from_str::<Vec<DoneEntry>>(&j).ok())
        {
            *self.done.lock() = d;
        }
    }

    // ---- staging bytes ------------------------------------------------------------

    /// Take `src` into `outbox/<id>` (a rename where possible), fsync it, and return
    /// its name, size and sha256.
    pub(crate) fn take_bytes(&self, src: &Path, id: u64) -> io::Result<(String, u64, [u8; 32])> {
        let dir = self.outbox_dir();
        std::fs::create_dir_all(&dir)?;
        let name = id.to_string();
        let dst = dir.join(&name);
        if std::fs::rename(src, &dst).is_err() {
            std::fs::copy(src, &dst)?;
            std::fs::remove_file(src)?;
        }
        let mut f = std::fs::File::open(&dst)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 256 << 10];
        let mut size = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            size += n as u64;
            h.update(&buf[..n]);
        }
        std::fs::OpenOptions::new()
            .write(true)
            .open(&dst)?
            .sync_all()?;
        Ok((name, size, h.finalize().into()))
    }
}
