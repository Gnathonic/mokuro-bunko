//! The process mount table: a path under a mounted store's root goes to that store.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::RwLock;

use crate::store::LibraryStore;

type Mounts = Vec<(u64, Arc<dyn LibraryStore>)>;

static MOUNTS: RwLock<Mounts> = RwLock::new(Vec::new());
static COUNT: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The store stays mounted while this lives.
#[must_use = "dropping the guard unmounts the store"]
pub struct MountGuard(u64);

impl Drop for MountGuard {
    fn drop(&mut self) {
        let mut m = MOUNTS.write();
        m.retain(|(id, _)| *id != self.0);
        COUNT.store(m.len(), Ordering::Release);
    }
}

/// Mount `store` at `store.root()`.
pub fn mount(store: Arc<dyn LibraryStore>) -> MountGuard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut m = MOUNTS.write();
    m.push((id, store));
    COUNT.store(m.len(), Ordering::Release);
    MountGuard(id)
}

/// The store whose root is the longest prefix of `p` (lexical, case-exact), if any.
pub fn store_for(p: &Path) -> Option<Arc<dyn LibraryStore>> {
    if COUNT.load(Ordering::Acquire) == 0 {
        return None;
    }
    let m = MOUNTS.read();
    m.iter()
        .filter(|(_, s)| p.starts_with(s.root()))
        .max_by_key(|(_, s)| s.root().as_os_str().len())
        .map(|(_, s)| Arc::clone(s))
}

/// Whether two handles are the same mounted store.
pub fn same_store(a: &Arc<dyn LibraryStore>, b: &Arc<dyn LibraryStore>) -> bool {
    std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))
}
