//! Benchmarks through the local link over the fake pipeline: the event stream, the
//! width search, the precision phase, precision-only runs, cancels and failures.

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use bunko_processor::{
    BenchConfig, FakeConfig, FakePipeline, LocalConfig, LocalLink, LocalProcessor,
};
use bunko_proto::{BenchOp, Event, Op, RowSpec};
use bunko_sched::bench::WindowRules;
use common::{next_event, row, until_exit};
use serde_json::{Map, Value, json};

fn quick() -> BenchConfig {
    BenchConfig {
        budget: Duration::from_secs(60),
        progress_interval: Duration::from_millis(20),
        sample_interval: Duration::from_millis(10),
        rules: WindowRules {
            min_window_seconds: 0.3,
            short_window_seconds: 0.15,
            ..WindowRules::default()
        },
        workers_budget: Some(3),
        ..BenchConfig::default()
    }
}

/// A stored sample of `n` pages named the way the library names them.
fn sample(dir: &Path, n: usize) -> PathBuf {
    let pages: Vec<(String, Vec<u8>)> = (0..n)
        .map(|i| (format!("{i:04}_Vol_01.jpg"), vec![i as u8; 64]))
        .collect();
    let refs: Vec<(&str, &[u8])> = pages
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    let path = dir.join("bench-x.cbz");
    std::fs::write(&path, common::stored_zip(&refs)).unwrap();
    path
}

fn start(config: FakeConfig, dir: &Path) -> (LocalLink, FakePipeline) {
    let fake = FakePipeline::new(config);
    let link = LocalProcessor::spawn_with(
        std::sync::Arc::new(fake.clone()),
        LocalConfig {
            results_dir: dir.join("results"),
        },
        quick(),
    );
    (link, fake)
}

fn bench_op(bid: &str, spec: RowSpec, sample: &Path, precision_only: bool) -> Op {
    Op::Bench(BenchOp {
        bid: bid.into(),
        spec,
        sample: sample.to_string_lossy().into_owned(),
        pages: 16,
        precision_only,
    })
}

fn spec(engine: &str, precision: &str, workers: &[(&str, u32)]) -> RowSpec {
    let mut s = row(engine);
    s.precision = precision.into();
    s.pools.stage_workers = workers.iter().map(|(k, v)| (k.to_string(), *v)).collect();
    s
}

#[derive(Default, Debug)]
struct Seen {
    ready: Option<Map<String, Value>>,
    progress: Vec<Map<String, Value>>,
    trials: Vec<Map<String, Value>>,
    done: Option<Map<String, Value>>,
    fatal: Option<String>,
    exit: Option<Option<i32>>,
}

fn sort(events: Vec<Event>, bid: &str) -> Seen {
    let mut seen = Seen::default();
    for e in events {
        match e {
            Event::BenchReady { bid: b, detail } if b == bid => seen.ready = Some(detail),
            Event::BenchProgress { bid: b, detail } if b == bid => seen.progress.push(detail),
            Event::BenchTrial { bid: b, detail } if b == bid => seen.trials.push(detail),
            Event::BenchDone { bid: b, detail } if b == bid => seen.done = Some(detail),
            Event::Fatal { sid, error } if sid == bid => seen.fatal = Some(error),
            Event::Exit { sid, returncode } if sid == bid => seen.exit = Some(returncode),
            other => panic!("unexpected {other:?}"),
        }
    }
    seen
}

