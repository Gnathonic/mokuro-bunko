//! Session event semantics with the fake pipeline, over the local link.

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bunko_processor::{FakeConfig, FakePipeline, LocalConfig, LocalLink, LocalProcessor};
use bunko_proto::{BenchOp, Event, Op, VolumeOp};
use common::*;
use sha2::Digest;

fn start(config: FakeConfig, results: &Path) -> (LocalLink, FakePipeline) {
    let fake = FakePipeline::new(config);
    let link = LocalProcessor::spawn(
        Arc::new(fake.clone()),
        LocalConfig {
            results_dir: results.to_path_buf(),
        },
    );
    (link, fake)
}

fn volume_op(sid: &str, claim: &str, archive: &Path) -> Op {
    let stem = archive.file_stem().unwrap().to_string_lossy().into_owned();
    Op::Volume(VolumeOp {
        sid: sid.into(),
        claim: claim.into(),
        archive: archive.to_string_lossy().into_owned(),
        sidecar_name: format!("{stem}.mokuro"),
        title: "Series".into(),
        volume_title: stem,
        title_uuid: None,
        volume_uuid: Some("vu".into()),
        size: None,
        etag: None,
    })
}

async fn send(link: &LocalLink, op: Op) {
    link.ops.send(op).await.unwrap();
}

fn open(sid: &str) -> Op {
    Op::OpenSession {
        sid: sid.into(),
        generation: row("fake"),
    }
}

