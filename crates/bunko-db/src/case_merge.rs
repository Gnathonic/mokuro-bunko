//! The local case merge's journal (0.8, spec §5.9, Track A): `case_merge_groups`,
//! `case_merge_steps`, `case_merge_blocks`, `case_merge_owner_snapshots`, and
//! `series_folder_adoption_deferred`.
//!
//! A rename and a SQLite commit cannot be one atomic act, so a merge is journaled whole
//! before the first filesystem change ([`Database::open_case_merge_group`]: the group
//! and every step in one transaction), each step's filesystem change runs, and then
//! [`Database::commit_case_merge_step`] applies every DB change the step makes (its
//! [`Carry`] list) and marks it `done`, again in one transaction. A crash leaves the
//! step `planned` with the rows unmoved, and recovery decides from the listing and the
//! recorded identities whether the move happened. `done` is final: a step done is never
//! set back, and committing it again applies nothing.
//!
//! [`Carry`] is the one list of DB rules a merge event implies, shared by the local
//! executor (inside the step's commit) and the cloud's hooks
//! ([`Database::apply_carries`]). Each carry reports the rows it dropped (deleted, or
//! replaced at the destination) as JSON, for the keep folder's manifest and the audit
//! record: nothing the merge removes is lost without a trace.
//!
//! Library paths are library-relative (`Series/Vol 1.cbz`), in the bytes on disk. Times
//! here are Unix ms, given by the caller. Identities (`dev`, `ino`) are stored as their
//! bit patterns in SQLite's signed 64-bit integers.

use crate::database::Database;
use crate::error::{DbError, Result};
use crate::folders::{key_of, owner_at};
use crate::identities::{forget_volume_uuid_in, rename_volume_uuid_in};
use crate::ocr::{
    forget_ocr_sidecar_in, forget_ocr_sidecars_of_volume_in, ocr_volume_moves,
    rename_ocr_sidecar_in, rename_ocr_sidecars_of_volume_in,
};
use crate::uploads::{normalize_volume_key, rename_volume_upload_in};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Map, Value};

/// A group's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GroupState {
    /// Journaled, with steps still to run (the scope is busy).
    Open,
    /// Stopped for a reason ([`Group::pause_reason`]) until a later pass resumes it (the
    /// scope stays busy).
    Paused,
    /// Closed: every step is done, skipped or blocked.
    Done,
}

/// What a step does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepKind {
    /// Move a loser entry into the winner.
    Move,
    /// Move a superseded entry into the keep folder.
    Keep,
    /// Replace the winner's main sidecar with the loser's (the winner's kept first).
    Replace,
    /// Ingest a loser `series.json` as a submission, then keep it.
    Submit,
    /// Remove an emptied losing folder.
    Finish,
    /// Fold the accounts' overlay spellings into the winner's.
    OverlayFold,
    /// The group's last step: the effects announced once.
    Close,
}

/// Where a step is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepState {
    /// Journaled; nothing done yet (or a crash came before the commit).
    Planned,
    /// A copy into its destination is being written.
    Copying,
    /// The copy is complete and verified; the source is not unlinked yet.
    Copied,
    /// A replace: the winner's main is safe in the keep folder; the rename is next.
    Kept,
    /// The filesystem change and its DB rows are committed. Final.
    Done,
    /// Not needed any more (planned again in a later pass).
    Skipped,
    /// The disk is not what the plan judged (the source changed, the destination was
    /// taken): the rest of its volume group is skipped.
    Diverged,
    /// Refused by a rule (a block recorded).
    Blocked,
}

macro_rules! text_enum {
    ($t:ident { $($v:ident => $s:literal),* $(,)? }) => {
        impl $t {
            pub fn as_str(self) -> &'static str {
                match self { $($t::$v => $s),* }
            }
            pub fn parse(s: &str) -> Option<$t> {
                match s { $($s => Some($t::$v),)* _ => None }
            }
        }
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

text_enum!(GroupState { Open => "open", Paused => "paused", Done => "done" });
text_enum!(StepKind {
    Move => "move",
    Keep => "keep",
    Replace => "replace",
    Submit => "submit",
    Finish => "finish",
    OverlayFold => "overlay_fold",
    Close => "close",
});
text_enum!(StepState {
    Planned => "planned",
    Copying => "copying",
    Copied => "copied",
    Kept => "kept",
    Done => "done",
    Skipped => "skipped",
    Diverged => "diverged",
    Blocked => "blocked",
});

/// Which file a name pointed at, and its size and mtime, as the plan judged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i64,
}

