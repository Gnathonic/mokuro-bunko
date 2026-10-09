//! A benchmark end to end over the network: the real server app on a TCP port, a real
//! remote processor (`bunko_processor::serve`, fake engines) logged in to it, and the
//! admin API asking for a benchmark of a row on that processor — the sample fetched
//! over HTTP, the `bench_*` events over the socket, the result stored in the
//! processor's profile.

use std::time::Duration;

use bunko_core::generations::default_generation;
use bunko_core::{Config, Role};
use bunko_db::UserStatus;
use bunko_processor::config::{LibrarySettings, ProcessorSettings, TlsVerify};
use bunko_processor::{BenchConfig, FakeConfig, FakePipeline, ProcessorConfig};
use bunko_sched::bench::WindowRules;
use bunko_server::app::{ServeOptions, Services, assemble};
use serde_json::{Value, json};

mod ocr_common;

fn basic(user: &str, pass: &str) -> String {
    use base64::Engine;
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"))
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_remote_processor_is_benchmarked_through_the_admin_api() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.base_path = dir.path().join("storage");
    config.ocr.autobench = false;
    config.ocr.local_processing = false;
    let mut row = default_generation("g-1");
    row.precision = "auto-balanced".into();
    config.ocr.generations = vec![row.clone()];
    let layout = config.storage.layout();
    layout.ensure_directories().unwrap();
    for v in ["A/V1.cbz", "A/V2.cbz", "B/V1.cbz"] {
        ocr_common::write_cbz(&layout.library().join(v), 10);
    }
    let opts = ServeOptions::default();
    let services = Services::new(config, None, &opts).unwrap();
    services
        .db
        .create_user(
            "boss",
            "boss-password-1",
            Role::Admin,
            UserStatus::Active,
            "",
        )
        .unwrap();
    services
        .db
        .create_user(
            "gpu",
            "processor-pass-1",
            Role::Processor,
            UserStatus::Active,
            "",
        )
        .unwrap();
    let app = assemble(&services, &opts);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let stop = services.stop.clone();
    services.ocr.start(stop.child_token());
    let server = tokio::spawn(bunko_server::serve::serve(
        listener,
        app,
        None,
        stop.clone(),
        Duration::from_secs(1),
    ));

    // The processor: a card computing in every format, bf16 three times faster, and a
    // second engine worker a quarter slower (with widths changing nothing, timing noise
    // alone could make the width search keep one).
    let fake = FakePipeline::new(FakeConfig {
        engines: vec!["hayai-nova".into(), "ppocr-manga".into()],
        gpu_formats: Some(vec!["fp32".into(), "fp16".into(), "bf16".into()]),
        precision_page_delay: [
            ("bf16".to_string(), Duration::from_millis(3)),
            ("fp32".to_string(), Duration::from_millis(9)),
        ]
        .into(),
        useful_width: 1,
        ..Default::default()
    });
    let mut options = bunko_processor::ServeOptions::new(
        ProcessorConfig {
            library: LibrarySettings {
                url: url.clone(),
                username: "gpu".into(),
                password: "processor-pass-1".into(),
                tls_verify: TlsVerify::Yes,
            },
            processor: ProcessorSettings {
                name: "tower".into(),
                public_name: None,
                max_sessions: 1,
                storage: dir.path().join("processor"),
                archive_memory_mb: 64,
                auto_update: false,
            },
            update: Default::default(),
        },
        std::sync::Arc::new(fake),
    );
    options.bench = BenchConfig {
        rules: WindowRules {
            min_window_seconds: 0.3,
            short_window_seconds: 0.15,
            ..WindowRules::default()
        },
        progress_interval: Duration::from_millis(50),
        sample_interval: Duration::from_millis(20),
        workers_budget: Some(2),
        ..BenchConfig::default()
    };
    let processor_stop = options.shutdown.clone();
    let processor = tokio::spawn(bunko_processor::serve(options));

    let http = reqwest::Client::new();
    let admin = basic("boss", "boss-password-1");
    let get = |path: String| {
        let (http, admin) = (http.clone(), admin.clone());
        async move {
            http.get(path)
                .header("authorization", admin)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    // Wait for the registration.
    let mut connected = false;
    for _ in 0..100 {
        let p = get(format!("{url}/_admin/api/processors")).await;
        if p["processors"]
            .as_array()
            .is_some_and(|l| l.iter().any(|e| e["name"] == "tower"))
        {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(connected, "the processor registered");

    // Listed once it registered; benchmarkable once its socket is open too (a moment
    // later, which a slow CI runner can make longer than the listing poll above).
    let mut answer = (0, String::new());
    for _ in 0..100 {
        let posted = http
            .post(format!("{url}/_admin/api/ocr/generations/g-1/bench"))
            .header("authorization", admin.clone())
            .json(&json!({"processor": "tower", "pages": 8}))
            .send()
            .await
            .unwrap();
        answer = (posted.status().as_u16(), posted.text().await.unwrap());
        if !(answer.0 == 400 && answer.1.contains("is connected")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(answer.0, 202, "{}", answer.1);
    let mut result = Value::Null;
    for _ in 0..600 {
        result = get(format!(
            "{url}/_admin/api/ocr/generations/g-1/bench?processor=tower"
        ))
        .await;
        if matches!(result["state"].as_str(), Some("done" | "failed")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(result["state"], "done", "{result:#}");
    assert_eq!(result["processor"], "tower");
    assert_eq!(result["sample"], json!({"pages": 8, "volumes": 2}));
    assert_eq!(result["host"]["devices"]["engine"], "gpu:0");
    assert_eq!(result["precision"], "bf16");
    assert_eq!(result["precision_mode"], "auto-balanced");
    let trials = result["trials"].as_array().unwrap();
    assert_eq!(trials[0]["note"], "precision bf16");
    assert_eq!(trials[1]["note"], "precision fp32");
    // The processor probes CPU busy (`/proc/stat`) on Linux only.
    assert!(
        trials
            .iter()
            .all(|t| t["cpu_busy_pct"].is_number() == cfg!(target_os = "linux"))
    );
    assert!(result["best"]["pages_per_second"].as_f64().unwrap() > 0.0);
    assert_eq!(result["best"]["same_as_spec"], true, "{result:#}");
    assert!(
        result["estimates"]["volume_200_pages_seconds"].is_number(),
        "{result:#}"
    );
    // A processor's result is ITS: in its profile, never in `.ocr-bench.json`.
    let storage = dir.path().join("storage");
    assert!(!storage.join(".ocr-bench.json").exists());
    let profile: Value = serde_json::from_str(
        &std::fs::read_to_string(storage.join("processors/tower.json")).unwrap(),
    )
    .unwrap();
    let bench = &profile["rows"]["g-1"]["bench"];
    assert_eq!(bench["precision"], "bf16");
    assert_eq!(bench["precision_trials"].as_array().unwrap().len(), 2);
    assert_eq!(bench["at"], result["finished_at"]);
    // The sample is gone from the library's workspace.
    let leftovers: Vec<_> = std::fs::read_dir(storage.join(".processing"))
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        leftovers
            .iter()
            .all(|n| !n.to_string_lossy().starts_with("bench-")),
        "{leftovers:?}"
    );
    // The admin's generations list says where the row's precision stands there.
    let generations = get(format!("{url}/_admin/api/ocr/generations")).await;
    let on = &generations["generations"][0]["precision_on"]["tower"]["auto-balanced"];
    assert_eq!(on["precision"], "bf16", "{generations:#}");
    assert_eq!(on["bench"], "done");
    assert_eq!(on["trials"].as_array().unwrap().len(), 2);
    let speed = get(format!("{url}/_admin/api/processors")).await;
    let layer = &speed["speed"][0]["layers"][0];
    assert_eq!(speed["speed"][0]["name"], "tower");
    assert!(layer["bench_pages_per_minute"].as_f64().unwrap() > 0.0);

    processor_stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(15), processor).await;
    stop.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(10), server).await;
    services.ocr.stop().await;
}