fn exits(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::Exit { .. }))
        .count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn volumes_run_in_order_and_close_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let results = dir.path().join("results");
    let a = write_volume(dir.path(), "Vol 1.cbz", 3);
    let b = write_volume(dir.path(), "Vol 2.cbz", 2);
    let (mut link, fake) = start(
        FakeConfig {
            load_delay: Duration::from_millis(200),
            ..Default::default()
        },
        &results,
    );
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    send(&link, volume_op("s1", "v2", &b)).await;
    // Both are handed to the runner while the models load; the close comes after.
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&link, Op::CloseSession { sid: "s1".into() }).await;
    let events = until_exit(&mut link.events, "s1", 10).await;
    assert_eq!(
        names(&events),
        [
            "ready",
            "volume_started:v1",
            "volume_done:v1",
            "volume_started:v2",
            "volume_done:v2",
            "exit"
        ],
        "{events:#?}"
    );
    let pages: Vec<(String, u32, u32)> = events
        .iter()
        .filter_map(|e| match e {
            Event::Page {
                id, done, total, ..
            } => Some((id.clone(), *done, *total)),
            _ => None,
        })
        .collect();
    assert_eq!(
        pages,
        [
            ("v1".into(), 1, 3),
            ("v1".into(), 2, 3),
            ("v1".into(), 3, 3),
            ("v2".into(), 1, 2),
            ("v2".into(), 2, 2)
        ]
    );
    for event in &events {
        match event {
            Event::Ready {
                startup_seconds,
                pipeline,
                precision,
                ..
            } => {
                assert!(*startup_seconds >= 0.2, "{startup_seconds}");
                assert_eq!(pipeline, "engine (cpu x1)");
                assert_eq!(precision.as_deref(), Some("fp32"));
            }
            Event::VolumeDone {
                id,
                pages,
                seconds,
                sidecar_sha256,
                ..
            } => {
                let path = results.join("s1").join(id).join(bunko_proto::RESULT_FILE);
                let bytes = std::fs::read(&path).unwrap();
                assert_eq!(
                    sidecar_sha256.as_deref(),
                    Some(hex::encode(sha2::Sha256::digest(&bytes)).as_str())
                );
                let sidecar: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(sidecar["pages"].as_array().unwrap().len() as u32, *pages);
                assert_eq!(sidecar["volume_uuid"], "vu");
                assert!(*seconds >= 0.0);
            }
            Event::Exit { returncode, .. } => assert_eq!(*returncode, Some(0)),
            _ => {}
        }
    }
    assert_eq!(fake.ran(), ["v1", "v2"]);
    quiet_for(&mut link.events, 200).await;
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_abandons_the_session_and_says_only_exit() {
    let dir = tempfile::tempdir().unwrap();
    let results = dir.path().join("results");
    let a = write_volume(dir.path(), "a.cbz", 20);
    let b = write_volume(dir.path(), "b.cbz", 20);
    let (mut link, _fake) = start(
        FakeConfig {
            page_delay: Duration::from_millis(30),
            ..Default::default()
        },
        &results,
    );
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    send(&link, volume_op("s1", "v2", &b)).await;
    // Wait for the first page of v1, then cancel both claims (as the library's kill does).
    loop {
        if let Event::Page { .. } = next_event(&mut link.events, 5).await {
            break;
        }
    }
    send(
        &link,
        Op::Cancel {
            sid: Some("s1".into()),
            claim: Some("v1".into()),
            bid: None,
        },
    )
    .await;
    send(
        &link,
        Op::Cancel {
            sid: Some("s1".into()),
            claim: Some("v2".into()),
            bid: None,
        },
    )
    .await;
    let events = until_exit(&mut link.events, "s1", 5).await;
    let terminal = events
        .iter()
        .any(|e| matches!(e, Event::VolumeDone { .. } | Event::VolumeFailed { .. }));
    assert!(
        !terminal,
        "a cancelled claim must not be reported: {events:#?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(Event::Exit {
                returncode: None,
                ..
            })
        ),
        "{events:#?}"
    );
    assert_eq!(exits(&events), 1);
    quiet_for(&mut link.events, 300).await;
    // Workspaces are gone.
    assert!(!results.join("s1").exists());
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_volume_costs_that_volume_only() {
    let dir = tempfile::tempdir().unwrap();
    let results = dir.path().join("results");
    let a = write_volume(dir.path(), "a.cbz", 2);
    let b = write_volume(dir.path(), "b.cbz", 2);
    let empty = dir.path().join("empty.cbz");
    std::fs::write(&empty, stored_zip(&[("notes.txt", b"no images")])).unwrap();
    let config = FakeConfig {
        fail_volumes: HashSet::from(["v1".to_string()]),
        ..Default::default()
    };
    let (mut link, _fake) = start(config, &results);
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    send(&link, volume_op("s1", "v2", &b)).await;
    let mut seen = Vec::new();
    while seen
        .iter()
        .filter(|e| matches!(e, Event::VolumeDone { .. } | Event::VolumeFailed { .. }))
        .count()
        < 2
    {
        seen.push(next_event(&mut link.events, 5).await);
    }
    send(&link, volume_op("s1", "v3", &empty)).await;
    send(&link, volume_op("s1", "v4", &dir.path().join("gone.cbz"))).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    send(&link, Op::CloseSession { sid: "s1".into() }).await;
    seen.extend(until_exit(&mut link.events, "s1", 5).await);
    // A return is said at once (from the feeder); other claims' events run on.
    let all = names(&seen);
    assert!(all.contains(&"volume_returned:v4".to_string()), "{all:?}");
    let in_order: Vec<String> = all
        .into_iter()
        .filter(|n| n != "volume_returned:v4")
        .collect();
    assert_eq!(
        in_order,
        [
            "ready",
            "volume_started:v1",
            "volume_failed:v1",
            "volume_started:v2",
            "volume_done:v2",
            "volume_started:v3",
            "volume_failed:v3",
            "exit"
        ],
        "{seen:#?}"
    );
    for event in &seen {
        match event {
            Event::VolumeFailed { id, error, .. } if id == "v1" => {
                assert_eq!(error, "every page failed")
            }
            Event::VolumeFailed { id, error, .. } if id == "v3" => {
                assert_eq!(error, "no page images found in empty.cbz")
            }
            Event::VolumeStarted { id, pages, .. } if id == "v3" => assert_eq!(*pages, 0),
            Event::VolumeReturned { class, .. } => assert_eq!(class, "missing"),
            Event::Exit { returncode, .. } => assert_eq!(*returncode, Some(0)),
            _ => {}
        }
    }
    // A failed claim's directory is removed; the finished one waits for the server.
    assert!(!results.join("s1/v1").exists());
    assert!(
        results
            .join("s1/v2")
            .join(bunko_proto::RESULT_FILE)
            .exists()
    );
    link.shutdown().await;
}

