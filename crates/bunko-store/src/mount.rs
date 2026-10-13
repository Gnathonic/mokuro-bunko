//! The process mount table: a path under a mounted store's root goes to that store.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::RwLock;

use crate::store::LibraryStore;

struct Mount {
    id: u64,
    /// The root's bytes, for a cheap prefix test on every call.
    root: Vec<u8>,
    store: Arc<dyn LibraryStore>,
}

static MOUNTS: RwLock<Vec<Mount>> = RwLock::new(Vec::new());
static COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The store stays mounted while this lives.
#[must_use = "dropping the guard unmounts the store"]
pub struct MountGuard(u64);

impl Drop for MountGuard {
    fn drop(&mut self) {
        let mut m = MOUNTS.write();
        m.retain(|mount| mount.id != self.0);
        COUNT.store(m.len(), Ordering::Release);
    }
}

fn path_bytes(p: &Path) -> &[u8] {
    p.as_os_str().as_encoded_bytes()
}

fn is_separator(b: u8) -> bool {
    b == b'/' || (cfg!(windows) && b == b'\\')
}

/// `p` is `root` or lies under it: a byte prefix that ends at a separator.
fn under(p: &[u8], root: &[u8]) -> bool {
    let root = match root.last() {
        Some(&last) if root.len() > 1 && is_separator(last) => &root[..root.len() - 1],
        _ => root,
    };
    p.len() >= root.len()
        && &p[..root.len()] == root
        && (p.len() == root.len()
            || root.last().is_some_and(|&b| is_separator(b))
            || is_separator(p[root.len()]))
}

/// Mount `store` at `store.root()`.
pub fn mount(store: Arc<dyn LibraryStore>) -> MountGuard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let root = path_bytes(store.root()).to_vec();
    let mut m = MOUNTS.write();
    m.push(Mount { id, root, store });
    COUNT.store(m.len(), Ordering::Release);
    MountGuard(id)
}

/// Run `found` with the store whose root is the longest prefix of `p`, or `none`
/// when no store covers it. The table stays read-locked (recursively, so a store may
/// itself use the shim) while `found` runs; no reference count is touched.
pub(crate) fn route<R>(
    p: &Path,
    found: impl FnOnce(&dyn LibraryStore) -> R,
    none: impl FnOnce() -> R,
) -> R {
    if COUNT.load(Ordering::Acquire) == 0 {
        return none();
    }
    let bytes = path_bytes(p);
    let m = MOUNTS.read_recursive();
    match m
        .iter()
        .filter(|mount| under(bytes, &mount.root))
        .max_by_key(|mount| mount.root.len())
    {
        Some(mount) => found(mount.store.as_ref()),
        None => {
            drop(m);
            none()
        }
    }
}

/// The store whose root is the longest prefix of `p` (lexical, case-exact), if any.
pub fn store_for(p: &Path) -> Option<Arc<dyn LibraryStore>> {
    if COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let bytes = path_bytes(p);
    let m = MOUNTS.read_recursive();
    m.iter()
        .filter(|mount| under(bytes, &mount.root))
        .max_by_key(|mount| mount.root.len())
        .map(|mount| Arc::clone(&mount.store))
}

/// Whether two handles are the same mounted store.
pub fn same_store(a: &Arc<dyn LibraryStore>, b: &Arc<dyn LibraryStore>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}

#[cfg(test)]
mod tests {
    use super::under;

    #[test]
    fn prefix_must_end_at_a_separator() {
        assert!(under(b"/data/library", b"/data/library"));
        assert!(under(b"/data/library/a.cbz", b"/data/library"));
        assert!(under(b"/data/library/a.cbz", b"/data/library/"));
        assert!(!under(b"/data/library2/a.cbz", b"/data/library"));
        assert!(!under(b"/data/lib", b"/data/library"));
        assert!(under(b"/x", b"/"));
    }
}
