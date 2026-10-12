//! Text parity with the 0.5.2 torch recognizers on the spike's crop sets.
//!
//! Needs the big model files, so it only runs when `BUNKO_VLM_SPIKE` points at the
//! ONNX spike directory (`~/.cache/mokuro-bunko-demo/onnx-spike`) and the Hugging Face
//! cache holds the tokenizers. Run in release:
//!
//! ```text
//! BUNKO_VLM_SPIKE=~/.cache/mokuro-bunko-demo/onnx-spike cargo test -p bunko-vlm --release --test parity -- --nocapture --test-threads 1
//! ```
//! `BUNKO_VLM_DEVICE` (default `cpu`) and `BUNKO_VLM_THREADS` (ORT intra-op threads,
//! default 16) select where it runs.

use std::path::{Path, PathBuf};
use std::time::Instant;

use bunko_vlm::{
    CropSet, Device, HayaiAssets, HayaiNova, OrtSessionFactory, PaddleAssets, PaddleManga,
    Precision, Recognizer, Rgb, SessionOptions,
};
use unicode_normalization::UnicodeNormalization;

fn spike() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("BUNKO_VLM_SPIKE")?);
    p.is_dir().then_some(p)
}

fn hf(repo: &str, rev: &str) -> PathBuf {
    let base = std::env::var_os("HF_HUB_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".cache/huggingface/hub")
        });
    base.join(format!("models--{}", repo.replace('/', "--")))
        .join("snapshots")
        .join(rev)
}

