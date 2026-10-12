//! Compiling, publishing and updating the reader's metadata files
//! (`metadata/service.py`), minus the timers: the server owns scheduling
//! (tokio) and calls [`MetadataService::recompile_all`] /
//! [`MetadataService::recompile_series`] when its debounce fires. The debounce
//! *policy* is here ([`DebouncePolicy`], [`Debouncer`]) so both agree.
//!
//! Locking mirrors 0.5.2: `pass_lock` guards each series' read-merge-publish
//! critical section (a full pass takes it PER SERIES so a waiting PUT gets in
//! between series), `full_pass_lock` keeps full passes from interleaving.
//! Hooks fire outside `pass_lock`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, MutexGuard};

use crate::compat::normalize_volume_title_key;
use crate::compiler::{SeriesFolder, compile_series_volumes, iter_series_folders, volume_key_for};
use crate::fsutil;
use crate::paths::{CATALOG_FILE_NAME, SERIES_FILE_NAME};
use crate::schema::{
    FACTLESS_UPDATED_AT, SeriesFacts, SeriesIndexData, VolumeEntry, dump_catalog_file,
    dump_series_file,
};
use crate::store::{CatalogSeriesRow, MetadataStore, SeriesFactsRow, StoreError};
use crate::update::{StoredSeries, merge_series_update, parse_series_update};

/// Timing policy of the metadata service (0.5.2's fixed constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebouncePolicy {
    /// Quiet period before a scheduled pass fires.
    pub debounce: Duration,
    /// A pending pass never drifts more than this past its first scheduling.
    pub max_debounce: Duration,
    /// Self-reschedule after a busy path, an unwritable file or an unreadable root.
    pub retry_delay: Duration,
    /// First full pass after startup.
    pub startup_delay: Duration,
    /// Periodic full pass (routed through the debounced path).
    pub periodic_rescan: Duration,
    /// How long a PUT waits for the pass lock before answering 503.
    pub update_lock_timeout: Duration,
}

impl Default for DebouncePolicy {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(10),
            max_debounce: Duration::from_secs(60),
            retry_delay: Duration::from_secs(5),
            startup_delay: Duration::from_secs(20),
            periodic_rescan: Duration::from_secs(6 * 3600),
            update_lock_timeout: Duration::from_secs(10),
        }
    }
}

/// `Retry-After` sent with the busy 503.
pub const BUSY_RETRY_AFTER_SECONDS: u64 = 30;

/// The capped-debounce state of ONE timer (the full pass, or one series'
/// timer keyed by `normalize_volume_title_key`).
///
/// Usage: on every trigger call [`Debouncer::schedule`] and (re)arm the timer
/// with the returned delay, cancelling the previous one; when the timer fires
/// call [`Debouncer::fired`] and run the pass.
#[derive(Debug, Clone, Default)]
pub struct Debouncer {
    deadline: Option<Instant>,
}

impl Debouncer {
    /// The delay to arm the timer with. `delay` overrides the base debounce
    /// (e.g. the 5 s retry); never past `first schedule + max_debounce`.
    pub fn schedule(
        &mut self,
        policy: &DebouncePolicy,
        now: Instant,
        delay: Option<Duration>,
    ) -> Duration {
        let deadline = *self.deadline.get_or_insert(now + policy.max_debounce);
        let base = delay.unwrap_or(policy.debounce);
        base.min(deadline.saturating_duration_since(now))
    }

    /// The timer fired: the next schedule starts a fresh cap window.
    pub fn fired(&mut self) {
        self.deadline = None;
    }

    pub fn is_pending(&self) -> bool {
        self.deadline.is_some()
    }
}

/// Callbacks into the server. All fire outside the pass lock.
pub trait MetadataHooks: Send + Sync {
    /// The service wants a (debounced) full pass, e.g. `Some(5 s)` after a
    /// busy path or an unreadable library root.
    fn schedule_regeneration(&self, delay: Option<Duration>) {
        let _ = delay;
    }
    /// One or more compiled files changed on disk (invalidate the library
    /// index, refresh PROPFIND caches). Must not trigger a regeneration.
    fn on_published(&self) {}
    /// An accepted update introduced or changed external ids (community
    /// fetch nudge).
    fn on_external_ids_changed(&self, series_key: &str) {
        let _ = series_key;
    }
}

