//! Per-path write locks (0.5.2 batch 5 `_PathWriteLocks`, spec §8.7).
//!
//! Non-blocking: a write that finds its path (or an ancestor or descendant of it) held
//! fails at once with 423 instead of queueing. Keys are the case-folded components of the
//! resolved path, so `Series/Vol.CBZ` conflicts with `series/vol.cbz` (case-insensitive
//! filesystems). Reads never take these locks: they see the old file until the atomic
//! rename.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

type Key = Vec<String>;

#[derive(Debug, Default, Clone)]
pub struct PathWriteLocks {
    held: Arc<Mutex<Vec<Key>>>,
}

/// Holds the locks of one operation; released on drop.
#[derive(Debug)]
pub struct WriteLockGuard {
    locks: PathWriteLocks,
    keys: Vec<Key>,
}

impl Drop for WriteLockGuard {
    fn drop(&mut self) {
        let mut held = self.locks.held.lock();
        for key in &self.keys {
            if let Some(pos) = held.iter().position(|k| k == key) {
                held.swap_remove(pos);
            }
        }
    }
}

fn key_of(path: &Path) -> Key {
    resolved(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_lowercase())
        .collect()
}

/// `path` with its deepest existing ancestor resolved (symlinks, `/var` vs
/// `/private/var` on macOS, a storage path under a symlinked mount), so every writer's
/// spelling of one file is one key: the DAV layer resolves its roots, other writers (an
/// OCR install, an upgrade swap) name the configured path. The last component is kept
/// as given: a symlink there is the entry a write replaces. A path with no existing
/// ancestor is kept whole.
fn resolved(path: &Path) -> PathBuf {
    let Some(name) = path.file_name() else {
        return path.to_path_buf();
    };
    let mut tail = vec![name.to_os_string()];
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.as_os_str().is_empty() {
            break;
        }
        if let Ok(real) = bunko_store::fs::canonicalize(d) {
            let mut out = real;
            out.extend(tail.iter().rev());
            return out;
        }
        match d.file_name() {
            Some(n) => tail.push(n.to_os_string()),
            None => break,
        }
        dir = d.parent();
    }
    path.to_path_buf()
}

fn conflicts(a: &Key, b: &Key) -> bool {
    let n = a.len().min(b.len());
    a[..n] == b[..n]
}

impl PathWriteLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire `path`, or `None` when it conflicts with a held lock.
    pub fn try_lock(&self, path: &Path) -> Option<WriteLockGuard> {
        self.try_lock_all(&[path])
    }

    /// Acquire every path or none (deterministic order, as 0.5.2 `_try_acquire_all`).
    pub fn try_lock_all(&self, paths: &[&Path]) -> Option<WriteLockGuard> {
        let mut keys: Vec<Key> = paths.iter().map(|p| key_of(p)).collect();
        keys.sort();
        // A rename that only changes case names one entry twice, and the keys are
        // case-folded: lock it once, or it conflicts with itself (0.5.2 answered 423;
        // 0.5.3 `_try_acquire_all`).
        keys.dedup();
        let mut held = self.held.lock();
        for (i, key) in keys.iter().enumerate() {
            if held.iter().any(|h| conflicts(h, key)) {
                return None;
            }
            // Two of the requested paths conflicting with each other (a MOVE onto its own
            // ancestor) is still one operation's business: 0.5.2 failed it too.
            if keys[..i].iter().any(|k| conflicts(k, key)) {
                return None;
            }
        }
        held.extend(keys.iter().cloned());
        drop(held);
        Some(WriteLockGuard {
            locks: self.clone(),
            keys,
        })
    }

    /// Number of held locks (tests).
    pub fn held_count(&self) -> usize {
        self.held.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_ancestor_descendant_and_case() {
        let locks = PathWriteLocks::new();
        let a = PathBuf::from("/lib/Series/Vol.cbz");
        let g = locks.try_lock(&a).unwrap();
        assert!(locks.try_lock(&a).is_none());
        assert!(locks.try_lock(Path::new("/lib/series/vol.CBZ")).is_none());
        assert!(locks.try_lock(Path::new("/lib/Series")).is_none());
        assert!(locks.try_lock(Path::new("/lib/Other/Vol.cbz")).is_some());
        drop(g);
        let folder = locks.try_lock(Path::new("/lib/Series")).unwrap();
        assert!(locks.try_lock(&a).is_none());
        drop(folder);
        assert!(locks.try_lock(&a).is_some());
        assert_eq!(locks.held_count(), 0);
    }

    #[test]
    fn a_case_only_rename_locks_its_entry_once() {
        let locks = PathWriteLocks::new();
        let both = locks
            .try_lock_all(&[Path::new("/lib/kingdom"), Path::new("/lib/Kingdom")])
            .expect("a case-only rename does not conflict with itself");
        assert_eq!(locks.held_count(), 1);
        assert!(locks.try_lock(Path::new("/lib/KINGDOM/v.cbz")).is_none());
        drop(both);
        assert_eq!(locks.held_count(), 0);
        // An ancestor and its descendant in one operation still conflict.
        assert!(
            locks
                .try_lock_all(&[Path::new("/lib/a"), Path::new("/lib/A/b")])
                .is_none()
        );
    }

    #[test]
    fn two_spellings_through_a_symlinked_folder_are_one_lock() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        bunko_store::fs::create_dir_all(real.join("Series")).unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        return;
        let locks = PathWriteLocks::new();
        let _g = locks
            .try_lock(&real.join("Series/Vol 1.mokuro"))
            .expect("free");
        assert!(
            locks.try_lock(&link.join("Series/Vol 1.mokuro")).is_none(),
            "the same file through the symlinked folder"
        );
        let _new = locks
            .try_lock(&real.join("Series/New/Vol 2.mokuro"))
            .expect("free");
        assert!(
            locks
                .try_lock(&link.join("Series/New/Vol 2.mokuro"))
                .is_none(),
            "a path that does not exist yet still resolves its existing ancestor"
        );
        assert!(locks.try_lock(&link.join("Other/Vol 1.mokuro")).is_some());
    }

    #[test]
    fn all_or_none() {
        let locks = PathWriteLocks::new();
        let _held = locks.try_lock(Path::new("/lib/b")).unwrap();
        assert!(
            locks
                .try_lock_all(&[Path::new("/lib/a"), Path::new("/lib/b")])
                .is_none()
        );
        assert_eq!(locks.held_count(), 1);
        let both = locks
            .try_lock_all(&[Path::new("/lib/a"), Path::new("/lib/c")])
            .unwrap();
        assert_eq!(locks.held_count(), 3);
        drop(both);
        assert_eq!(locks.held_count(), 1);
    }
}