fn opts() -> SessionOptions {
    let device: Device = std::env::var("BUNKO_VLM_DEVICE")
        .unwrap_or_else(|_| "cpu".into())
        .parse()
        .unwrap();
    let mut o = SessionOptions::new(device).unwrap();
    o.intra_threads = std::env::var("BUNKO_VLM_THREADS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    o
}

fn png(path: &Path) -> Rgb {
    let img = image::open(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .to_rgb8();
    Rgb::from_raw(img.width() as usize, img.height() as usize, img.into_raw()).unwrap()
}

/// 0.5.2's `normalize_text`: NFKC + `strip()`.
fn normalize(s: &str) -> String {
    let n: String = s.nfkc().collect();
    bunko_vlm::pyfmt::py_strip(&n).to_owned()
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn hayai_case(precision: Precision) {
    let Some(dir) = spike() else {
        eprintln!("BUNKO_VLM_SPIKE not set; skipping");
        return;
    };
    let dir = dir.join("hayai");
    let tok = hf(bunko_vlm::hayai::REPO, bunko_vlm::hayai::REVISION).join("tokenizer.json");
    let assets = HayaiAssets::in_dir(&dir.join("onnx"), &tok, precision);
    let t0 = Instant::now();
    let rec = HayaiNova::load(
        &OrtSessionFactory,
        &assets,
        &opts(),
        bunko_vlm::hayai::DEFAULT_PATCH_BUDGET,
    )
    .unwrap();
    let load = t0.elapsed();
    let man = json(&dir.join("crops.json"));
    let crops: Vec<CropSet> = man
        .as_array()
        .unwrap()
        .iter()
        .map(|m| CropSet::one(png(&dir.join("crops").join(m["file"].as_str().unwrap()))))
        .collect();
    let refs: Vec<String> = json(&dir.join("ref_torch_cpu_fp32.json"))
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    rec.read(&crops[..16], None).unwrap(); // warm-up
    let t0 = Instant::now();
    let out = rec.read(&crops, None).unwrap();
    let dt = t0.elapsed();
    let mut same = 0;
    for (i, (o, r)) in out.iter().zip(&refs).enumerate() {
        if normalize(o) == *r {
            same += 1;
        } else {
            eprintln!("  diff {i}: ref={r:?} got={:?}", normalize(o));
        }
    }
    eprintln!(
        "hayai-nova {precision} {}: {same}/{} identical; load {:.1}s; {:.1} ms/crop",
        rec.info().device,
        refs.len(),
        load.as_secs_f64(),
        dt.as_secs_f64() * 1000.0 / crops.len() as f64
    );
    match precision {
        Precision::Fp32 => assert_eq!(same, refs.len()),
        Precision::Fp16 => assert!(same + 3 >= refs.len(), "fp16 {same}/{}", refs.len()),
        Precision::Bf16 => unreachable!("no bf16 ONNX export"),
    }
}

#[test]
fn hayai_fp32_matches_torch() {
    hayai_case(Precision::Fp32);
}

#[test]
fn hayai_fp16_close_to_torch() {
    hayai_case(Precision::Fp16);
}

fn paddle_case(sub: &str) {
    let Some(dir) = spike() else {
        eprintln!("BUNKO_VLM_SPIKE not set; skipping");
        return;
    };
    let dir = dir.join("paddle");
    let tok = hf(
        bunko_vlm::paddle::BASE_REPO,
        bunko_vlm::paddle::BASE_REVISION,
    )
    .join("tokenizer.json");
    let assets = PaddleAssets::in_dir(&dir.join(sub), &tok);
    let t0 = Instant::now();
    let rec = PaddleManga::load(&OrtSessionFactory, &assets, &opts()).unwrap();
    let load = t0.elapsed();
    let index = json(&dir.join("crops/index.json"));
    let mut names: Vec<&String> = index.as_object().unwrap().keys().collect();
    names.sort();
    let crops: Vec<CropSet> = names
        .iter()
        .map(|n| CropSet::one(png(&dir.join("crops").join(n))))
        .collect();
    let reference = json(&dir.join("out_torch_merged_float32_cpu_b12.json"));
    let t0 = Instant::now();
    let out = rec.read(&crops, None).unwrap();
    let dt = t0.elapsed();
    let (mut same_raw, mut same_norm) = (0, 0);
    for (n, o) in names.iter().zip(&out) {
        let raw = reference["raw"][n.as_str()].as_str().unwrap();
        let text = reference["texts"][n.as_str()].as_str().unwrap();
        same_raw += usize::from(o == raw);
        if normalize(o) == text {
            same_norm += 1;
        } else {
            eprintln!("  diff {n}: ref={text:?} got={:?}", normalize(o));
        }
    }
    let precision = rec.info().precision;
    eprintln!(
        "paddle-manga {precision} {}: {same_norm}/{} identical (normalised), {same_raw} identical raw; load {:.1}s; {:.0} ms/crop",
        rec.info().device,
        names.len(),
        load.as_secs_f64(),
        dt.as_secs_f64() * 1000.0 / crops.len() as f64
    );
    match precision {
        Precision::Fp32 => assert_eq!((same_norm, same_raw), (names.len(), names.len())),
        Precision::Fp16 => assert!(same_norm + 3 >= names.len()),
        Precision::Bf16 => unreachable!("no bf16 ONNX export"),
    }
}

#[test]
fn paddle_fp32_matches_torch() {
    paddle_case("onnx_float32");
}

#[test]
fn paddle_fp16_close_to_torch() {
    paddle_case("onnx_float16");
}

/// Per-row caps: a row's text is its own cap's prefix, whatever its batch-mates' caps.
#[test]
fn paddle_caps_are_per_row() {
    let Some(dir) = spike() else {
        eprintln!("BUNKO_VLM_SPIKE not set; skipping");
        return;
    };
    let dir = dir.join("paddle");
    let tok = hf(
        bunko_vlm::paddle::BASE_REPO,
        bunko_vlm::paddle::BASE_REVISION,
    )
    .join("tokenizer.json");
    let rec = PaddleManga::load(
        &OrtSessionFactory,
        &PaddleAssets::in_dir(&dir.join("onnx_float32"), &tok),
        &opts(),
    )
    .unwrap();
    let names = ["c003.png", "c004.png", "c010.png", "c020.png"];
    let crops: Vec<CropSet> = names
        .iter()
        .map(|n| CropSet::one(png(&dir.join("crops").join(n))))
        .collect();
    let full = rec.read(&crops, Some(&[64, 64, 64, 64])).unwrap();
    let capped = rec.read(&crops, Some(&[3, 64, 5, 64])).unwrap();
    eprintln!("full {full:?}\ncapped {capped:?}");
    assert_eq!(capped[1], full[1]);
    assert_eq!(capped[3], full[3]);
    for (i, cap) in [(0usize, 3usize), (2, 5)] {
        // A cap can cut a byte-fallback character in half: 0.5.2 truncated the token
        // ids and decoded, which leaves U+FFFD for the partial bytes. Same here.
        let kept = capped[i].trim_end_matches('\u{fffd}');
        assert!(full[i].starts_with(kept), "{} vs {}", full[i], capped[i]);
        assert!(
            capped[i].chars().count() < full[i].chars().count(),
            "cap {cap} did not shorten {:?}",
            full[i]
        );
    }
}

/// One recognizer per key, shared by threads reading concurrently; same texts as alone.
#[test]
fn cache_shares_one_instance_across_threads() {
    let Some(dir) = spike() else {
        eprintln!("BUNKO_VLM_SPIKE not set; skipping");
        return;
    };
    let dir = dir.join("hayai");
    let tok = hf(bunko_vlm::hayai::REPO, bunko_vlm::hayai::REVISION).join("tokenizer.json");
    let key = bunko_vlm::EngineKey::Hayai {
        assets: HayaiAssets::in_dir(&dir.join("onnx"), &tok, Precision::Fp32),
        opts: SessionOptions {
            intra_threads: 2,
            ..opts()
        },
        budget: 512,
    };
    let cache = bunko_vlm::RecognizerCache::new(std::sync::Arc::new(OrtSessionFactory));
    let a = cache.get(&key).unwrap();
    let b = cache.get(&key).unwrap();
    assert!(std::sync::Arc::ptr_eq(&a, &b));
    let crops: Vec<CropSet> = (0..32)
        .map(|i| CropSet::one(png(&dir.join("crops").join(format!("{i:04}.png")))))
        .collect();
    let alone = a.read(&crops, None).unwrap();
    let parts: Vec<Vec<String>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4)
            .map(|k| {
                let r = std::sync::Arc::clone(&a);
                let mine: Vec<CropSet> = crops.iter().skip(k).step_by(4).cloned().collect();
                s.spawn(move || r.read(&mine, None).unwrap())
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (k, part) in parts.iter().enumerate() {
        for (j, t) in part.iter().enumerate() {
            assert_eq!(*t, alone[k + 4 * j]);
        }
    }
}
