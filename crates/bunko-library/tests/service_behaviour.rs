//! MetadataService behaviour beyond the golden bytes: hooks, busy PUTs,
//! busy/unwritable paths, stop(), the library index cache.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bunko_library::index::LibraryIndexCache;
use bunko_library::service::{
    DebouncePolicy, MetadataHooks, MetadataService, NoPathLocks, PathWriteLocks, UpdateError,
};
use bunko_library::store::{
    CachedEntryRow, CachedEntryWrite, CatalogSeriesRow, MetadataStore, SeriesFactsRow, StoreResult,
};
use common::{SqliteStore, fixture_library, load_json};
use parking_lot::Mutex;

fn schema() -> Vec<String> {
    let db = load_json("expected/db.json");
    common::obj(&db)
        .get("schema")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect()
}

#[derive(Default)]
struct RecordingHooks {
    published: AtomicUsize,
    ids_changed: Mutex<Vec<String>>,
    scheduled: Mutex<Vec<Option<Duration>>>,
}

impl MetadataHooks for RecordingHooks {
    fn schedule_regeneration(&self, delay: Option<Duration>) {
        self.scheduled.lock().push(delay);
    }
    fn on_published(&self) {
        self.published.fetch_add(1, Ordering::SeqCst);
    }
    fn on_external_ids_changed(&self, series_key: &str) {
        self.ids_changed.lock().push(series_key.to_owned());
    }
}

/// A store that can be made slow, to hold the pass lock.
struct SlowStore {
    inner: SqliteStore,
    slow: AtomicBool,
}

impl MetadataStore for SlowStore {
    fn series_facts(&self, key: &str) -> StoreResult<Option<SeriesFactsRow>> {
        if self.slow.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(300));
        }
        self.inner.series_facts(key)
    }
    fn put_series_facts(&self, row: &SeriesFactsRow) -> StoreResult<()> {
        self.inner.put_series_facts(row)
    }
    fn cached_volume_entry(&self, key: &str) -> StoreResult<Option<CachedEntryRow>> {
        self.inner.cached_volume_entry(key)
    }
    fn put_cached_volume_entry(&self, write: &CachedEntryWrite<'_>) -> StoreResult<()> {
        self.inner.put_cached_volume_entry(write)
    }
    fn prune_series_entry_cache(
        &self,
        keep: &std::collections::HashSet<String>,
    ) -> StoreResult<usize> {
        self.inner.prune_series_entry_cache(keep)
    }
    fn upsert_catalog_series(&self, row: &CatalogSeriesRow) -> StoreResult<()> {
        self.inner.upsert_catalog_series(row)
    }
    fn prune_catalog_series(&self, keep: &std::collections::HashSet<String>) -> StoreResult<usize> {
        self.inner.prune_catalog_series(keep)
    }
}

struct BusyLocks(PathBuf);

impl PathWriteLocks for BusyLocks {
    fn try_lock(&self, path: &Path) -> Option<Box<dyn Send>> {
        (path != self.0).then(|| Box::new(()) as Box<dyn Send>)
    }
}

const PUT: &[u8] =
    br#"{"version":2,"updated_at":"2026-08-18T19:36:24.324Z","external_ids":{"anilist":98416}}"#;

#[test]
fn hooks_fire_outside_and_after_the_work() {
    let (_temp, root) = fixture_library();
    let hooks = Arc::new(RecordingHooks::default());
    let service = MetadataService::new(
        &root,
        Arc::new(SqliteStore::new(&schema())),
        hooks.clone(),
        Arc::new(NoPathLocks),
        DebouncePolicy::default(),
    );
    assert!(service.recompile_all().unwrap() > 0);
    assert_eq!(hooks.published.load(Ordering::SeqCst), 1);
    // Nothing changed: no hook.
    assert_eq!(service.recompile_all().unwrap(), 0);
    assert_eq!(hooks.published.load(Ordering::SeqCst), 1);
    // Introducing ids nudges the community fetcher once; the same ids again do not.
    assert!(
        service
            .apply_series_update("Dr Stone", PUT, Some("alice"))
            .unwrap()
    );
    assert!(
        service
            .apply_series_update("Dr Stone", PUT, Some("alice"))
            .unwrap()
    );
    assert_eq!(*hooks.ids_changed.lock(), vec!["dr stone".to_owned()]);
    assert!(hooks.scheduled.lock().is_empty());
    // A title with no folder is refused; garbage is refused.
    assert!(!service.apply_series_update("Nope", PUT, None).unwrap());
    assert!(
        !service
            .apply_series_update("Dr Stone", b"nope", None)
            .unwrap()
    );
    service.stop();
    assert!(service.is_stopped());
    assert_eq!(service.recompile_all().unwrap(), 0);
    assert!(!service.apply_series_update("Dr Stone", PUT, None).unwrap());
}

