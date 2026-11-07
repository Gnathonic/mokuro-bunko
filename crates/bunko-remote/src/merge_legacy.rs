//! Test builds only: the case-merge planner as it read before its extraction into
//! [`bunko_core::case_merge`], kept verbatim as the gate that the move changed
//! nothing (Track A, step A1).
//!
//! Every variant set a test build plans is planned twice: by the shared planner
//! over [`RemoteHost`](super::RemoteHost), whose answers (listings, identity checks, reads, the
//! policy's `prefer_main`) are recorded on a [`Tape`], and by this code, replayed
//! from that tape, so it sees exactly what the new path saw and makes no provider
//! call. The two plans must have the same [`Plan::fingerprint`] and the same steps in
//! the same order, field by field; the winner order ([`set_paths_in`]) and the
//! index lookups ([`check_spec`]) are compared under one index lock, and every
//! duplicate plan's fingerprint is computed both ways. Any difference stops the test
//! process with both plans printed, so the store's own suites (`tests/case_merge.rs`,
//! `tests/dedup.rs`, the server's `cloud_case_merge.rs`) are the regression test.
//! [`dual_plan_report`] counts the comparisons; with `BUNKO_DUAL_PLAN_LOG` set, each
//! one appends a line to that file.
//!
//! The first deliberate rule change (A6, or Track B's fold) removes this module.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bunko_core::case_merge::{self as core_merge, Defer, SetSpec, StepKind, defer, last_name};
use bunko_core::sidecar_names::{self, SidecarClass, strip_suffix_ci as strip_ci};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use super::{CaseMergePolicy, MergeChoice, VariantSet};
use crate::backend::{Backend, Pid, RNode};
use crate::index::{Index, ROOT_EID, RelPath};
use crate::names::{fold, nfc};
use crate::store::RemoteStore;

// ---- the record --------------------------------------------------------------------

/// The answers to one question, in the order they were given.
type Answers<K, T> = Mutex<HashMap<K, VecDeque<Result<T, Defer>>>>;

/// What the shared planner was answered, in order per question.
#[derive(Default)]
pub(crate) struct Tape {
    lists: Answers<String, Vec<RNode>>,
    same: Answers<[String; 4], bool>,
    reads: Answers<[String; 2], Vec<u8>>,
    mains: Mutex<HashMap<(PathBuf, PathBuf), VecDeque<MergeChoice>>>,
    /// A question the old code asked that the new path never did.
    misses: Mutex<Vec<String>>,
}

fn pair_of(a: &RNode, b: &RNode) -> [String; 4] {
    [a.pid.clone(), a.cver.clone(), b.pid.clone(), b.cver.clone()]
}

impl Tape {
    pub(crate) fn listed(&self, id: &str, r: &Result<Vec<RNode>, Defer>) {
        self.lists
            .lock()
            .entry(id.to_string())
            .or_default()
            .push_back(r.clone());
    }

    pub(crate) fn compared(&self, a: &RNode, b: &RNode, r: &Result<bool, Defer>) {
        self.same
            .lock()
            .entry(pair_of(a, b))
            .or_default()
            .push_back(r.clone());
    }

    pub(crate) fn read(&self, n: &RNode, r: &Result<Vec<u8>, Defer>) {
        self.reads
            .lock()
            .entry([n.pid.clone(), n.cver.clone()])
            .or_default()
            .push_back(r.clone());
    }

    fn miss<T>(&self, what: String) -> Result<T, Defer> {
        self.misses.lock().push(what.clone());
        Err(format!("not on the tape: {what}"))
    }

    fn list(&self, id: &str) -> Result<Vec<RNode>, Defer> {
        let next = self.lists.lock().get_mut(id).and_then(VecDeque::pop_front);
        next.unwrap_or_else(|| self.miss(format!("list {id}")))
    }

