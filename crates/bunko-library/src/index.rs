//! The shared, cached index of the library tree (`library_index.py`): which
//! series/volumes exist, their covers and OCR layers, what still needs OCR or
//! a thumbnail. Feeds the catalog, home/health counts and the OCR queue.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bunko_core::sidecar_names::{self, Folder, LayerClass};
use parking_lot::Mutex;

use crate::fsutil;
use crate::sidecar::split_layer_sidecar;

/// A scan faster than this is simply redone after an invalidation.
pub const SLOW_SCAN: Duration = Duration::from_millis(100);
/// A slow scan's snapshot is served after an invalidation until it is this
/// many scan-durations old (a burst of changes costs one walk, not one each).
pub const RESCAN_FACTOR: f64 = 4.0;
/// Default snapshot time-to-live.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

/// One logical volume (a `.cbz` stem).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSnapshot {
    pub name: String,
    /// The archive `<name>.cbz` exists, its extension in any case (0.5.3 asked for
    /// exactly `.cbz`, so a `Vol 1.CBZ` was a volume with no archive: never pending OCR
    /// or a cover).
    pub has_cbz: bool,
    pub has_mokuro: bool,
    pub has_mokuro_gz: bool,
    /// `"<series>/<name>.webp"` when that file exists.
    pub cover: Option<String>,
    /// Layer ids OBSERVED beside the archive (sorted), not what is configured.
    pub sidecars: Vec<String>,
}

/// One series folder (any depth; name = relative `/` path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesSnapshot {
    pub name: String,
    pub cover: Option<String>,
    pub volumes: Vec<VolumeSnapshot>,
}

/// An immutable scan result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibrarySnapshot {
    pub series: Vec<SeriesSnapshot>,
    /// `(series, volume)` of every archive without a primary sidecar, in walk
    /// order (membership only; not the OCR queue's order).
    pub pending_ocr: Vec<(String, String)>,
    pub pending_thumbnails: usize,
}

impl LibrarySnapshot {
    pub fn series_by_name(&self, name: &str) -> Option<&SeriesSnapshot> {
        self.series.iter().find(|series| series.name == name)
    }

    /// Rough heap footprint, for cache accounting.
    pub fn approx_bytes(&self) -> usize {
        let volume = |v: &VolumeSnapshot| {
            64 + v.name.len()
                + v.cover.as_ref().map_or(0, String::len)
                + v.sidecars.iter().map(|s| 24 + s.len()).sum::<usize>()
        };
        self.series
            .iter()
            .map(|s| {
                64 + s.name.len()
                    + s.cover.as_ref().map_or(0, String::len)
                    + s.volumes.iter().map(volume).sum::<usize>()
            })
            .sum::<usize>()
            + self
                .pending_ocr
                .iter()
                .map(|(a, b)| 48 + a.len() + b.len())
                .sum::<usize>()
    }
}