/// A group to journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGroup {
    pub id: String,
    /// The core `Scope`, as JSON.
    pub scope: String,
    /// The folder the spellings are in (empty: the library root).
    pub parent: String,
    pub winner: String,
    pub losers: Vec<String>,
    /// `auto`, or `admin` (Keep this / Keep that).
    pub origin: String,
    /// The plan's fingerprint (a pass that would plan the same again plans nothing).
    pub fingerprint: String,
    pub created_at: i64,
}

/// A journaled group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub scope: String,
    pub parent: String,
    pub winner: String,
    pub losers: Vec<String>,
    pub origin: String,
    pub state: GroupState,
    pub pause_reason: Option<String>,
    pub fingerprint: String,
    pub created_at: i64,
    pub finished_at: Option<i64>,
    pub moved: u64,
    pub kept: u64,
    pub blocked: u64,
    pub audited: bool,
}

/// A step to journal (its `seq` is its place in the list given).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewStep {
    /// The plan's phase (moves and replaces, submits, removals, finishes, then the
    /// local-only overlay fold and close).
    pub phase: i64,
    pub kind: StepKind,
    /// The volume group (a diverged step skips the rest of it).
    pub vg: String,
    pub src: Option<String>,
    pub dest: Option<String>,
    pub keep: Option<String>,
    pub is_dir: bool,
    pub src_stat: Option<Stat>,
    pub sha256: Option<String>,
    /// The core step `Extra`, as JSON.
    pub extra: Option<String>,
}

impl NewStep {
    /// A step of `kind` with every other field empty.
    pub fn new(kind: StepKind) -> NewStep {
        NewStep {
            phase: 0,
            kind,
            vg: String::new(),
            src: None,
            dest: None,
            keep: None,
            is_dir: false,
            src_stat: None,
            sha256: None,
            extra: None,
        }
    }
}

/// A journaled step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub group_id: String,
    pub seq: i64,
    pub phase: i64,
    pub kind: StepKind,
    pub vg: String,
    pub src: Option<String>,
    pub dest: Option<String>,
    pub keep: Option<String>,
    pub is_dir: bool,
    pub src_stat: Option<Stat>,
    /// The identity verified at the destination.
    pub dest_id: Option<(u64, u64)>,
    pub sha256: Option<String>,
    pub extra: Option<String>,
    pub state: StepState,
    /// The move rung that ran (`bunko_store::MoveMode` as text).
    pub mode: Option<String>,
    pub detail: Option<String>,
}

/// What a step learned, written with its state. A `None` leaves the column as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StepNote {
    pub mode: Option<String>,
    pub detail: Option<String>,
    pub dest_id: Option<(u64, u64)>,
    pub sha256: Option<String>,
    pub keep: Option<String>,
}

/// What a committed step adds to its group's counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tally {
    pub moved: u64,
    pub kept: u64,
    pub blocked: u64,
}

/// A block to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBlock {
    pub group_id: Option<String>,
    pub scope: String,
    pub this: Vec<String>,
    pub that: Vec<String>,
    pub detail: String,
}

/// A recorded block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub id: i64,
    pub group_id: Option<String>,
    pub scope: String,
    pub this: Vec<String>,
    pub that: Vec<String>,
    pub detail: String,
    pub first_seen: i64,
    pub resolved_at: Option<i64>,
    /// `keep-this`, `keep-that`, or how else it ended.
    pub resolution: Option<String>,
}

/// A losing root folder's owner before the merge moved any of its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerSnapshot {
    pub loser: String,
    pub winner: String,
    /// The owner record of the loser's series key.
    pub loser_owner: Option<String>,
    /// The uploaders of the loser folder's tracked volumes, sorted, distinct.
    pub loser_uploaders: Vec<String>,
    pub taken_at: i64,
}

/// One DB rule a merge event implies. Paths are library-relative; an archive's rows
/// are keyed as the public methods key them (`normalize_volume_key`: a sidecar path
/// names its archive).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Carry {
    /// An archive moved: its ownership row follows (`rename_volume_upload`; a row
    /// already at `new` is replaced, and reported dropped).
    RenameUpload { old: String, new: String },
    /// An identical loser archive was kept aside: its ownership row moves to the
    /// winner's key only when the winner has none; otherwise it is dropped (reported).
    CarryUploadIfAbsent { from: String, to: String },
    /// The archive's ownership row goes (exact key).
    ForgetUpload { path: String },
    /// An archive moved: its sidecars' rows follow, re-spelled under its new stem
    /// (`rename_ocr_sidecars_of_volume`; rows replaced at the new paths are reported).
    RenameOcrVolume { old: String, new: String },
    /// One sidecar or layer moved, now belonging to `volume`.
    RenameOcrSidecar {
        old: String,
        new: String,
        volume: String,
    },
    /// One sidecar, layer or cover was kept aside: its row goes.
    ForgetOcrSidecar { path: String },
    /// Every sidecar row of the archive `volume` goes.
    ForgetOcrVolume { volume: String },
    /// An archive moved: the uuid its primary carried follows (a row at `new` is
    /// replaced, and reported).
    RenameUuid { old: String, new: String },
    /// An identical loser archive was kept aside: its uuid moves to the winner's key
    /// only when the winner has none; otherwise it is dropped (reported).
    CarryUuidIfAbsent { from: String, to: String },
    /// The archive's remembered uuid goes.
    ForgetUuid { path: String },
    /// The archive's compiled catalog entry goes (a cache: rebuilt, not reported).
    ForgetEntryCache { volume: String },
}