    fn identical(&self, a: &RNode, b: &RNode) -> Result<bool, Defer> {
        let next = self
            .same
            .lock()
            .get_mut(&pair_of(a, b))
            .and_then(VecDeque::pop_front);
        next.unwrap_or_else(|| self.miss(format!("identical {} {}", a.pid, b.pid)))
    }

    fn bytes_of(&self, n: &RNode) -> Result<Vec<u8>, Defer> {
        let next = self
            .reads
            .lock()
            .get_mut(&[n.pid.clone(), n.cver.clone()])
            .and_then(VecDeque::pop_front);
        next.unwrap_or_else(|| self.miss(format!("read {}", n.pid)))
    }

    /// Answers recorded and never asked again (the new path asked more).
    fn left(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (k, q) in self.lists.lock().iter() {
            out.extend(q.iter().map(|_| format!("list {k}")));
        }
        for (k, q) in self.same.lock().iter() {
            out.extend(q.iter().map(|_| format!("identical {} {}", k[0], k[2])));
        }
        for (k, q) in self.reads.lock().iter() {
            out.extend(q.iter().map(|_| format!("read {}", k[0])));
        }
        for (k, q) in self.mains.lock().iter() {
            out.extend(q.iter().map(|_| format!("prefer_main {k:?}")));
        }
        out
    }
}

/// The server's policy, its `prefer_main` answers recorded.
pub(crate) struct Recording<'a> {
    pub(crate) inner: &'a dyn CaseMergePolicy,
    pub(crate) tape: &'a Tape,
}

impl CaseMergePolicy for Recording<'_> {
    fn prefer_main(&self, a: &Path, b: &Path) -> MergeChoice {
        let c = self.inner.prefer_main(a, b);
        self.tape
            .mains
            .lock()
            .entry((a.to_path_buf(), b.to_path_buf()))
            .or_default()
            .push_back(c);
        c
    }
    fn is_unstamped_layer(&self, id: &str) -> bool {
        self.inner.is_unstamped_layer(id)
    }
    fn excluded(&self, dir: &[String], key: &str) -> bool {
        self.inner.excluded(dir, key)
    }
    fn busy(&self, path: &Path) -> bool {
        self.inner.busy(path)
    }
    fn audit(&self, merge: &core_merge::MergeRecord) {
        self.inner.audit(merge);
    }
}

/// The server's policy, its `prefer_main` answers replayed.
struct Replay<'a> {
    inner: &'a dyn CaseMergePolicy,
    tape: &'a Tape,
}

impl CaseMergePolicy for Replay<'_> {
    fn prefer_main(&self, a: &Path, b: &Path) -> MergeChoice {
        let next = self
            .tape
            .mains
            .lock()
            .get_mut(&(a.to_path_buf(), b.to_path_buf()))
            .and_then(VecDeque::pop_front);
        next.unwrap_or_else(|| {
            self.tape
                .misses
                .lock()
                .push(format!("prefer_main {a:?} {b:?}"));
            MergeChoice::Block
        })
    }
    fn is_unstamped_layer(&self, id: &str) -> bool {
        self.inner.is_unstamped_layer(id)
    }
}

// ---- the comparison ------------------------------------------------------------------

static SETS: AtomicU64 = AtomicU64::new(0);
static ORDERS: AtomicU64 = AtomicU64::new(0);
static SPECS: AtomicU64 = AtomicU64::new(0);
static DUPS: AtomicU64 = AtomicU64::new(0);

/// How many comparisons this process made (all of them equal: a difference stops it).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DualPlanReport {
    /// Variant-set plans, planned both ways.
    pub sets: u64,
    /// Winner orders (the old `set_paths`).
    pub orders: u64,
    /// Index lookups behind a set's plan (as the old `plan_set` looked them up).
    pub specs: u64,
    /// Duplicate-merge plans, fingerprinted both ways.
    pub dups: u64,
}

