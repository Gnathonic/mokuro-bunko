//! The change feed's persisted snapshot (`<state_dir>/folder-snapshot.json`): the same
//! design as the WebDAV driver's. A cursor is `<epoch>:<generation>`; `base` is the
//! generation the caller holds, `next` the one the last `changes` call returned, kept
//! until the caller moves on to it (delivery is at least once).

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::backend::{Cursor, Pid};

pub(crate) const SNAPSHOT_FILE: &str = "folder-snapshot.json";

/// pid → (version, is_dir), the root (`""`) included.
pub(crate) type Entries = BTreeMap<Pid, (String, bool)>;

/// One generation of the snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Generation {
    pub gen_no: u64,
    pub entries: Entries,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SnapFile {
    /// Changes whenever a new feed starts, so cursors of an older one never match.
    pub epoch: String,
    pub base: Generation,
    /// The generation the last `changes` call returned, until the caller uses it.
    pub next: Option<Generation>,
}

pub(crate) fn load(state_dir: &Path) -> Option<SnapFile> {
    let data = std::fs::read(state_dir.join(SNAPSHOT_FILE)).ok()?;
    serde_json::from_slice(&data).ok()
}

pub(crate) fn save(state_dir: &Path, snap: &SnapFile) -> std::io::Result<()> {
    let data = serde_json::to_vec(snap).map_err(std::io::Error::other)?;
    std::fs::create_dir_all(state_dir)?;
    let tmp = state_dir.join(format!("{SNAPSHOT_FILE}.tmp"));
    std::fs::write(&tmp, &data)?;
    std::fs::rename(&tmp, state_dir.join(SNAPSHOT_FILE))
}

pub(crate) fn cursor_of(epoch: &str, gen_no: u64) -> Cursor {
    Cursor(format!("{epoch}:{gen_no}"))
}

/// `(epoch, generation)` of a cursor this driver issued.
pub(crate) fn parse_cursor(c: &Cursor) -> Option<(&str, u64)> {
    let (epoch, gen_no) = c.0.rsplit_once(':')?;
    Some((epoch, gen_no.parse().ok()?))
}

/// Everything `g` holds at or below `root`.
pub(crate) fn copy_subtree(g: &Generation, root: &str, into: &mut Entries) {
    if root.is_empty() {
        into.extend(g.entries.iter().map(|(k, v)| (k.clone(), v.clone())));
        return;
    }
    if let Some(v) = g.entries.get(root) {
        into.insert(root.to_string(), v.clone());
    }
    let prefix = format!("{root}/");
    for (k, v) in g.entries.range(prefix.clone()..) {
        if !k.starts_with(&prefix) {
            break;
        }
        into.insert(k.clone(), v.clone());
    }
}

/// Removals in the order the feed wants them: children before their folders.
pub(crate) fn children_first(removed: &mut [&Pid]) {
    removed.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count() + usize::from(!p.is_empty())));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_subtrees_copy() {
        let c = cursor_of("e1", 7);
        assert_eq!(parse_cursor(&c), Some(("e1", 7)));
        assert_eq!(parse_cursor(&Cursor("nonsense".into())), None);
        let mut g = Generation::default();
        for p in ["", "S", "S/a", "S/b", "Sx", "T/c"] {
            g.entries.insert(p.into(), ("v".into(), false));
        }
        let mut out = Entries::new();
        copy_subtree(&g, "S", &mut out);
        assert_eq!(out.keys().cloned().collect::<Vec<_>>(), ["S", "S/a", "S/b"]);
        let (a, b, c) = ("S".to_string(), "S/a".to_string(), String::new());
        let mut v = vec![&c, &a, &b];
        children_first(&mut v);
        assert_eq!(v, [&b, &a, &c]);
    }
}
