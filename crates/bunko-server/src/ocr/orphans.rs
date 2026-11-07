//! `<stem>_<n>.mokuro` files the collector wrote before 0.8 (`unique_path`): a result
//! whose destination was taken went beside it under a numbered name, which no reader
//! ever matches. 0.8 never writes one (`collect::reconcile_existing`); this sweeps the
//! ones a library already has, when they are provably Bunko's: sealed
//! ([`bunko_layout::seal`]), or recorded in `ocr_sidecars` at their own path (the 0.7
//! files, written before the seal), and of the volume they are numbered after (their
//! `volume` names it; a row, if any, is keyed by it). An orphan takes the canonical name when that is
//! free; when it is taken the orphan is a stale second copy and goes. Anything else
//! that looks numbered stays where it is, counted.
//!
//! Names follow sidecar names rule v1 ([`sidecar_names`]): only a `.cbz` anchors, and a
//! numbered name that is itself any archive's stem (`Vol_1.cbz`) is a volume, never an
//! orphan.
//!
//! Whether the canonical name is taken is read from the folder listing, never by
//! opening the name, so every host does the same: taken when rule v1 already gives the
//! volume that file (the orphan is a stale copy and goes); left in place, counted, when
//! the folder lists the name only in another spelling rule v1 does not give the volume
//! (`Vol.FAST.mokuro` for `Vol.fast.mokuro`: moving the orphan there would make a second
//! spelling, removing it would lose the OCR), and when a case-variant group freezes the
//! orphan, its canonical name, its volume or a folder above them
//! ([`bunko_core::fold_groups`]).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use bunko_core::sidecar_names::{self, Folder};
use bunko_db::Database;
use bunko_library::sidecar;
use bunko_store::{Durability, Mode, Origin, Version, WriteIntent, fs};
use parking_lot::Mutex;

use super::types::{PathLocks, mtime_ns, rel_of};

/// A folder's archive stems, as [`orphan_target`] needs them.
pub struct OrphanStems<'a> {
    /// `.cbz` stems exactly as written: the collector numbered the archive's own stem.
    cbz: HashSet<&'a str>,
    /// Folded stems of every archive extension ([`sidecar_names::ARCHIVE_EXTENSIONS`]).
    archives: HashSet<String>,
}

impl<'a> OrphanStems<'a> {
    pub fn new(names: &[&'a str]) -> Self {
        let mut stems = OrphanStems {
            cbz: HashSet::new(),
            archives: HashSet::new(),
        };
        for &name in names {
            if let Some(stem) = sidecar_names::cbz_stem(name) {
                stems.cbz.insert(stem);
            }
            if let Some(stem) = sidecar_names::ARCHIVE_EXTENSIONS
                .iter()
                .find_map(|ext| sidecar_names::strip_suffix_ci(name, ext))
            {
                stems.archives.insert(sidecar_names::fold(stem));
            }
        }
        stems
    }
}

/// The canonical name of a numbered sidecar the collector wrote: `Vol_1.mokuro` ->
/// `Vol.mokuro` beside `Vol.cbz` (a primary), `Vol.paddle-manga_2.mokuro` ->
/// `Vol.paddle-manga.mokuro` (a layer). `None` for anything else.
pub fn orphan_target(name: &str, stems: &OrphanStems<'_>) -> Option<String> {
    orphan_of(name, stems).map(|(canonical, _)| canonical)
}

/// [`orphan_target`] with the stem of the `.cbz` it belongs to.
fn orphan_of<'n>(name: &'n str, stems: &OrphanStems<'_>) -> Option<(String, &'n str)> {
    let (numbered, _) = sidecar_names::split_mokuro(name)?;
    let (base, n) = numbered.rsplit_once('_')?;
    // `unique_path` counted from 1, never with a leading zero.
    let counted = n.bytes().all(|b| b.is_ascii_digit()) && !n.is_empty() && !n.starts_with('0');
    if base.is_empty() || !counted || stems.archives.contains(&sidecar_names::fold(numbered)) {
        return None;
    }
    let canonical = format!("{base}{}", &name[numbered.len()..]);
    if stems.cbz.contains(base) {
        return Some((canonical, base));
    }
    let (stem, id) = base.rsplit_once('.')?;
    (bunko_core::generations::is_layer_id(id) && stems.cbz.contains(stem))
        .then_some((canonical, stem))
}