fn scan_dir(
    dir: &Path,
    series_name: &str,
    snapshot: &mut LibrarySnapshot,
) -> Vec<(String, PathBuf)> {
    // `os.walk` skips directories it cannot list.
    let Ok(entries) = fsutil::list_dir(dir) else {
        return Vec::new();
    };
    // Folders and archives another listed spelling shadows are never reached: not
    // walked, not volumes. Their sidecars stay in the listing rule v1 reads.
    let losers = fsutil::case_losers(dir, entries.iter().map(|(n, _)| n.as_str()));
    let mut subdirs = Vec::new();
    let mut filenames: HashSet<String> = HashSet::new();
    for (name, path) in entries {
        if fsutil::is_dir(&path) {
            subdirs.push((name, path));
        } else {
            filenames.insert(name);
        }
    }
    subdirs.retain(|(name, _)| !name.starts_with('.') && !losers.contains(name));
    subdirs.sort_by(|a, b| a.0.cmp(&b.0));
    if series_name.starts_with('.') {
        return subdirs;
    }

    let mut sorted_names: Vec<&String> = filenames.iter().collect();
    sorted_names.sort();
    let volume_names: BTreeSet<String> = sorted_names
        .iter()
        .filter(|name| crate::pyunicode::lower(name).ends_with(".cbz") && !losers.contains(**name))
        .map(|name| name.chars().take(name.chars().count() - 4).collect())
        .collect();
    let mut layers_by_stem: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for name in &filenames {
        let Some((stem, layer)) = split_layer_sidecar(name) else {
            continue;
        };
        // A personal correction is one account's own, never the volume's (D27).
        if volume_names.contains(&format!("{stem}.{layer}"))
            || sidecar_names::layer_class(layer) == LayerClass::Personal
        {
            continue;
        }
        layers_by_stem.entry(stem).or_default().insert(layer);
    }

    // Stems with an archive, its extension in any case (`Vol 1.CBZ`), to its name.
    let archive_stems: HashMap<&str, &str> = filenames
        .iter()
        .filter(|name| crate::sidecar::is_cbz_name(name) && !losers.contains(*name))
        .map(|name| (&name[..name.len() - 4], name.as_str()))
        .collect();
    // What rule v1 reads: the names a request reaches (a shadowed spelling is none).
    let listed: Vec<&str> = sorted_names
        .iter()
        .map(|name| name.as_str())
        .filter(|name| !losers.contains(*name))
        .collect();
    let folder = Folder::new(&listed);
    let mut volumes = Vec::new();
    let mut series_cover = None;
    for volume_name in &volume_names {
        let has = |suffix: &str| filenames.contains(&format!("{volume_name}{suffix}"));
        let archive = archive_stems.get(volume_name.as_str());
        let has_cbz = archive.is_some();
        let mut has_mokuro = has(".mokuro");
        let mut has_mokuro_gz = has(".mokuro.gz");
        // Rule v1: the primary in another spelling of the stem (`V.MOKURO` on a
        // case-sensitive disk) is the volume's too; the OCR walk agrees.
        if !has_mokuro
            && !has_mokuro_gz
            && let Some(primary) = archive.and_then(|name| folder.primary_of(name))
        {
            match sidecar_names::split_mokuro(primary) {
                Some((_, true)) => has_mokuro_gz = true,
                _ => has_mokuro = true,
            }
        }
        // The cover and the marker in any spelling of the stem (rule v1: `V.WEBP`), as
        // the thumbnail worker finds them: listed under their own names.
        let files = archive
            .map(|name| folder.volume_files(name))
            .unwrap_or_default();
        let listed_as = |wanted: fn(&sidecar_names::VolumeFile) -> bool, suffix: &str| {
            let exact = format!("{volume_name}{suffix}");
            if filenames.contains(&exact) {
                return Some(exact);
            }
            files
                .iter()
                .find(|(_, f)| wanted(f))
                .map(|(n, _)| (*n).to_string())
        };
        let cover_name = listed_as(|f| matches!(f, sidecar_names::VolumeFile::Cover), ".webp");
        let has_webp = cover_name.is_some();
        let has_nocover = listed_as(
            |f| matches!(f, sidecar_names::VolumeFile::NoCover),
            ".nocover",
        )
        .is_some();
        let cover = cover_name.map(|name| format!("{series_name}/{name}"));
        if series_cover.is_none() {
            series_cover.clone_from(&cover);
        }
        if has_cbz && !has_mokuro && !has_mokuro_gz {
            snapshot
                .pending_ocr
                .push((series_name.to_owned(), volume_name.clone()));
        }
        if has_cbz && !has_webp && !has_nocover {
            snapshot.pending_thumbnails += 1;
        }
        volumes.push(VolumeSnapshot {
            name: volume_name.clone(),
            has_cbz,
            has_mokuro,
            has_mokuro_gz,
            cover,
            sidecars: layers_by_stem
                .get(volume_name.as_str())
                .map(|set| set.iter().map(|s| (*s).to_owned()).collect())
                .unwrap_or_default(),
        });
    }
    if !volumes.is_empty() {
        snapshot.series.push(SeriesSnapshot {
            name: series_name.to_owned(),
            cover: series_cover,
            volumes,
        });
    }
    subdirs
}

