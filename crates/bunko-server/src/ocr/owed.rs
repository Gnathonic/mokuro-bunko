//! What the library is owed: the in-memory owed index (0.5.2 `missing_generations`,
//! `_walk_candidates`, the shared walk cache).
//!
//! A full re-walk runs every poll interval (on a helper thread); WebDAV arrivals and
//! removals keep it current in between. The index holds only volumes that are owed
//! something (or withheld by the missing-pages rule), so a finished library costs
//! nothing. Every claim still re-checks its one job on disk ([`still_owed`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bunko_core::generations::Generation;
use bunko_core::sidecar_names::Folder;
use bunko_library::sidecar;
use bunko_store::fs;

use super::types::{LibraryFacts, Stamp, mtime_ns, rel_of};

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
    /// The page suffixes read from the archive at that version (`(size, mtime_ns)`),
    /// once a processor of an older release needed them (`machine_reads`). Read on a
    /// helper thread on a remote store; a walk keeps them while the version stays.
    pub page_suffixes: Option<(Stamp, PageSuffixes)>,
}

/// What a volume's zip directory says about its page formats.
#[derive(Clone, Debug, PartialEq)]
pub enum PageSuffixes {
    /// The suffixes of its pages (lowercase, with the dot), each once.
    Read(Arc<[String]>),
    /// Damaged or of a kind this build cannot open: no format to judge (the claim's
    /// own checks decide, as for any machine).
    Unreadable,
}

/// The page suffixes of `cbz` from its zip directory. `Err`: it could not be read
/// at all (an I/O error, the network), worth trying again.
pub fn read_page_suffixes(cbz: &Path) -> std::io::Result<PageSuffixes> {
    let volume = match bunko_library::Volume::open(cbz) {
        Ok(volume) => volume,
        Err(bunko_library::archive::ArchiveError::Io(e)) => return Err(e),
        Err(_) => return Ok(PageSuffixes::Unreadable),
    };
    let suffixes: std::collections::BTreeSet<String> = volume
        .pages()
        .iter()
        .filter_map(|page| {
            let name = page.path.rsplit('/').next().unwrap_or(&page.path);
            name.rfind('.').map(|i| name[i..].to_ascii_lowercase())
        })
        .collect();
    Ok(PageSuffixes::Read(suffixes.into_iter().collect()))
}

/// `<library>/<rel>` → its debt.
#[derive(Clone, Debug, Default)]
pub struct OwedIndex {
    pub volumes: BTreeMap<String, OwedVolume>,
    /// `<stem>_<n>.mokuro` files an older collector numbered, library-relative
    /// ([`super::orphans`]); swept after the walk.
    pub orphans: Vec<String>,
}

/// `(plain, plain + ".gz")` sidecar paths of `cbz` for a row suffix.
pub fn sidecar_paths(cbz: &Path, suffix: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let plain = bunko_library::sidecar::with_suffix(cbz, suffix);
    let mut gz = plain.clone().into_os_string();
    gz.push(".gz");
    (plain, gz.into())
}

/// [`sidecar_paths`] as Bunko reads and writes them: each name in the spelling the
/// archive's folder lists (`v.mokuro` for `V.cbz`,
/// [`bunko_library::sidecar::sibling_in`]), else the constructed name; a pair keeps one
/// spelling (`v.mokuro` when only `v.mokuro.gz` is listed). Bunko never writes a second
/// spelling of a sidecar. `None` when nothing may be written for the archive: another
/// spelling of it shadows it, or every listed spelling is another archive's. One listing.
pub fn listed_sidecar_paths(cbz: &Path, suffix: &str) -> Option<(PathBuf, PathBuf)> {
    let directory = cbz.parent().unwrap_or_else(|| Path::new(""));
    let names = sidecar::list_names(directory).unwrap_or_default();
    listed_sidecar_paths_in(cbz, suffix, &names)
}

