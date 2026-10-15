//! The shared, cached index of the library tree (`library_index.py`): which
//! series/volumes exist, their covers and OCR layers, what still needs OCR or
//! a thumbnail. Feeds the catalog, home/health counts and the OCR queue.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    let mut subdirs = Vec::new();
    let mut filenames: HashSet<String> = HashSet::new();
    for (name, path) in entries {
        if fsutil::is_dir(&path) {
            subdirs.push((name, path));
        } else {
            filenames.insert(name);
        }
    }
    subdirs.retain(|(name, _)| !name.starts_with('.'));
    subdirs.sort_by(|a, b| a.0.cmp(&b.0));
    if series_name.starts_with('.') {
        return subdirs;
    }

    let mut sorted_names: Vec<&String> = filenames.iter().collect();
    sorted_names.sort();
    let volume_names: BTreeSet<String> = sorted_names
        .iter()
        .filter(|name| crate::pyunicode::lower(name).ends_with(".cbz"))
        .map(|name| name.chars().take(name.chars().count() - 4).collect())
        .collect();
    let mut layers_by_stem: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for name in &filenames {
        let Some((stem, layer)) = split_layer_sidecar(name) else {
            continue;
        };
        if volume_names.contains(&format!("{stem}.{layer}")) {
            continue;
        }
        layers_by_stem.entry(stem).or_default().insert(layer);
    }

    // Stems with an archive, its extension in any case (`Vol 1.CBZ`).
    let archive_stems: HashSet<&str> = filenames
        .iter()
        .filter(|name| crate::sidecar::is_cbz_name(name))
        .map(|name| &name[..name.len() - 4])
        .collect();
    let mut volumes = Vec::new();
    let mut series_cover = None;
    for volume_name in &volume_names {
        let has = |suffix: &str| filenames.contains(&format!("{volume_name}{suffix}"));
        let has_cbz = archive_stems.contains(volume_name.as_str());
        let has_mokuro = has(".mokuro");
        let has_mokuro_gz = has(".mokuro.gz");
        let has_webp = has(".webp");
        let cover = has_webp.then(|| format!("{series_name}/{volume_name}.webp"));
        if series_cover.is_none() {
            series_cover.clone_from(&cover);
        }
        if has_cbz && !has_mokuro && !has_mokuro_gz {
            snapshot
                .pending_ocr
                .push((series_name.to_owned(), volume_name.clone()));
        }
        if has_cbz && !has_webp && !has(".nocover") {
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
pub struct LibraryIndexCache {
    library_path: PathBuf,
    ttl: Duration,
    max_bytes: usize,
    state: Mutex<CacheState>,
    /// One scan at a time: while one runs, other readers of a stale index get the last
    /// snapshot at once (they wait only when there has never been one) instead of each
    /// walking the whole library, or waiting for a scan a busy background caller began.
    scanning: Mutex<()>,
}

impl LibraryIndexCache {
    /// `max_bytes` is a budget for the retained snapshot: one larger than it
    /// is still served (the index cannot work without it) but logged, so an
    /// operator can see the library outgrew the configured cache.
    pub fn new(library_path: impl Into<PathBuf>, ttl: Duration, max_bytes: usize) -> Self {
        Self {
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
        }
    }

    pub fn library_path(&self) -> &Path {
        &self.library_path
    }

    /// The library changed: a cheap scan is redone on the next read; a slow
    /// one keeps serving until it is `RESCAN_FACTOR` scans old.
    pub fn invalidate(&self) {
        let mut state = self.state.lock();
        state.stale = true;
        state.changes += 1;
        if state.scan_duration < SLOW_SCAN {
            state.snapshot = None;
        }
    }

    /// A recent snapshot, rescanning when stale.
    pub fn get_snapshot(&self) -> Arc<LibrarySnapshot> {
        self.get_snapshot_counted().0
    }

    /// `get_snapshot` and the scan count that produced it, read together.
    pub fn get_snapshot_counted(&self) -> (Arc<LibrarySnapshot>, u64) {
        if let Some(hit) = self.servable() {
            return hit;
        }
        let _scan = match self.scanning.try_lock() {
            Some(guard) => guard,
            None => {
                let state = self.state.lock();
                if let Some(last) = &state.last {
                    return (Arc::clone(last), state.scans);
                }
                drop(state);
                self.scanning.lock()
            }
        };
        // A scan that finished while this one waited serves it too.
        if let Some(hit) = self.servable() {
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

    /// The last snapshot, however old, without ever scanning.
    pub fn cached_snapshot(&self) -> Option<Arc<LibrarySnapshot>> {
        self.state.lock().snapshot.clone()
    }

    /// How many scans have produced a snapshot.
    pub fn scans(&self) -> u64 {
        self.state.lock().scans
    }
}