/// Is `canonical` (a name [`orphan_target`] gave) taken in `names`, in any spelling rule
/// v1 gives the same file (`Vol.mokuro.gz` for `Vol.mokuro`, `vol.mokuro` beside
/// `Vol.cbz` alone)?
fn canonical_taken(canonical: &str, names: &[&str]) -> bool {
    if names.contains(&canonical) {
        return true;
    }
    let Some((base, _)) = sidecar_names::split_mokuro(canonical) else {
        return false;
    };
    let anchor = |stem: &str| {
        names
            .iter()
            .copied()
            .find(|n| sidecar_names::cbz_stem(n) == Some(stem))
    };
    let folder = Folder::new(names);
    if let Some(cbz) = anchor(base) {
        return folder.primary_of(cbz).is_some();
    }
    match base.rsplit_once('.') {
        Some((stem, id)) => anchor(stem).is_some_and(|cbz| folder.layer_of(cbz, id).is_some()),
        None => false,
    }
}

/// What one sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Took their canonical name, which was free.
    pub moved: u64,
    /// Removed: the canonical name was taken.
    pub removed: u64,
    /// Left in place: not provably Bunko's.
    pub unproven: u64,
    /// Of `unproven`: unchanged since judged, so not read again ([`Unproven`]).
    pub remembered: u64,
    /// Left in place: its canonical name is listed in another spelling that is not
    /// the volume's file, or a case-variant group freezes it.
    pub left: u64,
    /// Locked, or a write failed: tried again after the next walk.
    pub failed: u64,
}

/// The orphans judged unproven, by library path, with the stat they had then: the sweep
/// runs after every walk while one is left, and need not read (on a cloud library,
/// download) an unchanged file again.
#[derive(Default)]
pub struct Unproven(pub Mutex<HashMap<String, Stamp>>);

type Stamp = (u64, i128, Option<Version>);

enum Swept {
    Moved,
    Removed,
    Unproven,
    /// Its canonical name is another file's spelling, or it is frozen.
    Left,
    /// No longer there, or no longer an orphan (the folder changed since the walk).
    Gone,
}

/// Sweep `orphans` (library-relative paths a walk found, [`super::owed::OwedIndex`]),
/// one at a time with `pace` between them. Every name is judged again against its
/// folder as it is now.
pub fn sweep(
    library: &Path,
    db: Option<&Database>,
    locks: &PathLocks,
    orphans: &[String],
    pace: Duration,
) -> SweepReport {
    sweep_with(library, db, locks, orphans, pace, &Unproven::default())
}

/// [`sweep`], skipping the orphans `seen` judged unproven while they are unchanged.
pub fn sweep_with(
    library: &Path,
    db: Option<&Database>,
    locks: &PathLocks,
    orphans: &[String],
    pace: Duration,
    seen: &Unproven,
) -> SweepReport {
    seen.0.lock().retain(|rel, _| orphans.contains(rel));
    let mut report = SweepReport::default();
    let mut paced = false;
    for rel in orphans {
        let stamp = fs::metadata(library.join(rel))
            .ok()
            .map(|m| (m.len(), mtime_ns(&m), m.version().cloned()));
        if stamp.is_some() && seen.0.lock().get(rel) == stamp.as_ref() {
            report.unproven += 1;
            report.remembered += 1;
            continue;
        }
        if std::mem::replace(&mut paced, true) && !pace.is_zero() {
            std::thread::sleep(pace);
        }
        let swept = sweep_one(library, db, locks, rel);
        match (&swept, stamp) {
            (Ok(Swept::Unproven), Some(stamp)) => {
                seen.0.lock().insert(rel.clone(), stamp);
            }
            _ => {
                seen.0.lock().remove(rel);
            }
        }
        match swept {
            Ok(Swept::Moved) => report.moved += 1,
            Ok(Swept::Removed) => report.removed += 1,
            Ok(Swept::Unproven) => report.unproven += 1,
            Ok(Swept::Left) => report.left += 1,
            Ok(Swept::Gone) => {}
            Err(e) => {
                report.failed += 1;
                tracing::warn!("could not sweep {rel}: {e}");
            }
        }
    }
    report
}