/// [`listed_sidecar_paths`] over `names`, the archive folder's file names.
pub fn listed_sidecar_paths_in(
    cbz: &Path,
    suffix: &str,
    names: &[String],
) -> Option<(PathBuf, PathBuf)> {
    let directory = cbz.parent().unwrap_or_else(|| Path::new(""));
    let names = names.to_vec();
    let gz_suffix = format!("{suffix}.gz");
    let plain = sidecar::sibling_in(cbz, suffix, &names)?;
    let gz = sidecar::sibling_in(cbz, &gz_suffix, &names)?;
    let listed = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| names.iter().any(|l| l == n))
    };
    let with_gz = |p: &Path| {
        let mut s = p.as_os_str().to_owned();
        s.push(".gz");
        PathBuf::from(s)
    };
    Some(match (listed(&plain), listed(&gz)) {
        (false, true) => {
            let name = gz.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let base = name.get(..name.len().saturating_sub(3)).unwrap_or(name);
            (directory.join(base), gz)
        }
        (true, false) => {
            let gz = with_gz(&plain);
            (plain, gz)
        }
        _ => (plain, gz),
    })
}

/// `needs_sidecar`, the other way round: rule v1 finds the row's sidecar or its `.gz`
/// in the archive's folder listing, in any spelling of the volume's stem (`V.MOKURO`,
/// `v.fast.mokuro` on a case-sensitive disk), the layer id exact. Decided from the
/// listing alone, never by opening a built name, so every host answers alike; the
/// writer ([`listed_sidecar_paths`]) declines what rule v1 does not give the archive, so
/// a row is never owed for a file that cannot be its own (`V.FAST.mokuro`).
/// `folder`: the archive's folder when a walk has listed it; else it is listed here.
pub fn row_done(cbz: &Path, row: &Generation, folder: Option<&Folder>) -> bool {
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
    /// A generation layer `row` wrote, now outdated by its engine's revision: owed again
    /// although its file exists (critique C10).
    fn layer_outdated(&self, _cbz: &Path, _row: &Generation) -> bool {
        false
    }
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

/// [`compute`] with the archive's folder already listed (the walk lists each once):
/// its file names and their [`Folder`].
fn compute_in(
    library: &Path,
    cbz: &Path,
    rows: &[Generation],
    facts: &dyn LibraryFacts,
    upgrade: Option<&dyn UpgradeProbe>,
    listed: Option<(&[String], &Folder)>,
) -> Option<OwedVolume> {
    let folder = listed.map(|(_, folder)| folder);
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
    // Nothing is owed that could not be written: a sidecar whose every listed spelling
    // is another archive's (rule v1, a shadowed archive's included). Such a volume is
    // left as it is until its case variants are merged by hand (doctor reports them).
    let own_names;
    let names: &[String] = match listed {
        Some((names, _)) => names,
        None => {
            own_names = sidecar::list_names(cbz.parent().unwrap_or_else(|| Path::new("")))
                .unwrap_or_default();
            &own_names
        }
    };
    let writable = |suffix: &str| listed_sidecar_paths_in(cbz, suffix, names).is_some();
    if !writable(".mokuro") {
        return None;
    }
    let enabled: Vec<&Generation> = bunko_core::generations::enabled_generations(rows).collect();
    let mut owed: Vec<&Generation> = enabled
        .iter()
        .copied()
        .filter(|r| !row_done(cbz, r, folder) || upgrade.is_some_and(|p| p.layer_outdated(cbz, r)))
        .filter(|r| writable(&r.sidecar_suffix()))
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
        page_suffixes: None,
    })
}