async fn run(config: FakeConfig, spec: RowSpec, precision_only: bool) -> (Seen, FakePipeline) {
    let dir = tempfile::tempdir().unwrap();
    let path = sample(dir.path(), 16);
    let (mut link, fake) = start(config, dir.path());
    link.ops
        .send(bench_op("bench-1", spec, &path, precision_only))
        .await
        .unwrap();
    let events = until_exit(&mut link.events, "bench-1", 120).await;
    link.shutdown().await;
    assert!(
        !dir.path().join("results").join("bench-1").exists(),
        "the workspace is removed"
    );
    (sort(events, "bench-1"), fake)
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or_else(|| panic!("not a number: {v}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_width_search_widens_what_the_verdict_names_while_it_pays() {
    let config = FakeConfig {
        page_delay: Duration::from_millis(4),
        useful_width: 2,
        ..Default::default()
    };
    // The row's own width is NOT what is measured: the search starts from the
    // derivation (1 for the fake).
    let (seen, fake) = run(
        config,
        spec("fake", "auto-accuracy", &[("engine", 3)]),
        false,
    )
    .await;
    assert_eq!(seen.fatal, None);
    assert_eq!(seen.exit, Some(Some(0)));
    let ready = seen.ready.expect("bench_ready");
    assert_eq!(ready["pages"], 16);
    assert_eq!(ready["tunable"], true);
    assert_eq!(ready["max_trials"], 8);
    assert_eq!(ready["stage_keys"], json!(["engine"]));
    assert_eq!(ready["stage_device"], json!({"engine": "cpu"}));
    assert_eq!(ready["min_window_seconds"], 0.3);
    assert!(f(&ready["startup_seconds"]) >= f(&ready["model_load_seconds"]));
    let notes: Vec<(&str, bool)> = seen
        .trials
        .iter()
        .map(|t| (t["note"].as_str().unwrap(), t["accepted"] == true))
        .collect();
    assert_eq!(
        notes,
        [
            ("auto", true),
            ("widen engine to 2", true),
            ("widen engine to 3", false)
        ],
        "{:#?}",
        seen.trials
    );
    let first = &seen.trials[0];
    for key in [
        "n",
        "note",
        "stage_workers",
        "queue_capacity",
        "stage_device",
        "seconds",
        "pages_per_second",
        "window_seconds",
        "pages_measured",
        "passes",
        "short_window",
        "first_emission_at",
        "last_emission_at",
        "accepted",
        "verdict",
        "bottleneck",
        "stages",
        "queues",
        "precision",
        "gpu_busy_pct",
        "cpu_busy_pct",
    ] {
        assert!(first.contains_key(key), "trial lacks {key}: {first:?}");
    }
    assert_eq!(first["stage_workers"], json!({"engine": 1}));
    assert_eq!(
        first["verdict"],
        "engine busy 100% of 1 worker \u{2014} widen engine"
    );
    assert_eq!(first["bottleneck"], "engine");
    assert_eq!(first["stages"][0]["busy_pct"], 100);
    assert_eq!(first["queues"][0]["name"], "engine->out");
    assert!(
        first["window_seconds"].as_f64().unwrap() >= 0.3,
        "the sample is re-fed until the window is long enough: {first:?}"
    );
    assert!(first["passes"].as_u64().unwrap() >= 1);
    // CPU busy (`/proc/stat`) and peak RSS (`getrusage`) are probed on Linux only.
    assert_eq!(
        first["cpu_busy_pct"].is_number(),
        cfg!(target_os = "linux"),
        "{first:?}"
    );
    let done = seen.done.expect("bench_done");
    assert_eq!(done["best"]["trial"], 2);
    assert_eq!(done["best"]["stage_workers"], json!({"engine": 2}));
    assert_eq!(done["best"]["queue_capacity"], json!({}));
    assert_eq!(done["best"]["stage_device"], json!({}));
    // Wall-clock figures. The fake's page times are sleeps, only as exact as the host's
    // timer (a macOS runner turns a 4 ms page into ~25 ms), so what is checked is that
    // the accepted width paid and that the bench's arithmetic agrees with itself.
    let best = f(&done["best"]["pages_per_second"]);
    let baseline = f(&done["baseline"]["pages_per_second"]);
    assert!(baseline > 0.0 && best > baseline, "{done:?}");
    assert!(
        (f(&done["best"]["speedup"]) - best / baseline).abs() < 1e-3,
        "{done:?}"
    );
    assert_eq!(done["precision"], "fp32");
    assert_eq!(done["precision_mode"], "auto-accuracy");
    assert!(!done.contains_key("precision_trials"));
    if cfg!(target_os = "linux") {
        assert!(done["peak_rss_mb"].as_i64().unwrap() > 0);
    } else {
        assert!(done["peak_rss_mb"].is_null());
    }
    assert!(done.contains_key("peak_vram_mb"));
    // Progress came at most every 20 ms, inside trials only.
    assert!(!seen.progress.is_empty());
    assert!(
        seen.progress
            .iter()
            .all(|p| p["trial"].as_u64().unwrap() >= 1)
    );
    // One load for the first runner, one per width tried.
    assert_eq!(fake.opened(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engines_ceiling_bounds_the_search() {
    let config = FakeConfig {
        page_delay: Duration::from_millis(4),
        useful_width: 4,
        width_ceiling: Some(2),
        ..Default::default()
    };
    let (seen, _) = run(config, spec("fake", "auto-accuracy", &[]), false).await;
    let notes: Vec<&str> = seen
        .trials
        .iter()
        .map(|t| t["note"].as_str().unwrap())
        .collect();
    assert_eq!(notes, ["auto", "widen engine to 2"], "{:#?}", seen.trials);
    assert_eq!(
        seen.done.unwrap()["best"]["stage_workers"],
        json!({"engine": 2})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_speed_row_tries_each_candidate_and_keeps_the_fastest() {
    let config = FakeConfig {
        engines: vec!["hayai-nova".into()],
        gpu_formats: Some(vec!["fp32".into(), "fp16".into(), "bf16".into()]),
        precision_page_delay: BTreeMap::from([
            ("bf16".to_string(), Duration::from_millis(4)),
            ("fp16".to_string(), Duration::from_millis(7)),
            ("fp32".to_string(), Duration::from_millis(12)),
        ]),
        ..Default::default()
    };
    let (seen, _) = run(config, spec("hayai-nova", "auto-speed", &[]), false).await;
    assert_eq!(seen.fatal, None);
    let ready = seen.ready.unwrap();
    assert_eq!(ready["max_trials"], 11, "the phase brings its own trials");
    assert_eq!(ready["stage_device"], json!({"engine": "gpu:0"}));
    let notes: Vec<&str> = seen
        .trials
        .iter()
        .map(|t| t["note"].as_str().unwrap())
        .collect();
    assert_eq!(
        &notes[..3],
        ["precision bf16", "precision fp16", "precision fp32"]
    );
    assert_eq!(seen.trials[0]["accepted"], true);
    assert_eq!(seen.trials[1]["accepted"], false);
    assert_eq!(seen.trials[2]["accepted"], false);
    assert_eq!(seen.trials[2]["precision"], "fp32");
    let done = seen.done.unwrap();
    assert_eq!(done["precision"], "bf16");
    assert_eq!(done["precision_mode"], "auto-speed");
    let trials = done["precision_trials"].as_array().unwrap();
    assert_eq!(trials.len(), 3);
    assert_eq!(trials[0]["precision"], "bf16");
    assert_eq!(trials[0]["chosen"], true);
    assert_eq!(trials[2]["chosen"], false);
    assert!(
        done["precision_why"]
            .as_str()
            .unwrap()
            .starts_with("benchmark: bf16 "),
        "{done:?}"
    );
    // The winning precision trial is the width search's baseline.
    assert_eq!(
        done["baseline"]["pages_per_second"],
        trials[0]["pages_per_second"]
    );
    // Every trial after the phase runs at the pick.
    assert!(seen.trials[3..].iter().all(|t| t["precision"] == "bf16"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_candidate_that_will_not_load_is_reported_at_zero() {
    let config = FakeConfig {
        engines: vec!["hayai-nova".into()],
        gpu_formats: Some(vec!["fp32".into(), "fp16".into(), "bf16".into()]),
        missing_formats: vec!["fp16".into()],
        precision_page_delay: BTreeMap::from([
            ("bf16".to_string(), Duration::from_millis(4)),
            ("fp32".to_string(), Duration::from_millis(8)),
        ]),
        ..Default::default()
    };
    let (seen, _) = run(config, spec("hayai-nova", "auto-speed", &[]), true).await;
    assert_eq!(seen.fatal, None);
    let done = seen.done.unwrap();
    let trials = done["precision_trials"].as_array().unwrap();
    let formats: Vec<&str> = trials
        .iter()
        .map(|t| t["precision"].as_str().unwrap())
        .collect();
    assert_eq!(
        formats,
        ["bf16", "fp16", "fp32"],
        "every candidate, in order"
    );
    assert_eq!(trials[1]["pages_per_second"], 0.0);
    assert_eq!(trials[1]["chosen"], false);
    assert_eq!(done["precision"], "bf16");
    assert_eq!(seen.trials.len(), 2, "no trial for what would not load");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_without_native_bf16_tries_no_bf16() {
    // sm_75: bf16 emulated (a package exists, as on RDNA2): balanced has fp32 only.
    let config = FakeConfig {
        engines: vec!["hayai-nova".into()],
        gpu_formats: Some(vec!["fp32".into(), "fp16".into(), "bf16".into()]),
        gpu_arch: Some("sm_75".into()),
        ..Default::default()
    };
    let (seen, _) = run(
        config.clone(),
        spec("hayai-nova", "auto-balanced", &[]),
        false,
    )
    .await;
    let done = seen.done.unwrap();
    assert_eq!(done["precision"], "fp32");
    assert!(!done.contains_key("precision_trials"));
    // auto-speed tries fp16 and fp32, never bf16.
    let (seen, _) = run(config, spec("hayai-nova", "auto-speed", &[]), false).await;
    let done = seen.done.unwrap();
    let tried: Vec<&str> = done["precision_trials"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["precision"].as_str().unwrap())
        .collect();
    assert_eq!(tried, ["fp16", "fp32"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn precision_only_keeps_the_pools_and_tunes_nothing() {
    let config = FakeConfig {
        engines: vec!["paddle-manga".into()],
        gpu_formats: Some(vec!["fp32".into(), "fp16".into(), "bf16".into()]),
        precision_page_delay: BTreeMap::from([
            ("bf16".to_string(), Duration::from_millis(8)),
            ("fp32".to_string(), Duration::from_millis(3)),
        ]),
        useful_width: 4,
        ..Default::default()
    };
    let (seen, _) = run(
        config,
        spec("paddle-manga", "auto-balanced", &[("engine", 2)]),
        true,
    )
    .await;
    assert_eq!(seen.fatal, None);
    let ready = seen.ready.unwrap();
    assert_eq!(ready["tunable"], false);
    assert_eq!(ready["max_trials"], 3);
    assert_eq!(seen.trials.len(), 2);
    // The machine's pools exactly as given.
    assert!(
        seen.trials
            .iter()
            .all(|t| t["stage_workers"] == json!({"engine": 2}))
    );
    let done = seen.done.unwrap();
    assert_eq!(done["precision"], "fp32", "fp32 is clearly faster here");
    assert!(
        done["precision_why"]
            .as_str()
            .unwrap()
            .contains("beat bf16"),
        "{done:?}"
    );
    assert_eq!(done["best"]["trial"], 2);
    assert_eq!(done["best"]["stage_workers"], json!({}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_candidate_means_no_phase_and_a_cpu_runs_fp32() {
    let config = FakeConfig {
        engines: vec!["hayai-nova".into()],
        ..Default::default()
    };
    let (seen, _) = run(config, spec("hayai-nova", "auto-speed", &[]), false).await;
    let done = seen.done.unwrap();
    assert_eq!(done["precision"], "fp32");
    assert_eq!(done["precision_mode"], "auto-speed");
    assert!(!done.contains_key("precision_trials"));
    assert_eq!(seen.ready.unwrap()["max_trials"], 8);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_ends_it_with_cancelled_and_exit() {
    let dir = tempfile::tempdir().unwrap();
    let path = sample(dir.path(), 16);
    let config = FakeConfig {
        page_delay: Duration::from_millis(20),
        ..Default::default()
    };
    let (mut link, _fake) = start(config, dir.path());
    link.ops
        .send(bench_op("bench-2", row("fake"), &path, false))
        .await
        .unwrap();
    loop {
        if let Event::BenchReady { .. } = next_event(&mut link.events, 10).await {
            break;
        }
    }
    link.ops
        .send(Op::Cancel {
            sid: None,
            claim: None,
            bid: Some("bench-2".into()),
        })
        .await
        .unwrap();
    let rest = until_exit(&mut link.events, "bench-2", 10).await;
    let seen = sort(
        rest.into_iter()
            .filter(|e| !matches!(e, Event::BenchProgress { .. } | Event::BenchTrial { .. }))
            .collect(),
        "bench-2",
    );
    assert_eq!(seen.fatal.as_deref(), Some("cancelled"));
    assert!(seen.done.is_none());
    assert_eq!(seen.exit, Some(Some(1)));
    link.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_that_will_not_load_and_a_missing_sample_are_fatal() {
    let config = FakeConfig {
        fail_open: Some("could not load hayai-nova: no package".into()),
        ..Default::default()
    };
    let (seen, _) = run(config, row("fake"), false).await;
    assert_eq!(
        seen.fatal.as_deref(),
        Some("could not load hayai-nova: no package")
    );
    assert!(seen.ready.is_none());
    assert_eq!(seen.exit, Some(Some(1)));

    let dir = tempfile::tempdir().unwrap();
    let (mut link, _) = start(FakeConfig::default(), dir.path());
    link.ops
        .send(bench_op(
            "bench-3",
            row("fake"),
            &dir.path().join("gone.cbz"),
            false,
        ))
        .await
        .unwrap();
    let seen = sort(until_exit(&mut link.events, "bench-3", 10).await, "bench-3");
    assert!(
        seen.fatal
            .as_deref()
            .unwrap()
            .starts_with("could not fetch the benchmark sample: there is no "),
        "{seen:?}"
    );
    link.shutdown().await;
}
