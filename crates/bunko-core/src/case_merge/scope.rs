//! What one merge covers ([`Scope`]), the re-plan memo ([`Memo`]) and the
//! recreate-loop guard's limits.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::names::CONTROL_FILES;
use crate::relpath::RelPath;

use super::names::pair_key;

/// After this many finished merges of one losing spelling within [`RECREATE_WINDOW`]
/// (the first and three recreations), a further recreation is not merged: the reader
/// keeps making the folder again, and a `case-merge-blocked` conflict says so.
pub const RECREATE_LIMIT: u64 = 4;
/// The recreate-loop guard's window (24 h).
pub const RECREATE_WINDOW: Duration = Duration::from_secs(24 * 3600);
/// The detail of the recreate-loop guard's conflict.
pub const RECREATING: &str = "Mokuro Reader keeps recreating this folder";
/// How long store events are remembered for the quiet check and the re-plan memo.
pub const EVENT_RETAIN: Duration = Duration::from_secs(3600);

/// The paths one variant set covers, folded: a folder's subtree, or (same-folder file
/// variants) the files of one folded stem, or (a duplicate set, `shallow`) the direct
/// children of one folder, whose nodes are all a duplicate merge moves.
///
/// Journals keep it as JSON ([`Scope::json`]) and compare that string: the fields and
/// their order are a stored format.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub dir: Vec<String>,
    pub key: String,
    pub stem: bool,
    #[serde(default)]
    pub shallow: bool,
}

impl Scope {
    /// A duplicate set's scope: the direct children of the folder at `path` (the
    /// root: `dir` and `key` empty).
    pub fn dup(path: &RelPath) -> Scope {
        Scope {
            dir: folded(&parent_of(path)),
            key: path
                .segments()
                .last()
                .map(|s| pair_key(s))
                .unwrap_or_default(),
            stem: false,
            shallow: true,
        }
    }

    /// `segs` (folded library path segments) is inside this scope.
    pub fn covers(&self, segs: &[String]) -> bool {
        if self.shallow {
            if self.key.is_empty() {
                // Bunko's own status files are written into the canonical root, never
                // moved by a merge: their writes leave the root's set quiet.
                return segs.len() == 1 && !CONTROL_FILES.contains(&segs[0].as_str());
            }
            return segs.len() == self.dir.len() + 2
                && segs[..self.dir.len()] == self.dir[..]
                && segs[self.dir.len()] == self.key;
        }
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

    /// The stored form.
    pub fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// `rel`'s segments, each as [`pair_key`] folds it (a [`Scope`]'s `dir`).
pub fn folded(rel: &RelPath) -> Vec<String> {
    rel.segments().iter().map(|s| pair_key(s)).collect()
}

/// The folder `rel` is in (the root's: the root).
pub fn parent_of(rel: &RelPath) -> RelPath {
    let s = rel.segments();
    RelPath::from_segments(s[..s.len().saturating_sub(1)].iter().cloned()).unwrap_or_default()
}

/// `rel`'s last segment (the root's: empty).
pub fn last_name(rel: &RelPath) -> &str {
    rel.segments().last().map_or("", String::as_str)
}

/// Why a scope is not planned again yet.
#[derive(Debug, Clone)]
pub struct Memo {
    /// As of when: a store event in the scope after this plans it again.
    pub at: Instant,
    /// Until when (`at` + [`EVENT_RETAIN`]: events are only remembered that long).
    pub until: Instant,
    /// The plan ([`super::Plan::fingerprint`]) of a group that made no progress.
    /// Planned the same again, it would make none again: it journals nothing, and is
    /// memoized anew.
    pub idle: Option<String>,
}

impl Memo {
    pub fn new(at: Instant, idle: Option<String>) -> Memo {
        Memo {
            at,
            until: at + EVENT_RETAIN,
            idle,
        }
    }
}