/// Hooks that do nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHooks;

impl MetadataHooks for NoHooks {}

/// The WebDAV per-path write lock the compiled files share with uploads and
/// MOVEs (0.5.2 `path_write_lock`). `try_lock` returns a guard (released on
/// drop) or `None` when the path or an ancestor is busy.
pub trait PathWriteLocks: Send + Sync {
    fn try_lock(&self, path: &Path) -> Option<Box<dyn Send>>;
}

/// No DAV layer: every path is free.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoPathLocks;

impl PathWriteLocks for NoPathLocks {
    fn try_lock(&self, _path: &Path) -> Option<Box<dyn Send>> {
        Some(Box::new(()))
    }
}

/// Why a compiled file was not written.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// A DAV write holds the path; retry on the next pass.
    #[error("path is locked by a WebDAV write: {0}")]
    Busy(PathBuf),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// `write_if_changed`: publish `data` unless the file already holds exactly
/// those bytes (clients key caches on size/mtime). `Ok(true)` when written.
pub fn write_if_changed(
    path: &Path,
    data: &[u8],
    locks: &dyn PathWriteLocks,
) -> Result<bool, WriteError> {
    let same_length =
        std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.len() == data.len() as u64);
    if same_length && std::fs::read(path).is_ok_and(|current| current == data) {
        return Ok(false);
    }
    let _guard = locks
        .try_lock(path)
        .ok_or_else(|| WriteError::Busy(path.to_path_buf()))?;
    fsutil::atomic_write_bytes(path, data)?;
    Ok(true)
}

/// Why a client update could not be applied.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    /// The pass lock was not acquired in time: answer 503 +
    /// `Retry-After: 30` ("Server is busy compiling metadata; retry shortly").
    #[error("pass lock not acquired within {0:?}")]
    Busy(Duration),
    /// Persisting the facts row failed.
    #[error(transparent)]
    Store(StoreError),
}

/// Something went wrong in a pass (storage); files may be partially updated,
/// which the next pass settles.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct PassError(#[from] pub StoreError);

fn wall_clock() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

fn facts_of(row: &SeriesFactsRow) -> SeriesFacts {
    SeriesFacts {
        external_ids: row.external_ids.clone(),
        titles: row.titles.clone(),
        synonyms: row.synonyms.clone(),
        tag: row.tag.clone(),
        unit: row.unit.clone(),
        updated_at: if row.facts_updated_at.is_empty() {
            FACTLESS_UPDATED_AT.to_owned()
        } else {
            row.facts_updated_at.clone()
        },
    }
}

fn index_of(row: &SeriesFactsRow) -> SeriesIndexData {
    SeriesIndexData {
        spine_offset: row.spine_offset.clone(),
        volume_offsets: row.volume_offsets.clone(),
    }
}

/// Owns `<Series>/series.json` and the root `catalog.json`.
pub struct MetadataService {
    library_path: PathBuf,
    store: Arc<dyn MetadataStore>,
    hooks: Arc<dyn MetadataHooks>,
    locks: Arc<dyn PathWriteLocks>,
    policy: DebouncePolicy,
    clock: Box<dyn Fn() -> f64 + Send + Sync>,
    pass_lock: Mutex<()>,
    full_pass_lock: Mutex<()>,
    stopped: AtomicBool,
}

impl MetadataService {
    pub fn new(
        library_path: impl Into<PathBuf>,
        store: Arc<dyn MetadataStore>,
        hooks: Arc<dyn MetadataHooks>,
        locks: Arc<dyn PathWriteLocks>,
        policy: DebouncePolicy,
    ) -> Self {
        Self {
            library_path: library_path.into(),
            store,
            hooks,
            locks,
            policy,
            clock: Box::new(wall_clock),
            pass_lock: Mutex::new(()),
            full_pass_lock: Mutex::new(()),
            stopped: AtomicBool::new(false),
        }
    }

    /// Replace the wall clock (`time.time()`) used to clamp future stamps.
    pub fn with_clock(mut self, clock: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.clock = Box::new(clock);
        self
    }

    pub fn library_path(&self) -> &Path {
        &self.library_path
    }

    pub fn policy(&self) -> &DebouncePolicy {
        &self.policy
    }

    pub fn store(&self) -> &Arc<dyn MetadataStore> {
        &self.store
    }

