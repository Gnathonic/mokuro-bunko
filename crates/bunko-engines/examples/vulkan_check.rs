//! The Vulkan route's self-test goldens and checks (docs/rust-port/VULKAN.md §5).
//!
//! ```text
//! # goldens: what libtorch fp32 on the CPU reads from tools/vulkan_selftest's crops
//! MOKURO_TORCH_PACK=<cpu pack> cargo run -p bunko-engines --example vulkan_check -- \
//!     golden <engine> <crops dir> <out.json>
//! # a check: load <engine> on a Vulkan GPU through the real selection (self-test
//! # included), then read the goldens again and list any difference
//! MOKURO_VULKAN_PACK=<pack> MOKURO_VULKAN_MODELS_DIR=<dir> MOKURO_BACKENDS_DIR=<scratch> \
//! cargo run -p bunko-engines --example vulkan_check -- \
//!     check <engine> <golden.json> [--device gpu:N] [--runtime ncnn|webgpu]
//! ```

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use bunko_engines::vulkan::{Golden, GoldenCrop};
use bunko_engines::{Backend, EngineConfig, EnginePipeline};
use bunko_vlm::{CropSet, Precision, Rgb};

fn png(path: &Path) -> Result<Rgb> {
    let img = image::open(path)
        .with_context(|| path.display().to_string())?
        .to_rgb8();
    Rgb::from_raw(img.width() as usize, img.height() as usize, img.into_raw()).context("bad png")
}

fn models_dir() -> PathBuf {
    std::env::var_os("MOKURO_MODELS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("models"))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    match args.first().map(String::as_str) {
        Some("golden") => {
            let [_, engine, dir, out] = &args[..4] else {
                bail!("golden <engine> <crops dir> <out.json>");
            };
            let p = EnginePipeline::new(EngineConfig::new(models_dir(), Backend::Cpu));
            let rec = p
                .recognizer_for(engine, Precision::Fp32, "cpu", 512)
                .map_err(anyhow::Error::msg)?;
            let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "png"))
                .collect();
            files.sort();
            let crops: Vec<Rgb> = files.iter().map(|f| png(f)).collect::<Result<_>>()?;
            let lines: Vec<CropSet> = crops.iter().cloned().map(CropSet::one).collect();
            let texts = rec.read(&lines, None).map_err(|e| anyhow::anyhow!("{e}"))?;
            let g = Golden {
                engine: engine.clone(),
                crops: crops
                    .iter()
                    .zip(&texts)
                    .map(|(c, t)| GoldenCrop {
                        width: c.width as u32,
                        height: c.height as u32,
                        rgb: base64::engine::general_purpose::STANDARD.encode(&c.data),
                        text: t.clone(),
                        cap: None,
                    })
                    .collect(),
            };
            for (f, t) in files.iter().zip(&texts) {
                eprintln!("{}: {t}", f.display());
            }
            std::fs::write(out, serde_json::to_string(&g)?)?;
            println!("{} golden crops -> {out}", g.crops.len());
        }
        Some("check") => {
            let [_, engine, golden] = &args[..3] else {
                bail!("check <engine> <golden.json> [--device gpu:N] [--runtime R]");
            };
            let mut cfg = EngineConfig::new(models_dir(), Backend::Vulkan);
            if let Some(r) = flag("--runtime") {
                cfg.vulkan_backends = vec![r];
            }
            let p = EnginePipeline::new(cfg);
            let device = flag("--device").unwrap_or_else(|| "gpu:0".into());
            let t0 = Instant::now();
            let rec = p
                .recognizer_for(engine, Precision::Fp32, &device, 512)
                .map_err(anyhow::Error::msg)?;
            let load = t0.elapsed().as_secs_f64();
            let g = Golden::load(Path::new(golden)).map_err(anyhow::Error::msg)?;
            let crops: Vec<CropSet> = g
                .crops
                .iter()
                .map(|c| {
                    let b = base64::engine::general_purpose::STANDARD.decode(&c.rgb)?;
                    Ok(CropSet::one(
                        Rgb::from_raw(c.width as usize, c.height as usize, b).context("size")?,
                    ))
                })
                .collect::<Result<_>>()?;
            let t1 = Instant::now();
            let texts = rec.read(&crops, None).map_err(|e| anyhow::anyhow!("{e}"))?;
            let bad: Vec<String> = g
                .crops
                .iter()
                .zip(&texts)
                .enumerate()
                .filter(|(_, (c, t))| c.text != **t)
                .map(|(i, (c, t))| format!("crop {i}: {t:?} vs {:?}", c.text))
                .collect();
            println!(
                "{}",
                serde_json::json!({
                    "engine": engine, "device": device, "load_s": load,
                    "read_s": t1.elapsed().as_secs_f64(), "crops": texts.len(),
                    "exact": texts.len() - bad.len(), "diffs": bad,
                })
            );
            if !bad.is_empty() {
                std::process::exit(1);
            }
        }
        _ => bail!("vulkan_check golden|check ..."),
    }
    Ok(())
}