/// Scan the whole library tree once (`_scan_library`): a top-down walk,
/// sibling directories by name, hidden directories (and their subtrees)
/// skipped, symlinked directories listed but not descended.
pub fn scan_library(library_path: &Path) -> LibrarySnapshot {
    let mut snapshot = LibrarySnapshot::default();
    if !fsutil::is_dir(library_path) {
        return snapshot;
    }
    // (directory, relative name); the root's name is "." (never a series).
    let mut stack: Vec<(PathBuf, String)> = vec![(library_path.to_path_buf(), ".".to_owned())];
    while let Some((dir, name)) = stack.pop() {
        let subdirs = scan_dir(&dir, &name, &mut snapshot);
        for (child, path) in subdirs.into_iter().rev() {
            if bunko_store::fs::symlink_metadata(&path)
                .is_ok_and(|meta| meta.file_type().is_symlink())
            {
                continue;
            }
            let child_name = if name == "." {
                child
            } else {
                format!("{name}/{child}")
            };
            stack.push((path, child_name));
        }
    }
    snapshot
}

/// The series `name` (a relative `/` path) as [`scan_library`] would list it, reading
/// only its folder: `None` where the walk would not reach it (a hidden or symlinked
/// folder on the way) or would not list it (no volumes).
fn scan_one_series(library_path: &Path, name: &str) -> Option<SeriesSnapshot> {
    let mut dir = library_path.to_path_buf();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.starts_with('.') {
            return None;
        }
        dir.push(part);
        if bunko_store::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return None;
        }
    }
    if !fsutil::is_dir(&dir) {
        return None;
    }
    let mut snapshot = LibrarySnapshot::default();
    scan_dir(&dir, name, &mut snapshot);
    snapshot.series.pop()
}

struct CacheState {
    snapshot: Option<Arc<LibrarySnapshot>>,
    snapshot_time: Instant,
    stale: bool,
    /// Bumped by every [`LibraryIndexCache::invalidate`]: a change that lands while a
    /// scan runs leaves the result stale.
    changes: u64,
    /// The last scan's result, kept even when `snapshot` was dropped: what a reader gets
    /// while another reader's scan is in progress.
    last: Option<Arc<LibrarySnapshot>>,
    scan_duration: Duration,
    scans: u64,
}

/// Time-based cached scanner (`LibraryIndexCache`).
///
/// Once there is a snapshot, a read never walks the library: a stale or expired index
/// answers at once with the last snapshot and starts one background walk. (0.5.2 walked
/// inside the read, so a catalog or series view took as long as a whole-library scan
/// whenever the TTL had passed or a file had changed, which under OCR load is often.)
/// Only the very first read waits. [`get_snapshot_fresh`](Self::get_snapshot_fresh)
/// still waits, for a caller that must see a change it just made.
pub struct LibraryIndexCache(Arc<Inner>);

struct Inner {
    library_path: PathBuf,
    ttl: Duration,
    max_bytes: usize,
    state: Mutex<CacheState>,
    /// One scan at a time: a reader that must wait for a walk waits for the one in
    /// progress instead of starting its own.
    scanning: Mutex<()>,
    /// A background walk is queued or running (at most one at a time).
    refreshing: AtomicBool,
}

impl LibraryIndexCache {
    /// `max_bytes` is a budget for the retained snapshot: one larger than it
    /// is still served (the index cannot work without it) but logged, so an
    /// operator can see the library outgrew the configured cache.
    pub fn new(library_path: impl Into<PathBuf>, ttl: Duration, max_bytes: usize) -> Self {
        Self(Arc::new(Inner {
            library_path: library_path.into(),
            ttl,
            max_bytes,
            state: Mutex::new(CacheState {
                snapshot: None,
                snapshot_time: Instant::now(),
                stale: false,
                changes: 0,
                last: None,
                scan_duration: Duration::ZERO,
                scans: 0,
            }),
            scanning: Mutex::new(()),
            refreshing: AtomicBool::new(false),
        }))
    }

    pub fn library_path(&self) -> &Path {
        &self.0.library_path
    }

