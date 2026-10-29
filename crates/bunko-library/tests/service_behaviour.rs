//! MetadataService behaviour beyond the golden bytes: hooks, busy PUTs,
//! busy/unwritable paths, stop(), the library index cache.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bunko_library::index::LibraryIndexCache;
use bunko_library::pyjson::JsonObject;
use bunko_library::service::{
    DebouncePolicy, MetadataHooks, MetadataService, NoPathLocks, PathWriteLocks, SeriesUpdate,
    UpdateError,
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
fn a_cloud_submission_keeps_the_facts_of_a_folder_without_volumes() {
    let (_temp, root) = fixture_library();
    // Only sidecars: no archive, so it is no series folder yet.
    std::fs::create_dir_all(root.join("Sidecars Only")).unwrap();
    std::fs::write(root.join("Sidecars Only/v1.mokuro"), b"{}").unwrap();
    let store = Arc::new(SqliteStore::new(&schema()));
    let service = MetadataService::new(
        &root,
        store.clone(),
        Arc::new(RecordingHooks::default()),
        Arc::new(NoPathLocks),
        DebouncePolicy::default(),
    );
    // A client PUT there is still refused (contract §6)…
    assert!(
        !service
            .apply_series_update("Sidecars Only", PUT, None)
            .unwrap()
    );
    // …but the reader's file found in the cloud keeps its facts, and nothing is written.
    assert_eq!(
        service
            .ingest_series_submission("Sidecars Only", PUT, Some("cloud"))
            .unwrap(),
        SeriesUpdate::Applied
    );
    let row = store.series_facts("sidecars only").unwrap().unwrap();
    assert!(!row.external_ids.is_empty(), "{:?}", row.external_ids);
    assert!(!root.join("Sidecars Only/series.json").exists());
    assert_eq!(
        service
            .ingest_series_submission("Sidecars Only", b"nope", None)
            .unwrap(),
        SeriesUpdate::Invalid
    );
    service.stop();
    assert_eq!(
        service
            .ingest_series_submission("Sidecars Only", PUT, None)
            .unwrap(),
        SeriesUpdate::Stopped
    );
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
    // A fast scan is dropped on invalidation: a read that must see the change walks.
    cache.get_snapshot_fresh();
    assert_eq!(cache.scans(), 2);
    cache.get_snapshot_fresh();
    assert_eq!(cache.scans(), 2, "nothing changed since: no second walk");
    let expired = LibraryIndexCache::new(&root, Duration::ZERO, 1 << 20);
    expired.get_snapshot();
    expired.get_snapshot();
    wait_for_scans(&expired, 2);
}

/// Wait (up to 5 s) for the cache to reach `scans` scans.
fn wait_for_scans(cache: &LibraryIndexCache, scans: u64) {
    for _ in 0..500 {
        if cache.scans() >= scans {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the cache stayed at {} scans", cache.scans());
}

/// A request never walks the library once there is a snapshot: a stale or expired index
/// answers at once with the last one, and one background walk brings it up to date.
#[test]
fn a_stale_index_answers_at_once_and_refreshes_in_the_background() {
    let (_temp, root) = fixture_library();
    let cache = LibraryIndexCache::new(&root, Duration::from_secs(30), 1 << 20);
    let (first, scans) = cache.get_snapshot_counted();
    assert_eq!(scans, 1, "the very first read waits for the first walk");
    let volumes = |s: &bunko_library::index::LibrarySnapshot| {
        s.series_by_name("Nested/Inner").unwrap().volumes.len()
    };
    let before = volumes(&first);

    std::fs::write(root.join("Nested/Inner/Brand New.cbz"), b"").unwrap();
    cache.invalidate();
    let (served, scans) = cache.get_snapshot_counted();
    assert_eq!(
        scans, 1,
        "a changed index answers without walking the library"
    );
    assert!(Arc::ptr_eq(&served, &first));
    wait_for_scans(&cache, 2);
    let (fresh, scans) = cache.get_snapshot_counted();
    assert_eq!(scans, 2);
    assert_eq!(
        volumes(&fresh),
        before + 1,
        "the background walk saw the new volume"
    );

    // Past the TTL with nothing changed: the same.
    let expired = LibraryIndexCache::new(&root, Duration::ZERO, 1 << 20);
    let (old, _) = expired.get_snapshot_counted();
    let (served, scans) = expired.get_snapshot_counted();
    assert_eq!(
        scans, 1,
        "an expired index answers without walking the library"
    );
    assert!(Arc::ptr_eq(&served, &old));
    wait_for_scans(&expired, 2);
}

/// Before the first walk, one series is read from its own folder, and it matches what
/// the walk lists; folders the walk would not list are not served either.
#[test]
fn a_cold_index_lists_one_series_from_its_folder() {
    let (_temp, root) = fixture_library();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();
    std::fs::write(root.join(".hidden/Vol 1.cbz"), b"").unwrap();
    std::fs::create_dir_all(root.join("Empty")).unwrap();
    let walked = bunko_library::index::scan_library(&root);
    assert!(!walked.series.is_empty());
    for series in &walked.series {
        let cold = LibraryIndexCache::new(&root, Duration::from_secs(30), 1 << 20);
        assert_eq!(
            cold.series(&series.name).as_ref(),
            Some(series),
            "{}",
            series.name
        );
    }
    let cold = LibraryIndexCache::new(&root, Duration::from_secs(30), 1 << 20);
    for name in [".hidden", "Empty", "Missing", "../library", "", "Nested/"] {
        assert_eq!(cold.series(name), None, "{name:?}");
    }
    // The first read started the walk; once it is done, reads come from it.
    wait_for_scans(&cold, 1);
    let first = &walked.series[0];
    assert_eq!(cold.series(&first.name).as_ref(), Some(first));
}

#[test]
fn shared_types_are_thread_safe() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MetadataService>();
    assert_send_sync::<LibraryIndexCache>();
    fn assert_send<T: Send>() {}
    assert_send::<bunko_library::Volume>();
}

/// A present `spine_offset` of `0` is a deliberate reset: the compiled file
/// drops the key, and the same reset again writes nothing.
#[test]
fn a_zero_spine_offset_put_clears_the_published_one() {
    let (_temp, root) = fixture_library();
    let hooks = Arc::new(RecordingHooks::default());
    let service = MetadataService::new(
        &root,
        Arc::new(SqliteStore::new(&schema())),
        hooks.clone(),
        Arc::new(NoPathLocks),
        DebouncePolicy::default(),
    );
    service.recompile_all().unwrap();
    let series_file = root.join("Dr Stone/series.json");
    let set = br#"{"version":2,"updated_at":"2026-08-18T19:36:24.324Z","spine_offset":-40}"#;
    assert!(service.apply_series_update("Dr Stone", set, None).unwrap());
    let published = std::fs::read_to_string(&series_file).unwrap();
    assert!(published.contains(r#""spine_offset":-40"#), "{published}");

    let reset = br#"{"version":2,"updated_at":"2026-08-18T19:36:24.324Z","spine_offset":0}"#;
    let before = hooks.published.load(Ordering::SeqCst);
    assert!(
        service
            .apply_series_update("Dr Stone", reset, None)
            .unwrap()
    );
    let cleared = std::fs::read_to_string(&series_file).unwrap();
    assert!(!cleared.contains("spine_offset"), "{cleared}");
    assert_eq!(hooks.published.load(Ordering::SeqCst), before + 1);

    let modified = std::fs::metadata(&series_file).unwrap().modified().unwrap();
    assert!(
        service
            .apply_series_update("Dr Stone", reset, None)
            .unwrap()
    );
    assert_eq!(
        hooks.published.load(Ordering::SeqCst),
        before + 1,
        "a second identical reset writes nothing"
    );
    assert_eq!(
        std::fs::metadata(&series_file).unwrap().modified().unwrap(),
        modified
    );
    // An absent key keeps whatever is stored.
    assert!(service.apply_series_update("Dr Stone", set, None).unwrap());
    assert!(service.apply_series_update("Dr Stone", PUT, None).unwrap());
    let kept = std::fs::read_to_string(&series_file).unwrap();
    assert!(kept.contains(r#""spine_offset":-40"#), "{kept}");
}

// ---- orphan series.json ---------------------------------------------------------

const FACTS: &[u8] = br#"{"version":2,"updated_at":"2026-08-18T19:36:24.324Z","external_ids":{"anilist":98416},"spine_offset":-40}"#;

fn facts_row(key: &str, title: &str) -> SeriesFactsRow {
    SeriesFactsRow {
        series_key: key.to_owned(),
        series_title: title.to_owned(),
        external_ids: JsonObject::new(),
        titles: JsonObject::new(),
        synonyms: Vec::new(),
        tag: Some("kept".to_owned()),
        unit: None,
        facts_updated_at: "2026-08-18T19:36:24.324Z".to_owned(),
        spine_offset: None,
        volume_offsets: JsonObject::new(),
        updated_by: None,
    }
}

/// A folder that lost its last archive keeps no `series.json` (a reader would keep
/// showing the series), but keeps the folder and the facts row.
#[test]
fn a_folder_without_archives_loses_its_series_file() {
    let (_temp, root) = fixture_library();
    std::fs::create_dir(root.join("Gone")).unwrap();
    std::fs::write(root.join("Gone/series.json"), b"{}").unwrap();
    std::fs::write(root.join("Gone/Gone 1.mokuro"), b"{}").unwrap();
    // Hand-placed: facts of its own and no row, or not a series file at all.
    std::fs::create_dir(root.join("Hand Made")).unwrap();
    std::fs::write(
        root.join("Hand Made/series.json"),
        br#"{"version":2,"updated_at":"2026-01-01T00:00:00Z","tag":"mine"}"#,
    )
    .unwrap();
    std::fs::create_dir(root.join("Not Json")).unwrap();
    std::fs::write(root.join("Not Json/series.json"), b"{}").unwrap();
    // What Bunko compiles for a series nobody gave facts to: regenerable without a row.
    std::fs::create_dir(root.join("Factless")).unwrap();
    std::fs::write(
        root.join("Factless/series.json"),
        br#"{"version":2,"series_title":"Factless","external_ids":{},"titles":{},"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z","volumes":[{"volume_uuid":"u1","volume_title":"Factless 1","page_count":1,"character_count":0,"mokuro_version":""}]}"#,
    )
    .unwrap();
    let store = Arc::new(SqliteStore::new(&schema()));
    store.put_series_facts(&facts_row("gone", "Gone")).unwrap();
    let hooks = Arc::new(RecordingHooks::default());
    let service = MetadataService::new(
        &root,
        store.clone(),
        hooks.clone(),
        Arc::new(NoPathLocks),
        DebouncePolicy::default(),
    );
    assert!(service.recompile_all().unwrap() >= 1);
    assert!(!root.join("Gone/series.json").exists());
    assert!(
        root.join("Gone/Gone 1.mokuro").is_file(),
        "the folder stays"
    );
    assert!(
        store.series_facts("gone").unwrap().is_some(),
        "the facts stay"
    );
    assert!(!root.join("Factless/series.json").exists());
    assert!(
        root.join("Hand Made/series.json").is_file(),
        "facts of its own and no row: not ours to drop"
    );
    assert!(root.join("Not Json/series.json").is_file());
    assert!(root.join("Dr Stone/series.json").is_file());
    assert!(hooks.scheduled.lock().is_empty());
    assert_eq!(
        service.recompile_all().unwrap(),
        0,
        "a second pass is a no-op"
    );
}

/// The path the DELETE/MOVE hooks drive: one series lost its last volume.
#[test]
fn a_series_batch_retires_the_series_that_emptied_and_a_reupload_brings_it_back() {
    let (_temp, root) = fixture_library();
    let service = MetadataService::new(
        &root,
        Arc::new(SqliteStore::new(&schema())),
        Arc::new(RecordingHooks::default()),
        Arc::new(NoPathLocks),
        DebouncePolicy::default(),
    );
    service.recompile_all().unwrap();
    let series = "Odd.Name. With.Dots";
    assert!(service.apply_series_update(series, FACTS, None).unwrap());
    let folder = root.join(series);
    let other = root.join("Dr Stone/series.json");
    let other_before = std::fs::read(&other).unwrap();
    let other_modified = std::fs::metadata(&other).unwrap().modified().unwrap();
    let saved = std::fs::read(folder.join("v.1.cbz")).unwrap();
    std::fs::remove_file(folder.join("v.1.cbz")).unwrap();
    std::fs::remove_file(folder.join("v.2.cbz")).unwrap();

    assert!(
        service
            .recompile_series_batch(&[series.to_owned()])
            .unwrap()
            >= 1
    );
    assert!(!folder.join("series.json").exists());
    assert_eq!(std::fs::read(&other).unwrap(), other_before);
    assert_eq!(
        std::fs::metadata(&other).unwrap().modified().unwrap(),
        other_modified
    );
    let catalog = std::fs::read_to_string(root.join("catalog.json")).unwrap();
    assert!(!catalog.contains(series), "{catalog}");

    std::fs::write(folder.join("v.1.cbz"), saved).unwrap();
    assert!(service.recompile_series(series).unwrap());
    let back = std::fs::read_to_string(folder.join("series.json")).unwrap();
    assert!(back.contains(r#""anilist":98416"#), "{back}");
    assert!(back.contains(r#""spine_offset":-40"#), "{back}");
}

#[test]
fn a_busy_orphan_series_file_is_kept_and_retried() {
    let (_temp, root) = fixture_library();
    std::fs::create_dir(root.join("Gone")).unwrap();
    std::fs::write(root.join("Gone/series.json"), b"{}").unwrap();
    let store = Arc::new(SqliteStore::new(&schema()));
    store.put_series_facts(&facts_row("gone", "Gone")).unwrap();
    let hooks = Arc::new(RecordingHooks::default());
    let service = MetadataService::new(
        &root,
        store,
        hooks.clone(),
        Arc::new(BusyLocks(root.join("Gone/series.json"))),
        DebouncePolicy::default(),
    );
    service.recompile_all().unwrap();
    assert!(root.join("Gone/series.json").is_file());
    assert_eq!(*hooks.scheduled.lock(), vec![Some(Duration::from_secs(5))]);
}

/// The library root as a "remote" store whose health the test sets; files are local.
struct GatedStore {
    inner: bunko_store::LocalStore,
    health: Mutex<bunko_store::Health>,
}

mod gated {
    use std::io;
    use std::path::{Path, PathBuf};

    use bunko_store::{
        Caps, File, Health, LibraryStore, LocalPath, Meta, OpenOptions, Published, ReadDir,
        Version, WatchGuard, WatchSink, WriteIntent,
    };

    impl LibraryStore for super::GatedStore {
        fn root(&self) -> &Path {
            self.inner.root()
        }
        fn caps(&self) -> Caps {
            Caps {
                remote: true,
                ..self.inner.caps()
            }
        }
        fn health(&self) -> Health {
            self.health.lock().clone()
        }
        fn metadata(&self, p: &Path) -> io::Result<Meta> {
            self.inner.metadata(p)
        }
        fn symlink_metadata(&self, p: &Path) -> io::Result<Meta> {
            self.inner.symlink_metadata(p)
        }
        fn read_dir(&self, p: &Path) -> io::Result<ReadDir> {
            self.inner.read_dir(p)
        }
        fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
            self.inner.canonicalize(p)
        }
        fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
            self.inner.read_link(p)
        }
        fn open(&self, p: &Path) -> io::Result<File> {
            self.inner.open(p)
        }
        fn local_path(&self, p: &Path, expect: Option<&Version>) -> io::Result<LocalPath> {
            self.inner.local_path(p, expect)
        }
        fn open_with(&self, p: &Path, o: &OpenOptions) -> io::Result<File> {
            self.inner.open_with(p, o)
        }
        fn publish(&self, staged: &Path, dest: &Path, w: &WriteIntent) -> io::Result<Published> {
            self.inner.publish(staged, dest, w)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.inner.rename(from, to)
        }
        fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
            self.inner.copy(from, to)
        }
        fn remove_file(&self, p: &Path) -> io::Result<()> {
            self.inner.remove_file(p)
        }
        fn remove_dir(&self, p: &Path) -> io::Result<()> {
            self.inner.remove_dir(p)
        }
        fn remove_dir_all(&self, p: &Path) -> io::Result<()> {
            self.inner.remove_dir_all(p)
        }
        fn create_dir(&self, p: &Path) -> io::Result<()> {
            self.inner.create_dir(p)
        }
        fn create_dir_all(&self, p: &Path) -> io::Result<()> {
            self.inner.create_dir_all(p)
        }
        fn watch(&self, sink: WatchSink) -> io::Result<WatchGuard> {
            self.inner.watch(sink)
        }
    }
}

/// X16: on a remote library a folder may only look empty mid full sync, while
/// removals wait to be confirmed (Suspect), or while another Bunko holds the lease.
/// Its `series.json` goes only once the store is `Online` again.
#[test]
fn a_remote_library_retires_only_while_online() {
    use bunko_store::Health;

    let (_temp, root) = fixture_library();
    std::fs::create_dir(root.join("Gone")).unwrap();
    std::fs::write(root.join("Gone/series.json"), b"{}").unwrap();
    let store = Arc::new(SqliteStore::new(&schema()));
    store.put_series_facts(&facts_row("gone", "Gone")).unwrap();
    let gated = Arc::new(GatedStore {
        inner: bunko_store::LocalStore::new(&root),
        health: Mutex::new(Health::Syncing { done: 3, total: 0 }),
    });
    let _mount = bunko_store::mount(gated.clone());
    let hooks = Arc::new(RecordingHooks::default());
    let service = MetadataService::new(
        &root,
        store,
        hooks.clone(),
        Arc::new(NoPathLocks),
        DebouncePolicy::default(),
    );
    for health in [
        Health::Syncing { done: 3, total: 0 },
        Health::Suspect,
        Health::LeaseHeld { by: "nas".into() },
        Health::Offline,
    ] {
        *gated.health.lock() = health.clone();
        service.recompile_all().unwrap();
        service
            .recompile_series_batch(&["Gone".to_owned()])
            .unwrap();
        assert!(root.join("Gone/series.json").is_file(), "{health:?}");
    }
    assert!(
        hooks.scheduled.lock().is_empty(),
        "no retry loop while waiting"
    );
    *gated.health.lock() = Health::Online;
    service.recompile_all().unwrap();
    assert!(!root.join("Gone/series.json").exists());
}