/// The comparisons so far in this process.
pub fn dual_plan_report() -> DualPlanReport {
    DualPlanReport {
        sets: SETS.load(Ordering::Relaxed),
        orders: ORDERS.load(Ordering::Relaxed),
        specs: SPECS.load(Ordering::Relaxed),
        dups: DUPS.load(Ordering::Relaxed),
    }
}

fn log(line: &str) {
    if let Some(p) = std::env::var_os("BUNKO_DUAL_PLAN_LOG")
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
    {
        // One write per line: test processes append to it at once.
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}

/// A difference: say what, then stop the process (a panic on the merge thread would
/// only stop merging, and a test would time out instead of failing).
fn differ(what: &str, new: &str, old: &str) -> ! {
    let msg = format!(
        "case merge extraction: the shared planner and the old code differ ({what})\n\
         --- shared planner ---\n{new}\n--- old code ---\n{old}"
    );
    log(&format!("MISMATCH {what}"));
    eprintln!("{msg}");
    std::process::abort();
}

fn opt<T: std::fmt::Display>(v: &Option<T>) -> String {
    v.as_ref()
        .map_or_else(|| "-".to_string(), ToString::to_string)
}

fn bytes_sha(b: &Option<Vec<u8>>) -> String {
    b.as_ref()
        .map_or_else(|| "-".into(), |b| hex::encode(Sha256::digest(b)))
}

fn taken_lines(t: &HashMap<String, HashSet<String>>) -> String {
    let sorted: BTreeMap<&String, Vec<&String>> = t
        .iter()
        .map(|(k, v)| {
            let mut v: Vec<&String> = v.iter().collect();
            v.sort();
            (k, v)
        })
        .collect();
    format!("taken {sorted:?}")
}

/// Every field of the new plan, in the order it runs.
fn dump_new(p: &core_merge::Plan) -> String {
    let mut out = vec![format!("print {}", p.fingerprint())];
    for (phase, steps) in [
        ("pre_trash", &p.pre_trash),
        ("step", &p.steps),
        ("submit", &p.submits),
        ("l_trash", &p.l_trash),
        ("finish", &p.finishes),
    ] {
        for s in steps {
            out.push(format!(
                "{phase} {:?} vg={} id={} ver={} src={} dest={} parent={} dir={} hooks={} \
                 bytes={} series={} dup={}",
                s.kind,
                s.vg,
                s.id,
                opt(&s.ver),
                opt(&s.src),
                s.dest,
                opt(&s.parent_id),
                s.is_dir,
                s.hooks,
                bytes_sha(&s.bytes),
                opt(&s.series),
                s.expect_dup
            ));
        }
    }
    for b in &p.blocks {
        out.push(format!("block {:?} {:?} {}", b.this, b.that, b.detail));
    }
    out.push(format!("empty_loser {}", p.empty_loser));
    out.push(taken_lines(&p.taken));
    out.join("\n")
}

/// Every field of the old plan, in the same form.
fn dump_old(p: &Plan) -> String {
    let mut out = vec![format!("print {}", p.fingerprint())];
    for (phase, steps) in [
        ("pre_trash", &p.pre_trash),
        ("step", &p.steps),
        ("submit", &p.submits),
        ("l_trash", &p.l_trash),
        ("finish", &p.finishes),
    ] {
        for s in steps {
            out.push(format!(
                "{phase} {:?} vg={} id={} ver={} src={} dest={} parent={} dir={} hooks={} \
                 bytes={} series={} dup={}",
                s.kind,
                s.vg,
                s.pid,
                opt(&s.cver),
                opt(&s.src),
                s.dest,
                opt(&s.parent_pid),
                s.is_dir,
                s.hooks,
                bytes_sha(&s.bytes),
                opt(&s.series),
                s.expect_dup
            ));
        }
    }
    for b in &p.blocks {
        out.push(format!("block {:?} {:?} {}", b.this, b.that, b.detail));
    }
    out.push(format!("empty_loser {}", p.empty_loser));
    out.push(taken_lines(&p.taken));
    out.join("\n")
}

fn dump_result<T>(r: &Result<T, Defer>, dump: impl Fn(&T) -> String) -> String {
    match r {
        Ok(p) => dump(p),
        Err(e) => format!("deferred: {e}"),
    }
}

/// Plan `set` the old way from `tape` and hold `new` to it.
pub(crate) fn check_set<B: Backend>(
    store: &RemoteStore<B>,
    set: &VariantSet,
    spec: &SetSpec,
    tape: &Tape,
    policy: &dyn CaseMergePolicy,
    new: &Result<core_merge::Plan, Defer>,
) {
    let old = Old { store, tape }.plan_set(
        set,
        spec,
        &Replay {
            inner: policy,
            tape,
        },
    );
    let (n, o) = (dump_result(new, dump_new), dump_result(&old, dump_old));
    let misses = std::mem::take(&mut *tape.misses.lock());
    if !misses.is_empty() {
        differ(&format!("the old code asked {misses:?}"), &n, &o);
    }
    // A deferred plan stops at its first failed answer: the rest of the tape is
    // not asked by either.
    if new.is_ok() {
        let left = tape.left();
        if !left.is_empty() {
            differ(&format!("the old code never asked {left:?}"), &n, &o);
        }
    }
    if n != o {
        differ("the plans", &n, &o);
    }
    SETS.fetch_add(1, Ordering::Relaxed);
    log(&format!("set {}", n.lines().next().unwrap_or_default()));
}

/// The winner order the old `set_paths` gave, from the same index state.
pub(crate) fn check_set_paths(idx: &Index, set: &VariantSet, new: &(RelPath, Vec<RelPath>)) {
    let old = set_paths_in(idx, set);
    if *new != old {
        differ("the winner order", &format!("{new:?}"), &format!("{old:?}"));
    }
    ORDERS.fetch_add(1, Ordering::Relaxed);
}

/// The index lookups the old `plan_set` made, from the same index state.
pub(crate) fn check_spec(idx: &Index, set: &VariantSet, spec: &SetSpec) {
    let members = |rel: &RelPath| -> Vec<Pid> {
        idx.lookup(rel)
            .map(|e| idx.members(e.eid).into_iter().map(|n| n.pid).collect())
            .unwrap_or_default()
    };
    let old = if set.scope.stem {
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
        format!("{members:?} {target:?}")
    } else {
        let w_members = members(&spec.winner.0);
        let files: Vec<RelPath> = idx
            .variants(set.parent_eid, &set.scope.key)
            .into_iter()
            .filter(|e| !e.is_dir)
            .filter_map(|e| set.parent.join(e.name))
            .collect();
        let losers: Vec<(RelPath, Vec<Pid>)> = spec
            .losers
            .iter()
            .map(|(l, _)| (l.clone(), members(l)))
            .collect();
        format!("{w_members:?} {files:?} {losers:?}")
    };
    let new = if set.scope.stem {
        format!("{:?} {:?}", spec.parent_ids, spec.target)
    } else {
        format!("{:?} {:?} {:?}", spec.winner.1, spec.files_vs, spec.losers)
    };
    if new != old {
        differ("the index lookups", &new, &old);
    }
    SPECS.fetch_add(1, Ordering::Relaxed);
}

/// A duplicate plan (planned by the store, in the shared types): its fingerprint by
/// the old function.
pub(crate) fn check_dup(p: &core_merge::Plan) {
    let conv = |v: &[core_merge::Planned]| -> Vec<Planned> {
        v.iter()
            .map(|s| Planned {
                kind: s.kind,
                vg: s.vg.clone(),
                pid: s.id.clone(),
                cver: s.ver.clone(),
                src: s.src.clone(),
                dest: s.dest.clone(),
                parent_pid: s.parent_id.clone(),
                is_dir: s.is_dir,
                hooks: s.hooks,
                bytes: s.bytes.clone(),
                series: s.series.clone(),
                expect_dup: s.expect_dup,
            })
            .collect()
    };
    let old = Plan {
        pre_trash: conv(&p.pre_trash),
        steps: conv(&p.steps),
        submits: conv(&p.submits),
        l_trash: conv(&p.l_trash),
        finishes: conv(&p.finishes),
        blocks: p
            .blocks
            .iter()
            .map(|b| Block {
                this: b.this.clone(),
                that: b.that.clone(),
                detail: b.detail.clone(),
            })
            .collect(),
        taken: p.taken.clone(),
        empty_loser: p.empty_loser,
    };
    let (n, o) = (dump_new(p), dump_old(&old));
    if n != o {
        differ("a duplicate plan's fingerprint", &n, &o);
    }
    DUPS.fetch_add(1, Ordering::Relaxed);
    log(&format!("dup {}", n.lines().next().unwrap_or_default()));
}

// ---- the old code ----------------------------------------------------------------------
// Verbatim from `merge.rs` before the extraction, except: the index lookups of
// `plan_set` read the `SetSpec` (held to the old lookups by `check_spec`), the
// provider answers come from the tape, and `set_paths` is a function of the index.

/// `curated` of `curated-3`; `id` itself when it has no `-<n>` counter.
fn strip_counter(id: &str) -> &str {
    match id.rsplit_once('-') {
        Some((b, n)) if !b.is_empty() && !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()) => {
            b
        }
        _ => id,
    }
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
    expect_dup: bool,
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
            expect_dup: false,
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
            expect_dup: false,
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

/// A merge's steps, in the order they run: the older copies a duplicate merge
/// replaces first; moves and in-place replaces next; loser files are trashed only
/// after every move (a duplicate a move meets stops all trashing); emptied folders
/// last, deepest first.
#[derive(Default)]
struct Plan {
    /// Duplicate merge: the canonical folder's copy a newer one replaces, trashed
    /// before that one moves in.
    pre_trash: Vec<Planned>,
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
    /// What the plan would do, as one string: the same steps on the same versions, and
    /// the same blocks, give the same fingerprint, in whatever order the listings that
    /// planned them came back (Drive's `files.list` order is unspecified). Each step
    /// counts with its phase, so a trash before the moves is not one after them.
    fn fingerprint(&self) -> String {
        let phases: [(&str, &[Planned]); 5] = [
            ("pre_trash", &self.pre_trash),
            ("step", &self.steps),
            ("submit", &self.submits),
            ("l_trash", &self.l_trash),
            ("finish", &self.finishes),
        ];
        let mut lines: Vec<String> = phases
            .iter()
            .flat_map(|(phase, steps)| {
                steps.iter().map(move |p| {
                    format!(
                        "{phase}\0{:?}\0{}\0{:?}\0{:?}\0{}\0{:?}",
                        p.kind,
                        p.pid,
                        p.cver,
                        p.src.as_ref().map(ToString::to_string),
                        p.dest,
                        p.parent_pid
                    )
                })
            })
            .chain(self.blocks.iter().map(|b| {
                let sorted = |v: &[RelPath]| {
                    let mut v: Vec<String> = v.iter().map(ToString::to_string).collect();
                    v.sort_unstable();
                    v
                };
                format!(
                    "block\0{:?}\0{:?}\0{}",
                    sorted(&b.this),
                    sorted(&b.that),
                    b.detail
                )
            }))
            .collect();
        lines.sort_unstable();
        let mut h = Sha256::new();
        for l in &lines {
            h.update(l.as_bytes());
            h.update(b"\n");
        }
        h.update([u8::from(self.empty_loser)]);
        hex::encode(h.finalize())
    }
}

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

/// The folded stems of the `.cbz` archives among `names`: only they anchor sidecars
/// (rule v1). A `.zip` is still an [`Part::Archive`] for grouping moves.
fn archive_stems<'a>(names: impl Iterator<Item = &'a str>) -> HashSet<String> {
    sidecar_names::volume_stems(names)
}

