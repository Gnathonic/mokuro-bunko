//! OCR one archive with one engine, the way a processor session does, and print the
//! timing and pipeline counters.
//!
//! ```text
//! MOKURO_MODELS_DIR=~/.cache/mokuro-bunko-demo/models-v1 \
//! cargo run -p bunko-engines --release --example ocr_volume -- \
//!     hayai-nova path/to/volume.cbz out.mokuro [--backend cpu] [--precision fp32] \
//!     [--stage-workers detect=3] [--device gpu:0] [--uuids TITLE,VOLUME]
//! ```
//!
//! Memory is reported from `/proc/self/status` (Linux): RSS once the session is
//! loaded, the mean and largest RSS sampled every 50 ms while the volume runs, the
//! RSS at the end, and the process peak (`VmHWM`). The example uses mimalloc like the
//! `mokuro-bunko` binary, so the figures match the server's. `--uuids` fixes the
//! sidecar's title/volume uuids so two runs can be compared byte for byte.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bunko_engines::{Backend, EngineConfig, EnginePipeline};
use bunko_processor::{CancelToken, PagePipeline, PageProgress, VolumeMeta};
use bunko_proto::{PoolsSpec, RowSpec};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// A `/proc/self/status` field in MiB (0 where there is none).
fn status_mib(field: &str) -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        bail!(
            "usage: ocr_volume <engine> <archive.cbz> <out.mokuro> [--backend B] [--precision P] [--stage-workers k=n,...] [--device D] [--models DIR]"
        );
    }
    let (engine, archive, out) = (&args[0], PathBuf::from(&args[1]), PathBuf::from(&args[2]));
    let mut backend = Backend::Auto;
    let mut precision = "auto-accuracy".to_string();
    let mut pools = PoolsSpec::default();
    let mut models = std::env::temp_dir().join("bunko-engines-models");
    let mut uuids: (Option<String>, Option<String>) = (None, None);
    let mut i = 3;
    while i + 1 < args.len() {
        let v = &args[i + 1];
        match args[i].as_str() {
            "--backend" => backend = Backend::parse(v),
            "--precision" => precision = v.clone(),
            "--device" => {
                pools.stage_device.insert("engine".into(), v.clone());
            }
            "--models" => models = PathBuf::from(v),
            "--uuids" => {
                let (t, v) = v.split_once(',').context("--uuids TITLE,VOLUME")?;
                uuids = (Some(t.into()), Some(v.into()));
            }
            "--stage-workers" => {
                for kv in v.split(',') {
                    let (k, n) = kv.split_once('=').context("k=n")?;
                    pools.stage_workers.insert(k.into(), n.parse()?);
                }
            }
            other => bail!("unknown flag {other}"),
        }
        i += 2;
    }
    let pipeline = EnginePipeline::new(EngineConfig::new(models, backend));
    let info = pipeline.describe();
    println!("catalog: {}", serde_json::to_string(&info.catalog)?);
    let spec = RowSpec {
        id: "g-1".into(),
        name: engine.clone(),
        engine: engine.clone(),
        detector: None,
        patch_budget: 512,
        precision,
        pools,
        precision_pick: None,
        precision_why: String::new(),
        primary: true,
    };
    let t0 = Instant::now();
    let runner = pipeline.open(&spec).map_err(anyhow::Error::msg)?;
    let load = t0.elapsed().as_secs_f64();
    let ready = runner.ready();
    println!(
        "ready in {load:.2}s: {} precision={:?} weights={:?}",
        ready.pipeline, ready.precision, ready.weights
    );
    let stem = archive.file_stem().unwrap().to_string_lossy().into_owned();
    let meta = VolumeMeta {
        claim: "v1".into(),
        title: "Series".into(),
        volume: stem.clone(),
        title_uuid: uuids.0,
        volume_uuid: uuids.1,
        stem,
        sidecar_name: out.file_name().unwrap().to_string_lossy().into_owned(),
    };
    let loaded_rss = status_mib("VmRSS:");
    let loaded_split = (status_mib("RssAnon:"), status_mib("RssFile:"));
    let sampling = AtomicBool::new(true);
    let t1 = Instant::now();
    let first = std::sync::Mutex::new(None::<f64>);
    let progress = |p: PageProgress| {
        if let PageProgress::Page { done, total } = p {
            let mut f = first.lock().unwrap();
            if f.is_none() {
                *f = Some(t1.elapsed().as_secs_f64());
            }
            if done % 10 == 0 || done == total {
                eprintln!("page {done}/{total} at {:.1}s", t1.elapsed().as_secs_f64());
            }
        }
    };
    let (outcome, samples) = std::thread::scope(|scope| {
        let sampler = scope.spawn(|| {
            let mut samples = Vec::new();
            while sampling.load(Ordering::Relaxed) {
                samples.push(status_mib("VmRSS:"));
                std::thread::sleep(Duration::from_millis(50));
            }
            samples
        });
        let outcome = runner.run_volume(&archive, &meta, &out, &progress, &CancelToken::new());
        sampling.store(false, Ordering::Relaxed);
        (outcome, sampler.join().unwrap_or_default())
    });
    let outcome = outcome.map_err(|e| anyhow::anyhow!("{e}"))?;
    let secs = t1.elapsed().as_secs_f64();
    let mean = samples.iter().sum::<f64>() / samples.len().max(1) as f64;
    let max = samples.iter().copied().fold(0.0f64, f64::max);
    println!(
        "rss_mib: loaded={loaded_rss:.0} run_mean={mean:.0} run_max={max:.0} end={:.0} peak_hwm={:.0} (anon/file: loaded={:.0}/{:.0} end={:.0}/{:.0})",
        status_mib("VmRSS:"),
        status_mib("VmHWM:"),
        loaded_split.0,
        loaded_split.1,
        status_mib("RssAnon:"),
        status_mib("RssFile:"),
    );
    let first = first.lock().unwrap().unwrap_or(0.0);
    println!(
        "pages={} failed={} seconds={secs:.2} pages/s={:.3} steady pages/s={:.3} (first page at {first:.2}s)",
        outcome.pages,
        outcome.failed_pages,
        f64::from(outcome.pages) / secs,
        (f64::from(outcome.pages) - 1.0) / (secs - first).max(1e-3),
    );
    if let Some(stats) = outcome.stats {
        println!("stats: {}", serde_json::to_string(&stats.to_value())?);
    }
    Ok(())
}