    fn retry_later(&self) {
        self.hooks
            .schedule_regeneration(Some(self.policy.retry_delay));
    }

    fn row_for(&self, series_key: &str) -> Result<(SeriesFacts, SeriesIndexData), StoreError> {
        Ok(match self.store.series_facts(series_key)? {
            Some(row) => (facts_of(&row), index_of(&row)),
            None => (SeriesFacts::default(), SeriesIndexData::default()),
        })
    }

    /// Every series folder, or `None` when the library root isn't readable
    /// (a transient mount failure must not read as "the library emptied").
    fn scan_folders(&self) -> Option<Vec<SeriesFolder>> {
        std::fs::read_dir(&self.library_path).ok()?;
        Some(iter_series_folders(&self.library_path))
    }

    fn publish_series(
        &self,
        folder: &SeriesFolder,
        facts: &SeriesFacts,
        index: &SeriesIndexData,
        volumes: &[VolumeEntry],
    ) -> Result<bool, WriteError> {
        // Never resurrect a folder deleted since the scan.
        if !fsutil::is_dir(&folder.path) {
            return Ok(false);
        }
        let data = dump_series_file(&folder.title, facts, index, volumes)
            .map_err(|error| WriteError::Io(std::io::Error::other(error)))?;
        write_if_changed(
            &folder.path.join(SERIES_FILE_NAME),
            &data,
            self.locks.as_ref(),
        )
    }

    fn publish_catalog(&self, entries: &[(String, SeriesFacts)]) -> Result<bool, WriteError> {
        let data = dump_catalog_file(entries)
            .map_err(|error| WriteError::Io(std::io::Error::other(error)))?;
        write_if_changed(
            &self.library_path.join(CATALOG_FILE_NAME),
            &data,
            self.locks.as_ref(),
        )
    }

    /// Publish one series file, logging and rescheduling on failure.
    fn publish_series_logged(
        &self,
        folder: &SeriesFolder,
        facts: &SeriesFacts,
        index: &SeriesIndexData,
        volumes: &[VolumeEntry],
    ) -> usize {
        match self.publish_series(folder, facts, index, volumes) {
            Ok(written) => usize::from(written),
            Err(WriteError::Busy(_)) => {
                tracing::info!(series = %folder.title, "[METADATA] skipped busy series folder");
                self.retry_later();
                0
            }
            Err(WriteError::Io(error)) => {
                tracing::warn!(series = %folder.title, %error, "[METADATA] skipped unwritable series folder");
                self.retry_later();
                0
            }
        }
    }

    fn publish_catalog_logged(&self, entries: &[(String, SeriesFacts)]) -> usize {
        match self.publish_catalog(entries) {
            Ok(written) => usize::from(written),
            Err(WriteError::Busy(_)) => {
                tracing::info!("[METADATA] skipped busy catalog.json");
                self.retry_later();
                0
            }
            Err(WriteError::Io(error)) => {
                tracing::warn!(%error, "[METADATA] skipped unwritable catalog.json");
                self.retry_later();
                0
            }
        }
    }

    /// `_materialize_catalog_row`: this folder's render-ready catalog row
    /// (`catalog_folders`, keyed by folder name).
    fn materialize_catalog_row(
        &self,
        folder: &SeriesFolder,
        volumes: &[VolumeEntry],
    ) -> Result<(), StoreError> {
        let Ok(entries) = fsutil::list_dir(&folder.path) else {
            return Ok(());
        };
        let mut latest = 0.0f64;
        let mut covers: HashSet<String> = HashSet::new();
        for (name, path) in entries {
            let lower = crate::pyunicode::lower(&name);
            let Some(meta) = fsutil::stat(&path).filter(|meta| meta.is_file()) else {
                continue;
            };
            if lower.ends_with(".cbz") {
                latest = latest.max(fsutil::py_mtime(&meta));
            } else if lower.ends_with(".webp") {
                covers.insert(name);
            }
        }
        let cover_path = volumes
            .iter()
            .map(|volume| format!("{}.webp", volume.volume_title))
            .find(|candidate| covers.contains(candidate))
            .map(|candidate| format!("{}/{candidate}", folder.title));
        let character_total = volumes.iter().fold(0i64, |sum, v| {
            sum.saturating_add(match &v.character_count {
                crate::pyjson::JsonNum::Int(value) => *value,
                _ => i64::MAX,
            })
        });
        self.store.upsert_catalog_series(&CatalogSeriesRow {
            series_key: normalize_volume_title_key(&folder.title),
            folder_name: folder.title.clone(),
            cover_path,
            volume_count: volumes.len() as i64,
            latest_volume_modified: latest,
            total_pages: volumes
                .iter()
                .map(|v| v.page_count)
                .fold(0i64, i64::saturating_add),
            total_chars: character_total,
            missing_pages: volumes
                .iter()
                .map(VolumeEntry::missing_pages)
                .fold(0i64, i64::saturating_add),
            damaged_volumes: volumes.iter().filter(|v| v.missing_pages() > 0).count() as i64,
        })
    }