/// What one [`Carry`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CarryOutcome {
    /// Rows moved, inserted or deleted.
    pub changed: usize,
    /// Every row it deleted or replaced, as `{"table": ..., <column>: <value>...}`.
    pub dropped: Vec<Value>,
}

fn require_id(v: &str, what: &'static str) -> Result<()> {
    if v.is_empty() {
        return Err(DbError::invalid(format!("a case merge {what} needs an id")));
    }
    Ok(())
}

fn json_list(v: &[String]) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn parse_list(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn corrupt(table: &'static str, detail: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(DbError::CorruptRow { table, detail }),
    )
}

const GROUP_COLUMNS: &str = "id, scope, parent, winner, losers, origin, state, pause_reason, \
    fingerprint, created_at, finished_at, moved, kept, blocked, audited";

fn group_from_row(r: &Row<'_>) -> rusqlite::Result<Group> {
    let state: String = r.get(6)?;
    Ok(Group {
        id: r.get(0)?,
        scope: r.get(1)?,
        parent: r.get(2)?,
        winner: r.get(3)?,
        losers: parse_list(&r.get::<_, String>(4)?),
        origin: r.get(5)?,
        state: GroupState::parse(&state)
            .ok_or_else(|| corrupt("case_merge_groups", format!("state {state:?}")))?,
        pause_reason: r.get(7)?,
        fingerprint: r.get(8)?,
        created_at: r.get(9)?,
        finished_at: r.get(10)?,
        moved: r.get::<_, i64>(11)? as u64,
        kept: r.get::<_, i64>(12)? as u64,
        blocked: r.get::<_, i64>(13)? as u64,
        audited: r.get::<_, i64>(14)? != 0,
    })
}

const STEP_COLUMNS: &str = "group_id, seq, phase, kind, vg, src, dest, keep, is_dir, src_dev, \
    src_ino, src_size, src_mtime_ns, dest_dev, dest_ino, sha256, extra, state, mode, detail";

fn step_from_row(r: &Row<'_>) -> rusqlite::Result<Step> {
    let kind: String = r.get(3)?;
    let state: String = r.get(17)?;
    let src_stat = match (
        r.get::<_, Option<i64>>(9)?,
        r.get::<_, Option<i64>>(10)?,
        r.get::<_, Option<i64>>(11)?,
        r.get::<_, Option<i64>>(12)?,
    ) {
        (Some(dev), Some(ino), Some(size), Some(mtime_ns)) => Some(Stat {
            dev: dev as u64,
            ino: ino as u64,
            size: size as u64,
            mtime_ns,
        }),
        _ => None,
    };
    let dest_id = match (r.get::<_, Option<i64>>(13)?, r.get::<_, Option<i64>>(14)?) {
        (Some(dev), Some(ino)) => Some((dev as u64, ino as u64)),
        _ => None,
    };
    Ok(Step {
        group_id: r.get(0)?,
        seq: r.get(1)?,
        phase: r.get(2)?,
        kind: StepKind::parse(&kind)
            .ok_or_else(|| corrupt("case_merge_steps", format!("kind {kind:?}")))?,
        vg: r.get(4)?,
        src: r.get(5)?,
        dest: r.get(6)?,
        keep: r.get(7)?,
        is_dir: r.get::<_, i64>(8)? != 0,
        src_stat,
        dest_id,
        sha256: r.get(15)?,
        extra: r.get(16)?,
        state: StepState::parse(&state)
            .ok_or_else(|| corrupt("case_merge_steps", format!("state {state:?}")))?,
        mode: r.get(18)?,
        detail: r.get(19)?,
    })
}

const BLOCK_COLUMNS: &str =
    "id, group_id, scope, this, that, detail, first_seen, resolved_at, resolution";

