//! `index.sqlite` (spec §5.1): the disposable map from provider nodes to library
//! paths, fed by the change feed.
//!
//! A `node` row is one provider object; an `entry` row is one library path (NFC
//! segments under the merged root, [`ROOT_EID`]). Several nodes can share an entry
//! (§5.9): NFC-equal folders merge (the oldest-created member is the winner, the
//! others are role 1, children are the union); NFC-equal files shadow each other (the
//! newest-created wins, the rest are role 2 and reported as `dup-file`); a folder beats
//! a file of the same name (`file-vs-dir`). Case variants are distinct entries. Every
//! root candidate named exactly like the root is a member of the root entry; one
//! found inside the tree is an ordinary folder and a `nested-root` conflict.
//!
//! Writes run inside the caller's transaction ([`Index::begin`]) so a feed batch and
//! its cursor commit together; they report what became visible as [`IndexChange`]s.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fmt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::backend::{HashKind, Pid, RNode};
use crate::names::{effective_mtime_ns, fold, is_safe_segment, nfc};

/// The merged root's entry id.
pub const ROOT_EID: i64 = 1;

const KIND_FILE: i64 = 0;
const KIND_DIR: i64 = 1;
const ROLE_WINNER: i64 = 0;
const ROLE_MEMBER: i64 = 1;
const ROLE_SHADOWED: i64 = 2;
const DEFAULT_GRANULARITY_NS: i64 = 1_000_000_000;
/// A folder's mtime moves at least this much on a child change (1 µs).
const DIR_BUMP_NS: i64 = 1_000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS entry(eid INTEGER PRIMARY KEY, parent_eid INTEGER REFERENCES entry(eid),
  name TEXT NOT NULL, fold TEXT NOT NULL,
  kind INTEGER NOT NULL, size INTEGER NOT NULL DEFAULT 0, mtime_ns INTEGER NOT NULL,
  cver TEXT, own INTEGER NOT NULL DEFAULT 0,
  winner_pid TEXT, pending_op TEXT,
  members INTEGER NOT NULL DEFAULT 1, UNIQUE(parent_eid, name));
CREATE INDEX IF NOT EXISTS entry_fold ON entry(parent_eid, fold);
CREATE TABLE IF NOT EXISTS node(pid TEXT PRIMARY KEY, parent_pid TEXT, eid INTEGER REFERENCES entry(eid),
  name_raw BLOB NOT NULL, kind INTEGER, size INTEGER,
  base_mtime_ms INTEGER, created_ms INTEGER,
  version TEXT NOT NULL, cver TEXT NOT NULL,
  pin TEXT, hash_kind TEXT, hash TEXT,
  role INTEGER NOT NULL DEFAULT 0,
  seen_gen INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS node_parent ON node(parent_pid);
CREATE INDEX IF NOT EXISTS node_eid ON node(eid);
CREATE TABLE IF NOT EXISTS tombstone(pid TEXT PRIMARY KEY, path TEXT, is_dir INTEGER, removed_at INTEGER, applied INTEGER);
CREATE TABLE IF NOT EXISTS blob(sha256 TEXT PRIMARY KEY, size INTEGER, last_access INTEGER);
CREATE TABLE IF NOT EXISTS cached(pid TEXT, cver TEXT, sha256 TEXT, PRIMARY KEY(pid, cver));
CREATE TABLE IF NOT EXISTS zipdir(pid TEXT, cver TEXT, eocd_off INTEGER, cd BLOB, PRIMARY KEY(pid, cver));
CREATE TABLE IF NOT EXISTS spool(pid TEXT, cver TEXT, file TEXT, size INTEGER, filled INTEGER, verified INTEGER,
  pins INTEGER, last_access INTEGER, PRIMARY KEY(pid, cver));
CREATE TABLE IF NOT EXISTS api_log(at INTEGER, class TEXT, status INTEGER, ms INTEGER, units INTEGER, bytes INTEGER);
INSERT OR IGNORE INTO entry(eid, parent_eid, name, fold, kind, mtime_ns, members) VALUES(1, NULL, '', '', 1, 0, 0);
";

/// A library path under the merged root: NFC segments, `[]` for the root itself.
///
/// Every segment passes [`is_safe_segment`] (no `..`, `.`, separators or control
/// characters): the constructors refuse anything else, so a `RelPath` can always be
/// joined under the store root without aliasing or escaping it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(Vec<String>);

impl RelPath {
    /// The merged root.
    pub fn root() -> RelPath {
        RelPath(Vec::new())
    }

    /// Split a `/`-separated path; empty segments (doubled or edge slashes) are
    /// dropped. `None` when any segment is unsafe (`..`, `.`, `\`, control chars).
    pub fn parse(s: &str) -> Option<RelPath> {
        let mut p = RelPath::root();
        for seg in s.split('/').filter(|x| !x.is_empty()) {
            p.push_checked(seg).ok()?;
        }
        Some(p)
    }

    /// A path from segments; `None` when any segment is unsafe.
    pub fn from_segments<I, S>(segs: I) -> Option<RelPath>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut p = RelPath::root();
        for seg in segs {
            p.push_checked(seg).ok()?;
        }
        Some(p)
    }

    /// Append one segment if it is safe; otherwise hand it back unchanged.
    pub fn push_checked(&mut self, seg: impl Into<String>) -> Result<(), String> {
        let seg = seg.into();
        if is_safe_segment(&seg) {
            self.0.push(seg);
            Ok(())
        } else {
            Err(seg)
        }
    }

    /// This path with one more segment, if it is safe.
    pub fn join(&self, seg: impl Into<String>) -> Option<RelPath> {
        let mut p = self.clone();
        p.push_checked(seg).ok()?;
        Some(p)
    }

    pub fn segments(&self) -> &[String] {
        &self.0
    }

    pub fn into_segments(self) -> Vec<String> {
        self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("/"))
    }
}