/// Regression (upgrade test): the finished sidecar was written under the volume's own
/// name, which Windows refuses for `Who Is Ann? 1.mokuro` (and for `CON.mokuro`, the
/// console): such volumes could never be delivered from a Windows processor. The claim's
/// folder holds it as `RESULT_FILE`; the name only travels with the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sidecar_is_held_under_a_safe_name_not_the_volumes() {
    let dir = tempfile::tempdir().unwrap();
    let results = dir.path().join("results");
    let a = write_volume(dir.path(), "a.cbz", 2);
    let b = write_volume(dir.path(), "b.cbz", 2);
    let (mut link, _fake) = start(FakeConfig::default(), &results);
    send(&link, open("s1")).await;
    for (claim, archive, volume) in [("v1", &a, "Who Is Ann? 1"), ("v2", &b, "CON")] {
        let Op::Volume(mut op) = volume_op("s1", claim, archive) else {
            unreachable!()
        };
        op.sidecar_name = format!("{volume}.mokuro");
        op.volume_title = volume.into();
        send(&link, Op::Volume(op)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&link, Op::CloseSession { sid: "s1".into() }).await;
    let events = until_exit(&mut link.events, "s1", 10).await;
    let done: Vec<String> = names(&events)
        .into_iter()
        .filter(|n| n.starts_with("volume_done") || n.starts_with("volume_failed"))
        .collect();
    assert_eq!(done, ["volume_done:v1", "volume_done:v2"], "{events:#?}");
    for (claim, volume) in [("v1", "Who Is Ann? 1"), ("v2", "CON")] {
        let held: Vec<String> = std::fs::read_dir(results.join("s1").join(claim))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(held, [bunko_proto::RESULT_FILE], "{claim}");
        let sidecar: serde_json::Value = serde_json::from_slice(
            &std::fs::read(
                results
                    .join("s1")
                    .join(claim)
                    .join(bunko_proto::RESULT_FILE),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(sidecar["volume"], volume);
    }
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fatal_error_fails_every_accepted_volume_then_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_volume(dir.path(), "a.cbz", 3);
    let b = write_volume(dir.path(), "b.cbz", 3);
    let config = FakeConfig {
        fatal_volumes: HashSet::from(["v1".to_string()]),
        load_delay: Duration::from_millis(100),
        ..Default::default()
    };
    let (mut link, _fake) = start(config, &dir.path().join("results"));
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    send(&link, volume_op("s1", "v2", &b)).await;
    let events = until_exit(&mut link.events, "s1", 5).await;
    assert_eq!(
        names(&events),
        [
            "ready",
            "volume_started:v1",
            "volume_failed:v1",
            "volume_started:v2",
            "volume_failed:v2",
            "fatal",
            "exit"
        ],
        "{events:#?}"
    );
    let error = "fake failed to load: the recognizer is gone";
    for event in &events {
        match event {
            Event::VolumeFailed { error: e, .. } | Event::Fatal { error: e, .. } => {
                assert_eq!(e, error)
            }
            Event::Exit { returncode, .. } => assert_eq!(*returncode, Some(1)),
            _ => {}
        }
    }
    quiet_for(&mut link.events, 200).await;
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn models_that_will_not_load_are_fatal_and_blame_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_volume(dir.path(), "a.cbz", 1);
    let config = FakeConfig {
        fail_open: Some("no such model".into()),
        load_delay: Duration::from_millis(100),
        ..Default::default()
    };
    let (mut link, _fake) = start(config, &dir.path().join("results"));
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    let events = until_exit(&mut link.events, "s1", 5).await;
    assert_eq!(names(&events), ["fatal", "exit"], "{events:#?}");
    assert!(matches!(&events[0], Event::Fatal { error, .. } if error == "no such model"));
    assert!(matches!(
        &events[1],
        Event::Exit {
            returncode: Some(1),
            ..
        }
    ));
    // The session is gone: a late op is ignored.
    send(&link, volume_op("s1", "v2", &a)).await;
    quiet_for(&mut link.events, 200).await;
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn duplicates_and_a_third_outstanding_claim_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_volume(dir.path(), "a.cbz", 2);
    let config = FakeConfig {
        load_delay: Duration::from_millis(300),
        ..Default::default()
    };
    let (mut link, _fake) = start(config, &dir.path().join("results"));
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    send(&link, volume_op("s1", "v2", &a)).await;
    send(&link, volume_op("s1", "v3", &a)).await;
    // An id that is not an id drops the op whole.
    send(&link, volume_op("s1", "../x", &a)).await;
    let first = next_event(&mut link.events, 5).await;
    assert!(
        matches!(&first, Event::VolumeReturned { id, class, .. } if id == "v3" && class == "local"),
        "{first:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    send(&link, Op::CloseSession { sid: "s1".into() }).await;
    let events = until_exit(&mut link.events, "s1", 5).await;
    assert_eq!(
        names(&events),
        [
            "ready",
            "volume_started:v1",
            "volume_done:v1",
            "volume_started:v2",
            "volume_done:v2",
            "exit"
        ]
    );
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_volumes_finish_in_arrival_order_and_partition_time() {
    let dir = tempfile::tempdir().unwrap();
    let long = write_volume(dir.path(), "long.cbz", 10);
    let short = write_volume(dir.path(), "short.cbz", 1);
    let config = FakeConfig {
        overlap: 2,
        page_delay: Duration::from_millis(40),
        load_delay: Duration::from_millis(100),
        ..Default::default()
    };
    let (mut link, _fake) = start(config, &dir.path().join("results"));
    let began = std::time::Instant::now();
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &long)).await;
    send(&link, volume_op("s1", "v2", &short)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    send(&link, Op::CloseSession { sid: "s1".into() }).await;
    let events = until_exit(&mut link.events, "s1", 10).await;
    let elapsed = began.elapsed().as_secs_f64();
    let done: Vec<(String, f64)> = events
        .iter()
        .filter_map(|e| match e {
            Event::VolumeDone { id, seconds, .. } => Some((id.clone(), *seconds)),
            _ => None,
        })
        .collect();
    assert_eq!(
        done.iter().map(|d| d.0.as_str()).collect::<Vec<_>>(),
        ["v1", "v2"],
        "{events:#?}"
    );
    let total: f64 = done.iter().map(|d| d.1).sum();
    assert!(
        total <= elapsed,
        "volume seconds {total} exceed the session's {elapsed}"
    );
    assert!(done[0].1 >= 0.35, "v1 ran ten 40 ms pages: {}", done[0].1);
    // v2 ran beside v1, so its share after v1's end is small.
    assert!(done[1].1 < 0.1, "{done:?}");
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stats_flow_while_pages_do() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_volume(dir.path(), "a.cbz", 25);
    let config = FakeConfig {
        page_delay: Duration::from_millis(100),
        ..Default::default()
    };
    let (mut link, _fake) = start(config, &dir.path().join("results"));
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    let mut stats = Vec::new();
    loop {
        match next_event(&mut link.events, 10).await {
            Event::Stats {
                pipeline,
                cpu_pressure,
                other_cpu,
                ..
            } => stats.push((pipeline, cpu_pressure, other_cpu)),
            Event::VolumeDone {
                stats: volume_stats,
                ..
            } => {
                assert_eq!(volume_stats["stages"][0]["key"], "engine");
                break;
            }
            _ => {}
        }
    }
    assert!(!stats.is_empty(), "a 2.5 s volume reports stats");
    assert!(
        stats.len() <= 2,
        "stats are at most every 2 s: {}",
        stats.len()
    );
    assert_eq!(stats[0].0["bottleneck"], "engine");
    if std::path::Path::new("/proc/pressure/cpu").exists() {
        assert!(stats[0].1.is_some());
    }
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_says_nothing_more() {
    let dir = tempfile::tempdir().unwrap();
    let a = write_volume(dir.path(), "a.cbz", 50);
    let config = FakeConfig {
        page_delay: Duration::from_millis(20),
        ..Default::default()
    };
    let (mut link, _fake) = start(config, &dir.path().join("results"));
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    loop {
        if let Event::Page { .. } = next_event(&mut link.events, 5).await {
            break;
        }
    }
    link.shutdown().await;
    while let Ok(event) = link.events.try_recv() {
        assert!(
            matches!(event, Event::Page { .. } | Event::Stats { .. }),
            "after leaving: {event:?}"
        );
    }
    assert!(link.events.recv().await.is_none(), "the hub is gone");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bench_runs_beside_sessions_and_a_repeated_bid_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let sample = write_volume(dir.path(), "bench-1.cbz", 8);
    let a = write_volume(dir.path(), "a.cbz", 3);
    let config = FakeConfig {
        page_delay: Duration::from_millis(2),
        ..Default::default()
    };
    let fake = FakePipeline::new(config);
    let quick = bunko_processor::BenchConfig {
        rules: bunko_sched::bench::WindowRules {
            min_window_seconds: 0.1,
            short_window_seconds: 0.05,
            ..Default::default()
        },
        workers_budget: Some(1),
        ..Default::default()
    };
    let mut link = LocalProcessor::spawn_with(
        Arc::new(fake),
        LocalConfig {
            results_dir: dir.path().join("results"),
        },
        quick,
    );
    let op = Op::Bench(BenchOp {
        bid: "bench-1".into(),
        spec: row("fake"),
        sample: sample.to_string_lossy().into_owned(),
        pages: 8,
        precision_only: false,
    });
    send(&link, op.clone()).await;
    send(&link, op).await;
    send(&link, open("s1")).await;
    send(&link, volume_op("s1", "v1", &a)).await;
    send(&link, Op::CloseSession { sid: "s1".into() }).await;
    let mut exits = Vec::new();
    let mut done = 0;
    while exits.len() < 2 {
        match next_event(&mut link.events, 60).await {
            Event::Exit { sid, .. } => exits.push(sid),
            Event::BenchDone { bid, .. } if bid == "bench-1" => done += 1,
            _ => {}
        }
    }
    exits.sort();
    assert_eq!(exits, ["bench-1", "s1"]);
    assert_eq!(done, 1, "one benchmark for one bid");
    quiet_for(&mut link.events, 300).await;
    link.shutdown().await;
}