    fn published(&self, changed: usize) {
        if changed > 0 {
            self.hooks.on_published();
        }
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Stop: every public entry point becomes a no-op; returns once no pass
    /// is running. (Pending timers are the server's to cancel.)
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        drop(self.full_pass_lock.lock());
        drop(self.pass_lock.lock());
    }

    /// `regenerate_all`: recompile every series and the catalog, prune rows
    /// of vanished volumes/series. Returns the number of files written.
    pub fn recompile_all(&self) -> Result<usize, PassError> {
        if self.is_stopped() {
            return Ok(0);
        }
        let mut changed = 0;
        {
            let _full = self.full_pass_lock.lock();
            if self.is_stopped() {
                return Ok(0);
            }
            let Some(folders) = self.scan_folders() else {
                tracing::warn!(library = %self.library_path.display(), "[METADATA] library root unreadable, skipping pass");
                self.retry_later();
                return Ok(0);
            };
            let mut keep: HashSet<String> = HashSet::new();
            let mut catalog_entries: Vec<(String, SeriesFacts)> = Vec::new();
            let mut aborted = false;
            for folder in &folders {
                if self.is_stopped() {
                    aborted = true;
                    break;
                }
                // PER SERIES, released fairly so a waiting PUT gets in.
                let guard = self.pass_lock.lock();
                if self.is_stopped() {
                    aborted = true;
                    break;
                }
                let result = (|| -> Result<(), StoreError> {
                    let series_key = normalize_volume_title_key(&folder.title);
                    let (facts, index) = self.row_for(&series_key)?;
                    catalog_entries.push((folder.title.clone(), facts.clone()));
                    let volumes = compile_series_volumes(folder, Some(self.store.as_ref()), true)?;
                    for volume in &volumes {
                        keep.insert(volume_key_for(&folder.title, &volume.volume_title));
                    }
                    self.materialize_catalog_row(folder, &volumes)?;
                    changed += self.publish_series_logged(folder, &facts, &index, &volumes);
                    Ok(())
                })();
                MutexGuard::unlock_fair(guard);
                result?;
            }
            if !aborted && !self.is_stopped() {
                let _guard = self.pass_lock.lock();
                self.store.prune_series_entry_cache(&keep)?;
                // Kept by FOLDER (0.5.3): case-variant folders share a series key but
                // each has its own catalog row.
                let folder_names: HashSet<String> =
                    folders.iter().map(|folder| folder.title.clone()).collect();
                self.store.prune_catalog_series(&folder_names)?;
                changed += self.publish_catalog_logged(&catalog_entries);
            }
        }
        self.published(changed);
        Ok(changed)
    }

    /// `regenerate_series`: recompile ONE series (matched by
    /// `normalize_volume_title_key`) plus the catalog. True when a file changed.
    pub fn recompile_series(&self, series_title: &str) -> Result<bool, PassError> {
        if self.is_stopped() {
            return Ok(false);
        }
        let changed = {
            let _guard = self.pass_lock.lock();
            self.recompile_series_locked(series_title, true)?
        };
        self.published(changed);
        Ok(changed > 0)
    }

