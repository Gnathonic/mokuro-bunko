//! Read one page image with PP-OCR manga and print the raw lines JSON plus timings
//! (detection + recognition only; no layout passes).
//!
//! `cargo run -p bunko-ocr --release --example ppocr_page -- <models dir> <image> [threads] [repeat]`

use std::path::PathBuf;
use std::time::Instant;

use bunko_ocr::image::decode_bgr;
use bunko_ocr::models::{Manifest, ModelStore, StoreOptions};
use bunko_ocr::ppocr::{PpOcr, page_to_json};
use bunko_ocr::runtime::RuntimeOptions;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let models = PathBuf::from(&args[1]);
    let image = PathBuf::from(&args[2]);
    let threads: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
    let repeat: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(1);
    let tmp = std::env::temp_dir().join("bunko-ocr-example-models");
    let store = ModelStore::new(
        StoreOptions {
            root: tmp,
            override_dir: Some(models),
            download: false,
        },
        Manifest::builtin(),
    );
    let files = store.ppocr()?;
    let t = Instant::now();
    let engine = PpOcr::load(
        &files.detector,
        &files.recognizer,
        &files.dictionary,
        &RuntimeOptions {
            intra_threads: threads,
            ..Default::default()
        },
    )?;
    eprintln!("load {:.3}s", t.elapsed().as_secs_f64());
    let t = Instant::now();
    let img = decode_bgr(&std::fs::read(&image)?)?;
    eprintln!(
        "decode {:.3}s ({}x{})",
        t.elapsed().as_secs_f64(),
        img.width(),
        img.height()
    );
    let mut raw = None;
    for _ in 0..repeat {
        let t = Instant::now();
        let (mut lines, info) = engine.detect(&img)?;
        let td = t.elapsed().as_secs_f64();
        let quads: Vec<_> = lines.iter().map(|l| l.quad).collect();
        let t = Instant::now();
        let reads = engine.recognize_quads(&img, &quads)?;
        let tr = t.elapsed().as_secs_f64();
        for (l, (text, conf, confs)) in lines.iter_mut().zip(reads) {
            l.text = text;
            l.conf = conf;
            l.char_confs = confs;
        }
        eprintln!(
            "detect {td:.3}s  recognize {tr:.3}s  ({} lines)",
            lines.len()
        );
        raw = Some(page_to_json(&lines, img.width(), img.height(), Some(info)));
    }
    println!("{}", serde_json::to_string_pretty(&raw)?);
    Ok(())
}