fn block_from_row(r: &Row<'_>) -> rusqlite::Result<Block> {
    Ok(Block {
        id: r.get(0)?,
        group_id: r.get(1)?,
        scope: r.get(2)?,
        this: parse_list(&r.get::<_, String>(3)?),
        that: parse_list(&r.get::<_, String>(4)?),
        detail: r.get(5)?,
        first_seen: r.get(6)?,
        resolved_at: r.get(7)?,
        resolution: r.get(8)?,
    })
}

fn snapshot_from_row(r: &Row<'_>) -> rusqlite::Result<OwnerSnapshot> {
    Ok(OwnerSnapshot {
        loser: r.get(0)?,
        winner: r.get(1)?,
        loser_owner: r.get(2)?,
        loser_uploaders: parse_list(&r.get::<_, String>(3)?),
        taken_at: r.get(4)?,
    })
}

/// The row of `table` whose `col` is `key`, as `{"table": table, <column>: value...}`.
fn row_json(conn: &Connection, table: &'static str, col: &str, key: &str) -> Result<Option<Value>> {
    let mut stmt = conn.prepare_cached(&format!("SELECT * FROM {table} WHERE {col} = ?"))?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    Ok(stmt
        .query_row([key], |r| {
            let mut m = Map::new();
            m.insert("table".into(), Value::from(table));
            for (i, name) in names.iter().enumerate() {
                let v = match r.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => Value::from(n),
                    rusqlite::types::ValueRef::Real(f) => Value::from(f),
                    rusqlite::types::ValueRef::Text(t) => {
                        Value::from(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => Value::from(b.to_vec()),
                };
                m.insert(name.clone(), v);
            }
            Ok(Value::Object(m))
        })
        .optional()?)
}

/// Move the row of `table` keyed `from` to `to` when `to` has none; otherwise drop it.
fn carry_if_absent(
    conn: &Connection,
    table: &'static str,
    from: &str,
    to: &str,
) -> Result<CarryOutcome> {
    let mut out = CarryOutcome::default();
    if from == to {
        return Ok(out);
    }
    let Some(row) = row_json(conn, table, "volume_key", from)? else {
        return Ok(out);
    };
    if row_json(conn, table, "volume_key", to)?.is_some() {
        out.changed = conn.execute(&format!("DELETE FROM {table} WHERE volume_key = ?"), [from])?;
        out.dropped.push(row);
    } else {
        out.changed = conn.execute(
            &format!("UPDATE {table} SET volume_key = ? WHERE volume_key = ?"),
            params![to, from],
        )?;
    }
    Ok(out)
}

/// Apply one carry inside the caller's transaction.
pub(crate) fn apply_carry_in(conn: &Connection, carry: &Carry) -> Result<CarryOutcome> {
    let key = |p: &str| normalize_volume_key(p);
    let mut out = CarryOutcome::default();
    match carry {
        Carry::RenameUpload { old, new } => {
            let (Some(old), Some(new)) = (key(old), key(new)) else {
                return Ok(out);
            };
            if old == new || row_json(conn, "volume_uploads", "volume_key", &old)?.is_none() {
                return Ok(out);
            }
            if let Some(row) = row_json(conn, "volume_uploads", "volume_key", &new)? {
                out.dropped.push(row);
            }
            rename_volume_upload_in(conn, &old, &new, &|_| false)?;
            out.changed = 1;
        }
        Carry::CarryUploadIfAbsent { from, to } => {
            if let (Some(from), Some(to)) = (key(from), key(to)) {
                out = carry_if_absent(conn, "volume_uploads", &from, &to)?;
            }
        }
        Carry::ForgetUpload { path } => {
            if let Some(k) = key(path)
                && let Some(row) = row_json(conn, "volume_uploads", "volume_key", &k)?
            {
                out.changed =
                    conn.execute("DELETE FROM volume_uploads WHERE volume_key = ?", [&k])?;
                out.dropped.push(row);
            }
        }
        Carry::RenameOcrVolume { old, new } => {
            for (from, to) in ocr_volume_moves(conn, old, new)? {
                if from != to
                    && let Some(row) = row_json(conn, "ocr_sidecars", "sidecar_path", &to)?
                {
                    out.dropped.push(row);
                }
            }
            out.changed = rename_ocr_sidecars_of_volume_in(conn, old, new)?;
        }
        Carry::RenameOcrSidecar { old, new, volume } => {
            let (o, n) = (old.trim_matches('/'), new.trim_matches('/'));
            if o != n && row_json(conn, "ocr_sidecars", "sidecar_path", o)?.is_some() {
                if let Some(row) = row_json(conn, "ocr_sidecars", "sidecar_path", n)? {
                    out.dropped.push(row);
                }
                out.changed = rename_ocr_sidecar_in(conn, old, new, volume)?;
            }
        }
        Carry::ForgetOcrSidecar { path } => {
            let p = path.trim_matches('/');
            if let Some(row) = row_json(conn, "ocr_sidecars", "sidecar_path", p)? {
                out.dropped.push(row);
            }
            out.changed = forget_ocr_sidecar_in(conn, p)?;
        }
        Carry::ForgetOcrVolume { volume } => {
            let v = volume.trim_matches('/');
            let paths: Vec<String> = conn
                .prepare_cached(
                    "SELECT sidecar_path FROM ocr_sidecars WHERE volume_key = ? \
                     ORDER BY sidecar_path",
                )?
                .query_map([v], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for p in paths {
                if let Some(row) = row_json(conn, "ocr_sidecars", "sidecar_path", &p)? {
                    out.dropped.push(row);
                }
            }
            out.changed = forget_ocr_sidecars_of_volume_in(conn, v)?;
        }
        Carry::RenameUuid { old, new } => {
            let (Some(old), Some(new)) = (key(old), key(new)) else {
                return Ok(out);
            };
            if old == new || row_json(conn, "volume_identities", "volume_key", &old)?.is_none() {
                return Ok(out);
            }
            if let Some(row) = row_json(conn, "volume_identities", "volume_key", &new)? {
                out.dropped.push(row);
            }
            out.changed = rename_volume_uuid_in(conn, &old, &new)?;
        }
        Carry::CarryUuidIfAbsent { from, to } => {
            if let (Some(from), Some(to)) = (key(from), key(to)) {
                out = carry_if_absent(conn, "volume_identities", &from, &to)?;
            }
        }
        Carry::ForgetUuid { path } => {
            if let Some(k) = key(path) {
                if let Some(row) = row_json(conn, "volume_identities", "volume_key", &k)? {
                    out.dropped.push(row);
                }
                out.changed = forget_volume_uuid_in(conn, &k)?;
            }
        }
        Carry::ForgetEntryCache { volume } => {
            out.changed = conn.execute(
                "DELETE FROM series_entry_cache WHERE volume_key = ?",
                [volume.trim_matches('/')],
            )?;
        }
    }
    Ok(out)
}

fn insert_step(conn: &Connection, gid: &str, seq: i64, s: &NewStep) -> Result<()> {
    let kind = s.kind;
    let stat = s.src_stat;
    conn.prepare_cached(
        "INSERT INTO case_merge_steps (group_id, seq, phase, kind, vg, src, dest, keep, is_dir, \
         src_dev, src_ino, src_size, src_mtime_ns, sha256, extra, state) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'planned')",
    )?
    .execute(params![
        gid,
        seq,
        s.phase,
        kind.as_str(),
        s.vg,
        s.src,
        s.dest,
        s.keep,
        s.is_dir as i64,
        stat.map(|t| t.dev as i64),
        stat.map(|t| t.ino as i64),
        stat.map(|t| t.size as i64),
        stat.map(|t| t.mtime_ns),
        s.sha256,
        s.extra,
    ])?;
    Ok(())
}

/// Write `state` and the note's fields to an unfinished step; whether it was one.
fn note_step(
    conn: &Connection,
    gid: &str,
    seq: i64,
    state: StepState,
    note: &StepNote,
) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE case_merge_steps SET state = ?, \
         mode = COALESCE(?, mode), detail = COALESCE(?, detail), \
         dest_dev = COALESCE(?, dest_dev), dest_ino = COALESCE(?, dest_ino), \
         sha256 = COALESCE(?, sha256), keep = COALESCE(?, keep) \
         WHERE group_id = ? AND seq = ? AND state != 'done'",
        params![
            state.as_str(),
            note.mode,
            note.detail,
            note.dest_id.map(|(d, _)| d as i64),
            note.dest_id.map(|(_, i)| i as i64),
            note.sha256,
            note.keep,
            gid,
            seq,
        ],
    )? > 0)
}