/// Every `library/**/*.cbz` (any case), sorted. The per-user names at the root (a cloud
/// progress user's `history/`, any spelling) are not walked, nor is a folder or archive
/// another listed spelling shadows.
pub fn list_archives(library: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![library.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        let top = dir.as_path() == library;
        let entries: Vec<_> = rd.flatten().collect();
        // A folder or archive another listed spelling shadows is never reached: no
        // OCR is owed for it, and none is written beside the one that shadows it.
        let names: Vec<String> = entries
            .iter()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .collect();
        let losers = bunko_library::fsutil::case_losers(&dir, names.iter().map(String::as_str));
        for entry in entries {
            // Nor is the temporary name an interrupted case-only rename left (doctor's).
            if entry.file_name().to_str().is_some_and(|n| {
                losers.contains(n) || n.starts_with(bunko_store::fs::CASE_RENAME_PREFIX)
            }) {
                continue;
            }
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
        let stems = super::orphans::OrphanStems::new(&names);
        index.orphans.extend(
            names
                .iter()
                .filter(|n| super::orphans::orphan_target(n, &stems).is_some())
                .filter_map(|n| rel_of(library, &directory.join(n))),
        );
        for cbz in archives {
            let Some(rel) = rel_of(library, cbz) else {
                continue;
            };
            let Some(mut v) =
                compute_in(library, cbz, rows, facts, upgrade, Some((&listed, &folder)))
            else {
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
    still_owed_with(library, rel, row, upgrade, None)
}

/// [`still_owed`], where a layer `probe` finds outdated is owed again although its file
/// exists ([`UpgradeProbe::layer_outdated`]; `probe` is the census when upgrades are on).
pub fn still_owed_with(
    library: &Path,
    rel: &str,
    row: &Generation,
    upgrade: bool,
    probe: Option<&dyn UpgradeProbe>,
) -> bool {
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
    // Never owed where nothing may be written ([`compute`]).
    if listed_sidecar_paths(&cbz, ".mokuro").is_none()
        || listed_sidecar_paths(&cbz, &row.sidecar_suffix()).is_none()
    {
        return false;
    }
    if upgrade {
        true
    } else {
        !row_done(&cbz, row, None) || probe.is_some_and(|p| p.layer_outdated(&cbz, row))
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
    /// (`V.MOKURO`, `v2.fast.mokuro.gz`) is the volume's, so the
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
        // A volume with no sidecar in any spelling owes everything. (Two archives whose
        // stems fold alike, which only a case-sensitive store can list, are bunko-core's
        // synthetic-listing vectors.)
        std::fs::write(lib.join("S").join("X.cbz"), b"{}").unwrap();
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
                ("S/X.cbz", vec!["g-1".to_string(), "g-2".to_string()]),
            ]
        );
        assert!(!still_owed(lib, "S/V1.cbz", &rows()[0], false));
        assert!(!still_owed(lib, "S/V2.cbz", &rows()[1], false));
        assert!(still_owed(lib, "S/X.cbz", &rows()[0], false));
    }

    /// A layer file whose id is spelled in another case (`Vol.FAST.mokuro.gz`) is no
    /// layer of the volume by rule v1 (the id exact), and it is the layer's name by the
    /// filename fold: the walk owes nothing a write could not fill (it would be a second
    /// spelling of that file), so the row is neither done nor owed again and again, and
    /// the same on every host (decided from the listing, never by opening a built name).
    #[test]
    fn a_layer_id_in_another_case_is_never_owed() {
        for other in ["Vol.FAST.mokuro.gz", "Vol.Fast.mokuro"] {
            let dir = tempfile::tempdir().unwrap();
            let lib = dir.path();
            let s = lib.join("S");
            std::fs::create_dir_all(&s).unwrap();
            for name in ["Vol.cbz", "Vol.mokuro", other] {
                std::fs::write(s.join(name), b"{}").unwrap();
            }
            let cbz = s.join("Vol.cbz");
            let layer = &rows()[1];
            assert!(!row_done(&cbz, layer, None), "{other}");
            assert_eq!(listed_sidecar_paths(&cbz, ".fast.mokuro"), None, "{other}");
            let index = walk(lib, &rows(), &FileFacts, None, &KnownPages::new());
            assert!(index.volumes.is_empty(), "{other}: {:?}", index.volumes);
            assert!(!still_owed(lib, "S/Vol.cbz", layer, false), "{other}");
            let mut listed: Vec<String> = sidecar::list_names(&s).unwrap();
            listed.sort();
            let mut want = vec![
                "Vol.cbz".to_string(),
                "Vol.mokuro".to_string(),
                other.to_string(),
            ];
            want.sort();
            assert_eq!(listed, want);
        }
    }

    /// A result goes to the spelling the folder lists, on every host: a reader's
    /// `v.mokuro` beside `V.cbz` is the volume's primary, so the collector keeps it
    /// (the result is discarded) instead of writing `V.mokuro` beside it on a
    /// case-sensitive disk. A pair keeps the listed spelling; nothing listed keeps the
    /// archive's.
    #[test]
    fn a_result_goes_to_the_listed_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path().join("S");
        std::fs::create_dir_all(&s).unwrap();
        std::fs::write(s.join("V.cbz"), b"zip").unwrap();
        let pair = |a: &str, b: &str| Some((s.join(a), s.join(b)));
        assert_eq!(
            listed_sidecar_paths(&s.join("V.cbz"), ".mokuro"),
            pair("V.mokuro", "V.mokuro.gz")
        );
        std::fs::write(s.join("v.mokuro.gz"), b"{}").unwrap();
        assert_eq!(
            listed_sidecar_paths(&s.join("V.cbz"), ".mokuro"),
            pair("v.mokuro", "v.mokuro.gz")
        );
        std::fs::remove_file(s.join("v.mokuro.gz")).unwrap();
        std::fs::write(s.join("v.mokuro"), b"{\"pages\":[]}").unwrap();
        let (plain, gz) = listed_sidecar_paths(&s.join("V.cbz"), ".mokuro").unwrap();
        assert_eq!(
            (plain.clone(), gz.clone()),
            (s.join("v.mokuro"), s.join("v.mokuro.gz"))
        );
        assert!(matches!(
            crate::ocr::collect::reconcile_existing(&plain, &gz, Some(&("hayai-nova".into(), 9))),
            crate::ocr::collect::Existing::Keep
        ));
        // A layer in another spelling of the stem.
        std::fs::write(s.join("v.fast.mokuro"), b"{}").unwrap();
        assert_eq!(
            listed_sidecar_paths(&s.join("V.cbz"), ".fast.mokuro"),
            pair("v.fast.mokuro", "v.fast.mokuro.gz")
        );
    }

    /// A shadowed archive keeps the files its exact stem names (rule v1 over the whole
    /// listing, as the walk reads it): the archive that shadows it is never given
    /// `v.mokuro` or `v.webp` to overwrite, and is owed nothing until the two are merged
    /// by hand. Where the host folds `V.cbz` and `v.cbz` into one entry, that one entry
    /// owns them (done, nothing owed): the same outcome, nothing written.
    #[test]
    fn a_shadowed_archives_files_are_never_the_winners() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path();
        let s = lib.join("S");
        bunko_store::fs::create_dir_all(&s).unwrap();
        bunko_store::fs::write(s.join("v.cbz"), b"zip").unwrap();
        bunko_store::fs::write(s.join("v.mokuro"), b"{}").unwrap();
        bunko_store::fs::write(s.join("v.webp"), b"cover").unwrap();
        bunko_store::fs::write(s.join("V.cbz"), b"zip").unwrap();
        let archives: Vec<String> = sidecar::list_names(&s)
            .unwrap()
            .into_iter()
            .filter(|n| sidecar::is_cbz_name(n))
            .collect();
        let winner = s.join(&archives[0]);
        let index = walk(lib, &rows(), &FileFacts, None, &KnownPages::new());
        let primary = &rows()[0];
        if archives.len() == 2 {
            assert_eq!(archives, ["V.cbz", "v.cbz"]);
            assert_eq!(listed_sidecar_paths(&winner, ".mokuro"), None);
            assert_eq!(sidecar::sibling(&winner, ".webp"), None);
            assert!(!crate::thumbs::needs_thumbnail(&winner));
            assert!(index.volumes.is_empty(), "{:?}", index.volumes);
            assert!(!still_owed(lib, "S/V.cbz", primary, false));
            assert!(!still_owed(lib, "S/V.cbz", primary, true));
            assert!(!still_owed(lib, "S/v.cbz", primary, false));
        } else {
            assert_eq!(
                listed_sidecar_paths(&winner, ".mokuro").map(|(p, _)| p),
                Some(s.join("v.mokuro"))
            );
            assert!(!still_owed(
                lib,
                &format!("S/{}", archives[0]),
                primary,
                false
            ));
            // Only the layer is owed, written beside the listed primary.
            let owed: Vec<String> = index
                .volumes
                .values()
                .flat_map(|v| v.rows.iter().map(|r| r.to_string()))
                .collect();
            assert_eq!(owed, ["g-2"]);
        }
        assert_eq!(bunko_store::fs::read(s.join("v.mokuro")).unwrap(), b"{}");
        assert_eq!(bunko_store::fs::read(s.join("v.webp")).unwrap(), b"cover");
    }
}