    /// The library changed: a cheap scan is redone (in the background) on the next
    /// read; a slow one keeps serving until it is `RESCAN_FACTOR` scans old.
    pub fn invalidate(&self) {
        let mut state = self.0.state.lock();
        state.stale = true;
        state.changes += 1;
        if state.scan_duration < SLOW_SCAN {
            state.snapshot = None;
        }
    }

    /// A recent snapshot; never waits for a walk once there is one.
    pub fn get_snapshot(&self) -> Arc<LibrarySnapshot> {
        self.get_snapshot_counted().0
    }

    /// `get_snapshot` and the scan count that produced it, read together.
    pub fn get_snapshot_counted(&self) -> (Arc<LibrarySnapshot>, u64) {
        if let Some(hit) = self.0.servable() {
            return hit;
        }
        let last = {
            let state = self.0.state.lock();
            state
                .last
                .as_ref()
                .map(|last| (Arc::clone(last), state.scans))
        };
        match last {
            Some(last) => {
                self.refresh_in_background();
                last
            }
            None => self.0.scan(false),
        }
    }

    /// A snapshot that includes every change [`invalidate`](Self::invalidate)d before
    /// this call, walking the library (or waiting for the walk in progress) if needed.
    pub fn get_snapshot_fresh(&self) -> Arc<LibrarySnapshot> {
        match self.0.fresh() {
            Some((snapshot, _)) => snapshot,
            None => self.0.scan(true).0,
        }
    }

    /// One series as [`get_snapshot`](Self::get_snapshot) would list it. Before the
    /// first walk has finished, only that series' folder is listed (and the walk starts
    /// in the background), so a series view right after startup does not wait for the
    /// whole library.
    pub fn series(&self, name: &str) -> Option<SeriesSnapshot> {
        let walked = self.0.state.lock().last.is_some();
        if walked {
            return self.get_snapshot().series_by_name(name).cloned();
        }
        self.refresh_in_background();
        scan_one_series(&self.0.library_path, name)
    }

    /// The last snapshot, however old, without ever scanning.
    pub fn cached_snapshot(&self) -> Option<Arc<LibrarySnapshot>> {
        self.0.state.lock().snapshot.clone()
    }

    /// How many scans have produced a snapshot.
    pub fn scans(&self) -> u64 {
        self.0.state.lock().scans
    }

    /// Start one background walk, unless one is already queued or running.
    fn refresh_in_background(&self) {
        if self.0.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let inner = Arc::clone(&self.0);
        let spawned = std::thread::Builder::new()
            .name("library-index".into())
            .spawn(move || {
                inner.scan(false);
                inner.refreshing.store(false, Ordering::Release);
            });
        if let Err(error) = spawned {
            self.0.refreshing.store(false, Ordering::Release);
            tracing::warn!(%error, "library index: could not start a background walk");
        }
    }
}

impl Inner {
    /// Walk the library (one walk at a time) and keep the result. A walk that finished
    /// while this one waited for the lock serves it instead: one that may be served
    /// (`fresh_only` false) or one with every change seen (`fresh_only` true).
    fn scan(&self, fresh_only: bool) -> (Arc<LibrarySnapshot>, u64) {
        let _scan = self.scanning.lock();
        let done = if fresh_only {
            self.fresh()
        } else {
            self.servable()
        };
        if let Some(hit) = done {
            return hit;
        }
        let changes = self.state.lock().changes;
        let started = Instant::now();
        let snapshot = Arc::new(scan_library(&self.library_path));
        let duration = started.elapsed();
        let bytes = snapshot.approx_bytes();
        if bytes > self.max_bytes {
            tracing::warn!(
                bytes,
                budget = self.max_bytes,
                "library index snapshot exceeds its cache budget"
            );
        }
        let mut state = self.state.lock();
        state.scan_duration = duration;
        state.snapshot_time = Instant::now();
        state.stale = state.changes != changes;
        // Changed while scanning: as `invalidate` would, a cheap scan is redone on the
        // next read (this one still answers the readers that waited for it).
        state.snapshot = (!state.stale || duration >= SLOW_SCAN).then(|| Arc::clone(&snapshot));
        state.last = Some(Arc::clone(&snapshot));
        state.scans += 1;
        (snapshot, state.scans)
    }