fn sweep_one(
    library: &Path,
    db: Option<&Database>,
    locks: &PathLocks,
    rel: &str,
) -> Result<Swept, String> {
    let path = library.join(rel);
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return Ok(Swept::Gone);
    };
    if !fs::is_file(&path) || super::owed::merge_busy(&path) {
        return Ok(Swept::Gone);
    }
    let Some((entries, groups)) = bunko_library::fsutil::listed_groups(dir, false) else {
        return Ok(Swept::Gone);
    };
    let listed: Vec<String> = entries
        .iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(n, _)| n.clone())
        .collect();
    let names: Vec<&str> = listed.iter().map(String::as_str).collect();
    let Some((canonical, anchor)) = orphan_of(name, &OrphanStems::new(&names)) else {
        return Ok(Swept::Gone);
    };
    // Never in a frozen case-variant group: the orphan, the name it would take (and
    // with it its volume), or any folder above.
    if groups.is_frozen(name)
        || groups.is_frozen(&canonical)
        || bunko_library::fsutil::frozen_path(library, dir).is_some()
    {
        return Ok(Swept::Left);
    }
    // Bunko's own: the content seal, or (0.7, before the seal) its provenance row. And
    // the collector's numbering of THIS volume: a numbered name alone is not, since a
    // reader renaming `Vol (2)` to `Vol_2` uploads the new main (sealed again, C5) before
    // it moves the archive. The collector wrote the archive's stem into `volume` (a
    // reader writes the title it renames to) and keyed the row by that archive.
    let row = db.and_then(|db| db.get_ocr_sidecar(rel).ok().flatten());
    let bytes = sidecar::read_sidecar_bytes(&path).ok();
    let sealed = bytes.as_deref().is_some_and(bunko_layout::seal::is_sealed);
    let names_anchor = bytes
        .as_deref()
        .and_then(|b| {
            serde_json::from_slice::<serde_json::Value>(super::upgrade::strip_bom(b)).ok()
        })
        .is_some_and(|d| d.get("volume").and_then(|v| v.as_str()) == Some(anchor));
    let anchor_rel = names
        .iter()
        .find(|n| sidecar_names::cbz_stem(n) == Some(anchor))
        .and_then(|cbz| rel_of(library, &dir.join(cbz)));
    let row_ours = row
        .as_ref()
        .is_none_or(|r| Some(&r.volume_key) == anchor_rel.as_ref());
    if !names_anchor || !row_ours || (!sealed && row.is_none()) {
        return Ok(Swept::Unproven);
    }
    let target = dir.join(&canonical);
    let locked = |p: &Path| format!("{} is locked by a WebDAV write", p.display());
    let _orphan_guard = locks.try_lock(&path).ok_or_else(|| locked(&path))?;
    let _target_guard = locks.try_lock(&target).ok_or_else(|| locked(&target))?;
    // Decided again under both locks, from a fresh listing: a client's upload that
    // reached either name, in any spelling, since the first listing is seen (no
    // WebDAV write lands while the locks are held).
    let Some((entries, groups)) = bunko_library::fsutil::listed_groups(dir, false) else {
        return Ok(Swept::Gone);
    };
    let listed: Vec<String> = entries
        .iter()
        .filter(|(_, is_dir)| !is_dir)
        .map(|(n, _)| n.clone())
        .collect();
    let names: Vec<&str> = listed.iter().map(String::as_str).collect();
    if !names.contains(&name) {
        return Ok(Swept::Gone);
    }
    if orphan_of(name, &OrphanStems::new(&names)).map(|(c, _)| c) != Some(canonical.clone())
        || groups.is_frozen(name)
        || groups.is_frozen(&canonical)
    {
        return Ok(Swept::Left);
    }
    // From the listing: rule v1 gives the volume that file already (a stale copy), or
    // the name is listed in another spelling that is not the volume's, or a folder
    // takes it (left: no second spelling, nothing lost). Never by opening `target`,
    // which a host that folds names would answer with that other file.
    let folded = bunko_core::names::fold_name(&canonical);
    let swept = if canonical_taken(&canonical, &names) {
        Swept::Removed
    } else if entries
        .iter()
        .any(|(n, _)| *n == canonical || bunko_core::names::fold_name(n) == folded)
    {
        return Ok(Swept::Left);
    } else {
        // The orphan's exact bytes, under the name a reader matches, free in the
        // listing taken under the locks.
        let bytes = sidecar::read_capped(&path, sidecar::MAX_SIDECAR_BYTES)
            .map_err(|e| format!("could not read it: {e}"))?;
        let intent = WriteIntent::new(Mode::CreateNew, Origin::Upgrade, Durability::Sync);
        let tmp = target.with_file_name(format!(".{canonical}.orphan.tmp"));
        super::upgrade::stage_and_publish(&tmp, &target, &bytes, &intent)
            .map_err(|e| format!("could not publish it as {canonical}: {e}"))?;
        if let (Some(db), Some(row), Some(target_rel)) = (db, &row, rel_of(library, &target)) {
            let mut moved = row.clone();
            moved.sidecar_path = target_rel;
            let _ = db.record_ocr_sidecar(&moved);
        }
        Swept::Moved
    };
    // On a cloud library the store's remove deletes a version its ledger proves Bunko's
    // and sends anything else to the provider's trash.
    fs::remove_file(&path).map_err(|e| format!("could not remove it: {e}"))?;
    if let Some(db) = db {
        let _ = db.forget_ocr_sidecar(rel);
    }
    match swept {
        Swept::Moved => {
            tracing::info!("Renamed {rel}, the OCR an older Bunko numbered, to {canonical}")
        }
        _ => tracing::info!(
            "Removed {rel}: a second copy of {canonical}, numbered by an older Bunko"
        ),
    }
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, listed: &[&str]) -> Option<String> {
        orphan_target(name, &OrphanStems::new(listed))
    }

    #[test]
    fn numbered_primaries_and_layers_map_to_their_canonical_names() {
        let listed = ["Vol.cbz", "Vol 1.5.cbz", "Vol 2.cbz"];
        assert_eq!(
            target("Vol_1.mokuro", &listed).as_deref(),
            Some("Vol.mokuro")
        );
        assert_eq!(
            target("Vol_12.mokuro.gz", &listed).as_deref(),
            Some("Vol.mokuro.gz")
        );
        assert_eq!(
            target("Vol.paddle-manga_2.mokuro", &listed).as_deref(),
            Some("Vol.paddle-manga.mokuro")
        );
        assert_eq!(
            target("Vol 1.5_1.mokuro", &listed).as_deref(),
            Some("Vol 1.5.mokuro")
        );
        assert_eq!(
            target("Vol 2.fast_3.MOKURO", &listed).as_deref(),
            Some("Vol 2.fast.MOKURO"),
            "the suffix keeps its spelling"
        );
    }

    #[test]
    fn real_volumes_and_other_names_are_never_orphans() {
        // `Vol_1` is a volume of its own, in any archive format.
        assert_eq!(target("Vol_1.mokuro", &["Vol.cbz", "Vol_1.cbz"]), None);
        assert_eq!(target("Vol_1.mokuro", &["Vol.cbz", "vol_1.CBR"]), None);
        // Not the collector's numbering: a leading zero, no number, no archive.
        assert_eq!(target("Vol_01.mokuro", &["Vol.cbz"]), None);
        assert_eq!(target("Vol_0.mokuro", &["Vol.cbz"]), None);
        assert_eq!(target("Vol_.mokuro", &["Vol.cbz"]), None);
        assert_eq!(target("Vol_1a.mokuro", &["Vol.cbz"]), None);
        assert_eq!(target("Other_1.mokuro", &["Vol.cbz"]), None);
        assert_eq!(target("Vol.mokuro", &["Vol.cbz"]), None);
        assert_eq!(target("Vol_1.webp", &["Vol.cbz"]), None);
        // Only `.cbz` anchors (rule v1 #2); the collector numbered the archive's own stem.
        assert_eq!(target("Vol_1.mokuro", &["Vol.zip"]), None);
        assert_eq!(target("vol_1.mokuro", &["Vol.cbz"]), None);
        // A layer id the rule does not allow.
        assert_eq!(target("Vol.Fast_1.mokuro", &["Vol.cbz"]), None);
    }

    /// An orphan judged unproven is not read again until it changes: the sweep runs
    /// after every walk while one is left.
    #[test]
    fn an_unproven_orphan_is_read_again_only_once_it_changes() {
        // unmounted: a plain temp library.
        #![allow(clippy::disallowed_methods)]
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path();
        std::fs::create_dir(lib.join("A")).unwrap();
        std::fs::write(lib.join("A/V1.cbz"), b"PK").unwrap();
        let unsealed = r#"{"volume":"V1","ocr_engine":{"id":"hayai-nova"},"pages":[]}"#;
        std::fs::write(lib.join("A/V1_1.mokuro"), unsealed).unwrap();
        let locks: PathLocks = std::sync::Arc::new(bunko_library::service::NoPathLocks);
        let seen = Unproven::default();
        let orphans = ["A/V1_1.mokuro".to_string()];
        let run = || sweep_with(lib, None, &locks, &orphans, Duration::ZERO, &seen);

        let first = run();
        assert_eq!((first.unproven, first.remembered), (1, 0));
        let second = run();
        assert_eq!(
            (second.unproven, second.remembered),
            (1, 1),
            "not read again"
        );

        // Changed (now sealed): judged again, and taken to its canonical name.
        let mut v = bunko_layout::json::Value::parse(unsealed).unwrap();
        let sealed = bunko_layout::seal::seal(&mut v).unwrap();
        std::fs::write(lib.join("A/V1_1.mokuro"), &sealed).unwrap();
        let third = run();
        assert_eq!((third.moved, third.unproven, third.remembered), (1, 0, 0));
        assert_eq!(
            std::fs::read_to_string(lib.join("A/V1.mokuro")).unwrap(),
            sealed
        );
        assert!(seen.0.lock().is_empty(), "forgotten once swept");
    }

    /// Whether the canonical name is free is read from the listing, the same on every
    /// host: `Vol.FAST.mokuro` is listed, and sidecar names rule 2 (ids folded) makes it
    /// the volume's `fast` layer, so the canonical name is taken and the sealed
    /// `Vol.fast_1.mokuro` is a stale second copy that goes, exactly as beside a listed
    /// `Vol.fast.mokuro`. And nothing moves in a frozen case-variant group.
    #[test]
    fn a_canonical_name_listed_in_another_spelling_leaves_the_orphan() {
        // unmounted: a plain temp library.
        #![allow(clippy::disallowed_methods)]
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path();
        std::fs::create_dir(lib.join("A")).unwrap();
        std::fs::write(lib.join("A/Vol.cbz"), b"PK").unwrap();
        std::fs::write(lib.join("A/Vol.FAST.mokuro"), b"{\"pages\":[]}").unwrap();
        let mut v = bunko_layout::json::Value::parse(
            r#"{"volume":"Vol","ocr_engine":{"id":"hayai"},"pages":[]}"#,
        )
        .unwrap();
        let sealed = bunko_layout::seal::seal(&mut v).unwrap();
        std::fs::write(lib.join("A/Vol.fast_1.mokuro"), &sealed).unwrap();
        let locks: PathLocks = std::sync::Arc::new(bunko_library::service::NoPathLocks);
        let orphans = ["A/Vol.fast_1.mokuro".to_string()];
        let report = sweep(lib, None, &locks, &orphans, Duration::ZERO);
        assert_eq!(
            (report.moved, report.removed, report.left),
            (0, 1, 0),
            "{report:?}"
        );
        let mut names: Vec<String> = std::fs::read_dir(lib.join("A"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["Vol.FAST.mokuro", "Vol.cbz"]);
        assert_eq!(
            std::fs::read_to_string(lib.join("A/Vol.FAST.mokuro")).unwrap(),
            "{\"pages\":[]}"
        );
        std::fs::write(lib.join("A/Vol.fast_1.mokuro"), &sealed).unwrap();

        // A frozen group: `Vol.cbz` beside `vol.cbz` (where the disk lists both).
        std::fs::remove_file(lib.join("A/Vol.FAST.mokuro")).unwrap();
        if std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lib.join("A/vol.cbz"))
            .is_ok()
        {
            let report = sweep(lib, None, &locks, &orphans, Duration::ZERO);
            assert_eq!((report.moved, report.removed, report.left), (0, 0, 1));
            assert!(lib.join("A/Vol.fast_1.mokuro").is_file());
            assert!(
                !std::fs::read_dir(lib.join("A"))
                    .unwrap()
                    .any(|e| e.unwrap().file_name() == "Vol.fast.mokuro")
            );
        }
    }

    /// A lock whose first `try_lock` lands a client's upload: a PUT that finished
    /// between the sweep's first listing and its locks.
    struct Racing {
        upload: std::path::PathBuf,
        landed: std::sync::atomic::AtomicBool,
    }

    impl bunko_library::service::PathWriteLocks for Racing {
        fn try_lock(&self, _path: &Path) -> Option<Box<dyn Send>> {
            #[allow(clippy::disallowed_methods)] // the client's upload, on the plain disk
            if !self.landed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                std::fs::write(&self.upload, b"the client's upload").unwrap();
            }
            Some(Box::new(()))
        }
    }

    /// The sweep decides again under its locks, from a fresh listing: an upload that
    /// reached the canonical name meanwhile (exactly, or in another spelling) stays,
    /// byte for byte, and Bunko makes no second spelling beside it: the orphan, a
    /// second copy now, goes. The same on every host.
    #[test]
    fn an_upload_landing_before_the_locks_is_kept() {
        // unmounted: a plain temp library.
        #![allow(clippy::disallowed_methods)]
        for upload in ["Vol.mokuro", "vol.MOKURO"] {
            let dir = tempfile::tempdir().unwrap();
            let lib = dir.path();
            std::fs::create_dir(lib.join("S")).unwrap();
            std::fs::write(lib.join("S/Vol.cbz"), b"PK").unwrap();
            let mut v = bunko_layout::json::Value::parse(
                r#"{"volume":"Vol","ocr_engine":{"id":"hayai"},"pages":[]}"#,
            )
            .unwrap();
            let sealed = bunko_layout::seal::seal(&mut v).unwrap();
            std::fs::write(lib.join("S/Vol_1.mokuro"), &sealed).unwrap();
            let locks: PathLocks = std::sync::Arc::new(Racing {
                upload: lib.join("S").join(upload),
                landed: Default::default(),
            });
            let orphans = ["S/Vol_1.mokuro".to_string()];
            let report = sweep(lib, None, &locks, &orphans, Duration::ZERO);
            assert_eq!(
                (report.moved, report.removed),
                (0, 1),
                "{upload}: {report:?}"
            );
            let names: Vec<String> = std::fs::read_dir(lib.join("S"))
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            let mut names = names;
            names.sort();
            assert_eq!(names, ["Vol.cbz", upload], "{upload}");
            assert_eq!(
                std::fs::read(lib.join("S").join(upload)).unwrap(),
                b"the client's upload",
                "{upload}"
            );
        }
    }

    #[test]
    fn a_canonical_name_is_taken_in_any_spelling_of_the_same_file() {
        assert!(canonical_taken("Vol.mokuro", &["Vol.cbz", "Vol.mokuro"]));
        assert!(canonical_taken("Vol.mokuro", &["Vol.cbz", "Vol.mokuro.gz"]));
        assert!(canonical_taken("Vol.mokuro", &["Vol.cbz", "vol.MOKURO"]));
        assert!(canonical_taken(
            "Vol.fast.mokuro",
            &["Vol.cbz", "vol.fast.mokuro.gz"]
        ));
        assert!(!canonical_taken("Vol.mokuro", &["Vol.cbz", "Vol_1.mokuro"]));
        assert!(!canonical_taken(
            "Vol.fast.mokuro",
            &["Vol.cbz", "Vol.mokuro", "Vol.slow.mokuro"]
        ));
    }
}