/// `(stem, suffix, part)` of a file name, given the folder's archive stems.
fn split_name(name: &str, stems: &HashSet<String>) -> (String, String, Part) {
    if let Some(s) = strip_ci(name, ".cbz").or_else(|| strip_ci(name, ".zip")) {
        return (s.to_string(), name[s.len()..].to_string(), Part::Archive);
    }
    if let Some((base, _)) = sidecar_names::split_mokuro(name) {
        // The listing is at hand, so rule v1 decides: all-digit ids are layers.
        if let SidecarClass::Layer { stem, id, .. } = sidecar_names::classify(name, stems) {
            return (
                stem.to_string(),
                name[stem.len()..].to_string(),
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

fn set_paths_in(idx: &Index, set: &VariantSet) -> (RelPath, Vec<RelPath>) {
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

/// The old planner, over the tape.
struct Old<'a, B: Backend> {
    store: &'a RemoteStore<B>,
    tape: &'a Tape,
}

impl<B: Backend> Old<'_, B> {
    fn list(&self, pid: &str) -> Result<Vec<RNode>, Defer> {
        self.tape.list(pid)
    }

    fn identical(&self, a: &RNode, b: &RNode) -> Result<bool, Defer> {
        self.tape.identical(a, b)
    }

    fn bytes_of(&self, n: &RNode) -> Result<Vec<u8>, Defer> {
        self.tape.bytes_of(n)
    }

    fn plan_set(
        &self,
        set: &VariantSet,
        spec: &SetSpec,
        policy: &dyn CaseMergePolicy,
    ) -> Result<Plan, Defer> {
        let mut plan = Plan::default();
        // The index's answers as the new path had them (`check_spec` holds them to
        // the old lookups), so a feed batch in between is no difference.
        let winner = spec.winner.0.clone();
        let losers: Vec<RelPath> = spec.losers.iter().map(|(l, _)| l.clone()).collect();
        if set.scope.stem {
            // Same-folder file variants: one folder, renames.
            let (members, target) = (spec.parent_ids.clone(), spec.target.clone());
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
        let (w_members, files_vs) = (spec.winner.1.clone(), spec.files_vs.clone());
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
            let l_members: Vec<Pid> = spec
                .losers
                .iter()
                .find(|(p, _)| p == l)
                .map(|(_, m)| m.clone())
                .unwrap_or_default();
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
                expect_dup: false,
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
            series_dir
                && n.part == Part::Other
                && bunko_core::names::is_fixed_name(&n.name, "series.json")
        };
        let wstem = wg
            .iter()
            .find(|n| n.part == Part::Archive)
            .or_else(|| wg.first())
            .map(|n| n.stem.clone());
        let mut lsorted: Vec<&Named> = lg.iter().collect();
        lsorted.sort_by_key(|n| n.part != Part::Archive);
        let (mut moves, mut l_trash, mut submits) = (Vec::new(), Vec::new(), Vec::new());
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
                    expect_dup: false,
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
                    let choice = policy
                        .prefer_main(&self.store.core.abs(&wrel), &self.store.core.abs(&lrel));
                    if choice == MergeChoice::Block {
                        block(
                            plan,
                            format!(
                                "{lrel} and {wrel} hold different OCR, and neither (or both) \
                                 matches the primary OCR setting"
                            ),
                        );
                        return Ok(());
                    }
                    // No copy is kept (owner decision, 2026-11-01). A: the loser's main
                    // is trashed below, like any other superseded file. B: its bytes
                    // replace the winner's in place and the loser's node is trashed;
                    // where the provider's replace makes a new node, the winner's old
                    // node goes to the trash too ([`Self::replaced`]).
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
                            expect_dup: false,
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
        plan.steps.extend(moves);
        plan.submits.extend(submits);
        plan.l_trash.extend(l_trash);
        Ok(())
    }
}