    fn recompile_series_locked(
        &self,
        series_title: &str,
        fill_hashes: bool,
    ) -> Result<usize, StoreError> {
        let Some(folders) = self.scan_folders() else {
            tracing::warn!(library = %self.library_path.display(), "[METADATA] library root unreadable, skipping pass");
            self.retry_later();
            return Ok(0);
        };
        let key = normalize_volume_title_key(series_title);
        let mut changed = 0;
        let mut catalog_entries = Vec::with_capacity(folders.len());
        for folder in &folders {
            let series_key = normalize_volume_title_key(&folder.title);
            let (facts, index) = self.row_for(&series_key)?;
            catalog_entries.push((folder.title.clone(), facts.clone()));
            if series_key != key {
                continue;
            }
            let volumes = compile_series_volumes(folder, Some(self.store.as_ref()), fill_hashes)?;
            self.materialize_catalog_row(folder, &volumes)?;
            changed += self.publish_series_logged(folder, &facts, &index, &volumes);
        }
        changed += self.publish_catalog_logged(&catalog_entries);
        Ok(changed)
    }

    /// `apply_series_update` (contract §6): a client PUT of
    /// `<Series>/series.json` is an update REQUEST. `Ok(true)` = accepted (204,
    /// even when nothing changed); `Ok(false)` = refused (400: unparseable, or
    /// no folder of that title exists right now); `Err(Busy)` = 503.
    pub fn apply_series_update(
        &self,
        series_title: &str,
        payload: &[u8],
        actor: Option<&str>,
    ) -> Result<bool, UpdateError> {
        if self.is_stopped() {
            return Ok(false);
        }
        let series_key = normalize_volume_title_key(series_title);
        if series_key.is_empty() {
            return Ok(false);
        }
        let Some(update) = parse_series_update(payload, (self.clock)()) else {
            return Ok(false);
        };
        let Some(guard) = self.pass_lock.try_lock_for(self.policy.update_lock_timeout) else {
            return Err(UpdateError::Busy(self.policy.update_lock_timeout));
        };
        let mut changed = 0;
        let ids_changed;
        {
            let _guard = guard;
            let Some(folders) = self.scan_folders() else {
                return Ok(false);
            };
            let Some(resolved_title) = folders
                .iter()
                .find(|folder| normalize_volume_title_key(&folder.title) == series_key)
                .map(|folder| folder.title.clone())
            else {
                return Ok(false);
            };
            let stored = self
                .store
                .series_facts(&series_key)
                .map_err(UpdateError::Store)?
                .map(|row| StoredSeries {
                    facts: facts_of(&row),
                    index: index_of(&row),
                });
            let result = merge_series_update(stored.as_ref(), &update);
            let ids_after = &result.facts.external_ids;
            ids_changed = !ids_after.is_empty()
                && stored
                    .as_ref()
                    .is_none_or(|stored| !ids_after.py_eq(&stored.facts.external_ids));
            if stored.is_none() || result.changed() {
                self.store
                    .put_series_facts(&SeriesFactsRow {
                        series_key: series_key.clone(),
                        series_title: resolved_title,
                        external_ids: result.facts.external_ids.clone(),
                        titles: result.facts.titles.clone(),
                        synonyms: result.facts.synonyms.clone(),
                        tag: result.facts.tag.clone(),
                        unit: result.facts.unit.clone(),
                        facts_updated_at: result.facts.updated_at.clone(),
                        spine_offset: result.index.spine_offset.clone(),
                        volume_offsets: result.index.volume_offsets.clone(),
                        updated_by: actor.map(str::to_owned),
                    })
                    .map_err(UpdateError::Store)?;
            }
            match self.recompile_series_locked(series_title, false) {
                Ok(count) => changed = count,
                Err(error) => {
                    // The facts are durable; the PUT is still accepted.
                    tracing::warn!(%error, "[METADATA] republish failed after an accepted update");
                    self.retry_later();
                }
            }
        }
        if ids_changed {
            self.hooks.on_external_ids_changed(&series_key);
        }
        self.published(changed);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_is_capped() {
        let policy = DebouncePolicy::default();
        let mut debouncer = Debouncer::default();
        let start = Instant::now();
        assert_eq!(
            debouncer.schedule(&policy, start, None),
            Duration::from_secs(10)
        );
        assert_eq!(
            debouncer.schedule(&policy, start + Duration::from_secs(55), None),
            Duration::from_secs(5)
        );
        assert_eq!(
            debouncer.schedule(&policy, start + Duration::from_secs(70), None),
            Duration::ZERO
        );
        debouncer.fired();
        assert_eq!(
            debouncer.schedule(
                &policy,
                start + Duration::from_secs(80),
                Some(Duration::from_secs(5))
            ),
            Duration::from_secs(5)
        );
    }
}
