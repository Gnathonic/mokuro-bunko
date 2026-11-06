//! Which volumes OCR work touches right now, for the case merge (spec §5.9 "When":
//! no OCR claim running or spooling, no upgrade judged or swapped, under either
//! variant). The scheduler publishes its claims (a claim holds its spooled archive
//! too, and an upgrade swap settles inside its claim) and the paced direct upgrade
//! its volumes; the walk marks each volume while its upgrade is judged. The store's
//! merge asks [`busy_under`] through the server's `CaseMergePolicy`.
//!
//! Process-wide and keyed by absolute archive path: each library has its own
//! (synthetic) root, so several servers in one process never share an entry.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;

#[derive(Default)]
struct Busy {
    /// Claims (and their spools), per library root.
    claims: HashMap<PathBuf, HashSet<PathBuf>>,
    /// The paced direct upgrade's volumes, per library root.
    direct: HashMap<PathBuf, HashSet<PathBuf>>,
    /// Volumes whose upgrade verdict is being judged now.
    judging: HashMap<PathBuf, usize>,
}

static BUSY: Mutex<Option<Busy>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Busy) -> R) -> R {
    f(BUSY.lock().get_or_insert_with(Busy::default))
}

/// The scheduler's claims under `library` are exactly `claims` now.
pub fn set_claims(library: &Path, claims: HashSet<PathBuf>) {
    with(|b| {
        if claims.is_empty() {
            b.claims.remove(library);
        } else {
            b.claims.insert(library.to_path_buf(), claims);
        }
    });
}

/// The paced direct upgrade swaps `volumes` under `library` (empty: it ended).
pub fn set_direct(library: &Path, volumes: HashSet<PathBuf>) {
    with(|b| {
        if volumes.is_empty() {
            b.direct.remove(library);
        } else {
            b.direct.insert(library.to_path_buf(), volumes);
        }
    });
}

/// Marks `cbz` as being judged while the value lives.
pub struct Judging(PathBuf);

impl Drop for Judging {
    fn drop(&mut self) {
        with(|b| {
            if let Some(n) = b.judging.get_mut(&self.0) {
                *n -= 1;
                if *n == 0 {
                    b.judging.remove(&self.0);
                }
            }
        });
    }
}

/// The upgrade census judges `cbz` until the guard drops.
pub fn judging(cbz: &Path) -> Judging {
    with(|b| *b.judging.entry(cbz.to_path_buf()).or_insert(0) += 1);
    Judging(cbz.to_path_buf())
}

/// Some OCR work touches `path` or a volume under it.
pub fn busy_under(path: &Path) -> bool {
    with(|b| {
        let hit = |p: &PathBuf| p == path || p.starts_with(path);
        b.claims.values().flatten().any(hit)
            || b.direct.values().flatten().any(hit)
            || b.judging.keys().any(hit)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_direct_and_judging_count() {
        let lib = Path::new("/x/cloud/busy-test/library");
        assert!(!busy_under(&lib.join("S")));
        set_claims(lib, [lib.join("S/V1.cbz")].into());
        assert!(busy_under(&lib.join("S")));
        assert!(busy_under(&lib.join("S/V1.cbz")));
        assert!(!busy_under(&lib.join("s")), "exact spelling");
        set_claims(lib, HashSet::new());
        assert!(!busy_under(&lib.join("S")));
        {
            let _g = judging(&lib.join("T/V2.cbz"));
            assert!(busy_under(&lib.join("T")));
        }
        assert!(!busy_under(&lib.join("T")));
        set_direct(lib, [lib.join("U/V.cbz")].into());
        assert!(busy_under(&lib.join("U")));
        set_direct(lib, HashSet::new());
        assert!(!busy_under(&lib.join("U")));
    }
}
