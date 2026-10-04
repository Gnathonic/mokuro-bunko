//! Recognizer-only reads of the shootout crop sets through the real loader (backend
//! pack + model store), for crop parity with 0.5.2 and recognizer throughput.
//!
//! ```text
//! MOKURO_TORCH_PACK=<pack dir> MOKURO_TORCH_MODELS_DIR=<dir with <engine>/<prec>/<target>/> \
//! cargo run -p bunko-engines --release --example torch_crops -- \
//!     <hayai|paddle> <fp32|bf16|fp16> <cpu|gpu:N> <out.json> [--reps N] [--spike DIR] [--models DIR]
//! ```
//! Prints one JSON line of stats; `out.json` holds `texts` and `by_name` (for the
//! shootout's `py/score.py`). paddle-manga crops are read with a 64-token cap, as the
//! 0.5.2 reference run did.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use bunko_engines::{Backend, EngineConfig, EnginePipeline};
use bunko_vlm::{CropSet, Precision, Rgb};

fn png(path: &Path) -> Result<Rgb> {
    let img = image::open(path)
        .with_context(|| path.display().to_string())?
        .to_rgb8();
    Rgb::from_raw(img.width() as usize, img.height() as usize, img.into_raw()).context("bad png")
}

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
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        bail!(
            "usage: torch_crops <hayai|paddle> <prec> <device> <out.json> [--reps N] [--spike DIR] [--models DIR]"
        );
    }
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let engine = match args[0].as_str() {
        "hayai" | "hayai-nova" => "hayai-nova",
        "paddle" | "paddle-manga" => "paddle-manga",
        e => bail!("engine {e}?"),
    };
    let prec: Precision = args[1].parse().map_err(|e| anyhow::anyhow!("{e}"))?;
    let device = args[2].clone();
    let outp = PathBuf::from(&args[3]);
    let mut reps = 3usize;
    let mut spike = home.join(".cache/mokuro-bunko-demo/onnx-spike");
    let mut models = home.join(".cache/mokuro-bunko-demo/models-v1");
    let mut i = 4;
    while i + 1 < args.len() {
        match args[i].as_str() {
            "--reps" => reps = args[i + 1].parse()?,
            "--spike" => spike = PathBuf::from(&args[i + 1]),
            "--models" => models = PathBuf::from(&args[i + 1]),
            o => bail!("unknown flag {o}"),
        }
        i += 2;
    }
    let (crops, names): (Vec<Rgb>, Vec<String>) = if engine == "hayai-nova" {
        let d = spike.join("hayai");
        let man: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.join("crops.json"))?)?;
        let names: Vec<String> = man
            .as_array()
            .context("crops.json")?
            .iter()
            .map(|m| m["file"].as_str().unwrap_or_default().to_string())
            .collect();
        let crops = names
            .iter()
            .map(|n| png(&d.join("crops").join(n)))
            .collect::<Result<_>>()?;
        (crops, names)
    } else {
        let d = spike.join("paddle/crops");
        let idx: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(d.join("index.json"))?)?;
        let mut names: Vec<String> = match &idx {
            serde_json::Value::Object(m) => m.keys().cloned().collect(),
            serde_json::Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => bail!("index.json"),
        };
        names.sort();
        let crops = names
            .iter()
            .map(|n| png(&d.join(n)))
            .collect::<Result<_>>()?;
        (crops, names)
    };
    let backend = if device == "cpu" {
        Backend::Cpu
    } else {
        Backend::Auto
    };
    let t0 = Instant::now();
    let pipeline = EnginePipeline::new(EngineConfig::new(models, backend));
    let rec = pipeline
        .recognizer_for(engine, prec, &device, 512)
        .map_err(anyhow::Error::msg)?;
    let load_s = t0.elapsed().as_secs_f64();
    let lines: Vec<CropSet> = crops.iter().cloned().map(CropSet::one).collect();
    let caps = vec![64u32; lines.len()];
    let caps = rec.info().token_caps.then_some(caps.as_slice());
    let run = || rec.read(&lines, caps).map_err(|e| anyhow::anyhow!("{e}"));
    let tw = Instant::now();
    let _ = run()?;
    let warm_s = tw.elapsed().as_secs_f64();
    let mut times = Vec::new();
    let mut texts = Vec::new();
    eprintln!("TIMED_START");
    for _ in 0..reps {
        let t = Instant::now();
        texts = run()?;
        times.push(t.elapsed().as_secs_f64());
    }
    eprintln!("TIMED_END");
    times.sort_by(f64::total_cmp);
    let med = times[times.len() / 2];
    let stats = serde_json::json!({
        "engine": engine, "prec": prec.as_str(), "device": device, "n": crops.len(),
        "load_s": load_s, "warm_pass_s": warm_s, "times": times,
        "crops_per_s": crops.len() as f64 / med,
        "rss_mib": status_mib("VmRSS:"), "hwm_mib": status_mib("VmHWM:"),
    });
    let by_name: serde_json::Map<String, serde_json::Value> = names
        .iter()
        .cloned()
        .zip(texts.iter().map(|t| serde_json::Value::String(t.clone())))
        .collect();
    std::fs::write(
        &outp,
        serde_json::to_string(
            &serde_json::json!({"stats": stats, "texts": texts, "by_name": by_name}),
        )?,
    )?;
    println!("{stats}");
    Ok(())
}
