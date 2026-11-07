//! What a merge plans: [`Plan`], its [`Planned`] steps and [`Block`]s, and the
//! [`Node`]s it plans over.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::relpath::RelPath;

/// A plan cannot be made now (a read failed, a transfer quota): try again later.
pub type Defer = String;

/// A [`Defer`] from any error.
pub fn defer(e: impl std::fmt::Display) -> Defer {
    e.to_string()
}

/// One listed child, as a store's [`super::Host`] answers it.
#[derive(Debug, Clone)]
pub struct Node<X> {
    /// The store's id for it (a provider pid; locally, the library-relative path).
    pub id: String,
    /// Its NFC name.
    pub name: String,
    pub is_dir: bool,
    /// Unix ms (0 where the store does not know).
    pub created_ms: i64,
    /// The version the plan judges (a provider's content version); a step on
    /// another version is skipped.
    pub ver: String,
    pub size: u64,
    /// The store's own facts about it.
    pub x: X,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    /// Move a loser node into the winner's target node.
    Move,
    /// Trash a file the merge superseded (identical, kept, ingested, a superseded
    /// main sidecar).
    Trash,
    /// Replace the winner's main sidecar in place with the loser's bytes (a new
    /// version based on the judged one); a provider whose replace makes a new node
    /// has the old one trashed after the commit.
    Replace,
    /// Ingest a loser `series.json` as a submission, then trash it once applied.
    Submit,
    /// Trash an emptied losing member folder (after a direct listing shows it empty).
    Finish,
}

/// A step's own fields, as a journal keeps them (the remote store: the outbox row's
/// `resume` column).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extra {
    pub step: StepKind,
    /// The volume group (a block skips the rest of it).
    pub vg: String,
    #[serde(default)]
    pub is_dir: bool,
    /// The version the plan judged (a trash of another version is skipped).
    #[serde(default)]
    pub cver: Option<String>,
    /// Run the DB hooks for this step.
    #[serde(default)]
    pub hooks: bool,
    /// `Submit`: the winner's `series.json` path the submission is for.
    #[serde(default)]
    pub series: Option<String>,
    /// A duplicate merge's move beside a node of the same name (a folder, a keep-both
    /// file): the name is meant to be there twice afterwards.
    #[serde(default)]
    pub expect_dup: bool,
}

/// One planned step.
#[derive(Debug, Clone)]
pub struct Planned {
    pub kind: StepKind,
    pub vg: String,
    /// The node the step works on.
    pub id: String,
    /// Its judged version.
    pub ver: Option<String>,
    pub src: Option<RelPath>,
    pub dest: RelPath,
    /// The node a move lands in.
    pub parent_id: Option<String>,
    pub is_dir: bool,
    pub hooks: bool,
    /// A replace's new bytes (where the store uploads them).
    pub bytes: Option<Vec<u8>>,
    pub series: Option<RelPath>,
    pub expect_dup: bool,
}

impl Planned {
    /// Move `n` from `src` to `dest`, into the node `target`.
    pub fn mv<X>(vg: &str, n: &Node<X>, src: RelPath, dest: RelPath, target: &str) -> Planned {
        Planned {
            kind: StepKind::Move,
            vg: vg.to_string(),
            id: n.id.clone(),
            ver: Some(n.ver.clone()),
            src: Some(src),
            dest,
            parent_id: Some(target.to_string()),
            is_dir: n.is_dir,
            hooks: true,
            bytes: None,
            series: None,
            expect_dup: false,
        }
    }

    /// Trash `n`, at `path`.
    pub fn trash<X>(vg: &str, n: &Node<X>, path: RelPath) -> Planned {
        Planned {
            kind: StepKind::Trash,
            vg: vg.to_string(),
            id: n.id.clone(),
            ver: Some(n.ver.clone()),
            src: None,
            dest: path,
            parent_id: None,
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
pub struct Block {
    pub this: Vec<RelPath>,
    pub that: Vec<RelPath>,
    pub detail: String,
}

/// A merge's steps, in the order they run: the older copies a duplicate merge
/// replaces first; moves and in-place replaces next; loser files are trashed only
/// after every move (a duplicate a move meets stops all trashing); emptied folders
/// last, deepest first.
#[derive(Debug, Default)]
pub struct Plan {
    /// Duplicate merge: the canonical folder's copy a newer one replaces, trashed
    /// before that one moves in.
    pub pre_trash: Vec<Planned>,
    pub steps: Vec<Planned>,
    pub submits: Vec<Planned>,
    pub l_trash: Vec<Planned>,
    pub finishes: Vec<Planned>,
    pub blocks: Vec<Block>,
    /// Names planned into each target node (moves and layers), as
    /// [`super::taken_key`] folds them.
    pub taken: HashMap<String, HashSet<String>>,
    /// A losing member listed empty: its finish is work of its own.
    pub empty_loser: bool,
}

impl Plan {
    /// Anything to run (blocks alone are not).
    pub fn work(&self) -> bool {
        !(self.pre_trash.is_empty()
            && self.steps.is_empty()
            && self.submits.is_empty()
            && self.l_trash.is_empty()
            && !self.empty_loser)
    }

    /// What the plan would do, as one string: the same steps on the same versions, and
    /// the same blocks, give the same fingerprint, in whatever order the listings that
    /// planned them came back (Drive's `files.list` order is unspecified). Each step
    /// counts with its phase, so a trash before the moves is not one after them.
    pub fn fingerprint(&self) -> String {
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
                        p.id,
                        p.ver,
                        p.src.as_ref().map(ToString::to_string),
                        p.dest,
                        p.parent_id
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

    /// Every step, in the order it runs.
    pub fn ordered(&self) -> impl Iterator<Item = &Planned> {
        self.pre_trash
            .iter()
            .chain(&self.steps)
            .chain(&self.submits)
            .chain(&self.l_trash)
            .chain(&self.finishes)
    }
}