fn step_state(conn: &Connection, gid: &str, seq: i64) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT state FROM case_merge_steps WHERE group_id = ? AND seq = ?")?
        .query_row(params![gid, seq], |r| r.get(0))
        .optional()?)
}

impl Database {
    /// Journal a group and all its steps (`seq` = place in `steps`) in ONE transaction:
    /// a pass sees the whole plan or none of it. The group starts `open`, each step
    /// `planned`. An id already journaled is refused.
    pub fn open_case_merge_group(&self, g: &NewGroup, steps: &[NewStep]) -> Result<()> {
        require_id(&g.id, "group")?;
        self.write(|conn| {
            let taken = conn
                .query_row(
                    "SELECT 1 FROM case_merge_groups WHERE id = ?",
                    [&g.id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if taken {
                return Err(DbError::Conflict(format!(
                    "case merge group {} already exists",
                    g.id
                )));
            }
            conn.execute(
                "INSERT INTO case_merge_groups (id, scope, parent, winner, losers, origin, state, \
                 fingerprint, created_at) VALUES (?, ?, ?, ?, ?, ?, 'open', ?, ?)",
                params![
                    g.id,
                    g.scope,
                    g.parent,
                    g.winner,
                    json_list(&g.losers),
                    g.origin,
                    g.fingerprint,
                    g.created_at
                ],
            )?;
            for (seq, s) in steps.iter().enumerate() {
                insert_step(conn, &g.id, seq as i64, s)?;
            }
            Ok(())
        })
    }

    pub fn case_merge_group(&self, id: &str) -> Result<Option<Group>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {GROUP_COLUMNS} FROM case_merge_groups WHERE id = ?"
                ))?
                .query_row([id], group_from_row)
                .optional()?)
        })
    }

    /// Every group not done (open or paused), oldest first: what a start resumes and
    /// what keeps its scope busy.
    pub fn unfinished_case_merge_groups(&self) -> Result<Vec<Group>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {GROUP_COLUMNS} FROM case_merge_groups WHERE state != 'done' \
                     ORDER BY created_at, rowid"
                ))?
                .query_map([], group_from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The latest `limit` groups, newest first.
    pub fn recent_case_merge_groups(&self, limit: usize) -> Result<Vec<Group>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {GROUP_COLUMNS} FROM case_merge_groups \
                     ORDER BY created_at DESC, rowid DESC LIMIT ?"
                ))?
                .query_map([limit as i64], group_from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Finished groups of `scope` by `origin` created at or after `since` (Unix ms)
    /// that changed something: the recreate limit's count.
    pub fn case_merge_recreations(&self, scope: &str, origin: &str, since: i64) -> Result<u64> {
        self.read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM case_merge_groups WHERE scope = ? AND origin = ? \
                 AND state = 'done' AND created_at >= ? AND moved + kept > 0",
                params![scope, origin, since],
                |r| r.get::<_, i64>(0),
            )? as u64)
        })
    }

    /// Pause an unfinished group, saying why; whether it was unfinished.
    pub fn pause_case_merge_group(&self, id: &str, reason: &str) -> Result<bool> {
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE case_merge_groups SET state = 'paused', pause_reason = ? \
                 WHERE id = ? AND state != 'done'",
                params![reason, id],
            )? > 0)
        })
    }

    /// Resume a paused group; whether it was paused.
    pub fn resume_case_merge_group(&self, id: &str) -> Result<bool> {
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE case_merge_groups SET state = 'open', pause_reason = NULL \
                 WHERE id = ? AND state = 'paused'",
                [id],
            )? > 0)
        })
    }

    /// Close a group at `finished_at` (Unix ms); whether it was unfinished. Its steps
    /// are left as they ended.
    pub fn close_case_merge_group(&self, id: &str, finished_at: i64) -> Result<bool> {
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE case_merge_groups SET state = 'done', pause_reason = NULL, \
                 finished_at = ? WHERE id = ? AND state != 'done'",
                params![finished_at, id],
            )? > 0)
        })
    }

    /// The close's owner reconciliation ran for this group.
    pub fn mark_case_merge_group_audited(&self, id: &str) -> Result<bool> {
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE case_merge_groups SET audited = 1 WHERE id = ?",
                [id],
            )? > 0)
        })
    }

    /// A group's steps, in run order.
    pub fn case_merge_steps(&self, group_id: &str) -> Result<Vec<Step>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {STEP_COLUMNS} FROM case_merge_steps WHERE group_id = ? ORDER BY seq"
                ))?
                .query_map([group_id], step_from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn case_merge_step(&self, group_id: &str, seq: i64) -> Result<Option<Step>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {STEP_COLUMNS} FROM case_merge_steps WHERE group_id = ? AND seq = ?"
                ))?
                .query_row(params![group_id, seq], step_from_row)
                .optional()?)
        })
    }

    /// Move a step to `state` (any but `done`: that is [`Self::commit_case_merge_step`]'s)
    /// with what it learned. A step already done is left done: `Ok(false)`, as for one
    /// that does not exist.
    pub fn set_case_merge_step_state(
        &self,
        group_id: &str,
        seq: i64,
        state: StepState,
        note: &StepNote,
    ) -> Result<bool> {
        if state == StepState::Done {
            return Err(DbError::invalid(
                "a case merge step is done only by its commit (with its rows)",
            ));
        }
        self.write(|conn| note_step(conn, group_id, seq, state, note))
    }

    /// A step's filesystem change happened: in ONE transaction apply every carry, mark
    /// the step `done` with the note, and add `tally` to its group. `Ok(None)` when the
    /// step was already done (nothing is applied twice); `Ok(Some(outcomes))`, one per
    /// carry, when it is done now. Any failure rolls all of it back, leaving the step
    /// as it was. A step that does not exist is refused.
    pub fn commit_case_merge_step(
        &self,
        group_id: &str,
        seq: i64,
        carries: &[Carry],
        tally: Tally,
        note: &StepNote,
    ) -> Result<Option<Vec<CarryOutcome>>> {
        self.write(|conn| {
            match step_state(conn, group_id, seq)?.as_deref() {
                None => {
                    return Err(DbError::invalid(format!(
                        "no case merge step {group_id}/{seq}"
                    )));
                }
                Some("done") => return Ok(None),
                Some(_) => {}
            }
            let mut outcomes = Vec::with_capacity(carries.len());
            for c in carries {
                outcomes.push(apply_carry_in(conn, c)?);
            }
            note_step(conn, group_id, seq, StepState::Done, note)?;
            conn.execute(
                "UPDATE case_merge_groups SET moved = moved + ?, kept = kept + ?, \
                 blocked = blocked + ? WHERE id = ?",
                params![
                    tally.moved as i64,
                    tally.kept as i64,
                    tally.blocked as i64,
                    group_id
                ],
            )?;
            Ok(Some(outcomes))
        })
    }

    /// Apply `carries` in ONE transaction (the cloud merge's hooks); one outcome each.
    pub fn apply_carries(&self, carries: &[Carry]) -> Result<Vec<CarryOutcome>> {
        self.write(|conn| carries.iter().map(|c| apply_carry_in(conn, c)).collect())
    }

    /// Record a block at `at` (Unix ms) and return its id. An unresolved block of the
    /// same scope with the same `this` and `that` is the same block: its id is
    /// returned, its detail updated, and its first sighting kept.
    pub fn record_case_merge_block(&self, b: &NewBlock, at: i64) -> Result<i64> {
        let (this, that) = (json_list(&b.this), json_list(&b.that));
        self.write(|conn| {
            let existing: Option<i64> = conn
                .query_row(
                    "SELECT id FROM case_merge_blocks WHERE scope = ? AND this = ? AND that = ? \
                     AND resolved_at IS NULL ORDER BY id LIMIT 1",
                    params![b.scope, this, that],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                conn.execute(
                    "UPDATE case_merge_blocks SET detail = ?, \
                     group_id = COALESCE(?, group_id) WHERE id = ?",
                    params![b.detail, b.group_id, id],
                )?;
                return Ok(id);
            }
            conn.execute(
                "INSERT INTO case_merge_blocks (group_id, scope, this, that, detail, first_seen) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![b.group_id, b.scope, this, that, b.detail, at],
            )?;
            Ok(conn.last_insert_rowid())
        })
    }

    /// The blocks, oldest first: only the unresolved ones, or all.
    pub fn case_merge_blocks(&self, unresolved_only: bool) -> Result<Vec<Block>> {
        self.read(|conn| {
            let filter = if unresolved_only {
                "WHERE resolved_at IS NULL"
            } else {
                ""
            };
            Ok(conn
                .prepare(&format!(
                    "SELECT {BLOCK_COLUMNS} FROM case_merge_blocks {filter} ORDER BY id"
                ))?
                .query_map([], block_from_row)?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    pub fn case_merge_block(&self, id: i64) -> Result<Option<Block>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(&format!(
                    "SELECT {BLOCK_COLUMNS} FROM case_merge_blocks WHERE id = ?"
                ))?
                .query_row([id], block_from_row)
                .optional()?)
        })
    }

    /// Resolve a block (`keep-this`, `keep-that`, ...) at `at`; whether it was open.
    pub fn resolve_case_merge_block(&self, id: i64, resolution: &str, at: i64) -> Result<bool> {
        self.write(|conn| {
            Ok(conn.execute(
                "UPDATE case_merge_blocks SET resolved_at = ?, resolution = ? \
                 WHERE id = ? AND resolved_at IS NULL",
                params![at, resolution, id],
            )? > 0)
        })
    }

    /// Snapshot the losing root folder `loser`'s owner before any step moves its rows:
    /// the owner record of its series key and the uploaders of its tracked volumes,
    /// read and written in one transaction. The first snapshot of a loser stays (a
    /// later group judging residue uses it); whether this one was taken.
    pub fn take_case_merge_owner_snapshot(
        &self,
        loser: &str,
        winner: &str,
        at: i64,
    ) -> Result<bool> {
        let loser = loser.trim_matches('/');
        if loser.is_empty() {
            return Ok(false);
        }
        let head = format!("{loser}/");
        self.write(|conn| {
            let owner = match key_of(loser) {
                Some(k) => owner_at(conn, &k)?,
                None => None,
            };
            let uploaders: Vec<String> = conn
                .prepare_cached(
                    "SELECT DISTINCT uploader_username FROM volume_uploads \
                     WHERE substr(volume_key, 1, ?) = ? ORDER BY uploader_username",
                )?
                .query_map(params![head.chars().count() as i64, head], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(conn.execute(
                "INSERT OR IGNORE INTO case_merge_owner_snapshots \
                 (loser, winner, loser_owner, loser_uploaders, taken_at) VALUES (?, ?, ?, ?, ?)",
                params![
                    loser,
                    winner.trim_matches('/'),
                    owner,
                    json_list(&uploaders),
                    at
                ],
            )? > 0)
        })
    }

    pub fn case_merge_owner_snapshot(&self, loser: &str) -> Result<Option<OwnerSnapshot>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT loser, winner, loser_owner, loser_uploaders, taken_at \
                     FROM case_merge_owner_snapshots WHERE loser = ?",
                )?
                .query_row([loser.trim_matches('/')], snapshot_from_row)
                .optional()?)
        })
    }

    /// The reconciliation that used it is done; whether there was one.
    pub fn forget_case_merge_owner_snapshot(&self, loser: &str) -> Result<bool> {
        self.write(|conn| {
            Ok(conn.execute(
                "DELETE FROM case_merge_owner_snapshots WHERE loser = ?",
                [loser.trim_matches('/')],
            )? > 0)
        })
    }

    /// The startup adoption skipped these spellings of a series (two were listed):
    /// recorded under each one's series key, adding to the spellings already there.
    /// How many keys were recorded.
    pub fn defer_series_folder_adoption(&self, folders: &[String]) -> Result<usize> {
        let mut by_key: std::collections::BTreeMap<String, Vec<String>> = Default::default();
        for f in folders {
            if let Some(k) = key_of(f) {
                by_key
                    .entry(k)
                    .or_default()
                    .push(f.trim_matches('/').to_string());
            }
        }
        self.write(|conn| {
            for (key, names) in &by_key {
                let mut all: Vec<String> = conn
                    .query_row(
                        "SELECT folders FROM series_folder_adoption_deferred WHERE series_key = ?",
                        [key],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                    .map(|s| parse_list(&s))
                    .unwrap_or_default();
                all.extend(names.iter().cloned());
                all.sort();
                all.dedup();
                conn.execute(
                    "INSERT INTO series_folder_adoption_deferred (series_key, folders) VALUES (?, ?) \
                     ON CONFLICT(series_key) DO UPDATE SET folders = excluded.folders",
                    params![key, json_list(&all)],
                )?;
            }
            Ok(by_key.len())
        })
    }

    /// Every deferred adoption: `(series key, spellings)`, by key.
    pub fn deferred_series_folder_adoptions(&self) -> Result<Vec<(String, Vec<String>)>> {
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT series_key, folders FROM series_folder_adoption_deferred \
                     ORDER BY series_key",
                )?
                .query_map([], |r| Ok((r.get(0)?, parse_list(&r.get::<_, String>(1)?))))?
                .collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The deferred spellings of `folder`'s series, if its adoption was deferred.
    pub fn deferred_series_folder_adoption(&self, folder: &str) -> Result<Option<Vec<String>>> {
        let Some(key) = key_of(folder) else {
            return Ok(None);
        };
        self.read(|conn| {
            Ok(conn
                .prepare_cached(
                    "SELECT folders FROM series_folder_adoption_deferred WHERE series_key = ?",
                )?
                .query_row([&key], |r| r.get::<_, String>(0))
                .optional()?
                .map(|s| parse_list(&s)))
        })
    }

    /// `folder`'s series was adopted (or needs no adoption); whether it was deferred.
    pub fn forget_deferred_series_folder_adoption(&self, folder: &str) -> Result<bool> {
        let Some(key) = key_of(folder) else {
            return Ok(false);
        };
        self.write(|conn| {
            Ok(conn.execute(
                "DELETE FROM series_folder_adoption_deferred WHERE series_key = ?",
                [&key],
            )? > 0)
        })
    }
}
