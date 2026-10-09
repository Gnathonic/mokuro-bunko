//! Throughput of one shared recognizer driven by N caller threads.
//!
//! ```text
//! cargo run -p bunko-vlm --release --example bench -- hayai ~/.cache/mokuro-bunko-demo/onnx-spike \
//!     --precision fp32 --threads 1,8 --intra 16 --device cpu --reps 2
//! ```
//! Prints one JSON line per thread count. `--serial` forces one `Run` at a time
//! (what `ort`'s `&mut self` API would give), `--no-spin` turns ORT spinning off.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use bunko_vlm::{
    CropSet, Device, HayaiAssets, HayaiNova, OrtSessionFactory, PaddleAssets, PaddleManga,
    Precision, Recognizer, Rgb, SessionOptions, Sharing,
};

fn png(path: &std::path::Path) -> Result<Rgb> {
    let img = image::open(path)
        .with_context(|| path.display().to_string())?
        .to_rgb8();
    Rgb::from_raw(img.width() as usize, img.height() as usize, img.into_raw()).context("bad png")
}

fn hf(repo: &str, rev: &str) -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    home.join(".cache/huggingface/hub")
        .join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots")
        .join(rev)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        bail!(
            "usage: bench <hayai|paddle> <spike dir> [--precision fp32|fp16] [--threads 1,8] [--intra N] [--device cpu|gpu:N] [--reps N] [--limit N] [--tokenizer FILE] [--serial] [--no-spin]"
        );
    }
    let engine = args[0].as_str();
    let spike = PathBuf::from(&args[1]);
    let mut precision = Precision::Fp32;
    let mut threads = vec![1usize, 8];
    let mut intra = 0usize;
    let mut device = Device::Cpu;
    let mut reps = 1usize;
    let mut limit = usize::MAX;
    let mut serial = false;
    let mut spin = true;
    let mut tokenizer: Option<PathBuf> = None;
    let mut i = 2;
    while i < args.len() {
        let v = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--precision" => precision = v.parse()?,
            "--threads" => threads = v.split(',').map(str::parse).collect::<Result<_, _>>()?,
            "--intra" => intra = v.parse()?,
            "--device" => device = v.parse()?,
            "--reps" => reps = v.parse()?,
            "--limit" => limit = v.parse()?,
            "--tokenizer" => tokenizer = Some(PathBuf::from(v)),
            "--serial" => {
                serial = true;
                i += 1;
                continue;
            }
            "--no-spin" => {
                spin = false;
                i += 1;
                continue;
            }
            other => bail!("unknown flag {other}"),
        }
        i += 2;
    }
    let mut opts = SessionOptions::new(device)?;
    opts.intra_threads = intra;
    opts.spinning = spin;
    opts.sharing = if serial {
        Sharing::Serial
    } else {
        Sharing::Auto
    };

    let t0 = Instant::now();
    let (rec, crops): (Arc<dyn Recognizer>, Vec<Rgb>) = match engine {
        "hayai" => {
            let dir = spike.join("hayai");
            let tok = tokenizer.clone().unwrap_or_else(|| {
                hf(bunko_vlm::hayai::REPO, bunko_vlm::hayai::REVISION).join("tokenizer.json")
            });
            let rec = HayaiNova::load(
                &OrtSessionFactory,
                &HayaiAssets::in_dir(&dir.join("onnx"), &tok, precision),
                &opts,
                512,
            )?;
            let man: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("crops.json"))?)?;
            let crops = man
                .as_array()
                .context("crops.json")?
                .iter()
                .map(|m| {
                    png(&dir
                        .join("crops")
                        .join(m["file"].as_str().unwrap_or_default()))
                })
                .collect::<Result<_>>()?;
            (Arc::new(rec), crops)
        }
        "paddle" => {
            let dir = spike.join("paddle");
            let sub = if precision == Precision::Fp16 {
                "onnx_float16"
            } else {
                "onnx_float32"
            };
            let tok = tokenizer.clone().unwrap_or_else(|| {
                hf(
                    bunko_vlm::paddle::BASE_REPO,
                    bunko_vlm::paddle::BASE_REVISION,
                )
                .join("tokenizer.json")
            });
            let rec = PaddleManga::load(
                &OrtSessionFactory,
                &PaddleAssets::in_dir(&dir.join(sub), &tok),
                &opts,
            )?;
            let mut names: Vec<_> = std::fs::read_dir(dir.join("crops"))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|e| e == "png"))
                .collect();
            names.sort();
            let crops = names.iter().map(|p| png(p)).collect::<Result<_>>()?;
            (Arc::new(rec), crops)
        }
        other => bail!("unknown engine {other}"),
    };
    let load_s = t0.elapsed().as_secs_f64();
    let crops: Vec<Rgb> = crops.into_iter().take(limit).collect();
    // warm-up
    let warm: Vec<CropSet> = crops.iter().take(12).cloned().map(CropSet::one).collect();
    rec.read(&warm, None)?;

    let work: Vec<CropSet> = (0..reps)
        .flat_map(|_| crops.iter().cloned().map(CropSet::one))
        .collect();
    for &n in &threads {
        // round-robin split, like the spike's bench_threads.py
        let parts: Vec<Vec<CropSet>> = (0..n)
            .map(|k| work.iter().skip(k).step_by(n).cloned().collect())
            .collect();
        let t0 = Instant::now();
        std::thread::scope(|s| -> Result<()> {
            let hs: Vec<_> = parts
                .iter()
                .map(|p| s.spawn(|| rec.read(p, None)))
                .collect();
            for h in hs {
                h.join().map_err(|_| anyhow::anyhow!("worker panicked"))??;
            }
            Ok(())
        })?;
        let dt = t0.elapsed().as_secs_f64();
        println!(
            "{}",
            serde_json::json!({
                "engine": engine, "precision": precision.as_str(), "device": device.to_string(),
                "intra": intra, "spin": spin, "serial": serial, "threads": n, "crops": work.len(),
                "load_s": (load_s * 10.0).round() / 10.0,
                "crops_per_s": (work.len() as f64 / dt * 10.0).round() / 10.0,
                "ms_per_crop": (dt * 1000.0 / work.len() as f64 * 10.0).round() / 10.0,
            })
        );
    }
    Ok(())
}
