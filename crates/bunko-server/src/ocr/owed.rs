//! What the library is owed: the in-memory owed index (0.5.2 `missing_generations`,
//! `_walk_candidates`, the shared walk cache).
//!
//! A full re-walk runs every poll interval (on a helper thread); WebDAV arrivals and
//! removals keep it current in between. The index holds only volumes that are owed
//! something (or withheld by the missing-pages rule), so a finished library costs
//! nothing. Every claim still re-checks its one job on disk ([`still_owed`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use bunko_core::generations::Generation;
use bunko_core::sidecar_names::Folder;
use bunko_library::sidecar;
use bunko_store::fs;

use super::types::{LibraryFacts, mtime_ns, rel_of};

/// One volume's debt.
#[derive(Clone, Debug, PartialEq)]
pub struct OwedVolume {
    pub size: u64,
    pub mtime_ns: i128,
    /// `st_mtime` in seconds (failure records compare against it).
    pub mtime: f64,
    /// Row ids owed, in list (= run) order.
    pub rows: Vec<Arc<str>>,
    /// Non-primary rows withheld because the supplied `.mokuro` names pages the archive
    /// lacks (never a failure, no record).
    pub skipped: Vec<Arc<str>>,
    pub missing_pages: i64,
    /// The primary row is owed a generation upgrade (generate, then swap).
    pub upgrade: bool,
    /// The metadata cache's page count, else the zip directory's own.
    pub pages: Option<i64>,
}

/// `<library>/<rel>` → its debt.
#[derive(Clone, Debug, Default)]
pub struct OwedIndex {
    pub volumes: BTreeMap<String, OwedVolume>,
}

/// `(plain, plain + ".gz")` sidecar paths of `cbz` for a row suffix.
pub fn sidecar_paths(cbz: &Path, suffix: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let plain = bunko_library::sidecar::with_suffix(cbz, suffix);
    let mut gz = plain.clone().into_os_string();
    gz.push(".gz");
    (plain, gz.into())
}

/// `needs_sidecar`, the other way round: the row's sidecar or its `.gz` exists, in any
/// spelling of the volume's stem (sidecar names rule v1: `V.MOKURO`, `v.fast.mokuro`
/// on a case-sensitive disk; generating it again would put a second copy beside it).
/// `folder`: the archive's folder when a walk has listed it; else it is listed here,
/// and only when the exact names are missing.
pub fn row_done(cbz: &Path, row: &Generation, folder: Option<&Folder>) -> bool {
    let (plain, gz) = sidecar_paths(cbz, &row.sidecar_suffix());
    if fs::exists(&plain) || fs::exists(&gz) {
        return true;
    }
    let listed_in = |folder: &Folder| {
        if row.primary {
            sidecar::primary_sidecar_in(cbz, folder).is_some()
        } else {
            sidecar::layer_sidecar_in(cbz, &row.name, folder).is_some()
        }
    };
    match folder {
        Some(folder) => listed_in(folder),
        None => {
            let listed = sidecar::list_names(cbz.parent().unwrap_or_else(|| Path::new("")))
                .unwrap_or_default();
            let names: Vec<&str> = listed.iter().map(String::as_str).collect();
            listed_in(&Folder::new(&names))
        }
    }
}

/// Decides whether a volume whose primary is done is owed a generation upgrade
/// (see `upgrade.rs`); the walk asks it only for volumes with a bare `.mokuro`.
pub trait UpgradeProbe: Send + Sync {
    fn wants_upgrade(&self, cbz: &Path, rel: &str) -> bool;
}

/// `missing_generations(cbz)` with the missing-pages rule. None when nothing is owed.
pub fn compute(
    library: &Path,
    cbz: &Path,
    rows: &[Generation],
    facts: &dyn LibraryFacts,
    upgrade: Option<&dyn UpgradeProbe>,
) -> Option<OwedVolume> {
    compute_in(library, cbz, rows, facts, upgrade, None)
}