#[test]
fn a_put_waiting_too_long_for_the_pass_lock_is_busy() {
    let (_temp, root) = fixture_library();
    let store = Arc::new(SlowStore {
        inner: SqliteStore::new(&schema()),
        slow: AtomicBool::new(false),
    });
    let policy = DebouncePolicy {
        update_lock_timeout: Duration::from_millis(20),
        ..DebouncePolicy::default()
    };
    let service = Arc::new(MetadataService::new(
        &root,
        store.clone(),
        Arc::new(RecordingHooks::default()),
        Arc::new(NoPathLocks),
        policy,
    ));
    service.recompile_all().unwrap();
    store.slow.store(true, Ordering::SeqCst);
    let background = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || service.recompile_series("Dr Stone").unwrap())
    };
    std::thread::sleep(Duration::from_millis(50));
    let result = service.apply_series_update("Dr Stone", PUT, None);
    assert!(matches!(result, Err(UpdateError::Busy(_))), "{result:?}");
    background.join().unwrap();
}

#[test]
fn busy_and_unwritable_paths_are_skipped_and_retried() {
    let (_temp, root) = fixture_library();
    let hooks = Arc::new(RecordingHooks::default());
    // A directory squatting where a series file belongs.
    std::fs::create_dir(root.join("Dr Stone/series.json")).unwrap();
    let service = MetadataService::new(
        &root,
        Arc::new(SqliteStore::new(&schema())),
        hooks.clone(),
        Arc::new(BusyLocks(root.join("catalog.json"))),
        DebouncePolicy::default(),
    );
    let changed = service.recompile_all().unwrap();
    assert!(changed > 0);
    assert!(
        !root.join("catalog.json").exists(),
        "a busy path was written"
    );
    assert!(root.join("Dr Stone/series.json").is_dir());
    assert!(root.join("Odd.Name. With.Dots/series.json").is_file());
    let scheduled = hooks.scheduled.lock().clone();
    assert_eq!(
        scheduled,
        vec![Some(Duration::from_secs(5)), Some(Duration::from_secs(5))]
    );
    // No temp files left behind.
    let leftovers: Vec<_> = std::fs::read_dir(root.join("Dr Stone"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".compile-"))
        .collect();
    assert!(leftovers.is_empty());
}

#[cfg(unix)]
#[test]
fn compiled_files_get_umask_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let (_temp, root) = fixture_library();
    let service = MetadataService::new(
        &root,
        Arc::new(SqliteStore::new(&schema())),
        Arc::new(RecordingHooks::default()),
        Arc::new(NoPathLocks),
        DebouncePolicy::default(),
    );
    service.recompile_all().unwrap();
    let mode = std::fs::metadata(root.join("catalog.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    // 0o666 & ~umask: group/other can at least read (never mkstemp's 0o600).
    assert_eq!(mode & 0o044, 0o044, "mode {mode:o}");
}

#[test]
fn library_index_cache_ttl_and_invalidation() {
    let (_temp, root) = fixture_library();
    let cache = LibraryIndexCache::new(&root, Duration::from_secs(30), 1 << 20);
    assert!(cache.cached_snapshot().is_none());
    let (first, scans) = cache.get_snapshot_counted();
    assert_eq!(scans, 1);
    assert!(first.series_by_name("Nested/Inner").is_some());
    assert!(first.series_by_name(".hidden").is_none());
    let (_, scans) = cache.get_snapshot_counted();
    assert_eq!(scans, 1, "served from cache within the TTL");
    cache.invalidate();
    // A fast scan is dropped on invalidation: the next read rescans.
    let (_, scans) = cache.get_snapshot_counted();
    assert_eq!(scans, 2);
    let expired = LibraryIndexCache::new(&root, Duration::ZERO, 1 << 20);
    expired.get_snapshot();
    expired.get_snapshot();
    assert_eq!(expired.scans(), 2);
}

#[test]
fn shared_types_are_thread_safe() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MetadataService>();
    assert_send_sync::<LibraryIndexCache>();
    fn assert_send<T: Send>() {}
    assert_send::<bunko_library::Volume>();
}
