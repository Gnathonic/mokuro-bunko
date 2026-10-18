//! The metadata runtime: startup compile, watcher-triggered recompiles, the store over
//! bunko-db, and shutdown.

// Fixtures are set up and checked with std::fs on purpose.
#![allow(clippy::disallowed_methods)]

mod library_support;

use bunko_library::MetadataStore;
use bunko_library::compiler::{SeriesFolder, compile_series_volumes, entry_json};
use library_support::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_pass_writes_series_and_catalog_files() {
    let env = Env::new(|_| {});
    let dir = env.series_dir("Dr Stone");
    write_cbz(&dir.join("Volume 01.cbz"), 3);
    write_mokuro(&dir.join("Volume 01.mokuro"), "uuid-1", 3);
    write_cbz(&dir.join("Volume 02.cbz"), 2);
    std::fs::write(dir.join("Volume 01.webp"), b"RIFF").unwrap();

    env.runtime.start();
    let catalog = env.library.join("catalog.json");
    assert!(
        wait_for(Duration::from_secs(5), || catalog.is_file()).await,
        "startup pass never published"
    );

    let series = read_json(&dir.join("series.json"));
    assert_eq!(series["version"], 2);
    assert_eq!(series["series_title"], "Dr Stone");
    let volumes = series["volumes"].as_array().unwrap();
    assert_eq!(volumes.len(), 2);
    assert_eq!(volumes[0]["volume_uuid"], "uuid-1");
    assert_eq!(volumes[0]["page_count"], 3);
    assert_eq!(volumes[0]["character_count"], 15);
    assert_eq!(volumes[0]["mokuro_version"], "0.2.2");
    assert!(
        volumes[0]["mokuro_sha256"]
            .as_str()
            .is_some_and(|h| h.len() == 64)
    );
    assert_eq!(volumes[1]["volume_title"], "Volume 02");
    assert_eq!(volumes[1]["page_count"], 2);
    assert_eq!(volumes[1]["mokuro_version"], "");

    let text = std::fs::read_to_string(&catalog).unwrap();
    assert_eq!(
        text,
        r#"{"version":1,"updated_at":"1970-01-01T00:00:00.000Z","series":[{"series_title":"Dr Stone","external_ids":{},"titles":{},"synonyms":[],"updated_at":"1970-01-01T00:00:00.000Z"}]}"#
    );

    // The materialized catalog row and the entry cache are in the real database.
    let rows = env.db.list_catalog_series().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].folder_name, "Dr Stone");
    assert_eq!(
        rows[0].cover_path.as_deref(),
        Some("Dr Stone/Volume 01.webp")
    );
    assert_eq!(rows[0].volume_count, 2);
    assert_eq!(rows[0].total_pages, 5);
    // The identity of the sidecar-backed volume is remembered.
    assert_eq!(
        env.db
            .remembered_volume_uuid("Dr Stone/Volume 01.cbz")
            .unwrap()
            .as_deref(),
        Some("uuid-1")
    );

    // A second pass with no input change rewrites nothing.
    let before = std::fs::metadata(dir.join("series.json"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(env.runtime.recompile_all_now().await, 0);
    assert_eq!(
        std::fs::metadata(dir.join("series.json"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
    env.runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_recompiles_a_series_after_a_volume_arrives() {
    let archives = Arc::new(Archives::default());
    let published = Arc::new(AtomicUsize::new(0));
    let refreshed = Arc::new(AtomicUsize::new(0));
    let hooks = bunko_server::library::LibraryHooks {
        propfind_refresh: Some({
            let r = refreshed.clone();
            Arc::new(move || {
                r.fetch_add(1, Ordering::SeqCst);
            })
        }),
        on_published: Some({
            let p = published.clone();
            Arc::new(move || {
                p.fetch_add(1, Ordering::SeqCst);
            })
        }),
        archive_events: Some(archives.clone()),
    };
    let env = Env::with(
        |_| {},
        Options {
            watch: true,
            hooks,
            ..Options::default()
        },
    );
    let dir = env.series_dir("Aria");
    write_cbz(&dir.join("Aria 01.cbz"), 2);
    env.runtime.start();
    let series_file = dir.join("series.json");
    assert!(wait_for(Duration::from_secs(5), || series_file.is_file()).await);
    assert_eq!(
        read_json(&series_file)["volumes"].as_array().unwrap().len(),
        1
    );
    assert!(
        wait_for(Duration::from_secs(5), || published.load(Ordering::SeqCst)
            >= 1)
        .await
    );

    // A new archive lands (written aside, then renamed into place like an upload).
    let staging = dir.join(".upload.tmp");
    write_cbz(&staging, 4);
    std::fs::rename(&staging, dir.join("Aria 02.cbz")).unwrap();
    let grew = wait_for(Duration::from_secs(5), || {
        std::fs::read(&series_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|v| v["volumes"].as_array().is_some_and(|a| a.len() == 2))
    })
    .await;
    assert!(grew, "watcher did not trigger a recompile");
    let series = read_json(&series_file);
    assert_eq!(series["volumes"][1]["volume_title"], "Aria 02");
    assert_eq!(series["volumes"][1]["page_count"], 4);
    assert!(
        refreshed.load(Ordering::SeqCst) >= 1,
        "PROPFIND refresh hook not called"
    );
    let seen = archives.0.lock().clone();
    assert!(
        seen.iter().any(|p| p.ends_with("Aria 02.cbz")),
        "OCR not told about the arrival: {seen:?}"
    );

    // The index sees the new volume too (it was invalidated).
    let snapshot = env.runtime.index().get_snapshot();
    assert_eq!(snapshot.series_by_name("Aria").unwrap().volumes.len(), 2);
    assert_eq!(env.runtime.counts().total_volumes().unwrap(), 2);

    env.runtime.stop().await;
    assert!(!env.runtime.has_pending_pass());
    // After stop, nothing reacts any more.
    write_cbz(&dir.join("Aria 03.cbz"), 1);
    env.runtime
        .schedule_regeneration(Some(Duration::from_millis(1)));
    assert!(!env.runtime.has_pending_pass());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        read_json(&series_file)["volumes"].as_array().unwrap().len(),
        2
    );
}

#[derive(Default)]
struct Archives(parking_lot::Mutex<Vec<String>>);

impl bunko_server::library::ArchiveEvents for Archives {
    fn archive_added(&self, cbz: &std::path::Path) {
        self.0.lock().push(cbz.display().to_string());
    }
    fn archive_removed(&self, _cbz: &std::path::Path) {}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dav_writes_schedule_the_series_without_the_watcher() {
    let env = Env::new(|_| {});
    let dir = env.series_dir("Blame");
    write_cbz(&dir.join("Blame 01.cbz"), 1);
    assert_eq!(env.runtime.recompile_all_now().await, 2);
    write_cbz(&dir.join("Blame 02.cbz"), 1);
    // What bunko-dav reports after a PUT commits.
    bunko_dav::DavHooks::library_changed(env.runtime.as_ref(), &[dir.join("Blame 02.cbz")]);
    assert!(env.runtime.has_pending_pass());
    let series_file = dir.join("series.json");
    let grew = wait_for(Duration::from_secs(5), || {
        read_json(&series_file)["volumes"]
            .as_array()
            .is_some_and(|a| a.len() == 2)
    })
    .await;
    assert!(grew);
    // The batch that wrote it may still be publishing the catalog.
    let runtime = env.runtime.clone();
    assert!(
        tokio::task::spawn_blocking(move || runtime.wait_idle(Duration::from_secs(5)))
            .await
            .unwrap()
    );
    // A series.json write is not a change that needs compiling.
    bunko_dav::DavHooks::library_changed(env.runtime.as_ref(), std::slice::from_ref(&series_file));
    assert!(!env.runtime.has_pending_pass());
    env.runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_series_changes_is_compiled_in_a_few_batches() {
    // 0.7.0 ran one pass per changed series on the blocking pool, each scanning the
    // whole library: a burst of N changes queued N library-wide passes ahead of every
    // request. They now coalesce into batches on the metadata thread.
    let env = Env::new(|_| {});
    const SERIES: usize = 40;
    let dirs: Vec<_> = (0..SERIES)
        .map(|i| {
            let dir = env.series_dir(&format!("Series {i:02}"));
            write_cbz(&dir.join("Vol 01.cbz"), 1);
            dir
        })
        .collect();
    assert!(env.runtime.recompile_all_now().await > 0);
    let before = env.runtime.passes_run();
    for dir in &dirs {
        write_cbz(&dir.join("Vol 02.cbz"), 1);
        bunko_dav::DavHooks::library_changed(env.runtime.as_ref(), &[dir.join("Vol 02.cbz")]);
    }
    let all = wait_for(Duration::from_secs(10), || {
        dirs.iter().all(|dir| {
            read_json(&dir.join("series.json"))["volumes"]
                .as_array()
                .is_some_and(|a| a.len() == 2)
        })
    })
    .await;
    assert!(all, "every changed series is recompiled");
    let runtime = env.runtime.clone();
    assert!(
        tokio::task::spawn_blocking(move || runtime.wait_idle(Duration::from_secs(10)))
            .await
            .unwrap()
    );
    let batches = env.runtime.passes_run() - before;
    assert!(
        (1..=8).contains(&batches),
        "{SERIES} changed series took {batches} passes"
    );
    assert!(!env.runtime.has_pending_pass());
    env.runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_rows_match_what_the_compiler_serialises() {
    let env = Env::new(|_| {});
    let dir = env.series_dir("Mix");
    write_cbz(&dir.join("v1.cbz"), 2);
    let sidecar = serde_json::json!({"version": "0.2.2", "volume_uuid": "u-1", "spine_width": 250.5, "chars": 7,
        "pages": [{"img_path": "001.jpg"}, {"img_path": "002.jpg"}, {"img_path": "003.jpg"}]});
    std::fs::write(dir.join("v1.mokuro"), serde_json::to_vec(&sidecar).unwrap()).unwrap();
    let store = env.runtime.store().clone();
    let folder = SeriesFolder {
        title: "Mix".into(),
        path: dir.clone(),
    };
    let entries = compile_series_volumes(&folder, Some(store.as_ref()), true).unwrap();
    assert_eq!(entries.len(), 1);
    let expected = entry_json(&entries[0]);
    let row = store
        .cached_volume_entry("Mix/v1.cbz")
        .unwrap()
        .expect("cached row");
    assert_eq!(
        row.entry_json, expected,
        "entry_json differs from 0.5.2's json.dumps spelling"
    );
    assert!(expected.contains("\"spine_width\": 250.5"));
    assert!(expected.contains("\"matched_page_count\": 2"));
    // A second compile is served from the cache (same entry, no recompute needed).
    let again = compile_series_volumes(&folder, Some(store.as_ref()), true).unwrap();
    assert_eq!(entry_json(&again[0]), expected);

    // Facts round-trip with int/float kept apart.
    let mut offsets = bunko_library::pyjson::JsonObject::new();
    offsets.insert("u-1", bunko_library::pyjson::JsonValue::from(-40i64));
    let row = bunko_library::SeriesFactsRow {
        series_key: "mix".into(),
        series_title: "Mix".into(),
        external_ids: bunko_library::pyjson::JsonObject(vec![(
            "anilist".into(),
            bunko_library::pyjson::JsonValue::from(5i64),
        )]),
        titles: bunko_library::pyjson::JsonObject(vec![("native".into(), "ミックス".into())]),
        synonyms: vec!["x".into()],
        tag: Some("HD".into()),
        unit: Some("volumes".into()),
        facts_updated_at: "2026-08-18T19:36:24.324Z".into(),
        spine_offset: Some(bunko_library::pyjson::JsonNum::Float(12.5)),
        volume_offsets: offsets,
        updated_by: Some("ed".into()),
    };
    store.put_series_facts(&row).unwrap();
    let raw: (String, String, String) = env.db.with_writer_connection(|c| {
        c.query_row("SELECT titles, volume_offsets, typeof(spine_offset) FROM series_facts WHERE series_key='mix'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
    });
    assert_eq!(
        raw,
        (
            "{\"native\": \"ミックス\"}".into(),
            "{\"u-1\": -40}".into(),
            "real".into()
        )
    );
    let back = store.series_facts("mix").unwrap().unwrap();
    assert!(
        matches!(back.spine_offset, Some(bunko_library::pyjson::JsonNum::Float(f)) if f == 12.5)
    );
    // NUMERIC affinity stores an integral float as an integer, exactly as under 0.5.2.
    let mut integral = row.clone();
    integral.spine_offset = Some(bunko_library::pyjson::JsonNum::Float(12.0));
    store.put_series_facts(&integral).unwrap();
    assert!(matches!(
        store.series_facts("mix").unwrap().unwrap().spine_offset,
        Some(bunko_library::pyjson::JsonNum::Int(12))
    ));
    assert_eq!(back.tag.as_deref(), Some("HD"));
    assert!(back.volume_offsets.get("u-1").is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_steady_stream_of_changes_still_fires_at_the_cap() {
    // debounce 100 ms, cap 600 ms: a change every 20 ms would postpone the pass forever
    // without the cap. The stream is this task's own, and a busy runner can stall it
    // for a whole debounce (a 50 ms sleep took over 100 ms on macOS): the pass then
    // fires early, rightly, because the quiet period really elapsed, and the run says
    // nothing about the cap. Such a run is repeated, never judged.
    let policy = fast_policy();
    let mut stalled = Vec::new();
    for _ in 0..10 {
        let env = Env::new(|_| {});
        let dir = env.series_dir("Busy");
        let archive = dir.join("b1.cbz");
        write_cbz(&archive, 1);
        let series_file = dir.join("series.json");
        // A change is scheduled somewhere inside its call, so the quiet period between
        // two changes is at most from one call's start to the next call's end (or to
        // the check that found the file).
        let start = Instant::now();
        let mut last_call: Option<Instant> = None;
        let mut longest_quiet = Duration::ZERO;
        let fired_by = loop {
            let found = series_file.is_file();
            if let Some(last) = last_call {
                longest_quiet = longest_quiet.max(last.elapsed());
            }
            if found {
                break start.elapsed();
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the capped debounce never fired"
            );
            let called = Instant::now();
            env.runtime.on_library_write(&archive);
            if let Some(last) = last_call {
                longest_quiet = longest_quiet.max(last.elapsed());
            }
            last_call = Some(called);
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        env.runtime.stop().await;
        if longest_quiet >= policy.debounce {
            stalled.push((longest_quiet, fired_by));
            continue;
        }
        // Steady: only the cap, armed by the first change, could have fired it.
        assert!(
            fired_by >= policy.max_debounce,
            "fired before the cap: {fired_by:?} (longest quiet {longest_quiet:?})"
        );
        assert!(
            fired_by < Duration::from_millis(1500),
            "fired long after the cap: {fired_by:?}"
        );
        return;
    }
    panic!("the runner stalled every stream for a debounce (longest quiet, fired by): {stalled:?}");
}

/// 0.5.3 `test_case_variant_folders_each_keep_their_own_row`: `Kingdom/` and `kingdom/`
/// fold to one series key but are two folders on a case-sensitive host; neither catalog row
/// may overwrite the other (a stray one-volume `kingdom/` can hide all the
/// volumes of `Kingdom/` from the catalog).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case_variant_folders_each_keep_their_own_catalog_row() {
    let env = Env::new(|_| {});
    write_cbz(&env.series_dir("Kingdom").join("Volume 01.cbz"), 1);
    write_cbz(&env.series_dir("Kingdom").join("Volume 02.cbz"), 1);
    write_cbz(&env.series_dir("kingdom").join("Volume 13.cbz"), 1);
    let rows = |env: &Env| -> Vec<(String, i64, String)> {
        env.db
            .list_catalog_series()
            .unwrap()
            .into_iter()
            .map(|r| (r.folder_name, r.volume_count, r.series_key))
            .collect()
    };
    if env.library.join("KINGDOM").exists() {
        // A case-insensitive host (APFS, NTFS): `kingdom/` IS `Kingdom/`, so the two
        // folders cannot exist and the third volume landed in the one there is.
        env.runtime.recompile_all_now().await;
        assert_eq!(
            rows(&env),
            [("Kingdom".to_string(), 3, "kingdom".to_string())]
        );
        env.runtime.stop().await;
        return;
    }
    let both = vec![
        ("Kingdom".to_string(), 2, "kingdom".to_string()),
        ("kingdom".to_string(), 1, "kingdom".to_string()),
    ];

    env.runtime.recompile_all_now().await;
    assert_eq!(rows(&env), both);

    let service = env.runtime.service().clone();
    tokio::task::spawn_blocking(move || service.recompile_series("kingdom").unwrap())
        .await
        .unwrap();
    assert_eq!(rows(&env), both);

    std::fs::remove_dir_all(env.library.join("kingdom")).unwrap();
    env.runtime.recompile_all_now().await;
    let names: Vec<String> = rows(&env).into_iter().map(|r| r.0).collect();
    assert_eq!(names, ["Kingdom"]);
    env.runtime.stop().await;
}

/// A cloud library's first sync reports every file before the library services start:
/// held, the runtime arms no pass for them (the startup pass covers them all).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_runtime_arms_no_pass_before_it_starts() {
    let env = Env::with(
        |_| {},
        Options {
            hold_until_start: true,
            ..Options::default()
        },
    );
    let dir = env.series_dir("Held");
    write_cbz(&dir.join("Held 01.cbz"), 1);
    bunko_dav::DavHooks::library_changed(env.runtime.as_ref(), &[dir.join("Held 01.cbz")]);
    env.runtime
        .schedule_series_regeneration("Held", Some(Duration::from_millis(1)));
    assert!(!env.runtime.has_pending_pass());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!dir.join("series.json").exists(), "no pass before start");
    env.runtime.start();
    assert!(
        wait_for(Duration::from_secs(5), || dir.join("series.json").is_file()).await,
        "the startup pass compiles what arrived before"
    );
    env.runtime.stop().await;
}
