//! Run a processor's startup benchmark (the width search the library asks for) on this
//! machine with the real engines, and print its trials and verdicts.
//!
//! ```text
//! cargo run -p bunko-engines --release --features nc-models,gpl-models --example bench_local -- \
//!     nar-small sample.cbz --models DIR [--detector animetext-blocks] [--pages 24] \
//!     [--stage-workers detect=2,engine=10] [--budget-seconds 900] [--trials 8]
//! ```
//!
//! The trials, the verdicts and the winner are logged as the processor logs them
//! (`benchmark b-1: trial N: ... verdict: ...`); the `bench_done` detail is printed last.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bunko_engines::{Backend, EngineConfig, EnginePipeline};
use bunko_processor::bench::BenchRun;
use bunko_processor::{BenchConfig, CancelToken};
use bunko_proto::{BenchOp, Event, PoolsSpec, RowSpec};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        bail!(
            "usage: bench_local <engine> <sample.cbz> [--models DIR] [--detector D] [--pages N] [--stage-workers k=n,...] [--budget-seconds S] [--trials N]"
        );
    }
    let (engine, sample) = (&args[0], PathBuf::from(&args[1]));
    let mut models = std::env::temp_dir().join("bunko-engines-models");
    let mut detector: Option<String> = None;
    let mut pools = PoolsSpec::default();
    let mut pages: u32 = 24;
    let mut config = BenchConfig::default();
    let mut i = 2;
    while i + 1 < args.len() {
        let v = &args[i + 1];
        match args[i].as_str() {
            "--models" => models = PathBuf::from(v),
            "--detector" => detector = Some(v.clone()),
            "--pages" => pages = v.parse()?,
            "--budget-seconds" => config.budget = Duration::from_secs(v.parse()?),
            "--trials" => config.max_trials = v.parse()?,
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
    let pipeline = EnginePipeline::new(EngineConfig::new(models, Backend::Auto));
    let op = BenchOp {
        bid: "b-1".into(),
        spec: RowSpec {
            id: "g-1".into(),
            name: engine.clone(),
            engine: engine.clone(),
            detector,
            patch_budget: 512,
            precision: "auto-accuracy".into(),
            pools,
            precision_pick: None,
            precision_why: String::new(),
            primary: true,
        },
        sample: sample.to_string_lossy().into_owned(),
        pages,
        precision_only: false,
    };
    let workspace = tempfile::tempdir()?;
    let cancel = CancelToken::new();
    let say = |event: Event| {
        if let Event::BenchDone { detail, .. } = &event {
            println!(
                "bench_done: {}",
                serde_json::to_string(detail).unwrap_or_default()
            );
        }
    };
    let run = BenchRun::new(
        &pipeline,
        &op,
        sample.clone(),
        workspace.path().to_path_buf(),
        &config,
        &cancel,
        &say,
    );
    run.run()
        .map_err(|e| anyhow::anyhow!("the benchmark stopped: {e:?}"))?;
    Ok(())
}