/// One library path as the index answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub eid: i64,
    pub parent: Option<i64>,
    /// The NFC display spelling.
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// The effective mtime (§5.2); folders also move on child changes.
    pub mtime_ns: i128,
    pub cver: Option<String>,
    /// The bytes are a version Bunko wrote (filled from the ledger, task 5).
    pub own: bool,
    /// `None`: local-only (a pending Deferred create).
    pub winner_pid: Option<String>,
    pub pending_op: Option<String>,
    /// Provider nodes behind this entry (merged folders, shadowed duplicates).
    pub members: u32,
    /// The oldest member's creation time (the case-winner age, §5.9).
    pub created_ms: i64,
}

/// What became visible at a library path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexChange {
    Added {
        rel: RelPath,
        is_dir: bool,
    },
    Removed {
        rel: RelPath,
        is_dir: bool,
    },
    /// A rename or move; a folder's subtree moves with it.
    Moved {
        from: RelPath,
        to: RelPath,
        is_dir: bool,
    },
    ContentChanged {
        rel: RelPath,
    },
}

/// A conflict the index sees (the state db's `conflict.kind` vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictKind {
    DupFile,
    FileVsDir,
    NestedRoot,
    /// A provider name that cannot be a path segment (`..`, `a/b`, control chars):
    /// the node and its subtree are not indexed.
    InvalidName,
}

impl ConflictKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictKind::DupFile => "dup-file",
            ConflictKind::FileVsDir => "file-vs-dir",
            ConflictKind::NestedRoot => "nested-root",
            ConflictKind::InvalidName => "invalid-name",
        }
    }
}

/// A node the index hides or demotes, and the path it would have had (for
/// `InvalidName`: its parent's path, the name being unusable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexConflict {
    pub kind: ConflictKind,
    pub pid: Pid,
    pub rel: Option<RelPath>,
    /// `InvalidName`: the name, debug-escaped.
    pub detail: Option<String>,
}

/// What an entry looked like, for change reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snap {
    is_dir: bool,
    winner: Option<String>,
    cver: Option<String>,
}

struct NodeRow {
    parent_pid: Option<String>,
    eid: Option<i64>,
    name_raw: Vec<u8>,
    is_dir: bool,
}

/// The index database.
pub struct Index {
    conn: Connection,
    generation: Cell<u64>,
    granularity_ns: Cell<i64>,
    /// Between `bump_generation` and `sweep_older_than` (a full sync).
    full_sync: Cell<bool>,
    /// Folders created during this full sync: their children's arrival is not a
    /// change, so their listed mtime stands.
    fresh: RefCell<HashSet<i64>>,
}

const ENTRY_COLS: &str = "e.eid, e.parent_eid, e.name, e.kind, e.size, e.mtime_ns, e.cver, e.own, \
     e.winner_pid, e.pending_op, e.members, \
     COALESCE((SELECT MIN(n.created_ms) FROM node n WHERE n.eid = e.eid AND n.role IN (0, 1)), 0)";

fn entry_from_row(r: &Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        eid: r.get(0)?,
        parent: r.get(1)?,
        name: r.get(2)?,
        is_dir: r.get::<_, i64>(3)? == KIND_DIR,
        size: r.get::<_, i64>(4)? as u64,
        mtime_ns: i128::from(r.get::<_, i64>(5)?),
        cver: r.get(6)?,
        own: r.get::<_, i64>(7)? != 0,
        winner_pid: r.get(8)?,
        pending_op: r.get(9)?,
        members: r.get::<_, i64>(10)? as u32,
        created_ms: r.get(11)?,
    })
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

/// Log a read failure and answer "nothing" (reads have no error channel).
fn soft<T>(r: rusqlite::Result<T>, what: &str) -> Option<T> {
    r.map_err(|e| tracing::warn!("index {what}: {e}")).ok()
}