/// [`compute`] with the archive's folder already listed (the walk lists each once).
fn compute_in(
    library: &Path,
    cbz: &Path,
    rows: &[Generation],
    facts: &dyn LibraryFacts,
    upgrade: Option<&dyn UpgradeProbe>,
    folder: Option<&Folder>,
) -> Option<OwedVolume> {
    let name = cbz.file_name()?.to_string_lossy().into_owned();
    if !bunko_library::sidecar::is_cbz_name(&name) {
        return None;
    }
    // A case merge is moving this volume: the census skips it (it is judged again
    // where the merge puts it).
    if merge_busy(cbz) {
        return None;
    }
    let meta = fs::metadata(cbz).ok()?;
    if !meta.is_file() {
        return None;
    }
    let enabled: Vec<&Generation> = bunko_core::generations::enabled_generations(rows).collect();
    let mut owed: Vec<&Generation> = enabled
        .iter()
        .copied()
        .filter(|r| !row_done(cbz, r, folder))
        .collect();
    let primary_done = enabled
        .iter()
        .any(|r| r.primary && !owed.iter().any(|o| o.id == r.id));
    let mut skipped = Vec::new();
    let mut missing_pages = 0;
    if owed.iter().any(|r| !r.primary) && primary_done {
        missing_pages = facts.missing_pages(cbz);
        if missing_pages > 0 {
            skipped = owed
                .iter()
                .filter(|r| !r.primary)
                .map(|r| Arc::from(r.id.as_str()))
                .collect();
            owed.retain(|r| r.primary);
        }
    }
    let mut wants_upgrade = false;
    if let Some(probe) = upgrade
        && primary_done
        && let Some(rel) = rel_of(library, cbz)
    {
        // While judged, a case merge leaves the volume alone.
        let _judging = super::busy::judging(cbz);
        wants_upgrade = probe.wants_upgrade(cbz, &rel);
    }
    if owed.is_empty() && skipped.is_empty() && !wants_upgrade {
        return None;
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    Some(OwedVolume {
        size: meta.len(),
        mtime_ns: mtime_ns(&meta),
        mtime,
        rows: owed.iter().map(|r| Arc::from(r.id.as_str())).collect(),
        skipped,
        missing_pages,
        upgrade: wants_upgrade,
        pages: None,
    })
}

/// Every `library/**/*.cbz` (a case-sensitive `.cbz` suffix, as 0.5.2's glob), sorted.
/// The per-user names at the root (a cloud progress user's `history/`) are not walked.
pub fn list_archives(library: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![library.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        let top = dir.as_path() == library;
        for entry in rd.flatten() {
            if top
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(bunko_core::reader_tree::is_per_user_root_name)
            {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if (ft.is_file() || ft.is_symlink())
                && bunko_library::sidecar::is_cbz_name(&entry.file_name().to_string_lossy())
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// The full walk.
/// Page counts already known, by volume: `(size, mtime_ns, pages)`. A walk reuses a
/// count while the archive's stamp is unchanged, so a poll does not re-read the zip
/// directory of every owed volume.
pub type KnownPages = std::collections::HashMap<String, (u64, i128, Option<i64>)>;

/// The page count of a volume: the metadata cache's, else the zip directory's own.
pub fn pages_of(cbz: &Path, facts: &dyn LibraryFacts) -> Option<i64> {
    facts.page_count(cbz).or_else(|| archive_pages(cbz))
}

/// The full walk.
pub fn walk(
    library: &Path,
    rows: &[Generation],
    facts: &dyn LibraryFacts,
    upgrade: Option<&dyn UpgradeProbe>,
    known: &KnownPages,
) -> OwedIndex {
    let mut index = OwedIndex::default();
    // Each folder is listed once for all of its archives.
    let mut by_folder: BTreeMap<&Path, Vec<&Path>> = BTreeMap::new();
    let archives = list_archives(library);
    for cbz in &archives {
        let directory = cbz.parent().unwrap_or_else(|| Path::new(""));
        by_folder.entry(directory).or_default().push(cbz);
    }
    for (directory, archives) in by_folder {
        let listed = sidecar::list_names(directory).unwrap_or_default();
        let names: Vec<&str> = listed.iter().map(String::as_str).collect();
        let folder = Folder::new(&names);
        for cbz in archives {
            let Some(rel) = rel_of(library, cbz) else {
                continue;
            };
            let Some(mut v) = compute_in(library, cbz, rows, facts, upgrade, Some(&folder)) else {
                continue;
            };
            v.pages = match known.get(&rel) {
                Some((size, mtime, pages))
                    if *size == v.size && *mtime == v.mtime_ns && pages.is_some() =>
                {
                    *pages
                }
                _ => pages_of(cbz, facts),
            };
            index.volumes.insert(rel, v);
        }
    }
    index
}

/// `_still_owed`: the archive is a file and the row's sidecar is still missing (for an
/// upgrade job: the bare file still exists, i.e. the swap has not happened yet).
///
/// On a remote store (spec §5.4) a volume whose primary or row sidecar vanished within
/// the quiet period while its archive stayed is not owed now: the reader is replacing
/// that sidecar (MEGA deletes every copy before it uploads the new one).
pub fn still_owed(library: &Path, rel: &str, row: &Generation, upgrade: bool) -> bool {
    let cbz = library.join(rel);
    if !fs::is_file(&cbz) {
        return false;
    }
    // Held while a case merge is open on the volume's folder (spec §5.9).
    if merge_busy(&cbz) {
        return false;
    }
    if sidecar_recently_removed(&cbz, row) {
        return false;
    }
    if upgrade {
        true
    } else {
        !row_done(&cbz, row, None)
    }
}

/// A case merge journal group is open on `cbz` (or its folder) on a remote store
/// ([`bunko_store::LibraryStore::merge_busy`]). Always false locally.
pub fn merge_busy(cbz: &Path) -> bool {
    bunko_store::store_for(cbz).is_some_and(|s| s.merge_busy(cbz))
}

/// A sidecar of `cbz` (the primary's, or `row`'s) left a remote store within its quiet
/// period ([`bunko_store::LibraryStore::recently_removed`]). Always false locally.
pub fn sidecar_recently_removed(cbz: &Path, row: &Generation) -> bool {
    let Some(store) = bunko_store::store_for(cbz) else {
        return false;
    };
    if !store.caps().remote {
        return false;
    }
    let mut suffixes = vec![row.sidecar_suffix()];
    if !row.primary {
        suffixes.push(".mokuro".to_string());
    }
    suffixes.iter().any(|s| {
        let (plain, gz) = sidecar_paths(cbz, s);
        store.recently_removed(&plain).is_some() || store.recently_removed(&gz).is_some()
    })
}

/// `_archive_pages`: image members of the zip that are pages (0 / unreadable: None).
pub fn archive_pages(cbz: &Path) -> Option<i64> {
    let volume = bunko_library::Volume::open(cbz).ok()?;
    let n = volume.pages().len() as i64;
    (n > 0).then_some(n)
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // fixtures are written with std::fs
mod tests {
    use super::*;
    use crate::ocr::types::FileFacts;
    use bunko_core::generations::default_generation;

    /// The per-user tree at the root (a cloud progress user's `history/`, whose names
    /// may end in `.cbz`) is never walked; a series' own `history/` folder is.
    #[test]
    fn list_archives_skips_the_per_user_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = tmp.path().join("library");
        for rel in [
            "history/dev-1/x.cbz",
            "Series/history/v.cbz",
            "Series/v.cbz",
        ] {
            let path = lib.join(rel);
            bunko_store::fs::create_dir_all(path.parent().unwrap()).unwrap();
            bunko_store::fs::write(&path, b"zip").unwrap();
        }
        assert_eq!(
            list_archives(&lib),
            vec![lib.join("Series/history/v.cbz"), lib.join("Series/v.cbz")]
        );
    }

    fn rows() -> Vec<Generation> {
        let mut layer = default_generation("g-2");
        layer.name = "fast".into();
        layer.primary = false;
        vec![default_generation("g-1"), layer]
    }

    #[test]
    fn owed_rows_and_done() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path();
        std::fs::create_dir_all(lib.join("S")).unwrap();
        std::fs::write(lib.join("S/V1.cbz"), b"PK").unwrap();
        std::fs::write(lib.join("S/V2.cbz"), b"PK").unwrap();
        std::fs::write(lib.join("S/V2.mokuro"), b"{}").unwrap();
        std::fs::write(lib.join("S/V2.fast.mokuro.gz"), b"x").unwrap();
        let index = walk(lib, &rows(), &FileFacts, None, &KnownPages::new());
        let v1 = &index.volumes["S/V1.cbz"];
        assert_eq!(
            v1.rows.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
            vec!["g-1", "g-2"]
        );
        assert!(!index.volumes.contains_key("S/V2.cbz"));
    }

    /// Regression (upgrade test): `Vol.CBZ` is an archive to the catalog, uploads and
    /// covers, but the OCR walk asked for exactly `.cbz` and never read it.
    #[test]
    fn upper_case_archives_are_owed_ocr() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path();
        std::fs::create_dir_all(lib.join("S")).unwrap();
        std::fs::write(lib.join("S/V1.CBZ"), b"PK").unwrap();
        std::fs::write(lib.join("S/V2.Cbz"), b"PK").unwrap();
        std::fs::write(lib.join("S/V2.mokuro"), b"{}").unwrap();
        std::fs::write(lib.join("S/V3.cbz.txt"), b"x").unwrap();
        assert_eq!(
            list_archives(lib),
            [lib.join("S/V1.CBZ"), lib.join("S/V2.Cbz")]
        );
        let index = walk(lib, &rows(), &FileFacts, None, &KnownPages::new());
        let owed: Vec<(&str, Vec<String>)> = index
            .volumes
            .iter()
            .map(|(rel, v)| (rel.as_str(), v.rows.iter().map(|r| r.to_string()).collect()))
            .collect();
        assert_eq!(
            owed,
            [
                ("S/V1.CBZ", vec!["g-1".to_string(), "g-2".to_string()]),
                ("S/V2.Cbz", vec!["g-2".to_string()])
            ]
        );
        assert!(still_owed(lib, "S/V1.CBZ", &rows()[0], false));
    }

    /// Sidecar names rule v1: a sidecar in another spelling of the volume's stem
    /// (`V.MOKURO`, `v.fast.mokuro` on a case-sensitive disk) is the volume's, so the
    /// walk does not OCR it again and write a second primary beside it.
    #[test]
    fn a_sidecar_in_another_spelling_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path();
        std::fs::create_dir_all(lib.join("S")).unwrap();
        for name in [
            "V1.cbz",
            "V1.MOKURO",
            "V2.cbz",
            "V2.mokuro",
            "v2.fast.mokuro.gz",
        ] {
            std::fs::write(lib.join("S").join(name), b"{}").unwrap();
        }
        // Two archives fold alike: a folded-only sidecar is neither's.
        for name in [
            "W.cbz", "w.cbz", "W.mokuro", "w.mokuro", "x.cbz", "X.cbz", "x.MOKURO",
        ] {
            std::fs::write(lib.join("S").join(name), b"{}").unwrap();
        }
        let index = walk(lib, &rows(), &FileFacts, None, &KnownPages::new());
        let owed: Vec<(&str, Vec<String>)> = index
            .volumes
            .iter()
            .map(|(rel, v)| (rel.as_str(), v.rows.iter().map(|r| r.to_string()).collect()))
            .collect();
        assert_eq!(
            owed,
            [
                ("S/V1.cbz", vec!["g-2".to_string()]),
                ("S/W.cbz", vec!["g-2".to_string()]),
                ("S/X.cbz", vec!["g-1".to_string(), "g-2".to_string()]),
                ("S/w.cbz", vec!["g-2".to_string()]),
                ("S/x.cbz", vec!["g-2".to_string()]),
            ]
        );
        assert!(!still_owed(lib, "S/V1.cbz", &rows()[0], false));
        assert!(!still_owed(lib, "S/V2.cbz", &rows()[1], false));
        assert!(still_owed(lib, "S/X.cbz", &rows()[0], false));
    }
}