    /// The cached snapshot when it may be served as it is.
    fn servable(&self) -> Option<(Arc<LibrarySnapshot>, u64)> {
        let state = self.state.lock();
        let snapshot = state.snapshot.as_ref()?;
        let age = state.snapshot_time.elapsed();
        let fresh = !state.stale && age < self.ttl;
        let slow_and_recent =
            state.stale && age.as_secs_f64() < RESCAN_FACTOR * state.scan_duration.as_secs_f64();
        (fresh || slow_and_recent).then(|| (Arc::clone(snapshot), state.scans))
    }

    /// The cached snapshot when no change has landed since it was taken.
    fn fresh(&self) -> Option<(Arc<LibrarySnapshot>, u64)> {
        let state = self.state.lock();
        let snapshot = state.snapshot.as_ref()?;
        (!state.stale).then(|| (Arc::clone(snapshot), state.scans))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sidecar names rule v1: a primary in another spelling of the stem (`V.MOKURO`,
    /// `v.mokuro.gz`) means the volume is not pending OCR. (Two archives whose stems fold
    /// alike are bunko-core's synthetic-listing vectors.)
    #[test]
    fn a_primary_in_any_spelling_is_not_pending() {
        let dir = tempfile::tempdir().unwrap();
        let series = dir.path().join("S");
        bunko_store::fs::create_dir(&series).unwrap();
        for name in [
            "A.cbz",
            "A.MOKURO",
            "B.cbz",
            "b.mokuro.gz",
            "C.cbz",
            "D.cbz",
            "D.mokuro",
        ] {
            bunko_store::fs::write(series.join(name), b"{}").unwrap();
        }
        let snapshot = scan_library(dir.path());
        let pending: Vec<&str> = snapshot
            .pending_ocr
            .iter()
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(pending, ["C"]);
        let volumes = &snapshot.series[0].volumes;
        let flags = |name: &str| {
            let v = volumes.iter().find(|v| v.name == name).unwrap();
            (v.has_mokuro, v.has_mokuro_gz)
        };
        assert_eq!(flags("A"), (true, false));
        assert_eq!(flags("B"), (false, true));
        assert_eq!(flags("C"), (false, false));
        assert_eq!(flags("D"), (true, false));
    }

    /// A cover or a no-cover marker in another spelling of the stem (`a.WEBP`) is the
    /// volume's on every host: the catalog shows it under its listed name, and the volume
    /// is not waiting for a thumbnail the worker will never make.
    #[test]
    fn a_cover_in_any_spelling_is_the_volumes() {
        let dir = tempfile::tempdir().unwrap();
        let series = dir.path().join("S");
        bunko_store::fs::create_dir(&series).unwrap();
        for name in ["A.cbz", "a.WEBP", "B.cbz", "b.NOCOVER", "C.cbz"] {
            bunko_store::fs::write(series.join(name), b"x").unwrap();
        }
        let snapshot = scan_library(dir.path());
        assert_eq!(snapshot.pending_thumbnails, 1);
        let covers: Vec<Option<&str>> = snapshot.series[0]
            .volumes
            .iter()
            .map(|v| v.cover.as_deref())
            .collect();
        assert_eq!(covers, [Some("S/a.WEBP"), None, None]);
        assert_eq!(snapshot.series[0].cover.as_deref(), Some("S/a.WEBP"));
    }

    /// D27: a personal correction is one account's own, never a layer of the volume.
    #[test]
    fn sidecars_exclude_personal_corrections() {
        let dir = tempfile::tempdir().unwrap();
        let series = dir.path().join("S");
        bunko_store::fs::create_dir(&series).unwrap();
        for name in [
            "V.cbz",
            "V.hayai.mokuro",
            "V.curated.mokuro",
            "V.edit.mokuro",
            "V.edit-2.mokuro.gz",
            "V.mine.mokuro",
        ] {
            bunko_store::fs::write(series.join(name), b"{}").unwrap();
        }
        let snapshot = scan_library(dir.path());
        assert_eq!(
            snapshot.series[0].volumes[0].sidecars,
            ["curated", "hayai", "mine"]
        );
    }
}