impl Index {
    /// Open (or create) the index at `path`, in WAL mode.
    pub fn open(path: &Path) -> rusqlite::Result<Index> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        let idx = Index {
            conn,
            generation: Cell::new(0),
            granularity_ns: Cell::new(DEFAULT_GRANULARITY_NS),
            full_sync: Cell::new(false),
            fresh: RefCell::new(HashSet::new()),
        };
        if let Some(g) = idx.meta_get("generation").and_then(|v| v.parse().ok()) {
            idx.generation.set(g);
        }
        if let Some(g) = idx
            .meta_get("mtime_granularity_ns")
            .and_then(|v| v.parse().ok())
        {
            idx.granularity_ns.set(g);
        }
        Ok(idx)
    }

    /// Start the transaction that [`upsert`](Self::upsert), [`remove`](Self::remove)
    /// and [`sweep_older_than`](Self::sweep_older_than) run in. Other writes made
    /// while it is open ([`meta_set`](Self::meta_set), the cursor) join it.
    pub fn begin(&self) -> rusqlite::Result<Transaction<'_>> {
        self.conn.unchecked_transaction()
    }

    /// The current full-sync generation (`node.seen_gen` of everything seen in it).
    pub fn generation(&self) -> u64 {
        self.generation.get()
    }

    /// Start a new generation (a full sync): returns it.
    pub fn bump_generation(&self) -> rusqlite::Result<u64> {
        let g = self.generation.get() + 1;
        self.meta_set("generation", &g.to_string())?;
        self.generation.set(g);
        self.full_sync.set(true);
        self.fresh.borrow_mut().clear();
        Ok(g)
    }

    pub fn meta_get(&self, k: &str) -> Option<String> {
        soft(
            self.conn
                .query_row("SELECT v FROM meta WHERE k = ?1", [k], |r| r.get(0))
                .optional(),
            "meta_get",
        )
        .flatten()
    }

    pub fn meta_set(&self, k: &str, v: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO meta(k, v) VALUES(?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![k, v],
        )?;
        Ok(())
    }

    /// The merged root's member pids, oldest first (the first is the target for new
    /// top-level folders and control files).
    pub fn roots(&self) -> Vec<Pid> {
        self.pid_list("roots_json")
    }

    /// Root candidates found inside another root.
    pub fn nested_roots(&self) -> Vec<Pid> {
        self.pid_list("nested_roots_json")
    }

    fn pid_list(&self, key: &str) -> Vec<Pid> {
        self.meta_get(key)
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }

    fn set_pid_list(&self, c: &Connection, key: &str, pids: &[Pid]) -> rusqlite::Result<()> {
        let v = serde_json::to_string(pids).expect("a string list serialises");
        c.execute(
            "INSERT INTO meta(k, v) VALUES(?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, v],
        )?;
        Ok(())
    }

    /// Make `roots` (every folder `locate_roots` found) the merged root (D5): the
    /// oldest-created is the winner; a candidate inside another root, or already in
    /// the tree, is a `nested-root` instead. Former roots not listed are dropped
    /// with their subtrees.
    pub fn set_roots(&self, roots: &[RNode]) -> rusqlite::Result<Vec<IndexChange>> {
        self.savepoint(|c| {
            let mut out = Vec::new();
            let mut cands: Vec<&RNode> = roots.iter().collect();
            cands.sort_by(|a, b| (a.created_ms, &a.pid).cmp(&(b.created_ms, &b.pid)));
            cands.dedup_by(|a, b| a.pid == b.pid);
            // A candidate whose name could not be a path segment is no root at all.
            cands.retain(|r| is_safe_segment(&nfc(&r.name_raw)));
            let pids: HashSet<&str> = cands.iter().map(|r| r.pid.as_str()).collect();
            let mut keep = Vec::new();
            let mut nested = self.nested_roots();
            for r in cands {
                let in_cand = r.parent.as_deref().is_some_and(|p| pids.contains(p));
                let in_tree = self.parent_eid_of(c, r.parent.as_deref())?.is_some();
                let indexed = self
                    .node_row(c, &r.pid)?
                    .and_then(|n| n.eid)
                    .is_some_and(|e| e != ROOT_EID);
                if in_cand || in_tree || indexed {
                    if !nested.contains(&r.pid) {
                        nested.push(r.pid.clone());
                    }
                } else {
                    keep.push(r);
                }
            }
            let keep_pids: Vec<Pid> = keep.iter().map(|r| r.pid.clone()).collect();
            for old in self.roots() {
                if !keep_pids.contains(&old) {
                    self.detach(c, &old, &mut out)?;
                    c.execute("DELETE FROM node WHERE pid = ?1", [&old])?;
                }
            }
            let generation = self.generation.get();
            for r in &keep {
                self.write_node(c, r, generation)?;
                c.execute(
                    "UPDATE node SET eid = ?1 WHERE pid = ?2",
                    params![ROOT_EID, r.pid],
                )?;
            }
            self.recompute(c, ROOT_EID)?;
            for r in &keep {
                self.place_children(c, &r.pid, &mut out)?;
            }
            self.set_pid_list(c, "roots_json", &keep_pids)?;
            self.set_pid_list(c, "nested_roots_json", &nested)?;
            Ok(out)
        })
    }

    /// Ingest one node as seen in generation `generation`. A trashed node is removed.
    pub fn upsert(
        &self,
        tx: &Transaction<'_>,
        n: &RNode,
        generation: u64,
        granularity_ns: i64,
    ) -> rusqlite::Result<Vec<IndexChange>> {
        self.set_granularity(tx, granularity_ns)?;
        if n.trashed {
            return self.remove(tx, &n.pid);
        }
        let c: &Connection = tx;
        let mut out = Vec::new();
        let Some(old) = self.node_row(c, &n.pid)? else {
            self.write_node(c, n, generation)?;
            self.place(c, &n.pid, &mut out)?;
            return Ok(out);
        };
        match old.eid {
            None => {
                self.write_node(c, n, generation)?;
                self.place(c, &n.pid, &mut out)?;
            }
            Some(ROOT_EID) => {
                if self.parent_eid_of(c, n.parent.as_deref())?.is_some() {
                    // A root that turns out to be inside the tree.
                    self.demote_root(c, &n.pid, &mut out)?;
                    self.write_node(c, n, generation)?;
                    self.place(c, &n.pid, &mut out)?;
                } else {
                    self.write_node(c, n, generation)?;
                    self.recompute(c, ROOT_EID)?;
                }
            }
            Some(eid) => {
                let same_place = old.parent_pid == n.parent
                    && old.is_dir == n.is_dir
                    && nfc(&old.name_raw) == nfc(&n.name_raw);
                if same_place {
                    let before = self.snap(c, eid)?;
                    self.write_node(c, n, generation)?;
                    let after = self.recompute(c, eid)?;
                    self.report(c, eid, before, after, &mut out)?;
                } else {
                    self.relocate(c, n, eid, generation, &mut out)?;
                }
            }
        }
        Ok(out)
    }

    /// Forget a node (gone from the provider). Its children stay known but unplaced
    /// until it comes back (an untrash) or a sweep drops them.
    pub fn remove(&self, tx: &Transaction<'_>, pid: &str) -> rusqlite::Result<Vec<IndexChange>> {
        let c: &Connection = tx;
        let mut out = Vec::new();
        let Some(row) = self.node_row(c, pid)? else {
            return Ok(out);
        };
        if row.eid.is_some() {
            self.detach(c, pid, &mut out)?;
        }
        c.execute("DELETE FROM node WHERE pid = ?1", [pid])?;
        if row.eid == Some(ROOT_EID) {
            let roots: Vec<Pid> = self.roots().into_iter().filter(|p| p != pid).collect();
            self.set_pid_list(c, "roots_json", &roots)?;
            self.recompute(c, ROOT_EID)?;
        }
        let nested = self.nested_roots();
        if nested.iter().any(|p| p == pid) {
            let nested: Vec<Pid> = nested.into_iter().filter(|p| p != pid).collect();
            self.set_pid_list(c, "nested_roots_json", &nested)?;
        }
        Ok(out)
    }

    /// End of a full sync: remove every node not seen since `generation`, except the
    /// roots and those `keep` answers true for (ledger-newer, pending ops).
    pub fn sweep_older_than(
        &self,
        tx: &Transaction<'_>,
        generation: u64,
        keep: &dyn Fn(&str) -> bool,
    ) -> rusqlite::Result<Vec<IndexChange>> {
        let c: &Connection = tx;
        let stale: Vec<String> = {
            let mut st = c.prepare(
                "SELECT pid FROM node WHERE seen_gen < ?1 AND (eid IS NULL OR eid != ?2) ORDER BY pid",
            )?;
            st.query_map(params![generation as i64, ROOT_EID], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for pid in stale.iter().filter(|p| !keep(p)) {
            out.extend(self.remove(tx, pid)?);
        }
        self.full_sync.set(false);
        self.fresh.borrow_mut().clear();
        Ok(out)
    }

    /// The entry at `rel`, matching each segment byte-exactly.
    pub fn lookup(&self, rel: &RelPath) -> Option<Entry> {
        let mut eid = ROOT_EID;
        for seg in &rel.0 {
            // RelPath constructors refuse unsafe segments; stay strict regardless.
            if !is_safe_segment(seg) {
                return None;
            }
            eid = soft(
                self.conn
                    .prepare_cached("SELECT eid FROM entry WHERE parent_eid = ?1 AND name = ?2")
                    .and_then(|mut s| s.query_row(params![eid, seg], |r| r.get(0)).optional()),
                "lookup",
            )??;
        }
        self.entry(eid)
    }

    /// One entry by id.
    pub fn entry(&self, eid: i64) -> Option<Entry> {
        let sql = format!("SELECT {ENTRY_COLS} FROM entry e WHERE e.eid = ?1");
        soft(
            self.conn.query_row(&sql, [eid], entry_from_row).optional(),
            "entry",
        )
        .flatten()
    }

    /// The entries directly under `eid`, by name.
    pub fn children(&self, eid: i64) -> Vec<Entry> {
        let sql =
            format!("SELECT {ENTRY_COLS} FROM entry e WHERE e.parent_eid = ?1 ORDER BY e.name");
        self.entries(&sql, params![eid], "children")
    }

    /// The case variants under `parent_eid` whose fold is `fold`, oldest created
    /// first (then smallest winner pid): the first is the case winner (§5.9).
    pub fn variants(&self, parent_eid: i64, fold: &str) -> Vec<Entry> {
        let sql =
            format!("SELECT {ENTRY_COLS} FROM entry e WHERE e.parent_eid = ?1 AND e.fold = ?2");
        let mut v = self.entries(&sql, params![parent_eid, fold], "variants");
        v.sort_by(|a, b| (a.created_ms, &a.winner_pid).cmp(&(b.created_ms, &b.winner_pid)));
        v
    }

    fn entries(&self, sql: &str, p: impl rusqlite::Params, what: &str) -> Vec<Entry> {
        soft(
            self.conn.prepare_cached(sql).and_then(|mut s| {
                s.query_map(p, entry_from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()
            }),
            what,
        )
        .unwrap_or_default()
    }

    /// The entry a node is part of (as winner, member or shadowed duplicate).
    pub fn entry_by_pid(&self, pid: &str) -> Option<Entry> {
        let eid: i64 = soft(
            self.conn
                .query_row("SELECT eid FROM node WHERE pid = ?1", [pid], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .optional(),
            "entry_by_pid",
        )???;
        self.entry(eid)
    }

    /// The path of an entry.
    pub fn path_of(&self, eid: i64) -> Option<RelPath> {
        soft(self.path_c(&self.conn, eid), "path_of").flatten()
    }

    /// The node an entry shows (the merged root: the oldest root).
    pub fn winner_node(&self, eid: i64) -> Option<RNode> {
        soft(
            self.conn
                .query_row(
                    "SELECT pid, parent_pid, name_raw, kind, size, base_mtime_ms, created_ms, version, \
                     cver, pin, hash_kind, hash FROM node WHERE eid = ?1 AND role = 0 \
                     ORDER BY created_ms, pid LIMIT 1",
                    [eid],
                    |r| {
                        let hash_kind: Option<String> = r.get(10)?;
                        let hash: Option<String> = r.get(11)?;
                        Ok(RNode {
                            pid: r.get(0)?,
                            parent: r.get(1)?,
                            name_raw: r.get(2)?,
                            is_dir: r.get::<_, i64>(3)? == KIND_DIR,
                            size: r.get::<_, i64>(4)? as u64,
                            base_mtime_ms: r.get(5)?,
                            created_ms: r.get(6)?,
                            version: r.get(7)?,
                            cver: r.get(8)?,
                            pin: r.get(9)?,
                            hash: hash_kind.as_deref().and_then(HashKind::parse).zip(hash),
                            trashed: false,
                        })
                    },
                )
                .optional(),
            "winner_node",
        )
        .flatten()
    }

    /// Shadowed duplicates, files hidden by a folder, and nested roots.
    pub fn conflicts(&self) -> Vec<IndexConflict> {
        let rows: Vec<(String, i64, i64, i64)> = soft(
            self.conn
                .prepare(
                    "SELECT n.pid, n.eid, n.kind, e.kind FROM node n JOIN entry e ON e.eid = n.eid \
                     WHERE n.role = ?1 ORDER BY n.pid",
                )
                .and_then(|mut s| {
                    s.query_map([ROLE_SHADOWED], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })?
                    .collect()
                }),
            "conflicts",
        )
        .unwrap_or_default();
        let mut out: Vec<IndexConflict> = rows
            .into_iter()
            .map(|(pid, eid, node_kind, entry_kind)| IndexConflict {
                kind: if node_kind == KIND_FILE && entry_kind == KIND_DIR {
                    ConflictKind::FileVsDir
                } else {
                    ConflictKind::DupFile
                },
                pid,
                rel: self.path_of(eid),
                detail: None,
            })
            .collect();
        for pid in self.nested_roots() {
            let rel = self.entry_by_pid(&pid).and_then(|e| self.path_of(e.eid));
            out.push(IndexConflict {
                kind: ConflictKind::NestedRoot,
                pid,
                rel,
                detail: None,
            });
        }
        out.extend(self.invalid_names());
        out
    }

    /// Nodes in a visible folder whose names cannot be a path segment (their
    /// subtrees are unreachable, so only the top of each is reported).
    fn invalid_names(&self) -> Vec<IndexConflict> {
        let rows: Vec<(String, Vec<u8>, i64)> = soft(
            self.conn
                .prepare(
                    "SELECT n.pid, n.name_raw, p.eid FROM node n JOIN node p ON p.pid = n.parent_pid \
                     WHERE n.eid IS NULL AND p.eid IS NOT NULL ORDER BY n.pid",
                )
                .and_then(|mut s| {
                    s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                        .collect()
                }),
            "invalid_names",
        )
        .unwrap_or_default();
        rows.into_iter()
            .filter_map(|(pid, raw, parent_eid)| {
                let name = nfc(&raw);
                (!is_safe_segment(&name)).then(|| IndexConflict {
                    kind: ConflictKind::InvalidName,
                    pid,
                    rel: self.path_of(parent_eid),
                    detail: Some(format!("{name:?}")),
                })
            })
            .collect()
    }

    // ---- internals ----------------------------------------------------------------

    fn savepoint<T>(
        &self,
        f: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        self.conn.execute_batch("SAVEPOINT bunko_index")?;
        match f(&self.conn) {
            Ok(v) => {
                self.conn.execute_batch("RELEASE bunko_index")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self
                    .conn
                    .execute_batch("ROLLBACK TO bunko_index; RELEASE bunko_index");
                Err(e)
            }
        }
    }

    fn set_granularity(&self, c: &Connection, g: i64) -> rusqlite::Result<()> {
        if g > 0 && g != self.granularity_ns.get() {
            c.execute(
                "INSERT INTO meta(k, v) VALUES('mtime_granularity_ns', ?1) \
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                [g.to_string()],
            )?;
            self.granularity_ns.set(g);
        }
        Ok(())
    }

    fn node_row(&self, c: &Connection, pid: &str) -> rusqlite::Result<Option<NodeRow>> {
        c.prepare_cached("SELECT parent_pid, eid, name_raw, kind FROM node WHERE pid = ?1")?
            .query_row([pid], |r| {
                Ok(NodeRow {
                    parent_pid: r.get(0)?,
                    eid: r.get(1)?,
                    name_raw: r.get(2)?,
                    is_dir: r.get::<_, i64>(3)? == KIND_DIR,
                })
            })
            .optional()
    }

    /// Insert or update a node's provider fields (its placement is left alone).
    fn write_node(&self, c: &Connection, n: &RNode, generation: u64) -> rusqlite::Result<()> {
        let (hash_kind, hash) = match &n.hash {
            Some((k, h)) => (Some(k.as_str()), Some(h.as_str())),
            None => (None, None),
        };
        c.prepare_cached(
            "INSERT INTO node(pid, parent_pid, eid, name_raw, kind, size, base_mtime_ms, created_ms, \
             version, cver, pin, hash_kind, hash, role, seen_gen) \
             VALUES(?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13) \
             ON CONFLICT(pid) DO UPDATE SET parent_pid = excluded.parent_pid, name_raw = excluded.name_raw, \
             kind = excluded.kind, size = excluded.size, base_mtime_ms = excluded.base_mtime_ms, \
             created_ms = excluded.created_ms, version = excluded.version, cver = excluded.cver, \
             pin = excluded.pin, hash_kind = excluded.hash_kind, hash = excluded.hash, \
             seen_gen = excluded.seen_gen",
        )?
        .execute(params![
            n.pid,
            n.parent,
            n.name_raw,
            if n.is_dir { KIND_DIR } else { KIND_FILE },
            n.size as i64,
            n.base_mtime_ms,
            n.created_ms,
            n.version,
            n.cver,
            n.pin,
            hash_kind,
            hash,
            generation as i64,
        ])?;
        Ok(())
    }

    fn parent_eid_of(&self, c: &Connection, parent: Option<&str>) -> rusqlite::Result<Option<i64>> {
        let Some(p) = parent else { return Ok(None) };
        Ok(self.node_row(c, p)?.and_then(|n| n.eid))
    }

    fn entry_parent(&self, c: &Connection, eid: i64) -> rusqlite::Result<Option<i64>> {
        c.prepare_cached("SELECT parent_eid FROM entry WHERE eid = ?1")?
            .query_row([eid], |r| r.get(0))
            .optional()
            .map(Option::flatten)
    }

    fn path_c(&self, c: &Connection, mut eid: i64) -> rusqlite::Result<Option<RelPath>> {
        let mut segs = Vec::new();
        while eid != ROOT_EID {
            let row: Option<(Option<i64>, String)> = c
                .prepare_cached("SELECT parent_eid, name FROM entry WHERE eid = ?1")?
                .query_row([eid], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?;
            match row {
                Some((Some(p), name)) => {
                    segs.push(name);
                    eid = p;
                }
                _ => return Ok(None),
            }
        }
        segs.reverse();
        // Entry names are checked at ingest; re-check so no row can yield an unsafe path.
        Ok(RelPath::from_segments(segs))
    }

    fn snap(&self, c: &Connection, eid: i64) -> rusqlite::Result<Option<Snap>> {
        c.prepare_cached("SELECT kind, winner_pid, cver FROM entry WHERE eid = ?1")?
            .query_row([eid], |r| {
                Ok(Snap {
                    is_dir: r.get::<_, i64>(0)? == KIND_DIR,
                    winner: r.get(1)?,
                    cver: r.get(2)?,
                })
            })
            .optional()
    }

    /// Re-derive an entry from its nodes (§5.9 winner rules); delete it when none
    /// are left. Returns what it now looks like.
    fn recompute(&self, c: &Connection, eid: i64) -> rusqlite::Result<Option<Snap>> {
        struct M {
            pid: String,
            is_dir: bool,
            created: i64,
            size: i64,
            base: i64,
            cver: String,
        }
        let mut nodes: Vec<M> = c
            .prepare_cached(
                "SELECT pid, kind, created_ms, size, base_mtime_ms, cver FROM node WHERE eid = ?1",
            )?
            .query_map([eid], |r| {
                Ok(M {
                    pid: r.get(0)?,
                    is_dir: r.get::<_, i64>(1)? == KIND_DIR,
                    created: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    size: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    base: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    cver: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        if nodes.is_empty() {
            let pending: Option<Option<String>> = c
                .query_row("SELECT pending_op FROM entry WHERE eid = ?1", [eid], |r| {
                    r.get(0)
                })
                .optional()?;
            if eid == ROOT_EID || matches!(pending, Some(Some(_))) {
                c.execute("UPDATE entry SET members = 0 WHERE eid = ?1", [eid])?;
                return self.snap(c, eid);
            }
            c.execute("DELETE FROM entry WHERE eid = ?1", [eid])?;
            self.fresh.borrow_mut().remove(&eid);
            return Ok(None);
        }
        let has_dir = nodes.iter().any(|m| m.is_dir);
        if has_dir {
            // Folders first, oldest (then smallest pid) first; files after.
            nodes.sort_by(|a, b| {
                (!a.is_dir, a.created, &a.pid).cmp(&(!b.is_dir, b.created, &b.pid))
            });
        } else {
            // Newest created (then smallest pid) first.
            nodes.sort_by(|a, b| {
                (std::cmp::Reverse(a.created), &a.pid).cmp(&(std::cmp::Reverse(b.created), &b.pid))
            });
        }
        for (i, m) in nodes.iter().enumerate() {
            let role = match (i, m.is_dir) {
                (0, _) => ROLE_WINNER,
                (_, true) => ROLE_MEMBER,
                (_, false) => ROLE_SHADOWED,
            };
            c.prepare_cached("UPDATE node SET role = ?1 WHERE pid = ?2")?
                .execute(params![role, m.pid])?;
        }
        let w = &nodes[0];
        let eff = effective_mtime_ns(w.base, self.granularity_ns.get(), &w.pid, &w.cver) as i64;
        let (prev_kind, prev_mtime): (i64, i64) = c.query_row(
            "SELECT kind, mtime_ns FROM entry WHERE eid = ?1",
            [eid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let (kind, size, cver, mtime) = if has_dir {
            let mtime = if prev_kind == KIND_DIR {
                prev_mtime.max(eff)
            } else {
                eff
            };
            (KIND_DIR, 0, None, mtime)
        } else {
            (KIND_FILE, w.size, Some(w.cver.as_str()), eff)
        };
        c.prepare_cached(
            "UPDATE entry SET kind = ?1, size = ?2, mtime_ns = ?3, cver = ?4, winner_pid = ?5, members = ?6 \
             WHERE eid = ?7",
        )?
        .execute(params![kind, size, mtime, cver, w.pid, nodes.len() as i64, eid])?;
        Ok(Some(Snap {
            is_dir: has_dir,
            winner: Some(w.pid.clone()),
            cver: cver.map(str::to_string),
        }))
    }

    /// Report the visible difference of entry `eid` between `before` and `after`.
    fn report(
        &self,
        c: &Connection,
        eid: i64,
        before: Option<Snap>,
        after: Option<Snap>,
        out: &mut Vec<IndexChange>,
    ) -> rusqlite::Result<()> {
        if eid == ROOT_EID || before == after {
            return Ok(());
        }
        debug_assert!(after.is_some(), "report() on a deleted entry");
        let Some(rel) = self.path_c(c, eid)? else {
            return Ok(());
        };
        let parent = self.entry_parent(c, eid)?;
        self.report_at(c, rel, parent, before, after, out)
    }

    fn report_at(
        &self,
        c: &Connection,
        rel: RelPath,
        parent: Option<i64>,
        before: Option<Snap>,
        after: Option<Snap>,
        out: &mut Vec<IndexChange>,
    ) -> rusqlite::Result<()> {
        let structural = match (&before, &after) {
            (None, Some(a)) => {
                out.push(IndexChange::Added {
                    rel,
                    is_dir: a.is_dir,
                });
                true
            }
            (Some(b), None) => {
                out.push(IndexChange::Removed {
                    rel,
                    is_dir: b.is_dir,
                });
                true
            }
            (Some(b), Some(a)) if b.is_dir != a.is_dir => {
                out.push(IndexChange::Removed {
                    rel: rel.clone(),
                    is_dir: b.is_dir,
                });
                out.push(IndexChange::Added {
                    rel,
                    is_dir: a.is_dir,
                });
                true
            }
            (Some(b), Some(a)) => {
                if !a.is_dir && (b.cver != a.cver || b.winner != a.winner) {
                    out.push(IndexChange::ContentChanged { rel });
                }
                false
            }
            (None, None) => false,
        };
        if structural && let Some(p) = parent {
            self.bump(c, p)?;
        }
        Ok(())
    }

    /// A child of folder `eid` changed: move its mtime like a local directory's.
    fn bump(&self, c: &Connection, eid: i64) -> rusqlite::Result<()> {
        if self.fresh.borrow().contains(&eid) {
            return Ok(());
        }
        c.prepare_cached("UPDATE entry SET mtime_ns = MAX(mtime_ns + ?1, ?2) WHERE eid = ?3")?
            .execute(params![DIR_BUMP_NS, now_ns(), eid])?;
        Ok(())
    }

    /// Give an unplaced node its entry (creating it), then place its unplaced children.
    fn place(&self, c: &Connection, pid: &str, out: &mut Vec<IndexChange>) -> rusqlite::Result<()> {
        let Some(row) = self.node_row(c, pid)? else {
            return Ok(());
        };
        let Some(peid) = self.parent_eid_of(c, row.parent_pid.as_deref())? else {
            return Ok(()); // the parent is not known (yet)
        };
        let name = nfc(&row.name_raw);
        if !is_safe_segment(&name) {
            // Never a path segment: the node (and so its subtree) stays unplaced and
            // `conflicts()` reports it as `invalid-name`.
            tracing::warn!(pid, name = ?name, "cloud name cannot be a library path segment");
            return Ok(());
        }
        let existing: Option<i64> = c
            .prepare_cached("SELECT eid FROM entry WHERE parent_eid = ?1 AND name = ?2")?
            .query_row(params![peid, name], |r| r.get(0))
            .optional()?;
        let (eid, before) = match existing {
            Some(e) => (e, self.snap(c, e)?),
            None => {
                c.prepare_cached(
                    "INSERT INTO entry(parent_eid, name, fold, kind, mtime_ns) VALUES(?1, ?2, ?3, ?4, 0)",
                )?
                .execute(params![peid, name, fold(&name), if row.is_dir { KIND_DIR } else { KIND_FILE }])?;
                let e = c.last_insert_rowid();
                if self.full_sync.get() {
                    self.fresh.borrow_mut().insert(e);
                }
                (e, None)
            }
        };
        c.prepare_cached("UPDATE node SET eid = ?1 WHERE pid = ?2")?
            .execute(params![eid, pid])?;
        let after = self.recompute(c, eid)?;
        self.report(c, eid, before, after, out)?;
        if row.is_dir {
            self.place_children(c, pid, out)?;
        }
        Ok(())
    }

    fn place_children(
        &self,
        c: &Connection,
        pid: &str,
        out: &mut Vec<IndexChange>,
    ) -> rusqlite::Result<()> {
        let kids: Vec<String> = c
            .prepare_cached("SELECT pid FROM node WHERE parent_pid = ?1 AND eid IS NULL ORDER BY created_ms, pid")?
            .query_map([pid], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for k in kids {
            self.place(c, &k, out)?;
        }
        Ok(())
    }

    /// Take a placed node (and, first, its placed descendants) out of its entry.
    fn detach(
        &self,
        c: &Connection,
        pid: &str,
        out: &mut Vec<IndexChange>,
    ) -> rusqlite::Result<()> {
        let Some(eid) = self.node_row(c, pid)?.and_then(|r| r.eid) else {
            return Ok(());
        };
        let kids: Vec<String> = c
            .prepare_cached("SELECT pid FROM node WHERE parent_pid = ?1 AND eid IS NOT NULL ORDER BY created_ms, pid")?
            .query_map([pid], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for k in kids {
            self.detach(c, &k, out)?;
        }
        let rel = self.path_c(c, eid)?;
        let parent = self.entry_parent(c, eid)?;
        let before = self.snap(c, eid)?;
        c.prepare_cached("UPDATE node SET eid = NULL, role = 0 WHERE pid = ?1")?
            .execute([pid])?;
        let after = self.recompute(c, eid)?;
        if eid != ROOT_EID
            && before != after
            && let Some(rel) = rel
        {
            self.report_at(c, rel, parent, before, after, out)?;
        }
        Ok(())
    }

    /// A placed node whose parent or name changed.
    fn relocate(
        &self,
        c: &Connection,
        n: &RNode,
        eid: i64,
        generation: u64,
        out: &mut Vec<IndexChange>,
    ) -> rusqlite::Result<()> {
        let new_parent = self.parent_eid_of(c, n.parent.as_deref())?;
        let name = nfc(&n.name_raw);
        let alone: i64 = c.query_row("SELECT COUNT(*) FROM node WHERE eid = ?1", [eid], |r| {
            r.get(0)
        })?;
        let was_dir = self.snap(c, eid)?.is_some_and(|s| s.is_dir);
        if let Some(np) = new_parent
            && is_safe_segment(&name)
            && alone == 1
            && was_dir == n.is_dir
            && !self.name_used(c, np, &name)?
            && !self.is_within(c, np, eid)?
        {
            // One node, a free destination: rename the entry in place (its subtree follows).
            let from = self.path_c(c, eid)?;
            let old_parent = self.entry_parent(c, eid)?;
            let before = self.snap(c, eid)?;
            self.write_node(c, n, generation)?;
            c.execute(
                "UPDATE entry SET parent_eid = ?1, name = ?2, fold = ?3 WHERE eid = ?4",
                params![np, name, fold(&name), eid],
            )?;
            let after = self.recompute(c, eid)?;
            let to = self.path_c(c, eid)?;
            if let (Some(from), Some(to)) = (from, to) {
                out.push(IndexChange::Moved {
                    from,
                    to: to.clone(),
                    is_dir: n.is_dir,
                });
                if !n.is_dir && before.map(|b| b.cver) != after.map(|a| a.cver) {
                    out.push(IndexChange::ContentChanged { rel: to });
                }
            }
            if let Some(op) = old_parent {
                self.bump(c, op)?;
            }
            if old_parent != Some(np) {
                self.bump(c, np)?;
            }
            return Ok(());
        }
        self.detach(c, &n.pid, out)?;
        self.write_node(c, n, generation)?;
        self.place(c, &n.pid, out)
    }

    fn name_used(&self, c: &Connection, parent: i64, name: &str) -> rusqlite::Result<bool> {
        c.prepare_cached("SELECT 1 FROM entry WHERE parent_eid = ?1 AND name = ?2")?
            .query_row(params![parent, name], |_| Ok(()))
            .optional()
            .map(|r| r.is_some())
    }

    /// `eid` is `anc` or lies under it.
    fn is_within(&self, c: &Connection, mut eid: i64, anc: i64) -> rusqlite::Result<bool> {
        loop {
            if eid == anc {
                return Ok(true);
            }
            match self.entry_parent(c, eid)? {
                Some(p) => eid = p,
                None => return Ok(false),
            }
        }
    }

    /// A root candidate turned out to live inside the tree: make it a nested root.
    fn demote_root(
        &self,
        c: &Connection,
        pid: &str,
        out: &mut Vec<IndexChange>,
    ) -> rusqlite::Result<()> {
        self.detach(c, pid, out)?;
        let roots: Vec<Pid> = self.roots().into_iter().filter(|p| p != pid).collect();
        self.set_pid_list(c, "roots_json", &roots)?;
        let mut nested = self.nested_roots();
        if !nested.iter().any(|p| p == pid) {
            nested.push(pid.to_string());
        }
        self.set_pid_list(c, "nested_roots_json", &nested)?;
        self.recompute(c, ROOT_EID)?;
        Ok(())
    }
}
