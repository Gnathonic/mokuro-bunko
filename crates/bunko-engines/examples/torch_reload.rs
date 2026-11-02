//! Loads and drops a torch recognizer `N` times in one process (as a processor opens and
//! closes OCR sessions) and prints the process's memory after each drop: resident set
//! size, and GPU memory from `nvidia-smi` (0 without one). A recognizer that is not
//! freed shows as memory that grows by its weights every round.
//!
//! ```text
//! MOKURO_TORCH_PACK=<pack dir> MOKURO_TORCH_MODELS_DIR=<dir with <engine>/<prec>/<target>/> \
//! cargo run -p bunko-engines --release --example torch_reload -- <hayai|paddle> <prec> gpu:0 [N]
//! ```

use anyhow::{Result, bail};
use bunko_engines::{Backend, EngineConfig, EnginePipeline};
use bunko_vlm::{CropSet, Precision, Rgb};

fn gpu_mib() -> String {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-compute-apps=pid,used_memory", "--format=csv,noheader,nounits"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines()
        .find(|l| l.split(',').next().map(str::trim) == Some(pid.as_str()))
        .and_then(|l| l.split(',').nth(1))
        .map_or_else(|| "0".into(), |v| v.trim().to_string())
}

fn rss_mib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        bail!("usage: torch_reload <hayai|paddle> <prec> <device> [N]");
    }
    let engine = match args[0].as_str() {
        "hayai" | "hayai-nova" => "hayai-nova",
        "paddle" | "paddle-manga" => "paddle-manga",
        e => bail!("engine {e}?"),
    };
    let prec: Precision = args[1].parse().map_err(|e| anyhow::anyhow!("{e}"))?;
    let device = args[2].clone();
    let n: usize = args.get(3).map_or(Ok(5), |s| s.parse())?;
    let models = std::env::var_os("MOKURO_TORCH_MODELS_DIR").unwrap_or_default();
    let pipeline = EnginePipeline::new(EngineConfig::new(models.into(), Backend::Auto));
    // One synthetic line, so each round also runs the graphs (activations, KV cache).
    let crop = Rgb::from_raw(256, 48, vec![255u8; 256 * 48 * 3]).unwrap();
    let lines = vec![CropSet::one(crop)];
    println!("round,rss_mib,gpu_mib");
    for i in 0..n {
        let rec = pipeline
            .recognizer_for(engine, prec, &device, 512)
            .map_err(anyhow::Error::msg)?;
        rec.read(&lines, Some(&[16])).map_err(|e| anyhow::anyhow!("{e}"))?;
        drop(rec);
        println!("{i},{},{}", rss_mib(), gpu_mib());
    }
    Ok(())
}
