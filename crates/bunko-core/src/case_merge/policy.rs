//! What a case merge asks the server ([`CaseMergePolicy`]), and what it reports
//! ([`MergeRecord`]).

use std::path::Path;

use serde::Serialize;

use crate::sidecar_names::{self, LayerClass};

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

/// What a case merge needs from the server (installed on a store; no merge runs
/// before).
pub trait CaseMergePolicy: Send + Sync {
    /// Two differing main sidecars beside identical archives (`a` the winner's, `b`
    /// the loser's, both store paths): the one whose recipe matches the primary
    /// row's output-affecting recipe (`Recipe::matches`, not `Verdict::Current`).
    /// `Block` when `ocr.upgrade.enabled` is off, or neither or both match.
    fn prefer_main(&self, a: &Path, b: &Path) -> MergeChoice;
    /// A layer id Bunko never stamps (`edit*`, `curated*`): on a collision with
    /// different content it moves in under the next free id (`-2`, `-3`).
    fn is_unstamped_layer(&self, id: &str) -> bool {
        is_unstamped_layer_id(id)
    }
    /// A variant set the merge leaves alone: `dir` is its parent folder as spelled
    /// (store segments), `key` the folded name the variants share. The server keeps
    /// its per-user tree (a cloud root owner's `history/`) out of case merges.
    fn excluded(&self, _dir: &[String], _key: &str) -> bool {
        false
    }
    /// An OCR claim is running or spooling, or an upgrade is being judged or
    /// swapped, for the volume at `path` or any volume under it.
    fn busy(&self, _path: &Path) -> bool {
        false
    }
    /// A merge group finished (the audit record, as it reads once `done`). Called
    /// before the group shows `done`; a crash in between calls it again on resume.
    fn audit(&self, _merge: &MergeRecord) {}
}

/// `edit`, `curated` and their `-<n>` copies: layers whose content shows a human,
/// never Bunko's model output (D4, D27). Exact-case, like every layer id (sidecar
/// names rule v1).
pub fn is_unstamped_layer_id(id: &str) -> bool {
    matches!(
        sidecar_names::layer_class(strip_counter(id)),
        LayerClass::Personal | LayerClass::Curated
    )
}

/// `curated` of `curated-3`; `id` itself when it has no `-<n>` counter.
pub fn strip_counter(id: &str) -> &str {
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
