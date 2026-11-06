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
    let snapshot = env.runtime.index().get_snapshot_fresh();
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
struct Archives(parking_lot::Mutex<Vec<String>>, AtomicUsize);

impl bunko_server::library::ArchiveEvents for Archives {
    fn archive_added(&self, cbz: &std::path::Path) {
        self.0.lock().push(cbz.display().to_string());
    }
    fn archive_removed(&self, _cbz: &std::path::Path) {}
    fn rescan(&self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

/// A cloud progress user's `history/` sits at the library root and may hold names
/// ending in `.cbz`: the watcher's events there tell OCR nothing, refresh no listing
/// and schedule no pass. A series beside it still does all three.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watcher_events_in_the_per_user_tree_are_not_library_changes() {
    use bunko_store::WatchEvent;
    let archives = Arc::new(Archives::default());
    let refreshed = Arc::new(AtomicUsize::new(0));
    let hooks = bunko_server::library::LibraryHooks {
        propfind_refresh: Some({
            let r = refreshed.clone();
            Arc::new(move || {
                r.fetch_add(1, Ordering::SeqCst);
            })
        }),
        archive_events: Some(archives.clone()),
        ..Default::default()
    };
    let env = Env::with(
        |_| {},
        Options {
            hooks,
            ..Options::default()
        },
    );
    let dir = env.series_dir("Aria");
    write_cbz(&dir.join("Aria 01.cbz"), 2);
    env.runtime.start();
    assert!(wait_for(Duration::from_secs(5), || dir.join("series.json").is_file()).await);
    assert!(env.runtime.wait_idle(Duration::from_secs(5)));
    let refreshes = refreshed.load(Ordering::SeqCst);
    let rescans = archives.1.load(Ordering::SeqCst);

    let history = env.library.join("history/dev-1");
    std::fs::create_dir_all(&history).unwrap();
    write_cbz(&history.join("x.cbz"), 1);
    for event in [
        WatchEvent::Added {
            path: env.library.join("history"),
            is_dir: true,
        },
        WatchEvent::Added {
            path: history.clone(),
            is_dir: true,
        },
        WatchEvent::Added {
            path: history.join("x.cbz"),
            is_dir: false,
        },
        WatchEvent::Removed {
            path: history.join("x.cbz"),
            is_dir: false,
        },
        WatchEvent::Added {
            path: env.library.join("goals.json"),
            is_dir: false,
        },
    ] {
        env.runtime.on_watch_event(event);
    }
    assert!(archives.0.lock().is_empty(), "{:?}", archives.0.lock());
    assert_eq!(archives.1.load(Ordering::SeqCst), rescans);
    assert_eq!(refreshed.load(Ordering::SeqCst), refreshes);
    assert!(!env.runtime.has_pending_pass());

    // The control: the same event for a series volume reaches all of them.
    write_cbz(&dir.join("Aria 02.cbz"), 1);
    env.runtime.on_watch_event(WatchEvent::Added {
        path: dir.join("Aria 02.cbz"),
        is_dir: false,
    });
    assert!(archives.0.lock().iter().any(|p| p.ends_with("Aria 02.cbz")));
    assert!(refreshed.load(Ordering::SeqCst) > refreshes);
    assert!(env.runtime.has_pending_pass());
    env.runtime.stop().await;
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

/// `Kingdom/` and `kingdom/` are one series key, and one name: every host lists one
/// catalog row for them. A case-sensitive disk can hold both folders (made outside
/// Bunko); the byte-smallest spelling is the one Bunko reaches, and the other is hidden
/// until it is merged by hand (doctor names it). The fixture writes `Kingdom/` first, so
/// a case-insensitive host (one folder) gives the same row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn case_variants_list_one_catalog_row() {
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
    // The volumes listed in `Kingdom/` itself: 3 where the host merged the two
    // spellings, 2 where it did not.
    let listed = std::fs::read_dir(env.library.join("Kingdom"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".cbz"))
        .count() as i64;
    let one = vec![("Kingdom".to_string(), listed, "kingdom".to_string())];

    env.runtime.recompile_all_now().await;
    assert_eq!(rows(&env), one);

    let service = env.runtime.service().clone();
    tokio::task::spawn_blocking(move || service.recompile_series("kingdom").unwrap())
        .await
        .unwrap();
    assert_eq!(rows(&env), one);
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

/// A series that lost its last archive over WebDAV keeps no `series.json` (the reader
/// would keep showing it) and leaves `catalog.json`; its facts stay, so the next upload
/// publishes them again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleting_the_last_volume_retires_the_series_file() {
    let env = Env::new(|_| {});
    env.user("editor", bunko_core::Role::Editor);
    let gone = env.series_dir("Gone");
    let kept = env.series_dir("Kept");
    write_cbz(&gone.join("Gone 01.cbz"), 1);
    write_cbz(&kept.join("Kept 01.cbz"), 1);
    assert!(env.runtime.recompile_all_now().await > 0);
    const FACTS: &str = r#"{"version":2,"updated_at":"2026-08-18T19:36:24.324Z","external_ids":{"anilist":98416},"spine_offset":-40}"#;
    let r = env
        .send(
            req("PUT", "/mokuro-reader/Gone/series.json")
                .header("authorization", basic("editor"))
                .header("content-length", FACTS.len().to_string())
                .body(axum::body::Body::from(FACTS))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.text());
    let series_file = gone.join("series.json");
    assert!(series_file.is_file());
    let kept_before = std::fs::read(kept.join("series.json")).unwrap();

    // What bunko-dav reports after a DELETE of the last archive commits.
    std::fs::remove_file(gone.join("Gone 01.cbz")).unwrap();
    bunko_dav::DavHooks::library_changed(env.runtime.as_ref(), &[gone.join("Gone 01.cbz")]);
    assert!(
        wait_for(Duration::from_secs(5), || !series_file.exists()).await,
        "series.json of the emptied folder stays"
    );
    let runtime = env.runtime.clone();
    assert!(
        tokio::task::spawn_blocking(move || runtime.wait_idle(Duration::from_secs(5)))
            .await
            .unwrap()
    );
    let catalog = std::fs::read_to_string(env.library.join("catalog.json")).unwrap();
    assert!(!catalog.contains(r#""series_title":"Gone""#), "{catalog}");
    assert!(catalog.contains(r#""series_title":"Kept""#), "{catalog}");
    assert_eq!(
        std::fs::read(kept.join("series.json")).unwrap(),
        kept_before
    );
    assert!(gone.is_dir(), "the folder stays");
    assert!(env.db.get_series_facts("gone").unwrap().is_some());

    // A re-upload publishes the stored facts again.
    write_cbz(&gone.join("Gone 02.cbz"), 1);
    bunko_dav::DavHooks::library_changed(env.runtime.as_ref(), &[gone.join("Gone 02.cbz")]);
    assert!(wait_for(Duration::from_secs(5), || series_file.is_file()).await);
    let back = std::fs::read_to_string(&series_file).unwrap();
    assert!(back.contains(r#""anilist":98416"#), "{back}");
    assert!(back.contains(r#""spine_offset":-40"#), "{back}");
    env.runtime.stop().await;
}

/// A series file in another spelling (`Series.json`, made outside Bunko) is the
/// series file: the compiler reads and writes it there and never puts a second
/// `series.json` beside it, so a reader's GET of `series.json` (which reaches
/// `Series.json`) gets the compiled one. The same on every host; `catalog.json` too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_series_file_in_another_spelling_is_the_one_compiled() {
    let env = Env::new(|_| {});
    let dir = env.series_dir("Kingdom");
    write_cbz(&dir.join("V.cbz"), 1);
    std::fs::write(dir.join("Series.json"), "{}").unwrap();
    std::fs::write(env.library.join("Catalog.json"), "{}").unwrap();
    env.runtime.recompile_all_now().await;
    let names = |d: &std::path::Path| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        v.sort();
        v
    };
    assert_eq!(names(&dir), ["Series.json", "V.cbz"]);
    let compiled = std::fs::read_to_string(dir.join("Series.json")).unwrap();
    assert!(compiled.contains("\"volumes\""), "{compiled}");
    assert!(
        !names(&env.library).iter().any(|n| n == "catalog.json"),
        "{:?}",
        names(&env.library)
    );
    let catalog = std::fs::read_to_string(env.library.join("Catalog.json")).unwrap();
    assert!(catalog.contains("Kingdom"), "{catalog}");
    env.runtime.stop().await;
}
